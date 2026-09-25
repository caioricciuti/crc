//! One cancellable shaping worker per atlas, with bounded, coalesced requests.
use super::reuse::LineReuse;
use super::{ShapedLine, load_monospace, shape};
use crate::text::rope::Rope;
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};

type Owner = (u64, usize);
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;
const MAX_PENDING_LINES: usize = 64;

struct Job {
    rope: Rope,
    range: Range<usize>,
    cancelled: Arc<AtomicBool>,
}

struct Pending {
    rope: Rope,
    range: Range<usize>,
    cancelled: Arc<AtomicBool>,
    seen: bool,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    closed: bool,
}

struct Completed {
    cancelled: Arc<AtomicBool>,
    shaped: Option<ShapedLine>,
}

// A CTLine and its runs are used by exactly one thread at a time. The worker
// transfers its sole ownership here, then never accesses that line again.
// CTFont objects are immutable and shareable. No atlas, Rc or UI object crosses
// this channel; the foreground wraps the received geometry in Rc afterwards.
unsafe impl Send for Completed {}

pub(super) struct Worker {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    receiver: mpsc::Receiver<Completed>,
    pending: HashMap<Owner, Pending>,
}

impl Worker {
    pub fn new(font: String, size: f32, scale: f32) -> Self {
        let queue = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
        let (sender, receiver) = mpsc::sync_channel(1);
        let work = queue.clone();
        std::thread::Builder::new()
            .name("text-shaping".into())
            .spawn(move || {
                let font = load_monospace(&font, size).font;
                loop {
                    let job = {
                        let (lock, wake) = &*work;
                        let mut state = lock.lock().unwrap();
                        while !state.closed && state.jobs.is_empty() {
                            state = wake.wait(state).unwrap();
                        }
                        if state.closed {
                            return;
                        }
                        state.jobs.pop_front().unwrap()
                    };
                    if job.cancelled.load(Ordering::Relaxed) {
                        continue;
                    }
                    let source = job.rope.slice_to_string(job.range.clone());
                    let source = source.trim_end_matches(['\r', '\n']);
                    let shaped = shape(&font, source, scale, &job.cancelled);
                    if job.cancelled.load(Ordering::Relaxed) {
                        continue;
                    }
                    if sender
                        .send(Completed {
                            cancelled: job.cancelled,
                            shaped,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            })
            .expect("start text shaping worker");
        Self {
            queue,
            receiver,
            pending: HashMap::new(),
        }
    }

    pub fn begin_frame(&mut self) {
        for pending in self.pending.values_mut() {
            pending.seen = false;
        }
    }

    /// Preserve jobs whose complete paragraph is unchanged, including jobs
    /// already running or waiting in the result channel. Their token remains
    /// stable while the foreground owner, range and snapshot move together.
    pub fn rebase(&mut self, document: u64, mapping: Option<&LineReuse<'_>>) {
        let mut retained = HashMap::new();
        for (owner, mut pending) in self.pending.drain() {
            if owner.0 == document
                && let Some(mapping) = mapping
                && let Some((line, range)) = mapping.map(owner.1)
            {
                pending.rope = mapping.new.clone();
                pending.range = range;
                if let Some(displaced) = retained.insert((document, line), pending) {
                    displaced.cancelled.store(true, Ordering::Relaxed);
                }
            } else {
                pending.cancelled.store(true, Ordering::Relaxed);
            }
        }
        self.pending = retained;
    }

    pub fn request(&mut self, owner: Owner, rope: &Rope, range: Range<usize>) {
        if let Some(pending) = self.pending.get_mut(&owner)
            && pending.rope.same_snapshot(rope)
            && pending.range == range
        {
            pending.seen = true;
            return;
        }
        self.cancel(owner);
        // Never evict another visible line to admit a new one: a very tall
        // viewport would otherwise churn forever. Later lines retry as the
        // current requests finish and leave this bounded queue.
        let bytes: usize = self.pending.values().map(|p| p.range.len()).sum();
        if self.pending.len() >= MAX_PENDING_LINES || bytes + range.len() > MAX_PENDING_BYTES {
            return;
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        self.pending.insert(
            owner,
            Pending {
                rope: rope.clone(),
                range: range.clone(),
                cancelled: cancelled.clone(),
                seen: true,
            },
        );
        let (lock, wake) = &*self.queue;
        let mut queue = lock.lock().unwrap();
        queue
            .jobs
            .retain(|job| !job.cancelled.load(Ordering::Relaxed));
        queue.jobs.push_back(Job {
            rope: rope.clone(),
            range,
            cancelled,
        });
        wake.notify_one();
    }

    pub fn cancel(&mut self, owner: Owner) {
        if let Some(pending) = self.pending.remove(&owner) {
            pending.cancelled.store(true, Ordering::Relaxed);
        }
    }

    pub fn finish_frame(&mut self) {
        self.pending.retain(|_, pending| {
            if !pending.seen {
                pending.cancelled.store(true, Ordering::Relaxed);
            }
            pending.seen
        });
    }

    pub fn poll(&mut self) -> Vec<(Owner, Rope, Option<ShapedLine>)> {
        let mut ready = Vec::new();
        while let Ok(result) = self.receiver.try_recv() {
            if result.cancelled.load(Ordering::Relaxed) {
                continue;
            }
            let owner = self
                .pending
                .iter()
                .find(|(_, pending)| Arc::ptr_eq(&pending.cancelled, &result.cancelled))
                .map(|(owner, _)| *owner);
            let Some(owner) = owner else {
                continue;
            };
            let pending = self.pending.remove(&owner).unwrap();
            ready.push((owner, pending.rope, result.shaped));
        }
        ready
    }

    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        for pending in self.pending.values() {
            pending.cancelled.store(true, Ordering::Relaxed);
        }
        let (lock, wake) = &*self.queue;
        let mut queue = lock.lock().unwrap();
        queue.closed = true;
        queue.jobs.clear();
        wake.notify_one();
        // Dropping the receiver also releases a worker blocked on its bounded
        // result channel. Never join a CoreText operation on the UI thread.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn unchanged_pending_paragraph_moves_without_restarting_and_undo_moves_it_back() {
        let mut worker = Worker::new("Menlo".into(), 26.0, 2.0);
        let old = Rope::from_text(&format!("first\n{}\nlast", "אב 👩‍💻 ".repeat(1200)));
        worker.request((1, 1), &old, old.line_to_byte(1)..old.line_to_byte(2));
        let token = worker.pending[&(1, 1)].cancelled.clone();
        let mut new = old.clone();
        new.insert(0, "another\n");
        worker.rebase(1, Some(&LineReuse::new(&old, &new)));
        worker.begin_frame();
        worker.request((1, 2), &new, new.line_to_byte(2)..new.line_to_byte(3));
        worker.finish_frame();
        assert!(!token.load(Ordering::Relaxed));
        assert!(Arc::ptr_eq(&token, &worker.pending[&(1, 2)].cancelled));
        worker.rebase(1, Some(&LineReuse::new(&new, &old)));
        assert!(Arc::ptr_eq(&token, &worker.pending[&(1, 1)].cancelled));
        let start = Instant::now();
        let ready = loop {
            let ready = worker.poll();
            if !ready.is_empty() {
                break ready;
            }
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(ready[0].0, (1, 1));
        assert!(ready[0].1.same_snapshot(&old));
        assert_eq!(
            *ready[0].2.as_ref().unwrap().source_bytes.last().unwrap(),
            old.line(1).trim_end_matches(['\r', '\n']).len()
        );
    }

    #[test]
    fn rebasing_cancels_changed_paragraphs_and_other_documents() {
        let mut worker = Worker::new("Menlo".into(), 26.0, 2.0);
        let old = Rope::from_text("é old\nאב unchanged");
        worker.request((1, 0), &old, 0..old.line_to_byte(1));
        worker.request((1, 1), &old, old.line_to_byte(1)..old.len_bytes());
        let changed = worker.pending[&(1, 0)].cancelled.clone();
        let retained = worker.pending[&(1, 1)].cancelled.clone();
        let mut new = old.clone();
        new.insert(0, "x");
        worker.rebase(1, Some(&LineReuse::new(&old, &new)));
        assert!(changed.load(Ordering::Relaxed));
        assert!(!retained.load(Ordering::Relaxed));
        worker.rebase(2, None);
        assert!(retained.load(Ordering::Relaxed));
        assert!(!worker.is_pending());
        assert!(worker.poll().is_empty());
    }

    #[test]
    fn buffered_completion_uses_remapped_owner_and_snapshot() {
        // Drive the channel deterministically: the result is already buffered
        // when the edit arrives, rather than relying on native worker timing.
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut worker = Worker {
            queue: Arc::new((Mutex::new(Queue::default()), Condvar::new())),
            receiver,
            pending: HashMap::new(),
        };
        let old = Rope::from_text("header\né unchanged\n");
        worker.request((1, 1), &old, old.line_to_byte(1)..old.line_to_byte(2));
        sender
            .send(Completed {
                cancelled: worker.pending[&(1, 1)].cancelled.clone(),
                shaped: None,
            })
            .unwrap();
        let mut new = old.clone();
        new.insert(0, "inserted\n");
        worker.rebase(1, Some(&LineReuse::new(&old, &new)));
        let ready = worker.poll();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].0, (1, 2));
        assert!(ready[0].1.same_snapshot(&new));
        assert!(!worker.is_pending());
    }

    #[test]
    fn duplicate_pending_paragraph_remaps_cancel_the_displaced_job() {
        let mut worker = Worker::new("Menlo".into(), 26.0, 2.0);
        let old = Rope::from_text("é\né\n");
        worker.request((1, 0), &old, 0..3);
        worker.request((1, 1), &old, 3..6);
        let a = worker.pending[&(1, 0)].cancelled.clone();
        let b = worker.pending[&(1, 1)].cancelled.clone();
        let mut new = old.clone();
        new.delete(0..3);
        worker.rebase(1, Some(&LineReuse::new(&old, &new)));
        assert_eq!(worker.pending.len(), 1);
        assert_ne!(a.load(Ordering::Relaxed), b.load(Ordering::Relaxed));
        let kept = &worker.pending[&(1, 0)];
        assert!(kept.rope.same_snapshot(&new));
        assert!(!kept.cancelled.load(Ordering::Relaxed));
    }

    #[test]
    fn edits_cancel_old_work_and_only_latest_content_completes() {
        let mut worker = Worker::new("Menlo".into(), 26.0, 2.0);
        let owner = (1, 0);
        worker.begin_frame();
        let old_rope = Rope::from_text(&"אב 👩‍💻 ".repeat(1200));
        worker.request(owner, &old_rope, 0..old_rope.len_bytes());
        let old = worker.pending[&owner].cancelled.clone();
        let latest = Rope::from_text("é latest");
        worker.request(owner, &latest, 0..latest.len_bytes());
        assert!(old.load(Ordering::Relaxed));
        worker.finish_frame();
        let start = Instant::now();
        let mut ready = Vec::new();
        while worker.is_pending() {
            ready.extend(worker.poll());
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].0, owner);
        assert!(ready[0].1.same_snapshot(&latest));
        assert_eq!(ready[0].2.as_ref().unwrap().offsets.len(), 9);
    }

    #[test]
    fn leaving_a_document_cancels_its_pending_lines() {
        let mut worker = Worker::new("Menlo".into(), 26.0, 2.0);
        let old = Rope::from_text("é old document");
        worker.request((1, 0), &old, 0..old.len_bytes());
        let old = worker.pending[&(1, 0)].cancelled.clone();
        worker.begin_frame();
        let new = Rope::from_text("é new document");
        worker.request((2, 0), &new, 0..new.len_bytes());
        worker.finish_frame();
        assert!(old.load(Ordering::Relaxed));
        assert_eq!(worker.pending.len(), 1);
        assert!(worker.pending.contains_key(&(2, 0)));
    }

    #[test]
    fn queued_lines_and_source_bytes_are_bounded() {
        let mut worker = Worker::new("Menlo".into(), 26.0, 2.0);
        let text = Rope::from_text(&"a".repeat(256 * 1024));
        for line in 0..128 {
            worker.request((1, line), &text, 0..text.len_bytes());
        }
        assert!(worker.pending.len() <= MAX_PENDING_LINES);
        assert!(
            worker
                .pending
                .values()
                .map(|p| p.range.len())
                .sum::<usize>()
                <= MAX_PENDING_BYTES
        );
    }
}
