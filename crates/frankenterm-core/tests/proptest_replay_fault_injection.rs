//! Property-based tests for replay_fault_injection (ft-og6q6.4.2).
//!
//! Invariants tested:
//! - FI-1: Delay increases timestamp by exact duration_ms
//! - FI-2: Drop with p=0 keeps all events
//! - FI-3: Drop with p=1 drops all events
//! - FI-4: Drop with p=0.5 drops roughly half (statistical)
//! - FI-5: Duplicate creates exactly count+1 events (original + copies)
//! - FI-6: Corrupt timestamp subtracts correctly (saturating)
//! - FI-7: Empty spec preserves all events unchanged
//! - FI-8: Seeded PRNG is deterministic across identical runs
//! - FI-9: EventFilter match_all matches everything
//! - FI-10: EventFilter pane_id restricts correctly
//! - FI-11: EventFilter time_range restricts correctly
//! - FI-12: Batch reorder preserves event count
//! - FI-13: FaultLog count matches injections
//! - FI-14: FaultSpec serde roundtrip
//! - FI-15: EventFilter serde roundtrip
//! - FI-16: FaultLogEntry serde roundtrip
//! - FI-17: SimEvent serde roundtrip
//! - FI-18: Preset pane_death drops after cutoff
//! - FI-19: Preset network_partition delays in window only
//! - FI-20: FaultLog JSONL output has correct line count
//! - FI-21: Filter non-matching events are untouched by delay

use proptest::prelude::*;

use frankenterm_core::replay_fault_injection::{
    EventFilter, FaultInjector, FaultLog, FaultLogEntry, FaultPresets, FaultSpec, FaultType,
    SimEvent,
};

fn make_event(id: &str, pane: &str, kind: &str, ts: u64, seq: u64) -> SimEvent {
    SimEvent {
        event_id: id.to_string(),
        pane_id: pane.to_string(),
        event_kind: kind.to_string(),
        timestamp_ms: ts,
        sequence: seq,
        payload: format!("payload_{id}"),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // ── FI-1: Delay adds exact duration ─────────────────────────────────

    #[test]
    fn fi1_delay_exact(ts in 0u64..100_000, dur in 1u64..10_000) {
        let spec = FaultSpec {
            name: "fi1".into(),
            description: String::new(),
            seed: 0,
            faults: vec![FaultType::Delay {
                filter: EventFilter::match_all(),
                duration_ms: dur,
            }],
        };
        let mut inj = FaultInjector::new(spec);
        let evt = make_event("e1", "p1", "data", ts, 0);
        let result = inj.process(evt);
        prop_assert_eq!(result[0].timestamp_ms, ts.saturating_add(dur));
    }

    // ── FI-2: Drop p=0 keeps all ───────────────────────────────────────

    #[test]
    fn fi2_drop_zero(n in 1usize..50) {
        let spec = FaultSpec {
            name: "fi2".into(),
            description: String::new(),
            seed: 42,
            faults: vec![FaultType::Drop {
                filter: EventFilter::match_all(),
                probability: 0.0,
            }],
        };
        let mut inj = FaultInjector::new(spec);
        for i in 0..n {
            let evt = make_event(&format!("e{i}"), "p1", "data", i as u64 * 10, i as u64);
            let result = inj.process(evt);
            prop_assert_eq!(result.len(), 1);
        }
    }

    // ── FI-3: Drop p=1 drops all ───────────────────────────────────────

    #[test]
    fn fi3_drop_one(n in 1usize..50) {
        let spec = FaultSpec {
            name: "fi3".into(),
            description: String::new(),
            seed: 42,
            faults: vec![FaultType::Drop {
                filter: EventFilter::match_all(),
                probability: 1.0,
            }],
        };
        let mut inj = FaultInjector::new(spec);
        for i in 0..n {
            let evt = make_event(&format!("e{i}"), "p1", "data", i as u64 * 10, i as u64);
            let result = inj.process(evt);
            prop_assert!(result.is_empty());
        }
    }

    // ── FI-4: Drop p=0.5 roughly half ──────────────────────────────────

    #[test]
    fn fi4_drop_half(seed in 0u64..10000) {
        let spec = FaultSpec {
            name: "fi4".into(),
            description: String::new(),
            seed,
            faults: vec![FaultType::Drop {
                filter: EventFilter::match_all(),
                probability: 0.5,
            }],
        };
        let mut inj = FaultInjector::new(spec);
        let mut dropped = 0;
        let n = 500;
        for i in 0..n {
            let evt = make_event(&format!("e{i}"), "p1", "data", i as u64 * 10, i as u64);
            if inj.process(evt).is_empty() {
                dropped += 1;
            }
        }
        prop_assert!(dropped > 150, "expected >150 drops, got {}", dropped);
        prop_assert!(dropped < 350, "expected <350 drops, got {}", dropped);
    }

    // ── FI-5: Duplicate creates count+1 ─────────────────────────────────

    #[test]
    fn fi5_duplicate_count(count in 1usize..10) {
        let spec = FaultSpec {
            name: "fi5".into(),
            description: String::new(),
            seed: 0,
            faults: vec![FaultType::Duplicate {
                filter: EventFilter::match_all(),
                count,
            }],
        };
        let mut inj = FaultInjector::new(spec);
        let evt = make_event("e1", "p1", "data", 100, 0);
        let result = inj.process(evt);
        prop_assert_eq!(result.len(), count + 1);
    }

    // ── FI-6: Corrupt timestamp subtract (saturating) ───────────────────

    #[test]
    fn fi6_corrupt_timestamp(ts in 0u64..10000, delta in 0u64..5000) {
        let spec = FaultSpec {
            name: "fi6".into(),
            description: String::new(),
            seed: 0,
            faults: vec![FaultType::Corrupt {
                filter: EventFilter::match_all(),
                field: "timestamp_ms".into(),
                mutation: format!("-{delta}"),
            }],
        };
        let mut inj = FaultInjector::new(spec);
        let evt = make_event("e1", "p1", "data", ts, 0);
        let result = inj.process(evt);
        prop_assert_eq!(result[0].timestamp_ms, ts.saturating_sub(delta));
    }

    // ── FI-7: Empty spec no modifications ───────────────────────────────

    #[test]
    fn fi7_empty_spec(ts in 0u64..10000, seq in 0u64..1000) {
        let spec = FaultSpec {
            name: "fi7".into(),
            description: String::new(),
            seed: 0,
            faults: vec![],
        };
        let mut inj = FaultInjector::new(spec);
        let evt = make_event("e1", "p1", "data", ts, seq);
        let result = inj.process(evt);
        prop_assert_eq!(result.len(), 1);
        prop_assert_eq!(result[0].timestamp_ms, ts);
        prop_assert_eq!(result[0].sequence, seq);
    }

    // ── FI-8: Seeded PRNG deterministic ─────────────────────────────────

    #[test]
    fn fi8_deterministic(seed in 0u64..100000, n in 1usize..50) {
        let make_spec = |s| FaultSpec {
            name: "fi8".into(),
            description: String::new(),
            seed: s,
            faults: vec![FaultType::Drop {
                filter: EventFilter::match_all(),
                probability: 0.5,
            }],
        };
        let mut inj1 = FaultInjector::new(make_spec(seed));
        let mut inj2 = FaultInjector::new(make_spec(seed));
        for i in 0..n {
            let evt = make_event(&format!("e{i}"), "p1", "data", i as u64 * 10, i as u64);
            let r1 = inj1.process(evt.clone());
            let r2 = inj2.process(evt);
            prop_assert_eq!(r1.len(), r2.len());
        }
    }

    // ── FI-9: match_all matches everything ──────────────────────────────

    #[test]
    fn fi9_match_all(
        pane in "[a-z]{2,5}",
        kind in "[a-z]{2,5}",
        ts in 0u64..100000,
        seq in 0u64..1000
    ) {
        let f = EventFilter::match_all();
        prop_assert!(f.matches(&pane, &kind, ts, seq));
    }

    // ── FI-10: Pane filter restricts ────────────────────────────────────

    #[test]
    fn fi10_pane_filter(
        target in "[a-z]{3,6}",
        other in "[a-z]{3,6}"
    ) {
        prop_assume!(target != other);
        let f = EventFilter {
            pane_id: Some(target.clone()),
            ..EventFilter::match_all()
        };
        prop_assert!(f.matches(&target, "data", 100, 0));
        prop_assert!(!f.matches(&other, "data", 100, 0));
    }

    // ── FI-11: Time range restricts ─────────────────────────────────────

    #[test]
    fn fi11_time_range(start in 100u64..5000, width in 10u64..1000) {
        let end = start + width;
        let f = EventFilter {
            time_range_start_ms: Some(start),
            time_range_end_ms: Some(end),
            ..EventFilter::match_all()
        };
        // Inside range.
        let mid = start + width / 2;
        prop_assert!(f.matches("p", "d", mid, 0));
        // Before range.
        if start > 0 {
            prop_assert!(!f.matches("p", "d", start - 1, 0));
        }
        // After range.
        prop_assert!(!f.matches("p", "d", end + 1, 0));
    }

    // ── FI-12: Batch reorder preserves count ────────────────────────────

    #[test]
    fn fi12_batch_preserves_count(n in 2usize..30, window in 2usize..10) {
        let spec = FaultSpec {
            name: "fi12".into(),
            description: String::new(),
            seed: 42,
            faults: vec![FaultType::Reorder {
                filter: EventFilter::match_all(),
                window_size: window,
            }],
        };
        let events: Vec<_> = (0..n)
            .map(|i| make_event(&format!("e{i}"), "p1", "data", i as u64 * 100, i as u64))
            .collect();
        let mut inj = FaultInjector::new(spec);
        let result = inj.process_batch(events.clone());
        prop_assert_eq!(result.len(), events.len(),
            "reorder must preserve event count");
    }

    // ── FI-13: Log count matches ────────────────────────────────────────

    #[test]
    fn fi13_log_count(n in 1usize..20) {
        let spec = FaultSpec {
            name: "fi13".into(),
            description: String::new(),
            seed: 0,
            faults: vec![FaultType::Delay {
                filter: EventFilter::match_all(),
                duration_ms: 100,
            }],
        };
        let mut inj = FaultInjector::new(spec);
        for i in 0..n {
            let evt = make_event(&format!("e{i}"), "p1", "data", i as u64 * 10, i as u64);
            inj.process(evt);
        }
        prop_assert_eq!(inj.log().count(), n);
    }

    // ── FI-14: FaultSpec serde roundtrip ─────────────────────────────────

    #[test]
    fn fi14_spec_serde(seed in 0u64..10000, dur in 1u64..5000) {
        let spec = FaultSpec {
            name: "fi14".into(),
            description: "test".into(),
            seed,
            faults: vec![FaultType::Delay {
                filter: EventFilter::match_all(),
                duration_ms: dur,
            }],
        };
        let json = serde_json::to_string(&spec).unwrap();
        let restored: FaultSpec = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.seed, seed);
        prop_assert_eq!(restored.name.as_str(), "fi14");
    }

    // ── FI-15: EventFilter serde roundtrip ───────────────────────────────

    #[test]
    fn fi15_filter_serde(
        pane in proptest::option::of("[a-z]{3}"),
        start in proptest::option::of(0u64..10000),
        end in proptest::option::of(0u64..10000)
    ) {
        let f = EventFilter {
            pane_id: pane.clone(),
            event_kind: None,
            time_range_start_ms: start,
            time_range_end_ms: end,
            sequence_start: None,
            sequence_end: None,
        };
        let json = serde_json::to_string(&f).unwrap();
        let restored: EventFilter = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.pane_id, pane);
        prop_assert_eq!(restored.time_range_start_ms, start);
    }

    // ── FI-16: FaultLogEntry serde roundtrip ─────────────────────────────

    #[test]
    fn fi16_log_entry_serde(pos in 0u64..10000) {
        let entry = FaultLogEntry {
            fault_type: "delay".into(),
            event_id: "e1".into(),
            original_position: pos,
            description: "test".into(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let restored: FaultLogEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.original_position, pos);
    }

    // ── FI-17: SimEvent serde roundtrip ──────────────────────────────────

    #[test]
    fn fi17_event_serde(ts in 0u64..100000, seq in 0u64..1000) {
        let evt = make_event("e1", "p1", "data", ts, seq);
        let json = serde_json::to_string(&evt).unwrap();
        let restored: SimEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.timestamp_ms, ts);
        prop_assert_eq!(restored.sequence, seq);
    }

    // ── FI-18: Preset pane_death drops after cutoff ─────────────────────

    #[test]
    fn fi18_pane_death(cutoff in 100u64..5000, n_after in 1usize..20) {
        let spec = FaultPresets::pane_death("target", cutoff, 42);
        let mut inj = FaultInjector::new(spec);
        // Before cutoff: kept.
        let e_before = make_event("e0", "target", "data", cutoff.saturating_sub(10), 0);
        if cutoff >= 10 {
            prop_assert_eq!(inj.process(e_before).len(), 1);
        }
        // After cutoff: dropped.
        for i in 0..n_after {
            let evt = make_event(
                &format!("e{}", i + 1),
                "target",
                "data",
                cutoff + i as u64 * 10 + 10,
                i as u64 + 1,
            );
            let result = inj.process(evt);
            prop_assert!(result.is_empty(), "events after cutoff should be dropped");
        }
    }

    // ── FI-19: Network partition delays in window only ───────────────────

    #[test]
    fn fi19_network_partition(start in 100u64..1000, width in 100u64..1000, delay in 100u64..5000) {
        let end = start + width;
        let spec = FaultPresets::network_partition(start, end, delay, 42);
        let mut inj = FaultInjector::new(spec);
        // Inside window: delayed.
        let mid = start + width / 2;
        let e_in = make_event("e_in", "p1", "data", mid, 0);
        let r_in = inj.process(e_in);
        prop_assert_eq!(r_in[0].timestamp_ms, mid + delay);
        // Outside window: not delayed.
        let e_out = make_event("e_out", "p1", "data", end + 100, 1);
        let r_out = inj.process(e_out);
        prop_assert_eq!(r_out[0].timestamp_ms, end + 100);
    }

    // ── FI-20: JSONL line count ─────────────────────────────────────────

    #[test]
    fn fi20_jsonl_lines(n in 1usize..20) {
        let mut log = FaultLog::new();
        for i in 0..n {
            log.record("delay", &format!("e{i}"), i as u64, "test");
        }
        let jsonl = log.to_jsonl();
        let line_count = jsonl.lines().count();
        prop_assert_eq!(line_count, n);
    }

    // ── FI-21: Non-matching events untouched ────────────────────────────

    #[test]
    fn fi21_non_matching_untouched(ts in 0u64..10000, seq in 0u64..1000) {
        let spec = FaultSpec {
            name: "fi21".into(),
            description: String::new(),
            seed: 0,
            faults: vec![FaultType::Delay {
                filter: EventFilter {
                    pane_id: Some("target".into()),
                    ..EventFilter::match_all()
                },
                duration_ms: 999,
            }],
        };
        let mut inj = FaultInjector::new(spec);
        let evt = make_event("e1", "other_pane", "data", ts, seq);
        let result = inj.process(evt);
        prop_assert_eq!(result.len(), 1);
        prop_assert_eq!(result[0].timestamp_ms, ts, "non-matching event should be unchanged");
        prop_assert_eq!(inj.log().count(), 0, "no faults should be logged");
    }
}
