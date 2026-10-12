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
pub mod review_page;
pub mod search;
pub mod selftest;
pub mod session;
pub mod settings;
pub mod settings_page;
pub mod symbols;
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
