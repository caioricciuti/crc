//! The system clipboard, via NSPasteboard: text, images, and files.
//!
//! Files are on it the way the Finder puts them, one file URL to an item,
//! so a copy made in the Explorer pastes in the Finder and one made in the
//! Finder pastes in the Explorer. The Explorer's own copies carry the paths
//! as plain text as well, one to a line, for a paste into a terminal or a
//! text field; a cut carries a mark of its own beside them.

use std::path::PathBuf;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardTypeFileURL,
    NSPasteboardTypePNG, NSPasteboardTypeString, NSPasteboardTypeTIFF, NSPasteboardWriting,
};
use objc2_foundation::{NSArray, NSDictionary, NSString, NSURL};

/// The type beside the file URLs of items cut in the Explorer. Its value
/// is the process that cut them: a paste in that window moves them, and a
/// paste anywhere else, another crc window included, copies, since only
/// the window that cut them can take their tabs and history along.
const CUT_TYPE: &str = "dev.ricciuti.crc.explorer-cut";

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

/// The pasteboard a scripted drag from another app carries its files on,
/// under the self-test: one of the instance's own, never the system's drag
/// pasteboard, which belongs to whatever the person is dragging.
pub fn scripted_drag() -> Retained<NSPasteboard> {
    NSPasteboard::pasteboardWithName(&NSString::from_str(&format!(
        "crc-selftest-drag-{}",
        std::process::id()
    )))
}

/// Replaces the clipboard contents with `text`.
pub fn write_text(text: &str) {
    write_text_on(&general(), text);
}

pub fn write_text_on(pasteboard: &NSPasteboard, text: &str) {
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

/// The files a pasteboard holds, and whether this window cut them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Files {
    pub paths: Vec<PathBuf>,
    /// Cut in this process's Explorer: a paste moves them.
    pub cut: bool,
}

/// Replaces the clipboard contents with the files at `paths`, as file URLs
/// with the paths as text beside them, marked as cut when `cut`. Answers
/// the clipboard's change count with them on it: once it differs, something
/// else has been copied.
pub fn write_files(paths: &[PathBuf], cut: bool) -> isize {
    write_files_on(&general(), paths, cut)
}

pub fn write_files_on(pasteboard: &NSPasteboard, paths: &[PathBuf], cut: bool) -> isize {
    write_file_urls_on(pasteboard, paths);
    // The text and the mark are the first item's: read as a whole, the
    // pasteboard then has one string, every path in it.
    let text = paths
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");
    let string = unsafe { NSPasteboardTypeString };
    let mark = NSString::from_str(CUT_TYPE);
    let mut types = vec![string];
    if cut {
        types.push(&mark);
    }
    unsafe { pasteboard.addTypes_owner(&NSArray::from_slice(&types), None) };
    pasteboard.setString_forType(&NSString::from_str(&text), string);
    if cut {
        let process = NSString::from_str(&std::process::id().to_string());
        pasteboard.setString_forType(&process, &mark);
    }
    pasteboard.changeCount()
}

/// Replaces the clipboard contents with the files at `paths` as file URLs
/// and nothing else: what a copy in another app leaves there.
pub fn write_file_urls(paths: &[PathBuf]) {
    write_file_urls_on(&general(), paths);
}

pub fn write_file_urls_on(pasteboard: &NSPasteboard, paths: &[PathBuf]) {
    pasteboard.clearContents();
    pasteboard.writeObjects(&NSArray::from_retained_slice(&file_url_writers(paths)));
}

/// The files at `paths` as what a pasteboard is given, one file URL to an
/// item: what a copy writes, and what each item of a drag out of the
/// Explorer carries.
pub fn file_url_writers(
    paths: &[PathBuf],
) -> Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> {
    paths
        .iter()
        .filter_map(|path| NSURL::URLWithString(&NSString::from_str(&file_url(path))))
        .map(ProtocolObject::from_retained)
        .collect()
}

/// The `file:` URL of `path`, a folder's ending in a slash. Written out
/// byte for byte: a URL that Foundation makes from a path has its accents
/// decomposed, and a name read back from it is then not the name on disk.
fn file_url(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut url = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            url.push(byte as char);
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    // Asked of the item itself: a link to a folder is a link.
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) && !url.ends_with('/') {
        url.push('/');
    }
    url
}

/// The files on the clipboard: none when it holds something else.
pub fn read_files() -> Files {
    read_files_on(&general())
}

pub fn read_files_on(pasteboard: &NSPasteboard) -> Files {
    let paths = file_urls(pasteboard);
    let cut = !paths.is_empty()
        && pasteboard
            .stringForType(&NSString::from_str(CUT_TYPE))
            .is_some_and(|process| process.to_string() == std::process::id().to_string());
    Files { paths, cut }
}

/// The files on a pasteboard, as paths: the clipboard's, or a drag's.
pub fn file_urls(pasteboard: &NSPasteboard) -> Vec<PathBuf> {
    let Some(items) = pasteboard.pasteboardItems() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let text = item.stringForType(unsafe { NSPasteboardTypeFileURL })?;
            let url = NSURL::URLWithString(&text)?;
            let path = url.path()?;
            Some(PathBuf::from(path.to_string()))
        })
        .collect()
}

/// The clipboard's change count: it moves whenever anything is put there.
pub fn change_count() -> isize {
    general().changeCount()
}

/// Empties the clipboard.
pub fn clear() {
    general().clearContents();
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2::msg_send;

    /// A pasteboard of the test's own, gone when the test is: nothing here
    /// may read or replace what the person running the tests has copied.
    struct Private(Retained<NSPasteboard>);

    impl Private {
        fn new() -> Private {
            Private(NSPasteboard::pasteboardWithUniqueName())
        }

        fn text(&self) -> Option<String> {
            self.0
                .stringForType(unsafe { NSPasteboardTypeString })
                .map(|text| text.to_string())
        }
    }

    impl Drop for Private {
        fn drop(&mut self) {
            let _: () = unsafe { msg_send![&*self.0, releaseGlobally] };
        }
    }

    /// Invented items on disk: a name with a space, one with an accent as
    /// one character, a folder. Removed with the fixture.
    fn sample(name: &str) -> (crate::project::TempTree, Vec<PathBuf>) {
        let tree = crate::project::TempTree::new(
            &format!("clipboard-{name}"),
            &[
                ("packing list.txt", "socks"),
                ("caf\u{e9}.md", "open late"),
                ("trips/", ""),
            ],
        );
        let paths = ["packing list.txt", "caf\u{e9}.md", "trips"]
            .iter()
            .map(|item| tree.0.join(item))
            .collect();
        (tree, paths)
    }

    #[test]
    fn copied_files_are_file_urls_with_their_paths_as_text() {
        let private = Private::new();
        let (_fixture, paths) = sample("copied");
        let change = write_files_on(&private.0, &paths, false);
        assert_eq!(change, private.0.changeCount());
        assert_eq!(
            read_files_on(&private.0),
            Files {
                paths: paths.clone(),
                cut: false
            }
        );
        // One URL to an item, as the Finder reads them.
        let items = private.0.pasteboardItems().unwrap();
        assert_eq!(items.len(), paths.len());
        for item in items.iter() {
            let url = item
                .stringForType(unsafe { NSPasteboardTypeFileURL })
                .unwrap()
                .to_string();
            assert!(url.starts_with("file:///"), "{url}");
        }
        // And the paths, one to a line, for a terminal or a text field.
        let lines: Vec<String> = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(private.text(), Some(lines.join("\n")));
        assert!(!has_image_on(&private.0));
    }

    #[test]
    fn a_cut_is_marked_for_the_process_that_made_it() {
        let private = Private::new();
        let (_fixture, paths) = sample("cut");
        let change = write_files_on(&private.0, &paths, true);
        assert_eq!(
            read_files_on(&private.0),
            Files {
                paths: paths.clone(),
                cut: true
            }
        );
        // Another process's cut pastes here as a copy.
        let mark = NSString::from_str(CUT_TYPE);
        private.0.setString_forType(&NSString::from_str("1"), &mark);
        assert_eq!(
            read_files_on(&private.0),
            Files {
                paths: paths.clone(),
                cut: false
            }
        );
        // Copied again, the mark is gone and the count has moved.
        let again = write_files_on(&private.0, &paths[..1], false);
        assert_ne!(again, change);
        assert_eq!(
            read_files_on(&private.0),
            Files {
                paths: paths[..1].to_vec(),
                cut: false
            }
        );
    }

    #[test]
    fn files_from_another_app_are_read_and_text_is_not_files() {
        let private = Private::new();
        let (_fixture, paths) = sample("foreign");
        write_file_urls_on(&private.0, &paths);
        assert_eq!(read_files_on(&private.0), Files { paths, cut: false });
        assert_eq!(private.text(), None);

        let before = private.0.changeCount();
        private.0.clearContents();
        private
            .0
            .setString_forType(&NSString::from_str("/not/a/file"), unsafe {
                NSPasteboardTypeString
            });
        assert_eq!(read_files_on(&private.0), Files::default());
        assert_ne!(private.0.changeCount(), before);
    }

    /// What a drag out of the Explorer carries, one item and several: a
    /// URL to each, byte for byte the name on disk, with a space and an
    /// accent escaped as they are written there and a folder's ending in
    /// a slash.
    #[test]
    fn files_dragged_out_are_one_url_each_byte_for_byte() {
        let private = Private::new();
        let (fixture, paths) = sample("dragged-out");
        let root = file_url(&fixture.0);
        let carried = |paths: &[PathBuf]| -> Vec<String> {
            private.0.clearContents();
            private
                .0
                .writeObjects(&NSArray::from_retained_slice(&file_url_writers(paths)));
            let items = private.0.pasteboardItems().unwrap();
            items
                .iter()
                .filter_map(|item| item.stringForType(unsafe { NSPasteboardTypeFileURL }))
                .map(|url| url.to_string())
                .collect()
        };
        assert_eq!(carried(&paths[1..2]), [format!("{root}caf%C3%A9.md")]);
        assert_eq!(file_urls(&private.0), paths[1..2]);
        assert_eq!(
            carried(&paths),
            [
                format!("{root}packing%20list.txt"),
                format!("{root}caf%C3%A9.md"),
                format!("{root}trips/"),
            ]
        );
        assert_eq!(file_urls(&private.0), paths);
        // Nothing beside the URLs: no text for another app to take.
        assert_eq!(private.text(), None);
    }

    /// What a drag carries is read the same way. Text and an image are
    /// not files, a path written out as text included: dropped on the
    /// Explorer's tree, which takes files only, they are nothing.
    #[test]
    fn a_drag_of_text_or_of_an_image_carries_no_files() {
        let private = Private::new();
        let (_fixture, paths) = sample("drag");
        write_file_urls_on(&private.0, &paths);
        assert_eq!(file_urls(&private.0), paths);

        write_text_on(&private.0, &paths[0].to_string_lossy());
        assert_eq!(file_urls(&private.0), [] as [PathBuf; 0]);
        assert_eq!(
            private.text(),
            Some(paths[0].to_string_lossy().into_owned())
        );

        private.0.clearContents();
        let image = objc2_foundation::NSData::with_bytes(b"not really a png");
        private
            .0
            .setData_forType(Some(&image), unsafe { NSPasteboardTypePNG });
        assert!(has_image_on(&private.0));
        assert_eq!(file_urls(&private.0), [] as [PathBuf; 0]);
    }
}
