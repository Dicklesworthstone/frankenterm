//! Persisted operator kill switch (ft-xxfwy.14, closing ft-l59nq).
//!
//! `PolicyEngine` is process-local and every `ft` invocation, as well as the
//! watcher's auto-handler, builds its own. The graduated SoftStop / HardStop /
//! EmergencyHalt gate in `PolicyEngine::evaluate_authorization` (fix
//! f8c674376) was therefore unreachable in production: nothing outside a unit
//! test ever tripped it, and a tier tripped in one process was invisible to
//! every other one. `ft doctor` said as much (`process-local: fresh engine`).
//!
//! This module gives the kill switch one durable home: the workspace
//! database's generic `config` KV table (baseline schema, present in every
//! DB) under [`KILL_SWITCH_STATE_KEY`]. Every production engine restores the
//! persisted tier at construction through [`apply_persisted_kill_switch`], and
//! `ft robot kill-switch trip|reset|status` is the operator surface that
//! writes it.
//!
//! Fail-closed rules:
//!
//! - a missing key is the genuine "never armed" state (`Disarmed`);
//! - a value that cannot be read or decoded arms **HardStop** in the engine
//!   being restored and is reported as [`KillSwitchRestore::FailedClosed`];
//!   a corrupt blob must never silently disarm the switch;
//! - JSON arrays and non-objects are rejected explicitly (an all-default
//!   struct would otherwise deserialize positionally from an array);
//! - restore never touches the engine's audit chain or telemetry: it is
//!   persistence rehydration, not a new trip.
//!
//! No-claim: persistence makes the tier visible to every *new* engine. A
//! long-running watcher restores the tier when it starts and does not poll
//! for later changes; that is documented in
//! `docs/robot-contracts/kill-switch.md`.

use serde::{Deserialize, Serialize};

use crate::policy::PolicyEngine;
use crate::policy_quarantine::{KillSwitch, KillSwitchLevel};
use crate::storage_backend_trait::StorageBackend;

/// `config` KV key holding the persisted kill switch.
pub const KILL_SWITCH_STATE_KEY: &str = "policy.kill_switch_v1";

/// Schema tag written into every persisted value.
pub const KILL_SWITCH_STATE_SCHEMA: u32 = 1;

/// Actor recorded when a restore fails closed.
pub const FAIL_CLOSED_ACTOR: &str = "kill_switch_restore";

/// Persisted representation. Mirrors [`KillSwitch`] plus a schema tag so a
/// future shape change is detected instead of misread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedKillSwitch {
    /// Always [`KILL_SWITCH_STATE_SCHEMA`].
    pub schema: u32,
    /// Current tier.
    pub level: KillSwitchLevel,
    /// When the tier last changed (epoch ms).
    pub changed_at_ms: u64,
    /// Who changed it.
    pub changed_by: String,
    /// Why.
    pub reason: String,
    /// Auto-disarm deadline (0 = none).
    pub auto_disarm_at_ms: u64,
}

impl From<&KillSwitch> for PersistedKillSwitch {
    fn from(ks: &KillSwitch) -> Self {
        Self {
            schema: KILL_SWITCH_STATE_SCHEMA,
            level: ks.level,
            changed_at_ms: ks.changed_at_ms,
            changed_by: ks.changed_by.clone(),
            reason: ks.reason.clone(),
            auto_disarm_at_ms: ks.auto_disarm_at_ms,
        }
    }
}

impl From<PersistedKillSwitch> for KillSwitch {
    fn from(p: PersistedKillSwitch) -> Self {
        let mut ks = KillSwitch::disarmed();
        ks.level = p.level;
        ks.changed_at_ms = p.changed_at_ms;
        ks.changed_by = p.changed_by;
        ks.reason = p.reason;
        ks.auto_disarm_at_ms = p.auto_disarm_at_ms;
        ks
    }
}

/// Typed failure of the persistence layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillSwitchStateError {
    /// The `config` row could not be read.
    LoadFailed(String),
    /// The row exists but is not a valid persisted kill switch.
    Corrupt(String),
    /// The row could not be written.
    SaveFailed(String),
}

impl KillSwitchStateError {
    /// Stable robot error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::LoadFailed(_) => "robot.kill_switch.state_load_failed",
            Self::Corrupt(_) => "robot.kill_switch.state_corrupt",
            Self::SaveFailed(_) => "robot.kill_switch.state_save_failed",
        }
    }

    /// Human-readable detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::LoadFailed(d) | Self::Corrupt(d) | Self::SaveFailed(d) => d,
        }
    }
}

impl std::fmt::Display for KillSwitchStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.detail())
    }
}

impl std::error::Error for KillSwitchStateError {}

/// What [`apply_persisted_kill_switch`] did to the engine.
#[derive(Debug, Clone)]
pub enum KillSwitchRestore {
    /// No persisted row: the engine keeps its constructed (disarmed) state.
    Absent,
    /// The persisted tier was installed. `auto_disarmed` is true when the
    /// persisted auto-disarm deadline had already passed at restore time, in
    /// which case the engine is disarmed and the caller should persist that.
    Restored {
        /// The state now installed in the engine.
        state: KillSwitch,
        /// Whether the auto-disarm deadline lapsed during restore.
        auto_disarmed: bool,
    },
    /// The row could not be read or decoded: HardStop was armed instead.
    FailedClosed {
        /// The persistence error that caused the fail-closed arm.
        error: KillSwitchStateError,
    },
}

impl KillSwitchRestore {
    /// Short machine label for envelopes and doctor rows.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Restored { .. } => "restored",
            Self::FailedClosed { .. } => "failed_closed",
        }
    }
}

/// Serialize a kill switch for the `config` row.
pub fn encode_kill_switch_state(ks: &KillSwitch) -> Result<String, KillSwitchStateError> {
    serde_json::to_string(&PersistedKillSwitch::from(ks))
        .map_err(|e| KillSwitchStateError::SaveFailed(format!("encode: {e}")))
}

/// Parse a `config` row value. Rejects non-objects explicitly so an array or
/// scalar cannot be coerced into a valid-looking state.
pub fn decode_kill_switch_state(raw: &str) -> Result<KillSwitch, KillSwitchStateError> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| KillSwitchStateError::Corrupt(format!("not JSON: {e}")))?;
    if !value.is_object() {
        return Err(KillSwitchStateError::Corrupt(
            "persisted kill switch must be a JSON object".to_string(),
        ));
    }
    let persisted: PersistedKillSwitch = serde_json::from_value(value)
        .map_err(|e| KillSwitchStateError::Corrupt(format!("shape: {e}")))?;
    if persisted.schema != KILL_SWITCH_STATE_SCHEMA {
        return Err(KillSwitchStateError::Corrupt(format!(
            "schema {} is not {KILL_SWITCH_STATE_SCHEMA}",
            persisted.schema
        )));
    }
    Ok(persisted.into())
}

/// Install the persisted tier into `engine`.
///
/// `loaded` is the raw `config` row read (`Ok(None)` = never written). Any
/// read or decode failure arms HardStop; see the module docs.
pub fn apply_persisted_kill_switch(
    engine: &mut PolicyEngine,
    loaded: Result<Option<String>, KillSwitchStateError>,
    now_ms: u64,
) -> KillSwitchRestore {
    let raw = match loaded {
        Ok(None) => return KillSwitchRestore::Absent,
        Ok(Some(raw)) => raw,
        Err(error) => {
            fail_closed(engine, &error, now_ms);
            return KillSwitchRestore::FailedClosed { error };
        }
    };
    match decode_kill_switch_state(&raw) {
        Ok(mut state) => {
            let auto_disarmed = state.tick(now_ms);
            engine.restore_kill_switch(state.clone());
            KillSwitchRestore::Restored {
                state,
                auto_disarmed,
            }
        }
        Err(error) => {
            fail_closed(engine, &error, now_ms);
            KillSwitchRestore::FailedClosed { error }
        }
    }
}

fn fail_closed(engine: &mut PolicyEngine, error: &KillSwitchStateError, now_ms: u64) {
    let mut state = KillSwitch::disarmed();
    state.trip(
        KillSwitchLevel::HardStop,
        FAIL_CLOSED_ACTOR,
        &format!("persisted kill switch unreadable, failing closed ({error})"),
        now_ms,
    );
    engine.restore_kill_switch(state);
}

/// Read the persisted row through a one-shot backend.
pub fn load_kill_switch_state(
    backend: &dyn StorageBackend,
) -> Result<Option<String>, KillSwitchStateError> {
    crate::storage_backend_helpers::get_config_kv(backend, KILL_SWITCH_STATE_KEY)
        .map_err(|e| KillSwitchStateError::LoadFailed(e.to_string()))
}

/// Write the current engine kill switch through a one-shot backend.
pub fn persist_kill_switch_state(
    backend: &dyn StorageBackend,
    ks: &KillSwitch,
    now_ms: i64,
) -> Result<(), KillSwitchStateError> {
    let json = encode_kill_switch_state(ks)?;
    crate::storage_backend_helpers::set_config_kv(backend, KILL_SWITCH_STATE_KEY, &json, now_ms)
        .map_err(|e| KillSwitchStateError::SaveFailed(e.to_string()))
}

/// Restore through a one-shot backend: load + apply in one call.
pub fn restore_kill_switch_from_backend(
    engine: &mut PolicyEngine,
    backend: &dyn StorageBackend,
    now_ms: u64,
) -> KillSwitchRestore {
    apply_persisted_kill_switch(engine, load_kill_switch_state(backend), now_ms)
}

/// Restore through the writer-loop [`crate::storage::StorageHandle`]
/// (Cx-first: the read is checkpointed against `cx`).
pub async fn restore_kill_switch_from_storage_with_cx(
    cx: &crate::cx::Cx,
    engine: &mut PolicyEngine,
    storage: &crate::storage::StorageHandle,
    now_ms: u64,
) -> KillSwitchRestore {
    let loaded = storage
        .get_config_value_with_cx(cx, KILL_SWITCH_STATE_KEY)
        .await
        .map_err(|e| KillSwitchStateError::LoadFailed(e.to_string()));
    apply_persisted_kill_switch(engine, loaded, now_ms)
}

/// Persist through the writer-loop [`crate::storage::StorageHandle`]
/// (Cx-first: the write is checkpointed against `cx`).
pub async fn persist_kill_switch_to_storage_with_cx(
    cx: &crate::cx::Cx,
    storage: &crate::storage::StorageHandle,
    ks: &KillSwitch,
) -> Result<(), KillSwitchStateError> {
    let json = encode_kill_switch_state(ks)?;
    storage
        .set_config_value_with_cx(cx, KILL_SWITCH_STATE_KEY, &json)
        .await
        .map_err(|e| KillSwitchStateError::SaveFailed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armed(level: KillSwitchLevel) -> KillSwitch {
        let mut ks = KillSwitch::disarmed();
        ks.trip(level, "operator", "incident 42", 1_000);
        ks
    }

    #[test]
    fn encode_decode_round_trip_preserves_every_field() {
        let mut ks = armed(KillSwitchLevel::SoftStop);
        ks.auto_disarm_at_ms = 9_000;
        let raw = encode_kill_switch_state(&ks).expect("encode");
        let back = decode_kill_switch_state(&raw).expect("decode");
        assert_eq!(back.level, KillSwitchLevel::SoftStop);
        assert_eq!(back.changed_at_ms, 1_000);
        assert_eq!(back.changed_by, "operator");
        assert_eq!(back.reason, "incident 42");
        assert_eq!(back.auto_disarm_at_ms, 9_000);
        assert!(raw.contains("\"schema\":1"), "schema tag written: {raw}");
    }

    #[test]
    fn decode_rejects_arrays_scalars_garbage_and_foreign_schema() {
        for raw in [
            "[1,\"soft_stop\",0,\"\",\"\",0]",
            "\"soft_stop\"",
            "42",
            "null",
            "{not json",
            "{\"schema\":2,\"level\":\"soft_stop\",\"changed_at_ms\":0,\"changed_by\":\"\",\"reason\":\"\",\"auto_disarm_at_ms\":0}",
            "{\"schema\":1,\"level\":\"nuke\",\"changed_at_ms\":0,\"changed_by\":\"\",\"reason\":\"\",\"auto_disarm_at_ms\":0}",
            "{\"schema\":1,\"level\":\"soft_stop\",\"changed_at_ms\":0,\"changed_by\":\"\",\"reason\":\"\",\"auto_disarm_at_ms\":0,\"extra\":1}",
        ] {
            let err = decode_kill_switch_state(raw).expect_err(raw);
            assert_eq!(err.code(), "robot.kill_switch.state_corrupt", "{raw}");
        }
    }

    #[test]
    fn absent_row_leaves_a_fresh_engine_disarmed() {
        let mut engine = PolicyEngine::permissive();
        let outcome = apply_persisted_kill_switch(&mut engine, Ok(None), 5_000);
        assert!(matches!(outcome, KillSwitchRestore::Absent), "{outcome:?}");
        assert_eq!(engine.kill_switch_state().level, KillSwitchLevel::Disarmed);
    }

    #[test]
    fn persisted_soft_stop_is_restored_without_touching_the_audit_chain() {
        let mut engine = PolicyEngine::permissive();
        let audit_before = engine.audit_chain().len();
        let raw = encode_kill_switch_state(&armed(KillSwitchLevel::SoftStop)).expect("encode");
        let outcome = apply_persisted_kill_switch(&mut engine, Ok(Some(raw)), 5_000);
        match outcome {
            KillSwitchRestore::Restored {
                state,
                auto_disarmed,
            } => {
                assert_eq!(state.level, KillSwitchLevel::SoftStop);
                assert!(!auto_disarmed);
            }
            other => panic!("expected Restored, got {other:?}"),
        }
        assert_eq!(engine.kill_switch_state().level, KillSwitchLevel::SoftStop);
        assert_eq!(engine.kill_switch_state().changed_by, "operator");
        assert_eq!(
            engine.audit_chain().len(),
            audit_before,
            "restore is rehydration, not a new trip"
        );
    }

    #[test]
    fn corrupt_row_arms_hard_stop_and_reports_failed_closed() {
        let mut engine = PolicyEngine::permissive();
        let outcome =
            apply_persisted_kill_switch(&mut engine, Ok(Some("[\"soft_stop\"]".into())), 5_000);
        match &outcome {
            KillSwitchRestore::FailedClosed { error } => {
                assert_eq!(error.code(), "robot.kill_switch.state_corrupt");
            }
            other => panic!("expected FailedClosed, got {other:?}"),
        }
        let ks = engine.kill_switch_state();
        assert_eq!(ks.level, KillSwitchLevel::HardStop);
        assert_eq!(ks.changed_by, FAIL_CLOSED_ACTOR);
        assert!(ks.reason.contains("failing closed"), "{}", ks.reason);
    }

    #[test]
    fn unreadable_row_arms_hard_stop() {
        let mut engine = PolicyEngine::permissive();
        let outcome = apply_persisted_kill_switch(
            &mut engine,
            Err(KillSwitchStateError::LoadFailed("disk on fire".into())),
            5_000,
        );
        assert!(matches!(outcome, KillSwitchRestore::FailedClosed { .. }));
        assert_eq!(engine.kill_switch_state().level, KillSwitchLevel::HardStop);
    }

    #[test]
    fn lapsed_auto_disarm_deadline_restores_as_disarmed_and_says_so() {
        let mut engine = PolicyEngine::permissive();
        let mut ks = armed(KillSwitchLevel::HardStop);
        ks.auto_disarm_at_ms = 2_000;
        let raw = encode_kill_switch_state(&ks).expect("encode");
        let outcome = apply_persisted_kill_switch(&mut engine, Ok(Some(raw)), 3_000);
        match outcome {
            KillSwitchRestore::Restored {
                state,
                auto_disarmed,
            } => {
                assert!(auto_disarmed);
                assert_eq!(state.level, KillSwitchLevel::Disarmed);
            }
            other => panic!("expected Restored, got {other:?}"),
        }
        assert_eq!(engine.kill_switch_state().level, KillSwitchLevel::Disarmed);
    }

    #[test]
    fn backend_round_trip_restores_the_tier_into_a_second_engine() {
        let backend = crate::storage_backend_trait::RusqliteBackend::open(
            ":memory:",
            &crate::storage_backend_trait::OpenConfig::default(),
        )
        .expect("in-memory backend");
        backend
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS config (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL)",
            )
            .expect("config table");
        let mut first = PolicyEngine::permissive();
        assert!(first.trip_kill_switch(KillSwitchLevel::HardStop, "operator", "drill", 1_000));
        persist_kill_switch_state(&backend, first.kill_switch_state(), 1_000).expect("persist");

        let mut second = PolicyEngine::permissive();
        let outcome = restore_kill_switch_from_backend(&mut second, &backend, 2_000);
        assert!(
            matches!(outcome, KillSwitchRestore::Restored { .. }),
            "{outcome:?}"
        );
        assert_eq!(second.kill_switch_state().level, KillSwitchLevel::HardStop);
        assert_eq!(second.kill_switch_state().reason, "drill");

        second
            .quarantine_registry_mut()
            .reset_kill_switch("operator", 3_000);
        persist_kill_switch_state(&backend, second.kill_switch_state(), 3_000).expect("persist");
        let mut third = PolicyEngine::permissive();
        restore_kill_switch_from_backend(&mut third, &backend, 4_000);
        assert_eq!(third.kill_switch_state().level, KillSwitchLevel::Disarmed);
    }
}
