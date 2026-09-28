//! The preview pane: a WKWebView showing a page an extension made.
//!
//! WebKit is a system framework (linked in build.rs), driven by messages the
//! way Quick Look is, so there is no crate for it. What the page can do is
//! settled here, not by the extension:
//!
//! - no script: `allowsContentJavaScript` is off. crc's own calls through
//!   `evaluateJavaScript` still run, which is how an update replaces the
//!   page's contents without moving its scroll position;
//! - a content rule list blocks every load except files inside the
//!   document's folder, `data:` URLs and the page itself, so nothing
//!   reaches the network and nothing outside that folder is read, and it
//!   blocks every navigation, so a link or a meta refresh goes nowhere;
//! - a non-persistent data store: no cookies, cache or storage on disk.
//!
//! The first page is written to crc's cache folder and loaded as a file.
//! WebKit's web process reads files only where the loading app grants it,
//! and `loadFileURL` is how that grant is made: to the nearest folder
//! holding both the page and the document. The rule list is what narrows
//! it to the document's folder. `<base>`, added to every page, points
//! relative paths there.
//!
//! The rule list compiles asynchronously; until it is ready there is no
//! view, so no page is ever shown without it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::{MainThreadMarker, msg_send};
use objc2_foundation::{NSRect, NSString, NSURL};

/// Where the rule list for a folder and page stands.
enum Rules {
    Compiling,
    Ready(Retained<AnyObject>),
    Failed,
}

thread_local! {
    /// By the identifier each list is compiled under.
    static RULES: RefCell<HashMap<String, Rules>> = RefCell::new(HashMap::new());
    /// The last answer to [`WebPreview::probe`], for the self-test.
    static PROBE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The rule list for a page at `page` showing a document in `folder`,
/// compiling it on first ask. `None` while it compiles; `Err` when WebKit
/// refused it (a path its URL filter cannot express), and then there is no
/// preview.
pub fn rules(folder: Option<&Path>, page: &Path) -> Option<Result<Retained<AnyObject>, ()>> {
    let _ = MainThreadMarker::new()?;
    let json = rule_list(
        folder.map(|f| file_url(f, true)).as_deref(),
        &file_url(page, false),
    );
    let id = format!(
        "crc-preview-{:016x}",
        crate::platform::fnv1a(json.as_bytes())
    );
    let current = RULES.with(|r| match r.borrow().get(&id) {
        Some(Rules::Ready(list)) => Some(Some(Ok(list.clone()))),
        Some(Rules::Failed) => Some(Some(Err(()))),
        Some(Rules::Compiling) => Some(None),
        None => None,
    });
    if let Some(state) = current {
        return state;
    }
    let Some(store_class) = AnyClass::get(c"WKContentRuleListStore") else {
        return Some(Err(()));
    };
    RULES.with(|r| r.borrow_mut().insert(id.clone(), Rules::Compiling));
    PENDING.with(|p| p.borrow_mut().push(id.clone()));
    let (json, id) = (NSString::from_str(&json), NSString::from_str(&id));
    unsafe {
        let store: *mut AnyObject = msg_send![store_class, defaultStore];
        let _: () = msg_send![store,
            compileContentRuleListForIdentifier: &*id,
            encodedContentRuleList: &*json,
            completionHandler: compiled_block()];
    }
    None
}

thread_local! {
    /// Identifiers sent to WebKit, oldest first; its answers come back in
    /// the order asked.
    static PENDING: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// A path as the file URL WebKit matches loads against.
fn file_url(path: &Path, directory: bool) -> String {
    NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&path.to_string_lossy()), directory)
        .absoluteString()
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Block everything, then let back in the folder's files and `data:`, then
/// block every navigation, then let in the page itself: it is crc's own
/// load, and the only file outside the folder.
fn rule_list(folder_url: Option<&str>, page_url: &str) -> String {
    let rule = |filter: &str, action: &str, extra: &str| {
        format!(
            r#"{{"trigger":{{"url-filter":{}{extra}}},"action":{{"type":"{action}"}}}}"#,
            crate::json::compact(&crate::json::string(filter))
        )
    };
    let mut rules = vec![rule(".*", "block", "")];
    if let Some(url) = folder_url {
        rules.push(rule(
            &format!("^{}", regex_escape(url)),
            "ignore-previous-rules",
            r#","url-filter-is-case-sensitive":true"#,
        ));
    }
    rules.push(rule("^data:", "ignore-previous-rules", ""));
    rules.push(rule(".*", "block", r#","resource-type":["document"]"#));
    rules.push(rule(
        &format!("^{}$", regex_escape(page_url)),
        "ignore-previous-rules",
        r#","url-filter-is-case-sensitive":true"#,
    ));
    format!("[{}]", rules.join(","))
}

/// Where the preview of document `id` is written: crc's cache folder,
/// named for this process so two copies of crc never share one.
pub fn page_path(id: u64) -> Option<PathBuf> {
    Some(crate::platform::caches()?.join(format!("preview-{}-{id}.html", std::process::id())))
}

/// The deepest folder holding both `a` and `b`.
fn common_folder(a: &Path, b: &Path) -> PathBuf {
    let mut common = PathBuf::new();
    for (x, y) in a.components().zip(b.components()) {
        if x != y {
            break;
        }
        common.push(x);
    }
    common
}

/// `page` with a `<base>` for `folder` first in its head, so relative paths
/// in it resolve there and not beside the file crc wrote.
fn with_base(page: &str, folder: Option<&Path>) -> String {
    let Some(folder) = folder else {
        return page.to_string();
    };
    let base = format!(
        "<base href=\"{}\">",
        file_url(folder, true)
            .replace('&', "&amp;")
            .replace('"', "&quot;")
    );
    let head = page
        .to_ascii_lowercase()
        .find("<head>")
        .map(|at| at + "<head>".len());
    match head {
        Some(at) => format!("{}{base}{}", &page[..at], &page[at..]),
        None => format!("{base}{page}"),
    }
}

/// A URL as a literal in WebKit's URL-filter syntax.
fn regex_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if ".*+?^$()[]{}|\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A page on screen.
pub struct WebPreview {
    view: Retained<AnyObject>,
    folder: Option<PathBuf>,
    /// The file the first page is written to.
    page: PathBuf,
    /// The first page is loading or loaded; later pages replace its
    /// contents.
    loaded: bool,
    /// The first load finished. Until then the view is hidden, so the pane
    /// never flashes white before the page's own background.
    shown: bool,
    hidden: bool,
    /// When the first page was handed over, and whether its load has been
    /// seen running: WebKit starts loading on its own time, so right after
    /// `loadFileURL` it is not loading yet, and that is not "finished".
    load_started: Option<std::time::Instant>,
    seen_loading: bool,
    /// The appearance last given, so the page's `prefers-color-scheme`
    /// follows crc's theme and not only the system's.
    dark: Option<bool>,
}

impl WebPreview {
    /// A hidden web view under `rules`, for a document in `folder`, whose
    /// page goes to `page`.
    pub fn new(
        frame: NSRect,
        rules: &AnyObject,
        folder: Option<&Path>,
        page: &Path,
    ) -> Option<WebPreview> {
        let config_class = AnyClass::get(c"WKWebViewConfiguration")?;
        let store_class = AnyClass::get(c"WKWebsiteDataStore")?;
        let view_class = AnyClass::get(c"WKWebView")?;
        unsafe {
            let config: *mut AnyObject = msg_send![config_class, new];
            let config = Retained::from_raw(config)?;
            let preferences: *mut AnyObject = msg_send![&*config, defaultWebpagePreferences];
            let _: () = msg_send![preferences, setAllowsContentJavaScript: Bool::NO];
            let store: *mut AnyObject = msg_send![store_class, nonPersistentDataStore];
            let _: () = msg_send![&*config, setWebsiteDataStore: store];
            let content: *mut AnyObject = msg_send![&*config, userContentController];
            let _: () = msg_send![content, addContentRuleList: rules];
            let view: *mut AnyObject = msg_send![view_class, alloc];
            let view: *mut AnyObject =
                msg_send![view, initWithFrame: frame, configuration: &*config];
            let view = Retained::from_raw(view)?;
            let _: () = msg_send![&*view, setAllowsLinkPreview: Bool::NO];
            let _: () = msg_send![&*view, setHidden: Bool::YES];
            Some(WebPreview {
                view,
                folder: folder.map(Path::to_path_buf),
                page: page.to_path_buf(),
                loaded: false,
                shown: false,
                hidden: true,
                load_started: None,
                seen_loading: false,
                dark: None,
            })
        }
    }

    pub fn view(&self) -> &AnyObject {
        &self.view
    }

    pub fn set_frame(&self, frame: NSRect) {
        unsafe {
            let _: () = msg_send![&*self.view, setFrame: frame];
        }
    }

    /// Light or dark as crc is drawn, whatever the system says.
    pub fn set_dark(&mut self, dark: bool) {
        if self.dark == Some(dark) {
            return;
        }
        use objc2_app_kit::{NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua};
        // AppKit's documented constants, never written.
        let name = unsafe {
            if dark {
                NSAppearanceNameDarkAqua
            } else {
                NSAppearanceNameAqua
            }
        };
        if let Some(appearance) = NSAppearance::appearanceNamed(name) {
            unsafe {
                let _: () = msg_send![&*self.view, setAppearance: &*appearance];
            }
            self.dark = Some(dark);
        }
    }

    fn loading(&self) -> bool {
        let loading: Bool = unsafe { msg_send![&*self.view, isLoading] };
        loading.as_bool()
    }

    /// Shows `page`. `Ok(false)` when the view is still loading the last
    /// one: keep the page and offer it again next frame.
    pub fn show(&mut self, page: &str) -> Result<bool, String> {
        let page = with_base(page, self.folder.as_deref());
        if !self.loaded {
            write_private(&self.page, page.as_bytes())
                .map_err(|e| format!("could not write the preview: {e}"))?;
            let url = NSURL::fileURLWithPath(&NSString::from_str(&self.page.to_string_lossy()));
            let parent = self.page.parent().unwrap_or(Path::new("/"));
            let access = match &self.folder {
                Some(folder) => common_folder(parent, folder),
                None => self.page.clone(),
            };
            let access = NSURL::fileURLWithPath(&NSString::from_str(&access.to_string_lossy()));
            unsafe {
                let _: *mut AnyObject = msg_send![&*self.view,
                    loadFileURL: &*url, allowingReadAccessToURL: &*access];
            }
            self.loaded = true;
            self.load_started = Some(std::time::Instant::now());
            return Ok(true);
        }
        if self.loading() {
            return Ok(false);
        }
        // The new page's head and body into the old document: the scroll
        // position stays where the reader left it. DOMParser runs nothing
        // it parses.
        let script = format!(
            "(function(h){{var d=new DOMParser().parseFromString(h,'text/html');\
             document.head.innerHTML=d.head.innerHTML;\
             document.body.innerHTML=d.body.innerHTML;}})({})",
            crate::json::compact(&crate::json::string(&page))
        );
        self.evaluate(&script, None);
        Ok(true)
    }

    /// Shows the view once its first load has finished, unless `veiled`:
    /// something of crc's is drawn where it sits.
    pub fn reveal_when_loaded(&mut self, veiled: bool) {
        if self.loaded && !self.shown {
            let loading = self.loading();
            self.seen_loading |= loading;
            // Finished once a load was seen and is over; or, for a load too
            // quick for any frame to see, a moment after it was asked for.
            let late = self
                .load_started
                .is_some_and(|t| t.elapsed() > std::time::Duration::from_millis(1500));
            if !loading && (self.seen_loading || late) {
                self.shown = true;
            }
        }
        let hidden = !self.shown || veiled;
        if hidden != self.hidden {
            unsafe {
                let _: () = msg_send![&*self.view, setHidden: Bool::new(hidden)];
            }
            self.hidden = hidden;
        }
    }

    pub fn is_shown(&self) -> bool {
        self.shown
    }

    /// Loaded but stepped aside for one of crc's overlays.
    pub fn is_veiled(&self) -> bool {
        self.shown && self.hidden
    }

    fn evaluate(&self, script: &str, done: Option<BlockRef>) {
        let script = NSString::from_str(script);
        unsafe {
            let _: () = msg_send![&*self.view,
                evaluateJavaScript: &*script,
                completionHandler: done.unwrap_or(BlockRef(std::ptr::null()))];
        }
    }

    /// Asks the page `expression`; the answer lands in [`probe_answer`].
    /// For the self-test, which cannot see inside the web process otherwise.
    pub fn probe(&self, expression: &str) {
        self.evaluate(&format!("String({expression})"), Some(probed_block()));
    }

    pub fn close(self) {
        unsafe {
            let _: () = msg_send![&*self.view, removeFromSuperview];
        }
        let _ = std::fs::remove_file(&self.page);
    }
}

/// Writes `bytes` to `path`, readable by this user only: it holds what the
/// document says.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

/// The last answer to [`WebPreview::probe`].
pub fn probe_answer() -> Option<String> {
    PROBE.with(|p| p.borrow().clone())
}

// ---- blocks -----------------------------------------------------------------
//
// WebKit reports through blocks. These two capture nothing, so each is a
// global block: a static literal in the layout the Blocks ABI defines, which
// Block_copy returns as is. The results go to thread-locals on the main
// thread, where WebKit calls both. That is less code than a crate for
// blocks, and nothing here outlives the process.

#[repr(C)]
struct BlockDescriptor {
    reserved: usize,
    size: usize,
    signature: *const u8,
}

#[repr(C)]
struct Block {
    isa: *const c_void,
    flags: i32,
    reserved: i32,
    invoke: unsafe extern "C" fn(*const Block, *mut AnyObject, *mut AnyObject),
    descriptor: *const BlockDescriptor,
}

unsafe extern "C" {
    static _NSConcreteGlobalBlock: [usize; 0];
}

const BLOCK_IS_GLOBAL: i32 = 1 << 28;
const BLOCK_HAS_SIGNATURE: i32 = 1 << 30;
/// `void (^)(id, id)`.
const SIGNATURE: &[u8] = b"v24@?0@8@16\0";

fn global_block(
    invoke: unsafe extern "C" fn(*const Block, *mut AnyObject, *mut AnyObject),
) -> *const c_void {
    let descriptor = Box::leak(Box::new(BlockDescriptor {
        reserved: 0,
        size: std::mem::size_of::<Block>(),
        signature: SIGNATURE.as_ptr(),
    }));
    let block = Box::leak(Box::new(Block {
        isa: (&raw const _NSConcreteGlobalBlock).cast(),
        flags: BLOCK_IS_GLOBAL | BLOCK_HAS_SIGNATURE,
        reserved: 0,
        invoke,
        descriptor,
    }));
    (block as *const Block).cast()
}

/// A block pointer, typed as one for the message send: WebKit declares its
/// completion handlers as blocks (`@?`), and a bare pointer (`^v`) fails
/// objc2's argument check in debug builds.
#[derive(Clone, Copy)]
#[repr(transparent)]
struct BlockRef(*const c_void);

// SAFETY: a pointer to a block, which is what `@?` describes.
unsafe impl objc2::encode::Encode for BlockRef {
    const ENCODING: objc2::encode::Encoding = objc2::encode::Encoding::Block;
}

fn compiled_block() -> BlockRef {
    static BLOCK: OnceLock<usize> = OnceLock::new();
    BlockRef(*BLOCK.get_or_init(|| global_block(compiled) as usize) as *const c_void)
}

fn probed_block() -> BlockRef {
    static BLOCK: OnceLock<usize> = OnceLock::new();
    BlockRef(*BLOCK.get_or_init(|| global_block(probed) as usize) as *const c_void)
}

/// `^(WKContentRuleList *list, NSError *error)`. The list names its
/// identifier; a failure does not, and is the oldest one still pending.
unsafe extern "C" fn compiled(_: *const Block, list: *mut AnyObject, _error: *mut AnyObject) {
    let list = unsafe { Retained::retain(list) };
    let id = list.as_ref().and_then(|list| {
        let id: *mut NSString = unsafe { msg_send![&**list, identifier] };
        unsafe { id.as_ref() }.map(|id| id.to_string())
    });
    let id = PENDING.with(|p| {
        let mut p = p.borrow_mut();
        let at = match &id {
            Some(id) => p.iter().position(|x| x == id),
            None => (!p.is_empty()).then_some(0),
        }?;
        Some(p.remove(at))
    });
    if let Some(id) = id {
        RULES.with(|r| {
            r.borrow_mut().insert(
                id,
                match list {
                    Some(list) => Rules::Ready(list),
                    None => Rules::Failed,
                },
            )
        });
    }
}

/// `^(id result, NSError *error)`.
unsafe extern "C" fn probed(_: *const Block, result: *mut AnyObject, error: *mut AnyObject) {
    let describe = |object: *mut AnyObject| -> Option<String> {
        if object.is_null() {
            return None;
        }
        let text: *mut NSString = unsafe { msg_send![object, description] };
        unsafe { text.as_ref() }.map(|t| t.to_string())
    };
    let answer = describe(result)
        .or_else(|| describe(error).map(|e| format!("error {e}")))
        .unwrap_or_default();
    PROBE.with(|p| *p.borrow_mut() = Some(answer));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_block_all_but_the_folder_and_data_then_every_navigation() {
        let json = rule_list(
            Some("file:///Users/me/My%20Notes/a.b+c/"),
            "file:///Users/me/Library/Caches/crc/preview-1-2.html",
        );
        let value = crate::json::parse(&json).unwrap();
        let rules = value.as_array().unwrap();
        let filter = |i: usize| {
            rules[i]
                .get("trigger")
                .and_then(|t| t.get("url-filter"))
                .and_then(|f| f.as_str())
                .unwrap()
                .to_owned()
        };
        let action = |i: usize| {
            rules[i]
                .get("action")
                .and_then(|a| a.get("type"))
                .and_then(|t| t.as_str())
                .unwrap()
                .to_owned()
        };
        assert_eq!(rules.len(), 5);
        assert_eq!((filter(0).as_str(), action(0).as_str()), (".*", "block"));
        assert_eq!(filter(1), r"^file:///Users/me/My%20Notes/a\.b\+c/");
        assert_eq!(action(1), "ignore-previous-rules");
        assert_eq!(filter(2), "^data:");
        assert_eq!(action(3), "block");
        assert!(json.contains(r#""resource-type":["document"]"#));
        assert_eq!(
            filter(4),
            r"^file:///Users/me/Library/Caches/crc/preview-1-2\.html$"
        );
        assert_eq!(action(4), "ignore-previous-rules");
        // No folder (an untitled document): only the page comes back in.
        let alone = rule_list(None, "file:///p.html");
        assert_eq!(alone.matches("file").count(), 1, "{alone}");
    }

    #[test]
    fn the_page_gets_a_base_and_the_grant_is_the_shared_folder() {
        let page =
            "<!doctype html>\n<html><HEAD><meta charset=\"utf-8\"></head><body></body></html>";
        let based = with_base(page, Some(Path::new("/Users/me/My Notes")));
        assert!(
            based.contains("<HEAD><base href=\"file:///Users/me/My%20Notes/\"><meta"),
            "{based}"
        );
        assert_eq!(with_base(page, None), page);
        assert!(with_base("<p>x</p>", Some(Path::new("/a"))).starts_with("<base href="));
        assert_eq!(
            common_folder(
                Path::new("/Users/me/Library/Caches/crc"),
                Path::new("/Users/me/Desktop/notes")
            ),
            Path::new("/Users/me")
        );
        assert_eq!(
            common_folder(Path::new("/Users/me/x"), Path::new("/Volumes/usb")),
            Path::new("/")
        );
    }
}
