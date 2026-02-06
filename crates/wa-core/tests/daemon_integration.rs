//! Daemon integration tests using synthetic deltas (no WezTerm required).
//!
//! Tests the full observation pipeline: ingest → persist segments → pattern engine → persist events.
//!
//! These tests:
//! - Construct synthetic pane and delta models
//! - Feed synthetic segments through the persistence pipeline
//! - Assert end-to-end outcomes without requiring WezTerm
//!
//! # Determinism
//!
//! All tests use:
//! - Explicit timestamps (no system time dependencies)
//! - Temp databases with fixed ordering
//! - No timing-based assertions

use std::time::{SystemTime, UNIX_EPOCH};

use tempfile::TempDir;
use wa_core::ingest::{CapturedSegment, CapturedSegmentKind, persist_captured_segment};
use wa_core::patterns::{DetectionContext, PatternEngine};
use wa_core::storage::{PaneRecord, StorageHandle, StoredEvent};

/// Create a temp database path.
fn temp_db() -> (TempDir, String) {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("test.db").to_string_lossy().to_string();
    (dir, path)
}

/// Create a test pane record.
fn test_pane(pane_id: u64) -> PaneRecord {
    let now = now_ms();
    PaneRecord {
        pane_id,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: Some(1),
        tab_id: Some(1),
        title: Some("test".to_string()),
        cwd: Some("/tmp".to_string()),
        tty_name: None,
        first_seen_at: now,
        last_seen_at: now,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

/// Get current timestamp in milliseconds.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

fn offset_ms(offset: u64) -> i64 {
    i64::try_from(offset).unwrap_or(i64::MAX)
}

/// Create a synthetic delta segment.
fn synthetic_delta(pane_id: u64, seq: u64, content: &str, captured_at: i64) -> CapturedSegment {
    CapturedSegment {
        pane_id,
        seq,
        content: content.to_string(),
        kind: CapturedSegmentKind::Delta,
        captured_at,
    }
}

/// Create a synthetic gap segment.
fn synthetic_gap(
    pane_id: u64,
    seq: u64,
    content: &str,
    reason: &str,
    captured_at: i64,
) -> CapturedSegment {
    CapturedSegment {
        pane_id,
        seq,
        content: content.to_string(),
        kind: CapturedSegmentKind::Gap {
            reason: reason.to_string(),
        },
        captured_at,
    }
}

/// Convert a Detection to a StoredEvent for persistence.
fn detection_to_stored_event(
    pane_id: u64,
    detection: &wa_core::patterns::Detection,
    segment_id: Option<i64>,
    detected_at: i64,
) -> StoredEvent {
    StoredEvent {
        id: 0,
        pane_id,
        rule_id: detection.rule_id.clone(),
        agent_type: format!("{:?}", detection.agent_type),
        event_type: detection.event_type.clone(),
        severity: format!("{:?}", detection.severity),
        confidence: detection.confidence,
        extracted: Some(detection.extracted.clone()),
        matched_text: Some(detection.matched_text.clone()),
        segment_id,
        detected_at,
        dedupe_key: None,
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    }
}

// =============================================================================
// Integration Tests
// =============================================================================

/// Test: Segments are persisted correctly.
///
/// Verifies the basic ingest → persist flow without pattern detection.
#[tokio::test]
async fn segments_are_persisted() {
    let (_dir, db_path) = temp_db();
    let storage = StorageHandle::new(&db_path).await.expect("create storage");

    // Create pane first (foreign key requirement)
    storage
        .upsert_pane(test_pane(1))
        .await
        .expect("upsert pane");

    let ts = now_ms();

    // Create and persist a delta segment
    let segment = synthetic_delta(1, 0, "hello world\n", ts);
    let result = persist_captured_segment(&storage, &segment)
        .await
        .expect("persist segment");

    // Verify segment was persisted
    assert_eq!(result.segment.pane_id, 1);
    assert_eq!(result.segment.seq, 0);
    assert_eq!(result.segment.content, "hello world\n");
    assert!(result.gap.is_none(), "Delta should not create a gap");

    // Verify through query
    let segments = storage.get_segments(1, 10).await.expect("get segments");
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].content, "hello world\n");

    storage.shutdown().await.expect("shutdown");
}

/// Test: Multiple segments maintain seq monotonicity.
///
/// Verifies that sequence numbers increment correctly across multiple persists.
#[tokio::test]
async fn seq_monotonicity_across_persists() {
    let (_dir, db_path) = temp_db();
    let storage = StorageHandle::new(&db_path).await.expect("create storage");

    storage
        .upsert_pane(test_pane(1))
        .await
        .expect("upsert pane");

    let ts = now_ms();

    // Persist multiple segments
    for i in 0..5u64 {
        let segment = synthetic_delta(1, i, &format!("segment {i}\n"), ts + offset_ms(i));
        let result = persist_captured_segment(&storage, &segment)
            .await
            .expect("persist segment");

        assert_eq!(result.segment.seq, i, "seq should match");
    }

    // Verify all segments (returned in descending seq order)
    let segments = storage.get_segments(1, 10).await.expect("get segments");
    assert_eq!(segments.len(), 5);

    // Check that we have seqs 4,3,2,1,0 (descending order)
    let seqs: Vec<u64> = segments.iter().map(|s| s.seq).collect();
    assert_eq!(seqs, vec![4, 3, 2, 1, 0]);

    storage.shutdown().await.expect("shutdown");
}

/// Test: Gaps are recorded when segment kind is Gap.
///
/// Verifies that Gap segments create gap records in storage.
#[tokio::test]
async fn gaps_are_recorded_for_gap_segments() {
    let (_dir, db_path) = temp_db();
    let storage = StorageHandle::new(&db_path).await.expect("create storage");

    storage
        .upsert_pane(test_pane(1))
        .await
        .expect("upsert pane");

    let ts = now_ms();

    // First, persist a normal delta
    let delta = synthetic_delta(1, 0, "line1\n", ts);
    let r1 = persist_captured_segment(&storage, &delta)
        .await
        .expect("persist delta");
    assert!(r1.gap.is_none());

    // Now persist a gap segment (simulating overlap failure)
    let gap_segment = synthetic_gap(1, 1, "new content\n", "overlap_not_found", ts + 100);
    let r2 = persist_captured_segment(&storage, &gap_segment)
        .await
        .expect("persist gap");

    // Gap should be recorded
    assert!(r2.gap.is_some(), "Gap segment should create a gap record");
    let gap = r2.gap.unwrap();
    assert_eq!(gap.reason, "overlap_not_found");

    storage.shutdown().await.expect("shutdown");
}

/// Test: Pattern detection triggers events.
///
/// Verifies the full pipeline: content → pattern engine → event persistence.
#[tokio::test]
async fn pattern_detection_triggers_events() {
    let (_dir, db_path) = temp_db();
    let storage = StorageHandle::new(&db_path).await.expect("create storage");

    storage
        .upsert_pane(test_pane(1))
        .await
        .expect("upsert pane");

    let ts = now_ms();

    // Content that should trigger Codex usage warning pattern
    // Pattern: "less than 25% of your Xh limit remaining"
    let content = "Warning: You have less than 25% of your 20h limit remaining. 15% of your 20h limit remaining.\n";

    // Persist the segment
    let segment = synthetic_delta(1, 0, content, ts);
    let result = persist_captured_segment(&storage, &segment)
        .await
        .expect("persist segment");

    // Run pattern detection
    let engine = PatternEngine::new();
    let mut ctx = DetectionContext::new();
    ctx.pane_id = Some(1);

    let detections = engine.detect_with_context(content, &mut ctx);

    // Should have detection(s)
    assert!(!detections.is_empty(), "Should detect Codex usage warning");

    // Persist detections as events
    for detection in &detections {
        let event = detection_to_stored_event(1, detection, Some(result.segment.id), ts);
        storage.record_event(event).await.expect("record event");
    }

    // Verify events were recorded
    let events = storage.get_unhandled_events(10).await.expect("get events");
    assert!(!events.is_empty(), "Events should be recorded");

    // Verify event content
    let event = &events[0];
    assert_eq!(event.pane_id, 1);
    assert!(event.rule_id.contains("codex.usage"));

    storage.shutdown().await.expect("shutdown");
}

/// Test: No-match content exercises quick-reject path.
///
/// Verifies that content not matching any pattern produces no events.
#[tokio::test]
async fn no_match_content_produces_no_events() {
    let (_dir, db_path) = temp_db();
    let storage = StorageHandle::new(&db_path).await.expect("create storage");

    storage
        .upsert_pane(test_pane(1))
        .await
        .expect("upsert pane");

    let ts = now_ms();

    // Plain content that shouldn't match any patterns
    let content = "$ ls -la\ntotal 42\ndrwxr-xr-x 5 user user 4096 Jan 1 12:00 .\n";

    // Persist the segment
    let segment = synthetic_delta(1, 0, content, ts);
    persist_captured_segment(&storage, &segment)
        .await
        .expect("persist segment");

    // Run pattern detection
    let engine = PatternEngine::new();
    let detections = engine.detect(content);

    // Should have no detections
    assert!(
        detections.is_empty(),
        "Plain ls output should not trigger any patterns"
    );

    // Verify no events were recorded (since we didn't persist any)
    let events = storage.get_unhandled_events(10).await.expect("get events");
    assert!(events.is_empty(), "No events should exist");

    storage.shutdown().await.expect("shutdown");
}

/// Test: Explicit GAP required fixture.
///
/// Verifies that a deliberate discontinuity creates a GAP record.
#[tokio::test]
async fn explicit_gap_discontinuity_is_recorded() {
    let (_dir, db_path) = temp_db();
    let storage = StorageHandle::new(&db_path).await.expect("create storage");

    storage
        .upsert_pane(test_pane(1))
        .await
        .expect("upsert pane");

    let ts = now_ms();

    // Simulate a sequence where we miss segment 1 (deliberate discontinuity)
    // Cursor thinks seq should be 0, but storage will assign based on what's there

    // First segment at seq 0
    let seg0 = synthetic_delta(1, 0, "first\n", ts);
    let r0 = persist_captured_segment(&storage, &seg0)
        .await
        .expect("persist seg0");
    assert_eq!(r0.segment.seq, 0);
    assert!(r0.gap.is_none());

    // Now persist a GAP segment at seq 1 (simulating overlap failure)
    let seg1 = synthetic_gap(1, 1, "after gap\n", "test_discontinuity", ts + 100);
    let r1 = persist_captured_segment(&storage, &seg1)
        .await
        .expect("persist seg1");

    assert_eq!(r1.segment.seq, 1, "seq should continue");
    assert!(r1.gap.is_some(), "Gap should be recorded");
    assert_eq!(r1.gap.unwrap().reason, "test_discontinuity");

    storage.shutdown().await.expect("shutdown");
}

/// Test: OSC 133 prompt markers in content.
///
/// Verifies that content with shell integration markers is handled correctly.
#[tokio::test]
async fn osc_133_prompt_markers_handled() {
    let (_dir, db_path) = temp_db();
    let storage = StorageHandle::new(&db_path).await.expect("create storage");

    storage
        .upsert_pane(test_pane(1))
        .await
        .expect("upsert pane");

    let ts = now_ms();

    // Content with OSC 133 prompt markers (shell integration)
    // OSC 133 ; A ST = prompt start
    // OSC 133 ; B ST = prompt end
    // OSC 133 ; C ST = command start
    // OSC 133 ; D ; <exit> ST = command end
    let content = "\x1b]133;A\x07user@host:~$ \x1b]133;B\x07ls\x1b]133;C\x07\nfile1.txt\nfile2.txt\n\x1b]133;D;0\x07";

    let segment = synthetic_delta(1, 0, content, ts);
    let result = persist_captured_segment(&storage, &segment)
        .await
        .expect("persist segment");

    assert_eq!(result.segment.seq, 0);
    assert!(
        result.segment.content.contains("\x1b]133;A"),
        "OSC markers should be preserved"
    );

    // Verify segment is retrievable
    let segments = storage.get_segments(1, 10).await.expect("get segments");
    assert_eq!(segments.len(), 1);
    assert!(segments[0].content.contains("\x1b]133;A"));

    storage.shutdown().await.expect("shutdown");
}

/// Test: Multiple panes are isolated.
///
/// Verifies that segments from different panes don't interfere.
#[tokio::test]
async fn multiple_panes_isolation() {
    let (_dir, db_path) = temp_db();
    let storage = StorageHandle::new(&db_path).await.expect("create storage");

    // Create two panes
    storage
        .upsert_pane(test_pane(1))
        .await
        .expect("upsert pane 1");
    storage
        .upsert_pane(test_pane(2))
        .await
        .expect("upsert pane 2");

    let ts = now_ms();

    // Persist segments to both panes
    let seg1_0 = synthetic_delta(1, 0, "pane1 seg0\n", ts);
    let seg1_1 = synthetic_delta(1, 1, "pane1 seg1\n", ts + 10);
    let seg2_0 = synthetic_delta(2, 0, "pane2 seg0\n", ts + 20);

    persist_captured_segment(&storage, &seg1_0)
        .await
        .expect("persist");
    persist_captured_segment(&storage, &seg1_1)
        .await
        .expect("persist");
    persist_captured_segment(&storage, &seg2_0)
        .await
        .expect("persist");

    // Verify pane 1 segments
    let segments1 = storage.get_segments(1, 10).await.expect("get segments");
    assert_eq!(segments1.len(), 2);
    assert!(segments1[0].content.contains("pane1"));

    // Verify pane 2 segments
    let segments2 = storage.get_segments(2, 10).await.expect("get segments");
    assert_eq!(segments2.len(), 1);
    assert!(segments2[0].content.contains("pane2"));

    storage.shutdown().await.expect("shutdown");
}

/// Test: Claude Code compaction pattern triggers detection.
///
/// Verifies that Claude Code specific patterns work.
#[tokio::test]
async fn claude_code_compaction_detection() {
    let (_dir, db_path) = temp_db();
    let storage = StorageHandle::new(&db_path).await.expect("create storage");

    storage
        .upsert_pane(test_pane(1))
        .await
        .expect("upsert pane");

    let ts = now_ms();

    // Content that should trigger Claude Code compaction pattern
    let content = "Auto-compact: Conversation compacted 150,000 tokens to 50,000 tokens to fit context window.\n";

    let segment = synthetic_delta(1, 0, content, ts);
    let result = persist_captured_segment(&storage, &segment)
        .await
        .expect("persist segment");

    // Run pattern detection
    let engine = PatternEngine::new();
    let mut ctx = DetectionContext::new();
    ctx.pane_id = Some(1);

    let detections = engine.detect_with_context(content, &mut ctx);

    // Should detect compaction
    assert!(
        !detections.is_empty(),
        "Should detect Claude Code compaction"
    );
    assert!(detections.iter().any(|d| d.rule_id.contains("compaction")));

    // Persist events
    for detection in &detections {
        let event = detection_to_stored_event(1, detection, Some(result.segment.id), ts);
        storage.record_event(event).await.expect("record event");
    }

    let events = storage.get_unhandled_events(10).await.expect("get events");
    assert!(!events.is_empty());
    assert!(events.iter().any(|e| e.rule_id.contains("compaction")));

    storage.shutdown().await.expect("shutdown");
}

/// Test: Alt-screen content creates gap.
///
/// Verifies that alt-screen transitions are handled as gaps.
#[tokio::test]
async fn alt_screen_content_as_gap() {
    let (_dir, db_path) = temp_db();
    let storage = StorageHandle::new(&db_path).await.expect("create storage");

    storage
        .upsert_pane(test_pane(1))
        .await
        .expect("upsert pane");

    let ts = now_ms();

    // Normal content first
    let seg0 = synthetic_delta(1, 0, "normal output\n", ts);
    persist_captured_segment(&storage, &seg0)
        .await
        .expect("persist");

    // Alt-screen entered (would normally be detected by cursor, but we simulate as gap)
    let seg1 = synthetic_gap(
        1,
        1,
        "\x1b[?1049h vim content",
        "alt_screen_entered",
        ts + 100,
    );
    let r1 = persist_captured_segment(&storage, &seg1)
        .await
        .expect("persist");

    assert!(r1.gap.is_some());
    assert_eq!(r1.gap.unwrap().reason, "alt_screen_entered");

    storage.shutdown().await.expect("shutdown");
}

/// Test: Full pipeline with mixed content.
///
/// End-to-end test simulating realistic daemon operation.
#[tokio::test]
async fn full_pipeline_mixed_content() {
    let (_dir, db_path) = temp_db();
    let storage = StorageHandle::new(&db_path).await.expect("create storage");

    storage
        .upsert_pane(test_pane(1))
        .await
        .expect("upsert pane");

    let engine = PatternEngine::new();
    let mut ctx = DetectionContext::new();
    ctx.pane_id = Some(1);

    let ts = now_ms();
    let mut total_events = 0u64;

    // Simulate a series of captures
    let captures = vec![
        // Normal shell output (no match expected)
        (0, "$ cd /project\n$ ls\nREADME.md  src/  tests/\n", false),
        // Codex usage warning (match expected)
        (
            1,
            "Warning: less than 25% of your 20h limit remaining. 15% of your 20h limit remaining.\n",
            true,
        ),
        // More shell output (no match)
        (
            2,
            "$ cargo build\n   Compiling wa-core v0.1.0\n    Finished dev\n",
            false,
        ),
        // Claude Code compaction (match expected)
        (
            3,
            "Auto-compact: context compacted 100,000 tokens to 30,000 tokens.\n",
            true,
        ),
    ];

    for (seq, content, should_match) in captures {
        let seq_offset = offset_ms(seq);
        let segment = synthetic_delta(1, seq, content, ts + seq_offset.saturating_mul(100));
        let result = persist_captured_segment(&storage, &segment)
            .await
            .expect("persist segment");

        let detections = engine.detect_with_context(content, &mut ctx);

        if should_match {
            assert!(
                !detections.is_empty(),
                "Expected detection for seq {seq}: {content}"
            );
        }

        for detection in &detections {
            let event =
                detection_to_stored_event(1, detection, Some(result.segment.id), ts + seq_offset);
            storage.record_event(event).await.expect("record event");
            total_events += 1;
        }
    }

    // Verify final state
    let segments = storage.get_segments(1, 10).await.expect("get segments");
    assert_eq!(segments.len(), 4, "All 4 segments should be persisted");

    let events = storage.get_unhandled_events(20).await.expect("get events");
    assert!(
        events.len() >= 2,
        "Should have at least 2 events (usage + compaction)"
    );
    assert_eq!(events.len() as u64, total_events);

    storage.shutdown().await.expect("shutdown");
}

/// Test: Sequence discontinuity creates additional gap.
///
/// Verifies that when cursor seq doesn't match storage seq, a discontinuity gap is recorded.
#[tokio::test]
async fn sequence_discontinuity_gap() {
    let (_dir, db_path) = temp_db();
    let storage = StorageHandle::new(&db_path).await.expect("create storage");

    storage
        .upsert_pane(test_pane(1))
        .await
        .expect("upsert pane");

    let ts = now_ms();

    // First segment at seq 0
    let seg0 = synthetic_delta(1, 0, "first\n", ts);
    persist_captured_segment(&storage, &seg0)
        .await
        .expect("persist");

    // Cursor thinks this is seq 5, but storage has seq 1
    // This simulates a scenario where the cursor got out of sync
    let seg_wrong_seq = synthetic_delta(1, 5, "out of sync\n", ts + 100);
    let result = persist_captured_segment(&storage, &seg_wrong_seq)
        .await
        .expect("persist");

    // Storage assigns seq 1 (next available)
    assert_eq!(result.segment.seq, 1);

    // Gap should be recorded for the discontinuity
    assert!(
        result.gap.is_some(),
        "Sequence discontinuity should create gap"
    );
    assert!(
        result.gap.unwrap().reason.contains("seq_discontinuity"),
        "Gap reason should indicate discontinuity"
    );

    storage.shutdown().await.expect("shutdown");
}

/// Test: Large content batch.
///
/// Verifies the pipeline handles larger volumes correctly.
#[tokio::test]
async fn large_content_batch() {
    let (_dir, db_path) = temp_db();
    let storage = StorageHandle::new(&db_path).await.expect("create storage");

    storage
        .upsert_pane(test_pane(1))
        .await
        .expect("upsert pane");

    let ts = now_ms();

    // Persist 100 segments
    for i in 0..100u64 {
        let content = format!("segment {i} with some content\n");
        let segment = synthetic_delta(1, i, &content, ts + offset_ms(i));
        let result = persist_captured_segment(&storage, &segment)
            .await
            .expect("persist segment");

        assert_eq!(result.segment.seq, i);
    }

    // Verify all segments (returned in descending seq order)
    let segments = storage.get_segments(1, 150).await.expect("get segments");
    assert_eq!(segments.len(), 100);

    // Verify ordering is descending: 99, 98, 97, ..., 0
    for (i, seg) in segments.iter().enumerate() {
        assert_eq!(seg.seq, (99 - i) as u64, "seq should be descending");
    }

    storage.shutdown().await.expect("shutdown");
}

/// Test: Events reference correct segments.
///
/// Verifies that events have correct segment_id foreign keys.
#[tokio::test]
async fn events_reference_segments() {
    let (_dir, db_path) = temp_db();
    let storage = StorageHandle::new(&db_path).await.expect("create storage");

    storage
        .upsert_pane(test_pane(1))
        .await
        .expect("upsert pane");

    let ts = now_ms();
    let engine = PatternEngine::new();

    // Content that triggers detection
    let content = "less than 10% of your 20h limit remaining. 8% of your 20h limit remaining.\n";

    let segment = synthetic_delta(1, 0, content, ts);
    let result = persist_captured_segment(&storage, &segment)
        .await
        .expect("persist");

    let segment_id = result.segment.id;

    let detections = engine.detect(content);
    assert!(!detections.is_empty());

    for detection in &detections {
        let event = detection_to_stored_event(1, detection, Some(segment_id), ts);
        storage.record_event(event).await.expect("record event");
    }

    let events = storage.get_unhandled_events(10).await.expect("get events");
    assert!(!events.is_empty());

    // Verify segment_id reference
    for event in &events {
        assert_eq!(
            event.segment_id,
            Some(segment_id),
            "Event should reference the source segment"
        );
    }

    storage.shutdown().await.expect("shutdown");
}
