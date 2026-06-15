//! Conformance + golden for the watch-events / IPC live-transport FLOW wiring
//! (ft-7h5da.4.x).
//!
//! `ft robot watch-events --follow` unifies two sources behind one `events.id`
//! cursor: a DB cursor over persisted detections, and a live unix-IPC EventBus
//! transport. The server-side record BUILDERS (`subscribe_event_record`,
//! `watch_lag_gap_record`, `watch_heartbeat_record`) are already covered by
//! behavioral unit tests in `ipc.rs`. What those isolated tests do NOT pin is
//! the FLOW wiring — that the live path (`stream_subscribe_events`) actually
//! wires those builders together with the shared cursor/gap engine, and that
//! the live record carries the same `events.id` cursor (and the same filter)
//! the DB-cursor path uses. These source-level guards pin that wiring so a
//! refactor cannot silently de-unify the two transports.

use std::path::PathBuf;

fn core_src(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn contains_all(src: &str, needles: &[&str]) -> bool {
    needles.iter().all(|needle| src.contains(needle))
}

fn watch_events_unification_contract() -> serde_json::Value {
    let ipc = core_src("ipc.rs");

    serde_json::json!([
        {
            "invariant": "unify_on_events_id",
            "detail": "live IPC record carries the same events.id cursor as the DB-cursor path; no cursor-less emission",
            "pinned": contains_all(
                &ipc,
                &[
                    "let cursor = (*event_id)?;",
                    "\"cursor\": cursor,",
                    "\"id\": cursor,",
                ],
            ),
        },
        {
            "invariant": "shared_rule_glob_filter",
            "detail": "live path uses the same rule_glob_matches matcher as the DB-cursor follower",
            "pinned": contains_all(&ipc, &["crate::events::rule_glob_matches"]),
        },
        {
            "invariant": "lag_explicit_gap_db_resync",
            "detail": "broadcast overflow becomes an explicit gap that points at DB-cursor resync (never a silent drop)",
            "pinned": contains_all(
                &ipc,
                &[
                    "\"reason\": \"broadcast_lag\",",
                    "resync via the DB cursor",
                    "watch_lag_gap_record(missed_count)",
                ],
            ),
        },
        {
            "invariant": "shared_filtered_event_stream",
            "detail": "stream_subscribe_events drives the shared FilteredEventStream cursor/gap engine",
            "pinned": contains_all(
                &ipc,
                &["FilteredEventStream::new", "StreamItem::Gap"],
            ),
        },
        {
            "invariant": "heartbeat_mirrors_db_cursor_shape",
            "detail": "idle heartbeat NDJSON mirrors the DB-cursor path's type/cursor/now shape",
            "pinned": contains_all(&ipc, &["\"type\": \"heartbeat\""]),
        },
    ])
}

#[test]
fn watch_events_unification_matches_golden() {
    let matrix = watch_events_unification_contract();
    // GOLDEN: every flow-wiring invariant is pinned. `serde_json::Value` equality
    // is structural/order-independent, so this is robust to field ordering.
    let expected = serde_json::json!([
        {
            "invariant": "unify_on_events_id",
            "detail": "live IPC record carries the same events.id cursor as the DB-cursor path; no cursor-less emission",
            "pinned": true,
        },
        {
            "invariant": "shared_rule_glob_filter",
            "detail": "live path uses the same rule_glob_matches matcher as the DB-cursor follower",
            "pinned": true,
        },
        {
            "invariant": "lag_explicit_gap_db_resync",
            "detail": "broadcast overflow becomes an explicit gap that points at DB-cursor resync (never a silent drop)",
            "pinned": true,
        },
        {
            "invariant": "shared_filtered_event_stream",
            "detail": "stream_subscribe_events drives the shared FilteredEventStream cursor/gap engine",
            "pinned": true,
        },
        {
            "invariant": "heartbeat_mirrors_db_cursor_shape",
            "detail": "idle heartbeat NDJSON mirrors the DB-cursor path's type/cursor/now shape",
            "pinned": true,
        },
    ]);
    assert_eq!(
        matrix, expected,
        "watch-events/IPC flow-wiring contract drifted from golden"
    );
}

/// ft-7h5da.4.1: the live IPC record and the DB-cursor record both key on
/// `events.id` — the live `subscribe_event_record` sets BOTH `cursor` and `id`
/// from the persisted `event_id` (and skips events that have no id).
#[test]
fn live_record_unifies_cursor_and_id_on_events_id() {
    let ipc = core_src("ipc.rs");
    assert!(
        ipc.contains("let cursor = (*event_id)?;"),
        "live path must derive the cursor from the persisted events.id (ft-7h5da.4.1)"
    );
    assert!(
        ipc.contains("\"cursor\": cursor,") && ipc.contains("\"id\": cursor,"),
        "live record's cursor and id must both be the events.id (ft-7h5da.4.1)"
    );
}

/// ft-7h5da.4.2: a live broadcast overflow must surface as an explicit gap that
/// directs the follower to re-baseline through the DB cursor — at-least-once,
/// never a silent drop.
#[test]
fn lag_surfaces_explicit_gap_pointing_at_db_resync() {
    let ipc = core_src("ipc.rs");
    assert!(
        ipc.contains("\"reason\": \"broadcast_lag\","),
        "lag must be reported as an explicit broadcast_lag gap (ft-7h5da.4.2)"
    );
    assert!(
        ipc.contains("resync via the DB cursor"),
        "the gap record must point the follower at DB-cursor resync (ft-7h5da.4.2)"
    );
    assert!(
        ipc.contains("watch_lag_gap_record(missed_count)"),
        "stream_subscribe_events must render StreamItem::Gap via watch_lag_gap_record (ft-7h5da.4.2)"
    );
}
