use frankenterm_core::redactor::{REDACTED_MARKER, StreamingRedactor};
use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::scrollback_cold_tier_pipeline::{
    ChunkBytes, ColdTierKeyHandle, PipelineHealth, Raw, Written, finish_streaming_redaction,
};
use frankenterm_core::storage::{PaneRecord, StorageHandle};
use rusqlite::Connection;
use std::future::Future;
use std::path::Path;
use std::process::Command;

const MMAP_REDACTION_CHILD_ENV: &str = "FT_GD4ZA_MMAP_REDACTION_CHILD";

#[test]
fn redactor_evidence_drives_cold_tier_write_privacy_health() {
    let secret = "sk-proj-abcdefghijklmnopqrstuvwxyz123456";
    let raw_line = format!("export OPENAI_API_KEY={secret}\n");
    let raw_len = raw_line.len() as u32;
    let mut redactor = StreamingRedactor::new();

    let raw = ChunkBytes::<Raw>::from_raw(raw_line.into_bytes());
    let (redacted, evidence) = raw.redact_with_streaming(&mut redactor);
    assert!(
        finish_streaming_redaction(&mut redactor).is_none(),
        "newline-delimited whole secret should not leave a delayed tail"
    );

    let redacted_text =
        String::from_utf8(redacted.as_bytes().to_vec()).expect("redactor emits utf-8");
    assert!(evidence.redactor_applied());
    assert!(evidence.made_changes());
    assert!(!redacted_text.contains(secret));
    assert!(redacted_text.contains(REDACTED_MARKER));

    let compressed = redacted.compress_with(|bytes| bytes);
    let key = ColdTierKeyHandle {
        key_id: "test-key".to_string(),
        mmap_key_slug: "test-mmap-key".to_string(),
    };
    let encrypted = compressed.encrypt_with(&key, |bytes| bytes);

    let mut written_bytes = Vec::new();
    let written: ChunkBytes<Written> = encrypted
        .write_with::<_, ()>(|bytes| {
            written_bytes.extend_from_slice(bytes);
            Ok(())
        })
        .expect("test writer succeeds");

    assert_eq!(written.as_bytes(), written_bytes.as_slice());
    assert!(!String::from_utf8_lossy(&written_bytes).contains(secret));

    let mut health = PipelineHealth::baseline();
    health.record_write(
        raw_len,
        written.len() as u32,
        evidence.redactor_applied(),
        true,
    );

    assert!(health.is_safe());
    assert_eq!(health.chunks_written_total, 1);
    assert_eq!(health.redactions_applied_total, 1);
    assert_eq!(health.chunks_written_without_redactor, 0);
    assert_eq!(health.encryptions_applied_total, 1);
}

#[test]
fn streaming_redactor_drives_cold_tier_split_secret_privacy() {
    let secret = "sk-proj-abcdefghijklmnopqrstuvwxyz123456";
    let raw_line = format!("export OPENAI_API_KEY={secret}\n");
    let split = raw_line.find(secret).expect("secret in fixture") + "sk-proj-".len();
    let mut redactor = StreamingRedactor::new();

    let raw1 = ChunkBytes::<Raw>::from_raw(raw_line.as_bytes()[..split].to_vec());
    let raw2 = ChunkBytes::<Raw>::from_raw(raw_line.as_bytes()[split..].to_vec());
    let (redacted1, evidence1) = raw1.redact_with_streaming(&mut redactor);
    let (redacted2, evidence2) = raw2.redact_with_streaming(&mut redactor);

    let mut matches = evidence1.matches + evidence2.matches;
    let mut redacted_bytes = [redacted1.as_bytes(), redacted2.as_bytes()].concat();
    if let Some((tail, tail_evidence)) = finish_streaming_redaction(&mut redactor) {
        matches += tail_evidence.matches;
        redacted_bytes.extend_from_slice(tail.as_bytes());
    }

    let redacted_text = String::from_utf8(redacted_bytes).expect("redactor emits utf-8");
    assert!(matches > 0);
    assert!(!redacted_text.contains(secret));
    assert!(redacted_text.contains(REDACTED_MARKER));
}

#[test]
fn production_storage_redacts_split_secret_in_sqlite_and_mmap_mirror() {
    if std::env::var_os(MMAP_REDACTION_CHILD_ENV).is_none() {
        let output = Command::new(std::env::current_exe().expect("current test binary"))
            .arg("--exact")
            .arg("production_storage_redacts_split_secret_in_sqlite_and_mmap_mirror")
            .arg("--nocapture")
            .env(MMAP_REDACTION_CHILD_ENV, "1")
            .env("FT_STORAGE_MMAP_ENABLE", "1")
            .env_remove("FT_STORAGE_MMAP_DIR")
            .output()
            .expect("run mmap-enabled child test");
        assert!(
            output.status.success(),
            "mmap-enabled child failed\nstatus: {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    run_async_test(async {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let db_path = temp_dir.path().join("storage_redaction.db");
        let db_path_text = db_path.to_string_lossy().into_owned();
        let handle = StorageHandle::new(&db_path_text)
            .await
            .expect("create storage handle");
        handle
            .upsert_pane(test_pane_record(7))
            .await
            .expect("upsert pane");

        let secret = "sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgHiJkLmNoPqRs";
        let payload = format!("pane wrote {secret} after\n");
        let split = payload.find(secret).expect("secret in payload") + "sk-ant-api03-".len();
        let first = &payload[..split];
        let second = &payload[split..];

        let first_segment = handle
            .append_segment(7, first, Some("raw-hash-first".to_string()))
            .await
            .expect("append first split");
        let second_segment = handle
            .append_segment(7, second, Some("raw-hash-second".to_string()))
            .await
            .expect("append second split");

        assert_eq!(first_segment.content, "pane wrote ");
        assert_eq!(first_segment.content_hash, None);
        assert!(!second_segment.content.contains(secret));
        assert!(second_segment.content.contains(REDACTED_MARKER));
        assert_eq!(second_segment.content_hash, None);

        handle.shutdown().await.expect("shutdown storage");

        let sqlite_rows = sqlite_segment_rows(&db_path, 7);
        let sqlite_text = sqlite_rows
            .iter()
            .map(|(content, _)| content.as_str())
            .collect::<String>();
        assert!(sqlite_text.contains(REDACTED_MARKER));
        assert!(!sqlite_text.contains(secret));
        assert!(!sqlite_text.contains("sk-ant-api03-"));
        assert!(
            sqlite_rows
                .iter()
                .all(|(_, content_hash)| content_hash.is_none()),
            "raw content hashes must be dropped after redaction: {sqlite_rows:?}"
        );

        let mmap_log = temp_dir
            .path()
            .join("storage_redaction.mmap_scrollback")
            .join("7.log");
        let mmap_text = std::fs::read_to_string(&mmap_log)
            .unwrap_or_else(|error| panic!("read mmap mirror {}: {error}", mmap_log.display()));
        assert!(mmap_text.contains(REDACTED_MARKER));
        assert!(!mmap_text.contains(secret));
        assert!(!mmap_text.contains("sk-ant-api03-"));
    });
}

fn sqlite_segment_rows(db_path: &Path, pane_id: u64) -> Vec<(String, Option<String>)> {
    let conn = Connection::open(db_path).expect("open sqlite db");
    let pane_id_i64 = i64::try_from(pane_id).expect("test pane id fits i64");
    let mut stmt = conn
        .prepare(
            "SELECT content, content_hash
             FROM output_segments
             WHERE pane_id = ?1
             ORDER BY seq",
        )
        .expect("prepare segment query");
    let rows = stmt
        .query_map([pane_id_i64], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query segment rows");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect segment rows")
}

fn test_pane_record(pane_id: u64) -> PaneRecord {
    PaneRecord {
        pane_id,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: Some("pane".to_string()),
        cwd: None,
        tty_name: None,
        first_seen_at: 1_700_000_000_000,
        last_seen_at: 1_700_000_000_000,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

fn run_async_test<F>(future: F)
where
    F: Future<Output = ()>,
{
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("build current-thread runtime");
    CompatRuntime::block_on(&runtime, future);
}
