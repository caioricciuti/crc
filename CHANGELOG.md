# Changelog

One entry per tag, written for people who use the editor.

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
