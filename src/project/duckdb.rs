//! Runs SQL over a data file through the `duckdb` command line tool.
//!
//! One subprocess per query, on a worker thread, with the same shape of
//! limits curl and Git commands have: a wall-clock timeout, a cap on rows
//! and bytes kept, and stderr shown on failure. The file is exposed to the
//! statement as a view named `here`, so the user writes
//! `SELECT * FROM here WHERE ...` whatever the format underneath. Output is
//! CSV, which the grid already draws.
//!
//! `CRC_DUCKDB_FAKE=<script>` makes [`duckdb`] return that script instead
//! of looking for the real tool, which is how the tests and the GUI
//! self-test drive the whole path without DuckDB installed
//! (`scripts/fake-duckdb.py`).

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Rows kept from one query; the rest is dropped and the result marked
/// truncated. The grid has no use for more, and DuckDB would happily
/// stream a billion.
pub const MAX_ROWS: usize = 10_000;

/// Bytes of output kept, for a few very wide rows.
pub const MAX_BYTES: usize = 64 * 1024 * 1024;

/// How long a query may run before it is stopped.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// The `duckdb` binary, looked for where the LSP servers and `gh` are, or
/// the fake named by `CRC_DUCKDB_FAKE`.
pub fn duckdb() -> Option<PathBuf> {
    if let Some(fake) = std::env::var_os("CRC_DUCKDB_FAKE") {
        return Some(PathBuf::from(fake));
    }
    crate::lsp::servers::search_dirs()
        .into_iter()
        .map(|dir| dir.join("duckdb"))
        .find(|path| path.is_file())
}

/// What to tell the user when [`duckdb`] finds nothing.
pub fn missing() -> &'static str {
    "DuckDB is not installed (brew install duckdb)"
}

/// Which DuckDB reader opens a file, by its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reader {
    Csv,
    Parquet,
    Json,
    Sqlite,
}

impl Reader {
    /// The reader for `path`'s extension, or `None` for a file DuckDB has
    /// no reader for here.
    pub fn for_path(path: &Path) -> Option<Reader> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            "csv" | "tsv" => Some(Reader::Csv),
            "parquet" => Some(Reader::Parquet),
            "json" | "jsonl" | "ndjson" => Some(Reader::Json),
            "sqlite" | "db" => Some(Reader::Sqlite),
            _ => None,
        }
    }
}

/// The table expression that reads `path`: a DuckDB function call with the
/// path as a quoted string literal. SQLite files also need the `table` to
/// read. `Err` names a path no literal can carry.
pub fn source_sql(reader: Reader, path: &Path, table: Option<&str>) -> Result<String, String> {
    let path = quote(&path.to_string_lossy())?;
    Ok(match reader {
        Reader::Csv => format!("read_csv_auto({path})"),
        Reader::Parquet => format!("read_parquet({path})"),
        Reader::Json => format!("read_json_auto({path})"),
        Reader::Sqlite => {
            let table = table.ok_or("a SQLite file needs a table name")?;
            format!("sqlite_scan({path}, {})", quote(table)?)
        }
    })
}

/// `text` as a SQL string literal: apostrophes doubled. A NUL byte cannot
/// be passed to the tool at all.
fn quote(text: &str) -> Result<String, String> {
    if text.contains('\0') {
        return Err("the path contains a NUL byte".into());
    }
    Ok(format!("'{}'", text.replace('\'', "''")))
}

/// One statement over one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub source: PathBuf,
    pub reader: Reader,
    /// The table inside a SQLite file; ignored for other readers.
    pub table: Option<String>,
    pub sql: String,
}

/// The command line that runs `query` with the tool at `exe`, one argument
/// per element, no shell: the file becomes the view `here`, then the
/// user's statement runs. CSV output, NULL printed empty rather than as
/// the word NULL. `Err` is [`source_sql`]'s.
pub fn argv(query: &Query, exe: &Path) -> Result<Vec<OsString>, String> {
    let source = source_sql(query.reader, &query.source, query.table.as_deref())?;
    Ok(vec![
        exe.as_os_str().to_owned(),
        "-no-init".into(),
        "-csv".into(),
        "-nullvalue".into(),
        "".into(),
        "-c".into(),
        format!("CREATE VIEW here AS SELECT * FROM {source}").into(),
        "-c".into(),
        query.sql.clone().into(),
    ])
}

/// What a finished query produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outcome {
    /// The tool's CSV output, header first.
    pub csv: String,
    /// Lines after the header. A quoted field spanning lines counts once
    /// per line, so this is approximate for such output.
    pub rows: usize,
    /// Whether output stopped at [`MAX_ROWS`] or [`MAX_BYTES`].
    pub truncated: bool,
    pub elapsed: Duration,
}

/// Runs `query` with the tool at `exe` on a worker thread. The receiver
/// yields the outcome once.
pub fn spawn(query: Query, exe: &Path) -> mpsc::Receiver<Result<Outcome, String>> {
    let (tx, rx) = mpsc::channel();
    let exe = exe.to_path_buf();
    std::thread::spawn(move || {
        let _ = tx.send(run(&query, &exe));
    });
    rx
}

/// What the stdout reader hands back.
#[derive(Default)]
struct Captured {
    csv: String,
    lines: usize,
    truncated: bool,
}

/// Runs `query` to completion on this thread, within the caps.
pub fn run(query: &Query, exe: &Path) -> Result<Outcome, String> {
    let argv = argv(query, exe)?;
    let started = Instant::now();
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not start duckdb: {e}"))?;

    let mut stderr = child.stderr.take().ok_or("Missing duckdb stderr")?;
    let errors = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.by_ref().take(64 * 1024).read_to_end(&mut bytes);
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        bytes
    });

    // Read stdout line by line on its own thread, stopping at the caps;
    // this thread watches the clock and the child, and kills it when the
    // reader stops early or time runs out.
    let stdout = child.stdout.take().ok_or("Missing duckdb stdout")?;
    let (out_tx, out_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut captured = Captured::default();
        let mut reader = BufReader::new(stdout.take(MAX_BYTES as u64 + 1));
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if captured.csv.len() + line.len() > MAX_BYTES {
                captured.truncated = true;
                break;
            }
            captured.csv.push_str(&line);
            captured.lines += 1;
            if captured.lines > MAX_ROWS {
                captured.truncated = true;
                break;
            }
        }
        let _ = out_tx.send(captured);
    });

    let mut captured: Option<Captured> = None;
    let status = loop {
        if captured.is_none()
            && let Ok(done) = out_rx.try_recv()
        {
            if done.truncated {
                let _ = child.kill();
            }
            captured = Some(done);
        }
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if started.elapsed() > TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "the query took over {} seconds and was stopped",
                    TIMEOUT.as_secs()
                ));
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let captured = captured.or_else(|| out_rx.recv().ok()).unwrap_or_default();
    let errors = errors.join().unwrap_or_default();

    if !captured.truncated && !status.success() {
        let text = String::from_utf8_lossy(&errors);
        let said: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(2)
            .collect();
        return Err(if said.is_empty() {
            format!("duckdb exited with {status}")
        } else {
            said.join("\n")
        });
    }
    Ok(Outcome {
        rows: captured.lines.saturating_sub(1),
        csv: captured.csv,
        truncated: captured.truncated,
        elapsed: started.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/fake-duckdb.py")
    }

    fn query(path: &str, sql: &str) -> Query {
        let source = PathBuf::from(path);
        Query {
            reader: Reader::for_path(&source).expect("reader"),
            source,
            table: None,
            sql: sql.into(),
        }
    }

    fn strings(argv: &[OsString]) -> Vec<String> {
        argv.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn quotes_path_with_apostrophe() {
        let sql = source_sql(Reader::Csv, Path::new("/data/it's here.csv"), None).expect("sql");
        assert_eq!(sql, "read_csv_auto('/data/it''s here.csv')");
        let sqlite =
            source_sql(Reader::Sqlite, Path::new("/d/a.db"), Some("o'brien")).expect("sql");
        assert_eq!(sqlite, "sqlite_scan('/d/a.db', 'o''brien')");
        assert!(source_sql(Reader::Sqlite, Path::new("/d/a.db"), None).is_err());
        assert!(source_sql(Reader::Csv, Path::new("/d/a\0b.csv"), None).is_err());
    }

    #[test]
    fn picks_reader_by_extension() {
        for (name, reader) in [
            ("a.csv", Some(Reader::Csv)),
            ("a.TSV", Some(Reader::Csv)),
            ("a.parquet", Some(Reader::Parquet)),
            ("a.json", Some(Reader::Json)),
            ("a.jsonl", Some(Reader::Json)),
            ("a.ndjson", Some(Reader::Json)),
            ("a.sqlite", Some(Reader::Sqlite)),
            ("a.db", Some(Reader::Sqlite)),
            ("a.txt", None),
            ("noext", None),
        ] {
            assert_eq!(Reader::for_path(Path::new(name)), reader, "{name}");
        }
    }

    #[test]
    fn builds_argv_without_shell() {
        let q = query("/data/sales; rm -rf.csv", "SELECT * FROM here WHERE a > 1");
        let a = strings(&argv(&q, Path::new("/opt/homebrew/bin/duckdb")).expect("argv"));
        assert_eq!(
            a,
            [
                "/opt/homebrew/bin/duckdb",
                "-no-init",
                "-csv",
                "-nullvalue",
                "",
                "-c",
                "CREATE VIEW here AS SELECT * FROM read_csv_auto('/data/sales; rm -rf.csv')",
                "-c",
                "SELECT * FROM here WHERE a > 1",
            ]
        );
    }

    #[test]
    fn missing_tool_message() {
        assert_eq!(missing(), "DuckDB is not installed (brew install duckdb)");
    }

    #[test]
    fn a_query_returns_csv() {
        let q = query("/data/a.csv", "SELECT * FROM here");
        let outcome = spawn(q, &fake())
            .recv_timeout(Duration::from_secs(20))
            .expect("answers")
            .expect("succeeds");
        assert_eq!(outcome.csv, "a,b\n1,x\n2,y\n3,z\n");
        assert_eq!(outcome.rows, 3);
        assert!(!outcome.truncated);
    }

    #[test]
    fn an_error_names_the_catalog() {
        let q = query("/data/a.parquet", "SELECT * FROM nope");
        let error = spawn(q, &fake())
            .recv_timeout(Duration::from_secs(20))
            .expect("answers")
            .expect_err("fails");
        assert_eq!(
            error,
            "Catalog Error: Table with name nope does not exist!\nLINE 1: SELECT * FROM nope"
        );
    }

    #[test]
    fn caps_rows_and_marks_truncated() {
        let q = query("/data/a.json", "SELECT * FROM big");
        let outcome = spawn(q, &fake())
            .recv_timeout(Duration::from_secs(20))
            .expect("answers")
            .expect("a killed child is not an error");
        assert!(outcome.truncated);
        assert_eq!(outcome.rows, MAX_ROWS);
        assert_eq!(outcome.csv.lines().count(), MAX_ROWS + 1);
        assert!(outcome.csv.starts_with("n,word\n0,row 0\n"));
    }
}
