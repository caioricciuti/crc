//! Platform layer: the AppKit window, input, clipboard, files, session, and
//! timing.

pub mod agents;
pub mod claude;
pub mod clipboard;
pub mod commands;
pub mod conflicts;
pub mod dispatch;
pub mod extensions;
pub mod git_panel;
pub mod hang;
pub mod inspector;
pub mod latency;
pub mod mcp_page;
pub mod mcp_panel;
pub mod recovery;
pub mod remote;
pub mod report;
pub mod retry;
pub mod search;
pub mod selftest;
pub mod session;
pub mod settings;
pub mod settings_page;
pub mod switcher;
pub mod symbols;
pub mod table;
pub mod terminal;
pub mod update;
pub mod webview;
pub mod window;

/// crc's folder in Application Support: kept, and backed up, unlike
/// Caches, and not something a person browses, unlike Documents.
pub fn app_support() -> Option<std::path::PathBuf> {
    Some(home()?.join("Library/Application Support/crc"))
}

/// `~/Library/Logs/crc`, where Console.app looks for an app's logs.
pub fn logs() -> Option<std::path::PathBuf> {
    Some(home()?.join("Library/Logs/crc"))
}

/// crc's cache folder: anything in it can be rebuilt, and may be deleted.
pub fn caches() -> Option<std::path::PathBuf> {
    Some(home()?.join("Library/Caches/crc"))
}

/// A path as the file system resolves it (symbolic links followed, `..`
/// gone), or as given when it cannot be resolved (a file not written yet).
/// Open documents keep their paths this way, so comparing one key against
/// them needs no further lookups.
pub fn canonical(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A path from the raw bytes Git, the crash journal and the session store
/// keep. A `Vec` moves in without a copy.
pub fn path_from_bytes(bytes: impl Into<Vec<u8>>) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes.into()).into()
}

/// Seconds since 1970, 0 on a clock set before it.
pub fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// When `time` was on the local clock, as a person places a recent moment:
/// "14:32" today, "Tue 14:32" in the six days before, "12 Oct 14:32"
/// earlier this year and "12 Oct 2025 14:32" before that. 24-hour, in the
/// user's time zone.
pub fn clock_time(time: i64, now: i64) -> String {
    clock_words(time, utc_offset(time), now, utc_offset(now))
}

/// [`clock_time`] with the zone's offsets from UTC given, in seconds: one
/// for `time` and one for `now`, as daylight saving may change between.
pub fn clock_words(time: i64, time_offset: i64, now: i64, now_offset: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let at = Wall::of(time + time_offset);
    let today = Wall::of(now + now_offset);
    let hm = format!("{:02}:{:02}", at.hour, at.minute);
    match today.days - at.days {
        0 => hm,
        1..=6 => format!("{} {hm}", DAYS[at.weekday]),
        _ if at.year == today.year => format!("{} {} {hm}", at.day, MONTHS[at.month - 1]),
        _ => format!("{} {} {} {hm}", at.day, MONTHS[at.month - 1], at.year),
    }
}

/// A moment on a wall clock, from seconds since 1970 in its zone.
struct Wall {
    /// Days since 1 January 1970.
    days: i64,
    year: i64,
    /// 1 to 12.
    month: usize,
    day: i64,
    /// 0 for Sunday.
    weekday: usize,
    hour: i64,
    minute: i64,
}

impl Wall {
    fn of(seconds: i64) -> Wall {
        let days = seconds.div_euclid(86_400);
        let second = seconds.rem_euclid(86_400);
        // The civil date from a day count (Howard Hinnant's algorithm): in
        // 400-year eras of years that start in March.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let of_era = z - era * 146_097;
        let year_of_era = (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
        let of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let m = (5 * of_year + 2) / 153;
        let month = if m < 10 { m + 3 } else { m - 9 };
        Wall {
            days,
            year: year_of_era + era * 400 + i64::from(month <= 2),
            month: month as usize,
            day: of_year - (153 * m + 2) / 5 + 1,
            // 1 January 1970 was a Thursday.
            weekday: (days + 4).rem_euclid(7) as usize,
            hour: second / 3600,
            minute: second % 3600 / 60,
        }
    }
}

/// `struct tm` as macOS lays it out.
#[repr(C)]
struct Tm {
    sec: i32,
    min: i32,
    hour: i32,
    mday: i32,
    mon: i32,
    year: i32,
    wday: i32,
    yday: i32,
    isdst: i32,
    gmtoff: i64,
    zone: *mut std::ffi::c_char,
}

/// The local zone's offset from UTC at `time`, in seconds; 0 when the
/// system cannot say.
pub fn utc_offset(time: i64) -> i64 {
    let mut tm = std::mem::MaybeUninit::<Tm>::zeroed();
    // SAFETY: `tm` is a whole `struct tm` for localtime_r to fill, and
    // `time` lives across the call; it returns null on failure.
    let filled = unsafe { localtime_r(&time, tm.as_mut_ptr()) };
    if filled.is_null() {
        return 0;
    }
    // SAFETY: filled, and every field is a plain integer or pointer.
    unsafe { tm.assume_init() }.gmtoff
}

/// FNV-1a: a stable 64-bit hash for names, no crate for it. build.rs keeps
/// its own copy, as a build script cannot use the crate it builds.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
    fn localtime_r(time: *const i64, out: *mut Tm) -> *mut Tm;
    fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut u8, size: i32) -> i32;
    fn proc_listchildpids(pid: i32, buffer: *mut i32, size: i32) -> i32;
    fn sysctl(
        name: *mut i32,
        count: u32,
        old: *mut u8,
        old_len: *mut usize,
        new: *mut u8,
        new_len: usize,
    ) -> i32;
}

/// The command line of process `pid`, the program first, from the
/// kernel (`KERN_PROCARGS2`). `None` for a process gone or not ours.
pub fn process_args(pid: i32) -> Option<Vec<String>> {
    const CTL_KERN: i32 = 1;
    const KERN_PROCARGS2: i32 = 49;
    let mut name = [CTL_KERN, KERN_PROCARGS2, pid];
    let mut buffer = vec![0u8; 16 * 1024];
    let mut len = buffer.len();
    // SAFETY: `len` is the buffer's size, and the kernel writes no more
    // than that and says how much it wrote.
    let ok = unsafe {
        sysctl(
            name.as_mut_ptr(),
            3,
            buffer.as_mut_ptr(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    if !ok || len < 4 {
        return None;
    }
    // An int for argc, the executable's path, padding, then argc strings.
    let argc = i32::from_ne_bytes(buffer[..4].try_into().ok()?).max(0) as usize;
    let mut rest = buffer[4..len].split(|b| *b == 0);
    rest.next()?;
    let args: Vec<String> = rest
        .filter(|s| !s.is_empty())
        .take(argc)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    (!args.is_empty()).then_some(args)
}

/// The working directory of process `pid`, as the kernel has it: where a
/// shell is after its last `cd`. `None` for a process gone or not ours.
pub fn process_cwd(pid: i32) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    // `struct proc_vnodepathinfo`: the current directory's `vnode_info`
    // (152 bytes) and its path (MAXPATHLEN), then the same for the root.
    const VNODE_INFO: usize = 152;
    const MAX_PATH: usize = 1024;
    const PROC_PIDVNODEPATHINFO: i32 = 9;
    let mut info = vec![0u8; 2 * (VNODE_INFO + MAX_PATH)];
    // SAFETY: the buffer is as large as the size passed, and the kernel
    // writes no more than that.
    let written = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr(),
            info.len() as i32,
        )
    };
    if written as usize != info.len() {
        return None;
    }
    let path = &info[VNODE_INFO..VNODE_INFO + MAX_PATH];
    let end = path.iter().position(|b| *b == 0)?;
    (end > 0).then(|| std::ffi::OsStr::from_bytes(&path[..end]).into())
}

/// The processes `pid` started that are still running.
pub fn child_pids(pid: i32) -> Vec<i32> {
    let mut pids = vec![0i32; 64];
    // SAFETY: the size passed is the buffer's in bytes; the call returns
    // how many pids it wrote.
    let count = unsafe {
        proc_listchildpids(
            pid,
            pids.as_mut_ptr(),
            (pids.len() * std::mem::size_of::<i32>()) as i32,
        )
    };
    pids.truncate(count.max(0) as usize);
    pids.retain(|p| *p > 0);
    pids
}

/// Sends `signal` to `pid`, or to the process group `-pid`. Whether it was
/// delivered.
pub fn send_signal(pid: i32, signal: i32) -> bool {
    // SAFETY: kill(2) takes two integers and touches no memory of ours.
    unsafe { kill(pid, signal) == 0 }
}

/// Starts `command` and reaps it from a thread, so a short-lived helper
/// (`open -R`, `open <url>`) does not stay a zombie until quit.
pub fn spawn_reaped(command: &mut std::process::Command) -> std::io::Result<()> {
    let mut child = command.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Whether a process exists. Signal 0 checks without sending anything;
/// `EPERM` means it exists but belongs to someone else.
pub fn process_alive(pid: u64) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    if send_signal(pid, 0) {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(1)
}

/// Writes `bytes` to a temporary file beside `path` and renames it over, so
/// a crash or a full disk leaves the old file rather than half of the new.
pub fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    write_atomically_with(path, 0o666, |file| file.write_all(bytes))
}

/// [`write_atomically`], with the file created as `mode` (before the umask)
/// and its contents written by `write`.
pub fn write_atomically_with(
    path: &std::path::Path,
    mode: u32,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        write(&mut file)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod clock_tests {
    use super::clock_words;

    /// Friday 16 October 2026, 14:32 at UTC+2.
    const NOW: i64 = 1_792_153_920;
    const CEST: i64 = 7200;

    #[test]
    fn today_is_the_time_alone_and_the_zone_decides_the_day() {
        assert_eq!(clock_words(NOW - 60, CEST, NOW, CEST), "14:31");
        assert_eq!(clock_words(NOW, CEST, NOW, CEST), "14:32");
        // 22:10 UTC on Thursday is ten past midnight on Friday here.
        assert_eq!(clock_words(1_792_102_200, CEST, NOW, CEST), "00:10");
        assert_eq!(clock_words(1_792_102_200, 0, NOW, 0), "Thu 22:10");
    }

    #[test]
    fn the_last_six_days_are_named_and_earlier_ones_dated() {
        assert_eq!(clock_words(1_792_099_800, CEST, NOW, CEST), "Thu 23:30");
        assert_eq!(clock_words(1_791_635_520, CEST, NOW, CEST), "Sat 14:32");
        assert_eq!(clock_words(1_791_549_120, CEST, NOW, CEST), "9 Oct 14:32");
        assert_eq!(
            clock_words(1_760_272_320, CEST, NOW, CEST),
            "12 Oct 2025 14:32"
        );
        assert_eq!(
            clock_words(1_709_193_900, 0, NOW, CEST),
            "29 Feb 2024 08:05"
        );
    }

    #[test]
    fn across_a_new_year_and_a_change_of_offset() {
        // 00:30 UTC on 1 January 2026 is 23:30 on New Year's Eve at UTC-1.
        let eve = 1_767_227_400;
        assert_eq!(clock_words(eve, -3600, eve + 3600, -3600), "Wed 23:30");
        assert_eq!(clock_words(eve, -3600, NOW, CEST), "31 Dec 2025 23:30");
        // Saturday 23:30 in winter time, read on Sunday morning in summer
        // time: each moment in its own offset.
        assert_eq!(
            clock_words(1_774_737_000, 3600, 1_774_771_200, CEST),
            "Sat 23:30"
        );
    }

    #[test]
    fn the_local_offset_is_a_zones() {
        let now = super::unix_seconds() as i64;
        let offset = super::utc_offset(now);
        assert!(offset.abs() <= 14 * 3600 && offset % 900 == 0, "{offset}");
        assert_eq!(super::clock_time(now, now).len(), 5);
    }
}

#[cfg(test)]
mod process_tests {
    #[test]
    fn a_childs_working_directory_is_read_from_the_kernel() {
        let dir = std::env::temp_dir().join(format!("crc-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("5")
            .current_dir(&dir)
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        assert!(super::child_pids(std::process::id() as i32).contains(&pid));
        let read = super::process_cwd(pid).map(|p| super::canonical(&p));
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(read, Some(super::canonical(&dir)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
