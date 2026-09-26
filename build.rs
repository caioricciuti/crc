//! Compiles the native caret/grapheme bridges and vendored tree-sitter grammars.
//!
//! This is the only build script in the project that is ours, and one of only
//! two in the whole dependency tree. It exists so that syntax highlighting
//! does not cost the `tree-sitter` crate, which drags in `serde`,
//! `serde_derive`, `proc-macro2`, `quote` and `syn` as *build* dependencies:
//! nineteen extra crates, nine extra build scripts and a proc macro, all
//! executing during every build. See docs/dependency-review.md.
//!
//! What it does: hands `cc` our synchronous CoreText/NSString bridges and the C under
//! third_party/ and emits static
//! libraries, one for the runtime and one per grammar. No
//! network, no code generation, no filesystem writes outside OUT_DIR.
//!
//! The third-party C is checked into third_party/ and pinned by content
//! rather than by a version range, so a rebuild cannot pick up different
//! source than the one that was reviewed.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rustc-link-lib=framework=QuickLookUI");
    // FSEvents, for the project watcher.
    println!("cargo:rustc-link-lib=framework=CoreServices");
    let revision = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let dirty_diff = Command::new("git")
        .args(["diff", "--binary", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout)
        .unwrap_or_default();
    let dirty_suffix = if dirty_diff.is_empty() {
        String::new()
    } else {
        // A stable content fingerprint distinguishes two local builds from
        // the same commit without making clean builds non-reproducible.
        let hash = dirty_diff
            .iter()
            .fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
            });
        format!("+dirty.{hash:08x}", hash = hash as u32)
    };
    println!("cargo:rustc-env=CRC_BUILD_REVISION={revision}{dirty_suffix}");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    // A commit on a branch moves the branch's ref, not HEAD, and leaves the
    // index alone when everything was staged, so the revision went stale.
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    // Only paths that exist: Cargo reruns every build for a missing one.
    let refs = git(&["symbolic-ref", "-q", "HEAD"])
        .and_then(|branch| git(&["rev-parse", "--git-path", &branch]))
        .into_iter()
        .chain(git(&["rev-parse", "--git-path", "packed-refs"]));
    for path in refs.filter(|p| std::path::Path::new(p).exists()) {
        println!("cargo:rerun-if-changed={path}");
    }
    cc::Build::new()
        .file("src/render/caret.c")
        .flag("-fblocks")
        .compile("crc-caret");
    cc::Build::new()
        .file("src/text/grapheme.m")
        .compile("crc-grapheme");
    let root = Path::new("third_party");
    let core = root.join("tree-sitter");

    // `lib.c` #includes every other core .c file, so the runtime is a single
    // translation unit. `wasm_store.c` is inside an #ifdef we never define,
    // which is what keeps wasmtime out of this.
    cc::Build::new()
        .include(core.join("src"))
        .include(core.join("include"))
        .file(core.join("src/lib.c"))
        .flag_if_supported("-std=c11")
        // Vendored third-party C, not ours to clean up. Warnings here would
        // only train us to ignore warnings.
        .warnings(false)
        .compile("tree-sitter");

    // Each grammar is a generated parse table plus, for most, a small
    // hand-written external scanner. One static library apiece: the parsers
    // all define the same internal symbol names as `static`, so they must
    // not share a translation unit, and a library per grammar also keeps a
    // rebuild of one from recompiling eight megabytes of another.
    //
    // (library name, source directory, has a scanner)
    let grammars = [
        ("tree-sitter-rust", root.join("tree-sitter-rust/src"), true),
        ("tree-sitter-html", root.join("tree-sitter-html/src"), true),
        (
            "tree-sitter-javascript",
            root.join("tree-sitter-javascript/src"),
            true,
        ),
        ("tree-sitter-css", root.join("tree-sitter-css/src"), true),
        ("tree-sitter-json", root.join("tree-sitter-json/src"), false),
        (
            "tree-sitter-python",
            root.join("tree-sitter-python/src"),
            true,
        ),
        ("tree-sitter-c", root.join("tree-sitter-c/src"), false),
        ("tree-sitter-cpp", root.join("tree-sitter-cpp/src"), true),
        ("tree-sitter-go", root.join("tree-sitter-go/src"), false),
        // Two parsers from one repository. Their scanners both include
        // ../../common/scanner.h, which is why the layout upstream uses is
        // kept as it is.
        (
            "tree-sitter-typescript",
            root.join("tree-sitter-typescript/typescript/src"),
            true,
        ),
        (
            "tree-sitter-tsx",
            root.join("tree-sitter-typescript/tsx/src"),
            true,
        ),
        ("tree-sitter-toml", root.join("tree-sitter-toml/src"), true),
        // Its scanner #includes the schema tables beside it, so they are
        // vendored but not listed here.
        ("tree-sitter-yaml", root.join("tree-sitter-yaml/src"), true),
        ("tree-sitter-bash", root.join("tree-sitter-bash/src"), true),
    ];
    for (name, src, scanner) in &grammars {
        let mut build = cc::Build::new();
        build.include(src).file(src.join("parser.c"));
        if *scanner {
            build.file(src.join("scanner.c"));
        }
        build
            .flag_if_supported("-std=c11")
            .warnings(false)
            .compile(name);
    }

    // Rebuild when any of it changes. The whole directory rather than
    // individual files, so adding a grammar or editing a query (queries are
    // `include_str!`ed) does not silently skip a rebuild.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", root.display());
}
