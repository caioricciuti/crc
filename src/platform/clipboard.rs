//! The system clipboard, via NSPasteboard.

use objc2::rc::Retained;
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardTypePNG,
    NSPasteboardTypeString, NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSDictionary, NSString};

/// The general pasteboard; a private one under the self-test, so a run
/// that copies never replaces what the person running it had copied.
fn general() -> Retained<NSPasteboard> {
    if std::env::var_os("CRC_SELFTEST").is_some() {
        return NSPasteboard::pasteboardWithName(&NSString::from_str(&format!(
            "crc-selftest-{}",
            std::process::id()
        )));
    }
    NSPasteboard::generalPasteboard()
}

/// Replaces the clipboard contents with `text`.
pub fn write_text(text: &str) {
    let pasteboard = general();
    // Required before writing: the pasteboard is versioned, and setString
    // silently fails against a stale change count.
    unsafe {
        pasteboard.clearContents();
        pasteboard.setString_forType(&NSString::from_str(text), NSPasteboardTypeString);
    }
}

/// Reads plain text from the clipboard, if it holds any.
pub fn read_text() -> Option<String> {
    let pasteboard = general();
    let value = unsafe { pasteboard.stringForType(NSPasteboardTypeString) }?;
    Some(value.to_string())
}

/// Whether the clipboard holds an image (a screenshot, a copy from a
/// browser), without reading it.
pub fn has_image() -> bool {
    has_image_on(&general())
}

pub fn has_image_on(pasteboard: &NSPasteboard) -> bool {
    pasteboard.types().is_some_and(|types| {
        types.iter().any(|t| {
            &*t == unsafe { NSPasteboardTypePNG } || &*t == unsafe { NSPasteboardTypeTIFF }
        })
    })
}

/// The clipboard's image as PNG, if it holds one: as given when it
/// offers PNG, else its TIFF (what a screenshot is) converted.
pub fn read_image_png() -> Option<Vec<u8>> {
    read_image_png_on(&general())
}

pub fn read_image_png_on(pasteboard: &NSPasteboard) -> Option<Vec<u8>> {
    if let Some(png) = pasteboard.dataForType(unsafe { NSPasteboardTypePNG }) {
        return Some(png.to_vec());
    }
    let tiff = pasteboard.dataForType(unsafe { NSPasteboardTypeTIFF })?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
    let png = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }?;
    Some(png.to_vec())
}

/// Whether the clipboard offers plain text, without reading it: menu
/// validation asks on every key equivalent, and the text may be megabytes.
pub fn has_text() -> bool {
    let pasteboard = general();
    pasteboard.types().is_some_and(|types| {
        types
            .iter()
            .any(|t| &*t == unsafe { NSPasteboardTypeString })
    })
}
