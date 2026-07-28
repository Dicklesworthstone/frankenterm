//! Round-9 WAL skip-checkpoint promotion bench — bead `ft-yjihu.1`.
//!
//! The round-8 keep-ledger landed `FT_MOONSHOT_SKIP_STARTUP_WAL_CHECKPOINT`
//! default-OFF, with a Form-1 promotion predicate: promote to default-on only
//! after a deterministic startup-time measurement shows the skip path materially
//! faster on a dirty-WAL open with NO regression on the clean-start and
//! large-WAL paths (durability across crash/replay was already proven by the
//! `round8_wal_recovery` durability oracle).
//!
//! This is that measurement. The win (skip the ~6 ms startup PASSIVE checkpoint
//! on a small healthy WAL) is timed deterministically; the no-regression claim on
//! the clean and large-WAL paths is asserted via `WalRecoveryAction` branch
//! equivalence (skip-on takes the exact same branch as skip-off when it is not
//! eligible to skip), which is the honest observable — a PASSIVE checkpoint does
//! not resize the WAL, so filesystem inference is unreliable.
//!
//! Run (local Mac, deterministic timing):
//! ```text
//! cargo test --profile release-perf -p frankenterm-core \
//!   --test round9_wal_startup_time -- --nocapture
//! ```

#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::path::PathBuf;
use std::time::Instant;

use frankenterm_core::storage::{
    WalFrameEstimate, WalRecoveryAction, check_and_recover_wal_inner, estimate_wal_frames,
};
use frankenterm_core::storage_backend_trait::RusqliteBackend;
use rusqlite::{Connection, params};
use tempfile::TempDir;

// `WAL_RECOVERY_THRESHOLD` is `pub(crate)` (storage.rs:287); mirror it here
// (round8_wal_recovery.rs does the same — keep in sync).
const WAL_RECOVERY_THRESHOLD: i64 = 10_000;

const SMALL_ROWS: usize = 8_000; // ~4.7 MB WAL, <= threshold (round-7 dirty case)
const WARMUP_BLOCKS: usize = 4;
const BLOCKS: usize = 25;

/// Small dirty WAL (<= threshold). Writer `Connection` must be kept alive by the
/// caller (dropping it auto-checkpoints + removes the WAL fixture).
#[must_use]
fn small_dirty_wal(dir: &TempDir, name: &str) -> (Connection, PathBuf) {
    let path = dir.path().join(name);
    let mut conn = Connection::open(&path).expect("open dirty WAL db");
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("enable WAL");
    conn.pragma_update(None, "wal_autocheckpoint", 0)
        .expect("disable autocheckpoint");
    conn.execute_batch("CREATE TABLE sample(id INTEGER PRIMARY KEY, body TEXT NOT NULL);")
        .expect("create schema");
    let payload = "round9 dirty WAL frame payload ".repeat(8);
    let tx = conn.transaction().expect("begin tx");
    for row in 0..SMALL_ROWS {
        tx.execute(
            "INSERT INTO sample(id, body) VALUES (?1, ?2)",
            params![row as i64, payload],
        )
        .expect("insert row");
    }
    tx.commit().expect("commit rows");
    (conn, path)
}

/// Over-threshold WAL (512-byte pages, self-tuning until estimate clears the
/// threshold). Mirrors `round8_wal_recovery::large_dirty_wal_db`.
#[must_use]
fn large_dirty_wal(dir: &TempDir, name: &str) -> (Connection, PathBuf) {
    let path = dir.path().join(name);
    let mut conn = Connection::open(&path).expect("open large WAL db");
    conn.pragma_update(None, "page_size", 512)
        .expect("set 512-byte pages");
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("enable WAL");
    conn.pragma_update(None, "wal_autocheckpoint", 0)
        .expect("disable autocheckpoint");
    conn.execute_batch("CREATE TABLE sample(id INTEGER PRIMARY KEY, body TEXT NOT NULL);")
        .expect("create schema");
    let wal_path = format!("{}-wal", path.to_string_lossy());
    let payload = "x".repeat(1_500);
    let target = WAL_RECOVERY_THRESHOLD + 1_500;
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
    panic!("could not grow WAL past {target} frames");
}

/// Clean DB: rollback-journal mode, no `-wal` file at all.
#[must_use]
fn clean_db(dir: &TempDir, name: &str) -> PathBuf {
    let path = dir.path().join(name);
    let conn = Connection::open(&path).expect("open clean db");
    conn.execute_batch(
        "CREATE TABLE sample(id INTEGER PRIMARY KEY, body TEXT NOT NULL);
         INSERT INTO sample(body) VALUES ('clean startup row');",
    )
    .expect("seed clean db");
    drop(conn);
    path
}

/// Time one `check_and_recover_wal_inner` over a freshly built small dirty WAL.
fn time_small_recover(skip: bool, idx: usize) -> (u128, WalRecoveryAction) {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_writer, path) = small_dirty_wal(&dir, &format!("small_{skip}_{idx}.db"));
    let db_path = path.to_string_lossy().to_string();
    let conn = Connection::open(&path).expect("open backend conn");
    let backend = RusqliteBackend::new(conn);
    let start = Instant::now();
    let action = check_and_recover_wal_inner(&backend, &db_path, skip).expect("recover ok");
    let ns = start.elapsed().as_nanos();
    (ns, action)
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = xs.len();
    if n == 0 {
        0.0
    } else if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2.0
    }
}

fn cv(xs: &[f64]) -> f64 {
    let n = xs.len() as f64;
    if n == 0.0 {
        return 0.0;
    }
    let mean = xs.iter().sum::<f64>() / n;
    if mean == 0.0 {
        return 0.0;
    }
    let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    var.sqrt() / mean
}

#[test]
fn wal_skip_startup_time_ab_and_no_regression() {
    // ── Warmup ──
    for i in 0..WARMUP_BLOCKS {
        let _ = time_small_recover(false, 10_000 + i);
        let _ = time_small_recover(true, 20_000 + i);
    }

    // ── Small dirty WAL A/B (the win) ──
    let mut off_ns = Vec::with_capacity(BLOCKS);
    let mut on_ns = Vec::with_capacity(BLOCKS);
    for i in 0..BLOCKS {
        let (off, off_action) = time_small_recover(false, i);
        let (on, on_action) = time_small_recover(true, i);
        assert_eq!(
            off_action,
            WalRecoveryAction::Checkpointed,
            "small dirty WAL with skip OFF must PASSIVE-checkpoint"
        );
        assert_eq!(
            on_action,
            WalRecoveryAction::SkippedCheckpoint,
            "small healthy dirty WAL with skip ON must skip the checkpoint"
        );
        off_ns.push(off as f64);
        on_ns.push(on as f64);
    }
    let off_med = median(off_ns.clone());
    let on_med = median(on_ns.clone());
    let off_cv = cv(&off_ns);
    let on_cv = cv(&on_ns);
    let speedup = if off_med == 0.0 {
        0.0
    } else {
        (off_med - on_med) / off_med
    };

    // ── No regression on clean + large paths (branch equivalence) ──
    let clean_dir = tempfile::tempdir().expect("tempdir");
    let clean_path = clean_db(&clean_dir, "clean.db");
    let clean_db_path = clean_path.to_string_lossy().to_string();
    let clean_off = {
        let backend = RusqliteBackend::new(Connection::open(&clean_path).unwrap());
        check_and_recover_wal_inner(&backend, &clean_db_path, false).expect("clean off")
    };
    let clean_on = {
        let backend = RusqliteBackend::new(Connection::open(&clean_path).unwrap());
        check_and_recover_wal_inner(&backend, &clean_db_path, true).expect("clean on")
    };
    assert_eq!(
        clean_off, clean_on,
        "clean-start: skip ON must take the same branch as skip OFF (no regression)"
    );

    let large_dir = tempfile::tempdir().expect("tempdir");
    let (_lw_off, large_off_path) = large_dirty_wal(&large_dir, "large_off.db");
    let large_off_db = large_off_path.to_string_lossy().to_string();
    let large_off = {
        let backend = RusqliteBackend::new(Connection::open(&large_off_path).unwrap());
        check_and_recover_wal_inner(&backend, &large_off_db, false).expect("large off")
    };
    let large_dir2 = tempfile::tempdir().expect("tempdir");
    let (_lw_on, large_on_path) = large_dirty_wal(&large_dir2, "large_on.db");
    let large_on_db = large_on_path.to_string_lossy().to_string();
    let large_on = {
        let backend = RusqliteBackend::new(Connection::open(&large_on_path).unwrap());
        check_and_recover_wal_inner(&backend, &large_on_db, true).expect("large on")
    };
    assert_eq!(
        large_off, large_on,
        "large WAL (> threshold): skip ON must fall back to the same branch as skip OFF"
    );
    assert_eq!(
        large_off,
        WalRecoveryAction::Truncated,
        "over-threshold WAL must PASSIVE+TRUNCATE"
    );

    let cv_ok = off_cv <= 0.10 && on_cv <= 0.10; // startup I/O is noisier than CPU micro; 10% band
    // "materially faster" gate: skip path saves a clear majority of the checkpoint cost.
    let material = speedup >= 0.30 && (off_med - on_med) >= 1_000_000.0; // >=30% AND >=1ms absolute
    let promote = cv_ok && material;

    println!("\n=== ROUND-9 WAL skip-checkpoint startup-time A/B (ft-yjihu.1) ===");
    println!(
        "small dirty WAL ({SMALL_ROWS} rows): skip_OFF median {:.1} µs (cv {:.2}%)  vs  \
         skip_ON median {:.1} µs (cv {:.2}%)",
        off_med / 1000.0,
        off_cv * 100.0,
        on_med / 1000.0,
        on_cv * 100.0
    );
    println!(
        "skip speedup: {:+.2}%  absolute saved: {:.3} ms  cv_ok(<=10%): {cv_ok}  material(>=30% & >=1ms): {material}",
        speedup * 100.0,
        (off_med - on_med) / 1_000_000.0
    );
    println!("clean-start branch: OFF={clean_off:?} ON={clean_on:?} (equal => no regression)");
    println!("large-WAL branch: OFF={large_off:?} ON={large_on:?} (equal => no regression)");
    println!("VERDICT: promote_wal_skip_default_on = {promote}");
    println!(
        "ROUND9_WAL_JSON {{\"schema\":\"round9.wal_startup.v1\",\
         \"skip_off_median_ns\":{off_med:.0},\"skip_on_median_ns\":{on_med:.0},\
         \"skip_off_cv\":{off_cv:.4},\"skip_on_cv\":{on_cv:.4},\
         \"speedup\":{speedup:.6},\"abs_saved_ms\":{:.4},\"cv_ok\":{cv_ok},\
         \"material\":{material},\"clean_branch\":\"{clean_off:?}\",\
         \"large_branch\":\"{large_off:?}\",\"promote\":{promote}}}",
        (off_med - on_med) / 1_000_000.0
    );

    // The branch-equivalence (no-regression) assertions above are the hard gate
    // and already ran. The timing VERDICT is advisory evidence for the ledger; do
    // not fail the test on a noisy host (cv/material are reported, not asserted).
}
