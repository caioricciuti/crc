//! crc: an editor.
//!
//! Split into a library plus a thin binary so the text, layout and render
//! layers can be tested and benchmarked without opening a window.

// The window's `define_class!` holds every menu action, and expanding that
// many `sel!`s goes past the default macro recursion limit of 128.
#![recursion_limit = "256"]

pub mod base64;
pub mod complete;
pub mod ext;
pub mod http;
pub mod ide;
pub mod index;
pub mod json;
pub mod lsp;
pub mod markdown;
pub mod mcp_client;
pub mod platform;
pub mod project;
pub mod render;
pub mod syntax;
pub mod term;
pub mod text;

/// The package version and exact checkout used to build this binary.
pub fn build_label() -> String {
    format!(
        "{} ({})",
        env!("CARGO_PKG_VERSION"),
        option_env!("CRC_BUILD_REVISION").unwrap_or("unknown")
    )
}
