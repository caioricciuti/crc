//! The runs of lines the gutter marks, from the Git menu and the gutter:
//! stepping through them, peeking at what HEAD had, reverting one to that,
//! and staging one.

use super::*;
use crate::ide::diff::Change;

/// The peek at one change of the active document, open over the text.
pub(super) struct Peek {
    pub(super) buffer: u64,
    pub(super) change: Change,
    pub(super) content: layout::ChangePeek,
    /// The text it was opened on: an edit closes it.
    pub(super) text: crate::text::rope::Rope,
    /// The scroll it was first drawn at: scrolling closes it.
    pub(super) scroll: Option<(usize, f32, f32)>,
    /// Where its controls were drawn this frame; `None` when it was not.
    pub(super) hits: Option<layout::PeekHits>,
}

/// What a click on the open peek does.
pub(super) enum PeekClick {
    Revert,
    Stage,
    Close,
    /// On the card but on none of its controls.
    Nothing,
}

/// The peek's control under a point in the view, when the point is on the
/// peek.
pub(super) fn peek_click(state: &State, x: f32, y: f32) -> Option<PeekClick> {
    let hits = state.peek.as_ref()?.hits?;
    if !hits.panel.contains(x, y) {
        return None;
    }
    Some(if hits.revert.contains(x, y) {
        PeekClick::Revert
    } else if hits.stage.contains(x, y) {
        PeekClick::Stage
    } else if hits.close.contains(x, y) {
        PeekClick::Close
    } else {
        PeekClick::Nothing
    })
}

/// The line whose change mark a plain click at `(x, y)` in the view is
/// on, if any; shift, option and command clicks still select from there.
pub(super) fn change_bar_under(state: &State, event: &NSEvent, x: f32, y: f32) -> Option<usize> {
    let modified = event.modifierFlags().intersects(
        NSEventModifierFlags::Shift | NSEventModifierFlags::Command | NSEventModifierFlags::Option,
    );
    let chrome = chrome_of(state);
    if modified || !chrome.text.contains(x, y) {
        return None;
    }
    let (tx, ty) = chrome.to_text(x, y);
    let buffer = state.docs.active();
    let entry = state.gutter.docs.get(&buffer.id())?;
    layout::change_bar_at(buffer, &state.renderer.atlas, &entry.marks, tx, ty)
}

/// Closes the peek when it no longer fits what is drawn: another document,
/// an edit, or a scroll since it was first drawn.
pub(super) fn check_peek(peek: &mut Option<Peek>, buffer: &Buffer) {
    let scroll = (
        buffer.scroll_line,
        buffer.scroll_x(),
        buffer.scroll_fraction,
    );
    let Some(open) = peek.as_mut() else {
        return;
    };
    if open.buffer != buffer.id()
        || !open.text.same_as(&buffer.rope)
        || open.scroll.is_some_and(|s| s != scroll)
    {
        *peek = None;
    } else {
        open.scroll = Some(scroll);
    }
}

/// The text of `path` as of HEAD, with a file HEAD lacks as the empty
/// string, or `None` when it is not in a repository.
pub(super) fn read_head(path: &Path) -> Option<String> {
    path.parent()
        .ok_or_else(|| "no parent directory".to_string())
        .and_then(crate::project::git::toplevel)
        .and_then(|root| crate::project::git::head_text(&root, path))
        .ok()
        .map(|text| text.unwrap_or_default())
}

/// The active document's HEAD text and its changes against it, current to
/// the text in the buffer, or why there are none to act on. The gutter
/// diffs a pause after typing; when it is behind, the diff runs now rather
/// than acting on lines that have moved. With `fresh`, for what writes a
/// HEAD text somewhere, a HEAD that may have moved is read now, rather
/// than acting on the one the gutter still draws; stepping and peeking
/// take that one while the new read is on its way.
fn active_changes(
    state: &mut State,
    fresh: bool,
) -> Result<(Arc<String>, Vec<Change>), &'static str> {
    let State { docs, gutter, .. } = state;
    let buffer = docs.active();
    if buffer.path.is_none() {
        return Err("an unsaved document has no Git changes");
    }
    if buffer.is_preview_file() {
        return Err("a preview has no Git changes");
    }
    if buffer.rope.len_bytes() > GUTTER_MAX_BYTES {
        return Err("no Git changes for files over 2 MB");
    }
    let id = buffer.id();
    if fresh
        && gutter.stale.contains(&id)
        && let Some(path) = &buffer.path
    {
        let epoch = gutter.epoch;
        gutter.land_head(id, epoch, read_head(path));
    }
    let Some(entry) = gutter.docs.get_mut(&id) else {
        return Err("Git has not read this file yet");
    };
    let Some(head) = entry.head.clone() else {
        return Err("this file is not in a Git repository");
    };
    if !entry.text.as_ref().is_some_and(|t| t.same_as(&buffer.rope)) {
        let snapshot = buffer.rope.clone();
        let changes = crate::ide::diff::changes(&head, &snapshot.to_string());
        entry.marks = crate::project::git::marks(&changes);
        entry.changes = changes;
        entry.text = Some(snapshot);
        gutter.dirty.remove(&id);
    }
    Ok((head, entry.changes.clone()))
}

/// What staging the active document's change at `line` hands to Git, or
/// why it cannot. The patch is made of the editor's text, which has LF
/// line breaks and no encoding of its own: a file stored otherwise would
/// stage lines it does not have.
fn stage_job(
    state: &mut State,
    line: usize,
) -> Result<crate::platform::git_panel::StageText, String> {
    use crate::text::file_format::{Encoding, Ending};
    const ELSEWHERE: &str = "stage from Source Control";
    let format = state.docs.active().disk_format();
    if format.preferred != Ending::Lf || format.endings.iter().any(|e| *e != Ending::Lf) {
        return Err(format!(
            "cannot stage one change of a file with CRLF line breaks; {ELSEWHERE}"
        ));
    }
    if format.encoding != Encoding::Utf8 || format.bom {
        return Err(format!(
            "cannot stage one change of a file that is not plain UTF-8; {ELSEWHERE}"
        ));
    }
    let (head, changes) = active_changes(state, true)?;
    let change = crate::ide::diff::change_at(&changes, line)
        .map(|index| changes[index])
        .ok_or("the caret is not on a change")?;
    let buffer = state.docs.active();
    let path = buffer
        .path
        .clone()
        .ok_or("an unsaved document has no Git changes")?;
    let unsaved = if buffer.is_dirty() {
        ", unsaved lines included"
    } else {
        ""
    };
    Ok(crate::platform::git_panel::StageText {
        path,
        head,
        text: buffer.rope.to_string(),
        change,
        done: format!(
            "staged the change at line {}{unsaved}",
            change.new_start + 1
        ),
    })
}

/// Whether Next Change and friends have something to say for the active
/// document: changes, or a diff still to run.
pub(super) fn active_has_changes(state: &State) -> bool {
    let buffer = state.docs.active();
    state.gutter.docs.get(&buffer.id()).is_some_and(|entry| {
        entry.head.is_some()
            && (!entry.changes.is_empty()
                || !entry.text.as_ref().is_some_and(|t| t.same_as(&buffer.rope)))
    })
}

impl EditorView {
    /// Git > Next Change and Previous Change: the caret to the first line of
    /// the next change against HEAD, wrapping around the file.
    pub(super) fn step_change(&self, forward: bool) {
        let target = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let line = state.docs.active().cursor_position().0;
            active_changes(&mut state, false).and_then(|(_, changes)| {
                let index = crate::ide::diff::step_change(&changes, line, forward)
                    .ok_or("no changes since HEAD")?;
                Ok((index, changes.len(), changes[index].new_start))
            })
        };
        let (index, count, line) = match target {
            Ok(target) => target,
            Err(why) => {
                self.say_changes(why);
                return;
            }
        };
        self.caret_to_line(line);
        // An open peek moves along to the change stepped to.
        if self.state().is_some_and(|state| state.peek.is_some()) {
            self.show_change(Some(line));
        }
        self.say_changes(&format!("change {} of {count}", index + 1));
    }

    /// Git > Show Change, and a click on a change's mark in the gutter: the
    /// peek at the change holding `line`, or the caret's, with what HEAD
    /// had there.
    pub(super) fn show_change(&self, line: Option<usize>) {
        let opened = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let line = line.unwrap_or_else(|| state.docs.active().cursor_position().0);
            let found = active_changes(&mut state, false).and_then(|(head, changes)| {
                crate::ide::diff::change_at(&changes, line)
                    .map(|index| (head, changes[index]))
                    .ok_or("the caret is not on a change")
            });
            match found {
                Ok((head, change)) => {
                    let buffer = state.docs.active();
                    state.peek = Some(Peek {
                        buffer: buffer.id(),
                        change,
                        content: layout::change_peek(&head, &change),
                        text: buffer.rope.clone(),
                        scroll: None,
                        hits: None,
                    });
                    Ok(())
                }
                Err(why) => {
                    state.peek = None;
                    Err(why)
                }
            }
        };
        match opened {
            Ok(()) => {
                self.request_redraw();
                self.pump();
            }
            Err(why) => self.say_changes(why),
        }
    }

    /// A click on the mark of the change holding `line`: the caret to that
    /// line, and the peek at the change.
    pub(super) fn click_change_bar(&self, line: usize) {
        if let Some(mut state) = self.state_mut() {
            let buffer = state.docs.active_mut();
            let line = line.min(buffer.rope.len_lines().saturating_sub(1));
            let at = buffer.rope.line_to_byte(line);
            buffer.place_cursor(at, Motion::Move);
            state.completion = None;
        }
        self.show_change(Some(line));
    }

    /// A click on the open peek.
    pub(super) fn peek_action(&self, click: PeekClick) {
        let Some(change) = self
            .state()
            .and_then(|state| state.peek.as_ref().map(|p| p.change))
        else {
            return;
        };
        match click {
            PeekClick::Nothing => {}
            // It stays open: what was staged can still be read.
            PeekClick::Stage => self.stage_change(Some(change.new_start)),
            PeekClick::Revert => {
                // Revert Change acts on the caret's change: this one.
                if let Some(mut state) = self.state_mut() {
                    state.peek = None;
                    let buffer = state.docs.active_mut();
                    let line = change
                        .new_start
                        .min(buffer.rope.len_lines().saturating_sub(1));
                    let at = buffer.rope.line_to_byte(line);
                    buffer.place_cursor(at, Motion::Move);
                }
                self.revert_change();
            }
            PeekClick::Close => {
                if let Some(mut state) = self.state_mut() {
                    state.peek = None;
                }
                self.request_redraw();
                self.pump();
            }
        }
    }

    /// Git > Stage Change, and the peek's Stage: the change holding `line`,
    /// or the caret's, into the index as the buffer has it, unsaved lines
    /// included. Git runs on Source Control's worker, which says how it
    /// went and looks at the repository again.
    pub(super) fn stage_change(&self, line: Option<usize>) {
        let job = {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let line = line.unwrap_or_else(|| state.docs.active().cursor_position().0);
            stage_job(&mut state, line)
        };
        match job {
            Ok(job) => {
                if let Some(mut state) = self.state_mut() {
                    state.git.stage_text(job);
                }
                self.request_redraw();
                self.pump();
            }
            Err(why) => self.say_changes(&why),
        }
    }

    /// Git > Revert Change: the change the caret is in back to what HEAD
    /// has, in the buffer only. One edit, one undo step, the file left
    /// unsaved.
    pub(super) fn revert_change(&self) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let line = state.docs.active().cursor_position().0;
            let found = active_changes(&mut state, true).and_then(|(head, changes)| {
                let change = crate::ide::diff::change_at(&changes, line)
                    .map(|index| changes[index])
                    .ok_or("the caret is not on a change")?;
                let text = state.docs.active().rope.to_string();
                crate::ide::diff::revert_change(&head, &text, &change)
                    .ok_or("the change no longer fits the text")
            });
            let (range, text) = match found {
                Ok(found) => found,
                Err(why) => {
                    drop(state);
                    self.say_changes(why);
                    return;
                }
            };
            let buffer = state.docs.active_mut();
            // Its own undo step, never merged into typing just before it.
            let caret = buffer.cursor();
            buffer.place_cursor(caret, Motion::Move);
            if buffer.replace_ranges(&[(range.clone(), text)]) == 0 {
                drop(state);
                self.say_changes("this document is read-only");
                return;
            }
            state
                .docs
                .active_mut()
                .place_cursor(range.start, Motion::Move);
            state.message = Some(("change reverted to HEAD".to_string(), Instant::now()));
        }
        self.after_edit();
    }

    fn say_changes(&self, text: &str) {
        if let Some(mut state) = self.state_mut() {
            state.message = Some((text.to_string(), Instant::now()));
        }
        self.request_redraw();
        self.pump();
    }
}
