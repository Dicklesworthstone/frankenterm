//! Property-based tests for robot_types module.
//!
//! Verifies invariants for:
//! - ErrorCode: parse/as_str roundtrip, from_number/number roundtrip,
//!   category consistency, is_retryable classification, Unknown variant
//! - RobotResponse: serde roundtrip, into_result semantics
//! - Data types: serde roundtrips for GetTextData, TruncationInfo,
//!   WaitForData, SearchHit, WorkflowInfo, ReservationInfo, etc.

use std::collections::BTreeMap;

use frankenterm_core::error_codes::ErrorCategory;
use frankenterm_core::robot_types::*;
use proptest::prelude::*;

// ============================================================================
// Strategies
// ============================================================================

/// All 27 known ErrorCode variants.
fn arb_known_error_code() -> impl Strategy<Value = ErrorCode> {
    prop_oneof![
        Just(ErrorCode::WeztermNotFound),
        Just(ErrorCode::WeztermExecFailed),
        Just(ErrorCode::PaneNotFound),
        Just(ErrorCode::WeztermParseFailed),
        Just(ErrorCode::WeztermConnectionRefused),
        Just(ErrorCode::DatabaseLocked),
        Just(ErrorCode::StorageCorruption),
        Just(ErrorCode::FtsIndexError),
        Just(ErrorCode::MigrationFailed),
        Just(ErrorCode::DiskFull),
        Just(ErrorCode::InvalidRegex),
        Just(ErrorCode::RulePackNotFound),
        Just(ErrorCode::PatternTimeout),
        Just(ErrorCode::ActionDenied),
        Just(ErrorCode::RateLimitExceeded),
        Just(ErrorCode::ApprovalRequired),
        Just(ErrorCode::ApprovalExpired),
        Just(ErrorCode::WorkflowNotFound),
        Just(ErrorCode::WorkflowStepFailed),
        Just(ErrorCode::WorkflowTimeout),
        Just(ErrorCode::WorkflowAlreadyRunning),
        Just(ErrorCode::NetworkTimeout),
        Just(ErrorCode::ConnectionRefused),
        Just(ErrorCode::ConfigInvalid),
        Just(ErrorCode::ConfigNotFound),
        Just(ErrorCode::InternalError),
        Just(ErrorCode::FeatureNotAvailable),
        Just(ErrorCode::VersionMismatch),
    ]
}

/// Any ErrorCode including Unknown variants.
fn arb_error_code() -> impl Strategy<Value = ErrorCode> {
    prop_oneof![
        arb_known_error_code(),
        (0u16..=65535u16).prop_map(ErrorCode::Unknown),
    ]
}

fn arb_get_text_data() -> impl Strategy<Value = GetTextData> {
    (
        0u64..1000,
        "[a-zA-Z0-9 \n]{0,100}",
        0usize..500,
        proptest::bool::ANY,
        proptest::bool::ANY,
    )
        .prop_map(
            |(pane_id, text, tail_lines, escapes_included, truncated)| GetTextData {
                pane_id,
                text,
                tail_lines,
                escapes_included,
                truncated,
                truncation_info: None,
            },
        )
}

fn arb_truncation_info() -> impl Strategy<Value = TruncationInfo> {
    (
        0usize..100_000,
        0usize..100_000,
        0usize..10_000,
        0usize..10_000,
    )
        .prop_map(
            |(original_bytes, returned_bytes, original_lines, returned_lines)| TruncationInfo {
                original_bytes,
                returned_bytes,
                original_lines,
                returned_lines,
            },
        )
}

fn arb_wait_for_data() -> impl Strategy<Value = WaitForData> {
    (
        0u64..1000,
        "[a-z.\\$]{1,20}",
        proptest::bool::ANY,
        0u64..60_000,
        0usize..100,
        proptest::bool::ANY,
    )
        .prop_map(
            |(pane_id, pattern, matched, elapsed_ms, polls, is_regex)| WaitForData {
                pane_id,
                pattern,
                matched,
                elapsed_ms,
                polls,
                is_regex,
            },
        )
}

fn arb_search_hit() -> impl Strategy<Value = SearchHit> {
    (
        0i64..100_000,
        0u64..1000,
        0u64..100_000,
        0i64..2_000_000_000_000,
        0.0f64..100.0,
    )
        .prop_map(|(segment_id, pane_id, seq, captured_at, score)| SearchHit {
            segment_id,
            pane_id,
            seq,
            captured_at,
            score,
            snippet: None,
            content: None,
            semantic_score: None,
            fusion_rank: None,
        })
}

fn arb_workflow_info() -> impl Strategy<Value = WorkflowInfo> {
    ("[a-z_]{3,20}", proptest::bool::ANY).prop_map(|(name, enabled)| WorkflowInfo {
        name,
        enabled,
        trigger_event_types: None,
        requires_pane: None,
    })
}

fn arb_reservation_info() -> impl Strategy<Value = ReservationInfo> {
    (
        0i64..100_000,
        0u64..1000,
        "[a-z]{3,10}",
        "[a-z0-9-]{5,20}",
        0i64..2_000_000_000_000,
        0i64..2_000_000_000_000,
        prop_oneof![
            Just("active".to_string()),
            Just("released".to_string()),
            Just("expired".to_string())
        ],
    )
        .prop_map(
            |(id, pane_id, owner_kind, owner_id, created_at, expires_at, status)| ReservationInfo {
                id,
                pane_id,
                owner_kind,
                owner_id,
                reason: None,
                created_at,
                expires_at,
                released_at: None,
                status,
            },
        )
}

fn arb_lint_issue() -> impl Strategy<Value = LintIssue> {
    ("[a-z.]{3,20}", "[a-z]{3,15}", "[a-z ]{5,50}").prop_map(|(rule_id, category, message)| {
        LintIssue {
            rule_id,
            category,
            message,
            suggestion: None,
        }
    })
}

// ============================================================================
// ErrorCode properties
// ============================================================================

proptest! {
    /// Known ErrorCode: parse(as_str()) roundtrips.
    #[test]
    fn prop_known_code_parse_roundtrip(code in arb_known_error_code()) {
        let s = code.as_str();
        let parsed = ErrorCode::parse(&s).unwrap();
        prop_assert_eq!(parsed.number(), code.number(),
            "parse(as_str()) number mismatch for {}", s);
    }

    /// ErrorCode: from_number(number()) roundtrips.
    #[test]
    fn prop_code_from_number_roundtrip(code in arb_error_code()) {
        let n = code.number();
        let back = ErrorCode::from_number(n);
        prop_assert_eq!(back.number(), n,
            "from_number(number()) mismatch for number {}", n);
    }

    /// ErrorCode: as_str() always starts with "FT-".
    #[test]
    fn prop_code_as_str_prefix(code in arb_error_code()) {
        let s = code.as_str();
        prop_assert!(s.starts_with("FT-"), "as_str() missing FT- prefix: {}", s);
    }

    /// ErrorCode: as_str() contains the numeric code.
    #[test]
    fn prop_code_as_str_contains_number(code in arb_error_code()) {
        let s = code.as_str();
        let num_str = s.strip_prefix("FT-").unwrap();
        let parsed_num: u16 = num_str.parse().unwrap();
        prop_assert_eq!(parsed_num, code.number(),
            "as_str() number mismatch: {} vs {}", parsed_num, code.number());
    }

    /// ErrorCode: parse rejects non-FT- prefix.
    #[test]
    fn prop_code_parse_rejects_bad_prefix(prefix in "[A-Z]{2}", num in 0u16..10000) {
        if prefix != "FT" {
            let s = format!("{}-{}", prefix, num);
            prop_assert!(ErrorCode::parse(&s).is_none(),
                "parse should reject non-FT prefix: {}", s);
        }
    }

    /// ErrorCode: parse rejects non-numeric suffix.
    #[test]
    fn prop_code_parse_rejects_non_numeric(suffix in "[a-z]{1,10}") {
        let s = format!("FT-{}", suffix);
        prop_assert!(ErrorCode::parse(&s).is_none(),
            "parse should reject non-numeric suffix: {}", s);
    }

    /// ErrorCode: category is consistent with number range.
    #[test]
    fn prop_code_category_consistent(code in arb_known_error_code()) {
        let n = code.number();
        let cat = code.category();
        let expected_cat = match n / 1000 {
            1 => ErrorCategory::Wezterm,
            2 => ErrorCategory::Storage,
            3 => ErrorCategory::Pattern,
            4 => ErrorCategory::Policy,
            5 => ErrorCategory::Workflow,
            6 => ErrorCategory::Network,
            7 => ErrorCategory::Config,
            _ => ErrorCategory::Internal,
        };
        prop_assert_eq!(cat, expected_cat,
            "Category mismatch for code {}: {:?} vs {:?}", n, cat, expected_cat);
    }

    /// ErrorCode: Unknown variant preserves its number.
    #[test]
    fn prop_code_unknown_preserves_number(n in 0u16..=65535u16) {
        let code = ErrorCode::Unknown(n);
        prop_assert_eq!(code.number(), n, "Unknown number mismatch");
    }

    /// ErrorCode: is_retryable matches exactly the specified set.
    #[test]
    fn prop_code_is_retryable(code in arb_known_error_code()) {
        let expected = matches!(
            code,
            ErrorCode::DatabaseLocked
                | ErrorCode::RateLimitExceeded
                | ErrorCode::NetworkTimeout
                | ErrorCode::ConnectionRefused
                | ErrorCode::PatternTimeout
                | ErrorCode::WeztermConnectionRefused
        );
        prop_assert_eq!(code.is_retryable(), expected,
            "is_retryable mismatch for {:?}", code);
    }

    /// ErrorCode: Display matches as_str().
    #[test]
    fn prop_code_display_matches_as_str(code in arb_error_code()) {
        let displayed = format!("{}", code);
        prop_assert_eq!(displayed, code.as_str(),
            "Display vs as_str mismatch");
    }
}

// ============================================================================
// RobotResponse envelope properties
// ============================================================================

proptest! {
    /// RobotResponse<GetTextData> serde roundtrip (success case).
    #[test]
    fn prop_response_success_roundtrip(data in arb_get_text_data()) {
        let resp = RobotResponse {
            ok: true,
            data: Some(data),
            error: None,
            error_code: None,
            hint: None,
            elapsed_ms: 42,
            version: "0.1.0".to_string(),
            now: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: RobotResponse<GetTextData> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.ok, true);
        let back_data = back.data.unwrap();
        let orig_data = resp.data.unwrap();
        prop_assert_eq!(back_data.pane_id, orig_data.pane_id);
        prop_assert_eq!(&back_data.text, &orig_data.text);
        prop_assert_eq!(back_data.tail_lines, orig_data.tail_lines);
        prop_assert_eq!(back_data.escapes_included, orig_data.escapes_included);
    }

    /// RobotResponse into_result: ok=true with data returns Ok.
    #[test]
    fn prop_response_into_result_ok(data in arb_get_text_data()) {
        let pane_id = data.pane_id;
        let resp = RobotResponse {
            ok: true,
            data: Some(data),
            error: None,
            error_code: None,
            hint: None,
            elapsed_ms: 1,
            version: "0.1.0".to_string(),
            now: 0,
        };
        let result = resp.into_result();
        prop_assert!(result.is_ok(), "into_result should be Ok for ok=true+data");
        prop_assert_eq!(result.unwrap().pane_id, pane_id);
    }

    /// RobotResponse into_result: ok=false returns Err with message.
    #[test]
    fn prop_response_into_result_err(msg in "[a-z ]{1,50}", code in proptest::option::of("[A-Z]{2}-[0-9]{4}")) {
        let resp: RobotResponse<GetTextData> = RobotResponse {
            ok: false,
            data: None,
            error: Some(msg.clone()),
            error_code: code,
            hint: None,
            elapsed_ms: 1,
            version: "0.1.0".to_string(),
            now: 0,
        };
        let result = resp.into_result();
        prop_assert!(result.is_err(), "into_result should be Err for ok=false");
        prop_assert_eq!(&result.unwrap_err().message, &msg);
    }

    /// RobotResponse into_result: ok=true but null data returns Err.
    #[test]
    fn prop_response_into_result_null_data(_dummy in Just(())) {
        let resp: RobotResponse<GetTextData> = RobotResponse {
            ok: true,
            data: None,
            error: None,
            error_code: None,
            hint: None,
            elapsed_ms: 1,
            version: "0.1.0".to_string(),
            now: 0,
        };
        let result = resp.into_result();
        prop_assert!(result.is_err(), "into_result should be Err for null data");
        prop_assert!(result.unwrap_err().message.contains("null"),
            "Error message should mention null");
    }

    /// RobotResponse parsed_error_code matches manual parse.
    #[test]
    fn prop_response_parsed_error_code(code in arb_known_error_code()) {
        let code_str = code.as_str();
        let resp: RobotResponse<GetTextData> = RobotResponse {
            ok: false,
            data: None,
            error: Some("test".to_string()),
            error_code: Some(code_str.clone()),
            hint: None,
            elapsed_ms: 1,
            version: "0.1.0".to_string(),
            now: 0,
        };
        let parsed = resp.parsed_error_code();
        prop_assert!(parsed.is_some(), "parsed_error_code should return Some");
        prop_assert_eq!(parsed.unwrap().number(), code.number());
    }
}

// ============================================================================
// Data type serde roundtrip properties
// ============================================================================

proptest! {
    /// TruncationInfo serde roundtrip.
    #[test]
    fn prop_truncation_info_serde(info in arb_truncation_info()) {
        let json = serde_json::to_string(&info).unwrap();
        let back: TruncationInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.original_bytes, info.original_bytes);
        prop_assert_eq!(back.returned_bytes, info.returned_bytes);
        prop_assert_eq!(back.original_lines, info.original_lines);
        prop_assert_eq!(back.returned_lines, info.returned_lines);
    }

    /// WaitForData serde roundtrip.
    #[test]
    fn prop_wait_for_data_serde(data in arb_wait_for_data()) {
        let json = serde_json::to_string(&data).unwrap();
        let back: WaitForData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, data.pane_id);
        prop_assert_eq!(&back.pattern, &data.pattern);
        prop_assert_eq!(back.matched, data.matched);
        prop_assert_eq!(back.elapsed_ms, data.elapsed_ms);
        prop_assert_eq!(back.polls, data.polls);
        prop_assert_eq!(back.is_regex, data.is_regex);
    }

    /// SearchHit serde roundtrip.
    #[test]
    fn prop_search_hit_serde(hit in arb_search_hit()) {
        let json = serde_json::to_string(&hit).unwrap();
        let back: SearchHit = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.segment_id, hit.segment_id);
        prop_assert_eq!(back.pane_id, hit.pane_id);
        prop_assert_eq!(back.seq, hit.seq);
        prop_assert_eq!(back.captured_at, hit.captured_at);
        prop_assert!((back.score - hit.score).abs() < 1e-10,
            "score mismatch: {} vs {}", back.score, hit.score);
    }

    /// WorkflowInfo serde roundtrip.
    #[test]
    fn prop_workflow_info_serde(info in arb_workflow_info()) {
        let json = serde_json::to_string(&info).unwrap();
        let back: WorkflowInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &info.name);
        prop_assert_eq!(back.enabled, info.enabled);
    }

    /// ReservationInfo serde roundtrip.
    #[test]
    fn prop_reservation_info_serde(info in arb_reservation_info()) {
        let json = serde_json::to_string(&info).unwrap();
        let back: ReservationInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, info.id);
        prop_assert_eq!(back.pane_id, info.pane_id);
        prop_assert_eq!(&back.owner_kind, &info.owner_kind);
        prop_assert_eq!(&back.owner_id, &info.owner_id);
        prop_assert_eq!(back.created_at, info.created_at);
        prop_assert_eq!(back.expires_at, info.expires_at);
        prop_assert_eq!(&back.status, &info.status);
    }

    /// LintIssue serde roundtrip.
    #[test]
    fn prop_lint_issue_serde(issue in arb_lint_issue()) {
        let json = serde_json::to_string(&issue).unwrap();
        let back: LintIssue = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.rule_id, &issue.rule_id);
        prop_assert_eq!(&back.category, &issue.category);
        prop_assert_eq!(&back.message, &issue.message);
    }

    /// GetTextData serde roundtrip.
    #[test]
    fn prop_get_text_data_serde(data in arb_get_text_data()) {
        let json = serde_json::to_string(&data).unwrap();
        let back: GetTextData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, data.pane_id);
        prop_assert_eq!(&back.text, &data.text);
        prop_assert_eq!(back.tail_lines, data.tail_lines);
        prop_assert_eq!(back.escapes_included, data.escapes_included);
        prop_assert_eq!(back.truncated, data.truncated);
    }
}

// ============================================================================
// RobotError properties
// ============================================================================

proptest! {
    /// RobotError Display with code shows "[CODE] message".
    #[test]
    fn prop_robot_error_display_with_code(
        code in "[A-Z]{2}-[0-9]{4}",
        msg in "[a-z ]{1,50}",
    ) {
        let err = RobotError {
            code: Some(code.clone()),
            message: msg.clone(),
            hint: None,
        };
        let displayed = format!("{}", err);
        prop_assert!(displayed.contains(&code), "Display missing code: {}", displayed);
        prop_assert!(displayed.contains(&msg), "Display missing message: {}", displayed);
    }

    /// RobotError Display without code shows just message.
    #[test]
    fn prop_robot_error_display_no_code(msg in "[a-z ]{1,50}") {
        let err = RobotError {
            code: None,
            message: msg.clone(),
            hint: None,
        };
        let displayed = format!("{}", err);
        prop_assert_eq!(displayed, msg);
    }
}

// ============================================================================
// Cross-cutting invariants
// ============================================================================

proptest! {
    /// All known error codes map to non-Internal category.
    #[test]
    fn prop_known_codes_have_specific_category(code in arb_known_error_code()) {
        let cat = code.category();
        // Known codes should not all map to Internal (only 9xxx does)
        if code.number() < 9000 {
            prop_assert!(cat != ErrorCategory::Internal,
                "Code {} should not be Internal", code.number());
        }
    }

    /// ErrorCode from_number for all known numbers produces correct number.
    #[test]
    fn prop_from_number_for_known(code in arb_known_error_code()) {
        let n = code.number();
        let back = ErrorCode::from_number(n);
        prop_assert_eq!(back.number(), n,
            "from_number({}) produced number {}", n, back.number());
    }

    /// Unrecognized numbers produce Unknown variant.
    #[test]
    fn prop_unrecognized_numbers_are_unknown(n in 0u16..=65535u16) {
        let known_numbers: Vec<u16> = vec![
            1001, 1002, 1003, 1004, 1005,
            2001, 2002, 2003, 2004, 2005,
            3001, 3002, 3003,
            4001, 4002, 4003, 4004,
            5001, 5002, 5003, 5004,
            6001, 6002,
            7001, 7002,
            9001, 9002, 9003,
        ];
        if !known_numbers.contains(&n) {
            let code = ErrorCode::from_number(n);
            prop_assert_eq!(code, ErrorCode::Unknown(n),
                "from_number({}) should produce Unknown", n);
        }
    }

    /// ErrorCategory Debug is non-empty.
    #[test]
    fn prop_error_category_debug_nonempty(code in arb_known_error_code()) {
        let cat = code.category();
        let debug = format!("{:?}", cat);
        prop_assert!(!debug.is_empty());
    }

    /// ErrorCode Clone preserves number.
    #[test]
    fn prop_error_code_clone_preserves(code in arb_known_error_code()) {
        let cloned = code;
        prop_assert_eq!(cloned.number(), code.number());
    }

    /// ErrorCode Debug is non-empty.
    #[test]
    fn prop_error_code_debug_nonempty(code in arb_known_error_code()) {
        let debug = format!("{:?}", code);
        prop_assert!(!debug.is_empty());
    }
}

// ============================================================================
// Additional strategies for uncovered types
// ============================================================================

fn arb_search_index_stats() -> impl Strategy<Value = SearchIndexStatsData> {
    // Split into two tuples to avoid exceeding proptest's 12-element tuple limit
    let core = (
        "[a-z/]{5,30}",             // index_dir
        "[a-z/.]{5,30}",            // state_path
        1_u32..10,                  // format_version
        0_usize..100_000,           // document_count
        0_usize..100,               // segment_count
        0_u64..10_000_000,          // index_size_bytes
        0_usize..1000,              // pending_docs
        1_000_000_u64..100_000_000, // max_index_size_bytes
        1_u64..365,                 // ttl_days
        1_u64..3600,                // flush_interval_secs
        1_usize..10_000,            // flush_docs_threshold
    );
    let extra = (
        prop_oneof![
            Just("idle".to_string()),
            Just("running".to_string()),
            Just("error".to_string()),
        ],
        0_usize..100,                         // indexing_error_count
        proptest::option::of("[a-z ]{5,30}"), // last_error
    );
    (core, extra).prop_map(
        |(
            (dir, state, ver, docs, segs, size, pending, max_size, ttl, flush_int, flush_docs),
            (job_status, err_count, last_err),
        )| {
            SearchIndexStatsData {
                index_dir: dir,
                state_path: state,
                format_version: ver,
                document_count: docs,
                segment_count: segs,
                index_size_bytes: size,
                pending_docs: pending,
                max_index_size_bytes: max_size,
                ttl_days: ttl,
                flush_interval_secs: flush_int,
                flush_docs_threshold: flush_docs,
                newest_captured_at_ms: None,
                oldest_captured_at_ms: None,
                freshness_age_ms: None,
                last_update_ts: None,
                source_counts: BTreeMap::new(),
                embedder_tiers_available: vec![],
                background_job_status: job_status,
                indexing_error_count: err_count,
                last_error: last_err,
            }
        },
    )
}

fn arb_account_info() -> impl Strategy<Value = AccountInfo> {
    (
        "[a-z0-9-]{5,20}", // account_id
        prop_oneof![
            Just("anthropic".to_string()),
            Just("openai".to_string()),
            Just("google".to_string()),
        ],
        proptest::option::of("[A-Za-z ]{3,20}"), // name
        0.0_f64..100.0,                          // percent_remaining
        proptest::option::of("[0-9T:-]{10,25}"), // reset_at
        proptest::option::of(any::<i64>()),      // tokens_used
        proptest::option::of(any::<i64>()),      // tokens_remaining
        proptest::option::of(any::<i64>()),      // tokens_limit
        any::<i64>(),                            // last_refreshed_at
        proptest::option::of(any::<i64>()),      // last_used_at
    )
        .prop_map(
            |(id, service, name, pct, reset, used, remaining, limit, refreshed, last_used)| {
                AccountInfo {
                    account_id: id,
                    service,
                    name,
                    percent_remaining: pct,
                    reset_at: reset,
                    tokens_used: used,
                    tokens_remaining: remaining,
                    tokens_limit: limit,
                    last_refreshed_at: refreshed,
                    last_used_at: last_used,
                }
            },
        )
}

fn arb_event_item() -> impl Strategy<Value = EventItem> {
    (
        any::<i64>(),   // id
        any::<u64>(),   // pane_id
        "[a-z.]{3,20}", // rule_id
        "[a-z_]{3,15}", // pack_id
        "[a-z_]{3,15}", // event_type
        prop_oneof![
            Just("info".to_string()),
            Just("warning".to_string()),
            Just("error".to_string()),
            Just("critical".to_string()),
        ],
        0.5_f64..1.0, // confidence
        any::<i64>(), // captured_at
    )
        .prop_map(
            |(id, pane_id, rule_id, pack_id, event_type, severity, confidence, captured_at)| {
                EventItem {
                    id,
                    pane_id,
                    rule_id,
                    pack_id,
                    event_type,
                    severity,
                    confidence,
                    extracted: None,
                    annotations: None,
                    captured_at,
                    handled_at: None,
                    workflow_id: None,
                    would_handle_with: None,
                }
            },
        )
}

fn arb_send_data() -> impl Strategy<Value = SendData> {
    (any::<u64>(), proptest::option::of("[a-z ]{5,30}")).prop_map(
        |(pane_id, verification_error)| SendData {
            pane_id,
            injection: serde_json::json!({"text": "hello"}),
            wait_for: None,
            verification_error,
        },
    )
}

fn arb_pane_state_data() -> impl Strategy<Value = PaneStateData> {
    (
        any::<u64>(),                            // pane_id
        proptest::option::of("[a-z0-9-]{8,16}"), // pane_uuid
        any::<u64>(),                            // tab_id
        any::<u64>(),                            // window_id
        "[a-z_]{3,15}",                          // domain
        proptest::option::of("[a-z ]{3,20}"),    // title
        proptest::option::of("[a-z/]{5,30}"),    // cwd
        any::<bool>(),                           // observed
        proptest::option::of("[a-z ]{5,20}"),    // ignore_reason
    )
        .prop_map(
            |(
                pane_id,
                pane_uuid,
                tab_id,
                window_id,
                domain,
                title,
                cwd,
                observed,
                ignore_reason,
            )| {
                PaneStateData {
                    pane_id,
                    pane_uuid,
                    tab_id,
                    window_id,
                    domain,
                    title,
                    cwd,
                    observed,
                    ignore_reason,
                }
            },
        )
}

// ============================================================================
// SearchIndexStatsData serde roundtrip
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_search_index_stats_serde_roundtrip(stats in arb_search_index_stats()) {
        let json = serde_json::to_string(&stats).unwrap();
        let back: SearchIndexStatsData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.index_dir, &stats.index_dir);
        prop_assert_eq!(back.document_count, stats.document_count);
        prop_assert_eq!(back.segment_count, stats.segment_count);
        prop_assert_eq!(back.index_size_bytes, stats.index_size_bytes);
        prop_assert_eq!(back.format_version, stats.format_version);
        prop_assert_eq!(back.indexing_error_count, stats.indexing_error_count);
        prop_assert_eq!(back.last_error, stats.last_error);
    }

    #[test]
    fn prop_search_index_stats_json_structure(stats in arb_search_index_stats()) {
        let json = serde_json::to_string(&stats).unwrap();
        prop_assert!(json.contains("\"document_count\""));
        prop_assert!(json.contains("\"index_size_bytes\""));
        prop_assert!(json.contains("\"background_job_status\""));
    }

    #[test]
    fn prop_search_index_stats_serde_deterministic(stats in arb_search_index_stats()) {
        let j1 = serde_json::to_string(&stats).unwrap();
        let j2 = serde_json::to_string(&stats).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// ============================================================================
// AccountInfo serde roundtrip
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_account_info_serde_roundtrip(info in arb_account_info()) {
        let json = serde_json::to_string(&info).unwrap();
        let back: AccountInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.account_id, &info.account_id);
        prop_assert_eq!(&back.service, &info.service);
        prop_assert_eq!(back.name, info.name);
        prop_assert!((back.percent_remaining - info.percent_remaining).abs() < 1e-10);
        prop_assert_eq!(back.last_refreshed_at, info.last_refreshed_at);
    }

    #[test]
    fn prop_account_info_json_has_service(info in arb_account_info()) {
        let json = serde_json::to_string(&info).unwrap();
        prop_assert!(json.contains("\"service\""));
        prop_assert!(json.contains("\"account_id\""));
    }
}

// ============================================================================
// EventItem serde roundtrip
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_event_item_serde_roundtrip(item in arb_event_item()) {
        let json = serde_json::to_string(&item).unwrap();
        let back: EventItem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, item.id);
        prop_assert_eq!(&back.rule_id, &item.rule_id);
        prop_assert_eq!(&back.severity, &item.severity);
        prop_assert_eq!(back.pane_id, item.pane_id);
        prop_assert_eq!(&back.event_type, &item.event_type);
        prop_assert_eq!(back.captured_at, item.captured_at);
    }

    #[test]
    fn prop_event_item_json_has_rule_id(item in arb_event_item()) {
        let json = serde_json::to_string(&item).unwrap();
        prop_assert!(json.contains("\"rule_id\""));
        prop_assert!(json.contains("\"severity\""));
    }
}

// ============================================================================
// SendData serde roundtrip
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_send_data_serde_roundtrip(data in arb_send_data()) {
        let json = serde_json::to_string(&data).unwrap();
        let back: SendData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, data.pane_id);
        prop_assert_eq!(back.verification_error, data.verification_error);
    }

    #[test]
    fn prop_send_data_json_has_pane_id(data in arb_send_data()) {
        let json = serde_json::to_string(&data).unwrap();
        prop_assert!(json.contains("\"pane_id\""));
    }
}

// ============================================================================
// PaneStateData serde roundtrip
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_pane_state_data_serde_roundtrip(data in arb_pane_state_data()) {
        let json = serde_json::to_string(&data).unwrap();
        let back: PaneStateData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, data.pane_id);
        prop_assert_eq!(back.pane_uuid, data.pane_uuid);
        prop_assert_eq!(back.tab_id, data.tab_id);
        prop_assert_eq!(back.window_id, data.window_id);
        prop_assert_eq!(&back.domain, &data.domain);
        prop_assert_eq!(back.title, data.title);
        prop_assert_eq!(back.cwd, data.cwd);
        prop_assert_eq!(back.observed, data.observed);
        prop_assert_eq!(back.ignore_reason, data.ignore_reason);
    }

    #[test]
    fn prop_pane_state_data_json_structure(data in arb_pane_state_data()) {
        let json = serde_json::to_string(&data).unwrap();
        prop_assert!(json.contains("\"pane_id\""));
        prop_assert!(json.contains("\"tab_id\""));
    }
}

// ============================================================================
// Additional coverage tests (RT-42 through RT-66)
// ============================================================================

fn arb_mission_run_state() -> impl Strategy<Value = MissionRunState> {
    prop_oneof![
        Just(MissionRunState::Pending),
        Just(MissionRunState::Succeeded),
        Just(MissionRunState::Failed),
        Just(MissionRunState::Cancelled),
    ]
}

fn arb_mission_agent_state() -> impl Strategy<Value = MissionAgentState> {
    prop_oneof![
        Just(MissionAgentState::NotRequired),
        Just(MissionAgentState::Pending),
        Just(MissionAgentState::Approved),
        Just(MissionAgentState::Denied),
        Just(MissionAgentState::Expired),
    ]
}

fn arb_mission_action_state() -> impl Strategy<Value = MissionActionState> {
    prop_oneof![
        Just(MissionActionState::Ready),
        Just(MissionActionState::Blocked),
        Just(MissionActionState::Completed),
    ]
}

fn arb_tx_step_risk() -> impl Strategy<Value = TxStepRisk> {
    prop_oneof![
        Just(TxStepRisk::Low),
        Just(TxStepRisk::Medium),
        Just(TxStepRisk::High),
        Just(TxStepRisk::Critical),
    ]
}

fn arb_tx_phase_state() -> impl Strategy<Value = TxPhaseState> {
    prop_oneof![
        Just(TxPhaseState::Planned),
        Just(TxPhaseState::Preparing),
        Just(TxPhaseState::Committing),
        Just(TxPhaseState::Compensating),
        Just(TxPhaseState::Completed),
        Just(TxPhaseState::Aborted),
    ]
}

fn arb_tx_resume_recommendation() -> impl Strategy<Value = TxResumeRecommendation> {
    prop_oneof![
        Just(TxResumeRecommendation::ContinueFromCheckpoint),
        Just(TxResumeRecommendation::RestartFresh),
        Just(TxResumeRecommendation::CompensateAndAbort),
        Just(TxResumeRecommendation::AlreadyComplete),
    ]
}

fn arb_tx_bundle_classification() -> impl Strategy<Value = TxBundleClassification> {
    prop_oneof![
        Just(TxBundleClassification::Internal),
        Just(TxBundleClassification::TeamReview),
        Just(TxBundleClassification::ExternalAudit),
    ]
}

fn arb_tx_precondition_kind() -> impl Strategy<Value = TxPreconditionKind> {
    prop_oneof![
        Just(TxPreconditionKind::PolicyApproved),
        prop::collection::vec("[a-z/]{3,15}", 1..4)
            .prop_map(|paths| TxPreconditionKind::ReservationHeld { paths }),
        "[a-z]{3,10}".prop_map(|approver| TxPreconditionKind::ApprovalRequired { approver }),
        "[a-z0-9-]{5,15}".prop_map(|target_id| TxPreconditionKind::TargetReachable { target_id }),
        (0u64..60_000).prop_map(|max_age_ms| TxPreconditionKind::ContextFresh { max_age_ms }),
    ]
}

fn arb_tx_compensation_kind() -> impl Strategy<Value = TxCompensationKind> {
    prop_oneof![
        Just(TxCompensationKind::Rollback),
        Just(TxCompensationKind::NotifyOperator),
        (1u32..10).prop_map(|max_retries| TxCompensationKind::RetryWithBackoff { max_retries }),
        Just(TxCompensationKind::SkipAndContinue),
        "[a-z0-9-]{5,15}".prop_map(|alternative_step_id| TxCompensationKind::Alternative {
            alternative_step_id
        }),
    ]
}

fn arb_tx_step_outcome() -> impl Strategy<Value = TxStepOutcome> {
    prop_oneof![
        proptest::option::of("[a-z]{3,15}").prop_map(|result| TxStepOutcome::Success { result }),
        ("[a-z.]{3,10}", "[a-z ]{5,20}", proptest::bool::ANY).prop_map(
            |(error_code, error_message, compensated)| TxStepOutcome::Failed {
                error_code,
                error_message,
                compensated
            }
        ),
        "[a-z ]{5,20}".prop_map(|reason| TxStepOutcome::Skipped { reason }),
        "[a-z ]{5,20}".prop_map(|compensation_result| TxStepOutcome::Compensated {
            compensation_result
        }),
        Just(TxStepOutcome::Pending),
    ]
}

fn arb_search_stream_phase() -> impl Strategy<Value = SearchStreamPhase> {
    prop_oneof![
        (0usize..1000).prop_map(|result_count| SearchStreamPhase::Fast { result_count }),
        (0usize..1000).prop_map(|result_count| SearchStreamPhase::Quality { result_count }),
        (0usize..1000, 0u64..1_000_000).prop_map(|(total_results, total_us)| {
            SearchStreamPhase::Done {
                total_results,
                total_us,
            }
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // ── RT-42: MissionRunState serde roundtrip ──────────────────────────────

    #[test]
    fn rt42_mission_run_state_serde(state in arb_mission_run_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let back: MissionRunState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, state);
    }

    // ── RT-43: MissionRunState serializes to snake_case ─────────────────────

    #[test]
    fn rt43_mission_run_state_snake_case(state in arb_mission_run_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let s = json.trim_matches('"');
        prop_assert!(s.chars().all(|c| c.is_lowercase() || c == '_'),
            "MissionRunState should serialize to snake_case: {}", s);
    }

    // ── RT-44: MissionAgentState serde roundtrip ────────────────────────────

    #[test]
    fn rt44_mission_agent_state_serde(state in arb_mission_agent_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let back: MissionAgentState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, state);
    }

    // ── RT-45: MissionAgentState snake_case ─────────────────────────────────

    #[test]
    fn rt45_mission_agent_state_snake_case(state in arb_mission_agent_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let s = json.trim_matches('"');
        prop_assert!(s.chars().all(|c| c.is_lowercase() || c == '_'),
            "MissionAgentState should serialize to snake_case: {}", s);
    }

    // ── RT-46: MissionActionState serde roundtrip ───────────────────────────

    #[test]
    fn rt46_mission_action_state_serde(state in arb_mission_action_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let back: MissionActionState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, state);
    }

    // ── RT-47: TxStepRisk serde roundtrip ───────────────────────────────────

    #[test]
    fn rt47_tx_step_risk_serde(risk in arb_tx_step_risk()) {
        let json = serde_json::to_string(&risk).unwrap();
        let back: TxStepRisk = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, risk);
    }

    // ── RT-48: TxPhaseState serde roundtrip ─────────────────────────────────

    #[test]
    fn rt48_tx_phase_state_serde(phase in arb_tx_phase_state()) {
        let json = serde_json::to_string(&phase).unwrap();
        let back: TxPhaseState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, phase);
    }

    // ── RT-49: TxResumeRecommendation serde roundtrip ───────────────────────

    #[test]
    fn rt49_tx_resume_recommendation_serde(rec in arb_tx_resume_recommendation()) {
        let json = serde_json::to_string(&rec).unwrap();
        let back: TxResumeRecommendation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, rec);
    }

    // ── RT-50: TxBundleClassification serde roundtrip ───────────────────────

    #[test]
    fn rt50_tx_bundle_classification_serde(cls in arb_tx_bundle_classification()) {
        let json = serde_json::to_string(&cls).unwrap();
        let back: TxBundleClassification = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, cls);
    }

    // ── RT-51: TxPreconditionKind tagged serde roundtrip ────────────────────

    #[test]
    fn rt51_tx_precondition_kind_serde(kind in arb_tx_precondition_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: TxPreconditionKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, kind);
        // Verify JSON has "kind" tag
        prop_assert!(json.contains("\"kind\""), "missing kind tag in {}", json);
    }

    // ── RT-52: TxCompensationKind tagged serde roundtrip ────────────────────

    #[test]
    fn rt52_tx_compensation_kind_serde(kind in arb_tx_compensation_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: TxCompensationKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, kind);
        prop_assert!(json.contains("\"kind\""), "missing kind tag in {}", json);
    }

    // ── RT-53: TxStepOutcome tagged serde roundtrip ─────────────────────────

    #[test]
    fn rt53_tx_step_outcome_serde(outcome in arb_tx_step_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let back: TxStepOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, outcome);
        prop_assert!(json.contains("\"kind\""), "missing kind tag in {}", json);
    }

    // ── RT-54: SearchStreamPhase tagged serde roundtrip ─────────────────────

    #[test]
    fn rt54_search_stream_phase_serde(phase in arb_search_stream_phase()) {
        let json = serde_json::to_string(&phase).unwrap();
        let back: SearchStreamPhase = serde_json::from_str(&json).unwrap();
        // Can't use prop_assert_eq because no PartialEq, compare JSON
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(&json, &json2);
        prop_assert!(json.contains("\"type\""), "missing type tag in {}", json);
    }

    // ── RT-55: SearchScoringBreakdown serde roundtrip ───────────────────────

    #[test]
    fn rt55_search_scoring_breakdown_serde(
        final_score in 0.0f64..100.0,
        bm25 in proptest::option::of(0.0f64..100.0),
        semantic in proptest::option::of(0.0f64..1.0),
    ) {
        let breakdown = SearchScoringBreakdown {
            bm25_score: bm25,
            matching_terms: vec!["test".to_string()],
            semantic_similarity: semantic,
            embedder_tier: None,
            rrf_rank: None,
            rrf_score: None,
            reranker_score: None,
            final_score,
        };
        let json = serde_json::to_string(&breakdown).unwrap();
        let back: SearchScoringBreakdown = serde_json::from_str(&json).unwrap();
        prop_assert!((back.final_score - breakdown.final_score).abs() < 1e-10);
        prop_assert_eq!(back.bm25_score.is_some(), breakdown.bm25_score.is_some());
    }

    // ── RT-56: SearchPipelineTiming serde roundtrip ─────────────────────────

    #[test]
    fn rt56_search_pipeline_timing_serde(
        total_us in 0u64..1_000_000,
        lexical_us in proptest::option::of(0u64..500_000),
        semantic_us in proptest::option::of(0u64..500_000),
    ) {
        let timing = SearchPipelineTiming {
            total_us,
            lexical_us,
            semantic_us,
            fusion_us: None,
            rerank_us: None,
        };
        let json = serde_json::to_string(&timing).unwrap();
        let back: SearchPipelineTiming = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_us, timing.total_us);
        prop_assert_eq!(back.lexical_us, timing.lexical_us);
        prop_assert_eq!(back.semantic_us, timing.semantic_us);
    }

    // ── RT-57: MissionAssignmentCounters default is all zeros ───────────────

    #[test]
    fn rt57_mission_counters_default(_dummy in 0u8..1) {
        let counters = MissionAssignmentCounters::default();
        prop_assert_eq!(counters.pending_approval, 0);
        prop_assert_eq!(counters.approved, 0);
        prop_assert_eq!(counters.denied, 0);
        prop_assert_eq!(counters.expired, 0);
        prop_assert_eq!(counters.succeeded, 0);
        prop_assert_eq!(counters.failed, 0);
        prop_assert_eq!(counters.cancelled, 0);
        prop_assert_eq!(counters.unresolved, 0);
    }

    // ── RT-58: MissionAssignmentCounters serde roundtrip ────────────────────

    #[test]
    fn rt58_mission_counters_serde(
        pa in 0usize..100,
        ap in 0usize..100,
        dn in 0usize..100,
        ex in 0usize..100,
        su in 0usize..100,
        fa in 0usize..100,
        ca in 0usize..100,
        un in 0usize..100,
    ) {
        let counters = MissionAssignmentCounters {
            pending_approval: pa, approved: ap, denied: dn, expired: ex,
            succeeded: su, failed: fa, cancelled: ca, unresolved: un,
        };
        let json = serde_json::to_string(&counters).unwrap();
        let back: MissionAssignmentCounters = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pending_approval, counters.pending_approval);
        prop_assert_eq!(back.succeeded, counters.succeeded);
        prop_assert_eq!(back.failed, counters.failed);
        prop_assert_eq!(back.unresolved, counters.unresolved);
    }

    // ── RT-59: MissionTransitionInfo serde roundtrip ────────────────────────

    #[test]
    fn rt59_mission_transition_info_serde(
        kind in "[a-z_]{3,15}",
        to in "[a-z_]{3,15}",
    ) {
        let info = MissionTransitionInfo { kind: kind.clone(), to: to.clone() };
        let json = serde_json::to_string(&info).unwrap();
        let back: MissionTransitionInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.kind, &kind);
        prop_assert_eq!(&back.to, &to);
    }

    // ── RT-60: MissionFailureCatalogEntry serde roundtrip ───────────────────

    #[test]
    fn rt60_mission_failure_catalog_serde(
        reason in "[a-z_]{3,15}",
        error in "[a-z_]{3,15}",
        hint in "[a-z ]{5,30}",
    ) {
        let entry = MissionFailureCatalogEntry {
            reason_code: reason.clone(),
            error_code: error.clone(),
            terminality: "terminal".to_string(),
            retryability: "not_retryable".to_string(),
            human_hint: hint.clone(),
            machine_hint: "check logs".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: MissionFailureCatalogEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.reason_code, &reason);
        prop_assert_eq!(&back.error_code, &error);
        prop_assert_eq!(&back.human_hint, &hint);
    }

    // ── RT-61: TxRiskSummaryData serde roundtrip ────────────────────────────

    #[test]
    fn rt61_tx_risk_summary_serde(
        total in 0usize..100,
        high in 0usize..50,
        critical in 0usize..20,
        uncompensated in 0usize..50,
        risk in arb_tx_step_risk(),
    ) {
        let summary = TxRiskSummaryData {
            total_steps: total,
            high_risk_count: high,
            critical_risk_count: critical,
            uncompensated_steps: uncompensated,
            overall_risk: risk,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let back: TxRiskSummaryData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_steps, summary.total_steps);
        prop_assert_eq!(back.high_risk_count, summary.high_risk_count);
        prop_assert_eq!(back.critical_risk_count, summary.critical_risk_count);
    }

    // ── RT-62: TxChainVerificationData serde roundtrip ──────────────────────

    #[test]
    fn rt62_tx_chain_verification_serde(
        intact in proptest::bool::ANY,
        total in 0usize..1000,
    ) {
        let data = TxChainVerificationData {
            chain_intact: intact,
            first_break_at: if intact { None } else { Some(5) },
            missing_ordinals: vec![],
            total_records: total,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: TxChainVerificationData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.chain_intact, intact);
        prop_assert_eq!(back.total_records, total);
    }

    // ── RT-63: PaneTextResult::Ok tagged serde roundtrip ────────────────────

    #[test]
    fn rt63_pane_text_result_ok_serde(
        text in "[a-z ]{5,50}",
        truncated in proptest::bool::ANY,
    ) {
        let result = PaneTextResult::Ok {
            text: text.clone(),
            truncated,
            truncation_info: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: PaneTextResult = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(&json, &json2);
        prop_assert!(json.contains("\"status\":\"ok\""), "missing ok tag: {}", json);
    }

    // ── RT-64: PaneTextResult::Error tagged serde roundtrip ─────────────────

    #[test]
    fn rt64_pane_text_result_error_serde(
        code in "[A-Z]{2}-[0-9]{4}",
        message in "[a-z ]{5,30}",
    ) {
        let result = PaneTextResult::Error {
            code: code.clone(),
            message: message.clone(),
            hint: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: PaneTextResult = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(&json, &json2);
        prop_assert!(json.contains("\"status\":\"error\""), "missing error tag: {}", json);
    }

    // ── RT-65: SearchData serde roundtrip ───────────────────────────────────

    #[test]
    fn rt65_search_data_serde(
        query in "[a-z ]{3,20}",
        total in 0usize..1000,
        limit in 1usize..100,
    ) {
        let data = SearchData {
            query: query.clone(),
            results: vec![],
            total_hits: total,
            limit,
            pane_filter: None,
            since_filter: None,
            until_filter: None,
            mode: Some("hybrid".to_string()),
            metrics: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: SearchData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.query, &query);
        prop_assert_eq!(back.total_hits, total);
        prop_assert_eq!(back.limit, limit);
    }

    // ── RT-66: WorkflowRunData serde roundtrip ──────────────────────────────

    #[test]
    fn rt66_workflow_run_data_serde(
        name in "[a-z_]{3,20}",
        pane_id in 0u64..1000,
        status in prop_oneof![Just("running".to_string()), Just("completed".to_string()), Just("failed".to_string())],
    ) {
        let data = WorkflowRunData {
            workflow_name: name.clone(),
            pane_id,
            execution_id: Some("exec-123".to_string()),
            status: status.clone(),
            message: None,
            started_at: Some(1_700_000_000_000),
            step_index: None,
            elapsed_ms: Some(42),
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: WorkflowRunData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.workflow_name, &name);
        prop_assert_eq!(back.pane_id, pane_id);
        prop_assert_eq!(&back.status, &status);
    }
}
