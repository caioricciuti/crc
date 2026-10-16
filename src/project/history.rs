//! The versions of one file through its history, and their contents.
//!
//! [`file_history`] lists the commits that changed a file, following it
//! across renames, with the blob each one left; a [`BlobReader`] then reads
//! those blobs from one long-lived `git cat-file --batch`, so stepping
//! through a thousand versions costs one process, not a thousand. Call on
//! workers, never on the event/render thread.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};

use super::git::command;
use crate::history::store::{Entry, Reason, SnapshotId};
use crate::platform::path_from_bytes;

/// Commits [`file_history`] reads when the caller has no reason to pick.
pub const HISTORY_LIMIT: usize = 1000;

/// One version of a file: the commit that made it and the blob it left,
/// or a version crc kept of it in local history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub commit: String,
    pub author: String,
    /// Author time, or when a local version was taken, Unix seconds.
    pub time: i64,
    pub summary: String,
    pub blob: String,
    /// Where the file was in that commit, relative to the repository: it
    /// changes across a rename. A local version's is the file's own path.
    pub path: PathBuf,
    pub source: Source,
}

/// Where a version's text comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The blob a commit left, read through Git.
    Commit,
    /// A version kept in local history, read from its store.
    Local { id: SnapshotId, reason: Reason },
}

impl Version {
    /// The local version `entry` of the file at `path`.
    pub fn local(entry: &Entry, path: &Path) -> Version {
        Version {
            commit: String::new(),
            author: String::new(),
            time: entry.at.div_euclid(1000),
            summary: String::new(),
            blob: String::new(),
            path: path.to_path_buf(),
            source: Source::Local {
                id: entry.id,
                reason: entry.reason,
            },
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self.source, Source::Local { .. })
    }
}

/// Commits and local versions of one file in one timeline, oldest first.
/// `commits` come oldest first, as [`file_history`] lists them, and keep
/// their order; `local` in any order, and go by when they were taken, then
/// by id. Commit times are whole seconds: a local version taken in the
/// second of a commit goes before it, as a file is saved before it is
/// committed.
pub fn merge(commits: Vec<Version>, mut local: Vec<Version>) -> Vec<Version> {
    let id = |v: &Version| match v.source {
        Source::Local { id, .. } => id,
        Source::Commit => SnapshotId(0),
    };
    local.sort_by_key(|v| (v.time, id(v)));
    let mut out = Vec::with_capacity(commits.len() + local.len());
    let mut local = local.into_iter().peekable();
    for commit in commits {
        while let Some(next) = local.next_if(|l| l.time <= commit.time) {
            out.push(next);
        }
        out.push(commit);
    }
    out.extend(local);
    out
}

/// Leaves out the local versions whose text is a commit's beside them in
/// `versions` (from [`merge`]): the save before a commit, or the file as
/// opened at the commit checked out. `same(local, commit)` says whether
/// the two hold the same text; it is asked at most `checks` times, newest
/// first, and past that a local version is kept.
pub fn drop_duplicates(
    versions: &mut Vec<Version>,
    mut checks: usize,
    mut same: impl FnMut(&Version, &Version) -> bool,
) {
    let mut drop = vec![false; versions.len()];
    for at in (0..versions.len()).rev() {
        if !versions[at].is_local() {
            continue;
        }
        let neighbours = [at.checked_sub(1), Some(at + 1)];
        for commit in neighbours.into_iter().flatten() {
            let Some(commit) = versions.get(commit).filter(|v| !v.is_local()) else {
                continue;
            };
            if checks == 0 {
                break;
            }
            checks -= 1;
            if same(&versions[at], commit) {
                drop[at] = true;
                break;
            }
        }
    }
    let mut at = 0;
    versions.retain(|_| {
        at += 1;
        !drop[at - 1]
    });
}

/// The object id Git gives a blob of `bytes`, in hex: what a commit's
/// version names, so a text is matched to one without reading it.
pub fn blob_id(bytes: &[u8]) -> Option<String> {
    let mut object = format!("blob {}\0", bytes.len()).into_bytes();
    object.extend_from_slice(bytes);
    let hash = crate::ext::verify::sha1(&object)?;
    Some(hash.iter().map(|b| format!("{b:02x}")).collect())
}

/// The versions of `relative` (a path inside `root`), oldest first, from
/// the newest `limit` commits that changed it, and whether `limit` cut the
/// list short. A deletion, or a version that was a submodule or a symlink,
/// has no text to show and is left out. A repository with no commits yet
/// has no versions.
pub fn file_history(
    root: &Path,
    relative: &Path,
    limit: usize,
) -> Result<(Vec<Version>, bool), String> {
    let max = format!("--max-count={limit}");
    let output = command(root)
        .args([
            "log",
            "--follow",
            "-M",
            "--no-abbrev",
            "--raw",
            "-z",
            &max,
            "--format=%x01%H%x00%an%x00%at%x00%s",
            "--",
        ])
        .arg(relative)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        // No commit yet: nothing to list, not an error.
        let born = command(root)
            .args(["rev-parse", "--verify", "--quiet", "HEAD"])
            .output()
            .is_ok_and(|o| o.status.success());
        if !born {
            return Ok((Vec::new(), false));
        }
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let (versions, commits) = parse(&output.stdout);
    Ok((versions, limit > 0 && commits >= limit))
}

/// The versions in `git log --raw -z` output of the shape
/// [`file_history`] asks for, oldest first.
pub fn parse_file_log(bytes: &[u8]) -> Vec<Version> {
    parse(bytes).0
}

/// The versions, oldest first, and how many commits the log held.
fn parse(bytes: &[u8]) -> (Vec<Version>, usize) {
    let mut fields = bytes.split(|&b| b == 0).peekable();
    let mut versions = Vec::new();
    let mut commits = 0;
    // The commit the records that follow belong to.
    let mut current: Option<(String, String, i64, String)> = None;
    let text = |field: Option<&[u8]>| {
        super::worktree::clean_line(&String::from_utf8_lossy(field.unwrap_or_default()))
    };
    while let Some(field) = fields.next() {
        let field = field.strip_prefix(b"\n").unwrap_or(field);
        if let Some(hash) = field.strip_prefix(b"\x01") {
            commits += 1;
            let commit = String::from_utf8_lossy(hash).into_owned();
            let author = text(fields.next());
            let time = std::str::from_utf8(fields.next().unwrap_or_default())
                .ok()
                .and_then(|t| t.trim().parse().ok())
                .unwrap_or(0);
            let summary = text(fields.next());
            current = Some((commit, author, time, summary));
            continue;
        }
        let Some(record) = field.strip_prefix(b":") else {
            continue;
        };
        let record = String::from_utf8_lossy(record);
        let parts: Vec<&str> = record.split(' ').collect();
        let [_, mode, _, blob, status] = parts[..] else {
            continue;
        };
        // A rename or a copy names the old path, then the new one.
        let two = status.starts_with(['R', 'C']);
        let mut path = fields.next();
        if two {
            path = fields.next();
        }
        let (Some(path), Some((commit, author, time, summary))) = (path, &current) else {
            continue;
        };
        let deleted = status.starts_with('D') || blob.bytes().all(|b| b == b'0');
        // A submodule's commit or a symlink's target is not the file's text.
        let other = matches!(mode, "160000" | "120000");
        if deleted || other || path.is_empty() {
            continue;
        }
        versions.push(Version {
            commit: commit.clone(),
            author: author.clone(),
            time: *time,
            summary: summary.clone(),
            blob: blob.to_owned(),
            path: path_from_bytes(path),
            source: Source::Commit,
        });
    }
    // Git lists the newest first.
    versions.reverse();
    (versions, commits)
}

/// What a [`BlobReader`] found for an object id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blob {
    /// Decoded as the editor decodes the file on disk, line breaks as the
    /// buffer keeps them, so a version diffs cleanly against the buffer.
    Text(String),
    /// Bytes the editor would not open as text.
    Binary,
    /// Over the size asked for; not read into memory.
    TooLarge,
    /// No such object, or one that is not a blob.
    Missing,
}

/// One `git cat-file --batch` kept running to read blobs on demand. The
/// process is killed when the reader is dropped.
pub struct BlobReader {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Set when a read failed partway: the stream is out of step, and every
    /// later read fails rather than reading the wrong object.
    broken: bool,
}

impl BlobReader {
    pub fn open(root: &Path) -> Result<BlobReader, String> {
        let mut child = command(root)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("git cat-file has no pipes".into());
        };
        Ok(BlobReader {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            broken: false,
        })
    }

    /// The blob `oid`, read whole when it is at most `max_bytes`.
    pub fn read(&mut self, oid: &str, max_bytes: usize) -> Result<Blob, String> {
        if self.broken {
            return Err("git cat-file stopped".into());
        }
        // Anything but an object id would be read as another request.
        if oid.is_empty() || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("not an object id: {oid:?}"));
        }
        let result = self.request(oid, max_bytes);
        if result.is_err() {
            self.broken = true;
        }
        result
    }

    fn request(&mut self, oid: &str, max_bytes: usize) -> Result<Blob, String> {
        let io = |e: std::io::Error| e.to_string();
        self.stdin
            .write_all(format!("{oid}\n").as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(io)?;
        let mut header = Vec::new();
        self.stdout.read_until(b'\n', &mut header).map_err(io)?;
        if header.pop() != Some(b'\n') {
            return Err("git cat-file ended".into());
        }
        let header = String::from_utf8_lossy(&header);
        let parts: Vec<&str> = header.split(' ').collect();
        let [_, kind, size] = parts[..] else {
            // `<oid> missing`, or `<oid> ambiguous`: no contents follow.
            return Ok(Blob::Missing);
        };
        let size: usize = size
            .parse()
            .map_err(|_| format!("git cat-file sent {header:?}"))?;
        // The contents and the line break after them, read whatever is
        // done with them, so the next reply starts where it should.
        let wanted = kind == "blob" && size <= max_bytes;
        let mut contents = Vec::new();
        if wanted {
            contents.reserve_exact(size + 1);
            (&mut self.stdout)
                .take(size as u64 + 1)
                .read_to_end(&mut contents)
                .map_err(io)?;
        } else {
            let skipped = std::io::copy(
                &mut (&mut self.stdout).take(size as u64 + 1),
                &mut std::io::sink(),
            )
            .map_err(io)?;
            contents.resize(skipped as usize, 0);
        }
        if contents.len() != size + 1 {
            return Err("git cat-file ended".into());
        }
        if kind != "blob" {
            return Ok(Blob::Missing);
        }
        if !wanted {
            return Ok(Blob::TooLarge);
        }
        contents.pop();
        Ok(decode(&contents))
    }
}

/// `bytes` as the editor opens a file: what it would show as a preview
/// rather than text is binary.
fn decode(bytes: &[u8]) -> Blob {
    match crate::text::file_format::decode(bytes) {
        Ok((text, _)) => Blob::Text(text),
        Err(_) => Blob::Binary,
    }
}

impl Drop for BlobReader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
        let out = command(dir).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    /// A repository with a test identity and no signing.
    fn repo(name: &str) -> crate::project::TempTree {
        let tree = crate::project::TempTree::new(name, &[]);
        git(&tree.0, &["init", "--quiet"]);
        for (key, value) in [
            ("user.name", "caio test"),
            ("user.email", "test@example.invalid"),
            ("commit.gpgsign", "false"),
        ] {
            git(&tree.0, &["config", key, value]);
        }
        tree
    }

    fn commit(dir: &Path, message: &str) -> String {
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "--quiet", "-m", message]);
        String::from_utf8(git(dir, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn blob(dir: &Path, spec: &str) -> String {
        String::from_utf8(git(dir, &["rev-parse", spec]))
            .unwrap()
            .trim()
            .to_owned()
    }

    #[test]
    fn file_history_follows_renames_and_lists_blobs() {
        let tree = repo("history-follow");
        let dir = tree.0.as_path();
        let mut expected = Vec::new();
        for (n, text) in ["one\n", "one\ntwo\n", "one\ntwo\nthree\n"]
            .into_iter()
            .enumerate()
        {
            std::fs::write(dir.join("notes.txt"), text).unwrap();
            let hash = commit(dir, &format!("version {n}"));
            expected.push((
                hash.clone(),
                "notes.txt",
                blob(dir, &format!("{hash}:notes.txt")),
            ));
        }
        // Something else in the repository, which the history leaves out.
        std::fs::write(dir.join("other.txt"), "other\n").unwrap();
        commit(dir, "unrelated");
        git(dir, &["mv", "notes.txt", "journal.txt"]);
        let hash = commit(dir, "rename");
        expected.push((
            hash.clone(),
            "journal.txt",
            blob(dir, &format!("{hash}:journal.txt")),
        ));
        std::fs::write(dir.join("journal.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let hash = commit(dir, "version 4");
        expected.push((
            hash.clone(),
            "journal.txt",
            blob(dir, &format!("{hash}:journal.txt")),
        ));

        let (versions, capped) =
            file_history(dir, Path::new("journal.txt"), HISTORY_LIMIT).unwrap();
        assert!(!capped);
        let got: Vec<(String, &str, String)> = versions
            .iter()
            .map(|v| (v.commit.clone(), v.path.to_str().unwrap(), v.blob.clone()))
            .collect();
        assert_eq!(got, expected);
        assert_eq!(versions[0].author, "caio test");
        assert_eq!(versions[0].summary, "version 0");
        assert!(versions[0].time > 0);
        assert_eq!(
            versions[3].blob, versions[2].blob,
            "a rename keeps the blob"
        );

        let (versions, capped) = file_history(dir, Path::new("journal.txt"), 2).unwrap();
        assert!(capped);
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[1].summary, "version 4", "the newest are kept");

        let empty = repo("history-unborn");
        assert_eq!(
            file_history(&empty.0, Path::new("a.txt"), HISTORY_LIMIT).unwrap(),
            (Vec::new(), false)
        );
    }

    const ZERO: &str = "0000000000000000000000000000000000000000";
    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";

    /// `git log` output for one commit, as `-z --raw` writes it.
    fn entry(hash: &str, subject: &str, raw: &str, paths: &[&str]) -> Vec<u8> {
        let mut out = format!("\x01{hash}\0someone\x001700000000\0{subject}\0").into_bytes();
        if !raw.is_empty() {
            out.extend_from_slice(format!("\n:{raw}\0").as_bytes());
            for path in paths {
                out.extend_from_slice(path.as_bytes());
                out.push(0);
            }
        }
        out
    }

    #[test]
    fn parser_reads_renames_and_skips_what_has_no_text() {
        let mut log = Vec::new();
        // Newest first, as git writes it.
        log.extend(entry(
            "c5",
            "remove it",
            &format!("100644 000000 {B} {ZERO} D"),
            &["new.txt"],
        ));
        log.extend(entry(
            "c4",
            "a submodule now",
            &format!("100644 160000 {A} {B} T"),
            &["new.txt"],
        ));
        log.extend(entry(
            "c3",
            "a link now",
            &format!("100644 120000 {A} {B} T"),
            &["new.txt"],
        ));
        log.extend(entry(
            "c2",
            "move \x1b[31mit\x07\ttoo",
            &format!("100644 100644 {A} {A} R100"),
            &["old dir/old.txt", "new.txt"],
        ));
        log.extend(entry("merge", "a merge", "", &[]));
        log.extend(entry(
            "c1",
            "",
            &format!("000000 100644 {ZERO} {A} A"),
            &["old dir/old.txt"],
        ));
        let versions = parse_file_log(&log);
        let got: Vec<(&str, &str, &str, &str)> = versions
            .iter()
            .map(|v| {
                (
                    v.commit.as_str(),
                    v.summary.as_str(),
                    v.blob.as_str(),
                    v.path.to_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("c1", "", A, "old dir/old.txt"),
                ("c2", "move it too", A, "new.txt"),
            ]
        );
        assert_eq!(versions[0].time, 1_700_000_000);
        assert_eq!(versions[0].author, "someone");
        assert_eq!(parse(&log).1, 6, "every commit counts toward the limit");
        assert!(parse_file_log(b"").is_empty());
        assert!(parse_file_log(b"\x01abc\0trunc").is_empty());
    }

    fn commit_at(name: &str, time: i64) -> Version {
        Version {
            commit: name.repeat(40),
            author: "Robin".into(),
            time,
            summary: name.into(),
            blob: name.repeat(40),
            path: PathBuf::from("garden.txt"),
            source: Source::Commit,
        }
    }

    fn local_at(id: i64, at_ms: i64, reason: Reason) -> Version {
        let entry = Entry {
            id: SnapshotId(id),
            at: at_ms,
            reason,
            size: 1,
        };
        Version::local(&entry, Path::new("/garden/garden.txt"))
    }

    /// Each version as `c:<summary>` or `l:<id>`, oldest first.
    fn names(versions: &[Version]) -> Vec<String> {
        versions
            .iter()
            .map(|v| match v.source {
                Source::Commit => format!("c:{}", v.summary),
                Source::Local { id, .. } => format!("l:{}", id.0),
            })
            .collect()
    }

    #[test]
    fn local_versions_go_between_commits_by_time() {
        let commits = vec![
            commit_at("a", 100),
            commit_at("b", 200),
            commit_at("c", 300),
        ];
        // Newest first, as the store lists them; two in one second, and
        // one in the very second of a commit, which it goes before.
        let local = vec![
            local_at(6, 400_000, Reason::Idle),
            local_at(5, 300_500, Reason::Saved),
            local_at(4, 150_900, Reason::Idle),
            local_at(3, 150_100, Reason::Saved),
            local_at(2, 150_100, Reason::Opened),
            local_at(1, 50_000, Reason::Opened),
        ];
        let merged = merge(commits.clone(), local);
        assert_eq!(
            names(&merged),
            [
                "l:1", "c:a", "l:2", "l:3", "l:4", "c:b", "l:5", "c:c", "l:6"
            ]
        );
        assert_eq!(merged[0].time, 50, "milliseconds to seconds");
        assert!(merged[0].is_local() && !merged[1].is_local());
        // Either kind alone is itself.
        assert_eq!(merge(commits.clone(), Vec::new()), commits);
        let only = merge(
            Vec::new(),
            vec![
                local_at(2, 2_000, Reason::Saved),
                local_at(1, 1_000, Reason::Opened),
            ],
        );
        assert_eq!(names(&only), ["l:1", "l:2"]);
        // A clock set back: by time all the same, the id breaking ties.
        let back = merge(
            Vec::new(),
            vec![
                local_at(1, 9_000, Reason::Saved),
                local_at(2, 1_000, Reason::Saved),
            ],
        );
        assert_eq!(names(&back), ["l:2", "l:1"]);
    }

    #[test]
    fn a_local_version_equal_to_a_commit_beside_it_is_dropped() {
        // Opened at a, saved twice, the second save committed as b.
        let mut versions = merge(
            vec![commit_at("a", 100), commit_at("b", 300)],
            vec![
                local_at(1, 150_000, Reason::Opened),
                local_at(2, 200_000, Reason::Saved),
                local_at(3, 250_000, Reason::Saved),
                local_at(4, 400_000, Reason::Idle),
            ],
        );
        // Texts by local id; a commit's text is its summary.
        let text = |id: i64| ["", "a", "a and more", "b", "b and more"][id as usize];
        let mut asked = Vec::new();
        let same = |l: &Version, c: &Version| {
            let Source::Local { id, .. } = l.source else {
                panic!("asked about two commits");
            };
            text(id.0) == c.summary
        };
        drop_duplicates(&mut versions, 64, |l, c| {
            asked.push((names(std::slice::from_ref(l)), c.summary.clone()));
            same(l, c)
        });
        assert_eq!(names(&versions), ["c:a", "l:2", "c:b", "l:4"]);
        // Newest first, each local only against the commits beside it.
        assert_eq!(asked[0], (vec!["l:4".to_string()], "b".to_string()));
        assert!(!asked.iter().any(|(l, _)| l[0] == "l:2"), "{asked:?}");
        // Out of checks: kept, newest dropped first.
        let mut versions = merge(
            vec![commit_at("a", 100), commit_at("b", 300)],
            vec![
                local_at(1, 150_000, Reason::Opened),
                local_at(3, 250_000, Reason::Saved),
            ],
        );
        drop_duplicates(&mut versions, 1, same);
        assert_eq!(names(&versions), ["c:a", "l:1", "c:b"]);
        // Outside Git there is nothing to compare with.
        let mut alone = merge(Vec::new(), vec![local_at(1, 1_000, Reason::Saved)]);
        drop_duplicates(&mut alone, 64, |_, _| panic!("nothing to compare"));
        assert_eq!(names(&alone), ["l:1"]);
    }

    #[test]
    fn blob_ids_are_gits() {
        assert_eq!(
            blob_id(b"").as_deref(),
            Some("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391")
        );
        let tree = repo("history-blob-id");
        let dir = tree.0.as_path();
        std::fs::write(dir.join("a.txt"), "hello\nworld\n").unwrap();
        let hash = commit(dir, "a");
        assert_eq!(
            blob_id(b"hello\nworld\n"),
            Some(blob(dir, &format!("{hash}:a.txt")))
        );
    }

    #[test]
    fn blob_reader_reads_text_binary_missing_and_too_large() {
        let tree = repo("history-blobs");
        let dir = tree.0.as_path();
        std::fs::write(dir.join("text.txt"), "hello\nworld\n").unwrap();
        std::fs::write(dir.join("crlf.txt"), "one\r\ntwo\rthree\r\n").unwrap();
        std::fs::write(dir.join("data.bin"), [0u8, 0xff, 1, 2]).unwrap();
        std::fs::write(dir.join("big.txt"), "x".repeat(4096)).unwrap();
        let hash = commit(dir, "files");
        let id = |name: &str| blob(dir, &format!("{hash}:{name}"));
        let mut reader = BlobReader::open(dir).unwrap();
        // Several reads on one process, the large one skipped in between.
        assert_eq!(
            reader.read(&id("text.txt"), 1024).unwrap(),
            Blob::Text("hello\nworld\n".into())
        );
        assert_eq!(reader.read(&id("big.txt"), 1024).unwrap(), Blob::TooLarge);
        assert_eq!(
            reader.read(&id("crlf.txt"), 1024).unwrap(),
            Blob::Text("one\ntwo\nthree\n".into()),
            "line breaks as the buffer keeps them"
        );
        assert_eq!(reader.read(&id("data.bin"), 1024).unwrap(), Blob::Binary);
        assert_eq!(reader.read(&"0".repeat(40), 1024).unwrap(), Blob::Missing);
        // A commit is not a blob; its contents are skipped all the same.
        assert_eq!(reader.read(&hash, 1 << 20).unwrap(), Blob::Missing);
        assert!(reader.read("HEAD\nmaster", 1024).is_err());
        assert_eq!(
            reader.read(&id("big.txt"), 4096).unwrap(),
            Blob::Text("x".repeat(4096))
        );
        assert_eq!(
            reader.read(&id("text.txt"), 1024).unwrap(),
            Blob::Text("hello\nworld\n".into()),
            "still in step"
        );
    }
}
