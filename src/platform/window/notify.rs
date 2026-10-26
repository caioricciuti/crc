//! A notification when a session waits on you, answered where it shows.
//!
//! A session that starts waiting (see [`Tab::activity`]) while it is not
//! the one in front of you posts one macOS notification, once per wait,
//! taken back when it stops waiting or closes. Its Reply field types the
//! answer into the session as typing in crc would: a paste, then Return.
//! A click brings crc forward on that session.
//!
//! UserNotifications is a system framework (linked in build.rs), driven by
//! messages as WebKit is. Its completion handlers are blocks, and there is
//! no block crate here: the two kinds needed are made and called by hand,
//! by the Clang block ABI, in the section below.
//!
//! The framework raises an Objective-C exception in a process that is not
//! an app bundle, so a test instance, `cargo test` and a bare binary post
//! nothing. The decision of when to post is plain data, tested without
//! any of it.
//!
//! [`Tab::activity`]: crate::platform::terminal::Tab::activity

use std::ffi::{c_char, c_int, c_ulong, c_void};
use std::sync::atomic::{AtomicU8, Ordering};

use objc2::encode::{Encoding, RefEncode};
use objc2::{ClassType, Message, extern_class, extern_protocol};

use super::*;
use crate::platform::agents::task_of;
use crate::platform::terminal::Activity;

/// The category every notification here is in: the one with the Reply
/// field.
const CATEGORY: &str = "agent-waiting";
/// The Reply field's action.
const REPLY: &str = "reply";
/// What a click on the notification itself is reported as.
const DEFAULT_ACTION: &str = "com.apple.UNNotificationDefaultActionIdentifier";
/// The userInfo key that carries the session's id.
const TAB_KEY: &str = "tab";

/// What a session's wait asks of Notification Center now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Edge {
    Post,
    Withdraw,
    Nothing,
}

/// The edge of a session's wait, `told` whether this wait was told
/// ([`Tab::notified`]), kept up to date. A wait that starts is told once:
/// by a notification, or, while it is `showing` (crc in front on it), by
/// being in front of you. One that ends takes its notification back.
///
/// [`Tab::notified`]: crate::platform::terminal::Tab::notified
pub(super) fn edge(told: &mut bool, waiting: bool, showing: bool) -> Edge {
    match (waiting, *told) {
        (true, false) => {
            *told = true;
            if showing { Edge::Nothing } else { Edge::Post }
        }
        (false, true) => {
            *told = false;
            Edge::Withdraw
        }
        _ => Edge::Nothing,
    }
}

/// What a reply from a notification types before its Return: the text as
/// a paste, without the line breaks it ended on, which would be a Return
/// of their own. Nothing for a blank reply.
pub(super) fn reply_bytes(text: &str, bracketed: bool) -> Option<Vec<u8>> {
    let text = text.trim_end_matches(['\n', '\r']);
    if text.trim().is_empty() {
        return None;
    }
    Some(crate::term::keys::paste(text, bracketed))
}

thread_local! {
    /// The center, in an app bundle that may post; `None` everywhere else.
    static CENTER: RefCell<Option<Retained<UNUserNotificationCenter>>> = const { RefCell::new(None) };
    /// The view answers go to.
    static VIEW: RefCell<Option<Retained<EditorView>>> = const { RefCell::new(None) };
    /// What the self-test's dump says: notifications posted and taken
    /// back, counted whether or not there is a center to post to.
    static COUNTS: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

/// Whether notifications were asked for and what the person said.
static AUTHORIZATION: AtomicU8 = AtomicU8::new(UNASKED);

/// When the center was set up. Sessions held across a relaunch come back
/// already waiting; in the first [`LAUNCH_QUIET`] they count as told, so
/// Restart to Update is not a burst of banners.
static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
const LAUNCH_QUIET: std::time::Duration = std::time::Duration::from_secs(10);
const UNASKED: u8 = 0;
const ASKED: u8 = 1;
const GRANTED: u8 = 2;
const DENIED: u8 = 3;

/// What the self-test's dump says of the notifications.
pub(super) fn report() -> String {
    let (posted, withdrawn) = COUNTS.with(Cell::get);
    format!("posted={posted} withdrawn={withdrawn}")
}

/// Whether this process is an app bundle the framework accepts.
fn bundled() -> bool {
    let Some(class) = AnyClass::get(c"NSBundle") else {
        return false;
    };
    let id: Option<Retained<NSString>> = unsafe {
        let bundle: Option<Retained<AnyObject>> = msg_send![class, mainBundle];
        let Some(bundle) = bundle else {
            return false;
        };
        msg_send![&*bundle, bundleIdentifier]
    };
    id.is_some()
}

/// The center, its delegate and the Reply category, and the person asked
/// once whether crc may notify. Not in a test instance or outside a
/// bundle: see the module's comment.
pub(super) fn install(view: &EditorView) {
    if cfg!(test) || view.ivars().testing || std::env::var_os("CRC_SELFTEST").is_some() {
        return;
    }
    if !bundled() {
        return;
    }
    let center: Retained<UNUserNotificationCenter> =
        unsafe { msg_send![UNUserNotificationCenter::class(), currentNotificationCenter] };
    // The center holds its delegate weakly: this one lives as long as the
    // process, as the app's does.
    let delegate = Delegate::new();
    unsafe {
        let _: () = msg_send![&*center, setDelegate: &*delegate];
    }
    std::mem::forget(delegate);
    set_categories(&center);
    STARTED.get_or_init(std::time::Instant::now);
    VIEW.with(|slot| *slot.borrow_mut() = Some(view.retain()));
    CENTER.with(|slot| *slot.borrow_mut() = Some(center));
    if view.state().is_some_and(|state| state.notify_waiting) {
        authorize();
    }
}

/// The category with the Reply field.
fn set_categories(center: &UNUserNotificationCenter) {
    let Some(set) = AnyClass::get(c"NSSet") else {
        return;
    };
    unsafe {
        let action: Retained<UNTextInputNotificationAction> = msg_send![
            UNTextInputNotificationAction::class(),
            actionWithIdentifier: &*NSString::from_str(REPLY),
            title: &*NSString::from_str("Reply"),
            options: 0usize,
            textInputButtonTitle: &*NSString::from_str("Reply"),
            textInputPlaceholder: &*NSString::from_str("Reply to the agent")
        ];
        let actions = NSArray::from_retained_slice(&[action]);
        let none = NSArray::<AnyObject>::new();
        let category: Retained<UNNotificationCategory> = msg_send![
            UNNotificationCategory::class(),
            categoryWithIdentifier: &*NSString::from_str(CATEGORY),
            actions: &*actions,
            intentIdentifiers: &*none,
            options: 0usize
        ];
        let categories: Retained<AnyObject> = msg_send![set, setWithObject: &*category];
        let _: () = msg_send![center, setNotificationCategories: &*categories];
    }
}

/// Asks the person, once, whether crc may show alerts and play a sound.
/// A refusal is remembered by the system; posting then does nothing.
fn authorize() {
    if AUTHORIZATION
        .compare_exchange(UNASKED, ASKED, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    CENTER.with(|slot| {
        let Some(center) = slot.borrow().clone() else {
            return;
        };
        // UNAuthorizationOptionSound | UNAuthorizationOptionAlert.
        let options: usize = 2 | 4;
        unsafe {
            let _: () = msg_send![
                &*center,
                requestAuthorizationWithOptions: options,
                completionHandler: AUTHORIZED.block()
            ];
        }
    });
}

/// Posts the notification of the session `id`, in place of any it had.
fn post(id: &str, title: &str, body: &str) {
    COUNTS.with(|counts| {
        let (posted, withdrawn) = counts.get();
        counts.set((posted + 1, withdrawn));
    });
    authorize();
    if AUTHORIZATION.load(Ordering::Relaxed) == DENIED {
        return;
    }
    CENTER.with(|slot| {
        let Some(center) = slot.borrow().clone() else {
            return;
        };
        let id = NSString::from_str(id);
        unsafe {
            let content: Retained<UNMutableNotificationContent> =
                msg_send![UNMutableNotificationContent::class(), new];
            let _: () = msg_send![&*content, setTitle: &*NSString::from_str(title)];
            let _: () = msg_send![&*content, setBody: &*NSString::from_str(body)];
            let _: () = msg_send![&*content, setCategoryIdentifier: &*NSString::from_str(CATEGORY)];
            if let Some(dictionary) = AnyClass::get(c"NSDictionary") {
                let info: Retained<AnyObject> = msg_send![
                    dictionary,
                    dictionaryWithObject: &*id,
                    forKey: &*NSString::from_str(TAB_KEY)
                ];
                let _: () = msg_send![&*content, setUserInfo: &*info];
            }
            if let Some(sound) = AnyClass::get(c"UNNotificationSound") {
                let sound: Option<Retained<AnyObject>> = msg_send![sound, defaultSound];
                let _: () = msg_send![&*content, setSound: sound.as_deref()];
            }
            let request: Retained<UNNotificationRequest> = msg_send![
                UNNotificationRequest::class(),
                requestWithIdentifier: &*id,
                content: &*content,
                trigger: None::<&AnyObject>
            ];
            // The handler is nullable: nothing is done with a failure.
            let _: () = msg_send![
                &*center,
                addNotificationRequest: &*request,
                withCompletionHandler: None::<&Block>
            ];
        }
    });
}

/// Takes back the notification of the session `id`, shown or still to be.
fn withdraw(id: &str) {
    COUNTS.with(|counts| {
        let (posted, withdrawn) = counts.get();
        counts.set((posted, withdrawn + 1));
    });
    CENTER.with(|slot| {
        let Some(center) = slot.borrow().clone() else {
            return;
        };
        let ids = NSArray::from_retained_slice(&[NSString::from_str(id)]);
        unsafe {
            let _: () = msg_send![&*center, removeDeliveredNotificationsWithIdentifiers: &*ids];
            let _: () =
                msg_send![&*center, removePendingNotificationRequestsWithIdentifiers: &*ids];
        }
    });
}

/// Each session's wait, told or taken back as it starts and ends; see
/// [`edge`]. Cheap enough for every pump: one look at each session, and
/// nothing made unless one changed.
pub(super) fn sync_if_changed(view: &EditorView) {
    let due = view.state().is_some_and(|state| {
        let panel = &state.terminal;
        !panel.withdrawn.is_empty()
            || panel
                .tabs
                .iter()
                .any(|tab| tab.notified != (tab.activity() == Activity::Waiting))
    });
    if !due {
        return;
    }
    let front = view.in_front();
    let Some(mut state) = view.state_mut() else {
        return;
    };
    let state = &mut *state;
    for id in state.terminal.withdrawn.drain(..) {
        withdraw(&id);
    }
    // In front of you as the status line counts it: crc in front, the
    // keyboard in the terminal, on that session.
    let watching = front && state.terminal.has_keys();
    let active = state.terminal.active;
    let quiet = STARTED.get().is_some_and(|at| at.elapsed() < LAUNCH_QUIET);
    for (index, tab) in state.terminal.tabs.iter_mut().enumerate() {
        let waiting = tab.activity() == Activity::Waiting;
        match edge(&mut tab.notified, waiting, watching && index == active) {
            Edge::Post if state.notify_waiting && !quiet => {
                let title = task_of(&state.agents.tasks, tab)
                    .map_or_else(|| "crc".to_string(), |i| state.agents.tasks[i].name());
                let body = format!("{}: {}", tab.name(), tab.state());
                post(&tab.id().to_string_lossy(), &title, &body);
            }
            Edge::Withdraw => withdraw(&tab.id().to_string_lossy()),
            Edge::Post | Edge::Nothing => {}
        }
    }
}

/// An answer from Notification Center, on its way to the main thread.
struct Answer {
    id: String,
    reply: Option<String>,
}

/// Queued by the delegate: the answer to the view, on the main thread.
unsafe extern "C" fn answer_on_main(context: *mut c_void) {
    // SAFETY: `context` is the box the delegate queued, taken back once.
    let answer = unsafe { Box::from_raw(context as *mut Answer) };
    let Some(view) = VIEW.with(|slot| slot.borrow().clone()) else {
        return;
    };
    view.notification_answered(&answer.id, answer.reply.as_deref());
}

impl EditorView {
    /// A notification of the session `id` answered: a reply typed into
    /// it, or a click that shows it. Nothing for a session closed since.
    pub(super) fn notification_answered(&self, id: &str, reply: Option<&str>) {
        match reply {
            Some(text) => self.reply_to_session(id, text),
            None => self.focus_session(id),
        }
    }

    /// A reply typed into the session `id`, as the person typing it there
    /// would: what it asked is answered, auto-continue leaves it to them.
    /// Its Return follows a moment after the text, as auto-continue's
    /// does: one write of both reads as a paste with a line break in it
    /// to a program that tells pastes from typing by how fast bytes come.
    pub(super) fn reply_to_session(&self, id: &str, text: &str) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(index) = state.terminal.tab_by_id(id) else {
                return;
            };
            let tab = &mut state.terminal.tabs[index];
            let bracketed = tab
                .session
                .term
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .modes
                .bracketed_paste;
            let Some(bytes) = reply_bytes(text, bracketed) else {
                return;
            };
            if tab.type_in(&bytes) {
                super::agents::sync_dock_badge(&state, MainThreadMarker::from(self));
            }
        }
        let id = NSString::from_str(id);
        let gap = crate::platform::retry::ENTER_GAP.as_secs_f64();
        let _: () = unsafe {
            msg_send![self, performSelector: sel!(notifyReturn:),
                withObject: &*id, afterDelay: gap]
        };
        self.request_redraw();
        self.pump();
    }

    /// The Return after a reply's text, into the session `id` if it
    /// still lives.
    pub(super) fn reply_return(&self, id: &str) {
        {
            let Some(mut state) = self.state_mut() else {
                return;
            };
            let Some(index) = state.terminal.tab_by_id(id) else {
                return;
            };
            if state.terminal.tabs[index].type_in(b"\r") {
                super::agents::sync_dock_badge(&state, MainThreadMarker::from(self));
            }
        }
        self.request_redraw();
        self.pump();
    }

    /// A click on a session's notification: crc in front, on it.
    fn focus_session(&self, id: &str) {
        let Some(index) = self.state().and_then(|state| state.terminal.tab_by_id(id)) else {
            return;
        };
        self.status_item_show();
        self.show_session(index);
        self.request_redraw();
        self.pump();
    }

    /// The self-test's `notify N [text]`: the Nth session's notification
    /// answered through the path the delegate's answer takes.
    pub(super) fn selftest_notify(&self, number: usize, reply: Option<&str>) {
        let id = self.state().and_then(|state| {
            let tab = state.terminal.tabs.get(number.checked_sub(1)?)?;
            Some(tab.id().to_string_lossy().into_owned())
        });
        match id {
            Some(id) => self.notification_answered(&id, reply),
            None => eprintln!("selftest: no session {number}"),
        }
    }
}

// ---- The framework's classes ------------------------------------------------
// Only what is sent to them is used: each is driven by messages.

extern_class!(
    #[unsafe(super(NSObject))]
    struct UNUserNotificationCenter;
);

extern_class!(
    #[unsafe(super(NSObject))]
    struct UNMutableNotificationContent;
);

extern_class!(
    #[unsafe(super(NSObject))]
    struct UNNotificationRequest;
);

extern_class!(
    #[unsafe(super(NSObject))]
    struct UNNotificationCategory;
);

extern_class!(
    #[unsafe(super(NSObject))]
    struct UNTextInputNotificationAction;
);

extern_class!(
    #[unsafe(super(NSObject))]
    struct UNNotificationResponse;
);

extern_class!(
    #[unsafe(super(UNNotificationResponse, NSObject))]
    struct UNTextInputNotificationResponse;
);

extern_protocol!(
    /// What the center tells the app: every method is optional, and each
    /// one implemented has to take what the header declares.
    // The macro's own items carry no Safety section to find.
    #[allow(clippy::missing_safety_doc)]
    unsafe trait UNUserNotificationCenterDelegate: NSObjectProtocol {}
);

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - Delegate does not implement Drop.
    // - It has no state: called on any thread, it hands what it is told
    //   to the main thread.
    #[unsafe(super(NSObject))]
    #[name = "CrcNotificationDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        /// A reply, a click or a dismissal. The answer goes to the main
        /// thread; the handler is called before this returns, always.
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: Option<&AnyObject>,
            response: Option<&UNNotificationResponse>,
            handler: Option<&Block>,
        ) {
            if let Some(answer) = response.and_then(answer_of) {
                let answer = Box::into_raw(Box::new(answer));
                // SAFETY: `answer_on_main` takes the box back and frees it.
                unsafe { crate::platform::dispatch::on_main(answer.cast(), answer_on_main) };
            }
            if let Some(handler) = handler {
                // SAFETY: the handler is `void (^)(void)`.
                unsafe { call(handler) };
            }
        }

        /// One posted while crc is the active app: shown all the same. Only
        /// a session not in front of you posts one.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: Option<&AnyObject>,
            _notification: Option<&AnyObject>,
            handler: Option<&Block>,
        ) {
            if let Some(handler) = handler {
                // SAFETY: the handler is
                // `void (^)(UNNotificationPresentationOptions)`.
                unsafe { call_with(handler, PRESENT) };
            }
        }
    }
);

/// UNNotificationPresentationOptionBanner | List | Sound.
const PRESENT: usize = (1 << 4) | (1 << 3) | (1 << 1);

impl Delegate {
    fn new() -> Retained<Delegate> {
        let this = Delegate::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

/// The session a response is for, and its reply if it was one. Nothing
/// for a dismissal.
fn answer_of(response: &UNNotificationResponse) -> Option<Answer> {
    unsafe {
        let action: Retained<NSString> = msg_send![response, actionIdentifier];
        let action = action.to_string();
        let reply = if action == REPLY {
            let text_input: bool =
                msg_send![response, isKindOfClass: UNTextInputNotificationResponse::class()];
            if !text_input {
                return None;
            }
            let text: Option<Retained<NSString>> = msg_send![response, userText];
            Some(text.map(|t| t.to_string()).unwrap_or_default())
        } else if action == DEFAULT_ACTION {
            None
        } else {
            return None;
        };
        let notification: Retained<AnyObject> = msg_send![response, notification];
        let request: Retained<AnyObject> = msg_send![&*notification, request];
        let content: Retained<AnyObject> = msg_send![&*request, content];
        let info: Option<Retained<AnyObject>> = msg_send![&*content, userInfo];
        let tab: Option<Retained<AnyObject>> = match &info {
            Some(info) => msg_send![&**info, objectForKey: &*NSString::from_str(TAB_KEY)],
            None => None,
        };
        let id = match tab {
            Some(tab) if msg_send![&*tab, isKindOfClass: NSString::class()] => {
                let tab: Retained<NSString> = Retained::cast_unchecked(tab);
                tab.to_string()
            }
            _ => {
                let identifier: Retained<NSString> = msg_send![&*request, identifier];
                identifier.to_string()
            }
        };
        Some(Answer { id, reply })
    }
}

// ---- Blocks, by hand -------------------------------------------------------
// The Clang block ABI (clang.llvm.org/docs/Block-ABI-Apple.html). A block
// is a pointer to a literal: its class, flags, the function that runs it
// and a descriptor of its size. The function takes the literal first, then
// the block's own arguments. A literal made here is global: in a static,
// with no captures, so `Block_copy` hands it back as it is and nothing
// ever frees it. A block received is run through its function before the
// method it came to returns, so it is never copied either.

/// A literal in static storage.
const BLOCK_IS_GLOBAL: c_int = 1 << 28;
/// The descriptor has a type signature after the size.
const BLOCK_HAS_SIGNATURE: c_int = 1 << 30;

/// A literal's descriptor, without copy and dispose helpers.
#[repr(C)]
struct Descriptor {
    reserved: c_ulong,
    size: c_ulong,
    signature: *const c_char,
}

// SAFETY: only in statics, never written; the signature is a static string.
unsafe impl Sync for Descriptor {}

/// A block's literal, `F` the type of the function that runs it: a bare
/// pointer for a block received, read only to be called.
#[repr(C)]
struct Block<F = *const c_void> {
    isa: *const c_void,
    flags: c_int,
    reserved: c_int,
    invoke: F,
    descriptor: *const Descriptor,
}

// SAFETY: a pointer to a literal is a block, `@?`.
unsafe impl<F> RefEncode for Block<F> {
    const ENCODING_REF: Encoding = Encoding::Block;
}

/// A literal in a static.
#[repr(transparent)]
struct Global<F>(Block<F>);

// SAFETY: never written; its pointers are to statics and to a function.
unsafe impl<F> Sync for Global<F> {}

impl<F> Global<F> {
    /// The block, to pass where a method takes one.
    fn block(&'static self) -> &'static Block {
        // SAFETY: `Block<F>` and `Block` differ only in the type of a
        // pointer-sized field.
        unsafe { &*(&raw const self.0).cast::<Block>() }
    }
}

unsafe extern "C" {
    static _NSConcreteGlobalBlock: c_void;
}

/// `requestAuthorizationWithOptions:completionHandler:`'s handler,
/// `void (^)(BOOL granted, NSError *error)`. Called on any thread.
unsafe extern "C" fn authorized(_block: *const c_void, granted: Bool, _error: *mut AnyObject) {
    let said = if granted.as_bool() { GRANTED } else { DENIED };
    AUTHORIZATION.store(said, Ordering::Relaxed);
}

type Authorized = unsafe extern "C" fn(*const c_void, Bool, *mut AnyObject);

static AUTHORIZED: Global<Authorized> = Global(Block {
    isa: (&raw const _NSConcreteGlobalBlock).cast(),
    flags: BLOCK_IS_GLOBAL | BLOCK_HAS_SIGNATURE,
    reserved: 0,
    invoke: authorized,
    descriptor: &AUTHORIZED_DESCRIPTOR,
});

static AUTHORIZED_DESCRIPTOR: Descriptor = Descriptor {
    reserved: 0,
    size: size_of::<Block<Authorized>>() as c_ulong,
    signature: c"v20@?0B8@\"NSError\"12".as_ptr(),
};

/// Runs a block received as `void (^)(void)`.
///
/// # Safety
///
/// `block` has to be a live block of that type.
unsafe fn call(block: &Block) {
    // SAFETY: by the ABI, the function of a block of this type.
    unsafe {
        let invoke =
            std::mem::transmute::<*const c_void, unsafe extern "C" fn(*const Block)>(block.invoke);
        invoke(block);
    }
}

/// Runs a block received as `void (^)(NSUInteger)`.
///
/// # Safety
///
/// `block` has to be a live block of that type.
unsafe fn call_with(block: &Block, value: usize) {
    // SAFETY: by the ABI, the function of a block of this type.
    unsafe {
        let invoke = std::mem::transmute::<*const c_void, unsafe extern "C" fn(*const Block, usize)>(
            block.invoke,
        );
        invoke(block, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    #[test]
    fn a_wait_is_told_once_and_again_after_work() {
        let mut told = false;
        assert_eq!(edge(&mut told, true, false), Edge::Post);
        // Still waiting: nothing more, however many looks.
        assert_eq!(edge(&mut told, true, false), Edge::Nothing);
        assert_eq!(edge(&mut told, true, true), Edge::Nothing);
        // Back at work: taken back, once.
        assert_eq!(edge(&mut told, false, false), Edge::Withdraw);
        assert!(!told);
        assert_eq!(edge(&mut told, false, false), Edge::Nothing);
        // The next wait is told again.
        assert_eq!(edge(&mut told, true, false), Edge::Post);
    }

    #[test]
    fn the_session_in_front_of_you_posts_nothing() {
        let mut told = false;
        // In front when it starts waiting: that told it.
        assert_eq!(edge(&mut told, true, true), Edge::Nothing);
        assert!(told);
        // Gone behind another app, still the same wait: nothing late.
        assert_eq!(edge(&mut told, true, false), Edge::Nothing);
        assert_eq!(edge(&mut told, false, true), Edge::Withdraw);
        // A new wait while another session shows.
        assert_eq!(edge(&mut told, true, false), Edge::Post);
    }

    #[test]
    fn a_reply_is_a_paste_without_its_last_line_breaks() {
        assert_eq!(reply_bytes("yes", false).unwrap(), b"yes");
        assert_eq!(
            reply_bytes("yes", true).unwrap(),
            b"\x1b[200~yes\x1b[201~".as_slice()
        );
        // Trailing line breaks go, so there is no second Return.
        assert_eq!(reply_bytes("yes\n", false).unwrap(), b"yes");
        assert_eq!(
            reply_bytes("yes\r\n\n", true).unwrap(),
            b"\x1b[200~yes\x1b[201~"
        );
        // Inside, they stay, as a paste carries them.
        assert_eq!(reply_bytes("one\ntwo\n", false).unwrap(), b"one\rtwo");
        assert_eq!(
            reply_bytes("one\ntwo", true).unwrap(),
            b"\x1b[200~one\rtwo\x1b[201~".as_slice()
        );
        // A blank reply types nothing, not even a Return.
        assert_eq!(reply_bytes("", true), None);
        assert_eq!(reply_bytes("\n", false), None);
        assert_eq!(reply_bytes("  \n", true), None);
    }

    unsafe extern "C" {
        fn _Block_copy(block: *const c_void) -> *const c_void;
        fn _Block_release(block: *const c_void);
    }

    static RAN: AtomicBool = AtomicBool::new(false);
    static GOT: AtomicUsize = AtomicUsize::new(0);
    static PRESENTED: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn ran(_block: *const c_void) {
        RAN.store(true, Ordering::Relaxed);
    }

    unsafe extern "C" fn got(_block: *const c_void, value: usize) {
        GOT.store(value, Ordering::Relaxed);
    }

    unsafe extern "C" fn presented(_block: *const c_void, value: usize) {
        PRESENTED.store(value, Ordering::Relaxed);
    }

    static RAN_DESCRIPTOR: Descriptor = Descriptor {
        reserved: 0,
        size: size_of::<Block>() as c_ulong,
        signature: c"v8@?0".as_ptr(),
    };

    static RUN: Global<unsafe extern "C" fn(*const c_void)> = Global(Block {
        isa: (&raw const _NSConcreteGlobalBlock).cast(),
        flags: BLOCK_IS_GLOBAL | BLOCK_HAS_SIGNATURE,
        reserved: 0,
        invoke: ran,
        descriptor: &RAN_DESCRIPTOR,
    });

    static GET: Global<unsafe extern "C" fn(*const c_void, usize)> = Global(Block {
        isa: (&raw const _NSConcreteGlobalBlock).cast(),
        flags: BLOCK_IS_GLOBAL,
        reserved: 0,
        invoke: got,
        descriptor: &RAN_DESCRIPTOR,
    });

    static PRESENT_BLOCK: Global<unsafe extern "C" fn(*const c_void, usize)> = Global(Block {
        isa: (&raw const _NSConcreteGlobalBlock).cast(),
        flags: BLOCK_IS_GLOBAL,
        reserved: 0,
        invoke: presented,
        descriptor: &RAN_DESCRIPTOR,
    });

    #[test]
    fn a_global_block_has_the_abis_layout() {
        // isa, flags, reserved, invoke, descriptor.
        assert_eq!(std::mem::offset_of!(Block, flags), 8);
        assert_eq!(std::mem::offset_of!(Block, invoke), 16);
        assert_eq!(std::mem::offset_of!(Block, descriptor), 24);
        assert_eq!(size_of::<Block>(), 32);
        assert_eq!(size_of::<Block<Authorized>>(), 32);
        assert_eq!(AUTHORIZED_DESCRIPTOR.size, 32);
        assert_ne!(AUTHORIZED.0.flags & BLOCK_IS_GLOBAL, 0);
        // The runtime takes it as global: a copy is the block itself.
        let block = std::ptr::from_ref(AUTHORIZED.block()).cast::<c_void>();
        unsafe {
            let copy = _Block_copy(block);
            assert_eq!(copy, block);
            _Block_release(copy);
        }
    }

    #[test]
    fn a_block_runs_through_its_function() {
        unsafe { call(RUN.block()) };
        assert!(RAN.load(Ordering::Relaxed));
        unsafe { call_with(GET.block(), PRESENT) };
        assert_eq!(GOT.load(Ordering::Relaxed), PRESENT);
    }

    #[test]
    fn the_delegate_answers_will_present_with_banner_list_and_sound() {
        let delegate = Delegate::new();
        let conforms: bool = unsafe {
            msg_send![&*delegate, respondsToSelector: sel!(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:)]
        };
        assert!(conforms);
        unsafe {
            let _: () = msg_send![
                &*delegate,
                userNotificationCenter: None::<&AnyObject>,
                willPresentNotification: None::<&AnyObject>,
                withCompletionHandler: PRESENT_BLOCK.block()
            ];
        }
        assert_eq!(PRESENTED.load(Ordering::Relaxed), 26);
    }
}
