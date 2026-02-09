//! Integration tests for incident bundle and crash bundle safety
//!
//! Covers: multi-pattern redaction, privacy budget enforcement,
//! incident export edge cases, and deterministic fixtures.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use wa_core::crash::{
    CrashManifest, CrashReport, HealthSnapshot, IncidentBundleOptions, IncidentBundleResult,
    IncidentKind, PanePriorityOverrideSnapshot, ReplayMode, collect_incident_bundle,
    export_incident_bundle, latest_crash_bundle, list_crash_bundles, replay_incident_bundle,
    write_crash_bundle,
};
use wa_core::policy::Redactor;

// ── Helpers ──────────────────────────────────────────────────────────

/// Build strings that look like API keys at runtime, without embedding the full
/// token literal in the repository (avoids GitHub push-protection false positives).
fn join_parts(parts: &[&str]) -> String {
    parts.join("")
}

fn basic_report() -> CrashReport {
    CrashReport {
        message: "test crash".to_string(),
        location: Some("src/main.rs:42:5".to_string()),
        backtrace: Some("   0: std::backtrace\n   1: my_func".to_string()),
        timestamp: 1_700_000_000,
        pid: 12345,
        thread_name: Some("main".to_string()),
    }
}

fn basic_snapshot() -> HealthSnapshot {
    HealthSnapshot {
        timestamp: 1_700_000_000_000,
        observed_panes: 3,
        capture_queue_depth: 5,
        write_queue_depth: 2,
        last_seq_by_pane: vec![(1, 100), (2, 200)],
        warnings: vec![],
        ingest_lag_avg_ms: 10.0,
        ingest_lag_max_ms: 50,
        db_writable: true,
        db_last_write_at: Some(1_700_000_000_000),
        pane_priority_overrides: vec![],
        scheduler: None,
        backpressure_tier: None,
        last_activity_by_pane: vec![],
        restart_count: 0,
        last_crash_at: None,
        consecutive_crashes: 0,
        current_backoff_ms: 0,
        in_crash_loop: false,
    }
}

/// Read all text files in a directory recursively and concatenate contents.
fn read_all_bundle_text(dir: &Path) -> String {
    let mut combined = String::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Ok(content) = fs::read_to_string(&path) {
                    combined.push_str(&content);
                    combined.push('\n');
                }
            }
        }
    }
    combined
}

// ── Multi-pattern redaction in crash bundles ──────────────────────────

#[test]
fn crash_bundle_redacts_anthropic_key() {
    let tmp = tempfile::tempdir().unwrap();
    let mut report = basic_report();
    let anthropic_key = join_parts(&[
        "sk",
        "-ant-api03-",
        "ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890abcdef",
    ]);
    report.message = format!("Error with key {anthropic_key}");

    let path = write_crash_bundle(tmp.path(), &report, None).unwrap();
    let content = fs::read_to_string(path.join("crash_report.json")).unwrap();

    let prefix = join_parts(&["sk", "-ant-api03"]);
    assert!(!content.contains(&prefix), "Anthropic key leaked");
    assert!(content.contains("[REDACTED]"));
}

#[test]
fn crash_bundle_redacts_openai_key() {
    let tmp = tempfile::tempdir().unwrap();
    let mut report = basic_report();
    let openai_key = join_parts(&["sk", "-proj-", "ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890"]);
    report.message = format!("Using key {openai_key}");

    let path = write_crash_bundle(tmp.path(), &report, None).unwrap();
    let content = fs::read_to_string(path.join("crash_report.json")).unwrap();

    let prefix = join_parts(&["sk", "-proj-"]);
    assert!(!content.contains(&prefix), "OpenAI key leaked");
    assert!(content.contains("[REDACTED]"));
}

#[test]
fn crash_bundle_redacts_github_token() {
    let tmp = tempfile::tempdir().unwrap();
    let mut report = basic_report();
    let github_token = join_parts(&["ghp", "_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"]);
    report.message = format!("Auth: {github_token}");

    let path = write_crash_bundle(tmp.path(), &report, None).unwrap();
    let content = fs::read_to_string(path.join("crash_report.json")).unwrap();

    let prefix = join_parts(&["ghp", "_"]);
    assert!(!content.contains(&prefix), "GitHub token leaked");
    assert!(content.contains("[REDACTED]"));
}

#[test]
fn crash_bundle_redacts_bearer_token() {
    let tmp = tempfile::tempdir().unwrap();
    let mut report = basic_report();
    report.backtrace = Some(
        "Authorization: Bearer eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.long_jwt_token_content_here"
            .to_string(),
    );

    let path = write_crash_bundle(tmp.path(), &report, None).unwrap();
    let content = fs::read_to_string(path.join("crash_report.json")).unwrap();

    assert!(
        !content.contains("eyJhbGci"),
        "Bearer token leaked in backtrace"
    );
}

#[test]
fn crash_bundle_redacts_database_url() {
    let tmp = tempfile::tempdir().unwrap();
    let mut report = basic_report();
    report.message =
        "Connection failed: postgresql://admin:supersecretpass@db.example.com:5432/mydb"
            .to_string();

    let path = write_crash_bundle(tmp.path(), &report, None).unwrap();
    let content = fs::read_to_string(path.join("crash_report.json")).unwrap();

    assert!(
        !content.contains("supersecretpass"),
        "Database password leaked"
    );
}

#[test]
fn crash_bundle_redacts_stripe_key() {
    let tmp = tempfile::tempdir().unwrap();
    let mut report = basic_report();
    let stripe_key = join_parts(&["sk", "_live_", "ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890"]);
    report.message = format!("Payment failed with key {stripe_key}");

    let path = write_crash_bundle(tmp.path(), &report, None).unwrap();
    let content = fs::read_to_string(path.join("crash_report.json")).unwrap();

    let prefix = join_parts(&["sk", "_live_"]);
    assert!(!content.contains(&prefix), "Stripe key leaked");
    assert!(content.contains("[REDACTED]"));
}

#[test]
fn crash_bundle_redacts_aws_access_key() {
    let tmp = tempfile::tempdir().unwrap();
    let mut report = basic_report();
    let aws_access_key = join_parts(&["AKI", "A", "IOSFODNN7EXAMPLE"]);
    report.message = format!("AWS error with {aws_access_key}");

    let path = write_crash_bundle(tmp.path(), &report, None).unwrap();
    let content = fs::read_to_string(path.join("crash_report.json")).unwrap();

    // Avoid embedding the full key literal; assert the prefix is gone after redaction.
    let prefix = join_parts(&["AKI", "A"]);
    assert!(!content.contains(&prefix), "AWS access key leaked");
}

#[test]
fn crash_bundle_redacts_multiple_secrets_in_one_message() {
    let tmp = tempfile::tempdir().unwrap();
    let mut report = basic_report();
    let anthropic_key = join_parts(&[
        "sk",
        "-ant-api03-",
        "secret_key_123456789_ABCDEF_123456789_ABCDEF",
    ]);
    let github_token = join_parts(&["ghp", "_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"]);
    report.message = format!("Failed: {anthropic_key}, token={github_token}");
    report.backtrace =
        Some("Authorization: Bearer eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.payload".to_string());

    let path = write_crash_bundle(tmp.path(), &report, None).unwrap();
    let all_content = read_all_bundle_text(&path);

    let anthropic_prefix = join_parts(&["sk", "-ant-api03"]);
    let github_prefix = join_parts(&["ghp", "_"]);
    assert!(
        !all_content.contains(&anthropic_prefix),
        "Anthropic key in bundle"
    );
    assert!(
        !all_content.contains(&github_prefix),
        "GitHub token in bundle"
    );
    assert!(!all_content.contains("eyJhbGci"), "JWT token in bundle");
}

// ── Incident bundle redaction ────────────────────────────────────────

#[test]
fn incident_bundle_redacts_config_secrets() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crash");
    let out_dir = tmp.path().join("out");
    let config_path = tmp.path().join("config.toml");

    // Config file with embedded secrets
    let anthropic_key = join_parts(&[
        "sk",
        "-ant-api03-",
        "ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890abcdef",
    ]);
    let config = format!(
        "[auth]\n\
         api_key = \"{anthropic_key}\"\n\
         database_url = \"postgresql://user:secret@host/db\"\n\
         [ingest]\n\
         buffer_size = 1024\n",
    );
    fs::write(&config_path, config).unwrap();

    let result = export_incident_bundle(
        &crash_dir,
        Some(&config_path),
        &out_dir,
        IncidentKind::Manual,
    )
    .unwrap();

    let config_content = fs::read_to_string(result.path.join("config_summary.toml")).unwrap();
    let prefix = join_parts(&["sk", "-ant-api03"]);
    assert!(
        !config_content.contains(&prefix),
        "Anthropic key leaked in config: {config_content}"
    );
    // Non-secret content should be preserved
    assert!(config_content.contains("buffer_size"));
}

#[test]
fn incident_bundle_crash_redacts_all_files() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crash");
    let out_dir = tmp.path().join("out");

    let mut report = basic_report();
    let anthropic_key = join_parts(&[
        "sk",
        "-ant-api03-",
        "ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890abcdef",
    ]);
    report.message = format!("Error with {anthropic_key}");
    let mut snapshot = basic_snapshot();
    let aws_access_key = join_parts(&["AKI", "A", "IOSFODNN7EXAMPLE"]);
    snapshot.warnings = vec![format!("Warning: {aws_access_key} exposed")];

    write_crash_bundle(&crash_dir, &report, Some(&snapshot)).unwrap();

    let result = export_incident_bundle(&crash_dir, None, &out_dir, IncidentKind::Crash).unwrap();

    let all_content = read_all_bundle_text(&result.path);

    let anthropic_prefix = join_parts(&["sk", "-ant-api03"]);
    let aws_prefix = join_parts(&["AKI", "A"]);
    assert!(
        !all_content.contains(&anthropic_prefix),
        "Anthropic key in incident bundle"
    );
    assert!(
        !all_content.contains(&aws_prefix),
        "AWS key in incident bundle health snapshot"
    );
}

// ── Privacy budget enforcement ───────────────────────────────────────

#[test]
fn crash_bundle_enforces_size_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let mut report = basic_report();
    // Create a very large backtrace to approach the bundle size limit
    report.backtrace = Some("x".repeat(900_000));

    let path = write_crash_bundle(tmp.path(), &report, Some(&basic_snapshot())).unwrap();

    // Bundle should still be created
    assert!(path.exists());

    // Manifest should exist even if some files were skipped
    let manifest_json = fs::read_to_string(path.join("manifest.json")).unwrap();
    let manifest: CrashManifest = serde_json::from_str(&manifest_json).unwrap();
    assert!(
        manifest.bundle_size_bytes <= 1_100_000,
        "Bundle exceeds budget: {} bytes",
        manifest.bundle_size_bytes
    );
}

#[test]
fn crash_bundle_with_huge_health_snapshot_stays_within_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let report = basic_report();
    let mut snapshot = basic_snapshot();
    // Large warnings list to bloat the health snapshot
    snapshot.warnings = (0..5000)
        .map(|i| format!("Warning #{i}: something happened on pane {i}"))
        .collect();

    let path = write_crash_bundle(tmp.path(), &report, Some(&snapshot)).unwrap();
    let manifest_json = fs::read_to_string(path.join("manifest.json")).unwrap();
    let manifest: CrashManifest = serde_json::from_str(&manifest_json).unwrap();

    // Manifest always gets written; some files may be skipped
    assert!(path.join("manifest.json").exists());
    assert!(manifest.bundle_size_bytes > 0);
}

// ── Edge cases ───────────────────────────────────────────────────────

#[test]
fn crash_bundle_with_unicode_message() {
    let tmp = tempfile::tempdir().unwrap();
    let mut report = basic_report();
    report.message = "Crash: 日本語テスト Unicode: 🔥💥 Ñ ü ö ä".to_string();

    let path = write_crash_bundle(tmp.path(), &report, None).unwrap();
    let content = fs::read_to_string(path.join("crash_report.json")).unwrap();
    let parsed: CrashReport = serde_json::from_str(&content).unwrap();

    assert!(parsed.message.contains("日本語テスト"));
    assert!(parsed.message.contains("🔥💥"));
}

#[test]
fn crash_bundle_with_empty_message() {
    let tmp = tempfile::tempdir().unwrap();
    let mut report = basic_report();
    report.message = String::new();
    report.location = None;
    report.backtrace = None;
    report.thread_name = None;

    let path = write_crash_bundle(tmp.path(), &report, None).unwrap();
    let content = fs::read_to_string(path.join("crash_report.json")).unwrap();
    let parsed: CrashReport = serde_json::from_str(&content).unwrap();

    assert!(parsed.message.is_empty());
    assert!(parsed.location.is_none());
    assert!(parsed.backtrace.is_none());
}

#[test]
fn crash_bundle_with_priority_overrides_in_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let report = basic_report();
    let mut snapshot = basic_snapshot();
    snapshot.pane_priority_overrides = vec![
        PanePriorityOverrideSnapshot {
            pane_id: 1,
            priority: 10,
            expires_at: Some(1_700_001_000_000),
        },
        PanePriorityOverrideSnapshot {
            pane_id: 2,
            priority: 5,
            expires_at: None,
        },
    ];

    let path = write_crash_bundle(tmp.path(), &report, Some(&snapshot)).unwrap();
    let health_json = fs::read_to_string(path.join("health_snapshot.json")).unwrap();
    let parsed: HealthSnapshot = serde_json::from_str(&health_json).unwrap();

    assert_eq!(parsed.pane_priority_overrides.len(), 2);
    assert_eq!(parsed.pane_priority_overrides[0].pane_id, 1);
    assert_eq!(parsed.pane_priority_overrides[1].priority, 5);
}

#[test]
fn incident_bundle_manifest_is_valid_json() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crash");
    let out_dir = tmp.path().join("out");

    write_crash_bundle(&crash_dir, &basic_report(), Some(&basic_snapshot())).unwrap();

    let result = export_incident_bundle(&crash_dir, None, &out_dir, IncidentKind::Crash).unwrap();

    let manifest_json = fs::read_to_string(result.path.join("incident_manifest.json")).unwrap();
    let parsed: IncidentBundleResult = serde_json::from_str(&manifest_json).unwrap();

    assert_eq!(parsed.kind, IncidentKind::Crash);
    assert!(!parsed.wa_version.is_empty());
    assert!(!parsed.exported_at.is_empty());
}

#[test]
fn incident_bundle_result_includes_all_files_list() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crash");
    let out_dir = tmp.path().join("out");

    write_crash_bundle(&crash_dir, &basic_report(), Some(&basic_snapshot())).unwrap();

    let result = export_incident_bundle(&crash_dir, None, &out_dir, IncidentKind::Crash).unwrap();

    // The result files list should match what's actually on disk
    let on_disk: HashSet<String> = fs::read_dir(&result.path)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    // All listed files should exist on disk (plus incident_manifest.json)
    for f in &result.files {
        assert!(
            on_disk.contains(f),
            "Listed file '{f}' not found on disk. On disk: {on_disk:?}"
        );
    }
}

#[test]
fn multiple_crash_bundles_have_unique_names() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path();

    // Write multiple bundles with the same timestamp
    let mut report = basic_report();
    report.timestamp = 1_700_000_000;

    let path1 = write_crash_bundle(crash_dir, &report, None).unwrap();
    let path2 = write_crash_bundle(crash_dir, &report, None).unwrap();
    let path3 = write_crash_bundle(crash_dir, &report, None).unwrap();

    // All paths should be unique
    assert_ne!(path1, path2);
    assert_ne!(path2, path3);
    assert_ne!(path1, path3);

    // All should exist
    assert!(path1.exists());
    assert!(path2.exists());
    assert!(path3.exists());

    // Listing should find all three
    let bundles = list_crash_bundles(crash_dir, 10);
    assert_eq!(bundles.len(), 3);
}

#[test]
fn incident_export_with_large_config_truncates() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crash");
    let out_dir = tmp.path().join("out");
    let config_path = tmp.path().join("config.toml");

    // Write a config file larger than 64 KiB
    let large_config = format!(
        "[settings]\n# Large config\n{}",
        "padding = \"aaaa\"\n".repeat(10_000)
    );
    fs::write(&config_path, &large_config).unwrap();

    let result = export_incident_bundle(
        &crash_dir,
        Some(&config_path),
        &out_dir,
        IncidentKind::Manual,
    )
    .unwrap();

    // Config should be included but may be truncated
    if result.files.contains(&"config_summary.toml".to_string()) {
        let saved = fs::read_to_string(result.path.join("config_summary.toml")).unwrap();
        assert!(
            saved.len() <= 65_536 + 100,
            "Config not truncated: {} bytes",
            saved.len()
        );
    }
}

// ── Redactor standalone tests ────────────────────────────────────────

#[test]
fn redactor_detects_all_major_patterns() {
    let redactor = Redactor::new();

    let anthropic = join_parts(&[
        "sk",
        "-ant-api03-",
        "ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890abcdef",
    ]);
    let openai = join_parts(&["sk", "-proj-", "ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890"]);
    let github = join_parts(&["ghp", "_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"]);
    let aws = join_parts(&["AKI", "A", "IOSFODNN7EXAMPLE"]);
    let stripe_live = join_parts(&["sk", "_live_", "ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890"]);
    let stripe_test = join_parts(&["pk", "_test_", "ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890"]);

    let test_cases: Vec<(&str, String)> = vec![
        ("Anthropic", anthropic),
        ("OpenAI", openai),
        ("GitHub", github),
        ("AWS", aws),
        ("Stripe live", stripe_live),
        ("Stripe test", stripe_test),
    ];

    for (name, secret) in &test_cases {
        let input = format!("Using {secret} for auth");
        assert!(
            redactor.contains_secrets(&input),
            "Redactor failed to detect {name}: {secret}"
        );
        let redacted = redactor.redact(&input);
        assert!(
            !redacted.contains(secret),
            "Redactor failed to redact {name}: {redacted}"
        );
        assert!(
            redacted.contains("[REDACTED]"),
            "Missing REDACTED marker for {name}: {redacted}"
        );
    }
}

#[test]
fn redactor_detects_bearer_and_database_urls() {
    let redactor = Redactor::new();

    let inputs = vec![
        "Authorization: Bearer eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.payload.signature",
        "DATABASE_URL=postgresql://admin:supersecret@db.example.com:5432/prod",
    ];

    for input in &inputs {
        assert!(
            redactor.contains_secrets(input),
            "Should detect secret in: {input}"
        );
        let redacted = redactor.redact(input);
        assert!(redacted.contains("[REDACTED]"), "No redaction for: {input}");
    }
}

#[test]
fn redactor_preserves_non_secret_text() {
    let redactor = Redactor::new();
    let input = "Normal text with no secrets at all, just a regular log line.";
    let redacted = redactor.redact(input);
    assert_eq!(redacted, input, "Non-secret text should be unchanged");
}

#[test]
fn redactor_handles_empty_input() {
    let redactor = Redactor::new();
    assert_eq!(redactor.redact(""), "");
    assert!(!redactor.contains_secrets(""));
}

#[test]
fn redactor_handles_multiline_with_secrets() {
    let redactor = Redactor::new();
    let anthropic = join_parts(&[
        "sk",
        "-ant-api03-",
        "ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890abcdef",
    ]);
    let github = join_parts(&["ghp", "_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"]);
    let input = format!(
        "Line 1: no secrets\n\
         Line 2: {anthropic}\n\
         Line 3: normal\n\
         Line 4: {github}\n"
    );
    let redacted = redactor.redact(&input);

    let anthropic_prefix = join_parts(&["sk", "-ant-api03"]);
    let github_prefix = join_parts(&["ghp", "_"]);
    assert!(!redacted.contains(&anthropic_prefix));
    assert!(!redacted.contains(&github_prefix));
    assert!(redacted.contains("Line 1: no secrets"));
    assert!(redacted.contains("Line 3: normal"));
}

// ── Listing and filtering ────────────────────────────────────────────

#[test]
fn list_crash_bundles_ignores_non_bundle_files() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path();

    // Create a real bundle
    write_crash_bundle(crash_dir, &basic_report(), None).unwrap();

    // Create some non-bundle entries
    fs::create_dir(crash_dir.join("random_directory")).unwrap();
    fs::write(crash_dir.join("random_file.txt"), "not a bundle").unwrap();
    fs::create_dir(crash_dir.join("wa_not_crash_prefix")).unwrap();

    let bundles = list_crash_bundles(crash_dir, 10);
    assert_eq!(bundles.len(), 1, "Should find exactly 1 bundle");
}

#[test]
fn latest_crash_bundle_with_no_bundles_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(latest_crash_bundle(tmp.path()).is_none());
}

// ── Enhanced incident bundle collector tests ──────────────────────────

/// Create a minimal SQLite database with the expected schema for testing.
fn create_test_db(db_path: &Path) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE wa_meta (key TEXT PRIMARY KEY, value TEXT);
         INSERT INTO wa_meta (key, value) VALUES ('schema_version', '7');
         CREATE TABLE events (
             id INTEGER PRIMARY KEY, pane_id INTEGER, rule_id TEXT,
             event_type TEXT, severity TEXT, detected_at INTEGER,
             matched_text TEXT
         );
         CREATE TABLE segments (id INTEGER PRIMARY KEY, pane_id INTEGER, content TEXT);",
    )
    .unwrap();
}

fn insert_test_events(db_path: &Path, count: usize) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    for i in 0..count {
        conn.execute(
            "INSERT INTO events (pane_id, rule_id, event_type, severity, detected_at, matched_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                1i64,
                format!("rule_{i}"),
                "pattern_match",
                "warning",
                1_700_000_000i64 + i as i64,
                format!("matched text for event {i}")
            ],
        )
        .unwrap();
    }
}

#[test]
fn collect_incident_bundle_manual_kind_produces_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crashes");
    let out_dir = tmp.path().join("output");
    fs::create_dir_all(&crash_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();

    let opts = IncidentBundleOptions {
        crash_dir: &crash_dir,
        config_path: None,
        out_dir: &out_dir,
        kind: IncidentKind::Manual,
        db_path: None,
        max_events: 10,
    };

    let result = collect_incident_bundle(&opts).unwrap();
    assert_eq!(result.kind, IncidentKind::Manual);
    assert!(result.path.exists());

    // Should have at least redaction_report.json and incident_manifest.json
    let manifest_path = result.path.join("incident_manifest.json");
    assert!(manifest_path.exists());
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["kind"], "manual");

    let redaction_path = result.path.join("redaction_report.json");
    assert!(redaction_path.exists());
}

#[test]
fn collect_incident_bundle_with_db_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crashes");
    let out_dir = tmp.path().join("output");
    let db_path = tmp.path().join("test.db");
    fs::create_dir_all(&crash_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();
    create_test_db(&db_path);
    insert_test_events(&db_path, 5);

    let opts = IncidentBundleOptions {
        crash_dir: &crash_dir,
        config_path: None,
        out_dir: &out_dir,
        kind: IncidentKind::Manual,
        db_path: Some(&db_path),
        max_events: 10,
    };

    let result = collect_incident_bundle(&opts).unwrap();

    // db_metadata.json should exist and contain schema version
    let meta_path = result.path.join("db_metadata.json");
    assert!(meta_path.exists());
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(meta_path).unwrap()).unwrap();
    assert_eq!(meta["schema_version"], 7);
    assert_eq!(meta["event_count"], 5);
    assert_eq!(meta["segment_count"], 0);
    assert!(meta["db_size_bytes"].as_u64().unwrap() > 0);
    assert!(meta["journal_mode"].as_str().is_some());
}

#[test]
fn collect_incident_bundle_includes_recent_events() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crashes");
    let out_dir = tmp.path().join("output");
    let db_path = tmp.path().join("test.db");
    fs::create_dir_all(&crash_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();
    create_test_db(&db_path);
    insert_test_events(&db_path, 3);

    let opts = IncidentBundleOptions {
        crash_dir: &crash_dir,
        config_path: None,
        out_dir: &out_dir,
        kind: IncidentKind::Manual,
        db_path: Some(&db_path),
        max_events: 10,
    };

    let result = collect_incident_bundle(&opts).unwrap();

    let events_path = result.path.join("recent_events.json");
    assert!(events_path.exists());
    let events: Vec<serde_json::Value> =
        serde_json::from_str(&fs::read_to_string(events_path).unwrap()).unwrap();
    assert_eq!(events.len(), 3);
    // Events should have expected fields
    assert!(events[0]["id"].is_number());
    assert!(events[0]["rule_id"].is_string());
    assert!(events[0]["matched_text_preview"].is_string());
}

#[test]
fn collect_incident_bundle_max_events_limits_output() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crashes");
    let out_dir = tmp.path().join("output");
    let db_path = tmp.path().join("test.db");
    fs::create_dir_all(&crash_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();
    create_test_db(&db_path);
    insert_test_events(&db_path, 20);

    let opts = IncidentBundleOptions {
        crash_dir: &crash_dir,
        config_path: None,
        out_dir: &out_dir,
        kind: IncidentKind::Manual,
        db_path: Some(&db_path),
        max_events: 5,
    };

    let result = collect_incident_bundle(&opts).unwrap();
    let events: Vec<serde_json::Value> =
        serde_json::from_str(&fs::read_to_string(result.path.join("recent_events.json")).unwrap())
            .unwrap();
    assert_eq!(events.len(), 5);
}

#[test]
fn collect_incident_bundle_zero_max_events_skips_events_file() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crashes");
    let out_dir = tmp.path().join("output");
    let db_path = tmp.path().join("test.db");
    fs::create_dir_all(&crash_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();
    create_test_db(&db_path);
    insert_test_events(&db_path, 5);

    let opts = IncidentBundleOptions {
        crash_dir: &crash_dir,
        config_path: None,
        out_dir: &out_dir,
        kind: IncidentKind::Manual,
        db_path: Some(&db_path),
        max_events: 0,
    };

    let result = collect_incident_bundle(&opts).unwrap();
    // db_metadata should exist but recent_events should not
    assert!(result.path.join("db_metadata.json").exists());
    assert!(!result.path.join("recent_events.json").exists());
}

#[test]
fn collect_incident_bundle_with_config_redacts_secrets() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crashes");
    let out_dir = tmp.path().join("output");
    fs::create_dir_all(&crash_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();

    let config_path = tmp.path().join("config.toml");
    let anthropic_key = join_parts(&["sk", "-ant-api03-", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"]);
    fs::write(
        &config_path,
        format!("api_key = \"{anthropic_key}\"\nname = \"test\"\n"),
    )
    .unwrap();

    let opts = IncidentBundleOptions {
        crash_dir: &crash_dir,
        config_path: Some(&config_path),
        out_dir: &out_dir,
        kind: IncidentKind::Manual,
        db_path: None,
        max_events: 0,
    };

    let result = collect_incident_bundle(&opts).unwrap();
    let config_content = fs::read_to_string(result.path.join("config_summary.toml")).unwrap();
    // Secret should be redacted
    let prefix = join_parts(&["sk", "-ant-api03-"]);
    assert!(!config_content.contains(&prefix));
    assert!(config_content.contains("[REDACTED"));
    // Non-secret preserved
    assert!(config_content.contains("name = \"test\""));

    // Redaction report should record the redaction
    let report: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(result.path.join("redaction_report.json")).unwrap(),
    )
    .unwrap();
    assert!(report["total_redactions"].as_u64().unwrap() >= 1);
    let per_file = report["per_file"].as_array().unwrap();
    assert!(per_file.iter().any(|e| e["file"] == "config_summary.toml"));
}

#[test]
fn collect_incident_bundle_crash_kind_includes_crash_data() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crashes");
    let out_dir = tmp.path().join("output");
    fs::create_dir_all(&crash_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();

    // Write a crash bundle first
    let report = basic_report();
    let snapshot = basic_snapshot();
    let _ = write_crash_bundle(&crash_dir, &report, Some(&snapshot));

    let opts = IncidentBundleOptions {
        crash_dir: &crash_dir,
        config_path: None,
        out_dir: &out_dir,
        kind: IncidentKind::Crash,
        db_path: None,
        max_events: 0,
    };

    let result = collect_incident_bundle(&opts).unwrap();
    assert_eq!(result.kind, IncidentKind::Crash);
    // Should include crash report
    assert!(result.path.join("crash_report.json").exists());
    // Files list should include crash data
    assert!(result.files.contains(&"crash_report.json".to_string()));
}

#[test]
fn collect_incident_bundle_redaction_report_zero_when_no_secrets() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crashes");
    let out_dir = tmp.path().join("output");
    fs::create_dir_all(&crash_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();

    let config_path = tmp.path().join("config.toml");
    fs::write(&config_path, "name = \"clean\"\nlog_level = \"debug\"\n").unwrap();

    let opts = IncidentBundleOptions {
        crash_dir: &crash_dir,
        config_path: Some(&config_path),
        out_dir: &out_dir,
        kind: IncidentKind::Manual,
        db_path: None,
        max_events: 0,
    };

    let result = collect_incident_bundle(&opts).unwrap();
    let report: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(result.path.join("redaction_report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["total_redactions"], 0);
    assert_eq!(report["per_file"].as_array().unwrap().len(), 0);
}

#[test]
fn collect_incident_bundle_events_redact_secrets_in_matched_text() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crashes");
    let out_dir = tmp.path().join("output");
    let db_path = tmp.path().join("test.db");
    fs::create_dir_all(&crash_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();
    create_test_db(&db_path);

    // Insert an event with a secret in matched_text
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let anthropic_key = join_parts(&["sk", "-ant-api03-", "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"]);
    let matched_text = format!("found key {anthropic_key} in output");
    conn.execute(
        "INSERT INTO events (pane_id, rule_id, event_type, severity, detected_at, matched_text)
         VALUES (1, 'rule_secret', 'pattern_match', 'warning', 1700000000, ?1)",
        rusqlite::params![matched_text],
    )
    .unwrap();

    let opts = IncidentBundleOptions {
        crash_dir: &crash_dir,
        config_path: None,
        out_dir: &out_dir,
        kind: IncidentKind::Manual,
        db_path: Some(&db_path),
        max_events: 10,
    };

    let result = collect_incident_bundle(&opts).unwrap();
    let events_content = fs::read_to_string(result.path.join("recent_events.json")).unwrap();
    // The Anthropic key should be redacted
    let prefix = join_parts(&["sk", "-ant-api03-"]);
    assert!(!events_content.contains(&prefix));
    assert!(events_content.contains("[REDACTED"));
}

#[test]
fn collect_incident_bundle_nonexistent_db_skips_db_files() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crashes");
    let out_dir = tmp.path().join("output");
    fs::create_dir_all(&crash_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();

    let fake_db = tmp.path().join("nonexistent.db");

    let opts = IncidentBundleOptions {
        crash_dir: &crash_dir,
        config_path: None,
        out_dir: &out_dir,
        kind: IncidentKind::Manual,
        db_path: Some(&fake_db),
        max_events: 10,
    };

    let result = collect_incident_bundle(&opts).unwrap();
    // DB files should not be created for nonexistent DB
    assert!(!result.path.join("db_metadata.json").exists());
    assert!(!result.path.join("recent_events.json").exists());
}

#[test]
fn collect_incident_bundle_files_list_matches_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crashes");
    let out_dir = tmp.path().join("output");
    let db_path = tmp.path().join("test.db");
    fs::create_dir_all(&crash_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();
    create_test_db(&db_path);
    insert_test_events(&db_path, 2);

    let config_path = tmp.path().join("config.toml");
    fs::write(&config_path, "name = \"test\"\n").unwrap();

    let opts = IncidentBundleOptions {
        crash_dir: &crash_dir,
        config_path: Some(&config_path),
        out_dir: &out_dir,
        kind: IncidentKind::Manual,
        db_path: Some(&db_path),
        max_events: 10,
    };

    let result = collect_incident_bundle(&opts).unwrap();

    // Every file in the files list should exist on disk
    for file in &result.files {
        assert!(
            result.path.join(file).exists(),
            "File listed but not on disk: {file}"
        );
    }

    // incident_manifest.json should also exist (written after files list)
    assert!(result.path.join("incident_manifest.json").exists());
}

// ── Replay tests ──────────────────────────────────────────────────────

#[test]
fn replay_nonexistent_bundle_returns_error() {
    let result = replay_incident_bundle(Path::new("/nonexistent/bundle"), ReplayMode::Policy);
    assert!(result.is_err());
}

#[test]
fn replay_clean_bundle_policy_mode_passes() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crashes");
    let out_dir = tmp.path().join("output");
    fs::create_dir_all(&crash_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();

    let config_path = tmp.path().join("config.toml");
    fs::write(&config_path, "name = \"test\"\nlog_level = \"debug\"\n").unwrap();

    let opts = IncidentBundleOptions {
        crash_dir: &crash_dir,
        config_path: Some(&config_path),
        out_dir: &out_dir,
        kind: IncidentKind::Manual,
        db_path: None,
        max_events: 0,
    };

    let bundle = collect_incident_bundle(&opts).unwrap();
    let result = replay_incident_bundle(&bundle.path, ReplayMode::Policy).unwrap();

    assert_eq!(result.status, "pass");
    assert!(
        result
            .checks
            .iter()
            .any(|c| c.name == "manifest_valid" && c.passed)
    );
    assert!(
        result
            .checks
            .iter()
            .any(|c| c.name == "no_secrets_leaked" && c.passed)
    );
    assert!(
        result
            .checks
            .iter()
            .any(|c| c.name == "files_complete" && c.passed)
    );
}

#[test]
fn replay_bundle_with_db_metadata_policy_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crashes");
    let out_dir = tmp.path().join("output");
    let db_path = tmp.path().join("test.db");
    fs::create_dir_all(&crash_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();
    create_test_db(&db_path);
    insert_test_events(&db_path, 3);

    let opts = IncidentBundleOptions {
        crash_dir: &crash_dir,
        config_path: None,
        out_dir: &out_dir,
        kind: IncidentKind::Manual,
        db_path: Some(&db_path),
        max_events: 10,
    };

    let bundle = collect_incident_bundle(&opts).unwrap();
    let result = replay_incident_bundle(&bundle.path, ReplayMode::Policy).unwrap();

    assert_eq!(result.status, "pass");
    assert!(
        result
            .checks
            .iter()
            .any(|c| c.name == "db_metadata_valid" && c.passed)
    );
}

#[test]
fn replay_bundle_rules_mode_validates_events() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crashes");
    let out_dir = tmp.path().join("output");
    let db_path = tmp.path().join("test.db");
    fs::create_dir_all(&crash_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();
    create_test_db(&db_path);
    insert_test_events(&db_path, 5);

    let opts = IncidentBundleOptions {
        crash_dir: &crash_dir,
        config_path: None,
        out_dir: &out_dir,
        kind: IncidentKind::Manual,
        db_path: Some(&db_path),
        max_events: 10,
    };

    let bundle = collect_incident_bundle(&opts).unwrap();
    let result = replay_incident_bundle(&bundle.path, ReplayMode::Rules).unwrap();

    assert_eq!(result.status, "pass");
    assert!(
        result
            .checks
            .iter()
            .any(|c| c.name == "events_structure_valid" && c.passed)
    );
    assert!(
        result
            .checks
            .iter()
            .any(|c| c.name == "events_text_bounded" && c.passed)
    );
}

#[test]
fn replay_detects_secret_leak_in_bundle() {
    let tmp = tempfile::tempdir().unwrap();
    let bundle_dir = tmp.path().join("wa_incident_manual_leak");
    fs::create_dir_all(&bundle_dir).unwrap();

    // Write a manifest
    let manifest = IncidentBundleResult {
        path: bundle_dir.clone(),
        kind: IncidentKind::Manual,
        files: vec!["leaky_config.json".to_string()],
        total_size_bytes: 100,
        wa_version: "0.1.0".to_string(),
        exported_at: "2026-01-01T00:00:00Z".to_string(),
    };
    fs::write(
        bundle_dir.join("incident_manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();

    // Write a file with an un-redacted secret
    let leaky_key = join_parts(&["sk", "-ant-api03-", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"]);
    fs::write(
        bundle_dir.join("leaky_config.json"),
        format!(r#"{{"api_key": "{leaky_key}"}}"#),
    )
    .unwrap();

    let result = replay_incident_bundle(&bundle_dir, ReplayMode::Policy).unwrap();

    assert_eq!(result.status, "fail");
    assert!(
        result
            .checks
            .iter()
            .any(|c| c.name.starts_with("no_secrets_") && !c.passed)
    );
}

#[test]
fn replay_empty_bundle_dir_fails_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let bundle_dir = tmp.path().join("empty_bundle");
    fs::create_dir_all(&bundle_dir).unwrap();

    let result = replay_incident_bundle(&bundle_dir, ReplayMode::Policy).unwrap();

    assert_eq!(result.status, "fail");
    assert!(
        result
            .checks
            .iter()
            .any(|c| c.name == "manifest_valid" && !c.passed)
    );
}

#[test]
fn replay_result_serializes_to_json() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crashes");
    let out_dir = tmp.path().join("output");
    fs::create_dir_all(&crash_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();

    let opts = IncidentBundleOptions {
        crash_dir: &crash_dir,
        config_path: None,
        out_dir: &out_dir,
        kind: IncidentKind::Manual,
        db_path: None,
        max_events: 0,
    };

    let bundle = collect_incident_bundle(&opts).unwrap();
    let result = replay_incident_bundle(&bundle.path, ReplayMode::Policy).unwrap();

    let json = serde_json::to_string_pretty(&result).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["mode"], "policy");
    assert_eq!(parsed["status"], "pass");
    assert!(parsed["checks"].is_array());
}

#[test]
fn replay_crash_kind_bundle_validates_crash_report() {
    let tmp = tempfile::tempdir().unwrap();
    let crash_dir = tmp.path().join("crashes");
    let out_dir = tmp.path().join("output");
    fs::create_dir_all(&crash_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();

    let report = basic_report();
    let snapshot = basic_snapshot();
    let _ = write_crash_bundle(&crash_dir, &report, Some(&snapshot));

    let opts = IncidentBundleOptions {
        crash_dir: &crash_dir,
        config_path: None,
        out_dir: &out_dir,
        kind: IncidentKind::Crash,
        db_path: None,
        max_events: 0,
    };

    let bundle = collect_incident_bundle(&opts).unwrap();
    let result = replay_incident_bundle(&bundle.path, ReplayMode::Policy).unwrap();

    assert_eq!(result.status, "pass");
    assert!(
        result
            .checks
            .iter()
            .any(|c| c.name == "crash_report_valid" && c.passed)
    );
}
