//! Round-8 keep-gate proof for `ft-yjihu.1` — skip the startup PASSIVE WAL
//! checkpoint when the WAL is small and healthy. PROMOTED to default-ON in
//! round-9 (the env gate is now an opt-OUT; see `round9_wal_startup_time` for the
//! +74% startup-time A/B). The branch-equivalence cases below are unchanged; only
//! the default-state child case (`t5_child_public_path_default_on_and_opt_out`)
//! reflects the flipped default.
//!
//! Round-7 B0' profiling found `storage.wal_recovery_dirty` (storage.rs:1647,
//! the `check_and_recover_wal` writer-open path) at 3.528% startup self-time on
//! a ~4.7 MB dirty WAL. SQLite WAL durability does NOT require checkpoint-on-open:
//! open/read replays WAL frames; checkpointing is maintenance/compaction. The
//! lever skips that startup checkpoint when the gate is on, no rollback journal
//! exists, `quick_check` passes, and a conservative (over-counting) WAL-frame
//! estimate is `<= WAL_RECOVERY_THRESHOLD`. Any ambiguity falls back to the
//! existing checkpoint behavior, and the corruption fail-closed path is preserved.
//!
//! Correctness — not wall-clock — is the whole risk, so this is a deterministic
//! harness. The branch taken is observed via the `WalRecoveryAction` return value
//! (filesystem inference is unreliable: SQLite resets/removes the WAL on
//! last-connection close and a PASSIVE checkpoint does not resize the file). The
//! one real-gate end-to-end case runs in a child process via `Command::env`,
//! because mutating process env is `unsafe` and forbidden under
//! `#![forbid(unsafe_code)]`.

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use frankenterm_core::storage::{
    WalFrameEstimate, WalRecoveryAction, check_and_recover_wal_inner, estimate_wal_frames,
    skip_startup_wal_checkpoint_enabled_for_test,
};
use frankenterm_core::storage_backend_trait::RusqliteBackend;
use frankenterm_core::{Error, StorageError};
use rusqlite::{Connection, params};
use tempfile::TempDir;

// `WAL_RECOVERY_THRESHOLD` is `pub(crate)` in storage.rs; the integration-test
// crate cannot import it. Mirror it here (see storage.rs:287 — keep in sync).
const WAL_RECOVERY_THRESHOLD: i64 = 10_000;

const SKIP_GATE_ENV: &str = "FT_MOONSHOT_SKIP_STARTUP_WAL_CHECKPOINT";
const MOONSHOT_ALL_ENV: &str = "FT_MOONSHOT_ALL";
const CHILD_MODE_ENV: &str = "FT_ROUND8_WAL_CHILD";
const CHILD_LINE_PREFIX: &str = "ROUND8_WAL_CHILD:";

// ---------------------------------------------------------------------------
// WAL construction helpers
// ---------------------------------------------------------------------------

/// A WAL-mode DB with `n` committed rows and `wal_autocheckpoint=0`. The writer
/// `Connection` is returned and MUST be kept alive by the caller: dropping it is
/// a last-connection close, which makes SQLite auto-checkpoint and remove the
/// WAL — destroying the dirty-WAL fixture before recovery can observe it.
#[must_use]
fn dirty_wal_db(dir: &TempDir, name: &str, rows: usize) -> (Connection, PathBuf) {
    let path = dir.path().join(name);
    let mut conn = Connection::open(&path).expect("open dirty WAL db");
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("enable WAL");
    conn.pragma_update(None, "wal_autocheckpoint", 0)
        .expect("disable autocheckpoint");
    conn.execute_batch("CREATE TABLE sample(id INTEGER PRIMARY KEY, body TEXT NOT NULL);")
        .expect("create schema");
    let payload = "round8 dirty WAL frame payload ".repeat(8);
    let tx = conn.transaction().expect("begin tx");
    for row in 0..rows {
        tx.execute(
            "INSERT INTO sample(id, body) VALUES (?1, ?2)",
            params![row as i64, payload],
        )
        .expect("insert row");
    }
    tx.commit().expect("commit rows");
    (conn, path)
}

/// A WAL-mode DB whose WAL exceeds `WAL_RECOVERY_THRESHOLD` frames. Uses a 512-byte
/// page size and commits batches (autocheckpoint off) until the conservative frame
/// estimate clears the threshold with margin — self-tuning, no page-fill math.
#[must_use]
fn large_dirty_wal_db(dir: &TempDir, name: &str) -> (Connection, PathBuf) {
    let path = dir.path().join(name);
    let mut conn = Connection::open(&path).expect("open large WAL db");
    // page_size must be set before any page is written.
    conn.pragma_update(None, "page_size", 512)
        .expect("set 512-byte pages");
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("enable WAL");
    conn.pragma_update(None, "wal_autocheckpoint", 0)
        .expect("disable autocheckpoint");
    conn.execute_batch("CREATE TABLE sample(id INTEGER PRIMARY KEY, body TEXT NOT NULL);")
        .expect("create schema");

    let wal_path = format!("{}-wal", path.to_string_lossy());
    // ~1.5 KB payload so each row consumes >=1 page (and overflow pages at the
    // 512-byte page size) regardless of whether the page_size pragma took effect
    // — keeps frame growth fast and robust. The loop returns as soon as the
    // estimate clears the threshold, so the common case finishes in a few batches.
    let payload = "x".repeat(1_500);
    let target = WAL_RECOVERY_THRESHOLD + 1_500; // margin past the threshold
    let mut next_id: i64 = 0;
    for _batch in 0..40 {
        let tx = conn.transaction().expect("begin batch tx");
        for _ in 0..2_000 {
            tx.execute(
                "INSERT INTO sample(id, body) VALUES (?1, ?2)",
                params![next_id, payload],
            )
            .expect("insert large-WAL row");
            next_id += 1;
        }
        tx.commit().expect("commit batch");
        if let WalFrameEstimate::Frames(frames) = estimate_wal_frames(&wal_path) {
            if frames > target {
                return (conn, path);
            }
        }
    }
    panic!("could not grow WAL past {target} frames for the large-WAL case");
}

fn wal_path_for(db: &Path) -> String {
    format!("{}-wal", db.to_string_lossy())
}

fn read_all_rows(path: &Path) -> Vec<(i64, String)> {
    let conn = Connection::open(path).expect("open reader");
    let mut stmt = conn
        .prepare("SELECT id, body FROM sample ORDER BY id")
        .expect("prepare select");
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .expect("query rows")
        .map(|r| r.expect("row"))
        .collect();
    rows
}

fn recover(path: &Path, skip_enabled: bool) -> frankenterm_core::error::Result<WalRecoveryAction> {
    let conn = Connection::open(path).expect("open recovery connection");
    // Mirror the production writer-open path (storage.rs:1644), which sets a 5s
    // busy_timeout before WAL recovery so a transient lock waits instead of
    // failing immediately.
    let _ = conn.busy_timeout(Duration::from_secs(5));
    let backend = RusqliteBackend::new(conn);
    let db_path = path.to_string_lossy();
    let action = check_and_recover_wal_inner(&backend, &db_path, skip_enabled);
    // Drop the backend connection deterministically after recovery.
    let _ = backend.into_connection();
    action
}

/// Write a fabricated `-wal`-style file: `len` bytes, optional big-endian magic at
/// offset 0, optional big-endian page_size field at offset 8, zero-padded.
fn write_fake_wal(
    dir: &TempDir,
    name: &str,
    len: usize,
    magic: Option<u32>,
    page: Option<u32>,
) -> PathBuf {
    let mut buf = vec![0u8; len];
    if let Some(m) = magic {
        if len >= 4 {
            buf[0..4].copy_from_slice(&m.to_be_bytes());
        }
    }
    if let Some(p) = page {
        if len >= 12 {
            buf[8..12].copy_from_slice(&p.to_be_bytes());
        }
    }
    let path = dir.path().join(name);
    std::fs::write(&path, &buf).expect("write fake wal");
    path
}

// ---------------------------------------------------------------------------
// T1 — small dirty WAL + skip ON → checkpoint SKIPPED, data fully preserved
// ---------------------------------------------------------------------------

#[test]
fn t1_small_dirty_wal_skips_and_preserves_data() {
    let dir = TempDir::new().expect("tempdir");
    let (_writer, path) = dirty_wal_db(&dir, "t1.db", 8);
    // Keep `_writer` alive so the dirty WAL persists through recovery.

    let before = read_all_rows(&path);
    assert_eq!(before.len(), 8, "fixture must have 8 committed rows");
    assert!(
        matches!(estimate_wal_frames(&wal_path_for(&path)), WalFrameEstimate::Frames(n) if n <= WAL_RECOVERY_THRESHOLD),
        "small WAL must estimate at/below threshold to be skip-eligible"
    );

    let action = recover(&path, true).expect("recovery must succeed");
    assert_eq!(
        action,
        WalRecoveryAction::SkippedCheckpoint,
        "small healthy WAL with gate on must skip the startup checkpoint"
    );

    // Durability oracle: a fresh reader replays the WAL and sees every committed
    // row even though the checkpoint was skipped — byte-identical to before.
    let after = read_all_rows(&path);
    assert_eq!(
        before, after,
        "skipping the checkpoint must not lose or alter any committed row"
    );
}

// ---------------------------------------------------------------------------
// T2 — WAL over threshold + skip ON → NOT skipped (falls through to checkpoint)
// ---------------------------------------------------------------------------

#[test]
fn t2_large_wal_is_not_skipped() {
    let dir = TempDir::new().expect("tempdir");
    let (_writer, path) = large_dirty_wal_db(&dir, "t2.db");

    match estimate_wal_frames(&wal_path_for(&path)) {
        WalFrameEstimate::Frames(n) => assert!(
            n > WAL_RECOVERY_THRESHOLD,
            "large-WAL fixture must exceed threshold (got {n})"
        ),
        WalFrameEstimate::Unreadable => panic!("large-WAL fixture header unreadable"),
    }

    let action = recover(&path, true).expect("recovery must succeed");
    assert_ne!(
        action,
        WalRecoveryAction::SkippedCheckpoint,
        "an over-threshold WAL must NOT be skipped even with the gate on"
    );
}

// ---------------------------------------------------------------------------
// T3 — malformed / short / bad header → estimate Unreadable (deterministic fallback)
//      + positive-control valid headers compute the exact frame count.
// ---------------------------------------------------------------------------

#[test]
fn t3_estimate_rejects_malformed_headers_and_computes_valid_ones() {
    let dir = TempDir::new().expect("tempdir");

    // (a) shorter than the 32-byte header
    let short = write_fake_wal(&dir, "short.wal", 10, Some(0x377f_0682), Some(4096));
    assert_eq!(
        estimate_wal_frames(&short.to_string_lossy()),
        WalFrameEstimate::Unreadable
    );

    // (b) bad magic
    let bad_magic = write_fake_wal(&dir, "badmagic.wal", 64, Some(0x0000_0000), Some(4096));
    assert_eq!(
        estimate_wal_frames(&bad_magic.to_string_lossy()),
        WalFrameEstimate::Unreadable
    );

    // (c) valid magic, non-power-of-two page size
    let bad_page = write_fake_wal(&dir, "badpage.wal", 64, Some(0x377f_0683), Some(1000));
    assert_eq!(
        estimate_wal_frames(&bad_page.to_string_lossy()),
        WalFrameEstimate::Unreadable
    );

    // (d) valid magic, page size 0
    let zero_page = write_fake_wal(&dir, "zeropage.wal", 64, Some(0x377f_0682), Some(0));
    assert_eq!(
        estimate_wal_frames(&zero_page.to_string_lossy()),
        WalFrameEstimate::Unreadable
    );

    // (e) missing file
    let missing = dir.path().join("does-not-exist.wal");
    assert_eq!(
        estimate_wal_frames(&missing.to_string_lossy()),
        WalFrameEstimate::Unreadable
    );

    // (f) positive control: page_size 4096, 5 frames -> 32 + 5*(4096+24) = 20632
    let valid = write_fake_wal(
        &dir,
        "valid.wal",
        32 + 5 * (4096 + 24),
        Some(0x377f_0682),
        Some(4096),
    );
    assert_eq!(
        estimate_wal_frames(&valid.to_string_lossy()),
        WalFrameEstimate::Frames(5)
    );

    // (g) page_size field 1 normalizes to 65536: 32 + 2*(65536+24) = 131152 -> 2 frames
    let big_page = write_fake_wal(
        &dir,
        "bigpage.wal",
        32 + 2 * (65536 + 24),
        Some(0x377f_0683),
        Some(1),
    );
    assert_eq!(
        estimate_wal_frames(&big_page.to_string_lossy()),
        WalFrameEstimate::Frames(2)
    );
}

// ---------------------------------------------------------------------------
// T4 — rollback journal present + skip ON → NOT skipped (fallback)
// ---------------------------------------------------------------------------

#[test]
fn t4_rollback_journal_forces_fallback() {
    let dir = TempDir::new().expect("tempdir");
    let (_writer, path) = dirty_wal_db(&dir, "t4.db", 8);

    // An EMPTY `<db>-journal` sentinel: present on disk (so the guard's
    // `Path::exists` is true) but zero-length, so SQLite treats it as a non-hot
    // journal and performs no exclusive-lock rollback — avoiding lock contention
    // with the held writer. A non-empty journal would look "hot" and make the
    // second connection attempt a rollback that the writer blocks (SQLITE_BUSY).
    // The guard must bypass the skip on mere existence regardless of WAL size.
    let journal = format!("{}-journal", path.to_string_lossy());
    std::fs::File::create(&journal).expect("create empty journal sentinel");

    let action = recover(&path, true).expect("recovery must succeed");
    assert_ne!(
        action,
        WalRecoveryAction::SkippedCheckpoint,
        "a present rollback journal must force the checkpoint fallback"
    );
}

// ---------------------------------------------------------------------------
// T5 — gate OFF (default) behaves like today; child-process proves the real
//      env -> gate -> decision wiring on the public path.
// ---------------------------------------------------------------------------

#[test]
fn t5_gate_off_checkpoints_like_legacy() {
    let dir = TempDir::new().expect("tempdir");
    let (_writer, path) = dirty_wal_db(&dir, "t5.db", 8);

    let action = recover(&path, false).expect("recovery must succeed");
    assert_eq!(
        action,
        WalRecoveryAction::Checkpointed,
        "with skip disabled, a small dirty WAL must take the legacy PASSIVE checkpoint path"
    );
}

/// Child entrypoint: build a small dirty WAL, read the REAL gate, run recovery,
/// print `gate;action`. Returns early (no-op) in the parent process.
#[test]
fn round8_wal_recovery_child() {
    if std::env::var(CHILD_MODE_ENV).is_err() {
        return;
    }
    let dir = TempDir::new().expect("child tempdir");
    let (writer, path) = dirty_wal_db(&dir, "child.db", 8);
    let gate = skip_startup_wal_checkpoint_enabled_for_test();
    let action = recover(&path, gate).expect("child recovery must succeed");
    drop(writer);
    let action_str = match action {
        WalRecoveryAction::SkippedCheckpoint => "skipped",
        WalRecoveryAction::Checkpointed => "checkpointed",
        WalRecoveryAction::Truncated => "truncated",
    };
    println!("{CHILD_LINE_PREFIX}{gate};{action_str}");
}

fn run_child(gate_env: Option<&str>) -> (bool, String) {
    let mut command = Command::new(std::env::current_exe().expect("current test exe"));
    command
        .arg("--exact")
        .arg("round8_wal_recovery_child")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(CHILD_MODE_ENV, "1")
        .env_remove(MOONSHOT_ALL_ENV);
    match gate_env {
        Some(value) => {
            command.env(SKIP_GATE_ENV, value);
        }
        None => {
            command.env_remove(SKIP_GATE_ENV);
        }
    }
    let output = command.output().expect("run child");
    assert!(
        output.status.success(),
        "child failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("child stdout utf8");
    let line = stdout
        .lines()
        .find_map(|l| {
            l.find(CHILD_LINE_PREFIX)
                .map(|p| &l[p + CHILD_LINE_PREFIX.len()..])
        })
        .unwrap_or_else(|| panic!("missing child line in stdout:\n{stdout}"));
    let (gate, action) = line.split_once(';').expect("malformed child line");
    (gate.trim() == "true", action.trim().to_string())
}

#[test]
fn t5_child_public_path_default_on_and_opt_out() {
    // ft-yjihu.1 round-9 promotion: the gate is now default-ON (opt-out). Default
    // (no gate env): the public path must SKIP a small healthy WAL.
    let (gate_default, action_default) = run_child(None);
    assert!(
        gate_default,
        "unset {SKIP_GATE_ENV} must default the gate ON (round-9 promotion)"
    );
    assert_eq!(
        action_default, "skipped",
        "default-on must skip a small healthy WAL"
    );

    // Gate explicitly on (=1): the real env -> gate -> decision wiring must skip.
    let (gate_on, action_on) = run_child(Some("1"));
    assert!(gate_on, "{SKIP_GATE_ENV}=1 must enable the gate");
    assert_eq!(
        action_on, "skipped",
        "gate on must skip a small healthy WAL end-to-end"
    );

    // Falsey value is the opt-OUT: restores the legacy always-checkpoint path.
    let (gate_off, action_off) = run_child(Some("0"));
    assert!(!gate_off, "{SKIP_GATE_ENV}=0 must opt OUT (gate OFF)");
    assert_eq!(
        action_off, "checkpointed",
        "opt-out value must take the checkpoint path"
    );
}

// ---------------------------------------------------------------------------
// T6 — corruption (quick_check != "ok") fails closed under BOTH gate states
// ---------------------------------------------------------------------------

/// Build a non-WAL DB with content spanning several pages, then overwrite a
/// mid-file B-tree region so `PRAGMA quick_check` reports corruption while the
/// header stays valid enough to open.
fn corrupt_db(dir: &TempDir, name: &str) -> PathBuf {
    let path = dir.path().join(name);
    {
        let mut conn = Connection::open(&path).expect("open db to corrupt");
        conn.pragma_update(None, "journal_mode", "DELETE")
            .expect("rollback journal mode (no WAL)");
        conn.execute_batch("CREATE TABLE sample(id INTEGER PRIMARY KEY, body TEXT NOT NULL);")
            .expect("schema");
        let payload = "corruption fixture payload ".repeat(8);
        let tx = conn.transaction().expect("tx");
        for row in 0..400 {
            tx.execute(
                "INSERT INTO sample(id, body) VALUES (?1, ?2)",
                params![row as i64, payload],
            )
            .expect("insert");
        }
        tx.commit().expect("commit");
    } // connection dropped: data flushed to the main DB file, no WAL.

    // Overwrite a large mid-file region (well past the 100-byte header / page 1)
    // with 0xFF so quick_check flags corrupt pages.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open db file for corruption");
    file.seek(SeekFrom::Start(4096)).expect("seek into page 2");
    file.write_all(&[0xFFu8; 8192]).expect("clobber pages");
    file.flush().expect("flush corruption");
    path
}

fn assert_fails_closed(path: &Path, skip_enabled: bool) {
    let result = recover(path, skip_enabled);
    match result {
        Ok(action) => {
            panic!("corrupt DB must fail closed (skip_enabled={skip_enabled}), got Ok({action:?})")
        }
        Err(err) => {
            // Preferred surface is StorageError::Corruption; some SQLite builds may
            // surface the corrupt page as an integrity-check Database error instead.
            // Either way the contract is: an error, never a silent skip.
            let is_corruption = matches!(err, Error::Storage(StorageError::Corruption { .. }));
            let is_db_err = matches!(err, Error::Storage(StorageError::Database(_)));
            assert!(
                is_corruption || is_db_err,
                "expected a Corruption/Database storage error, got {err:?}"
            );
        }
    }
}

#[test]
fn t6_corruption_fails_closed_regardless_of_gate() {
    let dir = TempDir::new().expect("tempdir");
    let corrupt = corrupt_db(&dir, "t6.db");
    // Fresh copies per arm so the first quick_check can't influence the second.
    assert_fails_closed(&corrupt, true);

    let dir2 = TempDir::new().expect("tempdir2");
    let corrupt2 = corrupt_db(&dir2, "t6b.db");
    assert_fails_closed(&corrupt2, false);
}
