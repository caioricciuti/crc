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
        leb(funcs.len() as u32, &mut body);
        for i in 0..funcs.len() as u32 {
            let name = format!("f{}", i + imports.len() as u32);
            leb(name.len() as u32, &mut body);
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            leb(i + imports.len() as u32, &mut body);
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
