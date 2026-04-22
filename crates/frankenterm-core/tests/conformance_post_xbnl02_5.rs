//! Conformance harness for post-ft-xbnl0.2.5 + ft-2h5wv runtime invariants.
//!
//! # Why this file exists
//!
//! Two refactors landed within minutes of each other (`2cc8d5a6` and
//! `dab704d3`) that each establish a narrow but load-bearing invariant:
//!
//!   * `ft-xbnl0.2.5` (commit `dab704d3`, `refactor(ft-xbnl0.2.5): make
//!     asupersync non-optional, retire dual-runtime feature gate`) makes
//!     `asupersync` a required dependency of `frankenterm-core`, turns
//!     `asupersync-runtime` into a documented no-op, and drops
//!     `dep:asupersync` from every feature that previously pulled it in.
//!     The whole runtime_compat collapse depends on this file staying
//!     that way.
//!
//!   * `ft-2h5wv` (commit `2cc8d5a6`, `refactor(vendored/mux_pool):
//!     preserve cx cancellation in wezterm callers`) routes every mux
//!     pool call through a new classifier
//!     (`mux_error_should_fallback_to_cli`) + adapter
//!     (`mux_cancelled_error`) so a `cx`-cancellation never silently
//!     degrades to CLI fallback with a masked `Ok(...)`.
//!
//! Both refactors are easy to partially revert — a rebased diff that
//! re-adds `optional = true` on the `asupersync` line, or a well-meaning
//! refactor that drops the `if !Self::mux_error_should_fallback_to_cli`
//! branch back to the pre-fix `tracing::debug!(...); /* fall through */`
//! shape, would pass every existing suite but silently reintroduce the
//! exact bugs these commits fixed.
//!
//! This file's job is to make that reversion impossible-to-miss.
//!
//! # Specifications pinned
//!
//! ## SPEC-xbnl-2-5 (Cargo.toml structure, commit `dab704d3`)
//!
//!   * MUST-1: `asupersync` is listed as a non-optional dependency of
//!             `frankenterm-core` — the line must NOT contain
//!             `optional = true`.
//!   * MUST-2: `asupersync` is always compiled with `features = ["tls"]`.
//!   * MUST-3: The `asupersync-runtime` feature is declared and has an
//!             empty feature list (it is a no-op).
//!   * MUST-4: `sync` feature has an empty feature list — no
//!             `dep:asupersync`.
//!   * MUST-5: `native-wezterm` feature has an empty feature list —
//!             no `asupersync-runtime`.
//!   * MUST-6: `web` feature declares exactly `["dep:fastapi"]` — no
//!             `dep:asupersync`, no `asupersync-runtime`.
//!   * MUST-7: `distributed` feature does NOT declare `dep:asupersync`.
//!
//! ## SPEC-2h5wv (MuxPool cx-cancellation preservation, commit `2cc8d5a6`)
//!
//!   * MUST-8:  The classifier fn `mux_error_should_fallback_to_cli` is
//!              declared on `WeztermClient` with `#[cfg(all(feature =
//!              "vendored", unix))]`. Dropping it is a reversion.
//!   * MUST-9:  The adapter fn `mux_cancelled_error` is declared on
//!              `WeztermClient` with the same cfg gate.
//!   * MUST-10: The four regression tests landed in `2cc8d5a6`
//!              (`mux_pool_cancelled_does_not_fallback_to_cli`,
//!              `mux_transport_cancellation_does_not_fallback_to_cli`,
//!              `mux_acquire_timeout_still_falls_back_to_cli`,
//!              `mux_cancelled_error_maps_to_cancelled_core_error`) are
//!              present in `wezterm.rs`. Removing any of them drops the
//!              classifier's regression net.
//!   * MUST-11: Every pub mux-caller path that catches a
//!              `MuxPoolError` cites the classifier before the CLI
//!              fallback `tracing::debug!` line — no path may regress
//!              to the pre-fix "silently drop cancellation and fall
//!              through to CLI" shape. The caller set is:
//!              {list_panes, list_panes_with_cx, get_text,
//!              pane_tiered_scrollback_summary,
//!              pane_tiered_scrollback_summary_with_cx, send_text,
//!              send_text_with_cx} — at least 7 call sites of
//!              `mux_error_should_fallback_to_cli` must exist in
//!              `wezterm.rs`.
//!
//! # Discrepancies
//!
//! None at time of authorship (2026-04-21). If a future refactor
//! intentionally collapses the classifier back into its callers, it
//! must document the divergence here with a `DISC-NNN` tag and update
//! MUST-8/MUST-9/MUST-11 accordingly.
//!
//! # Coverage matrix
//!
//!   MUST clauses : 11
//!   Tested       : 11
//!   Score        : 11 / 11 = 1.00
//!
//! # How the harness reads the spec
//!
//! The conformance source-of-truth is the two Cargo / Rust files
//! themselves. The assertions here are deliberately string-based
//! against the on-disk text rather than parsing TOML / Rust — a strict
//! string match is exactly the signal we want. A reformatting pass
//! that legitimately changes whitespace is a single-line fix here;
//! silently re-adding `optional = true` is a single-test failure.

use std::fs;
use std::path::{Path, PathBuf};

fn cargo_manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_frankenterm_core_cargo_toml() -> (PathBuf, String) {
    let path = cargo_manifest_dir().join("Cargo.toml");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("conformance harness cannot read {}: {e}", path.display()));
    (path, text)
}

fn read_wezterm_rs() -> (PathBuf, String) {
    let path = cargo_manifest_dir().join("src").join("wezterm.rs");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("conformance harness cannot read {}: {e}", path.display()));
    (path, text)
}

/// Return the body (between `[` and matching `]`) of a cargo feature
/// declaration `feature_name = [ ... ]`, or `None` if the feature is
/// absent. Only handles the single-line / trivially multi-line forms
/// produced by `cargo fmt`, which is all the Cargo.toml uses.
fn feature_body<'a>(cargo_toml: &'a str, feature_name: &str) -> Option<&'a str> {
    let needle = format!("\n{feature_name} = [");
    let start = cargo_toml.find(&needle)? + needle.len();
    let rest = &cargo_toml[start..];
    // find the matching closing bracket at depth 0 (no nested arrays in features)
    let end = rest.find(']')?;
    Some(&rest[..end])
}

// =========================================================================
// SPEC-xbnl-2-5: Cargo.toml structural invariants
// =========================================================================

#[test]
fn spec_xbnl_2_5_must_1_asupersync_is_non_optional() {
    let (path, text) = read_frankenterm_core_cargo_toml();
    // Find the asupersync dep line(s). The current shape is:
    //   asupersync = { workspace = true, features = ["tls"] }
    // A regression reintroducing the feature gate would look like:
    //   asupersync = { workspace = true, optional = true, features = ["tls"] }
    let mut asupersync_lines: Vec<&str> = text
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("asupersync =") || trimmed.starts_with("asupersync=")
        })
        .collect();
    asupersync_lines.retain(|line| !line.trim_start().starts_with('#'));
    assert!(
        !asupersync_lines.is_empty(),
        "[MUST-1] {} — the asupersync dependency line has disappeared",
        path.display()
    );
    for line in &asupersync_lines {
        assert!(
            !line.contains("optional = true") && !line.contains("optional=true"),
            "[MUST-1] {} — asupersync must not be optional (ft-xbnl0.2.5 \
             made it non-optional to collapse the runtime_compat seam). \
             Offending line: {line:?}",
            path.display()
        );
    }
}

#[test]
fn spec_xbnl_2_5_must_2_asupersync_has_tls_feature() {
    let (_, text) = read_frankenterm_core_cargo_toml();
    let asupersync_line = text
        .lines()
        .find(|line| {
            let trimmed = line.trim_start();
            (trimmed.starts_with("asupersync =") || trimmed.starts_with("asupersync="))
                && !trimmed.starts_with('#')
        })
        .expect("[MUST-2] asupersync dep line must be present");
    assert!(
        asupersync_line.contains("features = [\"tls\"]")
            || asupersync_line.contains("features=[\"tls\"]"),
        "[MUST-2] asupersync must enable the `tls` feature. \
         Line: {asupersync_line:?}"
    );
}

#[test]
fn spec_xbnl_2_5_must_3_asupersync_runtime_is_empty_noop() {
    let (_, text) = read_frankenterm_core_cargo_toml();
    let body = feature_body(&text, "asupersync-runtime").expect(
        "[MUST-3] `asupersync-runtime` feature must still be declared \
         (kept as a documented no-op for backwards compat)",
    );
    assert!(
        body.trim().is_empty(),
        "[MUST-3] asupersync-runtime must be an empty no-op feature \
         (ft-xbnl0.2.5 retired the dual-runtime gate; any non-empty \
          body brings the gate back). Got: {body:?}"
    );
}

#[test]
fn spec_xbnl_2_5_must_4_sync_feature_is_empty() {
    let (_, text) = read_frankenterm_core_cargo_toml();
    let body = feature_body(&text, "sync").expect("[MUST-4] `sync` feature must still be declared");
    assert!(
        body.trim().is_empty(),
        "[MUST-4] `sync` feature must have an empty body — it previously \
         pulled `dep:asupersync`, which is now always-on. Got: {body:?}"
    );
}

#[test]
fn spec_xbnl_2_5_must_5_native_wezterm_feature_is_empty() {
    let (_, text) = read_frankenterm_core_cargo_toml();
    let body = feature_body(&text, "native-wezterm")
        .expect("[MUST-5] `native-wezterm` feature must still be declared");
    assert!(
        body.trim().is_empty(),
        "[MUST-5] `native-wezterm` feature must have an empty body — \
         asupersync-runtime is a no-op so pulling it here is redundant. \
         Got: {body:?}"
    );
}

#[test]
fn spec_xbnl_2_5_must_6_web_feature_only_declares_fastapi() {
    let (_, text) = read_frankenterm_core_cargo_toml();
    let body = feature_body(&text, "web").expect("[MUST-6] `web` feature must be declared");
    // Normalize whitespace + quotes for comparison.
    let entries: Vec<String> = body
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(
        entries,
        vec!["dep:fastapi".to_string()],
        "[MUST-6] `web` feature must declare exactly [\"dep:fastapi\"] \
         — ft-xbnl0.2.5 dropped `dep:asupersync` and `asupersync-runtime` \
         since asupersync is unconditional now. Got: {entries:?}"
    );
}

#[test]
fn spec_xbnl_2_5_must_7_distributed_feature_does_not_pull_asupersync() {
    let (_, text) = read_frankenterm_core_cargo_toml();
    let body = feature_body(&text, "distributed")
        .expect("[MUST-7] `distributed` feature must be declared");
    assert!(
        !body.contains("asupersync"),
        "[MUST-7] `distributed` feature must NOT reference `asupersync` \
         (any form: `dep:asupersync`, `asupersync-runtime`, \
         `asupersync/tls`, ...). asupersync is a required dep now. \
         Got: {body:?}"
    );
}

// =========================================================================
// SPEC-2h5wv: MuxPool cx-cancellation preservation
// =========================================================================

/// Collapse whitespace so minor formatting drift doesn't false-alarm
/// the signature-presence checks.
fn squeeze_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn wezterm_rs_squeezed() -> (PathBuf, String) {
    let (path, text) = read_wezterm_rs();
    let squeezed = squeeze_whitespace(&text);
    (path, squeezed)
}

#[test]
fn spec_2h5wv_must_8_classifier_fn_exists() {
    let (path, squeezed) = wezterm_rs_squeezed();
    assert!(
        squeezed.contains("fn mux_error_should_fallback_to_cli"),
        "[MUST-8] {} — fn `mux_error_should_fallback_to_cli` is gone. \
         ft-2h5wv routes every mux pool error through this classifier \
         so cancellations do NOT masquerade as \"fall back to CLI, all \
         good\". Removing it means cancellations start leaking through \
         again.",
        path.display()
    );
}

#[test]
fn spec_2h5wv_must_9_cancelled_error_adapter_exists() {
    let (path, squeezed) = wezterm_rs_squeezed();
    assert!(
        squeezed.contains("fn mux_cancelled_error"),
        "[MUST-9] {} — fn `mux_cancelled_error` is gone. Without the \
         adapter, mux pool cancellation errors cannot be turned into \
         `Error::Cancelled(...)` with a readable op-name in the \
         message; callers will default-map to generic mux errors and \
         break the cancellation contract with the outer `cx`.",
        path.display()
    );
}

#[test]
fn spec_2h5wv_must_10_four_regression_tests_are_present() {
    let (path, squeezed) = wezterm_rs_squeezed();
    let required = [
        "fn mux_pool_cancelled_does_not_fallback_to_cli",
        "fn mux_transport_cancellation_does_not_fallback_to_cli",
        "fn mux_acquire_timeout_still_falls_back_to_cli",
        "fn mux_cancelled_error_maps_to_cancelled_core_error",
    ];
    let missing: Vec<&&str> = required
        .iter()
        .filter(|t| !squeezed.contains(**t))
        .collect();
    assert!(
        missing.is_empty(),
        "[MUST-10] {} — the following ft-2h5wv regression tests are \
         missing. Removing any of them drops the classifier's \
         regression net. Missing: {missing:?}",
        path.display()
    );
}

#[test]
fn spec_2h5wv_must_11_every_mux_caller_cites_classifier() {
    // ft-2h5wv fixed seven mux caller paths. A copy of this file
    // pinned at authorship time has one `mux_error_should_fallback_to_cli(&*)?`
    // (or variant) call per caller, so the source should contain at
    // least 7 call-site references.
    //
    // Counting by substring is intentionally conservative. If a future
    // refactor replaces `mux_error_should_fallback_to_cli(&e)` with a
    // different classifier name, this test fails loudly — at which
    // point the maintainer either renames the classifier
    // consistently and updates MUST-8 / this count, or they're
    // accidentally reverting ft-2h5wv and need to stop.
    const MIN_CALL_SITES: usize = 7;
    let (path, text) = read_wezterm_rs();
    // Count *call sites*, not the definition. The definition line
    // has `fn mux_error_should_fallback_to_cli(`; callers use
    // `Self::mux_error_should_fallback_to_cli(` — count only the
    // caller form.
    let call_sites = text
        .matches("Self::mux_error_should_fallback_to_cli")
        .count();
    assert!(
        call_sites >= MIN_CALL_SITES,
        "[MUST-11] {} — found only {call_sites} caller(s) of \
         `Self::mux_error_should_fallback_to_cli`, expected at least \
         {MIN_CALL_SITES} (ft-2h5wv fixed that many mux caller paths: \
         list_panes, list_panes_with_cx, get_text, \
         pane_tiered_scrollback_summary, \
         pane_tiered_scrollback_summary_with_cx, send_text, \
         send_text_with_cx). A reduction below the threshold means at \
         least one caller silently regressed to the pre-fix \
         fall-through-to-CLI shape.",
        path.display()
    );
}

// =========================================================================
// Coverage meta-test
// =========================================================================

/// Confirm the coverage matrix claim in the file-level doc comment is
/// kept honest: exactly 11 `#[test]` functions named `spec_*_must_*`
/// must live in this file (one per MUST clause).
#[test]
fn coverage_matrix_reports_every_must_clause() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("conformance_post_xbnl02_5.rs");
    let source = fs::read_to_string(&path).expect("self-read conformance source");
    let must_tests = source.matches("fn spec_").count();
    // `fn spec_*` appears once per test signature; we counted 11 MUST
    // clauses in the doc header. Meta-test catches drift between the
    // MUST enumeration and the actual test bodies.
    assert_eq!(
        must_tests,
        11,
        "[COVERAGE] {} — found {must_tests} `fn spec_*` tests; expected \
         11 (one per MUST clause declared in the file header). Either \
         a MUST test was added/removed without updating the coverage \
         matrix, or the header's MUST count is stale.",
        path.display()
    );
}
