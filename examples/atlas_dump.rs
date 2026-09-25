//! Writes the glyph atlas out as a BMP so it can be inspected by eye.
//!
//! Unit tests can confirm a glyph has ink in it; they cannot tell you the
//! baseline is off by three pixels or the whole sheet is upside down. This
//! exists so that is checked by looking.
//!
//! Run with: cargo run --offline --example atlas_dump

use crc::render::font::Atlas;
use std::io::Write;

fn write_bmp(path: &str, w: u32, h: u32, gray: &[u8]) -> std::io::Result<()> {
    // 24-bit uncompressed BMP. Rows are bottom-up and padded to 4 bytes.
    let row_padded = ((w * 3) as usize).div_ceil(4) * 4;
    let pixel_bytes = row_padded * h as usize;
    let file_size = 54 + pixel_bytes;

    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"BM")?;
    f.write_all(&(file_size as u32).to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&54u32.to_le_bytes())?;
    f.write_all(&40u32.to_le_bytes())?; // DIB header size
    f.write_all(&(w as i32).to_le_bytes())?;
    f.write_all(&(h as i32).to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // planes
    f.write_all(&24u16.to_le_bytes())?; // bpp
    f.write_all(&0u32.to_le_bytes())?; // no compression
    f.write_all(&(pixel_bytes as u32).to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?; // ~72 DPI
    f.write_all(&2835i32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;

    let mut row = vec![0u8; row_padded];
    for y in (0..h).rev() {
        for x in 0..w {
            let v = gray[(y * w + x) as usize];
            let o = (x * 3) as usize;
            row[o] = v;
            row[o + 1] = v;
            row[o + 2] = v;
        }
        f.write_all(&row)?;
    }
    f.flush()
}

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

    write_bmp(&out, atlas.width, atlas.height, &atlas.pixels)?;
    println!("wrote       {out}");
    Ok(())
}
