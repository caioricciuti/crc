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
pub const HOST_FUNCTIONS: &[(&str, (usize, usize))] = &[("log", (2, 0))];

/// Memory an extension may grow to: 64 MB.
const MAX_PAGES: usize = 1024;
/// Instructions one call may run, about 1.5 s at the interpreter's speed.
const FUEL: u64 = 500_000_000;
/// And the wall clock, whatever the fuel says.
const DEADLINE: Duration = Duration::from_secs(2);
/// The largest preview page crc shows.
pub const MAX_HTML: usize = 8 << 20;
/// The most text a command is given. Its JSON, a copy in the module's
/// memory and the answer must fit in `MAX_PAGES`.
pub const MAX_REQUEST: usize = 8 << 20;
/// The largest replacement crc applies.
pub const MAX_REPLACE: usize = 16 << 20;
/// How a command that ran out of instructions or time ends its message.
const OVER_BUDGET: &str = "took too long and was stopped";
/// Lines of log kept per extension.
const LOG_LINES: usize = 200;

/// What a command is asked.
#[derive(Clone, Debug)]
pub struct Request {
    pub command: String,
    pub text: Text,
    pub selection: bool,
    pub language: String,
}

/// The text a command is given: a range of a document, cut on the
/// extension thread. The main thread hands over the rope, a pointer's
/// worth, and copies nothing however large the document.
#[derive(Clone)]
pub struct Text {
    rope: crate::text::rope::Rope,
    range: std::ops::Range<usize>,
}

impl Text {
    pub fn of(rope: &crate::text::rope::Rope, range: std::ops::Range<usize>) -> Text {
        Text {
            rope: rope.clone(),
            range,
        }
    }

    pub fn len(&self) -> usize {
        self.range.len()
    }

    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }
}

impl From<&str> for Text {
    fn from(text: &str) -> Text {
        let rope = crate::text::rope::Rope::from_text(text);
        let range = 0..rope.len_bytes();
        Text { rope, range }
    }
}

impl std::fmt::Debug for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Text({:?})", self.range)
    }
}

/// What it answered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Response {
    pub replace: Option<String>,
    pub message: Option<String>,
    /// A page for the preview pane.
    pub html: Option<String>,
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
    /// Its allocator stopped part-way through a free: whatever it holds is
    /// suspect, so the next command starts from a fresh instance.
    pub spoiled: bool,
    /// What one command may spend: `FUEL` instructions and `DEADLINE` of
    /// wall clock. A long fuzz run lowers both, to try more cases.
    pub fuel: u64,
    pub deadline: Duration,
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
                    let [ptr, len] = args else {
                        return Err(Trap("log takes a pointer and a length".into()));
                    };
                    let (ptr, len) = (*ptr as u32 as usize, *len as u32 as usize);
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
        spoiled: false,
        fuel: FUEL,
        deadline: DEADLINE,
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
        if request.text.len() > MAX_REQUEST {
            return Err(format!(
                "the text is over {} MB, more than an extension is given",
                MAX_REQUEST >> 20
            ));
        }
        let text = request
            .text
            .rope
            .slice_to_string(request.text.range.clone());
        let input = crate::json::compact(&object([
            ("api", crate::json::number(manifest::API)),
            ("command", string(&request.command)),
            ("text", string(&text)),
            ("selection", Value::Bool(request.selection)),
            ("language", string(&request.language)),
        ]));
        self.instance.fuel = self.fuel;
        self.instance.deadline = Some(Instant::now() + self.deadline);
        let trap = |t: Trap| match t.0.as_str() {
            "out of fuel" | "took too long" => format!("{name} {OVER_BUDGET}"),
            other => format!("{name} failed: {other}"),
        };
        let len = input.len() as u64;
        let first = |values: Vec<u64>| {
            values
                .first()
                .copied()
                .ok_or_else(|| format!("{name} returned nothing"))
        };
        let ptr = first(self.instance.call(self.alloc, &[len]).map_err(trap)?)? as usize;
        self.instance
            .memory
            .get_mut(ptr..ptr.saturating_add(input.len()))
            .ok_or_else(|| format!("{name} gave memory it does not have"))?
            .copy_from_slice(input.as_bytes());
        let packed = first(self.instance.call(func, &[ptr as u64, len]).map_err(trap)?)?;
        let (out, out_len) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
        let bytes = self
            .instance
            .memory
            .get(out..out.saturating_add(out_len))
            .ok_or_else(|| format!("{name} answered outside its memory"))?
            .to_vec();
        // Its buffer is its own to free, on a budget of its own rather than
        // what the command left over. A free that stops part-way leaves the
        // allocator half-updated: the answer stands, the instance does not.
        self.instance.fuel = self.fuel / 10;
        self.instance.deadline = Some(Instant::now() + self.deadline / 4);
        if self
            .instance
            .call(self.free, &[out as u64, out_len as u64])
            .is_err()
        {
            self.spoiled = true;
        }
        let text = String::from_utf8(bytes).map_err(|_| format!("{name} answered in bad UTF-8"))?;
        let value = crate::json::parse(&text).map_err(|_| format!("{name} answered badly"))?;
        let field = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        let mut response = Response {
            replace: field("replace"),
            message: field("message").map(|m| m.chars().take(200).collect()),
            html: field("html"),
        };
        if response.replace.is_some() && !self.manifest.may_replace(request.selection) {
            response.replace = None;
            response.message = Some(format!("{name} may not change the text"));
        }
        if response.html.is_some() && !self.manifest.may_preview() {
            response.html = None;
            response.message = Some(format!("{name} may not show a preview"));
        }
        if response
            .replace
            .as_ref()
            .is_some_and(|r| r.len() > MAX_REPLACE)
        {
            response.replace = None;
            response.message = Some(format!(
                "{name} answered with over {} MB; nothing was changed",
                MAX_REPLACE >> 20
            ));
        }
        if response.html.as_ref().is_some_and(|h| h.len() > MAX_HTML) {
            response.html = None;
            response.message = Some(format!(
                "{name} made a page over {} MB; it was not shown",
                MAX_HTML >> 20
            ));
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
    /// The module's digest at install. Bytes that differ are not run.
    pub sha256: String,
    /// What is installed, as a number that changes with it: a new one
    /// drops every loaded instance, so a reinstall is picked up.
    pub generation: u64,
    pub request: Request,
}

pub struct Done {
    pub tag: u64,
    pub id: String,
    pub result: Result<Response, String>,
    pub log: Vec<String>,
    /// It failed by running out of instructions or time.
    pub over_budget: bool,
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
            let mut generation = 0;
            for job in inbox {
                if job.generation != generation {
                    loaded.clear();
                    generation = job.generation;
                }
                let key = (job.manifest.id.clone(), job.manifest.version.clone());
                let id = job.manifest.id.clone();
                if !loaded.contains_key(&key) {
                    let started = std::fs::read(&job.wasm)
                        .map_err(|e| format!("could not read {}: {e}", job.wasm.display()))
                        .and_then(|bytes| {
                            // Checked on the bytes that run, not only when
                            // the list was read: a file swapped in between
                            // is caught here.
                            if super::store::digest(&bytes) != job.sha256 {
                                return Err(format!(
                                    "{} changed on disk since it was installed; reinstall it",
                                    job.manifest.name
                                ));
                            }
                            load(job.manifest.clone(), &bytes)
                        });
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
                                over_budget: false,
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
                let over_budget = result.as_ref().is_err_and(|e| e.ends_with(OVER_BUDGET));
                // A trap may leave the instance half-way through anything;
                // the next call starts from a fresh one.
                let log = extension.log.lock().map(|l| l.clone()).unwrap_or_default();
                if result.is_err() || extension.spoiled {
                    loaded.remove(&key);
                }
                let _ = outbox.send(Done {
                    tag: job.tag,
                    id,
                    result,
                    log,
                    over_budget,
                });
                wake();
            }
        })
        .expect("the extension thread starts");
    (jobs, done)
}
