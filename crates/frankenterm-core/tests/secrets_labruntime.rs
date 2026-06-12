//! LabRuntime-ported secrets tests for deterministic async testing.
//!
//! Ports all 11 `#[tokio::test]` functions from `secrets.rs` to asupersync-based
//! `RuntimeFixture`, gaining deterministic scheduling for StorageHandle I/O.
//!
//! Covers: scan_storage (6 tests), e2e (5 tests).
//!
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::secrets::{SECRET_SCAN_REPORT_VERSION, SecretScanEngine, SecretScanOptions};
use frankenterm_core::storage::{PaneRecord, StorageHandle};

// ===========================================================================
// Private helper re-implementations
// ===========================================================================

fn make_pane(pane_id: u64) -> PaneRecord {
    PaneRecord {
        pane_id,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: Some("test".to_string()),
        cwd: None,
        tty_name: None,
        first_seen_at: 1_000_000_000_000,
        last_seen_at: 1_000_000_000_000,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

async fn setup_storage(label: &str) -> (StorageHandle, std::path::PathBuf) {
    let db_path =
        std::env::temp_dir().join(format!("wa_secret_labrt_{label}_{}.db", std::process::id()));
    let db_str = db_path.to_string_lossy().to_string();
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_str}-wal"));
    let _ = std::fs::remove_file(format!("{db_str}-shm"));

    let storage = StorageHandle::new(&db_str).await.expect("open test db");
    storage
        .upsert_pane(make_pane(1))
        .await
        .expect("register pane");
    (storage, db_path)
}

async fn teardown(storage: StorageHandle, db_path: &std::path::Path) {
    storage.shutdown().await.expect("shutdown");
    let db_str = db_path.to_string_lossy().to_string();
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{db_str}-wal"));
    let _ = std::fs::remove_file(format!("{db_str}-shm"));
}

/// Insert raw segment content directly via rusqlite, bypassing the production
/// append path. `StorageHandle::append_segment` redacts secrets at rest
/// (`redact_segment_for_persistence`) before the row is written, so a complete
/// secret appended through it is masked to `[REDACTED]` and the scanner under
/// test finds zero raw matches. Tests that assert the scanner FINDS planted
/// secrets must plant them raw, mirroring the inline fixture in
/// `secrets.rs` (ft-r14kr).
fn insert_raw_secret_scan_fixture(db_path: &std::path::Path, pane_id: u64, content: &str) {
    let conn = rusqlite::Connection::open(db_path).expect("open raw fixture db");
    let pane_id_i64 = i64::try_from(pane_id).expect("pane id fits in i64");
    let next_seq: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM output_segments WHERE pane_id = ?1",
            [pane_id_i64],
            |row| row.get(0),
        )
        .expect("query next raw fixture seq");
    let content_len = i64::try_from(content.len()).expect("content length fits in i64");
    let captured_at = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_millis(),
    )
    .expect("epoch ms fits in i64");
    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, content_hash, captured_at)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
        rusqlite::params![pane_id_i64, next_seq, content, content_len, captured_at],
    )
    .expect("insert raw secret scan fixture");
}

const E2E_SECRETS: &[(&str, &str)] = &[
    ("openai", "sk-abc1234567890abcdef1234567890abcdef12345678"),
    ("anthropic", "sk-ant-api03-XXXXXXXXXXXXXXXXXXXXXXXXXXXX"),
    ("github", "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"),
    ("aws", "AKIAIOSFODNN7EXAMPLE"),
    ("slack", "xoxb-1234567890-abcdefghijklmn"),
    ("stripe", "sk_live_abcdefghijklmnopqrstuvwxyz0123"),
    (
        "database",
        "postgres://admin:s3cretP4ss@db.host.com:5432/mydb",
    ),
];

// ===========================================================================
// scan_storage tests (1-6)
// ===========================================================================

#[test]
fn scan_storage_empty_database() {
    RuntimeFixture::current_thread().block_on(async {
        let (storage, db_path) = setup_storage("empty").await;
        let engine = SecretScanEngine::new();
        let report = engine
            .scan_storage(&storage, SecretScanOptions::default())
            .await
            .expect("scan");
        assert_eq!(report.scanned_segments, 0);
        assert_eq!(report.scanned_bytes, 0);
        assert_eq!(report.matches_total, 0);
        assert!(report.samples.is_empty());
        assert_eq!(report.report_version, SECRET_SCAN_REPORT_VERSION);
        teardown(storage, &db_path).await;
    });
}

#[test]
fn scan_storage_with_secrets_finds_matches() {
    RuntimeFixture::current_thread().block_on(async {
        let (storage, db_path) = setup_storage("matches").await;
        insert_raw_secret_scan_fixture(
            &db_path,
            1,
            "export OPENAI_API_KEY=sk-abc1234567890abcdef1234567890abcdef12345678",
        );
        insert_raw_secret_scan_fixture(&db_path, 1, "Hello, no secrets here.");
        insert_raw_secret_scan_fixture(
            &db_path,
            1,
            "GH_TOKEN=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
        );
        let engine = SecretScanEngine::new();
        let report = engine
            .scan_storage(&storage, SecretScanOptions::default())
            .await
            .expect("scan");
        assert_eq!(report.scanned_segments, 3);
        assert!(report.matches_total >= 2, "should find at least 2 secrets");
        assert!(!report.samples.is_empty(), "should have sample records");
        for sample in &report.samples {
            assert_eq!(sample.secret_hash.len(), 64);
            assert_ne!(
                sample.secret_hash,
                "sk-abc1234567890abcdef1234567890abcdef12345678"
            );
        }
        teardown(storage, &db_path).await;
    });
}

#[test]
fn scan_storage_max_segments_caps_scan() {
    RuntimeFixture::current_thread().block_on(async {
        let (storage, db_path) = setup_storage("max").await;
        for i in 0..5u64 {
            storage
                .append_segment(1, &format!("content {i}"), None)
                .await
                .expect("insert");
        }
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions {
            max_segments: Some(3),
            batch_size: 2,
            ..Default::default()
        };
        let report = engine.scan_storage(&storage, options).await.expect("scan");
        assert_eq!(report.scanned_segments, 3, "should stop at max_segments");
        teardown(storage, &db_path).await;
    });
}

#[test]
fn scan_storage_zero_batch_size_defaults() {
    RuntimeFixture::current_thread().block_on(async {
        let (storage, db_path) = setup_storage("batchzero").await;
        storage
            .append_segment(1, "clean text", None)
            .await
            .expect("insert");
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions {
            batch_size: 0,
            ..Default::default()
        };
        let report = engine.scan_storage(&storage, options).await.expect("scan");
        assert_eq!(report.scanned_segments, 1);
        teardown(storage, &db_path).await;
    });
}

#[test]
fn scan_storage_incremental_resumes_from_checkpoint() {
    RuntimeFixture::current_thread().block_on(async {
        let (storage, db_path) = setup_storage("incr").await;
        for i in 0..3u64 {
            insert_raw_secret_scan_fixture(
                &db_path,
                1,
                &format!("line {i}: sk-abc{i}234567890abcdef1234567890abcdef12345678"),
            );
        }
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let report1 = engine
            .scan_storage_incremental(&storage, options.clone())
            .await
            .expect("first scan");
        assert_eq!(report1.scanned_segments, 3);
        let first_matches = report1.matches_total;
        assert!(first_matches > 0);

        for i in 3..5u64 {
            insert_raw_secret_scan_fixture(
                &db_path,
                1,
                &format!("line {i}: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345678{i}"),
            );
        }
        let report2 = engine
            .scan_storage_incremental(&storage, options)
            .await
            .expect("second scan");
        assert_eq!(
            report2.scanned_segments, 2,
            "incremental should only scan new segments"
        );
        assert!(
            report2.resume_after_id.is_some(),
            "should have resume point"
        );
        teardown(storage, &db_path).await;
    });
}

#[test]
fn scan_storage_filters_by_pane_id() {
    RuntimeFixture::current_thread().block_on(async {
        let (storage, db_path) = setup_storage("pane").await;
        storage
            .upsert_pane(make_pane(2))
            .await
            .expect("register pane 2");
        insert_raw_secret_scan_fixture(
            &db_path,
            1,
            "sk-abc1234567890abcdef1234567890abcdef12345678",
        );
        insert_raw_secret_scan_fixture(&db_path, 2, "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789");
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions {
            pane_id: Some(1),
            ..Default::default()
        };
        let report = engine.scan_storage(&storage, options).await.expect("scan");
        assert!(
            !report.samples.is_empty(),
            "pane-filtered scan should find the pane-1 secret"
        );
        for sample in &report.samples {
            assert_eq!(sample.pane_id, 1);
        }
        teardown(storage, &db_path).await;
    });
}

// ===========================================================================
// E2E tests (7-11)
// ===========================================================================

#[test]
fn e2e_fixtures_report_never_contains_raw_secrets() {
    RuntimeFixture::current_thread().block_on(async {
        let (storage, db_path) = setup_storage("e2e_fixtures").await;

        // Insert each secret as a distinct segment
        for (label, secret) in E2E_SECRETS {
            insert_raw_secret_scan_fixture(&db_path, 1, &format!("{label}_key={secret}"));
        }

        let engine = SecretScanEngine::new();
        let report = engine
            .scan_storage(&storage, SecretScanOptions::default())
            .await
            .expect("scan");

        // Serialize full report to JSON
        let json = serde_json::to_string_pretty(&report).expect("serialize report");

        // Verify no raw secret appears anywhere in the JSON
        for (_label, secret) in E2E_SECRETS {
            assert!(
                !json.contains(secret),
                "JSON must not contain raw secret: {secret}"
            );
        }

        // Verify scan actually found matches
        assert!(
            report.matches_total >= E2E_SECRETS.len() as u64,
            "should find at least {} secrets, got {}",
            E2E_SECRETS.len(),
            report.matches_total
        );

        // All samples have valid hashes (64-char hex)
        for sample in &report.samples {
            assert_eq!(
                sample.secret_hash.len(),
                64,
                "sample hash should be 64 hex chars"
            );
            assert!(
                sample.secret_hash.chars().all(|c| c.is_ascii_hexdigit()),
                "hash should be hex"
            );
        }

        teardown(storage, &db_path).await;
    });
}

#[test]
fn e2e_incremental_scan_skips_prior_segments() {
    RuntimeFixture::current_thread().block_on(async {
        let (storage, db_path) = setup_storage("e2e_incr").await;

        // Phase 1: insert 3 segments with secrets
        for i in 0..3u64 {
            insert_raw_secret_scan_fixture(
                &db_path,
                1,
                &format!("phase1-{i}: sk-key{i}234567890abcdef1234567890abcdef12345678"),
            );
        }

        let engine = SecretScanEngine::new();
        let opts = SecretScanOptions::default();

        let r1 = engine
            .scan_storage_incremental(&storage, opts.clone())
            .await
            .expect("scan1");
        assert_eq!(r1.scanned_segments, 3);
        let phase1_matches = r1.matches_total;

        // Phase 2: insert 2 more segments (one clean, one with secret)
        insert_raw_secret_scan_fixture(&db_path, 1, "phase2: clean output");
        insert_raw_secret_scan_fixture(
            &db_path,
            1,
            "phase2: ghp_NEWTOKEN123456789012345678901234567890",
        );

        let r2 = engine
            .scan_storage_incremental(&storage, opts.clone())
            .await
            .expect("scan2");

        // Should only scan the 2 new segments
        assert_eq!(
            r2.scanned_segments, 2,
            "second scan should only cover new segments"
        );
        // Phase 2 had 1 secret
        assert!(r2.matches_total >= 1, "should find the new secret");

        // Phase 3: no new segments -> zero work
        let r3 = engine
            .scan_storage_incremental(&storage, opts)
            .await
            .expect("scan3");
        assert_eq!(r3.scanned_segments, 0, "no new segments means no work");

        // Verify the cumulative coverage
        assert!(
            phase1_matches + r2.matches_total >= 4,
            "total matches across phases should be >= 4"
        );

        teardown(storage, &db_path).await;
    });
}

#[test]
fn e2e_report_json_artifact_stable() {
    RuntimeFixture::current_thread().block_on(async {
        let (storage, db_path) = setup_storage("e2e_json").await;

        // Insert fixtures
        insert_raw_secret_scan_fixture(
            &db_path,
            1,
            "export KEY=sk-abc1234567890abcdef1234567890abcdef12345678",
        );
        insert_raw_secret_scan_fixture(
            &db_path,
            1,
            "GH_TOKEN=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
        );

        let engine = SecretScanEngine::new();

        // Run two consecutive scans on the same data
        let r1 = engine
            .scan_storage(&storage, SecretScanOptions::default())
            .await
            .expect("scan1");
        let r2 = engine
            .scan_storage(&storage, SecretScanOptions::default())
            .await
            .expect("scan2");

        // The scans must actually find the planted secrets — determinism
        // assertions over empty reports would be vacuous.
        assert!(
            r1.matches_total >= 2,
            "should find both planted secrets, got {}",
            r1.matches_total
        );

        // Core metrics should be identical
        assert_eq!(r1.scanned_segments, r2.scanned_segments);
        assert_eq!(r1.scanned_bytes, r2.scanned_bytes);
        assert_eq!(r1.matches_total, r2.matches_total);
        assert_eq!(r1.matches_by_pattern, r2.matches_by_pattern);
        assert_eq!(r1.samples.len(), r2.samples.len());

        // Sample hashes should be identical (deterministic SHA-256)
        for (s1, s2) in r1.samples.iter().zip(r2.samples.iter()) {
            assert_eq!(s1.secret_hash, s2.secret_hash, "hashes should be stable");
            assert_eq!(s1.pattern, s2.pattern);
            assert_eq!(s1.segment_id, s2.segment_id);
            assert_eq!(s1.match_len, s2.match_len);
        }

        // Verify report_version is set
        assert_eq!(r1.report_version, SECRET_SCAN_REPORT_VERSION);

        teardown(storage, &db_path).await;
    });
}

#[test]
fn e2e_multi_pattern_per_segment_counts() {
    RuntimeFixture::current_thread().block_on(async {
        let (storage, db_path) = setup_storage("e2e_multi").await;

        // Single segment with multiple secret types
        let content = concat!(
            "OPENAI=sk-abc1234567890abcdef1234567890abcdef12345678 ",
            "GH=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 ",
            "STRIPE=sk_live_abcdefghijklmnopqrstuvwxyz0123 ",
            "DB=postgres://root:hunter2@db:5432/prod",
        );
        insert_raw_secret_scan_fixture(&db_path, 1, content);

        let engine = SecretScanEngine::new();
        let report = engine
            .scan_storage(&storage, SecretScanOptions::default())
            .await
            .expect("scan");

        // Should detect at least 4 different patterns
        assert!(
            report.matches_total >= 4,
            "should find at least 4 secrets in multi-pattern segment, got {}",
            report.matches_total
        );

        // matches_by_pattern should have multiple keys
        assert!(
            report.matches_by_pattern.len() >= 3,
            "should have at least 3 distinct patterns, got {:?}",
            report.matches_by_pattern.keys().collect::<Vec<_>>()
        );

        // Serialize and verify no raw secrets
        let json = serde_json::to_string(&report).expect("serialize");
        assert!(!json.contains("hunter2"), "no raw DB password in JSON");
        assert!(
            !json.contains("sk-abc1234567890"),
            "no raw OpenAI key in JSON"
        );

        teardown(storage, &db_path).await;
    });
}

#[test]
fn e2e_sample_limit_across_storage() {
    RuntimeFixture::current_thread().block_on(async {
        let (storage, db_path) = setup_storage("e2e_limit").await;

        // Insert many segments, each with a secret
        for i in 0..20u64 {
            insert_raw_secret_scan_fixture(
                &db_path,
                1,
                &format!("password=super_secret_pass_{i:03}xx"),
            );
        }

        let engine = SecretScanEngine::new();
        let options = SecretScanOptions {
            sample_limit: 5,
            ..Default::default()
        };
        let report = engine.scan_storage(&storage, options).await.expect("scan");

        assert_eq!(report.scanned_segments, 20);
        assert!(report.matches_total >= 20, "should count all matches");
        assert!(
            report.samples.len() <= 5,
            "samples should be capped at limit: got {}",
            report.samples.len()
        );

        teardown(storage, &db_path).await;
    });
}
