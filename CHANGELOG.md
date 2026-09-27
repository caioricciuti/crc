# Changelog

One entry per tag, written for people who use the editor.

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
