//! The parse thread for documents too big to parse between two frames.
//!
//! One thread, owning its own parsers, taking one job at a time. A job is a
//! snapshot of the text, which shares the rope's nodes rather than copying
//! them, and copies of the trees last drawn, already moved through the
//! edits since. The thread never sees a buffer or the store; the store
//! decides on the main thread whether a result still describes anything.
//!
//! Nothing here may panic: the release build aborts on a panic, from any
//! thread, and this one holds no state worth that.

use std::sync::mpsc;

use super::{Highlighter, Language, Tree, parse_document};
use crate::text::rope::Rope;

/// A parse to run.
pub(super) struct Job {
    pub id: u64,
    pub generation: u64,
    pub language: Language,
    pub rope: Rope,
    /// The last parse, edited to `rope`'s positions; `None` to parse from
    /// scratch.
    pub old: Option<Tree>,
    pub old_layers: Vec<(Language, Tree)>,
}

/// A finished parse, for the job with the same document and generation.
pub(super) struct Done {
    pub id: u64,
    pub generation: u64,
    /// `None` when the parse failed, which leaves the document with what it
    /// had.
    pub tree: Option<Tree>,
    pub layers: Vec<(Language, Tree)>,
}

/// What the thread is handed.
enum Work {
    Parse(Job),
    /// Trees the main thread is done with. Freeing one is linear in the
    /// nodes no other tree shares, which after a parse of a big file is
    /// milliseconds: a frame's worth on the main thread.
    Free(Vec<Tree>),
}

pub(super) struct Worker {
    jobs: mpsc::Sender<Work>,
    pub done: mpsc::Receiver<Done>,
}

impl Worker {
    /// Starts the thread, or `None` when the system will not give one.
    pub fn spawn() -> Option<Worker> {
        let (jobs, inbox) = mpsc::channel::<Work>();
        let (outbox, done) = mpsc::channel::<Done>();
        std::thread::Builder::new()
            .name("crc-syntax".into())
            .spawn(move || run(inbox, outbox))
            .ok()?;
        Some(Worker { jobs, done })
    }

    /// Queues `job`, or hands it back when the thread has gone.
    pub fn send(&self, job: Job) -> Result<(), Job> {
        let Err(mpsc::SendError(Work::Parse(job))) = self.jobs.send(Work::Parse(job)) else {
            return Ok(());
        };
        Err(job)
    }

    /// Frees `trees` on the thread. When it has gone they are freed here,
    /// as the returned work is dropped.
    pub fn free(&self, trees: Vec<Tree>) {
        if !trees.is_empty() {
            let _ = self.jobs.send(Work::Free(trees));
        }
    }
}

/// Parses jobs in the order they come until the store, and with it the
/// sending half, is dropped.
fn run(inbox: mpsc::Receiver<Work>, outbox: mpsc::Sender<Done>) {
    let mut highlighters: Vec<Highlighter> = Vec::new();
    while let Ok(work) = inbox.recv() {
        let job = match work {
            Work::Parse(job) => job,
            Work::Free(trees) => {
                drop(trees);
                continue;
            }
        };
        let parsed = parse_document(
            &mut highlighters,
            job.language,
            &job.rope,
            job.old.as_ref(),
            &job.old_layers,
        );
        let (tree, layers) = match parsed {
            Some((tree, layers)) => (Some(tree), layers),
            None => (None, Vec::new()),
        };
        let done = Done {
            id: job.id,
            generation: job.generation,
            tree,
            layers,
        };
        if outbox.send(done).is_err() {
            return;
        }
    }
}
