//! E4.F2.T2: Deterministic fixture and replay corpus lifecycle controls.
//!
//! Tests fixture loading, schema versioning, checksum validation,
//! deduplication of duplicate batch IDs, corrupt record recovery,
//! and migration manifest fixture validity.

use std::collections::HashMap;
use std::path::PathBuf;

use frankenterm_core::recorder_migration::MigrationManifest;
use frankenterm_core::recorder_storage::{
    AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, CheckpointConsumerId,
    DurabilityLevel, RecorderBackendKind, RecorderCheckpoint, RecorderOffset, RecorderStorage,
};
use frankenterm_core::recording::{
    RecorderControlMarkerType, RecorderEvent, RecorderEventCausality,
    RecorderEventPayload, RecorderEventSource, RecorderIngressKind, RecorderLifecyclePhase,
    RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
    RECORDER_EVENT_SCHEMA_VERSION_V1, parse_recorder_event_json,
};

// ═══════════════════════════════════════════════════════════════════════
// Fixture format and helpers
// ═══════════════════════════════════════════════════════════════════════

/// Schema version for the fixture envelope format.
const FIXTURE_SCHEMA_VERSION: &str = "ft.fixture.v1";

/// A versioned, checksum-pinned fixture envelope.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RecorderFixture {
    /// Fixture schema version for forward compatibility.
    fixture_schema_version: String,
    /// Human-readable name.
    name: String,
    /// FNV-1a checksum of the serialized events array.
    checksum: u64,
    /// Number of events expected.
    expected_event_count: usize,
    /// The events in this fixture.
    events: Vec<RecorderEvent>,
    /// Optional checkpoints associated with this fixture.
    #[serde(default)]
    checkpoints: Vec<RecorderCheckpoint>,
}

/// Compute FNV-1a checksum over canonical JSON of the events array.
fn compute_events_checksum(events: &[RecorderEvent]) -> u64 {
    let json = serde_json::to_string(events).expect("events serialize");
    fnv1a_hash(json.as_bytes())
}

/// FNV-1a hash (64-bit).
fn fnv1a_hash(data: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET_BASIS;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Build a fixture from events, computing checksum automatically.
fn build_fixture(
    name: &str,
    events: Vec<RecorderEvent>,
    checkpoints: Vec<RecorderCheckpoint>,
) -> RecorderFixture {
    let checksum = compute_events_checksum(&events);
    RecorderFixture {
        fixture_schema_version: FIXTURE_SCHEMA_VERSION.to_string(),
        name: name.to_string(),
        checksum,
        expected_event_count: events.len(),
        events,
        checkpoints,
    }
}

/// Load a fixture from JSON bytes and validate checksum + schema version.
fn load_fixture(json: &[u8]) -> Result<RecorderFixture, String> {
    let fixture: RecorderFixture =
        serde_json::from_slice(json).map_err(|e| format!("parse error: {e}"))?;

    // Validate schema version
    if fixture.fixture_schema_version != FIXTURE_SCHEMA_VERSION {
        return Err(format!(
            "schema version mismatch: expected {FIXTURE_SCHEMA_VERSION}, got {}",
            fixture.fixture_schema_version
        ));
    }

    // Validate event count
    if fixture.events.len() != fixture.expected_event_count {
        return Err(format!(
            "event count mismatch: expected {}, got {}",
            fixture.expected_event_count,
            fixture.events.len()
        ));
    }

    // Validate checksum
    let actual = compute_events_checksum(&fixture.events);
    if actual != fixture.checksum {
        return Err(format!(
            "checksum mismatch: expected {}, got {actual}",
            fixture.checksum
        ));
    }

    Ok(fixture)
}

/// Path to the fixtures directory (relative to crate root, resolved at test time).
fn fixtures_dir() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest).join("tests").join("fixtures").join("frankensqlite")
}

/// Load a named fixture from disk.
fn load_fixture_from_disk(name: &str) -> Result<RecorderFixture, String> {
    let path = fixtures_dir().join(name);
    let data = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    load_fixture(&data)
}

// ═══════════════════════════════════════════════════════════════════════
// Event generators
// ═══════════════════════════════════════════════════════════════════════

fn make_ingress_event(pane_id: u64, seq: u64, text: &str) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: format!("fix-{pane_id}-{seq}"),
        pane_id,
        session_id: Some("fixture-session".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::RobotMode,
        occurred_at_ms: 1_700_000_000_000 + seq,
        recorded_at_ms: 1_700_000_000_001 + seq,
        sequence: seq,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload: RecorderEventPayload::IngressText {
            text: text.to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::SendText,
        },
    }
}

fn make_egress_event(pane_id: u64, seq: u64, text: &str) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: format!("egr-{pane_id}-{seq}"),
        pane_id,
        session_id: Some("fixture-session".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        occurred_at_ms: 1_700_000_000_000 + seq,
        recorded_at_ms: 1_700_000_000_002 + seq,
        sequence: seq,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload: RecorderEventPayload::EgressOutput {
            text: text.to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            segment_kind: RecorderSegmentKind::Delta,
            is_gap: false,
        },
    }
}

fn make_lifecycle_event(pane_id: u64, seq: u64, phase: RecorderLifecyclePhase) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: format!("lc-{pane_id}-{seq}"),
        pane_id,
        session_id: Some("fixture-session".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        occurred_at_ms: 1_700_000_000_000 + seq,
        recorded_at_ms: 1_700_000_000_001 + seq,
        sequence: seq,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload: RecorderEventPayload::LifecycleMarker {
            lifecycle_phase: phase,
            reason: None,
            details: serde_json::Value::Null,
        },
    }
}

fn make_control_event(pane_id: u64, seq: u64) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: format!("ctl-{pane_id}-{seq}"),
        pane_id,
        session_id: Some("fixture-session".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::OperatorAction,
        occurred_at_ms: 1_700_000_000_000 + seq,
        recorded_at_ms: 1_700_000_000_001 + seq,
        sequence: seq,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload: RecorderEventPayload::ControlMarker {
            control_marker_type: RecorderControlMarkerType::PromptBoundary,
            details: serde_json::json!({"prompt": "$ "}),
        },
    }
}

/// Generate the "normal 100 events" fixture: 100 events across 5 panes.
fn generate_normal_100() -> Vec<RecorderEvent> {
    (0..100u64)
        .map(|i| {
            let pane = (i % 5) + 1;
            make_ingress_event(pane, i, &format!("command-{i}"))
        })
        .collect()
}

/// Generate a fixture with duplicate batch IDs (events sharing sequence
/// numbers within the same pane — simulates idempotent replay).
fn generate_duplicate_batch_ids() -> Vec<RecorderEvent> {
    let mut events = Vec::new();
    // First batch: 10 events
    for i in 0..10u64 {
        events.push(make_ingress_event(1, i, &format!("batch-a-{i}")));
    }
    // Duplicate batch: same pane + sequence as first 5
    for i in 0..5u64 {
        events.push(make_ingress_event(1, i, &format!("batch-b-{i}")));
    }
    // Third batch: unique
    for i in 10..20u64 {
        events.push(make_ingress_event(1, i, &format!("batch-c-{i}")));
    }
    events
}

/// Generate a fixture with a corrupt (truncated) last record.
/// The last event has an empty event_id which violates expectations.
fn generate_corrupt_torn_record() -> Vec<RecorderEvent> {
    let mut events: Vec<_> = (0..10u64)
        .map(|i| make_ingress_event(1, i, &format!("ok-{i}")))
        .collect();
    // "Torn" record: missing event_id (empty string) simulates truncation
    let mut torn = make_ingress_event(1, 10, "torn-record");
    torn.event_id = String::new(); // Simulates corruption
    events.push(torn);
    events
}

/// Generate a multi-type fixture with all 4 payload variants.
fn generate_mixed_payload_types() -> Vec<RecorderEvent> {
    vec![
        make_ingress_event(1, 0, "hello"),
        make_egress_event(1, 1, "world"),
        make_lifecycle_event(1, 2, RecorderLifecyclePhase::CaptureStarted),
        make_control_event(1, 3),
        make_ingress_event(2, 4, "pane2-cmd"),
        make_egress_event(2, 5, "pane2-output"),
        make_lifecycle_event(2, 6, RecorderLifecyclePhase::PaneOpened),
        make_control_event(2, 7),
    ]
}

/// Generate a large 10K event fixture for performance testing.
fn generate_large_10k() -> Vec<RecorderEvent> {
    (0..10_000u64)
        .map(|i| {
            let pane = (i % 10) + 1;
            make_ingress_event(pane, i, &format!("perf-event-{i}"))
        })
        .collect()
}

/// Create a test AppendLog config pointing at a temp dir.
fn test_append_config(path: &std::path::Path) -> AppendLogStorageConfig {
    AppendLogStorageConfig {
        data_path: path.join("events.log"),
        state_path: path.join("state.json"),
        queue_capacity: 64,
        max_batch_events: 256,
        max_batch_bytes: 256 * 1024,
        max_idempotency_entries: 4096,
    }
}

/// Helper: populate an AppendLog storage from a fixture's events.
async fn populate_from_fixture(
    storage: &AppendLogRecorderStorage,
    events: &[RecorderEvent],
) -> Vec<RecorderOffset> {
    let batch_size = 50;
    let mut offsets = Vec::new();
    for (batch_idx, chunk) in events.chunks(batch_size).enumerate() {
        let resp = storage
            .append_batch(AppendRequest {
                batch_id: format!("fixture-batch-{batch_idx}"),
                events: chunk.to_vec(),
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();
        let first_ord = resp.first_offset.ordinal;
        for i in 0..chunk.len() {
            offsets.push(RecorderOffset {
                segment_id: 0,
                byte_offset: (first_ord + i as u64) * 100,
                ordinal: first_ord + i as u64,
            });
        }
    }
    offsets
}

// ═══════════════════════════════════════════════════════════════════════
// Fixture generation and persistence (writes JSON files to disk)
// ═══════════════════════════════════════════════════════════════════════

/// Write all standard fixtures to disk. Called once by setup test.
fn write_standard_fixtures() {
    let dir = fixtures_dir();
    std::fs::create_dir_all(&dir).unwrap();

    let fixtures = vec![
        build_fixture("fixture_normal_100_events", generate_normal_100(), vec![]),
        build_fixture(
            "fixture_duplicate_batch_ids",
            generate_duplicate_batch_ids(),
            vec![],
        ),
        build_fixture(
            "fixture_corrupt_torn_record",
            generate_corrupt_torn_record(),
            vec![],
        ),
        build_fixture(
            "fixture_mixed_payload_types",
            generate_mixed_payload_types(),
            vec![],
        ),
        build_fixture(
            "fixture_migration_manifest",
            generate_normal_100(),
            vec![RecorderCheckpoint {
                consumer: CheckpointConsumerId("migration-consumer".to_string()),
                upto_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 9900,
                    ordinal: 99,
                },
                schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
                committed_at_ms: 1_700_000_010_000,
            }],
        ),
        build_fixture("fixture_large_10k_events", generate_large_10k(), vec![]),
    ];

    for fixture in &fixtures {
        let path = dir.join(format!("{}.json", fixture.name));
        let json = serde_json::to_string_pretty(fixture).unwrap();
        std::fs::write(&path, json).unwrap();
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

/// Setup: generate and write fixture files (idempotent).
#[test]
fn test_fixture_generation_writes_files() {
    write_standard_fixtures();
    let dir = fixtures_dir();
    assert!(dir.join("fixture_normal_100_events.json").exists());
    assert!(dir.join("fixture_duplicate_batch_ids.json").exists());
    assert!(dir.join("fixture_corrupt_torn_record.json").exists());
    assert!(dir.join("fixture_mixed_payload_types.json").exists());
    assert!(dir.join("fixture_migration_manifest.json").exists());
    assert!(dir.join("fixture_large_10k_events.json").exists());
}

// ── Normal fixture loading ────────────────────────────────────────────

#[test]
fn test_fixture_normal_loads_correctly() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_normal_100_events.json").unwrap();
    assert_eq!(fixture.events.len(), 100);
    assert_eq!(fixture.name, "fixture_normal_100_events");
}

#[test]
fn test_fixture_normal_events_span_5_panes() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_normal_100_events.json").unwrap();
    let panes: std::collections::HashSet<u64> =
        fixture.events.iter().map(|e| e.pane_id).collect();
    assert_eq!(panes.len(), 5);
    for p in 1..=5 {
        assert!(panes.contains(&p));
    }
}

#[test]
fn test_fixture_normal_sequence_monotonic_per_pane() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_normal_100_events.json").unwrap();
    let mut per_pane: HashMap<u64, Vec<u64>> = HashMap::new();
    for e in &fixture.events {
        per_pane.entry(e.pane_id).or_default().push(e.sequence);
    }
    for (pane, seqs) in &per_pane {
        for window in seqs.windows(2) {
            assert!(
                window[0] < window[1],
                "pane {pane}: sequence not monotonic: {} >= {}",
                window[0],
                window[1]
            );
        }
    }
}

#[test]
fn test_fixture_normal_all_schema_v1() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_normal_100_events.json").unwrap();
    for e in &fixture.events {
        assert_eq!(e.schema_version, RECORDER_EVENT_SCHEMA_VERSION_V1);
    }
}

// ── Checksum validation ───────────────────────────────────────────────

#[test]
fn test_fixture_checksum_validation() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_normal_100_events.json").unwrap();
    let recomputed = compute_events_checksum(&fixture.events);
    assert_eq!(fixture.checksum, recomputed);
}

#[test]
fn test_fixture_checksum_detects_tamper() {
    write_standard_fixtures();
    let path = fixtures_dir().join("fixture_normal_100_events.json");
    let data = std::fs::read(&path).unwrap();
    let mut fixture: RecorderFixture = serde_json::from_slice(&data).unwrap();
    // Tamper with an event
    fixture.events[50].pane_id = 999;
    let tampered_json = serde_json::to_vec(&fixture).unwrap();
    let result = load_fixture(&tampered_json);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("checksum mismatch"));
}

#[test]
fn test_fixture_checksum_deterministic() {
    let events = generate_normal_100();
    let c1 = compute_events_checksum(&events);
    let c2 = compute_events_checksum(&events);
    assert_eq!(c1, c2);
}

#[test]
fn test_fixture_checksum_sensitive_to_order() {
    let mut events = generate_normal_100();
    let c1 = compute_events_checksum(&events);
    events.swap(0, 99);
    let c2 = compute_events_checksum(&events);
    assert_ne!(c1, c2);
}

// ── Schema version validation ─────────────────────────────────────────

#[test]
fn test_fixture_schema_version_mismatch_detected() {
    let events = generate_normal_100();
    let mut fixture = build_fixture("bad-version", events, vec![]);
    fixture.fixture_schema_version = "ft.fixture.v99".to_string();
    let json = serde_json::to_vec(&fixture).unwrap();
    let result = load_fixture(&json);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("schema version mismatch"));
}

#[test]
fn test_fixture_event_count_mismatch_detected() {
    let events = generate_normal_100();
    let mut fixture = build_fixture("bad-count", events, vec![]);
    fixture.expected_event_count = 50; // Wrong count
    let json = serde_json::to_vec(&fixture).unwrap();
    let result = load_fixture(&json);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("event count mismatch"));
}

// ── Duplicate batch IDs ───────────────────────────────────────────────

#[test]
fn test_fixture_duplicate_batch_ids_loads() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_duplicate_batch_ids.json").unwrap();
    assert_eq!(fixture.events.len(), 25); // 10 + 5 dupes + 10
}

#[test]
fn test_fixture_duplicate_batch_ids_dedup_works() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_duplicate_batch_ids.json").unwrap();
    // Dedup by (pane_id, sequence) — keep first seen
    let mut seen = std::collections::HashSet::new();
    let deduped: Vec<_> = fixture
        .events
        .iter()
        .filter(|e| seen.insert((e.pane_id, e.sequence)))
        .collect();
    // 0..10 unique + 10..20 unique = 20 (the 5 dupes of 0..5 are removed)
    assert_eq!(deduped.len(), 20);
}

#[test]
fn test_fixture_duplicate_identifies_which_are_dupes() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_duplicate_batch_ids.json").unwrap();
    let mut seen = std::collections::HashMap::new();
    let mut dupe_count = 0;
    for e in &fixture.events {
        let key = (e.pane_id, e.sequence);
        if seen.contains_key(&key) {
            dupe_count += 1;
        }
        seen.entry(key).or_insert(e);
    }
    assert_eq!(dupe_count, 5);
}

// ── Corrupt record recovery ──────────────────────────────────────────

#[test]
fn test_fixture_corrupt_loads() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_corrupt_torn_record.json").unwrap();
    assert_eq!(fixture.events.len(), 11);
}

#[test]
fn test_fixture_corrupt_recovery_detects_torn() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_corrupt_torn_record.json").unwrap();
    // The torn record has an empty event_id
    let torn_events: Vec<_> = fixture
        .events
        .iter()
        .filter(|e| e.event_id.is_empty())
        .collect();
    assert_eq!(torn_events.len(), 1, "exactly one torn record");
}

#[test]
fn test_fixture_corrupt_recovery_healthy_prefix() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_corrupt_torn_record.json").unwrap();
    // First 10 events should be healthy
    let healthy: Vec<_> = fixture
        .events
        .iter()
        .filter(|e| !e.event_id.is_empty())
        .collect();
    assert_eq!(healthy.len(), 10);
    for e in &healthy {
        assert!(e.event_id.starts_with("fix-"));
    }
}

// ── Migration manifest fixture ────────────────────────────────────────

#[test]
fn test_fixture_migration_manifest_valid() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_migration_manifest.json").unwrap();
    assert_eq!(fixture.events.len(), 100);
    assert_eq!(fixture.checkpoints.len(), 1);
}

#[test]
fn test_fixture_migration_manifest_checkpoint_fields() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_migration_manifest.json").unwrap();
    let cp = &fixture.checkpoints[0];
    assert_eq!(cp.consumer.0, "migration-consumer");
    assert_eq!(cp.upto_offset.ordinal, 99);
    assert_eq!(cp.schema_version, RECORDER_EVENT_SCHEMA_VERSION_V1);
}

#[test]
fn test_fixture_migration_manifest_builds_valid_manifest() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_migration_manifest.json").unwrap();
    // Build a MigrationManifest from the fixture
    let mut per_pane_counts: HashMap<u64, u64> = HashMap::new();
    for e in &fixture.events {
        *per_pane_counts.entry(e.pane_id).or_default() += 1;
    }
    let manifest = MigrationManifest {
        event_count: fixture.events.len() as u64,
        first_ordinal: 0,
        last_ordinal: 99,
        per_pane_counts: per_pane_counts.clone(),
        export_digest: 0,
        export_count: fixture.events.len() as u64,
        import_digest: 0,
        import_count: 0,
        last_offset: Some(RecorderOffset {
            segment_id: 0,
            byte_offset: 9900,
            ordinal: 99,
        }),
    };
    assert_eq!(manifest.event_count, 100);
    assert_eq!(manifest.per_pane_counts.len(), 5);
    for count in manifest.per_pane_counts.values() {
        assert_eq!(*count, 20);
    }
}

// ── Mixed payload types ───────────────────────────────────────────────

#[test]
fn test_fixture_mixed_payload_types_loads() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_mixed_payload_types.json").unwrap();
    assert_eq!(fixture.events.len(), 8);
}

#[test]
fn test_fixture_mixed_all_four_variants_present() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_mixed_payload_types.json").unwrap();
    let has_ingress = fixture
        .events
        .iter()
        .any(|e| matches!(e.payload, RecorderEventPayload::IngressText { .. }));
    let has_egress = fixture
        .events
        .iter()
        .any(|e| matches!(e.payload, RecorderEventPayload::EgressOutput { .. }));
    let has_lifecycle = fixture
        .events
        .iter()
        .any(|e| matches!(e.payload, RecorderEventPayload::LifecycleMarker { .. }));
    let has_control = fixture
        .events
        .iter()
        .any(|e| matches!(e.payload, RecorderEventPayload::ControlMarker { .. }));
    assert!(has_ingress, "missing IngressText variant");
    assert!(has_egress, "missing EgressOutput variant");
    assert!(has_lifecycle, "missing LifecycleMarker variant");
    assert!(has_control, "missing ControlMarker variant");
}

// ── Large fixture (10K events) ────────────────────────────────────────

#[test]
fn test_fixture_large_10k_loads() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_large_10k_events.json").unwrap();
    assert_eq!(fixture.events.len(), 10_000);
}

#[test]
fn test_fixture_large_10k_spans_10_panes() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_large_10k_events.json").unwrap();
    let panes: std::collections::HashSet<u64> =
        fixture.events.iter().map(|e| e.pane_id).collect();
    assert_eq!(panes.len(), 10);
}

#[test]
fn test_fixture_large_10k_checksum_stable() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_large_10k_events.json").unwrap();
    let recomputed = compute_events_checksum(&fixture.events);
    assert_eq!(fixture.checksum, recomputed);
}

// ── Serde roundtrip ───────────────────────────────────────────────────

#[test]
fn test_fixture_serde_roundtrip() {
    let events = generate_normal_100();
    let fixture = build_fixture("roundtrip-test", events, vec![]);
    let json = serde_json::to_string(&fixture).unwrap();
    let back: RecorderFixture = serde_json::from_str(&json).unwrap();
    assert_eq!(fixture.events.len(), back.events.len());
    assert_eq!(fixture.checksum, back.checksum);
    for (a, b) in fixture.events.iter().zip(back.events.iter()) {
        assert_eq!(a, b);
    }
}

#[test]
fn test_fixture_event_json_roundtrip_via_parse() {
    let event = make_ingress_event(1, 42, "hello world");
    let json = serde_json::to_string(&event).unwrap();
    let parsed = parse_recorder_event_json(&json).unwrap();
    assert_eq!(event, parsed);
}

#[test]
fn test_fixture_egress_json_roundtrip() {
    let event = make_egress_event(2, 7, "output text");
    let json = serde_json::to_string(&event).unwrap();
    let parsed: RecorderEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(event, parsed);
}

#[test]
fn test_fixture_lifecycle_json_roundtrip() {
    let event = make_lifecycle_event(3, 99, RecorderLifecyclePhase::PaneClosed);
    let json = serde_json::to_string(&event).unwrap();
    let parsed: RecorderEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(event, parsed);
}

#[test]
fn test_fixture_control_json_roundtrip() {
    let event = make_control_event(1, 0);
    let json = serde_json::to_string(&event).unwrap();
    let parsed: RecorderEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(event, parsed);
}

// ── End-to-end: load fixture into AppendLog storage ───────────────────

#[tokio::test]
async fn test_fixture_ingest_into_storage() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_normal_100_events.json").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
    let offsets = populate_from_fixture(&storage, &fixture.events).await;
    assert_eq!(offsets.len(), 100);

    let health = storage.health().await;
    assert_eq!(health.backend, RecorderBackendKind::AppendLog);
    assert!(!health.degraded);
}

#[tokio::test]
async fn test_fixture_ingest_preserves_event_count() {
    let fixture = build_fixture("inline-test", generate_normal_100(), vec![]);
    let dir = tempfile::tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
    let offsets = populate_from_fixture(&storage, &fixture.events).await;
    assert_eq!(offsets.len(), fixture.expected_event_count);
}

#[tokio::test]
async fn test_fixture_ingest_large_10k_completes() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_large_10k_events.json").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
    let offsets = populate_from_fixture(&storage, &fixture.events).await;
    assert_eq!(offsets.len(), 10_000);
}

// ── Fixture replay corpus: load → ingest → verify ─────────────────────

#[tokio::test]
async fn test_fixture_replay_corpus_offsets_monotonic() {
    let events = generate_normal_100();
    let fixture = build_fixture("replay-test", events, vec![]);

    let dir = tempfile::tempdir().unwrap();
    let source = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
    let offsets = populate_from_fixture(&source, &fixture.events).await;

    // Offsets should be strictly monotonically increasing
    for window in offsets.windows(2) {
        assert!(
            window[0].ordinal < window[1].ordinal,
            "ordinals not monotonic: {} >= {}",
            window[0].ordinal,
            window[1].ordinal,
        );
    }
}

#[tokio::test]
async fn test_fixture_replay_corpus_idempotent_batch() {
    let events = generate_normal_100();

    let dir = tempfile::tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();

    // First ingest
    let _offsets = populate_from_fixture(&storage, &events).await;

    // Replay same batch_id — idempotent
    let resp = storage
        .append_batch(AppendRequest {
            batch_id: "fixture-batch-0".to_string(),
            events: events[..50].to_vec(),
            required_durability: DurabilityLevel::Appended,
            producer_ts_ms: 1,
        })
        .await
        .unwrap();
    assert_eq!(resp.accepted_count, 50);
}

// ── Edge cases ────────────────────────────────────────────────────────

#[test]
fn test_fixture_empty_events_valid() {
    let fixture = build_fixture("empty", vec![], vec![]);
    let json = serde_json::to_vec(&fixture).unwrap();
    let loaded = load_fixture(&json).unwrap();
    assert_eq!(loaded.events.len(), 0);
    assert_eq!(loaded.checksum, fixture.checksum);
}

#[test]
fn test_fixture_single_event_valid() {
    let events = vec![make_ingress_event(1, 0, "solo")];
    let fixture = build_fixture("single", events, vec![]);
    let json = serde_json::to_vec(&fixture).unwrap();
    let loaded = load_fixture(&json).unwrap();
    assert_eq!(loaded.events.len(), 1);
}

#[test]
fn test_fixture_invalid_json_returns_error() {
    let result = load_fixture(b"not valid json");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("parse error"));
}

#[test]
fn test_fixture_missing_file_returns_error() {
    let result = load_fixture_from_disk("nonexistent_fixture.json");
    assert!(result.is_err());
}

#[test]
fn test_fixture_checkpoints_serde_roundtrip() {
    let cp = RecorderCheckpoint {
        consumer: CheckpointConsumerId("test-consumer".to_string()),
        upto_offset: RecorderOffset {
            segment_id: 0,
            byte_offset: 500,
            ordinal: 5,
        },
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        committed_at_ms: 1_700_000_000_000,
    };
    let fixture = build_fixture("cp-test", generate_normal_100(), vec![cp.clone()]);
    let json = serde_json::to_vec(&fixture).unwrap();
    let loaded = load_fixture(&json).unwrap();
    assert_eq!(loaded.checkpoints.len(), 1);
    assert_eq!(loaded.checkpoints[0].consumer.0, "test-consumer");
}

#[test]
fn test_fixture_event_ids_unique_in_normal() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_normal_100_events.json").unwrap();
    let ids: std::collections::HashSet<&str> =
        fixture.events.iter().map(|e| e.event_id.as_str()).collect();
    assert_eq!(ids.len(), 100, "all event IDs should be unique");
}

#[test]
fn test_fixture_timestamps_monotonic() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_normal_100_events.json").unwrap();
    for window in fixture.events.windows(2) {
        assert!(
            window[0].occurred_at_ms <= window[1].occurred_at_ms,
            "timestamps should be monotonically non-decreasing"
        );
    }
}

#[test]
fn test_fixture_recorded_at_after_occurred_at() {
    write_standard_fixtures();
    let fixture = load_fixture_from_disk("fixture_normal_100_events.json").unwrap();
    for e in &fixture.events {
        assert!(
            e.recorded_at_ms >= e.occurred_at_ms,
            "recorded_at should be >= occurred_at for event {}",
            e.event_id
        );
    }
}

#[test]
fn test_fixture_schema_version_v1_constant() {
    assert_eq!(FIXTURE_SCHEMA_VERSION, "ft.fixture.v1");
}
