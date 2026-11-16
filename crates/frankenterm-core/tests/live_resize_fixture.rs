//! Live-resize state-machine regression fixture (`ft-mpc9b.2.1`).
//!
//! Foundation slice for sub-epic 2's live-resize fast path. Until
//! the per-platform recorder beads land (touching macOS / Wayland /
//! X11 window.rs files), this fixture exercises the
//! `LiveResizeStateMachine` against synthetic event streams that
//! mirror what the platforms emit and pins the state-diagram
//! invariants the integration beads must preserve.
//!
//! ## Goldens
//!
//! `crates/frankenterm-core/tests/live_resize/golden/<platform>-<scenario>.jsonl`.
//! `FT_LIVE_RESIZE_BLESS=1` regenerates with the deliberate-bless
//! flow used by the prior fixtures in this session.

use std::path::PathBuf;

use frankenterm_core::live_resize::{
    LiveResizeEvent, LiveResizePlatform, LiveResizeState, LiveResizeStateMachine,
    LiveResizeTransition, LiveResizeTransitionSource, parse_transitions_jsonl,
    render_transitions_jsonl,
};
use proptest::prelude::*;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("live_resize")
        .join("golden")
}

fn golden_path(scenario: &str) -> PathBuf {
    golden_dir().join(format!("{scenario}.jsonl"))
}

fn bless_enabled() -> bool {
    std::env::var("FT_LIVE_RESIZE_BLESS")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn ensure_golden_dir_exists() {
    std::fs::create_dir_all(golden_dir()).expect("create golden dir");
}

/// Drive a sequence of events through a fresh machine and return
/// the produced transition log.
fn drive(events: &[LiveResizeEvent]) -> Vec<LiveResizeTransition> {
    let mut m = LiveResizeStateMachine::new();
    let mut log = Vec::new();
    for e in events {
        if let Some(t) = m.step(*e) {
            log.push(t);
        }
    }
    log
}

// ============================================================================
// Per-platform synthetic scenarios
// ============================================================================

/// macOS happy path: WillStartLiveResize, a stream of
/// configures, DidEndLiveResize.
fn macos_happy_path() -> Vec<LiveResizeEvent> {
    let mut events = vec![LiveResizeEvent::BeginSignal { ts_ms: 0 }];
    for i in 0..5u64 {
        events.push(LiveResizeEvent::Configure {
            ts_ms: 16 * (i + 1),
            width: 800 + i as u32 * 10,
            height: 600,
        });
    }
    events.push(LiveResizeEvent::EndSignal { ts_ms: 100 });
    events
}

/// macOS skipped-DidEnd: the bead's failure mode #2. Begin fires,
/// resize happens, but DidEnd never arrives — a mouse-up event
/// must drive recovery.
fn macos_skipped_did_end() -> Vec<LiveResizeEvent> {
    vec![
        LiveResizeEvent::BeginSignal { ts_ms: 0 },
        LiveResizeEvent::Configure {
            ts_ms: 16,
            width: 810,
            height: 600,
        },
        LiveResizeEvent::Configure {
            ts_ms: 32,
            width: 820,
            height: 600,
        },
        // No EndSignal — mouse-up recovery instead.
        LiveResizeEvent::MouseUpDuringResize { ts_ms: 40 },
    ]
}

/// Wayland configure storm: the bead's failure mode #3. >100
/// configures in 100ms must coalesce.
fn wayland_configure_storm() -> Vec<LiveResizeEvent> {
    let mut events = vec![LiveResizeEvent::BeginSignal { ts_ms: 0 }];
    // 200 configures over 50ms — exceeds 100/100ms threshold.
    for i in 0..200u64 {
        events.push(LiveResizeEvent::Configure {
            ts_ms: i / 4 + 1,
            width: 800 + (i % 20) as u32,
            height: 600,
        });
    }
    events.push(LiveResizeEvent::EndSignal { ts_ms: 60 });
    events
}

/// X11 burst-synthesized begin: no `_NET_WM_STATE_LIVE_RESIZE`,
/// just a stream of dimension-changing ConfigureNotify events.
fn x11_burst_synthesized() -> Vec<LiveResizeEvent> {
    vec![
        // No BeginSignal — first Configure synthesizes Begin.
        LiveResizeEvent::Configure {
            ts_ms: 0,
            width: 800,
            height: 600,
        },
        LiveResizeEvent::Configure {
            ts_ms: 16,
            width: 810,
            height: 600,
        },
        LiveResizeEvent::Configure {
            ts_ms: 32,
            width: 820,
            height: 605,
        },
        LiveResizeEvent::EndSignal { ts_ms: 100 },
    ]
}

/// X11 fake-positive: a ConfigureNotify with UNCHANGED dimensions
/// (workspace switch / window-move). Must NOT trigger a begin.
fn x11_fake_positive() -> Vec<LiveResizeEvent> {
    vec![
        // First Configure with dimensions changed (legitimate).
        LiveResizeEvent::Configure {
            ts_ms: 0,
            width: 800,
            height: 600,
        },
        LiveResizeEvent::EndSignal { ts_ms: 50 },
        LiveResizeEvent::WatchdogTick { ts_ms: 100 },
        // Workspace switch — same dimensions. Must produce 0
        // additional transitions.
        LiveResizeEvent::Configure {
            ts_ms: 200,
            width: 800,
            height: 600,
        },
    ]
}

/// Watchdog forced end: Begin fires, no further events for 5+s.
fn watchdog_stuck_in_resizing() -> Vec<LiveResizeEvent> {
    vec![
        LiveResizeEvent::BeginSignal { ts_ms: 0 },
        LiveResizeEvent::Configure {
            ts_ms: 5,
            width: 800,
            height: 600,
        },
        // 4s tick — should NOT fire.
        LiveResizeEvent::WatchdogTick { ts_ms: 4_000 },
        // 6s tick — MUST fire.
        LiveResizeEvent::WatchdogTick { ts_ms: 6_000 },
    ]
}

// ============================================================================
// Test 1 — synthetic scenarios produce valid transition logs.
// ============================================================================

#[test]
fn macos_happy_path_produces_clean_transitions() {
    let log = drive(&macos_happy_path());
    assert_state_diagram_acyclic(&log);
    assert!(
        log.iter()
            .any(|t| t.next_state == LiveResizeState::ResizeEnd
                && t.source == LiveResizeTransitionSource::PlatformEnd)
    );
}

#[test]
fn macos_skipped_did_end_recovers_via_mouse_up() {
    let log = drive(&macos_skipped_did_end());
    assert_state_diagram_acyclic(&log);
    let recovery = log
        .iter()
        .find(|t| t.source == LiveResizeTransitionSource::MouseUpRecovery)
        .expect("mouse-up recovery transition missing");
    assert_eq!(recovery.next_state, LiveResizeState::ResizeEnd);
}

#[test]
fn wayland_configure_storm_does_not_explode_transition_log() {
    let mut m = LiveResizeStateMachine::new();
    let mut log = Vec::new();
    for ev in wayland_configure_storm() {
        if let Some(t) = m.step(ev) {
            log.push(t);
        }
    }
    // Only the structural transitions appear:
    // Idle → ResizeBegin → Resizing → ResizeEnd (then auto-Idle on
    // next event). The 200 configures don't each emit transitions.
    assert!(
        log.len() <= 6,
        "configure storm produced {} transitions; expected ≤6",
        log.len()
    );
    // Coalescing counter MUST have fired.
    assert!(
        m.health().coalesced_total > 0,
        "configure storm should have triggered coalescing"
    );
}

#[test]
fn x11_burst_synthesizes_begin_without_explicit_signal() {
    let log = drive(&x11_burst_synthesized());
    let first = &log[0];
    assert_eq!(first.next_state, LiveResizeState::ResizeBegin);
    assert_eq!(
        first.source,
        LiveResizeTransitionSource::ConfigureBurstSynthesizedBegin
    );
}

#[test]
fn x11_fake_positive_with_unchanged_dimensions_is_filtered() {
    let mut m = LiveResizeStateMachine::new();
    let mut transitions_after_idle = 0;
    let mut went_idle = false;
    for ev in x11_fake_positive() {
        if let Some(t) = m.step(ev) {
            if went_idle {
                transitions_after_idle += 1;
            }
            if t.next_state == LiveResizeState::Idle {
                went_idle = true;
            }
        }
    }
    assert!(
        went_idle,
        "machine must have reached Idle in the fake-positive scenario"
    );
    assert_eq!(
        transitions_after_idle, 0,
        "Configure with unchanged dimensions while Idle must not transition"
    );
}

#[test]
fn watchdog_forces_end_in_stuck_scenario() {
    let log = drive(&watchdog_stuck_in_resizing());
    let forced = log
        .iter()
        .find(|t| t.source == LiveResizeTransitionSource::WatchdogForcedEnd)
        .expect("watchdog forced-end transition missing");
    assert_eq!(forced.next_state, LiveResizeState::ResizeEnd);
}

// ============================================================================
// Test 2 — golden snapshots per-platform.
// ============================================================================

#[test]
fn golden_macos_happy_path() {
    snapshot_golden("macos-happy_path", &drive(&macos_happy_path()));
}

#[test]
fn golden_macos_skipped_did_end() {
    snapshot_golden("macos-skipped_did_end", &drive(&macos_skipped_did_end()));
}

#[test]
fn golden_wayland_configure_storm() {
    snapshot_golden(
        "wayland-configure_storm",
        &drive(&wayland_configure_storm()),
    );
}

#[test]
fn golden_x11_burst_synthesized() {
    snapshot_golden("x11-burst_synthesized", &drive(&x11_burst_synthesized()));
}

#[test]
fn golden_x11_fake_positive() {
    snapshot_golden("x11-fake_positive", &drive(&x11_fake_positive()));
}

#[test]
fn golden_watchdog_stuck() {
    snapshot_golden(
        "synthetic-watchdog_stuck",
        &drive(&watchdog_stuck_in_resizing()),
    );
}

fn snapshot_golden(scenario: &str, transitions: &[LiveResizeTransition]) {
    let rendered = render_transitions_jsonl(transitions);
    let path = golden_path(scenario);

    if bless_enabled() {
        ensure_golden_dir_exists();
        std::fs::write(&path, &rendered).expect("write blessed golden");
        panic!(
            "{scenario}: golden blessed at {}; re-run without FT_LIVE_RESIZE_BLESS to validate",
            path.display()
        );
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "missing golden for {scenario} at {}: {err} \
             (re-run with FT_LIVE_RESIZE_BLESS=1 to generate)",
            path.display()
        )
    });

    assert_eq!(
        rendered,
        expected,
        "{scenario} drifted from golden at {}",
        path.display()
    );

    let parsed = parse_transitions_jsonl(&rendered).expect("parse");
    assert_eq!(parsed, transitions, "JSONL roundtrip drift for {scenario}");
}

// ============================================================================
// Test 3 — sentinel for the "no integration wired today" state.
// ============================================================================

#[test]
fn only_synthetic_platform_is_wired_today() {
    assert!(LiveResizePlatform::Synthetic.is_wired());
    for not_wired in [
        LiveResizePlatform::Macos,
        LiveResizePlatform::Wayland,
        LiveResizePlatform::X11,
    ] {
        assert!(
            !not_wired.is_wired(),
            "{not_wired:?} reports wired but no integration has landed"
        );
    }
}

// ============================================================================
// Test 4 — state-diagram invariants (the bead's correctness rules).
// ============================================================================

/// The bead's headline correctness rule: projected onto
/// `(Idle → Begin → Resizing* → End → Idle)` the state diagram is
/// acyclic; every `ResizeBegin` is followed by exactly one
/// `ResizeEnd` before the next `ResizeBegin`.
fn assert_state_diagram_acyclic(log: &[LiveResizeTransition]) {
    let mut begins_pending: i64 = 0;
    for t in log {
        if t.next_state == LiveResizeState::ResizeBegin {
            assert!(
                begins_pending == 0,
                "second ResizeBegin without intervening ResizeEnd: {t:?}"
            );
            begins_pending += 1;
        } else if t.next_state == LiveResizeState::ResizeEnd {
            begins_pending -= 1;
            assert!(
                begins_pending >= -1,
                "ResizeEnd without preceding ResizeBegin: {t:?}"
            );
            // -1 is allowed only for the auto-clear from ResizeEnd
            // → Idle case (which doesn't emit on its own).
        }
        // Timestamps must be non-decreasing.
        // (Verified separately in test 5.)
    }
}

// ============================================================================
// Test 5 — proptest invariants over arbitrary event streams.
// ============================================================================

#[derive(Debug, Clone, Copy)]
enum OpKind {
    Begin,
    Configure,
    End,
    MouseUp,
    Watchdog,
}

prop_compose! {
    fn arb_op()(
        choice in 0u8..5,
        delta_ms in 1u64..=200,
        width in 100u32..1920,
        height in 100u32..1080,
    ) -> (OpKind, u64, u32, u32) {
        let kind = match choice {
            0 => OpKind::Begin,
            1 => OpKind::Configure,
            2 => OpKind::End,
            3 => OpKind::MouseUp,
            _ => OpKind::Watchdog,
        };
        (kind, delta_ms, width, height)
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        .. ProptestConfig::default()
    })]

    /// State transitions never panic on arbitrary event streams.
    /// Timestamps are monotonic in the produced log.
    #[test]
    fn state_machine_is_total_under_arbitrary_events(
        ops in proptest::collection::vec(arb_op(), 0..32),
    ) {
        let mut m = LiveResizeStateMachine::new();
        let mut ts = 0u64;
        let mut log = Vec::new();
        for (kind, delta, w, h) in ops {
            ts = ts.saturating_add(delta);
            let event = match kind {
                OpKind::Begin => LiveResizeEvent::BeginSignal { ts_ms: ts },
                OpKind::Configure => LiveResizeEvent::Configure { ts_ms: ts, width: w, height: h },
                OpKind::End => LiveResizeEvent::EndSignal { ts_ms: ts },
                OpKind::MouseUp => LiveResizeEvent::MouseUpDuringResize { ts_ms: ts },
                OpKind::Watchdog => LiveResizeEvent::WatchdogTick { ts_ms: ts },
            };
            if let Some(t) = m.step(event) {
                log.push(t);
            }
        }
        // Timestamps non-decreasing.
        let mut prior_ts = 0u64;
        for t in &log {
            prop_assert!(t.ts_ms >= prior_ts, "timestamps regressed in log");
            prior_ts = t.ts_ms;
        }
        // Counters are well-formed.
        let h = m.health();
        prop_assert!(h.transitions_total >= h.watchdog_forced_ends_total);
        prop_assert!(h.transitions_total >= h.mouse_up_recoveries_total);
    }

    /// JSONL roundtrip identity.
    #[test]
    fn jsonl_roundtrip(
        ops in proptest::collection::vec(arb_op(), 0..16),
    ) {
        let mut m = LiveResizeStateMachine::new();
        let mut ts = 0u64;
        let mut log = Vec::new();
        for (kind, delta, w, h) in ops {
            ts = ts.saturating_add(delta);
            let event = match kind {
                OpKind::Begin => LiveResizeEvent::BeginSignal { ts_ms: ts },
                OpKind::Configure => LiveResizeEvent::Configure { ts_ms: ts, width: w, height: h },
                OpKind::End => LiveResizeEvent::EndSignal { ts_ms: ts },
                OpKind::MouseUp => LiveResizeEvent::MouseUpDuringResize { ts_ms: ts },
                OpKind::Watchdog => LiveResizeEvent::WatchdogTick { ts_ms: ts },
            };
            if let Some(t) = m.step(event) {
                log.push(t);
            }
        }
        let rendered = render_transitions_jsonl(&log);
        let parsed = parse_transitions_jsonl(&rendered).expect("parse");
        prop_assert_eq!(parsed, log);
    }

    /// Adversarial recovery: any prefix of arbitrary events
    /// followed by a 5+s WatchdogTick must leave the machine in
    /// `Idle` (after the watchdog forces End and the auto-clear
    /// runs on the next event). Pins the bead's "stuck-in-Resizing
    /// recovery within 5s" rule.
    #[test]
    fn adversarial_recovery_returns_to_idle(
        ops in proptest::collection::vec(arb_op(), 0..16),
    ) {
        let mut m = LiveResizeStateMachine::new();
        let mut ts = 0u64;
        for (kind, delta, w, h) in ops {
            ts = ts.saturating_add(delta);
            let event = match kind {
                OpKind::Begin => LiveResizeEvent::BeginSignal { ts_ms: ts },
                OpKind::Configure => LiveResizeEvent::Configure { ts_ms: ts, width: w, height: h },
                OpKind::End => LiveResizeEvent::EndSignal { ts_ms: ts },
                OpKind::MouseUp => LiveResizeEvent::MouseUpDuringResize { ts_ms: ts },
                OpKind::Watchdog => LiveResizeEvent::WatchdogTick { ts_ms: ts },
            };
            m.step(event);
        }
        // Now fire a watchdog tick 6+ seconds past the most recent
        // event timestamp — must force End if we're stuck in
        // ResizeBegin or Resizing.
        ts = ts.saturating_add(6_001);
        m.step(LiveResizeEvent::WatchdogTick { ts_ms: ts });
        // Then a follow-up tick to flush ResizeEnd → Idle.
        ts = ts.saturating_add(100);
        m.step(LiveResizeEvent::WatchdogTick { ts_ms: ts });
        prop_assert_eq!(m.state(), LiveResizeState::Idle);
    }
}
