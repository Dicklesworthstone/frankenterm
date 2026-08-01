//! Crash-consistent GUI window and mixed-domain tab-layout persistence.
//!
//! The original implementation synchronously read, rewrote, and replaced one
//! unversioned JSON map from the resize callback. Besides adding storage I/O to
//! a latency-sensitive GUI path, concurrent windows could lose one another's
//! updates and a crash could leave a truncated authority file.
//!
//! This module keeps the existing project-owned `window-state.json` authority
//! and migrates its legacy geometry map in place. New records are:
//!
//! - versioned and checksum protected;
//! - bounded before allocation or mutation;
//! - written through a two-slot copy-on-write journal;
//! - serialized by a cross-process file lock;
//! - coalesced behind a nonblocking, capacity-one worker wakeup; and
//! - restricted to opaque identities, revisions, and workspace association.
//!
//! The two-slot journal deliberately avoids relying on replacement-rename
//! behavior, which differs across operating systems. A commit rewrites only
//! the older/inactive slot and fsyncs it. On restart, the highest completely
//! validated generation wins. An interrupted write therefore yields either
//! the prior generation or the new generation, never a partially decoded
//! topology mutation.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use window::WindowState;

const STORE_SCHEMA_VERSION: u32 = 3;
const PREVIOUS_STORE_SCHEMA_VERSION: u32 = 2;
const MAX_STATE_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_WORKSPACE_BYTES: usize = 1_024;
const MAX_WORKSPACES: usize = 4_096;
const MAX_DOMAIN_BINDINGS: usize = 4_096;
const MAX_LAYOUT_OVERLAYS: usize = 4_096;
const MAX_OVERLAY_TOMBSTONES: usize = 4_096;
const MAX_TABS_PER_OVERLAY: usize = 4_096;
const MAX_TOTAL_OVERLAY_TABS: usize = 16_384;
const MAX_PENDING_WAITERS: usize = 4_096;
const MAX_CORRUPT_EVIDENCE_FILES: usize = 8;
const WRITE_DEBOUNCE: Duration = Duration::from_millis(25);

/// Finite, privacy-safe diagnostic classification for persistence failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistenceFailureCode {
    Io,
    Corrupt,
    UnsupportedVersion,
    Oversized,
    Invalid,
    Quota,
    RevisionExhausted,
    StaleOverlay,
    OverlayRevisionConflict,
    OverlayCasConflict,
    RetiredOverlay,
    AmbiguousGeneration,
    WorkerStopped,
}

/// A fail-closed persistence error. No variant contains terminal text, a
/// command, a cwd, credentials, or a transport locator.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PersistenceFailure {
    #[error("{operation} failed ({kind:?})")]
    Io {
        operation: &'static str,
        kind: io::ErrorKind,
    },
    #[error("persisted state is corrupt: {reason}")]
    Corrupt { reason: String },
    #[error("unsupported persisted schema version {found}; current is {current}")]
    UnsupportedVersion { found: u32, current: u32 },
    #[error("persisted state is oversized: {actual} bytes exceeds {maximum}")]
    Oversized { actual: u64, maximum: u64 },
    #[error("persisted state is invalid: {reason}")]
    Invalid { reason: String },
    #[error("persisted state quota exceeded: {reason}")]
    Quota { reason: String },
    #[error("persisted revision namespace is exhausted")]
    RevisionExhausted,
    #[error("overlay revision {incoming} is older than committed revision {committed}")]
    StaleOverlay { incoming: u64, committed: u64 },
    #[error("overlay revision {revision} was reused with different content")]
    OverlayRevisionConflict { revision: u64 },
    #[error("overlay CAS expected base {expected:?}, but authority is at {committed:?}")]
    OverlayCasConflict {
        expected: Option<u64>,
        committed: Option<u64>,
    },
    #[error("overlay identity was retired at local revision {last_revision}")]
    RetiredOverlay { last_revision: u64 },
    #[error("two different persisted states claim generation {revision}")]
    AmbiguousGeneration { revision: u64 },
    #[error("persistence worker stopped")]
    WorkerStopped,
}

impl PersistenceFailure {
    #[must_use]
    pub const fn code(&self) -> PersistenceFailureCode {
        match self {
            Self::Io { .. } => PersistenceFailureCode::Io,
            Self::Corrupt { .. } => PersistenceFailureCode::Corrupt,
            Self::UnsupportedVersion { .. } => PersistenceFailureCode::UnsupportedVersion,
            Self::Oversized { .. } => PersistenceFailureCode::Oversized,
            Self::Invalid { .. } => PersistenceFailureCode::Invalid,
            Self::Quota { .. } => PersistenceFailureCode::Quota,
            Self::RevisionExhausted => PersistenceFailureCode::RevisionExhausted,
            Self::StaleOverlay { .. } => PersistenceFailureCode::StaleOverlay,
            Self::OverlayRevisionConflict { .. } => {
                PersistenceFailureCode::OverlayRevisionConflict
            }
            Self::OverlayCasConflict { .. } => PersistenceFailureCode::OverlayCasConflict,
            Self::RetiredOverlay { .. } => PersistenceFailureCode::RetiredOverlay,
            Self::AmbiguousGeneration { .. } => {
                PersistenceFailureCode::AmbiguousGeneration
            }
            Self::WorkerStopped => PersistenceFailureCode::WorkerStopped,
        }
    }

    fn io(operation: &'static str, error: io::Error) -> Self {
        Self::Io {
            operation,
            kind: error.kind(),
        }
    }

    #[cfg(test)]
    const fn injected_io(operation: &'static str) -> Self {
        Self::Io {
            operation,
            kind: io::ErrorKind::Other,
        }
    }

    fn corrupt(reason: impl Into<String>) -> Self {
        Self::Corrupt {
            reason: reason.into(),
        }
    }

    fn invalid(reason: impl Into<String>) -> Self {
        Self::Invalid {
            reason: reason.into(),
        }
    }

    fn quota(reason: impl Into<String>) -> Self {
        Self::Quota {
            reason: reason.into(),
        }
    }
}

/// Client-owned stable identity for one configured connection binding.
///
/// It is random, contains no credentials, and remains stable across GUI
/// process restarts. The privacy-safe target fingerprint is stored separately.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct DomainBindingId([u8; 16]);

impl DomainBindingId {
    #[must_use]
    pub fn new() -> Self {
        Self(*uuid::Uuid::new_v4().as_bytes())
    }

    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(self) -> [u8; 16] {
        self.0
    }
}

impl Default for DomainBindingId {
    fn default() -> Self {
        Self::new()
    }
}

/// Hash of a canonical non-secret transport target. The raw hostname, socket
/// path, username, and credentials are never stored in this file.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct PrivacySafeTargetFingerprint([u8; 32]);

impl PrivacySafeTargetFingerprint {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Server-owned identity for one live mux-session incarnation.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct StableMuxSessionId([u8; 16]);

impl StableMuxSessionId {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(self) -> [u8; 16] {
        self.0
    }
}

/// Client-owned stable association for one mixed GUI window.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct LayoutWindowId([u8; 16]);

impl LayoutWindowId {
    #[must_use]
    pub fn new() -> Self {
        Self(*uuid::Uuid::new_v4().as_bytes())
    }

    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(self) -> [u8; 16] {
        self.0
    }
}

impl Default for LayoutWindowId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable local-session identity required before a local tab can participate
/// in a durable mixed overlay.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct StableLocalSessionId([u8; 16]);

impl StableLocalSessionId {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(self) -> [u8; 16] {
        self.0
    }
}

/// Stable local tab identity supplied by a restorable local/session runtime.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct StableLocalTabId([u8; 16]);

impl StableLocalTabId {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(self) -> [u8; 16] {
        self.0
    }
}

/// Provenance-bound identity of one tab occupying a mixed GUI layout.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StableTabSlot {
    Remote {
        binding_id: DomainBindingId,
        session_id: StableMuxSessionId,
        remote_window_id: u64,
        remote_tab_id: u64,
    },
    Local {
        session_id: StableLocalSessionId,
        tab_id: StableLocalTabId,
    },
}

impl StableTabSlot {
    #[must_use]
    pub const fn remote(
        binding_id: DomainBindingId,
        session_id: StableMuxSessionId,
        remote_window_id: u64,
        remote_tab_id: u64,
    ) -> Self {
        Self::Remote {
            binding_id,
            session_id,
            remote_window_id,
            remote_tab_id,
        }
    }

    #[must_use]
    pub const fn local(session_id: StableLocalSessionId, tab_id: StableLocalTabId) -> Self {
        Self::Local { session_id, tab_id }
    }

    #[must_use]
    pub const fn remote_binding(self) -> Option<DomainBindingId> {
        match self {
            Self::Remote { binding_id, .. } => Some(binding_id),
            Self::Local { .. } => None,
        }
    }
}

/// The persisted maximize/fullscreen state for a single workspace.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PersistedWindowState {
    #[serde(default)]
    pub maximized: bool,
    #[serde(default)]
    pub fullscreen: bool,
}

/// Select the restore state for a window after asynchronous construction.
///
/// A mux window can move workspaces while its native window and renderer are
/// being created. The cohort capture remains valid only while the workspace is
/// unchanged; otherwise the caller supplies a current, already-cached lookup.
pub fn resolve_saved_window_state<F>(
    captured_workspace: &str,
    captured_state: Option<PersistedWindowState>,
    current_workspace: Option<&str>,
    load_current: F,
) -> Option<PersistedWindowState>
where
    F: FnOnce(&str) -> Option<PersistedWindowState>,
{
    match current_workspace {
        Some(workspace) if workspace == captured_workspace => captured_state,
        Some(workspace) => load_current(workspace),
        None => None,
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DomainBindingRecord {
    target_fingerprint: PrivacySafeTargetFingerprint,
    binding_id: DomainBindingId,
}

impl DomainBindingRecord {
    #[must_use]
    pub const fn target_fingerprint(&self) -> PrivacySafeTargetFingerprint {
        self.target_fingerprint
    }

    #[must_use]
    pub const fn binding_id(&self) -> DomainBindingId {
        self.binding_id
    }
}

/// Versioned authoritative overlay for a mixed-domain/local GUI window.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MixedDomainLayoutOverlay {
    window_id: LayoutWindowId,
    workspace: String,
    local_revision: u64,
    slots: Vec<StableTabSlot>,
    active: Option<StableTabSlot>,
}

impl MixedDomainLayoutOverlay {
    pub fn new(
        window_id: LayoutWindowId,
        workspace: impl AsRef<str>,
        local_revision: u64,
        slots: Vec<StableTabSlot>,
        active: Option<StableTabSlot>,
    ) -> Result<Self, PersistenceFailure> {
        let workspace = workspace.as_ref();
        validate_workspace(workspace)?;
        let overlay = Self {
            window_id,
            workspace: workspace.to_owned(),
            local_revision,
            slots,
            active,
        };
        validate_overlay(&overlay)?;
        Ok(overlay)
    }

    #[must_use]
    pub const fn window_id(&self) -> LayoutWindowId {
        self.window_id
    }

    #[must_use]
    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    #[must_use]
    pub const fn local_revision(&self) -> u64 {
        self.local_revision
    }

    #[must_use]
    pub fn slots(&self) -> &[StableTabSlot] {
        &self.slots
    }

    #[must_use]
    pub const fn active(&self) -> Option<StableTabSlot> {
        self.active
    }

    pub fn next_revision(
        &self,
        slots: Vec<StableTabSlot>,
        active: Option<StableTabSlot>,
    ) -> Result<Self, PersistenceFailure> {
        let next = self
            .local_revision
            .checked_add(1)
            .ok_or(PersistenceFailure::RevisionExhausted)?;
        Self::new(self.window_id, &self.workspace, next, slots, active)
    }
}

/// Durable proof that one stable mixed-layout window identity was retired.
///
/// Tombstones deliberately retain only the last live local revision and the
/// store generation that committed the retirement. A stable
/// [`LayoutWindowId`] is never reusable; the bounded retention policy protects
/// recent lineages, while a mutation with `base_revision = Some(_)` still
/// fails against absence after an old tombstone is pruned.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayTombstone {
    window_id: LayoutWindowId,
    last_local_revision: u64,
    retired_at_store_revision: u64,
}

impl OverlayTombstone {
    fn new(
        window_id: LayoutWindowId,
        last_local_revision: u64,
        retired_at_store_revision: u64,
    ) -> Result<Self, PersistenceFailure> {
        let tombstone = Self {
            window_id,
            last_local_revision,
            retired_at_store_revision,
        };
        validate_tombstone(&tombstone)?;
        Ok(tombstone)
    }

    #[must_use]
    pub const fn window_id(self) -> LayoutWindowId {
        self.window_id
    }

    #[must_use]
    pub const fn last_local_revision(self) -> u64 {
        self.last_local_revision
    }

    #[must_use]
    pub const fn retired_at_store_revision(self) -> u64 {
        self.retired_at_store_revision
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DesiredOverlayState {
    Live(MixedDomainLayoutOverlay),
    Deleted {
        window_id: LayoutWindowId,
        last_local_revision: u64,
    },
}

impl DesiredOverlayState {
    const fn window_id(&self) -> LayoutWindowId {
        match self {
            Self::Live(overlay) => overlay.window_id,
            Self::Deleted { window_id, .. } => *window_id,
        }
    }

    const fn last_local_revision(&self) -> u64 {
        match self {
            Self::Live(overlay) => overlay.local_revision,
            Self::Deleted {
                last_local_revision,
                ..
            } => *last_local_revision,
        }
    }

    fn live_tab_count(&self) -> usize {
        match self {
            Self::Live(overlay) => overlay.slots.len(),
            Self::Deleted { .. } => 0,
        }
    }
}

/// One coalesced compare-and-swap lineage.
///
/// `base_revision` is always the revision that was durably observed before
/// the first queued mutation. Coalescing replaces only `desired`, so a chain
/// never becomes a CAS against an intermediate revision that exists only in
/// memory.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingOverlayMutation {
    base_revision: Option<u64>,
    desired: DesiredOverlayState,
    superseded_updates: usize,
}

impl PendingOverlayMutation {
    fn live(
        base_revision: Option<u64>,
        overlay: MixedDomainLayoutOverlay,
    ) -> Result<Self, PersistenceFailure> {
        validate_overlay(&overlay)?;
        let expected_revision = match base_revision {
            Some(base) => base
                .checked_add(1)
                .ok_or(PersistenceFailure::RevisionExhausted)?,
            None => 1,
        };
        if overlay.local_revision != expected_revision {
            return Err(PersistenceFailure::OverlayCasConflict {
                expected: base_revision,
                committed: overlay.local_revision.checked_sub(1),
            });
        }
        Ok(Self {
            base_revision,
            desired: DesiredOverlayState::Live(overlay),
            superseded_updates: 0,
        })
    }

    fn deleted(
        window_id: LayoutWindowId,
        base_revision: Option<u64>,
    ) -> Result<Self, PersistenceFailure> {
        let Some(last_local_revision) = base_revision else {
            return Err(PersistenceFailure::invalid(
                "cannot retire an overlay without a committed or pending live base",
            ));
        };
        if last_local_revision == 0 {
            return Err(PersistenceFailure::invalid(
                "overlay revision zero is reserved",
            ));
        }
        Ok(Self {
            base_revision,
            desired: DesiredOverlayState::Deleted {
                window_id,
                last_local_revision,
            },
            superseded_updates: 0,
        })
    }

    const fn window_id(&self) -> LayoutWindowId {
        self.desired.window_id()
    }

    const fn desired_revision(&self) -> u64 {
        self.desired.last_local_revision()
    }

    fn live_tab_count(&self) -> usize {
        self.desired.live_tab_count()
    }
}

/// Source selected by the two-slot recovery algorithm.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreSource {
    Empty,
    LegacyGeometry,
    Primary,
    Shadow,
}

/// Validated state returned to startup/reconciliation code. Callers receive no
/// topology mutation capability; they must reconcile the stable keys against a
/// validated live snapshot before applying anything.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayoutStateSnapshot {
    pub source: StoreSource,
    pub degraded_recovery: bool,
    pub store_revision: u64,
    pub window_states: BTreeMap<String, PersistedWindowState>,
    pub domain_bindings: Vec<DomainBindingRecord>,
    pub overlays: Vec<MixedDomainLayoutOverlay>,
    pub tombstones: Vec<OverlayTombstone>,
}

impl LayoutStateSnapshot {
    #[must_use]
    pub fn binding_for(
        &self,
        target_fingerprint: PrivacySafeTargetFingerprint,
    ) -> Option<DomainBindingId> {
        self.domain_bindings
            .iter()
            .find(|record| record.target_fingerprint == target_fingerprint)
            .map(|record| record.binding_id)
    }

    #[must_use]
    pub fn overlay(&self, window_id: LayoutWindowId) -> Option<&MixedDomainLayoutOverlay> {
        self.overlays
            .iter()
            .find(|overlay| overlay.window_id == window_id)
    }

    #[must_use]
    pub fn tombstone(&self, window_id: LayoutWindowId) -> Option<OverlayTombstone> {
        self.tombstones
            .iter()
            .copied()
            .find(|tombstone| tombstone.window_id == window_id)
    }
}

/// Side-effect-free mixed-overlay reconciliation result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconciledOverlay {
    /// Persisted live slots plus bounded unavailable-domain placeholders, then
    /// newly observed live slots in source-authoritative order.
    pub ordered_slots: Vec<StableTabSlot>,
    /// The actionable subset of `ordered_slots`.
    pub live_slots: Vec<StableTabSlot>,
    /// Active identity after right-neighbor, left-neighbor, then appended-live
    /// fallback. It is always a member of `live_slots`.
    pub active_live_slot: Option<StableTabSlot>,
    pub retained_unavailable: usize,
    pub dropped_closed_or_stale: usize,
    pub appended_new: usize,
}

/// Reconcile a validated overlay against exact live stable identities.
///
/// A remote slot whose binding is listed in `unavailable_bindings` remains as
/// a non-actionable placeholder. An absent slot from an available binding is
/// closed or stale and is removed. A same-number tab under a new server
/// incarnation is a distinct key and therefore cannot inherit the old slot.
/// A live slot from an unavailable binding is contradictory input and fails
/// closed rather than receiving topology authority.
pub fn reconcile_overlay(
    overlay: &MixedDomainLayoutOverlay,
    live: &[StableTabSlot],
    unavailable_bindings: &BTreeSet<DomainBindingId>,
) -> Result<ReconciledOverlay, PersistenceFailure> {
    validate_overlay(overlay)?;
    if live.len() > MAX_TABS_PER_OVERLAY {
        return Err(PersistenceFailure::quota(format!(
            "live tab count {} exceeds {}",
            live.len(),
            MAX_TABS_PER_OVERLAY
        )));
    }
    if unavailable_bindings.len() > MAX_DOMAIN_BINDINGS {
        return Err(PersistenceFailure::quota(format!(
            "unavailable binding count {} exceeds {}",
            unavailable_bindings.len(),
            MAX_DOMAIN_BINDINGS
        )));
    }

    let live_set = live.iter().copied().collect::<HashSet<_>>();
    if live_set.len() != live.len() {
        return Err(PersistenceFailure::invalid(
            "live layout contains duplicate stable tab identities",
        ));
    }
    if live.iter().any(|slot| {
        slot.remote_binding()
            .is_some_and(|binding| unavailable_bindings.contains(&binding))
    }) {
        return Err(PersistenceFailure::invalid(
            "live layout contains a remote identity from an unavailable binding",
        ));
    }

    let requested_capacity = overlay
        .slots
        .len()
        .checked_add(live.len())
        .unwrap_or(MAX_TABS_PER_OVERLAY);
    let mut ordered_slots =
        Vec::with_capacity(requested_capacity.min(MAX_TABS_PER_OVERLAY));
    let mut seen = HashSet::with_capacity(ordered_slots.capacity());
    let mut retained_unavailable = 0usize;
    let mut dropped_closed_or_stale = 0usize;

    for slot in &overlay.slots {
        let is_live = live_set.contains(slot);
        let unavailable = slot
            .remote_binding()
            .is_some_and(|binding| unavailable_bindings.contains(&binding));
        if is_live || unavailable {
            if seen.insert(*slot) {
                if ordered_slots.len() == MAX_TABS_PER_OVERLAY {
                    return Err(PersistenceFailure::quota(format!(
                        "reconciled tab count would exceed {MAX_TABS_PER_OVERLAY}"
                    )));
                }
                ordered_slots.push(*slot);
            }
            if unavailable && !is_live {
                retained_unavailable += 1;
            }
        } else {
            dropped_closed_or_stale += 1;
        }
    }

    let mut appended_new = 0usize;
    for slot in live {
        if seen.insert(*slot) {
            if ordered_slots.len() == MAX_TABS_PER_OVERLAY {
                return Err(PersistenceFailure::quota(format!(
                    "reconciled tab count would exceed {MAX_TABS_PER_OVERLAY}"
                )));
            }
            ordered_slots.push(*slot);
            appended_new += 1;
        }
    }

    let active_live_slot = match overlay.active {
        Some(active) if live_set.contains(&active) => Some(active),
        Some(active) => {
            let active_index = overlay
                .slots
                .iter()
                .position(|candidate| *candidate == active)
                .ok_or_else(|| {
                    PersistenceFailure::invalid(
                        "overlay active identity is not a member of its ordered slots",
                    )
                })?;
            overlay.slots[active_index + 1..]
                .iter()
                .find(|candidate| live_set.contains(candidate))
                .or_else(|| {
                    overlay.slots[..active_index]
                        .iter()
                        .rev()
                        .find(|candidate| live_set.contains(candidate))
                })
                .copied()
                .or_else(|| {
                    live.iter()
                        .find(|candidate| !overlay.slots.contains(candidate))
                        .copied()
                })
        }
        None => live.first().copied(),
    };

    let live_slots = ordered_slots
        .iter()
        .copied()
        .filter(|slot| live_set.contains(slot))
        .collect::<Vec<_>>();
    debug_assert!(
        active_live_slot.is_none() || active_live_slot.is_some_and(|slot| live_set.contains(&slot))
    );

    Ok(ReconciledOverlay {
        ordered_slots,
        live_slots,
        active_live_slot,
        retained_unavailable,
        dropped_closed_or_stale,
        appended_new,
    })
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedState {
    schema_version: u32,
    store_revision: u64,
    #[serde(default)]
    window_states: BTreeMap<String, PersistedWindowState>,
    #[serde(default)]
    domain_bindings: Vec<DomainBindingRecord>,
    #[serde(default)]
    overlays: Vec<MixedDomainLayoutOverlay>,
    tombstones: Vec<OverlayTombstone>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            schema_version: STORE_SCHEMA_VERSION,
            store_revision: 0,
            window_states: BTreeMap::new(),
            domain_bindings: Vec::new(),
            overlays: Vec::new(),
            tombstones: Vec::new(),
        }
    }
}

/// Exact schema-v2 payload shape. Keeping this type separate is essential:
/// its checksum must be verified over the bytes produced by the v2 field set
/// before an empty tombstone collection is introduced by migration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedStateV2 {
    schema_version: u32,
    store_revision: u64,
    #[serde(default)]
    window_states: BTreeMap<String, PersistedWindowState>,
    #[serde(default)]
    domain_bindings: Vec<DomainBindingRecord>,
    #[serde(default)]
    overlays: Vec<MixedDomainLayoutOverlay>,
}

impl PersistedStateV2 {
    fn into_current(self) -> PersistedState {
        PersistedState {
            schema_version: STORE_SCHEMA_VERSION,
            store_revision: self.store_revision,
            window_states: self.window_states,
            domain_bindings: self.domain_bindings,
            overlays: self.overlays,
            tombstones: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DiskSlot {
    payload: PersistedState,
    sha256: [u8; 32],
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DiskSlotV2 {
    payload: PersistedStateV2,
    sha256: [u8; 32],
}

#[derive(Debug)]
struct CorruptEvidence {
    path: PathBuf,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotSchema {
    Current,
    V2,
    LegacyGeometry,
}

impl SlotSchema {
    const fn preference(self) -> u8 {
        match self {
            Self::Current => 2,
            Self::V2 => 1,
            Self::LegacyGeometry => 0,
        }
    }
}

#[derive(Debug)]
struct ValidatedSlot {
    state: PersistedState,
    schema: SlotSchema,
}

#[derive(Debug)]
enum ReadSlot {
    Missing,
    Valid(ValidatedSlot),
    Corrupt {
        failure: PersistenceFailure,
        evidence: CorruptEvidence,
    },
    UnsupportedVersion(u32),
    Oversized(u64),
}

impl ReadSlot {
    const fn kind(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Valid(validated) => match validated.schema {
                SlotSchema::Current => "current",
                SlotSchema::V2 => "schema_v2",
                SlotSchema::LegacyGeometry => "legacy_geometry",
            },
            Self::Corrupt { .. } => "corrupt",
            Self::UnsupportedVersion(_) => "unsupported_version",
            Self::Oversized(_) => "oversized",
        }
    }
}

#[derive(Debug)]
struct LoadedAuthoritative {
    state: PersistedState,
    source: StoreSource,
    target: PathBuf,
    degraded_recovery: bool,
    corrupt_evidence: Option<CorruptEvidence>,
    requires_schema_upgrade: bool,
}

#[derive(Clone, Debug, Default)]
struct PendingBatch {
    window_states: BTreeMap<String, PersistedWindowState>,
    overlays: BTreeMap<LayoutWindowId, MixedDomainLayoutOverlay>,
    ensure_bindings: BTreeSet<PrivacySafeTargetFingerprint>,
    overlay_tab_count: usize,
    superseded_updates: usize,
}

impl PendingBatch {
    fn queued_updates(&self) -> usize {
        self.window_states
            .len()
            .saturating_add(self.overlays.len())
            .saturating_add(self.ensure_bindings.len())
    }

    fn queue_window_state(
        &mut self,
        workspace: String,
        state: PersistedWindowState,
    ) -> Result<EnqueueOutcome, PersistenceFailure> {
        if !self.window_states.contains_key(&workspace)
            && self.window_states.len() >= MAX_WORKSPACES
        {
            return Err(PersistenceFailure::quota(format!(
                "pending workspace count would exceed {MAX_WORKSPACES}"
            )));
        }
        match self.window_states.insert(workspace, state) {
            None => Ok(EnqueueOutcome::Queued),
            Some(previous) if previous == state => Ok(EnqueueOutcome::Unchanged),
            Some(_) => {
                self.superseded_updates = self.superseded_updates.saturating_add(1);
                Ok(EnqueueOutcome::Coalesced)
            }
        }
    }

    fn queue_overlay(
        &mut self,
        overlay: MixedDomainLayoutOverlay,
    ) -> Result<EnqueueOutcome, PersistenceFailure> {
        let previous = self.overlays.get(&overlay.window_id);
        match previous {
            None if self.overlays.len() >= MAX_LAYOUT_OVERLAYS => {
                return Err(PersistenceFailure::quota(format!(
                    "pending overlay count would exceed {MAX_LAYOUT_OVERLAYS}"
                )));
            }
            None => {
                let total = self
                    .overlay_tab_count
                    .checked_add(overlay.slots.len())
                    .ok_or_else(|| {
                        PersistenceFailure::quota("pending overlay tab count overflowed")
                    })?;
                if total > MAX_TOTAL_OVERLAY_TABS {
                    return Err(PersistenceFailure::quota(format!(
                        "pending overlay tab count {total} exceeds {MAX_TOTAL_OVERLAY_TABS}"
                    )));
                }
                self.overlay_tab_count = total;
                self.overlays.insert(overlay.window_id, overlay);
                Ok(EnqueueOutcome::Queued)
            }
            Some(previous) if previous.local_revision > overlay.local_revision => {
                Err(PersistenceFailure::StaleOverlay {
                    incoming: overlay.local_revision,
                    committed: previous.local_revision,
                })
            }
            Some(previous)
                if previous.local_revision == overlay.local_revision && previous == &overlay =>
            {
                Ok(EnqueueOutcome::Unchanged)
            }
            Some(previous) if previous.local_revision == overlay.local_revision => {
                Err(PersistenceFailure::OverlayRevisionConflict {
                    revision: overlay.local_revision,
                })
            }
            Some(previous) => {
                let total = self
                    .overlay_tab_count
                    .checked_sub(previous.slots.len())
                    .and_then(|count| count.checked_add(overlay.slots.len()))
                    .ok_or_else(|| {
                        PersistenceFailure::quota("pending overlay tab count overflowed")
                    })?;
                if total > MAX_TOTAL_OVERLAY_TABS {
                    return Err(PersistenceFailure::quota(format!(
                        "pending overlay tab count {total} exceeds {MAX_TOTAL_OVERLAY_TABS}"
                    )));
                }
                self.overlay_tab_count = total;
                self.overlays.insert(overlay.window_id, overlay);
                self.superseded_updates = self.superseded_updates.saturating_add(1);
                Ok(EnqueueOutcome::Coalesced)
            }
        }
    }

    fn acknowledge_committed(
        &mut self,
        committed: &Self,
        binding_requests_after_snapshot: &BTreeSet<PrivacySafeTargetFingerprint>,
    ) {
        for (workspace, state) in &committed.window_states {
            if self.window_states.get(workspace) == Some(state) {
                self.window_states.remove(workspace);
            }
        }
        for (window_id, overlay) in &committed.overlays {
            if self.overlays.get(window_id) == Some(overlay) {
                self.overlay_tab_count = self
                    .overlay_tab_count
                    .saturating_sub(overlay.slots.len());
                self.overlays.remove(window_id);
            }
        }
        for fingerprint in &committed.ensure_bindings {
            if !binding_requests_after_snapshot.contains(fingerprint) {
                self.ensure_bindings.remove(fingerprint);
            }
        }
        self.superseded_updates = self
            .superseded_updates
            .saturating_sub(committed.superseded_updates);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnqueueOutcome {
    Queued,
    Coalesced,
    Unchanged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitReceipt {
    pub store_revision: u64,
    pub wrote_new_generation: bool,
    pub committed_updates: usize,
    pub coalesced_updates: usize,
}

#[derive(Debug)]
struct BatchCommit {
    receipt: CommitReceipt,
    bindings: BTreeMap<PrivacySafeTargetFingerprint, DomainBindingId>,
}

pub type CommitResult = Result<CommitReceipt, PersistenceFailure>;
pub type BindingResult = Result<DomainBindingId, PersistenceFailure>;

#[derive(Default)]
struct CoordinatorPending {
    batch: PendingBatch,
    flush_waiters: Vec<flume::Sender<CommitResult>>,
    binding_waiters:
        BTreeMap<PrivacySafeTargetFingerprint, Vec<flume::Sender<BindingResult>>>,
    waiter_count: usize,
}

struct CoordinatorShared {
    primary_path: PathBuf,
    pending: Mutex<CoordinatorPending>,
}

/// Cloneable nonblocking handle to the dedicated persistence worker.
#[derive(Clone)]
pub struct PersistenceWriter {
    shared: Arc<CoordinatorShared>,
    wake: flume::Sender<()>,
}

impl PersistenceWriter {
    pub fn open(primary_path: impl Into<PathBuf>) -> Result<Self, PersistenceFailure> {
        let (wake, receiver) = flume::bounded(1);
        let shared = Arc::new(CoordinatorShared {
            primary_path: primary_path.into(),
            pending: Mutex::new(CoordinatorPending::default()),
        });
        let worker_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("ft-layout-persistence".to_string())
            .spawn(move || persistence_worker(worker_shared, receiver))
            .map_err(|error| PersistenceFailure::io("spawn persistence worker", error))?;
        Ok(Self { shared, wake })
    }

    pub fn queue_window_state(
        &self,
        workspace: impl AsRef<str>,
        state: PersistedWindowState,
    ) -> Result<EnqueueOutcome, PersistenceFailure> {
        let workspace = workspace.as_ref();
        validate_workspace(workspace)?;
        let outcome = {
            let mut pending = lock_pending(&self.shared.pending);
            pending
                .batch
                .queue_window_state(workspace.to_owned(), state)?
        };
        self.wake_worker()?;
        Ok(outcome)
    }

    pub fn queue_overlay(
        &self,
        overlay: MixedDomainLayoutOverlay,
    ) -> Result<EnqueueOutcome, PersistenceFailure> {
        validate_overlay(&overlay)?;
        let outcome = {
            let mut pending = lock_pending(&self.shared.pending);
            pending.batch.queue_overlay(overlay)?
        };
        self.wake_worker()?;
        Ok(outcome)
    }

    /// Resolve or create a stable domain binding on the storage worker.
    ///
    /// The returned receiver is intentionally asynchronous. Callers must await
    /// it outside input, parser, resize, render, and present callbacks.
    pub fn ensure_domain_binding(
        &self,
        target_fingerprint: PrivacySafeTargetFingerprint,
    ) -> Result<flume::Receiver<BindingResult>, PersistenceFailure> {
        let (sender, receiver) = flume::bounded(1);
        {
            let mut pending = lock_pending(&self.shared.pending);
            if pending.waiter_count >= MAX_PENDING_WAITERS {
                return Err(PersistenceFailure::quota(format!(
                    "pending persistence waiter count would exceed {MAX_PENDING_WAITERS}"
                )));
            }
            if !pending.batch.ensure_bindings.contains(&target_fingerprint)
                && pending.batch.ensure_bindings.len() >= MAX_DOMAIN_BINDINGS
            {
                return Err(PersistenceFailure::quota(format!(
                    "pending domain binding count would exceed {MAX_DOMAIN_BINDINGS}"
                )));
            }
            pending.batch.ensure_bindings.insert(target_fingerprint);
            pending
                .binding_waiters
                .entry(target_fingerprint)
                .or_default()
                .push(sender);
            pending.waiter_count += 1;
        }
        self.wake_worker()?;
        Ok(receiver)
    }

    /// Request an explicit commit barrier without blocking the caller.
    pub fn flush(&self) -> Result<flume::Receiver<CommitResult>, PersistenceFailure> {
        let (sender, receiver) = flume::bounded(1);
        {
            let mut pending = lock_pending(&self.shared.pending);
            if pending.waiter_count >= MAX_PENDING_WAITERS {
                return Err(PersistenceFailure::quota(format!(
                    "pending persistence waiter count would exceed {MAX_PENDING_WAITERS}"
                )));
            }
            pending.flush_waiters.push(sender);
            pending.waiter_count += 1;
        }
        self.wake_worker()?;
        Ok(receiver)
    }

    fn wake_worker(&self) -> Result<(), PersistenceFailure> {
        match self.wake.try_send(()) {
            Ok(()) | Err(flume::TrySendError::Full(())) => Ok(()),
            Err(flume::TrySendError::Disconnected(())) => {
                Err(PersistenceFailure::WorkerStopped)
            }
        }
    }
}

fn lock_pending<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::error!("window-state: recovering a poisoned coordinator mutex");
            poisoned.into_inner()
        }
    }
}

fn persistence_worker(shared: Arc<CoordinatorShared>, receiver: flume::Receiver<()>) {
    while receiver.recv().is_ok() {
        let deadline = Instant::now() + WRITE_DEBOUNCE;
        loop {
            let must_flush_now = {
                let pending = lock_pending(&shared.pending);
                !pending.flush_waiters.is_empty() || !pending.binding_waiters.is_empty()
            };
            if must_flush_now {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match receiver.recv_timeout(deadline.saturating_duration_since(now)) {
                Ok(()) => {}
                Err(flume::RecvTimeoutError::Timeout) => break,
                Err(flume::RecvTimeoutError::Disconnected) => break,
            }
        }

        let (batch, flush_waiters, binding_waiters) = {
            let mut pending = lock_pending(&shared.pending);
            let batch = pending.batch.clone();
            let flush_waiters = std::mem::take(&mut pending.flush_waiters);
            let binding_waiters = std::mem::take(&mut pending.binding_waiters);
            pending.waiter_count = 0;
            (batch, flush_waiters, binding_waiters)
        };

        let result = commit_batch(&shared.primary_path, &batch, WriteInterruption::None);
        match result {
            Ok(committed) => {
                {
                    let mut pending = lock_pending(&shared.pending);
                    let binding_requests_after_snapshot =
                        pending.binding_waiters.keys().copied().collect();
                    pending
                        .batch
                        .acknowledge_committed(&batch, &binding_requests_after_snapshot);
                }
                for (fingerprint, waiters) in binding_waiters {
                    let binding = committed.bindings.get(&fingerprint).copied().ok_or_else(|| {
                        PersistenceFailure::corrupt(
                            "binding commit succeeded without its requested identity",
                        )
                    });
                    for waiter in waiters {
                        let _ = waiter.send(binding.clone());
                    }
                }
                for waiter in flush_waiters {
                    let _ = waiter.send(Ok(committed.receipt));
                }
            }
            Err(failure) => {
                log::warn!(
                    "window-state: persistence commit rejected ({:?})",
                    failure.code()
                );
                for waiters in binding_waiters.into_values() {
                    for waiter in waiters {
                        let _ = waiter.send(Err(failure.clone()));
                    }
                }
                for waiter in flush_waiters {
                    let _ = waiter.send(Err(failure.clone()));
                }
            }
        }
    }

    let mut pending = lock_pending(&shared.pending);
    for waiters in std::mem::take(&mut pending.binding_waiters).into_values() {
        for waiter in waiters {
            let _ = waiter.send(Err(PersistenceFailure::WorkerStopped));
        }
    }
    for waiter in std::mem::take(&mut pending.flush_waiters) {
        let _ = waiter.send(Err(PersistenceFailure::WorkerStopped));
    }
    pending.waiter_count = 0;
}

fn validate_workspace(workspace: &str) -> Result<(), PersistenceFailure> {
    if workspace.is_empty() {
        return Err(PersistenceFailure::invalid("workspace is empty"));
    }
    if workspace.len() > MAX_WORKSPACE_BYTES {
        return Err(PersistenceFailure::quota(format!(
            "workspace length {} exceeds {}",
            workspace.len(),
            MAX_WORKSPACE_BYTES
        )));
    }
    if workspace.chars().any(char::is_control) {
        return Err(PersistenceFailure::invalid(
            "workspace contains control characters",
        ));
    }
    Ok(())
}

fn validate_overlay(overlay: &MixedDomainLayoutOverlay) -> Result<(), PersistenceFailure> {
    validate_workspace(&overlay.workspace)?;
    if overlay.local_revision == 0 {
        return Err(PersistenceFailure::invalid(
            "overlay revision zero is reserved",
        ));
    }
    if overlay.slots.len() > MAX_TABS_PER_OVERLAY {
        return Err(PersistenceFailure::quota(format!(
            "overlay tab count {} exceeds {}",
            overlay.slots.len(),
            MAX_TABS_PER_OVERLAY
        )));
    }
    let identities = overlay.slots.iter().copied().collect::<HashSet<_>>();
    if identities.len() != overlay.slots.len() {
        return Err(PersistenceFailure::invalid(
            "overlay contains duplicate stable tab identities",
        ));
    }
    match overlay.active {
        Some(active) if identities.contains(&active) => {}
        Some(_) => {
            return Err(PersistenceFailure::invalid(
                "overlay active identity is not a member",
            ));
        }
        None if overlay.slots.is_empty() => {}
        None => {
            return Err(PersistenceFailure::invalid(
                "non-empty overlay has no active identity",
            ));
        }
    }
    Ok(())
}

fn validate_state(state: &PersistedState) -> Result<(), PersistenceFailure> {
    if state.schema_version != STORE_SCHEMA_VERSION {
        return Err(PersistenceFailure::UnsupportedVersion {
            found: state.schema_version,
            current: STORE_SCHEMA_VERSION,
        });
    }
    if state.window_states.len() > MAX_WORKSPACES {
        return Err(PersistenceFailure::quota(format!(
            "workspace count {} exceeds {}",
            state.window_states.len(),
            MAX_WORKSPACES
        )));
    }
    for workspace in state.window_states.keys() {
        validate_workspace(workspace)?;
    }
    if state.domain_bindings.len() > MAX_DOMAIN_BINDINGS {
        return Err(PersistenceFailure::quota(format!(
            "domain binding count {} exceeds {}",
            state.domain_bindings.len(),
            MAX_DOMAIN_BINDINGS
        )));
    }
    let fingerprints = state
        .domain_bindings
        .iter()
        .map(|record| record.target_fingerprint)
        .collect::<HashSet<_>>();
    if fingerprints.len() != state.domain_bindings.len() {
        return Err(PersistenceFailure::invalid(
            "duplicate domain target fingerprint",
        ));
    }
    let binding_ids = state
        .domain_bindings
        .iter()
        .map(|record| record.binding_id)
        .collect::<HashSet<_>>();
    if binding_ids.len() != state.domain_bindings.len() {
        return Err(PersistenceFailure::invalid(
            "one domain binding identity maps to multiple targets",
        ));
    }
    if state.overlays.len() > MAX_LAYOUT_OVERLAYS {
        return Err(PersistenceFailure::quota(format!(
            "layout overlay count {} exceeds {}",
            state.overlays.len(),
            MAX_LAYOUT_OVERLAYS
        )));
    }
    let mut overlay_ids = HashSet::with_capacity(state.overlays.len());
    let mut total_tabs = 0usize;
    for overlay in &state.overlays {
        validate_overlay(overlay)?;
        if !overlay_ids.insert(overlay.window_id) {
            return Err(PersistenceFailure::invalid(
                "duplicate mixed-layout window identity",
            ));
        }
        total_tabs = total_tabs
            .checked_add(overlay.slots.len())
            .ok_or_else(|| PersistenceFailure::quota("total overlay tab count overflowed"))?;
        if total_tabs > MAX_TOTAL_OVERLAY_TABS {
            return Err(PersistenceFailure::quota(format!(
                "total overlay tab count {total_tabs} exceeds {MAX_TOTAL_OVERLAY_TABS}"
            )));
        }
        for slot in &overlay.slots {
            if let Some(binding_id) = slot.remote_binding()
                && !binding_ids.contains(&binding_id)
            {
                return Err(PersistenceFailure::invalid(
                    "remote overlay slot references an unknown domain binding",
                ));
            }
        }
    }
    Ok(())
}

fn canonicalize_state(state: &mut PersistedState) {
    state.domain_bindings.sort_by_key(|record| {
        (
            record.target_fingerprint.as_bytes(),
            record.binding_id.as_bytes(),
        )
    });
    state
        .overlays
        .sort_by_key(|overlay| overlay.window_id.as_bytes());
}

fn validate_published_state(state: &PersistedState) -> Result<(), PersistenceFailure> {
    validate_state(state)?;
    if state.store_revision == 0 {
        return Err(PersistenceFailure::invalid(
            "published current-schema state has revision zero",
        ));
    }
    Ok(())
}

fn encode_disk_slot(state: &PersistedState) -> Result<Vec<u8>, PersistenceFailure> {
    validate_published_state(state)?;
    let payload = serde_json::to_vec(state)
        .map_err(|_| PersistenceFailure::corrupt("could not serialize state payload"))?;
    let sha256: [u8; 32] = Sha256::digest(&payload).into();
    let encoded = serde_json::to_vec(&DiskSlot {
        payload: state.clone(),
        sha256,
    })
    .map_err(|_| PersistenceFailure::corrupt("could not serialize state slot"))?;
    let actual = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
    if actual > MAX_STATE_FILE_BYTES {
        return Err(PersistenceFailure::Oversized {
            actual,
            maximum: MAX_STATE_FILE_BYTES,
        });
    }
    Ok(encoded)
}

fn schema_version_probe(value: &serde_json::Value) -> Option<u32> {
    value
        .get("payload")
        .and_then(|payload| payload.get("schema_version"))
        .or_else(|| value.get("schema_version"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
}

fn read_slot(path: &Path, allow_legacy: bool) -> Result<ReadSlot, PersistenceFailure> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(ReadSlot::Missing),
        Err(error) => return Err(PersistenceFailure::io("stat state slot", error)),
    };
    if metadata.len() > MAX_STATE_FILE_BYTES {
        return Ok(ReadSlot::Oversized(metadata.len()));
    }
    let read_limit = MAX_STATE_FILE_BYTES + 1;
    let bounded_capacity = usize::try_from(metadata.len().min(MAX_STATE_FILE_BYTES))
        .unwrap_or(0);
    let mut bytes = Vec::with_capacity(bounded_capacity);
    File::open(path)
        .map_err(|error| PersistenceFailure::io("open state slot", error))?
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|error| PersistenceFailure::io("read state slot", error))?;
    let actual = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if actual > MAX_STATE_FILE_BYTES {
        return Ok(ReadSlot::Oversized(actual));
    }
    let value = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(value) => value,
        Err(_) => {
            return Ok(ReadSlot::Corrupt {
                failure: PersistenceFailure::corrupt("state slot is not valid JSON"),
                evidence: CorruptEvidence {
                    path: path.to_path_buf(),
                    bytes,
                },
            });
        }
    };

    if let Some(version) = schema_version_probe(&value) {
        if version != STORE_SCHEMA_VERSION {
            return Ok(ReadSlot::UnsupportedVersion(version));
        }
    } else if allow_legacy {
        return match serde_json::from_value::<BTreeMap<String, PersistedWindowState>>(value) {
            Ok(window_states) => {
                let state = PersistedState {
                    window_states,
                    ..PersistedState::default()
                };
                match validate_state(&state) {
                    Ok(()) => Ok(ReadSlot::Legacy(state)),
                    Err(failure) => Ok(ReadSlot::Corrupt {
                        failure,
                        evidence: CorruptEvidence {
                            path: path.to_path_buf(),
                            bytes,
                        },
                    }),
                }
            }
            Err(_) => Ok(ReadSlot::Corrupt {
                failure: PersistenceFailure::corrupt("legacy geometry map is invalid"),
                evidence: CorruptEvidence {
                    path: path.to_path_buf(),
                    bytes,
                },
            }),
        };
    } else {
        return Ok(ReadSlot::Corrupt {
            failure: PersistenceFailure::corrupt("state slot has no schema version"),
            evidence: CorruptEvidence {
                path: path.to_path_buf(),
                bytes,
            },
        });
    }

    let disk = match serde_json::from_value::<DiskSlot>(value) {
        Ok(disk) => disk,
        Err(_) => {
            return Ok(ReadSlot::Corrupt {
                failure: PersistenceFailure::corrupt("state slot schema is invalid"),
                evidence: CorruptEvidence {
                    path: path.to_path_buf(),
                    bytes,
                },
            });
        }
    };
    let payload = serde_json::to_vec(&disk.payload)
        .map_err(|_| PersistenceFailure::corrupt("could not verify state payload"))?;
    let expected: [u8; 32] = Sha256::digest(&payload).into();
    if expected != disk.sha256 {
        return Ok(ReadSlot::Corrupt {
            failure: PersistenceFailure::corrupt("state slot checksum mismatch"),
            evidence: CorruptEvidence {
                path: path.to_path_buf(),
                bytes,
            },
        });
    }
    match validate_published_state(&disk.payload) {
        Ok(()) => Ok(ReadSlot::Current(disk.payload)),
        Err(failure) => Ok(ReadSlot::Corrupt {
            failure,
            evidence: CorruptEvidence {
                path: path.to_path_buf(),
                bytes,
            },
        }),
    }
}

fn state_file_name() -> PathBuf {
    config::DATA_DIR.join("window-state.json")
}

fn shadow_file_name(primary: &Path) -> PathBuf {
    let name = primary
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("window-state.json");
    primary.with_file_name(format!("{name}.shadow"))
}

fn lock_file_name(primary: &Path) -> PathBuf {
    let name = primary
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("window-state.json");
    primary.with_file_name(format!("{name}.lock"))
}

fn open_lock_file(primary: &Path) -> Result<File, PersistenceFailure> {
    if let Some(parent) = primary.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| PersistenceFailure::io("create state directory", error))?;
    }
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_file_name(primary))
        .map_err(|error| PersistenceFailure::io("open state lock", error))
}

fn reject_blocking_slot(slot: &ReadSlot) -> Result<(), PersistenceFailure> {
    match slot {
        ReadSlot::UnsupportedVersion(found) => Err(PersistenceFailure::UnsupportedVersion {
            found: *found,
            current: STORE_SCHEMA_VERSION,
        }),
        ReadSlot::Oversized(actual) => Err(PersistenceFailure::Oversized {
            actual: *actual,
            maximum: MAX_STATE_FILE_BYTES,
        }),
        _ => Ok(()),
    }
}

fn load_authoritative_unlocked(
    primary_path: &Path,
) -> Result<LoadedAuthoritative, PersistenceFailure> {
    let shadow_path = shadow_file_name(primary_path);
    let primary = read_slot(primary_path, true)?;
    let shadow = read_slot(&shadow_path, false)?;
    reject_blocking_slot(&primary)?;
    reject_blocking_slot(&shadow)?;

    match (primary, shadow) {
        (ReadSlot::Missing, ReadSlot::Missing) => Ok(LoadedAuthoritative {
            state: PersistedState::default(),
            source: StoreSource::Empty,
            target: primary_path.to_path_buf(),
            degraded_recovery: false,
            corrupt_evidence: None,
        }),
        (ReadSlot::Legacy(state), ReadSlot::Missing) => Ok(LoadedAuthoritative {
            state,
            source: StoreSource::LegacyGeometry,
            target: shadow_path,
            degraded_recovery: false,
            corrupt_evidence: None,
        }),
        (ReadSlot::Current(state), ReadSlot::Missing) => Ok(LoadedAuthoritative {
            state,
            source: StoreSource::Primary,
            target: shadow_path,
            degraded_recovery: false,
            corrupt_evidence: None,
        }),
        (ReadSlot::Missing, ReadSlot::Current(state)) => Ok(LoadedAuthoritative {
            state,
            source: StoreSource::Shadow,
            target: primary_path.to_path_buf(),
            degraded_recovery: false,
            corrupt_evidence: None,
        }),
        (ReadSlot::Legacy(_), ReadSlot::Current(state)) => Ok(LoadedAuthoritative {
            state,
            source: StoreSource::Shadow,
            target: primary_path.to_path_buf(),
            degraded_recovery: false,
            corrupt_evidence: None,
        }),
        (ReadSlot::Current(primary), ReadSlot::Current(shadow)) => {
            if primary.store_revision > shadow.store_revision {
                Ok(LoadedAuthoritative {
                    state: primary,
                    source: StoreSource::Primary,
                    target: shadow_path,
                    degraded_recovery: false,
                    corrupt_evidence: None,
                })
            } else if shadow.store_revision > primary.store_revision {
                Ok(LoadedAuthoritative {
                    state: shadow,
                    source: StoreSource::Shadow,
                    target: primary_path.to_path_buf(),
                    degraded_recovery: false,
                    corrupt_evidence: None,
                })
            } else if primary == shadow {
                Ok(LoadedAuthoritative {
                    state: primary,
                    source: StoreSource::Primary,
                    target: shadow_path,
                    degraded_recovery: false,
                    corrupt_evidence: None,
                })
            } else {
                Err(PersistenceFailure::AmbiguousGeneration {
                    revision: primary.store_revision,
                })
            }
        }
        (
            ReadSlot::Current(state),
            ReadSlot::Corrupt {
                evidence,
                failure: _,
            },
        ) => Ok(LoadedAuthoritative {
            state,
            source: StoreSource::Primary,
            target: shadow_path,
            degraded_recovery: true,
            corrupt_evidence: Some(evidence),
        }),
        (
            ReadSlot::Corrupt {
                evidence,
                failure: _,
            },
            ReadSlot::Current(state),
        ) => {
            let target = evidence.path.clone();
            Ok(LoadedAuthoritative {
                state,
                source: StoreSource::Shadow,
                target,
                degraded_recovery: true,
                corrupt_evidence: Some(evidence),
            })
        }
        (ReadSlot::Legacy(state), ReadSlot::Corrupt { evidence, .. }) => {
            let target = evidence.path.clone();
            Ok(LoadedAuthoritative {
                state,
                source: StoreSource::LegacyGeometry,
                target,
                degraded_recovery: true,
                corrupt_evidence: Some(evidence),
            })
        }
        (
            ReadSlot::Corrupt {
                failure,
                evidence: _,
            },
            ReadSlot::Missing,
        )
        | (
            ReadSlot::Missing,
            ReadSlot::Corrupt {
                failure,
                evidence: _,
            },
        )
        | (
            ReadSlot::Corrupt {
                failure,
                evidence: _,
            },
            ReadSlot::Corrupt { .. },
        ) => Err(failure),
        (unexpected_primary, unexpected_shadow) => {
            Err(PersistenceFailure::corrupt(format!(
                "unsupported slot combination: primary={}, shadow={}",
                unexpected_primary.kind(),
                unexpected_shadow.kind()
            )))
        }
    }
}

fn load_snapshot_unlocked(
    primary_path: &Path,
) -> Result<LayoutStateSnapshot, PersistenceFailure> {
    let loaded = load_authoritative_unlocked(primary_path)?;
    Ok(LayoutStateSnapshot {
        source: loaded.source,
        degraded_recovery: loaded.degraded_recovery,
        store_revision: loaded.state.store_revision,
        window_states: loaded.state.window_states,
        domain_bindings: loaded.state.domain_bindings,
        overlays: loaded.state.overlays,
    })
}

/// Load and validate the current authority at an explicit path.
pub fn load_snapshot_at(primary_path: &Path) -> Result<LayoutStateSnapshot, PersistenceFailure> {
    let lock = open_lock_file(primary_path)?;
    fs2::FileExt::lock_shared(&lock)
        .map_err(|error| PersistenceFailure::io("lock state for reading", error))?;
    load_snapshot_unlocked(primary_path)
}

/// Load the default GUI state authority.
pub fn load_layout_state() -> Result<LayoutStateSnapshot, PersistenceFailure> {
    load_snapshot_at(&state_file_name())
}

#[derive(Debug)]
struct StartupRestoreCache {
    snapshot: Result<LayoutStateSnapshot, PersistenceFailure>,
    admitted_window_states: Mutex<BTreeMap<String, PersistedWindowState>>,
}

impl StartupRestoreCache {
    fn new(snapshot: Result<LayoutStateSnapshot, PersistenceFailure>) -> Self {
        Self {
            snapshot,
            admitted_window_states: Mutex::new(BTreeMap::new()),
        }
    }

    fn window_state(
        &self,
        workspace: &str,
    ) -> Result<Option<PersistedWindowState>, PersistenceFailure> {
        if let Some(state) = lock_pending(&self.admitted_window_states)
            .get(workspace)
            .copied()
        {
            return Ok(Some(state));
        }
        match &self.snapshot {
            Ok(snapshot) => Ok(snapshot.window_states.get(workspace).copied()),
            Err(failure) => Err(failure.clone()),
        }
    }

    fn snapshot_failure(&self) -> Option<PersistenceFailure> {
        self.snapshot.as_ref().err().cloned()
    }

    fn admit_window_state<Q>(
        &self,
        workspace: &str,
        state: PersistedWindowState,
        queue: Q,
    ) -> Result<(), PersistenceFailure>
    where
        Q: FnOnce(&str, PersistedWindowState) -> Result<(), PersistenceFailure>,
    {
        let mut admitted = lock_pending(&self.admitted_window_states);
        if !admitted.contains_key(workspace) && admitted.len() >= MAX_WORKSPACES {
            return Err(PersistenceFailure::quota(format!(
                "startup restore override count would exceed {MAX_WORKSPACES}"
            )));
        }
        // Keep admission and the in-memory mirror atomic with respect to other
        // restore-cache readers/writers. The queue closure is nonblocking and
        // only takes the persistence coordinator's pending-batch mutex; no
        // persistence path takes these locks in the opposite order.
        queue(workspace, state)?;
        admitted.insert(workspace.to_owned(), state);
        Ok(())
    }
}

fn cached_startup_snapshot<F>(
    cache: &OnceLock<StartupRestoreCache>,
    loader: F,
) -> &StartupRestoreCache
where
    F: FnOnce() -> Result<LayoutStateSnapshot, PersistenceFailure>,
{
    cache.get_or_init(|| StartupRestoreCache::new(loader()))
}

fn load_startup_workspace_from<F>(
    cache: &OnceLock<StartupRestoreCache>,
    workspace: &str,
    loader: F,
) -> Result<Option<PersistedWindowState>, PersistenceFailure>
where
    F: FnOnce() -> Result<LayoutStateSnapshot, PersistenceFailure>,
{
    validate_workspace(workspace)?;
    cached_startup_snapshot(cache, loader).window_state(workspace)
}

fn admit_window_state_and_record<Q>(
    cache: &OnceLock<StartupRestoreCache>,
    workspace: &str,
    state: PersistedWindowState,
    queue: Q,
) -> Result<(), PersistenceFailure>
where
    Q: FnOnce(&str, PersistedWindowState) -> Result<(), PersistenceFailure>,
{
    validate_workspace(workspace)?;
    match cache.get() {
        Some(cache) => cache.admit_window_state(workspace, state, queue),
        None => queue(workspace, state),
    }
}

/// The saved startup maximize/fullscreen state for `workspace`, if any.
///
/// Each process uses one pinned, validated load result. Window-state changes
/// admitted later in this process override either a valid baseline or a pinned
/// baseline failure for their exact workspace, so dynamically created windows
/// cannot regress to launch-time state. Fresh authority reads remain available
/// through [`load_layout_state`], while commit paths continue to reload under
/// the exclusive cross-process lock. External-process commits become restore
/// input on the next process startup or an explicit fresh read; they do not
/// rewrite an active restore cohort.
pub fn load_startup_for_workspace(workspace: &str) -> Option<PersistedWindowState> {
    if let Err(failure) = validate_workspace(workspace) {
        log::warn!(
            "window-state: restore ignored invalid workspace ({:?})",
            failure.code()
        );
        return None;
    }
    match cached_startup_snapshot(&STARTUP_SNAPSHOT, load_layout_state).window_state(workspace) {
        Ok(state) => state,
        // Initialization reports the pinned baseline failure once. A later
        // successfully admitted runtime update still overrides that failure
        // for its exact workspace through the same cache.
        Err(_) => None,
    }
}

fn count_retained_evidence(
    parent: &Path,
    prefix: &str,
    operation: &'static str,
) -> Result<usize, PersistenceFailure> {
    let entries =
        std::fs::read_dir(parent).map_err(|error| PersistenceFailure::io(operation, error))?;
    let mut retained = 0usize;
    for entry in entries {
        let entry = entry.map_err(|error| PersistenceFailure::io(operation, error))?;
        if entry.file_name().to_string_lossy().starts_with(prefix) {
            retained = retained.saturating_add(1);
            if retained >= MAX_CORRUPT_EVIDENCE_FILES {
                break;
            }
        }
    }
    Ok(retained)
}

fn quarantine_corrupt_evidence(evidence: &CorruptEvidence) -> Result<(), PersistenceFailure> {
    let digest = Sha256::digest(&evidence.bytes);
    let mut digest_hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut digest_hex, "{byte:02x}");
    }
    let name = evidence
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("window-state.json");
    let quarantine = evidence
        .path
        .with_file_name(format!("{name}.corrupt-{digest_hex}"));
    if quarantine.exists() {
        return Ok(());
    }
    if let Some(parent) = evidence.path.parent() {
        let prefix = format!("{name}.corrupt-");
        let retained =
            count_retained_evidence(parent, &prefix, "list corrupt-state evidence")?;
        if retained >= MAX_CORRUPT_EVIDENCE_FILES {
            return Err(PersistenceFailure::quota(format!(
                "corrupt-state evidence count reached {MAX_CORRUPT_EVIDENCE_FILES}"
            )));
        }
    }
    let mut file = match OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&quarantine)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => {
            return Err(PersistenceFailure::io(
                "create corrupt-state evidence",
                error,
            ));
        }
    };
    file.write_all(&evidence.bytes)
        .map_err(|error| PersistenceFailure::io("write corrupt-state evidence", error))?;
    file.sync_all()
        .map_err(|error| PersistenceFailure::io("sync corrupt-state evidence", error))?;
    sync_parent_directory(&quarantine)
}

fn apply_batch(
    state: &mut PersistedState,
    batch: &PendingBatch,
) -> Result<(bool, BTreeMap<PrivacySafeTargetFingerprint, DomainBindingId>), PersistenceFailure> {
    validate_state(state)?;
    let mut changed = false;
    let mut bindings = BTreeMap::new();

    for fingerprint in &batch.ensure_bindings {
        if let Some(existing) = state
            .domain_bindings
            .iter()
            .find(|record| record.target_fingerprint == *fingerprint)
            .map(|record| record.binding_id)
        {
            bindings.insert(*fingerprint, existing);
            continue;
        }
        if state.domain_bindings.len() >= MAX_DOMAIN_BINDINGS {
            return Err(PersistenceFailure::quota(format!(
                "domain binding count would exceed {MAX_DOMAIN_BINDINGS}"
            )));
        }
        let existing_ids = state
            .domain_bindings
            .iter()
            .map(|record| record.binding_id)
            .collect::<HashSet<_>>();
        let binding_id = (0..8)
            .map(|_| DomainBindingId::new())
            .find(|candidate| !existing_ids.contains(candidate))
            .ok_or_else(|| {
                PersistenceFailure::invalid("could not allocate a unique domain binding identity")
            })?;
        state.domain_bindings.push(DomainBindingRecord {
            target_fingerprint: *fingerprint,
            binding_id,
        });
        bindings.insert(*fingerprint, binding_id);
        changed = true;
    }

    for (workspace, window_state) in &batch.window_states {
        validate_workspace(workspace)?;
        match state.window_states.get(workspace) {
            Some(existing) if existing == window_state => {}
            Some(_) => {
                state.window_states.insert(workspace.clone(), *window_state);
                changed = true;
            }
            None => {
                if state.window_states.len() >= MAX_WORKSPACES {
                    return Err(PersistenceFailure::quota(format!(
                        "workspace count would exceed {MAX_WORKSPACES}"
                    )));
                }
                state.window_states.insert(workspace.clone(), *window_state);
                changed = true;
            }
        }
    }

    for overlay in batch.overlays.values() {
        validate_overlay(overlay)?;
        match state
            .overlays
            .iter()
            .position(|existing| existing.window_id == overlay.window_id)
        {
            None => {
                if state.overlays.len() >= MAX_LAYOUT_OVERLAYS {
                    return Err(PersistenceFailure::quota(format!(
                        "layout overlay count would exceed {MAX_LAYOUT_OVERLAYS}"
                    )));
                }
                state.overlays.push(overlay.clone());
                changed = true;
            }
            Some(index)
                if state.overlays[index].local_revision > overlay.local_revision =>
            {
                return Err(PersistenceFailure::StaleOverlay {
                    incoming: overlay.local_revision,
                    committed: state.overlays[index].local_revision,
                });
            }
            Some(index)
                if state.overlays[index].local_revision == overlay.local_revision
                    && state.overlays[index] == *overlay => {}
            Some(index)
                if state.overlays[index].local_revision == overlay.local_revision =>
            {
                return Err(PersistenceFailure::OverlayRevisionConflict {
                    revision: overlay.local_revision,
                });
            }
            Some(index) => {
                state.overlays[index] = overlay.clone();
                changed = true;
            }
        }
    }

    canonicalize_state(state);
    validate_state(state)?;
    Ok((changed, bindings))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WriteInterruption {
    None,
    #[cfg(test)]
    AfterTruncate,
    #[cfg(test)]
    AfterPartialWrite,
    #[cfg(test)]
    AfterFullWrite,
    #[cfg(test)]
    AfterSync,
}

fn write_inactive_slot(
    path: &Path,
    bytes: &[u8],
    interruption: WriteInterruption,
) -> Result<(), PersistenceFailure> {
    let _ = interruption;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| PersistenceFailure::io("create state directory", error))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(|error| PersistenceFailure::io("open inactive state slot", error))?;
    #[cfg(test)]
    if interruption == WriteInterruption::AfterTruncate {
        return Err(PersistenceFailure::injected_io(
            "injected crash after slot truncate",
        ));
    }
    #[cfg(test)]
    if interruption == WriteInterruption::AfterPartialWrite {
        file.write_all(&bytes[..bytes.len() / 2])
            .map_err(|error| PersistenceFailure::io("write partial state slot", error))?;
        return Err(PersistenceFailure::injected_io(
            "injected crash during slot write",
        ));
    }
    file.write_all(bytes)
        .map_err(|error| PersistenceFailure::io("write inactive state slot", error))?;
    #[cfg(test)]
    if interruption == WriteInterruption::AfterFullWrite {
        return Err(PersistenceFailure::injected_io(
            "injected crash before state-slot sync",
        ));
    }
    file.sync_all()
        .map_err(|error| PersistenceFailure::io("sync inactive state slot", error))?;
    #[cfg(test)]
    if interruption == WriteInterruption::AfterSync {
        return Err(PersistenceFailure::injected_io(
            "injected acknowledgement loss after state-slot sync",
        ));
    }
    sync_parent_directory(path)
}

fn write_initial_slot(
    path: &Path,
    bytes: &[u8],
    interruption: WriteInterruption,
) -> Result<(), PersistenceFailure> {
    let _ = interruption;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|error| PersistenceFailure::io("create state directory", error))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("window-state.json");
    let prefix = format!("{name}.initial-");
    let retained = count_retained_evidence(parent, &prefix, "list initial-state evidence")?;
    if retained >= MAX_CORRUPT_EVIDENCE_FILES {
        return Err(PersistenceFailure::quota(format!(
            "initial-state evidence count reached {MAX_CORRUPT_EVIDENCE_FILES}"
        )));
    }
    let temporary = path.with_file_name(format!("{prefix}{}", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| PersistenceFailure::io("create initial state slot", error))?;
    #[cfg(test)]
    if interruption == WriteInterruption::AfterTruncate {
        return Err(PersistenceFailure::injected_io(
            "injected crash after initial slot create",
        ));
    }
    #[cfg(test)]
    if interruption == WriteInterruption::AfterPartialWrite {
        file.write_all(&bytes[..bytes.len() / 2])
            .map_err(|error| PersistenceFailure::io("write partial initial slot", error))?;
        return Err(PersistenceFailure::injected_io(
            "injected crash during initial slot write",
        ));
    }
    file.write_all(bytes)
        .map_err(|error| PersistenceFailure::io("write initial state slot", error))?;
    #[cfg(test)]
    if interruption == WriteInterruption::AfterFullWrite {
        return Err(PersistenceFailure::injected_io(
            "injected crash before initial slot sync",
        ));
    }
    file.sync_all()
        .map_err(|error| PersistenceFailure::io("sync initial state slot", error))?;
    #[cfg(test)]
    if interruption == WriteInterruption::AfterSync {
        return Err(PersistenceFailure::injected_io(
            "injected crash before initial slot publish",
        ));
    }
    std::fs::rename(&temporary, path)
        .map_err(|error| PersistenceFailure::io("publish initial state slot", error))?;
    sync_parent_directory(path)
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<(), PersistenceFailure> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| PersistenceFailure::io("sync state directory", error))
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> Result<(), PersistenceFailure> {
    Ok(())
}

fn commit_batch(
    primary_path: &Path,
    batch: &PendingBatch,
    interruption: WriteInterruption,
) -> Result<BatchCommit, PersistenceFailure> {
    let lock = open_lock_file(primary_path)?;
    fs2::FileExt::lock_exclusive(&lock)
        .map_err(|error| PersistenceFailure::io("lock state for writing", error))?;

    let loaded = load_authoritative_unlocked(primary_path)?;
    let source = loaded.source;
    let mut state = loaded.state;
    let (changed, bindings) = apply_batch(&mut state, batch)?;
    if !changed {
        return Ok(BatchCommit {
            receipt: CommitReceipt {
                store_revision: state.store_revision,
                wrote_new_generation: false,
                committed_updates: batch.queued_updates(),
                coalesced_updates: batch.superseded_updates,
            },
            bindings,
        });
    }

    state.store_revision = state
        .store_revision
        .checked_add(1)
        .ok_or(PersistenceFailure::RevisionExhausted)?;
    canonicalize_state(&mut state);
    validate_state(&state)?;
    let encoded = encode_disk_slot(&state)?;
    if let Some(evidence) = loaded.corrupt_evidence.as_ref() {
        quarantine_corrupt_evidence(evidence)?;
    }
    if source == StoreSource::Empty {
        write_initial_slot(primary_path, &encoded, interruption)?;
    } else {
        write_inactive_slot(&loaded.target, &encoded, interruption)?;
    }
    Ok(BatchCommit {
        receipt: CommitReceipt {
            store_revision: state.store_revision,
            wrote_new_generation: true,
            committed_updates: batch.queued_updates(),
            coalesced_updates: batch.superseded_updates,
        },
        bindings,
    })
}

static GLOBAL_WRITER: OnceLock<Result<PersistenceWriter, PersistenceFailure>> = OnceLock::new();
static STARTUP_SNAPSHOT: OnceLock<StartupRestoreCache> = OnceLock::new();

fn global_writer() -> Result<&'static PersistenceWriter, PersistenceFailure> {
    match GLOBAL_WRITER.get_or_init(|| PersistenceWriter::open(state_file_name())) {
        Ok(writer) => Ok(writer),
        Err(failure) => Err(failure.clone()),
    }
}

/// Start the persistence worker and load one validated restore snapshot before
/// latency-sensitive GUI callbacks run.
///
/// A load failure is pinned for the process so one startup cannot mix restored
/// and default state across windows. Writer and snapshot initialization are
/// attempted independently and retain distinct finite diagnostics; per-window
/// lookups then remain silent and allocation-bounded.
pub fn initialize() -> Result<(), PersistenceFailure> {
    let writer_failure = global_writer().err();
    let snapshot_failure =
        cached_startup_snapshot(&STARTUP_SNAPSHOT, load_layout_state).snapshot_failure();

    match (writer_failure, snapshot_failure) {
        (None, None) => Ok(()),
        (Some(failure), None) | (None, Some(failure)) => Err(failure),
        (Some(writer_failure), Some(snapshot_failure)) => {
            // Return the restore failure to the caller and retain the distinct
            // writer failure as a finite secondary diagnostic. Both startup
            // paths were attempted, so a writer failure cannot force a later
            // GUI callback to perform the restore read.
            log::warn!(
                "window-state: persistence worker initialization also failed ({:?})",
                writer_failure.code()
            );
            Err(snapshot_failure)
        }
    }
}

/// Queue the maximize/fullscreen subset of `window_state` for `workspace`.
///
/// This function performs no filesystem access and never waits for worker
/// completion. It is therefore safe to call from the existing resize/state
/// transition callback. Failures remain observable in logs and through
/// explicit `flush`.
pub fn save_for_workspace(workspace: &str, window_state: WindowState) {
    let state = PersistedWindowState {
        maximized: window_state.contains(WindowState::MAXIMIZED),
        fullscreen: window_state.contains(WindowState::FULL_SCREEN),
    };
    let result = admit_window_state_and_record(
        &STARTUP_SNAPSHOT,
        workspace,
        state,
        |workspace, state| {
            global_writer().and_then(|writer| {
                writer.queue_window_state(workspace, state).map(|_| ())
            })
        },
    );
    if let Err(failure) = result {
        log::warn!(
            "window-state: could not enqueue geometry state ({:?})",
            failure.code()
        );
    }
}

/// Queue a validated mixed-domain layout overlay on the default worker.
pub fn queue_layout_overlay(
    overlay: MixedDomainLayoutOverlay,
) -> Result<EnqueueOutcome, PersistenceFailure> {
    global_writer()?.queue_overlay(overlay)
}

/// Request a nonblocking lifecycle barrier on the default authority.
pub fn flush() -> Result<flume::Receiver<CommitResult>, PersistenceFailure> {
    global_writer()?.flush()
}

/// Resolve or create a stable domain binding on the default storage worker.
pub fn ensure_domain_binding(
    target_fingerprint: PrivacySafeTargetFingerprint,
) -> Result<flume::Receiver<BindingResult>, PersistenceFailure> {
    global_writer()?.ensure_domain_binding(target_fingerprint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::fs::OpenOptions;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;

    fn local_slot(value: u8) -> StableTabSlot {
        StableTabSlot::local(
            StableLocalSessionId::from_bytes([0x11; 16]),
            StableLocalTabId::from_bytes([value; 16]),
        )
    }

    fn local_slots(count: usize) -> Vec<StableTabSlot> {
        (0..count)
            .map(|index| {
                let mut tab_id = [0u8; 16];
                tab_id[..8].copy_from_slice(
                    &u64::try_from(index)
                        .expect("test tab index fits u64")
                        .to_le_bytes(),
                );
                StableTabSlot::local(
                    StableLocalSessionId::from_bytes([0x12; 16]),
                    StableLocalTabId::from_bytes(tab_id),
                )
            })
            .collect()
    }

    fn remote_slot(
        binding: DomainBindingId,
        session: u8,
        window: u64,
        tab: u64,
    ) -> StableTabSlot {
        StableTabSlot::remote(
            binding,
            StableMuxSessionId::from_bytes([session; 16]),
            window,
            tab,
        )
    }

    fn commit_for_test(
        path: &Path,
        batch: &PendingBatch,
        interruption: WriteInterruption,
    ) -> Result<BatchCommit, PersistenceFailure> {
        commit_batch(path, batch, interruption)
    }

    fn ensure_binding_for_test(
        path: &Path,
        byte: u8,
    ) -> (PrivacySafeTargetFingerprint, DomainBindingId) {
        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([byte; 32]);
        let mut batch = PendingBatch::default();
        batch.ensure_bindings.insert(fingerprint);
        let result =
            commit_for_test(path, &batch, WriteInterruption::None).expect("commit binding");
        (fingerprint, result.bindings[&fingerprint])
    }

    fn snapshot_with_window_states(
        window_states: BTreeMap<String, PersistedWindowState>,
    ) -> LayoutStateSnapshot {
        LayoutStateSnapshot {
            source: StoreSource::Empty,
            degraded_recovery: false,
            store_revision: 0,
            window_states,
            domain_bindings: Vec::new(),
            overlays: Vec::new(),
        }
    }

    #[test]
    fn moved_window_refreshes_restore_state_for_current_workspace() {
        let captured = PersistedWindowState {
            maximized: true,
            fullscreen: false,
        };
        let current = PersistedWindowState {
            maximized: false,
            fullscreen: true,
        };

        assert_eq!(
            resolve_saved_window_state("workspace-a", Some(captured), Some("workspace-a"), |_| {
                panic!("unchanged workspace must keep the cohort capture")
            }),
            Some(captured)
        );
        assert_eq!(
            resolve_saved_window_state("workspace-a", Some(captured), Some("workspace-b"), |name| {
                assert_eq!(name, "workspace-b");
                Some(current)
            }),
            Some(current)
        );
        assert_eq!(
            resolve_saved_window_state("workspace-a", None, Some("workspace-b"), |_| {
                Some(current)
            }),
            Some(current),
            "a missing old-workspace state must not suppress the new workspace state"
        );
        assert_eq!(
            resolve_saved_window_state("workspace-a", Some(captured), None, |_| {
                panic!("removed mux windows have no current workspace to restore")
            }),
            None
        );
    }

    #[test]
    fn startup_snapshot_loads_once_for_multiple_workspaces() {
        let cache = OnceLock::new();
        let loads = AtomicUsize::new(0);
        let first = PersistedWindowState {
            maximized: true,
            fullscreen: false,
        };
        let second = PersistedWindowState {
            maximized: false,
            fullscreen: true,
        };
        let states = BTreeMap::from([
            ("first".to_string(), first),
            ("second".to_string(), second),
        ]);

        assert_eq!(
            load_startup_workspace_from(&cache, "first", || {
                loads.fetch_add(1, Ordering::SeqCst);
                Ok(snapshot_with_window_states(states))
            })
            .expect("load first workspace"),
            Some(first)
        );
        assert_eq!(
            load_startup_workspace_from(&cache, "second", || {
                panic!("cached startup snapshot must prevent a second load")
            })
            .expect("load second workspace"),
            Some(second)
        );
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn startup_snapshot_caches_failure_for_the_whole_restore() {
        let cache = OnceLock::new();
        let loads = AtomicUsize::new(0);
        let first = load_startup_workspace_from(&cache, "default", || {
            loads.fetch_add(1, Ordering::SeqCst);
            Err(PersistenceFailure::corrupt("test startup failure"))
        })
        .expect_err("first startup load must fail");
        let second = load_startup_workspace_from(&cache, "default", || {
            panic!("cached startup failure must prevent a second load")
        })
        .expect_err("cached startup load must fail");

        assert_eq!(first, second);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn admitted_runtime_state_overrides_a_cached_startup_failure() {
        let cache = OnceLock::new();
        let runtime = PersistedWindowState {
            maximized: false,
            fullscreen: true,
        };
        load_startup_workspace_from(&cache, "default", || {
            Err(PersistenceFailure::corrupt("test startup failure"))
        })
        .expect_err("startup baseline must fail");

        admit_window_state_and_record(&cache, "default", runtime, |_, _| Ok(()))
            .expect("admit runtime state after baseline failure");

        assert_eq!(
            load_startup_workspace_from(&cache, "default", || {
                panic!("cached failure plus override must not reread authority")
            })
            .expect("runtime override must supersede baseline failure"),
            Some(runtime)
        );
        assert!(
            load_startup_workspace_from(&cache, "other", || {
                panic!("cached baseline failure must remain pinned")
            })
            .is_err(),
            "unmodified workspaces must retain the pinned baseline failure"
        );
    }

    #[test]
    fn admitted_window_state_overrides_the_pinned_startup_snapshot() {
        let cache = OnceLock::new();
        let initial = PersistedWindowState {
            maximized: true,
            fullscreen: false,
        };
        let admitted = PersistedWindowState {
            maximized: false,
            fullscreen: true,
        };
        assert_eq!(
            load_startup_workspace_from(&cache, "default", || {
                Ok(snapshot_with_window_states(BTreeMap::from([(
                    "default".to_string(),
                    initial,
                )])))
            })
            .expect("load initial state"),
            Some(initial)
        );

        admit_window_state_and_record(&cache, "default", admitted, |_, _| Ok(()))
            .expect("record admitted runtime state");

        assert_eq!(
            load_startup_workspace_from(&cache, "default", || {
                panic!("runtime override lookup must not reread the authority")
            })
            .expect("load admitted runtime state"),
            Some(admitted)
        );
    }

    #[test]
    fn failed_window_state_admission_does_not_override_startup_snapshot() {
        let cache = OnceLock::new();
        let initial = PersistedWindowState {
            maximized: true,
            fullscreen: false,
        };
        let rejected = PersistedWindowState {
            maximized: false,
            fullscreen: true,
        };
        assert_eq!(
            load_startup_workspace_from(&cache, "default", || {
                Ok(snapshot_with_window_states(BTreeMap::from([(
                    "default".to_string(),
                    initial,
                )])))
            })
            .expect("load initial state"),
            Some(initial)
        );

        let failure = admit_window_state_and_record(
            &cache,
            "default",
            rejected,
            |_, _| Err(PersistenceFailure::WorkerStopped),
        )
        .expect_err("failed queue admission must remain a failure");
        assert_eq!(failure.code(), PersistenceFailureCode::WorkerStopped);
        assert_eq!(
            load_startup_workspace_from(&cache, "default", || {
                panic!("failed admission lookup must not reread the authority")
            })
            .expect("load state after failed admission"),
            Some(initial)
        );
    }

    #[test]
    fn override_quota_failure_does_not_enqueue_window_state() {
        let cache = OnceLock::new();
        let startup = cached_startup_snapshot(&cache, || {
            Ok(snapshot_with_window_states(BTreeMap::new()))
        });
        {
            let mut admitted = lock_pending(&startup.admitted_window_states);
            for index in 0..MAX_WORKSPACES {
                admitted.insert(
                    format!("workspace-{index}"),
                    PersistedWindowState::default(),
                );
            }
        }
        let queue_calls = AtomicUsize::new(0);

        let failure = admit_window_state_and_record(
            &cache,
            "one-workspace-too-many",
            PersistedWindowState {
                maximized: true,
                fullscreen: false,
            },
            |_, _| {
                queue_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .expect_err("cache quota must reject before queue admission");

        assert_eq!(failure.code(), PersistenceFailureCode::Quota);
        assert_eq!(queue_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            lock_pending(&startup.admitted_window_states).len(),
            MAX_WORKSPACES
        );
    }

    #[test]
    fn reconciliation_cohort_keeps_one_value_across_runtime_updates() {
        let cache = OnceLock::new();
        let launch = PersistedWindowState {
            maximized: true,
            fullscreen: false,
        };
        let runtime = PersistedWindowState {
            maximized: false,
            fullscreen: true,
        };
        let cohort_value = load_startup_workspace_from(&cache, "default", || {
            Ok(snapshot_with_window_states(BTreeMap::from([(
                "default".to_string(),
                launch,
            )])))
        })
        .expect("capture reconciliation cohort state");

        admit_window_state_and_record(&cache, "default", runtime, |_, _| Ok(()))
            .expect("admit runtime update");

        assert_eq!(cohort_value, Some(launch));
        assert_eq!(
            load_startup_workspace_from(&cache, "default", || {
                panic!("runtime lookup must not reread the authority")
            })
            .expect("load post-cohort runtime state"),
            Some(runtime)
        );
    }

    #[test]
    fn concurrent_startup_snapshot_callers_initialize_once() {
        let cache = Arc::new(OnceLock::new());
        let loads = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(8));
        let handles = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let loads = Arc::clone(&loads);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    load_startup_workspace_from(&cache, "default", || {
                        loads.fetch_add(1, Ordering::SeqCst);
                        Ok(snapshot_with_window_states(BTreeMap::new()))
                    })
                    .expect("concurrent startup load")
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            assert_eq!(handle.join().expect("startup loader thread"), None);
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn invalid_workspace_does_not_initialize_startup_snapshot() {
        let cache = OnceLock::new();
        let loads = AtomicUsize::new(0);
        let failure = load_startup_workspace_from(&cache, "", || {
            loads.fetch_add(1, Ordering::SeqCst);
            Ok(snapshot_with_window_states(BTreeMap::new()))
        })
        .expect_err("empty workspace must fail");

        assert_eq!(failure.code(), PersistenceFailureCode::Invalid);
        assert_eq!(loads.load(Ordering::SeqCst), 0);
        assert!(cache.get().is_none());
    }

    #[test]
    fn startup_snapshot_remains_pinned_after_authority_changes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let mut first_batch = PendingBatch::default();
        first_batch
            .queue_window_state(
                "default".to_string(),
                PersistedWindowState {
                    maximized: true,
                    fullscreen: false,
                },
            )
            .expect("queue first state");
        commit_for_test(&path, &first_batch, WriteInterruption::None)
            .expect("commit first state");

        let cache = OnceLock::new();
        let first = load_startup_workspace_from(&cache, "default", || {
            load_snapshot_at(&path)
        })
        .expect("load pinned snapshot");

        let mut second_batch = PendingBatch::default();
        second_batch
            .queue_window_state(
                "default".to_string(),
                PersistedWindowState {
                    maximized: false,
                    fullscreen: true,
                },
            )
            .expect("queue second state");
        commit_for_test(&path, &second_batch, WriteInterruption::None)
            .expect("commit second state");
        let pinned = load_startup_workspace_from(&cache, "default", || {
            panic!("pinned startup snapshot must not reread the authority")
        })
        .expect("load pinned state after commit");

        assert_eq!(first, pinned);
        assert_eq!(
            pinned,
            Some(PersistedWindowState {
                maximized: true,
                fullscreen: false,
            })
        );
        assert_eq!(
            load_snapshot_at(&path)
                .expect("load fresh authority")
                .window_states["default"],
            PersistedWindowState {
                maximized: false,
                fullscreen: true,
            }
        );
    }

    #[test]
    fn extracts_only_maximize_and_fullscreen_bits() {
        let entry = PersistedWindowState {
            maximized: WindowState::MAXIMIZED.contains(WindowState::MAXIMIZED),
            fullscreen: WindowState::MAXIMIZED.contains(WindowState::FULL_SCREEN),
        };
        assert!(entry.maximized);
        assert!(!entry.fullscreen);
    }

    #[test]
    fn legacy_geometry_map_migrates_without_losing_bits() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let legacy = BTreeMap::from([(
            "default".to_string(),
            PersistedWindowState {
                maximized: true,
                fullscreen: false,
            },
        )]);
        std::fs::write(&path, serde_json::to_vec(&legacy).expect("legacy JSON"))
            .expect("write legacy");

        let before = load_snapshot_at(&path).expect("load legacy");
        assert_eq!(before.source, StoreSource::LegacyGeometry);
        assert!(before.window_states["default"].maximized);

        let mut batch = PendingBatch::default();
        batch.queue_window_state(
            "other".to_string(),
            PersistedWindowState {
                maximized: false,
                fullscreen: true,
            },
        )
        .expect("queue migrated state");
        commit_for_test(&path, &batch, WriteInterruption::None).expect("migrate");

        let after = load_snapshot_at(&path).expect("load migrated");
        assert_eq!(after.store_revision, 1);
        assert!(after.window_states["default"].maximized);
        assert!(after.window_states["other"].fullscreen);
    }

    #[test]
    fn overlay_and_binding_roundtrip_through_validated_slots() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let (_, binding) = ensure_binding_for_test(&path, 0x22);
        let remote = remote_slot(binding, 0x33, 7, 9);
        let local = local_slot(0x44);
        let overlay = MixedDomainLayoutOverlay::new(
            LayoutWindowId::from_bytes([0x55; 16]),
            "default",
            1,
            vec![local, remote],
            Some(remote),
        )
        .expect("valid overlay");
        let mut batch = PendingBatch::default();
        batch.queue_overlay(overlay.clone()).expect("queue overlay");
        commit_for_test(&path, &batch, WriteInterruption::None).expect("commit overlay");

        let snapshot = load_snapshot_at(&path).expect("load current");
        assert_eq!(snapshot.overlays, vec![overlay]);
        assert_eq!(snapshot.domain_bindings.len(), 1);
        assert_eq!(snapshot.store_revision, 2);
    }

    #[test]
    fn rapid_overlay_updates_coalesce_to_latest_revision() {
        let window = LayoutWindowId::from_bytes([0x61; 16]);
        let mut batch = PendingBatch::default();
        for revision in 1..=64 {
            let slot = local_slot(u8::try_from(revision).expect("bounded revision"));
            let overlay =
                MixedDomainLayoutOverlay::new(window, "default", revision, vec![slot], Some(slot))
                    .expect("valid overlay");
            batch.queue_overlay(overlay).expect("monotonic update");
        }
        assert_eq!(batch.overlays.len(), 1);
        assert_eq!(batch.overlays[&window].local_revision, 64);
        assert_eq!(batch.superseded_updates, 63);
    }

    #[test]
    fn equal_revision_with_different_content_is_rejected() {
        let window = LayoutWindowId::from_bytes([0x62; 16]);
        let first = local_slot(1);
        let second = local_slot(2);
        let mut batch = PendingBatch::default();
        batch
            .queue_overlay(
                MixedDomainLayoutOverlay::new(
                    window,
                    "default",
                    1,
                    vec![first],
                    Some(first),
                )
                .expect("first"),
            )
            .expect("queue first");
        let failure = batch
            .queue_overlay(
                MixedDomainLayoutOverlay::new(
                    window,
                    "default",
                    1,
                    vec![second],
                    Some(second),
                )
                .expect("second"),
            )
            .expect_err("revision reuse must fail");
        assert_eq!(
            failure.code(),
            PersistenceFailureCode::OverlayRevisionConflict
        );
    }

    #[test]
    fn commit_acknowledgement_retains_updates_queued_while_io_was_in_flight() {
        let window = LayoutWindowId::from_bytes([0x63; 16]);
        let first_slot = local_slot(1);
        let second_slot = local_slot(2);
        let mut pending = PendingBatch::default();
        pending
            .queue_window_state(
                "default".to_string(),
                PersistedWindowState {
                    maximized: true,
                    fullscreen: false,
                },
            )
            .expect("queue first geometry");
        pending
            .queue_overlay(
                MixedDomainLayoutOverlay::new(
                    window,
                    "default",
                    1,
                    vec![first_slot],
                    Some(first_slot),
                )
                .expect("first overlay"),
            )
            .expect("queue first overlay");
        let committed_snapshot = pending.clone();

        pending
            .queue_window_state(
                "default".to_string(),
                PersistedWindowState {
                    maximized: false,
                    fullscreen: true,
                },
            )
            .expect("queue newer geometry");
        pending
            .queue_overlay(
                MixedDomainLayoutOverlay::new(
                    window,
                    "default",
                    2,
                    vec![second_slot],
                    Some(second_slot),
                )
                .expect("second overlay"),
            )
            .expect("queue newer overlay");
        pending.acknowledge_committed(&committed_snapshot, &BTreeSet::new());

        assert!(pending.window_states["default"].fullscreen);
        assert_eq!(pending.overlays[&window].local_revision, 2);
        assert_eq!(pending.overlay_tab_count, 1);
    }

    #[test]
    fn binding_acknowledgement_preserves_post_snapshot_waiters() {
        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x64; 32]);
        let mut pending = PendingBatch::default();
        pending.ensure_bindings.insert(fingerprint);
        let committed_snapshot = pending.clone();

        pending.acknowledge_committed(
            &committed_snapshot,
            &BTreeSet::from([fingerprint]),
        );
        assert!(pending.ensure_bindings.contains(&fingerprint));

        pending.acknowledge_committed(&committed_snapshot, &BTreeSet::new());
        assert!(!pending.ensure_bindings.contains(&fingerprint));
    }

    #[test]
    fn pending_overlay_tab_quota_is_enforced_before_growth() {
        let mut pending = PendingBatch::default();
        for window_number in 1u8..=4 {
            let slots = local_slots(MAX_TABS_PER_OVERLAY);
            let active = slots.first().copied();
            pending
                .queue_overlay(
                    MixedDomainLayoutOverlay::new(
                        LayoutWindowId::from_bytes([window_number; 16]),
                        "default",
                        1,
                        slots,
                        active,
                    )
                    .expect("maximum-sized overlay"),
                )
                .expect("overlay within aggregate quota");
        }
        assert_eq!(pending.overlay_tab_count, MAX_TOTAL_OVERLAY_TABS);

        let slot = local_slot(0xff);
        let failure = pending
            .queue_overlay(
                MixedDomainLayoutOverlay::new(
                    LayoutWindowId::from_bytes([0x65; 16]),
                    "default",
                    1,
                    vec![slot],
                    Some(slot),
                )
                .expect("small overlay"),
            )
            .expect_err("aggregate quota must reject growth");
        assert_eq!(failure.code(), PersistenceFailureCode::Quota);
        assert_eq!(pending.overlay_tab_count, MAX_TOTAL_OVERLAY_TABS);
        assert_eq!(pending.overlays.len(), 4);
    }

    #[test]
    fn unavailable_remote_slots_keep_position_but_never_become_active() {
        let unavailable = DomainBindingId::from_bytes([0x71; 16]);
        let available = DomainBindingId::from_bytes([0x72; 16]);
        let left = remote_slot(available, 1, 3, 10);
        let missing_active = remote_slot(unavailable, 2, 4, 20);
        let right = remote_slot(available, 1, 3, 30);
        let unseen = remote_slot(available, 1, 3, 40);
        let overlay = MixedDomainLayoutOverlay::new(
            LayoutWindowId::from_bytes([0x73; 16]),
            "default",
            1,
            vec![left, missing_active, right],
            Some(missing_active),
        )
        .expect("overlay");
        let unavailable_bindings = BTreeSet::from([unavailable]);
        let reconciled =
            reconcile_overlay(&overlay, &[right, left, unseen], &unavailable_bindings)
                .expect("reconcile");

        assert_eq!(
            reconciled.ordered_slots,
            vec![left, missing_active, right, unseen]
        );
        assert_eq!(reconciled.live_slots, vec![left, right, unseen]);
        assert_eq!(reconciled.active_live_slot, Some(right));
        assert_eq!(reconciled.retained_unavailable, 1);
        assert_eq!(reconciled.appended_new, 1);
    }

    #[test]
    fn live_remote_slot_cannot_come_from_an_unavailable_binding() {
        let unavailable = DomainBindingId::from_bytes([0x74; 16]);
        let slot = remote_slot(unavailable, 1, 1, 1);
        let overlay = MixedDomainLayoutOverlay::new(
            LayoutWindowId::from_bytes([0x75; 16]),
            "default",
            1,
            vec![slot],
            Some(slot),
        )
        .expect("overlay");

        let failure =
            reconcile_overlay(&overlay, &[slot], &BTreeSet::from([unavailable]))
                .expect_err("conflicting availability must fail closed");
        assert_eq!(failure.code(), PersistenceFailureCode::Invalid);
    }

    #[test]
    fn unavailable_remote_slot_reappears_in_its_persisted_position() {
        let unavailable = DomainBindingId::from_bytes([0x76; 16]);
        let available = DomainBindingId::from_bytes([0x77; 16]);
        let left = remote_slot(available, 1, 1, 1);
        let missing_active = remote_slot(unavailable, 2, 2, 2);
        let right = remote_slot(available, 1, 1, 3);
        let overlay = MixedDomainLayoutOverlay::new(
            LayoutWindowId::from_bytes([0x78; 16]),
            "default",
            1,
            vec![left, missing_active, right],
            Some(missing_active),
        )
        .expect("overlay");

        let disconnected = reconcile_overlay(
            &overlay,
            &[right, left],
            &BTreeSet::from([unavailable]),
        )
        .expect("retain unavailable placeholder");
        assert_eq!(
            disconnected.ordered_slots,
            vec![left, missing_active, right]
        );
        assert_eq!(disconnected.active_live_slot, Some(right));

        let reappeared =
            reconcile_overlay(&overlay, &[right, missing_active, left], &BTreeSet::new())
                .expect("reconcile reappeared binding");
        assert_eq!(
            reappeared.ordered_slots,
            vec![left, missing_active, right]
        );
        assert_eq!(reappeared.active_live_slot, Some(missing_active));
        assert_eq!(reappeared.retained_unavailable, 0);
        assert_eq!(reappeared.appended_new, 0);
    }

    #[test]
    fn new_server_incarnation_cannot_reuse_old_numeric_tab_identity() {
        let binding = DomainBindingId::from_bytes([0x81; 16]);
        let old = remote_slot(binding, 1, 9, 99);
        let replacement = remote_slot(binding, 2, 9, 99);
        let overlay = MixedDomainLayoutOverlay::new(
            LayoutWindowId::from_bytes([0x82; 16]),
            "default",
            1,
            vec![old],
            Some(old),
        )
        .expect("overlay");
        let reconciled =
            reconcile_overlay(&overlay, &[replacement], &BTreeSet::new()).expect("reconcile");
        assert_eq!(reconciled.ordered_slots, vec![replacement]);
        assert_eq!(reconciled.active_live_slot, Some(replacement));
        assert_eq!(reconciled.dropped_closed_or_stale, 1);
        assert_eq!(reconciled.appended_new, 1);
    }

    #[test]
    fn partial_write_recovers_prior_generation_and_synced_ack_loss_recovers_new() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let mut first = PendingBatch::default();
        first.queue_window_state(
            "default".to_string(),
            PersistedWindowState {
                maximized: true,
                fullscreen: false,
            },
        )
        .expect("queue first state");
        commit_for_test(&path, &first, WriteInterruption::None).expect("first commit");

        let mut second = PendingBatch::default();
        second.queue_window_state(
            "default".to_string(),
            PersistedWindowState {
                maximized: false,
                fullscreen: true,
            },
        )
        .expect("queue second state");
        commit_for_test(&path, &second, WriteInterruption::AfterPartialWrite)
            .expect_err("injected partial write");
        let after_partial = load_snapshot_at(&path).expect("recover old");
        assert!(after_partial.window_states["default"].maximized);
        assert!(!after_partial.window_states["default"].fullscreen);

        commit_for_test(&path, &second, WriteInterruption::None).expect("retry second");
        let mut third = PendingBatch::default();
        third.queue_window_state(
            "default".to_string(),
            PersistedWindowState {
                maximized: true,
                fullscreen: true,
            },
        )
        .expect("queue third state");
        commit_for_test(&path, &third, WriteInterruption::AfterSync)
            .expect_err("injected acknowledgement loss");
        let after_sync = load_snapshot_at(&path).expect("recover new");
        assert!(after_sync.window_states["default"].maximized);
        assert!(after_sync.window_states["default"].fullscreen);

        let replay =
            commit_for_test(&path, &third, WriteInterruption::None).expect("idempotent retry");
        assert!(!replay.receipt.wrote_new_generation);
    }

    #[test]
    fn truncate_or_unsynced_full_write_recovers_one_complete_generation() {
        for interruption in [
            WriteInterruption::AfterTruncate,
            WriteInterruption::AfterFullWrite,
        ] {
            let temp = tempfile::tempdir().expect("tempdir");
            let path = temp.path().join("window-state.json");
            let mut first = PendingBatch::default();
            first
                .queue_window_state(
                    "default".to_string(),
                    PersistedWindowState {
                        maximized: true,
                        fullscreen: false,
                    },
                )
                .expect("queue first state");
            commit_for_test(&path, &first, WriteInterruption::None).expect("first commit");

            let mut second = PendingBatch::default();
            second
                .queue_window_state(
                    "default".to_string(),
                    PersistedWindowState {
                        maximized: false,
                        fullscreen: true,
                    },
                )
                .expect("queue second state");
            commit_for_test(&path, &second, interruption)
                .expect_err("injected incomplete durability");

            let recovered = load_snapshot_at(&path).expect("recover complete generation");
            let state = recovered.window_states["default"];
            assert!(
                (state.maximized && !state.fullscreen)
                    || (!state.maximized && state.fullscreen),
                "recovery must select the prior or fully encoded generation"
            );
        }
    }

    #[test]
    fn interrupted_first_publish_retains_the_empty_authority() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let mut first = PendingBatch::default();
        first.queue_window_state(
            "default".to_string(),
            PersistedWindowState {
                maximized: true,
                fullscreen: false,
            },
        )
        .expect("queue first state");
        commit_for_test(&path, &first, WriteInterruption::AfterPartialWrite)
            .expect_err("injected first partial write");

        let snapshot = load_snapshot_at(&path).expect("empty authority survives");
        assert_eq!(snapshot.source, StoreSource::Empty);
        assert!(snapshot.window_states.is_empty());

        commit_for_test(&path, &first, WriteInterruption::None).expect("retry first publish");
        assert!(load_snapshot_at(&path)
            .expect("load committed first generation")
            .window_states["default"]
            .maximized);
    }

    #[test]
    fn corrupt_inactive_slot_is_quarantined_before_reuse() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let mut first = PendingBatch::default();
        first.queue_window_state(
            "default".to_string(),
            PersistedWindowState {
                maximized: true,
                fullscreen: false,
            },
        )
        .expect("queue first state");
        commit_for_test(&path, &first, WriteInterruption::None).expect("first");
        std::fs::write(shadow_file_name(&path), b"truncated").expect("corrupt inactive shadow");

        let mut second = PendingBatch::default();
        second.queue_window_state(
            "default".to_string(),
            PersistedWindowState {
                maximized: false,
                fullscreen: true,
            },
        )
        .expect("queue second state");
        commit_for_test(&path, &second, WriteInterruption::None).expect("recover and commit");
        let quarantines = std::fs::read_dir(temp.path())
            .expect("list directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".corrupt-"))
            .count();
        assert_eq!(quarantines, 1);
        assert!(load_snapshot_at(&path)
            .expect("load recovered")
            .window_states["default"]
            .fullscreen);
    }

    #[test]
    fn future_and_oversized_state_fail_closed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let future_path = temp.path().join("future.json");
        let future = serde_json::json!({
            "payload": {
                "schema_version": STORE_SCHEMA_VERSION + 1,
                "store_revision": 1,
                "window_states": {},
                "domain_bindings": [],
                "overlays": []
            },
            "sha256": vec![0; 32]
        });
        std::fs::write(
            &future_path,
            serde_json::to_vec(&future).expect("future JSON"),
        )
        .expect("write future");
        assert_eq!(
            load_snapshot_at(&future_path)
                .expect_err("future must fail")
                .code(),
            PersistenceFailureCode::UnsupportedVersion
        );

        let oversized_path = temp.path().join("oversized.json");
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&oversized_path)
            .expect("create oversized");
        file.set_len(MAX_STATE_FILE_BYTES + 1)
            .expect("extend oversized");
        assert_eq!(
            load_snapshot_at(&oversized_path)
                .expect_err("oversized must fail")
                .code(),
            PersistenceFailureCode::Oversized
        );
    }

    #[test]
    fn published_current_schema_revision_zero_fails_closed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("revision-zero.json");
        let state = PersistedState::default();
        let payload = serde_json::to_vec(&state).expect("serialize payload");
        let sha256: [u8; 32] = Sha256::digest(&payload).into();
        let slot = DiskSlot {
            payload: state,
            sha256,
        };
        std::fs::write(
            &path,
            serde_json::to_vec(&slot).expect("serialize current-schema slot"),
        )
        .expect("write revision-zero slot");

        assert_eq!(
            load_snapshot_at(&path)
                .expect_err("published revision zero must fail")
                .code(),
            PersistenceFailureCode::Invalid
        );
    }

    #[test]
    fn malformed_overlay_cannot_mutate_prior_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let mut first = PendingBatch::default();
        first.queue_window_state(
            "default".to_string(),
            PersistedWindowState {
                maximized: true,
                fullscreen: false,
            },
        )
        .expect("queue first state");
        commit_for_test(&path, &first, WriteInterruption::None).expect("first");
        let before = load_snapshot_at(&path).expect("before");

        let slot = local_slot(1);
        let malformed = MixedDomainLayoutOverlay {
            window_id: LayoutWindowId::from_bytes([0x91; 16]),
            workspace: "default".to_string(),
            local_revision: 1,
            slots: vec![slot, slot],
            active: Some(slot),
        };
        let mut batch = PendingBatch::default();
        batch.overlays.insert(malformed.window_id, malformed);
        assert_eq!(
            commit_for_test(&path, &batch, WriteInterruption::None)
                .expect_err("malformed must fail")
                .code(),
            PersistenceFailureCode::Invalid
        );
        assert_eq!(load_snapshot_at(&path).expect("after"), before);
    }

    #[test]
    fn stale_overlay_cannot_mutate_committed_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = LayoutWindowId::from_bytes([0x92; 16]);
        let current_slot = local_slot(2);
        let mut current = PendingBatch::default();
        current
            .queue_overlay(
                MixedDomainLayoutOverlay::new(
                    window,
                    "default",
                    2,
                    vec![current_slot],
                    Some(current_slot),
                )
                .expect("current overlay"),
            )
            .expect("queue current overlay");
        commit_for_test(&path, &current, WriteInterruption::None).expect("commit current");
        let before = load_snapshot_at(&path).expect("load current");

        let stale_slot = local_slot(1);
        let mut stale = PendingBatch::default();
        stale
            .queue_overlay(
                MixedDomainLayoutOverlay::new(
                    window,
                    "default",
                    1,
                    vec![stale_slot],
                    Some(stale_slot),
                )
                .expect("stale overlay"),
            )
            .expect("queue stale overlay");
        assert_eq!(
            commit_for_test(&path, &stale, WriteInterruption::None)
                .expect_err("stale overlay must fail")
                .code(),
            PersistenceFailureCode::StaleOverlay
        );
        assert_eq!(load_snapshot_at(&path).expect("load after rejection"), before);
    }

    #[test]
    fn asynchronous_writer_coalesces_and_flushes_latest_geometry() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let writer = PersistenceWriter::open(path.clone()).expect("writer");
        for index in 0..256 {
            writer
                .queue_window_state(
                    "default",
                    PersistedWindowState {
                        maximized: index % 2 == 0,
                        fullscreen: index == 255,
                    },
                )
                .expect("queue");
        }
        let receipt = writer
            .flush()
            .expect("flush receiver")
            .recv_timeout(Duration::from_secs(5))
            .expect("flush response")
            .expect("flush success");
        assert!(receipt.coalesced_updates > 0);
        let snapshot = load_snapshot_at(&path).expect("load");
        assert!(!snapshot.window_states["default"].maximized);
        assert!(snapshot.window_states["default"].fullscreen);
    }

    #[test]
    fn domain_binding_is_stable_across_writer_restart() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0xa1; 32]);
        let first_writer = PersistenceWriter::open(path.clone()).expect("first writer");
        let first = first_writer
            .ensure_domain_binding(fingerprint)
            .expect("first receiver")
            .recv_timeout(Duration::from_secs(5))
            .expect("first response")
            .expect("first binding");
        drop(first_writer);

        let second_writer = PersistenceWriter::open(path).expect("second writer");
        let second = second_writer
            .ensure_domain_binding(fingerprint)
            .expect("second receiver")
            .recv_timeout(Duration::from_secs(5))
            .expect("second response")
            .expect("second binding");
        assert_eq!(first, second);
    }

    #[test]
    fn concurrent_writers_preserve_disjoint_updates() {
        use std::sync::Barrier;

        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for (workspace, maximized) in [("left", true), ("right", false)] {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let writer = PersistenceWriter::open(path).expect("writer");
                writer
                    .queue_window_state(
                        workspace,
                        PersistedWindowState {
                            maximized,
                            fullscreen: !maximized,
                        },
                    )
                    .expect("queue disjoint state");
                barrier.wait();
                writer
                    .flush()
                    .expect("flush receiver")
                    .recv_timeout(Duration::from_secs(10))
                    .expect("flush response")
                    .expect("flush success");
            }));
        }
        barrier.wait();
        for handle in handles {
            handle.join().expect("writer thread");
        }

        let snapshot = load_snapshot_at(&path).expect("load concurrent result");
        assert!(snapshot.window_states["left"].maximized);
        assert!(!snapshot.window_states["left"].fullscreen);
        assert!(!snapshot.window_states["right"].maximized);
        assert!(snapshot.window_states["right"].fullscreen);
    }

    #[test]
    fn serialized_state_is_privacy_minimal() {
        let mut state = PersistedState::default();
        state.store_revision = 1;
        state.domain_bindings.push(DomainBindingRecord {
            target_fingerprint: PrivacySafeTargetFingerprint::from_bytes([0xb1; 32]),
            binding_id: DomainBindingId::from_bytes([0xb2; 16]),
        });
        let slot = remote_slot(state.domain_bindings[0].binding_id, 0xb3, 1, 2);
        state.overlays.push(
            MixedDomainLayoutOverlay::new(
                LayoutWindowId::from_bytes([0xb4; 16]),
                "default",
                1,
                vec![slot],
                Some(slot),
            )
            .expect("overlay"),
        );
        let encoded = String::from_utf8(encode_disk_slot(&state).expect("encode"))
            .expect("JSON is UTF-8");
        for forbidden in [
            "terminal_contents",
            "command",
            "cwd",
            "environment",
            "credential",
            "hostname",
            "username",
            "socket_path",
            "tab_title",
        ] {
            assert!(!encoded.contains(forbidden), "forbidden field {forbidden}");
        }
    }

    #[test]
    fn corrupt_field_names_are_not_reflected_in_diagnostics() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let secret_marker = "credential_value_that_must_not_escape";
        let malformed = serde_json::json!({
            "payload": {
                "schema_version": STORE_SCHEMA_VERSION,
                "store_revision": 1,
                "window_states": {},
                "domain_bindings": [],
                "overlays": [],
                "credential_value_that_must_not_escape": "sensitive"
            },
            "sha256": vec![0; 32]
        });
        std::fs::write(
            &path,
            serde_json::to_vec(&malformed).expect("malformed JSON"),
        )
        .expect("write malformed slot");

        let failure = load_snapshot_at(&path).expect_err("unknown field must fail closed");
        assert_eq!(failure.code(), PersistenceFailureCode::Corrupt);
        assert!(!failure.to_string().contains(secret_marker));
    }

    proptest! {
        #[test]
        fn overlay_json_roundtrip_preserves_exact_order_and_active_identity(
            mut tab_bytes in prop::collection::vec(any::<u8>(), 0..64),
            active_seed in any::<usize>(),
        ) {
            tab_bytes.sort_unstable();
            tab_bytes.dedup();
            let slots = tab_bytes.iter().copied().map(local_slot).collect::<Vec<_>>();
            let active = if slots.is_empty() {
                None
            } else {
                Some(slots[active_seed % slots.len()])
            };
            let overlay = MixedDomainLayoutOverlay::new(
                LayoutWindowId::from_bytes([0xc1; 16]),
                "property",
                1,
                slots,
                active,
            ).expect("generated overlay");
            let encoded = serde_json::to_vec(&overlay).expect("serialize");
            let decoded: MixedDomainLayoutOverlay =
                serde_json::from_slice(&encoded).expect("deserialize");
            prop_assert_eq!(decoded, overlay);
        }
    }
}
