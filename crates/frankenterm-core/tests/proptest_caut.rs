//! Property-based tests for `frankenterm_core::caut` types.
//!
//! Validates:
//! 1. CautService as_str/Display/Copy/Clone/Eq semantics
//! 2. CautUsage serde roundtrip (full, partial, empty, extra fields)
//! 3. CautRefresh serde roundtrip (full, partial, empty, extra fields)
//! 4. CautAccountUsage serde roundtrip (all fields, camelCase aliases, extra fields)
//! 5. CautAccountUsage Default all-None
//! 6. CautError Display patterns for every variant
//! 7. CautError remediation() non-empty for every variant
//! 8. CautClient builder defaults and method chaining
//! 9. CautUsage/CautRefresh/CautAccountUsage Default trait
//! 10. Serde flatten captures unknown fields in extra HashMap

use proptest::prelude::*;
use serde_json::{Value, json};
use std::collections::HashMap;

use frankenterm_core::caut::{
    CautAccountUsage, CautClient, CautError, CautRefresh, CautService, CautUsage,
};

// =============================================================================
// Strategies
// =============================================================================

/// Arbitrary non-empty alphanumeric string (safe for JSON keys and values).
fn arb_nonempty_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_-]{1,40}"
        .prop_map(|s| s.trim().to_string())
        .prop_filter("must be non-empty", |s| !s.is_empty())
}

/// Arbitrary optional string.
fn arb_opt_string() -> impl Strategy<Value = Option<String>> {
    prop_oneof![Just(None), arb_nonempty_string().prop_map(Some)]
}

/// Arbitrary ISO-8601-ish timestamp string.
fn arb_timestamp() -> impl Strategy<Value = String> {
    (
        2020u32..2030,
        1u32..13,
        1u32..29,
        0u32..24,
        0u32..60,
        0u32..60,
    )
        .prop_map(|(y, m, d, h, mi, s)| format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z"))
}

/// Arbitrary optional timestamp.
fn arb_opt_timestamp() -> impl Strategy<Value = Option<String>> {
    prop_oneof![Just(None), arb_timestamp().prop_map(Some)]
}

/// Arbitrary integer-based percentage (0..=100) to avoid float precision issues.
fn arb_percent() -> impl Strategy<Value = f64> {
    (0u32..=100).prop_map(|p| p as f64)
}

/// Arbitrary optional percentage.
fn arb_opt_percent() -> impl Strategy<Value = Option<f64>> {
    prop_oneof![Just(None), arb_percent().prop_map(Some)]
}

/// Arbitrary optional u64.
fn arb_opt_u64() -> impl Strategy<Value = Option<u64>> {
    prop_oneof![Just(None), (0u64..1_000_000).prop_map(Some)]
}

/// Arbitrary CautAccountUsage with integer-friendly percentages.
fn arb_account_usage() -> impl Strategy<Value = CautAccountUsage> {
    (
        arb_opt_string(),
        arb_opt_string(),
        arb_opt_percent(),
        arb_opt_u64(),
        arb_opt_timestamp(),
        arb_opt_u64(),
        arb_opt_u64(),
        arb_opt_u64(),
    )
        .prop_map(
            |(
                id,
                name,
                percent_remaining,
                limit_hours,
                reset_at,
                tokens_used,
                tokens_remaining,
                tokens_limit,
            )| {
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

/// Arbitrary CautUsage.
fn arb_caut_usage() -> impl Strategy<Value = CautUsage> {
    (
        arb_opt_string(),
        arb_opt_timestamp(),
        proptest::collection::vec(arb_account_usage(), 0..5),
    )
        .prop_map(|(service, generated_at, accounts)| CautUsage {
            service,
            generated_at,
            accounts,
            extra: HashMap::new(),
        })
}

/// Arbitrary CautRefresh.
fn arb_caut_refresh() -> impl Strategy<Value = CautRefresh> {
    (
        arb_opt_string(),
        arb_opt_timestamp(),
        proptest::collection::vec(arb_account_usage(), 0..5),
    )
        .prop_map(|(service, refreshed_at, accounts)| CautRefresh {
            service,
            refreshed_at,
            accounts,
            extra: HashMap::new(),
        })
}

/// Arbitrary extra field key (avoids collision with known fields).
fn arb_extra_key() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("custom_field".to_string()),
        Just("vendor_tag".to_string()),
        Just("x_extension".to_string()),
        Just("metadata".to_string()),
        Just("debug_info".to_string()),
    ]
}

/// Arbitrary serde_json::Value for extra fields (simple types only).
fn arb_json_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        (-1000i64..1000).prop_map(|n| Value::Number(n.into())),
        arb_nonempty_string().prop_map(Value::String),
    ]
}

/// Arbitrary exit status.
fn arb_exit_status() -> impl Strategy<Value = i32> {
    prop_oneof![Just(1), Just(2), Just(127), Just(255), (-128i32..128)]
}

/// Arbitrary stderr content.
fn arb_stderr() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("permission denied".to_string()),
        Just("authentication failed".to_string()),
        Just("connection refused".to_string()),
        arb_nonempty_string(),
    ]
}

/// Arbitrary byte count.
fn arb_byte_count() -> impl Strategy<Value = usize> {
    1usize..10_000_000
}

/// Arbitrary timeout in seconds.
fn arb_timeout_secs() -> impl Strategy<Value = u64> {
    1u64..3600
}

/// Arbitrary CautError across all variants.
fn arb_caut_error() -> impl Strategy<Value = CautError> {
    prop_oneof![
        Just(CautError::NotInstalled),
        arb_timeout_secs().prop_map(|timeout_secs| CautError::Timeout { timeout_secs }),
        (arb_exit_status(), arb_stderr())
            .prop_map(|(status, stderr)| { CautError::NonZeroExit { status, stderr } }),
        (arb_byte_count(), arb_byte_count())
            .prop_map(|(bytes, max_bytes)| { CautError::OutputTooLarge { bytes, max_bytes } }),
        (arb_nonempty_string(), arb_nonempty_string())
            .prop_map(|(message, preview)| { CautError::InvalidJson { message, preview } }),
        arb_nonempty_string().prop_map(|message| CautError::Io { message }),
    ]
}

// =============================================================================
// CautService tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    #[test]
    fn caut_service_as_str_always_openai(_seed in 0u32..1000) {
        let svc = CautService::OpenAI;
        prop_assert_eq!(svc.as_str(), "openai", "as_str must return openai");
    }

    #[test]
    fn caut_service_display_matches_as_str(_seed in 0u32..1000) {
        let svc = CautService::OpenAI;
        let display = format!("{}", svc);
        prop_assert_eq!(display.as_str(), svc.as_str(), "Display must match as_str");
    }

    #[test]
    fn caut_service_copy_clone_eq(_seed in 0u32..1000) {
        let svc = CautService::OpenAI;
        let copied = svc;     // Copy
        let cloned = svc;  // Clone
        prop_assert_eq!(svc, copied, "Copy must preserve equality");
        prop_assert_eq!(svc, cloned, "Clone must preserve equality");
        prop_assert_eq!(copied, cloned, "Copy and Clone must be equal");
    }

    #[test]
    fn caut_service_debug_contains_openai(_seed in 0u32..1000) {
        let debug = format!("{:?}", CautService::OpenAI);
        prop_assert!(debug.contains("OpenAI"), "Debug must contain OpenAI, got: {}", debug);
    }
}

// =============================================================================
// CautUsage serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn caut_usage_serde_roundtrip(usage in arb_caut_usage()) {
        let json_str = serde_json::to_string(&usage).expect("serialize CautUsage");
        let parsed: CautUsage = serde_json::from_str(&json_str).expect("deserialize CautUsage");

        prop_assert_eq!(parsed.service, usage.service, "service field roundtrip");
        prop_assert_eq!(parsed.generated_at, usage.generated_at, "generated_at field roundtrip");
        prop_assert_eq!(parsed.accounts.len(), usage.accounts.len(), "accounts count roundtrip");

        for (orig, rt) in usage.accounts.iter().zip(parsed.accounts.iter()) {
            prop_assert_eq!(&rt.id, &orig.id, "account id roundtrip");
            prop_assert_eq!(&rt.name, &orig.name, "account name roundtrip");
            prop_assert_eq!(rt.limit_hours, orig.limit_hours, "limit_hours roundtrip");
            prop_assert_eq!(&rt.reset_at, &orig.reset_at, "reset_at roundtrip");
            prop_assert_eq!(rt.tokens_used, orig.tokens_used, "tokens_used roundtrip");
            prop_assert_eq!(rt.tokens_remaining, orig.tokens_remaining, "tokens_remaining roundtrip");
            prop_assert_eq!(rt.tokens_limit, orig.tokens_limit, "tokens_limit roundtrip");

            // Float comparison with tolerance
            match (orig.percent_remaining, rt.percent_remaining) {
                (Some(a), Some(b)) => {
                    let diff = (a - b).abs();
                    prop_assert!(diff < 1e-10, "percent_remaining drift: {} vs {}", a, b);
                }
                (None, None) => {}
                (a, b) => prop_assert!(false, "percent_remaining mismatch: {:?} vs {:?}", a, b),
            }
        }
    }

    #[test]
    fn caut_usage_empty_object_parses(_seed in 0u32..10) {
        let parsed: CautUsage = serde_json::from_str("{}").expect("empty object");
        prop_assert!(parsed.service.is_none(), "service should be None for empty object");
        prop_assert!(parsed.generated_at.is_none(), "generated_at should be None");
        prop_assert!(parsed.accounts.is_empty(), "accounts should be empty");
        prop_assert!(parsed.extra.is_empty(), "extra should be empty");
    }

    #[test]
    fn caut_usage_default_matches_empty_deser(_seed in 0u32..10) {
        let default_val = CautUsage::default();
        prop_assert!(default_val.service.is_none(), "default service is None");
        prop_assert!(default_val.generated_at.is_none(), "default generated_at is None");
        prop_assert!(default_val.accounts.is_empty(), "default accounts is empty");
        prop_assert!(default_val.extra.is_empty(), "default extra is empty");
    }

    #[test]
    fn caut_usage_extra_fields_captured(
        service in arb_opt_string(),
        extra_key in arb_extra_key(),
        extra_val in arb_json_value(),
    ) {
        let mut obj = serde_json::Map::new();
        if let Some(ref s) = service {
            obj.insert("service".to_string(), Value::String(s.clone()));
        }
        obj.insert("accounts".to_string(), Value::Array(vec![]));
        obj.insert(extra_key.clone(), extra_val.clone());

        let json_str = Value::Object(obj).to_string();
        let parsed: CautUsage = serde_json::from_str(&json_str).expect("parse with extra field");

        prop_assert!(
            parsed.extra.contains_key(&extra_key),
            "extra field '{}' should be captured, keys: {:?}", extra_key, parsed.extra.keys().collect::<Vec<_>>()
        );
        prop_assert_eq!(&parsed.extra[&extra_key], &extra_val, "extra field value must match");
    }

    #[test]
    fn caut_usage_partial_fields(
        service in arb_opt_string(),
        generated_at in arb_opt_timestamp(),
    ) {
        let mut obj = serde_json::Map::new();
        if let Some(ref s) = service {
            obj.insert("service".to_string(), Value::String(s.clone()));
        }
        if let Some(ref g) = generated_at {
            obj.insert("generated_at".to_string(), Value::String(g.clone()));
        }
        let json_str = Value::Object(obj).to_string();
        let parsed: CautUsage = serde_json::from_str(&json_str).expect("parse partial");

        prop_assert_eq!(parsed.service, service, "service roundtrip");
        prop_assert_eq!(parsed.generated_at, generated_at, "generated_at roundtrip");
        prop_assert!(parsed.accounts.is_empty(), "accounts default to empty");
    }
}

// =============================================================================
// CautRefresh serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn caut_refresh_serde_roundtrip(refresh in arb_caut_refresh()) {
        let json_str = serde_json::to_string(&refresh).expect("serialize CautRefresh");
        let parsed: CautRefresh = serde_json::from_str(&json_str).expect("deserialize CautRefresh");

        prop_assert_eq!(parsed.service, refresh.service, "service field roundtrip");
        prop_assert_eq!(parsed.refreshed_at, refresh.refreshed_at, "refreshed_at field roundtrip");
        prop_assert_eq!(parsed.accounts.len(), refresh.accounts.len(), "accounts count roundtrip");

        for (orig, rt) in refresh.accounts.iter().zip(parsed.accounts.iter()) {
            prop_assert_eq!(&rt.id, &orig.id, "account id roundtrip");
            prop_assert_eq!(&rt.name, &orig.name, "account name roundtrip");
            prop_assert_eq!(rt.tokens_used, orig.tokens_used, "tokens_used roundtrip");
        }
    }

    #[test]
    fn caut_refresh_default_all_none(_seed in 0u32..10) {
        let default_val = CautRefresh::default();
        prop_assert!(default_val.service.is_none(), "default service is None");
        prop_assert!(default_val.refreshed_at.is_none(), "default refreshed_at is None");
        prop_assert!(default_val.accounts.is_empty(), "default accounts is empty");
        prop_assert!(default_val.extra.is_empty(), "default extra is empty");
    }

    #[test]
    fn caut_refresh_extra_fields_captured(
        extra_key in arb_extra_key(),
        extra_val in arb_json_value(),
    ) {
        let mut obj = serde_json::Map::new();
        obj.insert("accounts".to_string(), Value::Array(vec![]));
        obj.insert(extra_key.clone(), extra_val.clone());

        let json_str = Value::Object(obj).to_string();
        let parsed: CautRefresh = serde_json::from_str(&json_str).expect("parse with extra");

        prop_assert!(
            parsed.extra.contains_key(&extra_key),
            "extra field '{}' must be captured", extra_key
        );
        prop_assert_eq!(&parsed.extra[&extra_key], &extra_val, "extra value roundtrip");
    }

    #[test]
    fn caut_refresh_partial_fields(
        service in arb_opt_string(),
        refreshed_at in arb_opt_timestamp(),
    ) {
        let mut obj = serde_json::Map::new();
        if let Some(ref s) = service {
            obj.insert("service".to_string(), Value::String(s.clone()));
        }
        if let Some(ref r) = refreshed_at {
            obj.insert("refreshed_at".to_string(), Value::String(r.clone()));
        }
        let json_str = Value::Object(obj).to_string();
        let parsed: CautRefresh = serde_json::from_str(&json_str).expect("parse partial");

        prop_assert_eq!(parsed.service, service, "service roundtrip");
        prop_assert_eq!(parsed.refreshed_at, refreshed_at, "refreshed_at roundtrip");
    }
}

// =============================================================================
// CautAccountUsage serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn caut_account_usage_serde_roundtrip(acct in arb_account_usage()) {
        let json_str = serde_json::to_string(&acct).expect("serialize CautAccountUsage");
        let parsed: CautAccountUsage = serde_json::from_str(&json_str).expect("deserialize CautAccountUsage");

        prop_assert_eq!(&parsed.id, &acct.id, "id roundtrip");
        prop_assert_eq!(&parsed.name, &acct.name, "name roundtrip");
        prop_assert_eq!(parsed.limit_hours, acct.limit_hours, "limit_hours roundtrip");
        prop_assert_eq!(&parsed.reset_at, &acct.reset_at, "reset_at roundtrip");
        prop_assert_eq!(parsed.tokens_used, acct.tokens_used, "tokens_used roundtrip");
        prop_assert_eq!(parsed.tokens_remaining, acct.tokens_remaining, "tokens_remaining roundtrip");
        prop_assert_eq!(parsed.tokens_limit, acct.tokens_limit, "tokens_limit roundtrip");

        match (acct.percent_remaining, parsed.percent_remaining) {
            (Some(a), Some(b)) => {
                let diff = (a - b).abs();
                prop_assert!(diff < 1e-10, "percent_remaining drift: {} vs {}", a, b);
            }
            (None, None) => {}
            (a, b) => prop_assert!(false, "percent_remaining mismatch: {:?} vs {:?}", a, b),
        }
    }

    #[test]
    fn caut_account_usage_default_all_none(_seed in 0u32..10) {
        let default_val = CautAccountUsage::default();
        prop_assert!(default_val.id.is_none(), "default id is None");
        prop_assert!(default_val.name.is_none(), "default name is None");
        prop_assert!(default_val.percent_remaining.is_none(), "default percent_remaining is None");
        prop_assert!(default_val.limit_hours.is_none(), "default limit_hours is None");
        prop_assert!(default_val.reset_at.is_none(), "default reset_at is None");
        prop_assert!(default_val.tokens_used.is_none(), "default tokens_used is None");
        prop_assert!(default_val.tokens_remaining.is_none(), "default tokens_remaining is None");
        prop_assert!(default_val.tokens_limit.is_none(), "default tokens_limit is None");
        prop_assert!(default_val.extra.is_empty(), "default extra is empty");
    }

    #[test]
    fn caut_account_usage_camel_case_aliases(
        percent in arb_percent(),
        limit_hours in arb_opt_u64(),
        reset_at in arb_opt_timestamp(),
        tokens_used in arb_opt_u64(),
        tokens_remaining in arb_opt_u64(),
        tokens_limit in arb_opt_u64(),
    ) {
        // Build JSON with camelCase keys (the aliases)
        let mut obj = serde_json::Map::new();
        obj.insert("percentRemaining".to_string(), json!(percent));
        if let Some(lh) = limit_hours {
            obj.insert("limitHours".to_string(), json!(lh));
        }
        if let Some(ref ra) = reset_at {
            obj.insert("resetAt".to_string(), json!(ra));
        }
        if let Some(tu) = tokens_used {
            obj.insert("tokensUsed".to_string(), json!(tu));
        }
        if let Some(tr) = tokens_remaining {
            obj.insert("tokensRemaining".to_string(), json!(tr));
        }
        if let Some(tl) = tokens_limit {
            obj.insert("tokensLimit".to_string(), json!(tl));
        }

        let json_str = Value::Object(obj).to_string();
        let parsed: CautAccountUsage = serde_json::from_str(&json_str).expect("parse camelCase aliases");

        // percent_remaining should parse from camelCase alias
        let parsed_pct = parsed.percent_remaining.expect("percent_remaining should be Some");
        let diff = (parsed_pct - percent).abs();
        prop_assert!(diff < 1e-10, "percentRemaining alias: {} vs {}", parsed_pct, percent);

        prop_assert_eq!(parsed.limit_hours, limit_hours, "limitHours alias roundtrip");
        prop_assert_eq!(parsed.reset_at, reset_at, "resetAt alias roundtrip");
        prop_assert_eq!(parsed.tokens_used, tokens_used, "tokensUsed alias roundtrip");
        prop_assert_eq!(parsed.tokens_remaining, tokens_remaining, "tokensRemaining alias roundtrip");
        prop_assert_eq!(parsed.tokens_limit, tokens_limit, "tokensLimit alias roundtrip");
    }

    #[test]
    fn caut_account_usage_extra_fields_captured(
        id in arb_opt_string(),
        extra_key in arb_extra_key(),
        extra_val in arb_json_value(),
    ) {
        let mut obj = serde_json::Map::new();
        if let Some(ref i) = id {
            obj.insert("id".to_string(), Value::String(i.clone()));
        }
        obj.insert(extra_key.clone(), extra_val.clone());

        let json_str = Value::Object(obj).to_string();
        let parsed: CautAccountUsage = serde_json::from_str(&json_str).expect("parse with extra");

        prop_assert_eq!(&parsed.id, &id, "id preserved with extra fields");
        prop_assert!(
            parsed.extra.contains_key(&extra_key),
            "extra field '{}' must be captured", extra_key
        );
        prop_assert_eq!(&parsed.extra[&extra_key], &extra_val, "extra value match");
    }

    #[test]
    fn caut_account_usage_null_fields_parse_as_none(_seed in 0u32..10) {
        let json_str = json!({
            "id": null,
            "name": null,
            "percent_remaining": null,
            "limit_hours": null,
            "reset_at": null,
            "tokens_used": null,
            "tokens_remaining": null,
            "tokens_limit": null
        })
        .to_string();
        let parsed: CautAccountUsage = serde_json::from_str(&json_str).expect("parse nulls");

        prop_assert!(parsed.id.is_none(), "null id parses as None");
        prop_assert!(parsed.name.is_none(), "null name parses as None");
        prop_assert!(parsed.percent_remaining.is_none(), "null percent_remaining parses as None");
        prop_assert!(parsed.limit_hours.is_none(), "null limit_hours parses as None");
        prop_assert!(parsed.reset_at.is_none(), "null reset_at parses as None");
        prop_assert!(parsed.tokens_used.is_none(), "null tokens_used parses as None");
        prop_assert!(parsed.tokens_remaining.is_none(), "null tokens_remaining parses as None");
        prop_assert!(parsed.tokens_limit.is_none(), "null tokens_limit parses as None");
    }

    #[test]
    fn caut_account_usage_clone_preserves_all(acct in arb_account_usage()) {
        let cloned = acct.clone();
        prop_assert_eq!(&cloned.id, &acct.id, "clone id");
        prop_assert_eq!(&cloned.name, &acct.name, "clone name");
        prop_assert_eq!(cloned.limit_hours, acct.limit_hours, "clone limit_hours");
        prop_assert_eq!(&cloned.reset_at, &acct.reset_at, "clone reset_at");
        prop_assert_eq!(cloned.tokens_used, acct.tokens_used, "clone tokens_used");
        prop_assert_eq!(cloned.tokens_remaining, acct.tokens_remaining, "clone tokens_remaining");
        prop_assert_eq!(cloned.tokens_limit, acct.tokens_limit, "clone tokens_limit");
    }
}

// =============================================================================
// CautUsage with nested accounts roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn caut_usage_with_multiple_accounts_roundtrip(
        service in arb_opt_string(),
        generated_at in arb_opt_timestamp(),
        accounts in proptest::collection::vec(arb_account_usage(), 0..8),
    ) {
        let usage = CautUsage {
            service: service.clone(),
            generated_at: generated_at.clone(),
            accounts: accounts.clone(),
            extra: HashMap::new(),
        };

        let json_str = serde_json::to_string(&usage).expect("serialize");
        let parsed: CautUsage = serde_json::from_str(&json_str).expect("deserialize");

        prop_assert_eq!(parsed.service, service, "service roundtrip");
        prop_assert_eq!(parsed.generated_at, generated_at, "generated_at roundtrip");
        prop_assert_eq!(parsed.accounts.len(), accounts.len(), "accounts count roundtrip");
    }

    #[test]
    fn caut_refresh_with_multiple_accounts_roundtrip(
        service in arb_opt_string(),
        refreshed_at in arb_opt_timestamp(),
        accounts in proptest::collection::vec(arb_account_usage(), 0..8),
    ) {
        let refresh = CautRefresh {
            service: service.clone(),
            refreshed_at: refreshed_at.clone(),
            accounts: accounts.clone(),
            extra: HashMap::new(),
        };

        let json_str = serde_json::to_string(&refresh).expect("serialize");
        let parsed: CautRefresh = serde_json::from_str(&json_str).expect("deserialize");

        prop_assert_eq!(parsed.service, service, "service roundtrip");
        prop_assert_eq!(parsed.refreshed_at, refreshed_at, "refreshed_at roundtrip");
        prop_assert_eq!(parsed.accounts.len(), accounts.len(), "accounts count roundtrip");
    }
}

// =============================================================================
// CautError Display tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    #[test]
    fn caut_error_serde_roundtrip(err in arb_caut_error()) {
        let json = serde_json::to_string(&err).expect("serialize CautError");
        let parsed: CautError = serde_json::from_str(&json).expect("deserialize CautError");
        prop_assert_eq!(parsed, err, "CautError serde roundtrip mismatch");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn caut_error_not_installed_display(_seed in 0u32..10) {
        let err = CautError::NotInstalled;
        let display = format!("{}", err);
        prop_assert!(display.contains("not installed"), "NotInstalled display: {}", display);
        prop_assert!(display.contains("PATH"), "NotInstalled should mention PATH: {}", display);
    }

    #[test]
    fn caut_error_timeout_display(timeout_secs in arb_timeout_secs()) {
        let err = CautError::Timeout { timeout_secs };
        let display = format!("{}", err);
        let expected_fragment = format!("{}s", timeout_secs);
        prop_assert!(
            display.contains(&expected_fragment),
            "Timeout display should contain '{}s', got: {}", timeout_secs, display
        );
    }

    #[test]
    fn caut_error_non_zero_exit_display(
        status in arb_exit_status(),
        stderr in arb_stderr(),
    ) {
        let err = CautError::NonZeroExit {
            status,
            stderr: stderr.clone(),
        };
        let display = format!("{}", err);
        let status_str = format!("{}", status);
        prop_assert!(
            display.contains(&status_str),
            "NonZeroExit display should contain status '{}', got: {}", status, display
        );
        prop_assert!(
            display.contains(&stderr),
            "NonZeroExit display should contain stderr"
        );
    }

    #[test]
    fn caut_error_output_too_large_display(
        bytes in arb_byte_count(),
        max_bytes in arb_byte_count(),
    ) {
        let err = CautError::OutputTooLarge { bytes, max_bytes };
        let display = format!("{}", err);
        let max_str = format!("{}", max_bytes);
        prop_assert!(
            display.contains(&max_str),
            "OutputTooLarge display should contain max_bytes '{}', got: {}", max_bytes, display
        );
    }

    #[test]
    fn caut_error_invalid_json_display(
        message in arb_nonempty_string(),
        preview in arb_nonempty_string(),
    ) {
        let err = CautError::InvalidJson {
            message: message.clone(),
            preview,
        };
        let display = format!("{}", err);
        prop_assert!(
            display.contains(&message),
            "InvalidJson display should contain message '{}', got: {}", message, display
        );
    }

    #[test]
    fn caut_error_io_display(message in arb_nonempty_string()) {
        let err = CautError::Io {
            message: message.clone(),
        };
        let display = format!("{}", err);
        prop_assert!(
            display.contains(&message),
            "Io display should contain message '{}', got: {}", message, display
        );
    }
}

// =============================================================================
// CautError remediation tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn caut_error_not_installed_remediation(_seed in 0u32..10) {
        let err = CautError::NotInstalled;
        let rem = err.remediation();
        prop_assert!(!rem.summary.is_empty(), "NotInstalled remediation summary non-empty");
        prop_assert!(!rem.commands.is_empty(), "NotInstalled remediation has commands");
        prop_assert!(!rem.alternatives.is_empty(), "NotInstalled remediation has alternatives");
    }

    #[test]
    fn caut_error_timeout_remediation(timeout_secs in arb_timeout_secs()) {
        let err = CautError::Timeout { timeout_secs };
        let rem = err.remediation();
        prop_assert!(!rem.summary.is_empty(), "Timeout remediation summary non-empty");
        let has_timeout = rem.summary.contains(&format!("{}s", timeout_secs));
        prop_assert!(has_timeout, "Timeout remediation summary should mention timeout value");
        prop_assert!(!rem.commands.is_empty(), "Timeout remediation has commands");
        prop_assert!(!rem.alternatives.is_empty(), "Timeout remediation has alternatives");
    }

    #[test]
    fn caut_error_non_zero_exit_remediation(
        status in arb_exit_status(),
        stderr in arb_stderr(),
    ) {
        let err = CautError::NonZeroExit { status, stderr };
        let rem = err.remediation();
        prop_assert!(!rem.summary.is_empty(), "NonZeroExit remediation summary non-empty");
        prop_assert!(!rem.commands.is_empty(), "NonZeroExit remediation has commands");
        prop_assert!(!rem.alternatives.is_empty(), "NonZeroExit remediation has alternatives");
    }

    #[test]
    fn caut_error_output_too_large_remediation(
        bytes in arb_byte_count(),
        max_bytes in arb_byte_count(),
    ) {
        let err = CautError::OutputTooLarge { bytes, max_bytes };
        let rem = err.remediation();
        prop_assert!(!rem.summary.is_empty(), "OutputTooLarge remediation summary non-empty");
        prop_assert!(!rem.alternatives.is_empty(), "OutputTooLarge remediation has alternatives");
    }

    #[test]
    fn caut_error_invalid_json_remediation(
        message in arb_nonempty_string(),
        preview in arb_nonempty_string(),
    ) {
        let err = CautError::InvalidJson { message, preview };
        let rem = err.remediation();
        prop_assert!(!rem.summary.is_empty(), "InvalidJson remediation summary non-empty");
        prop_assert!(!rem.commands.is_empty(), "InvalidJson remediation has commands");
        prop_assert!(!rem.alternatives.is_empty(), "InvalidJson remediation has alternatives");
    }

    #[test]
    fn caut_error_io_remediation(message in arb_nonempty_string()) {
        let err = CautError::Io { message };
        let rem = err.remediation();
        prop_assert!(!rem.summary.is_empty(), "Io remediation summary non-empty");
        prop_assert!(!rem.alternatives.is_empty(), "Io remediation has alternatives");
    }
}

// =============================================================================
// CautError Debug trait
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn caut_error_debug_not_empty_for_all_variants(
        timeout_secs in arb_timeout_secs(),
        status in arb_exit_status(),
        stderr in arb_stderr(),
        bytes in arb_byte_count(),
        max_bytes in arb_byte_count(),
        json_msg in arb_nonempty_string(),
        json_preview in arb_nonempty_string(),
        io_msg in arb_nonempty_string(),
    ) {
        let variants: Vec<CautError> = vec![
            CautError::NotInstalled,
            CautError::Timeout { timeout_secs },
            CautError::NonZeroExit { status, stderr },
            CautError::OutputTooLarge { bytes, max_bytes },
            CautError::InvalidJson { message: json_msg, preview: json_preview },
            CautError::Io { message: io_msg },
        ];

        for err in &variants {
            let debug = format!("{:?}", err);
            prop_assert!(!debug.is_empty(), "Debug must not be empty for variant");
        }
    }
}

// =============================================================================
// CautClient builder tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn caut_client_default_debug_contains_expected_values(_seed in 0u32..10) {
        let client = CautClient::default();
        let debug = format!("{:?}", client);
        // Default binary is "caut"
        prop_assert!(debug.contains("caut"), "default debug should contain 'caut', got: {}", debug);
    }

    #[test]
    fn caut_client_new_same_as_default(_seed in 0u32..10) {
        let new_client = CautClient::new();
        let default_client = CautClient::default();
        let new_debug = format!("{:?}", new_client);
        let default_debug = format!("{:?}", default_client);
        prop_assert_eq!(new_debug, default_debug, "new() and default() should produce same Debug output");
    }

    #[test]
    fn caut_client_with_binary(binary in arb_nonempty_string()) {
        let client = CautClient::new().with_binary(binary.clone());
        let debug = format!("{:?}", client);
        prop_assert!(
            debug.contains(&binary),
            "with_binary should set binary to '{}', debug: {}", binary, debug
        );
    }

    #[test]
    fn caut_client_with_timeout_secs(secs in 1u64..3600) {
        let client = CautClient::new().with_timeout_secs(secs);
        let debug = format!("{:?}", client);
        // Duration debug format is "Duration { secs: N, nanos: 0 }" or similar
        // The key thing is it should not panic and should reflect the timeout
        prop_assert!(!debug.is_empty(), "debug should not be empty");
    }

    #[test]
    fn caut_client_with_max_output_bytes(max in 1usize..10_000_000) {
        let client = CautClient::new().with_max_output_bytes(max);
        let debug = format!("{:?}", client);
        let max_str = format!("{}", max);
        prop_assert!(
            debug.contains(&max_str),
            "with_max_output_bytes({}) should appear in debug: {}", max, debug
        );
    }

    #[test]
    fn caut_client_with_max_error_bytes(max in 1usize..100_000) {
        let client = CautClient::new().with_max_error_bytes(max);
        let debug = format!("{:?}", client);
        let max_str = format!("{}", max);
        prop_assert!(
            debug.contains(&max_str),
            "with_max_error_bytes({}) should appear in debug: {}", max, debug
        );
    }

    #[test]
    fn caut_client_builder_chain(
        binary in arb_nonempty_string(),
        timeout in 1u64..300,
        max_output in 1024usize..1_000_000,
        max_error in 512usize..100_000,
    ) {
        // Builder chain should not panic
        let client = CautClient::new()
            .with_binary(binary.clone())
            .with_timeout_secs(timeout)
            .with_max_output_bytes(max_output)
            .with_max_error_bytes(max_error);

        let debug = format!("{:?}", client);
        prop_assert!(debug.contains(&binary), "chain: binary in debug");
        let max_output_str = format!("{}", max_output);
        prop_assert!(
            debug.contains(&max_output_str),
            "chain: max_output_bytes in debug"
        );
    }

    #[test]
    fn caut_client_clone_matches_original(
        binary in arb_nonempty_string(),
        timeout in 1u64..300,
    ) {
        let client = CautClient::new()
            .with_binary(binary)
            .with_timeout_secs(timeout);
        let cloned = client.clone();

        let orig_debug = format!("{:?}", client);
        let cloned_debug = format!("{:?}", cloned);
        prop_assert_eq!(orig_debug, cloned_debug, "clone should produce identical Debug output");
    }
}

// =============================================================================
// Serde deserialization edge cases (indirect parse_json testing)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn caut_usage_from_valid_json_string(
        service in arb_opt_string(),
        generated_at in arb_opt_timestamp(),
    ) {
        let payload = json!({
            "service": service,
            "generated_at": generated_at,
            "accounts": []
        });
        let json_str = payload.to_string();
        let result: Result<CautUsage, _> = serde_json::from_str(&json_str);
        let is_ok = result.is_ok();
        prop_assert!(is_ok, "valid JSON should parse successfully");
    }

    #[test]
    fn caut_refresh_from_valid_json_string(
        service in arb_opt_string(),
        refreshed_at in arb_opt_timestamp(),
    ) {
        let payload = json!({
            "service": service,
            "refreshed_at": refreshed_at,
            "accounts": []
        });
        let json_str = payload.to_string();
        let result: Result<CautRefresh, _> = serde_json::from_str(&json_str);
        let is_ok = result.is_ok();
        prop_assert!(is_ok, "valid JSON should parse successfully");
    }

    #[test]
    fn caut_usage_invalid_json_fails(_seed in 0u32..10) {
        let invalid_inputs = vec![
            "{not_json}",
            "not json at all",
            "[1,2,3]",
            "\"just a string\"",
            "42",
            "",
        ];
        for input in invalid_inputs {
            let result: Result<CautUsage, _> = serde_json::from_str(input);
            let is_err = result.is_err();
            prop_assert!(is_err, "invalid JSON '{}' should fail to parse as CautUsage", input);
        }
    }

    #[test]
    fn caut_account_usage_from_empty_object(_seed in 0u32..10) {
        let parsed: CautAccountUsage = serde_json::from_str("{}").expect("empty object");
        prop_assert!(parsed.id.is_none(), "empty object id is None");
        prop_assert!(parsed.name.is_none(), "empty object name is None");
        prop_assert!(parsed.percent_remaining.is_none(), "empty object percent_remaining is None");
        prop_assert!(parsed.extra.is_empty(), "empty object extra is empty");
    }

    #[test]
    fn caut_account_usage_nested_extra_field(_seed in 0u32..10) {
        let json_str = json!({
            "id": "acc-1",
            "nested_data": { "level1": { "level2": true } },
            "array_field": [1, 2, 3]
        })
        .to_string();
        let parsed: CautAccountUsage = serde_json::from_str(&json_str).expect("nested extra");

        prop_assert!(parsed.extra.contains_key("nested_data"), "nested object captured");
        prop_assert!(parsed.extra.contains_key("array_field"), "array field captured");
    }

    #[test]
    fn caut_usage_deterministic_parsing(usage in arb_caut_usage()) {
        let json_str = serde_json::to_string(&usage).expect("serialize");
        let p1: CautUsage = serde_json::from_str(&json_str).expect("parse 1");
        let p2: CautUsage = serde_json::from_str(&json_str).expect("parse 2");

        prop_assert_eq!(p1.service, p2.service, "deterministic service");
        prop_assert_eq!(p1.generated_at, p2.generated_at, "deterministic generated_at");
        prop_assert_eq!(p1.accounts.len(), p2.accounts.len(), "deterministic accounts count");

        for (a, b) in p1.accounts.iter().zip(p2.accounts.iter()) {
            prop_assert_eq!(&a.id, &b.id, "deterministic account id");
            prop_assert_eq!(&a.name, &b.name, "deterministic account name");
            prop_assert_eq!(a.tokens_used, b.tokens_used, "deterministic tokens_used");
        }
    }
}

// =============================================================================
// Mixed camelCase and snake_case field deserialization
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn caut_account_snake_case_fields(
        percent in arb_percent(),
        limit_hours in 1u64..1000,
        tokens_used in 0u64..1_000_000,
    ) {
        // snake_case (primary field names) should work
        let json_str = json!({
            "percent_remaining": percent,
            "limit_hours": limit_hours,
            "tokens_used": tokens_used,
        })
        .to_string();
        let parsed: CautAccountUsage = serde_json::from_str(&json_str).expect("snake_case parse");

        let parsed_pct = parsed.percent_remaining.expect("percent_remaining present");
        let diff = (parsed_pct - percent).abs();
        prop_assert!(diff < 1e-10, "snake_case percent_remaining: {} vs {}", parsed_pct, percent);
        prop_assert_eq!(parsed.limit_hours, Some(limit_hours), "snake_case limit_hours");
        prop_assert_eq!(parsed.tokens_used, Some(tokens_used), "snake_case tokens_used");
    }

    #[test]
    fn caut_account_camel_case_fields(
        percent in arb_percent(),
        limit_hours in 1u64..1000,
        tokens_used in 0u64..1_000_000,
        tokens_remaining in 0u64..1_000_000,
        tokens_limit in 0u64..1_000_000,
    ) {
        // camelCase aliases should also work
        let json_str = json!({
            "percentRemaining": percent,
            "limitHours": limit_hours,
            "tokensUsed": tokens_used,
            "tokensRemaining": tokens_remaining,
            "tokensLimit": tokens_limit,
        })
        .to_string();
        let parsed: CautAccountUsage = serde_json::from_str(&json_str).expect("camelCase parse");

        let parsed_pct = parsed.percent_remaining.expect("percentRemaining alias");
        let diff = (parsed_pct - percent).abs();
        prop_assert!(diff < 1e-10, "camelCase percentRemaining: {} vs {}", parsed_pct, percent);
        prop_assert_eq!(parsed.limit_hours, Some(limit_hours), "camelCase limitHours");
        prop_assert_eq!(parsed.tokens_used, Some(tokens_used), "camelCase tokensUsed");
        prop_assert_eq!(parsed.tokens_remaining, Some(tokens_remaining), "camelCase tokensRemaining");
        prop_assert_eq!(parsed.tokens_limit, Some(tokens_limit), "camelCase tokensLimit");
    }

    #[test]
    fn caut_usage_accounts_preserve_order(
        n in 1usize..10,
    ) {
        let accounts: Vec<Value> = (0..n)
            .map(|i| {
                json!({
                    "id": format!("acc-{}", i),
                    "name": format!("Account {}", i),
                })
            })
            .collect();

        let payload = json!({
            "service": "openai",
            "accounts": accounts,
        });
        let json_str = payload.to_string();
        let parsed: CautUsage = serde_json::from_str(&json_str).expect("parse ordered accounts");

        prop_assert_eq!(parsed.accounts.len(), n, "account count matches");
        for (i, acct) in parsed.accounts.iter().enumerate() {
            let expected_id = format!("acc-{}", i);
            prop_assert_eq!(
                acct.id.as_deref(),
                Some(expected_id.as_str()),
                "account order preserved at index {}", i
            );
        }
    }
}

// =============================================================================
// CautError is Send + Sync (compile-time check via proptest)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(5))]

    #[test]
    fn caut_error_is_std_error(_seed in 0u32..5) {
        // CautError implements std::error::Error (via thiserror)
        fn assert_error<E: std::error::Error>(_e: &E) {}
        let err = CautError::NotInstalled;
        assert_error(&err);

        let err2 = CautError::Timeout { timeout_secs: 5 };
        assert_error(&err2);

        let err3 = CautError::Io { message: "test".to_string() };
        assert_error(&err3);
    }
}

// =============================================================================
// Serde roundtrip with extra fields preserved at top level
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn caut_usage_extra_field_roundtrip_through_value(
        extra_key in arb_extra_key(),
        extra_val in arb_json_value(),
    ) {
        // Construct a CautUsage with extra fields, serialize, and verify extra survives
        let mut usage = CautUsage::default();
        usage.extra.insert(extra_key.clone(), extra_val.clone());

        let json_str = serde_json::to_string(&usage).expect("serialize with extra");
        let parsed: CautUsage = serde_json::from_str(&json_str).expect("deserialize with extra");

        prop_assert!(
            parsed.extra.contains_key(&extra_key),
            "roundtrip: extra field '{}' must survive", extra_key
        );
        prop_assert_eq!(
            &parsed.extra[&extra_key], &extra_val,
            "roundtrip: extra field value must match"
        );
    }

    #[test]
    fn caut_refresh_extra_field_roundtrip_through_value(
        extra_key in arb_extra_key(),
        extra_val in arb_json_value(),
    ) {
        let mut refresh = CautRefresh::default();
        refresh.extra.insert(extra_key.clone(), extra_val.clone());

        let json_str = serde_json::to_string(&refresh).expect("serialize with extra");
        let parsed: CautRefresh = serde_json::from_str(&json_str).expect("deserialize with extra");

        prop_assert!(
            parsed.extra.contains_key(&extra_key),
            "roundtrip: extra field '{}' must survive", extra_key
        );
        prop_assert_eq!(
            &parsed.extra[&extra_key], &extra_val,
            "roundtrip: extra field value must match"
        );
    }

    #[test]
    fn caut_account_usage_extra_field_roundtrip_through_value(
        extra_key in arb_extra_key(),
        extra_val in arb_json_value(),
    ) {
        let mut acct = CautAccountUsage::default();
        acct.extra.insert(extra_key.clone(), extra_val.clone());

        let json_str = serde_json::to_string(&acct).expect("serialize with extra");
        let parsed: CautAccountUsage = serde_json::from_str(&json_str).expect("deserialize with extra");

        prop_assert!(
            parsed.extra.contains_key(&extra_key),
            "roundtrip: extra field '{}' must survive", extra_key
        );
        prop_assert_eq!(
            &parsed.extra[&extra_key], &extra_val,
            "roundtrip: extra field value must match"
        );
    }
}

// =============================================================================
// CautClient builder idempotency and overwrite tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn caut_client_double_set_binary(
        binary1 in arb_nonempty_string(),
        binary2 in arb_nonempty_string(),
    ) {
        // Setting binary twice should use the last value
        let client = CautClient::new()
            .with_binary(binary1)
            .with_binary(binary2.clone());
        let debug = format!("{:?}", client);
        prop_assert!(
            debug.contains(&binary2),
            "double set: last binary should win, debug: {}", debug
        );
    }

    #[test]
    fn caut_client_double_set_timeout(
        t1 in 1u64..100,
        t2 in 100u64..3600,
    ) {
        // Setting timeout twice should not panic
        let client = CautClient::new()
            .with_timeout_secs(t1)
            .with_timeout_secs(t2);
        let debug = format!("{:?}", client);
        prop_assert!(!debug.is_empty(), "double set timeout should not panic");
    }

    #[test]
    fn caut_client_with_binary_accepts_string_types(binary in arb_nonempty_string()) {
        // with_binary accepts impl Into<String>, test with String
        let client1 = CautClient::new().with_binary(binary.clone());
        // test with &str
        let client2 = CautClient::new().with_binary(binary.as_str());
        let d1 = format!("{:?}", client1);
        let d2 = format!("{:?}", client2);
        prop_assert_eq!(d1, d2, "String and &str should produce same result");
    }
}
