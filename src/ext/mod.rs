//! Extensions: WebAssembly modules with a manifest that says what they may
//! touch, run by crc's own interpreter off the main thread. The design, and
//! why an interpreter of our own, is in `docs/extensions.md`.
//!
//! - [`wasm`]: the interpreter.
//! - [`manifest`]: what an extension declares, and the rules for it.
//! - [`run`]: loading one against its manifest and running commands, on
//!   the extension thread.
//! - [`store`]: installed extensions on disk.
//! - [`registry`]: the signed list of official extensions.
//! - [`verify`]: SHA-256 and signatures, through macOS.

pub mod manifest;
pub mod registry;
pub mod run;
pub mod store;
pub mod verify;
pub mod wasm;

#[cfg(test)]
mod tests {
    use super::run::{Request, load};
    use super::wasm::{Instance, Module, Trap};
    use std::path::PathBuf;

    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extensions/sort-lines")
    }

    fn sort_lines() -> super::run::Loaded {
        let package = super::store::Package::from_folder(&fixture()).unwrap();
        load(package.manifest, &package.wasm).unwrap()
    }

    fn request(command: &str, text: &str, selection: bool) -> Request {
        Request {
            command: command.into(),
            text: text.into(),
            selection,
            language: "text".into(),
        }
    }

    #[test]
    fn runs_sort_lines_end_to_end() {
        let mut ext = sort_lines();
        let out = ext
            .run(&request("sort", "pear\napple\nfig é\n", true))
            .unwrap();
        assert_eq!(out.replace.as_deref(), Some("apple\nfig é\npear\n"));
        let out = ext.run(&request("unique", "a\nb\na\n", true)).unwrap();
        assert_eq!(out.replace.as_deref(), Some("a\nb\n"));
        assert_eq!(out.message.as_deref(), Some("removed 1 duplicate line"));
        // The same instance again and again: buffers are freed, memory
        // does not creep.
        let text = "zeta\nalpha\n".repeat(2000);
        ext.run(&request("sort", &text, true)).unwrap();
        let after_one = ext_memory(&mut ext);
        for _ in 0..20 {
            ext.run(&request("sort", &text, true)).unwrap();
        }
        assert_eq!(ext_memory(&mut ext), after_one, "memory grew across calls");
    }

    fn ext_memory(ext: &mut super::run::Loaded) -> usize {
        // Through a harmless call, so the value is read after the last run.
        ext.run(&request("sort", "", true)).unwrap();
        ext.memory_len()
    }

    #[test]
    fn capabilities_decide_what_goes_in_and_comes_out() {
        // Sort Lines with only the selection capabilities: no whole
        // document goes in.
        let package = super::store::Package::from_folder(&fixture()).unwrap();
        let mut manifest = package.manifest.clone();
        manifest.capabilities.retain(|c| {
            matches!(
                c,
                super::manifest::Capability::SelectionRead
                    | super::manifest::Capability::SelectionReplace
            )
        });
        let mut ext = load(manifest, &package.wasm).unwrap();
        let error = ext.run(&request("sort", "b\na\n", false)).unwrap_err();
        assert_eq!(error, "Sort Lines may not read the whole document");
        // Read but not replace: the answer's text is dropped.
        let mut manifest = package.manifest.clone();
        manifest
            .capabilities
            .retain(|c| *c != super::manifest::Capability::DocumentEdit);
        let mut ext = load(manifest, &package.wasm).unwrap();
        let out = ext.run(&request("sort", "b\na\n", false)).unwrap();
        assert_eq!(out.replace, None);
        assert_eq!(
            out.message.as_deref(),
            Some("Sort Lines may not change the text")
        );
        let mut ext = sort_lines();
        let error = ext.run(&request("nope", "x", true)).unwrap_err();
        assert!(error.contains("no command nope"), "{error}");
    }

    /// Markdown Preview, built from crc-extensions, through the interpreter:
    /// a page comes back only with `preview.show`, and a long document
    /// renders inside half the deadline.
    #[test]
    fn a_preview_answers_with_a_page() {
        let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/extensions/markdown-preview");
        let package = super::store::Package::from_folder(&folder).unwrap();
        let mut ext = load(package.manifest.clone(), &package.wasm).unwrap();
        let markdown = |text: &str| Request {
            language: "markdown".into(),
            ..request("preview", text, false)
        };
        let out = ext
            .run(&markdown(
                "# Notes\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n<script>x</script>\n",
            ))
            .unwrap();
        let html = out.html.unwrap();
        assert!(
            html.contains("<h1>Notes</h1>") && html.contains("<td>1</td>"),
            "{html}"
        );
        assert!(!html.contains("<script>"), "raw HTML stays text: {html}");
        assert_eq!(out.replace, None);
        let out = ext
            .run(&Request {
                language: "rust".into(),
                ..markdown("fn main() {}")
            })
            .unwrap();
        assert_eq!(out.html, None);
        assert!(out.message.unwrap().contains("Markdown"));

        let section = "## Section\n\nSome *text* with `code`, a [link](x.md) and a list:\n\n\
                       - one\n- two\n  - nested\n\n```rust\nfn main() {}\n```\n\n";
        let long = section.repeat(100_000 / section.len());
        let started = std::time::Instant::now();
        let out = ext.run(&markdown(&long)).unwrap();
        let took = started.elapsed();
        assert!(out.html.is_some(), "{:?}", out.message);
        // About 0.4 s here in release; half the deadline leaves room for a
        // slow CI machine.
        assert!(
            took < std::time::Duration::from_millis(1000),
            "100 KB took {took:?}"
        );

        let mut manifest = package.manifest.clone();
        manifest
            .capabilities
            .retain(|c| *c != super::manifest::Capability::PreviewShow);
        let mut ext = load(manifest, &package.wasm).unwrap();
        let out = ext.run(&markdown("# x")).unwrap();
        assert_eq!(out.html, None);
        assert_eq!(
            out.message.as_deref(),
            Some("Markdown Preview may not show a preview")
        );
    }

    #[test]
    fn a_module_must_match_its_manifest() {
        let package = super::store::Package::from_folder(&fixture()).unwrap();
        let mut manifest = package.manifest.clone();
        manifest.commands[0].id = "missing".into();
        let error = match load(manifest, &package.wasm) {
            Err(e) => e,
            Ok(_) => panic!("loaded against the wrong manifest"),
        };
        assert!(error.contains("missing"), "{error}");
    }

    /// Corrupted copies of a real extension: none may panic, whatever they
    /// do inside their sandbox.
    #[test]
    fn corrupted_modules_never_panic() {
        let base = std::fs::read(fixture().join("sort_lines.wasm")).unwrap();
        let real_manifest = super::store::Package::from_folder(&fixture())
            .unwrap()
            .manifest;
        let mut seed: u64 = 0x9E3779B97F4A7C15;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..3000 {
            let mut bytes = base.clone();
            for _ in 0..1 + rnd() % 8 {
                let at = 8 + (rnd() as usize) % (bytes.len() - 8);
                bytes[at] = rnd() as u8;
            }
            let outcome = std::panic::catch_unwind(|| {
                let module = Module::parse(&bytes).ok()?;
                let mut instance = Instance::new(module, 64, |_, _| {
                    Some(Box::new(|_: &mut [u8], _: &[u64]| Ok(None)))
                })
                .ok()?;
                let alloc = instance.func("crc_alloc")?;
                let sort = instance.func("sort")?;
                instance.fuel = 2_000_000;
                let ptr = *instance.call(alloc, &[16]).ok()?.first()?;
                instance.fuel = 2_000_000;
                instance.call(sort, &[ptr, 16]).ok()
            });
            assert!(outcome.is_ok(), "round {round} panicked");
            // And through the way crc really runs one: load against its
            // manifest, marshal a request in, read the answer back out.
            let manifest = real_manifest.clone();
            let command = manifest.commands[0].id.clone();
            let outcome = std::panic::catch_unwind(|| {
                let mut loaded = load(manifest, &bytes).ok()?;
                loaded.run(&request(&command, "b\na\n", true)).ok()
            });
            assert!(outcome.is_ok(), "round {round} panicked through run::load");
        }
    }

    // ---- hand-assembled modules for the edge cases -------------------------

    fn leb(mut n: u32, out: &mut Vec<u8>) {
        loop {
            let byte = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn section(id: u8, body: &[u8], out: &mut Vec<u8>) {
        out.push(id);
        leb(body.len() as u32, out);
        out.extend_from_slice(body);
    }

    /// A module of `(params, results)` types, imports (type index, as
    /// crc.<name>), and functions (type index, locals, code without the
    /// final end), each exported as f<n> over the whole index space.
    fn module(types: &[(u8, u8)], imports: &[(u32, &str)], funcs: &[(u32, u8, &[u8])]) -> Vec<u8> {
        let names: Vec<String> = (0..funcs.len())
            .map(|i| format!("f{}", i + imports.len()))
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        named_module(types, imports, funcs, &names, false)
    }

    /// [`module`] with the functions exported as `names`, and the memory as
    /// `memory` when asked.
    fn named_module(
        types: &[(u8, u8)],
        imports: &[(u32, &str)],
        funcs: &[(u32, u8, &[u8])],
        names: &[&str],
        export_memory: bool,
    ) -> Vec<u8> {
        let mut out = b"\0asm\x01\0\0\0".to_vec();
        let mut body = Vec::new();
        leb(types.len() as u32, &mut body);
        for &(p, r) in types {
            body.push(0x60);
            body.push(p);
            body.extend(std::iter::repeat_n(0x7f, p as usize));
            body.push(r);
            body.extend(std::iter::repeat_n(0x7f, r as usize));
        }
        section(1, &body, &mut out);
        if !imports.is_empty() {
            let mut body = Vec::new();
            leb(imports.len() as u32, &mut body);
            for &(ty, name) in imports {
                body.extend_from_slice(&[3, b'c', b'r', b'c']);
                leb(name.len() as u32, &mut body);
                body.extend_from_slice(name.as_bytes());
                body.push(0);
                leb(ty, &mut body);
            }
            section(2, &body, &mut out);
        }
        let mut body = Vec::new();
        leb(funcs.len() as u32, &mut body);
        for &(ty, _, _) in funcs {
            leb(ty, &mut body);
        }
        section(3, &body, &mut out);
        // A table of every defined function, for call_indirect.
        let n = funcs.len() as u32;
        let mut body = vec![1, 0x70, 0];
        leb(n, &mut body);
        section(4, &body, &mut out);
        section(5, &[1, 1, 1, 2], &mut out);
        let mut body = Vec::new();
        leb(funcs.len() as u32 + u32::from(export_memory), &mut body);
        for (i, name) in names.iter().enumerate() {
            leb(name.len() as u32, &mut body);
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            leb(i as u32 + imports.len() as u32, &mut body);
        }
        if export_memory {
            body.extend_from_slice(&[6, b'm', b'e', b'm', b'o', b'r', b'y', 2, 0]);
        }
        section(7, &body, &mut out);
        let mut body = vec![1, 0, 0x41, 0, 0x0b];
        leb(n, &mut body);
        for i in 0..n {
            leb(i + imports.len() as u32, &mut body);
        }
        section(9, &body, &mut out);
        let mut body = Vec::new();
        leb(funcs.len() as u32, &mut body);
        for &(_, locals, code) in funcs {
            let mut f = Vec::new();
            if locals > 0 {
                f.extend_from_slice(&[1, locals, 0x7f]);
            } else {
                f.push(0);
            }
            f.extend_from_slice(code);
            f.push(0x0b);
            leb(f.len() as u32, &mut body);
            body.extend_from_slice(&f);
        }
        section(10, &body, &mut out);
        out
    }

    fn instance(bytes: &[u8]) -> Instance {
        Instance::new(Module::parse(bytes).unwrap(), 4, |_, name| match name {
            "double" => Some(Box::new(|_: &mut [u8], args: &[u64]| {
                Ok(Some((args[0] as u32).wrapping_mul(2) as u64))
            })),
            _ => None,
        })
        .unwrap()
    }

    fn call(bytes: &[u8], f: &str, args: &[u64]) -> Result<Vec<u64>, Trap> {
        let mut i = instance(bytes);
        let func = i.func(f).unwrap();
        i.call(func, args)
    }

    fn manifest_for(commands: &str) -> crate::ext::manifest::Manifest {
        crate::ext::manifest::parse(
            &crate::json::parse(&format!(
                r#"{{"id": "t.shapes", "name": "Shapes", "version": "0.1.0",
            "description": "x", "authors": ["t"], "license": "MIT",
            "api": 1, "entry": "t.wasm", "capabilities": ["selection.read"],
            "commands": [{commands}]}}"#
            ))
            .unwrap(),
        )
        .unwrap()
    }

    fn run_once(bytes: &[u8]) -> Result<crate::ext::run::Response, String> {
        let manifest = manifest_for(r#"{"id": "go", "title": "Go"}"#);
        let mut loaded = crate::ext::run::load(manifest, bytes)?;
        loaded.run(&crate::ext::run::Request {
            command: "go".into(),
            text: "x".into(),
            selection: true,
            language: String::new(),
        })
    }

    #[test]
    fn wrong_import_and_export_shapes_are_refused_not_run() {
        // The host indexed arguments and results by the shape
        // crc expects, and the module declared another.
        let zero: &[u8] = &[0x41, 0x00];
        let nothing: &[u8] = &[];
        let log_bare: &[u8] = &[0x10, 0x00, 0x41, 0x00];
        // Types: 0 () -> (), 1 (i32) -> i32, 2 (i32 i32) -> (),
        // 3 (i32 i32) -> i32, 4 (i32) -> ().
        let types = &[(0, 0), (1, 1), (2, 0), (2, 1), (1, 0)];
        let names = &["crc_alloc", "crc_free", "go"];
        let good = named_module(
            types,
            &[],
            &[(1, 0, zero), (2, 0, nothing), (3, 0, zero)],
            names,
            true,
        );
        assert!(run_once(&good).is_err_and(|e| e.contains("answered")));
        let bad_alloc = named_module(
            types,
            &[],
            &[(4, 0, nothing), (2, 0, nothing), (3, 0, zero)],
            names,
            true,
        );
        assert!(run_once(&bad_alloc).is_err_and(|e| e.contains("crc_alloc")));
        let bad_command = named_module(
            types,
            &[],
            &[(1, 0, zero), (2, 0, nothing), (2, 0, nothing)],
            names,
            true,
        );
        assert!(run_once(&bad_command).is_err_and(|e| e.contains("go")));
        let bad_log = named_module(
            types,
            &[(0, "log")],
            &[(1, 0, zero), (2, 0, nothing), (3, 0, log_bare)],
            names,
            true,
        );
        assert!(run_once(&bad_log).is_err_and(|e| e.contains("crc.log")));
    }

    #[test]
    fn control_flow() {
        // (i32) -> i32: sum 1..=n in a loop, leaving through br_if.
        let sum: &[u8] = &[
            0x03, 0x40, // loop
            0x20, 0x00, 0x45, 0x0d, 0x01, // n == 0 -> br 1 (the function)
            0x20, 0x01, 0x20, 0x00, 0x6a, 0x21, 0x01, // acc += n
            0x20, 0x00, 0x41, 0x01, 0x6b, 0x21, 0x00, // n -= 1
            0x0c, 0x00, // br 0 (again)
            0x0b, 0x20, 0x01,
        ];
        // A branch to the function's own block is a return; here with a
        // value on the stack, as rustc emits it.
        let early: &[u8] = &[0x41, 0x07, 0x0c, 0x00, 0x41, 0x09];
        // br_table: 0 -> 10, 1 -> 20, anything else -> 30.
        let table: &[u8] = &[
            0x02, 0x40, 0x02, 0x40, 0x02, 0x40, 0x20, 0x00, 0x0e, 0x02, 0x00, 0x01, 0x02, 0x0b,
            0x41, 0x0a, 0x0f, 0x0b, 0x41, 0x14, 0x0f, 0x0b, 0x41, 0x1e,
        ];
        // if/else with a result.
        let pick: &[u8] = &[0x20, 0x00, 0x04, 0x7f, 0x41, 0x01, 0x05, 0x41, 0x02, 0x0b];
        let m = module(
            &[(1, 1), (0, 1)],
            &[],
            &[(0, 1, sum), (1, 0, early), (0, 0, table), (0, 0, pick)],
        );
        assert_eq!(call(&m, "f0", &[10]).unwrap(), [55]);
        assert_eq!(call(&m, "f1", &[]).unwrap(), [7]);
        assert_eq!(call(&m, "f2", &[0]).unwrap(), [10]);
        assert_eq!(call(&m, "f2", &[1]).unwrap(), [20]);
        assert_eq!(call(&m, "f2", &[99]).unwrap(), [30]);
        assert_eq!(call(&m, "f3", &[1]).unwrap(), [1]);
        assert_eq!(call(&m, "f3", &[0]).unwrap(), [2]);
    }

    #[test]
    fn calls_imports_and_indirect_calls() {
        // f1: double(x) + 1 through the import; f2: call_indirect table[0].
        let via_import: &[u8] = &[0x20, 0x00, 0x10, 0x00, 0x41, 0x01, 0x6a];
        let indirect: &[u8] = &[0x20, 0x00, 0x41, 0x00, 0x11, 0x00, 0x00];
        let m = module(
            &[(1, 1)],
            &[(0, "double")],
            &[(0, 0, via_import), (0, 0, indirect)],
        );
        assert_eq!(call(&m, "f1", &[20]).unwrap(), [41]);
        assert_eq!(call(&m, "f2", &[20]).unwrap(), [41]);
        // An import the host does not give fails the whole instance.
        let unknown = module(&[(1, 1)], &[(0, "network")], &[(0, 0, via_import)]);
        let refused = Instance::new(Module::parse(&unknown).unwrap(), 4, |_, _| None);
        assert!(matches!(refused, Err(t) if t.0.contains("crc.network")));
    }

    #[test]
    fn traps_instead_of_crashing() {
        let forever: &[u8] = &[0x03, 0x40, 0x0c, 0x00, 0x0b];
        let recurse: &[u8] = &[0x10, 0x01];
        let divide: &[u8] = &[0x41, 0x01, 0x41, 0x00, 0x6d, 0x1a];
        let wild: &[u8] = &[0x41, 0x7f, 0x28, 0x02, 0xff, 0xff, 0x03, 0x1a];
        let grow: &[u8] = &[0x41, 0xe4, 0x00, 0x40, 0x00, 0x1a];
        let m = module(
            &[(0, 0)],
            &[],
            &[
                (0, 0, forever),
                (0, 0, recurse),
                (0, 0, divide),
                (0, 0, wild),
                (0, 0, grow),
            ],
        );
        let mut i = instance(&m);
        i.fuel = 100_000;
        let f = i.func("f0").unwrap();
        assert_eq!(i.call(f, &[]).unwrap_err().0, "out of fuel");
        i.fuel = u64::MAX;
        i.deadline = Some(std::time::Instant::now());
        assert_eq!(i.call(f, &[]).unwrap_err().0, "took too long");
        i.deadline = None;
        let f = i.func("f1").unwrap();
        assert_eq!(i.call(f, &[]).unwrap_err().0, "call stack exhausted");
        let f = i.func("f2").unwrap();
        assert_eq!(i.call(f, &[]).unwrap_err().0, "integer divide by zero");
        let f = i.func("f3").unwrap();
        assert_eq!(i.call(f, &[]).unwrap_err().0, "memory access out of bounds");
        // memory.grow past the cap answers -1 rather than allocating.
        let f = i.func("f4").unwrap();
        assert!(i.call(f, &[]).is_ok());
        assert!(i.memory.len() <= 4 * 65536);
    }
}
