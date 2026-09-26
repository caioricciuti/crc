//! The system clipboard, via NSPasteboard.

use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

/// Replaces the clipboard contents with `text`.
pub fn write_text(text: &str) {
    let pasteboard = NSPasteboard::generalPasteboard();
    // Required before writing: the pasteboard is versioned, and setString
    // silently fails against a stale change count.
    unsafe {
        pasteboard.clearContents();
        pasteboard.setString_forType(&NSString::from_str(text), NSPasteboardTypeString);
    }
}

/// Reads plain text from the clipboard, if it holds any.
pub fn read_text() -> Option<String> {
    let pasteboard = NSPasteboard::generalPasteboard();
    let value = unsafe { pasteboard.stringForType(NSPasteboardTypeString) }?;
    Some(value.to_string())
}

/// Whether the clipboard offers plain text, without reading it: menu
/// validation asks on every key equivalent, and the text may be megabytes.
pub fn has_text() -> bool {
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.types().is_some_and(|types| {
        types
            .iter()
            .any(|t| &*t == unsafe { NSPasteboardTypeString })
    })
}
