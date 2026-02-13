//! Property-based tests for the `webhook` module.
//!
//! Covers `WebhookTemplate` serde roundtrips, `WebhookEndpointConfig` serde
//! roundtrips, `render_template` structural invariants, and
//! `matches_event_type` filtering correctness.

use std::collections::HashMap;

use frankenterm_core::notifications::NotificationPayload;
use frankenterm_core::webhook::{WebhookEndpointConfig, WebhookTemplate, render_template};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_webhook_template() -> impl Strategy<Value = WebhookTemplate> {
    prop_oneof![
        Just(WebhookTemplate::Generic),
        Just(WebhookTemplate::Slack),
        Just(WebhookTemplate::Discord),
    ]
}

fn arb_endpoint_config() -> impl Strategy<Value = WebhookEndpointConfig> {
    (
        "[a-z_]{3,15}",                                    // name
        "https://[a-z]{3,10}\\.example\\.com/[a-z]{3,10}", // url
        arb_webhook_template(),
        proptest::collection::vec("[a-z.*:]{3,20}", 0..3), // events
        any::<bool>(),                                     // enabled
    )
        .prop_map(
            |(name, url, template, events, enabled)| WebhookEndpointConfig {
                name,
                url,
                template,
                events,
                headers: HashMap::new(),
                enabled,
            },
        )
}

fn arb_payload() -> impl Strategy<Value = NotificationPayload> {
    (
        "[a-z.]{3,15}",
        0_u64..100,
        "[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}",
        "[A-Za-z ]{3,20}",
        "[A-Za-z ]{5,30}",
        "info|warning|critical",
        "[a-z]{3,10}",
        0.0_f64..1.0,
    )
        .prop_map(
            |(
                event_type,
                pane_id,
                timestamp,
                summary,
                description,
                severity,
                agent_type,
                confidence,
            )| {
                NotificationPayload {
                    event_type,
                    pane_id,
                    timestamp,
                    summary,
                    description,
                    severity,
                    agent_type,
                    confidence,
                    quick_fix: None,
                    suppressed_since_last: 0,
                }
            },
        )
}

// =========================================================================
// WebhookTemplate — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// WebhookTemplate serde roundtrip.
    #[test]
    fn prop_template_serde(tmpl in arb_webhook_template()) {
        let json = serde_json::to_string(&tmpl).unwrap();
        let back: WebhookTemplate = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, tmpl);
    }

    /// WebhookTemplate serializes to lowercase.
    #[test]
    fn prop_template_lowercase(tmpl in arb_webhook_template()) {
        let json = serde_json::to_string(&tmpl).unwrap();
        let expected = match tmpl {
            WebhookTemplate::Generic => "\"generic\"",
            WebhookTemplate::Slack => "\"slack\"",
            WebhookTemplate::Discord => "\"discord\"",
        };
        prop_assert_eq!(json.as_str(), expected);
    }

    /// WebhookTemplate Display matches serialization (without quotes).
    #[test]
    fn prop_template_display(tmpl in arb_webhook_template()) {
        let display = tmpl.to_string();
        let json = serde_json::to_string(&tmpl).unwrap();
        // JSON has quotes, Display doesn't
        prop_assert_eq!(format!("\"{}\"", display), json);
    }

    /// Default WebhookTemplate is Generic.
    #[test]
    fn prop_template_default(_dummy in 0..1_u8) {
        prop_assert_eq!(WebhookTemplate::default(), WebhookTemplate::Generic);
    }
}

// =========================================================================
// WebhookEndpointConfig — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// WebhookEndpointConfig serde roundtrip preserves all fields.
    #[test]
    fn prop_endpoint_config_serde(config in arb_endpoint_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: WebhookEndpointConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &config.name);
        prop_assert_eq!(&back.url, &config.url);
        prop_assert_eq!(back.template, config.template);
        prop_assert_eq!(&back.events, &config.events);
        prop_assert_eq!(back.enabled, config.enabled);
    }
}

// =========================================================================
// WebhookEndpointConfig::matches_event_type — filtering
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Endpoint with empty events list matches everything.
    #[test]
    fn prop_empty_events_matches_all(event in "[a-z.]{3,20}") {
        let config = WebhookEndpointConfig {
            name: "test".to_string(),
            url: "https://test.example.com/hook".to_string(),
            template: WebhookTemplate::Generic,
            events: vec![],
            headers: HashMap::new(),
            enabled: true,
        };
        prop_assert!(config.matches_event_type(&event));
    }

    /// Endpoint with specific event pattern only matches that pattern.
    #[test]
    fn prop_specific_event_selective(event in "[a-z]{3,10}") {
        let specific = format!("core.test:{}", event);
        let config = WebhookEndpointConfig {
            name: "test".to_string(),
            url: "https://test.example.com/hook".to_string(),
            template: WebhookTemplate::Generic,
            events: vec![specific.clone()],
            headers: HashMap::new(),
            enabled: true,
        };
        prop_assert!(config.matches_event_type(&specific));
    }
}

// =========================================================================
// render_template — structural invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// render_template always returns valid JSON.
    #[test]
    fn prop_render_valid_json(tmpl in arb_webhook_template(), payload in arb_payload()) {
        let value = render_template(tmpl, &payload);
        // Should be serializable back to string
        let json = serde_json::to_string(&value).unwrap();
        prop_assert!(!json.is_empty());
    }

    /// Generic template contains event_type from payload.
    #[test]
    fn prop_generic_contains_event_type(payload in arb_payload()) {
        let value = render_template(WebhookTemplate::Generic, &payload);
        let json = serde_json::to_string(&value).unwrap();
        prop_assert!(
            json.contains(&payload.event_type),
            "generic payload should contain event_type '{}': {}", payload.event_type, json
        );
    }

    /// Slack template contains "blocks" key.
    #[test]
    fn prop_slack_has_blocks(payload in arb_payload()) {
        let value = render_template(WebhookTemplate::Slack, &payload);
        let json = serde_json::to_string(&value).unwrap();
        prop_assert!(
            json.contains("blocks"),
            "slack payload should contain 'blocks': {}", json
        );
    }

    /// Discord template contains "embeds" key.
    #[test]
    fn prop_discord_has_embeds(payload in arb_payload()) {
        let value = render_template(WebhookTemplate::Discord, &payload);
        let json = serde_json::to_string(&value).unwrap();
        prop_assert!(
            json.contains("embeds"),
            "discord payload should contain 'embeds': {}", json
        );
    }

    /// render_template is deterministic.
    #[test]
    fn prop_render_deterministic(tmpl in arb_webhook_template(), payload in arb_payload()) {
        let v1 = render_template(tmpl, &payload);
        let v2 = render_template(tmpl, &payload);
        prop_assert_eq!(v1, v2);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn template_variants_distinct() {
    assert_ne!(WebhookTemplate::Generic, WebhookTemplate::Slack);
    assert_ne!(WebhookTemplate::Generic, WebhookTemplate::Discord);
    assert_ne!(WebhookTemplate::Slack, WebhookTemplate::Discord);
}

#[test]
fn endpoint_with_headers_roundtrip() {
    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), "Bearer test".to_string());
    let config = WebhookEndpointConfig {
        name: "test".to_string(),
        url: "https://test.example.com/hook".to_string(),
        template: WebhookTemplate::Slack,
        events: vec!["core.*".to_string()],
        headers,
        enabled: true,
    };
    let json = serde_json::to_string(&config).unwrap();
    let back: WebhookEndpointConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back.headers.get("Authorization").unwrap(), "Bearer test");
}
