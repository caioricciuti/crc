//! LZFSE through the system libcompression.
//!
//! macOS ships libcompression and the linker resolves `-lcompression`
//! through the SDK stub, so this costs zero crates and zero build scripts,
//! the same move as `src/index/ffi.rs` makes for SQLite. LZFSE is Apple's
//! own codec: about zlib's ratio on text at several times its speed.
//!
//! Both calls are one-shot buffer to buffer. A result that does not fit,
//! or does not shrink, is stored raw; a stored blob that does not decode
//! to exactly the recorded size is corrupt and decodes to `None`.

use std::cell::RefCell;
use std::ffi::c_void;

const COMPRESSION_LZFSE: u32 = 0x801;

#[link(name = "compression")]
unsafe extern "C" {
    fn compression_encode_scratch_buffer_size(algorithm: u32) -> usize;
    fn compression_encode_buffer(
        dst: *mut u8,
        dst_size: usize,
        src: *const u8,
        src_size: usize,
        scratch: *mut c_void,
        algorithm: u32,
    ) -> usize;
    fn compression_decode_scratch_buffer_size(algorithm: u32) -> usize;
    fn compression_decode_buffer(
        dst: *mut u8,
        dst_size: usize,
        src: *const u8,
        src_size: usize,
        scratch: *mut c_void,
        algorithm: u32,
    ) -> usize;
}

/// How a blob's bytes are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Raw,
    Lzfse,
}

impl Codec {
    /// The value written to the database. Never renumber.
    pub fn id(self) -> i64 {
        match self {
            Codec::Raw => 0,
            Codec::Lzfse => 1,
        }
    }

    pub fn from_id(id: i64) -> Option<Codec> {
        match id {
            0 => Some(Codec::Raw),
            1 => Some(Codec::Lzfse),
            _ => None,
        }
    }
}

thread_local! {
    // One scratch buffer per thread, grown to the larger of what encode and
    // decode ask for and then reused. `u64` for its alignment.
    static SCRATCH: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

fn with_scratch<R>(bytes: usize, f: impl FnOnce(*mut c_void) -> R) -> R {
    SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        let words = bytes.div_ceil(8);
        if scratch.len() < words {
            scratch.resize(words, 0);
        }
        f(scratch.as_mut_ptr().cast())
    })
}

/// Compresses `src`, or keeps it as is when LZFSE does not make it smaller.
pub fn encode(src: &[u8]) -> (Codec, Vec<u8>) {
    if src.is_empty() {
        return (Codec::Raw, Vec::new());
    }
    let mut dst: Vec<u8> = Vec::with_capacity(src.len());
    // SAFETY: the scratch size comes from the library for this algorithm.
    let scratch_size = unsafe { compression_encode_scratch_buffer_size(COMPRESSION_LZFSE) };
    let written = with_scratch(scratch_size, |scratch| {
        // SAFETY: reads `src.len()` bytes of `src`, writes at most
        // `src.len()` bytes into `dst`'s capacity, and uses `scratch_size`
        // bytes of scratch.
        unsafe {
            compression_encode_buffer(
                dst.as_mut_ptr(),
                src.len(),
                src.as_ptr(),
                src.len(),
                scratch,
                COMPRESSION_LZFSE,
            )
        }
    });
    // 0 means it did not fit in `src.len()` bytes; equal is no gain.
    if written == 0 || written >= src.len() {
        return (Codec::Raw, src.to_vec());
    }
    // SAFETY: the library initialized the first `written` bytes, and
    // `written` is within the capacity it was given.
    unsafe { dst.set_len(written) };
    (Codec::Lzfse, dst)
}

/// The original bytes of a blob that decodes to exactly `size` bytes, or
/// `None` when it is corrupt, truncated, or recorded with another size.
pub fn decode(codec: Codec, data: &[u8], size: usize) -> Option<Vec<u8>> {
    match codec {
        Codec::Raw => (data.len() == size).then(|| data.to_vec()),
        Codec::Lzfse => {
            // `encode` never stores an empty text as LZFSE, and the library
            // reports failure as 0, so the two cannot be told apart.
            if size == 0 || data.is_empty() {
                return None;
            }
            // One byte of room past `size`, so a stream that holds more
            // than was recorded fills it and is refused rather than cut.
            let room = size.checked_add(1)?;
            let mut dst: Vec<u8> = Vec::new();
            // A corrupt size column must not abort the process.
            dst.try_reserve_exact(room).ok()?;
            // SAFETY: the scratch size comes from the library for this algorithm.
            let scratch_size = unsafe { compression_decode_scratch_buffer_size(COMPRESSION_LZFSE) };
            let written = with_scratch(scratch_size, |scratch| {
                // SAFETY: reads `data.len()` bytes of `data`, writes at most
                // `room` bytes into `dst`'s reserved capacity, and uses
                // `scratch_size` bytes of scratch.
                unsafe {
                    compression_decode_buffer(
                        dst.as_mut_ptr(),
                        room,
                        data.as_ptr(),
                        data.len(),
                        scratch,
                        COMPRESSION_LZFSE,
                    )
                }
            });
            if written != size {
                return None;
            }
            // SAFETY: the library initialized the first `written` bytes,
            // within the reserved capacity.
            unsafe { dst.set_len(written) };
            Some(dst)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(src: &[u8]) -> Codec {
        let (codec, data) = encode(src);
        assert_eq!(decode(codec, &data, src.len()).as_deref(), Some(src));
        codec
    }

    /// Bytes no compressor can shrink, from a fixed-seed xorshift.
    fn noise(len: usize) -> Vec<u8> {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    fn repetitive(len: usize) -> Vec<u8> {
        let line = "fn main() {\n    println!(\"hello, world\");\n}\n";
        line.repeat(len / line.len() + 1).into_bytes()[..len].to_vec()
    }

    #[test]
    fn empty_is_raw_and_round_trips() {
        assert_eq!(encode(b""), (Codec::Raw, Vec::new()));
        assert_eq!(round_trip(b""), Codec::Raw);
    }

    #[test]
    fn text_round_trips() {
        round_trip(b"a");
        round_trip(b"just some ascii text, short enough that it may stay raw");
        round_trip(
            "\u{6f22}\u{5b57}\u{304b}\u{306a} \u{1f600}\u{1f469}\u{200d}\u{1f4bb} e\u{301}a\u{308}"
                .as_bytes(),
        );
        round_trip(b"line one\r\nline two\r\n\r\nline four\r\n");
        let mixed = "caf\u{e9} \u{4e16}\u{754c} \u{1f30d}\r\n".repeat(500);
        assert_eq!(round_trip(mixed.as_bytes()), Codec::Lzfse);
        let crlf = "let x = 1;\r\n".repeat(1000);
        assert_eq!(round_trip(crlf.as_bytes()), Codec::Lzfse);
    }

    #[test]
    fn repetitive_text_compresses_well() {
        let src = repetitive(2 << 20);
        let (codec, data) = encode(&src);
        assert_eq!(codec, Codec::Lzfse);
        assert!(
            data.len() * 50 < src.len(),
            "{} bytes from {}",
            data.len(),
            src.len()
        );
        assert_eq!(decode(codec, &data, src.len()), Some(src));
    }

    #[test]
    fn incompressible_bytes_stay_raw() {
        let src = noise(64 << 10);
        let (codec, data) = encode(&src);
        assert_eq!(codec, Codec::Raw);
        assert_eq!(data, src);
        assert_eq!(decode(codec, &data, src.len()), Some(src));
    }

    #[test]
    fn wrong_size_is_refused() {
        let src = repetitive(100_000);
        let (codec, data) = encode(&src);
        assert_eq!(codec, Codec::Lzfse);
        assert_eq!(decode(codec, &data, src.len() - 1), None);
        assert_eq!(decode(codec, &data, src.len() + 1), None);
        assert_eq!(decode(codec, &data, 0), None);
        assert_eq!(decode(Codec::Raw, b"abc", 2), None);
        assert_eq!(decode(Codec::Raw, b"abc", 4), None);
        // A size no allocation can hold is refused, not an abort.
        assert_eq!(decode(codec, &data, usize::MAX), None);
        assert_eq!(decode(codec, &data, usize::MAX - 1), None);
    }

    #[test]
    fn corrupt_input_is_refused() {
        let src = repetitive(100_000);
        let (codec, data) = encode(&src);
        assert_eq!(codec, Codec::Lzfse);
        // Every truncation, including down to nothing.
        for cut in (0..data.len()).step_by(7).chain([data.len() - 1]) {
            assert_eq!(decode(codec, &data[..cut], src.len()), None, "cut at {cut}");
        }
        // A flipped byte anywhere must not panic or read past the input.
        // LZFSE has no checksum, so a flip inside a literal can still
        // decode to the right size with other bytes: the blob's SHA-256 is
        // what catches that, not this layer.
        for at in (0..data.len()).step_by(3) {
            let mut bad = data.clone();
            bad[at] ^= 0x5a;
            if let Some(out) = decode(codec, &bad, src.len()) {
                assert_eq!(out.len(), src.len());
            }
        }
        // Garbage, with and without a plausible block header.
        assert_eq!(decode(codec, &noise(4096), src.len()), None);
        let mut fake = b"bvx2".to_vec();
        fake.extend(noise(512));
        assert_eq!(decode(codec, &fake, src.len()), None);
        assert_eq!(decode(codec, b"bvx$", 10), None);
        assert_eq!(decode(codec, b"", 10), None);
    }

    #[test]
    fn codec_ids_are_stable() {
        for codec in [Codec::Raw, Codec::Lzfse] {
            assert_eq!(Codec::from_id(codec.id()), Some(codec));
        }
        assert_eq!(Codec::Raw.id(), 0);
        assert_eq!(Codec::Lzfse.id(), 1);
        assert_eq!(Codec::from_id(2), None);
    }
}
