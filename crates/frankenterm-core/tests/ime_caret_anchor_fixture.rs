//! IME caret-anchor regression fixture (`ft-mpc9b.10.2`).
//!
//! Foundation slice for the per-platform IME integration lane. Until
//! the per-platform recorder beads land (one each for macOS
//! NSTextInputClient, Linux text-input-v3, Linux XIM), this fixture
//! pins the canonical `ImeUpdate` sequences against the
//! `ContractRecorder`-style synthetic recorder, runs the proptest
//! invariants on the pure caret-rect math, and proves the
//! `should_dispatch_after_state_change` predicate is strictly
//! broader than the X11 / Wayland platform-side dedups.
//!
//! ## Goldens
//!
//! `crates/frankenterm-core/tests/ime/golden/synthetic-<scenario>.jsonl`
//! is the committed baseline. `FT_IME_BLESS=1` regenerates with the
//! same deliberate-bless flow used by the a11y_tree and
//! color_management fixtures.

use std::path::PathBuf;

use frankenterm_core::ime_caret::{
    CaretAnchorRect, CaretGeometry, ImeDispatchState, ImePlatform, ImeScenario, RenderQuality,
    compute_caret_anchor_rect, contract_updates, parse_updates_jsonl, render_updates_jsonl,
    should_dispatch_after_state_change,
};
use proptest::prelude::*;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("ime")
        .join("golden")
}

fn golden_path(scenario: ImeScenario) -> PathBuf {
    golden_dir().join(ImePlatform::Synthetic.golden_filename(scenario))
}

fn bless_enabled() -> bool {
    std::env::var("FT_IME_BLESS")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn ensure_golden_dir_exists() {
    std::fs::create_dir_all(golden_dir()).expect("create golden dir");
}

// ============================================================================
// Test 1 — every scenario produces a non-empty stream and every
// update is dispatched (no silent elides).
// ============================================================================

#[test]
fn every_scenario_dispatches_every_update() {
    for scenario in ImeScenario::ALL {
        let updates = contract_updates(*scenario);
        assert!(!updates.is_empty(), "{scenario:?} produced empty stream");
        for u in &updates {
            assert!(
                u.dispatched,
                "{scenario:?}: update {u:?} reports dispatched=false; the bead's \
                 correctness rule is that no scenario silently elides a caret update"
            );
        }
    }
}

// ============================================================================
// Test 2 — golden snapshot per scenario.
// ============================================================================

#[test]
fn golden_synthetic_typing() {
    snapshot_golden(ImeScenario::Typing);
}

#[test]
fn golden_synthetic_draft_quality_burst() {
    snapshot_golden(ImeScenario::DraftQualityBurst);
}

#[test]
fn golden_synthetic_live_resize() {
    snapshot_golden(ImeScenario::LiveResize);
}

#[test]
fn golden_synthetic_idle_wakeup() {
    snapshot_golden(ImeScenario::IdleWakeup);
}

#[test]
fn golden_synthetic_focus_change() {
    snapshot_golden(ImeScenario::FocusChange);
}

fn snapshot_golden(scenario: ImeScenario) {
    let updates = contract_updates(scenario);
    let rendered = render_updates_jsonl(&updates);
    let path = golden_path(scenario);

    if bless_enabled() {
        ensure_golden_dir_exists();
        std::fs::write(&path, &rendered).expect("write blessed golden");
        panic!(
            "{}: golden blessed at {}; re-run without FT_IME_BLESS to validate",
            scenario.slug(),
            path.display()
        );
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "missing golden for {scenario:?} at {}: {err} \
             (re-run with FT_IME_BLESS=1 to generate)",
            path.display()
        )
    });

    assert_eq!(
        rendered,
        expected,
        "{scenario:?} drifted from golden at {}",
        path.display()
    );

    // Round-trip sanity.
    let parsed = parse_updates_jsonl(&rendered).expect("parse rendered");
    assert_eq!(parsed, updates, "JSONL round-trip drift for {scenario:?}");
}

// ============================================================================
// Test 3 — the per-platform recorder is wired honestly.
// Sentinel that fires when an integration lands.
// ============================================================================

#[test]
fn only_synthetic_platform_is_wired_today() {
    assert!(ImePlatform::Synthetic.is_wired());
    for not_wired in [
        ImePlatform::MacosNsTextInput,
        ImePlatform::WaylandTextInputV3,
        ImePlatform::X11Xim,
    ] {
        assert!(
            !not_wired.is_wired(),
            "{not_wired:?} reports wired but no integration has landed"
        );
    }
}

// ============================================================================
// Test 4 — `should_dispatch_after_state_change` is strictly broader
// than the X11 / Wayland platform-side cell-rect dedups.
//
// Concrete: there exist (caret, state) pairs where the dedup would
// elide (cell-rect equal) but the corrected predicate fires (state
// changed). Pinning these proves the integration beads' fixes are
// observable.
// ============================================================================

#[test]
fn corrected_predicate_dominates_cell_rect_dedup() {
    let caret = CaretAnchorRect::new(40, 60, 8, 16);
    let s_idle = ImeDispatchState {
        window_screen_origin: (10, 20),
        window_size: (800, 600),
        render_quality: RenderQuality::Standard,
        was_idle: true,
    };
    let s_active = ImeDispatchState {
        was_idle: false,
        ..s_idle
    };

    // Cell-rect dedup says "skip"; corrected predicate says "fire"
    // (idle → active transition).
    assert!(should_dispatch_after_state_change(
        Some(caret),
        caret,
        Some(s_idle),
        s_active,
    ));

    // Window moved on screen — cell-rect dedup misses, corrected
    // predicate catches.
    let s_moved = ImeDispatchState {
        window_screen_origin: (200, 300),
        ..s_idle
    };
    assert!(should_dispatch_after_state_change(
        Some(caret),
        caret,
        Some(s_idle),
        s_moved,
    ));

    // Quality flip — same cell-rect, different RenderQuality.
    let s_draft = ImeDispatchState {
        render_quality: RenderQuality::Draft,
        ..s_idle
    };
    assert!(should_dispatch_after_state_change(
        Some(caret),
        caret,
        Some(s_idle),
        s_draft,
    ));
}

// ============================================================================
// Test 5 — proptest properties on the pure caret-rect math.
// ============================================================================

prop_compose! {
    fn arb_geometry()(
        cursor_cell_col in -32i64..256,
        cursor_cell_row in -32i64..256,
        pane_top_cell in 0i64..64,
        pane_left_cell in 0i64..64,
        physical_top in 0i64..1000,
        cell_w in 1i64..32,
        cell_h in 1i64..48,
        tab_h in 0i64..64,
        pad_l in 0i64..32,
        pad_t in 0i64..32,
    ) -> CaretGeometry {
        CaretGeometry {
            cursor_cell_col,
            cursor_cell_row,
            pane_top_cell,
            pane_left_cell,
            physical_top,
            cell_width_px: cell_w,
            cell_height_px: cell_h,
            tab_bar_height_px: tab_h,
            padding_left_px: pad_l,
            padding_top_px: pad_t,
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        .. ProptestConfig::default()
    })]

    /// `compute_caret_anchor_rect` is total: every well-formed
    /// geometry produces a finite rect. The clamp on negative
    /// cell-row positions and the saturating multiplications mean
    /// the function never panics.
    #[test]
    fn caret_rect_total(g in arb_geometry()) {
        let r = compute_caret_anchor_rect(g);
        prop_assert!(r.width as i64 == g.cell_width_px || r.width == 0);
        prop_assert!(r.height as i64 == g.cell_height_px || r.height == 0);
    }

    /// Y is monotonic in `cursor_cell_row` — moving the caret
    /// downward moves the anchor downward (no folding).
    #[test]
    fn caret_y_monotonic_in_row(
        mut g in arb_geometry(),
        delta in 1i64..32,
    ) {
        // Place the cursor strictly above physical_top so neither
        // call hits the max(0) clamp.
        g.physical_top = 0;
        g.cursor_cell_row = 10;
        g.pane_top_cell = 0;
        let lo = compute_caret_anchor_rect(g);
        g.cursor_cell_row = 10 + delta;
        let hi = compute_caret_anchor_rect(g);
        prop_assert!(
            hi.origin_y >= lo.origin_y,
            "caret y not monotonic: lo={} hi={} delta={delta}",
            lo.origin_y,
            hi.origin_y
        );
    }

    /// X is monotonic in `cursor_cell_col` for non-negative columns.
    #[test]
    fn caret_x_monotonic_in_col(
        mut g in arb_geometry(),
        delta in 1i64..32,
    ) {
        g.cursor_cell_col = 10;
        g.pane_left_cell = 0;
        let lo = compute_caret_anchor_rect(g);
        g.cursor_cell_col = 10 + delta;
        let hi = compute_caret_anchor_rect(g);
        prop_assert!(
            hi.origin_x >= lo.origin_x,
            "caret x not monotonic: lo={} hi={} delta={delta}",
            lo.origin_x,
            hi.origin_x
        );
    }

    /// Tab-bar height contributes one-for-one to the y offset when
    /// the caret is in the visible region.
    #[test]
    fn tab_bar_height_offsets_y(
        mut g in arb_geometry(),
        extra_tab_h in 0i64..32,
    ) {
        g.physical_top = 0;
        g.cursor_cell_row = 5;
        g.pane_top_cell = 0;
        g.tab_bar_height_px = 0;
        let no_tab = compute_caret_anchor_rect(g);
        g.tab_bar_height_px = extra_tab_h;
        let with_tab = compute_caret_anchor_rect(g);
        prop_assert_eq!(with_tab.origin_y - no_tab.origin_y, extra_tab_h);
    }
}

// ============================================================================
// Test 6 — JSONL round-trip totality.
// ============================================================================

#[test]
fn every_scenario_jsonl_round_trips() {
    for scenario in ImeScenario::ALL {
        let updates = contract_updates(*scenario);
        let rendered = render_updates_jsonl(&updates);
        let parsed = parse_updates_jsonl(&rendered)
            .unwrap_or_else(|err| panic!("{scenario:?} JSONL parse failed: {err}"));
        assert_eq!(parsed, updates, "{scenario:?} round-trip drift");
    }
}
