//! Property-based tests for the `email_notify` module.
//!
//! Covers `EmailNotifyConfig::validate()` invariants (disabled-always-ok,
//! required-field gating, email format checking, credential pairing),
//! serde roundtrips for `EmailTlsMode` and `EmailNotifyConfig`, and
//! default value correctness.

use frankenterm_core::email_notify::{EmailNotifyConfig, EmailTlsMode};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_tls_mode() -> impl Strategy<Value = EmailTlsMode> {
    prop_oneof![
        Just(EmailTlsMode::None),
        Just(EmailTlsMode::StartTls),
        Just(EmailTlsMode::Tls),
    ]
}

fn arb_valid_email() -> impl Strategy<Value = String> {
    ("[a-z]{3,10}", "[a-z]{3,10}\\.[a-z]{2,4}")
        .prop_map(|(local, domain)| format!("{local}@{domain}"))
}

fn arb_valid_config() -> impl Strategy<Value = EmailNotifyConfig> {
    (
        "[a-z]{3,15}\\.[a-z]{2,5}", // smtp_host
        1_u16..65535,               // smtp_port
        arb_tls_mode(),
        arb_valid_email(),                                  // from
        proptest::collection::vec(arb_valid_email(), 1..4), // to
        "[a-z]{3,10}",                                      // subject_prefix
        proptest::option::of("[a-z]{3,10}"),                // username
        1_u64..3600,                                        // timeout_secs
    )
        .prop_map(|(host, port, tls, from, to, prefix, username, timeout)| {
            let password = username.as_ref().map(|_| "secret123".to_string());
            EmailNotifyConfig {
                enabled: true,
                smtp_host: host,
                smtp_port: port,
                username,
                password,
                from,
                to,
                subject_prefix: prefix,
                tls,
                timeout_secs: timeout,
            }
        })
}

// =========================================================================
// validate() — disabled config is always Ok
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// A disabled config always validates successfully regardless of fields.
    #[test]
    fn prop_disabled_config_always_ok(
        host in ".*",
        port in any::<u16>(),
        from in ".*",
    ) {
        let config = EmailNotifyConfig {
            enabled: false,
            smtp_host: host,
            smtp_port: port,
            from,
            ..Default::default()
        };
        prop_assert!(config.validate().is_ok());
    }
}

// =========================================================================
// validate() — required field gating
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// An enabled config with empty smtp_host always fails validation.
    #[test]
    fn prop_empty_host_rejected(
        from in arb_valid_email(),
        to in proptest::collection::vec(arb_valid_email(), 1..3),
    ) {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: String::new(),
            smtp_port: 587,
            from,
            to,
            ..Default::default()
        };
        let err = config.validate().unwrap_err();
        prop_assert!(err.contains("smtp_host"), "error should mention smtp_host: {err}");
    }

    /// An enabled config with port == 0 always fails validation.
    #[test]
    fn prop_zero_port_rejected(
        host in "[a-z]{3,10}\\.[a-z]{2,4}",
        from in arb_valid_email(),
        to in proptest::collection::vec(arb_valid_email(), 1..3),
    ) {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: host,
            smtp_port: 0,
            from,
            to,
            ..Default::default()
        };
        let err = config.validate().unwrap_err();
        prop_assert!(err.contains("smtp_port"), "error should mention smtp_port: {err}");
    }

    /// An enabled config with empty `to` list always fails.
    #[test]
    fn prop_empty_to_rejected(
        host in "[a-z]{3,10}\\.[a-z]{2,4}",
        from in arb_valid_email(),
    ) {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: host,
            smtp_port: 587,
            from,
            to: vec![],
            ..Default::default()
        };
        let err = config.validate().unwrap_err();
        prop_assert!(err.contains("to"), "error should mention 'to': {err}");
    }

    /// An enabled config with empty `from` always fails.
    #[test]
    fn prop_empty_from_rejected(
        host in "[a-z]{3,10}\\.[a-z]{2,4}",
        to in proptest::collection::vec(arb_valid_email(), 1..3),
    ) {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: host,
            smtp_port: 587,
            from: String::new(),
            to,
            ..Default::default()
        };
        let err = config.validate().unwrap_err();
        prop_assert!(err.contains("from"), "error should mention 'from': {err}");
    }
}

// =========================================================================
// validate() — credential pairing
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// Username without password (or vice versa) fails validation.
    #[test]
    fn prop_unpaired_credentials_rejected(
        host in "[a-z]{3,10}\\.[a-z]{2,4}",
        from in arb_valid_email(),
        to in proptest::collection::vec(arb_valid_email(), 1..3),
        cred in "[a-z]{3,10}",
    ) {
        // username present, password absent
        let config_user_only = EmailNotifyConfig {
            enabled: true,
            smtp_host: host.clone(),
            smtp_port: 587,
            username: Some(cred.clone()),
            password: None,
            from: from.clone(),
            to: to.clone(),
            ..Default::default()
        };
        prop_assert!(config_user_only.validate().is_err());

        // password present, username absent
        let config_pass_only = EmailNotifyConfig {
            enabled: true,
            smtp_host: host,
            smtp_port: 587,
            username: None,
            password: Some(cred),
            from,
            to,
            ..Default::default()
        };
        prop_assert!(config_pass_only.validate().is_err());
    }

    /// Both credentials present passes (if other fields valid).
    #[test]
    fn prop_paired_credentials_ok(config in arb_valid_config()) {
        // arb_valid_config ensures username/password are either both present or both absent
        prop_assert!(config.validate().is_ok(), "valid config should pass: {:?}", config);
    }
}

// =========================================================================
// validate() — email format checking
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// A `from` address without '@' and '.' in domain fails validation.
    #[test]
    fn prop_invalid_from_email_rejected(
        host in "[a-z]{3,10}\\.[a-z]{2,4}",
        bad_from in "[a-z]{3,10}",  // no @ sign
        to in proptest::collection::vec(arb_valid_email(), 1..3),
    ) {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: host,
            smtp_port: 587,
            from: bad_from,
            to,
            ..Default::default()
        };
        prop_assert!(config.validate().is_err());
    }

    /// Valid email addresses always pass the email format check.
    #[test]
    fn prop_valid_emails_accepted(config in arb_valid_config()) {
        prop_assert!(config.validate().is_ok());
    }
}

// =========================================================================
// Serde roundtrips
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// EmailTlsMode serde roundtrip.
    #[test]
    fn prop_tls_mode_serde_roundtrip(mode in arb_tls_mode()) {
        let json = serde_json::to_string(&mode).unwrap();
        let back: EmailTlsMode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, mode);
    }

    /// EmailTlsMode serializes to snake_case.
    #[test]
    fn prop_tls_mode_snake_case(mode in arb_tls_mode()) {
        let json = serde_json::to_string(&mode).unwrap();
        let expected = match mode {
            EmailTlsMode::None => "\"none\"",
            EmailTlsMode::StartTls => "\"start_tls\"",
            EmailTlsMode::Tls => "\"tls\"",
        };
        prop_assert_eq!(json.as_str(), expected);
    }

    /// EmailNotifyConfig serde roundtrip preserves all fields.
    #[test]
    fn prop_email_config_serde_roundtrip(config in arb_valid_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: EmailNotifyConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.enabled, config.enabled);
        prop_assert_eq!(&back.smtp_host, &config.smtp_host);
        prop_assert_eq!(back.smtp_port, config.smtp_port);
        prop_assert_eq!(&back.username, &config.username);
        prop_assert_eq!(&back.password, &config.password);
        prop_assert_eq!(&back.from, &config.from);
        prop_assert_eq!(&back.to, &config.to);
        prop_assert_eq!(&back.subject_prefix, &config.subject_prefix);
        prop_assert_eq!(back.tls, config.tls);
        prop_assert_eq!(back.timeout_secs, config.timeout_secs);
    }

    /// Disabled config roundtrips correctly.
    #[test]
    fn prop_disabled_config_serde_roundtrip(
        host in "[a-z]{3,10}",
        port in 1_u16..65535,
    ) {
        let config = EmailNotifyConfig {
            enabled: false,
            smtp_host: host,
            smtp_port: port,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: EmailNotifyConfig = serde_json::from_str(&json).unwrap();
        prop_assert!(!back.enabled);
        prop_assert_eq!(back.smtp_port, config.smtp_port);
    }
}

// =========================================================================
// Default values
// =========================================================================

#[test]
fn default_config_is_disabled() {
    let config = EmailNotifyConfig::default();
    assert!(!config.enabled);
    assert!(config.validate().is_ok());
}

#[test]
fn default_tls_mode_is_starttls() {
    assert_eq!(EmailTlsMode::default(), EmailTlsMode::StartTls);
}

#[test]
fn default_config_has_expected_port() {
    let config = EmailNotifyConfig::default();
    assert_eq!(config.smtp_port, 587);
}

#[test]
fn default_config_has_subject_prefix() {
    let config = EmailNotifyConfig::default();
    assert_eq!(config.subject_prefix, "[wa]");
}

#[test]
fn tls_modes_are_distinct() {
    let modes = [
        EmailTlsMode::None,
        EmailTlsMode::StartTls,
        EmailTlsMode::Tls,
    ];
    for (i, a) in modes.iter().enumerate() {
        for (j, b) in modes.iter().enumerate() {
            if i != j {
                assert_ne!(a, b);
            }
        }
    }
}
