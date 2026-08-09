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

use frankenterm_sigpipe::{RecoverablePanicSite, catch_recoverable};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
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
    WorkerPanicked,
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
    #[error("persisted state encoded-byte upper bound {projected_upper_bound} exceeds {maximum}")]
    EncodedQuota {
        projected_upper_bound: u64,
        maximum: u64,
    },
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
    #[error("persistence worker recovered from an internal panic")]
    WorkerPanicked,
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
            Self::Oversized { .. } | Self::EncodedQuota { .. } => PersistenceFailureCode::Oversized,
            Self::Invalid { .. } => PersistenceFailureCode::Invalid,
            Self::Quota { .. } => PersistenceFailureCode::Quota,
            Self::RevisionExhausted => PersistenceFailureCode::RevisionExhausted,
            Self::StaleOverlay { .. } => PersistenceFailureCode::StaleOverlay,
            Self::OverlayRevisionConflict { .. } => PersistenceFailureCode::OverlayRevisionConflict,
            Self::OverlayCasConflict { .. } => PersistenceFailureCode::OverlayCasConflict,
            Self::RetiredOverlay { .. } => PersistenceFailureCode::RetiredOverlay,
            Self::AmbiguousGeneration { .. } => PersistenceFailureCode::AmbiguousGeneration,
            Self::WorkerPanicked => PersistenceFailureCode::WorkerPanicked,
            Self::WorkerStopped => PersistenceFailureCode::WorkerStopped,
        }
    }

    const fn may_have_published_generation(&self) -> bool {
        matches!(self, Self::Io { .. } | Self::WorkerPanicked)
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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

/// Provenance-bound identity of one tab, independent of its current window.
///
/// A remote tab keeps this identity when the server atomically moves it to a
/// different window.  The parent window is routing/placement state carried by
/// [`StableTabSlot`], not part of equality or ownership authority here.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StableTabIdentity {
    Remote {
        binding_id: DomainBindingId,
        session_id: StableMuxSessionId,
        remote_tab_id: u64,
    },
    Local {
        session_id: StableLocalSessionId,
        tab_id: StableLocalTabId,
    },
}

/// Server-scoped tab authority used to reject the same incarnation/tab pair
/// being aliased through two client bindings.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RemoteTabAuthority {
    session_id: StableMuxSessionId,
    remote_tab_id: u64,
}

/// Provenance-bound placement of one tab occupying a mixed GUI layout.
///
/// The serialized remote window remains available for routing, while
/// [`Self::identity`] deliberately excludes that mutable parent.
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

    /// Stable ownership/reconciliation key for this placement.
    #[must_use]
    pub const fn identity(self) -> StableTabIdentity {
        match self {
            Self::Remote {
                binding_id,
                session_id,
                remote_tab_id,
                ..
            } => StableTabIdentity::Remote {
                binding_id,
                session_id,
                remote_tab_id,
            },
            Self::Local { session_id, tab_id } => StableTabIdentity::Local { session_id, tab_id },
        }
    }

    const fn remote_authority(self) -> Option<RemoteTabAuthority> {
        match self {
            Self::Remote {
                session_id,
                remote_tab_id,
                ..
            } => Some(RemoteTabAuthority {
                session_id,
                remote_tab_id,
            }),
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
/// [`LayoutWindowId`] is never reusable and its tombstone is never pruned. The
/// hard cap therefore fails a new distinct retirement closed before removing
/// its live overlay; safe reuse beyond that cap requires a future durable
/// identity-generation scheme rather than lossy eviction.
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
        if base_revision == Some(0) {
            return Err(PersistenceFailure::invalid(
                "overlay base revision zero is reserved; absence is represented by none",
            ));
        }
        let expected_revision = match base_revision {
            Some(base) => base
                .checked_add(1)
                .ok_or(PersistenceFailure::RevisionExhausted)?,
            None => 1,
        };
        if overlay.local_revision != expected_revision {
            return Err(PersistenceFailure::invalid(
                "overlay local revision must be exactly one greater than its declared base",
            ));
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

    let mut live_by_identity = HashMap::with_capacity(live.len());
    for slot in live {
        validate_stable_tab_slot(*slot, "live layout")?;
        if live_by_identity.insert(slot.identity(), *slot).is_some() {
            return Err(PersistenceFailure::invalid(
                "live layout contains duplicate stable tab identities",
            ));
        }
    }
    validate_remote_binding_aliases(live, "live layout")?;
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
    let mut ordered_slots = Vec::with_capacity(requested_capacity.min(MAX_TABS_PER_OVERLAY));
    let mut seen = HashSet::with_capacity(ordered_slots.capacity());
    let mut retained_unavailable = 0usize;
    let mut dropped_closed_or_stale = 0usize;

    for slot in &overlay.slots {
        let identity = slot.identity();
        let live_placement = live_by_identity.get(&identity).copied();
        let unavailable = slot
            .remote_binding()
            .is_some_and(|binding| unavailable_bindings.contains(&binding));
        if live_placement.is_some() || unavailable {
            if seen.insert(identity) {
                if ordered_slots.len() == MAX_TABS_PER_OVERLAY {
                    return Err(PersistenceFailure::quota(format!(
                        "reconciled tab count would exceed {MAX_TABS_PER_OVERLAY}"
                    )));
                }
                ordered_slots.push(live_placement.unwrap_or(*slot));
            }
            if unavailable && live_placement.is_none() {
                retained_unavailable += 1;
            }
        } else {
            dropped_closed_or_stale += 1;
        }
    }

    let mut appended_new = 0usize;
    for slot in live {
        if seen.insert(slot.identity()) {
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
        Some(active) if live_by_identity.contains_key(&active.identity()) => {
            live_by_identity.get(&active.identity()).copied()
        }
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
                .find_map(|candidate| live_by_identity.get(&candidate.identity()))
                .or_else(|| {
                    overlay.slots[..active_index]
                        .iter()
                        .rev()
                        .find_map(|candidate| live_by_identity.get(&candidate.identity()))
                })
                .copied()
                .or_else(|| {
                    live.iter()
                        .find(|candidate| {
                            !overlay
                                .slots
                                .iter()
                                .any(|persisted| persisted.identity() == candidate.identity())
                        })
                        .copied()
                })
        }
        None => live.first().copied(),
    };

    let live_slots = ordered_slots
        .iter()
        .copied()
        .filter(|slot| live_by_identity.contains_key(&slot.identity()))
        .collect::<Vec<_>>();
    debug_assert!(
        active_live_slot.is_none()
            || active_live_slot.is_some_and(|slot| live_by_identity.contains_key(&slot.identity()))
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

#[derive(Serialize)]
struct BorrowedDiskSlot<'a> {
    payload: &'a PersistedState,
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
    authority: Option<PathBuf>,
    target: PathBuf,
    degraded_recovery: bool,
    corrupt_evidence: Option<CorruptEvidence>,
    requires_schema_upgrade: bool,
}

#[derive(Clone, Debug, Default)]
struct PendingBatch {
    window_states: BTreeMap<String, PersistedWindowState>,
    window_state_superseded: BTreeMap<String, usize>,
    overlay_mutations: BTreeMap<LayoutWindowId, PendingOverlayMutation>,
    ensure_bindings: BTreeSet<PrivacySafeTargetFingerprint>,
    overlay_tab_count: usize,
}

impl PendingBatch {
    fn superseded_updates_for(
        &self,
        accepted_workspaces: &BTreeSet<String>,
        accepted_overlay_ids: &BTreeSet<LayoutWindowId>,
    ) -> usize {
        let window_updates = accepted_workspaces
            .iter()
            .filter_map(|workspace| self.window_state_superseded.get(workspace).copied())
            .fold(0usize, usize::saturating_add);
        accepted_overlay_ids
            .iter()
            .filter_map(|window_id| self.overlay_mutations.get(window_id))
            .map(|mutation| mutation.superseded_updates)
            .fold(window_updates, usize::saturating_add)
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
        match self.window_states.get(&workspace) {
            Some(previous) if *previous == state => return Ok(EnqueueOutcome::Unchanged),
            Some(_) => {
                let count = self
                    .window_state_superseded
                    .entry(workspace.clone())
                    .or_default();
                *count = count.saturating_add(1);
            }
            None => {}
        }
        let outcome = if self.window_states.insert(workspace, state).is_some() {
            EnqueueOutcome::Coalesced
        } else {
            EnqueueOutcome::Queued
        };
        Ok(outcome)
    }

    fn queue_overlay_live(
        &mut self,
        base_revision: Option<u64>,
        overlay: MixedDomainLayoutOverlay,
    ) -> Result<EnqueueOutcome, PersistenceFailure> {
        self.queue_overlay_mutation(PendingOverlayMutation::live(base_revision, overlay)?)
    }

    fn queue_overlay_delete(
        &mut self,
        window_id: LayoutWindowId,
        base_revision: Option<u64>,
    ) -> Result<EnqueueOutcome, PersistenceFailure> {
        self.queue_overlay_mutation(PendingOverlayMutation::deleted(window_id, base_revision)?)
    }

    fn queue_overlay_mutation(
        &mut self,
        mutation: PendingOverlayMutation,
    ) -> Result<EnqueueOutcome, PersistenceFailure> {
        let window_id = mutation.window_id();
        let previous = self.overlay_mutations.get(&window_id).cloned();
        match previous {
            None if self.overlay_mutations.len() >= MAX_LAYOUT_OVERLAYS => {
                return Err(PersistenceFailure::quota(format!(
                    "pending overlay mutation count would exceed {MAX_LAYOUT_OVERLAYS}"
                )));
            }
            None => {
                let total = self
                    .overlay_tab_count
                    .checked_add(mutation.live_tab_count())
                    .ok_or_else(|| {
                        PersistenceFailure::quota("pending overlay tab count overflowed")
                    })?;
                if total > MAX_TOTAL_OVERLAY_TABS {
                    return Err(PersistenceFailure::quota(format!(
                        "pending overlay tab count {total} exceeds {MAX_TOTAL_OVERLAY_TABS}"
                    )));
                }
                self.overlay_tab_count = total;
                self.overlay_mutations.insert(window_id, mutation);
                Ok(EnqueueOutcome::Queued)
            }
            Some(previous) if previous.desired == mutation.desired => Ok(EnqueueOutcome::Unchanged),
            Some(previous) if matches!(&previous.desired, DesiredOverlayState::Deleted { .. }) => {
                Err(PersistenceFailure::RetiredOverlay {
                    last_revision: previous.desired_revision(),
                })
            }
            Some(previous) if mutation.base_revision == Some(previous.desired_revision()) => {
                let total = self
                    .overlay_tab_count
                    .checked_sub(previous.live_tab_count())
                    .and_then(|count| count.checked_add(mutation.live_tab_count()))
                    .ok_or_else(|| {
                        PersistenceFailure::quota("pending overlay tab count overflowed")
                    })?;
                if total > MAX_TOTAL_OVERLAY_TABS {
                    return Err(PersistenceFailure::quota(format!(
                        "pending overlay tab count {total} exceeds {MAX_TOTAL_OVERLAY_TABS}"
                    )));
                }
                let composed = PendingOverlayMutation {
                    base_revision: previous.base_revision,
                    desired: mutation.desired,
                    superseded_updates: previous.superseded_updates.saturating_add(1),
                };
                self.overlay_tab_count = total;
                self.overlay_mutations.insert(window_id, composed);
                Ok(EnqueueOutcome::Coalesced)
            }
            Some(previous) if mutation.desired_revision() < previous.desired_revision() => {
                Err(PersistenceFailure::StaleOverlay {
                    incoming: mutation.desired_revision(),
                    committed: previous.desired_revision(),
                })
            }
            Some(previous) if mutation.desired_revision() == previous.desired_revision() => {
                Err(PersistenceFailure::OverlayRevisionConflict {
                    revision: mutation.desired_revision(),
                })
            }
            Some(previous) => Err(PersistenceFailure::OverlayCasConflict {
                expected: mutation.base_revision,
                committed: Some(previous.desired_revision()),
            }),
        }
    }

    fn acknowledge_resolved(
        &mut self,
        snapshot: &Self,
        accepted_overlay_ids: &BTreeSet<LayoutWindowId>,
        rejected_overlay_ids: &BTreeSet<LayoutWindowId>,
        binding_requests_after_snapshot: &BTreeSet<PrivacySafeTargetFingerprint>,
    ) {
        for (workspace, state) in &snapshot.window_states {
            if self.window_states.get(workspace) == Some(state) {
                self.window_states.remove(workspace);
                self.window_state_superseded.remove(workspace);
            } else if self.window_states.contains_key(workspace) {
                let committed_superseded = snapshot
                    .window_state_superseded
                    .get(workspace)
                    .copied()
                    .unwrap_or(0);
                let remove_counter =
                    if let Some(current) = self.window_state_superseded.get_mut(workspace) {
                        *current = current.saturating_sub(committed_superseded.saturating_add(1));
                        *current == 0
                    } else {
                        false
                    };
                if remove_counter {
                    self.window_state_superseded.remove(workspace);
                }
            }
        }

        for window_id in accepted_overlay_ids {
            let Some(committed) = snapshot.overlay_mutations.get(window_id) else {
                continue;
            };
            if self.overlay_mutations.get(window_id) == Some(committed) {
                self.overlay_tab_count = self
                    .overlay_tab_count
                    .saturating_sub(committed.live_tab_count());
                self.overlay_mutations.remove(window_id);
                continue;
            }
            let Some(current) = self.overlay_mutations.get_mut(window_id) else {
                continue;
            };
            let committed_revision = committed.desired_revision();
            let continues_committed_live = current.desired_revision() > committed_revision
                || matches!(
                    &current.desired,
                    DesiredOverlayState::Deleted {
                        last_local_revision,
                        ..
                    } if *last_local_revision == committed_revision
                );
            if current.base_revision == committed.base_revision
                && matches!(&committed.desired, DesiredOverlayState::Live(_))
                && continues_committed_live
            {
                current.base_revision = Some(committed_revision);
                current.superseded_updates = current
                    .superseded_updates
                    .saturating_sub(committed.superseded_updates.saturating_add(1));
            }
        }

        for window_id in rejected_overlay_ids {
            let Some(rejected) = snapshot.overlay_mutations.get(window_id) else {
                continue;
            };
            // A coalesced descendant retains the snapshot's durable base. If
            // the snapshot was rejected, that descendant has no committed
            // predecessor and must not survive as a revision-skipping create
            // or update. Callers may submit a fresh lineage after observing
            // the rejection.
            let same_rejected_lineage = self
                .overlay_mutations
                .get(window_id)
                .is_some_and(|current| current.base_revision == rejected.base_revision);
            if same_rejected_lineage
                && let Some(rejected_lineage) = self.overlay_mutations.remove(window_id)
            {
                self.overlay_tab_count = self
                    .overlay_tab_count
                    .saturating_sub(rejected_lineage.live_tab_count());
            }
        }

        for fingerprint in &snapshot.ensure_bindings {
            if !binding_requests_after_snapshot.contains(fingerprint) {
                self.ensure_bindings.remove(fingerprint);
            }
        }
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
    pub rejected_updates: usize,
}

#[derive(Debug)]
struct RejectedOverlayMutation {
    mutation: PendingOverlayMutation,
    failure: PersistenceFailure,
}

#[derive(Debug)]
struct BatchCommit {
    receipt: CommitReceipt,
    bindings: BTreeMap<PrivacySafeTargetFingerprint, DomainBindingId>,
    rejected_bindings: BTreeMap<PrivacySafeTargetFingerprint, PersistenceFailure>,
    rejected_workspaces: BTreeMap<String, PersistenceFailure>,
    accepted_overlay_ids: BTreeSet<LayoutWindowId>,
    rejected_overlay_mutations: BTreeMap<LayoutWindowId, RejectedOverlayMutation>,
}

impl BatchCommit {
    fn first_semantic_failure(&self) -> Option<&PersistenceFailure> {
        self.rejected_workspaces
            .values()
            .next()
            .or_else(|| self.rejected_bindings.values().next())
            .or_else(|| {
                self.rejected_overlay_mutations
                    .values()
                    .next()
                    .map(|rejected| &rejected.failure)
            })
    }
}

pub type CommitResult = Result<CommitReceipt, PersistenceFailure>;
pub type BindingResult = Result<DomainBindingId, PersistenceFailure>;

#[derive(Clone, Debug)]
struct SemanticFailureOutcome {
    identity: Arc<()>,
    failure: PersistenceFailure,
}

struct FlushWaiter {
    sender: flume::Sender<CommitResult>,
    prior_semantic_failure: Option<SemanticFailureOutcome>,
}

impl FlushWaiter {
    fn new(sender: flume::Sender<CommitResult>) -> Self {
        Self {
            sender,
            prior_semantic_failure: None,
        }
    }

    fn remember_semantic_failure(&mut self, outcome: &SemanticFailureOutcome) {
        if self.prior_semantic_failure.is_none() {
            self.prior_semantic_failure = Some(outcome.clone());
        }
    }

    fn result(
        &self,
        current_semantic_failure: Option<&SemanticFailureOutcome>,
        receipt: CommitReceipt,
    ) -> CommitResult {
        self.prior_semantic_failure
            .as_ref()
            .or(current_semantic_failure)
            .map(|outcome| outcome.failure.clone())
            .map_or(Ok(receipt), Err)
    }

    fn reported_semantic_identity(
        &self,
        current_semantic_failure: Option<&SemanticFailureOutcome>,
    ) -> Option<Arc<()>> {
        self.prior_semantic_failure
            .as_ref()
            .or(current_semantic_failure)
            .map(|outcome| Arc::clone(&outcome.identity))
    }

    fn transaction_failure_result(&self, failure: &PersistenceFailure) -> CommitResult {
        Err(failure.clone())
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestWorkerCommitPhase {
    Pending,
    ExactRetry,
}

#[cfg(test)]
#[derive(Debug)]
enum TestWorkerCommitEvent {
    BeforeWake {
        waiting_epoch: u64,
        retry_pending: bool,
    },
    CommitEntered {
        commit_epoch: u64,
        phase: TestWorkerCommitPhase,
        batch: PendingBatch,
    },
    CommitFinished {
        commit_epoch: u64,
        phase: TestWorkerCommitPhase,
        result: TestWorkerCommitResult,
    },
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
enum TestWorkerCommitAction {
    Run(WriteInterruption),
    ReturnDefinite(PersistenceFailure),
    Panic,
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
enum TestWorkerCommitResult {
    Committed(CommitReceipt),
    Failed(PersistenceFailureCode),
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
enum TestWorkerDirectiveAction {
    ContinueWake,
    PanicBeforeWake,
    Commit(TestWorkerCommitAction),
    ContinueAfterCommit,
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct TestWorkerCommitDirective {
    epoch: u64,
    action: TestWorkerDirectiveAction,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TestWorkerStopped {
    waiting_epoch: u64,
    commit_epoch: u64,
}

/// Per-worker deterministic commit gates used only by unit tests.
///
/// Events use a capacity-one channel and nonblocking sends so a dropped,
/// disconnected, or backpressured event receiver fails closed. The harness
/// intentionally blocks the worker at each gate until it sends the exact
/// epoch-matched directive or disconnects the directive channel. Disconnect
/// or a mismatched/invalid directive asks the worker to stop and drain every
/// admitted waiter as `WorkerStopped`.
#[cfg(test)]
struct TestWorkerCommitControl {
    events: flume::Sender<TestWorkerCommitEvent>,
    directives: flume::Receiver<TestWorkerCommitDirective>,
    stopped: flume::Sender<TestWorkerStopped>,
    waiting_epoch: u64,
    commit_epoch: u64,
}

#[cfg(test)]
impl TestWorkerCommitControl {
    fn before_wake(&mut self, retry_pending: bool) -> bool {
        let Some(waiting_epoch) = self.waiting_epoch.checked_add(1) else {
            return false;
        };
        self.waiting_epoch = waiting_epoch;
        if self
            .events
            .try_send(TestWorkerCommitEvent::BeforeWake {
                waiting_epoch: self.waiting_epoch,
                retry_pending,
            })
            .is_err()
        {
            return false;
        }
        match self.directives.recv() {
            Ok(TestWorkerCommitDirective {
                epoch,
                action: TestWorkerDirectiveAction::ContinueWake,
            }) if epoch == self.waiting_epoch => true,
            Ok(TestWorkerCommitDirective {
                epoch,
                action: TestWorkerDirectiveAction::PanicBeforeWake,
            }) if epoch == self.waiting_epoch => {
                panic!("intentional controlled persistence worker-loop panic")
            }
            _ => false,
        }
    }

    fn enter_commit(
        &mut self,
        phase: TestWorkerCommitPhase,
        batch: &PendingBatch,
    ) -> Option<TestWorkerCommitAction> {
        self.commit_epoch = self.commit_epoch.checked_add(1)?;
        if self
            .events
            .try_send(TestWorkerCommitEvent::CommitEntered {
                commit_epoch: self.commit_epoch,
                phase,
                batch: batch.clone(),
            })
            .is_err()
        {
            return None;
        }
        let action = match self.directives.recv().ok()? {
            TestWorkerCommitDirective {
                epoch,
                action: TestWorkerDirectiveAction::Commit(action),
            } if epoch == self.commit_epoch => action,
            _ => return None,
        };
        if matches!(
            &action,
            TestWorkerCommitAction::ReturnDefinite(failure)
                if failure.may_have_published_generation()
        ) {
            return None;
        }
        Some(action)
    }

    fn commit_finished(
        &mut self,
        phase: TestWorkerCommitPhase,
        result: &Result<BatchCommit, PersistenceFailure>,
    ) -> bool {
        let result = match result {
            Ok(committed) => TestWorkerCommitResult::Committed(committed.receipt),
            Err(failure) => TestWorkerCommitResult::Failed(failure.code()),
        };
        if self
            .events
            .try_send(TestWorkerCommitEvent::CommitFinished {
                commit_epoch: self.commit_epoch,
                phase,
                result,
            })
            .is_err()
        {
            return false;
        }
        matches!(
            self.directives.recv(),
            Ok(TestWorkerCommitDirective {
                epoch,
                action: TestWorkerDirectiveAction::ContinueAfterCommit,
            }) if epoch == self.commit_epoch
        )
    }

    fn report_stopped(&self) {
        let _ = self.stopped.try_send(TestWorkerStopped {
            waiting_epoch: self.waiting_epoch,
            commit_epoch: self.commit_epoch,
        });
    }
}

#[derive(Default)]
struct CoordinatorPending {
    batch: PendingBatch,
    flush_waiters: Vec<FlushWaiter>,
    binding_waiters: BTreeMap<PrivacySafeTargetFingerprint, Vec<flume::Sender<BindingResult>>>,
    waiter_count: usize,
    unreported_semantic_failure: Option<SemanticFailureOutcome>,
    #[cfg(test)]
    worker_commit_control: Option<TestWorkerCommitControl>,
}

impl CoordinatorPending {
    fn record_semantic_failure(&mut self, failure: &PersistenceFailure) -> SemanticFailureOutcome {
        // Retain the first rejection not yet crossed by a successful explicit
        // flush. Later rejections in the same barrier interval remain covered
        // by that deterministic first failure; exact per-lineage receipts are
        // a separate API layer.
        let outcome = self.unreported_semantic_failure.clone().unwrap_or_else(|| {
            let outcome = SemanticFailureOutcome {
                identity: Arc::new(()),
                failure: failure.clone(),
            };
            self.unreported_semantic_failure = Some(outcome.clone());
            outcome
        });
        for waiter in &mut self.flush_waiters {
            waiter.remember_semantic_failure(&outcome);
        }
        outcome
    }

    fn clear_reported_semantic_failure(&mut self, identity: &Arc<()>) {
        if self
            .unreported_semantic_failure
            .as_ref()
            .is_some_and(|outcome| Arc::ptr_eq(&outcome.identity, identity))
        {
            self.unreported_semantic_failure = None;
        }
    }
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
            self.wake_worker()?;
            pending
                .batch
                .queue_window_state(workspace.to_owned(), state)?
        };
        Ok(outcome)
    }

    pub fn queue_overlay(
        &self,
        base_revision: Option<u64>,
        overlay: MixedDomainLayoutOverlay,
    ) -> Result<EnqueueOutcome, PersistenceFailure> {
        validate_overlay(&overlay)?;
        let outcome = {
            let mut pending = lock_pending(&self.shared.pending);
            self.wake_worker()?;
            pending.batch.queue_overlay_live(base_revision, overlay)?
        };
        Ok(outcome)
    }

    pub fn queue_overlay_delete(
        &self,
        window_id: LayoutWindowId,
        base_revision: Option<u64>,
    ) -> Result<EnqueueOutcome, PersistenceFailure> {
        let outcome = {
            let mut pending = lock_pending(&self.shared.pending);
            self.wake_worker()?;
            pending
                .batch
                .queue_overlay_delete(window_id, base_revision)?
        };
        Ok(outcome)
    }

    /// Resolve or create a stable domain binding on the storage worker.
    ///
    /// The returned receiver is intentionally asynchronous. Callers must await
    /// it outside input, parser, resize, render, and present callbacks. A remote
    /// slot may use the binding ID only after the receiver resolves it as
    /// durable; a queued allocation request is not publication authority.
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
            self.wake_worker()?;
            pending.batch.ensure_bindings.insert(target_fingerprint);
            pending
                .binding_waiters
                .entry(target_fingerprint)
                .or_default()
                .push(sender);
            pending.waiter_count += 1;
        }
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
            self.wake_worker()?;
            let mut waiter = FlushWaiter::new(sender);
            if let Some(outcome) = pending.unreported_semantic_failure.as_ref() {
                waiter.remember_semantic_failure(outcome);
            }
            pending.flush_waiters.push(waiter);
            pending.waiter_count += 1;
        }
        Ok(receiver)
    }

    fn wake_worker(&self) -> Result<(), PersistenceFailure> {
        match self.wake.try_send(()) {
            Ok(()) | Err(flume::TrySendError::Full(())) => Ok(()),
            Err(flume::TrySendError::Disconnected(())) => Err(PersistenceFailure::WorkerStopped),
        }
    }
}

fn lock_pending<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::error!("window-state: recovering a poisoned coordinator mutex");
            let guard = poisoned.into_inner();
            // Existing recovery already trusts and continues with the
            // protected value. Clear the poison while this recovered guard is
            // still exclusive so every later persistence operation does not
            // re-enter the error path and emit an unbounded log storm.
            mutex.clear_poison();
            guard
        }
    }
}

type BindingWaiters = BTreeMap<PrivacySafeTargetFingerprint, Vec<flume::Sender<BindingResult>>>;

/// Owns one exact admitted waiter cohort outside the coordinator mutex.
///
/// Any unexpected unwind before an ordinary response is terminalized fails
/// the still-owned cohort explicitly. This prevents the whole-worker recovery
/// boundary from degrading a typed persistence result into a bare disconnected
/// channel after the cohort has left `CoordinatorPending`.
struct AdmittedWaiters {
    flush: Vec<FlushWaiter>,
    bindings: BindingWaiters,
}

struct AdmittedFlushWaiter {
    waiter: Option<FlushWaiter>,
}

impl AdmittedFlushWaiter {
    fn new(waiter: FlushWaiter) -> Self {
        Self {
            waiter: Some(waiter),
        }
    }

    fn waiter(&self) -> &FlushWaiter {
        self.waiter
            .as_ref()
            .expect("admitted flush waiter is unresolved")
    }

    fn respond(mut self, result: CommitResult) {
        let waiter = self
            .waiter
            .as_ref()
            .expect("admitted flush waiter is unresolved");
        let _ = waiter.sender.try_send(result);
        self.waiter = None;
    }
}

impl Drop for AdmittedFlushWaiter {
    fn drop(&mut self) {
        let Some(waiter) = self.waiter.take() else {
            return;
        };
        let _ = waiter
            .sender
            .try_send(Err(PersistenceFailure::WorkerPanicked));
    }
}

struct AdmittedBindingWaiter {
    sender: Option<flume::Sender<BindingResult>>,
}

impl AdmittedBindingWaiter {
    fn new(sender: flume::Sender<BindingResult>) -> Self {
        Self {
            sender: Some(sender),
        }
    }

    fn respond(mut self, result: BindingResult) {
        let sender = self
            .sender
            .as_ref()
            .expect("admitted binding waiter is unresolved");
        let _ = sender.try_send(result);
        self.sender = None;
    }
}

impl Drop for AdmittedBindingWaiter {
    fn drop(&mut self) {
        let Some(sender) = self.sender.take() else {
            return;
        };
        let _ = sender.try_send(Err(PersistenceFailure::WorkerPanicked));
    }
}

impl AdmittedWaiters {
    fn new(mut flush: Vec<FlushWaiter>, mut bindings: BindingWaiters) -> Self {
        // Pending vectors are append-only admission queues. Reverse them once
        // so the O(1) guarded `pop` operations below preserve the original
        // oldest-first response order instead of silently turning it into
        // LIFO under a large waiter cohort.
        flush.reverse();
        for waiters in bindings.values_mut() {
            waiters.reverse();
        }
        Self { flush, bindings }
    }

    fn take_from(shared: &CoordinatorShared) -> Self {
        let mut pending = lock_pending(&shared.pending);
        let flush = std::mem::take(&mut pending.flush_waiters);
        let bindings = std::mem::take(&mut pending.binding_waiters);
        pending.waiter_count = 0;
        drop(pending);
        Self::new(flush, bindings)
    }

    fn respond_failure(mut self, failure: &PersistenceFailure) {
        while let Some((_, waiter)) = self.next_binding() {
            waiter.respond(Err(failure.clone()));
        }
        while let Some(waiter) = self.next_flush() {
            let result = waiter.waiter().transaction_failure_result(failure);
            waiter.respond(result);
        }
    }

    fn flush_in_admission_order(&self) -> impl Iterator<Item = &FlushWaiter> {
        self.flush.iter().rev()
    }

    fn next_flush(&mut self) -> Option<AdmittedFlushWaiter> {
        self.flush.pop().map(AdmittedFlushWaiter::new)
    }

    fn next_binding(&mut self) -> Option<(PrivacySafeTargetFingerprint, AdmittedBindingWaiter)> {
        loop {
            let fingerprint = *self.bindings.keys().next()?;
            let sender = self.bindings.get_mut(&fingerprint).and_then(Vec::pop);
            if self.bindings.get(&fingerprint).is_some_and(Vec::is_empty) {
                self.bindings.remove(&fingerprint);
            }
            if let Some(sender) = sender {
                return Some((fingerprint, AdmittedBindingWaiter::new(sender)));
            }
        }
    }
}

impl Drop for AdmittedWaiters {
    fn drop(&mut self) {
        if self.flush.is_empty() && self.bindings.is_empty() {
            return;
        }
        while let Some((_, waiter)) = self.next_binding() {
            drop(waiter);
        }
        while let Some(waiter) = self.next_flush() {
            drop(waiter);
        }
    }
}

fn drain_pending_waiters(shared: &CoordinatorShared, failure: &PersistenceFailure) {
    let mut pending = lock_pending(&shared.pending);
    let flush_waiters = std::mem::take(&mut pending.flush_waiters);
    let binding_waiters = std::mem::take(&mut pending.binding_waiters);
    pending.waiter_count = 0;
    drop(pending);
    AdmittedWaiters::new(flush_waiters, binding_waiters).respond_failure(failure);
}

fn acknowledge_committed_batch(
    shared: &CoordinatorShared,
    batch: &PendingBatch,
    committed: &BatchCommit,
) -> Option<SemanticFailureOutcome> {
    let rejected_overlay_ids = committed
        .rejected_overlay_mutations
        .keys()
        .copied()
        .collect::<BTreeSet<_>>();
    let semantic_failure = committed.first_semantic_failure();
    let flush_outcome = {
        let mut pending = lock_pending(&shared.pending);
        let binding_requests_after_snapshot = pending.binding_waiters.keys().copied().collect();
        pending.batch.acknowledge_resolved(
            batch,
            &committed.accepted_overlay_ids,
            &rejected_overlay_ids,
            &binding_requests_after_snapshot,
        );
        semantic_failure.map(|failure| pending.record_semantic_failure(failure))
    };

    for failure in committed.rejected_workspaces.values() {
        log::warn!(
            "window-state: workspace mutation rejected ({:?})",
            failure.code()
        );
    }
    for failure in committed.rejected_bindings.values() {
        log::warn!(
            "window-state: domain binding mutation rejected ({:?})",
            failure.code()
        );
    }
    for (window_id, rejected) in &committed.rejected_overlay_mutations {
        debug_assert_eq!(
            batch.overlay_mutations.get(window_id),
            Some(&rejected.mutation)
        );
        log::warn!(
            "window-state: overlay mutation rejected ({:?})",
            rejected.failure.code()
        );
    }
    flush_outcome
}

fn run_persistence_transaction<T>(
    transaction: impl FnOnce() -> T,
) -> Result<T, PersistenceFailure> {
    match catch_recoverable(
        RecoverablePanicSite::StorageWriter,
        std::panic::AssertUnwindSafe(transaction),
    ) {
        Ok(result) => Ok(result),
        Err(_) => {
            // A panic may occur after the inactive journal slot has reached
            // durable storage. Classify it as uncertain publication so the
            // worker preserves this exact frozen batch as retry debt and
            // fences every successor behind its recovery.
            log::error!(
                "window-state: persistence transaction panic recovered; fencing successors behind exact retry"
            );
            metrics::counter!("gui.window_state.persistence_transaction_panic").increment(1);
            Err(PersistenceFailure::WorkerPanicked)
        }
    }
}

fn resolve_exact_retry(
    shared: &CoordinatorShared,
    batch: &PendingBatch,
) -> Result<(), PersistenceFailure> {
    let committed = commit_batch(&shared.primary_path, batch, WriteInterruption::None)?;
    let _ = acknowledge_committed_batch(shared, batch, &committed);
    Ok(())
}

#[cfg(test)]
fn controlled_worker_commit(
    shared: &CoordinatorShared,
    batch: &PendingBatch,
    control: &mut Option<TestWorkerCommitControl>,
    phase: TestWorkerCommitPhase,
) -> Option<Result<BatchCommit, PersistenceFailure>> {
    let Some(control) = control.as_mut() else {
        return Some(commit_batch(
            &shared.primary_path,
            batch,
            WriteInterruption::None,
        ));
    };
    let result = match control.enter_commit(phase, batch)? {
        TestWorkerCommitAction::Run(interruption) => {
            commit_batch(&shared.primary_path, batch, interruption)
        }
        TestWorkerCommitAction::ReturnDefinite(failure) => Err(failure),
        TestWorkerCommitAction::Panic => {
            panic!("intentional controlled persistence transaction panic")
        }
    };
    if control.commit_finished(phase, &result) {
        Some(result)
    } else {
        None
    }
}

#[cfg(test)]
fn controlled_worker_exact_retry(
    shared: &CoordinatorShared,
    batch: &PendingBatch,
    control: &mut Option<TestWorkerCommitControl>,
) -> Option<Result<(), PersistenceFailure>> {
    Some(
        controlled_worker_commit(shared, batch, control, TestWorkerCommitPhase::ExactRetry)?.map(
            |committed| {
                let _ = acknowledge_committed_batch(shared, batch, &committed);
            },
        ),
    )
}

#[cfg(test)]
fn controlled_worker_receive_wake(
    control: &mut Option<TestWorkerCommitControl>,
    receiver: &flume::Receiver<()>,
    retry_pending: bool,
) -> bool {
    if control
        .as_mut()
        .is_some_and(|control| !control.before_wake(retry_pending))
    {
        return false;
    }
    receiver.recv().is_ok()
}

fn persistence_worker_loop(
    shared: &CoordinatorShared,
    receiver: &flume::Receiver<()>,
    #[cfg(test)] worker_commit_control: &mut Option<TestWorkerCommitControl>,
) {
    let mut retry_batch = None;
    while {
        #[cfg(test)]
        {
            controlled_worker_receive_wake(
                &mut *worker_commit_control,
                receiver,
                retry_batch.is_some(),
            )
        }
        #[cfg(not(test))]
        {
            receiver.recv().is_ok()
        }
    } {
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

        if let Some(batch) = retry_batch.take() {
            #[cfg(test)]
            let retry_result = match run_persistence_transaction(|| {
                controlled_worker_exact_retry(shared, &batch, &mut *worker_commit_control)
            }) {
                Ok(Some(result)) => result,
                Ok(None) => break,
                Err(failure) => Err(failure),
            };
            #[cfg(not(test))]
            let retry_result =
                match run_persistence_transaction(|| resolve_exact_retry(shared, &batch)) {
                    Ok(result) => result,
                    Err(failure) => Err(failure),
                };
            match retry_result {
                Ok(()) => {}
                Err(failure) => {
                    // This batch is already exact-retry debt from an earlier
                    // commit that may have published. A later failure cannot
                    // prove the predecessor absent, regardless of its own
                    // classification, so successors must remain fenced behind
                    // the same frozen snapshot until it resolves.
                    retry_batch = Some(batch);
                    log::warn!(
                        "window-state: exact in-flight retry rejected ({:?})",
                        failure.code()
                    );
                    AdmittedWaiters::take_from(shared).respond_failure(&failure);
                    continue;
                }
            }
        }

        let (batch, mut waiters) = {
            let mut pending = lock_pending(&shared.pending);
            let batch = pending.batch.clone();
            let flush_waiters = std::mem::take(&mut pending.flush_waiters);
            let binding_waiters = std::mem::take(&mut pending.binding_waiters);
            pending.waiter_count = 0;
            drop(pending);
            (batch, AdmittedWaiters::new(flush_waiters, binding_waiters))
        };

        #[cfg(test)]
        let result = match run_persistence_transaction(|| {
            controlled_worker_commit(
                shared,
                &batch,
                &mut *worker_commit_control,
                TestWorkerCommitPhase::Pending,
            )
        }) {
            Ok(Some(result)) => result,
            Ok(None) => {
                waiters.respond_failure(&PersistenceFailure::WorkerStopped);
                break;
            }
            Err(failure) => Err(failure),
        };
        #[cfg(not(test))]
        let result = match run_persistence_transaction(|| {
            commit_batch(&shared.primary_path, &batch, WriteInterruption::None)
        }) {
            Ok(result) => result,
            Err(failure) => Err(failure),
        };
        match result {
            Ok(committed) => {
                let flush_outcome = acknowledge_committed_batch(shared, &batch, &committed);
                let reported_semantic_identity = waiters
                    .flush_in_admission_order()
                    .find_map(|waiter| waiter.reported_semantic_identity(flush_outcome.as_ref()));
                if let Some(identity) = reported_semantic_identity {
                    lock_pending(&shared.pending).clear_reported_semantic_failure(&identity);
                }
                let mut binding_result = None;
                while let Some((fingerprint, waiter)) = waiters.next_binding() {
                    let needs_result = binding_result
                        .as_ref()
                        .is_none_or(|(cached, _)| *cached != fingerprint);
                    if needs_result {
                        let result = if let Some(binding) = committed.bindings.get(&fingerprint) {
                            Ok(*binding)
                        } else if let Some(failure) = committed.rejected_bindings.get(&fingerprint)
                        {
                            Err(failure.clone())
                        } else {
                            Err(PersistenceFailure::corrupt(
                                "binding commit succeeded without its requested identity",
                            ))
                        };
                        binding_result = Some((fingerprint, result));
                    }
                    let Some((_, binding)) = binding_result.as_ref() else {
                        waiter.respond(Err(PersistenceFailure::corrupt(
                            "binding result cache was empty after refresh",
                        )));
                        continue;
                    };
                    waiter.respond(binding.clone());
                }
                while let Some(waiter) = waiters.next_flush() {
                    let result = waiter
                        .waiter()
                        .result(flush_outcome.as_ref(), committed.receipt);
                    waiter.respond(result);
                }
            }
            Err(failure) => {
                if failure.may_have_published_generation() {
                    retry_batch = Some(batch);
                }
                log::warn!(
                    "window-state: persistence commit rejected ({:?})",
                    failure.code()
                );
                waiters.respond_failure(&failure);
            }
        }
    }
}

fn persistence_worker(shared: Arc<CoordinatorShared>, receiver: flume::Receiver<()>) {
    #[cfg(test)]
    let mut worker_commit_control = {
        let mut pending = lock_pending(&shared.pending);
        pending.worker_commit_control.take()
    };

    #[cfg(test)]
    let outcome = catch_recoverable(
        RecoverablePanicSite::StorageWriter,
        std::panic::AssertUnwindSafe(|| {
            persistence_worker_loop(&shared, &receiver, &mut worker_commit_control);
        }),
    );
    #[cfg(not(test))]
    let outcome = catch_recoverable(
        RecoverablePanicSite::StorageWriter,
        std::panic::AssertUnwindSafe(|| {
            persistence_worker_loop(&shared, &receiver);
        }),
    );

    // Close admission before draining. Otherwise a caller can successfully
    // enqueue between the final drain and this function dropping `receiver`,
    // stranding a waiter after the worker has already decided to stop.
    drop(receiver);
    let worker_panicked = outcome.is_err();
    let terminal_failure = if worker_panicked {
        PersistenceFailure::WorkerPanicked
    } else {
        PersistenceFailure::WorkerStopped
    };
    drain_pending_waiters(&shared, &terminal_failure);

    #[cfg(test)]
    if let Some(control) = worker_commit_control.as_ref() {
        control.report_stopped();
    }

    // Terminalize caller-visible liveness before optional observability. A
    // custom logger or metrics recorder must never be able to strand an
    // admitted receiver after this worker has already closed admission.
    if worker_panicked {
        let _ = catch_recoverable(
            RecoverablePanicSite::StorageWriter,
            std::panic::AssertUnwindSafe(|| {
                log::error!(
                    "window-state: persistence worker panic recovered; stopping the writer and failing admitted waiters"
                );
                metrics::counter!("gui.window_state.persistence_worker_panic").increment(1);
            }),
        );
    }
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

fn validate_tombstone(tombstone: &OverlayTombstone) -> Result<(), PersistenceFailure> {
    if tombstone.last_local_revision == 0 {
        return Err(PersistenceFailure::invalid(
            "overlay tombstone revision zero is reserved",
        ));
    }
    if tombstone.retired_at_store_revision == 0 {
        return Err(PersistenceFailure::invalid(
            "overlay tombstone store revision zero is reserved",
        ));
    }
    Ok(())
}

fn validate_remote_binding_aliases(
    slots: &[StableTabSlot],
    context: &'static str,
) -> Result<(), PersistenceFailure> {
    let mut bindings_by_authority = HashMap::new();
    for slot in slots {
        let (Some(authority), Some(binding)) = (slot.remote_authority(), slot.remote_binding())
        else {
            continue;
        };
        if bindings_by_authority
            .insert(authority, binding)
            .is_some_and(|prior| prior != binding)
        {
            return Err(PersistenceFailure::invalid(format!(
                "{context} aliases one remote session/tab identity through multiple bindings"
            )));
        }
    }
    Ok(())
}

fn validate_stable_tab_slot(
    slot: StableTabSlot,
    context: &'static str,
) -> Result<(), PersistenceFailure> {
    let StableTabSlot::Remote {
        binding_id,
        session_id,
        remote_window_id,
        remote_tab_id,
    } = slot
    else {
        return Ok(());
    };

    if binding_id.as_bytes() == [0; 16] {
        return Err(PersistenceFailure::invalid(format!(
            "{context} remote tab slot uses reserved zero domain binding identity"
        )));
    }
    if session_id.as_bytes() == [0; 16] {
        return Err(PersistenceFailure::invalid(format!(
            "{context} remote tab slot uses reserved zero mux-session identity"
        )));
    }
    if remote_window_id == u64::MAX {
        return Err(PersistenceFailure::invalid(format!(
            "{context} remote tab slot uses the terminal remote-window identity"
        )));
    }
    if remote_tab_id == u64::MAX {
        return Err(PersistenceFailure::invalid(format!(
            "{context} remote tab slot uses the terminal remote-tab identity"
        )));
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
    let mut identities = HashSet::with_capacity(overlay.slots.len());
    for &slot in &overlay.slots {
        validate_stable_tab_slot(slot, "overlay")?;
        if !identities.insert(slot.identity()) {
            return Err(PersistenceFailure::invalid(
                "overlay contains duplicate stable tab identities",
            ));
        }
    }
    validate_remote_binding_aliases(&overlay.slots, "overlay")?;
    match overlay.active {
        Some(active) if overlay.slots.contains(&active) => {}
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
    if state
        .domain_bindings
        .iter()
        .any(|record| record.binding_id.as_bytes() == [0; 16])
    {
        return Err(PersistenceFailure::invalid(
            "domain binding record uses reserved zero identity",
        ));
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
    let mut globally_owned_tabs = HashSet::new();
    let mut global_remote_bindings = HashMap::new();
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
            if !globally_owned_tabs.insert(slot.identity()) {
                return Err(PersistenceFailure::invalid(
                    "one stable tab identity is owned by multiple layout windows",
                ));
            }
            if let (Some(authority), Some(binding_id)) =
                (slot.remote_authority(), slot.remote_binding())
                && global_remote_bindings
                    .insert(authority, binding_id)
                    .is_some_and(|prior| prior != binding_id)
            {
                return Err(PersistenceFailure::invalid(
                    "one remote session/tab identity is aliased through multiple bindings",
                ));
            }
            if let Some(binding_id) = slot.remote_binding()
                && !binding_ids.contains(&binding_id)
            {
                return Err(PersistenceFailure::invalid(
                    "remote overlay slot references an unknown domain binding",
                ));
            }
        }
    }
    if state.tombstones.len() > MAX_OVERLAY_TOMBSTONES {
        return Err(PersistenceFailure::quota(format!(
            "overlay tombstone count {} exceeds {}",
            state.tombstones.len(),
            MAX_OVERLAY_TOMBSTONES
        )));
    }
    let mut tombstone_ids = HashSet::with_capacity(state.tombstones.len());
    for tombstone in &state.tombstones {
        validate_tombstone(tombstone)?;
        if tombstone.retired_at_store_revision > state.store_revision {
            return Err(PersistenceFailure::invalid(
                "overlay tombstone retirement is newer than its store generation",
            ));
        }
        if !tombstone_ids.insert(tombstone.window_id) {
            return Err(PersistenceFailure::invalid(
                "duplicate overlay tombstone identity",
            ));
        }
        if overlay_ids.contains(&tombstone.window_id) {
            return Err(PersistenceFailure::invalid(
                "overlay identity is both live and tombstoned",
            ));
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
    state.tombstones.sort_by_key(|tombstone| {
        (
            tombstone.retired_at_store_revision,
            tombstone.window_id.as_bytes(),
        )
    });
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

#[derive(Default)]
struct JsonLengthWriter {
    bytes: u64,
}

impl Write for JsonLengthWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let added = u64::try_from(buf.len())
            .map_err(|_| io::Error::other("serialized JSON chunk length does not fit u64"))?;
        self.bytes = self
            .bytes
            .checked_add(added)
            .ok_or_else(|| io::Error::other("serialized JSON length overflowed"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encoded_json_len<T>(value: &T) -> Result<u64, PersistenceFailure>
where
    T: Serialize + ?Sized,
{
    let mut writer = JsonLengthWriter::default();
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| PersistenceFailure::corrupt("could not count serialized JSON"))?;
    Ok(writer.bytes)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct JsonCollectionBudget {
    item_bytes: u64,
    item_count: usize,
}

impl JsonCollectionBudget {
    fn insert(&mut self, item_bytes: u64) -> Result<(), PersistenceFailure> {
        self.item_bytes = self
            .item_bytes
            .checked_add(item_bytes)
            .ok_or_else(encoded_size_overflow)?;
        self.item_count = self
            .item_count
            .checked_add(1)
            .ok_or_else(encoded_size_overflow)?;
        Ok(())
    }

    fn remove(&mut self, item_bytes: u64) -> Result<(), PersistenceFailure> {
        self.item_bytes = self
            .item_bytes
            .checked_sub(item_bytes)
            .ok_or_else(encoded_size_inconsistent)?;
        self.item_count = self
            .item_count
            .checked_sub(1)
            .ok_or_else(encoded_size_inconsistent)?;
        Ok(())
    }

    fn replace(
        &mut self,
        old_item_bytes: u64,
        new_item_bytes: u64,
    ) -> Result<(), PersistenceFailure> {
        self.remove(old_item_bytes)?;
        self.insert(new_item_bytes)
    }

    fn contribution(self) -> Result<u64, PersistenceFailure> {
        let separators = if self.item_count == 0 {
            0
        } else {
            self.item_count
                .checked_sub(1)
                .ok_or_else(encoded_size_inconsistent)?
        };
        let separators = u64::try_from(separators).map_err(|_| encoded_size_overflow())?;
        self.item_bytes
            .checked_add(separators)
            .ok_or_else(encoded_size_overflow)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EncodedStateBudget {
    empty_slot_bytes: u64,
    window_states: JsonCollectionBudget,
    domain_bindings: JsonCollectionBudget,
    overlays: JsonCollectionBudget,
    tombstones: JsonCollectionBudget,
}

impl EncodedStateBudget {
    fn from_state(
        state: &PersistedState,
        published_revision: u64,
    ) -> Result<Self, PersistenceFailure> {
        Self::from_state_with_binding_width(state, published_revision, true)
    }

    fn from_state_with_physical_bindings(
        state: &PersistedState,
        published_revision: u64,
    ) -> Result<Self, PersistenceFailure> {
        Self::from_state_with_binding_width(state, published_revision, false)
    }

    fn from_state_with_binding_width(
        state: &PersistedState,
        published_revision: u64,
        normalize_existing_bindings: bool,
    ) -> Result<Self, PersistenceFailure> {
        let empty = PersistedState {
            store_revision: published_revision,
            ..PersistedState::default()
        };
        let empty_slot_bytes = encoded_json_len(&BorrowedDiskSlot {
            payload: &empty,
            sha256: [u8::MAX; 32],
        })?;
        let mut budget = Self {
            empty_slot_bytes,
            window_states: JsonCollectionBudget::default(),
            domain_bindings: JsonCollectionBudget::default(),
            overlays: JsonCollectionBudget::default(),
            tombstones: JsonCollectionBudget::default(),
        };
        for (workspace, window_state) in &state.window_states {
            budget
                .window_states
                .insert(window_state_entry_len(workspace, window_state)?)?;
        }
        for binding in &state.domain_bindings {
            let binding_bytes = if normalize_existing_bindings {
                maximum_width_binding_len(binding.target_fingerprint)?
            } else {
                encoded_json_len(binding)?
            };
            budget.domain_bindings.insert(binding_bytes)?;
        }
        for overlay in &state.overlays {
            budget.overlays.insert(encoded_json_len(overlay)?)?;
        }
        for tombstone in &state.tombstones {
            budget.tombstones.insert(encoded_json_len(tombstone)?)?;
        }
        Ok(budget)
    }

    fn upper_bound(&self) -> Result<u64, PersistenceFailure> {
        [
            self.window_states,
            self.domain_bindings,
            self.overlays,
            self.tombstones,
        ]
        .iter()
        .try_fold(self.empty_slot_bytes, |total, collection| {
            total
                .checked_add(collection.contribution()?)
                .ok_or_else(encoded_size_overflow)
        })
    }
}

fn encoded_size_overflow() -> PersistenceFailure {
    PersistenceFailure::corrupt("encoded-size accounting overflowed")
}

fn encoded_size_inconsistent() -> PersistenceFailure {
    PersistenceFailure::corrupt("encoded-size accounting became inconsistent")
}

fn window_state_entry_len(
    workspace: &str,
    state: &PersistedWindowState,
) -> Result<u64, PersistenceFailure> {
    let workspace_bytes = encoded_json_len(workspace)?;
    let state_bytes = encoded_json_len(state)?;
    workspace_bytes
        .checked_add(1)
        .and_then(|length| length.checked_add(state_bytes))
        .ok_or_else(encoded_size_overflow)
}

fn maximum_width_binding_len(
    fingerprint: PrivacySafeTargetFingerprint,
) -> Result<u64, PersistenceFailure> {
    encoded_json_len(&DomainBindingRecord {
        target_fingerprint: fingerprint,
        binding_id: DomainBindingId::from_bytes([u8::MAX; 16]),
    })
}

struct PayloadHashWriter<'a> {
    output: &'a mut Vec<u8>,
    hasher: Sha256,
}

impl<'a> PayloadHashWriter<'a> {
    fn new(output: &'a mut Vec<u8>) -> Self {
        Self {
            output,
            hasher: Sha256::new(),
        }
    }

    fn finish(self) -> [u8; 32] {
        self.hasher.finalize().into()
    }
}

impl Write for PayloadHashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.output.extend_from_slice(buf);
        self.hasher.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_disk_slot(state: &PersistedState) -> Result<Vec<u8>, PersistenceFailure> {
    validate_published_state(state)?;
    let mut encoded = Vec::new();
    encoded.extend_from_slice(br#"{"payload":"#);
    let sha256 = {
        let mut writer = PayloadHashWriter::new(&mut encoded);
        serde_json::to_writer(&mut writer, state)
            .map_err(|_| PersistenceFailure::corrupt("could not serialize state payload"))?;
        writer.finish()
    };
    encoded.extend_from_slice(br#","sha256":"#);
    // Keep the on-disk representation byte-compatible with the derived
    // `DiskSlot` schema while avoiding a deep state clone and a second
    // materialized payload buffer. The spacing-free literal is verified by
    // the serializer-oracle regression below.
    serde_json::to_writer(&mut encoded, &sha256)
        .map_err(|_| PersistenceFailure::corrupt("could not serialize state checksum"))?;
    encoded.push(b'}');
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

fn corrupt_slot(path: &Path, bytes: Vec<u8>, failure: PersistenceFailure) -> ReadSlot {
    ReadSlot::Corrupt {
        failure,
        evidence: CorruptEvidence {
            path: path.to_path_buf(),
            bytes,
        },
    }
}

fn migrate_v2_state(state: PersistedStateV2) -> Result<PersistedState, PersistenceFailure> {
    if state.schema_version != PREVIOUS_STORE_SCHEMA_VERSION {
        return Err(PersistenceFailure::UnsupportedVersion {
            found: state.schema_version,
            current: STORE_SCHEMA_VERSION,
        });
    }
    if state.store_revision == 0 {
        return Err(PersistenceFailure::invalid(
            "published schema-v2 state has revision zero",
        ));
    }
    let current = state.into_current();
    validate_state(&current)?;
    Ok(current)
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
    let bounded_capacity = usize::try_from(metadata.len().min(MAX_STATE_FILE_BYTES)).unwrap_or(0);
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
            return Ok(corrupt_slot(
                path,
                bytes,
                PersistenceFailure::corrupt("state slot is not valid JSON"),
            ));
        }
    };

    let Some(version) = schema_version_probe(&value) else {
        if !allow_legacy {
            return Ok(corrupt_slot(
                path,
                bytes,
                PersistenceFailure::corrupt("state slot has no schema version"),
            ));
        }
        return match serde_json::from_value::<BTreeMap<String, PersistedWindowState>>(value) {
            Ok(window_states) => {
                let state = PersistedState {
                    window_states,
                    ..PersistedState::default()
                };
                match validate_state(&state) {
                    Ok(()) => Ok(ReadSlot::Valid(ValidatedSlot {
                        state,
                        schema: SlotSchema::LegacyGeometry,
                    })),
                    Err(failure) => Ok(corrupt_slot(path, bytes, failure)),
                }
            }
            Err(_) => Ok(corrupt_slot(
                path,
                bytes,
                PersistenceFailure::corrupt("legacy geometry map is invalid"),
            )),
        };
    };

    match version {
        STORE_SCHEMA_VERSION => {
            let disk = match serde_json::from_value::<DiskSlot>(value) {
                Ok(disk) => disk,
                Err(_) => {
                    return Ok(corrupt_slot(
                        path,
                        bytes,
                        PersistenceFailure::corrupt("state slot schema is invalid"),
                    ));
                }
            };
            let payload = serde_json::to_vec(&disk.payload)
                .map_err(|_| PersistenceFailure::corrupt("could not verify state payload"))?;
            let expected: [u8; 32] = Sha256::digest(&payload).into();
            if expected != disk.sha256 {
                return Ok(corrupt_slot(
                    path,
                    bytes,
                    PersistenceFailure::corrupt("state slot checksum mismatch"),
                ));
            }
            match validate_published_state(&disk.payload) {
                Ok(()) => Ok(ReadSlot::Valid(ValidatedSlot {
                    state: disk.payload,
                    schema: SlotSchema::Current,
                })),
                Err(failure) => Ok(corrupt_slot(path, bytes, failure)),
            }
        }
        PREVIOUS_STORE_SCHEMA_VERSION => {
            let disk = match serde_json::from_value::<DiskSlotV2>(value) {
                Ok(disk) => disk,
                Err(_) => {
                    return Ok(corrupt_slot(
                        path,
                        bytes,
                        PersistenceFailure::corrupt("schema-v2 state slot is invalid"),
                    ));
                }
            };
            // Verify the exact v2 payload shape before introducing v3 fields.
            let payload = serde_json::to_vec(&disk.payload).map_err(|_| {
                PersistenceFailure::corrupt("could not verify schema-v2 state payload")
            })?;
            let expected: [u8; 32] = Sha256::digest(&payload).into();
            if expected != disk.sha256 {
                return Ok(corrupt_slot(
                    path,
                    bytes,
                    PersistenceFailure::corrupt("schema-v2 state slot checksum mismatch"),
                ));
            }
            match migrate_v2_state(disk.payload) {
                Ok(state) => Ok(ReadSlot::Valid(ValidatedSlot {
                    state,
                    schema: SlotSchema::V2,
                })),
                Err(failure) => Ok(corrupt_slot(path, bytes, failure)),
            }
        }
        _ => Ok(ReadSlot::UnsupportedVersion(version)),
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotPosition {
    Primary,
    Shadow,
}

fn loaded_from_valid(
    valid: ValidatedSlot,
    position: SlotPosition,
    primary_path: &Path,
    shadow_path: &Path,
    degraded_recovery: bool,
    corrupt_evidence: Option<CorruptEvidence>,
) -> LoadedAuthoritative {
    let source = match (valid.schema, position) {
        (SlotSchema::LegacyGeometry, _) => StoreSource::LegacyGeometry,
        (_, SlotPosition::Primary) => StoreSource::Primary,
        (_, SlotPosition::Shadow) => StoreSource::Shadow,
    };
    let (authority, target) = match position {
        SlotPosition::Primary => (primary_path.to_path_buf(), shadow_path.to_path_buf()),
        SlotPosition::Shadow => (shadow_path.to_path_buf(), primary_path.to_path_buf()),
    };
    LoadedAuthoritative {
        state: valid.state,
        source,
        authority: Some(authority),
        target,
        degraded_recovery,
        corrupt_evidence,
        requires_schema_upgrade: valid.schema != SlotSchema::Current,
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
            authority: None,
            target: primary_path.to_path_buf(),
            degraded_recovery: false,
            corrupt_evidence: None,
            requires_schema_upgrade: false,
        }),
        (ReadSlot::Valid(valid), ReadSlot::Missing) => Ok(loaded_from_valid(
            valid,
            SlotPosition::Primary,
            primary_path,
            &shadow_path,
            true,
            None,
        )),
        (ReadSlot::Missing, ReadSlot::Valid(valid)) => Ok(loaded_from_valid(
            valid,
            SlotPosition::Shadow,
            primary_path,
            &shadow_path,
            true,
            None,
        )),
        (ReadSlot::Valid(primary), ReadSlot::Valid(shadow)) => {
            let primary_revision = primary.state.store_revision;
            let shadow_revision = shadow.state.store_revision;
            if primary_revision > shadow_revision {
                Ok(loaded_from_valid(
                    primary,
                    SlotPosition::Primary,
                    primary_path,
                    &shadow_path,
                    false,
                    None,
                ))
            } else if shadow_revision > primary_revision {
                Ok(loaded_from_valid(
                    shadow,
                    SlotPosition::Shadow,
                    primary_path,
                    &shadow_path,
                    false,
                    None,
                ))
            } else if primary.state == shadow.state {
                if shadow.schema.preference() > primary.schema.preference() {
                    Ok(loaded_from_valid(
                        shadow,
                        SlotPosition::Shadow,
                        primary_path,
                        &shadow_path,
                        false,
                        None,
                    ))
                } else {
                    Ok(loaded_from_valid(
                        primary,
                        SlotPosition::Primary,
                        primary_path,
                        &shadow_path,
                        false,
                        None,
                    ))
                }
            } else {
                Err(PersistenceFailure::AmbiguousGeneration {
                    revision: primary_revision,
                })
            }
        }
        (
            ReadSlot::Valid(valid),
            ReadSlot::Corrupt {
                evidence,
                failure: _,
            },
        ) => Ok(loaded_from_valid(
            valid,
            SlotPosition::Primary,
            primary_path,
            &shadow_path,
            true,
            Some(evidence),
        )),
        (
            ReadSlot::Corrupt {
                evidence,
                failure: _,
            },
            ReadSlot::Valid(valid),
        ) => Ok(loaded_from_valid(
            valid,
            SlotPosition::Shadow,
            primary_path,
            &shadow_path,
            true,
            Some(evidence),
        )),
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
        (unexpected_primary, unexpected_shadow) => Err(PersistenceFailure::corrupt(format!(
            "unsupported slot combination: primary={}, shadow={}",
            unexpected_primary.kind(),
            unexpected_shadow.kind()
        ))),
    }
}

fn load_snapshot_unlocked(primary_path: &Path) -> Result<LayoutStateSnapshot, PersistenceFailure> {
    let loaded = load_authoritative_unlocked(primary_path)?;
    Ok(LayoutStateSnapshot {
        source: loaded.source,
        degraded_recovery: loaded.degraded_recovery,
        store_revision: loaded.state.store_revision,
        window_states: loaded.state.window_states,
        domain_bindings: loaded.state.domain_bindings,
        overlays: loaded.state.overlays,
        tombstones: loaded.state.tombstones,
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

fn validate_existing_corrupt_evidence(
    path: &Path,
    expected: &[u8],
) -> Result<(), PersistenceFailure> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| PersistenceFailure::io("inspect corrupt-state evidence", error))?;
    if !metadata.file_type().is_file()
        || metadata.len() != u64::try_from(expected.len()).unwrap_or(u64::MAX)
    {
        return Err(PersistenceFailure::corrupt(
            "existing corrupt-state evidence does not match its digest identity",
        ));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| PersistenceFailure::io("open corrupt-state evidence", error))?;
    let read_limit = u64::try_from(expected.len())
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut retained = Vec::with_capacity(expected.len());
    (&mut file)
        .take(read_limit)
        .read_to_end(&mut retained)
        .map_err(|error| PersistenceFailure::io("read corrupt-state evidence", error))?;
    if retained != expected {
        return Err(PersistenceFailure::corrupt(
            "existing corrupt-state evidence does not match its digest identity",
        ));
    }
    file.sync_all()
        .map_err(|error| PersistenceFailure::io("sync corrupt-state evidence", error))?;
    sync_parent_directory(path)
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
    match std::fs::symlink_metadata(&quarantine) {
        Ok(_) => return validate_existing_corrupt_evidence(&quarantine, &evidence.bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(PersistenceFailure::io(
                "inspect corrupt-state evidence",
                error,
            ));
        }
    }
    if let Some(parent) = evidence.path.parent() {
        let prefix = format!("{name}.corrupt-");
        let retained = count_retained_evidence(parent, &prefix, "list corrupt-state evidence")?;
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
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return validate_existing_corrupt_evidence(&quarantine, &evidence.bytes);
        }
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

#[derive(Debug)]
struct OverlayPreflight {
    accepted_overlay_ids: BTreeSet<LayoutWindowId>,
    apply_overlay_ids: BTreeSet<LayoutWindowId>,
    rejected_overlay_mutations: BTreeMap<LayoutWindowId, RejectedOverlayMutation>,
    new_tombstones: usize,
}

#[derive(Debug)]
struct BatchPreflight {
    overlays: OverlayPreflight,
    accepted_workspaces: BTreeSet<String>,
    rejected_workspaces: BTreeMap<String, PersistenceFailure>,
    accepted_bindings: BTreeSet<PrivacySafeTargetFingerprint>,
    rejected_bindings: BTreeMap<PrivacySafeTargetFingerprint, PersistenceFailure>,
    encoded_upper_bound: u64,
    byte_admission: ByteAdmissionStats,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ByteAdmissionKey {
    Workspace(String),
    Binding(PrivacySafeTargetFingerprint),
    Overlay(LayoutWindowId),
}

#[derive(Clone, Debug)]
struct OverlayBudgetMutation {
    old_overlay_bytes: Option<u64>,
    new_overlay_bytes: Option<u64>,
    new_tombstone_bytes: Option<u64>,
    old_tab_count: usize,
    new_tab_count: usize,
    old_is_live: bool,
    new_is_live: bool,
    adds_tombstone: bool,
}

#[derive(Clone, Debug)]
enum ByteBudgetMutation {
    Workspace {
        old_entry_bytes: Option<u64>,
        new_entry_bytes: u64,
    },
    Binding {
        new_record_bytes: u64,
    },
    OverlayComponent {
        window_ids: Vec<LayoutWindowId>,
        mutations: Vec<OverlayBudgetMutation>,
    },
}

#[derive(Clone, Debug)]
struct ByteAdmissionCandidate {
    key: ByteAdmissionKey,
    admission_rank: u8,
    mutation: ByteBudgetMutation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AdmissionCountBudget {
    workspaces: usize,
    bindings: usize,
    live_overlays: usize,
    tombstones: usize,
    tabs: usize,
}

impl AdmissionCountBudget {
    fn from_state(state: &PersistedState) -> Result<Self, PersistenceFailure> {
        let tabs = state.overlays.iter().try_fold(0usize, |total, overlay| {
            total
                .checked_add(overlay.slots.len())
                .ok_or_else(|| PersistenceFailure::quota("total overlay tab count overflowed"))
        })?;
        Ok(Self {
            workspaces: state.window_states.len(),
            bindings: state.domain_bindings.len(),
            live_overlays: state.overlays.len(),
            tombstones: state.tombstones.len(),
            tabs,
        })
    }

    fn apply_overlay_mutations(
        &mut self,
        mutations: &[OverlayBudgetMutation],
    ) -> Result<(), PersistenceFailure> {
        for mutation in mutations {
            if mutation.old_is_live {
                self.live_overlays = self
                    .live_overlays
                    .checked_sub(1)
                    .ok_or_else(encoded_size_inconsistent)?;
            }
            if mutation.new_is_live {
                self.live_overlays = self
                    .live_overlays
                    .checked_add(1)
                    .ok_or_else(encoded_size_overflow)?;
            }
            self.tabs = self
                .tabs
                .checked_sub(mutation.old_tab_count)
                .and_then(|tabs| tabs.checked_add(mutation.new_tab_count))
                .ok_or_else(encoded_size_inconsistent)?;
            if mutation.adds_tombstone {
                self.tombstones = self
                    .tombstones
                    .checked_add(1)
                    .ok_or_else(encoded_size_overflow)?;
            }
        }
        Ok(())
    }

    fn revert_overlay_mutations(
        &mut self,
        mutations: &[OverlayBudgetMutation],
    ) -> Result<(), PersistenceFailure> {
        for mutation in mutations.iter().rev() {
            if mutation.new_is_live {
                self.live_overlays = self
                    .live_overlays
                    .checked_sub(1)
                    .ok_or_else(encoded_size_inconsistent)?;
            }
            if mutation.old_is_live {
                self.live_overlays = self
                    .live_overlays
                    .checked_add(1)
                    .ok_or_else(encoded_size_overflow)?;
            }
            self.tabs = self
                .tabs
                .checked_sub(mutation.new_tab_count)
                .and_then(|tabs| tabs.checked_add(mutation.old_tab_count))
                .ok_or_else(encoded_size_inconsistent)?;
            if mutation.adds_tombstone {
                self.tombstones = self
                    .tombstones
                    .checked_sub(1)
                    .ok_or_else(encoded_size_inconsistent)?;
            }
        }
        Ok(())
    }

    fn apply_candidate(&mut self, mutation: &ByteBudgetMutation) -> Result<(), PersistenceFailure> {
        match mutation {
            ByteBudgetMutation::Workspace {
                old_entry_bytes: None,
                ..
            } => {
                self.workspaces = self
                    .workspaces
                    .checked_add(1)
                    .ok_or_else(encoded_size_overflow)?;
            }
            ByteBudgetMutation::Binding { .. } => {
                self.bindings = self
                    .bindings
                    .checked_add(1)
                    .ok_or_else(encoded_size_overflow)?;
            }
            ByteBudgetMutation::OverlayComponent { mutations, .. } => {
                self.apply_overlay_mutations(mutations)?;
            }
            ByteBudgetMutation::Workspace {
                old_entry_bytes: Some(_),
                ..
            } => {}
        }
        Ok(())
    }

    fn revert_candidate(
        &mut self,
        mutation: &ByteBudgetMutation,
    ) -> Result<(), PersistenceFailure> {
        match mutation {
            ByteBudgetMutation::Workspace {
                old_entry_bytes: None,
                ..
            } => {
                self.workspaces = self
                    .workspaces
                    .checked_sub(1)
                    .ok_or_else(encoded_size_inconsistent)?;
            }
            ByteBudgetMutation::Binding { .. } => {
                self.bindings = self
                    .bindings
                    .checked_sub(1)
                    .ok_or_else(encoded_size_inconsistent)?;
            }
            ByteBudgetMutation::OverlayComponent { mutations, .. } => {
                self.revert_overlay_mutations(mutations)?;
            }
            ByteBudgetMutation::Workspace {
                old_entry_bytes: Some(_),
                ..
            } => {}
        }
        Ok(())
    }

    fn quota_failure(self) -> Option<PersistenceFailure> {
        if self.workspaces > MAX_WORKSPACES {
            Some(PersistenceFailure::quota(format!(
                "workspace count would exceed {MAX_WORKSPACES}"
            )))
        } else if self.bindings > MAX_DOMAIN_BINDINGS {
            Some(PersistenceFailure::quota(format!(
                "domain binding count would exceed {MAX_DOMAIN_BINDINGS}"
            )))
        } else if self.live_overlays > MAX_LAYOUT_OVERLAYS {
            Some(PersistenceFailure::quota(format!(
                "layout overlay count would exceed {MAX_LAYOUT_OVERLAYS}"
            )))
        } else if self.tombstones > MAX_OVERLAY_TOMBSTONES {
            Some(PersistenceFailure::quota(format!(
                "overlay tombstone count would exceed {MAX_OVERLAY_TOMBSTONES}"
            )))
        } else if self.tabs > MAX_TOTAL_OVERLAY_TABS {
            Some(PersistenceFailure::quota(format!(
                "total overlay tab count {} exceeds {MAX_TOTAL_OVERLAY_TABS}",
                self.tabs
            )))
        } else {
            None
        }
    }
}

impl ByteBudgetMutation {
    fn apply(&self, budget: &mut EncodedStateBudget) -> Result<(), PersistenceFailure> {
        match self {
            Self::Workspace {
                old_entry_bytes,
                new_entry_bytes,
            } => match old_entry_bytes {
                Some(old_entry_bytes) => budget
                    .window_states
                    .replace(*old_entry_bytes, *new_entry_bytes),
                None => budget.window_states.insert(*new_entry_bytes),
            },
            Self::Binding { new_record_bytes } => budget.domain_bindings.insert(*new_record_bytes),
            Self::OverlayComponent { mutations, .. } => {
                for mutation in mutations {
                    if let Some(old_overlay_bytes) = mutation.old_overlay_bytes {
                        budget.overlays.remove(old_overlay_bytes)?;
                    }
                    if let Some(new_overlay_bytes) = mutation.new_overlay_bytes {
                        budget.overlays.insert(new_overlay_bytes)?;
                    }
                    if let Some(new_tombstone_bytes) = mutation.new_tombstone_bytes {
                        budget.tombstones.insert(new_tombstone_bytes)?;
                    }
                }
                Ok(())
            }
        }
    }

    fn revert(&self, budget: &mut EncodedStateBudget) -> Result<(), PersistenceFailure> {
        match self {
            Self::Workspace {
                old_entry_bytes,
                new_entry_bytes,
            } => match old_entry_bytes {
                Some(old_entry_bytes) => budget
                    .window_states
                    .replace(*new_entry_bytes, *old_entry_bytes),
                None => budget.window_states.remove(*new_entry_bytes),
            },
            Self::Binding { new_record_bytes } => budget.domain_bindings.remove(*new_record_bytes),
            Self::OverlayComponent { mutations, .. } => {
                for mutation in mutations.iter().rev() {
                    if let Some(new_tombstone_bytes) = mutation.new_tombstone_bytes {
                        budget.tombstones.remove(new_tombstone_bytes)?;
                    }
                    if let Some(new_overlay_bytes) = mutation.new_overlay_bytes {
                        budget.overlays.remove(new_overlay_bytes)?;
                    }
                    if let Some(old_overlay_bytes) = mutation.old_overlay_bytes {
                        budget.overlays.insert(old_overlay_bytes)?;
                    }
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug)]
struct AppliedBatch {
    changed: bool,
    bindings: BTreeMap<PrivacySafeTargetFingerprint, DomainBindingId>,
    rejected_bindings: BTreeMap<PrivacySafeTargetFingerprint, PersistenceFailure>,
}

fn preflight_one_overlay_mutation(
    live: Option<&MixedDomainLayoutOverlay>,
    tombstone: Option<&OverlayTombstone>,
    mutation: &PendingOverlayMutation,
) -> Result<bool, PersistenceFailure> {
    debug_assert!(live.is_none() || tombstone.is_none());

    match (&mutation.desired, live, tombstone) {
        (DesiredOverlayState::Live(desired), Some(current), _) if desired == current => {
            return Ok(false);
        }
        (
            DesiredOverlayState::Deleted {
                last_local_revision,
                ..
            },
            _,
            Some(current),
        ) if *last_local_revision == current.last_local_revision => {
            return Ok(false);
        }
        _ => {}
    }

    match (live, tombstone) {
        (None, None) if mutation.base_revision.is_none() => Ok(true),
        (None, None) => Err(PersistenceFailure::OverlayCasConflict {
            expected: mutation.base_revision,
            committed: None,
        }),
        (Some(current), None) if mutation.base_revision == Some(current.local_revision) => Ok(true),
        (Some(current), None) => {
            let incoming = mutation.desired_revision();
            if incoming < current.local_revision {
                Err(PersistenceFailure::StaleOverlay {
                    incoming,
                    committed: current.local_revision,
                })
            } else if incoming == current.local_revision
                && matches!(&mutation.desired, DesiredOverlayState::Live(_))
            {
                Err(PersistenceFailure::OverlayRevisionConflict { revision: incoming })
            } else {
                Err(PersistenceFailure::OverlayCasConflict {
                    expected: mutation.base_revision,
                    committed: Some(current.local_revision),
                })
            }
        }
        (None, Some(current)) => Err(PersistenceFailure::RetiredOverlay {
            last_revision: current.last_local_revision,
        }),
        (Some(_), Some(_)) => Err(PersistenceFailure::invalid(
            "overlay identity is both live and tombstoned",
        )),
    }
}

fn overlay_component_tabs_are_unique(
    current_owners: &HashMap<StableTabIdentity, LayoutWindowId>,
    current_remote_owners: &HashMap<RemoteTabAuthority, LayoutWindowId>,
    batch: &PendingBatch,
    component: &[LayoutWindowId],
) -> bool {
    let component_ids = component.iter().copied().collect::<HashSet<_>>();
    let mut replacement_owners = HashMap::new();
    let mut replacement_remote_owners = HashMap::new();
    for window_id in component {
        let slots = match &batch.overlay_mutations[window_id].desired {
            DesiredOverlayState::Live(overlay) => overlay.slots.as_slice(),
            DesiredOverlayState::Deleted { .. } => &[],
        };
        for slot in slots {
            let identity = slot.identity();
            if current_owners
                .get(&identity)
                .is_some_and(|owner| !component_ids.contains(owner))
                || replacement_owners.insert(identity, *window_id).is_some()
            {
                return false;
            }
            if let Some(authority) = slot.remote_authority()
                && (current_remote_owners
                    .get(&authority)
                    .is_some_and(|owner| !component_ids.contains(owner))
                    || replacement_remote_owners
                        .insert(authority, *window_id)
                        .is_some())
            {
                return false;
            }
        }
    }
    true
}

fn preflight_overlay_mutations(
    state: &PersistedState,
    batch: &PendingBatch,
) -> Result<OverlayPreflight, PersistenceFailure> {
    validate_state(state)?;
    let live_by_id = state
        .overlays
        .iter()
        .map(|overlay| (overlay.window_id, overlay))
        .collect::<BTreeMap<_, _>>();
    let tombstone_by_id = state
        .tombstones
        .iter()
        .map(|tombstone| (tombstone.window_id, tombstone))
        .collect::<BTreeMap<_, _>>();
    // Only IDs already present in the loaded durable authority authenticate
    // overlay slots. `batch.ensure_bindings` requests future allocation; it
    // cannot authorize a guessed, not-yet-published ID in the same batch.
    let authoritative_binding_ids = state
        .domain_bindings
        .iter()
        .map(|record| record.binding_id)
        .collect::<HashSet<_>>();
    let mut accepted_overlay_ids = BTreeSet::new();
    let mut apply_overlay_ids = BTreeSet::new();
    let mut rejected_overlay_mutations = BTreeMap::new();

    for (window_id, mutation) in &batch.overlay_mutations {
        debug_assert_eq!(*window_id, mutation.window_id());
        if let DesiredOverlayState::Live(overlay) = &mutation.desired
            && overlay.slots.iter().any(|slot| {
                slot.remote_binding()
                    .is_some_and(|binding| !authoritative_binding_ids.contains(&binding))
            })
        {
            rejected_overlay_mutations.insert(
                *window_id,
                RejectedOverlayMutation {
                    mutation: mutation.clone(),
                    failure: PersistenceFailure::invalid(
                        "overlay remote slot references an unknown domain binding",
                    ),
                },
            );
            continue;
        }
        match preflight_one_overlay_mutation(
            live_by_id.get(window_id).copied(),
            tombstone_by_id.get(window_id).copied(),
            mutation,
        ) {
            Ok(false) => {
                accepted_overlay_ids.insert(*window_id);
            }
            Ok(true) => {
                accepted_overlay_ids.insert(*window_id);
                apply_overlay_ids.insert(*window_id);
            }
            Err(failure) => {
                rejected_overlay_mutations.insert(
                    *window_id,
                    RejectedOverlayMutation {
                        mutation: mutation.clone(),
                        failure,
                    },
                );
            }
        }
    }

    Ok(OverlayPreflight {
        accepted_overlay_ids,
        apply_overlay_ids,
        rejected_overlay_mutations,
        new_tombstones: 0,
    })
}

fn find_overlay_component(parent: &mut [usize], index: usize) -> usize {
    let mut root = index;
    while parent[root] != root {
        root = parent[root];
    }
    let mut cursor = index;
    while parent[cursor] != cursor {
        let next = parent[cursor];
        parent[cursor] = root;
        cursor = next;
    }
    root
}

fn union_overlay_components(parent: &mut [usize], left: usize, right: usize) {
    let left_root = find_overlay_component(parent, left);
    let right_root = find_overlay_component(parent, right);
    if left_root == right_root {
        return;
    }
    let (root, child) = if left_root < right_root {
        (left_root, right_root)
    } else {
        (right_root, left_root)
    };
    parent[child] = root;
}

fn overlay_admission_components(
    state: &PersistedState,
    batch: &PendingBatch,
) -> Vec<Vec<LayoutWindowId>> {
    // Ownership is a property of the complete requested transition, not only
    // of mutations that passed their individual revision preflight.  A
    // rejected destination can still claim a slot released by an otherwise
    // valid source.  Omitting that destination would let the source commit its
    // half of the transfer and destroy the atomic ownership hand-off.
    let window_ids = batch.overlay_mutations.keys().copied().collect::<Vec<_>>();
    let index_by_window = window_ids
        .iter()
        .copied()
        .enumerate()
        .map(|(index, window_id)| (window_id, index))
        .collect::<BTreeMap<_, _>>();
    let live_by_window = state
        .overlays
        .iter()
        .map(|overlay| (overlay.window_id, overlay))
        .collect::<BTreeMap<_, _>>();
    let mut parent = (0..window_ids.len()).collect::<Vec<_>>();
    let mut first_delta_owner = HashMap::new();
    let mut first_remote_delta_owner = HashMap::new();

    for window_id in &window_ids {
        let index = index_by_window[window_id];
        let old_slots = live_by_window
            .get(window_id)
            .map_or(&[][..], |overlay| overlay.slots.as_slice())
            .iter()
            .map(|slot| slot.identity())
            .collect::<HashSet<_>>();
        let new_slots = match &batch.overlay_mutations[window_id].desired {
            DesiredOverlayState::Live(overlay) => overlay.slots.as_slice(),
            DesiredOverlayState::Deleted { .. } => &[],
        }
        .iter()
        .map(|slot| slot.identity())
        .collect::<HashSet<_>>();

        for identity in old_slots.symmetric_difference(&new_slots).copied() {
            if let Some(previous) = first_delta_owner.insert(identity, index) {
                union_overlay_components(&mut parent, previous, index);
            }
        }

        let old_remote_tabs = live_by_window
            .get(window_id)
            .map_or(&[][..], |overlay| overlay.slots.as_slice())
            .iter()
            .filter_map(|slot| slot.remote_authority())
            .collect::<HashSet<_>>();
        let new_remote_tabs = match &batch.overlay_mutations[window_id].desired {
            DesiredOverlayState::Live(overlay) => overlay.slots.as_slice(),
            DesiredOverlayState::Deleted { .. } => &[],
        }
        .iter()
        .filter_map(|slot| slot.remote_authority())
        .collect::<HashSet<_>>();
        for authority in old_remote_tabs
            .symmetric_difference(&new_remote_tabs)
            .copied()
        {
            if let Some(previous) = first_remote_delta_owner.insert(authority, index) {
                union_overlay_components(&mut parent, previous, index);
            }
        }
    }

    let mut by_root = BTreeMap::<usize, Vec<LayoutWindowId>>::new();
    for (index, window_id) in window_ids.into_iter().enumerate() {
        let root = find_overlay_component(&mut parent, index);
        by_root.entry(root).or_default().push(window_id);
    }
    let mut components = by_root.into_values().collect::<Vec<_>>();
    components.sort_by_key(|component| component[0]);
    components
}

const fn overlay_preflight_failure_precedence(failure: &PersistenceFailure) -> u8 {
    match failure {
        PersistenceFailure::Invalid { .. } => 0,
        PersistenceFailure::RetiredOverlay { .. } => 1,
        PersistenceFailure::StaleOverlay { .. } => 2,
        PersistenceFailure::OverlayRevisionConflict { .. } => 3,
        PersistenceFailure::OverlayCasConflict { .. } => 4,
        PersistenceFailure::RevisionExhausted => 5,
        PersistenceFailure::Quota { .. } => 6,
        PersistenceFailure::EncodedQuota { .. } | PersistenceFailure::Oversized { .. } => 7,
        PersistenceFailure::Corrupt { .. } => 8,
        PersistenceFailure::UnsupportedVersion { .. } => 9,
        PersistenceFailure::AmbiguousGeneration { .. } => 10,
        PersistenceFailure::Io { .. } => 11,
        PersistenceFailure::WorkerPanicked => 12,
        PersistenceFailure::WorkerStopped => 13,
    }
}

fn build_byte_admission_candidates(
    state: &PersistedState,
    batch: &PendingBatch,
    preflight: &BatchPreflight,
    overlay_components: &[Vec<LayoutWindowId>],
    published_revision: u64,
    initial_budget: &EncodedStateBudget,
) -> Result<Vec<ByteAdmissionCandidate>, PersistenceFailure> {
    let mut candidates = Vec::new();
    let initial_upper_bound = initial_budget.upper_bound()?;

    for workspace in &preflight.accepted_workspaces {
        let desired = &batch.window_states[workspace];
        let current = state.window_states.get(workspace);
        if current == Some(desired) {
            continue;
        }
        let mutation = ByteBudgetMutation::Workspace {
            old_entry_bytes: current
                .map(|current| window_state_entry_len(workspace, current))
                .transpose()?,
            new_entry_bytes: window_state_entry_len(workspace, desired)?,
        };
        let mut projected = *initial_budget;
        mutation.apply(&mut projected)?;
        candidates.push(ByteAdmissionCandidate {
            key: ByteAdmissionKey::Workspace(workspace.clone()),
            admission_rank: if projected.upper_bound()? > initial_upper_bound {
                2
            } else {
                0
            },
            mutation,
        });
    }

    let existing_bindings = state
        .domain_bindings
        .iter()
        .map(|record| record.target_fingerprint)
        .collect::<HashSet<_>>();
    for fingerprint in &preflight.accepted_bindings {
        if existing_bindings.contains(fingerprint) {
            continue;
        }
        let mutation = ByteBudgetMutation::Binding {
            new_record_bytes: maximum_width_binding_len(*fingerprint)?,
        };
        let mut projected = *initial_budget;
        mutation.apply(&mut projected)?;
        candidates.push(ByteAdmissionCandidate {
            key: ByteAdmissionKey::Binding(*fingerprint),
            admission_rank: if projected.upper_bound()? > initial_upper_bound {
                2
            } else {
                0
            },
            mutation,
        });
    }

    let overlays_by_id = state
        .overlays
        .iter()
        .map(|overlay| (overlay.window_id, overlay))
        .collect::<BTreeMap<_, _>>();
    for window_ids in overlay_components {
        let mut mutations = Vec::with_capacity(window_ids.len());
        for window_id in window_ids {
            let old_overlay = overlays_by_id.get(window_id).copied();
            let old_overlay_bytes = old_overlay.map(encoded_json_len).transpose()?;
            let old_tab_count = old_overlay.map_or(0, |overlay| overlay.slots.len());
            let (new_overlay_bytes, new_tombstone_bytes) =
                match &batch.overlay_mutations[window_id].desired {
                    DesiredOverlayState::Live(overlay) => (Some(encoded_json_len(overlay)?), None),
                    DesiredOverlayState::Deleted {
                        last_local_revision,
                        ..
                    } => {
                        let tombstone = OverlayTombstone::new(
                            *window_id,
                            *last_local_revision,
                            published_revision,
                        )?;
                        (None, Some(encoded_json_len(&tombstone)?))
                    }
                };
            mutations.push(OverlayBudgetMutation {
                old_overlay_bytes,
                new_overlay_bytes,
                new_tombstone_bytes,
                old_tab_count,
                new_tab_count: batch.overlay_mutations[window_id].live_tab_count(),
                old_is_live: old_overlay.is_some(),
                new_is_live: matches!(
                    &batch.overlay_mutations[window_id].desired,
                    DesiredOverlayState::Live(_)
                ),
                adds_tombstone: matches!(
                    &batch.overlay_mutations[window_id].desired,
                    DesiredOverlayState::Deleted { .. }
                ),
            });
        }
        let contains_delete = mutations.iter().any(|mutation| mutation.adds_tombstone);
        let mutation = ByteBudgetMutation::OverlayComponent {
            window_ids: window_ids.to_vec(),
            mutations,
        };
        let mut projected = *initial_budget;
        mutation.apply(&mut projected)?;
        let admission_rank = if projected.upper_bound()? <= initial_upper_bound {
            0
        } else if contains_delete {
            1
        } else {
            2
        };
        candidates.push(ByteAdmissionCandidate {
            key: ByteAdmissionKey::Overlay(window_ids[0]),
            admission_rank,
            mutation,
        });
    }

    Ok(candidates)
}

fn byte_quota_failure(projected_upper_bound: u64, maximum_bytes: u64) -> PersistenceFailure {
    PersistenceFailure::EncodedQuota {
        projected_upper_bound,
        maximum: maximum_bytes,
    }
}

fn reject_overlay_admission_component(
    window_ids: &[LayoutWindowId],
    batch: &PendingBatch,
    preflight: &mut BatchPreflight,
    failure: PersistenceFailure,
) {
    for window_id in window_ids {
        preflight.overlays.accepted_overlay_ids.remove(window_id);
        preflight.overlays.apply_overlay_ids.remove(window_id);
        preflight.overlays.rejected_overlay_mutations.insert(
            *window_id,
            RejectedOverlayMutation {
                mutation: batch.overlay_mutations[window_id].clone(),
                failure: failure.clone(),
            },
        );
    }
}

fn reject_byte_admission_candidate(
    candidate: &ByteAdmissionCandidate,
    batch: &PendingBatch,
    preflight: &mut BatchPreflight,
    failure: PersistenceFailure,
) {
    match (&candidate.key, &candidate.mutation) {
        (ByteAdmissionKey::Workspace(workspace), ByteBudgetMutation::Workspace { .. }) => {
            preflight.accepted_workspaces.remove(workspace);
            preflight
                .rejected_workspaces
                .insert(workspace.clone(), failure);
        }
        (ByteAdmissionKey::Binding(fingerprint), ByteBudgetMutation::Binding { .. }) => {
            preflight.accepted_bindings.remove(fingerprint);
            preflight.rejected_bindings.insert(*fingerprint, failure);
        }
        (ByteAdmissionKey::Overlay(_), ByteBudgetMutation::OverlayComponent { window_ids, .. }) => {
            reject_overlay_admission_component(window_ids, batch, preflight, failure);
        }
        _ => unreachable!("byte-admission key and mutation kind must agree"),
    }
}

fn partition_overlay_ownership_components(
    state: &PersistedState,
    batch: &PendingBatch,
    preflight: &mut BatchPreflight,
) -> Vec<Vec<LayoutWindowId>> {
    let components = overlay_admission_components(state, batch);
    let current_owners = state
        .overlays
        .iter()
        .flat_map(|overlay| {
            overlay
                .slots
                .iter()
                .map(move |slot| (slot.identity(), overlay.window_id))
        })
        .collect::<HashMap<_, _>>();
    let current_remote_owners = state
        .overlays
        .iter()
        .flat_map(|overlay| {
            overlay.slots.iter().filter_map(move |slot| {
                slot.remote_authority()
                    .map(|authority| (authority, overlay.window_id))
            })
        })
        .collect::<HashMap<_, _>>();
    let mut valid_components = Vec::with_capacity(components.len());
    for component in components {
        let component_failure = component
            .iter()
            .filter_map(|window_id| {
                preflight
                    .overlays
                    .rejected_overlay_mutations
                    .get(window_id)
                    .map(|rejected| {
                        (
                            overlay_preflight_failure_precedence(&rejected.failure),
                            *window_id,
                            &rejected.failure,
                        )
                    })
            })
            .min_by_key(|(precedence, window_id, _)| (*precedence, *window_id))
            .map(|(_, _, failure)| failure.clone());
        if let Some(failure) = component_failure {
            reject_overlay_admission_component(&component, batch, preflight, failure);
            continue;
        }

        if !overlay_component_tabs_are_unique(
            &current_owners,
            &current_remote_owners,
            batch,
            &component,
        ) {
            reject_overlay_admission_component(
                &component,
                batch,
                preflight,
                PersistenceFailure::invalid(
                    "one stable tab identity would be owned by multiple layout windows",
                ),
            );
            continue;
        }

        let applied_component = component
            .into_iter()
            .filter(|window_id| preflight.overlays.apply_overlay_ids.contains(window_id))
            .collect::<Vec<_>>();
        if !applied_component.is_empty() {
            valid_components.push(applied_component);
        }
    }
    valid_components
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AdmissionProjection {
    normalized_bytes: EncodedStateBudget,
    physical_bytes: EncodedStateBudget,
    counts: AdmissionCountBudget,
}

impl AdmissionProjection {
    fn apply(&mut self, candidate: &ByteAdmissionCandidate) -> Result<(), PersistenceFailure> {
        candidate.mutation.apply(&mut self.normalized_bytes)?;
        candidate.mutation.apply(&mut self.physical_bytes)?;
        self.counts.apply_candidate(&candidate.mutation)
    }

    fn revert(&mut self, candidate: &ByteAdmissionCandidate) -> Result<(), PersistenceFailure> {
        candidate.mutation.revert(&mut self.normalized_bytes)?;
        candidate.mutation.revert(&mut self.physical_bytes)?;
        self.counts.revert_candidate(&candidate.mutation)
    }
}

const VIOLATION_WORKSPACES: u8 = 1 << 0;
const VIOLATION_BINDINGS: u8 = 1 << 1;
const VIOLATION_LIVE_OVERLAYS: u8 = 1 << 2;
const VIOLATION_TOMBSTONES: u8 = 1 << 3;
const VIOLATION_TABS: u8 = 1 << 4;
const VIOLATION_NORMALIZED_BYTES: u8 = 1 << 5;
const VIOLATION_PHYSICAL_BYTES: u8 = 1 << 6;
const ADMISSION_RESOURCE_BITS: [u8; 7] = [
    VIOLATION_WORKSPACES,
    VIOLATION_BINDINGS,
    VIOLATION_LIVE_OVERLAYS,
    VIOLATION_TOMBSTONES,
    VIOLATION_TABS,
    VIOLATION_NORMALIZED_BYTES,
    VIOLATION_PHYSICAL_BYTES,
];
const ADMISSION_SUPPORT_MASKS: usize = 1 << ADMISSION_RESOURCE_BITS.len();

fn admission_violation_mask(
    base: AdmissionProjection,
    projected: AdmissionProjection,
    maximum_bytes: u64,
    has_candidates: bool,
) -> Result<u8, PersistenceFailure> {
    let mut mask = 0u8;
    if projected.counts.workspaces > MAX_WORKSPACES {
        mask |= VIOLATION_WORKSPACES;
    }
    if projected.counts.bindings > MAX_DOMAIN_BINDINGS {
        mask |= VIOLATION_BINDINGS;
    }
    if projected.counts.live_overlays > MAX_LAYOUT_OVERLAYS {
        mask |= VIOLATION_LIVE_OVERLAYS;
    }
    if projected.counts.tombstones > MAX_OVERLAY_TOMBSTONES {
        mask |= VIOLATION_TOMBSTONES;
    }
    if projected.counts.tabs > MAX_TOTAL_OVERLAY_TABS {
        mask |= VIOLATION_TABS;
    }

    let normalized = projected.normalized_bytes.upper_bound()?;
    if normalized > maximum_bytes {
        let base_normalized = base.normalized_bytes.upper_bound()?;
        if normalized > base_normalized {
            mask |= VIOLATION_NORMALIZED_BYTES;
        } else if has_candidates && projected.physical_bytes.upper_bound()? > maximum_bytes {
            // Existing durable binding identifiers keep their exact physical
            // widths here, while every newly allocated identifier and the
            // checksum remain maximum-width. This escape admits a genuine
            // reduction from conservative normalization debt only when the
            // resulting file is proven below the physical byte limit. The
            // candidate-free base remains admissible because it does not
            // publish a new generation.
            mask |= VIOLATION_PHYSICAL_BYTES;
        }
    }
    Ok(mask)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AdmissionDelta {
    values: [i128; ADMISSION_RESOURCE_BITS.len()],
}

impl AdmissionDelta {
    fn for_candidate(
        base: AdmissionProjection,
        candidate: &ByteAdmissionCandidate,
    ) -> Result<Self, PersistenceFailure> {
        let mut projected = base;
        projected.apply(candidate)?;
        let base_normalized = base.normalized_bytes.upper_bound()?;
        let projected_normalized = projected.normalized_bytes.upper_bound()?;
        let base_physical = base.physical_bytes.upper_bound()?;
        let projected_physical = projected.physical_bytes.upper_bound()?;
        Ok(Self {
            values: [
                signed_usize_delta(projected.counts.workspaces, base.counts.workspaces)?,
                signed_usize_delta(projected.counts.bindings, base.counts.bindings)?,
                signed_usize_delta(projected.counts.live_overlays, base.counts.live_overlays)?,
                signed_usize_delta(projected.counts.tombstones, base.counts.tombstones)?,
                signed_usize_delta(projected.counts.tabs, base.counts.tabs)?,
                i128::from(projected_normalized) - i128::from(base_normalized),
                i128::from(projected_physical) - i128::from(base_physical),
            ],
        })
    }

    fn positively_contributes(self, resource: usize) -> bool {
        self.values[resource] > 0
    }

    fn positive_magnitude(self, resource: usize) -> u64 {
        u64::try_from(self.values[resource].max(0)).unwrap_or(u64::MAX)
    }

    fn support_magnitude(self, resource: usize) -> u64 {
        u64::try_from(self.values[resource].saturating_neg().max(0)).unwrap_or(u64::MAX)
    }

    fn support_count(self) -> u64 {
        u64::try_from(self.values.iter().filter(|value| **value < 0).count()).unwrap_or(u64::MAX)
    }

    fn support_mask(self) -> u8 {
        self.values
            .iter()
            .enumerate()
            .fold(0u8, |mask, (resource, value)| {
                if *value < 0 {
                    mask | ADMISSION_RESOURCE_BITS[resource]
                } else {
                    mask
                }
            })
    }
}

fn signed_usize_delta(after: usize, before: usize) -> Result<i128, PersistenceFailure> {
    let after = u64::try_from(after).map_err(|_| encoded_size_overflow())?;
    let before = u64::try_from(before).map_err(|_| encoded_size_overflow())?;
    Ok(i128::from(after) - i128::from(before))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ByteAdmissionStats {
    candidate_count: usize,
    leave_one_out_trials: usize,
    peel_removals: usize,
    backfill_queries: usize,
    backfill_candidate_trials: usize,
    final_rejection_trials: usize,
    backfill_index_rebuilds: usize,
    backfill_index_entries_peak: usize,
}

#[derive(Debug)]
struct CandidateSubsetSelection {
    projection: AdmissionProjection,
    rejected: Vec<usize>,
    accepted_count: usize,
    stats: ByteAdmissionStats,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AdmissionCollectionShape([bool; 4]);

impl AdmissionCollectionShape {
    fn from_projection(projection: AdmissionProjection) -> Result<Self, PersistenceFailure> {
        let normalized = projection.normalized_bytes;
        let physical = projection.physical_bytes;
        let normalized_counts = [
            normalized.window_states.item_count,
            normalized.domain_bindings.item_count,
            normalized.overlays.item_count,
            normalized.tombstones.item_count,
        ];
        let physical_counts = [
            physical.window_states.item_count,
            physical.domain_bindings.item_count,
            physical.overlays.item_count,
            physical.tombstones.item_count,
        ];
        if normalized_counts != physical_counts {
            return Err(PersistenceFailure::corrupt(
                "normalized and physical byte projections disagree on collection cardinality",
            ));
        }
        Ok(Self(normalized_counts.map(|count| count == 0)))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BackfillPoint {
    candidate_index: usize,
    live_overlay_delta: i128,
    tab_delta: i128,
    normalized_byte_delta: i128,
}

impl BackfillPoint {
    fn for_candidate(
        projection: AdmissionProjection,
        candidate_index: usize,
        candidate: &ByteAdmissionCandidate,
    ) -> Result<Self, PersistenceFailure> {
        let delta = AdmissionDelta::for_candidate(projection, candidate)?;
        for resource in [0usize, 1, 3] {
            if delta.values[resource] < 0 {
                return Err(PersistenceFailure::corrupt(
                    "byte-admission candidate frees a monotonic cardinality resource",
                ));
            }
        }
        if delta.values[5] != delta.values[6] {
            return Err(PersistenceFailure::corrupt(
                "normalized and physical candidate byte deltas diverged",
            ));
        }
        Ok(Self {
            candidate_index,
            live_overlay_delta: delta.values[2],
            tab_delta: delta.values[4],
            normalized_byte_delta: delta.values[5],
        })
    }
}

type DominanceMinimum = (i128, usize);

fn minimum_dominance(
    left: Option<DominanceMinimum>,
    right: Option<DominanceMinimum>,
) -> Option<DominanceMinimum> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

#[derive(Debug)]
struct DominanceBucket {
    keys: Vec<(i128, usize)>,
    tree_base: usize,
    minima: Vec<Option<DominanceMinimum>>,
}

impl DominanceBucket {
    fn new(mut points: Vec<BackfillPoint>) -> Result<Self, PersistenceFailure> {
        points.sort_by_key(|point| (point.tab_delta, point.candidate_index));
        let keys = points
            .iter()
            .map(|point| (point.tab_delta, point.candidate_index))
            .collect::<Vec<_>>();
        let tree_base = points
            .len()
            .max(1)
            .checked_next_power_of_two()
            .ok_or_else(encoded_size_overflow)?;
        let tree_len = tree_base.checked_mul(2).ok_or_else(encoded_size_overflow)?;
        let mut minima = vec![None; tree_len];
        for (position, point) in points.into_iter().enumerate() {
            minima[tree_base + position] =
                Some((point.normalized_byte_delta, point.candidate_index));
        }
        for node in (1..tree_base).rev() {
            minima[node] = minimum_dominance(minima[node * 2], minima[node * 2 + 1]);
        }
        Ok(Self {
            keys,
            tree_base,
            minima,
        })
    }

    fn remove(&mut self, point: BackfillPoint) -> Result<(), PersistenceFailure> {
        let position = self
            .keys
            .binary_search(&(point.tab_delta, point.candidate_index))
            .map_err(|_| {
                PersistenceFailure::corrupt("dominance index lost a byte-admission candidate key")
            })?;
        let mut node = self.tree_base + position;
        if self.minima[node].take().is_none() {
            return Err(PersistenceFailure::corrupt(
                "dominance index removed one byte-admission candidate twice",
            ));
        }
        node /= 2;
        while node != 0 {
            self.minima[node] = minimum_dominance(self.minima[node * 2], self.minima[node * 2 + 1]);
            node /= 2;
        }
        Ok(())
    }

    fn query(&self, maximum_tab_delta: i128, maximum_byte_delta: i128) -> Option<DominanceMinimum> {
        let upper = self
            .keys
            .partition_point(|(tab_delta, _)| *tab_delta <= maximum_tab_delta);
        let mut left = self.tree_base;
        let mut right = self.tree_base + upper;
        let mut selected = None;
        while left < right {
            if left % 2 == 1 {
                selected = minimum_dominance(selected, self.minima[left]);
                left += 1;
            }
            if right % 2 == 1 {
                right -= 1;
                selected = minimum_dominance(selected, self.minima[right]);
            }
            left /= 2;
            right /= 2;
        }
        selected.filter(|(byte_delta, _)| *byte_delta <= maximum_byte_delta)
    }
}

#[derive(Debug)]
struct BackfillDominanceIndex {
    live_overlay_deltas: Vec<i128>,
    tree_base: usize,
    buckets: Vec<DominanceBucket>,
    points_by_candidate: Vec<Option<BackfillPoint>>,
    entry_count: usize,
}

impl BackfillDominanceIndex {
    fn build(
        points: Vec<BackfillPoint>,
        candidate_count: usize,
    ) -> Result<Self, PersistenceFailure> {
        let mut live_overlay_deltas = points
            .iter()
            .map(|point| point.live_overlay_delta)
            .collect::<Vec<_>>();
        live_overlay_deltas.sort_unstable();
        live_overlay_deltas.dedup();
        let tree_base = live_overlay_deltas
            .len()
            .max(1)
            .checked_next_power_of_two()
            .ok_or_else(encoded_size_overflow)?;
        let tree_len = tree_base.checked_mul(2).ok_or_else(encoded_size_overflow)?;
        let mut pending_buckets = vec![Vec::new(); tree_len];
        let mut points_by_candidate = vec![None; candidate_count];
        let mut entry_count = 0usize;

        for point in points {
            let slot = points_by_candidate
                .get_mut(point.candidate_index)
                .ok_or_else(|| {
                    PersistenceFailure::corrupt(
                        "dominance index candidate identity was out of bounds",
                    )
                })?;
            if slot.replace(point).is_some() {
                return Err(PersistenceFailure::corrupt(
                    "dominance index received a duplicate byte-admission candidate",
                ));
            }
            let coordinate = live_overlay_deltas
                .binary_search(&point.live_overlay_delta)
                .map_err(|_| {
                    PersistenceFailure::corrupt("dominance index lost a live-overlay coordinate")
                })?;
            let mut node = tree_base + coordinate;
            while node != 0 {
                pending_buckets[node].push(point);
                entry_count = entry_count
                    .checked_add(1)
                    .ok_or_else(encoded_size_overflow)?;
                node /= 2;
            }
        }

        let buckets = pending_buckets
            .into_iter()
            .map(DominanceBucket::new)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            live_overlay_deltas,
            tree_base,
            buckets,
            points_by_candidate,
            entry_count,
        })
    }

    fn remove(&mut self, candidate_index: usize) -> Result<(), PersistenceFailure> {
        let point = self
            .points_by_candidate
            .get_mut(candidate_index)
            .and_then(Option::take)
            .ok_or_else(|| {
                PersistenceFailure::corrupt(
                    "dominance index removed an absent byte-admission candidate",
                )
            })?;
        let coordinate = self
            .live_overlay_deltas
            .binary_search(&point.live_overlay_delta)
            .map_err(|_| {
                PersistenceFailure::corrupt("dominance index removal lost its coordinate")
            })?;
        let mut node = self.tree_base + coordinate;
        while node != 0 {
            self.buckets[node].remove(point)?;
            node /= 2;
        }
        Ok(())
    }

    fn query(
        &self,
        maximum_live_overlay_delta: i128,
        maximum_tab_delta: i128,
        maximum_byte_delta: i128,
    ) -> Option<usize> {
        let upper = self
            .live_overlay_deltas
            .partition_point(|delta| *delta <= maximum_live_overlay_delta);
        let mut left = self.tree_base;
        let mut right = self.tree_base + upper;
        let mut selected = None;
        while left < right {
            if left % 2 == 1 {
                selected = minimum_dominance(
                    selected,
                    self.buckets[left].query(maximum_tab_delta, maximum_byte_delta),
                );
                left += 1;
            }
            if right % 2 == 1 {
                right -= 1;
                selected = minimum_dominance(
                    selected,
                    self.buckets[right].query(maximum_tab_delta, maximum_byte_delta),
                );
            }
            left /= 2;
            right /= 2;
        }
        selected.map(|(_, candidate_index)| candidate_index)
    }
}

type RemovalPriority = [u64; 17];
type RemovalHeap = BinaryHeap<(RemovalPriority, usize)>;

fn removal_priority(
    violation_mask: u8,
    candidate: &ByteAdmissionCandidate,
    delta: AdmissionDelta,
) -> RemovalPriority {
    let mut priority = [0; 17];
    let active_supports = u64::from((delta.support_mask() & violation_mask).count_ones());
    priority[0] = u64::try_from(ADMISSION_RESOURCE_BITS.len())
        .unwrap_or(u64::MAX)
        .saturating_sub(active_supports);
    priority[1] = u64::try_from(ADMISSION_RESOURCE_BITS.len())
        .unwrap_or(u64::MAX)
        .saturating_sub(delta.support_count());
    let preferred_order = [5, 6, 4, 2, 3, 0, 1];
    for (offset, resource) in preferred_order.into_iter().enumerate() {
        priority[2 + offset] = u64::MAX.saturating_sub(delta.support_magnitude(resource));
        priority[10 + offset] = delta.positive_magnitude(resource);
    }
    priority[9] = u64::from(candidate.admission_rank);
    priority
}

const MONOTONIC_ADMISSION_VIOLATIONS: u8 =
    VIOLATION_WORKSPACES | VIOLATION_BINDINGS | VIOLATION_TOMBSTONES;
const MAX_BACKFILL_INDEX_REBUILDS: usize = 1 + 2 * 4;

fn normalized_admission_ceiling(
    base: AdmissionProjection,
    maximum_bytes: u64,
) -> Result<u64, PersistenceFailure> {
    let normalized = base.normalized_bytes.upper_bound()?;
    let physical = base.physical_bytes.upper_bound()?;
    let normalization_debt = normalized.checked_sub(physical).ok_or_else(|| {
        PersistenceFailure::corrupt(
            "physical byte projection exceeded its conservative normalized projection",
        )
    })?;
    let physical_ceiling_as_normalized = maximum_bytes.saturating_add(normalization_debt);
    Ok(maximum_bytes.max(normalized.min(physical_ceiling_as_normalized)))
}

fn admission_backfill_slack(
    projection: AdmissionProjection,
    normalized_byte_ceiling: u64,
) -> Result<(i128, i128, i128), PersistenceFailure> {
    let live_overlays =
        u64::try_from(projection.counts.live_overlays).map_err(|_| encoded_size_overflow())?;
    let maximum_live_overlays =
        u64::try_from(MAX_LAYOUT_OVERLAYS).map_err(|_| encoded_size_overflow())?;
    let tabs = u64::try_from(projection.counts.tabs).map_err(|_| encoded_size_overflow())?;
    let maximum_tabs =
        u64::try_from(MAX_TOTAL_OVERLAY_TABS).map_err(|_| encoded_size_overflow())?;
    let normalized_bytes = projection.normalized_bytes.upper_bound()?;
    Ok((
        i128::from(maximum_live_overlays) - i128::from(live_overlays),
        i128::from(maximum_tabs) - i128::from(tabs),
        i128::from(normalized_byte_ceiling) - i128::from(normalized_bytes),
    ))
}

fn build_backfill_dominance_index(
    projection: AdmissionProjection,
    candidates: &[ByteAdmissionCandidate],
    pending_backfill: &[bool],
    stats: &mut ByteAdmissionStats,
) -> Result<BackfillDominanceIndex, PersistenceFailure> {
    if stats.backfill_index_rebuilds == MAX_BACKFILL_INDEX_REBUILDS {
        return Err(PersistenceFailure::corrupt(
            "byte-admission collection shape changed more often than its bounded mutation model",
        ));
    }
    stats.backfill_index_rebuilds += 1;
    let points = candidates
        .iter()
        .enumerate()
        .filter(|(index, _)| pending_backfill[*index])
        .map(|(index, candidate)| BackfillPoint::for_candidate(projection, index, candidate))
        .collect::<Result<Vec<_>, _>>()?;
    let index = BackfillDominanceIndex::build(points, candidates.len())?;
    stats.backfill_index_entries_peak = stats.backfill_index_entries_peak.max(index.entry_count);
    Ok(index)
}

fn restore_inclusion_maximal_subset(
    base: AdmissionProjection,
    candidates: &[ByteAdmissionCandidate],
    aggregate: &mut AdmissionProjection,
    remaining: &mut [bool],
    pending_backfill: &mut [bool],
    remaining_count: &mut usize,
    maximum_bytes: u64,
    stats: &mut ByteAdmissionStats,
) -> Result<(), PersistenceFailure> {
    let normalized_byte_ceiling = normalized_admission_ceiling(base, maximum_bytes)?;
    let mut shape = AdmissionCollectionShape::from_projection(*aggregate)?;
    let mut index =
        build_backfill_dominance_index(*aggregate, candidates, pending_backfill, stats)?;

    loop {
        let (live_overlay_slack, tab_slack, byte_slack) =
            admission_backfill_slack(*aggregate, normalized_byte_ceiling)?;
        stats.backfill_queries = stats.backfill_queries.saturating_add(1);
        let Some(candidate_index) = index.query(live_overlay_slack, tab_slack, byte_slack) else {
            break;
        };
        if !pending_backfill[candidate_index] || remaining[candidate_index] {
            return Err(PersistenceFailure::corrupt(
                "dominance index selected a byte-admission candidate outside backfill",
            ));
        }

        stats.backfill_candidate_trials = stats.backfill_candidate_trials.saturating_add(1);
        let mut projected = *aggregate;
        projected.apply(&candidates[candidate_index])?;
        let violation_mask = admission_violation_mask(base, projected, maximum_bytes, true)?;
        index.remove(candidate_index)?;
        pending_backfill[candidate_index] = false;

        if violation_mask == 0 {
            *aggregate = projected;
            remaining[candidate_index] = true;
            *remaining_count = remaining_count
                .checked_add(1)
                .ok_or_else(encoded_size_overflow)?;
            let next_shape = AdmissionCollectionShape::from_projection(*aggregate)?;
            if next_shape != shape {
                shape = next_shape;
                index = build_backfill_dominance_index(
                    *aggregate,
                    candidates,
                    pending_backfill,
                    stats,
                )?;
            }
        } else if violation_mask & !MONOTONIC_ADMISSION_VIOLATIONS != 0 {
            return Err(PersistenceFailure::corrupt(
                "dominance index selected a candidate that violates a mixed-sign resource",
            ));
        }
    }
    Ok(())
}

fn select_compatible_candidate_subset(
    base: AdmissionProjection,
    candidates: &[ByteAdmissionCandidate],
    pending: &[usize],
    maximum_bytes: u64,
    force_write: bool,
) -> Result<CandidateSubsetSelection, PersistenceFailure> {
    let mut stats = ByteAdmissionStats {
        candidate_count: pending.len(),
        ..ByteAdmissionStats::default()
    };
    let mut aggregate = base;
    let mut remaining = vec![false; candidates.len()];
    let mut remaining_count = pending.len();
    for index in pending {
        let candidate = candidates.get(*index).ok_or_else(|| {
            PersistenceFailure::corrupt("byte-admission pending candidate was out of bounds")
        })?;
        if remaining[*index] {
            return Err(PersistenceFailure::corrupt(
                "byte-admission pending candidates contained a duplicate identity",
            ));
        }
        remaining[*index] = true;
        aggregate.apply(candidate)?;
    }

    let deltas = candidates
        .iter()
        .map(|candidate| AdmissionDelta::for_candidate(base, candidate))
        .collect::<Result<Vec<_>, _>>()?;
    let initial_violation_mask = admission_violation_mask(
        base,
        aggregate,
        maximum_bytes,
        force_write || remaining_count != 0,
    )?;
    if initial_violation_mask == 0 {
        return Ok(CandidateSubsetSelection {
            projection: aggregate,
            rejected: Vec::new(),
            accepted_count: remaining_count,
            stats,
        });
    }

    // The central isolation contract is exact, not heuristic: when one
    // lineage is the only reason an otherwise mutually enabling batch is
    // inadmissible, identify that lineage directly. This linear leave-one-out
    // pass prevents a small or mixed-sign poison from causing the bounded
    // multi-conflict selector to dismantle the valid remainder.
    let mut isolating_removal = None;
    for index in pending {
        stats.leave_one_out_trials = stats.leave_one_out_trials.saturating_add(1);
        let mut without_candidate = aggregate;
        without_candidate.revert(&candidates[*index])?;
        if admission_violation_mask(
            base,
            without_candidate,
            maximum_bytes,
            force_write || remaining_count > 1,
        )? == 0
        {
            let choice = (
                removal_priority(initial_violation_mask, &candidates[*index], deltas[*index]),
                *index,
            );
            if isolating_removal
                .as_ref()
                .is_none_or(|current| choice > *current)
            {
                isolating_removal = Some(choice);
            }
        }
    }
    if let Some((_, index)) = isolating_removal {
        aggregate.revert(&candidates[index])?;
        return Ok(CandidateSubsetSelection {
            projection: aggregate,
            rejected: vec![index],
            accepted_count: remaining_count
                .checked_sub(1)
                .ok_or_else(encoded_size_inconsistent)?,
            stats,
        });
    }

    // Seven resources and 128 possible supplier masks are fixed constants.
    // Each candidate enters at most seven heaps, each stale entry is popped
    // once, and each peel probes at most 7 * 128 heap heads. This keeps the
    // peel phase O(C log C) in the number of candidate lineages and prevents
    // alternating byte/count violations from triggering rescans.
    let mut relief_heaps: Vec<Vec<RemovalHeap>> = (0..ADMISSION_RESOURCE_BITS.len())
        .map(|_| {
            (0..ADMISSION_SUPPORT_MASKS)
                .map(|_| BinaryHeap::new())
                .collect()
        })
        .collect();
    for index in pending {
        let support_bucket = usize::from(deltas[*index].support_mask());
        for (resource, resource_heaps) in relief_heaps.iter_mut().enumerate() {
            if deltas[*index].positively_contributes(resource) {
                resource_heaps[support_bucket].push((
                    removal_priority(
                        ADMISSION_RESOURCE_BITS[resource],
                        &candidates[*index],
                        deltas[*index],
                    ),
                    *index,
                ));
            }
        }
    }

    let mut removed = Vec::new();
    loop {
        let violation_mask = admission_violation_mask(
            base,
            aggregate,
            maximum_bytes,
            force_write || remaining_count != 0,
        )?;
        if violation_mask == 0 {
            break;
        }

        let mut selected = None;
        for (resource, resource_heaps) in relief_heaps.iter_mut().enumerate() {
            if violation_mask & ADMISSION_RESOURCE_BITS[resource] == 0 {
                continue;
            }
            for heap in resource_heaps {
                while heap.peek().is_some_and(|(_, index)| !remaining[*index]) {
                    heap.pop();
                }
                if let Some((_, index)) = heap.peek() {
                    let choice = (
                        removal_priority(violation_mask, &candidates[*index], deltas[*index]),
                        *index,
                    );
                    if selected.as_ref().is_none_or(|current| choice > *current) {
                        selected = Some(choice);
                    }
                }
            }
        }

        let Some((_, index)) = selected else {
            // Fixed base-relative deltas cannot always identify relief at an
            // empty/nonempty JSON collection boundary, and the durable base
            // may also carry conservative normalization debt. Reset to the
            // candidate-free authority and let exact fixed-point backfill
            // reconstruct every individually admissible lineage.
            for index in pending {
                if remaining[*index] {
                    removed.push(*index);
                    remaining[*index] = false;
                }
            }
            stats.peel_removals = stats.peel_removals.saturating_add(remaining_count);
            remaining_count = 0;
            aggregate = base;
            break;
        };
        aggregate.revert(&candidates[index])?;
        remaining[index] = false;
        remaining_count = remaining_count
            .checked_sub(1)
            .ok_or_else(encoded_size_inconsistent)?;
        stats.peel_removals = stats.peel_removals.saturating_add(1);
        removed.push(index);
    }

    // A removed lineage can become individually admissible after a later
    // reducer is restored. Exact cyclic rescanning makes an adversarial
    // alternating dependency chain quadratic while the cross-process lock is
    // held. The deletion-only dominance index instead asks whether any
    // rejected point fits the three resources that can improve during
    // backfill: live overlays, tabs, and the exact normalized-byte ceiling.
    // Workspace, binding, and tombstone counts only grow, so a candidate that
    // fails one of those dimensions is permanently rejected after one exact
    // trial. Normalized and physical byte deltas are identical; their fixed
    // base offset reduces the two byte predicates to one exact ceiling.
    //
    // The outer live-overlay tree and per-node tab-prefix/min-byte trees make
    // build, deletion, and query O(C log^2 C) with O(C log C) memory. JSON
    // separator deltas are stable until a collection crosses empty/nonempty;
    // the mutation model permits at most two crossings per collection, so a
    // fixed bounded number of complete index rebuilds preserves exact byte
    // accounting without recurring candidate scans.
    let mut pending_backfill = vec![false; candidates.len()];
    for index in removed {
        pending_backfill[index] = true;
    }
    restore_inclusion_maximal_subset(
        base,
        candidates,
        &mut aggregate,
        &mut remaining,
        &mut pending_backfill,
        &mut remaining_count,
        maximum_bytes,
        &mut stats,
    )?;

    let accepted_count = remaining_count;
    let final_violation_mask = admission_violation_mask(
        base,
        aggregate,
        maximum_bytes,
        force_write || accepted_count != 0,
    )?;
    if final_violation_mask != 0 {
        return Err(aggregate
            .counts
            .quota_failure()
            .unwrap_or(byte_quota_failure(
                aggregate.normalized_bytes.upper_bound()?,
                maximum_bytes,
            )));
    }

    let rejected = pending
        .iter()
        .copied()
        .filter(|index| !remaining[*index])
        .collect::<Vec<_>>();
    Ok(CandidateSubsetSelection {
        projection: aggregate,
        rejected,
        accepted_count,
        stats,
    })
}

fn enforce_encoded_byte_admission(
    state: &PersistedState,
    batch: &PendingBatch,
    preflight: &mut BatchPreflight,
    force_write: bool,
    maximum_bytes: u64,
) -> Result<(), PersistenceFailure> {
    // Resolve semantic ownership components before deciding whether this batch
    // needs a new store revision. A batch whose only apparent changes are a
    // rejected ownership component publishes nothing and therefore remains
    // admissible even when the revision namespace is already at its ceiling.
    let overlay_components = partition_overlay_ownership_components(state, batch, preflight);
    let changing_workspace = preflight.accepted_workspaces.iter().any(|workspace| {
        state.window_states.get(workspace) != Some(&batch.window_states[workspace])
    });
    let existing_bindings = state
        .domain_bindings
        .iter()
        .map(|record| record.target_fingerprint)
        .collect::<HashSet<_>>();
    let changing_binding = preflight
        .accepted_bindings
        .iter()
        .any(|fingerprint| !existing_bindings.contains(fingerprint));
    let has_candidate_changes =
        changing_workspace || changing_binding || !preflight.overlays.apply_overlay_ids.is_empty();
    let published_revision = if force_write || has_candidate_changes {
        state
            .store_revision
            .checked_add(1)
            .ok_or(PersistenceFailure::RevisionExhausted)?
    } else {
        state.store_revision
    };
    let normalized_budget = EncodedStateBudget::from_state(state, published_revision)?;
    let physical_budget =
        EncodedStateBudget::from_state_with_physical_bindings(state, published_revision)?;
    let mut candidates = build_byte_admission_candidates(
        state,
        batch,
        preflight,
        &overlay_components,
        published_revision,
        &normalized_budget,
    )?;
    candidates.sort_by(|left, right| {
        (left.admission_rank, &left.key).cmp(&(right.admission_rank, &right.key))
    });

    let base_projection = AdmissionProjection {
        normalized_bytes: normalized_budget,
        physical_bytes: physical_budget,
        counts: AdmissionCountBudget::from_state(state)?,
    };
    let pending = (0..candidates.len()).collect::<Vec<_>>();
    let CandidateSubsetSelection {
        projection,
        rejected,
        accepted_count,
        mut stats,
    } = select_compatible_candidate_subset(
        base_projection,
        &candidates,
        &pending,
        maximum_bytes,
        force_write,
    )?;
    if accepted_count.checked_add(rejected.len()) != Some(candidates.len()) {
        return Err(PersistenceFailure::corrupt(
            "byte-admission selector lost a candidate lineage",
        ));
    }
    for index in rejected {
        stats.final_rejection_trials = stats.final_rejection_trials.saturating_add(1);
        let candidate = &candidates[index];
        let mut projected = projection;
        projected.apply(candidate)?;
        if admission_violation_mask(base_projection, projected, maximum_bytes, true)? == 0 {
            return Err(PersistenceFailure::corrupt(
                "byte-admission selector returned an admissible rejection",
            ));
        }
        let failure = match projected.counts.quota_failure() {
            Some(failure) => failure,
            None => byte_quota_failure(projected.normalized_bytes.upper_bound()?, maximum_bytes),
        };
        reject_byte_admission_candidate(candidate, batch, preflight, failure);
    }
    preflight.byte_admission = stats;

    preflight.overlays.new_tombstones = preflight
        .overlays
        .apply_overlay_ids
        .iter()
        .filter(|window_id| {
            matches!(
                &batch.overlay_mutations[window_id].desired,
                DesiredOverlayState::Deleted { .. }
            )
        })
        .count();
    let encoded_upper_bound = projection.normalized_bytes.upper_bound()?;
    preflight.encoded_upper_bound = encoded_upper_bound;
    Ok(())
}

fn preflight_batch(
    state: &PersistedState,
    batch: &PendingBatch,
    force_write: bool,
) -> Result<BatchPreflight, PersistenceFailure> {
    preflight_batch_with_byte_limit(state, batch, force_write, MAX_STATE_FILE_BYTES)
}

fn preflight_batch_with_byte_limit(
    state: &PersistedState,
    batch: &PendingBatch,
    force_write: bool,
    maximum_bytes: u64,
) -> Result<BatchPreflight, PersistenceFailure> {
    let overlays = preflight_overlay_mutations(state, batch)?;
    let accepted_workspaces = batch.window_states.keys().cloned().collect();
    let rejected_workspaces = BTreeMap::new();
    let accepted_bindings = batch.ensure_bindings.clone();
    let rejected_bindings = BTreeMap::new();

    let mut preflight = BatchPreflight {
        overlays,
        accepted_workspaces,
        rejected_workspaces,
        accepted_bindings,
        rejected_bindings,
        encoded_upper_bound: 0,
        byte_admission: ByteAdmissionStats::default(),
    };
    enforce_encoded_byte_admission(state, batch, &mut preflight, force_write, maximum_bytes)?;
    Ok(preflight)
}

fn apply_batch(
    state: &mut PersistedState,
    batch: &PendingBatch,
    preflight: &BatchPreflight,
    retirement_store_revision: Option<u64>,
) -> Result<AppliedBatch, PersistenceFailure> {
    let mut changed = false;
    let mut bindings = BTreeMap::new();
    let mut rejected_bindings = BTreeMap::new();
    let mut binding_by_fingerprint = state
        .domain_bindings
        .iter()
        .map(|record| (record.target_fingerprint, record.binding_id))
        .collect::<BTreeMap<_, _>>();
    let mut binding_ids = state
        .domain_bindings
        .iter()
        .map(|record| record.binding_id)
        .collect::<HashSet<_>>();

    for fingerprint in &preflight.accepted_bindings {
        if let Some(existing) = binding_by_fingerprint.get(fingerprint).copied() {
            bindings.insert(*fingerprint, existing);
            continue;
        }
        let Some(binding_id) = (0..8)
            .map(|_| DomainBindingId::new())
            .find(|candidate| !binding_ids.contains(candidate))
        else {
            rejected_bindings.insert(
                *fingerprint,
                PersistenceFailure::invalid("could not allocate a unique domain binding identity"),
            );
            continue;
        };
        binding_by_fingerprint.insert(*fingerprint, binding_id);
        binding_ids.insert(binding_id);
        state.domain_bindings.push(DomainBindingRecord {
            target_fingerprint: *fingerprint,
            binding_id,
        });
        bindings.insert(*fingerprint, binding_id);
        changed = true;
    }

    for workspace in &preflight.accepted_workspaces {
        let window_state = &batch.window_states[workspace];
        validate_workspace(workspace)?;
        match state.window_states.get(workspace) {
            Some(existing) if existing == window_state => {}
            Some(_) => {
                state.window_states.insert(workspace.clone(), *window_state);
                changed = true;
            }
            None => {
                state.window_states.insert(workspace.clone(), *window_state);
                changed = true;
            }
        }
    }

    let mut overlay_apply_order = preflight
        .overlays
        .apply_overlay_ids
        .iter()
        .copied()
        .collect::<Vec<_>>();
    overlay_apply_order.sort_by_key(|window_id| {
        let mutation = &batch.overlay_mutations[window_id];
        let priority = match &mutation.desired {
            DesiredOverlayState::Deleted { .. } => 0,
            DesiredOverlayState::Live(_) => 1,
        };
        (priority, *window_id)
    });

    let mut overlays_by_id = std::mem::take(&mut state.overlays)
        .into_iter()
        .map(|overlay| (overlay.window_id, overlay))
        .collect::<BTreeMap<_, _>>();
    let mut tombstones_by_id = std::mem::take(&mut state.tombstones)
        .into_iter()
        .map(|tombstone| (tombstone.window_id, tombstone))
        .collect::<BTreeMap<_, _>>();

    for window_id in overlay_apply_order {
        let mutation = &batch.overlay_mutations[&window_id];
        match &mutation.desired {
            DesiredOverlayState::Live(overlay) => {
                overlays_by_id.insert(window_id, overlay.clone());
            }
            DesiredOverlayState::Deleted {
                last_local_revision,
                ..
            } => {
                overlays_by_id.remove(&window_id);
                let retired_at_store_revision = retirement_store_revision.ok_or_else(|| {
                    PersistenceFailure::invalid(
                        "overlay retirement has no reserved store generation",
                    )
                })?;
                tombstones_by_id.insert(
                    window_id,
                    OverlayTombstone::new(
                        window_id,
                        *last_local_revision,
                        retired_at_store_revision,
                    )?,
                );
            }
        }
        changed = true;
    }

    state.overlays = overlays_by_id.into_values().collect();
    state.tombstones = tombstones_by_id.into_values().collect();

    canonicalize_state(state);
    Ok(AppliedBatch {
        changed,
        bindings,
        rejected_bindings,
    })
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
    #[cfg(test)]
    AfterDirectorySync,
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
    sync_parent_directory(path)?;
    #[cfg(test)]
    if interruption == WriteInterruption::AfterDirectorySync {
        return Err(PersistenceFailure::injected_io(
            "injected acknowledgement loss after state-directory sync",
        ));
    }
    Ok(())
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
    sync_parent_directory(path)?;
    #[cfg(test)]
    if interruption == WriteInterruption::AfterDirectorySync {
        return Err(PersistenceFailure::injected_io(
            "injected acknowledgement loss after initial-state directory sync",
        ));
    }
    Ok(())
}

fn sync_authoritative_slot(path: &Path) -> Result<(), PersistenceFailure> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| PersistenceFailure::io("sync authoritative state slot", error))?;
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

struct PersistenceLockTelemetry {
    requested_at: Instant,
    acquired_at: Option<Instant>,
    byte_admission: Option<ByteAdmissionStats>,
}

impl PersistenceLockTelemetry {
    fn begin() -> Self {
        Self {
            requested_at: Instant::now(),
            acquired_at: None,
            byte_admission: None,
        }
    }

    fn acquired(&mut self) {
        self.acquired_at = Some(Instant::now());
    }

    fn byte_admission(&mut self, stats: ByteAdmissionStats) {
        self.byte_admission = Some(stats);
    }
}

impl Drop for PersistenceLockTelemetry {
    fn drop(&mut self) {
        let released_at = Instant::now();
        if let Some(acquired_at) = self.acquired_at {
            metrics::histogram!("window_state.persistence_lock_wait")
                .record(acquired_at.saturating_duration_since(self.requested_at));
            metrics::histogram!("window_state.persistence_lock_hold")
                .record(released_at.saturating_duration_since(acquired_at));
        }
        if let Some(stats) = self.byte_admission {
            let as_u64 = |value| u64::try_from(value).unwrap_or(u64::MAX);
            metrics::histogram!("window_state.byte_admission.candidates")
                .record(as_u64(stats.candidate_count));
            metrics::histogram!("window_state.byte_admission.leave_one_out_trials")
                .record(as_u64(stats.leave_one_out_trials));
            metrics::histogram!("window_state.byte_admission.peel_removals")
                .record(as_u64(stats.peel_removals));
            metrics::histogram!("window_state.byte_admission.backfill_queries")
                .record(as_u64(stats.backfill_queries));
            metrics::histogram!("window_state.byte_admission.backfill_candidate_trials")
                .record(as_u64(stats.backfill_candidate_trials));
            metrics::histogram!("window_state.byte_admission.final_rejection_trials")
                .record(as_u64(stats.final_rejection_trials));
            metrics::histogram!("window_state.byte_admission.backfill_index_rebuilds")
                .record(as_u64(stats.backfill_index_rebuilds));
            metrics::histogram!("window_state.byte_admission.backfill_index_entries_peak")
                .record(as_u64(stats.backfill_index_entries_peak));
        }
    }
}

fn commit_batch(
    primary_path: &Path,
    batch: &PendingBatch,
    interruption: WriteInterruption,
) -> Result<BatchCommit, PersistenceFailure> {
    commit_batch_with_byte_limit(primary_path, batch, interruption, MAX_STATE_FILE_BYTES)
}

fn commit_batch_with_byte_limit(
    primary_path: &Path,
    batch: &PendingBatch,
    interruption: WriteInterruption,
    maximum_bytes: u64,
) -> Result<BatchCommit, PersistenceFailure> {
    // Declared before `lock` so Rust drops the file lock first and records the
    // finite, label-free metrics outside the cross-process critical section on
    // every return path.
    let mut lock_telemetry = PersistenceLockTelemetry::begin();
    let lock = open_lock_file(primary_path)?;
    fs2::FileExt::lock_exclusive(&lock)
        .map_err(|error| PersistenceFailure::io("lock state for writing", error))?;
    lock_telemetry.acquired();

    let loaded = load_authoritative_unlocked(primary_path)?;
    let source = loaded.source;
    let requires_schema_upgrade = loaded.requires_schema_upgrade;
    let degraded_recovery = loaded.degraded_recovery;
    let mut state = loaded.state;
    let preflight = preflight_batch_with_byte_limit(
        &state,
        batch,
        requires_schema_upgrade || degraded_recovery,
        maximum_bytes,
    )?;
    lock_telemetry.byte_admission(preflight.byte_admission);
    let reserved_retirement_revision = if preflight.overlays.new_tombstones == 0 {
        None
    } else {
        Some(
            state
                .store_revision
                .checked_add(1)
                .ok_or(PersistenceFailure::RevisionExhausted)?,
        )
    };
    let AppliedBatch {
        changed: batch_changed,
        bindings,
        rejected_bindings: allocation_rejections,
    } = apply_batch(&mut state, batch, &preflight, reserved_retirement_revision)?;
    let committed_binding_count = preflight
        .accepted_bindings
        .len()
        .saturating_sub(allocation_rejections.len());
    let mut rejected_bindings = preflight.rejected_bindings;
    rejected_bindings.extend(allocation_rejections);
    let changed = batch_changed || requires_schema_upgrade || degraded_recovery;
    let committed_updates = preflight
        .accepted_workspaces
        .len()
        .saturating_add(committed_binding_count)
        .saturating_add(preflight.overlays.accepted_overlay_ids.len());
    let coalesced_updates = batch.superseded_updates_for(
        &preflight.accepted_workspaces,
        &preflight.overlays.accepted_overlay_ids,
    );
    let rejected_updates = preflight
        .rejected_workspaces
        .len()
        .saturating_add(rejected_bindings.len())
        .saturating_add(preflight.overlays.rejected_overlay_mutations.len());
    if !changed {
        if let Some(authority) = loaded.authority.as_deref() {
            sync_authoritative_slot(authority)?;
            #[cfg(test)]
            if interruption == WriteInterruption::AfterDirectorySync {
                return Err(PersistenceFailure::injected_io(
                    "injected acknowledgement loss after authoritative replay sync",
                ));
            }
        }
        return Ok(BatchCommit {
            receipt: CommitReceipt {
                store_revision: state.store_revision,
                wrote_new_generation: false,
                committed_updates,
                coalesced_updates,
                rejected_updates,
            },
            bindings,
            rejected_bindings,
            rejected_workspaces: preflight.rejected_workspaces,
            accepted_overlay_ids: preflight.overlays.accepted_overlay_ids,
            rejected_overlay_mutations: preflight.overlays.rejected_overlay_mutations,
        });
    }

    state.store_revision = match reserved_retirement_revision {
        Some(revision) => revision,
        None => state
            .store_revision
            .checked_add(1)
            .ok_or(PersistenceFailure::RevisionExhausted)?,
    };
    canonicalize_state(&mut state);
    validate_state(&state)?;
    let encoded = encode_disk_slot(&state)?;
    let actual_encoded_bytes = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
    if actual_encoded_bytes > maximum_bytes {
        return Err(PersistenceFailure::Oversized {
            actual: actual_encoded_bytes,
            maximum: maximum_bytes,
        });
    }
    if actual_encoded_bytes > preflight.encoded_upper_bound {
        return Err(PersistenceFailure::corrupt(
            "encoded state exceeded its admitted upper bound",
        ));
    }
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
            committed_updates,
            coalesced_updates,
            rejected_updates,
        },
        bindings,
        rejected_bindings,
        rejected_workspaces: preflight.rejected_workspaces,
        accepted_overlay_ids: preflight.overlays.accepted_overlay_ids,
        rejected_overlay_mutations: preflight.overlays.rejected_overlay_mutations,
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
    let result =
        admit_window_state_and_record(&STARTUP_SNAPSHOT, workspace, state, |workspace, state| {
            global_writer()
                .and_then(|writer| writer.queue_window_state(workspace, state).map(|_| ()))
        });
    if let Err(failure) = result {
        log::warn!(
            "window-state: could not enqueue geometry state ({:?})",
            failure.code()
        );
    }
}

/// Queue a validated mixed-domain layout overlay on the default worker.
///
/// This is a storage primitive, not GUI lifecycle wiring: it does not derive
/// stable identities from live mux tabs, observe tab-order changes, or apply a
/// reconciled overlay when a native window is reopened.
pub fn queue_layout_overlay(
    base_revision: Option<u64>,
    overlay: MixedDomainLayoutOverlay,
) -> Result<EnqueueOutcome, PersistenceFailure> {
    global_writer()?.queue_overlay(base_revision, overlay)
}

/// Queue retirement of one exact live overlay revision on the default worker.
///
/// This is intentionally only a storage primitive. The GUI close path must not
/// call it until that path owns a durable [`LayoutWindowId`] and exact live
/// revision rather than a transient mux window number.
pub fn queue_layout_overlay_delete(
    window_id: LayoutWindowId,
    base_revision: Option<u64>,
) -> Result<EnqueueOutcome, PersistenceFailure> {
    global_writer()?.queue_overlay_delete(window_id, base_revision)
}

/// Request a nonblocking lifecycle barrier on the default authority.
pub fn flush() -> Result<flume::Receiver<CommitResult>, PersistenceFailure> {
    global_writer()?.flush()
}

/// Resolve or create a stable domain binding on the default storage worker.
///
/// Callers may construct or queue remote slots with the returned ID only after
/// the receiver resolves `Ok(binding_id)`; the request itself is not durable
/// binding authority.
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
    use std::process::Command;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CONTROLLED_WORKER_WATCHDOG: Duration = Duration::from_secs(5);
    // The terminal numeric identity is reserved; its predecessor retains the
    // same 20-digit serialized width for maximum-budget coverage.
    const MAX_ADMISSIBLE_REMOTE_ID: u64 = u64::MAX - 1;
    const CROSS_PROCESS_GROWTH_HELPER: &str =
        "window_state_persist::tests::near_ceiling_external_growth_process_helper";
    const CROSS_PROCESS_STATE_PATH_ENV: &str = "FT_TEST_WINDOW_STATE_PATH";
    const CROSS_PROCESS_WORKSPACE_ENV: &str = "FT_TEST_WINDOW_STATE_WORKSPACE";
    const CROSS_PROCESS_MARKER_ENV: &str = "FT_TEST_WINDOW_STATE_MARKER";

    struct ControlledPersistenceWorker {
        writer: Option<PersistenceWriter>,
        events: Option<flume::Receiver<TestWorkerCommitEvent>>,
        directives: Option<flume::Sender<TestWorkerCommitDirective>>,
        stopped: flume::Receiver<TestWorkerStopped>,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl ControlledPersistenceWorker {
        fn open(primary_path: PathBuf) -> Self {
            let (event_sender, events) = flume::bounded(1);
            let (directives, directive_receiver) = flume::bounded(1);
            let (stopped_sender, stopped) = flume::bounded(1);
            let control = TestWorkerCommitControl {
                events: event_sender,
                directives: directive_receiver,
                stopped: stopped_sender,
                waiting_epoch: 0,
                commit_epoch: 0,
            };
            let shared = Arc::new(CoordinatorShared {
                primary_path,
                pending: Mutex::new(CoordinatorPending {
                    worker_commit_control: Some(control),
                    ..CoordinatorPending::default()
                }),
            });
            let (wake, receiver) = flume::bounded(1);
            let writer = PersistenceWriter {
                shared: Arc::clone(&shared),
                wake,
            };
            let join = std::thread::Builder::new()
                .name("ft-layout-persistence-controlled".to_string())
                .spawn(move || persistence_worker(shared, receiver))
                .expect("spawn controlled persistence worker");
            Self {
                writer: Some(writer),
                events: Some(events),
                directives: Some(directives),
                stopped,
                join: Some(join),
            }
        }

        fn writer(&self) -> &PersistenceWriter {
            self.writer.as_ref().expect("controlled writer is live")
        }

        fn next_event(&self) -> TestWorkerCommitEvent {
            self.events
                .as_ref()
                .expect("controlled event receiver is connected")
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("controlled worker event")
        }

        fn expect_waiting(&self, waiting_epoch: u64, retry_pending: bool) {
            match self.next_event() {
                TestWorkerCommitEvent::BeforeWake {
                    waiting_epoch: actual_epoch,
                    retry_pending: actual_retry,
                } => {
                    assert_eq!(actual_epoch, waiting_epoch);
                    assert_eq!(actual_retry, retry_pending);
                }
                TestWorkerCommitEvent::CommitEntered { .. } => {
                    panic!("expected worker to wait for a wake")
                }
                TestWorkerCommitEvent::CommitFinished { .. } => {
                    panic!("expected worker to wait for a wake")
                }
            }
        }

        fn expect_commit(&self, commit_epoch: u64, phase: TestWorkerCommitPhase) -> PendingBatch {
            match self.next_event() {
                TestWorkerCommitEvent::CommitEntered {
                    commit_epoch: actual_epoch,
                    phase: actual_phase,
                    batch,
                } => {
                    assert_eq!(actual_epoch, commit_epoch);
                    assert_eq!(actual_phase, phase);
                    batch
                }
                TestWorkerCommitEvent::BeforeWake { .. } => {
                    panic!("expected worker to enter a commit")
                }
                TestWorkerCommitEvent::CommitFinished { .. } => {
                    panic!("expected worker to enter a commit")
                }
            }
        }

        fn expect_commit_finished(
            &self,
            commit_epoch: u64,
            phase: TestWorkerCommitPhase,
        ) -> TestWorkerCommitResult {
            match self.next_event() {
                TestWorkerCommitEvent::CommitFinished {
                    commit_epoch: actual_epoch,
                    phase: actual_phase,
                    result: actual_result,
                } => {
                    assert_eq!(actual_epoch, commit_epoch);
                    assert_eq!(actual_phase, phase);
                    actual_result
                }
                TestWorkerCommitEvent::BeforeWake { .. }
                | TestWorkerCommitEvent::CommitEntered { .. } => {
                    panic!("expected worker to finish a commit")
                }
            }
        }

        fn send_directive(&self, epoch: u64, action: TestWorkerDirectiveAction) {
            self.directives
                .as_ref()
                .expect("controlled directive sender is connected")
                .try_send(TestWorkerCommitDirective { epoch, action })
                .expect("release controlled worker epoch");
        }

        fn continue_wake(&self, waiting_epoch: u64) {
            self.send_directive(waiting_epoch, TestWorkerDirectiveAction::ContinueWake);
        }

        fn panic_before_wake(&self, waiting_epoch: u64) {
            self.send_directive(waiting_epoch, TestWorkerDirectiveAction::PanicBeforeWake);
        }

        fn release_commit(&self, commit_epoch: u64, action: TestWorkerCommitAction) {
            self.send_directive(commit_epoch, TestWorkerDirectiveAction::Commit(action));
        }

        fn continue_after_commit(&self, commit_epoch: u64) {
            self.send_directive(commit_epoch, TestWorkerDirectiveAction::ContinueAfterCommit);
        }

        fn wake_without_waiter(&self) {
            self.writer()
                .wake_worker()
                .expect("wake controlled persistence worker");
        }

        fn admit_batch_with_flush(&self, batch: PendingBatch) -> flume::Receiver<CommitResult> {
            let (sender, receiver) = flume::bounded(1);
            {
                let mut pending = lock_pending(&self.writer().shared.pending);
                assert!(pending.batch.window_states.is_empty());
                assert!(pending.batch.overlay_mutations.is_empty());
                assert!(pending.batch.ensure_bindings.is_empty());
                assert!(pending.flush_waiters.is_empty());
                assert!(pending.binding_waiters.is_empty());
                assert_eq!(pending.waiter_count, 0);
                pending.batch = batch;
                pending.flush_waiters.push(FlushWaiter::new(sender));
                pending.waiter_count = 1;
            }
            self.wake_without_waiter();
            receiver
        }

        fn admit_batch_with_flush_and_binding(
            &self,
            mut batch: PendingBatch,
            fingerprint: PrivacySafeTargetFingerprint,
        ) -> (
            flume::Receiver<CommitResult>,
            flume::Receiver<BindingResult>,
        ) {
            let (flush_sender, flush_receiver) = flume::bounded(1);
            let (binding_sender, binding_receiver) = flume::bounded(1);
            batch.ensure_bindings.insert(fingerprint);
            {
                let mut pending = lock_pending(&self.writer().shared.pending);
                assert!(pending.batch.window_states.is_empty());
                assert!(pending.batch.overlay_mutations.is_empty());
                assert!(pending.batch.ensure_bindings.is_empty());
                assert!(pending.flush_waiters.is_empty());
                assert!(pending.binding_waiters.is_empty());
                assert_eq!(pending.waiter_count, 0);
                pending.batch = batch;
                pending
                    .binding_waiters
                    .insert(fingerprint, vec![binding_sender]);
                pending.flush_waiters.push(FlushWaiter::new(flush_sender));
                pending.waiter_count = 2;
            }
            self.wake_without_waiter();
            (flush_receiver, binding_receiver)
        }

        fn disconnect_directives(&mut self) {
            self.directives.take();
        }

        fn disconnect_events(&mut self) {
            self.events.take();
        }

        fn wait_stopped_and_join(mut self) -> TestWorkerStopped {
            self.writer.take();
            self.directives.take();
            self.events.take();
            let stopped = self
                .stopped
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("controlled worker stopped event");
            self.join
                .take()
                .expect("controlled worker join handle")
                .join()
                .expect("controlled persistence worker exits");
            stopped
        }

        fn stop_and_join(mut self) -> TestWorkerStopped {
            self.writer.take();
            self.directives.take();
            self.wait_stopped_and_join()
        }
    }

    fn local_slot(value: u8) -> StableTabSlot {
        StableTabSlot::local(
            StableLocalSessionId::from_bytes([0x11; 16]),
            StableLocalTabId::from_bytes([value; 16]),
        )
    }

    fn local_slots(session: u8, count: usize) -> Vec<StableTabSlot> {
        (0..count)
            .map(|index| {
                let mut tab_id = [0u8; 16];
                tab_id[..8].copy_from_slice(
                    &u64::try_from(index)
                        .expect("test tab index fits u64")
                        .to_le_bytes(),
                );
                StableTabSlot::local(
                    StableLocalSessionId::from_bytes([session; 16]),
                    StableLocalTabId::from_bytes(tab_id),
                )
            })
            .collect()
    }

    fn window_id(number: u64) -> LayoutWindowId {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&number.to_le_bytes());
        LayoutWindowId::from_bytes(bytes)
    }

    fn local_overlay(
        window_id: LayoutWindowId,
        local_revision: u64,
        tab_value: u8,
    ) -> MixedDomainLayoutOverlay {
        let slot = local_slot(tab_value);
        MixedDomainLayoutOverlay::new(window_id, "default", local_revision, vec![slot], Some(slot))
            .expect("valid local overlay")
    }

    fn remote_slot(binding: DomainBindingId, session: u8, window: u64, tab: u64) -> StableTabSlot {
        StableTabSlot::remote(
            binding,
            StableMuxSessionId::from_bytes([session; 16]),
            window,
            tab,
        )
    }

    fn indexed_fingerprint(index: u64) -> PrivacySafeTargetFingerprint {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&index.to_le_bytes());
        PrivacySafeTargetFingerprint::from_bytes(bytes)
    }

    fn indexed_binding_id(index: u64) -> DomainBindingId {
        let mut bytes = [0u8; 16];
        let nonzero_ordinal = index
            .checked_add(1)
            .expect("test binding index has a nonzero successor");
        bytes[..8].copy_from_slice(&nonzero_ordinal.to_le_bytes());
        DomainBindingId::from_bytes(bytes)
    }

    fn capped_domain_bindings() -> Vec<DomainBindingRecord> {
        (0..MAX_DOMAIN_BINDINGS)
            .map(|index| {
                let index = u64::try_from(index).expect("binding index fits u64");
                DomainBindingRecord {
                    target_fingerprint: indexed_fingerprint(index),
                    binding_id: indexed_binding_id(index),
                }
            })
            .collect()
    }

    fn boundary_workspace_key(index: usize, encoded_bytes: usize) -> String {
        let prefix = format!("{index:04x}");
        assert!(prefix.len() <= encoded_bytes);
        let mut workspace = String::with_capacity(encoded_bytes);
        workspace.push_str(&prefix);
        workspace.push_str(&"w".repeat(encoded_bytes - prefix.len()));
        workspace
    }

    fn workspace_state_at_encoded_upper_bound(target_bytes: u64) -> (PersistedState, String) {
        let window_state = PersistedWindowState {
            maximized: true,
            fullscreen: true,
        };
        let mut state = PersistedState {
            store_revision: 1,
            ..PersistedState::default()
        };
        let empty_slot_bytes = EncodedStateBudget::from_state(&state, state.store_revision)
            .expect("count empty boundary state")
            .upper_bound()
            .expect("empty boundary state upper bound");
        let window_state_bytes =
            encoded_json_len(&window_state).expect("count boundary window state");
        let workspace_count = u64::try_from(MAX_WORKSPACES).expect("workspace cap fits u64");
        // Each map entry contributes the quoted ASCII key, one colon, the
        // window-state value, and (except for the first entry) one comma. With
        // `MAX_WORKSPACES` entries, the non-key contribution is therefore
        // count * (value + two quotes + colon + comma) - one comma.
        let fixed_collection_bytes = workspace_count
            .checked_mul(
                window_state_bytes
                    .checked_add(4)
                    .expect("boundary entry fixed bytes fit u64"),
            )
            .and_then(|bytes| bytes.checked_sub(1))
            .expect("boundary collection fixed bytes fit u64");
        let target_workspace_bytes = target_bytes
            .checked_sub(empty_slot_bytes)
            .and_then(|bytes| bytes.checked_sub(fixed_collection_bytes))
            .expect("target leaves room for workspace keys");
        let minimum_workspace_bytes = workspace_count
            .checked_mul(4)
            .expect("minimum workspace bytes fit u64");
        let maximum_workspace_bytes = workspace_count
            .checked_mul(u64::try_from(MAX_WORKSPACE_BYTES).expect("workspace byte cap fits u64"))
            .expect("maximum workspace bytes fit u64");
        assert!(
            (minimum_workspace_bytes..=maximum_workspace_bytes).contains(&target_workspace_bytes)
        );

        let mut remaining_workspace_bytes = target_workspace_bytes;
        for index in 0..MAX_WORKSPACES {
            let entries_after = MAX_WORKSPACES - index - 1;
            let minimum_after = u64::try_from(entries_after)
                .expect("remaining workspace count fits u64")
                .checked_mul(4)
                .expect("remaining minimum workspace bytes fit u64");
            let workspace_bytes = remaining_workspace_bytes
                .checked_sub(minimum_after)
                .expect("boundary distribution retains minimum suffix")
                .min(u64::try_from(MAX_WORKSPACE_BYTES).expect("workspace byte cap fits u64"));
            let workspace_bytes =
                usize::try_from(workspace_bytes).expect("workspace length fits usize");
            assert!((4..=MAX_WORKSPACE_BYTES).contains(&workspace_bytes));
            let workspace = boundary_workspace_key(index, workspace_bytes);
            assert_eq!(workspace.len(), workspace_bytes);
            assert!(
                state
                    .window_states
                    .insert(workspace, window_state)
                    .is_none()
            );
            remaining_workspace_bytes = remaining_workspace_bytes
                .checked_sub(u64::try_from(workspace_bytes).expect("workspace length fits u64"))
                .expect("distributed workspace bytes do not underflow");
        }
        assert_eq!(remaining_workspace_bytes, 0);
        let changed_workspace = state
            .window_states
            .keys()
            .next()
            .expect("boundary state has workspaces")
            .clone();
        (state, changed_workspace)
    }

    fn near_ceiling_state_with_overlay(
        target_bytes: u64,
    ) -> (PersistedState, String, String, MixedDomainLayoutOverlay) {
        let overlay = local_overlay(window_id(90_001), 1, 0x91);
        let empty = PersistedState {
            store_revision: 1,
            ..PersistedState::default()
        };
        let empty_upper_bound = EncodedStateBudget::from_state(&empty, empty.store_revision)
            .and_then(|budget| budget.upper_bound())
            .expect("count empty near-ceiling state");
        let mut overlay_only = empty.clone();
        overlay_only.overlays.push(overlay.clone());
        let overlay_upper_bound =
            EncodedStateBudget::from_state(&overlay_only, overlay_only.store_revision)
                .and_then(|budget| budget.upper_bound())
                .expect("count near-ceiling overlay contribution");
        let overlay_bytes = overlay_upper_bound
            .checked_sub(empty_upper_bound)
            .expect("overlay contribution cannot shrink an empty state");
        let workspace_target = target_bytes
            .checked_sub(overlay_bytes)
            .expect("near-ceiling target leaves room for one overlay");
        let (mut state, _) = workspace_state_at_encoded_upper_bound(workspace_target);
        state.overlays.push(overlay.clone());
        canonicalize_state(&mut state);
        validate_published_state(&state).expect("near-ceiling mixed state is structurally valid");
        let upper_bound = EncodedStateBudget::from_state(&state, state.store_revision)
            .and_then(|budget| budget.upper_bound())
            .expect("count near-ceiling mixed state");
        assert_eq!(upper_bound, target_bytes);

        let mut workspaces = state.window_states.keys().take(2).cloned();
        let external_growth_workspace = workspaces.next().expect("first boundary workspace");
        let stale_growth_workspace = workspaces.next().expect("second boundary workspace");
        assert_ne!(external_growth_workspace, stale_growth_workspace);
        (
            state,
            external_growth_workspace,
            stale_growth_workspace,
            overlay,
        )
    }

    fn maximum_width_index_bytes<const N: usize>(namespace: u8, mut index: usize) -> [u8; N] {
        assert!(N >= 3);
        assert!((100..=u8::MAX).contains(&namespace));
        let mut bytes = [u8::MAX; N];
        bytes[0] = namespace;
        for byte in &mut bytes[1..] {
            *byte = 100 + u8::try_from(index % 156).expect("base-156 digit fits u8");
            index /= 156;
        }
        assert_eq!(index, 0, "test identity exceeded maximum-width namespace");
        bytes
    }

    fn maximally_escaped_workspace_key(index: usize) -> String {
        let mut workspace = format!("{index:04x}");
        let escape_pairs = (MAX_WORKSPACE_BYTES - workspace.len()) / 2;
        workspace.reserve(MAX_WORKSPACE_BYTES - workspace.len());
        for _ in 0..escape_pairs {
            workspace.push('"');
            workspace.push('\\');
        }
        assert_eq!(workspace.len(), MAX_WORKSPACE_BYTES);
        workspace
    }

    fn all_maxima_escaped_state() -> PersistedState {
        let mut state = PersistedState {
            store_revision: u64::MAX,
            ..PersistedState::default()
        };

        for index in 0..MAX_WORKSPACES {
            let workspace = maximally_escaped_workspace_key(index);
            assert!(
                state
                    .window_states
                    .insert(
                        workspace,
                        PersistedWindowState {
                            maximized: index & 1 == 0,
                            fullscreen: index & 2 == 0,
                        },
                    )
                    .is_none()
            );
        }

        state.domain_bindings = (0..MAX_DOMAIN_BINDINGS)
            .map(|index| DomainBindingRecord {
                target_fingerprint: PrivacySafeTargetFingerprint::from_bytes(
                    maximum_width_index_bytes(101, index),
                ),
                binding_id: DomainBindingId::from_bytes(maximum_width_index_bytes(102, index)),
            })
            .collect();

        let full_overlay_count = MAX_TOTAL_OVERLAY_TABS / MAX_TABS_PER_OVERLAY;
        assert_eq!(
            full_overlay_count
                .checked_mul(MAX_TABS_PER_OVERLAY)
                .expect("maximum overlay tab product fits usize"),
            MAX_TOTAL_OVERLAY_TABS
        );
        let overlay_workspace = maximally_escaped_workspace_key(0);
        for overlay_index in 0..MAX_LAYOUT_OVERLAYS {
            let slots = if overlay_index < full_overlay_count {
                let binding_id = state.domain_bindings[overlay_index].binding_id;
                (0..MAX_TABS_PER_OVERLAY)
                    .map(|tab_index| {
                        let global_tab_index = overlay_index
                            .checked_mul(MAX_TABS_PER_OVERLAY)
                            .and_then(|base| base.checked_add(tab_index))
                            .expect("global test tab index fits usize");
                        StableTabSlot::remote(
                            binding_id,
                            StableMuxSessionId::from_bytes(maximum_width_index_bytes(
                                103,
                                global_tab_index,
                            )),
                            MAX_ADMISSIBLE_REMOTE_ID,
                            MAX_ADMISSIBLE_REMOTE_ID,
                        )
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let active = slots.first().copied();
            state.overlays.push(
                MixedDomainLayoutOverlay::new(
                    LayoutWindowId::from_bytes(maximum_width_index_bytes(104, overlay_index)),
                    &overlay_workspace,
                    u64::MAX,
                    slots,
                    active,
                )
                .expect("all-maxima overlay is structurally valid"),
            );
        }

        state.tombstones = (0..MAX_OVERLAY_TOMBSTONES)
            .map(|index| {
                OverlayTombstone::new(
                    LayoutWindowId::from_bytes(maximum_width_index_bytes(105, index)),
                    u64::MAX,
                    u64::MAX,
                )
                .expect("all-maxima tombstone is structurally valid")
            })
            .collect();
        canonicalize_state(&mut state);
        state
    }

    fn commit_for_test(
        path: &Path,
        batch: &PendingBatch,
        interruption: WriteInterruption,
    ) -> Result<BatchCommit, PersistenceFailure> {
        commit_batch(path, batch, interruption)
    }

    fn encode_v2_slot_for_test(state: PersistedStateV2) -> Vec<u8> {
        let payload = serde_json::to_vec(&state).expect("serialize schema-v2 payload");
        let sha256: [u8; 32] = Sha256::digest(&payload).into();
        serde_json::to_vec(&DiskSlotV2 {
            payload: state,
            sha256,
        })
        .expect("serialize schema-v2 slot")
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
            tombstones: Vec::new(),
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
            resolve_saved_window_state(
                "workspace-a",
                Some(captured),
                Some("workspace-b"),
                |name| {
                    assert_eq!(name, "workspace-b");
                    Some(current)
                }
            ),
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
        let states = BTreeMap::from([("first".to_string(), first), ("second".to_string(), second)]);

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

        let failure = admit_window_state_and_record(&cache, "default", rejected, |_, _| {
            Err(PersistenceFailure::WorkerStopped)
        })
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
        let startup =
            cached_startup_snapshot(&cache, || Ok(snapshot_with_window_states(BTreeMap::new())));
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
        commit_for_test(&path, &first_batch, WriteInterruption::None).expect("commit first state");

        let cache = OnceLock::new();
        let first = load_startup_workspace_from(&cache, "default", || load_snapshot_at(&path))
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
        batch
            .queue_window_state(
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
    fn checksum_verified_v2_migrates_to_v3_on_an_empty_commit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let overlay = local_overlay(window_id(20), 1, 20);
        let v2 = PersistedStateV2 {
            schema_version: PREVIOUS_STORE_SCHEMA_VERSION,
            store_revision: 7,
            window_states: BTreeMap::from([(
                "default".to_string(),
                PersistedWindowState {
                    maximized: true,
                    fullscreen: false,
                },
            )]),
            domain_bindings: Vec::new(),
            overlays: vec![overlay.clone()],
        };
        std::fs::write(&path, encode_v2_slot_for_test(v2)).expect("write schema-v2 slot");

        let before = load_snapshot_at(&path).expect("load schema-v2 authority");
        assert_eq!(before.source, StoreSource::Primary);
        assert_eq!(before.store_revision, 7);
        assert_eq!(before.overlays, vec![overlay.clone()]);
        assert!(before.tombstones.is_empty());

        let migration = commit_for_test(&path, &PendingBatch::default(), WriteInterruption::None)
            .expect("publish schema-v3 generation");
        assert!(migration.receipt.wrote_new_generation);
        assert_eq!(migration.receipt.store_revision, 8);
        assert_eq!(migration.receipt.committed_updates, 0);

        let after = load_snapshot_at(&path).expect("load migrated authority");
        assert_eq!(after.store_revision, 8);
        assert_eq!(after.window_states, before.window_states);
        assert_eq!(after.overlays, vec![overlay]);
        assert!(after.tombstones.is_empty());
        assert!(matches!(
            read_slot(&shadow_file_name(&path), false).expect("read migrated slot"),
            ReadSlot::Valid(ValidatedSlot {
                schema: SlotSchema::Current,
                ..
            })
        ));
    }

    #[test]
    fn invalid_v2_checksum_never_migrates() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let payload = PersistedStateV2 {
            schema_version: PREVIOUS_STORE_SCHEMA_VERSION,
            store_revision: 1,
            window_states: BTreeMap::new(),
            domain_bindings: Vec::new(),
            overlays: Vec::new(),
        };
        let disk = DiskSlotV2 {
            payload,
            sha256: [0; 32],
        };
        std::fs::write(
            &path,
            serde_json::to_vec(&disk).expect("serialize corrupt schema-v2 slot"),
        )
        .expect("write corrupt schema-v2 slot");

        assert_eq!(
            load_snapshot_at(&path)
                .expect_err("bad schema-v2 checksum must fail")
                .code(),
            PersistenceFailureCode::Corrupt
        );
        assert_eq!(
            commit_for_test(&path, &PendingBatch::default(), WriteInterruption::None,)
                .expect_err("bad schema-v2 checksum must not migrate")
                .code(),
            PersistenceFailureCode::Corrupt
        );
        assert!(!shadow_file_name(&path).exists());
    }

    #[test]
    fn equal_generation_prefers_v3_only_when_logical_state_matches() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let v2 = PersistedStateV2 {
            schema_version: PREVIOUS_STORE_SCHEMA_VERSION,
            store_revision: 9,
            window_states: BTreeMap::from([(
                "default".to_string(),
                PersistedWindowState {
                    maximized: true,
                    fullscreen: false,
                },
            )]),
            domain_bindings: Vec::new(),
            overlays: Vec::new(),
        };
        std::fs::write(&path, encode_v2_slot_for_test(v2.clone())).expect("write schema-v2 slot");
        std::fs::write(
            shadow_file_name(&path),
            encode_disk_slot(&v2.clone().into_current()).expect("encode matching schema-v3 slot"),
        )
        .expect("write matching schema-v3 slot");
        assert_eq!(
            load_snapshot_at(&path)
                .expect("load matching generations")
                .source,
            StoreSource::Shadow
        );

        let ambiguous_path = temp.path().join("ambiguous.json");
        std::fs::write(&ambiguous_path, encode_v2_slot_for_test(v2.clone()))
            .expect("write ambiguous schema-v2 slot");
        let mut different = v2.into_current();
        different
            .window_states
            .get_mut("default")
            .expect("window state")
            .fullscreen = true;
        std::fs::write(
            shadow_file_name(&ambiguous_path),
            encode_disk_slot(&different).expect("encode different schema-v3 slot"),
        )
        .expect("write different schema-v3 slot");
        assert_eq!(
            load_snapshot_at(&ambiguous_path)
                .expect_err("different equal generations are ambiguous")
                .code(),
            PersistenceFailureCode::AmbiguousGeneration
        );
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
        batch
            .queue_overlay_live(None, overlay.clone())
            .expect("queue overlay");
        commit_for_test(&path, &batch, WriteInterruption::None).expect("commit overlay");

        let snapshot = load_snapshot_at(&path).expect("load current");
        assert_eq!(snapshot.overlays, vec![overlay]);
        assert_eq!(snapshot.domain_bindings.len(), 1);
        assert_eq!(snapshot.store_revision, 2);
    }

    #[test]
    fn remote_overlay_and_live_reconcile_reject_reserved_authority_identities() {
        let binding = DomainBindingId::from_bytes([0x21; 16]);
        let session = StableMuxSessionId::from_bytes([0x31; 16]);
        let valid_local = local_slot(0x40);
        let valid_overlay = MixedDomainLayoutOverlay::new(
            LayoutWindowId::from_bytes([0x42; 16]),
            "default",
            1,
            vec![valid_local],
            Some(valid_local),
        )
        .expect("valid local-only overlay");
        let cases = [
            (
                StableTabSlot::remote(DomainBindingId::from_bytes([0; 16]), session, 1, 2),
                "zero domain binding",
            ),
            (
                StableTabSlot::remote(binding, StableMuxSessionId::from_bytes([0; 16]), 1, 2),
                "zero mux-session",
            ),
            (
                StableTabSlot::remote(binding, session, u64::MAX, 2),
                "terminal remote-window",
            ),
            (
                StableTabSlot::remote(binding, session, 1, u64::MAX),
                "terminal remote-tab",
            ),
        ];

        for (index, (slot, expected_reason)) in cases.into_iter().enumerate() {
            let error = MixedDomainLayoutOverlay::new(
                LayoutWindowId::from_bytes([0x41; 16]),
                "default",
                u64::try_from(index + 1).expect("small test revision fits u64"),
                vec![slot],
                Some(slot),
            )
            .expect_err("reserved remote identity must fail before overlay admission");
            assert_eq!(error.code(), PersistenceFailureCode::Invalid);
            let PersistenceFailure::Invalid { reason } = &error else {
                panic!("reserved remote identity returned the wrong failure: {error}");
            };
            assert!(
                reason.starts_with("overlay remote tab slot"),
                "overlay rejection must identify its authority source: {error}"
            );
            assert!(
                reason.contains(expected_reason),
                "unexpected rejection: {error}"
            );

            let error = reconcile_overlay(
                &valid_overlay,
                std::slice::from_ref(&slot),
                &BTreeSet::new(),
            )
            .expect_err("reserved live identity must fail before reconciliation");
            assert_eq!(error.code(), PersistenceFailureCode::Invalid);
            let PersistenceFailure::Invalid { reason } = &error else {
                panic!("reserved live identity returned the wrong failure: {error}");
            };
            assert!(
                reason.starts_with("live layout remote tab slot"),
                "live rejection must identify its authority source: {error}"
            );
            assert!(
                reason.contains(expected_reason),
                "unexpected live-slot rejection: {error}"
            );
        }
    }

    #[test]
    fn persisted_reserved_zero_domain_binding_is_rejected_before_restore() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let mut state = PersistedState::default();
        state.store_revision = 1;
        state.domain_bindings.push(DomainBindingRecord {
            target_fingerprint: PrivacySafeTargetFingerprint::from_bytes([0x51; 32]),
            binding_id: DomainBindingId::from_bytes([0; 16]),
        });
        let payload = serde_json::to_vec(&state).expect("serialize malformed state payload");
        let sha256: [u8; 32] = Sha256::digest(&payload).into();
        std::fs::write(
            &path,
            serde_json::to_vec(&DiskSlot {
                payload: state,
                sha256,
            })
            .expect("serialize malformed disk slot"),
        )
        .expect("write malformed disk slot");

        let error = load_snapshot_at(&path)
            .expect_err("reserved zero binding must not enter the startup restore cohort");
        assert_eq!(error.code(), PersistenceFailureCode::Invalid);
        assert!(
            error.to_string().contains("reserved zero identity"),
            "unexpected rejection: {error}"
        );
    }

    #[test]
    fn overlay_retirement_roundtrips_and_blocks_every_resurrection_shape() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(30);
        let live = local_overlay(window, 1, 30);
        let mut create = PendingBatch::default();
        create
            .queue_overlay_live(None, live.clone())
            .expect("queue live overlay");
        commit_for_test(&path, &create, WriteInterruption::None).expect("commit live overlay");

        let mut retire = PendingBatch::default();
        retire
            .queue_overlay_delete(window, Some(1))
            .expect("queue exact retirement");
        let retirement =
            commit_for_test(&path, &retire, WriteInterruption::None).expect("commit retirement");
        assert!(retirement.receipt.wrote_new_generation);
        let retired_revision = retirement.receipt.store_revision;

        let snapshot = load_snapshot_at(&path).expect("reload retired overlay");
        assert!(snapshot.overlay(window).is_none());
        assert_eq!(
            snapshot.tombstone(window),
            Some(OverlayTombstone::new(window, 1, retired_revision).expect("expected tombstone"))
        );

        for (base_revision, overlay) in [
            (Some(1), local_overlay(window, 2, 31)),
            (None, local_overlay(window, 1, 32)),
        ] {
            let mut resurrection = PendingBatch::default();
            resurrection
                .queue_overlay_live(base_revision, overlay)
                .expect("queue stale resurrection attempt");
            let rejection = commit_for_test(&path, &resurrection, WriteInterruption::None)
                .expect("retired lineage is partitioned");
            assert!(!rejection.receipt.wrote_new_generation);
            assert_eq!(rejection.receipt.rejected_updates, 1);
            assert_eq!(
                rejection.rejected_overlay_mutations[&window].failure.code(),
                PersistenceFailureCode::RetiredOverlay
            );
        }

        let mut replay = PendingBatch::default();
        replay
            .queue_overlay_delete(window, Some(1))
            .expect("queue exact retirement replay");
        let replay = commit_for_test(&path, &replay, WriteInterruption::None)
            .expect("retirement replay is idempotent");
        assert!(!replay.receipt.wrote_new_generation);
        assert_eq!(replay.receipt.rejected_updates, 0);
        assert_eq!(replay.receipt.committed_updates, 1);
        assert_eq!(
            load_snapshot_at(&path).expect("reload after replays"),
            snapshot
        );
    }

    #[test]
    fn live_then_delete_coalesces_to_one_precommit_tombstone() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(40);
        let mut pending = PendingBatch::default();
        pending
            .queue_overlay_live(None, local_overlay(window, 1, 40))
            .expect("queue live overlay");
        assert_eq!(
            pending
                .queue_overlay_delete(window, Some(1))
                .expect("coalesce retirement"),
            EnqueueOutcome::Coalesced
        );
        let mutation = &pending.overlay_mutations[&window];
        assert_eq!(mutation.base_revision, None);
        assert!(matches!(
            &mutation.desired,
            DesiredOverlayState::Deleted {
                last_local_revision,
                ..
            } if *last_local_revision == 1
        ));
        assert_eq!(mutation.superseded_updates, 1);
        assert_eq!(pending.overlay_tab_count, 0);

        let committed = commit_for_test(&path, &pending, WriteInterruption::None)
            .expect("commit coalesced retirement");
        assert_eq!(committed.receipt.committed_updates, 1);
        assert_eq!(committed.receipt.coalesced_updates, 1);
        let snapshot = load_snapshot_at(&path).expect("load coalesced retirement");
        assert!(snapshot.overlay(window).is_none());
        assert_eq!(
            snapshot
                .tombstone(window)
                .expect("coalesced tombstone")
                .last_local_revision(),
            1
        );
    }

    #[test]
    fn inflight_live_ack_rebases_equal_revision_delete_without_losing_it() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(50);
        let mut pending = PendingBatch::default();
        pending
            .queue_overlay_live(None, local_overlay(window, 1, 50))
            .expect("queue live overlay");
        let in_flight = pending.clone();
        commit_for_test(&path, &in_flight, WriteInterruption::None)
            .expect("commit in-flight live snapshot");

        pending
            .queue_overlay_delete(window, Some(1))
            .expect("queue delete behind in-flight live snapshot");
        pending.acknowledge_resolved(
            &in_flight,
            &BTreeSet::from([window]),
            &BTreeSet::new(),
            &BTreeSet::new(),
        );
        let rebased = &pending.overlay_mutations[&window];
        assert_eq!(rebased.base_revision, Some(1));
        assert!(matches!(
            &rebased.desired,
            DesiredOverlayState::Deleted { .. }
        ));

        commit_for_test(&path, &pending, WriteInterruption::None).expect("commit retained delete");
        let snapshot = load_snapshot_at(&path).expect("load retained delete");
        assert!(snapshot.overlay(window).is_none());
        assert!(snapshot.tombstone(window).is_some());
    }

    #[test]
    fn synced_delete_ack_loss_replays_without_a_second_generation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(60);
        let mut create = PendingBatch::default();
        create
            .queue_overlay_live(None, local_overlay(window, 1, 60))
            .expect("queue live overlay");
        commit_for_test(&path, &create, WriteInterruption::None).expect("commit live overlay");

        let mut retire = PendingBatch::default();
        retire
            .queue_overlay_delete(window, Some(1))
            .expect("queue retirement");
        commit_for_test(&path, &retire, WriteInterruption::AfterSync)
            .expect_err("inject delete acknowledgement loss");
        let after_sync = load_snapshot_at(&path).expect("recover synced retirement");
        assert!(after_sync.overlay(window).is_none());
        assert!(after_sync.tombstone(window).is_some());

        let replay = commit_for_test(&path, &retire, WriteInterruption::None)
            .expect("retry exact retirement");
        assert!(!replay.receipt.wrote_new_generation);
        assert_eq!(replay.receipt.store_revision, after_sync.store_revision);
    }

    #[test]
    fn exact_retry_rebases_a_post_snapshot_overlay_successor_after_ack_loss() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(61);
        let mut initial = PendingBatch::default();
        initial
            .queue_overlay_live(None, local_overlay(window, 1, 61))
            .expect("queue initial overlay");
        commit_for_test(&path, &initial, WriteInterruption::None).expect("commit initial overlay");

        let mut in_flight = PendingBatch::default();
        in_flight
            .queue_overlay_live(Some(1), local_overlay(window, 2, 62))
            .expect("queue in-flight update");
        let shared = CoordinatorShared {
            primary_path: path.clone(),
            pending: Mutex::new(CoordinatorPending {
                batch: in_flight.clone(),
                ..CoordinatorPending::default()
            }),
        };

        commit_for_test(&path, &in_flight, WriteInterruption::AfterDirectorySync)
            .expect_err("inject acknowledgement loss after durable publication");
        {
            let mut pending = lock_pending(&shared.pending);
            pending
                .batch
                .queue_overlay_live(Some(2), local_overlay(window, 3, 63))
                .expect("queue successor behind ambiguous in-flight update");
            assert_eq!(
                pending.batch.overlay_mutations[&window].base_revision,
                Some(1)
            );
        }

        resolve_exact_retry(&shared, &in_flight).expect("resolve exact durable snapshot first");
        let successor = {
            let pending = lock_pending(&shared.pending);
            assert_eq!(
                pending.batch.overlay_mutations[&window].base_revision,
                Some(2)
            );
            assert_eq!(
                pending.batch.overlay_mutations[&window].desired_revision(),
                3
            );
            pending.batch.clone()
        };
        let committed = commit_for_test(&path, &successor, WriteInterruption::None)
            .expect("commit rebased successor");
        let _ = acknowledge_committed_batch(&shared, &successor, &committed);

        let snapshot = load_snapshot_at(&path).expect("load final successor");
        assert_eq!(
            snapshot
                .overlay(window)
                .expect("successor overlay")
                .local_revision(),
            3
        );
        assert!(
            lock_pending(&shared.pending)
                .batch
                .overlay_mutations
                .is_empty()
        );
    }

    #[test]
    fn exact_retry_rebases_a_post_snapshot_delete_after_ack_loss() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(62);
        let mut initial = PendingBatch::default();
        initial
            .queue_overlay_live(None, local_overlay(window, 1, 64))
            .expect("queue initial overlay");
        commit_for_test(&path, &initial, WriteInterruption::None).expect("commit initial overlay");

        let mut in_flight = PendingBatch::default();
        in_flight
            .queue_overlay_live(Some(1), local_overlay(window, 2, 65))
            .expect("queue in-flight update");
        let shared = CoordinatorShared {
            primary_path: path.clone(),
            pending: Mutex::new(CoordinatorPending {
                batch: in_flight.clone(),
                ..CoordinatorPending::default()
            }),
        };

        commit_for_test(&path, &in_flight, WriteInterruption::AfterDirectorySync)
            .expect_err("inject acknowledgement loss after durable publication");
        {
            let mut pending = lock_pending(&shared.pending);
            pending
                .batch
                .queue_overlay_delete(window, Some(2))
                .expect("queue retirement behind ambiguous in-flight update");
        }

        resolve_exact_retry(&shared, &in_flight).expect("resolve exact durable snapshot first");
        let successor = {
            let pending = lock_pending(&shared.pending);
            let mutation = &pending.batch.overlay_mutations[&window];
            assert_eq!(mutation.base_revision, Some(2));
            assert!(matches!(
                &mutation.desired,
                DesiredOverlayState::Deleted { .. }
            ));
            pending.batch.clone()
        };
        let committed = commit_for_test(&path, &successor, WriteInterruption::None)
            .expect("commit rebased retirement");
        let _ = acknowledge_committed_batch(&shared, &successor, &committed);

        let snapshot = load_snapshot_at(&path).expect("load retired overlay");
        assert!(snapshot.overlay(window).is_none());
        assert_eq!(
            snapshot
                .tombstone(window)
                .expect("durable retirement")
                .last_local_revision(),
            2
        );
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
            let base_revision = if revision == 1 {
                None
            } else {
                Some(revision - 1)
            };
            batch
                .queue_overlay_live(base_revision, overlay)
                .expect("monotonic update");
        }
        assert_eq!(batch.overlay_mutations.len(), 1);
        assert_eq!(batch.overlay_mutations[&window].desired_revision(), 64);
        assert_eq!(batch.overlay_mutations[&window].base_revision, None);
        assert_eq!(batch.overlay_mutations[&window].superseded_updates, 63);
    }

    #[test]
    fn equal_revision_with_different_content_is_rejected() {
        let window = LayoutWindowId::from_bytes([0x62; 16]);
        let first = local_slot(1);
        let second = local_slot(2);
        let mut batch = PendingBatch::default();
        batch
            .queue_overlay_live(
                None,
                MixedDomainLayoutOverlay::new(window, "default", 1, vec![first], Some(first))
                    .expect("first"),
            )
            .expect("queue first");
        let failure = batch
            .queue_overlay_live(
                None,
                MixedDomainLayoutOverlay::new(window, "default", 1, vec![second], Some(second))
                    .expect("second"),
            )
            .expect_err("revision reuse must fail");
        assert_eq!(
            failure.code(),
            PersistenceFailureCode::OverlayRevisionConflict
        );
    }

    #[test]
    fn zero_base_revision_cannot_alias_absent_overlay_authority() {
        let window = window_id(64);
        let mut batch = PendingBatch::default();
        let failure = batch
            .queue_overlay_live(Some(0), local_overlay(window, 1, 64))
            .expect_err("zero base must not masquerade as absent authority");
        assert_eq!(failure.code(), PersistenceFailureCode::Invalid);
        assert!(batch.overlay_mutations.is_empty());
    }

    #[test]
    fn local_revision_chain_mismatch_is_not_reported_as_authority_state() {
        let window = window_id(640);
        let mut batch = PendingBatch::default();
        let failure = batch
            .queue_overlay_live(Some(7), local_overlay(window, 3, 64))
            .expect_err("malformed local revision chain must fail before authority lookup");
        assert_eq!(failure.code(), PersistenceFailureCode::Invalid);
        assert!(batch.overlay_mutations.is_empty());
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
            .queue_overlay_live(
                None,
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
            .queue_overlay_live(
                Some(1),
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
        pending.acknowledge_resolved(
            &committed_snapshot,
            &BTreeSet::from([window]),
            &BTreeSet::new(),
            &BTreeSet::new(),
        );

        assert!(pending.window_states["default"].fullscreen);
        assert_eq!(pending.overlay_mutations[&window].desired_revision(), 2);
        assert_eq!(pending.overlay_mutations[&window].base_revision, Some(1));
        assert_eq!(pending.overlay_tab_count, 1);
    }

    #[test]
    fn rejected_ack_invalidates_every_post_snapshot_descendant() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(65);
        let mut exact = PendingBatch::default();
        exact
            .queue_overlay_live(None, local_overlay(window, 1, 65))
            .expect("queue rejected snapshot");
        let rejected_snapshot = exact.clone();
        exact.acknowledge_resolved(
            &rejected_snapshot,
            &BTreeSet::new(),
            &BTreeSet::from([window]),
            &BTreeSet::new(),
        );
        assert!(exact.overlay_mutations.is_empty());
        assert_eq!(exact.overlay_tab_count, 0);

        let mut advanced = rejected_snapshot.clone();
        advanced
            .queue_overlay_live(Some(1), local_overlay(window, 2, 66))
            .expect("queue post-snapshot descendant");
        advanced.acknowledge_resolved(
            &rejected_snapshot,
            &BTreeSet::new(),
            &BTreeSet::from([window]),
            &BTreeSet::new(),
        );
        assert!(advanced.overlay_mutations.is_empty());
        assert_eq!(advanced.overlay_tab_count, 0);
        commit_for_test(&path, &advanced, WriteInterruption::None)
            .expect("empty post-rejection batch cannot publish the descendant");
        assert_eq!(
            load_snapshot_at(&path)
                .expect("load empty authority")
                .source,
            StoreSource::Empty
        );
    }

    #[test]
    fn fresh_coalesced_overlay_chain_can_publish_its_latest_revision() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(651);
        let mut batch = PendingBatch::default();
        batch
            .queue_overlay_live(None, local_overlay(window, 1, 67))
            .expect("queue first in-memory revision");
        batch
            .queue_overlay_live(Some(1), local_overlay(window, 2, 68))
            .expect("coalesce second in-memory revision");
        assert_eq!(batch.overlay_mutations[&window].base_revision, None);

        let committed = commit_for_test(&path, &batch, WriteInterruption::None)
            .expect("publish latest coalesced create");
        assert_eq!(committed.receipt.rejected_updates, 0);
        assert_eq!(committed.receipt.coalesced_updates, 1);
        assert_eq!(
            load_snapshot_at(&path)
                .expect("load coalesced create")
                .overlay(window)
                .expect("published overlay")
                .local_revision(),
            2
        );
    }

    #[test]
    fn binding_acknowledgement_preserves_post_snapshot_waiters() {
        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x64; 32]);
        let mut pending = PendingBatch::default();
        pending.ensure_bindings.insert(fingerprint);
        let committed_snapshot = pending.clone();

        pending.acknowledge_resolved(
            &committed_snapshot,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::from([fingerprint]),
        );
        assert!(pending.ensure_bindings.contains(&fingerprint));

        pending.acknowledge_resolved(
            &committed_snapshot,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
        );
        assert!(!pending.ensure_bindings.contains(&fingerprint));
    }

    #[test]
    fn pending_overlay_tab_quota_is_enforced_before_growth() {
        let mut pending = PendingBatch::default();
        for window_number in 1u8..=4 {
            let slots = local_slots(window_number, MAX_TABS_PER_OVERLAY);
            let active = slots.first().copied();
            pending
                .queue_overlay_live(
                    None,
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
            .queue_overlay_live(
                None,
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
        assert_eq!(pending.overlay_mutations.len(), 4);
    }

    #[test]
    fn encoded_byte_quota_rejects_one_workspace_without_poisoning_other_lineages() {
        let state = PersistedState::default();
        let valid_window = LayoutWindowId::from_bytes([0x44; 16]);
        let valid_fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x55; 32]);
        let valid_workspace = "z-valid".to_string();
        let valid_geometry = PersistedWindowState {
            maximized: true,
            fullscreen: false,
        };
        let valid_overlay = local_overlay(valid_window, 1, 0x66);

        let mut valid_only = PendingBatch::default();
        valid_only
            .queue_window_state(valid_workspace.clone(), valid_geometry)
            .expect("queue valid workspace");
        valid_only.ensure_bindings.insert(valid_fingerprint);
        valid_only
            .queue_overlay_live(None, valid_overlay.clone())
            .expect("queue valid overlay");
        let exact_limit = preflight_batch_with_byte_limit(&state, &valid_only, false, u64::MAX)
            .expect("measure valid mixed batch")
            .encoded_upper_bound;

        let oversized_workspace = format!("a-{}", "x".repeat(MAX_WORKSPACE_BYTES - 2));
        assert_eq!(oversized_workspace.len(), MAX_WORKSPACE_BYTES);
        let mut mixed = valid_only;
        mixed
            .queue_window_state(
                oversized_workspace.clone(),
                PersistedWindowState {
                    maximized: false,
                    fullscreen: true,
                },
            )
            .expect("queue independently valid long workspace");

        let preflight = preflight_batch_with_byte_limit(&state, &mixed, false, exact_limit)
            .expect("partition byte-exhausting workspace");
        assert_eq!(preflight.encoded_upper_bound, exact_limit);
        assert!(preflight.accepted_workspaces.contains(&valid_workspace));
        assert_eq!(
            preflight.rejected_workspaces[&oversized_workspace].code(),
            // Cardinality remains below its cap; encoded bytes are the sole
            // violated resource, whose public classification is Oversized.
            PersistenceFailureCode::Oversized
        );
        assert!(preflight.accepted_bindings.contains(&valid_fingerprint));
        assert!(
            preflight
                .overlays
                .accepted_overlay_ids
                .contains(&valid_window)
        );
        assert_eq!(
            preflight.overlays.apply_overlay_ids,
            BTreeSet::from([valid_window])
        );
    }

    #[test]
    fn encoded_byte_admission_accepts_exact_boundary_and_rejects_plus_one() {
        assert_eq!(MAX_STATE_FILE_BYTES, 4 * 1024 * 1024);
        let (state, workspace) = workspace_state_at_encoded_upper_bound(MAX_STATE_FILE_BYTES);
        validate_published_state(&state)
            .expect("literal 4 MiB boundary state is structurally valid");
        let budget = EncodedStateBudget::from_state(&state, state.store_revision)
            .expect("count literal 4 MiB boundary state");
        assert_eq!(
            budget.upper_bound().expect("boundary upper bound"),
            MAX_STATE_FILE_BYTES
        );
        let encoded = encode_disk_slot(&state).expect("literal boundary state physically encodes");
        eprintln!(
            "window_state_persist boundary: admitted_upper_bound={} physical_encoded_bytes={} limit={}",
            budget.upper_bound().expect("boundary upper bound evidence"),
            encoded.len(),
            MAX_STATE_FILE_BYTES
        );
        assert!(
            u64::try_from(encoded.len()).expect("encoded boundary length fits u64")
                <= MAX_STATE_FILE_BYTES
        );

        let exact = preflight_batch(&state, &PendingBatch::default(), false)
            .expect("literal 4 MiB byte boundary is admissible");
        assert_eq!(exact.encoded_upper_bound, MAX_STATE_FILE_BYTES);

        let mut plus_one = PendingBatch::default();
        plus_one
            .queue_window_state(
                workspace.clone(),
                PersistedWindowState {
                    // `false` is one JSON byte wider than the committed
                    // `true`; every other field and collection is identical.
                    maximized: false,
                    fullscreen: true,
                },
            )
            .expect("queue one-byte-wider workspace value");
        let rejected = preflight_batch(&state, &plus_one, false)
            .expect("plus-one candidate yields a lineage-isolated rejection");
        assert!(!rejected.accepted_workspaces.contains(&workspace));
        assert_eq!(rejected.encoded_upper_bound, MAX_STATE_FILE_BYTES);
        assert_eq!(
            rejected.rejected_workspaces[&workspace],
            PersistenceFailure::EncodedQuota {
                projected_upper_bound: MAX_STATE_FILE_BYTES + 1,
                maximum: MAX_STATE_FILE_BYTES,
            }
        );
    }

    #[test]
    fn controlled_worker_rebases_stale_mixed_batch_at_literal_byte_ceiling() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let target_before_race = MAX_STATE_FILE_BYTES - 1;
        let (base, external_growth_workspace, stale_growth_workspace, base_overlay) =
            near_ceiling_state_with_overlay(target_before_race);
        std::fs::write(
            &path,
            encode_disk_slot(&base).expect("encode near-ceiling mixed authority"),
        )
        .expect("write near-ceiling mixed authority");

        let updated_overlay = local_overlay(base_overlay.window_id(), 2, 0x92);
        let one_byte_wider_state = PersistedWindowState {
            maximized: false,
            fullscreen: true,
        };
        let mut stale_batch = PendingBatch::default();
        stale_batch
            .queue_window_state(stale_growth_workspace.clone(), one_byte_wider_state)
            .expect("queue stale one-byte workspace growth");
        stale_batch
            .queue_overlay_live(Some(base_overlay.local_revision()), updated_overlay.clone())
            .expect("queue stale same-width overlay update");
        let local_preflight = preflight_batch(&base, &stale_batch, false)
            .expect("stale mixed batch fits before the authority race");
        assert!(
            local_preflight
                .accepted_workspaces
                .contains(&stale_growth_workspace)
        );
        assert!(
            local_preflight
                .overlays
                .apply_overlay_ids
                .contains(&base_overlay.window_id())
        );
        assert_eq!(local_preflight.encoded_upper_bound, MAX_STATE_FILE_BYTES);

        let worker = ControlledPersistenceWorker::open(path.clone());
        worker.expect_waiting(1, false);
        let flush = worker.admit_batch_with_flush(stale_batch.clone());
        worker.continue_wake(1);
        let frozen = worker.expect_commit(1, TestWorkerCommitPhase::Pending);
        assert_eq!(
            frozen.window_states.get(&stale_growth_workspace),
            Some(&one_byte_wider_state)
        );
        assert!(
            frozen
                .overlay_mutations
                .contains_key(&base_overlay.window_id())
        );

        // Interpose a literal second process' one-byte authority growth after
        // the worker freezes its queued batch but before it takes the file
        // lock. The child must publish a success marker; a misspelled test
        // filter therefore cannot silently turn this into a zero-test pass.
        let child_marker = temp.path().join("external-growth-committed");
        let child = Command::new(std::env::current_exe().expect("resolve test executable"))
            .env_clear()
            .args(["--exact", CROSS_PROCESS_GROWTH_HELPER, "--nocapture"])
            .env(CROSS_PROCESS_STATE_PATH_ENV, &path)
            .env(
                CROSS_PROCESS_WORKSPACE_ENV,
                external_growth_workspace.as_str(),
            )
            .env(CROSS_PROCESS_MARKER_ENV, &child_marker)
            .output()
            .expect("run external authority writer process");
        assert!(
            child.status.success(),
            "external authority writer failed: status={:?}\nstdout={}\nstderr={}",
            child.status.code(),
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr),
        );
        assert_eq!(
            std::fs::read_to_string(&child_marker)
                .expect("external authority writer success marker"),
            "2"
        );

        worker.release_commit(1, TestWorkerCommitAction::Run(WriteInterruption::None));
        let receipt = match worker.expect_commit_finished(1, TestWorkerCommitPhase::Pending) {
            TestWorkerCommitResult::Committed(receipt) => receipt,
            TestWorkerCommitResult::Failed(code) => {
                panic!("rebased near-ceiling worker commit failed: {code:?}")
            }
        };
        assert!(receipt.wrote_new_generation);
        assert_eq!(receipt.committed_updates, 1);
        assert_eq!(receipt.rejected_updates, 1);
        worker.continue_after_commit(1);
        assert_eq!(
            flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("rebased near-ceiling flush response")
                .expect_err("byte-rejected lineage reaches its flush waiter")
                .code(),
            PersistenceFailureCode::Oversized
        );
        worker.expect_waiting(2, false);

        let snapshot = load_snapshot_at(&path).expect("load rebased near-ceiling authority");
        assert_eq!(
            snapshot.window_states[&external_growth_workspace],
            one_byte_wider_state
        );
        assert_eq!(
            snapshot.window_states[&stale_growth_workspace],
            PersistedWindowState {
                maximized: true,
                fullscreen: true,
            }
        );
        assert_eq!(
            snapshot
                .overlay(base_overlay.window_id())
                .expect("rebased overlay remains live"),
            &updated_overlay
        );
        let restored = PersistedState {
            schema_version: STORE_SCHEMA_VERSION,
            store_revision: snapshot.store_revision,
            window_states: snapshot.window_states.clone(),
            domain_bindings: snapshot.domain_bindings.clone(),
            overlays: snapshot.overlays.clone(),
            tombstones: snapshot.tombstones.clone(),
        };
        let restored_budget = EncodedStateBudget::from_state(&restored, restored.store_revision)
            .and_then(|budget| budget.upper_bound())
            .expect("count rebased near-ceiling authority");
        let restored_encoded =
            encode_disk_slot(&restored).expect("encode rebased near-ceiling authority");
        eprintln!(
            "window_state_persist stale-rebase: admitted_upper_bound={} physical_encoded_bytes={} limit={} committed={} rejected={}",
            restored_budget,
            restored_encoded.len(),
            MAX_STATE_FILE_BYTES,
            receipt.committed_updates,
            receipt.rejected_updates
        );
        assert_eq!(restored_budget, MAX_STATE_FILE_BYTES);
        assert!(
            u64::try_from(restored_encoded.len()).expect("restored length fits u64")
                <= MAX_STATE_FILE_BYTES
        );
        let pending = lock_pending(&worker.writer().shared.pending);
        assert!(pending.batch.window_states.is_empty());
        assert!(pending.batch.overlay_mutations.is_empty());
        assert!(pending.flush_waiters.is_empty());
        assert!(pending.binding_waiters.is_empty());
        assert_eq!(pending.waiter_count, 0);
        drop(pending);

        assert_eq!(
            worker.stop_and_join(),
            TestWorkerStopped {
                waiting_epoch: 2,
                commit_epoch: 1,
            }
        );
    }

    #[test]
    fn near_ceiling_external_growth_process_helper() {
        let Some(path) = std::env::var_os(CROSS_PROCESS_STATE_PATH_ENV) else {
            // Ordinary parent-suite execution must remain inert. This helper
            // performs work only when the controlling test supplies all three
            // private process-contract variables.
            return;
        };
        let workspace = std::env::var(CROSS_PROCESS_WORKSPACE_ENV)
            .expect("external authority writer workspace");
        let marker = std::env::var_os(CROSS_PROCESS_MARKER_ENV)
            .map(PathBuf::from)
            .expect("external authority writer marker path");
        let mut external_batch = PendingBatch::default();
        external_batch
            .queue_window_state(
                workspace,
                PersistedWindowState {
                    maximized: false,
                    fullscreen: true,
                },
            )
            .expect("queue external one-byte authority growth");
        let external = commit_for_test(Path::new(&path), &external_batch, WriteInterruption::None)
            .expect("publish intervening authority growth from child process");
        assert_eq!(external.receipt.committed_updates, 1);
        assert_eq!(external.receipt.rejected_updates, 0);
        assert!(external.receipt.wrote_new_generation);
        std::fs::write(marker, external.receipt.store_revision.to_string())
            .expect("publish external authority writer success marker");
    }

    #[test]
    fn near_ceiling_mixed_batch_retry_is_bounded_across_every_crash_point() {
        for interruption in [
            WriteInterruption::AfterTruncate,
            WriteInterruption::AfterPartialWrite,
            WriteInterruption::AfterFullWrite,
            WriteInterruption::AfterSync,
            WriteInterruption::AfterDirectorySync,
        ] {
            let temp = tempfile::tempdir().expect("tempdir");
            let path = temp.path().join("window-state.json");
            let (base, external_growth_workspace, stale_growth_workspace, base_overlay) =
                near_ceiling_state_with_overlay(MAX_STATE_FILE_BYTES - 1);
            std::fs::write(
                &path,
                encode_disk_slot(&base).expect("encode crash-matrix base authority"),
            )
            .expect("write crash-matrix base authority");

            let one_byte_wider_state = PersistedWindowState {
                maximized: false,
                fullscreen: true,
            };
            let mut external_batch = PendingBatch::default();
            external_batch
                .queue_window_state(external_growth_workspace.clone(), one_byte_wider_state)
                .expect("queue crash-matrix external growth");
            commit_for_test(&path, &external_batch, WriteInterruption::None)
                .expect("publish crash-matrix external growth");

            let updated_overlay = local_overlay(base_overlay.window_id(), 2, 0x92);
            let mut stale_batch = PendingBatch::default();
            stale_batch
                .queue_window_state(stale_growth_workspace.clone(), one_byte_wider_state)
                .expect("queue crash-matrix stale growth");
            stale_batch
                .queue_overlay_live(Some(base_overlay.local_revision()), updated_overlay.clone())
                .expect("queue crash-matrix overlay update");
            assert_eq!(
                commit_for_test(&path, &stale_batch, interruption)
                    .expect_err("inject crash during partitioned mixed commit")
                    .code(),
                PersistenceFailureCode::Io
            );

            let replay = commit_for_test(&path, &stale_batch, WriteInterruption::None)
                .expect("retry exact partitioned mixed batch");
            assert_eq!(replay.receipt.committed_updates, 1);
            assert_eq!(replay.receipt.rejected_updates, 1);
            assert_eq!(
                replay.rejected_workspaces[&stale_growth_workspace],
                PersistenceFailure::EncodedQuota {
                    projected_upper_bound: MAX_STATE_FILE_BYTES + 1,
                    maximum: MAX_STATE_FILE_BYTES,
                }
            );
            assert!(
                replay
                    .accepted_overlay_ids
                    .contains(&base_overlay.window_id())
            );

            let snapshot = load_snapshot_at(&path).expect("load crash-matrix retry authority");
            assert_eq!(
                snapshot.window_states[&external_growth_workspace],
                one_byte_wider_state
            );
            assert_eq!(
                snapshot.window_states[&stale_growth_workspace],
                PersistedWindowState {
                    maximized: true,
                    fullscreen: true,
                }
            );
            assert_eq!(
                snapshot
                    .overlay(base_overlay.window_id())
                    .expect("crash-matrix overlay remains live"),
                &updated_overlay
            );
            let restored = PersistedState {
                schema_version: STORE_SCHEMA_VERSION,
                store_revision: snapshot.store_revision,
                window_states: snapshot.window_states,
                domain_bindings: snapshot.domain_bindings,
                overlays: snapshot.overlays,
                tombstones: snapshot.tombstones,
            };
            let restored_upper_bound =
                EncodedStateBudget::from_state(&restored, restored.store_revision)
                    .and_then(|budget| budget.upper_bound())
                    .expect("count crash-matrix retry authority");
            let restored_encoded =
                encode_disk_slot(&restored).expect("encode crash-matrix retry authority");
            eprintln!(
                "window_state_persist crash-retry {interruption:?}: admitted_upper_bound={} physical_encoded_bytes={} limit={} wrote_new_generation={}",
                restored_upper_bound,
                restored_encoded.len(),
                MAX_STATE_FILE_BYTES,
                replay.receipt.wrote_new_generation
            );
            assert_eq!(restored_upper_bound, MAX_STATE_FILE_BYTES);
            assert!(
                u64::try_from(restored_encoded.len()).expect("retry length fits u64")
                    <= MAX_STATE_FILE_BYTES
            );
        }
    }

    #[test]
    fn all_maxima_escaped_composite_is_countable_and_rejected_before_publication() {
        let state = all_maxima_escaped_state();
        validate_published_state(&state)
            .expect("all independent structural maxima compose into a valid state shape");
        assert_eq!(state.window_states.len(), MAX_WORKSPACES);
        assert_eq!(state.domain_bindings.len(), MAX_DOMAIN_BINDINGS);
        assert_eq!(state.overlays.len(), MAX_LAYOUT_OVERLAYS);
        assert_eq!(state.tombstones.len(), MAX_OVERLAY_TOMBSTONES);
        assert_eq!(
            state
                .overlays
                .iter()
                .map(|overlay| overlay.slots.len())
                .sum::<usize>(),
            MAX_TOTAL_OVERLAY_TABS
        );
        assert_eq!(
            state
                .overlays
                .iter()
                .filter(|overlay| overlay.slots.len() == MAX_TABS_PER_OVERLAY)
                .count(),
            MAX_TOTAL_OVERLAY_TABS / MAX_TABS_PER_OVERLAY
        );
        assert!(state.window_states.keys().all(|workspace| {
            workspace.len() == MAX_WORKSPACE_BYTES
                && workspace.contains('"')
                && workspace.contains('\\')
                && encoded_json_len(workspace).expect("count escaped workspace")
                    > u64::try_from(workspace.len()).expect("workspace length fits u64") + 2
        }));
        assert!(state.overlays.iter().all(|overlay| {
            overlay.workspace.len() == MAX_WORKSPACE_BYTES
                && overlay.local_revision == u64::MAX
                && overlay.window_id.as_bytes().iter().all(|byte| *byte >= 100)
        }));
        assert!(state.tombstones.iter().all(|tombstone| {
            tombstone.last_local_revision == u64::MAX
                && tombstone.retired_at_store_revision == u64::MAX
                && tombstone
                    .window_id
                    .as_bytes()
                    .iter()
                    .all(|byte| *byte >= 100)
        }));

        for binding in &state.domain_bindings {
            assert!(
                binding
                    .target_fingerprint
                    .as_bytes()
                    .iter()
                    .all(|byte| *byte >= 100)
            );
            assert!(
                binding
                    .binding_id
                    .as_bytes()
                    .iter()
                    .all(|byte| *byte >= 100)
            );
            assert_eq!(
                encoded_json_len(binding).expect("count maximum-width binding"),
                maximum_width_binding_len(binding.target_fingerprint)
                    .expect("count normalized maximum-width binding")
            );
        }

        let maximum_remote_slot_bytes = encoded_json_len(&StableTabSlot::remote(
            DomainBindingId::from_bytes([u8::MAX; 16]),
            StableMuxSessionId::from_bytes([u8::MAX; 16]),
            MAX_ADMISSIBLE_REMOTE_ID,
            MAX_ADMISSIBLE_REMOTE_ID,
        ))
        .expect("count canonical maximum-width remote slot");
        for slot in state
            .overlays
            .iter()
            .flat_map(|overlay| overlay.slots.iter())
        {
            let StableTabSlot::Remote {
                binding_id,
                session_id,
                remote_window_id,
                remote_tab_id,
            } = slot
            else {
                panic!("all-maxima composite must use remote slots");
            };
            assert!(binding_id.as_bytes().iter().all(|byte| *byte >= 100));
            assert!(session_id.as_bytes().iter().all(|byte| *byte >= 100));
            assert_eq!(*remote_window_id, MAX_ADMISSIBLE_REMOTE_ID);
            assert_eq!(*remote_tab_id, MAX_ADMISSIBLE_REMOTE_ID);
            assert_eq!(
                encoded_json_len(slot).expect("count maximum-width remote slot"),
                maximum_remote_slot_bytes
            );
        }

        let budget = EncodedStateBudget::from_state(&state, state.store_revision)
            .expect("count deterministic all-maxima composite");
        let projected_upper_bound = budget.upper_bound().expect("all-maxima upper bound");
        let maximum_checksum_oracle = encoded_json_len(&BorrowedDiskSlot {
            payload: &state,
            sha256: [u8::MAX; 32],
        })
        .expect("count all-maxima serializer oracle");
        assert_eq!(projected_upper_bound, maximum_checksum_oracle);
        assert!(projected_upper_bound > MAX_STATE_FILE_BYTES);

        let failure = encode_disk_slot(&state)
            .expect_err("all-maxima composite must be rejected before slot publication");
        let PersistenceFailure::Oversized { actual, maximum } = failure else {
            panic!("all-maxima composite must fail with physical Oversized evidence");
        };
        eprintln!(
            "window_state_persist all-maxima: admitted_upper_bound={} physical_encoded_bytes={} limit={}",
            projected_upper_bound, actual, maximum
        );
        assert_eq!(maximum, MAX_STATE_FILE_BYTES);
        assert!(actual > MAX_STATE_FILE_BYTES);
        assert!(actual <= projected_upper_bound);
    }

    #[test]
    fn aggregate_admission_preserves_mutually_enabling_byte_and_tab_reductions() {
        let growing_window = LayoutWindowId::from_bytes([0; 16]);
        let shrinking_window = LayoutWindowId::from_bytes([u8::MAX; 16]);
        let mut state = PersistedState::default();
        state.store_revision = 1;
        state.overlays.push(
            MixedDomainLayoutOverlay::new(
                growing_window,
                "x".repeat(MAX_WORKSPACE_BYTES),
                1,
                Vec::new(),
                None,
            )
            .expect("large empty growing overlay"),
        );
        let shrinking_slots = local_slots(0xf0, MAX_TABS_PER_OVERLAY);
        state.overlays.push(
            MixedDomainLayoutOverlay::new(
                shrinking_window,
                "s",
                1,
                shrinking_slots.clone(),
                shrinking_slots.first().copied(),
            )
            .expect("full shrinking overlay"),
        );
        for number in 1..=3 {
            let slots = local_slots(
                u8::try_from(number).expect("test window number fits u8"),
                MAX_TABS_PER_OVERLAY,
            );
            state.overlays.push(
                MixedDomainLayoutOverlay::new(
                    window_id(number),
                    "stable",
                    1,
                    slots.clone(),
                    slots.first().copied(),
                )
                .expect("full stable overlay"),
            );
        }
        canonicalize_state(&mut state);
        validate_state(&state).expect("authority is at the aggregate tab cap");

        let growth_slot = local_slot(0x7a);
        let mut reduced_slots = shrinking_slots;
        reduced_slots.pop();
        let mut exchange = PendingBatch::default();
        exchange
            .queue_overlay_live(
                Some(1),
                MixedDomainLayoutOverlay::new(
                    growing_window,
                    "g",
                    2,
                    vec![growth_slot],
                    Some(growth_slot),
                )
                .expect("byte-reducing one-tab growth"),
            )
            .expect("queue count growth");
        exchange
            .queue_overlay_live(
                Some(1),
                MixedDomainLayoutOverlay::new(
                    shrinking_window,
                    "x".repeat(MAX_WORKSPACE_BYTES),
                    2,
                    reduced_slots.clone(),
                    reduced_slots.first().copied(),
                )
                .expect("byte-growing one-tab shrink"),
            )
            .expect("queue count shrink");

        let exchange_limit = preflight_batch_with_byte_limit(&state, &exchange, false, u64::MAX)
            .expect("measure aggregate-valid exchange")
            .encoded_upper_bound;
        // This poison is deliberately much smaller than the byte-growing
        // half of the valid exchange. A relief-only greedy selector would
        // remove that required half first and lose both overlay mutations.
        let unrelated_poison_workspace = "c".to_string();
        let mut batch = exchange;
        batch
            .queue_window_state(
                unrelated_poison_workspace.clone(),
                PersistedWindowState {
                    maximized: true,
                    fullscreen: true,
                },
            )
            .expect("queue unrelated byte poison");
        let preflight = preflight_batch_with_byte_limit(&state, &batch, false, exchange_limit)
            .expect("unrelated poison must not break the aggregate-valid exchange");
        assert_eq!(
            preflight.overlays.apply_overlay_ids,
            BTreeSet::from([growing_window, shrinking_window])
        );
        assert!(preflight.overlays.rejected_overlay_mutations.is_empty());
        assert_eq!(
            preflight.rejected_workspaces[&unrelated_poison_workspace].code(),
            PersistenceFailureCode::Oversized
        );
        assert_eq!(preflight.encoded_upper_bound, exchange_limit);
        let mut projected = state;
        apply_batch(&mut projected, &batch, &preflight, None)
            .expect("apply aggregate-valid exchange");
        validate_state(&projected).expect("aggregate exchange preserves every state quota");
        assert_eq!(
            projected
                .overlays
                .iter()
                .map(|overlay| overlay.slots.len())
                .sum::<usize>(),
            MAX_TOTAL_OVERLAY_TABS
        );
    }

    #[test]
    fn byte_rejected_overlay_does_not_prevent_later_capacity_backfill() {
        let oversized_window = window_id(0);
        let backfill_window = window_id(1);
        let mut state = PersistedState::default();
        state.store_revision = 1;
        state.overlays = (10..10 + MAX_LAYOUT_OVERLAYS - 1)
            .map(|index| {
                MixedDomainLayoutOverlay::new(
                    window_id(u64::try_from(index).expect("overlay index fits u64")),
                    "stable",
                    1,
                    Vec::new(),
                    None,
                )
                .expect("valid empty overlay")
            })
            .collect();
        canonicalize_state(&mut state);
        validate_state(&state).expect("authority has one live-overlay slot available");

        let backfill_overlay =
            MixedDomainLayoutOverlay::new(backfill_window, "backfill", 1, Vec::new(), None)
                .expect("small backfill overlay");
        let mut backfill_only = PendingBatch::default();
        backfill_only
            .queue_overlay_live(None, backfill_overlay.clone())
            .expect("queue backfill overlay");
        let exact_backfill_limit =
            preflight_batch_with_byte_limit(&state, &backfill_only, false, u64::MAX)
                .expect("measure one valid backfill")
                .encoded_upper_bound;

        let mut mixed = backfill_only;
        mixed
            .queue_overlay_live(
                None,
                MixedDomainLayoutOverlay::new(
                    oversized_window,
                    "x".repeat(MAX_WORKSPACE_BYTES),
                    1,
                    Vec::new(),
                    None,
                )
                .expect("independently valid but byte-expensive overlay"),
            )
            .expect("queue byte-expensive overlay");
        let preflight =
            preflight_batch_with_byte_limit(&state, &mixed, false, exact_backfill_limit)
                .expect("backfill around rejected earlier candidate");

        assert_eq!(
            preflight.overlays.apply_overlay_ids,
            BTreeSet::from([backfill_window])
        );
        assert!(
            preflight
                .overlays
                .rejected_overlay_mutations
                .contains_key(&oversized_window)
        );
        assert_eq!(preflight.encoded_upper_bound, exact_backfill_limit);
    }

    #[test]
    fn byte_rejected_workspace_does_not_consume_the_last_cardinality_slot() {
        let oversized_workspace = format!("a-{}", "x".repeat(MAX_WORKSPACE_BYTES - 2));
        let backfill_workspace = "z-backfill".to_string();
        let geometry = PersistedWindowState {
            maximized: true,
            fullscreen: false,
        };
        let mut state = PersistedState::default();
        state.store_revision = 1;
        state.window_states = (0..MAX_WORKSPACES - 1)
            .map(|index| {
                (
                    format!("existing-{index:04}"),
                    PersistedWindowState::default(),
                )
            })
            .collect();
        validate_state(&state).expect("authority has one workspace slot available");

        let mut backfill_only = PendingBatch::default();
        backfill_only
            .queue_window_state(backfill_workspace.clone(), geometry)
            .expect("queue small workspace backfill");
        let exact_backfill_limit =
            preflight_batch_with_byte_limit(&state, &backfill_only, false, u64::MAX)
                .expect("measure workspace backfill")
                .encoded_upper_bound;
        let mut mixed = backfill_only;
        mixed
            .queue_window_state(
                oversized_workspace.clone(),
                PersistedWindowState {
                    maximized: false,
                    fullscreen: true,
                },
            )
            .expect("queue byte-expensive workspace");

        let preflight =
            preflight_batch_with_byte_limit(&state, &mixed, false, exact_backfill_limit)
                .expect("backfill the cardinality slot after byte rejection");
        assert!(preflight.accepted_workspaces.contains(&backfill_workspace));
        assert!(!preflight.accepted_workspaces.contains(&oversized_workspace));
        assert_eq!(
            preflight.rejected_workspaces[&oversized_workspace].code(),
            // With the backfill accepted, restoring this candidate would
            // exceed both byte and workspace caps. Count-quota precedence is
            // intentional and leaves the one remaining slot to the backfill.
            PersistenceFailureCode::Quota
        );
        assert_eq!(preflight.encoded_upper_bound, exact_backfill_limit);
    }

    #[test]
    fn byte_rejected_binding_does_not_consume_the_last_cardinality_slot() {
        let oversized_fingerprint = {
            let mut bytes = [u8::MAX; 32];
            bytes[0] = 0;
            PrivacySafeTargetFingerprint::from_bytes(bytes)
        };
        let backfill_fingerprint = {
            let mut bytes = [0u8; 32];
            bytes[0] = 1;
            PrivacySafeTargetFingerprint::from_bytes(bytes)
        };
        assert!(oversized_fingerprint < backfill_fingerprint);
        let mut state = PersistedState::default();
        state.store_revision = 1;
        state.domain_bindings = (10..10 + MAX_DOMAIN_BINDINGS - 1)
            .map(|index| {
                let index = u64::try_from(index).expect("binding index fits u64");
                DomainBindingRecord {
                    target_fingerprint: indexed_fingerprint(index),
                    binding_id: indexed_binding_id(index),
                }
            })
            .collect();
        canonicalize_state(&mut state);
        validate_state(&state).expect("authority has one binding slot available");

        let mut backfill_only = PendingBatch::default();
        backfill_only.ensure_bindings.insert(backfill_fingerprint);
        let exact_backfill_limit =
            preflight_batch_with_byte_limit(&state, &backfill_only, false, u64::MAX)
                .expect("measure binding backfill")
                .encoded_upper_bound;
        let mut mixed = backfill_only;
        mixed.ensure_bindings.insert(oversized_fingerprint);

        let preflight =
            preflight_batch_with_byte_limit(&state, &mixed, false, exact_backfill_limit)
                .expect("backfill the binding slot after byte rejection");
        assert!(preflight.accepted_bindings.contains(&backfill_fingerprint));
        assert!(!preflight.accepted_bindings.contains(&oversized_fingerprint));
        assert_eq!(
            preflight.rejected_bindings[&oversized_fingerprint].code(),
            PersistenceFailureCode::Quota
        );
        assert_eq!(preflight.encoded_upper_bound, exact_backfill_limit);
    }

    #[test]
    fn commit_and_ack_partition_an_oversized_workspace_without_losing_valid_work() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let valid_workspace = "z-valid".to_string();
        let oversized_workspace = format!("a-{}", "x".repeat(MAX_WORKSPACE_BYTES - 2));
        let geometry = PersistedWindowState {
            maximized: true,
            fullscreen: false,
        };
        let mut valid_only = PendingBatch::default();
        valid_only
            .queue_window_state(valid_workspace.clone(), geometry)
            .expect("queue valid workspace");
        let exact_limit = preflight_batch_with_byte_limit(
            &PersistedState::default(),
            &valid_only,
            false,
            u64::MAX,
        )
        .expect("measure valid commit")
        .encoded_upper_bound;

        let mut batch = valid_only;
        batch
            .queue_window_state(
                oversized_workspace.clone(),
                PersistedWindowState {
                    maximized: false,
                    fullscreen: true,
                },
            )
            .expect("queue independently valid long workspace");
        let committed =
            commit_batch_with_byte_limit(&path, &batch, WriteInterruption::None, exact_limit)
                .expect("commit valid lineage and reject oversized lineage");

        assert_eq!(committed.receipt.committed_updates, 1);
        assert_eq!(committed.receipt.rejected_updates, 1);
        assert!(committed.receipt.wrote_new_generation);
        assert_eq!(
            committed.rejected_workspaces[&oversized_workspace].code(),
            PersistenceFailureCode::Oversized
        );
        let snapshot = load_snapshot_at(&path).expect("load partitioned commit");
        assert_eq!(
            snapshot.window_states.get(&valid_workspace),
            Some(&geometry)
        );
        assert!(!snapshot.window_states.contains_key(&oversized_workspace));

        let shared = CoordinatorShared {
            primary_path: path,
            pending: Mutex::new(CoordinatorPending {
                batch: batch.clone(),
                ..CoordinatorPending::default()
            }),
        };
        let semantic_failure = acknowledge_committed_batch(&shared, &batch, &committed)
            .expect("acknowledgement retains the typed rejection");
        assert_eq!(
            semantic_failure.failure.code(),
            PersistenceFailureCode::Oversized
        );
        let pending = lock_pending(&shared.pending);
        assert!(pending.batch.window_states.is_empty());
        assert!(pending.batch.ensure_bindings.is_empty());
        assert!(pending.batch.overlay_mutations.is_empty());
        assert_eq!(pending.batch.overlay_tab_count, 0);
    }

    #[test]
    fn byte_quota_rejects_an_ownership_transfer_component_atomically() {
        let source_window = LayoutWindowId::from_bytes([0x10; 16]);
        let destination_window = LayoutWindowId::from_bytes([0x20; 16]);
        let transferred = local_slot(0x30);
        let mut state = PersistedState::default();
        state.store_revision = 1;
        state.overlays.push(
            MixedDomainLayoutOverlay::new(
                source_window,
                "source",
                1,
                vec![transferred],
                Some(transferred),
            )
            .expect("source overlay"),
        );
        state.overlays.push(
            MixedDomainLayoutOverlay::new(destination_window, "destination", 1, Vec::new(), None)
                .expect("empty destination overlay"),
        );
        canonicalize_state(&mut state);
        validate_state(&state).expect("valid transfer authority");
        let unchanged_upper_bound = EncodedStateBudget::from_state(&state, 2)
            .and_then(|budget| budget.upper_bound())
            .expect("count unchanged next-generation authority");

        let mut batch = PendingBatch::default();
        batch
            .queue_overlay_live(
                Some(1),
                MixedDomainLayoutOverlay::new(source_window, "source", 2, Vec::new(), None)
                    .expect("source release"),
            )
            .expect("queue source release");
        batch
            .queue_overlay_live(
                Some(1),
                MixedDomainLayoutOverlay::new(
                    destination_window,
                    "x".repeat(MAX_WORKSPACE_BYTES),
                    2,
                    vec![transferred],
                    Some(transferred),
                )
                .expect("large destination acquisition"),
            )
            .expect("queue destination acquisition");

        let preflight =
            preflight_batch_with_byte_limit(&state, &batch, false, unchanged_upper_bound)
                .expect("reject transfer component semantically");
        assert!(preflight.overlays.apply_overlay_ids.is_empty());
        assert!(preflight.overlays.accepted_overlay_ids.is_empty());
        assert_eq!(preflight.overlays.rejected_overlay_mutations.len(), 2);
        for window_id in [source_window, destination_window] {
            assert_eq!(
                preflight.overlays.rejected_overlay_mutations[&window_id]
                    .failure
                    .code(),
                PersistenceFailureCode::Oversized
            );
        }
    }

    #[test]
    fn byte_rejection_cannot_leave_growth_dependent_on_a_rejected_tab_shrink() {
        let growing_window = LayoutWindowId::from_bytes([0; 16]);
        let shrinking_window = LayoutWindowId::from_bytes([u8::MAX; 16]);
        let mut state = PersistedState::default();
        state.store_revision = 1;
        state.overlays.push(
            MixedDomainLayoutOverlay::new(growing_window, "default", 1, Vec::new(), None)
                .expect("empty growing overlay"),
        );
        let shrinking_slots = local_slots(0xf0, MAX_TABS_PER_OVERLAY);
        state.overlays.push(
            MixedDomainLayoutOverlay::new(
                shrinking_window,
                "default",
                1,
                shrinking_slots.clone(),
                shrinking_slots.first().copied(),
            )
            .expect("full shrinking overlay"),
        );
        for number in 1..=3 {
            let slots = local_slots(
                u8::try_from(number).expect("test window number fits u8"),
                MAX_TABS_PER_OVERLAY,
            );
            state.overlays.push(
                MixedDomainLayoutOverlay::new(
                    window_id(number),
                    "default",
                    1,
                    slots.clone(),
                    slots.first().copied(),
                )
                .expect("full stable overlay"),
            );
        }
        canonicalize_state(&mut state);
        validate_state(&state).expect("authority is exactly at the aggregate tab cap");
        let byte_limit = EncodedStateBudget::from_state(&state, 2)
            .and_then(|budget| budget.upper_bound())
            .expect("count unchanged next generation");

        let growth_slot = local_slot(0x7a);
        let mut reduced_slots = shrinking_slots;
        reduced_slots.pop();
        let mut batch = PendingBatch::default();
        batch
            .queue_overlay_live(
                Some(1),
                MixedDomainLayoutOverlay::new(
                    growing_window,
                    "default",
                    2,
                    vec![growth_slot],
                    Some(growth_slot),
                )
                .expect("one-tab growth"),
            )
            .expect("queue growth");
        batch
            .queue_overlay_live(
                Some(1),
                MixedDomainLayoutOverlay::new(
                    shrinking_window,
                    "x".repeat(MAX_WORKSPACE_BYTES),
                    2,
                    reduced_slots.clone(),
                    reduced_slots.first().copied(),
                )
                .expect("byte-growing one-tab shrink"),
            )
            .expect("queue shrink");

        let preflight = preflight_batch_with_byte_limit(&state, &batch, false, byte_limit)
            .expect("partition mutually incompatible quota candidates");
        assert!(preflight.overlays.apply_overlay_ids.is_empty());
        assert_eq!(
            preflight.overlays.rejected_overlay_mutations[&growing_window]
                .failure
                .code(),
            PersistenceFailureCode::Quota
        );
        assert_eq!(
            preflight.overlays.rejected_overlay_mutations[&shrinking_window]
                .failure
                .code(),
            PersistenceFailureCode::Oversized
        );
    }

    #[test]
    fn conservative_binding_width_does_not_block_a_real_byte_reduction() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let workspace = "reduce".to_string();
        let mut state = PersistedState::default();
        state.store_revision = 1;
        state
            .window_states
            .insert(workspace.clone(), PersistedWindowState::default());
        state.domain_bindings.push(DomainBindingRecord {
            target_fingerprint: PrivacySafeTargetFingerprint::from_bytes([0; 32]),
            binding_id: indexed_binding_id(0),
        });
        canonicalize_state(&mut state);
        let reduced_state = PersistedWindowState {
            maximized: false,
            fullscreen: true,
        };
        let mut projected = state.clone();
        projected.store_revision = 2;
        projected
            .window_states
            .insert(workspace.clone(), reduced_state);
        let physical_limit = EncodedStateBudget::from_state_with_physical_bindings(&projected, 2)
            .and_then(|budget| budget.upper_bound())
            .expect("count safe physical upper bound");
        std::fs::write(
            &path,
            encode_disk_slot(&state).expect("encode initial conservative-debt state"),
        )
        .expect("write initial conservative-debt state");

        let mut batch = PendingBatch::default();
        batch
            .queue_window_state(workspace.clone(), reduced_state)
            .expect("queue byte-reducing workspace update");
        let preflight = preflight_batch_with_byte_limit(&state, &batch, false, physical_limit)
            .expect("conservative overage must retain a real reduction");
        assert!(preflight.accepted_workspaces.contains(&workspace));
        assert!(preflight.encoded_upper_bound > physical_limit);
        let committed =
            commit_batch_with_byte_limit(&path, &batch, WriteInterruption::None, physical_limit)
                .expect("physical upper bound permits the reducing commit");
        assert_eq!(committed.receipt.committed_updates, 1);
        let loaded = load_snapshot_at(&path).expect("load reducing commit");
        assert_eq!(loaded.window_states.get(&workspace), Some(&reduced_state));
    }

    #[test]
    fn insufficient_reduction_from_conservative_physical_debt_is_semantic_rejection() {
        let workspace = "reduce".to_string();
        let mut state = PersistedState::default();
        state.store_revision = 9;
        state
            .window_states
            .insert(workspace.clone(), PersistedWindowState::default());
        state.domain_bindings.push(DomainBindingRecord {
            target_fingerprint: PrivacySafeTargetFingerprint::from_bytes([0; 32]),
            binding_id: indexed_binding_id(0),
        });
        canonicalize_state(&mut state);

        let reduced_state = PersistedWindowState {
            maximized: false,
            fullscreen: true,
        };
        let mut projected = state.clone();
        projected.store_revision = 10;
        projected
            .window_states
            .insert(workspace.clone(), reduced_state);
        let projected_physical =
            EncodedStateBudget::from_state_with_physical_bindings(&projected, 10)
                .and_then(|budget| budget.upper_bound())
                .expect("count projected physical upper bound");
        let maximum_bytes = projected_physical
            .checked_sub(1)
            .expect("physical upper bound is nonzero");
        let mut next_base = state.clone();
        next_base.store_revision = 10;
        let next_base_actual = u64::try_from(
            encode_disk_slot(&next_base)
                .expect("encode next-generation baseline")
                .len(),
        )
        .expect("encoded baseline length fits u64");
        assert!(next_base_actual <= maximum_bytes);

        let mut batch = PendingBatch::default();
        batch
            .queue_window_state(workspace.clone(), reduced_state)
            .expect("queue insufficient reduction");
        let preflight = preflight_batch_with_byte_limit(&state, &batch, false, maximum_bytes)
            .expect("conservative debt must not become transaction corruption");
        assert!(!preflight.accepted_workspaces.contains(&workspace));
        assert_eq!(
            preflight.rejected_workspaces[&workspace].code(),
            PersistenceFailureCode::Oversized
        );
    }

    #[test]
    fn admission_projection_apply_then_revert_is_identity_for_every_mutation_kind() {
        let window = window_id(0x4a);
        let old_slot = local_slot(0x4a);
        let old_overlay =
            MixedDomainLayoutOverlay::new(window, "old", 1, vec![old_slot], Some(old_slot))
                .expect("old overlay");
        let mut state = PersistedState::default();
        state.store_revision = 1;
        state
            .window_states
            .insert("old".to_string(), PersistedWindowState::default());
        state.domain_bindings.push(DomainBindingRecord {
            target_fingerprint: PrivacySafeTargetFingerprint::from_bytes([0; 32]),
            binding_id: indexed_binding_id(0),
        });
        state.overlays.push(old_overlay.clone());
        canonicalize_state(&mut state);

        let base = AdmissionProjection {
            normalized_bytes: EncodedStateBudget::from_state(&state, 2).expect("normalized base"),
            physical_bytes: EncodedStateBudget::from_state_with_physical_bindings(&state, 2)
                .expect("physical base"),
            counts: AdmissionCountBudget::from_state(&state).expect("count base"),
        };
        let replacement_state = PersistedWindowState {
            maximized: true,
            fullscreen: true,
        };
        let replacement_slots = vec![local_slot(0x4b), local_slot(0x4c)];
        let replacement_overlay = MixedDomainLayoutOverlay::new(
            window,
            "new",
            2,
            replacement_slots.clone(),
            replacement_slots.first().copied(),
        )
        .expect("replacement overlay");
        let tombstone = OverlayTombstone::new(window, 1, 2).expect("delete tombstone");
        let candidates = [
            ByteAdmissionCandidate {
                key: ByteAdmissionKey::Workspace("old".to_string()),
                admission_rank: 2,
                mutation: ByteBudgetMutation::Workspace {
                    old_entry_bytes: Some(
                        window_state_entry_len("old", &PersistedWindowState::default())
                            .expect("old workspace bytes"),
                    ),
                    new_entry_bytes: window_state_entry_len("old", &replacement_state)
                        .expect("replacement workspace bytes"),
                },
            },
            ByteAdmissionCandidate {
                key: ByteAdmissionKey::Workspace("inserted".to_string()),
                admission_rank: 2,
                mutation: ByteBudgetMutation::Workspace {
                    old_entry_bytes: None,
                    new_entry_bytes: window_state_entry_len("inserted", &replacement_state)
                        .expect("inserted workspace bytes"),
                },
            },
            ByteAdmissionCandidate {
                key: ByteAdmissionKey::Binding(PrivacySafeTargetFingerprint::from_bytes([1; 32])),
                admission_rank: 2,
                mutation: ByteBudgetMutation::Binding {
                    new_record_bytes: maximum_width_binding_len(
                        PrivacySafeTargetFingerprint::from_bytes([1; 32]),
                    )
                    .expect("new binding bytes"),
                },
            },
            ByteAdmissionCandidate {
                key: ByteAdmissionKey::Overlay(window),
                admission_rank: 2,
                mutation: ByteBudgetMutation::OverlayComponent {
                    window_ids: vec![window],
                    mutations: vec![OverlayBudgetMutation {
                        old_overlay_bytes: Some(
                            encoded_json_len(&old_overlay).expect("old overlay bytes"),
                        ),
                        new_overlay_bytes: Some(
                            encoded_json_len(&replacement_overlay)
                                .expect("replacement overlay bytes"),
                        ),
                        new_tombstone_bytes: None,
                        old_tab_count: 1,
                        new_tab_count: 2,
                        old_is_live: true,
                        new_is_live: true,
                        adds_tombstone: false,
                    }],
                },
            },
            ByteAdmissionCandidate {
                key: ByteAdmissionKey::Overlay(window),
                admission_rank: 1,
                mutation: ByteBudgetMutation::OverlayComponent {
                    window_ids: vec![window],
                    mutations: vec![OverlayBudgetMutation {
                        old_overlay_bytes: Some(
                            encoded_json_len(&old_overlay).expect("deleted overlay bytes"),
                        ),
                        new_overlay_bytes: None,
                        new_tombstone_bytes: Some(
                            encoded_json_len(&tombstone).expect("tombstone bytes"),
                        ),
                        old_tab_count: 1,
                        new_tab_count: 0,
                        old_is_live: true,
                        new_is_live: false,
                        adds_tombstone: true,
                    }],
                },
            },
        ];

        for candidate in &candidates {
            let mut projected = base;
            projected.apply(candidate).expect("apply candidate");
            projected.revert(candidate).expect("revert candidate");
            assert_eq!(projected, base, "failed for {:?}", candidate.key);
        }
    }

    #[test]
    fn selector_backfill_reaches_inclusion_maximal_fixed_point() {
        let byte_budget = EncodedStateBudget {
            empty_slot_bytes: 53,
            window_states: JsonCollectionBudget::default(),
            domain_bindings: JsonCollectionBudget::default(),
            overlays: JsonCollectionBudget {
                item_bytes: 40,
                item_count: 4,
            },
            tombstones: JsonCollectionBudget::default(),
        };
        let base = AdmissionProjection {
            normalized_bytes: byte_budget,
            physical_bytes: byte_budget,
            counts: AdmissionCountBudget {
                workspaces: 0,
                bindings: 0,
                live_overlays: 4,
                tombstones: 0,
                tabs: MAX_TOTAL_OVERLAY_TABS,
            },
        };
        let replacement =
            |number, old_bytes, new_bytes, old_tabs, new_tabs, rank| ByteAdmissionCandidate {
                key: ByteAdmissionKey::Overlay(window_id(number)),
                admission_rank: rank,
                mutation: ByteBudgetMutation::OverlayComponent {
                    window_ids: vec![window_id(number)],
                    mutations: vec![OverlayBudgetMutation {
                        old_overlay_bytes: Some(old_bytes),
                        new_overlay_bytes: Some(new_bytes),
                        new_tombstone_bytes: None,
                        old_tab_count: old_tabs,
                        new_tab_count: new_tabs,
                        old_is_live: true,
                        new_is_live: true,
                        adds_tombstone: false,
                    }],
                },
            };
        let candidates = [
            replacement(0x81, 10, 13, 2, 0, 2),
            replacement(0x82, 10, 22, 3, 0, 2),
            replacement(0x83, 10, 25, 0, 0, 2),
            replacement(0x84, 10, 5, 0, 2, 0),
        ];

        let CandidateSubsetSelection {
            projection: selected,
            rejected,
            accepted_count,
            ..
        } = select_compatible_candidate_subset(base, &candidates, &[0, 1, 2, 3], 100, false)
            .expect("select an inclusion-maximal fixed point");

        assert_eq!(accepted_count, 2);
        assert_eq!(rejected, vec![1, 2]);
        assert_eq!(
            selected
                .normalized_bytes
                .upper_bound()
                .expect("selected bytes"),
            94
        );
        assert_eq!(selected.counts.tabs, MAX_TOTAL_OVERLAY_TABS);
        for index in rejected {
            let mut plus_rejected = selected;
            plus_rejected
                .apply(&candidates[index])
                .expect("apply rejected candidate for invariant check");
            assert_ne!(
                admission_violation_mask(base, plus_rejected, 100, true)
                    .expect("classify rejected candidate"),
                0,
                "candidate {index} remained individually admissible"
            );
        }
    }

    fn mixed_sign_unlock_point(candidate_index: usize, candidate_count: usize) -> BackfillPoint {
        let distance = candidate_count
            .checked_sub(candidate_index)
            .and_then(|distance| distance.checked_sub(1))
            .expect("candidate index belongs to the unlock chain");
        let distance = i128::try_from(distance).expect("test distance fits i128");
        let (live_overlay_delta, tab_delta) = if distance == 0 {
            (-1, 0)
        } else if distance % 2 == 1 {
            (distance, -(distance + 1))
        } else {
            (-(distance + 1), distance)
        };
        BackfillPoint {
            candidate_index,
            live_overlay_delta,
            tab_delta,
            normalized_byte_delta: 0,
        }
    }

    #[test]
    fn dominance_index_replaces_quadratic_mixed_sign_rescans_at_4096() {
        const CANDIDATES: usize = 4_096;
        let points = (0..CANDIDATES)
            .map(|index| mixed_sign_unlock_point(index, CANDIDATES))
            .collect::<Vec<_>>();

        let legacy_started = Instant::now();
        let mut legacy_live_slack = 0i128;
        let mut legacy_tab_slack = 0i128;
        let mut legacy_trials = 0usize;
        let mut legacy_rejections_since_progress = 0usize;
        let mut legacy = std::collections::VecDeque::from_iter(points.iter().copied());
        while let Some(point) = legacy.pop_front() {
            legacy_trials = legacy_trials.checked_add(1).expect("trial count fits");
            if point.live_overlay_delta <= legacy_live_slack && point.tab_delta <= legacy_tab_slack
            {
                legacy_live_slack -= point.live_overlay_delta;
                legacy_tab_slack -= point.tab_delta;
                legacy_rejections_since_progress = 0;
            } else {
                legacy.push_back(point);
                legacy_rejections_since_progress = legacy_rejections_since_progress
                    .checked_add(1)
                    .expect("rejection count fits");
                assert_ne!(
                    legacy_rejections_since_progress,
                    legacy.len(),
                    "unlock chain must always make deterministic progress"
                );
            }
        }
        let legacy_elapsed = legacy_started.elapsed();

        let dominance_started = Instant::now();
        let mut dominance = BackfillDominanceIndex::build(points, CANDIDATES)
            .expect("build bounded dominance index");
        let index_entries = dominance.entry_count;
        let mut dominance_live_slack = 0i128;
        let mut dominance_tab_slack = 0i128;
        let mut dominance_queries = 0usize;
        while dominance_queries != CANDIDATES {
            let candidate_index = dominance
                .query(dominance_live_slack, dominance_tab_slack, 0)
                .expect("one chain candidate is admissible");
            let point = mixed_sign_unlock_point(candidate_index, CANDIDATES);
            dominance
                .remove(candidate_index)
                .expect("remove selected dominance point once");
            dominance_live_slack -= point.live_overlay_delta;
            dominance_tab_slack -= point.tab_delta;
            dominance_queries += 1;
        }
        assert_eq!(dominance.query(i128::MAX, i128::MAX, 0), None);
        let dominance_elapsed = dominance_started.elapsed();

        let expected_legacy_trials = CANDIDATES
            .checked_mul(CANDIDATES + 1)
            .and_then(|trials| trials.checked_div(2))
            .expect("legacy triangular trial count fits");
        let tree_levels = usize::try_from(CANDIDATES.next_power_of_two().ilog2())
            .expect("tree level count fits usize")
            + 1;
        assert_eq!(legacy_trials, expected_legacy_trials);
        assert_eq!(dominance_queries, CANDIDATES);
        assert!(index_entries <= CANDIDATES * tree_levels);
        eprintln!(
            "window_state byte-admission 4096-lineage comparison: legacy_trials={legacy_trials} dominance_queries={dominance_queries} index_entries={index_entries} legacy_elapsed={legacy_elapsed:?} dominance_elapsed={dominance_elapsed:?}"
        );
    }

    fn mixed_sign_unlock_candidate(
        candidate_index: usize,
        candidate_count: usize,
    ) -> ByteAdmissionCandidate {
        let point = mixed_sign_unlock_point(candidate_index, candidate_count);
        let mut mutations = Vec::new();
        if point.live_overlay_delta > 0 {
            for _ in 0..usize::try_from(point.live_overlay_delta)
                .expect("positive live delta fits usize")
            {
                mutations.push(OverlayBudgetMutation {
                    old_overlay_bytes: None,
                    new_overlay_bytes: Some(10),
                    new_tombstone_bytes: None,
                    old_tab_count: 0,
                    new_tab_count: 0,
                    old_is_live: false,
                    new_is_live: true,
                    adds_tombstone: false,
                });
            }
        } else {
            for _ in 0..usize::try_from(-point.live_overlay_delta)
                .expect("negative live delta magnitude fits usize")
            {
                mutations.push(OverlayBudgetMutation {
                    old_overlay_bytes: Some(10),
                    new_overlay_bytes: None,
                    new_tombstone_bytes: Some(10),
                    old_tab_count: 0,
                    new_tab_count: 0,
                    old_is_live: true,
                    new_is_live: false,
                    adds_tombstone: true,
                });
            }
        }
        if point.tab_delta != 0 {
            let (old_tab_count, new_tab_count) = if point.tab_delta > 0 {
                (
                    0,
                    usize::try_from(point.tab_delta).expect("positive tab delta fits usize"),
                )
            } else {
                (
                    usize::try_from(-point.tab_delta)
                        .expect("negative tab delta magnitude fits usize"),
                    0,
                )
            };
            mutations.push(OverlayBudgetMutation {
                old_overlay_bytes: Some(10),
                new_overlay_bytes: Some(10),
                new_tombstone_bytes: None,
                old_tab_count,
                new_tab_count,
                old_is_live: true,
                new_is_live: true,
                adds_tombstone: false,
            });
        }
        ByteAdmissionCandidate {
            key: ByteAdmissionKey::Overlay(window_id(
                u64::try_from(candidate_index).expect("candidate index fits u64"),
            )),
            admission_rank: if point.live_overlay_delta < 0 { 1 } else { 2 },
            mutation: ByteBudgetMutation::OverlayComponent {
                window_ids: vec![window_id(
                    u64::try_from(candidate_index).expect("candidate index fits u64"),
                )],
                mutations,
            },
        }
    }

    #[test]
    fn exact_backfill_trials_each_adversarial_lineage_once() {
        const CANDIDATES: usize = 64;
        let byte_budget = EncodedStateBudget {
            empty_slot_bytes: 100,
            window_states: JsonCollectionBudget::default(),
            domain_bindings: JsonCollectionBudget::default(),
            overlays: JsonCollectionBudget {
                item_bytes: u64::try_from(MAX_LAYOUT_OVERLAYS).expect("overlay cap fits u64") * 10,
                item_count: MAX_LAYOUT_OVERLAYS,
            },
            tombstones: JsonCollectionBudget {
                item_bytes: 10,
                item_count: 1,
            },
        };
        let base = AdmissionProjection {
            normalized_bytes: byte_budget,
            physical_bytes: byte_budget,
            counts: AdmissionCountBudget {
                workspaces: 0,
                bindings: 0,
                live_overlays: MAX_LAYOUT_OVERLAYS,
                tombstones: 1,
                tabs: MAX_TOTAL_OVERLAY_TABS,
            },
        };
        let candidates = (0..CANDIDATES)
            .map(|index| mixed_sign_unlock_candidate(index, CANDIDATES))
            .collect::<Vec<_>>();
        let mut aggregate = base;
        let mut remaining = vec![false; CANDIDATES];
        let mut pending_backfill = vec![true; CANDIDATES];
        let mut remaining_count = 0usize;
        let mut stats = ByteAdmissionStats {
            candidate_count: CANDIDATES,
            ..ByteAdmissionStats::default()
        };

        restore_inclusion_maximal_subset(
            base,
            &candidates,
            &mut aggregate,
            &mut remaining,
            &mut pending_backfill,
            &mut remaining_count,
            u64::MAX,
            &mut stats,
        )
        .expect("restore the exact mixed-sign unlock chain");

        assert_eq!(remaining_count, CANDIDATES);
        assert!(remaining.into_iter().all(|accepted| accepted));
        assert!(pending_backfill.into_iter().all(|pending| !pending));
        assert_eq!(stats.backfill_candidate_trials, CANDIDATES);
        assert_eq!(stats.backfill_queries, CANDIDATES + 1);
        assert_eq!(stats.backfill_index_rebuilds, 1);
        assert_eq!(
            admission_violation_mask(base, aggregate, u64::MAX, true)
                .expect("classify final aggregate"),
            0
        );
    }

    #[test]
    fn selector_handles_separator_relief_hidden_from_base_relative_deltas() {
        let byte_budget = EncodedStateBudget {
            empty_slot_bytes: 100,
            window_states: JsonCollectionBudget::default(),
            domain_bindings: JsonCollectionBudget::default(),
            overlays: JsonCollectionBudget {
                item_bytes: 30,
                item_count: 3,
            },
            tombstones: JsonCollectionBudget::default(),
        };
        let maximum_bytes = byte_budget.upper_bound().expect("base byte limit");
        let base = AdmissionProjection {
            normalized_bytes: byte_budget,
            physical_bytes: byte_budget,
            counts: AdmissionCountBudget {
                workspaces: 0,
                bindings: 0,
                live_overlays: 3,
                tombstones: 0,
                tabs: 0,
            },
        };
        let deletion = |number| ByteAdmissionCandidate {
            key: ByteAdmissionKey::Overlay(window_id(number)),
            admission_rank: 1,
            mutation: ByteBudgetMutation::OverlayComponent {
                window_ids: vec![window_id(number)],
                mutations: vec![OverlayBudgetMutation {
                    old_overlay_bytes: Some(10),
                    new_overlay_bytes: None,
                    new_tombstone_bytes: Some(11),
                    old_tab_count: 0,
                    new_tab_count: 0,
                    old_is_live: true,
                    new_is_live: false,
                    adds_tombstone: true,
                }],
            },
        };
        let candidates = [deletion(0x91), deletion(0x92), deletion(0x93)];
        for candidate in &candidates {
            assert_eq!(
                AdmissionDelta::for_candidate(base, candidate)
                    .expect("compute base-relative delta")
                    .values[5],
                0
            );
        }

        let CandidateSubsetSelection {
            projection: selected,
            rejected,
            accepted_count,
            ..
        } = select_compatible_candidate_subset(base, &candidates, &[0, 1, 2], maximum_bytes, false)
            .expect("reconstruct exact separator-safe subset");

        assert_eq!(accepted_count, 1);
        assert_eq!(rejected.len(), 2);
        assert_eq!(selected.counts.live_overlays, 2);
        assert_eq!(selected.counts.tombstones, 1);
        assert_eq!(
            selected
                .normalized_bytes
                .upper_bound()
                .expect("selected bytes"),
            maximum_bytes
        );
        for index in rejected {
            let mut plus_rejected = selected;
            plus_rejected
                .apply(&candidates[index])
                .expect("apply rejected separator candidate");
            assert_ne!(
                admission_violation_mask(base, plus_rejected, maximum_bytes, true)
                    .expect("classify separator rejection"),
                0
            );
        }
    }

    #[test]
    fn forced_candidate_free_publication_checks_physical_byte_limit() {
        let byte_budget = EncodedStateBudget {
            empty_slot_bytes: 101,
            window_states: JsonCollectionBudget::default(),
            domain_bindings: JsonCollectionBudget::default(),
            overlays: JsonCollectionBudget::default(),
            tombstones: JsonCollectionBudget::default(),
        };
        let base = AdmissionProjection {
            normalized_bytes: byte_budget,
            physical_bytes: byte_budget,
            counts: AdmissionCountBudget {
                workspaces: 0,
                bindings: 0,
                live_overlays: 0,
                tombstones: 0,
                tabs: 0,
            },
        };

        let unchanged = select_compatible_candidate_subset(base, &[], &[], 100, false)
            .expect("candidate-free no-write base remains admissible");
        assert_eq!(unchanged.projection, base);

        let failure = select_compatible_candidate_subset(base, &[], &[], 100, true)
            .expect_err("forced publication must prove its physical byte bound");
        assert_eq!(failure.code(), PersistenceFailureCode::Oversized);
    }

    #[test]
    fn selector_preserves_active_resource_suppliers_when_peeling_unrelated_mixed_poison() {
        let byte_budget = EncodedStateBudget {
            empty_slot_bytes: 100,
            window_states: JsonCollectionBudget::default(),
            domain_bindings: JsonCollectionBudget::default(),
            overlays: JsonCollectionBudget {
                item_bytes: 300,
                item_count: 3,
            },
            tombstones: JsonCollectionBudget::default(),
        };
        let maximum_bytes = byte_budget.upper_bound().expect("base byte limit");
        let base = AdmissionProjection {
            normalized_bytes: byte_budget,
            physical_bytes: byte_budget,
            counts: AdmissionCountBudget {
                workspaces: 0,
                bindings: 0,
                live_overlays: MAX_LAYOUT_OVERLAYS,
                tombstones: 0,
                tabs: MAX_TOTAL_OVERLAY_TABS,
            },
        };
        let candidates = [
            ByteAdmissionCandidate {
                key: ByteAdmissionKey::Overlay(window_id(0x51)),
                admission_rank: 0,
                mutation: ByteBudgetMutation::OverlayComponent {
                    window_ids: vec![window_id(0x51)],
                    mutations: vec![OverlayBudgetMutation {
                        old_overlay_bytes: Some(100),
                        new_overlay_bytes: Some(20),
                        new_tombstone_bytes: None,
                        old_tab_count: 0,
                        new_tab_count: 1,
                        old_is_live: true,
                        new_is_live: true,
                        adds_tombstone: false,
                    }],
                },
            },
            ByteAdmissionCandidate {
                key: ByteAdmissionKey::Overlay(window_id(0x52)),
                admission_rank: 2,
                mutation: ByteBudgetMutation::OverlayComponent {
                    window_ids: vec![window_id(0x52)],
                    mutations: vec![OverlayBudgetMutation {
                        old_overlay_bytes: Some(20),
                        new_overlay_bytes: Some(90),
                        new_tombstone_bytes: None,
                        old_tab_count: 1,
                        new_tab_count: 0,
                        old_is_live: true,
                        new_is_live: true,
                        adds_tombstone: false,
                    }],
                },
            },
            ByteAdmissionCandidate {
                key: ByteAdmissionKey::Overlay(window_id(0x53)),
                admission_rank: 1,
                mutation: ByteBudgetMutation::OverlayComponent {
                    window_ids: vec![window_id(0x53), window_id(0x54)],
                    mutations: vec![
                        OverlayBudgetMutation {
                            old_overlay_bytes: Some(50),
                            new_overlay_bytes: None,
                            new_tombstone_bytes: Some(30),
                            old_tab_count: 0,
                            new_tab_count: 0,
                            old_is_live: true,
                            new_is_live: false,
                            adds_tombstone: true,
                        },
                        OverlayBudgetMutation {
                            old_overlay_bytes: Some(50),
                            new_overlay_bytes: Some(90),
                            new_tombstone_bytes: None,
                            old_tab_count: 0,
                            new_tab_count: 1,
                            old_is_live: true,
                            new_is_live: true,
                            adds_tombstone: false,
                        },
                    ],
                },
            },
        ];

        let CandidateSubsetSelection {
            projection: selected,
            rejected,
            accepted_count,
            ..
        } = select_compatible_candidate_subset(base, &candidates, &[0, 1, 2], maximum_bytes, false)
            .expect("select mutually enabling exchange");
        assert_eq!(rejected, vec![2]);
        assert_eq!(accepted_count, 2);
        assert_eq!(selected.counts.tabs, MAX_TOTAL_OVERLAY_TABS);
        assert_eq!(selected.counts.live_overlays, MAX_LAYOUT_OVERLAYS);
        assert!(
            selected
                .normalized_bytes
                .upper_bound()
                .expect("selected bytes")
                <= maximum_bytes
        );
    }

    #[test]
    fn selector_preserves_larger_inactive_supplier_for_a_balanced_exchange() {
        let byte_budget = EncodedStateBudget {
            empty_slot_bytes: 100,
            window_states: JsonCollectionBudget::default(),
            domain_bindings: JsonCollectionBudget::default(),
            overlays: JsonCollectionBudget {
                item_bytes: 100,
                item_count: 3,
            },
            tombstones: JsonCollectionBudget::default(),
        };
        let maximum_bytes = byte_budget.upper_bound().expect("base byte limit");
        let base = AdmissionProjection {
            normalized_bytes: byte_budget,
            physical_bytes: byte_budget,
            counts: AdmissionCountBudget {
                workspaces: 0,
                bindings: 0,
                live_overlays: 3,
                tombstones: 0,
                tabs: MAX_TOTAL_OVERLAY_TABS,
            },
        };
        let replacement =
            |number, old_bytes, new_bytes, old_tabs, new_tabs, rank| ByteAdmissionCandidate {
                key: ByteAdmissionKey::Overlay(window_id(number)),
                admission_rank: rank,
                mutation: ByteBudgetMutation::OverlayComponent {
                    window_ids: vec![window_id(number)],
                    mutations: vec![OverlayBudgetMutation {
                        old_overlay_bytes: Some(old_bytes),
                        new_overlay_bytes: Some(new_bytes),
                        new_tombstone_bytes: None,
                        old_tab_count: old_tabs,
                        new_tab_count: new_tabs,
                        old_is_live: true,
                        new_is_live: true,
                        adds_tombstone: false,
                    }],
                },
            };
        let candidates = [
            replacement(0x61, 20, 30, 10, 0, 2),
            replacement(0x62, 30, 20, 0, 10, 0),
            replacement(0x63, 40, 41, 1, 0, 2),
        ];

        let CandidateSubsetSelection {
            projection: selected,
            rejected,
            accepted_count,
            ..
        } = select_compatible_candidate_subset(base, &candidates, &[0, 1, 2], maximum_bytes, false)
            .expect("select balanced exchange over smaller inactive supplier");
        assert_eq!(rejected, vec![2]);
        assert_eq!(accepted_count, 2);
        assert_eq!(selected.counts.tabs, MAX_TOTAL_OVERLAY_TABS);
        assert_eq!(
            selected
                .normalized_bytes
                .upper_bound()
                .expect("selected bytes"),
            maximum_bytes
        );
    }

    #[test]
    fn selector_exactly_isolates_one_expensive_mixed_sign_lineage() {
        let byte_budget = EncodedStateBudget {
            empty_slot_bytes: 100,
            window_states: JsonCollectionBudget::default(),
            domain_bindings: JsonCollectionBudget::default(),
            overlays: JsonCollectionBudget {
                item_bytes: 1_400,
                item_count: 4,
            },
            tombstones: JsonCollectionBudget::default(),
        };
        let maximum_bytes = byte_budget.upper_bound().expect("base byte limit");
        let base = AdmissionProjection {
            normalized_bytes: byte_budget,
            physical_bytes: byte_budget,
            counts: AdmissionCountBudget {
                workspaces: 0,
                bindings: 0,
                live_overlays: 4,
                tombstones: 0,
                tabs: MAX_TOTAL_OVERLAY_TABS,
            },
        };
        let candidates = [
            ByteAdmissionCandidate {
                key: ByteAdmissionKey::Overlay(window_id(0x71)),
                admission_rank: 0,
                mutation: ByteBudgetMutation::OverlayComponent {
                    window_ids: vec![window_id(0x71)],
                    mutations: vec![OverlayBudgetMutation {
                        old_overlay_bytes: Some(1_100),
                        new_overlay_bytes: Some(100),
                        new_tombstone_bytes: None,
                        old_tab_count: 0,
                        new_tab_count: 1,
                        old_is_live: true,
                        new_is_live: true,
                        adds_tombstone: false,
                    }],
                },
            },
            ByteAdmissionCandidate {
                key: ByteAdmissionKey::Overlay(window_id(0x72)),
                admission_rank: 1,
                mutation: ByteBudgetMutation::OverlayComponent {
                    window_ids: vec![window_id(0x72), window_id(0x73)],
                    mutations: vec![
                        OverlayBudgetMutation {
                            old_overlay_bytes: Some(100),
                            new_overlay_bytes: None,
                            new_tombstone_bytes: Some(100),
                            old_tab_count: 2,
                            new_tab_count: 0,
                            old_is_live: true,
                            new_is_live: false,
                            adds_tombstone: true,
                        },
                        OverlayBudgetMutation {
                            old_overlay_bytes: Some(100),
                            new_overlay_bytes: Some(1_200),
                            new_tombstone_bytes: None,
                            old_tab_count: 0,
                            new_tab_count: 0,
                            old_is_live: true,
                            new_is_live: true,
                            adds_tombstone: false,
                        },
                    ],
                },
            },
            ByteAdmissionCandidate {
                key: ByteAdmissionKey::Overlay(window_id(0x74)),
                admission_rank: 2,
                mutation: ByteBudgetMutation::OverlayComponent {
                    window_ids: vec![window_id(0x74)],
                    mutations: vec![OverlayBudgetMutation {
                        old_overlay_bytes: Some(100),
                        new_overlay_bytes: Some(1_000),
                        new_tombstone_bytes: None,
                        old_tab_count: 1,
                        new_tab_count: 0,
                        old_is_live: true,
                        new_is_live: true,
                        adds_tombstone: false,
                    }],
                },
            },
        ];

        let CandidateSubsetSelection {
            projection: selected,
            rejected,
            accepted_count,
            ..
        } = select_compatible_candidate_subset(base, &candidates, &[0, 1, 2], maximum_bytes, false)
            .expect("isolate one mixed-sign poison exactly");
        assert_eq!(rejected, vec![1]);
        assert_eq!(accepted_count, 2);
        assert_eq!(selected.counts.tabs, MAX_TOTAL_OVERLAY_TABS);
        assert_eq!(selected.counts.live_overlays, 4);
        assert_eq!(selected.counts.tombstones, 0);
        assert!(
            selected
                .normalized_bytes
                .upper_bound()
                .expect("selected bytes")
                <= maximum_bytes
        );
    }

    #[test]
    fn higher_id_delete_makes_room_for_lower_id_empty_create_at_live_cap() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let create_window = LayoutWindowId::from_bytes([0; 16]);
        let retiring_window = LayoutWindowId::from_bytes([0xff; 16]);
        let mut state = PersistedState::default();
        state.store_revision = 1;
        state.overlays = (1..MAX_LAYOUT_OVERLAYS)
            .map(|index| {
                MixedDomainLayoutOverlay::new(
                    window_id(u64::try_from(index).expect("overlay index fits u64")),
                    "default",
                    1,
                    Vec::new(),
                    None,
                )
                .expect("valid empty overlay")
            })
            .collect();
        state.overlays.push(
            MixedDomainLayoutOverlay::new(retiring_window, "default", 1, Vec::new(), None)
                .expect("valid retiring overlay"),
        );
        canonicalize_state(&mut state);
        std::fs::write(
            &path,
            encode_disk_slot(&state).expect("encode full live state"),
        )
        .expect("write full live state");

        let mut batch = PendingBatch::default();
        batch
            .queue_overlay_live(
                None,
                MixedDomainLayoutOverlay::new(create_window, "default", 1, Vec::new(), None)
                    .expect("valid empty create"),
            )
            .expect("queue lexically earlier create");
        batch
            .queue_overlay_delete(retiring_window, Some(1))
            .expect("queue lexically later delete");

        let committed = commit_for_test(&path, &batch, WriteInterruption::None)
            .expect("delete must free live capacity before create admission");
        assert_eq!(committed.receipt.rejected_updates, 0);
        let snapshot = load_snapshot_at(&path).expect("load swapped live identity");
        assert_eq!(snapshot.overlays.len(), MAX_LAYOUT_OVERLAYS);
        assert!(snapshot.overlay(create_window).is_some());
        assert!(snapshot.overlay(retiring_window).is_none());
        assert!(snapshot.tombstone(retiring_window).is_some());
    }

    #[test]
    fn higher_id_shrink_makes_room_for_lower_id_growth_at_tab_cap() {
        let growing_window = LayoutWindowId::from_bytes([0; 16]);
        let shrinking_window = LayoutWindowId::from_bytes([0xff; 16]);
        let mut state = PersistedState::default();
        state.store_revision = 1;
        state.overlays.push(
            MixedDomainLayoutOverlay::new(growing_window, "default", 1, Vec::new(), None)
                .expect("empty growing overlay"),
        );
        let full_slots = local_slots(0xf0, MAX_TABS_PER_OVERLAY);
        state.overlays.push(
            MixedDomainLayoutOverlay::new(
                shrinking_window,
                "default",
                1,
                full_slots.clone(),
                full_slots.first().copied(),
            )
            .expect("full shrinking overlay"),
        );
        for number in 1..=3 {
            let slots = local_slots(
                u8::try_from(number).expect("test window number fits u8"),
                MAX_TABS_PER_OVERLAY,
            );
            state.overlays.push(
                MixedDomainLayoutOverlay::new(
                    window_id(number),
                    "default",
                    1,
                    slots.clone(),
                    slots.first().copied(),
                )
                .expect("full stable overlay"),
            );
        }
        validate_state(&state).expect("state exactly at aggregate tab cap");

        let mut batch = PendingBatch::default();
        batch
            .queue_overlay_live(Some(1), local_overlay(growing_window, 2, 90))
            .expect("queue lexically earlier growth");
        let mut reduced_slots = full_slots;
        reduced_slots.pop();
        batch
            .queue_overlay_live(
                Some(1),
                MixedDomainLayoutOverlay::new(
                    shrinking_window,
                    "default",
                    2,
                    reduced_slots.clone(),
                    reduced_slots.first().copied(),
                )
                .expect("one-tab shrink"),
            )
            .expect("queue lexically later shrink");

        let preflight = preflight_overlay_mutations(&state, &batch)
            .expect("shrink must free tab capacity before growth admission");
        assert_eq!(
            preflight.accepted_overlay_ids,
            BTreeSet::from([growing_window, shrinking_window])
        );
        assert!(preflight.rejected_overlay_mutations.is_empty());
    }

    #[test]
    fn tombstone_cap_rejects_only_retirement_and_preserves_unrelated_work() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let mut state = PersistedState::default();
        state.store_revision = 5_000;
        state.tombstones = (1..=MAX_OVERLAY_TOMBSTONES)
            .map(|index| {
                OverlayTombstone::new(
                    window_id(u64::try_from(index).expect("tombstone index fits u64")),
                    1,
                    u64::try_from(index).expect("tombstone revision fits u64"),
                )
                .expect("valid capped tombstone")
            })
            .collect();
        let retiring_window = window_id(10_000);
        let updating_window = window_id(10_001);
        state.overlays = vec![
            local_overlay(retiring_window, 1, 70),
            local_overlay(updating_window, 1, 71),
        ];
        canonicalize_state(&mut state);
        std::fs::write(
            &path,
            encode_disk_slot(&state).expect("encode capped state"),
        )
        .expect("write capped state");

        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x72; 32]);
        let mut batch = PendingBatch::default();
        batch
            .queue_overlay_delete(retiring_window, Some(1))
            .expect("queue cap-exceeding retirement");
        batch
            .queue_overlay_live(Some(1), local_overlay(updating_window, 2, 73))
            .expect("queue unrelated overlay update");
        batch
            .queue_window_state(
                "other".to_string(),
                PersistedWindowState {
                    maximized: false,
                    fullscreen: true,
                },
            )
            .expect("queue unrelated geometry");
        batch.ensure_bindings.insert(fingerprint);

        let committed = commit_for_test(&path, &batch, WriteInterruption::None)
            .expect("partition capped retirement");
        assert!(committed.receipt.wrote_new_generation);
        assert_eq!(committed.receipt.rejected_updates, 1);
        assert_eq!(
            committed.rejected_overlay_mutations[&retiring_window]
                .failure
                .code(),
            PersistenceFailureCode::Quota
        );
        assert!(committed.accepted_overlay_ids.contains(&updating_window));
        assert!(committed.bindings.contains_key(&fingerprint));

        let after = load_snapshot_at(&path).expect("load partial success");
        assert_eq!(after.tombstones.len(), MAX_OVERLAY_TOMBSTONES);
        assert_eq!(
            after
                .overlay(retiring_window)
                .expect("rejected retirement remains live")
                .local_revision(),
            1
        );
        assert_eq!(
            after
                .overlay(updating_window)
                .expect("unrelated update committed")
                .local_revision(),
            2
        );
        assert!(after.window_states["other"].fullscreen);
        assert_eq!(
            after.binding_for(fingerprint),
            committed.bindings.get(&fingerprint).copied()
        );

        let replay_window = window_id(1);
        let mut replay = PendingBatch::default();
        replay
            .queue_overlay_delete(replay_window, Some(1))
            .expect("queue capped tombstone replay");
        let replay = commit_for_test(&path, &replay, WriteInterruption::None)
            .expect("existing tombstone replay remains idempotent at cap");
        assert!(!replay.receipt.wrote_new_generation);
        assert_eq!(replay.receipt.rejected_updates, 0);
        assert_eq!(replay.receipt.store_revision, after.store_revision);
    }

    #[test]
    fn cas_rejection_does_not_poison_overlay_geometry_or_binding_commits() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let conflicted_window = window_id(11_000);
        let valid_window = window_id(11_001);
        let mut initial = PendingBatch::default();
        initial
            .queue_overlay_live(None, local_overlay(conflicted_window, 1, 80))
            .expect("queue first overlay");
        initial
            .queue_overlay_live(None, local_overlay(valid_window, 1, 81))
            .expect("queue second overlay");
        commit_for_test(&path, &initial, WriteInterruption::None).expect("commit initial overlays");

        let mut advance_conflicted = PendingBatch::default();
        advance_conflicted
            .queue_overlay_live(Some(1), local_overlay(conflicted_window, 2, 82))
            .expect("queue committed advance");
        commit_for_test(&path, &advance_conflicted, WriteInterruption::None)
            .expect("advance conflicted authority");

        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x83; 32]);
        let mut mixed = PendingBatch::default();
        mixed
            .queue_overlay_live(Some(3), local_overlay(conflicted_window, 4, 84))
            .expect("queue wrong-base update");
        mixed
            .queue_overlay_live(Some(1), local_overlay(valid_window, 2, 85))
            .expect("queue valid update");
        mixed
            .queue_window_state(
                "mixed".to_string(),
                PersistedWindowState {
                    maximized: true,
                    fullscreen: false,
                },
            )
            .expect("queue geometry");
        mixed.ensure_bindings.insert(fingerprint);

        let committed = commit_for_test(&path, &mixed, WriteInterruption::None)
            .expect("partition wrong-base lineage");
        assert_eq!(committed.receipt.rejected_updates, 1);
        assert_eq!(
            committed.rejected_overlay_mutations[&conflicted_window]
                .failure
                .code(),
            PersistenceFailureCode::OverlayCasConflict
        );
        assert!(committed.accepted_overlay_ids.contains(&valid_window));
        assert!(committed.bindings.contains_key(&fingerprint));

        let snapshot = load_snapshot_at(&path).expect("load mixed partial success");
        assert_eq!(
            snapshot
                .overlay(conflicted_window)
                .expect("conflicted overlay unchanged")
                .local_revision(),
            2
        );
        assert_eq!(
            snapshot
                .overlay(valid_window)
                .expect("valid overlay advanced")
                .local_revision(),
            2
        );
        assert!(snapshot.window_states["mixed"].maximized);
        assert!(snapshot.binding_for(fingerprint).is_some());
    }

    #[test]
    fn authority_cap_races_reject_only_new_workspace_and_binding_lineages() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let mut state = PersistedState::default();
        state.store_revision = 1;
        state.window_states = (0..MAX_WORKSPACES)
            .map(|index| {
                (
                    format!("workspace-{index}"),
                    PersistedWindowState::default(),
                )
            })
            .collect();
        state.domain_bindings = capped_domain_bindings();
        canonicalize_state(&mut state);
        validate_state(&state).expect("valid state at independent authority caps");
        std::fs::write(
            &path,
            encode_disk_slot(&state).expect("encode capped authority"),
        )
        .expect("write capped authority");

        let existing_fingerprint = indexed_fingerprint(0);
        let new_fingerprint = PrivacySafeTargetFingerprint::from_bytes([0xff; 32]);
        let valid_window = window_id(11_050);
        let mut batch = PendingBatch::default();
        batch
            .queue_window_state(
                "workspace-0".to_string(),
                PersistedWindowState {
                    maximized: true,
                    fullscreen: false,
                },
            )
            .expect("queue existing workspace update");
        batch
            .queue_window_state(
                "zz-cross-process-overflow".to_string(),
                PersistedWindowState {
                    maximized: false,
                    fullscreen: true,
                },
            )
            .expect("local queue cannot yet observe authority cap");
        batch.ensure_bindings.insert(existing_fingerprint);
        batch.ensure_bindings.insert(new_fingerprint);
        batch
            .queue_overlay_live(None, local_overlay(valid_window, 1, 89))
            .expect("queue unrelated overlay");

        let committed = commit_for_test(&path, &batch, WriteInterruption::None)
            .expect("partition cross-process authority cap races");
        assert_eq!(committed.receipt.committed_updates, 3);
        assert_eq!(committed.receipt.rejected_updates, 2);
        assert_eq!(
            committed.rejected_workspaces["zz-cross-process-overflow"].code(),
            PersistenceFailureCode::Quota
        );
        assert_eq!(
            committed.rejected_bindings[&new_fingerprint].code(),
            PersistenceFailureCode::Quota
        );
        assert_eq!(
            committed.bindings[&existing_fingerprint],
            indexed_binding_id(0)
        );

        let shared = CoordinatorShared {
            primary_path: path.clone(),
            pending: Mutex::new(CoordinatorPending {
                batch: batch.clone(),
                ..CoordinatorPending::default()
            }),
        };
        {
            let mut pending = lock_pending(&shared.pending);
            pending
                .batch
                .queue_window_state(
                    "zz-cross-process-overflow".to_string(),
                    PersistedWindowState {
                        maximized: true,
                        fullscreen: false,
                    },
                )
                .expect("queue post-snapshot workspace successor");
        }
        let _ = acknowledge_committed_batch(&shared, &batch, &committed);
        let pending = lock_pending(&shared.pending);
        assert_eq!(pending.batch.window_states.len(), 1);
        assert!(pending.batch.window_states["zz-cross-process-overflow"].maximized);
        drop(pending);

        let snapshot = load_snapshot_at(&path).expect("load partitioned capped authority");
        assert!(snapshot.window_states["workspace-0"].maximized);
        assert!(
            !snapshot
                .window_states
                .contains_key("zz-cross-process-overflow")
        );
        assert_eq!(snapshot.domain_bindings.len(), MAX_DOMAIN_BINDINGS);
        assert!(snapshot.overlay(valid_window).is_some());
    }

    #[test]
    fn final_workspace_and_binding_capacity_is_admitted_deterministically() {
        let mut state = PersistedState::default();
        state.window_states = (0..MAX_WORKSPACES - 1)
            .map(|index| {
                (
                    format!("workspace-{index}"),
                    PersistedWindowState::default(),
                )
            })
            .collect();
        state.domain_bindings = capped_domain_bindings();
        state.domain_bindings.pop();
        validate_state(&state).expect("valid authority one slot below both caps");

        let first_fingerprint = PrivacySafeTargetFingerprint::from_bytes([0xfe; 32]);
        let second_fingerprint = PrivacySafeTargetFingerprint::from_bytes([0xff; 32]);
        let mut batch = PendingBatch::default();
        batch
            .queue_window_state("zz-first".to_string(), PersistedWindowState::default())
            .expect("queue first workspace candidate");
        batch
            .queue_window_state("zz-second".to_string(), PersistedWindowState::default())
            .expect("queue second workspace candidate");
        batch.ensure_bindings.insert(first_fingerprint);
        batch.ensure_bindings.insert(second_fingerprint);

        let preflight =
            preflight_batch(&state, &batch, false).expect("partition final capacity slots");
        assert!(preflight.accepted_workspaces.contains("zz-first"));
        assert_eq!(
            preflight.rejected_workspaces["zz-second"].code(),
            PersistenceFailureCode::Quota
        );
        assert!(preflight.accepted_bindings.contains(&first_fingerprint));
        assert_eq!(
            preflight.rejected_bindings[&second_fingerprint].code(),
            PersistenceFailureCode::Quota
        );
    }

    #[test]
    fn controlled_worker_retries_frozen_update_without_spin_or_successor_bypass() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(11_055);
        let worker = ControlledPersistenceWorker::open(path.clone());
        worker.expect_waiting(1, false);

        let mut initial = PendingBatch::default();
        initial
            .queue_overlay_live(None, local_overlay(window, 1, 80))
            .expect("queue initial frozen overlay");
        let initial_flush = worker.admit_batch_with_flush(initial);
        worker.continue_wake(1);
        let entered = worker.expect_commit(1, TestWorkerCommitPhase::Pending);
        assert_eq!(entered.overlay_mutations[&window].desired_revision(), 1);
        worker.release_commit(
            1,
            TestWorkerCommitAction::Run(WriteInterruption::AfterDirectorySync),
        );
        assert_eq!(
            worker.expect_commit_finished(1, TestWorkerCommitPhase::Pending),
            TestWorkerCommitResult::Failed(PersistenceFailureCode::Io)
        );

        // This is the true acknowledgement-loss interval: the real journal
        // commit and its lock have completed, but the worker has not yet
        // acknowledged or rejected its frozen snapshot.
        worker
            .writer()
            .queue_overlay(Some(1), local_overlay(window, 2, 81))
            .expect("queue post-commit successor");
        let successor_flush = worker.writer().flush().expect("successor flush");
        worker.continue_after_commit(1);
        assert_eq!(
            initial_flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("initial ambiguous result")
                .expect_err("ambiguous publication is not an acknowledgement")
                .code(),
            PersistenceFailureCode::Io
        );

        worker.expect_waiting(2, true);
        worker.continue_wake(2);
        let first_retry = worker.expect_commit(2, TestWorkerCommitPhase::ExactRetry);
        assert_eq!(first_retry.overlay_mutations[&window].desired_revision(), 1);
        worker.release_commit(
            2,
            TestWorkerCommitAction::Run(WriteInterruption::AfterDirectorySync),
        );
        assert_eq!(
            worker.expect_commit_finished(2, TestWorkerCommitPhase::ExactRetry),
            TestWorkerCommitResult::Failed(PersistenceFailureCode::Io)
        );
        worker.continue_after_commit(2);
        assert_eq!(
            successor_flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("repeated retry result")
                .expect_err("repeated ambiguous retry fails the waiting barrier")
                .code(),
            PersistenceFailureCode::Io
        );

        // The event is emitted immediately before a blocking wake receive.
        // With no token queued, observing retry_pending=true is a causal
        // no-spin proof rather than a time-based assertion.
        worker.expect_waiting(3, true);
        let non_io_retry_flush = worker.writer().flush().expect("non-I/O retry flush");
        worker.continue_wake(3);
        let second_retry = worker.expect_commit(3, TestWorkerCommitPhase::ExactRetry);
        assert_eq!(
            second_retry.overlay_mutations[&window].desired_revision(),
            1
        );
        worker.release_commit(
            3,
            TestWorkerCommitAction::ReturnDefinite(PersistenceFailure::Corrupt {
                reason: "controlled definite exact-retry failure".to_string(),
            }),
        );
        assert_eq!(
            worker.expect_commit_finished(3, TestWorkerCommitPhase::ExactRetry),
            TestWorkerCommitResult::Failed(PersistenceFailureCode::Corrupt)
        );
        worker.continue_after_commit(3);
        assert_eq!(
            non_io_retry_flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("non-I/O retry result")
                .expect_err("definite retry failure reaches its waiter")
                .code(),
            PersistenceFailureCode::Corrupt
        );

        // Retry debt predates the Corrupt result, so the exact predecessor
        // remains fenced even though the latest failure was non-I/O.
        worker.expect_waiting(4, true);
        let recovery_flush = worker.writer().flush().expect("recovery flush");
        worker.continue_wake(4);
        let third_retry = worker.expect_commit(4, TestWorkerCommitPhase::ExactRetry);
        assert_eq!(third_retry.overlay_mutations[&window].desired_revision(), 1);
        worker.release_commit(4, TestWorkerCommitAction::Run(WriteInterruption::None));
        assert!(matches!(
            worker.expect_commit_finished(4, TestWorkerCommitPhase::ExactRetry),
            TestWorkerCommitResult::Committed(CommitReceipt {
                wrote_new_generation: false,
                ..
            })
        ));
        worker.continue_after_commit(4);

        let successor = worker.expect_commit(5, TestWorkerCommitPhase::Pending);
        assert_eq!(successor.overlay_mutations[&window].desired_revision(), 2);
        worker.release_commit(
            5,
            TestWorkerCommitAction::ReturnDefinite(PersistenceFailure::Corrupt {
                reason: "controlled definite successor failure".to_string(),
            }),
        );
        assert_eq!(
            worker.expect_commit_finished(5, TestWorkerCommitPhase::Pending),
            TestWorkerCommitResult::Failed(PersistenceFailureCode::Corrupt)
        );
        worker.continue_after_commit(5);
        assert_eq!(
            recovery_flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("definite successor result")
                .expect_err("definite successor failure reaches its waiter")
                .code(),
            PersistenceFailureCode::Corrupt
        );

        // A definite successor failure retains ordinary pending work but does
        // not manufacture ambiguous retry debt.
        worker.expect_waiting(5, false);
        {
            let pending = lock_pending(&worker.writer().shared.pending);
            assert_eq!(
                pending.batch.overlay_mutations[&window].desired_revision(),
                2
            );
            assert!(pending.flush_waiters.is_empty());
            assert!(pending.binding_waiters.is_empty());
            assert_eq!(pending.waiter_count, 0);
        }
        let final_flush = worker.writer().flush().expect("final successor flush");
        worker.continue_wake(5);
        let successor_retry = worker.expect_commit(6, TestWorkerCommitPhase::Pending);
        assert_eq!(
            successor_retry.overlay_mutations[&window].desired_revision(),
            2
        );
        worker.release_commit(6, TestWorkerCommitAction::Run(WriteInterruption::None));
        let successor_receipt =
            match worker.expect_commit_finished(6, TestWorkerCommitPhase::Pending) {
                TestWorkerCommitResult::Committed(receipt) => receipt,
                TestWorkerCommitResult::Failed(code) => {
                    panic!("successor retry failed unexpectedly: {code:?}")
                }
            };
        assert!(successor_receipt.wrote_new_generation);
        assert_eq!(successor_receipt.committed_updates, 1);
        worker.continue_after_commit(6);
        assert_eq!(
            final_flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("final successor result")
                .expect("final successor commit"),
            successor_receipt
        );

        worker.expect_waiting(6, false);
        let barrier = worker.writer().flush().expect("post-success barrier");
        worker.continue_wake(6);
        let empty = worker.expect_commit(7, TestWorkerCommitPhase::Pending);
        assert!(empty.window_states.is_empty());
        assert!(empty.overlay_mutations.is_empty());
        assert!(empty.ensure_bindings.is_empty());
        worker.release_commit(7, TestWorkerCommitAction::Run(WriteInterruption::None));
        let barrier_receipt = match worker.expect_commit_finished(7, TestWorkerCommitPhase::Pending)
        {
            TestWorkerCommitResult::Committed(receipt) => receipt,
            TestWorkerCommitResult::Failed(code) => {
                panic!("post-success barrier failed unexpectedly: {code:?}")
            }
        };
        assert!(!barrier_receipt.wrote_new_generation);
        assert_eq!(
            barrier_receipt.store_revision,
            successor_receipt.store_revision
        );
        worker.continue_after_commit(7);
        assert_eq!(
            barrier
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("post-success barrier result")
                .expect("post-success barrier succeeds"),
            barrier_receipt
        );
        worker.expect_waiting(7, false);

        let snapshot = load_snapshot_at(&path).expect("load controlled successor");
        assert_eq!(
            snapshot
                .overlay(window)
                .expect("successor overlay")
                .local_revision(),
            2
        );
        let pending = lock_pending(&worker.writer().shared.pending);
        assert!(pending.batch.overlay_mutations.is_empty());
        assert!(pending.flush_waiters.is_empty());
        assert!(pending.binding_waiters.is_empty());
        assert_eq!(pending.waiter_count, 0);
        drop(pending);

        assert_eq!(
            worker.stop_and_join(),
            TestWorkerStopped {
                waiting_epoch: 7,
                commit_epoch: 7,
            }
        );
    }

    #[test]
    fn controlled_worker_panic_reports_typed_failure_and_fences_successor() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(11_055_100);
        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x83; 32]);
        let worker = ControlledPersistenceWorker::open(path.clone());
        worker.expect_waiting(1, false);

        let mut initial = PendingBatch::default();
        initial
            .queue_overlay_live(None, local_overlay(window, 1, 82))
            .expect("queue panicking transaction overlay");
        let (initial_flush, initial_binding) =
            worker.admit_batch_with_flush_and_binding(initial, fingerprint);
        worker.continue_wake(1);
        let entered = worker.expect_commit(1, TestWorkerCommitPhase::Pending);
        assert_eq!(entered.overlay_mutations[&window].desired_revision(), 1);
        worker.release_commit(1, TestWorkerCommitAction::Panic);

        assert_eq!(
            initial_flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("panicking transaction flush result")
                .expect_err("a recovered panic cannot acknowledge durability")
                .code(),
            PersistenceFailureCode::WorkerPanicked
        );
        assert_eq!(
            initial_binding
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("panicking transaction binding result")
                .expect_err("a recovered panic cannot publish binding authority")
                .code(),
            PersistenceFailureCode::WorkerPanicked
        );

        // The worker must not spin and must not let a later revision bypass
        // the exact frozen transaction whose publication status is unknown.
        worker.expect_waiting(2, true);
        worker
            .writer()
            .queue_overlay(Some(1), local_overlay(window, 2, 83))
            .expect("queue successor behind recovered panic");
        let successor_flush = worker.writer().flush().expect("successor flush");
        worker.continue_wake(2);

        let retry = worker.expect_commit(2, TestWorkerCommitPhase::ExactRetry);
        assert_eq!(retry.overlay_mutations[&window].desired_revision(), 1);
        worker.release_commit(2, TestWorkerCommitAction::Panic);
        assert_eq!(
            successor_flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("panicking exact-retry successor result")
                .expect_err("a retry panic cannot release its successor fence")
                .code(),
            PersistenceFailureCode::WorkerPanicked
        );

        // A panic while resolving existing retry debt must retain that same
        // frozen predecessor. It must neither downgrade the debt to an
        // ordinary pending batch nor allow the queued successor to bypass it.
        worker.expect_waiting(3, true);
        let recovery_flush = worker.writer().flush().expect("recovery flush");
        worker.continue_wake(3);
        let repeated_retry = worker.expect_commit(3, TestWorkerCommitPhase::ExactRetry);
        assert_eq!(
            repeated_retry.overlay_mutations[&window].desired_revision(),
            1
        );
        worker.release_commit(3, TestWorkerCommitAction::Run(WriteInterruption::None));
        assert!(matches!(
            worker.expect_commit_finished(3, TestWorkerCommitPhase::ExactRetry),
            TestWorkerCommitResult::Committed(_)
        ));
        worker.continue_after_commit(3);

        let successor = worker.expect_commit(4, TestWorkerCommitPhase::Pending);
        assert_eq!(successor.overlay_mutations[&window].desired_revision(), 2);
        worker.release_commit(4, TestWorkerCommitAction::Run(WriteInterruption::None));
        let successor_receipt =
            match worker.expect_commit_finished(4, TestWorkerCommitPhase::Pending) {
                TestWorkerCommitResult::Committed(receipt) => receipt,
                TestWorkerCommitResult::Failed(code) => {
                    panic!("successor after recovered panic failed: {code:?}")
                }
            };
        worker.continue_after_commit(4);
        assert_eq!(
            recovery_flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("recovery flush result")
                .expect("successor commits after exact predecessor retry"),
            successor_receipt
        );
        worker.expect_waiting(4, false);

        let snapshot = load_snapshot_at(&path).expect("load recovered panic authority");
        assert_eq!(
            snapshot
                .overlay(window)
                .expect("successor overlay remains live")
                .local_revision(),
            2
        );
        assert!(snapshot.binding_for(fingerprint).is_some());
        let pending = lock_pending(&worker.writer().shared.pending);
        assert!(pending.batch.overlay_mutations.is_empty());
        assert!(pending.batch.ensure_bindings.is_empty());
        assert!(pending.flush_waiters.is_empty());
        assert!(pending.binding_waiters.is_empty());
        assert_eq!(pending.waiter_count, 0);
        drop(pending);

        assert_eq!(
            worker.stop_and_join(),
            TestWorkerStopped {
                waiting_epoch: 4,
                commit_epoch: 4,
            }
        );
    }

    #[test]
    fn whole_worker_panic_drains_pending_waiters_before_stopping() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(11_055_101);
        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x84; 32]);
        let worker = ControlledPersistenceWorker::open(path.clone());
        worker.expect_waiting(1, false);

        let mut batch = PendingBatch::default();
        batch
            .queue_overlay_live(None, local_overlay(window, 1, 84))
            .expect("queue before worker-loop panic");
        let (flush, binding) = worker.admit_batch_with_flush_and_binding(batch, fingerprint);
        let shared = Arc::clone(&worker.writer().shared);

        // Panic before the blocking receive, outside the narrower journal
        // transaction boundary. The whole-worker guard must still translate
        // every admitted response and leave the unacknowledged batch intact.
        worker.panic_before_wake(1);
        assert_eq!(
            flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("whole-worker panic flush result")
                .expect_err("whole-worker panic cannot acknowledge durability")
                .code(),
            PersistenceFailureCode::WorkerPanicked
        );
        assert_eq!(
            binding
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("whole-worker panic binding result")
                .expect_err("whole-worker panic cannot publish binding authority")
                .code(),
            PersistenceFailureCode::WorkerPanicked
        );
        let post_panic_admission = match worker.writer().flush() {
            Ok(_) => panic!("stopped worker accepted a post-panic waiter"),
            Err(failure) => failure,
        };
        assert_eq!(
            post_panic_admission.code(),
            PersistenceFailureCode::WorkerStopped,
            "receiver closure must precede the terminal pending-waiter drain"
        );
        assert_eq!(
            worker.wait_stopped_and_join(),
            TestWorkerStopped {
                waiting_epoch: 1,
                commit_epoch: 0,
            }
        );

        assert!(!path.exists());
        let pending = lock_pending(&shared.pending);
        assert!(pending.batch.overlay_mutations.contains_key(&window));
        assert!(pending.batch.ensure_bindings.contains(&fingerprint));
        assert!(pending.flush_waiters.is_empty());
        assert!(pending.binding_waiters.is_empty());
        assert_eq!(pending.waiter_count, 0);
    }

    #[test]
    fn admitted_waiter_guard_translates_unwind_after_cohort_snapshot() {
        let (flush_sender, flush) = flume::bounded(1);
        let (remaining_flush_sender, remaining_flush) = flume::bounded(1);
        let (binding_sender, binding) = flume::bounded(1);
        let (remaining_binding_sender, remaining_binding) = flume::bounded(1);
        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x85; 32]);
        let mut bindings = BindingWaiters::new();
        bindings.insert(fingerprint, vec![remaining_binding_sender, binding_sender]);

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut waiters = AdmittedWaiters::new(
                vec![
                    FlushWaiter::new(remaining_flush_sender),
                    FlushWaiter::new(flush_sender),
                ],
                bindings,
            );
            let _in_flight_binding = waiters
                .next_binding()
                .expect("take one in-flight binding waiter")
                .1;
            let _in_flight_flush = waiters
                .next_flush()
                .expect("take one in-flight flush waiter");
            panic!("intentional unwind after waiter cohort snapshot");
        }));
        assert!(outcome.is_err());
        for receiver in [flush, remaining_flush] {
            assert_eq!(
                receiver
                    .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                    .expect("guarded flush unwind result")
                    .expect_err("unwind cannot acknowledge a flush")
                    .code(),
                PersistenceFailureCode::WorkerPanicked
            );
        }
        for receiver in [binding, remaining_binding] {
            assert_eq!(
                receiver
                    .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                    .expect("guarded binding unwind result")
                    .expect_err("unwind cannot publish a binding")
                    .code(),
                PersistenceFailureCode::WorkerPanicked
            );
        }
    }

    #[test]
    fn admitted_waiter_guard_preserves_fifo_completion_order() {
        let (first_flush_sender, first_flush_result) = flume::bounded(1);
        let (second_flush_sender, second_flush_result) = flume::bounded(1);
        let (first_binding_sender, first_binding_result) = flume::bounded(1);
        let (second_binding_sender, second_binding_result) = flume::bounded(1);
        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x86; 32]);
        let first_binding = DomainBindingId::from_bytes([0x31; 16]);
        let second_binding = DomainBindingId::from_bytes([0x32; 16]);
        let mut bindings = BindingWaiters::new();
        bindings.insert(
            fingerprint,
            vec![first_binding_sender, second_binding_sender],
        );
        let mut waiters = AdmittedWaiters::new(
            vec![
                FlushWaiter::new(first_flush_sender),
                FlushWaiter::new(second_flush_sender),
            ],
            bindings,
        );

        waiters
            .next_binding()
            .expect("first admitted binding waiter")
            .1
            .respond(Ok(first_binding));
        waiters
            .next_binding()
            .expect("second admitted binding waiter")
            .1
            .respond(Ok(second_binding));
        waiters
            .next_flush()
            .expect("first admitted flush waiter")
            .respond(Ok(CommitReceipt {
                store_revision: 1,
                wrote_new_generation: true,
                committed_updates: 1,
                coalesced_updates: 0,
                rejected_updates: 0,
            }));
        waiters
            .next_flush()
            .expect("second admitted flush waiter")
            .respond(Ok(CommitReceipt {
                store_revision: 2,
                wrote_new_generation: true,
                committed_updates: 1,
                coalesced_updates: 0,
                rejected_updates: 0,
            }));

        assert_eq!(
            first_binding_result
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("first binding"),
            Ok(first_binding)
        );
        assert_eq!(
            second_binding_result
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("second binding"),
            Ok(second_binding)
        );
        assert_eq!(
            first_flush_result
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("first flush")
                .expect("first receipt")
                .store_revision,
            1
        );
        assert_eq!(
            second_flush_result
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("second flush")
                .expect("second receipt")
                .store_revision,
            2
        );
    }

    #[test]
    fn terminal_drain_recovers_poisoned_pending_mutex_without_stranding_waiters() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (flush_sender, flush) = flume::bounded(1);
        let (binding_sender, binding) = flume::bounded(1);
        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x86; 32]);
        let shared = Arc::new(CoordinatorShared {
            primary_path: temp.path().join("window-state.json"),
            pending: Mutex::new(CoordinatorPending {
                flush_waiters: vec![FlushWaiter::new(flush_sender)],
                binding_waiters: BTreeMap::from([(fingerprint, vec![binding_sender])]),
                waiter_count: 2,
                ..CoordinatorPending::default()
            }),
        });

        let poisoned = Arc::clone(&shared);
        let outcome = catch_recoverable(
            RecoverablePanicSite::StorageWriter,
            std::panic::AssertUnwindSafe(|| {
                let _pending = poisoned.pending.lock().expect("lock before poison");
                panic!("intentional coordinator mutex poison");
            }),
        );
        assert!(outcome.is_err());

        drain_pending_waiters(&shared, &PersistenceFailure::WorkerPanicked);
        assert_eq!(
            flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("poisoned-mutex flush result")
                .expect_err("terminal drain cannot acknowledge a flush")
                .code(),
            PersistenceFailureCode::WorkerPanicked
        );
        assert_eq!(
            binding
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("poisoned-mutex binding result")
                .expect_err("terminal drain cannot publish a binding")
                .code(),
            PersistenceFailureCode::WorkerPanicked
        );
        let pending = lock_pending(&shared.pending);
        assert!(!shared.pending.is_poisoned());
        assert!(pending.flush_waiters.is_empty());
        assert!(pending.binding_waiters.is_empty());
        assert_eq!(pending.waiter_count, 0);
    }

    #[test]
    fn controlled_worker_retires_post_snapshot_successor_and_drains_dropped_waiters() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let mut seed = PendingBatch::default();
        seed.queue_window_state(
            "seed".to_string(),
            PersistedWindowState {
                maximized: true,
                fullscreen: false,
            },
        )
        .expect("queue journal seed");
        commit_for_test(&path, &seed, WriteInterruption::None).expect("seed controlled journal");

        let window = window_id(11_056);
        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x82; 32]);
        let worker = ControlledPersistenceWorker::open(path.clone());
        worker.expect_waiting(1, false);

        let mut initial = PendingBatch::default();
        initial
            .queue_overlay_live(None, local_overlay(window, 1, 82))
            .expect("queue frozen live overlay");
        let initial_flush = worker.admit_batch_with_flush(initial);
        worker.continue_wake(1);
        let entered = worker.expect_commit(1, TestWorkerCommitPhase::Pending);
        assert!(matches!(
            &entered.overlay_mutations[&window].desired,
            DesiredOverlayState::Live(overlay) if overlay.local_revision() == 1
        ));
        worker.release_commit(1, TestWorkerCommitAction::Run(WriteInterruption::AfterSync));
        assert_eq!(
            worker.expect_commit_finished(1, TestWorkerCommitPhase::Pending),
            TestWorkerCommitResult::Failed(PersistenceFailureCode::Io)
        );

        // Admit the delete and all of its waiters only after the real journal
        // write has returned, while acknowledgement of the frozen live
        // predecessor is still gated.
        worker
            .writer()
            .queue_overlay_delete(window, Some(1))
            .expect("queue post-snapshot retirement");
        let retirement_flush = worker.writer().flush().expect("retirement flush");
        drop(worker.writer().flush().expect("dropped retirement flush"));
        let binding = worker
            .writer()
            .ensure_domain_binding(fingerprint)
            .expect("retirement binding waiter");
        drop(
            worker
                .writer()
                .ensure_domain_binding(fingerprint)
                .expect("dropped retirement binding waiter"),
        );
        worker.continue_after_commit(1);
        assert_eq!(
            initial_flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("initial live result")
                .expect_err("ambiguous live publication is not acknowledged")
                .code(),
            PersistenceFailureCode::Io
        );

        worker.expect_waiting(2, true);
        worker.continue_wake(2);
        let retry = worker.expect_commit(2, TestWorkerCommitPhase::ExactRetry);
        assert!(matches!(
            &retry.overlay_mutations[&window].desired,
            DesiredOverlayState::Live(overlay) if overlay.local_revision() == 1
        ));
        worker.release_commit(2, TestWorkerCommitAction::Run(WriteInterruption::None));
        assert!(matches!(
            worker.expect_commit_finished(2, TestWorkerCommitPhase::ExactRetry),
            TestWorkerCommitResult::Committed(_)
        ));
        worker.continue_after_commit(2);

        let retirement = worker.expect_commit(3, TestWorkerCommitPhase::Pending);
        let retirement_mutation = &retirement.overlay_mutations[&window];
        assert_eq!(retirement_mutation.base_revision, Some(1));
        assert!(matches!(
            &retirement_mutation.desired,
            DesiredOverlayState::Deleted {
                last_local_revision: 1,
                ..
            }
        ));
        assert!(retirement.ensure_bindings.contains(&fingerprint));
        worker.release_commit(3, TestWorkerCommitAction::Run(WriteInterruption::None));
        let retirement_receipt =
            match worker.expect_commit_finished(3, TestWorkerCommitPhase::Pending) {
                TestWorkerCommitResult::Committed(receipt) => receipt,
                TestWorkerCommitResult::Failed(code) => {
                    panic!("retirement successor failed unexpectedly: {code:?}")
                }
            };
        assert!(retirement_receipt.wrote_new_generation);
        assert_eq!(retirement_receipt.committed_updates, 2);
        worker.continue_after_commit(3);

        assert_eq!(
            retirement_flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("retirement flush result")
                .expect("retirement successor commits"),
            retirement_receipt
        );
        let committed_binding = binding
            .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
            .expect("retirement binding result")
            .expect("retirement binding commits");
        worker.expect_waiting(3, false);
        assert!(matches!(
            retirement_flush.try_recv(),
            Err(flume::TryRecvError::Disconnected)
        ));
        assert!(matches!(
            binding.try_recv(),
            Err(flume::TryRecvError::Disconnected)
        ));

        let snapshot = load_snapshot_at(&path).expect("load retired successor");
        assert!(snapshot.overlay(window).is_none());
        assert_eq!(
            snapshot
                .tombstone(window)
                .expect("durable retirement tombstone")
                .last_local_revision(),
            1
        );
        assert_eq!(snapshot.binding_for(fingerprint), Some(committed_binding));
        let pending = lock_pending(&worker.writer().shared.pending);
        assert!(pending.batch.window_states.is_empty());
        assert!(pending.batch.overlay_mutations.is_empty());
        assert!(pending.batch.ensure_bindings.is_empty());
        assert!(pending.flush_waiters.is_empty());
        assert!(pending.binding_waiters.is_empty());
        assert_eq!(pending.waiter_count, 0);
        drop(pending);

        assert_eq!(
            worker.stop_and_join(),
            TestWorkerStopped {
                waiting_epoch: 3,
                commit_epoch: 3,
            }
        );
    }

    #[test]
    fn controlled_worker_reports_sticky_semantic_failure_exactly_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(11_057);
        let mut initial = PendingBatch::default();
        initial
            .queue_overlay_live(None, local_overlay(window, 1, 83))
            .expect("queue initial semantic-test overlay");
        commit_for_test(&path, &initial, WriteInterruption::None)
            .expect("commit initial semantic-test overlay");

        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x84; 32]);
        let worker = ControlledPersistenceWorker::open(path.clone());
        worker.expect_waiting(1, false);
        worker
            .writer()
            .queue_overlay(Some(2), local_overlay(window, 3, 85))
            .expect("admit durable-CAS rejection candidate");
        let binding = worker
            .writer()
            .ensure_domain_binding(fingerprint)
            .expect("semantic-test binding waiter");
        worker.continue_wake(1);

        let rejected = worker.expect_commit(1, TestWorkerCommitPhase::Pending);
        assert_eq!(rejected.overlay_mutations[&window].base_revision, Some(2));
        assert!(rejected.ensure_bindings.contains(&fingerprint));
        worker.release_commit(1, TestWorkerCommitAction::Run(WriteInterruption::None));
        let partial_receipt = match worker.expect_commit_finished(1, TestWorkerCommitPhase::Pending)
        {
            TestWorkerCommitResult::Committed(receipt) => receipt,
            TestWorkerCommitResult::Failed(code) => {
                panic!("partitioned semantic commit failed unexpectedly: {code:?}")
            }
        };
        assert_eq!(partial_receipt.committed_updates, 1);
        assert_eq!(partial_receipt.rejected_updates, 1);
        worker.continue_after_commit(1);

        let committed_binding = binding
            .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
            .expect("semantic-test binding result")
            .expect("unrelated binding commits");
        worker.expect_waiting(2, false);
        {
            let pending = lock_pending(&worker.writer().shared.pending);
            assert!(pending.batch.overlay_mutations.is_empty());
            assert!(pending.batch.ensure_bindings.is_empty());
            assert_eq!(
                pending
                    .unreported_semantic_failure
                    .as_ref()
                    .expect("sticky semantic failure")
                    .failure
                    .code(),
                PersistenceFailureCode::OverlayCasConflict
            );
            assert_eq!(pending.waiter_count, 0);
        }

        let reporting_flush = worker.writer().flush().expect("reporting flush");
        worker.continue_wake(2);
        let reporting_batch = worker.expect_commit(2, TestWorkerCommitPhase::Pending);
        assert!(reporting_batch.window_states.is_empty());
        assert!(reporting_batch.overlay_mutations.is_empty());
        assert!(reporting_batch.ensure_bindings.is_empty());
        worker.release_commit(2, TestWorkerCommitAction::Run(WriteInterruption::None));
        worker.expect_commit_finished(2, TestWorkerCommitPhase::Pending);
        worker.continue_after_commit(2);
        assert_eq!(
            reporting_flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("reporting flush result")
                .expect_err("first later flush reports sticky rejection")
                .code(),
            PersistenceFailureCode::OverlayCasConflict
        );

        worker.expect_waiting(3, false);
        assert!(matches!(
            reporting_flush.try_recv(),
            Err(flume::TryRecvError::Disconnected)
        ));
        let clean_flush = worker.writer().flush().expect("post-report flush");
        worker.continue_wake(3);
        let clean_batch = worker.expect_commit(3, TestWorkerCommitPhase::Pending);
        assert!(clean_batch.window_states.is_empty());
        assert!(clean_batch.overlay_mutations.is_empty());
        assert!(clean_batch.ensure_bindings.is_empty());
        worker.release_commit(3, TestWorkerCommitAction::Run(WriteInterruption::None));
        let clean_receipt = match worker.expect_commit_finished(3, TestWorkerCommitPhase::Pending) {
            TestWorkerCommitResult::Committed(receipt) => receipt,
            TestWorkerCommitResult::Failed(code) => {
                panic!("post-report barrier failed unexpectedly: {code:?}")
            }
        };
        worker.continue_after_commit(3);
        assert_eq!(
            clean_flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("post-report flush result")
                .expect("sticky rejection was consumed exactly once"),
            clean_receipt
        );
        worker.expect_waiting(4, false);

        let snapshot = load_snapshot_at(&path).expect("load semantic-test state");
        assert_eq!(snapshot.binding_for(fingerprint), Some(committed_binding));
        assert_eq!(
            snapshot
                .overlay(window)
                .expect("rejected overlay remains unchanged")
                .local_revision(),
            1
        );
        let pending = lock_pending(&worker.writer().shared.pending);
        assert!(pending.unreported_semantic_failure.is_none());
        assert!(pending.flush_waiters.is_empty());
        assert!(pending.binding_waiters.is_empty());
        assert_eq!(pending.waiter_count, 0);
        drop(pending);

        assert_eq!(
            worker.stop_and_join(),
            TestWorkerStopped {
                waiting_epoch: 4,
                commit_epoch: 3,
            }
        );
    }

    #[test]
    fn controlled_worker_before_wake_disconnect_drains_every_admitted_waiter() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(11_058_010);
        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x89; 32]);
        let mut worker = ControlledPersistenceWorker::open(path.clone());
        worker.expect_waiting(1, false);
        let mut batch = PendingBatch::default();
        batch
            .queue_overlay_live(None, local_overlay(window, 1, 89))
            .expect("admit pre-wake disconnect overlay");
        let (flush, binding) = worker.admit_batch_with_flush_and_binding(batch, fingerprint);
        let shared = Arc::clone(&worker.writer().shared);

        // The admitted work has queued a wake token, but the worker is still
        // outside the receive at the deterministic BeforeWake gate.
        worker.disconnect_directives();
        assert_eq!(
            flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("pre-wake disconnect flush response")
                .expect_err("pre-wake disconnect stops flush")
                .code(),
            PersistenceFailureCode::WorkerStopped
        );
        assert_eq!(
            binding
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("pre-wake disconnect binding response")
                .expect_err("pre-wake disconnect stops binding")
                .code(),
            PersistenceFailureCode::WorkerStopped
        );
        assert_eq!(
            worker.wait_stopped_and_join(),
            TestWorkerStopped {
                waiting_epoch: 1,
                commit_epoch: 0,
            }
        );
        assert!(!path.exists());
        let pending = lock_pending(&shared.pending);
        assert!(pending.batch.overlay_mutations.contains_key(&window));
        assert!(pending.batch.ensure_bindings.contains(&fingerprint));
        assert!(pending.flush_waiters.is_empty());
        assert!(pending.binding_waiters.is_empty());
        assert_eq!(pending.waiter_count, 0);
    }

    #[test]
    fn controlled_worker_rejects_stale_commit_epoch_before_writing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(11_058_011);
        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x8a; 32]);
        let worker = ControlledPersistenceWorker::open(path.clone());
        worker.expect_waiting(1, false);
        let mut batch = PendingBatch::default();
        batch
            .queue_overlay_live(None, local_overlay(window, 1, 90))
            .expect("admit stale-epoch overlay");
        let (flush, binding) = worker.admit_batch_with_flush_and_binding(batch, fingerprint);
        let shared = Arc::clone(&worker.writer().shared);
        worker.continue_wake(1);
        worker.expect_commit(1, TestWorkerCommitPhase::Pending);

        // A directive from any earlier epoch must fail closed before the
        // requested commit action can touch storage.
        worker.send_directive(
            0,
            TestWorkerDirectiveAction::Commit(TestWorkerCommitAction::Run(WriteInterruption::None)),
        );
        assert_eq!(
            flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("stale-epoch flush response")
                .expect_err("stale epoch stops flush")
                .code(),
            PersistenceFailureCode::WorkerStopped
        );
        assert_eq!(
            binding
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("stale-epoch binding response")
                .expect_err("stale epoch stops binding")
                .code(),
            PersistenceFailureCode::WorkerStopped
        );
        assert_eq!(
            worker.wait_stopped_and_join(),
            TestWorkerStopped {
                waiting_epoch: 1,
                commit_epoch: 1,
            }
        );
        assert!(!path.exists());
        let pending = lock_pending(&shared.pending);
        assert!(pending.batch.overlay_mutations.contains_key(&window));
        assert!(pending.batch.ensure_bindings.contains(&fingerprint));
        assert!(pending.flush_waiters.is_empty());
        assert!(pending.binding_waiters.is_empty());
        assert_eq!(pending.waiter_count, 0);
    }

    #[test]
    fn controlled_worker_directive_disconnect_drains_every_admitted_waiter() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(11_058);
        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x86; 32]);
        let mut worker = ControlledPersistenceWorker::open(path.clone());
        worker.expect_waiting(1, false);
        let mut batch = PendingBatch::default();
        batch
            .queue_overlay_live(None, local_overlay(window, 1, 86))
            .expect("admit disconnect overlay");
        let (flush, binding) = worker.admit_batch_with_flush_and_binding(batch, fingerprint);
        let shared = Arc::clone(&worker.writer().shared);
        worker.continue_wake(1);
        worker.expect_commit(1, TestWorkerCommitPhase::Pending);
        worker.disconnect_directives();

        assert_eq!(
            flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("directive-disconnect flush response")
                .expect_err("directive disconnect stops flush")
                .code(),
            PersistenceFailureCode::WorkerStopped
        );
        assert_eq!(
            binding
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("directive-disconnect binding response")
                .expect_err("directive disconnect stops binding")
                .code(),
            PersistenceFailureCode::WorkerStopped
        );
        assert_eq!(
            worker.wait_stopped_and_join(),
            TestWorkerStopped {
                waiting_epoch: 1,
                commit_epoch: 1,
            }
        );
        assert!(!path.exists());
        let pending = lock_pending(&shared.pending);
        assert!(pending.batch.overlay_mutations.contains_key(&window));
        assert!(pending.batch.ensure_bindings.contains(&fingerprint));
        assert!(pending.flush_waiters.is_empty());
        assert!(pending.binding_waiters.is_empty());
        assert_eq!(pending.waiter_count, 0);
    }

    #[test]
    fn controlled_worker_post_commit_disconnect_preserves_recoverable_durable_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(11_058_100);
        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x88; 32]);
        let mut worker = ControlledPersistenceWorker::open(path.clone());
        worker.expect_waiting(1, false);
        let mut batch = PendingBatch::default();
        batch
            .queue_overlay_live(None, local_overlay(window, 1, 88))
            .expect("admit post-commit disconnect overlay");
        let (flush, binding) = worker.admit_batch_with_flush_and_binding(batch, fingerprint);
        let shared = Arc::clone(&worker.writer().shared);
        worker.continue_wake(1);
        worker.expect_commit(1, TestWorkerCommitPhase::Pending);
        worker.release_commit(
            1,
            TestWorkerCommitAction::Run(WriteInterruption::AfterDirectorySync),
        );
        assert_eq!(
            worker.expect_commit_finished(1, TestWorkerCommitPhase::Pending),
            TestWorkerCommitResult::Failed(PersistenceFailureCode::Io)
        );

        // The real write has returned and its journal lock is gone, but the
        // worker has not acknowledged the frozen batch or answered waiters.
        // Disconnecting this release gate must stop cleanly without claiming
        // that the durably recoverable publication was acknowledged.
        worker.disconnect_directives();
        assert_eq!(
            flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("post-commit disconnect flush response")
                .expect_err("post-commit disconnect stops flush")
                .code(),
            PersistenceFailureCode::WorkerStopped
        );
        assert_eq!(
            binding
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("post-commit disconnect binding response")
                .expect_err("post-commit disconnect stops binding")
                .code(),
            PersistenceFailureCode::WorkerStopped
        );
        assert_eq!(
            worker.wait_stopped_and_join(),
            TestWorkerStopped {
                waiting_epoch: 1,
                commit_epoch: 1,
            }
        );

        let snapshot = load_snapshot_at(&path).expect("recover post-commit publication");
        assert_eq!(
            snapshot
                .overlay(window)
                .expect("durably published overlay remains recoverable")
                .local_revision(),
            1
        );
        assert!(snapshot.binding_for(fingerprint).is_some());
        let pending = lock_pending(&shared.pending);
        // No acknowledgement occurred, so the exact admitted batch remains
        // intact even though every waiter was drained as WorkerStopped.
        assert!(pending.batch.overlay_mutations.contains_key(&window));
        assert!(pending.batch.ensure_bindings.contains(&fingerprint));
        assert!(pending.flush_waiters.is_empty());
        assert!(pending.binding_waiters.is_empty());
        assert_eq!(pending.waiter_count, 0);
    }

    #[test]
    fn controlled_worker_event_disconnect_drains_every_admitted_waiter() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(11_059);
        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x87; 32]);
        let mut worker = ControlledPersistenceWorker::open(path.clone());
        worker.expect_waiting(1, false);
        worker
            .writer()
            .queue_overlay(None, local_overlay(window, 1, 87))
            .expect("admit event-disconnect overlay");
        let flush = worker.writer().flush().expect("event-disconnect flush");
        let binding = worker
            .writer()
            .ensure_domain_binding(fingerprint)
            .expect("event-disconnect binding");
        let shared = Arc::clone(&worker.writer().shared);

        // Drop the event receiver while the worker is still held at the
        // already-observed wake gate. Its subsequent CommitEntered try_send
        // therefore fails deterministically rather than racing a receiver.
        worker.disconnect_events();
        worker.continue_wake(1);
        assert_eq!(
            flush
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("event-disconnect flush response")
                .expect_err("event disconnect stops flush")
                .code(),
            PersistenceFailureCode::WorkerStopped
        );
        assert_eq!(
            binding
                .recv_timeout(CONTROLLED_WORKER_WATCHDOG)
                .expect("event-disconnect binding response")
                .expect_err("event disconnect stops binding")
                .code(),
            PersistenceFailureCode::WorkerStopped
        );
        assert_eq!(
            worker.wait_stopped_and_join(),
            TestWorkerStopped {
                waiting_epoch: 1,
                commit_epoch: 1,
            }
        );
        assert!(!path.exists());
        let pending = lock_pending(&shared.pending);
        assert!(pending.batch.overlay_mutations.contains_key(&window));
        assert!(pending.batch.ensure_bindings.contains(&fingerprint));
        assert!(pending.flush_waiters.is_empty());
        assert!(pending.binding_waiters.is_empty());
        assert_eq!(pending.waiter_count, 0);
    }

    #[test]
    fn duplicate_tab_owner_rejection_does_not_wedge_later_worker_commits() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let owner_window = window_id(11_060);
        let duplicate_window = window_id(11_061);
        let later_window = window_id(11_062);
        let owned_slot = local_slot(90);
        let mut initial = PendingBatch::default();
        initial
            .queue_overlay_live(
                None,
                MixedDomainLayoutOverlay::new(
                    owner_window,
                    "default",
                    1,
                    vec![owned_slot],
                    Some(owned_slot),
                )
                .expect("initial owner"),
            )
            .expect("queue initial owner");
        commit_for_test(&path, &initial, WriteInterruption::None).expect("commit initial owner");

        let mut first_batch = PendingBatch::default();
        first_batch
            .queue_overlay_live(
                None,
                MixedDomainLayoutOverlay::new(
                    duplicate_window,
                    "default",
                    1,
                    vec![owned_slot],
                    Some(owned_slot),
                )
                .expect("structurally valid duplicate owner"),
            )
            .expect("queue duplicate owner");
        first_batch
            .queue_window_state(
                "duplicate-owner".to_string(),
                PersistedWindowState {
                    maximized: false,
                    fullscreen: true,
                },
            )
            .expect("queue unrelated geometry");
        let (first_sender, first_receiver) = flume::bounded(1);
        let shared = Arc::new(CoordinatorShared {
            primary_path: path.clone(),
            pending: Mutex::new(CoordinatorPending {
                batch: first_batch,
                flush_waiters: vec![FlushWaiter::new(first_sender)],
                binding_waiters: BTreeMap::new(),
                waiter_count: 1,
                ..CoordinatorPending::default()
            }),
        });
        let (wake, receiver) = flume::bounded(1);
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::spawn(move || persistence_worker(worker_shared, receiver));
        wake.send(()).expect("wake duplicate-owner batch");
        assert_eq!(
            first_receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("first flush response")
                .expect_err("duplicate owner must be a semantic rejection")
                .code(),
            PersistenceFailureCode::Invalid
        );

        let (second_sender, second_receiver) = flume::bounded(1);
        {
            let mut pending = lock_pending(&shared.pending);
            pending
                .batch
                .queue_overlay_live(None, local_overlay(later_window, 1, 91))
                .expect("queue later valid overlay");
            pending.flush_waiters.push(FlushWaiter::new(second_sender));
            pending.waiter_count = 1;
        }
        wake.send(()).expect("wake later valid batch");
        second_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("second flush response")
            .expect("later valid batch must not be wedged");
        drop(wake);
        worker.join().expect("persistence worker exits");

        let snapshot = load_snapshot_at(&path).expect("load post-rejection state");
        assert!(snapshot.overlay(owner_window).is_some());
        assert!(snapshot.overlay(duplicate_window).is_none());
        assert!(snapshot.overlay(later_window).is_some());
        assert!(snapshot.window_states["duplicate-owner"].fullscreen);
    }

    #[test]
    fn individually_rejected_acquirer_cannot_split_an_ownership_transfer() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let source_window = window_id(11_063);
        let destination_window = window_id(11_064);
        let slot = local_slot(92);
        let mut initial = PendingBatch::default();
        initial
            .queue_overlay_live(None, local_overlay(source_window, 1, 92))
            .expect("queue initial source owner");
        initial
            .queue_overlay_live(
                None,
                MixedDomainLayoutOverlay::new(destination_window, "default", 1, Vec::new(), None)
                    .expect("initial empty destination"),
            )
            .expect("queue initial destination");
        commit_for_test(&path, &initial, WriteInterruption::None)
            .expect("commit initial transfer authority");
        // Initial publication creates one valid slot by design. Repair the
        // missing journal peer before testing rejected-batch publication so
        // durability maintenance cannot legitimately write a generation.
        commit_for_test(&path, &PendingBatch::default(), WriteInterruption::None)
            .expect("establish healthy two-slot transfer authority");
        let before = load_snapshot_at(&path).expect("load healthy transfer authority");
        assert!(!before.degraded_recovery);

        let mut transfer = PendingBatch::default();
        transfer
            .queue_overlay_live(
                Some(1),
                MixedDomainLayoutOverlay::new(source_window, "default", 2, Vec::new(), None)
                    .expect("source release"),
            )
            .expect("queue source release");
        transfer
            .queue_overlay_live(
                Some(2),
                MixedDomainLayoutOverlay::new(
                    destination_window,
                    "default",
                    3,
                    vec![slot],
                    Some(slot),
                )
                .expect("wrong-base destination acquisition"),
            )
            .expect("queue wrong-base destination acquisition");

        let rejected = commit_for_test(&path, &transfer, WriteInterruption::None)
            .expect("reject the complete transfer component");
        assert_eq!(rejected.receipt.committed_updates, 0);
        assert_eq!(rejected.receipt.rejected_updates, 2);
        assert!(!rejected.receipt.wrote_new_generation);
        assert_eq!(rejected.receipt.store_revision, before.store_revision);
        assert!(rejected.accepted_overlay_ids.is_empty());
        for window_id in [source_window, destination_window] {
            assert_eq!(
                rejected.rejected_overlay_mutations[&window_id]
                    .failure
                    .code(),
                PersistenceFailureCode::OverlayCasConflict
            );
        }

        let snapshot = load_snapshot_at(&path).expect("load rejected transfer authority");
        assert_eq!(snapshot, before);
        assert_eq!(
            snapshot
                .overlay(source_window)
                .expect("source ownership retained")
                .slots(),
            &[slot]
        );
        assert!(
            snapshot
                .overlay(destination_window)
                .expect("destination retained")
                .slots()
                .is_empty()
        );
    }

    #[test]
    fn disjoint_conflict_components_reject_exactly_their_own_claims_in_either_order() {
        for reverse in [false, true] {
            let temp = tempfile::tempdir().expect("tempdir");
            let path = temp.path().join("window-state.json");
            let first_conflict_left = window_id(0x10);
            let first_conflict_right = window_id(0x11);
            let second_conflict_left = window_id(0x20);
            let second_conflict_right = window_id(0x21);
            let disjoint = window_id(0x30);
            let first_conflicted_slot = local_slot(0xa0);
            let second_conflicted_slot = local_slot(0xb0);
            let disjoint_slot = local_slot(0xc0);
            let order = if reverse {
                [
                    disjoint,
                    second_conflict_right,
                    first_conflict_left,
                    second_conflict_left,
                    first_conflict_right,
                ]
            } else {
                [
                    first_conflict_right,
                    second_conflict_left,
                    disjoint,
                    first_conflict_left,
                    second_conflict_right,
                ]
            };
            let mut batch = PendingBatch::default();
            for window_id in order {
                let slot = if [first_conflict_left, first_conflict_right].contains(&window_id) {
                    first_conflicted_slot
                } else if [second_conflict_left, second_conflict_right].contains(&window_id) {
                    second_conflicted_slot
                } else {
                    assert_eq!(window_id, disjoint);
                    disjoint_slot
                };
                batch
                    .queue_overlay_live(
                        None,
                        MixedDomainLayoutOverlay::new(
                            window_id,
                            "default",
                            1,
                            vec![slot],
                            Some(slot),
                        )
                        .expect("new ownership claimant"),
                    )
                    .expect("queue ownership claimant");
            }
            assert_eq!(
                overlay_admission_components(&PersistedState::default(), &batch),
                vec![
                    vec![first_conflict_left, first_conflict_right],
                    vec![second_conflict_left, second_conflict_right],
                    vec![disjoint],
                ]
            );

            let committed = commit_for_test(&path, &batch, WriteInterruption::None)
                .expect("partition conflicting and disjoint ownership components");
            assert_eq!(committed.receipt.committed_updates, 1);
            assert_eq!(committed.receipt.coalesced_updates, 0);
            assert_eq!(committed.receipt.rejected_updates, 4);
            assert!(committed.receipt.wrote_new_generation);
            assert_eq!(committed.accepted_overlay_ids, BTreeSet::from([disjoint]));
            assert_eq!(
                committed
                    .rejected_overlay_mutations
                    .keys()
                    .copied()
                    .collect::<Vec<_>>(),
                vec![
                    first_conflict_left,
                    first_conflict_right,
                    second_conflict_left,
                    second_conflict_right,
                ]
            );
            for window_id in [
                first_conflict_left,
                first_conflict_right,
                second_conflict_left,
                second_conflict_right,
            ] {
                assert_eq!(
                    committed.rejected_overlay_mutations[&window_id].failure,
                    PersistenceFailure::invalid(
                        "one stable tab identity would be owned by multiple layout windows"
                    )
                );
            }

            let snapshot = load_snapshot_at(&path).expect("load partitioned ownership claims");
            assert_eq!(snapshot.store_revision, 1);
            for window_id in [
                first_conflict_left,
                first_conflict_right,
                second_conflict_left,
                second_conflict_right,
            ] {
                assert!(snapshot.overlay(window_id).is_none());
            }
            assert_eq!(
                snapshot
                    .overlay(disjoint)
                    .expect("disjoint claim committed")
                    .slots(),
                &[disjoint_slot]
            );
        }
    }

    #[test]
    fn ownership_only_rejection_at_revision_ceiling_requires_no_publication() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let source_window = window_id(11_068);
        let destination_window = window_id(11_069);
        let slot = local_slot(95);
        let mut state = PersistedState {
            store_revision: u64::MAX,
            ..PersistedState::default()
        };
        state.overlays = vec![
            MixedDomainLayoutOverlay::new(source_window, "default", 1, vec![slot], Some(slot))
                .expect("revision-ceiling source owner"),
            MixedDomainLayoutOverlay::new(destination_window, "default", 1, Vec::new(), None)
                .expect("revision-ceiling empty destination"),
        ];
        canonicalize_state(&mut state);
        validate_state(&state).expect("revision-ceiling authority is valid");
        let encoded = encode_disk_slot(&state).expect("encode revision-ceiling authority");
        std::fs::write(&path, &encoded).expect("write primary revision-ceiling authority");
        std::fs::write(shadow_file_name(&path), encoded)
            .expect("write shadow revision-ceiling authority");
        let before = load_snapshot_at(&path).expect("load healthy revision-ceiling authority");
        assert!(!before.degraded_recovery);

        let mut transfer = PendingBatch::default();
        transfer
            .queue_overlay_live(
                Some(1),
                MixedDomainLayoutOverlay::new(source_window, "default", 2, Vec::new(), None)
                    .expect("revision-ceiling source release"),
            )
            .expect("queue revision-ceiling source release");
        transfer
            .queue_overlay_live(
                Some(2),
                MixedDomainLayoutOverlay::new(
                    destination_window,
                    "default",
                    3,
                    vec![slot],
                    Some(slot),
                )
                .expect("revision-ceiling wrong-base acquisition"),
            )
            .expect("queue revision-ceiling wrong-base acquisition");

        let rejected = commit_for_test(&path, &transfer, WriteInterruption::None)
            .expect("fully rejected transfer does not consume a store revision");
        assert_eq!(rejected.receipt.store_revision, u64::MAX);
        assert_eq!(rejected.receipt.committed_updates, 0);
        assert_eq!(rejected.receipt.rejected_updates, 2);
        assert!(!rejected.receipt.wrote_new_generation);
        assert!(rejected.accepted_overlay_ids.is_empty());
        for window_id in [source_window, destination_window] {
            assert_eq!(
                rejected.rejected_overlay_mutations[&window_id]
                    .failure
                    .code(),
                PersistenceFailureCode::OverlayCasConflict
            );
        }

        let snapshot = load_snapshot_at(&path).expect("reload revision-ceiling authority");
        assert_eq!(snapshot, before);
        assert_eq!(
            snapshot
                .overlay(source_window)
                .expect("source ownership retained")
                .slots(),
            &[slot]
        );
        assert!(
            snapshot
                .overlay(destination_window)
                .expect("destination retained")
                .slots()
                .is_empty()
        );
    }

    #[test]
    fn one_batch_can_transfer_tab_ownership_between_layout_windows() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let source_window = window_id(11_070);
        let destination_window = window_id(11_071);
        let slot = local_slot(92);
        let mut initial = PendingBatch::default();
        initial
            .queue_overlay_live(
                None,
                MixedDomainLayoutOverlay::new(source_window, "default", 1, vec![slot], Some(slot))
                    .expect("source owner"),
            )
            .expect("queue source owner");
        commit_for_test(&path, &initial, WriteInterruption::None).expect("commit source owner");

        let mut transfer = PendingBatch::default();
        transfer
            .queue_overlay_live(
                Some(1),
                MixedDomainLayoutOverlay::new(source_window, "default", 2, Vec::new(), None)
                    .expect("empty source after transfer"),
            )
            .expect("queue source removal");
        transfer
            .queue_overlay_live(
                None,
                MixedDomainLayoutOverlay::new(
                    destination_window,
                    "default",
                    1,
                    vec![slot],
                    Some(slot),
                )
                .expect("destination owner"),
            )
            .expect("queue destination ownership");
        let committed = commit_for_test(&path, &transfer, WriteInterruption::None)
            .expect("commit atomic ownership transfer");
        assert_eq!(committed.receipt.rejected_updates, 0);

        let snapshot = load_snapshot_at(&path).expect("load ownership transfer");
        assert!(
            snapshot
                .overlay(source_window)
                .expect("source overlay retained")
                .slots()
                .is_empty()
        );
        assert_eq!(
            snapshot
                .overlay(destination_window)
                .expect("destination overlay")
                .slots(),
            &[slot]
        );
    }

    #[test]
    fn one_batch_transfers_remote_identity_while_updating_window_placement() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let (_, binding) = ensure_binding_for_test(&path, 0x93);
        let source_window = window_id(11_080);
        let destination_window = window_id(11_081);
        let source_placement = remote_slot(binding, 0x94, 10, 55);
        let destination_placement = remote_slot(binding, 0x94, 20, 55);
        assert_eq!(
            source_placement.identity(),
            destination_placement.identity()
        );

        let mut initial = PendingBatch::default();
        initial
            .queue_overlay_live(
                None,
                MixedDomainLayoutOverlay::new(
                    source_window,
                    "default",
                    1,
                    vec![source_placement],
                    Some(source_placement),
                )
                .expect("source remote owner"),
            )
            .expect("queue source remote owner");
        commit_for_test(&path, &initial, WriteInterruption::None)
            .expect("commit source remote owner");

        let mut transfer = PendingBatch::default();
        transfer
            .queue_overlay_live(
                Some(1),
                MixedDomainLayoutOverlay::new(source_window, "default", 2, Vec::new(), None)
                    .expect("empty source after remote transfer"),
            )
            .expect("queue remote source removal");
        transfer
            .queue_overlay_live(
                None,
                MixedDomainLayoutOverlay::new(
                    destination_window,
                    "default",
                    1,
                    vec![destination_placement],
                    Some(destination_placement),
                )
                .expect("destination remote owner"),
            )
            .expect("queue destination remote ownership");
        let committed = commit_for_test(&path, &transfer, WriteInterruption::None)
            .expect("commit atomic remote ownership transfer");
        assert_eq!(committed.receipt.rejected_updates, 0);

        let snapshot = load_snapshot_at(&path).expect("load remote ownership transfer");
        assert!(
            snapshot
                .overlay(source_window)
                .expect("source overlay retained")
                .slots()
                .is_empty()
        );
        assert_eq!(
            snapshot
                .overlay(destination_window)
                .expect("destination remote overlay")
                .slots(),
            &[destination_placement]
        );
    }

    #[test]
    fn unresolved_same_batch_binding_request_cannot_authorize_a_guessed_remote_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let invalid_window = window_id(11_100);
        let valid_window = window_id(11_101);
        let unknown_binding = DomainBindingId::from_bytes([0x86; 16]);
        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x85; 32]);
        let remote = remote_slot(unknown_binding, 0x87, 1, 1);
        let invalid_overlay =
            MixedDomainLayoutOverlay::new(invalid_window, "default", 1, vec![remote], Some(remote))
                .expect("structurally valid overlay");
        let mut batch = PendingBatch::default();
        batch.ensure_bindings.insert(fingerprint);
        batch
            .queue_overlay_live(None, invalid_overlay)
            .expect("queue cross-state-invalid overlay");
        batch
            .queue_overlay_live(None, local_overlay(valid_window, 1, 88))
            .expect("queue valid local overlay");
        batch
            .queue_window_state(
                "binding-check".to_string(),
                PersistedWindowState {
                    maximized: true,
                    fullscreen: false,
                },
            )
            .expect("queue unrelated geometry");

        let committed = commit_for_test(&path, &batch, WriteInterruption::None)
            .expect("partition unknown binding lineage");
        assert_eq!(committed.receipt.committed_updates, 3);
        assert_eq!(committed.receipt.coalesced_updates, 0);
        assert_eq!(committed.receipt.rejected_updates, 1);
        assert_eq!(
            committed.rejected_overlay_mutations[&invalid_window].failure,
            PersistenceFailure::invalid("overlay remote slot references an unknown domain binding")
        );
        let committed_binding = committed.bindings[&fingerprint];
        assert!(committed.accepted_overlay_ids.contains(&valid_window));
        let snapshot = load_snapshot_at(&path).expect("load partitioned binding result");
        assert!(snapshot.overlay(invalid_window).is_none());
        assert!(snapshot.overlay(valid_window).is_some());
        assert!(snapshot.window_states["binding-check"].maximized);
        assert!(snapshot.domain_bindings.iter().any(|record| {
            record.target_fingerprint == fingerprint && record.binding_id == committed_binding
        }));
    }

    #[test]
    fn worker_reports_overlay_rejection_without_failing_committed_binding_waiter() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(12_000);
        let mut initial = PendingBatch::default();
        initial
            .queue_overlay_live(None, local_overlay(window, 1, 91))
            .expect("queue initial overlay");
        commit_for_test(&path, &initial, WriteInterruption::None).expect("commit initial overlay");

        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x92; 32]);
        let mut batch = PendingBatch::default();
        batch
            .queue_overlay_live(Some(2), local_overlay(window, 3, 93))
            .expect("queue wrong-base mutation");
        batch
            .queue_window_state(
                "worker".to_string(),
                PersistedWindowState {
                    maximized: false,
                    fullscreen: true,
                },
            )
            .expect("queue worker geometry");
        batch.ensure_bindings.insert(fingerprint);

        let (flush_sender, flush_receiver) = flume::bounded(1);
        let (binding_sender, binding_receiver) = flume::bounded(1);
        let shared = Arc::new(CoordinatorShared {
            primary_path: path.clone(),
            pending: Mutex::new(CoordinatorPending {
                batch,
                flush_waiters: vec![FlushWaiter::new(flush_sender)],
                binding_waiters: BTreeMap::from([(fingerprint, vec![binding_sender])]),
                waiter_count: 2,
                ..CoordinatorPending::default()
            }),
        });
        let (wake, receiver) = flume::bounded(1);
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::spawn(move || persistence_worker(worker_shared, receiver));
        wake.send(()).expect("wake deterministic worker");

        let binding = binding_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("binding waiter response")
            .expect("binding committed despite overlay rejection");
        let flush_failure = flush_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("flush waiter response")
            .expect_err("flush reports first semantic rejection");
        assert_eq!(
            flush_failure.code(),
            PersistenceFailureCode::OverlayCasConflict
        );
        drop(wake);
        worker.join().expect("persistence worker exits");

        let snapshot = load_snapshot_at(&path).expect("load worker partial success");
        assert_eq!(snapshot.binding_for(fingerprint), Some(binding));
        assert!(snapshot.window_states["worker"].fullscreen);
        assert_eq!(
            snapshot
                .overlay(window)
                .expect("wrong-base overlay unchanged")
                .local_revision(),
            1
        );
        let pending = lock_pending(&shared.pending);
        assert!(pending.batch.overlay_mutations.is_empty());
        assert!(pending.batch.window_states.is_empty());
        assert!(pending.batch.ensure_bindings.is_empty());
    }

    #[test]
    fn later_explicit_flush_reports_and_consumes_a_debounced_rejection() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = window_id(12_050);
        let mut initial = PendingBatch::default();
        initial
            .queue_overlay_live(None, local_overlay(window, 1, 94))
            .expect("queue initial overlay");
        commit_for_test(&path, &initial, WriteInterruption::None).expect("commit initial overlay");

        let fingerprint = PrivacySafeTargetFingerprint::from_bytes([0x95; 32]);
        let mut rejected_batch = PendingBatch::default();
        rejected_batch
            .queue_overlay_live(Some(2), local_overlay(window, 3, 96))
            .expect("queue wrong-base overlay");
        rejected_batch.ensure_bindings.insert(fingerprint);
        let (binding_sender, binding_receiver) = flume::bounded(1);
        let shared = Arc::new(CoordinatorShared {
            primary_path: path.clone(),
            pending: Mutex::new(CoordinatorPending {
                batch: rejected_batch,
                flush_waiters: Vec::new(),
                binding_waiters: BTreeMap::from([(fingerprint, vec![binding_sender])]),
                waiter_count: 1,
                ..CoordinatorPending::default()
            }),
        });
        let (wake, receiver) = flume::bounded(1);
        let writer = PersistenceWriter {
            shared: Arc::clone(&shared),
            wake: wake.clone(),
        };
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::spawn(move || persistence_worker(worker_shared, receiver));
        wake.send(()).expect("wake debounced rejection");
        binding_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("binding completion proves first batch resolved")
            .expect("unrelated binding commits");

        assert_eq!(
            writer
                .flush()
                .expect("register later explicit flush")
                .recv_timeout(Duration::from_secs(5))
                .expect("later explicit flush response")
                .expect_err("later flush must observe the unreported rejection")
                .code(),
            PersistenceFailureCode::OverlayCasConflict
        );
        writer
            .flush()
            .expect("register post-consumption flush")
            .recv_timeout(Duration::from_secs(5))
            .expect("post-consumption flush response")
            .expect("the first rejection is reported exactly once");

        drop(writer);
        drop(wake);
        worker.join().expect("persistence worker exits");
        assert!(
            load_snapshot_at(&path)
                .expect("load first batch partial success")
                .binding_for(fingerprint)
                .is_some()
        );
    }

    #[test]
    fn worker_reports_exact_binding_quota_failure_while_committing_geometry() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let mut state = PersistedState::default();
        state.store_revision = 1;
        state.domain_bindings = capped_domain_bindings();
        canonicalize_state(&mut state);
        std::fs::write(&path, encode_disk_slot(&state).expect("encode binding cap"))
            .expect("write binding cap");

        let existing_fingerprint = indexed_fingerprint(0);
        let rejected_fingerprint = PrivacySafeTargetFingerprint::from_bytes([0xfe; 32]);
        let mut batch = PendingBatch::default();
        batch.ensure_bindings.insert(existing_fingerprint);
        batch.ensure_bindings.insert(rejected_fingerprint);
        batch
            .queue_window_state(
                "quota-worker".to_string(),
                PersistedWindowState {
                    maximized: true,
                    fullscreen: false,
                },
            )
            .expect("queue unrelated geometry");

        let (flush_sender, flush_receiver) = flume::bounded(1);
        let (existing_sender, existing_receiver) = flume::bounded(1);
        let (rejected_sender, rejected_receiver) = flume::bounded(1);
        let shared = Arc::new(CoordinatorShared {
            primary_path: path.clone(),
            pending: Mutex::new(CoordinatorPending {
                batch,
                flush_waiters: vec![FlushWaiter::new(flush_sender)],
                binding_waiters: BTreeMap::from([
                    (existing_fingerprint, vec![existing_sender]),
                    (rejected_fingerprint, vec![rejected_sender]),
                ]),
                waiter_count: 3,
                ..CoordinatorPending::default()
            }),
        });
        let (wake, receiver) = flume::bounded(1);
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::spawn(move || persistence_worker(worker_shared, receiver));
        wake.send(()).expect("wake deterministic worker");

        assert_eq!(
            existing_receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("existing binding response")
                .expect("existing binding remains resolvable"),
            indexed_binding_id(0)
        );
        assert_eq!(
            rejected_receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("rejected binding response")
                .expect_err("new binding must be rejected at the authority cap")
                .code(),
            PersistenceFailureCode::Quota
        );
        assert_eq!(
            flush_receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("flush response")
                .expect_err("flush reports the deterministic partial rejection")
                .code(),
            PersistenceFailureCode::Quota
        );
        drop(wake);
        worker.join().expect("persistence worker exits");

        let snapshot = load_snapshot_at(&path).expect("load partial binding result");
        assert!(snapshot.window_states["quota-worker"].maximized);
        let pending = lock_pending(&shared.pending);
        assert!(pending.batch.ensure_bindings.is_empty());
        assert!(pending.batch.window_states.is_empty());
    }

    #[test]
    fn post_snapshot_flush_waiter_retains_first_rejection_until_its_later_barrier() {
        let (sender, receiver) = flume::bounded(1);
        let mut waiter = FlushWaiter::new(sender);
        let first = PersistenceFailure::OverlayCasConflict {
            expected: Some(3),
            committed: Some(2),
        };
        waiter.remember_semantic_failure(&SemanticFailureOutcome {
            identity: Arc::new(()),
            failure: first.clone(),
        });
        waiter.remember_semantic_failure(&SemanticFailureOutcome {
            identity: Arc::new(()),
            failure: PersistenceFailure::RetiredOverlay { last_revision: 4 },
        });
        let later_receipt = CommitReceipt {
            store_revision: 9,
            wrote_new_generation: true,
            committed_updates: 1,
            coalesced_updates: 0,
            rejected_updates: 0,
        };
        let transaction_failure = PersistenceFailure::Io {
            operation: "test later barrier",
            kind: io::ErrorKind::Other,
        };
        assert_eq!(
            waiter.transaction_failure_result(&transaction_failure),
            Err(transaction_failure)
        );

        waiter
            .sender
            .send(waiter.result(None, later_receipt))
            .expect("send later barrier result");
        assert_eq!(
            receiver.recv().expect("receive carried rejection"),
            Err(first)
        );
    }

    #[test]
    fn stale_semantic_failure_identity_cannot_clear_a_new_identical_failure() {
        let mut pending = CoordinatorPending::default();
        let failure = PersistenceFailure::RetiredOverlay { last_revision: 7 };
        let first = pending.record_semantic_failure(&failure);
        pending.clear_reported_semantic_failure(&first.identity);
        let second = pending.record_semantic_failure(&failure);

        pending.clear_reported_semantic_failure(&first.identity);

        let retained = pending
            .unreported_semantic_failure
            .as_ref()
            .expect("the newer semantic failure must remain pending");
        assert!(Arc::ptr_eq(&retained.identity, &second.identity));
        assert!(!Arc::ptr_eq(&first.identity, &second.identity));
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
        let reconciled = reconcile_overlay(&overlay, &[right, left, unseen], &unavailable_bindings)
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
    fn remote_window_move_preserves_persisted_order_active_identity_and_routing() {
        let binding = DomainBindingId::from_bytes([0x68; 16]);
        let left = remote_slot(binding, 1, 10, 1);
        let moved_before = remote_slot(binding, 1, 10, 2);
        let moved_after = remote_slot(binding, 1, 99, 2);
        let right = remote_slot(binding, 1, 10, 3);
        assert_eq!(moved_before.identity(), moved_after.identity());
        assert_ne!(moved_before, moved_after);

        let overlay = MixedDomainLayoutOverlay::new(
            LayoutWindowId::from_bytes([0x69; 16]),
            "default",
            1,
            vec![left, moved_before, right],
            Some(moved_before),
        )
        .expect("overlay before remote move");
        let reconciled = reconcile_overlay(&overlay, &[right, moved_after, left], &BTreeSet::new())
            .expect("moved placement reconciles by stable tab identity");

        assert_eq!(reconciled.ordered_slots, vec![left, moved_after, right]);
        assert_eq!(reconciled.live_slots, vec![left, moved_after, right]);
        assert_eq!(reconciled.active_live_slot, Some(moved_after));
        assert_eq!(reconciled.dropped_closed_or_stale, 0);
        assert_eq!(reconciled.appended_new, 0);

        let encoded = serde_json::to_vec(&moved_after).expect("encode moved placement");
        let decoded: StableTabSlot =
            serde_json::from_slice(&encoded).expect("decode moved placement");
        assert_eq!(decoded, moved_after);
        assert_eq!(decoded.identity(), moved_before.identity());
        let StableTabSlot::Remote {
            remote_window_id, ..
        } = decoded
        else {
            panic!("remote placement roundtrip changed slot kind");
        };
        assert_eq!(remote_window_id, 99);
    }

    #[test]
    fn duplicate_remote_placements_and_cross_binding_aliases_fail_closed() {
        let first_binding = DomainBindingId::from_bytes([0x6a; 16]);
        let second_binding = DomainBindingId::from_bytes([0x6b; 16]);
        let first_placement = remote_slot(first_binding, 7, 1, 44);
        let second_placement = remote_slot(first_binding, 7, 2, 44);

        let duplicate_overlay = MixedDomainLayoutOverlay::new(
            LayoutWindowId::from_bytes([0x6c; 16]),
            "default",
            1,
            vec![first_placement, second_placement],
            Some(first_placement),
        )
        .expect_err("one stable tab cannot occupy two windows");
        assert_eq!(duplicate_overlay.code(), PersistenceFailureCode::Invalid);

        let valid_overlay = MixedDomainLayoutOverlay::new(
            LayoutWindowId::from_bytes([0x6d; 16]),
            "default",
            1,
            vec![first_placement],
            Some(first_placement),
        )
        .expect("single placement overlay");
        let duplicate_live = reconcile_overlay(
            &valid_overlay,
            &[first_placement, second_placement],
            &BTreeSet::new(),
        )
        .expect_err("live topology cannot publish two placements for one tab");
        assert_eq!(duplicate_live.code(), PersistenceFailureCode::Invalid);

        let aliased = remote_slot(second_binding, 7, 3, 44);
        let alias_failure = MixedDomainLayoutOverlay::new(
            LayoutWindowId::from_bytes([0x6e; 16]),
            "default",
            1,
            vec![first_placement, aliased],
            Some(first_placement),
        )
        .expect_err("one server tab cannot be aliased through two bindings");
        assert_eq!(alias_failure.code(), PersistenceFailureCode::Invalid);
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

        let failure = reconcile_overlay(&overlay, &[slot], &BTreeSet::from([unavailable]))
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

        let disconnected =
            reconcile_overlay(&overlay, &[right, left], &BTreeSet::from([unavailable]))
                .expect("retain unavailable placeholder");
        assert_eq!(
            disconnected.ordered_slots,
            vec![left, missing_active, right]
        );
        assert_eq!(disconnected.active_live_slot, Some(right));

        let moved_active = remote_slot(unavailable, 2, 42, 2);
        let reappeared =
            reconcile_overlay(&overlay, &[right, moved_active, left], &BTreeSet::new())
                .expect("reconcile reappeared binding after remote-window move");
        assert_eq!(reappeared.ordered_slots, vec![left, moved_active, right]);
        assert_eq!(reappeared.active_live_slot, Some(moved_active));
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
        commit_for_test(&path, &second, WriteInterruption::AfterPartialWrite)
            .expect_err("injected partial write");
        let after_partial = load_snapshot_at(&path).expect("recover old");
        assert!(after_partial.window_states["default"].maximized);
        assert!(!after_partial.window_states["default"].fullscreen);

        commit_for_test(&path, &second, WriteInterruption::None).expect("retry second");
        let mut third = PendingBatch::default();
        third
            .queue_window_state(
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
    fn full_write_retry_syncs_the_selected_authority_before_success() {
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
        commit_for_test(&path, &first, WriteInterruption::None).expect("commit first state");

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
        commit_for_test(&path, &second, WriteInterruption::AfterFullWrite)
            .expect_err("inject failure after complete but unsynced bytes");

        assert_eq!(
            commit_for_test(&path, &second, WriteInterruption::AfterDirectorySync,)
                .expect_err("prove idempotent retry crosses the durability barrier")
                .code(),
            PersistenceFailureCode::Io
        );
        let replay = commit_for_test(&path, &second, WriteInterruption::None)
            .expect("durable idempotent replay");
        assert!(!replay.receipt.wrote_new_generation);
        assert!(
            load_snapshot_at(&path)
                .expect("load durable retry")
                .window_states["default"]
                .fullscreen
        );
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
                (state.maximized && !state.fullscreen) || (!state.maximized && state.fullscreen),
                "recovery must select the prior or fully encoded generation"
            );
        }
    }

    #[test]
    fn interrupted_first_publish_retains_the_empty_authority() {
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
        commit_for_test(&path, &first, WriteInterruption::AfterPartialWrite)
            .expect_err("injected first partial write");

        let snapshot = load_snapshot_at(&path).expect("empty authority survives");
        assert_eq!(snapshot.source, StoreSource::Empty);
        assert!(snapshot.window_states.is_empty());

        commit_for_test(&path, &first, WriteInterruption::None).expect("retry first publish");
        assert!(
            load_snapshot_at(&path)
                .expect("load committed first generation")
                .window_states["default"]
                .maximized
        );
    }

    #[test]
    fn corrupt_inactive_slot_is_quarantined_before_reuse() {
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
        commit_for_test(&path, &first, WriteInterruption::None).expect("first");
        std::fs::write(shadow_file_name(&path), b"truncated").expect("corrupt inactive shadow");

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
        commit_for_test(&path, &second, WriteInterruption::None).expect("recover and commit");
        let quarantines = std::fs::read_dir(temp.path())
            .expect("list directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".corrupt-"))
            .count();
        assert_eq!(quarantines, 1);
        assert!(
            load_snapshot_at(&path)
                .expect("load recovered")
                .window_states["default"]
                .fullscreen
        );
    }

    #[test]
    fn partial_existing_quarantine_never_authorizes_source_overwrite() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let shadow = shadow_file_name(&path);
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
        let corrupt_bytes = b"truncated".to_vec();
        std::fs::write(&shadow, &corrupt_bytes).expect("write corrupt inactive slot");
        quarantine_corrupt_evidence(&CorruptEvidence {
            path: shadow.clone(),
            bytes: corrupt_bytes.clone(),
        })
        .expect("create complete quarantine evidence");
        let quarantine = std::fs::read_dir(temp.path())
            .expect("list quarantine directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().contains(".corrupt-"))
            })
            .expect("find quarantine evidence");
        std::fs::write(&quarantine, b"partial").expect("simulate partial evidence publish");

        assert_eq!(
            commit_for_test(&path, &PendingBatch::default(), WriteInterruption::None,)
                .expect_err("partial evidence must block corrupt-slot reuse")
                .code(),
            PersistenceFailureCode::Corrupt
        );
        assert_eq!(
            std::fs::read(&shadow).expect("read preserved corrupt source"),
            corrupt_bytes
        );
    }

    #[test]
    fn empty_commit_repairs_a_degraded_two_slot_journal() {
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
        std::fs::write(shadow_file_name(&path), b"truncated").expect("corrupt inactive slot");
        assert!(
            load_snapshot_at(&path)
                .expect("load degraded authority")
                .degraded_recovery
        );

        let repaired = commit_for_test(&path, &PendingBatch::default(), WriteInterruption::None)
            .expect("empty barrier repairs degraded redundancy");
        assert!(repaired.receipt.wrote_new_generation);
        let snapshot = load_snapshot_at(&path).expect("load repaired journal");
        assert!(!snapshot.degraded_recovery);
        assert!(snapshot.window_states["default"].maximized);
        let quarantines = std::fs::read_dir(temp.path())
            .expect("list repaired directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".corrupt-"))
            .count();
        assert_eq!(quarantines, 1);
    }

    #[test]
    fn empty_commit_repairs_either_missing_journal_peer() {
        let temp = tempfile::tempdir().expect("tempdir");
        let primary_only = temp.path().join("primary-only.json");
        let mut initial = PendingBatch::default();
        initial
            .queue_window_state(
                "primary".to_string(),
                PersistedWindowState {
                    maximized: true,
                    fullscreen: false,
                },
            )
            .expect("queue primary-only state");
        commit_for_test(&primary_only, &initial, WriteInterruption::None)
            .expect("publish primary-only authority");
        assert!(
            load_snapshot_at(&primary_only)
                .expect("load primary-only authority")
                .degraded_recovery
        );
        commit_for_test(
            &primary_only,
            &PendingBatch::default(),
            WriteInterruption::None,
        )
        .expect("repair missing shadow");
        assert!(
            !load_snapshot_at(&primary_only)
                .expect("load repaired primary authority")
                .degraded_recovery
        );

        let shadow_only = temp.path().join("shadow-only.json");
        let mut shadow_state = PersistedState::default();
        shadow_state.store_revision = 7;
        shadow_state.window_states.insert(
            "shadow".to_string(),
            PersistedWindowState {
                maximized: false,
                fullscreen: true,
            },
        );
        std::fs::write(
            shadow_file_name(&shadow_only),
            encode_disk_slot(&shadow_state).expect("encode shadow-only authority"),
        )
        .expect("write shadow-only authority");
        let before = load_snapshot_at(&shadow_only).expect("load shadow-only authority");
        assert_eq!(before.source, StoreSource::Shadow);
        assert!(before.degraded_recovery);
        commit_for_test(
            &shadow_only,
            &PendingBatch::default(),
            WriteInterruption::None,
        )
        .expect("repair missing primary");
        let after = load_snapshot_at(&shadow_only).expect("load repaired shadow authority");
        assert!(!after.degraded_recovery);
        assert!(after.window_states["shadow"].fullscreen);
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
                "overlays": [],
                "tombstones": []
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
        first
            .queue_window_state(
                "default".to_string(),
                PersistedWindowState {
                    maximized: true,
                    fullscreen: false,
                },
            )
            .expect("queue first state");
        commit_for_test(&path, &first, WriteInterruption::None).expect("first");
        commit_for_test(&path, &PendingBatch::default(), WriteInterruption::None)
            .expect("repair initial one-slot journal");
        let before = load_snapshot_at(&path).expect("before");
        assert!(!before.degraded_recovery);

        let slot = local_slot(1);
        let malformed_window = LayoutWindowId::from_bytes([0x91; 16]);
        let malformed = MixedDomainLayoutOverlay {
            window_id: malformed_window,
            workspace: "default".to_string(),
            local_revision: 1,
            slots: vec![slot, slot],
            active: Some(slot),
        };
        let malformed_mutation = PendingOverlayMutation {
            base_revision: None,
            desired: DesiredOverlayState::Live(malformed.clone()),
            superseded_updates: 0,
        };
        let mut batch = PendingBatch::default();
        batch.overlay_tab_count = malformed.slots.len();
        batch
            .overlay_mutations
            .insert(malformed_window, malformed_mutation.clone());
        let rejection = commit_for_test(&path, &batch, WriteInterruption::None)
            .expect("malformed overlay lineage is partitioned");
        assert!(!rejection.receipt.wrote_new_generation);
        assert_eq!(rejection.receipt.store_revision, before.store_revision);
        assert_eq!(rejection.receipt.committed_updates, 0);
        assert_eq!(rejection.receipt.rejected_updates, 1);
        assert!(rejection.accepted_overlay_ids.is_empty());
        assert_eq!(rejection.rejected_overlay_mutations.len(), 1);
        let rejected = &rejection.rejected_overlay_mutations[&malformed_window];
        assert_eq!(rejected.mutation, malformed_mutation);
        assert_eq!(
            rejected.failure,
            PersistenceFailure::Invalid {
                reason: "one stable tab identity would be owned by multiple layout windows"
                    .to_string(),
            }
        );
        assert_eq!(load_snapshot_at(&path).expect("after"), before);
    }

    #[test]
    fn stale_overlay_cannot_mutate_committed_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        let window = LayoutWindowId::from_bytes([0x92; 16]);
        let initial_slot = local_slot(1);
        let mut initial = PendingBatch::default();
        initial
            .queue_overlay_live(
                None,
                MixedDomainLayoutOverlay::new(
                    window,
                    "default",
                    1,
                    vec![initial_slot],
                    Some(initial_slot),
                )
                .expect("initial overlay"),
            )
            .expect("queue initial overlay");
        commit_for_test(&path, &initial, WriteInterruption::None).expect("commit initial");

        let current_slot = local_slot(2);
        let mut current = PendingBatch::default();
        current
            .queue_overlay_live(
                Some(1),
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
            .queue_overlay_live(
                None,
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
        let rejection = commit_for_test(&path, &stale, WriteInterruption::None)
            .expect("stale lineage is partitioned from the transaction");
        assert!(!rejection.receipt.wrote_new_generation);
        assert_eq!(rejection.receipt.rejected_updates, 1);
        assert_eq!(
            rejection.rejected_overlay_mutations[&window].failure.code(),
            PersistenceFailureCode::StaleOverlay
        );
        assert_eq!(
            load_snapshot_at(&path).expect("load after rejection"),
            before
        );
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
    fn stopped_worker_rejects_before_mutation_or_waiter_admission() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (wake, receiver) = flume::bounded(1);
        drop(receiver);
        let writer = PersistenceWriter {
            shared: Arc::new(CoordinatorShared {
                primary_path: temp.path().join("window-state.json"),
                pending: Mutex::new(CoordinatorPending::default()),
            }),
            wake,
        };
        assert_eq!(
            writer
                .queue_window_state("stopped", PersistedWindowState::default())
                .expect_err("stopped worker rejects geometry")
                .code(),
            PersistenceFailureCode::WorkerStopped
        );
        assert_eq!(
            writer
                .queue_overlay(None, local_overlay(window_id(20_000), 1, 100))
                .expect_err("stopped worker rejects overlay")
                .code(),
            PersistenceFailureCode::WorkerStopped
        );
        assert_eq!(
            writer
                .ensure_domain_binding(PrivacySafeTargetFingerprint::from_bytes([0xa1; 32]))
                .expect_err("stopped worker rejects binding waiter")
                .code(),
            PersistenceFailureCode::WorkerStopped
        );
        assert_eq!(
            writer
                .flush()
                .expect_err("stopped worker rejects flush waiter")
                .code(),
            PersistenceFailureCode::WorkerStopped
        );
        let pending = lock_pending(&writer.shared.pending);
        assert!(pending.batch.window_states.is_empty());
        assert!(pending.batch.overlay_mutations.is_empty());
        assert!(pending.batch.ensure_bindings.is_empty());
        assert!(pending.flush_waiters.is_empty());
        assert!(pending.binding_waiters.is_empty());
        assert_eq!(pending.waiter_count, 0);
    }

    #[test]
    fn waiter_capacity_rejects_without_waking_or_mutating_pending_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (wake, _receiver) = flume::bounded(1);
        let writer = PersistenceWriter {
            shared: Arc::new(CoordinatorShared {
                primary_path: temp.path().join("window-state.json"),
                pending: Mutex::new(CoordinatorPending {
                    waiter_count: MAX_PENDING_WAITERS,
                    ..CoordinatorPending::default()
                }),
            }),
            wake,
        };
        assert_eq!(
            writer
                .ensure_domain_binding(PrivacySafeTargetFingerprint::from_bytes([0xa2; 32]))
                .expect_err("binding waiter cap must reject")
                .code(),
            PersistenceFailureCode::Quota
        );
        assert_eq!(
            writer
                .flush()
                .expect_err("flush waiter cap must reject")
                .code(),
            PersistenceFailureCode::Quota
        );
        let pending = lock_pending(&writer.shared.pending);
        assert!(pending.batch.ensure_bindings.is_empty());
        assert!(pending.flush_waiters.is_empty());
        assert!(pending.binding_waiters.is_empty());
        assert_eq!(pending.waiter_count, MAX_PENDING_WAITERS);
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
        let encoded =
            String::from_utf8(encode_disk_slot(&state).expect("encode")).expect("JSON is UTF-8");
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
    fn streaming_disk_slot_encoder_matches_derived_serializer_oracle() {
        let mut state = PersistedState::default();
        state.store_revision = 19;
        state.window_states.insert(
            "quote-\"-slash-\\-snowman-☃".to_string(),
            PersistedWindowState {
                maximized: true,
                fullscreen: false,
            },
        );
        let binding_id = DomainBindingId::from_bytes([0x9a; 16]);
        state.domain_bindings.push(DomainBindingRecord {
            target_fingerprint: PrivacySafeTargetFingerprint::from_bytes([0x8b; 32]),
            binding_id,
        });
        let slot = remote_slot(binding_id, 0x7c, 99, 101);
        state.overlays.push(
            MixedDomainLayoutOverlay::new(
                LayoutWindowId::from_bytes([0x6d; 16]),
                "quote-\"-slash-\\-snowman-☃",
                1,
                vec![slot],
                Some(slot),
            )
            .expect("oracle overlay"),
        );
        state.tombstones.push(
            OverlayTombstone::new(LayoutWindowId::from_bytes([0x5e; 16]), 4, 17)
                .expect("oracle tombstone"),
        );
        canonicalize_state(&mut state);
        validate_published_state(&state).expect("oracle state is publishable");

        let payload = serde_json::to_vec(&state).expect("serialize oracle payload");
        let sha256: [u8; 32] = Sha256::digest(&payload).into();
        let oracle = serde_json::to_vec(&DiskSlot {
            payload: state.clone(),
            sha256,
        })
        .expect("serialize derived disk-slot oracle");
        let encoded = encode_disk_slot(&state).expect("streaming disk-slot encode");

        assert_eq!(encoded, oracle);
        assert_eq!(
            encoded_json_len(&BorrowedDiskSlot {
                payload: &state,
                sha256,
            })
            .expect("count borrowed slot"),
            u64::try_from(encoded.len()).expect("encoded length fits u64")
        );
    }

    #[test]
    fn encoded_state_budget_matches_serde_with_maximum_checksum_width() {
        let mut state = PersistedState::default();
        state.store_revision = 9;
        state.window_states.insert(
            "escaped-\"-\\-☃".to_string(),
            PersistedWindowState {
                maximized: false,
                fullscreen: true,
            },
        );
        state.domain_bindings.push(DomainBindingRecord {
            target_fingerprint: PrivacySafeTargetFingerprint::from_bytes([10; 32]),
            binding_id: DomainBindingId::from_bytes([100; 16]),
        });
        let slots = local_slots(99, 3);
        state.overlays.push(
            MixedDomainLayoutOverlay::new(
                LayoutWindowId::from_bytes([9; 16]),
                "escaped-\"-\\-☃",
                7,
                slots.clone(),
                slots.first().copied(),
            )
            .expect("budget overlay"),
        );
        state.tombstones.push(
            OverlayTombstone::new(LayoutWindowId::from_bytes([255; 16]), 8, 9)
                .expect("budget tombstone"),
        );
        canonicalize_state(&mut state);

        let published_revision = 10;
        let budget = EncodedStateBudget::from_state(&state, published_revision)
            .expect("construct encoded budget");
        let mut projected = state;
        projected.store_revision = published_revision;
        for binding in &mut projected.domain_bindings {
            binding.binding_id = DomainBindingId::from_bytes([u8::MAX; 16]);
        }
        let oracle = encoded_json_len(&BorrowedDiskSlot {
            payload: &projected,
            sha256: [u8::MAX; 32],
        })
        .expect("count maximum-checksum oracle");

        assert_eq!(budget.upper_bound().expect("budget upper bound"), oracle);
    }

    #[test]
    fn maximum_width_binding_budget_bounds_numeric_byte_arrays() {
        for byte in [0, 9, 10, 99, 100, 255] {
            let fingerprint = PrivacySafeTargetFingerprint::from_bytes([byte; 32]);
            let actual = encoded_json_len(&DomainBindingRecord {
                target_fingerprint: fingerprint,
                binding_id: DomainBindingId::from_bytes([byte; 16]),
            })
            .expect("count actual binding");
            let maximum =
                maximum_width_binding_len(fingerprint).expect("count maximum-width binding");
            assert!(actual <= maximum, "byte pattern {byte} exceeded its bound");
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
                "tombstones": [],
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
        fn streaming_encoder_and_upper_bound_hold_for_varied_valid_states(
            workspace_seed in prop::collection::vec(any::<u8>(), 0..80),
            mut tab_bytes in prop::collection::vec(any::<u8>(), 0..32),
            binding_byte in any::<u8>(),
            include_window in any::<bool>(),
            include_binding in any::<bool>(),
            include_overlay in any::<bool>(),
            include_tombstone in any::<bool>(),
            store_revision in 1u64..1_000_000,
        ) {
            let mut workspace = String::from("property-");
            for byte in workspace_seed {
                workspace.push_str(match byte % 5 {
                    0 => "a",
                    1 => "\"",
                    2 => "\\",
                    3 => "☃",
                    _ => "z",
                });
            }
            let mut state = PersistedState::default();
            state.store_revision = store_revision;
            if include_window {
                state.window_states.insert(
                    workspace.clone(),
                    PersistedWindowState {
                        maximized: binding_byte & 1 != 0,
                        fullscreen: binding_byte & 2 != 0,
                    },
                );
            }
            if include_binding {
                state.domain_bindings.push(DomainBindingRecord {
                    target_fingerprint: PrivacySafeTargetFingerprint::from_bytes([
                        binding_byte;
                        32
                    ]),
                    binding_id: DomainBindingId::from_bytes([binding_byte.max(1); 16]),
                });
            }
            tab_bytes.sort_unstable();
            tab_bytes.dedup();
            if include_overlay {
                let slots = tab_bytes.iter().copied().map(local_slot).collect::<Vec<_>>();
                state.overlays.push(
                    MixedDomainLayoutOverlay::new(
                        LayoutWindowId::from_bytes([0xc2; 16]),
                        &workspace,
                        1,
                        slots.clone(),
                        slots.first().copied(),
                    )
                    .expect("generated valid overlay"),
                );
            }
            if include_tombstone {
                state.tombstones.push(
                    OverlayTombstone::new(
                        LayoutWindowId::from_bytes([0xc3; 16]),
                        1,
                        store_revision,
                    )
                    .expect("generated valid tombstone"),
                );
            }
            canonicalize_state(&mut state);
            validate_published_state(&state).expect("generated state is publishable");

            let payload = serde_json::to_vec(&state).expect("serialize property payload");
            let sha256: [u8; 32] = Sha256::digest(&payload).into();
            let oracle = serde_json::to_vec(&DiskSlot {
                payload: state.clone(),
                sha256,
            })
            .expect("serialize property oracle");
            let encoded = encode_disk_slot(&state).expect("stream property state");
            prop_assert_eq!(&encoded, &oracle);

            let upper_bound = EncodedStateBudget::from_state(&state, store_revision)
                .and_then(|budget| budget.upper_bound())
                .expect("count property upper bound");
            let physical_upper_bound =
                EncodedStateBudget::from_state_with_physical_bindings(&state, store_revision)
                    .and_then(|budget| budget.upper_bound())
                    .expect("count property physical upper bound");
            prop_assert!(
                u64::try_from(encoded.len()).expect("encoded length fits u64")
                    <= physical_upper_bound
            );
            prop_assert!(physical_upper_bound <= upper_bound);
        }

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
