// This test-harness crate compiles the production GUI module graph with
// `cfg(test)`. The `#[cfg(not(test))] fn main` fence in main.rs therefore cannot
// be linked or executed by this target. That fence does not make every included
// test noninteractive: proof commands must still select only exact test bodies
// audited not to initialize the frontend, event loop, or an OS window.
#![allow(clippy::all, clippy::pedantic, clippy::nursery)]
// The included production graph is intentionally much larger than the
// glyph-cache test surface. Keep dead-code suppression scoped to this opt-in
// harness; the production binary target retains its normal rustc lint policy.
#![allow(dead_code)]

include!("main.rs");
