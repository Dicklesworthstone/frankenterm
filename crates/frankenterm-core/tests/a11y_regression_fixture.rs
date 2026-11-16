//! Accessibility-tree regression fixture (`ft-mpc9b.10.1`).
//!
//! Foundation slice for the per-platform AT regression lane. The
//! shared harness in this file is what future per-platform integration
//! beads (macOS NSAccessibility, Linux AT-SPI) plug into. Until those
//! integrations land, the fixture operates against the in-memory
//! [`ContractRecorder`]: it pins the canonical "expected event"
//! sequence per scenario as a golden JSONL file, and proves the
//! recorder + invariant checker round-trip cleanly.
//!
//! When a real recorder lands, the only change required to this file
//! is swapping the recorder constructor and adding a per-platform
//! integration test path (under `#[cfg(target_os = …)]`) that imports
//! the existing scenario corpus + golden harness verbatim.
//!
//! ## Goldens
//!
//! Files at `crates/frankenterm-core/tests/a11y/golden/synthetic-<scenario>.jsonl`
//! are checked in. The `synthetic` slug names the recorder origin, not
//! the platform — once real recorders land, sibling files like
//! `macos-<scenario>.jsonl` will appear without disturbing the
//! synthetic baselines.
//!
//! Run with `FT_A11Y_BLESS=1` to regenerate the golden files when the
//! contract intentionally changes; the test panics with a clear
//! "blessed; re-run without `FT_A11Y_BLESS`" message so the bless
//! flow is two-step and deliberate.

use std::path::PathBuf;

use frankenterm_core::a11y_tree::{
    AccessibilityEvent, AccessibilityPlatform, AccessibilityRecorder, AccessibilityScenario,
    AnnouncePriority, ContractRecorder, JsonlLogWriter, check_invariants, contract_events,
};
use proptest::prelude::*;

const PANE_ID: u64 = 42;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("a11y")
        .join("golden")
}

fn golden_path(scenario: AccessibilityScenario) -> PathBuf {
    golden_dir().join(AccessibilityPlatform::Synthetic.golden_filename(scenario))
}

/// Bless mode: regenerate golden files from `contract_events`.
fn bless_enabled() -> bool {
    std::env::var("FT_A11Y_BLESS")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn ensure_golden_dir_exists() {
    std::fs::create_dir_all(golden_dir()).expect("create golden dir");
}

// ============================================================================
// Test 1 — every contract scenario satisfies the invariants on its own.
// (Meta-test: caught a regression when a future bead edits `contract_events`
// without re-checking invariants.)
// ============================================================================

#[test]
fn every_contract_scenario_passes_invariants() {
    for scenario in AccessibilityScenario::ALL {
        let events = contract_events(*scenario, PANE_ID);
        let v = check_invariants(*scenario, &events);
        assert!(
            v.is_empty(),
            "{:?} contract violates invariants: {v:?}",
            scenario
        );
    }
}

// ============================================================================
// Test 2 — golden snapshot per scenario.
// ============================================================================

#[test]
fn golden_synthetic_steady_typing() {
    snapshot_golden(AccessibilityScenario::SteadyTyping);
}

#[test]
fn golden_synthetic_pane_focus_change() {
    snapshot_golden(AccessibilityScenario::PaneFocusChange);
}

#[test]
fn golden_synthetic_dialog_open() {
    snapshot_golden(AccessibilityScenario::DialogOpen);
}

#[test]
fn golden_synthetic_selection_change() {
    snapshot_golden(AccessibilityScenario::SelectionChange);
}

#[test]
fn golden_synthetic_scroll_position_change() {
    snapshot_golden(AccessibilityScenario::ScrollPositionChange);
}

fn snapshot_golden(scenario: AccessibilityScenario) {
    let mut recorder = ContractRecorder::new();
    recorder.start(scenario);
    for event in contract_events(scenario, PANE_ID) {
        recorder.record(event);
    }
    let captured = recorder.finish();
    let rendered = JsonlLogWriter::render(&captured);
    let path = golden_path(scenario);

    if bless_enabled() {
        ensure_golden_dir_exists();
        std::fs::write(&path, &rendered).expect("write blessed golden");
        panic!(
            "{}: golden blessed at {}; re-run without FT_A11Y_BLESS to validate",
            scenario.slug(),
            path.display()
        );
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "missing golden for {scenario:?} at {}: {err} \
             (re-run with FT_A11Y_BLESS=1 to generate)",
            path.display()
        )
    });

    assert_eq!(
        rendered,
        expected,
        "{scenario:?} drifted from golden at {}",
        path.display()
    );

    // Sanity: the captured stream MUST round-trip through JSONL.
    let parsed = JsonlLogWriter::parse(&rendered).expect("parse rendered jsonl");
    assert_eq!(parsed, captured, "JSONL round-trip drift");
}

// ============================================================================
// Test 3 — the `Synthetic` recorder is wired; real recorders aren't.
// Proves the platform-metadata API stays honest as integrations land.
// ============================================================================

#[test]
fn only_synthetic_platform_is_wired_today() {
    assert!(AccessibilityPlatform::Synthetic.is_wired());
    for not_wired in [
        AccessibilityPlatform::MacosNsAccessibility,
        AccessibilityPlatform::LinuxAtSpi,
        AccessibilityPlatform::WindowsUiAutomation,
    ] {
        assert!(
            !not_wired.is_wired(),
            "{:?} reports wired but no integration has landed",
            not_wired
        );
    }
}

// ============================================================================
// Test 4 — proptest invariants.
// Random event streams MUST either trip an invariant or pass the
// well-formedness check; the property here is "the checker is total".
// ============================================================================

prop_compose! {
    fn arb_role()(seed in 0u8..3) -> String {
        match seed {
            0 => "Terminal",
            1 => "Dialog",
            _ => "StatusBar",
        }.to_string()
    }
}

prop_compose! {
    fn arb_pane_name()(id in 1u64..16) -> String {
        format!("pane:{id}")
    }
}

prop_compose! {
    fn arb_focus_event()(
        ts in 0u64..10_000,
        role in arb_role(),
        name in arb_pane_name(),
    ) -> AccessibilityEvent {
        AccessibilityEvent::FocusChanged { ts_ms: ts, role, name }
    }
}

prop_compose! {
    fn arb_selection_event()(
        ts in 0u64..10_000,
        name in arb_pane_name(),
        sl in 0u32..200,
        sc in 0u32..120,
        delta_l in 0u32..50,
        delta_c in 0u32..120,
    ) -> AccessibilityEvent {
        AccessibilityEvent::SelectionChanged {
            ts_ms: ts,
            role: "Terminal".to_string(),
            name,
            range_start_line: sl,
            range_start_col: sc,
            range_end_line: sl + delta_l,
            range_end_col: sc + delta_c,
        }
    }
}

prop_compose! {
    fn arb_text_event()(
        ts in 0u64..10_000,
        name in arb_pane_name(),
    ) -> AccessibilityEvent {
        AccessibilityEvent::TextValueChanged {
            ts_ms: ts,
            role: "Terminal".to_string(),
            name,
            value: "byte".to_string(),
        }
    }
}

fn arb_event() -> impl Strategy<Value = AccessibilityEvent> {
    prop_oneof![arb_focus_event(), arb_selection_event(), arb_text_event()]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        .. ProptestConfig::default()
    })]

    /// The invariant checker is *total*: it never panics on an
    /// arbitrary stream, and either reports zero violations or a
    /// non-empty `Vec<InvariantViolation>` — never some other
    /// runtime failure.
    #[test]
    fn invariant_checker_is_total(events in proptest::collection::vec(arb_event(), 0..32)) {
        for scenario in AccessibilityScenario::ALL {
            let _ = check_invariants(*scenario, &events);
        }
    }

    /// A monotonically-increasing-timestamp focus-only stream that
    /// alternates between two distinct panes is always invariant-clean
    /// (no monotonicity, duplicate-focus, selection-before-focus, or
    /// inverted-range violations possible). This is the positive
    /// counterpart to the negative tests in the lib unit suite.
    #[test]
    fn alternating_focus_stream_is_clean(
        steps in proptest::collection::vec(any::<bool>(), 1..16),
    ) {
        let mut events = Vec::new();
        for (i, pick_a) in steps.iter().enumerate() {
            let name = if *pick_a { "pane:1" } else { "pane:2" };
            events.push(AccessibilityEvent::FocusChanged {
                ts_ms: i as u64 * 10,
                role: "Terminal".to_string(),
                name: name.to_string(),
            });
        }
        // Collapse adjacent identical focuses (same as a real
        // recorder would do) so the duplicate-focus invariant stays
        // satisfied even with random alternations.
        events.dedup_by(|a, b| match (a, b) {
            (
                AccessibilityEvent::FocusChanged { name: an, .. },
                AccessibilityEvent::FocusChanged { name: bn, .. },
            ) => an == bn,
            _ => false,
        });
        let v = check_invariants(AccessibilityScenario::PaneFocusChange, &events);
        prop_assert!(v.is_empty(), "alternating-focus stream violations: {v:?}");
    }

    /// Round-tripping any event through JSONL is identity.
    #[test]
    fn jsonl_render_parse_roundtrip(events in proptest::collection::vec(arb_event(), 0..16)) {
        let rendered = JsonlLogWriter::render(&events);
        let parsed = JsonlLogWriter::parse(&rendered).expect("parse");
        prop_assert_eq!(parsed, events);
    }
}

// ============================================================================
// Test 5 — recorder-trait dyn-compat sanity.
//
// Locks in the trait surface that future per-platform recorders
// implement. If a future change to `AccessibilityRecorder` accidentally
// breaks dyn-compat, this fails to compile — the lane catches it
// before the integration beads inherit the breakage.
// ============================================================================

#[test]
fn recorder_trait_is_dyn_compatible() {
    let mut rec: Box<dyn AccessibilityRecorder> = Box::new(ContractRecorder::new());
    rec.start(AccessibilityScenario::SteadyTyping);
    rec.record(AccessibilityEvent::AnnounceMessage {
        ts_ms: 0,
        priority: AnnouncePriority::Polite,
        value: "hello".to_string(),
    });
    let drained = rec.finish();
    assert_eq!(drained.len(), 1);
}
