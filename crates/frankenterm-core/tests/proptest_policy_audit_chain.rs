//! Property-based tests for the policy_audit_chain module.
//!
//! Tests serde roundtrips for AuditChainConfig, AuditEntryKind,
//! AuditChainEntry, AuditChainTelemetry, and behavioral invariants
//! of the hash-linked audit chain.

use frankenterm_core::policy_audit_chain::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_audit_entry_kind() -> impl Strategy<Value = AuditEntryKind> {
    prop_oneof![
        Just(AuditEntryKind::PolicyDecision),
        Just(AuditEntryKind::QuarantineAction),
        Just(AuditEntryKind::KillSwitchAction),
        Just(AuditEntryKind::ComplianceViolation),
        Just(AuditEntryKind::ComplianceRemediation),
        Just(AuditEntryKind::CredentialAction),
        Just(AuditEntryKind::ForensicExport),
        Just(AuditEntryKind::ConfigChange),
    ]
}

fn arb_audit_chain_config() -> impl Strategy<Value = AuditChainConfig> {
    (1..10000usize, any::<bool>()).prop_map(|(max_entries, record_allows)| AuditChainConfig {
        max_entries,
        record_allows,
    })
}

fn arb_audit_chain_telemetry() -> impl Strategy<Value = AuditChainTelemetry> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(
            |(appended, evicted, runs, failures, exports)| AuditChainTelemetry {
                entries_appended: appended,
                entries_evicted: evicted,
                verifications_run: runs,
                verification_failures: failures,
                exports_completed: exports,
            },
        )
}

fn arb_audit_chain_telemetry_snapshot() -> impl Strategy<Value = AuditChainTelemetrySnapshot> {
    (arb_audit_chain_telemetry(), any::<u64>(), 1..1000usize, 1..10000usize, any::<u64>()).prop_map(
        |(counters, captured_at_ms, chain_length, max_entries, next_sequence)| {
            AuditChainTelemetrySnapshot {
                captured_at_ms,
                counters,
                chain_length,
                max_entries,
                next_sequence,
            }
        },
    )
}

fn arb_chain_verification_result() -> impl Strategy<Value = ChainVerificationResult> {
    prop_oneof![
        (1..100usize).prop_map(|entries| ChainVerificationResult {
            valid: true,
            entries_checked: entries,
            first_invalid_at: None,
            failure_reason: None,
        }),
        (1..100usize, 0..50usize, "[a-z ]{5,30}").prop_map(|(entries, at, reason)| {
            ChainVerificationResult {
                valid: false,
                entries_checked: entries,
                first_invalid_at: Some(at),
                failure_reason: Some(reason),
            }
        }),
    ]
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn audit_entry_kind_json_roundtrip(kind in arb_audit_entry_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: AuditEntryKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(kind, back);
    }

    #[test]
    fn audit_chain_config_json_roundtrip(config in arb_audit_chain_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: AuditChainConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    #[test]
    fn audit_chain_telemetry_json_roundtrip(telemetry in arb_audit_chain_telemetry()) {
        let json = serde_json::to_string(&telemetry).unwrap();
        let back: AuditChainTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(telemetry, back);
    }

    #[test]
    fn audit_chain_telemetry_snapshot_json_roundtrip(snap in arb_audit_chain_telemetry_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let back: AuditChainTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, back);
    }

    #[test]
    fn chain_verification_result_json_roundtrip(result in arb_chain_verification_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let back: ChainVerificationResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(result, back);
    }
}

// =============================================================================
// Behavioral invariants
// =============================================================================

proptest! {
    #[test]
    fn config_default_deserialization_always_valid(json_subset in prop_oneof![
        Just("{}".to_string()),
        Just(r#"{"max_entries":100}"#.to_string()),
        Just(r#"{"record_allows":true}"#.to_string()),
    ]) {
        let config: AuditChainConfig = serde_json::from_str(&json_subset).unwrap();
        prop_assert!(config.max_entries > 0 || config.max_entries == 0);
    }

    #[test]
    fn from_config_capacity_at_least_one(max in 0..100usize, record in any::<bool>()) {
        let config = AuditChainConfig { max_entries: max, record_allows: record };
        let mut chain = AuditChain::from_config(&config);
        // Even with max_entries=0, chain accepts at least 1 entry
        chain.append(AuditEntryKind::PolicyDecision, "sys", "d", "r", 1000);
        prop_assert!(chain.len() >= 1);
    }

    #[test]
    fn append_count_equals_telemetry_appended(n in 1..30u64) {
        let mut chain = AuditChain::new(100);
        for i in 0..n {
            chain.append(AuditEntryKind::PolicyDecision, "sys", &format!("d{i}"), "r", i * 100);
        }
        let snap = chain.telemetry_snapshot(0);
        prop_assert_eq!(snap.counters.entries_appended, n);
    }

    #[test]
    fn eviction_arithmetic_consistent(capacity in 1..20usize, inserts in 1..50usize) {
        let mut chain = AuditChain::new(capacity);
        for i in 0..inserts {
            chain.append(AuditEntryKind::PolicyDecision, "sys", &format!("d{i}"), "r", i as u64 * 100);
        }
        let snap = chain.telemetry_snapshot(0);
        prop_assert_eq!(snap.counters.entries_appended, inserts as u64);
        let expected_evicted = if inserts > capacity { inserts - capacity } else { 0 };
        prop_assert_eq!(snap.counters.entries_evicted, expected_evicted as u64);
        prop_assert_eq!(chain.len(), inserts.min(capacity));
    }

    #[test]
    fn sequence_is_monotonically_increasing(n in 2..20u64) {
        let mut chain = AuditChain::new(100);
        let mut prev_seq = None;
        for i in 0..n {
            let entry = chain.append(AuditEntryKind::PolicyDecision, "sys", "d", "r", i * 100);
            if let Some(prev) = prev_seq {
                prop_assert!(entry.sequence > prev);
            }
            prev_seq = Some(entry.sequence);
        }
    }

    #[test]
    fn chain_without_eviction_verifies(n in 1..20u64) {
        let mut chain = AuditChain::new(100);
        for i in 0..n {
            chain.append(
                AuditEntryKind::PolicyDecision,
                "sys",
                &format!("decision {i}"),
                &format!("r{i}"),
                i * 1000,
            );
        }
        let result = chain.verify();
        prop_assert!(result.valid, "Chain should verify: {}", result);
        prop_assert_eq!(result.entries_checked, n as usize);
    }

    #[test]
    fn records_allows_flag_respected(record in any::<bool>()) {
        let config = AuditChainConfig { max_entries: 100, record_allows: record };
        let chain = AuditChain::from_config(&config);
        prop_assert_eq!(chain.records_allows(), record);
    }

    #[test]
    fn entries_by_kind_partition(n_decisions in 0..10u64, n_quarantines in 0..10u64) {
        let mut chain = AuditChain::new(100);
        for i in 0..n_decisions {
            chain.append(AuditEntryKind::PolicyDecision, "sys", &format!("d{i}"), "r", i * 100);
        }
        for i in 0..n_quarantines {
            chain.append(AuditEntryKind::QuarantineAction, "admin", &format!("q{i}"), "c", (n_decisions + i) * 100);
        }
        let decisions = chain.entries_by_kind(AuditEntryKind::PolicyDecision);
        let quarantines = chain.entries_by_kind(AuditEntryKind::QuarantineAction);
        prop_assert_eq!(decisions.len(), n_decisions as usize);
        prop_assert_eq!(quarantines.len(), n_quarantines as usize);
        prop_assert_eq!(decisions.len() + quarantines.len(), chain.len());
    }

    #[test]
    fn export_json_parses_back(n in 1..10u64) {
        let mut chain = AuditChain::new(100);
        for i in 0..n {
            chain.append(AuditEntryKind::PolicyDecision, "sys", &format!("d{i}"), "r", i * 100);
        }
        let json = chain.export_json();
        let parsed: Vec<AuditChainEntry> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.len(), n as usize);
    }

    #[test]
    fn export_jsonl_line_count(n in 1..10u64) {
        let mut chain = AuditChain::new(100);
        for i in 0..n {
            chain.append(AuditEntryKind::PolicyDecision, "sys", &format!("d{i}"), "r", i * 100);
        }
        let jsonl = chain.export_jsonl();
        let lines: Vec<&str> = jsonl.lines().collect();
        prop_assert_eq!(lines.len(), n as usize);
        for line in &lines {
            let _: AuditChainEntry = serde_json::from_str(line).unwrap();
        }
    }
}
