//! Transaction execution engine (ft-1i2ge.8).
//!
//! Orchestrates the full tx lifecycle: plan → prepare → commit → compensate,
//! tying together the plan compiler, idempotency ledger, and observability pipeline.
//!
//! # Architecture
//!
//! ```text
//! TxPlan ──────┐
//!              ├──> TxExecutionEngine::execute() ──> TxExecutionResult
//! StepExecutor ┤                                     ├─ ledger
//!              │                                     ├─ events
//! Config ──────┘                                     └─ forensic bundle
//! ```
//!
//! Safety doctrine: no commit before prepare; no prepare bypass of policy gates;
//! every transition emits observability events with reason codes.

use crate::plan::{
    Mission, MissionEconomicAuditRow, MissionEconomicBreakerDecision,
    MissionEconomicHardStopEnvelope, MissionKillSwitchLevel, MissionTxContract, MissionTxState,
    StepAction, TxCommitOutcome, TxCommitReport, TxCommitStepInput, TxCompensationReport,
    TxCompensationStepInput, TxOutcome, TxPrepareApprovalChecker, TxPrepareEvaluationContext,
    TxPrepareGateInput, TxPrepareOutcome, TxPreparePolicyAuthorizer, TxPrepareReport,
    TxPrepareTargetLookup, evaluate_prepare_phase, execute_commit_phase,
    execute_compensation_phase, mission_tx_rollback_commit_report,
};
use crate::runtime_async::CompatRuntime;
use crate::tx_idempotency::{
    DurableKeyLeaseSet, IdempotencyError, IdempotencyKey, IdempotencyPolicy, IdempotencyStore,
    ResumeRecommendation, StepOutcome, TxExecutionLedger, TxPhase,
};
use crate::tx_observability::{
    TxEventKind, TxForensicBundle, TxObservabilityConfig, TxObservabilityEvent,
    TxObservabilityPhase,
};
use crate::tx_plan_compiler::StepRisk;
#[cfg(unix)]
use cap_fs_ext::OpenOptionsSyncExt;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
#[cfg(unix)]
use cap_std::fs::OpenOptionsExt as CapOpenOptionsExt;
use cap_std::fs::{
    Dir as CapDir, File as CapFile, Metadata as CapMetadata, MetadataExt as CapMetadataExt,
    OpenOptions as CapOpenOptions,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt as StdMetadataExt;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt as StdMetadataExt;

// ── Configuration ────────────────────────────────────────────────────────────

/// Configuration for the tx execution engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxExecutionConfig {
    /// Whether to auto-trigger compensation on partial failure.
    pub auto_compensate: bool,
    /// Whether to produce a forensic bundle after execution.
    pub produce_forensic_bundle: bool,
    /// Maximum number of steps to execute before pausing for safety.
    pub max_steps_per_batch: usize,
    /// Kill switch level for the entire execution.
    pub kill_switch: MissionKillSwitchLevel,
    /// Whether execution is paused (commit phase suspended).
    pub paused: bool,
    /// Optional step ID to inject a failure at (for testing/chaos).
    pub fail_step: Option<String>,
    /// Optional step ID to inject a compensation failure at (for testing/chaos).
    pub fail_compensation_for_step: Option<String>,
    /// Observability configuration.
    pub observability: TxObservabilityConfig,
}

impl Default for TxExecutionConfig {
    fn default() -> Self {
        Self {
            auto_compensate: true,
            produce_forensic_bundle: true,
            max_steps_per_batch: 1000,
            kill_switch: MissionKillSwitchLevel::Off,
            paused: false,
            fail_step: None,
            fail_compensation_for_step: None,
            observability: TxObservabilityConfig::default(),
        }
    }
}

// ── Durable contract boundary ──────────────────────────────────────────────────────────────────────────

/// Maximum serialized size accepted by the mission transaction loaders.
pub const TX_CONTRACT_MAX_BYTES: usize = 16 * 1024 * 1024;

static TX_CONTRACT_LOCKS: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
static TX_EXECUTION_NONCE: AtomicU64 = AtomicU64::new(0);
static TX_CONTRACT_TEMP_NONCE: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
std::thread_local! {
    static ROLLBACK_POST_PROOF_LEASE_TEST_HOOK:
        std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_rollback_post_proof_lease_test_hook(hook: Option<Box<dyn FnOnce()>>) {
    ROLLBACK_POST_PROOF_LEASE_TEST_HOOK.with(|slot| {
        *slot.borrow_mut() = hook;
    });
}

#[cfg(test)]
fn run_rollback_post_proof_lease_test_hook() {
    let hook = ROLLBACK_POST_PROOF_LEASE_TEST_HOOK.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

/// Stable classification for transaction contract lock and persistence errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxContractStoreErrorKind {
    InProgress,
    /// The namespace observation changed before lock ownership or effects.
    Conflict,
    Lock,
    Validation,
    Serialization,
    TooLarge,
    Write,
    Sync,
    Rename,
}

/// Failure returned by the shared transaction contract durability boundary.
#[derive(Debug, Clone)]
pub struct TxContractStoreError {
    kind: TxContractStoreErrorKind,
    message: String,
    recovery_path: Option<PathBuf>,
    published: bool,
}

impl TxContractStoreError {
    fn new(kind: TxContractStoreErrorKind, message: String) -> Self {
        Self {
            kind,
            message,
            recovery_path: None,
            published: false,
        }
    }

    fn with_recovery_path(mut self, recovery_path: PathBuf) -> Self {
        self.recovery_path = Some(recovery_path);
        self
    }

    fn after_publication(mut self) -> Self {
        self.published = true;
        self
    }

    /// Whether replacement occurred before the error. Never retry such a
    /// mutation without reading and reconciling its authoritative state.
    #[must_use]
    pub fn published(&self) -> bool {
        self.published
    }

    #[must_use]
    pub fn kind(&self) -> TxContractStoreErrorKind {
        self.kind
    }

    /// Path retaining the attempted post-execution snapshot.
    ///
    /// A write failure may leave a partial snapshot. Callers must validate the
    /// retained bytes before using them for recovery.
    #[must_use]
    pub fn recovery_path(&self) -> Option<&Path> {
        self.recovery_path.as_deref()
    }
}

impl std::fmt::Display for TxContractStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)?;
        if let Some(path) = &self.recovery_path {
            write!(
                f,
                "; last-known recovery artifact path {} may not resolve if its pinned parent was renamed",
                path.display()
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for TxContractStoreError {}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FsObjectIdentity {
    device: u64,
    inode: u64,
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FsObjectIdentity {
    volume: u32,
    index: u64,
}

#[cfg(not(any(unix, windows)))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FsObjectIdentity;

#[cfg(unix)]
fn cap_object_identity(metadata: &CapMetadata) -> Result<FsObjectIdentity, TxContractStoreError> {
    Ok(FsObjectIdentity {
        device: CapMetadataExt::dev(metadata),
        inode: CapMetadataExt::ino(metadata),
    })
}

#[cfg(windows)]
fn cap_object_identity(metadata: &CapMetadata) -> Result<FsObjectIdentity, TxContractStoreError> {
    let volume = CapMetadataExt::volume_serial_number(metadata).ok_or_else(|| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            "filesystem did not expose a volume identity for a pinned transaction object"
                .to_string(),
        )
    })?;
    let index = CapMetadataExt::file_index(metadata).ok_or_else(|| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            "filesystem did not expose a file identity for a pinned transaction object".to_string(),
        )
    })?;
    Ok(FsObjectIdentity { volume, index })
}

#[cfg(not(any(unix, windows)))]
fn cap_object_identity(_metadata: &CapMetadata) -> Result<FsObjectIdentity, TxContractStoreError> {
    Err(TxContractStoreError::new(
        TxContractStoreErrorKind::Lock,
        "pinned transaction object identity is unsupported on this platform".to_string(),
    ))
}

#[cfg(unix)]
fn std_object_identity(
    metadata: &std::fs::Metadata,
) -> Result<FsObjectIdentity, TxContractStoreError> {
    Ok(FsObjectIdentity {
        device: StdMetadataExt::dev(metadata),
        inode: StdMetadataExt::ino(metadata),
    })
}

#[cfg(windows)]
fn std_object_identity(
    metadata: &std::fs::Metadata,
) -> Result<FsObjectIdentity, TxContractStoreError> {
    let volume = StdMetadataExt::volume_serial_number(metadata).ok_or_else(|| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            "filesystem did not expose a volume identity for the transaction namespace".to_string(),
        )
    })?;
    let index = StdMetadataExt::file_index(metadata).ok_or_else(|| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            "filesystem did not expose a file identity for the transaction namespace".to_string(),
        )
    })?;
    Ok(FsObjectIdentity { volume, index })
}

#[cfg(not(any(unix, windows)))]
fn std_object_identity(
    _metadata: &std::fs::Metadata,
) -> Result<FsObjectIdentity, TxContractStoreError> {
    Err(TxContractStoreError::new(
        TxContractStoreErrorKind::Lock,
        "transaction namespace identity is unsupported on this platform".to_string(),
    ))
}

fn require_single_link(
    metadata: &CapMetadata,
    display_path: &Path,
    object_kind: &str,
) -> Result<(), TxContractStoreError> {
    #[cfg(unix)]
    let link_count = Some(CapMetadataExt::nlink(metadata));
    #[cfg(windows)]
    let link_count = CapMetadataExt::number_of_links(metadata).map(u64::from);
    #[cfg(not(any(unix, windows)))]
    let link_count: Option<u64> = None;

    match link_count {
        Some(1) => Ok(()),
        Some(count) => Err(TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "transaction {object_kind} {} has {count} hard links; exactly one namespace link is required",
                display_path.display()
            ),
        )),
        None => Err(TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "filesystem did not expose the hard-link count for transaction {object_kind} {}",
                display_path.display()
            ),
        )),
    }
}

/// Guard holding the pinned contract object, parent-directory capability, and
/// both the process-local and OS-backed sidecar locks.
///
/// Contract-parent and contract-basename races are contained by capability
/// handles, so mutating I/O cannot be redirected through an ambient path.
/// The workspace root and its `.ft` child are the trusted control-plane
/// anchors for the mutation. On Unix, capabilities cannot defend against a
/// same-UID actor replacing those trust anchors themselves; callers therefore
/// fail closed when either anchor's namespace identity changes before effects.
pub struct TxContractLockGuard {
    key: PathBuf,
    workspace_display: PathBuf,
    workspace_identity: FsObjectIdentity,
    workspace_dir: CapDir,
    control_display: PathBuf,
    control_identity: FsObjectIdentity,
    control_dir: CapDir,
    lock_dir: CapDir,
    lock_dir_identity: FsObjectIdentity,
    parent_relative: PathBuf,
    parent_display: PathBuf,
    parent_identity: FsObjectIdentity,
    parent_dir: CapDir,
    parent_sync_file: File,
    contract_name: OsString,
    contract_file: Mutex<CapFile>,
    lock_name: OsString,
    lock_identity: FsObjectIdentity,
    file: File,
}

impl std::fmt::Debug for TxContractLockGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxContractLockGuard")
            .field("key", &self.key)
            .field("workspace_display", &self.workspace_display)
            .field("control_display", &self.control_display)
            .field("parent_display", &self.parent_display)
            .field("contract_name", &self.contract_name)
            .field("lock_name", &self.lock_name)
            .finish_non_exhaustive()
    }
}

impl TxContractLockGuard {
    /// Return the exact canonical contract path protected by this guard.
    ///
    /// Mutating callers must use this path for loading, durable-ledger
    /// placement, effect dispatch, and persistence. Continuing to use a
    /// caller-supplied alias after locking would permit an intermediate
    /// symlink to be retargeted to a different contract identity.
    #[must_use]
    pub fn authoritative_path(&self) -> &Path {
        &self.key
    }

    /// Read the locked contract through the file handle pinned during lock
    /// acquisition. No ambient path lookup occurs, so renaming the parent and
    /// installing a foreign replacement cannot redirect this read.
    ///
    /// # Errors
    ///
    /// Returns `Lock` when the pinned file or sidecar identity is no longer
    /// safe, and `TooLarge` when the contract exceeds
    /// [`TX_CONTRACT_MAX_BYTES`].
    pub fn read_authoritative_contract_bytes(&self) -> Result<Vec<u8>, TxContractStoreError> {
        self.verify_pinned_mutation_edges()?;
        let file = self.contract_file.lock().map_err(|_| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                "pinned transaction contract file mutex is poisoned".to_string(),
            )
        })?;
        let metadata = file.metadata().map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to inspect pinned transaction contract {}: {err}",
                    self.key.display()
                ),
            )
        })?;
        require_single_link(&metadata, &self.key, "contract")?;
        if metadata.len() > TX_CONTRACT_MAX_BYTES as u64 {
            return Err(TxContractStoreError::new(
                TxContractStoreErrorKind::TooLarge,
                format!(
                    "transaction contract {} is {} bytes; maximum is {TX_CONTRACT_MAX_BYTES}",
                    self.key.display(),
                    metadata.len()
                ),
            ));
        }

        let mut reader = file.try_clone().map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to clone pinned transaction contract handle {}: {err}",
                    self.key.display()
                ),
            )
        })?;
        reader.seek(SeekFrom::Start(0)).map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to seek pinned transaction contract {}: {err}",
                    self.key.display()
                ),
            )
        })?;
        let mut bytes = Vec::with_capacity((metadata.len() as usize).min(TX_CONTRACT_MAX_BYTES));
        reader
            .take(TX_CONTRACT_MAX_BYTES.saturating_add(1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|err| {
                TxContractStoreError::new(
                    TxContractStoreErrorKind::Lock,
                    format!(
                        "failed to read pinned transaction contract {}: {err}",
                        self.key.display()
                    ),
                )
            })?;
        if bytes.len() > TX_CONTRACT_MAX_BYTES {
            return Err(TxContractStoreError::new(
                TxContractStoreErrorKind::TooLarge,
                format!(
                    "transaction contract {} exceeds the {TX_CONTRACT_MAX_BYTES}-byte maximum",
                    self.key.display()
                ),
            ));
        }
        Ok(bytes)
    }

    /// Open the workspace-global durable idempotency store through the pinned
    /// `.ft` control-directory capability.
    ///
    /// The display path is diagnostic only. Every ledger operation is rooted
    /// beneath the pinned control directory, so detaching or replacing the
    /// contract parent cannot redirect durable replay state.
    ///
    /// # Errors
    ///
    /// Returns `Lock` if the workspace/control trust anchors changed, or
    /// `Write` if the durable store cannot be opened or validated.
    pub fn open_idempotency_store(
        &self,
        policy: IdempotencyPolicy,
    ) -> Result<IdempotencyStore, TxContractStoreError> {
        self.verify_pinned_mutation_edges()?;
        let control_dir = self.control_dir.try_clone().map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to clone pinned transaction control directory {}: {err}",
                    self.control_display.display()
                ),
            )
        })?;
        IdempotencyStore::open_in_pinned_dir(
            control_dir,
            self.control_display.clone(),
            policy,
        )
        .map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Write,
                format!(
                    "failed to open workspace-global transaction ledger through pinned control directory {}: {err}",
                    self.control_display.display()
                ),
            )
        })
    }

    /// Verify that this guard owns the canonical lock for `path`.
    ///
    /// Call this before dispatching external effects when a guard and path are
    /// passed through separate layers. [`save_tx_contract_atomic`] repeats the
    /// check at the persistence boundary.
    ///
    /// # Errors
    ///
    /// Returns `Lock` when the logical path differs or the pinned sidecar/file
    /// safety edges are no longer intact. This method never supplies an I/O
    /// path; all mutation I/O remains capability-relative.
    pub fn authorizes(&self, path: &Path) -> Result<(), TxContractStoreError> {
        self.verify_logical_path(path)?;
        self.verify_pinned_mutation_edges()?;
        #[cfg(target_os = "macos")]
        {
            let contract = self.contract_file.lock().map_err(|_| {
                TxContractStoreError::new(
                    TxContractStoreErrorKind::Lock,
                    "pinned transaction contract file mutex is poisoned".to_string(),
                )
            })?;
            // Inode equality alone does not detect a case-only rename on a
            // case-insensitive volume. Refuse a stale pathname-derived lock
            // identity, including changes to parent or workspace spelling.
            if native_tx_descriptor_path(&*contract, &self.key)? != self.key
                || native_tx_descriptor_path(&self.workspace_dir, &self.workspace_display)?
                    != self.workspace_display
            {
                return Err(TxContractStoreError::new(
                    TxContractStoreErrorKind::Conflict,
                    "transaction contract canonical spelling changed before effect dispatch"
                        .to_string(),
                ));
            }
        }
        if !self.named_parent_matches_pinned_parent()? {
            return Err(TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "transaction contract parent namespace changed before effect dispatch: last-known path {} no longer names the pinned parent",
                    self.parent_display.display()
                ),
            ));
        }
        if !self.named_contract_entry_matches_pinned_file()? {
            return Err(TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "transaction contract basename changed before effect dispatch: {} no longer names the pinned contract file",
                    self.key.display()
                ),
            ));
        }
        Ok(())
    }

    fn verify_logical_path(&self, path: &Path) -> Result<(), TxContractStoreError> {
        if path == self.key {
            return Ok(());
        }
        Err(TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "transaction contract lock for {} cannot authorize {}",
                self.key.display(),
                path.display()
            ),
        ))
    }

    fn verify_pinned_mutation_edges(&self) -> Result<(), TxContractStoreError> {
        let workspace_metadata = std::fs::metadata(&self.workspace_display).map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to verify pinned transaction workspace {}: {err}",
                    self.workspace_display.display()
                ),
            )
        })?;
        if std_object_identity(&workspace_metadata)? != self.workspace_identity {
            return Err(TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "transaction workspace namespace changed while lock was held: {}",
                    self.workspace_display.display()
                ),
            ));
        }

        let contract = self.contract_file.lock().map_err(|_| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                "pinned transaction contract file mutex is poisoned".to_string(),
            )
        })?;
        let contract_metadata = contract.metadata().map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to inspect pinned transaction contract {}: {err}",
                    self.key.display()
                ),
            )
        })?;
        require_single_link(&contract_metadata, &self.key, "contract")?;
        drop(contract);

        let named_control = self
            .workspace_dir
            .open_dir_nofollow(Path::new(".ft"))
            .map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to re-open pinned transaction control directory {} without following symlinks: {err}",
                    self.control_display.display()
                ),
            )
        })?;
        let named_control_identity =
            cap_object_identity(&named_control.dir_metadata().map_err(|err| {
                TxContractStoreError::new(
                    TxContractStoreErrorKind::Lock,
                    format!(
                        "failed to identify current transaction control directory {}: {err}",
                        self.control_display.display()
                    ),
                )
            })?)?;
        if named_control_identity != self.control_identity {
            return Err(TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "transaction control directory namespace changed while lock was held: {}",
                    self.control_display.display()
                ),
            ));
        }

        let named_lock_dir = self
            .control_dir
            .open_dir_nofollow(Path::new("tx_contract_locks"))
            .map_err(|err| {
                TxContractStoreError::new(
                    TxContractStoreErrorKind::Lock,
                    format!(
                        "failed to re-open pinned transaction lock directory {} without following symlinks: {err}",
                        self.control_display.join("tx_contract_locks").display()
                    ),
                )
            })?;
        let named_lock_dir_identity =
            cap_object_identity(&named_lock_dir.dir_metadata().map_err(|err| {
                TxContractStoreError::new(
                    TxContractStoreErrorKind::Lock,
                    format!(
                        "failed to identify current transaction lock directory {}: {err}",
                        self.control_display.join("tx_contract_locks").display()
                    ),
                )
            })?)?;
        if named_lock_dir_identity != self.lock_dir_identity {
            return Err(TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "transaction lock directory namespace changed while lock was held: {}",
                    self.control_display.join("tx_contract_locks").display()
                ),
            ));
        }

        let lock_metadata = self
            .lock_dir
            .symlink_metadata(&self.lock_name)
            .map_err(|err| {
                TxContractStoreError::new(
                    TxContractStoreErrorKind::Lock,
                    format!(
                        "failed to inspect pinned transaction sidecar {}: {err}",
                        self.control_display
                            .join("tx_contract_locks")
                            .join(&self.lock_name)
                            .display()
                    ),
                )
            })?;
        if !lock_metadata.is_file() || cap_object_identity(&lock_metadata)? != self.lock_identity {
            return Err(TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "pinned transaction sidecar identity changed: {}",
                    self.control_display
                        .join("tx_contract_locks")
                        .join(&self.lock_name)
                        .display()
                ),
            ));
        }
        require_single_link(
            &lock_metadata,
            &self
                .control_display
                .join("tx_contract_locks")
                .join(&self.lock_name),
            "lock sidecar",
        )?;
        Ok(())
    }

    fn named_contract_entry_matches_pinned_file(&self) -> Result<bool, TxContractStoreError> {
        let pinned = self.contract_file.lock().map_err(|_| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                "pinned transaction contract file mutex is poisoned".to_string(),
            )
        })?;
        let pinned_metadata = pinned.metadata().map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to identify pinned transaction contract {}: {err}",
                    self.key.display()
                ),
            )
        })?;
        let pinned_identity = cap_object_identity(&pinned_metadata)?;
        drop(pinned);
        self.named_contract_entry_matches_identity(pinned_identity)
    }

    fn named_contract_entry_matches_identity(
        &self,
        pinned_identity: FsObjectIdentity,
    ) -> Result<bool, TxContractStoreError> {
        let mut options = CapOpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        options.nonblock(true);
        let named_file = match self.parent_dir.open_with(&self.contract_name, &options) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => {
                return Err(TxContractStoreError::new(
                    TxContractStoreErrorKind::Lock,
                    format!(
                        "failed to re-open transaction contract basename {} without following symlinks: {err}",
                        self.key.display()
                    ),
                ));
            }
        };
        let named_metadata = named_file.metadata().map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to identify transaction contract basename {}: {err}",
                    self.key.display()
                ),
            )
        })?;
        require_single_link(&named_metadata, &self.key, "contract")?;
        Ok(cap_object_identity(&named_metadata)? == pinned_identity)
    }

    fn pinned_contract_metadata(&self) -> Result<CapMetadata, TxContractStoreError> {
        let file = self.contract_file.lock().map_err(|_| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                "pinned transaction contract file mutex is poisoned".to_string(),
            )
        })?;
        file.metadata().map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Write,
                format!(
                    "failed to inspect pinned transaction contract {} before save: {err}",
                    self.key.display()
                ),
            )
        })
    }

    fn sync_parent(&self) -> std::io::Result<()> {
        self.parent_sync_file.sync_all()
    }

    fn named_parent_matches_pinned_parent(&self) -> Result<bool, TxContractStoreError> {
        let mut current = self.workspace_dir.try_clone().map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!("failed to clone pinned workspace capability: {err}"),
            )
        })?;
        for component in self.parent_relative.components() {
            let std::path::Component::Normal(part) = component else {
                continue;
            };
            current = match current.open_dir_nofollow(part) {
                Ok(dir) => dir,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(err) => {
                    return Err(TxContractStoreError::new(
                        TxContractStoreErrorKind::Lock,
                        format!(
                            "failed to revalidate transaction contract parent {} without following symlinks: {err}",
                            self.parent_display.display()
                        ),
                    ));
                }
            };
        }
        let current_identity = cap_object_identity(&current.dir_metadata().map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to identify current transaction contract parent {}: {err}",
                    self.parent_display.display()
                ),
            )
        })?)?;
        Ok(current_identity == self.parent_identity)
    }

    fn recovery_display_path(&self, name: &Path) -> PathBuf {
        self.parent_display.join(name)
    }
}

impl Drop for TxContractLockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
        if let Ok(mut locks) = TX_CONTRACT_LOCKS.lock() {
            locks.remove(&self.key);
        }
    }
}

#[cfg(not(windows))]
fn normalized_workspace_relative_contract_path(
    workspace_root: &Path,
    workspace_display: &Path,
    contract_path: &Path,
) -> Result<PathBuf, TxContractStoreError> {
    let relative = if contract_path.is_absolute() {
        contract_path
            .strip_prefix(workspace_display)
            .or_else(|_| contract_path.strip_prefix(workspace_root))
            .map_err(|_| {
                TxContractStoreError::new(
                    TxContractStoreErrorKind::Lock,
                    format!(
                        "transaction contract {} is outside workspace root {}",
                        contract_path.display(),
                        workspace_display.display()
                    ),
                )
            })?
    } else {
        contract_path
    };

    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err(TxContractStoreError::new(
                    TxContractStoreErrorKind::Lock,
                    format!(
                        "transaction contract path must be a contained normalized workspace-relative path: {}",
                        contract_path.display()
                    ),
                ));
            }
        }
    }
    if normalized.file_name().is_none() {
        return Err(TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "transaction contract path has no file name: {}",
                contract_path.display()
            ),
        ));
    }
    Ok(normalized)
}

#[cfg(not(windows))]
fn workspace_root_lock_name(relative_contract_path: &Path) -> OsString {
    let mut hasher = Sha256::new();
    let normalized = relative_contract_path.to_string_lossy();
    hasher.update(normalized.as_bytes());
    OsString::from(format!("{}.lock", hex::encode(hasher.finalize())))
}

#[cfg(not(windows))]
fn open_directory_sync_file(dir: &CapDir, display: &Path) -> Result<File, TxContractStoreError> {
    // `CapDir` may internally hold an `O_PATH` descriptor on Linux. Such a
    // descriptor is suitable for capability-relative lookup but `fsync(2)`
    // rejects it with `EBADF`. Re-open `.` read-only through the already pinned
    // capability so the returned descriptor names the same directory and is
    // synchronizable on every supported Unix target.
    let file = dir
        .open(".")
        .map(cap_std::fs::File::into_std)
        .map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to open transaction directory sync handle {}: {err}",
                    display.display()
                ),
            )
        })?;

    let std_identity = std_object_identity(&file.metadata().map_err(|err| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "failed to identify transaction directory sync handle {}: {err}",
                display.display()
            ),
        )
    })?)?;
    let cap_identity = cap_object_identity(&dir.dir_metadata().map_err(|err| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "failed to identify pinned transaction directory {}: {err}",
                display.display()
            ),
        )
    })?)?;
    if std_identity != cap_identity {
        return Err(TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "transaction directory changed while opening sync handle: {}",
                display.display()
            ),
        ));
    }
    Ok(file)
}

#[cfg(not(windows))]
fn open_workspace_relative_directory_sync_file(
    workspace_dir: &CapDir,
    relative: &Path,
    pinned_dir: &CapDir,
    display: &Path,
) -> Result<File, TxContractStoreError> {
    let _ = (workspace_dir, relative);
    open_directory_sync_file(pinned_dir, display)
}

#[cfg(not(windows))]
fn sync_pinned_directory(dir: &CapDir, display: &Path) -> Result<(), TxContractStoreError> {
    open_directory_sync_file(dir, display)?
        .sync_all()
        .map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Sync,
                format!(
                    "failed to synchronize pinned transaction directory {}: {err}",
                    display.display()
                ),
            )
        })
}

#[cfg(not(windows))]
fn open_or_create_pinned_dir(
    parent: &CapDir,
    name: &Path,
    display: &Path,
) -> Result<CapDir, TxContractStoreError> {
    match parent.open_dir_nofollow(name) {
        Ok(dir) => Ok(dir),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let mut created = false;
            match parent.create_dir(name) {
                Ok(()) => created = true,
                Err(create_err) if create_err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(create_err) => {
                    return Err(TxContractStoreError::new(
                        TxContractStoreErrorKind::Lock,
                        format!(
                            "failed to create pinned transaction directory {}: {create_err}",
                            display.display()
                        ),
                    ));
                }
            }
            let dir = parent.open_dir_nofollow(name).map_err(|open_err| {
                TxContractStoreError::new(
                    TxContractStoreErrorKind::Lock,
                    format!(
                        "failed to pin transaction directory {} without following symlinks after create: {open_err}",
                        display.display()
                    ),
                )
            })?;
            if created {
                let parent_display = display
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                sync_pinned_directory(parent, parent_display)?;
            }
            Ok(dir)
        }
        Err(err) => Err(TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "failed to pin transaction directory {} without following symlinks: {err}",
                display.display()
            ),
        )),
    }
}

#[cfg(not(windows))]
fn release_tx_contract_lock_key(key: &Path) {
    if let Ok(mut locks) = TX_CONTRACT_LOCKS.lock() {
        locks.remove(key);
    }
}

/// Acquire a transaction contract lock from a pinned workspace-root
/// capability before loading mutable state.
///
/// The returned guard must remain alive until the updated contract has been
/// durably replaced. The sidecar lock is intentionally retained on disk; file
/// locking, rather than sidecar existence, represents ownership.
///
/// # Errors
///
/// Returns [`TxContractStoreErrorKind::InProgress`] when this process or another
/// process already holds the contract lock, and `Lock` for lock-file I/O errors.
pub fn acquire_tx_contract_lock(
    workspace_root: &Path,
    path: &Path,
) -> Result<TxContractLockGuard, TxContractStoreError> {
    #[cfg(windows)]
    {
        let _ = (workspace_root, path);
        Err(TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            "transaction mutation is unavailable on Windows because durable capability-relative atomic replacement cannot yet be established with a documented directory-flush primitive; no transaction effects were dispatched"
                .to_string(),
        ))
    }
    #[cfg(not(windows))]
    {
        acquire_tx_contract_lock_supported(workspace_root, path, || {})
    }
}

#[cfg(not(windows))]
fn verify_tx_contract_canonical_leaf(
    parent: &CapDir,
    requested: &std::ffi::OsStr,
    canonical: &std::ffi::OsStr,
    display: &Path,
) -> Result<(), TxContractStoreError> {
    let observe = |name: &std::ffi::OsStr| {
        parent.symlink_metadata(name).map_err(|error| {
            TxContractStoreError::new(
                if error.kind() == std::io::ErrorKind::NotFound {
                    TxContractStoreErrorKind::Conflict
                } else {
                    TxContractStoreErrorKind::Lock
                },
                format!(
                    "cannot verify canonical transaction leaf {} before locking: {error}",
                    display.display()
                ),
            )
        })
    };
    let requested_metadata = observe(requested)?;
    let canonical_metadata = observe(canonical)?;
    if !requested_metadata.is_file() || !canonical_metadata.is_file() {
        return Err(TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "transaction contract is not a regular file: {}",
                display.display()
            ),
        ));
    }
    if cap_object_identity(&requested_metadata)? != cap_object_identity(&canonical_metadata)? {
        return Err(TxContractStoreError::new(
            TxContractStoreErrorKind::Conflict,
            format!(
                "canonical transaction leaf changed before locking: {}",
                display.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn native_tx_descriptor_path(
    file: impl std::os::fd::AsFd,
    display: &Path,
) -> Result<PathBuf, TxContractStoreError> {
    use std::os::unix::ffi::OsStringExt;

    // F_GETPATH returns the filesystem's stored spelling; realpath and the
    // capability library's manual canonicalizer retain caller case on macOS.
    rustix::fs::getpath(file)
        .map(|path| PathBuf::from(OsString::from_vec(path.into_bytes())))
        .map_err(|err| {
            TxContractStoreError::new(
                if err == rustix::io::Errno::NOENT {
                    TxContractStoreErrorKind::Conflict
                } else {
                    TxContractStoreErrorKind::Lock
                },
                format!(
                    "failed to resolve native transaction descriptor path {}: {err}",
                    display.display()
                ),
            )
        })
}

#[cfg(target_os = "macos")]
fn discover_native_tx_contract_path(
    parent: &CapDir,
    name: &std::ffi::OsStr,
    display: &Path,
) -> Result<PathBuf, TxContractStoreError> {
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let file = parent.open_with(name, &options).map_err(|err| {
        TxContractStoreError::new(
            if err.kind() == std::io::ErrorKind::NotFound {
                TxContractStoreErrorKind::Conflict
            } else {
                TxContractStoreErrorKind::Lock
            },
            format!(
                "failed to open native transaction discovery descriptor {}: {err}",
                display.display()
            ),
        )
    })?;
    let metadata = file.metadata().map_err(|err| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "failed to inspect native transaction discovery descriptor {}: {err}",
                display.display()
            ),
        )
    })?;
    if !metadata.is_file() {
        return Err(TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "transaction contract is not a regular file: {}",
                display.display()
            ),
        ));
    }
    // Discovery never supplies the mutation handle. Pin the authoritative
    // file only after acquiring both locks, so a completed competing commit
    // is observed through its replacement inode.
    native_tx_descriptor_path(&file, display)
}

#[cfg(not(windows))]
fn acquire_tx_contract_lock_supported(
    workspace_root: &Path,
    path: &Path,
    before_lock: impl FnOnce(),
) -> Result<TxContractLockGuard, TxContractStoreError> {
    let workspace_dir = CapDir::open_ambient_dir(workspace_root, cap_std::ambient_authority())
        .map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to pin transaction workspace root {}: {err}",
                    workspace_root.display()
                ),
            )
        })?;
    let workspace_identity =
        cap_object_identity(&workspace_dir.dir_metadata().map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to inspect pinned transaction workspace root {}: {err}",
                    workspace_root.display()
                ),
            )
        })?)?;
    #[cfg(target_os = "macos")]
    let workspace_display = native_tx_descriptor_path(&workspace_dir, workspace_root)?;
    #[cfg(not(target_os = "macos"))]
    let workspace_display = workspace_root.canonicalize().map_err(|err| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "failed to resolve transaction workspace root {}: {err}",
                workspace_root.display()
            ),
        )
    })?;
    let workspace_namespace_metadata = std::fs::metadata(&workspace_display).map_err(|err| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "failed to verify transaction workspace root {}: {err}",
                workspace_display.display()
            ),
        )
    })?;
    if std_object_identity(&workspace_namespace_metadata)? != workspace_identity {
        return Err(TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "transaction workspace root changed while being pinned: {}",
                workspace_display.display()
            ),
        ));
    }

    let relative =
        normalized_workspace_relative_contract_path(workspace_root, &workspace_display, path)?;
    let control_display = workspace_display.join(".ft");
    let control_dir =
        open_or_create_pinned_dir(&workspace_dir, Path::new(".ft"), &control_display)?;
    let control_identity = cap_object_identity(&control_dir.dir_metadata().map_err(|err| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "failed to inspect pinned transaction control directory {}: {err}",
                control_display.display()
            ),
        )
    })?)?;
    let lock_dir_display = control_display.join("tx_contract_locks");
    let lock_dir = open_or_create_pinned_dir(
        &control_dir,
        Path::new("tx_contract_locks"),
        &lock_dir_display,
    )?;
    let lock_dir_identity = cap_object_identity(&lock_dir.dir_metadata().map_err(|err| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "failed to inspect pinned transaction lock directory {}: {err}",
                lock_dir_display.display()
            ),
        )
    })?)?;
    let requested_contract_name = relative
        .file_name()
        .expect("normalized transaction contract path has a file name")
        .to_os_string();
    let requested_parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
    let mut parent_dir = workspace_dir.try_clone().map_err(|err| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!("failed to clone pinned transaction workspace capability: {err}"),
        )
    })?;
    for component in requested_parent_relative.components() {
        let std::path::Component::Normal(part) = component else {
            continue;
        };
        parent_dir = parent_dir.open_dir_nofollow(part).map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to descend transaction contract parent {} without following symlinks: {err}",
                    workspace_display.join(requested_parent_relative).display()
                ),
            )
        })?;
    }
    let parent_identity = cap_object_identity(&parent_dir.dir_metadata().map_err(|err| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "failed to inspect pinned transaction contract parent {}: {err}",
                workspace_display.join(requested_parent_relative).display()
            ),
        )
    })?)?;
    let requested_key = workspace_display
        .join(requested_parent_relative)
        .join(&requested_contract_name);

    // Reject hostile leaf types before discovery. Native discovery and the
    // post-lock authoritative open also use O_NONBLOCK and no-follow, then
    // recheck the opened type, so replacement with a FIFO cannot block either.
    let leaf_metadata = parent_dir
        .symlink_metadata(&requested_contract_name)
        .map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to inspect transaction contract {}: {err}",
                    requested_key.display()
                ),
            )
        })?;
    if !leaf_metadata.is_file() {
        return Err(TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "transaction contract is not a regular file: {}",
                requested_key.display()
            ),
        ));
    }

    // Derive the process and sidecar lock key from the filesystem's verified
    // canonical spelling. This collapses case/spelling aliases on
    // case-insensitive filesystems before either lock namespace is consulted.
    #[cfg(target_os = "macos")]
    let canonical_relative =
        discover_native_tx_contract_path(&parent_dir, &requested_contract_name, &requested_key)?;
    #[cfg(not(target_os = "macos"))]
    let canonical_relative = workspace_dir.canonicalize(&relative).map_err(|err| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "failed to canonicalize pinned transaction contract {} relative to workspace: {err}",
                requested_key.display()
            ),
        )
    })?;
    let relative = normalized_workspace_relative_contract_path(
        workspace_root,
        &workspace_display,
        &canonical_relative,
    )?;
    let contract_name = relative
        .file_name()
        .expect("canonical transaction contract path has a file name")
        .to_os_string();
    let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
    let mut canonical_parent_dir = workspace_dir.try_clone().map_err(|err| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!("failed to clone pinned transaction workspace capability: {err}"),
        )
    })?;
    for component in parent_relative.components() {
        let std::path::Component::Normal(part) = component else {
            continue;
        };
        canonical_parent_dir = canonical_parent_dir.open_dir_nofollow(part).map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to verify canonical transaction contract parent {} without following symlinks: {err}",
                    workspace_display.join(parent_relative).display()
                ),
            )
        })?;
    }
    let canonical_parent_identity =
        cap_object_identity(&canonical_parent_dir.dir_metadata().map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to identify canonical transaction contract parent {}: {err}",
                    workspace_display.join(parent_relative).display()
                ),
            )
        })?)?;
    if canonical_parent_identity != parent_identity {
        return Err(TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "transaction contract parent changed while canonicalizing {}",
                requested_key.display()
            ),
        ));
    }
    parent_dir = canonical_parent_dir;
    let parent_identity = canonical_parent_identity;
    let parent_display = workspace_display.join(parent_relative);
    let key = workspace_display.join(&relative);
    // On Linux, capability canonicalization opens O_PATH and reads its procfs
    // name. A concurrent rename can leave a deleted-inode spelling. Confirm
    // that the observed spelling still names the requested regular leaf;
    // stale discovery is a pre-effect conflict, never a different lock key.
    // Keep filesystem spelling/alias resolution instead of lowercasing names.
    verify_tx_contract_canonical_leaf(
        &parent_dir,
        &requested_contract_name,
        &contract_name,
        &requested_key,
    )?;
    let parent_sync_file = open_workspace_relative_directory_sync_file(
        &workspace_dir,
        parent_relative,
        &parent_dir,
        &parent_display,
    )?;

    // The production callback is empty. Tests use this exact cutpoint to
    // complete a competing durable replacement before this contender locks.
    before_lock();
    let mut locks = TX_CONTRACT_LOCKS.lock().map_err(|_| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            "transaction contract lock registry is poisoned".to_string(),
        )
    })?;

    if !locks.insert(key.clone()) {
        return Err(TxContractStoreError::new(
            TxContractStoreErrorKind::InProgress,
            format!(
                "transaction contract is already being executed: {}",
                key.display()
            ),
        ));
    }
    drop(locks);

    let lock_name = workspace_root_lock_name(&relative);
    let lock_display = lock_dir_display.join(&lock_name);
    let mut create_lock_options = CapOpenOptions::new();
    create_lock_options
        .create_new(true)
        .truncate(false)
        .read(true)
        .write(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    create_lock_options.mode(0o600).nonblock(true);
    let (cap_lock_file, created_lock_file) = match lock_dir
        .open_with(&lock_name, &create_lock_options)
    {
        Ok(file) => (file, true),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut existing_lock_options = CapOpenOptions::new();
            existing_lock_options
                .read(true)
                .write(true)
                .follow(FollowSymlinks::No);
            #[cfg(unix)]
            existing_lock_options.nonblock(true);
            match lock_dir.open_with(&lock_name, &existing_lock_options) {
                Ok(file) => (file, false),
                Err(open_err) => {
                    release_tx_contract_lock_key(&key);
                    return Err(TxContractStoreError::new(
                        TxContractStoreErrorKind::Lock,
                        format!(
                            "failed to open existing transaction contract lock {} without following symlinks: {open_err}",
                            lock_display.display()
                        ),
                    ));
                }
            }
        }
        Err(err) => {
            release_tx_contract_lock_key(&key);
            return Err(TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to open transaction contract lock {}: {err}",
                    lock_display.display()
                ),
            ));
        }
    };
    let lock_metadata = match cap_lock_file.metadata() {
        Ok(metadata) => metadata,
        Err(err) => {
            release_tx_contract_lock_key(&key);
            return Err(TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to identify transaction contract lock {}: {err}",
                    lock_display.display()
                ),
            ));
        }
    };
    if !lock_metadata.is_file() {
        release_tx_contract_lock_key(&key);
        return Err(TxContractStoreError::new(
            TxContractStoreErrorKind::Lock,
            format!(
                "transaction contract lock is not a regular file: {}",
                lock_display.display()
            ),
        ));
    }
    if let Err(err) = require_single_link(&lock_metadata, &lock_display, "lock sidecar") {
        release_tx_contract_lock_key(&key);
        return Err(err);
    }
    let lock_identity = match cap_object_identity(&lock_metadata) {
        Ok(identity) => identity,
        Err(err) => {
            release_tx_contract_lock_key(&key);
            return Err(err);
        }
    };
    if created_lock_file {
        if let Err(err) = cap_lock_file
            .sync_all()
            .map_err(|sync_err| {
                TxContractStoreError::new(
                    TxContractStoreErrorKind::Sync,
                    format!(
                        "failed to synchronize new transaction contract lock {}: {sync_err}",
                        lock_display.display()
                    ),
                )
            })
            .and_then(|()| sync_pinned_directory(&lock_dir, &lock_dir_display))
        {
            release_tx_contract_lock_key(&key);
            return Err(err);
        }
    }
    let file = cap_lock_file.into_std();

    if let Err(err) = file.try_lock_exclusive() {
        release_tx_contract_lock_key(&key);
        let kind = if err.kind() == std::io::ErrorKind::WouldBlock {
            TxContractStoreErrorKind::InProgress
        } else {
            TxContractStoreErrorKind::Lock
        };
        return Err(TxContractStoreError::new(
            kind,
            format!(
                "failed to acquire transaction contract lock {}: {err}",
                lock_display.display()
            ),
        ));
    }

    // Pin the mutable leaf only after both lock namespaces are owned. A
    // cooperative commit can replace its inode at any earlier point; keeping
    // that stale inode used to turn ordinary contention into an authority
    // failure. Repeat all leaf/type/alias checks under the lock instead.
    let pinned_contract = (|| -> Result<CapFile, TxContractStoreError> {
        let mut contract_options = CapOpenOptions::new();
        contract_options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        contract_options.nonblock(true);
        let contract_file = parent_dir
            .open_with(&requested_contract_name, &contract_options)
            .map_err(|err| {
                TxContractStoreError::new(
                    TxContractStoreErrorKind::Lock,
                    format!(
                        "failed to open transaction contract {} without following symlinks: {err}",
                        requested_key.display()
                    ),
                )
            })?;
        let contract_metadata = contract_file.metadata().map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to inspect transaction contract {}: {err}",
                    requested_key.display()
                ),
            )
        })?;
        if !contract_metadata.is_file() {
            return Err(TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "transaction contract is not a regular file: {}",
                    requested_key.display()
                ),
            ));
        }
        require_single_link(&contract_metadata, &requested_key, "contract")?;
        let canonical_file = parent_dir
            .open_with(&contract_name, &contract_options)
            .map_err(|err| {
                TxContractStoreError::new(
                    TxContractStoreErrorKind::Lock,
                    format!(
                        "failed to verify canonical transaction contract {} without following symlinks: {err}",
                        key.display()
                    ),
                )
            })?;
        let canonical_metadata = canonical_file.metadata().map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "failed to identify canonical transaction contract {}: {err}",
                    key.display()
                ),
            )
        })?;
        if cap_object_identity(&canonical_metadata)? != cap_object_identity(&contract_metadata)? {
            return Err(TxContractStoreError::new(
                TxContractStoreErrorKind::Lock,
                format!(
                    "transaction contract identity changed while pinning {}",
                    requested_key.display()
                ),
            ));
        }
        require_single_link(&canonical_metadata, &key, "contract")?;
        Ok(contract_file)
    })();
    let contract_file = match pinned_contract {
        Ok(contract_file) => contract_file,
        Err(err) => {
            drop(file);
            release_tx_contract_lock_key(&key);
            return Err(err);
        }
    };

    let guard = TxContractLockGuard {
        key,
        workspace_display,
        workspace_identity,
        workspace_dir,
        control_display,
        control_identity,
        control_dir,
        lock_dir,
        lock_dir_identity,
        parent_relative: parent_relative.to_path_buf(),
        parent_display,
        parent_identity,
        parent_dir,
        parent_sync_file,
        contract_name,
        contract_file: Mutex::new(contract_file),
        lock_name,
        lock_identity,
        file,
    };
    if let Err(err) = guard.authorizes(guard.authoritative_path()) {
        drop(guard);
        return Err(err);
    }
    Ok(guard)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxContractSaveFaultPoint {
    BeforeWrite,
    BeforeFileSync,
    BeforeAtomicReplace,
    ParentDirectorySync,
}

fn retain_failed_tx_contract_temp(
    guard: &TxContractLockGuard,
    temp_name: &Path,
    kind: TxContractStoreErrorKind,
    mut message: String,
) -> TxContractStoreError {
    let recovery_path = guard.recovery_display_path(temp_name);
    if let Err(err) = guard.sync_parent() {
        message.push_str(&format!(
            "; recovery entry exists through the pinned parent but its containing directory could not be synchronized: {err}"
        ));
    }
    TxContractStoreError::new(kind, message).with_recovery_path(recovery_path)
}

fn create_tx_contract_recovery_file(
    guard: &TxContractLockGuard,
) -> Result<(PathBuf, CapFile), TxContractStoreError> {
    let stem = guard
        .contract_name
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "_");
    for _ in 0..64 {
        let nonce = TX_CONTRACT_TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
        let name = PathBuf::from(format!(
            ".{stem}.{}.{nonce}.recovery.tmp",
            std::process::id()
        ));
        let mut options = CapOpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        #[cfg(unix)]
        options.mode(0o600).nonblock(true);
        match guard.parent_dir.open_with(&name, &options) {
            Ok(file) => return Ok((name, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(TxContractStoreError::new(
                    TxContractStoreErrorKind::Write,
                    format!(
                        "failed to securely create transaction contract recovery file beside {} through the pinned parent: {err}",
                        guard.key.display()
                    ),
                ));
            }
        }
    }
    Err(TxContractStoreError::new(
        TxContractStoreErrorKind::Write,
        format!(
            "failed to allocate a unique transaction contract recovery name beside {}",
            guard.key.display()
        ),
    ))
}

/// Validate, serialize, and durably replace a mission transaction contract.
///
/// Bytes are written through a securely-created sibling file, synchronized,
/// atomically renamed over the authoritative path, and followed by a parent
/// directory sync on supported Unix platforms. Windows mutation lock
/// acquisition currently fails closed before effects because a documented
/// durable replacement primitive is not yet wired. Every failure after the
/// sibling file is created and before the rename retains that file as a
/// recovery artifact. A write failure may leave a partial artifact; later
/// failures retain the full serialized snapshot. A parent-sync failure is
/// reported only after the authoritative path has already been replaced.
///
/// Immediately before publication, the pinned parent must still name the
/// originally authorized contract inode. POSIX rename cannot atomically
/// compare the destination inode while replacing it, so a same-UID actor that
/// swaps the basename in the tiny identity-check-to-rename window is outside
/// this API's noninterference guarantee. Workspace-root and `.ft` replacement
/// by a same-UID actor are likewise outside the trusted-anchor model.
///
/// # Errors
///
/// Returns a typed error for validation, serialization, size, write, sync, or
/// rename failures. The supplied guard must own the canonical path being
/// replaced. Callers must not emit transaction success after an error.
pub fn save_tx_contract_atomic(
    guard: &TxContractLockGuard,
    path: &Path,
    contract: &MissionTxContract,
) -> Result<(), TxContractStoreError> {
    save_tx_contract_atomic_impl(guard, path, contract, |_| Ok(()))
}

fn save_tx_contract_atomic_impl<F>(
    guard: &TxContractLockGuard,
    path: &Path,
    contract: &MissionTxContract,
    fault: F,
) -> Result<(), TxContractStoreError>
where
    F: FnMut(TxContractSaveFaultPoint) -> std::io::Result<()>,
{
    guard.verify_logical_path(path)?;
    // Do not resolve an ambient contract path here. A parent namespace swap
    // can race immediately after pre-effect authorization; the pinned file,
    // workspace-global ledger, and capability-relative parent remain the
    // authoritative recovery objects. The basename inside that pinned parent
    // is revalidated immediately before publication below.
    guard.verify_pinned_mutation_edges()?;
    contract.validate().map_err(|err| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Validation,
            format!("transaction contract validation failed before save: {err}"),
        )
    })?;
    validate_tx_contract_state_outcome(contract).map_err(|err| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Validation,
            format!("transaction contract lifecycle/outcome mismatch before save: {err}"),
        )
    })?;
    let bytes = serde_json::to_vec_pretty(contract).map_err(|err| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Serialization,
            format!("failed to serialize transaction contract: {err}"),
        )
    })?;
    save_contract_bytes_atomic_impl(guard, path, &bytes, fault)
}

fn save_contract_bytes_atomic_impl<F>(
    guard: &TxContractLockGuard,
    path: &Path,
    bytes: &[u8],
    mut fault: F,
) -> Result<(), TxContractStoreError>
where
    F: FnMut(TxContractSaveFaultPoint) -> std::io::Result<()>,
{
    guard.verify_logical_path(path)?;
    guard.verify_pinned_mutation_edges()?;
    let path = guard.key.as_path();
    let metadata = guard.pinned_contract_metadata()?;
    let existing_permissions = metadata.permissions();
    let (temp_name, mut temp_file) = create_tx_contract_recovery_file(guard)?;
    let temp_path = guard.recovery_display_path(&temp_name);

    if let Err(err) =
        fault(TxContractSaveFaultPoint::BeforeWrite).and_then(|()| temp_file.write_all(bytes))
    {
        return Err(retain_failed_tx_contract_temp(
            guard,
            &temp_name,
            TxContractStoreErrorKind::Write,
            format!(
                "failed to write transaction contract recovery file {}: {err}",
                temp_path.display()
            ),
        ));
    }
    if let Err(err) = temp_file.set_permissions(existing_permissions) {
        return Err(retain_failed_tx_contract_temp(
            guard,
            &temp_name,
            TxContractStoreErrorKind::Write,
            format!(
                "failed to preserve transaction contract permissions on {}: {err}",
                temp_path.display()
            ),
        ));
    }
    if let Err(err) = fault(TxContractSaveFaultPoint::BeforeFileSync) {
        return Err(retain_failed_tx_contract_temp(
            guard,
            &temp_name,
            TxContractStoreErrorKind::Sync,
            format!(
                "failed before synchronizing transaction contract recovery file {}: {err}",
                temp_path.display()
            ),
        ));
    }
    if let Err(err) = temp_file.sync_all() {
        return Err(retain_failed_tx_contract_temp(
            guard,
            &temp_name,
            TxContractStoreErrorKind::Sync,
            format!(
                "failed to synchronize transaction contract recovery file {}: {err}",
                temp_path.display()
            ),
        ));
    }

    // Always retain a complete, synchronized post-execution snapshot before
    // enforcing the normal loader limit. A contract may grow past the limit
    // only after external effects append receipts; returning before this write
    // would discard the only durable evidence of those effects.
    if bytes.len() > TX_CONTRACT_MAX_BYTES {
        return Err(retain_failed_tx_contract_temp(
            guard,
            &temp_name,
            TxContractStoreErrorKind::TooLarge,
            format!(
                "serialized transaction contract is {} bytes; maximum is {TX_CONTRACT_MAX_BYTES}",
                bytes.len()
            ),
        ));
    }

    if let Err(err) = fault(TxContractSaveFaultPoint::BeforeAtomicReplace) {
        return Err(retain_failed_tx_contract_temp(
            guard,
            &temp_name,
            TxContractStoreErrorKind::Rename,
            format!(
                "failed before atomically replacing transaction contract {}: {err}",
                path.display()
            ),
        ));
    }

    let replacement_metadata = match temp_file.metadata() {
        Ok(metadata) => metadata,
        Err(err) => {
            return Err(retain_failed_tx_contract_temp(
                guard,
                &temp_name,
                TxContractStoreErrorKind::Rename,
                format!(
                    "failed to inspect synchronized transaction contract replacement {} before publication: {err}",
                    temp_path.display()
                ),
            ));
        }
    };
    if let Err(err) = require_single_link(&replacement_metadata, &temp_path, "replacement") {
        return Err(retain_failed_tx_contract_temp(
            guard,
            &temp_name,
            TxContractStoreErrorKind::Rename,
            format!(
                "refused to publish transaction contract replacement {}: {err}",
                temp_path.display()
            ),
        ));
    }
    let mut pinned_contract = match guard.contract_file.lock() {
        Ok(file) => file,
        Err(_) => {
            return Err(retain_failed_tx_contract_temp(
                guard,
                &temp_name,
                TxContractStoreErrorKind::Lock,
                "pinned transaction contract file mutex is poisoned before publication".to_string(),
            ));
        }
    };
    let pinned_metadata = match pinned_contract.metadata() {
        Ok(metadata) => metadata,
        Err(err) => {
            return Err(retain_failed_tx_contract_temp(
                guard,
                &temp_name,
                TxContractStoreErrorKind::Rename,
                format!(
                    "failed to identify the authorized transaction contract immediately before publication: {err}"
                ),
            ));
        }
    };
    let pinned_identity = match cap_object_identity(&pinned_metadata) {
        Ok(identity) => identity,
        Err(err) => {
            return Err(retain_failed_tx_contract_temp(
                guard,
                &temp_name,
                TxContractStoreErrorKind::Rename,
                format!(
                    "failed to identify the authorized transaction contract immediately before publication: {err}"
                ),
            ));
        }
    };
    let destination_still_authorized = match guard
        .named_contract_entry_matches_identity(pinned_identity)
    {
        Ok(matches) => matches,
        Err(err) => {
            return Err(retain_failed_tx_contract_temp(
                guard,
                &temp_name,
                TxContractStoreErrorKind::Rename,
                format!(
                    "failed to revalidate the authorized transaction contract basename immediately before publication: {err}"
                ),
            ));
        }
    };
    if !destination_still_authorized {
        return Err(retain_failed_tx_contract_temp(
            guard,
            &temp_name,
            TxContractStoreErrorKind::Rename,
            format!(
                "refused to replace transaction contract {} because its basename no longer names the pre-effect authorized inode",
                path.display()
            ),
        ));
    }

    #[cfg(target_os = "macos")]
    {
        // Preserve recovery through a detached pinned parent, but never
        // replace a case-renamed basename under the old sidecar identity.
        let spelling = native_tx_descriptor_path(&*pinned_contract, path);
        if !matches!(spelling, Ok(ref current) if current.file_name() == Some(guard.contract_name.as_os_str()))
        {
            return Err(retain_failed_tx_contract_temp(
                guard,
                &temp_name,
                TxContractStoreErrorKind::Rename,
                "transaction contract native basename changed before publication".to_string(),
            ));
        }
    }

    if let Err(err) = guard.parent_dir.rename(
        &temp_name,
        &guard.parent_dir,
        Path::new(&guard.contract_name),
    ) {
        return Err(retain_failed_tx_contract_temp(
            guard,
            &temp_name,
            TxContractStoreErrorKind::Rename,
            format!(
                "failed to atomically replace transaction contract {} through its pinned parent: {err}",
                path.display()
            ),
        ));
    }

    // Publication has happened. Attempt the real parent sync on every path,
    // including when the test fault seam reports a simulated sync failure,
    // before performing any other fallible operation. The replacement handle
    // and its identity were validated and the mutex acquired pre-rename, so
    // updating the pinned handle is now infallible.
    let injected_sync_error = fault(TxContractSaveFaultPoint::ParentDirectorySync).err();
    let parent_sync_error = guard.sync_parent().err();
    *pinned_contract = temp_file;
    drop(pinned_contract);
    if injected_sync_error.is_some() || parent_sync_error.is_some() {
        let detail = match (injected_sync_error, parent_sync_error) {
            (Some(injected), Some(actual)) => {
                format!("injected sync failure: {injected}; actual sync also failed: {actual}")
            }
            (Some(injected), None) => injected.to_string(),
            (None, Some(actual)) => actual.to_string(),
            (None, None) => unreachable!("sync failure branch requires an error"),
        };
        return Err(TxContractStoreError::new(
            TxContractStoreErrorKind::Sync,
            format!(
                "transaction contract was replaced through its pinned parent, but parent directory {} could not be confirmed synchronized: {detail}",
                path.parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."))
                    .display()
            ),
        ).after_publication());
    }

    let parent_still_named = guard.named_parent_matches_pinned_parent().map_err(|err| {
        TxContractStoreError::new(
            TxContractStoreErrorKind::Rename,
            format!(
                "transaction contract was durably saved through its pinned parent, but the last-known namespace {} could not be revalidated: {err}",
                path.display()
            ),
        ).after_publication()
    })?;
    if !parent_still_named {
        return Err(TxContractStoreError::new(
            TxContractStoreErrorKind::Rename,
            format!(
                "transaction contract was durably saved through its pinned parent, but the last-known contract path {} is namespace-detached and may resolve to stale or foreign data",
                path.display()
            ),
        ).after_publication());
    }
    let basename_still_names_replacement = guard
        .named_contract_entry_matches_pinned_file()
        .map_err(|err| {
            TxContractStoreError::new(
                TxContractStoreErrorKind::Rename,
                format!(
                    "transaction contract was durably saved through its pinned parent, but the last-known basename {} could not be revalidated: {err}",
                    path.display()
                ),
            ).after_publication()
        })?;
    if !basename_still_names_replacement {
        return Err(TxContractStoreError::new(
            TxContractStoreErrorKind::Rename,
            format!(
                "transaction contract was durably saved through its pinned parent, but the last-known basename {} is stale or now resolves to foreign data",
                path.display()
            ),
        ).after_publication());
    }

    Ok(())
}

/// Optimistic concurrency token for one complete persisted mission incarnation.
/// The semantic mission ID alone is reusable and cannot authorize an update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MissionRevisionToken {
    pub mission_id: String,
    pub generation: String,
    /// Canonical decimal text on the wire preserves all 64 bits through TOON
    /// and JavaScript clients. A floating-point counter cannot be authority.
    #[serde(with = "mission_revision_wire")]
    pub revision: u64,
    pub content_sha256: String,
}

mod mission_revision_wire {
    use serde::{Deserialize, Deserializer, Serializer};

    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "Serde serialize_with callbacks receive a reference to the field"
    )]
    pub fn serialize<S: Serializer>(revision: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&revision.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let text = String::deserialize(deserializer)?;
        let revision = text.parse::<u64>().map_err(|_| {
            serde::de::Error::custom("mission revision must be a canonical decimal u64 string")
        })?;
        if text != revision.to_string() {
            return Err(serde::de::Error::custom(
                "mission revision is not canonical decimal text",
            ));
        }
        Ok(revision)
    }
}

impl MissionRevisionToken {
    /// Compute from every serialized field, including checkpoint history.
    pub fn from_mission(mission: &Mission) -> Result<Self, MissionStoreError> {
        let bytes = serde_json::to_vec(mission).map_err(|_| MissionStoreError::Invalid)?;
        Ok(Self {
            mission_id: mission.mission_id.0.clone(),
            generation: mission.generation.clone(),
            revision: mission.revision,
            content_sha256: hex::encode(Sha256::digest(&bytes)),
        })
    }
}

/// Content-free classifications: neither task text nor filesystem error payloads
/// are copied into the control response. Indeterminate requires reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MissionStoreError {
    #[error("another mission mutation owns the file lock")]
    InProgress,
    #[error("mission revision or incarnation changed; reread before retrying")]
    Conflict,
    #[error("mission contract or revision token is invalid")]
    Invalid,
    #[error("mission contract exceeds the bounded storage limit")]
    TooLarge,
    #[error("mission file authority could not be established")]
    Authority,
    #[error("mission write failed before publication")]
    Write,
    #[error("mission file sync failed before publication")]
    Sync,
    #[error("mission replacement failed before publication")]
    Rename,
    #[error(
        "mission was published but durability or namespace is indeterminate; reconcile before any retry"
    )]
    Indeterminate,
}

impl MissionStoreError {
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::InProgress => "mission.mutation_in_progress",
            Self::Conflict => "mission.revision_conflict",
            Self::Invalid => "mission.invalid_contract",
            Self::TooLarge => "mission.too_large",
            Self::Authority => "mission.authority_failed",
            Self::Write => "mission.write_failed",
            Self::Sync => "mission.sync_failed",
            Self::Rename => "mission.rename_failed",
            Self::Indeterminate => "mission.durability_indeterminate",
        }
    }
}

impl From<TxContractStoreError> for MissionStoreError {
    fn from(error: TxContractStoreError) -> Self {
        if error.published() {
            return Self::Indeterminate;
        }
        match error.kind() {
            TxContractStoreErrorKind::InProgress => Self::InProgress,
            TxContractStoreErrorKind::Conflict => Self::Conflict,
            TxContractStoreErrorKind::Lock => Self::Authority,
            TxContractStoreErrorKind::Validation | TxContractStoreErrorKind::Serialization => {
                Self::Invalid
            }
            TxContractStoreErrorKind::TooLarge => Self::TooLarge,
            TxContractStoreErrorKind::Write => Self::Write,
            TxContractStoreErrorKind::Sync => Self::Sync,
            TxContractStoreErrorKind::Rename => Self::Rename,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MissionMutationReceipt {
    pub previous: MissionRevisionToken,
    pub current: MissionRevisionToken,
    pub changed: bool,
    pub durability: &'static str,
    /// A persisted lifecycle request is not acknowledgement by a running owner.
    pub owner_acknowledgement: &'static str,
}

/// One lock spans the authoritative bounded read, transition, and durable save.
/// Uses the same pinned capability and trusted workspace assumptions as tx saves.
pub struct MissionMutationGuard {
    guard: TxContractLockGuard,
    mission: Mission,
    original: MissionRevisionToken,
    original_bytes_sha256: String,
}

impl MissionMutationGuard {
    pub fn acquire(
        workspace_root: &Path,
        path: &Path,
        expected: Option<&MissionRevisionToken>,
    ) -> Result<Self, MissionStoreError> {
        let guard = acquire_tx_contract_lock(workspace_root, path)?;
        Self::from_guard(guard, expected)
    }

    fn from_guard(
        guard: TxContractLockGuard,
        expected: Option<&MissionRevisionToken>,
    ) -> Result<Self, MissionStoreError> {
        let bytes = guard.read_authoritative_contract_bytes()?;
        let mission: Mission =
            serde_json::from_slice(&bytes).map_err(|_| MissionStoreError::Invalid)?;
        mission.validate().map_err(|_| MissionStoreError::Invalid)?;
        let original = MissionRevisionToken::from_mission(&mission)?;
        if expected.is_some_and(|token| token != &original) {
            return Err(MissionStoreError::Conflict);
        }
        Ok(Self {
            guard,
            mission,
            original,
            original_bytes_sha256: hex::encode(Sha256::digest(&bytes)),
        })
    }

    #[must_use]
    pub fn mission(&self) -> &Mission {
        &self.mission
    }

    pub fn commit(
        self,
        mission: &mut Mission,
    ) -> Result<MissionMutationReceipt, MissionStoreError> {
        self.commit_impl(mission, |_| Ok(()))
    }

    fn commit_impl<F>(
        self,
        mission: &mut Mission,
        fault: F,
    ) -> Result<MissionMutationReceipt, MissionStoreError>
    where
        F: FnMut(TxContractSaveFaultPoint) -> std::io::Result<()>,
    {
        if mission.mission_id != self.mission.mission_id
            || mission.generation != self.mission.generation
            || mission.workspace_id != self.mission.workspace_id
            || mission.created_at_ms != self.mission.created_at_ms
            || mission.revision != self.mission.revision
        {
            return Err(MissionStoreError::Conflict);
        }
        if mission.updated_at_ms.unwrap_or(mission.created_at_ms)
            < self
                .mission
                .updated_at_ms
                .unwrap_or(self.mission.created_at_ms)
            || mission.pause_resume_state.total_pause_count
                < self.mission.pause_resume_state.total_pause_count
            || mission.pause_resume_state.total_resume_count
                < self.mission.pause_resume_state.total_resume_count
            || mission.pause_resume_state.total_abort_count
                < self.mission.pause_resume_state.total_abort_count
            || mission.pause_resume_state.cumulative_pause_duration_ms
                < self.mission.pause_resume_state.cumulative_pause_duration_ms
        {
            return Err(MissionStoreError::Invalid);
        }
        mission.validate().map_err(|_| MissionStoreError::Invalid)?;
        // Detect an uncooperative in-place writer as well as inode replacement.
        // Same-UID mutation racing the final read/rename remains outside the
        // shared trusted-anchor model; cooperative writers all take this lock.
        let bytes = self.guard.read_authoritative_contract_bytes()?;
        if hex::encode(Sha256::digest(&bytes)) != self.original_bytes_sha256 {
            return Err(MissionStoreError::Conflict);
        }
        let proposed = MissionRevisionToken::from_mission(mission)?;
        let changed = proposed != self.original;
        let current = if changed {
            let mut next = mission.clone();
            next.revision = next
                .revision
                .checked_add(1)
                .ok_or(MissionStoreError::Invalid)?;
            let bytes = serde_json::to_vec_pretty(&next).map_err(|_| MissionStoreError::Invalid)?;
            if bytes.len() > TX_CONTRACT_MAX_BYTES {
                return Err(MissionStoreError::TooLarge);
            }
            let current = MissionRevisionToken::from_mission(&next)?;
            save_contract_bytes_atomic_impl(
                &self.guard,
                self.guard.authoritative_path(),
                &bytes,
                fault,
            )?;
            *mission = next;
            current
        } else {
            proposed
        };
        Ok(MissionMutationReceipt {
            previous: self.original,
            current,
            changed,
            durability: if changed {
                "file_and_directory_synced"
            } else {
                "unchanged_observation"
            },
            owner_acknowledgement: "unavailable_no_mission_driver",
        })
    }
}

// ── Step Executor Trait ──────────────────────────────────────────────────────

/// Trait for executing individual tx steps.
///
/// The engine calls this to perform actual work (e.g., sending commands to panes,
/// acquiring reservations, evaluating policies). The default synthetic implementation
/// uses deterministic inputs for testing.
pub trait StepExecutor {
    /// Evaluate prepare-phase gates for all steps.
    fn evaluate_gates(&self, contract: &MissionTxContract, now_ms: i64) -> Vec<TxPrepareGateInput>;

    /// Execute commit-phase steps and return inputs.
    fn execute_steps(
        &self,
        contract: &MissionTxContract,
        fail_step: Option<&str>,
        now_ms: i64,
    ) -> Vec<TxCommitStepInput>;

    /// Execute compensation steps and return inputs.
    fn execute_compensations(
        &self,
        contract: &MissionTxContract,
        commit_report: &TxCommitReport,
        fail_for_step: Option<&str>,
        now_ms: i64,
    ) -> Vec<TxCompensationStepInput>;
}

mod effect_seal {
    /// Sealed supertrait of [`super::NonEffectfulStepExecutor`]. It lives in a
    /// private module so the set of executors that may dispatch through the
    /// storeless engine entrypoints is enumerated in this file and nowhere
    /// else — no downstream crate can widen it.
    pub trait NonEffectful {}
}

/// Witness that a [`StepExecutor`] dispatches **no** external side effects:
/// no pane writes, no lock acquisition, no storage mutation, no workflow runs.
///
/// Only such an executor may use the storeless
/// [`TxExecutionEngine::execute`] / [`TxExecutionEngine::rollback`]
/// entrypoints. Those run without a durable idempotency spool, and therefore
/// without a write-ahead `Pending` record, per-key/execution locks, durable
/// replay proof, or crash reconciliation. Pairing a *real* effect executor
/// with them would dispatch external effects outside the exactly-once
/// boundary that `*_with_store` enforces (ft-3lqyu / ft-0rlfq.8).
///
/// The trait is sealed via the private [`effect_seal::NonEffectful`]
/// supertrait, so `frankenterm-core` is the only crate that can classify an
/// executor as non-effectful. [`PaneStepExecutor`] deliberately does **not**
/// implement it; real dispatch must go through
/// [`TxExecutionEngine::execute_with_store`],
/// [`TxExecutionEngine::rollback_with_store`], or
/// [`TxExecutionEngine::resume`], each of which rejects a non-durable spool.
pub trait NonEffectfulStepExecutor: StepExecutor + effect_seal::NonEffectful {}

impl<E> NonEffectfulStepExecutor for E where E: StepExecutor + effect_seal::NonEffectful {}

/// Compile-time canary for the effect seal. Accepts only executors that
/// satisfy [`NonEffectfulStepExecutor`].
///
/// # Examples
///
/// Accepts the synthetic executor:
///
/// ```
/// use frankenterm_core::tx_execution::{assert_non_effectful_executor, SyntheticStepExecutor};
///
/// assert_non_effectful_executor(&SyntheticStepExecutor);
/// ```
///
/// Rejects an out-of-crate executor, however it is implemented — the seal is
/// what stops a downstream crate from declaring its own effectful executor
/// safe for the storeless entrypoints:
///
/// ```compile_fail
/// use frankenterm_core::plan::{
///     MissionTxContract, TxCommitReport, TxCommitStepInput, TxCompensationStepInput,
///     TxPrepareGateInput,
/// };
/// use frankenterm_core::tx_execution::{assert_non_effectful_executor, StepExecutor};
///
/// struct ForeignExecutor;
///
/// impl StepExecutor for ForeignExecutor {
///     fn evaluate_gates(&self, _c: &MissionTxContract, _n: i64) -> Vec<TxPrepareGateInput> {
///         Vec::new()
///     }
///     fn execute_steps(
///         &self,
///         _c: &MissionTxContract,
///         _f: Option<&str>,
///         _n: i64,
///     ) -> Vec<TxCommitStepInput> {
///         Vec::new()
///     }
///     fn execute_compensations(
///         &self,
///         _c: &MissionTxContract,
///         _r: &TxCommitReport,
///         _f: Option<&str>,
///         _n: i64,
///     ) -> Vec<TxCompensationStepInput> {
///         Vec::new()
///     }
/// }
///
/// assert_non_effectful_executor(&ForeignExecutor);
/// ```
#[inline]
pub fn assert_non_effectful_executor<E: NonEffectfulStepExecutor + ?Sized>(_executor: &E) {}

/// Synthetic step executor that produces deterministic results for testing.
pub struct SyntheticStepExecutor;

impl effect_seal::NonEffectful for SyntheticStepExecutor {}

impl StepExecutor for SyntheticStepExecutor {
    fn evaluate_gates(
        &self,
        contract: &MissionTxContract,
        _now_ms: i64,
    ) -> Vec<TxPrepareGateInput> {
        crate::plan::tx_prepare_gate_inputs_allow_all(contract)
    }

    fn execute_steps(
        &self,
        contract: &MissionTxContract,
        fail_step: Option<&str>,
        now_ms: i64,
    ) -> Vec<TxCommitStepInput> {
        crate::plan::mission_tx_commit_step_inputs(contract, fail_step, now_ms)
    }

    fn execute_compensations(
        &self,
        _contract: &MissionTxContract,
        commit_report: &TxCommitReport,
        fail_for_step: Option<&str>,
        now_ms: i64,
    ) -> Vec<TxCompensationStepInput> {
        crate::plan::mission_tx_compensation_inputs(commit_report, fail_for_step, now_ms)
    }
}

/// Prepare-phase gate evaluator wired to the real policy engine, approval
/// store, and target-state providers.
///
/// This is deliberately **not** a [`StepExecutor`] (ft-0rlfq.8). It used to
/// implement the full trait, with `execute_steps` / `execute_compensations`
/// synthesizing *successful* commit and compensation inputs without
/// dispatching anything. That combination is a false-success footgun: it
/// performs real prepare gates, so it reads as the production executor, but
/// any engine driven by it mints receipts for effects that never happened.
/// Restricting it to gate evaluation removes the footgun at the type level —
/// the only consumer is [`PaneStepExecutor`], which delegates `evaluate_gates`
/// here and dispatches commit/compensation against real panes itself.
pub struct PolicyPrepareStepExecutor<P, A, T> {
    policy: P,
    approvals: A,
    targets: T,
    prepare_context: TxPrepareEvaluationContext,
}

impl<P, A, T> PolicyPrepareStepExecutor<P, A, T> {
    #[must_use]
    pub fn new(
        policy: P,
        approvals: A,
        targets: T,
        prepare_context: TxPrepareEvaluationContext,
    ) -> Self {
        Self {
            policy,
            approvals,
            targets,
            prepare_context,
        }
    }
}

impl<P, A, T> PolicyPrepareStepExecutor<P, A, T>
where
    P: TxPreparePolicyAuthorizer,
    A: TxPrepareApprovalChecker,
    T: TxPrepareTargetLookup,
{
    /// Evaluate the real prepare-phase gates for every step in `contract`.
    pub fn evaluate_gates(
        &self,
        contract: &MissionTxContract,
        now_ms: i64,
    ) -> Vec<TxPrepareGateInput> {
        crate::plan::mission_tx_prepare_gate_inputs(
            contract,
            &self.policy,
            &self.approvals,
            &self.targets,
            &self.prepare_context,
            now_ms,
        )
    }
}

// ── Storage Adapter (ft-2j3vo) ──────────────────────────────────────────────

/// Storage adapter trait for `StepAction::StoreData` execution and compensation (ft-2j3vo).
///
/// Implementations provide observable, atomic, and idempotent key-value mutation
/// for transaction steps.
pub trait TxStorageAdapter: Send + Sync {
    /// Store a JSON value under `key`. Overwrites any existing value at `key`.
    fn store_data(&self, key: &str, value: &serde_json::Value) -> Result<(), String>;

    /// Retrieve the JSON value stored under `key`, if present.
    fn get_data(&self, key: &str) -> Result<Option<serde_json::Value>, String>;

    /// Delete the value stored under `key`. Returns `Ok(true)` if deleted, `Ok(false)` if not found.
    fn delete_data(&self, key: &str) -> Result<bool, String>;

    /// Check if `key` exists in the storage backend.
    fn has_data(&self, key: &str) -> Result<bool, String> {
        self.get_data(key).map(|v| v.is_some())
    }
}

impl<T: TxStorageAdapter + ?Sized> TxStorageAdapter for std::sync::Arc<T> {
    fn store_data(&self, key: &str, value: &serde_json::Value) -> Result<(), String> {
        (**self).store_data(key, value)
    }

    fn get_data(&self, key: &str) -> Result<Option<serde_json::Value>, String> {
        (**self).get_data(key)
    }

    fn delete_data(&self, key: &str) -> Result<bool, String> {
        (**self).delete_data(key)
    }

    fn has_data(&self, key: &str) -> Result<bool, String> {
        (**self).has_data(key)
    }
}

/// In-memory storage adapter for `StepAction::StoreData` execution (ft-2j3vo).
/// Thread-safe and suitable for in-process testing and ephemeral transactions.
#[derive(Debug, Default, Clone)]
pub struct InMemoryTxStorageAdapter {
    entries:
        std::sync::Arc<std::sync::RwLock<std::collections::HashMap<String, serde_json::Value>>>,
}

impl InMemoryTxStorageAdapter {
    /// Create a new empty in-memory storage adapter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Retrieve a snapshot clone of all stored key-value pairs.
    #[must_use]
    pub fn snapshot(&self) -> std::collections::HashMap<String, serde_json::Value> {
        self.entries.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// Check the number of stored keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.read().map(|g| g.len()).unwrap_or(0)
    }

    /// Check if the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all stored entries.
    pub fn clear(&self) {
        if let Ok(mut guard) = self.entries.write() {
            guard.clear();
        }
    }
}

impl TxStorageAdapter for InMemoryTxStorageAdapter {
    fn store_data(&self, key: &str, value: &serde_json::Value) -> Result<(), String> {
        let mut guard = self
            .entries
            .write()
            .map_err(|e| format!("in-memory storage lock poisoned: {e}"))?;
        guard.insert(key.to_string(), value.clone());
        Ok(())
    }

    fn get_data(&self, key: &str) -> Result<Option<serde_json::Value>, String> {
        let guard = self
            .entries
            .read()
            .map_err(|e| format!("in-memory storage lock poisoned: {e}"))?;
        Ok(guard.get(key).cloned())
    }

    fn delete_data(&self, key: &str) -> Result<bool, String> {
        let mut guard = self
            .entries
            .write()
            .map_err(|e| format!("in-memory storage lock poisoned: {e}"))?;
        Ok(guard.remove(key).is_some())
    }
}

/// Durable directory-backed JSON file storage adapter for `StepAction::StoreData` (ft-2j3vo).
/// Writes are atomic (write to temp file, fsync, atomic rename).
#[derive(Debug, Clone)]
pub struct FileTxStorageAdapter {
    dir: std::path::PathBuf,
}

impl FileTxStorageAdapter {
    /// Create or open a directory-backed storage adapter at `dir`.
    pub fn new(dir: impl Into<std::path::PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Path to the storage directory.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.dir
    }

    fn validate_key(key: &str) -> Result<(), String> {
        if key.is_empty() {
            return Err("storage key cannot be empty".to_string());
        }
        if key.contains('/')
            || key.contains('\\')
            || key.contains('\0')
            || key.starts_with('.')
            || key == ".."
        {
            return Err(format!(
                "invalid storage key '{key}': illegal characters or path traversal"
            ));
        }
        Ok(())
    }

    fn key_path(&self, key: &str) -> std::path::PathBuf {
        self.dir.join(format!("{key}.json"))
    }
}

impl TxStorageAdapter for FileTxStorageAdapter {
    fn store_data(&self, key: &str, value: &serde_json::Value) -> Result<(), String> {
        Self::validate_key(key)?;
        let target = self.key_path(key);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let temp = self
            .dir
            .join(format!(".tmp_{}_{}_{}", key, std::process::id(), nanos));
        let bytes = serde_json::to_vec_pretty(value)
            .map_err(|e| format!("failed to serialize JSON value for '{key}': {e}"))?;

        {
            let mut f = std::fs::File::create(&temp)
                .map_err(|e| format!("failed to create temp file for '{key}': {e}"))?;
            use std::io::Write;
            f.write_all(&bytes)
                .map_err(|e| format!("failed to write data for '{key}': {e}"))?;
            f.sync_all()
                .map_err(|e| format!("failed to fsync temp file for '{key}': {e}"))?;
        }

        std::fs::rename(&temp, &target)
            .map_err(|e| format!("failed to atomically commit file for '{key}': {e}"))?;
        Ok(())
    }

    fn get_data(&self, key: &str) -> Result<Option<serde_json::Value>, String> {
        Self::validate_key(key)?;
        let target = self.key_path(key);
        if !target.exists() {
            return Ok(None);
        }
        let f = std::fs::File::open(&target)
            .map_err(|e| format!("failed to open file for '{key}': {e}"))?;
        let value = serde_json::from_reader(f)
            .map_err(|e| format!("failed to deserialize JSON value for '{key}': {e}"))?;
        Ok(Some(value))
    }

    fn delete_data(&self, key: &str) -> Result<bool, String> {
        Self::validate_key(key)?;
        let target = self.key_path(key);
        if !target.exists() {
            return Ok(false);
        }
        std::fs::remove_file(&target)
            .map_err(|e| format!("failed to remove file for '{key}': {e}"))?;
        Ok(true)
    }
}

// ── Pane Step Executor ──────────────────────────────────────────────────────

/// Configuration for `PaneStepExecutor` timeout and backpressure behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneStepExecutorConfig {
    /// Default timeout for `SendText` steps (ms). Defaults to 30_000.
    pub default_send_timeout_ms: u64,
    /// Phase-level timeout buffer added on top of aggregate step timeouts (ms). Defaults to 30_000.
    pub phase_timeout_buffer_ms: u64,
    /// Whether to check backpressure before each step. Defaults to true.
    pub backpressure_enabled: bool,
}

impl Default for PaneStepExecutorConfig {
    fn default() -> Self {
        Self {
            default_send_timeout_ms: 30_000,
            phase_timeout_buffer_ms: 30_000,
            backpressure_enabled: true,
        }
    }
}

/// Step executor that dispatches step operations to real panes via `WeztermInterface`.
///
/// - `evaluate_gates`: delegates to `PolicyPrepareStepExecutor` for real policy evaluation.
/// - `execute_steps`: dispatches `SendText`, `WaitFor`, `StoreData` etc. to real panes.
/// - `execute_compensations`: dispatches compensation actions to real panes.
///
/// Supports per-step timeouts, phase-level timeout budgets, and backpressure
/// integration with `FleetMemoryController`.
///
/// Uses `thread::spawn` + a fresh runtime internally so it can call async
/// `WeztermInterface` methods from the sync `StepExecutor` trait.
pub struct PaneStepExecutor<P, A, T> {
    handle: crate::wezterm::WeztermHandle,
    policy_executor: PolicyPrepareStepExecutor<P, A, T>,
    config: PaneStepExecutorConfig,
    fleet_controller: Option<std::sync::Arc<crate::fleet_memory_controller::FleetMemoryController>>,
    /// Optional external signal registry for `WaitCondition::External` (ft-wgc1q).
    /// Without one, External waits return an explicit unsupported error rather
    /// than the legacy pane-text-polling mock that aliased the signal key into
    /// the search pattern.
    external_signals: Option<std::sync::Arc<crate::workflows::ExternalSignalRegistry>>,
    /// Optional storage adapter for `StepAction::StoreData` (ft-2j3vo).
    /// Without one, StoreData returns an explicit unsupported error (`store_data_unwired`)
    /// and fails closed.
    storage_adapter: Option<std::sync::Arc<dyn TxStorageAdapter>>,
}

impl<P, A, T> PaneStepExecutor<P, A, T> {
    /// Create a new pane step executor.
    #[must_use]
    pub fn new(
        handle: crate::wezterm::WeztermHandle,
        policy: P,
        approvals: A,
        targets: T,
        prepare_context: TxPrepareEvaluationContext,
    ) -> Self {
        Self {
            handle,
            policy_executor: PolicyPrepareStepExecutor::new(
                policy,
                approvals,
                targets,
                prepare_context,
            ),
            config: PaneStepExecutorConfig::default(),
            fleet_controller: None,
            external_signals: None,
            storage_adapter: None,
        }
    }

    /// Set custom timeout/backpressure configuration.
    #[must_use]
    pub fn with_config(mut self, config: PaneStepExecutorConfig) -> Self {
        self.config = config;
        self
    }

    /// Attach a fleet memory controller for backpressure-aware execution.
    #[must_use]
    pub fn with_fleet_controller(
        mut self,
        controller: std::sync::Arc<crate::fleet_memory_controller::FleetMemoryController>,
    ) -> Self {
        self.fleet_controller = Some(controller);
        self
    }

    /// Attach an external signal registry consulted by `WaitCondition::External`
    /// (ft-wgc1q). Without a registry, External waits surface as an explicit
    /// unsupported error naming the signal key — never as the legacy
    /// pane-text-polling mock.
    #[must_use]
    pub fn with_external_signals(
        mut self,
        registry: std::sync::Arc<crate::workflows::ExternalSignalRegistry>,
    ) -> Self {
        self.external_signals = Some(registry);
        self
    }

    /// Attach a storage adapter for `StepAction::StoreData` (ft-2j3vo).
    #[must_use]
    pub fn with_storage_adapter(mut self, adapter: std::sync::Arc<dyn TxStorageAdapter>) -> Self {
        self.storage_adapter = Some(adapter);
        self
    }
}

/// Extract the step-level timeout from a `StepAction`. Returns `None` for non-I/O actions.
fn step_timeout_ms(action: &crate::plan::StepAction, default_send_ms: u64) -> Option<u64> {
    match action {
        crate::plan::StepAction::SendText { .. } => Some(default_send_ms),
        crate::plan::StepAction::WaitFor { timeout_ms, .. } => Some(*timeout_ms),
        _ => None, // Non-I/O actions (StoreData, AcquireLock, etc.) don't need timeouts
    }
}

/// Compute the phase-level timeout budget with saturating arithmetic.
///
/// Transaction plans and executor configuration are external inputs. A plain
/// `sum::<u64>() + buffer` can panic in debug builds or wrap in release builds,
/// turning an over-large budget into a tiny one.
#[must_use]
fn phase_timeout_budget_ms(
    steps: &[crate::plan::TxStep],
    default_send_ms: u64,
    phase_timeout_buffer_ms: u64,
) -> u64 {
    steps
        .iter()
        .filter_map(|s| step_timeout_ms(&s.action, default_send_ms))
        .fold(phase_timeout_buffer_ms, |acc, timeout| {
            acc.saturating_add(timeout)
        })
}

/// Check whether the given action targets a specific pane.
fn action_has_pane(action: &crate::plan::StepAction) -> bool {
    matches!(
        action,
        crate::plan::StepAction::SendText { .. } | crate::plan::StepAction::WaitFor { .. }
    )
}

/// Execute a single step action against the real backend (blocking).
///
/// Spawns a one-shot runtime for async calls. If `timeout_ms` is provided, wraps
/// the async operation in `runtime_async::timeout`. Returns `(success, reason_code, error_code)`.
fn execute_step_action(
    handle: &crate::wezterm::WeztermHandle,
    action: &crate::plan::StepAction,
    timeout_ms: Option<u64>,
    external_signals: Option<&crate::workflows::ExternalSignalRegistry>,
    storage_adapter: Option<&(dyn TxStorageAdapter + 'static)>,
) -> (bool, String, Option<String>) {
    let _ = timeout_ms; // Step-level timeout is already embedded in WaitFor's poll loop.
    // For SendText, the backend's own timeouts apply.

    // ft-506i5: capture the parent tx `Cx` on THIS (driver) thread, before any
    // `std::thread::spawn` below. `Cx::current()` is task-local and is NOT
    // carried into a freshly spawned OS thread, so the in-thread
    // `Cx::current()` always saw `None` and fell back to an unlinked
    // `for_request()` Cx — silently dropping the parent cancellation token,
    // deadline, and budget. Capturing here and threading a clone into each
    // spawned backend call + poll loop restores propagation so an operator
    // cancel or the parent deadline interrupts an in-flight step (within one
    // poll interval) instead of blocking on `join()` until `timeout_ms`.
    let parent_cx = crate::cx::Cx::current();

    match action {
        crate::plan::StepAction::SendText {
            pane_id,
            text,
            paste_mode,
        } => {
            let h = handle.clone();
            let pane_id = *pane_id;
            let text = text.clone();
            let no_paste = paste_mode.is_some_and(|pm| !pm);
            let thread_cx = parent_cx.clone();
            let result = match std::thread::Builder::new()
                .name("ft-tx-send-step".to_string())
                .spawn(move || {
                    let rt = crate::runtime_async::RuntimeBuilder::current_thread()
                        .build()
                        .map_err(|e| format!("failed to build runtime for pane step: {e}"))?;
                    rt.block_on(async {
                        // ft-xbnl0.2.3 tick 262: cx-first tx-execution send.
                        // ft-506i5: use the parent tx Cx captured on the driver
                        // thread (thread::spawn does not propagate the task-local
                        // Cx); fall back to a fresh request Cx only when there was
                        // no ambient cx to begin with.
                        let send_cx = thread_cx.unwrap_or_else(crate::cx::for_request);
                        let result = if no_paste {
                            h.send_text_no_paste_with_cx(&send_cx, pane_id, &text).await
                        } else {
                            h.send_text_with_cx(&send_cx, pane_id, &text).await
                        };
                        result.map_err(|e| e.to_string())
                    })
                }) {
                Ok(handle) => handle.join(),
                Err(e) => {
                    return (
                        false,
                        "send_text_thread_spawn_failed".to_string(),
                        Some(format!("FTX_THREAD: {e}")),
                    );
                }
            };
            match result {
                Ok(Ok(())) => (true, "send_text_succeeded".to_string(), None),
                Ok(Err(e)) => (
                    false,
                    "send_text_failed".to_string(),
                    Some(format!("FTX_SEND: {e}")),
                ),
                Err(_) => (
                    false,
                    "send_text_thread_panic".to_string(),
                    Some("FTX_PANIC".to_string()),
                ),
            }
        }
        crate::plan::StepAction::WaitFor {
            pane_id,
            condition,
            timeout_ms,
        } => {
            let timeout_val = *timeout_ms;
            let timeout = std::time::Duration::from_millis(timeout_val);

            // ft-wgc1q: route External waits through the registry instead of
            // aliasing the signal key into the pane-text search pattern.
            if let crate::plan::WaitCondition::External { key } = condition {
                let Some(registry) = external_signals else {
                    return (
                        false,
                        "wait_for_external_unsupported".to_string(),
                        Some(format!(
                            "FTX_WAIT_EXTERNAL_UNSUPPORTED: signal '{key}' requires registry; \
                             wire one via PaneStepExecutor::with_external_signals(registry)"
                        )),
                    );
                };
                let deadline = std::time::Instant::now();
                let Some(deadline) = deadline.checked_add(timeout) else {
                    return (
                        false,
                        "wait_for_timeout_overflow".to_string(),
                        Some(format!(
                            "FTX_WAIT: external timeout is too large: {timeout_val}ms"
                        )),
                    );
                };
                let mut interval = std::time::Duration::from_millis(5);
                let max_interval = std::time::Duration::from_millis(50);
                loop {
                    if registry.is_signaled(key) {
                        return (true, "wait_for_external_satisfied".to_string(), None);
                    }
                    // ft-506i5: this external wait runs on the driver thread, so
                    // the captured `parent_cx` is the live tx Cx. Honor
                    // cancellation / deadline / budget each iteration rather than
                    // blocking until `timeout_ms`. (No ambient cx -> no gate, same
                    // as before.)
                    if parent_cx
                        .as_ref()
                        .is_some_and(|cx| cx.checkpoint().is_err())
                    {
                        return (
                            false,
                            "wait_for_cancelled".to_string(),
                            Some(format!(
                                "FTX_WAIT_CANCELLED: external signal '{key}' wait interrupted by tx cancellation or deadline"
                            )),
                        );
                    }
                    let now = std::time::Instant::now();
                    if now >= deadline {
                        return (
                            false,
                            "wait_for_timeout".to_string(),
                            Some(format!(
                                "FTX_WAIT: external signal '{key}' not fired within {timeout_val}ms"
                            )),
                        );
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    let chunk = interval.min(remaining);
                    if !chunk.is_zero() {
                        std::thread::sleep(chunk);
                    }
                    interval = interval.saturating_mul(2).min(max_interval);
                }
            }

            let _ = pane_id;
            match condition {
                crate::plan::WaitCondition::Pattern { .. } => (
                    false,
                    "wait_for_pattern_registry_unwired".to_string(),
                    Some(
                        "FTX_WAIT_PATTERN_REGISTRY_UNWIRED: Pattern carries a rule_id and cannot be evaluated as a literal substring; wire the transaction executor to the pattern registry before enabling this action"
                            .to_string(),
                    ),
                ),
                crate::plan::WaitCondition::PaneIdle { .. } => (
                    false,
                    "wait_for_pane_idle_unwired".to_string(),
                    Some(
                        "FTX_WAIT_PANE_IDLE_UNWIRED: PaneIdle requires a real pane-activity provider"
                            .to_string(),
                    ),
                ),
                crate::plan::WaitCondition::StableTail { .. } => (
                    false,
                    "wait_for_stable_tail_unwired".to_string(),
                    Some(
                        "FTX_WAIT_STABLE_TAIL_UNWIRED: StableTail requires a real tail-stability provider"
                            .to_string(),
                    ),
                ),
                crate::plan::WaitCondition::External { .. } => {
                    unreachable!("external wait returns from its registry loop")
                }
            }
        }
        crate::plan::StepAction::StoreData { key, value } => {
            let Some(storage) = storage_adapter else {
                return (
                    false,
                    "store_data_unwired".to_string(),
                    Some(
                        "FTX_STORE_DATA_UNWIRED: StoreData requires a durable storage adapter; wire one via PaneStepExecutor::with_storage_adapter(adapter)"
                            .to_string(),
                    ),
                );
            };
            match storage.store_data(key, value) {
                Ok(()) => (true, "store_data_succeeded".to_string(), None),
                Err(e) => (
                    false,
                    "store_data_failed".to_string(),
                    Some(format!("FTX_STORE_DATA_FAILED: {e}")),
                ),
            }
        }
        crate::plan::StepAction::AcquireLock { .. } => (
            false,
            "acquire_lock_unwired".to_string(),
            Some(
                "FTX_ACQUIRE_LOCK_UNWIRED: AcquireLock requires a real lock provider; no lock was acquired"
                    .to_string(),
            ),
        ),
        crate::plan::StepAction::ReleaseLock { .. } => (
            false,
            "release_lock_unwired".to_string(),
            Some(
                "FTX_RELEASE_LOCK_UNWIRED: ReleaseLock requires a real lock provider; no lock was released"
                    .to_string(),
            ),
        ),
        crate::plan::StepAction::MarkEventHandled { .. } => (
            false,
            "mark_event_handled_unwired".to_string(),
            Some(
                "FTX_MARK_EVENT_HANDLED_UNWIRED: MarkEventHandled requires an event-state adapter; no event was changed"
                    .to_string(),
            ),
        ),
        crate::plan::StepAction::ValidateApproval { .. } => (
            false,
            "validate_approval_unwired".to_string(),
            Some(
                "FTX_APPROVAL_UNWIRED: ValidateApproval cannot succeed without scoped approval \
                 consumption; rely on prepare approval gates until a scoped consume path is wired"
                    .to_string(),
            ),
        ),
        crate::plan::StepAction::RunWorkflow { workflow_id, .. } => (
            false,
            "unsupported_action".to_string(),
            Some(format!("FTX_UNSUPPORTED: RunWorkflow({workflow_id})")),
        ),
        crate::plan::StepAction::NestedPlan { .. } => (
            false,
            "unsupported_action".to_string(),
            Some("FTX_UNSUPPORTED: NestedPlan".to_string()),
        ),
        crate::plan::StepAction::Custom {
            action_type,
            payload,
        } => {
            if action_type == "delete_data" || action_type == "delete_store_data" {
                let Some(storage) = storage_adapter else {
                    return (
                        false,
                        "delete_data_unwired".to_string(),
                        Some(
                            "FTX_DELETE_DATA_UNWIRED: delete_data requires a durable storage adapter; wire one via PaneStepExecutor::with_storage_adapter(adapter)"
                                .to_string(),
                        ),
                    );
                };
                let key = payload.get("key").and_then(|k| k.as_str()).unwrap_or("");
                if key.is_empty() {
                    return (
                        false,
                        "delete_data_invalid_key".to_string(),
                        Some("FTX_DELETE_DATA_INVALID_KEY: missing or empty 'key' in payload".to_string()),
                    );
                }
                match storage.delete_data(key) {
                    Ok(_) => (true, "delete_data_succeeded".to_string(), None),
                    Err(e) => (
                        false,
                        "delete_data_failed".to_string(),
                        Some(format!("FTX_DELETE_DATA_FAILED: {e}")),
                    ),
                }
            } else {
                (
                    false,
                    "unsupported_action".to_string(),
                    Some(format!("FTX_UNSUPPORTED: Custom({action_type})")),
                )
            }
        }
    }
}

impl<P, A, T> StepExecutor for PaneStepExecutor<P, A, T>
where
    P: TxPreparePolicyAuthorizer,
    A: TxPrepareApprovalChecker,
    T: TxPrepareTargetLookup,
{
    fn evaluate_gates(&self, contract: &MissionTxContract, now_ms: i64) -> Vec<TxPrepareGateInput> {
        self.policy_executor.evaluate_gates(contract, now_ms)
    }

    fn execute_steps(
        &self,
        contract: &MissionTxContract,
        fail_step: Option<&str>,
        now_ms: i64,
    ) -> Vec<TxCommitStepInput> {
        let mut results = Vec::with_capacity(contract.plan.steps.len());
        let mut had_failure = false;

        // Phase-level timeout: sum of step timeouts + buffer.
        let phase_budget_ms = phase_timeout_budget_ms(
            &contract.plan.steps,
            self.config.default_send_timeout_ms,
            self.config.phase_timeout_buffer_ms,
        );
        let phase_budget = std::time::Duration::from_millis(phase_budget_ms);
        let phase_start = std::time::Instant::now();

        for step in &contract.plan.steps {
            // Deterministic failure injection
            if fail_step == Some(step.step_id.0.as_str()) {
                tracing::warn!(step_id = %step.step_id.0, "injecting deterministic failure");
                results.push(TxCommitStepInput {
                    step_id: step.step_id.clone(),
                    success: false,
                    reason_code: "commit_step_failed_injected".to_string(),
                    error_code: Some("FTX3999".to_string()),
                    completed_at_ms: now_ms,
                });
                had_failure = true;
                continue;
            }

            // Stop executing after first failure (failure boundary)
            if had_failure {
                results.push(TxCommitStepInput {
                    step_id: step.step_id.clone(),
                    success: false,
                    reason_code: "skipped_after_failure".to_string(),
                    error_code: Some("FTX_SKIPPED".to_string()),
                    completed_at_ms: now_ms,
                });
                continue;
            }

            // Phase-level timeout check
            let elapsed = phase_start.elapsed();
            if elapsed >= phase_budget {
                let remaining = contract.plan.steps.len() - results.len();
                tracing::error!(
                    elapsed_ms = elapsed.as_millis() as u64,
                    remaining_steps = remaining,
                    "phase timeout exceeded, skipping remaining steps"
                );
                results.push(TxCommitStepInput {
                    step_id: step.step_id.clone(),
                    success: false,
                    reason_code: "phase_timeout".to_string(),
                    error_code: Some(format!(
                        "FTX_PHASE_TIMEOUT: elapsed {}ms exceeds budget {}ms",
                        elapsed.as_millis(),
                        phase_budget.as_millis()
                    )),
                    completed_at_ms: now_ms,
                });
                had_failure = true;
                continue;
            }

            // Backpressure check
            if self.config.backpressure_enabled {
                if let Some(ref controller) = self.fleet_controller {
                    use crate::fleet_memory_controller::FleetPressureTier;
                    let tier = controller.compound_tier();
                    match tier {
                        FleetPressureTier::Normal => {}
                        FleetPressureTier::Elevated => {
                            tracing::warn!(
                                step_id = %step.step_id.0,
                                tier = ?tier,
                                "elevated backpressure — proceeding with caution"
                            );
                        }
                        FleetPressureTier::Critical => {
                            if !action_has_pane(&step.action) {
                                tracing::warn!(
                                    step_id = %step.step_id.0,
                                    tier = ?tier,
                                    "critical backpressure — deferring non-pane step"
                                );
                                results.push(TxCommitStepInput {
                                    step_id: step.step_id.clone(),
                                    success: false,
                                    reason_code: "backpressure_deferred".to_string(),
                                    error_code: Some("FTX_BACKPRESSURE_CRITICAL".to_string()),
                                    completed_at_ms: now_ms,
                                });
                                had_failure = true;
                                continue;
                            }
                        }
                        FleetPressureTier::Emergency => {
                            tracing::error!(
                                step_id = %step.step_id.0,
                                tier = ?tier,
                                "emergency backpressure — deferring all steps"
                            );
                            results.push(TxCommitStepInput {
                                step_id: step.step_id.clone(),
                                success: false,
                                reason_code: "backpressure_emergency".to_string(),
                                error_code: Some("FTX_BACKPRESSURE_EMERGENCY".to_string()),
                                completed_at_ms: now_ms,
                            });
                            had_failure = true;
                            continue;
                        }
                    }
                }
            }

            let step_timeout = step_timeout_ms(&step.action, self.config.default_send_timeout_ms);

            tracing::info!(
                step_id = %step.step_id.0,
                action = ?std::mem::discriminant(&step.action),
                timeout_ms = ?step_timeout,
                "executing pane step"
            );

            let (success, reason_code, error_code) = execute_step_action(
                &self.handle,
                &step.action,
                step_timeout,
                self.external_signals.as_deref(),
                self.storage_adapter.as_deref(),
            );

            tracing::info!(
                step_id = %step.step_id.0,
                success,
                reason = %reason_code,
                "pane step completed"
            );

            if !success {
                had_failure = true;
            }
            results.push(TxCommitStepInput {
                step_id: step.step_id.clone(),
                success,
                reason_code,
                error_code,
                completed_at_ms: now_ms,
            });
        }

        results
    }

    fn execute_compensations(
        &self,
        contract: &MissionTxContract,
        commit_report: &TxCommitReport,
        fail_for_step: Option<&str>,
        now_ms: i64,
    ) -> Vec<TxCompensationStepInput> {
        let contract_compensations = contract
            .plan
            .compensations
            .iter()
            .map(|comp| (comp.for_step_id.0.as_str(), &comp.action))
            .collect::<HashMap<_, _>>();

        let mut inputs = Vec::new();
        for result in commit_report
            .step_results
            .iter()
            .rev()
            .filter(|result| result.outcome.is_committed())
        {
            let input = if fail_for_step == Some(result.step_id.0.as_str()) {
                tracing::warn!(
                    step_id = %result.step_id.0,
                    "injecting deterministic compensation failure"
                );
                TxCompensationStepInput {
                    for_step_id: result.step_id.clone(),
                    success: false,
                    reason_code: "compensation_failed_injected".to_string(),
                    error_code: Some("FTX4999".to_string()),
                    completed_at_ms: now_ms,
                }
            } else {
                tracing::info!(step_id = %result.step_id.0, "executing pane compensation");
                let Some(action) = contract_compensations.get(result.step_id.0.as_str()) else {
                    inputs.push(TxCompensationStepInput {
                        for_step_id: result.step_id.clone(),
                        success: false,
                        reason_code: "compensation_action_missing".to_string(),
                        error_code: Some("FTX_COMPENSATION_MISSING".to_string()),
                        completed_at_ms: now_ms,
                    });
                    break;
                };

                let step_timeout = step_timeout_ms(action, self.config.default_send_timeout_ms);
                let (success, reason_code, error_code) = execute_step_action(
                    &self.handle,
                    action,
                    step_timeout,
                    self.external_signals.as_deref(),
                    self.storage_adapter.as_deref(),
                );

                TxCompensationStepInput {
                    for_step_id: result.step_id.clone(),
                    success,
                    reason_code,
                    error_code,
                    completed_at_ms: now_ms,
                }
            };
            let success = input.success;
            inputs.push(input);
            if !success {
                break;
            }
        }
        inputs
    }
}

// ── Execution Result ─────────────────────────────────────────────────────────

/// Complete result from a tx execution run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxExecutionResult {
    /// Final lifecycle state of the contract.
    pub final_state: MissionTxState,
    /// Final transaction outcome.
    pub outcome: TxOutcome,
    /// Prepare phase report.
    pub prepare_report: TxPrepareReport,
    /// Commit phase report (None if prepare was denied/deferred).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_report: Option<TxCommitReport>,
    /// Compensation report (None if no compensation was needed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensation_report: Option<TxCompensationReport>,
    /// Observability events emitted during execution.
    pub events: Vec<TxObservabilityEvent>,
    /// The execution ledger.
    pub ledger: TxExecutionLedger,
    /// Forensic bundle (None if not requested).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forensic_bundle: Option<TxForensicBundle>,
    /// Decision path trace for the overall execution.
    pub decision_path: String,
    /// Reason code summarizing the execution.
    pub reason_code: String,
}

/// Complete result from an explicit transaction rollback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxRollbackExecutionResult {
    /// Compensation report persisted into the authoritative contract.
    pub compensation_report: TxCompensationReport,
    /// Observability events emitted while gating and compensating.
    pub events: Vec<TxObservabilityEvent>,
    /// Execution ledger, including its terminal phase and hash chain.
    pub ledger: TxExecutionLedger,
    /// Decision-path trace for the rollback operation.
    pub decision_path: String,
    /// Globally unique, timestamp-sortable rollback execution identifier.
    pub execution_id: String,
}

/// Canonical overall outcome represented by a transaction lifecycle state.
#[must_use]
pub fn tx_outcome_for_state(state: MissionTxState) -> TxOutcome {
    match state {
        MissionTxState::Committed => TxOutcome::Committed,
        MissionTxState::Failed => TxOutcome::Failed,
        MissionTxState::Compensated | MissionTxState::RolledBack => TxOutcome::Compensated,
        MissionTxState::Draft
        | MissionTxState::Planned
        | MissionTxState::Prepared
        | MissionTxState::Committing
        | MissionTxState::Compensating => TxOutcome::Pending,
    }
}

fn validate_tx_contract_state_outcome(contract: &MissionTxContract) -> Result<(), String> {
    let expected = tx_outcome_for_state(contract.lifecycle_state);
    if contract.outcome != expected {
        return Err(format!(
            "transaction state {} requires outcome {:?}, got {:?}",
            contract.lifecycle_state, expected, contract.outcome
        ));
    }
    Ok(())
}

fn unique_execution_id(kind: &str, now_ms: i64) -> String {
    let nonce = TX_EXECUTION_NONCE.fetch_add(1, Ordering::Relaxed);
    let process_time_nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!(
        "txe-{now_ms:020}-{process_time_nonce:032x}-{:010}-{nonce:016x}-{kind}",
        std::process::id()
    )
}

fn commit_idempotency_key(
    contract: &MissionTxContract,
    step_id: &str,
) -> Result<IdempotencyKey, TxExecutionError> {
    let step = contract
        .plan
        .steps
        .iter()
        .find(|step| step.step_id.0 == step_id)
        .ok_or_else(|| {
            TxExecutionError::InvalidContract(format!(
                "transaction plan has no commit step {step_id}"
            ))
        })?;
    let fingerprint = format!(
        "commit:{}:{}",
        contract.compute_hash(),
        step.action.canonical_string()
    );
    Ok(IdempotencyKey::new(
        &contract.plan.plan_id.0,
        step_id,
        &fingerprint,
    ))
}

fn compensation_idempotency_key(
    contract: &MissionTxContract,
    step_id: &str,
) -> Result<IdempotencyKey, TxExecutionError> {
    let compensation = contract
        .plan
        .compensations
        .iter()
        .find(|compensation| compensation.for_step_id.0 == step_id)
        .ok_or_else(|| {
            TxExecutionError::InvalidContract(format!(
                "transaction plan has no compensation action for committed step {step_id}"
            ))
        })?;
    let fingerprint = format!(
        "rollback:{}:{}",
        contract.compute_hash(),
        compensation.action.canonical_string()
    );
    Ok(IdempotencyKey::for_compensation(
        &contract.plan.plan_id.0,
        step_id,
        &fingerprint,
    ))
}

fn rollback_proof_idempotency_keys(
    contract: &MissionTxContract,
    commit_report: &TxCommitReport,
) -> Result<Vec<IdempotencyKey>, TxExecutionError> {
    let mut keys = contract
        .plan
        .steps
        .iter()
        .map(|step| commit_idempotency_key(contract, &step.step_id.0))
        .collect::<Result<Vec<_>, _>>()?;
    for step_result in commit_report.step_results.iter().filter(|result| {
        result.outcome.is_committed()
            || matches!(
                &result.outcome,
                crate::plan::TxCommitStepOutcome::Skipped { reason_code }
                    if reason_code == "already_compensated"
            )
    }) {
        keys.push(compensation_idempotency_key(
            contract,
            &step_result.step_id.0,
        )?);
    }
    Ok(keys)
}

fn classify_rollback_proof_lease_error(error: IdempotencyError) -> TxExecutionError {
    match error {
        IdempotencyError::LedgerIndexCorrupt { reason } => TxExecutionError::RollbackProof {
            kind: RollbackProofKind::Conflict,
            step_id: "durable-proof-set".to_string(),
            detail: format!(
                "authoritative durable proof index is contradictory: {reason}; reconcile the ledger before rollback"
            ),
        },
        IdempotencyError::ReservationInProgress { key } => TxExecutionError::InProgress(format!(
            "another transaction mutation currently owns durable proof key {key}"
        )),
        other => TxExecutionError::LedgerWrite(format!(
            "failed to acquire atomic rollback proof leases before mutation: {other}"
        )),
    }
}

fn validate_execution_recovery_entry(
    contract: &MissionTxContract,
    store: Option<&IdempotencyStore>,
) -> Result<(), TxExecutionError> {
    if !matches!(
        contract.lifecycle_state,
        MissionTxState::Planned | MissionTxState::Prepared | MissionTxState::Committing
    ) {
        return Err(TxExecutionError::InvalidContract(format!(
            "transaction execution requires planned or recoverable prepared/committing state; got {}",
            contract.lifecycle_state
        )));
    }
    if contract.lifecycle_state != MissionTxState::Planned
        && store.is_none_or(|store| !store.is_durable())
    {
        return Err(TxExecutionError::InvalidContract(format!(
            "interrupted {} transaction requires a durable idempotency store",
            contract.lifecycle_state
        )));
    }
    if contract.receipts.iter().any(|receipt| {
        receipt.get("phase").and_then(serde_json::Value::as_str) == Some("compensate")
    }) {
        return Err(TxExecutionError::InvalidContract(
            "commit recovery cannot run a contract that already contains compensation receipts"
                .to_string(),
        ));
    }

    for receipt in contract.receipts.iter().filter(|receipt| {
        receipt.get("phase").and_then(serde_json::Value::as_str) == Some("commit")
            && receipt.get("outcome").and_then(serde_json::Value::as_str) == Some("committed")
    }) {
        let step_id = receipt
            .get("step_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                TxExecutionError::InvalidContract(
                    "committed recovery receipt is missing step_id".to_string(),
                )
            })?;
        commit_idempotency_key(contract, step_id)?;
        let Some(store) = store else {
            return Err(TxExecutionError::InvalidContract(format!(
                "commit recovery for previously committed step {step_id} requires durable idempotency proof"
            )));
        };
        if !store.is_durable() {
            return Err(TxExecutionError::InvalidContract(format!(
                "commit recovery for previously committed step {step_id} requires a durable idempotency store"
            )));
        }
        // Exact proof is checked later by run_prepare_phase after the new
        // execution ledger exists, using the reservation-backed live spool
        // refresh. The bounded advisory cache cannot prove absence here.
    }
    Ok(())
}

fn checked_logical_now_ms(now_ms: i64) -> Result<u64, TxExecutionError> {
    u64::try_from(now_ms).map_err(|_| {
        TxExecutionError::InvalidContract(
            "transaction execution timestamp must be non-negative".to_string(),
        )
    })
}

fn validate_receipt_sequence_capacity(
    contract: &MissionTxContract,
    additional_receipts: usize,
) -> Result<(), TxExecutionError> {
    let last_seq = contract
        .receipts
        .iter()
        .rev()
        .find(|receipt| receipt.get("phase").is_some())
        .and_then(|receipt| receipt.get("seq"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let additional = u64::try_from(additional_receipts).map_err(|_| {
        TxExecutionError::InvalidContract(
            "transaction receipt count cannot be represented as u64".to_string(),
        )
    })?;
    last_seq.checked_add(additional).ok_or_else(|| {
        TxExecutionError::InvalidContract(format!(
            "transaction receipt sequence has insufficient headroom after seq {last_seq} for {additional_receipts} possible receipts"
        ))
    })?;
    Ok(())
}

fn latest_tx_receipt_matches(
    contract: &MissionTxContract,
    phase: &str,
    step_id: &str,
    outcome: &str,
) -> bool {
    contract
        .receipts
        .iter()
        .rev()
        .find(|receipt| {
            receipt.get("phase").and_then(serde_json::Value::as_str) == Some(phase)
                && receipt.get("step_id").and_then(serde_json::Value::as_str) == Some(step_id)
        })
        .is_some_and(|receipt| {
            receipt.get("outcome").and_then(serde_json::Value::as_str) == Some(outcome)
        })
}

fn last_tx_receipt_sequence(contract: &MissionTxContract) -> u64 {
    contract
        .receipts
        .iter()
        .rev()
        .find(|receipt| receipt.get("phase").is_some())
        .and_then(|receipt| receipt.get("seq"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn recovered_commit_report(
    contract: &MissionTxContract,
    durable_success_step_ids: &HashSet<String>,
    unresolved_reason_code: &str,
    now_ms: i64,
) -> Result<TxCommitReport, TxExecutionError> {
    if durable_success_step_ids.is_empty() {
        return Err(TxExecutionError::CommitPhase(
            "durable commit recovery report requires at least one proven success".to_string(),
        ));
    }

    let mut next_sequence = last_tx_receipt_sequence(contract);
    let mut step_results = Vec::with_capacity(contract.plan.steps.len());
    let mut receipts = Vec::new();
    let mut committed_count = 0usize;
    let mut skipped_count = 0usize;
    let mut failure_boundary = None;

    for step in &contract.plan.steps {
        let (outcome, receipt_outcome, reason_code, decision_path) =
            if durable_success_step_ids.contains(&step.step_id.0) {
                committed_count += 1;
                (
                    crate::plan::TxCommitStepOutcome::Committed {
                        reason_code: "commit_step_recovered_durable_success".to_string(),
                    },
                    "committed",
                    "commit_step_recovered_durable_success",
                    "commit_phase->recovered_durable_success",
                )
            } else {
                skipped_count += 1;
                failure_boundary.get_or_insert_with(|| step.step_id.0.clone());
                (
                    crate::plan::TxCommitStepOutcome::Skipped {
                        reason_code: unresolved_reason_code.to_string(),
                    },
                    "skipped",
                    unresolved_reason_code,
                    "commit_phase->recovery_unresolved",
                )
            };

        step_results.push(crate::plan::TxCommitStepResult {
            step_id: step.step_id.clone(),
            ordinal: step.ordinal,
            outcome,
            decision_path: decision_path.to_string(),
            completed_at_ms: now_ms,
        });

        // A prior buggy recovery may have appended a later skipped receipt for
        // a durably successful step. Suppress a new receipt only when the
        // latest receipt for this phase/step already carries the same outcome.
        if !latest_tx_receipt_matches(contract, "commit", &step.step_id.0, receipt_outcome) {
            next_sequence = next_sequence.checked_add(1).ok_or_else(|| {
                TxExecutionError::CommitPhase(format!(
                    "transaction receipt sequence overflow after seq {next_sequence}"
                ))
            })?;
            receipts.push(serde_json::json!({
                "seq": next_sequence,
                "phase": "commit",
                "tx_id": contract.intent.tx_id.0,
                "plan_id": contract.plan.plan_id.0,
                "state": MissionTxState::Committing,
                "step_id": step.step_id.0,
                "outcome": receipt_outcome,
                "reason_code": reason_code,
                "error_code": null,
                "decision_path": decision_path,
                "emitted_at_ms": now_ms,
            }));
        }
    }

    Ok(TxCommitReport {
        tx_id: contract.intent.tx_id.clone(),
        plan_id: contract.plan.plan_id.clone(),
        outcome: if skipped_count == 0 {
            TxCommitOutcome::FullyCommitted
        } else {
            TxCommitOutcome::PartialFailure
        },
        step_results,
        failure_boundary,
        committed_count,
        failed_count: 0,
        skipped_count,
        decision_path: "commit_phase->durable_recovery".to_string(),
        reason_code: "durable_commit_recovery_requires_compensation".to_string(),
        error_code: None,
        completed_at_ms: now_ms,
        receipts,
    })
}

fn recovered_commit_success_report(
    contract: &MissionTxContract,
    durable_success_step_ids: &HashSet<String>,
    now_ms: i64,
) -> Result<TxCommitReport, TxExecutionError> {
    if durable_success_step_ids.is_empty() {
        return Err(TxExecutionError::CommitPhase(
            "durable commit success reconciliation requires at least one proven success"
                .to_string(),
        ));
    }

    let mut next_sequence = last_tx_receipt_sequence(contract);
    let mut step_results = Vec::with_capacity(durable_success_step_ids.len());
    let mut receipts = Vec::new();

    for step in &contract.plan.steps {
        if !durable_success_step_ids.contains(&step.step_id.0) {
            continue;
        }

        step_results.push(crate::plan::TxCommitStepResult {
            step_id: step.step_id.clone(),
            ordinal: step.ordinal,
            outcome: crate::plan::TxCommitStepOutcome::Committed {
                reason_code: "commit_step_recovered_durable_success".to_string(),
            },
            decision_path: "commit_phase->recovered_durable_success".to_string(),
            completed_at_ms: now_ms,
        });

        if !latest_tx_receipt_matches(contract, "commit", &step.step_id.0, "committed") {
            next_sequence = next_sequence.checked_add(1).ok_or_else(|| {
                TxExecutionError::CommitPhase(format!(
                    "transaction receipt sequence overflow after seq {next_sequence}"
                ))
            })?;
            receipts.push(serde_json::json!({
                "seq": next_sequence,
                "phase": "commit",
                "tx_id": contract.intent.tx_id.0,
                "plan_id": contract.plan.plan_id.0,
                "state": MissionTxState::Committing,
                "step_id": step.step_id.0,
                "outcome": "committed",
                "reason_code": "commit_step_recovered_durable_success",
                "error_code": null,
                "decision_path": "commit_phase->recovered_durable_success",
                "emitted_at_ms": now_ms,
            }));
        }
    }

    let committed_count = step_results.len();
    Ok(TxCommitReport {
        tx_id: contract.intent.tx_id.clone(),
        plan_id: contract.plan.plan_id.clone(),
        outcome: if committed_count == contract.plan.steps.len() {
            TxCommitOutcome::FullyCommitted
        } else {
            TxCommitOutcome::PartialFailure
        },
        step_results,
        failure_boundary: None,
        committed_count,
        failed_count: 0,
        skipped_count: 0,
        decision_path: "commit_phase->durable_success_reconciliation".to_string(),
        reason_code: "durable_commit_success_reconciled".to_string(),
        error_code: None,
        completed_at_ms: now_ms,
        receipts,
    })
}

fn reconcile_before_ambiguous_retirement(
    contract: &mut MissionTxContract,
    store: &mut IdempotencyStore,
    prior_execution_id: &str,
    now_ms: i64,
    retirement_error: &crate::tx_idempotency::IdempotencyError,
) -> TxExecutionError {
    let logical_now_ms = match checked_logical_now_ms(now_ms) {
        Ok(now_ms) => now_ms,
        Err(err) => return err,
    };
    let mut durable_success_step_ids = HashSet::new();
    let mut first_conflict = None;

    for step in &contract.plan.steps {
        let idem_key = match commit_idempotency_key(contract, &step.step_id.0) {
            Ok(key) => key,
            Err(err) => return err,
        };
        let observed_outcome = match store.acquire_durable_reservation(
            prior_execution_id,
            &idem_key,
            logical_now_ms,
        ) {
            Ok(reservation) => reservation.observed_outcome().cloned(),
            Err(err) => {
                return TxExecutionError::LedgerWrite(format!(
                    "failed to classify ambiguous prior execution {prior_execution_id} for step {} after retirement was rejected: {err}",
                    step.step_id.0
                ));
            }
        };
        match observed_outcome {
            Some(StepOutcome::Success { .. }) => {
                durable_success_step_ids.insert(step.step_id.0.clone());
            }
            Some(other) if first_conflict.is_none() => {
                first_conflict = Some((step.step_id.0.clone(), format!("{other:?}")));
            }
            Some(_) => {}
            None => {}
        }
    }

    if !durable_success_step_ids.is_empty() {
        let reconciliation =
            match recovered_commit_success_report(contract, &durable_success_step_ids, now_ms) {
                Ok(report) => report,
                Err(err) => return err,
            };
        contract
            .receipts
            .extend(reconciliation.receipts.iter().cloned());
    }

    first_conflict.map_or_else(
        || {
            TxExecutionError::LedgerWrite(format!(
                "failed to retire prior execution ledgers for plan {}: {retirement_error}",
                contract.plan.plan_id.0
            ))
        },
        |(step_id, outcome)| TxExecutionError::DedupConflict { step_id, outcome },
    )
}

fn apply_compensation_report(contract: &mut MissionTxContract, report: &TxCompensationReport) {
    contract.receipts.extend(report.receipts.iter().cloned());
    let final_state = report.outcome.target_tx_state();
    contract.lifecycle_state = final_state;
    contract.outcome = tx_outcome_for_state(final_state);
}

fn archive_terminal_execution_ledger(
    ledger: &mut TxExecutionLedger,
    store: Option<&mut IdempotencyStore>,
    execution_id: &str,
) -> Result<(), TxExecutionError> {
    if !ledger.phase().is_terminal() {
        return Ok(());
    }
    let Some(store) = store else {
        return Ok(());
    };

    // The durable store keeps the terminal spool file as restart dedup proof;
    // archival only releases the in-memory active-ledger budget. Use the
    // store's returned snapshot for the public result so the two copies cannot
    // silently diverge.
    *ledger = store.archive_ledger(execution_id).map_err(|err| {
        TxExecutionError::LedgerWrite(format!(
            "failed to archive terminal ledger {execution_id}: {err}"
        ))
    })?;
    Ok(())
}

// ── Engine ───────────────────────────────────────────────────────────────────

/// The tx execution engine orchestrates the full lifecycle of a mission transaction.
///
/// Given a `MissionTxContract` and a `StepExecutor`, it runs:
/// 1. **Prepare**: Evaluate gates (policy, reservation, approval, liveness)
/// 2. **Commit**: Execute steps in plan order with failure boundary semantics
/// 3. **Compensate**: Roll back committed steps on partial failure
///
/// Each phase transition is recorded in the idempotency ledger and emits
/// structured observability events.
pub struct TxExecutionEngine<E: StepExecutor> {
    executor: E,
    config: TxExecutionConfig,
    event_seq: std::cell::Cell<u64>,
}

/// One-shot proof that every compensation effect which may dispatch passed
/// the complete prepare-gate set before transaction state was mutated.
///
/// This private value is load-bearing: `run_compensation_phase` cannot be
/// called without a permit, which prevents future entry paths from silently
/// bypassing compensation policy/approval/reservation/liveness gates. Gate
/// events are retained here until the compensation runner begins, then emitted
/// before `CompensationStarted` so vector order remains monotonic by sequence.
struct CompensationDispatchPermit {
    execution_id: String,
    plan_id: String,
    contract_hash: String,
    step_ids: HashSet<String>,
    gate_events: Vec<TxObservabilityEvent>,
}

impl CompensationDispatchPermit {
    fn authorizes(&self, step_id: &str) -> bool {
        self.step_ids.contains(step_id)
    }
}

type CompensationPhaseOutput = (
    TxCompensationReport,
    HashMap<String, IdempotencyKey>,
    HashMap<String, StepOutcome>,
    HashMap<String, StepOutcome>,
);

type CompensationInputsWithDedupOutput = (
    Vec<TxCompensationStepInput>,
    HashMap<String, IdempotencyKey>,
    HashMap<String, StepOutcome>,
    HashMap<String, StepOutcome>,
);

struct TxLedgerRecordingContext<'a> {
    execution_id: &'a str,
    ledger: &'a mut TxExecutionLedger,
    store: Option<&'a mut IdempotencyStore>,
    authoritative_recovery_outcomes: Option<&'a HashMap<String, StepOutcome>>,
    events: &'a mut Vec<TxObservabilityEvent>,
    now_ms: i64,
}

/// Storeless entrypoints — simulation only.
///
/// These run the full lifecycle **without** a durable idempotency spool: no
/// write-ahead `Pending` record, no per-key or execution locks, no durable
/// replay proof, and no crash reconciliation. That is sound only for an
/// executor that dispatches nothing externally, so the bound is
/// [`NonEffectfulStepExecutor`] rather than [`StepExecutor`]
/// (ft-3lqyu / ft-0rlfq.8). Effectful executors — [`PaneStepExecutor`] above
/// all — must use [`TxExecutionEngine::execute_with_store`],
/// [`TxExecutionEngine::rollback_with_store`], or
/// [`TxExecutionEngine::resume`].
impl<E: NonEffectfulStepExecutor> TxExecutionEngine<E> {
    /// Simulate the full tx lifecycle on the given contract.
    ///
    /// Receipts produced here prove contract/lifecycle bookkeeping only; they
    /// are not evidence that any external effect occurred.
    ///
    /// # Errors
    ///
    /// Returns an error if the contract is invalid or a phase transition fails.
    pub fn execute(
        &self,
        contract: &mut MissionTxContract,
        now_ms: i64,
    ) -> Result<TxExecutionResult, TxExecutionError> {
        self.execute_inner(contract, now_ms, None)
    }

    /// Simulate an explicit rollback of the given contract.
    ///
    /// The contract is mutated with compensation receipts and its terminal
    /// lifecycle/outcome before this method returns, but no compensation
    /// effect is dispatched.
    ///
    /// # Errors
    ///
    /// Returns an error if committed work cannot be proven from receipts, the
    /// contract is already terminal, or compensation/reporting fails.
    pub fn rollback(
        &self,
        contract: &mut MissionTxContract,
        now_ms: i64,
    ) -> Result<TxRollbackExecutionResult, TxExecutionError> {
        self.rollback_inner(contract, now_ms, None)
    }
}

impl<E: StepExecutor> TxExecutionEngine<E> {
    /// Create a new execution engine.
    #[must_use]
    pub fn new(executor: E, config: TxExecutionConfig) -> Self {
        Self {
            executor,
            config,
            event_seq: std::cell::Cell::new(0),
        }
    }

    /// Execute the full tx lifecycle while consulting and updating a cross-instance
    /// idempotency store before any commit or compensation side effects dispatch.
    ///
    /// # Errors
    ///
    /// Returns an error if the contract is invalid, a phase transition fails, the
    /// store cannot create or update the execution ledger, or a prior non-success
    /// idempotency outcome would make replay ambiguous.
    pub fn execute_with_store(
        &self,
        contract: &mut MissionTxContract,
        store: &mut IdempotencyStore,
        now_ms: i64,
    ) -> Result<TxExecutionResult, TxExecutionError> {
        if !store.is_durable() {
            return Err(TxExecutionError::InvalidContract(
                "execute_with_store requires a durable idempotency spool".to_string(),
            ));
        }
        self.execute_inner(contract, now_ms, Some(store))
    }

    /// Execute an explicit rollback with durable cross-process compensation
    /// deduplication before any external side effect dispatch.
    ///
    /// Contract receipts identify rollback candidates, but only a live-spool
    /// durable commit `Success` authorizes compensation. Likewise, a receipt
    /// that claims prior compensation is diagnostic until the spool proves the
    /// matching durable `Compensated` outcome.
    ///
    /// # Errors
    ///
    /// Returns an error when rollback proof is invalid, the durable ledger
    /// cannot reserve/record compensation, or compensation execution fails.
    pub fn rollback_with_store(
        &self,
        contract: &mut MissionTxContract,
        store: &mut IdempotencyStore,
        now_ms: i64,
    ) -> Result<TxRollbackExecutionResult, TxExecutionError> {
        if !store.is_durable() {
            return Err(TxExecutionError::InvalidContract(
                "rollback_with_store requires a durable idempotency spool".to_string(),
            ));
        }
        self.rollback_inner(contract, now_ms, Some(store))
    }

    fn rollback_inner(
        &self,
        contract: &mut MissionTxContract,
        now_ms: i64,
        mut store: Option<&mut IdempotencyStore>,
    ) -> Result<TxRollbackExecutionResult, TxExecutionError> {
        if now_ms < 0 {
            return Err(TxExecutionError::InvalidContract(
                "transaction execution timestamp must be non-negative".to_string(),
            ));
        }
        contract
            .validate()
            .map_err(TxExecutionError::InvalidContract)?;
        validate_tx_contract_state_outcome(contract).map_err(TxExecutionError::InvalidContract)?;
        let logical_now_ms = checked_logical_now_ms(now_ms)?;
        let mut commit_report = mission_tx_rollback_commit_report(contract, now_ms)
            .map_err(TxExecutionError::CompensationPhase)?;
        let mut rollback_proof_leases = None;
        let mut prevalidated_original_commit_outcomes = None;
        if let Some(store) = store.as_deref_mut() {
            let (outcomes, leases) = Self::acquire_atomic_compensation_proof(
                contract,
                &mut commit_report,
                store,
                logical_now_ms,
            )?;
            prevalidated_original_commit_outcomes = Some(outcomes);
            rollback_proof_leases = Some(leases);
        }
        validate_receipt_sequence_capacity(
            contract,
            Self::compensation_receipt_headroom(
                contract,
                &commit_report,
                rollback_proof_leases.as_ref(),
            )?,
        )?;
        #[cfg(test)]
        run_rollback_post_proof_lease_test_hook();
        let execution_id = unique_execution_id("rollback", now_ms);
        let mut events = Vec::new();
        let mut decision_path = "rollback".to_string();
        let economic_hard_stop = contract
            .economic_hard_stop_decision_current(now_ms)
            .map_err(TxExecutionError::InvalidContract)?;
        let effective_kill_switch = if economic_hard_stop.is_some() {
            MissionKillSwitchLevel::HardStop
        } else {
            self.config.kill_switch
        };
        // Gate the complete compensation set exactly once before any durable
        // ledger or caller-owned contract mutation. Gate evaluation may
        // consume scoped one-shot approvals, so repeating it per step would be
        // semantically wrong; dispatch uses this prepared decision just as the
        // normal prepare/commit path does.
        let gate_report = Self::compensation_gate_report(
            contract,
            &commit_report,
            rollback_proof_leases.as_ref(),
        )?;
        let compensation_permit = self.validate_compensation_dispatch(
            contract,
            &gate_report,
            &execution_id,
            effective_kill_switch,
            now_ms,
        )?;
        let plan_id = contract.plan.plan_id.0.clone();
        let compiled_plan = compiled_plan_from_contract(contract);
        let mut ledger = TxExecutionLedger::new(&execution_id, &plan_id, compiled_plan.plan_hash);
        if let Some(store) = store.as_deref_mut() {
            store
                .abort_and_archive_matching_ledgers(&compiled_plan.plan_id, compiled_plan.plan_hash)
                .map_err(|err| {
                    TxExecutionError::LedgerWrite(format!(
                        "failed to retire prior rollback ledgers for plan {}: {err}",
                        compiled_plan.plan_id
                    ))
                })?;
            store
                .create_ledger(&execution_id, &compiled_plan)
                .map_err(|err| {
                    TxExecutionError::LedgerWrite(format!(
                        "failed to create rollback ledger {execution_id}: {err}"
                    ))
                })?;
        }
        for phase in [
            TxPhase::Preparing,
            TxPhase::Committing,
            TxPhase::Compensating,
        ] {
            transition_execution_ledger_pair(
                &mut ledger,
                store.as_deref_mut(),
                &execution_id,
                phase,
            )?;
        }

        contract.lifecycle_state = MissionTxState::Compensating;
        contract.outcome = TxOutcome::Pending;
        let (
            report,
            compensation_idem_keys,
            authoritative_recovery_outcomes,
            original_commit_outcomes,
        ) = self.run_compensation_phase(
            contract,
            &commit_report,
            &execution_id,
            &mut events,
            &mut decision_path,
            compensation_permit,
            store.as_deref_mut(),
            prevalidated_original_commit_outcomes.as_ref(),
            rollback_proof_leases.as_mut(),
            now_ms,
        )?;

        // Preserve external-effect evidence on the caller-owned contract before
        // any later ledger write can fail. Callers persist this mutated state on
        // both success and error paths.
        apply_compensation_report(contract, &report);
        self.record_compensation_results_to_ledger(
            contract,
            &report,
            &compensation_idem_keys,
            &original_commit_outcomes,
            TxLedgerRecordingContext {
                execution_id: &execution_id,
                ledger: &mut ledger,
                store: store.as_deref_mut(),
                authoritative_recovery_outcomes: Some(&authoritative_recovery_outcomes),
                events: &mut events,
                now_ms,
            },
        )?;

        let terminal_phase = successful_terminal_ledger_phase(contract.lifecycle_state);
        transition_execution_ledger_pair(
            &mut ledger,
            store.as_deref_mut(),
            &execution_id,
            terminal_phase,
        )?;
        archive_terminal_execution_ledger(&mut ledger, store, &execution_id)?;
        Ok(TxRollbackExecutionResult {
            compensation_report: report,
            events,
            ledger,
            decision_path,
            execution_id,
        })
    }

    fn execute_inner(
        &self,
        contract: &mut MissionTxContract,
        now_ms: i64,
        mut store: Option<&mut IdempotencyStore>,
    ) -> Result<TxExecutionResult, TxExecutionError> {
        if now_ms < 0 {
            return Err(TxExecutionError::InvalidContract(
                "transaction execution timestamp must be non-negative".to_string(),
            ));
        }
        contract
            .validate()
            .map_err(TxExecutionError::InvalidContract)?;
        validate_tx_contract_state_outcome(contract).map_err(TxExecutionError::InvalidContract)?;
        validate_execution_recovery_entry(contract, store.as_deref())?;
        validate_receipt_sequence_capacity(
            contract,
            contract.plan.steps.len().checked_mul(3).ok_or_else(|| {
                TxExecutionError::InvalidContract(
                    "transaction step count overflow while reserving receipt headroom".to_string(),
                )
            })?,
        )?;

        let execution_id = unique_execution_id("run", now_ms);
        let plan_id = contract.plan.plan_id.0.clone();
        let compiled_plan = compiled_plan_from_contract(contract);
        let mut ledger = TxExecutionLedger::new(&execution_id, &plan_id, compiled_plan.plan_hash);
        if let Some(store) = store.as_deref_mut() {
            if let Err(err) = store
                .abort_and_archive_matching_ledgers(&compiled_plan.plan_id, compiled_plan.plan_hash)
            {
                if let crate::tx_idempotency::IdempotencyError::AmbiguousTerminalTransition {
                    execution_id: prior_execution_id,
                    ..
                } = &err
                {
                    return Err(reconcile_before_ambiguous_retirement(
                        contract,
                        store,
                        prior_execution_id,
                        now_ms,
                        &err,
                    ));
                }
                return Err(TxExecutionError::LedgerWrite(format!(
                    "failed to retire prior execution ledgers for plan {}: {err}",
                    compiled_plan.plan_id
                )));
            }
            store
                .create_ledger(&execution_id, &compiled_plan)
                .map_err(|err| {
                    TxExecutionError::LedgerWrite(format!(
                        "failed to create store-backed ledger {execution_id}: {err}"
                    ))
                })?;
        }
        let mut events: Vec<TxObservabilityEvent> = Vec::new();
        let mut decision_path = String::new();
        let economic_hard_stop = contract
            .economic_hard_stop_decision_current(now_ms)
            .map_err(TxExecutionError::InvalidContract)?;
        let effective_kill_switch = if economic_hard_stop.is_some() {
            MissionKillSwitchLevel::HardStop
        } else {
            self.config.kill_switch
        };
        if let Some(MissionEconomicBreakerDecision::HardStop {
            envelope,
            audit_row,
        }) = &economic_hard_stop
        {
            self.record_economic_hard_stop_event(
                envelope,
                audit_row,
                &execution_id,
                &plan_id,
                &mut events,
                now_ms,
            );
            decision_path.push_str("economic_hard_stop->");
        }

        // Phase 1: Prepare
        let (prepare_report, durable_prepare_successes) = self.run_prepare_phase(
            contract,
            &execution_id,
            &mut ledger,
            &mut events,
            &mut decision_path,
            effective_kill_switch,
            store.as_deref_mut(),
            now_ms,
        )?;

        let recovering_rejected_prepare =
            !prepare_report.outcome.commit_eligible() && !durable_prepare_successes.is_empty();

        if !prepare_report.outcome.commit_eligible() && !recovering_rejected_prepare {
            let final_state = match &prepare_report.outcome {
                TxPrepareOutcome::Denied => MissionTxState::Failed,
                TxPrepareOutcome::RequireApproval => MissionTxState::Planned,
                _ => MissionTxState::Planned,
            };
            contract.lifecycle_state = final_state;
            contract.outcome = match final_state {
                MissionTxState::Failed => TxOutcome::Failed,
                _ => TxOutcome::Pending,
            };
            decision_path.push_str("->prepare_not_eligible");
            transition_execution_ledger_pair(
                &mut ledger,
                store.as_deref_mut(),
                &execution_id,
                TxPhase::Aborted,
            )?;

            let forensic_bundle = self.maybe_build_forensic_bundle(
                contract,
                &ledger,
                &mut events,
                None,
                &execution_id,
                now_ms,
            );
            archive_terminal_execution_ledger(&mut ledger, store.as_deref_mut(), &execution_id)?;

            return Ok(TxExecutionResult {
                final_state,
                outcome: contract.outcome.clone(),
                prepare_report,
                commit_report: None,
                compensation_report: None,
                events,
                ledger,
                forensic_bundle,
                decision_path,
                reason_code: "prepare_not_eligible".to_string(),
            });
        }

        // Transition: Planned → Prepared → Committing
        if !recovering_rejected_prepare && contract.lifecycle_state == MissionTxState::Planned {
            contract.lifecycle_state = MissionTxState::Prepared;
        }
        transition_execution_ledger_pair(
            &mut ledger,
            store.as_deref_mut(),
            &execution_id,
            TxPhase::Preparing,
        )?;
        transition_execution_ledger_pair(
            &mut ledger,
            store.as_deref_mut(),
            &execution_id,
            TxPhase::Committing,
        )?;

        // Phase 2: Commit
        contract.lifecycle_state = MissionTxState::Committing;
        let (mut commit_report, recovery_requires_compensation) = if recovering_rejected_prepare {
            let unresolved_reason_code = match &prepare_report.outcome {
                TxPrepareOutcome::Denied => "prepare_denied_recovery_unresolved",
                TxPrepareOutcome::RequireApproval => "prepare_approval_recovery_unresolved",
                TxPrepareOutcome::Deferred => "prepare_deferred_recovery_unresolved",
                TxPrepareOutcome::AllReady => {
                    unreachable!("rejected prepare recovery cannot have an all-ready outcome")
                }
            };
            decision_path.push_str("->commit(durable_recovery)");
            (
                recovered_commit_report(
                    contract,
                    &durable_prepare_successes,
                    unresolved_reason_code,
                    now_ms,
                )?,
                true,
            )
        } else {
            self.run_commit_phase(
                contract,
                &execution_id,
                &mut events,
                &mut decision_path,
                effective_kill_switch,
                store.as_deref_mut(),
                now_ms,
            )?
        };

        contract
            .receipts
            .extend(commit_report.receipts.iter().cloned());

        // Record commit step results in the ledger
        self.record_commit_results_to_ledger(
            contract,
            &commit_report,
            TxLedgerRecordingContext {
                execution_id: &execution_id,
                ledger: &mut ledger,
                store: store.as_deref_mut(),
                authoritative_recovery_outcomes: None,
                events: &mut events,
                now_ms,
            },
        )?;

        if recovery_requires_compensation && !self.config.auto_compensate {
            // The contract and active ledger deliberately remain Committing:
            // durable external effects exist, unresolved steps were never
            // dispatched, and no compensation was authorized.
            contract.lifecycle_state = MissionTxState::Committing;
            contract.outcome = TxOutcome::Pending;
            return Err(TxExecutionError::CommitPhase(
                "durable commit recovery found unresolved work after safety gating, but automatic compensation is disabled"
                    .to_string(),
            ));
        }

        let commit_outcome_state = commit_report.outcome.target_tx_state();
        contract.lifecycle_state = commit_outcome_state;
        contract.outcome = tx_outcome_for_state(commit_outcome_state);

        // Phase 3: Compensate (if needed)
        let compensation_report =
            if Self::should_run_compensation(&commit_report, self.config.auto_compensate) {
                let mut compensation_proof_leases = None;
                let mut prevalidated_original_commit_outcomes = None;
                if let Some(durable_store) = store.as_deref_mut() {
                    let (outcomes, leases) = Self::acquire_atomic_compensation_proof(
                        contract,
                        &mut commit_report,
                        durable_store,
                        checked_logical_now_ms(now_ms)?,
                    )?;
                    prevalidated_original_commit_outcomes = Some(outcomes);
                    compensation_proof_leases = Some(leases);
                }
                let gate_report = Self::compensation_gate_report(
                    contract,
                    &commit_report,
                    compensation_proof_leases.as_ref(),
                )?;
                // Deferral and denial are different verdicts and must leave
                // different contract states.
                //
                // A *deferred* batch — execution paused, or more outstanding
                // steps than `max_steps_per_batch` — means compensation is
                // still owed and merely has to wait. `commit_report.outcome
                // .target_tx_state()` above already wrote the commit-phase
                // verdict (`Failed` for a partial failure), and transitioning
                // to `Compensating` only after the dispatch gate meant a
                // deferral returned with the contract terminally `Failed`
                // while a durable effect was live and un-compensated —
                // nothing recorded that rollback was still due. Record it
                // before failing.
                //
                // A gate *denial* is the opposite: policy, approval, or
                // liveness decided compensation must not run at all, so the
                // terminal commit verdict stands and the contract must not
                // claim a rollback is pending.
                if let Err(deferred) = self.validated_compensation_batch(&gate_report) {
                    contract.lifecycle_state = MissionTxState::Compensating;
                    contract.outcome = TxOutcome::Pending;
                    return Err(deferred);
                }

                // A hard stop must block every unresolved forward step, but it
                // must not strand an external effect whose durable success was
                // recovered from an earlier process. This exception grants no
                // commit authority: it applies only to the synthesized
                // recovery report and still evaluates the compensation
                // action's policy, approval, reservation, and liveness gates.
                let compensation_kill_switch = if recovery_requires_compensation {
                    MissionKillSwitchLevel::Off
                } else {
                    effective_kill_switch
                };
                let compensation_permit = self.validate_compensation_dispatch(
                    contract,
                    &gate_report,
                    &execution_id,
                    compensation_kill_switch,
                    now_ms,
                )?;

                contract.lifecycle_state = MissionTxState::Compensating;
                contract.outcome = TxOutcome::Pending;
                transition_execution_ledger_pair(
                    &mut ledger,
                    store.as_deref_mut(),
                    &execution_id,
                    TxPhase::Compensating,
                )?;

                let (
                    comp,
                    compensation_idem_keys,
                    authoritative_recovery_outcomes,
                    original_commit_outcomes,
                ) = self.run_compensation_phase(
                    contract,
                    &commit_report,
                    &execution_id,
                    &mut events,
                    &mut decision_path,
                    compensation_permit,
                    store.as_deref_mut(),
                    prevalidated_original_commit_outcomes.as_ref(),
                    compensation_proof_leases.as_mut(),
                    now_ms,
                )?;

                apply_compensation_report(contract, &comp);

                self.record_compensation_results_to_ledger(
                    contract,
                    &comp,
                    &compensation_idem_keys,
                    &original_commit_outcomes,
                    TxLedgerRecordingContext {
                        execution_id: &execution_id,
                        ledger: &mut ledger,
                        store: store.as_deref_mut(),
                        authoritative_recovery_outcomes: Some(&authoritative_recovery_outcomes),
                        events: &mut events,
                        now_ms,
                    },
                )?;

                Some(comp)
            } else {
                None
            };

        // Determine final outcome
        let (final_state, outcome) = Self::determine_final_outcome(
            contract.lifecycle_state,
            &commit_report,
            compensation_report.as_ref(),
        );
        contract.lifecycle_state = final_state;
        contract.outcome = outcome.clone();
        decision_path.push_str(&format!("->final:{final_state}"));

        // Transition ledger to terminal phase (skip if outcome is Pending —
        // the tx is suspended, not finished)
        if outcome != TxOutcome::Pending {
            let terminal_phase = successful_terminal_ledger_phase(final_state);
            transition_execution_ledger_pair(
                &mut ledger,
                store.as_deref_mut(),
                &execution_id,
                terminal_phase,
            )
            .map_err(|err| {
                TxExecutionError::LedgerWrite(format!(
                    "failed to transition ledger to terminal phase {terminal_phase:?}: {err}"
                ))
            })?;
        }

        // Emit completion event
        events.push(self.make_event(
            TxEventKind::CommitCompleted,
            TxObservabilityPhase::Commit,
            &format!("tx.execution.{}", reason_code_for_outcome(&outcome)),
            &execution_id,
            &plan_id,
            ledger.phase(),
            now_ms,
        ));

        let forensic_bundle = self.maybe_build_forensic_bundle(
            contract,
            &ledger,
            &mut events,
            None,
            &execution_id,
            now_ms,
        );
        archive_terminal_execution_ledger(&mut ledger, store, &execution_id)?;

        Ok(TxExecutionResult {
            final_state,
            outcome,
            prepare_report,
            commit_report: Some(commit_report),
            compensation_report,
            events,
            ledger,
            forensic_bundle,
            decision_path,
            reason_code: format!("execution_{final_state}"),
        })
    }

    /// Resume execution from a persisted ledger.
    pub fn resume(
        &self,
        contract: &mut MissionTxContract,
        store: &mut IdempotencyStore,
        execution_id: &str,
        now_ms: i64,
    ) -> Result<TxExecutionResult, TxExecutionError> {
        if !store.is_durable() {
            return Err(TxExecutionError::InvalidContract(
                "transaction resume requires a durable idempotency spool".to_string(),
            ));
        }
        if now_ms < 0 {
            return Err(TxExecutionError::InvalidContract(
                "transaction execution timestamp must be non-negative".to_string(),
            ));
        }
        contract
            .validate()
            .map_err(TxExecutionError::InvalidContract)?;
        validate_tx_contract_state_outcome(contract).map_err(TxExecutionError::InvalidContract)?;
        let mut ledger = store
            .get_ledger(execution_id)
            .ok_or_else(|| TxExecutionError::LedgerNotFound(execution_id.to_string()))?
            .clone();
        let compiled_plan = compiled_plan_from_contract(contract);
        if ledger.plan_id() != compiled_plan.plan_id
            || ledger.plan_hash() != compiled_plan.plan_hash
        {
            return Err(TxExecutionError::InvalidContract(format!(
                "execution {execution_id} belongs to plan {} with hash {}, not supplied plan {} with hash {}",
                ledger.plan_id(),
                ledger.plan_hash(),
                compiled_plan.plan_id,
                compiled_plan.plan_hash
            )));
        }
        let resume_ctx = store
            .resume_context(execution_id, &compiled_plan)
            .ok_or_else(|| TxExecutionError::LedgerNotFound(execution_id.to_string()))?;
        let mut events = Vec::new();

        events.push(self.make_event(
            TxEventKind::ResumeContextBuilt,
            TxObservabilityPhase::Resume,
            "tx.resume.context_built",
            execution_id,
            &contract.plan.plan_id.0,
            ledger.phase(),
            now_ms,
        ));

        match resume_ctx.recommendation.clone() {
            ResumeRecommendation::AlreadyComplete => {
                let (final_state, outcome) = resume_terminal_outcome(contract, &resume_ctx);
                let terminal_phase = successful_terminal_ledger_phase(final_state);
                if ledger.phase().is_terminal() {
                    if ledger.phase() != terminal_phase {
                        return Err(TxExecutionError::PhaseTransition(format!(
                            "completed recovery for {execution_id} requires terminal phase {terminal_phase:?}, but durable ledger is sealed as {:?}",
                            ledger.phase()
                        )));
                    }
                } else {
                    transition_execution_ledger_pair(
                        &mut ledger,
                        Some(&mut *store),
                        execution_id,
                        terminal_phase,
                    )?;
                }
                contract.lifecycle_state = final_state;
                contract.outcome = outcome.clone();
                archive_terminal_execution_ledger(&mut ledger, Some(&mut *store), execution_id)?;
                let forensic_bundle = self.maybe_build_forensic_bundle(
                    contract,
                    &ledger,
                    &mut events,
                    Some(&resume_ctx),
                    execution_id,
                    now_ms,
                );
                Ok(TxExecutionResult {
                    final_state,
                    outcome,
                    prepare_report: TxPrepareReport {
                        outcome: TxPrepareOutcome::AllReady,
                        gate_inputs: Vec::new(),
                    },
                    commit_report: None,
                    compensation_report: None,
                    events,
                    ledger,
                    forensic_bundle,
                    decision_path: "resume->already_complete".to_string(),
                    reason_code: "already_complete".to_string(),
                })
            }
            ResumeRecommendation::RestartFresh => {
                contract.lifecycle_state = MissionTxState::Planned;
                contract.outcome = TxOutcome::Pending;
                events.push(self.make_event(
                    TxEventKind::ResumeExecuted,
                    TxObservabilityPhase::Resume,
                    "tx.resume.restart_fresh",
                    execution_id,
                    &contract.plan.plan_id.0,
                    ledger.phase(),
                    now_ms,
                ));
                self.execute_with_store(contract, store, now_ms)
            }
            recommendation @ (ResumeRecommendation::CompensateAndAbort
            | ResumeRecommendation::ContinueFromCheckpoint) => {
                if resume_ctx.completed_steps.is_empty()
                    && resume_ctx.failed_steps.is_empty()
                    && resume_ctx.compensated_steps.is_empty()
                {
                    events.push(self.make_event(
                        TxEventKind::ResumeExecuted,
                        TxObservabilityPhase::Resume,
                        "tx.resume.replay_from_start",
                        execution_id,
                        &contract.plan.plan_id.0,
                        ledger.phase(),
                        now_ms,
                    ));
                    contract.lifecycle_state = MissionTxState::Planned;
                    contract.outcome = TxOutcome::Pending;
                    return self.execute_with_store(contract, store, now_ms);
                }

                Err(TxExecutionError::UnsafeResume {
                    execution_id: execution_id.to_string(),
                    recommendation,
                })
            }
        }
    }

    // ── Phase Runners ────────────────────────────────────────────────────────

    fn run_prepare_phase(
        &self,
        contract: &mut MissionTxContract,
        execution_id: &str,
        ledger: &mut TxExecutionLedger,
        events: &mut Vec<TxObservabilityEvent>,
        decision_path: &mut String,
        kill_switch: MissionKillSwitchLevel,
        store: Option<&mut IdempotencyStore>,
        now_ms: i64,
    ) -> Result<(TxPrepareReport, HashSet<String>), TxExecutionError> {
        let logical_now_ms = checked_logical_now_ms(now_ms)?;
        events.push(self.make_event(
            TxEventKind::PrepareStarted,
            TxObservabilityPhase::Prepare,
            "tx.prepare.started",
            execution_id,
            &contract.plan.plan_id.0,
            TxPhase::Preparing,
            now_ms,
        ));

        let (gate_inputs, has_unresolved_effects, durable_success_step_ids) = if let Some(
            durable_store,
        ) = store
        {
            let committed_receipt_step_ids = contract
                .receipts
                .iter()
                .filter(|receipt| {
                    receipt.get("phase").and_then(serde_json::Value::as_str) == Some("commit")
                        && receipt.get("outcome").and_then(serde_json::Value::as_str)
                            == Some("committed")
                })
                .filter_map(|receipt| {
                    receipt
                        .get("step_id")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string)
                })
                .collect::<HashSet<_>>();
            let mut unresolved_steps = Vec::new();
            let mut proven_steps = Vec::new();
            let mut authoritative_recovery_outcomes = HashMap::new();
            let mut first_conflict = None;

            for step in &contract.plan.steps {
                let idem_key = commit_idempotency_key(contract, &step.step_id.0)?;
                let observed_outcome = durable_store
                    .acquire_durable_reservation(execution_id, &idem_key, logical_now_ms)
                    .map_err(|err| {
                        TxExecutionError::LedgerWrite(format!(
                            "failed to classify durable prepare state for step {}: {err}",
                            step.step_id.0
                        ))
                    })?
                    .observed_outcome()
                    .cloned();
                match observed_outcome {
                    Some(outcome @ StepOutcome::Success { .. }) => {
                        proven_steps.push(step.clone());
                        authoritative_recovery_outcomes.insert(step.step_id.0.clone(), outcome);
                    }
                    Some(other) => {
                        if first_conflict.is_none() {
                            first_conflict = Some((step.step_id.0.clone(), format!("{other:?}")));
                        }
                    }
                    None => {
                        if committed_receipt_step_ids.contains(&step.step_id.0) {
                            if first_conflict.is_none() {
                                first_conflict = Some((
                                        step.step_id.0.clone(),
                                        "missing live durable Success proof for committed recovery receipt"
                                            .to_string(),
                                    ));
                            }
                        } else {
                            unresolved_steps.push(step.clone());
                        }
                    }
                }
            }

            let durable_success_step_ids = proven_steps
                .iter()
                .map(|step| step.step_id.0.clone())
                .collect::<HashSet<_>>();

            // Reconcile every proven effect before any unrelated conflict or
            // fresh prepare-gate failure can return. The durable ledger remains
            // the authority; these receipts make the caller-owned contract
            // tell the same truth after a crash between ledger sync and save.
            if !durable_success_step_ids.is_empty() {
                let reconciliation =
                    recovered_commit_success_report(contract, &durable_success_step_ids, now_ms)?;
                contract
                    .receipts
                    .extend(reconciliation.receipts.iter().cloned());
                self.record_commit_results_to_ledger(
                    contract,
                    &reconciliation,
                    TxLedgerRecordingContext {
                        execution_id,
                        ledger,
                        store: Some(&mut *durable_store),
                        authoritative_recovery_outcomes: Some(&authoritative_recovery_outcomes),
                        events,
                        now_ms,
                    },
                )?;
            }

            if let Some((step_id, outcome)) = first_conflict {
                return Err(TxExecutionError::DedupConflict { step_id, outcome });
            }

            let has_unresolved_effects = !unresolved_steps.is_empty();
            let mut unresolved_contract = contract.clone();
            unresolved_contract.plan.steps = unresolved_steps;
            let unresolved_step_ids = unresolved_contract
                .plan
                .steps
                .iter()
                .map(|step| step.step_id.clone())
                .collect::<HashSet<_>>();
            unresolved_contract
                .plan
                .compensations
                .retain(|compensation| unresolved_step_ids.contains(&compensation.for_step_id));
            let mut evaluated = if unresolved_contract.plan.steps.is_empty() {
                Vec::new()
            } else {
                self.executor.evaluate_gates(&unresolved_contract, now_ms)
            };

            if !proven_steps.is_empty() {
                let mut proven_contract = contract.clone();
                proven_contract.plan.steps = proven_steps;
                proven_contract.plan.preconditions.clear();
                proven_contract.plan.compensations.clear();
                evaluated.extend(crate::plan::tx_prepare_gate_inputs_allow_all(
                    &proven_contract,
                ));
            }
            let ordinal_by_step = contract
                .plan
                .steps
                .iter()
                .map(|step| (step.step_id.0.as_str(), step.ordinal))
                .collect::<HashMap<_, _>>();
            evaluated.sort_by_key(|gate| {
                ordinal_by_step
                    .get(gate.step_id.0.as_str())
                    .copied()
                    .unwrap_or(usize::MAX)
            });
            (evaluated, has_unresolved_effects, durable_success_step_ids)
        } else {
            (
                self.executor.evaluate_gates(contract, now_ms),
                true,
                HashSet::new(),
            )
        };
        self.record_prepare_gate_events(contract, execution_id, events, &gate_inputs, now_ms);

        let report = evaluate_prepare_phase(
            &contract.intent.tx_id,
            &contract.plan,
            &gate_inputs,
            if has_unresolved_effects {
                kill_switch
            } else {
                MissionKillSwitchLevel::Off
            },
            now_ms,
        )
        .map_err(TxExecutionError::PreparePhase)?;

        let reason = match &report.outcome {
            TxPrepareOutcome::AllReady => "tx.prepare.all_ready",
            TxPrepareOutcome::RequireApproval => "tx.prepare.require_approval",
            TxPrepareOutcome::Denied => "tx.prepare.denied",
            TxPrepareOutcome::Deferred => "tx.prepare.deferred",
        };

        events.push(self.make_event(
            TxEventKind::PrepareCompleted,
            TxObservabilityPhase::Prepare,
            reason,
            execution_id,
            &contract.plan.plan_id.0,
            TxPhase::Preparing,
            now_ms,
        ));

        decision_path.push_str(&format!("prepare({:?})", report.outcome));
        Ok((report, durable_success_step_ids))
    }

    fn run_commit_phase(
        &self,
        contract: &MissionTxContract,
        execution_id: &str,
        events: &mut Vec<TxObservabilityEvent>,
        decision_path: &mut String,
        kill_switch: MissionKillSwitchLevel,
        store: Option<&mut IdempotencyStore>,
        now_ms: i64,
    ) -> Result<(TxCommitReport, bool), TxExecutionError> {
        events.push(self.make_event(
            TxEventKind::CommitStarted,
            TxObservabilityPhase::Commit,
            "tx.commit.started",
            execution_id,
            &contract.plan.plan_id.0,
            TxPhase::Committing,
            now_ms,
        ));

        let batch_limit_exceeded = contract.plan.steps.len() > self.config.max_steps_per_batch;
        if batch_limit_exceeded {
            tracing::warn!(
                plan_id = %contract.plan.plan_id.0,
                step_count = contract.plan.steps.len(),
                max_steps_per_batch = self.config.max_steps_per_batch,
                "tx commit batch limit exceeded; suspending commit dispatch"
            );
        }

        let safety_paused = self.config.paused || batch_limit_exceeded;
        let dispatch_allowed = kill_switch == MissionKillSwitchLevel::Off && !safety_paused;
        let commit_inputs =
            self.commit_inputs_with_dedup(contract, execution_id, store, dispatch_allowed, now_ms)?;
        let fully_proven_replay = commit_inputs.len() == contract.plan.steps.len()
            && commit_inputs
                .iter()
                .all(|input| input.reason_code == "commit_step_deduped");
        let durable_success_step_ids = commit_inputs
            .iter()
            .filter(|input| input.reason_code == "commit_step_deduped")
            .map(|input| input.step_id.0.clone())
            .collect::<HashSet<_>>();

        if !dispatch_allowed && !fully_proven_replay && !durable_success_step_ids.is_empty() {
            let unresolved_reason_code = if kill_switch != MissionKillSwitchLevel::Off {
                "kill_switch_recovery_unresolved"
            } else if self.config.paused {
                "pause_suspended_recovery_unresolved"
            } else {
                "batch_limit_recovery_unresolved"
            };
            let report = recovered_commit_report(
                contract,
                &durable_success_step_ids,
                unresolved_reason_code,
                now_ms,
            )?;
            decision_path.push_str("->commit(durable_recovery)");
            return Ok((report, true));
        }

        let report = execute_commit_phase(
            contract,
            &commit_inputs,
            if fully_proven_replay {
                MissionKillSwitchLevel::Off
            } else {
                kill_switch
            },
            safety_paused && !fully_proven_replay,
            now_ms,
        )
        .map_err(TxExecutionError::CommitPhase)?;

        decision_path.push_str(&format!("->commit({:?})", report.outcome));
        Ok((report, false))
    }

    fn run_compensation_phase(
        &self,
        contract: &MissionTxContract,
        commit_report: &TxCommitReport,
        execution_id: &str,
        events: &mut Vec<TxObservabilityEvent>,
        decision_path: &mut String,
        mut dispatch_permit: CompensationDispatchPermit,
        store: Option<&mut IdempotencyStore>,
        prevalidated_original_commit_outcomes: Option<&HashMap<String, StepOutcome>>,
        preacquired_rollback_proof_leases: Option<&mut DurableKeyLeaseSet>,
        now_ms: i64,
    ) -> Result<CompensationPhaseOutput, TxExecutionError> {
        if dispatch_permit.execution_id != execution_id
            || dispatch_permit.plan_id != contract.plan.plan_id.0
            || dispatch_permit.contract_hash != contract.compute_hash()
        {
            return Err(TxExecutionError::InvalidContract(format!(
                "compensation dispatch permit context mismatch: permit execution {:?} plan {:?} contract {}, attempted execution {execution_id:?} plan {:?} contract {}",
                dispatch_permit.execution_id,
                dispatch_permit.plan_id,
                dispatch_permit.contract_hash,
                contract.plan.plan_id.0,
                contract.compute_hash()
            )));
        }
        events.append(&mut dispatch_permit.gate_events);
        events.push(self.make_event(
            TxEventKind::CompensationStarted,
            TxObservabilityPhase::Compensate,
            "tx.compensation.started",
            execution_id,
            &contract.plan.plan_id.0,
            TxPhase::Compensating,
            now_ms,
        ));
        let (
            comp_inputs,
            compensation_idem_keys,
            authoritative_recovery_outcomes,
            original_commit_outcomes,
        ) = self.compensation_inputs_with_dedup(
            contract,
            commit_report,
            execution_id,
            store,
            prevalidated_original_commit_outcomes,
            preacquired_rollback_proof_leases,
            &dispatch_permit,
            now_ms,
        )?;

        let mut report = execute_compensation_phase(contract, commit_report, &comp_inputs, now_ms)
            .map_err(TxExecutionError::CompensationPhase)?;

        // Durable compensated outcomes prove prior work. Keep fresh-only
        // step/count semantics, but recover one authoritative receipt when a
        // crash persisted the durable outcome before the contract snapshot.
        let deduped_step_ids = comp_inputs
            .iter()
            .filter(|input| input.reason_code == "compensation_step_deduped")
            .map(|input| input.for_step_id.0.as_str())
            .collect::<HashSet<_>>();
        if !deduped_step_ids.is_empty() {
            let deduped_steps_with_receipts = deduped_step_ids
                .iter()
                .copied()
                .filter(|step_id| {
                    latest_tx_receipt_matches(contract, "compensate", step_id, "compensated")
                })
                .collect::<HashSet<_>>();
            report
                .step_results
                .retain(|result| !deduped_step_ids.contains(result.step_id.0.as_str()));
            report.receipts.retain(|receipt| {
                receipt
                    .get("step_id")
                    .and_then(serde_json::Value::as_str)
                    .is_none_or(|step_id| !deduped_steps_with_receipts.contains(step_id))
            });
            report.compensated_count = report
                .step_results
                .iter()
                .filter(|result| result.outcome.is_committed())
                .count();
            report.failed_count = report
                .step_results
                .iter()
                .filter(|result| {
                    matches!(
                        result.outcome,
                        crate::plan::TxCommitStepOutcome::Failed { .. }
                    )
                })
                .count();
            report.skipped_count = report
                .step_results
                .iter()
                .filter(|result| {
                    matches!(
                        result.outcome,
                        crate::plan::TxCommitStepOutcome::Skipped { .. }
                    )
                })
                .count();
        }

        let reason = match &report.outcome {
            crate::plan::TxCompensationOutcome::FullyRolledBack => {
                "tx.compensation.fully_rolled_back"
            }
            crate::plan::TxCompensationOutcome::CompensationFailed => "tx.compensation.failed",
            crate::plan::TxCompensationOutcome::NothingToCompensate => {
                "tx.compensation.nothing_to_compensate"
            }
        };

        events.push(self.make_event(
            TxEventKind::CompensationCompleted,
            TxObservabilityPhase::Compensate,
            reason,
            execution_id,
            &contract.plan.plan_id.0,
            TxPhase::Compensating,
            now_ms,
        ));

        decision_path.push_str(&format!("->compensate({:?})", report.outcome));
        Ok((
            report,
            compensation_idem_keys,
            authoritative_recovery_outcomes,
            original_commit_outcomes,
        ))
    }

    fn validate_compensation_dispatch(
        &self,
        contract: &MissionTxContract,
        commit_report: &TxCommitReport,
        execution_id: &str,
        kill_switch: MissionKillSwitchLevel,
        now_ms: i64,
    ) -> Result<CompensationDispatchPermit, TxExecutionError> {
        let outstanding = self.validated_compensation_batch(commit_report)?;
        if outstanding.is_empty() {
            return Ok(CompensationDispatchPermit {
                execution_id: execution_id.to_string(),
                plan_id: contract.plan.plan_id.0.clone(),
                contract_hash: contract.compute_hash(),
                step_ids: HashSet::new(),
                gate_events: Vec::new(),
            });
        }

        // Evaluate policy, approval, reservation, and liveness against the
        // compensation actions themselves. Reusing the original commit actions
        // here would authorize a different external effect than the one about
        // to be dispatched.
        let mut gate_contract = contract.clone();
        gate_contract.plan.steps = contract
            .plan
            .steps
            .iter()
            .filter(|step| outstanding.contains(step.step_id.0.as_str()))
            .map(|step| {
                let compensation = contract
                    .plan
                    .compensations
                    .iter()
                    .find(|compensation| compensation.for_step_id == step.step_id)
                    .ok_or_else(|| {
                        TxExecutionError::InvalidContract(format!(
                            "transaction plan has no compensation action for committed step {}",
                            step.step_id.0
                        ))
                    })?;
                let mut compensation_step = step.clone();
                compensation_step.action = compensation.action.clone();
                Ok(compensation_step)
            })
            .collect::<Result<Vec<_>, TxExecutionError>>()?;
        gate_contract.plan.compensations.clear();
        gate_contract.plan.preconditions.clear();

        let gate_inputs = self.executor.evaluate_gates(&gate_contract, now_ms);
        let mut gate_events = Vec::new();
        self.record_prepare_gate_events(
            &gate_contract,
            execution_id,
            &mut gate_events,
            &gate_inputs,
            now_ms,
        );
        let report = evaluate_prepare_phase(
            &gate_contract.intent.tx_id,
            &gate_contract.plan,
            &gate_inputs,
            kill_switch,
            now_ms,
        )
        .map_err(TxExecutionError::CompensationPhase)?;
        if report.outcome != TxPrepareOutcome::AllReady {
            return Err(TxExecutionError::CompensationPhase(format!(
                "compensation prepare gates rejected dispatch with outcome {:?}",
                report.outcome
            )));
        }
        Ok(CompensationDispatchPermit {
            execution_id: execution_id.to_string(),
            plan_id: contract.plan.plan_id.0.clone(),
            contract_hash: contract.compute_hash(),
            step_ids: outstanding.into_iter().map(str::to_string).collect(),
            gate_events,
        })
    }

    fn compensation_gate_report(
        contract: &MissionTxContract,
        commit_report: &TxCommitReport,
        durable_leases: Option<&DurableKeyLeaseSet>,
    ) -> Result<TxCommitReport, TxExecutionError> {
        let Some(durable_leases) = durable_leases else {
            return Ok(commit_report.clone());
        };
        let mut gate_report = commit_report.clone();
        for result in &mut gate_report.step_results {
            if !result.outcome.is_committed() {
                continue;
            }
            let idem_key = compensation_idempotency_key(contract, &result.step_id.0)?;
            let observed = durable_leases
                .get(&idem_key)
                .ok_or_else(|| {
                    TxExecutionError::LedgerWrite(format!(
                        "atomic compensation proof lease set is missing key for step {}",
                        result.step_id.0
                    ))
                })?
                .observed_outcome();
            match observed {
                Some(StepOutcome::Compensated { .. }) => {
                    result.outcome = crate::plan::TxCommitStepOutcome::Skipped {
                        reason_code: "durable_compensation_recovery".to_string(),
                    };
                    result
                        .decision_path
                        .push_str("->durable_compensation_recovery");
                }
                Some(
                    StepOutcome::Failed {
                        compensated: false, ..
                    }
                    | StepOutcome::Skipped { .. },
                )
                | None => {}
                Some(other) => {
                    return Err(TxExecutionError::RollbackProof {
                        kind: RollbackProofKind::Conflict,
                        step_id: result.step_id.0.clone(),
                        detail: format!(
                            "authoritative durable compensation state {other:?} cannot produce a dispatch permit"
                        ),
                    });
                }
            }
        }
        gate_report.committed_count = gate_report
            .step_results
            .iter()
            .filter(|result| result.outcome.is_committed())
            .count();
        gate_report.failed_count = gate_report
            .step_results
            .iter()
            .filter(|result| {
                matches!(
                    result.outcome,
                    crate::plan::TxCommitStepOutcome::Failed { .. }
                )
            })
            .count();
        gate_report.skipped_count = gate_report
            .step_results
            .iter()
            .filter(|result| result.outcome.is_skipped())
            .count();
        Ok(gate_report)
    }

    fn compensation_receipt_headroom(
        contract: &MissionTxContract,
        commit_report: &TxCommitReport,
        durable_leases: Option<&DurableKeyLeaseSet>,
    ) -> Result<usize, TxExecutionError> {
        let mut headroom = 0usize;
        for result in commit_report
            .step_results
            .iter()
            .filter(|result| result.outcome.is_committed())
        {
            let needs_receipt = if let Some(durable_leases) = durable_leases {
                let idem_key = compensation_idempotency_key(contract, &result.step_id.0)?;
                match durable_leases
                    .get(&idem_key)
                    .ok_or_else(|| {
                        TxExecutionError::LedgerWrite(format!(
                            "atomic compensation proof lease set is missing receipt-capacity key for step {}",
                            result.step_id.0
                        ))
                    })?
                    .observed_outcome()
                {
                    Some(StepOutcome::Compensated { .. }) => !latest_tx_receipt_matches(
                        contract,
                        "compensate",
                        &result.step_id.0,
                        "compensated",
                    ),
                    Some(
                        StepOutcome::Failed {
                            compensated: false, ..
                        }
                        | StepOutcome::Skipped { .. },
                    )
                    | None => true,
                    Some(other) => {
                        return Err(TxExecutionError::RollbackProof {
                            kind: RollbackProofKind::Conflict,
                            step_id: result.step_id.0.clone(),
                            detail: format!(
                                "authoritative durable compensation state {other:?} cannot reserve receipt capacity"
                            ),
                        });
                    }
                }
            } else {
                true
            };
            if needs_receipt {
                headroom = headroom.checked_add(1).ok_or_else(|| {
                    TxExecutionError::InvalidContract(
                        "compensation receipt headroom count overflow".to_string(),
                    )
                })?;
            }
        }
        Ok(headroom)
    }

    fn validated_compensation_batch<'a>(
        &self,
        commit_report: &'a TxCommitReport,
    ) -> Result<HashSet<&'a str>, TxExecutionError> {
        let outstanding = commit_report
            .step_results
            .iter()
            .filter(|result| result.outcome.is_committed())
            .map(|result| result.step_id.0.as_str())
            .collect::<HashSet<_>>();
        if outstanding.is_empty() {
            return Ok(outstanding);
        }
        if self.config.paused {
            return Err(TxExecutionError::CompensationPhase(
                "compensation dispatch is suspended while transaction execution is paused"
                    .to_string(),
            ));
        }
        if outstanding.len() > self.config.max_steps_per_batch {
            return Err(TxExecutionError::CompensationPhase(format!(
                "compensation batch has {} steps, exceeding configured maximum {}",
                outstanding.len(),
                self.config.max_steps_per_batch
            )));
        }
        Ok(outstanding)
    }

    fn commit_inputs_with_dedup(
        &self,
        contract: &MissionTxContract,
        execution_id: &str,
        store: Option<&mut IdempotencyStore>,
        dispatch_allowed: bool,
        now_ms: i64,
    ) -> Result<Vec<TxCommitStepInput>, TxExecutionError> {
        let logical_now_ms = checked_logical_now_ms(now_ms)?;
        let Some(store) = store else {
            if !dispatch_allowed {
                return Ok(Vec::new());
            }
            return Ok(self.executor.execute_steps(
                contract,
                self.config.fail_step.as_deref(),
                now_ms,
            ));
        };

        let mut commit_inputs = Vec::new();
        for step in &contract.plan.steps {
            let idem_key = commit_idempotency_key(contract, &step.step_id.0)?;
            let mut reservation = store
                .acquire_durable_reservation(execution_id, &idem_key, logical_now_ms)
                .map_err(|err| {
                    TxExecutionError::LedgerWrite(format!(
                        "failed to acquire durable reservation for commit step {}: {err}",
                        step.step_id.0
                    ))
                })?;
            if let Some(outcome) = reservation.observed_outcome() {
                commit_inputs.push(deduped_commit_input(&step.step_id, outcome, now_ms)?);
                continue;
            }
            if !dispatch_allowed {
                continue;
            }

            let risk = contract_step_risk(contract, step.step_id.0.as_str());
            let agent_id = format!("agent-{}", step.step_id.0);
            store
                .record_execution_reserved(
                    &mut reservation,
                    execution_id,
                    idem_key.clone(),
                    StepOutcome::Pending,
                    risk,
                    &agent_id,
                    logical_now_ms,
                )
                .map_err(|err| {
                    TxExecutionError::LedgerWrite(format!(
                        "failed to reserve commit step {} before dispatch: {err}",
                        step.step_id.0
                    ))
                })?;

            // Reserve only the next step, then dispatch exactly that step. A
            // later step must never become Pending before an earlier external
            // effect has a durable terminal outcome.
            let mut dispatch_contract = contract.clone();
            dispatch_contract.plan.steps = vec![step.clone()];
            dispatch_contract
                .plan
                .compensations
                .retain(|compensation| compensation.for_step_id == step.step_id);
            let mut dispatched = self.executor.execute_steps(
                &dispatch_contract,
                self.config.fail_step.as_deref(),
                now_ms,
            );
            if dispatched.len() != 1 || dispatched[0].step_id != step.step_id {
                return Err(TxExecutionError::CommitPhase(format!(
                    "step executor returned {} results while dispatching commit step {}",
                    dispatched.len(),
                    step.step_id.0
                )));
            }
            let input = dispatched.remove(0);
            let succeeded = input.success;
            let outcome = if succeeded {
                StepOutcome::Success {
                    result: Some(input.reason_code.clone()),
                }
            } else {
                StepOutcome::Failed {
                    error_code: input.reason_code.clone(),
                    error_message: format!("Step {} failed", step.step_id.0),
                    compensated: false,
                }
            };
            store
                .complete_execution_reserved(
                    reservation,
                    execution_id,
                    idem_key,
                    outcome,
                    logical_now_ms,
                )
                .map_err(|err| {
                    TxExecutionError::LedgerWrite(format!(
                        "commit step {} was dispatched but its terminal outcome could not be durably recorded; later steps were not dispatched: {err}",
                        step.step_id.0
                    ))
                })?;
            commit_inputs.push(input);
            if !succeeded {
                break;
            }
        }

        Ok(commit_inputs)
    }

    fn acquire_atomic_compensation_proof(
        contract: &MissionTxContract,
        commit_report: &mut TxCommitReport,
        store: &mut IdempotencyStore,
        logical_now_ms: u64,
    ) -> Result<(HashMap<String, StepOutcome>, DurableKeyLeaseSet), TxExecutionError> {
        let proof_keys = rollback_proof_idempotency_keys(contract, commit_report)?;
        let leases = store
            .acquire_durable_key_leases(proof_keys, logical_now_ms)
            .map_err(classify_rollback_proof_lease_error)?;
        let outcomes = Self::authoritative_commit_outcomes_with_leases(contract, &leases)?;
        Self::restore_durable_already_compensated_candidates(commit_report, &outcomes)?;
        Self::validate_authoritative_compensation_outcomes_with_leases(
            contract,
            commit_report,
            &outcomes,
            &leases,
        )?;
        Ok((outcomes, leases))
    }

    fn authoritative_commit_outcomes_with_leases(
        contract: &MissionTxContract,
        leases: &DurableKeyLeaseSet,
    ) -> Result<HashMap<String, StepOutcome>, TxExecutionError> {
        Self::authoritative_commit_outcomes_from_observer(contract, |idem_key, step_id| {
            leases
                .get(idem_key)
                .map(|lease| lease.observed_outcome().cloned())
                .ok_or_else(|| {
                    TxExecutionError::LedgerWrite(format!(
                        "atomic rollback proof lease set is missing commit key for step {step_id}"
                    ))
                })
        })
    }

    fn authoritative_commit_outcomes_from_observer<F>(
        contract: &MissionTxContract,
        mut observe: F,
    ) -> Result<HashMap<String, StepOutcome>, TxExecutionError>
    where
        F: FnMut(&IdempotencyKey, &str) -> Result<Option<StepOutcome>, TxExecutionError>,
    {
        let receipt_candidate_step_ids = contract
            .receipts
            .iter()
            .filter_map(|receipt| {
                if receipt.get("phase").and_then(serde_json::Value::as_str) == Some("commit")
                    && receipt.get("outcome").and_then(serde_json::Value::as_str)
                        == Some("committed")
                {
                    receipt
                        .get("step_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                } else {
                    None
                }
            })
            .collect::<HashSet<_>>();
        let mut outcomes = HashMap::new();
        for step in &contract.plan.steps {
            let step_id = &step.step_id.0;
            let is_receipt_candidate = receipt_candidate_step_ids.contains(step_id);
            let idem_key = commit_idempotency_key(contract, step_id)?;
            let observed = observe(&idem_key, step_id)?;
            match observed {
                Some(outcome @ StepOutcome::Success { .. }) if is_receipt_candidate => {
                    outcomes.insert(step_id.clone(), outcome);
                }
                Some(StepOutcome::Success { .. }) => {
                    return Err(TxExecutionError::RollbackProof {
                        kind: RollbackProofKind::Conflict,
                        step_id: step_id.clone(),
                        detail:
                            "authoritative durable commit Success is omitted or downgraded by the contract receipt history; reconcile the contract and ledger before rollback"
                                .to_string(),
                    });
                }
                Some(other) if is_receipt_candidate => {
                    return Err(TxExecutionError::RollbackProof {
                        kind: RollbackProofKind::Conflict,
                        step_id: step_id.clone(),
                        detail: format!(
                            "rollback requires authoritative durable commit Success, got {other:?}"
                        ),
                    });
                }
                None if is_receipt_candidate => {
                    return Err(TxExecutionError::RollbackProof {
                        kind: RollbackProofKind::Missing,
                        step_id: step_id.clone(),
                        detail:
                            "rollback receipt identifies a commit candidate but no authoritative durable commit Success exists"
                                .to_string(),
                    });
                }
                Some(other)
                    if matches!(
                        &other,
                        StepOutcome::Pending
                            | StepOutcome::Compensated { .. }
                            | StepOutcome::Failed {
                                compensated: true,
                                ..
                            }
                    ) =>
                {
                    return Err(TxExecutionError::RollbackProof {
                        kind: RollbackProofKind::Conflict,
                        step_id: step_id.clone(),
                        detail: format!(
                            "authoritative durable commit state {other:?} is ambiguous or impossible while the receipt history does not identify a committed effect; reconcile the contract and ledger before rollback"
                        ),
                    });
                }
                Some(
                    StepOutcome::Failed {
                        compensated: false, ..
                    }
                    | StepOutcome::Skipped { .. },
                )
                | None => {}
                Some(other) => {
                    return Err(TxExecutionError::RollbackProof {
                        kind: RollbackProofKind::Conflict,
                        step_id: step_id.clone(),
                        detail: format!(
                            "authoritative durable commit state {other:?} is ambiguous or impossible while the receipt history does not identify a committed effect; reconcile the contract and ledger before rollback"
                        ),
                    });
                }
            }
        }
        Ok(outcomes)
    }

    fn validate_authoritative_compensation_outcomes_with_leases(
        contract: &MissionTxContract,
        commit_report: &TxCommitReport,
        original_commit_outcomes: &HashMap<String, StepOutcome>,
        leases: &DurableKeyLeaseSet,
    ) -> Result<(), TxExecutionError> {
        Self::validate_authoritative_compensation_outcomes_from_observer(
            contract,
            commit_report,
            original_commit_outcomes,
            |idem_key, step_id| {
                leases
                    .get(idem_key)
                    .map(|lease| lease.observed_outcome().cloned())
                    .ok_or_else(|| {
                        TxExecutionError::LedgerWrite(format!(
                            "atomic rollback proof lease set is missing compensation key for step {step_id}"
                        ))
                    })
            },
        )
    }

    fn validate_authoritative_compensation_outcomes_from_observer<F>(
        contract: &MissionTxContract,
        commit_report: &TxCommitReport,
        original_commit_outcomes: &HashMap<String, StepOutcome>,
        mut observe: F,
    ) -> Result<(), TxExecutionError>
    where
        F: FnMut(&IdempotencyKey, &str) -> Result<Option<StepOutcome>, TxExecutionError>,
    {
        for step_result in commit_report
            .step_results
            .iter()
            .filter(|result| result.outcome.is_committed())
        {
            let idem_key = compensation_idempotency_key(contract, &step_result.step_id.0)?;
            let observed = observe(&idem_key, &step_result.step_id.0)?;
            match observed {
                Some(StepOutcome::Compensated {
                    original_outcome, ..
                }) => {
                    let expected = original_commit_outcomes.get(&step_result.step_id.0);
                    if expected != Some(original_outcome.as_ref()) {
                        return Err(TxExecutionError::RollbackProof {
                            kind: RollbackProofKind::Conflict,
                            step_id: step_result.step_id.0.clone(),
                            detail: format!(
                                "authoritative durable compensation proof embeds original outcome {:?}, but the separately proven commit outcome is {expected:?}; reconcile the contract and ledger before rollback",
                                original_outcome.as_ref()
                            ),
                        });
                    }
                }
                Some(
                    StepOutcome::Failed {
                        compensated: false, ..
                    }
                    | StepOutcome::Skipped { .. },
                )
                | None => {}
                Some(other) => {
                    return Err(TxExecutionError::RollbackProof {
                        kind: RollbackProofKind::Conflict,
                        step_id: step_result.step_id.0.clone(),
                        detail: format!(
                            "authoritative durable compensation state {other:?} is ambiguous or impossible; reconcile the contract and ledger before rollback"
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    fn restore_durable_already_compensated_candidates(
        commit_report: &mut TxCommitReport,
        original_commit_outcomes: &HashMap<String, StepOutcome>,
    ) -> Result<(), TxExecutionError> {
        let mut restored = 0usize;
        for step_result in &mut commit_report.step_results {
            let already_compensated = matches!(
                &step_result.outcome,
                crate::plan::TxCommitStepOutcome::Skipped { reason_code }
                    if reason_code == "already_compensated"
            );
            if !already_compensated {
                continue;
            }
            if !matches!(
                original_commit_outcomes.get(&step_result.step_id.0),
                Some(StepOutcome::Success { .. })
            ) {
                return Err(TxExecutionError::RollbackProof {
                    kind: RollbackProofKind::Conflict,
                    step_id: step_result.step_id.0.clone(),
                    detail:
                        "already-compensated receipt lacks authoritative durable commit Success"
                            .to_string(),
                });
            }
            step_result.outcome = crate::plan::TxCommitStepOutcome::Committed {
                reason_code: "durable_commit_success".to_string(),
            };
            step_result.decision_path =
                "rollback_receipt_candidate->durable_commit_success".to_string();
            restored = restored.checked_add(1).ok_or_else(|| {
                TxExecutionError::InvalidContract(
                    "durable rollback candidate count overflow".to_string(),
                )
            })?;
        }
        if restored == 0 {
            return Ok(());
        }
        commit_report.committed_count = commit_report
            .committed_count
            .checked_add(restored)
            .ok_or_else(|| {
                TxExecutionError::InvalidContract(
                    "durable rollback committed count overflow".to_string(),
                )
            })?;
        commit_report.skipped_count = commit_report
            .skipped_count
            .checked_sub(restored)
            .ok_or_else(|| {
                TxExecutionError::InvalidContract(
                    "durable rollback skipped count underflow".to_string(),
                )
            })?;
        let (outcome, reason_code) = if commit_report.failed_count == 0 {
            (TxCommitOutcome::FullyCommitted, "fully_committed")
        } else if commit_report.committed_count == 0 {
            (TxCommitOutcome::ImmediateFailure, "immediate_failure")
        } else {
            (TxCommitOutcome::PartialFailure, "partial_failure")
        };
        commit_report.outcome = outcome;
        commit_report.reason_code = reason_code.to_string();
        Ok(())
    }

    fn compensation_inputs_with_dedup(
        &self,
        contract: &MissionTxContract,
        commit_report: &TxCommitReport,
        execution_id: &str,
        store: Option<&mut IdempotencyStore>,
        prevalidated_original_commit_outcomes: Option<&HashMap<String, StepOutcome>>,
        mut preacquired_rollback_proof_leases: Option<&mut DurableKeyLeaseSet>,
        dispatch_permit: &CompensationDispatchPermit,
        now_ms: i64,
    ) -> Result<CompensationInputsWithDedupOutput, TxExecutionError> {
        let logical_now_ms = checked_logical_now_ms(now_ms)?;
        if store.is_some()
            && (prevalidated_original_commit_outcomes.is_none()
                || preacquired_rollback_proof_leases.is_none())
        {
            return Err(TxExecutionError::LedgerWrite(
                "durable compensation requires a continuously held atomic proof set acquired before mutation"
                    .to_string(),
            ));
        }
        let original_commit_outcomes =
            if let Some(prevalidated) = prevalidated_original_commit_outcomes {
                prevalidated.clone()
            } else {
                commit_report
                    .step_results
                    .iter()
                    .filter(|result| result.outcome.is_committed())
                    .map(|result| {
                        committed_step_success_outcome(result)
                            .map(|outcome| (result.step_id.0.clone(), outcome))
                    })
                    .collect::<Result<HashMap<_, _>, _>>()?
            };
        let Some(store) = store else {
            for step_result in commit_report
                .step_results
                .iter()
                .filter(|result| result.outcome.is_committed())
            {
                if !dispatch_permit.authorizes(&step_result.step_id.0) {
                    return Err(TxExecutionError::CompensationPhase(format!(
                        "compensation step {} lacks a pre-mutation dispatch permit",
                        step_result.step_id.0
                    )));
                }
            }
            let inputs = self.executor.execute_compensations(
                contract,
                commit_report,
                self.config.fail_compensation_for_step.as_deref(),
                now_ms,
            );
            let keys = inputs
                .iter()
                .map(|input| {
                    compensation_idempotency_key(contract, &input.for_step_id.0)
                        .map(|key| (input.for_step_id.0.clone(), key))
                })
                .collect::<Result<HashMap<_, _>, TxExecutionError>>()?;
            return Ok((inputs, keys, HashMap::new(), original_commit_outcomes));
        };

        let mut compensation_inputs = Vec::new();
        let mut compensation_keys = HashMap::new();
        let mut authoritative_recovery_outcomes = HashMap::new();

        'steps: for step_result in commit_report
            .step_results
            .iter()
            .rev()
            .filter(|result| result.outcome.is_committed())
        {
            let original_commit_outcome = original_commit_outcomes
                .get(&step_result.step_id.0)
                .cloned()
                .ok_or_else(|| TxExecutionError::DedupConflict {
                    step_id: step_result.step_id.0.clone(),
                    outcome: "compensation candidate lacks authoritative commit Success"
                        .to_string(),
                })?;
            if !matches!(&original_commit_outcome, StepOutcome::Success { .. }) {
                return Err(TxExecutionError::DedupConflict {
                    step_id: step_result.step_id.0.clone(),
                    outcome: format!(
                        "compensation candidate requires authoritative commit Success, got {original_commit_outcome:?}"
                    ),
                });
            }
            let idem_key = compensation_idempotency_key(contract, &step_result.step_id.0)?;
            let risk = compensation_step_risk(contract, step_result.step_id.0.as_str());
            let agent_id = format!("agent-{}", step_result.step_id.0);
            let mut reservation = if let Some(leases) =
                preacquired_rollback_proof_leases.as_deref_mut()
            {
                let lease = leases.take(&idem_key).ok_or_else(|| {
                    TxExecutionError::LedgerWrite(format!(
                        "atomic rollback proof lease set lost compensation key for step {}",
                        step_result.step_id.0
                    ))
                })?;
                store
                    .bind_durable_key_lease(execution_id, lease)
                    .map_err(|err| {
                        TxExecutionError::LedgerWrite(format!(
                            "failed to bind pre-acquired durable compensation lease for step {}: {err}",
                            step_result.step_id.0
                        ))
                    })?
            } else {
                store
                    .acquire_durable_reservation(execution_id, &idem_key, logical_now_ms)
                    .map_err(|err| {
                        TxExecutionError::LedgerWrite(format!(
                            "failed to acquire durable reservation for compensation step {}: {err}",
                            step_result.step_id.0
                        ))
                    })?
            };
            match reservation.observed_outcome() {
                Some(outcome @ StepOutcome::Compensated { .. }) => {
                    let recovered_outcome = outcome.clone();
                    compensation_inputs.push(deduped_compensation_input(
                        &step_result.step_id,
                        &recovered_outcome,
                        now_ms,
                    )?);
                    store
                        .record_recovered_execution_reserved(
                            reservation,
                            execution_id,
                            idem_key.clone(),
                            recovered_outcome.clone(),
                            risk,
                            &agent_id,
                            logical_now_ms,
                        )
                        .map_err(|err| {
                            TxExecutionError::LedgerWrite(format!(
                                "failed to link recovered durable compensation proof for step {}: {err}",
                                step_result.step_id.0
                            ))
                        })?;
                    authoritative_recovery_outcomes
                        .insert(step_result.step_id.0.clone(), recovered_outcome);
                    compensation_keys.insert(step_result.step_id.0.clone(), idem_key);
                    continue 'steps;
                }
                Some(
                    StepOutcome::Failed {
                        compensated: false, ..
                    }
                    | StepOutcome::Skipped { .. },
                )
                | None => {}
                Some(other) => {
                    return Err(TxExecutionError::DedupConflict {
                        step_id: step_result.step_id.0.clone(),
                        outcome: format!("{other:?}"),
                    });
                }
            }

            if !dispatch_permit.authorizes(&step_result.step_id.0) {
                return Err(TxExecutionError::CompensationPhase(format!(
                    "compensation step {} lacks a pre-mutation dispatch permit",
                    step_result.step_id.0
                )));
            }

            let mut dispatch_report = commit_report.clone();
            dispatch_report.step_results = vec![step_result.clone()];
            store
                .record_execution_reserved(
                    &mut reservation,
                    execution_id,
                    idem_key.clone(),
                    StepOutcome::Pending,
                    risk,
                    &agent_id,
                    logical_now_ms,
                )
                .map_err(|err| {
                    TxExecutionError::LedgerWrite(format!(
                        "failed to reserve compensation step {} before dispatch: {err}",
                        step_result.step_id.0
                    ))
                })?;
            compensation_keys.insert(step_result.step_id.0.clone(), idem_key.clone());

            let mut dispatched = self.executor.execute_compensations(
                contract,
                &dispatch_report,
                self.config.fail_compensation_for_step.as_deref(),
                now_ms,
            );
            if dispatched.len() != 1 || dispatched[0].for_step_id != step_result.step_id {
                return Err(TxExecutionError::CompensationPhase(format!(
                    "step executor returned {} results while dispatching compensation for step {}",
                    dispatched.len(),
                    step_result.step_id.0
                )));
            }
            let input = dispatched.remove(0);
            let succeeded = input.success;
            let outcome = if succeeded {
                StepOutcome::Compensated {
                    original_outcome: Box::new(original_commit_outcome),
                    compensation_result: "rollback_complete".to_string(),
                }
            } else {
                StepOutcome::Failed {
                    error_code: "compensation_failed".to_string(),
                    error_message: format!(
                        "Compensation for step {} failed",
                        step_result.step_id.0
                    ),
                    compensated: false,
                }
            };
            store
                .complete_execution_reserved(
                    reservation,
                    execution_id,
                    idem_key,
                    outcome,
                    logical_now_ms,
                )
                .map_err(|err| {
                    TxExecutionError::LedgerWrite(format!(
                        "compensation for step {} was dispatched but its terminal outcome could not be durably recorded; later compensations were not dispatched: {err}",
                        step_result.step_id.0
                    ))
                })?;
            compensation_inputs.push(input);
            if !succeeded {
                break 'steps;
            }
        }

        Ok((
            compensation_inputs,
            compensation_keys,
            authoritative_recovery_outcomes,
            original_commit_outcomes,
        ))
    }

    // ── Ledger Recording ─────────────────────────────────────────────────────

    fn record_commit_results_to_ledger(
        &self,
        contract: &MissionTxContract,
        commit_report: &TxCommitReport,
        mut context: TxLedgerRecordingContext<'_>,
    ) -> Result<(), TxExecutionError> {
        let timestamp_ms = checked_logical_now_ms(context.now_ms)?;
        for step_result in &commit_report.step_results {
            // Skipped results describe work that was never dispatched (pause,
            // kill switch, batch suspension, or post-failure boundary). They
            // must not poison the stable side-effect dedup namespace.
            if matches!(
                step_result.outcome,
                crate::plan::TxCommitStepOutcome::Skipped { .. }
            ) {
                continue;
            }
            let idem_key = commit_idempotency_key(contract, &step_result.step_id.0)?;

            if let Some(local_outcome) = context.ledger.get_outcome(&idem_key).cloned() {
                if let Some(store) = context.store.as_deref_mut() {
                    let authoritative_outcome = store
                        .get_ledger(context.execution_id)
                        .and_then(|ledger| ledger.get_outcome(&idem_key))
                        .cloned();
                    if authoritative_outcome.as_ref() != Some(&local_outcome) {
                        return Err(TxExecutionError::DedupConflict {
                            step_id: step_result.step_id.0.clone(),
                            outcome: format!(
                                "local recovered outcome {local_outcome:?} conflicts with current execution ledger outcome {authoritative_outcome:?}"
                            ),
                        });
                    }
                    refresh_local_execution_ledger(context.ledger, store, context.execution_id)?;
                }
                continue;
            }

            let outcome = match &step_result.outcome {
                crate::plan::TxCommitStepOutcome::Committed { reason_code } => {
                    StepOutcome::Success {
                        result: Some(reason_code.clone()),
                    }
                }
                crate::plan::TxCommitStepOutcome::Failed { reason_code } => StepOutcome::Failed {
                    error_code: reason_code.clone(),
                    error_message: format!("Step {} failed", step_result.step_id.0),
                    compensated: false,
                },
                crate::plan::TxCommitStepOutcome::Skipped { .. } => {
                    unreachable!("skipped commit outcomes are filtered before ledger recording")
                }
            };

            let risk = contract_step_risk(contract, step_result.step_id.0.as_str());
            let agent_id = format!("agent-{}", step_result.step_id.0);
            if let Some(store) = context.store.as_deref_mut() {
                let durable = store.is_durable();
                let current_outcome = store
                    .get_ledger(context.execution_id)
                    .and_then(|ledger| ledger.get_outcome(&idem_key))
                    .cloned();
                let store_result = match current_outcome {
                    Some(existing) if existing.is_pending() && durable => {
                        return Err(TxExecutionError::LedgerWrite(format!(
                            "durable commit step {} is still Pending after reservation-bound dispatch completion",
                            step_result.step_id.0
                        )));
                    }
                    Some(existing) if existing.is_pending() => store.complete_execution(
                        context.execution_id,
                        idem_key.clone(),
                        outcome.clone(),
                        timestamp_ms,
                    ),
                    Some(existing) if existing == outcome => Ok(String::new()),
                    Some(existing) => {
                        return Err(TxExecutionError::DedupConflict {
                            step_id: step_result.step_id.0.clone(),
                            outcome: format!(
                                "durable outcome {existing:?} conflicts with report outcome {outcome:?}"
                            ),
                        });
                    }
                    None if durable => {
                        let recovered = context
                            .authoritative_recovery_outcomes
                            .and_then(|outcomes| outcomes.get(&step_result.step_id.0))
                            .cloned()
                            .or_else(|| store.peek_cached_outcome(&idem_key, timestamp_ms).cloned())
                            .ok_or_else(|| TxExecutionError::DedupConflict {
                                step_id: step_result.step_id.0.clone(),
                                outcome:
                                    "missing durable terminal proof for recovered commit result"
                                        .to_string(),
                            })?;
                        let equivalent = matches!(
                            (&step_result.outcome, &recovered),
                            (
                                crate::plan::TxCommitStepOutcome::Committed { .. },
                                StepOutcome::Success { .. }
                            )
                        );
                        if !equivalent {
                            return Err(TxExecutionError::DedupConflict {
                                step_id: step_result.step_id.0.clone(),
                                outcome: format!(
                                    "durable recovered outcome {recovered:?} conflicts with report outcome {:?}",
                                    step_result.outcome
                                ),
                            });
                        }
                        store.record_recovered_execution(
                            context.execution_id,
                            idem_key.clone(),
                            recovered,
                            risk,
                            &agent_id,
                            timestamp_ms,
                        )
                    }
                    None => store.record_execution(
                        context.execution_id,
                        idem_key.clone(),
                        outcome.clone(),
                        risk,
                        &agent_id,
                        timestamp_ms,
                    ),
                };
                store_result.map_err(|err| {
                    TxExecutionError::LedgerWrite(format!(
                        "failed to record commit step {} in idempotency store: {err}",
                        step_result.step_id.0
                    ))
                })?;
                refresh_local_execution_ledger(context.ledger, store, context.execution_id)?;
            } else {
                context
                    .ledger
                    .append(idem_key, outcome, risk, &agent_id, timestamp_ms)
                    .map_err(|err| {
                        TxExecutionError::LedgerWrite(format!(
                            "failed to record commit step {} in idempotency ledger: {err}",
                            step_result.step_id.0
                        ))
                    })?;
            }

            let event_kind = if step_result.outcome.is_committed() {
                TxEventKind::StepCommitted
            } else {
                TxEventKind::StepFailed
            };

            context.events.push(self.make_event(
                event_kind,
                TxObservabilityPhase::Commit,
                &format!(
                    "tx.commit.step_{}",
                    if step_result.outcome.is_committed() {
                        "committed"
                    } else {
                        "failed"
                    }
                ),
                context.execution_id,
                &contract.plan.plan_id.0,
                TxPhase::Committing,
                context.now_ms,
            ));
        }

        Ok(())
    }

    fn record_compensation_results_to_ledger(
        &self,
        contract: &MissionTxContract,
        comp_report: &TxCompensationReport,
        compensation_idem_keys: &HashMap<String, IdempotencyKey>,
        original_commit_outcomes: &HashMap<String, StepOutcome>,
        mut context: TxLedgerRecordingContext<'_>,
    ) -> Result<(), TxExecutionError> {
        let timestamp_ms = checked_logical_now_ms(context.now_ms)?;
        for receipt in &comp_report.receipts {
            if let Some(step_id) = receipt.get("step_id").and_then(|v| v.as_str()) {
                let outcome_str = receipt
                    .get("outcome")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                // The phase report synthesizes skipped receipts after the
                // first failure. Those steps were never dispatched or
                // reserved, so they have no side-effect ledger record.
                if outcome_str == "skipped" {
                    continue;
                }
                let idem_key = compensation_idem_keys
                    .get(step_id)
                    .cloned()
                    .ok_or_else(|| {
                        TxExecutionError::LedgerWrite(format!(
                            "missing reserved idempotency key for compensation step {step_id}"
                        ))
                    })?;

                if let Some(local_outcome) = context.ledger.get_outcome(&idem_key).cloned() {
                    if let Some(store) = context.store.as_deref_mut() {
                        let authoritative_outcome = store
                            .get_ledger(context.execution_id)
                            .and_then(|ledger| ledger.get_outcome(&idem_key))
                            .cloned();
                        if authoritative_outcome.as_ref() != Some(&local_outcome) {
                            return Err(TxExecutionError::DedupConflict {
                                step_id: step_id.to_string(),
                                outcome: format!(
                                    "local compensation outcome {local_outcome:?} conflicts with current execution ledger outcome {authoritative_outcome:?}"
                                ),
                            });
                        }
                        refresh_local_execution_ledger(
                            context.ledger,
                            store,
                            context.execution_id,
                        )?;
                    }
                    continue;
                }

                let outcome = if let Some(recovered) = context
                    .authoritative_recovery_outcomes
                    .and_then(|outcomes| outcomes.get(step_id))
                {
                    let equivalent = matches!(
                        (outcome_str, recovered),
                        ("compensated", StepOutcome::Compensated { .. })
                            | (
                                "failed",
                                StepOutcome::Failed {
                                    compensated: false,
                                    ..
                                }
                            )
                    );
                    if !equivalent {
                        return Err(TxExecutionError::DedupConflict {
                            step_id: step_id.to_string(),
                            outcome: format!(
                                "authoritative recovered compensation outcome {recovered:?} conflicts with report outcome {outcome_str}"
                            ),
                        });
                    }
                    recovered.clone()
                } else if outcome_str == "compensated" {
                    let original_outcome = original_commit_outcomes
                        .get(step_id)
                        .cloned()
                        .ok_or_else(|| {
                            TxExecutionError::LedgerWrite(format!(
                                "missing successful original outcome for compensated step {step_id}"
                            ))
                        })?;
                    if !matches!(&original_outcome, StepOutcome::Success { .. }) {
                        return Err(TxExecutionError::DedupConflict {
                            step_id: step_id.to_string(),
                            outcome: format!(
                                "compensated step requires successful original outcome, got {original_outcome:?}"
                            ),
                        });
                    }
                    StepOutcome::Compensated {
                        original_outcome: Box::new(original_outcome),
                        compensation_result: "rollback_complete".to_string(),
                    }
                } else {
                    StepOutcome::Failed {
                        error_code: "compensation_failed".to_string(),
                        error_message: format!("Compensation for step {step_id} failed"),
                        compensated: false,
                    }
                };

                let risk = compensation_step_risk(contract, step_id);
                let agent_id = format!("agent-{step_id}");
                if let Some(store) = context.store.as_deref_mut() {
                    let durable = store.is_durable();
                    let current_outcome = store
                        .get_ledger(context.execution_id)
                        .and_then(|ledger| ledger.get_outcome(&idem_key))
                        .cloned();
                    let store_result = match current_outcome {
                        Some(existing) if existing.is_pending() && durable => {
                            return Err(TxExecutionError::LedgerWrite(format!(
                                "durable compensation step {step_id} is still Pending after reservation-bound dispatch completion"
                            )));
                        }
                        Some(existing) if existing.is_pending() => store.complete_execution(
                            context.execution_id,
                            idem_key.clone(),
                            outcome.clone(),
                            timestamp_ms,
                        ),
                        Some(existing) if existing == outcome => Ok(String::new()),
                        Some(existing) => {
                            return Err(TxExecutionError::DedupConflict {
                                step_id: step_id.to_string(),
                                outcome: format!(
                                    "durable compensation outcome {existing:?} conflicts with report outcome {outcome:?}"
                                ),
                            });
                        }
                        None if durable => {
                            let recovered = context
                                .authoritative_recovery_outcomes
                                .and_then(|outcomes| outcomes.get(step_id))
                                .cloned()
                                .or_else(|| {
                                    store
                                        .peek_cached_outcome(&idem_key, timestamp_ms)
                                        .cloned()
                                })
                                .ok_or_else(|| TxExecutionError::DedupConflict {
                                    step_id: step_id.to_string(),
                                    outcome: "missing durable terminal proof for recovered compensation result"
                                        .to_string(),
                                })?;
                            let equivalent = matches!(
                                (outcome_str, &recovered),
                                ("compensated", StepOutcome::Compensated { .. })
                            );
                            if !equivalent {
                                return Err(TxExecutionError::DedupConflict {
                                    step_id: step_id.to_string(),
                                    outcome: format!(
                                        "durable recovered compensation outcome {recovered:?} conflicts with report outcome {outcome_str}"
                                    ),
                                });
                            }
                            store.record_recovered_execution(
                                context.execution_id,
                                idem_key.clone(),
                                recovered,
                                risk,
                                &agent_id,
                                timestamp_ms,
                            )
                        }
                        None => store.record_execution(
                            context.execution_id,
                            idem_key.clone(),
                            outcome.clone(),
                            risk,
                            &agent_id,
                            timestamp_ms,
                        ),
                    };
                    store_result.map_err(|err| {
                        TxExecutionError::LedgerWrite(format!(
                            "failed to record compensation step {step_id} in idempotency store: {err}"
                        ))
                    })?;
                    refresh_local_execution_ledger(context.ledger, store, context.execution_id)?;
                } else {
                    context
                        .ledger
                        .append(idem_key, outcome, risk, &agent_id, timestamp_ms)
                        .map_err(|err| {
                            TxExecutionError::LedgerWrite(format!(
                                "failed to record compensation step {step_id} in idempotency ledger: {err}"
                            ))
                        })?;
                }

                context.events.push(self.make_event(
                    TxEventKind::StepCompensated,
                    TxObservabilityPhase::Compensate,
                    &format!("tx.compensate.step_{outcome_str}"),
                    context.execution_id,
                    &contract.plan.plan_id.0,
                    TxPhase::Compensating,
                    context.now_ms,
                ));
            }
        }

        // Contract receipt deduplication is independent of execution-ledger
        // proof. A recovered compensation whose equivalent receipt was
        // already saved is absent from `comp_report.receipts`, but the current
        // execution must still link the durable outcome before it can seal.
        if let Some(store) = context.store.as_deref_mut() {
            let ordinal_by_step = contract
                .plan
                .steps
                .iter()
                .map(|step| (step.step_id.0.as_str(), step.ordinal))
                .collect::<HashMap<_, _>>();
            let mut proof_entries = compensation_idem_keys.iter().collect::<Vec<_>>();
            proof_entries.sort_by_key(|(step_id, _)| {
                ordinal_by_step
                    .get(step_id.as_str())
                    .copied()
                    .unwrap_or(usize::MAX)
            });
            proof_entries.reverse();

            for (step_id, idem_key) in proof_entries {
                if context.ledger.is_executed(idem_key) {
                    continue;
                }
                if store
                    .get_ledger(context.execution_id)
                    .and_then(|ledger| ledger.get_outcome(idem_key))
                    .is_some()
                {
                    refresh_local_execution_ledger(context.ledger, store, context.execution_id)?;
                    if context.ledger.is_executed(idem_key) {
                        continue;
                    }
                }

                let recovered = context
                    .authoritative_recovery_outcomes
                    .and_then(|outcomes| outcomes.get(step_id.as_str()))
                    .cloned()
                    .or_else(|| store.peek_cached_outcome(idem_key, timestamp_ms).cloned())
                    .ok_or_else(|| TxExecutionError::DedupConflict {
                        step_id: step_id.clone(),
                        outcome:
                            "missing durable compensated proof for receipt-deduplicated recovery"
                                .to_string(),
                    })?;
                if !matches!(&recovered, StepOutcome::Compensated { .. }) {
                    return Err(TxExecutionError::DedupConflict {
                        step_id: step_id.clone(),
                        outcome: format!(
                            "receipt-deduplicated compensation requires durable Compensated proof, got {recovered:?}"
                        ),
                    });
                }

                let risk = compensation_step_risk(contract, step_id);
                let agent_id = format!("agent-{step_id}");
                store
                    .record_recovered_execution(
                        context.execution_id,
                        idem_key.clone(),
                        recovered,
                        risk,
                        &agent_id,
                        timestamp_ms,
                    )
                    .map_err(|err| {
                        TxExecutionError::LedgerWrite(format!(
                            "failed to link receipt-deduplicated compensation step {step_id}: {err}"
                        ))
                    })?;
                refresh_local_execution_ledger(context.ledger, store, context.execution_id)?;
                context.events.push(self.make_event(
                    TxEventKind::StepCompensated,
                    TxObservabilityPhase::Compensate,
                    "tx.compensate.step_deduped",
                    context.execution_id,
                    &contract.plan.plan_id.0,
                    TxPhase::Compensating,
                    context.now_ms,
                ));
            }
        }

        Ok(())
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn determine_final_outcome(
        current_state: MissionTxState,
        commit_report: &TxCommitReport,
        compensation_report: Option<&TxCompensationReport>,
    ) -> (MissionTxState, TxOutcome) {
        if commit_report.is_fully_committed() {
            return (MissionTxState::Committed, TxOutcome::Committed);
        }

        // Paused: remain in a resumable state, not Failed. The commit was
        // suspended before completion — no steps failed, so this is not a
        // failure. The operator can resume later.
        if commit_report.outcome == TxCommitOutcome::PauseSuspended {
            return (current_state, TxOutcome::Pending);
        }

        if let Some(comp) = compensation_report {
            if comp.is_fully_rolled_back() {
                return (MissionTxState::RolledBack, TxOutcome::Compensated);
            }
            if comp.has_residual_risk() {
                return (MissionTxState::Failed, TxOutcome::Failed);
            }
            if current_state == MissionTxState::Compensated {
                return (MissionTxState::Compensated, TxOutcome::Compensated);
            }
        }

        (current_state, TxOutcome::Failed)
    }

    fn should_run_compensation(commit_report: &TxCommitReport, auto_compensate: bool) -> bool {
        auto_compensate
            && commit_report.has_failures()
            && !matches!(commit_report.outcome, TxCommitOutcome::KillSwitchBlocked)
    }

    // Tx events are emitted from ledger state with all event dimensions explicit.
    #[allow(clippy::too_many_arguments)]
    fn make_event(
        &self,
        kind: TxEventKind,
        phase: TxObservabilityPhase,
        reason_code: &str,
        execution_id: &str,
        plan_id: &str,
        tx_phase: TxPhase,
        timestamp_ms: i64,
    ) -> TxObservabilityEvent {
        let seq = self.event_seq.get();
        self.event_seq.set(seq + 1);
        TxObservabilityEvent {
            sequence: seq,
            timestamp_ms: timestamp_ms as u64,
            kind,
            reason_code: reason_code.to_string(),
            phase,
            execution_id: execution_id.to_string(),
            plan_id: plan_id.to_string(),
            plan_hash: 0,
            step_id: String::new(),
            idem_key: String::new(),
            tx_phase,
            chain_hash: String::new(),
            agent_id: String::new(),
            details: HashMap::new(),
        }
    }

    fn record_economic_hard_stop_event(
        &self,
        envelope: &MissionEconomicHardStopEnvelope,
        audit_row: &MissionEconomicAuditRow,
        execution_id: &str,
        plan_id: &str,
        events: &mut Vec<TxObservabilityEvent>,
        now_ms: i64,
    ) {
        let mut event = self.make_event(
            TxEventKind::EconomicHardStop,
            TxObservabilityPhase::Commit,
            crate::tx_observability::reason_codes::ECONOMIC_HARD_STOP,
            execution_id,
            plan_id,
            TxPhase::Aborted,
            now_ms,
        );
        if let Ok(value) = serde_json::to_value(envelope) {
            event.details.insert("envelope".to_string(), value);
        }
        if let Ok(value) = serde_json::to_value(audit_row) {
            event.details.insert("audit_row".to_string(), value);
        }
        events.push(event);
    }

    fn record_prepare_gate_events(
        &self,
        contract: &MissionTxContract,
        execution_id: &str,
        events: &mut Vec<TxObservabilityEvent>,
        gate_inputs: &[TxPrepareGateInput],
        now_ms: i64,
    ) {
        for gate_input in gate_inputs {
            let gate_results = [
                (
                    "plan_preconditions",
                    gate_input.preconditions_satisfied,
                    gate_input.precondition_reason_code.as_deref(),
                ),
                (
                    "policy",
                    gate_input.policy_passed,
                    gate_input.policy_reason_code.as_deref(),
                ),
                (
                    "reservation",
                    gate_input.reservation_available,
                    gate_input.reservation_reason_code.as_deref(),
                ),
                (
                    "approval",
                    gate_input.approval_satisfied,
                    gate_input.approval_reason_code.as_deref(),
                ),
                (
                    "liveness",
                    gate_input.target_liveness,
                    gate_input.liveness_reason_code.as_deref(),
                ),
            ];

            for (gate_name, passed, gate_reason_code) in gate_results {
                let mut event = self.make_event(
                    if passed {
                        TxEventKind::PreconditionValidated
                    } else {
                        TxEventKind::PreconditionFailed
                    },
                    TxObservabilityPhase::Prepare,
                    if passed {
                        crate::tx_observability::reason_codes::PRECONDITION_PASS
                    } else {
                        crate::tx_observability::reason_codes::PRECONDITION_FAIL
                    },
                    execution_id,
                    &contract.plan.plan_id.0,
                    TxPhase::Preparing,
                    now_ms,
                );
                event.step_id.clone_from(&gate_input.step_id.0);
                event.details.insert(
                    "gate".to_string(),
                    serde_json::Value::String(gate_name.to_string()),
                );
                event
                    .details
                    .insert("passed".to_string(), serde_json::Value::Bool(passed));
                if let Some(pane_id) = gate_input.pane_id {
                    event.details.insert(
                        "pane_id".to_string(),
                        serde_json::Value::Number(serde_json::Number::from(pane_id)),
                    );
                }
                if let Some(reason_code) = gate_reason_code {
                    event.details.insert(
                        "gate_reason_code".to_string(),
                        serde_json::Value::String(reason_code.to_string()),
                    );
                }
                if let Some(required_approval) = &gate_input.required_approval
                    && let Ok(value) = serde_json::to_value(required_approval)
                {
                    event.details.insert("required_approval".to_string(), value);
                }
                events.push(event);
            }
        }
    }

    fn maybe_build_forensic_bundle(
        &self,
        contract: &MissionTxContract,
        ledger: &TxExecutionLedger,
        events: &mut Vec<TxObservabilityEvent>,
        resume_ctx: Option<&crate::tx_idempotency::ResumeContext>,
        execution_id: &str,
        now_ms: i64,
    ) -> Option<TxForensicBundle> {
        if !self.config.produce_forensic_bundle {
            return None;
        }

        events.push(self.make_event(
            TxEventKind::BundleExported,
            TxObservabilityPhase::Observability,
            crate::tx_observability::reason_codes::BUNDLE_EXPORTED,
            execution_id,
            &contract.plan.plan_id.0,
            ledger.phase(),
            now_ms,
        ));

        let compiled_plan = compiled_plan_from_contract(contract);
        Some(crate::tx_observability::build_forensic_bundle(
            &compiled_plan,
            ledger,
            events,
            resume_ctx,
            "tx_execution_engine",
            execution_id,
            now_ms as u64,
            &self.config.observability,
        ))
    }
}

fn transition_execution_ledger_pair(
    ledger: &mut TxExecutionLedger,
    store: Option<&mut IdempotencyStore>,
    execution_id: &str,
    next: TxPhase,
) -> Result<(), TxExecutionError> {
    if let Some(store) = store {
        store.transition_phase(execution_id, next).map_err(|err| {
            TxExecutionError::PhaseTransition(format!(
                "failed to durably transition ledger {execution_id} to {next:?}: {err}"
            ))
        })?;
        refresh_local_execution_ledger(ledger, store, execution_id)?;
    } else {
        ledger
            .transition_phase(next)
            .map_err(|err| TxExecutionError::PhaseTransition(err.to_string()))?;
    }

    Ok(())
}

fn refresh_local_execution_ledger(
    ledger: &mut TxExecutionLedger,
    store: &IdempotencyStore,
    execution_id: &str,
) -> Result<(), TxExecutionError> {
    *ledger = store
        .get_ledger(execution_id)
        .cloned()
        .ok_or_else(|| TxExecutionError::LedgerNotFound(execution_id.to_string()))?;
    Ok(())
}

fn reason_code_for_outcome(outcome: &TxOutcome) -> &'static str {
    match outcome {
        TxOutcome::Pending => "pending",
        TxOutcome::Committed => "committed",
        TxOutcome::Failed => "failed",
        TxOutcome::Compensated => "compensated",
    }
}

fn committed_step_success_outcome(
    step_result: &crate::plan::TxCommitStepResult,
) -> Result<StepOutcome, TxExecutionError> {
    match &step_result.outcome {
        crate::plan::TxCommitStepOutcome::Committed { reason_code } => Ok(StepOutcome::Success {
            result: Some(reason_code.clone()),
        }),
        other => Err(TxExecutionError::CompensationPhase(format!(
            "step {} cannot be compensated without a successful commit outcome; got {other:?}",
            step_result.step_id.0
        ))),
    }
}

fn deduped_commit_input(
    step_id: &crate::plan::TxStepId,
    outcome: &StepOutcome,
    now_ms: i64,
) -> Result<TxCommitStepInput, TxExecutionError> {
    match outcome {
        StepOutcome::Success { .. } => Ok(TxCommitStepInput {
            step_id: step_id.clone(),
            success: true,
            reason_code: "commit_step_deduped".to_string(),
            error_code: None,
            completed_at_ms: now_ms,
        }),
        other => Err(TxExecutionError::DedupConflict {
            step_id: step_id.0.clone(),
            outcome: format!("{other:?}"),
        }),
    }
}

fn deduped_compensation_input(
    step_id: &crate::plan::TxStepId,
    outcome: &StepOutcome,
    now_ms: i64,
) -> Result<TxCompensationStepInput, TxExecutionError> {
    match outcome {
        StepOutcome::Compensated { .. } => Ok(TxCompensationStepInput {
            for_step_id: step_id.clone(),
            success: true,
            reason_code: "compensation_step_deduped".to_string(),
            error_code: None,
            completed_at_ms: now_ms,
        }),
        other => Err(TxExecutionError::DedupConflict {
            step_id: step_id.0.clone(),
            outcome: format!("{other:?}"),
        }),
    }
}

fn compiled_plan_from_contract(contract: &MissionTxContract) -> crate::tx_plan_compiler::TxPlan {
    let immutable_hash = contract.compute_hash();
    let plan_hash = u64::from_str_radix(
        immutable_hash
            .strip_prefix("sha256:")
            .and_then(|hash| hash.get(..16))
            .expect("MissionTxContract::compute_hash returns sha256 plus 32 hex digits"),
        16,
    )
    .expect("MissionTxContract::compute_hash returns hexadecimal digits");
    let mut ordered_steps = contract.plan.steps.iter().collect::<Vec<_>>();
    ordered_steps.sort_by_key(|step| step.ordinal);

    let execution_order = ordered_steps
        .iter()
        .map(|step| step.step_id.0.clone())
        .collect::<Vec<_>>();

    let steps = ordered_steps
        .into_iter()
        .map(|step| {
            let step_id = step.step_id.0.clone();
            let compensations = contract
                .plan
                .compensations
                .iter()
                .filter(|comp| comp.for_step_id.0 == step_id)
                .map(|_| crate::tx_plan_compiler::CompensatingAction {
                    step_id: step_id.clone(),
                    description: format!("Resume compensation for {step_id}"),
                    action_type: crate::tx_plan_compiler::CompensationKind::Rollback,
                })
                .collect();

            crate::tx_plan_compiler::TxStep {
                id: step.step_id.0.clone(),
                bead_id: step.step_id.0.clone(),
                agent_id: String::new(),
                description: step.description.clone(),
                depends_on: Vec::new(),
                preconditions: Vec::new(),
                compensations,
                risk: contract_step_risk(contract, step.step_id.0.as_str()),
                score: 1.0,
            }
        })
        .collect::<Vec<_>>();

    let parallel_levels = if execution_order.is_empty() {
        Vec::new()
    } else {
        vec![execution_order.clone()]
    };

    let high_risk_count = steps
        .iter()
        .filter(|step| step.risk == StepRisk::High)
        .count();
    let critical_risk_count = steps
        .iter()
        .filter(|step| step.risk == StepRisk::Critical)
        .count();
    let uncompensated_steps = steps
        .iter()
        .filter(|step| {
            matches!(step.risk, StepRisk::High | StepRisk::Critical)
                && step.compensations.is_empty()
        })
        .count();
    let overall_risk = if critical_risk_count > 0 {
        StepRisk::Critical
    } else if high_risk_count > 0 {
        StepRisk::High
    } else if steps.iter().any(|step| step.risk == StepRisk::Medium) {
        StepRisk::Medium
    } else {
        StepRisk::Low
    };

    crate::tx_plan_compiler::TxPlan {
        plan_id: contract.plan.plan_id.0.clone(),
        plan_hash,
        steps,
        execution_order,
        parallel_levels,
        risk_summary: crate::tx_plan_compiler::TxRiskSummary {
            total_steps: contract.plan.steps.len(),
            high_risk_count,
            critical_risk_count,
            uncompensated_steps,
            overall_risk,
        },
        rejected_edges: Vec::new(),
        rejected_assignments: Vec::new(),
    }
}

const fn successful_terminal_ledger_phase(state: MissionTxState) -> TxPhase {
    match state {
        MissionTxState::Committed | MissionTxState::RolledBack | MissionTxState::Compensated => {
            TxPhase::Completed
        }
        MissionTxState::Failed
        | MissionTxState::Draft
        | MissionTxState::Planned
        | MissionTxState::Prepared
        | MissionTxState::Committing
        | MissionTxState::Compensating => TxPhase::Aborted,
    }
}

fn action_execution_risk(action: &StepAction) -> StepRisk {
    match action {
        StepAction::WaitFor { .. } | StepAction::MarkEventHandled { .. } => StepRisk::Low,
        StepAction::AcquireLock { .. }
        | StepAction::ReleaseLock { .. }
        | StepAction::StoreData { .. } => StepRisk::Medium,
        StepAction::SendText { .. }
        | StepAction::RunWorkflow { .. }
        | StepAction::ValidateApproval { .. }
        | StepAction::NestedPlan { .. }
        | StepAction::Custom { .. } => StepRisk::High,
    }
}

fn contract_step_risk(contract: &MissionTxContract, step_id: &str) -> StepRisk {
    contract
        .plan
        .steps
        .iter()
        .find(|step| step.step_id.0 == step_id)
        .map(|step| action_execution_risk(&step.action))
        .unwrap_or(StepRisk::High)
}

fn compensation_step_risk(contract: &MissionTxContract, step_id: &str) -> StepRisk {
    let original_risk = contract_step_risk(contract, step_id);
    let compensation_risk = contract
        .plan
        .compensations
        .iter()
        .find(|compensation| compensation.for_step_id.0 == step_id)
        .map(|compensation| action_execution_risk(&compensation.action))
        .unwrap_or(StepRisk::High);
    original_risk.max(compensation_risk)
}

fn resume_terminal_outcome(
    contract: &MissionTxContract,
    resume_ctx: &crate::tx_idempotency::ResumeContext,
) -> (MissionTxState, TxOutcome) {
    if contract.lifecycle_state == MissionTxState::RolledBack {
        return (MissionTxState::RolledBack, TxOutcome::Compensated);
    }
    if contract.lifecycle_state == MissionTxState::Compensated {
        return (MissionTxState::Compensated, TxOutcome::Compensated);
    }

    // Failure records are historical facts, not necessarily residual risk. A
    // later durable compensation for every durably successful commit proves
    // that the external effects were fully removed and must win over the older
    // failure when reconstructing terminal state.
    let complete_compensation_coverage = !resume_ctx.compensated_steps.is_empty()
        && resume_ctx
            .completed_steps
            .iter()
            .all(|step_id| resume_ctx.compensated_steps.contains(step_id));
    if complete_compensation_coverage {
        return (MissionTxState::RolledBack, TxOutcome::Compensated);
    }
    if contract.lifecycle_state == MissionTxState::Failed || !resume_ctx.failed_steps.is_empty() {
        return (MissionTxState::Failed, TxOutcome::Failed);
    }
    (MissionTxState::Committed, TxOutcome::Committed)
}

// ── Errors ───────────────────────────────────────────────────────────────────

/// Classification for an explicit rollback proof failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackProofKind {
    /// The receipt identifies a commit candidate, but no durable record exists.
    Missing,
    /// Durable state is ambiguous or contradicts the persisted receipt history.
    Conflict,
}

impl RollbackProofKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Conflict => "conflict",
        }
    }
}

/// Errors from the tx execution engine.
#[derive(Debug, Clone)]
pub enum TxExecutionError {
    /// Contract validation failed.
    InvalidContract(String),
    /// Phase transition failed.
    PhaseTransition(String),
    /// Prepare phase error.
    PreparePhase(String),
    /// Commit phase error.
    CommitPhase(String),
    /// Compensation phase error.
    CompensationPhase(String),
    /// Idempotency ledger write or terminalization failed.
    LedgerWrite(String),
    /// Another process currently owns a durable transaction proof/mutation lock.
    InProgress(String),
    /// Ledger not found for resume.
    LedgerNotFound(String),
    /// Resume would replay already executed work without a checkpoint-aware executor.
    UnsafeResume {
        execution_id: String,
        recommendation: ResumeRecommendation,
    },
    /// A cross-instance dedup record exists but is not safe to replay as a successful input.
    DedupConflict { step_id: String, outcome: String },
    /// A rollback receipt claim lacks proof or conflicts with durable commit state.
    RollbackProof {
        kind: RollbackProofKind,
        step_id: String,
        detail: String,
    },
}

impl std::fmt::Display for TxExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidContract(msg) => write!(f, "Invalid contract: {msg}"),
            Self::PhaseTransition(msg) => write!(f, "Phase transition error: {msg}"),
            Self::PreparePhase(msg) => write!(f, "Prepare phase error: {msg}"),
            Self::CommitPhase(msg) => write!(f, "Commit phase error: {msg}"),
            Self::CompensationPhase(msg) => write!(f, "Compensation phase error: {msg}"),
            Self::LedgerWrite(msg) => write!(f, "Ledger write error: {msg}"),
            Self::InProgress(msg) => write!(f, "Transaction in progress: {msg}"),
            Self::LedgerNotFound(id) => write!(f, "Ledger not found: {id}"),
            Self::UnsafeResume {
                execution_id,
                recommendation,
            } => write!(
                f,
                "Unsafe resume for {execution_id}: recommendation {:?} requires checkpoint-aware replay",
                recommendation
            ),
            Self::DedupConflict { step_id, outcome } => write!(
                f,
                "Dedup conflict for step {step_id}: prior outcome {outcome} cannot be replayed as a successful side-effect input"
            ),
            Self::RollbackProof {
                kind,
                step_id,
                detail,
            } => {
                write!(
                    f,
                    "Rollback proof {} for step {step_id}: {detail}",
                    kind.label()
                )
            }
        }
    }
}

impl std::error::Error for TxExecutionError {}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{
        MissionActorRole, MissionEconomicBreakerDecision, MissionTokenBudget,
        MissionTokenUsageSample, MissionTxContract, MissionTxState, StepAction, TxCompensation,
        TxId, TxIntent, TxOutcome, TxPlan as ContractTxPlan, TxPlanId, TxPrecondition, TxStep,
        TxStepId,
    };
    use crate::tx_idempotency::{IdempotencyKey, IdempotencyPolicy, IdempotencyStore, StepOutcome};
    use crate::tx_plan_compiler::StepRisk;
    use std::cell::RefCell;
    use std::rc::Rc;

    type RecordedStepIds = Rc<RefCell<Vec<String>>>;

    fn stored_mission_fixture(path: &Path) -> Mission {
        let mut mission = Mission::new(
            crate::plan::MissionId("mission:durability-test".to_string()),
            "Owned mission persistence test",
            "owned-test-workspace",
            crate::plan::MissionOwnership {
                planner: "test-planner".to_string(),
                dispatcher: "test-dispatcher".to_string(),
                operator: "test-operator".to_string(),
            },
            1_000,
        );
        mission.lifecycle_state = crate::plan::MissionLifecycleState::Running;
        std::fs::write(path, serde_json::to_vec_pretty(&mission).unwrap()).unwrap();
        mission
    }

    #[test]
    fn mission_store_revision_token_preserves_every_bit_through_json_and_toon() {
        for revision in [
            0,
            1,
            (1_u64 << 53) - 1,
            1_u64 << 53,
            (1_u64 << 53) + 1,
            u64::MAX,
        ] {
            let token = MissionRevisionToken {
                mission_id: "mission:wire-control".to_string(),
                generation: "0123456789abcdef0123456789abcdef".to_string(),
                revision,
                content_sha256: "a".repeat(64),
            };
            let json = serde_json::to_value(&token).unwrap();
            assert_eq!(json["revision"], revision.to_string());
            assert_eq!(
                serde_json::from_value::<MissionRevisionToken>(json.clone()).unwrap(),
                token
            );
            let encoded = toon_rust::encode(json.clone(), None);
            let decoded = toon_rust::try_decode(&encoded, None).unwrap();
            let decoded_json =
                toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
            assert_eq!(
                serde_json::from_str::<MissionRevisionToken>(&decoded_json).unwrap(),
                token
            );
            let mut lossy_numeric = json;
            lossy_numeric["revision"] = serde_json::json!(revision);
            assert!(serde_json::from_value::<MissionRevisionToken>(lossy_numeric).is_err());
        }
        for invalid in [
            "",
            "-1",
            "+1",
            "01",
            "1.0",
            "1e0",
            "18446744073709551616",
            " 1",
        ] {
            let value = serde_json::json!({"mission_id": "mission:wire-control", "generation": "0".repeat(32),
                "revision": invalid, "content_sha256": "a".repeat(64)});
            assert!(
                serde_json::from_value::<MissionRevisionToken>(value).is_err(),
                "accepted revision {invalid:?}"
            );
        }
        println!(
            "MISSION_REVISION_WIRE exact_u64_boundaries=6 invalid_text_controls=8 numeric_tokens_refused=true"
        );
    }

    #[cfg(unix)]
    #[test]
    fn mission_store_revision_incarnation_and_content_conflicts() {
        let dir = tempfile::Builder::new()
            .disable_cleanup(true)
            .tempdir()
            .unwrap();
        let path = dir.path().join("mission.json");
        let baseline = stored_mission_fixture(&path);
        let expected = MissionRevisionToken::from_mission(&baseline).unwrap();
        let guard = MissionMutationGuard::acquire(dir.path(), &path, Some(&expected)).unwrap();
        let mut mission = guard.mission().clone();
        mission
            .pause_mission("operator", "pause", 2_000, None)
            .unwrap();
        let receipt = guard.commit(&mut mission).unwrap();
        assert_eq!(receipt.current.revision, 1);
        assert_eq!(receipt.previous, expected);
        assert!(receipt.changed);
        assert_eq!(
            receipt.owner_acknowledgement,
            "unavailable_no_mission_driver"
        );
        let accepted = std::fs::read(&path).unwrap();
        assert_eq!(
            MissionMutationGuard::acquire(dir.path(), &path, Some(&expected))
                .err()
                .unwrap(),
            MissionStoreError::Conflict
        );
        assert_eq!(std::fs::read(&path).unwrap(), accepted);

        let guard =
            MissionMutationGuard::acquire(dir.path(), &path, Some(&receipt.current)).unwrap();
        let unchanged = guard.commit(&mut mission).unwrap();
        assert!(!unchanged.changed);
        assert_eq!(unchanged.current, receipt.current);
        assert_eq!(unchanged.durability, "unchanged_observation");

        // Same semantic ID and caller timestamp, independently created incarnation.
        let recreated = stored_mission_fixture(&path);
        assert_eq!(recreated.mission_id, baseline.mission_id);
        assert_eq!(recreated.created_at_ms, baseline.created_at_ms);
        assert_ne!(recreated.generation, baseline.generation);
        assert_eq!(
            MissionMutationGuard::acquire(dir.path(), &path, Some(&expected))
                .err()
                .unwrap(),
            MissionStoreError::Conflict
        );

        let guard = MissionMutationGuard::acquire(dir.path(), &path, None).unwrap();
        let mut proposal = guard.mission().clone();
        proposal.title = "our update".to_string();
        let mut external = recreated.clone();
        external.title = "foreign in-place update".to_string();
        let foreign = serde_json::to_vec(&external).unwrap();
        std::fs::write(&path, &foreign).unwrap();
        assert_eq!(
            guard.commit(&mut proposal).unwrap_err(),
            MissionStoreError::Conflict
        );
        assert_eq!(std::fs::read(&path).unwrap(), foreign);
    }

    #[cfg(unix)]
    #[test]
    fn mission_store_commit_before_lock_pins_new_revision_and_refuses_stale_token() {
        let root = tempfile::tempdir().unwrap().keep();
        let path = root.join("mission.json");
        let baseline = stored_mission_fixture(&path);
        let expected = MissionRevisionToken::from_mission(&baseline).unwrap();
        let guard = acquire_tx_contract_lock_supported(&root, &path, || {
            let winner = MissionMutationGuard::acquire(&root, &path, Some(&expected)).unwrap();
            let mut mission = winner.mission().clone();
            mission
                .pause_mission("operator", "competing pause", 2_000, None)
                .unwrap();
            winner.commit(&mut mission).unwrap();
        })
        .unwrap();
        let accepted = std::fs::read(&path).unwrap();
        assert_eq!(guard.read_authoritative_contract_bytes().unwrap(), accepted);
        assert_eq!(
            MissionMutationGuard::from_guard(guard, Some(&expected))
                .err()
                .unwrap(),
            MissionStoreError::Conflict
        );
        assert_eq!(std::fs::read(&path).unwrap(), accepted);
        let current = MissionMutationGuard::acquire(&root, &path, None).unwrap();
        assert_eq!(current.mission().revision, 1);
        assert_eq!(
            current.mission().lifecycle_state,
            crate::plan::MissionLifecycleState::Paused
        );
        println!(
            "MISSION_LOCK_ORDER competing_commit_before_lock=true latest_inode_pinned=true stale_token_conflict=true accepted_bytes_preserved=true"
        );
    }

    #[cfg(unix)]
    #[test]
    fn mission_store_late_leaf_attack_refuses_and_releases_lock_registry() {
        for symlink in [false, true] {
            let root = tempfile::tempdir().unwrap().keep();
            let path = root.join("mission.json");
            stored_mission_fixture(&path);
            let original = std::fs::read(&path).unwrap();
            let retained = root.join("retained.json");
            let error = acquire_tx_contract_lock_supported(&root, &path, || {
                if symlink {
                    std::fs::rename(&path, &retained).unwrap();
                    std::os::unix::fs::symlink(&retained, &path).unwrap();
                } else {
                    std::fs::hard_link(&path, &retained).unwrap();
                }
            })
            .err()
            .unwrap();
            assert_eq!(error.kind(), TxContractStoreErrorKind::Lock);
            assert_eq!(std::fs::read(&retained).unwrap(), original);
            let again = acquire_tx_contract_lock(&root, &path).err().unwrap();
            assert_eq!(again.kind(), TxContractStoreErrorKind::Lock);
            assert!(
                !TX_CONTRACT_LOCKS
                    .lock()
                    .unwrap()
                    .contains(&root.canonicalize().unwrap().join("mission.json"))
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mission_store_deleted_inode_canonical_spelling_is_a_pre_effect_conflict() {
        use std::os::fd::AsRawFd;
        let root = tempfile::tempdir().unwrap().keep();
        let path = root.join("mission.json");
        stored_mission_fixture(&path);
        let old_inode = File::open(&path).unwrap();
        let winner = MissionMutationGuard::acquire(&root, &path, None).unwrap();
        let mut mission = winner.mission().clone();
        mission
            .pause_mission("operator", "replace old inode", 2_000, None)
            .unwrap();
        winner.commit(&mut mission).unwrap();
        let accepted = std::fs::read(&path).unwrap();
        let stale_name =
            std::fs::read_link(format!("/proc/self/fd/{}", old_inode.as_raw_fd())).unwrap();
        let parent = CapDir::open_ambient_dir(&root, cap_std::ambient_authority()).unwrap();
        let error = verify_tx_contract_canonical_leaf(
            &parent,
            std::ffi::OsStr::new("mission.json"),
            stale_name.file_name().unwrap(),
            &path,
        )
        .unwrap_err();
        assert_eq!(error.kind(), TxContractStoreErrorKind::Conflict);
        assert_eq!(MissionStoreError::from(error), MissionStoreError::Conflict);
        assert_eq!(std::fs::read(&path).unwrap(), accepted);
        verify_tx_contract_canonical_leaf(
            &parent,
            std::ffi::OsStr::new("mission.json"),
            std::ffi::OsStr::new("mission.json"),
            &path,
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn mission_store_faults_preserve_old_or_new_complete_snapshot() {
        for point in [
            TxContractSaveFaultPoint::BeforeWrite,
            TxContractSaveFaultPoint::BeforeFileSync,
            TxContractSaveFaultPoint::BeforeAtomicReplace,
            TxContractSaveFaultPoint::ParentDirectorySync,
        ] {
            let dir = tempfile::Builder::new()
                .disable_cleanup(true)
                .tempdir()
                .unwrap();
            let path = dir.path().join("mission.json");
            stored_mission_fixture(&path);
            let original = std::fs::read(&path).unwrap();
            let guard = MissionMutationGuard::acquire(dir.path(), &path, None).unwrap();
            let mut mission = guard.mission().clone();
            mission
                .abort_mission("operator", "abort", None, 2_000, None)
                .unwrap();
            let result = guard.commit_impl(&mut mission, |at| {
                if at == point {
                    Err(std::io::Error::other("owned deterministic fault"))
                } else {
                    Ok(())
                }
            });
            let error = result.unwrap_err();
            let actual = std::fs::read(&path).unwrap();
            let loaded: Mission = serde_json::from_slice(&actual).unwrap();
            loaded.validate().unwrap();
            if point == TxContractSaveFaultPoint::ParentDirectorySync {
                assert_eq!(error, MissionStoreError::Indeterminate);
                assert_eq!(loaded.revision, 1);
                assert_eq!(
                    loaded.lifecycle_state,
                    crate::plan::MissionLifecycleState::Cancelled
                );
                assert_ne!(actual, original);
            } else {
                let expected = match point {
                    TxContractSaveFaultPoint::BeforeWrite => MissionStoreError::Write,
                    TxContractSaveFaultPoint::BeforeFileSync => MissionStoreError::Sync,
                    TxContractSaveFaultPoint::BeforeAtomicReplace => MissionStoreError::Rename,
                    TxContractSaveFaultPoint::ParentDirectorySync => unreachable!(),
                };
                assert_eq!(error, expected);
                assert_eq!(actual, original);
            }
            assert_eq!(
                mission.revision, 0,
                "failed commit must not claim a durable new revision"
            );
            assert!(!error.to_string().contains("owned deterministic fault"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn mission_store_rejects_replaced_path_and_revision_overflow() {
        let dir = tempfile::Builder::new()
            .disable_cleanup(true)
            .tempdir()
            .unwrap();
        let path = dir.path().join("mission.json");
        let mut mission = stored_mission_fixture(&path);
        mission.revision = u64::MAX;
        std::fs::write(&path, serde_json::to_vec(&mission).unwrap()).unwrap();
        let before = std::fs::read(&path).unwrap();
        let guard = MissionMutationGuard::acquire(dir.path(), &path, None).unwrap();
        mission.title = "overflow proposal".to_string();
        assert_eq!(
            guard.commit(&mut mission).unwrap_err(),
            MissionStoreError::Invalid
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let guard = MissionMutationGuard::acquire(dir.path(), &path, None).unwrap();
        let mut proposal = guard.mission().clone();
        proposal.title = "replaced path proposal".to_string();
        let foreign_path = dir.path().join("foreign.json");
        stored_mission_fixture(&foreign_path);
        let foreign = std::fs::read(&foreign_path).unwrap();
        std::fs::rename(&path, dir.path().join("original-retained.json")).unwrap();
        std::fs::rename(&foreign_path, &path).unwrap();
        assert!(guard.commit(&mut proposal).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), foreign);
        assert_eq!(
            std::fs::read(dir.path().join("original-retained.json")).unwrap(),
            before
        );
    }

    #[cfg(unix)]
    static OWNED_MISSION_CHILD_COUNT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    #[cfg(unix)]
    static RETAINED_MISSION_CHILDREN: Mutex<Vec<std::process::Child>> = Mutex::new(Vec::new());

    #[cfg(unix)]
    struct OwnedMissionChild {
        child: Option<std::process::Child>,
        stdout: PathBuf,
        stderr: PathBuf,
    }

    #[cfg(unix)]
    impl OwnedMissionChild {
        fn spawn(root: &Path, name: &str, action: &str, expected: &MissionRevisionToken) -> Self {
            let stdout = root.join(format!("{name}.stdout"));
            let stderr = root.join(format!("{name}.stderr"));
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "tx_execution::tests::mission_store_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("FT_MISSION_TEST_ROOT", root)
                .env("FT_MISSION_TEST_NAME", name)
                .env("FT_MISSION_TEST_ACTION", action)
                .env(
                    "FT_MISSION_TEST_TOKEN",
                    serde_json::to_string(expected).unwrap(),
                )
                .stdin(std::process::Stdio::null())
                .stdout(File::create_new(&stdout).unwrap())
                .stderr(File::create_new(&stderr).unwrap());
            OWNED_MISSION_CHILD_COUNT
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                    (count < 16).then_some(count + 1)
                })
                .expect("finite capacity for live and indeterminate mission children");
            let child = command.spawn().unwrap_or_else(|error| {
                OWNED_MISSION_CHILD_COUNT.fetch_sub(1, Ordering::SeqCst);
                panic!("spawn owned mission child: {error}");
            });
            Self {
                child: Some(child),
                stdout,
                stderr,
            }
        }

        fn finish_reaped(&mut self) {
            if self.child.take().is_some() {
                OWNED_MISSION_CHILD_COUNT.fetch_sub(1, Ordering::SeqCst);
            }
        }

        fn stop_bounded(&mut self) -> Option<std::process::ExitStatus> {
            let child = self.child.as_mut()?;
            if let Ok(Some(status)) = child.try_wait() {
                self.finish_reaped();
                return Some(status);
            }
            let killed = child.kill();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Ok(Some(status)) = child.try_wait() {
                    self.finish_reaped();
                    return Some(status);
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let child = self.child.take().expect("unsettled owned child");
            eprintln!(
                "owned mission child retained: pid={}, kill={killed:?}, settlement=indeterminate",
                child.id()
            );
            // Retain both the handle and its capacity reservation. Never turn
            // an unconfirmed kill into a reaping claim or an unbounded wait.
            RETAINED_MISSION_CHILDREN
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(child);
            None
        }

        fn wait(&mut self) -> std::process::ExitStatus {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let oversized = [&self.stdout, &self.stderr].iter().any(|path| {
                    std::fs::metadata(path).map_or(true, |metadata| metadata.len() > 256 * 1024)
                });
                let poll = self
                    .child
                    .as_mut()
                    .expect("owned child not yet consumed")
                    .try_wait();
                if !oversized {
                    if let Ok(Some(status)) = &poll {
                        self.finish_reaped();
                        return *status;
                    }
                }
                if oversized || poll.is_err() || std::time::Instant::now() >= deadline {
                    let reaped = self.stop_bounded();
                    let mut stderr = String::new();
                    let diagnostic = File::open(&self.stderr)
                        .and_then(|file| file.take(64 * 1024).read_to_string(&mut stderr));
                    panic!(
                        "owned mission child failed: poll={poll:?}, oversized={oversized}, bounded_reap={reaped:?}, stderr_read={diagnostic:?}, stderr={stderr}"
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }

    #[cfg(unix)]
    impl Drop for OwnedMissionChild {
        fn drop(&mut self) {
            if self.child.is_some() {
                let reaped = self.stop_bounded();
                eprintln!("owned mission child cleanup: bounded_reap={reaped:?}");
            }
        }
    }

    /// Executed only as an owned child by the tests below. Crash injection tests
    /// process interruption at filesystem cut points, not actual power loss.
    #[cfg(unix)]
    #[test]
    #[ignore = "owned subprocess entrypoint"]
    fn mission_store_child() {
        let root = std::env::var_os("FT_MISSION_TEST_ROOT")
            .expect("owned subprocess requires its isolated workspace");
        let root = PathBuf::from(root);
        let name = std::env::var("FT_MISSION_TEST_NAME").unwrap();
        let action = std::env::var("FT_MISSION_TEST_ACTION").unwrap();
        let expected: MissionRevisionToken =
            serde_json::from_str(&std::env::var("FT_MISSION_TEST_TOKEN").unwrap()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !root.join("release").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "parent did not release owned child"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let guard = loop {
            match MissionMutationGuard::acquire(&root, &root.join("mission.json"), Some(&expected))
            {
                Err(MissionStoreError::InProgress) if std::time::Instant::now() < deadline => {
                    let marker = root.join(format!("{name}.contended"));
                    if !marker.exists() {
                        std::fs::write(marker, b"observed cross-process lock contention").unwrap();
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                result => break result,
            }
        };
        let result = guard.and_then(|guard| {
            let mut mission = guard.mission().clone();
            if action == "resume" {
                mission
                    .resume_mission("owned-child", "resume", 3_000, None)
                    .unwrap();
            } else {
                mission
                    .abort_mission("owned-child", "abort", None, 3_000, None)
                    .unwrap();
            }
            guard.commit_impl(&mut mission, |point| {
                let crash = match action.as_str() {
                    "crash_before_write" => point == TxContractSaveFaultPoint::BeforeWrite,
                    "crash_before_file_sync" => point == TxContractSaveFaultPoint::BeforeFileSync,
                    "crash_before_replace" => {
                        point == TxContractSaveFaultPoint::BeforeAtomicReplace
                    }
                    "crash_before_directory_sync" => {
                        point == TxContractSaveFaultPoint::ParentDirectorySync
                    }
                    _ => false,
                };
                if crash {
                    std::process::exit(72);
                }
                Ok(())
            })
        });
        let report = match result {
            Ok(receipt) => serde_json::json!({"ok": true, "receipt": receipt}),
            Err(error) => serde_json::json!({"ok": false, "error": error.code()}),
        };
        std::fs::write(
            root.join(format!("{name}.result.json")),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn mission_store_owned_child_termination_is_bounded_and_reaped() {
        let root = tempfile::tempdir().unwrap().keep();
        let path = root.join("mission.json");
        let mission = stored_mission_fixture(&path);
        let token = MissionRevisionToken::from_mission(&mission).unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut child = OwnedMissionChild::spawn(&root, "stop-before-release", "abort", &token);
        let pid = child.child.as_ref().unwrap().id();
        assert!(child.child.as_mut().unwrap().try_wait().unwrap().is_none());
        let started = std::time::Instant::now();
        let status = child.stop_bounded().expect("owned waiting child reaped");
        assert!(!status.success());
        assert!(started.elapsed() < std::time::Duration::from_secs(6));
        assert!(child.child.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!root.join("stop-before-release.result.json").exists());
        println!(
            "MISSION_CHILD_SETTLEMENT pid={pid} owned_leader_reaped=true before_release=true mission_bytes_preserved=true"
        );
    }

    #[cfg(unix)]
    #[test]
    fn mission_store_process_lock_spans_read_through_commit() {
        let dir = tempfile::Builder::new()
            .disable_cleanup(true)
            .tempdir()
            .unwrap();
        let path = dir.path().join("mission.json");
        let mission = stored_mission_fixture(&path);
        let token = MissionRevisionToken::from_mission(&mission).unwrap();
        let original = std::fs::read(&path).unwrap();
        let guard = MissionMutationGuard::acquire(dir.path(), &path, Some(&token)).unwrap();
        let mut child = OwnedMissionChild::spawn(dir.path(), "blocked-abort", "abort", &token);
        std::fs::write(dir.path().join("release"), b"release").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !dir.path().join("blocked-abort.contended").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "child did not observe held file lock"
            );
            assert!(child.child.as_mut().unwrap().try_wait().unwrap().is_none());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(guard.mission().revision, 0);
        drop(guard);
        assert!(
            child.wait().success(),
            "child stderr: {}",
            std::fs::read_to_string(&child.stderr).unwrap()
        );
        let accepted: Mission = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(accepted.revision, 1);
        assert_eq!(
            accepted.lifecycle_state,
            crate::plan::MissionLifecycleState::Cancelled
        );
    }

    #[cfg(unix)]
    #[test]
    fn mission_store_two_process_cas_race_preserves_accepted_abort() {
        let dir = tempfile::Builder::new()
            .disable_cleanup(true)
            .tempdir()
            .unwrap();
        let path = dir.path().join("mission.json");
        let mut mission = stored_mission_fixture(&path);
        let guard = MissionMutationGuard::acquire(dir.path(), &path, None).unwrap();
        mission
            .pause_mission("operator", "pause", 2_000, None)
            .unwrap();
        let token = guard.commit(&mut mission).unwrap().current;
        let mut resume = OwnedMissionChild::spawn(dir.path(), "resume", "resume", &token);
        let mut abort = OwnedMissionChild::spawn(dir.path(), "abort", "abort", &token);
        std::fs::write(dir.path().join("release"), b"owned children may proceed").unwrap();
        assert!(
            resume.wait().success(),
            "resume child stderr: {}",
            std::fs::read_to_string(&resume.stderr).unwrap()
        );
        assert!(
            abort.wait().success(),
            "abort child stderr: {}",
            std::fs::read_to_string(&abort.stderr).unwrap()
        );
        let resume: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("resume.result.json")).unwrap())
                .unwrap();
        let abort: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("abort.result.json")).unwrap())
                .unwrap();
        assert_ne!(
            resume["ok"], abort["ok"],
            "exactly one mutation may accept a shared revision"
        );
        let rejected = if resume["ok"] == true {
            &abort
        } else {
            &resume
        };
        assert_eq!(rejected["error"], "mission.revision_conflict");
        let mut current: Mission = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        current.validate().unwrap();
        assert_eq!(current.revision, token.revision + 1);
        if abort["ok"] == false {
            assert_eq!(
                current.lifecycle_state,
                crate::plan::MissionLifecycleState::Running
            );
            let fresh = MissionRevisionToken::from_mission(&current).unwrap();
            let mut abort = OwnedMissionChild::spawn(dir.path(), "fresh-abort", "abort", &fresh);
            assert!(
                abort.wait().success(),
                "fresh abort stderr: {}",
                std::fs::read_to_string(&abort.stderr).unwrap()
            );
            let receipt: serde_json::Value = serde_json::from_slice(
                &std::fs::read(dir.path().join("fresh-abort.result.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(receipt["ok"], true);
            current = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        }
        assert_eq!(
            current.lifecycle_state,
            crate::plan::MissionLifecycleState::Cancelled
        );
        assert_eq!(current.pause_resume_state.total_abort_count, 1);
        let accepted = std::fs::read(&path).unwrap();
        assert_eq!(
            MissionMutationGuard::acquire(dir.path(), &path, Some(&token))
                .err()
                .unwrap(),
            MissionStoreError::Conflict
        );
        assert_eq!(std::fs::read(&path).unwrap(), accepted);
    }

    #[cfg(unix)]
    #[test]
    fn mission_store_process_crash_cutpoints_leave_complete_old_or_new() {
        for action in [
            "crash_before_write",
            "crash_before_file_sync",
            "crash_before_replace",
            "crash_before_directory_sync",
        ] {
            let dir = tempfile::Builder::new()
                .disable_cleanup(true)
                .tempdir()
                .unwrap();
            let path = dir.path().join("mission.json");
            let mission = stored_mission_fixture(&path);
            let original = std::fs::read(&path).unwrap();
            let token = MissionRevisionToken::from_mission(&mission).unwrap();
            let mut child = OwnedMissionChild::spawn(dir.path(), "crash", action, &token);
            std::fs::write(dir.path().join("release"), b"release").unwrap();
            assert_eq!(
                child.wait().code(),
                Some(72),
                "crash child stderr: {}",
                std::fs::read_to_string(&child.stderr).unwrap()
            );
            assert!(
                !dir.path().join("crash.result.json").exists(),
                "interrupted mutation must not acknowledge success"
            );
            let actual = std::fs::read(&path).unwrap();
            let loaded: Mission = serde_json::from_slice(&actual).unwrap();
            loaded.validate().unwrap();
            if action == "crash_before_directory_sync" {
                assert_eq!(loaded.revision, 1);
                assert_eq!(
                    loaded.lifecycle_state,
                    crate::plan::MissionLifecycleState::Cancelled
                );
            } else {
                assert_eq!(actual, original);
            }
            assert!(
                MissionMutationGuard::acquire(dir.path(), &path, None).is_ok(),
                "crashed process must release its OS lock"
            );
        }
    }

    // ── ft-3lqyu / ft-0rlfq.8: effect seal ──────────────────────────────
    //
    // The storeless `execute`/`rollback` entrypoints run with no durable
    // idempotency spool: no write-ahead `Pending` record, no per-key or
    // execution lock, no crash reconciliation. The seal below is what keeps
    // an effectful executor off those paths. The mechanical negative proof
    // is the `compile_fail` doctest on `assert_non_effectful_executor`; the
    // tests here pin the positive side and the runtime fail-closed half.

    #[test]
    fn synthetic_executor_satisfies_the_effect_seal() {
        assert_non_effectful_executor(&SyntheticStepExecutor);
    }

    #[test]
    fn execute_with_store_rejects_a_non_durable_spool() {
        let mut contract = make_test_contract(1);
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        assert!(!store.is_durable());

        let err = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default())
            .execute_with_store(&mut contract, &mut store, 5_000)
            .expect_err("in-memory spool must not authorize commit dispatch");

        assert!(
            matches!(&err, TxExecutionError::InvalidContract(msg) if msg.contains("durable")),
            "unexpected error: {err:?}"
        );
        assert_eq!(
            contract.lifecycle_state,
            MissionTxState::Planned,
            "a rejected execution must not advance contract lifecycle"
        );
        assert!(
            contract.receipts.is_empty(),
            "a rejected execution must not mint receipts"
        );
    }

    #[test]
    fn rollback_with_store_rejects_a_non_durable_spool() {
        let mut contract = make_test_contract(1);
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());

        let err = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default())
            .rollback_with_store(&mut contract, &mut store, 5_000)
            .expect_err("in-memory spool must not authorize compensation dispatch");

        assert!(
            matches!(&err, TxExecutionError::InvalidContract(msg) if msg.contains("durable")),
            "unexpected error: {err:?}"
        );
        assert!(
            contract.receipts.is_empty(),
            "a rejected rollback must not mint compensation receipts"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn directory_sync_handle_is_synchronizable_for_contract_store() {
        let workspace = tempfile::tempdir().expect("create contract sync workspace");
        let pinned = CapDir::open_ambient_dir(workspace.path(), cap_std::ambient_authority())
            .expect("pin contract sync workspace");
        let sync_file = open_directory_sync_file(&pinned, workspace.path())
            .expect("open synchronizable contract directory handle");

        sync_file
            .sync_all()
            .expect("synchronize contract directory handle");
    }

    fn make_test_contract(num_steps: usize) -> MissionTxContract {
        let steps: Vec<TxStep> = (0..num_steps)
            .map(|i| TxStep {
                step_id: TxStepId(format!("step-{i}")),
                ordinal: i,
                action: StepAction::SendText {
                    pane_id: i as u64,
                    text: format!("action-{i}"),
                    paste_mode: None,
                },
                description: format!("Test step {i}"),
            })
            .collect();
        let compensations = steps
            .iter()
            .map(|step| TxCompensation {
                for_step_id: step.step_id.clone(),
                action: StepAction::SendText {
                    pane_id: step.ordinal as u64,
                    text: format!("undo-{}", step.ordinal),
                    paste_mode: None,
                },
            })
            .collect();

        MissionTxContract {
            tx_version: 1,
            intent: TxIntent {
                tx_id: TxId("tx-test-1".to_string()),
                requested_by: MissionActorRole::Operator,
                summary: "Test transaction".to_string(),
                correlation_id: "corr-1".to_string(),
                created_at_ms: 1000,
            },
            plan: ContractTxPlan {
                plan_id: TxPlanId("plan-1".to_string()),
                tx_id: TxId("tx-test-1".to_string()),
                steps,
                preconditions: Vec::new(),
                compensations,
            },
            lifecycle_state: MissionTxState::Planned,
            outcome: TxOutcome::Pending,
            receipts: Vec::new(),
        }
    }

    fn write_authoritative_contract(path: &Path, contract: &MissionTxContract) {
        std::fs::write(path, serde_json::to_vec_pretty(contract).unwrap()).unwrap();
    }

    /// A tempdir whose path is already canonical.
    ///
    /// `acquire_tx_contract_lock` binds the guard to the *canonicalized*
    /// contract path, and `save_tx_contract_atomic` refuses every other
    /// spelling of it (`verify_logical_path`) — that refusal is the point of
    /// the guard. Production callers therefore always pass
    /// `guard.authoritative_path()`.
    ///
    /// A contract-store test that builds its own path from
    /// `tempfile::tempdir()` only agrees with the guard when TMPDIR contains
    /// no symlink. On the remote RCH workers it does: the repo is mounted at a
    /// different root, so `tempdir()` hands back an alias and every test in
    /// this family failed with a `Lock` error unrelated to the behavior under
    /// test — making the whole contract-store suite unprovable remotely
    /// (ft-ehcqr). Canonicalizing the root once keeps these tests testing the
    /// store rather than the host's path layout.
    fn canonical_tempdir() -> (tempfile::TempDir, std::path::PathBuf) {
        let mut dir = tempfile::tempdir().expect("contract store tempdir");
        dir.disable_cleanup(true);
        let root = dir
            .path()
            .canonicalize()
            .expect("canonicalize contract store tempdir root");
        (dir, root)
    }

    fn durable_store() -> (tempfile::TempDir, IdempotencyStore) {
        let dir = tempfile::tempdir().expect("durable tx store tempdir");
        let store = IdempotencyStore::open(dir.path(), IdempotencyPolicy::default())
            .expect("open durable tx store");
        (dir, store)
    }

    /// Run a full lifecycle against a throwaway durable spool.
    ///
    /// The `integration_*` fixtures below drive a real [`PaneStepExecutor`],
    /// which dispatches against (mock-backed) panes. The storeless
    /// [`TxExecutionEngine::execute`] is sealed to non-effectful executors
    /// (ft-3lqyu / ft-0rlfq.8), so those fixtures go through the same durable
    /// entrypoint production uses. Each call gets a fresh spool, so no prior
    /// outcome is ever observed and every step dispatches exactly once.
    fn execute_durable<E: StepExecutor>(
        engine: &TxExecutionEngine<E>,
        contract: &mut MissionTxContract,
        now_ms: i64,
    ) -> Result<TxExecutionResult, TxExecutionError> {
        let (_spool, mut store) = durable_store();
        engine.execute_with_store(contract, &mut store, now_ms)
    }

    fn durable_ledger_file_snapshot(
        store_dir: &Path,
    ) -> std::collections::BTreeMap<String, Vec<u8>> {
        std::fs::read_dir(store_dir.join("tx_ledgers"))
            .expect("read durable ledger spool")
            .map(|entry| entry.expect("read durable ledger spool entry"))
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
            .map(|entry| {
                let name = entry
                    .file_name()
                    .into_string()
                    .expect("durable ledger file name is UTF-8");
                let bytes = std::fs::read(entry.path()).expect("read durable ledger file");
                (name, bytes)
            })
            .collect()
    }

    fn record_durable_test_outcome(
        store: &mut IdempotencyStore,
        execution_id: &str,
        idem_key: IdempotencyKey,
        outcome: StepOutcome,
        risk: StepRisk,
        agent_id: &str,
        timestamp_ms: u64,
    ) -> Result<(), String> {
        let mut reservation = store
            .acquire_durable_reservation(execution_id, &idem_key, timestamp_ms)
            .map_err(|err| err.to_string())?;
        if let Some(observed) = reservation.observed_outcome() {
            return Err(format!(
                "test reservation unexpectedly observed prior outcome {observed:?}"
            ));
        }
        store
            .record_execution_reserved(
                &mut reservation,
                execution_id,
                idem_key.clone(),
                StepOutcome::Pending,
                risk,
                agent_id,
                timestamp_ms,
            )
            .map_err(|err| err.to_string())?;
        store
            .complete_execution_reserved(reservation, execution_id, idem_key, outcome, timestamp_ms)
            .map_err(|err| err.to_string())?;
        Ok(())
    }

    fn test_commit_key(contract: &MissionTxContract, step_id: &str) -> IdempotencyKey {
        commit_idempotency_key(contract, step_id).unwrap()
    }

    fn test_compensation_key(contract: &MissionTxContract, step_id: &str) -> IdempotencyKey {
        compensation_idempotency_key(contract, step_id).unwrap()
    }

    #[test]
    fn execution_ids_are_unique_and_timestamp_sortable() {
        let first = unique_execution_id("run", 7_000);
        let second = unique_execution_id("run", 7_000);
        let later = unique_execution_id("rollback", 7_001);

        assert_ne!(first, second);
        assert!(first.starts_with("txe-00000000000000007000-"));
        assert!(first.ends_with("-run"));
        assert!(later.starts_with("txe-00000000000000007001-"));
        assert!(later.ends_with("-rollback"));
        assert!(first < later);
        assert!(second < later);
    }

    #[test]
    fn atomic_save_requires_the_matching_contract_lock() {
        let (dir, _root) = canonical_tempdir();
        let first_path = dir.path().join("first.json");
        let second_path = dir.path().join("second.json");
        let contract = make_test_contract(1);
        write_authoritative_contract(&first_path, &contract);
        write_authoritative_contract(&second_path, &contract);
        let second_before = std::fs::read(&second_path).unwrap();
        let first_guard = acquire_tx_contract_lock(dir.path(), &first_path).unwrap();

        let preflight_err = first_guard.authorizes(&second_path).unwrap_err();
        assert_eq!(preflight_err.kind(), TxContractStoreErrorKind::Lock);
        let err = save_tx_contract_atomic(&first_guard, &second_path, &contract).unwrap_err();

        assert_eq!(err.kind(), TxContractStoreErrorKind::Lock);
        assert_eq!(std::fs::read(&second_path).unwrap(), second_before);
    }

    #[cfg(unix)]
    #[test]
    fn contract_lock_and_save_reject_symbolic_links() {
        use std::os::unix::fs::symlink;

        let (dir, _root) = canonical_tempdir();
        let target = dir.path().join("target.json");
        let link = dir.path().join("link.json");
        let contract = make_test_contract(1);
        write_authoritative_contract(&target, &contract);
        symlink(&target, &link).unwrap();

        let lock_err = acquire_tx_contract_lock(dir.path(), &link).err().unwrap();
        assert_eq!(lock_err.kind(), TxContractStoreErrorKind::Lock);

        let target_guard = acquire_tx_contract_lock(dir.path(), &target).unwrap();
        let save_err = save_tx_contract_atomic(&target_guard, &link, &contract).unwrap_err();
        assert_eq!(save_err.kind(), TxContractStoreErrorKind::Lock);
    }

    #[cfg(unix)]
    #[test]
    fn contract_lock_rejects_contracts_with_multiple_hard_links() {
        let (dir, _root) = canonical_tempdir();
        let path = dir.path().join("tx.json");
        let second_link = dir.path().join("tx-second-link.json");
        let contract = make_test_contract(1);
        write_authoritative_contract(&path, &contract);
        std::fs::hard_link(&path, &second_link).unwrap();

        let err = acquire_tx_contract_lock(dir.path(), &path).err().unwrap();

        assert_eq!(err.kind(), TxContractStoreErrorKind::Lock);
        assert!(err.to_string().contains("has 2 hard links"));
    }

    #[cfg(unix)]
    #[test]
    fn contract_lock_rejects_fifo_leaf_without_waiting_for_a_writer() {
        let (dir, _root) = canonical_tempdir();
        let path = dir.path().join("tx.json");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("invoke mkfifo for hostile contract leaf");
        assert!(status.success());

        let err = acquire_tx_contract_lock(dir.path(), &path).err().unwrap();

        assert_eq!(err.kind(), TxContractStoreErrorKind::Lock);
        assert!(err.to_string().contains("is not a regular file"));
    }

    #[cfg(unix)]
    #[test]
    fn contract_lock_rejects_symbolic_link_sidecar() {
        use std::os::unix::fs::symlink;

        let (dir, _root) = canonical_tempdir();
        let path = dir.path().join("tx.json");
        let lock_target = dir.path().join("unrelated.lock");
        let contract = make_test_contract(1);
        write_authoritative_contract(&path, &contract);
        std::fs::write(&lock_target, b"unrelated").unwrap();
        let lock_dir = dir.path().join(".ft").join("tx_contract_locks");
        std::fs::create_dir_all(&lock_dir).unwrap();
        let lock_path = lock_dir.join(workspace_root_lock_name(Path::new("tx.json")));
        symlink(&lock_target, lock_path).unwrap();

        let err = acquire_tx_contract_lock(dir.path(), &path).err().unwrap();

        assert_eq!(err.kind(), TxContractStoreErrorKind::Lock);
        assert_eq!(std::fs::read(&lock_target).unwrap(), b"unrelated");
    }

    #[cfg(unix)]
    #[test]
    fn contract_lock_rejects_parent_namespace_detach_until_original_is_restored() {
        let (dir, _root) = canonical_tempdir();
        let active_dir = dir.path().join("active");
        let foreign_dir = dir.path().join("foreign");
        let detached_dir = dir.path().join("active-detached");
        let displaced_foreign_dir = dir.path().join("foreign-displaced");
        std::fs::create_dir(&active_dir).unwrap();
        std::fs::create_dir(&foreign_dir).unwrap();
        let active_path = active_dir.join("tx.json");
        let foreign_path = foreign_dir.join("tx.json");
        let contract = make_test_contract(1);
        write_authoritative_contract(&active_path, &contract);
        write_authoritative_contract(&foreign_path, &contract);

        let guard = acquire_tx_contract_lock(dir.path(), &active_path).unwrap();
        let authoritative_path = active_path.canonicalize().unwrap();

        assert_eq!(guard.authoritative_path(), authoritative_path.as_path());
        std::fs::rename(&active_dir, &detached_dir).unwrap();
        std::fs::rename(&foreign_dir, &active_dir).unwrap();

        assert_eq!(guard.authoritative_path(), authoritative_path.as_path());
        assert!(guard.authorizes(guard.authoritative_path()).is_err());

        std::fs::rename(&active_dir, &displaced_foreign_dir).unwrap();
        std::fs::rename(&detached_dir, &active_dir).unwrap();
        guard.authorizes(guard.authoritative_path()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn contract_lock_rejects_symlinked_ancestor_directory() {
        use std::os::unix::fs::symlink;

        let (dir, _root) = canonical_tempdir();
        let (external, _external_root) = canonical_tempdir();
        let sub_dir = dir.path().join("sub");
        let external_sub = external.path().join("external_sub");
        std::fs::create_dir(&external_sub).unwrap();
        let target_path = external_sub.join("tx.json");
        let contract = make_test_contract(1);
        write_authoritative_contract(&target_path, &contract);
        symlink(&external_sub, &sub_dir).unwrap();

        let contract_path = sub_dir.join("tx.json");
        let err = acquire_tx_contract_lock(dir.path(), &contract_path)
            .err()
            .unwrap();
        assert_eq!(err.kind(), TxContractStoreErrorKind::Lock);
        assert!(err.to_string().contains("without following symlinks"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn contract_lock_native_case_aliases_share_ownership() {
        let (_dir, root) = canonical_tempdir();
        let path = root.join("Active.json");
        let alias = root.join("ACTIVE.JSON");
        let contract = make_test_contract(1);
        write_authoritative_contract(&path, &contract);
        let guard = acquire_tx_contract_lock(&root, &path).unwrap();
        if alias.exists() {
            assert_eq!(
                std_object_identity(&std::fs::metadata(&path).unwrap()).unwrap(),
                std_object_identity(&std::fs::metadata(&alias).unwrap()).unwrap()
            );
            let error = acquire_tx_contract_lock(&root, &alias).err().unwrap();
            assert_eq!(error.kind(), TxContractStoreErrorKind::InProgress);
        } else {
            // Case-sensitive volumes must retain two independent identities.
            write_authoritative_contract(&alias, &contract);
            let independent = acquire_tx_contract_lock(&root, &alias).unwrap();
            assert_ne!(independent.authoritative_path(), guard.authoritative_path());
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            serde_json::to_vec_pretty(&contract).unwrap()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn contract_lock_native_case_rename_refuses_stale_spelling() {
        for rename_parent in [false, true] {
            let (_dir, root) = canonical_tempdir();
            let parent = root.join("Active");
            std::fs::create_dir(&parent).unwrap();
            let path = parent.join("Mission.json");
            let renamed_parent = root.join("ACTIVE");
            let renamed = if rename_parent {
                renamed_parent.join("Mission.json")
            } else {
                parent.join("MISSION.JSON")
            };
            let contract = make_test_contract(1);
            write_authoritative_contract(&path, &contract);
            let before = std::fs::read(&path).unwrap();
            let mut new_guard = None;
            let result = acquire_tx_contract_lock_supported(&root, &path, || {
                if rename_parent {
                    std::fs::rename(&parent, &renamed_parent).unwrap();
                } else {
                    std::fs::rename(&path, &renamed).unwrap();
                }
                new_guard = Some(acquire_tx_contract_lock(&root, &renamed).unwrap());
            });
            let error = result
                .err()
                .expect("stale spelling cannot acquire authority");
            if path.exists() || rename_parent {
                assert_eq!(error.kind(), TxContractStoreErrorKind::Conflict);
            } else {
                // On a case-sensitive volume the old name is simply absent.
                assert_eq!(error.kind(), TxContractStoreErrorKind::Lock);
            }
            let current = new_guard.unwrap();
            current.authorizes(current.authoritative_path()).unwrap();
            assert_eq!(std::fs::read(&renamed).unwrap(), before);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn contract_lock_native_discovery_rejects_replaced_fifo_without_blocking() {
        let (_dir, root) = canonical_tempdir();
        let path = root.join("mission.json");
        let retained = root.join("retained.json");
        let contract = make_test_contract(1);
        write_authoritative_contract(&path, &contract);
        let parent = CapDir::open_ambient_dir(&root, cap_std::ambient_authority()).unwrap();
        assert!(parent.symlink_metadata("mission.json").unwrap().is_file());
        std::fs::rename(&path, &retained).unwrap();
        let status = std::process::Command::new("/usr/bin/mkfifo")
            .arg(&path)
            .status()
            .expect("create owned FIFO discovery control");
        assert!(status.success());
        let started = std::time::Instant::now();
        let error =
            discover_native_tx_contract_path(&parent, std::ffi::OsStr::new("mission.json"), &path)
                .unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(error.kind(), TxContractStoreErrorKind::Lock);
        assert!(error.to_string().contains("is not a regular file"));
        assert_eq!(
            std::fs::read(&retained).unwrap(),
            serde_json::to_vec_pretty(&contract).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_save_preserves_authoritative_file_mode() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, root) = canonical_tempdir();
        let path = root.join("tx.json");
        let contract = make_test_contract(1);
        write_authoritative_contract(&path, &contract);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let guard = acquire_tx_contract_lock(&root, &path).unwrap();

        save_tx_contract_atomic(&guard, &path, &contract).unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn oversize_post_execution_snapshot_is_retained_for_recovery() {
        let (_dir, root) = canonical_tempdir();
        let path = root.join("tx.json");
        let baseline = make_test_contract(1);
        write_authoritative_contract(&path, &baseline);
        let guard = acquire_tx_contract_lock(&root, &path).unwrap();
        let mut oversized = baseline.clone();
        let StepAction::SendText { text, .. } = &mut oversized.plan.steps[0].action else {
            unreachable!("test fixture uses send_text")
        };
        *text = "x".repeat(TX_CONTRACT_MAX_BYTES);

        let err = save_tx_contract_atomic(&guard, &path, &oversized).unwrap_err();

        assert_eq!(err.kind(), TxContractStoreErrorKind::TooLarge);
        let recovery_path = err
            .recovery_path()
            .expect("complete oversize evidence must be retained");
        let recovery_bytes = std::fs::read(recovery_path).unwrap();
        assert!(recovery_bytes.len() > TX_CONTRACT_MAX_BYTES);
        let recovered: MissionTxContract = serde_json::from_slice(&recovery_bytes).unwrap();
        assert_eq!(recovered.compute_hash(), oversized.compute_hash());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            serde_json::to_vec_pretty(&baseline).unwrap()
        );
    }

    #[test]
    fn pre_rename_failure_retains_deserializable_recovery_snapshot() {
        let (_dir, root) = canonical_tempdir();
        let path = root.join("tx.json");
        let baseline = make_test_contract(1);
        let replacement = make_test_contract(2);
        write_authoritative_contract(&path, &baseline);
        let guard = acquire_tx_contract_lock(&root, &path).unwrap();

        let err = save_tx_contract_atomic_impl(&guard, &path, &replacement, |point| {
            if point == TxContractSaveFaultPoint::BeforeAtomicReplace {
                Err(std::io::Error::other("injected pre-rename failure"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();

        assert_eq!(err.kind(), TxContractStoreErrorKind::Rename);
        assert!(err.to_string().contains("injected pre-rename failure"));
        let recovery_path = err
            .recovery_path()
            .expect("pre-rename failure must surface the retained snapshot");
        let recovered: MissionTxContract =
            serde_json::from_slice(&std::fs::read(recovery_path).unwrap()).unwrap();
        assert_eq!(recovered.compute_hash(), replacement.compute_hash());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            serde_json::to_vec_pretty(&baseline).unwrap()
        );
    }

    #[test]
    fn post_effect_destination_substitution_preserves_foreign_sentinel_and_recovery() {
        let (_dir, root) = canonical_tempdir();
        let path = root.join("tx.json");
        let original_detached = root.join("tx-original-detached.json");
        let baseline = make_test_contract(1);
        let replacement = make_test_contract(2);
        let foreign_sentinel = b"foreign transaction sentinel".to_vec();
        write_authoritative_contract(&path, &baseline);
        let guard = acquire_tx_contract_lock(&root, &path).unwrap();
        guard.authorizes(guard.authoritative_path()).unwrap();

        let mut substituted = false;
        let err = save_tx_contract_atomic_impl(&guard, &path, &replacement, |point| {
            if point == TxContractSaveFaultPoint::BeforeAtomicReplace && !substituted {
                std::fs::rename(&path, &original_detached)?;
                std::fs::write(&path, &foreign_sentinel)?;
                substituted = true;
            }
            Ok(())
        })
        .unwrap_err();

        assert_eq!(err.kind(), TxContractStoreErrorKind::Rename);
        assert!(
            err.to_string()
                .contains("basename no longer names the pre-effect authorized inode")
        );
        assert_eq!(std::fs::read(&path).unwrap(), foreign_sentinel);
        assert_eq!(
            std::fs::read(&original_detached).unwrap(),
            serde_json::to_vec_pretty(&baseline).unwrap()
        );
        let recovery_path = err
            .recovery_path()
            .expect("post-effect destination substitution must retain recovery evidence");
        let recovered: MissionTxContract =
            serde_json::from_slice(&std::fs::read(recovery_path).unwrap()).unwrap();
        assert_eq!(recovered.compute_hash(), replacement.compute_hash());
    }

    #[test]
    fn file_sync_failure_retains_deserializable_snapshot_without_replacement() {
        let (_dir, root) = canonical_tempdir();
        let path = root.join("tx.json");
        let baseline = make_test_contract(1);
        let replacement = make_test_contract(2);
        write_authoritative_contract(&path, &baseline);
        let authoritative_before = std::fs::read(&path).unwrap();
        let guard = acquire_tx_contract_lock(&root, &path).unwrap();

        let err = save_tx_contract_atomic_impl(&guard, &path, &replacement, |point| {
            if point == TxContractSaveFaultPoint::BeforeFileSync {
                Err(std::io::Error::other("injected file-sync failure"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();

        assert_eq!(err.kind(), TxContractStoreErrorKind::Sync);
        assert!(err.to_string().contains("injected file-sync failure"));
        let recovery_path = err
            .recovery_path()
            .expect("file-sync failure must surface the complete attempted snapshot");
        let recovered: MissionTxContract =
            serde_json::from_slice(&std::fs::read(recovery_path).unwrap()).unwrap();
        assert_eq!(recovered.compute_hash(), replacement.compute_hash());
        assert_eq!(std::fs::read(&path).unwrap(), authoritative_before);
    }

    #[test]
    fn parent_sync_failure_reports_that_authoritative_bytes_were_replaced() {
        let (_dir, root) = canonical_tempdir();
        let path = root.join("tx.json");
        let baseline = make_test_contract(1);
        let replacement = make_test_contract(2);
        write_authoritative_contract(&path, &baseline);
        let guard = acquire_tx_contract_lock(&root, &path).unwrap();

        let err = save_tx_contract_atomic_impl(&guard, &path, &replacement, |point| {
            if point == TxContractSaveFaultPoint::ParentDirectorySync {
                Err(std::io::Error::other("injected parent-sync failure"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();

        assert_eq!(err.kind(), TxContractStoreErrorKind::Sync);
        assert!(err.recovery_path().is_none());
        assert!(
            err.to_string()
                .contains("transaction contract was replaced")
        );
        assert!(err.to_string().contains("injected parent-sync failure"));
        let authoritative: MissionTxContract =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(authoritative.compute_hash(), replacement.compute_hash());
    }

    struct CommitDispatchPanicExecutor;

    // Every executor below builds its inputs from the `plan` helpers and
    // dispatches nothing externally, so each is a genuine
    // `NonEffectfulStepExecutor` and may drive the storeless entrypoints.
    impl effect_seal::NonEffectful for CommitDispatchPanicExecutor {}

    impl StepExecutor for CommitDispatchPanicExecutor {
        fn evaluate_gates(
            &self,
            contract: &MissionTxContract,
            _now_ms: i64,
        ) -> Vec<TxPrepareGateInput> {
            crate::plan::tx_prepare_gate_inputs_allow_all(contract)
        }

        fn execute_steps(
            &self,
            contract: &MissionTxContract,
            _fail_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCommitStepInput> {
            crate::plan::mission_tx_commit_step_inputs(contract, None, now_ms)
        }

        fn execute_compensations(
            &self,
            _contract: &MissionTxContract,
            commit_report: &TxCommitReport,
            _fail_for_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCompensationStepInput> {
            crate::plan::mission_tx_compensation_inputs(commit_report, None, now_ms)
        }
    }

    #[derive(Clone)]
    struct RecordingExecutor {
        dispatched_steps: Rc<RefCell<Vec<String>>>,
    }

    impl RecordingExecutor {
        fn new() -> (Self, Rc<RefCell<Vec<String>>>) {
            let dispatched_steps = Rc::new(RefCell::new(Vec::new()));
            (
                Self {
                    dispatched_steps: Rc::clone(&dispatched_steps),
                },
                dispatched_steps,
            )
        }
    }

    impl effect_seal::NonEffectful for RecordingExecutor {}

    impl StepExecutor for RecordingExecutor {
        fn evaluate_gates(
            &self,
            contract: &MissionTxContract,
            _now_ms: i64,
        ) -> Vec<TxPrepareGateInput> {
            crate::plan::tx_prepare_gate_inputs_allow_all(contract)
        }

        fn execute_steps(
            &self,
            contract: &MissionTxContract,
            fail_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCommitStepInput> {
            self.dispatched_steps.borrow_mut().extend(
                contract
                    .plan
                    .steps
                    .iter()
                    .map(|step| step.step_id.0.clone()),
            );
            crate::plan::mission_tx_commit_step_inputs(contract, fail_step, now_ms)
        }

        fn execute_compensations(
            &self,
            _contract: &MissionTxContract,
            commit_report: &TxCommitReport,
            fail_for_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCompensationStepInput> {
            crate::plan::mission_tx_compensation_inputs(commit_report, fail_for_step, now_ms)
        }
    }

    struct MalformedPrepareGateExecutor;

    impl effect_seal::NonEffectful for MalformedPrepareGateExecutor {}

    impl StepExecutor for MalformedPrepareGateExecutor {
        fn evaluate_gates(
            &self,
            contract: &MissionTxContract,
            _now_ms: i64,
        ) -> Vec<TxPrepareGateInput> {
            let mut gates = crate::plan::tx_prepare_gate_inputs_allow_all(contract);
            if let Some(first) = gates.first_mut() {
                first.step_id.0.clear();
            }
            gates
        }

        fn execute_steps(
            &self,
            _contract: &MissionTxContract,
            _fail_step: Option<&str>,
            _now_ms: i64,
        ) -> Vec<TxCommitStepInput> {
            panic!("malformed prepare gates must stop before commit dispatch")
        }

        fn execute_compensations(
            &self,
            _contract: &MissionTxContract,
            _commit_report: &TxCommitReport,
            _fail_for_step: Option<&str>,
            _now_ms: i64,
        ) -> Vec<TxCompensationStepInput> {
            panic!("malformed prepare gates must stop before compensation dispatch")
        }
    }

    #[derive(Clone)]
    struct CompensationRecordingExecutor {
        dispatched_steps: Rc<RefCell<Vec<String>>>,
        deny_gates: bool,
    }

    impl CompensationRecordingExecutor {
        fn new(deny_gates: bool) -> (Self, Rc<RefCell<Vec<String>>>) {
            let dispatched_steps = Rc::new(RefCell::new(Vec::new()));
            (
                Self {
                    dispatched_steps: Rc::clone(&dispatched_steps),
                    deny_gates,
                },
                dispatched_steps,
            )
        }
    }

    impl effect_seal::NonEffectful for CompensationRecordingExecutor {}

    impl StepExecutor for CompensationRecordingExecutor {
        fn evaluate_gates(
            &self,
            contract: &MissionTxContract,
            _now_ms: i64,
        ) -> Vec<TxPrepareGateInput> {
            let mut gates = crate::plan::tx_prepare_gate_inputs_allow_all(contract);
            if self.deny_gates {
                for gate in &mut gates {
                    gate.policy_passed = false;
                    gate.policy_reason_code = Some("compensation_policy_denied".to_string());
                }
            }
            gates
        }

        fn execute_steps(
            &self,
            contract: &MissionTxContract,
            fail_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCommitStepInput> {
            crate::plan::mission_tx_commit_step_inputs(contract, fail_step, now_ms)
        }

        fn execute_compensations(
            &self,
            _contract: &MissionTxContract,
            commit_report: &TxCommitReport,
            fail_for_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCompensationStepInput> {
            self.dispatched_steps.borrow_mut().extend(
                commit_report
                    .step_results
                    .iter()
                    .filter(|result| result.outcome.is_committed())
                    .map(|result| result.step_id.0.clone()),
            );
            crate::plan::mission_tx_compensation_inputs(commit_report, fail_for_step, now_ms)
        }
    }

    #[derive(Clone)]
    struct CompensationGateAuditExecutor {
        compensation_gate_calls: Rc<RefCell<usize>>,
        dispatched_steps: RecordedStepIds,
        deny_compensation_gates: bool,
    }

    impl CompensationGateAuditExecutor {
        fn new(deny_compensation_gates: bool) -> (Self, Rc<RefCell<usize>>, RecordedStepIds) {
            let compensation_gate_calls = Rc::new(RefCell::new(0));
            let dispatched_steps = Rc::new(RefCell::new(Vec::new()));
            (
                Self {
                    compensation_gate_calls: Rc::clone(&compensation_gate_calls),
                    dispatched_steps: Rc::clone(&dispatched_steps),
                    deny_compensation_gates,
                },
                compensation_gate_calls,
                dispatched_steps,
            )
        }
    }

    impl effect_seal::NonEffectful for CompensationGateAuditExecutor {}

    impl StepExecutor for CompensationGateAuditExecutor {
        fn evaluate_gates(
            &self,
            contract: &MissionTxContract,
            _now_ms: i64,
        ) -> Vec<TxPrepareGateInput> {
            let is_compensation_gate = contract.plan.compensations.is_empty();
            let mut gates = crate::plan::tx_prepare_gate_inputs_allow_all(contract);
            if is_compensation_gate {
                *self.compensation_gate_calls.borrow_mut() += 1;
                if self.deny_compensation_gates {
                    for gate in &mut gates {
                        gate.policy_passed = false;
                        gate.policy_reason_code = Some("compensation_policy_denied".to_string());
                    }
                }
            }
            gates
        }

        fn execute_steps(
            &self,
            contract: &MissionTxContract,
            fail_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCommitStepInput> {
            crate::plan::mission_tx_commit_step_inputs(contract, fail_step, now_ms)
        }

        fn execute_compensations(
            &self,
            _contract: &MissionTxContract,
            commit_report: &TxCommitReport,
            fail_for_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCompensationStepInput> {
            self.dispatched_steps.borrow_mut().extend(
                commit_report
                    .step_results
                    .iter()
                    .filter(|result| result.outcome.is_committed())
                    .map(|result| result.step_id.0.clone()),
            );
            crate::plan::mission_tx_compensation_inputs(commit_report, fail_for_step, now_ms)
        }
    }

    struct NoFreshCompensationExecutor;

    impl effect_seal::NonEffectful for NoFreshCompensationExecutor {}

    impl StepExecutor for NoFreshCompensationExecutor {
        fn evaluate_gates(
            &self,
            _contract: &MissionTxContract,
            _now_ms: i64,
        ) -> Vec<TxPrepareGateInput> {
            panic!("dedup-only compensation recovery must not evaluate fresh dispatch gates")
        }

        fn execute_steps(
            &self,
            _contract: &MissionTxContract,
            _fail_step: Option<&str>,
            _now_ms: i64,
        ) -> Vec<TxCommitStepInput> {
            panic!("rollback-only executor must not dispatch commit steps")
        }

        fn execute_compensations(
            &self,
            _contract: &MissionTxContract,
            _commit_report: &TxCommitReport,
            _fail_for_step: Option<&str>,
            _now_ms: i64,
        ) -> Vec<TxCompensationStepInput> {
            panic!("dedup-only compensation recovery must not dispatch an external effect")
        }
    }

    #[derive(Clone)]
    struct SelectivePrepareDenyExecutor {
        denied_step_id: String,
        committed_steps: RecordedStepIds,
        compensated_steps: RecordedStepIds,
    }

    impl SelectivePrepareDenyExecutor {
        fn new(denied_step_id: &str) -> (Self, RecordedStepIds, RecordedStepIds) {
            let committed_steps = Rc::new(RefCell::new(Vec::new()));
            let compensated_steps = Rc::new(RefCell::new(Vec::new()));
            (
                Self {
                    denied_step_id: denied_step_id.to_string(),
                    committed_steps: Rc::clone(&committed_steps),
                    compensated_steps: Rc::clone(&compensated_steps),
                },
                committed_steps,
                compensated_steps,
            )
        }
    }

    impl effect_seal::NonEffectful for SelectivePrepareDenyExecutor {}

    impl StepExecutor for SelectivePrepareDenyExecutor {
        fn evaluate_gates(
            &self,
            contract: &MissionTxContract,
            _now_ms: i64,
        ) -> Vec<TxPrepareGateInput> {
            let mut gates = crate::plan::tx_prepare_gate_inputs_allow_all(contract);
            for gate in &mut gates {
                if gate.step_id.0 == self.denied_step_id {
                    gate.policy_passed = false;
                    gate.policy_reason_code = Some("selective_prepare_denial".to_string());
                }
            }
            gates
        }

        fn execute_steps(
            &self,
            contract: &MissionTxContract,
            fail_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCommitStepInput> {
            self.committed_steps.borrow_mut().extend(
                contract
                    .plan
                    .steps
                    .iter()
                    .map(|step| step.step_id.0.clone()),
            );
            crate::plan::mission_tx_commit_step_inputs(contract, fail_step, now_ms)
        }

        fn execute_compensations(
            &self,
            _contract: &MissionTxContract,
            commit_report: &TxCommitReport,
            fail_for_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCompensationStepInput> {
            self.compensated_steps.borrow_mut().extend(
                commit_report
                    .step_results
                    .iter()
                    .filter(|result| result.outcome.is_committed())
                    .map(|result| result.step_id.0.clone()),
            );
            crate::plan::mission_tx_compensation_inputs(commit_report, fail_for_step, now_ms)
        }
    }

    #[test]
    fn execution_rejects_invalid_entry_state_without_dispatch() {
        let (executor, dispatched_steps) = RecordingExecutor::new();
        let engine = TxExecutionEngine::new(executor, TxExecutionConfig::default());
        let mut mismatched = make_test_contract(1);
        mismatched.outcome = TxOutcome::Failed;

        let mismatch_err = engine.execute(&mut mismatched, 5_000).unwrap_err();
        assert!(matches!(mismatch_err, TxExecutionError::InvalidContract(_)));
        assert!(dispatched_steps.borrow().is_empty());

        let mut prepared = make_test_contract(1);
        prepared.lifecycle_state = MissionTxState::Prepared;
        let prepared_err = engine.execute(&mut prepared, 5_001).unwrap_err();
        assert!(matches!(prepared_err, TxExecutionError::InvalidContract(_)));
        assert!(dispatched_steps.borrow().is_empty());

        let mut negative_time = make_test_contract(1);
        let time_err = engine.execute(&mut negative_time, -1).unwrap_err();
        assert!(matches!(time_err, TxExecutionError::InvalidContract(_)));
        assert!(dispatched_steps.borrow().is_empty());
    }

    #[test]
    fn receipt_sequence_exhaustion_is_rejected_before_dispatch() {
        let (executor, dispatched_steps) = RecordingExecutor::new();
        let engine = TxExecutionEngine::new(executor, TxExecutionConfig::default());
        let mut contract = make_test_contract(1);
        contract.receipts.push(serde_json::json!({
            "seq": u64::MAX,
            "phase": "lifecycle",
            "tx_id": contract.intent.tx_id.0.clone(),
            "plan_id": contract.plan.plan_id.0.clone(),
            "state": "planned",
            "step_id": null,
            "outcome": "state_checkpoint",
            "emitted_at_ms": 1_000,
            "reason_code": "checkpoint",
            "error_code": null,
            "decision_path": "test"
        }));

        let err = engine.execute(&mut contract, 5_000).unwrap_err();

        assert!(matches!(err, TxExecutionError::InvalidContract(_)));
        assert!(err.to_string().contains("insufficient headroom"));
        assert!(dispatched_steps.borrow().is_empty());
    }

    #[test]
    fn recovery_receipt_headroom_is_rejected_before_fresh_dispatch() -> Result<(), String> {
        let mut contract = make_test_contract(3);
        contract.receipts.push(serde_json::json!({
            "seq": u64::MAX - 6,
            "phase": "lifecycle",
            "tx_id": contract.intent.tx_id.0.clone(),
            "plan_id": contract.plan.plan_id.0.clone(),
            "state": "planned",
            "step_id": null,
            "outcome": "state_checkpoint",
            "emitted_at_ms": 1_000,
            "reason_code": "checkpoint",
            "error_code": null,
            "decision_path": "test"
        }));

        let (store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        store
            .create_ledger("prior-near-sequence-exhaustion", &compiled_plan)
            .map_err(|err| err.to_string())?;
        store
            .transition_phase("prior-near-sequence-exhaustion", TxPhase::Preparing)
            .map_err(|err| err.to_string())?;
        store
            .transition_phase("prior-near-sequence-exhaustion", TxPhase::Committing)
            .map_err(|err| err.to_string())?;
        for (step_id, timestamp_ms) in [("step-0", 5_000), ("step-1", 5_001)] {
            record_durable_test_outcome(
                &mut store,
                "prior-near-sequence-exhaustion",
                test_commit_key(&contract, step_id),
                StepOutcome::Success {
                    result: Some(format!("recovered_{step_id}")),
                },
                StepRisk::High,
                &format!("agent-{step_id}"),
                timestamp_ms,
            )?;
        }
        store
            .transition_phase("prior-near-sequence-exhaustion", TxPhase::Aborted)
            .map_err(|err| err.to_string())?;
        store
            .archive_ledger("prior-near-sequence-exhaustion")
            .map_err(|err| err.to_string())?;

        let original_contract = serde_json::to_vec(&contract).map_err(|err| err.to_string())?;
        let original_spool = durable_ledger_file_snapshot(store_dir.path());
        let (executor, dispatched_steps) = RecordingExecutor::new();
        let engine = TxExecutionEngine::new(
            executor,
            TxExecutionConfig {
                fail_step: Some("step-2".to_string()),
                ..TxExecutionConfig::default()
            },
        );

        let error = engine
            .execute_with_store(&mut contract, &mut store, 6_000)
            .expect_err("three-phase receipt headroom must fail before recovery or dispatch");

        assert!(matches!(error, TxExecutionError::InvalidContract(_)));
        assert!(error.to_string().contains("insufficient headroom"));
        assert!(dispatched_steps.borrow().is_empty());
        assert_eq!(
            serde_json::to_vec(&contract).map_err(|err| err.to_string())?,
            original_contract
        );
        assert_eq!(
            durable_ledger_file_snapshot(store_dir.path()),
            original_spool
        );
        Ok(())
    }

    #[test]
    fn execute_happy_path_single_step() {
        let mut contract = make_test_contract(1);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert_eq!(result.final_state, MissionTxState::Committed);
        assert_eq!(result.outcome, TxOutcome::Committed);
        assert!(result.commit_report.is_some());
        assert!(result.compensation_report.is_none());
        assert!(result.prepare_report.outcome.commit_eligible());
        assert!(!result.events.is_empty());
    }

    #[test]
    fn execute_happy_path_multiple_steps() {
        let mut contract = make_test_contract(5);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert_eq!(result.final_state, MissionTxState::Committed);
        assert_eq!(result.outcome, TxOutcome::Committed);
        let commit = result.commit_report.unwrap();
        assert_eq!(commit.committed_count, 5);
        assert_eq!(commit.failed_count, 0);
        assert_eq!(commit.skipped_count, 0);
    }

    #[test]
    fn durable_entry_points_reject_in_memory_stores_before_mutation() {
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let mut contract = make_test_contract(1);
        let original = contract.clone();
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());

        let error = engine
            .execute_with_store(&mut contract, &mut store, 5_000)
            .unwrap_err();

        assert!(matches!(error, TxExecutionError::InvalidContract(_)));
        assert_eq!(
            serde_json::to_value(&contract).unwrap(),
            serde_json::to_value(&original).unwrap()
        );
        assert_eq!(store.active_count(), 0);
    }

    #[test]
    fn execute_with_store_dedups_full_replay_before_dispatch() -> Result<(), String> {
        let (executor, dispatched_steps) = RecordingExecutor::new();
        let engine = TxExecutionEngine::new(executor, TxExecutionConfig::default());
        let (_store_dir, mut store) = durable_store();
        let mut first_contract = make_test_contract(2);
        let first = engine
            .execute_with_store(&mut first_contract, &mut store, 5_000)
            .map_err(|err| err.to_string())?;
        assert_eq!(first.final_state, MissionTxState::Committed);
        assert_eq!(
            dispatched_steps.borrow().as_slice(),
            ["step-0".to_string(), "step-1".to_string()]
        );

        dispatched_steps.borrow_mut().clear();
        let mut replay_contract = make_test_contract(2);
        let replay = engine
            .execute_with_store(&mut replay_contract, &mut store, 6_000)
            .map_err(|err| err.to_string())?;

        assert!(dispatched_steps.borrow().is_empty());
        let commit = replay
            .commit_report
            .as_ref()
            .ok_or_else(|| "missing replay commit report".to_string())?;
        assert_eq!(commit.committed_count, 2);
        assert_eq!(commit.failed_count, 0);
        assert!(
            commit
                .step_results
                .iter()
                .all(|step| step.decision_path == "commit_phase->committed")
        );
        Ok(())
    }

    #[test]
    fn changed_action_with_reused_ids_is_not_deduped() -> Result<(), String> {
        let (executor, dispatched_steps) = RecordingExecutor::new();
        let engine = TxExecutionEngine::new(executor, TxExecutionConfig::default());
        let (_store_dir, mut store) = durable_store();
        let mut first_contract = make_test_contract(1);
        engine
            .execute_with_store(&mut first_contract, &mut store, 5_000)
            .map_err(|err| err.to_string())?;

        dispatched_steps.borrow_mut().clear();
        let mut changed_contract = make_test_contract(1);
        let StepAction::SendText { text, .. } = &mut changed_contract.plan.steps[0].action else {
            return Err("test fixture uses send_text".to_string());
        };
        *text = "different external effect".to_string();
        engine
            .execute_with_store(&mut changed_contract, &mut store, 6_000)
            .map_err(|err| err.to_string())?;

        assert_eq!(dispatched_steps.borrow().as_slice(), ["step-0".to_string()]);
        Ok(())
    }

    #[test]
    fn durable_commit_reserves_only_the_step_about_to_dispatch() -> Result<(), String> {
        let (executor, dispatched_steps) = RecordingExecutor::new();
        let engine = TxExecutionEngine::new(
            executor,
            TxExecutionConfig {
                auto_compensate: false,
                fail_step: Some("step-0".to_string()),
                ..TxExecutionConfig::default()
            },
        );
        let (_store_dir, mut store) = durable_store();
        let mut contract = make_test_contract(3);

        let result = engine
            .execute_with_store(&mut contract, &mut store, 5_000)
            .map_err(|err| err.to_string())?;

        assert_eq!(dispatched_steps.borrow().as_slice(), ["step-0".to_string()]);
        assert!(matches!(
            store.peek_cached_outcome(&test_commit_key(&contract, "step-0"), 5_000),
            Some(StepOutcome::Failed { .. })
        ));
        assert!(
            store
                .peek_cached_outcome(&test_commit_key(&contract, "step-1"), 5_000)
                .is_none()
        );
        assert!(
            store
                .peek_cached_outcome(&test_commit_key(&contract, "step-2"), 5_000)
                .is_none()
        );
        assert_eq!(result.final_state, MissionTxState::Failed);
        Ok(())
    }

    #[test]
    fn paused_store_backed_execution_resumes_without_poisoned_skip_keys() -> Result<(), String> {
        let (executor, dispatched_steps) = RecordingExecutor::new();
        let paused_engine = TxExecutionEngine::new(
            executor.clone(),
            TxExecutionConfig {
                paused: true,
                ..TxExecutionConfig::default()
            },
        );
        let resumed_engine = TxExecutionEngine::new(executor, TxExecutionConfig::default());
        let (_store_dir, mut store) = durable_store();
        let mut contract = make_test_contract(2);

        let paused = paused_engine
            .execute_with_store(&mut contract, &mut store, 5_000)
            .map_err(|err| err.to_string())?;
        assert_eq!(paused.outcome, TxOutcome::Pending);
        assert_eq!(contract.lifecycle_state, MissionTxState::Committing);
        assert!(dispatched_steps.borrow().is_empty());
        assert_eq!(store.active_count(), 1);
        assert!(
            store
                .peek_cached_outcome(&test_commit_key(&contract, "step-0"), 5_000)
                .is_none()
        );

        let resumed = resumed_engine
            .execute_with_store(&mut contract, &mut store, 6_000)
            .map_err(|err| err.to_string())?;

        assert_eq!(resumed.final_state, MissionTxState::Committed);
        assert_eq!(
            dispatched_steps.borrow().as_slice(),
            ["step-0".to_string(), "step-1".to_string()]
        );
        assert_eq!(store.active_count(), 0);
        Ok(())
    }

    #[test]
    fn execute_with_store_dedups_partial_prior_commit_before_dispatch() -> Result<(), String> {
        let (executor, dispatched_steps) = RecordingExecutor::new();
        let engine = TxExecutionEngine::new(executor, TxExecutionConfig::default());
        let (_store_dir, mut store) = durable_store();
        let contract = make_test_contract(2);
        let compiled_plan = compiled_plan_from_contract(&contract);
        store
            .create_ledger("prior-exec", &compiled_plan)
            .map_err(|err| err.to_string())?;
        record_durable_test_outcome(
            &mut store,
            "prior-exec",
            test_commit_key(&contract, "step-0"),
            StepOutcome::Success {
                result: Some("prior_commit".to_string()),
            },
            StepRisk::High,
            "agent-step-0",
            5_000,
        )?;

        let mut replay_contract = make_test_contract(2);
        let replay = engine
            .execute_with_store(&mut replay_contract, &mut store, 6_000)
            .map_err(|err| err.to_string())?;

        assert_eq!(dispatched_steps.borrow().as_slice(), ["step-1".to_string()]);
        let commit = replay
            .commit_report
            .as_ref()
            .ok_or_else(|| "missing replay commit report".to_string())?;
        assert_eq!(commit.committed_count, 2);
        assert_eq!(commit.failed_count, 0);
        assert_eq!(
            store.peek_cached_outcome(&test_commit_key(&replay_contract, "step-1"), 6_000),
            Some(&StepOutcome::Success {
                result: Some("commit_step_succeeded".to_string())
            })
        );
        let forensic = replay
            .forensic_bundle
            .as_ref()
            .ok_or_else(|| "missing replay forensic bundle".to_string())?;
        let bundle_step_order = forensic
            .ledger
            .records
            .iter()
            .map(|record| record.step_id.as_str())
            .collect::<Vec<_>>();
        let result_step_order = replay
            .ledger
            .records()
            .iter()
            .map(|record| record.idem_key.step_id())
            .collect::<Vec<_>>();
        assert_eq!(bundle_step_order, result_step_order);
        assert_eq!(forensic.ledger.last_hash, replay.ledger.last_hash());
        Ok(())
    }

    #[test]
    fn restart_reconciles_durable_successes_beyond_dedup_capacity() -> Result<(), String> {
        let store_dir = tempfile::tempdir().map_err(|err| err.to_string())?;
        let policy = IdempotencyPolicy {
            dedup_capacity: 1,
            ..IdempotencyPolicy::default()
        };
        let mut store = IdempotencyStore::open(store_dir.path(), policy.clone())
            .map_err(|err| err.to_string())?;
        let contract = make_test_contract(3);
        let compiled_plan = compiled_plan_from_contract(&contract);
        store
            .create_ledger("prior-over-capacity", &compiled_plan)
            .map_err(|err| err.to_string())?;
        store
            .transition_phase("prior-over-capacity", TxPhase::Preparing)
            .map_err(|err| err.to_string())?;
        store
            .transition_phase("prior-over-capacity", TxPhase::Committing)
            .map_err(|err| err.to_string())?;
        for (index, step) in contract.plan.steps.iter().enumerate() {
            let timestamp_ms = 5_000
                + u64::try_from(index)
                    .map_err(|_| "test step index does not fit in u64".to_string())?;
            record_durable_test_outcome(
                &mut store,
                "prior-over-capacity",
                test_commit_key(&contract, &step.step_id.0),
                StepOutcome::Success {
                    result: Some(format!("prior_commit_{index}")),
                },
                StepRisk::High,
                &format!("agent-{}", step.step_id.0),
                timestamp_ms,
            )?;
        }
        store
            .transition_phase("prior-over-capacity", TxPhase::Completed)
            .map_err(|err| err.to_string())?;
        store
            .archive_ledger("prior-over-capacity")
            .map_err(|err| err.to_string())?;

        drop(store);
        let mut store =
            IdempotencyStore::open(store_dir.path(), policy).map_err(|err| err.to_string())?;
        let (executor, dispatched_steps) = RecordingExecutor::new();
        let engine = TxExecutionEngine::new(executor, TxExecutionConfig::default());
        let mut replay_contract = make_test_contract(3);
        assert!(
            store
                .peek_cached_outcome(&test_commit_key(&replay_contract, "step-0"), 6_000)
                .is_none(),
            "the committed receipt regression must exercise proof beyond the bounded cache"
        );
        let preexisting_receipt = recovered_commit_success_report(
            &replay_contract,
            &HashSet::from(["step-0".to_string()]),
            5_500,
        )
        .map_err(|err| err.to_string())?;
        replay_contract
            .receipts
            .extend(preexisting_receipt.receipts);
        replay_contract.lifecycle_state = MissionTxState::Committing;
        replay_contract.outcome = TxOutcome::Pending;

        let replay = engine
            .execute_with_store(&mut replay_contract, &mut store, 6_000)
            .map_err(|err| err.to_string())?;

        assert!(dispatched_steps.borrow().is_empty());
        assert_eq!(replay.final_state, MissionTxState::Committed);
        assert_eq!(replay.ledger.records().len(), 3);
        assert!(
            replay
                .ledger
                .records()
                .iter()
                .all(|record| matches!(record.outcome, StepOutcome::Success { .. }))
        );
        for step in &replay_contract.plan.steps {
            assert!(latest_tx_receipt_matches(
                &replay_contract,
                "commit",
                &step.step_id.0,
                "committed"
            ));
        }
        Ok(())
    }

    #[test]
    fn durable_success_after_prior_failure_is_reconciled_before_conflict() -> Result<(), String> {
        let mut contract = make_test_contract(2);
        let (_store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        store
            .create_ledger("prior-mixed-outcomes", &compiled_plan)
            .map_err(|err| err.to_string())?;
        store
            .transition_phase("prior-mixed-outcomes", TxPhase::Preparing)
            .map_err(|err| err.to_string())?;
        store
            .transition_phase("prior-mixed-outcomes", TxPhase::Committing)
            .map_err(|err| err.to_string())?;
        record_durable_test_outcome(
            &mut store,
            "prior-mixed-outcomes",
            test_commit_key(&contract, "step-0"),
            StepOutcome::Failed {
                error_code: "FTX3999".to_string(),
                error_message: "failed before later effect evidence was saved".to_string(),
                compensated: false,
            },
            StepRisk::High,
            "agent-step-0",
            5_000,
        )?;
        record_durable_test_outcome(
            &mut store,
            "prior-mixed-outcomes",
            test_commit_key(&contract, "step-1"),
            StepOutcome::Success {
                result: Some("later_effect_completed".to_string()),
            },
            StepRisk::High,
            "agent-step-1",
            5_001,
        )?;
        store
            .transition_phase("prior-mixed-outcomes", TxPhase::Aborted)
            .map_err(|err| err.to_string())?;
        store
            .archive_ledger("prior-mixed-outcomes")
            .map_err(|err| err.to_string())?;

        let (executor, dispatched_steps) = RecordingExecutor::new();
        let engine = TxExecutionEngine::new(executor, TxExecutionConfig::default());
        let error = engine
            .execute_with_store(&mut contract, &mut store, 6_000)
            .expect_err("the prior failed outcome must remain a replay conflict");

        assert!(matches!(
            error,
            TxExecutionError::DedupConflict { ref step_id, .. } if step_id == "step-0"
        ));
        assert!(dispatched_steps.borrow().is_empty());
        assert!(latest_tx_receipt_matches(
            &contract,
            "commit",
            "step-1",
            "committed"
        ));
        Ok(())
    }

    #[test]
    fn durable_success_is_reconciled_before_malformed_fresh_prepare_gates() -> Result<(), String> {
        let mut contract = make_test_contract(2);
        let (_store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        store
            .create_ledger("prior-success-before-gate-error", &compiled_plan)
            .map_err(|err| err.to_string())?;
        store
            .transition_phase("prior-success-before-gate-error", TxPhase::Preparing)
            .map_err(|err| err.to_string())?;
        store
            .transition_phase("prior-success-before-gate-error", TxPhase::Committing)
            .map_err(|err| err.to_string())?;
        record_durable_test_outcome(
            &mut store,
            "prior-success-before-gate-error",
            test_commit_key(&contract, "step-1"),
            StepOutcome::Success {
                result: Some("effect_completed_before_gate_error".to_string()),
            },
            StepRisk::High,
            "agent-step-1",
            5_000,
        )?;
        store
            .transition_phase("prior-success-before-gate-error", TxPhase::Aborted)
            .map_err(|err| err.to_string())?;
        store
            .archive_ledger("prior-success-before-gate-error")
            .map_err(|err| err.to_string())?;

        let engine =
            TxExecutionEngine::new(MalformedPrepareGateExecutor, TxExecutionConfig::default());
        let error = engine
            .execute_with_store(&mut contract, &mut store, 6_000)
            .expect_err("missing unresolved gate input must fail prepare");

        assert!(matches!(error, TxExecutionError::PreparePhase(_)));
        assert!(latest_tx_receipt_matches(
            &contract,
            "commit",
            "step-1",
            "committed"
        ));
        Ok(())
    }

    #[test]
    fn rejected_prepare_recovers_later_durable_success_and_compensates_it() -> Result<(), String> {
        let mut contract = make_test_contract(2);
        let (_store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        store
            .create_ledger("prior-rejected-prepare", &compiled_plan)
            .map_err(|err| err.to_string())?;
        record_durable_test_outcome(
            &mut store,
            "prior-rejected-prepare",
            test_commit_key(&contract, "step-1"),
            StepOutcome::Success {
                result: Some("effect_completed_before_crash".to_string()),
            },
            StepRisk::High,
            "agent-step-1",
            5_000,
        )?;

        let (executor, committed_steps, compensated_steps) =
            SelectivePrepareDenyExecutor::new("step-0");
        let engine = TxExecutionEngine::new(executor, TxExecutionConfig::default());
        let result = engine
            .execute_with_store(&mut contract, &mut store, 6_000)
            .map_err(|err| err.to_string())?;

        assert_eq!(result.prepare_report.outcome, TxPrepareOutcome::Denied);
        let commit = result
            .commit_report
            .as_ref()
            .ok_or_else(|| "durable recovery omitted the commit report".to_string())?;
        assert!(matches!(
            commit.step_results[0].outcome,
            crate::plan::TxCommitStepOutcome::Skipped { .. }
        ));
        assert!(matches!(
            commit.step_results[1].outcome,
            crate::plan::TxCommitStepOutcome::Committed { .. }
        ));
        assert!(committed_steps.borrow().is_empty());
        assert_eq!(
            compensated_steps.borrow().as_slice(),
            ["step-1".to_string()]
        );
        assert_eq!(result.final_state, MissionTxState::RolledBack);
        assert_eq!(result.outcome, TxOutcome::Compensated);
        assert!(latest_tx_receipt_matches(
            &contract,
            "commit",
            "step-1",
            "committed"
        ));
        assert!(latest_tx_receipt_matches(
            &contract, "commit", "step-0", "skipped"
        ));
        assert!(latest_tx_receipt_matches(
            &contract,
            "compensate",
            "step-1",
            "compensated"
        ));
        assert!(matches!(
            store.peek_cached_outcome(
                &test_compensation_key(&contract, "step-1"),
                6_000
            ),
            Some(StepOutcome::Compensated {
                original_outcome,
                ..
            }) if matches!(
                original_outcome.as_ref(),
                StepOutcome::Success { result: Some(result) }
                    if result == "effect_completed_before_crash"
            )
        ));
        Ok(())
    }

    #[test]
    fn paused_mixed_recovery_keeps_sticky_commit_evidence_and_fails_closed() -> Result<(), String> {
        let mut contract = make_test_contract(2);
        let (_store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        store
            .create_ledger("prior-paused-recovery", &compiled_plan)
            .map_err(|err| err.to_string())?;
        record_durable_test_outcome(
            &mut store,
            "prior-paused-recovery",
            test_commit_key(&contract, "step-1"),
            StepOutcome::Success {
                result: Some("effect_completed_before_pause".to_string()),
            },
            StepRisk::High,
            "agent-step-1",
            5_000,
        )?;

        let (executor, dispatched_steps) = RecordingExecutor::new();
        let engine = TxExecutionEngine::new(
            executor,
            TxExecutionConfig {
                paused: true,
                ..TxExecutionConfig::default()
            },
        );
        let error = engine
            .execute_with_store(&mut contract, &mut store, 6_000)
            .expect_err("paused recovery must not claim a terminal result with live effects");

        assert!(matches!(error, TxExecutionError::CompensationPhase(_)));
        assert!(error.to_string().contains("suspended"));
        assert!(dispatched_steps.borrow().is_empty());
        assert_eq!(contract.lifecycle_state, MissionTxState::Compensating);
        assert_eq!(contract.outcome, TxOutcome::Pending);
        assert!(latest_tx_receipt_matches(
            &contract,
            "commit",
            "step-1",
            "committed"
        ));
        assert!(latest_tx_receipt_matches(
            &contract, "commit", "step-0", "skipped"
        ));
        assert_eq!(store.active_count(), 1);
        Ok(())
    }

    #[test]
    fn execute_with_store_fails_closed_on_crash_orphaned_pending() -> Result<(), String> {
        let (executor, dispatched_steps) = RecordingExecutor::new();
        let engine = TxExecutionEngine::new(executor, TxExecutionConfig::default());
        let (store_dir, mut store) = durable_store();
        let contract = make_test_contract(2);
        let compiled_plan = compiled_plan_from_contract(&contract);
        store
            .create_ledger("crashed-exec", &compiled_plan)
            .map_err(|err| err.to_string())?;
        // Crash window: step-0 was reserved with the write-ahead Pending marker and
        // the process died before the terminal-outcome upgrade. A bare re-execute
        // after reopening the durable store must refuse to re-dispatch (fail
        // closed) rather than double-apply the side effect.
        let pending_key = test_commit_key(&contract, "step-0");
        let mut pending_reservation = store
            .acquire_durable_reservation("crashed-exec", &pending_key, 5_000)
            .map_err(|err| err.to_string())?;
        store
            .record_execution_reserved(
                &mut pending_reservation,
                "crashed-exec",
                pending_key,
                StepOutcome::Pending,
                StepRisk::High,
                "agent-step-0",
                5_000,
            )
            .map_err(|err| err.to_string())?;
        drop(pending_reservation);
        record_durable_test_outcome(
            &mut store,
            "crashed-exec",
            test_commit_key(&contract, "step-1"),
            StepOutcome::Success {
                result: Some("later_effect_completed_before_crash".to_string()),
            },
            StepRisk::High,
            "agent-step-1",
            5_001,
        )?;
        drop(store);
        let mut store = IdempotencyStore::open(store_dir.path(), IdempotencyPolicy::default())
            .map_err(|err| err.to_string())?;

        let mut replay_contract = make_test_contract(2);
        let err = engine
            .execute_with_store(&mut replay_contract, &mut store, 6_000)
            .err()
            .ok_or_else(|| "bare re-execute over an in-flight step must fail closed".to_string())?;

        match &err {
            TxExecutionError::DedupConflict { step_id, outcome } => {
                if step_id != "step-0" {
                    return Err(format!("conflict on unexpected step: {step_id}"));
                }
                if !outcome.contains("Pending") {
                    return Err(format!("conflict outcome should be Pending: {outcome}"));
                }
            }
            other => return Err(format!("expected DedupConflict, got: {other}")),
        }
        assert!(
            dispatched_steps.borrow().is_empty(),
            "no side effects may dispatch when an in-flight marker conflicts"
        );
        assert!(latest_tx_receipt_matches(
            &replay_contract,
            "commit",
            "step-1",
            "committed"
        ));
        Ok(())
    }

    #[test]
    fn durable_rollback_rejects_receipt_only_commit_claims_before_mutation() -> Result<(), String> {
        let mut contract = make_test_contract(1);
        let synthetic = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        synthetic
            .execute(&mut contract, 5_000)
            .map_err(|err| err.to_string())?;
        let original_contract = serde_json::to_value(&contract).map_err(|err| err.to_string())?;
        let (_store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        store
            .create_ledger("txe-existing-before-forged-rollback", &compiled_plan)
            .map_err(|err| err.to_string())?;
        store
            .transition_phase("txe-existing-before-forged-rollback", TxPhase::Preparing)
            .map_err(|err| err.to_string())?;
        let (executor, dispatched_steps) = CompensationRecordingExecutor::new(false);
        let rollback_engine = TxExecutionEngine::new(executor, TxExecutionConfig::default());

        let error = rollback_engine
            .rollback_with_store(&mut contract, &mut store, 6_000)
            .expect_err("receipt-only commit claims must not authorize durable compensation");

        assert!(matches!(
            error,
            TxExecutionError::RollbackProof {
                kind: RollbackProofKind::Missing,
                ref step_id,
                ref detail,
            } if step_id == "step-0" && detail.contains("no authoritative durable commit Success")
        ));
        assert!(dispatched_steps.borrow().is_empty());
        assert_eq!(
            serde_json::to_value(&contract).map_err(|err| err.to_string())?,
            original_contract
        );
        assert_eq!(store.active_count(), 1);
        assert_eq!(
            store
                .get_ledger("txe-existing-before-forged-rollback")
                .expect("pre-existing ledger must remain active")
                .phase(),
            TxPhase::Preparing
        );
        Ok(())
    }

    #[test]
    fn durable_rollback_rejects_downgraded_receipt_before_mutation() -> Result<(), String> {
        let (store_dir, mut store) = durable_store();
        let mut contract = make_test_contract(2);
        let commit_engine =
            TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        commit_engine
            .execute_with_store(&mut contract, &mut store, 5_000)
            .map_err(|err| err.to_string())?;
        let original_spool = durable_ledger_file_snapshot(store_dir.path());

        let downgraded = contract
            .receipts
            .iter_mut()
            .find(|receipt| {
                receipt.get("phase").and_then(serde_json::Value::as_str) == Some("commit")
                    && receipt.get("step_id").and_then(serde_json::Value::as_str) == Some("step-0")
            })
            .expect("step-0 commit receipt");
        downgraded["outcome"] = serde_json::json!("failed");
        downgraded["reason_code"] = serde_json::json!("forged_commit_failure");
        downgraded["error_code"] = serde_json::json!("FTX3999");
        downgraded["decision_path"] = serde_json::json!("forged_receipt_history");
        let original_contract = serde_json::to_value(&contract).map_err(|err| err.to_string())?;

        let (executor, dispatched_steps) = CompensationRecordingExecutor::new(false);
        let rollback_engine = TxExecutionEngine::new(executor, TxExecutionConfig::default());
        let error = rollback_engine
            .rollback_with_store(&mut contract, &mut store, 6_000)
            .expect_err("receipt history must not hide an authoritative durable commit Success");

        assert!(matches!(
            error,
            TxExecutionError::RollbackProof {
                kind: RollbackProofKind::Conflict,
                ref step_id,
                ref detail,
            } if step_id == "step-0" && detail.contains("omitted or downgraded")
        ));
        assert!(dispatched_steps.borrow().is_empty());
        assert_eq!(
            serde_json::to_value(&contract).map_err(|err| err.to_string())?,
            original_contract
        );
        assert!(
            store
                .peek_cached_outcome(&test_compensation_key(&contract, "step-0"), 6_000)
                .is_none(),
            "proof conflict must not reserve or dispatch compensation"
        );
        assert_eq!(
            durable_ledger_file_snapshot(store_dir.path()),
            original_spool,
            "proof conflict must not create, retire, or rewrite a durable execution ledger"
        );
        Ok(())
    }

    #[test]
    fn durable_rollback_holds_commit_and_compensation_proof_leases_before_mutation()
    -> Result<(), String> {
        let (store_dir, mut store) = durable_store();
        let mut contract = make_test_contract(1);
        TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default())
            .execute_with_store(&mut contract, &mut store, 5_000)
            .map_err(|err| err.to_string())?;

        let commit_key = test_commit_key(&contract, "step-0");
        let compensation_key = test_compensation_key(&contract, "step-0");
        let mut contender = IdempotencyStore::open(store_dir.path(), IdempotencyPolicy::default())
            .map_err(|err| err.to_string())?;
        set_rollback_post_proof_lease_test_hook(Some(Box::new(move || {
            for (kind, key) in [("commit", commit_key), ("compensation", compensation_key)] {
                let error = contender
                    .acquire_durable_reservation("txe-contending-rollback", &key, 6_000)
                    .expect_err("atomic rollback proof lease must reject a concurrent writer");
                assert!(
                    matches!(
                        error,
                        crate::tx_idempotency::IdempotencyError::ReservationInProgress { .. }
                    ),
                    "{kind} key contention must fail at the held proof lease, got {error}"
                );
            }
        })));

        let (executor, dispatched_steps) = CompensationRecordingExecutor::new(false);
        let result = TxExecutionEngine::new(executor, TxExecutionConfig::default())
            .rollback_with_store(&mut contract, &mut store, 6_000)
            .map_err(|err| err.to_string())?;

        assert!(result.compensation_report.is_fully_rolled_back());
        assert_eq!(dispatched_steps.borrow().as_slice(), ["step-0".to_string()]);
        assert_eq!(contract.lifecycle_state, MissionTxState::RolledBack);
        assert_eq!(contract.outcome, TxOutcome::Compensated);
        Ok(())
    }

    #[test]
    fn durable_rollback_classifies_pending_commit_proof_as_conflict() -> Result<(), String> {
        let mut contract = make_test_contract(1);
        TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default())
            .execute(&mut contract, 5_000)
            .map_err(|err| err.to_string())?;
        let original_contract = serde_json::to_value(&contract).map_err(|err| err.to_string())?;
        let (store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        let execution_id = "txe-pending-before-rollback";
        store
            .create_ledger(execution_id, &compiled_plan)
            .map_err(|err| err.to_string())?;
        store
            .transition_phase(execution_id, TxPhase::Preparing)
            .map_err(|err| err.to_string())?;
        let commit_key = test_commit_key(&contract, "step-0");
        let mut reservation = store
            .acquire_durable_reservation(execution_id, &commit_key, 5_500)
            .map_err(|err| err.to_string())?;
        store
            .record_execution_reserved(
                &mut reservation,
                execution_id,
                commit_key.clone(),
                StepOutcome::Pending,
                StepRisk::High,
                "agent-step-0",
                5_500,
            )
            .map_err(|err| err.to_string())?;
        drop(reservation);
        let original_spool = durable_ledger_file_snapshot(store_dir.path());

        let (executor, dispatched_steps) = CompensationRecordingExecutor::new(false);
        let error = TxExecutionEngine::new(executor, TxExecutionConfig::default())
            .rollback_with_store(&mut contract, &mut store, 6_000)
            .expect_err("ambiguous Pending commit proof must block rollback");

        assert!(matches!(
            error,
            TxExecutionError::RollbackProof {
                kind: RollbackProofKind::Conflict,
                ref step_id,
                ref detail,
            } if step_id == "step-0" && detail.contains("Pending")
        ));
        assert!(dispatched_steps.borrow().is_empty());
        assert_eq!(
            serde_json::to_value(&contract).map_err(|err| err.to_string())?,
            original_contract
        );
        assert!(matches!(
            store.peek_cached_outcome(&commit_key, 6_000),
            Some(StepOutcome::Pending)
        ));
        assert_eq!(store.active_count(), 1);
        assert_eq!(
            durable_ledger_file_snapshot(store_dir.path()),
            original_spool,
            "Pending proof conflict must preserve the exact source ledger bytes"
        );
        Ok(())
    }

    #[test]
    fn durable_rollback_rejects_pending_commit_hidden_by_failed_receipt_before_mutation()
    -> Result<(), String> {
        let mut contract = make_test_contract(1);
        TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default())
            .execute(&mut contract, 5_000)
            .map_err(|err| err.to_string())?;
        let receipt = contract
            .receipts
            .iter_mut()
            .find(|receipt| {
                receipt.get("phase").and_then(serde_json::Value::as_str) == Some("commit")
                    && receipt.get("step_id").and_then(serde_json::Value::as_str) == Some("step-0")
            })
            .expect("step-0 commit receipt");
        receipt["outcome"] = serde_json::json!("failed");
        receipt["reason_code"] = serde_json::json!("forged_commit_failure");
        receipt["error_code"] = serde_json::json!("FTX3999");
        receipt["decision_path"] = serde_json::json!("forged_receipt_history");
        let original_contract = serde_json::to_value(&contract).map_err(|err| err.to_string())?;

        let (_store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        let execution_id = "txe-hidden-pending-before-rollback";
        store
            .create_ledger(execution_id, &compiled_plan)
            .map_err(|err| err.to_string())?;
        store
            .transition_phase(execution_id, TxPhase::Preparing)
            .map_err(|err| err.to_string())?;
        let commit_key = test_commit_key(&contract, "step-0");
        let mut reservation = store
            .acquire_durable_reservation(execution_id, &commit_key, 5_500)
            .map_err(|err| err.to_string())?;
        store
            .record_execution_reserved(
                &mut reservation,
                execution_id,
                commit_key.clone(),
                StepOutcome::Pending,
                StepRisk::High,
                "agent-step-0",
                5_500,
            )
            .map_err(|err| err.to_string())?;
        drop(reservation);

        let (executor, dispatched_steps) = CompensationRecordingExecutor::new(false);
        let error = TxExecutionEngine::new(executor, TxExecutionConfig::default())
            .rollback_with_store(&mut contract, &mut store, 6_000)
            .expect_err("hidden Pending commit proof must block rollback");

        assert!(matches!(
            error,
            TxExecutionError::RollbackProof {
                kind: RollbackProofKind::Conflict,
                ref step_id,
                ref detail,
            } if step_id == "step-0"
                && detail.contains("does not identify a committed effect")
        ));
        assert!(dispatched_steps.borrow().is_empty());
        assert_eq!(
            serde_json::to_value(&contract).map_err(|err| err.to_string())?,
            original_contract
        );
        assert!(matches!(
            store.peek_cached_outcome(&commit_key, 6_000),
            Some(StepOutcome::Pending)
        ));
        assert_eq!(store.active_count(), 1);
        assert_eq!(
            store
                .get_ledger(execution_id)
                .expect("ambiguous source ledger must remain active")
                .phase(),
            TxPhase::Preparing
        );
        Ok(())
    }

    #[test]
    fn durable_rollback_rejects_pending_compensation_before_mutation() -> Result<(), String> {
        let (_store_dir, mut store) = durable_store();
        let mut contract = make_test_contract(1);
        TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default())
            .execute_with_store(&mut contract, &mut store, 5_000)
            .map_err(|err| err.to_string())?;
        let original_contract = serde_json::to_value(&contract).map_err(|err| err.to_string())?;

        let compiled_plan = compiled_plan_from_contract(&contract);
        let execution_id = "txe-pending-compensation-before-rollback";
        store
            .create_ledger(execution_id, &compiled_plan)
            .map_err(|err| err.to_string())?;
        store
            .transition_phase(execution_id, TxPhase::Preparing)
            .map_err(|err| err.to_string())?;
        let compensation_key = test_compensation_key(&contract, "step-0");
        let mut reservation = store
            .acquire_durable_reservation(execution_id, &compensation_key, 5_500)
            .map_err(|err| err.to_string())?;
        store
            .record_execution_reserved(
                &mut reservation,
                execution_id,
                compensation_key.clone(),
                StepOutcome::Pending,
                StepRisk::High,
                "agent-step-0",
                5_500,
            )
            .map_err(|err| err.to_string())?;
        drop(reservation);

        let (executor, dispatched_steps) = CompensationRecordingExecutor::new(false);
        let error = TxExecutionEngine::new(executor, TxExecutionConfig::default())
            .rollback_with_store(&mut contract, &mut store, 6_000)
            .expect_err("Pending compensation proof must block rollback before mutation");

        assert!(matches!(
            error,
            TxExecutionError::RollbackProof {
                kind: RollbackProofKind::Conflict,
                ref step_id,
                ref detail,
            } if step_id == "step-0"
                && detail.contains("durable compensation state Pending")
        ));
        assert!(dispatched_steps.borrow().is_empty());
        assert_eq!(
            serde_json::to_value(&contract).map_err(|err| err.to_string())?,
            original_contract
        );
        assert!(matches!(
            store.peek_cached_outcome(&compensation_key, 6_000),
            Some(StepOutcome::Pending)
        ));
        assert_eq!(store.active_count(), 1);
        assert_eq!(
            store
                .get_ledger(execution_id)
                .expect("ambiguous compensation ledger must remain active")
                .phase(),
            TxPhase::Preparing
        );
        Ok(())
    }

    #[test]
    fn durable_rollback_rejects_compensation_proof_with_mismatched_original_outcome()
    -> Result<(), String> {
        let (store_dir, mut store) = durable_store();
        let mut contract = make_test_contract(1);
        TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default())
            .execute_with_store(&mut contract, &mut store, 5_000)
            .map_err(|err| err.to_string())?;

        let compiled_plan = compiled_plan_from_contract(&contract);
        let execution_id = "txe-mismatched-compensation-proof";
        store
            .create_ledger(execution_id, &compiled_plan)
            .map_err(|err| err.to_string())?;
        for phase in [
            TxPhase::Preparing,
            TxPhase::Committing,
            TxPhase::Compensating,
        ] {
            store
                .transition_phase(execution_id, phase)
                .map_err(|err| err.to_string())?;
        }
        record_durable_test_outcome(
            &mut store,
            execution_id,
            test_compensation_key(&contract, "step-0"),
            StepOutcome::Compensated {
                original_outcome: Box::new(StepOutcome::Failed {
                    error_code: "forged_original".to_string(),
                    error_message: "commit never succeeded".to_string(),
                    compensated: false,
                }),
                compensation_result: "forged_undo".to_string(),
            },
            StepRisk::High,
            "agent-step-0",
            5_500,
        )?;
        let original_contract = serde_json::to_value(&contract).map_err(|err| err.to_string())?;
        let original_spool = durable_ledger_file_snapshot(store_dir.path());

        let (executor, dispatched_steps) = CompensationRecordingExecutor::new(false);
        let error = TxExecutionEngine::new(executor, TxExecutionConfig::default())
            .rollback_with_store(&mut contract, &mut store, 6_000)
            .expect_err("mismatched embedded original outcome must fail closed");

        assert!(matches!(
            error,
            TxExecutionError::RollbackProof {
                kind: RollbackProofKind::Conflict,
                ref step_id,
                ref detail,
            } if step_id == "step-0" && detail.contains("embeds original outcome")
        ));
        assert!(dispatched_steps.borrow().is_empty());
        assert_eq!(
            serde_json::to_value(&contract).map_err(|err| err.to_string())?,
            original_contract
        );
        assert_eq!(
            durable_ledger_file_snapshot(store_dir.path()),
            original_spool,
            "proof conflict must not mutate existing ledger evidence"
        );
        Ok(())
    }

    #[test]
    fn durable_rollback_rejects_missing_compensation_before_ledger_mutation() -> Result<(), String>
    {
        let (_store_dir, mut store) = durable_store();
        let mut contract = make_test_contract(1);
        contract.plan.compensations.clear();
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        engine
            .execute_with_store(&mut contract, &mut store, 5_000)
            .map_err(|err| err.to_string())?;
        let original_contract = serde_json::to_value(&contract).map_err(|err| err.to_string())?;

        let error = engine
            .rollback_with_store(&mut contract, &mut store, 6_000)
            .expect_err("missing compensation action must fail before rollback mutation");

        assert!(matches!(
            error,
            TxExecutionError::InvalidContract(ref message)
                if message.contains("no compensation action for committed step step-0")
        ));
        assert_eq!(
            serde_json::to_value(&contract).map_err(|err| err.to_string())?,
            original_contract
        );
        assert_eq!(store.active_count(), 0);
        Ok(())
    }

    #[test]
    fn forged_compensation_receipt_cannot_suppress_durable_undo() -> Result<(), String> {
        let (_store_dir, mut store) = durable_store();
        let mut contract = make_test_contract(1);
        let commit_engine =
            TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        commit_engine
            .execute_with_store(&mut contract, &mut store, 5_000)
            .map_err(|err| err.to_string())?;

        let mut receipt_donor = contract.clone();
        let forged = commit_engine
            .rollback(&mut receipt_donor, 6_000)
            .map_err(|err| err.to_string())?;
        let mut forged_contract = contract;
        forged_contract
            .receipts
            .extend(forged.compensation_report.receipts);

        let (executor, dispatched_steps) = CompensationRecordingExecutor::new(false);
        let rollback_engine = TxExecutionEngine::new(executor, TxExecutionConfig::default());
        let result = rollback_engine
            .rollback_with_store(&mut forged_contract, &mut store, 7_000)
            .map_err(|err| err.to_string())?;

        assert_eq!(dispatched_steps.borrow().as_slice(), ["step-0"]);
        assert!(result.compensation_report.is_fully_rolled_back());
        assert!(matches!(
            store.peek_cached_outcome(&test_compensation_key(&forged_contract, "step-0"), 7_000),
            Some(StepOutcome::Compensated { .. })
        ));
        Ok(())
    }

    #[test]
    fn failed_compensation_reuses_stable_key_and_sticky_success_survives_ttl() -> Result<(), String>
    {
        let store_dir = tempfile::tempdir().expect("durable tx store tempdir");
        let policy = IdempotencyPolicy {
            dedup_ttl_ms: 100,
            ..IdempotencyPolicy::default()
        };
        let mut store = IdempotencyStore::open(store_dir.path(), policy.clone())
            .map_err(|err| err.to_string())?;
        let mut contract = make_test_contract(2);
        let commit_engine =
            TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        commit_engine
            .execute_with_store(&mut contract, &mut store, 5_000)
            .map_err(|err| err.to_string())?;

        let (failing_executor, dispatched_steps) = CompensationRecordingExecutor::new(false);
        let failing_rollback = TxExecutionEngine::new(
            failing_executor,
            TxExecutionConfig {
                fail_compensation_for_step: Some("step-1".to_string()),
                ..TxExecutionConfig::default()
            },
        );
        let first = failing_rollback
            .rollback_with_store(&mut contract, &mut store, 6_000)
            .map_err(|err| err.to_string())?;
        assert_eq!(first.compensation_report.failed_count, 1);
        assert!(matches!(
            store.peek_cached_outcome(&test_compensation_key(&contract, "step-1"), 6_000),
            Some(StepOutcome::Failed { .. })
        ));
        assert!(
            store
                .peek_cached_outcome(&test_compensation_key(&contract, "step-0"), 6_000)
                .is_none(),
            "later compensation must not be reserved after the first failure"
        );
        let contract_before_successful_retry = contract.clone();
        drop(store);
        let mut store = IdempotencyStore::open(store_dir.path(), policy.clone())
            .map_err(|err| err.to_string())?;

        let (retry_executor, retry_dispatched_steps) = CompensationRecordingExecutor::new(false);
        let retry_engine = TxExecutionEngine::new(retry_executor, TxExecutionConfig::default());
        let retry = retry_engine
            .rollback_with_store(&mut contract, &mut store, 6_050)
            .map_err(|err| err.to_string())?;

        assert!(retry.compensation_report.is_fully_rolled_back());
        assert_eq!(retry.compensation_report.compensated_count, 2);
        assert_eq!(contract.lifecycle_state, MissionTxState::RolledBack);
        assert!(matches!(
            store.peek_cached_outcome(&test_compensation_key(&contract, "step-1"), 6_050),
            Some(StepOutcome::Compensated { .. })
        ));
        assert!(matches!(
            store.peek_cached_outcome(&test_compensation_key(&contract, "step-0"), 6_050),
            Some(StepOutcome::Compensated { .. })
        ));
        assert!(!retry.events.is_empty());
        assert_eq!(retry.execution_id, retry.ledger.execution_id());
        assert_eq!(dispatched_steps.borrow().as_slice(), ["step-1"]);
        assert_eq!(
            retry_dispatched_steps.borrow().as_slice(),
            ["step-1", "step-0"]
        );

        drop(store);
        let mut store =
            IdempotencyStore::open(store_dir.path(), policy).map_err(|err| err.to_string())?;
        contract = contract_before_successful_retry;
        let (recovery_executor, recovery_dispatched_steps) =
            CompensationRecordingExecutor::new(false);
        let recovery_engine =
            TxExecutionEngine::new(recovery_executor, TxExecutionConfig::default());
        let recovered = recovery_engine
            .rollback_with_store(&mut contract, &mut store, 7_000)
            .map_err(|err| err.to_string())?;

        assert!(recovered.compensation_report.is_fully_rolled_back());
        assert!(recovery_dispatched_steps.borrow().is_empty());
        assert!(matches!(
            store.peek_cached_outcome(&test_compensation_key(&contract, "step-1"), 7_000),
            Some(StepOutcome::Compensated { .. })
        ));
        Ok(())
    }

    #[test]
    fn deduped_compensation_recovers_one_missing_authoritative_receipt() -> Result<(), String> {
        let (store_dir, mut store) = durable_store();
        let mut contract = make_test_contract(1);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        engine
            .execute_with_store(&mut contract, &mut store, 5_000)
            .map_err(|err| err.to_string())?;
        let commit_report =
            mission_tx_rollback_commit_report(&contract, 6_000).map_err(|err| err.to_string())?;

        // Crash window: the compensation outcome reached the durable spool,
        // but the caller-owned contract did not receive its receipt.
        let compiled_plan = compiled_plan_from_contract(&contract);
        store
            .create_ledger("compensation-before-contract-save", &compiled_plan)
            .map_err(|err| err.to_string())?;
        for phase in [
            TxPhase::Preparing,
            TxPhase::Committing,
            TxPhase::Compensating,
        ] {
            store
                .transition_phase("compensation-before-contract-save", phase)
                .map_err(|err| err.to_string())?;
        }
        record_durable_test_outcome(
            &mut store,
            "compensation-before-contract-save",
            test_compensation_key(&contract, "step-0"),
            StepOutcome::Compensated {
                // Must be the outcome the engine would actually have written
                // at commit time: `record_commit_results_to_ledger` stores
                // `Success { result: Some(input.reason_code) }`, and the
                // reason code for a successful synthetic commit is
                // `commit_step_succeeded`. Seeding the *receipt* vocabulary
                // ("committed") describes a history no commit path can
                // produce, so the engine's exact cross-check against the
                // separately proven commit outcome correctly rejected it as a
                // rollback proof conflict. The check is load-bearing — a
                // durable compensation proof whose embedded original outcome
                // disagrees with the proven commit outcome means the ledger
                // and the contract describe different histories.
                original_outcome: Box::new(StepOutcome::Success {
                    result: Some("commit_step_succeeded".to_string()),
                }),
                compensation_result: "provider_specific_undo_receipt".to_string(),
            },
            StepRisk::High,
            "agent-step-0",
            6_000,
        )?;
        drop(store);
        let mut store = IdempotencyStore::open(store_dir.path(), IdempotencyPolicy::default())
            .map_err(|err| err.to_string())?;

        let recovered = engine
            .rollback_with_store(&mut contract, &mut store, 7_000)
            .map_err(|err| err.to_string())?;

        assert_eq!(recovered.compensation_report.receipts.len(), 1);
        assert_eq!(
            contract
                .receipts
                .iter()
                .filter(|receipt| {
                    receipt.get("phase").and_then(serde_json::Value::as_str) == Some("compensate")
                        && receipt.get("step_id").and_then(serde_json::Value::as_str)
                            == Some("step-0")
                        && receipt.get("outcome").and_then(serde_json::Value::as_str)
                            == Some("compensated")
                })
                .count(),
            1
        );

        // Reconstructing the same durable outcome against a contract that
        // already carries the equivalent latest receipt must not duplicate it.
        contract.lifecycle_state = MissionTxState::Compensating;
        contract.outcome = TxOutcome::Pending;
        let mut duplicate_commit_report = commit_report.clone();
        let (duplicate_original_proof, mut duplicate_proof_leases) =
            TxExecutionEngine::<SyntheticStepExecutor>::acquire_atomic_compensation_proof(
                &contract,
                &mut duplicate_commit_report,
                &mut store,
                8_000,
            )
            .map_err(|err| err.to_string())?;
        let duplicate_gate_report =
            TxExecutionEngine::<SyntheticStepExecutor>::compensation_gate_report(
                &contract,
                &duplicate_commit_report,
                Some(&duplicate_proof_leases),
            )
            .map_err(|err| err.to_string())?;
        let duplicate_permit = engine
            .validate_compensation_dispatch(
                &contract,
                &duplicate_gate_report,
                "receipt-recovery-probe",
                MissionKillSwitchLevel::Off,
                8_000,
            )
            .map_err(|err| err.to_string())?;
        store
            .create_ledger("receipt-recovery-probe", &compiled_plan)
            .map_err(|err| err.to_string())?;
        for phase in [
            TxPhase::Preparing,
            TxPhase::Committing,
            TxPhase::Compensating,
        ] {
            store
                .transition_phase("receipt-recovery-probe", phase)
                .map_err(|err| err.to_string())?;
        }
        let mut events = Vec::new();
        let mut decision_path = String::new();
        let (
            duplicate,
            duplicate_keys,
            duplicate_recovery_outcomes,
            duplicate_original_commit_outcomes,
        ) = engine
            .run_compensation_phase(
                &contract,
                &duplicate_commit_report,
                "receipt-recovery-probe",
                &mut events,
                &mut decision_path,
                duplicate_permit,
                Some(&mut store),
                Some(&duplicate_original_proof),
                Some(&mut duplicate_proof_leases),
                8_000,
            )
            .map_err(|err| err.to_string())?;
        assert!(duplicate.receipts.is_empty());

        let mut probe_ledger = TxExecutionLedger::new(
            "receipt-recovery-probe",
            &compiled_plan.plan_id,
            compiled_plan.plan_hash,
        );
        for phase in [
            TxPhase::Preparing,
            TxPhase::Committing,
            TxPhase::Compensating,
        ] {
            probe_ledger
                .transition_phase(phase)
                .map_err(|err| err.to_string())?;
        }
        engine
            .record_compensation_results_to_ledger(
                &contract,
                &duplicate,
                &duplicate_keys,
                &duplicate_original_commit_outcomes,
                TxLedgerRecordingContext {
                    execution_id: "receipt-recovery-probe",
                    ledger: &mut probe_ledger,
                    store: Some(&mut store),
                    authoritative_recovery_outcomes: Some(&duplicate_recovery_outcomes),
                    events: &mut events,
                    now_ms: 8_000,
                },
            )
            .map_err(|err| err.to_string())?;
        let compensation_key = duplicate_keys
            .get("step-0")
            .ok_or_else(|| "missing recovered compensation key".to_string())?;
        assert!(matches!(
            probe_ledger.get_outcome(compensation_key),
            Some(StepOutcome::Compensated { .. })
        ));
        assert!(matches!(
            store
                .get_ledger("receipt-recovery-probe")
                .and_then(|ledger| ledger.get_outcome(compensation_key)),
            Some(StepOutcome::Compensated {
                compensation_result,
                ..
            }) if compensation_result == "provider_specific_undo_receipt"
        ));
        Ok(())
    }

    #[test]
    fn rollback_safety_gates_prevent_compensation_dispatch() -> Result<(), String> {
        let mut committed = make_test_contract(2);
        TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default())
            .execute(&mut committed, 5_000)
            .map_err(|err| err.to_string())?;

        let cases = [
            (
                TxExecutionConfig {
                    kill_switch: MissionKillSwitchLevel::HardStop,
                    ..TxExecutionConfig::default()
                },
                false,
            ),
            (
                TxExecutionConfig {
                    paused: true,
                    ..TxExecutionConfig::default()
                },
                false,
            ),
            (
                TxExecutionConfig {
                    max_steps_per_batch: 1,
                    ..TxExecutionConfig::default()
                },
                false,
            ),
            (TxExecutionConfig::default(), true),
        ];

        for (config, deny_gates) in cases {
            let mut contract = committed.clone();
            let (executor, dispatched) = CompensationRecordingExecutor::new(deny_gates);
            let engine = TxExecutionEngine::new(executor, config);

            let err = engine.rollback(&mut contract, 6_000).unwrap_err();

            assert!(matches!(err, TxExecutionError::CompensationPhase(_)));
            assert!(
                dispatched.borrow().is_empty(),
                "rejected rollback safety gate dispatched compensation"
            );
        }
        Ok(())
    }

    #[test]
    fn rollback_proof_lease_errors_preserve_conflict_and_contention_semantics() {
        let conflict = classify_rollback_proof_lease_error(IdempotencyError::LedgerIndexCorrupt {
            reason: "two sticky outcomes disagree".to_string(),
        });
        assert!(matches!(
            conflict,
            TxExecutionError::RollbackProof {
                kind: RollbackProofKind::Conflict,
                ref step_id,
                ref detail,
            } if step_id == "durable-proof-set"
                && detail.contains("two sticky outcomes disagree")
                && detail.contains("reconcile")
        ));

        let contention =
            classify_rollback_proof_lease_error(IdempotencyError::ReservationInProgress {
                key: "plan:test-plan".to_string(),
            });
        assert!(matches!(
            contention,
            TxExecutionError::InProgress(ref detail)
                if detail.contains("plan:test-plan")
        ));
    }

    #[test]
    fn durable_rollback_gate_denial_preserves_contract_and_spool_exactly() -> Result<(), String> {
        let (store_dir, mut store) = durable_store();
        let mut contract = make_test_contract(2);
        TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default())
            .execute_with_store(&mut contract, &mut store, 5_000)
            .map_err(|err| err.to_string())?;
        let original_contract = serde_json::to_value(&contract).map_err(|err| err.to_string())?;
        let original_spool = durable_ledger_file_snapshot(store_dir.path());
        let (executor, gate_calls, dispatched) = CompensationGateAuditExecutor::new(true);
        let engine = TxExecutionEngine::new(executor, TxExecutionConfig::default());

        let error = engine
            .rollback_with_store(&mut contract, &mut store, 6_000)
            .expect_err("full compensation gate denial must stop before rollback mutation");

        assert!(matches!(error, TxExecutionError::CompensationPhase(_)));
        assert_eq!(*gate_calls.borrow(), 1);
        assert!(dispatched.borrow().is_empty());
        assert_eq!(
            serde_json::to_value(&contract).map_err(|err| err.to_string())?,
            original_contract
        );
        assert_eq!(
            durable_ledger_file_snapshot(store_dir.path()),
            original_spool,
            "gate denial must not create, retire, transition, or append a rollback ledger"
        );
        Ok(())
    }

    #[test]
    fn automatic_compensation_uses_exactly_one_gate_batch_and_monotonic_events() {
        let mut contract = make_test_contract(2);
        let (executor, gate_calls, dispatched) = CompensationGateAuditExecutor::new(false);
        let engine = TxExecutionEngine::new(
            executor,
            TxExecutionConfig {
                fail_step: Some("step-1".to_string()),
                ..TxExecutionConfig::default()
            },
        );

        let result = engine
            .execute(&mut contract, 5_000)
            .expect("approved automatic compensation succeeds");

        assert_eq!(*gate_calls.borrow(), 1);
        assert_eq!(dispatched.borrow().as_slice(), ["step-0"]);
        assert!(result.compensation_report.is_some());
        assert!(
            result
                .events
                .windows(2)
                .all(|pair| pair[0].sequence < pair[1].sequence)
        );
    }

    #[test]
    fn automatic_compensation_gate_denial_blocks_nondurable_dispatch() {
        let mut contract = make_test_contract(2);
        let (executor, gate_calls, dispatched) = CompensationGateAuditExecutor::new(true);
        let engine = TxExecutionEngine::new(
            executor,
            TxExecutionConfig {
                fail_step: Some("step-1".to_string()),
                ..TxExecutionConfig::default()
            },
        );

        let error = engine
            .execute(&mut contract, 5_000)
            .expect_err("denied automatic compensation must fail closed");

        assert!(matches!(error, TxExecutionError::CompensationPhase(_)));
        assert_eq!(*gate_calls.borrow(), 1);
        assert!(dispatched.borrow().is_empty());
        assert_eq!(contract.lifecycle_state, MissionTxState::Failed);
        assert_eq!(contract.outcome, TxOutcome::Failed);
        assert!(!contract.receipts.iter().any(|receipt| {
            receipt.get("phase").and_then(serde_json::Value::as_str) == Some("compensate")
        }));
    }

    #[test]
    fn automatic_compensation_gate_denial_blocks_durable_mutation() -> Result<(), String> {
        let (_store_dir, mut store) = durable_store();
        let mut contract = make_test_contract(2);
        let (executor, gate_calls, dispatched) = CompensationGateAuditExecutor::new(true);
        let engine = TxExecutionEngine::new(
            executor,
            TxExecutionConfig {
                fail_step: Some("step-1".to_string()),
                ..TxExecutionConfig::default()
            },
        );

        let error = engine
            .execute_with_store(&mut contract, &mut store, 5_000)
            .expect_err("denied durable automatic compensation must fail closed");

        assert!(matches!(error, TxExecutionError::CompensationPhase(_)));
        assert_eq!(*gate_calls.borrow(), 1);
        assert!(dispatched.borrow().is_empty());
        assert_eq!(contract.lifecycle_state, MissionTxState::Failed);
        assert_eq!(contract.outcome, TxOutcome::Failed);
        assert_eq!(store.active_count(), 1);
        assert!(
            store
                .peek_cached_outcome(&test_compensation_key(&contract, "step-0"), 5_000)
                .is_none(),
            "gate denial must not reserve a durable compensation key"
        );
        Ok(())
    }

    #[test]
    fn dedup_only_rollback_recovery_ignores_fresh_dispatch_gates() -> Result<(), String> {
        let (_store_dir, mut store) = durable_store();
        let mut contract = make_test_contract(1);
        TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default())
            .execute_with_store(&mut contract, &mut store, 5_000)
            .map_err(|err| err.to_string())?;
        let original_commit_outcome = store
            .peek_cached_outcome(&test_commit_key(&contract, "step-0"), 6_000)
            .cloned()
            .ok_or_else(|| "missing authoritative commit outcome".to_string())?;
        let compiled_plan = compiled_plan_from_contract(&contract);
        store
            .create_ledger("dedup-only-compensation-source", &compiled_plan)
            .map_err(|err| err.to_string())?;
        for phase in [
            TxPhase::Preparing,
            TxPhase::Committing,
            TxPhase::Compensating,
        ] {
            store
                .transition_phase("dedup-only-compensation-source", phase)
                .map_err(|err| err.to_string())?;
        }
        record_durable_test_outcome(
            &mut store,
            "dedup-only-compensation-source",
            test_compensation_key(&contract, "step-0"),
            StepOutcome::Compensated {
                original_outcome: Box::new(original_commit_outcome),
                compensation_result: "provider_undo_complete".to_string(),
            },
            StepRisk::High,
            "agent-step-0",
            6_000,
        )?;
        let engine = TxExecutionEngine::new(
            NoFreshCompensationExecutor,
            TxExecutionConfig {
                paused: true,
                max_steps_per_batch: 0,
                kill_switch: MissionKillSwitchLevel::HardStop,
                ..TxExecutionConfig::default()
            },
        );

        let result = engine
            .rollback_with_store(&mut contract, &mut store, 7_000)
            .map_err(|err| err.to_string())?;

        assert!(result.compensation_report.is_fully_rolled_back());
        assert!(contract.receipts.iter().any(|receipt| {
            receipt.get("phase").and_then(serde_json::Value::as_str) == Some("compensate")
                && receipt.get("step_id").and_then(serde_json::Value::as_str) == Some("step-0")
                && receipt.get("outcome").and_then(serde_json::Value::as_str) == Some("compensated")
        }));
        Ok(())
    }

    #[test]
    fn dedup_only_rollback_with_existing_receipt_needs_zero_sequence_headroom() -> Result<(), String>
    {
        let (_store_dir, mut store) = durable_store();
        let mut contract = make_test_contract(1);
        let synthetic = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        synthetic
            .execute_with_store(&mut contract, &mut store, 5_000)
            .map_err(|err| err.to_string())?;
        synthetic
            .rollback_with_store(&mut contract, &mut store, 6_000)
            .map_err(|err| err.to_string())?;
        let compensation_receipt = contract
            .receipts
            .iter_mut()
            .rev()
            .find(|receipt| {
                receipt.get("phase").and_then(serde_json::Value::as_str) == Some("compensate")
                    && receipt.get("step_id").and_then(serde_json::Value::as_str) == Some("step-0")
                    && receipt.get("outcome").and_then(serde_json::Value::as_str)
                        == Some("compensated")
            })
            .ok_or_else(|| "missing authoritative compensation receipt".to_string())?;
        compensation_receipt["seq"] = serde_json::json!(u64::MAX);
        contract.lifecycle_state = MissionTxState::Compensating;
        contract.outcome = TxOutcome::Pending;
        let receipt_count = contract.receipts.len();
        let engine = TxExecutionEngine::new(
            NoFreshCompensationExecutor,
            TxExecutionConfig {
                paused: true,
                max_steps_per_batch: 0,
                kill_switch: MissionKillSwitchLevel::HardStop,
                ..TxExecutionConfig::default()
            },
        );

        let result = engine
            .rollback_with_store(&mut contract, &mut store, 7_000)
            .map_err(|err| err.to_string())?;

        assert!(result.compensation_report.receipts.is_empty());
        assert_eq!(contract.receipts.len(), receipt_count);
        assert_eq!(
            contract
                .receipts
                .iter()
                .filter(|receipt| {
                    receipt.get("phase").and_then(serde_json::Value::as_str) == Some("compensate")
                        && receipt.get("step_id").and_then(serde_json::Value::as_str)
                            == Some("step-0")
                })
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn durable_rollback_rejects_oversize_compensation_batch_before_mutation() -> Result<(), String>
    {
        let (store_dir, mut store) = durable_store();
        let mut contract = make_test_contract(2);
        TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default())
            .execute_with_store(&mut contract, &mut store, 5_000)
            .map_err(|err| err.to_string())?;
        let original_contract = serde_json::to_value(&contract).map_err(|err| err.to_string())?;
        let original_spool = durable_ledger_file_snapshot(store_dir.path());

        let (executor, dispatched) = CompensationRecordingExecutor::new(false);
        let engine = TxExecutionEngine::new(
            executor,
            TxExecutionConfig {
                max_steps_per_batch: 1,
                ..TxExecutionConfig::default()
            },
        );
        let error = engine
            .rollback_with_store(&mut contract, &mut store, 6_000)
            .expect_err("durable rollback must enforce the full compensation batch limit");

        assert!(matches!(
            error,
            TxExecutionError::CompensationPhase(ref detail)
                if detail.contains("compensation batch has 2 steps")
        ));
        assert!(dispatched.borrow().is_empty());
        assert_eq!(
            serde_json::to_value(&contract).map_err(|err| err.to_string())?,
            original_contract,
            "batch rejection must precede caller-owned contract mutation"
        );
        assert_eq!(
            durable_ledger_file_snapshot(store_dir.path()),
            original_spool,
            "batch rejection must precede durable ledger mutation"
        );
        Ok(())
    }

    #[test]
    fn execute_with_failure_injection_triggers_compensation() {
        let mut contract = make_test_contract(3);
        let config = TxExecutionConfig {
            fail_step: Some("step-1".to_string()),
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert_eq!(result.final_state, MissionTxState::RolledBack);
        assert_eq!(result.outcome, TxOutcome::Compensated);
        assert!(result.compensation_report.is_some());
        let commit = result.commit_report.unwrap();
        assert!(commit.has_failures());
        assert_eq!(commit.committed_count, 1);
        assert_eq!(commit.failed_count, 1);
        assert_eq!(commit.skipped_count, 1);
    }

    #[test]
    fn execute_with_failure_at_first_step() {
        let mut contract = make_test_contract(3);
        let config = TxExecutionConfig {
            fail_step: Some("step-0".to_string()),
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert_eq!(result.final_state, MissionTxState::Compensated);
        assert_eq!(result.outcome, TxOutcome::Compensated);
        let comp = result.compensation_report.unwrap();
        assert_eq!(
            comp.outcome,
            crate::plan::TxCompensationOutcome::NothingToCompensate
        );
    }

    #[test]
    fn execute_with_compensation_failure() {
        let mut contract = make_test_contract(3);
        let config = TxExecutionConfig {
            fail_step: Some("step-2".to_string()),
            fail_compensation_for_step: Some("step-0".to_string()),
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert_eq!(result.final_state, MissionTxState::Failed);
        assert_eq!(result.outcome, TxOutcome::Failed);
        let comp = result.compensation_report.unwrap();
        assert!(comp.has_residual_risk());
    }

    #[test]
    fn execute_without_auto_compensate() {
        let mut contract = make_test_contract(3);
        let config = TxExecutionConfig {
            fail_step: Some("step-1".to_string()),
            auto_compensate: false,
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert_eq!(result.final_state, MissionTxState::Failed);
        assert_eq!(result.outcome, TxOutcome::Failed);
        assert!(result.compensation_report.is_none());
    }

    #[test]
    fn execute_with_kill_switch_blocks_at_prepare() {
        let mut contract = make_test_contract(2);
        let config = TxExecutionConfig {
            kill_switch: MissionKillSwitchLevel::HardStop,
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert!(!result.prepare_report.outcome.commit_eligible());
        assert!(result.commit_report.is_none());
    }

    #[test]
    fn economic_hard_stop_blocks_prepare_and_emits_audit_event() {
        let mut contract = make_test_contract(2);
        contract
            .attach_token_budget(MissionTokenBudget {
                max_tokens: 1_000,
                max_no_progress_tokens: 100,
            })
            .unwrap();
        let decision = contract
            .record_economic_usage(MissionTokenUsageSample {
                prompt_tokens: 75,
                output_tokens: 50,
                progress_delta: 0,
                observed_at_ms: 4_000,
            })
            .unwrap();
        assert!(matches!(
            decision,
            MissionEconomicBreakerDecision::HardStop { .. }
        ));

        let engine =
            TxExecutionEngine::new(CommitDispatchPanicExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5_000).unwrap();

        assert_eq!(result.final_state, MissionTxState::Failed);
        assert_eq!(result.outcome, TxOutcome::Failed);
        assert_eq!(result.prepare_report.outcome, TxPrepareOutcome::Denied);
        assert!(result.commit_report.is_none());
        assert!(result.decision_path.starts_with("economic_hard_stop->"));

        let event = result
            .events
            .iter()
            .find(|event| event.kind == TxEventKind::EconomicHardStop)
            .expect("economic hard-stop event should be emitted");
        assert_eq!(
            event.reason_code,
            crate::tx_observability::reason_codes::ECONOMIC_HARD_STOP
        );
        assert!(event.details.contains_key("envelope"));
        assert!(event.details.contains_key("audit_row"));
    }

    #[test]
    fn commit_safety_blocks_do_not_dispatch_steps() {
        let mut safe_mode_contract = make_test_contract(2);
        let safe_mode_engine = TxExecutionEngine::new(
            CommitDispatchPanicExecutor,
            TxExecutionConfig {
                kill_switch: MissionKillSwitchLevel::SafeMode,
                ..TxExecutionConfig::default()
            },
        );
        let safe_mode = safe_mode_engine
            .execute(&mut safe_mode_contract, 5000)
            .unwrap();
        let safe_mode_commit = safe_mode.commit_report.expect("commit report");
        assert_eq!(
            safe_mode_commit.outcome,
            crate::plan::TxCommitOutcome::KillSwitchBlocked
        );
        assert_eq!(safe_mode_commit.committed_count, 0);
        assert_eq!(safe_mode_commit.skipped_count, 2);
        assert!(safe_mode.compensation_report.is_none());
        assert_eq!(safe_mode.final_state, MissionTxState::Failed);
        assert_eq!(safe_mode.outcome, TxOutcome::Failed);

        let mut paused_contract = make_test_contract(2);
        let paused_engine = TxExecutionEngine::new(
            CommitDispatchPanicExecutor,
            TxExecutionConfig {
                paused: true,
                ..TxExecutionConfig::default()
            },
        );
        let paused = paused_engine.execute(&mut paused_contract, 5000).unwrap();
        let paused_commit = paused.commit_report.expect("commit report");
        assert_eq!(
            paused_commit.outcome,
            crate::plan::TxCommitOutcome::PauseSuspended
        );
        assert_eq!(paused_commit.committed_count, 0);
        assert_eq!(paused_commit.skipped_count, 2);
        assert!(paused.compensation_report.is_none());
        assert_eq!(paused.final_state, MissionTxState::Committing);
        assert_eq!(paused.outcome, TxOutcome::Pending);
    }

    #[test]
    fn unwired_safety_controls_max_steps_per_batch_blocks_commit_dispatch() {
        let mut oversized_contract = make_test_contract(2);
        let oversized_engine = TxExecutionEngine::new(
            CommitDispatchPanicExecutor,
            TxExecutionConfig {
                max_steps_per_batch: 1,
                ..TxExecutionConfig::default()
            },
        );
        let oversized = oversized_engine
            .execute(&mut oversized_contract, 5000)
            .unwrap();
        let oversized_commit = oversized.commit_report.expect("commit report");
        assert_eq!(
            oversized_commit.outcome,
            crate::plan::TxCommitOutcome::PauseSuspended
        );
        assert_eq!(oversized_commit.committed_count, 0);
        assert_eq!(oversized_commit.failed_count, 0);
        assert_eq!(oversized_commit.skipped_count, 2);
        assert!(oversized.compensation_report.is_none());
        assert_eq!(oversized.final_state, MissionTxState::Committing);
        assert_eq!(oversized.outcome, TxOutcome::Pending);

        let mut bounded_contract = make_test_contract(2);
        let bounded_engine = TxExecutionEngine::new(
            SyntheticStepExecutor,
            TxExecutionConfig {
                max_steps_per_batch: 2,
                ..TxExecutionConfig::default()
            },
        );
        let bounded = bounded_engine.execute(&mut bounded_contract, 5000).unwrap();
        assert_eq!(bounded.final_state, MissionTxState::Committed);
        assert_eq!(bounded.outcome, TxOutcome::Committed);
        assert_eq!(
            bounded
                .commit_report
                .expect("commit report")
                .committed_count,
            2,
        );
    }

    #[test]
    fn execute_with_pause_suspends_commit() {
        let mut contract = make_test_contract(2);
        let config = TxExecutionConfig {
            paused: true,
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let result = engine.execute(&mut contract, 5000).unwrap();

        let commit = result.commit_report.unwrap();
        assert_eq!(commit.outcome, crate::plan::TxCommitOutcome::PauseSuspended);
        assert_eq!(commit.skipped_count, 2);
    }

    #[test]
    fn execute_empty_contract_is_error() {
        let mut contract = MissionTxContract {
            tx_version: 1,
            intent: TxIntent {
                tx_id: TxId("tx-empty".to_string()),
                requested_by: MissionActorRole::Operator,
                summary: "Empty".to_string(),
                correlation_id: "corr-0".to_string(),
                created_at_ms: 0,
            },
            plan: ContractTxPlan {
                plan_id: TxPlanId("plan-empty".to_string()),
                tx_id: TxId("tx-empty".to_string()),
                steps: Vec::new(),
                preconditions: Vec::new(),
                compensations: Vec::new(),
            },
            lifecycle_state: MissionTxState::Planned,
            outcome: TxOutcome::Pending,
            receipts: Vec::new(),
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let err = engine.execute(&mut contract, 5000).unwrap_err();
        assert!(matches!(err, TxExecutionError::InvalidContract(_)));
    }

    #[test]
    fn execute_rejects_ambiguous_tx_contract_topology() {
        fn invalid_contract_message(err: TxExecutionError) -> Result<String, TxExecutionError> {
            match err {
                TxExecutionError::InvalidContract(message) => Ok(message),
                other => Err(other),
            }
        }

        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());

        let mut duplicate_step_id = make_test_contract(2);
        duplicate_step_id.plan.steps[1].step_id = duplicate_step_id.plan.steps[0].step_id.clone();
        let err = engine.execute(&mut duplicate_step_id, 5000).unwrap_err();
        let message = match invalid_contract_message(err) {
            Ok(message) => message,
            Err(other) => format!("unexpected error {other:?}"),
        };
        assert!(
            message.contains("duplicate step_id step-0"),
            "duplicate step ids must fail before commit dispatch"
        );

        let mut duplicate_ordinal = make_test_contract(2);
        duplicate_ordinal.plan.steps[1].ordinal = duplicate_ordinal.plan.steps[0].ordinal;
        assert!(
            duplicate_ordinal
                .validate()
                .unwrap_err()
                .contains("duplicate step ordinal 0"),
            "duplicate ordinals make replay order ambiguous"
        );

        let mut mismatched_tx = make_test_contract(1);
        mismatched_tx.plan.tx_id = TxId("tx-other".to_string());
        assert!(
            mismatched_tx
                .validate()
                .unwrap_err()
                .contains("does not match plan tx_id tx-other"),
            "intent and plan tx ids must identify the same transaction"
        );

        let mut unknown_compensation = make_test_contract(1);
        unknown_compensation
            .plan
            .compensations
            .push(TxCompensation {
                for_step_id: TxStepId("missing-step".to_string()),
                action: unknown_compensation.plan.steps[0].action.clone(),
            });
        assert!(
            unknown_compensation
                .validate()
                .unwrap_err()
                .contains("unknown step_id missing-step"),
            "compensations must target a concrete committed step"
        );

        let mut duplicate_compensation = make_test_contract(1);
        duplicate_compensation
            .plan
            .compensations
            .push(TxCompensation {
                for_step_id: duplicate_compensation.plan.steps[0].step_id.clone(),
                action: duplicate_compensation.plan.steps[0].action.clone(),
            });
        duplicate_compensation
            .plan
            .compensations
            .push(TxCompensation {
                for_step_id: duplicate_compensation.plan.steps[0].step_id.clone(),
                action: duplicate_compensation.plan.steps[0].action.clone(),
            });
        assert!(
            duplicate_compensation
                .validate()
                .unwrap_err()
                .contains("duplicate compensation for step_id step-0"),
            "duplicate compensation actions would make rollback dispatch ambiguous"
        );
    }

    #[test]
    fn events_emitted_for_all_phases() {
        let mut contract = make_test_contract(2);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        let event_kinds: Vec<_> = result.events.iter().map(|e| &e.kind).collect();
        assert!(event_kinds.contains(&&TxEventKind::PrepareStarted));
        assert!(event_kinds.contains(&&TxEventKind::PrepareCompleted));
        assert!(event_kinds.contains(&&TxEventKind::CommitStarted));
        assert!(event_kinds.contains(&&TxEventKind::CommitCompleted));
    }

    #[test]
    fn unwired_safety_controls_produce_forensic_bundle_respected() {
        let mut enabled_contract = make_test_contract(2);
        let enabled_engine =
            TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let enabled = enabled_engine.execute(&mut enabled_contract, 5000).unwrap();

        let bundle = enabled
            .forensic_bundle
            .as_ref()
            .expect("default config should produce forensic bundle");
        assert_eq!(bundle.metadata.generator, "tx_execution_engine");
        assert!(
            bundle
                .metadata
                .incident_id
                .starts_with("txe-00000000000000005000-")
        );
        assert!(bundle.metadata.incident_id.ends_with("-run"));
        assert_eq!(bundle.ledger.execution_id, enabled.ledger.execution_id());
        assert!(bundle.chain_verification.chain_intact);
        assert!(
            enabled
                .events
                .iter()
                .any(|event| event.kind == TxEventKind::BundleExported),
            "bundle export must be visible in the observability stream",
        );

        let mut disabled_contract = make_test_contract(1);
        let disabled_engine = TxExecutionEngine::new(
            SyntheticStepExecutor,
            TxExecutionConfig {
                produce_forensic_bundle: false,
                ..TxExecutionConfig::default()
            },
        );
        let disabled = disabled_engine
            .execute(&mut disabled_contract, 6000)
            .unwrap();
        assert!(disabled.forensic_bundle.is_none());
        assert!(
            disabled
                .events
                .iter()
                .all(|event| event.kind != TxEventKind::BundleExported),
            "disabled bundle production must not emit a bundle export event",
        );
    }

    #[test]
    fn execution_ledger_and_bundle_preserve_action_risk() {
        let mut contract = make_test_contract(3);
        contract.plan.steps[0].action = StepAction::MarkEventHandled { event_id: 42 };
        contract.plan.steps[1].action = StepAction::StoreData {
            key: "mission-metadata".to_string(),
            value: serde_json::json!({"status": "prepared"}),
        };

        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        let ledger_risks = result
            .ledger
            .records()
            .iter()
            .map(|record| record.risk)
            .collect::<Vec<_>>();
        assert_eq!(
            ledger_risks,
            vec![StepRisk::Low, StepRisk::Medium, StepRisk::High],
            "tx execution must not flatten all ledger records to low risk"
        );

        let bundle = result
            .forensic_bundle
            .as_ref()
            .expect("default execution exports forensic bundle");
        assert_eq!(
            bundle.plan.step_risks,
            vec![
                ("step-0".to_string(), StepRisk::Low),
                ("step-1".to_string(), StepRisk::Medium),
                ("step-2".to_string(), StepRisk::High),
            ]
        );
        assert_eq!(bundle.plan.high_risk_count, 1);
        assert_eq!(bundle.plan.critical_risk_count, 0);
        assert_eq!(bundle.plan.overall_risk, StepRisk::High);
    }

    #[test]
    fn prepare_gate_events_emitted_for_each_gate_check() {
        let mut contract = make_test_contract(2);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        let prepare_gate_events: Vec<_> = result
            .events
            .iter()
            .filter(|event| {
                matches!(
                    event.kind,
                    TxEventKind::PreconditionValidated | TxEventKind::PreconditionFailed
                )
            })
            .collect();

        assert_eq!(prepare_gate_events.len(), 10);
        assert!(
            prepare_gate_events
                .iter()
                .all(|event| event.details.contains_key("gate"))
        );
    }

    #[test]
    fn ledger_records_commit_steps() {
        let mut contract = make_test_contract(3);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert!(result.ledger.record_count() >= 3);
    }

    #[test]
    fn ledger_reaches_terminal_phase_on_success() {
        let mut contract = make_test_contract(1);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert!(result.ledger.phase().is_terminal());
    }

    #[test]
    fn record_commit_results_to_ledger_fails_closed_when_ledger_is_sealed() {
        let contract = make_test_contract(1);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let mut ledger = TxExecutionLedger::new("exec-1", &contract.plan.plan_id.0, 0);
        ledger
            .transition_phase(crate::tx_idempotency::TxPhase::Preparing)
            .unwrap();
        ledger
            .transition_phase(crate::tx_idempotency::TxPhase::Committing)
            .unwrap();
        ledger
            .transition_phase(crate::tx_idempotency::TxPhase::Completed)
            .unwrap();

        let commit_report = TxCommitReport {
            tx_id: contract.intent.tx_id.clone(),
            plan_id: contract.plan.plan_id.clone(),
            outcome: TxCommitOutcome::FullyCommitted,
            step_results: vec![crate::plan::TxCommitStepResult {
                step_id: contract.plan.steps[0].step_id.clone(),
                ordinal: contract.plan.steps[0].ordinal,
                outcome: crate::plan::TxCommitStepOutcome::Committed {
                    reason_code: "ok".to_string(),
                },
                decision_path: "test".to_string(),
                completed_at_ms: 1000,
            }],
            failure_boundary: None,
            committed_count: 1,
            failed_count: 0,
            skipped_count: 0,
            decision_path: "test".to_string(),
            reason_code: "ok".to_string(),
            error_code: None,
            completed_at_ms: 1000,
            receipts: Vec::new(),
        };
        let mut events = Vec::new();

        let err = engine
            .record_commit_results_to_ledger(
                &contract,
                &commit_report,
                TxLedgerRecordingContext {
                    execution_id: "exec-1",
                    ledger: &mut ledger,
                    store: None,
                    authoritative_recovery_outcomes: None,
                    events: &mut events,
                    now_ms: 1000,
                },
            )
            .unwrap_err();

        assert!(matches!(err, TxExecutionError::LedgerWrite(_)));
        assert!(err.to_string().contains("step-0"));
        assert!(events.is_empty());
    }

    #[test]
    fn record_compensation_results_to_ledger_fails_closed_when_ledger_is_sealed() {
        let contract = make_test_contract(1);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let mut ledger = TxExecutionLedger::new("exec-1", &contract.plan.plan_id.0, 0);
        ledger
            .transition_phase(crate::tx_idempotency::TxPhase::Preparing)
            .unwrap();
        ledger
            .transition_phase(crate::tx_idempotency::TxPhase::Committing)
            .unwrap();
        ledger
            .transition_phase(crate::tx_idempotency::TxPhase::Compensating)
            .unwrap();
        ledger
            .transition_phase(crate::tx_idempotency::TxPhase::Completed)
            .unwrap();

        let comp_report = crate::plan::TxCompensationReport {
            outcome: crate::plan::TxCompensationOutcome::FullyRolledBack,
            compensated_count: 1,
            failed_count: 0,
            no_compensation_count: 0,
            skipped_count: 0,
            step_results: Vec::new(),
            decision_path: "test".to_string(),
            reason_code: "rollback_complete".to_string(),
            error_code: None,
            completed_at_ms: 1000,
            receipts: vec![serde_json::json!({
                "step_id": contract.plan.steps[0].step_id.0,
                "outcome": "compensated"
            })],
        };
        let mut events = Vec::new();
        let compensation_key = compensation_idempotency_key(&contract, "step-0").unwrap();
        let compensation_keys = HashMap::from([("step-0".to_string(), compensation_key)]);
        let original_commit_outcomes = HashMap::from([(
            "step-0".to_string(),
            StepOutcome::Success {
                result: Some("ok".to_string()),
            },
        )]);

        let err = engine
            .record_compensation_results_to_ledger(
                &contract,
                &comp_report,
                &compensation_keys,
                &original_commit_outcomes,
                TxLedgerRecordingContext {
                    execution_id: "exec-1",
                    ledger: &mut ledger,
                    store: None,
                    authoritative_recovery_outcomes: None,
                    events: &mut events,
                    now_ms: 1000,
                },
            )
            .unwrap_err();

        assert!(matches!(err, TxExecutionError::LedgerWrite(_)));
        assert!(err.to_string().contains("step-0"));
        assert!(events.is_empty());
    }

    #[test]
    fn decision_path_traces_execution() {
        let mut contract = make_test_contract(2);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert!(result.decision_path.contains("prepare"));
        assert!(result.decision_path.contains("commit"));
        assert!(result.decision_path.contains("final"));
    }

    #[test]
    fn execution_config_serde_roundtrip() {
        let config = TxExecutionConfig {
            auto_compensate: false,
            produce_forensic_bundle: false,
            max_steps_per_batch: 50,
            kill_switch: MissionKillSwitchLevel::SafeMode,
            paused: true,
            fail_step: Some("s1".to_string()),
            fail_compensation_for_step: Some("s2".to_string()),
            observability: TxObservabilityConfig::default(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: TxExecutionConfig = serde_json::from_str(&json).unwrap();
        assert!(!back.auto_compensate);
        assert!(back.paused);
        assert_eq!(back.fail_step, Some("s1".to_string()));
    }

    #[test]
    fn execution_result_serde_roundtrip() {
        let mut contract = make_test_contract(1);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        let json = serde_json::to_string(&result).unwrap();
        let back: TxExecutionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.final_state, MissionTxState::Committed);
        assert_eq!(back.outcome, TxOutcome::Committed);
    }

    #[test]
    fn error_display_formats() {
        let errors = vec![
            TxExecutionError::InvalidContract("bad".to_string()),
            TxExecutionError::PhaseTransition("bad transition".to_string()),
            TxExecutionError::PreparePhase("failed".to_string()),
            TxExecutionError::CommitPhase("failed".to_string()),
            TxExecutionError::CompensationPhase("failed".to_string()),
            TxExecutionError::LedgerWrite("ledger sealed".to_string()),
            TxExecutionError::LedgerNotFound("id-1".to_string()),
        ];
        for err in &errors {
            let msg = err.to_string();
            assert_ne!(msg, "");
        }
    }

    struct DenyingExecutor;

    impl effect_seal::NonEffectful for DenyingExecutor {}

    impl StepExecutor for DenyingExecutor {
        fn evaluate_gates(
            &self,
            contract: &MissionTxContract,
            _now_ms: i64,
        ) -> Vec<TxPrepareGateInput> {
            contract
                .plan
                .steps
                .iter()
                .map(|step| TxPrepareGateInput {
                    step_id: step.step_id.clone(),
                    pane_id: Some(step.ordinal as u64),
                    preconditions_satisfied: true,
                    precondition_reason_code: None,
                    policy_passed: false,
                    policy_reason_code: Some("policy.denied".to_string()),
                    reservation_available: true,
                    reservation_reason_code: None,
                    approval_satisfied: true,
                    approval_reason_code: None,
                    target_liveness: true,
                    liveness_reason_code: None,
                    required_approval: None,
                })
                .collect()
        }

        fn execute_steps(
            &self,
            contract: &MissionTxContract,
            fail_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCommitStepInput> {
            crate::plan::mission_tx_commit_step_inputs(contract, fail_step, now_ms)
        }

        fn execute_compensations(
            &self,
            _contract: &MissionTxContract,
            commit_report: &TxCommitReport,
            fail_for_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCompensationStepInput> {
            crate::plan::mission_tx_compensation_inputs(commit_report, fail_for_step, now_ms)
        }
    }

    #[test]
    fn custom_executor_policy_denial_blocks_commit() {
        let mut contract = make_test_contract(2);
        let engine = TxExecutionEngine::new(DenyingExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert_eq!(result.prepare_report.outcome, TxPrepareOutcome::Denied);
        assert!(result.commit_report.is_none());
        assert_eq!(result.final_state, MissionTxState::Failed);
        assert_eq!(result.ledger.phase(), TxPhase::Aborted);
    }

    struct ApprovalBlockingExecutor;

    impl effect_seal::NonEffectful for ApprovalBlockingExecutor {}

    impl StepExecutor for ApprovalBlockingExecutor {
        fn evaluate_gates(
            &self,
            contract: &MissionTxContract,
            _now_ms: i64,
        ) -> Vec<TxPrepareGateInput> {
            contract
                .plan
                .steps
                .iter()
                .map(|step| TxPrepareGateInput {
                    step_id: step.step_id.clone(),
                    pane_id: Some(step.ordinal as u64),
                    preconditions_satisfied: true,
                    precondition_reason_code: None,
                    policy_passed: true,
                    policy_reason_code: None,
                    reservation_available: true,
                    reservation_reason_code: None,
                    approval_satisfied: false,
                    approval_reason_code: Some("policy.test.require_approval".to_string()),
                    target_liveness: true,
                    liveness_reason_code: None,
                    required_approval: Some(crate::plan::TxPrepareApprovalRequirement {
                        workspace_id: "workspace:test".to_string(),
                        action_kind: "send_text".to_string(),
                        pane_id: Some(step.ordinal as u64),
                        action_fingerprint: format!("sha256:step-{}", step.ordinal),
                        reason_code: Some("policy.test.require_approval".to_string()),
                    }),
                })
                .collect()
        }

        fn execute_steps(
            &self,
            contract: &MissionTxContract,
            fail_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCommitStepInput> {
            crate::plan::mission_tx_commit_step_inputs(contract, fail_step, now_ms)
        }

        fn execute_compensations(
            &self,
            _contract: &MissionTxContract,
            commit_report: &TxCommitReport,
            fail_for_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCompensationStepInput> {
            crate::plan::mission_tx_compensation_inputs(commit_report, fail_for_step, now_ms)
        }
    }

    #[test]
    fn custom_executor_require_approval_blocks_without_failing_tx() {
        let mut contract = make_test_contract(2);
        let engine = TxExecutionEngine::new(ApprovalBlockingExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert_eq!(
            result.prepare_report.outcome,
            TxPrepareOutcome::RequireApproval
        );
        assert!(result.commit_report.is_none());
        assert_eq!(result.final_state, MissionTxState::Planned);
        assert_eq!(result.outcome, TxOutcome::Pending);
    }

    #[test]
    fn reason_code_mapping() {
        assert_eq!(reason_code_for_outcome(&TxOutcome::Pending), "pending");
        assert_eq!(reason_code_for_outcome(&TxOutcome::Committed), "committed");
        assert_eq!(reason_code_for_outcome(&TxOutcome::Failed), "failed");
        assert_eq!(
            reason_code_for_outcome(&TxOutcome::Compensated),
            "compensated"
        );
    }

    #[test]
    fn synthetic_executor_implements_trait() {
        let executor = SyntheticStepExecutor;
        let contract = make_test_contract(2);
        let gates = executor.evaluate_gates(&contract, 5_000);
        assert_eq!(gates.len(), 2);
        assert!(gates[0].policy_passed);
        assert!(gates[0].target_liveness);
    }

    #[test]
    fn event_sequence_numbers_are_monotonic() {
        let mut contract = make_test_contract(2);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        for (i, event) in result.events.iter().enumerate() {
            if i > 0 {
                assert!(event.sequence > result.events[i - 1].sequence);
            }
        }
    }

    #[test]
    fn contract_state_updates_after_execution() {
        let mut contract = make_test_contract(2);
        assert_eq!(contract.lifecycle_state, MissionTxState::Planned);
        assert_eq!(contract.outcome, TxOutcome::Pending);

        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let _ = engine.execute(&mut contract, 5000).unwrap();

        assert_eq!(contract.lifecycle_state, MissionTxState::Committed);
        assert_eq!(contract.outcome, TxOutcome::Committed);
    }

    #[test]
    fn resume_with_no_step_activity_restarts_execution_safely() {
        let mut contract = make_test_contract(2);
        let (_store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        store.create_ledger("exec-1", &compiled_plan).unwrap();
        store
            .transition_phase("exec-1", TxPhase::Preparing)
            .unwrap();

        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine
            .resume(&mut contract, &mut store, "exec-1", 5000)
            .unwrap();

        assert_eq!(result.final_state, MissionTxState::Committed);
        assert_eq!(result.outcome, TxOutcome::Committed);
    }

    #[test]
    fn resume_rejects_contract_whose_immutable_plan_differs_from_ledger() {
        let original_contract = make_test_contract(1);
        let (_store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&original_contract);
        store
            .create_ledger("exec-identity", &compiled_plan)
            .unwrap();

        let mut supplied_contract = original_contract.clone();
        let StepAction::SendText { text, .. } = &mut supplied_contract.plan.steps[0].action else {
            panic!("test fixture must use send_text");
        };
        *text = "different external effect".to_string();
        let unchanged = supplied_contract.clone();

        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let error = engine
            .resume(&mut supplied_contract, &mut store, "exec-identity", 5_000)
            .unwrap_err();

        assert!(matches!(error, TxExecutionError::InvalidContract(_)));
        assert_eq!(
            serde_json::to_value(&supplied_contract).unwrap(),
            serde_json::to_value(&unchanged).unwrap()
        );
    }

    #[test]
    fn compiled_plan_hash_binds_immutable_contract_actions() {
        let original = make_test_contract(1);
        let mut changed = original.clone();
        let StepAction::SendText { text, .. } = &mut changed.plan.steps[0].action else {
            panic!("test fixture must use send_text");
        };
        *text = "different external effect".to_string();

        let original_hash = compiled_plan_from_contract(&original).plan_hash;
        let changed_hash = compiled_plan_from_contract(&changed).plan_hash;
        assert_ne!(original_hash, 0);
        assert_ne!(original_hash, changed_hash);
    }

    #[test]
    fn resume_blocks_partial_progress_without_checkpoint_replay_support() {
        let mut contract = make_test_contract(3);
        let (_store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        store.create_ledger("exec-1", &compiled_plan).unwrap();
        store
            .transition_phase("exec-1", TxPhase::Preparing)
            .unwrap();
        store
            .transition_phase("exec-1", TxPhase::Committing)
            .unwrap();
        record_durable_test_outcome(
            &mut store,
            "exec-1",
            commit_idempotency_key(&contract, "step-0").unwrap(),
            StepOutcome::Success {
                result: Some("ok".into()),
            },
            StepRisk::Low,
            "agent-step-0",
            1000,
        )
        .unwrap();

        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let err = engine
            .resume(&mut contract, &mut store, "exec-1", 5000)
            .unwrap_err();

        assert!(matches!(
            err,
            TxExecutionError::UnsafeResume {
                recommendation: ResumeRecommendation::ContinueFromCheckpoint,
                ..
            }
        ));
    }

    #[test]
    fn resume_already_complete_terminalizes_and_archives_nonterminal_ledger() {
        let mut contract = make_test_contract(1);
        let (_store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        store
            .create_ledger("exec-complete", &compiled_plan)
            .unwrap();
        store
            .transition_phase("exec-complete", TxPhase::Preparing)
            .unwrap();
        store
            .transition_phase("exec-complete", TxPhase::Committing)
            .unwrap();
        record_durable_test_outcome(
            &mut store,
            "exec-complete",
            test_commit_key(&contract, "step-0"),
            StepOutcome::Success {
                result: Some("committed_before_restart".to_string()),
            },
            StepRisk::High,
            "agent-step-0",
            1_000,
        )
        .unwrap();

        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine
            .resume(&mut contract, &mut store, "exec-complete", 5_000)
            .unwrap();

        assert_eq!(result.final_state, MissionTxState::Committed);
        assert_eq!(result.outcome, TxOutcome::Committed);
        assert_eq!(result.ledger.phase(), TxPhase::Completed);
        assert_eq!(store.active_count(), 0);
        assert!(store.get_ledger("exec-complete").is_none());
    }

    #[test]
    fn resume_paused_execution_remains_pending_instead_of_committed() {
        let mut contract = make_test_contract(2);
        contract.lifecycle_state = MissionTxState::Committing;
        contract.outcome = TxOutcome::Pending;

        let (_store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        store.create_ledger("exec-1", &compiled_plan).unwrap();
        store
            .transition_phase("exec-1", TxPhase::Preparing)
            .unwrap();
        store
            .transition_phase("exec-1", TxPhase::Committing)
            .unwrap();
        // Paused work was never dispatched, so it must not occupy the stable
        // side-effect namespace with ambiguous Skipped records. The empty
        // committing ledger is the durable checkpoint produced by the live
        // paused path and remains safely resumable.
        assert_eq!(store.get_ledger("exec-1").unwrap().record_count(), 0);

        let engine = TxExecutionEngine::new(
            SyntheticStepExecutor,
            TxExecutionConfig {
                paused: true,
                ..TxExecutionConfig::default()
            },
        );
        let result = engine
            .resume(&mut contract, &mut store, "exec-1", 5000)
            .unwrap();

        assert_eq!(result.final_state, MissionTxState::Committing);
        assert_eq!(result.outcome, TxOutcome::Pending);
    }

    #[test]
    fn resume_prefers_complete_compensation_coverage_over_historical_failure() {
        let mut contract = make_test_contract(2);
        contract.lifecycle_state = MissionTxState::Failed;
        contract.outcome = TxOutcome::Failed;

        let (_store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        store.create_ledger("exec-1", &compiled_plan).unwrap();
        store
            .transition_phase("exec-1", TxPhase::Preparing)
            .unwrap();
        store
            .transition_phase("exec-1", TxPhase::Committing)
            .unwrap();
        record_durable_test_outcome(
            &mut store,
            "exec-1",
            commit_idempotency_key(&contract, "step-0").unwrap(),
            StepOutcome::Success { result: None },
            StepRisk::Low,
            "agent-step-0",
            1000,
        )
        .unwrap();
        record_durable_test_outcome(
            &mut store,
            "exec-1",
            commit_idempotency_key(&contract, "step-1").unwrap(),
            StepOutcome::Failed {
                error_code: "FTX3999".into(),
                error_message: "commit failed".into(),
                compensated: false,
            },
            StepRisk::Low,
            "agent-step-1",
            1001,
        )
        .unwrap();
        store
            .transition_phase("exec-1", TxPhase::Compensating)
            .unwrap();
        record_durable_test_outcome(
            &mut store,
            "exec-1",
            compensation_idempotency_key(&contract, "step-0").unwrap(),
            StepOutcome::Compensated {
                original_outcome: Box::new(StepOutcome::Success { result: None }),
                compensation_result: "rollback_complete".into(),
            },
            StepRisk::Low,
            "agent-step-0",
            1002,
        )
        .unwrap();
        store
            .transition_phase("exec-1", TxPhase::Completed)
            .unwrap();

        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine
            .resume(&mut contract, &mut store, "exec-1", 5000)
            .unwrap();

        assert_eq!(result.final_state, MissionTxState::RolledBack);
        assert_eq!(result.outcome, TxOutcome::Compensated);
        assert_eq!(result.ledger.phase(), TxPhase::Completed);
        assert_eq!(store.active_count(), 0);
    }

    #[test]
    fn resume_preserves_compensated_terminal_state() {
        let mut contract = make_test_contract(1);
        contract.lifecycle_state = MissionTxState::Compensated;
        contract.outcome = TxOutcome::Compensated;

        let (_store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        store.create_ledger("exec-1", &compiled_plan).unwrap();
        store
            .transition_phase("exec-1", TxPhase::Preparing)
            .unwrap();
        store
            .transition_phase("exec-1", TxPhase::Committing)
            .unwrap();
        record_durable_test_outcome(
            &mut store,
            "exec-1",
            commit_idempotency_key(&contract, "step-0").unwrap(),
            StepOutcome::Failed {
                error_code: "FTX3999".into(),
                error_message: "commit failed before any side effects".into(),
                compensated: false,
            },
            StepRisk::Low,
            "agent-step-0",
            1000,
        )
        .unwrap();
        store
            .transition_phase("exec-1", TxPhase::Completed)
            .unwrap();

        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine
            .resume(&mut contract, &mut store, "exec-1", 5000)
            .unwrap();

        assert_eq!(result.final_state, MissionTxState::Compensated);
        assert_eq!(result.outcome, TxOutcome::Compensated);
    }

    // ── PaneStepExecutor tests ──────────────────────────────────────────────

    use crate::approval::ApprovalScope;
    use crate::plan::{
        TxPrepareApprovalChecker, TxPreparePolicyAuthorizer, TxPrepareTargetLookup,
        TxPrepareTargetSnapshot, WaitCondition,
    };
    use crate::policy::{PolicyDecision, PolicyInput};
    use crate::wezterm::{MockWezterm, WeztermHandle, mock_wezterm_handle};
    use std::sync::Arc;

    /// Allow-all policy authorizer for PaneStepExecutor tests.
    struct TestAllowAllPolicy;
    impl TxPreparePolicyAuthorizer for TestAllowAllPolicy {
        fn authorize_prepare(&self, _input: &PolicyInput) -> PolicyDecision {
            PolicyDecision::allow()
        }
    }

    /// Allow-all approval checker for PaneStepExecutor tests.
    struct TestAllowAllApprovals;
    impl TxPrepareApprovalChecker for TestAllowAllApprovals {
        fn has_active_approval(
            &self,
            _scope: &ApprovalScope,
            _now_ms: i64,
        ) -> std::result::Result<bool, String> {
            Ok(true)
        }
    }

    /// All-live target lookup for PaneStepExecutor tests.
    struct TestAllLiveTargets;
    impl TxPrepareTargetLookup for TestAllLiveTargets {
        fn lookup_target(
            &self,
            pane_id: u64,
        ) -> std::result::Result<Option<TxPrepareTargetSnapshot>, String> {
            Ok(Some(TxPrepareTargetSnapshot {
                pane_id,
                capabilities: Default::default(),
                last_seen_at_ms: Some(1000),
                observed: true,
                known_dead: false,
                domain: None,
                pane_title: None,
                pane_cwd: None,
                reserved_by: None,
                reservation_lookup_error: None,
            }))
        }
    }

    /// Build a contract with specific StepActions for PaneStepExecutor testing.
    fn make_pane_contract(actions: Vec<(String, StepAction)>) -> MissionTxContract {
        let steps = actions
            .into_iter()
            .enumerate()
            .map(|(i, (id, action))| TxStep {
                step_id: TxStepId(id),
                ordinal: i,
                action,
                description: format!("pane test step {i}"),
            })
            .collect();

        MissionTxContract {
            tx_version: 1,
            intent: TxIntent {
                tx_id: TxId("tx-pane-1".to_string()),
                requested_by: MissionActorRole::Operator,
                summary: "Pane step executor test".to_string(),
                correlation_id: "corr-pane-1".to_string(),
                created_at_ms: 1000,
            },
            plan: ContractTxPlan {
                plan_id: TxPlanId("plan-pane-1".to_string()),
                tx_id: TxId("tx-pane-1".to_string()),
                steps,
                preconditions: Vec::new(),
                compensations: Vec::new(),
            },
            lifecycle_state: MissionTxState::Planned,
            outcome: TxOutcome::Pending,
            receipts: Vec::new(),
        }
    }

    /// Create a PaneStepExecutor using allow-all test policy delegates.
    fn make_pane_executor(
        handle: WeztermHandle,
    ) -> PaneStepExecutor<TestAllowAllPolicy, TestAllowAllApprovals, TestAllLiveTargets> {
        PaneStepExecutor::new(
            handle,
            TestAllowAllPolicy,
            TestAllowAllApprovals,
            TestAllLiveTargets,
            TxPrepareEvaluationContext::new("test-workspace"),
        )
    }

    /// Create a PaneStepExecutor with custom config.
    fn make_pane_executor_with_config(
        handle: WeztermHandle,
        config: PaneStepExecutorConfig,
    ) -> PaneStepExecutor<TestAllowAllPolicy, TestAllowAllApprovals, TestAllLiveTargets> {
        PaneStepExecutor::new(
            handle,
            TestAllowAllPolicy,
            TestAllowAllApprovals,
            TestAllLiveTargets,
            TxPrepareEvaluationContext::new("test-workspace"),
        )
        .with_config(config)
    }

    /// Create a PaneStepExecutor with a fleet memory controller.
    fn make_pane_executor_with_controller(
        handle: WeztermHandle,
        controller: std::sync::Arc<crate::fleet_memory_controller::FleetMemoryController>,
    ) -> PaneStepExecutor<TestAllowAllPolicy, TestAllowAllApprovals, TestAllLiveTargets> {
        PaneStepExecutor::new(
            handle,
            TestAllowAllPolicy,
            TestAllowAllApprovals,
            TestAllLiveTargets,
            TxPrepareEvaluationContext::new("test-workspace"),
        )
        .with_fleet_controller(controller)
    }

    /// ft-3lqyu / ft-0rlfq.8: the real effect executor must be unable to
    /// dispatch without durable idempotency authority.
    ///
    /// The compile-time half is the effect seal — `PaneStepExecutor` has no
    /// `effect_seal::NonEffectful` impl, so `engine.execute(..)` /
    /// `engine.rollback(..)` do not resolve for it at all. (Uncommenting the
    /// `engine.execute` line below is a type error, not a runtime failure.)
    /// This test pins the runtime half: the only entrypoints that *do*
    /// resolve reject a non-durable spool before any pane is touched.
    #[test]
    fn pane_executor_cannot_dispatch_without_durable_authority() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async { mock.add_default_pane(0).await });

        let executor = make_pane_executor(mock.clone() as WeztermHandle);
        let engine = TxExecutionEngine::new(executor, TxExecutionConfig::default());
        let mut contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::SendText {
                pane_id: 0,
                text: "must-not-reach-the-pane".to_string(),
                paste_mode: None,
            },
        )]);

        // Compile-time seal (would not build):
        //     engine.execute(&mut contract, 5_000);

        let mut volatile = IdempotencyStore::new(IdempotencyPolicy::default());
        let err = engine
            .execute_with_store(&mut contract, &mut volatile, 5_000)
            .expect_err("a non-durable spool must not authorize pane dispatch");
        assert!(
            matches!(&err, TxExecutionError::InvalidContract(msg) if msg.contains("durable")),
            "unexpected error: {err:?}"
        );

        // MockWezterm::send_text echoes into the pane's content, so an empty
        // content proves no send ever reached the pane.
        let content = rt.block_on(async {
            mock.pane_state(0)
                .await
                .expect("pane 0 exists")
                .content
                .clone()
        });
        assert!(
            content.is_empty(),
            "no text may reach the pane when durable authority is absent; got {content:?}"
        );
        assert_eq!(contract.lifecycle_state, MissionTxState::Planned);
        assert!(contract.receipts.is_empty());
    }

    #[test]
    fn pane_executor_send_text_happy_path() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async { mock.add_default_pane(0).await });

        let executor = make_pane_executor(mock.clone() as WeztermHandle);
        let contract = make_pane_contract(vec![
            (
                "s1".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "hello".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s2".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "world".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s3".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "done".to_string(),
                    paste_mode: Some(false),
                },
            ),
        ]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 3);
        for r in &results {
            assert!(r.success, "step {} failed: {}", r.step_id.0, r.reason_code);
            assert_eq!(r.reason_code, "send_text_succeeded");
        }
    }

    #[test]
    fn pane_executor_send_text_pane_not_found() {
        let mock = Arc::new(MockWezterm::new());
        // No panes added — pane 99 doesn't exist
        let executor = make_pane_executor(mock.clone() as WeztermHandle);
        let contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::SendText {
                pane_id: 99,
                text: "oops".to_string(),
                paste_mode: None,
            },
        )]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "send_text_failed");
        assert!(results[0].error_code.is_some());
    }

    #[test]
    fn pane_executor_wait_for_pattern_fails_closed_until_registry_is_wired() {
        let executor = make_pane_executor(mock_wezterm_handle());
        let contract = make_pane_contract(vec![(
            "w1".to_string(),
            StepAction::WaitFor {
                pane_id: Some(0),
                condition: WaitCondition::Pattern {
                    pane_id: None,
                    rule_id: "READY".to_string(),
                },
                timeout_ms: 2000,
            },
        )]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "wait_for_pattern_registry_unwired");
        assert!(
            results[0]
                .error_code
                .as_deref()
                .is_some_and(|error| error.contains("FTX_WAIT_PATTERN_REGISTRY_UNWIRED"))
        );
    }

    #[test]
    fn pane_executor_wait_for_pane_idle_fails_closed_until_provider_is_wired() {
        let executor = make_pane_executor(mock_wezterm_handle());
        let contract = make_pane_contract(vec![(
            "w1".to_string(),
            StepAction::WaitFor {
                pane_id: Some(0),
                condition: WaitCondition::PaneIdle {
                    pane_id: None,
                    idle_threshold_ms: 500,
                },
                timeout_ms: 1_000,
            },
        )]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "wait_for_pane_idle_unwired");
        assert!(
            results[0]
                .error_code
                .as_deref()
                .is_some_and(|error| error.contains("FTX_WAIT_PANE_IDLE_UNWIRED"))
        );
    }

    #[test]
    fn pane_executor_wait_for_stable_tail_fails_closed_until_provider_is_wired() {
        let executor = make_pane_executor(mock_wezterm_handle());
        let contract = make_pane_contract(vec![(
            "w1".to_string(),
            StepAction::WaitFor {
                pane_id: Some(0),
                condition: WaitCondition::StableTail {
                    pane_id: None,
                    stable_for_ms: 500,
                },
                timeout_ms: 1_000,
            },
        )]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "wait_for_stable_tail_unwired");
        assert!(
            results[0]
                .error_code
                .as_deref()
                .is_some_and(|error| error.contains("FTX_WAIT_STABLE_TAIL_UNWIRED"))
        );
    }

    #[test]
    fn pane_executor_store_data_fails_closed_until_adapter_is_wired() {
        let mock = mock_wezterm_handle();
        let executor = make_pane_executor(mock);
        let contract = make_pane_contract(vec![(
            "sd1".to_string(),
            StepAction::StoreData {
                key: "test_key".to_string(),
                value: serde_json::json!({"value": 42}),
            },
        )]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "store_data_unwired");
        assert!(
            results[0]
                .error_code
                .as_deref()
                .is_some_and(|error| error.contains("FTX_STORE_DATA_UNWIRED"))
        );
    }

    #[test]
    fn pane_executor_unsupported_action_run_workflow() {
        let mock = mock_wezterm_handle();
        let executor = make_pane_executor(mock);
        let contract = make_pane_contract(vec![(
            "rw1".to_string(),
            StepAction::RunWorkflow {
                workflow_id: "test-wf".to_string(),
                params: None,
            },
        )]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "unsupported_action");
        assert!(
            results[0]
                .error_code
                .as_ref()
                .unwrap()
                .contains("RunWorkflow")
        );
    }

    #[test]
    fn pane_executor_validate_approval_fails_closed_until_wired() {
        let mock = mock_wezterm_handle();
        let executor = make_pane_executor(mock);
        let contract = make_pane_contract(vec![(
            "approval".to_string(),
            StepAction::ValidateApproval {
                approval_code: "ABC12345".to_string(),
            },
        )]);

        let results = executor.execute_steps(&contract, None, 5_000);

        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "validate_approval_unwired");
        let error = results[0].error_code.as_deref().expect("error code");
        assert!(error.contains("FTX_APPROVAL_UNWIRED"));
        assert!(
            !error.contains("ABC12345"),
            "approval code must not be copied into failure evidence"
        );
    }

    #[test]
    fn pane_executor_fail_step_injection() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
        });

        let executor = make_pane_executor(mock as WeztermHandle);
        let contract = make_pane_contract(vec![
            (
                "s1".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "a".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s2".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "b".to_string(),
                    paste_mode: None,
                },
            ),
        ]);
        // Inject failure at step s2
        let results = executor.execute_steps(&contract, Some("s2"), 5000);
        assert_eq!(results.len(), 2);
        assert!(results[0].success); // s1 succeeds
        assert!(!results[1].success); // s2 is injected failure
        assert_eq!(results[1].reason_code, "commit_step_failed_injected");
    }

    #[test]
    fn pane_executor_mixed_steps_failure_boundary() {
        let mock = Arc::new(MockWezterm::new());
        // No pane added — pane 0 will fail

        let executor = make_pane_executor(mock as WeztermHandle);
        let contract = make_pane_contract(vec![
            (
                "s1".to_string(),
                StepAction::StoreData {
                    key: "k1".to_string(),
                    value: serde_json::json!("v1"),
                },
            ),
            (
                "s2".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "fail".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s3".to_string(),
                StepAction::StoreData {
                    key: "k2".to_string(),
                    value: serde_json::json!("v2"),
                },
            ),
        ]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 3);
        assert!(!results[0].success); // StoreData fails closed before dispatch.
        assert_eq!(results[0].reason_code, "store_data_unwired");
        assert!(!results[1].success); // Skipped after the first failure.
        assert_eq!(results[1].reason_code, "skipped_after_failure");
        assert!(!results[2].success); // Skipped after failure
        assert_eq!(results[2].reason_code, "skipped_after_failure");
    }

    #[test]
    fn pane_executor_compensations_happy_path() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async { mock.add_default_pane(0).await });
        let executor = make_pane_executor(mock.clone() as WeztermHandle);
        let contract = make_pane_contract_with_compensations(
            vec![(
                "s1".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "forward".to_string(),
                    paste_mode: None,
                },
            )],
            vec![
                (
                    "s1".to_string(),
                    StepAction::SendText {
                        pane_id: 0,
                        text: "rollback-1".to_string(),
                        paste_mode: None,
                    },
                ),
                (
                    "s2".to_string(),
                    StepAction::SendText {
                        pane_id: 0,
                        text: "rollback-2".to_string(),
                        paste_mode: None,
                    },
                ),
            ],
        );

        // Create a commit report with 2 committed steps
        let commit_report = crate::plan::TxCommitReport {
            tx_id: TxId("tx-1".to_string()),
            plan_id: TxPlanId("plan-1".to_string()),
            outcome: crate::plan::TxCommitOutcome::PartialFailure,
            step_results: vec![
                crate::plan::TxCommitStepResult {
                    step_id: TxStepId("s1".to_string()),
                    ordinal: 0,
                    outcome: crate::plan::TxCommitStepOutcome::Committed {
                        reason_code: "ok".to_string(),
                    },
                    decision_path: "test".to_string(),
                    completed_at_ms: 1000,
                },
                crate::plan::TxCommitStepResult {
                    step_id: TxStepId("s2".to_string()),
                    ordinal: 1,
                    outcome: crate::plan::TxCommitStepOutcome::Committed {
                        reason_code: "ok".to_string(),
                    },
                    decision_path: "test".to_string(),
                    completed_at_ms: 2000,
                },
            ],
            failure_boundary: None,
            committed_count: 2,
            failed_count: 0,
            skipped_count: 0,
            decision_path: "test".to_string(),
            reason_code: "test".to_string(),
            error_code: None,
            completed_at_ms: 3000,
            receipts: Vec::new(),
        };
        let results = executor.execute_compensations(&contract, &commit_report, None, 5000);
        assert_eq!(results.len(), 2);
        assert!(results[0].success);
        assert!(results[1].success);
        assert_eq!(results[0].for_step_id.0, "s2");
        assert_eq!(results[1].for_step_id.0, "s1");
        assert_eq!(results[0].reason_code, "send_text_succeeded");
        rt.block_on(async {
            let pane = mock.pane_state(0).await.expect("pane should exist");
            assert_eq!(pane.content, "rollback-2rollback-1");
        });
    }

    #[test]
    fn pane_executor_compensations_with_failure_injection() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async { mock.add_default_pane(0).await });
        let executor = make_pane_executor(mock as WeztermHandle);
        let contract = make_pane_contract_with_compensations(
            vec![(
                "s1".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "forward".to_string(),
                    paste_mode: None,
                },
            )],
            vec![(
                "s1".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "rollback".to_string(),
                    paste_mode: None,
                },
            )],
        );

        let commit_report = crate::plan::TxCommitReport {
            tx_id: TxId("tx-1".to_string()),
            plan_id: TxPlanId("plan-1".to_string()),
            outcome: crate::plan::TxCommitOutcome::PartialFailure,
            step_results: vec![crate::plan::TxCommitStepResult {
                step_id: TxStepId("s1".to_string()),
                ordinal: 0,
                outcome: crate::plan::TxCommitStepOutcome::Committed {
                    reason_code: "ok".to_string(),
                },
                decision_path: "test".to_string(),
                completed_at_ms: 1000,
            }],
            failure_boundary: None,
            committed_count: 1,
            failed_count: 0,
            skipped_count: 0,
            decision_path: "test".to_string(),
            reason_code: "test".to_string(),
            error_code: None,
            completed_at_ms: 2000,
            receipts: Vec::new(),
        };
        let results = executor.execute_compensations(&contract, &commit_report, Some("s1"), 5000);
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "compensation_failed_injected");
    }

    #[test]
    fn pane_executor_compensations_empty() {
        let mock = mock_wezterm_handle();
        let executor = make_pane_executor(mock);
        let contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::StoreData {
                key: "k".to_string(),
                value: serde_json::json!("v"),
            },
        )]);

        let commit_report = crate::plan::TxCommitReport {
            tx_id: TxId("tx-1".to_string()),
            plan_id: TxPlanId("plan-1".to_string()),
            outcome: crate::plan::TxCommitOutcome::ImmediateFailure,
            step_results: vec![crate::plan::TxCommitStepResult {
                step_id: TxStepId("s1".to_string()),
                ordinal: 0,
                outcome: crate::plan::TxCommitStepOutcome::Failed {
                    reason_code: "err".to_string(),
                },
                decision_path: "test".to_string(),
                completed_at_ms: 1000,
            }],
            failure_boundary: None,
            committed_count: 0,
            failed_count: 1,
            skipped_count: 0,
            decision_path: "test".to_string(),
            reason_code: "test".to_string(),
            error_code: None,
            completed_at_ms: 2000,
            receipts: Vec::new(),
        };
        let results = executor.execute_compensations(&contract, &commit_report, None, 5000);
        // No committed steps → no compensations
        assert!(results.is_empty());
    }

    #[test]
    fn pane_executor_compensations_fail_closed_when_action_missing() {
        let mock = mock_wezterm_handle();
        let executor = make_pane_executor(mock);
        let contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::StoreData {
                key: "k".to_string(),
                value: serde_json::json!("v"),
            },
        )]);

        let commit_report = crate::plan::TxCommitReport {
            tx_id: TxId("tx-1".to_string()),
            plan_id: TxPlanId("plan-1".to_string()),
            outcome: crate::plan::TxCommitOutcome::PartialFailure,
            step_results: vec![crate::plan::TxCommitStepResult {
                step_id: TxStepId("s1".to_string()),
                ordinal: 0,
                outcome: crate::plan::TxCommitStepOutcome::Committed {
                    reason_code: "ok".to_string(),
                },
                decision_path: "test".to_string(),
                completed_at_ms: 1000,
            }],
            failure_boundary: None,
            committed_count: 1,
            failed_count: 0,
            skipped_count: 0,
            decision_path: "test".to_string(),
            reason_code: "test".to_string(),
            error_code: None,
            completed_at_ms: 2000,
            receipts: Vec::new(),
        };

        let results = executor.execute_compensations(&contract, &commit_report, None, 5000);
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "compensation_action_missing");
        assert_eq!(
            results[0].error_code.as_deref(),
            Some("FTX_COMPENSATION_MISSING")
        );
    }

    #[test]
    fn pane_executor_evaluate_gates_delegates() {
        let mock = mock_wezterm_handle();
        let executor = make_pane_executor(mock);
        let contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::SendText {
                pane_id: 0,
                text: "test".to_string(),
                paste_mode: None,
            },
        )]);
        let gates = executor.evaluate_gates(&contract, 5000);
        assert_eq!(gates.len(), 1);
        // Allow-all policy: all gates should pass
        assert!(gates[0].policy_passed);
        assert!(gates[0].approval_satisfied);
        assert!(gates[0].reservation_available);
        assert!(gates[0].target_liveness);
    }

    #[test]
    fn pane_executor_unwired_lock_and_event_actions_fail_closed() {
        let mock = mock_wezterm_handle();
        let cases = [
            (
                StepAction::AcquireLock {
                    lock_name: "test-lock".to_string(),
                    timeout_ms: Some(5000),
                },
                "acquire_lock_unwired",
                "FTX_ACQUIRE_LOCK_UNWIRED",
            ),
            (
                StepAction::MarkEventHandled { event_id: 42 },
                "mark_event_handled_unwired",
                "FTX_MARK_EVENT_HANDLED_UNWIRED",
            ),
            (
                StepAction::ReleaseLock {
                    lock_name: "test-lock".to_string(),
                },
                "release_lock_unwired",
                "FTX_RELEASE_LOCK_UNWIRED",
            ),
        ];

        for (action, expected_reason, expected_error) in cases {
            let result = execute_step_action(&mock, &action, None, None, None);
            assert!(!result.0, "{expected_reason} must fail closed");
            assert_eq!(result.1, expected_reason);
            assert!(
                result
                    .2
                    .as_deref()
                    .is_some_and(|error| error.contains(expected_error)),
                "{expected_reason} must expose its stable error code"
            );
        }
    }

    // ── Timeout and backpressure tests (ft-y9lnb.4) ────────────────────

    #[test]
    fn pane_executor_external_wait_timeout_skips_remaining() {
        let registry = Arc::new(crate::workflows::ExternalSignalRegistry::new());

        // Keep this as a real timeout-path test: External is the one WaitFor
        // condition with a wired provider. Pattern, PaneIdle, and StableTail
        // now fail closed immediately until their respective providers exist.
        let config = PaneStepExecutorConfig {
            default_send_timeout_ms: 30_000,
            phase_timeout_buffer_ms: 0,
            backpressure_enabled: false,
        };
        let executor = make_pane_executor_with_config(mock_wezterm_handle(), config)
            .with_external_signals(registry);

        let contract = make_pane_contract(vec![
            (
                "w1".to_string(),
                StepAction::WaitFor {
                    pane_id: None,
                    condition: WaitCondition::External {
                        key: "never-fired".to_string(),
                    },
                    timeout_ms: 120,
                },
            ),
            (
                "s2".to_string(),
                StepAction::StoreData {
                    key: "k2".to_string(),
                    value: serde_json::json!("v2"),
                },
            ),
        ]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 2);
        // First step times out because the external signal never fires.
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "wait_for_timeout");
        // Second step is skipped after failure boundary
        assert!(!results[1].success);
        assert_eq!(results[1].reason_code, "skipped_after_failure");
    }

    #[test]
    fn pane_executor_default_send_timeout_config() {
        let mock = mock_wezterm_handle();
        let config = PaneStepExecutorConfig {
            default_send_timeout_ms: 5000,
            phase_timeout_buffer_ms: 60_000,
            backpressure_enabled: false,
        };
        let executor = make_pane_executor_with_config(mock, config);
        assert_eq!(executor.config.default_send_timeout_ms, 5_000);
    }

    #[test]
    fn pane_executor_backpressure_normal_proceeds() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async { mock.add_default_pane(0).await });
        let controller =
            std::sync::Arc::new(crate::fleet_memory_controller::FleetMemoryController::default());
        // Default controller is Normal tier
        let executor =
            make_pane_executor_with_controller(mock.clone() as WeztermHandle, controller);
        let contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::SendText {
                pane_id: 0,
                text: "normal-tier-send".to_string(),
                paste_mode: None,
            },
        )]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 1);
        assert!(results[0].success);
    }

    #[test]
    fn pane_executor_backpressure_emergency_defers_all() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
        });

        let mut controller = crate::fleet_memory_controller::FleetMemoryController::new(
            crate::fleet_memory_controller::FleetMemoryConfig {
                escalation_threshold: 1,
                deescalation_threshold: 1,
                ..Default::default()
            },
        );
        // Push to Emergency via black signals
        let emergency_signals = crate::fleet_memory_controller::PressureSignals {
            backpressure: crate::backpressure::BackpressureTier::Black,
            memory_pressure: crate::memory_pressure::MemoryPressureTier::Red,
            worst_budget: crate::memory_budget::BudgetLevel::OverBudget,
            pane_count: 200,
            paused_pane_count: 100,
        };
        controller.evaluate(&emergency_signals);
        assert_eq!(
            controller.compound_tier(),
            crate::fleet_memory_controller::FleetPressureTier::Emergency
        );

        let controller = std::sync::Arc::new(controller);
        let executor = make_pane_executor_with_controller(mock as WeztermHandle, controller);

        let contract = make_pane_contract(vec![
            (
                "s1".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "hello".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s2".to_string(),
                StepAction::StoreData {
                    key: "k".to_string(),
                    value: serde_json::json!("v"),
                },
            ),
        ]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 2);
        // All steps deferred under emergency
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "backpressure_emergency");
        assert!(!results[1].success);
        assert_eq!(results[1].reason_code, "skipped_after_failure");
    }

    #[test]
    fn pane_executor_backpressure_critical_defers_non_pane() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
        });

        let mut controller = crate::fleet_memory_controller::FleetMemoryController::new(
            crate::fleet_memory_controller::FleetMemoryConfig {
                escalation_threshold: 1,
                deescalation_threshold: 1,
                ..Default::default()
            },
        );
        // Push to Critical via red signals
        let critical_signals = crate::fleet_memory_controller::PressureSignals {
            backpressure: crate::backpressure::BackpressureTier::Red,
            memory_pressure: crate::memory_pressure::MemoryPressureTier::Orange,
            worst_budget: crate::memory_budget::BudgetLevel::Throttled,
            pane_count: 200,
            paused_pane_count: 10,
        };
        controller.evaluate(&critical_signals);
        assert_eq!(
            controller.compound_tier(),
            crate::fleet_memory_controller::FleetPressureTier::Critical
        );

        let controller = std::sync::Arc::new(controller);
        let executor = make_pane_executor_with_controller(mock as WeztermHandle, controller);

        // StoreData (no pane) first, then SendText (has pane)
        let contract = make_pane_contract(vec![
            (
                "s1".to_string(),
                StepAction::StoreData {
                    key: "k".to_string(),
                    value: serde_json::json!("v"),
                },
            ),
            (
                "s2".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "hello".to_string(),
                    paste_mode: None,
                },
            ),
        ]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 2);
        // StoreData deferred (no pane, Critical)
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "backpressure_deferred");
        // SendText skipped after failure
        assert!(!results[1].success);
        assert_eq!(results[1].reason_code, "skipped_after_failure");
    }

    #[test]
    fn pane_executor_backpressure_disabled_ignores_controller() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async { mock.add_default_pane(0).await });

        let mut controller = crate::fleet_memory_controller::FleetMemoryController::new(
            crate::fleet_memory_controller::FleetMemoryConfig {
                escalation_threshold: 1,
                deescalation_threshold: 1,
                ..Default::default()
            },
        );
        let emergency_signals = crate::fleet_memory_controller::PressureSignals {
            backpressure: crate::backpressure::BackpressureTier::Black,
            memory_pressure: crate::memory_pressure::MemoryPressureTier::Red,
            worst_budget: crate::memory_budget::BudgetLevel::OverBudget,
            pane_count: 200,
            paused_pane_count: 100,
        };
        controller.evaluate(&emergency_signals);

        let config = PaneStepExecutorConfig {
            default_send_timeout_ms: 30_000,
            phase_timeout_buffer_ms: 30_000,
            backpressure_enabled: false, // Disabled!
        };
        let executor = PaneStepExecutor::new(
            mock.clone() as WeztermHandle,
            TestAllowAllPolicy,
            TestAllowAllApprovals,
            TestAllLiveTargets,
            TxPrepareEvaluationContext::new("test-workspace"),
        )
        .with_config(config)
        .with_fleet_controller(std::sync::Arc::new(controller));

        let contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::SendText {
                pane_id: 0,
                text: "backpressure-disabled-send".to_string(),
                paste_mode: None,
            },
        )]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 1);
        // Backpressure disabled — the supported SendText step succeeds despite Emergency tier.
        assert!(results[0].success);
    }

    #[test]
    fn pane_executor_step_timeout_helper() {
        assert_eq!(
            step_timeout_ms(
                &StepAction::SendText {
                    pane_id: 0,
                    text: "test".to_string(),
                    paste_mode: None,
                },
                30_000,
            ),
            Some(30_000),
        );
        assert_eq!(
            step_timeout_ms(
                &StepAction::WaitFor {
                    pane_id: Some(0),
                    condition: WaitCondition::Pattern {
                        pane_id: None,
                        rule_id: "test".to_string(),
                    },
                    timeout_ms: 5000,
                },
                30_000,
            ),
            Some(5000),
        );
        assert_eq!(
            step_timeout_ms(
                &StepAction::StoreData {
                    key: "k".to_string(),
                    value: serde_json::json!("v"),
                },
                30_000,
            ),
            None,
        );
    }

    #[test]
    fn pane_executor_phase_timeout_budget_saturates_on_overflow() {
        let steps = vec![
            TxStep {
                step_id: TxStepId("send".to_string()),
                ordinal: 0,
                action: StepAction::SendText {
                    pane_id: 0,
                    text: "test".to_string(),
                    paste_mode: None,
                },
                description: "send with default timeout".to_string(),
            },
            TxStep {
                step_id: TxStepId("wait".to_string()),
                ordinal: 1,
                action: StepAction::WaitFor {
                    pane_id: Some(0),
                    condition: WaitCondition::Pattern {
                        pane_id: None,
                        rule_id: "test".to_string(),
                    },
                    timeout_ms: u64::MAX,
                },
                description: "wait with huge timeout".to_string(),
            },
        ];

        assert_eq!(
            phase_timeout_budget_ms(&steps, u64::MAX, u64::MAX),
            u64::MAX
        );
    }

    #[test]
    fn pane_executor_action_has_pane_helper() {
        assert!(action_has_pane(&StepAction::SendText {
            pane_id: 0,
            text: "test".to_string(),
            paste_mode: None,
        }));
        assert!(action_has_pane(&StepAction::WaitFor {
            pane_id: Some(0),
            condition: WaitCondition::Pattern {
                pane_id: None,
                rule_id: "test".to_string(),
            },
            timeout_ms: 5000,
        }));
        assert!(!action_has_pane(&StepAction::StoreData {
            key: "k".to_string(),
            value: serde_json::json!("v"),
        }));
        assert!(!action_has_pane(&StepAction::AcquireLock {
            lock_name: "lock".to_string(),
            timeout_ms: None,
        }));
    }

    // ── Integration tests: TxExecutionEngine<PaneStepExecutor> (ft-y9lnb.5) ─

    /// Create a PaneStepExecutor-powered engine with allow-all policy.
    fn make_pane_engine(
        handle: WeztermHandle,
    ) -> TxExecutionEngine<
        PaneStepExecutor<TestAllowAllPolicy, TestAllowAllApprovals, TestAllLiveTargets>,
    > {
        let executor = make_pane_executor(handle);
        TxExecutionEngine::new(executor, TxExecutionConfig::default())
    }

    /// Create a contract with compensations for rollback testing.
    fn make_pane_contract_with_compensations(
        steps: Vec<(String, StepAction)>,
        compensations: Vec<(String, StepAction)>,
    ) -> MissionTxContract {
        let step_entries: Vec<TxStep> = steps
            .into_iter()
            .enumerate()
            .map(|(i, (id, action))| TxStep {
                step_id: TxStepId(id),
                ordinal: i,
                action,
                description: format!("step {i}"),
            })
            .collect();

        let comp_entries: Vec<crate::plan::TxCompensation> = compensations
            .into_iter()
            .map(|(for_id, action)| crate::plan::TxCompensation {
                for_step_id: TxStepId(for_id),
                action,
            })
            .collect();

        MissionTxContract {
            tx_version: 1,
            intent: TxIntent {
                tx_id: TxId("tx-integ-1".to_string()),
                requested_by: MissionActorRole::Operator,
                summary: "Integration test tx".to_string(),
                correlation_id: "corr-integ-1".to_string(),
                created_at_ms: 1000,
            },
            plan: ContractTxPlan {
                plan_id: TxPlanId("plan-integ-1".to_string()),
                tx_id: TxId("tx-integ-1".to_string()),
                steps: step_entries,
                preconditions: Vec::new(),
                compensations: comp_entries,
            },
            lifecycle_state: MissionTxState::Planned,
            outcome: TxOutcome::Pending,
            receipts: Vec::new(),
        }
    }

    #[test]
    fn integration_happy_path_3_steps() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
            mock.add_default_pane(1).await;
            mock.add_default_pane(2).await;
        });

        let engine = make_pane_engine(mock as WeztermHandle);
        let mut contract = make_pane_contract(vec![
            (
                "s1".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "hello".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s2".to_string(),
                StepAction::SendText {
                    pane_id: 1,
                    text: "world".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s3".to_string(),
                StepAction::SendText {
                    pane_id: 2,
                    text: "done".to_string(),
                    paste_mode: None,
                },
            ),
        ]);

        let result = execute_durable(&engine, &mut contract, 5000).unwrap();
        assert_eq!(result.final_state, MissionTxState::Committed);
        assert_eq!(result.outcome, TxOutcome::Committed);
        let commit = result.commit_report.unwrap();
        assert_eq!(commit.committed_count, 3);
        assert_eq!(commit.failed_count, 0);
        assert!(result.compensation_report.is_none());
        assert!(!result.events.is_empty());
    }

    #[test]
    fn integration_single_step_minimal() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
        });

        let engine = make_pane_engine(mock as WeztermHandle);
        let mut contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::SendText {
                pane_id: 0,
                text: "single".to_string(),
                paste_mode: None,
            },
        )]);

        let result = execute_durable(&engine, &mut contract, 5000).unwrap();
        assert_eq!(result.final_state, MissionTxState::Committed);
        assert_eq!(result.outcome, TxOutcome::Committed);
    }

    #[test]
    fn integration_pane_not_found_triggers_compensation() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
            // Pane 99 is NOT added
        });

        let config = TxExecutionConfig {
            auto_compensate: true,
            ..Default::default()
        };

        let executor = make_pane_executor(mock.clone() as WeztermHandle);
        let engine = TxExecutionEngine::new(executor, config);

        let mut contract = make_pane_contract_with_compensations(
            vec![
                (
                    "s1".to_string(),
                    StepAction::SendText {
                        pane_id: 0,
                        text: "ok".to_string(),
                        paste_mode: None,
                    },
                ),
                (
                    "s2".to_string(),
                    StepAction::SendText {
                        pane_id: 99,
                        text: "fail".to_string(),
                        paste_mode: None,
                    },
                ),
            ],
            vec![(
                "s1".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "ROLLBACK".to_string(),
                    paste_mode: None,
                },
            )],
        );

        let result = execute_durable(&engine, &mut contract, 5000).unwrap();
        // Partial failure: step 1 committed, step 2 failed
        assert_eq!(result.final_state, MissionTxState::RolledBack);
        assert_eq!(result.outcome, TxOutcome::Compensated);
        let commit = result.commit_report.unwrap();
        assert!(commit.committed_count >= 1);
        assert!(commit.failed_count >= 1);
        let compensation = result
            .compensation_report
            .expect("compensation should run for committed step");
        assert_eq!(
            compensation.outcome,
            crate::plan::TxCompensationOutcome::FullyRolledBack
        );
        assert_eq!(compensation.compensated_count, 1);
        rt.block_on(async {
            let pane = mock.pane_state(0).await.expect("pane should exist");
            assert_eq!(pane.content, "okROLLBACK");
        });
    }

    #[test]
    fn integration_fail_step_injection() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
        });

        let config = TxExecutionConfig {
            auto_compensate: false,
            fail_step: Some("s2".to_string()),
            ..Default::default()
        };

        let executor = make_pane_executor(mock as WeztermHandle);
        let engine = TxExecutionEngine::new(executor, config);

        let mut contract = make_pane_contract(vec![
            (
                "s1".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "ok".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s2".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "injected-fail".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s3".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "skipped".to_string(),
                    paste_mode: None,
                },
            ),
        ]);

        let result = execute_durable(&engine, &mut contract, 5000).unwrap();
        // This fixture has no compensation actions. Keep the test focused on
        // deterministic failure injection rather than asking the engine to
        // synthesize an unproven rollback action.
        assert_eq!(result.outcome, TxOutcome::Failed);
        let commit = result.commit_report.unwrap();
        assert_eq!(commit.committed_count, 1); // s1 succeeded
        assert!(commit.failed_count >= 1); // s2 failed
        assert!(commit.skipped_count >= 1); // s3 skipped
    }

    #[test]
    fn integration_observability_events_emitted() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
        });

        let engine = make_pane_engine(mock as WeztermHandle);
        let mut contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::SendText {
                pane_id: 0,
                text: "observe".to_string(),
                paste_mode: None,
            },
        )]);

        let result = execute_durable(&engine, &mut contract, 5000).unwrap();
        // Should have at least prepare and commit events
        assert!(
            result.events.len() >= 2,
            "expected at least 2 observability events, got {}",
            result.events.len()
        );
        // Events should have sequential IDs
        for (i, event) in result.events.iter().enumerate() {
            if i > 0 {
                assert!(
                    event.sequence > result.events[i - 1].sequence,
                    "event sequences should be monotonically increasing"
                );
            }
        }
    }

    #[test]
    fn integration_prepare_gates_pass_but_unwired_action_fails_closed() {
        let mock = mock_wezterm_handle();
        let executor = make_pane_executor(mock);
        let engine = TxExecutionEngine::new(
            executor,
            TxExecutionConfig {
                auto_compensate: false,
                ..TxExecutionConfig::default()
            },
        );
        let mut contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::StoreData {
                key: "k".to_string(),
                value: serde_json::json!("v"),
            },
        )]);

        let result = execute_durable(&engine, &mut contract, 5000).unwrap();
        // Prepare phase should pass with allow-all policy
        assert!(
            result.prepare_report.outcome.commit_eligible(),
            "all gates should pass with allow-all policy"
        );
        assert_eq!(result.final_state, MissionTxState::Failed);
        assert_eq!(result.outcome, TxOutcome::Failed);
        let commit = result.commit_report.expect("commit report");
        assert_eq!(commit.committed_count, 0);
        assert_eq!(commit.failed_count, 1);
        assert!(matches!(
            &commit.step_results[0].outcome,
            crate::plan::TxCommitStepOutcome::Failed { reason_code }
                if reason_code == "store_data_unwired"
        ));
    }

    #[test]
    fn integration_pattern_wait_fails_closed_until_registry_is_wired() {
        let executor = make_pane_executor(mock_wezterm_handle());
        let engine = TxExecutionEngine::new(
            executor,
            TxExecutionConfig {
                auto_compensate: false,
                ..TxExecutionConfig::default()
            },
        );
        let mut contract = make_pane_contract(vec![(
            "w1".to_string(),
            StepAction::WaitFor {
                pane_id: Some(0),
                condition: WaitCondition::Pattern {
                    pane_id: None,
                    rule_id: "NEVER_MATCH".to_string(),
                },
                timeout_ms: 500,
            },
        )]);

        let result = execute_durable(&engine, &mut contract, 5000).unwrap();
        assert_eq!(result.final_state, MissionTxState::Failed);
        assert_eq!(result.outcome, TxOutcome::Failed);
        let commit = result.commit_report.expect("commit report");
        assert_eq!(commit.failed_count, 1);
        assert!(matches!(
            &commit.step_results[0].outcome,
            crate::plan::TxCommitStepOutcome::Failed { reason_code }
                if reason_code == "wait_for_pattern_registry_unwired"
        ));
    }

    #[test]
    fn integration_mixed_actions_fail_closed_at_unwired_lock() {
        let executor = make_pane_executor(mock_wezterm_handle());
        let engine = TxExecutionEngine::new(
            executor,
            TxExecutionConfig {
                auto_compensate: false,
                ..TxExecutionConfig::default()
            },
        );
        let mut contract = make_pane_contract(vec![
            (
                "l1".to_string(),
                StepAction::AcquireLock {
                    lock_name: "test".to_string(),
                    timeout_ms: None,
                },
            ),
            (
                "s1".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "action".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "d1".to_string(),
                StepAction::StoreData {
                    key: "result".to_string(),
                    value: serde_json::json!({"status": "done"}),
                },
            ),
            (
                "r1".to_string(),
                StepAction::ReleaseLock {
                    lock_name: "test".to_string(),
                },
            ),
        ]);

        let result = execute_durable(&engine, &mut contract, 5000).unwrap();
        assert_eq!(result.final_state, MissionTxState::Failed);
        assert_eq!(result.outcome, TxOutcome::Failed);
        let commit = result.commit_report.expect("commit report");
        assert_eq!(commit.committed_count, 0);
        assert_eq!(commit.failed_count, 1);
        assert_eq!(commit.skipped_count, 3);
        assert!(matches!(
            &commit.step_results[0].outcome,
            crate::plan::TxCommitStepOutcome::Failed { reason_code }
                if reason_code == "acquire_lock_unwired"
        ));
    }

    #[test]
    fn integration_ledger_populated() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
        });

        let engine = make_pane_engine(mock as WeztermHandle);
        let mut contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::SendText {
                pane_id: 0,
                text: "ledger-test".to_string(),
                paste_mode: None,
            },
        )]);

        let result = execute_durable(&engine, &mut contract, 5000).unwrap();
        // Ledger should be populated after execution
        assert!(
            !result.ledger.execution_id().is_empty(),
            "ledger should have execution_id"
        );
    }

    // ── ft-wgc1q: External waits observe ExternalSignalRegistry, not pane text ─

    #[test]
    fn external_wait_without_registry_returns_unsupported() {
        let mock = Arc::new(MockWezterm::new());
        let result = execute_step_action(
            &(mock as WeztermHandle),
            &StepAction::WaitFor {
                pane_id: None,
                condition: WaitCondition::External {
                    key: "deploy-ready".to_string(),
                },
                timeout_ms: 50,
            },
            None,
            None,
            None,
        );
        assert!(!result.0, "external wait must fail without a registry");
        assert_eq!(result.1, "wait_for_external_unsupported");
        let err = result.2.expect("error code present");
        assert!(
            err.contains("deploy-ready") && err.contains("with_external_signals"),
            "error must name signal key + wiring API: {err}"
        );
    }

    #[test]
    fn external_wait_observes_pre_fired_signal() {
        let mock = Arc::new(MockWezterm::new());
        let registry = Arc::new(crate::workflows::ExternalSignalRegistry::new());
        registry.signal("ready");
        let start = std::time::Instant::now();
        let result = execute_step_action(
            &(mock as WeztermHandle),
            &StepAction::WaitFor {
                pane_id: None,
                condition: WaitCondition::External {
                    key: "ready".to_string(),
                },
                timeout_ms: 60_000,
            },
            None,
            Some(registry.as_ref()),
            None,
        );
        let elapsed = start.elapsed();
        assert!(result.0, "pre-fired signal must satisfy: {:?}", result);
        assert_eq!(result.1, "wait_for_external_satisfied");
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "pre-fired signal returned too slowly: {elapsed:?}"
        );
    }

    #[test]
    fn external_wait_unblocks_when_signal_fires_during_wait() {
        let mock = Arc::new(MockWezterm::new());
        let registry = Arc::new(crate::workflows::ExternalSignalRegistry::new());
        let signaler = Arc::clone(&registry);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(80));
            signaler.signal("late");
        });
        let start = std::time::Instant::now();
        let result = execute_step_action(
            &(mock as WeztermHandle),
            &StepAction::WaitFor {
                pane_id: None,
                condition: WaitCondition::External {
                    key: "late".to_string(),
                },
                timeout_ms: 30_000,
            },
            None,
            Some(registry.as_ref()),
            None,
        );
        let elapsed = start.elapsed();
        assert!(result.0, "late signal must satisfy: {:?}", result);
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "signal observed too late: {elapsed:?}"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(40),
            "signal observed before it could fire: {elapsed:?}"
        );
    }

    #[test]
    fn external_wait_returns_timeout_when_signal_never_fires() {
        let mock = Arc::new(MockWezterm::new());
        let registry = Arc::new(crate::workflows::ExternalSignalRegistry::new());
        let start = std::time::Instant::now();
        let result = execute_step_action(
            &(mock as WeztermHandle),
            &StepAction::WaitFor {
                pane_id: None,
                condition: WaitCondition::External {
                    key: "never".to_string(),
                },
                timeout_ms: 120,
            },
            None,
            Some(registry.as_ref()),
            None,
        );
        let elapsed = start.elapsed();
        assert!(!result.0, "no signal must time out");
        assert_eq!(result.1, "wait_for_timeout");
        let err = result.2.expect("error code present");
        assert!(
            err.contains("never") && err.contains("120ms"),
            "timeout must name signal + duration: {err}"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(100),
            "wait returned before timeout: {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "wait grossly exceeded timeout: {elapsed:?}"
        );
    }

    #[test]
    fn external_wait_does_not_alias_signal_key_into_pane_text_search() {
        // Pre-fix: tx_execution would put the signal key in `pattern` and call
        // get_text on a target_pane it could not resolve, returning
        // wait_for_no_pane. With the registry path, the key is consulted
        // against the registry instead — never against pane text.
        let mock = Arc::new(MockWezterm::new());
        let registry = Arc::new(crate::workflows::ExternalSignalRegistry::new());
        registry.signal("ok");
        let result = execute_step_action(
            &(mock as WeztermHandle),
            &StepAction::WaitFor {
                pane_id: None,
                condition: WaitCondition::External {
                    key: "ok".to_string(),
                },
                timeout_ms: 60_000,
            },
            None,
            Some(registry.as_ref()),
            None,
        );
        assert!(result.0, "external wait must succeed without a target pane");
        assert_ne!(
            result.1, "wait_for_no_pane",
            "external wait must not fall through to pane-text path"
        );
    }

    // ── StoreData storage adapter and durability tests (ft-2j3vo) ──────

    #[test]
    fn store_data_without_adapter_fails_closed() {
        let mock = Arc::new(MockWezterm::new());
        let executor = make_pane_executor(mock as WeztermHandle);
        let contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::StoreData {
                key: "test_key".to_string(),
                value: serde_json::json!({"foo": "bar"}),
            },
        )]);

        let results = executor.execute_steps(&contract, None, 1000);
        assert_eq!(results.len(), 1);
        assert!(
            !results[0].success,
            "StoreData without adapter must fail closed"
        );
        assert_eq!(results[0].reason_code, "store_data_unwired");
        assert!(
            results[0]
                .error_code
                .as_deref()
                .unwrap_or("")
                .contains("FTX_STORE_DATA_UNWIRED"),
            "Error code must indicate unwired storage adapter"
        );
    }

    #[test]
    fn store_data_in_memory_adapter_stores_and_observes_value() {
        let mock = Arc::new(MockWezterm::new());
        let storage = Arc::new(InMemoryTxStorageAdapter::new());
        let executor = make_pane_executor(mock as WeztermHandle)
            .with_storage_adapter(Arc::clone(&storage) as Arc<dyn TxStorageAdapter>);

        let payload = serde_json::json!({
            "service": "postgres",
            "port": 5432,
            "replica_count": 3,
            "tags": ["db", "primary"]
        });

        let contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::StoreData {
                key: "service.postgres".to_string(),
                value: payload.clone(),
            },
        )]);

        let results = executor.execute_steps(&contract, None, 1000);
        assert_eq!(results.len(), 1);
        assert!(
            results[0].success,
            "StoreData with adapter must succeed: {:?}",
            results[0]
        );
        assert_eq!(results[0].reason_code, "store_data_succeeded");

        // Verify stored value is observable and exact
        let stored = storage.get_data("service.postgres").unwrap();
        assert_eq!(stored, Some(payload));
        assert!(storage.has_data("service.postgres").unwrap());
        assert_eq!(storage.len(), 1);
    }

    #[test]
    fn store_data_in_memory_adapter_idempotent_retry_overwrites() {
        let mock = Arc::new(MockWezterm::new());
        let storage = Arc::new(InMemoryTxStorageAdapter::new());
        let executor = make_pane_executor(mock as WeztermHandle)
            .with_storage_adapter(Arc::clone(&storage) as Arc<dyn TxStorageAdapter>);

        let v1 = serde_json::json!({"version": 1});
        let v2 = serde_json::json!({"version": 2});

        let contract1 = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::StoreData {
                key: "app.version".to_string(),
                value: v1.clone(),
            },
        )]);

        let r1 = executor.execute_steps(&contract1, None, 1000);
        assert!(r1[0].success);
        assert_eq!(storage.get_data("app.version").unwrap(), Some(v1));

        let contract2 = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::StoreData {
                key: "app.version".to_string(),
                value: v2.clone(),
            },
        )]);

        let r2 = executor.execute_steps(&contract2, None, 1000);
        assert!(r2[0].success);
        assert_eq!(storage.get_data("app.version").unwrap(), Some(v2));
    }

    #[test]
    fn store_data_file_adapter_atomic_durability() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let storage =
            Arc::new(FileTxStorageAdapter::new(temp_dir.path()).expect("create file adapter"));
        let mock = Arc::new(MockWezterm::new());
        let executor = make_pane_executor(mock as WeztermHandle)
            .with_storage_adapter(Arc::clone(&storage) as Arc<dyn TxStorageAdapter>);

        let val = serde_json::json!({
            "tx_id": "tx-12345",
            "status": "committed",
            "timestamp": 1720000000
        });

        let contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::StoreData {
                key: "tx_record_12345".to_string(),
                value: val.clone(),
            },
        )]);

        let results = executor.execute_steps(&contract, None, 1000);
        assert!(results[0].success);

        // Verify observable via adapter
        assert_eq!(
            storage.get_data("tx_record_12345").unwrap(),
            Some(val.clone())
        );

        // Verify directly on disk as a committed JSON file
        let file_path = temp_dir.path().join("tx_record_12345.json");
        assert!(file_path.exists(), "Target file must exist on disk");
        let disk_bytes = std::fs::read(&file_path).unwrap();
        let disk_val: serde_json::Value = serde_json::from_slice(&disk_bytes).unwrap();
        assert_eq!(disk_val, val);

        // Verify no lingering temp files in directory
        let entries = std::fs::read_dir(temp_dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec!["tx_record_12345.json".to_string()]);
    }

    #[test]
    fn store_data_file_adapter_invalid_keys_rejected() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let storage =
            Arc::new(FileTxStorageAdapter::new(temp_dir.path()).expect("create file adapter"));
        let mock = Arc::new(MockWezterm::new());
        let executor = make_pane_executor(mock as WeztermHandle)
            .with_storage_adapter(Arc::clone(&storage) as Arc<dyn TxStorageAdapter>);

        let invalid_keys = vec!["", "../secret", "path/to/key", "foo\\bar", ".hidden", ".."];

        for key in invalid_keys {
            let contract = make_pane_contract(vec![(
                "s1".to_string(),
                StepAction::StoreData {
                    key: key.to_string(),
                    value: serde_json::json!("data"),
                },
            )]);

            let results = executor.execute_steps(&contract, None, 1000);
            assert!(!results[0].success, "Invalid key '{key}' must fail closed");
            assert_eq!(results[0].reason_code, "store_data_failed");
        }
    }

    #[test]
    fn store_data_compensation_restores_previous_state() {
        let mock = Arc::new(MockWezterm::new());
        let storage = Arc::new(InMemoryTxStorageAdapter::new());
        // Seed initial state
        storage
            .store_data("cluster_mode", &serde_json::json!("standalone"))
            .unwrap();

        let executor = make_pane_executor(mock as WeztermHandle)
            .with_storage_adapter(Arc::clone(&storage) as Arc<dyn TxStorageAdapter>);

        let contract = make_pane_contract_with_compensations(
            vec![
                (
                    "s1".to_string(),
                    StepAction::StoreData {
                        key: "cluster_mode".to_string(),
                        value: serde_json::json!("replicated"),
                    },
                ),
                (
                    "s2".to_string(),
                    StepAction::SendText {
                        pane_id: 999, // Non-existent pane to trigger failure
                        text: "invalid".to_string(),
                        paste_mode: None,
                    },
                ),
            ],
            vec![(
                "s1".to_string(),
                StepAction::StoreData {
                    key: "cluster_mode".to_string(),
                    value: serde_json::json!("standalone"),
                },
            )],
        );

        // Execute commit phase (s1 succeeds, s2 fails)
        let commit_inputs = executor.execute_steps(&contract, None, 1000);
        assert!(commit_inputs[0].success);
        assert!(!commit_inputs[1].success);

        // While committed, value was mutated
        assert_eq!(
            storage.get_data("cluster_mode").unwrap(),
            Some(serde_json::json!("replicated"))
        );

        let commit_report = crate::plan::TxCommitReport {
            tx_id: TxId("tx-1".to_string()),
            plan_id: TxPlanId("plan-1".to_string()),
            outcome: crate::plan::TxCommitOutcome::PartialFailure,
            step_results: vec![
                crate::plan::TxCommitStepResult {
                    step_id: TxStepId("s1".to_string()),
                    ordinal: 0,
                    outcome: crate::plan::TxCommitStepOutcome::Committed {
                        reason_code: "store_data_succeeded".to_string(),
                    },
                    decision_path: "test".to_string(),
                    completed_at_ms: 1000,
                },
                crate::plan::TxCommitStepResult {
                    step_id: TxStepId("s2".to_string()),
                    ordinal: 1,
                    outcome: crate::plan::TxCommitStepOutcome::Failed {
                        reason_code: "send_text_failed".to_string(),
                    },
                    decision_path: "test".to_string(),
                    completed_at_ms: 1000,
                },
            ],
            failure_boundary: Some("s2".to_string()),
            committed_count: 1,
            failed_count: 1,
            skipped_count: 0,
            decision_path: "test".to_string(),
            reason_code: "test".to_string(),
            error_code: None,
            completed_at_ms: 1500,
            receipts: Vec::new(),
        };

        // Execute compensation phase
        let comp_inputs = executor.execute_compensations(&contract, &commit_report, None, 2000);
        assert_eq!(comp_inputs.len(), 1);
        assert!(comp_inputs[0].success, "Compensation must succeed");

        // Stored value is restored back to standalone
        assert_eq!(
            storage.get_data("cluster_mode").unwrap(),
            Some(serde_json::json!("standalone"))
        );
    }

    #[test]
    fn store_data_compensation_with_delete_data_removes_key() {
        let mock = Arc::new(MockWezterm::new());
        let storage = Arc::new(InMemoryTxStorageAdapter::new());
        let executor = make_pane_executor(mock as WeztermHandle)
            .with_storage_adapter(Arc::clone(&storage) as Arc<dyn TxStorageAdapter>);

        let contract = make_pane_contract_with_compensations(
            vec![
                (
                    "s1".to_string(),
                    StepAction::StoreData {
                        key: "ephemeral_lease".to_string(),
                        value: serde_json::json!({"holder": "agent-1"}),
                    },
                ),
                (
                    "s2".to_string(),
                    StepAction::SendText {
                        pane_id: 999,
                        text: "fail".to_string(),
                        paste_mode: None,
                    },
                ),
            ],
            vec![(
                "s1".to_string(),
                StepAction::Custom {
                    action_type: "delete_data".to_string(),
                    payload: serde_json::json!({"key": "ephemeral_lease"}),
                },
            )],
        );

        let commit_inputs = executor.execute_steps(&contract, None, 1000);
        assert!(commit_inputs[0].success);
        assert!(!commit_inputs[1].success);

        assert!(storage.has_data("ephemeral_lease").unwrap());

        let commit_report = crate::plan::TxCommitReport {
            tx_id: TxId("tx-1".to_string()),
            plan_id: TxPlanId("plan-1".to_string()),
            outcome: crate::plan::TxCommitOutcome::PartialFailure,
            step_results: vec![crate::plan::TxCommitStepResult {
                step_id: TxStepId("s1".to_string()),
                ordinal: 0,
                outcome: crate::plan::TxCommitStepOutcome::Committed {
                    reason_code: "store_data_succeeded".to_string(),
                },
                decision_path: "test".to_string(),
                completed_at_ms: 1000,
            }],
            failure_boundary: Some("s2".to_string()),
            committed_count: 1,
            failed_count: 1,
            skipped_count: 0,
            decision_path: "test".to_string(),
            reason_code: "test".to_string(),
            error_code: None,
            completed_at_ms: 1500,
            receipts: Vec::new(),
        };

        let comp_inputs = executor.execute_compensations(&contract, &commit_report, None, 2000);
        assert_eq!(comp_inputs.len(), 1);
        assert!(comp_inputs[0].success);
        assert_eq!(comp_inputs[0].reason_code, "delete_data_succeeded");

        // Key must now be deleted
        assert!(!storage.has_data("ephemeral_lease").unwrap());
        assert_eq!(storage.get_data("ephemeral_lease").unwrap(), None);
    }

    #[derive(Clone)]
    struct MixedRecoveryGateExecutor {
        dispatched_compensations: Rc<RefCell<Vec<String>>>,
        require_approval_step: Option<String>,
        fail_precondition_step: Option<String>,
    }

    impl MixedRecoveryGateExecutor {
        fn new() -> (Self, Rc<RefCell<Vec<String>>>) {
            let dispatched_compensations = Rc::new(RefCell::new(Vec::new()));
            (
                Self {
                    dispatched_compensations: Rc::clone(&dispatched_compensations),
                    require_approval_step: None,
                    fail_precondition_step: None,
                },
                dispatched_compensations,
            )
        }
    }

    impl effect_seal::NonEffectful for MixedRecoveryGateExecutor {}

    impl StepExecutor for MixedRecoveryGateExecutor {
        fn evaluate_gates(
            &self,
            contract: &MissionTxContract,
            _now_ms: i64,
        ) -> Vec<TxPrepareGateInput> {
            let mut gates = crate::plan::tx_prepare_gate_inputs_allow_all(contract);
            for gate in &mut gates {
                if let Some(ref req_step) = self.require_approval_step {
                    if gate.step_id.0 == *req_step {
                        gate.required_approval = Some(crate::plan::TxPrepareApprovalRequirement {
                            workspace_id: "ws-1".to_string(),
                            action_kind: "send_text".to_string(),
                            pane_id: gate.pane_id,
                            action_fingerprint: "fp-1".to_string(),
                            reason_code: Some("approval_required_for_test".to_string()),
                        });
                        gate.approval_satisfied = false;
                        gate.approval_reason_code = Some("approval_required_for_test".to_string());
                    }
                }
                if let Some(ref fail_step) = self.fail_precondition_step {
                    if gate.step_id.0 == *fail_step {
                        gate.preconditions_satisfied = false;
                        gate.precondition_reason_code =
                            Some("precondition_target_dead".to_string());
                    }
                }
            }
            gates
        }

        fn execute_steps(
            &self,
            contract: &MissionTxContract,
            fail_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCommitStepInput> {
            crate::plan::mission_tx_commit_step_inputs(contract, fail_step, now_ms)
        }

        fn execute_compensations(
            &self,
            _contract: &MissionTxContract,
            commit_report: &TxCommitReport,
            fail_for_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCompensationStepInput> {
            self.dispatched_compensations.borrow_mut().extend(
                commit_report
                    .step_results
                    .iter()
                    .filter(|result| result.outcome.is_committed())
                    .map(|result| result.step_id.0.clone()),
            );
            crate::plan::mission_tx_compensation_inputs(commit_report, fail_for_step, now_ms)
        }
    }

    #[test]
    fn mixed_recovery_reconstructs_proven_receipts_under_kill_switch() -> Result<(), String> {
        let mut contract = make_test_contract(3);
        let (_store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        store
            .create_ledger("kill-switch-mixed-recovery", &compiled_plan)
            .map_err(|err| err.to_string())?;
        record_durable_test_outcome(
            &mut store,
            "kill-switch-mixed-recovery",
            test_commit_key(&contract, "step-0"),
            StepOutcome::Success {
                result: Some("effect_0_applied".to_string()),
            },
            StepRisk::High,
            "agent-step-0",
            5_000,
        )?;

        let (executor, dispatched_compensations) = MixedRecoveryGateExecutor::new();
        let engine = TxExecutionEngine::new(
            executor,
            TxExecutionConfig {
                auto_compensate: true,
                kill_switch: MissionKillSwitchLevel::HardStop,
                ..TxExecutionConfig::default()
            },
        );

        // Replay with KillSwitch on (HardStop)
        let result = engine
            .execute_with_store(&mut contract, &mut store, 6_000)
            .map_err(|err| err.to_string())?;

        assert_eq!(result.final_state, MissionTxState::RolledBack);
        assert_eq!(result.outcome, TxOutcome::Compensated);
        assert_eq!(
            *dispatched_compensations.borrow(),
            vec!["step-0".to_string()]
        );

        // Verify reconstructed commit receipts for step-0, and skipped for step-1 & step-2
        assert!(latest_tx_receipt_matches(
            &contract,
            "commit",
            "step-0",
            "committed"
        ));
        assert!(latest_tx_receipt_matches(
            &contract, "commit", "step-1", "skipped"
        ));
        assert!(latest_tx_receipt_matches(
            &contract, "commit", "step-2", "skipped"
        ));
        assert!(latest_tx_receipt_matches(
            &contract,
            "compensate",
            "step-0",
            "compensated"
        ));

        // Verify no duplicate receipts for step-0 commit
        let commit_receipts_step_0 = contract
            .receipts
            .iter()
            .filter(|r| {
                r.get("phase").and_then(serde_json::Value::as_str) == Some("commit")
                    && r.get("step_id").and_then(serde_json::Value::as_str) == Some("step-0")
            })
            .count();
        assert_eq!(
            commit_receipts_step_0, 1,
            "must not have duplicate commit receipts for step-0"
        );

        Ok(())
    }

    #[test]
    fn mixed_recovery_reconstructs_proven_receipts_under_approval_required() -> Result<(), String> {
        let mut contract = make_test_contract(2);
        let (_store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        store
            .create_ledger("approval-mixed-recovery", &compiled_plan)
            .map_err(|err| err.to_string())?;
        record_durable_test_outcome(
            &mut store,
            "approval-mixed-recovery",
            test_commit_key(&contract, "step-0"),
            StepOutcome::Success {
                result: Some("effect_0_applied".to_string()),
            },
            StepRisk::High,
            "agent-step-0",
            5_000,
        )?;

        let (mut executor, dispatched_compensations) = MixedRecoveryGateExecutor::new();
        executor.require_approval_step = Some("step-1".to_string());

        let engine = TxExecutionEngine::new(
            executor,
            TxExecutionConfig {
                auto_compensate: true,
                ..TxExecutionConfig::default()
            },
        );

        let result = engine
            .execute_with_store(&mut contract, &mut store, 6_000)
            .map_err(|err| err.to_string())?;

        assert_eq!(result.final_state, MissionTxState::RolledBack);
        assert_eq!(result.outcome, TxOutcome::Compensated);
        assert_eq!(
            *dispatched_compensations.borrow(),
            vec!["step-0".to_string()]
        );

        assert!(latest_tx_receipt_matches(
            &contract,
            "commit",
            "step-0",
            "committed"
        ));
        assert!(latest_tx_receipt_matches(
            &contract, "commit", "step-1", "skipped"
        ));
        assert!(latest_tx_receipt_matches(
            &contract,
            "compensate",
            "step-0",
            "compensated"
        ));
        Ok(())
    }

    #[test]
    fn mixed_recovery_reconstructs_proven_receipts_under_failed_precondition() -> Result<(), String>
    {
        let mut contract = make_test_contract(2);
        contract
            .plan
            .preconditions
            .push(TxPrecondition::PromptActive { pane_id: 1 });
        let (_store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        store
            .create_ledger("precondition-mixed-recovery", &compiled_plan)
            .map_err(|err| err.to_string())?;
        record_durable_test_outcome(
            &mut store,
            "precondition-mixed-recovery",
            test_commit_key(&contract, "step-0"),
            StepOutcome::Success {
                result: Some("effect_0_applied".to_string()),
            },
            StepRisk::High,
            "agent-step-0",
            5_000,
        )?;

        let (mut executor, dispatched_compensations) = MixedRecoveryGateExecutor::new();
        executor.fail_precondition_step = Some("step-1".to_string());

        let engine = TxExecutionEngine::new(
            executor,
            TxExecutionConfig {
                auto_compensate: true,
                ..TxExecutionConfig::default()
            },
        );

        let result = engine
            .execute_with_store(&mut contract, &mut store, 6_000)
            .map_err(|err| err.to_string())?;

        assert_eq!(result.final_state, MissionTxState::RolledBack);
        assert_eq!(result.outcome, TxOutcome::Compensated);
        assert_eq!(
            *dispatched_compensations.borrow(),
            vec!["step-0".to_string()]
        );

        assert!(latest_tx_receipt_matches(
            &contract,
            "commit",
            "step-0",
            "committed"
        ));
        assert!(latest_tx_receipt_matches(
            &contract, "commit", "step-1", "skipped"
        ));
        assert!(latest_tx_receipt_matches(
            &contract,
            "compensate",
            "step-0",
            "compensated"
        ));
        Ok(())
    }

    #[test]
    fn mixed_recovery_without_auto_compensate_fails_closed_in_committing_state()
    -> Result<(), String> {
        let mut contract = make_test_contract(2);
        let (_store_dir, mut store) = durable_store();
        let compiled_plan = compiled_plan_from_contract(&contract);
        store
            .create_ledger("no-autocomp-mixed-recovery", &compiled_plan)
            .map_err(|err| err.to_string())?;
        record_durable_test_outcome(
            &mut store,
            "no-autocomp-mixed-recovery",
            test_commit_key(&contract, "step-0"),
            StepOutcome::Success {
                result: Some("effect_0_applied".to_string()),
            },
            StepRisk::High,
            "agent-step-0",
            5_000,
        )?;

        let (executor, dispatched_compensations) = MixedRecoveryGateExecutor::new();
        let engine = TxExecutionEngine::new(
            executor,
            TxExecutionConfig {
                auto_compensate: false,
                kill_switch: MissionKillSwitchLevel::HardStop,
                ..TxExecutionConfig::default()
            },
        );

        let err = engine
            .execute_with_store(&mut contract, &mut store, 6_000)
            .expect_err(
                "must fail closed when auto-compensate is false and uncommitted steps exist",
            );

        assert!(matches!(err, TxExecutionError::CommitPhase(_)));
        assert!(
            err.to_string()
                .contains("automatic compensation is disabled")
        );
        assert!(dispatched_compensations.borrow().is_empty());

        assert_eq!(contract.lifecycle_state, MissionTxState::Committing);
        assert_eq!(contract.outcome, TxOutcome::Pending);
        assert!(latest_tx_receipt_matches(
            &contract,
            "commit",
            "step-0",
            "committed"
        ));
        assert!(latest_tx_receipt_matches(
            &contract, "commit", "step-1", "skipped"
        ));
        Ok(())
    }
}
