//! The jank log: the main thread's turns that took longer than a 30Hz
//! frame, with the parts that took the time. The hang recorder sees a stall
//! of a second and a half; a lag a person feels while typing is a tenth of
//! that, and the recorder never fires on it.
//!
//! An event handler, the display link or a wake opens a turn with
//! [`enter`]; the code inside names what it just finished with [`lap`].
//! A turn opened inside another (a nested modal loop, a draw from a key)
//! belongs to the outer one. A slow turn appends one line to
//! ~/Library/Logs/crc/jank.log: when, what opened it, how long, and the
//! laps that took the most. The rest of the time is "unnamed". After
//! them, from [`super::sampler`], the functions the turn was in.
//!
//! A menu or a modal panel runs its own loop inside the turn that opened
//! it, for as long as it stays up. [`modal`] (or a [`Pause`]) leaves that
//! wait out of the turn's time.

use std::cell::RefCell;
use std::io::Write;
use std::time::{Duration, Instant};

/// A turn that took this long is written down.
const SLOW: Duration = Duration::from_millis(33);
/// Past this, the log moves to jank.1.log and starts again.
const KEEP: u64 = 512 * 1024;
/// The laps a line names, the slowest first.
const NAMED: usize = 6;

struct Turn {
    entry: &'static str,
    started: Instant,
    last: Instant,
    /// Time spent waiting in a nested modal loop, not counted as work.
    paused: Duration,
    /// A pause is open: the display link still fires in a menu's loop
    /// and joins the turn, but its laps would be the wait.
    pausing: bool,
    laps: Vec<(&'static str, Duration)>,
}

/// The time a turn spent working: since it opened, less its pauses.
fn busy(turn: &Turn) -> Duration {
    turn.started.elapsed().saturating_sub(turn.paused)
}

thread_local! {
    static TURN: RefCell<Option<Turn>> = const { RefCell::new(None) };
    /// The spare lap list, so a turn allocates nothing.
    static SPARE: RefCell<Vec<(&'static str, Duration)>> = const { RefCell::new(Vec::new()) };
}

/// A turn kept open while it lives. Only the outermost one closes it.
pub struct Entry {
    outer: bool,
}

/// Opens a turn named `entry`, or joins the turn already open.
pub fn enter(entry: &'static str) -> Entry {
    if !enabled() {
        return Entry { outer: false };
    }
    TURN.with_borrow_mut(|turn| {
        if turn.is_some() {
            return Entry { outer: false };
        }
        let now = Instant::now();
        let mut laps = SPARE.with_borrow_mut(std::mem::take);
        laps.clear();
        *turn = Some(Turn {
            entry,
            started: now,
            last: now,
            paused: Duration::ZERO,
            pausing: false,
            laps,
        });
        super::sampler::begin();
        Entry { outer: true }
    })
}

/// Names the time since the last lap (or the turn's start). Laps of one
/// name add up.
pub fn lap(name: &'static str) {
    TURN.with_borrow_mut(|turn| {
        let Some(turn) = turn.as_mut().filter(|t| !t.pausing) else {
            return;
        };
        let now = Instant::now();
        let took = now - turn.last;
        turn.last = now;
        match turn.laps.iter_mut().find(|(n, _)| *n == name) {
            Some((_, sum)) => *sum += took,
            None => turn.laps.push((name, took)),
        }
    });
}

/// A wait left out of the open turn while it lives.
pub struct Pause(Option<Instant>);

/// Starts a pause in the open turn; nothing when no turn is open or a
/// pause already is.
pub fn pause() -> Pause {
    TURN.with_borrow_mut(|turn| match turn {
        Some(turn) if !turn.pausing => {
            turn.pausing = true;
            Pause(Some(Instant::now()))
        }
        _ => Pause(None),
    })
}

impl Drop for Pause {
    fn drop(&mut self) {
        let Some(since) = self.0 else {
            return;
        };
        let waited = since.elapsed();
        TURN.with_borrow_mut(|turn| {
            if let Some(turn) = turn {
                turn.pausing = false;
                turn.paused += waited;
                // The next lap starts after the wait, not before it.
                turn.last += waited;
            }
        });
    }
}

/// Runs `f`, a nested modal loop (a context menu, an open panel, an
/// alert), with its time left out of the open turn.
pub fn modal<T>(f: impl FnOnce() -> T) -> T {
    let _pause = pause();
    f()
}

impl Drop for Entry {
    fn drop(&mut self) {
        if !self.outer {
            return;
        }
        let Some(turn) = TURN.with_borrow_mut(Option::take) else {
            return;
        };
        let took = busy(&turn);
        let samples = super::sampler::end();
        if took >= SLOW {
            let mut line = line(turn.entry, took, &turn.laps);
            let said = super::sampler::summary(&samples);
            if !said.is_empty() {
                line.insert_str(line.len() - 1, &format!(" | {said}"));
            }
            write(&line);
        }
        SPARE.with_borrow_mut(|spare| *spare = turn.laps);
    }
}

/// Off in a test instance, whose scripted waits hold the main thread on
/// purpose, as the hang recorder is.
fn enabled() -> bool {
    thread_local! {
        static ON: bool = std::env::var_os("CRC_SELFTEST").is_none();
    }
    ON.with(|on| *on)
}

/// One log line: `unix-seconds clock entry total: lap ms, ...`.
fn line(entry: &str, took: Duration, laps: &[(&'static str, Duration)]) -> String {
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let mut sorted = laps.to_vec();
    sorted.sort_by_key(|(_, d)| std::cmp::Reverse(*d));
    let named: Duration = laps.iter().map(|(_, d)| *d).sum();
    let mut parts: Vec<String> = sorted
        .iter()
        .take(NAMED)
        .filter(|(_, d)| ms(*d) >= 0.5)
        .map(|(n, d)| format!("{n} {:.1}", ms(*d)))
        .collect();
    let unnamed = took.saturating_sub(named);
    if ms(unnamed) >= 0.5 {
        parts.push(format!("unnamed {:.1}", ms(unnamed)));
    }
    let now = super::unix_seconds() as i64;
    format!(
        "{now} {} {entry} {:.1} ms: {}\n",
        super::clock_time(now, now),
        ms(took),
        parts.join(", ")
    )
}

fn write(line: &str) {
    let Some(dir) = super::logs() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("jank.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > KEEP) {
        let _ = std::fs::rename(&path, dir.join("jank.1.log"));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_names_the_slowest_laps_first_and_the_rest_unnamed() {
        let laps = [
            ("poll_git", Duration::from_millis(2)),
            ("draw.build", Duration::from_millis(30)),
            ("tiny", Duration::from_micros(100)),
        ];
        let line = line("displayLink", Duration::from_millis(40), &laps);
        let body = line.split_once("displayLink ").unwrap().1;
        assert_eq!(
            body,
            "40.0 ms: draw.build 30.0, poll_git 2.0, unnamed 7.9\n"
        );
    }

    #[test]
    fn a_nested_turn_joins_the_outer_one() {
        TURN.with_borrow_mut(|t| *t = None);
        let outer = Entry { outer: true };
        TURN.with_borrow_mut(|t| {
            let now = Instant::now();
            *t = Some(Turn {
                entry: "keyDown",
                started: now,
                last: now,
                paused: Duration::ZERO,
                pausing: false,
                laps: Vec::new(),
            });
        });
        let inner = enter("displayLink");
        assert!(!inner.outer);
        lap("a");
        lap("a");
        drop(inner);
        assert!(TURN.with_borrow(|t| t.as_ref().is_some_and(|t| t.laps.len() == 1)));
        // The outer turn closes; it was fast, so nothing is written.
        drop(outer);
        assert!(TURN.with_borrow(Option::is_none));
    }

    #[test]
    fn time_in_a_modal_pause_is_not_counted() {
        let now = Instant::now();
        let turn = Turn {
            entry: "mouseDown",
            started: now - Duration::from_secs(3),
            last: now,
            paused: Duration::from_millis(2980),
            pausing: false,
            laps: Vec::new(),
        };
        assert!(busy(&turn) < SLOW);
        assert!(busy(&turn) >= Duration::from_millis(20));
        assert_eq!(turn.paused, Duration::from_millis(2980));
    }

    #[test]
    fn a_pause_counts_only_inside_an_open_turn() {
        TURN.with_borrow_mut(|t| *t = None);
        let idle = pause();
        assert!(idle.0.is_none());
        drop(idle);
        assert!(TURN.with_borrow(Option::is_none));

        let now = Instant::now();
        TURN.with_borrow_mut(|t| {
            *t = Some(Turn {
                entry: "mouseDown",
                started: now,
                last: now,
                paused: Duration::ZERO,
                pausing: false,
                laps: Vec::new(),
            });
        });
        let mut held = pause();
        assert!(held.0.is_some());
        // A pause inside it, and a lap from the display link, add nothing.
        assert!(pause().0.is_none());
        lap("deferred");
        // As if the menu stayed up for two seconds.
        held.0 = Some(Instant::now() - Duration::from_secs(2));
        drop(held);
        TURN.with_borrow(|t| {
            let t = t.as_ref().unwrap();
            assert!(t.paused >= Duration::from_secs(2));
            assert!(t.last >= now + Duration::from_secs(2));
            assert!(busy(t) < SLOW);
            assert!(t.laps.is_empty());
            assert!(!t.pausing);
        });
        TURN.with_borrow_mut(|t| *t = None);
    }
}
