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

// ============================================================================
// Extended coverage: remaining 67 uncovered types (PinkForge session 16)
// ============================================================================

use frankenterm_core::plan::{ApprovalState, MissionActorRole, MissionLifecycleState, Outcome};

fn arb_short_str2() -> impl Strategy<Value = String> {
    "[a-z0-9_]{1,15}"
}

fn arb_mission_lifecycle_state() -> impl Strategy<Value = MissionLifecycleState> {
    prop_oneof![
        Just(MissionLifecycleState::Planned),
        Just(MissionLifecycleState::Planning),
        Just(MissionLifecycleState::Dispatching),
        Just(MissionLifecycleState::AwaitingApproval),
        Just(MissionLifecycleState::Running),
        Just(MissionLifecycleState::Executing),
        Just(MissionLifecycleState::RetryPending),
        Just(MissionLifecycleState::Blocked),
        Just(MissionLifecycleState::Paused),
        Just(MissionLifecycleState::Completed),
        Just(MissionLifecycleState::Cancelled),
        Just(MissionLifecycleState::Failed),
    ]
}

fn arb_mission_actor_role() -> impl Strategy<Value = MissionActorRole> {
    prop_oneof![
        Just(MissionActorRole::Planner),
        Just(MissionActorRole::Dispatcher),
        Just(MissionActorRole::Operator),
    ]
}

fn arb_approval_state() -> impl Strategy<Value = ApprovalState> {
    prop_oneof![
        Just(ApprovalState::NotRequired),
        (arb_short_str2(), 0i64..2_000_000_000_000).prop_map(|(by, at)| {
            ApprovalState::Pending {
                requested_by: by,
                requested_at_ms: at,
            }
        }),
        (arb_short_str2(), 0i64..2_000_000_000_000, arb_short_str2()).prop_map(|(by, at, hash)| {
            ApprovalState::Approved {
                approved_by: by,
                approved_at_ms: at,
                approval_code_hash: hash,
            }
        }),
        (arb_short_str2(), 0i64..2_000_000_000_000, arb_short_str2()).prop_map(
            |(by, at, reason)| {
                ApprovalState::Denied {
                    denied_by: by,
                    denied_at_ms: at,
                    reason_code: reason,
                }
            }
        ),
        (0i64..2_000_000_000_000, arb_short_str2()).prop_map(|(at, reason)| {
            ApprovalState::Expired {
                expired_at_ms: at,
                reason_code: reason,
            }
        }),
    ]
}

fn arb_outcome() -> impl Strategy<Value = Outcome> {
    prop_oneof![
        (arb_short_str2(), 0i64..2_000_000_000_000).prop_map(|(r, at)| {
            Outcome::Success {
                reason_code: r,
                completed_at_ms: at,
            }
        }),
        (arb_short_str2(), arb_short_str2(), 0i64..2_000_000_000_000).prop_map(|(r, e, at)| {
            Outcome::Failed {
                reason_code: r,
                error_code: e,
                completed_at_ms: at,
            }
        }),
        (arb_short_str2(), 0i64..2_000_000_000_000).prop_map(|(r, at)| {
            Outcome::Cancelled {
                reason_code: r,
                completed_at_ms: at,
            }
        }),
    ]
}

fn arb_mission_transition_info() -> impl Strategy<Value = MissionTransitionInfo> {
    (arb_short_str2(), arb_short_str2()).prop_map(|(kind, to)| MissionTransitionInfo { kind, to })
}

fn arb_mission_failure_catalog_entry() -> impl Strategy<Value = MissionFailureCatalogEntry> {
    (
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
    )
        .prop_map(|(rc, ec, term, retry, hh, mh)| MissionFailureCatalogEntry {
            reason_code: rc,
            error_code: ec,
            terminality: term,
            retryability: retry,
            human_hint: hh,
            machine_hint: mh,
        })
}

fn arb_mission_assignment_counters() -> impl Strategy<Value = MissionAssignmentCounters> {
    (
        0usize..10,
        0usize..10,
        0usize..10,
        0usize..10,
        0usize..10,
        0usize..10,
        0usize..10,
        0usize..10,
    )
        .prop_map(
            |(pa, ap, dn, ex, su, fa, ca, un)| MissionAssignmentCounters {
                pending_approval: pa,
                approved: ap,
                denied: dn,
                expired: ex,
                succeeded: su,
                failed: fa,
                cancelled: ca,
                unresolved: un,
            },
        )
}

fn arb_mission_state_filters() -> impl Strategy<Value = MissionStateFilters> {
    (0usize..100,).prop_map(|(limit,)| MissionStateFilters {
        mission_state: None,
        run_state: None,
        agent_state: None,
        action_state: None,
        assignment_id: None,
        assignee: None,
        limit,
    })
}

fn arb_mission_assignment_data() -> impl Strategy<Value = MissionAssignmentData> {
    (
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
        arb_mission_actor_role(),
        arb_short_str2(),
        arb_mission_run_state(),
        arb_mission_agent_state(),
        arb_mission_action_state(),
        arb_approval_state(),
    )
        .prop_map(
            |(
                aid,
                cid,
                assignee,
                assigned_by,
                action_type,
                run_state,
                agent_state,
                action_state,
                approval_state,
            )| {
                MissionAssignmentData {
                    assignment_id: aid,
                    candidate_id: cid,
                    assignee,
                    assigned_by,
                    action_type,
                    run_state,
                    agent_state,
                    action_state,
                    approval_state,
                    outcome: None,
                    reason_code: None,
                    error_code: None,
                }
            },
        )
}

fn arb_mission_state_data() -> impl Strategy<Value = MissionStateData> {
    (
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
        arb_mission_lifecycle_state(),
        proptest::bool::ANY,
        0usize..10,
        0usize..10,
    )
        .prop_map(
            |(mf, mid, title, hash, ls, matches, cc, ac)| MissionStateData {
                mission_file: mf,
                mission_id: mid,
                title,
                mission_hash: hash,
                lifecycle_state: ls,
                mission_matches_filter: matches,
                candidate_count: cc,
                assignment_count: ac,
                matched_assignment_count: 0,
                returned_assignment_count: 0,
                filters: MissionStateFilters {
                    mission_state: None,
                    run_state: None,
                    agent_state: None,
                    action_state: None,
                    assignment_id: None,
                    assignee: None,
                    limit: 100,
                },
                assignment_counters: MissionAssignmentCounters::default(),
                available_transitions: vec![],
                assignments: vec![],
            },
        )
}

fn arb_tx_precondition_data() -> impl Strategy<Value = TxPreconditionData> {
    (
        arb_tx_precondition_kind(),
        arb_short_str2(),
        proptest::bool::ANY,
    )
        .prop_map(|(kind, desc, req)| TxPreconditionData {
            kind,
            description: desc,
            required: req,
        })
}

fn arb_tx_compensating_action_data() -> impl Strategy<Value = TxCompensatingActionData> {
    (
        arb_short_str2(),
        arb_short_str2(),
        arb_tx_compensation_kind(),
    )
        .prop_map(|(sid, desc, at)| TxCompensatingActionData {
            step_id: sid,
            description: desc,
            action_type: at,
        })
}

fn arb_tx_risk_summary_data() -> impl Strategy<Value = TxRiskSummaryData> {
    (
        0usize..50,
        0usize..10,
        0usize..5,
        0usize..10,
        arb_tx_step_risk(),
    )
        .prop_map(|(ts, hr, cr, uc, or)| TxRiskSummaryData {
            total_steps: ts,
            high_risk_count: hr,
            critical_risk_count: cr,
            uncompensated_steps: uc,
            overall_risk: or,
        })
}

fn arb_tx_rejected_edge_data() -> impl Strategy<Value = TxRejectedEdgeData> {
    (arb_short_str2(), arb_short_str2(), arb_short_str2()).prop_map(|(f, t, r)| {
        TxRejectedEdgeData {
            from_step: f,
            to_step: t,
            reason: r,
        }
    })
}

fn arb_tx_step_data() -> impl Strategy<Value = TxStepData> {
    (
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
        arb_tx_step_risk(),
    )
        .prop_map(|(id, bid, aid, desc, risk)| TxStepData {
            id,
            bead_id: bid,
            agent_id: aid,
            description: desc,
            depends_on: vec![],
            preconditions: vec![],
            compensations: vec![],
            risk,
            score: 0.5,
        })
}

fn arb_tx_step_record_data() -> impl Strategy<Value = TxStepRecordData> {
    (
        0u64..1000,
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
        0u64..2_000_000_000_000,
        arb_tx_step_outcome(),
        arb_tx_step_risk(),
        arb_short_str2(),
        arb_short_str2(),
    )
        .prop_map(
            |(ord, sid, idem, eid, ts, outcome, risk, prev, aid)| TxStepRecordData {
                ordinal: ord,
                step_id: sid,
                idem_key: idem,
                execution_id: eid,
                timestamp_ms: ts,
                outcome,
                risk,
                prev_hash: prev,
                agent_id: aid,
            },
        )
}

fn arb_tx_chain_verification_data() -> impl Strategy<Value = TxChainVerificationData> {
    (proptest::bool::ANY, 0usize..100).prop_map(|(intact, total)| TxChainVerificationData {
        chain_intact: intact,
        first_break_at: None,
        missing_ordinals: vec![],
        total_records: total,
    })
}

fn arb_tx_timeline_entry_data() -> impl Strategy<Value = TxTimelineEntryData> {
    (
        0u64..2_000_000_000_000,
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
    )
        .prop_map(
            |(ts, phase, sid, kind, rc, summary, aid, hash)| TxTimelineEntryData {
                timestamp_ms: ts,
                phase,
                step_id: sid,
                kind,
                reason_code: rc,
                summary,
                agent_id: aid,
                ordinal: None,
                record_hash: hash,
            },
        )
}

fn arb_pipeline_watermark_info() -> impl Strategy<Value = PipelineWatermarkInfo> {
    (0u64..1000, 0i64..2_000_000_000_000, 0u64..100_000).prop_map(|(pid, last, total)| {
        PipelineWatermarkInfo {
            pane_id: pid,
            last_indexed_at_ms: last,
            total_docs_indexed: total,
            session_id: None,
        }
    })
}

fn arb_rule_item() -> impl Strategy<Value = RuleItem> {
    (
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
        arb_short_str2(),
        0usize..10,
        proptest::bool::ANY,
    )
        .prop_map(|(id, at, et, sev, desc, ac, hr)| RuleItem {
            id,
            agent_type: at,
            event_type: et,
            severity: sev,
            description: desc,
            workflow: None,
            anchor_count: ac,
            has_regex: hr,
        })
}

fn arb_rule_match_item() -> impl Strategy<Value = RuleMatchItem> {
    (
        arb_short_str2(),
        0usize..1000,
        0usize..1000,
        arb_short_str2(),
    )
        .prop_map(|(rid, s, e, mt)| RuleMatchItem {
            rule_id: rid,
            start: s,
            end: e,
            matched_text: mt,
            trace: None,
        })
}

fn arb_rule_trace_info() -> impl Strategy<Value = RuleTraceInfo> {
    (proptest::bool::ANY, proptest::bool::ANY).prop_map(|(a, r)| RuleTraceInfo {
        anchors_checked: a,
        regex_matched: r,
    })
}

fn arb_workflow_step_log() -> impl Strategy<Value = WorkflowStepLog> {
    (
        0usize..100,
        arb_short_str2(),
        arb_short_str2(),
        0i64..2_000_000_000_000,
    )
        .prop_map(|(idx, name, rt, started)| WorkflowStepLog {
            step_index: idx,
            step_name: name,
            result_type: rt,
            step_id: None,
            step_kind: None,
            result_data: None,
            policy_summary: None,
            verification_refs: None,
            error_code: None,
            started_at: started,
            completed_at: None,
            duration_ms: None,
        })
}

fn arb_workflow_action_plan() -> impl Strategy<Value = WorkflowActionPlan> {
    (arb_short_str2(), arb_short_str2()).prop_map(|(pid, hash)| WorkflowActionPlan {
        plan_id: pid,
        plan_hash: hash,
        plan: None,
        created_at: None,
    })
}

// ============================================================================
// Serde roundtrip tests for uncovered types
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // --- Pane / text types ---

    #[test]
    fn rt67_batch_get_text_data_serde(pane_id in 0u64..1000, lines in 0usize..500) {
        let data = BatchGetTextData {
            pane_ids: vec![pane_id],
            tail_lines: lines,
            escapes_included: false,
            results: BTreeMap::new(),
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: BatchGetTextData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_ids, vec![pane_id]);
        prop_assert_eq!(back.tail_lines, lines);
    }

    #[test]
    fn rt68_state_with_text_data_serde(pane_id in 0u64..1000) {
        let data = StateWithTextData {
            panes: vec![PaneStateData {
                pane_id, pane_uuid: None, tab_id: 0, window_id: 0,
                domain: "local".to_string(), title: None, cwd: None,
                observed: true, ignore_reason: None,
            }],
            tail_lines: 100,
            escapes_included: false,
            pane_text: BTreeMap::new(),
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: StateWithTextData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.panes.len(), 1);
        prop_assert_eq!(back.panes[0].pane_id, pane_id);
    }

    // --- Search types ---

    #[test]
    fn rt69_search_data_serde(query in "[a-z ]{1,20}", total in 0usize..1000, limit in 1usize..100) {
        let data = SearchData {
            query: query.clone(), results: vec![], total_hits: total, limit,
            pane_filter: None, since_filter: None, until_filter: None,
            mode: Some("hybrid".to_string()), metrics: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: SearchData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.query, &query);
        prop_assert_eq!(back.total_hits, total);
    }

    #[test]
    fn rt70_explained_search_hit_serde(seg in 0i64..10000, pane in 0u64..100) {
        let data = ExplainedSearchHit {
            hit: SearchHit {
                segment_id: seg, pane_id: pane, seq: 0, captured_at: 1_700_000_000_000,
                score: 0.95, snippet: None, content: None, semantic_score: None, fusion_rank: None,
            },
            scoring: SearchScoringBreakdown {
                bm25_score: Some(1.5), matching_terms: vec!["test".to_string()],
                semantic_similarity: None, embedder_tier: None,
                rrf_rank: None, rrf_score: None, reranker_score: None, final_score: 0.95,
            },
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: ExplainedSearchHit = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.hit.segment_id, seg);
        prop_assert_eq!(back.hit.pane_id, pane);
    }

    #[test]
    fn rt71_search_explain_data_serde(query in "[a-z]{1,15}", mode in "[a-z]{3,10}") {
        let data = SearchExplainData {
            query: query.clone(), results: vec![], total_hits: 0, limit: 10,
            pane_filter: None, mode: mode.clone(), timing: None, tier_metrics: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: SearchExplainData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.query, &query);
        prop_assert_eq!(&back.mode, &mode);
    }

    #[test]
    fn rt72_search_pipeline_status_serde(state in "[a-z]{3,10}", ticks in 0u64..100000) {
        let data = SearchPipelineStatusData {
            state: state.clone(), watermarks: vec![], total_ticks: ticks,
            total_docs_indexed: 0, total_lines_consumed: 0, index_stats: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: SearchPipelineStatusData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.state, &state);
        prop_assert_eq!(back.total_ticks, ticks);
    }

    #[test]
    fn rt73_pipeline_watermark_info_serde(val in arb_pipeline_watermark_info()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: PipelineWatermarkInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, val.pane_id);
        prop_assert_eq!(back.total_docs_indexed, val.total_docs_indexed);
    }

    #[test]
    fn rt74_search_pipeline_control_serde(action in "[a-z]{3,10}", success in proptest::bool::ANY) {
        let data = SearchPipelineControlResult {
            action: action.clone(), success, state_after: "running".to_string(), message: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: SearchPipelineControlResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.action, &action);
        prop_assert_eq!(back.success, success);
    }

    #[test]
    fn rt75_search_metrics_data_serde(req_mode in "[a-z]{3,10}", eff_mode in "[a-z]{3,10}") {
        let data = SearchMetricsData {
            requested_mode: req_mode.clone(), effective_mode: eff_mode.clone(),
            fallback_reason: None, rrf_k: 60, lexical_weight: 0.5, semantic_weight: 0.5,
            fusion_backend: "frankensearch_rrf".to_string(),
            lexical_candidates: 100, semantic_candidates: 50,
            semantic_cache_hit: false, semantic_latency_ms: 42,
            semantic_rows_scanned: 1000, semantic_budget_state: "nominal".to_string(),
            semantic_backoff_until_ms: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: SearchMetricsData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.requested_mode, &req_mode);
        prop_assert_eq!(&back.effective_mode, &eff_mode);
    }

    #[test]
    fn rt76_search_index_reindex_serde(batch in 1usize..1000, scanned in 0usize..10000) {
        let data = SearchIndexReindexData {
            batch_size: batch, pane_filter: None, since_filter: None, until_filter: None,
            scanned_segments: scanned, submitted_docs: 0, accepted_docs: 0,
            skipped_empty_docs: 0, skipped_duplicate_docs: 0, skipped_cass_docs: 0,
            skipped_resize_pause_docs: 0, deferred_rate_limited_docs: 0, flushed_docs: 0,
            expired_docs: 0, evicted_docs: 0, pane_metadata_docs: 0, flush_operations: 0,
            final_document_count: 0, final_index_size_bytes: 0, source_counts: BTreeMap::new(),
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: SearchIndexReindexData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.batch_size, batch);
        prop_assert_eq!(back.scanned_segments, scanned);
    }

    // --- Events types ---

    #[test]
    fn rt77_events_data_serde(total in 0usize..1000, limit in 1usize..100) {
        let data = EventsData {
            events: vec![], total_count: total, limit,
            pane_filter: None, rule_id_filter: None, event_type_filter: None,
            triage_state_filter: None, label_filter: None,
            unhandled_only: false, since_filter: None, would_handle: false, dry_run: false,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: EventsData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_count, total);
        prop_assert_eq!(back.limit, limit);
    }

    #[test]
    fn rt78_event_would_handle_serde(wf in "[a-z_]{3,15}") {
        let data = EventWouldHandle {
            workflow: wf.clone(), preview_command: None, first_step: None,
            estimated_duration_ms: Some(5000), would_run: Some(true), reason: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: EventWouldHandle = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.workflow, &wf);
    }

    #[test]
    fn rt79_event_mutation_data_serde(eid in 0i64..100_000) {
        let data = EventMutationData {
            event_id: eid, changed: Some(true),
            annotations: serde_json::json!({"key": "val"}),
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: EventMutationData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.event_id, eid);
    }

    // --- Agent inventory types ---

    #[test]
    fn rt80_agent_inventory_data_serde(slug in "[a-z]{3,10}", detected in proptest::bool::ANY) {
        let data = AgentInventoryData {
            installed: vec![InstalledAgentInfo {
                slug: slug.clone(), display_name: None, detected, evidence: vec![],
                root_paths: vec![], config_path: None, binary_path: None, version: None,
            }],
            running: BTreeMap::new(),
            summary: AgentInventorySummary { installed_count: 1, running_count: 0, configured_count: 0, installed_but_idle_count: 1 },
            filesystem_detection_available: true,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: AgentInventoryData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.installed[0].slug, &slug);
        prop_assert_eq!(back.installed[0].detected, detected);
    }

    #[test]
    fn rt81_installed_agent_info_serde(slug in "[a-z]{3,10}") {
        let data = InstalledAgentInfo {
            slug: slug.clone(), display_name: Some("Test Agent".to_string()),
            detected: true, evidence: vec!["found binary".to_string()],
            root_paths: vec!["/usr/local/bin".to_string()],
            config_path: Some("/home/.config".to_string()), binary_path: None, version: Some("1.0".to_string()),
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: InstalledAgentInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.slug, &slug);
        prop_assert!(back.detected);
    }

    #[test]
    fn rt82_running_agent_info_serde(slug in "[a-z]{3,10}", pane in 0u64..1000) {
        let data = RunningAgentInfo {
            slug: slug.clone(), display_name: None,
            state: "working".to_string(), session_id: None,
            source: "pattern_engine".to_string(), pane_id: pane,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: RunningAgentInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.slug, &slug);
        prop_assert_eq!(back.pane_id, pane);
    }

    #[test]
    fn rt83_agent_detect_refresh_serde(refreshed in proptest::bool::ANY, count in 0usize..20) {
        let data = AgentDetectRefreshResult {
            refreshed, detected_count: count, total_probed: 10, message: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: AgentDetectRefreshResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.refreshed, refreshed);
        prop_assert_eq!(back.detected_count, count);
    }

    #[test]
    fn rt84_agent_inventory_summary_serde(ic in 0usize..20, rc in 0usize..20) {
        let data = AgentInventorySummary {
            installed_count: ic, running_count: rc, configured_count: 0, installed_but_idle_count: 0,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: AgentInventorySummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.installed_count, ic);
        prop_assert_eq!(back.running_count, rc);
    }

    // --- Agent configure types ---

    #[test]
    fn rt85_agent_configure_data_serde(total in 0usize..20) {
        let data = AgentConfigureData {
            results: vec![], total, created: 0, updated: 0, skipped: 0, errors: 0,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: AgentConfigureData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total, total);
    }

    #[test]
    fn rt86_agent_configure_result_item_serde(slug in "[a-z]{3,10}") {
        let data = AgentConfigureResultItem {
            slug: slug.clone(), display_name: "Test".to_string(),
            action: "created".to_string(), filename: "AGENTS.md".to_string(),
            backup_created: false, error: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: AgentConfigureResultItem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.slug, &slug);
    }

    #[test]
    fn rt87_agent_configure_dry_run_serde(total in 0usize..20) {
        let data = AgentConfigureDryRunData {
            plan: vec![], total, would_create: 0, would_modify: 0, would_skip: 0, errors: 0,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: AgentConfigureDryRunData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total, total);
    }

    #[test]
    fn rt88_agent_configure_plan_item_serde(slug in "[a-z]{3,10}") {
        let data = AgentConfigurePlanItem {
            slug: slug.clone(), display_name: "Test".to_string(),
            config_kind: "claude_md".to_string(), scope: "project".to_string(),
            filename: "CLAUDE.md".to_string(), file_exists: true,
            section_exists: false, action: "append".to_string(),
            content_preview: Some("# FrankenTerm".to_string()), error: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: AgentConfigurePlanItem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.slug, &slug);
        prop_assert!(back.file_exists);
    }

    // --- Mission types ---

    #[test]
    fn rt89_mission_state_filters_serde(limit in 0usize..1000) {
        let data = MissionStateFilters {
            mission_state: Some(MissionLifecycleState::Running),
            run_state: Some(MissionRunState::Pending),
            agent_state: None, action_state: None,
            assignment_id: None, assignee: Some("agent-1".to_string()), limit,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: MissionStateFilters = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.limit, limit);
    }

    #[test]
    fn rt90_mission_assignment_data_serde(val in arb_mission_assignment_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: MissionAssignmentData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.assignment_id, &val.assignment_id);
        prop_assert_eq!(back.run_state, val.run_state);
        prop_assert_eq!(back.agent_state, val.agent_state);
    }

    #[test]
    fn rt91_mission_state_data_serde(val in arb_mission_state_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: MissionStateData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.mission_id, &val.mission_id);
        prop_assert_eq!(back.lifecycle_state, val.lifecycle_state);
    }

    #[test]
    fn rt92_mission_decision_data_serde(val in arb_mission_assignment_data()) {
        let data = MissionDecisionData {
            assignment: val, candidate_action: None, dispatch_contract: None,
            dispatch_target: None, dry_run_execution: None, decision_error: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: MissionDecisionData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.assignment.assignment_id, &data.assignment.assignment_id);
    }

    #[test]
    fn rt93_mission_decisions_data_serde(val in arb_mission_state_data()) {
        let data = MissionDecisionsData {
            mission_file: val.mission_file, mission_id: val.mission_id.clone(),
            title: val.title, mission_hash: val.mission_hash,
            lifecycle_state: val.lifecycle_state,
            mission_matches_filter: val.mission_matches_filter,
            candidate_count: val.candidate_count, assignment_count: val.assignment_count,
            matched_assignment_count: 0, returned_assignment_count: 0,
            filters: val.filters, available_transitions: vec![],
            failure_catalog: vec![], decisions: vec![],
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: MissionDecisionsData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.mission_id, &val.mission_id);
    }

    // --- Transaction types ---

    #[test]
    fn rt94_tx_precondition_data_serde(val in arb_tx_precondition_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: TxPreconditionData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.description, &val.description);
        prop_assert_eq!(back.required, val.required);
    }

    #[test]
    fn rt95_tx_compensating_action_serde(val in arb_tx_compensating_action_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: TxCompensatingActionData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.step_id, &val.step_id);
    }

    #[test]
    fn rt96_tx_risk_summary_serde(val in arb_tx_risk_summary_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: TxRiskSummaryData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_steps, val.total_steps);
        prop_assert_eq!(back.overall_risk, val.overall_risk);
    }

    #[test]
    fn rt97_tx_rejected_edge_serde(val in arb_tx_rejected_edge_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: TxRejectedEdgeData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.from_step, &val.from_step);
        prop_assert_eq!(&back.reason, &val.reason);
    }

    #[test]
    fn rt98_tx_step_data_serde(val in arb_tx_step_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: TxStepData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.id, &val.id);
        prop_assert_eq!(back.risk, val.risk);
    }

    #[test]
    fn rt99_tx_plan_data_serde(pid in arb_short_str2(), hash in 0u64..1_000_000) {
        let data = TxPlanData {
            plan_id: pid.clone(), plan_hash: hash,
            steps: vec![], execution_order: vec![], parallel_levels: vec![],
            risk_summary: TxRiskSummaryData {
                total_steps: 0, high_risk_count: 0, critical_risk_count: 0,
                uncompensated_steps: 0, overall_risk: TxStepRisk::Low,
            },
            rejected_edges: vec![],
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: TxPlanData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.plan_id, &pid);
        prop_assert_eq!(back.plan_hash, hash);
    }

    #[test]
    fn rt100_tx_step_record_serde(val in arb_tx_step_record_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: TxStepRecordData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.ordinal, val.ordinal);
        prop_assert_eq!(&back.step_id, &val.step_id);
    }

    #[test]
    fn rt101_tx_run_data_serde(eid in arb_short_str2(), pid in arb_short_str2()) {
        let data = TxRunData {
            execution_id: eid.clone(), plan_id: pid.clone(), plan_hash: 42,
            phase: TxPhaseState::Committing, step_count: 3,
            completed_count: 1, failed_count: 0, skipped_count: 0,
            records: vec![],
            chain_verification: TxChainVerificationData {
                chain_intact: true, first_break_at: None, missing_ordinals: vec![], total_records: 0,
            },
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: TxRunData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.execution_id, &eid);
        prop_assert_eq!(&back.plan_id, &pid);
    }

    #[test]
    fn rt102_tx_resume_data_serde(eid in arb_short_str2(), rec in arb_tx_resume_recommendation()) {
        let data = TxResumeData {
            execution_id: eid.clone(), plan_id: "plan-1".to_string(),
            interrupted_phase: TxPhaseState::Committing,
            completed_steps: vec!["s1".to_string()],
            failed_steps: vec![], remaining_steps: vec!["s2".to_string()],
            compensated_steps: vec![], chain_intact: true,
            last_hash: "abc123".to_string(), recommendation: rec,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: TxResumeData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.execution_id, &eid);
        prop_assert_eq!(back.recommendation, rec);
    }

    #[test]
    fn rt103_tx_rollback_data_serde(eid in arb_short_str2()) {
        let data = TxRollbackData {
            execution_id: eid.clone(), plan_id: "plan-1".to_string(),
            phase: TxPhaseState::Compensating,
            compensated_steps: vec!["s1".to_string()],
            failed_compensations: vec![], total_compensated: 1, total_failed: 0,
            chain_verification: TxChainVerificationData {
                chain_intact: true, first_break_at: None, missing_ordinals: vec![], total_records: 1,
            },
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: TxRollbackData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.execution_id, &eid);
        prop_assert_eq!(back.total_compensated, 1);
    }

    #[test]
    fn rt104_tx_timeline_entry_serde(val in arb_tx_timeline_entry_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: TxTimelineEntryData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.timestamp_ms, val.timestamp_ms);
        prop_assert_eq!(&back.step_id, &val.step_id);
    }

    #[test]
    fn rt105_tx_show_data_serde(eid in arb_short_str2(), cls in arb_tx_bundle_classification()) {
        let data = TxShowData {
            execution_id: eid.clone(), plan_id: "plan-1".to_string(), plan_hash: 42,
            phase: TxPhaseState::Completed, classification: cls,
            step_count: 3, record_count: 3, high_risk_count: 0,
            critical_risk_count: 0, overall_risk: TxStepRisk::Low,
            chain_intact: true, timeline: vec![], resume: None,
            records: vec![], redacted_field_count: 0,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: TxShowData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.execution_id, &eid);
        prop_assert_eq!(back.classification, cls);
    }

    // --- Workflow types ---

    #[test]
    fn rt106_workflow_list_data_serde(total in 0usize..100) {
        let data = WorkflowListData {
            workflows: vec![WorkflowInfo {
                name: "test".to_string(), enabled: true,
                trigger_event_types: None, requires_pane: None,
            }],
            total, enabled_count: Some(1),
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: WorkflowListData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total, total);
    }

    #[test]
    fn rt107_workflow_status_data_serde(eid in arb_short_str2(), name in "[a-z_]{3,15}") {
        let data = WorkflowStatusData {
            execution_id: eid.clone(), workflow_name: name.clone(),
            pane_id: Some(1), trigger_event_id: None,
            status: "running".to_string(), message: None,
            started_at: Some(1_700_000_000_000), completed_at: None,
            current_step: Some(0), total_steps: Some(3),
            plan: None, created_at: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: WorkflowStatusData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.execution_id, &eid);
        prop_assert_eq!(&back.workflow_name, &name);
    }

    #[test]
    fn rt108_workflow_status_list_serde(count in 0usize..50) {
        let data = WorkflowStatusListData {
            executions: vec![], pane_filter: None, active_only: Some(true), count,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: WorkflowStatusListData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.count, count);
    }

    #[test]
    fn rt109_workflow_abort_data_serde(eid in arb_short_str2(), aborted in proptest::bool::ANY) {
        let data = WorkflowAbortData {
            execution_id: eid.clone(), aborted, forced: false,
            workflow_name: Some("test_wf".to_string()), previous_status: Some("running".to_string()),
            message: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: WorkflowAbortData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.execution_id, &eid);
        prop_assert_eq!(back.aborted, aborted);
    }

    #[test]
    fn rt110_workflow_step_log_serde(val in arb_workflow_step_log()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: WorkflowStepLog = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.step_index, val.step_index);
        prop_assert_eq!(&back.step_name, &val.step_name);
    }

    #[test]
    fn rt111_workflow_action_plan_serde(val in arb_workflow_action_plan()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: WorkflowActionPlan = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.plan_id, &val.plan_id);
        prop_assert_eq!(&back.plan_hash, &val.plan_hash);
    }

    #[test]
    fn rt112_workflow_status_detail_serde(eid in arb_short_str2(), name in "[a-z_]{3,15}") {
        let data = WorkflowStatusDetailData {
            execution_id: eid.clone(), workflow_name: name.clone(),
            pane_id: Some(1), trigger_event_id: None, status: "completed".to_string(),
            step_name: None, elapsed_ms: Some(100), last_step_result: None,
            current_step: None, total_steps: Some(3), wait_condition: None,
            context: None, result: None, error: None,
            started_at: Some(1_700_000_000_000), updated_at: None,
            completed_at: Some(1_700_000_000_100), step_logs: None, action_plan: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: WorkflowStatusDetailData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.execution_id, &eid);
        prop_assert_eq!(&back.workflow_name, &name);
    }

    // --- Rules types ---

    #[test]
    fn rt113_rules_list_data_serde(val in arb_rule_item()) {
        let data = RulesListData {
            rules: vec![val.clone()], pack_filter: None, agent_type_filter: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: RulesListData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.rules.len(), 1);
        prop_assert_eq!(&back.rules[0].id, &val.id);
    }

    #[test]
    fn rt114_rule_item_serde(val in arb_rule_item()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: RuleItem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.id, &val.id);
        prop_assert_eq!(back.anchor_count, val.anchor_count);
        prop_assert_eq!(back.has_regex, val.has_regex);
    }

    #[test]
    fn rt115_rules_test_data_serde(tl in 0usize..10000, mc in 0usize..100) {
        let data = RulesTestData { text_length: tl, match_count: mc, matches: vec![] };
        let json = serde_json::to_string(&data).unwrap();
        let back: RulesTestData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.text_length, tl);
        prop_assert_eq!(back.match_count, mc);
    }

    #[test]
    fn rt116_rule_match_item_serde(val in arb_rule_match_item()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: RuleMatchItem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.rule_id, &val.rule_id);
        prop_assert_eq!(back.start, val.start);
    }

    #[test]
    fn rt117_rule_trace_info_serde(val in arb_rule_trace_info()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: RuleTraceInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.anchors_checked, val.anchors_checked);
        prop_assert_eq!(back.regex_matched, val.regex_matched);
    }

    #[test]
    fn rt118_rule_detail_data_serde(id in arb_short_str2()) {
        let data = RuleDetailData {
            id: id.clone(), agent_type: "claude".to_string(),
            event_type: "error".to_string(), severity: "high".to_string(),
            description: "test rule".to_string(), anchors: vec!["ERROR".to_string()],
            regex: Some("error.*".to_string()), workflow: None,
            manual_fix: None, learn_more_url: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: RuleDetailData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.id, &id);
    }

    #[test]
    fn rt119_rules_lint_data_serde(total in 0usize..100, passed in proptest::bool::ANY) {
        let data = RulesLintData {
            total_rules: total, rules_checked: total,
            errors: vec![], warnings: vec![],
            fixture_coverage: None, passed,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: RulesLintData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_rules, total);
        prop_assert_eq!(back.passed, passed);
    }

    #[test]
    fn rt120_fixture_coverage_serde(with_fix in 0usize..50, total in 0usize..100) {
        let data = FixtureCoverage {
            rules_with_fixtures: with_fix,
            rules_without_fixtures: vec!["rule-1".to_string()],
            total_fixtures: total,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: FixtureCoverage = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.rules_with_fixtures, with_fix);
        prop_assert_eq!(back.total_fixtures, total);
    }

    // --- Account types ---

    #[test]
    fn rt121_accounts_list_data_serde(service in "[a-z]{3,10}", total in 0usize..20) {
        let data = AccountsListData {
            accounts: vec![], total, service: service.clone(), pick_preview: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: AccountsListData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.service, &service);
        prop_assert_eq!(back.total, total);
    }

    #[test]
    fn rt122_account_pick_preview_serde(reason in "[a-z_ ]{5,30}", candidates in 0usize..20) {
        let data = AccountPickPreview {
            selected_account_id: Some("acc-1".to_string()),
            selected_name: Some("Test".to_string()),
            selection_reason: reason.clone(),
            threshold_percent: 20.0, candidates_count: candidates,
            filtered_count: 0, quota_advisory: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: AccountPickPreview = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.selection_reason, &reason);
        prop_assert_eq!(back.candidates_count, candidates);
    }

    #[test]
    fn rt123_account_quota_advisory_serde(avail in "[a-z]{3,10}", blocking in proptest::bool::ANY) {
        let data = AccountQuotaAdvisoryInfo {
            availability: avail.clone(), low_quota_threshold_percent: 15.0,
            selected_percent_remaining: Some(80.0), warning: None, blocking,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: AccountQuotaAdvisoryInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.availability, &avail);
        prop_assert_eq!(back.blocking, blocking);
    }

    #[test]
    fn rt124_accounts_refresh_data_serde(service in "[a-z]{3,10}", count in 0usize..10) {
        let data = AccountsRefreshData {
            service: service.clone(), refreshed_count: count,
            refreshed_at: Some("2026-01-01T00:00:00Z".to_string()), accounts: vec![],
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: AccountsRefreshData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.service, &service);
        prop_assert_eq!(back.refreshed_count, count);
    }

    // --- Reservation types ---

    #[test]
    fn rt125_reserve_data_serde(pane in 0u64..1000) {
        let data = ReserveData {
            reservation: ReservationInfo {
                id: 1, pane_id: pane, owner_kind: "agent".to_string(),
                owner_id: "agent-1".to_string(), reason: Some("testing".to_string()),
                created_at: 1_700_000_000_000, expires_at: 1_700_000_060_000,
                released_at: None, status: "active".to_string(),
            },
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: ReserveData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.reservation.pane_id, pane);
    }

    #[test]
    fn rt126_release_data_serde(rid in 0i64..10000, released in proptest::bool::ANY) {
        let data = ReleaseData { reservation_id: rid, released };
        let json = serde_json::to_string(&data).unwrap();
        let back: ReleaseData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.reservation_id, rid);
        prop_assert_eq!(back.released, released);
    }

    #[test]
    fn rt127_reservations_list_data_serde(total in 0usize..100) {
        let data = ReservationsListData { reservations: vec![], total };
        let json = serde_json::to_string(&data).unwrap();
        let back: ReservationsListData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total, total);
    }

    // --- Meta / diagnostics types ---

    #[test]
    fn rt128_why_data_serde(code in "[A-Z]{2}-[0-9]{4}") {
        let data = WhyData {
            code: code.clone(), category: "storage".to_string(),
            title: "Database Locked".to_string(),
            explanation: "Another process holds the lock.".to_string(),
            suggestions: Some(vec!["Retry later".to_string()]), see_also: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: WhyData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.code, &code);
    }

    #[test]
    fn rt129_approve_data_serde(code in "[a-z0-9]{6,10}", valid in proptest::bool::ANY) {
        let data = ApproveData {
            code: code.clone(), valid,
            created_at: Some(1_700_000_000_000), action_kind: Some("send_text".to_string()),
            pane_id: Some(1), expires_at: Some(1_700_000_060_000),
            action_fingerprint: None, dry_run: Some(false),
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: ApproveData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.code, &code);
        prop_assert_eq!(back.valid, valid);
    }

    // --- QuickStart types ---

    #[test]
    fn rt130_quick_start_data_serde(desc in "[a-z ]{5,30}") {
        let data = QuickStartData {
            description: desc.clone(),
            global_flags: vec![QuickStartGlobalFlag {
                flag: "--format".to_string(), env_var: None, description: "Output format".to_string(),
            }],
            core_loop: vec![QuickStartStep { step: 1, action: "observe".to_string(), command: "ft robot state".to_string() }],
            commands: vec![QuickStartCommand {
                name: "state".to_string(), args: "[--pane N]".to_string(),
                summary: "Get pane state".to_string(), examples: vec!["ft robot state".to_string()],
            }],
            tips: vec!["Use --format json".to_string()],
            error_handling: QuickStartErrorHandling {
                common_codes: vec![QuickStartErrorCode {
                    code: "FT-1003".to_string(), meaning: "Pane not found".to_string(),
                    recovery: "Check pane ID".to_string(),
                }],
                safety_notes: vec!["Always check ok field".to_string()],
            },
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: QuickStartData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.description, &desc);
        prop_assert_eq!(back.global_flags.len(), 1);
    }

    #[test]
    fn rt131_quick_start_global_flag_serde(flag in "[a-z-]{2,15}") {
        let data = QuickStartGlobalFlag {
            flag: flag.clone(), env_var: Some("FT_FORMAT".to_string()),
            description: "test flag".to_string(),
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: QuickStartGlobalFlag = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.flag, &flag);
    }

    #[test]
    fn rt132_quick_start_step_serde(step in 1u8..10, action in "[a-z]{3,10}") {
        let data = QuickStartStep { step, action: action.clone(), command: "ft robot test".to_string() };
        let json = serde_json::to_string(&data).unwrap();
        let back: QuickStartStep = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.step, step);
        prop_assert_eq!(&back.action, &action);
    }

    #[test]
    fn rt133_quick_start_command_serde(name in "[a-z]{3,10}") {
        let data = QuickStartCommand {
            name: name.clone(), args: "--flag".to_string(),
            summary: "Does a thing".to_string(), examples: vec!["ft robot test".to_string()],
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: QuickStartCommand = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &name);
    }

    #[test]
    fn rt134_quick_start_error_handling_serde(note in "[a-z ]{5,30}") {
        let data = QuickStartErrorHandling {
            common_codes: vec![], safety_notes: vec![note.clone()],
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: QuickStartErrorHandling = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.safety_notes[0], &note);
    }

    #[test]
    fn rt135_quick_start_error_code_serde(code in "[A-Z]{2}-[0-9]{4}") {
        let data = QuickStartErrorCode {
            code: code.clone(), meaning: "test".to_string(), recovery: "retry".to_string(),
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: QuickStartErrorCode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.code, &code);
    }
}
