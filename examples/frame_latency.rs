//! Milestone 0's gate: can a keystroke become a finished frame inside a
//! 120Hz budget, with a large file open?
//!
//! Each iteration does exactly what a keystroke does in the running app:
//! mutate the buffer, lay out the visible region, encode the draw, and wait
//! for the GPU to report the frame complete. Nothing is warmed between
//! samples and nothing is skipped.
//!
//! Honest scope: this covers our work end to end, from edit through GPU
//! completion. It excludes the time before AppKit hands us the event and the
//! display's own scanout and panel latency, neither of which any software
//! timer on this machine can observe. So treat it as a floor.
//!
//! Run with: cargo run --release --offline --example frame_latency
//! (`-- --wrap`, `--fold` or `--chrome` for those frames, `--scrub` for
//! steps through a file's history, `--heat` to add Git > Blame Heat's
//! strip to every frame, `--syntax` for typing in highlighted Rust files
//! of 128 KiB to 4 MiB).

use crc::project::tree::Tree;
use crc::render::font::Atlas;
use crc::render::layout::{self, Chrome, Theme, Viewport};
use crc::render::metal::{GlyphInstance, Renderer};
use crc::text::buffer::{Buffer, Motion};
use crc::text::documents::Documents;
use objc2_metal::MTLCreateSystemDefaultDevice;
use std::time::{Duration, Instant};

#[path = "common/synth.rs"]
mod synth;

/// One 120Hz frame.
const BUDGET_MS: f64 = 1000.0 / 120.0;

/// `--wrap` measures the editor with soft wrap on, `--fold` after Fold All.
///
/// Optional native chrome gate: includes proportional labels, a project tree,
/// tabs, breadcrumbs and a changing cursor status in every measured frame.
struct ChromeFrame {
    rects: Chrome,
    tree: Tree,
    docs: Documents,
    hits: Vec<layout::TabHit>,
}

impl ChromeFrame {
    fn append(
        &mut self,
        atlas: &mut Atlas,
        buffer: &Buffer,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        layout::build_toolbar(
            &self.tree,
            false,
            None,
            atlas,
            self.rects.toolbar,
            theme,
            out,
        );
        layout::build_sidebar(
            &self.tree,
            false,
            atlas,
            self.rects.sidebar.unwrap(),
            theme,
            out,
        );
        layout::build_tab_bar(
            &self.docs,
            0,
            None,
            atlas,
            self.rects.tabs,
            theme,
            out,
            &mut self.hits,
        );
        layout::build_breadcrumbs(
            buffer,
            &self.tree,
            false,
            atlas,
            self.rects.breadcrumbs,
            theme,
            out,
        );
        let status = self.rects.status;
        layout::push_rect(
            out,
            atlas,
            [status.x, status.y],
            [status.width, status.height],
            theme.status_background,
        );
        let (line, column) = buffer.cursor_position();
        layout::push_ui_text(
            out,
            atlas,
            status,
            &format!(
                "main.rs     Ln {}, Col {}     UTF-8     LF",
                line + 1,
                column + 1
            ),
            theme.status_text,
        );
    }
}

/// `--heat`: the blame heat strip as the window draws it, over a blame
/// that names one of five commits of every age for each line, the
/// unsigned ones dashed.
struct HeatFrame {
    lines: Vec<u32>,
    alpha: Vec<f32>,
    tips: Vec<String>,
    dashed: Vec<bool>,
}

impl HeatFrame {
    fn new(lines: usize) -> Self {
        HeatFrame {
            lines: (0..lines).map(|n| (n / 7 % 5) as u32).collect(),
            alpha: vec![1.0, 0.8, 0.55, 0.35, 0.18],
            tips: (0..5)
                .map(|n| format!("{n}abcdef  Robin Example  {n} days ago\nplant beans"))
                .collect(),
            dashed: vec![true, false, true, false, true],
        }
    }

    fn append(
        &self,
        atlas: &Atlas,
        buffer: &Buffer,
        viewport: Viewport,
        theme: &Theme,
        out: &mut Vec<GlyphInstance>,
    ) {
        // The window starts the frame's hotspots, and works the rows out
        // once for everything drawn on them; `build` keeps its own.
        layout::begin_frame();
        let rows = layout::screen_rows(buffer, viewport, atlas.metrics.line_height);
        layout::push_blame_heat(
            out,
            atlas,
            viewport,
            &rows,
            theme,
            &layout::BlameHeat {
                lines: &self.lines,
                alpha: &self.alpha,
                tips: &self.tips,
                dashed: &self.dashed,
                hide_from: usize::MAX,
            },
        );
    }
}

/// What an edit costs the strip besides drawing: finding the first line
/// that differs from the text blamed, in a file at the 2 MB limit, with
/// the edit at its end, the worst place.
fn heat_follow() {
    use crc::text::rope::Rope;
    let text = "    let x = compute(a, b, c); // a line of ordinary length\n".repeat(36_000);
    let blamed = Rope::from_text(&text);
    let mut edited = blamed.clone();
    let mut samples = Vec::with_capacity(200);
    for _ in 0..200 {
        edited.insert(edited.len_bytes() - 1, "x");
        let started = Instant::now();
        let line = blamed
            .first_difference(&edited)
            .map(|at| edited.byte_to_line(at));
        samples.push(started.elapsed().as_secs_f64() * 1e3);
        std::hint::black_box(line);
    }
    println!();
    println!(
        "follow      {} KiB blamed, edits at the end",
        text.len() / 1024
    );
    report("heat follow", samples);
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    crc::platform::latency::nearest_rank(sorted, p)
}

fn report(name: &str, mut samples: Vec<f64>) {
    samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let p50 = percentile(&samples, 0.50);
    let p99 = percentile(&samples, 0.99);
    let max = samples.last().copied().unwrap_or(0.0);
    let over = samples.iter().filter(|&&s| s > BUDGET_MS).count();
    println!(
        "{name:<22} p50 {p50:>6.3}  p99 {p99:>6.3}  max {max:>6.3} ms   \
         over budget: {over}/{} ({:.1}% of frame)",
        samples.len(),
        p99 / BUDGET_MS * 100.0
    );
}

/// PASS or FAIL on the p99 of whole frames; `what` names them when a run
/// has more than one kind.
fn verdict(what: &str, mut full: Vec<f64>) {
    full.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let p99 = percentile(&full, 0.99);
    println!(
        "\n{} {what}p99 {:.3} ms against a {:.3} ms budget",
        if p99 < BUDGET_MS { "PASS:" } else { "FAIL:" },
        p99,
        BUDGET_MS
    );
}

fn main() {
    let scale = 2.0f32;
    let (logical_w, logical_h) = (1400.0f32, 900.0f32);
    let (px_w, px_h) = ((logical_w * scale) as usize, (logical_h * scale) as usize);

    let device = MTLCreateSystemDefaultDevice().expect("no Metal device");
    let atlas = Atlas::build("SF Mono", 13.0, scale);
    println!("font        {}", atlas.font_name);
    println!("viewport    {logical_w}x{logical_h} pt  ({px_w}x{px_h} px at {scale}x)");
    println!("budget      {BUDGET_MS:.3} ms per frame at 120Hz\n");

    let mut renderer = Renderer::new(device, atlas);

    // `--scrub`: stepping through a file's history instead of typing.
    if std::env::args().any(|arg| arg == "--scrub") {
        scrub_steps(&mut renderer, (logical_w, logical_h), (px_w, px_h));
        return;
    }
    // `--syntax`: typing with highlighting in a file past the main thread's
    // parse budget.
    if std::env::args().any(|arg| arg == "--syntax") {
        syntax_typing(&mut renderer, (logical_w, logical_h), (px_w, px_h));
        return;
    }

    const MB: usize = 100 * 1024 * 1024;
    let src = synth::synth(
        MB,
        "    // a comment line that runs on a bit longer than the others\n",
    );
    let t = Instant::now();
    let mut buffer = Buffer::from_text(&src);
    buffer.path = Some("src/main.rs".into());
    println!(
        "opened      {} MiB, {} lines in {:.1} ms",
        src.len() / 1024 / 1024,
        buffer.rope.len_lines(),
        t.elapsed().as_secs_f64() * 1e3
    );

    // Work in the middle of the file, where a naive structure would be worst.
    let middle = buffer.rope.len_lines() / 2;
    for _ in 0..middle {
        buffer.move_down(Motion::Move);
    }
    // `--wrap`: the same frames with soft wrap at 40 columns, so most
    // lines of the synthetic source take two or three rows.
    if std::env::args().any(|arg| arg == "--wrap") {
        buffer.wrap = Some(40);
        println!("scope       soft wrap at 40 columns");
    }
    // `--fold`: Fold All first, so the frames skip what folds hide.
    if std::env::args().any(|arg| arg == "--fold") {
        // Fold All refuses a file this size; fold every block directly, the
        // worst case for drawing.
        let t = Instant::now();
        let mut line = 0;
        while line < buffer.rope.len_lines() {
            match buffer.fold_range(line) {
                Some((a, b)) => {
                    buffer.folds.push((a, b));
                    line = b + 1;
                }
                None => line += 1,
            }
        }
        println!(
            "scope       Fold All: {} folds in {:.1} ms",
            buffer.folds.len(),
            t.elapsed().as_secs_f64() * 1e3
        );
    }
    let window = Viewport::new(logical_w, logical_h);
    let mut chrome = std::env::args().any(|arg| arg == "--chrome").then(|| {
        let mut tree = Tree::new();
        tree.open(std::env::current_dir().unwrap());
        let mut tab = Buffer::from_text("");
        tab.path = buffer.path.clone();
        println!("scope       native chrome and editor");
        ChromeFrame {
            rects: Chrome::new(window, Some(260.0), 0),
            tree,
            docs: Documents::new(tab),
            hits: Vec::new(),
        }
    });
    let viewport = chrome.as_ref().map_or(window, |c| c.rects.text);
    let heat = std::env::args().any(|arg| arg == "--heat").then(|| {
        println!("scope       blame heat strip, five commits, every line blamed");
        HeatFrame::new(buffer.rope.len_lines())
    });
    let rows = viewport.rows(renderer.atlas.metrics.line_height);
    buffer.scroll_to_cursor(rows, 0);
    println!("cursor      line {middle}, viewport holds {rows} lines\n");

    let theme = Theme::default();
    let mut glyphs = Vec::with_capacity(16_384);

    // Warm up: first frame pays for shader pipeline warm-up and the first
    // texture allocation, which no steady-state keystroke pays again.
    for _ in 0..30 {
        layout::build(&buffer, &mut renderer.atlas, viewport, &theme, &mut glyphs);
        if let Some(heat) = &heat {
            heat.append(&renderer.atlas, &buffer, viewport, &theme, &mut glyphs);
        }
        if let Some(chrome) = &mut chrome {
            chrome.append(&mut renderer.atlas, &buffer, &theme, &mut glyphs);
        }
        renderer.render_offscreen_frame(
            px_w,
            px_h,
            &glyphs,
            (logical_w, logical_h),
            theme.background,
        );
    }

    const N: usize = 400;
    let mut full = Vec::with_capacity(N);
    let mut layout_only = Vec::with_capacity(N);
    let mut gpu_only = Vec::with_capacity(N);
    let mut quads = 0usize;

    for i in 0..N {
        let started = Instant::now();

        // Exactly what a keystroke does.
        if i % 8 == 7 {
            buffer.backspace();
        } else {
            buffer.insert("x");
        }
        buffer.scroll_to_cursor(rows, 0);

        let t_layout = Instant::now();
        let stats = layout::build(&buffer, &mut renderer.atlas, viewport, &theme, &mut glyphs);
        if let Some(heat) = &heat {
            heat.append(&renderer.atlas, &buffer, viewport, &theme, &mut glyphs);
        }
        if let Some(chrome) = &mut chrome {
            chrome.append(&mut renderer.atlas, &buffer, &theme, &mut glyphs);
        }
        let layout_ms = t_layout.elapsed().as_secs_f64() * 1e3;
        quads = stats.quads;

        let t_gpu = Instant::now();
        renderer.render_offscreen_frame(
            px_w,
            px_h,
            &glyphs,
            (logical_w, logical_h),
            theme.background,
        );
        let gpu_ms = t_gpu.elapsed().as_secs_f64() * 1e3;

        full.push(started.elapsed().as_secs_f64() * 1e3);
        layout_only.push(layout_ms);
        gpu_only.push(gpu_ms);
    }

    println!("{quads} quads per frame\n");
    report("edit + layout", layout_only);
    report("encode + GPU wait", gpu_only);
    report("keystroke to frame", full.clone());
    verdict("", full);

    // Scrolling is the other thing that has to stay smooth.
    let mut scroll = Vec::with_capacity(200);
    for i in 0..200 {
        let started = Instant::now();
        buffer.scroll_by(if i % 2 == 0 { 3 } else { -1 }, rows);
        layout::build(&buffer, &mut renderer.atlas, viewport, &theme, &mut glyphs);
        if let Some(heat) = &heat {
            heat.append(&renderer.atlas, &buffer, viewport, &theme, &mut glyphs);
        }
        if let Some(chrome) = &mut chrome {
            chrome.append(&mut renderer.atlas, &buffer, &theme, &mut glyphs);
        }
        renderer.render_offscreen_frame(
            px_w,
            px_h,
            &glyphs,
            (logical_w, logical_h),
            theme.background,
        );
        scroll.push(started.elapsed().as_secs_f64() * 1e3);
    }
    println!();
    report("scroll frame", scroll);

    if heat.is_some() {
        heat_follow();
    }

    std::hint::black_box(&glyphs);
    std::hint::black_box(Duration::from_nanos(0));
}

// ---- --syntax --------------------------------------------------------------
// Typing in a highlighted Rust file, at sizes either side of the window's
// parse budgets. A sample is what the window does for a keystroke and the
// frame after it: the edit, `update_async`, a `poll` for whatever the worker
// has finished, the spans on screen, layout and the GPU. Keystrokes come a
// frame apart, as typing does, which costs a main-thread parse several times
// what it costs back to back. Before the worker, a 4 MiB file's parse alone
// was about 10 ms here.

/// Sizes of the files typed into.
const SYNTAX_SIZES_KIB: [usize; 5] = [128, 256, 512, 1024, 4096];
/// The window's `reparse_budget` and `first_parse_budget`.
const SYNTAX_EDIT_BUDGET: usize = 64 * 1024;
const SYNTAX_OPEN_BUDGET: usize = 512 * 1024;

fn syntax_typing(renderer: &mut Renderer, logical: (f32, f32), px: (usize, usize)) {
    use crc::syntax::SyntaxStore;

    let viewport = Viewport::new(logical.0, logical.1);
    let theme = Theme::default();
    let line_height = renderer.atlas.metrics.line_height;
    let rows = viewport.rows(line_height);
    let mut glyphs = Vec::with_capacity(16_384);
    let mut spans = Vec::new();
    let mut frame = |buffer: &Buffer,
                     syntax: &SyntaxStore,
                     spans: &mut Vec<crc::syntax::Span>,
                     renderer: &mut Renderer| {
        spans.clear();
        let screen = layout::screen_rows(buffer, viewport, line_height);
        if let Some(lines) = layout::lines_of(&screen) {
            let from = buffer.rope.line_to_byte(lines.start);
            let to = if lines.end < buffer.rope.len_lines() {
                buffer.rope.line_to_byte(lines.end)
            } else {
                buffer.rope.len_bytes()
            };
            spans.extend(
                syntax.spans_with(buffer.id(), from..to, |r| buffer.rope.slice_to_string(r)),
            );
        }
        layout::build_full_search(
            buffer,
            &mut renderer.atlas,
            viewport,
            &screen,
            &theme,
            "",
            None,
            spans,
            &layout::Markdown::default(),
            true,
            &mut glyphs,
        );
        renderer.render_offscreen_frame(px.0, px.1, &glyphs, logical, theme.background);
    };
    let settle = |syntax: &mut SyntaxStore| {
        while syntax.busy() {
            syntax.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
    };

    // The query compiles once per language, not per document.
    let mut syntax = SyntaxStore::new();
    let mut warm = Buffer::from_text("fn f() {}\n");
    warm.path = Some("warm.rs".into());
    syntax.update_async(&mut warm, SYNTAX_EDIT_BUDGET, SYNTAX_OPEN_BUDGET);

    println!(
        "{:>8}  {:>8}  {:>13}  {:>22}  {:>22}  {:>10}",
        "size",
        "open",
        "colour after",
        "update_async+poll p50/p99",
        "keystroke-frame p99/max",
        "caught up"
    );
    let mut verdicts = Vec::new();
    for kib in SYNTAX_SIZES_KIB {
        let src = synth::synth(kib * 1024, "    // a comment line\n");
        let mut buffer = Buffer::from_text(&src);
        buffer.path = Some("src/big.rs".into());
        let middle = buffer.rope.len_lines() / 2;
        for _ in 0..middle {
            buffer.move_down(Motion::Move);
        }
        buffer.scroll_to_cursor(rows, 0);

        let t = Instant::now();
        syntax.update_async(&mut buffer, SYNTAX_EDIT_BUDGET, SYNTAX_OPEN_BUDGET);
        let open_ms = t.elapsed().as_secs_f64() * 1e3;
        settle(&mut syntax);
        let coloured_ms = t.elapsed().as_secs_f64() * 1e3;
        for _ in 0..30 {
            frame(&buffer, &syntax, &mut spans, renderer);
        }

        const N: usize = 200;
        let (mut full, mut parse) = (Vec::with_capacity(N), Vec::with_capacity(N));
        for i in 0..N {
            std::thread::sleep(Duration::from_secs_f64(BUDGET_MS / 1e3));
            let started = Instant::now();
            if i % 8 == 7 {
                buffer.backspace();
            } else {
                buffer.insert("x");
            }
            buffer.scroll_to_cursor(rows, 0);
            let t_parse = Instant::now();
            syntax.update_async(&mut buffer, SYNTAX_EDIT_BUDGET, SYNTAX_OPEN_BUDGET);
            syntax.poll();
            parse.push(t_parse.elapsed().as_secs_f64() * 1e3);
            frame(&buffer, &syntax, &mut spans, renderer);
            full.push(started.elapsed().as_secs_f64() * 1e3);
        }
        let t = Instant::now();
        settle(&mut syntax);
        let caught_up_ms = t.elapsed().as_secs_f64() * 1e3;
        parse.sort_by(f64::total_cmp);
        full.sort_by(f64::total_cmp);
        println!(
            "{:>4} KiB  {open_ms:>5.1} ms  {coloured_ms:>10.1} ms  {:>10.3} / {:>6.3} ms  {:>10.3} / {:>6.3} ms  {caught_up_ms:>7.1} ms",
            kib,
            percentile(&parse, 0.50),
            percentile(&parse, 0.99),
            percentile(&full, 0.99),
            full[N - 1],
        );
        verdicts.push((kib, full));
        syntax.retain(|_| false);
    }
    for (kib, full) in verdicts {
        verdict(&format!("{kib} KiB "), full);
    }
}

// ---- --scrub ----------------------------------------------------------------
// Git > File History steps through a file's versions in a read-only tab, and
// a step has to be as quick as a keystroke. Each sample does what a step
// does on the main thread: the text goes in through the neighbours' diff, an
// edit per change, the parser goes on from the version before with those
// edits (a full reparse would cost several frames), and the frame draws the
// fade (washes and removed lines) over the version it lands on. Reading the
// blobs and diffing neighbours is the history worker's, so it happens before
// the clock starts, as it has by the time the app shows a step.

/// Versions of the file the history steps through.
const SCRUB_VERSIONS: usize = 60;
/// Size of the oldest version.
const SCRUB_BYTES: usize = 300 * 1024;

/// A small deterministic generator, so every run steps the same history.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n.max(1)
    }
}

/// One function of the file, varied by `i` so no two are the same.
fn scrub_item(i: usize, out: &mut Vec<String>) {
    out.push(format!(
        "/// Folds the samples of channel {i} into one reading."
    ));
    out.push(format!(
        "pub fn channel_{i}(input: &[u8], scale: f64) -> Result<Vec<u32>, String> {{"
    ));
    out.push("    let mut out = Vec::with_capacity(input.len());".into());
    out.push("    for (n, byte) in input.iter().enumerate() {".into());
    out.push(format!(
        "        if *byte > {} {{ out.push((*byte as f64 * scale) as u32 + n as u32); }}",
        i % 200
    ));
    out.push("    }".into());
    out.push("    // keep going while there is room".into());
    out.push(format!("    let label = \"channel {i}\";"));
    out.push("    if out.is_empty() { return Err(label.to_string()); }".into());
    out.push("    Ok(out)".into());
    out.push("}".into());
    out.push(String::new());
}

/// The file's versions, oldest first: each one a commit's worth of edits
/// to the last, a few lines added, removed and changed in a few places.
/// Every edit keeps the file valid Rust, as a real commit mostly does.
fn scrub_versions() -> Vec<String> {
    let mut lines = Vec::new();
    let mut items = 0;
    let mut bytes = 0;
    while bytes < SCRUB_BYTES {
        let at = lines.len();
        scrub_item(items, &mut lines);
        bytes += lines[at..].iter().map(|l| l.len() + 1).sum::<usize>();
        items += 1;
    }
    let mut rng = Lcg(0x5eed);
    let mut versions = vec![lines.join("\n") + "\n"];
    for v in 1..SCRUB_VERSIONS {
        for e in 0..2 + rng.below(3) {
            // A statement line somewhere in the file, at random.
            let mut at = rng.below(lines.len());
            while !lines[at].starts_with("    let ") {
                at = (at + 1) % lines.len();
            }
            match rng.below(4) {
                0 => {
                    for k in 0..1 + rng.below(4) {
                        lines.insert(
                            at + 1,
                            format!("    let extra_{v}_{e}_{k} = {v} * {k}; // since {v}"),
                        );
                    }
                }
                1 => {
                    // Drop the comment of the next function along.
                    if let Some(off) = lines[at..]
                        .iter()
                        .position(|l| l == "    // keep going while there is room")
                    {
                        lines.remove(at + off);
                    }
                }
                2 => {
                    let mut item = Vec::new();
                    scrub_item(items, &mut item);
                    items += 1;
                    let start = lines[..at]
                        .iter()
                        .rposition(|l| l.starts_with("/// "))
                        .unwrap_or(0);
                    lines.splice(start..start, item);
                }
                _ => lines[at] = format!("{} // reviewed in {v}", lines[at]),
            }
        }
        versions.push(lines.join("\n") + "\n");
    }
    versions
}

/// The lines a step washes, by kind, and the ghosts of those it removed.
type ScrubFade = (
    Vec<(std::ops::Range<usize>, crc::project::git::MarkKind)>,
    Vec<layout::Ghost>,
);

/// What a step marks for the fade, as the scrubber's own `Fade::new` does:
/// a wash over the lines added or changed, and the lines removed, where
/// they were, three of them at most.
fn scrub_fade(changes: &[crc::ide::diff::Change], old: &str) -> ScrubFade {
    use crc::project::git::MarkKind;
    let mut tints = Vec::new();
    let mut ghosts = Vec::new();
    let mut lines: Option<Vec<&str>> = None;
    for change in changes {
        if change.kind != MarkKind::Removed {
            tints.push((change.new_start..change.new_end, change.kind));
            continue;
        }
        let lines = lines.get_or_insert_with(|| old.split_inclusive('\n').collect());
        let removed = change.old_end - change.old_start;
        ghosts.push(layout::Ghost {
            at: change.new_start,
            lines: lines
                .iter()
                .skip(change.old_start)
                .take(removed.min(3))
                .map(|line| layout::expanded_line(line))
                .collect(),
            more: removed.saturating_sub(3),
        });
    }
    (tints, ghosts)
}

/// `changes` from one text to the other as the changes back.
fn scrub_reversed(changes: &[crc::ide::diff::Change]) -> Vec<crc::ide::diff::Change> {
    use crc::project::git::MarkKind;
    changes
        .iter()
        .map(|c| crc::ide::diff::Change {
            new_start: c.old_start,
            new_end: c.old_end,
            old_start: c.new_start,
            old_end: c.new_end,
            kind: match c.kind {
                MarkKind::Added => MarkKind::Removed,
                MarkKind::Removed => MarkKind::Added,
                MarkKind::Modified => MarkKind::Modified,
            },
        })
        .collect()
}

fn scrub_steps(renderer: &mut Renderer, logical: (f32, f32), px: (usize, usize)) {
    use crc::project::git::MarkKind;
    use crc::syntax::SyntaxStore;

    let t = Instant::now();
    let versions = scrub_versions();
    // The worker's diffs between neighbours, by the older of the two.
    let diffs: Vec<Vec<crc::ide::diff::Change>> = versions
        .windows(2)
        .map(|pair| crc::ide::diff::changes(&pair[0], &pair[1]))
        .collect();
    let hunks: usize = diffs.iter().map(Vec::len).sum();
    println!(
        "history     {} versions of {} to {} KiB, {:.1} hunks a step, made in {:.1} ms",
        versions.len(),
        versions[0].len() / 1024,
        versions[versions.len() - 1].len() / 1024,
        hunks as f64 / diffs.len() as f64,
        t.elapsed().as_secs_f64() * 1e3
    );

    // The history's tab, as the scrubber makes it: generated, frozen, Rust.
    let mut buffer = Buffer::generated("history.rs @ 0000000", "rs", "");
    buffer.set_frozen(true);
    let middle = versions[0].lines().count() / 2;
    buffer.show_text(&versions[0], None, middle, 4);
    let mut syntax = SyntaxStore::new();
    let budget = 2 * 1024 * 1024;
    let t = Instant::now();
    syntax.update(&mut buffer, budget);
    let first_parse = t.elapsed().as_secs_f64() * 1e3;
    println!("first parse {first_parse:.1} ms (once, as the tab opens)");

    let viewport = Viewport::new(logical.0, logical.1);
    let theme = Theme::default();
    let line_height = renderer.atlas.metrics.line_height;
    let rows = viewport.rows(line_height);
    buffer.scroll_line = middle.saturating_sub(rows / 2);
    println!("cursor      line {middle}, viewport holds {rows} lines\n");

    let mut glyphs = Vec::with_capacity(16_384);
    let mut spans = Vec::new();
    let mut frame = |buffer: &Buffer,
                     syntax: &SyntaxStore,
                     spans: &mut Vec<crc::syntax::Span>,
                     renderer: &mut Renderer,
                     marks: &[crc::project::git::Mark],
                     fade: Option<&ScrubFade>| {
        // As the window draws the editor: the rows once, the spans of what
        // is on screen, the text, the gutter marks, then the fade.
        spans.clear();
        let screen = layout::screen_rows(buffer, viewport, line_height);
        if let Some(lines) = layout::lines_of(&screen) {
            let from = buffer.rope.line_to_byte(lines.start);
            let to = if lines.end < buffer.rope.len_lines() {
                buffer.rope.line_to_byte(lines.end)
            } else {
                buffer.rope.len_bytes()
            };
            spans.extend(
                syntax.spans_with(buffer.id(), from..to, |r| buffer.rope.slice_to_string(r)),
            );
        }
        layout::build_full_search(
            buffer,
            &mut renderer.atlas,
            viewport,
            &screen,
            &theme,
            "",
            None,
            spans,
            &layout::Markdown::default(),
            true,
            &mut glyphs,
        );
        layout::push_gutter_marks(
            &mut glyphs,
            &renderer.atlas,
            viewport,
            &screen,
            &theme,
            marks,
        );
        if let Some((tints, ghosts)) = fade {
            // Drawn at its strongest, the first frame of a step.
            let tints: Vec<_> = tints
                .iter()
                .map(|(lines, kind)| {
                    let band = match kind {
                        MarkKind::Modified => {
                            let [r, g, b, _] = theme.diff_modified;
                            [r, g, b, theme.diff_added_band[3]]
                        }
                        _ => theme.diff_added_band,
                    };
                    (lines.clone(), band)
                })
                .collect();
            glyphs.splice(
                0..0,
                layout::line_tints(&screen, &renderer.atlas, viewport, &tints),
            );
            for ghost in ghosts {
                layout::push_ghost(
                    &mut glyphs,
                    &mut renderer.atlas,
                    buffer,
                    viewport,
                    &screen,
                    ghost,
                    1.0,
                    &theme,
                );
            }
        }
        let quads = glyphs.len();
        renderer.render_offscreen_frame(px.0, px.1, &glyphs, logical, theme.background);
        quads
    };

    for _ in 0..30 {
        frame(&buffer, &syntax, &mut spans, renderer, &[], None);
    }

    // Back and forth across the whole history: one version a step, the way
    // Left and Right step it, then seven at a time, the way a drag along
    // the timeline jumps. A jump's changes are the worker's, diffed while
    // the version shown stays put, so they are worked out off the clock.
    const N: usize = 400;
    let mut verdicts = Vec::new();
    for (name, stride) in [("step", 1usize), ("jump", 7)] {
        let mut full = Vec::with_capacity(N);
        let mut show = Vec::with_capacity(N);
        let mut reparse = Vec::with_capacity(N);
        let mut draw = Vec::with_capacity(N);
        let (mut shown, mut forward) = (0usize, true);
        let (mut tinted, mut ghosted, mut quads) = (0usize, 0usize, 0usize);
        buffer.show_text(&versions[0], None, middle, 4);
        syntax.update(&mut buffer, budget);
        for _ in 0..N {
            if shown < stride {
                forward = true;
            } else if shown + stride >= versions.len() {
                forward = false;
            }
            let target = if forward {
                shown + stride
            } else {
                shown - stride
            };
            let jumped =
                (stride > 1).then(|| crc::ide::diff::changes(&versions[shown], &versions[target]));
            let started = Instant::now();

            // What `show` does: the fade from the step's changes, the caret
            // and the view mapped through them, the text in, the gutter's
            // marks.
            let changes = match &jumped {
                Some(changes) => changes.clone(),
                None if forward => diffs[shown].clone(),
                None => scrub_reversed(&diffs[target]),
            };
            let fade = scrub_fade(&changes, &versions[shown]);
            let map = |line| crc::ide::diff::map_line(&changes, line);
            let (line, column) = buffer.cursor_position();
            let top = map(buffer.scroll_line);
            buffer.show_text(&versions[target], Some(&changes), map(line), column);
            buffer.scroll_line = top.min(buffer.rope.len_lines().saturating_sub(1));
            let marks = crc::project::git::marks(&diffs[target.saturating_sub(1)]);
            let shown_ms = started.elapsed().as_secs_f64() * 1e3;

            let t_parse = Instant::now();
            syntax.update(&mut buffer, budget);
            let parse_ms = t_parse.elapsed().as_secs_f64() * 1e3;

            let t_draw = Instant::now();
            quads = frame(&buffer, &syntax, &mut spans, renderer, &marks, Some(&fade));
            let draw_ms = t_draw.elapsed().as_secs_f64() * 1e3;

            tinted += fade.0.len();
            ghosted += fade.1.len();
            full.push(started.elapsed().as_secs_f64() * 1e3);
            show.push(shown_ms);
            reparse.push(parse_ms);
            draw.push(draw_ms);
            shown = target;
        }

        println!(
            "{name} {stride}: {quads} quads per frame, {:.1} washes and {:.1} ghosts a step\n",
            tinted as f64 / N as f64,
            ghosted as f64 / N as f64
        );
        report("show text", show);
        report("syntax reparse", reparse);
        report("layout + GPU + fade", draw);
        report(&format!("{name} to frame"), full.clone());
        verdicts.push((name, full));
        println!();
    }
    for (name, full) in verdicts {
        verdict(&format!("{name} "), full);
    }
}
