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
//! Storage-backed injectors also refresh under a workspace fence before each
//! effect. Operator transitions use the same fence, so a successful transition
//! cannot be overtaken by a send admitted under an older revision. This fences
//! the integrated injector, not legacy binaries or previously admitted remote
//! effects whose settlement is unknown.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::policy::PolicyEngine;
use crate::policy_quarantine::{KillSwitch, KillSwitchLevel};
use crate::storage_backend_trait::StorageBackend;

/// `config` KV key holding the persisted kill switch.
pub const KILL_SWITCH_STATE_KEY: &str = "policy.kill_switch_v1";

/// Separate revision authority permits an explicit reset to repair a corrupt
/// state blob without rolling back the watermark seen by a running injector.
pub const KILL_SWITCH_REVISION_KEY: &str = "policy.kill_switch_revision_v1";

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
    /// Monotone workspace revision. Existing schema-1 rows start at zero.
    #[serde(default)]
    pub revision: u64,
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
            revision: 0,
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
    /// Another effect or transition holds the workspace fence. Not applied.
    FencePending,
    /// Workspace identity or locking authority could not be established.
    FenceFailed,
}

impl KillSwitchStateError {
    /// Stable robot error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::LoadFailed(_) => "robot.kill_switch.state_load_failed",
            Self::Corrupt(_) => "robot.kill_switch.state_corrupt",
            Self::SaveFailed(_) => "robot.kill_switch.state_save_failed",
            Self::FencePending => "robot.kill_switch.fence_pending",
            Self::FenceFailed => "robot.kill_switch.fence_failed",
        }
    }

    /// Human-readable detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::LoadFailed(d) | Self::Corrupt(d) | Self::SaveFailed(d) => d,
            Self::FencePending => {
                "workspace effect or transition still active; transition not applied"
            }
            Self::FenceFailed => "workspace effect fence unavailable; transition not applied",
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
    decode_kill_switch_document(raw).map(Into::into)
}

fn decode_kill_switch_document(raw: &str) -> Result<PersistedKillSwitch, KillSwitchStateError> {
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
    Ok(persisted)
}

/// An exclusive workspace effect fence. Closing the file releases the lock,
/// including on cancellation or unwind; the persistent lock file is never
/// unlinked because replacing its inode would split the fencing domain.
pub struct KillSwitchFence {
    _file: std::fs::File,
    workspace: PathBuf,
}

/// Acquire without blocking an async worker. Contention is an explicit pending
/// outcome, never permission to dispatch or acknowledge an operator transition.
pub fn acquire_kill_switch_fence(db_path: &Path) -> Result<KillSwitchFence, KillSwitchStateError> {
    let canonical =
        std::fs::canonicalize(db_path).map_err(|_| KillSwitchStateError::FenceFailed)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata(&canonical)
            .map_err(|_| KillSwitchStateError::FenceFailed)?
            .nlink()
            != 1
        {
            return Err(KillSwitchStateError::FenceFailed);
        }
    }
    let mut lock_path = canonical.clone().into_os_string();
    lock_path.push(".policy-kill-switch.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(PathBuf::from(lock_path))
        .map_err(|_| KillSwitchStateError::FenceFailed)?;
    fs2::FileExt::try_lock_exclusive(&file).map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            KillSwitchStateError::FencePending
        } else {
            KillSwitchStateError::FenceFailed
        }
    })?;
    Ok(KillSwitchFence {
        _file: file,
        workspace: canonical,
    })
}

fn backend_fence(
    backend: &dyn StorageBackend,
) -> Result<Option<KillSwitchFence>, KillSwitchStateError> {
    let path = backend
        .query_scalar("SELECT file FROM pragma_database_list WHERE name = 'main'")
        .map_err(|_| KillSwitchStateError::FenceFailed)?
        .ok_or(KillSwitchStateError::FenceFailed)?;
    // Private in-memory databases have no cross-process owners. Production
    // StorageHandle injectors require a real path and cannot take this branch.
    if path.is_empty() {
        return Ok(None);
    }
    acquire_kill_switch_fence(Path::new(&path)).map(Some)
}

/// Refresh watermark bound to one injector's workspace. Equal-revision changed
/// content, revision rollback, and disappearance after observation fail closed.
#[derive(Default)]
pub(crate) struct KillSwitchFreshness {
    observed: Option<PersistedKillSwitch>,
    workspace: Option<PathBuf>,
}

impl KillSwitchFreshness {
    pub(crate) fn bind(&mut self, fence: &KillSwitchFence) -> Result<(), KillSwitchStateError> {
        if self
            .workspace
            .as_ref()
            .is_some_and(|path| path != &fence.workspace)
        {
            return Err(KillSwitchStateError::FenceFailed);
        }
        self.workspace = Some(fence.workspace.clone());
        Ok(())
    }

    pub(crate) fn apply(
        &mut self,
        engine: &mut PolicyEngine,
        loaded: Result<Option<String>, KillSwitchStateError>,
        now_ms: u64,
    ) -> KillSwitchRestore {
        let checked = loaded.and_then(|raw| {
            match raw.as_deref() {
                Some(value) => {
                    let document = decode_kill_switch_document(value)?;
                    if self.observed.as_ref().is_some_and(|previous| {
                        document.revision < previous.revision
                            || (document.revision == previous.revision && document != *previous)
                    }) {
                        return Err(KillSwitchStateError::Corrupt(
                            "persisted revision regressed or changed without advancement".into(),
                        ));
                    }
                    self.observed = Some(document);
                }
                None if self.observed.is_some() => {
                    return Err(KillSwitchStateError::Corrupt(
                        "previously observed switch disappeared".into(),
                    ));
                }
                None => {}
            }
            Ok(raw)
        });
        apply_persisted_kill_switch(engine, checked, now_ms)
    }
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
    let state = crate::storage_backend_helpers::get_config_kv(backend, KILL_SWITCH_STATE_KEY)
        .map_err(|e| KillSwitchStateError::LoadFailed(e.to_string()))?;
    let anchor = crate::storage_backend_helpers::get_config_kv(backend, KILL_SWITCH_REVISION_KEY)
        .map_err(|e| KillSwitchStateError::LoadFailed(e.to_string()))?;
    validate_revision_anchor(state, anchor)
}

fn validate_revision_anchor(
    state: Option<String>,
    anchor: Option<String>,
) -> Result<Option<String>, KillSwitchStateError> {
    let revision = state
        .as_deref()
        .map(decode_kill_switch_document)
        .transpose()?
        .map(|document| document.revision);
    let authority = anchor
        .as_deref()
        .map(str::parse::<u64>)
        .transpose()
        .map_err(|_| {
            KillSwitchStateError::Corrupt("revision authority is not an unsigned integer".into())
        })?;
    if !matches!((revision, authority), (None | Some(0), None)) && revision != authority {
        return Err(KillSwitchStateError::Corrupt(
            "state and revision authority disagree".into(),
        ));
    }
    Ok(state)
}

pub(crate) async fn load_kill_switch_state_from_storage_with_cx(
    cx: &crate::cx::Cx,
    storage: &crate::storage::StorageHandle,
) -> Result<Option<String>, KillSwitchStateError> {
    let state = storage
        .get_config_value_with_cx(cx, KILL_SWITCH_STATE_KEY)
        .await
        .map_err(|_| KillSwitchStateError::LoadFailed("pre-effect switch read failed".into()))?;
    let anchor = storage
        .get_config_value_with_cx(cx, KILL_SWITCH_REVISION_KEY)
        .await
        .map_err(|_| KillSwitchStateError::LoadFailed("pre-effect revision read failed".into()))?;
    validate_revision_anchor(state, anchor)
}

/// The detached workflow adapter must release its injector mutex before any
/// awaited effect. Its admission phase therefore uses a bounded, read-only
/// connection with no SQLite busy wait, under the already acquired fence.
pub(crate) fn load_kill_switch_state_from_path_with_cx(
    cx: &crate::cx::Cx,
    path: &str,
) -> Result<Option<String>, KillSwitchStateError> {
    use crate::storage_backend_trait::{OpenConfig, RusqliteBackend};
    cx.checkpoint().map_err(|_| {
        KillSwitchStateError::LoadFailed("cancelled before switch admission".into())
    })?;
    let backend = RusqliteBackend::open(
        path,
        &OpenConfig {
            read_only: true,
            wal_mode: false,
            ..Default::default()
        },
    )
    .map_err(|_| KillSwitchStateError::LoadFailed("pre-effect database open failed".into()))?;
    backend
        .set_busy_timeout(std::time::Duration::ZERO)
        .map_err(|_| {
            KillSwitchStateError::LoadFailed("pre-effect read timeout setup failed".into())
        })?;
    let loaded = load_kill_switch_state(&backend)?;
    cx.checkpoint().map_err(|_| {
        KillSwitchStateError::LoadFailed("cancelled during switch admission".into())
    })?;
    Ok(loaded)
}

fn write_revisioned_state(
    backend: &dyn StorageBackend,
    ks: &KillSwitch,
    now_ms: i64,
) -> Result<u64, KillSwitchStateError> {
    use crate::storage_backend_trait::{BackendError, ToSqlValue};

    let mut revision = 0;
    backend
        .with_transaction_dyn(&mut |tx| {
            let previous =
                tx.query_scalar("SELECT value FROM config WHERE key = 'policy.kill_switch_v1'")?;
            let anchor = tx.query_scalar(
                "SELECT value FROM config WHERE key = 'policy.kill_switch_revision_v1'",
            )?;
            let anchor = anchor
                .as_deref()
                .map(str::parse::<u64>)
                .transpose()
                .map_err(|_| {
                    BackendError::Other("corrupt kill-switch revision authority".into())
                })?;
            let prior_revision = match previous
                .as_deref()
                .map(decode_kill_switch_document)
                .transpose()
            {
                Ok(document) => document
                    .map_or(0, |document| document.revision)
                    .max(anchor.unwrap_or(0)),
                Err(_) => anchor.ok_or_else(|| {
                    BackendError::Other("corrupt switch has no trusted revision authority".into())
                })?,
            };
            revision = prior_revision
                .checked_add(1)
                .ok_or_else(|| BackendError::Other("kill-switch revision exhausted".into()))?;
            let mut document = PersistedKillSwitch::from(ks);
            document.revision = revision;
            let json = serde_json::to_string(&document)
                .map_err(|_| BackendError::Other("kill-switch encoding failed".into()))?;
            tx.query_row_cells(
                "INSERT INTO config (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at
             RETURNING value",
                &[
                    ToSqlValue::Text(KILL_SWITCH_STATE_KEY),
                    ToSqlValue::Text(&json),
                    ToSqlValue::Integer(now_ms),
                ],
            )?;
            let anchor = revision.to_string();
            tx.query_row_cells(
                "INSERT INTO config (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at
             RETURNING value",
                &[
                    ToSqlValue::Text(KILL_SWITCH_REVISION_KEY),
                    ToSqlValue::Text(&anchor),
                    ToSqlValue::Integer(now_ms),
                ],
            )?;
            Ok(())
        })
        .map_err(|_| {
            KillSwitchStateError::SaveFailed("atomic kill-switch revision write failed".into())
        })?;
    Ok(revision)
}

/// Operator transitions reload under the same authority used by dispatch.
/// `by` is an audit label; the calling operator surface owns authentication.
pub enum KillSwitchTransition<'a> {
    Trip {
        level: KillSwitchLevel,
        by: &'a str,
        reason: &'a str,
    },
    Reset {
        by: &'a str,
    },
}

/// The durable transition receipt deliberately names the integrated owner.
/// It does not assert settlement or cancellation of pre-admitted remote work.
#[derive(Debug, Serialize)]
pub struct KillSwitchTransitionReceipt {
    pub state: KillSwitch,
    pub revision: u64,
    pub fenced_owner: &'static str,
    pub pre_admitted_remote_effects: &'static str,
}

pub fn transition_kill_switch_from_backend(
    backend: &dyn StorageBackend,
    transition: KillSwitchTransition<'_>,
    now_ms: u64,
) -> Result<KillSwitchTransitionReceipt, KillSwitchStateError> {
    let _fence = backend_fence(backend)?;
    let loaded = crate::storage_backend_helpers::get_config_kv(backend, KILL_SWITCH_STATE_KEY)
        .map_err(|_| KillSwitchStateError::LoadFailed("operator switch read failed".into()))?;
    let decoded = loaded.as_deref().map(decode_kill_switch_state).transpose();
    let mut state = match decoded {
        Ok(state) => state.unwrap_or_else(KillSwitch::disarmed),
        // Only the explicit operator reset can repair a corrupt state. The
        // atomic writer still requires a trusted, monotone revision anchor.
        Err(_) if matches!(transition, KillSwitchTransition::Reset { .. }) => {
            KillSwitch::disarmed()
        }
        Err(error) => return Err(error),
    };
    state.tick(now_ms);
    match transition {
        KillSwitchTransition::Trip { level, by, reason } => {
            if level <= state.level {
                return Err(KillSwitchStateError::SaveFailed(
                    "trip must raise the current persisted tier".into(),
                ));
            }
            state.trip(level, by, reason, now_ms);
        }
        KillSwitchTransition::Reset { by } => state.reset(by, now_ms),
    }
    let timestamp = i64::try_from(now_ms).map_err(|_| {
        KillSwitchStateError::SaveFailed("transition timestamp out of range".into())
    })?;
    let revision = write_revisioned_state(backend, &state, timestamp)?;
    Ok(KillSwitchTransitionReceipt {
        state,
        revision,
        fenced_owner: "policy_gated_injector",
        pre_admitted_remote_effects: "not_proven_settled",
    })
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
    let loaded = load_kill_switch_state_from_storage_with_cx(cx, storage).await;
    apply_persisted_kill_switch(engine, loaded, now_ms)
}

/// Persist through the writer-loop [`crate::storage::StorageHandle`]
/// (Cx-first: the write is checkpointed against `cx`).
pub async fn initialize_kill_switch_to_storage_with_cx(
    cx: &crate::cx::Cx,
    storage: &crate::storage::StorageHandle,
    ks: &KillSwitch,
) -> Result<(), KillSwitchStateError> {
    cx.checkpoint().map_err(|_| {
        KillSwitchStateError::SaveFailed("cancelled before persistence admission".into())
    })?;
    let _fence = acquire_kill_switch_fence(Path::new(storage.db_path()))?;
    let loaded = storage
        .get_config_value_with_cx(cx, KILL_SWITCH_STATE_KEY)
        .await
        .map_err(|_| KillSwitchStateError::LoadFailed("kill-switch revision read failed".into()))?;
    let anchor = storage
        .get_config_value_with_cx(cx, KILL_SWITCH_REVISION_KEY)
        .await
        .map_err(|_| {
            KillSwitchStateError::LoadFailed("kill-switch revision authority read failed".into())
        })?;
    if loaded.is_some() || anchor.is_some() {
        return Err(KillSwitchStateError::SaveFailed(
            "kill switch already initialized; use an operator transition".into(),
        ));
    }
    let mut document = PersistedKillSwitch::from(ks);
    document.revision = 1;
    let json = serde_json::to_string(&document)
        .map_err(|_| KillSwitchStateError::SaveFailed("kill-switch encoding failed".into()))?;
    storage
        .set_config_value_with_cx(cx, KILL_SWITCH_REVISION_KEY, "1")
        .await
        .map_err(|_| {
            KillSwitchStateError::SaveFailed("kill-switch revision initialization failed".into())
        })?;
    storage
        .set_config_value_with_cx(cx, KILL_SWITCH_STATE_KEY, &json)
        .await
        .map_err(|_| KillSwitchStateError::SaveFailed("kill-switch persistence failed".into()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn armed(level: KillSwitchLevel) -> KillSwitch {
        let mut ks = KillSwitch::disarmed();
        ks.trip(level, "operator", "incident 42", 1_000);
        ks
    }

    /// Runs the real transition API in a separately owned test process. The
    /// output contains only state/control receipts, never captured pane text.
    pub(crate) fn transition_in_test_process(path: &Path, operation: &str) -> String {
        use std::io::Read;
        fn drain(pipe: impl Read) -> std::io::Result<Vec<u8>> {
            let mut bytes = Vec::new();
            pipe.take(65_537).read_to_end(&mut bytes)?;
            Ok(bytes)
        }
        let mut child =
            std::process::Command::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--exact",
                    "policy_kill_switch_state::tests::kill_switch_fence_subprocess",
                    "--ignored",
                    "--nocapture",
                ])
                .env("FT_KILL_SWITCH_TEST_DB", path)
                .env("FT_KILL_SWITCH_TEST_OPERATION", operation)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("owned transition subprocess");
        let stdout_pipe = child.stdout.take().unwrap();
        let stderr_pipe = child.stderr.take().unwrap();
        let stdout_reader = std::thread::spawn(move || drain(stdout_pipe));
        let stderr_reader = std::thread::spawn(move || drain(stderr_pipe));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if std::time::Instant::now() >= deadline => {
                    timed_out = true;
                    if let Err(error) = child.kill() {
                        eprintln!("owned child termination returned: {error}");
                    }
                    break child.wait().expect("reap owned child after deadline");
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(error) => {
                    eprintln!("owned child polling failed: {error}");
                    let termination = child.kill();
                    let reaping = child.wait();
                    panic!(
                        "owned child polling failed: {error}; termination={termination:?}; reaping={reaping:?}"
                    );
                }
            }
        };
        let stdout = stdout_reader
            .join()
            .expect("stdout reader joined")
            .expect("stdout drained");
        let stderr = stderr_reader
            .join()
            .expect("stderr reader joined")
            .expect("stderr drained");
        assert!(
            !timed_out && status.success(),
            "child failed or timed out: stdout={} stderr={}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        assert!(
            stdout.len() <= 65_536 && stderr.len() <= 65_536,
            "owned child exceeded output cap"
        );
        assert!(
            stderr.is_empty(),
            "unexpected child stderr: {}",
            String::from_utf8_lossy(&stderr)
        );
        let stdout = String::from_utf8(stdout).unwrap();
        assert!(
            stdout.contains("1 passed"),
            "child test did not execute: {stdout}"
        );
        println!(
            "owned transition workspace={} operation={operation}\n{stdout}",
            path.display()
        );
        stdout
    }

    #[test]
    #[ignore = "owned subprocess fixture; invoked by the fence and injector regressions"]
    fn kill_switch_fence_subprocess() {
        let path =
            std::env::var("FT_KILL_SWITCH_TEST_DB").expect("fixture requires an owned database");
        let operation = std::env::var("FT_KILL_SWITCH_TEST_OPERATION").expect("fixture operation");
        let backend = crate::storage_backend_trait::RusqliteBackend::open(
            &path,
            &crate::storage_backend_trait::OpenConfig {
                wal_mode: false,
                ..Default::default()
            },
        )
        .unwrap();
        let transition = if operation == "reset" {
            KillSwitchTransition::Reset {
                by: "isolated_test_operator",
            }
        } else {
            KillSwitchTransition::Trip {
                level: KillSwitchLevel::HardStop,
                by: "isolated_test_operator",
                reason: "cross-process trip",
            }
        };
        let result = transition_kill_switch_from_backend(&backend, transition, 5000);
        if operation == "pending" {
            assert!(
                matches!(result, Err(KillSwitchStateError::FencePending)),
                "{result:?}"
            );
            println!("KILL_SWITCH_CHILD_PENDING pid={}", std::process::id());
        } else {
            let receipt = result.expect("durable transition");
            println!(
                "KILL_SWITCH_CHILD_RECEIPT pid={} {}",
                std::process::id(),
                serde_json::to_string(&receipt).unwrap()
            );
        }
    }

    #[test]
    fn kill_switch_fence_serializes_separate_process_trip_and_reset() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("policy.db");
        let backend =
            crate::storage_backend_trait::RusqliteBackend::open_path(&path, &Default::default())
                .unwrap();
        backend.execute_batch("CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL)").unwrap();
        let fence = acquire_kill_switch_fence(&path).unwrap();
        let pending = transition_in_test_process(&path, "pending");
        assert!(pending.contains("KILL_SWITCH_CHILD_PENDING"));
        assert_eq!(
            load_kill_switch_state(&backend).unwrap(),
            None,
            "pending transition must not claim or persist application"
        );
        drop(fence);
        let applied = transition_in_test_process(&path, "trip");
        assert!(applied.contains("\"revision\":1"));
        let first = load_kill_switch_state(&backend).unwrap().unwrap();
        assert_eq!(
            decode_kill_switch_state(&first).unwrap().level,
            KillSwitchLevel::HardStop
        );
        let reset = transition_in_test_process(&path, "reset");
        assert!(reset.contains("\"revision\":2"));
        assert_eq!(
            decode_kill_switch_state(&load_kill_switch_state(&backend).unwrap().unwrap())
                .unwrap()
                .level,
            KillSwitchLevel::Disarmed
        );
    }

    #[test]
    fn kill_switch_fence_reset_repairs_corrupt_state_without_reusing_revision() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("policy.db");
        let backend =
            crate::storage_backend_trait::RusqliteBackend::open_path(&path, &Default::default())
                .unwrap();
        backend.execute_batch("CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL)").unwrap();
        transition_in_test_process(&path, "trip");
        let mut watermark = KillSwitchFreshness::default();
        let mut engine = PolicyEngine::permissive();
        watermark.apply(&mut engine, load_kill_switch_state(&backend), 6000);
        crate::storage_backend_helpers::set_config_kv(
            &backend,
            KILL_SWITCH_STATE_KEY,
            "{broken",
            7000,
        )
        .unwrap();
        assert!(matches!(
            watermark.apply(&mut engine, load_kill_switch_state(&backend), 7001),
            KillSwitchRestore::FailedClosed { .. }
        ));
        let reset = transition_in_test_process(&path, "reset");
        assert!(reset.contains("\"revision\":2"));
        watermark.apply(&mut engine, load_kill_switch_state(&backend), 8000);
        assert_eq!(engine.kill_switch_state().level, KillSwitchLevel::Disarmed);
        crate::storage_backend_helpers::set_config_kv(
            &backend,
            KILL_SWITCH_REVISION_KEY,
            "not-a-revision",
            9000,
        )
        .unwrap();
        let before =
            crate::storage_backend_helpers::get_config_kv(&backend, KILL_SWITCH_STATE_KEY).unwrap();
        assert!(
            transition_kill_switch_from_backend(
                &backend,
                KillSwitchTransition::Reset { by: "operator" },
                10000
            )
            .is_err()
        );
        assert_eq!(
            crate::storage_backend_helpers::get_config_kv(&backend, KILL_SWITCH_STATE_KEY).unwrap(),
            before,
            "invalid revision authority cannot be papered over by reset"
        );
    }

    #[cfg(unix)]
    #[test]
    fn kill_switch_fence_aliases_share_authority_and_hardlinks_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("policy.db");
        let _backend =
            crate::storage_backend_trait::RusqliteBackend::open_path(&path, &Default::default())
                .unwrap();
        let alias = directory.path().join("alias.db");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        let guard = acquire_kill_switch_fence(&path).unwrap();
        assert!(matches!(
            acquire_kill_switch_fence(&alias),
            Err(KillSwitchStateError::FencePending)
        ));
        drop(guard);
        let guard = acquire_kill_switch_fence(&alias).unwrap();
        let mut freshness = KillSwitchFreshness::default();
        freshness.bind(&guard).unwrap();
        drop(guard);
        freshness
            .bind(&acquire_kill_switch_fence(&path).unwrap())
            .unwrap();
        let other = directory.path().join("other.db");
        let _other_backend =
            crate::storage_backend_trait::RusqliteBackend::open_path(&other, &Default::default())
                .unwrap();
        assert!(
            freshness
                .bind(&acquire_kill_switch_fence(&other).unwrap())
                .is_err()
        );
        std::fs::hard_link(&path, directory.path().join("hardlink.db")).unwrap();
        assert!(matches!(
            acquire_kill_switch_fence(&path),
            Err(KillSwitchStateError::FenceFailed)
        ));
    }

    #[test]
    fn kill_switch_freshness_rejects_rollback_rewrite_disappearance_and_read_failure() {
        let mut newer = PersistedKillSwitch::from(&armed(KillSwitchLevel::HardStop));
        newer.revision = 2;
        let newer_raw = serde_json::to_string(&newer).unwrap();
        let mut stale = PersistedKillSwitch::from(&KillSwitch::disarmed());
        stale.revision = 1;
        let stale_raw = serde_json::to_string(&stale).unwrap();
        stale.revision = 2;
        let rewrite = serde_json::to_string(&stale).unwrap();
        for invalid in [
            Ok(Some(stale_raw)),
            Ok(Some(rewrite)),
            Ok(None),
            Ok(Some("{broken".into())),
            Err(KillSwitchStateError::LoadFailed("unreadable".into())),
        ] {
            let mut engine = PolicyEngine::permissive();
            let mut freshness = KillSwitchFreshness::default();
            freshness.apply(&mut engine, Ok(Some(newer_raw.clone())), 6000);
            assert!(matches!(
                freshness.apply(&mut engine, invalid, 7000),
                KillSwitchRestore::FailedClosed { .. }
            ));
            for action in [
                crate::policy::ActionKind::SendText,
                crate::policy::ActionKind::ExecCommand,
                crate::policy::ActionKind::WriteFile,
                crate::policy::ActionKind::ConnectorInvoke,
                crate::policy::ActionKind::WorkflowRun,
            ] {
                let decision = engine.authorize(&crate::policy::PolicyInput::new(
                    action,
                    crate::policy::ActorKind::Robot,
                ));
                assert_eq!(decision.rule_id(), Some("policy.kill_switch"));
                assert!(decision.is_denied());
            }
            // Only a newer valid operator transition can recover the engine.
            stale.revision = 3;
            freshness.apply(
                &mut engine,
                Ok(Some(serde_json::to_string(&stale).unwrap())),
                8000,
            );
            assert_eq!(engine.kill_switch_state().level, KillSwitchLevel::Disarmed);
        }
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
        transition_kill_switch_from_backend(
            &backend,
            KillSwitchTransition::Trip {
                level: KillSwitchLevel::HardStop,
                by: "operator",
                reason: "drill",
            },
            1_000,
        )
        .expect("persist trip");

        let mut second = PolicyEngine::permissive();
        let outcome = restore_kill_switch_from_backend(&mut second, &backend, 2_000);
        assert!(
            matches!(outcome, KillSwitchRestore::Restored { .. }),
            "{outcome:?}"
        );
        assert_eq!(second.kill_switch_state().level, KillSwitchLevel::HardStop);
        assert_eq!(second.kill_switch_state().reason, "drill");

        transition_kill_switch_from_backend(
            &backend,
            KillSwitchTransition::Reset { by: "operator" },
            3_000,
        )
        .expect("persist reset");
        let mut third = PolicyEngine::permissive();
        restore_kill_switch_from_backend(&mut third, &backend, 4_000);
        assert_eq!(third.kill_switch_state().level, KillSwitchLevel::Disarmed);
    }
}
