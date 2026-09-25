//! The Metal pipeline: atlas texture, instance buffer, one draw call.
//!
//! Drawing a screen of text costs one `drawPrimitives` with an instance per
//! visible glyph. There is no per-frame geometry work beyond filling a flat
//! array of [`GlyphInstance`], which is why the frame budget stays free for
//! everything else.

use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSString, ns_string};
use objc2_metal::{
    MTLBlendFactor, MTLBuffer, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCompileOptions, MTLDevice, MTLLibrary, MTLLoadAction, MTLPixelFormat, MTLPrimitiveType,
    MTLRegion, MTLRenderCommandEncoder, MTLRenderPassDescriptor, MTLRenderPipelineDescriptor,
    MTLRenderPipelineState, MTLResourceOptions, MTLSamplerDescriptor, MTLSamplerMinMagFilter,
    MTLSamplerState, MTLSize, MTLStorageMode, MTLStoreAction, MTLTexture, MTLTextureDescriptor,
    MTLTextureUsage,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};

use crate::render::font::Atlas;

/// The pixel format of the drawable. BGRA8 unorm is the native format for a
/// `CAMetalLayer` on macOS; anything else costs a conversion on present.
pub const DRAWABLE_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm;

/// A cached offscreen render target and the size it was made for.
type OffscreenTarget = (usize, usize, Retained<ProtocolObject<dyn MTLTexture>>);

/// One glyph to draw. Layout must match `struct Glyph` in shader.metal.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GlyphInstance {
    /// Top-left in logical points, y down.
    pub pos: [f32; 2],
    /// Cell size in logical points.
    pub size: [f32; 2],
    /// Atlas rect: u0, v0, u1, v1.
    pub uv: [f32; 4],
    /// Straight (non-premultiplied) RGBA tint.
    pub color: [f32; 4],
    /// Bit 0: sample the atlas colour instead of tinting. Bits 1+: page index.
    /// See [`COLORED`] and `Slot::flags`.
    pub flags: u32,
    /// Explicit padding. MSL rounds the struct to its 16-byte alignment, so
    /// spelling the tail out keeps both sides provably identical.
    pub _pad: [u32; 3],
}

/// [`GlyphInstance::flags`] bit 0: the glyph brings its own colour (emoji).
pub const COLORED: u32 = 1;
/// Analytic rounded rectangle; `_pad[0]` stores its logical-point radius.
pub const ROUNDED: u32 = 1 << 31;

const _: () = assert!(
    size_of::<GlyphInstance>() == 64,
    "GlyphInstance must match the MSL struct layout"
);

/// Where a frame's wall-clock time actually went.
///
/// Split out because the three phases fail for completely different reasons:
/// `acquire` blocking means the display is throttling us, `encode` growing
/// means we are submitting too much geometry, and `commit` is normally noise.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameTiming {
    /// Waiting for `nextDrawable`. Display-paced, not our work.
    pub acquire: Duration,
    /// Building the render pass and encoding the draw.
    pub encode: Duration,
    /// Presenting and committing to the GPU.
    pub commit: Duration,
}

impl FrameTiming {
    pub fn total(&self) -> Duration {
        self.acquire + self.encode + self.commit
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Uniforms {
    viewport: [f32; 2],
}

pub struct Renderer {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    atlas_textures: Vec<Retained<ProtocolObject<dyn MTLTexture>>>,
    /// Shared-storage instance buffer, written directly by the CPU each frame.
    instances: Retained<ProtocolObject<dyn MTLBuffer>>,
    instance_capacity: usize,
    /// Reused offscreen render target, keyed by size. Allocating a texture
    /// per frame would show up in any timing taken through this path.
    offscreen: Option<OffscreenTarget>,
    pub atlas: Atlas,
}

impl Renderer {
    /// Builds the pipeline and uploads the glyph atlas.
    ///
    /// The shader is compiled from source at startup rather than from a
    /// prebuilt `.metallib`, which keeps the build a plain `cargo build` with
    /// no extra toolchain step. It costs tens of milliseconds once.
    pub fn new(device: Retained<ProtocolObject<dyn MTLDevice>>, atlas: Atlas) -> Renderer {
        let queue = device
            .newCommandQueue()
            .expect("could not create a Metal command queue");

        let source = NSString::from_str(include_str!("shader.metal"));
        let options = MTLCompileOptions::new();
        let library = device
            .newLibraryWithSource_options_error(&source, Some(&options))
            .expect("shader.metal failed to compile");

        let vs = library
            .newFunctionWithName(ns_string!("vs_glyph"))
            .expect("vs_glyph missing from shader");
        let fs = library
            .newFunctionWithName(ns_string!("fs_glyph"))
            .expect("fs_glyph missing from shader");

        let desc = MTLRenderPipelineDescriptor::new();
        unsafe {
            desc.setVertexFunction(Some(&vs));
            desc.setFragmentFunction(Some(&fs));
            let attachments = desc.colorAttachments();
            let color = attachments.objectAtIndexedSubscript(0);
            color.setPixelFormat(DRAWABLE_FORMAT);
            // Premultiplied alpha. The atlas stores premultiplied pixels and
            // the shader returns premultiplied output, so the source factor
            // is One rather than SourceAlpha. Getting this wrong shows up as
            // dark fringing around antialiased glyph edges.
            color.setBlendingEnabled(true);
            color.setSourceRGBBlendFactor(MTLBlendFactor::One);
            color.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
            color.setSourceAlphaBlendFactor(MTLBlendFactor::One);
            color.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        }

        let pipeline = device
            .newRenderPipelineStateWithDescriptor_error(&desc)
            .expect("could not create the render pipeline state");

        // Nearest filtering: the atlas is rasterized at the exact device
        // scale, so every texel maps 1:1 to a pixel. Linear filtering would
        // only soften glyphs that are already correctly sized.
        let sampler_desc = MTLSamplerDescriptor::new();
        sampler_desc.setMinFilter(MTLSamplerMinMagFilter::Nearest);
        sampler_desc.setMagFilter(MTLSamplerMinMagFilter::Nearest);
        let sampler = device
            .newSamplerStateWithDescriptor(&sampler_desc)
            .expect("could not create sampler state");

        let atlas_textures = vec![upload_atlas(&device, &atlas, 0)];

        let instance_capacity = 16_384;
        let instances = device
            .newBufferWithLength_options(
                instance_capacity * size_of::<GlyphInstance>(),
                MTLResourceOptions::StorageModeShared,
            )
            .expect("could not allocate the instance buffer");

        Renderer {
            device,
            queue,
            pipeline,
            sampler,
            atlas_textures,
            instances,
            instance_capacity,
            offscreen: None,
            atlas,
        }
    }

    /// Grows the instance buffer if a frame needs more glyphs than it holds.
    fn reserve(&mut self, needed: usize) {
        if needed <= self.instance_capacity {
            return;
        }
        let capacity = needed.next_power_of_two();
        self.instances = self
            .device
            .newBufferWithLength_options(
                capacity * size_of::<GlyphInstance>(),
                MTLResourceOptions::StorageModeShared,
            )
            .expect("could not grow the instance buffer");
        self.instance_capacity = capacity;
    }

    /// Draws one frame into the layer's next drawable.
    ///
    /// Returns `None` if the layer had no drawable available, which happens
    /// when the window is off-screen. That is a normal condition, not an
    /// error: the caller skips the frame.
    pub fn draw(
        &mut self,
        layer: &CAMetalLayer,
        glyphs: &[GlyphInstance],
        viewport: (f32, f32),
        background: [f64; 4],
    ) -> Option<FrameTiming> {
        // `nextDrawable` is the one call here that can block, and for an
        // interesting reason: a CAMetalLayer hands out a small fixed pool of
        // drawables (three by default) and will not return until one comes
        // back from the compositor. So it is throttled by the display, not by
        // us, and timing it separately is the only way to tell "our work was
        // slow" apart from "we were waiting for a vsync".
        let t = Instant::now();
        let drawable = layer.nextDrawable()?;
        let acquire = t.elapsed();

        let t = Instant::now();
        let texture = drawable.texture();
        let command_buffer = self.encode(&texture, glyphs, viewport, background);
        let encode = t.elapsed();

        let t = Instant::now();
        command_buffer.presentDrawable(ProtocolObject::from_ref(&*drawable));
        command_buffer.commit();
        let commit = t.elapsed();

        Some(FrameTiming {
            acquire,
            encode,
            commit,
        })
    }

    /// Renders one frame into an offscreen texture and reads it back as BGRA8.
    ///
    /// Same pipeline, same shader, same layout as the on-screen path; only
    /// the render target differs. That makes it usable as a real rendering
    /// test rather than a separate code path that could drift.
    pub fn render_offscreen(
        &mut self,
        width: usize,
        height: usize,
        glyphs: &[GlyphInstance],
        viewport: (f32, f32),
        background: [f64; 4],
    ) -> Vec<u8> {
        self.render_offscreen_frame(width, height, glyphs, viewport, background);
        let target = self.offscreen_target(width, height);

        let mut out = vec![0u8; width * height * 4];
        let region = MTLRegion {
            origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width,
                height,
                depth: 1,
            },
        };
        unsafe {
            target.getBytes_bytesPerRow_fromRegion_mipmapLevel(
                std::ptr::NonNull::new(out.as_mut_ptr() as *mut _).expect("non-empty"),
                width * 4,
                region,
                0,
            );
        }
        out
    }

    /// Renders one frame offscreen and waits for the GPU to finish it, with
    /// no readback. This is the path a latency benchmark should time: it
    /// covers exactly the work a real frame does, plus the wait needed to
    /// know when the GPU actually finished.
    pub fn render_offscreen_frame(
        &mut self,
        width: usize,
        height: usize,
        glyphs: &[GlyphInstance],
        viewport: (f32, f32),
        background: [f64; 4],
    ) {
        let target = self.offscreen_target(width, height);
        let command_buffer = self.encode(&target, glyphs, viewport, background);
        command_buffer.commit();
        command_buffer.waitUntilCompleted();
    }

    /// The cached offscreen target, reallocated only when the size changes.
    fn offscreen_target(
        &mut self,
        width: usize,
        height: usize,
    ) -> Retained<ProtocolObject<dyn MTLTexture>> {
        if let Some((w, h, tex)) = &self.offscreen
            && *w == width
            && *h == height
        {
            return tex.clone();
        }
        let desc = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                DRAWABLE_FORMAT,
                width,
                height,
                false,
            )
        };
        desc.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
        desc.setStorageMode(MTLStorageMode::Shared);
        let tex = self
            .device
            .newTextureWithDescriptor(&desc)
            .expect("could not create the offscreen target");
        self.offscreen = Some((width, height, tex.clone()));
        tex
    }

    /// Swaps in a freshly built atlas, for a new font size or backing scale.
    /// Its pages upload on the next frame; the old textures are dropped
    /// once any command buffer still reading them completes.
    pub fn replace_atlas(&mut self, atlas: Atlas) {
        self.atlas = atlas;
        self.atlas_textures.clear();
    }

    /// Upload only changed pages. Replacing their textures also lets in-flight
    /// command buffers finish with the old pixels after an LRU eviction.
    fn sync_atlas(&mut self) {
        if !self.atlas.dirty {
            return;
        }
        for page in 0..self.atlas.page_count() {
            if page == self.atlas_textures.len() {
                self.atlas_textures
                    .push(upload_atlas(&self.device, &self.atlas, page));
            } else if self.atlas.page_dirty(page) {
                self.atlas_textures[page] = upload_atlas(&self.device, &self.atlas, page);
            }
        }
        self.atlas.mark_uploaded();
    }

    /// Encodes a frame into `target` and returns the uncommitted buffer.
    fn encode(
        &mut self,
        target: &ProtocolObject<dyn MTLTexture>,
        glyphs: &[GlyphInstance],
        viewport: (f32, f32),
        background: [f64; 4],
    ) -> Retained<ProtocolObject<dyn MTLCommandBuffer>> {
        // Layout runs before this and may have rasterized new characters.
        self.sync_atlas();
        self.reserve(glyphs.len());
        if !glyphs.is_empty() {
            // Shared storage means this is a plain memcpy into memory the GPU
            // will read; no staging buffer, no blit encoder.
            unsafe {
                let dst = self.instances.contents().as_ptr() as *mut GlyphInstance;
                std::ptr::copy_nonoverlapping(glyphs.as_ptr(), dst, glyphs.len());
            }
        }

        let pass = MTLRenderPassDescriptor::renderPassDescriptor();
        unsafe {
            let attachments = pass.colorAttachments();
            let color = attachments.objectAtIndexedSubscript(0);
            color.setTexture(Some(target));
            color.setLoadAction(MTLLoadAction::Clear);
            color.setStoreAction(MTLStoreAction::Store);
            color.setClearColor(objc2_metal::MTLClearColor {
                red: background[0],
                green: background[1],
                blue: background[2],
                alpha: background[3],
            });
        }

        let command_buffer = self
            .queue
            .commandBuffer()
            .expect("could not create a command buffer");
        let encoder = command_buffer
            .renderCommandEncoderWithDescriptor(&pass)
            .expect("could not create a render encoder");

        if !glyphs.is_empty() {
            let uniforms = Uniforms {
                viewport: [viewport.0, viewport.1],
            };
            unsafe {
                encoder.setRenderPipelineState(&self.pipeline);
                encoder.setVertexBuffer_offset_atIndex(Some(&self.instances), 0, 0);
                encoder.setVertexBytes_length_atIndex(
                    std::ptr::NonNull::from(&uniforms).cast(),
                    size_of::<Uniforms>(),
                    1,
                );
                for (page, texture) in self.atlas_textures.iter().enumerate() {
                    encoder.setFragmentTexture_atIndex(Some(texture), page);
                }
                encoder.setFragmentSamplerState_atIndex(Some(&self.sampler), 0);
                encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
                    MTLPrimitiveType::TriangleStrip,
                    0,
                    4,
                    glyphs.len(),
                );
            }
        }

        encoder.endEncoding();
        command_buffer
    }
}

/// Uploads one RGBA atlas page as a shared-storage texture.
fn upload_atlas(
    device: &ProtocolObject<dyn MTLDevice>,
    atlas: &Atlas,
    page: usize,
) -> Retained<ProtocolObject<dyn MTLTexture>> {
    let desc = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            // RGBA, not a coverage mask: the atlas holds colour glyphs too.
            MTLPixelFormat::RGBA8Unorm,
            atlas.width as usize,
            atlas.height as usize,
            false,
        )
    };
    {
        desc.setUsage(MTLTextureUsage::ShaderRead);
        // Managed would also work, but the atlas is written exactly once at
        // startup, so shared storage keeps the upload a single memcpy.
        desc.setStorageMode(MTLStorageMode::Shared);
    }

    let texture = device
        .newTextureWithDescriptor(&desc)
        .expect("could not create the atlas texture");

    let region = MTLRegion {
        origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
        size: MTLSize {
            width: atlas.width as usize,
            height: atlas.height as usize,
            depth: 1,
        },
    };
    unsafe {
        texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
            region,
            0,
            std::ptr::NonNull::new(atlas.page_pixels(page).as_ptr() as *mut _)
                .expect("atlas is non-empty"),
            atlas.width as usize * 4,
        );
    }
    texture
}
