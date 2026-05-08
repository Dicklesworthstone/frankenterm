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
//! ## Linux-only manual evidence test
//!
//! The `#[cfg(target_os = "linux")]` block below consumes
//! operator-captured evidence from a real Wayland session and
//! asserts the `chain_depth_peak <= 1` bound. When CI grows a
//! Wayland-capable runner (Ubuntu 24.04 + `weston --headless`,
//! mutter via `gnome-shell --headless`, or sway via a virtual
//! seat), that test can grow from evidence consumption into
//! driving the ft window directly. Today the always-on tests
//! validate the contract + harness shape on every PR.

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

fn compositor_slug_list() -> String {
    CompositorIdentity::ALL
        .iter()
        .map(|c| c.slug())
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_compositor_identity(raw: &str) -> Result<CompositorIdentity, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!(
            "empty compositor slug; expected one of: {}",
            compositor_slug_list()
        ));
    }

    let slug = trimmed.to_ascii_lowercase();
    CompositorIdentity::from_slug(&slug).ok_or_else(|| {
        format!(
            "unsupported compositor slug `{trimmed}`; expected one of: {}",
            compositor_slug_list()
        )
    })
}

fn parse_chain_depth_peak(raw: &str) -> Result<u32, String> {
    let trimmed = raw.trim();
    trimmed
        .parse::<u32>()
        .map_err(|err| format!("invalid chain depth peak `{trimmed}`: {err}"))
}

fn parse_chain_depth_bound(raw: Option<&str>) -> Result<ChainDepthBound, String> {
    let Some(raw) = raw else {
        return Ok(ChainDepthBound::PRE_FIX);
    };

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(ChainDepthBound::PRE_FIX);
    }

    match trimmed.to_ascii_lowercase().as_str() {
        "pre_fix" | "pre-fix" | "pre" | "1" => Ok(ChainDepthBound::PRE_FIX),
        "post_fix" | "post-fix" | "post" | "2" => Ok(ChainDepthBound::POST_FIX),
        other => {
            let peak_max = other.strip_prefix("peak_max=").unwrap_or(other);
            peak_max
                .parse::<u32>()
                .map(|peak_max| ChainDepthBound { peak_max })
                .map_err(|err| {
                    format!(
                        "unsupported chain depth bound `{trimmed}`: {err}; \
                         use pre_fix, post_fix, or peak_max=N"
                    )
                })
        }
    }
}

#[cfg(target_os = "linux")]
fn required_env(name: &str) -> String {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => value,
        Ok(_) => panic!("{name} must not be empty for the Wayland resize-storm evidence test"),
        Err(err) => {
            panic!("{name} is required for the ignored Linux resize-storm evidence test: {err}");
        }
    }
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

#[test]
fn parse_compositor_identity_accepts_case_and_spacing() {
    assert_eq!(
        parse_compositor_identity(" Mutter "),
        Ok(CompositorIdentity::Mutter)
    );
    assert_eq!(
        parse_compositor_identity("SWAY"),
        Ok(CompositorIdentity::Sway)
    );
}

#[test]
fn parse_compositor_identity_rejects_unknown_slug() {
    let err = parse_compositor_identity("gnome").expect_err("gnome is not a matrix slug");
    assert!(err.contains("unsupported compositor slug"));
    assert!(err.contains("mutter"));
}

#[test]
fn parse_chain_depth_peak_accepts_only_unsigned_integers() {
    assert_eq!(parse_chain_depth_peak(" 1 "), Ok(1));
    assert!(parse_chain_depth_peak("-1").is_err());
    assert!(parse_chain_depth_peak("1.5").is_err());
}

#[test]
fn parse_chain_depth_bound_defaults_to_pre_fix() {
    assert_eq!(parse_chain_depth_bound(None), Ok(ChainDepthBound::PRE_FIX));
    assert_eq!(
        parse_chain_depth_bound(Some("")),
        Ok(ChainDepthBound::PRE_FIX)
    );
}

#[test]
fn parse_chain_depth_bound_accepts_named_and_numeric_forms() {
    assert_eq!(
        parse_chain_depth_bound(Some("post-fix")),
        Ok(ChainDepthBound::POST_FIX)
    );
    assert_eq!(
        parse_chain_depth_bound(Some("peak_max=7")),
        Ok(ChainDepthBound { peak_max: 7 })
    );
    assert!(parse_chain_depth_bound(Some("wide-open")).is_err());
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
// Linux-only manual evidence hook. This is ignored because it requires a real
// Wayland compositor and ft window, but the test itself is executable today:
// operators feed it the observed chain-depth peak after running the storm.
// ----------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires Wayland compositor + ft binary; manual operator runs this"]
fn linux_resize_storm_against_running_ft_window() {
    // Operator-driven: the bead's manual verification matrix
    // calls this after sampling a real ft window. The test is
    // #[ignore]'d so it doesn't run in headless CI. An operator
    // runs it explicitly with evidence captured from the
    // release host:
    //
    //     FT_WAYLAND_RESIZE_STORM_COMPOSITOR=mutter \
    //     FT_WAYLAND_RESIZE_STORM_VERSION="$(mutter --version)" \
    //     FT_WAYLAND_RESIZE_STORM_CHAIN_DEPTH_PEAK="$PEAK" \
    //     cargo test -p frankenterm-core --test wayland_resize_storm \
    //         --no-default-features \
    //         linux_resize_storm_against_running_ft_window \
    //         -- --ignored --nocapture
    //
    // Optional: set FT_WAYLAND_RESIZE_STORM_BOUND=post_fix,
    // pre_fix, or peak_max=N. Unset defaults to pre_fix.

    let compositor_raw = required_env("FT_WAYLAND_RESIZE_STORM_COMPOSITOR");
    let version = required_env("FT_WAYLAND_RESIZE_STORM_VERSION");
    let peak_raw = required_env("FT_WAYLAND_RESIZE_STORM_CHAIN_DEPTH_PEAK");
    let bound_raw = std::env::var("FT_WAYLAND_RESIZE_STORM_BOUND").ok();

    let compositor = match parse_compositor_identity(&compositor_raw) {
        Ok(compositor) => compositor,
        Err(message) => panic!("{message}"),
    };
    let chain_depth_peak = match parse_chain_depth_peak(&peak_raw) {
        Ok(chain_depth_peak) => chain_depth_peak,
        Err(message) => panic!("{message}"),
    };
    let bound = match parse_chain_depth_bound(bound_raw.as_deref()) {
        Ok(bound) => bound,
        Err(message) => panic!("{message}"),
    };

    let result = verify_compositor(
        compositor,
        &version,
        ResizeStormConfig::default(),
        bound,
        chain_depth_peak,
    );
    let result_json =
        serde_json::to_string_pretty(&result).expect("verification result serializes");
    println!("{result_json}");

    assert!(
        result.passed,
        "resize-storm evidence failed for {} {}: chain_depth_peak={} exceeds bound peak_max={}",
        result.compositor.slug(),
        result.version,
        result.chain_depth_peak,
        result.bound.peak_max
    );
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
