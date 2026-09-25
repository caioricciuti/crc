//! Bounded cold-fallback rasterization. Only chars and owned RGBA bytes cross
//! threads; all native font/context objects are created and dropped there.
use super::{
    CGFloat, CGPoint, CTFontSymbolicTraits, PAD, display_width, load_monospace, rasterize_bitmap,
    resolve_glyph,
};
use std::collections::HashSet;
use std::sync::mpsc;

const CAPACITY: usize = 64;
const INSTALL_PER_FRAME: usize = 8;

pub(super) struct Bitmap {
    pub pixels: Vec<u8>,
    pub cells: usize,
    pub color: bool,
}

pub(super) struct Worker {
    sender: mpsc::SyncSender<char>,
    receiver: mpsc::Receiver<(char, Option<Bitmap>)>,
    pending: HashSet<char>,
}

impl Worker {
    pub fn new(name: String, size: f32, cell_px: (usize, usize), ascent: f32) -> Self {
        let (sender, requests) = mpsc::sync_channel::<char>(CAPACITY);
        let (completed, receiver) = mpsc::sync_channel(CAPACITY);
        std::thread::Builder::new()
            .name("fallback-glyphs".into())
            .spawn(move || {
                let font = load_monospace(&name, size).font;
                while let Ok(ch) = requests.recv() {
                    let bitmap = (|| {
                        let mut units = [0; 2];
                        let (fallback, glyph) = resolve_glyph(&font, ch.encode_utf16(&mut units))?;
                        let font = fallback.as_deref().unwrap_or(&font);
                        let cells = display_width(ch);
                        let pos = CGPoint {
                            x: (PAD / 2) as CGFloat,
                            y: (cell_px.1 as f32 - (PAD / 2) as f32 - ascent) as CGFloat,
                        };
                        Some(Bitmap {
                            pixels: rasterize_bitmap(cell_px, font, glyph, pos, cells)?,
                            cells,
                            color: unsafe { font.symbolic_traits() }
                                .contains(CTFontSymbolicTraits::ColorGlyphsTrait),
                        })
                    })();
                    if completed.send((ch, bitmap)).is_err() {
                        break;
                    }
                }
            })
            .expect("start fallback glyph worker");
        Self {
            sender,
            receiver,
            pending: HashSet::new(),
        }
    }

    pub fn request(&mut self, ch: char) {
        if self.pending.contains(&ch) || self.pending.len() >= CAPACITY {
            return;
        }
        if self.sender.try_send(ch).is_ok() {
            self.pending.insert(ch);
        }
    }

    pub fn poll(&mut self) -> Vec<(char, Option<Bitmap>)> {
        let mut ready = Vec::new();
        for _ in 0..INSTALL_PER_FRAME {
            let Ok(result) = self.receiver.try_recv() else {
                break;
            };
            self.pending.remove(&result.0);
            ready.push(result);
        }
        ready
    }

    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_coalesce_and_installation_is_bounded() {
        let mut worker = Worker::new("Menlo".into(), 26.0, (18, 35), 25.0);
        for ch in (0x4e00..0x4f00).filter_map(char::from_u32) {
            worker.request(ch);
            worker.request(ch);
        }
        assert_eq!(worker.pending.len(), CAPACITY);
        let started = std::time::Instant::now();
        let mut installed = 0;
        while worker.is_pending() {
            let ready = worker.poll();
            assert!(ready.len() <= INSTALL_PER_FRAME);
            installed += ready.len();
            assert!(started.elapsed().as_secs() < 10);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(installed, CAPACITY);
    }
}
