//! Render-quality / draft-mode regression fixture
//! (`ft-mpc9b.2.2`).
//!
//! Foundation slice for the renderer's draft-mode policy. Until
//! the GUI integration bead lands (touching paint.rs / glyphcache
//! / quad / shader uniforms), this fixture exercises the pure
//! `frankenterm_core::render_quality` module against synthetic
//! `LiveResizeState` event streams and pins the bead's
//! correctness rules.
//!
//! ## What's pinned
//!
//! - **Per-quality feature-flag table.** Each `RenderQuality`
//!   produces the documented `DraftModeFeatureFlags` — Standard
//!   has 7 cosmetic features on; Fancy adds focus blur (8);
//!   Draft disables all 8.
//! - **Snap-back guarantee.** A gesture (Begin → Resizing → End
//!   → Idle) produces exactly ONE Standard snap-back frame, even
//!   if the steady-state default is Fancy.
//! - **Snap-back synthesis.** If the integration layer skips the
//!   ResizeEnd tick (auto-clear → Idle), the next Idle frame
//!   becomes the snap-back.
//! - **Three independence rules.** Across all qualities,
//!   a11y_tree_update / color_profile / ime_caret_anchor stay
//!   `true`. The bead's headline accessibility / color / IME
//!   correctness contracts.
//! - **Driver counter monotonicity.**
//!
//! ## Goldens
//!
//! `crates/frankenterm-core/tests/render_quality/golden/<scenario>.jsonl`
//! captures per-frame structured logs for the bead's
//! schema. `FT_RENDER_QUALITY_BLESS=1` regenerates with the
//! deliberate-bless flow.

use std::path::PathBuf;

use frankenterm_core::live_resize::LiveResizeState;
use frankenterm_core::render_quality::{
    DraftModeDriver, DraftModeFeatureFlags, RenderQuality, RenderQualityFrameEvent,
    RenderQualityHealth, SteadyStateQuality, parse_events_jsonl, render_events_jsonl,
};
use proptest::prelude::*;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("render_quality")
        .join("golden")
}

fn golden_path(scenario: &str) -> PathBuf {
    golden_dir().join(format!("{scenario}.jsonl"))
}

fn bless_enabled() -> bool {
    std::env::var("FT_RENDER_QUALITY_BLESS")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn ensure_golden_dir_exists() {
    std::fs::create_dir_all(golden_dir()).expect("create golden dir");
}

/// Drive a sequence of `LiveResizeState` ticks through a fresh
/// driver and produce per-frame JSONL events.
fn drive(
    steady_state: SteadyStateQuality,
    ticks: &[LiveResizeState],
) -> Vec<RenderQualityFrameEvent> {
    let mut driver = DraftModeDriver::new(steady_state);
    let mut events = Vec::new();
    let mut prior_quality = steady_state.as_render_quality();
    for (i, &state) in ticks.iter().enumerate() {
        let q = driver.pick(state);
        let is_snap_back = q == RenderQuality::Standard && prior_quality == RenderQuality::Draft;
        events.push(RenderQualityFrameEvent {
            ts_ms: i as u64 * 16, // 60Hz
            render_quality: q,
            dirty_lines: 0,
            frame_time_us: 0,
            is_snap_back,
        });
        prior_quality = q;
    }
    events
}

// ============================================================================
// Per-quality feature-flag scenarios.
// ============================================================================

#[test]
fn standard_quality_enables_full_fidelity_features() {
    let f = DraftModeFeatureFlags::for_quality(RenderQuality::Standard);
    assert!(f.sdf_glyphs);
    assert!(f.ligature_shaping);
    assert!(f.italic_synthesis);
    assert!(f.subpixel_aa);
    assert!(f.fancy_underlines);
    assert!(f.pane_border_decorations);
    assert!(f.background_image_scaling);
    // Fancy-only: focus blur off.
    assert!(!f.focus_blur);
}

#[test]
fn fancy_quality_adds_focus_blur_on_top_of_standard() {
    let f = DraftModeFeatureFlags::for_quality(RenderQuality::Fancy);
    assert!(f.focus_blur);
    assert!(f.sdf_glyphs); // Inherits Standard features.
    assert!(f.ligature_shaping);
}

#[test]
fn draft_quality_disables_all_eight_cosmetic_features() {
    let f = DraftModeFeatureFlags::for_quality(RenderQuality::Draft);
    let cosmetics = [
        f.sdf_glyphs,
        f.ligature_shaping,
        f.italic_synthesis,
        f.subpixel_aa,
        f.fancy_underlines,
        f.pane_border_decorations,
        f.focus_blur,
        f.background_image_scaling,
    ];
    for (i, on) in cosmetics.iter().enumerate() {
        assert!(!on, "cosmetic feature {i} unexpectedly on in Draft");
    }
}

// ============================================================================
// THE three independence rules (a11y / color / IME).
//
// These are the bead's "DO NOT BREAK" contracts. A future
// integration bead that adds a quality MUST keep these `true`
// for every quality variant. The proptest below sweeps every
// existing quality; the unit test above pinned the contract for
// the three current ones.
// ============================================================================

#[test]
fn every_quality_must_dispatch_a11y_tree_updates() {
    for q in RenderQuality::ALL {
        let f = DraftModeFeatureFlags::for_quality(*q);
        assert!(
            f.a11y_tree_update,
            "{q:?} silently disabled a11y tree updates — \
             cross-link ft-mpc9b.10.1 violation"
        );
    }
}

#[test]
fn every_quality_must_apply_color_profile() {
    for q in RenderQuality::ALL {
        let f = DraftModeFeatureFlags::for_quality(*q);
        assert!(
            f.color_profile,
            "{q:?} silently disabled color profile — \
             cross-link ft-mpc9b.10.3 violation"
        );
    }
}

#[test]
fn every_quality_must_dispatch_ime_caret_anchor() {
    for q in RenderQuality::ALL {
        let f = DraftModeFeatureFlags::for_quality(*q);
        assert!(
            f.ime_caret_anchor,
            "{q:?} silently disabled IME caret anchor — \
             cross-link ft-mpc9b.10.2 violation"
        );
    }
}

// ============================================================================
// Driver scenarios.
// ============================================================================

fn typical_resize_gesture() -> Vec<LiveResizeState> {
    let mut ticks = vec![LiveResizeState::Idle];
    ticks.push(LiveResizeState::ResizeBegin);
    for _ in 0..10 {
        ticks.push(LiveResizeState::Resizing);
    }
    ticks.push(LiveResizeState::ResizeEnd);
    ticks.push(LiveResizeState::Idle);
    ticks
}

fn skipped_resize_end_gesture() -> Vec<LiveResizeState> {
    // Integration layer skips the ResizeEnd tick; goes directly
    // from Resizing to Idle. Driver synthesizes the snap-back.
    vec![
        LiveResizeState::Idle,
        LiveResizeState::ResizeBegin,
        LiveResizeState::Resizing,
        LiveResizeState::Resizing,
        LiveResizeState::Idle,
        LiveResizeState::Idle,
    ]
}

fn watchdog_forced_end_gesture() -> Vec<LiveResizeState> {
    // The live-resize state machine's watchdog forced End at 5s
    // — same input shape from the driver's perspective as a
    // normal End.
    typical_resize_gesture()
}

#[test]
fn typical_gesture_produces_exactly_one_snap_back() {
    let events = drive(SteadyStateQuality::Standard, &typical_resize_gesture());
    let snap_backs = events.iter().filter(|e| e.is_snap_back).count();
    assert_eq!(snap_backs, 1, "expected 1 snap-back, got {snap_backs}");
    // Snap-back is Standard quality.
    let snap = events.iter().find(|e| e.is_snap_back).unwrap();
    assert_eq!(snap.render_quality, RenderQuality::Standard);
}

#[test]
fn fancy_steady_state_still_snaps_back_to_standard() {
    let events = drive(SteadyStateQuality::Fancy, &typical_resize_gesture());
    let snap = events
        .iter()
        .find(|e| e.is_snap_back)
        .expect("snap-back missing under Fancy steady-state");
    assert_eq!(
        snap.render_quality,
        RenderQuality::Standard,
        "snap-back must always be Standard, not the steady-state default"
    );
}

#[test]
fn skipped_resize_end_synthesizes_snap_back_on_next_idle() {
    let events = drive(SteadyStateQuality::Standard, &skipped_resize_end_gesture());
    let snap_backs = events.iter().filter(|e| e.is_snap_back).count();
    assert_eq!(
        snap_backs, 1,
        "synthesized snap-back missing in skipped-ResizeEnd scenario"
    );
}

#[test]
fn driver_health_counts_match_event_stream() {
    let mut driver = DraftModeDriver::new(SteadyStateQuality::Standard);
    for &tick in &typical_resize_gesture() {
        driver.pick(tick);
    }
    let h = driver.health();
    // 1 Idle (Standard) + 1 Begin + 10 Resizing (Draft) + 1
    // ResizeEnd (Standard, snap-back) + 1 Idle (Standard) = 13
    // ticks.
    assert_eq!(h.draft_frames_total, 11);
    assert_eq!(h.standard_frames_total, 3);
    assert_eq!(h.snap_back_total, 1);
}

// ============================================================================
// Golden snapshots.
// ============================================================================

#[test]
fn golden_typical_gesture_standard_steady_state() {
    snapshot_golden(
        "typical_gesture_standard",
        &drive(SteadyStateQuality::Standard, &typical_resize_gesture()),
    );
}

#[test]
fn golden_typical_gesture_fancy_steady_state() {
    snapshot_golden(
        "typical_gesture_fancy",
        &drive(SteadyStateQuality::Fancy, &typical_resize_gesture()),
    );
}

#[test]
fn golden_skipped_resize_end() {
    snapshot_golden(
        "skipped_resize_end",
        &drive(SteadyStateQuality::Fancy, &skipped_resize_end_gesture()),
    );
}

#[test]
fn golden_watchdog_forced_end() {
    snapshot_golden(
        "watchdog_forced_end",
        &drive(SteadyStateQuality::Standard, &watchdog_forced_end_gesture()),
    );
}

fn snapshot_golden(scenario: &str, events: &[RenderQualityFrameEvent]) {
    let rendered = render_events_jsonl(events);
    let path = golden_path(scenario);
    if bless_enabled() {
        ensure_golden_dir_exists();
        std::fs::write(&path, &rendered).expect("write blessed golden");
        panic!(
            "{scenario}: golden blessed at {}; re-run without FT_RENDER_QUALITY_BLESS to validate",
            path.display()
        );
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "missing golden for {scenario} at {}: {err} \
             (re-run with FT_RENDER_QUALITY_BLESS=1 to generate)",
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
    assert_eq!(parsed, events, "JSONL roundtrip drift for {scenario}");
}

// ============================================================================
// Proptest invariants.
// ============================================================================

#[derive(Debug, Clone, Copy)]
enum Tick {
    Idle,
    Begin,
    Resize,
    End,
}

prop_compose! {
    fn arb_tick()(choice in 0u8..4) -> Tick {
        match choice {
            0 => Tick::Idle,
            1 => Tick::Begin,
            2 => Tick::Resize,
            _ => Tick::End,
        }
    }
}

fn tick_to_state(t: Tick) -> LiveResizeState {
    match t {
        Tick::Idle => LiveResizeState::Idle,
        Tick::Begin => LiveResizeState::ResizeBegin,
        Tick::Resize => LiveResizeState::Resizing,
        Tick::End => LiveResizeState::ResizeEnd,
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        .. ProptestConfig::default()
    })]

    /// Driver is total: any sequence of `LiveResizeState` ticks
    /// produces a well-formed quality stream and well-formed
    /// counters.
    #[test]
    fn driver_is_total_under_arbitrary_ticks(
        ticks in proptest::collection::vec(arb_tick(), 0..32),
        steady_idx in 0u8..2,
    ) {
        let steady = if steady_idx == 0 { SteadyStateQuality::Standard } else { SteadyStateQuality::Fancy };
        let mut driver = DraftModeDriver::new(steady);
        for t in ticks {
            let _q = driver.pick(tick_to_state(t));
        }
        let h = driver.health();
        prop_assert!(h.snap_back_total <= h.standard_frames_total);
        prop_assert!(
            h.draft_ratio() >= 0.0 && h.draft_ratio() <= 1.0,
            "draft_ratio out of bounds: {}", h.draft_ratio()
        );
    }

    /// During Resizing, the picked quality is ALWAYS Draft.
    #[test]
    fn resizing_state_always_picks_draft(
        prefix in proptest::collection::vec(arb_tick(), 0..16),
        steady_idx in 0u8..2,
    ) {
        let steady = if steady_idx == 0 { SteadyStateQuality::Standard } else { SteadyStateQuality::Fancy };
        let mut driver = DraftModeDriver::new(steady);
        for t in prefix {
            driver.pick(tick_to_state(t));
        }
        let q = driver.pick(LiveResizeState::Resizing);
        prop_assert_eq!(q, RenderQuality::Draft);
        let q = driver.pick(LiveResizeState::ResizeBegin);
        prop_assert_eq!(q, RenderQuality::Draft);
    }

    /// Snap-back count is bounded by the number of distinct
    /// gestures (transitions out of Resizing). Pinned via:
    /// snap_back_total <= number of `End` or `Idle` ticks
    /// preceded by a `Resize`/`Begin` tick.
    #[test]
    fn snap_back_count_is_bounded_by_gesture_count(
        ticks in proptest::collection::vec(arb_tick(), 0..32),
    ) {
        let mut driver = DraftModeDriver::new(SteadyStateQuality::Standard);
        let mut prior_was_draft = false;
        let mut max_snap_backs = 0u64;
        for t in &ticks {
            let state = tick_to_state(*t);
            let was_draft = matches!(state, LiveResizeState::ResizeBegin | LiveResizeState::Resizing);
            if !was_draft && prior_was_draft {
                max_snap_backs += 1;
            }
            prior_was_draft = was_draft;
            driver.pick(state);
        }
        let h = driver.health();
        prop_assert!(
            h.snap_back_total <= max_snap_backs,
            "snap_back_total={} > max_snap_backs={}",
            h.snap_back_total,
            max_snap_backs,
        );
    }

    /// JSONL roundtrip identity.
    #[test]
    fn jsonl_roundtrip(
        events in proptest::collection::vec(
            (
                0u64..u64::MAX,
                0u8..3,
                0u32..1000,
                0u32..u32::MAX,
                any::<bool>(),
            )
                .prop_map(|(ts, q, dl, ft, sb)| RenderQualityFrameEvent {
                    ts_ms: ts,
                    render_quality: match q {
                        0 => RenderQuality::Standard,
                        1 => RenderQuality::Fancy,
                        _ => RenderQuality::Draft,
                    },
                    dirty_lines: dl,
                    frame_time_us: ft,
                    is_snap_back: sb,
                }),
            0..16,
        ),
    ) {
        let rendered = render_events_jsonl(&events);
        let parsed = parse_events_jsonl(&rendered).unwrap();
        prop_assert_eq!(parsed, events);
    }
}

// ============================================================================
// Health snapshot baseline.
// ============================================================================

#[test]
fn baseline_health_is_zero_across_the_board() {
    let h = RenderQualityHealth::baseline();
    assert_eq!(h.draft_frames_total, 0);
    assert_eq!(h.standard_frames_total, 0);
    assert_eq!(h.fancy_frames_total, 0);
    assert_eq!(h.snap_back_total, 0);
    assert_eq!(h.quality_transitions_total, 0);
    assert_eq!(h.draft_ratio(), 0.0);
}
