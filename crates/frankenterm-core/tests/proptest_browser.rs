// Property-based tests for the browser automation module.
//
// Covers: serde roundtrips and behavioral invariants for BrowserConfig,
// ProfileMetadata, BootstrapMethod, BootstrapConfig, BootstrapResult,
// GoogleAuthConfig, GooglePageSelectors, AnthropicAuthConfig,
// AnthropicPageSelectors, OpenAiDeviceAuthConfig, DevicePageSelectors,
// AuthFlowResult, AuthFlowFailureKind, ArtifactKind.
#![allow(clippy::ignored_unit_patterns)]

use std::path::PathBuf;

use proptest::prelude::*;

use frankenterm_core::browser::{
    BootstrapMethod, BrowserConfig, ProfileMetadata,
    anthropic_auth::{AnthropicAuthConfig, AnthropicPageSelectors},
    bootstrap::{BootstrapConfig, BootstrapResult},
    google_auth::{GoogleAuthConfig, GooglePageSelectors},
    openai_device::{
        ArtifactKind, AuthFlowFailureKind, AuthFlowResult, DevicePageSelectors,
        OpenAiDeviceAuthConfig,
    },
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_]{1,20}"
}

fn arb_url() -> impl Strategy<Value = String> {
    "https://[a-z]{3,15}\\.[a-z]{2,5}/[a-z]{1,10}"
}

fn arb_selector() -> impl Strategy<Value = String> {
    "[a-z.#\\[\\]='_]{5,30}"
}

fn arb_browser_config() -> impl Strategy<Value = BrowserConfig> {
    (
        any::<bool>(),
        1000u64..120_000,
        1000u64..120_000,
        arb_string(),
    )
        .prop_map(
            |(headless, navigation_timeout_ms, page_load_timeout_ms, browser_type)| BrowserConfig {
                headless,
                navigation_timeout_ms,
                page_load_timeout_ms,
                browser_type,
            },
        )
}

fn arb_bootstrap_method() -> impl Strategy<Value = BootstrapMethod> {
    prop_oneof![
        Just(BootstrapMethod::Interactive),
        Just(BootstrapMethod::Automated),
    ]
}

fn arb_profile_metadata() -> impl Strategy<Value = ProfileMetadata> {
    (
        arb_string(),
        arb_string(),
        prop::option::of("[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z"),
        prop::option::of(arb_bootstrap_method()),
        prop::option::of("[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z"),
        0u64..10_000,
    )
        .prop_map(
            |(
                service,
                account,
                bootstrapped_at,
                bootstrap_method,
                last_used_at,
                automated_use_count,
            )| {
                ProfileMetadata {
                    service,
                    account,
                    bootstrapped_at,
                    bootstrap_method,
                    last_used_at,
                    automated_use_count,
                }
            },
        )
}

fn arb_bootstrap_config() -> impl Strategy<Value = BootstrapConfig> {
    (
        arb_url(),
        1000u64..600_000,
        500u64..10_000,
        prop::collection::vec(arb_url(), 0..3),
        prop::collection::vec("[a-z ]{5,20}", 0..3),
    )
        .prop_map(
            |(
                login_url,
                timeout_ms,
                poll_interval_ms,
                success_url_prefixes,
                success_text_markers,
            )| {
                BootstrapConfig {
                    login_url,
                    timeout_ms,
                    poll_interval_ms,
                    success_url_prefixes,
                    success_text_markers,
                }
            },
        )
}

fn arb_bootstrap_result() -> impl Strategy<Value = BootstrapResult> {
    prop_oneof![
        (0u64..600_000, "[a-z/]{5,20}".prop_map(PathBuf::from)).prop_map(
            |(elapsed_ms, profile_dir)| BootstrapResult::Success {
                elapsed_ms,
                profile_dir,
            }
        ),
        (0u64..600_000).prop_map(|waited_ms| BootstrapResult::Timeout { waited_ms }),
        "[a-z ]{5,30}".prop_map(|reason| BootstrapResult::Cancelled { reason }),
        "[a-z ]{5,30}".prop_map(|error| BootstrapResult::Failed { error }),
    ]
}

fn arb_auth_flow_failure_kind() -> impl Strategy<Value = AuthFlowFailureKind> {
    prop_oneof![
        Just(AuthFlowFailureKind::InvalidUserCode),
        Just(AuthFlowFailureKind::BrowserNotReady),
        Just(AuthFlowFailureKind::NavigationFailed),
        Just(AuthFlowFailureKind::SelectorMismatch),
        Just(AuthFlowFailureKind::BotDetected),
        Just(AuthFlowFailureKind::VerificationFailed),
        Just(AuthFlowFailureKind::PlaywrightError),
        Just(AuthFlowFailureKind::Unknown),
    ]
}

fn arb_auth_flow_result() -> impl Strategy<Value = AuthFlowResult> {
    prop_oneof![
        (0u64..600_000).prop_map(|elapsed_ms| AuthFlowResult::Success { elapsed_ms }),
        (
            "[a-z ]{5,30}",
            prop::option::of("[a-z/]{5,20}".prop_map(PathBuf::from)),
        )
            .prop_map(|(reason, artifacts_dir)| {
                AuthFlowResult::InteractiveBootstrapRequired {
                    reason,
                    artifacts_dir,
                }
            }),
        (
            "[a-z ]{5,30}",
            arb_auth_flow_failure_kind(),
            prop::option::of("[a-z/]{5,20}".prop_map(PathBuf::from)),
        )
            .prop_map(|(error, kind, artifacts_dir)| AuthFlowResult::Failed {
                error,
                kind,
                artifacts_dir,
            }),
    ]
}

fn arb_artifact_kind() -> impl Strategy<Value = ArtifactKind> {
    prop_oneof![
        Just(ArtifactKind::Screenshot),
        Just(ArtifactKind::RedactedDom),
        Just(ArtifactKind::FailureReport),
    ]
}

fn arb_google_page_selectors() -> impl Strategy<Value = GooglePageSelectors> {
    (
        arb_selector(),
        arb_selector(),
        arb_selector(),
        arb_selector(),
        arb_selector(),
        arb_selector(),
        arb_selector(),
        arb_selector(),
    )
        .prop_map(
            |(
                signed_in_marker,
                email_input,
                email_next,
                password_prompt,
                mfa_indicator,
                security_key_indicator,
                sso_indicator,
                verify_indicator,
            )| GooglePageSelectors {
                signed_in_marker,
                email_input,
                email_next,
                password_prompt,
                mfa_indicator,
                security_key_indicator,
                sso_indicator,
                verify_indicator,
            },
        )
}

fn arb_google_auth_config() -> impl Strategy<Value = GoogleAuthConfig> {
    (arb_url(), 1000u64..120_000, arb_google_page_selectors()).prop_map(
        |(auth_url, flow_timeout_ms, selectors)| GoogleAuthConfig {
            auth_url,
            flow_timeout_ms,
            selectors,
        },
    )
}

fn arb_device_page_selectors() -> impl Strategy<Value = DevicePageSelectors> {
    (
        arb_selector(),
        arb_selector(),
        arb_selector(),
        arb_selector(),
        arb_selector(),
        arb_selector(),
        arb_selector(),
    )
        .prop_map(
            |(
                code_input,
                submit_button,
                email_prompt,
                email_input,
                email_submit,
                password_prompt,
                success_marker,
            )| DevicePageSelectors {
                code_input,
                submit_button,
                email_prompt,
                email_input,
                email_submit,
                password_prompt,
                success_marker,
            },
        )
}

fn arb_openai_device_auth_config() -> impl Strategy<Value = OpenAiDeviceAuthConfig> {
    (arb_url(), 1000u64..120_000, arb_device_page_selectors()).prop_map(
        |(device_url, flow_timeout_ms, selectors)| OpenAiDeviceAuthConfig {
            device_url,
            flow_timeout_ms,
            selectors,
        },
    )
}

fn arb_anthropic_page_selectors() -> impl Strategy<Value = AnthropicPageSelectors> {
    (
        arb_selector(),
        arb_selector(),
        arb_selector(),
        arb_selector(),
        arb_selector(),
        arb_selector(),
    )
        .prop_map(
            |(
                logged_in_marker,
                email_input,
                email_submit,
                password_prompt,
                sso_indicator,
                captcha_indicator,
            )| AnthropicPageSelectors {
                logged_in_marker,
                email_input,
                email_submit,
                password_prompt,
                sso_indicator,
                captcha_indicator,
            },
        )
}

fn arb_anthropic_auth_config() -> impl Strategy<Value = AnthropicAuthConfig> {
    (arb_url(), 1000u64..120_000, arb_anthropic_page_selectors()).prop_map(
        |(login_url, flow_timeout_ms, selectors)| AnthropicAuthConfig {
            login_url,
            flow_timeout_ms,
            selectors,
        },
    )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn browser_config_serde_roundtrip(val in arb_browser_config()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: BrowserConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.headless, back.headless);
        prop_assert_eq!(val.navigation_timeout_ms, back.navigation_timeout_ms);
        prop_assert_eq!(val.page_load_timeout_ms, back.page_load_timeout_ms);
        prop_assert_eq!(val.browser_type, back.browser_type);
    }

    #[test]
    fn bootstrap_method_serde_roundtrip(val in arb_bootstrap_method()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: BootstrapMethod = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &back);
    }

    #[test]
    fn profile_metadata_serde_roundtrip(val in arb_profile_metadata()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: ProfileMetadata = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val.service, &back.service);
        prop_assert_eq!(&val.account, &back.account);
        prop_assert_eq!(val.automated_use_count, back.automated_use_count);
        prop_assert_eq!(&val.bootstrap_method, &back.bootstrap_method);
    }

    #[test]
    fn bootstrap_config_serde_roundtrip(val in arb_bootstrap_config()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: BootstrapConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val.login_url, &back.login_url);
        prop_assert_eq!(val.timeout_ms, back.timeout_ms);
        prop_assert_eq!(val.poll_interval_ms, back.poll_interval_ms);
        prop_assert_eq!(val.success_url_prefixes, back.success_url_prefixes);
        prop_assert_eq!(val.success_text_markers, back.success_text_markers);
    }

    #[test]
    fn bootstrap_result_serde_roundtrip(val in arb_bootstrap_result()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: BootstrapResult = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    #[test]
    fn auth_flow_failure_kind_serde_roundtrip(val in arb_auth_flow_failure_kind()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: AuthFlowFailureKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &back);
    }

    #[test]
    fn auth_flow_result_serde_roundtrip(val in arb_auth_flow_result()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: AuthFlowResult = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    #[test]
    fn artifact_kind_serde_roundtrip(val in arb_artifact_kind()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: ArtifactKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &back);
    }

    #[test]
    fn google_auth_config_serde_roundtrip(val in arb_google_auth_config()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: GoogleAuthConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val.auth_url, &back.auth_url);
        prop_assert_eq!(val.flow_timeout_ms, back.flow_timeout_ms);
        prop_assert_eq!(&val.selectors.email_input, &back.selectors.email_input);
    }

    #[test]
    fn google_page_selectors_serde_roundtrip(val in arb_google_page_selectors()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: GooglePageSelectors = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val.signed_in_marker, &back.signed_in_marker);
        prop_assert_eq!(&val.email_input, &back.email_input);
        prop_assert_eq!(&val.password_prompt, &back.password_prompt);
    }

    #[test]
    fn openai_device_auth_config_serde_roundtrip(val in arb_openai_device_auth_config()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: OpenAiDeviceAuthConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val.device_url, &back.device_url);
        prop_assert_eq!(val.flow_timeout_ms, back.flow_timeout_ms);
        prop_assert_eq!(&val.selectors.code_input, &back.selectors.code_input);
    }

    #[test]
    fn device_page_selectors_serde_roundtrip(val in arb_device_page_selectors()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: DevicePageSelectors = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val.code_input, &back.code_input);
        prop_assert_eq!(&val.submit_button, &back.submit_button);
        prop_assert_eq!(&val.success_marker, &back.success_marker);
    }

    #[test]
    fn anthropic_auth_config_serde_roundtrip(val in arb_anthropic_auth_config()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: AnthropicAuthConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val.login_url, &back.login_url);
        prop_assert_eq!(val.flow_timeout_ms, back.flow_timeout_ms);
        prop_assert_eq!(&val.selectors.email_input, &back.selectors.email_input);
    }

    #[test]
    fn anthropic_page_selectors_serde_roundtrip(val in arb_anthropic_page_selectors()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: AnthropicPageSelectors = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val.logged_in_marker, &back.logged_in_marker);
        prop_assert_eq!(&val.email_input, &back.email_input);
        prop_assert_eq!(&val.password_prompt, &back.password_prompt);
    }
}

// =============================================================================
// Behavioral invariant tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn browser_config_default_not_headless(_dummy in 0u8..1) {
        let cfg = BrowserConfig::default();
        prop_assert!(!cfg.headless);
        prop_assert_eq!(cfg.browser_type, "chromium");
        prop_assert!(cfg.navigation_timeout_ms > 0);
        prop_assert!(cfg.page_load_timeout_ms > 0);
    }

    #[test]
    fn browser_config_deserializes_from_empty_json(_dummy in 0u8..1) {
        let cfg: BrowserConfig = serde_json::from_str("{}").unwrap();
        let default = BrowserConfig::default();
        prop_assert_eq!(cfg.headless, default.headless);
        prop_assert_eq!(cfg.navigation_timeout_ms, default.navigation_timeout_ms);
    }

    #[test]
    fn bootstrap_method_interactive_serializes_correctly(_dummy in 0u8..1) {
        let json = serde_json::to_string(&BootstrapMethod::Interactive).unwrap();
        prop_assert_eq!(json, "\"interactive\"");
    }

    #[test]
    fn bootstrap_method_automated_serializes_correctly(_dummy in 0u8..1) {
        let json = serde_json::to_string(&BootstrapMethod::Automated).unwrap();
        prop_assert_eq!(json, "\"automated\"");
    }

    #[test]
    fn profile_metadata_new_has_zero_count(
        service in arb_string(),
        account in arb_string(),
    ) {
        let meta = ProfileMetadata::new(&service, &account);
        prop_assert_eq!(&meta.service, &service);
        prop_assert_eq!(&meta.account, &account);
        prop_assert_eq!(meta.automated_use_count, 0);
        prop_assert!(meta.bootstrapped_at.is_none());
        prop_assert!(meta.bootstrap_method.is_none());
        prop_assert!(meta.last_used_at.is_none());
    }

    #[test]
    fn profile_metadata_record_use_increments(count in 1u32..20) {
        let mut meta = ProfileMetadata::new("test", "acct");
        for _ in 0..count {
            meta.record_use();
        }
        prop_assert_eq!(meta.automated_use_count, count as u64);
        prop_assert!(meta.last_used_at.is_some());
    }

    #[test]
    fn profile_metadata_record_bootstrap_sets_fields(method in arb_bootstrap_method()) {
        let mut meta = ProfileMetadata::new("test", "acct");
        meta.record_bootstrap(method.clone());
        prop_assert!(meta.bootstrapped_at.is_some());
        prop_assert_eq!(meta.bootstrap_method, Some(method));
        prop_assert!(meta.last_used_at.is_some());
    }

    #[test]
    fn profile_metadata_skip_none_serialization(
        service in arb_string(),
        account in arb_string(),
    ) {
        let meta = ProfileMetadata::new(&service, &account);
        let json = serde_json::to_string(&meta).unwrap();
        prop_assert!(!json.contains("bootstrapped_at"));
        prop_assert!(!json.contains("bootstrap_method"));
        prop_assert!(!json.contains("last_used_at"));
    }

    #[test]
    fn bootstrap_config_default_has_url(_dummy in 0u8..1) {
        let cfg = BootstrapConfig::default();
        prop_assert!(!cfg.login_url.is_empty());
        prop_assert!(cfg.timeout_ms > 0);
        prop_assert!(cfg.poll_interval_ms > 0);
        prop_assert!(!cfg.success_url_prefixes.is_empty());
        prop_assert!(!cfg.success_text_markers.is_empty());
    }

    #[test]
    fn bootstrap_result_status_tag(val in arb_bootstrap_result()) {
        let json = serde_json::to_string(&val).unwrap();
        prop_assert!(json.contains("\"status\":"));
        match &val {
            BootstrapResult::Success { .. } => prop_assert!(json.contains("\"status\":\"success\"")),
            BootstrapResult::Timeout { .. } => prop_assert!(json.contains("\"status\":\"timeout\"")),
            BootstrapResult::Cancelled { .. } => prop_assert!(json.contains("\"status\":\"cancelled\"")),
            BootstrapResult::Failed { .. } => prop_assert!(json.contains("\"status\":\"failed\"")),
        }
    }

    #[test]
    fn auth_flow_result_status_tag(val in arb_auth_flow_result()) {
        let json = serde_json::to_string(&val).unwrap();
        prop_assert!(json.contains("\"status\":"));
        match &val {
            AuthFlowResult::Success { .. } => prop_assert!(json.contains("\"status\":\"success\"")),
            AuthFlowResult::InteractiveBootstrapRequired { .. } => {
                prop_assert!(json.contains("\"status\":\"interactive_required\""));
            }
            AuthFlowResult::Failed { .. } => prop_assert!(json.contains("\"status\":\"failed\"")),
        }
    }

    #[test]
    fn google_auth_config_default_has_url(_dummy in 0u8..1) {
        let cfg = GoogleAuthConfig::default();
        prop_assert!(cfg.auth_url.contains("google.com"));
        prop_assert!(cfg.flow_timeout_ms > 0);
    }

    #[test]
    fn openai_device_auth_config_default_has_url(_dummy in 0u8..1) {
        let cfg = OpenAiDeviceAuthConfig::default();
        prop_assert!(cfg.device_url.contains("openai.com"));
        prop_assert!(cfg.flow_timeout_ms > 0);
    }

    #[test]
    fn anthropic_auth_config_default_has_url(_dummy in 0u8..1) {
        let cfg = AnthropicAuthConfig::default();
        prop_assert!(cfg.login_url.contains("anthropic.com"));
        prop_assert!(cfg.flow_timeout_ms > 0);
    }

    #[test]
    fn auth_flow_failure_kind_all_variants_serialize(val in arb_auth_flow_failure_kind()) {
        let json = serde_json::to_string(&val).unwrap();
        prop_assert!(!json.is_empty());
        // All variants should deserialize back
        let _back: AuthFlowFailureKind = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn artifact_kind_all_variants_serialize(val in arb_artifact_kind()) {
        let json = serde_json::to_string(&val).unwrap();
        prop_assert!(!json.is_empty());
        let _back: ArtifactKind = serde_json::from_str(&json).unwrap();
    }
}
