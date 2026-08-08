// Don't create a new standard console window when launched from the Windows GUI.
#![cfg_attr(not(test), windows_subsystem = "windows")]
// Keep this in sync with Cargo.toml: the vendored GUI crate is not yet a
// pedantic-clean primary lint target.
#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

// Keep the production implementation shared with the opt-in glyph-cache test
// harness without assigning one source path to two Cargo targets.
include!("main.rs");
