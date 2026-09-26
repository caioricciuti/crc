//! Long fuzz of the extension interpreter: corrupted copies of a real
//! extension, parsed, instantiated and called with a budget. Nothing may
//! panic. Run with overflow checks (the default dev profile):
//!
//!     cargo run --example ext_fuzz -- [rounds]
//!
//! The test suite runs a short version of this on every `cargo test`.

use crc::ext::wasm::{Instance, Module};

fn main() {
    // `ext_fuzz <file.wasm>` replays one saved case with the panic shown.
    if let Some(file) = std::env::args().nth(1).filter(|a| a.ends_with(".wasm")) {
        let bytes = std::fs::read(file).expect("the case");
        let module = Module::parse(&bytes);
        println!("parse: {:?}", module.as_ref().err());
        let mut instance = Instance::new(module.expect("parses"), 64, |_, _| {
            Some(Box::new(|_: &mut [u8], _: &[u64]| Ok(None)))
        })
        .expect("instantiates");
        let alloc = instance.func("crc_alloc").expect("alloc");
        let sort = instance.func("sort").expect("sort");
        instance.fuel = 2_000_000;
        let ptr = instance
            .call(alloc, &[16])
            .expect("alloc runs")
            .first()
            .copied()
            .unwrap_or(0);
        instance.fuel = 2_000_000;
        println!("{:?}", instance.call(sort, &[ptr, 16]));
        return;
    }
    let rounds: u64 = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(200_000);
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/extensions/sort-lines/sort_lines.wasm"
    );
    let base = std::fs::read(path).expect("the fixture");
    std::panic::set_hook(Box::new(|_| {}));
    let mut seed: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        | 1;
    println!("seed {seed:#x}");
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let (mut ran, mut refused, mut panics) = (0u64, 0u64, 0u64);
    for round in 0..rounds {
        let mut bytes = base.clone();
        match rnd() % 4 {
            // Flip a few bytes.
            0 => {
                for _ in 0..1 + rnd() % 8 {
                    let at = 8 + (rnd() as usize) % (bytes.len() - 8);
                    bytes[at] = rnd() as u8;
                }
            }
            // Cut it short.
            1 => bytes.truncate(8 + (rnd() as usize) % (bytes.len() - 8)),
            // Insert junk.
            2 => {
                let at = 8 + (rnd() as usize) % (bytes.len() - 8);
                let junk: Vec<u8> = (0..1 + rnd() % 16).map(|_| rnd() as u8).collect();
                bytes.splice(at..at, junk);
            }
            // Copy a run from elsewhere in the module.
            _ => {
                let len = 1 + (rnd() as usize) % 64;
                let from = 8 + (rnd() as usize) % (bytes.len() - 8 - len);
                let to = 8 + (rnd() as usize) % (bytes.len() - 8 - len);
                let run = bytes[from..from + len].to_vec();
                bytes[to..to + len].copy_from_slice(&run);
            }
        }
        let outcome = std::panic::catch_unwind(|| {
            let module = Module::parse(&bytes).ok()?;
            let mut instance = Instance::new(module, 64, |_, _| {
                Some(Box::new(|_: &mut [u8], _: &[u64]| Ok(None)))
            })
            .ok()?;
            let alloc = instance.func("crc_alloc")?;
            let sort = instance.func("sort")?;
            instance.fuel = 2_000_000;
            let ptr = *instance.call(alloc, &[16]).ok()?.first()?;
            instance.fuel = 2_000_000;
            instance.call(sort, &[ptr, 16]).ok()
        });
        match outcome {
            Ok(Some(_)) => ran += 1,
            Ok(None) => refused += 1,
            Err(_) => {
                panics += 1;
                std::fs::write(format!("ext-fuzz-panic-{round}.wasm"), &bytes).ok();
            }
        }
    }
    println!("{rounds} rounds: ran {ran}, refused or trapped {refused}, panics {panics}");
    std::process::exit(if panics == 0 { 0 } else { 1 });
}
