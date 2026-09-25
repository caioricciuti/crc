//! Measures the editor's CPU work on a single million-character line.
//! Run with: cargo run --release --offline --example long_line_latency

use crc::render::font::Atlas;
use crc::render::layout::{self, Theme, Viewport};
use crc::text::buffer::{Buffer, Motion};
use std::hint::black_box;
use std::time::Instant;

fn p99(samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[(samples.len() * 99).div_ceil(100).saturating_sub(1)]
}

fn measure(label: &str, source: &str, cursor: usize, scroll: usize) {
    let mut buffer = Buffer::from_text(source);
    buffer.place_cursor(cursor, Motion::Move);
    buffer.scroll_column = scroll;
    let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
    let viewport = Viewport::new(1400.0, 900.0);
    let rows = viewport.rows(atlas.metrics.line_height);
    let cols = (viewport.width / atlas.metrics.advance) as usize;
    let mut glyphs = Vec::new();
    let mut layout = Vec::new();
    let mut clamp = Vec::new();
    let mut motion = Vec::new();
    let mut hit_test = Vec::new();
    let mut keyboard = Vec::new();

    for iteration in 0..220 {
        let started = Instant::now();
        buffer.clamp_scroll(rows, cols);
        let clamp_ms = started.elapsed().as_secs_f64() * 1_000.0;

        let started = Instant::now();
        black_box(buffer.position_of(cursor));
        black_box(buffer.offset_at(0, scroll));
        let motion_ms = started.elapsed().as_secs_f64() * 1_000.0;

        let started = Instant::now();
        buffer.move_right(Motion::Move);
        buffer.move_left(Motion::Move);
        let keyboard_ms = started.elapsed().as_secs_f64() * 1000.0;
        buffer.place_cursor(cursor, Motion::Move);

        let started = Instant::now();
        black_box(layout::build(
            &buffer,
            &mut atlas,
            viewport,
            &Theme::default(),
            &mut glyphs,
        ));
        let layout_ms = started.elapsed().as_secs_f64() * 1_000.0;

        let started = Instant::now();
        black_box(layout::offset_at_point(&buffer, &atlas, 600.0, 2.0));
        black_box(layout::caret_rect(&buffer, &atlas, viewport));
        let hit_ms = started.elapsed().as_secs_f64() * 1000.0;

        if iteration == 0 {
            println!("{label}: first layout {layout_ms:.3} ms");
            assert!(
                layout_ms < 1000.0 / 120.0,
                "cold layout exceeds frame budget"
            );
            let started = Instant::now();
            let mut max_poll_ms = 0.0_f64;
            while atlas.has_pending_shaping() {
                assert!(started.elapsed().as_secs() < 30, "shaping did not finish");
                std::thread::sleep(std::time::Duration::from_millis(2));
                let frame = Instant::now();
                layout::build(
                    &buffer,
                    &mut atlas,
                    viewport,
                    &Theme::default(),
                    &mut glyphs,
                );
                max_poll_ms = max_poll_ms.max(frame.elapsed().as_secs_f64() * 1000.0);
            }
            assert!(
                max_poll_ms < 1000.0 / 120.0,
                "shaping installation exceeds frame budget"
            );
            println!(
                "shaping settled in {:.1} ms; polling/install frame max {max_poll_ms:.3} ms",
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
        if iteration == 0
            && let Some(shaped) = atlas.cached_editor_line((buffer.id(), 0), &buffer.rope)
        {
            // Each index is queried once. Repeating one cursor only measures
            // the lazy cache and hides the cost of a newly visited position.
            let step = (shaped.offsets.len() / 200).max(1);
            let mut first = Vec::new();
            for (sample, base) in (0..shaped.offsets.len()).step_by(step).enumerate() {
                // Vary the position within each stratum so repeated fixtures
                // do not accidentally sample only one cluster/bidi affinity.
                let index = base + (sample * 73 + 19) % step.min(shaped.offsets.len() - base);
                let started = Instant::now();
                black_box(shaped.caret_offset(index));
                first.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            let first_p99 = p99(&mut first);
            println!("first caret    p99 {first_p99:.3} ms");
            assert!(
                first_p99 < 1000.0 / 120.0,
                "first caret exceeds frame budget"
            );
        }
        if iteration >= 20 {
            keyboard.push(keyboard_ms);
            hit_test.push(hit_ms);
            clamp.push(clamp_ms);
            motion.push(motion_ms);
            layout.push(layout_ms);
        }
    }

    let keyboard_p99 = p99(&mut keyboard);
    println!("grapheme keys  p99 {keyboard_p99:.3} ms");
    assert!(
        keyboard_p99 < 1000.0 / 120.0,
        "grapheme motion exceeds frame budget"
    );
    let hit_p99 = p99(&mut hit_test);
    println!("click + IME    p99 {hit_p99:.3} ms");
    assert!(hit_p99 < 1000.0 / 120.0, "click/IME exceeds frame budget");
    let clamp_p99 = p99(&mut clamp);
    let motion_p99 = p99(&mut motion);
    println!("scroll clamp   p99 {clamp_p99:.3} ms");
    println!("cursor mapping p99 {motion_p99:.3} ms");
    assert!(
        clamp_p99 < 1000.0 / 120.0,
        "scroll clamp exceeds frame budget"
    );
    assert!(
        motion_p99 < 1000.0 / 120.0,
        "cursor mapping exceeds frame budget"
    );
    let layout_p99 = p99(&mut layout);
    println!("visible layout p99 {layout_p99:.3} ms");
    assert!(
        layout_p99 < 1000.0 / 120.0,
        "visible layout exceeds 120 Hz budget"
    );
}

fn measure_edits(source: &str) {
    let mut buffer = Buffer::from_text(source);
    let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
    let viewport = Viewport::new(1400.0, 900.0);
    let mut glyphs = Vec::new();
    let mut samples = Vec::new();
    for _ in 0..24 {
        let start = Instant::now();
        buffer.insert("x"); // Every sample has new line content: no warmed cache.
        layout::build(
            &buffer,
            &mut atlas,
            viewport,
            &Theme::default(),
            &mut glyphs,
        );
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let edit_p99 = p99(&mut samples);
    println!("Unicode edit + layout p99 {edit_p99:.3} ms");
    assert!(
        edit_p99 < 1000.0 / 120.0,
        "Unicode edit blocks the UI beyond the frame budget"
    );
}

fn measure_selection(source: &str) {
    let mut buffer = Buffer::from_text(source);
    buffer.place_cursor(source.len(), Motion::Extend);
    buffer.scroll_column = 8000;
    let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
    let viewport = Viewport::new(1400.0, 900.0);
    let mut glyphs = Vec::new();
    layout::build(
        &buffer,
        &mut atlas,
        viewport,
        &Theme::default(),
        &mut glyphs,
    );
    let deadline = Instant::now();
    while atlas.has_pending_shaping() {
        assert!(deadline.elapsed().as_secs() < 30);
        std::thread::sleep(std::time::Duration::from_millis(2));
        layout::build(
            &buffer,
            &mut atlas,
            viewport,
            &Theme::default(),
            &mut glyphs,
        );
    }
    let mut samples = Vec::new();
    for _ in 0..200 {
        let start = Instant::now();
        layout::build(
            &buffer,
            &mut atlas,
            viewport,
            &Theme::default(),
            &mut glyphs,
        );
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let p99 = p99(&mut samples);
    println!(
        "whole-line selection {} bytes p99 {p99:.3} ms",
        source.len()
    );
    assert!(p99 < 1000.0 / 120.0, "selection exceeds frame budget");
}

fn measure_unchanged_paragraph_edits(source: &str) {
    let mut buffer = Buffer::from_text(&format!("header\n{source}\ntail"));
    let mut atlas = Atlas::build("SF Mono", 13.0, 2.0);
    let viewport = Viewport::new(1400.0, 900.0);
    let mut glyphs = Vec::new();
    let theme = Theme::default();
    layout::build(&buffer, &mut atlas, viewport, &theme, &mut glyphs);
    let deadline = Instant::now();
    while atlas.has_pending_shaping() {
        assert!(deadline.elapsed().as_secs() < 30);
        std::thread::sleep(std::time::Duration::from_millis(2));
        layout::build(&buffer, &mut atlas, viewport, &theme, &mut glyphs);
    }
    let original = atlas
        .shape_editor_line(
            (buffer.id(), 1),
            &buffer.rope,
            buffer.rope.line_to_byte(1)..buffer.rope.line_to_byte(2),
        )
        .expect("large paragraph must be natively shaped before measuring reuse");
    let mut samples = Vec::new();
    for _ in 0..100 {
        for undo in [false, true] {
            let start = Instant::now();
            if undo {
                buffer.undo();
            } else {
                buffer.insert("x\n");
            }
            layout::build(&buffer, &mut atlas, viewport, &theme, &mut glyphs);
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
            let line = if undo { 1 } else { 2 };
            let cached = atlas
                .cached_editor_line((buffer.id(), line), &buffer.rope)
                .expect("unaffected paragraph must never fall back during edits");
            assert!(
                std::ptr::eq(original.as_ref(), cached),
                "edit restarted unchanged geometry"
            );
            assert!(
                !atlas.has_pending_shaping(),
                "edit queued redundant shaping"
            );
        }
    }
    let latency = p99(&mut samples);
    println!(
        "unchanged paragraph {} bytes: edit/undo + layout p99 {latency:.3} ms",
        source.len()
    );
    assert!(
        latency < 1000.0 / 120.0,
        "geometry reuse exceeds frame budget"
    );
}

fn main() {
    measure("million ASCII", &"a".repeat(1_000_000), 900_000, 899_950);
    let million_unicode = "é漢🌍".repeat(350_000);
    measure("million Unicode scalars", &million_unicode, 0, 0);
    measure(
        "million Unicode deeply scrolled",
        &million_unicode,
        2_700_000,
        1_499_950,
    );
    let tabbed = "é漢\t🌍x\t".repeat(150_000);
    measure(
        "tabbed Unicode deeply scrolled",
        &tabbed,
        1_200_000,
        799_950,
    );
    let mut rope_buffer = Buffer::from_text(&million_unicode);
    let mut mapping = Vec::new();
    for i in 0..1000 {
        let start = Instant::now();
        black_box(rope_buffer.position_of(3_000_000));
        black_box(rope_buffer.offset_at(0, 900_000 + i));
        rope_buffer.clamp_scroll(40, 160);
        mapping.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let mapping_p99 = p99(&mut mapping);
    println!("million Unicode mapping + clamp p99 {mapping_p99:.3} ms");
    assert!(
        mapping_p99 < 1000.0 / 120.0,
        "deep Unicode mapping exceeds frame budget"
    );
    let unicode = "e\u{301} 👩‍💻 لا שלום 漢字\t".repeat(1200);
    measure_edits(&unicode);
    measure("long Unicode", &unicode, 0, 0);
    measure("long Unicode scrolled", &unicode, unicode.len() / 2, 8000);
    let larger = "e\u{301} 👩‍💻 لا שלום 漢字\t".repeat(2400);
    measure_edits(&larger);
    measure("Unicode beyond 64 KiB", &larger, 0, 0);
    measure_selection(&larger);
    let near_limit = format!("{}e\u{301} 👩‍💻", "a".repeat(950_000));
    measure_edits(&near_limit);
    measure("near shaping limit", &near_limit, 949_980, 949_950);
    measure_selection(&near_limit);
    let beyond_limit = format!("{}e\u{301} 👩‍💻 لا שלום", "a".repeat(1_200_000));
    measure_edits(&beyond_limit);
    measure(
        "beyond old 1 MiB limit",
        &beyond_limit,
        1_200_000,
        1_199_980,
    );
    measure_selection(&beyond_limit);
    measure_unchanged_paragraph_edits(&beyond_limit);
    let controls = "prefix \u{2067}שלום abc\u{2069} suffix ".repeat(512);
    measure("explicit bidi controls", &controls, 0, 0);
}
