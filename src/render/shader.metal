// Text rendering: one instanced quad per glyph.
//
// There is no vertex buffer. The quad's four corners are derived from
// vertex_id, and everything that varies per glyph lives in an instance
// buffer, so drawing a screen of text is a single draw call with no
// per-frame geometry upload beyond the instance array itself.

#include <metal_stdlib>
using namespace metal;

struct Glyph {
    float2 pos;    // top-left in logical points, y down
    float2 size;   // cell size in logical points
    float4 uv;     // atlas rect: u0, v0, u1, v1
    float4 color;  // straight (non-premultiplied) alpha
    uint   flags;  // bit 0: colour glyph; bits 1+: atlas page
    uint   pad0;   // explicit padding, so the Rust struct can match exactly
    uint   pad1;
    uint   pad2;
};

// Bit 0 of Glyph::flags. Set for colour fonts (emoji), which bring their own
// pixels and must not be tinted by the instance colour.
constant uint GLYPH_COLORED = 1u;

struct Uniforms {
    float2 viewport;  // logical points
};

struct VertexOut {
    float4 position [[position]];
    float2 uv;
    float4 color;
    uint   flags [[flat]];  // integers cannot be interpolated
    float2 local;
    float2 size [[flat]];
    float radius [[flat]];
};

vertex VertexOut vs_glyph(uint vid                    [[vertex_id]],
                          uint iid                    [[instance_id]],
                          constant Glyph*    glyphs   [[buffer(0)]],
                          constant Uniforms& uniforms [[buffer(1)]])
{
    // Corners for a triangle strip: (0,0) (1,0) (0,1) (1,1).
    float2 corner = float2(float(vid & 1u), float((vid >> 1u) & 1u));

    Glyph g = glyphs[iid];
    float2 p = g.pos + corner * g.size;

    // Logical points (origin top-left, y down) to clip space (y up).
    float2 ndc = float2(p.x / uniforms.viewport.x * 2.0 - 1.0,
                        1.0 - p.y / uniforms.viewport.y * 2.0);

    VertexOut out;
    out.position = float4(ndc, 0.0, 1.0);
    out.uv       = mix(g.uv.xy, g.uv.zw, corner);
    out.color    = g.color;
    out.flags    = g.flags;
    out.local    = corner * g.size;
    out.size     = g.size;
    out.radius   = as_type<float>(g.pad0);
    return out;
}

fragment float4 fs_glyph(VertexOut in [[stage_in]],
                         array<texture2d<float>, 8> atlases [[texture(0)]],
                         sampler         samp  [[sampler(0)]])
{
    if (in.flags & (1u << 31u)) {
        float2 q = abs(in.local - in.size * 0.5) - in.size * 0.5 + in.radius;
        float distance = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - in.radius;
        float coverage = 1.0 - smoothstep(-fwidth(distance) * 0.5, fwidth(distance) * 0.5, distance);
        float alpha = in.color.a * coverage;
        return float4(in.color.rgb * alpha, alpha);
    }
    // The atlas holds premultiplied RGBA.
    float4 texel = atlases[in.flags >> 1u].sample(samp, in.uv);

    if (in.flags & GLYPH_COLORED) {
        // A colour glyph carries its own pixels; only the instance alpha
        // still applies. Already premultiplied, so return it directly.
        return texel * in.color.a;
    }

    // A monochrome glyph is stored as premultiplied white, so its alpha is
    // the coverage. Tint it, premultiplied to match the blend state.
    float coverage = texel.a * in.color.a;
    return float4(in.color.rgb * coverage, coverage);
}
