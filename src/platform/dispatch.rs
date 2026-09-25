//! Running a function on the main thread from anywhere.
//!
//! `dispatch_async_f` takes a plain function pointer and a context, so
//! nothing here needs blocks or a crate. The watcher and the language
//! servers use it to wake the editor when something arrived on one of their
//! threads; the editor's state is only ever touched from the main thread.

use std::ffi::c_void;

unsafe extern "C" {
    fn dispatch_async_f(
        queue: *mut c_void,
        context: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );
    static _dispatch_main_q: c_void;
}

/// Queues `work(context)` on the main thread.
///
/// # Safety
///
/// `context` has to stay valid until `work` runs, which is why the callers
/// keep theirs alive for the process lifetime.
pub unsafe fn on_main(context: *mut c_void, work: unsafe extern "C" fn(*mut c_void)) {
    unsafe {
        dispatch_async_f(
            std::ptr::addr_of!(_dispatch_main_q) as *mut c_void,
            context,
            work,
        );
    }
}
