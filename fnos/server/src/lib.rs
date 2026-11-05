pub mod api;
pub mod model;
pub mod service;
pub mod target;

// Compile the same pure functions as the desktop edition, without its GUI runtime.
#[path = "../../../src-tauri/src/hosts_text.rs"]
#[allow(dead_code, clippy::manual_is_multiple_of)]
pub(crate) mod hosts_text;
#[path = "../../../src-tauri/src/hosts_apply/content.rs"]
pub(crate) mod managed_content;
