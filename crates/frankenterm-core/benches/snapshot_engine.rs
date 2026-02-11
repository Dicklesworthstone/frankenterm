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
        "PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=5000;

         CREATE TABLE mux_sessions (
             session_id TEXT PRIMARY KEY,
             created_at INTEGER NOT NULL,
             last_checkpoint_at INTEGER,
             shutdown_clean INTEGER NOT NULL DEFAULT 0,
             topology_json TEXT NOT NULL,
             window_metadata_json TEXT,
             ft_version TEXT NOT NULL,
             host_id TEXT
         );

         CREATE TABLE session_checkpoints (
             id INTEGER PRIMARY KEY,
             session_id TEXT NOT NULL,
             checkpoint_at INTEGER NOT NULL,
             checkpoint_type TEXT,
             state_hash TEXT NOT NULL,
             pane_count INTEGER NOT NULL,
             total_bytes INTEGER NOT NULL,
             metadata_json TEXT
         );

         CREATE TABLE mux_pane_state (
             id INTEGER PRIMARY KEY,
             checkpoint_id INTEGER NOT NULL,
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
         CREATE INDEX idx_pane_state_checkpoint ON mux_pane_state(checkpoint_id);",
    )
    .unwrap();

    // Insert a session
    conn.execute(
        "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version)
         VALUES ('bench-session', ?1, '{}', '0.1.0')",
        [now_ms() as i64],
    )
    .unwrap();

    std::mem::forget(dir);
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
