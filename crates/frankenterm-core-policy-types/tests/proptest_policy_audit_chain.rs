use frankenterm_core_policy_types::policy_audit_chain::{
    AuditChain, AuditChainConfig, AuditChainEntry, AuditEntryKind,
};
use proptest::prelude::*;

fn audit_kind_strategy() -> impl Strategy<Value = AuditEntryKind> {
    prop::sample::select(vec![
        AuditEntryKind::PolicyDecision,
        AuditEntryKind::QuarantineAction,
        AuditEntryKind::KillSwitchAction,
        AuditEntryKind::ComplianceViolation,
        AuditEntryKind::ComplianceRemediation,
        AuditEntryKind::CredentialAction,
        AuditEntryKind::ForensicExport,
        AuditEntryKind::ConfigChange,
        AuditEntryKind::Osc52Action,
    ])
}

fn audit_text_strategy() -> impl Strategy<Value = String> {
    "[A-Za-z0-9 _./:-]{0,32}"
}

fn audit_event_strategy() -> impl Strategy<Value = (AuditEntryKind, String, String, String, u64)> {
    (
        audit_kind_strategy(),
        audit_text_strategy(),
        audit_text_strategy(),
        audit_text_strategy(),
        any::<u64>(),
    )
}

fn append_event(
    chain: &mut AuditChain,
    (kind, actor, description, entity_ref, timestamp_ms): &(
        AuditEntryKind,
        String,
        String,
        String,
        u64,
    ),
) {
    chain.append(*kind, actor, description, entity_ref, *timestamp_ms);
}

fn exported_entries(chain: &mut AuditChain) -> Vec<AuditChainEntry> {
    serde_json::from_str(&chain.export_json()).expect("audit chain export should parse")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_policy_audit_chain_retention_keeps_verified_suffix(
        max_entries in 0_usize..=16,
        events in prop::collection::vec(audit_event_strategy(), 0..=48),
    ) {
        let effective_capacity = max_entries.max(1);
        let mut chain = AuditChain::new(max_entries);

        for event in &events {
            append_event(&mut chain, event);
        }

        let verification = chain.verify();
        let snapshot = chain.telemetry_snapshot(777);

        prop_assert!(verification.valid, "retained suffix should verify: {verification}");
        prop_assert_eq!(chain.len(), events.len().min(effective_capacity));
        prop_assert_eq!(verification.entries_checked, chain.len());
        prop_assert_eq!(snapshot.max_entries, effective_capacity);
        prop_assert_eq!(snapshot.next_sequence, events.len() as u64);
        prop_assert_eq!(snapshot.counters.entries_appended, events.len() as u64);
        prop_assert_eq!(
            snapshot.counters.entries_evicted,
            events.len().saturating_sub(effective_capacity) as u64,
        );
        prop_assert_eq!(snapshot.counters.verifications_run, 1);
        prop_assert_eq!(snapshot.counters.verification_failures, 0);
    }

    #[test]
    fn proptest_policy_audit_chain_range_queries_match_retained_exports(
        events in prop::collection::vec(audit_event_strategy(), 0..=40),
        start_ms in any::<u64>(),
        end_ms in any::<u64>(),
    ) {
        let mut chain = AuditChain::new(17);
        for event in &events {
            append_event(&mut chain, event);
        }
        let (lo, hi) = if start_ms <= end_ms {
            (start_ms, end_ms)
        } else {
            (end_ms, start_ms)
        };

        let actual_sequences: Vec<u64> = chain
            .entries_in_range(lo, hi)
            .into_iter()
            .map(|entry| entry.sequence)
            .collect();
        let expected_sequences: Vec<u64> = exported_entries(&mut chain)
            .into_iter()
            .filter(|entry| entry.timestamp_ms >= lo && entry.timestamp_ms <= hi)
            .map(|entry| entry.sequence)
            .collect();

        prop_assert_eq!(actual_sequences, expected_sequences);
    }

    #[test]
    fn proptest_policy_audit_chain_kind_queries_partition_retained_entries(
        events in prop::collection::vec(audit_event_strategy(), 0..=40),
        selected_kind in audit_kind_strategy(),
    ) {
        let mut chain = AuditChain::new(23);
        for event in &events {
            append_event(&mut chain, event);
        }

        let actual_sequences: Vec<u64> = chain
            .entries_by_kind(selected_kind)
            .into_iter()
            .map(|entry| entry.sequence)
            .collect();
        let retained = exported_entries(&mut chain);
        let expected_sequences: Vec<u64> = retained
            .iter()
            .filter(|entry| entry.kind == selected_kind)
            .map(|entry| entry.sequence)
            .collect();

        prop_assert_eq!(&actual_sequences, &expected_sequences);
        prop_assert!(actual_sequences.len() <= retained.len());
    }

    #[test]
    fn proptest_policy_audit_chain_sequence_lookup_tracks_retained_window(
        capacity in 1_usize..=16,
        events in prop::collection::vec(audit_event_strategy(), 1..=48),
    ) {
        let mut chain = AuditChain::new(capacity);
        for event in &events {
            append_event(&mut chain, event);
        }

        let retained = exported_entries(&mut chain);
        let first_retained = retained.first().expect("at least one retained entry").sequence;
        let last_retained = retained.last().expect("at least one retained entry").sequence;

        prop_assert_eq!(chain.latest().map(|entry| entry.sequence), Some(last_retained));
        for retained_entry in &retained {
            let found = chain
                .get_by_sequence(retained_entry.sequence)
                .expect("retained sequence should be queryable");
            prop_assert_eq!(&found.chain_hash, &retained_entry.chain_hash);
            prop_assert_eq!(&found.content_hash, &retained_entry.content_hash);
        }
        if first_retained > 0 {
            prop_assert!(chain.get_by_sequence(first_retained - 1).is_none());
        }
    }

    #[test]
    fn proptest_policy_audit_chain_exports_roundtrip_and_count_exports(
        events in prop::collection::vec(audit_event_strategy(), 0..=36),
    ) {
        let mut chain = AuditChain::new(19);
        for event in &events {
            append_event(&mut chain, event);
        }

        let json_entries: Vec<AuditChainEntry> =
            serde_json::from_str(&chain.export_json()).expect("json export should parse");
        let jsonl = chain.export_jsonl();
        let jsonl_entries: Vec<AuditChainEntry> = if jsonl.is_empty() {
            Vec::new()
        } else {
            jsonl
                .lines()
                .map(|line| serde_json::from_str(line).expect("jsonl entry should parse"))
                .collect()
        };
        let snapshot = chain.telemetry_snapshot(42);

        prop_assert_eq!(json_entries.len(), chain.len());
        prop_assert_eq!(jsonl_entries, json_entries);
        prop_assert_eq!(snapshot.counters.exports_completed, 2);
    }

    #[test]
    fn proptest_policy_audit_chain_config_roundtrips_and_clamps_zero_capacity(
        max_entries in 0_usize..=32,
        record_allows in any::<bool>(),
    ) {
        let config = AuditChainConfig {
            max_entries,
            record_allows,
        };
        let encoded = serde_json::to_string(&config).expect("config should serialize");
        let decoded: AuditChainConfig =
            serde_json::from_str(&encoded).expect("config should deserialize");
        let mut chain = AuditChain::from_config(&decoded);

        prop_assert_eq!(decoded, config);
        prop_assert_eq!(chain.records_allows(), record_allows);
        chain.append(AuditEntryKind::PolicyDecision, "actor", "description", "entity", 1);

        let snapshot = chain.telemetry_snapshot(2);
        prop_assert_eq!(chain.len(), 1);
        prop_assert_eq!(snapshot.max_entries, max_entries.max(1));
    }
}
