//! The official registry: `index.json` and its signature from the latest
//! published release of crc-extensions, and the modules it lists. Fetched
//! with `/usr/bin/curl` like the update check, on a worker thread. Nothing
//! from it is trusted until the signature verifies against the key built
//! into crc, and no module until its SHA-256 matches the signed index.

use super::manifest::{self, Manifest};
use super::store::Package;
use crate::json::Value;

pub const DEFAULT_BASE: &str =
    "https://github.com/caioricciuti/crc-extensions/releases/latest/download/";

/// One extension the registry offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub manifest: Manifest,
    pub manifest_json: String,
    pub wasm: String,
    pub sha256: String,
    pub size: u64,
    pub readme: String,
}

/// Where the registry is: the release download base, or in a self-test run
/// the folder `CRC_EXT_REGISTRY` names.
fn base() -> String {
    if std::env::var_os("CRC_SELFTEST").is_some()
        && let Ok(base) = std::env::var("CRC_EXT_REGISTRY")
    {
        return base;
    }
    DEFAULT_BASE.to_owned()
}

fn fetch(url: &str, max: u64) -> Result<Vec<u8>, String> {
    // file:// only in a self-test run; everything real is https.
    let protocols = if std::env::var_os("CRC_SELFTEST").is_some() {
        "=https,file"
    } else {
        "=https"
    };
    let output = std::process::Command::new("/usr/bin/curl")
        .args(["--silent", "--show-error", "--fail", "--location"])
        .args(["--max-time", "30", "--max-filesize", &max.to_string()])
        .args(["--proto", protocols, "--proto-redir", "=https"])
        .arg("--user-agent")
        .arg(format!("crc/{}", env!("CARGO_PKG_VERSION")))
        .arg(url)
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        if error.contains("error: 404") {
            return Err("the registry has no published release yet".into());
        }
        return Err(error.trim().trim_start_matches("curl: ").to_owned());
    }
    Ok(output.stdout)
}

/// The signed list of extensions. Blocking; call it off the main thread.
pub fn fetch_index() -> Result<Vec<Entry>, String> {
    if !super::verify::has_registry_key() && std::env::var_os("CRC_SELFTEST").is_none() {
        return Err("this build of crc has no registry key yet".into());
    }
    let base = base();
    let index = fetch(&format!("{base}index.json"), 4 << 20)?;
    let signature = fetch(&format!("{base}index.json.sig"), 4 << 10)?;
    if !super::verify::registry_signed(&index, &signature) {
        return Err("the registry's signature does not verify; nothing was trusted".into());
    }
    parse_index(&index)
}

pub fn parse_index(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "the registry is not UTF-8")?;
    let value = crate::json::parse(text).map_err(|e| format!("the registry: {e}"))?;
    if value.get("api").and_then(Value::as_u64) != Some(manifest::API) {
        return Err("the registry is for another version of crc".into());
    }
    let mut entries = Vec::new();
    for item in value
        .get("extensions")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        // One bad entry is skipped, not the whole registry.
        let Ok(manifest) = manifest::parse(item) else {
            continue;
        };
        let text = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        let wasm = text("wasm");
        let sha256 = text("sha256");
        if wasm.contains('/') || !wasm.ends_with(".wasm") || sha256.len() != 64 {
            continue;
        }
        entries.push(Entry {
            manifest_json: crate::json::pretty(item),
            wasm,
            sha256,
            size: item.get("size").and_then(Value::as_u64).unwrap_or(0),
            readme: text("readme"),
            manifest,
        });
    }
    Ok(entries)
}

/// Downloads an entry's module and checks it against the signed index.
/// Blocking.
pub fn download(entry: &Entry) -> Result<Package, String> {
    let wasm = fetch(&format!("{}{}", base(), entry.wasm), 16 << 20)?;
    let hash = super::verify::sha256(&wasm)
        .map(|h| super::verify::hex(&h))
        .unwrap_or_default();
    if hash != entry.sha256 {
        return Err(format!(
            "{} does not match the registry; it was not installed",
            entry.wasm
        ));
    }
    let package = Package {
        manifest_json: entry.manifest_json.clone(),
        manifest: entry.manifest.clone(),
        wasm,
        readme: entry.readme.clone(),
        signed: true,
        source: "registry".into(),
    };
    package.check()?;
    Ok(package)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_with(sha: &str) -> String {
        let manifest = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/extensions/sort-lines/manifest.json"
        ))
        .unwrap();
        let entry = manifest.trim_end().trim_end_matches('}').to_owned()
            + &format!(
                ",\"wasm\":\"crc.sort-lines-0.1.0.wasm\",\"sha256\":\"{sha}\",\"size\":1,\"readme\":\"# Sort\"}}"
            );
        format!("{{\"api\":1,\"extensions\":[{entry},{{\"id\":\"bad\"}}]}}")
    }

    #[test]
    fn a_signed_index_verifies_parses_and_a_changed_one_does_not() {
        let dir = std::env::temp_dir().join(format!("crc-registry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let Some(public) = crate::ext::verify::tests::test_key(&dir) else {
            eprintln!("no openssl; skipping");
            return;
        };
        let index = index_with(&"a".repeat(64));
        let signature = crate::ext::verify::tests::sign(&dir, index.as_bytes());
        assert!(crate::ext::verify::verify_with(
            &public,
            index.as_bytes(),
            &signature
        ));
        let entries = parse_index(index.as_bytes()).unwrap();
        assert_eq!(entries.len(), 1, "the bad entry is skipped, the rest kept");
        assert_eq!(entries[0].manifest.name, "Sort Lines");
        assert_eq!(entries[0].readme, "# Sort");
        let tampered = index.replace("selection.read", "document.read");
        assert!(!crate::ext::verify::verify_with(
            &public,
            tampered.as_bytes(),
            &signature
        ));
        assert!(parse_index(b"{\"api\":2,\"extensions\":[]}").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
