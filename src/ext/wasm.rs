//! A small WebAssembly interpreter, what runs extensions.
//!
//! Enough of the 2.0 core spec for what rustc emits for
//! wasm32-unknown-unknown: integer and float arithmetic, memory, globals,
//! tables and indirect calls, sign extension, saturating truncation, bulk
//! memory copy and fill, multi-value block types, and imported functions,
//! which the host supplies when it links the module. No SIMD, no threads.
//! The design and the measurements behind choosing this over a crate are in
//! `docs/extensions.md`.
//!
//! Safety model: no `unsafe`, and nothing panics on a bad module. Every
//! stack pop, index and memory access is checked and becomes a `Trap`.
//! Without a full spec validator a malformed module can misbehave inside
//! its own sandbox (wrong results, a trap), but cannot reach outside it.
//! Fuel bounds the instructions a call may run, and frame depth is capped,
//! so an extension cannot hang or overflow the editor.

use std::fmt;
use std::time::Instant;

#[derive(Debug, Clone, PartialEq)]
pub struct Trap(pub String);

impl fmt::Display for Trap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn trap<T>(what: impl Into<String>) -> Result<T, Trap> {
    Err(Trap(what.into()))
}

const PAGE: usize = 65536;
const MAX_FRAMES: usize = 2048;
const MAX_STACK: usize = 1 << 20;

// ---- decoding ----------------------------------------------------------

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, at: 0 }
    }
    fn done(&self) -> bool {
        self.at >= self.bytes.len()
    }
    fn byte(&mut self) -> Result<u8, Trap> {
        let b = *self
            .bytes
            .get(self.at)
            .ok_or_else(|| Trap("unexpected end".into()))?;
        self.at += 1;
        Ok(b)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], Trap> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| Trap("length overflow".into()))?;
        let s = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| Trap("unexpected end".into()))?;
        self.at = end;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32, Trap> {
        let mut result: u64 = 0;
        let mut shift = 0;
        loop {
            let b = self.byte()?;
            result |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 35 {
                return trap("bad u32");
            }
        }
        u32::try_from(result).or(trap("u32 out of range"))
    }
    fn signed(&mut self, bits: u32) -> Result<i64, Trap> {
        let mut result: i64 = 0;
        let mut shift = 0;
        loop {
            let b = self.byte()?;
            result |= i64::from(b & 0x7f) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                if shift < 64 && b & 0x40 != 0 {
                    result |= -1i64 << shift;
                }
                break;
            }
            // At most ceil(bits / 7) bytes: one more would shift past the
            // value, which a malformed module is free to try.
            if shift >= bits.div_ceil(7) * 7 {
                return trap("bad signed leb");
            }
        }
        Ok(result)
    }
    fn name(&mut self) -> Result<String, Trap> {
        let n = self.u32()? as usize;
        let s = self.take(n)?;
        String::from_utf8(s.to_vec()).or(trap("bad name"))
    }
}

#[derive(Clone, Debug, PartialEq)]
struct FuncType {
    params: usize,
    results: usize,
}

#[derive(Clone, Copy, Debug)]
enum Load {
    I32,
    I64,
    F32,
    F64,
    I32S8,
    I32U8,
    I32S16,
    I32U16,
    I64S8,
    I64U8,
    I64S16,
    I64U16,
    I64S32,
    I64U32,
}

#[derive(Clone, Copy, Debug)]
enum Store {
    B32,
    B64,
    B8,
    B16,
    B64As32,
}

/// One decoded instruction. Branch targets are instruction indexes.
#[derive(Clone, Copy, Debug)]
enum Op {
    Unreachable,
    Nop,
    /// `end`: index of the matching End. Arity counts are block results
    /// (what a branch carries) and params (what entry takes).
    Block {
        end: u32,
        params: u16,
        results: u16,
    },
    Loop {
        params: u16,
    },
    If {
        else_: u32,
        end: u32,
        params: u16,
        results: u16,
    },
    Else {
        end: u32,
    },
    End,
    Br(u32),
    BrIf(u32),
    BrTable {
        at: u32,
        len: u32,
    },
    Return,
    Call(u32),
    /// The table index is always 0 before reference types.
    CallIndirect {
        ty: u32,
    },
    Drop,
    Select,
    LocalGet(u32),
    LocalSet(u32),
    LocalTee(u32),
    GlobalGet(u32),
    GlobalSet(u32),
    Load(Load, u32),
    Store(Store, u32),
    MemorySize,
    MemoryGrow,
    Const(u64),
    Num(u8),
    Sat(u8),
    MemoryCopy,
    MemoryFill,
}

struct Func {
    ty: FuncType,
    locals: usize,
    code: Vec<Op>,
    tables: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Extern {
    Func(u32),
    Memory,
    Global(u32),
    Table(u32),
}

pub struct Module {
    types: Vec<FuncType>,
    /// Imported functions: module, name and type. They come first in the
    /// function index space.
    imports: Vec<(String, String, FuncType)>,
    funcs: Vec<Func>,
    memory: Option<(u32, Option<u32>)>,
    globals: Vec<(bool, u64)>,
    tables: Vec<(u32, Option<u32>)>,
    elements: Vec<(u32, u32, Vec<u32>)>,
    data: Vec<(u32, Vec<u8>)>,
    exports: Vec<(String, Extern)>,
    start: Option<u32>,
}

fn block_type(r: &mut Reader, types: &[FuncType]) -> Result<(u16, u16), Trap> {
    let first = *r
        .bytes
        .get(r.at)
        .ok_or_else(|| Trap("unexpected end".into()))?;
    if first == 0x40 {
        r.at += 1;
        return Ok((0, 0));
    }
    if matches!(first, 0x7f | 0x7e | 0x7d | 0x7c | 0x70 | 0x6f) {
        r.at += 1;
        return Ok((0, 1));
    }
    let index = r.signed(33)?;
    let ty = usize::try_from(index)
        .ok()
        .and_then(|i| types.get(i))
        .ok_or_else(|| Trap("bad block type".into()))?;
    Ok((ty.params as u16, ty.results as u16))
}

/// A constant expression: one const or global.get, then end.
fn const_expr(r: &mut Reader, globals: &[(bool, u64)]) -> Result<u64, Trap> {
    let value = match r.byte()? {
        0x41 => r.signed(32)? as i32 as u32 as u64,
        0x42 => r.signed(64)? as u64,
        0x43 => u32::from_le_bytes(r.take(4)?.try_into().unwrap_or([0; 4])) as u64,
        0x44 => u64::from_le_bytes(r.take(8)?.try_into().unwrap_or([0; 8])),
        0x23 => {
            let i = r.u32()? as usize;
            globals
                .get(i)
                .ok_or_else(|| Trap("bad global in init".into()))?
                .1
        }
        0xd0 => {
            r.byte()?;
            u64::MAX
        }
        other => return trap(format!("unsupported constant expression 0x{other:02x}")),
    };
    if r.byte()? != 0x0b {
        return trap("constant expression too long");
    }
    Ok(value)
}

fn decode_body(body: &[u8], types: &[FuncType], ty: FuncType) -> Result<Func, Trap> {
    let mut r = Reader::new(body);
    let mut locals = 0usize;
    for _ in 0..r.u32()? {
        let n = r.u32()? as usize;
        r.byte()?;
        locals = locals
            .checked_add(n)
            .filter(|&l| l < 50_000)
            .ok_or_else(|| Trap("too many locals".into()))?;
    }
    let mut code = Vec::new();
    let mut tables = Vec::new();
    // Open blocks: index of the opening op, for patching ends.
    let mut open: Vec<usize> = Vec::new();
    while !r.done() {
        let op = r.byte()?;
        let decoded = match op {
            0x00 => Op::Unreachable,
            0x01 => Op::Nop,
            0x02..=0x04 => {
                let (params, results) = block_type(&mut r, types)?;
                open.push(code.len());
                match op {
                    0x02 => Op::Block {
                        end: 0,
                        params,
                        results,
                    },
                    0x03 => Op::Loop { params },
                    _ => Op::If {
                        else_: 0,
                        end: 0,
                        params,
                        results,
                    },
                }
            }
            0x05 => {
                let at = *open.last().ok_or_else(|| Trap("else outside if".into()))?;
                let here = code.len() as u32;
                match &mut code[at] {
                    Op::If { else_, .. } => *else_ = here,
                    _ => return trap("else outside if"),
                }
                Op::Else { end: 0 }
            }
            0x0b => {
                if let Some(at) = open.pop() {
                    let end = code.len() as u32;
                    let mut else_at = None;
                    match &mut code[at] {
                        Op::Block { end: e, .. } => *e = end,
                        Op::If { end: e, else_, .. } => {
                            *e = end;
                            if *else_ != 0 {
                                else_at = Some(*else_ as usize);
                            }
                        }
                        _ => {}
                    }
                    if let Some(e) = else_at
                        && let Op::Else { end: target } = &mut code[e]
                    {
                        *target = end;
                    }
                }
                Op::End
            }
            0x0c => Op::Br(r.u32()?),
            0x0d => Op::BrIf(r.u32()?),
            0x0e => {
                let len = r.u32()?;
                let at = tables.len() as u32;
                for _ in 0..=len {
                    tables.push(r.u32()?);
                }
                Op::BrTable { at, len }
            }
            0x0f => Op::Return,
            0x10 => Op::Call(r.u32()?),
            0x11 => {
                let ty = r.u32()?;
                r.u32()?;
                Op::CallIndirect { ty }
            }
            0x1a => Op::Drop,
            0x1b => Op::Select,
            0x1c => {
                for _ in 0..r.u32()? {
                    r.byte()?;
                }
                Op::Select
            }
            0x20 => Op::LocalGet(r.u32()?),
            0x21 => Op::LocalSet(r.u32()?),
            0x22 => Op::LocalTee(r.u32()?),
            0x23 => Op::GlobalGet(r.u32()?),
            0x24 => Op::GlobalSet(r.u32()?),
            0x28..=0x3e => {
                r.u32()?;
                let offset = r.u32()?;
                match op {
                    0x28 => Op::Load(Load::I32, offset),
                    0x29 => Op::Load(Load::I64, offset),
                    0x2a => Op::Load(Load::F32, offset),
                    0x2b => Op::Load(Load::F64, offset),
                    0x2c => Op::Load(Load::I32S8, offset),
                    0x2d => Op::Load(Load::I32U8, offset),
                    0x2e => Op::Load(Load::I32S16, offset),
                    0x2f => Op::Load(Load::I32U16, offset),
                    0x30 => Op::Load(Load::I64S8, offset),
                    0x31 => Op::Load(Load::I64U8, offset),
                    0x32 => Op::Load(Load::I64S16, offset),
                    0x33 => Op::Load(Load::I64U16, offset),
                    0x34 => Op::Load(Load::I64S32, offset),
                    0x35 => Op::Load(Load::I64U32, offset),
                    0x36 | 0x38 => Op::Store(Store::B32, offset),
                    0x37 | 0x39 => Op::Store(Store::B64, offset),
                    0x3a | 0x3c => Op::Store(Store::B8, offset),
                    0x3b | 0x3d => Op::Store(Store::B16, offset),
                    _ => Op::Store(Store::B64As32, offset),
                }
            }
            0x3f => {
                r.byte()?;
                Op::MemorySize
            }
            0x40 => {
                r.byte()?;
                Op::MemoryGrow
            }
            0x41 => Op::Const(r.signed(32)? as i32 as u32 as u64),
            0x42 => Op::Const(r.signed(64)? as u64),
            0x43 => Op::Const(u32::from_le_bytes(r.take(4)?.try_into().unwrap_or([0; 4])) as u64),
            0x44 => Op::Const(u64::from_le_bytes(r.take(8)?.try_into().unwrap_or([0; 8]))),
            0x45..=0xc4 => Op::Num(op),
            0xfc => match r.u32()? {
                n @ 0..=7 => Op::Sat(n as u8),
                10 => {
                    r.byte()?;
                    r.byte()?;
                    Op::MemoryCopy
                }
                11 => {
                    r.byte()?;
                    Op::MemoryFill
                }
                n => return trap(format!("unsupported 0xfc {n}")),
            },
            other => return trap(format!("unsupported opcode 0x{other:02x}")),
        };
        code.push(decoded);
    }
    if !open.is_empty() {
        return trap("unclosed block");
    }
    Ok(Func {
        ty,
        locals,
        code,
        tables,
    })
}

impl Module {
    pub fn parse(bytes: &[u8]) -> Result<Module, Trap> {
        let mut r = Reader::new(bytes);
        if r.take(8)? != b"\0asm\x01\0\0\0" {
            return trap("not a wasm 1 module");
        }
        let mut m = Module {
            types: Vec::new(),
            imports: Vec::new(),
            funcs: Vec::new(),
            memory: None,
            globals: Vec::new(),
            tables: Vec::new(),
            elements: Vec::new(),
            data: Vec::new(),
            exports: Vec::new(),
            start: None,
        };
        let mut declared: Vec<u32> = Vec::new();
        while !r.done() {
            let id = r.byte()?;
            let len = r.u32()? as usize;
            let mut s = Reader::new(r.take(len)?);
            match id {
                0 | 12 => {}
                1 => {
                    for _ in 0..s.u32()? {
                        if s.byte()? != 0x60 {
                            return trap("bad func type");
                        }
                        let params = s.u32()? as usize;
                        s.take(params)?;
                        let results = s.u32()? as usize;
                        s.take(results)?;
                        m.types.push(FuncType { params, results });
                    }
                }
                2 => {
                    for _ in 0..s.u32()? {
                        let module = s.name()?;
                        let name = s.name()?;
                        if s.byte()? != 0 {
                            return trap(format!(
                                "imports {module}.{name}, which is not a function"
                            ));
                        }
                        let ty = s.u32()? as usize;
                        let ty = m
                            .types
                            .get(ty)
                            .ok_or_else(|| Trap("bad import type".into()))?
                            .clone();
                        m.imports.push((module, name, ty));
                    }
                }
                3 => {
                    for _ in 0..s.u32()? {
                        declared.push(s.u32()?);
                    }
                }
                4 => {
                    for _ in 0..s.u32()? {
                        s.byte()?;
                        let flags = s.byte()?;
                        let min = s.u32()?;
                        let max = if flags & 1 != 0 { Some(s.u32()?) } else { None };
                        m.tables.push((min, max));
                    }
                }
                5 => {
                    for _ in 0..s.u32()? {
                        let flags = s.byte()?;
                        let min = s.u32()?;
                        let max = if flags & 1 != 0 { Some(s.u32()?) } else { None };
                        m.memory = Some((min, max));
                    }
                }
                6 => {
                    for _ in 0..s.u32()? {
                        s.byte()?;
                        let mutable = s.byte()? == 1;
                        let value = const_expr(&mut s, &m.globals)?;
                        m.globals.push((mutable, value));
                    }
                }
                7 => {
                    for _ in 0..s.u32()? {
                        let name = s.name()?;
                        let kind = s.byte()?;
                        let index = s.u32()?;
                        let ext = match kind {
                            0 => Extern::Func(index),
                            1 => Extern::Table(index),
                            2 => Extern::Memory,
                            _ => Extern::Global(index),
                        };
                        m.exports.push((name, ext));
                    }
                }
                8 => m.start = Some(s.u32()?),
                9 => {
                    for _ in 0..s.u32()? {
                        let flags = s.u32()?;
                        if flags != 0 {
                            return trap(format!("element segment kind {flags} not supported"));
                        }
                        let offset = const_expr(&mut s, &m.globals)? as u32;
                        let mut items = Vec::new();
                        for _ in 0..s.u32()? {
                            items.push(s.u32()?);
                        }
                        m.elements.push((0, offset, items));
                    }
                }
                10 => {
                    let count = s.u32()? as usize;
                    if count != declared.len() {
                        return trap("function and code counts differ");
                    }
                    for &ty in &declared {
                        let size = s.u32()? as usize;
                        let body = s.take(size)?;
                        let ty = m
                            .types
                            .get(ty as usize)
                            .ok_or_else(|| Trap("bad type index".into()))?
                            .clone();
                        m.funcs.push(decode_body(body, &m.types, ty)?);
                    }
                }
                11 => {
                    for _ in 0..s.u32()? {
                        let flags = s.u32()?;
                        if flags != 0 {
                            return trap(format!("data segment kind {flags} not supported"));
                        }
                        let offset = const_expr(&mut s, &m.globals)? as u32;
                        let n = s.u32()? as usize;
                        m.data.push((offset, s.take(n)?.to_vec()));
                    }
                }
                other => return trap(format!("unknown section {other}")),
            }
        }
        Ok(m)
    }

    pub fn export(&self, name: &str) -> Option<Extern> {
        self.exports
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, e)| *e)
    }

    /// Every export's name.
    pub fn export_names(&self) -> impl Iterator<Item = &str> {
        self.exports.iter().map(|(n, _)| n.as_str())
    }

    /// What the module imports, as `module.name`.
    pub fn import_names(&self) -> impl Iterator<Item = (&str, &str)> {
        self.imports
            .iter()
            .map(|(m, n, _)| (m.as_str(), n.as_str()))
    }

    /// The type of function `index` in the whole index space.
    fn func_type(&self, index: usize) -> Option<&FuncType> {
        match index.checked_sub(self.imports.len()) {
            None => self.imports.get(index).map(|i| &i.2),
            Some(defined) => self.funcs.get(defined).map(|f| &f.ty),
        }
    }
}

// ---- execution ---------------------------------------------------------

/// A function the host supplies for an import: it gets the module's memory
/// and the arguments, and returns the result, if the type has one.
pub type Host = Box<dyn FnMut(&mut [u8], &[u64]) -> Result<Option<u64>, Trap> + Send>;

pub struct Instance {
    module: Module,
    hosts: Vec<Host>,
    /// When a call must be over, checked every 65536 instructions.
    pub deadline: Option<Instant>,
    pub memory: Vec<u8>,
    max_pages: usize,
    globals: Vec<u64>,
    table: Vec<Option<u32>>,
    /// Instructions a call may still run.
    pub fuel: u64,
}

struct Label {
    /// Where a branch goes, and the values it carries. A loop's target is
    /// its Loop op, which pushes the label again; a block's is its End,
    /// which pops it.
    target: usize,
    arity: usize,
    height: usize,
    is_loop: bool,
}

struct Frame {
    func: usize,
    pc: usize,
    base: usize,
    labels: usize,
}

impl Instance {
    /// `max_pages` caps memory whatever the module asks for. `link` supplies
    /// each import, or refuses it, which fails the whole instance.
    pub fn new(
        module: Module,
        max_pages: usize,
        mut link: impl FnMut(&str, &str) -> Option<Host>,
    ) -> Result<Instance, Trap> {
        let mut hosts = Vec::new();
        for (m, n, _) in &module.imports {
            hosts.push(link(m, n).ok_or_else(|| Trap(format!("{m}.{n} is not available")))?);
        }
        let (min, max) = module.memory.unwrap_or((0, Some(0)));
        let max_pages = max.map_or(max_pages, |m| (m as usize).min(max_pages));
        if min as usize > max_pages {
            return trap("memory larger than allowed");
        }
        let mut memory = vec![0u8; min as usize * PAGE];
        for (offset, bytes) in &module.data {
            let at = *offset as usize;
            memory
                .get_mut(at..at + bytes.len())
                .ok_or_else(|| Trap("data segment out of bounds".into()))?
                .copy_from_slice(bytes);
        }
        let size = module
            .tables
            .first()
            .map_or(0, |t| t.0 as usize)
            .min(1 << 16);
        let mut table = vec![None; size];
        for (_, offset, items) in &module.elements {
            for (i, f) in items.iter().enumerate() {
                *table
                    .get_mut(*offset as usize + i)
                    .ok_or_else(|| Trap("element out of bounds".into()))? = Some(*f);
            }
        }
        let globals = module.globals.iter().map(|g| g.1).collect();
        let mut instance = Instance {
            module,
            hosts,
            deadline: None,
            memory,
            max_pages,
            globals,
            table,
            fuel: u64::MAX,
        };
        if let Some(start) = instance.module.start {
            instance.call(start, &[])?;
        }
        Ok(instance)
    }

    pub fn func(&self, name: &str) -> Option<u32> {
        match self.module.export(name)? {
            Extern::Func(f) => Some(f),
            _ => None,
        }
    }

    pub fn call(&mut self, func: u32, args: &[u64]) -> Result<Vec<u64>, Trap> {
        let ty = self
            .module
            .func_type(func as usize)
            .ok_or_else(|| Trap("no such function".into()))?;
        if ty.params != args.len() {
            return trap("wrong argument count");
        }
        let results = ty.results;
        let mut stack: Vec<u64> = args.to_vec();
        if (func as usize) < self.module.imports.len() {
            self.call_host(func as usize, &mut stack)?;
        } else {
            self.run(func as usize, &mut stack)?;
        }
        if stack.len() < results {
            return trap("missing results");
        }
        Ok(stack.split_off(stack.len() - results))
    }

    /// Runs defined function `entry` (whole index space) to completion.
    fn run(&mut self, entry: usize, stack: &mut Vec<u64>) -> Result<(), Trap> {
        let mut frames: Vec<Frame> = Vec::new();
        let mut labels: Vec<Label> = Vec::new();
        self.enter(entry, stack, &mut frames, &mut labels)?;
        macro_rules! pop {
            () => {
                stack.pop().ok_or_else(|| Trap("stack underflow".into()))?
            };
        }
        macro_rules! push {
            ($v:expr) => {
                stack.push($v)
            };
        }
        while let Some(frame) = frames.last_mut() {
            let (fi, pc, base) = (frame.func, frame.pc, frame.base);
            let Some(&op) = self.module.funcs[fi].code.get(pc) else {
                return trap("ran off the end of a function");
            };
            frame.pc = pc + 1;
            if self.fuel == 0 {
                return trap("out of fuel");
            }
            self.fuel -= 1;
            if self.fuel & 0xffff == 0 && self.deadline.is_some_and(|d| Instant::now() >= d) {
                return trap("took too long");
            }
            let jump = |frames: &mut Vec<Frame>, to: usize| {
                if let Some(f) = frames.last_mut() {
                    f.pc = to;
                }
            };
            match op {
                Op::Unreachable => return trap("unreachable"),
                Op::Nop => {}
                Op::Block {
                    end,
                    params,
                    results,
                } => labels.push(Label {
                    target: end as usize,
                    arity: results as usize,
                    height: stack.len().saturating_sub(params as usize),
                    is_loop: false,
                }),
                Op::Loop { params } => labels.push(Label {
                    target: pc,
                    arity: params as usize,
                    height: stack.len().saturating_sub(params as usize),
                    is_loop: true,
                }),
                Op::If {
                    else_,
                    end,
                    params,
                    results,
                } => {
                    let c = pop!() as u32;
                    labels.push(Label {
                        target: end as usize,
                        arity: results as usize,
                        height: stack.len().saturating_sub(params as usize),
                        is_loop: false,
                    });
                    if c == 0 {
                        jump(
                            &mut frames,
                            if else_ != 0 {
                                else_ as usize + 1
                            } else {
                                end as usize
                            },
                        );
                    }
                }
                Op::Else { end } => jump(&mut frames, end as usize),
                Op::End => {
                    let floor = frames.last().map_or(0, |f| f.labels);
                    if labels.len() > floor {
                        labels.pop();
                    } else {
                        self.leave(stack, &mut frames, &mut labels)?;
                    }
                }
                Op::Br(depth) => self.branch(depth, stack, &mut frames, &mut labels)?,
                Op::BrIf(depth) => {
                    if pop!() as u32 != 0 {
                        self.branch(depth, stack, &mut frames, &mut labels)?;
                    }
                }
                Op::BrTable { at, len } => {
                    let i = (pop!() as u32).min(len);
                    let depth = *self.module.funcs[fi]
                        .tables
                        .get((at + i) as usize)
                        .ok_or_else(|| Trap("bad br_table".into()))?;
                    self.branch(depth, stack, &mut frames, &mut labels)?;
                }
                Op::Return => self.leave(stack, &mut frames, &mut labels)?,
                Op::Call(f) => self.enter(f as usize, stack, &mut frames, &mut labels)?,
                Op::CallIndirect { ty } => {
                    let i = pop!() as u32 as usize;
                    let f = self
                        .table
                        .get(i)
                        .copied()
                        .flatten()
                        .ok_or_else(|| Trap("indirect call to a null slot".into()))?;
                    let want = self.module.types.get(ty as usize);
                    let have = self.module.func_type(f as usize);
                    if want.is_none() || want != have {
                        return trap("indirect call type mismatch");
                    }
                    self.enter(f as usize, stack, &mut frames, &mut labels)?;
                }
                Op::Drop => {
                    pop!();
                }
                Op::Select => {
                    let c = pop!() as u32;
                    let b = pop!();
                    let a = pop!();
                    push!(if c != 0 { a } else { b });
                }
                Op::LocalGet(i) => {
                    let v = *stack
                        .get(base + i as usize)
                        .ok_or_else(|| Trap("bad local".into()))?;
                    push!(v);
                }
                Op::LocalSet(i) => {
                    let v = pop!();
                    *stack
                        .get_mut(base + i as usize)
                        .ok_or_else(|| Trap("bad local".into()))? = v;
                }
                Op::LocalTee(i) => {
                    let v = *stack.last().ok_or_else(|| Trap("stack underflow".into()))?;
                    *stack
                        .get_mut(base + i as usize)
                        .ok_or_else(|| Trap("bad local".into()))? = v;
                }
                Op::GlobalGet(i) => {
                    let v = *self
                        .globals
                        .get(i as usize)
                        .ok_or_else(|| Trap("bad global".into()))?;
                    push!(v);
                }
                Op::GlobalSet(i) => {
                    let v = pop!();
                    *self
                        .globals
                        .get_mut(i as usize)
                        .ok_or_else(|| Trap("bad global".into()))? = v;
                }
                Op::Load(kind, offset) => {
                    let addr = pop!() as u32 as usize + offset as usize;
                    let v = self.load(kind, addr)?;
                    push!(v);
                }
                Op::Store(kind, offset) => {
                    let v = pop!();
                    let addr = pop!() as u32 as usize + offset as usize;
                    self.store(kind, addr, v)?;
                }
                Op::MemorySize => push!((self.memory.len() / PAGE) as u64),
                Op::MemoryGrow => {
                    let by = pop!() as u32 as usize;
                    let old = self.memory.len() / PAGE;
                    if old + by > self.max_pages {
                        push!(u32::MAX as u64);
                    } else {
                        self.memory.resize((old + by) * PAGE, 0);
                        push!(old as u64);
                    }
                }
                Op::Const(v) => push!(v),
                Op::Num(code) => numeric(code, stack)?,
                Op::Sat(code) => saturate(code, stack)?,
                Op::MemoryCopy => {
                    let n = pop!() as u32 as usize;
                    let src = pop!() as u32 as usize;
                    let dst = pop!() as u32 as usize;
                    if src + n > self.memory.len() || dst + n > self.memory.len() {
                        return trap("memory.copy out of bounds");
                    }
                    self.memory.copy_within(src..src + n, dst);
                }
                Op::MemoryFill => {
                    let n = pop!() as u32 as usize;
                    let v = pop!() as u8;
                    let dst = pop!() as u32 as usize;
                    self.memory
                        .get_mut(dst..dst + n)
                        .ok_or_else(|| Trap("memory.fill out of bounds".into()))?
                        .fill(v);
                }
            }
            if stack.len() > MAX_STACK {
                return trap("value stack overflow");
            }
        }
        Ok(())
    }

    /// Calls an imported function with its arguments from the stack.
    fn call_host(&mut self, index: usize, stack: &mut Vec<u64>) -> Result<(), Trap> {
        let ty = &self.module.imports[index].2;
        let (params, results) = (ty.params, ty.results);
        let from = stack
            .len()
            .checked_sub(params)
            .ok_or_else(|| Trap("stack underflow".into()))?;
        let args: Vec<u64> = stack.drain(from..).collect();
        let result = (self.hosts[index])(&mut self.memory, &args)?;
        match (results, result) {
            (0, _) => {}
            (1, Some(v)) => stack.push(v),
            _ => return trap("host function returned the wrong results"),
        }
        Ok(())
    }

    /// Enters function `func` (whole index space): an import runs at once,
    /// a defined function gets a frame.
    fn enter(
        &mut self,
        func: usize,
        stack: &mut Vec<u64>,
        frames: &mut Vec<Frame>,
        labels: &mut [Label],
    ) -> Result<(), Trap> {
        let imported = self.module.imports.len();
        let Some(defined) = func.checked_sub(imported) else {
            return self.call_host(func, stack);
        };
        let func = defined;
        let f = self
            .module
            .funcs
            .get(func)
            .ok_or_else(|| Trap("no such function".into()))?;
        if frames.len() >= MAX_FRAMES {
            return trap("call stack exhausted");
        }
        let base = stack
            .len()
            .checked_sub(f.ty.params)
            .ok_or_else(|| Trap("stack underflow".into()))?;
        stack.resize(stack.len() + f.locals, 0);
        frames.push(Frame {
            func,
            pc: 0,
            base,
            labels: labels.len(),
        });
        Ok(())
    }

    /// Returns from the top frame: its results replace its locals.
    fn leave(
        &self,
        stack: &mut Vec<u64>,
        frames: &mut Vec<Frame>,
        labels: &mut Vec<Label>,
    ) -> Result<(), Trap> {
        let frame = frames.pop().ok_or_else(|| Trap("no frame".into()))?;
        let results = self.module.funcs[frame.func].ty.results;
        let from = stack
            .len()
            .checked_sub(results)
            .filter(|&from| from >= frame.base)
            .ok_or_else(|| Trap("stack underflow at return".into()))?;
        stack.drain(frame.base..from);
        labels.truncate(frame.labels);
        Ok(())
    }

    /// A branch `depth` labels out. Past the function's own labels it is
    /// a return.
    fn branch(
        &self,
        depth: u32,
        stack: &mut Vec<u64>,
        frames: &mut Vec<Frame>,
        labels: &mut Vec<Label>,
    ) -> Result<(), Trap> {
        let floor = frames.last().ok_or_else(|| Trap("no frame".into()))?.labels;
        let depth = depth as usize;
        if depth >= labels.len() - floor {
            return self.leave(stack, frames, labels);
        }
        let at = labels.len() - 1 - depth;
        let (target, arity, height, is_loop) = {
            let l = &labels[at];
            (l.target, l.arity, l.height, l.is_loop)
        };
        let from = stack
            .len()
            .checked_sub(arity)
            .ok_or_else(|| Trap("stack underflow".into()))?;
        if from < height {
            return trap("stack underflow at branch");
        }
        stack.drain(height..from);
        labels.truncate(if is_loop { at } else { at + 1 });
        if let Some(f) = frames.last_mut() {
            f.pc = target;
        }
        Ok(())
    }

    fn bytes<const N: usize>(&self, addr: usize) -> Result<[u8; N], Trap> {
        self.memory
            .get(addr..addr + N)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| Trap("memory access out of bounds".into()))
    }

    fn load(&self, kind: Load, addr: usize) -> Result<u64, Trap> {
        Ok(match kind {
            Load::I32 | Load::F32 | Load::I64U32 => u32::from_le_bytes(self.bytes(addr)?) as u64,
            Load::I64 | Load::F64 => u64::from_le_bytes(self.bytes(addr)?),
            Load::I32S8 => self.bytes::<1>(addr)?[0] as i8 as i32 as u32 as u64,
            Load::I32U8 | Load::I64U8 => self.bytes::<1>(addr)?[0] as u64,
            Load::I32S16 => i16::from_le_bytes(self.bytes(addr)?) as i32 as u32 as u64,
            Load::I32U16 | Load::I64U16 => u16::from_le_bytes(self.bytes(addr)?) as u64,
            Load::I64S8 => self.bytes::<1>(addr)?[0] as i8 as i64 as u64,
            Load::I64S16 => i16::from_le_bytes(self.bytes(addr)?) as i64 as u64,
            Load::I64S32 => i32::from_le_bytes(self.bytes(addr)?) as i64 as u64,
        })
    }

    fn store(&mut self, kind: Store, addr: usize, v: u64) -> Result<(), Trap> {
        let bytes: &[u8] = match kind {
            Store::B32 | Store::B64As32 => &(v as u32).to_le_bytes(),
            Store::B64 => &v.to_le_bytes(),
            Store::B8 => &[v as u8],
            Store::B16 => &(v as u16).to_le_bytes(),
        };
        self.memory
            .get_mut(addr..addr + bytes.len())
            .ok_or_else(|| Trap("memory access out of bounds".into()))?
            .copy_from_slice(bytes);
        Ok(())
    }
}

fn numeric(code: u8, stack: &mut Vec<u64>) -> Result<(), Trap> {
    let underflow = || Trap("stack underflow".into());
    macro_rules! un {
        ($f:expr) => {{
            let a = stack.pop().ok_or_else(underflow)?;
            stack.push($f(a));
        }};
    }
    macro_rules! bin {
        ($f:expr) => {{
            let b = stack.pop().ok_or_else(underflow)?;
            let a = stack.pop().ok_or_else(underflow)?;
            stack.push($f(a, b)?);
        }};
    }
    let i32_ = |v: u64| v as u32 as i32;
    let u32_ = |v: u64| v as u32;
    let f32_ = |v: u64| f32::from_bits(v as u32);
    let f64_ = |v: u64| f64::from_bits(v);
    let b = |c: bool| c as u64;
    let w32 = |v: u32| v as u64;
    let wf32 = |v: f32| v.to_bits() as u64;
    let wf64 = |v: f64| v.to_bits();
    let ok = |v: u64| -> Result<u64, Trap> { Ok(v) };
    let div0 = || Trap("integer divide by zero".into());
    let ovf = || Trap("integer overflow".into());
    let trunc = |v: f64, lo: f64, hi: f64| -> Result<f64, Trap> {
        if v.is_nan() {
            return trap("invalid conversion to integer");
        }
        let t = v.trunc();
        if t < lo || t >= hi {
            return trap("integer overflow");
        }
        Ok(t)
    };
    match code {
        0x45 => un!(|a| b(u32_(a) == 0)),
        0x46 => bin!(|a, c| ok(b(u32_(a) == u32_(c)))),
        0x47 => bin!(|a, c| ok(b(u32_(a) != u32_(c)))),
        0x48 => bin!(|a, c| ok(b(i32_(a) < i32_(c)))),
        0x49 => bin!(|a, c| ok(b(u32_(a) < u32_(c)))),
        0x4a => bin!(|a, c| ok(b(i32_(a) > i32_(c)))),
        0x4b => bin!(|a, c| ok(b(u32_(a) > u32_(c)))),
        0x4c => bin!(|a, c| ok(b(i32_(a) <= i32_(c)))),
        0x4d => bin!(|a, c| ok(b(u32_(a) <= u32_(c)))),
        0x4e => bin!(|a, c| ok(b(i32_(a) >= i32_(c)))),
        0x4f => bin!(|a, c| ok(b(u32_(a) >= u32_(c)))),
        0x50 => un!(|a| b(a == 0)),
        0x51 => bin!(|a, c| ok(b(a == c))),
        0x52 => bin!(|a, c| ok(b(a != c))),
        0x53 => bin!(|a, c| ok(b((a as i64) < c as i64))),
        0x54 => bin!(|a, c| ok(b(a < c))),
        0x55 => bin!(|a, c| ok(b(a as i64 > c as i64))),
        0x56 => bin!(|a, c| ok(b(a > c))),
        0x57 => bin!(|a, c| ok(b(a as i64 <= c as i64))),
        0x58 => bin!(|a, c| ok(b(a <= c))),
        0x59 => bin!(|a, c| ok(b(a as i64 >= c as i64))),
        0x5a => bin!(|a, c| ok(b(a >= c))),
        0x5b => bin!(|a, c| ok(b(f32_(a) == f32_(c)))),
        0x5c => bin!(|a, c| ok(b(f32_(a) != f32_(c)))),
        0x5d => bin!(|a, c| ok(b(f32_(a) < f32_(c)))),
        0x5e => bin!(|a, c| ok(b(f32_(a) > f32_(c)))),
        0x5f => bin!(|a, c| ok(b(f32_(a) <= f32_(c)))),
        0x60 => bin!(|a, c| ok(b(f32_(a) >= f32_(c)))),
        0x61 => bin!(|a, c| ok(b(f64_(a) == f64_(c)))),
        0x62 => bin!(|a, c| ok(b(f64_(a) != f64_(c)))),
        0x63 => bin!(|a, c| ok(b(f64_(a) < f64_(c)))),
        0x64 => bin!(|a, c| ok(b(f64_(a) > f64_(c)))),
        0x65 => bin!(|a, c| ok(b(f64_(a) <= f64_(c)))),
        0x66 => bin!(|a, c| ok(b(f64_(a) >= f64_(c)))),
        0x67 => un!(|a| w32(u32_(a).leading_zeros())),
        0x68 => un!(|a| w32(u32_(a).trailing_zeros())),
        0x69 => un!(|a| w32(u32_(a).count_ones())),
        0x6a => bin!(|a, c| ok(w32(u32_(a).wrapping_add(u32_(c))))),
        0x6b => bin!(|a, c| ok(w32(u32_(a).wrapping_sub(u32_(c))))),
        0x6c => bin!(|a, c| ok(w32(u32_(a).wrapping_mul(u32_(c))))),
        0x6d => bin!(|a, c| {
            if u32_(c) == 0 {
                return Err(div0());
            }
            i32_(a)
                .checked_div(i32_(c))
                .map(|v| w32(v as u32))
                .ok_or_else(ovf)
        }),
        0x6e => bin!(|a, c| u32_(a).checked_div(u32_(c)).map(w32).ok_or_else(div0)),
        0x6f => bin!(|a, c| {
            if u32_(c) == 0 {
                return Err(div0());
            }
            Ok(w32(i32_(a).wrapping_rem(i32_(c)) as u32))
        }),
        0x70 => bin!(|a, c| u32_(a).checked_rem(u32_(c)).map(w32).ok_or_else(div0)),
        0x71 => bin!(|a, c| ok(w32(u32_(a) & u32_(c)))),
        0x72 => bin!(|a, c| ok(w32(u32_(a) | u32_(c)))),
        0x73 => bin!(|a, c| ok(w32(u32_(a) ^ u32_(c)))),
        0x74 => bin!(|a, c| ok(w32(u32_(a).wrapping_shl(u32_(c))))),
        0x75 => bin!(|a, c| ok(w32(i32_(a).wrapping_shr(u32_(c)) as u32))),
        0x76 => bin!(|a, c| ok(w32(u32_(a).wrapping_shr(u32_(c))))),
        0x77 => bin!(|a, c| ok(w32(u32_(a).rotate_left(u32_(c) % 32)))),
        0x78 => bin!(|a, c| ok(w32(u32_(a).rotate_right(u32_(c) % 32)))),
        0x79 => un!(|a: u64| a.leading_zeros() as u64),
        0x7a => un!(|a: u64| a.trailing_zeros() as u64),
        0x7b => un!(|a: u64| a.count_ones() as u64),
        0x7c => bin!(|a: u64, c| ok(a.wrapping_add(c))),
        0x7d => bin!(|a: u64, c| ok(a.wrapping_sub(c))),
        0x7e => bin!(|a: u64, c| ok(a.wrapping_mul(c))),
        0x7f => bin!(|a: u64, c: u64| {
            if c == 0 {
                return Err(div0());
            }
            (a as i64)
                .checked_div(c as i64)
                .map(|v| v as u64)
                .ok_or_else(ovf)
        }),
        0x80 => bin!(|a: u64, c| a.checked_div(c).ok_or_else(div0)),
        0x81 => bin!(|a: u64, c: u64| {
            if c == 0 {
                return Err(div0());
            }
            Ok((a as i64).wrapping_rem(c as i64) as u64)
        }),
        0x82 => bin!(|a: u64, c| a.checked_rem(c).ok_or_else(div0)),
        0x83 => bin!(|a: u64, c| ok(a & c)),
        0x84 => bin!(|a: u64, c| ok(a | c)),
        0x85 => bin!(|a: u64, c| ok(a ^ c)),
        0x86 => bin!(|a: u64, c| ok(a.wrapping_shl(c as u32))),
        0x87 => bin!(|a: u64, c| ok((a as i64).wrapping_shr(c as u32) as u64)),
        0x88 => bin!(|a: u64, c| ok(a.wrapping_shr(c as u32))),
        0x89 => bin!(|a: u64, c| ok(a.rotate_left((c % 64) as u32))),
        0x8a => bin!(|a: u64, c| ok(a.rotate_right((c % 64) as u32))),
        0x8b => un!(|a| wf32(f32_(a).abs())),
        0x8c => un!(|a| wf32(-f32_(a))),
        0x8d => un!(|a| wf32(f32_(a).ceil())),
        0x8e => un!(|a| wf32(f32_(a).floor())),
        0x8f => un!(|a| wf32(f32_(a).trunc())),
        0x90 => un!(|a| wf32(f32_(a).round_ties_even())),
        0x91 => un!(|a| wf32(f32_(a).sqrt())),
        0x92 => bin!(|a, c| ok(wf32(f32_(a) + f32_(c)))),
        0x93 => bin!(|a, c| ok(wf32(f32_(a) - f32_(c)))),
        0x94 => bin!(|a, c| ok(wf32(f32_(a) * f32_(c)))),
        0x95 => bin!(|a, c| ok(wf32(f32_(a) / f32_(c)))),
        0x96 => bin!(|a, c| ok(wf32(fmin32(f32_(a), f32_(c))))),
        0x97 => bin!(|a, c| ok(wf32(fmax32(f32_(a), f32_(c))))),
        0x98 => bin!(|a, c| ok(wf32(f32_(a).copysign(f32_(c))))),
        0x99 => un!(|a| wf64(f64_(a).abs())),
        0x9a => un!(|a| wf64(-f64_(a))),
        0x9b => un!(|a| wf64(f64_(a).ceil())),
        0x9c => un!(|a| wf64(f64_(a).floor())),
        0x9d => un!(|a| wf64(f64_(a).trunc())),
        0x9e => un!(|a| wf64(f64_(a).round_ties_even())),
        0x9f => un!(|a| wf64(f64_(a).sqrt())),
        0xa0 => bin!(|a, c| ok(wf64(f64_(a) + f64_(c)))),
        0xa1 => bin!(|a, c| ok(wf64(f64_(a) - f64_(c)))),
        0xa2 => bin!(|a, c| ok(wf64(f64_(a) * f64_(c)))),
        0xa3 => bin!(|a, c| ok(wf64(f64_(a) / f64_(c)))),
        0xa4 => bin!(|a, c| ok(wf64(fmin64(f64_(a), f64_(c))))),
        0xa5 => bin!(|a, c| ok(wf64(fmax64(f64_(a), f64_(c))))),
        0xa6 => bin!(|a, c| ok(wf64(f64_(a).copysign(f64_(c))))),
        0xa7 => un!(|a: u64| a as u32 as u64),
        0xa8 | 0xaa => {
            let a = stack.pop().ok_or_else(underflow)?;
            let v = if code == 0xa8 {
                f32_(a) as f64
            } else {
                f64_(a)
            };
            stack.push(trunc(v, -2147483648.0, 2147483648.0)? as i32 as u32 as u64);
        }
        0xa9 | 0xab => {
            let a = stack.pop().ok_or_else(underflow)?;
            let v = if code == 0xa9 {
                f32_(a) as f64
            } else {
                f64_(a)
            };
            stack.push(trunc(v, 0.0, 4294967296.0)? as u32 as u64);
        }
        0xac => un!(|a| a as u32 as i32 as i64 as u64),
        0xad => un!(|a| a as u32 as u64),
        0xae | 0xb0 => {
            let a = stack.pop().ok_or_else(underflow)?;
            let v = if code == 0xae {
                f32_(a) as f64
            } else {
                f64_(a)
            };
            stack.push(trunc(v, -9223372036854775808.0, 9223372036854775808.0)? as i64 as u64);
        }
        0xaf | 0xb1 => {
            let a = stack.pop().ok_or_else(underflow)?;
            let v = if code == 0xaf {
                f32_(a) as f64
            } else {
                f64_(a)
            };
            stack.push(trunc(v, 0.0, 18446744073709551616.0)? as u64);
        }
        0xb2 => un!(|a| wf32(i32_(a) as f32)),
        0xb3 => un!(|a| wf32(u32_(a) as f32)),
        0xb4 => un!(|a| wf32(a as i64 as f32)),
        0xb5 => un!(|a| wf32(a as f32)),
        0xb6 => un!(|a| wf32(f64_(a) as f32)),
        0xb7 => un!(|a| wf64(i32_(a) as f64)),
        0xb8 => un!(|a| wf64(u32_(a) as f64)),
        0xb9 => un!(|a| wf64(a as i64 as f64)),
        0xba => un!(|a| wf64(a as f64)),
        0xbb => un!(|a| wf64(f32_(a) as f64)),
        0xbc => un!(|a| a as u32 as u64),
        0xbd => un!(|a| a),
        0xbe => un!(|a| a as u32 as u64),
        0xbf => un!(|a| a),
        0xc0 => un!(|a| a as u8 as i8 as i32 as u32 as u64),
        0xc1 => un!(|a| a as u16 as i16 as i32 as u32 as u64),
        0xc2 => un!(|a| a as u8 as i8 as i64 as u64),
        0xc3 => un!(|a| a as u16 as i16 as i64 as u64),
        0xc4 => un!(|a| a as u32 as i32 as i64 as u64),
        _ => return trap(format!("bad numeric opcode 0x{code:02x}")),
    }
    Ok(())
}

/// The saturating truncations: Rust's `as` already saturates and maps NaN
/// to zero, which is what the spec asks for.
fn saturate(code: u8, stack: &mut Vec<u64>) -> Result<(), Trap> {
    let a = stack.pop().ok_or_else(|| Trap("stack underflow".into()))?;
    let f = if matches!(code, 0 | 1 | 4 | 5) {
        f32::from_bits(a as u32) as f64
    } else {
        f64::from_bits(a)
    };
    stack.push(match code {
        0 | 2 => (f as i32) as u32 as u64,
        1 | 3 => (f as u32) as u64,
        4 | 6 => (f as i64) as u64,
        _ => f as u64,
    });
    Ok(())
}

fn fmin32(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        f32::NAN
    } else if a == b {
        if a.is_sign_negative() { a } else { b }
    } else {
        a.min(b)
    }
}
fn fmax32(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        f32::NAN
    } else if a == b {
        if a.is_sign_positive() { a } else { b }
    } else {
        a.max(b)
    }
}
fn fmin64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == b {
        if a.is_sign_negative() { a } else { b }
    } else {
        a.min(b)
    }
}
fn fmax64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == b {
        if a.is_sign_positive() { a } else { b }
    } else {
        a.max(b)
    }
}
