//! The Explorer's file operations: the one way the window changes the
//! project's files, and copy, cut, paste and duplicate on top of it.
//!
//! Copied and cut items go on the clipboard as files, so they paste in the
//! Finder and the Finder's paste here. A cut changes nothing until it is
//! pasted: its rows are dimmed meanwhile. A paste that moves is done at
//! once, with the tabs and the local history of what moved; one that
//! copies runs on a worker, so a large folder does not hold the window,
//! and Escape in the tree stops it. Nothing is ever replaced. What a drag
//! in the tree drops is moved or copied the same way.
//!
//! Files dragged in from the Finder or another app and dropped on the tree
//! are copied into the folder under the pointer, never moved. Where names
//! they bring are in use there the drop asks, once for all of them:
//! Replace, which moves each item that is there to the Trash before the
//! copy takes its name, Keep Both, or Cancel.
//!
//! What was done can be taken back: Undo while the tree has the keyboard
//! undoes the last thing done to the files, all of it at once (a paste of
//! five items is one step), and Redo does it again. Nothing is deleted or
//! written over on the way; see `fileops.rs`.

use std::io;
use std::path::PathBuf;

use super::*;
use crate::project::fileops::{
    self, DropCheck, Env, OnTaken, Op, OpKind, Planned, Record, Refusal, Reversal, Skip, Skipped,
    Steps, Undone,
};

/// Under the self-test, a file a copy waits for before each item it
/// copies: a script holds a copy where it is, looks, and lets it go.
const COPY_GATE: &str = "CRC_COPY_GATE";

/// Under the self-test, the answers to the question a drop from outside
/// asks, in order and comma separated: `replace`, `keep-both` or `cancel`.
/// Past the last of them the instance only says what it would ask.
const DROP_ANSWER: &str = "CRC_DROP_ANSWER";

/// How many of those questions a test instance has been asked.
static DROPS_ASKED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The Explorer's file work: the copy running, the cut waiting for its
/// paste, and what has been done.
#[derive(Default)]
pub(super) struct Files {
    /// The copy running on a worker, if one is.
    job: Option<Job>,
    /// The clipboard's change count with the cut items on it. Once it has
    /// moved, something else was copied and the cut is over.
    cut_change: Option<isize>,
    /// What a paste or a duplicate made, to select when the tree has been
    /// read again and has rows for it.
    made: Vec<PathBuf>,
    /// The clipboard's change count when `made` was pasted from it;
    /// `None` when it was moved or duplicated.
    made_from: Option<isize>,
    /// What the last paste made, for as long as a paste straight after it
    /// goes beside that and not into it.
    pasted: Option<Pasted>,
    /// The operations done, as [`fileops::perform`] answered them, a step
    /// for each thing the user did, and the steps taken back: what Undo
    /// and Redo go by.
    steps: Steps,
    /// A drop from outside that found names in use, waiting for its
    /// question to be asked once the drag that brought it is over.
    asking: Option<DropCheck>,
    /// What a scripted drag from outside was last answered, for a `dump`.
    scripted_drag: &'static str,
}

/// What the last paste made and left selected. While the same content is
/// on the clipboard and nobody has put the selection anywhere else, a
/// paste on one of these is another copy of the same thing, and belongs
/// beside the first.
struct Pasted {
    /// The clipboard's change count when it was pasted.
    change: isize,
    /// The tree's selection count once the new items were selected.
    selection: u64,
    paths: Vec<PathBuf>,
}

/// Where a paste on the row of the folder `dir` goes: into it, unless it
/// is what the `last` paste of this same clipboard (`change`) made and the
/// selection (`selection`) is still the one that paste left. Then it goes
/// beside it, so that Cmd-C, Cmd-V, Cmd-V on a folder gives two copies
/// side by side and not one inside the other. A cursor moved to the copy
/// on purpose pastes into it.
fn paste_target(dir: PathBuf, last: Option<&Pasted>, change: isize, selection: u64) -> PathBuf {
    let again = last.is_some_and(|last| {
        last.change == change && last.selection == selection && last.paths.contains(&dir)
    });
    match dir.parent() {
        Some(parent) if again => parent.to_path_buf(),
        _ => dir,
    }
}

impl Files {
    /// No copy is running that the display link has to look in on.
    pub(super) fn idle(&self) -> bool {
        self.job.is_none()
    }

    /// A new step, for the operations of one thing the user does: they
    /// are undone together.
    pub(super) fn begin(&mut self) -> u64 {
        self.steps.begin()
    }

    /// Forgets what was done and what was taken back: the project is
    /// another folder now, and those files are not its own.
    pub(super) fn forget_steps(&mut self) {
        self.steps.clear();
    }
}

/// A paste or a duplicate being copied on a worker.
struct Job {
    /// Each item as it ends: where it was, and what came of it.
    rx: mpsc::Receiver<(PathBuf, io::Result<Record>)>,
    /// Set to stop the copy: by Escape in the tree, or on the way out.
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// How many items it was given.
    total: usize,
    /// The step its copies are kept under: one undo for all of them.
    step: u64,
    verb: Verb,
    /// A drop: the folder it went to, as the outcome names it.
    to: Option<String>,
    /// A paste: the clipboard's change count when it was asked for.
    from: Option<isize>,
    /// What the status line says while it runs.
    said: String,
    made: Vec<PathBuf>,
    /// How many of them took the place of an item, now in the Trash.
    replaced: usize,
    /// The items that could not be copied, each with why.
    failed: Vec<String>,
    /// What the plan refused before anything was copied.
    refused: Vec<String>,
}

/// What an operation came to.
pub(super) struct Did {
    /// Where the result is; `None` for an item moved to the Trash.
    pub(super) path: Option<PathBuf>,
    /// Why the original of a move to another volume is still there.
    pub(super) original_left: Option<String>,
}

/// Every change the window makes to the project's files, on the main
/// thread: a new file or folder, a rename, a move, a duplicate, a move to
/// the Trash. What was done is kept in `files`, as one of the operations
/// of `step`. With `free`, a move or a copy whose name was taken since it
/// was chosen takes the next free one instead of failing. The copies of a
/// paste go through the same [`fileops::perform`] on their worker.
pub(super) fn file_op(files: &mut Files, step: u64, op: Op<'_>, free: bool) -> io::Result<Did> {
    let (did, record) = perform(op, free)?;
    files.steps.keep(step, record);
    Ok(did)
}

/// [`file_op`] without the keeping: what it came to, and its record.
fn perform(op: Op<'_>, free: bool) -> io::Result<(Did, Record)> {
    let env = Env {
        trash: &|path| project::trash_item(path).map_err(io::Error::other),
        stop: &|| false,
    };
    let record = if free {
        fileops::perform_free(op, &env)
    } else {
        fileops::perform(op, &env)
    }?;
    let did = Did {
        path: record.result.clone(),
        original_left: record.original_left.as_ref().map(ToString::to_string),
    };
    Ok((did, record))
}

impl EditorView {
    /// [`file_op`] for this window, as a step of its own. The state is
    /// not held while the disk is worked on: moving to the Trash is the
    /// system's, and may take a moment.
    pub(super) fn file_op(&self, op: Op<'_>) -> io::Result<Did> {
        let step = self.begin_file_step();
        self.file_op_in(step, op)
    }

    /// [`EditorView::file_op`], as one of the operations of `step`.
    pub(super) fn file_op_in(&self, step: u64, op: Op<'_>) -> io::Result<Did> {
        let (did, record) = perform(op, false)?;
        if let Some(mut state) = self.state_mut() {
            state.explorer.steps.keep(step, record);
        }
        Ok(did)
    }

    /// A new step, for the operations of one thing the user does: one
    /// undo takes all of them back.
    pub(super) fn begin_file_step(&self) -> u64 {
        self.state_mut()
            .map_or(0, |mut state| state.explorer.begin())
    }

    /// Copy and Cut while the tree has the keyboard: the selected items go
    /// on the clipboard as files, a folder taking what is in it. A cut is
    /// only marked: nothing moves until it is pasted. `false` when the
    /// tree does not have the keys and the command is someone else's.
    pub(super) fn copy_tree_items(&self, cut: bool) -> bool {
        {
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            if !sidebar::tree_has_keys(&state) {
                return false;
            }
            let paths = state.tree.selected_roots();
            if !paths.is_empty() {
                let change = clipboard::write_files(&paths, cut);
                let said = match (cut, paths.as_slice()) {
                    (false, [path]) => format!("copied {}", sidebar::item_name(path)),
                    (false, _) => format!("copied {} items", paths.len()),
                    (true, [path]) => {
                        format!("cut {}: paste to move it", sidebar::item_name(path))
                    }
                    (true, _) => format!("cut {} items: paste to move them", paths.len()),
                };
                state.explorer.cut_change = cut.then_some(change);
                state.tree.set_cut(if cut { paths } else { Vec::new() });
                state.say(layout::Feedback::Info, said);
            }
        }
        self.request_redraw();
        self.pump();
        true
    }

    /// Paste while the tree has the keyboard: the files on the clipboard
    /// go into the cursor's folder (a folder's row is that folder, a
    /// file's row the folder it is in, no selection the project root; on
    /// the row of an item being pasted, or of what the paste before this
    /// one just made, beside it). They are copied, or moved when this
    /// window cut them. `false` when the tree does not have the keys.
    pub(super) fn paste_tree_items(&self) -> bool {
        let dir = {
            let Some(state) = self.state() else {
                return false;
            };
            if !sidebar::tree_has_keys(&state) {
                return false;
            }
            state.tree.target_dir().map(|dir| {
                paste_target(
                    dir,
                    state.explorer.pasted.as_ref(),
                    clipboard::change_count(),
                    state.tree.selection_changes(),
                )
            })
        };
        let Some(dir) = dir else {
            return true;
        };
        let files = clipboard::read_files();
        if files.paths.is_empty() {
            self.say_about_files(
                layout::Feedback::Info,
                "nothing to paste: the clipboard holds no files",
            );
            return true;
        }
        let plan = fileops::plan_paste(&files.paths, &dir, files.cut);
        self.carry_out(plan, if files.cut { Verb::Move } else { Verb::Paste });
        true
    }

    /// Duplicate: a copy beside each selected item, a folder with what is
    /// in it, under the next free `-copy` name.
    pub(super) fn duplicate_tree_items(&self) {
        let Some(paths) = self.state().map(|state| {
            if sidebar::tree_has_keys(&state) {
                state.tree.selected_roots()
            } else {
                Vec::new()
            }
        }) else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        self.carry_out(fileops::plan_duplicate(&paths), Verb::Duplicate);
    }

    /// What a drag in the tree dropped on the folder `into`: moved there as
    /// `plan` says, or with `copy` copied. A move by drag takes no other
    /// name than its own, so an item whose name is in use there is skipped
    /// and the rest still go.
    pub(super) fn drop_items(&self, plan: Vec<Planned>, copy: bool, into: &Path) {
        let verb = if copy { Verb::DragCopy } else { Verb::Drag };
        let mut outcome = Outcome::new(verb);
        outcome.to = Some(sidebar::item_name(into));
        self.carry_out_as(plan, outcome);
    }

    /// Does what `plan` says. The moves are done here and now, with the
    /// tabs and the local history of what moves; the copies are handed to
    /// a worker.
    fn carry_out(&self, plan: Vec<Planned>, verb: Verb) {
        self.carry_out_as(plan, Outcome::new(verb));
    }

    /// [`EditorView::carry_out`], into an outcome already begun.
    fn carry_out_as(&self, plan: Vec<Planned>, mut outcome: Outcome) {
        let verb = outcome.verb;
        if self.copy_in_the_way() {
            return;
        }
        let mut copies = Vec::new();
        let mut moves = Vec::new();
        for planned in plan {
            match planned {
                Planned::Copy { from, to } => copies.push((from, to, false)),
                Planned::Replace { from, to } => copies.push((from, to, true)),
                Planned::Move { from, to } => moves.push((from, to)),
                // Dropped where it is: nothing to do, and nothing to say.
                Planned::Stays(_) if verb.dragged() => {}
                Planned::Stays(from) => outcome.stays.push(sidebar::item_name(&from)),
                Planned::Refused {
                    from,
                    why: Refusal::Taken,
                } => outcome.skipped.push(sidebar::item_name(&from)),
                Planned::Refused { from, why } => outcome.refused.push(format!(
                    "{} {}",
                    sidebar::item_name(&from),
                    verb.refusal(why)
                )),
            }
        }
        outcome.total = copies.len() + moves.len();
        // Dropped where everything already is: nothing was done.
        if verb.dragged() && outcome.total == 0 && outcome.nothing_to_say() {
            return;
        }
        // Unsaved work on an item that is to be replaced, or under it,
        // would be left on a file that went to the Trash.
        let replacing = copies.iter().filter(|(_, _, replace)| *replace).count();
        let unsaved = replacing > 0
            && self.state().is_some_and(|state| {
                copies.iter().any(|(_, to, replace)| {
                    *replace && sidebar::dirty_tabs_under(&state, &crate::platform::canonical(to))
                })
            });
        if unsaved {
            self.say_about_files(
                layout::Feedback::Failure,
                if replacing == 1 {
                    "save the affected tabs before replacing this item"
                } else {
                    "save the affected tabs before replacing these items"
                },
            );
            return;
        }
        if !copies.is_empty() {
            self.start_copy(copies, outcome);
            return;
        }
        // Unsaved work under an item would be stranded at a path that no
        // longer exists: the rule a drag in the tree has. Nothing moves
        // then, and the cut waits.
        let dirty = self.state().is_some_and(|state| {
            moves.iter().any(|(from, _)| {
                sidebar::dirty_tabs_under(&state, &crate::platform::canonical(from))
            })
        });
        if dirty {
            self.say_about_files(
                layout::Feedback::Failure,
                if moves.len() == 1 {
                    "save the affected tabs before moving this item"
                } else {
                    "save the affected tabs before moving these items"
                },
            );
            return;
        }
        // One step, however many items move: one undo takes them all back.
        let step = self.begin_file_step();
        for (from, to) in &moves {
            // A paste's move takes the next free name when its own was
            // taken since the plan; a drag's is skipped.
            match self.relocate(Op::Move { from, to }, !verb.dragged(), step) {
                Err(error) if verb.dragged() && error.kind() == io::ErrorKind::AlreadyExists => {
                    outcome.skipped.push(sidebar::item_name(from))
                }
                Ok(did) => {
                    if let Some(why) = did.original_left {
                        outcome.failed.push(format!(
                            "{}: copied, and the original could not be removed: {why}",
                            sidebar::item_name(from)
                        ));
                    }
                    outcome.made.extend(did.path);
                }
                Err(error) => outcome
                    .failed
                    .push(format!("{}: {error}", sidebar::item_name(from))),
            }
        }
        let moved = !outcome.made.is_empty();
        if let Some(mut state) = self.state_mut() {
            if moved && verb == Verb::Move {
                // The cut is over, and what is on the clipboard is gone
                // from where it says: a second paste has nothing to move.
                if state.explorer.cut_change == Some(clipboard::change_count()) {
                    clipboard::clear();
                }
                forget_cut(&mut state);
            }
            let (kind, said) = outcome.said();
            state.explorer.made = outcome.made;
            state.explorer.made_from = None;
            state.say(kind, said);
        }
        if moved {
            self.refresh_project_after_disk_change();
            self.sync_title();
            self.reparse();
        }
        self.request_redraw();
        self.pump();
    }

    /// Hands the copies of a paste, a duplicate or a drop to a worker and
    /// says so in the status line. The display link takes what comes back.
    /// Each is where it is from, where it goes, and whether it goes in
    /// place of the item that is there.
    fn start_copy(&self, copies: Vec<(PathBuf, PathBuf, bool)>, outcome: Outcome) {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let total = copies.len();
        let verb = outcome.verb;
        let duplicate = verb == Verb::Duplicate;
        let from = (verb == Verb::Paste).then(clipboard::change_count);
        let said = match total {
            1 => "Copying 1 item\u{2026}".to_string(),
            n => format!("Copying {n} items\u{2026}"),
        };
        let gate = std::env::var_os(COPY_GATE)
            .filter(|_| self.ivars().testing)
            .map(PathBuf::from);
        let worker = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let halted = || {
                    if let Some(gate) = &gate {
                        while !gate.exists() && !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                    stop.load(Ordering::Relaxed)
                };
                let env = Env {
                    // Only a copy told to replace an item trashes one.
                    trash: &|path| project::trash_item(path).map_err(io::Error::other),
                    stop: &halted,
                };
                for (from, to, replace) in copies {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let (from, to) = (from.as_path(), to.as_path());
                    let op = if replace {
                        Op::Replace { from, to }
                    } else if duplicate {
                        Op::Duplicate { from, to }
                    } else {
                        Op::Copy { from, to }
                    };
                    let result = fileops::perform_free(op, &env);
                    if tx.send((from.to_path_buf(), result)).is_err() {
                        break;
                    }
                }
            })
        };
        if let Some(mut state) = self.state_mut() {
            state.say(layout::Feedback::Info, said.clone());
            let step = state.explorer.begin();
            state.explorer.job = Some(Job {
                rx,
                stop,
                worker: Some(worker),
                total,
                step,
                verb,
                to: outcome.to,
                from,
                said,
                made: Vec::new(),
                replaced: 0,
                failed: Vec::new(),
                refused: outcome.refused,
            });
        } else {
            // Nowhere to keep it: stopped, and joined so nothing is left.
            stop.store(true, Ordering::Relaxed);
            let _ = worker.join();
        }
        self.request_redraw();
        self.resume_display_link();
        self.pump();
    }

    /// Takes what the copy on the worker has finished. While it runs the
    /// status line keeps saying so; when it is over the outcome is said,
    /// the tree is read again, once, and the new items are selected.
    pub(super) fn poll_file_job(&self) {
        let (kind, said, replaced) = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let state = &mut *state;
            let Files { job, steps, .. } = &mut state.explorer;
            let Some(running) = job else {
                return;
            };
            let over = loop {
                match running.rx.try_recv() {
                    Ok((_, Ok(record))) => {
                        running.made.extend(record.result.clone());
                        running.replaced += usize::from(record.kind == OpKind::Replace);
                        steps.keep(running.step, record);
                    }
                    // Stopped: neither made nor failed.
                    Ok((_, Err(error))) if error.kind() == io::ErrorKind::Interrupted => {}
                    Ok((from, Err(error))) => running
                        .failed
                        .push(format!("{}: {error}", sidebar::item_name(&from))),
                    Err(mpsc::TryRecvError::Empty) => break false,
                    Err(mpsc::TryRecvError::Disconnected) => break true,
                }
            };
            if !over {
                // Said for as long as it runs, under whatever else is said
                // meanwhile.
                let ours = state
                    .message
                    .as_ref()
                    .is_none_or(|(text, _)| *text == running.said);
                if ours {
                    state.message = Some((running.said.clone(), Instant::now()));
                }
                return;
            }
            let Some(mut job) = job.take() else {
                return;
            };
            if let Some(worker) = job.worker.take() {
                let _ = worker.join();
            }
            let outcome = Outcome {
                total: job.total,
                cancelled: job.stop.load(Ordering::Relaxed),
                made: job.made,
                replaced: job.replaced,
                failed: job.failed,
                refused: job.refused,
                to: job.to,
                ..Outcome::new(job.verb)
            };
            let (kind, said) = outcome.said();
            state.explorer.made = outcome.made;
            state.explorer.made_from = job.from;
            (kind, said, outcome.replaced > 0)
        };
        // A tab open on a file that was replaced shows what is there now,
        // before the outcome is said: that it was read again is the
        // smaller news, and would be said over it a moment later.
        if replaced {
            self.check_open_files();
        }
        if let Some(mut state) = self.state_mut() {
            state.say(kind, said);
        }
        self.refresh_project_after_disk_change();
        self.request_redraw();
        self.pump();
    }

    /// Stops the copy that is running, and waits for it to have stopped:
    /// on the way out, so that no item is left half copied and no hidden
    /// folder is left behind. What was copied whole stays.
    pub(super) fn finish_file_job(&self) {
        let job = self
            .state_mut()
            .and_then(|mut state| state.explorer.job.take());
        if let Some(mut job) = job {
            job.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = job.worker.take() {
                let _ = worker.join();
            }
        }
    }

    /// Undo, or with `redo` Redo, while the tree has the keyboard: the
    /// last thing done to the files is taken back, every item of it, or
    /// the last one taken back is done again. An item that is no longer
    /// as the operation left it, or whose place is taken, is skipped and
    /// said so, and the rest still go. Nothing moves while a tab on an
    /// item that would move or go to the Trash holds unsaved work, or
    /// while a copy is running. What came back is the selection
    /// afterwards. `false` when the tree does not have the keys and the
    /// command is the document's.
    pub(super) fn turn_file_step(&self, redo: bool) -> bool {
        let turn = {
            let Some(mut state) = self.state_mut() else {
                return false;
            };
            if !sidebar::tree_has_keys(&state) {
                return false;
            }
            if state.explorer.idle() {
                let State {
                    explorer,
                    docs,
                    panes,
                    ..
                } = &mut *state;
                let dirty_under = |path: &Path| {
                    std::iter::once(&*docs)
                        .chain(panes.iter().map(|pane| &pane.docs))
                        .any(|docs| docs.has_dirty_under(path))
                };
                Some(next_turn(explorer, redo, &dirty_under))
            } else {
                None
            }
        };
        let (word, doing) = if redo {
            ("redo", "redoing")
        } else {
            ("undo", "undoing")
        };
        let turn = match turn {
            None => {
                self.copy_in_the_way();
                return true;
            }
            Some(Turn::Nothing) => {
                self.say_about_files(layout::Feedback::Info, &format!("nothing to {word}"));
                return true;
            }
            Some(Turn::Unsaved) => {
                self.say_about_files(
                    layout::Feedback::Failure,
                    &format!("save the affected tabs before {doing} this"),
                );
                return true;
            }
            Some(turn) => turn,
        };
        let trash = |path: &Path| project::trash_item(path).map_err(io::Error::other);
        let mut landed = Vec::new();
        let mut exchanged = false;
        let mut apply = |reversal: &Reversal| -> Result<Option<PathBuf>, Skip> {
            // A tab is kept by where its file really is, which can only
            // be asked while the item is still there.
            let key = reversal.leaves().map(crate::platform::canonical);
            let aside = reversal.apply(&trash)?;
            self.follow_reversal(reversal, key.as_deref());
            exchanged |= matches!(reversal, Reversal::Exchange { .. });
            landed.extend(reversal.lands().map(Path::to_path_buf));
            Ok(aside)
        };
        // Neither is a new step: an undo leaves what it took back to be
        // done again, and a redo puts it back where an undo finds it.
        let (label, done, skipped) = match turn {
            Turn::Undo(records) => {
                let label = Label::of(records.iter());
                let (undone, skipped) = fileops::undo_step(records, &mut apply);
                let done = undone.len();
                if let Some(mut state) = self.state_mut() {
                    state.explorer.steps.took_back(undone);
                }
                (label, done, skipped)
            }
            Turn::Redo(undone) => {
                let label = Label::of(undone.iter().map(|undone| &undone.record));
                let (records, skipped) = fileops::redo_step(undone, &mut apply);
                let done = records.len();
                if let Some(mut state) = self.state_mut() {
                    state.explorer.steps.did_again(records);
                }
                (label, done, skipped)
            }
            Turn::Nothing | Turn::Unsaved => return true,
        };
        // A tab on a file that changed places with the one it replaced
        // shows what is there now, before the outcome is said.
        if exchanged {
            self.check_open_files();
        }
        if let Some(mut state) = self.state_mut() {
            reveal_active_tab(&mut state);
            if let Some(label) = &label {
                let (kind, said) = turned_said(redo, label, done, &skipped);
                state.say(kind, said);
            }
            // Selected once the tree has been read again and has rows
            // for them.
            state.explorer.made = landed;
            state.explorer.made_from = None;
        }
        self.refresh_project_after_disk_change();
        self.sync_title();
        self.reparse();
        self.request_redraw();
        self.pump();
        true
    }

    /// What goes with an item `reversal` has moved from `key`, where a
    /// tab open on it, or under it, has it. Moved back, its tabs in every
    /// pane and its local history follow it. Gone to the Trash, its tabs
    /// close: none of them holds unsaved work, or nothing had moved.
    fn follow_reversal(&self, reversal: &Reversal, key: Option<&Path>) {
        let (Some(key), Some(mut state)) = (key, self.state_mut()) else {
            return;
        };
        match reversal {
            Reversal::Move { to, .. } => {
                let State {
                    docs,
                    panes,
                    history,
                    ..
                } = &mut *state;
                let open = std::iter::once(docs).chain(panes.iter_mut().map(|pane| &mut pane.docs));
                project::follow_moved(open, history, key, to);
            }
            Reversal::Trash(_) => {
                for docs in all_docs_mut(&mut state) {
                    docs.close_under(key);
                }
            }
            Reversal::PutBack { .. } | Reversal::Exchange { .. } => {}
        }
    }

    /// Under the self-test, two lines for a `dump`: what Edit > Undo and
    /// Redo are called and whether they can be chosen (`(off)` after one
    /// that cannot), asked of the window as the menu asks before it shows
    /// them.
    pub(super) fn edit_menu_report(&self) -> String {
        let mtm = MainThreadMarker::from(self);
        let asked = |title: &str, action: Sel| {
            let item = menu_item(mtm, title, action);
            let enabled = self
                .validate_menu_item(sel!(validateMenuItem:), &item)
                .as_bool();
            format!("{}{}", item.title(), if enabled { "" } else { " (off)" })
        };
        format!(
            "edit_undo: {}\nedit_redo: {}\n",
            asked("Undo", sel!(undo:)),
            asked("Redo", sel!(redo:))
        )
    }

    /// Files dragged in from outside, let go over the folder `check` is
    /// aimed at: copied there, after the one question when names they
    /// bring are in use. The question waits for the drag to be over: the
    /// app the files came from is held up until the drop is answered.
    /// Whether the drop is taken.
    pub(super) fn drop_from_outside(&self, check: DropCheck) -> bool {
        // Before the question, which would be asked for nothing.
        if check.dir().is_none() || self.copy_in_the_way() {
            return false;
        }
        if check.taken_there().is_empty() {
            return self.bring_in(&check, OnTaken::KeepBoth);
        }
        if let Some(mut state) = self.state_mut() {
            state.explorer.asking = Some(check);
        }
        let _: () = unsafe {
            msg_send![self, performSelector: sel!(askAboutDrop:),
                withObject: None::<&AnyObject>, afterDelay: 0.0f64]
        };
        true
    }

    /// Asks what the drop that is waiting does about the names in use, and
    /// does it: one question for the whole drop. Cancel copies nothing.
    pub(super) fn ask_about_drop(&self) {
        let Some(check) = self
            .state_mut()
            .and_then(|mut state| state.explorer.asking.take())
        else {
            return;
        };
        let (question, detail) = {
            // Looked at again: the folder may have changed since the drop.
            let taken = check.taken_there();
            if taken.is_empty() {
                self.bring_in(&check, OnTaken::KeepBoth);
                return;
            }
            let folder = check.dir().map(sidebar::item_name).unwrap_or_default();
            taken_question(&taken, &folder)
        };
        let answer = if self.ivars().testing {
            // A test instance shows no alert, which would wait for a
            // person: it takes its answers from the environment, and past
            // the last of them says what it would ask.
            let asked = DROPS_ASKED.fetch_add(1, Ordering::SeqCst);
            match scripted_drop_answer(&std::env::var(DROP_ANSWER).unwrap_or_default(), asked) {
                Some(answer) => answer,
                None => {
                    self.say_about_files(layout::Feedback::Info, &format!("would ask: {question}"));
                    return;
                }
            }
        } else {
            let mtm = MainThreadMarker::from(self);
            // The files came from another app, which is the one in front:
            // a question asked from behind it would not be seen.
            NSApplication::sharedApplication(mtm).activate();
            match ask(mtm, &question, detail, &["Replace", "Keep Both", "Cancel"]) {
                0 => Some(OnTaken::Replace),
                1 => Some(OnTaken::KeepBoth),
                _ => None,
            }
        };
        if let Some(on_taken) = answer {
            self.bring_in(&check, on_taken);
        }
    }

    /// Copies what a drop from outside brought into the folder it was let
    /// go over, the names in use dealt with as `on_taken` says. Whether
    /// anything is copied.
    fn bring_in(&self, check: &DropCheck, on_taken: OnTaken) -> bool {
        let Some(into) = check.dir() else {
            return false;
        };
        let plan = check.plan_from_outside(on_taken);
        let acts = acts(&plan);
        if acts && let Some(mut state) = self.state_mut() {
            sidebar::take_tree_keys(&mut state);
        }
        self.drop_items(plan, true, into);
        self.request_redraw();
        self.pump();
        acts
    }

    /// Under the self-test: what the window answered a scripted drag from
    /// outside, kept for the next `dump`.
    pub(super) fn scripted_drag_answered(&self, answered: &'static str) {
        if let Some(mut state) = self.state_mut() {
            state.explorer.scripted_drag = answered;
        }
    }

    /// Whether a copy is running, which the next one has to wait for: said
    /// when it is.
    fn copy_in_the_way(&self) -> bool {
        let running = self.state().is_some_and(|state| !state.explorer.idle());
        if running {
            self.say_about_files(
                layout::Feedback::Info,
                "a copy is still running: Escape in the Explorer stops it",
            );
        }
        running
    }

    fn say_about_files(&self, kind: layout::Feedback, text: &str) {
        if let Some(mut state) = self.state_mut() {
            state.say(kind, text);
        }
        self.request_redraw();
        self.pump();
    }
}

/// Whether `plan` copies or moves anything. The tree takes the keyboard
/// after a drop that does: what landed is selected there, and Escape stops
/// a copy that runs.
pub(super) fn acts(plan: &[Planned]) -> bool {
    plan.iter().any(|planned| {
        matches!(
            planned,
            Planned::Copy { .. } | Planned::Replace { .. } | Planned::Move { .. }
        )
    })
}

/// The step an undo or a redo is about to turn.
enum Turn {
    /// Nothing has been done, or nothing taken back.
    Nothing,
    /// A tab on an item that would move or go to the Trash holds unsaved
    /// work: the step stays where it is.
    Unsaved,
    Undo(Vec<Record>),
    Redo(Vec<Undone>),
}

/// Takes the step an undo would take back off `files`, or with `redo` the
/// one a redo would do again. It stays there when `dirty_under` says that
/// a tab on an item it would move or send to the Trash, or under one,
/// holds unsaved work: that would be left on a file that is gone. Only
/// the items that would go are asked about, not the ones to be skipped.
fn next_turn(files: &mut Files, redo: bool, dirty_under: &dyn Fn(&Path) -> bool) -> Turn {
    let plans: Vec<Reversal> = if redo {
        let Some(undone) = files.steps.last_undone() else {
            return Turn::Nothing;
        };
        undone.iter().filter_map(|one| one.redo().ok()).collect()
    } else {
        let Some(records) = files.steps.last() else {
            return Turn::Nothing;
        };
        records.iter().filter_map(|one| one.undo().ok()).collect()
    };
    let unsaved = plans
        .iter()
        .filter_map(Reversal::leaves)
        .any(|path| dirty_under(&crate::platform::canonical(path)));
    if unsaved {
        Turn::Unsaved
    } else if redo {
        files.steps.take_undone().map_or(Turn::Nothing, Turn::Redo)
    } else {
        files.steps.take_last().map_or(Turn::Nothing, Turn::Undo)
    }
}

/// What a step is called, in the Edit menu and when it has been taken
/// back or done again: what was done, and to which item or to how many.
#[derive(Debug, PartialEq, Eq)]
struct Label {
    kind: OpKind,
    count: usize,
    /// The first item's name: the only one's, where it is named.
    name: String,
}

impl Label {
    /// The label of the step `records` are the operations of; `None` for
    /// a step of none.
    fn of<'a>(records: impl Iterator<Item = &'a Record>) -> Option<Label> {
        let mut label: Option<Label> = None;
        for record in records {
            match &mut label {
                None => {
                    label = Some(Label {
                        kind: record.kind,
                        count: 1,
                        name: sidebar::item_name(record.shown()),
                    })
                }
                Some(label) => {
                    // A drop that replaced some items and only copied
                    // the others is a copy.
                    if label.kind != record.kind {
                        label.kind = OpKind::Copy;
                    }
                    label.count += 1;
                }
            }
        }
        label
    }

    /// As the menu says it after Undo or Redo: `Move of 3 Items`,
    /// `Rename`, `Copy of bread.md`.
    fn title(&self) -> String {
        let what = match self.kind {
            OpKind::NewFile => return "New File".to_string(),
            OpKind::NewFolder => return "New Folder".to_string(),
            OpKind::Rename => return "Rename".to_string(),
            OpKind::Move => "Move",
            OpKind::Copy => "Copy",
            OpKind::Duplicate => "Duplicate",
            OpKind::Replace => "Replace",
            OpKind::Trash => "Move to Trash",
        };
        match self.count {
            1 => format!("{what} of {}", self.name),
            count => format!("{what} of {count} Items"),
        }
    }

    /// As a sentence says it, when `done` of its items went through:
    /// `move of 3 items`, `move of 2 of 3 items`, `rename to stew.md`.
    fn phrase(&self, done: usize) -> String {
        let name = &self.name;
        let what = match self.kind {
            OpKind::NewFile => return format!("new file {name}"),
            OpKind::NewFolder => return format!("new folder {name}"),
            OpKind::Rename => return format!("rename to {name}"),
            OpKind::Move => "move",
            OpKind::Copy => "copy",
            OpKind::Duplicate => "duplicate",
            OpKind::Replace => "replace",
            OpKind::Trash => "move to Trash",
        };
        match self.count {
            1 => format!("{what} of {name}"),
            count if done < count => format!("{what} of {done} of {count} items"),
            count => format!("{what} of {count} items"),
        }
    }
}

/// What Edit > Undo, or with `redo` Redo, is called and whether it can be
/// chosen while the tree has the keyboard: named after the step it would
/// turn, and off with none. `None` when the tree does not have the keys
/// and the item is the document's.
pub(super) fn edit_item(state: &State, redo: bool) -> Option<(String, bool)> {
    sidebar::tree_has_keys(state).then(|| edit_title(&state.explorer, redo))
}

/// [`edit_item`], of the steps in `files`.
fn edit_title(files: &Files, redo: bool) -> (String, bool) {
    let word = if redo { "Redo" } else { "Undo" };
    let label = if redo {
        files
            .steps
            .last_undone()
            .and_then(|undone| Label::of(undone.iter().map(|undone| &undone.record)))
    } else {
        files
            .steps
            .last()
            .and_then(|records| Label::of(records.iter()))
    };
    match label {
        Some(label) => (format!("{word} {}", label.title()), true),
        None => (word.to_string(), false),
    }
}

/// What is said when a step has been taken back, or with `redo` done
/// again: what it was, how much of it went through (`done` items), and
/// every item that was skipped, with why. A failure when any was.
fn turned_said(
    redo: bool,
    label: &Label,
    done: usize,
    skipped: &[Skipped],
) -> (layout::Feedback, String) {
    let (did, doing) = if redo {
        ("redid", "redo")
    } else {
        ("undid", "undo")
    };
    let reasons: Vec<String> = skipped
        .iter()
        .map(|skipped| skipped.why.said(&sidebar::item_name(&skipped.item)))
        .collect();
    match (done, reasons.is_empty()) {
        (_, true) => (
            layout::Feedback::Success,
            format!("{did} {}", label.phrase(done)),
        ),
        (0, false) => (
            layout::Feedback::Failure,
            format!(
                "could not {doing} {}: {}",
                label.phrase(label.count),
                reasons.join("; ")
            ),
        ),
        (_, false) => (
            layout::Feedback::Failure,
            format!("{did} {}; {}", label.phrase(done), reasons.join("; ")),
        ),
    }
}

/// What a drop from outside asks when names it brings are in use in
/// `folder`, and what the answers mean: the one item by its name, or how
/// many there are.
fn taken_question(taken: &[&Path], folder: &str) -> (String, &'static str) {
    match taken {
        [path] => (
            format!(
                "\u{201c}{}\u{201d} already exists in \u{201c}{folder}\u{201d}.",
                sidebar::item_name(path)
            ),
            "Replace moves the item that is there to the Trash. \
             Keep Both copies the new one under another name.",
        ),
        _ => (
            format!(
                "{} of these items already exist in \u{201c}{folder}\u{201d}.",
                taken.len()
            ),
            "Replace moves the items that are there to the Trash. \
             Keep Both copies the new ones under other names.",
        ),
    }
}

/// The answer a test instance gives the `asked`th question of a drop,
/// from `script` (see [`DROP_ANSWER`]). `Some(None)` cancels, and `None`
/// is no answer at all: there is none left, or it is not one.
fn scripted_drop_answer(script: &str, asked: usize) -> Option<Option<OnTaken>> {
    match script.split(',').map(str::trim).nth(asked)? {
        "replace" => Some(Some(OnTaken::Replace)),
        "keep-both" => Some(Some(OnTaken::KeepBoth)),
        "cancel" => Some(None),
        _ => None,
    }
}

/// Escape in the tree, before it does anything to the selection: a copy
/// that is running is told to stop, or else a cut that is waiting is
/// called off. Whether there was either.
pub(super) fn escape(state: &mut State) -> bool {
    if let Some(job) = &state.explorer.job {
        job.stop.store(true, Ordering::Relaxed);
        return true;
    }
    if state.explorer.cut_change.is_none() {
        return false;
    }
    // Still ours: the items stay on the clipboard as a copy, so a paste
    // after this cannot move them.
    if state.explorer.cut_change == Some(clipboard::change_count()) {
        clipboard::write_files(state.tree.cut(), false);
    }
    forget_cut(state);
    state.say(layout::Feedback::Info, "cut called off");
    true
}

/// Before a frame: the cut rows stop being dimmed once something else has
/// been copied, here or in another app.
pub(super) fn sync_cut(state: &mut State) {
    if state
        .explorer
        .cut_change
        .is_some_and(|change| change != clipboard::change_count())
    {
        forget_cut(state);
    }
}

fn forget_cut(state: &mut State) {
    state.explorer.cut_change = None;
    state.tree.set_cut(Vec::new());
}

/// After the tree was read again: what a paste or a duplicate made is
/// selected, the folders above it opened, and scrolled into view. What a
/// paste made is remembered with the selection as it is left here, for
/// the paste that may follow straight after: see [`paste_target`].
pub(super) fn select_made(state: &mut State) {
    let made = std::mem::take(&mut state.explorer.made);
    let from = state.explorer.made_from.take();
    if made.is_empty() {
        return;
    }
    state.tree.select_items(&made);
    let rows = chrome_of(state).sidebar.map_or(0, layout::sidebar_rows);
    state.tree.scroll_to_selection(rows);
    state.explorer.pasted = from.map(|change| Pasted {
        change,
        selection: state.tree.selection_changes(),
        paths: made,
    });
}

/// What a `dump` says of the file work: the clipboard's files and text,
/// the copy running, the operations done that an undo can take back,
/// oldest first (one that replaced an item with where that went in the
/// Trash), and what a scripted drag from outside was last answered.
pub(super) fn report(state: &State) -> String {
    let shown = |path: &Path| {
        state
            .tree
            .root()
            .and_then(|root| path.strip_prefix(root).ok())
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    };
    let files = clipboard::read_files();
    let on_clipboard = if files.paths.is_empty() {
        "none".to_string()
    } else {
        format!(
            "{} {}",
            if files.cut { "cut" } else { "copy" },
            files
                .paths
                .iter()
                .map(|path| shown(path))
                .collect::<Vec<_>>()
                .join("|")
        )
    };
    let text = clipboard::read_text()
        .unwrap_or_default()
        .lines()
        .map(|line| shown(Path::new(line)))
        .collect::<Vec<_>>()
        .join("|");
    let done: Vec<String> = state
        .explorer
        .steps
        .records()
        .map(|record| {
            let kind = match record.kind {
                OpKind::NewFile => "new-file",
                OpKind::NewFolder => "new-folder",
                OpKind::Rename => "rename",
                OpKind::Move => "move",
                OpKind::Copy => "copy",
                OpKind::Duplicate => "duplicate",
                OpKind::Replace => "replace",
                OpKind::Trash => "trash",
            };
            let at = |path: &Option<PathBuf>| path.as_deref().map_or("-".to_string(), shown);
            let trashed = match record.kind {
                OpKind::Replace => format!(" trashed {}", at(&record.trashed)),
                _ => String::new(),
            };
            format!(
                "{kind} {} > {}{trashed}",
                at(&record.source),
                at(&record.result)
            )
        })
        .collect();
    format!(
        "clipboard_files: {on_clipboard}\nclipboard_text: {text}\nfile_job: {}\nfile_ops: {}\nfile_drag: {}",
        state
            .explorer
            .job
            .as_ref()
            .map_or("none".to_string(), |job| format!("copying {}", job.total)),
        done.join("|"),
        match state.explorer.scripted_drag {
            "" => "none",
            answered => answered,
        },
    )
}

/// What a paste, a duplicate or a drop is called when it is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verb {
    Paste,
    /// A cut, pasted.
    Move,
    Duplicate,
    /// Dragged in the tree to another folder.
    Drag,
    /// Dragged there with Option held.
    DragCopy,
}

impl Verb {
    /// By a drag in the tree, not from the clipboard.
    fn dragged(self) -> bool {
        matches!(self, Verb::Drag | Verb::DragCopy)
    }

    /// Why an item was left alone, as the rest of a sentence that starts
    /// with its name.
    fn refusal(self, why: Refusal) -> &'static str {
        match (self, why) {
            (Verb::Drag, Refusal::IntoItself) => "cannot be moved into itself",
            (Verb::DragCopy, Refusal::IntoItself) => "cannot be copied into itself",
            _ => why.said(),
        }
    }
}

/// How a paste, a duplicate or a drop went, for the status line.
struct Outcome {
    verb: Verb,
    /// How many items were to be copied or moved.
    total: usize,
    /// Stopped before it was through.
    cancelled: bool,
    /// Where the new or moved items are.
    made: Vec<PathBuf>,
    /// How many of them took the place of an item, now in the Trash.
    replaced: usize,
    /// What could not be copied or moved, each as `name: why`.
    failed: Vec<String>,
    /// What the plan refused, each as a sentence about the item.
    refused: Vec<String>,
    /// The cut items that were pasted where they already are.
    stays: Vec<String>,
    /// The dragged items left where they were: something in the folder
    /// they were dropped on has their name.
    skipped: Vec<String>,
    /// A drop: the name of the folder it went to.
    to: Option<String>,
}

impl Outcome {
    fn new(verb: Verb) -> Outcome {
        Outcome {
            verb,
            total: 0,
            cancelled: false,
            made: Vec::new(),
            replaced: 0,
            failed: Vec::new(),
            refused: Vec::new(),
            stays: Vec::new(),
            skipped: Vec::new(),
            to: None,
        }
    }

    /// Nothing was refused, skipped or failed.
    fn nothing_to_say(&self) -> bool {
        self.failed.is_empty() && self.refused.is_empty() && self.skipped.is_empty()
    }

    /// The message, and what kind it is: a failure when anything was
    /// refused or could not be done, each of them named.
    fn said(&self) -> (layout::Feedback, String) {
        let (done, doing) = match self.verb {
            Verb::Paste => ("pasted", "copy"),
            Verb::Move | Verb::Drag => ("moved", "move"),
            Verb::Duplicate => ("duplicated", "duplicate"),
            Verb::DragCopy => ("copied", "copy"),
        };
        let made = self.made.len();
        let mut parts = Vec::new();
        if self.cancelled && made < self.total {
            parts.push(match made {
                0 => "copy stopped: nothing was copied".to_string(),
                _ => format!("copy stopped: {made} of {} items were copied", self.total),
            });
        } else if made > 0 {
            let what = match (made, self.total) {
                (1, 1) => format!("{done} {}", sidebar::item_name(&self.made[0])),
                (made, total) if made == total => format!("{done} {made} items"),
                (made, total) => format!("{done} {made} of {total} items"),
            };
            parts.push(match &self.to {
                Some(folder) => format!("{what} to {folder}"),
                None => what,
            });
        }
        match self.replaced {
            0 => {}
            1 => parts.push("the item it replaced is in the Trash".to_string()),
            n => parts.push(format!("the {n} items replaced are in the Trash")),
        }
        parts.extend(
            self.failed
                .iter()
                .map(|failed| format!("could not {doing} {failed}")),
        );
        parts.extend(self.refused.iter().cloned());
        match self.stays.as_slice() {
            [] => {}
            [name] => parts.push(format!("{name} is already in this folder")),
            names => parts.push(format!("{} items are already in this folder", names.len())),
        }
        match self.skipped.as_slice() {
            [] => {}
            [name] => parts.push(format!(
                "skipped {name}: an item of that name is already there"
            )),
            names => parts.push(format!(
                "skipped {} items: items of those names are already there",
                names.len()
            )),
        }
        let kind = if !self.nothing_to_say() {
            layout::Feedback::Failure
        } else if made > 0 && !self.cancelled {
            layout::Feedback::Success
        } else {
            layout::Feedback::Info
        };
        (kind, parts.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(verb: Verb, total: usize, made: &[&str]) -> Outcome {
        Outcome {
            total,
            made: made.iter().map(PathBuf::from).collect(),
            ..Outcome::new(verb)
        }
    }

    #[test]
    fn the_outcome_names_one_item_counts_several_and_names_every_failure() {
        use layout::Feedback::{Failure, Info, Success};
        let said = |outcome: Outcome| outcome.said();
        assert_eq!(
            said(outcome(Verb::Paste, 1, &["/p/notes/today-copy.md"])),
            (Success, "pasted today-copy.md".to_string())
        );
        assert_eq!(
            said(outcome(Verb::Move, 3, &["/p/a", "/p/b", "/p/c"])),
            (Success, "moved 3 items".to_string())
        );
        assert_eq!(
            said(outcome(Verb::Duplicate, 1, &["/p/trips-copy"])),
            (Success, "duplicated trips-copy".to_string())
        );
        // Part of it failed: how much was done, and each failure by name.
        let mut partly = outcome(Verb::Paste, 3, &["/p/a"]);
        partly.failed = vec![
            "big.bin: permission denied".into(),
            "old: no such file".into(),
        ];
        assert_eq!(
            said(partly),
            (
                Failure,
                "pasted 1 of 3 items; could not copy big.bin: permission denied; \
                 could not copy old: no such file"
                    .to_string()
            )
        );
        // Refused before anything was done, alone and beside what was.
        let mut refused = outcome(Verb::Paste, 0, &[]);
        refused.refused = vec!["recipes cannot be pasted into itself".into()];
        assert_eq!(
            said(refused),
            (Failure, "recipes cannot be pasted into itself".to_string())
        );
        let mut mixed = outcome(Verb::Move, 1, &["/p/out/a.txt"]);
        mixed.refused = vec!["gone.txt is no longer there".into()];
        assert_eq!(
            said(mixed),
            (
                Failure,
                "moved a.txt; gone.txt is no longer there".to_string()
            )
        );
        // A cut pasted where it is: nothing to do, and no failure.
        let mut stays = outcome(Verb::Move, 0, &[]);
        stays.stays = vec!["garden.txt".into()];
        assert_eq!(
            said(stays),
            (Info, "garden.txt is already in this folder".to_string())
        );
        let mut stay = outcome(Verb::Move, 0, &[]);
        stay.stays = vec!["a".into(), "b".into()];
        assert_eq!(
            said(stay),
            (Info, "2 items are already in this folder".to_string())
        );
        // Stopped: what was copied whole is said, and it is no failure.
        let mut stopped = outcome(Verb::Paste, 5, &["/p/a", "/p/b"]);
        stopped.cancelled = true;
        assert_eq!(
            said(stopped),
            (Info, "copy stopped: 2 of 5 items were copied".to_string())
        );
        let mut stopped = outcome(Verb::Duplicate, 1, &[]);
        stopped.cancelled = true;
        assert_eq!(
            said(stopped),
            (Info, "copy stopped: nothing was copied".to_string())
        );
        // Escape just as the last item landed: it all went through.
        let mut late = outcome(Verb::Paste, 1, &["/p/a.txt"]);
        late.cancelled = true;
        assert_eq!(said(late), (Info, "pasted a.txt".to_string()));
    }

    /// A drop says where it went, and counts what it left behind because
    /// the folder already has something of that name.
    #[test]
    fn a_drop_names_its_folder_and_counts_what_it_skipped() {
        use layout::Feedback::{Failure, Success};
        let dropped = |verb, total, made: &[&str], skipped: &[&str]| {
            let mut outcome = outcome(verb, total, made);
            outcome.to = Some("trips".into());
            outcome.skipped = skipped.iter().map(|name| name.to_string()).collect();
            outcome.said()
        };
        assert_eq!(
            dropped(Verb::Drag, 1, &["/p/trips/garden.txt"], &[]),
            (Success, "moved garden.txt to trips".to_string())
        );
        assert_eq!(
            dropped(
                Verb::Drag,
                3,
                &["/p/trips/a", "/p/trips/b", "/p/trips/c"],
                &[]
            ),
            (Success, "moved 3 items to trips".to_string())
        );
        assert_eq!(
            dropped(Verb::DragCopy, 1, &["/p/trips/garden-copy.txt"], &[]),
            (Success, "copied garden-copy.txt to trips".to_string())
        );
        // One name in use there: the others went, and it is named.
        assert_eq!(
            dropped(Verb::Drag, 2, &["/p/trips/a", "/p/trips/b"], &["soup.md"]),
            (
                Failure,
                "moved 2 items to trips; skipped soup.md: an item of that name is already there"
                    .to_string()
            )
        );
        // Several: how many.
        assert_eq!(
            dropped(Verb::Drag, 1, &["/p/trips/a"], &["soup.md", "bread.md"]),
            (
                Failure,
                "moved a to trips; skipped 2 items: items of those names are already there"
                    .to_string()
            )
        );
        // Nothing could go.
        assert_eq!(
            dropped(Verb::Drag, 0, &[], &["soup.md", "bread.md", "list.txt"]),
            (
                Failure,
                "skipped 3 items: items of those names are already there".to_string()
            )
        );
        // A folder into itself, by its own word for each way of dropping.
        assert_eq!(
            Verb::Drag.refusal(Refusal::IntoItself),
            "cannot be moved into itself"
        );
        assert_eq!(
            Verb::DragCopy.refusal(Refusal::IntoItself),
            "cannot be copied into itself"
        );
        assert_eq!(
            Verb::Paste.refusal(Refusal::IntoItself),
            "cannot be pasted into itself"
        );
        assert_eq!(Verb::Drag.refusal(Refusal::Missing), "is no longer there");
    }

    /// A drop from outside says where it went, as a copy by drag does, and
    /// that what it replaced is in the Trash. Its one question names the
    /// item whose name is in use, or counts them, and a test instance
    /// answers it from a list.
    #[test]
    fn a_drop_from_outside_asks_once_and_says_what_it_replaced() {
        use layout::Feedback::{Failure, Success};
        let dropped = |total, made: &[&str], replaced| {
            let mut outcome = outcome(Verb::DragCopy, total, made);
            outcome.to = Some("recipes".into());
            outcome.replaced = replaced;
            outcome
        };
        assert_eq!(
            dropped(3, &["/p/recipes/a", "/p/recipes/b", "/p/recipes/c"], 0).said(),
            (Success, "copied 3 items to recipes".to_string())
        );
        assert_eq!(
            dropped(1, &["/p/recipes/soup.md"], 1).said(),
            (
                Success,
                "copied soup.md to recipes; the item it replaced is in the Trash".to_string()
            )
        );
        let mut mixed = dropped(3, &["/p/recipes/a", "/p/recipes/b", "/p/recipes/c"], 2);
        mixed.refused = vec!["gone.txt is no longer there".into()];
        assert_eq!(
            mixed.said(),
            (
                Failure,
                "copied 3 items to recipes; the 2 items replaced are in the Trash; \
                 gone.txt is no longer there"
                    .to_string()
            )
        );

        let (soup, bread) = (Path::new("/desk/soup.md"), Path::new("/desk/bread.md"));
        let (question, detail) = taken_question(&[soup], "recipes");
        assert_eq!(
            question,
            "\u{201c}soup.md\u{201d} already exists in \u{201c}recipes\u{201d}."
        );
        assert!(detail.starts_with("Replace moves the item that is there to the Trash."));
        let (question, detail) = taken_question(&[soup, bread], "recipes");
        assert_eq!(
            question,
            "2 of these items already exist in \u{201c}recipes\u{201d}."
        );
        assert!(detail.starts_with("Replace moves the items that are there to the Trash."));

        // In order, and past the last of them there is no answer: the
        // instance says what it would ask and copies nothing.
        let script = "replace, keep-both,cancel";
        assert_eq!(
            scripted_drop_answer(script, 0),
            Some(Some(OnTaken::Replace))
        );
        assert_eq!(
            scripted_drop_answer(script, 1),
            Some(Some(OnTaken::KeepBoth))
        );
        assert_eq!(scripted_drop_answer(script, 2), Some(None));
        assert_eq!(scripted_drop_answer(script, 3), None);
        assert_eq!(scripted_drop_answer("", 0), None);
        assert_eq!(scripted_drop_answer("overwrite", 0), None);

        // A plan that only stays or refuses does nothing: the tree is not
        // given the keyboard for it.
        let stays = [Planned::Stays("/p/a".into())];
        assert!(!acts(&stays));
        let replaces = [Planned::Replace {
            from: "/desk/a".into(),
            to: "/p/a".into(),
        }];
        assert!(acts(&replaces));
    }

    /// Cmd-C, Cmd-V, Cmd-V on a folder: the second paste finds the cursor
    /// on the copy the first one made, and used to go into it. It goes
    /// beside it, unless something else was copied since or the cursor was
    /// put there on purpose.
    #[test]
    fn a_paste_straight_after_a_paste_goes_beside_what_the_first_made() {
        let last = Pasted {
            change: 7,
            selection: 40,
            paths: vec![
                PathBuf::from("/p/recipes-copy"),
                PathBuf::from("/p/notes/list.txt"),
            ],
        };
        let target = |dir: &str, last: Option<&Pasted>, change, selection| {
            paste_target(PathBuf::from(dir), last, change, selection)
        };
        // Nothing touched since: beside the copy, in the folder it is in.
        assert_eq!(
            target("/p/recipes-copy", Some(&last), 7, 40),
            PathBuf::from("/p")
        );
        // The cursor was moved, and brought back or not: into the folder
        // it is on, the copy included.
        assert_eq!(
            target("/p/recipes-copy", Some(&last), 7, 41),
            PathBuf::from("/p/recipes-copy")
        );
        // Something else was copied since: its paste is a first one.
        assert_eq!(
            target("/p/recipes-copy", Some(&last), 8, 40),
            PathBuf::from("/p/recipes-copy")
        );
        // Another folder, or no paste before this one: into it.
        assert_eq!(
            target("/p/trips", Some(&last), 7, 40),
            PathBuf::from("/p/trips")
        );
        assert_eq!(
            target("/p/notes", Some(&last), 7, 40),
            PathBuf::from("/p/notes"),
            "the folder a pasted file is in"
        );
        assert_eq!(
            target("/p/recipes-copy", None, 7, 40),
            PathBuf::from("/p/recipes-copy")
        );
    }

    /// The window's one way to change files keeps what each of them did.
    #[test]
    fn every_operation_is_kept_and_the_oldest_go_first() {
        let tree = crate::project::TempTree::new("explorer-kept", &[("a.txt", "a")]);
        let dir = tree.0.as_path();
        let mut files = Files::default();
        let step = files.begin();
        let made = file_op(&mut files, step, Op::NewFolder(&dir.join("notes")), false).unwrap();
        assert_eq!(made.path, Some(dir.join("notes")));
        let (from, to) = (dir.join("a.txt"), dir.join("notes/a.txt"));
        let step = files.begin();
        file_op(
            &mut files,
            step,
            Op::Move {
                from: &from,
                to: &to,
            },
            false,
        )
        .unwrap();
        // Refused, so not kept.
        let step = files.begin();
        let clash = file_op(&mut files, step, Op::NewFile(&to), false);
        assert_eq!(
            clash.err().map(|e| e.kind()),
            Some(io::ErrorKind::AlreadyExists)
        );
        let kinds: Vec<OpKind> = files.steps.records().map(|record| record.kind).collect();
        assert_eq!(kinds, [OpKind::NewFolder, OpKind::Move]);
        let moved = &files.steps.last().unwrap()[0];
        assert_eq!(moved.source.as_deref(), Some(from.as_path()));
        assert_eq!(moved.result.as_deref(), Some(to.as_path()));

        for n in 0..fileops::STEPS_KEPT {
            let step = files.begin();
            file_op(
                &mut files,
                step,
                Op::NewFile(&dir.join(format!("notes/{n}.txt"))),
                false,
            )
            .unwrap();
        }
        assert_eq!(files.steps.len(), fileops::STEPS_KEPT);
        assert_eq!(
            files.steps.records().next().map(|record| record.kind),
            Some(OpKind::NewFile)
        );
    }

    /// A Trash for a test: a folder of the fixture, where an item takes
    /// the next free name, as the Trash renames what would clash.
    fn trash_into(bin: &Path) -> impl Fn(&Path) -> io::Result<Option<PathBuf>> + '_ {
        move |path: &Path| {
            let folder = path.is_dir();
            let landed = bin.join(fileops::free_name(bin, path.file_name().unwrap(), folder));
            std::fs::rename(path, &landed)?;
            Ok(Some(landed))
        }
    }

    /// Takes the last step of `files` back, or with `redo` does the last
    /// one taken back again, as the window does with no tab open.
    fn turn(files: &mut Files, redo: bool, bin: &Path) -> (usize, Vec<Skipped>) {
        let trash = trash_into(bin);
        let mut apply = |reversal: &Reversal| reversal.apply(&trash);
        match next_turn(files, redo, &|_| false) {
            Turn::Undo(records) => {
                let (undone, skipped) = fileops::undo_step(records, &mut apply);
                let done = undone.len();
                files.steps.took_back(undone);
                (done, skipped)
            }
            Turn::Redo(undone) => {
                let (records, skipped) = fileops::redo_step(undone, &mut apply);
                let done = records.len();
                files.steps.did_again(records);
                (done, skipped)
            }
            Turn::Nothing | Turn::Unsaved => (0, Vec::new()),
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// One thing done is one step, whatever it came to, and one undo. An
    /// undo and a redo are not steps themselves: they move a step between
    /// what can be taken back and what can be done again. Anything new
    /// done to the files ends what could be done again, and another
    /// folder starts with nothing.
    #[test]
    fn a_step_is_undone_whole_and_redone_until_something_new_is_done() {
        let tree = crate::project::TempTree::new(
            "explorer-steps",
            &[
                ("bread.md", "flour\n"),
                ("soup.md", "leeks\n"),
                ("plots.txt", "beans\n"),
                ("recipes/", ""),
                ("bin/", ""),
            ],
        );
        let dir = tree.0.as_path();
        let bin = dir.join("bin");
        let mut files = Files::default();
        assert_eq!(edit_title(&files, false), ("Undo".to_string(), false));
        assert_eq!(edit_title(&files, true), ("Redo".to_string(), false));
        assert!(matches!(
            next_turn(&mut files, false, &|_| false),
            Turn::Nothing
        ));

        // A drag of three items: three operations, one step.
        let step = files.begin();
        for name in ["bread.md", "soup.md", "plots.txt"] {
            let (from, to) = (dir.join(name), dir.join("recipes").join(name));
            file_op(
                &mut files,
                step,
                Op::Move {
                    from: &from,
                    to: &to,
                },
                false,
            )
            .unwrap();
        }
        // A rename after it: a step of its own.
        let step = files.begin();
        let (from, to) = (dir.join("recipes/soup.md"), dir.join("recipes/stew.md"));
        file_op(
            &mut files,
            step,
            Op::Rename {
                from: &from,
                to: &to,
            },
            false,
        )
        .unwrap();
        assert_eq!(files.steps.len(), 2);
        assert_eq!(files.steps.records().count(), 4);
        assert_eq!(edit_title(&files, false), ("Undo Rename".to_string(), true));

        // Undo: the rename alone, then the whole drag.
        assert_eq!(turn(&mut files, false, &bin).0, 1);
        assert_eq!(
            names(&dir.join("recipes")),
            ["bread.md", "plots.txt", "soup.md"]
        );
        assert_eq!(
            edit_title(&files, false),
            ("Undo Move of 3 Items".to_string(), true)
        );
        assert_eq!(edit_title(&files, true), ("Redo Rename".to_string(), true));
        assert_eq!(turn(&mut files, false, &bin).0, 3);
        assert_eq!(
            names(dir),
            ["bin", "bread.md", "plots.txt", "recipes", "soup.md"]
        );
        assert_eq!(names(&dir.join("recipes")), [] as [&str; 0]);
        // Neither undo is a step: nothing is left to take back.
        assert_eq!(files.steps.len(), 0);
        assert_eq!(edit_title(&files, false), ("Undo".to_string(), false));
        assert_eq!(
            edit_title(&files, true),
            ("Redo Move of 3 Items".to_string(), true)
        );

        // Redo, in the order they were done, and the redo is no new step
        // either: the rename can still be done again after it.
        assert_eq!(turn(&mut files, true, &bin).0, 3);
        assert_eq!(
            names(&dir.join("recipes")),
            ["bread.md", "plots.txt", "soup.md"]
        );
        assert_eq!(files.steps.len(), 1);
        assert_eq!(edit_title(&files, true), ("Redo Rename".to_string(), true));
        assert_eq!(turn(&mut files, true, &bin).0, 1);
        assert_eq!(
            names(&dir.join("recipes")),
            ["bread.md", "plots.txt", "stew.md"]
        );
        assert_eq!(files.steps.len(), 2);
        assert_eq!(edit_title(&files, true), ("Redo".to_string(), false));

        // Something new done after an undo: nothing can be done again.
        assert_eq!(turn(&mut files, false, &bin).0, 1);
        assert_eq!(edit_title(&files, true), ("Redo Rename".to_string(), true));
        let step = files.begin();
        file_op(&mut files, step, Op::NewFile(&dir.join("notes.txt")), false).unwrap();
        assert_eq!(edit_title(&files, true), ("Redo".to_string(), false));
        assert_eq!(
            edit_title(&files, false),
            ("Undo New File".to_string(), true)
        );
        assert!(matches!(
            next_turn(&mut files, true, &|_| false),
            Turn::Nothing
        ));
        assert_eq!(files.steps.len(), 2);

        // Nothing was deleted on the way, and nothing went to the Trash
        // that an undo did not send there.
        assert_eq!(names(&bin), [] as [&str; 0]);
        assert_eq!(turn(&mut files, false, &bin).0, 1);
        assert_eq!(names(&bin), ["notes.txt"]);

        // Another folder: its steps are its own.
        files.forget_steps();
        assert_eq!(edit_title(&files, false), ("Undo".to_string(), false));
        assert_eq!(edit_title(&files, true), ("Redo".to_string(), false));
    }

    /// Unsaved work in a tab on an item that would move or go to the
    /// Trash, or under one, would be left on a file that is gone: the
    /// step is refused whole and stays where it is, to be undone once the
    /// tab is saved. A tab on an item that would be skipped anyway is not
    /// in the way, and neither is one on an item coming out of the Trash.
    #[test]
    fn a_step_stays_while_a_tab_it_would_strand_holds_unsaved_work() {
        let tree = crate::project::TempTree::new(
            "explorer-undo-unsaved",
            &[
                ("soup.md", "leeks\n"),
                ("bread.md", "flour\n"),
                ("notes/", ""),
                ("bin/", ""),
            ],
        );
        let dir = crate::platform::canonical(&tree.0);
        let bin = dir.join("bin");
        let mut files = Files::default();
        let step = files.begin();
        for name in ["soup.md", "bread.md"] {
            let (from, to) = (dir.join(name), dir.join("notes").join(name));
            file_op(
                &mut files,
                step,
                Op::Move {
                    from: &from,
                    to: &to,
                },
                false,
            )
            .unwrap();
        }
        // A tab on the moved soup.md, typed in and not saved.
        let mut buffer = Buffer::open(dir.join("notes/soup.md")).unwrap();
        buffer.insert("more ");
        let docs = Documents::new(buffer);
        assert!(docs.has_dirty_under(&dir.join("notes/soup.md")));
        let dirty = |path: &Path| docs.has_dirty_under(path);

        assert!(matches!(
            next_turn(&mut files, false, &dirty),
            Turn::Unsaved
        ));
        assert_eq!(files.steps.len(), 1, "the step is still there");
        assert_eq!(
            edit_title(&files, false),
            ("Undo Move of 2 Items".to_string(), true)
        );
        assert_eq!(names(&dir.join("notes")), ["bread.md", "soup.md"]);

        // The place soup.md would go back to is taken: it would be
        // skipped, so its tab is not in the way of bread.md.
        std::fs::write(dir.join("soup.md"), "another soup\n").unwrap();
        let Turn::Undo(records) = next_turn(&mut files, false, &dirty) else {
            panic!("only what would move is asked about");
        };
        assert_eq!(records.len(), 2);
        assert_eq!(files.steps.len(), 0);

        // The same for a redo: a tab on the item it would move again.
        std::fs::remove_file(dir.join("soup.md")).unwrap();
        let mut files = Files::default();
        let step = files.begin();
        let (from, to) = (dir.join("notes/bread.md"), dir.join("notes/rye.md"));
        file_op(
            &mut files,
            step,
            Op::Rename {
                from: &from,
                to: &to,
            },
            false,
        )
        .unwrap();
        assert_eq!(turn(&mut files, false, &bin).0, 1);
        let mut buffer = Buffer::open(from.clone()).unwrap();
        buffer.insert("dark ");
        let docs = Documents::new(buffer);
        let dirty = |path: &Path| docs.has_dirty_under(path);
        assert!(matches!(next_turn(&mut files, true, &dirty), Turn::Unsaved));
        assert_eq!(edit_title(&files, true), ("Redo Rename".to_string(), true));
        assert!(from.exists() && !to.exists());

        // An item coming out of the Trash moves nothing a tab is on.
        let mut files = Files::default();
        let trash = trash_into(&bin);
        let record = fileops::perform(
            Op::Trash(&from),
            &Env {
                trash: &trash,
                stop: &|| false,
            },
        )
        .unwrap();
        let step = files.begin();
        files.steps.keep(step, record);
        assert!(matches!(
            next_turn(&mut files, false, &|_| true),
            Turn::Undo(_)
        ));
    }

    /// A record as an operation of `kind` leaves it, on paths that need
    /// not exist.
    fn record(kind: OpKind, source: Option<&str>, result: Option<&str>) -> Record {
        Record {
            kind,
            source: source.map(PathBuf::from),
            result: result.map(PathBuf::from),
            trashed: None,
            original_left: None,
            item: None,
            replaced: None,
        }
    }

    /// Edit > Undo and Redo say what they would do: the operation, and
    /// the one item by its name or how many there are.
    #[test]
    fn the_menu_names_the_step_it_would_take_back_or_do_again() {
        let title = |records: &[Record]| Label::of(records.iter()).unwrap().title();
        assert_eq!(
            title(&[record(OpKind::NewFile, None, Some("/p/plots/beds.md"))]),
            "New File"
        );
        assert_eq!(
            title(&[record(OpKind::NewFolder, None, Some("/p/plots"))]),
            "New Folder"
        );
        assert_eq!(
            title(&[record(
                OpKind::Rename,
                Some("/p/soup.md"),
                Some("/p/stew.md")
            )]),
            "Rename"
        );
        let moved = |name: &str| {
            record(
                OpKind::Move,
                Some(&format!("/p/{name}")),
                Some(&format!("/p/recipes/{name}")),
            )
        };
        assert_eq!(title(&[moved("bread.md")]), "Move of bread.md");
        assert_eq!(
            title(&[moved("bread.md"), moved("soup.md"), moved("plots.txt")]),
            "Move of 3 Items"
        );
        let copied = record(OpKind::Copy, Some("/desk/bread.md"), Some("/p/bread.md"));
        assert_eq!(title(std::slice::from_ref(&copied)), "Copy of bread.md");
        assert_eq!(
            title(&[record(
                OpKind::Duplicate,
                Some("/p/trips"),
                Some("/p/trips-copy")
            )]),
            "Duplicate of trips-copy"
        );
        // A trashed item is named as it was: it is nowhere now.
        let trashed = |name: &str| record(OpKind::Trash, Some(&format!("/p/{name}")), None);
        assert_eq!(title(&[trashed("old.md")]), "Move to Trash of old.md");
        assert_eq!(
            title(&[trashed("old.md"), trashed("older.md")]),
            "Move to Trash of 2 Items"
        );
        let replaced = |name: &str| {
            record(
                OpKind::Replace,
                Some(&format!("/desk/{name}")),
                Some(&format!("/p/{name}")),
            )
        };
        assert_eq!(title(&[replaced("soup.md")]), "Replace of soup.md");
        assert_eq!(
            title(&[replaced("soup.md"), replaced("bread.md")]),
            "Replace of 2 Items"
        );
        // A drop that replaced two items and copied a third is a copy.
        assert_eq!(
            title(&[replaced("soup.md"), replaced("bread.md"), copied]),
            "Copy of 3 Items"
        );
        assert_eq!(Label::of(std::iter::empty()), None);

        // Under Undo for what was done, under Redo for what was taken
        // back, and off with neither.
        let mut files = Files::default();
        let step = files.begin();
        files.steps.keep(step, moved("bread.md"));
        files.steps.keep(step, moved("soup.md"));
        assert_eq!(
            edit_title(&files, false),
            ("Undo Move of 2 Items".to_string(), true)
        );
        assert_eq!(edit_title(&files, true), ("Redo".to_string(), false));
        let records = files.steps.take_last().unwrap();
        files.steps.took_back(
            records
                .into_iter()
                .map(|record| Undone {
                    record,
                    aside: None,
                })
                .collect(),
        );
        assert_eq!(edit_title(&files, false), ("Undo".to_string(), false));
        assert_eq!(
            edit_title(&files, true),
            ("Redo Move of 2 Items".to_string(), true)
        );
    }

    /// The status line says what was taken back or done again, how much
    /// of it, and each item that was skipped with why. A step of which
    /// nothing could be done says so.
    #[test]
    fn a_turned_step_says_what_went_and_what_was_skipped() {
        use layout::Feedback::{Failure, Success};
        let label = |kind, count, name: &str| Label {
            kind,
            count,
            name: name.to_string(),
        };
        let skipped = |item: &str, why| Skipped {
            item: PathBuf::from(item),
            why,
        };
        assert_eq!(
            turned_said(false, &label(OpKind::Move, 3, "bread.md"), 3, &[]),
            (Success, "undid move of 3 items".to_string())
        );
        assert_eq!(
            turned_said(true, &label(OpKind::Copy, 1, "bread.md"), 1, &[]),
            (Success, "redid copy of bread.md".to_string())
        );
        assert_eq!(
            turned_said(false, &label(OpKind::Rename, 1, "stew.md"), 1, &[]),
            (Success, "undid rename to stew.md".to_string())
        );
        assert_eq!(
            turned_said(false, &label(OpKind::NewFile, 1, "beds.md"), 1, &[]),
            (Success, "undid new file beds.md".to_string())
        );
        assert_eq!(
            turned_said(true, &label(OpKind::Trash, 2, "old.md"), 2, &[]),
            (Success, "redid move to Trash of 2 items".to_string())
        );
        // Part of it: how much, and every item left alone by name.
        assert_eq!(
            turned_said(
                false,
                &label(OpKind::Move, 3, "bread.md"),
                1,
                &[
                    skipped("/p/recipes/soup.md", Skip::Taken("/p/soup.md".into())),
                    skipped("/p/recipes/plots.txt", Skip::Gone),
                ]
            ),
            (
                Failure,
                "undid move of 1 of 3 items; \
                 soup.md was left alone: an item named soup.md is in the way; \
                 plots.txt is no longer there"
                    .to_string()
            )
        );
        // None of it.
        assert_eq!(
            turned_said(
                false,
                &label(OpKind::Rename, 1, "stew.md"),
                0,
                &[skipped("/p/stew.md", Skip::Taken("/p/soup.md".into()))]
            ),
            (
                Failure,
                "could not undo rename to stew.md: \
                 stew.md was left alone: an item named soup.md is in the way"
                    .to_string()
            )
        );
        // Still in the Trash: said plainly, with where it belongs.
        let stuck = Skip::StillInTrash {
            belongs: "/p/recipes/old.md".into(),
            error: io::Error::new(io::ErrorKind::PermissionDenied, "not permitted"),
        };
        assert_eq!(
            turned_said(
                false,
                &label(OpKind::Trash, 1, "old.md"),
                0,
                &[skipped("/p/recipes/old.md", stuck)]
            ),
            (
                Failure,
                "could not undo move to Trash of old.md: \
                 old.md is still in the Trash and belongs in /p/recipes: not permitted"
                    .to_string()
            )
        );
        assert_eq!(
            turned_said(
                true,
                &label(OpKind::Replace, 2, "soup.md"),
                0,
                &[
                    skipped("/p/soup.md", Skip::Changed),
                    skipped("/p/bread.md", Skip::GoneFromTrash),
                ]
            ),
            (
                Failure,
                "could not redo replace of 2 items: \
                 soup.md is no longer the same kind of item; \
                 bread.md is no longer in the Trash"
                    .to_string()
            )
        );
    }
}
