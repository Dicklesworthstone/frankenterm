//! Edge case tests for distributed mode: token validation, readiness evaluation,
//! credential resolution, and security error codes.
//!
//! Bead: wa-1u90p.7.1
//!
//! Validates:
//! 1. Token validation with identity matching and constant-time comparison
//! 2. Token source detection (inline / env / file)
//! 3. Credential resolution from config (inline, env, file, ambiguous, missing)
//! 4. Readiness evaluation for all config branches
//! 5. Security error codes and Display impls
//! 6. Serde roundtrips for readiness types

use frankenterm_core::config::{DistributedAuthMode, DistributedConfig, DistributedTlsConfig};
use frankenterm_core::distributed::{
    DistributedCredentialError, DistributedTokenSourceKind, ReadinessReport,
    configured_token_source_kind, evaluate_readiness, resolve_expected_token, validate_token,
};

// =============================================================================
// Token validation
// =============================================================================

#[test]
fn validate_token_passes_when_mode_does_not_require_token() {
    // mTLS mode doesn't require a token
    assert!(validate_token(DistributedAuthMode::Mtls, None, None, None).is_ok());
}

#[test]
fn validate_token_rejects_missing_expected_token() {
    let err = validate_token(DistributedAuthMode::Token, None, Some("secret"), None)
        .expect_err("should fail with missing expected token");
    assert_eq!(err.to_string(), "distributed token required");
}

#[test]
fn validate_token_rejects_missing_presented_token() {
    let err = validate_token(DistributedAuthMode::Token, Some("secret"), None, None)
        .expect_err("should fail with missing presented token");
    assert_eq!(err.to_string(), "distributed token required");
}

#[test]
fn validate_token_passes_matching_simple_tokens() {
    assert!(
        validate_token(DistributedAuthMode::Token, Some("abc123"), Some("abc123"), None).is_ok()
    );
}

#[test]
fn validate_token_rejects_mismatched_simple_tokens() {
    assert!(
        validate_token(DistributedAuthMode::Token, Some("abc123"), Some("wrong"), None).is_err()
    );
}

#[test]
fn validate_token_passes_matching_identity_tokens() {
    // identity:secret format
    assert!(validate_token(
        DistributedAuthMode::Token,
        Some("agent-1:secret123"),
        Some("agent-1:secret123"),
        None,
    )
    .is_ok());
}

#[test]
fn validate_token_rejects_mismatched_identity() {
    assert!(validate_token(
        DistributedAuthMode::Token,
        Some("agent-1:secret123"),
        Some("agent-2:secret123"),
        None,
    )
    .is_err());
}

#[test]
fn validate_token_rejects_mismatched_secret_with_identity() {
    assert!(validate_token(
        DistributedAuthMode::Token,
        Some("agent-1:secret123"),
        Some("agent-1:wrong"),
        None,
    )
    .is_err());
}

#[test]
fn validate_token_identity_matching_is_case_insensitive() {
    assert!(validate_token(
        DistributedAuthMode::Token,
        Some("Agent-1:secret"),
        Some("agent-1:secret"),
        None,
    )
    .is_ok());
}

#[test]
fn validate_token_validates_client_identity_against_token_identity() {
    // Client identity must match token identity
    assert!(validate_token(
        DistributedAuthMode::Token,
        Some("agent-1:secret"),
        Some("agent-1:secret"),
        Some("agent-1"),
    )
    .is_ok());

    // Client identity doesn't match
    assert!(validate_token(
        DistributedAuthMode::Token,
        Some("agent-1:secret"),
        Some("agent-1:secret"),
        Some("agent-2"),
    )
    .is_err());
}

#[test]
fn validate_token_client_identity_case_insensitive() {
    assert!(validate_token(
        DistributedAuthMode::Token,
        Some("AGENT-1:secret"),
        Some("agent-1:secret"),
        Some("Agent-1"),
    )
    .is_ok());
}

#[test]
fn validate_token_works_with_token_and_mtls_mode() {
    assert!(validate_token(
        DistributedAuthMode::TokenAndMtls,
        Some("secret"),
        Some("secret"),
        None,
    )
    .is_ok());

    assert!(validate_token(
        DistributedAuthMode::TokenAndMtls,
        Some("secret"),
        Some("wrong"),
        None,
    )
    .is_err());
}

#[test]
fn validate_token_empty_identity_part_treated_as_no_identity() {
    // ":secret" should parse as identity=None, secret=":secret" (whole thing)
    // because the identity part is empty after trim
    assert!(validate_token(
        DistributedAuthMode::Token,
        Some(":secret"),
        Some(":secret"),
        None,
    )
    .is_ok());
}

// =============================================================================
// Token source detection
// =============================================================================

#[test]
fn configured_token_source_kind_inline() {
    let mut config = DistributedConfig::default();
    config.token = Some("abc".to_string());
    assert_eq!(
        configured_token_source_kind(&config),
        Some(DistributedTokenSourceKind::Inline)
    );
}

#[test]
fn configured_token_source_kind_env() {
    let mut config = DistributedConfig::default();
    config.token_env = Some("FT_TOKEN".to_string());
    assert_eq!(
        configured_token_source_kind(&config),
        Some(DistributedTokenSourceKind::Env)
    );
}

#[test]
fn configured_token_source_kind_file() {
    let mut config = DistributedConfig::default();
    config.token_path = Some("/path/to/token".to_string());
    assert_eq!(
        configured_token_source_kind(&config),
        Some(DistributedTokenSourceKind::File)
    );
}

#[test]
fn configured_token_source_kind_none_when_no_sources() {
    let config = DistributedConfig::default();
    assert_eq!(configured_token_source_kind(&config), None);
}

#[test]
fn configured_token_source_kind_none_when_multiple_sources() {
    let mut config = DistributedConfig::default();
    config.token = Some("inline".to_string());
    config.token_env = Some("ENV".to_string());
    assert_eq!(
        configured_token_source_kind(&config),
        None,
        "ambiguous: both inline and env set"
    );
}

#[test]
fn configured_token_source_kind_ignores_empty_strings() {
    let mut config = DistributedConfig::default();
    config.token = Some(String::new());
    config.token_env = Some("  ".to_string());
    config.token_path = Some("/valid/path".to_string());
    assert_eq!(
        configured_token_source_kind(&config),
        Some(DistributedTokenSourceKind::File),
        "empty/whitespace-only sources should be ignored"
    );
}

// =============================================================================
// Credential resolution
// =============================================================================

#[test]
fn resolve_expected_token_inline() {
    let mut config = DistributedConfig::default();
    config.auth_mode = DistributedAuthMode::Token;
    config.token = Some("my-secret".to_string());
    let tok = resolve_expected_token(&config).unwrap().unwrap();
    assert_eq!(tok, "my-secret");
}

#[test]
fn resolve_expected_token_from_file() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(file.as_file_mut(), b"file-secret").unwrap();

    let mut config = DistributedConfig::default();
    config.auth_mode = DistributedAuthMode::Token;
    config.token_path = Some(file.path().display().to_string());

    let tok = resolve_expected_token(&config).unwrap().unwrap();
    assert_eq!(tok, "file-secret");
}

#[test]
fn resolve_expected_token_trims_whitespace() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(file.as_file_mut(), b"  secret-with-space  \n").unwrap();

    let mut config = DistributedConfig::default();
    config.auth_mode = DistributedAuthMode::Token;
    config.token_path = Some(file.path().display().to_string());

    let tok = resolve_expected_token(&config).unwrap().unwrap();
    assert_eq!(tok, "secret-with-space");
}

#[test]
fn resolve_expected_token_rejects_ambiguous() {
    let mut config = DistributedConfig::default();
    config.auth_mode = DistributedAuthMode::Token;
    config.token = Some("inline".to_string());
    config.token_env = Some("ENV".to_string());

    let err = resolve_expected_token(&config).unwrap_err();
    assert!(matches!(err, DistributedCredentialError::TokenAmbiguous));
}

#[test]
fn resolve_expected_token_rejects_missing() {
    let mut config = DistributedConfig::default();
    config.auth_mode = DistributedAuthMode::Token;
    // No token sources configured

    let err = resolve_expected_token(&config).unwrap_err();
    assert!(matches!(err, DistributedCredentialError::TokenMissing));
}

#[test]
fn resolve_expected_token_rejects_empty_file() {
    let file = tempfile::NamedTempFile::new().unwrap();
    // File is empty

    let mut config = DistributedConfig::default();
    config.auth_mode = DistributedAuthMode::Token;
    config.token_path = Some(file.path().display().to_string());

    let err = resolve_expected_token(&config).unwrap_err();
    assert!(matches!(err, DistributedCredentialError::TokenEmpty));
}

#[test]
fn resolve_expected_token_rejects_missing_env_var() {
    let mut config = DistributedConfig::default();
    config.auth_mode = DistributedAuthMode::Token;
    config.token_env = Some("FT_NONEXISTENT_TOKEN_VAR_12345".to_string());

    let err = resolve_expected_token(&config).unwrap_err();
    assert!(matches!(err, DistributedCredentialError::TokenEnvMissing(_)));
}

#[test]
fn resolve_expected_token_rejects_nonexistent_file() {
    let mut config = DistributedConfig::default();
    config.auth_mode = DistributedAuthMode::Token;
    config.token_path = Some("/nonexistent/path/to/token".to_string());

    let err = resolve_expected_token(&config).unwrap_err();
    assert!(matches!(err, DistributedCredentialError::TokenFileRead { .. }));
}

#[test]
fn resolve_expected_token_skips_when_mode_does_not_require_token() {
    let mut config = DistributedConfig::default();
    config.auth_mode = DistributedAuthMode::Mtls;
    // No token configured — should return None

    let result = resolve_expected_token(&config).unwrap();
    assert!(result.is_none());
}

// =============================================================================
// Readiness evaluation
// =============================================================================

fn make_readiness_config() -> DistributedConfig {
    let mut config = DistributedConfig::default();
    config.enabled = true;
    config.bind_addr = "127.0.0.1:9090".to_string();
    config.auth_mode = DistributedAuthMode::Token;
    config.token = Some("test-secret".to_string());
    config
}

#[test]
fn readiness_disabled_config_fails() {
    let mut config = make_readiness_config();
    config.enabled = false;

    let report = evaluate_readiness(&config);
    // runtime_enabled should be false
    assert!(!report.runtime_enabled);
    // The "runtime_enabled" item should fail
    let item = report
        .items
        .iter()
        .find(|i| i.id == "security.runtime_enabled")
        .unwrap();
    assert!(!item.pass);
}

#[test]
fn readiness_loopback_bind_allows_no_tls() {
    let config = make_readiness_config();
    let report = evaluate_readiness(&config);

    let item = report
        .items
        .iter()
        .find(|i| i.id == "security.tls_for_remote")
        .unwrap();
    assert!(item.pass, "loopback bind should not require TLS");
}

#[test]
fn readiness_remote_bind_without_tls_fails() {
    let mut config = make_readiness_config();
    config.bind_addr = "0.0.0.0:9090".to_string();
    config.tls.enabled = false;
    config.allow_insecure = false;

    let report = evaluate_readiness(&config);

    let item = report
        .items
        .iter()
        .find(|i| i.id == "security.tls_for_remote")
        .unwrap();
    assert!(!item.pass, "remote bind without TLS should fail");
}

#[test]
fn readiness_remote_bind_with_allow_insecure_passes() {
    let mut config = make_readiness_config();
    config.bind_addr = "0.0.0.0:9090".to_string();
    config.tls.enabled = false;
    config.allow_insecure = true;

    let report = evaluate_readiness(&config);

    let item = report
        .items
        .iter()
        .find(|i| i.id == "security.tls_for_remote")
        .unwrap();
    assert!(item.pass, "allow_insecure should bypass TLS requirement");
}

#[test]
fn readiness_insecure_override_is_advisory() {
    let mut config = make_readiness_config();
    config.allow_insecure = true;

    let report = evaluate_readiness(&config);

    let item = report
        .items
        .iter()
        .find(|i| i.id == "security.no_insecure_override")
        .unwrap();
    assert!(!item.pass, "insecure override should be flagged");
    assert!(!item.required, "insecure override check should be advisory");
}

#[test]
fn readiness_no_insecure_override_passes() {
    let mut config = make_readiness_config();
    config.allow_insecure = false;

    let report = evaluate_readiness(&config);

    let item = report
        .items
        .iter()
        .find(|i| i.id == "security.no_insecure_override")
        .unwrap();
    assert!(item.pass);
}

#[test]
fn readiness_auth_configured_with_token() {
    let config = make_readiness_config();
    let report = evaluate_readiness(&config);

    let item = report
        .items
        .iter()
        .find(|i| i.id == "security.auth_configured")
        .unwrap();
    assert!(item.pass, "token auth with token set should pass");
}

#[test]
fn readiness_auth_configured_mtls_without_token() {
    let mut config = make_readiness_config();
    config.auth_mode = DistributedAuthMode::Mtls;
    config.token = None;

    let report = evaluate_readiness(&config);

    let item = report
        .items
        .iter()
        .find(|i| i.id == "security.auth_configured")
        .unwrap();
    assert!(item.pass, "mTLS mode should not require token credential");
}

#[test]
fn readiness_auth_not_configured_when_token_missing() {
    let mut config = make_readiness_config();
    config.auth_mode = DistributedAuthMode::Token;
    config.token = None;
    config.token_env = None;
    config.token_path = None;

    let report = evaluate_readiness(&config);

    let item = report
        .items
        .iter()
        .find(|i| i.id == "security.auth_configured")
        .unwrap();
    assert!(!item.pass, "token auth without any token should fail");
}

#[test]
fn readiness_agent_allowlist_is_advisory() {
    let config = make_readiness_config();
    let report = evaluate_readiness(&config);

    let item = report
        .items
        .iter()
        .find(|i| i.id == "security.agent_allowlist")
        .unwrap();
    assert!(!item.required, "agent allowlist should be advisory");
    assert!(
        !item.pass,
        "empty allowlist should fail advisory check"
    );
}

#[test]
fn readiness_agent_allowlist_passes_when_configured() {
    let mut config = make_readiness_config();
    config.allow_agent_ids = vec!["agent-1".to_string()];

    let report = evaluate_readiness(&config);

    let item = report
        .items
        .iter()
        .find(|i| i.id == "security.agent_allowlist")
        .unwrap();
    assert!(item.pass);
}

#[test]
fn readiness_empty_bind_addr_fails() {
    let mut config = make_readiness_config();
    config.bind_addr = String::new();

    let report = evaluate_readiness(&config);

    let item = report
        .items
        .iter()
        .find(|i| i.id == "config.bind_addr_set")
        .unwrap();
    assert!(!item.pass, "empty bind_addr should fail");
}

#[test]
fn readiness_tls_enabled_without_paths_fails() {
    let mut config = make_readiness_config();
    config.tls = DistributedTlsConfig {
        enabled: true,
        cert_path: None,
        key_path: None,
        ..DistributedTlsConfig::default()
    };

    let report = evaluate_readiness(&config);

    let item = report
        .items
        .iter()
        .find(|i| i.id == "config.tls_paths")
        .unwrap();
    assert!(!item.pass, "TLS enabled without cert/key paths should fail");
}

#[test]
fn readiness_tls_enabled_with_paths_passes() {
    let mut config = make_readiness_config();
    config.tls = DistributedTlsConfig {
        enabled: true,
        cert_path: Some("/path/to/cert.pem".to_string()),
        key_path: Some("/path/to/key.pem".to_string()),
        ..DistributedTlsConfig::default()
    };

    let report = evaluate_readiness(&config);

    let item = report
        .items
        .iter()
        .find(|i| i.id == "config.tls_paths")
        .unwrap();
    assert!(item.pass, "TLS enabled with paths should pass");
}

#[test]
fn readiness_tls_disabled_paths_not_needed() {
    let mut config = make_readiness_config();
    config.tls.enabled = false;
    config.tls.cert_path = None;
    config.tls.key_path = None;

    let report = evaluate_readiness(&config);

    let item = report
        .items
        .iter()
        .find(|i| i.id == "config.tls_paths")
        .unwrap();
    assert!(item.pass, "TLS disabled — paths not required");
}

#[test]
fn readiness_logging_always_passes() {
    let config = make_readiness_config();
    let report = evaluate_readiness(&config);

    let item = report
        .items
        .iter()
        .find(|i| i.id == "observability.logging_assumed")
        .unwrap();
    assert!(item.pass, "logging should always pass");
    assert!(item.required);
}

#[test]
fn readiness_aggregate_counts_are_correct() {
    let config = make_readiness_config();
    let report = evaluate_readiness(&config);

    // Verify aggregate counts match items
    let actual_required_pass = report.items.iter().filter(|i| i.required && i.pass).count();
    let actual_required_total = report.items.iter().filter(|i| i.required).count();
    let actual_advisory_pass = report.items.iter().filter(|i| !i.required && i.pass).count();
    let actual_advisory_total = report.items.iter().filter(|i| !i.required).count();

    assert_eq!(report.required_pass, actual_required_pass);
    assert_eq!(report.required_total, actual_required_total);
    assert_eq!(report.advisory_pass, actual_advisory_pass);
    assert_eq!(report.advisory_total, actual_advisory_total);
}

#[test]
fn readiness_ready_only_when_all_required_pass() {
    // Good config: all required items pass
    let config = make_readiness_config();
    let report = evaluate_readiness(&config);
    // Note: feature_compiled will be false without --features distributed
    // so 'ready' will be false, but the check is still valid
    assert_eq!(
        report.ready,
        report.required_pass == report.required_total,
        "ready should match required pass count"
    );
}

#[test]
fn readiness_ipv6_loopback_detected() {
    let mut config = make_readiness_config();
    config.bind_addr = "[::1]:9090".to_string();

    let report = evaluate_readiness(&config);

    let item = report
        .items
        .iter()
        .find(|i| i.id == "security.tls_for_remote")
        .unwrap();
    assert!(item.pass, "IPv6 loopback should not require TLS");
}

#[test]
fn readiness_localhost_detected_as_loopback() {
    let mut config = make_readiness_config();
    config.bind_addr = "localhost:9090".to_string();

    let report = evaluate_readiness(&config);

    let item = report
        .items
        .iter()
        .find(|i| i.id == "security.tls_for_remote")
        .unwrap();
    assert!(item.pass, "localhost should be detected as loopback");
}

// =============================================================================
// Security error codes
// =============================================================================

#[test]
fn security_error_codes_are_stable() {
    use frankenterm_core::distributed::DistributedSecurityError;

    assert_eq!(DistributedSecurityError::MissingToken.code(), "dist.auth_failed");
    assert_eq!(DistributedSecurityError::AuthFailed.code(), "dist.auth_failed");
    assert_eq!(DistributedSecurityError::ReplayDetected.code(), "dist.replay_detected");
    assert_eq!(DistributedSecurityError::SessionLimitReached.code(), "dist.session_limit");
    assert_eq!(DistributedSecurityError::ConnectionLimitReached.code(), "dist.connection_limit");
    assert_eq!(DistributedSecurityError::MessageTooLarge.code(), "dist.message_too_large");
    assert_eq!(DistributedSecurityError::RateLimited.code(), "dist.rate_limited");
    assert_eq!(DistributedSecurityError::HandshakeTimeout.code(), "dist.handshake_timeout");
    assert_eq!(DistributedSecurityError::MessageTimeout.code(), "dist.message_timeout");
}

#[test]
fn security_error_display_is_descriptive() {
    use frankenterm_core::distributed::DistributedSecurityError;

    let err = DistributedSecurityError::MissingToken;
    let msg = err.to_string();
    assert!(!msg.is_empty());
    assert!(msg.contains("token"));
}

#[test]
fn security_errors_are_clone_and_eq() {
    use frankenterm_core::distributed::DistributedSecurityError;

    let a = DistributedSecurityError::AuthFailed;
    let b = a.clone();
    assert_eq!(a, b);
    assert_ne!(a, DistributedSecurityError::RateLimited);
}

// =============================================================================
// Serde roundtrips for readiness types
// =============================================================================

#[test]
fn serde_roundtrip_readiness_report() {
    let config = make_readiness_config();
    let report = evaluate_readiness(&config);

    let json = serde_json::to_string(&report).unwrap();
    let restored: ReadinessReport = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.ready, report.ready);
    assert_eq!(restored.feature_compiled, report.feature_compiled);
    assert_eq!(restored.runtime_enabled, report.runtime_enabled);
    assert_eq!(restored.items.len(), report.items.len());
    assert_eq!(restored.required_pass, report.required_pass);
    assert_eq!(restored.required_total, report.required_total);
    assert_eq!(restored.advisory_pass, report.advisory_pass);
    assert_eq!(restored.advisory_total, report.advisory_total);
}

#[test]
fn serde_roundtrip_readiness_item_fields() {
    let config = make_readiness_config();
    let report = evaluate_readiness(&config);

    let json = serde_json::to_string(&report).unwrap();
    let restored: ReadinessReport = serde_json::from_str(&json).unwrap();

    for (orig, restored) in report.items.iter().zip(restored.items.iter()) {
        assert_eq!(orig.id, restored.id);
        assert_eq!(orig.category, restored.category);
        assert_eq!(orig.description, restored.description);
        assert_eq!(orig.pass, restored.pass);
        assert_eq!(orig.detail, restored.detail);
        assert_eq!(orig.required, restored.required);
    }
}

// =============================================================================
// Auth mode requires_token
// =============================================================================

#[test]
fn auth_mode_requires_token_semantics() {
    assert!(DistributedAuthMode::Token.requires_token());
    assert!(DistributedAuthMode::TokenAndMtls.requires_token());
    assert!(!DistributedAuthMode::Mtls.requires_token());
}

// =============================================================================
// Credential error display
// =============================================================================

#[test]
fn credential_error_display_messages() {
    let err = DistributedCredentialError::TokenMissing;
    assert!(err.to_string().contains("token required"));

    let err = DistributedCredentialError::TokenAmbiguous;
    assert!(err.to_string().contains("ambiguous"));

    let err = DistributedCredentialError::TokenEmpty;
    assert!(err.to_string().contains("empty"));

    let err = DistributedCredentialError::TokenEnvMissing("MY_VAR".to_string());
    assert!(err.to_string().contains("MY_VAR"));
}

// =============================================================================
// Edge cases in token parsing
// =============================================================================

#[test]
fn validate_token_colon_only_token() {
    // Just ":" — identity is empty, secret is empty
    // This should fail because empty secret after identity
    // The parser treats ":something" as identity=None, secret=":something"
    // And ":" as identity=None, secret=":"
    assert!(validate_token(
        DistributedAuthMode::Token,
        Some(":"),
        Some(":"),
        None,
    )
    .is_ok(), "matching colon-only tokens should pass");
}

#[test]
fn validate_token_whitespace_identity() {
    // " :secret" — identity is whitespace-only, treated as no identity
    assert!(validate_token(
        DistributedAuthMode::Token,
        Some(" :secret"),
        Some(" :secret"),
        None,
    )
    .is_ok());
}

#[test]
fn validate_token_multiple_colons() {
    // "identity:part1:part2" — identity="identity", secret="part1:part2"
    assert!(validate_token(
        DistributedAuthMode::Token,
        Some("agent:pass:extra"),
        Some("agent:pass:extra"),
        None,
    )
    .is_ok());

    // Different secret after first colon
    assert!(validate_token(
        DistributedAuthMode::Token,
        Some("agent:pass:extra"),
        Some("agent:pass:different"),
        None,
    )
    .is_err());
}
