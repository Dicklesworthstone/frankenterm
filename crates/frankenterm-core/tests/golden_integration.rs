//! Golden artifact tests for wire-protocol and snapshot serialization.
//!
//! Freezes byte-exact JSON for representative instances of types that are
//! load-bearing on the wire or in persisted storage. Any change that shifts
//! field order, renames a variant, alters the enum tag layout, or changes
//! the serde representation will flip a golden and fail the suite.
//!
//! ## Regenerating
//!
//! ```
//! UPDATE_GOLDENS=1 cargo test -p frankenterm-core --test golden_integration
//! ```
//!
//! Then `git diff crates/frankenterm-core/tests/golden/` — review every
//! byte change before committing. A golden change is a schema migration.
//!
//! ## Scope note
//!
//! Per project memory: **varbincode types are positional** and must never
//! use `#[serde(skip_serializing_if = ...)]`. This suite targets the
//! JSON-serialized types in `wire_protocol.rs` and `snapshot_engine.rs`,
//! not the varbincode PDUs in `frankenterm/codec/` (which already have
//! conformance coverage in `frankenterm/codec/tests/conformance_pdu_wire.rs`).

use std::fs;
use std::path::{Path, PathBuf};

use frankenterm_core::patterns::{AgentType, Severity};
use frankenterm_core::snapshot_engine::SnapshotTrigger;
use frankenterm_core::wire_protocol::{
    DetectionNotice, GapNotice, PaneDelta, PaneMeta, PanesMeta, WireEnvelope, WirePayload,
    PROTOCOL_VERSION,
};

// ─── Golden helper ─────────────────────────────────────────────────────

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
}

fn updating_goldens() -> bool {
    std::env::var_os("UPDATE_GOLDENS").is_some()
}

/// Compare `actual` bytes against the golden file at
/// `tests/golden/<relative_path>`. On mismatch, writes a `.actual` sibling
/// for easy diffing and panics with a helpful message.
///
/// `UPDATE_GOLDENS=1` overwrites the golden and skips the compare —
/// the operator MUST `git diff` the result before committing.
fn assert_golden_bytes(relative_path: &str, actual: &[u8]) {
    let path = golden_dir().join(relative_path);

    if updating_goldens() {
        fs::create_dir_all(path.parent().expect("golden path has parent")).unwrap();
        fs::write(&path, actual).unwrap_or_else(|e| {
            panic!("failed to write golden {}: {e}", path.display())
        });
        eprintln!("[GOLDEN UPDATED] {}", path.display());
        return;
    }

    let expected = fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "golden missing: {}\n\
             cause: {e}\n\
             run: UPDATE_GOLDENS=1 cargo test -p frankenterm-core --test golden_integration\n\
             then: git diff {}",
            path.display(),
            path.display()
        )
    });

    if actual != expected.as_slice() {
        // Persist the actual output for a human to diff against the golden.
        let actual_path = path.with_extension("actual.json");
        fs::write(&actual_path, actual).ok();
        panic!(
            "GOLDEN MISMATCH: {}\n\n\
             expected ({} bytes): {}\n\
             actual   ({} bytes): {}\n\n\
             To view diff:    diff {} {}\n\
             To regenerate:   UPDATE_GOLDENS=1 cargo test -p frankenterm-core \
             --test golden_integration\n\
             Regenerating is a schema migration. Bump PROTOCOL_VERSION or add \
             a snapshot migration if this is an intentional wire/persistence change.",
            relative_path,
            expected.len(),
            String::from_utf8_lossy(&expected),
            actual.len(),
            String::from_utf8_lossy(actual),
            path.display(),
            actual_path.display(),
        );
    }
}

/// Build a canonical (constant-valued) [`PaneMeta`] used as a fixture in
/// multiple envelope goldens.
fn canonical_pane_meta() -> PaneMeta {
    PaneMeta {
        pane_id: 42,
        pane_uuid: Some("abc-def-123".to_string()),
        domain: "local".to_string(),
        title: Some("claude-code".to_string()),
        cwd: Some("/Users/jemanuel/projects/demo".to_string()),
        rows: Some(64),
        cols: Some(220),
        observed: true,
        timestamp_ms: 1_700_000_000_000,
    }
}

/// Wrap a payload in a canonical envelope with fixed seq, sender, and time
/// so the golden is byte-stable across runs and machines.
fn canonical_envelope(seq: u64, payload: WirePayload) -> WireEnvelope {
    WireEnvelope {
        version: PROTOCOL_VERSION,
        seq,
        sender: "agent-alpha".to_string(),
        sent_at_ms: 1_700_000_000_000,
        payload,
    }
}

// ─── Wire protocol envelope goldens ────────────────────────────────────
//
// One golden per `WirePayload` variant. All envelope fields are constants;
// every field of every payload is populated (no `Option::None` hides a
// default path that could drift silently). A round-trip check on every
// golden catches the inverse shape: bytes on disk that we can no longer
// deserialize into the current struct layout.

#[test]
fn golden_envelope_pane_meta() {
    let env = canonical_envelope(1, WirePayload::PaneMeta(canonical_pane_meta()));
    let bytes = env.to_json().expect("serialize PaneMeta envelope");
    assert_golden_bytes("wire_protocol/envelope_pane_meta.json", &bytes);

    // Round-trip guard: golden bytes must still deserialize into the
    // current struct. Catches field-rename drift that preserves byte
    // output by coincidence but breaks the type.
    let back = WireEnvelope::from_json(&bytes).expect("round-trip");
    assert_eq!(back, env, "round-trip mismatch for PaneMeta");
}

#[test]
fn golden_envelope_pane_delta() {
    let env = canonical_envelope(
        2,
        WirePayload::PaneDelta(PaneDelta {
            pane_id: 42,
            seq: 55,
            content: "Hello, world!\nbuilding demo...".to_string(),
            content_len: 30,
            captured_at_ms: 1_700_000_001_000,
        }),
    );
    let bytes = env.to_json().expect("serialize PaneDelta envelope");
    assert_golden_bytes("wire_protocol/envelope_pane_delta.json", &bytes);

    let back = WireEnvelope::from_json(&bytes).expect("round-trip");
    assert_eq!(back, env);
}

#[test]
fn golden_envelope_gap() {
    let env = canonical_envelope(
        3,
        WirePayload::Gap(GapNotice {
            pane_id: 42,
            seq_before: 5,
            seq_after: 10,
            reason: "daemon_restart".to_string(),
            detected_at_ms: 1_700_000_002_000,
        }),
    );
    let bytes = env.to_json().expect("serialize Gap envelope");
    assert_golden_bytes("wire_protocol/envelope_gap.json", &bytes);

    let back = WireEnvelope::from_json(&bytes).expect("round-trip");
    assert_eq!(back, env);
}

#[test]
fn golden_envelope_detection() {
    // `extracted` is a `serde_json::Value`. Use a simple string value so
    // the golden doesn't depend on HashMap iteration order — a nested
    // object would serialize key-order dependent on insertion order,
    // which is generally stable for small serde_json::Map but we don't
    // need to test that here.
    let extracted = serde_json::json!({ "reset_time": "2:30 PM" });

    let env = canonical_envelope(
        4,
        WirePayload::Detection(DetectionNotice {
            rule_id: "codex.usage.reached".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 1.0,
            extracted,
            matched_text: "You've hit your usage limit".to_string(),
            pane_id: 42,
            pane_uuid: Some("abc-def-123".to_string()),
            detected_at_ms: 1_700_000_003_000,
        }),
    );
    let bytes = env.to_json().expect("serialize Detection envelope");
    assert_golden_bytes("wire_protocol/envelope_detection.json", &bytes);

    let back = WireEnvelope::from_json(&bytes).expect("round-trip");
    assert_eq!(back, env);
}

#[test]
fn golden_envelope_panes_meta() {
    // Two panes in fixed order — Vec preserves insertion order so the
    // golden is byte-stable.
    let pane_a = canonical_pane_meta();
    let pane_b = PaneMeta {
        pane_id: 43,
        pane_uuid: None,                      // exercise None
        domain: "ssh://remote".to_string(),
        title: None,                          // exercise None
        cwd: None,                            // exercise None
        rows: Some(24),
        cols: Some(80),
        observed: false,
        timestamp_ms: 1_700_000_000_500,
    };
    let env = canonical_envelope(
        5,
        WirePayload::PanesMeta(PanesMeta {
            panes: vec![pane_a, pane_b],
            timestamp_ms: 1_700_000_000_000,
        }),
    );
    let bytes = env.to_json().expect("serialize PanesMeta envelope");
    assert_golden_bytes("wire_protocol/envelope_panes_meta.json", &bytes);

    let back = WireEnvelope::from_json(&bytes).expect("round-trip");
    assert_eq!(back, env);
}

// ─── Snapshot schema goldens ───────────────────────────────────────────

/// Every variant of `SnapshotTrigger` serialized as an array.
///
/// Variants are listed in declaration order so the golden reflects the
/// authoritative enum order. A reorder, rename, or added variant will
/// flip the golden. Adding a variant is a schema change — the golden
/// update is the review gate.
#[test]
fn golden_snapshot_triggers_all_variants() {
    let all = [
        SnapshotTrigger::Periodic,
        SnapshotTrigger::PeriodicFallback,
        SnapshotTrigger::Manual,
        SnapshotTrigger::Shutdown,
        SnapshotTrigger::Startup,
        SnapshotTrigger::Event,
        SnapshotTrigger::WorkCompleted,
        SnapshotTrigger::HazardThreshold,
        SnapshotTrigger::StateTransition,
        SnapshotTrigger::IdleWindow,
        SnapshotTrigger::MemoryPressure,
    ];

    // Exhaustiveness gate. If a new `SnapshotTrigger` variant is added to
    // the source enum, this match stops compiling — forcing the author to
    // add the variant to `all` above AND regenerate the golden. The
    // `#[allow(unreachable_patterns)]` is there because `_ =>` would
    // silently eat new variants; we want the compile error.
    #[allow(clippy::match_same_arms)]
    fn _exhaustiveness_gate(t: SnapshotTrigger) {
        match t {
            SnapshotTrigger::Periodic
            | SnapshotTrigger::PeriodicFallback
            | SnapshotTrigger::Manual
            | SnapshotTrigger::Shutdown
            | SnapshotTrigger::Startup
            | SnapshotTrigger::Event
            | SnapshotTrigger::WorkCompleted
            | SnapshotTrigger::HazardThreshold
            | SnapshotTrigger::StateTransition
            | SnapshotTrigger::IdleWindow
            | SnapshotTrigger::MemoryPressure => {}
        }
    }
    let bytes = serde_json::to_vec(&all).expect("serialize SnapshotTrigger list");
    assert_golden_bytes("snapshot/snapshot_triggers.json", &bytes);

    // Round-trip: deserialize the golden back and compare.
    let back: Vec<SnapshotTrigger> =
        serde_json::from_slice(&bytes).expect("round-trip SnapshotTrigger");
    assert_eq!(back.len(), all.len());
    for (b, a) in back.iter().zip(all.iter()) {
        assert_eq!(b, a, "SnapshotTrigger round-trip variant mismatch");
    }
}

// ─── Protocol version guard ────────────────────────────────────────────

/// Catches an accidental bump of `PROTOCOL_VERSION` — that's a wire
/// change and must be paired with a deliberate golden regeneration
/// across every `wire_protocol/*.json` file.
#[test]
fn protocol_version_is_pinned() {
    // When you genuinely bump PROTOCOL_VERSION, change this constant
    // AND regenerate every wire_protocol/*.json golden. That two-site
    // change is the review surface.
    const PINNED: u32 = 1;
    assert_eq!(
        PROTOCOL_VERSION, PINNED,
        "PROTOCOL_VERSION changed from {PINNED} to {PROTOCOL_VERSION} — \
         this is a wire protocol migration. Update this test to the new \
         value AND regenerate every tests/golden/wire_protocol/*.json \
         golden (UPDATE_GOLDENS=1)."
    );
}
