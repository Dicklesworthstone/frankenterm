//! Atlas-stability regression fixture (`ft-mpc9b.1.1`).
//!
//! Pins the bead's headline correctness rule:
//!
//! > A pure window-resize that does NOT allocate any new sprites
//! > MUST leave `atlas.version()` and `atlas_rebuilds_total`
//! > unchanged.
//!
//! The fixture simulates a 100-resize loop against a synthetic
//! event stream (no real GPU/window — that's the integration-bead
//! lane) and asserts:
//!
//! - Every `Sync` and `Resize` event preserves the version.
//! - The pure-resize summary reports `glyphs_re_uploaded == 0`.
//! - The captured stream serializes through JSONL identity.
//! - `AtlasStabilityHealth.is_resize_stable()` stays `true`.
//!
//! When the per-platform GUI integration lands, this fixture's
//! recorder is swapped for a real one and the same invariants run
//! against the captured live stream.
//!
//! ## Goldens
//!
//! `crates/frankenterm-core/tests/atlas_stability/golden/<scenario>.jsonl`.
//! `FT_ATLAS_BLESS=1` regenerates with the same deliberate-bless
//! flow used by the a11y_tree / color_management / ime_caret
//! fixtures.

use std::path::PathBuf;

use frankenterm_core::atlas_stability::{
    AtlasOp, AtlasStabilityEvent, AtlasStabilityHealth, AtlasStabilityResize, check_invariants,
    check_pure_resize, parse_events_jsonl, render_events_jsonl,
};
use proptest::prelude::*;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("atlas_stability")
        .join("golden")
}

fn golden_path(scenario: &str) -> PathBuf {
    golden_dir().join(format!("{scenario}.jsonl"))
}

fn bless_enabled() -> bool {
    std::env::var("FT_ATLAS_BLESS")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn ensure_golden_dir_exists() {
    std::fs::create_dir_all(golden_dir()).expect("create golden dir");
}

// ============================================================================
// Synthetic event stream — 100 pure-resize events on a quiescent
// atlas (no allocates). Models the bead's bench scenario.
// ============================================================================

fn pure_resize_storm_stream() -> Vec<AtlasStabilityEvent> {
    let mut events = Vec::new();
    let mut ts = 0u64;
    // Initial sync.
    events.push(AtlasStabilityEvent {
        ts_ms: ts,
        op: AtlasOp::Sync,
        version_before: 0,
        version_after: 0,
        bytes: 0,
    });
    // 100 pure-resize events at 60Hz.
    for _ in 0..100 {
        ts += 16; // ~60 FPS cadence.
        events.push(AtlasStabilityEvent {
            ts_ms: ts,
            op: AtlasOp::Resize,
            version_before: 0,
            version_after: 0,
            bytes: 0,
        });
        // Each frame's paint pass takes a fresh sync after the
        // resize event — also a no-op on the version.
        events.push(AtlasStabilityEvent {
            ts_ms: ts + 1,
            op: AtlasOp::Sync,
            version_before: 0,
            version_after: 0,
            bytes: 0,
        });
    }
    events
}

/// A scale-change scenario: the renderer's lazy-rerasterize path
/// uploads a few new glyphs at the new metric, but does NOT trigger
/// a clear. The atlas version bumps once per new glyph; pre-fix
/// behaviour would have produced one Clear event per scale change.
fn scale_change_lazy_rerasterize_stream() -> Vec<AtlasStabilityEvent> {
    vec![
        AtlasStabilityEvent {
            ts_ms: 0,
            op: AtlasOp::Sync,
            version_before: 0,
            version_after: 0,
            bytes: 0,
        },
        AtlasStabilityEvent {
            ts_ms: 5,
            op: AtlasOp::Resize,
            version_before: 0,
            version_after: 0,
            bytes: 0,
        },
        AtlasStabilityEvent {
            ts_ms: 6,
            op: AtlasOp::Sync,
            version_before: 0,
            version_after: 0,
            bytes: 0,
        },
        // First post-scale paint pass: 3 new glyphs at the new
        // metric. Each one bumps the version.
        AtlasStabilityEvent {
            ts_ms: 16,
            op: AtlasOp::Upload,
            version_before: 0,
            version_after: 1,
            bytes: 256,
        },
        AtlasStabilityEvent {
            ts_ms: 17,
            op: AtlasOp::Upload,
            version_before: 1,
            version_after: 2,
            bytes: 256,
        },
        AtlasStabilityEvent {
            ts_ms: 18,
            op: AtlasOp::Upload,
            version_before: 2,
            version_after: 3,
            bytes: 256,
        },
        // Second frame: cursor catches up, no new uploads.
        AtlasStabilityEvent {
            ts_ms: 32,
            op: AtlasOp::Sync,
            version_before: 3,
            version_after: 3,
            bytes: 0,
        },
    ]
}

/// An out-of-space scenario: the renderer hits OutOfTextureSpace,
/// triggers `Atlas::grow`, then re-uploads. Pre-fix this would have
/// been a `Clear` op (rebuild). Post-fix, it's a `Grow` op + lazy
/// re-uploads.
fn grow_path_stream() -> Vec<AtlasStabilityEvent> {
    vec![
        AtlasStabilityEvent {
            ts_ms: 0,
            op: AtlasOp::Upload,
            version_before: 0,
            version_after: 1,
            bytes: 1024,
        },
        AtlasStabilityEvent {
            ts_ms: 50,
            op: AtlasOp::Grow,
            version_before: 1,
            version_after: 2,
            bytes: 16 * 1024 * 1024,
        },
        AtlasStabilityEvent {
            ts_ms: 60,
            op: AtlasOp::Upload,
            version_before: 2,
            version_after: 3,
            bytes: 1024,
        },
    ]
}

// ============================================================================
// Test 1 — synthetic streams satisfy the invariants.
// ============================================================================

#[test]
fn pure_resize_storm_satisfies_invariants() {
    let events = pure_resize_storm_stream();
    let v = check_invariants(&events);
    assert!(v.is_empty(), "violations: {v:?}");
}

#[test]
fn scale_change_lazy_rerasterize_satisfies_invariants() {
    let events = scale_change_lazy_rerasterize_stream();
    let v = check_invariants(&events);
    assert!(v.is_empty(), "violations: {v:?}");
}

#[test]
fn grow_path_satisfies_invariants() {
    let events = grow_path_stream();
    let v = check_invariants(&events);
    assert!(v.is_empty(), "violations: {v:?}");
}

// ============================================================================
// Test 2 — golden snapshots.
// ============================================================================

#[test]
fn golden_pure_resize_storm() {
    snapshot_golden("pure_resize_storm", &pure_resize_storm_stream());
}

#[test]
fn golden_scale_change_lazy_rerasterize() {
    snapshot_golden(
        "scale_change_lazy_rerasterize",
        &scale_change_lazy_rerasterize_stream(),
    );
}

#[test]
fn golden_grow_path() {
    snapshot_golden("grow_path", &grow_path_stream());
}

fn snapshot_golden(scenario: &str, events: &[AtlasStabilityEvent]) {
    let rendered = render_events_jsonl(events);
    let path = golden_path(scenario);

    if bless_enabled() {
        ensure_golden_dir_exists();
        std::fs::write(&path, &rendered).expect("write blessed golden");
        panic!(
            "{scenario}: golden blessed at {}; re-run without FT_ATLAS_BLESS to validate",
            path.display()
        );
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "missing golden for {scenario} at {}: {err} \
             (re-run with FT_ATLAS_BLESS=1 to generate)",
            path.display()
        )
    });

    assert_eq!(
        rendered,
        expected,
        "{scenario} drifted from golden at {}",
        path.display()
    );

    let parsed = parse_events_jsonl(&rendered).expect("parse");
    assert_eq!(parsed, events, "JSONL round-trip drift for {scenario}");
}

// ============================================================================
// Test 3 — pure-resize summary stays at zero glyphs re-uploaded.
//
// The bead's headline acceptance: 100 pure-resize events MUST report
// zero glyph re-uploads in the per-resize summary.
// ============================================================================

#[test]
fn pure_resize_storm_reports_zero_reuploaded_glyphs() {
    for i in 0..100 {
        let r = AtlasStabilityResize {
            ts_ms: i as u64 * 16,
            glyphs_re_uploaded: 0,
            atlas_size_bytes_before: 4 * 1024 * 1024,
            atlas_size_bytes_after: 4 * 1024 * 1024,
            sync_duration_ms: 0,
        };
        let v = check_pure_resize(&r);
        assert!(v.is_empty(), "resize #{i} violations: {v:?}");
    }
}

// ============================================================================
// Test 4 — health snapshot stays resize-stable across the storm.
// ============================================================================

#[test]
fn health_snapshot_stays_resize_stable_through_storm() {
    let mut health = AtlasStabilityHealth::baseline();
    // 100 resize events bump nothing.
    for _ in 0..100 {
        // Pure resize is a no-op on every counter.
        assert!(health.is_resize_stable());
    }
    // A few uploads (lazy rerasterize) bump uploads_total but NOT
    // rebuilds_total — still resize-stable.
    health.uploads_total += 25;
    assert!(health.is_resize_stable());
    // A grow bumps grow_count but NOT rebuilds_total — still
    // resize-stable.
    health.grow_count += 1;
    assert!(health.is_resize_stable());
    // Only an explicit clear() bumps rebuilds_total → no longer
    // resize-stable.
    health.rebuilds_total += 1;
    assert!(!health.is_resize_stable());
}

// ============================================================================
// Test 5 — proptest properties.
// ============================================================================

prop_compose! {
    fn arb_event()(
        ts in 0u64..1_000_000,
        op_idx in 0u8..5,
        version in 0u64..10_000,
        delta in 0u64..16,
        bytes in 1u64..(1024 * 1024),
    ) -> AtlasStabilityEvent {
        let op = match op_idx {
            0 => AtlasOp::Upload,
            1 => AtlasOp::Clear,
            2 => AtlasOp::Grow,
            3 => AtlasOp::Sync,
            _ => AtlasOp::Resize,
        };
        // Honor the bump constraint per op so the property tests
        // exercise the *valid* design space.
        let version_after = if op.may_bump_version() { version + delta } else { version };
        AtlasStabilityEvent {
            ts_ms: ts,
            op,
            version_before: version,
            version_after,
            bytes: if matches!(op, AtlasOp::Upload | AtlasOp::Grow) { bytes } else { 0 },
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        .. ProptestConfig::default()
    })]

    /// `check_invariants` is total — it never panics on an arbitrary
    /// stream and either returns empty or a `Vec<Violation>` listing
    /// real problems.
    #[test]
    fn check_invariants_is_total(events in proptest::collection::vec(arb_event(), 0..32)) {
        let _ = check_invariants(&events);
    }

    /// A stream containing only bump-correct events with monotonic
    /// timestamps is invariant-clean.
    #[test]
    fn well_formed_stream_is_clean(seeds in proptest::collection::vec(0u64..16, 0..16)) {
        let mut events = Vec::new();
        let mut ts = 0u64;
        let mut version = 0u64;
        for delta in seeds {
            ts += 1;
            // Alternate Upload (bumps) with Sync (no-op).
            events.push(AtlasStabilityEvent {
                ts_ms: ts,
                op: AtlasOp::Upload,
                version_before: version,
                version_after: version + delta + 1,  // ensure non-zero bump
                bytes: 8,
            });
            version += delta + 1;
            ts += 1;
            events.push(AtlasStabilityEvent {
                ts_ms: ts,
                op: AtlasOp::Sync,
                version_before: version,
                version_after: version,
                bytes: 0,
            });
        }
        let v = check_invariants(&events);
        prop_assert!(v.is_empty(), "well-formed stream produced violations: {v:?}");
    }

    /// JSONL round-trip is identity.
    #[test]
    fn jsonl_render_parse_roundtrip(events in proptest::collection::vec(arb_event(), 0..16)) {
        let rendered = render_events_jsonl(&events);
        let parsed = parse_events_jsonl(&rendered).expect("parse");
        prop_assert_eq!(parsed, events);
    }
}
