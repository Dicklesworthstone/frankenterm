use frankenterm_core::scrollback_mmap_format::{HEADER_SIZE, RecordKind};
use frankenterm_core::scrollback_mmap_writer::{
    LinearRecordReadLimits, MmapScrollback, MmapScrollbackConfig, V2_RECORD_HEADER_SIZE,
    read_linear_records,
};
use proptest::prelude::*;
use sha2::{Digest as _, Sha256};
use std::path::Path;
use std::time::Duration;

fn record_kind_strategy() -> impl Strategy<Value = RecordKind> {
    prop::sample::select(vec![
        RecordKind::Text,
        RecordKind::Osc,
        RecordKind::Csi,
        RecordKind::Cursor,
        RecordKind::Clear,
    ])
}

fn safe_payload_strategy(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(b'a'..=b'z', 0..=max_len)
}

fn record_strategy(max_payload_len: usize) -> impl Strategy<Value = (RecordKind, Vec<u8>)> {
    (
        record_kind_strategy(),
        safe_payload_strategy(max_payload_len),
    )
}

fn config_for(dir: &tempfile::TempDir, pane_uuid: &str, cap_bytes: u64) -> MmapScrollbackConfig {
    // Let the writer create its private leaf rather than inheriting the
    // caller's tempfile mode (which can include group access under CI umasks).
    MmapScrollbackConfig::new(dir.path().join("scrollback"), pane_uuid)
        .with_cap_bytes(cap_bytes)
        .with_sync_every_appends(0)
        .with_sync_interval(Duration::from_secs(3600))
}

fn required_capacity(records: &[(RecordKind, Vec<u8>)]) -> u64 {
    let payload_bytes: u64 = records
        .iter()
        .map(|(_, payload)| payload.len() as u64)
        .sum();
    let max_streaming_records = records.len().saturating_mul(2) as u64;

    payload_bytes
        .saturating_add(max_streaming_records.saturating_mul(V2_RECORD_HEADER_SIZE as u64))
        .saturating_add(64)
        .max(V2_RECORD_HEADER_SIZE as u64 + 1)
}

fn read_limits(path: &Path) -> LinearRecordReadLimits {
    let max_file_bytes = std::fs::metadata(path)
        .expect("read mmap file metadata")
        .len();
    LinearRecordReadLimits {
        max_file_bytes,
        max_records: 1_024,
        max_payload_bytes: max_file_bytes,
    }
}

#[test]
fn mmap_writer_streaming_redacts_secret_split_across_record_kinds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config_for(&dir, "pane-split-kind-secret", 4096).with_sync_every_appends(1);
    let path = config.bin_path();
    let mut writer = MmapScrollback::open(config).expect("open writer");
    let secret = b"sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgHiJkLmNoPqRs";
    let split = 24;

    let first = writer
        .append(RecordKind::Text, &secret[..split])
        .expect("append first chunk");
    assert_eq!(first.payload_bytes, 0);

    let second_payload = [&secret[split..], b"\n".as_slice()].concat();
    let second = writer
        .append(RecordKind::Osc, &second_payload)
        .expect("append second chunk");
    assert!(second.redaction.replacement_count > 0);
    writer.sync().expect("sync writer");
    drop(writer);

    let bytes = std::fs::read(&path).expect("read mmap file");
    assert!(
        !bytes
            .windows(b"sk-ant-api03-".len())
            .any(|window| window == b"sk-ant-api03-")
    );
    assert!(
        bytes
            .windows(b"[REDACTED]".len())
            .any(|window| window == b"[REDACTED]")
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn proptest_scrollback_mmap_writer_config_paths_bind_canonical_sha256_stem(
        pane_uuid in "[A-Za-z0-9_./:;\\\\ -]{0,80}",
        cap_mb in 0_u32..=8,
        sync_every in 0_u64..=128,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = MmapScrollbackConfig::new(dir.path(), pane_uuid.clone())
            .with_cap_mb(cap_mb)
            .with_sync_every_appends(sync_every);
        let expected_stem = if pane_uuid.len() == 64
            && pane_uuid
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            pane_uuid.clone()
        } else {
            hex::encode(Sha256::digest(pane_uuid.as_bytes()))
        };
        let expected_cap = if cap_mb == 0 {
            50 * 1024 * 1024
        } else {
            u64::from(cap_mb) * 1024 * 1024
        };

        prop_assert_eq!(config.cap_bytes, expected_cap);
        prop_assert_eq!(config.sync_every_appends, sync_every);
        prop_assert!(config.bin_path().starts_with(dir.path()));
        prop_assert!(config.lock_path().starts_with(dir.path()));
        let expected_bin_suffix = format!("{expected_stem}.bin");
        let expected_lock_suffix = format!("{expected_stem}.bin.lock");
        prop_assert!(config.bin_path().ends_with(&expected_bin_suffix));
        prop_assert!(config.lock_path().ends_with(&expected_lock_suffix));
    }

    #[test]
    fn proptest_scrollback_mmap_writer_appends_read_back_linearly_when_capacity_does_not_wrap(
        records in prop::collection::vec(record_strategy(24), 0..=16),
        pane_uuid in "[A-Za-z0-9_-]{1,32}",
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cap_bytes = required_capacity(&records);
        let config = config_for(&dir, &pane_uuid, cap_bytes);
        let path = config.bin_path();
        let mut writer = MmapScrollback::open(config).expect("open writer");
        let mut expected_bytes = Vec::new();

        for (kind, payload) in &records {
            let report = writer.append(*kind, payload).expect("append record");
            prop_assert!(report.payload_bytes <= payload.len() + expected_bytes.len());
            prop_assert_eq!(report.redaction.replacement_count, 0);
            prop_assert!(!report.synced);
            expected_bytes.extend_from_slice(payload);
        }
        let _ = writer.flush_pending_redaction().expect("flush pending redaction");
        writer.sync().expect("sync writer");
        drop(writer);

        let read_back = read_linear_records(&path, read_limits(&path))
            .expect("read linear records");
        let actual_bytes: Vec<u8> = read_back
            .records
            .iter()
            .flat_map(|(_, payload)| payload.iter().copied())
            .collect();
        prop_assert_eq!(actual_bytes, expected_bytes);
    }

    #[test]
    fn proptest_scrollback_mmap_writer_header_accounting_matches_appended_records(
        records in prop::collection::vec(record_strategy(32), 1..=12),
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cap_bytes = required_capacity(&records);
        let config = config_for(&dir, "pane-accounting", cap_bytes);
        let path = config.bin_path();
        let mut writer = MmapScrollback::open(config)
            .expect("open writer");

        for (kind, payload) in &records {
            writer.append(*kind, payload).expect("append record");
            prop_assert_eq!(writer.header().redactions_applied, 0);
        }
        let _ = writer.flush_pending_redaction().expect("flush pending redaction");
        writer.sync().expect("sync writer");
        let header = writer.header();
        drop(writer);

        let persisted = read_linear_records(&path, read_limits(&path)).expect("read records");
        let expected_total: u64 = persisted
            .records
            .iter()
            .map(|(_, payload)| V2_RECORD_HEADER_SIZE as u64 + payload.len() as u64)
            .sum();

        prop_assert_eq!(header.write_cursor_bytes, expected_total);
        prop_assert_eq!(header.total_bytes_written, expected_total);
        prop_assert_eq!(std::fs::metadata(&path).expect("metadata").len(), HEADER_SIZE as u64 + cap_bytes);
    }

    #[test]
    fn proptest_scrollback_mmap_writer_oversized_payload_is_tail_truncated_to_capacity(
        cap_bytes in (V2_RECORD_HEADER_SIZE as u64 + 1)..=128_u64,
        payload in prop::collection::vec(prop::sample::select(vec![b'z', b'Z']), 129..=256),
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut writer = MmapScrollback::open(config_for(&dir, "pane-truncate", cap_bytes))
            .expect("open writer");
        let path = writer.path().to_path_buf();
        let mut payload = payload;
        // No secret anchors, and a distinct final byte makes head truncation
        // observably wrong even at the one-byte minimum payload capacity.
        payload.push(b'!');
        let max_payload = cap_bytes as usize - V2_RECORD_HEADER_SIZE;
        prop_assert!(payload.len() > max_payload);

        // Streaming retention protects secret-like suffixes; ordinary safe
        // bytes emit immediately even when shorter than the maximum tail window.
        let report = writer.append(RecordKind::Text, &payload).expect("append oversized payload");
        prop_assert!(writer.flush_pending_redaction().expect("finish stream").is_none());

        prop_assert_eq!(report.payload_bytes, max_payload);
        prop_assert_eq!(report.redaction.replacement_count, 0);
        prop_assert_eq!(report.write_cursor_bytes, 0);
        prop_assert_eq!(writer.header().write_cursor_bytes, 0);
        prop_assert_eq!(writer.header().total_bytes_written, cap_bytes);
        prop_assert_eq!(writer.header().capacity_bytes, cap_bytes);
        writer.sync().expect("sync truncated record");
        drop(writer);
        let persisted = read_linear_records(&path, read_limits(&path)).expect("read truncated record");
        prop_assert_eq!(
            persisted.records,
            vec![(RecordKind::Text, payload[payload.len() - max_payload..].to_vec())],
            "full ring must retain exactly the newest payload suffix"
        );
    }

    #[test]
    fn proptest_scrollback_mmap_writer_reopen_preserves_header_and_linear_records(
        records in prop::collection::vec(record_strategy(20), 1..=10),
        pane_uuid in "[A-Za-z0-9_-]{1,32}",
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cap_bytes = required_capacity(&records);
        let config = config_for(&dir, &pane_uuid, cap_bytes);
        let path = config.bin_path();

        {
            let mut writer = MmapScrollback::open(config.clone()).expect("open writer");
            for (kind, payload) in &records {
                writer.append(*kind, payload).expect("append record");
            }
            let _ = writer.flush_pending_redaction().expect("flush pending redaction");
            writer.sync().expect("sync writer");
        }

        let read_back = read_linear_records(&path, read_limits(&path)).expect("read records");
        let expected_cursor: u64 = read_back
            .records
            .iter()
            .map(|(_, payload)| V2_RECORD_HEADER_SIZE as u64 + payload.len() as u64)
            .sum();
        let bytes_before_reopen = std::fs::read(&path).expect("snapshot before reopen");
        let reopened = MmapScrollback::open(config).expect("reopen writer");
        prop_assert_eq!(reopened.header().capacity_bytes, cap_bytes);
        prop_assert_eq!(reopened.header().write_cursor_bytes, expected_cursor);
        prop_assert_eq!(reopened.header().total_bytes_written, expected_cursor);
        drop(reopened);
        prop_assert_eq!(
            std::fs::read(&path).expect("snapshot after reopen"),
            bytes_before_reopen,
            "a successful reopen must not rewrite, resize, or reinitialize the leaf",
        );

        let reopened_read = read_linear_records(&path, read_limits(&path))
            .expect("read records after reopen");
        prop_assert_eq!(&reopened_read.records, &read_back.records);
        prop_assert_eq!(reopened_read.completeness, read_back.completeness);

        let actual_bytes: Vec<u8> = read_back
            .records
            .iter()
            .flat_map(|(_, payload)| payload.iter().copied())
            .collect();
        let expected_bytes: Vec<u8> = records
            .iter()
            .flat_map(|(_, payload)| payload.iter().copied())
            .collect();
        prop_assert_eq!(actual_bytes, expected_bytes);
    }
}

#[cfg(unix)]
#[test]
fn mmap_writer_refuses_group_accessible_directory_without_creating_leaves() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("tempdir");
    let config = config_for(&dir, "unsafe-directory", 4096);
    let base_dir = config.bin_path().parent().expect("parent").to_path_buf();
    std::fs::create_dir(&base_dir).expect("create unsafe directory");
    std::fs::set_permissions(&base_dir, std::fs::Permissions::from_mode(0o770))
        .expect("set explicit unsafe mode");
    assert!(matches!(
        MmapScrollback::open(config),
        Err(frankenterm_core::scrollback_mmap_writer::MmapScrollbackError::UnsafeReadSource { .. })
    ));
    assert_eq!(
        std::fs::read_dir(&base_dir)
            .expect("read directory")
            .count(),
        0
    );
    assert_eq!(
        std::fs::metadata(&base_dir)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777,
        0o770
    );
}
