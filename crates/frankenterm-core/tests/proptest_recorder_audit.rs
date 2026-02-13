//! Property-based tests for recorder_audit module.
//!
//! Verifies tamper-evident audit log invariants:
//! - Hash chain integrity: append N entries → verify_chain succeeds from genesis
//! - Hash chain tamper detection: mutating any entry breaks the chain
//! - Ordinal monotonicity: ordinals are strictly increasing, contiguous
//! - Access tier total order: A0 < A1 < A2 < A3 < A4
//! - Access tier satisfies transitivity: a.satisfies(b) ∧ b.satisfies(c) → a.satisfies(c)
//! - Authorization monotonicity: higher tier → superset of allowed operations
//! - Default actor tiers: each ActorKind has a fixed default tier
//! - Required tier coverage: every AuditEventType maps to exactly one tier
//! - Memory limit enforcement: len() never exceeds max_memory_entries
//! - Drain preserves chain state: drain + append produces valid chain continuation
//! - Resume preserves chain: log1 → resume → log2 → combined chain is valid
//! - Stats consistency: sum of by_type counts == total_entries
//! - Serde roundtrip: AccessTier, AuthzDecision, AuditEventType, RecorderAuditEntry
//! - Hash determinism: same entry always produces the same SHA-256 hash
//! - Filter correctness: entries_by_type/actor/range return correct subsets
//! - Entry hash length: always 64 hex characters (SHA-256)

use proptest::prelude::*;

use frankenterm_core::policy::ActorKind;
use frankenterm_core::recorder_audit::{
    AccessTier, AuditEventBuilder, AuditEventType, AuditLog, AuditLogConfig, AuthzDecision,
    GENESIS_HASH, RecorderAuditEntry, check_authorization, required_tier_for_event,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_access_tier() -> impl Strategy<Value = AccessTier> {
    prop_oneof![
        Just(AccessTier::A0PublicMetadata),
        Just(AccessTier::A1RedactedQuery),
        Just(AccessTier::A2FullQuery),
        Just(AccessTier::A3PrivilegedRaw),
        Just(AccessTier::A4Admin),
    ]
}

fn arb_actor_kind() -> impl Strategy<Value = ActorKind> {
    prop_oneof![
        Just(ActorKind::Human),
        Just(ActorKind::Robot),
        Just(ActorKind::Mcp),
        Just(ActorKind::Workflow),
    ]
}

fn arb_authz_decision() -> impl Strategy<Value = AuthzDecision> {
    prop_oneof![
        Just(AuthzDecision::Allow),
        Just(AuthzDecision::Deny),
        Just(AuthzDecision::Elevate),
    ]
}

fn arb_event_type() -> impl Strategy<Value = AuditEventType> {
    prop_oneof![
        Just(AuditEventType::RecorderQuery),
        Just(AuditEventType::RecorderQueryPrivileged),
        Just(AuditEventType::RecorderReplay),
        Just(AuditEventType::RecorderExport),
        Just(AuditEventType::AdminRetentionOverride),
        Just(AuditEventType::AdminPurge),
        Just(AuditEventType::AdminPolicyChange),
        Just(AuditEventType::AccessApprovalGranted),
        Just(AuditEventType::AccessApprovalExpired),
        Just(AuditEventType::AccessIncidentMode),
        Just(AuditEventType::AccessDebugMode),
        Just(AuditEventType::RetentionSegmentSealed),
        Just(AuditEventType::RetentionSegmentArchived),
        Just(AuditEventType::RetentionSegmentPurged),
        Just(AuditEventType::RetentionAcceleratedPurge),
    ]
}

/// Generate a sequence of audit event builders with monotonically increasing timestamps.
fn arb_event_sequence(
    max_len: usize,
) -> impl Strategy<Value = Vec<(AuditEventType, ActorKind, u64, AuthzDecision)>> {
    proptest::collection::vec(
        (
            arb_event_type(),
            arb_actor_kind(),
            1u64..=1_000,
            arb_authz_decision(),
        ),
        1..=max_len,
    )
    .prop_map(|events| {
        let mut ts = 0u64;
        events
            .into_iter()
            .map(|(et, ak, delta, decision)| {
                ts += delta;
                (et, ak, ts, decision)
            })
            .collect()
    })
}

fn make_actor(kind: ActorKind) -> frankenterm_core::recorder_audit::ActorIdentity {
    frankenterm_core::recorder_audit::ActorIdentity::new(kind, format!("test-{:?}", kind))
}

// ────────────────────────────────────────────────────────────────────
// Property: Access tier total order
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn access_tier_level_is_injective(a in arb_access_tier(), b in arb_access_tier()) {
        // If tiers are different, levels must be different (injective mapping).
        if a != b {
            prop_assert_ne!(a.level(), b.level(),
                "Different tiers {:?} and {:?} must have different levels", a, b);
        }
    }

    #[test]
    fn access_tier_satisfies_reflexive(tier in arb_access_tier()) {
        // Every tier satisfies itself.
        prop_assert!(tier.satisfies(tier),
            "Tier {:?} must satisfy itself", tier);
    }

    #[test]
    fn access_tier_satisfies_transitivity(
        a in arb_access_tier(),
        b in arb_access_tier(),
        c in arb_access_tier()
    ) {
        // If a.satisfies(b) and b.satisfies(c), then a.satisfies(c).
        if a.satisfies(b) && b.satisfies(c) {
            prop_assert!(a.satisfies(c),
                "Transitivity violated: {:?}.satisfies({:?}) ∧ {:?}.satisfies({:?}) but !{:?}.satisfies({:?})",
                a, b, b, c, a, c);
        }
    }

    #[test]
    fn access_tier_satisfies_antisymmetric(a in arb_access_tier(), b in arb_access_tier()) {
        // If a.satisfies(b) and b.satisfies(a), then a == b.
        if a.satisfies(b) && b.satisfies(a) {
            prop_assert_eq!(a, b,
                "Antisymmetry violated: {:?} and {:?} mutually satisfy but are not equal", a, b);
        }
    }

    #[test]
    fn access_tier_satisfies_matches_level(a in arb_access_tier(), b in arb_access_tier()) {
        // satisfies is equivalent to level comparison.
        prop_assert_eq!(a.satisfies(b), a.level() >= b.level(),
            "satisfies({:?}, {:?}) must match level comparison {} >= {}",
            a, b, a.level(), b.level());
    }

    // ────────────────────────────────────────────────────────────────────
    // Property: Authorization consistency
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn authz_allow_implies_tier_satisfies(actor in arb_actor_kind(), required in arb_access_tier()) {
        let decision = check_authorization(actor, required);
        let default_tier = AccessTier::default_for_actor(actor);
        if decision == AuthzDecision::Allow {
            prop_assert!(default_tier.satisfies(required),
                "Allow for {:?} at {:?} but default tier {:?} doesn't satisfy",
                actor, required, default_tier);
        }
    }

    #[test]
    fn authz_default_tier_always_allowed(actor in arb_actor_kind()) {
        // Every actor must be allowed at their default tier.
        let default_tier = AccessTier::default_for_actor(actor);
        let decision = check_authorization(actor, default_tier);
        prop_assert_eq!(decision, AuthzDecision::Allow,
            "Actor {:?} must be allowed at their default tier {:?}", actor, default_tier);
    }

    #[test]
    fn authz_below_default_always_allowed(actor in arb_actor_kind(), required in arb_access_tier()) {
        // If required tier <= default tier, must be Allow.
        let default_tier = AccessTier::default_for_actor(actor);
        if default_tier.satisfies(required) {
            let decision = check_authorization(actor, required);
            prop_assert_eq!(decision, AuthzDecision::Allow,
                "Actor {:?} with default {:?} must be allowed at {:?}",
                actor, default_tier, required);
        }
    }

    #[test]
    fn authz_a0_always_allowed(actor in arb_actor_kind()) {
        // A0 is the lowest tier; every actor should be allowed.
        let decision = check_authorization(actor, AccessTier::A0PublicMetadata);
        prop_assert_eq!(decision, AuthzDecision::Allow,
            "Actor {:?} must be allowed at A0", actor);
    }

    #[test]
    fn authz_decision_never_allow_above_default_without_elevation(
        actor in arb_actor_kind(),
        required in arb_access_tier()
    ) {
        let default_tier = AccessTier::default_for_actor(actor);
        let decision = check_authorization(actor, required);
        if !default_tier.satisfies(required) {
            // Above default tier: must be Elevate or Deny, never Allow.
            prop_assert_ne!(decision, AuthzDecision::Allow,
                "Actor {:?} at {:?} should not be allowed above default {:?}",
                actor, required, default_tier);
        }
    }

    // ────────────────────────────────────────────────────────────────────
    // Property: Required tier for event
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn required_tier_returns_valid_tier(et in arb_event_type()) {
        let tier = required_tier_for_event(et);
        // Tier level must be in [0, 4].
        prop_assert!(tier.level() <= 4,
            "Required tier for {:?} has invalid level {}", et, tier.level());
    }

    #[test]
    fn admin_events_require_a4(et in arb_event_type()) {
        let tier = required_tier_for_event(et);
        match et {
            AuditEventType::AdminRetentionOverride
            | AuditEventType::AdminPurge
            | AuditEventType::AdminPolicyChange => {
                prop_assert_eq!(tier, AccessTier::A4Admin,
                    "Admin event {:?} must require A4, got {:?}", et, tier);
            }
            _ => { /* other events have varying requirements */ }
        }
    }

    // ────────────────────────────────────────────────────────────────────
    // Property: Hash chain integrity under arbitrary sequences
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn hash_chain_intact_for_arbitrary_sequence(
        events in arb_event_sequence(50)
    ) {
        let config = AuditLogConfig {
            max_memory_entries: 10_000,
            hash_chain_enabled: true,
            ..AuditLogConfig::default()
        };
        let log = AuditLog::new(config);

        for (et, ak, ts, decision) in &events {
            log.append(
                AuditEventBuilder::new(*et, make_actor(*ak), *ts)
                    .with_decision(decision.clone()),
            );
        }

        let entries = log.entries();
        let result = AuditLog::verify_chain(&entries, GENESIS_HASH);
        prop_assert!(result.chain_intact,
            "Chain should be intact for {} entries, first_break_at: {:?}",
            entries.len(), result.first_break_at);
        prop_assert_eq!(result.total_entries, entries.len() as u64);
        prop_assert!(result.missing_ordinals.is_empty());
    }

    #[test]
    fn hash_chain_detects_tamper_at_any_position(
        events in arb_event_sequence(10),
        tamper_offset in 0u64..=100_000
    ) {
        let config = AuditLogConfig {
            max_memory_entries: 10_000,
            hash_chain_enabled: true,
            ..AuditLogConfig::default()
        };
        let log = AuditLog::new(config);

        for (et, ak, ts, decision) in &events {
            log.append(
                AuditEventBuilder::new(*et, make_actor(*ak), *ts)
                    .with_decision(decision.clone()),
            );
        }

        let mut entries = log.entries();
        if entries.len() < 2 {
            return Ok(());
        }

        // Tamper with a random entry (not the last one, so chain break is detectable).
        let tamper_idx = (tamper_offset as usize) % (entries.len() - 1);
        let original_ts = entries[tamper_idx].timestamp_ms;
        entries[tamper_idx].timestamp_ms = original_ts.wrapping_add(1);

        let result = AuditLog::verify_chain(&entries, GENESIS_HASH);
        // The chain should be broken at tamper_idx+1 (or tamper_idx if it's 0 and genesis doesn't match).
        prop_assert!(!result.chain_intact,
            "Chain should detect tamper at index {}", tamper_idx);
        if let Some(break_at) = result.first_break_at {
            // Break must be detected at or after the tampered entry.
            prop_assert!(break_at as usize <= tamper_idx + 1,
                "Break at {} should be near tampered index {}", break_at, tamper_idx);
        }
    }

    // ────────────────────────────────────────────────────────────────────
    // Property: Ordinal monotonicity
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn ordinals_strictly_monotonic(
        events in arb_event_sequence(50)
    ) {
        let log = AuditLog::new(AuditLogConfig {
            max_memory_entries: 10_000,
            ..AuditLogConfig::default()
        });

        let mut appended = Vec::new();
        for (et, ak, ts, decision) in &events {
            let entry = log.append(
                AuditEventBuilder::new(*et, make_actor(*ak), *ts)
                    .with_decision(decision.clone()),
            );
            appended.push(entry);
        }

        for i in 1..appended.len() {
            prop_assert_eq!(appended[i].ordinal, appended[i - 1].ordinal + 1,
                "Ordinals must be contiguous: {} vs {}", appended[i-1].ordinal, appended[i].ordinal);
        }

        prop_assert_eq!(log.next_ordinal(), events.len() as u64);
        prop_assert_eq!(log.total_appended(), events.len() as u64);
    }

    // ────────────────────────────────────────────────────────────────────
    // Property: Memory limit enforcement
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn memory_limit_never_exceeded(
        max_entries in 5usize..=50,
        num_appends in 1usize..=200
    ) {
        let config = AuditLogConfig {
            max_memory_entries: max_entries,
            ..AuditLogConfig::default()
        };
        let log = AuditLog::new(config);

        for i in 0..num_appends {
            log.append(AuditEventBuilder::new(
                AuditEventType::RecorderQuery,
                make_actor(ActorKind::Human),
                (i as u64) * 1000,
            ));

            prop_assert!(log.len() <= max_entries,
                "Log length {} exceeds max {} after {} appends",
                log.len(), max_entries, i + 1);
        }

        // After all appends, verify total_appended tracks all.
        prop_assert_eq!(log.total_appended(), num_appends as u64);

        // Verify entries are from the tail (newest entries preserved).
        if num_appends > max_entries {
            let entries = log.entries();
            let expected_first_ordinal = (num_appends - max_entries) as u64;
            prop_assert_eq!(entries[0].ordinal, expected_first_ordinal,
                "Oldest in-memory entry should have ordinal {}", expected_first_ordinal);
        }
    }

    // ────────────────────────────────────────────────────────────────────
    // Property: Drain preserves chain state
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn drain_then_append_produces_valid_chain(
        events1 in arb_event_sequence(20),
        events2 in arb_event_sequence(20)
    ) {
        let log = AuditLog::new(AuditLogConfig {
            max_memory_entries: 10_000,
            ..AuditLogConfig::default()
        });

        // Phase 1: append and drain.
        for (et, ak, ts, decision) in &events1 {
            log.append(
                AuditEventBuilder::new(*et, make_actor(*ak), *ts)
                    .with_decision(decision.clone()),
            );
        }
        let drained = log.drain();
        prop_assert!(log.is_empty());

        // Phase 2: append more.
        let base_ts = events1.last().map_or(0, |e| e.2);
        for (et, ak, ts, decision) in &events2 {
            log.append(
                AuditEventBuilder::new(*et, make_actor(*ak), base_ts + *ts)
                    .with_decision(decision.clone()),
            );
        }
        let phase2 = log.entries();

        // Combined chain should verify from genesis.
        let mut all = drained;
        all.extend(phase2);
        let result = AuditLog::verify_chain(&all, GENESIS_HASH);
        prop_assert!(result.chain_intact,
            "Combined drain+append chain should be intact, first_break_at: {:?}",
            result.first_break_at);
        prop_assert_eq!(result.total_entries as usize, events1.len() + events2.len());
    }

    // ────────────────────────────────────────────────────────────────────
    // Property: Resume preserves chain continuity
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn resume_produces_valid_chain_continuation(
        events1 in arb_event_sequence(20),
        events2 in arb_event_sequence(20)
    ) {
        // Phase 1: original log.
        let log1 = AuditLog::new(AuditLogConfig {
            max_memory_entries: 10_000,
            ..AuditLogConfig::default()
        });
        for (et, ak, ts, decision) in &events1 {
            log1.append(
                AuditEventBuilder::new(*et, make_actor(*ak), *ts)
                    .with_decision(decision.clone()),
            );
        }
        let phase1_entries = log1.entries();
        let last_hash = log1.last_hash();
        let next_ordinal = log1.next_ordinal();

        // Phase 2: resumed log.
        let log2 = AuditLog::resume(
            AuditLogConfig {
                max_memory_entries: 10_000,
                ..AuditLogConfig::default()
            },
            next_ordinal,
            last_hash,
        );
        let base_ts = events1.last().map_or(0, |e| e.2);
        for (et, ak, ts, decision) in &events2 {
            log2.append(
                AuditEventBuilder::new(*et, make_actor(*ak), base_ts + *ts)
                    .with_decision(decision.clone()),
            );
        }
        let phase2_entries = log2.entries();

        // Combined chain should verify from genesis.
        let mut all = phase1_entries;
        all.extend(phase2_entries);
        let result = AuditLog::verify_chain(&all, GENESIS_HASH);
        prop_assert!(result.chain_intact,
            "Resumed chain should be intact, first_break_at: {:?}", result.first_break_at);
        prop_assert_eq!(result.total_entries as usize, events1.len() + events2.len());
        prop_assert!(result.missing_ordinals.is_empty());
    }

    // ────────────────────────────────────────────────────────────────────
    // Property: Stats consistency
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn stats_counts_are_consistent(
        events in arb_event_sequence(50)
    ) {
        let log = AuditLog::new(AuditLogConfig {
            max_memory_entries: 10_000,
            ..AuditLogConfig::default()
        });

        let mut deny_count = 0u64;
        let mut elevate_count = 0u64;

        for (et, ak, ts, decision) in &events {
            log.append(
                AuditEventBuilder::new(*et, make_actor(*ak), *ts)
                    .with_decision(decision.clone()),
            );
            match decision {
                AuthzDecision::Deny => deny_count += 1,
                AuthzDecision::Elevate => elevate_count += 1,
                AuthzDecision::Allow => {}
            }
        }

        let stats = log.stats();

        // Total entries matches.
        prop_assert_eq!(stats.total_entries, events.len() as u64);

        // Sum of by_type counts == total.
        let type_sum: u64 = stats.by_type.values().sum();
        prop_assert_eq!(type_sum, stats.total_entries,
            "Sum of by_type ({}) must equal total_entries ({})", type_sum, stats.total_entries);

        // Sum of by_actor counts == total.
        let actor_sum: u64 = stats.by_actor.values().sum();
        prop_assert_eq!(actor_sum, stats.total_entries,
            "Sum of by_actor ({}) must equal total_entries ({})", actor_sum, stats.total_entries);

        // Deny and elevate counts match.
        prop_assert_eq!(stats.denied_count, deny_count);
        prop_assert_eq!(stats.elevated_count, elevate_count);

        // Ordinal range.
        if let Some((first, last)) = stats.ordinal_range {
            prop_assert_eq!(first, 0);
            prop_assert_eq!(last, events.len() as u64 - 1);
        }
    }

    // ────────────────────────────────────────────────────────────────────
    // Property: Filter correctness
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn entries_by_type_returns_correct_subset(
        events in arb_event_sequence(30),
        filter_type in arb_event_type()
    ) {
        let log = AuditLog::new(AuditLogConfig {
            max_memory_entries: 10_000,
            ..AuditLogConfig::default()
        });

        for (et, ak, ts, decision) in &events {
            log.append(
                AuditEventBuilder::new(*et, make_actor(*ak), *ts)
                    .with_decision(decision.clone()),
            );
        }

        let filtered = log.entries_by_type(filter_type);
        let expected_count = events.iter().filter(|(et, _, _, _)| *et == filter_type).count();

        prop_assert_eq!(filtered.len(), expected_count,
            "entries_by_type({:?}) returned {} but expected {}", filter_type, filtered.len(), expected_count);

        // All returned entries have the correct type.
        for entry in &filtered {
            prop_assert_eq!(entry.event_type, filter_type);
        }
    }

    #[test]
    fn entries_by_actor_returns_correct_subset(
        events in arb_event_sequence(30),
        filter_actor in arb_actor_kind()
    ) {
        let log = AuditLog::new(AuditLogConfig {
            max_memory_entries: 10_000,
            ..AuditLogConfig::default()
        });

        for (et, ak, ts, decision) in &events {
            log.append(
                AuditEventBuilder::new(*et, make_actor(*ak), *ts)
                    .with_decision(decision.clone()),
            );
        }

        let filtered = log.entries_by_actor(filter_actor);
        let expected_count = events.iter().filter(|(_, ak, _, _)| *ak == filter_actor).count();

        prop_assert_eq!(filtered.len(), expected_count,
            "entries_by_actor({:?}) returned {} but expected {}", filter_actor, filtered.len(), expected_count);

        for entry in &filtered {
            prop_assert_eq!(entry.actor.kind, filter_actor);
        }
    }

    #[test]
    fn entries_in_range_returns_correct_subset(
        events in arb_event_sequence(30),
        range_start in 0u64..=500,
        range_width in 1u64..=2000
    ) {
        let log = AuditLog::new(AuditLogConfig {
            max_memory_entries: 10_000,
            ..AuditLogConfig::default()
        });

        let mut timestamps = Vec::new();
        for (et, ak, ts, decision) in &events {
            log.append(
                AuditEventBuilder::new(*et, make_actor(*ak), *ts)
                    .with_decision(decision.clone()),
            );
            timestamps.push(*ts);
        }

        let range_end = range_start + range_width;
        let filtered = log.entries_in_range(range_start, range_end);
        let expected_count = timestamps.iter()
            .filter(|&&ts| ts >= range_start && ts <= range_end)
            .count();

        prop_assert_eq!(filtered.len(), expected_count,
            "entries_in_range({}, {}) returned {} but expected {}",
            range_start, range_end, filtered.len(), expected_count);

        for entry in &filtered {
            prop_assert!(entry.timestamp_ms >= range_start && entry.timestamp_ms <= range_end,
                "Entry timestamp {} outside range [{}, {}]",
                entry.timestamp_ms, range_start, range_end);
        }
    }

    // ────────────────────────────────────────────────────────────────────
    // Property: Hash determinism and length
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn entry_hash_is_deterministic_and_correct_length(
        et in arb_event_type(),
        ak in arb_actor_kind(),
        ts in 1u64..=1_000_000
    ) {
        let log = AuditLog::new(AuditLogConfig::default());
        let entry = log.append(AuditEventBuilder::new(et, make_actor(ak), ts));

        let hash1 = entry.hash();
        let hash2 = entry.hash();

        prop_assert_eq!(&hash1, &hash2, "Hash must be deterministic");
        prop_assert_eq!(hash1.len(), 64, "SHA-256 hex must be 64 chars, got {}", hash1.len());

        // Must be valid hex.
        prop_assert!(hash1.chars().all(|c| c.is_ascii_hexdigit()),
            "Hash must be valid hex: {}", hash1);
    }

    // ────────────────────────────────────────────────────────────────────
    // Property: Serde roundtrip
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn access_tier_serde_roundtrip(tier in arb_access_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let parsed: AccessTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tier, parsed);
    }

    #[test]
    fn authz_decision_serde_roundtrip(decision in arb_authz_decision()) {
        let json = serde_json::to_string(&decision).unwrap();
        let parsed: AuthzDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decision, parsed);
    }

    #[test]
    fn audit_event_type_serde_roundtrip(et in arb_event_type()) {
        let json = serde_json::to_string(&et).unwrap();
        let parsed: AuditEventType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(et, parsed);
    }

    #[test]
    fn audit_entry_serde_roundtrip(
        et in arb_event_type(),
        ak in arb_actor_kind(),
        ts in 1u64..=1_000_000,
        decision in arb_authz_decision()
    ) {
        let log = AuditLog::new(AuditLogConfig::default());
        let entry = log.append(
            AuditEventBuilder::new(et, make_actor(ak), ts)
                .with_decision(decision)
                .with_pane_ids(vec![1, 2])
                .with_query("test query"),
        );

        let json = serde_json::to_string(&entry).unwrap();
        let parsed: RecorderAuditEntry = serde_json::from_str(&json).unwrap();

        // Hash must be preserved across serde (compute before any partial moves).
        let entry_hash = entry.hash();
        let parsed_hash = parsed.hash();
        prop_assert_eq!(&entry_hash, &parsed_hash,
            "Hash must survive serde roundtrip");

        prop_assert_eq!(entry.ordinal, parsed.ordinal);
        prop_assert_eq!(entry.event_type, parsed.event_type);
        prop_assert_eq!(entry.actor, parsed.actor);
        prop_assert_eq!(entry.timestamp_ms, parsed.timestamp_ms);
        prop_assert_eq!(&entry.prev_entry_hash, &parsed.prev_entry_hash);
        prop_assert_eq!(entry.decision, parsed.decision);
        prop_assert_eq!(&entry.policy_version, &parsed.policy_version);
    }

    // ────────────────────────────────────────────────────────────────────
    // Property: Hash chain disabled
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn hash_chain_disabled_all_genesis(
        events in arb_event_sequence(20)
    ) {
        let config = AuditLogConfig {
            hash_chain_enabled: false,
            max_memory_entries: 10_000,
            ..AuditLogConfig::default()
        };
        let log = AuditLog::new(config);

        for (et, ak, ts, decision) in &events {
            let entry = log.append(
                AuditEventBuilder::new(*et, make_actor(*ak), *ts)
                    .with_decision(decision.clone()),
            );
            // When hash chain is disabled, all entries have genesis hash as prev.
            prop_assert_eq!(entry.prev_entry_hash, GENESIS_HASH,
                "Entry ordinal {} should have genesis hash when chain disabled", entry.ordinal);
        }
    }

    // ────────────────────────────────────────────────────────────────────
    // Property: Different entries have different hashes
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn consecutive_entries_have_different_hashes(
        events in arb_event_sequence(10)
    ) {
        let log = AuditLog::new(AuditLogConfig {
            max_memory_entries: 10_000,
            ..AuditLogConfig::default()
        });

        let mut prev_hash = String::new();
        for (et, ak, ts, decision) in &events {
            let entry = log.append(
                AuditEventBuilder::new(*et, make_actor(*ak), *ts)
                    .with_decision(decision.clone()),
            );
            let h = entry.hash();
            if !prev_hash.is_empty() {
                // Consecutive entries should have different hashes (extremely high probability).
                prop_assert_ne!(&h, &prev_hash,
                    "Consecutive entries should have different hashes (ordinal {})", entry.ordinal);
            }
            prev_hash = h;
        }
    }

    // ────────────────────────────────────────────────────────────────────
    // Property: Verify chain gap detection
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn verify_chain_detects_ordinal_gaps(
        events in arb_event_sequence(10),
        remove_idx in 0usize..=8
    ) {
        if events.len() < 3 {
            return Ok(());
        }
        let log = AuditLog::new(AuditLogConfig {
            max_memory_entries: 10_000,
            ..AuditLogConfig::default()
        });

        for (et, ak, ts, decision) in &events {
            log.append(
                AuditEventBuilder::new(*et, make_actor(*ak), *ts)
                    .with_decision(decision.clone()),
            );
        }

        let mut entries = log.entries();
        let idx = remove_idx % (entries.len().saturating_sub(1)).max(1);
        if idx < entries.len() {
            let removed_ordinal = entries[idx].ordinal;
            entries.remove(idx);

            if entries.len() >= 2 {
                let result = AuditLog::verify_chain(&entries, GENESIS_HASH);
                // Should detect the gap (unless we removed first or last).
                if idx > 0 && idx < events.len() - 1 {
                    prop_assert!(result.missing_ordinals.contains(&removed_ordinal),
                        "Should detect missing ordinal {} at index {}", removed_ordinal, idx);
                }
            }
        }
    }
}
