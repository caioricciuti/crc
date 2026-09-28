//! Installed extensions, one folder each under
//! `~/Library/Application Support/crc/extensions/<id>/`: the manifest, the
//! module, the README and `origin.json` (signed or not, from where). Never
//! inside a project: cloning a repository is not a way to install code.

use std::path::{Path, PathBuf};

use super::manifest::{self, Manifest};
use crate::json::{Value, object, string};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub manifest: Manifest,
    pub dir: PathBuf,
    /// Came from the signed registry.
    pub signed: bool,
    pub enabled: bool,
    pub readme: String,
}

impl Installed {
    pub fn wasm(&self) -> PathBuf {
        self.dir.join(&self.manifest.entry)
    }
}

/// The folder every extension lives under.
pub fn root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/crc/extensions"))
}

/// Everything installed, by name. Folders that do not read back (a
/// half-written install, a hand edit) are skipped.
pub fn list() -> Vec<Installed> {
    root().map(|r| list_in(&r)).unwrap_or_default()
}

pub fn list_in(root: &Path) -> Vec<Installed> {
    recover(root);
    let mut out: Vec<Installed> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| read(&e.path()))
        .collect();
    out.sort_by(|a, b| {
        a.manifest
            .name
            .to_lowercase()
            .cmp(&b.manifest.name.to_lowercase())
    });
    out
}

/// Puts back what an install cut short left behind. An install writes
/// `.staging-<id>-<pid>`, moves the installed folder aside to
/// `.old-<id>-<pid>`, then renames the staging folder into place; a crash
/// between the two renames leaves the extension only in `.old`, and it is
/// moved back. Anything else left over is removed. Folders of a crc that is
/// still running are its own business and left alone.
fn recover(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let (old, rest) = if let Some(rest) = name.strip_prefix(".old-") {
            (true, rest)
        } else if let Some(rest) = name.strip_prefix(".staging-") {
            (false, rest)
        } else {
            continue;
        };
        let Some((id, pid)) = rest.rsplit_once('-') else {
            continue;
        };
        let Ok(pid) = pid.parse::<u64>() else {
            continue;
        };
        if crate::platform::process_alive(pid) {
            continue;
        }
        let target = root.join(id);
        if old && !target.exists() {
            let _ = std::fs::rename(entry.path(), &target);
        } else {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn read(dir: &Path) -> Option<Installed> {
    let text = std::fs::read_to_string(dir.join("manifest.json")).ok()?;
    let manifest = manifest::parse(&crate::json::parse(&text).ok()?).ok()?;
    // The folder must be the id it claims.
    if dir.file_name()?.to_str()? != manifest.id || !dir.join(&manifest.entry).is_file() {
        return None;
    }
    let origin = std::fs::read_to_string(dir.join("origin.json"))
        .ok()
        .and_then(|t| crate::json::parse(&t).ok());
    Some(Installed {
        signed: origin
            .as_ref()
            .and_then(|o| o.get("signed"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        enabled: !dir.join("disabled").exists(),
        readme: read_readme(&dir.join("README.md")),
        dir: dir.to_path_buf(),
        manifest,
    })
}

/// What an install is made of, checked, before anything is written.
pub struct Package {
    pub manifest_json: String,
    pub manifest: Manifest,
    pub wasm: Vec<u8>,
    pub readme: String,
    pub signed: bool,
    pub source: String,
}

/// The largest module an extension may bring.
const MAX_MODULE_BYTES: usize = 16 << 20;
/// A README past this is not shown.
const MAX_README_BYTES: usize = 1 << 20;

/// A README to show, or nothing when it is missing or over the limit: the
/// size is checked before anything is read.
fn read_readme(path: &Path) -> String {
    if std::fs::metadata(path).map_or(true, |m| m.len() > MAX_README_BYTES as u64) {
        return String::new();
    }
    std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

impl Package {
    /// A package from a folder holding manifest.json, README.md and the
    /// module. Unsigned.
    pub fn from_folder(folder: &Path) -> Result<Package, String> {
        let manifest_json = std::fs::read_to_string(folder.join("manifest.json"))
            .map_err(|_| "the folder has no manifest.json".to_owned())?;
        let value =
            crate::json::parse(&manifest_json).map_err(|e| format!("manifest.json: {e}"))?;
        let manifest = manifest::parse(&value)?;
        // The size first: this runs just after the folder is picked, and a
        // multi-gigabyte `entry` was read whole before being refused.
        let module = folder.join(&manifest.entry);
        let size = std::fs::metadata(&module)
            .map_err(|_| format!("the folder has no {}", manifest.entry))?
            .len();
        if size > MAX_MODULE_BYTES as u64 {
            return Err("the module is over 16 MB".into());
        }
        let wasm =
            std::fs::read(&module).map_err(|_| format!("the folder has no {}", manifest.entry))?;
        let readme = read_readme(&folder.join("README.md"));
        let package = Package {
            manifest_json,
            manifest,
            wasm,
            readme,
            signed: false,
            source: folder.display().to_string(),
        };
        package.check()?;
        Ok(package)
    }

    /// The module parses and matches the manifest.
    pub fn check(&self) -> Result<(), String> {
        if self.wasm.len() > MAX_MODULE_BYTES {
            return Err("the module is over 16 MB".into());
        }
        let module = super::wasm::Module::parse(&self.wasm)
            .map_err(|t| format!("{} is not a module crc can run: {t}", self.manifest.entry))?;
        manifest::check_module(&self.manifest, &module)
    }
}

/// Installs `package`, replacing an installed version. Written to a
/// staging folder and renamed into place, so a failure leaves the old one.
pub fn install(package: &Package) -> Result<Installed, String> {
    install_in(&root().ok_or("no home folder")?, package)
}

pub fn install_in(root: &Path, package: &Package) -> Result<Installed, String> {
    let fail = |e: std::io::Error| format!("could not install: {e}");
    std::fs::create_dir_all(root).map_err(fail)?;
    let id = &package.manifest.id;
    let staging = root.join(format!(".staging-{id}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(fail)?;
    let write = || -> std::io::Result<()> {
        std::fs::write(staging.join("manifest.json"), &package.manifest_json)?;
        std::fs::write(staging.join(&package.manifest.entry), &package.wasm)?;
        std::fs::write(staging.join("README.md"), &package.readme)?;
        let origin = object([
            ("signed", Value::Bool(package.signed)),
            ("source", string(&package.source)),
            (
                "sha256",
                string(
                    &super::verify::sha256(&package.wasm)
                        .map(|h| super::verify::hex(&h))
                        .unwrap_or_default(),
                ),
            ),
        ]);
        std::fs::write(staging.join("origin.json"), crate::json::pretty(&origin))
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(fail(e));
    }
    let target = root.join(id);
    let old = root.join(format!(".old-{id}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&old);
    // Keep the enabled state across an update.
    let disabled = target.join("disabled").exists();
    if target.exists() {
        std::fs::rename(&target, &old).map_err(fail)?;
    }
    if let Err(e) = std::fs::rename(&staging, &target) {
        let _ = std::fs::rename(&old, &target);
        let _ = std::fs::remove_dir_all(&staging);
        return Err(fail(e));
    }
    let _ = std::fs::remove_dir_all(&old);
    if disabled {
        let _ = std::fs::write(target.join("disabled"), "");
    }
    read(&target).ok_or_else(|| "installed, but it does not read back".to_owned())
}

/// Removes an installed extension. Only ever a folder under the root whose
/// name is the manifest's id.
pub fn uninstall(installed: &Installed) -> Result<(), String> {
    uninstall_in(&root().ok_or("no home folder")?, installed)
}

pub fn uninstall_in(root: &Path, installed: &Installed) -> Result<(), String> {
    if installed.dir.parent() != Some(root)
        || installed.dir.file_name().and_then(|n| n.to_str())
            != Some(installed.manifest.id.as_str())
    {
        return Err("not an installed extension".into());
    }
    std::fs::remove_dir_all(&installed.dir).map_err(|e| format!("could not remove it: {e}"))
}

pub fn set_enabled(installed: &Installed, enabled: bool) -> Result<(), String> {
    let marker = installed.dir.join("disabled");
    let result = if enabled {
        match std::fs::remove_file(&marker) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    } else {
        std::fs::write(&marker, "")
    };
    result.map_err(|e| format!("could not change it: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extensions/sort-lines")
    }

    #[test]
    fn an_install_cut_short_is_put_back_or_cleaned_up() {
        let root = std::env::temp_dir().join(format!("crc-ext-recover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let package = Package::from_folder(&fixture()).unwrap();
        install_in(&root, &package).unwrap();
        // A pid no process has: the install that left these is gone.
        let dead = u32::MAX as u64 - 7;
        assert!(!crate::platform::process_alive(dead));
        // The crash came between the renames: only the old folder is left.
        std::fs::rename(
            root.join("crc.sort-lines"),
            root.join(format!(".old-crc.sort-lines-{dead}")),
        )
        .unwrap();
        std::fs::create_dir_all(root.join(format!(".staging-crc.sort-lines-{dead}"))).unwrap();
        // One of this process's, which is alive: left alone.
        let ours = root.join(format!(".staging-crc.other-{}", std::process::id()));
        std::fs::create_dir_all(&ours).unwrap();
        let listed = list_in(&root);
        assert_eq!(listed.len(), 1, "the extension is back");
        assert_eq!(listed[0].manifest.id, "crc.sort-lines");
        let mut names: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                ours.file_name().unwrap().to_string_lossy().into_owned(),
                "crc.sort-lines".into()
            ]
        );
        // Both the old and the new present: the old one goes.
        std::fs::create_dir_all(root.join(format!(".old-crc.sort-lines-{dead}"))).unwrap();
        list_in(&root);
        assert!(!root.join(format!(".old-crc.sort-lines-{dead}")).exists());
        assert!(root.join("crc.sort-lines").is_dir());
        // A README over the limit is not shown.
        std::fs::write(
            root.join("crc.sort-lines/README.md"),
            "x".repeat(MAX_README_BYTES + 1),
        )
        .unwrap();
        assert_eq!(list_in(&root)[0].readme, "");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn installs_lists_disables_updates_and_uninstalls() {
        let root = std::env::temp_dir().join(format!("crc-ext-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let package = Package::from_folder(&fixture()).unwrap();
        assert!(!package.signed, "a folder install is unsigned");

        let installed = install_in(&root, &package).unwrap();
        assert_eq!(installed.manifest.id, "crc.sort-lines");
        assert!(installed.enabled && !installed.signed);
        assert!(installed.wasm().is_file());
        assert_eq!(list_in(&root).len(), 1);

        set_enabled(&installed, false).unwrap();
        assert!(!list_in(&root)[0].enabled);
        // An update keeps it disabled, and leaves no staging folders.
        install_in(&root, &package).unwrap();
        let listed = list_in(&root);
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].enabled);
        let names: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["crc.sort-lines"]);

        // A folder whose name is not its id does not read back.
        std::fs::rename(root.join("crc.sort-lines"), root.join("other")).unwrap();
        assert!(list_in(&root).is_empty());
        std::fs::rename(root.join("other"), root.join("crc.sort-lines")).unwrap();

        let installed = list_in(&root).remove(0);
        uninstall_in(&root, &installed).unwrap();
        assert!(list_in(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_package_whose_module_lies_is_refused() {
        let dir = std::env::temp_dir().join(format!("crc-ext-lie-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = std::fs::read_to_string(fixture().join("manifest.json")).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            manifest.replace("\"unique\"", "\"shout\""),
        )
        .unwrap();
        std::fs::copy(
            fixture().join("sort_lines.wasm"),
            dir.join("sort_lines.wasm"),
        )
        .unwrap();
        let error = Package::from_folder(&dir).err().unwrap();
        assert!(error.contains("shout"), "{error}");
        std::fs::write(dir.join("sort_lines.wasm"), b"not wasm").unwrap();
        assert!(Package::from_folder(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
