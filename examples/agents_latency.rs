//! Agents mode's gate: can a keystroke into the terminal become a finished
//! frame inside a 120Hz budget, with a long workspace list beside it and an
//! inspector full of repositories and their issues?
//!
//! Each iteration does what output in the active terminal does in the
//! running app: the screen takes a byte, then the frame Agents mode draws
//! (the workspace list, the inspector, the terminal) is laid out, encoded,
//! and waited on until the GPU reports it complete. As in the window, the
//! atlas's own frame is advanced once per frame.
//!
//! The fixture is built through the library alone: twelve folder
//! workspaces, eight terminals, the selected workspace holding six
//! repositories with their status read and fifteen open issues each. No
//! Git, no `gh`, no network, no processes behind the terminals.
//!
//! Honest scope: as `frame_latency`, our work from the byte to GPU
//! completion. The activity bar and status line are left out, and the
//! pointer is nowhere, so nothing is hovered.
//!
//! Run with: cargo run --release --offline --example agents_latency
//! (`-- --repos N` repositories in the selected workspace, `--no-issues`,
//! `--workspaces N`, `--hidden-streaming N` for that many background
//! terminals printing between samples, every tenth line retitling its tab
//! as an agent's spinner does; the redraws they ask for are counted under
//! the window's rule, [`terminal::Pace::decide`]).

use crc::platform::agents::{self, IssueSet, Panel, RepoKey, Workspace};
use crc::platform::inspector::{self, Issues};
use crc::platform::terminal::{self, Launch, Tab};
use crc::project::worktree::{Issue, Status};
use crc::render::font::Atlas;
use crc::render::layout::{self, Theme, Viewport};
use crc::render::metal::{GlyphInstance, Renderer};
use crc::term::pty::Session;
use objc2_metal::MTLCreateSystemDefaultDevice;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// One 120Hz frame.
const BUDGET_MS: f64 = 1000.0 / 120.0;
/// The most UI labels a warm frame may shape: anything on screen that did
/// not change should come from the cache.
const RESHAPE_LIMIT: u64 = 20;
/// Twelve days after the first issues were opened, in Unix seconds.
const NOW: u64 = 1_790_856_000 + 12 * 86_400;
/// The terminals, as the defaults have them.
const TABS: usize = 8;
/// Of those, the selected workspace's.
const SHOWN_TABS: usize = 2;

const REPOS: [&str; 6] = ["seedling", "trellis", "compost", "mulch", "loam", "kiln"];

struct Options {
    repos: usize,
    issues: bool,
    workspaces: usize,
    hidden_streaming: usize,
}

impl Options {
    fn parse() -> Options {
        let args: Vec<String> = std::env::args().collect();
        let number = |name: &str, default: usize| {
            args.iter()
                .position(|a| a == name)
                .and_then(|at| args.get(at + 1))
                .map_or(default, |n| {
                    n.parse()
                        .unwrap_or_else(|_| panic!("{name} takes a number, not {n}"))
                })
        };
        Options {
            repos: number("--repos", 6),
            issues: !args.iter().any(|a| a == "--no-issues"),
            workspaces: number("--workspaces", 12).max(1),
            hidden_streaming: number("--hidden-streaming", 0),
        }
    }
}

// ---- the fixture -----------------------------------------------------------

/// `garden-a` to `garden-z`, then `garden-27` on.
fn workspace_name(n: usize) -> String {
    if n < 26 {
        format!("garden-{}", (b'a' + n as u8) as char)
    } else {
        format!("garden-{}", n + 1)
    }
}

fn folder(n: usize) -> Workspace {
    let name = workspace_name(n);
    Workspace {
        path: PathBuf::from("/srv/garden").join(&name),
        key: format!("folder:{name}"),
        name,
        branch: None,
        main: false,
        folder: true,
        repo: None,
        base: None,
        commit: None,
        merged: false,
        host: None,
        github: None,
    }
}

/// The six names, then the same again with a number.
fn repo_name(n: usize) -> String {
    if n < REPOS.len() {
        REPOS[n].to_owned()
    } else {
        format!("{}-{}", REPOS[n % REPOS.len()], n / REPOS.len() + 1)
    }
}

/// A title long enough to be cut in the inspector, different for every
/// repository and issue.
fn issue(repo: &str, number: u64) -> Issue {
    const WHAT: [&str; 5] = [
        "seed tray drains too slowly",
        "trellis report skips the climbing beans",
        "compost temperature reads stay at zero",
        "watering schedule doubles on the first frost",
        "harvest log loses the weights it was given",
    ];
    const LABELS: [&str; 4] = ["bug", "watering", "planning", "soil"];
    let n = number as usize;
    Issue {
        number,
        title: format!(
            "{repo}: the {} after a long dry week, seen again on bed {}",
            WHAT[n % WHAT.len()],
            n * 3 + 1
        ),
        state: "OPEN".into(),
        labels: vec![
            LABELS[n % LABELS.len()].into(),
            LABELS[(n + 1) % LABELS.len()].into(),
        ],
        created_at: format!("2026-09-{:02}T08:00:00Z", 1 + n % 28),
        ..Default::default()
    }
}

fn status(n: usize) -> Status {
    Status {
        branch: Some("main".into()),
        base: Some("main".into()),
        ahead: n,
        behind: 2,
        files: 3 + n,
        added: 120,
        removed: 40,
        upstream: Some("origin/main".into()),
        to_push: 1,
        to_pull: n % 2,
        dirty: 4,
        dirty_added: 18,
        dirty_removed: 6,
        tag: Some(("v0.4.0".into(), 7)),
    }
}

/// A terminal started in `workspace`, with no program behind it, its
/// screen already full of output.
fn tab(title: &str, workspace: &Workspace, cols: usize, rows: usize) -> Tab {
    let session = Session::offline(cols, rows).expect("a socket pair");
    let mut fill = String::new();
    for n in 0..rows * 2 {
        fill.push_str(&format!(
            "   Compiling {title}-part-{n} v0.{n}.0 (/srv/garden/{})\r\n",
            workspace.name
        ));
    }
    fill.push_str("$ ");
    session.feed(fill.as_bytes());
    session.drain_wake();
    Tab {
        session,
        title: title.into(),
        claude: false,
        launch: Launch {
            program: "/bin/zsh".into(),
            args: Vec::new(),
            cwd: workspace.path.clone(),
            env: Vec::new(),
            unset: Vec::new(),
        },
        attention: None,
        last_output: None,
        review: PathBuf::new(),
        events_read: 0,
        review_files: 0,
        folder: workspace.path.clone(),
        workspace: Some(workspace.id()),
        alive: Arc::new(()),
        held: None,
        status: None,
        cwd: workspace.path.clone(),
        program_pid: None,
        remote: None,
        told_focus: false,
        last_probe: None,
        probe_owed: false,
        seen: None,
    }
}

// ---- the frame ---------------------------------------------------------------

struct Scene {
    agents: Panel,
    terminal: terminal::Panel,
    theme: Theme,
    sidebar: Viewport,
    inspector: Viewport,
    screen: Viewport,
    issues: bool,
    /// The background terminals that print between samples.
    streaming: Vec<usize>,
}

/// What one frame cost, by part.
#[derive(Default)]
struct Drawn {
    list_ms: f64,
    inspector_ms: f64,
    terminal_ms: f64,
    gpu_ms: f64,
    glyphs: usize,
    /// UI labels shaped, by part: the list, the inspector, the terminal.
    shaped: [u64; 3],
}

impl Scene {
    /// What `render` does in Agents mode, from the sidebar down: the
    /// terminal's shown tabs, the workspace list, the inspector with what
    /// the window gathers for it, the terminal; then encode and wait.
    fn frame(
        &mut self,
        renderer: &mut Renderer,
        glyphs: &mut Vec<GlyphInstance>,
        size: (f32, f32),
        px: (usize, usize),
    ) -> Drawn {
        let mut drawn = Drawn::default();
        layout::begin_frame();
        renderer.atlas.begin_frame();
        glyphs.clear();
        let shapes = renderer.atlas.ui_shapes();

        let t = Instant::now();
        let tasks = &self.agents.tasks;
        let selected = self.agents.selected_index();
        self.terminal.shown = Some(
            (0..self.terminal.tabs.len())
                .filter(|i| agents::task_of(tasks, &self.terminal.tabs[*i]) == selected)
                .collect(),
        );
        let active = self.terminal.active_tab().map(|_| self.terminal.active);
        self.agents.draw(
            &self.terminal.tabs,
            active,
            &mut renderer.atlas,
            self.sidebar,
            &self.theme,
            glyphs,
        );
        drawn.list_ms = t.elapsed().as_secs_f64() * 1e3;
        let after_list = renderer.atlas.ui_shapes();

        let t = Instant::now();
        let agents = &mut self.agents;
        let selected = agents.selected_index().map(|i| agents.tasks[i].clone());
        let repos = selected.as_ref().and_then(|w| agents.repos_of(w));
        let worktrees: Vec<_> = match (&selected, &repos) {
            (Some(w), Some(repos)) if w.folder => agents
                .worktrees_of(w.host.as_deref(), repos)
                .into_iter()
                .map(|t| {
                    let title = agents.title(&t);
                    let pace = agents.pace(&t);
                    let merged = agents.is_merged(&t);
                    (t, title, pace, merged)
                })
                .collect(),
            _ => Vec::new(),
        };
        let issues = Issues {
            read: &agents.issues,
            asked: self.issues,
            loaded: &agents.arrangement.issues,
            now: NOW,
        };
        agents.inspector.draw(
            selected.as_ref(),
            repos.as_deref(),
            &worktrees,
            None,
            None,
            self.issues,
            issues,
            &agents.arrangement.folded_repos,
            &mut renderer.atlas,
            self.inspector,
            &self.theme,
            glyphs,
        );
        drawn.inspector_ms = t.elapsed().as_secs_f64() * 1e3;
        let after_inspector = renderer.atlas.ui_shapes();

        let t = Instant::now();
        terminal::draw(
            &self.terminal,
            None,
            &mut renderer.atlas,
            self.screen,
            &self.theme,
            glyphs,
        );
        drawn.terminal_ms = t.elapsed().as_secs_f64() * 1e3;
        let after_terminal = renderer.atlas.ui_shapes();

        let t = Instant::now();
        renderer.render_offscreen_frame(px.0, px.1, glyphs, size, self.theme.background);
        drawn.gpu_ms = t.elapsed().as_secs_f64() * 1e3;
        drawn.glyphs = glyphs.len();
        drawn.shaped = [
            after_list - shapes,
            after_inspector - after_list,
            after_terminal - after_inspector,
        ];
        drawn
    }

    /// What `poll_terminal` does on a wake, as far as drawing goes: every
    /// session's wake drained; output on screen redraws, output off screen
    /// only for a change in what is shown of its tab, paced as the window
    /// paces it. A change held back is drawn here once due, where the
    /// window has a timer for it. Returns whether a frame is drawn.
    fn poll_terminal(&mut self) -> bool {
        let now = Instant::now();
        let visible = self.terminal.visible(true);
        let (mut shown, mut changed) = (false, false);
        for (index, tab) in self.terminal.tabs.iter_mut().enumerate() {
            if !tab.session.drain_wake() {
                continue;
            }
            let was = tab.activity();
            tab.last_output = Some(now);
            let on_screen = visible.contains(&index);
            shown |= on_screen;
            changed |= tab.face_changed(was) && !on_screen;
        }
        let pace = &mut self.terminal.pace;
        let due = pace.take_due(now);
        pace.decide(shown, changed, now) == terminal::When::Now || due
    }
}

// ---- reporting ---------------------------------------------------------------

fn percentile(sorted: &[f64], p: f64) -> f64 {
    crc::platform::latency::nearest_rank(sorted, p)
}

fn report(name: &str, mut samples: Vec<f64>) {
    samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let p50 = percentile(&samples, 0.50);
    let p95 = percentile(&samples, 0.95);
    let p99 = percentile(&samples, 0.99);
    let max = samples.last().copied().unwrap_or(0.0);
    let over = samples.iter().filter(|&&s| s > BUDGET_MS).count();
    println!(
        "{name:<22} p50 {p50:>6.3}  p95 {p95:>6.3}  p99 {p99:>6.3}  max {max:>6.3} ms   \
         over budget: {over}/{} ({:.1}% of frame)",
        samples.len(),
        p99 / BUDGET_MS * 100.0
    );
}

fn main() {
    let options = Options::parse();
    let scale = 2.0f32;
    let (logical_w, logical_h) = (1400.0f32, 900.0f32);
    let (px_w, px_h) = ((logical_w * scale) as usize, (logical_h * scale) as usize);

    let device = MTLCreateSystemDefaultDevice().expect("no Metal device");
    let atlas = Atlas::build("SF Mono", 13.0, scale);
    println!("font        {}", atlas.font_name);
    println!("viewport    {logical_w}x{logical_h} pt  ({px_w}x{px_h} px at {scale}x)");
    println!("budget      {BUDGET_MS:.3} ms per frame at 120Hz\n");
    let mut renderer = Renderer::new(device, atlas);

    // The window's columns in Agents mode: the list, the terminal, the
    // inspector, over a status line.
    let height = logical_h - 22.0;
    let sidebar = Viewport {
        x: 0.0,
        y: 0.0,
        width: 260.0,
        height,
    };
    let inspector_rect = Viewport {
        x: logical_w - inspector::WIDTH,
        y: 0.0,
        width: inspector::WIDTH,
        height,
    };
    let screen = Viewport {
        x: sidebar.width,
        y: 0.0,
        width: logical_w - sidebar.width - inspector::WIDTH,
        height,
    };
    let (cols, rows) = terminal::grid_size(&renderer.atlas, terminal::split(screen).1);

    let folders: Vec<Workspace> = (0..options.workspaces).map(folder).collect();
    let home = folders[0].clone();
    let repos: Vec<PathBuf> = (0..options.repos)
        .map(|n| home.path.join(repo_name(n)))
        .collect();
    let mut panel = Panel::default();
    panel.on = true;
    panel.folders = folders.clone();
    panel.selected = Some(home.id());
    panel.repos.insert(home.id(), repos.clone());
    panel.inspector.status = repos
        .iter()
        .enumerate()
        .map(|(n, repo)| (repo.clone(), Ok(status(n))))
        .collect();
    if options.issues {
        for (n, repo) in repos.iter().enumerate() {
            let name = repo_name(n);
            let key: RepoKey = (None, repo.clone());
            // Asked for, so they draw as a list, not Load Issues.
            panel.arrangement.issues.insert(key.clone());
            let set = IssueSet {
                list: (1..=15).map(|i| issue(&name, i)).collect(),
                capped: false,
                error: None,
                read_at: Instant::now(),
            };
            panel.issues.insert(key, set);
        }
    }
    panel.arrange();

    // The selected workspace's two terminals, the first showing; the rest
    // spread over the others, and as many more as stream.
    let hidden = (TABS - SHOWN_TABS).max(options.hidden_streaming);
    let mut tabs: Vec<Tab> = (0..SHOWN_TABS)
        .map(|n| tab(&format!("shell-{}", n + 1), &home, cols, rows))
        .collect();
    for n in 0..hidden {
        let owner = &folders[(n + 1) % folders.len()];
        tabs.push(tab(&format!("agent-{}", n + 1), owner, cols, rows));
    }
    let streaming: Vec<usize> = (SHOWN_TABS..SHOWN_TABS + options.hidden_streaming).collect();
    let terminal = terminal::Panel {
        open: true,
        focus: true,
        tabs,
        active: 0,
        ..terminal::Panel::default()
    };
    let mut scene = Scene {
        agents: panel,
        terminal,
        theme: Theme::default(),
        sidebar,
        inspector: inspector_rect,
        screen,
        issues: options.issues,
        streaming,
    };
    println!(
        "fixture     {} workspaces, {} terminals ({} printing in the background), \
         {} repositories with {} issues each",
        options.workspaces,
        scene.terminal.tabs.len(),
        options.hidden_streaming,
        options.repos,
        if options.issues { 15 } else { 0 },
    );
    println!("terminal    {cols}x{rows} cells\n");

    let size = (logical_w, logical_h);
    let px = (px_w, px_h);
    let mut glyphs = Vec::with_capacity(16_384);
    // Warm up: shaders, textures, and every label shaped once.
    for _ in 0..30 {
        scene.frame(&mut renderer, &mut glyphs, size, px);
    }

    const N: usize = 400;
    let mut full = Vec::with_capacity(N);
    let mut list = Vec::with_capacity(N);
    let mut inspector_ms = Vec::with_capacity(N);
    let mut terminal_ms = Vec::with_capacity(N);
    let mut gpu = Vec::with_capacity(N);
    let mut background = Vec::new();
    let mut shaped_max = [0u64; 3];
    let mut shaped_total = [0u64; 3];
    // The most labels any one frame shaped, all parts together.
    let mut worst = 0u64;
    let mut glyph_count = 0usize;
    let (mut wakes, mut redraws) = (0usize, 0usize);

    for i in 0..N {
        // Between keystrokes, each background terminal prints a line on
        // its own, and each wake is a `poll_terminal`; every redraw it asks
        // for is a frame drawn. Before the pace, each wake asked for one.
        for (n, &index) in scene.streaming.clone().iter().enumerate() {
            let mut line = format!("   Compiling sprout-{n} v0.{i}.0\r\n");
            if i % 10 == 0 {
                line.push_str(&format!("\x1b]0;sprout-{n} step {i}\x07"));
            }
            if scene.terminal.tabs[index].session.feed(line.as_bytes()) {
                wakes += 1;
                if scene.poll_terminal() {
                    redraws += 1;
                    let t = Instant::now();
                    scene.frame(&mut renderer, &mut glyphs, size, px);
                    background.push(t.elapsed().as_secs_f64() * 1e3);
                }
            }
        }

        let started = Instant::now();
        // What the program echoes for a keystroke.
        let byte: &[u8] = if i % 60 == 59 { b"\r\n$ " } else { b"x" };
        scene.terminal.tabs[0].session.feed(byte);
        scene.poll_terminal();
        let drawn = scene.frame(&mut renderer, &mut glyphs, size, px);
        full.push(started.elapsed().as_secs_f64() * 1e3);
        list.push(drawn.list_ms);
        inspector_ms.push(drawn.inspector_ms);
        terminal_ms.push(drawn.terminal_ms);
        gpu.push(drawn.gpu_ms);
        glyph_count = drawn.glyphs;
        for part in 0..3 {
            shaped_max[part] = shaped_max[part].max(drawn.shaped[part]);
            shaped_total[part] += drawn.shaped[part];
        }
        worst = worst.max(drawn.shaped.iter().sum());
    }

    println!("{glyph_count} glyphs per frame");
    let parts = ["workspace list", "inspector", "terminal"];
    for part in 0..3 {
        println!(
            "{:<22} {:>8.1} labels shaped a frame on average, {} at most",
            parts[part],
            shaped_total[part] as f64 / N as f64,
            shaped_max[part]
        );
    }
    println!();
    report("workspace list", list);
    report("inspector", inspector_ms);
    report("terminal", terminal_ms);
    report("encode + GPU wait", gpu);
    report("keystroke to frame", full.clone());
    if options.hidden_streaming > 0 {
        // A change still held back at the end is drawn when its time comes.
        if scene.terminal.pace.due.is_some() {
            redraws += 1;
        }
        println!(
            "\nbackground  {wakes} wakes from {} hidden terminals: {wakes} redraws asked for \
             before the pace (one a wake), {redraws} now ({:.2} per keystroke)",
            options.hidden_streaming,
            redraws as f64 / N as f64
        );
        report("hidden-output frame", background);
    }

    full.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let p99 = percentile(&full, 0.99);
    println!(
        "\n{} p99 {:.3} ms against a {:.3} ms budget",
        if p99 < BUDGET_MS { "PASS:" } else { "FAIL:" },
        p99,
        BUDGET_MS
    );
    println!(
        "{} a warm frame shaped at most {} labels, against a limit of {RESHAPE_LIMIT}",
        if worst <= RESHAPE_LIMIT {
            "PASS:"
        } else {
            "FAIL:"
        },
        worst
    );
    std::hint::black_box(&glyphs);
}
