//! A 24-bit uncompressed BMP writer for the dump examples: no image crate,
//! and any viewer opens it.

use std::io::Write;

/// Writes a `w` by `h` image whose pixel at `(x, y)`, from the top left, is
/// `pixel(x, y)` as blue, green, red. Rows go bottom-up, padded to 4 bytes.
pub fn write_bmp(
    path: &str,
    w: usize,
    h: usize,
    pixel: impl Fn(usize, usize) -> [u8; 3],
) -> std::io::Result<()> {
    let row_padded = (w * 3).div_ceil(4) * 4;
    let pixel_bytes = row_padded * h;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);

    f.write_all(b"BM")?;
    f.write_all(&((54 + pixel_bytes) as u32).to_le_bytes())?;
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
            row[x * 3..x * 3 + 3].copy_from_slice(&pixel(x, y));
        }
        f.write_all(&row)?;
    }
    f.flush()
}
