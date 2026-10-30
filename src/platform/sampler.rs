//! Where a slow turn of the main thread spent its time, by function. The
//! jank log's laps say which phase was slow; a phase can be a function of
//! hundreds of lines (a click), so they cannot say what inside it.
//!
//! A thread of its own waits while turns are short. Once one has run
//! [`FIRST`], it stops the main thread every [`EVERY`], copies its program
//! counter and the chain of frame pointers into an array on its own stack,
//! and lets it go. Nothing is allocated and no lock is taken while the main
//! thread is stopped: it may hold the allocator's lock. The addresses are
//! named afterwards, on the main thread, once the turn is over and only if
//! it was slow.
//!
//! On Apple Silicon every function keeps a frame pointer (the platform ABI
//! requires it), so the chain is the call stack.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A turn this old starts being sampled. Under the jank log's threshold, so
/// a turn written down has samples from most of its length.
const FIRST: Duration = Duration::from_millis(16);
/// Between two samples.
const EVERY: Duration = Duration::from_millis(2);
/// The deepest stack kept.
const DEPTH: usize = 96;
/// The most samples one turn keeps: eight seconds of them.
const MOST: usize = 4000;
/// The functions a line names.
const NAMED: usize = 3;

/// The thread being sampled, as the kernel names it, and its stack.
#[derive(Clone, Copy)]
struct Target {
    port: u32,
    low: usize,
    high: usize,
}

struct Sampler {
    thread: std::thread::Thread,
}

static SAMPLER: OnceLock<Option<Sampler>> = OnceLock::new();
/// The turn under way: its start in nanoseconds since [`epoch`], 0 for
/// none. The start is the turn's identity too.
static STARTED: AtomicU64 = AtomicU64::new(0);
/// The samples of the turn that started at `.0`.
static SAMPLES: Mutex<(u64, Vec<Stack>)> = Mutex::new((0, Vec::new()));

type Stack = Vec<usize>;

fn epoch() -> Instant {
    static AT: OnceLock<Instant> = OnceLock::new();
    *AT.get_or_init(Instant::now)
}

fn now() -> u64 {
    // At least 1, as 0 is "no turn".
    (epoch().elapsed().as_nanos() as u64).max(1)
}

/// A turn begins on the main thread. The first one starts the sampler.
pub fn begin() {
    let Some(sampler) = SAMPLER.get_or_init(start) else {
        return;
    };
    STARTED.store(now(), Ordering::Release);
    sampler.thread.unpark();
}

/// The turn is over: its samples, if any were taken.
pub fn end() -> Vec<Stack> {
    let started = STARTED.swap(0, Ordering::AcqRel);
    let mut samples = SAMPLES.lock().unwrap_or_else(|e| e.into_inner());
    if samples.0 != started {
        return Vec::new();
    }
    samples.0 = 0;
    std::mem::take(&mut samples.1)
}

/// Run on the main thread: it is the one sampled.
fn start() -> Option<Sampler> {
    let main = current()?;
    let thread = std::thread::Builder::new()
        .name("crc-jank-sample".into())
        .spawn(move || run(main))
        .ok()?
        .thread()
        .clone();
    Some(Sampler { thread })
}

fn run(main: Target) {
    loop {
        std::thread::park();
        loop {
            let started = STARTED.load(Ordering::Acquire);
            if started == 0 {
                break;
            }
            let age = Duration::from_nanos(now().saturating_sub(started));
            if age < FIRST {
                std::thread::sleep(FIRST - age);
                continue;
            }
            let mut frames = [0usize; DEPTH];
            // SAFETY: `main` is this process's main thread, which lives as
            // long as the process; see `capture`.
            let n = unsafe { capture(main, &mut frames) };
            // Kept only if it is still the turn it was taken in.
            if n > 0 && STARTED.load(Ordering::Acquire) == started {
                let mut samples = SAMPLES.lock().unwrap_or_else(|e| e.into_inner());
                if samples.0 != started {
                    *samples = (started, Vec::new());
                }
                if samples.1.len() < MOST {
                    samples.1.push(frames[..n].to_vec());
                }
            }
            std::thread::sleep(EVERY);
        }
    }
}

/// The calling thread, as a target.
fn current() -> Option<Target> {
    // SAFETY: plain queries about the calling thread.
    unsafe {
        let me = pthread_self();
        let port = pthread_mach_thread_np(me);
        let high = pthread_get_stackaddr_np(me) as usize;
        let size = pthread_get_stacksize_np(me);
        (port != 0 && high > size).then(|| Target {
            port,
            low: high - size,
            high,
        })
    }
}

/// The user-space part of an address: return addresses saved by system
/// code carry a pointer signature in the bits above it.
fn strip(address: u64) -> usize {
    (address & ((1u64 << 47) - 1)) as usize
}

/// Stops `target`, copies its program counter and return addresses into
/// `frames`, starts it again; how many it copied. Allocates nothing and
/// takes no lock while it is stopped.
///
/// # Safety
///
/// `target` names a thread of this process that outlives the call, and
/// its stack lies within `low..high`.
unsafe fn capture(target: Target, frames: &mut [usize; DEPTH]) -> usize {
    // SAFETY: as the caller promises, a live thread of ours.
    if unsafe { thread_suspend(target.port) } != 0 {
        return 0;
    }
    let mut state = ThreadState::default();
    let mut count = THREAD_STATE_COUNT;
    // SAFETY: `state` is the kernel's ARM_THREAD_STATE64, `count` its size
    // in 32-bit words.
    let got = unsafe {
        thread_get_state(
            target.port,
            ARM_THREAD_STATE64,
            (&mut state as *mut ThreadState).cast(),
            &mut count,
        )
    };
    let mut n = 0;
    if got == 0 {
        frames[0] = strip(state.pc);
        n = 1;
        let mut fp = strip(state.fp);
        // Each frame: the caller's frame pointer, then the return address.
        while n < DEPTH && fp >= target.low && fp + 16 <= target.high && fp.is_multiple_of(8) {
            // SAFETY: inside the stopped thread's stack, checked above.
            let (next, ret) = unsafe { (*(fp as *const u64), *((fp + 8) as *const u64)) };
            let ret = strip(ret);
            if ret == 0 {
                break;
            }
            frames[n] = ret;
            n += 1;
            let next = strip(next);
            if next <= fp {
                break;
            }
            fp = next;
        }
    }
    // SAFETY: suspended above, once.
    unsafe { thread_resume(target.port) };
    n
}

/// One sample's place: the innermost function of crc's own, and the
/// library it had called into, if it was in one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Place {
    function: String,
    library: Option<String>,
}

/// Names addresses, remembering each.
#[derive(Default)]
struct Names {
    known: HashMap<usize, Option<(String, String)>>,
    ours: Option<String>,
}

impl Names {
    /// The function and the file of the image `address` is in.
    fn of(&mut self, address: usize) -> Option<(String, String)> {
        self.known
            .entry(address)
            .or_insert_with(|| symbol(address))
            .clone()
    }

    /// Where a stack, innermost first, was.
    fn place(&mut self, stack: &[usize]) -> Option<Place> {
        let ours = self
            .ours
            .get_or_insert_with(|| {
                symbol(place_marker as *const () as usize).map_or_else(String::new, |(_, f)| f)
            })
            .clone();
        let mut library = None;
        for (n, &address) in stack.iter().enumerate() {
            // A return address points after its call: one back is the call.
            let at = if n == 0 { address } else { address - 1 };
            let Some((function, file)) = self.of(at) else {
                continue;
            };
            if file == ours {
                return Some(Place {
                    function: short_name(&function),
                    library,
                });
            }
            if library.is_none() {
                library = file.rsplit('/').next().map(str::to_owned);
            }
        }
        None
    }
}

/// An address inside crc's own image, to know it by.
#[inline(never)]
fn place_marker() {}

/// What `dladdr` says of an address: its function and its image's file.
fn symbol(address: usize) -> Option<(String, String)> {
    let mut info = DlInfo {
        file: std::ptr::null(),
        base: std::ptr::null_mut(),
        name: std::ptr::null(),
        start: std::ptr::null_mut(),
    };
    // SAFETY: dladdr only reads the loader's tables; the strings it
    // returns live as long as the image, and are copied at once.
    unsafe {
        if dladdr(address as *const u8, &mut info) == 0 || info.name.is_null() {
            return None;
        }
        let name = std::ffi::CStr::from_ptr(info.name)
            .to_string_lossy()
            .into_owned();
        let file = if info.file.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(info.file)
                .to_string_lossy()
                .into_owned()
        };
        Some((name, file))
    }
}

/// A Rust symbol as a short path, its last three parts: `font::Atlas::known`
/// from `_RNvMs1_NtNtCs..._3crc6render4fontNtB5_5Atlas5known`, legacy
/// `_ZN...17h<hash>E` names the same way. Anything else as it is.
pub fn short_name(symbol: &str) -> String {
    let Some(body) = symbol
        .strip_prefix("_R")
        .or_else(|| symbol.strip_prefix("_ZN"))
    else {
        return symbol.trim_start_matches('_').to_owned();
    };
    // Left to right, each identifier taken whole by its length, so its
    // letters are never read as anything else. Around them: a namespace
    // tag and its letter (`Nv`), backrefs and disambiguators (`B5_`,
    // `s1_`, `Cs..._`), and single-letter tags, all skipped.
    let v0 = symbol.starts_with("_R");
    let bytes = body.as_bytes();
    let mut parts: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' => {
                let digits = bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count();
                let len: usize = body[i..i + digits].parse().unwrap_or(usize::MAX);
                let mut from = i + digits;
                // A v0 identifier that starts with a digit or `_` has a `_`
                // before it.
                if v0 && bytes.get(from) == Some(&b'_') {
                    from += 1;
                }
                let Some(ident) = from.checked_add(len).and_then(|to| body.get(from..to)) else {
                    break;
                };
                if !ident.is_empty() {
                    parts.push(ident);
                }
                i = from + len;
            }
            b'B' | b's' if v0 => {
                i += bytes[i..]
                    .iter()
                    .position(|&b| b == b'_')
                    .map_or(bytes.len(), |p| p + 1);
            }
            b'N' if v0 => i += 2,
            _ => i += 1,
        }
    }
    // A legacy name ends with its hash.
    if parts.last().is_some_and(|p| {
        p.len() == 17 && p.starts_with('h') && p[1..].bytes().all(|b| b.is_ascii_hexdigit())
    }) {
        parts.pop();
    }
    if parts.first() == Some(&"crc") {
        parts.remove(0);
    }
    if parts.is_empty() {
        return symbol.to_owned();
    }
    let from = parts.len().saturating_sub(3);
    parts[from..].join("::")
}

/// The functions a slow turn's samples were in, the most first, as a
/// line's tail: `samples 9: inspector::Inspector::draw_body 56%, ...`.
/// Empty when there are none.
pub fn summary(samples: &[Stack]) -> String {
    if samples.is_empty() {
        return String::new();
    }
    let mut names = Names::default();
    // Per function: its samples, and how many of them were in each library.
    let mut counts: HashMap<String, (usize, HashMap<Option<String>, usize>)> = HashMap::new();
    for stack in samples {
        let place = names.place(stack).unwrap_or(Place {
            function: "outside crc".into(),
            library: None,
        });
        let (n, libraries) = counts.entry(place.function).or_default();
        *n += 1;
        *libraries.entry(place.library).or_default() += 1;
    }
    let places = counts
        .into_iter()
        .map(|(function, (n, libraries))| {
            // The library it was mostly in, when it was in one most of
            // the time.
            let library = libraries
                .into_iter()
                .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
                .filter(|(_, m)| m * 2 > n)
                .and_then(|(library, _)| library);
            (function, n, library)
        })
        .collect();
    describe(samples.len(), places)
}

fn describe(total: usize, mut places: Vec<(String, usize, Option<String>)>) -> String {
    places.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let named: Vec<String> = places
        .iter()
        .take(NAMED)
        .map(|(function, n, library)| {
            let share = n * 100 / total;
            match library {
                Some(library) => format!("{function} (in {library}) {share}%"),
                None => format!("{function} {share}%"),
            }
        })
        .collect();
    format!("samples {total}: {}", named.join(", "))
}

#[repr(C)]
struct DlInfo {
    file: *const std::ffi::c_char,
    base: *mut u8,
    name: *const std::ffi::c_char,
    start: *mut u8,
}

/// The kernel's ARM_THREAD_STATE64: x0 to x28, fp, lr, sp, pc, cpsr and
/// padding.
#[repr(C)]
#[derive(Default)]
struct ThreadState {
    x: [u64; 29],
    fp: u64,
    lr: u64,
    sp: u64,
    pc: u64,
    cpsr: u32,
    pad: u32,
}

const ARM_THREAD_STATE64: i32 = 6;
const THREAD_STATE_COUNT: u32 = (std::mem::size_of::<ThreadState>() / 4) as u32;

unsafe extern "C" {
    fn pthread_self() -> usize;
    fn pthread_mach_thread_np(thread: usize) -> u32;
    fn pthread_get_stackaddr_np(thread: usize) -> *mut u8;
    fn pthread_get_stacksize_np(thread: usize) -> usize;
    fn thread_suspend(thread: u32) -> i32;
    fn thread_resume(thread: u32) -> i32;
    fn thread_get_state(thread: u32, flavor: i32, state: *mut u32, count: *mut u32) -> i32;
    fn dladdr(address: *const u8, info: *mut DlInfo) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn a_symbol_reads_as_its_last_three_parts() {
        assert_eq!(
            short_name("_RNvMs1_NtNtCsiXgLn5Xs9Su_3crc6render4fontNtB5_5Atlas5known"),
            "font::Atlas::known"
        );
        assert_eq!(
            short_name("_RNvNtNtCsiXgLn5Xs9Su_3crc8platform8terminal4draw"),
            "platform::terminal::draw"
        );
        assert_eq!(
            short_name("_ZN3std2io5stdio6_print17h0123456789abcdefE"),
            "io::stdio::_print"
        );
        assert_eq!(short_name("_objc_msgSend"), "objc_msgSend");
        assert_eq!(short_name("-[NSView display]"), "-[NSView display]");
    }

    #[test]
    fn a_summary_names_the_most_sampled_first_with_shares() {
        let place = |f: &str, n: usize, l: Option<&str>| (f.to_owned(), n, l.map(str::to_owned));
        let places = vec![
            place("git::status", 1, None),
            place("inspector::Inspector::draw_body", 5, None),
            place("font::Atlas::shape", 3, Some("CoreText")),
            place("tiny", 1, None),
        ];
        assert_eq!(
            describe(10, places),
            "samples 10: inspector::Inspector::draw_body 50%, \
             font::Atlas::shape (in CoreText) 30%, git::status 10%"
        );
    }

    #[inline(never)]
    fn spin_in_a_known_place(stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            std::hint::spin_loop();
        }
    }

    /// The real path: another thread stopped, walked and started again,
    /// and its samples named as the function it was spinning in.
    #[test]
    fn a_stopped_thread_is_named_where_it_was() {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let spinner = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                tx.send(current().map(|t| (t.port, t.low, t.high))).unwrap();
                spin_in_a_known_place(&stop);
            })
        };
        let (port, low, high) = rx.recv().unwrap().expect("target");
        let target = Target { port, low, high };
        std::thread::sleep(Duration::from_millis(20));
        let mut samples = Vec::new();
        for _ in 0..20 {
            let mut frames = [0usize; DEPTH];
            // SAFETY: the spinner lives until `stop` is set below.
            let n = unsafe { capture(target, &mut frames) };
            assert!(n >= 2, "a stack of {n}");
            samples.push(frames[..n].to_vec());
            std::thread::sleep(Duration::from_millis(1));
        }
        stop.store(true, Ordering::Relaxed);
        spinner.join().unwrap();
        let said = summary(&samples);
        assert!(
            said.starts_with("samples 20: ") && said.contains("spin_in_a_known_place"),
            "{said}"
        );
    }
}
