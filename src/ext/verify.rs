//! Checking what crc downloads: SHA-256 of a module against the registry,
//! and the registry's ECDSA P-256 signature against the key built into the
//! app. Both through what macOS ships (CommonCrypto and Security.framework),
//! so no cryptography is written here and none is added as a dependency.

use std::ffi::c_void;

/// The public key official registries are signed with, as PEM. Replaced in
/// the file, never at runtime. A file without a key means this build trusts
/// no registry.
const REGISTRY_KEY: &str = include_str!("registry-key.pem");

#[link(name = "System")]
unsafe extern "C" {
    fn CC_SHA256(data: *const c_void, len: u32, md: *mut u8) -> *mut u8;
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

type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;

#[repr(C)]
struct CFDictionaryCallBacks {
    _private: [u8; 0],
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: CFDictionaryCallBacks;
    static kCFTypeDictionaryValueCallBacks: CFDictionaryCallBacks;
    fn CFDataCreate(allocator: CFAllocatorRef, bytes: *const u8, length: isize) -> CFTypeRef;
    fn CFDictionaryCreate(
        allocator: CFAllocatorRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        key_callbacks: *const CFDictionaryCallBacks,
        value_callbacks: *const CFDictionaryCallBacks,
    ) -> CFTypeRef;
    fn CFRelease(value: CFTypeRef);
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecAttrKeyType: CFTypeRef;
    static kSecAttrKeyTypeECSECPrimeRandom: CFTypeRef;
    static kSecAttrKeyClass: CFTypeRef;
    static kSecAttrKeyClassPublic: CFTypeRef;
    static kSecKeyAlgorithmECDSASignatureMessageX962SHA256: CFTypeRef;
    fn SecKeyCreateWithData(
        data: CFTypeRef,
        attributes: CFTypeRef,
        error: *mut CFTypeRef,
    ) -> CFTypeRef;
    fn SecKeyVerifySignature(
        key: CFTypeRef,
        algorithm: CFTypeRef,
        signed: CFTypeRef,
        signature: CFTypeRef,
        error: *mut CFTypeRef,
    ) -> u8;
}

/// Releases a Core Foundation object when dropped.
struct Owned(CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: an object this module created and owns once.
            unsafe { CFRelease(self.0) };
        }
    }
}

fn data(bytes: &[u8]) -> Owned {
    // SAFETY: copies `bytes` into a new CFData.
    Owned(unsafe { CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize) })
}

/// The DER of a PEM block, decoded from base64 by hand (a dozen lines are
/// not worth a crate).
fn pem_der(pem: &str) -> Option<Vec<u8>> {
    let body: String = pem
        .lines()
        .skip_while(|l| !l.starts_with("-----BEGIN PUBLIC KEY-----"))
        .skip(1)
        .take_while(|l| !l.starts_with("-----END"))
        .collect();
    if body.is_empty() {
        return None;
    }
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in body
        .bytes()
        .filter(|&c| c != b'=' && !c.is_ascii_whitespace())
    {
        acc = (acc << 6) | value(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The 65-byte uncompressed point of a P-256 SubjectPublicKeyInfo, the only
/// shape accepted: the prefix pins both the algorithm and the curve.
fn p256_point(der: &[u8]) -> Option<&[u8]> {
    const PREFIX: [u8; 26] = [
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
    ];
    let point = der.strip_prefix(&PREFIX[..])?;
    (point.len() == 65 && point[0] == 0x04).then_some(point)
}

/// Whether `signature` (DER ECDSA over SHA-256) is `key_pem`'s over
/// `message`. Any malformed input is simply `false`.
pub fn verify_with(key_pem: &str, message: &[u8], signature: &[u8]) -> bool {
    let Some(der) = pem_der(key_pem) else {
        return false;
    };
    let Some(point) = p256_point(&der) else {
        return false;
    };
    let key_data = data(point);
    // SAFETY: the statics are Security.framework constants; the arrays
    // outlive the call, which copies them.
    let attributes = unsafe {
        let keys = [kSecAttrKeyType, kSecAttrKeyClass];
        let values = [kSecAttrKeyTypeECSECPrimeRandom, kSecAttrKeyClassPublic];
        Owned(CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            2,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        ))
    };
    let mut error: CFTypeRef = std::ptr::null();
    // SAFETY: valid CFData and CFDictionary; the error, if any, is ours.
    let key = Owned(unsafe { SecKeyCreateWithData(key_data.0, attributes.0, &mut error) });
    let _error = Owned(error);
    if key.0.is_null() {
        return false;
    }
    let (message, signature) = (data(message), data(signature));
    let mut error: CFTypeRef = std::ptr::null();
    // SAFETY: a valid public key and two CFData; the error, if any, is ours.
    let ok = unsafe {
        SecKeyVerifySignature(
            key.0,
            kSecKeyAlgorithmECDSASignatureMessageX962SHA256,
            message.0,
            signature.0,
            &mut error,
        )
    };
    let _error = Owned(error);
    ok != 0
}

/// Whether this build has a registry key at all.
pub fn has_registry_key() -> bool {
    registry_key().is_some()
}

/// The key registries are checked against: the built-in one, or, in a
/// self-test run only, the test key it names.
fn registry_key() -> Option<String> {
    if std::env::var_os("CRC_SELFTEST").is_some()
        && let Some(path) = std::env::var_os("CRC_EXT_REGISTRY_KEY")
    {
        return std::fs::read_to_string(path).ok();
    }
    pem_der(REGISTRY_KEY).map(|_| REGISTRY_KEY.to_owned())
}

/// Whether `signature` over `index` is the official registry's.
pub fn registry_signed(index: &[u8], signature: &[u8]) -> bool {
    registry_key().is_some_and(|key| verify_with(&key, index, signature))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn the_built_in_registry_key_is_a_p256_key() {
        let der = pem_der(REGISTRY_KEY).expect("src/ext/registry-key.pem holds a key");
        assert!(p256_point(&der).is_some(), "not a P-256 public key");
    }

    #[test]
    fn sha256_matches_the_known_answer() {
        assert_eq!(
            hex(&sha256(b"abc").unwrap()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// A throwaway key pair and a signing function, from macOS's openssl.
    pub(crate) fn test_key(dir: &std::path::Path) -> Option<String> {
        let key = dir.join("key.pem");
        let ok = Command::new("/usr/bin/openssl")
            .args([
                "ecparam",
                "-name",
                "prime256v1",
                "-genkey",
                "-noout",
                "-out",
            ])
            .arg(&key)
            .status()
            .ok()?
            .success();
        let public = Command::new("/usr/bin/openssl")
            .args(["ec", "-pubout", "-in"])
            .arg(&key)
            .output()
            .ok()?;
        (ok && public.status.success())
            .then(|| String::from_utf8_lossy(&public.stdout).into_owned())
    }

    pub(crate) fn sign(dir: &std::path::Path, message: &[u8]) -> Vec<u8> {
        let file = dir.join("message");
        std::fs::write(&file, message).unwrap();
        let out = Command::new("/usr/bin/openssl")
            .args(["dgst", "-sha256", "-sign"])
            .arg(dir.join("key.pem"))
            .arg(&file)
            .output()
            .unwrap();
        out.stdout
    }

    #[test]
    fn verifies_an_openssl_signature_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("crc-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let Some(public) = test_key(&dir) else {
            eprintln!("no openssl; skipping");
            return;
        };
        let signature = sign(&dir, b"index");
        assert!(verify_with(&public, b"index", &signature));
        assert!(
            !verify_with(&public, b"indeX", &signature),
            "another message"
        );
        let mut bent = signature.clone();
        let last = bent.len() - 1;
        bent[last] ^= 1;
        assert!(
            !verify_with(&public, b"index", &bent),
            "a changed signature"
        );
        assert!(!verify_with("not a key", b"index", &signature));
        assert!(!verify_with(&public, b"index", b""));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
