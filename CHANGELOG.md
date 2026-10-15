# Changelog

One entry per tag, written for people who use the editor.

## Unreleased

- Agents mode runs far less Git when left alone. The inspector reads
  its status once a minute instead of every twenty seconds, and sooner
  when a file it shows changes, its repository's refs move, or you
  select a workspace, open a card or press Read Again. Each read is one
  `git status` where it took up to ten Git commands, and the latest tag
  is looked up again only when HEAD or the tags move. The worktree list
  reads the selected workspace's folder each minute and every folder
  every five. A read that finds nothing new no longer redraws the
  window. Left alone with a folder of five repositories, a minute went
  from about 106 Git commands to about 30.
- Agents mode rests while crc is behind other apps or its window is
  covered: no status reads, and the worktree list and pull requests are
  read every five minutes instead of every twenty seconds, so badges
  still move. Coming back reads what is due at once.
- Agents mode: the inspector no longer reads a repository's GitHub
  issues until you ask. Each repository card has Load Issues where its
  issues were; once clicked they are read then and every five minutes,
  and crc remembers it for that repository. The button beside the list
  hides it again and stops the reads. Issues are also no longer read
  while the inspector is hidden.
- Agents mode: in a folder of several repositories, a click on a
  repository's name in the inspector folds its card to one row with its
  branch and a dot for uncommitted work, as last read. A folded
  repository's Git status is not read until you open it again, and the
  fold is kept across launches.
- Agents mode no longer lags with several repositories and their issues
  in the inspector. Cutting a long issue title to fit measured it once
  per character and overflowed the text cache, which then emptied
  itself every frame: with six repositories a frame took about 186 ms.
  Titles are now measured once, the cache keeps half when it overflows,
  and it learns when a frame starts in every mode. The same frame takes
  under 3 ms.
- Terminals you cannot see no longer redraw the window for every bit of
  output. A hidden tab redraws only when something shown for it changes
  (its status, title or a bell), at most ten times a second, and its
  process and folder are read at most twice a second.
- crc opens on Agents mode, with a shell ready in the selected
  workspace. Opening a file or folder with it still opens the editor.
- Source Control: Amend, beside Commit, replaces the last commit
  instead of adding one. Turned on with an empty message, it fills in
  the last commit's message; Commit then reads Amend and folds in what
  is staged, or with nothing staged just rewords it. It turns itself off
  after the amend. It cannot be turned on before the first commit,
  during a merge, rebase, cherry-pick or revert, or once the commit is already on its
  upstream branch, and its tooltip says which.
- Source Control: the commit message completes conventional-commit
  types. Type the start of one (`fe`, `re`) and the rest shows dimmed
  after the caret; Tab takes it. Where several types match, the more
  common one is offered (`re` gives `refactor`).
- Source Control: the commit message shows how long its subject line
  is, such as `52/50`, and turns amber past 50 characters.
- Terminal: a program can copy to the clipboard through OSC 52. Since
  programs that ask for the mouse get it, a drag in Claude Code selects
  its text and copies it this way, and crc dropped the copy, so the paste
  that followed had nothing. The text now lands on the clipboard. A
  program asking to read the clipboard gets nothing back, an empty copy
  leaves the clipboard alone, and copies over 1 MiB are ignored.

## v0.2.0-alpha.24, 2026-10-15

- Terminal: programs that ask for the mouse now get it. crc read the
  request but never passed clicks on, so the buttons of full-screen
  programs (Claude Code's panels among them) did nothing. Clicks, drags,
  the wheel and focus changes now reach the program; Option or Shift
  keeps a click for crc's own selection, and Cmd-click still opens a
  path.
- Git > Blame Heat (off by default, also `blame_heat` in the settings)
  draws a thin violet strip beside the line numbers: strongest for lines
  changed in the last day, fading over a year, nothing for older or
  uncommitted lines. It follows your edits and is worked out again a
  second after typing stops. Hovering a run shows its commit, author, age
  and summary, and whether it is signed; the first time a signed commit's
  tooltip shows, its signature is checked in the background ("signed,
  verified", "signature bad" or "signed, cannot verify here"). Unsigned
  commits draw dashed. Cmd-click the strip, or click it where no change
  mark is, to open File History at the commit that last changed the line,
  with the caret where the line was then.
- Terminal: `Cmd-F` finds text in what a session printed, history and
  screen, with the same find bar as documents laid over the terminal
  (case, word and regex options; `Cmd-G` and `Cmd-Shift-G` or Return
  and Shift-Return step, scrolling the match into view; matches follow
  new output). `Cmd-K` (View > Clear Terminal, and Clear in its menu)
  clears the screen and history, keeping the line you are on; it no
  longer types Ctrl-L into the program, and a full-screen program keeps
  its screen. Works in Agents mode and in split terminals too.
- Git > File History (`Cmd-Option-Y`) opens a read-only tab beside the
  file showing it at its newest commit; Left and Right step to older and
  newer versions, Home and End go to the oldest one and to your working
  text, and Escape closes it, leaving your file and caret as they were.
  Renames are followed, the caret stays on the same code from version to
  version, and the gutter marks what each commit changed. Versions are
  read in the background, nearest first. A strip above it names the
  commit (hash, author, age, summary, "12 of 47") with Older and Newer
  buttons and a timeline to click or drag along; lines a step adds or
  changes are washed in colour and removed lines fade out where they
  were, over about 150 ms.
- Git > Next Change (`Option-F5`) and Previous Change (`Shift-Option-F5`)
  move the caret through the lines that differ from HEAD, wrapping at the
  ends, and say which change of how many it is on. Git > Revert Change
  puts the change at the caret back to its HEAD text in the editor, as
  one undoable edit; the file on disk is only written when you save.
- Click a change mark in the gutter (or Git > Show Change) to see what
  HEAD had there, in a card under the change with Revert and Stage
  buttons; Next and Previous Change move the open card along. Git > Stage
  Change stages just that change, unsaved lines included. It refuses, and
  says so, for files with CRLF line endings and for files whose index
  already differs from HEAD; stage those from Source Control.
- Agents: New Workspace no longer starts from the selected worktree's
  repository, which could quietly make a second workspace for a
  repository that already had one and take over its worktrees in the
  list. Each folder workspace's row shows its folder, so a wrong one is
  easy to see. A folder can hold any number of workspaces, and a
  workspace needs no folder at all: New Workspace offers one "without a
  folder", whose terminals start in your home folder.
- Agents: closing or exiting a terminal shows the next terminal of the
  same workspace, never another workspace's. A workspace whose last
  terminal ends stays selected, saying so, instead of being forgotten.
- The gutter's change marks follow typing again: text typed, Return,
  Backspace and the other key edits never asked for a new diff, so the
  marks stayed as they were until a menu edit or a save.
- Gutter marks no longer blink out after a stage, commit or branch
  switch, and the change commands stay available while Git reads the
  file again.
- Agents: the workspace inspector lists the open GitHub issues of each
  repository under its card (the newest ten, Show all for the rest, with
  labels and age), read with `gh` every five minutes and on Refresh.
  Clicking one opens it in a read-only tab with its comments, with Start
  Task and Open on GitHub above it. Start Task makes a worktree on an
  `issue-N-title` branch (or goes back to the one it made before) and
  starts Claude there, told to read the issue from a private file; the
  issue's text never reaches a command line. Repositories on a server
  list their issues, but Start Task works only on this Mac for now.

## v0.2.0-alpha.23, 2026-10-13

- Agents: a workspace can live on a server. Type `host:/path` (an
  `~/.ssh/config` alias or `user@host`) in New Workspace; its terminals
  and Claude sessions open over `ssh` in that folder, sharing one
  connection per host, and its row and inspector show the host and
  folder. Its repositories and worktrees are listed like a local
  workspace's (branch, last commit, ahead, merged), read over `ssh` in
  the background without ever prompting; a host that does not answer
  says so and keeps the rows it had. Its inspector shows the same Git
  cards as a local one, and Commit All, Pull, Push and Merge base In run
  on the host. Worktrees work there too: a new task makes its worktree
  on the host, beside the repository in `<repo>-worktrees`, and opens a
  terminal in it; Remove, Clean Up, Clean Up All Merged and Prune run on
  the host with the same checks and the same confirmation. Pull requests
  of a repository whose origin is on GitHub show as badges and in the
  inspector, asked of `gh` on this Mac by the repository's name, and can
  be created and merged from crc. Whatever the login shell on the host
  (bash, zsh, csh, fish), crc's commands run there with `sh`. A host ssh
  cannot reach keeps its terminal open with ssh's error and the hosts in
  `~/.ssh/config`, and New Workspace says when a typed host is not one of
  them. A remote terminal never counts files on this Mac for review. Not
  over `ssh`: Review, and merging a branch into a checkout.
- Agents mode no longer stalls while agents write: counting the files to
  review read every checkpoint on the main thread, which with a large
  review history held the window for seconds. The count now reads one
  listing per session, on a worker.
- Pull request badges refresh on their own: every 20 s while checks run,
  60 s with open pull requests, less often otherwise, and right after a
  push of the branch from anywhere. A failed `gh` keeps the last badge.
- In Agents mode a toolbar button shows or hides the workspace
  inspector (also View > Workspace Inspector, `Cmd-Option-I`).
- Leaving Agents opens the selected workspace's folder in the editor,
  and never opens the terminal panel by itself.
- Removing a workspace takes the worktrees under it out of the list and
  closes their sessions. Nothing on disk changes.
- Closing a terminal tab always ends its program, also one that ignores
  the hangup.
- After crc is reinstalled while open, every new terminal says that Git
  signing and SSH keys fail there until Help > Restart to Update.
- Edit > Copy as One Line (`Cmd-Option-C`, also in the terminal's
  right-click menu) copies a terminal selection as a single line: the
  line breaks and indentation a program such as Claude Code puts in to
  fit a long command to the width go, so the command pastes and runs
  whole. A word split at the edge is joined back without a space.
  Plain `Cmd-C` is unchanged.

## v0.2.0-alpha.22, 2026-10-12

- A terminal inside `ssh` says so: its tab is titled `ssh <host>`, its
  workspace row shows a server icon and `ssh user@host · 5 min`, the
  inspector shows the session (host, user, port, how long) instead of
  this Mac's Git, and the status line names it. Read from the ssh
  command line and `~/.ssh/config` only; nothing is asked of the server.
- The Agents list keeps the order you give it: pinned first, then the
  order you dragged, then the rest as they were made. A session waiting
  on you or a pull request's checks no longer move a row.
- When crc stops answering for more than a second and a half, it writes
  where it was stuck to `~/Library/Logs/crc/hang-<time>.txt` (with
  macOS's `sample`, at most once a minute), so a stall can be traced.
- The workspace inspector scrolls, so a folder of ten repositories shows
  all of them.
- Closing a workspace's last terminal (or typing `exit` in it) removes
  the workspace. When the terminal showing closes, the next one shown is
  the same workspace's, not whichever slid into its place.
- `Cmd-1` to `Cmd-9` show that tab, `Cmd-9` the last: in Agents mode the
  selected workspace's terminals, in the editor its documents.
- In Agents mode the workspace follows the terminal showing: `cd`
  somewhere else and the workspace is that folder now (named after it,
  unless you named it yourself), the inspector shows the Git repository
  you are in, and the status line shows the folder and its branch. The
  editor's project menu and the document's position leave the toolbar
  and status line while Agents mode is on.
- Problems in the editor are marked with a smooth wave under the text,
  which no longer covers underscores; it used to be a row of broken
  dashes. Underlined and struck-through text in the terminal uses the
  font's own underline position and a hairline stroke.
- Agents mode keeps one list of workspaces for the whole app instead of
  following the folder open in the editor. Opening another folder in the
  editor no longer changes the list or what is selected. The workspaces
  kept per project until now are carried over into the one list.
- New Workspace (`Cmd-N` in Agents mode) asks for a folder first; the
  other choices are the selected workspace's folder and a new branch of
  its repository. A double-click under the list uses the selected
  workspace's folder.
- A workspace shows every Git repository in its folder: the folder
  itself and the folders directly in it. The inspector has a card per
  repository with its branch, remote and uncommitted work, and Commit
  All, Pull and Push for that repository. The worktrees of those
  repositories are listed under the folder workspaces.
- A new workspace in Agents mode starts with its own terminal and
  nothing else. It could take the name, pin or terminals of a workspace
  removed earlier under the same name, and terminals left without a
  workspace moved into whichever one was made last.

## v0.2.0-alpha.21, 2026-10-12

- The Agents sidebar is one row per workspace, as in cmux and
  supacode: its name, and under it what a session waiting on you asks,
  what a working agent says, its team's open tasks, or its terminals,
  commits ahead and folder. At the right, a count of sessions waiting
  on you and the pull request number coloured by its checks; hover
  shows the menu button. Sessions are the terminal's tabs and no longer
  repeat in the sidebar. A spinner means an agent said it is working,
  not that a terminal printed something.
- A single click on the empty part of the list does nothing; a
  double-click makes a workspace. The list no longer reorders while the
  pointer is over it.
- Drag a workspace to put it where you want; Pin to Top (its menu) keeps
  it under the main checkout. A double-click on a workspace, or Rename in
  its menu, edits its name in the row: a folder workspace takes the
  name, a worktree keeps its branch and shows your name over it. The
  order, pins and names are kept per project. `Cmd-N` in Agents mode is
  New Workspace.
- Terminals come back after a reboot. Sessions already outlived quitting
  crc; now the ones whose helper ended (a restart of the Mac, a crash)
  are started again at the next launch, in the same folder and
  workspace, and where Claude Code was running `claude --continue` is
  typed in, which resumes the latest conversation in that folder.
- Two terminal sessions started in the same microsecond no longer get
  the same id. They could, and then the second took over the first's
  connection, leaving the first blank; starting terminals again after a
  reboot starts several at once.

## v0.2.0-alpha.20, 2026-10-11

- Agents mode opens terminals, not Claude: `Cmd-T`, the tab bar's +, a
  workspace's +, a new workspace and switching to Agents mode all start
  your login shell in the workspace, and `claude` typed there starts
  Claude Code as usual. The workspace and session menus still offer a
  Claude session directly.
- Every terminal in Agents mode belongs to a workspace. In a folder
  outside Git the folder becomes a workspace, named after it, the first
  time a terminal needs one, and a click on the empty part of the
  workspace list makes a new workspace with a terminal in it.
- The workspace inspector, at the right of Agents mode (`Cmd-Option-I`
  hides it): the selected branch against main (ahead, behind, lines
  changed), its upstream (to push, to pull), its uncommitted files, and
  for the main checkout the latest tag. Buttons commit everything with
  a message typed in the palette, merge main in (never a rebase; a
  conflict leaves nothing changed), pull and push. With `gh` it shows
  the pull request with its author, commits, lines and every check, and
  opens or merges it (squash, merge commit or rebase, asked each time
  and confirmed). The workspace menu has the same operations.
- The workspace list puts what needs you first after the main checkout
  (a session waiting on you, failing checks, a pull request ready to
  merge) and is read again every minute while Agents mode is on.
- crc notices when it is installed again while open. macOS then stops
  giving the running copy crc's permissions (the microphone, other
  apps' data such as an SSH agent's keys, so signed commits fail in its
  terminals), so the status line says so and Help > Restart to Update
  reopens crc, sessions kept. `scripts/bundle.sh --install` says the
  same when crc is open.

## v0.2.0-alpha.19, 2026-10-11

- Agents mode lists workspaces, cmux style: every worktree of the open
  repository as before, plus workspaces made by hand on any folder
  (`Cmd-Shift-N` offers the folder first and, in a repository, a task
  on a new branch second). Sessions remember the workspace they were
  started in, through a quit and reopen too. Folder workspaces are
  kept per project under `~/Library/Application Support/crc/agents/`.
- Agents mode: `Cmd-T` and the tab bar's + start a Claude session in
  the selected workspace; the activity bar is hidden so the list starts
  at the window's edge; a right-click in the sidebar gives the Agents
  menu (session: new session beside it, close; workspace: the task menu,
  plus rename and remove for a folder workspace; the list: new
  workspace, refresh) instead of the file tree's.
- Terminal: `Cmd-A` selects everything the session printed, and a drag
  held past the top or bottom of the screen scrolls the history under
  the pointer while the selection follows.
- The microphone works for programs in a terminal session (Claude
  Code's voice mode): the app now carries the audio-input entitlement
  and a microphone usage description, so macOS asks once for crc and
  then lets them record. Before, it refused silently ("No audio
  detected from microphone").

## v0.2.0-alpha.18, 2026-10-10

Sessions that outlive the window, drop and paste for agents, and the
agent's own word on its state.

- Terminal sessions outlive the window. Each one runs in a helper process
  of its own (`crc --hold`), so quitting crc, a crash or Restart to Update
  leaves the shell or agent running; the next crc opened on the same
  project takes the sessions back, screens and history included, with the
  tab that had the keyboard active again. Closing a tab still ends its
  session, and a window opened on a single file keeps nothing, as before.
- Files and images can be dropped on the window. Dropped on the
  terminal, their paths are typed into the session, quoted, so an
  agent can read a screenshot; dropped on the editor, they open. An
  image with no file behind it (dragged from a browser) is saved as a
  PNG under `~/Library/Application Support/crc/drops/` first, kept a
  week. `Cmd-V` with an image on the clipboard does the same in a
  terminal tab: a Claude tab takes it straight from the clipboard, any
  other gets the saved PNG's path.
- The terminal speaks the Program Status Protocol (OSC 7501), which
  Claude Code 2.1.295 and later report through: a session is marked
  working, waiting (a permission, a question, a login, a finished
  turn) or failed from the agent's own word, the moment it changes,
  with the agent's message on the row and in the status line. The
  hooks and the output timer stay as the fallback for programs that
  do not report.
- Agent teams (Claude Code's `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS`):
  the Agents sidebar lists a session's shared task list under it, each
  task with the teammate on it, checked when finished, and says which
  teammates have gone idle. The list folds away once every task is
  done. Read from the TaskCreated, TaskCompleted and TeammateIdle
  hooks crc passes to Claude.

## v0.2.0-alpha.17, 2026-10-10

Agents mode: run several agents at once, each on a branch of its own.

- The Editor / Agents switch in the toolbar (`Cmd-Shift-A`) gives the
  terminal the whole editor column and turns the sidebar into a list of
  tasks. A task is a Git worktree of the open repository: the repository
  itself, plus one per task, so agents never edit the same files. Under
  each task are its sessions, marked waiting, working or idle; the
  terminal's tabs are that task's. Nothing stops when you switch back.
- New Task (`Cmd-Shift-N`, or + in the list) takes a name, makes a branch
  of that name from HEAD in a worktree under
  `~/Library/Application Support/crc/worktrees/`, and starts Claude in it.
- An agent that finishes its turn, asks a question or rings the bell
  shows as waiting, with a count on the Dock icon and a bounce when crc
  is in the background. `Ctrl-1` to `9` show a task, `Cmd-Shift-U` the
  next session waiting on you.
- A task's menu (the ... on its row) opens it in the editor, starts
  another Claude session or a shell in it, merges its branch into the
  main checkout, pushes it and opens a pull request with `gh` in a shell
  of its own, or removes it. Removing refuses while the worktree has
  uncommitted changes and keeps a branch with unmerged commits.
- `Cmd-\` in Agents mode puts two of a task's sessions side by side (a
  new shell when it has only one); `Cmd-Option-]` moves the keyboard
  across and `Cmd-Option-W` goes back to one.
- A task's row says how many commits it has that the main checkout does
  not, or, with `gh` installed, its pull request and whether its checks
  pass; the menu then opens the pull request.
- Cmd-click on a path an agent printed opens it in the editor.

## v0.2.0-alpha.16, 2026-10-10

crc installs its own updates now, and agent review also lists files an
agent changed with shell commands.

- Help > Check for Updates downloads a newer release and checks it before
  offering it: its SHA256SUMS, a signature from crc's developer with crc's
  bundle id, Gatekeeper's notarization check, and a version newer than the
  running one. Then Help > Restart to Update, or the row on Home, quits the
  way Cmd-Q does (unsaved documents ask first), puts the new crc in place
  of the old one and opens it. The daily check still only announces, and
  nothing installs without that click. This is the last release you need
  to install from the DMG by hand.
- Files changed while a terminal is busy, but not through the agent's file
  tools (a shell command, an agent without hooks), show in Review with the
  note that no copy from before was kept. They are compared with Git's
  staged copy and can be kept, not undone. Files Git ignores are left out.

## v0.2.0-alpha.15, 2026-10-09

What an agent changes in a crc terminal can now be reviewed and undone,
change by change, and settings get a page of their own.

- The Claude tab crc starts (Cmd-Shift-C) keeps each file as it was
  before Claude's first edit to it. The hooks that do it are passed with
  `claude --settings`, so nothing in ~/.claude changes; the copies stay in
  crc's folder for 14 days. `agent_review = false` turns it off.
- Git > Review Agent Changes, also in the palette and on Home, shows
  every file a session changed against that copy. Keep a change and it
  leaves the review; Undo it and it comes out of the file. By hunk, by
  file, or a whole session at once; undoing a whole file or session asks
  twice. Undo is refused while the file has unsaved edits in crc, and an
  open file reloads with the agent's version one Cmd-Z away.
- A terminal tab says how many of its files wait for review, and Claude's
  questions mark its tab the way terminal notifications do.
- Cmd-, opens a Settings page: every setting by group, what it does, its
  value and its default. Edit opens config.toml at that setting; saving
  the file updates the page.
- Cmd-W on the last tab goes back to Home instead of closing the window
  and quitting.

## v0.2.0-alpha.14, 2026-10-05

MCP servers get a page instead of an empty JSON file, and what goes
wrong is said in the status line.

- Opening MCP Servers shows a page in the editor column: what a server
  is, a card per server with its command or URL, its status and
  Start/Stop, and a server's tools, resources and prompts with their
  descriptions. Close, Escape or a tab gives the column back; the list
  stays in the sidebar.
- Add Server picks an installed program with the Open panel and Add by
  URL takes a Streamable HTTP address in the palette; both write the
  entry to mcp.json and show it. Nothing starts until Start is clicked.
- With no servers the page says how to add the first one and shows the
  file's shape, which is Claude Code's .mcp.json shape.
- Saving mcp.json in crc reads it again at once; the status line says
  how many servers it holds, or what is wrong with it.
- In the sidebar a server's name shows its details and only its
  Start/Stop starts or stops it.
- Push, pull or fetch that fails because ssh's key agent lives in another
  app's container (Secretive) now says so: crc needs Full Disk Access,
  and where to give it. Before, the note was ssh's "Load key" line.
- Things that went wrong quietly are said in the status line: a line in
  config.toml that cannot be used (with its line number, at launch and
  on save), a project folder that cannot be read, a terminal program
  that exited with an error code, and blame that Git could not give.
  Failures are red and stay longer whatever their wording.
- The status line shows the document's language server by name once it
  has answered, and says when it stopped. Before, only its start had a
  spinner.

## v0.2.0-alpha.13, 2026-10-02

A window that looks and answers like one app: every button, row and
menu reworked, and the Save panel crash fixed.

- Cmd-S on a document never saved, Save As and New File no longer quit
  the app when macOS gives no Save panel: the status line says so and
  the text stays, unsaved.
- One set of controls everywhere: buttons, toggles and fields share one
  height and corner, light up under the pointer, show when held, and dim
  when they cannot act. Icon buttons say what they do, with their
  shortcut, when the pointer rests on them.
- Source Control is rebuilt. Refresh and a menu of every Git command sit
  in the header; Pull and Push sit beside the branch and show how many
  commits it is behind or ahead; Commit says what it will commit, shows
  Cmd-Return, and says why when it cannot run. Sections fold and offer
  Stage All or Unstage All; each file shows Open and Stage under the
  pointer, its status letter at the right. A clean tree and a folder
  that is not a repository each get a plain explanation.
- Right-click menus of their own: a changed file (open it, open its
  changes, stage or unstage, copy its path, reveal it), a Source Control
  section, the terminal (copy, paste, clear), the empty tab strip, and
  the Extensions and MCP Servers lists. A right-click in those panels no
  longer acts on the file tree hidden behind them. Menus show the same
  shortcuts as the menu bar, and Move to Trash and Close All Tabs sit
  apart from their neighbours.
- Loading shows as it happens: placeholder rows while the extension
  registry or the repository is first read, a progress bar under the
  panel's header, and a spinner in the status line while a push, pull or
  fetch runs or a language server starts.
- The status line marks a failure in red with an icon and keeps it on
  screen longer; a success gets a check. Git errors lead with the line
  Git itself marks as the error, the rest in the tooltip.
- The sidebar header is one row, the panel's name with its buttons
  beside it. The change count on Source Control's icon is a small badge
  that no longer covers the icon.
- The palette has a search glyph and an esc key that closes it; the
  terminal header has New Terminal and Hide Panel; the find bar's arrows,
  close and options are the shared controls; tabs are sized by their
  titles; the Extensions page shows each extension with its icon and
  what it may do on cards.

## v0.2.0-alpha.12, 2026-10-02

Workspaces, MCP servers you can use yourself, crc as an MCP server for
agents, and a terminal that knows which session waits on you. A folder
that holds several repositories and the notes about them opens as one
project.

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
- `private_markers` in `.crc/workspace.toml` (text separated by `|`)
  turns on a check before each commit from Source Control: a staged line
  holding a marker, or the workspace's own path, stops the commit, and
  the status line names the file, line and marker.
- File > New Workspace writes the notes a workspace starts with into a
  folder you pick (`AGENTS.md`, a `CLAUDE.md` that reads it, a state doc,
  a log, `runbooks/` and `.crc/workspace.toml`), never over an existing
  file, and opens it. A folder inside a repository is refused.
- MCP servers you can use yourself (View > MCP Servers, or the plug in
  the activity bar). `~/.config/crc/mcp.json` lists them in the shape
  Claude Code's `.mcp.json` uses; nothing starts until its name is
  clicked. A running server lists its tools, resources and prompts. A
  tool or prompt opens a call document, JSON with a placeholder for each
  argument, and Cmd-Return (Run > Run MCP Call) runs it into an answer
  tab; Cmd-Return there runs it again, and a saved call runs the same
  way. A resource opens at once. A tool the server marks as able to
  change or delete things asks first.
- Servers started through `npx`, `bunx`, `uvx`, `pnpm dlx` or another
  command that downloads code when it runs are refused, with the reason.
  Both protocol eras are spoken: the stateless revision of 2026-07-28 and
  the earlier `initialize` handshake.
- An entry with a `url` and no `command` is reached over Streamable HTTP,
  through the system curl: plain JSON or event-stream replies, and a
  legacy server's session kept. A server that needs a sign-in says so;
  crc does not sign in yet.
- Run > Save MCP Call to Workspace writes the call into the workspace's
  `calls/` folder, never over another; Home lists saved calls, and a
  click runs one. Refused when the open folder is itself a repository,
  where the call would be committed.
- The terminal knows when a session waits on you. A program that sends
  a notification (OSC 9, as iTerm2 takes it, OSC 777 `notify` or kitty's
  OSC 99) or rings the bell marks its tab with a dot until you type into
  it, and the status line says which session asked and what, when you
  are not already typing there. Claude Code sends these with its iTerm2
  notification channel; Codex with its terminal notifications turned on.
- Home lists the terminal sessions first, the waiting ones on top with
  their question, the others as working or idle; a click opens one.
- `crc --mcp [folder]` runs no window: it answers MCP on stdio for the
  folder, in both protocol eras, so any agent can ask what the editor
  knows. Its tools only read: `project.symbols` and `project.files` from
  the project index (built on the spot when the app has not built it),
  `project.read` for lines of a file inside the folder, and
  `workspace.state` and `workspace.repos` for the workspace's state doc,
  what waits on you, and each repository's branch and changes. Run >
  Copy crc MCP Server Command puts the line for the open folder on the
  clipboard; crc never edits another tool's settings.

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
