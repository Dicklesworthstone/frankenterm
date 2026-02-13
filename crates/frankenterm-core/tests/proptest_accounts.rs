//! Property-based tests for the accounts module.
//!
//! Covers serde roundtrips, selection invariants, threshold semantics,
//! reset-time parsing, exhaustion info construction, and from_caut conversion.
//!
//! 45 property tests across 15 proptest! blocks.

use std::collections::HashMap;

use proptest::prelude::*;

use frankenterm_core::accounts::{
    build_exhaustion_info, find_earliest_reset, parse_reset_at_ms, select_account,
    AccountExhaustionInfo, AccountRecord, AccountSelectionConfig, AccountSelectionResult,
    CandidateAccount, FilteredAccount, SelectionExplanation,
};
use frankenterm_core::caut::{CautAccountUsage, CautService};

// =============================================================================
// Strategies
// =============================================================================

/// Safe string strategy: alphanumeric + underscore/hyphen, 1..30 chars.
fn arb_safe_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_-]{1,30}"
}

/// Percent remaining in valid range [0.0, 100.0].
fn arb_percent() -> impl Strategy<Value = f64> {
    (0u64..=10_000u64).prop_map(|v| v as f64 / 100.0)
}

/// Reasonable positive timestamp in epoch milliseconds.
fn arb_timestamp() -> impl Strategy<Value = i64> {
    1_000_000_000i64..2_000_000_000_000i64
}

/// Optional timestamp.
fn arb_opt_timestamp() -> impl Strategy<Value = Option<i64>> {
    prop_oneof![
        3 => Just(None),
        7 => arb_timestamp().prop_map(Some),
    ]
}

/// Optional safe string.
fn arb_opt_string() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        3 => Just(None),
        7 => arb_safe_string().prop_map(Some),
    ]
}

/// Optional token count (non-negative i64).
fn arb_opt_tokens() -> impl Strategy<Value = Option<i64>> {
    prop_oneof![
        3 => Just(None),
        7 => (0i64..1_000_000i64).prop_map(Some),
    ]
}

/// Generate a complete AccountRecord with controlled randomness.
fn arb_account_record() -> impl Strategy<Value = AccountRecord> {
    (
        arb_safe_string(),    // account_id
        arb_safe_string(),    // service
        arb_opt_string(),     // name
        arb_percent(),        // percent_remaining
        arb_opt_string(),     // reset_at
        arb_opt_tokens(),     // tokens_used
        arb_opt_tokens(),     // tokens_remaining
        arb_opt_tokens(),     // tokens_limit
        arb_timestamp(),      // last_refreshed_at
        arb_opt_timestamp(),  // last_used_at
        arb_timestamp(),      // created_at
        arb_timestamp(),      // updated_at
    )
        .prop_map(
            |(
                account_id,
                service,
                name,
                percent_remaining,
                reset_at,
                tokens_used,
                tokens_remaining,
                tokens_limit,
                last_refreshed_at,
                last_used_at,
                created_at,
                updated_at,
            )| {
                AccountRecord {
                    id: 0,
                    account_id,
                    service,
                    name,
                    percent_remaining,
                    reset_at,
                    tokens_used,
                    tokens_remaining,
                    tokens_limit,
                    last_refreshed_at,
                    last_used_at,
                    created_at,
                    updated_at,
                }
            },
        )
}

/// Generate a Vec of AccountRecords (0..max_len).
fn arb_account_vec(max_len: usize) -> impl Strategy<Value = Vec<AccountRecord>> {
    prop::collection::vec(arb_account_record(), 0..max_len)
}

/// Generate AccountSelectionConfig with valid threshold.
fn arb_selection_config() -> impl Strategy<Value = AccountSelectionConfig> {
    arb_percent().prop_map(|threshold_percent| AccountSelectionConfig { threshold_percent })
}

/// Generate a FilteredAccount.
fn arb_filtered_account() -> impl Strategy<Value = FilteredAccount> {
    (arb_safe_string(), arb_opt_string(), arb_percent(), arb_safe_string()).prop_map(
        |(account_id, name, percent_remaining, reason)| FilteredAccount {
            account_id,
            name,
            percent_remaining,
            reason,
        },
    )
}

/// Generate a CandidateAccount.
fn arb_candidate_account() -> impl Strategy<Value = CandidateAccount> {
    (arb_safe_string(), arb_opt_string(), arb_percent(), arb_opt_timestamp()).prop_map(
        |(account_id, name, percent_remaining, last_used_at)| CandidateAccount {
            account_id,
            name,
            percent_remaining,
            last_used_at,
        },
    )
}

/// Generate a SelectionExplanation.
fn arb_selection_explanation() -> impl Strategy<Value = SelectionExplanation> {
    (
        0usize..20,
        prop::collection::vec(arb_filtered_account(), 0..5),
        prop::collection::vec(arb_candidate_account(), 0..5),
        arb_safe_string(),
    )
        .prop_map(
            |(total_considered, filtered_out, candidates, selection_reason)| {
                SelectionExplanation {
                    total_considered,
                    filtered_out,
                    candidates,
                    selection_reason,
                }
            },
        )
}

/// Generate a CautAccountUsage with controlled fields.
fn arb_caut_account_usage() -> impl Strategy<Value = CautAccountUsage> {
    (
        arb_opt_string(),                                           // id
        arb_opt_string(),                                           // name
        prop_oneof![3 => Just(None), 7 => arb_percent().prop_map(Some)], // percent_remaining
        prop_oneof![3 => Just(None), 7 => (1u64..1000u64).prop_map(Some)], // limit_hours
        arb_opt_string(),                                           // reset_at
        prop_oneof![3 => Just(None), 7 => (0u64..1_000_000u64).prop_map(Some)], // tokens_used
        prop_oneof![3 => Just(None), 7 => (0u64..1_000_000u64).prop_map(Some)], // tokens_remaining
        prop_oneof![3 => Just(None), 7 => (0u64..1_000_000u64).prop_map(Some)], // tokens_limit
    )
        .prop_map(
            |(id, name, percent_remaining, limit_hours, reset_at, tokens_used, tokens_remaining, tokens_limit)| {
                CautAccountUsage {
                    id,
                    name,
                    percent_remaining,
                    limit_hours,
                    reset_at,
                    tokens_used,
                    tokens_remaining,
                    tokens_limit,
                    extra: HashMap::new(),
                }
            },
        )
}

/// Generate an epoch ms string (all digits).
fn arb_epoch_ms_string() -> impl Strategy<Value = String> {
    arb_timestamp().prop_map(|ts| ts.to_string())
}

/// Generate an ISO 8601 UTC datetime string.
fn arb_iso8601_string() -> impl Strategy<Value = String> {
    (2000i32..2050, 1u32..=12, 1u32..=28, 0u32..24, 0u32..60, 0u32..60).prop_map(
        |(y, m, d, h, min, s)| format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, h, min, s),
    )
}

/// Generate an ISO 8601 date-only string.
fn arb_iso8601_date_string() -> impl Strategy<Value = String> {
    (2000i32..2050, 1u32..=12, 1u32..=28)
        .prop_map(|(y, m, d)| format!("{:04}-{:02}-{:02}", y, m, d))
}

/// Generate an AccountRecord with a parseable epoch-ms reset_at.
fn arb_account_with_epoch_reset() -> impl Strategy<Value = AccountRecord> {
    (arb_safe_string(), arb_epoch_ms_string()).prop_map(|(account_id, reset_str)| AccountRecord {
        id: 0,
        account_id,
        service: "openai".to_string(),
        name: None,
        percent_remaining: 0.0,
        reset_at: Some(reset_str),
        tokens_used: None,
        tokens_remaining: None,
        tokens_limit: None,
        last_refreshed_at: 1000,
        last_used_at: None,
        created_at: 1000,
        updated_at: 1000,
    })
}

/// Helper: build a minimal AccountRecord with given id, pct, and last_used.
fn make_record(id: &str, pct: f64, last_used: Option<i64>) -> AccountRecord {
    AccountRecord {
        id: 0,
        account_id: id.to_string(),
        service: "openai".to_string(),
        name: None,
        percent_remaining: pct,
        reset_at: None,
        tokens_used: None,
        tokens_remaining: None,
        tokens_limit: None,
        last_refreshed_at: 1000,
        last_used_at: last_used,
        created_at: 1000,
        updated_at: 1000,
    }
}

// =============================================================================
// 1. AccountRecord serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn account_record_serde_roundtrip(record in arb_account_record()) {
        let json = serde_json::to_string(&record).expect("serialize");
        let parsed: AccountRecord = serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(&parsed.account_id, &record.account_id, "account_id mismatch");
        prop_assert_eq!(&parsed.service, &record.service, "service mismatch");
        prop_assert_eq!(&parsed.name, &record.name, "name mismatch");
        prop_assert!(
            (parsed.percent_remaining - record.percent_remaining).abs() < 0.01,
            "percent_remaining mismatch: {} vs {}", parsed.percent_remaining, record.percent_remaining
        );
        prop_assert_eq!(&parsed.reset_at, &record.reset_at, "reset_at mismatch");
        prop_assert_eq!(parsed.tokens_used, record.tokens_used, "tokens_used mismatch");
        prop_assert_eq!(parsed.tokens_remaining, record.tokens_remaining, "tokens_remaining mismatch");
        prop_assert_eq!(parsed.tokens_limit, record.tokens_limit, "tokens_limit mismatch");
        prop_assert_eq!(parsed.last_refreshed_at, record.last_refreshed_at, "last_refreshed_at mismatch");
        prop_assert_eq!(parsed.last_used_at, record.last_used_at, "last_used_at mismatch");
        prop_assert_eq!(parsed.created_at, record.created_at, "created_at mismatch");
        prop_assert_eq!(parsed.updated_at, record.updated_at, "updated_at mismatch");
    }

    /// AccountRecord JSON contains expected keys.
    #[test]
    fn account_record_json_has_expected_keys(record in arb_account_record()) {
        let json = serde_json::to_string(&record).expect("serialize");
        prop_assert!(json.contains("\"account_id\""), "JSON missing account_id key");
        prop_assert!(json.contains("\"service\""), "JSON missing service key");
        prop_assert!(json.contains("\"percent_remaining\""), "JSON missing percent_remaining key");
        prop_assert!(json.contains("\"last_refreshed_at\""), "JSON missing last_refreshed_at key");
        prop_assert!(json.contains("\"created_at\""), "JSON missing created_at key");
        prop_assert!(json.contains("\"updated_at\""), "JSON missing updated_at key");
    }
}

// =============================================================================
// 2. AccountSelectionConfig serde roundtrip + default
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn selection_config_serde_roundtrip(threshold in arb_percent()) {
        let config = AccountSelectionConfig { threshold_percent: threshold };
        let json = serde_json::to_string(&config).expect("serialize");
        let parsed: AccountSelectionConfig = serde_json::from_str(&json).expect("deserialize");

        prop_assert!(
            (parsed.threshold_percent - config.threshold_percent).abs() < 0.01,
            "threshold mismatch: {} vs {}", parsed.threshold_percent, config.threshold_percent
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    #[test]
    fn selection_config_default_is_five(_dummy in 0..1i32) {
        let config = AccountSelectionConfig::default();
        prop_assert!(
            (config.threshold_percent - 5.0).abs() < 0.001,
            "default threshold should be 5.0, got {}", config.threshold_percent
        );
    }
}

// =============================================================================
// 3. AccountSelectionResult serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn selection_result_serde_roundtrip(
        accounts in arb_account_vec(8),
        config in arb_selection_config(),
    ) {
        let result = select_account(&accounts, &config);
        let json = serde_json::to_string(&result).expect("serialize");
        let parsed: AccountSelectionResult = serde_json::from_str(&json).expect("deserialize");

        // Check selected account_id matches
        let orig_id = result.selected.as_ref().map(|a| a.account_id.clone());
        let parsed_id = parsed.selected.as_ref().map(|a| a.account_id.clone());
        prop_assert_eq!(&orig_id, &parsed_id, "selected account_id mismatch after roundtrip");

        // Check explanation fields
        prop_assert_eq!(
            parsed.explanation.total_considered,
            result.explanation.total_considered,
            "total_considered mismatch"
        );
        prop_assert_eq!(
            parsed.explanation.filtered_out.len(),
            result.explanation.filtered_out.len(),
            "filtered_out len mismatch"
        );
        prop_assert_eq!(
            parsed.explanation.candidates.len(),
            result.explanation.candidates.len(),
            "candidates len mismatch"
        );
    }
}

// =============================================================================
// 4. AccountExhaustionInfo serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn exhaustion_info_serde_roundtrip(
        explanation in arb_selection_explanation(),
        accounts in arb_account_vec(5),
    ) {
        let info = build_exhaustion_info(&accounts, explanation);
        let json = serde_json::to_string(&info).expect("serialize");
        let parsed: AccountExhaustionInfo = serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(parsed.accounts_checked, info.accounts_checked, "accounts_checked mismatch");
        prop_assert_eq!(parsed.earliest_reset_ms, info.earliest_reset_ms, "earliest_reset_ms mismatch");
        prop_assert_eq!(
            &parsed.earliest_reset_account,
            &info.earliest_reset_account,
            "earliest_reset_account mismatch"
        );
        prop_assert_eq!(
            parsed.explanation.total_considered,
            info.explanation.total_considered,
            "explanation.total_considered mismatch"
        );
    }
}

// =============================================================================
// 5. is_above_threshold — biconditional property
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn is_above_threshold_iff_pct_gte(pct in arb_percent(), threshold in arb_percent()) {
        let record = make_record("test", pct, None);

        let result = record.is_above_threshold(threshold);
        let expected = pct >= threshold;
        prop_assert_eq!(result, expected,
            "is_above_threshold({}, {}) = {} but pct >= threshold = {}",
            pct, threshold, result, expected
        );
    }

    /// is_above_threshold with 0.0 threshold always true for non-negative pct.
    #[test]
    fn is_above_zero_threshold_always_true(pct in arb_percent()) {
        let record = make_record("test", pct, None);
        prop_assert!(record.is_above_threshold(0.0),
            "pct {} should be >= 0.0", pct);
    }
}

// =============================================================================
// 6. select_account invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// total_considered = len(accounts)
    #[test]
    fn select_total_considered_equals_len(
        accounts in arb_account_vec(15),
        config in arb_selection_config(),
    ) {
        let result = select_account(&accounts, &config);
        prop_assert_eq!(
            result.explanation.total_considered, accounts.len(),
            "total_considered should equal input length"
        );
    }

    /// filtered_out + candidates = total_considered
    #[test]
    fn select_partition_is_complete(
        accounts in arb_account_vec(15),
        config in arb_selection_config(),
    ) {
        let result = select_account(&accounts, &config);
        let sum = result.explanation.filtered_out.len() + result.explanation.candidates.len();
        prop_assert_eq!(
            sum, result.explanation.total_considered,
            "filtered_out({}) + candidates({}) != total_considered({})",
            result.explanation.filtered_out.len(),
            result.explanation.candidates.len(),
            result.explanation.total_considered
        );
    }

    /// If selected.is_some(), the selected account must be above threshold.
    #[test]
    fn select_winner_above_threshold(
        accounts in arb_account_vec(15),
        config in arb_selection_config(),
    ) {
        let result = select_account(&accounts, &config);
        if let Some(ref selected) = result.selected {
            prop_assert!(
                selected.percent_remaining >= config.threshold_percent,
                "selected account pct {} below threshold {}",
                selected.percent_remaining,
                config.threshold_percent
            );
        }
    }

    /// Selected account has the highest percent_remaining among candidates.
    #[test]
    fn select_winner_has_highest_pct(
        accounts in arb_account_vec(15),
        config in arb_selection_config(),
    ) {
        let result = select_account(&accounts, &config);
        if let Some(ref selected) = result.selected {
            for candidate in &result.explanation.candidates {
                prop_assert!(
                    selected.percent_remaining >= candidate.percent_remaining - 0.001,
                    "selected pct {} less than candidate pct {}",
                    selected.percent_remaining,
                    candidate.percent_remaining
                );
            }
        }
    }

    /// Deterministic: same input produces same output.
    #[test]
    fn select_deterministic(
        accounts in arb_account_vec(10),
        config in arb_selection_config(),
    ) {
        let r1 = select_account(&accounts, &config);
        let r2 = select_account(&accounts, &config);

        let id1 = r1.selected.as_ref().map(|a| a.account_id.clone());
        let id2 = r2.selected.as_ref().map(|a| a.account_id.clone());
        prop_assert_eq!(&id1, &id2, "selection should be deterministic");

        prop_assert_eq!(
            r1.explanation.total_considered,
            r2.explanation.total_considered,
            "total_considered should be deterministic"
        );
        prop_assert_eq!(
            r1.explanation.filtered_out.len(),
            r2.explanation.filtered_out.len(),
            "filtered_out count should be deterministic"
        );
    }

    /// Empty accounts always yields None selected.
    #[test]
    fn select_empty_yields_none(config in arb_selection_config()) {
        let result = select_account(&[], &config);
        prop_assert!(result.selected.is_none(), "empty input should yield None");
        prop_assert_eq!(result.explanation.total_considered, 0usize, "total_considered should be 0");
    }

    /// If all accounts are below threshold, selected is None.
    #[test]
    fn select_all_below_threshold_yields_none(
        pcts in prop::collection::vec(0.0f64..=4.99f64, 1..10),
        threshold in 5.0f64..=100.0f64,
    ) {
        let accounts: Vec<AccountRecord> = pcts.iter().enumerate().map(|(i, &pct)| {
            make_record(&format!("acct-{}", i), pct, None)
        }).collect();

        let config = AccountSelectionConfig { threshold_percent: threshold };
        let result = select_account(&accounts, &config);
        prop_assert!(result.selected.is_none(),
            "all accounts below threshold {} should yield None, pcts: {:?}",
            threshold, pcts
        );
    }

    /// Tie-break: same pct, different last_used_at — older wins.
    #[test]
    fn select_tiebreak_older_wins(
        pct in 10.0f64..=100.0f64,
        ts_a in 100i64..500_000,
        ts_b in 500_001i64..1_000_000,
    ) {
        // ts_a < ts_b, so account A is older and should win
        let accounts = vec![
            make_record("older", pct, Some(ts_a)),
            make_record("newer", pct, Some(ts_b)),
        ];

        let config = AccountSelectionConfig { threshold_percent: 0.0 };
        let result = select_account(&accounts, &config);
        let selected_id = result.selected.as_ref().map(|a| a.account_id.as_str());
        prop_assert_eq!(selected_id, Some("older"),
            "with same pct, older last_used_at ({}) should beat newer ({})",
            ts_a, ts_b
        );
    }

    /// Tie-break: None last_used_at beats any Some(ts) (None treated as 0).
    #[test]
    fn select_tiebreak_none_beats_some(
        pct in 10.0f64..=100.0f64,
        ts in 1i64..1_000_000_000,
    ) {
        let accounts = vec![
            make_record("never_used", pct, None),
            make_record("used", pct, Some(ts)),
        ];

        let config = AccountSelectionConfig { threshold_percent: 0.0 };
        let result = select_account(&accounts, &config);
        let selected_id = result.selected.as_ref().map(|a| a.account_id.as_str());
        prop_assert_eq!(selected_id, Some("never_used"),
            "None last_used_at should beat Some({})", ts
        );
    }

    /// Higher pct always wins regardless of last_used_at.
    #[test]
    fn select_higher_pct_wins_over_lru(
        pct_high in 51.0f64..=100.0f64,
        pct_low in 10.0f64..=50.0f64,
        ts_old in 1i64..1000,
        ts_new in 1001i64..2000,
    ) {
        // Higher pct account has newer last_used — should still win
        let accounts = vec![
            make_record("high_new", pct_high, Some(ts_new)),
            make_record("low_old", pct_low, Some(ts_old)),
        ];

        let config = AccountSelectionConfig { threshold_percent: 0.0 };
        let result = select_account(&accounts, &config);
        let selected_id = result.selected.as_ref().map(|a| a.account_id.as_str());
        prop_assert_eq!(selected_id, Some("high_new"),
            "higher pct {} should beat lower pct {} regardless of LRU",
            pct_high, pct_low
        );
    }

    /// All filtered accounts have percent_remaining below threshold.
    #[test]
    fn select_filtered_all_below_threshold(
        accounts in arb_account_vec(10),
        config in arb_selection_config(),
    ) {
        let result = select_account(&accounts, &config);
        for filtered in &result.explanation.filtered_out {
            prop_assert!(
                filtered.percent_remaining < config.threshold_percent,
                "filtered account pct {} should be < threshold {}",
                filtered.percent_remaining, config.threshold_percent
            );
        }
    }

    /// All candidate accounts have percent_remaining >= threshold.
    #[test]
    fn select_candidates_all_above_threshold(
        accounts in arb_account_vec(10),
        config in arb_selection_config(),
    ) {
        let result = select_account(&accounts, &config);
        for candidate in &result.explanation.candidates {
            prop_assert!(
                candidate.percent_remaining >= config.threshold_percent,
                "candidate pct {} should be >= threshold {}",
                candidate.percent_remaining, config.threshold_percent
            );
        }
    }

    /// With threshold=0.0, nothing gets filtered (all pcts are >= 0).
    #[test]
    fn select_zero_threshold_no_filtering(
        accounts in arb_account_vec(10),
    ) {
        let config = AccountSelectionConfig { threshold_percent: 0.0 };
        let result = select_account(&accounts, &config);
        prop_assert_eq!(
            result.explanation.filtered_out.len(), 0usize,
            "zero threshold should filter nothing, but got {} filtered",
            result.explanation.filtered_out.len()
        );
        prop_assert_eq!(
            result.explanation.candidates.len(), accounts.len(),
            "zero threshold should keep all candidates"
        );
    }
}

// =============================================================================
// 7. parse_reset_at_ms
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// All-digit strings parse as epoch ms.
    #[test]
    fn parse_epoch_digits_round_trip(ts in arb_timestamp()) {
        let s = ts.to_string();
        let parsed = parse_reset_at_ms(&s);
        prop_assert_eq!(parsed, Some(ts), "epoch string '{}' should parse to {}", s, ts);
    }

    /// ISO 8601 "YYYY-MM-DDThh:mm:ssZ" parses to Some.
    #[test]
    fn parse_iso8601_utc_yields_some(s in arb_iso8601_string()) {
        let result = parse_reset_at_ms(&s);
        prop_assert!(result.is_some(), "ISO 8601 string '{}' should parse to Some", s);
    }

    /// ISO 8601 date-only "YYYY-MM-DD" parses to Some and is midnight (divisible by 86400000).
    #[test]
    fn parse_iso8601_date_only_midnight(s in arb_iso8601_date_string()) {
        let result = parse_reset_at_ms(&s);
        prop_assert!(result.is_some(), "date-only '{}' should parse to Some", s);
        if let Some(ms) = result {
            prop_assert_eq!(ms % 86_400_000, 0,
                "date-only '{}' should be midnight, got remainder {}",
                s, ms % 86_400_000
            );
        }
    }

    /// Empty and whitespace-only strings parse to None.
    #[test]
    fn parse_empty_whitespace_yields_none(ws in "[ \t]{0,5}") {
        let result = parse_reset_at_ms(&ws);
        prop_assert!(result.is_none(),
            "whitespace-only string '{}' should parse to None", ws
        );
    }

    /// Garbage strings (no leading digit pattern and no YYYY- format) yield None.
    #[test]
    fn parse_garbage_yields_none(s in "[qrstuvwxyz!@#]{1,20}") {
        let result = parse_reset_at_ms(&s);
        prop_assert!(result.is_none(),
            "garbage string '{}' should parse to None", s
        );
    }

    /// ISO 8601 with +00:00 offset also parses.
    #[test]
    fn parse_iso8601_plus_offset(
        y in 2000i32..2050,
        m in 1u32..=12,
        d in 1u32..=28,
        h in 0u32..24,
        min in 0u32..60,
        sec in 0u32..60,
    ) {
        let s = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00", y, m, d, h, min, sec);
        let result = parse_reset_at_ms(&s);
        prop_assert!(result.is_some(), "ISO 8601 +00:00 '{}' should parse", s);
    }

    /// ISO 8601 Z and +00:00 parse to the same millisecond value.
    #[test]
    fn parse_iso8601_z_and_offset_equal(
        y in 2000i32..2050,
        m in 1u32..=12,
        d in 1u32..=28,
        h in 0u32..24,
        min in 0u32..60,
        sec in 0u32..60,
    ) {
        let z_str = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, h, min, sec);
        let offset_str = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00", y, m, d, h, min, sec);

        let z_ms = parse_reset_at_ms(&z_str);
        let offset_ms = parse_reset_at_ms(&offset_str);
        prop_assert_eq!(z_ms, offset_ms,
            "'{}' and '{}' should parse to same ms", z_str, offset_str);
    }
}

// =============================================================================
// 8. find_earliest_reset — picks the smallest parsed reset_at ms
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// The returned earliest ms is indeed the minimum among parseable reset_at values.
    #[test]
    fn find_earliest_is_minimum(
        accounts in prop::collection::vec(arb_account_with_epoch_reset(), 1..8),
    ) {
        let (earliest_ms, earliest_acct) = find_earliest_reset(&accounts);

        // Compute expected minimum manually
        let mut expected_min: Option<(i64, String)> = None;
        for acct in &accounts {
            if let Some(ref reset_str) = acct.reset_at {
                if let Some(ms) = parse_reset_at_ms(reset_str) {
                    match &expected_min {
                        None => expected_min = Some((ms, acct.account_id.clone())),
                        Some((cur, _)) if ms < *cur => {
                            expected_min = Some((ms, acct.account_id.clone()));
                        }
                        _ => {}
                    }
                }
            }
        }

        let expected_ms = expected_min.as_ref().map(|(ms, _)| *ms);
        let expected_acct = expected_min.as_ref().map(|(_, id)| id.clone());
        prop_assert_eq!(earliest_ms, expected_ms, "earliest_ms mismatch");
        prop_assert_eq!(earliest_acct, expected_acct, "earliest_acct mismatch");
    }

    /// Empty accounts returns (None, None).
    #[test]
    fn find_earliest_empty_none(_dummy in 0..1i32) {
        let (ms, acct) = find_earliest_reset(&[]);
        prop_assert!(ms.is_none(), "empty accounts should return None ms");
        prop_assert!(acct.is_none(), "empty accounts should return None acct");
    }

    /// Accounts with no reset_at fields return (None, None).
    #[test]
    fn find_earliest_no_reset_at(accounts in arb_account_vec(5)) {
        // Strip all reset_at fields
        let stripped: Vec<AccountRecord> = accounts.into_iter().map(|mut a| {
            a.reset_at = None;
            a
        }).collect();

        let (ms, acct) = find_earliest_reset(&stripped);
        prop_assert!(ms.is_none(), "no reset_at should return None ms");
        prop_assert!(acct.is_none(), "no reset_at should return None acct");
    }
}

// =============================================================================
// 9. build_exhaustion_info — accounts_checked + earliest_reset coherence
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// accounts_checked = len(accounts).
    #[test]
    fn exhaustion_accounts_checked_eq_len(
        accounts in arb_account_vec(10),
        explanation in arb_selection_explanation(),
    ) {
        let info = build_exhaustion_info(&accounts, explanation);
        prop_assert_eq!(
            info.accounts_checked as usize, accounts.len(),
            "accounts_checked should equal input length"
        );
    }

    /// earliest_reset matches find_earliest_reset output.
    #[test]
    fn exhaustion_earliest_matches_find(
        accounts in arb_account_vec(8),
        explanation in arb_selection_explanation(),
    ) {
        let info = build_exhaustion_info(&accounts, explanation);
        let (expected_ms, expected_acct) = find_earliest_reset(&accounts);

        prop_assert_eq!(info.earliest_reset_ms, expected_ms,
            "earliest_reset_ms should match find_earliest_reset");
        prop_assert_eq!(&info.earliest_reset_account, &expected_acct,
            "earliest_reset_account should match find_earliest_reset");
    }

    /// The explanation is preserved exactly.
    #[test]
    fn exhaustion_preserves_explanation(
        accounts in arb_account_vec(3),
        explanation in arb_selection_explanation(),
    ) {
        let expected_total = explanation.total_considered;
        let expected_reason = explanation.selection_reason.clone();
        let expected_filtered = explanation.filtered_out.len();
        let expected_candidates = explanation.candidates.len();

        let info = build_exhaustion_info(&accounts, explanation);

        prop_assert_eq!(info.explanation.total_considered, expected_total,
            "explanation.total_considered should be preserved");
        prop_assert_eq!(&info.explanation.selection_reason, &expected_reason,
            "explanation.selection_reason should be preserved");
        prop_assert_eq!(info.explanation.filtered_out.len(), expected_filtered,
            "explanation.filtered_out.len should be preserved");
        prop_assert_eq!(info.explanation.candidates.len(), expected_candidates,
            "explanation.candidates.len should be preserved");
    }
}

// =============================================================================
// 10. AccountRecord::from_caut — various CautAccountUsage inputs
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// from_caut sets id=0, service="openai", timestamps to now_ms.
    #[test]
    fn from_caut_basic_fields(
        usage in arb_caut_account_usage(),
        now_ms in arb_timestamp(),
    ) {
        let record = AccountRecord::from_caut(&usage, CautService::OpenAI, now_ms);

        prop_assert_eq!(record.id, 0i64, "id should always be 0");
        prop_assert_eq!(&record.service, "openai", "service should be 'openai'");
        prop_assert_eq!(record.last_refreshed_at, now_ms, "last_refreshed_at should be now_ms");
        prop_assert_eq!(record.created_at, now_ms, "created_at should be now_ms");
        prop_assert_eq!(record.updated_at, now_ms, "updated_at should be now_ms");
        prop_assert!(record.last_used_at.is_none(), "last_used_at should be None for new records");
    }

    /// from_caut account_id fallback chain: id -> name -> "unknown-{now_ms}".
    #[test]
    fn from_caut_account_id_fallback(
        id_opt in arb_opt_string(),
        name_opt in arb_opt_string(),
        now_ms in arb_timestamp(),
    ) {
        let usage = CautAccountUsage {
            id: id_opt.clone(),
            name: name_opt.clone(),
            percent_remaining: Some(50.0),
            limit_hours: None,
            reset_at: None,
            tokens_used: None,
            tokens_remaining: None,
            tokens_limit: None,
            extra: HashMap::new(),
        };

        let record = AccountRecord::from_caut(&usage, CautService::OpenAI, now_ms);

        if let Some(ref id_val) = id_opt {
            prop_assert_eq!(&record.account_id, id_val,
                "when id is Some, account_id should equal id");
        } else if let Some(ref name_val) = name_opt {
            prop_assert_eq!(&record.account_id, name_val,
                "when id is None but name is Some, account_id should equal name");
        } else {
            let expected = format!("unknown-{}", now_ms);
            prop_assert_eq!(&record.account_id, &expected,
                "when both None, account_id should be 'unknown-{}'", now_ms);
        }
    }

    /// from_caut percent_remaining defaults to 0.0 when None.
    #[test]
    fn from_caut_pct_default(now_ms in arb_timestamp()) {
        let usage = CautAccountUsage {
            id: Some("test".to_string()),
            percent_remaining: None,
            ..Default::default()
        };

        let record = AccountRecord::from_caut(&usage, CautService::OpenAI, now_ms);
        prop_assert!(
            record.percent_remaining.abs() < 0.001,
            "None percent should default to 0.0, got {}",
            record.percent_remaining
        );
    }

    /// from_caut preserves percent_remaining when present.
    #[test]
    fn from_caut_pct_preserved(pct in arb_percent(), now_ms in arb_timestamp()) {
        let usage = CautAccountUsage {
            id: Some("test".to_string()),
            percent_remaining: Some(pct),
            ..Default::default()
        };

        let record = AccountRecord::from_caut(&usage, CautService::OpenAI, now_ms);
        prop_assert!(
            (record.percent_remaining - pct).abs() < 0.001,
            "percent should be preserved: expected {}, got {}",
            pct, record.percent_remaining
        );
    }

    /// from_caut maps name, reset_at, tokens correctly.
    #[test]
    fn from_caut_optional_fields(usage in arb_caut_account_usage(), now_ms in arb_timestamp()) {
        let record = AccountRecord::from_caut(&usage, CautService::OpenAI, now_ms);

        prop_assert_eq!(&record.name, &usage.name, "name mismatch");
        prop_assert_eq!(&record.reset_at, &usage.reset_at, "reset_at mismatch");

        // tokens_used: u64 -> i64 conversion (should succeed for reasonable values)
        match usage.tokens_used {
            Some(v) if i64::try_from(v).is_ok() => {
                prop_assert_eq!(record.tokens_used, Some(v as i64), "tokens_used mismatch");
            }
            Some(_) => {
                prop_assert!(record.tokens_used.is_none(),
                    "overflow u64 should map to None");
            }
            None => {
                prop_assert!(record.tokens_used.is_none(), "None tokens_used should stay None");
            }
        }
    }

    /// from_caut with fully-default CautAccountUsage produces valid record.
    #[test]
    fn from_caut_default_usage(now_ms in arb_timestamp()) {
        let usage = CautAccountUsage::default();
        let record = AccountRecord::from_caut(&usage, CautService::OpenAI, now_ms);

        let expected_id = format!("unknown-{}", now_ms);
        prop_assert_eq!(&record.account_id, &expected_id,
            "default usage should produce 'unknown-{}' id", now_ms);
        prop_assert!(record.name.is_none(), "default usage should have no name");
        prop_assert!(record.percent_remaining.abs() < 0.001,
            "default usage should have 0.0 pct");
        prop_assert!(record.tokens_used.is_none(), "default usage should have no tokens_used");
        prop_assert!(record.tokens_remaining.is_none(), "default usage should have no tokens_remaining");
        prop_assert!(record.tokens_limit.is_none(), "default usage should have no tokens_limit");
    }
}

// =============================================================================
// 11. FilteredAccount / CandidateAccount serde roundtrips
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn filtered_account_serde_roundtrip(fa in arb_filtered_account()) {
        let json = serde_json::to_string(&fa).expect("serialize");
        let parsed: FilteredAccount = serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(&parsed.account_id, &fa.account_id, "account_id mismatch");
        prop_assert_eq!(&parsed.name, &fa.name, "name mismatch");
        prop_assert!(
            (parsed.percent_remaining - fa.percent_remaining).abs() < 0.01,
            "percent_remaining mismatch: {} vs {}",
            parsed.percent_remaining, fa.percent_remaining
        );
        prop_assert_eq!(&parsed.reason, &fa.reason, "reason mismatch");
    }

    #[test]
    fn candidate_account_serde_roundtrip(ca in arb_candidate_account()) {
        let json = serde_json::to_string(&ca).expect("serialize");
        let parsed: CandidateAccount = serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(&parsed.account_id, &ca.account_id, "account_id mismatch");
        prop_assert_eq!(&parsed.name, &ca.name, "name mismatch");
        prop_assert!(
            (parsed.percent_remaining - ca.percent_remaining).abs() < 0.01,
            "percent_remaining mismatch: {} vs {}",
            parsed.percent_remaining, ca.percent_remaining
        );
        prop_assert_eq!(parsed.last_used_at, ca.last_used_at, "last_used_at mismatch");
    }
}

// =============================================================================
// 12. SelectionExplanation serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(25))]

    #[test]
    fn selection_explanation_serde_roundtrip(expl in arb_selection_explanation()) {
        let json = serde_json::to_string(&expl).expect("serialize");
        let parsed: SelectionExplanation = serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(parsed.total_considered, expl.total_considered,
            "total_considered mismatch");
        prop_assert_eq!(parsed.filtered_out.len(), expl.filtered_out.len(),
            "filtered_out len mismatch");
        prop_assert_eq!(parsed.candidates.len(), expl.candidates.len(),
            "candidates len mismatch");
        prop_assert_eq!(&parsed.selection_reason, &expl.selection_reason,
            "selection_reason mismatch");
    }
}

// =============================================================================
// 13. Cross-cutting: select_account + build_exhaustion_info integration
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// When selection fails, exhaustion info is coherent.
    #[test]
    fn exhaustion_from_failed_selection(
        accounts in prop::collection::vec(arb_account_record(), 1..8),
    ) {
        // Use a very high threshold to force failure
        let config = AccountSelectionConfig { threshold_percent: 101.0 };
        let result = select_account(&accounts, &config);

        // All should be filtered
        prop_assert!(result.selected.is_none(),
            "threshold 101 should filter all accounts");

        let info = build_exhaustion_info(&accounts, result.explanation);
        prop_assert_eq!(info.accounts_checked as usize, accounts.len(),
            "accounts_checked should match input length");
    }

    /// parse_reset_at_ms is consistent: parsing twice yields same result.
    #[test]
    fn parse_reset_at_idempotent(s in arb_epoch_ms_string()) {
        let r1 = parse_reset_at_ms(&s);
        let r2 = parse_reset_at_ms(&s);
        prop_assert_eq!(r1, r2, "parse_reset_at_ms should be idempotent for '{}'", s);
    }

    /// select_account: selected account_id appears in the candidates list.
    #[test]
    fn select_winner_in_candidates(
        accounts in arb_account_vec(10),
        config in arb_selection_config(),
    ) {
        let result = select_account(&accounts, &config);
        if let Some(ref selected) = result.selected {
            let found = result.explanation.candidates.iter()
                .any(|c| c.account_id == selected.account_id);
            prop_assert!(found,
                "selected account '{}' should appear in candidates list",
                selected.account_id
            );
        }
    }

    /// select_account: selected account_id never appears in filtered_out.
    #[test]
    fn select_winner_not_in_filtered(
        accounts in arb_account_vec(10),
        config in arb_selection_config(),
    ) {
        let result = select_account(&accounts, &config);
        if let Some(ref selected) = result.selected {
            let in_filtered = result.explanation.filtered_out.iter()
                .any(|f| f.account_id == selected.account_id);
            prop_assert!(!in_filtered,
                "selected account '{}' should not appear in filtered_out",
                selected.account_id
            );
        }
    }
}
