//! Property-based tests for Replay Provenance (ft-og6q6.3.4).
//!
//! Verifies invariants of ReplayProvenanceEmitter, DecisionExplanationTrace,
//! ReplayAuditTrail, and tamper detection.

use frankenterm_core::replay_provenance::{
    AuditEntryParams, DecisionExplanationTrace, DecisionType,
    ExplanationLink, ExplanationTraceCollector, ProvenanceConfig,
    ProvenanceRecordParams, ProvenanceVerbosity, ReplayAuditEntry, ReplayAuditTrail,
    ReplayProvenanceEmitter, REPLAY_AUDIT_GENESIS,
    verify_chain,
};
use proptest::prelude::*;
use serde_json::json;

// ── Strategies ─────────────────────────────────────────────────────────

fn arb_decision_type() -> impl Strategy<Value = DecisionType> {
    prop_oneof![
        Just(DecisionType::PatternMatch),
        Just(DecisionType::WorkflowStep),
        Just(DecisionType::PolicyEvaluation),
        Just(DecisionType::SideEffectBarrier),
        Just(DecisionType::MergeReorder),
        Just(DecisionType::OverrideApplied),
        Just(DecisionType::CheckpointCreate),
        Just(DecisionType::FaultInjection),
    ]
}

fn arb_verbosity() -> impl Strategy<Value = ProvenanceVerbosity> {
    prop_oneof![
        Just(ProvenanceVerbosity::Minimal),
        Just(ProvenanceVerbosity::Standard),
        Just(ProvenanceVerbosity::Verbose),
    ]
}

fn arb_record_params() -> impl Strategy<Value = ProvenanceRecordParams> {
    (
        "[a-z0-9]{4,12}",     // event_id
        arb_decision_type(),
        "[a-z_]{3,15}",       // rule_id
        "[a-f0-9]{8,16}",     // definition_hash
        "[a-z ]{3,20}",       // output_summary
        0..100_000_u64,        // wall_clock_ms
        0..100_000_u64,        // virtual_clock_ms
    )
        .prop_map(
            |(event_id, dt, rule_id, def_hash, output, wall, virt)| ProvenanceRecordParams {
                event_id,
                decision_type: dt,
                rule_id,
                definition_hash: def_hash,
                output_summary: output,
                wall_clock_ms: wall,
                virtual_clock_ms: virt,
                input_data: json!({"ts": wall}),
                event_context: Some(json!({"v": virt})),
            },
        )
}

fn arb_audit_params() -> impl Strategy<Value = AuditEntryParams> {
    (
        "[a-z0-9]{6,12}",    // replay_run_id
        "[a-z_]{3,10}",      // actor
        0..100_000_u64,       // started_at_ms
        100_000..200_000_u64, // completed_at_ms
        "[a-z.]{5,20}",      // artifact_ref
        0..100_u64,           // decision_count
        0..10_u64,            // anomaly_count
    )
        .prop_map(
            |(run_id, actor, start, end, art, dec, anom)| AuditEntryParams {
                replay_run_id: run_id,
                actor,
                started_at_ms: start,
                completed_at_ms: end,
                artifact_ref: art,
                override_ref: None,
                decision_count: dec,
                anomaly_count: anom,
            },
        )
}

// ── Emitter Properties ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // PE-1: Position monotonically increases
    #[test]
    fn position_monotone(n in 1..30_usize) {
        let emitter = ReplayProvenanceEmitter::with_defaults("run_mono".into());
        let mut positions = Vec::new();
        for i in 0..n {
            let p = emitter.record(ProvenanceRecordParams {
                event_id: format!("e{i}"),
                decision_type: DecisionType::PatternMatch,
                rule_id: "r".into(),
                definition_hash: "h".into(),
                output_summary: "ok".into(),
                wall_clock_ms: i as u64 * 100,
                virtual_clock_ms: i as u64 * 50,
                input_data: json!({"i": i}),
                event_context: None,
            });
            positions.push(p);
        }
        for i in 1..positions.len() {
            prop_assert!(
                positions[i] == positions[i - 1] + 1,
                "Position not monotone at {}", i
            );
        }
    }

    // PE-2: All entries have correct replay_run_id
    #[test]
    fn run_id_consistent(
        run_id in "[a-z]{5,15}",
        n in 1..10_usize,
    ) {
        let emitter = ReplayProvenanceEmitter::with_defaults(run_id.clone());
        for i in 0..n {
            emitter.record(ProvenanceRecordParams {
                event_id: format!("e{i}"),
                decision_type: DecisionType::PatternMatch,
                rule_id: "r".into(),
                definition_hash: "h".into(),
                output_summary: "ok".into(),
                wall_clock_ms: 0,
                virtual_clock_ms: 0,
                input_data: json!(null),
                event_context: None,
            });
        }
        for entry in emitter.entries() {
            prop_assert_eq!(&entry.replay_run_id, &run_id);
        }
    }

    // PE-3: Input hash is SHA-256 hex (64 chars)
    #[test]
    fn input_hash_format(params in arb_record_params()) {
        let emitter = ReplayProvenanceEmitter::with_defaults("run".into());
        emitter.record(params);
        let entries = emitter.entries();
        let hash_len = entries[0].input_hash.len();
        let all_hex = entries[0].input_hash.chars().all(|c| c.is_ascii_hexdigit());
        prop_assert_eq!(hash_len, 64);
        prop_assert!(all_hex);
    }

    // PE-4: Minimal verbosity never includes input_data or event_context
    #[test]
    fn minimal_no_data(params in arb_record_params()) {
        let config = ProvenanceConfig {
            verbosity: ProvenanceVerbosity::Minimal,
            max_memory_entries: 100,
        };
        let emitter = ReplayProvenanceEmitter::new("run".into(), config);
        emitter.record(params);
        let entries = emitter.entries();
        prop_assert!(entries[0].input_data.is_none());
        prop_assert!(entries[0].event_context.is_none());
    }

    // PE-5: Verbose verbosity always includes input_data
    #[test]
    fn verbose_has_data(params in arb_record_params()) {
        let config = ProvenanceConfig {
            verbosity: ProvenanceVerbosity::Verbose,
            max_memory_entries: 100,
        };
        let emitter = ReplayProvenanceEmitter::new("run".into(), config);
        emitter.record(params);
        let entries = emitter.entries();
        prop_assert!(entries[0].input_data.is_some());
    }

    // PE-6: FIFO eviction keeps most recent entries
    #[test]
    fn fifo_eviction_keeps_recent(
        cap in 3..10_usize,
        n in 10..30_usize,
    ) {
        let config = ProvenanceConfig {
            verbosity: ProvenanceVerbosity::Minimal,
            max_memory_entries: cap,
        };
        let emitter = ReplayProvenanceEmitter::new("run".into(), config);
        for i in 0..n {
            emitter.record(ProvenanceRecordParams {
                event_id: format!("e{i}"),
                decision_type: DecisionType::PatternMatch,
                rule_id: "r".into(),
                definition_hash: "h".into(),
                output_summary: "ok".into(),
                wall_clock_ms: 0,
                virtual_clock_ms: 0,
                input_data: json!(null),
                event_context: None,
            });
        }
        let entries = emitter.entries();
        prop_assert!(entries.len() <= cap);
        // Last entry should be the most recently recorded
        let last_eid = entries[entries.len() - 1].event_id.clone();
        prop_assert_eq!(last_eid, format!("e{}", n - 1));
    }

    // PE-7: Drain clears buffer
    #[test]
    fn drain_clears(n in 1..20_usize) {
        let emitter = ReplayProvenanceEmitter::with_defaults("run".into());
        for i in 0..n {
            emitter.record(ProvenanceRecordParams {
                event_id: format!("e{i}"),
                decision_type: DecisionType::PatternMatch,
                rule_id: "r".into(),
                definition_hash: "h".into(),
                output_summary: "ok".into(),
                wall_clock_ms: 0,
                virtual_clock_ms: 0,
                input_data: json!(null),
                event_context: None,
            });
        }
        let drained = emitter.drain();
        prop_assert_eq!(drained.len(), n);
        prop_assert!(emitter.is_empty());
    }

    // PE-8: Filter by type returns correct subset
    #[test]
    fn filter_by_type_correct(
        params in prop::collection::vec(arb_record_params(), 5..20),
    ) {
        let emitter = ReplayProvenanceEmitter::with_defaults("run".into());
        for p in &params {
            emitter.record(ProvenanceRecordParams {
                event_id: p.event_id.clone(),
                decision_type: p.decision_type,
                rule_id: p.rule_id.clone(),
                definition_hash: p.definition_hash.clone(),
                output_summary: p.output_summary.clone(),
                wall_clock_ms: p.wall_clock_ms,
                virtual_clock_ms: p.virtual_clock_ms,
                input_data: p.input_data.clone(),
                event_context: p.event_context.clone(),
            });
        }
        let total = emitter.len();
        let mut sum = 0;
        let types = [
            DecisionType::PatternMatch, DecisionType::WorkflowStep,
            DecisionType::PolicyEvaluation, DecisionType::SideEffectBarrier,
            DecisionType::MergeReorder, DecisionType::OverrideApplied,
            DecisionType::CheckpointCreate, DecisionType::FaultInjection,
        ];
        for dt in types {
            let filtered = emitter.entries_of_type(dt);
            for entry in &filtered {
                prop_assert_eq!(entry.decision_type, dt);
            }
            sum += filtered.len();
        }
        prop_assert_eq!(sum, total, "Union of all type filters must equal total");
    }

    // PE-9: JSONL roundtrip preserves entries
    #[test]
    fn jsonl_roundtrip(n in 1..10_usize) {
        let emitter = ReplayProvenanceEmitter::with_defaults("run".into());
        for i in 0..n {
            emitter.record(ProvenanceRecordParams {
                event_id: format!("e{i}"),
                decision_type: DecisionType::PatternMatch,
                rule_id: "r".into(),
                definition_hash: "h".into(),
                output_summary: "ok".into(),
                wall_clock_ms: i as u64 * 100,
                virtual_clock_ms: i as u64 * 50,
                input_data: json!({"i": i}),
                event_context: None,
            });
        }
        let jsonl = emitter.to_jsonl();
        let restored = ReplayProvenanceEmitter::from_jsonl(&jsonl).unwrap();
        prop_assert_eq!(restored.len(), n);
        for i in 0..n {
            let eid = restored[i].event_id.clone();
            prop_assert_eq!(eid, format!("e{i}"));
        }
    }
}

// ── Explanation Trace Properties ───────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // ET-1: Mismatch detection correct
    #[test]
    fn mismatch_detection(
        hash_a in "[a-f0-9]{8,16}",
        hash_b in "[a-f0-9]{8,16}",
    ) {
        let trace = DecisionExplanationTrace::single(
            0, "e".into(), "r".into(),
            hash_a.clone(), hash_b.clone(), "out".into(),
        );
        let expected_mismatch = hash_a != hash_b;
        prop_assert_eq!(trace.has_counterfactual, expected_mismatch);
        prop_assert_eq!(trace.chain[0].definition_mismatch, expected_mismatch);
    }

    // ET-2: Push link updates counterfactual flag
    #[test]
    fn push_link_updates_flag(
        n_links in 1..5_usize,
        has_mismatch_at in 0..5_usize,
    ) {
        let mut trace = DecisionExplanationTrace::single(
            0, "e".into(), "r".into(), "h".into(), "h".into(), "ok".into(),
        );
        for i in 0..n_links {
            let mismatch = i == has_mismatch_at;
            trace.push_link(ExplanationLink {
                triggering_event_id: format!("e{i}"),
                rule_id: format!("r{i}"),
                replay_definition_hash: "h1".into(),
                artifact_definition_hash: if mismatch { "h2".into() } else { "h1".into() },
                definition_mismatch: mismatch,
                decision_output: "out".into(),
            });
        }
        let expected = has_mismatch_at < n_links;
        prop_assert_eq!(trace.has_counterfactual, expected);
        prop_assert_eq!(trace.depth(), 1 + n_links);
    }

    // ET-3: Trace serde roundtrip
    #[test]
    fn trace_serde_roundtrip(
        pos in 0..1000_u64,
        eid in "[a-z]{4,8}",
        rid in "[a-z_]{3,10}",
    ) {
        let trace = DecisionExplanationTrace::single(
            pos, eid.clone(), rid.clone(), "h1".into(), "h2".into(), "out".into(),
        );
        let json = serde_json::to_string(&trace).unwrap();
        let back: DecisionExplanationTrace = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(trace.event_position, back.event_position);
        prop_assert_eq!(trace.has_counterfactual, back.has_counterfactual);
        prop_assert_eq!(trace.chain.len(), back.chain.len());
    }

    // ET-4: Collector counterfactual count correct
    #[test]
    fn collector_cf_count(
        n_matching in 0..5_usize,
        n_different in 0..5_usize,
    ) {
        let collector = ExplanationTraceCollector::new();
        for i in 0..n_matching {
            collector.add(DecisionExplanationTrace::single(
                i as u64, "e".into(), "r".into(), "h".into(), "h".into(), "ok".into(),
            ));
        }
        for i in 0..n_different {
            collector.add(DecisionExplanationTrace::single(
                (n_matching + i) as u64, "e".into(), "r".into(), "a".into(), "b".into(), "diff".into(),
            ));
        }
        prop_assert_eq!(collector.len(), n_matching + n_different);
        prop_assert_eq!(collector.counterfactual_count(), n_different);
    }
}

// ── Audit Trail Properties ─────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // AT-1: Chain hash links are consistent
    #[test]
    fn chain_links_consistent(n in 1..15_usize) {
        let trail = ReplayAuditTrail::new();
        for i in 0..n {
            trail.append(AuditEntryParams {
                replay_run_id: format!("run_{i}"),
                actor: "agent".into(),
                started_at_ms: i as u64 * 1000,
                completed_at_ms: i as u64 * 1000 + 500,
                artifact_ref: "art.ftreplay".into(),
                override_ref: None,
                decision_count: 10,
                anomaly_count: 0,
            });
        }
        let entries = trail.entries();
        let first_prev = entries[0].prev_entry_hash.clone();
        prop_assert_eq!(first_prev, REPLAY_AUDIT_GENESIS);
        for i in 1..entries.len() {
            let prev = entries[i].prev_entry_hash.clone();
            let expected = entries[i - 1].hash();
            prop_assert_eq!(prev, expected);
        }
    }

    // AT-2: Verify always passes for untampered chain
    #[test]
    fn untampered_chain_intact(n in 1..15_usize) {
        let trail = ReplayAuditTrail::new();
        for i in 0..n {
            trail.append(AuditEntryParams {
                replay_run_id: format!("run_{i}"),
                actor: "a".into(),
                started_at_ms: i as u64 * 100,
                completed_at_ms: i as u64 * 100 + 50,
                artifact_ref: "art".into(),
                override_ref: None,
                decision_count: 5,
                anomaly_count: 0,
            });
        }
        let v = trail.verify();
        prop_assert!(v.chain_intact, "Untampered chain should be intact");
        prop_assert_eq!(v.total_entries, n as u64);
        prop_assert!(v.missing_ordinals.is_empty());
    }

    // AT-3: Modifying any entry breaks chain
    #[test]
    fn tamper_breaks_chain(
        n in 2..10_usize,
        tamper_idx in 0..10_usize,
    ) {
        let trail = ReplayAuditTrail::new();
        for i in 0..n {
            trail.append(AuditEntryParams {
                replay_run_id: format!("run_{i}"),
                actor: "a".into(),
                started_at_ms: 0,
                completed_at_ms: 100,
                artifact_ref: "art".into(),
                override_ref: None,
                decision_count: 10,
                anomaly_count: 0,
            });
        }
        let mut entries = trail.entries();
        let idx = tamper_idx % n;
        entries[idx].decision_count += 1; // Tamper

        let v = verify_chain(&entries);
        // If we tampered with entry 0, chain break at entry 0 (genesis mismatch won't happen
        // because prev_entry_hash was set correctly; the break is at entry idx+1 which links to
        // the tampered entry). Exception: if idx is the last entry, there's no entry after it to detect.
        if idx < n - 1 {
            prop_assert!(!v.chain_intact, "Tamper at {} should break chain", idx);
        }
        // If idx == 0 and n == 1, the chain is "intact" because there's no entry pointing to it.
        // If idx == last, similar situation.
    }

    // AT-4: Ordinals are sequential
    #[test]
    fn ordinals_sequential(n in 1..20_usize) {
        let trail = ReplayAuditTrail::new();
        for i in 0..n {
            trail.append(AuditEntryParams {
                replay_run_id: format!("run_{i}"),
                actor: "a".into(),
                started_at_ms: 0,
                completed_at_ms: 100,
                artifact_ref: "art".into(),
                override_ref: None,
                decision_count: 0,
                anomaly_count: 0,
            });
        }
        let entries = trail.entries();
        for i in 0..entries.len() {
            prop_assert_eq!(entries[i].ordinal, i as u64);
        }
    }

    // AT-5: Audit entry serde roundtrip
    #[test]
    fn audit_serde_roundtrip(params in arb_audit_params()) {
        let trail = ReplayAuditTrail::new();
        trail.append(params);
        let entries = trail.entries();
        let json = serde_json::to_string(&entries[0]).unwrap();
        let back: ReplayAuditEntry = serde_json::from_str(&json).unwrap();
        let orig_run_id = entries[0].replay_run_id.clone();
        let orig_actor = entries[0].actor.clone();
        prop_assert_eq!(orig_run_id, back.replay_run_id);
        prop_assert_eq!(entries[0].ordinal, back.ordinal);
        prop_assert_eq!(orig_actor, back.actor);
        prop_assert_eq!(entries[0].decision_count, back.decision_count);
    }

    // AT-6: JSONL roundtrip preserves entries
    #[test]
    fn audit_jsonl_roundtrip(n in 1..10_usize) {
        let trail = ReplayAuditTrail::new();
        for i in 0..n {
            trail.append(AuditEntryParams {
                replay_run_id: format!("run_{i}"),
                actor: "a".into(),
                started_at_ms: 0,
                completed_at_ms: 100,
                artifact_ref: "art".into(),
                override_ref: None,
                decision_count: 10,
                anomaly_count: 0,
            });
        }
        let jsonl = trail.to_jsonl();
        let restored = ReplayAuditTrail::from_jsonl(&jsonl).unwrap();
        prop_assert_eq!(restored.len(), n);
    }

    // AT-7: Hash determinism
    #[test]
    fn hash_deterministic(params in arb_audit_params()) {
        let trail = ReplayAuditTrail::new();
        trail.append(params);
        let entries = trail.entries();
        let h1 = entries[0].hash();
        let h2 = entries[0].hash();
        prop_assert_eq!(h1.len(), 64);
        prop_assert_eq!(h1, h2);
    }
}

// ── Serde Properties ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // S-1: DecisionType serde roundtrip
    #[test]
    fn decision_type_serde(dt in arb_decision_type()) {
        let json = serde_json::to_string(&dt).unwrap();
        let back: DecisionType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(dt, back);
    }

    // S-2: Verbosity serde roundtrip
    #[test]
    fn verbosity_serde(v in arb_verbosity()) {
        let json = serde_json::to_string(&v).unwrap();
        let back: ProvenanceVerbosity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(v, back);
    }

    // S-3: ProvenanceConfig serde roundtrip
    #[test]
    fn config_serde(
        verbosity in arb_verbosity(),
        max in 1..10_000_usize,
    ) {
        let config = ProvenanceConfig { verbosity, max_memory_entries: max };
        let json = serde_json::to_string(&config).unwrap();
        let back: ProvenanceConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.verbosity, back.verbosity);
        prop_assert_eq!(config.max_memory_entries, back.max_memory_entries);
    }
}
