#![cfg(feature = "mcp")]

use frankenterm_core::mcp_framework::{
    FrameworkContent, FrameworkResource, FrameworkResourceTemplate, FrameworkTool,
};
use proptest::prelude::*;

fn ascii_text() -> impl Strategy<Value = String> {
    "[A-Za-z0-9 _.,:/?&=-]{0,96}".prop_map(String::from)
}

fn safe_name() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_.-]{0,31}".prop_map(String::from)
}

fn mime_type() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("text/plain".to_string()),
        Just("application/json".to_string()),
        Just("image/png".to_string()),
        Just("audio/wav".to_string()),
        "[a-z]{1,12}/[a-z0-9.+-]{1,20}".prop_map(String::from),
    ]
}

fn tags() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec("[a-z][a-z0-9_-]{0,15}", 0..5)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn proptest_mcp_framework_text_content_serializes_as_mcp_text(text in ascii_text()) {
        let content = FrameworkContent::text(text.clone());
        let json = serde_json::to_value(content).expect("serialize framework text content");

        prop_assert_eq!(json.get("type").and_then(serde_json::Value::as_str), Some("text"));
        prop_assert_eq!(json.get("text").and_then(serde_json::Value::as_str), Some(text.as_str()));
        prop_assert!(json.get("mimeType").is_none());
    }

    #[test]
    fn proptest_mcp_framework_binary_content_preserves_payload_and_mime(
        payload in "[A-Za-z0-9+/=]{0,96}",
        mime in mime_type(),
        image in any::<bool>(),
    ) {
        let content = if image {
            FrameworkContent::image_base64(payload.clone(), mime.clone())
        } else {
            FrameworkContent::audio_base64(payload.clone(), mime.clone())
        };
        let json = serde_json::to_value(content).expect("serialize framework binary content");

        prop_assert_eq!(
            json.get("type").and_then(serde_json::Value::as_str),
            Some(if image { "image" } else { "audio" })
        );
        prop_assert_eq!(json.get("data").and_then(serde_json::Value::as_str), Some(payload.as_str()));
        prop_assert_eq!(json.get("mimeType").and_then(serde_json::Value::as_str), Some(mime.as_str()));
    }

    #[test]
    fn proptest_mcp_framework_resource_content_keeps_exact_payload_slot(
        uri_suffix in "[a-z0-9/_-]{1,32}",
        payload in ascii_text(),
        mime in proptest::option::of(mime_type()),
        blob in any::<bool>(),
    ) {
        let uri = format!("ft://resource/{uri_suffix}");
        let content = if blob {
            FrameworkContent::resource_blob_base64(uri.clone(), mime.clone(), payload.clone())
        } else {
            FrameworkContent::resource_text(uri.clone(), mime.clone(), payload.clone())
        };
        let json = serde_json::to_value(content).expect("serialize framework resource content");
        let resource = json
            .get("resource")
            .and_then(serde_json::Value::as_object)
            .expect("resource object");

        prop_assert_eq!(json.get("type").and_then(serde_json::Value::as_str), Some("resource"));
        prop_assert_eq!(resource.get("uri").and_then(serde_json::Value::as_str), Some(uri.as_str()));
        prop_assert_eq!(
            resource.get("mimeType").and_then(serde_json::Value::as_str),
            mime.as_deref()
        );
        if blob {
            prop_assert_eq!(resource.get("blob").and_then(serde_json::Value::as_str), Some(payload.as_str()));
            prop_assert!(resource.get("text").is_none());
        } else {
            prop_assert_eq!(resource.get("text").and_then(serde_json::Value::as_str), Some(payload.as_str()));
            prop_assert!(resource.get("blob").is_none());
        }
    }

    #[test]
    fn proptest_mcp_framework_tool_metadata_uses_stable_field_names(
        name in safe_name(),
        description in proptest::option::of(ascii_text()),
        output_property in safe_name(),
        version in proptest::option::of("[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}".prop_map(String::from)),
        tags in tags(),
    ) {
        let mut output_properties = serde_json::Map::new();
        output_properties.insert(output_property, serde_json::json!({ "type": "string" }));
        let input_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string" }
            }
        });
        let output_schema = serde_json::json!({
            "type": "object",
            "properties": output_properties
        });
        let tool = FrameworkTool {
            name: name.clone(),
            description: description.clone(),
            input_schema: input_schema.clone(),
            output_schema: Some(output_schema.clone()),
            icon: None,
            version: version.clone(),
            tags: tags.clone(),
            annotations: None,
        };
        let json = serde_json::to_value(tool).expect("serialize framework tool");

        prop_assert_eq!(json.get("name").and_then(serde_json::Value::as_str), Some(name.as_str()));
        prop_assert_eq!(json.get("description").and_then(serde_json::Value::as_str), description.as_deref());
        prop_assert_eq!(json.get("inputSchema"), Some(&input_schema));
        prop_assert_eq!(json.get("outputSchema"), Some(&output_schema));
        prop_assert_eq!(json.get("version").and_then(serde_json::Value::as_str), version.as_deref());
        prop_assert_eq!(json.get("tags").cloned().unwrap_or_else(|| serde_json::json!([])), serde_json::json!(tags));
    }

    #[test]
    fn proptest_mcp_framework_resource_template_metadata_uses_mcp_names(
        name in safe_name(),
        template_suffix in "[a-z0-9/_-]{1,32}",
        description in proptest::option::of(ascii_text()),
        mime in proptest::option::of(mime_type()),
        version in proptest::option::of("[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}".prop_map(String::from)),
        tags in tags(),
    ) {
        let uri_template = format!("ft://resource/{template_suffix}/{{id}}");
        let expected_uri = uri_template.replace("{id}", "example");
        let template = FrameworkResourceTemplate {
            uri_template: uri_template.clone(),
            name: name.clone(),
            description: description.clone(),
            mime_type: mime.clone(),
            icon: None,
            version: version.clone(),
            tags: tags.clone(),
        };
        let resource = FrameworkResource {
            uri: expected_uri.clone(),
            name: name.clone(),
            description: description.clone(),
            mime_type: mime.clone(),
            icon: None,
            version: version.clone(),
            tags: tags.clone(),
        };

        let template_json = serde_json::to_value(template).expect("serialize framework resource template");
        let resource_json = serde_json::to_value(resource).expect("serialize framework resource");

        prop_assert_eq!(
            template_json.get("uriTemplate").and_then(serde_json::Value::as_str),
            Some(uri_template.as_str())
        );
        prop_assert!(template_json.get("uri").is_none());
        prop_assert_eq!(template_json.get("mimeType").and_then(serde_json::Value::as_str), mime.as_deref());
        prop_assert_eq!(template_json.get("tags").cloned().unwrap_or_else(|| serde_json::json!([])), serde_json::json!(tags));

        prop_assert_eq!(
            resource_json.get("uri").and_then(serde_json::Value::as_str),
            Some(expected_uri.as_str())
        );
        prop_assert!(resource_json.get("uriTemplate").is_none());
        prop_assert_eq!(resource_json.get("mimeType").and_then(serde_json::Value::as_str), mime.as_deref());
    }
}
