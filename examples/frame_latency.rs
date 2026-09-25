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

use crc::project::tree::Tree;
use crc::render::font::Atlas;
use crc::render::layout::{self, Chrome, Theme, Viewport};
use crc::render::metal::{GlyphInstance, Renderer};
use crc::text::buffer::{Buffer, Motion};
use crc::text::documents::Documents;
use objc2_metal::MTLCreateSystemDefaultDevice;
use std::time::{Duration, Instant};

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
        layout::build_toolbar(&self.tree, atlas, self.rects.toolbar, theme, out);
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

fn synth(target_bytes: usize) -> String {
    let mut s = String::with_capacity(target_bytes + 128);
    let mut i = 0usize;
    while s.len() < target_bytes {
        match i % 7 {
            0 => s.push_str("fn handle_event(&mut self, ev: &Event) -> Result<()> {\n"),
            1 => s.push_str("    let span = self.tree.node_at(ev.offset)?;\n"),
            2 => s.push_str("    // a comment line that runs on a bit longer than the others\n"),
            3 => s.push_str("    match span.kind() { Kind::Ident => self.bump(), _ => {} }\n"),
            4 => s.push('\n'),
            5 => s.push_str("    self.dirty.insert(span.range());\n"),
            _ => s.push_str("}\n"),
        }
        i += 1;
    }
    s
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[(rank - 1).min(sorted.len() - 1)]
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

    const MB: usize = 100 * 1024 * 1024;
    let src = synth(MB);
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
    let rows = viewport.rows(renderer.atlas.metrics.line_height);
    buffer.scroll_to_cursor(rows, 0);
    println!("cursor      line {middle}, viewport holds {rows} lines\n");

    let theme = Theme::default();
    let mut glyphs = Vec::with_capacity(16_384);

    // Warm up: first frame pays for shader pipeline warm-up and the first
    // texture allocation, which no steady-state keystroke pays again.
    for _ in 0..30 {
        layout::build(&buffer, &mut renderer.atlas, viewport, &theme, &mut glyphs);
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

    let mut sorted = full;
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let p99 = percentile(&sorted, 0.99);
    println!(
        "\n{} p99 {:.3} ms against a {:.3} ms budget",
        if p99 < BUDGET_MS { "PASS:" } else { "FAIL:" },
        p99,
        BUDGET_MS
    );

    // Scrolling is the other thing that has to stay smooth.
    let mut scroll = Vec::with_capacity(200);
    for i in 0..200 {
        let started = Instant::now();
        buffer.scroll_by(if i % 2 == 0 { 3 } else { -1 }, rows);
        layout::build(&buffer, &mut renderer.atlas, viewport, &theme, &mut glyphs);
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

    std::hint::black_box(&glyphs);
    std::hint::black_box(Duration::from_nanos(0));
}
