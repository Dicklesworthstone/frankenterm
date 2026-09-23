// Don't create a new standard console window when launched from the Windows GUI.
#![cfg_attr(not(test), windows_subsystem = "windows")]
// Bounded selection-copy transfers nest the channel and retained pane types.
// Let rustc complete their Send/Sync proofs without suppressing the lint.
#![recursion_limit = "256"]
// Keep this in sync with Cargo.toml: the vendored GUI crate is not yet a
// pedantic-clean primary lint target.
#![allow(clippy::all, clippy::pedantic, clippy::nursery)]
// `cargo check --all-targets` still constructs a test-harness form of this
// binary even though the manifest marks the shipped binary `test = false`.
// That form intentionally excludes `main`, severing the production call graph
// and making every entrypoint-owned helper appear dead. Keep the allowance
// confined to that synthetic target; non-test builds retain dead-code checks.
#![cfg_attr(test, allow(dead_code))]

// Keep the production implementation shared with the opt-in glyph-cache test
// harness without assigning one source path to two Cargo targets.
include!("main.rs");
