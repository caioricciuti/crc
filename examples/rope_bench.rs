//! Milestone 0 gate for the text layer.
//!
//! The renderer is useless if the buffer underneath it cannot keep up, so
//! these numbers get checked before any Metal work. The budget to beat is one
//! 120Hz frame: 8.33ms. Everything here should be orders of magnitude under
//! that, because the buffer is only one part of a frame's work.
//!
//! Run with: cargo run --release --offline --example rope_bench

use crc::text::rope::Rope;
use std::time::Instant;

fn synth(target_bytes: usize) -> String {
    // Roughly source-code shaped: ~50 byte lines, mixed content, some
    // multi-byte characters so the chunker has to work for its living.
    let mut s = String::with_capacity(target_bytes + 128);
    let mut i = 0usize;
    while s.len() < target_bytes {
        match i % 7 {
            0 => s.push_str("fn handle_event(&mut self, ev: &Event) -> Result<()> {\n"),
            1 => s.push_str("    let span = self.tree.node_at(ev.offset)?;\n"),
            2 => s.push_str("    // café, naïve, 🌍 — multi-byte lives here too\n"),
            3 => s.push_str("    match span.kind() { Kind::Ident => self.bump(), _ => {} }\n"),
            4 => s.push('\n'),
            5 => s.push_str("    self.dirty.insert(span.range());\n"),
            _ => s.push_str("}\n"),
        }
        i += 1;
    }
    s
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn main() {
    const TARGET: usize = 100 * 1024 * 1024;

    println!(
        "building {} MiB of synthetic source...",
        TARGET / 1024 / 1024
    );
    let src = synth(TARGET);
    println!(
        "  {} bytes, {} lines\n",
        src.len(),
        src.matches('\n').count() + 1
    );

    // --- bulk load ---------------------------------------------------------
    let t = Instant::now();
    let mut rope = Rope::from_text(&src);
    let load = t.elapsed();
    println!(
        "load            {:>9.2} ms   ({:.0} MiB/s)",
        ms(load),
        (src.len() as f64 / 1024.0 / 1024.0) / load.as_secs_f64()
    );
    println!("  lines: {}", rope.len_lines());

    // --- O(1) snapshot -----------------------------------------------------
    let t = Instant::now();
    let snapshot = rope.clone();
    let clone = t.elapsed();
    println!(
        "clone           {:>9.4} ms   (snapshot for a background parse)",
        ms(clone)
    );
    assert_eq!(snapshot.len_bytes(), rope.len_bytes());

    // --- line lookup, the render hot path ----------------------------------
    let total_lines = rope.len_lines();
    let probes: Vec<usize> = (0..10_000).map(|i| (i * 7919) % total_lines).collect();
    let t = Instant::now();
    let mut acc = 0usize;
    for &l in &probes {
        acc = acc.wrapping_add(rope.line_to_byte(l));
    }
    let lookup = t.elapsed();
    std::hint::black_box(acc);
    println!(
        "line_to_byte    {:>9.4} us   per lookup, scattered over the file",
        lookup.as_secs_f64() * 1e6 / probes.len() as f64
    );

    // --- pulling one viewport of text --------------------------------------
    // 60 lines is a generous full-screen viewport at a readable size.
    let t = Instant::now();
    let mut bytes = 0usize;
    for w in 0..1000 {
        let first = (w * 997) % (total_lines - 61);
        let start = rope.line_to_byte(first);
        let end = rope.line_to_byte(first + 60);
        for chunk in rope.chunks_in(start..end) {
            bytes += chunk.len();
        }
    }
    let viewport = t.elapsed();
    std::hint::black_box(bytes);
    println!(
        "viewport (60ln) {:>9.4} ms   per frame's worth of text",
        ms(viewport) / 1000.0
    );

    // --- editing in the middle, the thing a Vec<String> gets wrong ----------
    let mid = rope.line_to_byte(total_lines / 2);
    let t = Instant::now();
    for i in 0..1000 {
        rope.insert(mid + i, "x");
    }
    let insert = t.elapsed();
    println!(
        "insert 1 char   {:>9.4} ms   mid-file, {} keystrokes",
        ms(insert) / 1000.0,
        1000
    );

    let t = Instant::now();
    for _ in 0..1000 {
        rope.delete(mid..mid + 1);
    }
    let delete = t.elapsed();
    println!("delete 1 char   {:>9.4} ms   mid-file", ms(delete) / 1000.0);

    // The snapshot taken before all those edits must be untouched.
    assert_eq!(snapshot.len_bytes(), src.len(), "snapshot was mutated");
    println!("\nsnapshot still intact after 2000 edits to the live rope");

    let frame = ms(viewport) / 1000.0 + ms(insert) / 1000.0;
    println!(
        "\nworst-case text work in one frame: {:.4} ms of an 8.33 ms budget ({:.2}%)",
        frame,
        frame / 8.33 * 100.0
    );
}
