//! The completion worker: one thread that answers "what could follow this
//! prefix" from everything but the language server, and records what was
//! accepted. Only the newest question is answered; older ones queued behind
//! it are dropped, since typing has already moved past them.

use std::path::PathBuf;
use std::sync::mpsc;

use super::history::History;
use super::{Boost, Candidate, PathQuery, Source};
use crate::index::store::{Reader, db_path};
use crate::text::rope::Rope;

/// Bytes either side of the caret scanned for words in the file itself.
const WINDOW: usize = 256 * 1024;

/// One question.
pub struct Query {
    pub generation: u64,
    pub rope: Rope,
    /// The word being typed: `anchor..caret`.
    pub anchor: usize,
    pub caret: usize,
    pub prefix: String,
    /// `tree.` in `state.tree.ro`; see [`super::context_before`].
    pub context: String,
    pub root: Option<PathBuf>,
    /// The history's language key: the file's extension.
    pub language: String,
    /// When the text before the caret is a path: list that folder instead.
    pub path: Option<PathQuery>,
}

/// Its answer.
pub struct Answer {
    pub generation: u64,
    pub candidates: Vec<Candidate>,
    pub boosts: Vec<Boost>,
}

enum Job {
    Query(Query),
    Accepted {
        root: PathBuf,
        language: String,
        context: String,
        text: String,
    },
    Forget(PathBuf),
}

pub struct Worker {
    tx: mpsc::Sender<Job>,
    rx: mpsc::Receiver<Answer>,
    /// Rows deleted by the last Forget, when it has finished.
    forgotten: mpsc::Receiver<i32>,
}

impl Worker {
    /// Starts the thread. `wake` is called after each answer, from the
    /// worker, to get the main thread to look.
    pub fn start(history: Option<PathBuf>, wake: Box<dyn Fn() + Send>) -> Option<Worker> {
        let (tx, jobs) = mpsc::channel::<Job>();
        let (answers, rx) = mpsc::channel::<Answer>();
        let (forgot, forgotten) = mpsc::channel::<i32>();
        std::thread::Builder::new()
            .name("crc-complete".into())
            .spawn(move || {
                let history = history.and_then(|path| History::open(&path).ok());
                let mut reader: Option<(PathBuf, Reader)> = None;
                while let Ok(mut job) = jobs.recv() {
                    // Skip to the newest question, doing any bookkeeping on
                    // the way.
                    while let Ok(next) = jobs.try_recv() {
                        if let Job::Query(_) = job {
                            job = next;
                            continue;
                        }
                        run_side_job(job, history.as_ref(), &forgot);
                        job = next;
                    }
                    let Job::Query(query) = job else {
                        run_side_job(job, history.as_ref(), &forgot);
                        continue;
                    };
                    // The index file appears once the indexer's first pass
                    // has started; keep trying until it has.
                    if let Some(root) = &query.root
                        && reader.as_ref().is_none_or(|(r, _)| r != root)
                    {
                        reader = db_path(root)
                            .and_then(|p| Reader::open(&p))
                            .map(|r| (root.clone(), r));
                    }
                    let answer = answer(&query, reader.as_ref().map(|(_, r)| r), history.as_ref());
                    if answers.send(answer).is_err() {
                        return;
                    }
                    wake();
                }
            })
            .ok()?;
        Some(Worker { tx, rx, forgotten })
    }

    pub fn ask(&self, query: Query) {
        let _ = self.tx.send(Job::Query(query));
    }

    /// The newest answer that has arrived, if any.
    pub fn take(&self) -> Option<Answer> {
        self.rx.try_iter().last()
    }

    pub fn accepted(&self, root: PathBuf, language: String, context: String, text: String) {
        let _ = self.tx.send(Job::Accepted {
            root,
            language,
            context,
            text,
        });
    }

    /// Deletes `root`'s history; the count arrives through [`Worker::forgotten`].
    pub fn forget(&self, root: PathBuf) {
        let _ = self.tx.send(Job::Forget(root));
    }

    pub fn forgotten(&self) -> Option<i32> {
        self.forgotten.try_recv().ok()
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn run_side_job(job: Job, history: Option<&History>, forgot: &mpsc::Sender<i32>) {
    match job {
        Job::Accepted {
            root,
            language,
            context,
            text,
        } => {
            if let Some(history) = history {
                let _ = history.record(&root.to_string_lossy(), &language, &context, &text, now());
            }
        }
        Job::Forget(root) => {
            let rows = history.map_or(0, |h| h.forget(&root.to_string_lossy()).unwrap_or(0));
            let _ = forgot.send(rows);
        }
        Job::Query(_) => {}
    }
}

/// Everything but the server has to say about `query`.
pub fn answer(query: &Query, reader: Option<&Reader>, history: Option<&History>) -> Answer {
    let project = query
        .root
        .as_ref()
        .map(|r| r.to_string_lossy().into_owned())
        .unwrap_or_default();
    let boosts = history
        .and_then(|h| {
            h.boosts(
                &project,
                &query.language,
                &query.context,
                &query.prefix,
                now(),
            )
            .ok()
        })
        .unwrap_or_default();
    let mut candidates = Vec::new();

    if let Some(path) = &query.path {
        candidates = super::path_candidates(path, 60);
        return Answer {
            generation: query.generation,
            candidates,
            boosts,
        };
    }

    // Words near the caret, in this file.
    let total = query.rope.len_bytes();
    let from = query
        .rope
        .line_to_byte(query.rope.byte_to_line(query.anchor.saturating_sub(WINDOW)));
    let to_line = query.rope.byte_to_line((query.caret + WINDOW).min(total));
    let to = if to_line + 1 < query.rope.len_lines() {
        query.rope.line_to_byte(to_line + 1)
    } else {
        total
    };
    let text = query.rope.slice_to_string(from..to);
    let skip = query.anchor.saturating_sub(from)..query.caret.saturating_sub(from);
    for (word, count) in super::words(&text, &query.prefix, Some(skip)) {
        candidates.push(Candidate {
            insert: word.clone(),
            label: word,
            source: Source::Word,
            why: if count == 1 {
                "used once in this file".into()
            } else {
                format!("used {count}× in this file")
            },
            weight: (0.3 + 0.1 * count as f32).min(0.9),
            server: None,
        });
    }

    // The project index: definitions anywhere, and words across files.
    if let Some(reader) = reader
        && !query.prefix.is_empty()
    {
        for symbol in reader.symbols(&query.prefix, 60).unwrap_or_default() {
            candidates.push(Candidate {
                insert: symbol.name.clone(),
                label: symbol.name,
                source: Source::Symbol,
                why: format!("{} in {}:{}", symbol.kind, symbol.path, symbol.line + 1),
                weight: 0.8,
                server: None,
            });
        }
        for (word, count, files) in reader.words(&query.prefix, 60).unwrap_or_default() {
            candidates.push(Candidate {
                insert: word.clone(),
                label: word,
                source: Source::Word,
                why: if files == 1 {
                    format!("used {count}× in 1 project file")
                } else {
                    format!("used {count}× in {files} project files")
                },
                weight: (0.25 + 0.05 * files as f32).min(0.75),
                server: None,
            });
        }
    }

    // What you accepted before and nothing else offers.
    for boost in &boosts {
        if candidates.iter().any(|c| c.label == boost.text) {
            continue;
        }
        candidates.push(Candidate {
            label: boost.text.clone(),
            insert: boost.text.clone(),
            source: Source::History,
            why: String::new(),
            weight: 0.6,
            server: None,
        });
    }
    Answer {
        generation: query.generation,
        candidates,
        boosts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(text: &str, prefix: &str) -> Query {
        let caret = text.len();
        Query {
            generation: 7,
            rope: Rope::from_text(text),
            anchor: caret - prefix.len(),
            caret,
            prefix: prefix.into(),
            context: String::new(),
            root: None,
            language: "rs".into(),
            path: None,
        }
    }

    #[test]
    fn words_from_the_file_say_how_often() {
        let answer = answer(&query("water watering water\nwat", "wat"), None, None);
        assert_eq!(answer.generation, 7);
        let water = answer
            .candidates
            .iter()
            .find(|c| c.label == "water")
            .unwrap();
        assert_eq!(
            (water.source, water.why.as_str()),
            (Source::Word, "used 2× in this file")
        );
        assert!(
            !answer.candidates.iter().any(|c| c.label == "wat"),
            "not the word being typed"
        );
    }

    #[test]
    fn the_worker_answers_the_newest_question_and_records_history() {
        let dir = std::env::temp_dir().join(format!("crc-worker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (woke, wakes) = mpsc::channel();
        let worker = Worker::start(
            Some(dir.join("completion.db")),
            Box::new(move || {
                let _ = woke.send(());
            }),
        )
        .unwrap();
        worker.accepted(dir.clone(), "rs".into(), String::new(), "watering".into());
        let mut ask = query("water watering\nwa", "wa");
        ask.root = Some(dir.clone());
        worker.ask(ask);
        wakes
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let answer = worker.take().unwrap();
        assert_eq!(
            answer.boosts.len(),
            1,
            "the acceptance was recorded before the query ran"
        );
        let ranked = super::super::rank("wa", answer.candidates, &answer.boosts, 5);
        assert_eq!(
            ranked[0].label, "watering",
            "history lifts it over the plain word"
        );
        assert!(
            ranked[0].why.starts_with("you picked this 1× here"),
            "{}",
            ranked[0].why
        );

        worker.forget(dir.clone());
        let mut waited = 0;
        let rows = loop {
            if let Some(rows) = worker.forgotten() {
                break rows;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            waited += 1;
            assert!(waited < 1000);
        };
        assert_eq!(rows, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
