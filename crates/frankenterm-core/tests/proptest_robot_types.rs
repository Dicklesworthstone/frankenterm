//! Property-based tests for robot_types module.
//!
//! Verifies invariants for:
//! - ErrorCode: parse/as_str roundtrip, from_number/number roundtrip,
//!   category consistency, is_retryable classification, Unknown variant
//! - RobotResponse: serde roundtrip, into_result semantics
//! - Data types: serde roundtrips for GetTextData, TruncationInfo,
//!   WaitForData, SearchHit, WorkflowInfo, ReservationInfo, etc.

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
}
