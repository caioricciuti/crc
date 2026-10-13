//! The runs of lines the gutter marks, from the Git menu: stepping through
//! them and reverting one to what HEAD has.

use super::*;
use crate::ide::diff::Change;

/// The active document's HEAD text and its changes against it, current to
/// the text in the buffer, or why there are none to act on. The gutter
/// diffs a pause after typing; when it is behind, the diff runs now rather
/// than acting on lines that have moved.
fn active_changes(state: &mut State) -> Result<(Arc<String>, Vec<Change>), &'static str> {
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
            active_changes(&mut state).and_then(|(_, changes)| {
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
        self.say_changes(&format!("change {} of {count}", index + 1));
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
            let found = active_changes(&mut state).and_then(|(head, changes)| {
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
