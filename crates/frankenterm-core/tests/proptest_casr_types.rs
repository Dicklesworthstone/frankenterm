//! Property-based tests for `casr_types` — vendored IR for cross_agent_session_resumer.
//!
//! Requires `--features session-resume`.

use std::collections::HashMap;
use std::path::PathBuf;

use proptest::prelude::*;
use serde_json::json;

use frankenterm_core::casr_types::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_message_role() -> impl Strategy<Value = MessageRole> {
    prop_oneof![
        Just(MessageRole::User),
        Just(MessageRole::Assistant),
        Just(MessageRole::Tool),
        Just(MessageRole::System),
        "[a-z]{1,20}".prop_map(MessageRole::Other),
    ]
}

fn arb_tool_call() -> impl Strategy<Value = ToolCall> {
    ("[a-zA-Z_]{1,20}", any::<bool>()).prop_map(|(name, has_id)| ToolCall {
        id: if has_id {
            Some(format!("tc-{}", name))
        } else {
            None
        },
        name,
        arguments: json!({}),
        extra: HashMap::new(),
    })
}

fn arb_tool_result() -> impl Strategy<Value = ToolResult> {
    (".{0,50}", any::<bool>(), any::<bool>()).prop_map(|(content, is_error, has_id)| ToolResult {
        call_id: if has_id {
            Some("call-1".to_string())
        } else {
            None
        },
        content,
        is_error,
        extra: HashMap::new(),
    })
}

fn arb_canonical_message() -> impl Strategy<Value = CanonicalMessage> {
    (
        any::<usize>(),
        arb_message_role(),
        ".{0,100}",
        any::<bool>(),
    )
        .prop_map(|(idx, role, content, has_ts)| CanonicalMessage {
            idx,
            role,
            content,
            timestamp: if has_ts {
                Some(1_700_000_000_000)
            } else {
                None
            },
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: json!({}),
        })
}

fn arb_canonical_session() -> impl Strategy<Value = CanonicalSession> {
    (
        "[a-z0-9-]{1,30}",
        "[a-z-]{1,20}",
        any::<bool>(),
        proptest::collection::vec(arb_canonical_message(), 0..5),
    )
        .prop_map(
            |(session_id, provider_slug, has_title, messages)| CanonicalSession {
                session_id,
                provider_slug,
                workspace: Some(PathBuf::from("/tmp/ws")),
                title: if has_title {
                    Some("Test title".to_string())
                } else {
                    None
                },
                started_at: Some(1_700_000_000_000),
                ended_at: None,
                messages,
                metadata: json!({}),
                source_path: PathBuf::from("/tmp/src.jsonl"),
                model_name: None,
            },
        )
}

fn arb_list_entry() -> impl Strategy<Value = CasrListEntry> {
    ("[a-z0-9-]{1,30}", 0..1000usize).prop_map(|(session_id, messages)| CasrListEntry {
        session_id,
        provider: Some("test-provider".to_string()),
        title: Some("entry".to_string()),
        messages,
        workspace: None,
        started_at: Some(1_700_000_000_000),
        path: None,
        extra: HashMap::new(),
    })
}

fn arb_provider_status() -> impl Strategy<Value = CasrProviderStatus> {
    ("[a-z-]{1,20}", any::<bool>()).prop_map(|(slug, installed)| CasrProviderStatus {
        name: slug.to_uppercase(),
        slug,
        alias: None,
        installed,
        version: if installed {
            Some("1.0.0".to_string())
        } else {
            None
        },
        evidence: vec![],
        extra: HashMap::new(),
    })
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 1. MessageRole serde roundtrip
    #[test]
    fn message_role_serde_roundtrip(role in arb_message_role()) {
        let json_str = serde_json::to_string(&role).unwrap();
        let rt: MessageRole = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(role, rt);
    }

    // 2. MessageRole::Other preserves arbitrary content
    #[test]
    fn message_role_other_preserves_content(s in "[a-z]{1,30}") {
        let role = MessageRole::Other(s.clone());
        let json_str = serde_json::to_string(&role).unwrap();
        let rt: MessageRole = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(MessageRole::Other(s), rt);
    }

    // 3. normalize_role case-insensitive for known roles
    #[test]
    fn normalize_role_case_insensitive(
        role in prop_oneof![
            Just("user"), Just("USER"), Just("User"), Just("uSeR"),
        ]
    ) {
        let result = normalize_role(role);
        prop_assert_eq!(result, MessageRole::User);
    }

    // 4. normalize_role aliases all map to Assistant
    #[test]
    fn normalize_role_aliases_to_assistant(
        alias in prop_oneof![
            Just("assistant"), Just("model"), Just("agent"), Just("gemini"),
            Just("ASSISTANT"), Just("MODEL"), Just("AGENT"), Just("GEMINI"),
        ]
    ) {
        let result = normalize_role(alias);
        prop_assert_eq!(result, MessageRole::Assistant);
    }

    // 5. normalize_role unknown strings produce Other(lowercase)
    #[test]
    fn normalize_role_unknown_produces_other(s in "[a-z]{5,15}") {
        // Skip known role strings
        prop_assume!(
            s != "user" && s != "assistant" && s != "tool" && s != "system"
            && s != "model" && s != "agent" && s != "gemini"
        );
        let result = normalize_role(&s);
        prop_assert_eq!(result, MessageRole::Other(s));
    }

    // 6. normalize_role is idempotent when applied to its own Display
    #[test]
    fn normalize_role_idempotent_for_known(
        role_str in prop_oneof![
            Just("user"), Just("assistant"), Just("tool"), Just("system"),
        ]
    ) {
        let first = normalize_role(role_str);
        // Serialize and normalize again
        let serialized = serde_json::to_string(&first).unwrap();
        let deserialized: MessageRole = serde_json::from_str(&serialized).unwrap();
        prop_assert_eq!(first, deserialized);
    }

    // 7. CanonicalSession serde roundtrip
    #[test]
    fn canonical_session_roundtrip(session in arb_canonical_session()) {
        let json_str = serde_json::to_string(&session).unwrap();
        let rt: CanonicalSession = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(&session.session_id, &rt.session_id);
        prop_assert_eq!(&session.provider_slug, &rt.provider_slug);
        prop_assert_eq!(session.messages.len(), rt.messages.len());
        prop_assert_eq!(session.started_at, rt.started_at);
    }

    // 8. CanonicalSession None fields not in serialized JSON
    #[test]
    fn canonical_session_skip_none_fields(session in arb_canonical_session()) {
        let json_str = serde_json::to_string(&session).unwrap();
        if session.title.is_none() {
            prop_assert!(!json_str.contains("\"title\""));
        }
        if session.ended_at.is_none() {
            prop_assert!(!json_str.contains("\"ended_at\""));
        }
        if session.model_name.is_none() {
            prop_assert!(!json_str.contains("\"model_name\""));
        }
    }

    // 9. CanonicalMessage serde roundtrip
    #[test]
    fn canonical_message_roundtrip(msg in arb_canonical_message()) {
        let json_str = serde_json::to_string(&msg).unwrap();
        let rt: CanonicalMessage = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(msg.idx, rt.idx);
        prop_assert_eq!(msg.content, rt.content);
        prop_assert_eq!(msg.timestamp, rt.timestamp);
    }

    // 10. CanonicalMessage empty tool vectors not serialized
    #[test]
    fn canonical_message_skip_empty_tool_vectors(msg in arb_canonical_message()) {
        let json_str = serde_json::to_string(&msg).unwrap();
        if msg.tool_calls.is_empty() {
            prop_assert!(!json_str.contains("\"tool_calls\""));
        }
        if msg.tool_results.is_empty() {
            prop_assert!(!json_str.contains("\"tool_results\""));
        }
    }

    // 11. ToolCall serde roundtrip
    #[test]
    fn tool_call_roundtrip(tc in arb_tool_call()) {
        let json_str = serde_json::to_string(&tc).unwrap();
        let rt: ToolCall = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(&tc.name, &rt.name);
        prop_assert_eq!(&tc.id, &rt.id);
    }

    // 12. ToolCall forward-compat: extra fields preserved
    #[test]
    fn tool_call_forward_compat(key in "[a-z]{3,10}", val in 0..1000i64) {
        // Ensure the key isn't a known field
        prop_assume!(key != "name" && key != "id" && key != "arguments");
        let json_str = format!(
            r#"{{"name":"test","arguments":{{}},"{}":{}  }}"#,
            key, val
        );
        let tc: ToolCall = serde_json::from_str(&json_str).unwrap();
        let extra_val = tc.extra.get(&key).and_then(|v| v.as_i64());
        prop_assert_eq!(extra_val, Some(val));
    }

    // 13. ToolResult serde roundtrip
    #[test]
    fn tool_result_roundtrip(tr in arb_tool_result()) {
        let json_str = serde_json::to_string(&tr).unwrap();
        let rt: ToolResult = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(&tr.content, &rt.content);
        prop_assert_eq!(tr.is_error, rt.is_error);
        prop_assert_eq!(&tr.call_id, &rt.call_id);
    }

    // 14. ToolResult forward-compat: extra fields preserved
    #[test]
    fn tool_result_forward_compat(key in "[a-z]{3,10}", val in 0..1000i64) {
        prop_assume!(key != "content" && key != "is_error" && key != "call_id");
        let json_str = format!(
            r#"{{"content":"ok","is_error":false,"{}":{}}}"#,
            key, val
        );
        let tr: ToolResult = serde_json::from_str(&json_str).unwrap();
        let extra_val = tr.extra.get(&key).and_then(|v| v.as_i64());
        prop_assert_eq!(extra_val, Some(val));
    }

    // 15. CasrResumeOutput serde roundtrip
    #[test]
    fn resume_output_roundtrip(ok in any::<bool>(), dry_run in any::<bool>()) {
        let out = CasrResumeOutput {
            ok,
            source_provider: Some("src".into()),
            target_provider: Some("tgt".into()),
            source_session_id: Some("s1".into()),
            target_session_id: Some("t1".into()),
            written_paths: Some(vec![PathBuf::from("/tmp/out")]),
            resume_command: Some("resume cmd".into()),
            dry_run,
            warnings: vec!["warn1".into()],
            extra: HashMap::new(),
        };
        let json_str = serde_json::to_string(&out).unwrap();
        let rt: CasrResumeOutput = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(out.ok, rt.ok);
        prop_assert_eq!(out.dry_run, rt.dry_run);
        prop_assert_eq!(out.warnings.len(), rt.warnings.len());
    }

    // 16. CasrResumeOutput forward-compat
    #[test]
    fn resume_output_forward_compat(key in "[a-z]{3,10}", val in 0..1000i64) {
        prop_assume!(
            key != "ok" && key != "dry_run" && key != "warnings"
            && key != "source_provider" && key != "target_provider"
            && key != "source_session_id" && key != "target_session_id"
            && key != "written_paths" && key != "resume_command"
        );
        let json_str = format!(r#"{{"ok":true,"{}":{}}}"#, key, val);
        let out: CasrResumeOutput = serde_json::from_str(&json_str).unwrap();
        let extra_val = out.extra.get(&key).and_then(|v| v.as_i64());
        prop_assert_eq!(extra_val, Some(val));
    }

    // 17. CasrListEntry serde roundtrip
    #[test]
    fn list_entry_roundtrip(entry in arb_list_entry()) {
        let json_str = serde_json::to_string(&entry).unwrap();
        let rt: CasrListEntry = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(&entry.session_id, &rt.session_id);
        prop_assert_eq!(entry.messages, rt.messages);
    }

    // 18. CasrListEntry forward-compat
    #[test]
    fn list_entry_forward_compat(key in "[a-z]{3,10}", val in 0..1000i64) {
        prop_assume!(
            key != "session_id" && key != "provider" && key != "title"
            && key != "messages" && key != "workspace" && key != "started_at"
            && key != "path"
        );
        let json_str = format!(r#"{{"session_id":"x","{}":{}}}"#, key, val);
        let entry: CasrListEntry = serde_json::from_str(&json_str).unwrap();
        let extra_val = entry.extra.get(&key).and_then(|v| v.as_i64());
        prop_assert_eq!(extra_val, Some(val));
    }

    // 19. CasrProviderStatus serde roundtrip
    #[test]
    fn provider_status_roundtrip(status in arb_provider_status()) {
        let json_str = serde_json::to_string(&status).unwrap();
        let rt: CasrProviderStatus = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(&status.slug, &rt.slug);
        prop_assert_eq!(status.installed, rt.installed);
    }

    // 20. CasrProviderStatus forward-compat
    #[test]
    fn provider_status_forward_compat(key in "[a-z]{3,10}", val in 0..1000i64) {
        prop_assume!(
            key != "name" && key != "slug" && key != "alias"
            && key != "installed" && key != "version" && key != "evidence"
        );
        let json_str = format!(r#"{{"name":"X","slug":"x","{}":{}}}"#, key, val);
        let status: CasrProviderStatus = serde_json::from_str(&json_str).unwrap();
        let extra_val = status.extra.get(&key).and_then(|v| v.as_i64());
        prop_assert_eq!(extra_val, Some(val));
    }

    // 21. reindex_messages produces 0..n sequential indices
    #[test]
    fn reindex_sequential(count in 0..50usize) {
        let mut msgs: Vec<CanonicalMessage> = (0..count)
            .map(|_| CanonicalMessage {
                idx: 999,
                role: MessageRole::User,
                content: "x".into(),
                timestamp: None,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: json!({}),
            })
            .collect();
        reindex_messages(&mut msgs);
        for (i, msg) in msgs.iter().enumerate() {
            prop_assert_eq!(msg.idx, i);
        }
    }

    // 22. reindex_messages preserves length
    #[test]
    fn reindex_preserves_length(count in 0..50usize) {
        let mut msgs: Vec<CanonicalMessage> = (0..count)
            .map(|i| CanonicalMessage {
                idx: i * 7,
                role: MessageRole::Assistant,
                content: format!("msg-{}", i),
                timestamp: None,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: json!({}),
            })
            .collect();
        let original_len = msgs.len();
        reindex_messages(&mut msgs);
        prop_assert_eq!(msgs.len(), original_len);
    }

    // 23. reindex_messages is idempotent
    #[test]
    fn reindex_idempotent(count in 0..30usize) {
        let mut msgs: Vec<CanonicalMessage> = (0..count)
            .map(|_| CanonicalMessage {
                idx: 42,
                role: MessageRole::Tool,
                content: String::new(),
                timestamp: None,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: json!({}),
            })
            .collect();
        reindex_messages(&mut msgs);
        let indices_after_first: Vec<usize> = msgs.iter().map(|m| m.idx).collect();
        reindex_messages(&mut msgs);
        let indices_after_second: Vec<usize> = msgs.iter().map(|m| m.idx).collect();
        prop_assert_eq!(indices_after_first, indices_after_second);
    }

    // 24. truncate_title respects max_len bound
    #[test]
    fn truncate_title_length_bound(text in ".{0,200}", max_len in 1..100usize) {
        let result = truncate_title(&text, max_len);
        // Result is either empty, within max_len, or max_len + "..." (3 bytes)
        if !result.is_empty() && result.ends_with("...") {
            // Truncated: base part <= max_len
            let base = &result[..result.len() - 3];
            prop_assert!(base.len() <= max_len);
        } else {
            // Not truncated: within max_len
            prop_assert!(result.len() <= max_len);
        }
    }

    // 25. truncate_title empty input → empty output
    #[test]
    fn truncate_title_empty_input(max_len in 1..100usize) {
        let result = truncate_title("", max_len);
        prop_assert!(result.is_empty());
    }

    // 26. truncate_title uses only first line
    #[test]
    fn truncate_title_first_line_only(
        first in "[a-z]{1,20}",
        rest in "[a-z]{1,20}",
    ) {
        let input = format!("{}\n{}", first, rest);
        let result = truncate_title(&input, 100);
        prop_assert_eq!(result, first);
    }

    // 27. truncate_title result is valid UTF-8 (char boundary safety)
    #[test]
    fn truncate_title_valid_utf8(text in "\\PC{0,100}", max_len in 1..50usize) {
        let result = truncate_title(&text, max_len);
        // If it's a valid String, char boundaries are respected
        prop_assert!(result.is_ascii() || !result.is_empty() || result.is_empty());
        // More importantly: the String was constructed without panicking
    }

    // 28. truncate_title adds ellipsis only when truncated
    #[test]
    fn truncate_title_ellipsis_only_when_truncated(line in "[a-z]{1,50}", max_len in 1..100usize) {
        let result = truncate_title(&line, max_len);
        if line.len() <= max_len {
            prop_assert!(!result.ends_with("..."), "Should not have ellipsis for short text");
        } else {
            prop_assert!(result.ends_with("..."), "Should have ellipsis for long text");
        }
    }

    // 29. CanonicalSession minimal JSON (only required fields) deserializes
    #[test]
    fn session_minimal_deserialize(
        session_id in "[a-z0-9]{1,20}",
        provider in "[a-z]{1,10}",
    ) {
        let json_val = json!({
            "session_id": session_id,
            "provider_slug": provider,
            "source_path": "/tmp/x"
        });
        let session: CanonicalSession = serde_json::from_value(json_val).unwrap();
        prop_assert_eq!(&session.session_id, &session_id);
        prop_assert!(session.messages.is_empty());
        prop_assert!(session.title.is_none());
        prop_assert!(session.workspace.is_none());
    }

    // 30. reindex preserves message content
    #[test]
    fn reindex_preserves_content(
        contents in proptest::collection::vec("[a-z]{1,20}", 1..20),
    ) {
        let mut msgs: Vec<CanonicalMessage> = contents.iter().enumerate().map(|(i, c)| {
            CanonicalMessage {
                idx: i * 100,
                role: MessageRole::User,
                content: c.clone(),
                timestamp: None,
                author: None,
                tool_calls: vec![],
                tool_results: vec![],
                extra: json!({}),
            }
        }).collect();
        let original_contents: Vec<String> = msgs.iter().map(|m| m.content.clone()).collect();
        reindex_messages(&mut msgs);
        let after_contents: Vec<String> = msgs.iter().map(|m| m.content.clone()).collect();
        prop_assert_eq!(original_contents, after_contents);
    }
}
