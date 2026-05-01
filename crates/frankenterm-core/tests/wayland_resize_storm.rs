//! Linux integration test scaffold for the Wayland resize-
//! storm reproducer
//! ([BR-TERM-EMULATOR-UPLIFT.3.2.cont] / `ft-28opz`).
//!
//! This test exercises the bead's enumerated reproducer:
//!
//! > Drag window edge rapidly back and forth for 5 seconds.
//! > Assert via the `chain_depth_peak()` accessor that depth
//! > never exceeded 1.
//!
//! Driving an actual ft window on Wayland is out of scope for
//! the always-on regression net (CI runners are typically
//! headless / non-Wayland). This file ships:
//!
//! 1. The **contract-side validation** — every reachable
//!    `ChainDepthBound` / `ResizeStormConfig` / `Tier-1
//!    coverage` assertion the harness consumes.
//! 2. The **harness shape** — a `verify_compositor` fn the
//!    Linux CI lane (or a manual operator) invokes with the
//!    observed peak; output is a `VerificationResult` that
//!    feeds the per-release JSON artifact.
//! 3. The **invariant assertion** — `assert_chain_depth_safe`
//!    is the load-bearing predicate the actual driver calls
//!    after the storm completes.
//!
//! When the operator runs `ft` on a real Linux Wayland host
//! and drives the storm, they call `verify_compositor` with
//! the observed peak and append the result to
//! `docs/security/wayland-frame-pacing-validation.md`.
//!
//! ## Future Linux-only test
//!
//! When CI grows a Wayland-capable runner (Ubuntu 24.04 +
//! `weston --headless`, mutter via `gnome-shell --headless`,
//! or sway via a virtual seat), the `#[cfg(target_os =
//! "linux")]` block below extends to actually drive ft and
//! assert the chain_depth_peak ≤ 1 bound at the end. Today
//! the test is target-agnostic: it validates the contract +
//! harness shape on every PR and serves as the slot the
//! Linux driver plugs into.

use frankenterm_core::wayland_compositor_matrix::{
    ChainDepthBound, CompositorIdentity, CompositorMatrixSnapshot, CompositorTier,
    FrameCallbackHealth, ResizeStormConfig, VerificationResult,
};

// ----------------------------------------------------------------------------
// Contract-side coverage
// ----------------------------------------------------------------------------

#[test]
fn every_tier1_compositor_appears_in_all_table() {
    // The compositor enum is closed; this test pins coverage
    // for the bead's "Tier-1 verification matrix" requirement.
    let tier1: Vec<_> = CompositorIdentity::ALL
        .iter()
        .copied()
        .filter(|c| c.tier() == CompositorTier::Tier1)
        .collect();
    assert_eq!(
        tier1.len(),
        3,
        "Tier-1 compositors are mutter / kwin / sway"
    );
}

#[test]
fn slug_uniqueness_across_compositors() {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    for c in CompositorIdentity::ALL {
        assert!(
            seen.insert(c.slug()),
            "duplicate compositor slug: {}",
            c.slug()
        );
    }
}

#[test]
fn resize_storm_reproducer_default_matches_bead_text() {
    // Bead enumerates "5 seconds" of rapid resize. The
    // default config materializes that.
    let cfg = ResizeStormConfig::default();
    assert_eq!(cfg.duration_ms, 5_000);
    // 60 events/s × 5s = 300 events — typical resize-event
    // density on a fast-scrub mouse drag.
    assert_eq!(cfg.total_events(), 300);
}

#[test]
fn pre_fix_bound_is_one() {
    // The bead's stated pre-fix target: structural guards at
    // window.rs lines 1070 / 1153 / 1192 bound chain depth
    // to ≤ 1 by inspection.
    assert_eq!(ChainDepthBound::PRE_FIX.peak_max, 1);
    assert!(ChainDepthBound::PRE_FIX.check(0));
    assert!(ChainDepthBound::PRE_FIX.check(1));
    assert!(!ChainDepthBound::PRE_FIX.check(2));
}

#[test]
fn post_fix_bound_is_two() {
    // The bead's stated post-fix target: chain depth stays
    // ≤ 2 transient during the swap (only relevant if option-
    // #1 reorder is ever shipped — currently NOT shipped).
    assert_eq!(ChainDepthBound::POST_FIX.peak_max, 2);
}

// ----------------------------------------------------------------------------
// Harness — what the Linux driver consumes
// ----------------------------------------------------------------------------

/// The load-bearing predicate the Linux driver calls after
/// the resize-storm reproducer completes. Asserts the
/// observed `chain_depth_peak` is within the configured bound.
///
/// Returns `VerificationResult` with `passed = true/false` so
/// the result can flow into the per-release JSON artifact.
fn verify_compositor(
    compositor: CompositorIdentity,
    version: &str,
    config: ResizeStormConfig,
    bound: ChainDepthBound,
    chain_depth_peak: u32,
) -> VerificationResult {
    VerificationResult::observed(compositor, version, config, bound, chain_depth_peak)
}

#[test]
fn verify_compositor_reports_pass_at_pre_fix_bound_zero_peak() {
    let r = verify_compositor(
        CompositorIdentity::Mutter,
        "mutter 47.2",
        ResizeStormConfig::default(),
        ChainDepthBound::PRE_FIX,
        0,
    );
    assert!(r.passed);
    assert_eq!(r.tier, CompositorTier::Tier1);
}

#[test]
fn verify_compositor_reports_pass_at_pre_fix_bound_one_peak() {
    // Peak = 1 is the boundary case: structural guards
    // permit one in-flight callback.
    let r = verify_compositor(
        CompositorIdentity::Sway,
        "sway 1.10",
        ResizeStormConfig::default(),
        ChainDepthBound::PRE_FIX,
        1,
    );
    assert!(r.passed);
}

#[test]
fn verify_compositor_reports_fail_above_pre_fix_bound() {
    // If peak ever climbs above 1 under pre-fix bounds, the
    // bead enumerates the option-#1 reorder fix. The harness
    // catches the regression; the Linux operator then ships
    // the fix per the bead's #3 action.
    let r = verify_compositor(
        CompositorIdentity::Kwin,
        "kwin 6.2",
        ResizeStormConfig::default(),
        ChainDepthBound::PRE_FIX,
        3,
    );
    assert!(!r.passed);
    assert_eq!(r.chain_depth_peak, 3);
}

#[test]
fn assert_chain_depth_safe_passes_at_one() {
    // Convenience predicate used by the future Linux driver:
    // call this immediately after the storm to short-circuit
    // a CI failure with a useful message.
    let h = FrameCallbackHealth {
        chain_depth_now: 0,
        chain_depth_peak: 1,
        resize_events_total: 300,
        depth_gt_one_observations_total: 0,
    };
    assert!(h.is_safe(ChainDepthBound::PRE_FIX));
}

#[test]
fn assert_chain_depth_safe_fails_above_bound() {
    let h = FrameCallbackHealth {
        chain_depth_now: 2,
        chain_depth_peak: 4,
        resize_events_total: 300,
        depth_gt_one_observations_total: 17,
    };
    assert!(!h.is_safe(ChainDepthBound::PRE_FIX));
    assert!(!h.is_safe(ChainDepthBound::POST_FIX));
}

// ----------------------------------------------------------------------------
// Matrix shape — the per-release JSON artifact contract
// ----------------------------------------------------------------------------

#[test]
fn matrix_snapshot_passes_when_all_tier1_clean() {
    let mut m = CompositorMatrixSnapshot::new();
    for c in [
        CompositorIdentity::Mutter,
        CompositorIdentity::Kwin,
        CompositorIdentity::Sway,
    ] {
        m.record(verify_compositor(
            c,
            "test-version",
            ResizeStormConfig::default(),
            ChainDepthBound::PRE_FIX,
            1,
        ));
    }
    assert!(m.all_tier1_passed());
    assert!(m.missing_tier1().is_empty());
    assert_eq!(m.bead, "ft-28opz");
}

#[test]
fn matrix_snapshot_serializes_to_per_release_artifact_shape() {
    let mut m = CompositorMatrixSnapshot::new();
    m.record(verify_compositor(
        CompositorIdentity::Mutter,
        "mutter 47.2",
        ResizeStormConfig::default(),
        ChainDepthBound::PRE_FIX,
        1,
    ));
    let json = serde_json::to_string_pretty(&m).expect("matrix serializes");
    // The JSON artifact is human-readable; sanity-check it
    // has the expected top-level keys.
    assert!(json.contains("\"bead\""));
    assert!(json.contains("\"results\""));
    assert!(json.contains("\"mutter\""));
    assert!(json.contains("\"chain_depth_peak\""));
}

// ----------------------------------------------------------------------------
// Linux-only driver hook (compile-only when target_os = "linux";
// no actual driver yet — the integration follow-on populates
// this).
// ----------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires Wayland compositor + ft binary; manual operator runs this"]
fn linux_resize_storm_against_running_ft_window() {
    // Operator-driven: the bead's manual verification matrix
    // calls this with a real ft window present. The test is
    // #[ignore]'d so it doesn't run in headless CI; an
    // operator runs it explicitly:
    //
    //     CARGO_TARGET_DIR=... \
    //     cargo test -p frankenterm-core --test wayland_resize_storm \
    //         linux_resize_storm_against_running_ft_window \
    //         --features asupersync-runtime --no-default-features \
    //         -- --ignored
    //
    // The driver:
    // 1. Spawn `ft` (or attach to a running instance via the
    //    GUI IPC seam).
    // 2. Read the compositor identity from `WAYLAND_COMPOSITOR`
    //    or `XDG_CURRENT_DESKTOP`.
    // 3. Run the resize-storm reproducer (vary window dim
    //    every ~17ms for 5s).
    // 4. Sample `frame_callback_chain_depth_peak()` from the
    //    GUI process via the doctor seam.
    // 5. Call verify_compositor; append to the matrix
    //    snapshot.
    //
    // This stub is the slot; the actual driver lands in the
    // integration follow-on bead (when the Linux Wayland CI
    // runner is provisioned).

    panic!("Linux Wayland resize-storm driver not yet implemented; see ft-28opz follow-on");
}

#[cfg(not(target_os = "linux"))]
#[test]
fn non_linux_targets_skip_the_resize_storm_driver_gracefully() {
    // Sanity: on macOS / Windows we can still validate the
    // contract layer (everything above this comment runs
    // here). The actual driver is Linux-only; this test
    // exists so non-Linux developers see "0 ignored, 0
    // failed" rather than a confusing skip.
    let cfg = ResizeStormConfig::default();
    assert!(cfg.total_events() > 0);
}
