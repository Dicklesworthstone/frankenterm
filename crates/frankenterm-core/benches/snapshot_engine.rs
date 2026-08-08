//! Benchmarks for the snapshot engine.
//!
//! Performance budgets:
//! - Snapshot capture (10 panes): **< 5ms**
//! - Snapshot capture (50 panes): **< 20ms**
//! - State hash computation: **< 100us**
//! - Checkpoint save to SQLite: **< 10ms**
//! - Checkpoint load from SQLite: **< 5ms**
//! - Dedup check: **< 50us**

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::session_pane_state::PaneStateSnapshot;
use frankenterm_core::session_topology::TopologySnapshot;
use frankenterm_core::wezterm::{PaneInfo, PaneSize};
use rusqlite::Connection;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "topology_from_panes",
        budget: "p50 < 1ms (capture topology from panes)",
    },
    bench_common::BenchBudget {
        name: "pane_state_from_info",
        budget: "p50 < 10us per pane (extract pane state)",
    },
    bench_common::BenchBudget {
        name: "state_hash",
        budget: "p50 < 100us (hash computation for dedup)",
    },
    bench_common::BenchBudget {
        name: "checkpoint_save",
        budget: "p50 < 10ms (SQLite transaction)",
    },
    bench_common::BenchBudget {
        name: "checkpoint_load",
        budget: "p50 < 5ms (SQLite query + deserialize)",
    },
];

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn generate_panes(count: usize) -> Vec<PaneInfo> {
    let mut panes = Vec::with_capacity(count);
    for i in 0..count {
        panes.push(PaneInfo {
            window_id: 0,
            tab_id: (i / 4) as u64,
            pane_id: i as u64,
            domain_id: None,
            domain_name: None,
            workspace: Some("default".to_string()),
            size: Some(PaneSize {
                rows: 24,
                cols: 80,
                pixel_width: Some(640),
                pixel_height: Some(384),
                dpi: None,
            }),
            rows: None,
            cols: None,
            title: Some(format!("pane-{i}")),
            cwd: Some(format!("file:///home/user/project-{i}")),
            tty_name: None,
            cursor_x: Some(0),
            cursor_y: Some(0),
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: i == 0,
            is_zoomed: false,
            extra: HashMap::new(),
        });
    }
    panes
}

fn setup_db() -> (String, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("bench.db").to_string_lossy().to_string();
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "PRAGMA foreign_keys=ON;
         PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=5000;

         CREATE TABLE mux_sessions (
             session_id TEXT PRIMARY KEY,
             created_at INTEGER NOT NULL,
             last_checkpoint_at INTEGER,
             shutdown_clean INTEGER NOT NULL DEFAULT 0,
             topology_json TEXT NOT NULL,
             window_metadata_json TEXT,
             ft_version TEXT NOT NULL,
             host_id TEXT,
             clean_checkpoint_id INTEGER
                 REFERENCES session_checkpoints(id) ON DELETE SET NULL
         );

         CREATE TABLE session_checkpoints (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id TEXT NOT NULL
                 REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
             checkpoint_at INTEGER NOT NULL,
             checkpoint_type TEXT NOT NULL
                 CHECK(checkpoint_type IN ('periodic','event','shutdown','startup')),
             state_hash TEXT NOT NULL,
             pane_count INTEGER NOT NULL,
             total_bytes INTEGER NOT NULL,
             metadata_json TEXT,
             checkpoint_role TEXT NOT NULL DEFAULT 'snapshot'
                 CHECK(checkpoint_role IN ('snapshot','restore_intent','restore_receipt')),
             topology_json TEXT,
             restore_intent_checkpoint_id INTEGER
                 REFERENCES session_checkpoints(id) ON DELETE CASCADE,
             CHECK(checkpoint_role = 'restore_receipt' OR restore_intent_checkpoint_id IS NULL)
         );

         CREATE TABLE restore_attempt_lifecycle (
             intent_checkpoint_id INTEGER PRIMARY KEY
                 REFERENCES session_checkpoints(id)
                 ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED,
             session_id TEXT NOT NULL
                 REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
             source_checkpoint_id INTEGER NOT NULL,
             outcome_checkpoint_id INTEGER
                 REFERENCES session_checkpoints(id) ON DELETE SET NULL,
             status TEXT NOT NULL
                 CHECK(status IN ('intent','outcome_complete','resolved','reconciliation_required')),
             created_at INTEGER NOT NULL,
             resolved_at INTEGER,
             CHECK(intent_checkpoint_id <> source_checkpoint_id),
             CHECK(outcome_checkpoint_id IS NULL OR outcome_checkpoint_id <> intent_checkpoint_id),
             CHECK(outcome_checkpoint_id IS NULL OR outcome_checkpoint_id <> source_checkpoint_id),
             CHECK(created_at >= 0),
             CHECK(resolved_at IS NULL OR resolved_at >= created_at),
             CHECK(
                 (status = 'intent'
                     AND outcome_checkpoint_id IS NULL
                     AND resolved_at IS NULL)
                 OR (status = 'outcome_complete'
                     AND outcome_checkpoint_id IS NOT NULL
                     AND resolved_at IS NULL)
                 OR (status = 'reconciliation_required'
                     AND resolved_at IS NULL)
                 OR (status = 'resolved'
                     AND resolved_at IS NOT NULL)
             )
         );

         CREATE TABLE mux_pane_state (
             id INTEGER PRIMARY KEY,
             checkpoint_id INTEGER NOT NULL
                 REFERENCES session_checkpoints(id) ON DELETE CASCADE,
             pane_id INTEGER NOT NULL,
             cwd TEXT,
             command TEXT,
             env_json TEXT,
             terminal_state_json TEXT NOT NULL,
             agent_metadata_json TEXT,
             scrollback_checkpoint_seq INTEGER,
             last_output_at INTEGER
         );

         CREATE INDEX idx_checkpoints_session ON session_checkpoints(session_id, checkpoint_at);
         CREATE INDEX idx_checkpoints_session_role_latest
             ON session_checkpoints(session_id, checkpoint_role, checkpoint_at DESC, id DESC);
         CREATE INDEX idx_checkpoints_session_role_causal
             ON session_checkpoints(session_id, checkpoint_role, id DESC);
         CREATE INDEX idx_checkpoints_global_latest
             ON session_checkpoints(checkpoint_at DESC, id DESC);
         CREATE INDEX idx_checkpoints_global_snapshot_latest
             ON session_checkpoints(checkpoint_at DESC, id DESC)
             WHERE checkpoint_role = 'snapshot';
         CREATE UNIQUE INDEX idx_checkpoints_restore_intent_outcome
             ON session_checkpoints(restore_intent_checkpoint_id)
             WHERE restore_intent_checkpoint_id IS NOT NULL;
         CREATE INDEX idx_mux_sessions_clean_checkpoint
             ON mux_sessions(clean_checkpoint_id);
         CREATE INDEX idx_restore_attempt_lifecycle_session_status
             ON restore_attempt_lifecycle(session_id, status, intent_checkpoint_id);
         CREATE UNIQUE INDEX idx_restore_attempt_lifecycle_outcome
             ON restore_attempt_lifecycle(outcome_checkpoint_id)
             WHERE outcome_checkpoint_id IS NOT NULL;
         CREATE INDEX idx_pane_state_checkpoint ON mux_pane_state(checkpoint_id);
         CREATE INDEX idx_pane_state_pane ON mux_pane_state(pane_id);",
    )
    .unwrap();
    conn.execute_batch(
        frankenterm_core::storage::migrations::session_retained_size_schema_sql()
            .expect("locate canonical v40 retained-size schema"),
    )
    .expect("install canonical v40 retained-size authority");

    // Insert a session
    conn.execute(
        "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version)
         VALUES ('bench-session', ?1, '{}', '0.1.0')",
        [now_ms() as i64],
    )
    .unwrap();

    let _ = dir.keep();
    (db_path, conn)
}

fn bench_topology_capture(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot/topology_capture");

    for &count in &[4, 10, 20, 50] {
        let panes = generate_panes(count);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &panes, |b, panes| {
            b.iter(|| {
                let ts = now_ms();
                TopologySnapshot::from_panes(panes, ts)
            });
        });
    }

    group.finish();
}

fn bench_pane_state_extraction(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot/pane_state_extraction");

    for &count in &[1, 10, 50] {
        let panes = generate_panes(count);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &panes, |b, panes| {
            b.iter(|| {
                let ts = now_ms();
                panes
                    .iter()
                    .map(|p| PaneStateSnapshot::from_pane_info(p, ts, false))
                    .collect::<Vec<_>>()
            });
        });
    }

    group.finish();
}

fn bench_state_hash(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot/state_hash");

    for &count in &[4, 10, 50] {
        let panes = generate_panes(count);
        let ts = now_ms();
        let (topology, _) = TopologySnapshot::from_panes(&panes, ts);
        let topo_json = topology.to_json().unwrap();
        let pane_states: Vec<PaneStateSnapshot> = panes
            .iter()
            .map(|p| PaneStateSnapshot::from_pane_info(p, ts, false))
            .collect();
        let pane_jsons: Vec<String> = pane_states.iter().map(|ps| ps.to_json().unwrap()).collect();

        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &(&topo_json, &pane_jsons),
            |b, &(topo, panes)| {
                b.iter(|| {
                    use std::collections::hash_map::DefaultHasher;
                    use std::hash::{Hash, Hasher};
                    let mut hasher = DefaultHasher::new();
                    topo.hash(&mut hasher);
                    for p in panes {
                        p.hash(&mut hasher);
                    }
                    hasher.finish()
                });
            },
        );
    }

    group.finish();
}

fn bench_checkpoint_save(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot/checkpoint_save");
    group.sample_size(30); // Reduce samples for I/O-bound benchmarks

    for &count in &[1, 10, 50] {
        let panes = generate_panes(count);
        let ts = now_ms();
        let pane_states: Vec<PaneStateSnapshot> = panes
            .iter()
            .map(|p| PaneStateSnapshot::from_pane_info(p, ts, false))
            .collect();

        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &pane_states,
            |b, states| {
                // Fresh DB per iteration to avoid unique constraint issues
                b.iter(|| {
                    let (db_path, conn) = setup_db();
                    let cp_ts = now_ms();

                    let tx = conn.unchecked_transaction().unwrap();
                    tx.execute(
                        "INSERT INTO session_checkpoints
                         (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes)
                         VALUES ('bench-session', ?1, 'periodic', 'hash', ?2, 0)",
                        rusqlite::params![cp_ts as i64, states.len() as i64],
                    )
                    .unwrap();
                    let cp_id = tx.last_insert_rowid();

                    for ps in states {
                        let ts_json = serde_json::to_string(&ps.terminal).unwrap();
                        tx.execute(
                            "INSERT INTO mux_pane_state
                             (checkpoint_id, pane_id, cwd, terminal_state_json)
                             VALUES (?1, ?2, ?3, ?4)",
                            rusqlite::params![cp_id, ps.pane_id as i64, ps.cwd, ts_json],
                        )
                        .unwrap();
                    }
                    tx.commit().unwrap();
                    drop(conn);
                    let _ = std::fs::remove_file(&db_path);
                });
            },
        );
    }

    group.finish();
}

fn bench_checkpoint_load(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot/checkpoint_load");
    group.sample_size(30);

    for &count in &[1, 10, 50] {
        // Set up a DB with data
        let (db_path, conn) = setup_db();
        let ts = now_ms();
        let panes = generate_panes(count);

        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes)
             VALUES ('bench-session', ?1, 'periodic', 'hash', ?2, 1024)",
            rusqlite::params![ts as i64, count as i64],
        )
        .unwrap();
        let cp_id = conn.last_insert_rowid();

        for p in &panes {
            let ts_json = r#"{"rows":24,"cols":80,"cursor_row":0,"cursor_col":0,"is_alt_screen":false,"title":"test"}"#;
            conn.execute(
                "INSERT INTO mux_pane_state
                 (checkpoint_id, pane_id, cwd, terminal_state_json)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![cp_id, p.pane_id as i64, &p.cwd, ts_json],
            )
            .unwrap();
        }

        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &db_path,
            |b, db_path| {
                b.iter(|| {
                    frankenterm_core::session_restore::load_latest_checkpoint(
                        db_path,
                        "bench-session",
                    )
                    .unwrap()
                });
            },
        );
    }

    group.finish();
}

fn bench_session_list(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot/session_list");
    group.sample_size(30);

    // Create DB with multiple sessions
    let (db_path, conn) = setup_db();
    for i in 0..20 {
        conn.execute(
            "INSERT OR IGNORE INTO mux_sessions (session_id, created_at, topology_json, ft_version)
             VALUES (?1, ?2, '{}', '0.1.0')",
            rusqlite::params![format!("sess-{i:04}"), (1700000000000i64 + i * 1000)],
        )
        .unwrap();
    }

    group.bench_function("20_sessions", |b| {
        b.iter(|| frankenterm_core::session_restore::list_sessions(&db_path).unwrap());
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("snapshot_engine", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_topology_capture,
        bench_pane_state_extraction,
        bench_state_hash,
        bench_checkpoint_save,
        bench_checkpoint_load,
        bench_session_list
);
criterion_main!(benches);
