//! SHA-1 and SHA-256 through CommonCrypto, which macOS ships, so no
//! cryptography is written here and none is added as a dependency.

use std::ffi::c_void;

#[link(name = "System")]
unsafe extern "C" {
    fn CC_SHA256(data: *const c_void, len: u32, md: *mut u8) -> *mut u8;
    fn CC_SHA1(data: *const c_void, len: u32, md: *mut u8) -> *mut u8;
}

/// SHA-1, which the WebSocket handshake and Git object ids ask for. Not for
/// anything that must resist a forger.
pub fn sha1(data: &[u8]) -> Option<[u8; 20]> {
    let len = u32::try_from(data.len()).ok()?;
    let mut out = [0u8; 20];
    // SAFETY: reads `len` bytes of `data`, writes 20 into `out`.
    unsafe { CC_SHA1(data.as_ptr().cast(), len, out.as_mut_ptr()) };
    Some(out)
}

pub fn sha256(data: &[u8]) -> Option<[u8; 32]> {
    let len = u32::try_from(data.len()).ok()?;
    let mut out = [0u8; 32];
    // SAFETY: reads `len` bytes of `data`, writes 32 into `out`.
    unsafe { CC_SHA256(data.as_ptr().cast(), len, out.as_mut_ptr()) };
    Some(out)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_digests() {
        assert_eq!(
            hex(&sha256(b"abc").unwrap()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha1(b"abc").unwrap()),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }
}
