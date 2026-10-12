//! The hang recorder: a thread that asks the main thread to answer every
//! second and a half. When it has not, the main thread is stuck (the
//! spinning cursor), and the recorder has macOS's `sample` write where it
//! is stuck to ~/Library/Logs/crc/hang-<time>.txt, at most once a minute.
//! A stall seen once can then be named instead of guessed at.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long the main thread may go without answering.
const PATIENCE: Duration = Duration::from_millis(1500);
/// Between two recordings, however many stalls.
const QUIET: Duration = Duration::from_secs(60);

static ASKED: AtomicU64 = AtomicU64::new(0);
static ANSWERED: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn answer(_: *mut c_void) {
    ANSWERED.store(ASKED.load(Ordering::Acquire), Ordering::Release);
}

/// Starts the recorder. A test instance has none: its scripted waits hold
/// the main thread on purpose.
pub fn watch() {
    if std::env::var_os("CRC_SELFTEST").is_some() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("crc-hang-watch".into())
        .spawn(|| {
            let mut recorded: Option<Instant> = None;
            loop {
                let asked = ASKED.fetch_add(1, Ordering::AcqRel) + 1;
                // SAFETY: `answer` touches only the two statics.
                unsafe { super::dispatch::on_main(std::ptr::null_mut(), answer) };
                std::thread::sleep(PATIENCE);
                if ANSWERED.load(Ordering::Acquire) >= asked {
                    continue;
                }
                if recorded.is_none_or(|at| at.elapsed() >= QUIET) {
                    recorded = Some(Instant::now());
                    record();
                }
                // One stall, one recording: wait for it to end.
                while ANSWERED.load(Ordering::Acquire) < asked {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        });
}

/// Three seconds of every thread's stack, while the main one is stuck.
fn record() {
    let Some(dir) = super::logs() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let file = dir.join(format!("hang-{stamp}.txt"));
    let _ = std::process::Command::new("/usr/bin/sample")
        .arg(std::process::id().to_string())
        .arg("3")
        .arg("-file")
        .arg(&file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}
