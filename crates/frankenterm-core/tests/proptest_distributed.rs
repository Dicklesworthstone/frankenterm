//! Property-based tests for the `distributed` module.
//!
//! Covers `ReadinessItem`/`ReadinessReport` serde roundtrips,
//! `DistributedSecurityError::code()` stability, `validate_token` correctness,
//! `evaluate_readiness` aggregate invariants, and `configured_token_source_kind`.

use frankenterm_core::config::{DistributedAuthMode, DistributedConfig};
use frankenterm_core::distributed::{
    DistributedSecurityError, DistributedTokenSourceKind, ReadinessItem, ReadinessReport,
    configured_token_source_kind, evaluate_readiness, validate_token,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_readiness_item() -> impl Strategy<Value = ReadinessItem> {
    (
        "[a-z.]{3,20}",
        "[A-Za-z]{3,15}",
        "[A-Za-z ]{5,30}",
        any::<bool>(),
        "[A-Za-z ]{5,30}",
        any::<bool>(),
    )
        .prop_map(
            |(id, category, description, pass, detail, required)| ReadinessItem {
                id,
                category,
                description,
                pass,
                detail,
                required,
            },
        )
}

fn arb_security_error() -> impl Strategy<Value = DistributedSecurityError> {
    prop_oneof![
        Just(DistributedSecurityError::MissingToken),
        Just(DistributedSecurityError::AuthFailed),
        Just(DistributedSecurityError::ReplayDetected),
        Just(DistributedSecurityError::SessionLimitReached),
        Just(DistributedSecurityError::ConnectionLimitReached),
        Just(DistributedSecurityError::MessageTooLarge),
        Just(DistributedSecurityError::RateLimited),
        Just(DistributedSecurityError::HandshakeTimeout),
        Just(DistributedSecurityError::MessageTimeout),
    ]
}

// =========================================================================
// ReadinessItem — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// ReadinessItem serde roundtrip preserves all fields.
    #[test]
    fn prop_readiness_item_serde(item in arb_readiness_item()) {
        let json = serde_json::to_string(&item).unwrap();
        let back: ReadinessItem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, item);
    }

    /// ReadinessItem serde is deterministic.
    #[test]
    fn prop_readiness_item_deterministic(item in arb_readiness_item()) {
        let j1 = serde_json::to_string(&item).unwrap();
        let j2 = serde_json::to_string(&item).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// ReadinessReport — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// ReadinessReport serde roundtrip preserves fields.
    #[test]
    fn prop_readiness_report_serde(
        items in proptest::collection::vec(arb_readiness_item(), 0..5),
        ready in any::<bool>(),
        feature_compiled in any::<bool>(),
        runtime_enabled in any::<bool>(),
    ) {
        let req_pass = items.iter().filter(|i| i.required && i.pass).count();
        let req_total = items.iter().filter(|i| i.required).count();
        let adv_pass = items.iter().filter(|i| !i.required && i.pass).count();
        let adv_total = items.iter().filter(|i| !i.required).count();
        let report = ReadinessReport {
            ready,
            feature_compiled,
            runtime_enabled,
            items,
            required_pass: req_pass,
            required_total: req_total,
            advisory_pass: adv_pass,
            advisory_total: adv_total,
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: ReadinessReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.ready, report.ready);
        prop_assert_eq!(back.feature_compiled, report.feature_compiled);
        prop_assert_eq!(back.runtime_enabled, report.runtime_enabled);
        prop_assert_eq!(back.items.len(), report.items.len());
        prop_assert_eq!(back.required_pass, report.required_pass);
        prop_assert_eq!(back.required_total, report.required_total);
        prop_assert_eq!(back.advisory_pass, report.advisory_pass);
        prop_assert_eq!(back.advisory_total, report.advisory_total);
    }
}

// =========================================================================
// DistributedSecurityError — code() stability
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// code() always returns a non-empty "dist." prefixed string.
    #[test]
    fn prop_error_code_format(err in arb_security_error()) {
        let code = err.code();
        prop_assert!(!code.is_empty());
        prop_assert!(code.starts_with("dist."), "code should start with 'dist.': {}", code);
    }

    /// code() is deterministic.
    #[test]
    fn prop_error_code_deterministic(err in arb_security_error()) {
        let c1 = err.code();
        let c2 = err.code();
        prop_assert_eq!(c1, c2);
    }

    /// Display is non-empty for all error variants.
    #[test]
    fn prop_error_display_nonempty(err in arb_security_error()) {
        let display = err.to_string();
        prop_assert!(!display.is_empty());
    }

    /// MissingToken and AuthFailed share the same code (intentional).
    #[test]
    fn prop_error_code_auth_group(_dummy in 0..1_u8) {
        let missing = DistributedSecurityError::MissingToken.code();
        let failed = DistributedSecurityError::AuthFailed.code();
        prop_assert_eq!(missing, failed);
        prop_assert_eq!(missing, "dist.auth_failed");
    }

    /// Each non-auth variant has a unique code.
    #[test]
    fn prop_error_codes_distinct(_dummy in 0..1_u8) {
        let codes = [
            DistributedSecurityError::ReplayDetected.code(),
            DistributedSecurityError::SessionLimitReached.code(),
            DistributedSecurityError::ConnectionLimitReached.code(),
            DistributedSecurityError::MessageTooLarge.code(),
            DistributedSecurityError::RateLimited.code(),
            DistributedSecurityError::HandshakeTimeout.code(),
            DistributedSecurityError::MessageTimeout.code(),
        ];
        for (i, a) in codes.iter().enumerate() {
            for (j, b) in codes.iter().enumerate() {
                if i != j {
                    prop_assert_ne!(a, b);
                }
            }
        }
    }
}

// =========================================================================
// validate_token — correctness
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Matching tokens always succeed for Token mode.
    #[test]
    fn prop_matching_tokens_succeed(token in "[a-zA-Z0-9_-]{4,24}") {
        let result = validate_token(
            DistributedAuthMode::Token,
            Some(&token),
            Some(&token),
            None,
        );
        prop_assert!(result.is_ok());
    }

    /// Mismatched tokens always fail for Token mode.
    #[test]
    fn prop_mismatched_tokens_fail(
        expected in "[a-zA-Z0-9]{4,12}",
        presented in "[a-zA-Z0-9]{4,12}",
    ) {
        prop_assume!(expected != presented);
        let result = validate_token(
            DistributedAuthMode::Token,
            Some(&expected),
            Some(&presented),
            None,
        );
        prop_assert!(result.is_err());
        let err = result.unwrap_err();
        prop_assert_eq!(err, DistributedSecurityError::AuthFailed);
    }

    /// Missing presented token gives MissingToken error.
    #[test]
    fn prop_missing_presented_token(token in "[a-zA-Z0-9]{4,12}") {
        let result = validate_token(
            DistributedAuthMode::Token,
            Some(&token),
            None,
            None,
        );
        prop_assert!(result.is_err());
        prop_assert_eq!(result.unwrap_err(), DistributedSecurityError::MissingToken);
    }

    /// Error messages never contain the token secrets.
    #[test]
    fn prop_errors_dont_leak_secrets(
        expected in "[a-zA-Z0-9]{8,24}",
        presented in "[a-zA-Z0-9]{8,24}",
    ) {
        prop_assume!(expected != presented);
        let err = validate_token(
            DistributedAuthMode::Token,
            Some(&expected),
            Some(&presented),
            None,
        )
        .unwrap_err();
        let message = err.to_string();
        prop_assert!(!message.contains(&expected));
        prop_assert!(!message.contains(&presented));
    }

    /// Identity-bearing tokens with matching identity and secret succeed.
    #[test]
    fn prop_identity_tokens_match(
        identity in "[a-zA-Z0-9_-]{2,10}",
        secret in "[a-zA-Z0-9]{4,16}",
    ) {
        let token = format!("{}:{}", identity, secret);
        let result = validate_token(
            DistributedAuthMode::TokenAndMtls,
            Some(&token),
            Some(&token),
            Some(&identity),
        );
        prop_assert!(result.is_ok());
    }

    /// Mtls mode without token requirement always succeeds.
    #[test]
    fn prop_mtls_no_token_required(_dummy in 0..1_u8) {
        let result = validate_token(
            DistributedAuthMode::Mtls,
            None,
            None,
            None,
        );
        prop_assert!(result.is_ok());
    }
}

// =========================================================================
// evaluate_readiness — aggregate invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// evaluate_readiness: ready == (required_pass == required_total).
    #[test]
    fn prop_readiness_ready_consistency(enabled in any::<bool>(), has_token in any::<bool>()) {
        let mut config = DistributedConfig::default();
        config.enabled = enabled;
        if has_token {
            config.token = Some("test-secret".to_string());
        }
        let report = evaluate_readiness(&config);
        prop_assert_eq!(report.ready, report.required_pass == report.required_total);
    }

    /// evaluate_readiness: required_pass + failed == required_total.
    #[test]
    fn prop_readiness_counts_sum(enabled in any::<bool>()) {
        let mut config = DistributedConfig::default();
        config.enabled = enabled;
        config.token = Some("tok".to_string());
        let report = evaluate_readiness(&config);
        let required_fail = report.items.iter().filter(|i| i.required && !i.pass).count();
        prop_assert_eq!(report.required_pass + required_fail, report.required_total);
        let advisory_fail = report.items.iter().filter(|i| !i.required && !i.pass).count();
        prop_assert_eq!(report.advisory_pass + advisory_fail, report.advisory_total);
    }

    /// evaluate_readiness is deterministic.
    #[test]
    fn prop_readiness_deterministic(enabled in any::<bool>()) {
        let mut config = DistributedConfig::default();
        config.enabled = enabled;
        let r1 = evaluate_readiness(&config);
        let r2 = evaluate_readiness(&config);
        prop_assert_eq!(r1.ready, r2.ready);
        prop_assert_eq!(r1.items.len(), r2.items.len());
        prop_assert_eq!(r1.required_pass, r2.required_pass);
    }

    /// evaluate_readiness: runtime_enabled matches config.enabled.
    #[test]
    fn prop_readiness_runtime_matches_config(enabled in any::<bool>()) {
        let mut config = DistributedConfig::default();
        config.enabled = enabled;
        let report = evaluate_readiness(&config);
        prop_assert_eq!(report.runtime_enabled, enabled);
    }

    /// evaluate_readiness: all items have non-empty id and category.
    #[test]
    fn prop_readiness_items_nonempty_fields(enabled in any::<bool>()) {
        let mut config = DistributedConfig::default();
        config.enabled = enabled;
        let report = evaluate_readiness(&config);
        for item in &report.items {
            prop_assert!(!item.id.is_empty());
            prop_assert!(!item.category.is_empty());
            prop_assert!(!item.description.is_empty());
            prop_assert!(!item.detail.is_empty());
        }
    }
}

// =========================================================================
// configured_token_source_kind
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// Single inline token returns Some(Inline).
    #[test]
    fn prop_token_source_inline(token in "[a-z]{4,12}") {
        let mut config = DistributedConfig::default();
        config.token = Some(token);
        prop_assert_eq!(
            configured_token_source_kind(&config),
            Some(DistributedTokenSourceKind::Inline)
        );
    }

    /// Single env token returns Some(Env).
    #[test]
    fn prop_token_source_env(env_var in "[A-Z_]{4,12}") {
        let mut config = DistributedConfig::default();
        config.token_env = Some(env_var);
        prop_assert_eq!(
            configured_token_source_kind(&config),
            Some(DistributedTokenSourceKind::Env)
        );
    }

    /// Single file path returns Some(File).
    #[test]
    fn prop_token_source_file(path in "/[a-z]{3,10}/[a-z]{3,10}") {
        let mut config = DistributedConfig::default();
        config.token_path = Some(path);
        prop_assert_eq!(
            configured_token_source_kind(&config),
            Some(DistributedTokenSourceKind::File)
        );
    }

    /// No sources returns None.
    #[test]
    fn prop_token_source_none(_dummy in 0..1_u8) {
        let config = DistributedConfig::default();
        prop_assert_eq!(configured_token_source_kind(&config), None);
    }

    /// Multiple sources returns None (ambiguous).
    #[test]
    fn prop_token_source_ambiguous(
        token in "[a-z]{4,8}",
        env_var in "[A-Z_]{4,8}",
    ) {
        let mut config = DistributedConfig::default();
        config.token = Some(token);
        config.token_env = Some(env_var);
        prop_assert_eq!(configured_token_source_kind(&config), None);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn security_error_variants_exist() {
    // Verify all 9 variants compile and have stable codes
    let errors = [
        DistributedSecurityError::MissingToken,
        DistributedSecurityError::AuthFailed,
        DistributedSecurityError::ReplayDetected,
        DistributedSecurityError::SessionLimitReached,
        DistributedSecurityError::ConnectionLimitReached,
        DistributedSecurityError::MessageTooLarge,
        DistributedSecurityError::RateLimited,
        DistributedSecurityError::HandshakeTimeout,
        DistributedSecurityError::MessageTimeout,
    ];
    for err in &errors {
        assert!(!err.code().is_empty());
        assert!(!err.to_string().is_empty());
    }
}

#[test]
fn evaluate_readiness_default_not_ready() {
    let config = DistributedConfig::default();
    let report = evaluate_readiness(&config);
    assert!(!report.ready);
    assert!(!report.runtime_enabled);
}

#[test]
fn validate_token_mtls_no_token_ok() {
    assert!(validate_token(DistributedAuthMode::Mtls, None, None, None).is_ok());
}
