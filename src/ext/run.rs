//! Running extension commands: loading a module against its manifest, one
//! call with a budget, and the worker thread every call runs on.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::manifest::{self, Manifest};
use super::wasm::{self, Host, Instance, Trap};
use crate::json::{Value, object, string};

/// The host functions crc provides, all in the import module `crc`. `log`
/// needs no capability; later ones are linked only when granted.
pub const HOST_FUNCTIONS: &[&str] = &["log"];

/// Memory an extension may grow to: 64 MB.
const MAX_PAGES: usize = 1024;
/// Instructions one call may run, about 1.5 s at the interpreter's speed.
const FUEL: u64 = 500_000_000;
/// And the wall clock, whatever the fuel says.
const DEADLINE: Duration = Duration::from_secs(2);
/// Lines of log kept per extension.
const LOG_LINES: usize = 200;

/// What a command is asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub command: String,
    pub text: String,
    pub selection: bool,
    pub language: String,
}

/// What it answered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Response {
    pub replace: Option<String>,
    pub message: Option<String>,
}

type Log = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// An extension ready to run.
pub struct Loaded {
    pub manifest: Manifest,
    instance: Instance,
    alloc: u32,
    free: u32,
    commands: HashMap<String, u32>,
    pub log: Log,
}

/// Loads `wasm` as the module `manifest` describes, checking one against
/// the other.
pub fn load(manifest: Manifest, wasm: &[u8]) -> Result<Loaded, String> {
    let module = wasm::Module::parse(wasm).map_err(|t| format!("not a valid module: {t}"))?;
    manifest::check_module(&manifest, &module)?;
    let log: Log = Default::default();
    let sink = log.clone();
    let instance = Instance::new(module, MAX_PAGES, |module, name| -> Option<Host> {
        match (module, name) {
            ("crc", "log") => {
                let sink = sink.clone();
                Some(Box::new(move |memory: &mut [u8], args: &[u64]| {
                    let (ptr, len) = (args[0] as u32 as usize, args[1] as u32 as usize);
                    let bytes = memory
                        .get(ptr..ptr.saturating_add(len))
                        .ok_or_else(|| Trap("log outside memory".into()))?;
                    let line: String = String::from_utf8_lossy(bytes).chars().take(1000).collect();
                    let mut log = sink.lock().unwrap_or_else(|e| e.into_inner());
                    if log.len() >= LOG_LINES {
                        log.remove(0);
                    }
                    log.push(line);
                    Ok(None)
                }))
            }
            _ => None,
        }
    })
    .map_err(|t| format!("could not start: {t}"))?;
    let func = |name: &str| {
        instance
            .func(name)
            .ok_or_else(|| format!("{name} is not a function"))
    };
    let alloc = func("crc_alloc")?;
    let free = func("crc_free")?;
    let mut commands = HashMap::new();
    for command in &manifest.commands {
        commands.insert(command.id.clone(), func(&command.id)?);
    }
    Ok(Loaded {
        manifest,
        instance,
        alloc,
        free,
        commands,
        log,
    })
}

impl Loaded {
    /// The module's memory now, in bytes.
    pub fn memory_len(&self) -> usize {
        self.instance.memory.len()
    }

    /// Runs one command. The text goes in only if the manifest may read it;
    /// a replacement comes back only if it may replace it.
    pub fn run(&mut self, request: &Request) -> Result<Response, String> {
        let name = &self.manifest.name;
        if !self.manifest.may_read(request.selection) {
            return Err(format!(
                "{name} may not read {}",
                if request.selection {
                    "the selection"
                } else {
                    "the whole document"
                }
            ));
        }
        let func = *self
            .commands
            .get(&request.command)
            .ok_or_else(|| format!("{name} has no command {}", request.command))?;
        let input = crate::json::compact(&object([
            ("api", crate::json::number(manifest::API)),
            ("command", string(&request.command)),
            ("text", string(&request.text)),
            ("selection", Value::Bool(request.selection)),
            ("language", string(&request.language)),
        ]));
        self.instance.fuel = FUEL;
        self.instance.deadline = Some(Instant::now() + DEADLINE);
        let trap = |t: Trap| match t.0.as_str() {
            "out of fuel" | "took too long" => format!("{name} took too long and was stopped"),
            other => format!("{name} failed: {other}"),
        };
        let len = input.len() as u64;
        let ptr = self.instance.call(self.alloc, &[len]).map_err(trap)?[0] as usize;
        self.instance
            .memory
            .get_mut(ptr..ptr + input.len())
            .ok_or_else(|| format!("{name} gave memory it does not have"))?
            .copy_from_slice(input.as_bytes());
        let packed = self.instance.call(func, &[ptr as u64, len]).map_err(trap)?[0];
        let (out, out_len) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
        let bytes = self
            .instance
            .memory
            .get(out..out.saturating_add(out_len))
            .ok_or_else(|| format!("{name} answered outside its memory"))?
            .to_vec();
        // Its buffer is its own to free; a failure here changes nothing.
        let _ = self.instance.call(self.free, &[out as u64, out_len as u64]);
        let text = String::from_utf8(bytes).map_err(|_| format!("{name} answered in bad UTF-8"))?;
        let value = crate::json::parse(&text).map_err(|_| format!("{name} answered badly"))?;
        let field = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        let mut response = Response {
            replace: field("replace"),
            message: field("message").map(|m| m.chars().take(200).collect()),
        };
        if response.replace.is_some() && !self.manifest.may_replace(request.selection) {
            response.replace = None;
            response.message = Some(format!("{name} may not change the text"));
        }
        Ok(response)
    }
}

/// A job for the worker: which extension, what to run, and a tag the
/// answer carries back.
pub struct Job {
    pub tag: u64,
    pub manifest: Manifest,
    pub wasm: std::path::PathBuf,
    pub request: Request,
}

pub struct Done {
    pub tag: u64,
    pub id: String,
    pub result: Result<Response, String>,
    pub log: Vec<String>,
}

/// The thread every extension runs on. Instances stay loaded between calls,
/// keyed by id and version; `wake` is called after each answer.
pub fn spawn(wake: Box<dyn Fn() + Send>) -> (mpsc::Sender<Job>, mpsc::Receiver<Done>) {
    let (jobs, inbox) = mpsc::channel::<Job>();
    let (outbox, done) = mpsc::channel();
    std::thread::Builder::new()
        .name("crc-extensions".into())
        .spawn(move || {
            let mut loaded: HashMap<(String, String), Loaded> = HashMap::new();
            for job in inbox {
                let key = (job.manifest.id.clone(), job.manifest.version.clone());
                let id = job.manifest.id.clone();
                if !loaded.contains_key(&key) {
                    let started = std::fs::read(&job.wasm)
                        .map_err(|e| format!("could not read {}: {e}", job.wasm.display()))
                        .and_then(|bytes| load(job.manifest.clone(), &bytes));
                    match started {
                        Ok(l) => {
                            loaded.insert(key.clone(), l);
                        }
                        Err(error) => {
                            let _ = outbox.send(Done {
                                tag: job.tag,
                                id,
                                result: Err(error),
                                log: Vec::new(),
                            });
                            wake();
                            continue;
                        }
                    }
                }
                let Some(extension) = loaded.get_mut(&key) else {
                    continue;
                };
                let result = extension.run(&job.request);
                // A trap may leave the instance half-way through anything;
                // the next call starts from a fresh one.
                let log = extension.log.lock().map(|l| l.clone()).unwrap_or_default();
                if result.is_err() {
                    loaded.remove(&key);
                }
                let _ = outbox.send(Done {
                    tag: job.tag,
                    id,
                    result,
                    log,
                });
                wake();
            }
        })
        .expect("the extension thread starts");
    (jobs, done)
}
