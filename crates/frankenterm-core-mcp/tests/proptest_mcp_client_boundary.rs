use frankenterm_core_mcp::{
    ExternalServerConfig, McpClientConfig, McpClientContentItem, McpClientError,
    McpClientToolDefinition,
};
use proptest::prelude::*;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

fn small_text() -> impl Strategy<Value = String> {
    "[A-Za-z0-9_.:/ -]{0,32}"
}

fn non_empty_name() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_.:-]{0,23}"
}

fn unique_names() -> impl Strategy<Value = Vec<String>> {
    prop::collection::hash_set(non_empty_name(), 0..=8)
        .prop_map(|names| names.into_iter().collect())
}

fn env_map() -> impl Strategy<Value = HashMap<String, String>> {
    prop::collection::hash_map(non_empty_name(), small_text(), 0..=8)
}

fn valid_client_config() -> impl Strategy<Value = McpClientConfig> {
    (
        any::<bool>(),
        any::<bool>(),
        unique_names(),
        unique_names(),
        1_u64..=120_000,
        any::<u32>(),
        1_u64..=10_000,
        non_empty_name(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(
                discovery_enabled,
                include_default_paths,
                discovery_paths,
                preferred_servers,
                timeout_ms,
                max_retries,
                retry_delay_ms,
                proxy_prefix,
                proxy_mount_all_discovered,
                proxy_strict,
                proxy_allow_mutating_tools,
            )| {
                let proxy_servers = if proxy_mount_all_discovered {
                    Vec::new()
                } else if preferred_servers.is_empty() {
                    vec!["default".to_string()]
                } else {
                    preferred_servers.clone()
                };

                McpClientConfig {
                    enabled: true,
                    discovery_enabled,
                    include_default_paths,
                    discovery_paths,
                    preferred_servers,
                    timeout_ms,
                    max_retries,
                    retry_delay_ms,
                    proxy_enabled: true,
                    proxy_prefix,
                    proxy_mount_all_discovered,
                    proxy_servers,
                    proxy_strict,
                    proxy_fallback_to_local: !proxy_strict,
                    proxy_allow_mutating_tools,
                }
            },
        )
}

fn tool_definition(annotation: Option<Value>) -> McpClientToolDefinition {
    McpClientToolDefinition {
        name: "tool".to_string(),
        description: None,
        input_schema: json!({"type": "object"}),
        output_schema: None,
        icon: None,
        version: None,
        tags: Vec::new(),
        annotations: annotation,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_mcp_client_boundary_valid_configs_roundtrip_and_validate(config in valid_client_config()) {
        let encoded = serde_json::to_string(&config).expect("mcp config should serialize");
        let decoded: McpClientConfig =
            serde_json::from_str(&encoded).expect("mcp config should deserialize");

        prop_assert_eq!(decoded, config);
        prop_assert!(decoded.validate().is_ok());

        let preferred: HashSet<String> = decoded
            .preferred_servers
            .iter()
            .map(|server| server.to_ascii_lowercase())
            .collect();
        prop_assert_eq!(preferred.len(), decoded.preferred_servers.len());
    }

    #[test]
    fn proptest_mcp_client_boundary_duplicate_servers_are_rejected_case_insensitively(name in non_empty_name()) {
        let mut upper = name.to_ascii_uppercase();
        if upper == name {
            upper = name.to_ascii_lowercase();
        }
        let config = McpClientConfig {
            preferred_servers: vec![name.clone(), upper],
            ..McpClientConfig::default()
        };

        let error = config.validate().expect_err("case-insensitive duplicate should fail");

        prop_assert!(error.contains("duplicate server name"));
        prop_assert!(error.contains(name.trim()));
    }

    #[test]
    fn proptest_mcp_client_boundary_external_server_config_roundtrips(
        name in non_empty_name(),
        command in non_empty_name(),
        args in prop::collection::vec(small_text(), 0..=8),
        env in env_map(),
        cwd in prop::option::of(small_text()),
        disabled in any::<bool>(),
    ) {
        let config = ExternalServerConfig {
            name,
            command,
            args,
            env,
            cwd,
            disabled,
        };

        let encoded = serde_json::to_string(&config).expect("external server should serialize");
        let decoded: ExternalServerConfig =
            serde_json::from_str(&encoded).expect("external server should deserialize");

        prop_assert_eq!(decoded, config);
    }

    #[test]
    fn proptest_mcp_client_boundary_destructive_annotation_requires_true_bool(value in any::<bool>()) {
        let tool = tool_definition(Some(json!({"destructive": value})));
        let stringly_tool = tool_definition(Some(json!({"destructive": value.to_string()})));
        let missing_tool = tool_definition(Some(json!({"other": value})));

        prop_assert_eq!(tool.is_destructive(), value);
        prop_assert!(!stringly_tool.is_destructive());
        prop_assert!(!missing_tool.is_destructive());
        prop_assert!(!tool_definition(None).is_destructive());
    }

    #[test]
    fn proptest_mcp_client_boundary_text_content_extraction_is_shape_sensitive(text in small_text(), wrong_type in small_text()) {
        let text_item = McpClientContentItem(json!({"type": "text", "text": text}));
        let wrong_type_item = McpClientContentItem(json!({"type": wrong_type, "text": text}));
        let missing_text_item = McpClientContentItem(json!({"type": "text"}));

        prop_assert_eq!(text_item.as_text(), Some(text.as_str()));
        if wrong_type != "text" {
            prop_assert_eq!(wrong_type_item.as_text(), None);
        }
        prop_assert_eq!(missing_text_item.as_text(), None);
    }

    #[test]
    fn proptest_mcp_client_boundary_error_hint_roundtrips(code in prop::sample::select(vec![
        "mcp_client.timeout",
        "mcp_client.protocol",
        "mcp_client.discovery",
    ]), message in small_text(), hint in prop::option::of(small_text())) {
        let mut error = McpClientError::new(code, message.clone());
        if let Some(hint) = hint.clone() {
            error = error.with_hint(hint);
        }

        let encoded = serde_json::to_value(&error).expect("client error should serialize");

        prop_assert_eq!(encoded.get("code"), Some(&Value::String(code.to_string())));
        prop_assert_eq!(encoded.get("message"), Some(&Value::String(message.clone())));
        prop_assert_eq!(error.to_string(), format!("[{code}] {message}"));
    }
}
