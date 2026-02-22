//! Vendored types for `casr` (cross_agent_session_resumer) integration.
//!
//! These are local mirrors of the casr canonical IR, designed for subprocess
//! communication (parsing JSON output from `casr resume`, `casr list`, `casr providers`).
//! They intentionally duplicate rather than depend on the casr crate to avoid
//! a hard compile-time dependency and to allow ft-side evolution.
//!
//! Feature-gated behind `session-resume`.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Core IR types — mirrors of casr::model
// ---------------------------------------------------------------------------

/// Provider-agnostic session representation.
///
/// Mirrors `casr::model::CanonicalSession`. The `metadata` field preserves
/// provider-specific JSON that doesn't map to canonical fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalSession {
    pub session_id: String,
    pub provider_slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    /// Epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    #[serde(default)]
    pub messages: Vec<CanonicalMessage>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub source_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
}

/// A single conversation message.
///
/// Mirrors `casr::model::CanonicalMessage`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalMessage {
    #[serde(default)]
    pub idx: usize,
    pub role: MessageRole,
    #[serde(default)]
    pub content: String,
    /// Epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_results: Vec<ToolResult>,
    /// Provider-specific fields for round-trip fidelity.
    #[serde(default)]
    pub extra: serde_json::Value,
}

/// Sender role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
    System,
    Other(String),
}

/// A tool invocation within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
    /// Forward-compat: captures unknown fields from future casr versions.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// A result returned from a tool invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Subprocess JSON output types — parsed from `casr` CLI --json output
// ---------------------------------------------------------------------------

/// JSON output from `casr resume --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasrResumeOutput {
    #[serde(default)]
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written_paths: Option<Vec<PathBuf>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_command: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// A single entry from `casr list --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasrListEntry {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub messages: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Provider installation status from `casr providers --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasrProviderStatus {
    pub name: String,
    pub slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default)]
    pub installed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Helpers — subset ported from casr::model
// ---------------------------------------------------------------------------

/// Map provider role strings to canonical [`MessageRole`] (case-insensitive).
pub fn normalize_role(role_str: &str) -> MessageRole {
    match role_str.to_ascii_lowercase().as_str() {
        "user" => MessageRole::User,
        "assistant" | "model" | "agent" | "gemini" => MessageRole::Assistant,
        "tool" => MessageRole::Tool,
        "system" => MessageRole::System,
        other => MessageRole::Other(other.to_string()),
    }
}

/// Re-assign sequential idx values after filtering/sorting.
pub fn reindex_messages(messages: &mut [CanonicalMessage]) {
    for (i, msg) in messages.iter_mut().enumerate() {
        msg.idx = i;
    }
}

/// Extract title from content: first line, truncated to `max_len` with ellipsis.
pub fn truncate_title(text: &str, max_len: usize) -> String {
    let first_line = text.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return String::new();
    }
    if first_line.len() <= max_len {
        first_line.to_string()
    } else {
        let mut end = max_len;
        while !first_line.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}...", &first_line[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- Core type serde round-trips --

    #[test]
    fn canonical_session_roundtrip() {
        let session = CanonicalSession {
            session_id: "sess-1".into(),
            provider_slug: "claude-code".into(),
            workspace: Some(PathBuf::from("/project")),
            title: Some("Test session".into()),
            started_at: Some(1_700_000_000_000),
            ended_at: None,
            messages: vec![],
            metadata: json!({"origin": "test"}),
            source_path: PathBuf::from("/tmp/session.jsonl"),
            model_name: Some("claude-4".into()),
        };
        let json_str = serde_json::to_string(&session).unwrap();
        let rt: CanonicalSession = serde_json::from_str(&json_str).unwrap();
        assert_eq!(rt.session_id, "sess-1");
        assert_eq!(rt.provider_slug, "claude-code");
        assert_eq!(rt.started_at, Some(1_700_000_000_000));
    }

    #[test]
    fn canonical_message_roundtrip() {
        let msg = CanonicalMessage {
            idx: 0,
            role: MessageRole::Assistant,
            content: "Hello".into(),
            timestamp: Some(1_700_000_000_000),
            author: Some("claude-3".into()),
            tool_calls: vec![ToolCall {
                id: Some("tc1".into()),
                name: "Read".into(),
                arguments: json!({"file_path": "/foo.rs"}),
                extra: HashMap::new(),
            }],
            tool_results: vec![ToolResult {
                call_id: Some("tc1".into()),
                content: "contents".into(),
                is_error: false,
                extra: HashMap::new(),
            }],
            extra: json!({"custom": true}),
        };
        let json_str = serde_json::to_string(&msg).unwrap();
        let rt: CanonicalMessage = serde_json::from_str(&json_str).unwrap();
        assert_eq!(rt.idx, 0);
        assert_eq!(rt.role, MessageRole::Assistant);
        assert_eq!(rt.tool_calls.len(), 1);
        assert_eq!(rt.tool_results.len(), 1);
    }

    #[test]
    fn message_role_other_roundtrip() {
        let role = MessageRole::Other("reasoning".into());
        let json_str = serde_json::to_string(&role).unwrap();
        let rt: MessageRole = serde_json::from_str(&json_str).unwrap();
        assert_eq!(rt, role);
    }

    #[test]
    fn tool_call_forward_compat() {
        let json_str = r#"{"name":"Bash","arguments":{},"new_field":"hello"}"#;
        let tc: ToolCall = serde_json::from_str(json_str).unwrap();
        assert_eq!(tc.name, "Bash");
        assert_eq!(
            tc.extra.get("new_field").and_then(|v| v.as_str()),
            Some("hello")
        );
    }

    #[test]
    fn tool_result_forward_compat() {
        let json_str = r#"{"content":"ok","is_error":false,"exit_code":0}"#;
        let tr: ToolResult = serde_json::from_str(json_str).unwrap();
        assert_eq!(tr.content, "ok");
        assert!(!tr.is_error);
        assert_eq!(tr.extra.get("exit_code").and_then(|v| v.as_u64()), Some(0));
    }

    // -- Subprocess output types --

    #[test]
    fn resume_output_parse() {
        let json_str = json!({
            "ok": true,
            "source_provider": "claude-code",
            "target_provider": "codex",
            "source_session_id": "s1",
            "target_session_id": "t1",
            "written_paths": ["/tmp/out.jsonl"],
            "resume_command": "codex --resume t1",
            "dry_run": false,
            "warnings": ["minor issue"]
        });
        let out: CasrResumeOutput = serde_json::from_value(json_str).unwrap();
        assert!(out.ok);
        assert_eq!(out.source_provider.as_deref(), Some("claude-code"));
        assert_eq!(out.warnings.len(), 1);
    }

    #[test]
    fn resume_output_forward_compat() {
        let json_str = json!({"ok": true, "dry_run": false, "new_field": 42});
        let out: CasrResumeOutput = serde_json::from_value(json_str).unwrap();
        assert!(out.ok);
        assert_eq!(
            out.extra.get("new_field").and_then(|v| v.as_u64()),
            Some(42)
        );
    }

    #[test]
    fn list_entry_parse() {
        let json_str = json!({
            "session_id": "abc-123",
            "provider": "claude-code",
            "title": "Fix the bug",
            "messages": 42,
            "workspace": "/project",
            "started_at": 1_700_000_000_000_i64,
            "path": "/tmp/session.jsonl"
        });
        let entry: CasrListEntry = serde_json::from_value(json_str).unwrap();
        assert_eq!(entry.session_id, "abc-123");
        assert_eq!(entry.messages, 42);
    }

    #[test]
    fn list_entry_forward_compat() {
        let json_str = json!({"session_id": "x", "messages": 0, "tags": ["important"]});
        let entry: CasrListEntry = serde_json::from_value(json_str).unwrap();
        assert!(entry.extra.contains_key("tags"));
    }

    #[test]
    fn provider_status_parse() {
        let json_str = json!({
            "name": "Claude Code",
            "slug": "claude-code",
            "alias": "cc",
            "installed": true,
            "version": "1.2.3",
            "evidence": ["found at /usr/local/bin/claude"]
        });
        let status: CasrProviderStatus = serde_json::from_value(json_str).unwrap();
        assert_eq!(status.slug, "claude-code");
        assert!(status.installed);
        assert_eq!(status.evidence.len(), 1);
    }

    #[test]
    fn provider_status_forward_compat() {
        let json_str = json!({"name": "X", "slug": "x", "installed": false, "beta": true});
        let status: CasrProviderStatus = serde_json::from_value(json_str).unwrap();
        assert!(!status.installed);
        assert_eq!(
            status.extra.get("beta").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    // -- Helper function tests --

    #[test]
    fn normalize_role_standard() {
        assert_eq!(normalize_role("user"), MessageRole::User);
        assert_eq!(normalize_role("assistant"), MessageRole::Assistant);
        assert_eq!(normalize_role("tool"), MessageRole::Tool);
        assert_eq!(normalize_role("system"), MessageRole::System);
    }

    #[test]
    fn normalize_role_case_insensitive() {
        assert_eq!(normalize_role("USER"), MessageRole::User);
        assert_eq!(normalize_role("Assistant"), MessageRole::Assistant);
    }

    #[test]
    fn normalize_role_aliases() {
        assert_eq!(normalize_role("model"), MessageRole::Assistant);
        assert_eq!(normalize_role("agent"), MessageRole::Assistant);
        assert_eq!(normalize_role("gemini"), MessageRole::Assistant);
    }

    #[test]
    fn normalize_role_unknown() {
        assert_eq!(
            normalize_role("reasoning"),
            MessageRole::Other("reasoning".into())
        );
    }

    #[test]
    fn reindex_messages_sequential() {
        let mut msgs = vec![
            CanonicalMessage {
                idx: 99,
                role: MessageRole::User,
                content: "a".into(),
                timestamp: None,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: json!({}),
            },
            CanonicalMessage {
                idx: 42,
                role: MessageRole::Assistant,
                content: "b".into(),
                timestamp: None,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: json!({}),
            },
        ];
        reindex_messages(&mut msgs);
        assert_eq!(msgs[0].idx, 0);
        assert_eq!(msgs[1].idx, 1);
    }

    #[test]
    fn reindex_messages_empty() {
        let mut msgs: Vec<CanonicalMessage> = vec![];
        reindex_messages(&mut msgs);
        assert!(msgs.is_empty());
    }

    #[test]
    fn truncate_title_short() {
        assert_eq!(truncate_title("Hello", 100), "Hello");
    }

    #[test]
    fn truncate_title_long() {
        let long = "a".repeat(200);
        let result = truncate_title(&long, 50);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 53);
    }

    #[test]
    fn truncate_title_multiline() {
        assert_eq!(truncate_title("first\nsecond\nthird", 100), "first");
    }

    #[test]
    fn truncate_title_empty() {
        assert_eq!(truncate_title("", 100), "");
    }

    #[test]
    fn truncate_title_whitespace() {
        assert_eq!(truncate_title("   \n   ", 100), "");
    }

    #[test]
    fn message_role_all_variants_serialize() {
        let variants = vec![
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Tool,
            MessageRole::System,
            MessageRole::Other("custom".into()),
        ];
        for v in variants {
            let json_str = serde_json::to_string(&v).unwrap();
            let rt: MessageRole = serde_json::from_str(&json_str).unwrap();
            assert_eq!(rt, v);
        }
    }

    #[test]
    fn canonical_session_minimal_deserialize() {
        let json_str = json!({
            "session_id": "s1",
            "provider_slug": "cod",
            "source_path": "/tmp/x"
        });
        let session: CanonicalSession = serde_json::from_value(json_str).unwrap();
        assert_eq!(session.session_id, "s1");
        assert!(session.messages.is_empty());
        assert!(session.title.is_none());
    }

    #[test]
    fn canonical_message_minimal_deserialize() {
        let json_str = json!({"role": "User", "content": "hi"});
        let msg: CanonicalMessage = serde_json::from_value(json_str).unwrap();
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.content, "hi");
        assert_eq!(msg.idx, 0);
    }

    #[test]
    fn resume_output_minimal_deserialize() {
        let json_str = json!({"ok": false});
        let out: CasrResumeOutput = serde_json::from_value(json_str).unwrap();
        assert!(!out.ok);
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn provider_status_minimal_deserialize() {
        let json_str = json!({"name": "X", "slug": "x"});
        let status: CasrProviderStatus = serde_json::from_value(json_str).unwrap();
        assert!(!status.installed);
        assert!(status.evidence.is_empty());
    }

    #[test]
    fn list_entry_minimal_deserialize() {
        let json_str = json!({"session_id": "s1"});
        let entry: CasrListEntry = serde_json::from_value(json_str).unwrap();
        assert_eq!(entry.session_id, "s1");
        assert_eq!(entry.messages, 0);
    }

    #[test]
    fn skip_serializing_none_fields() {
        let session = CanonicalSession {
            session_id: "s1".into(),
            provider_slug: "cc".into(),
            workspace: None,
            title: None,
            started_at: None,
            ended_at: None,
            messages: vec![],
            metadata: json!(null),
            source_path: PathBuf::from("/tmp"),
            model_name: None,
        };
        let json_str = serde_json::to_string(&session).unwrap();
        assert!(!json_str.contains("workspace"));
        assert!(!json_str.contains("title"));
        assert!(!json_str.contains("model_name"));
    }

    #[test]
    fn skip_serializing_empty_tool_vectors() {
        let msg = CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: "hi".into(),
            timestamp: None,
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: json!({}),
        };
        let json_str = serde_json::to_string(&msg).unwrap();
        assert!(!json_str.contains("tool_calls"));
        assert!(!json_str.contains("tool_results"));
    }
}
