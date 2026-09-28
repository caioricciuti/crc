//! The official WebAssembly spec tests for numbers, run through crc's own
//! interpreter.
//!
//! `tests/spec/` holds the testsuite's numeric files at a pinned commit
//! (see `tests/spec/SOURCES.md`). Every function those files test is one
//! instruction over its parameters, so no text-format assembler is needed:
//! this reads each module's functions, builds the binary module itself,
//! and runs every `assert_return` and `assert_trap` against it. Numbers are
//! where an interpreter goes wrong quietly (NaN bits, rounding, truncation
//! traps, shift counts), and these files are the spec's own answers.
//!
//! `assert_invalid` and `assert_malformed` test a validator, which the
//! interpreter does not have by design (docs/extensions.md); they are
//! counted and left out.

use crc::ext::wasm::{Instance, Module};

// ---- reading the text format ------------------------------------------------

#[derive(Clone, Debug)]
enum Sexp {
    Atom(String),
    Str(Vec<u8>),
    List(Vec<Sexp>),
}

impl Sexp {
    fn atom(&self) -> Option<&str> {
        match self {
            Sexp::Atom(a) => Some(a),
            _ => None,
        }
    }
    fn list(&self) -> Option<&[Sexp]> {
        match self {
            Sexp::List(l) => Some(l),
            _ => None,
        }
    }
    fn head(&self) -> Option<&str> {
        self.list()?.first()?.atom()
    }
    fn string(&self) -> Option<String> {
        match self {
            Sexp::Str(s) => Some(String::from_utf8_lossy(s).into_owned()),
            _ => None,
        }
    }
}

fn parse(text: &str) -> Vec<Sexp> {
    let b = text.as_bytes();
    let mut i = 0;
    let mut stack: Vec<Vec<Sexp>> = vec![Vec::new()];
    while i < b.len() {
        match b[i] {
            b';' if b.get(i + 1) == Some(&b';') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'(' if b.get(i + 1) == Some(&b';') => {
                let mut depth = 0;
                while i < b.len() {
                    if b[i..].starts_with(b"(;") {
                        depth += 1;
                        i += 2;
                    } else if b[i..].starts_with(b";)") {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'(' => {
                stack.push(Vec::new());
                i += 1;
            }
            b')' => {
                let done = stack.pop().expect("balanced");
                stack.last_mut().expect("balanced").push(Sexp::List(done));
                i += 1;
            }
            b'"' => {
                let mut out = Vec::new();
                i += 1;
                while b[i] != b'"' {
                    if b[i] == b'\\' {
                        let next = b[i + 1];
                        if next.is_ascii_hexdigit() {
                            let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap();
                            out.push(u8::from_str_radix(hex, 16).unwrap());
                            i += 3;
                        } else {
                            out.push(match next {
                                b'n' => b'\n',
                                b't' => b'\t',
                                other => other,
                            });
                            i += 2;
                        }
                    } else {
                        out.push(b[i]);
                        i += 1;
                    }
                }
                i += 1;
                stack.last_mut().unwrap().push(Sexp::Str(out));
            }
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                let start = i;
                while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'(' && b[i] != b')' {
                    i += 1;
                }
                let atom = String::from_utf8_lossy(&b[start..i]).into_owned();
                stack.last_mut().unwrap().push(Sexp::Atom(atom));
            }
        }
    }
    stack.pop().unwrap()
}

// ---- literals ---------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Ty {
    I32,
    I64,
    F32,
    F64,
}

impl Ty {
    fn of(name: &str) -> Ty {
        Ty::try_of(name).unwrap_or_else(|| panic!("type {name}"))
    }

    /// A number type, or `None` for a reference type crc does not run.
    fn try_of(name: &str) -> Option<Ty> {
        Some(match name {
            "i32" => Ty::I32,
            "i64" => Ty::I64,
            "f32" => Ty::F32,
            "f64" => Ty::F64,
            _ => return None,
        })
    }
    fn code(self) -> u8 {
        match self {
            Ty::I32 => 0x7f,
            Ty::I64 => 0x7e,
            Ty::F32 => 0x7d,
            Ty::F64 => 0x7c,
        }
    }
}

/// An integer literal, wrapped to `bits`.
fn int(text: &str, bits: u32) -> u64 {
    let t = text.replace('_', "");
    let (neg, t) = match t.strip_prefix('-') {
        Some(rest) => (true, rest.to_string()),
        None => (false, t.trim_start_matches('+').to_string()),
    };
    let v = match t.strip_prefix("0x") {
        Some(hex) => u128::from_str_radix(hex, 16).unwrap(),
        None => t.parse::<u128>().unwrap(),
    };
    let v = if neg { v.wrapping_neg() } else { v };
    let mask = if bits == 64 {
        u64::MAX as u128
    } else {
        (1u128 << bits) - 1
    };
    (v & mask) as u64
}

/// A float literal as the bits of a float with `mant` fraction bits and
/// `exp` exponent bits, rounded to nearest, ties to even.
fn float(text: &str, mant: u32, exp: u32) -> u64 {
    let t = text.replace('_', "");
    let (neg, t) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(&t)),
    };
    let sign = if neg { 1u64 << (mant + exp) } else { 0 };
    let exp_mask = ((1u64 << exp) - 1) << mant;
    if t == "inf" {
        return sign | exp_mask;
    }
    if let Some(rest) = t.strip_prefix("nan") {
        let payload = match rest.strip_prefix(":0x") {
            Some(hex) => u64::from_str_radix(hex, 16).unwrap(),
            None => 1 << (mant - 1),
        };
        return sign | exp_mask | payload;
    }
    if let Some(hex) = t.strip_prefix("0x") {
        return sign | hex_float(hex, mant, exp);
    }
    // Decimal: the standard library parses these correctly rounded.
    let bits = if mant == 23 {
        t.parse::<f32>().unwrap().to_bits() as u64
    } else {
        t.parse::<f64>().unwrap().to_bits()
    };
    sign | bits
}

/// A hexadecimal float, correctly rounded.
fn hex_float(hex: &str, mant: u32, exp: u32) -> u64 {
    let (digits, power) = match hex.split_once(['p', 'P']) {
        Some((d, p)) => (d, p.parse::<i64>().unwrap()),
        None => (hex, 0),
    };
    let (whole, frac) = digits.split_once('.').unwrap_or((digits, ""));
    // The significant bits, as many as fit, and whether any past them are
    // set (for rounding).
    let (mut m, mut e2, mut sticky) = (0u128, power, false);
    for (i, c) in whole.chars().chain(frac.chars()).enumerate() {
        let d = c.to_digit(16).unwrap() as u128;
        if m >> 120 == 0 {
            m = (m << 4) | d;
            if i >= whole.len() {
                e2 -= 4;
            }
        } else {
            sticky |= d != 0;
            if i < whole.len() {
                e2 += 4;
            }
        }
    }
    if m == 0 {
        return 0;
    }
    let bias = (1i64 << (exp - 1)) - 1;
    let emin = 1 - bias;
    let nbits = 128 - m.leading_zeros() as i64;
    let mut top = e2 + nbits - 1;
    // Bits kept: the whole significand when normal, fewer below the
    // normal range.
    let precision = mant as i64 + 1;
    let keep = if top >= emin {
        precision
    } else {
        precision - (emin - top)
    };
    let shift = nbits - keep;
    let mut kept: u128 = if shift <= 0 {
        m << (-shift) as u32
    } else if shift >= 128 {
        0
    } else {
        let dropped = m & ((1u128 << shift) - 1);
        let half = 1u128 << (shift - 1);
        let base = m >> shift;
        // Nearest, ties to even: past half rounds up, exactly half (with
        // nothing set beyond the kept digits) rounds to the even neighbour.
        let round_up = dropped > half || (dropped == half && (sticky || base & 1 == 1));
        base + round_up as u128
    };
    if shift > 0 && shift < 128 && kept >> keep.max(0) != 0 {
        // Rounding carried into a new top bit.
        top += 1;
        if keep == precision {
            kept >>= 1;
        }
    }
    let max_exp = (1i64 << exp) - 2 - bias;
    if top > max_exp {
        return ((1u64 << exp) - 1) << mant;
    }
    if top < emin || kept >> mant == 0 {
        // Subnormal: the exponent field is zero.
        return kept as u64 & ((1u64 << (mant + 1)) - 1);
    }
    let biased = (top + bias) as u64;
    (biased << mant) | (kept as u64 & ((1u64 << mant) - 1))
}

/// A constant written as `(ty.const value)`.
fn constant(s: &Sexp) -> (Ty, u64) {
    let l = s.list().unwrap();
    let head = l[0].atom().unwrap();
    let value = l[1].atom().unwrap();
    let ty = Ty::of(head.strip_suffix(".const").unwrap());
    let bits = match ty {
        Ty::I32 => int(value, 32),
        Ty::I64 => int(value, 64),
        Ty::F32 => float(value, 23, 8),
        Ty::F64 => float(value, 52, 11),
    };
    (ty, bits)
}

// ---- opcodes ----------------------------------------------------------------

/// The encoding of a numeric instruction the interpreter supports.
fn opcode(name: &str) -> Option<Vec<u8>> {
    const PLAIN: &[&str] = &[
        "i32.eqz",
        "i32.eq",
        "i32.ne",
        "i32.lt_s",
        "i32.lt_u",
        "i32.gt_s",
        "i32.gt_u",
        "i32.le_s",
        "i32.le_u",
        "i32.ge_s",
        "i32.ge_u",
        "i64.eqz",
        "i64.eq",
        "i64.ne",
        "i64.lt_s",
        "i64.lt_u",
        "i64.gt_s",
        "i64.gt_u",
        "i64.le_s",
        "i64.le_u",
        "i64.ge_s",
        "i64.ge_u",
        "f32.eq",
        "f32.ne",
        "f32.lt",
        "f32.gt",
        "f32.le",
        "f32.ge",
        "f64.eq",
        "f64.ne",
        "f64.lt",
        "f64.gt",
        "f64.le",
        "f64.ge",
        "i32.clz",
        "i32.ctz",
        "i32.popcnt",
        "i32.add",
        "i32.sub",
        "i32.mul",
        "i32.div_s",
        "i32.div_u",
        "i32.rem_s",
        "i32.rem_u",
        "i32.and",
        "i32.or",
        "i32.xor",
        "i32.shl",
        "i32.shr_s",
        "i32.shr_u",
        "i32.rotl",
        "i32.rotr",
        "i64.clz",
        "i64.ctz",
        "i64.popcnt",
        "i64.add",
        "i64.sub",
        "i64.mul",
        "i64.div_s",
        "i64.div_u",
        "i64.rem_s",
        "i64.rem_u",
        "i64.and",
        "i64.or",
        "i64.xor",
        "i64.shl",
        "i64.shr_s",
        "i64.shr_u",
        "i64.rotl",
        "i64.rotr",
        "f32.abs",
        "f32.neg",
        "f32.ceil",
        "f32.floor",
        "f32.trunc",
        "f32.nearest",
        "f32.sqrt",
        "f32.add",
        "f32.sub",
        "f32.mul",
        "f32.div",
        "f32.min",
        "f32.max",
        "f32.copysign",
        "f64.abs",
        "f64.neg",
        "f64.ceil",
        "f64.floor",
        "f64.trunc",
        "f64.nearest",
        "f64.sqrt",
        "f64.add",
        "f64.sub",
        "f64.mul",
        "f64.div",
        "f64.min",
        "f64.max",
        "f64.copysign",
        "i32.wrap_i64",
        "i32.trunc_f32_s",
        "i32.trunc_f32_u",
        "i32.trunc_f64_s",
        "i32.trunc_f64_u",
        "i64.extend_i32_s",
        "i64.extend_i32_u",
        "i64.trunc_f32_s",
        "i64.trunc_f32_u",
        "i64.trunc_f64_s",
        "i64.trunc_f64_u",
        "f32.convert_i32_s",
        "f32.convert_i32_u",
        "f32.convert_i64_s",
        "f32.convert_i64_u",
        "f32.demote_f64",
        "f64.convert_i32_s",
        "f64.convert_i32_u",
        "f64.convert_i64_s",
        "f64.convert_i64_u",
        "f64.promote_f32",
        "i32.reinterpret_f32",
        "i64.reinterpret_f64",
        "f32.reinterpret_i32",
        "f64.reinterpret_i64",
        "i32.extend8_s",
        "i32.extend16_s",
        "i64.extend8_s",
        "i64.extend16_s",
        "i64.extend32_s",
    ];
    const SAT: &[&str] = &[
        "i32.trunc_sat_f32_s",
        "i32.trunc_sat_f32_u",
        "i32.trunc_sat_f64_s",
        "i32.trunc_sat_f64_u",
        "i64.trunc_sat_f32_s",
        "i64.trunc_sat_f32_u",
        "i64.trunc_sat_f64_s",
        "i64.trunc_sat_f64_u",
    ];
    if let Some(i) = PLAIN.iter().position(|n| *n == name) {
        return Some(vec![0x45 + i as u8]);
    }
    SAT.iter()
        .position(|n| *n == name)
        .map(|i| vec![0xfc, i as u8])
}

// ---- building a module ------------------------------------------------------

/// A module as the runner builds it from the text format: the parts crc
/// runs. A function that uses anything else (tables, imports) is built as
/// a stub that traps, so indices stay right, and its export is marked.
#[derive(Default)]
struct Program {
    types: Vec<(Vec<Ty>, Vec<Ty>)>,
    type_names: Vec<Option<String>>,
    funcs: Vec<Built>,
    func_names: Vec<Option<String>>,
    globals: Vec<(Ty, bool, Vec<u8>)>,
    global_names: Vec<Option<String>>,
    memory: Option<(u32, Option<u32>)>,
    data: Vec<(Vec<u8>, Vec<u8>)>,
    exports: Vec<(String, u8, u32)>,
    /// Function exports whose function could not be built.
    unbuilt: Vec<String>,
}

#[derive(Default)]
struct Built {
    ty: u32,
    locals: Vec<Ty>,
    body: Vec<u8>,
}

impl Program {
    fn type_of(&mut self, params: Vec<Ty>, results: Vec<Ty>) -> u32 {
        if let Some(i) = self
            .types
            .iter()
            .position(|t| t.0 == params && t.1 == results)
        {
            return i as u32;
        }
        self.types.push((params, results));
        self.type_names.push(None);
        (self.types.len() - 1) as u32
    }

    fn named(names: &[Option<String>], name: &str) -> Option<u32> {
        match names.iter().position(|n| n.as_deref() == Some(name)) {
            Some(i) => Some(i as u32),
            None => name.parse().ok(),
        }
    }
}

fn sleb(mut n: i64, out: &mut Vec<u8>) {
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        let done = (n == 0 && byte & 0x40 == 0) || (n == -1 && byte & 0x40 != 0);
        out.push(if done { byte } else { byte | 0x80 });
        if done {
            return;
        }
    }
}

fn is_name(s: &Sexp) -> Option<&str> {
    s.atom().filter(|a| a.starts_with('$'))
}

/// What a function body is compiled against.
struct Scope<'a> {
    locals: Vec<Option<String>>,
    labels: Vec<Option<String>>,
    program: &'a mut Program,
}

impl Scope<'_> {
    fn local(&self, name: &str) -> Option<u32> {
        Program::named(&self.locals, name)
    }

    /// A branch target, as a depth.
    fn label(&self, name: &str) -> Option<u32> {
        if name.starts_with('$') {
            let from_top = self
                .labels
                .iter()
                .rev()
                .position(|l| l.as_deref() == Some(name))?;
            Some(from_top as u32)
        } else {
            name.parse().ok()
        }
    }
}

/// `(param ...)`, `(result ...)` and `(local ...)` lists: the types, and a
/// name for each (`None` when unnamed).
fn declarations(items: &[Sexp], kind: &str) -> Option<(Vec<Ty>, Vec<Option<String>>)> {
    let (mut types, mut names) = (Vec::new(), Vec::new());
    for part in items.iter().filter(|p| p.head() == Some(kind)) {
        let rest = &part.list()?[1..];
        match rest.first().and_then(is_name) {
            Some(n) => {
                names.push(Some(n.to_string()));
                types.push(Ty::try_of(rest.get(1)?.atom()?)?);
            }
            None => {
                for t in rest {
                    names.push(None);
                    types.push(Ty::try_of(t.atom()?)?);
                }
            }
        }
    }
    Some((types, names))
}

/// A block's type from its `(type)`, `(param)` and `(result)` parts, and how
/// many leading items they took.
fn block_type(items: &[Sexp], program: &mut Program) -> Option<(Vec<u8>, usize)> {
    let mut used = 0;
    let mut named = None;
    while let Some(part) = items.get(used) {
        match part.head() {
            Some("type") => named = Some(part.list()?[1].atom()?.to_string()),
            Some("param") | Some("result") => {}
            _ => break,
        }
        used += 1;
    }
    let (params, _) = declarations(&items[..used], "param")?;
    let (results, _) = declarations(&items[..used], "result")?;
    let mut out = Vec::new();
    if let Some(name) = named {
        sleb(Program::named(&program.type_names, &name)? as i64, &mut out);
    } else if params.is_empty() && results.len() <= 1 {
        out.push(results.first().map_or(0x40, |t| t.code()));
    } else {
        let index = program.type_of(params, results);
        sleb(index as i64, &mut out);
    }
    Some((out, used))
}

/// Loads and stores: the opcode and the natural alignment, as a power of 2.
fn memory_op(op: &str) -> Option<(u8, u32)> {
    const OPS: &[(&str, u8, u32)] = &[
        ("i32.load", 0x28, 2),
        ("i64.load", 0x29, 3),
        ("f32.load", 0x2a, 2),
        ("f64.load", 0x2b, 3),
        ("i32.load8_s", 0x2c, 0),
        ("i32.load8_u", 0x2d, 0),
        ("i32.load16_s", 0x2e, 1),
        ("i32.load16_u", 0x2f, 1),
        ("i64.load8_s", 0x30, 0),
        ("i64.load8_u", 0x31, 0),
        ("i64.load16_s", 0x32, 1),
        ("i64.load16_u", 0x33, 1),
        ("i64.load32_s", 0x34, 2),
        ("i64.load32_u", 0x35, 2),
        ("i32.store", 0x36, 2),
        ("i64.store", 0x37, 3),
        ("f32.store", 0x38, 2),
        ("f64.store", 0x39, 3),
        ("i32.store8", 0x3a, 0),
        ("i32.store16", 0x3b, 1),
        ("i64.store8", 0x3c, 0),
        ("i64.store16", 0x3d, 1),
        ("i64.store32", 0x3e, 2),
    ];
    OPS.iter()
        .find(|(n, _, _)| *n == op)
        .map(|&(_, code, align)| (code, align))
}

/// One plain instruction and the immediates it takes from `rest`, which
/// holds what follows it. Answers how many items of `rest` it used, or
/// `None` for an instruction this runner does not build.
fn plain(op: &str, rest: &[Sexp], scope: &mut Scope, out: &mut Vec<u8>) -> Option<usize> {
    let atom = |i: usize| rest.get(i).and_then(Sexp::atom);
    Some(match op {
        "local.get" | "local.set" | "local.tee" => {
            out.push(match op {
                "local.get" => 0x20,
                "local.set" => 0x21,
                _ => 0x22,
            });
            leb(scope.local(atom(0)?)?, out);
            1
        }
        "global.get" | "global.set" => {
            out.push(if op == "global.get" { 0x23 } else { 0x24 });
            leb(Program::named(&scope.program.global_names, atom(0)?)?, out);
            1
        }
        "call" => {
            out.push(0x10);
            leb(Program::named(&scope.program.func_names, atom(0)?)?, out);
            1
        }
        "br" | "br_if" => {
            out.push(if op == "br" { 0x0c } else { 0x0d });
            leb(scope.label(atom(0)?)?, out);
            1
        }
        "br_table" => {
            let mut targets = Vec::new();
            while let Some(t) = atom(targets.len()).and_then(|a| scope.label(a)) {
                targets.push(t);
            }
            let default = targets.pop()?;
            out.push(0x0e);
            leb(targets.len() as u32, out);
            for t in &targets {
                leb(*t, out);
            }
            leb(default, out);
            targets.len() + 1
        }
        "i32.const" | "i64.const" | "f32.const" | "f64.const" => {
            let value = atom(0)?;
            match op {
                "i32.const" => {
                    out.push(0x41);
                    sleb(int(value, 32) as u32 as i32 as i64, out);
                }
                "i64.const" => {
                    out.push(0x42);
                    sleb(int(value, 64) as i64, out);
                }
                "f32.const" => {
                    out.push(0x43);
                    out.extend((float(value, 23, 8) as u32).to_le_bytes());
                }
                _ => {
                    out.push(0x44);
                    out.extend(float(value, 52, 11).to_le_bytes());
                }
            }
            1
        }
        "memory.size" => {
            out.extend([0x3f, 0]);
            0
        }
        "memory.grow" => {
            out.extend([0x40, 0]);
            0
        }
        "select" => match rest.first().filter(|r| r.head() == Some("result")) {
            Some(result) => {
                out.extend([0x1c, 1, Ty::try_of(result.list()?[1].atom()?)?.code()]);
                1
            }
            None => {
                out.push(0x1b);
                0
            }
        },
        "unreachable" => {
            out.push(0x00);
            0
        }
        "nop" => {
            out.push(0x01);
            0
        }
        "drop" => {
            out.push(0x1a);
            0
        }
        "return" => {
            out.push(0x0f);
            0
        }
        other => {
            if let Some((code, natural)) = memory_op(other) {
                let (mut offset, mut align, mut used) = (0u32, natural, 0);
                while let Some(a) = atom(used) {
                    if let Some(v) = a.strip_prefix("offset=") {
                        offset = int(v, 32) as u32;
                    } else if let Some(v) = a.strip_prefix("align=") {
                        align = int(v, 32).trailing_zeros();
                    } else {
                        break;
                    }
                    used += 1;
                }
                out.push(code);
                leb(align, out);
                leb(offset, out);
                used
            } else {
                out.extend(opcode(other)?);
                0
            }
        }
    })
}

/// Compiles a sequence of instructions, flat or folded, structured control
/// included.
fn compile(items: &[Sexp], scope: &mut Scope, out: &mut Vec<u8>) -> Option<()> {
    let mut i = 0;
    while i < items.len() {
        match &items[i] {
            Sexp::Atom(op) => match op.as_str() {
                "block" | "loop" | "if" => {
                    let label = items.get(i + 1).and_then(is_name).map(str::to_string);
                    i += 1 + label.is_some() as usize;
                    let (bt, used) = block_type(&items[i..], scope.program)?;
                    i += used;
                    out.push(match op.as_str() {
                        "block" => 0x02,
                        "loop" => 0x03,
                        _ => 0x04,
                    });
                    out.extend(bt);
                    scope.labels.push(label);
                }
                "else" => {
                    out.push(0x05);
                    i += 1 + items.get(i + 1).and_then(is_name).is_some() as usize;
                }
                "end" => {
                    out.push(0x0b);
                    scope.labels.pop()?;
                    i += 1 + items.get(i + 1).and_then(is_name).is_some() as usize;
                }
                _ => {
                    let used = plain(op, &items[i + 1..], scope, out)?;
                    i += 1 + used;
                }
            },
            Sexp::List(folded) => {
                let op = folded.first()?.atom()?;
                match op {
                    "block" | "loop" => {
                        let label = folded.get(1).and_then(is_name).map(str::to_string);
                        let at = 1 + label.is_some() as usize;
                        let (bt, used) = block_type(&folded[at..], scope.program)?;
                        out.push(if op == "block" { 0x02 } else { 0x03 });
                        out.extend(bt);
                        scope.labels.push(label);
                        compile(&folded[at + used..], scope, out)?;
                        scope.labels.pop();
                        out.push(0x0b);
                    }
                    "if" => {
                        let label = folded.get(1).and_then(is_name).map(str::to_string);
                        let at = 1 + label.is_some() as usize;
                        let (bt, used) = block_type(&folded[at..], scope.program)?;
                        let rest = &folded[at + used..];
                        let arm = |name: &str| rest.iter().find(|p| p.head() == Some(name));
                        let condition: Vec<Sexp> = rest
                            .iter()
                            .filter(|p| !matches!(p.head(), Some("then") | Some("else")))
                            .cloned()
                            .collect();
                        compile(&condition, scope, out)?;
                        out.push(0x04);
                        out.extend(bt);
                        scope.labels.push(label);
                        compile(&arm("then")?.list()?[1..], scope, out)?;
                        if let Some(otherwise) = arm("else") {
                            out.push(0x05);
                            compile(&otherwise.list()?[1..], scope, out)?;
                        }
                        scope.labels.pop();
                        out.push(0x0b);
                    }
                    _ => {
                        // Immediates first, then the folded operands, which
                        // run before the instruction itself.
                        let mut this = Vec::new();
                        let used = plain(op, &folded[1..], scope, &mut this)?;
                        compile(&folded[1 + used..], scope, out)?;
                        out.extend(this);
                    }
                }
                i += 1;
            }
            Sexp::Str(_) => return None,
        }
    }
    Some(())
}

/// Builds a `(module ...)`'s fields, or `None` when the module needs what
/// crc does not run as a whole (imports, a start function).
fn program(fields: &[Sexp]) -> Option<Program> {
    let mut p = Program::default();
    let inline_exports = |parts: &[Sexp]| -> Vec<String> {
        parts
            .iter()
            .filter(|x| x.head() == Some("export"))
            .filter_map(|x| x.list()?[1].string())
            .collect()
    };
    // Names first: a call may name a function defined further down.
    for field in fields {
        let parts = field.list()?;
        let name = parts.get(1).and_then(is_name).map(str::to_string);
        match field.head()? {
            "type" => {
                let func = parts.iter().find(|x| x.head() == Some("func"))?.list()?;
                // A type with a reference in it still takes its index.
                let params = declarations(&func[1..], "param").map(|d| d.0);
                let results = declarations(&func[1..], "result").map(|d| d.0);
                p.types
                    .push((params.unwrap_or_default(), results.unwrap_or_default()));
                p.type_names.push(name);
            }
            "func" => p.func_names.push(name),
            "import" | "start" => return None,
            _ => {}
        }
    }
    for field in fields {
        let parts = field.list()?;
        let rest = &parts[1 + parts.get(1).and_then(is_name).is_some() as usize..];
        match field.head()? {
            "func" => {
                let index = p.funcs.len() as u32;
                for name in inline_exports(rest) {
                    p.exports.push((name, 0, index));
                }
                let named_type = rest
                    .iter()
                    .find(|x| x.head() == Some("type"))
                    .and_then(|x| Program::named(&p.type_names, x.list()?[1].atom()?));
                // A reference type anywhere in the signature: a stub.
                let signature = declarations(rest, "param").zip(declarations(rest, "result"));
                let locals_ok = declarations(rest, "local");
                let ((mut params, mut names), (mut results, _)) =
                    signature.clone().unwrap_or_default();
                if params.is_empty()
                    && results.is_empty()
                    && let Some(t) = named_type
                {
                    let (tp, tr) = p.types[t as usize].clone();
                    names = vec![None; tp.len()];
                    params = tp;
                    results = tr;
                }
                let (locals, local_names) = locals_ok.clone().unwrap_or_default();
                let body: Vec<Sexp> = rest
                    .iter()
                    .filter(|x| {
                        !matches!(
                            x.head(),
                            Some("export")
                                | Some("type")
                                | Some("param")
                                | Some("result")
                                | Some("local")
                        )
                    })
                    .cloned()
                    .collect();
                let ty = p.type_of(params, results);
                names.extend(local_names);
                let mut scope = Scope {
                    locals: names,
                    labels: Vec::new(),
                    program: &mut p,
                };
                let mut code = Vec::new();
                let built = signature.is_some()
                    && locals_ok.is_some()
                    && compile(&body, &mut scope, &mut code).is_some();
                if !built {
                    let exported: Vec<String> = p
                        .exports
                        .iter()
                        .filter(|e| e.1 == 0 && e.2 == index)
                        .map(|e| e.0.clone())
                        .collect();
                    p.unbuilt.extend(exported);
                    code = vec![0x00];
                }
                code.push(0x0b);
                p.funcs.push(Built {
                    ty,
                    locals: if built { locals } else { Vec::new() },
                    body: code,
                });
            }
            "global" => {
                let index = p.globals.len() as u32;
                for name in inline_exports(rest) {
                    p.exports.push((name, 3, index));
                }
                let rest: Vec<Sexp> = rest
                    .iter()
                    .filter(|x| x.head() != Some("export"))
                    .cloned()
                    .collect();
                let (ty, mutable) = match &rest[0] {
                    Sexp::Atom(t) => (Ty::try_of(t)?, false),
                    list => (Ty::try_of(list.list()?[1].atom()?)?, true),
                };
                let mut init = Vec::new();
                let mut scope = Scope {
                    locals: Vec::new(),
                    labels: Vec::new(),
                    program: &mut p,
                };
                compile(&rest[1..], &mut scope, &mut init)?;
                init.push(0x0b);
                p.globals.push((ty, mutable, init));
                p.global_names
                    .push(parts.get(1).and_then(is_name).map(str::to_string));
            }
            "memory" => {
                for name in inline_exports(rest) {
                    p.exports.push((name, 2, 0));
                }
                let rest: Vec<&Sexp> = rest.iter().filter(|x| x.head() != Some("export")).collect();
                if let Some(data) = rest.iter().find(|x| x.head() == Some("data")) {
                    let bytes: Vec<u8> = data.list()?[1..]
                        .iter()
                        .flat_map(|s| match s {
                            Sexp::Str(b) => b.clone(),
                            _ => Vec::new(),
                        })
                        .collect();
                    let pages = bytes.len().div_ceil(65536) as u32;
                    p.memory = Some((pages, Some(pages)));
                    p.data.push((vec![0x41, 0, 0x0b], bytes));
                } else {
                    let min = int(rest.first()?.atom()?, 32) as u32;
                    let max = rest
                        .get(1)
                        .and_then(|a| a.atom())
                        .map(|a| int(a, 32) as u32);
                    p.memory = Some((min, max));
                }
            }
            "data" => {
                let rest: Vec<&Sexp> = rest.iter().filter(|x| x.atom().is_none()).collect();
                let mut offset = Vec::new();
                let expr = match rest.first()?.head() {
                    Some("offset") => rest[0].list()?[1..].to_vec(),
                    _ => vec![(*rest.first()?).clone()],
                };
                let mut scope = Scope {
                    locals: Vec::new(),
                    labels: Vec::new(),
                    program: &mut p,
                };
                compile(&expr, &mut scope, &mut offset)?;
                offset.push(0x0b);
                let bytes: Vec<u8> = rest[1..]
                    .iter()
                    .flat_map(|s| match s {
                        Sexp::Str(b) => b.clone(),
                        _ => Vec::new(),
                    })
                    .collect();
                p.data.push((offset, bytes));
            }
            "export" => {
                let name = parts[1].string()?;
                let target = parts[2].list()?;
                let kind = match target[0].atom()? {
                    "func" => 0,
                    "memory" => 2,
                    "global" => 3,
                    _ => continue,
                };
                let index = match kind {
                    0 => Program::named(&p.func_names, target[1].atom()?)?,
                    3 => Program::named(&p.global_names, target[1].atom()?)?,
                    _ => 0,
                };
                p.exports.push((name, kind, index));
            }
            // Tables and their elements serve call_indirect, which crc runs
            // but this runner does not build: functions using it are stubs.
            "type" | "table" | "elem" => {}
            _ => return None,
        }
    }
    Some(p)
}

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

fn section(id: u8, body: Vec<u8>, out: &mut Vec<u8>) {
    out.push(id);
    leb(body.len() as u32, out);
    out.extend(body);
}

fn binary(p: &Program) -> Vec<u8> {
    let mut out = b"\0asm\x01\0\0\0".to_vec();
    let mut types = Vec::new();
    leb(p.types.len() as u32, &mut types);
    for (params, results) in &p.types {
        types.push(0x60);
        leb(params.len() as u32, &mut types);
        types.extend(params.iter().map(|t| t.code()));
        leb(results.len() as u32, &mut types);
        types.extend(results.iter().map(|t| t.code()));
    }
    section(1, types, &mut out);
    let mut decls = Vec::new();
    leb(p.funcs.len() as u32, &mut decls);
    for f in &p.funcs {
        leb(f.ty, &mut decls);
    }
    section(3, decls, &mut out);
    if let Some((min, max)) = p.memory {
        let mut body = vec![1];
        match max {
            Some(max) => {
                body.push(1);
                leb(min, &mut body);
                leb(max, &mut body);
            }
            None => {
                body.push(0);
                leb(min, &mut body);
            }
        }
        section(5, body, &mut out);
    }
    if !p.globals.is_empty() {
        let mut body = Vec::new();
        leb(p.globals.len() as u32, &mut body);
        for (ty, mutable, init) in &p.globals {
            body.push(ty.code());
            body.push(*mutable as u8);
            body.extend(init);
        }
        section(6, body, &mut out);
    }
    let mut exports = Vec::new();
    leb(p.exports.len() as u32, &mut exports);
    for (name, kind, index) in &p.exports {
        leb(name.len() as u32, &mut exports);
        exports.extend(name.as_bytes());
        exports.push(*kind);
        leb(*index, &mut exports);
    }
    section(7, exports, &mut out);
    let mut code = Vec::new();
    leb(p.funcs.len() as u32, &mut code);
    for f in &p.funcs {
        let mut body = Vec::new();
        leb(f.locals.len() as u32, &mut body);
        for t in &f.locals {
            body.push(1);
            body.push(t.code());
        }
        body.extend(&f.body);
        leb(body.len() as u32, &mut code);
        code.extend(body);
    }
    section(10, code, &mut out);
    if !p.data.is_empty() {
        let mut body = Vec::new();
        leb(p.data.len() as u32, &mut body);
        for (offset, bytes) in &p.data {
            body.push(0);
            body.extend(offset);
            leb(bytes.len() as u32, &mut body);
            body.extend(bytes);
        }
        section(11, body, &mut out);
    }
    out
}

// ---- running ----------------------------------------------------------------

/// A constant, or `None` for a value this runner does not model (a
/// reference, `either`).
fn value(s: &Sexp) -> Option<(Ty, u64)> {
    let head = s.head()?;
    matches!(head, "i32.const" | "i64.const" | "f32.const" | "f64.const").then(|| constant(s))
}

/// Whether `got` is what `expected` says.
fn matches(expected: &Sexp, got: u64) -> bool {
    let l = expected.list().unwrap();
    let head = l[0].atom().unwrap();
    let value = l[1].atom().unwrap();
    let ty = Ty::of(head.strip_suffix(".const").unwrap());
    let (bits, quiet, exp_mask, width) = match ty {
        Ty::I32 => return got as u32 as u64 == int(value, 32),
        Ty::I64 => return got == int(value, 64),
        Ty::F32 => (got & 0xffff_ffff, 1u64 << 22, 0xffu64 << 23, 31),
        Ty::F64 => (got, 1u64 << 51, 0x7ffu64 << 52, 63),
    };
    let magnitude = bits & !(1u64 << width);
    match value.trim_start_matches(['-', '+']) {
        // Either sign; the payload exactly the canonical one.
        "nan:canonical" => magnitude == exp_mask | quiet,
        // Either sign; a NaN with the quiet bit set.
        "nan:arithmetic" => magnitude & (exp_mask | quiet) == exp_mask | quiet,
        _ => bits == constant(expected).1,
    }
}

#[derive(Default, Debug)]
struct Tally {
    passed: usize,
    failed: Vec<String>,
    not_run: usize,
    validation: usize,
}

/// crc's own cap on memory, the one it gives extensions.
const MAX_PAGES: usize = 1024;

/// A module instantiated, and the exports that are stubs.
type Loaded = (Instance, Vec<String>);

/// A module form built and instantiated. `Err` when it builds but does not
/// instantiate; `None` when it needs what this runner does not build.
fn instantiate(form: &[Sexp]) -> Option<Result<Loaded, String>> {
    let fields = match form.get(1) {
        Some(s) if is_name(s).is_some() => &form[2..],
        _ => &form[1..],
    };
    let (bytes, unbuilt) = if fields.first().and_then(Sexp::atom) == Some("binary") {
        let bytes = fields[1..]
            .iter()
            .flat_map(|s| match s {
                Sexp::Str(b) => b.clone(),
                _ => Vec::new(),
            })
            .collect();
        (bytes, Vec::new())
    } else if fields.first().and_then(Sexp::atom) == Some("quote") {
        return None;
    } else {
        let p = program(fields)?;
        (binary(&p), p.unbuilt)
    };
    let module = match Module::parse(&bytes) {
        Ok(m) => m,
        Err(t) => return Some(Err(format!("parse: {t}"))),
    };
    Some(
        Instance::new(module, MAX_PAGES, |_, _| None)
            .map(|i| (i, unbuilt))
            .map_err(|t| t.0),
    )
}

fn run_file(name: &str, tally: &mut Tally) {
    let path = format!("{}/tests/spec/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap();
    // Every module so far, with its name if it has one: an action runs
    // against the latest, or the one it names.
    let mut modules: Vec<(Option<String>, Option<Loaded>)> = Vec::new();
    for form in parse(&text) {
        let items = form.list().unwrap();
        match form.head().unwrap() {
            "module" => {
                let built = instantiate(items).and_then(|r| match r {
                    Ok(built) => Some(built),
                    Err(e) => {
                        tally
                            .failed
                            .push(format!("{name}: a module would not load: {e}"));
                        None
                    }
                });
                let label = items.get(1).and_then(is_name).map(str::to_string);
                modules.push((label, built));
            }
            "register" => {}
            "assert_invalid" | "assert_malformed" | "assert_unlinkable" => tally.validation += 1,
            // A module that must fail to instantiate (a data segment out of
            // bounds): built, it must be refused.
            "assert_uninstantiable" | "assert_trap" if items[1].head() == Some("module") => {
                match instantiate(items[1].list().unwrap()) {
                    Some(Err(_)) => tally.passed += 1,
                    Some(Ok(_)) => tally
                        .failed
                        .push(format!("{name}: a module that should not instantiate did")),
                    None => tally.not_run += 1,
                }
            }
            kind @ ("assert_return" | "assert_trap" | "assert_exhaustion" | "invoke") => {
                let action = if kind == "invoke" { &form } else { &items[1] };
                let action = action.list().unwrap();
                if action[0].atom() != Some("invoke") {
                    // `(get "global")`: reading an exported global is not
                    // something the interpreter's API offers.
                    tally.not_run += 1;
                    continue;
                }
                let (target, at) = match action.get(1).and_then(is_name) {
                    Some(label) => (Some(label.to_string()), 2),
                    None => (None, 1),
                };
                let export = action[at].string().unwrap();
                let args: Option<Vec<u64>> = action[at + 1..]
                    .iter()
                    .map(|a| value(a).map(|v| v.1))
                    .collect();
                let expected = if kind == "assert_return" {
                    &items[2..]
                } else {
                    &[][..]
                };
                let instance = match target {
                    Some(label) => modules
                        .iter_mut()
                        .rev()
                        .find(|(n, _)| n.as_deref() == Some(label.as_str())),
                    None => modules.last_mut(),
                }
                .and_then(|(_, built)| built.as_mut());
                let (Some((i, unbuilt)), Some(args)) = (instance, args) else {
                    tally.not_run += 1;
                    continue;
                };
                if unbuilt.contains(&export) || expected.iter().any(|e| value(e).is_none()) {
                    tally.not_run += 1;
                    continue;
                }
                let Some(func) = i.func(&export) else {
                    tally.not_run += 1;
                    continue;
                };
                i.fuel = 50_000_000;
                let result = i.call(func, &args);
                if kind == "invoke" {
                    continue;
                }
                let ok = match (kind, &result) {
                    ("assert_trap" | "assert_exhaustion", r) => r.is_err(),
                    (_, Ok(values)) => {
                        values.len() == expected.len()
                            && expected.iter().zip(values).all(|(e, &g)| matches(e, g))
                    }
                    (_, Err(_)) => false,
                };
                if ok {
                    tally.passed += 1;
                } else {
                    tally.failed.push(format!(
                        "{name}: {kind} {export} {:?} -> {result:?}",
                        &action[at + 1..]
                    ));
                }
            }
            other => panic!("{name}: unknown form {other}"),
        }
    }
}

#[test]
fn the_official_numeric_spec_tests_pass() {
    let mut tally = Tally::default();
    for name in [
        "i32.wast",
        "i64.wast",
        "f32.wast",
        "f64.wast",
        "f32_cmp.wast",
        "f64_cmp.wast",
        "f32_bitwise.wast",
        "f64_bitwise.wast",
        "conversions.wast",
    ] {
        run_file(name, &mut tally);
    }
    eprintln!(
        "spec: {} passed, {} failed, {} not run, {} validation cases left out",
        tally.passed,
        tally.failed.len(),
        tally.not_run,
        tally.validation
    );
    assert!(
        tally.failed.is_empty(),
        "{} of {} failed, first 40:\n{}",
        tally.failed.len(),
        tally.passed + tally.failed.len(),
        tally
            .failed
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(
        tally.not_run, 0,
        "every function and assertion in these files runs"
    );
    assert!(tally.passed > 11_000, "{tally:?}");
}

/// The testsuite's files for control flow, locals, globals, calls and
/// memory, built as whole modules. Functions that need what this runner
/// does not build (tables for call_indirect, imports) are counted, not
/// run; every assertion that runs must pass.
#[test]
fn the_official_control_and_memory_spec_tests_pass() {
    let mut tally = Tally::default();
    for name in [
        "address.wast",
        "block.wast",
        "br.wast",
        "br_if.wast",
        "br_table.wast",
        "loop.wast",
        "if.wast",
        "local_get.wast",
        "local_set.wast",
        "local_tee.wast",
        "select.wast",
        "nop.wast",
        "return.wast",
        "unreachable.wast",
        "load.wast",
        "store.wast",
        "endianness.wast",
        "memory_grow.wast",
        "memory_size.wast",
        "memory.wast",
        "fac.wast",
        "call.wast",
        "stack.wast",
        "labels.wast",
        "switch.wast",
        "left-to-right.wast",
        "forward.wast",
        "unwind.wast",
        "global.wast",
    ] {
        let before = tally.not_run;
        run_file(name, &mut tally);
        if tally.not_run > before {
            eprintln!("  {name}: {} not run", tally.not_run - before);
        }
    }
    eprintln!(
        "control and memory: {} passed, {} failed, {} not run, {} validation cases left out",
        tally.passed,
        tally.failed.len(),
        tally.not_run,
        tally.validation
    );
    assert!(
        tally.failed.is_empty(),
        "{} failed, first 60:\n{}",
        tally.failed.len(),
        tally
            .failed
            .iter()
            .take(60)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    // What is not run needs tables, imports or references (see
    // tests/spec/SOURCES.md). More than this means the builder stopped
    // building something it built before.
    assert!(tally.not_run <= 194, "{} not run", tally.not_run);
    assert!(tally.passed >= 1_666, "{tally:?}");
}

/// The testsuite's expression files: arithmetic that must not be folded,
/// reordered or fused, and literals written every way the text format
/// allows. Functions using control flow, memory or globals are not built
/// here (they are counted); every assertion that runs must pass.
#[test]
fn the_official_expression_spec_tests_pass() {
    let mut tally = Tally::default();
    for name in [
        "int_exprs.wast",
        "float_exprs.wast",
        "float_misc.wast",
        "int_literals.wast",
        "float_literals.wast",
    ] {
        run_file(name, &mut tally);
    }
    eprintln!(
        "expressions: {} passed, {} failed, {} not run, {} validation cases left out",
        tally.passed,
        tally.failed.len(),
        tally.not_run,
        tally.validation
    );
    assert!(
        tally.failed.is_empty(),
        "{} failed, first 40:\n{}",
        tally.failed.len(),
        tally
            .failed
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(tally.passed > 1_000, "{tally:?}");
}

#[test]
fn hex_floats_round_to_nearest_even() {
    // Spot checks against values the spec files rely on.
    assert_eq!(float("0x1p-149", 23, 8), 1);
    assert_eq!(float("0x1p-126", 23, 8), 0x0080_0000);
    assert_eq!(float("0x1.fffffep+127", 23, 8), 0x7f7f_ffff);
    assert_eq!(float("0x1.fffffefffffffffffp+127", 23, 8), 0x7f7f_ffff);
    assert_eq!(
        float("0x1.ffffffp+127", 23, 8),
        0x7f80_0000,
        "rounds up to infinity"
    );
    assert_eq!(
        float("0x1.000001p+0", 23, 8),
        0x3f80_0000,
        "a tie goes to even"
    );
    assert_eq!(
        float("0x1.000003p+0", 23, 8),
        0x3f80_0002,
        "a tie goes to even, upward"
    );
    assert_eq!(
        float("0x1.0000010000000000001p+0", 23, 8),
        0x3f80_0001,
        "past the tie"
    );
    assert_eq!(float("-0x0p+0", 23, 8), 0x8000_0000);
    assert_eq!(float("0x1p-1074", 52, 11), 1);
    assert_eq!(
        float("0x1.fffffffffffffp+1023", 52, 11),
        0x7fef_ffff_ffff_ffff
    );
    assert_eq!(float("nan:0x200000", 23, 8), 0x7fa0_0000);
    assert_eq!(float("-inf", 52, 11), 0xfff0_0000_0000_0000);
}
