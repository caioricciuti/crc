# Changelog

One entry per tag, written for people who use the editor.

## Unreleased

Workspaces: a folder that holds several repositories and the notes about
them opens as one project.

- Opening a folder finds the Git repositories in it, up to two levels
  down. Source Control shows one at a time and names it in a menu at the
  top; the menu, or Git > Switch Repository, shows another. A plain
  repository works as before, with no menu.
- Blame in the status line asks the repository the file is in, so it
  works for files in a repository nested inside the open folder.
- Home is the workspace's front page. Waiting on You lists the items
  under the state doc's "waiting" heading; Repositories shows each one's
  branch and changes, a click opening it in Source Control; Since You
  Were Last Here lists commits and edited notes since the folder was
  last opened. Start Session opens the state doc, End Session the log
  and the state doc.
- `.crc/workspace.toml` in the open folder names the state doc, the log
  and the heading to read (`state`, `log`, `waiting_heading`). Without
  it, `docs/state.md` or `STATE.md` and `docs/log.md` or `LOG.md` are
  used when they exist. When the folder was last opened is kept in
  crc's own folder, never in the workspace.

## v0.2.0-alpha.11, 2026-10-01

Branches you can rename and delete, a way out of a merge or rebase, and
pulls for branches that have diverged.

- Git > Rename Branch and Delete Branch, also in the palette. A rename
  keeps the branch's upstream; a delete never offers the branch you are
  on, and asks before deleting one whose commits are on no other branch.
- The branch at the top of Source Control and the one in the status bar
  open the branch list.
- Git > Abort Merge or Rebase gives up a merge, rebase, cherry-pick or
  revert after asking, and Continue Rebase goes on once the conflicts are
  resolved, without opening an editor.
- Pull stays fast-forward only; when the branches have diverged it says
  to use the new Pull with Rebase or Pull with Merge.
- Conflicts Side by Side shows a checkmark while the columns are up.
- Cmd-Shift-P (File > Command Palette) opens the palette on its commands.
  The toolbar box says it searches files and commands.
- The Extensions list shows that it is checking the registry, instead of
  growing when the registry answers.

## v0.2.0-alpha.10, 2026-10-01

Markdown that hides its syntax in any language, and a preview you can
size or give the whole tab.

- Markdown hides its syntax and lines up table columns on lines with
  accents, CJK or emoji too, not only on plain ASCII lines. Bold shows
  there as well; italic on those lines is still to come, and a line
  with right-to-left text keeps its syntax visible.
- A table's delimiter row runs on in dashes to its pipes, and a quote's
  `>` is drawn as a bar while you are not editing that line.
- Lines with accents, CJK or emoji now keep to the same columns as the
  lines around them; they used to come out about 2% narrower, which put
  table pipes out of line.
- The divider between the text and the Markdown preview drags, so either
  side can have more room.
- View > Markdown Preview Only (Cmd-Shift-E) shows the preview in the
  whole tab; again, or Escape, puts it back beside the text.

## v0.2.0-alpha.9, 2026-09-30

A fix for editing with the find bar open.

- Clicking in the text while the find bar is open now lets you edit the
  file; before, typing kept going into the search field. The bar stays
  open with its matches highlighted. Cmd-F or a click on the search field
  puts the keyboard back there, with your query and options kept, and the
  focus ring shows which one has it. Opening a file from the Explorer
  gives that file the keyboard too. Escape still closes the bar.

## v0.2.0-alpha.8, 2026-09-30

Markdown that reads like the page it becomes, fixes for saving, Git and
renaming, and a great deal of internal cleanup behind the same editor.

- Markdown hides its syntax on every line the caret is not on: emphasis,
  code and strike markers, link targets and heading hashes take no room,
  and the line you are editing shows everything. Table cells line up with
  their column. Lines with non-ASCII text still show their syntax for now.
- Markdown with a long run of `*`, `_` or `~` no longer freezes the
  editor or the Extensions page, and an underline under a paragraph of
  several lines makes it a heading, as CommonMark reads it.
- A save that failed in a file with mixed line endings no longer gives
  lines the wrong endings on the next save.
- In a repository with no commits yet, unstaging a file you edited since
  adding it works.
- Renaming `café.txt` in the sidebar selects `café`, not `café.`.
- A crash folder that could not be read whole is offered once, then kept
  aside instead of being offered at every launch.
- Go to line, rename and the commit message have Home, End, forward
  delete and Option word moves, like the find bar.
- The Extensions page no longer draws past the bottom of a short window.
- A large JSON response in an HTTP tab no longer stalls the window the
  first time it shows: it is formatted before it arrives.
- Replace in Project keeps saying how many files are open and unsaved
  when some also failed. HTTP responses and file completions show sizes
  the way the status line does.

## v0.2.0-alpha.7, 2026-09-28

Fixes where two copies of the same path had drifted apart, a faster
frame, and extensions that are checked, bounded and kept apart.

- Frames take about half the time they did: the rows on screen are worked
  out once a frame instead of up to ten times.
- Typing, deleting and pasting at several cursors keeps folds open or
  closed as they were, recolours only what changed, and keeps your main
  cursor as the main one.
- Selections: a selection that ends at the start of a line now marks the
  newline it covers, an empty line selected that way shows, and no stray
  mark appears at the end of the line above. The find bar and the commit
  message now show their selection.
- Go to definition and hover answer about the text as it is now, not as it
  was before your last few keystrokes. Replace in project updates the Git
  gutter marks and the language server.
- "Save your changes?" treats an unexpected answer as Cancel, never as
  Don't Save. The quit and close prompts show the tab they are asking
  about.
- Clicks in the sidebar while naming a new file land on the right row. The
  mouse wheel no longer loses part of a notch, and each view keeps its
  own scroll remainder.
- Markdown: a tab after `#` makes a heading, and tables need a proper
  delimiter row, the same in the text and in the preview. A remote image
  in Markdown Preview shows as its alt text in a small frame, since the
  preview loads nothing from the network.
- The preview pane appears once its page has loaded, not blank before it.
- Extensions: an installed extension whose files changed on disk is
  marked, loses its commands and is never shown as signed. A command gets
  at most 8 MB of text. Previews run apart from commands, and an extension
  that fails three times in a row is turned off until you turn it back
  on. An interrupted update no longer loses the extension.
- The extension engine now passes the official WebAssembly test suite for
  what it supports and refuses modules that use features it does not.
- Committing with a long message, or a commit Git refuses, reports Git's
  own error instead of a broken pipe.

## v0.2.0-alpha.6, 2026-09-27

Markdown, rendered beside the text again, this time from an extension.

- Install Markdown Preview from crc > Extensions, then press Cmd-E in a
  Markdown file: the rendered page opens beside the text. It follows your edits, keeps its scroll position, loads images
  from the document's folder, and follows crc's light or dark theme.
  CommonMark with tables, task lists, strikethrough and footnotes.
- The page runs no script, loads nothing from the network, reads files
  only from the document's folder and follows no links. Extensions ask for
  this with a new capability, `preview.show`, shown before install.

## v0.2.0-alpha.5, 2026-09-27

Markdown is text again, and extensions are closer to hand.

- Markdown files open in the editor as text, styled where it is written:
  headings in bold, bold and italic in the editor font's own faces, inline
  code on a tint, links and list markers set apart, and the characters
  that make the syntax (`#`, `**`, backticks, table pipes) drawn faint.
  Fenced code blocks sit on a band and are coloured by their language;
  YAML front matter too. Selection, find, multiple cursors and scrolling
  work as in any other file.
- The rendered Markdown view (Cmd-E) is gone. It could not select text and
  scrolled a block at a time. A rendered preview is coming as an
  extension; until then Cmd-E says so.
- Right-click in the editor: Extensions, then an extension, then its
  commands. The Extensions menu groups commands the same way.
- The Extensions page opens on an overview with Install from Folder,
  Refresh and Close; an extension's own page has a way back, and its
  README is rendered, tables and code included, and scrolls.
- Breadcrumbs are clickable: a folder lists what is in it, the file lists
  the files beside it. Pick a file to open it, a folder to show it in the
  Explorer.
- Home no longer shows "Untitled" under its tab.

## v0.2.0-alpha.4, 2026-09-27

A release of fixes: a full review of the code found close to two hundred
problems, and all of them are fixed but one on the release process, left
for later.

- No more crashes from: pressing Up after going to a line inside a fold,
  clicking right of a wrapped Chinese or Japanese line, a language server's
  malformed reply, or an extension that declares its functions wrong.
- Edits that could be lost or damaged are safe: formatting a CRLF file no
  longer doubles its line endings, a crash-recovery file cut short is never
  offered back, a settings file with one odd byte is no longer rewritten,
  a rename from the language server is not applied over text typed since,
  and saving a hard-linked file or a file in a read-only folder is safe.
- Home is a page: typing there no longer makes an Untitled document.
- A Source Control diff opens in a tab of its own, named after the file.
- Git: staging a renamed file, fetches with a lot of output, diffs of
  lines that start with `--`, conflicts shown as a plain diff, a new
  repository's gutter, and repositories whose Git folder is elsewhere
  (worktrees, a project inside a repository) all work.
- With Source Control open, typing, Paste and Undo go to the document
  until you click the commit message. Cmd-W in a split closes the pane,
  not the window; a tab's close button works on any tab.
- Much less waiting: find in large files, project search, references,
  renames across many files, branch lists, big reloads, the Git gutter,
  Markdown previews, symbols and undo no longer stall the window.
- Rendering: long words and wide code in Markdown previews, selections of
  empty lines, the scrollbar with wrapping or folds, split panes showing
  two files, italic letters and CJK text while scrolling.
- The terminal answers colour queries, sends F1-F12, keeps history when
  the panel shrinks under full-screen programs, and a pasted wall of text
  no longer freezes the editor.
- .http requests are limited to http and https, and a body file must be in
  the project.
- Extensions run inside a tighter budget, and the registry cannot be
  replayed with an older signed list.
- crc no longer offers itself in Open With for every kind of file, only
  for text and folders.

## v0.2.0-alpha.3, 2026-09-26

- Extensions. They are WebAssembly run by crc's own interpreter, and each
  can do only what it declares: read and replace the selection or the
  document, nothing else. The Extensions icon in the new strip (or
  Cmd-Shift-X) lists what is installed and what the signed registry
  offers; installing shows exactly what an extension may do and waits for
  your yes. Their commands are in the Extensions menu and the palette.
  Three official ones to start: Sort Lines, Change Case and Encode and
  Decode. Install from Folder… installs your own, unsigned and marked so.
- An icon strip at the window's left: Explorer, Source Control (with a
  count of changed files) and Extensions. Clicking the panel that is
  showing hides the sidebar; the strip stays. It replaces the Explorer and
  Source Control switch at the top of the sidebar.
- Code actions from the language server: a lightbulb in place of the line
  number where the caret rests on a line with fixes, and Go > Quick Fix
  (Cmd-.) to list them in the palette, preferred first. Go > Organize
  Imports (Shift-Option-O), and `organize_imports_on_save` in the
  settings, which runs before format-on-save.
- A rename's edits now refresh the Git gutter marks of the open files
  they change.

## v0.2.0-alpha.2, 2026-09-26

- Merge conflicts: a Conflicts group in Source Control, `MERGING` and
  friends in the status bar, and each conflict resolved in the file (Accept
  Current, Incoming, Both or Base on its first line) or side by side in
  aligned columns. Mark Resolved saves and stages the file once no markers
  are left. Git > Next Conflict and Previous Conflict; `conflict_view` in
  the settings picks the view a file opens in.
- Git status is re-read when the repository changes with Source Control
  closed too, so the status bar branch and the gutter marks stay current
  after a commit, merge or checkout in the terminal.
- The disk image is now `crc.dmg` in every release, and the site's Download
  button fetches it directly.

## v0.2.0-alpha.1, 2026-09-24

The first build meant for other people. Apple Silicon, macOS 14 or later,
signed and notarized.

- A syntax palette with eight distinct hue families, indent guides, a
  bracket-pair highlight, and Git marks in the gutter for added, changed and
  deleted lines.
- Files changed by another program, Claude Code included, reload in their
  tab when it is clean; Save asks before overwriting when it is not, and a
  deleted file marks its tab unsaved. File > Revert to Saved.
- A draggable overlay scrollbar in the editor.
- Font zoom (`Cmd-=`, `Cmd--`, `Cmd-0`) and a settings file
  (`~/.config/crc/config.toml`, crc > Settings) with `font`, `font_size`
  and `theme`.
- Light appearance, following the system or the `theme` setting.
- A native home screen with recent projects.
- TOML, YAML and shell highlighting.
- Earlier in September: language servers, split panes, the terminal panel,
  Claude Code inside the editor, the command palette, local Git with hunk
  staging, `.http` requests, project watching.

Known limits are listed in the README under "What does not".
