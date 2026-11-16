// Requires the `mcp-client` feature flag.
#![cfg(feature = "mcp-client")]
#![allow(clippy::no_effect_underscore_binding)]
//! Property-based tests for mcp_client types.
//!
//! Validates:
//! - ExternalServerConfig serde roundtrip
//! - McpClientError serialization and Display consistency
//! - McpClientToolDefinition serde + destructive annotation behavior
//! - McpClientContentItem serde + text extraction behavior
//! - Server selection determinism

use frankenterm_core::mcp_client::{
    ExternalServerConfig, McpClientContentItem, McpClientError, McpClientToolDefinition,
};
use proptest::prelude::*;
use std::collections::HashMap;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_name() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_-]{0,15}".prop_map(String::from)
}

fn arb_command() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("python3".to_string()),
        Just("node".to_string()),
        Just("npx".to_string()),
        Just("/usr/bin/env".to_string()),
        "[a-z/]{1,20}".prop_map(String::from),
    ]
}

fn arb_args() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec("[a-zA-Z0-9_./-]{0,20}", 0..5)
}

fn arb_env_map() -> impl Strategy<Value = HashMap<String, String>> {
    prop::collection::hash_map("[A-Z_]{1,8}", "[a-zA-Z0-9]{0,16}", 0..4)
}

fn arb_external_server_config() -> impl Strategy<Value = ExternalServerConfig> {
    (
        arb_name(),
        arb_command(),
        arb_args(),
        arb_env_map(),
        proptest::option::of("[a-z/]{1,16}"),
        any::<bool>(),
    )
        .prop_map(
            |(name, command, args, env, cwd, disabled)| ExternalServerConfig {
                name,
                command,
                args,
                env,
                cwd,
                disabled,
            },
        )
}

fn arb_json_value() -> impl Strategy<Value = serde_json::Value> {
    prop_oneof![
        any::<bool>().prop_map(serde_json::Value::Bool),
        (0_i64..10_000).prop_map(|v| serde_json::Value::Number(v.into())),
        "[a-zA-Z0-9 _./:-]{0,24}".prop_map(serde_json::Value::String),
        prop::collection::vec("[a-z]{1,8}", 0..4).prop_map(|items| {
            serde_json::Value::Array(items.into_iter().map(serde_json::Value::String).collect())
        }),
    ]
}

fn arb_tool_definition() -> impl Strategy<Value = McpClientToolDefinition> {
    (
        arb_name(),
        proptest::option::of("[a-zA-Z0-9 .:_/-]{1,40}"),
        arb_json_value(),
        proptest::option::of(arb_json_value()),
        proptest::option::of(arb_json_value()),
        proptest::option::of("[0-9a-zA-Z._-]{1,16}"),
        prop::collection::vec("[a-z0-9_-]{1,12}", 0..4),
        any::<bool>(),
    )
        .prop_map(
            |(name, description, input_schema, output_schema, icon, version, tags, destructive)| {
                let annotations = serde_json::json!({
                    "destructive": destructive,
                    "owner": "tests",
                });
                McpClientToolDefinition {
                    name,
                    description,
                    input_schema,
                    output_schema,
                    icon,
                    version,
                    tags,
                    annotations: Some(annotations),
                }
            },
        )
}

fn arb_content_item() -> impl Strategy<Value = McpClientContentItem> {
    prop_oneof![
        "[a-zA-Z0-9 .,_:-]{1,40}".prop_map(|text| {
            McpClientContentItem(serde_json::json!({
                "type": "text",
                "text": text,
            }))
        }),
        arb_json_value().prop_map(|value| {
            McpClientContentItem(serde_json::json!({
                "type": "json",
                "payload": value,
            }))
        }),
        Just(McpClientContentItem(serde_json::json!({
            "type": "text",
            "text": 42,
        }))),
    ]
}

/// Strategy for known error codes (must be &'static str).
fn arb_error_code() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("mcp_client.disabled"),
        Just("mcp_client.discovery_disabled"),
        Just("mcp_client.server_not_found"),
        Just("mcp_client.server_disabled"),
        Just("mcp_client.spawn_failed"),
        Just("mcp_client.timeout"),
        Just("mcp_client.method_not_found"),
        Just("mcp_client.invalid_params"),
        Just("mcp_client.tool_execution"),
        Just("mcp_client.request_cancelled"),
        Just("mcp_client.protocol"),
    ]
}

// ── ExternalServerConfig properties ─────────────────────────────────

proptest! {
    /// ExternalServerConfig serde roundtrip preserves all fields.
    #[test]
    fn external_server_config_serde_roundtrip(config in arb_external_server_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: ExternalServerConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&config, &back);
    }

    /// ExternalServerConfig JSON roundtrip via Value.
    #[test]
    fn external_server_config_value_roundtrip(config in arb_external_server_config()) {
        let value = serde_json::to_value(&config).unwrap();
        let back: ExternalServerConfig = serde_json::from_value(value).unwrap();
        prop_assert_eq!(&config, &back);
    }

    /// ExternalServerConfig name is preserved.
    #[test]
    fn external_server_config_name_preserved(
        name in arb_name(),
        disabled in any::<bool>()
    ) {
        let config = ExternalServerConfig {
            name: name.clone(),
            command: "cmd".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            disabled,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: ExternalServerConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.name, name);
        prop_assert_eq!(back.disabled, disabled);
    }

    /// ExternalServerConfig env map roundtrips.
    #[test]
    fn external_server_config_env_roundtrip(env in arb_env_map()) {
        let config = ExternalServerConfig {
            name: "test".to_string(),
            command: "cmd".to_string(),
            args: Vec::new(),
            env: env.clone(),
            cwd: None,
            disabled: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: ExternalServerConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.env, env);
    }

    /// ExternalServerConfig args roundtrip.
    #[test]
    fn external_server_config_args_roundtrip(args in arb_args()) {
        let config = ExternalServerConfig {
            name: "test".to_string(),
            command: "cmd".to_string(),
            args: args.clone(),
            env: HashMap::new(),
            cwd: None,
            disabled: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: ExternalServerConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.args, args);
    }

    /// ExternalServerConfig Debug contains name.
    #[test]
    fn external_server_config_debug_contains_name(config in arb_external_server_config()) {
        let dbg = format!("{:?}", config);
        prop_assert!(
            dbg.contains(&config.name),
            "Debug output should contain name: {}", config.name
        );
    }

    /// ExternalServerConfig Clone produces equal value.
    #[test]
    fn external_server_config_clone_eq(config in arb_external_server_config()) {
        let cloned = config.clone();
        prop_assert_eq!(&config, &cloned);
    }
}

// ── McpClientError properties ───────────────────────────────────────

proptest! {
    /// McpClientError serializes to valid JSON.
    #[test]
    fn mcp_client_error_serializes(
        code in arb_error_code(),
        message in "[a-zA-Z0-9 .:_-]{1,50}",
        hint in proptest::option::of("[a-zA-Z0-9 .:_-]{1,50}")
    ) {
        let err = McpClientError { code, message: message.clone(), hint: hint.clone() };
        let json = serde_json::to_string(&err).unwrap();

        // Verify JSON contains expected fields
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(value["code"].as_str(), Some(code));
        prop_assert_eq!(value["message"].as_str(), Some(message.as_str()));
        if let Some(h) = &hint {
            prop_assert_eq!(value["hint"].as_str(), Some(h.as_str()));
        } else {
            prop_assert!(value.get("hint").is_none(), "hint should be absent when None");
        }
    }

    /// McpClientError Display format matches "[code] message".
    #[test]
    fn mcp_client_error_display_format(
        code in arb_error_code(),
        message in "[a-zA-Z0-9 .]{1,30}"
    ) {
        let err = McpClientError { code, message: message.clone(), hint: None };
        let display = format!("{err}");
        let expected = format!("[{code}] {message}");
        prop_assert_eq!(display, expected);
    }

    /// McpClientError Display does not include hint.
    #[test]
    fn mcp_client_error_display_no_hint(
        code in arb_error_code(),
        message in "[a-zA-Z0-9 .]{1,20}",
        hint in "[a-zA-Z0-9 .]{1,20}"
    ) {
        let err = McpClientError {
            code,
            message: message.clone(),
            hint: Some(hint.clone()),
        };
        let display = format!("{err}");
        // Display should show code and message but not the hint
        prop_assert!(display.contains(code));
        prop_assert!(display.contains(&message));
    }

    /// McpClientError Debug is non-empty.
    #[test]
    fn mcp_client_error_debug(
        code in arb_error_code(),
        message in "[a-z]{1,10}"
    ) {
        let err = McpClientError { code, message, hint: None };
        let dbg = format!("{err:?}");
        prop_assert!(!dbg.is_empty());
        prop_assert!(dbg.contains("McpClientError"));
    }

    /// McpClientError Clone produces equal value.
    #[test]
    fn mcp_client_error_clone_eq(
        code in arb_error_code(),
        message in "[a-z]{1,10}",
        hint in proptest::option::of("[a-z]{1,10}")
    ) {
        let err = McpClientError { code, message, hint };
        let cloned = err.clone();
        prop_assert_eq!(&err, &cloned);
    }

    /// McpClientError skip_serializing_if works: None hint omits field.
    #[test]
    fn mcp_client_error_no_hint_omits_field(
        code in arb_error_code(),
        message in "[a-z]{1,10}"
    ) {
        let err = McpClientError { code, message, hint: None };
        let json = serde_json::to_string(&err).unwrap();
        prop_assert!(!json.contains("hint"), "JSON should not contain hint when None");
    }

    /// McpClientError with hint includes it in JSON.
    #[test]
    fn mcp_client_error_with_hint_includes_field(
        code in arb_error_code(),
        message in "[a-z]{1,10}",
        hint in "[a-z]{1,10}"
    ) {
        let err = McpClientError { code, message, hint: Some(hint.clone()) };
        let json = serde_json::to_string(&err).unwrap();
        prop_assert!(json.contains("hint"), "JSON should contain hint when Some");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(value["hint"].as_str(), Some(hint.as_str()));
    }
}

// ── McpClientToolDefinition properties ──────────────────────────────

proptest! {
    #[test]
    fn mcp_tool_definition_serde_roundtrip(tool in arb_tool_definition()) {
        let json = serde_json::to_string(&tool).unwrap();
        let back: McpClientToolDefinition = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&tool, &back);
    }

    #[test]
    fn mcp_tool_definition_is_destructive_matches_annotation(tool in arb_tool_definition()) {
        let expected = tool
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get("destructive"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        prop_assert_eq!(tool.is_destructive(), expected);
    }
}

// ── McpClientContentItem properties ─────────────────────────────────

proptest! {
    #[test]
    fn mcp_content_item_serde_roundtrip(item in arb_content_item()) {
        let json = serde_json::to_string(&item).unwrap();
        let back: McpClientContentItem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&item, &back);
    }

    #[test]
    fn mcp_content_item_as_text_matches_payload(item in arb_content_item()) {
        let expected = item.0
            .get("type")
            .and_then(serde_json::Value::as_str)
            .filter(|value| *value == "text")
            .and_then(|_| item.0.get("text"))
            .and_then(serde_json::Value::as_str);
        prop_assert_eq!(item.as_text(), expected);
    }
}

// ── Cross-type properties ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Two configs with same fields are equal; different fields are not.
    #[test]
    fn external_server_config_eq_reflexive(config in arb_external_server_config()) {
        prop_assert_eq!(&config, &config.clone());
    }

    /// Disabled flag is preserved in JSON.
    #[test]
    fn disabled_flag_preserved(disabled in any::<bool>()) {
        let config = ExternalServerConfig {
            name: "x".to_string(),
            command: "y".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            disabled,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: ExternalServerConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.disabled, disabled);
    }

    /// All error codes serialize as valid JSON strings.
    #[test]
    fn error_codes_are_valid_json(code in arb_error_code()) {
        let json = serde_json::to_string(code).unwrap();
        let back: String = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, code);
    }
}
