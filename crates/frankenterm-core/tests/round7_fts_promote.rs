// The Send proof for this file's async fixtures walks the storage-open chain
// and, on nightly-2026-08-31 on macOS, exhausts the default 128-step budget
// ("overflow evaluating the requirement ... : Send"). The same code type-checks
// on the Linux proof workers, so this is the solver's depth budget, not a cycle;
// frankenterm-core's lib crate has carried the same raise for a while.
#![recursion_limit = "256"]

//! Round-7 EV4 promotion keep-gate.
//!
//! The process environment is global, and this workspace forbids unsafe code, so
//! env-gate cases run in child test processes via `Command::env`.

#![cfg(feature = "asupersync-runtime")]

use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::storage::{
    FtsSyncConfig, PaneRecord, SearchOptions, SearchResult, StorageConfig, StorageHandle, now_ms,
};
use serde::{Deserialize, Serialize};
use std::process::Command;
use tempfile::TempDir;

const CHILD_MODE_ENV: &str = "FT_ROUND7_FTS_PROMOTE_CHILD";
const EV4_GATE_ENV: &str = "FT_MOONSHOT_FTS_INSERT_SELECT_BATCH";
const MOONSHOT_ALL_ENV: &str = "FT_MOONSHOT_ALL";
const SNAPSHOT_PREFIX: &str = "ROUND7_FTS_PROMOTE_JSON:";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SyncShape {
    segments_indexed: u64,
    panes_processed: u64,
    full_rebuild: bool,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SearchProjection {
    segment_id: i64,
    pane_id: u64,
    seq: u64,
    content: String,
    snippet: Option<String>,
    highlight: Option<String>,
    score_bits: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SearchCaseProjection {
    case_name: String,
    rows: Vec<SearchProjection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Round7FtsPromoteSnapshot {
    mode: String,
    gate_enabled: bool,
    sync: SyncShape,
    second_sync: SyncShape,
    searches: Vec<SearchCaseProjection>,
}

fn runtime() -> frankenterm_core::runtime_async::Runtime {
    RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime")
}

fn temp_db() -> (TempDir, String) {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir
        .path()
        .join("round7-fts-promote.db")
        .to_string_lossy()
        .to_string();
    (dir, path)
}

fn pane(pane_id: u64) -> PaneRecord {
    let now = now_ms();
    PaneRecord {
        pane_id,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: Some(format!("round7-pane-{pane_id}")),
        cwd: Some("/tmp/round7".to_string()),
        tty_name: None,
        first_seen_at: now,
        last_seen_at: now,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

fn sync_shape(result: frankenterm_core::storage::FtsSyncResult) -> SyncShape {
    SyncShape {
        segments_indexed: result.segments_indexed,
        panes_processed: result.panes_processed,
        full_rebuild: result.full_rebuild,
        warnings: result.warnings,
    }
}

fn project(results: Vec<SearchResult>) -> Vec<SearchProjection> {
    results
        .into_iter()
        .map(|result| SearchProjection {
            segment_id: result.segment.id,
            pane_id: result.segment.pane_id,
            seq: result.segment.seq,
            content: result.segment.content,
            snippet: result.snippet,
            highlight: result.highlight,
            score_bits: result.score.to_bits(),
        })
        .collect()
}

async fn capture_snapshot(mode: &str) -> Round7FtsPromoteSnapshot {
    let (_dir, db_path) = temp_db();
    let storage = StorageHandle::with_config(
        &db_path,
        StorageConfig {
            defer_fts_triggers: true,
            ..StorageConfig::default()
        },
    )
    .await
    .expect("open deferred FTS storage");

    storage.upsert_pane(pane(1)).await.expect("upsert pane 1");
    storage.upsert_pane(pane(2)).await.expect("upsert pane 2");

    for (pane_id, zone, content) in [
        (1, "prompt", "needle alpha prompt transcript"),
        (1, "output", "needle beta output transcript"),
        (1, "output", "control row without match"),
        (2, "output", "needle gamma output transcript"),
        (2, "prompt", "needle delta prompt transcript"),
        (1, "output", "needle epsilon output transcript"),
    ] {
        storage
            .append_segment_with_zone(pane_id, content, None, Some(zone))
            .await
            .expect("append zoned segment");
    }

    let config = FtsSyncConfig {
        batch_size: 2,
        max_batch_bytes: 128,
        commit_progress: true,
    };
    let sync = sync_shape(storage.sync_fts(config.clone()).await.expect("sync FTS"));
    let second_sync = sync_shape(
        storage
            .sync_fts(config)
            .await
            .expect("second sync should be no-op"),
    );

    let search_cases = [
        (
            "all",
            SearchOptions {
                limit: Some(10),
                highlight_prefix: Some("[[".to_string()),
                highlight_suffix: Some("]]".to_string()),
                snippet_max_tokens: Some(6),
                ..SearchOptions::default()
            },
        ),
        (
            "pane",
            SearchOptions {
                pane_id: Some(1),
                limit: Some(10),
                highlight_prefix: Some("[[".to_string()),
                highlight_suffix: Some("]]".to_string()),
                snippet_max_tokens: Some(6),
                ..SearchOptions::default()
            },
        ),
        (
            "zone",
            SearchOptions {
                zone_type: Some("output".to_string()),
                limit: Some(10),
                highlight_prefix: Some("[[".to_string()),
                highlight_suffix: Some("]]".to_string()),
                snippet_max_tokens: Some(6),
                ..SearchOptions::default()
            },
        ),
        (
            "pane-zone-time",
            SearchOptions {
                pane_id: Some(1),
                zone_type: Some("output".to_string()),
                since: Some(0),
                until: Some(i64::MAX),
                limit: Some(10),
                highlight_prefix: Some("[[".to_string()),
                highlight_suffix: Some("]]".to_string()),
                snippet_max_tokens: Some(6),
                ..SearchOptions::default()
            },
        ),
    ];

    let mut searches = Vec::with_capacity(search_cases.len());
    for (case_name, options) in search_cases {
        let rows = storage
            .search_with_results("needle", options)
            .await
            .map(project)
            .expect("search FTS");
        searches.push(SearchCaseProjection {
            case_name: case_name.to_string(),
            rows,
        });
    }

    storage.shutdown().await.expect("shutdown storage");

    Round7FtsPromoteSnapshot {
        mode: mode.to_string(),
        gate_enabled: frankenterm_core::storage::fts_insert_select_batch_enabled_for_test(),
        sync,
        second_sync,
        searches,
    }
}

#[test]
fn round7_fts_promote_child_capture() {
    let Ok(mode) = std::env::var(CHILD_MODE_ENV) else {
        return;
    };
    let snapshot = runtime().block_on(capture_snapshot(&mode));
    println!(
        "{SNAPSHOT_PREFIX}{}",
        serde_json::to_string(&snapshot).expect("serialize snapshot")
    );
}

fn run_child(mode: &str, gate_override: Option<&str>) -> Round7FtsPromoteSnapshot {
    let mut command = Command::new(std::env::current_exe().expect("current test exe"));
    command
        .arg("--exact")
        .arg("round7_fts_promote_child_capture")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(CHILD_MODE_ENV, mode)
        .env_remove(MOONSHOT_ALL_ENV);
    match gate_override {
        Some(value) => {
            command.env(EV4_GATE_ENV, value);
        }
        None => {
            command.env_remove(EV4_GATE_ENV);
        }
    }

    let output = command.output().expect("run child test process");
    assert!(
        output.status.success(),
        "child test failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("child stdout utf8");
    let json = stdout
        .lines()
        .find_map(|line| {
            line.find(SNAPSHOT_PREFIX)
                .map(|prefix| &line[prefix + SNAPSHOT_PREFIX.len()..])
        })
        .unwrap_or_else(|| panic!("missing snapshot line in child stdout:\n{stdout}"));
    serde_json::from_str(json).expect("deserialize child snapshot")
}

#[test]
fn round7_fts_promote_default_on_matches_disabled_per_row_oracle_small_batch() {
    let default_on = run_child("default-unset", None);
    let disabled = run_child("disabled-zero", Some("0"));

    assert!(
        default_on.gate_enabled,
        "unset {EV4_GATE_ENV} must default the EV4 set-based FTS batcher on"
    );
    assert!(
        !disabled.gate_enabled,
        "{EV4_GATE_ENV}=0 must keep the per-row oracle available as a safety valve"
    );

    assert_eq!(
        default_on.sync, disabled.sync,
        "small-batch default-on sync shape must match the per-row oracle"
    );
    assert_eq!(
        default_on.second_sync, disabled.second_sync,
        "progress replay must remain a no-op in both modes"
    );
    assert_eq!(
        default_on.searches, disabled.searches,
        "default-on set-based FTS must be byte-equivalent to per-row FTS for pane/zone/time searches"
    );

    assert_eq!(default_on.sync.segments_indexed, 6);
    assert_eq!(default_on.sync.panes_processed, 2);
    assert!(!default_on.sync.full_rebuild);
    assert_eq!(default_on.second_sync.segments_indexed, 0);
    assert_eq!(default_on.searches.len(), 4);
}
