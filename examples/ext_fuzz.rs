//! Long fuzz of the extension interpreter: corrupted copies of real
//! extensions, loaded against their manifests and run as crc runs them,
//! with a smaller budget so more cases fit. Nothing may panic. Run with
//! overflow checks (the default dev profile):
//!
//!     cargo run --example ext_fuzz -- [--rounds N | --seconds N] [--seed S]
//!     cargo run --example ext_fuzz -- case.wasm      replay a saved case
//!
//! A case that panics is written to ext-fuzz-panic-<round>.wasm, and the
//! run exits 1. The seed is printed first, so a failing run can be repeated
//! exactly. The test suite runs a short version on every `cargo test`, and
//! CI runs this for 25 minutes every night (.github/workflows/fuzz.yml).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crc::ext::run::{Request, load};
use crc::ext::store::Package;

/// A real extension, and how to call it.
struct Base {
    package: Package,
    command: String,
    selection: bool,
    language: &'static str,
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/extensions")
        .join(name)
}

fn bases() -> Vec<Base> {
    let sort = Package::from_folder(&fixture("sort-lines")).expect("the Sort Lines fixture");
    let preview =
        Package::from_folder(&fixture("markdown-preview")).expect("the Markdown Preview fixture");
    vec![
        Base {
            package: sort,
            command: "sort".into(),
            selection: true,
            language: "text",
        },
        Base {
            package: preview,
            command: "preview".into(),
            selection: false,
            language: "markdown",
        },
    ]
}

/// Loads `bytes` against `base`'s manifest and runs its command on a little
/// text, as crc would. `None` when it is refused or traps; either is fine.
fn attempt(base: &Base, bytes: &[u8]) -> Option<()> {
    let mut loaded = load(base.package.manifest.clone(), bytes).ok()?;
    loaded.fuel = 2_000_000;
    loaded.deadline = Duration::from_millis(200);
    let request = Request {
        command: base.command.clone(),
        text: "# b\n\n- a\n\n| x |\n|---|\n| 1 |\n".into(),
        selection: base.selection,
        language: base.language.into(),
    };
    loaded.run(&request).ok().map(|_| ())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let bases = bases();
    // `ext_fuzz <file.wasm>` replays one saved case with the panic shown.
    if let Some(file) = args.first().filter(|a| a.ends_with(".wasm")) {
        let bytes = std::fs::read(file).expect("the case");
        for base in &bases {
            println!(
                "as {}: {:?}",
                base.package.manifest.id,
                attempt(base, &bytes)
            );
        }
        return;
    }
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<u64>().ok())
    };
    let seconds = value("--seconds");
    let rounds = value("--rounds")
        .or_else(|| args.first().and_then(|a| a.parse().ok()))
        .unwrap_or(200_000);
    let mut seed: u64 = value("--seed").unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1)
    }) | 1;
    println!("seed {seed}");
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    std::panic::set_hook(Box::new(|_| {}));
    let started = Instant::now();
    let (mut round, mut ran, mut refused, mut panics) = (0u64, 0u64, 0u64, 0u64);
    loop {
        match seconds {
            Some(s) if started.elapsed() >= Duration::from_secs(s) => break,
            None if round >= rounds => break,
            _ => {}
        }
        let base = &bases[(rnd() as usize) % bases.len()];
        let mut bytes = base.package.wasm.clone();
        let body = bytes.len() - 8;
        match rnd() % 4 {
            // Flip a few bytes.
            0 => {
                for _ in 0..1 + rnd() % 8 {
                    let at = 8 + (rnd() as usize) % body;
                    bytes[at] = rnd() as u8;
                }
            }
            // Cut it short.
            1 => bytes.truncate(8 + (rnd() as usize) % body),
            // Insert junk.
            2 => {
                let at = 8 + (rnd() as usize) % body;
                let junk: Vec<u8> = (0..1 + rnd() % 16).map(|_| rnd() as u8).collect();
                bytes.splice(at..at, junk);
            }
            // Copy a run from elsewhere in the module.
            _ => {
                let len = 1 + (rnd() as usize) % 64;
                let from = 8 + (rnd() as usize) % (body - len);
                let to = 8 + (rnd() as usize) % (body - len);
                let run = bytes[from..from + len].to_vec();
                bytes[to..to + len].copy_from_slice(&run);
            }
        }
        match std::panic::catch_unwind(|| attempt(base, &bytes)) {
            Ok(Some(())) => ran += 1,
            Ok(None) => refused += 1,
            Err(_) => {
                panics += 1;
                std::fs::write(format!("ext-fuzz-panic-{round}.wasm"), &bytes).ok();
            }
        }
        round += 1;
    }
    println!(
        "{round} rounds in {:.0?}: ran {ran}, refused or trapped {refused}, panics {panics}",
        started.elapsed()
    );
    std::process::exit(if panics == 0 { 0 } else { 1 });
}
