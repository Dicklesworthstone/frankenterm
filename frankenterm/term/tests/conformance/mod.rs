//! Headless conformance runner (ft-yccm0.1.9).
//!
//! The differential harness (ft-yccm0.1.8) proves a candidate engine behaves
//! like the legacy one. This suite checks absolute correctness, so a bug both
//! engines share still shows, and a legitimate fix is not blocked by a
//! faithfully reproduced bug. It has two halves:
//!
//! - **vttest**: real vttest 2.7 sessions, recorded through a pty by
//!   `fixtures/conformance/vttest/capture_vttest.py`. Each recorded screen is
//!   fed to the engine and its dump compared with the golden in
//!   `fixtures/conformance/vttest/goldens/` (text plus attribute-run JSON;
//!   format in [`dump`]).
//! - **esctest-style cases** ([`cases`]): feed bytes, then assert cursor,
//!   cells, modes, replies and clipboard writes, with expectations taken from
//!   the xterm and DEC documentation.
//!
//! Every case ends PASS, FAIL, XFAIL (fails, listed with a reason in
//! `fixtures/conformance/xfail.txt`) or XPASS (listed but passes, which is
//! also an error for the legacy engine). The legacy engine's run is committed
//! in `fixtures/conformance/baseline/legacy.txt`, so a later engine change
//! shows as deltas rather than absolute noise.
//!
//! # Running
//!
//! - Legacy, must equal the baseline exactly:
//!   `cargo test -p frankenterm-term --test conformance`
//! - A parser candidate registered in `differential::engine::candidates`:
//!   `FT_CONFORMANCE_ENGINE=mux_two_stage cargo test -p frankenterm-term
//!   --test conformance`. Regressions against the baseline fail; fixes are
//!   reported.
//! - The page grid once ft-yccm0.3.3.4 lands (it reads `FT_GRID_ENGINE` in
//!   `Terminal::new`): `FT_GRID_ENGINE=page cargo test -p frankenterm-term
//!   --test conformance`.
//!
//! The full report is printed (use `-- --nocapture`) and written to
//! `$FT_CONFORMANCE_OUT/conformance-<engine>.txt`, else the test temporary
//! directory. `FT_CONFORMANCE_PRINT=1` also prints the engine's dumps in
//! golden-file form and its baseline, for review. Goldens and the baseline
//! change only with a recorded reason in the commit body; never regenerate
//! them to make a run pass.

pub mod cases;
pub mod dump;
pub mod esctest;
pub mod suite;
pub mod vtrec;
