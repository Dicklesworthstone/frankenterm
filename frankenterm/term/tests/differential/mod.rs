//! Differential terminal-engine harness (ft-yccm0.1.8).
//!
//! Track B replaces the hottest code in the terminal (the parser in B2, the
//! grid in B3). The existing engine is the oracle: every candidate receives
//! the same byte stream, split at the same random chunk boundaries, and after
//! every chunk the two normalized snapshots must be equal. A snapshot covers
//! the visible rows plus a bounded scrollback window (text, attributes
//! including hyperlinks and images, per-cell widths, wrap and line bits), the
//! cursor (position, shape, visibility), and every mode and register from
//! `TerminalState::mode_snapshot` (DEC private modes, insert, origin, auto
//! wrap, margins, charsets, pending wrap, title, palette overrides).
//!
//! On a divergence the harness minimizes the input (ddmin over chunks, then
//! over bytes) and writes `<name>.repro` plus a human-readable `<name>.diff.txt`
//! to `$FT_DIFFERENTIAL_OUT`, else the test temporary directory.
//!
//! Engines register as candidates in [`engine::candidates`]. The harness
//! itself is engine-agnostic.
//!
//! Shared by `tests/engine_differential.rs` (the CI runner) and
//! `fuzz/fuzz_targets/term_engine_differential.rs` (cargo-fuzz), so this
//! module must compile under both edition 2018 and edition 2024.
//!
//! # Modes
//!
//! - CI-cheap: `cargo test -p frankenterm-term --test engine_differential`.
//!   Fixed seeds; the random campaign stops after `FT_DIFFERENTIAL_SECONDS`
//!   (default 20) or `FT_DIFFERENTIAL_CASES` (default 400) cases.
//! - Long campaign (hours):
//!   `FT_DIFFERENTIAL_SECONDS=14400 FT_DIFFERENTIAL_CASES=100000000
//!   FT_DIFFERENTIAL_SEED=<n> cargo test --profile release-perf -p
//!   frankenterm-term --test engine_differential fixed_seed_campaign --
//!   --nocapture`, or the fuzz target:
//!   `cargo fuzz run term_engine_differential --
//!   -dict=fuzz/term_engine_differential.dict`.

pub mod ddmin;
pub mod engine;
pub mod harness;
pub mod snapshot;
pub mod streams;

#[path = "../../benches/ingest/corpus.rs"]
pub mod corpus;
