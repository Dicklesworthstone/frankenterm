//! Wire-format conformance harness for `PaneStateSnapshot` (session
//! snapshot) and `CheckpointState` (replay checkpoint manifest).
//!
//! The existing `proptest_session_pane_state.rs` and
//! `proptest_replay_checkpoint.rs` suites cover struct-level round-
//! trips. This file targets the ORTHOGONAL surface: the JSON wire
//! shape itself must conform to a frozen specification so downstream
//! consumers (session-restore tooling, `ft replay` checkpoints on
//! disk, cross-version restore) don't silently break when a derive,
//! rename, or `#[serde(default)]` attribute drifts.
//!
//! The two conformance harnesses below pin:
//!
//! - **Required top-level keys** — a rename of any required wire
//!   field must fail here rather than silently changing the on-disk
//!   format.
//! - **Schema-version contract** — `PaneStateSnapshot::schema_version`
//!   must equal `PANE_STATE_SCHEMA_VERSION` on construction, and
//!   `CheckpointState::checkpoint_version` must equal the crate-
//!   public `CHECKPOINT_VERSION` constant `"ft.replay.checkpoint.v1"`.
//! - **Optional-field omission** — fields tagged
//!   `#[serde(skip_serializing_if = "Option::is_none")]` must be
//!   absent from the JSON object when None; regressions that drop
//!   the attribute would surface extra `"key": null` payloads on the
//!   wire and break forward/backward compat.
//! - **Reserialize idempotence** — serialize → deserialize →
//!   serialize must produce byte-identical JSON on the second round
//!   (canonicalized). Catches drift in field ordering or number
//!   encoding that would break exact-string checkpointed fixtures.
//! - **Forward compat** — payloads carrying unknown top-level fields
//!   must parse cleanly and the unknown fields must not corrupt the
//!   typed result.
//!
//! Inputs are generated with proptest and fed through the real serde
//! serializer, so random shrinking can localize any conformance
//! violation to a minimal counterexample.
//!
//! Domain: session/checkpoint wire conformance (pane 5).

use frankenterm_core::replay_checkpoint::{CHECKPOINT_VERSION, CheckpointState};
use frankenterm_core::session_pane_state::{
    AgentMetadata, CapturedEnv, PANE_STATE_SCHEMA_VERSION, PaneStateSnapshot, ProcessInfo,
    ScrollbackRef, TerminalState,
};
use proptest::prelude::*;
use serde_json::Value;
use std::collections::HashMap;

// ── Canonicalization ────────────────────────────────────────────────────

/// Recursively sort object keys so byte-equality tests don't depend
/// on HashMap iteration order.
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: Vec<(&String, &Value)> = map.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = serde_json::Map::new();
            for (k, v) in sorted {
                out.insert(k.clone(), canonicalize(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

fn canonical_json(value: &Value) -> String {
    serde_json::to_string(&canonicalize(value)).expect("canonical serialize")
}

// ── Strategies ──────────────────────────────────────────────────────────

fn arb_small_ascii() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[A-Za-z0-9_./:-]{0,24}").expect("ascii regex")
}

fn arb_terminal_state() -> impl Strategy<Value = TerminalState> {
    (
        0u16..=512,
        0u16..=512,
        0u16..=512,
        0u16..=512,
        any::<bool>(),
        arb_small_ascii(),
    )
        .prop_map(
            |(rows, cols, cursor_row, cursor_col, is_alt_screen, title)| TerminalState {
                rows,
                cols,
                cursor_row,
                cursor_col,
                is_alt_screen,
                title,
            },
        )
}

fn arb_process_info() -> impl Strategy<Value = ProcessInfo> {
    (
        arb_small_ascii(),
        prop::option::of(any::<u32>()),
        prop::option::of(proptest::collection::vec(arb_small_ascii(), 0..5)),
    )
        .prop_map(|(name, pid, argv)| ProcessInfo { name, pid, argv })
}

fn arb_scrollback_ref() -> impl Strategy<Value = ScrollbackRef> {
    (0i64..10_000, 0u64..10_000, 0u64..10_000_000).prop_map(
        |(output_segments_seq, total_lines_captured, last_capture_at)| ScrollbackRef {
            output_segments_seq,
            total_lines_captured,
            last_capture_at,
        },
    )
}

fn arb_agent_metadata() -> impl Strategy<Value = AgentMetadata> {
    (
        proptest::sample::select(vec!["claude_code", "codex", "gemini", "wezterm", "unknown"])
            .prop_map(String::from),
        prop::option::of(arb_small_ascii()),
        prop::option::of(proptest::sample::select(vec![
            "idle",
            "working",
            "rate_limited",
            "waiting",
        ]))
        .prop_map(|v| v.map(String::from)),
    )
        .prop_map(|(agent_type, session_id, state)| AgentMetadata {
            agent_type,
            session_id,
            state,
        })
}

fn arb_captured_env() -> impl Strategy<Value = CapturedEnv> {
    (
        proptest::collection::hash_map(arb_small_ascii(), arb_small_ascii(), 0..4),
        0usize..16,
    )
        .prop_map(|(vars, redacted_count)| CapturedEnv {
            vars,
            redacted_count,
        })
}

fn arb_pane_state_snapshot() -> impl Strategy<Value = PaneStateSnapshot> {
    (
        any::<u64>(),
        any::<u64>(),
        arb_terminal_state(),
        prop::option::of(arb_small_ascii()),
        prop::option::of(arb_process_info()),
        prop::option::of(arb_small_ascii()),
        prop::option::of(arb_scrollback_ref()),
        prop::option::of(arb_agent_metadata()),
        prop::option::of(arb_captured_env()),
    )
        .prop_map(
            |(
                pane_id,
                captured_at,
                terminal,
                cwd,
                foreground_process,
                shell,
                scrollback_ref,
                agent,
                env,
            )| PaneStateSnapshot {
                schema_version: PANE_STATE_SCHEMA_VERSION,
                pane_id,
                captured_at,
                cwd,
                foreground_process,
                shell,
                terminal,
                scrollback_ref,
                agent,
                env,
            },
        )
}

fn arb_checkpoint_state() -> impl Strategy<Value = CheckpointState> {
    (
        arb_small_ascii(),
        0u64..100_000,
        0u64..1_000_000,
        0u64..10_000,
        0u64..10_000,
        0u64..10_000,
        0u64..10_000,
        "[0-9a-f]{16}",
        0u64..10_000_000,
    )
        .prop_map(
            |(
                replay_run_id,
                event_position,
                virtual_clock_ms,
                decisions_made,
                events_skipped,
                effects_logged,
                anomalies_detected,
                effect_log_hash,
                checkpoint_created_ms,
            )| {
                let mut cp = CheckpointState::new(replay_run_id);
                cp.event_position = event_position;
                cp.virtual_clock_ms = virtual_clock_ms;
                cp.decisions_made = decisions_made;
                cp.events_skipped = events_skipped;
                cp.effects_logged = effects_logged;
                cp.anomalies_detected = anomalies_detected;
                cp.effect_log_hash = effect_log_hash;
                cp.checkpoint_created_ms = checkpoint_created_ms;
                cp
            },
        )
}

// ── Properties ──────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // ===== PaneStateSnapshot (session snapshot) =====================

    /// Required top-level keys must always appear on the wire.
    /// Renaming `schema_version`, `pane_id`, `captured_at`, or
    /// `terminal` must fail this test, not silently drift.
    #[test]
    fn pane_state_snapshot_required_keys_present(
        snapshot in arb_pane_state_snapshot(),
    ) {
        let value: Value =
            serde_json::to_value(&snapshot).expect("serialize PaneStateSnapshot");
        let object = value
            .as_object()
            .expect("PaneStateSnapshot must serialize as JSON object");
        for required in ["schema_version", "pane_id", "captured_at", "terminal"] {
            prop_assert!(
                object.contains_key(required),
                "PaneStateSnapshot wire format must include required key `{}`; got keys {:?}",
                required,
                object.keys().collect::<Vec<_>>()
            );
        }
        // schema_version must be the crate-public constant.
        prop_assert_eq!(
            object.get("schema_version").and_then(Value::as_u64),
            Some(PANE_STATE_SCHEMA_VERSION as u64),
            "schema_version on wire must equal PANE_STATE_SCHEMA_VERSION"
        );
    }

    /// Optional fields tagged `skip_serializing_if = "Option::is_none"`
    /// must be absent from the JSON object when None. Catches drift
    /// that would start emitting `"cwd": null` etc. on the wire.
    #[test]
    fn pane_state_snapshot_skip_none_fields_absent_when_none(
        terminal in arb_terminal_state(),
        pane_id in any::<u64>(),
        captured_at in any::<u64>(),
    ) {
        let snapshot = PaneStateSnapshot {
            schema_version: PANE_STATE_SCHEMA_VERSION,
            pane_id,
            captured_at,
            cwd: None,
            foreground_process: None,
            shell: None,
            terminal,
            scrollback_ref: None,
            agent: None,
            env: None,
        };
        let value: Value = serde_json::to_value(&snapshot).unwrap();
        let object = value.as_object().unwrap();
        for optional in ["cwd", "foreground_process", "shell", "scrollback_ref", "agent", "env"] {
            prop_assert!(
                !object.contains_key(optional),
                "optional `{}` must be absent when None; got keys {:?}",
                optional,
                object.keys().collect::<Vec<_>>()
            );
        }
    }

    /// Reserialize idempotence: serialize → deserialize → serialize
    /// must produce byte-identical canonicalized JSON. Regression
    /// guard against drift in field ordering / number encoding.
    #[test]
    fn pane_state_snapshot_reserialize_is_idempotent(
        snapshot in arb_pane_state_snapshot(),
    ) {
        let first_value: Value = serde_json::to_value(&snapshot).unwrap();
        let roundtrip: PaneStateSnapshot = serde_json::from_value(first_value.clone()).unwrap();
        let second_value: Value = serde_json::to_value(&roundtrip).unwrap();
        prop_assert_eq!(
            canonical_json(&first_value),
            canonical_json(&second_value),
            "second-round serialization must be byte-identical to the first round"
        );
        // PaneStateSnapshot derives PartialEq/Eq — the strong contract.
        prop_assert_eq!(
            snapshot,
            roundtrip,
            "struct-level equality must hold across roundtrip"
        );
    }

    /// Forward compat: payloads carrying unknown top-level fields
    /// must parse cleanly. Downstream consumers can add fields and
    /// the typed deserializer must tolerate them.
    #[test]
    fn pane_state_snapshot_forward_compat_unknown_fields_ignored(
        snapshot in arb_pane_state_snapshot(),
        unknown_key in "[a-z_]{3,16}",
    ) {
        // Avoid accidentally collision with real field names.
        prop_assume!(
            !["schema_version","pane_id","captured_at","cwd","foreground_process","shell","terminal","scrollback_ref","agent","env"]
                .contains(&unknown_key.as_str())
        );

        let mut value: Value = serde_json::to_value(&snapshot).unwrap();
        value.as_object_mut().unwrap().insert(
            unknown_key,
            Value::String("future-field-value".to_string()),
        );

        let parsed: PaneStateSnapshot =
            serde_json::from_value(value).expect("forward-compat parse must succeed");
        prop_assert_eq!(parsed, snapshot, "unknown fields must not corrupt typed result");
    }

    // ===== CheckpointState (replay checkpoint manifest) =============

    /// Required top-level keys for checkpoint manifests on disk.
    /// Regressions in `to_json()` (e.g. rename to camelCase, drop of
    /// `replay_run_id`) must fail this test before they ship.
    #[test]
    fn checkpoint_state_required_keys_present(
        state in arb_checkpoint_state(),
    ) {
        let json = state.to_json();
        let value: Value =
            serde_json::from_str(&json).expect("to_json output must be valid JSON");
        let object = value
            .as_object()
            .expect("CheckpointState must serialize as JSON object");
        for required in [
            "checkpoint_version",
            "event_position",
            "virtual_clock_ms",
            "decisions_made",
            "events_skipped",
            "effects_logged",
            "anomalies_detected",
            "effect_log_hash",
            "replay_run_id",
            "checkpoint_created_ms",
        ] {
            prop_assert!(
                object.contains_key(required),
                "CheckpointState wire format must include required key `{}`; got keys {:?}",
                required,
                object.keys().collect::<Vec<_>>()
            );
        }
        // checkpoint_version must equal the crate-public CHECKPOINT_VERSION
        // constant (currently "ft.replay.checkpoint.v1").
        prop_assert_eq!(
            object.get("checkpoint_version").and_then(Value::as_str),
            Some(CHECKPOINT_VERSION),
            "checkpoint_version on wire must equal CHECKPOINT_VERSION constant"
        );
    }

    /// Reserialize idempotence for CheckpointState. This is the
    /// conformance contract for `ft replay` checkpoint files on disk:
    /// read → re-write must produce byte-identical content so that
    /// checksum-based integrity checks can freeze the payload.
    #[test]
    fn checkpoint_state_reserialize_is_idempotent(
        state in arb_checkpoint_state(),
    ) {
        let first_json = state.to_json();
        let restored =
            CheckpointState::from_json(&first_json).expect("from_json must accept to_json output");
        let second_json = restored.to_json();
        let first_value: Value = serde_json::from_str(&first_json).unwrap();
        let second_value: Value = serde_json::from_str(&second_json).unwrap();
        prop_assert_eq!(
            canonical_json(&first_value),
            canonical_json(&second_value),
            "CheckpointState re-emit must be byte-identical after canonicalization"
        );
    }

    /// Field-level roundtrip: every scalar field must survive
    /// JSON → struct → JSON unchanged. CheckpointState has no
    /// `PartialEq` derive, so we compare via explicit field access.
    #[test]
    fn checkpoint_state_field_roundtrip_preserves_all_scalars(
        state in arb_checkpoint_state(),
    ) {
        let json = state.to_json();
        let r = CheckpointState::from_json(&json).unwrap();
        prop_assert_eq!(r.checkpoint_version, state.checkpoint_version.clone());
        prop_assert_eq!(r.event_position, state.event_position);
        prop_assert_eq!(r.virtual_clock_ms, state.virtual_clock_ms);
        prop_assert_eq!(r.decisions_made, state.decisions_made);
        prop_assert_eq!(r.events_skipped, state.events_skipped);
        prop_assert_eq!(r.effects_logged, state.effects_logged);
        prop_assert_eq!(r.anomalies_detected, state.anomalies_detected);
        prop_assert_eq!(r.effect_log_hash, state.effect_log_hash.clone());
        prop_assert_eq!(r.replay_run_id, state.replay_run_id.clone());
        prop_assert_eq!(r.checkpoint_created_ms, state.checkpoint_created_ms);
    }

    /// Forward compat: checkpoint files carrying unknown top-level
    /// fields must parse cleanly — lets future versions add fields
    /// without breaking older readers.
    #[test]
    fn checkpoint_state_forward_compat_unknown_fields_ignored(
        state in arb_checkpoint_state(),
        unknown_key in "[a-z_]{3,16}",
    ) {
        prop_assume!(
            !["checkpoint_version","event_position","virtual_clock_ms","decisions_made","events_skipped","effects_logged","anomalies_detected","effect_log_hash","replay_run_id","checkpoint_created_ms"]
                .contains(&unknown_key.as_str())
        );

        let mut value: Value = serde_json::from_str(&state.to_json()).unwrap();
        value.as_object_mut().unwrap().insert(
            unknown_key,
            Value::Number(42.into()),
        );
        let modified_json = serde_json::to_string(&value).unwrap();
        let parsed = CheckpointState::from_json(&modified_json)
            .expect("forward-compat parse must succeed for CheckpointState");
        prop_assert_eq!(parsed.event_position, state.event_position);
        prop_assert_eq!(parsed.replay_run_id, state.replay_run_id.clone());
    }
}

// ── Hand-rolled conformance regressions ─────────────────────────────────

#[test]
fn pane_state_snapshot_schema_version_constant_is_1() {
    // Pin the current schema version value. Any bump must be deliberate.
    assert_eq!(
        PANE_STATE_SCHEMA_VERSION, 1,
        "PANE_STATE_SCHEMA_VERSION changed — update wire consumers and bump this assertion"
    );
}

#[test]
fn checkpoint_state_version_constant_is_v1() {
    assert_eq!(
        CHECKPOINT_VERSION, "ft.replay.checkpoint.v1",
        "CHECKPOINT_VERSION changed — update wire consumers and bump this assertion"
    );
}

#[test]
fn checkpoint_state_new_emits_pinned_version() {
    let state = CheckpointState::new("run-abc".into());
    assert_eq!(state.checkpoint_version, CHECKPOINT_VERSION);
    assert_eq!(state.event_position, 0);
    assert_eq!(state.virtual_clock_ms, 0);
    assert_eq!(state.replay_run_id, "run-abc");
}

#[test]
fn pane_state_snapshot_minimal_payload_roundtrips() {
    let snapshot = PaneStateSnapshot {
        schema_version: PANE_STATE_SCHEMA_VERSION,
        pane_id: 1,
        captured_at: 1_700_000_000_000,
        cwd: None,
        foreground_process: None,
        shell: None,
        terminal: TerminalState {
            rows: 24,
            cols: 80,
            cursor_row: 0,
            cursor_col: 0,
            is_alt_screen: false,
            title: String::new(),
        },
        scrollback_ref: None,
        agent: None,
        env: None,
    };
    let json = serde_json::to_string(&snapshot).unwrap();
    let roundtripped: PaneStateSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(roundtripped, snapshot);
}

#[test]
fn checkpoint_state_from_json_rejects_invalid_json() {
    assert!(CheckpointState::from_json("").is_err());
    assert!(CheckpointState::from_json("{").is_err());
    assert!(CheckpointState::from_json("not json").is_err());
}

// Compile-time suppressed warning for the HashMap import in the CapturedEnv
// strategy; keep the explicit use so moves/renames fail loudly here too.
#[allow(dead_code)]
fn _hashmap_witness() -> HashMap<String, String> {
    HashMap::new()
}
