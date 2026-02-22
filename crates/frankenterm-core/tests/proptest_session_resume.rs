//! Property-based tests for `session_resume` — CASR bridge orchestrator.
//!
//! Requires `--features session-resume`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use serde_json::json;

use frankenterm_core::casr_types::*;
use frankenterm_core::session_resume::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_agent_provider() -> impl Strategy<Value = AgentProvider> {
    prop_oneof![
        Just(AgentProvider::ClaudeCode),
        Just(AgentProvider::Codex),
        Just(AgentProvider::Gemini),
        Just(AgentProvider::Grok),
        "[a-z-]{1,20}".prop_map(AgentProvider::Other),
    ]
}

fn arb_config() -> impl Strategy<Value = SessionResumeConfig> {
    (any::<bool>(), 1..120u64).prop_map(|(dry_run, timeout)| SessionResumeConfig {
        casr_binary: "casr".to_string(),
        working_dir: Some(PathBuf::from("/tmp/ws")),
        timeout_secs: timeout,
        dry_run,
    })
}

fn arb_canonical_message() -> impl Strategy<Value = CanonicalMessage> {
    (".{0,50}", any::<bool>()).prop_map(|(content, has_ts)| CanonicalMessage {
        idx: 0,
        role: MessageRole::User,
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

fn arb_list_entry() -> impl Strategy<Value = CasrListEntry> {
    (
        "[a-z0-9-]{1,20}",
        prop::option::of("[a-z-]{1,10}"),
        0..500usize,
    )
        .prop_map(|(session_id, provider, messages)| CasrListEntry {
            session_id,
            provider,
            title: Some("entry".into()),
            messages,
            workspace: None,
            started_at: None,
            path: None,
            extra: HashMap::new(),
        })
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 1. AgentProvider slug roundtrip for all known variants
    #[test]
    fn agent_provider_slug_roundtrip(provider in arb_agent_provider()) {
        let slug = provider.slug();
        let rt = AgentProvider::from_slug(slug);
        prop_assert_eq!(provider, rt);
    }

    // 2. AgentProvider serde roundtrip
    #[test]
    fn agent_provider_serde_roundtrip(provider in arb_agent_provider()) {
        let json_str = serde_json::to_string(&provider).unwrap();
        let rt: AgentProvider = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(provider, rt);
    }

    // 3. AgentProvider::Other preserves arbitrary slugs
    #[test]
    fn agent_provider_other_preserves_slug(slug in "[a-z]{5,15}") {
        prop_assume!(
            slug != "claude" && slug != "codex" && slug != "gemini" && slug != "grok"
        );
        let provider = AgentProvider::Other(slug.clone());
        prop_assert_eq!(provider.slug(), slug.as_str());
    }

    // 4. AgentProvider Display matches slug
    #[test]
    fn agent_provider_display_matches_slug(provider in arb_agent_provider()) {
        let display = provider.to_string();
        let slug = provider.slug();
        prop_assert_eq!(display, slug);
    }

    // 5. SessionResumeConfig serde roundtrip
    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json_str = serde_json::to_string(&config).unwrap();
        let rt: SessionResumeConfig = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(config.dry_run, rt.dry_run);
        prop_assert_eq!(config.timeout_secs, rt.timeout_secs);
        prop_assert_eq!(config.casr_binary, rt.casr_binary);
    }

    // 6. SessionResumeConfig default serde
    #[test]
    fn config_default_fields_present(_dummy in 0..1u8) {
        let config = SessionResumeConfig::default();
        prop_assert_eq!(&config.casr_binary, "casr");
        prop_assert_eq!(config.timeout_secs, 30);
        prop_assert!(!config.dry_run);
    }

    // 7. SessionResumer with missing binary always fails discover
    #[test]
    fn resumer_missing_binary_fails(suffix in "[a-z]{5,15}") {
        let binary = format!("/nonexistent-{}", suffix);
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: binary,
            ..Default::default()
        });
        let result = r.discover_sessions();
        prop_assert!(result.is_err());
    }

    // 8. SessionResumer with missing binary: is_casr_available is false
    #[test]
    fn resumer_missing_binary_not_available(suffix in "[a-z]{5,15}") {
        let binary = format!("/nonexistent-{}", suffix);
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: binary,
            ..Default::default()
        });
        prop_assert!(!r.is_casr_available());
    }

    // 9. export_for_recorder preserves session_id
    #[test]
    fn export_preserves_session_id(session_id in "[a-z0-9-]{1,30}") {
        let r = SessionResumer::with_defaults();
        let export = r.export_for_recorder(
            &session_id, "test", Path::new("/tmp/x"), vec![], vec![],
        );
        prop_assert_eq!(&export.session.session_id, &session_id);
    }

    // 10. export_for_recorder preserves provider_slug
    #[test]
    fn export_preserves_provider_slug(slug in "[a-z-]{1,20}") {
        let r = SessionResumer::with_defaults();
        let export = r.export_for_recorder(
            "s1", &slug, Path::new("/tmp/x"), vec![], vec![],
        );
        prop_assert_eq!(&export.session.provider_slug, &slug);
    }

    // 11. export_for_recorder events_processed matches message count
    #[test]
    fn export_events_processed_matches_messages(
        msgs in proptest::collection::vec(arb_canonical_message(), 0..20),
    ) {
        let expected_count = msgs.len();
        let r = SessionResumer::with_defaults();
        let export = r.export_for_recorder(
            "s1", "test", Path::new("/tmp/x"), msgs, vec![],
        );
        prop_assert_eq!(export.events_processed, expected_count);
    }

    // 12. export_for_recorder preserves pane_ids
    #[test]
    fn export_preserves_pane_ids(
        pane_ids in proptest::collection::vec(0..1000u64, 0..10),
    ) {
        let r = SessionResumer::with_defaults();
        let export = r.export_for_recorder(
            "s1", "test", Path::new("/tmp/x"), vec![], pane_ids.clone(),
        );
        prop_assert_eq!(export.pane_ids, pane_ids);
    }

    // 13. export_for_recorder started_at from first message
    #[test]
    fn export_started_at_from_first_message(ts in 1..i64::MAX) {
        let msgs = vec![CanonicalMessage {
            idx: 0,
            role: MessageRole::User,
            content: "x".into(),
            timestamp: Some(ts),
            author: None,
            tool_calls: vec![],
            tool_results: vec![],
            extra: json!({}),
        }];
        let r = SessionResumer::with_defaults();
        let export = r.export_for_recorder("s", "t", Path::new("/x"), msgs, vec![]);
        prop_assert_eq!(export.session.started_at, Some(ts));
    }

    // 14. RecorderCasrExport serde roundtrip
    #[test]
    fn recorder_export_serde_roundtrip(
        session_id in "[a-z0-9]{1,20}",
        pane_id in 0..1000u64,
    ) {
        let r = SessionResumer::with_defaults();
        let export = r.export_for_recorder(
            &session_id, "test", Path::new("/tmp/x"), vec![], vec![pane_id],
        );
        let json_str = serde_json::to_string(&export).unwrap();
        let rt: RecorderCasrExport = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(&rt.session.session_id, &session_id);
        prop_assert_eq!(rt.pane_ids, vec![pane_id]);
    }

    // 15. SessionResumeError display contains relevant info
    #[test]
    fn error_display_contains_message(msg in "[a-z ]{1,30}") {
        let e = SessionResumeError::CasrNotFound(msg.clone());
        prop_assert!(e.to_string().contains(&msg));
    }

    // 16. SessionResumeError::SubprocessFailed includes exit code
    #[test]
    fn error_subprocess_includes_code(code in 1..127i32) {
        let e = SessionResumeError::SubprocessFailed {
            code: Some(code),
            stderr: "fail".into(),
        };
        let display = e.to_string();
        let code_str = code.to_string();
        prop_assert!(display.contains(&code_str));
    }

    // 17. provider_from_list_entry maps known slugs
    #[test]
    fn provider_from_entry_known(entry in arb_list_entry()) {
        let provider = provider_from_list_entry(&entry);
        match &entry.provider {
            Some(slug) => {
                let expected = AgentProvider::from_slug(slug);
                prop_assert_eq!(provider, expected);
            }
            None => {
                prop_assert_eq!(provider, AgentProvider::Other("unknown".into()));
            }
        }
    }

    // 18. summarize_entry contains session_id
    #[test]
    fn summarize_contains_session_id(entry in arb_list_entry()) {
        let summary = summarize_entry(&entry);
        prop_assert!(summary.contains(&entry.session_id));
    }

    // 19. summarize_entry contains message count
    #[test]
    fn summarize_contains_msg_count(entry in arb_list_entry()) {
        let summary = summarize_entry(&entry);
        let count_str = format!("{} msgs", entry.messages);
        prop_assert!(summary.contains(&count_str));
    }

    // 20. discover_sessions_failopen never panics
    #[test]
    fn failopen_never_panics(suffix in "[a-z]{3,10}") {
        let config = SessionResumeConfig {
            casr_binary: format!("/nonexistent-{}", suffix),
            ..Default::default()
        };
        let result = discover_sessions_failopen(&config);
        prop_assert!(result.is_empty());
    }

    // 21. AgentProvider from_slug known aliases
    #[test]
    fn agent_provider_alias_cc(_dummy in 0..1u8) {
        prop_assert_eq!(AgentProvider::from_slug("cc"), AgentProvider::ClaudeCode);
        prop_assert_eq!(AgentProvider::from_slug("cod"), AgentProvider::Codex);
        prop_assert_eq!(AgentProvider::from_slug("gmi"), AgentProvider::Gemini);
    }

    // 22. AgentProvider Other slug is preserved
    #[test]
    fn agent_provider_other_slug_preserved(s in "[a-z]{5,20}") {
        prop_assume!(s != "claude" && s != "codex" && s != "gemini" && s != "grok");
        let p = AgentProvider::from_slug(&s);
        if let AgentProvider::Other(ref inner) = p {
            prop_assert_eq!(inner, &s);
        }
    }

    // 23. SessionResumer config() returns what was provided
    #[test]
    fn resumer_config_matches(config in arb_config()) {
        let dry_run = config.dry_run;
        let timeout = config.timeout_secs;
        let r = SessionResumer::new(config);
        prop_assert_eq!(r.config().dry_run, dry_run);
        prop_assert_eq!(r.config().timeout_secs, timeout);
    }

    // 24. export warnings start empty
    #[test]
    fn export_warnings_start_empty(
        id in "[a-z]{1,10}",
        slug in "[a-z]{1,10}",
    ) {
        let r = SessionResumer::with_defaults();
        let export = r.export_for_recorder(&id, &slug, Path::new("/x"), vec![], vec![]);
        prop_assert!(export.warnings.is_empty());
    }

    // 25. export exported_at is positive (epoch ms)
    #[test]
    fn export_exported_at_positive(id in "[a-z]{1,10}") {
        let r = SessionResumer::with_defaults();
        let export = r.export_for_recorder(&id, "t", Path::new("/x"), vec![], vec![]);
        prop_assert!(export.exported_at > 0);
    }

    // 26. export with no messages has None started_at/ended_at
    #[test]
    fn export_empty_no_timestamps(id in "[a-z]{1,10}") {
        let r = SessionResumer::with_defaults();
        let export = r.export_for_recorder(&id, "t", Path::new("/x"), vec![], vec![]);
        prop_assert!(export.session.started_at.is_none());
        prop_assert!(export.session.ended_at.is_none());
    }

    // 27. SessionResumeError is Send + Sync
    #[test]
    fn error_is_send_sync(_dummy in 0..1u8) {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SessionResumeError>();
    }

    // 28. AgentProvider hash is consistent
    #[test]
    fn agent_provider_hash_consistent(provider in arb_agent_provider()) {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        provider.hash(&mut h1);
        provider.hash(&mut h2);
        prop_assert_eq!(h1.finish(), h2.finish());
    }

    // 29. resume_session with missing binary returns CasrNotFound
    #[test]
    fn resume_missing_binary_returns_not_found(suffix in "[a-z]{3,10}") {
        let r = SessionResumer::new(SessionResumeConfig {
            casr_binary: format!("/nonexistent-{}", suffix),
            ..Default::default()
        });
        let result = r.resume_session("s1", &AgentProvider::Codex);
        prop_assert!(result.is_err());
    }

    // 30. export source_path preserved
    #[test]
    fn export_source_path_preserved(path_str in "[a-z/]{1,30}") {
        let r = SessionResumer::with_defaults();
        let source = PathBuf::from(&path_str);
        let export = r.export_for_recorder(
            "s", "t", &source, vec![], vec![],
        );
        prop_assert_eq!(export.session.source_path, source);
    }
}
