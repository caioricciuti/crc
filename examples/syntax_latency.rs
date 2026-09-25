//! What syntax highlighting costs, at realistic file sizes.
//!
//! The editor re-parses the whole active document on every keystroke, which
//! is only defensible if a parse is well inside a frame. This is where that
//! claim is checked rather than assumed, and it is what sets the size cutoff
//! in `reparse_budget`.
//!
//! Run with: cargo run --release --example syntax_latency

use crc::syntax::{Highlighter, Language};
use crc::text::rope::Rope;
use std::time::Instant;

const BUDGET_MS: f64 = 1000.0 / 120.0;

fn main() {
    let unit = "\
fn handle_event(&mut self, ev: &Event) -> Result<()> {
    let span = self.tree.node_at(ev.offset)?;  // a comment
    match span.kind() { Kind::Ident => self.bump(), _ => {} }
    self.dirty.insert(span.range());
    let name = \"a string literal\";
    Ok(())
}

";
    println!("budget      {BUDGET_MS:.3} ms per frame at 120Hz\n");

    for kb in [16usize, 64, 256, 1024, 4096] {
        let target = kb * 1024;
        let mut src = String::with_capacity(target + unit.len());
        while src.len() < target {
            src.push_str(unit);
        }
        let rope = Rope::from_text(&src);
        let mut h = Highlighter::new(Language::Rust).expect("grammar");

        // Warm one parse so the first allocation is not in the sample.
        let tree = h.parse(&rope).expect("parse");

        let runs = if kb >= 1024 { 5 } else { 20 };
        let t = Instant::now();
        let mut last = None;
        for _ in 0..runs {
            last = h.parse(&rope);
        }
        let parse_ms = t.elapsed().as_secs_f64() * 1e3 / runs as f64;
        std::hint::black_box(&last);

        // A screenful of highlight spans, which is the per-frame cost.
        let viewport_end = rope.line_to_byte(60.min(rope.len_lines() - 1));
        let t = Instant::now();
        let mut spans = 0;
        for _ in 0..200 {
            spans = h.spans(&tree, 0..viewport_end).len();
        }
        let query_ms = t.elapsed().as_secs_f64() * 1e3 / 200.0;

        // The keystroke path: one character typed, re-parsed incrementally.
        let mut typed = crc::text::buffer::Buffer::from_text(&src);
        typed.move_buffer_end(crc::text::buffer::Motion::Move);
        typed.drain_edits();
        let mut live = h.parse(&typed.rope).expect("parse");
        let t = Instant::now();
        const KEYS: usize = 50;
        for _ in 0..KEYS {
            typed.insert("x");
            let edits = typed.drain_edits().expect("replayable");
            if let Some(next) = h.parse_incremental(&typed.rope, &live, &edits) {
                live = next;
            }
        }
        let inc_ms = t.elapsed().as_secs_f64() * 1e3 / KEYS as f64;

        println!(
            "{kb:>5} KiB   full {parse_ms:>8.3} ms   incremental {inc_ms:>7.4} ms   \
             viewport query {query_ms:>6.3} ms  ({spans} spans)   {}",
            if inc_ms < BUDGET_MS {
                "within budget"
            } else {
                "OVER BUDGET"
            }
        );
    }
}
