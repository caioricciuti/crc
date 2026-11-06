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

use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;

use super::*;
use crate::project::fileops::{self, Env, Op, OpKind, Planned, Record, Refusal};

/// How many finished operations are kept.
const KEPT: usize = 100;

/// Under the self-test, a file a copy waits for before each item it
/// copies: a script holds a copy where it is, looks, and lets it go.
const COPY_GATE: &str = "CRC_COPY_GATE";

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
    /// The operations done, oldest first, as [`fileops::perform`] answered
    /// them: what an undo has to go by.
    done: VecDeque<Record>,
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
}

/// Adds `record` to the operations `done`, the oldest making room.
fn keep(done: &mut VecDeque<Record>, record: Record) {
    if done.len() == KEPT {
        done.pop_front();
    }
    done.push_back(record);
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
    verb: Verb,
    /// A drop: the folder it went to, as the outcome names it.
    to: Option<String>,
    /// A paste: the clipboard's change count when it was asked for.
    from: Option<isize>,
    /// What the status line says while it runs.
    said: String,
    made: Vec<PathBuf>,
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
/// the Trash. What was done is kept in `files`. With `free`, a move or a
/// copy whose name was taken since it was chosen takes the next free one
/// instead of failing. The copies of a paste go through the same
/// [`fileops::perform`] on their worker.
pub(super) fn file_op(files: &mut Files, op: Op<'_>, free: bool) -> io::Result<Did> {
    let (did, record) = perform(op, free)?;
    keep(&mut files.done, record);
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
    /// [`file_op`] for this window. The state is not held while the disk
    /// is worked on: moving to the Trash is the system's, and may take a
    /// moment.
    pub(super) fn file_op(&self, op: Op<'_>) -> io::Result<Did> {
        let (did, record) = perform(op, false)?;
        if let Some(mut state) = self.state_mut() {
            keep(&mut state.explorer.done, record);
        }
        Ok(did)
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
        if self.state().is_some_and(|state| !state.explorer.idle()) {
            self.say_about_files(
                layout::Feedback::Info,
                "a copy is still running: Escape in the Explorer stops it",
            );
            return;
        }
        let mut copies = Vec::new();
        let mut moves = Vec::new();
        for planned in plan {
            match planned {
                Planned::Copy { from, to } => copies.push((from, to)),
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
        for (from, to) in &moves {
            // A paste's move takes the next free name when its own was
            // taken since the plan; a drag's is skipped.
            match self.relocate(Op::Move { from, to }, !verb.dragged()) {
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

    /// Hands the copies of a paste or a duplicate to a worker and says so
    /// in the status line. The display link takes what comes back.
    fn start_copy(&self, copies: Vec<(PathBuf, PathBuf)>, outcome: Outcome) {
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
                    // A copy trashes nothing.
                    trash: &|_| Err(io::ErrorKind::Unsupported.into()),
                    stop: &halted,
                };
                for (from, to) in copies {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let (from, to) = (from.as_path(), to.as_path());
                    let op = if duplicate {
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
            state.explorer.job = Some(Job {
                rx,
                stop,
                worker: Some(worker),
                total,
                verb,
                to: outcome.to,
                from,
                said,
                made: Vec::new(),
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
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let state = &mut *state;
            let Files { job, done, .. } = &mut state.explorer;
            let Some(running) = job else {
                return;
            };
            let over = loop {
                match running.rx.try_recv() {
                    Ok((_, Ok(record))) => {
                        running.made.extend(record.result.clone());
                        keep(done, record);
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
                failed: job.failed,
                refused: job.refused,
                to: job.to,
                ..Outcome::new(job.verb)
            };
            let (kind, said) = outcome.said();
            state.explorer.made = outcome.made;
            state.explorer.made_from = job.from;
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

    fn say_about_files(&self, kind: layout::Feedback, text: &str) {
        if let Some(mut state) = self.state_mut() {
            state.say(kind, text);
        }
        self.request_redraw();
        self.pump();
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
/// the copy running, and the operations done, oldest first.
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
        .done
        .iter()
        .map(|record| {
            let kind = match record.kind {
                OpKind::NewFile => "new-file",
                OpKind::NewFolder => "new-folder",
                OpKind::Rename => "rename",
                OpKind::Move => "move",
                OpKind::Copy => "copy",
                OpKind::Duplicate => "duplicate",
                OpKind::Trash => "trash",
            };
            let at = |path: &Option<PathBuf>| path.as_deref().map_or("-".to_string(), shown);
            format!("{kind} {} > {}", at(&record.source), at(&record.result))
        })
        .collect();
    format!(
        "clipboard_files: {on_clipboard}\nclipboard_text: {text}\nfile_job: {}\nfile_ops: {}",
        state
            .explorer
            .job
            .as_ref()
            .map_or("none".to_string(), |job| format!("copying {}", job.total)),
        done.join("|"),
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
        let made = file_op(&mut files, Op::NewFolder(&dir.join("notes")), false).unwrap();
        assert_eq!(made.path, Some(dir.join("notes")));
        let (from, to) = (dir.join("a.txt"), dir.join("notes/a.txt"));
        file_op(
            &mut files,
            Op::Move {
                from: &from,
                to: &to,
            },
            false,
        )
        .unwrap();
        // Refused, so not kept.
        let clash = file_op(&mut files, Op::NewFile(&to), false);
        assert_eq!(
            clash.err().map(|e| e.kind()),
            Some(io::ErrorKind::AlreadyExists)
        );
        let kinds: Vec<OpKind> = files.done.iter().map(|record| record.kind).collect();
        assert_eq!(kinds, [OpKind::NewFolder, OpKind::Move]);
        assert_eq!(files.done[1].source.as_deref(), Some(from.as_path()));
        assert_eq!(files.done[1].result.as_deref(), Some(to.as_path()));

        for n in 0..KEPT {
            file_op(
                &mut files,
                Op::NewFile(&dir.join(format!("notes/{n}.txt"))),
                false,
            )
            .unwrap();
        }
        assert_eq!(files.done.len(), KEPT);
        assert_eq!(files.done[0].kind, OpKind::NewFile);
    }
}
