//! Renders a real frame offscreen and writes it to a BMP.
//!
//! Same device, pipeline, shader and layout as the live window; only the
//! render target differs. So this checks the thing that actually ships,
//! rather than a parallel drawing path that could quietly drift away from it.
//!
//! Run with: cargo run --release --offline --example frame_dump -- out.bmp [file] [start:end] [--scroll=column]
//!
//! With a file, that file is what gets drawn, highlighted exactly as the app
//! would, and the palette overlay is left off so the text can be seen.

use crc::project::finder::Finder;
use crc::project::tree::Tree;
use crc::render::font::Atlas;
use crc::render::layout::{self, Chrome, Theme, Viewport};
use crc::render::metal::Renderer;
use crc::syntax::SyntaxStore;
use crc::text::buffer::{Buffer, Motion};
use crc::text::documents::Documents;
use objc2_metal::MTLCreateSystemDefaultDevice;
use std::io::Write;

const SAMPLE: &str = "\
// Unicode check: everything below used to be silently dropped.
use std::collections::HashMap;

/// Counts how often each word appears. Compte les mots.
fn word_counts(text: &str) -> HashMap<&str, usize> {
\tlet mut counts = HashMap::new();
\tfor word in text.split_whitespace() {
\t\t*counts.entry(word).or_insert(0) += 1;
\t}
\tcounts
}

fn main() {
\t// Accents:   café, naïve, Straße, œuvre, Ångström
\t// Greek:     λ = μ * σ²   Cyrillic: Привет
\t// CJK:       漢字 かな 한글   (double width)
\t// Emoji:     🌍 🚀 ✅ 🎉   (colour, double width)
\tlet text = \"the quick brown fox jumps over the lazy dog\";
\tlet counts = word_counts(text);

\tfor (word, n) in counts.iter().take(3) {
\t\tprintln!(\"{n:>3}  {word}\");
\t}
}
";

/// BGRA8 from Metal to a 24-bit BMP (bottom-up rows, 4-byte padded).
fn write_bmp(path: &str, w: usize, h: usize, bgra: &[u8]) -> std::io::Result<()> {
    let row_padded = (w * 3).div_ceil(4) * 4;
    let pixel_bytes = row_padded * h;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);

    f.write_all(b"BM")?;
    f.write_all(&((54 + pixel_bytes) as u32).to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&54u32.to_le_bytes())?;
    f.write_all(&40u32.to_le_bytes())?;
    f.write_all(&(w as i32).to_le_bytes())?;
    f.write_all(&(h as i32).to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&24u16.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&(pixel_bytes as u32).to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;

    let mut row = vec![0u8; row_padded];
    for y in (0..h).rev() {
        for x in 0..w {
            let src = (y * w + x) * 4;
            let dst = x * 3;
            // Metal gives BGRA; BMP wants BGR in that same order.
            row[dst] = bgra[src];
            row[dst + 1] = bgra[src + 1];
            row[dst + 2] = bgra[src + 2];
        }
        f.write_all(&row)?;
    }
    f.flush()
}

fn main() -> std::io::Result<()> {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "frame.bmp".to_string());

    // Logical size, then device pixels at 2x, matching a Retina window.
    let scale = 2.0f32;
    let logical_w = std::env::args()
        .find_map(|arg| {
            arg.strip_prefix("--window-width=")
                .and_then(|value| value.parse::<f32>().ok())
        })
        .filter(|width| *width >= 300.0)
        .unwrap_or(1100.0);
    let logical_h = 720.0f32;
    let project_root = std::env::args()
        .find_map(|arg| {
            arg.strip_prefix("--project-root=")
                .map(std::path::PathBuf::from)
        })
        .unwrap_or_else(|| std::env::current_dir().expect("cwd"));
    let (px_w, px_h) = ((logical_w * scale) as usize, (logical_h * scale) as usize);

    let device = MTLCreateSystemDefaultDevice().expect("no Metal device");
    let atlas = Atlas::build("SF Mono", 13.0, scale);
    println!("font        {}", atlas.font_name);

    let mut renderer = Renderer::new(device, atlas);

    if std::env::args().any(|arg| arg == "--atlas-stress") {
        // More unique wide glyphs than all pages can hold. Simulate scrolling
        // through different views so LRU eviction is exercised before drawing.
        for batch in 0..24 {
            renderer.atlas.begin_frame();
            for offset in 0..512 {
                let ch = char::from_u32(0x5000 + batch * 512 + offset).unwrap();
                assert!(renderer.atlas.slot_for(ch).is_some());
            }
        }
    }
    // Positional arguments, skipping flags. Taking `nth(2)` blindly meant
    // `frame_dump out.bmp --git` tried to open a file called "--git" and
    // failed before drawing anything.
    let positional: Vec<String> = std::env::args()
        .skip(2)
        .filter(|arg| !arg.starts_with("--"))
        .collect();
    let file = positional.first().cloned();
    let mut buffer = match &file {
        Some(path) => Buffer::open(path)?,
        None => {
            let mut sample = Buffer::from_text(SAMPLE);
            // A path, so the language is picked the way the app picks it.
            sample.path = Some("sample.rs".into());
            sample
        }
    };
    // Start on line 6, a few characters in, then shift-select across a line
    // break so the selection band, its newline stub and the caret all render.
    for _ in 0..5 {
        buffer.move_down(Motion::Move);
    }
    for _ in 0..8 {
        buffer.move_right(Motion::Move);
    }
    for _ in 0..30 {
        buffer.move_right(Motion::Extend);
    }
    if let Some(range) = positional.get(1)
        && let Some((start, end)) = range.split_once(':')
    {
        let start = start.parse::<usize>().expect("selection start byte");
        let end = end.parse::<usize>().expect("selection end byte");
        buffer.select_range(start, end);
    }
    if let Some(scroll) =
        std::env::args().find_map(|arg| arg.strip_prefix("--scroll=").map(str::to_owned))
    {
        buffer.scroll_column = scroll.parse().expect("horizontal scroll column");
    }
    println!(
        "selection   {:?} bytes: {:?}",
        buffer.selection().map(|r| r.end - r.start),
        buffer.selected_text().map(|t| t.replace('\n', "\\n"))
    );

    // The app's own layout, not a copy of it. This dump once worked the
    // rects out for itself and drifted: a 19pt tab bar where the app has 28,
    // no padding above the text, no status line. Everything it showed was
    // drawn correctly, in places the app does not draw it.
    const SIDEBAR: f32 = 240.0;
    let chrome = Chrome::new(Viewport::new(logical_w, logical_h), Some(SIDEBAR), 0);
    let sidebar_rect = chrome.sidebar.expect("the window is wide enough for one");
    let tab_rect = chrome.tabs;
    let viewport = chrome.text;

    let mut tree = Tree::new();
    tree.open(project_root.clone());
    // Expand src so the dump shows nesting, indentation and the markers.
    if let Some(i) = tree.rows().iter().position(|e| e.name == "src") {
        tree.toggle(i);
    }
    if let Some(i) = tree.rows().iter().position(|e| e.name == "render") {
        tree.toggle(i);
        tree.select(i + 2);
    }

    // Three tabs, with the middle one edited so the dirty marker shows.
    let mut docs = Documents::new(buffer);
    let mut scratch = Buffer::from_text("// notes\n");
    scratch.insert("edited");
    docs.push(scratch);
    docs.push(Buffer::from_text("# README\n"));
    docs.switch(0);
    let tab_stress = std::env::args().any(|arg| arg == "--tab-stress");
    if tab_stress {
        for name in [
            "issues.md",
            "roadmap.md",
            "session-history.md",
            "dependency-review.md",
            "CONTRIBUTING.md",
            "Cargo.toml",
        ] {
            let mut tab = Buffer::from_text("");
            tab.path = Some(std::env::current_dir().unwrap().join(name));
            docs.add(tab);
        }
    }

    let theme = Theme::default();
    let mut glyphs = Vec::new();
    let mut tab_hits = Vec::new();
    // Highlighted by the store the app uses, predicates, embedded languages
    // and all, rather than by a highlighter driven by hand.
    let mut syntax = SyntaxStore::new();
    syntax.update(docs.active_mut(), usize::MAX);
    let active = docs.active();
    let spans = syntax.spans_with(active.id(), 0..active.rope.len_bytes(), |r| {
        active.rope.slice_to_string(r)
    });
    println!("syntax      {} spans", spans.len());
    let mut stats = layout::build_full(
        docs.active(),
        &mut renderer.atlas,
        viewport,
        &theme,
        "",
        &spans,
        &mut glyphs,
    );
    // The live display link polls pending shaping without waiting for input.
    // Export the settled shaped view, not the temporary character fallback.
    let shaping_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while renderer.atlas.has_pending_shaping() {
        assert!(
            std::time::Instant::now() < shaping_deadline,
            "shaping worker timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
        stats = layout::build_full(
            docs.active(),
            &mut renderer.atlas,
            viewport,
            &theme,
            "",
            &spans,
            &mut glyphs,
        );
    }
    // Export the first frame after an unrelated newline edit, proving that
    // the native paragraph survives a row shift without a fallback frame.
    if std::env::args().any(|arg| arg == "--reuse-edit") {
        let active = docs.active();
        let original = renderer
            .atlas
            .shape_editor_line(
                (active.id(), 0),
                &active.rope,
                0..active.rope.line_to_byte(1),
            )
            .expect("--reuse-edit requires a shaped first line");
        let selection = active.selection();
        let prefix = "edited header\n";
        let active = docs.active_mut();
        active.place_cursor(0, Motion::Move);
        active.insert(prefix);
        if let Some(range) = selection {
            active.select_range(range.start + prefix.len(), range.end + prefix.len());
        }
        syntax.update(active, usize::MAX);
        let spans = syntax.spans_with(active.id(), 0..active.rope.len_bytes(), |r| {
            active.rope.slice_to_string(r)
        });
        stats = layout::build_full(
            active,
            &mut renderer.atlas,
            viewport,
            &theme,
            "",
            &spans,
            &mut glyphs,
        );
        let reused = renderer
            .atlas
            .cached_editor_line((active.id(), 1), &active.rope)
            .expect("edited frame lost the unchanged native paragraph");
        assert!(std::ptr::eq(original.as_ref(), reused));
        assert!(!renderer.atlas.has_pending_shaping());
        println!("reuse       unchanged native paragraph retained after newline insertion");
    }
    println!(
        "atlas       {}x{} px, {} resident glyphs, {} pages",
        renderer.atlas.width,
        renderer.atlas.height,
        renderer.atlas.resident(),
        renderer.atlas.page_count()
    );
    layout::build_toolbar(
        &tree,
        &mut renderer.atlas,
        chrome.toolbar,
        &theme,
        &mut glyphs,
    );
    layout::build_tab_bar(
        &docs,
        if tab_stress { docs.active_index() } else { 0 },
        None,
        &mut renderer.atlas,
        tab_rect,
        &theme,
        &mut glyphs,
        &mut tab_hits,
    );
    println!("tabs        {} drawn", tab_hits.len());
    layout::build_breadcrumbs(
        docs.active(),
        &tree,
        &mut renderer.atlas,
        chrome.breadcrumbs,
        &theme,
        &mut glyphs,
    );
    // The rendered Markdown view of the given file, drawn through the same
    // call the window makes. `--live=N` shows line N as its own source, the
    // way the preview does for the line being edited.
    if std::env::args().any(|arg| arg == "--markdown") {
        let source = docs.active().rope.to_string();
        let blocks = crc::markdown::parse_spanned(&source);
        let active = std::env::args()
            .find_map(|arg| arg.strip_prefix("--live=").map(str::to_owned))
            .and_then(|line| line.parse::<usize>().ok())
            .map(|line| docs.active().rope.line_to_byte(line.saturating_sub(1)));
        let mut md_hits = Vec::new();
        let drawn = layout::build_markdown(
            &blocks,
            &source,
            active,
            None,
            0,
            &mut renderer.atlas,
            viewport,
            &theme,
            &mut glyphs,
            &mut md_hits,
        );
        println!(
            "markdown    {} blocks, {drawn} drawn, {} hits",
            blocks.len(),
            md_hits.len()
        );
        layout::build_toolbar(
            &tree,
            &mut renderer.atlas,
            chrome.toolbar,
            &theme,
            &mut glyphs,
        );
        layout::build_tab_bar(
            &docs,
            0,
            None,
            &mut renderer.atlas,
            tab_rect,
            &theme,
            &mut glyphs,
            &mut tab_hits,
        );
        layout::build_breadcrumbs(
            docs.active(),
            &tree,
            &mut renderer.atlas,
            chrome.breadcrumbs,
            &theme,
            &mut glyphs,
        );
        layout::build_sidebar(
            &tree,
            false,
            &mut renderer.atlas,
            sidebar_rect,
            &theme,
            &mut glyphs,
        );
        let bgra = renderer.render_offscreen(
            px_w,
            px_h,
            &glyphs,
            (logical_w, logical_h),
            theme.background,
        );
        write_bmp(&out, px_w, px_h, &bgra)?;
        println!("wrote       {out}");
        return Ok(());
    }

    // The find bar, drawn from the same geometry the window hit-tests.
    if std::env::args().any(|arg| arg == "--find") {
        let find_rect = Viewport {
            x: viewport.x,
            y: chrome.breadcrumbs.y + chrome.breadcrumbs.height,
            width: viewport.width,
            height: layout::FIND_ROW_HEIGHT * 2.0,
        };
        let g = layout::FindGeometry::new(find_rect);
        layout::push_rect(
            &mut glyphs,
            &renderer.atlas,
            [find_rect.x, find_rect.y],
            [find_rect.width, find_rect.height],
            theme.find_background,
        );
        for (rect, placeholder, text, focused, trailing) in [
            (g.find_field, "Find", "viewport", true, Some("3 of 17")),
            (g.replace_field, "Replace with", "", false, None),
        ] {
            if focused {
                layout::push_focus_ring(&mut glyphs, rect, 6.0, 1.5);
            }
            layout::push_rounded_rect(&mut glyphs, rect, 6.0, theme.tab_active);
            let inner = Viewport {
                x: rect.x + 10.0,
                width: (rect.width - 20.0).max(0.0),
                ..rect
            };
            let mut room = inner.width;
            if let Some(trailing) = trailing {
                layout::push_ui_text_right(
                    &mut glyphs,
                    &mut renderer.atlas,
                    inner,
                    trailing,
                    theme.status_text,
                );
                room -= layout::ui_text_width(&mut renderer.atlas, trailing) + 12.0;
            }
            layout::push_ui_text(
                &mut glyphs,
                &mut renderer.atlas,
                Viewport {
                    width: room,
                    ..inner
                },
                if text.is_empty() { placeholder } else { text },
                if text.is_empty() {
                    theme.status_text
                } else {
                    theme.text
                },
            );
        }
        for (rect, label, enabled) in [
            (g.previous, "\u{2039}", true),
            (g.next, "\u{203a}", true),
            (g.close, "\u{2715}", true),
            (g.replace_one, "Replace", true),
            (g.replace_all, "All", true),
        ] {
            layout::push_rounded_rect(&mut glyphs, rect, 5.0, theme.tab_hover);
            layout::push_ui_text_centered(
                &mut glyphs,
                &mut renderer.atlas,
                rect,
                label,
                if enabled {
                    theme.tab_text
                } else {
                    theme.gutter_text
                },
            );
        }
        for (slot, (label, on)) in [
            ("Aa", true),
            ("Word", false),
            (".*", false),
            ("Project", false),
        ]
        .into_iter()
        .enumerate()
        {
            let rect = g.options[slot];
            layout::push_rounded_rect(
                &mut glyphs,
                rect,
                5.0,
                if on {
                    theme.palette_selected
                } else {
                    theme.tab_hover
                },
            );
            layout::push_ui_text_centered(
                &mut glyphs,
                &mut renderer.atlas,
                rect,
                label,
                if on { theme.accent } else { theme.status_text },
            );
        }
        println!("find        two rows, {} option chips", g.options.len());
    }

    // The palette, over everything, with a query that exercises scoring.
    let mut finder = Finder::new();
    finder.scan(project_root.clone());
    let query = "rlay";
    let hits = finder.search(query, layout::PALETTE_RESULTS);
    println!(
        "palette     {} files indexed, {} hits for {query:?}",
        finder.len(),
        hits.len()
    );
    if let Some(top) = hits.first().and_then(|h| finder.entry(h.index)) {
        println!("            best match: {}", top.relative);
    }

    let sidebar_rows = layout::build_sidebar(
        &tree,
        false,
        &mut renderer.atlas,
        sidebar_rect,
        &theme,
        &mut glyphs,
    );
    println!(
        "sidebar     {sidebar_rows} rows of {} in the tree",
        tree.len()
    );
    println!(
        "layout      {} quads over {} lines ({} unsupported)",
        stats.quads, stats.lines, stats.unsupported
    );

    let palette_rect = layout::palette_rect(Viewport::new(logical_w, logical_h), hits.len());
    if file.is_none() {
        layout::push_rect(
            &mut glyphs,
            &renderer.atlas,
            [0.0, 0.0],
            [logical_w, logical_h],
            theme.scrim,
        );
        let rows: Vec<layout::PaletteRow> = hits
            .iter()
            .filter_map(|hit| layout::palette_file_row(&finder, hit))
            .collect();
        layout::build_palette(
            layout::PaletteView {
                rows: &rows,
                heading: "Project files",
                empty: "No matching files",
                placeholder: "Find a file",
                action: "Open",
                query,
                selected: 1,
                scroll: 0,
                cursor: query.len(),
                // `--select-query` exports the state Select All leaves the
                // field in, which used to draw nothing at all.
                selection: std::env::args()
                    .any(|arg| arg == "--select-query")
                    .then_some(0..query.len()),
            },
            &mut renderer.atlas,
            palette_rect,
            &theme,
            &mut glyphs,
        );
    }

    // Source control as it is really composed: the sidebar column plus the
    // diff in the editor column, not a panel drawn over a window.
    if std::env::args().any(|arg| arg == "--git") {
        let mut panel = crc::platform::git_panel::Panel::new(project_root);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let settle = |panel: &mut crc::platform::git_panel::Panel| {
            while panel.busy() {
                assert!(std::time::Instant::now() < deadline, "git worker timed out");
                panel.poll();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        };
        settle(&mut panel);
        // Select a change so the frame shows a real parsed diff rather than
        // an empty column.
        if panel
            .snapshot
            .as_ref()
            .is_some_and(|s| !s.changes.is_empty())
        {
            panel.select(panel.selected);
            settle(&mut panel);
            panel.showing_diff = true;
        }
        glyphs.clear();
        layout::push_rect(
            &mut glyphs,
            &renderer.atlas,
            [viewport.x, viewport.y],
            [viewport.width, viewport.height],
            theme.tab_active,
        );
        panel.draw_diff(&mut renderer.atlas, viewport, &theme, &mut glyphs);
        layout::build_toolbar(
            &tree,
            &mut renderer.atlas,
            chrome.toolbar,
            &theme,
            &mut glyphs,
        );
        layout::build_tab_bar(
            &docs,
            0,
            None,
            &mut renderer.atlas,
            tab_rect,
            &theme,
            &mut glyphs,
            &mut tab_hits,
        );
        // The breadcrumb row names the change on screen, as the window does.
        layout::push_rect(
            &mut glyphs,
            &renderer.atlas,
            [chrome.breadcrumbs.x, chrome.breadcrumbs.y],
            [chrome.breadcrumbs.width, chrome.breadcrumbs.height],
            theme.tab_active,
        );
        layout::push_ui_text(
            &mut glyphs,
            &mut renderer.atlas,
            Viewport {
                x: chrome.breadcrumbs.x + 12.0,
                width: (chrome.breadcrumbs.width - 120.0).max(0.0),
                ..chrome.breadcrumbs
            },
            &panel.diff_title(),
            theme.text,
        );
        layout::push_ui_text_right(
            &mut glyphs,
            &mut renderer.atlas,
            Viewport {
                width: (chrome.breadcrumbs.width - 12.0).max(0.0),
                ..chrome.breadcrumbs
            },
            &panel.diff_summary(),
            theme.status_text,
        );
        layout::build_sidebar(
            &tree,
            true,
            &mut renderer.atlas,
            sidebar_rect,
            &theme,
            &mut glyphs,
        );
        panel.draw_sidebar(&mut renderer.atlas, sidebar_rect, &theme, &mut glyphs);
        println!(
            "git         {} entries, diff {}",
            panel.entries().len(),
            panel.diff_summary()
        );
    }

    let bgra = renderer.render_offscreen(
        px_w,
        px_h,
        &glyphs,
        (logical_w, logical_h),
        theme.background,
    );

    // A frame that is entirely the clear colour means the draw silently did
    // nothing, which is the failure this dump exists to catch.
    let bg8 = [
        (theme.background[2] * 255.0).round() as u8,
        (theme.background[1] * 255.0).round() as u8,
        (theme.background[0] * 255.0).round() as u8,
    ];
    let non_background = bgra
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| {
            (p[0] as i16 - bg8[0] as i16).abs() > 6
                || (p[1] as i16 - bg8[1] as i16).abs() > 6
                || (p[2] as i16 - bg8[2] as i16).abs() > 6
        })
        .count();
    println!(
        "frame       {px_w}x{px_h}, {:.2}% of pixels differ from the background",
        non_background as f64 / (px_w * px_h) as f64 * 100.0
    );
    assert!(
        non_background > 0,
        "the frame is entirely background: nothing drew"
    );

    write_bmp(&out, px_w, px_h, &bgra)?;
    println!("wrote       {out}");
    Ok(())
}
