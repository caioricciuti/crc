//! Writes the glyph atlas out as a BMP so it can be inspected by eye.
//!
//! Unit tests can confirm a glyph has ink in it; they cannot tell you the
//! baseline is off by three pixels or the whole sheet is upside down. This
//! exists so that is checked by looking.
//!
//! Run with: cargo run --offline --example atlas_dump

use crc::render::font::Atlas;
#[path = "common/bmp.rs"]
mod bmp;

fn main() -> std::io::Result<()> {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "atlas.bmp".to_string());

    let atlas = Atlas::build("SF Mono", 13.0, 2.0);
    let m = atlas.metrics;

    println!("font        {} (requested \"SF Mono\")", atlas.font_name);
    println!("atlas       {} x {} px", atlas.width, atlas.height);
    println!("cell        {:?} logical pt", atlas.cell_size());
    println!("advance     {:.3} pt", m.advance);
    println!("line height {:.3} pt", m.line_height);
    println!("ascent      {:.3} pt", m.ascent);
    println!("descent     {:.3} pt", m.descent);
    println!("scale       {:.1}x", m.scale);

    let inked = atlas.pixels.iter().filter(|&&p| p > 0).count();
    println!(
        "coverage    {:.1}% of the sheet has ink",
        inked as f64 / atlas.pixels.len() as f64 * 100.0
    );

    let w = atlas.width as usize;
    bmp::write_bmp(&out, w, atlas.height as usize, |x, y| {
        let v = atlas.pixels[y * w + x];
        [v, v, v]
    })?;
    println!("wrote       {out}");
    Ok(())
}
