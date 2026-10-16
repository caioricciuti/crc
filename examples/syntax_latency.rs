//! What syntax highlighting costs, at realistic file sizes.
//!
//! The editor re-parses the whole active document on every keystroke, which
//! is only defensible if a parse is well inside a frame. This is where that
//! claim is checked rather than assumed, and it is what sets the size cutoff
//! in `reparse_budget`, past which a keystroke's parse runs on the syntax
//! worker. The second line per size is what the main thread pays the way
//! the window works, with keystrokes a frame apart: opening (the first
//! parse, synchronous up to `first_parse_budget`), then per keystroke the
//! edit, `update_async` and a `poll`, all parsed here ("here") and with the
//! window's budgets ("window"), and how long after the last keystroke the
//! colours had caught up.
//!
//! Run with: cargo run --release --example syntax_latency

use crc::syntax::{Highlighter, Language, SyntaxStore};
use crc::text::rope::Rope;
use std::time::{Duration, Instant};

const BUDGET_MS: f64 = 1000.0 / 120.0;
/// The window's `reparse_budget` and `first_parse_budget`.
const EDIT_BUDGET: usize = 64 * 1024;
const OPEN_BUDGET: usize = 512 * 1024;

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

    for kb in [16usize, 64, 128, 256, 512, 1024, 4096, 16384] {
        let target = kb * 1024;
        let mut src = String::with_capacity(target + unit.len());
        while src.len() < target {
            src.push_str(unit);
        }
        let rope = Rope::from_text(&src);
        let mut h = Highlighter::new(Language::Rust).expect("grammar");

        // Warm one parse so the first allocation is not in the sample.
        let tree = h.parse(&rope).expect("parse");

        let runs = match kb {
            ..1024 => 20,
            1024..4096 => 5,
            _ => 2,
        };
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
             viewport query {query_ms:>6.3} ms  ({spans} spans)"
        );
        // Everything here is the old way; the window's budgets the new one.
        let here = typing(&src, usize::MAX, usize::MAX);
        let window = typing(&src, EDIT_BUDGET, OPEN_BUDGET);
        println!(
            "            a frame apart: opening {:>7.3} ms here, {:>7.3} ms window   \
             keystroke here {:>6.3} (max {:>6.3})   window {:>6.3} (max {:>6.3}), \
             caught up in {:>5.1} ms   {}",
            here.open_ms,
            window.open_ms,
            here.mean_ms,
            here.max_ms,
            window.mean_ms,
            window.max_ms,
            window.caught_up_ms,
            if window.max_ms < 1.0 {
                "under 1 ms"
            } else if window.max_ms < BUDGET_MS {
                "within budget"
            } else {
                "OVER BUDGET"
            }
        );
    }
}

/// What typing costs the main thread through the store.
struct Typing {
    /// The first `update_async`: the whole parse when it runs here.
    open_ms: f64,
    mean_ms: f64,
    max_ms: f64,
    /// From the last keystroke to nothing left on the worker.
    caught_up_ms: f64,
}

/// Opens `src` as a Rust document and types into it a frame apart, as the
/// window does: the edit, `update_async` and a `poll` per keystroke. Off
/// the clock: the wait for the first parse, and the wait for a frame.
fn typing(src: &str, edit_budget: usize, open_budget: usize) -> Typing {
    const KEYS: usize = 50;
    let mut store = SyntaxStore::new();
    // The query compiles once per language, not per document.
    let mut warm = crc::text::buffer::Buffer::from_text("fn f() {}\n");
    warm.path = Some("warm.rs".into());
    store.update_async(&mut warm, edit_budget, open_budget);

    let mut typed = crc::text::buffer::Buffer::from_text(src);
    typed.path = Some("big.rs".into());
    typed.move_buffer_end(crc::text::buffer::Motion::Move);
    let t = Instant::now();
    store.update_async(&mut typed, edit_budget, open_budget);
    let open_ms = t.elapsed().as_secs_f64() * 1e3;
    settle(&mut store);
    let mut main = Vec::with_capacity(KEYS);
    for _ in 0..KEYS {
        std::thread::sleep(Duration::from_secs_f64(BUDGET_MS / 1e3));
        let t = Instant::now();
        typed.insert("x");
        store.update_async(&mut typed, edit_budget, open_budget);
        store.poll();
        main.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let t = Instant::now();
    settle(&mut store);
    let caught_up_ms = t.elapsed().as_secs_f64() * 1e3;
    main.sort_by(f64::total_cmp);
    Typing {
        open_ms,
        mean_ms: main.iter().sum::<f64>() / KEYS as f64,
        max_ms: main[KEYS - 1],
        caught_up_ms,
    }
}

/// Polls until nothing is parsing, as the display link would.
fn settle(store: &mut SyntaxStore) {
    while store.busy() {
        store.poll();
        std::thread::sleep(Duration::from_millis(1));
    }
}
