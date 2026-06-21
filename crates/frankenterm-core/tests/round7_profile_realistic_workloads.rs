//! Round-7 new-axis profile sweep for bead `ft-mcz7t`.
//!
//! This is intentionally an evidence harness, not an optimization. It reuses
//! the round-6 B0 shape: warmed leaf-call timings are weighted by an explicit
//! fleet-minute model, then scored against the >=0.5% self-time gate. The
//! candidate frames are the round-7 new axes:
//!
//! - startup WAL recovery: `storage::check_and_recover_wal`
//! - burst EventBus fanout: `events::EventBus::publish`
//! - per-capture BOCPD: `bocpd::BocpdManager::observe_text_chunk`
//!
//! Context frames (`scan_pipeline.process`, `redactor.redact`) are retained in
//! the denominator so sub-gate verdicts are not inflated by measuring only the
//! new candidates.

#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use frankenterm_core::bocpd::{BocpdConfig, BocpdManager};
use frankenterm_core::events::{Event, EventBus};
use frankenterm_core::patterns::{AgentType, Detection, Severity};
use frankenterm_core::redactor::Redactor;
use frankenterm_core::scan_pipeline::{ScanPipeline, ScanPipelineConfig};
use frankenterm_core::storage::check_and_recover_wal;
use frankenterm_core::storage_backend_trait::RusqliteBackend;
use rusqlite::{params, Connection};

const CAPTURE_DELTAS_PER_SEC: u64 = 192;
const REDACT_READS_PER_SEC: u64 = 64;
const DETECTION_BURST_PUBLISHES_PER_SEC: u64 = 16;
const STARTUP_RECOVERIES_PER_MIN: u64 = 1;
const WINDOW_SECS: u64 = 60;
const GATE_SHARE: f64 = 0.005;

const WARMUP: usize = 1_000;
const ITERS: usize = 8_000;
const WAL_CASES: usize = 6;
const DIRTY_WAL_ROWS: usize = 8_000;

struct Frame {
    name: &'static str,
    location: &'static str,
    workload: &'static str,
    candidate: bool,
    calls_measured: u64,
    total_nanos: u128,
    calls_per_min: u64,
    notes: &'static str,
}

impl Frame {
    fn mean_nanos(&self) -> f64 {
        if self.calls_measured == 0 {
            0.0
        } else {
            self.total_nanos as f64 / self.calls_measured as f64
        }
    }

    fn realistic_self_ns(&self) -> f64 {
        self.mean_nanos() * self.calls_per_min as f64
    }
}

struct WalCase {
    _dir: tempfile::TempDir,
    path: PathBuf,
    _writer: Option<Connection>,
    wal_bytes: u64,
}

fn measure(mut body: impl FnMut(), warmup: usize, iters: usize) -> (u64, u128) {
    for _ in 0..warmup {
        body();
    }
    let start = Instant::now();
    for _ in 0..iters {
        body();
    }
    (iters as u64, start.elapsed().as_nanos())
}

fn mixed_terminal_frame() -> Vec<u8> {
    "\x1b[32m   Compiling\x1b[0m frankenterm-core v0.9.0\n\
warning: field is never read: `tmp`\n\
\x1b[31merror[E0277]\x1b[0m: the trait bound is not satisfied\n\
    Finished dev [unoptimized] target(s) in 1.04s\n"
        .repeat(8)
        .into_bytes()
}

fn secret_dense_text() -> String {
    format!(
        "{}\nleaked sk-proj-abcdefghijklmnopqrstuvwxyz012345 and AKIAIOSFODNN7EXAMPLE token\n",
        "normal agent log output line with no secret ".repeat(20)
    )
}

fn detection(seq: u64) -> Detection {
    Detection {
        rule_id: "core.codex:usage_reached".to_string(),
        agent_type: AgentType::Codex,
        event_type: "usage_reached".to_string(),
        severity: Severity::Warning,
        confidence: 0.875,
        extracted: serde_json::json!({ "seq": seq, "reset_at": "2026-06-21T12:00:00Z" }),
        matched_text: "Usage limit reached".to_string(),
        span: (0, 19),
    }
}

fn pattern_event(seq: u64) -> Event {
    Event::PatternDetected {
        pane_id: seq % 64,
        pane_uuid: Some(format!("pane-{seq:08x}")),
        detection: detection(seq),
        event_id: Some(i64::try_from(seq).unwrap_or(i64::MAX)),
    }
}

fn bocpd_chunks() -> [&'static str; 4] {
    [
        "   Compiling frankenterm-core v0.9.0\n",
        "\x1b[32mtest storage::wal_recovery ... ok\x1b[0m\n",
        "warning: unused import in round7 profile harness\n",
        "Usage limit reached. Try again at 2026-06-21 12:00 UTC\n",
    ]
}

fn recover_wal_once(path: &Path) {
    let conn = Connection::open(path).expect("open WAL recovery profile DB");
    let backend = RusqliteBackend::new(conn);
    let db_path = path.to_string_lossy();
    check_and_recover_wal(&backend, &db_path).expect("WAL recovery must succeed");
    black_box(backend.into_connection());
}

fn clean_wal_case(idx: usize) -> WalCase {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("round7_clean_{idx}.db"));
    {
        let conn = Connection::open(&path).expect("open clean DB");
        conn.execute_batch(
            "CREATE TABLE sample(id INTEGER PRIMARY KEY, body TEXT NOT NULL);
             INSERT INTO sample(body) VALUES ('clean startup row');",
        )
        .expect("seed clean DB");
    }
    WalCase {
        _dir: dir,
        path,
        _writer: None,
        wal_bytes: 0,
    }
}

fn dirty_wal_case(idx: usize) -> WalCase {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("round7_dirty_{idx}.db"));
    let mut conn = Connection::open(&path).expect("open dirty DB");
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("enable WAL");
    conn.pragma_update(None, "wal_autocheckpoint", 0)
        .expect("disable auto-checkpoint");
    conn.execute_batch("CREATE TABLE sample(id INTEGER PRIMARY KEY, body TEXT NOT NULL);")
        .expect("create dirty DB schema");
    {
        let tx = conn.transaction().expect("dirty WAL transaction");
        let payload = "dirty startup WAL frame payload ".repeat(16);
        for row in 0..DIRTY_WAL_ROWS {
            tx.execute(
                "INSERT INTO sample(id, body) VALUES (?1, ?2)",
                params![row as i64, payload],
            )
            .expect("insert dirty WAL row");
        }
        tx.commit().expect("commit dirty WAL rows");
    }
    let wal_path = format!("{}-wal", path.to_string_lossy());
    let wal_bytes = std::fs::metadata(&wal_path).map_or(0, |meta| meta.len());
    assert!(wal_bytes > 0, "dirty WAL profile case produced no WAL file");
    WalCase {
        _dir: dir,
        path,
        _writer: Some(conn),
        wal_bytes,
    }
}

fn measure_wal_cases(mut make_case: impl FnMut(usize) -> WalCase) -> (u64, u128, u64) {
    let cases: Vec<WalCase> = (0..WAL_CASES).map(&mut make_case).collect();
    let avg_wal_bytes = cases.iter().map(|case| case.wal_bytes).sum::<u64>() / WAL_CASES as u64;
    let start = Instant::now();
    for case in &cases {
        recover_wal_once(&case.path);
    }
    (WAL_CASES as u64, start.elapsed().as_nanos(), avg_wal_bytes)
}

fn run_profile() -> (Vec<Frame>, u64) {
    let pipeline = ScanPipeline::new(ScanPipelineConfig::default());
    let mixed = mixed_terminal_frame();
    let (scan_calls, scan_ns) = measure(
        || {
            black_box(pipeline.process(black_box(&mixed)));
        },
        WARMUP,
        ITERS,
    );

    let redactor = Redactor::new();
    let secret = secret_dense_text();
    let (redact_calls, redact_ns) = measure(
        || {
            black_box(redactor.redact(black_box(&secret)));
        },
        WARMUP,
        ITERS,
    );

    let (clean_calls, clean_ns, _) = measure_wal_cases(clean_wal_case);
    let (dirty_calls, dirty_ns, avg_dirty_wal_bytes) = measure_wal_cases(dirty_wal_case);

    let bus = EventBus::new(4_096);
    let _all_sub = bus.subscribe();
    let _detection_sub = bus.subscribe_detections();
    let mut seq = 0_u64;
    let (event_calls, event_ns) = measure(
        || {
            let event = pattern_event(seq);
            seq = seq.wrapping_add(1);
            black_box(bus.publish(black_box(event)));
        },
        WARMUP,
        ITERS,
    );

    let mut manager = BocpdManager::new(BocpdConfig::default());
    for pane_id in 0..64 {
        manager.register_pane(pane_id);
    }
    let chunks = bocpd_chunks();
    let mut idx = 0usize;
    let (bocpd_calls, bocpd_ns) = measure(
        || {
            let pane_id = (idx % 64) as u64;
            let chunk = chunks[idx % chunks.len()];
            idx = idx.wrapping_add(1);
            black_box(manager.observe_text_chunk(
                pane_id,
                black_box(chunk),
                Duration::from_millis(250),
            ));
        },
        WARMUP,
        ITERS,
    );

    let capture_per_min = CAPTURE_DELTAS_PER_SEC * WINDOW_SECS;
    let redact_per_min = REDACT_READS_PER_SEC * WINDOW_SECS;
    let event_per_min = DETECTION_BURST_PUBLISHES_PER_SEC * WINDOW_SECS;

    let frames = vec![
        Frame {
            name: "scan_pipeline.process",
            location: "scan_pipeline.rs:528",
            workload: "round6 denominator: capture scan",
            candidate: false,
            calls_measured: scan_calls,
            total_nanos: scan_ns,
            calls_per_min: capture_per_min,
            notes: "denominator",
        },
        Frame {
            name: "redactor.redact",
            location: "redactor.rs:690",
            workload: "round6 denominator: outbound read redaction",
            candidate: false,
            calls_measured: redact_calls,
            total_nanos: redact_ns,
            calls_per_min: redact_per_min,
            notes: "denominator",
        },
        Frame {
            name: "storage.wal_recovery_clean",
            location: "storage.rs:1647",
            workload: "startup clean DB",
            candidate: true,
            calls_measured: clean_calls,
            total_nanos: clean_ns,
            calls_per_min: STARTUP_RECOVERIES_PER_MIN,
            notes: "startup clean contrast",
        },
        Frame {
            name: "storage.wal_recovery_dirty",
            location: "storage.rs:1647",
            workload: "startup WAL-dirty DB",
            candidate: true,
            calls_measured: dirty_calls,
            total_nanos: dirty_ns,
            calls_per_min: STARTUP_RECOVERIES_PER_MIN,
            notes: "startup dirty contrast",
        },
        Frame {
            name: "events.event_bus_publish",
            location: "events.rs:1280",
            workload: "burst pattern detection fanout",
            candidate: true,
            calls_measured: event_calls,
            total_nanos: event_ns,
            calls_per_min: event_per_min,
            notes: "live via runtime/ipc/bridge publishers",
        },
        Frame {
            name: "bocpd.observe_text_chunk",
            location: "runtime.rs:3758 -> bocpd.rs:844",
            workload: "per-capture BOCPD segment observation",
            candidate: true,
            calls_measured: bocpd_calls,
            total_nanos: bocpd_ns,
            calls_per_min: capture_per_min,
            notes: "quality ARL metric deferred",
        },
    ];

    (frames, avg_dirty_wal_bytes)
}

#[test]
fn profile_round7_new_axes_and_emit_gate_verdicts() {
    let (mut frames, avg_dirty_wal_bytes) = run_profile();
    let total_realistic_ns: f64 = frames.iter().map(Frame::realistic_self_ns).sum();
    assert!(
        total_realistic_ns > 0.0,
        "no self-time recorded; harness is dead"
    );

    frames.sort_by(|a, b| {
        b.realistic_self_ns()
            .partial_cmp(&a.realistic_self_ns())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    println!("\n=== ROUND-7 NEW-AXIS PROFILE — scored target list ===");
    println!(
        "model: {CAPTURE_DELTAS_PER_SEC} captures/s, {REDACT_READS_PER_SEC} redacted reads/s, \
         {DETECTION_BURST_PUBLISHES_PER_SEC} burst detections/s, \
         {STARTUP_RECOVERIES_PER_MIN} startup recovery/min over {WINDOW_SECS}s | \
         gate = {:.1}% self-time | avg dirty WAL bytes = {avg_dirty_wal_bytes}",
        GATE_SHARE * 100.0
    );
    println!(
        "{:<30} {:<30} {:>10} {:>10} {:>9} {:>9} {:<10} {}",
        "frame", "location", "mean_ns", "calls/min", "share%", "rank_ns", "candidate", "gate"
    );
    for frame in &frames {
        let share = frame.realistic_self_ns() / total_realistic_ns;
        let gate = if share >= GATE_SHARE { "PASS" } else { "below" };
        println!(
            "{:<30} {:<30} {:>10.1} {:>10} {:>8.3}% {:>9.0} {:<10} {}",
            frame.name,
            frame.location,
            frame.mean_nanos(),
            frame.calls_per_min,
            share * 100.0,
            frame.realistic_self_ns(),
            frame.candidate,
            gate
        );
    }

    let mut json = String::from(
        "ROUND7_PROFILE_JSON {\"schema\":\"round7.new_axis.profile.v1\",\"gate_share\":",
    );
    json.push_str(&format!(
        "{GATE_SHARE},\"avg_dirty_wal_bytes\":{avg_dirty_wal_bytes},\"frames\":["
    ));
    for (i, frame) in frames.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        let share = frame.realistic_self_ns() / total_realistic_ns;
        json.push_str(&format!(
            "{{\"frame\":\"{}\",\"location\":\"{}\",\"workload\":\"{}\",\
             \"candidate\":{},\"mean_ns\":{:.2},\"calls_per_min\":{},\
             \"realistic_self_ns\":{:.0},\"share\":{:.6},\"gate_pass\":{},\
             \"notes\":\"{}\"}}",
            frame.name,
            frame.location,
            frame.workload,
            frame.candidate,
            frame.mean_nanos(),
            frame.calls_per_min,
            frame.realistic_self_ns(),
            share,
            share >= GATE_SHARE,
            frame.notes
        ));
    }
    json.push_str("]}");
    println!("{json}");

    for frame in &frames {
        assert!(
            frame.mean_nanos() > 0.0,
            "frame {} measured zero mean ns",
            frame.name
        );
    }
    let share_sum: f64 = frames
        .iter()
        .map(|frame| frame.realistic_self_ns() / total_realistic_ns)
        .sum();
    assert!(
        (share_sum - 1.0).abs() < 1e-6,
        "shares must sum to 1, got {share_sum}"
    );
    assert!(
        frames.iter().any(|frame| !frame.candidate),
        "denominator context frames are missing"
    );
}
