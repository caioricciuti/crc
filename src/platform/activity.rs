//! The whole Mac at a glance, at the foot of the Agents sidebar: CPU,
//! memory, disk and network, and the three processes that used the most
//! CPU. Read from the kernel's own counters on a worker, every two seconds
//! and only while the panel shows unfolded; a rate is the change between
//! two reads. No crate: the calls are the ones Activity Monitor's numbers
//! come from, declared here.

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::platform::dispatch::Wake;
use crate::render::{
    font::Atlas,
    layout::{self, Theme, Viewport},
    metal::GlyphInstance,
};

/// How often the panel is read while it shows.
pub const EVERY: Duration = Duration::from_secs(2);
/// The first read after the panel opens: sooner, so its skeleton does not
/// stay long.
const FIRST: Duration = Duration::from_secs(1);
/// The CPU samples the sparkline keeps: a minute at [`EVERY`].
pub const HISTORY: usize = 30;
/// The processes listed under the totals.
pub const TOP: usize = 3;
/// Whole-Mac CPU over this, in percent, is drawn in the theme's red.
const HOT: f32 = 80.0;
/// The title row, as tall as the list's rows.
const TITLE: f32 = 26.0;
/// A total's or a process's line.
const LINE: f32 = 20.0;
/// The CPU chart under its line.
const CHART: f32 = 28.0;
/// The memory meter under its line.
const METER: f32 = 4.0;
/// A card's inset: its lines from its sides, and from its top and bottom.
const PAD_X: f32 = 10.0;
const PAD_Y: f32 = 4.0;
/// The card of totals: CPU and its chart, memory and its meter, disk and
/// network.
const CARD: f32 = PAD_Y + LINE + 2.0 + CHART + 6.0 + LINE + 2.0 + METER + 6.0 + 2.0 * LINE + PAD_Y;
/// The small heading over the processes.
const HEADING: f32 = 18.0;
/// Air under the last line.
const FOOT: f32 = 6.0;

/// How hard the kernel says memory is pressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pressure {
    Ok,
    Warn,
    Critical,
}

impl Pressure {
    /// From `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warning,
    /// 4 critical.
    fn of(level: i32) -> Pressure {
        match level {
            4 => Pressure::Critical,
            2 => Pressure::Warn,
            _ => Pressure::Ok,
        }
    }

    /// The word its tag says; none when memory is not pressed, which
    /// needs no word.
    fn label(self) -> Option<&'static str> {
        match self {
            Pressure::Ok => None,
            Pressure::Warn => Some("High"),
            Pressure::Critical => Some("Critical"),
        }
    }
}

/// One of the busiest processes, as its row shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Busy {
    pub name: String,
    /// Of one core, as Activity Monitor counts it: `212%` is two cores
    /// and some.
    pub cpu: String,
    pub memory: String,
}

/// One read, formatted on the worker: a frame only draws strings, and a
/// frame without a new read shapes nothing new.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub cpu: String,
    pub cpu_hot: bool,
    /// Whole-Mac CPU from 0 to 1, oldest first, for the sparkline.
    pub history: Vec<f32>,
    pub memory: String,
    /// Memory in use, from 0 to 1, for the meter.
    pub memory_used: f32,
    pub pressure: Pressure,
    /// Bytes a second read and written.
    pub disk: [String; 2],
    /// Bytes a second in and out.
    pub net: [String; 2],
    pub top: Vec<Busy>,
}

/// What the kernel counts up, at one moment: every rate is the change
/// between two of these.
#[derive(Default)]
struct Counters {
    /// User, system, idle and nice ticks of all the cores together.
    cpu: [u32; 4],
    /// Per disk driver, by its registry id: bytes read and written.
    disks: HashMap<u64, [u64; 2]>,
    /// Per network interface: bytes in and out. 32-bit in the kernel, so
    /// they wrap.
    links: HashMap<String, [u64; 2]>,
    /// Per process we may read: its CPU time so far, in nanoseconds.
    procs: HashMap<i32, u64>,
}

impl Counters {
    fn read() -> Counters {
        Counters {
            cpu: cpu_ticks(),
            disks: disk_bytes(),
            links: link_bytes(),
            procs: process_times(),
        }
    }
}

/// The share of the ticks between two reads that the cores were busy, in
/// percent of the whole Mac. The tick counters are 32-bit and wrap.
pub fn cpu_percent(prev: [u32; 4], now: [u32; 4]) -> f32 {
    let d: Vec<u64> = (0..4)
        .map(|i| u64::from(now[i].wrapping_sub(prev[i])))
        .collect();
    let busy = d[0] + d[1] + d[3];
    let total = busy + d[2];
    if total == 0 {
        return 0.0;
    }
    busy as f32 / total as f32 * 100.0
}

/// How much keyed pairs of counters grew from `prev` to `now`, summed. A
/// key new in `now` has nothing to count from and is left out (a disk
/// just plugged in is not a burst). A counter that went back wrapped at
/// `wraps_at`; with 0 it was reset, and what it holds is the growth.
pub fn grown<K: Eq + std::hash::Hash>(
    prev: &HashMap<K, [u64; 2]>,
    now: &HashMap<K, [u64; 2]>,
    wraps_at: u64,
) -> [u64; 2] {
    let mut sum = [0u64; 2];
    for (key, n) in now {
        let Some(p) = prev.get(key) else {
            continue;
        };
        for i in 0..2 {
            sum[i] += if n[i] >= p[i] {
                n[i] - p[i]
            } else {
                wraps_at.saturating_sub(p[i]) + n[i]
            };
        }
    }
    sum
}

/// The `n` processes whose CPU time grew most between two reads, with how
/// much (nanoseconds), the most first; a tie goes to the lower pid, so the
/// order holds still. A process new since `prev` has nothing to count
/// from and is left out.
pub fn busiest(prev: &HashMap<i32, u64>, now: &HashMap<i32, u64>, n: usize) -> Vec<(i32, u64)> {
    let mut grew: Vec<(i32, u64)> = now
        .iter()
        .filter_map(|(pid, t)| Some((*pid, t.saturating_sub(*prev.get(pid)?))))
        .collect();
    grew.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    grew.truncate(n);
    grew
}

/// A size as Activity Monitor gives one, in powers of 1024: `3.4 GB`,
/// `212 MB`, `80 KB`. One decimal under ten, none from ten on, and none
/// when it rounds whole.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1023.5 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let tenths = (value * 10.0).round() / 10.0;
    if unit == 0 || value >= 9.95 || tenths.fract() == 0.0 {
        format!("{} {}", value.round(), UNITS[unit])
    } else {
        format!("{tenths:.1} {}", UNITS[unit])
    }
}

/// Bytes a second: `1.2 MB/s`.
pub fn rate(bytes_per_second: u64) -> String {
    format!("{}/s", size(bytes_per_second))
}

/// Memory used of all of it, in whole gigabytes (of 1024^3): `22 / 36 GB`.
pub fn memory(used: u64, total: u64) -> String {
    const GB: f64 = (1u64 << 30) as f64;
    format!(
        "{} / {} GB",
        (used as f64 / GB).round(),
        (total as f64 / GB).round()
    )
}

/// A percentage, whole: `212%`.
pub fn percent(value: f32) -> String {
    format!("{}%", value.max(0.0).round() as u64)
}

/// Bytes a second from bytes grown over `seconds`.
fn per_second(bytes: u64, seconds: f64) -> u64 {
    if seconds <= 0.0 {
        return 0;
    }
    (bytes as f64 / seconds).round() as u64
}

/// The read the panel shows, from two counter reads `seconds` apart and
/// memory as it is now. `history` is the CPU's, kept by the caller, and
/// takes this read's.
fn sample(
    prev: &Counters,
    now: &Counters,
    seconds: f64,
    history: &mut Vec<f32>,
    top: Vec<Busy>,
) -> Sample {
    let cpu = cpu_percent(prev.cpu, now.cpu);
    history.push(cpu / 100.0);
    if history.len() > HISTORY {
        history.remove(0);
    }
    let [read, written] = grown(&prev.disks, &now.disks, 0);
    let [down, up] = grown(&prev.links, &now.links, 1 << 32);
    let (used, total, pressure) = memory_now();
    Sample {
        cpu: percent(cpu),
        cpu_hot: cpu > HOT,
        history: history.clone(),
        memory: memory(used, total),
        memory_used: if total > 0 {
            (used as f64 / total as f64) as f32
        } else {
            0.0
        },
        pressure,
        disk: [read, written].map(|b| rate(per_second(b, seconds))),
        net: [down, up].map(|b| rate(per_second(b, seconds))),
        top,
    }
}

/// The busiest processes' rows: names and memory are read for those few
/// alone. One gone since, or not ours to read, is left out.
fn top_rows(prev: &Counters, now: &Counters, seconds: f64) -> Vec<Busy> {
    let span = seconds * 1e9;
    busiest(&prev.procs, &now.procs, TOP)
        .into_iter()
        .filter_map(|(pid, grew)| {
            let name = process_name(pid)?;
            let memory = footprint(pid)?;
            let cpu = if span > 0.0 {
                grew as f64 / span * 100.0
            } else {
                0.0
            };
            Some(Busy {
                name,
                cpu: percent(cpu as f32),
                memory: size(memory),
            })
        })
        .collect()
}

/// The worker: a read to count from, then a sample every [`EVERY`] until
/// the panel lets it go (its sender dropped).
fn run(stop: mpsc::Receiver<()>, tx: mpsc::Sender<Sample>, wake: Wake) {
    let mut last = Counters::read();
    let mut at = Instant::now();
    let mut history = Vec::with_capacity(HISTORY + 1);
    let mut wait = FIRST;
    loop {
        match stop.recv_timeout(wait) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            _ => return,
        }
        wait = EVERY;
        let now = Counters::read();
        let seconds = at.elapsed().as_secs_f64();
        at = Instant::now();
        let top = top_rows(&last, &now, seconds);
        let sample = sample(&last, &now, seconds, &mut history, top);
        last = now;
        if tx.send(sample).is_err() {
            return;
        }
        wake();
    }
}

/// The panel's reader: running while the panel shows unfolded, and what
/// it read last.
#[derive(Default)]
pub struct Monitor {
    /// Drawn unfolded in the last frame. The window starts the worker for
    /// it after the frame, and stops it after one without.
    pub shown: bool,
    worker: Option<Worker>,
    /// The latest read; none until the first lands, and none again once
    /// the worker stops, so a panel opened later never shows old numbers.
    pub sample: Option<Sample>,
}

struct Worker {
    /// Dropped to stop the worker, which waits on it between reads.
    _stop: mpsc::Sender<()>,
    rx: mpsc::Receiver<Sample>,
}

impl Monitor {
    pub fn running(&self) -> bool {
        self.worker.is_some()
    }

    /// Starts reading; `wake` brings each read to the main thread.
    pub fn start(&mut self, wake: Wake) {
        let (stop, stop_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("crc-activity".into())
            .spawn(move || run(stop_rx, tx, wake));
        if spawned.is_ok() {
            self.worker = Some(Worker { _stop: stop, rx });
        }
    }

    /// Stops reading, and forgets the last read.
    pub fn stop(&mut self) {
        self.worker = None;
        self.sample = None;
    }

    /// Takes what the worker read: whether a new read landed.
    pub fn poll(&mut self) -> bool {
        let Some(worker) = &self.worker else {
            return false;
        };
        let mut landed = false;
        while let Ok(sample) = worker.rx.try_recv() {
            self.sample = Some(sample);
            landed = true;
        }
        landed
    }

    /// For the self-test: folded, whether the worker runs, whether a read
    /// has landed, and how many processes it lists.
    pub fn report(&self, folded: bool) -> String {
        let yes = |b: bool| if b { "yes" } else { "no" };
        format!(
            "folded={} running={} sampled={} rows={}",
            yes(folded),
            yes(self.running()),
            yes(self.sample.is_some()),
            self.sample.as_ref().map_or(0, |s| s.top.len())
        )
    }

    /// The panel's height: its title row alone when folded.
    pub fn height(folded: bool) -> f32 {
        if folded {
            TITLE
        } else {
            TITLE + 2.0 + CARD + 6.0 + HEADING + TOP as f32 * LINE + FOOT
        }
    }

    /// Draws the panel into `rect`, as tall as [`Monitor::height`]. Its
    /// title row, which folds it, is returned for the hit.
    pub fn draw(
        &self,
        folded: bool,
        atlas: &mut Atlas,
        rect: Viewport,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) -> Viewport {
        layout::push_rect(
            out,
            atlas,
            [rect.x, rect.y],
            [rect.width, rect.height],
            theme.sidebar_background,
        );
        layout::push_rect(
            out,
            atlas,
            [rect.x, rect.y],
            [rect.width, 1.0],
            theme.hairline,
        );
        let title = Viewport {
            x: rect.x + 4.0,
            y: rect.y + 1.0,
            width: (rect.width - 8.0).max(0.0),
            height: TITLE - 1.0,
        };
        layout::push_row_hover(out, title, theme);
        layout::push_section_heading(
            out,
            atlas,
            title,
            "Activity",
            None,
            !folded,
            theme.status_text,
            theme,
        );
        if folded {
            return title;
        }
        // The totals on a card, as the inspector groups its facts; the
        // processes under it, on the card's lines.
        let card = Viewport {
            x: rect.x + 8.0,
            y: rect.y + TITLE + 2.0,
            width: (rect.width - 16.0).max(0.0),
            height: CARD,
        };
        layout::push_card(out, card, theme);
        let x = card.x + PAD_X;
        let width = (card.width - 2.0 * PAD_X).max(0.0);
        let at = |y: f32, height: f32| Viewport {
            x,
            y,
            width,
            height,
        };
        let cpu = at(card.y + PAD_Y, LINE);
        let chart = at(cpu.y + LINE + 2.0, CHART);
        let mem = at(chart.y + CHART + 6.0, LINE);
        let meter = at(mem.y + LINE + 2.0, METER);
        let disk = at(meter.y + METER + 6.0, LINE);
        let net = at(disk.y + LINE, LINE);
        let heading = at(card.y + CARD + 6.0, HEADING);
        let process = |n: usize| at(heading.y + HEADING + n as f32 * LINE, LINE);
        const SMALL: f32 = 0.85;
        const PROCESSES: &str = "TOP PROCESSES";
        let heading_w = layout::ui_text_width(atlas, PROCESSES) * SMALL;
        layout::push_ui_text_scaled(
            out,
            atlas,
            Viewport {
                width: heading_w,
                ..heading
            },
            PROCESSES,
            theme.gutter_text,
            SMALL,
        );
        let Some(sample) = &self.sample else {
            // The shape of what is coming: the chart's empty well and the
            // meter's track, a bar for each line's figures.
            for (row, w) in [(cpu, 0.3), (mem, 0.45), (disk, 0.8), (net, 0.8)] {
                layout::push_skeleton_row(out, row, 0.0, (w, 0.0), false, theme);
            }
            layout::push_sparkline(
                out,
                atlas,
                chart,
                &[],
                HISTORY,
                theme.accent,
                (HOT / 100.0, theme.diff_removed),
                theme,
            );
            layout::push_meter(out, meter, 0.0, theme.accent, theme);
            for (n, w) in [0.9, 0.75, 0.82].into_iter().take(TOP).enumerate() {
                layout::push_skeleton_row(out, process(n), 0.0, (w, 0.0), false, theme);
            }
            return title;
        };
        layout::push_ui_text(out, atlas, cpu, "CPU", theme.status_text);
        let cpu_colour = if sample.cpu_hot {
            theme.diff_removed
        } else {
            theme.text
        };
        layout::push_ui_text_right(out, atlas, cpu, &sample.cpu, cpu_colour);
        layout::push_sparkline(
            out,
            atlas,
            chart,
            &sample.history,
            HISTORY,
            theme.accent,
            (HOT / 100.0, theme.diff_removed),
            theme,
        );
        let pressed = match sample.pressure {
            Pressure::Ok => theme.accent,
            Pressure::Warn => theme.diff_modified,
            Pressure::Critical => theme.diff_removed,
        };
        layout::push_ui_text(out, atlas, mem, "Memory", theme.status_text);
        layout::push_ui_text_right(out, atlas, mem, &sample.memory, theme.text);
        if let Some(word) = sample.pressure.label() {
            let tag_x = mem.x + mem.width
                - layout::ui_text_width(atlas, &sample.memory)
                - layout::UI_GAP
                - layout::tag_width(atlas, word);
            layout::push_tag(out, atlas, tag_x, mem, word, pressed);
        }
        layout::push_meter(out, meter, sample.memory_used, pressed, theme);
        // In and out in two columns as wide as a busy rate, figures to
        // their right edge so both lines' units stand one above the other;
        // each arrow just before its figure.
        let arrow_w = layout::ui_text_width(atlas, "\u{2193}");
        let column = ["999 KB/s", "1023 B/s", "99.9 MB/s"]
            .into_iter()
            .map(|widest| layout::ui_text_width(atlas, widest))
            .fold(0.0, f32::max);
        for (row, label, [down, up]) in
            [(disk, "Disk", &sample.disk), (net, "Network", &sample.net)]
        {
            layout::push_ui_text(out, atlas, row, label, theme.status_text);
            let right = row.x + row.width;
            for (n, (arrow, value)) in [("\u{2193}", down), ("\u{2191}", up)]
                .into_iter()
                .enumerate()
            {
                let end = right - (1 - n) as f32 * (column + PAD_X + arrow_w + 3.0);
                let value_w = layout::ui_text_width(atlas, value);
                let arrow_at = Viewport {
                    x: end - value_w - 3.0 - arrow_w,
                    width: arrow_w,
                    ..row
                };
                layout::push_ui_text(out, atlas, arrow_at, arrow, theme.gutter_text);
                layout::push_ui_text_right(
                    out,
                    atlas,
                    Viewport {
                        width: end - row.x,
                        ..row
                    },
                    value,
                    theme.text,
                );
            }
        }
        // Name, then CPU and memory in columns as wide as their widest
        // plausible figure, so they hold still from one read to the next.
        let cpu_w = layout::ui_text_width(atlas, "999%");
        let memory_w =
            layout::ui_text_width(atlas, "1023 MB").max(layout::ui_text_width(atlas, "99.9 GB"));
        for (n, busy) in sample.top.iter().enumerate() {
            let row = process(n);
            let name_w = (row.width - cpu_w - memory_w - 2.0 * PAD_X).max(0.0);
            let name = crate::platform::inspector::fit(atlas, &busy.name, name_w);
            layout::push_ui_text(
                out,
                atlas,
                Viewport {
                    width: name_w,
                    ..row
                },
                &name,
                theme.text,
            );
            layout::push_ui_text_right(
                out,
                atlas,
                Viewport {
                    x: row.x + row.width - memory_w - PAD_X - cpu_w,
                    width: cpu_w,
                    ..row
                },
                &busy.cpu,
                theme.status_text,
            );
            layout::push_ui_text_right(
                out,
                atlas,
                Viewport {
                    x: row.x + row.width - memory_w,
                    width: memory_w,
                    ..row
                },
                &busy.memory,
                theme.status_text,
            );
        }
        title
    }
}

// What follows reads the kernel's counters. Every call is one Activity
// Monitor's numbers come from; a read that fails gives nothing, not an
// error, and the panel shows what it has.

type KernReturn = i32;
type MachPort = u32;
type CFTypeRef = *const c_void;

/// `struct mach_timebase_info`: Mach time units to nanoseconds.
#[repr(C)]
#[derive(Default)]
struct Timebase {
    numer: u32,
    denom: u32,
}

/// The head of `struct ifaddrs`, as far as the link data.
#[repr(C)]
struct IfAddrs {
    next: *mut IfAddrs,
    name: *const c_char,
    flags: u32,
    addr: *const SockAddr,
    netmask: *const c_void,
    dstaddr: *const c_void,
    data: *const c_void,
}

/// The head of `struct sockaddr`.
#[repr(C)]
struct SockAddr {
    len: u8,
    family: u8,
}

unsafe extern "C" {
    fn mach_host_self() -> MachPort;
    fn host_statistics(host: MachPort, flavor: i32, info: *mut i32, count: *mut u32) -> KernReturn;
    fn host_statistics64(
        host: MachPort,
        flavor: i32,
        info: *mut i32,
        count: *mut u32,
    ) -> KernReturn;
    fn host_page_size(host: MachPort, size: *mut usize) -> KernReturn;
    fn mach_timebase_info(info: *mut Timebase) -> KernReturn;
    fn sysctlbyname(
        name: *const c_char,
        old: *mut c_void,
        old_len: *mut usize,
        new: *mut c_void,
        new_len: usize,
    ) -> i32;
    fn getifaddrs(list: *mut *mut IfAddrs) -> i32;
    fn freeifaddrs(list: *mut IfAddrs);
    fn proc_listallpids(buffer: *mut c_void, size: i32) -> i32;
    fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut u8, size: i32) -> i32;
    fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut c_void) -> i32;
    fn proc_name(pid: i32, buffer: *mut c_void, size: u32) -> i32;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingServices(
        main: MachPort,
        matching: *mut c_void,
        iterator: *mut u32,
    ) -> KernReturn;
    fn IOIteratorNext(iterator: u32) -> u32;
    fn IOObjectRelease(object: u32) -> KernReturn;
    fn IORegistryEntryGetRegistryEntryID(entry: u32, id: *mut u64) -> KernReturn;
    fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: CFTypeRef,
        allocator: CFTypeRef,
        options: u32,
    ) -> CFTypeRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        allocator: CFTypeRef,
        text: *const c_char,
        encoding: u32,
    ) -> CFTypeRef;
    fn CFGetTypeID(value: CFTypeRef) -> usize;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFDictionaryGetValue(dictionary: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFNumberGetValue(number: CFTypeRef, kind: isize, out: *mut c_void) -> u8;
    fn CFRelease(value: CFTypeRef);
}

/// This Mac's host port, taken once: each `mach_host_self` adds a
/// reference to it.
fn host() -> MachPort {
    static HOST: std::sync::OnceLock<MachPort> = std::sync::OnceLock::new();
    // SAFETY: takes no arguments and returns a port name.
    *HOST.get_or_init(|| unsafe { mach_host_self() })
}

/// The CPU ticks of every core together (`HOST_CPU_LOAD_INFO`).
fn cpu_ticks() -> [u32; 4] {
    const HOST_CPU_LOAD_INFO: i32 = 3;
    let mut ticks = [0u32; 4];
    let mut count = 4u32;
    // SAFETY: `count` says the buffer holds four naturals, and the kernel
    // writes no more than that.
    let ok = unsafe {
        host_statistics(
            host(),
            HOST_CPU_LOAD_INFO,
            ticks.as_mut_ptr().cast(),
            &mut count,
        )
    } == 0;
    if ok { ticks } else { [0; 4] }
}

/// A plain value from `sysctlbyname`, when it has the size of `T`.
fn sysctl_value<T: Copy + Default>(name: &CStr) -> Option<T> {
    let mut value = T::default();
    let mut len = std::mem::size_of::<T>();
    // SAFETY: `len` is the size of `value`, and the kernel writes no more
    // than that and says how much it wrote.
    let ok = unsafe {
        sysctlbyname(
            name.as_ptr(),
            (&mut value as *mut T).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    (ok && len == std::mem::size_of::<T>()).then_some(value)
}

/// Memory in use as Activity Monitor counts it (app memory, wired and
/// compressed), all of it, and the pressure.
fn memory_now() -> (u64, u64, Pressure) {
    let total = sysctl_value::<u64>(c"hw.memsize").unwrap_or(0);
    let pressure =
        Pressure::of(sysctl_value::<i32>(c"kern.memorystatus_vm_pressure_level").unwrap_or(1));
    (used_memory().unwrap_or(0), total, pressure)
}

/// `struct vm_statistics64` as naturals: 38 of them, 8-aligned.
#[repr(C, align(8))]
struct VmStatistics([u32; 38]);

fn used_memory() -> Option<u64> {
    const HOST_VM_INFO64: i32 = 4;
    // The naturals at which the counts used here sit.
    const WIRE: usize = 3;
    const PURGEABLE: usize = 22;
    const COMPRESSOR: usize = 32;
    const INTERNAL: usize = 35;
    let mut stats = VmStatistics([0; 38]);
    let mut count = 38u32;
    // SAFETY: `count` is the buffer's size in naturals; the kernel writes
    // no more than that.
    let ok = unsafe {
        host_statistics64(
            host(),
            HOST_VM_INFO64,
            stats.0.as_mut_ptr().cast(),
            &mut count,
        )
    } == 0;
    let mut page = 0usize;
    // SAFETY: writes one vm_size_t.
    let paged = unsafe { host_page_size(host(), &mut page) } == 0;
    if !ok || !paged {
        return None;
    }
    let s = &stats.0;
    let app = u64::from(s[INTERNAL].saturating_sub(s[PURGEABLE]));
    let pages = app + u64::from(s[WIRE]) + u64::from(s[COMPRESSOR]);
    Some(pages * page as u64)
}

/// Bytes read and written by each disk, from IOKit's block storage
/// drivers' statistics, by registry id.
fn disk_bytes() -> HashMap<u64, [u64; 2]> {
    const UTF8: u32 = 0x0800_0100;
    const SINT64: isize = 4;
    let mut out = HashMap::new();
    // SAFETY: each CF string is made here and released below; null is the
    // default allocator.
    let cf =
        |text: &CStr| unsafe { CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), UTF8) };
    let keys = [cf(c"Statistics"), cf(c"Bytes (Read)"), cf(c"Bytes (Write)")];
    if keys.iter().any(|k| k.is_null()) {
        for key in keys.into_iter().filter(|k| !k.is_null()) {
            // SAFETY: made above, released once.
            unsafe { CFRelease(key) };
        }
        return out;
    }
    let mut iterator = 0u32;
    // SAFETY: the matching dictionary is consumed by the call; the
    // iterator is released below. Port 0 is the default main port.
    let found = unsafe {
        let matching = IOServiceMatching(c"IOBlockStorageDriver".as_ptr());
        !matching.is_null() && IOServiceGetMatchingServices(0, matching, &mut iterator) == 0
    };
    if found {
        loop {
            // SAFETY: an iterator IOKit gave; 0 ends it.
            let entry = unsafe { IOIteratorNext(iterator) };
            if entry == 0 {
                break;
            }
            let mut id = 0u64;
            // SAFETY: `entry` is a live registry entry, released at the
            // end of the turn; the property is a copy we release; a value
            // is read only from a dictionary, and as a number only when it
            // is one.
            unsafe {
                let stats = IORegistryEntryCreateCFProperty(entry, keys[0], std::ptr::null(), 0);
                if !stats.is_null() {
                    if CFGetTypeID(stats) == CFDictionaryGetTypeID()
                        && IORegistryEntryGetRegistryEntryID(entry, &mut id) == 0
                    {
                        let number = |key: CFTypeRef| {
                            let value = CFDictionaryGetValue(stats, key);
                            let mut n = 0i64;
                            if !value.is_null()
                                && CFGetTypeID(value) == CFNumberGetTypeID()
                                && CFNumberGetValue(value, SINT64, (&mut n as *mut i64).cast()) != 0
                            {
                                n.max(0) as u64
                            } else {
                                0
                            }
                        };
                        out.insert(id, [number(keys[1]), number(keys[2])]);
                    }
                    CFRelease(stats);
                }
                IOObjectRelease(entry);
            }
        }
        // SAFETY: the iterator IOKit gave, released once.
        unsafe { IOObjectRelease(iterator) };
    }
    for key in keys {
        // SAFETY: made above, released once.
        unsafe { CFRelease(key) };
    }
    out
}

/// Bytes in and out of each network interface but the loopback, from the
/// link-level entries `getifaddrs` lists.
fn link_bytes() -> HashMap<String, [u64; 2]> {
    const AF_LINK: u8 = 18;
    const IFF_LOOPBACK: u32 = 0x8;
    // `struct if_data`: eight bytes of chars, then 32-bit counters, the
    // bytes in and out ninth and tenth of them.
    const IBYTES: usize = 40;
    const OBYTES: usize = 44;
    let mut out = HashMap::new();
    let mut list: *mut IfAddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, freed below.
    if unsafe { getifaddrs(&mut list) } != 0 {
        return out;
    }
    let mut at = list;
    while !at.is_null() {
        // SAFETY: a node of the list getifaddrs made, alive until it is
        // freed; an AF_LINK entry's data is its `if_data`.
        unsafe {
            let entry = &*at;
            at = entry.next;
            if entry.addr.is_null()
                || (*entry.addr).family != AF_LINK
                || entry.flags & IFF_LOOPBACK != 0
                || entry.data.is_null()
                || entry.name.is_null()
            {
                continue;
            }
            let data = entry.data.cast::<u8>();
            let ibytes = data.add(IBYTES).cast::<u32>().read_unaligned();
            let obytes = data.add(OBYTES).cast::<u32>().read_unaligned();
            let name = CStr::from_ptr(entry.name).to_string_lossy().into_owned();
            out.insert(name, [u64::from(ibytes), u64::from(obytes)]);
        }
    }
    // SAFETY: the list getifaddrs made, freed once.
    unsafe { freeifaddrs(list) };
    out
}

/// Mach time units to nanoseconds: 1 on Intel, 125/3 on Apple silicon.
fn timebase() -> (u64, u64) {
    static TIMEBASE: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
    *TIMEBASE.get_or_init(|| {
        let mut info = Timebase::default();
        // SAFETY: writes one struct mach_timebase_info.
        let ok = unsafe { mach_timebase_info(&mut info) } == 0;
        if ok && info.denom != 0 {
            (u64::from(info.numer), u64::from(info.denom))
        } else {
            (1, 1)
        }
    })
}

/// Every process's CPU time so far, in nanoseconds: those we may read
/// (another user's, or root's, are left out quietly).
fn process_times() -> HashMap<i32, u64> {
    const PROC_PIDTASKINFO: i32 = 4;
    // `struct proc_taskinfo`: six 64-bit fields, then twelve 32-bit ones;
    // total user and system time third and fourth.
    const TASKINFO: usize = 96;
    let (numer, denom) = timebase();
    // SAFETY: with no buffer it says how many pids there are.
    let count = unsafe { proc_listallpids(std::ptr::null_mut(), 0) }.max(0) as usize;
    let mut pids = vec![0i32; count + 64];
    // SAFETY: the size passed is the buffer's in bytes; the call returns
    // how many pids it wrote.
    let written = unsafe {
        proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<i32>()) as i32,
        )
    };
    pids.truncate(written.max(0) as usize);
    let mut out = HashMap::with_capacity(pids.len());
    let mut info = [0u64; TASKINFO / 8];
    for pid in pids.into_iter().filter(|p| *p > 0) {
        // SAFETY: the buffer is as large as the size passed, and the
        // kernel writes no more than that.
        let n = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDTASKINFO,
                0,
                info.as_mut_ptr().cast(),
                TASKINFO as i32,
            )
        };
        if n as usize != TASKINFO {
            continue;
        }
        let ticks = info[2].saturating_add(info[3]);
        out.insert(
            pid,
            (u128::from(ticks) * u128::from(numer) / u128::from(denom)) as u64,
        );
    }
    out
}

/// A process's name: the last part of the program it was started as,
/// which names a tool installed under a version (`claude`, not
/// `2.1.314`), a login shell's without its dash; the kernel's short name
/// for one whose arguments we may not read.
fn process_name(pid: i32) -> Option<String> {
    let program = crate::platform::process_args(pid)
        .and_then(|args| args.into_iter().next())
        .and_then(|first| {
            std::path::Path::new(&first)
                .file_name()
                .map(|n| n.to_string_lossy().trim_start_matches('-').to_owned())
        })
        .filter(|name| !name.is_empty());
    program.or_else(|| kernel_name(pid))
}

/// A process's name as the kernel keeps it, as `ps -c` gives it.
fn kernel_name(pid: i32) -> Option<String> {
    let mut name = [0u8; 256];
    // SAFETY: the size passed is the buffer's; it returns the length.
    let n = unsafe { proc_name(pid, name.as_mut_ptr().cast(), name.len() as u32) };
    if n <= 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&name[..n as usize]).into_owned())
}

/// A process's memory as Activity Monitor's Memory column has it: its
/// physical footprint (`RUSAGE_INFO_V2`).
fn footprint(pid: i32) -> Option<u64> {
    const RUSAGE_INFO_V2: i32 = 2;
    // `struct rusage_info_v2`: a 16-byte uuid then seventeen 64-bit
    // counters, the footprint the eighth.
    const FOOTPRINT: usize = 9;
    let mut info = [0u64; 19];
    // SAFETY: the buffer is a whole rusage_info_v2 for the kernel to fill.
    let ok = unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V2, info.as_mut_ptr().cast()) } == 0;
    ok.then_some(info[FOOTPRINT])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_is_the_busy_share_of_the_ticks_between_two_reads() {
        // user, system, idle, nice
        assert_eq!(cpu_percent([10, 10, 80, 0], [40, 20, 130, 10]), 50.0);
        assert_eq!(cpu_percent([5, 5, 5, 5], [5, 5, 5, 5]), 0.0);
        // A counter that wrapped still counts what it grew.
        let near = u32::MAX - 9;
        assert_eq!(cpu_percent([near, 0, 0, 0], [10, 0, 20, 0]), 50.0);
    }

    #[test]
    fn counters_grow_by_key_and_survive_a_wrap_or_a_reset() {
        let prev: HashMap<&str, [u64; 2]> =
            [("en0", [100, 50]), ("en1", [u64::from(u32::MAX) - 9, 0])].into();
        let now: HashMap<&str, [u64; 2]> = [
            ("en0", [300, 70]),
            ("en1", [10, 5]),
            // New since: nothing to count from.
            ("utun0", [1 << 30, 1 << 30]),
        ]
        .into();
        assert_eq!(grown(&prev, &now, 1 << 32), [200 + 20, 20 + 5]);
        // A disk's counter that went back was reset: what it holds grew.
        let prev: HashMap<u64, [u64; 2]> = [(1, [5000, 9000])].into();
        let now: HashMap<u64, [u64; 2]> = [(1, [700, 9100])].into();
        assert_eq!(grown(&prev, &now, 0), [700, 100]);
    }

    #[test]
    fn the_busiest_are_those_that_grew_most_and_new_ones_wait() {
        let prev: HashMap<i32, u64> = [(1, 100), (2, 100), (3, 100), (4, 100), (5, 0)].into();
        let now: HashMap<i32, u64> =
            [(1, 150), (2, 900), (3, 400), (4, 400), (5, 10), (6, 9_999)].into();
        assert_eq!(busiest(&prev, &now, 3), vec![(2, 800), (3, 300), (4, 300)]);
        assert_eq!(busiest(&prev, &now, 10).len(), 5);
        assert!(busiest(&HashMap::new(), &now, 3).is_empty());
    }

    #[test]
    fn sizes_and_rates_read_as_activity_monitors() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(80 * 1024), "80 KB");
        assert_eq!(size(1024 * 1024 * 6 / 5), "1.2 MB");
        assert_eq!(size(12 * 1024 * 1024 + 300_000), "12 MB");
        assert_eq!(size(3 * 1024 * 1024), "3 MB");
        assert_eq!(size(34 * (1u64 << 30) / 10), "3.4 GB");
        // Just under a unit rounds up into it, not to `1024 KB`.
        assert_eq!(size(1024 * 1024 - 100), "1 MB");
        assert_eq!(rate(1024 * 1024 * 6 / 5), "1.2 MB/s");
        assert_eq!(rate(80 * 1024), "80 KB/s");
        assert_eq!(per_second(4000, 2.0), 2000);
        assert_eq!(per_second(4000, 0.0), 0);
    }

    #[test]
    fn memory_and_percentages_are_whole() {
        let gb = 1u64 << 30;
        assert_eq!(memory(22 * gb + gb / 3, 36 * gb), "22 / 36 GB");
        assert_eq!(percent(212.4), "212%");
        assert_eq!(percent(40.6), "41%");
        assert_eq!(percent(-1.0), "0%");
    }

    #[test]
    fn pressure_follows_the_kernels_levels() {
        assert_eq!(Pressure::of(1), Pressure::Ok);
        assert_eq!(Pressure::of(2), Pressure::Warn);
        assert_eq!(Pressure::of(4), Pressure::Critical);
        assert_eq!(Pressure::of(0).label(), None);
        assert_eq!(Pressure::of(4).label(), Some("Critical"));
    }

    #[test]
    fn a_sample_keeps_a_minute_of_cpu_and_flags_a_hot_one() {
        let prev = Counters {
            cpu: [0, 0, 100, 0],
            ..Counters::default()
        };
        let now = Counters {
            cpu: [90, 0, 110, 0],
            ..Counters::default()
        };
        let mut history = vec![0.1; HISTORY];
        let s = sample(&prev, &now, 2.0, &mut history, Vec::new());
        assert_eq!(s.cpu, "90%");
        assert!(s.cpu_hot);
        assert_eq!(s.history.len(), HISTORY);
        assert_eq!(s.history.last(), Some(&0.9));
        assert_eq!(s.disk, ["0 B/s", "0 B/s"]);
    }

    #[test]
    fn this_mac_reads_its_own_counters() {
        // Not values, which change: that each source answers at all.
        let (used, total, _) = memory_now();
        assert!(total > 0 && used > 0 && used <= total, "{used} of {total}");
        assert!(cpu_ticks().iter().any(|t| *t > 0));
        let me = std::process::id() as i32;
        assert!(process_times().contains_key(&me));
        assert!(process_name(me).is_some());
        assert!(footprint(me).is_some_and(|f| f > 0));
        // A Mac without a disk driver or a network interface is not one
        // these run on, but neither read may fail loudly.
        let _ = disk_bytes();
        let _ = link_bytes();
    }

    #[test]
    fn the_worker_stops_when_the_panel_lets_it_go() {
        let mut monitor = Monitor::default();
        monitor.start(Box::new(|| {}));
        assert!(monitor.running());
        assert!(!monitor.poll());
        monitor.stop();
        assert!(!monitor.running() && monitor.sample.is_none());
        assert_eq!(
            monitor.report(true),
            "folded=yes running=no sampled=no rows=0"
        );
    }
}
