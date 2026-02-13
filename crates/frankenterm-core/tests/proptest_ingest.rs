//! Property-based tests for the ingest module.
//!
//! Validates:
//! 1. generate_pane_uuid: format invariants (length, hex charset, non-determinism)
//! 2. PaneFingerprint: construction, content_hash behavior, is_same_generation symmetry/reflexivity
//! 3. ObservationDecision: is_observed and ignore_reason correctness
//! 4. PanePriorityOverride: serde JSON roundtrip
//! 5. DiscoveryDiff: default emptiness, change_count arithmetic, non-empty detection
//! 6. PaneCursor: initial state, from_seq, last_seq, capture_delta, emit_gap, resync_seq
//! 7. CapturedSegmentKind: PartialEq semantics

use std::collections::HashMap;

use proptest::prelude::*;

use frankenterm_core::ingest::{
    CapturedSegmentKind, DiscoveryDiff, ObservationDecision, PaneCursor, PaneFingerprint,
    PanePriorityOverride, generate_pane_uuid,
};
use frankenterm_core::wezterm::PaneInfo;

// =============================================================================
// Helpers
// =============================================================================

/// Build a minimal PaneInfo with the given domain, title, and cwd.
fn make_pane_info(pane_id: u64, domain: &str, title: &str, cwd: &str) -> PaneInfo {
    PaneInfo {
        pane_id,
        tab_id: 0,
        window_id: 0,
        domain_id: None,
        domain_name: Some(domain.to_string()),
        workspace: None,
        size: None,
        rows: None,
        cols: None,
        title: Some(title.to_string()),
        cwd: Some(cwd.to_string()),
        tty_name: None,
        cursor_x: None,
        cursor_y: None,
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active: false,
        is_zoomed: false,
        extra: HashMap::new(),
    }
}

// =============================================================================
// Strategies
// =============================================================================

/// Arbitrary non-empty printable strings (domain-like).
fn arb_domain() -> impl Strategy<Value = String> {
    "[a-zA-Z][a-zA-Z0-9_-]{0,30}".prop_map(|s| s)
}

/// Arbitrary title strings.
fn arb_title() -> impl Strategy<Value = String> {
    "[ -~]{0,60}"
}

/// Arbitrary cwd-like strings.
fn arb_cwd() -> impl Strategy<Value = String> {
    "/[a-z]{1,8}(/[a-z]{1,8}){0,4}"
}

/// Arbitrary pane id.
fn arb_pane_id() -> impl Strategy<Value = u64> {
    0u64..10_000
}

/// Arbitrary timestamp (epoch ms).
fn arb_timestamp() -> impl Strategy<Value = i64> {
    0i64..2_000_000_000_000
}

/// Arbitrary content string (for fingerprinting).
fn arb_content() -> impl Strategy<Value = String> {
    proptest::collection::vec("[ -~]{0,80}\n", 0..100)
        .prop_map(|lines| lines.join(""))
}

/// Arbitrary reason string for Ignored / Gap.
fn arb_reason() -> impl Strategy<Value = String> {
    "[a-z_]{1,30}"
}

/// Arbitrary priority value.
fn arb_priority() -> impl Strategy<Value = u32> {
    0u32..1000
}

/// Arbitrary vec of u64 pane ids (for DiscoveryDiff).
fn arb_pane_vec() -> impl Strategy<Value = Vec<u64>> {
    proptest::collection::vec(arb_pane_id(), 0..20)
}

// =============================================================================
// 1. generate_pane_uuid
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// UUID is always exactly 32 characters long.
    #[test]
    fn uuid_length_is_32(
        domain in arb_domain(),
        pane_id in arb_pane_id(),
        created_at in arb_timestamp(),
    ) {
        let uuid = generate_pane_uuid(&domain, pane_id, created_at);
        prop_assert_eq!(uuid.len(), 32, "expected length 32, got {}", uuid.len());
    }

    /// UUID contains only lowercase hex characters [0-9a-f].
    #[test]
    fn uuid_is_lowercase_hex(
        domain in arb_domain(),
        pane_id in arb_pane_id(),
        created_at in arb_timestamp(),
    ) {
        let uuid = generate_pane_uuid(&domain, pane_id, created_at);
        let all_hex = uuid.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
        prop_assert!(all_hex, "UUID contains non-hex or uppercase chars: {}", uuid);
    }

    /// Two calls with identical inputs produce different UUIDs (non-deterministic due to entropy).
    #[test]
    fn uuid_non_deterministic(
        domain in arb_domain(),
        pane_id in arb_pane_id(),
        created_at in arb_timestamp(),
    ) {
        let uuid1 = generate_pane_uuid(&domain, pane_id, created_at);
        let uuid2 = generate_pane_uuid(&domain, pane_id, created_at);
        // With 64 bits of random entropy, collision probability is negligible.
        prop_assert_ne!(uuid1, uuid2, "two UUIDs from same inputs should differ");
    }
}

// =============================================================================
// 2. PaneFingerprint
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// without_content always sets content_hash to 0.
    #[test]
    fn fingerprint_without_content_hash_is_zero(
        pane_id in arb_pane_id(),
        domain in arb_domain(),
        title in arb_title(),
        cwd in arb_cwd(),
    ) {
        let info = make_pane_info(pane_id, &domain, &title, &cwd);
        let fp = PaneFingerprint::without_content(&info);
        prop_assert_eq!(fp.content_hash, 0, "without_content should yield content_hash 0");
    }

    /// Fingerprint with Some(content) may have non-zero content_hash (unless content hashes to 0, which is astronomically unlikely).
    #[test]
    fn fingerprint_with_content_has_hash(
        pane_id in arb_pane_id(),
        domain in arb_domain(),
        title in arb_title(),
        cwd in arb_cwd(),
        content in "hello world [a-z]{5,20}",
    ) {
        let info = make_pane_info(pane_id, &domain, &title, &cwd);
        let fp = PaneFingerprint::new(&info, Some(&content));
        // Non-empty content should generally produce a non-zero hash.
        // While theoretically possible to be 0, it's astronomically unlikely.
        prop_assert_ne!(fp.content_hash, 0, "non-empty content should yield non-zero hash");
    }

    /// is_same_generation is reflexive: fp.is_same_generation(&fp) is always true.
    #[test]
    fn fingerprint_same_generation_reflexive(
        pane_id in arb_pane_id(),
        domain in arb_domain(),
        title in arb_title(),
        cwd in arb_cwd(),
    ) {
        let info = make_pane_info(pane_id, &domain, &title, &cwd);
        let fp = PaneFingerprint::without_content(&info);
        prop_assert!(fp.is_same_generation(&fp), "reflexive: fp must match itself");
    }

    /// is_same_generation is symmetric: if a matches b, then b matches a.
    #[test]
    fn fingerprint_same_generation_symmetric(
        pane_id in arb_pane_id(),
        domain in arb_domain(),
        title in arb_title(),
        cwd in arb_cwd(),
        content_a in arb_content(),
        content_b in arb_content(),
    ) {
        let info = make_pane_info(pane_id, &domain, &title, &cwd);
        // Same domain/title/cwd but different content should still be same generation.
        let fp_a = PaneFingerprint::new(&info, Some(&content_a));
        let fp_b = PaneFingerprint::new(&info, Some(&content_b));
        let a_to_b = fp_a.is_same_generation(&fp_b);
        let b_to_a = fp_b.is_same_generation(&fp_a);
        prop_assert_eq!(a_to_b, b_to_a, "symmetry violated");
    }

    /// Different domains means different generation.
    #[test]
    fn fingerprint_different_domain_not_same_gen(
        pane_id in arb_pane_id(),
        domain_a in arb_domain(),
        domain_b in arb_domain(),
        title in arb_title(),
        cwd in arb_cwd(),
    ) {
        prop_assume!(domain_a != domain_b);
        let info_a = make_pane_info(pane_id, &domain_a, &title, &cwd);
        let info_b = make_pane_info(pane_id, &domain_b, &title, &cwd);
        let fp_a = PaneFingerprint::without_content(&info_a);
        let fp_b = PaneFingerprint::without_content(&info_b);
        prop_assert!(!fp_a.is_same_generation(&fp_b), "different domain should not be same generation");
    }

    /// Different titles means different generation.
    #[test]
    fn fingerprint_different_title_not_same_gen(
        pane_id in arb_pane_id(),
        domain in arb_domain(),
        title_a in "alpha[a-z]{3,10}",
        title_b in "beta[a-z]{3,10}",
        cwd in arb_cwd(),
    ) {
        let info_a = make_pane_info(pane_id, &domain, &title_a, &cwd);
        let info_b = make_pane_info(pane_id, &domain, &title_b, &cwd);
        let fp_a = PaneFingerprint::without_content(&info_a);
        let fp_b = PaneFingerprint::without_content(&info_b);
        prop_assert!(!fp_a.is_same_generation(&fp_b), "different title should not be same generation");
    }

    /// Different cwd means different generation.
    #[test]
    fn fingerprint_different_cwd_not_same_gen(
        pane_id in arb_pane_id(),
        domain in arb_domain(),
        title in arb_title(),
        cwd_a in "/alpha[a-z]{2,8}",
        cwd_b in "/beta[a-z]{2,8}",
    ) {
        let info_a = make_pane_info(pane_id, &domain, &title, &cwd_a);
        let info_b = make_pane_info(pane_id, &domain, &title, &cwd_b);
        let fp_a = PaneFingerprint::without_content(&info_a);
        let fp_b = PaneFingerprint::without_content(&info_b);
        prop_assert!(!fp_a.is_same_generation(&fp_b), "different cwd should not be same generation");
    }

    /// content_hash does NOT affect is_same_generation.
    #[test]
    fn fingerprint_content_hash_ignored_for_generation(
        domain in arb_domain(),
        title in arb_title(),
        cwd in arb_cwd(),
        hash_a in any::<u64>(),
        hash_b in any::<u64>(),
    ) {
        let fp_a = PaneFingerprint {
            domain: domain.clone(),
            initial_title: title.clone(),
            initial_cwd: cwd.clone(),
            content_hash: hash_a,
        };
        let fp_b = PaneFingerprint {
            domain,
            initial_title: title,
            initial_cwd: cwd,
            content_hash: hash_b,
        };
        prop_assert!(
            fp_a.is_same_generation(&fp_b),
            "content_hash should not affect is_same_generation"
        );
    }

    /// Fingerprint domain is inferred from PaneInfo.domain_name.
    #[test]
    fn fingerprint_domain_from_pane_info(
        pane_id in arb_pane_id(),
        domain in arb_domain(),
        title in arb_title(),
        cwd in arb_cwd(),
    ) {
        let info = make_pane_info(pane_id, &domain, &title, &cwd);
        let fp = PaneFingerprint::without_content(&info);
        prop_assert_eq!(fp.domain.as_str(), domain.as_str(), "domain mismatch");
    }

    /// Fingerprint title and cwd come from PaneInfo.
    #[test]
    fn fingerprint_title_cwd_from_pane_info(
        pane_id in arb_pane_id(),
        domain in arb_domain(),
        title in arb_title(),
        cwd in arb_cwd(),
    ) {
        let info = make_pane_info(pane_id, &domain, &title, &cwd);
        let fp = PaneFingerprint::without_content(&info);
        prop_assert_eq!(fp.initial_title.as_str(), title.as_str(), "title mismatch");
        prop_assert_eq!(fp.initial_cwd.as_str(), cwd.as_str(), "cwd mismatch");
    }
}

// =============================================================================
// 3. ObservationDecision
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Observed.is_observed() returns true.
    #[test]
    fn observed_is_observed(
        _pane_id in arb_pane_id(),
    ) {
        let decision = ObservationDecision::Observed;
        prop_assert!(decision.is_observed(), "Observed must return true for is_observed");
    }

    /// Ignored.is_observed() returns false.
    #[test]
    fn ignored_is_not_observed(reason in arb_reason()) {
        let decision = ObservationDecision::Ignored { reason };
        prop_assert!(!decision.is_observed(), "Ignored must return false for is_observed");
    }

    /// Observed.ignore_reason() returns None.
    #[test]
    fn observed_ignore_reason_is_none(_dummy in 0u8..1) {
        let decision = ObservationDecision::Observed;
        let result = decision.ignore_reason().is_none();
        prop_assert!(result, "Observed.ignore_reason() must be None");
    }

    /// Ignored.ignore_reason() returns Some with the correct reason.
    #[test]
    fn ignored_ignore_reason_is_some(reason in arb_reason()) {
        let decision = ObservationDecision::Ignored { reason: reason.clone() };
        let got = decision.ignore_reason();
        let is_some = got.is_some();
        prop_assert!(is_some, "Ignored.ignore_reason() must be Some");
        prop_assert_eq!(got.unwrap(), reason.as_str(), "reason mismatch");
    }

    /// PartialEq: Observed == Observed.
    #[test]
    fn observed_eq_observed(_dummy in 0u8..1) {
        let a = ObservationDecision::Observed;
        let b = ObservationDecision::Observed;
        prop_assert_eq!(a, b);
    }

    /// PartialEq: Ignored with same reason are equal.
    #[test]
    fn ignored_eq_same_reason(reason in arb_reason()) {
        let a = ObservationDecision::Ignored { reason: reason.clone() };
        let b = ObservationDecision::Ignored { reason };
        prop_assert_eq!(a, b);
    }

    /// PartialEq: Ignored with different reasons are not equal.
    #[test]
    fn ignored_neq_different_reason(
        reason_a in "alpha[a-z]{3,10}",
        reason_b in "beta[a-z]{3,10}",
    ) {
        let a = ObservationDecision::Ignored { reason: reason_a };
        let b = ObservationDecision::Ignored { reason: reason_b };
        prop_assert_ne!(a, b);
    }
}

// =============================================================================
// 4. PanePriorityOverride — serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// JSON serialize then deserialize yields the same struct.
    #[test]
    fn priority_override_serde_roundtrip(
        priority in arb_priority(),
        set_at in arb_timestamp(),
        has_expiry in any::<bool>(),
        expires_at in arb_timestamp(),
    ) {
        let original = PanePriorityOverride {
            priority,
            set_at,
            expires_at: if has_expiry { Some(expires_at) } else { None },
        };
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: PanePriorityOverride = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(deserialized.priority, original.priority, "priority mismatch after roundtrip");
        prop_assert_eq!(deserialized.set_at, original.set_at, "set_at mismatch after roundtrip");
        prop_assert_eq!(deserialized.expires_at, original.expires_at, "expires_at mismatch after roundtrip");
    }

    /// Roundtrip through serde_json::Value preserves structure.
    #[test]
    fn priority_override_value_roundtrip(
        priority in arb_priority(),
        set_at in arb_timestamp(),
    ) {
        let original = PanePriorityOverride {
            priority,
            set_at,
            expires_at: None,
        };
        let value = serde_json::to_value(&original).unwrap();
        let deserialized: PanePriorityOverride = serde_json::from_value(value).unwrap();
        prop_assert_eq!(deserialized.priority, original.priority, "priority mismatch");
        prop_assert_eq!(deserialized.set_at, original.set_at, "set_at mismatch");
        let is_none = deserialized.expires_at.is_none();
        prop_assert!(is_none, "expires_at should be None");
    }
}

// =============================================================================
// 5. DiscoveryDiff
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Default DiscoveryDiff is empty.
    #[test]
    fn discovery_diff_default_is_empty(_dummy in 0u8..1) {
        let diff = DiscoveryDiff::default();
        prop_assert!(diff.is_empty(), "default diff must be empty");
        prop_assert_eq!(diff.change_count(), 0, "default diff must have zero changes");
    }

    /// change_count equals the sum of all vec lengths.
    #[test]
    fn discovery_diff_change_count_is_sum(
        new_panes in arb_pane_vec(),
        closed_panes in arb_pane_vec(),
        changed_panes in arb_pane_vec(),
        new_generations in arb_pane_vec(),
    ) {
        let expected = new_panes.len() + closed_panes.len() + changed_panes.len() + new_generations.len();
        let diff = DiscoveryDiff {
            new_panes,
            closed_panes,
            changed_panes,
            new_generations,
        };
        prop_assert_eq!(diff.change_count(), expected, "change_count mismatch");
    }

    /// is_empty returns false when any vec is non-empty.
    #[test]
    fn discovery_diff_non_empty_when_has_panes(
        panes in proptest::collection::vec(arb_pane_id(), 1..10),
        which in 0u8..4,
    ) {
        let mut diff = DiscoveryDiff::default();
        match which {
            0 => diff.new_panes = panes,
            1 => diff.closed_panes = panes,
            2 => diff.changed_panes = panes,
            _ => diff.new_generations = panes,
        }
        prop_assert!(!diff.is_empty(), "diff with panes should not be empty");
    }

    /// is_empty is true only when all vecs are empty.
    #[test]
    fn discovery_diff_is_empty_iff_all_empty(
        new_panes in arb_pane_vec(),
        closed_panes in arb_pane_vec(),
        changed_panes in arb_pane_vec(),
        new_generations in arb_pane_vec(),
    ) {
        let diff = DiscoveryDiff {
            new_panes: new_panes.clone(),
            closed_panes: closed_panes.clone(),
            changed_panes: changed_panes.clone(),
            new_generations: new_generations.clone(),
        };
        let all_empty = new_panes.is_empty()
            && closed_panes.is_empty()
            && changed_panes.is_empty()
            && new_generations.is_empty();
        prop_assert_eq!(diff.is_empty(), all_empty, "is_empty should match all-empty check");
    }
}

// =============================================================================
// 6. PaneCursor
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// PaneCursor::new initializes with expected defaults.
    #[test]
    fn cursor_new_initial_state(pane_id in arb_pane_id()) {
        let cursor = PaneCursor::new(pane_id);
        prop_assert_eq!(cursor.pane_id, pane_id, "pane_id mismatch");
        prop_assert_eq!(cursor.next_seq, 0, "next_seq should be 0");
        prop_assert!(cursor.last_snapshot.is_empty(), "last_snapshot should be empty");
        let no_hash = cursor.last_hash.is_none();
        prop_assert!(no_hash, "last_hash should be None");
        prop_assert!(!cursor.in_gap, "in_gap should be false");
        prop_assert!(!cursor.in_alt_screen, "in_alt_screen should be false");
    }

    /// PaneCursor::from_seq starts at the given sequence number.
    #[test]
    fn cursor_from_seq_initial_state(
        pane_id in arb_pane_id(),
        next_seq in 0u64..1_000_000,
    ) {
        let cursor = PaneCursor::from_seq(pane_id, next_seq);
        prop_assert_eq!(cursor.pane_id, pane_id, "pane_id mismatch");
        prop_assert_eq!(cursor.next_seq, next_seq, "next_seq mismatch");
        prop_assert!(cursor.last_snapshot.is_empty(), "last_snapshot should be empty");
        let no_hash = cursor.last_hash.is_none();
        prop_assert!(no_hash, "last_hash should be None");
        prop_assert!(!cursor.in_gap, "in_gap should be false");
        prop_assert!(!cursor.in_alt_screen, "in_alt_screen should be false");
    }

    /// last_seq returns -1 when next_seq is 0.
    #[test]
    fn cursor_last_seq_minus_one_when_zero(pane_id in arb_pane_id()) {
        let cursor = PaneCursor::new(pane_id);
        prop_assert_eq!(cursor.last_seq(), -1, "last_seq should be -1 when next_seq is 0");
    }

    /// last_seq returns next_seq - 1 when next_seq > 0.
    #[test]
    fn cursor_last_seq_is_next_minus_one(
        pane_id in arb_pane_id(),
        next_seq in 1u64..1_000_000,
    ) {
        let cursor = PaneCursor::from_seq(pane_id, next_seq);
        let expected = (next_seq - 1) as i64;
        prop_assert_eq!(cursor.last_seq(), expected, "last_seq should be next_seq - 1");
    }

    /// capture_delta creates a Delta segment with correct seq and increments next_seq.
    #[test]
    fn cursor_capture_delta_creates_delta(
        pane_id in arb_pane_id(),
        initial_seq in 0u64..1_000_000,
        content in "[ -~]{0,100}",
        captured_at in arb_timestamp(),
    ) {
        let mut cursor = PaneCursor::from_seq(pane_id, initial_seq);
        let segment = cursor.capture_delta(content.clone(), captured_at);

        prop_assert_eq!(segment.pane_id, pane_id, "pane_id mismatch");
        prop_assert_eq!(segment.seq, initial_seq, "seq should equal initial next_seq");
        prop_assert_eq!(segment.content, content, "content mismatch");
        let is_delta = segment.kind == CapturedSegmentKind::Delta;
        prop_assert!(is_delta, "kind should be Delta");
        prop_assert_eq!(segment.captured_at, captured_at, "captured_at mismatch");
        prop_assert_eq!(cursor.next_seq, initial_seq + 1, "next_seq should increment by 1");
        prop_assert!(!cursor.in_gap, "capture_delta should clear in_gap");
    }

    /// Multiple capture_delta calls produce monotonically increasing seq.
    #[test]
    fn cursor_capture_delta_monotonic_seq(
        pane_id in arb_pane_id(),
        count in 1usize..20,
    ) {
        let mut cursor = PaneCursor::new(pane_id);
        let mut prev_seq: Option<u64> = None;
        for i in 0..count {
            let segment = cursor.capture_delta(format!("line {}", i), 1000 + i as i64);
            if let Some(prev) = prev_seq {
                prop_assert!(segment.seq > prev, "seq must be strictly increasing");
            }
            prev_seq = Some(segment.seq);
        }
        prop_assert_eq!(cursor.next_seq, count as u64, "next_seq should equal number of captures");
    }

    /// emit_gap creates a Gap segment and sets in_gap to true.
    #[test]
    fn cursor_emit_gap_creates_gap(
        pane_id in arb_pane_id(),
        initial_seq in 0u64..1_000_000,
        reason in arb_reason(),
    ) {
        let mut cursor = PaneCursor::from_seq(pane_id, initial_seq);
        let segment = cursor.emit_gap(&reason);

        prop_assert_eq!(segment.pane_id, pane_id, "pane_id mismatch");
        prop_assert_eq!(segment.seq, initial_seq, "seq should equal initial next_seq");
        prop_assert!(segment.content.is_empty(), "gap content should be empty");

        let kind_matches = segment.kind == CapturedSegmentKind::Gap { reason: reason.clone() };
        prop_assert!(kind_matches, "kind should be Gap with matching reason");

        prop_assert_eq!(cursor.next_seq, initial_seq + 1, "next_seq should increment by 1");
        prop_assert!(cursor.in_gap, "emit_gap should set in_gap to true");
    }

    /// resync_seq updates next_seq and sets in_gap.
    #[test]
    fn cursor_resync_seq_updates_state(
        pane_id in arb_pane_id(),
        initial_seq in 0u64..500_000,
        storage_seq in 0u64..500_000,
    ) {
        let mut cursor = PaneCursor::from_seq(pane_id, initial_seq);
        cursor.resync_seq(storage_seq);
        prop_assert_eq!(cursor.next_seq, storage_seq + 1, "next_seq should be storage_seq + 1");
        prop_assert!(cursor.in_gap, "resync_seq should set in_gap to true");
    }

    /// capture_delta after emit_gap clears in_gap.
    #[test]
    fn cursor_delta_after_gap_clears_gap(
        pane_id in arb_pane_id(),
        reason in arb_reason(),
    ) {
        let mut cursor = PaneCursor::new(pane_id);
        cursor.emit_gap(&reason);
        prop_assert!(cursor.in_gap, "should be in gap after emit_gap");

        cursor.capture_delta("recovery".to_string(), 2000);
        prop_assert!(!cursor.in_gap, "capture_delta should clear in_gap");
    }

    /// capture_delta after resync_seq uses the resynced sequence number.
    #[test]
    fn cursor_delta_after_resync_uses_new_seq(
        pane_id in arb_pane_id(),
        storage_seq in 10u64..500_000,
    ) {
        let mut cursor = PaneCursor::new(pane_id);
        cursor.resync_seq(storage_seq);
        let segment = cursor.capture_delta("post-resync".to_string(), 3000);
        prop_assert_eq!(segment.seq, storage_seq + 1, "seq should be storage_seq + 1 after resync");
        prop_assert_eq!(cursor.next_seq, storage_seq + 2, "next_seq should be storage_seq + 2");
    }
}

// =============================================================================
// 7. CapturedSegmentKind — PartialEq
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Delta == Delta.
    #[test]
    fn segment_kind_delta_eq_delta(_dummy in 0u8..1) {
        prop_assert_eq!(CapturedSegmentKind::Delta, CapturedSegmentKind::Delta);
    }

    /// Gap == Gap when reasons match.
    #[test]
    fn segment_kind_gap_eq_same_reason(reason in arb_reason()) {
        let a = CapturedSegmentKind::Gap { reason: reason.clone() };
        let b = CapturedSegmentKind::Gap { reason };
        prop_assert_eq!(a, b);
    }

    /// Gap != Gap when reasons differ.
    #[test]
    fn segment_kind_gap_neq_different_reason(
        reason_a in "alpha[a-z]{3,10}",
        reason_b in "beta[a-z]{3,10}",
    ) {
        let a = CapturedSegmentKind::Gap { reason: reason_a };
        let b = CapturedSegmentKind::Gap { reason: reason_b };
        prop_assert_ne!(a, b);
    }

    /// Delta != Gap regardless of reason.
    #[test]
    fn segment_kind_delta_neq_gap(reason in arb_reason()) {
        let delta = CapturedSegmentKind::Delta;
        let gap = CapturedSegmentKind::Gap { reason };
        let not_equal = delta != gap;
        prop_assert!(not_equal, "Delta should not equal Gap");
    }
}
