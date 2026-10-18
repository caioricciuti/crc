//! Terminal parser lock hold time, without a window or GPU.
//! Models live 64 KiB bursts and the held session's 2 MiB replay.
//! Run with cargo run --release --offline --example terminal_lock_latency.

use crc::term::Term;
use std::sync::Mutex;
use std::time::Instant;

fn main() {
    let line = b"\x1b[32mCompiling personal sample\x1b[0m: progress 1234567890\r\n";
    for size in [64 * 1024, crc::term::hold::KEEP] {
        let output: Vec<u8> = line.iter().copied().cycle().take(size).collect();
        for chunk in [size, 4096] {
            let terminal = Mutex::new(Term::new(120, 40));
            let mut held = Vec::new();
            let started = Instant::now();
            for _ in 0..8 {
                for bytes in output.chunks(chunk) {
                    let mut term = terminal.lock().unwrap();
                    let at = Instant::now();
                    term.advance(bytes);
                    held.push(at.elapsed().as_secs_f64() * 1000.0);
                }
            }
            held.sort_by(f64::total_cmp);
            println!(
                "burst={size} chunk={chunk}: lock p99={:.3} ms max={:.3} ms total={:.1} ms",
                held[(held.len() * 99 / 100).min(held.len() - 1)],
                held.last().unwrap(),
                started.elapsed().as_secs_f64() * 1000.0,
            );
        }
    }
}
