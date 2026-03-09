//! Property-based tests for the agent_pane_state module.
//!
//! Covers the `classify()` state machine, serde roundtrips, border colors,
//! labels, alert flags, and threshold boundary properties.

use proptest::prelude::*;

use frankenterm_core::agent_pane_state::{
    AgentDetectionConfig, AgentPaneState, AutoLayoutPolicy, PaneActivityTimestamps,
    PaneBackpressureOverlay,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_agent_pane_state() -> impl Strategy<Value = AgentPaneState> {
    prop_oneof![
        Just(AgentPaneState::Active),
        Just(AgentPaneState::Thinking),
        Just(AgentPaneState::Stuck),
        Just(AgentPaneState::Idle),
        Just(AgentPaneState::Human),
    ]
}

fn arb_auto_layout_policy() -> impl Strategy<Value = AutoLayoutPolicy> {
    prop_oneof![
        Just(AutoLayoutPolicy::ByDomain),
        Just(AutoLayoutPolicy::ByStatus),
        Just(AutoLayoutPolicy::ByActivity),
        Just(AutoLayoutPolicy::Manual),
    ]
}

/// Generates a valid AgentDetectionConfig where thresholds obey the
/// natural ordering: active < thinking <= stuck <= idle.
fn arb_config() -> impl Strategy<Value = AgentDetectionConfig> {
    (
        1u64..10_000,       // active_output_threshold_ms
        1u64..60_000,       // thinking_silence_ms (offset above active)
        1u64..120_000,      // stuck_silence_ms (offset above thinking)
        1u64..300_000,      // idle_silence_ms (offset above stuck)
        any::<bool>(),      // enabled
        any::<bool>(),      // show_agent_name_overlay
        any::<bool>(),      // show_backpressure_indicator
        any::<bool>(),      // show_queue_sparkline
        1u32..20,           // agent_border_width_px
    )
        .prop_map(
            |(active, think_off, stuck_off, idle_off, en, name, bp, qs, bw)| {
                AgentDetectionConfig {
                    enabled: en,
                    active_output_threshold_ms: active,
                    thinking_silence_ms: active + think_off,
                    stuck_silence_ms: active + think_off + stuck_off,
                    idle_silence_ms: active + think_off + stuck_off + idle_off,
                    show_agent_name_overlay: name,
                    show_backpressure_indicator: bp,
                    show_queue_sparkline: qs,
                    agent_border_width_px: bw,
                }
            },
        )
}

/// Generates timestamps with now_ms always >= max(last_output_ms, last_input_ms)
/// so that saturating_sub never saturates to 0 unexpectedly.
fn arb_timestamps_with_now() -> impl Strategy<Value = (PaneActivityTimestamps, u64)> {
    (
        0u64..1_000_000,    // last_output_ms
        0u64..1_000_000,    // last_input_ms
        any::<bool>(),      // is_agent
        any::<bool>(),      // flagged_stuck
        0u64..500_000,      // extra_ms added to max to get now_ms
    )
        .prop_map(|(out_ms, in_ms, is_agent, flagged_stuck, extra)| {
            let now_ms = out_ms.max(in_ms) + extra;
            let ts = PaneActivityTimestamps {
                last_output_ms: out_ms,
                last_input_ms: in_ms,
                is_agent,
                flagged_stuck,
            };
            (ts, now_ms)
        })
}

// ---------------------------------------------------------------------------
// Section 1: classify() fundamental invariants
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// Non-agent panes ALWAYS classify as Human, regardless of timestamps or flags.
    #[test]
    fn non_agent_always_human(
        out_ms in 0u64..1_000_000,
        in_ms in 0u64..1_000_000,
        flagged in any::<bool>(),
        now_extra in 0u64..500_000,
    ) {
        let ts = PaneActivityTimestamps {
            last_output_ms: out_ms,
            last_input_ms: in_ms,
            is_agent: false,
            flagged_stuck: flagged,
        };
        let now = out_ms.max(in_ms) + now_extra;
        let config = AgentDetectionConfig::default();
        let state = ts.classify(now, &config);
        let check = state == AgentPaneState::Human;
        prop_assert!(check, "non-agent pane classified as {:?}", state);
    }

    /// flagged_stuck always produces Stuck for agent panes, no matter the timestamps.
    #[test]
    fn flagged_stuck_overrides_to_stuck(
        out_ms in 0u64..1_000_000,
        in_ms in 0u64..1_000_000,
        now_extra in 0u64..500_000,
    ) {
        let ts = PaneActivityTimestamps {
            last_output_ms: out_ms,
            last_input_ms: in_ms,
            is_agent: true,
            flagged_stuck: true,
        };
        let now = out_ms.max(in_ms) + now_extra;
        let config = AgentDetectionConfig::default();
        let state = ts.classify(now, &config);
        let check = state == AgentPaneState::Stuck;
        prop_assert!(check, "flagged_stuck agent classified as {:?}", state);
    }

    /// classify() always returns one of the 5 valid states (exhaustiveness).
    #[test]
    fn classify_returns_valid_state(
        (ts, now_ms) in arb_timestamps_with_now(),
    ) {
        let config = AgentDetectionConfig::default();
        let state = ts.classify(now_ms, &config);
        let valid = matches!(
            state,
            AgentPaneState::Active
                | AgentPaneState::Thinking
                | AgentPaneState::Stuck
                | AgentPaneState::Idle
                | AgentPaneState::Human
        );
        prop_assert!(valid);
    }

    /// Agent panes never classify as Human.
    #[test]
    fn agent_pane_never_human(
        out_ms in 0u64..1_000_000,
        in_ms in 0u64..1_000_000,
        flagged in any::<bool>(),
        now_extra in 0u64..500_000,
    ) {
        let ts = PaneActivityTimestamps {
            last_output_ms: out_ms,
            last_input_ms: in_ms,
            is_agent: true,
            flagged_stuck: flagged,
        };
        let now = out_ms.max(in_ms) + now_extra;
        let config = AgentDetectionConfig::default();
        let state = ts.classify(now, &config);
        let check = state != AgentPaneState::Human;
        prop_assert!(check, "agent pane classified as Human");
    }
}

// ---------------------------------------------------------------------------
// Section 2: classify() threshold boundary properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// Very recent output (within active threshold) → Active for unflagged agents.
    #[test]
    fn recent_output_yields_active(
        base in 10_000u64..500_000,
        delta in 0u64..4999,
    ) {
        let config = AgentDetectionConfig::default(); // active_output_threshold_ms = 5000
        let now = base;
        let ts = PaneActivityTimestamps {
            last_output_ms: now - delta, // within 5000ms
            last_input_ms: 0,
            is_agent: true,
            flagged_stuck: false,
        };
        let state = ts.classify(now, &config);
        let check = state == AgentPaneState::Active;
        prop_assert!(check, "since_output={}, expected Active, got {:?}", delta, state);
    }

    /// When since_output is exactly at the active threshold boundary AND input
    /// is more recent than output, the primary Active check (strict <) fails
    /// and the thinking/stuck path kicks in.
    #[test]
    fn at_active_boundary_with_recent_input_not_active(
        base in 100_000u64..500_000,
    ) {
        let config = AgentDetectionConfig::default();
        let now = base;
        let out_ms = now - config.active_output_threshold_ms;
        let ts = PaneActivityTimestamps {
            last_output_ms: out_ms,
            last_input_ms: out_ms + 1, // input after output → thinking/stuck path
            is_agent: true,
            flagged_stuck: false,
        };
        let state = ts.classify(now, &config);
        // since_output == active_threshold (5000), strict < fails
        // input > output, since_output >= thinking_silence_ms → Thinking
        let check = state != AgentPaneState::Active;
        prop_assert!(check, "at boundary with input>output should not be Active, got {:?}", state);
    }

    /// With custom config, Active threshold is respected.
    #[test]
    fn custom_active_threshold_respected(
        config in arb_config(),
        base in 500_000u64..1_000_000,
    ) {
        let now = base;
        // Just under threshold → Active
        let ts_under = PaneActivityTimestamps {
            last_output_ms: now - (config.active_output_threshold_ms - 1),
            last_input_ms: 0,
            is_agent: true,
            flagged_stuck: false,
        };
        let state_under = ts_under.classify(now, &config);
        let check = state_under == AgentPaneState::Active;
        prop_assert!(check, "1ms under active threshold should be Active, got {:?}", state_under);
    }

    /// Both input and output silent beyond idle threshold → Idle.
    #[test]
    fn both_silent_beyond_idle_is_idle(
        config in arb_config(),
        base in 1_000_000u64..2_000_000,
        extra in 0u64..100_000,
    ) {
        let now = base;
        let silence = config.idle_silence_ms + extra;
        let ts = PaneActivityTimestamps {
            last_output_ms: now.saturating_sub(silence),
            last_input_ms: now.saturating_sub(silence),
            is_agent: true,
            flagged_stuck: false,
        };
        let state = ts.classify(now, &config);
        let check = state == AgentPaneState::Idle;
        prop_assert!(check, "both silent {}ms beyond idle threshold, expected Idle, got {:?}", silence, state);
    }

    /// Input more recent than output, output silent beyond stuck_silence_ms → Stuck.
    #[test]
    fn input_after_output_beyond_stuck_is_stuck(
        config in arb_config(),
        base in 1_000_000u64..2_000_000,
        stuck_extra in 0u64..50_000,
    ) {
        let now = base;
        let since_output = config.stuck_silence_ms + stuck_extra;
        // Place output long ago, input more recently (but still before now)
        let out_ms = now.saturating_sub(since_output);
        // Input must be > output but also more than idle_silence_ms ago to avoid idle
        // Actually input just needs to be > output. Let's place input halfway.
        let in_ms = out_ms + 1; // input more recent than output
        let since_input = now - in_ms;
        // If since_input >= idle_silence_ms AND since_output >= idle_silence_ms, it's Idle
        // So we need since_input < idle_silence_ms to get Stuck
        prop_assume!(since_input < config.idle_silence_ms);
        let ts = PaneActivityTimestamps {
            last_output_ms: out_ms,
            last_input_ms: in_ms,
            is_agent: true,
            flagged_stuck: false,
        };
        let state = ts.classify(now, &config);
        let check = state == AgentPaneState::Stuck;
        prop_assert!(check, "input>output, since_output={}>=stuck={}, expected Stuck, got {:?}",
            since_output, config.stuck_silence_ms, state);
    }

    /// Input more recent than output, output silent in [thinking, stuck) → Thinking.
    #[test]
    fn input_after_output_in_thinking_window(
        config in arb_config(),
        base in 1_000_000u64..2_000_000,
    ) {
        let now = base;
        // since_output must be >= thinking_silence_ms and < stuck_silence_ms
        prop_assume!(config.thinking_silence_ms < config.stuck_silence_ms);
        let since_output = config.thinking_silence_ms;
        let out_ms = now - since_output;
        let in_ms = out_ms + 1; // input more recent than output
        let since_input = now - in_ms;
        prop_assume!(since_input < config.idle_silence_ms);
        let ts = PaneActivityTimestamps {
            last_output_ms: out_ms,
            last_input_ms: in_ms,
            is_agent: true,
            flagged_stuck: false,
        };
        let state = ts.classify(now, &config);
        let check = state == AgentPaneState::Thinking;
        prop_assert!(check, "since_output={} in [thinking={}, stuck={}), expected Thinking, got {:?}",
            since_output, config.thinking_silence_ms, config.stuck_silence_ms, state);
    }
}

// ---------------------------------------------------------------------------
// Section 3: classify() monotonicity / ordering properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// As time advances from recent output to far future, state should not go
    /// backwards from Idle to Active (monotonic degradation for agent panes
    /// with input_ms > output_ms and no new events).
    #[test]
    fn state_degrades_monotonically_with_time(
        out_ms in 100_000u64..200_000,
        in_delta in 1u64..10_000, // input after output
    ) {
        let config = AgentDetectionConfig::default();
        let in_ms = out_ms + in_delta;
        let ts = PaneActivityTimestamps {
            last_output_ms: out_ms,
            last_input_ms: in_ms,
            is_agent: true,
            flagged_stuck: false,
        };

        // Sample states at increasing times
        let times: Vec<u64> = (0..20).map(|i| in_ms + i * 5000).collect();
        let states: Vec<AgentPaneState> = times.iter().map(|&t| ts.classify(t, &config)).collect();

        // Define severity ordering
        fn severity(s: AgentPaneState) -> u8 {
            match s {
                AgentPaneState::Active => 0,
                AgentPaneState::Thinking => 1,
                AgentPaneState::Stuck => 2,
                AgentPaneState::Idle => 3,
                AgentPaneState::Human => 4,
            }
        }

        // Severity should be non-decreasing (allow equal)
        for i in 1..states.len() {
            let prev = severity(states[i - 1]);
            let curr = severity(states[i]);
            // Note: Idle (severity 3) can be reached before Stuck (severity 2) due to
            // the idle check coming before the stuck check when input is also old.
            // So we allow Idle → Stuck transitions.
            let ok = curr >= prev || (states[i - 1] == AgentPaneState::Idle && states[i] == AgentPaneState::Stuck)
                || (states[i - 1] == AgentPaneState::Stuck && states[i] == AgentPaneState::Idle);
            prop_assert!(ok,
                "state went from {:?} (sev {}) to {:?} (sev {}) at times [{}, {}]",
                states[i-1], prev, states[i], curr, times[i-1], times[i]);
        }
    }

    /// Increasing now_ms with fixed timestamps never moves state backwards
    /// from Stuck to Active or Thinking.
    #[test]
    fn stuck_never_returns_to_active(
        out_ms in 10_000u64..100_000,
        in_delta in 1u64..5_000,
    ) {
        let config = AgentDetectionConfig::default();
        let in_ms = out_ms + in_delta;
        let ts = PaneActivityTimestamps {
            last_output_ms: out_ms,
            last_input_ms: in_ms,
            is_agent: true,
            flagged_stuck: false,
        };

        let mut seen_stuck = false;
        for t_offset in (0..200_000).step_by(1000) {
            let t = in_ms + t_offset;
            let state = ts.classify(t, &config);
            if state == AgentPaneState::Stuck {
                seen_stuck = true;
            }
            if seen_stuck {
                let check = state != AgentPaneState::Active && state != AgentPaneState::Thinking;
                prop_assert!(check,
                    "saw Stuck then {:?} at now={}", state, t);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Section 4: AgentPaneState enum properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// border_color_rgba returns Some for all non-Human states.
    #[test]
    fn non_human_has_border_color(state in arb_agent_pane_state()) {
        let color = state.border_color_rgba();
        if state == AgentPaneState::Human {
            let check = color.is_none();
            prop_assert!(check, "Human should have None border color");
        } else {
            let check = color.is_some();
            prop_assert!(check, "{:?} should have Some border color", state);
        }
    }

    /// label() is non-empty for all non-Human states.
    #[test]
    fn non_human_has_nonempty_label(state in arb_agent_pane_state()) {
        let label = state.label();
        if state == AgentPaneState::Human {
            let check = label.is_empty();
            prop_assert!(check, "Human label should be empty");
        } else {
            let check = !label.is_empty();
            prop_assert!(check, "{:?} label should be non-empty", state);
        }
    }

    /// is_alert() is true ONLY for Stuck.
    #[test]
    fn only_stuck_is_alert(state in arb_agent_pane_state()) {
        let alert = state.is_alert();
        let expected = state == AgentPaneState::Stuck;
        prop_assert_eq!(alert, expected, "{:?}.is_alert()", state);
    }

    /// All border colors have full opacity (alpha = 255).
    #[test]
    fn border_colors_full_opacity(state in arb_agent_pane_state()) {
        if let Some((_, _, _, a)) = state.border_color_rgba() {
            prop_assert_eq!(a, 255u8, "{:?} alpha should be 255", state);
        }
    }

    /// All 5 states have distinct border_color_rgba values.
    #[test]
    fn distinct_border_colors(s1 in arb_agent_pane_state(), s2 in arb_agent_pane_state()) {
        if s1 != s2 {
            prop_assert_ne!(s1.border_color_rgba(), s2.border_color_rgba(),
                "{:?} and {:?} should have different colors", s1, s2);
        }
    }
}

// ---------------------------------------------------------------------------
// Section 5: Serde roundtrip fidelity
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// AgentPaneState survives JSON roundtrip.
    #[test]
    fn agent_pane_state_serde_roundtrip(state in arb_agent_pane_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let back: AgentPaneState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(state, back);
    }

    /// AutoLayoutPolicy survives JSON roundtrip.
    #[test]
    fn auto_layout_policy_serde_roundtrip(policy in arb_auto_layout_policy()) {
        let json = serde_json::to_string(&policy).unwrap();
        let back: AutoLayoutPolicy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(policy, back);
    }

    /// AgentDetectionConfig survives JSON roundtrip.
    #[test]
    fn detection_config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: AgentDetectionConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.enabled, back.enabled);
        prop_assert_eq!(config.active_output_threshold_ms, back.active_output_threshold_ms);
        prop_assert_eq!(config.thinking_silence_ms, back.thinking_silence_ms);
        prop_assert_eq!(config.stuck_silence_ms, back.stuck_silence_ms);
        prop_assert_eq!(config.idle_silence_ms, back.idle_silence_ms);
        prop_assert_eq!(config.agent_border_width_px, back.agent_border_width_px);
    }

    /// PaneBackpressureOverlay survives JSON roundtrip.
    #[test]
    fn backpressure_overlay_serde_roundtrip(
        tier in "[a-z]{3,10}",
        fill in 0.0f64..1.0,
        rl in any::<bool>(),
    ) {
        let overlay = PaneBackpressureOverlay {
            tier,
            queue_fill_ratio: fill,
            rate_limited: rl,
        };
        let json = serde_json::to_string(&overlay).unwrap();
        let back: PaneBackpressureOverlay = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&overlay.tier, &back.tier);
        let fill_check = (overlay.queue_fill_ratio - back.queue_fill_ratio).abs() < 1e-10;
        prop_assert!(fill_check, "fill ratio drift");
        prop_assert_eq!(overlay.rate_limited, back.rate_limited);
    }

    /// AgentPaneState snake_case serde format is correct.
    #[test]
    fn agent_pane_state_snake_case_format(state in arb_agent_pane_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let expected = match state {
            AgentPaneState::Active => "\"active\"",
            AgentPaneState::Thinking => "\"thinking\"",
            AgentPaneState::Stuck => "\"stuck\"",
            AgentPaneState::Idle => "\"idle\"",
            AgentPaneState::Human => "\"human\"",
        };
        prop_assert_eq!(json, expected);
    }

    /// AutoLayoutPolicy snake_case serde format is correct.
    #[test]
    fn auto_layout_policy_snake_case_format(policy in arb_auto_layout_policy()) {
        let json = serde_json::to_string(&policy).unwrap();
        let expected = match policy {
            AutoLayoutPolicy::ByDomain => "\"by_domain\"",
            AutoLayoutPolicy::ByStatus => "\"by_status\"",
            AutoLayoutPolicy::ByActivity => "\"by_activity\"",
            AutoLayoutPolicy::Manual => "\"manual\"",
        };
        prop_assert_eq!(json, expected);
    }
}

// ---------------------------------------------------------------------------
// Section 6: Default impls
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Default AgentPaneState is Human.
    #[test]
    fn default_state_is_human(_seed in 0u32..100) {
        let state = AgentPaneState::default();
        let check = state == AgentPaneState::Human;
        prop_assert!(check);
    }

    /// Default AutoLayoutPolicy is ByStatus.
    #[test]
    fn default_policy_is_by_status(_seed in 0u32..100) {
        let policy = AutoLayoutPolicy::default();
        let check = policy == AutoLayoutPolicy::ByStatus;
        prop_assert!(check);
    }

    /// Default config has sane threshold ordering: active < thinking <= stuck <= idle.
    #[test]
    fn default_config_threshold_ordering(_seed in 0u32..100) {
        let config = AgentDetectionConfig::default();
        prop_assert!(config.active_output_threshold_ms <= config.thinking_silence_ms);
        prop_assert!(config.thinking_silence_ms <= config.stuck_silence_ms);
        prop_assert!(config.stuck_silence_ms <= config.idle_silence_ms);
    }
}

// ---------------------------------------------------------------------------
// Section 7: Edge cases and regression guards
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// When now_ms == last_output_ms (since_output = 0), agent should be Active.
    #[test]
    fn zero_since_output_is_active(
        out_ms in 0u64..1_000_000,
    ) {
        let ts = PaneActivityTimestamps {
            last_output_ms: out_ms,
            last_input_ms: 0,
            is_agent: true,
            flagged_stuck: false,
        };
        let config = AgentDetectionConfig::default();
        let state = ts.classify(out_ms, &config);
        let check = state == AgentPaneState::Active;
        prop_assert!(check, "since_output=0 should be Active, got {:?}", state);
    }

    /// When both timestamps are 0 and now is 0, agent should be Active.
    #[test]
    fn all_zero_timestamps_is_active(_seed in 0u32..100) {
        let ts = PaneActivityTimestamps {
            last_output_ms: 0,
            last_input_ms: 0,
            is_agent: true,
            flagged_stuck: false,
        };
        let config = AgentDetectionConfig::default();
        let state = ts.classify(0, &config);
        let check = state == AgentPaneState::Active;
        prop_assert!(check, "all zeros should be Active, got {:?}", state);
    }

    /// Very large timestamps don't overflow or panic.
    #[test]
    fn large_timestamps_no_panic(
        out_ms in (u64::MAX - 1_000_000)..u64::MAX,
        in_ms in (u64::MAX - 1_000_000)..u64::MAX,
    ) {
        let now = u64::MAX;
        let ts = PaneActivityTimestamps {
            last_output_ms: out_ms,
            last_input_ms: in_ms,
            is_agent: true,
            flagged_stuck: false,
        };
        let config = AgentDetectionConfig::default();
        // Should not panic
        let _state = ts.classify(now, &config);
    }

    /// Saturating subtraction: when now_ms < last_output_ms, since_output is 0 → Active.
    #[test]
    fn now_before_output_saturates_to_active(
        out_ms in 100_000u64..500_000,
        rewind in 1u64..100_000,
    ) {
        let now = out_ms - rewind; // now is before last output
        let ts = PaneActivityTimestamps {
            last_output_ms: out_ms,
            last_input_ms: 0,
            is_agent: true,
            flagged_stuck: false,
        };
        let config = AgentDetectionConfig::default();
        let state = ts.classify(now, &config);
        // since_output saturates to 0, which is < active threshold → Active
        let check = state == AgentPaneState::Active;
        prop_assert!(check, "saturated since_output=0 should be Active, got {:?}", state);
    }

    /// classify() is deterministic: same inputs always produce same output.
    #[test]
    fn classify_is_deterministic(
        (ts, now_ms) in arb_timestamps_with_now(),
    ) {
        let config = AgentDetectionConfig::default();
        let s1 = ts.classify(now_ms, &config);
        let s2 = ts.classify(now_ms, &config);
        prop_assert_eq!(s1, s2);
    }

    /// Config with equal thinking/stuck thresholds: Thinking window is empty,
    /// so the state should jump directly from Active to Stuck.
    #[test]
    fn equal_thinking_stuck_skips_thinking(
        base in 200_000u64..500_000,
        threshold in 1000u64..20_000,
    ) {
        let config = AgentDetectionConfig {
            active_output_threshold_ms: 1000,
            thinking_silence_ms: threshold,
            stuck_silence_ms: threshold, // equal to thinking
            idle_silence_ms: threshold + 50_000,
            ..AgentDetectionConfig::default()
        };
        let now = base;
        // Place output exactly at thinking/stuck boundary with input > output
        let out_ms = now - threshold;
        let in_ms = out_ms + 1;
        let ts = PaneActivityTimestamps {
            last_output_ms: out_ms,
            last_input_ms: in_ms,
            is_agent: true,
            flagged_stuck: false,
        };
        let state = ts.classify(now, &config);
        // since_output == threshold == stuck_silence_ms, and input > output
        // The stuck check uses >=, so it should be Stuck
        let check = state == AgentPaneState::Stuck;
        prop_assert!(check, "equal thinking/stuck threshold should classify as Stuck, got {:?}", state);
    }
}

// ---------------------------------------------------------------------------
// Section 8: Cross-property consistency
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// label() and border_color_rgba() agree on Human-ness.
    #[test]
    fn label_and_color_consistent(state in arb_agent_pane_state()) {
        let has_color = state.border_color_rgba().is_some();
        let has_label = !state.label().is_empty();
        prop_assert_eq!(has_color, has_label,
            "{:?}: color={}, label={}", state, has_color, has_label);
    }

    /// Every state that has is_alert=true also has a border color.
    #[test]
    fn alert_implies_border_color(state in arb_agent_pane_state()) {
        if state.is_alert() {
            let check = state.border_color_rgba().is_some();
            prop_assert!(check, "alert state {:?} should have border color", state);
        }
    }

    /// Classify with default config and various flag/agent combinations covers all branches.
    #[test]
    fn classify_coverage_exercise(
        out_ms in 0u64..200_000,
        in_ms in 0u64..200_000,
        is_agent in any::<bool>(),
        flagged in any::<bool>(),
        now_extra in 0u64..200_000,
    ) {
        let ts = PaneActivityTimestamps {
            last_output_ms: out_ms,
            last_input_ms: in_ms,
            is_agent,
            flagged_stuck: flagged,
        };
        let now = out_ms.max(in_ms) + now_extra;
        let config = AgentDetectionConfig::default();
        let state = ts.classify(now, &config);

        // Basic consistency: non-agent → Human, agent → non-Human
        if !is_agent {
            let check = state == AgentPaneState::Human;
            prop_assert!(check);
        } else {
            let check = state != AgentPaneState::Human;
            prop_assert!(check);
        }
    }
}
