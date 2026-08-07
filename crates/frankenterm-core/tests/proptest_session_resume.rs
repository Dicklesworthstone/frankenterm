//! Property-based tests for `session_resume` — CASR bridge orchestrator.
//!
//! Requires `--features session-resume`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use serde_json::json;

use frankenterm_core::casr_types::*;
use frankenterm_core::runtime_async::process::{CommandCleanupTrigger, CommandOutputStream};
use frankenterm_core::session_resume::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_agent_provider() -> impl Strategy<Value = AgentProvider> {
    prop_oneof![
        Just(AgentProvider::ClaudeCode),
        Just(AgentProvider::Codex),
        Just(AgentProvider::Gemini),
        Just(AgentProvider::Antigravity),
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

fn arb_antigravity_uuid() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
        .expect("valid Antigravity UUID regex")
}

fn arb_cleanup_trigger() -> impl Strategy<Value = CommandCleanupTrigger> {
    prop_oneof![
        Just(CommandCleanupTrigger::Cancelled),
        Just(CommandCleanupTrigger::TimedOut),
        Just(CommandCleanupTrigger::CaptureLimit(
            CommandOutputStream::Stdout
        )),
        Just(CommandCleanupTrigger::CaptureLimit(
            CommandOutputStream::Stderr
        )),
        Just(CommandCleanupTrigger::CaptureRead),
        Just(CommandCleanupTrigger::StdinWrite),
        Just(CommandCleanupTrigger::ReadinessPoll),
        Just(CommandCleanupTrigger::StatusProbe),
    ]
}

fn path_has_component(path: &str, component: &str) -> bool {
    Path::new(path)
        .components()
        .any(|part| part.as_os_str().to_str() == Some(component))
}

fn log_agy_contract_scenario(
    scenario_id: &str,
    surface: &str,
    entry: &CasrListEntry,
    command_argv: Option<Vec<String>>,
    fallback_reason: Option<&str>,
) {
    eprintln!(
        "{}",
        json!({
            "bead_id": "ft-agy-provider-q8o4y-685af.2",
            "scenario_id": scenario_id,
            "surface": surface,
            "provider": entry.provider.as_deref().unwrap_or("<none>"),
            "session_id": entry.session_id,
            "source_path": entry.path.as_deref(),
            "command_argv": command_argv,
            "exit_code": null,
            "error_code": null,
            "fallback_reason": fallback_reason,
        })
    );
}

#[test]
fn antigravity_discovery_lists_only_conversation_db_files() {
    let temp = tempfile::tempdir().expect("temp home");
    let home = temp.path();
    let conversations = antigravity_conversations_dir(home);
    fs::create_dir_all(&conversations).expect("create agy conversations dir");

    let conversation_id = "123e4567-e89b-12d3-a456-426614174000";
    fs::write(
        conversations.join(format!("{conversation_id}.db")),
        b"sqlite",
    )
    .expect("write agy db fixture");
    fs::write(conversations.join("not-a-conversation.sqlite"), b"sqlite")
        .expect("write non-db fixture");
    fs::write(conversations.join("not-a-uuid.db"), b"sqlite")
        .expect("write invalid db-name fixture");
    fs::create_dir_all(conversations.join("directory.db")).expect("create db-named directory");

    let report = discover_antigravity_conversations_from_home(home)
        .expect("bounded native discovery succeeds");
    let entries = report.entries;

    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.session_id, conversation_id);
    assert_eq!(entry.provider.as_deref(), Some("agy"));
    assert_eq!(
        entry.title.as_deref(),
        Some("Antigravity conversation (metadata schema not read)")
    );
    assert_eq!(provider_from_list_entry(entry), AgentProvider::Antigravity);
    assert_eq!(
        entry.extra.get("discovery_source"),
        Some(&json!(ANTIGRAVITY_DISCOVERY_SOURCE))
    );
    assert_eq!(
        entry.extra.get("model_name"),
        Some(&json!(ANTIGRAVITY_MODEL))
    );
    assert_eq!(
        entry.extra.get("native_resume_binary"),
        Some(&json!(ANTIGRAVITY_BINARY))
    );
    assert_eq!(entry.extra.get("provider_slug"), Some(&json!("agy")));
    assert_eq!(
        entry.extra.get("conversation_id"),
        Some(&json!(conversation_id))
    );
    assert_eq!(
        entry.extra.get("metadata_fallback_reason"),
        Some(&json!(ANTIGRAVITY_METADATA_FALLBACK_REASON))
    );
    assert_eq!(
        entry.extra.get("native_resume_command"),
        Some(&json!([
            "agy",
            "--conversation",
            conversation_id,
            "--model",
            ANTIGRAVITY_MODEL
        ]))
    );
    log_agy_contract_scenario(
        "agy-only",
        "session_resume.discovery",
        entry,
        AgentProvider::Antigravity.native_resume_command(conversation_id),
        Some(ANTIGRAVITY_METADATA_FALLBACK_REASON),
    );
}

#[test]
fn antigravity_discovery_is_disjoint_from_legacy_gemini_tmp_chats() {
    let temp = tempfile::tempdir().expect("temp home");
    let home = temp.path();
    let conversations = antigravity_conversations_dir(home);
    fs::create_dir_all(&conversations).expect("create agy conversations dir");
    fs::write(
        conversations.join("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.db"),
        b"sqlite",
    )
    .expect("write agy db fixture");

    let legacy_gmi_chats = home
        .join(".gemini")
        .join("tmp")
        .join("legacy-hash")
        .join("chats");
    fs::create_dir_all(&legacy_gmi_chats).expect("create legacy gmi chats dir");
    fs::write(legacy_gmi_chats.join("session-legacy.json"), b"{}")
        .expect("write legacy gmi fixture");

    let report = discover_antigravity_conversations_from_home(home)
        .expect("bounded native discovery succeeds");
    let entries = report.entries;

    assert_eq!(entries.len(), 1);
    let discovered_path = entries[0].path.as_deref().expect("agy entry path");
    assert!(discovered_path.contains("antigravity-cli"));
    assert!(!discovered_path.contains("legacy-hash"));
    assert!(!entries.iter().any(|entry| {
        entry
            .path
            .as_deref()
            .is_some_and(|path| path.contains("session-legacy.json"))
    }));
}

#[test]
fn antigravity_e2e_fixture_merges_casr_and_native_sessions_without_cross_listing() {
    let temp = tempfile::tempdir().expect("temp home");
    let home = temp.path();

    let conversation_id = "123e4567-e89b-12d3-a456-426614174000";
    let conversations = antigravity_conversations_dir(home);
    fs::create_dir_all(&conversations).expect("create agy conversations dir");
    fs::write(
        conversations.join(format!("{conversation_id}.db")),
        b"sqlite",
    )
    .expect("write agy db fixture");

    let legacy_session_id = "session-legacy";
    let legacy_gmi_chats = home
        .join(".gemini")
        .join("tmp")
        .join("legacy-hash")
        .join("chats");
    fs::create_dir_all(&legacy_gmi_chats).expect("create legacy gmi chats dir");
    let legacy_gmi_path = legacy_gmi_chats.join(format!("{legacy_session_id}.json"));
    fs::write(&legacy_gmi_path, b"{}").expect("write legacy gmi fixture");

    let mut report = discover_antigravity_conversations_from_home(home)
        .expect("bounded native discovery succeeds");
    merge_session_discovery_entries(
        &mut report,
        vec![CasrListEntry {
            session_id: legacy_session_id.to_string(),
            provider: Some("gemini".to_string()),
            title: Some("legacy Gemini CLI session".to_string()),
            messages: 1,
            workspace: None,
            started_at: None,
            path: Some(legacy_gmi_path.display().to_string()),
            extra: HashMap::new(),
        }],
    )
    .expect("pure CASR/native merge succeeds");
    let entries = report.entries;

    for entry in &entries {
        let provider = provider_from_list_entry(entry);
        let resume_command = provider.native_resume_command(&entry.session_id);
        log_agy_contract_scenario(
            "mixed-agy-gmi",
            "session_resume.discovery",
            entry,
            resume_command,
            entry
                .extra
                .get("metadata_fallback_reason")
                .and_then(|value| value.as_str()),
        );
    }

    assert_eq!(entries.len(), 2);
    let agy_entry = entries
        .iter()
        .find(|entry| provider_from_list_entry(entry) == AgentProvider::Antigravity)
        .expect("agy entry discovered");
    let legacy_gmi_entry = entries
        .iter()
        .find(|entry| provider_from_list_entry(entry) == AgentProvider::Gemini)
        .expect("legacy gmi entry preserved from casr");

    assert_eq!(agy_entry.session_id, conversation_id);
    assert_eq!(agy_entry.provider.as_deref(), Some("agy"));
    let agy_path = agy_entry.path.as_deref().expect("agy path");
    assert!(path_has_component(agy_path, "antigravity-cli"));
    assert!(agy_path.ends_with(&format!("{conversation_id}.db")));
    assert!(!path_has_component(agy_path, "legacy-hash"));
    assert_eq!(
        agy_entry.extra.get("native_resume_command"),
        Some(&json!([
            "agy",
            "--conversation",
            conversation_id,
            "--model",
            ANTIGRAVITY_MODEL
        ]))
    );

    let assembled_resume_command = AgentProvider::Antigravity
        .native_resume_command(conversation_id)
        .expect("agy resume command");
    eprintln!(
        "{}",
        json!({
            "bead_id": "ft-agy-provider-q8o4y-685af.2",
            "scenario_id": "mixed-agy-gmi",
            "surface": "session_resume.resume_plan",
            "provider": "agy",
            "session_id": conversation_id,
            "source_path": agy_path,
            "command_argv": assembled_resume_command.clone(),
            "exit_code": null,
            "error_code": null,
            "fallback_reason": null,
        })
    );
    assert_eq!(
        assembled_resume_command,
        vec![
            "agy".to_string(),
            "--conversation".to_string(),
            conversation_id.to_string(),
            "--model".to_string(),
            ANTIGRAVITY_MODEL.to_string(),
        ]
    );

    assert_eq!(legacy_gmi_entry.session_id, legacy_session_id);
    assert_eq!(legacy_gmi_entry.provider.as_deref(), Some("gemini"));
    let legacy_path = legacy_gmi_entry.path.as_deref().expect("legacy gmi path");
    assert!(path_has_component(legacy_path, ".gemini"));
    assert!(path_has_component(legacy_path, "tmp"));
    assert!(path_has_component(legacy_path, "legacy-hash"));
    assert!(legacy_path.ends_with(&format!("{legacy_session_id}.json")));
    assert!(!path_has_component(legacy_path, "antigravity-cli"));
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
            slug != "claude"
                && slug != "codex"
                && slug != "gemini"
                && slug != "grok"
                && slug != "antigravity"
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

    // 15. SessionResumeError display does not retain untrusted content
    #[test]
    fn error_display_is_content_free(msg in "[QWXZ]{32,64}") {
        let e = SessionResumeError::InvalidSessionIdentifier {
            input_bytes: msg.len(),
        };
        prop_assert!(!e.to_string().contains(&msg));
        prop_assert!(e.to_string().contains(&msg.len().to_string()));
    }

    // 16. SessionResumeError::SubprocessFailed includes exit code
    #[test]
    fn error_subprocess_includes_code(code in 1..127i32) {
        let e = SessionResumeError::SubprocessFailed {
            code: Some(code),
        };
        let display = e.to_string();
        let code_str = code.to_string();
        prop_assert!(display.contains(&code_str));
    }

    // 16a. Cooperative cancellation remains distinct from timeout.
    #[test]
    fn error_cancelled_has_stable_structural_display(_content in "[QWXZ]{32,64}") {
        let display = SessionResumeError::Cancelled.to_string();
        prop_assert_eq!(display, "casr operation cancelled");
    }

    // 16b. Incomplete capture exposes only bounded structural detail.
    #[test]
    fn error_capture_incomplete_has_structural_display(
        stdout_open in any::<bool>(),
        stderr_open in any::<bool>(),
        drain_timeout_ms in 0_u64..60_000,
    ) {
        let display = SessionResumeError::CaptureIncomplete {
            stdout_open,
            stderr_open,
            drain_timeout_ms,
        }
        .to_string();
        prop_assert_eq!(
            display,
            format!(
                "casr output capture incomplete after {drain_timeout_ms} ms (stdout_open={stdout_open}, stderr_open={stderr_open})"
            )
        );
    }

    // 16c. Failed cleanup preserves only structural process state.
    #[test]
    fn error_cleanup_incomplete_has_structural_display(
        trigger in arb_cleanup_trigger(),
        leader_reaped in any::<bool>(),
        signal_helper_settled in any::<bool>(),
        process_tree_signalled in any::<bool>(),
        stdout_open in any::<bool>(),
        stderr_open in any::<bool>(),
        settle_timeout_ms in 0_u64..60_000,
    ) {
        let display = SessionResumeError::CleanupIncomplete {
            trigger,
            leader_reaped,
            signal_helper_settled,
            process_tree_signalled,
            stdout_open,
            stderr_open,
            settle_timeout_ms,
        }
        .to_string();
        prop_assert_eq!(
            display,
            format!(
                "casr process cleanup incomplete after {settle_timeout_ms} ms (trigger={trigger}, leader_reaped={leader_reaped}, signal_helper_settled={signal_helper_settled}, process_tree_signalled={process_tree_signalled}, stdout_open={stdout_open}, stderr_open={stderr_open})"
            )
        );
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
        prop_assert!(result.entries.is_empty());
        prop_assert!(!result.is_complete());
    }

    // 21. AgentProvider from_slug known aliases
    #[test]
    fn agent_provider_alias_cc(_dummy in 0..1u8) {
        prop_assert_eq!(AgentProvider::from_slug("cc"), AgentProvider::ClaudeCode);
        prop_assert_eq!(AgentProvider::from_slug("cod"), AgentProvider::Codex);
        prop_assert_eq!(AgentProvider::from_slug("gmi"), AgentProvider::Gemini);
        prop_assert_eq!(AgentProvider::from_slug("gemini"), AgentProvider::Gemini);
        prop_assert_eq!(AgentProvider::from_slug("agy"), AgentProvider::Antigravity);
        prop_assert_eq!(
            AgentProvider::from_slug("antigravity"),
            AgentProvider::Antigravity
        );
        prop_assert_eq!(
            AgentProvider::from_slug("antigravity-cli"),
            AgentProvider::Antigravity
        );
    }

    // 22. AgentProvider Other slug is preserved
    #[test]
    fn agent_provider_other_slug_preserved(s in "[a-z]{5,20}") {
        prop_assume!(
            s != "claude"
                && s != "codex"
                && s != "gemini"
                && s != "grok"
                && s != "antigravity"
        );
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

    // 31. Antigravity native resume command is model pinned
    #[test]
    fn antigravity_native_resume_command_is_model_pinned(session_id in arb_antigravity_uuid()) {
        let command = AgentProvider::Antigravity
            .native_resume_command(&session_id)
            .expect("antigravity must have a native resume command");
        prop_assert_eq!(
            command,
            vec![
                "agy".to_string(),
                "--conversation".to_string(),
                session_id,
                "--model".to_string(),
                ANTIGRAVITY_MODEL.to_string(),
            ]
        );
    }

    // 32. Antigravity checked resume plan never permits non-pinned models
    #[test]
    fn antigravity_checked_resume_rejects_non_pinned_model(
        session_id in arb_antigravity_uuid(),
        model in "[A-Za-z0-9 .()_-]{1,40}",
    ) {
        prop_assume!(model != ANTIGRAVITY_MODEL);
        let err = antigravity_native_resume_plan_with_model(&session_id, &model)
            .expect_err("Antigravity must reject any non-pinned model");
        let is_non_pinned_antigravity_model_error = matches!(
            &err,
            SessionResumeError::NonPinnedNativeModel {
                provider_slug,
                required_model,
                ..
            } if provider_slug.as_str() == "agy" && required_model.as_str() == ANTIGRAVITY_MODEL
        );
        prop_assert!(is_non_pinned_antigravity_model_error);
    }

    // 33. Non-Antigravity providers do not get Antigravity's pinned command
    #[test]
    fn only_antigravity_has_native_resume_command(provider in prop_oneof![
        Just(AgentProvider::ClaudeCode),
        Just(AgentProvider::Codex),
        Just(AgentProvider::Gemini),
        Just(AgentProvider::Grok),
    ]) {
        prop_assert!(provider.native_resume_command("session").is_none());
    }
}
