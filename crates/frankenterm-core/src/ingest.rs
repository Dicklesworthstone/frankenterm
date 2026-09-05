//! Ingest pipeline for pane output capture
//!
//! Handles delta extraction, sequence numbering, gap detection, and pane discovery.
//!
//! # Discovery Loop
//!
//! The discovery system polls `wezterm cli list` to:
//! - Track pane lifecycle (new/closed/changed)
//! - Apply include/exclude filters for privacy and performance
//! - Maintain stable lifecycle identities separately from mutable metadata
//!
//! # Delta Extraction
//!
//! Converts repeated snapshots into minimal deltas using overlap matching.

use std::collections::{HashMap, HashSet, VecDeque, hash_map::Entry};
use std::hash::Hash;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use frankenterm_alloc::{PaneArena, PaneArenaRegistry, PaneArenaSnapshot, PaneArenaStats};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::config::{PaneFilterConfig, TraumaGuardConfig};
use crate::error::{Result, RuntimeOperationSource};
use crate::storage::{Gap, PaneRecord, Segment, StorageHandle};
use crate::trauma_guard::{TraumaDecision, TraumaState, hash_command};
use crate::wezterm::{PaneInfo, stable_hash};

// =============================================================================
// Time Utilities
// =============================================================================

/// Get current time as epoch milliseconds
fn epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

// =============================================================================
// Ingest Telemetry
// =============================================================================

/// Operational telemetry counters for the ingest pipeline.
///
/// All counters are monotonically increasing. Use `snapshot()` to read
/// current values for reporting and serialization.
#[derive(Debug, Clone, Default)]
pub struct IngestTelemetry {
    /// Number of `discovery_tick()` calls
    discovery_ticks: u64,
    /// Total panes discovered (first seen)
    panes_discovered: u64,
    /// Total panes closed (removed from registry)
    panes_closed: u64,
    /// Total authoritative lifecycle replacements detected
    lifecycle_replacements: u64,
    /// Total metadata-only changes detected
    metadata_changes: u64,
    /// Total panes filtered out by observation rules
    panes_filtered: u64,
}

impl IngestTelemetry {
    /// Create a new telemetry instance with all counters at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a completed discovery tick and its diff results.
    fn record_discovery_tick(&mut self, diff: &DiscoveryDiff) {
        self.discovery_ticks = self.discovery_ticks.saturating_add(1);
        self.panes_discovered = self
            .panes_discovered
            .saturating_add(u64::try_from(diff.new_panes.len()).unwrap_or(u64::MAX));
        self.panes_closed = self
            .panes_closed
            .saturating_add(u64::try_from(diff.closed_panes.len()).unwrap_or(u64::MAX));
        self.lifecycle_replacements = self
            .lifecycle_replacements
            .saturating_add(u64::try_from(diff.lifecycle_replacements.len()).unwrap_or(u64::MAX));
        self.metadata_changes = self
            .metadata_changes
            .saturating_add(u64::try_from(diff.metadata_changes.len()).unwrap_or(u64::MAX));
    }

    /// Record a pane being filtered out by observation rules.
    fn record_pane_filtered(&mut self) {
        self.panes_filtered = self.panes_filtered.saturating_add(1);
    }

    /// Take a serializable snapshot of current counter values.
    #[must_use]
    pub fn snapshot(&self) -> IngestTelemetrySnapshot {
        IngestTelemetrySnapshot {
            discovery_ticks: self.discovery_ticks,
            panes_discovered: self.panes_discovered,
            panes_closed: self.panes_closed,
            lifecycle_replacements: self.lifecycle_replacements,
            metadata_changes: self.metadata_changes,
            panes_filtered: self.panes_filtered,
        }
    }
}

/// Serializable snapshot of ingest telemetry counters.
///
/// Produced by [`IngestTelemetry::snapshot()`] for reporting, persistence,
/// or export to the telemetry pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestTelemetrySnapshot {
    pub discovery_ticks: u64,
    pub panes_discovered: u64,
    pub panes_closed: u64,
    pub lifecycle_replacements: u64,
    pub metadata_changes: u64,
    pub panes_filtered: u64,
}

// =============================================================================
// Pane UUID
// =============================================================================

/// Generate a stable pane UUID.
///
/// The UUID is a hex-encoded hash combining:
/// - domain name
/// - pane_id (session-local, but helps distinguish within session)
/// - creation timestamp (epoch ms)
/// - random entropy (ensures uniqueness even with identical metadata)
///
/// Format: 32-character lowercase hex string (16 bytes / 128 bits)
///
/// This approach:
/// - Is bounded: computed once at pane discovery, never updated
/// - Is safe: purely read-based, no writes to WezTerm
/// - Is non-deterministic: random entropy is mixed in to avoid collisions
#[must_use]
pub fn generate_pane_uuid(domain: &str, pane_id: u64, created_at: i64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update(pane_id.to_le_bytes());
    hasher.update(created_at.to_le_bytes());

    // Add random entropy to ensure uniqueness even if same pane_id reappears
    let entropy: [u8; 8] = rand::rng().random();
    hasher.update(entropy);

    let hash = hasher.finalize();

    // Take first 16 bytes and encode as lowercase hex (32 chars)
    hex_encode(&hash[..16])
}

/// Encode bytes as lowercase hex string
fn hex_encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        })
}

// =============================================================================
// Pane lifecycle identity and mutable metadata
// =============================================================================

/// Checked lifecycle revision for one continuously tracked numeric pane ID.
///
/// This revision changes only when authoritative membership evidence changes.
/// Presentation metadata such as title, cwd, geometry, focus, and display name
/// is deliberately excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaneLifecycleRevision(u32);

impl PaneLifecycleRevision {
    pub(crate) const INITIAL: Self = Self(0);

    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    fn checked_next(self) -> Option<Self> {
        self.get().checked_add(1).map(Self)
    }
}

/// Checked revision for latest-wins mutable pane metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaneMetadataRevision(u64);

impl PaneMetadataRevision {
    pub(crate) const INITIAL: Self = Self(0);

    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    fn checked_next(self) -> Option<Self> {
        self.get().checked_add(1).map(Self)
    }
}

/// Authoritative pane membership evidence available from the current backend.
///
/// Numeric pane ID is the mux's continuity authority while it remains present
/// in consecutive coherent listings. Domain display name, title, cwd,
/// workspace, geometry, cursor, focus, and zoom are intentionally absent.
/// `domain_id` and `tty_name` strengthen replacement detection when supplied.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PaneLifecycleIdentity {
    pub pane_id: u64,
    pub domain_id: Option<u64>,
    pub tty_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneLifecycleContinuity {
    Same,
    Replaced,
    Ambiguous,
}

impl PaneLifecycleIdentity {
    #[must_use]
    pub fn from_pane_info(info: &PaneInfo) -> Self {
        Self {
            pane_id: info.pane_id,
            domain_id: info.domain_id,
            tty_name: info.tty_name.clone(),
        }
    }

    /// Compare exact lifecycle evidence without consulting mutable display
    /// metadata. Losing previously available exact evidence is ambiguous and
    /// must not silently preserve capture admission.
    #[must_use]
    pub fn continuity_with(&self, next: &Self) -> PaneLifecycleContinuity {
        if self.pane_id != next.pane_id {
            return PaneLifecycleContinuity::Replaced;
        }
        if (self.domain_id.is_some() && next.domain_id.is_none())
            || (self.tty_name.is_some() && next.tty_name.is_none())
        {
            return PaneLifecycleContinuity::Ambiguous;
        }
        if self
            .domain_id
            .zip(next.domain_id)
            .is_some_and(|(left, right)| left != right)
            || self
                .tty_name
                .as_deref()
                .zip(next.tty_name.as_deref())
                .is_some_and(|(left, right)| left != right)
        {
            return PaneLifecycleContinuity::Replaced;
        }
        PaneLifecycleContinuity::Same
    }

    fn merge_available_evidence(&mut self, next: &Self) {
        if self.domain_id.is_none() {
            self.domain_id = next.domain_id;
        }
        if self.tty_name.is_none() {
            self.tty_name.clone_from(&next.tty_name);
        }
    }
}

/// Field-level mutable metadata difference for one discovery observation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaneMetadataDiff(u16);

impl PaneMetadataDiff {
    const DOMAIN_DISPLAY_NAME: u16 = 1 << 0;
    const WORKSPACE: u16 = 1 << 1;
    const PLACEMENT: u16 = 1 << 2;
    const SIZE: u16 = 1 << 3;
    const TITLE: u16 = 1 << 4;
    const CWD: u16 = 1 << 5;
    const CURSOR: u16 = 1 << 6;
    const LAYOUT: u16 = 1 << 7;
    const ACTIVE_ZOOM: u16 = 1 << 8;
    const EXTRA: u16 = 1 << 9;
    const OBSERVATION: u16 = 1 << 10;
    const IDENTITY_EVIDENCE: u16 = 1 << 11;

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    fn include_observation_change(&mut self) {
        self.0 |= Self::OBSERVATION;
    }

    fn between(old: &PaneInfo, new: &PaneInfo) -> Self {
        let mut bits = 0;
        if old.domain_name != new.domain_name {
            bits |= Self::DOMAIN_DISPLAY_NAME;
        }
        if old.workspace != new.workspace {
            bits |= Self::WORKSPACE;
        }
        if old.window_id != new.window_id || old.tab_id != new.tab_id {
            bits |= Self::PLACEMENT;
        }
        if old.size != new.size || old.rows != new.rows || old.cols != new.cols {
            bits |= Self::SIZE;
        }
        if old.title != new.title {
            bits |= Self::TITLE;
        }
        if old.cwd != new.cwd {
            bits |= Self::CWD;
        }
        if old.cursor_x != new.cursor_x
            || old.cursor_y != new.cursor_y
            || old.cursor_visibility != new.cursor_visibility
        {
            bits |= Self::CURSOR;
        }
        if old.left_col != new.left_col || old.top_row != new.top_row {
            bits |= Self::LAYOUT;
        }
        if old.is_active != new.is_active || old.is_zoomed != new.is_zoomed {
            bits |= Self::ACTIVE_ZOOM;
        }
        if old.extra != new.extra {
            bits |= Self::EXTRA;
        }
        if old.domain_id != new.domain_id || old.tty_name != new.tty_name {
            // Newly available exact evidence strengthens the existing
            // lifecycle identity without rotating it, but still needs a
            // latest-wins persistence/UI refresh. Contradictions are handled
            // by `PaneLifecycleIdentity::continuity_with` before this diff is
            // committed.
            bits |= Self::IDENTITY_EVIDENCE;
        }
        Self(bits)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneMetadataChange {
    pub pane_id: u64,
    pub lifecycle_revision: PaneLifecycleRevision,
    pub metadata_revision: PaneMetadataRevision,
    pub diff: PaneMetadataDiff,
}

// =============================================================================
// Observation Decision
// =============================================================================

/// Decision about whether to observe a pane
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationDecision {
    /// Pane should be observed
    Observed,
    /// Pane should be ignored with a reason
    Ignored { reason: String },
}

impl ObservationDecision {
    /// Check if this is an observed decision
    #[must_use]
    pub fn is_observed(&self) -> bool {
        matches!(self, Self::Observed)
    }

    /// Get the ignore reason if ignored
    #[must_use]
    pub fn ignore_reason(&self) -> Option<&str> {
        match self {
            Self::Observed => None,
            Self::Ignored { reason } => Some(reason),
        }
    }
}

/// Result of re-deciding a tracked pane's observation state (ft-0kdi9).
///
/// Returned by [`PaneRegistry::re_evaluate_observation`] so callers can mirror
/// the transition into whatever capture-side state they own. The registry's own
/// cursor map is not the one the capture pipeline reads; a caller that drops
/// this value keeps the registry consistent but leaves any sibling map stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationTransition {
    /// Pane is untracked, or its observation state did not change.
    Unchanged,
    /// Pane went `Ignored` -> `Observed`; capture state must be re-created.
    Resumed,
    /// Pane went `Observed` -> `Ignored`; capture state may be retired.
    Retired,
}

// =============================================================================
// Extended Pane Entry
// =============================================================================

/// Runtime override for pane capture priority.
///
/// This is an operator knob intended for incident response. It is stored
/// in-memory only (watcher process); callers may optionally set a TTL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanePriorityOverride {
    /// Priority value (lower = higher priority).
    pub priority: u32,
    /// When the override was set (epoch ms).
    pub set_at: i64,
    /// When the override expires (epoch ms). `None` means "until cleared".
    pub expires_at: Option<i64>,
}

/// Extended pane state with lifecycle identity and observation tracking
#[derive(Debug, Clone)]
pub struct PaneEntry {
    /// Current pane info from WezTerm
    pub info: PaneInfo,
    /// Exact lifecycle evidence, separate from mutable presentation metadata.
    pub lifecycle_identity: PaneLifecycleIdentity,
    /// Observation decision (observe vs ignore)
    pub observation: ObservationDecision,
    /// Stable pane UUID (persists across renames/moves within a session)
    ///
    /// Assigned once at discovery, never changes for this pane's lifetime.
    /// Format: 32-character lowercase hex string.
    pub pane_uuid: String,
    /// First seen timestamp (epoch ms)
    pub first_seen_at: i64,
    /// Last seen timestamp (epoch ms)
    pub last_seen_at: i64,
    /// When observation decision was made (epoch ms)
    pub decision_at: i64,
    /// Next sequence number to resume from if observation is later restored.
    pub resume_next_seq: u64,
    /// Checked revision that changes only on authoritative replacement.
    pub lifecycle_revision: PaneLifecycleRevision,
    /// Checked latest-wins metadata revision.
    pub metadata_revision: PaneMetadataRevision,
    /// Whether pane is in alternate screen buffer.
    ///
    /// DEPRECATED: This field was populated by Lua status updates which were removed
    /// in v0.2.0. The authoritative source for alt-screen state is now
    /// `PaneCursor.in_alt_screen` which is populated via escape sequence detection.
    /// This field is kept for backward compatibility but is always `false`.
    pub is_alt_screen: bool,
    /// Timestamp of last status update (epoch ms).
    ///
    /// DEPRECATED: This field was populated by Lua status updates which were removed
    /// in v0.2.0. It is now always `None`. Kept for backward compatibility.
    pub last_status_at: Option<i64>,

    /// Optional operator-set priority override for capture scheduling.
    pub priority_override: Option<PanePriorityOverride>,
    /// Logical allocator arena reservation for this pane.
    pub pane_arena: PaneArena,
}

impl PaneEntry {
    fn revision_namespace_is_exhausted(&self) -> bool {
        (matches!(
            self.observation.ignore_reason(),
            Some("lifecycle_revision_exhausted")
        ) && self.lifecycle_revision.get() == u32::MAX)
            || (matches!(
                self.observation.ignore_reason(),
                Some("metadata_revision_exhausted")
            ) && self.metadata_revision.get() == u64::MAX)
    }

    /// Create a new pane entry
    ///
    /// Generates a per-runtime `pane_uuid` based on domain, pane_id, and creation time.
    /// The UUID is assigned once and never changes for this pane's lifetime.
    #[must_use]
    pub fn new(
        info: PaneInfo,
        lifecycle_identity: PaneLifecycleIdentity,
        observation: ObservationDecision,
        pane_arena: PaneArena,
    ) -> Self {
        let now = epoch_ms();
        let domain = info.inferred_domain();
        let pane_uuid = generate_pane_uuid(&domain, info.pane_id, now);

        Self {
            info,
            lifecycle_identity,
            observation,
            pane_uuid,
            first_seen_at: now,
            last_seen_at: now,
            decision_at: now,
            resume_next_seq: 0,
            lifecycle_revision: PaneLifecycleRevision::INITIAL,
            metadata_revision: PaneMetadataRevision::INITIAL,
            is_alt_screen: false,
            last_status_at: None,
            priority_override: None,
            pane_arena,
        }
    }

    /// Create a pane entry with a specific UUID (for recovery/testing)
    #[must_use]
    pub fn with_uuid(
        info: PaneInfo,
        lifecycle_identity: PaneLifecycleIdentity,
        observation: ObservationDecision,
        pane_arena: PaneArena,
        pane_uuid: String,
    ) -> Self {
        let now = epoch_ms();
        Self {
            info,
            lifecycle_identity,
            observation,
            pane_uuid,
            first_seen_at: now,
            last_seen_at: now,
            decision_at: now,
            resume_next_seq: 0,
            lifecycle_revision: PaneLifecycleRevision::INITIAL,
            metadata_revision: PaneMetadataRevision::INITIAL,
            is_alt_screen: false,
            last_status_at: None,
            priority_override: None,
            pane_arena,
        }
    }

    /// Install registry-classified pane info while preserving first-seen time.
    ///
    /// This stays private so callers cannot bypass lifecycle continuity and
    /// checked metadata-revision allocation in [`PaneRegistry::discovery_tick`].
    fn update_info(&mut self, info: PaneInfo) {
        self.info = info;
        self.last_seen_at = epoch_ms();
    }

    /// Approximate logical bytes owned by this pane entry.
    ///
    /// This deliberately tracks the dynamic strings/maps held by pane metadata
    /// rather than pretending we have true allocator arena isolation today.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + pane_info_dynamic_bytes(&self.info)
            + pane_lifecycle_identity_dynamic_bytes(&self.lifecycle_identity)
            + observation_dynamic_bytes(&self.observation)
            + self.pane_uuid.len()
    }

    // NOTE: update_from_status was removed in v0.2.0 to eliminate Lua performance bottleneck.
    // Alt-screen detection is now handled via escape sequence parsing (see screen_state.rs).
    // Pane metadata (title, dimensions, cursor) is obtained via `wezterm cli list`.

    /// Check if this pane should be observed
    #[must_use]
    pub fn should_observe(&self) -> bool {
        self.observation.is_observed()
    }

    /// Convert to a PaneRecord for storage persistence
    #[must_use]
    pub fn to_pane_record(&self) -> PaneRecord {
        PaneRecord {
            pane_id: self.info.pane_id,
            pane_uuid: Some(self.pane_uuid.clone()),
            domain: self.info.inferred_domain(),
            window_id: Some(self.info.window_id),
            tab_id: Some(self.info.tab_id),
            title: self.info.title.clone(),
            cwd: self.info.cwd.clone(),
            tty_name: self.info.tty_name.clone(),
            first_seen_at: self.first_seen_at,
            last_seen_at: self.last_seen_at,
            observed: self.observation.is_observed(),
            ignore_reason: self.observation.ignore_reason().map(ToString::to_string),
            last_decision_at: Some(self.decision_at),
        }
    }

    /// Get the pane UUID
    #[must_use]
    pub fn uuid(&self) -> &str {
        &self.pane_uuid
    }
}

fn option_string_len(value: Option<&String>) -> usize {
    value.map_or(0, String::len)
}

fn json_value_dynamic_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 0,
        serde_json::Value::String(text) => text.len(),
        serde_json::Value::Array(items) => items.iter().map(json_value_dynamic_bytes).sum(),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(key, nested)| key.len() + json_value_dynamic_bytes(nested))
            .sum(),
    }
}

fn pane_info_dynamic_bytes(info: &PaneInfo) -> usize {
    option_string_len(info.domain_name.as_ref())
        + option_string_len(info.workspace.as_ref())
        + option_string_len(info.title.as_ref())
        + option_string_len(info.cwd.as_ref())
        + option_string_len(info.tty_name.as_ref())
        + info
            .extra
            .iter()
            .map(|(key, value)| key.len() + json_value_dynamic_bytes(value))
            .sum::<usize>()
}

fn pane_lifecycle_identity_dynamic_bytes(identity: &PaneLifecycleIdentity) -> usize {
    option_string_len(identity.tty_name.as_ref())
}

fn observation_dynamic_bytes(observation: &ObservationDecision) -> usize {
    match observation {
        ObservationDecision::Observed => 0,
        ObservationDecision::Ignored { reason } => reason.len(),
    }
}

// =============================================================================
// Discovery Diff
// =============================================================================

/// Changes detected during a discovery tick
#[derive(Debug, Clone, Default)]
pub struct DiscoveryDiff {
    /// Newly discovered panes
    pub new_panes: Vec<u64>,
    /// Panes that have closed (no longer in WezTerm list)
    pub closed_panes: Vec<u64>,
    /// Checked latest-wins metadata changes, including title and cwd.
    pub metadata_changes: Vec<PaneMetadataChange>,
    /// Panes whose authoritative lifecycle evidence changed.
    pub lifecycle_replacements: Vec<u64>,
    /// Panes withheld because previously available lifecycle evidence vanished.
    pub ambiguous_lifecycle_panes: Vec<u64>,
    /// Panes withheld because a checked revision namespace was exhausted.
    pub revision_exhausted_panes: Vec<u64>,
    /// Panes that flipped `Ignored` -> `Observed` on this tick (ft-0kdi9).
    ///
    /// An already-tracked pane can re-enter observation at any time, because
    /// [`PaneRegistry::decide_observation`] re-runs the filter against the
    /// live title/cwd on every tick. Such a pane is *not* in
    /// [`Self::new_panes`] — the registry entry already existed — so a
    /// consumer that only creates per-pane capture state for `new_panes`
    /// silently never re-creates it. The observation runtime compacts its own
    /// cursor map against the observed set, so without this signal capture for
    /// the pane stays dead for the life of the process.
    pub re_observed_panes: Vec<u64>,
}

impl DiscoveryDiff {
    /// Check if there are any changes
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.new_panes.is_empty()
            && self.closed_panes.is_empty()
            && self.metadata_changes.is_empty()
            && self.lifecycle_replacements.is_empty()
            && self.ambiguous_lifecycle_panes.is_empty()
            && self.revision_exhausted_panes.is_empty()
            && self.re_observed_panes.is_empty()
    }

    /// Total number of changes
    #[must_use]
    pub fn change_count(&self) -> usize {
        self.new_panes.len()
            + self.closed_panes.len()
            + self.metadata_changes.len()
            + self.lifecycle_replacements.len()
            + self.ambiguous_lifecycle_panes.len()
            + self.revision_exhausted_panes.len()
            + self.re_observed_panes.len()
    }
}

// =============================================================================
// Pane Cursor Sequence Saturation Counter [ft-g8nbu]
// =============================================================================
//
// `PaneCursor::next_seq` is bumped via `saturating_add(1)`. At u64::MAX the
// saturation pins every subsequent capture's seq to MAX, breaking
// monotonic-uniqueness for downstream consumers that dedup or stitch on seq.
// This counter records every saturation event so forensic stitching can detect
// the (practically unreachable but semantically real) silent-collision class.
//
// Conservation contract:
//     sum_per_pane(captures) - count_distinct_seq_per_pane
//         == pane_cursor_seq_saturation_count()
//
// (Same "silent state loss → observable counter" pattern as
// mcp_audit_failure_count, policy_clock_anomaly_count, events_dropped_dedup.)

static PANE_CURSOR_SEQ_SATURATION_COUNT: AtomicU64 = AtomicU64::new(0);

fn record_pane_cursor_seq_saturation() {
    let _ = crate::try_update_atomic_u64(
        &PANE_CURSOR_SEQ_SATURATION_COUNT,
        Ordering::Relaxed,
        Ordering::Relaxed,
        |count| Some(count.saturating_add(1)),
    );
}

/// Number of times a `PaneCursor::next_seq` increment saturated at `u64::MAX`,
/// producing a duplicate seq for the resulting capture.
#[must_use]
pub fn pane_cursor_seq_saturation_count() -> u64 {
    PANE_CURSOR_SEQ_SATURATION_COUNT.load(Ordering::Relaxed)
}

/// Test-only reset of the saturation counter so tests don't observe
/// cross-test pollution.
#[cfg(test)]
pub fn reset_pane_cursor_seq_saturation_count_for_test() {
    PANE_CURSOR_SEQ_SATURATION_COUNT.store(0, Ordering::Relaxed);
}

/// Per-pane state for tracking capture position
#[derive(Debug, Clone)]
pub struct PaneCursor {
    /// Pane ID
    pub pane_id: u64,
    /// Next sequence number to assign for captured output
    pub next_seq: u64,
    /// Last captured snapshot (used for delta extraction)
    pub last_snapshot: String,
    /// Hash of last captured snapshot (diagnostic; future fast-path)
    pub last_hash: Option<u64>,
    /// Whether we're in a known gap state
    pub in_gap: bool,
    /// Whether we're currently in alternate screen buffer
    pub in_alt_screen: bool,
    /// Tail of already-persisted output, used exactly once to re-anchor the
    /// first capture on a cursor that resumed without a snapshot baseline
    /// (ft-6lso5).
    ///
    /// Private: consumed by [`Self::capture_snapshot`] on first use, and a
    /// caller that could overwrite it after capture started would silently
    /// re-anchor mid-stream. Set it with [`Self::with_resume_anchor`].
    resume_anchor: Option<String>,
    /// Cumulative correction applied to issued sequence numbers. Each capture
    /// retains its issuance-time value so queued generations stay distinguishable.
    seq_correction: i128,
}

/// The capture-advanced fields of a [`PaneCursor`], lifted out so the
/// observation runtime can hand them to
/// [`PaneRegistry::publish_live_cursor_state`] without holding both the
/// runtime cursor lock and the registry lock at the same time (ft-c87rx).
///
/// Deliberately excludes `last_snapshot`: it is unbounded pane text, the
/// registry has no use for it, and copying it every discovery tick for every
/// pane would be a real cost at fleet scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveCursorState {
    pub pane_id: u64,
    pub next_seq: u64,
    pub in_gap: bool,
    pub in_alt_screen: bool,
}

impl From<&PaneCursor> for LiveCursorState {
    fn from(cursor: &PaneCursor) -> Self {
        Self {
            pane_id: cursor.pane_id,
            next_seq: cursor.next_seq,
            in_gap: cursor.in_gap,
            in_alt_screen: cursor.in_alt_screen,
        }
    }
}

impl PaneCursor {
    /// Create a new cursor for a pane
    #[must_use]
    pub fn new(pane_id: u64) -> Self {
        Self {
            pane_id,
            next_seq: 0,
            last_snapshot: String::new(),
            last_hash: None,
            in_gap: false,
            in_alt_screen: false,
            resume_anchor: None,
            seq_correction: 0,
        }
    }

    /// Create a new cursor starting from a specific sequence number.
    #[must_use]
    pub fn from_seq(pane_id: u64, next_seq: u64) -> Self {
        Self {
            pane_id,
            next_seq,
            last_snapshot: String::new(),
            last_hash: None,
            in_gap: false,
            in_alt_screen: false,
            resume_anchor: None,
            seq_correction: 0,
        }
    }

    /// Attach the tail of already-persisted output so the first capture can be
    /// anchored against it instead of being re-emitted whole (ft-6lso5).
    ///
    /// A cursor built from a stored `next_seq` has no snapshot baseline, so
    /// `extract_delta` took the `previous.is_empty()` branch and returned the
    /// pane's entire current scrollback as a normal `Delta`. Every observed pane
    /// re-stored everything it had already stored on every daemon restart, with
    /// no gap marker to tell any consumer a discontinuity had occurred — search
    /// returned each line twice and replay showed each command twice.
    ///
    /// The anchor cannot simply be assigned to `last_snapshot`: delta extraction
    /// matches a *suffix* of the baseline against a *prefix* of the capture, and
    /// the persisted tail is the newest content while a capture's prefix is its
    /// oldest. The first capture instead locates the anchor inside the new text
    /// and emits only what follows it; see [`Self::capture_snapshot`].
    ///
    /// An empty anchor is ignored — nothing has been persisted, so there is
    /// nothing to resume from and the pane is genuinely new.
    #[must_use]
    pub fn with_resume_anchor(mut self, anchor: impl Into<String>) -> Self {
        let anchor = anchor.into();
        if !anchor.is_empty() {
            self.resume_anchor = Some(anchor);
        }
        self
    }

    /// Whether this cursor still carries an unconsumed resume anchor (ft-6lso5).
    #[must_use]
    pub fn has_resume_anchor(&self) -> bool {
        self.resume_anchor.is_some()
    }

    /// Get the last assigned sequence number.
    ///
    /// Returns -1 if no segments have been captured yet, otherwise
    /// returns `next_seq - 1`.
    #[must_use]
    pub fn last_seq(&self) -> i64 {
        if self.next_seq == 0 {
            -1
        } else {
            i64::try_from(self.next_seq - 1).unwrap_or(i64::MAX)
        }
    }

    /// Bump `next_seq` by one using `checked_add`, recording a saturation
    /// event in the process-wide counter when the increment overflows. [ft-g8nbu]
    ///
    /// `next_seq` was previously bumped via `saturating_add(1)`, which silently
    /// pinned the value at `u64::MAX` and produced duplicate seqs for every
    /// subsequent capture. The switch to `checked_add` makes the overflow
    /// branch explicit at the call site so future fixes (e.g. forcing a Gap
    /// segment with reason `seq_overflow_u64_max` instead of a duplicate
    /// emission) can attach without re-discovering where the silent failure
    /// lived.
    ///
    /// Current semantics on overflow are still saturating-equivalent (pin at
    /// `u64::MAX` and bump the counter) — the behaviour-preserving step. The
    /// counter is the load-bearing observability signal:
    /// [`pane_cursor_seq_saturation_count`].
    fn bump_next_seq(&mut self) {
        match self.next_seq.checked_add(1) {
            Some(next) => self.next_seq = next,
            None => {
                record_pane_cursor_seq_saturation();
                // self.next_seq is already u64::MAX; leaving it pinned
                // preserves the prior saturating_add behaviour.
            }
        }
    }

    /// Process a new pane snapshot and return a captured segment if something changed.
    ///
    /// This assigns a monotonically increasing per-pane sequence number (`seq`).
    ///
    /// # Gap Detection
    ///
    /// Gaps are detected in the following scenarios:
    /// 1. **Overlap failure**: Delta extraction couldn't find matching content
    /// 2. **Alt-screen toggle**: Detected `ESC[?1049h/l` or `ESC[?47h/l` sequences
    ///    indicating the terminal switched between normal and alternate screen buffers
    /// 3. **External state change**: `external_alt_screen` (from Lua IPC) differs from current state
    pub fn capture_snapshot(
        &mut self,
        current_snapshot: &str,
        overlap_size: usize,
        external_alt_screen: Option<bool>,
    ) -> Option<CapturedSegment> {
        if current_snapshot == self.last_snapshot && external_alt_screen.is_none() {
            return None;
        }

        let current_hash = hash_text(current_snapshot);

        // Check for alt-screen changes via text detection
        let alt_screen_changes = detect_alt_screen_changes(current_snapshot);

        // Determine the next state based on text detection first
        let mut next_state = self.in_alt_screen;

        for change in &alt_screen_changes {
            let s = match change {
                AltScreenChange::Entered => true,
                AltScreenChange::Exited => false,
            };

            if s != next_state {
                next_state = s;
            }
        }

        // If external authoritative state is provided, it overrides text detection
        let final_state = external_alt_screen.unwrap_or(next_state);
        // br-ft-6tevg: a balanced toggle pair within a single tick
        // (Entered + Exited in the same snapshot) computes the same
        // final state as the prior in_alt_screen but DID disrupt the
        // intervening content. The main-screen buffer may have been
        // overwritten and restored; delta extraction across this
        // boundary is unsound. Force a Gap with reason
        // `alt_screen_toggled` when alt_screen_changes is non-empty
        // even if final_state == self.in_alt_screen.
        //
        // The defect is currently DORMANT in production (tailer.rs
        // get_text(escapes=false) strips raw ESC sequences before
        // detect_alt_screen_changes runs). Future raw-capture paths
        // (recording / debug captures with escapes=true) would
        // expose the silent-loss class. This fix is hardening for
        // those callers + brings behaviour into agreement with the
        // function's documented intent (line 643+).
        let toggle_observed_within_tick =
            !alt_screen_changes.is_empty() && final_state == self.in_alt_screen;
        let actual_transition_occurred =
            final_state != self.in_alt_screen || toggle_observed_within_tick;

        // Update final state
        self.in_alt_screen = final_state;

        // Save old snapshot for comparison before updating
        let previous_snapshot = std::mem::take(&mut self.last_snapshot);

        // ft-6lso5: a cursor resumed from storage has no baseline, so the
        // ordinary path would classify the pane's whole scrollback as a fresh
        // delta and store it a second time. Anchor against what is already
        // persisted instead. The anchor is consumed here whether or not it
        // matched: it describes the state at resume time, and after this
        // capture `last_snapshot` is authoritative.
        let delta = match self.resume_anchor.take() {
            Some(anchor) if previous_snapshot.is_empty() => {
                resume_delta_from_anchor(&anchor, current_snapshot)
            }
            _ => extract_delta(&previous_snapshot, current_snapshot, overlap_size),
        };

        // Update snapshot state regardless; capture is derived from these snapshots.
        self.last_snapshot = current_snapshot.to_string();
        self.last_hash = Some(current_hash);

        // If alt-screen changed, force a gap even if delta extraction succeeded
        // because the content relationship is broken
        if actual_transition_occurred {
            self.in_gap = true;
            let seq = self.next_seq;
            self.bump_next_seq();

            // Determine reason. br-ft-6tevg: a balanced toggle
            // pair (Entered + Exited within the same tick) lands
            // here with final_state == self.in_alt_screen (i.e.,
            // no net state change but a real content disruption);
            // emit the dedicated `alt_screen_toggled` reason so
            // operators can distinguish it from clean enter/exit
            // transitions in forensic logs.
            let reason = if toggle_observed_within_tick {
                "alt_screen_toggled".to_string()
            } else if self.in_alt_screen {
                "alt_screen_entered".to_string()
            } else {
                "alt_screen_exited".to_string()
            };

            // If alt-screen changed, we must send the full current snapshot because
            // the consumer will treat the Gap as a reset. Any delta extracted relative
            // to the *previous* screen buffer is invalid and would result in data loss.
            let content = current_snapshot.to_string();

            return Some(CapturedSegment {
                pane_id: self.pane_id,
                seq,
                seq_correction: self.seq_correction,
                content,
                kind: CapturedSegmentKind::Gap { reason },
                captured_at: epoch_ms(),
            });
        }

        if current_snapshot == previous_snapshot {
            // If we reached here, it means no transition occurred, and content didn't change.
            // We early-returned at the top if external_alt_screen was None.
            // If external_alt_screen was Some but matched current state, we effectively have no change.
            return None;
        }

        match delta {
            DeltaResult::NoChange => None,
            DeltaResult::Content(content) => {
                self.in_gap = false;
                let seq = self.next_seq;
                self.bump_next_seq();
                Some(CapturedSegment {
                    pane_id: self.pane_id,
                    seq,
                    seq_correction: self.seq_correction,
                    content,
                    kind: CapturedSegmentKind::Delta,
                    captured_at: epoch_ms(),
                })
            }
            DeltaResult::Gap { reason, content } => {
                self.in_gap = true;
                let seq = self.next_seq;
                self.bump_next_seq();
                Some(CapturedSegment {
                    pane_id: self.pane_id,
                    seq,
                    seq_correction: self.seq_correction,
                    content,
                    kind: CapturedSegmentKind::Gap { reason },
                    captured_at: epoch_ms(),
                })
            }
        }
    }

    /// Resync cursor's sequence number to match storage after a discontinuity.
    ///
    /// Call this after `persist_captured_segment` returns a gap with reason
    /// containing "seq_discontinuity". The `storage_seq` should be the `seq`
    /// from the returned `PersistedCapture.segment`.
    ///
    /// After resyncing, subsequent captures will have sequence numbers that
    /// align with storage.
    ///
    /// Mirrors [`Self::bump_next_seq`]'s overflow semantics [ft-g8nbu]: a
    /// `storage_seq` of `u64::MAX` pins `next_seq` at `u64::MAX` and records
    /// the saturation in [`pane_cursor_seq_saturation_count`] so the
    /// observability signal fires at the resync that saturated, not on the
    /// next bump.
    pub fn resync_seq(&mut self, storage_seq: u64) {
        let previous_next_seq = self.next_seq;
        self.next_seq = match storage_seq.checked_add(1) {
            Some(next) => next,
            None => {
                record_pane_cursor_seq_saturation();
                u64::MAX
            }
        };
        self.seq_correction = self
            .seq_correction
            .saturating_add(i128::from(self.next_seq) - i128::from(previous_next_seq));
        self.in_gap = true;
    }

    /// Realign this producer's numbering with storage after a persisted
    /// segment came back with a different `seq` than it was captured with
    /// (ft-xxfwy.32).
    ///
    /// The cursor is shared with an asynchronous persistence loop: by the
    /// time a mismatch is observed, later segments have usually already been
    /// captured and queued with the old numbering. [`Self::resync_seq`]
    /// resets `next_seq` to `storage_seq + 1` regardless, so the next capture
    /// reuses a number that is still in flight and the mismatch reappears on
    /// every segment forever (12 of 12 in the first real observe run). This
    /// method instead treats the mismatch as an *offset* between the two
    /// numberings. The capture's issuance-time correction plus the observed
    /// difference gives the desired cumulative correction; only the difference
    /// from the cursor's current correction is applied. Old queued captures
    /// therefore cannot double-apply a shift, and a newly lost capture is still
    /// recognized even if no matching acknowledgement separated two losses.
    /// Call this for every persisted segment, including matching sequences.
    /// Returns the shift applied (0 when the correction was already applied).
    pub fn realign_next_seq(&mut self, captured: &CapturedSegment, storage_seq: u64) -> i128 {
        let observed = i128::from(storage_seq) - i128::from(captured.seq);
        let correction = captured.seq_correction.saturating_add(observed);
        let shift = correction.saturating_sub(self.seq_correction);
        if shift != 0 {
            let shifted = i128::from(self.next_seq)
                .saturating_add(shift)
                .max(i128::from(storage_seq) + 1);
            self.next_seq = if shifted > i128::from(u64::MAX) {
                record_pane_cursor_seq_saturation();
                u64::MAX
            } else {
                u64::try_from(shifted).unwrap_or(u64::MAX)
            };
            self.seq_correction = correction;
        }
        if captured.seq != storage_seq || shift != 0 {
            self.in_gap = true;
        }
        shift
    }

    /// Offset currently applied between this producer's numbering and
    /// storage's (`0` until a mismatch was observed).
    #[must_use]
    pub fn seq_correction(&self) -> i128 {
        self.seq_correction
    }

    /// Create a captured delta segment from raw content (native event path).
    ///
    /// This bypasses snapshot-based delta extraction and simply appends the
    /// provided content as a new segment with a monotonically increasing seq.
    pub fn capture_delta(&mut self, content: String, captured_at: i64) -> CapturedSegment {
        self.in_gap = false;
        let seq = self.next_seq;
        self.bump_next_seq();

        CapturedSegment {
            pane_id: self.pane_id,
            seq,
            seq_correction: self.seq_correction,
            content,
            kind: CapturedSegmentKind::Delta,
            captured_at,
        }
    }

    /// Capture the first snapshot of a replacement generation as one explicit
    /// resynchronization gap.
    ///
    /// The cursor is rebuilt from durability-confirmed state before this call.
    /// When its persisted tail is still visible, only bytes following that
    /// anchor are carried by the gap.  If the anchor has scrolled away (or no
    /// trusted anchor exists), the full visible snapshot is carried so the
    /// consumer converges without pretending the discarded predecessor queue
    /// was continuous.
    pub(crate) fn capture_generation_resync(
        &mut self,
        current_snapshot: &str,
        reason: &str,
    ) -> CapturedSegment {
        let (content, reason) = match self.resume_anchor.take() {
            Some(anchor) => match resume_delta_from_anchor(&anchor, current_snapshot) {
                DeltaResult::NoChange => (String::new(), reason.to_string()),
                DeltaResult::Content(content) => (content, reason.to_string()),
                DeltaResult::Gap {
                    reason: anchor_reason,
                    content,
                } => (content, format!("{reason}:{anchor_reason}")),
            },
            None => (
                current_snapshot.to_string(),
                format!("{reason}:durable_anchor_unavailable"),
            ),
        };

        self.last_snapshot = current_snapshot.to_string();
        self.last_hash = Some(hash_text(current_snapshot));
        self.in_gap = true;
        let seq = self.next_seq;
        self.bump_next_seq();

        CapturedSegment {
            pane_id: self.pane_id,
            seq,
            seq_correction: self.seq_correction,
            content,
            kind: CapturedSegmentKind::Gap { reason },
            captured_at: epoch_ms(),
        }
    }

    /// Emit a gap segment with the provided reason.
    pub fn emit_gap(&mut self, reason: &str) -> CapturedSegment {
        self.in_gap = true;
        let seq = self.next_seq;
        self.bump_next_seq();
        CapturedSegment {
            pane_id: self.pane_id,
            seq,
            seq_correction: self.seq_correction,
            content: String::new(),
            kind: CapturedSegmentKind::Gap {
                reason: reason.to_string(),
            },
            captured_at: epoch_ms(),
        }
    }

    /// Emit a synthetic gap due to backpressure overflow.
    ///
    /// Called by the tailer when consecutive backpressure events exceed the
    /// overflow threshold, indicating that capture data was likely lost.
    /// The gap has empty content because no snapshot was captured during the
    /// overflow period.
    pub fn emit_overflow_gap(&mut self, reason: &str) -> CapturedSegment {
        self.emit_gap(reason)
    }

    /// Alias for `capture_snapshot` for backward compatibility.
    pub fn capture(&mut self, content: &str, overlap_size: usize) -> Option<CapturedSegment> {
        self.capture_snapshot(content, overlap_size, None)
    }
}

/// Pane registry for tracking discovered panes with lifecycle management
pub struct PaneRegistry {
    /// Extended pane entries with lifecycle identity and observation state
    entries: HashMap<u64, PaneEntry>,
    /// Reverse index: pane_uuid -> pane_id
    uuid_index: HashMap<String, u64>,
    /// Cursors for each pane (delta extraction state)
    cursors: HashMap<u64, PaneCursor>,
    /// Per-pane trauma guard state (recent command + error-signature history)
    trauma_states: HashMap<u64, TraumaState>,
    /// Runtime trauma guard tuning and enablement.
    trauma_guard_config: TraumaGuardConfig,
    /// Pane filter configuration (cached)
    filter_config: PaneFilterConfig,
    /// Logical per-pane allocator arena reservations.
    ///
    /// br-ft-rsv5b: callsites that update tracked bytes via
    /// `pane_arenas.set_tracked_bytes(pane_id, _)` swallow the
    /// `Option<PaneArenaStats>` return with `let _ = ...`. The `None`
    /// case means the pane is no longer reserved (released or never
    /// allocated); silent skip is intentional best-effort accounting —
    /// the next `discovery_tick` reconciles state if the pane returns.
    /// See the upstream contract on `PaneArenaRegistry::set_tracked_bytes`.
    pane_arenas: PaneArenaRegistry,
    /// Reusable scratch space for panes closed in the current discovery tick.
    closed_panes_scratch: Vec<u64>,
    /// Operational telemetry counters
    telemetry: IngestTelemetry,
}

impl Default for PaneRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneRegistry {
    /// Create a new empty registry
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            uuid_index: HashMap::new(),
            cursors: HashMap::new(),
            trauma_states: HashMap::new(),
            trauma_guard_config: TraumaGuardConfig::default(),
            filter_config: PaneFilterConfig::default(),
            pane_arenas: PaneArenaRegistry::new(),
            closed_panes_scratch: Vec::new(),
            telemetry: IngestTelemetry::new(),
        }
    }

    /// Create a registry with filter configuration
    #[must_use]
    pub fn with_filter(filter_config: PaneFilterConfig) -> Self {
        Self::with_filter_and_trauma(filter_config, TraumaGuardConfig::default())
    }

    /// Create a registry with filter and trauma-guard configuration.
    #[must_use]
    pub fn with_filter_and_trauma(
        filter_config: PaneFilterConfig,
        trauma_guard_config: TraumaGuardConfig,
    ) -> Self {
        Self {
            entries: HashMap::new(),
            uuid_index: HashMap::new(),
            cursors: HashMap::new(),
            trauma_states: HashMap::new(),
            trauma_guard_config,
            filter_config,
            pane_arenas: PaneArenaRegistry::new(),
            closed_panes_scratch: Vec::new(),
            telemetry: IngestTelemetry::new(),
        }
    }

    /// Update the filter configuration and return exact metadata states
    /// produced by observation-decision changes.
    ///
    /// Filter policy is mutable metadata, not lifecycle authority. Returning
    /// these changes lets callers persist the new `observed`/`ignore_reason`
    /// state without treating a policy transition as a pane restart.
    pub fn set_filter(&mut self, filter_config: PaneFilterConfig) -> Vec<PaneMetadataChange> {
        self.filter_config = filter_config;

        let pane_ids: Vec<u64> = self.entries.keys().copied().collect();
        let mut metadata_changes = Vec::new();
        for pane_id in pane_ids {
            let before = self
                .entries
                .get(&pane_id)
                .map(|entry| (entry.metadata_revision, entry.observation.clone()));
            self.re_evaluate_observation(pane_id);
            if let Some(entry) = self.entries.get(&pane_id) {
                if before.is_some_and(|(revision, observation)| {
                    revision != entry.metadata_revision || observation != entry.observation
                }) {
                    let mut diff = PaneMetadataDiff::default();
                    diff.include_observation_change();
                    metadata_changes.push(PaneMetadataChange {
                        pane_id,
                        lifecycle_revision: entry.lifecycle_revision,
                        metadata_revision: entry.metadata_revision,
                        diff,
                    });
                }
            }
        }
        metadata_changes.sort_unstable_by_key(|change| change.pane_id);
        metadata_changes
    }

    /// Update trauma-guard tuning and apply it to tracked panes.
    pub fn set_trauma_guard_config(&mut self, trauma_guard_config: TraumaGuardConfig) {
        if self.trauma_guard_config == trauma_guard_config {
            return;
        }
        self.trauma_guard_config = trauma_guard_config;

        // Reinitialize per-pane state to deterministically apply the new thresholds.
        // This intentionally drops prior loop history across live panes on config change.
        let trauma_state_config = self.trauma_guard_config.to_trauma_config();
        for state in self.trauma_states.values_mut() {
            *state = TraumaState::with_config(trauma_state_config.clone());
        }
    }

    /// Set or update a runtime capture priority override for a pane.
    ///
    /// Returns the installed override if the pane is known.
    pub fn set_priority_override(
        &mut self,
        pane_id: u64,
        priority: u32,
        ttl_ms: Option<u64>,
    ) -> Result<PanePriorityOverride> {
        let Some(entry) = self.entries.get_mut(&pane_id) else {
            return Err(crate::Error::Wezterm(
                crate::error::WeztermError::PaneNotFound(pane_id),
            ));
        };

        let now = epoch_ms();
        let expires_at = ttl_ms.and_then(|ttl| {
            if ttl == 0 {
                None
            } else {
                i64::try_from(ttl)
                    .ok()
                    .and_then(|ttl_i64| now.checked_add(ttl_i64))
            }
        });

        let override_state = PanePriorityOverride {
            priority,
            set_at: now,
            expires_at,
        };
        entry.priority_override = Some(override_state.clone());
        let tracked_bytes = entry.estimated_bytes();
        let _ = self.pane_arenas.set_tracked_bytes(pane_id, tracked_bytes);
        Ok(override_state)
    }

    /// Clear any runtime capture priority override for a pane.
    pub fn clear_priority_override(&mut self, pane_id: u64) -> Result<()> {
        let Some(entry) = self.entries.get_mut(&pane_id) else {
            return Err(crate::Error::Wezterm(
                crate::error::WeztermError::PaneNotFound(pane_id),
            ));
        };
        entry.priority_override = None;
        let tracked_bytes = entry.estimated_bytes();
        let _ = self.pane_arenas.set_tracked_bytes(pane_id, tracked_bytes);
        Ok(())
    }

    /// Remove any expired priority overrides.
    ///
    /// Returns the number of overrides cleared.
    pub fn purge_expired_priority_overrides(&mut self, now_ms: i64) -> usize {
        let mut cleared = 0usize;
        let mut tracked_byte_updates = Vec::new();
        for (pane_id, entry) in &mut self.entries {
            let Some(ref ov) = entry.priority_override else {
                continue;
            };
            if ov.expires_at.is_some_and(|exp| exp <= now_ms) {
                entry.priority_override = None;
                cleared = cleared.saturating_add(1);
                tracked_byte_updates.push((*pane_id, entry.estimated_bytes()));
            }
        }
        for (pane_id, tracked_bytes) in tracked_byte_updates {
            let _ = self.pane_arenas.set_tracked_bytes(pane_id, tracked_bytes);
        }
        cleared
    }

    /// List active priority overrides for observed panes.
    ///
    /// Expired overrides are not returned (but are not purged here).
    #[must_use]
    pub fn list_active_priority_overrides(&self, now_ms: i64) -> Vec<(u64, PanePriorityOverride)> {
        let mut overrides = Vec::new();
        for (pane_id, entry) in &self.entries {
            if !entry.should_observe() {
                continue;
            }
            let Some(ov) = entry.priority_override.clone() else {
                continue;
            };
            if ov.expires_at.is_some_and(|exp| exp <= now_ms) {
                continue;
            }
            overrides.push((*pane_id, ov));
        }
        overrides.sort_by_key(|(pane_id, _)| *pane_id);
        overrides
    }

    /// Perform a discovery tick: update registry with new pane list
    ///
    /// Returns a diff describing what changed.
    pub fn discovery_tick(&mut self, panes: Vec<PaneInfo>) -> DiscoveryDiff {
        let mut diff = DiscoveryDiff::default();
        let mut seen: HashSet<u64> = HashSet::with_capacity(panes.len());
        let mut duplicate_ids = HashSet::new();
        for pane in &panes {
            if !seen.insert(pane.pane_id) {
                duplicate_ids.insert(pane.pane_id);
            }
        }

        for pane in panes {
            let pane_id = pane.pane_id;
            if duplicate_ids.contains(&pane_id) {
                continue;
            }
            let mut new_observation = self.decide_observation(&pane);

            match self.entries.entry(pane_id) {
                Entry::Occupied(mut occupied) => {
                    let entry = occupied.get_mut();
                    if entry.revision_namespace_is_exhausted() {
                        // Checked revision exhaustion is a terminal state for
                        // this tracked incarnation. Keep presence/accounting
                        // fresh, but never manufacture an unversioned recovery
                        // or restage the same terminal storage write forever.
                        entry.update_info(pane);
                        let tracked_bytes = entry.estimated_bytes();
                        let _ = self.pane_arenas.set_tracked_bytes(pane_id, tracked_bytes);
                        continue;
                    }
                    let next_identity = PaneLifecycleIdentity::from_pane_info(&pane);
                    let mut metadata_diff = PaneMetadataDiff::between(&entry.info, &pane);
                    let was_observed = entry.should_observe();
                    let mut lifecycle_replaced = false;
                    let mut lifecycle_revision_exhausted = false;
                    match entry.lifecycle_identity.continuity_with(&next_identity) {
                        PaneLifecycleContinuity::Same => {
                            entry
                                .lifecycle_identity
                                .merge_available_evidence(&next_identity);
                        }
                        PaneLifecycleContinuity::Replaced => {
                            if let Some(revision) = entry.lifecycle_revision.checked_next() {
                                entry.lifecycle_identity = next_identity;
                                entry.lifecycle_revision = revision;
                                entry.metadata_revision = PaneMetadataRevision::INITIAL;
                                diff.lifecycle_replacements.push(pane_id);
                                lifecycle_replaced = true;
                                entry.decision_at = epoch_ms();
                            } else {
                                diff.revision_exhausted_panes.push(pane_id);
                                lifecycle_revision_exhausted = true;
                                new_observation = ObservationDecision::Ignored {
                                    reason: "lifecycle_revision_exhausted".to_string(),
                                };
                            }
                        }
                        PaneLifecycleContinuity::Ambiguous => {
                            diff.ambiguous_lifecycle_panes.push(pane_id);
                            new_observation = ObservationDecision::Ignored {
                                reason: "lifecycle_identity_ambiguous".to_string(),
                            };
                        }
                    }

                    if entry.observation != new_observation {
                        metadata_diff.include_observation_change();
                    }
                    if !lifecycle_replaced
                        && !lifecycle_revision_exhausted
                        && !metadata_diff.is_empty()
                    {
                        if let Some(revision) = entry.metadata_revision.checked_next() {
                            entry.metadata_revision = revision;
                            diff.metadata_changes.push(PaneMetadataChange {
                                pane_id,
                                lifecycle_revision: entry.lifecycle_revision,
                                metadata_revision: revision,
                                diff: metadata_diff,
                            });
                        } else {
                            diff.revision_exhausted_panes.push(pane_id);
                            new_observation = ObservationDecision::Ignored {
                                reason: "metadata_revision_exhausted".to_string(),
                            };
                        }
                    }

                    let is_observed = new_observation.is_observed();

                    entry.update_info(pane);

                    if entry.observation != new_observation {
                        entry.observation = new_observation;
                        entry.decision_at = epoch_ms();
                    }

                    if is_observed && !was_observed {
                        // ft-0kdi9: report the resumption so the observation
                        // runtime can re-create the capture-side state it
                        // dropped when this pane went unobserved. The registry
                        // cursor below is not the one the tailer reads.
                        diff.re_observed_panes.push(pane_id);
                        self.cursors.insert(
                            pane_id,
                            PaneCursor::from_seq(pane_id, entry.resume_next_seq),
                        );
                    } else if !is_observed
                        && was_observed
                        && let Some(cursor) = self.cursors.remove(&pane_id)
                    {
                        entry.resume_next_seq = cursor.next_seq;
                    }

                    let tracked_bytes = entry.estimated_bytes();
                    let _ = self.pane_arenas.set_tracked_bytes(pane_id, tracked_bytes);
                }
                Entry::Vacant(vacant) => {
                    diff.new_panes.push(pane_id);

                    let lifecycle_identity = PaneLifecycleIdentity::from_pane_info(&pane);
                    let pane_arena = self.pane_arenas.reserve(pane_id).arena();

                    let entry =
                        PaneEntry::new(pane, lifecycle_identity, new_observation, pane_arena);
                    let tracked_bytes = entry.estimated_bytes();
                    let should_observe = entry.should_observe();
                    self.uuid_index.insert(entry.pane_uuid.clone(), pane_id);
                    vacant.insert(entry);
                    let _ = self.pane_arenas.set_tracked_bytes(pane_id, tracked_bytes);
                    self.trauma_states.insert(
                        pane_id,
                        TraumaState::with_config(self.trauma_guard_config.to_trauma_config()),
                    );

                    if should_observe {
                        self.cursors.insert(pane_id, PaneCursor::new(pane_id));
                    } else {
                        self.telemetry.record_pane_filtered();
                    }
                }
            }
        }

        let mut duplicate_ids = duplicate_ids.into_iter().collect::<Vec<_>>();
        duplicate_ids.sort_unstable();
        for pane_id in duplicate_ids {
            diff.ambiguous_lifecycle_panes.push(pane_id);
            let Some(entry) = self.entries.get_mut(&pane_id) else {
                // A duplicate first sighting has no identity safe to retain.
                // Do not create an entry from either arbitrary row; a later
                // unique listing will be admitted as a new pane.
                continue;
            };
            let was_observed = entry.should_observe();
            let mut new_observation = ObservationDecision::Ignored {
                reason: "duplicate_pane_identity".to_string(),
            };
            let mut metadata_diff = PaneMetadataDiff::default();
            if entry.observation != new_observation {
                metadata_diff.include_observation_change();
                if let Some(revision) = entry.metadata_revision.checked_next() {
                    entry.metadata_revision = revision;
                    diff.metadata_changes.push(PaneMetadataChange {
                        pane_id,
                        lifecycle_revision: entry.lifecycle_revision,
                        metadata_revision: revision,
                        diff: metadata_diff,
                    });
                } else {
                    diff.revision_exhausted_panes.push(pane_id);
                    new_observation = ObservationDecision::Ignored {
                        reason: "metadata_revision_exhausted".to_string(),
                    };
                }
                entry.observation = new_observation;
                entry.decision_at = epoch_ms();
            }
            if was_observed && let Some(cursor) = self.cursors.remove(&pane_id) {
                entry.resume_next_seq = cursor.next_seq;
            }
            let tracked_bytes = entry.estimated_bytes();
            let _ = self.pane_arenas.set_tracked_bytes(pane_id, tracked_bytes);
        }

        let mut closed_panes = std::mem::take(&mut self.closed_panes_scratch);
        closed_panes.clear();
        closed_panes.extend(self.entries.keys().filter(|id| !seen.contains(id)).copied());

        for pane_id in closed_panes.iter().copied() {
            diff.closed_panes.push(pane_id);
            // Remove UUID from index before removing entry
            if let Some(entry) = self.entries.get(&pane_id) {
                self.uuid_index.remove(&entry.pane_uuid);
            }
            self.entries.remove(&pane_id);
            self.cursors.remove(&pane_id);
            self.trauma_states.remove(&pane_id);
            self.pane_arenas.release(pane_id);
        }
        closed_panes.clear();
        self.closed_panes_scratch = closed_panes;

        self.telemetry.record_discovery_tick(&diff);

        diff
    }

    /// Simple update without diff tracking (for backward compatibility)
    pub fn update(&mut self, panes: Vec<PaneInfo>) {
        let _ = self.discovery_tick(panes);
    }

    /// Decide whether to observe a pane based on filter rules
    fn decide_observation(&self, pane: &PaneInfo) -> ObservationDecision {
        let domain = pane.inferred_domain();
        let title = pane.title.as_deref().unwrap_or("");
        let cwd = pane.cwd.as_deref().unwrap_or("");

        self.filter_config
            .check_pane(&domain, title, cwd)
            .map_or(ObservationDecision::Observed, |reason| {
                ObservationDecision::Ignored { reason }
            })
    }

    /// Get all tracked pane IDs
    #[must_use]
    pub fn pane_ids(&self) -> Vec<u64> {
        self.entries.keys().copied().collect()
    }

    /// Lookup the logical allocator arena for a pane.
    #[must_use]
    pub fn pane_arena(&self, pane_id: u64) -> Option<PaneArena> {
        self.pane_arenas.get(pane_id)
    }

    /// Number of active logical pane-arena reservations.
    #[must_use]
    pub fn pane_arena_count(&self) -> usize {
        self.pane_arenas.count()
    }

    /// Snapshot of active pane-arena reservations sorted by pane id.
    #[must_use]
    pub fn pane_arenas_snapshot(&self) -> Vec<PaneArena> {
        self.pane_arenas.snapshot()
    }

    /// Current accounting snapshot for a pane arena.
    #[must_use]
    pub fn pane_arena_stats(&self, pane_id: u64) -> Option<PaneArenaStats> {
        self.pane_arenas.stats(pane_id)
    }

    /// Snapshot of active pane-arena reservations with logical byte accounting.
    #[must_use]
    pub fn pane_arena_stats_snapshot(&self) -> Vec<PaneArenaSnapshot> {
        self.pane_arenas.stats_snapshot()
    }

    /// Get only observed pane IDs (for tailing)
    #[must_use]
    pub fn observed_pane_ids(&self) -> Vec<u64> {
        self.entries
            .iter()
            .filter(|(_, e)| e.should_observe())
            .map(|(id, _)| *id)
            .collect()
    }

    /// Get pane entry by ID
    #[must_use]
    pub fn get_entry(&self, pane_id: u64) -> Option<&PaneEntry> {
        self.entries.get(&pane_id)
    }

    /// Get mutable pane entry by ID for crate-internal diagnostics and tests.
    ///
    /// External mutation would bypass lifecycle and metadata revision
    /// authority, so production consumers use the classified registry APIs.
    #[cfg(test)]
    pub(crate) fn get_entry_mut(&mut self, pane_id: u64) -> Option<&mut PaneEntry> {
        self.entries.get_mut(&pane_id)
    }

    /// Get pane info by ID (convenience method)
    #[must_use]
    pub fn get_pane(&self, pane_id: u64) -> Option<&PaneInfo> {
        self.entries.get(&pane_id).map(|e| &e.info)
    }

    /// Get pane_id by UUID
    #[must_use]
    pub fn get_pane_id_by_uuid(&self, uuid: &str) -> Option<u64> {
        self.uuid_index.get(uuid).copied()
    }

    /// Get pane entry by UUID
    #[must_use]
    pub fn get_entry_by_uuid(&self, uuid: &str) -> Option<&PaneEntry> {
        self.uuid_index
            .get(uuid)
            .and_then(|pane_id| self.entries.get(pane_id))
    }

    /// Get pane info by UUID (convenience method)
    #[must_use]
    pub fn get_pane_by_uuid(&self, uuid: &str) -> Option<&PaneInfo> {
        self.get_entry_by_uuid(uuid).map(|e| &e.info)
    }

    /// Get cursor for a pane
    #[must_use]
    pub fn get_cursor(&self, pane_id: u64) -> Option<&PaneCursor> {
        self.cursors.get(&pane_id)
    }

    /// Get mutable cursor for a pane
    pub fn get_cursor_mut(&mut self, pane_id: u64) -> Option<&mut PaneCursor> {
        self.cursors.get_mut(&pane_id)
    }

    /// Publish live capture state from the observation runtime into the
    /// registry's cursors (ft-c87rx).
    ///
    /// The registry owns the *observation lifecycle* of a cursor — creating it
    /// when a pane becomes observed, retiring it into `resume_next_seq` when it
    /// stops — but the capture pipeline advances a separate map, so the fields
    /// that change during capture were frozen at their initial values for the
    /// life of the process. That is a policy fail-open, not just stale
    /// telemetry: `plan.rs` feeds `in_alt_screen` and `in_gap` into
    /// `PaneCapabilities`, so a rule meant to refuse a send into an
    /// alt-screen app, or into a pane with a recent capture gap, was
    /// evaluating a permanent `false`.
    ///
    /// Called once per discovery tick from a snapshot taken under the runtime
    /// cursor lock, so the two locks are never held at once and the published
    /// state is at most one discovery interval stale. Only panes the registry
    /// already tracks are updated; a snapshot entry for an untracked pane is
    /// ignored rather than resurrecting a retired cursor.
    pub fn publish_live_cursor_state(&mut self, snapshot: &[LiveCursorState]) {
        for live in snapshot {
            if let Some(cursor) = self.cursors.get_mut(&live.pane_id) {
                cursor.next_seq = live.next_seq;
                cursor.in_gap = live.in_gap;
                cursor.in_alt_screen = live.in_alt_screen;
            }
        }
    }

    /// Get trauma guard state for a pane.
    #[must_use]
    pub fn get_trauma_state(&self, pane_id: u64) -> Option<&TraumaState> {
        self.trauma_states.get(&pane_id)
    }

    /// Get mutable trauma guard state for a pane.
    pub fn get_trauma_state_mut(&mut self, pane_id: u64) -> Option<&mut TraumaState> {
        self.trauma_states.get_mut(&pane_id)
    }

    /// Record a command result in the pane's trauma guard state.
    pub fn record_trauma_command_result(
        &mut self,
        pane_id: u64,
        timestamp_ms: u64,
        command: &str,
        error_signatures: &[String],
    ) -> Result<TraumaDecision> {
        if !self.entries.contains_key(&pane_id) {
            return Err(crate::Error::Wezterm(
                crate::error::WeztermError::PaneNotFound(pane_id),
            ));
        }

        if !self.trauma_guard_config.enabled {
            return Ok(TraumaDecision {
                should_intervene: false,
                reason_code: None,
                command_hash: hash_command(command),
                repeat_count: 0,
                recurring_signatures: Vec::new(),
            });
        }

        let trauma_state_config = self.trauma_guard_config.to_trauma_config();
        let state = self
            .trauma_states
            .entry(pane_id)
            .or_insert_with(|| TraumaState::with_config(trauma_state_config));
        Ok(state.record_command_result(timestamp_ms, command, error_signatures))
    }

    /// Count panes with an allocated trauma guard state.
    #[must_use]
    pub fn trauma_state_count(&self) -> usize {
        self.trauma_states.len()
    }

    /// Re-evaluate observation decision for a pane (e.g., after filter change)
    ///
    /// Returns the transition that occurred. A `Resumed` result means the
    /// caller must re-create any capture-side per-pane state it owns outside
    /// this registry — see [`ObservationTransition`] and ft-0kdi9.
    pub fn re_evaluate_observation(&mut self, pane_id: u64) -> ObservationTransition {
        // Clone the PaneInfo to avoid borrow conflicts
        let pane_info = match self.entries.get(&pane_id) {
            Some(entry) => entry.info.clone(),
            None => return ObservationTransition::Unchanged,
        };

        let new_decision = self.decide_observation(&pane_info);

        let Some(entry) = self.entries.get_mut(&pane_id) else {
            return ObservationTransition::Unchanged;
        };

        if entry.revision_namespace_is_exhausted() {
            // A filter refresh cannot allocate the missing checked revision;
            // reviving capture here would bypass the exhaustion fence.
            return ObservationTransition::Unchanged;
        }

        let was_observed = entry.should_observe();
        if entry.observation != new_decision {
            if let Some(revision) = entry.metadata_revision.checked_next() {
                entry.metadata_revision = revision;
                entry.observation = new_decision;
            } else {
                // Admission metadata without a representable successor
                // revision cannot be published safely. Retire capture and pin
                // this incarnation in the same terminal state used by the
                // discovery path.
                entry.observation = ObservationDecision::Ignored {
                    reason: "metadata_revision_exhausted".to_string(),
                };
            }
            entry.decision_at = epoch_ms();
        }
        let is_observed = entry.should_observe();

        // Update cursor state
        let transition = if is_observed && !was_observed {
            // Now observed - resume monotonic sequencing instead of restarting at zero.
            self.cursors.insert(
                pane_id,
                PaneCursor::from_seq(pane_id, entry.resume_next_seq),
            );
            ObservationTransition::Resumed
        } else if !is_observed && was_observed {
            // Now ignored - preserve the next sequence number for future resumption.
            if let Some(cursor) = self.cursors.remove(&pane_id) {
                entry.resume_next_seq = cursor.next_seq;
            }
            ObservationTransition::Retired
        } else {
            ObservationTransition::Unchanged
        };
        let tracked_bytes = entry.estimated_bytes();
        let _ = self.pane_arenas.set_tracked_bytes(pane_id, tracked_bytes);
        transition
    }

    /// Adopt an existing stable UUID for a pane (e.g. recovered from storage).
    ///
    /// This updates the pane entry and the reverse lookup index.
    /// Returns `true` if successful, `false` if pane not found.
    pub fn adopt_uuid(&mut self, pane_id: u64, new_uuid: String) -> bool {
        if let Some(existing_owner) = self.uuid_index.get(&new_uuid) {
            if *existing_owner != pane_id {
                warn!(
                    "UUID collision during adoption: {} is already owned by pane {}",
                    new_uuid, existing_owner
                );
                return false;
            }
        }

        let Some(entry) = self.entries.get_mut(&pane_id) else {
            return false;
        };

        if entry.pane_uuid == new_uuid {
            return true;
        }

        let old_uuid = std::mem::replace(&mut entry.pane_uuid, new_uuid.clone());
        self.uuid_index.remove(&old_uuid);
        self.uuid_index.insert(new_uuid, pane_id);
        true
    }

    /// Get all entries as an iterator
    pub fn entries(&self) -> impl Iterator<Item = (&u64, &PaneEntry)> {
        self.entries.iter()
    }

    /// Get pane count
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if registry is empty
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get all pane records for persistence
    ///
    /// Converts all tracked pane entries to PaneRecord format
    /// suitable for storage in the database.
    #[must_use]
    pub fn to_pane_records(&self) -> Vec<PaneRecord> {
        self.entries
            .values()
            .map(PaneEntry::to_pane_record)
            .collect()
    }

    /// Get pane records for observed panes only
    #[must_use]
    pub fn observed_pane_records(&self) -> Vec<PaneRecord> {
        self.entries
            .values()
            .filter(|e| e.should_observe())
            .map(PaneEntry::to_pane_record)
            .collect()
    }

    /// Get pane records for ignored panes only
    #[must_use]
    pub fn ignored_pane_records(&self) -> Vec<PaneRecord> {
        self.entries
            .values()
            .filter(|e| !e.should_observe())
            .map(PaneEntry::to_pane_record)
            .collect()
    }

    // NOTE: update_from_status was removed in v0.2.0 to eliminate Lua performance bottleneck.
    // Alt-screen detection is now handled via escape sequence parsing (see screen_state.rs).
    // Pane metadata (title, dimensions, cursor) is obtained via `wezterm cli list`.

    /// Get the alt-screen state for a pane (authoritative only).
    ///
    /// Returns `None` when we don't have an authoritative status update.
    /// This avoids forcing a false value that would override text-based
    /// alt-screen detection in the capture pipeline.
    #[must_use]
    pub fn is_alt_screen(&self, pane_id: u64) -> Option<bool> {
        self.entries.get(&pane_id).and_then(|e| {
            if e.last_status_at.is_some() {
                Some(e.is_alt_screen)
            } else {
                None
            }
        })
    }

    /// Access the operational telemetry counters.
    #[must_use]
    pub fn telemetry(&self) -> &IngestTelemetry {
        &self.telemetry
    }
}

/// Delta extraction result
#[derive(Debug)]
pub enum DeltaResult {
    /// New content extracted
    Content(String),
    /// No new content
    NoChange,
    /// Gap detected - overlap failed or content was modified in-place
    Gap { reason: String, content: String },
}

/// A captured segment derived from successive pane snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedSegment {
    /// Pane id
    pub pane_id: u64,
    /// Producer sequence number. Corrections can rebase this value; interpret
    /// queued captures together with their issuance-time `seq_correction`.
    pub seq: u64,
    /// Cumulative correction already applied when this sequence was issued.
    /// Preserve this through truncation, queuing, and cloning; standalone
    /// synthetic captures that have never been corrected use zero.
    pub seq_correction: i128,
    /// Captured content (delta or full snapshot when `Gap`)
    pub content: String,
    /// Segment kind
    pub kind: CapturedSegmentKind,
    /// Timestamp when the capture was taken (epoch ms)
    pub captured_at: i64,
}

/// Whether a captured segment is a differential delta or a full gap snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapturedSegmentKind {
    /// Delta extracted from overlap
    Delta,
    /// Full snapshot emitted due to discontinuity
    Gap { reason: String },
}

/// Result of persisting a captured segment.
#[derive(Debug, Clone)]
pub struct PersistedCapture {
    /// Stored segment row
    pub segment: Segment,
    /// Gap row if the capture represented a discontinuity
    pub gap: Option<Gap>,
}

/// Safety rail for persisted capture payload size.
///
/// This keeps per-segment storage, FTS, and regex detection work bounded even
/// if a pane emits pathological bursts of output.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SegmentSizeEnforcement {
    original_bytes: usize,
    kept_bytes: usize,
    max_bytes: usize,
}

fn trim_utf8_tail_to_max_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    if max_bytes == 0 {
        return String::new();
    }

    let mut start = text.len().saturating_sub(max_bytes);
    // Snap forward to the next valid UTF-8 char boundary so we don't
    // slice in the middle of a multi-byte code point.
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    // [ft-a0up5] If snapping consumed all remaining bytes — i.e. the
    // last character is wider than max_bytes and there's no shorter
    // suffix that fits — return empty rather than the previous
    // "last full character" fallback. The pre-fix fallback returned
    // the entire last character even when it exceeded max_bytes
    // (e.g. trim("中", 1) returned 3 bytes), violating the function's
    // declared cap. Callers that need at-least-one-character semantics
    // must explicitly handle empty here; persistence-side
    // enforce_segment_size_for_persistence already wraps the oversize
    // case in a Gap segment via the truncation_reason path, so an
    // empty-content gap is a sound default — operators see "this
    // pane's snapshot exceeded max_segment_bytes" without us secretly
    // exceeding the configured cap.
    if start >= text.len() {
        return String::new();
    }
    text[start..].to_string()
}

fn enforce_segment_size_for_persistence(
    captured: &CapturedSegment,
    max_segment_bytes: usize,
) -> (CapturedSegment, Option<SegmentSizeEnforcement>) {
    if max_segment_bytes == 0 || captured.content.len() <= max_segment_bytes {
        return (captured.clone(), None);
    }

    let truncated_content = trim_utf8_tail_to_max_bytes(&captured.content, max_segment_bytes);
    let detail = SegmentSizeEnforcement {
        original_bytes: captured.content.len(),
        kept_bytes: truncated_content.len(),
        max_bytes: max_segment_bytes,
    };
    let truncation_reason = format!(
        "segment_truncated:original_bytes={},max_bytes={}",
        detail.original_bytes, detail.max_bytes
    );

    let kind = match &captured.kind {
        CapturedSegmentKind::Gap { reason } => CapturedSegmentKind::Gap {
            reason: format!("{reason};{truncation_reason}"),
        },
        CapturedSegmentKind::Delta => CapturedSegmentKind::Gap {
            reason: truncation_reason,
        },
    };

    (
        CapturedSegment {
            pane_id: captured.pane_id,
            seq: captured.seq,
            seq_correction: captured.seq_correction,
            content: truncated_content,
            kind,
            captured_at: captured.captured_at,
        },
        Some(detail),
    )
}

/// Return the capture payload bounded to the configured persistence size limit.
///
/// This is used by callers that need deterministic downstream behavior (for
/// example, bounded regex detection work) to match persistence semantics.
#[must_use]
pub(crate) fn bounded_segment_for_persistence(
    captured: &CapturedSegment,
    max_segment_bytes: usize,
) -> CapturedSegment {
    let (bounded, _) = enforce_segment_size_for_persistence(captured, max_segment_bytes);
    bounded
}

/// Append a captured segment into the crash-safe scrollback file writer.
///
/// This shares [`persist_captured_segment`]'s configured size bound so the
/// mmap sidecar and SQLite persistence see the same truncated payload.
pub fn append_captured_segment_to_mmap_scrollback(
    writer: &mut crate::scrollback_mmap_writer::MmapScrollback,
    captured: &CapturedSegment,
    max_segment_bytes: usize,
) -> std::result::Result<
    crate::scrollback_mmap_writer::MmapAppendReport,
    crate::scrollback_mmap_writer::MmapScrollbackError,
> {
    let bounded_segment = bounded_segment_for_persistence(captured, max_segment_bytes);
    writer.append(
        crate::scrollback_mmap_format::RecordKind::Text,
        bounded_segment.content.as_bytes(),
    )
}

/// Apply secret-pattern redaction to segment content before persistence.
///
/// ft-gd4za: production scrollback persistence (SQLite output_segments +
/// mmap mirror) was bypassing redaction entirely. Applying the redactor
/// here is the single chokepoint that covers both downstream sinks.
fn redact_segment_for_persist(content: &str) -> String {
    crate::redactor::Redactor::new().redact(content)
}

fn ingest_cancelled_error(operation: &'static str, err: impl std::fmt::Display) -> crate::Error {
    crate::Error::RuntimeOperation {
        operation,
        source: RuntimeOperationSource::Cancelled(err.to_string()),
    }
}

/// Persist a captured segment and optional gap into storage.
///
/// The pane must already exist in storage (use `upsert_pane` elsewhere).
///
/// # Redaction (ft-gd4za)
///
/// Segment content is passed through [`crate::redactor::Redactor`] before
/// the storage write so that the SQLite `output_segments.content` column
/// and the per-pane mmap log file never contain unredacted credentials,
/// API keys, or other matched secret patterns. The bytes returned to
/// callers in [`PersistedCapture::segment.content`] are likewise the
/// redacted form, since `append_segment` returns the content it stored.
///
/// # Gap Recording
///
/// Gaps are recorded in two scenarios:
/// 1. **Overlap failure**: When `captured.kind` is `Gap`, the original gap reason
///    (e.g., "overlap_not_found") is recorded.
/// 2. **Sequence discontinuity**: When the storage's sequence number doesn't match
///    the cursor's expected sequence, an additional "seq_discontinuity" gap is recorded.
///
/// After a sequence discontinuity, callers should resync their cursor's `next_seq`
/// to `stored.segment.seq + 1` to prevent further mismatches.
pub async fn persist_captured_segment(
    storage: &StorageHandle,
    captured: &CapturedSegment,
    max_segment_bytes: usize,
) -> Result<PersistedCapture> {
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    persist_captured_segment_with_cx(&cx, storage, captured, max_segment_bytes).await
}

/// Persist a captured segment with optional semantic zone metadata.
///
/// The `zone_type` stamp is additive metadata only; it is ignored for gap
/// segments so discontinuity rows remain explicitly untyped.
pub async fn persist_captured_segment_with_zone(
    storage: &StorageHandle,
    captured: &CapturedSegment,
    max_segment_bytes: usize,
    zone_type: Option<&str>,
) -> Result<PersistedCapture> {
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    persist_captured_segment_with_zone_with_cx(&cx, storage, captured, max_segment_bytes, zone_type)
        .await
}

/// Cx-first [`persist_captured_segment`] (ft-xbnl0.2.3).
///
/// Tick 196: upgraded from pre-flight-only wrapper to a fully
/// cx-threaded body. Every inner storage call now routes through
/// its `_with_cx` sibling:
///   - `storage.record_gap_with_cx(cx, ...)` for the initial gap.
///   - `storage.append_segment_with_cx(cx, ...)` for the main write.
///   - `storage.record_gap_with_cx(cx, ...)` for the truncation gap.
///   - `storage.record_gap_with_cx(cx, ...)` for the discontinuity gap.
///
/// Cancellation now propagates into each mpsc backpressure reserve
/// (send_with_cx, tick 176) and each storage pre-flight checkpoint,
/// so an operator who cancels mid-ingest bails at the next await
/// boundary rather than blocking until the whole chain drains.
///
/// Legacy [`persist_captured_segment`] preserved for ambient-cx
/// callers and non-asupersync builds.
pub async fn persist_captured_segment_with_cx(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    captured: &CapturedSegment,
    max_segment_bytes: usize,
) -> Result<PersistedCapture> {
    persist_captured_segment_with_zone_with_cx(cx, storage, captured, max_segment_bytes, None).await
}

/// Cx-first [`persist_captured_segment_with_zone`].
pub async fn persist_captured_segment_with_zone_with_cx(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    captured: &CapturedSegment,
    max_segment_bytes: usize,
    zone_type: Option<&str>,
) -> Result<PersistedCapture> {
    persist_captured_segment_with_zone_and_guard_with_cx(
        cx,
        storage,
        captured,
        max_segment_bytes,
        zone_type,
        None,
        None,
    )
    .await
}

/// Persist one already-admitted capture event while delegating authority to
/// each storage-writer command that may outlive this async caller.
pub(crate) async fn persist_authorized_captured_segment_with_zone_with_cx(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    captured: &CapturedSegment,
    max_segment_bytes: usize,
    zone_type: Option<&str>,
    guard: &crate::capture_authority::CapturePersistenceGuard,
) -> Result<PersistedCapture> {
    persist_captured_segment_with_zone_and_guard_with_cx(
        cx,
        storage,
        captured,
        max_segment_bytes,
        zone_type,
        None,
        Some(guard),
    )
    .await
}

/// Persist one admitted capture and atomically enqueue its selected-recorder
/// obligation after storage's stateful redaction has finalized the text.
pub(crate) async fn persist_authorized_captured_segment_with_zone_and_recorder_delivery_with_cx(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    captured: &CapturedSegment,
    max_segment_bytes: usize,
    zone_type: Option<&str>,
    recorder_delivery: crate::storage::RecorderDeliverySeed,
    guard: &crate::capture_authority::CapturePersistenceGuard,
) -> Result<PersistedCapture> {
    persist_captured_segment_with_zone_and_guard_with_cx(
        cx,
        storage,
        captured,
        max_segment_bytes,
        zone_type,
        Some(recorder_delivery),
        Some(guard),
    )
    .await
}

async fn persist_captured_segment_with_zone_and_guard_with_cx(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    captured: &CapturedSegment,
    max_segment_bytes: usize,
    zone_type: Option<&str>,
    recorder_delivery: Option<crate::storage::RecorderDeliverySeed>,
    guard: Option<&crate::capture_authority::CapturePersistenceGuard>,
) -> Result<PersistedCapture> {
    cx.checkpoint()
        .map_err(|err| ingest_cancelled_error("persist_captured_segment", err))?;

    let (bounded_segment, truncation) =
        enforce_segment_size_for_persistence(captured, max_segment_bytes);

    if let Some(detail) = truncation.as_ref() {
        warn!(
            pane_id = bounded_segment.pane_id,
            seq = bounded_segment.seq,
            original_bytes = detail.original_bytes,
            kept_bytes = detail.kept_bytes,
            max_bytes = detail.max_bytes,
            "Captured segment exceeded max bytes and was truncated with explicit GAP"
        );
    }

    // This call runs BEFORE the append for a reason (ft-5yi36): `record_gap`
    // derives the gap's bounds from `MAX(seq)` for the pane, so afterwards the
    // new segment is itself the maximum and the bounds come out one segment too
    // late. `None` here means the pane has no prior segment — see the retry
    // below for why that case is still recorded despite imprecise bounds.
    let gap = match &bounded_segment.kind {
        CapturedSegmentKind::Gap { reason } => {
            record_gap_with_optional_capture_hold(
                cx,
                storage,
                bounded_segment.pane_id,
                reason,
                guard,
            )
            .await?
        }
        CapturedSegmentKind::Delta => None,
    };

    let redacted_content = redact_segment_for_persist(&bounded_segment.content);
    let stored_zone_type = match &bounded_segment.kind {
        CapturedSegmentKind::Delta => zone_type,
        CapturedSegmentKind::Gap { .. } => None,
    };
    let stored = append_segment_with_optional_capture_hold(
        cx,
        storage,
        bounded_segment.pane_id,
        &redacted_content,
        stored_zone_type,
        recorder_delivery,
        guard,
    )
    .await?;

    // ft-5yi36 (open, analysis recorded here so it is not re-litigated): this
    // retry fires only when the pre-append call returned `None`, which happens
    // only when the pane had no prior segment. The bounds it then produces are
    // imprecise — `append_segment` has just assigned seq 0, so `MAX(seq)` is 0
    // and the row lands as (seq_before=0, seq_after=1), which reads as "content
    // is missing between segments 0 and 1" when the discontinuity actually
    // precedes segment 0.
    //
    // Dropping the retry to avoid the imprecise row was tried and REVERTED: it
    // makes a pane's first-ever gap, and the truncation gap of a pane's first
    // oversized segment, record nothing at all — no `output_gaps` row and no
    // `GapDetected` event, since the runtime publishes that event only when this
    // field is `Some`. Silently losing the only durable record of a
    // discontinuity is the exact failure class the capture pipeline exists to
    // prevent, and three tests pin it deliberately
    // (`fresh_eyes_persist_initial_gap_records_gap_after_first_segment_exists`,
    // its `_with_cx` sibling, and
    // `persist_captured_oversized_delta_records_truncation_gap`).
    //
    // A correct fix needs an honest encoding for a leading gap, which
    // `output_gaps` cannot express today: `seq_before` and `seq_after` are both
    // NOT NULL and the explicit-bounds path requires `seq_after > seq_before`,
    // so "nothing precedes this" has no representation. That is a schema and
    // consumer-contract decision, not a local edit.
    let mut gap = gap;

    if gap.is_none()
        && let CapturedSegmentKind::Gap { reason } = &bounded_segment.kind
    {
        gap = record_gap_with_optional_capture_hold(
            cx,
            storage,
            bounded_segment.pane_id,
            reason,
            guard,
        )
        .await?;
    }

    if stored.seq != bounded_segment.seq {
        let discontinuity_reason = format!(
            "seq_discontinuity:expected={},actual={}",
            bounded_segment.seq, stored.seq
        );
        let discontinuity_gap = record_gap_with_optional_capture_hold(
            cx,
            storage,
            bounded_segment.pane_id,
            &discontinuity_reason,
            guard,
        )
        .await?;

        if gap.is_none() {
            gap = discontinuity_gap;
        }
    }

    Ok(PersistedCapture {
        segment: stored,
        gap,
    })
}

async fn record_gap_with_optional_capture_hold(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    pane_id: u64,
    reason: &str,
    guard: Option<&crate::capture_authority::CapturePersistenceGuard>,
) -> Result<Option<Gap>> {
    match guard {
        Some(guard) => {
            storage
                .record_capture_gap_with_cx(cx, pane_id, reason, guard.delegate_storage()?)
                .await
        }
        None => storage.record_gap_with_cx(cx, pane_id, reason).await,
    }
}

async fn append_segment_with_optional_capture_hold(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    pane_id: u64,
    content: &str,
    zone_type: Option<&str>,
    recorder_delivery: Option<crate::storage::RecorderDeliverySeed>,
    guard: Option<&crate::capture_authority::CapturePersistenceGuard>,
) -> Result<Segment> {
    retry_capture_append(cx, || async {
        // Each admitted attempt consumes its own delegated hold. Keep the
        // parent authority and immutable recorder identity across safe retries.
        match (guard, recorder_delivery.clone()) {
            (Some(guard), Some(recorder_delivery)) => {
                storage
                    .append_captured_segment_with_recorder_delivery_with_cx(
                        cx,
                        pane_id,
                        content,
                        None,
                        zone_type,
                        recorder_delivery,
                        guard.delegate_storage()?,
                    )
                    .await
            }
            (Some(guard), None) => {
                storage
                    .append_captured_segment_with_zone_with_cx(
                        cx,
                        pane_id,
                        content,
                        None,
                        zone_type,
                        guard.delegate_storage()?,
                    )
                    .await
            }
            (None, None) => {
                storage
                    .append_segment_with_zone_with_cx(cx, pane_id, content, None, zone_type)
                    .await
            }
            (None, Some(_)) => Err(crate::Error::Storage(crate::error::StorageError::Database(
                "recorder delivery seed requires capture persistence authority".to_string(),
            ))),
        }
    })
    .await
}

/// Retry only an append whose writer proved BEGIN never acquired a transaction.
/// Never wrap the complete persistence flow: a later gap write may fail after
/// the segment and recorder delivery have already committed.
async fn retry_capture_append<F, Fut>(cx: &crate::cx::Cx, mut append: F) -> Result<Segment>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Segment>>,
{
    const BACKOFF_MS: [u64; 2] = [50, 100];
    let mut retries = 0;
    loop {
        cx.checkpoint()
            .map_err(|err| ingest_cancelled_error("capture_append_retry", err))?;
        if retries > 0 {
            metrics::counter!("capture.persist.retries").increment(1);
        }
        // Do not race or time out an admitted write: its authoritative response
        // must settle before another attempt can be considered.
        let error = match append().await {
            Ok(segment) => return Ok(segment),
            Err(error) => error,
        };
        if !matches!(
            error,
            crate::Error::Storage(crate::error::StorageError::WriterBusyNotCommitted)
        ) {
            return Err(error);
        }
        let Some(&base_ms) = BACKOFF_MS.get(retries) else {
            metrics::counter!("capture.persist.retry_exhausted").increment(1);
            metrics::counter!("capture.persist.dropped").increment(1);
            return Err(error);
        };
        cx.checkpoint()
            .map_err(|err| ingest_cancelled_error("capture_append_retry", err))?;
        let delay =
            std::time::Duration::from_millis(rand::rng().random_range(base_ms..=base_ms * 2));
        // This budget-aware timer also supports explicitly delegated contexts.
        // Jitter avoids synchronized fleet retries. Cancellation is checked on
        // both sides, bounding its delay to 200 ms (300 ms total backoff).
        crate::runtime_async::sleep_with_cx(cx, delay)
            .await
            .map_err(|error| crate::Error::RuntimeOperation {
                operation: "capture_append_retry_backoff",
                source: RuntimeOperationSource::Backend(error),
            })?;
        retries += 1;
    }
}

fn hash_text(text: &str) -> u64 {
    stable_hash(text.as_bytes())
}

/// Overlap length above which continuity is taken on faith (ft-r5xkf).
///
/// At or beyond this many bytes an exact suffix/prefix match is strong enough
/// evidence on its own; below it the overlap has to carry some content (see
/// [`overlap_is_plausible`]).
pub const MIN_PLAUSIBLE_OVERLAP_BYTES: usize = 8;

/// Whether a suffix/prefix match is evidence that two snapshots are continuous
/// (ft-r5xkf).
///
/// The overlap search accepts any byte-border down to length one, and one byte
/// is not evidence of anything. `get_text` output is newline-terminated and a
/// leading blank line is routine after `clear` or a terminal reset, so
/// `previous = "…line B\n"` against `current = "\n$ fresh\n"` matched on the
/// single shared `\n` and was reported as a clean `Content` delta. When
/// thousands of lines scroll off between polls and the boundary bytes happen to
/// coincide, that silently loses the scrollback and records no gap — defeating
/// the explicit-gap guarantee the capture pipeline is built on.
///
/// The discriminator is what the overlap *is*, not how long it is. A pure
/// length floor is wrong: `"hello world"` -> `"world peace"` overlaps by five
/// bytes and `"x🚀"` -> `"🚀y"` by four, and both are genuine continuations this
/// crate pins as such. What the false match has instead is a boundary-noise
/// overlap: a single byte, or nothing but whitespace — exactly the characters
/// that sit at the end of one capture and the start of the next by default.
///
/// So: accept a long overlap unconditionally, and accept a short one only when
/// it carries at least one non-whitespace byte and is more than a single byte.
///
/// Plausibility is monotone in the direction callers need. A shorter border is
/// a prefix of a longer one, so if the maximal border is implausible (one byte,
/// or all whitespace) every shorter border is too — checking the maximal border
/// alone is sufficient, in both search arms.
fn overlap_is_plausible(overlap: &str) -> bool {
    overlap.len() >= MIN_PLAUSIBLE_OVERLAP_BYTES
        || (overlap.len() >= 2 && overlap.bytes().any(|byte| !byte.is_ascii_whitespace()))
}

/// Bytes of already-persisted output kept as a resume anchor (ft-6lso5).
///
/// Long enough that a match inside a scrollback is not a coincidence — the
/// anchor is located by substring search, so it has to be distinctive — and
/// short enough that loading it per pane at startup is cheap. Roughly forty
/// terminal lines.
pub const RESUME_ANCHOR_BYTES: usize = 4 * 1024;

/// Delta for the first capture on a cursor resumed from storage (ft-6lso5).
///
/// The anchor is the tail of what has already been persisted for this pane, so
/// everything after its last occurrence in the new capture is exactly the output
/// that arrived while nothing was watching. Uses the *last* occurrence: a tail
/// that repeats (a prompt line, a progress banner) must resume from the most
/// recent one, otherwise the intervening output would be stored twice.
///
/// A missing anchor means the persisted tail is no longer in the pane's
/// scrollback — it scrolled off, or the pane was cleared. That is a real
/// discontinuity and is reported as one rather than being papered over.
#[must_use]
fn resume_delta_from_anchor(anchor: &str, current: &str) -> DeltaResult {
    match current.rfind(anchor) {
        Some(position) => {
            let delta = &current[position + anchor.len()..];
            if delta.is_empty() {
                // Everything the pane still shows is already stored.
                DeltaResult::NoChange
            } else {
                DeltaResult::Content(delta.to_string())
            }
        }
        None => DeltaResult::Gap {
            reason: "resume_anchor_not_found".to_string(),
            content: current.to_string(),
        },
    }
}

/// Trailing `max_bytes` of `text`, snapped to a UTF-8 boundary (ft-6lso5).
#[must_use]
pub fn resume_anchor_tail(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Extract delta from current vs previous content.
///
/// This is designed for the "sliding window" case (polling successive snapshots):
/// it finds the largest overlap where a suffix of `previous` matches a prefix of `current`.
///
/// A border that [`overlap_is_plausible`] rejects is treated as boundary noise
/// and reported as an explicit gap (ft-r5xkf).
#[must_use]
pub fn extract_delta(previous: &str, current: &str, overlap_size: usize) -> DeltaResult {
    // ft-li2hc: the search window has to be able to reach a whole snapshot,
    // because a sliding capture needs a border of `previous.len() - scrolled`
    // bytes — it grows with the SNAPSHOT, not with the amount of new output. A
    // window that large is unsafe for the legacy nested-memchr search, whose
    // worst case is quadratic when the first byte repeats across the window
    // (a screen of box-drawing characters or a progress bar does exactly that).
    // The KMP search is single-pass and proven byte-identical to the legacy one
    // (see `kmp_longest_overlap` and
    // `proptest_ingest_delta_linear_overlap_equivalence`), so it is used
    // whenever the window is large enough for the quadratic path to be a risk,
    // independently of the `FT_MOONSHOT_DELTA_LINEAR_OVERLAP` gate.
    let window = overlap_size.min(previous.len()).min(current.len());
    let linear = delta_linear_overlap_enabled() || window >= LINEAR_OVERLAP_SEARCH_THRESHOLD_BYTES;
    extract_delta_with_overlap_mode(previous, current, overlap_size, linear)
}

/// Window size at or above which the overlap search always uses the linear
/// (KMP) path rather than the legacy nested-memchr scan (ft-li2hc).
///
/// Below this the quadratic worst case is bounded by a small constant and the
/// legacy path stays in charge, preserving the shipping default for every
/// existing small-window caller.
pub const LINEAR_OVERLAP_SEARCH_THRESHOLD_BYTES: usize = 64 * 1024;

/// Q3 moonshot gate env var (`ingest.delta_linear_overlap`, default false).
///
/// Mirrors the existing `FT_MOONSHOT_*` knobs (e.g.
/// `FT_MOONSHOT_INSTANCED_GLYPH_QUADS`). When *set* (any value, incl. empty),
/// [`extract_delta`] runs the single-pass KMP overlap search; when *unset*
/// (the shipping default) it runs the legacy nested-memchr quadratic search.
const FT_MOONSHOT_DELTA_LINEAR_OVERLAP: &str = "FT_MOONSHOT_DELTA_LINEAR_OVERLAP";

static DELTA_LINEAR_OVERLAP_ENABLED: LazyLock<bool> = LazyLock::new(|| {
    // FT_MOONSHOT_ALL master switch (round-5 "everything-on" test build) enables
    // every FT_MOONSHOT_* gate at once. Default-off / revert-safe.
    std::env::var_os(FT_MOONSHOT_DELTA_LINEAR_OVERLAP).is_some()
        || std::env::var_os("FT_MOONSHOT_ALL").is_some()
});

/// Whether the `ingest.delta_linear_overlap` Q3 moonshot gate is enabled
/// (default `false`). Read once from `FT_MOONSHOT_DELTA_LINEAR_OVERLAP`.
#[must_use]
pub fn delta_linear_overlap_enabled() -> bool {
    *DELTA_LINEAR_OVERLAP_ENABLED
}

/// Test/bench entry point: run [`extract_delta`]'s logic with the overlap-search
/// algorithm forced, bypassing the `ingest.delta_linear_overlap` gate. This lets
/// the equivalence property test drive both arms in a single process.
///
/// `linear == false` is the shipping default (legacy quadratic nested-memchr
/// search); `linear == true` is the Q3 single-pass KMP search. The two are proven
/// byte-equivalent (identical [`DeltaResult`], including `reason` strings) for all
/// `&str` inputs — see `proptest_ingest_delta_linear_overlap_equivalence`.
#[doc(hidden)]
#[must_use]
pub fn extract_delta_with_overlap_mode(
    previous: &str,
    current: &str,
    overlap_size: usize,
    linear: bool,
) -> DeltaResult {
    // FND-002 / MT8: per-frame self-time (no-op unless `hot-path-metrics`).
    let _hpt = crate::hot_path_metrics::HotPathTimer::start("ingest.extract_delta");
    if previous == current {
        return DeltaResult::NoChange;
    }

    if previous.is_empty() {
        return DeltaResult::Content(current.to_string());
    }

    // Fast path: pure append (current starts with previous)
    // This handles the common case efficiently (O(N)) and avoids the overlap limit
    if current.len() > previous.len()
        && current.starts_with(previous)
        && current.is_char_boundary(previous.len())
    {
        return DeltaResult::Content(current[previous.len()..].to_string());
    }
    // If boundary check fails (should be very rare if starts_with matched), fall through to full check

    // br-ft-baaex: split the previously-conflated
    // `overlap_size_zero_or_current_empty` reason into the two
    // semantically distinct causes. Order matters when both hold:
    // `current_empty` is the diagnosable downstream symptom
    // (capture-source error or terminal-clear), so check it first
    // even if overlap_size is also zero.
    if current.is_empty() {
        return DeltaResult::Gap {
            reason: "current_empty".to_string(),
            content: String::new(),
        };
    }
    if overlap_size == 0 {
        return DeltaResult::Gap {
            reason: "overlap_size_zero".to_string(),
            content: current.to_string(),
        };
    }

    // Limit overlap search to a bounded suffix/prefix window.
    let max_overlap = overlap_size.min(previous.len()).min(current.len());
    // ft-li2hc: remember whether the CAP is what bounded the search, rather than
    // the operands. When it is, a failed search means "this window could not
    // reach far enough back", which is a different fact from "these two
    // snapshots share no border" and must not be reported as the same reason:
    // the first is a configuration limit an operator can raise, the second is
    // real content loss.
    let window_truncated_search = max_overlap < previous.len().min(current.len());
    let no_border_reason = if window_truncated_search {
        "overlap_window_exhausted"
    } else {
        "overlap_not_found"
    };
    let mut search_start = previous.len() - max_overlap;
    // Snap forward to the next valid UTF-8 char boundary to avoid panicking
    // on multi-byte characters (Cyrillic=2B, box-drawing=3B, emoji=4B).
    while search_start < previous.len() && !previous.is_char_boundary(search_start) {
        search_start += 1;
    }
    let search_window = &previous[search_start..];

    // Q3 moonshot (gate `ingest.delta_linear_overlap`, default off): a single-pass
    // KMP longest-suffix(search_window)/prefix(current) match replaces the legacy
    // nested-memchr + per-candidate slice-compare below. Both select the *same*
    // maximal overlap length on valid UTF-8 (proof in `kmp_longest_overlap`), so the
    // resulting `DeltaResult` — including every `reason` string — is byte-identical.
    if linear {
        return match kmp_longest_overlap(search_window.as_bytes(), current.as_bytes()) {
            // ft-r5xkf: the maximal border is the best evidence available, so if
            // even it is boundary noise, no acceptable border exists.
            Some(overlap_len) if !overlap_is_plausible(&current[..overlap_len]) => {
                DeltaResult::Gap {
                    reason: "overlap_implausible".to_string(),
                    content: current.to_string(),
                }
            }
            Some(overlap_len) => {
                // `overlap_len` is always a `current` char boundary for a true
                // byte-border (see `kmp_longest_overlap`), so this slice never panics.
                let delta = &current[overlap_len..];
                if delta.is_empty() {
                    DeltaResult::Gap {
                        reason: "content_changed_without_append".to_string(),
                        content: current.to_string(),
                    }
                } else {
                    DeltaResult::Content(delta.to_string())
                }
            }
            None => DeltaResult::Gap {
                reason: no_border_reason.to_string(),
                content: current.to_string(),
            },
        };
    }

    // Safety: current is known not to be empty from check above
    let first_char = current.as_bytes()[0];

    // Find all occurrences of first_char in search_window using memchr (SIMD-optimized)
    // We iterate from left to right (smallest pos -> largest overlap)
    for pos in memchr::memchr_iter(first_char, search_window.as_bytes()) {
        // memchr returns byte offsets — skip if not on a char boundary
        if !search_window.is_char_boundary(pos) {
            continue;
        }
        // Candidate overlap starts at pos relative to search_window
        let overlap_len = search_window.len() - pos;

        if overlap_len > current.len() || !current.is_char_boundary(overlap_len) {
            continue;
        }

        // Check full match
        // search_window[pos..] has length overlap_len
        // current[..overlap_len] has length overlap_len
        if search_window[pos..] == current[..overlap_len] {
            // ft-r5xkf: candidates are examined longest-first, so this is the
            // maximal border. If even it is boundary noise, every shorter
            // candidate is too, and no acceptable border exists.
            if !overlap_is_plausible(&current[..overlap_len]) {
                return DeltaResult::Gap {
                    reason: "overlap_implausible".to_string(),
                    content: current.to_string(),
                };
            }

            let delta = &current[overlap_len..];
            if delta.is_empty() {
                return DeltaResult::Gap {
                    reason: "content_changed_without_append".to_string(),
                    content: current.to_string(),
                };
            }

            return DeltaResult::Content(delta.to_string());
        }
    }

    DeltaResult::Gap {
        reason: no_border_reason.to_string(),
        content: current.to_string(),
    }
}

/// Single-pass longest-suffix(`text`)/prefix(`pattern`) overlap length, computed
/// with the Knuth–Morris–Pratt prefix-function. Returns the largest `L >= 1` such
/// that `text[text.len() - L..] == pattern[..L]` (byte equality), or `None` when no
/// non-empty overlap exists. Here `text` is the bounded `search_window` (a suffix of
/// `previous`) and `pattern` is `current`.
///
/// Cost is `O(text.len() + min(text.len(), pattern.len()))` — a single forward pass —
/// replacing the legacy `for pos in memchr_iter(..) { slice_compare }` loop whose worst
/// case is `O(text.len() * pattern.len())` when the first byte repeats across the window.
///
/// # Byte-equivalence to the quadratic search
///
/// For valid UTF-8 inputs this returns *exactly* the overlap length the legacy loop
/// selects, so the two `extract_delta` arms are observably identical:
///
/// * The legacy loop returns the first (smallest-`pos`, i.e. largest-`overlap_len`)
///   memchr hit whose `search_window[pos..] == current[..overlap_len]`. That is the
///   maximal byte-border — exactly what KMP's final automaton state encodes.
/// * The loop's two char-boundary guards never exclude a true byte-border: the matched
///   first byte is `current[0]`, always a UTF-8 *leading* byte, so `pos` lands on a
///   `search_window` char boundary; and `current[..L]` shares its bytes with the valid
///   `search_window[pos..]` suffix, so `current[..L]` is itself valid UTF-8 — i.e. `L`
///   is a `current` char boundary. Both guards are therefore always satisfied for a real
///   match, and the accepted set equals the set of byte-borders.
fn kmp_longest_overlap(text: &[u8], pattern: &[u8]) -> Option<usize> {
    // A border cannot exceed either operand; clamp the pattern to that bound. Any
    // prefix of `current` long enough to be a suffix of `text` is `<= text.len()`.
    let cap = text.len().min(pattern.len());
    if cap == 0 {
        return None;
    }
    let pat = &pattern[..cap];

    // KMP prefix-function (failure links) of the clamped pattern.
    let mut fail = vec![0usize; cap];
    let mut k = 0usize;
    for i in 1..cap {
        while k > 0 && pat[i] != pat[k] {
            k = fail[k - 1];
        }
        if pat[i] == pat[k] {
            k += 1;
        }
        fail[i] = k;
    }

    // Stream `text` through the automaton. After the final byte, `state` is the length
    // of the longest prefix of `pat` that is a suffix of `text` — the maximal overlap.
    // The `state == cap` guard short-circuits before indexing `pat[cap]` (out of bounds)
    // and falls back via the failure link, mirroring KMP occurrence resumption.
    let mut state = 0usize;
    for &b in text {
        while state > 0 && (state == cap || pat[state] != b) {
            state = fail[state - 1];
        }
        if state < cap && pat[state] == b {
            state += 1;
        }
    }

    if state == 0 { None } else { Some(state) }
}

// =============================================================================
// Output Cache (Memory-Efficient Deduplication)
// =============================================================================

/// Configuration for the output cache.
#[derive(Debug, Clone)]
pub struct OutputCacheConfig {
    /// Maximum number of content hashes to store in the global LRU
    pub global_lru_capacity: usize,
    /// Maximum age for per-pane state before pruning (milliseconds)
    pub per_pane_max_age_ms: u64,
}

impl Default for OutputCacheConfig {
    fn default() -> Self {
        Self {
            global_lru_capacity: 1024,
            per_pane_max_age_ms: 5 * 60 * 1000, // 5 minutes
        }
    }
}

/// Per-pane cache state for tracking content changes.
#[derive(Debug, Clone)]
struct PaneCacheState {
    /// Hash of the last seen content
    content_hash: u64,
    /// Content length (secondary discriminator)
    content_len: usize,
    /// Last update timestamp (epoch ms)
    last_updated: i64,
}

/// Global LRU entry: last-seen timestamp plus the recency generation that
/// stamps this hash's current `lru_order` token (ft-zo4hw).
#[derive(Debug, Clone, Copy)]
struct GlobalHashEntry {
    /// Last access timestamp (epoch ms) — used by `prune` for age eviction.
    last_seen: i64,
    /// Recency generation of this hash's live `lru_order` token. Older tokens
    /// for the same hash carry smaller generations and are skipped as stale.
    generation: u64,
}

/// Memory-efficient output cache for skipping redundant processing.
///
/// Uses two complementary mechanisms:
/// 1. Global LRU of content hashes - deduplicates across panes
/// 2. Per-pane rolling hash state - fast per-pane deduplication
#[derive(Debug)]
pub struct OutputCache {
    config: OutputCacheConfig,
    global_hashes: HashMap<u64, GlobalHashEntry>,
    /// LRU order tracking as `(hash, generation)` tokens, front = least
    /// recently used. Eviction pops the front in O(1). Access refreshes
    /// recency in O(1) amortized by pushing a fresh token and lazily skipping
    /// the superseded one on pop (ft-zo4hw); previously this was an O(n)
    /// position-scan + `VecDeque::remove` per hit (ft-fesg7).
    lru_order: VecDeque<(u64, u64)>,
    /// Monotonic recency counter stamping `lru_order` tokens.
    lru_generation: u64,
    pane_states: HashMap<u64, PaneCacheState>,
    hits: u64,
    misses: u64,
}

// Perf-regression instrumentation (ft-zo4hw / ft-wo323): number of
// `lru_order` tokens *examined* during LRU maintenance (eviction skip-loop +
// compaction retain). The amortized-O(1) refresh contract is that this total
// grows linearly with the number of cache operations, not as
// `operations * capacity` (the cost of the old O(n) position-scan refresh).
// A pure refresh below the compaction threshold examines zero tokens.
//
// Thread-local + `cfg(test)`: zero production overhead, and each test (its own
// thread) sees only its own counts despite parallel test execution.
#[cfg(test)]
thread_local! {
    static OUTPUT_CACHE_LRU_MAINTENANCE_STEPS: std::cell::Cell<u64> =
        const { std::cell::Cell::new(0) };
}

/// Record `steps` examined LRU-maintenance tokens (no-op outside tests).
#[inline]
fn record_output_cache_lru_steps(_steps: u64) {
    #[cfg(test)]
    OUTPUT_CACHE_LRU_MAINTENANCE_STEPS.with(|c| c.set(c.get().saturating_add(_steps)));
}

#[cfg(test)]
fn output_cache_lru_maintenance_steps() -> u64 {
    OUTPUT_CACHE_LRU_MAINTENANCE_STEPS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn reset_output_cache_lru_maintenance_steps() {
    OUTPUT_CACHE_LRU_MAINTENANCE_STEPS.with(|c| c.set(0));
}

impl OutputCache {
    /// Create a new output cache with the given configuration.
    #[must_use]
    pub fn new(config: OutputCacheConfig) -> Self {
        Self {
            config,
            global_hashes: HashMap::new(),
            lru_order: VecDeque::new(),
            lru_generation: 0,
            pane_states: HashMap::new(),
            hits: 0,
            misses: 0,
        }
    }

    /// Create a new output cache with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(OutputCacheConfig::default())
    }

    /// Check if content is new (not previously seen).
    ///
    /// Returns `true` if the content should be processed (new or changed).
    /// Returns `false` if the content can be skipped (unchanged).
    pub fn is_new(&mut self, pane_id: u64, content: &str) -> bool {
        let now = epoch_ms();
        let hash = hash_text(content);
        let len = content.len();

        // Check per-pane state first (fast path)
        if let Some(state) = self.pane_states.get_mut(&pane_id) {
            if state.content_hash == hash && state.content_len == len {
                self.hits = self.hits.saturating_add(1);
                state.last_updated = now;
                return false;
            }
        }

        // Check global LRU (cross-pane deduplication)
        if self.global_hashes.contains_key(&hash) {
            self.update_pane_state(pane_id, hash, len, now);
            self.update_global_lru(hash, now);
            self.hits = self.hits.saturating_add(1);
            return false;
        }

        // New content
        self.update_pane_state(pane_id, hash, len, now);
        self.update_global_lru(hash, now);
        self.misses = self.misses.saturating_add(1);
        true
    }

    fn update_pane_state(&mut self, pane_id: u64, hash: u64, len: usize, now: i64) {
        self.pane_states.insert(
            pane_id,
            PaneCacheState {
                content_hash: hash,
                content_len: len,
                last_updated: now,
            },
        );
    }

    fn update_global_lru(&mut self, hash: u64, now: i64) {
        if self.config.global_lru_capacity == 0 {
            return;
        }
        if self.lru_generation == u64::MAX {
            // The ordering token is meaningful only within this bounded cache.
            // Discard the entire global epoch before recycling so a stale token
            // can never alias a live entry after wraparound.
            self.global_hashes.clear();
            self.lru_order.clear();
            self.lru_generation = 0;
        }
        if let Entry::Occupied(mut entry) = self.global_hashes.entry(hash) {
            // ft-zo4hw: refresh recency on access in O(1) amortized. Stamp a
            // fresh generation and push a new ordering token to the back; the
            // hash's previous token is now stale (older generation) and is
            // skipped lazily during eviction/compaction. This replaces the
            // ft-fesg7 O(n) position-scan + `VecDeque::remove`. Eviction
            // (`pop_front`) still reflects access recency because a hash's
            // *current-generation* token is always its most recently pushed
            // one, so the frontmost live token is the true LRU entry.
            // Inline the counter bump (a disjoint field access) so the live
            // `entry` borrow of `global_hashes` doesn't collide with a
            // whole-`self` method borrow.
            self.lru_generation += 1;
            let generation = self.lru_generation;
            *entry.get_mut() = GlobalHashEntry {
                last_seen: now,
                generation,
            };
            self.lru_order.push_back((hash, generation));
            self.compact_lru_order_if_needed();
            return;
        }

        // Evict least-recently-used live entries if at capacity. Gate on the
        // live hash count (not `lru_order.len()`, which may hold stale tokens),
        // popping from the front and skipping superseded tokens in O(1) each.
        while self.global_hashes.len() >= self.config.global_lru_capacity {
            let Some((old_hash, generation)) = self.lru_order.pop_front() else {
                break;
            };
            // Count each examined token (ft-wo323 amortized-O(1) guard).
            record_output_cache_lru_steps(1);
            if let Entry::Occupied(slot) = self.global_hashes.entry(old_hash)
                && slot.get().generation == generation
            {
                slot.remove();
            }
        }

        let generation = self.next_lru_generation();
        self.global_hashes.insert(
            hash,
            GlobalHashEntry {
                last_seen: now,
                generation,
            },
        );
        self.lru_order.push_back((hash, generation));
    }

    /// Next monotonic recency generation within the current cache epoch.
    fn next_lru_generation(&mut self) -> u64 {
        debug_assert_ne!(self.lru_generation, u64::MAX);
        self.lru_generation += 1;
        self.lru_generation
    }

    /// Drop superseded `lru_order` tokens when stale entries have accumulated
    /// past ~capacity. Amortized O(1) per access: a refresh adds at most one
    /// stale token, so this O(n) retain runs at most once per `capacity`
    /// refreshes, bounding `lru_order` to <= 2x capacity even on all-hit loads.
    fn compact_lru_order_if_needed(&mut self) {
        let cap = self.config.global_lru_capacity.saturating_mul(2);
        if self.lru_order.len() <= cap {
            return;
        }
        // The retain pass visits every token once (ft-wo323 amortized-O(1)
        // guard); fires at most once per ~capacity refreshes.
        record_output_cache_lru_steps(self.lru_order.len() as u64);
        let live = &self.global_hashes;
        self.lru_order.retain(|&(hash, generation)| {
            live.get(&hash).is_some_and(|e| e.generation == generation)
        });
    }

    /// Prune stale per-pane entries older than max_age.
    pub fn prune(&mut self, max_age_ms: u64) {
        let now = epoch_ms();
        let max_age = i64::try_from(max_age_ms).unwrap_or(i64::MAX);
        let cutoff = now.saturating_sub(max_age);

        self.pane_states
            .retain(|_, state| state.last_updated > cutoff);

        let hashes_to_remove: std::collections::HashSet<u64> = self
            .global_hashes
            .iter()
            .filter(|(_, entry)| entry.last_seen < cutoff)
            .map(|(hash, _)| *hash)
            .collect();

        for hash in &hashes_to_remove {
            self.global_hashes.remove(hash);
        }
        // Single O(n) pass instead of O(n*m) per-hash retain calls. Drops
        // tokens for pruned hashes; superseded tokens for surviving hashes are
        // cleaned by later eviction/compaction (ft-zo4hw lazy invalidation).
        self.lru_order
            .retain(|&(hash, _)| !hashes_to_remove.contains(&hash));
    }

    /// Prune stale entries using the configured max_age.
    pub fn prune_stale(&mut self) {
        self.prune(self.config.per_pane_max_age_ms);
    }

    /// Get the current cache hit rate (0.0 - 1.0).
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn hit_rate(&self) -> f64 {
        // Convert before adding: both operational counters saturate at
        // `u64::MAX`, so integer addition could overflow exactly when this
        // diagnostic is most needed. f64 has ample exponent range for the
        // sum and preserves the meaningful 0.5 ratio when both counters are
        // saturated.
        let total = self.hits as f64 + self.misses as f64;
        if total == 0.0 {
            0.0
        } else {
            self.hits as f64 / total
        }
    }

    /// Get cache statistics.
    #[must_use]
    pub fn stats(&self) -> OutputCacheStats {
        OutputCacheStats {
            hits: self.hits,
            misses: self.misses,
            hit_rate: self.hit_rate(),
            global_entries: self.global_hashes.len(),
            pane_entries: self.pane_states.len(),
        }
    }

    /// Reset statistics counters.
    pub fn reset_stats(&mut self) {
        self.hits = 0;
        self.misses = 0;
    }

    /// Remove a specific pane from the cache.
    pub fn remove_pane(&mut self, pane_id: u64) {
        self.pane_states.remove(&pane_id);
    }

    /// Clear all cache entries.
    pub fn clear(&mut self) {
        self.global_hashes.clear();
        self.lru_order.clear();
        self.lru_generation = 0;
        self.pane_states.clear();
        self.hits = 0;
        self.misses = 0;
    }
}

/// Statistics from the output cache.
#[derive(Debug, Clone)]
pub struct OutputCacheStats {
    /// Number of cache hits
    pub hits: u64,
    /// Number of cache misses
    pub misses: u64,
    /// Hit rate (0.0 - 1.0)
    pub hit_rate: f64,
    /// Number of entries in global LRU
    pub global_entries: usize,
    /// Number of per-pane entries
    pub pane_entries: usize,
}

// =============================================================================
// OSC 133 Semantic Markers (Shell Integration)
// =============================================================================

/// OSC 133 marker types for shell integration.
///
/// These markers are emitted by shells with semantic prompt integration enabled.
/// WezTerm supports these markers through its shell integration scripts.
///
/// Reference: <https://gitlab.freedesktop.org/Per_Bothner/specifications/-/blob/4d2e1d75d4861a1d924895e106f8f016880e12a7/proposals/semantic-prompts.md>
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Osc133Marker {
    /// `A` - Fresh line / start of prompt
    PromptStart,
    /// `B` - End of prompt, start of user input
    CommandStart,
    /// `C` - End of user input, start of command output
    CommandExecuted,
    /// `D` - End of command output (optional exit code)
    CommandFinished { exit_code: Option<i32> },
}

/// Pane shell state derived from OSC 133 markers.
///
/// This tracks the semantic state of a shell session based on OSC 133 markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellState {
    /// No shell integration detected or unknown state
    #[default]
    Unknown,
    /// Prompt is being displayed (after A marker)
    PromptActive,
    /// User is typing a command (after B marker)
    InputActive,
    /// Command is running (after C marker)
    CommandRunning,
    /// Command finished (after D marker), ready for next prompt
    CommandFinished { exit_code: Option<i32> },
}

impl ShellState {
    /// Check if the shell is at a prompt (safe to send commands)
    #[must_use]
    pub fn is_at_prompt(&self) -> bool {
        matches!(
            self,
            Self::PromptActive | Self::CommandFinished { .. } | Self::InputActive
        )
    }

    /// Check if a command is currently running
    #[must_use]
    pub fn is_command_running(&self) -> bool {
        matches!(self, Self::CommandRunning)
    }

    /// Check if the shell is idle (at prompt, ready for commands, not running anything)
    ///
    /// This is equivalent to `is_at_prompt()` but with a name that better conveys
    /// the "nothing happening, ready for input" semantics.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.is_at_prompt()
    }
}

/// Per-pane state tracker for OSC 133 markers.
#[derive(Debug, Clone)]
pub struct Osc133State {
    /// Current shell state
    pub state: ShellState,
    /// Last exit code received (from most recent D marker)
    pub last_exit_code: Option<i32>,
    /// Count of markers processed (for diagnostics)
    pub markers_seen: u64,
    /// Timestamp of last state change (epoch ms)
    pub last_change_at: i64,
}

impl Default for Osc133State {
    fn default() -> Self {
        Self::new()
    }
}

impl Osc133State {
    /// Create a new state tracker
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: ShellState::Unknown,
            last_exit_code: None,
            markers_seen: 0,
            last_change_at: 0,
        }
    }

    /// Process a marker and update state
    pub fn process_marker(&mut self, marker: Osc133Marker) {
        self.markers_seen = self.markers_seen.saturating_add(1);
        self.last_change_at = epoch_ms();

        match marker {
            Osc133Marker::PromptStart => {
                self.state = ShellState::PromptActive;
            }
            Osc133Marker::CommandStart => {
                self.state = ShellState::InputActive;
            }
            Osc133Marker::CommandExecuted => {
                self.state = ShellState::CommandRunning;
            }
            Osc133Marker::CommandFinished { exit_code } => {
                self.last_exit_code = exit_code;
                self.state = ShellState::CommandFinished { exit_code };
            }
        }
    }
}

/// Parse OSC 133 markers from terminal output.
///
/// This parser is designed to be robust:
/// - Handles partial/truncated sequences gracefully
/// - Does not panic on malformed input
/// - Returns all valid markers found
///
/// # Arguments
/// * `text` - Terminal output that may contain escape sequences
///
/// # Returns
/// Vector of parsed markers in order of occurrence
#[must_use]
pub fn parse_osc133_markers(text: &str) -> Vec<Osc133Marker> {
    let mut markers = Vec::new();
    let bytes = text.as_bytes();
    let mut base = 0;

    while base < bytes.len() {
        let Some(offset) = memchr::memchr(0x1b, &bytes[base..]) else {
            break;
        };
        let pos = base + offset;

        // Look for ESC ] (OSC start)
        if pos + 1 < bytes.len() && bytes[pos + 1] == b']' {
            // Found OSC start, look for "133;"
            if let Some((marker, consumed)) = try_parse_osc133(&bytes[pos..]) {
                markers.push(marker);
                base = pos + consumed; // Skip past the parsed sequence
                continue;
            }
        }

        base = pos + 1;
    }

    markers
}

/// Try to parse an OSC 133 sequence starting at the given position.
///
/// Returns the marker and number of bytes consumed, or None if not a valid OSC 133.
fn try_parse_osc133(bytes: &[u8]) -> Option<(Osc133Marker, usize)> {
    // Minimum sequence: ESC ] 1 3 3 ; X ST (where ST is BEL or ESC \)
    // That's at least 7 bytes: \x1b ] 1 3 3 ; A \x07
    if bytes.len() < 7 {
        return None;
    }

    // Check for ESC ]
    if bytes[0] != 0x1b || bytes[1] != b']' {
        return None;
    }

    // Check for "133;"
    if bytes.len() < 6 || &bytes[2..6] != b"133;" {
        return None;
    }

    // Get the marker type (A, B, C, or D)
    let marker_type = bytes[6];

    // Find the string terminator (BEL \x07 or ESC \ )
    let mut end_pos = 7;
    let mut params_end = 7;
    let mut found_terminator = false;

    // Scan for terminator, collecting any parameters after the marker type
    while end_pos < bytes.len() {
        if bytes[end_pos] == 0x07 {
            // BEL terminator
            params_end = end_pos;
            end_pos += 1;
            found_terminator = true;
            break;
        } else if bytes[end_pos] == 0x1b && end_pos + 1 < bytes.len() && bytes[end_pos + 1] == b'\\'
        {
            // ESC \ terminator (ST)
            params_end = end_pos;
            end_pos += 2;
            found_terminator = true;
            break;
        } else if end_pos > 50 {
            // Safety limit - don't scan too far
            return None;
        }
        end_pos += 1;
    }

    // If we didn't find a terminator, this is incomplete
    if !found_terminator {
        return None;
    }

    // Parse the marker
    let marker = match marker_type {
        b'A' => Osc133Marker::PromptStart,
        b'B' => Osc133Marker::CommandStart,
        b'C' => Osc133Marker::CommandExecuted,
        b'D' => {
            // D marker may have exit code: D;exitcode
            let exit_code = if params_end > 7 && bytes[7] == b';' {
                // Try to parse exit code from bytes[8..params_end]
                std::str::from_utf8(&bytes[8..params_end])
                    .ok()
                    .and_then(|s| s.parse::<i32>().ok())
            } else {
                None
            };
            Osc133Marker::CommandFinished { exit_code }
        }
        _ => return None, // Unknown marker type
    };

    Some((marker, end_pos))
}

/// Process terminal output and update OSC 133 state.
///
/// This is a convenience function that parses markers and updates state in one call.
pub fn process_osc133_output(state: &mut Osc133State, text: &str) {
    for marker in parse_osc133_markers(text) {
        state.process_marker(marker);
    }
}

// =============================================================================
// Alt-Screen Detection
// =============================================================================

/// Alternate screen buffer state change detected in terminal output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AltScreenChange {
    /// Entered alternate screen buffer (e.g., vim, less, htop started)
    Entered,
    /// Left alternate screen buffer (program exited back to normal shell)
    Exited,
}

/// Detect alternate screen buffer changes in terminal output.
///
/// Terminals use the following escape sequences for alternate screen:
/// - `ESC [ ? 1049 h` - Enable alternate screen buffer (DECSET 1049)
/// - `ESC [ ? 1049 l` - Disable alternate screen buffer (DECRST 1049)
/// - `ESC [ ? 47 h` / `ESC [ ? 47 l` - Older alternate screen (less common)
///
/// When a program enters alternate screen (vim, less, htop, etc.), the entire
/// visible buffer is replaced. When it exits, the original buffer is restored.
/// This invalidates delta extraction because the content relationship is broken.
///
/// # Returns
/// A vector of alt-screen changes in order of occurrence. Multiple changes
/// can occur if a program rapidly enters and exits alternate screen.
#[inline]
fn alt_screen_change_at(bytes: &[u8], pos: usize) -> Option<AltScreenChange> {
    let tail = bytes.get(pos..)?;
    if tail.starts_with(b"\x1b[?1049h") || tail.starts_with(b"\x1b[?47h") {
        Some(AltScreenChange::Entered)
    } else if tail.starts_with(b"\x1b[?1049l") || tail.starts_with(b"\x1b[?47l") {
        Some(AltScreenChange::Exited)
    } else {
        None
    }
}

#[must_use]
#[allow(clippy::items_after_statements)]
pub fn detect_alt_screen_changes(text: &str) -> Vec<AltScreenChange> {
    let bytes = text.as_bytes();
    let mut changes = Vec::new();
    for pos in memchr::memchr_iter(0x1b, bytes) {
        if let Some(change) = alt_screen_change_at(bytes, pos) {
            changes.push(change);
        }
    }
    changes
}

/// Check if text contains any alternate screen transitions.
///
/// This is a fast check that can be used before full delta extraction
/// to determine if the content might be from a different screen context.
#[must_use]
pub fn has_alt_screen_change(text: &str) -> bool {
    let bytes = text.as_bytes();
    memchr::memchr_iter(0x1b, bytes).any(|pos| alt_screen_change_at(bytes, pos).is_some())
}

// =============================================================================
// Streaming Design (wa-nu4.4.2.1)
// =============================================================================
//
// This section defines the types and policies for real-time output streaming
// from vendored WezTerm's subscribe_output API. The streaming path produces
// the same CapturedSegment type as the polling path but receives events
// pushed from the mux server rather than pulling via CLI snapshots.
//
// ## Streamed Unit
//
// The streamed unit is a **delta string**: a UTF-8 string representing new
// output appended to a pane. This aligns with the existing CapturedSegment
// model where `kind: Delta` carries the incremental text and `kind: Gap`
// carries a full snapshot when continuity is broken.
//
// The vendored subscribe_output API delivers chunks of bytes as they arrive
// at the PTY. These are decoded to UTF-8 (lossy) and wrapped in StreamEvent
// for channel delivery. The StreamIngester then maps each event through a
// PaneCursor to assign monotonic seq numbers and detect gaps.
//
// ## Backpressure Strategy
//
// A bounded mpsc channel sits between the mux event source and the ingester.
// When the channel fills (consumer too slow), the overflow policy determines
// behavior:
//
// - **EmitGap** (default): The sender drops the event and sets a per-pane
//   overflow flag. The next successfully delivered event for that pane will
//   carry an `overflow: true` annotation, causing the ingester to emit an
//   explicit GAP segment before the delta. This ensures no silent data loss.
//
// - **DropOldest**: The sender removes the oldest event in the channel to
//   make room for the new one, and marks both the dropped pane and the new
//   event's pane as having experienced overflow.
//
// Silent drops are never permitted. Every lost event manifests as a GAP in
// the segment stream.

/// An event from the streaming output source (vendored mux subscribe_output).
///
/// This is the "wire format" between the mux event loop and the ingester.
/// Each event carries raw delta text for a single pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// New output data from a pane.
    OutputData {
        /// Pane that produced the output.
        pane_id: u64,
        /// UTF-8 delta text (new bytes decoded from PTY).
        ///
        /// This may be empty for synthetic upstream gap markers; in that case
        /// the ingester should emit only a GAP and not fabricate a zero-length
        /// delta segment.
        data: String,
        /// Epoch milliseconds when the data was received from the mux.
        received_at: i64,
        /// True if one or more events were dropped before this one due to
        /// channel overflow. The ingester must emit a GAP before this delta.
        overflow: bool,
    },
    /// Pane was closed or the subscription ended for this pane.
    PaneClosed { pane_id: u64 },
    /// The entire subscription was disconnected (mux server gone, reconnect needed).
    Disconnected { reason: String },
}

/// Policy for handling channel overflow when the consumer cannot keep up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OverflowPolicy {
    /// Drop the new event and mark the pane as having overflow.
    /// The next successfully delivered event for that pane will
    /// carry an `overflow: true` annotation, causing the ingester to emit an
    /// explicit GAP segment before the delta. This ensures no silent data loss.
    #[default]
    EmitGap,
    /// Remove the oldest event in the channel to make room for the new one.
    /// The dropped event's pane gets an overflow marker on its next buffered or
    /// subsequently accepted event.
    DropOldest,
}

/// Configuration for the streaming channel between mux source and ingester.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChannelConfig {
    /// Maximum number of events the channel can buffer before overflow
    /// policy kicks in. Must be >= 1.
    pub capacity: usize,
    /// What to do when the channel is full.
    pub overflow_policy: OverflowPolicy,
}

impl Default for StreamChannelConfig {
    fn default() -> Self {
        Self {
            capacity: 4096,
            overflow_policy: OverflowPolicy::EmitGap,
        }
    }
}

/// Converts streaming events into CapturedSegments with monotonic seq.
///
/// The ingester maintains a PaneCursor per pane (same as the polling path)
/// and maps each StreamEvent::OutputData into a CapturedSegment. When
/// overflow is indicated, it emits a GAP before the delta.
///
/// # Invariants
///
/// 1. **Seq monotonicity**: For any pane, each emitted CapturedSegment has
///    a strictly increasing `seq` (no duplicates, no decreases).
/// 2. **GAP determinism**: Every overflow or disconnect produces exactly one
///    GAP segment per affected pane before the next delta.
/// 3. **No silent drops**: If data is lost between source and storage, a GAP
///    with a descriptive reason appears in the segment stream.
pub struct StreamIngester {
    /// Per-pane cursors (same type as polling path).
    cursors: HashMap<u64, PaneCursor>,
    /// Panes that have experienced overflow and need a GAP on next data.
    overflow_pending: HashSet<u64>,
    /// Total segments emitted (diagnostic counter).
    segments_emitted: u64,
    /// Total gaps emitted (diagnostic counter).
    gaps_emitted: u64,
}

impl StreamIngester {
    /// Create a new ingester with no pane state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cursors: HashMap::new(),
            overflow_pending: HashSet::new(),
            segments_emitted: 0,
            gaps_emitted: 0,
        }
    }

    /// Process a stream event and return zero or more CapturedSegments.
    ///
    /// Returns a Vec because overflow events with payload produce two segments
    /// (GAP + Delta). Explicit upstream gaps, PaneClosed, and Disconnected may
    /// produce GAP-only output.
    pub fn process(&mut self, event: StreamEvent) -> Vec<CapturedSegment> {
        let capacity_timer = crate::runtime_telemetry::SwarmCapacityStageTimer::start(
            crate::runtime_telemetry::SwarmCapacityStage::IngestCapture,
            u64::try_from(self.cursors.len()).unwrap_or(u64::MAX),
        );
        let segments = match event {
            StreamEvent::OutputData {
                pane_id,
                data,
                received_at,
                overflow,
            } => self.process_output(pane_id, data, received_at, overflow),
            StreamEvent::PaneClosed { pane_id } => self.process_pane_closed(pane_id),
            StreamEvent::Disconnected { reason } => self.process_disconnected(&reason),
        };
        capacity_timer.finish_completion();
        segments
    }

    fn process_output(
        &mut self,
        pane_id: u64,
        data: String,
        received_at: i64,
        overflow: bool,
    ) -> Vec<CapturedSegment> {
        let mut segments = Vec::new();

        // Track overflow from the event itself or from prior pending state
        if overflow {
            self.overflow_pending.insert(pane_id);
        }

        if data.is_empty() && !self.overflow_pending.contains(&pane_id) {
            return segments;
        }

        let cursor = self
            .cursors
            .entry(pane_id)
            .or_insert_with(|| PaneCursor::new(pane_id));

        // If this pane has pending overflow, emit GAP first
        if self.overflow_pending.remove(&pane_id) {
            let gap = cursor.emit_gap("stream_overflow");
            self.gaps_emitted = self.gaps_emitted.saturating_add(1);
            self.segments_emitted = self.segments_emitted.saturating_add(1);
            segments.push(gap);
        }

        // Vendored explicit gap markers are bridged as overflow + empty data.
        // Once the GAP is emitted, there is no delta payload to persist.
        if data.is_empty() {
            return segments;
        }

        // Emit the delta segment via PaneCursor (bypasses snapshot diff,
        // since streaming gives us actual deltas directly)
        let seg = cursor.capture_delta(data, received_at);
        self.segments_emitted = self.segments_emitted.saturating_add(1);
        segments.push(seg);

        segments
    }

    fn process_pane_closed(&mut self, pane_id: u64) -> Vec<CapturedSegment> {
        self.overflow_pending.remove(&pane_id);

        // If we have a cursor for this pane, emit a final gap marking the close
        if let Some(mut cursor) = self.cursors.remove(&pane_id) {
            let gap = cursor.emit_gap("pane_closed");
            self.gaps_emitted = self.gaps_emitted.saturating_add(1);
            self.segments_emitted = self.segments_emitted.saturating_add(1);
            vec![gap]
        } else {
            vec![]
        }
    }

    fn process_disconnected(&mut self, reason: &str) -> Vec<CapturedSegment> {
        let mut segments = Vec::new();
        let gap_reason = format!("stream_disconnected:{reason}");

        // Emit a GAP for every active pane
        for cursor in self.cursors.values_mut() {
            let gap = cursor.emit_gap(&gap_reason);
            self.gaps_emitted = self.gaps_emitted.saturating_add(1);
            self.segments_emitted = self.segments_emitted.saturating_add(1);
            segments.push(gap);
        }

        // Mark all panes as overflow-pending for when they reconnect
        let pane_ids: Vec<u64> = self.cursors.keys().copied().collect();
        for pid in pane_ids {
            self.overflow_pending.insert(pid);
        }

        segments
    }

    /// Number of active pane cursors.
    #[must_use]
    pub fn active_panes(&self) -> usize {
        self.cursors.len()
    }

    /// Total segments emitted since creation.
    #[must_use]
    pub fn total_segments(&self) -> u64 {
        self.segments_emitted
    }

    /// Total gap segments emitted since creation.
    #[must_use]
    pub fn total_gaps(&self) -> u64 {
        self.gaps_emitted
    }

    /// Check if a pane has pending overflow (next data will produce GAP first).
    #[must_use]
    pub fn has_pending_overflow(&self, pane_id: u64) -> bool {
        self.overflow_pending.contains(&pane_id)
    }

    /// Get the current cursor state for a pane (for diagnostics).
    #[must_use]
    pub fn cursor_for(&self, pane_id: u64) -> Option<&PaneCursor> {
        self.cursors.get(&pane_id)
    }

    /// Take a serializable snapshot of stream ingester telemetry.
    #[must_use]
    pub fn telemetry_snapshot(&self) -> StreamIngesterTelemetrySnapshot {
        StreamIngesterTelemetrySnapshot {
            active_panes: self.cursors.len() as u64,
            segments_emitted: self.segments_emitted,
            gaps_emitted: self.gaps_emitted,
            overflow_pending: self.overflow_pending.len() as u64,
        }
    }
}

/// Serializable snapshot of stream ingester telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamIngesterTelemetrySnapshot {
    pub active_panes: u64,
    pub segments_emitted: u64,
    pub gaps_emitted: u64,
    pub overflow_pending: u64,
}

impl Default for StreamIngester {
    fn default() -> Self {
        Self::new()
    }
}

/// Simulates a bounded channel with overflow tracking for testing.
///
/// In production, this is backed by the compat mpsc channel with
/// `try_send` for non-blocking overflow detection. This sync version
/// exists for property testing without a runtime.
pub struct StreamChannel {
    buffer: VecDeque<StreamEvent>,
    capacity: usize,
    policy: OverflowPolicy,
    /// Per-pane overflow flag: set when an event is dropped.
    overflow_panes: HashSet<u64>,
    /// Total events dropped due to overflow.
    pub events_dropped: u64,
}

impl StreamChannel {
    /// Create a new channel with the given config.
    #[must_use]
    pub fn new(config: &StreamChannelConfig) -> Self {
        Self {
            buffer: VecDeque::with_capacity(config.capacity),
            capacity: config.capacity.max(1),
            policy: config.overflow_policy,
            overflow_panes: HashSet::new(),
            events_dropped: 0,
        }
    }

    /// Try to send an event into the channel.
    ///
    /// Returns `true` if the event was buffered, `false` if the new event was
    /// dropped by the EmitGap policy.
    pub fn send(&mut self, mut event: StreamEvent) -> bool {
        if self.buffer.len() < self.capacity {
            self.apply_pending_overflow_to_event(&mut event);
            self.buffer.push_back(event);
            return true;
        }

        // Channel full — apply overflow policy
        match self.policy {
            OverflowPolicy::EmitGap => {
                // Mark the pane as having overflow
                if let StreamEvent::OutputData { pane_id, .. } = &event {
                    self.overflow_panes.insert(*pane_id);
                }
                self.events_dropped = self.events_dropped.saturating_add(1);
                false
            }
            OverflowPolicy::DropOldest => {
                // Evict oldest, mark its pane
                if let Some(StreamEvent::OutputData { pane_id, .. }) =
                    self.buffer.pop_front().as_ref()
                {
                    self.mark_next_event_for_pane_overflow(*pane_id, &mut event);
                }
                self.apply_pending_overflow_to_event(&mut event);
                self.buffer.push_back(event);
                self.events_dropped = self.events_dropped.saturating_add(1);
                true
            }
        }
    }

    /// Receive the next event from the channel.
    pub fn recv(&mut self) -> Option<StreamEvent> {
        self.buffer.pop_front()
    }

    /// Number of events currently buffered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Whether the channel is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Whether the channel is at capacity.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.buffer.len() >= self.capacity
    }

    fn apply_pending_overflow_to_event(&mut self, event: &mut StreamEvent) {
        let StreamEvent::OutputData {
            pane_id, overflow, ..
        } = event
        else {
            return;
        };

        if self.overflow_panes.remove(pane_id) {
            *overflow = true;
        }
    }

    fn mark_next_event_for_pane_overflow(&mut self, pane_id: u64, new_event: &mut StreamEvent) {
        if let Some(StreamEvent::OutputData { overflow, .. }) =
            self.buffer.iter_mut().find(|event| {
                matches!(
                    event,
                    StreamEvent::OutputData {
                        pane_id: event_pane_id,
                        ..
                    } if *event_pane_id == pane_id
                )
            })
        {
            *overflow = true;
            return;
        }

        if let StreamEvent::OutputData {
            pane_id: event_pane_id,
            overflow,
            ..
        } = new_event
            && *event_pane_id == pane_id
        {
            *overflow = true;
            return;
        }

        self.overflow_panes.insert(pane_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DB_COUNTER: AtomicU64 = AtomicU64::new(0);
    const TEST_MAX_PERSIST_SEGMENT_BYTES: usize =
        crate::tuning_config::IngestTuning::DEFAULT_MAX_PERSIST_SEGMENT_BYTES;

    #[test]
    fn ingest_cancelled_error_uses_structured_runtime_operation() {
        let err = ingest_cancelled_error("persist_captured_segment", "caller cancelled");

        match err {
            crate::Error::RuntimeOperation { operation, source } => {
                assert_eq!(operation, "persist_captured_segment");
                assert_eq!(
                    source,
                    RuntimeOperationSource::Cancelled("caller cancelled".to_string())
                );
            }
            other => panic!("expected RuntimeOperation, got {other:?}"),
        }
    }

    /// Test-local counter observer, not an exporter. Unrelated runtime metrics
    /// and gauge/histogram descriptions are intentionally outside this oracle.
    #[derive(Default)]
    struct CaptureRetryRecorder {
        retries: std::sync::Arc<metrics::atomics::AtomicU64>,
        dropped: std::sync::Arc<metrics::atomics::AtomicU64>,
        exhausted: std::sync::Arc<metrics::atomics::AtomicU64>,
    }

    impl metrics::Recorder for CaptureRetryRecorder {
        fn describe_counter(
            &self,
            _key: metrics::KeyName,
            _unit: Option<metrics::Unit>,
            _description: metrics::SharedString,
        ) {
        }

        fn describe_gauge(
            &self,
            _key: metrics::KeyName,
            _unit: Option<metrics::Unit>,
            _description: metrics::SharedString,
        ) {
        }

        fn describe_histogram(
            &self,
            _key: metrics::KeyName,
            _unit: Option<metrics::Unit>,
            _description: metrics::SharedString,
        ) {
        }

        fn register_counter(
            &self,
            key: &metrics::Key,
            _metadata: &metrics::Metadata<'_>,
        ) -> metrics::Counter {
            let counter = match key.name() {
                "capture.persist.retries" => &self.retries,
                "capture.persist.dropped" => &self.dropped,
                "capture.persist.retry_exhausted" => &self.exhausted,
                _ => return metrics::Counter::noop(),
            };
            metrics::Counter::from_arc(std::sync::Arc::clone(counter))
        }

        fn register_gauge(
            &self,
            _key: &metrics::Key,
            _metadata: &metrics::Metadata<'_>,
        ) -> metrics::Gauge {
            metrics::Gauge::noop()
        }

        fn register_histogram(
            &self,
            _key: &metrics::Key,
            _metadata: &metrics::Metadata<'_>,
        ) -> metrics::Histogram {
            metrics::Histogram::noop()
        }
    }

    fn run_capture_retry_test<F>(expected_counts: (u64, u64, u64), future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let recorder = CaptureRetryRecorder::default();
        metrics::with_local_recorder(&recorder, || run_async_test(future));
        assert_eq!(
            recorder.retries.load(Ordering::SeqCst),
            expected_counts.0,
            "capture.persist.retries"
        );
        assert_eq!(
            recorder.dropped.load(Ordering::SeqCst),
            expected_counts.1,
            "capture.persist.dropped"
        );
        assert_eq!(
            recorder.exhausted.load(Ordering::SeqCst),
            expected_counts.2,
            "capture.persist.retry_exhausted"
        );
    }

    // Synthetic append responses exercise retry control flow only. Native
    // SQLite contention and writer settlement require their separate tests.
    #[test]
    fn retry_capture_append_succeeds_after_two_verified_busy_responses() {
        run_capture_retry_test((2, 0, 0), async {
            let cx = crate::cx::for_testing();
            let expected = Segment {
                id: 41,
                pane_id: 7,
                seq: 3,
                content: "captured output".to_string(),
                content_len: 15,
                content_hash: Some("retained-hash".to_string()),
                captured_at: 1_700_000_000_000,
            };
            let mut attempts = 0;
            let segment = retry_capture_append(&cx, || {
                attempts += 1;
                std::future::ready(if attempts < 3 {
                    Err(crate::Error::Storage(
                        crate::error::StorageError::WriterBusyNotCommitted,
                    ))
                } else {
                    Ok(expected.clone())
                })
            })
            .await
            .expect("third append response succeeds");

            assert_eq!(attempts, 3);
            assert_eq!(segment.id, expected.id);
            assert_eq!(segment.pane_id, expected.pane_id);
            assert_eq!(segment.seq, expected.seq);
            assert_eq!(segment.content, expected.content);
            assert_eq!(segment.content_len, expected.content_len);
            assert_eq!(segment.content_hash, expected.content_hash);
            assert_eq!(segment.captured_at, expected.captured_at);
        });
    }

    #[test]
    fn retry_capture_append_exhausts_after_three_verified_busy_responses() {
        run_capture_retry_test((2, 1, 1), async {
            let cx = crate::cx::for_testing();
            let mut attempts = 0;
            let result = retry_capture_append(&cx, || {
                attempts += 1;
                std::future::ready(Err(crate::Error::Storage(
                    crate::error::StorageError::WriterBusyNotCommitted,
                )))
            })
            .await;

            assert_eq!(attempts, 3);
            assert!(matches!(
                result,
                Err(crate::Error::Storage(
                    crate::error::StorageError::WriterBusyNotCommitted
                ))
            ));
        });
    }

    #[test]
    fn retry_capture_append_never_retries_unverified_or_indeterminate_errors() {
        run_capture_retry_test((0, 0, 0), async {
            for error in [
                crate::error::StorageError::Database("database is locked".to_string()),
                crate::error::StorageError::WriterBackendEpochPoisoned,
                crate::error::StorageError::WriterSettlementIndeterminate {
                    phase: "command_response",
                },
                crate::error::StorageError::IndeterminateMutation {
                    operation: "append_segment",
                },
            ] {
                let cx = crate::cx::for_testing();
                let expected_variant = std::mem::discriminant(&error);
                let expected_message = error.to_string();
                let mut response = Some(error);
                let mut attempts = 0;
                let result = retry_capture_append(&cx, || {
                    attempts += 1;
                    std::future::ready(Err(crate::Error::Storage(
                        response
                            .take()
                            .expect("unverified append must not be attempted twice"),
                    )))
                })
                .await;

                assert_eq!(attempts, 1);
                let Err(crate::Error::Storage(returned)) = result else {
                    panic!("expected original storage error");
                };
                assert_eq!(std::mem::discriminant(&returned), expected_variant);
                assert_eq!(returned.to_string(), expected_message);
            }
        });
    }

    #[test]
    fn retry_capture_append_precancelled_context_admits_no_attempt() {
        run_capture_retry_test((0, 0, 0), async {
            let cx = crate::cx::for_testing();
            cx.set_cancel_requested(true);
            let mut attempts = 0;
            let result = retry_capture_append(&cx, || {
                attempts += 1;
                std::future::ready(Err(crate::Error::Storage(
                    crate::error::StorageError::WriterBusyNotCommitted,
                )))
            })
            .await;

            assert_eq!(attempts, 0);
            assert!(matches!(
                result,
                Err(crate::Error::RuntimeOperation {
                    operation: "capture_append_retry",
                    source: RuntimeOperationSource::Cancelled(_),
                })
            ));
        });
    }

    #[test]
    fn retry_capture_append_cancellation_after_busy_prevents_second_attempt() {
        run_capture_retry_test((0, 0, 0), async {
            let cx = crate::cx::for_testing();
            let mut attempts = 0;
            let result = retry_capture_append(&cx, || {
                attempts += 1;
                cx.set_cancel_requested(true);
                std::future::ready(Err(crate::Error::Storage(
                    crate::error::StorageError::WriterBusyNotCommitted,
                )))
            })
            .await;

            assert_eq!(attempts, 1);
            assert!(matches!(
                result,
                Err(crate::Error::RuntimeOperation {
                    operation: "capture_append_retry",
                    source: RuntimeOperationSource::Cancelled(_),
                })
            ));
        });
    }

    fn temp_db_path() -> String {
        let counter = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir();
        dir.join(format!(
            "wa_ingest_test_{counter}_{}.db",
            std::process::id()
        ))
        .to_string_lossy()
        .to_string()
    }

    fn cleanup_db(path: &str) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path}-wal"));
        let _ = std::fs::remove_file(format!("{path}-shm"));
    }

    fn test_pane_record(pane_id: u64) -> PaneRecord {
        let now = epoch_ms();
        PaneRecord {
            pane_id,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: Some("shell".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: now,
            last_seen_at: now,
            observed: true,
            ignore_reason: None,
            last_decision_at: Some(now),
        }
    }

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::CompatRuntime::block_on(&runtime, future);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn cursor_starts_at_zero() {
        let cursor = PaneCursor::new(42);
        assert_eq!(cursor.pane_id, 42);
        assert_eq!(cursor.next_seq, 0);
        assert!(!cursor.in_gap);
    }

    /// Serializes the saturation-counter tests so they don't observe each
    /// other's state. The counter is process-wide; tests must reset and read
    /// it under this lock to remain deterministic. [ft-g8nbu]
    static SEQ_SATURATION_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn pane_cursor_seq_saturation_counter_zero_baseline() {
        let _guard = SEQ_SATURATION_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_pane_cursor_seq_saturation_count_for_test();

        let mut cursor = PaneCursor::new(1);
        let _ = cursor.capture_delta("hi".to_string(), 0);
        let _ = cursor.capture_delta("there".to_string(), 0);

        assert_eq!(
            pane_cursor_seq_saturation_count(),
            0,
            "normal increments must not bump the saturation counter"
        );
    }

    #[test]
    fn pane_cursor_seq_saturation_counter_increments_at_u64_max() {
        let _guard = SEQ_SATURATION_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_pane_cursor_seq_saturation_count_for_test();

        let mut cursor = PaneCursor::from_seq(7, u64::MAX - 1);
        let _ = cursor.capture_delta("a".to_string(), 0);
        assert_eq!(cursor.next_seq, u64::MAX);
        assert_eq!(
            pane_cursor_seq_saturation_count(),
            0,
            "the increment that arrives AT u64::MAX is still unique — no saturation yet"
        );

        let _ = cursor.capture_delta("b".to_string(), 0);
        assert_eq!(cursor.next_seq, u64::MAX, "saturating_add pinned at MAX");
        assert_eq!(
            pane_cursor_seq_saturation_count(),
            1,
            "first increment that would have overflowed should bump the counter"
        );

        let _ = cursor.capture_delta("c".to_string(), 0);
        assert_eq!(
            pane_cursor_seq_saturation_count(),
            2,
            "every subsequent saturating increment also bumps"
        );
    }

    #[test]
    fn pane_cursor_seq_saturation_counter_never_wraps() {
        let _guard = SEQ_SATURATION_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        PANE_CURSOR_SEQ_SATURATION_COUNT.store(u64::MAX, Ordering::Relaxed);

        let mut cursor = PaneCursor::from_seq(7, u64::MAX);
        let _ = cursor.capture_delta("still saturated".to_string(), 0);

        assert_eq!(pane_cursor_seq_saturation_count(), u64::MAX);
        reset_pane_cursor_seq_saturation_count_for_test();
    }

    #[test]
    fn ingest_telemetry_counters_saturate_without_panicking() {
        let mut telemetry = IngestTelemetry {
            discovery_ticks: u64::MAX,
            panes_discovered: u64::MAX,
            panes_closed: u64::MAX,
            lifecycle_replacements: u64::MAX,
            metadata_changes: u64::MAX,
            panes_filtered: u64::MAX,
        };
        let diff = DiscoveryDiff {
            new_panes: vec![1],
            closed_panes: vec![2],
            metadata_changes: vec![PaneMetadataChange {
                pane_id: 3,
                lifecycle_revision: PaneLifecycleRevision::INITIAL,
                metadata_revision: PaneMetadataRevision::INITIAL,
                diff: PaneMetadataDiff(PaneMetadataDiff::TITLE),
            }],
            lifecycle_replacements: vec![4],
            ..DiscoveryDiff::default()
        };

        telemetry.record_discovery_tick(&diff);
        telemetry.record_pane_filtered();

        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.discovery_ticks, u64::MAX);
        assert_eq!(snapshot.panes_discovered, u64::MAX);
        assert_eq!(snapshot.panes_closed, u64::MAX);
        assert_eq!(snapshot.lifecycle_replacements, u64::MAX);
        assert_eq!(snapshot.metadata_changes, u64::MAX);
        assert_eq!(snapshot.panes_filtered, u64::MAX);
    }

    #[test]
    fn pane_cursor_seq_saturation_counter_bumps_across_emit_paths() {
        let _guard = SEQ_SATURATION_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_pane_cursor_seq_saturation_count_for_test();

        // emit_gap path
        let mut cursor = PaneCursor::from_seq(11, u64::MAX);
        let _ = cursor.emit_gap("synthetic");
        assert_eq!(pane_cursor_seq_saturation_count(), 1);

        // capture_snapshot Content path (pure-append fast path)
        let mut cursor = PaneCursor::from_seq(12, u64::MAX);
        cursor.last_snapshot = "abc".to_string();
        let _ = cursor.capture_snapshot("abcdef", 1024, None);
        assert_eq!(
            pane_cursor_seq_saturation_count(),
            2,
            "Content delta from capture_snapshot must also bump"
        );
    }

    #[test]
    fn registry_tracks_panes() {
        let registry = PaneRegistry::new();
        assert_eq!(registry.pane_ids(), [] as [u64; 0]);
    }

    #[test]
    fn extract_delta_no_change() {
        let result = extract_delta("abc", "abc", 1024);
        assert!(matches!(result, DeltaResult::NoChange));
    }

    #[test]
    fn extract_delta_append_only() {
        let result = extract_delta("hello\n", "hello\nworld\n", 1024);
        assert!(matches!(result, DeltaResult::Content(ref s) if s == "world\n"));
    }

    #[test]
    fn extract_delta_multibyte_append() {
        let prev = "hello";
        let cur = "hello world 🌍";
        let result = extract_delta(prev, cur, 1024);
        assert!(matches!(result, DeltaResult::Content(ref s) if s == " world 🌍"));
    }

    #[test]
    fn extract_delta_sliding_window() {
        let prev = "line1\nline2\nline3\n";
        let cur = "line2\nline3\nline4\n";
        let result = extract_delta(prev, cur, 1024);
        assert!(matches!(result, DeltaResult::Content(ref s) if s == "line4\n"));
    }

    /// ft-r5xkf: a one-byte coincidence at the capture boundary is not evidence
    /// of continuity. `get_text` output is newline-terminated and a leading
    /// blank line is routine after `clear`, so this shape occurs in normal
    /// operation — and reporting it as a clean delta silently discards whatever
    /// scrolled off in between, with no gap recorded.
    #[test]
    fn ft_r5xkf_single_newline_coincidence_is_a_gap() {
        let previous = "history line A\nhistory line B\n";
        let current = "\n$ fresh\n";

        match extract_delta(previous, current, 4096) {
            DeltaResult::Gap { reason, content } => {
                assert_eq!(reason, "overlap_implausible");
                assert_eq!(content, current, "gap content must carry the full capture");
            }
            other => panic!("expected an explicit gap, got {other:?}"),
        }
    }

    /// ft-r5xkf: whitespace-only overlaps of any length are boundary noise too —
    /// a run of blank lines at the end of one capture and the start of the next
    /// says nothing about continuity.
    #[test]
    fn ft_r5xkf_whitespace_only_overlap_is_a_gap() {
        assert!(matches!(
            extract_delta("output done\n\n", "\n\n$ next\n", 4096),
            DeltaResult::Gap { .. }
        ));
    }

    /// ft-r5xkf: the guard must not reclassify genuine continuations. These are
    /// the shapes the crate pins elsewhere — short ASCII words, multibyte
    /// boundaries, and full-line overlaps — all well under the 8-byte
    /// take-on-faith length.
    #[test]
    fn ft_r5xkf_short_but_contentful_overlaps_stay_deltas() {
        let cases = [
            ("hello world", "world peace", " peace"),
            ("ab│─", "│─cd", "cd"),
            ("x🚀", "🚀y", "y"),
            ("head aaa", "aaa tail", " tail"),
            ("00 alpha\n01 beta\n", "01 beta\n02 gamma\n", "02 gamma\n"),
        ];

        for (previous, current, expected_delta) in cases {
            match extract_delta(previous, current, 4096) {
                DeltaResult::Content(delta) => assert_eq!(
                    delta, expected_delta,
                    "genuine continuation {previous:?} -> {current:?} must stay a delta"
                ),
                other => panic!("expected Content for {previous:?} -> {current:?}, got {other:?}"),
            }
        }
    }

    /// ft-li2hc: a scrolled snapshot at realistic scrollback size must still
    /// produce a delta.
    ///
    /// The required border length is `previous.len() - scrolled_bytes` — it grows
    /// with the SNAPSHOT, not with the amount of new output — so a fixed 4 KiB
    /// cap against whole-scrollback captures made every post-scroll poll a gap
    /// carrying the entire snapshot: a spurious gap marker where nothing was
    /// lost, plus the whole scrollback re-persisted, re-redacted, re-indexed and
    /// re-scanned every tick.
    #[test]
    fn ft_li2hc_scrolled_snapshot_at_scrollback_size_yields_content() {
        // The bead's hand-verified shape: 140 lines of 61 bytes = 8540 bytes,
        // then the first line drops off and one new line arrives.
        let lines: Vec<String> = (0..140)
            .map(|i| format!("line{i:04} {:>50}\n", format!("payload-{i}")))
            .collect();
        let previous: String = lines.concat();
        let mut rest: String = lines[1..].concat();
        let new_line = format!("line{:04} {:>50}\n", 140, "payload-140");
        rest.push_str(&new_line);

        // The daemon's overlap_size now tracks RuntimeConfig::default().
        match extract_delta(
            &previous,
            &rest,
            crate::runtime::RuntimeConfig::default().overlap_size,
        ) {
            DeltaResult::Content(delta) => assert_eq!(delta, new_line),
            other => panic!("a scrolled snapshot must produce a delta, got {other:?}"),
        }
    }

    /// ft-li2hc: when the cap is what stopped the search, say so. "This window
    /// could not reach far enough back" is an operator-actionable configuration
    /// fact; "these snapshots share no border" is content loss. Reporting both
    /// as `overlap_not_found` made a tuning problem look like data loss.
    #[test]
    fn ft_li2hc_window_exhaustion_is_a_distinct_reason() {
        let lines: Vec<String> = (0..140)
            .map(|i| format!("line{i:04} {:>50}\n", format!("payload-{i}")))
            .collect();
        let previous: String = lines.concat();
        let mut rest: String = lines[1..].concat();
        rest.push_str(&format!("line{:04} {:>50}\n", 140, "payload-140"));

        // The old daemon value: too small to reach the border.
        match extract_delta(&previous, &rest, 4096) {
            DeltaResult::Gap { reason, content } => {
                assert_eq!(reason, "overlap_window_exhausted");
                assert_eq!(content, rest);
            }
            other => panic!("expected a window-exhaustion gap, got {other:?}"),
        }

        // The bead's minimal shape, same fact at three bytes.
        match extract_delta("L1\nL2\nL3\n", "L2\nL3\nL4\n", 3) {
            DeltaResult::Gap { reason, .. } => assert_eq!(reason, "overlap_window_exhausted"),
            other => panic!("expected a window-exhaustion gap, got {other:?}"),
        }

        // A genuine no-border case keeps the original reason: the window covered
        // everything it could and there simply is no shared border.
        match extract_delta("abc", "xyz", 1024) {
            DeltaResult::Gap { reason, .. } => assert_eq!(reason, "overlap_not_found"),
            other => panic!("expected a no-border gap, got {other:?}"),
        }
    }

    /// ft-li2hc: a window big enough to be dangerous for the quadratic search
    /// must route to the linear one, whatever the moonshot gate says.
    #[test]
    fn ft_li2hc_large_windows_use_the_linear_search() {
        // Adversarial for the legacy path: every byte in the window matches the
        // first byte of `current`, so it probes every position.
        let previous = "a".repeat(LINEAR_OVERLAP_SEARCH_THRESHOLD_BYTES + 1024);
        let mut current = previous[512..].to_string();
        current.push_str("tail\n");

        let started = std::time::Instant::now();
        let result = extract_delta(&previous, &current, usize::MAX);
        let elapsed = started.elapsed();

        assert!(
            matches!(result, DeltaResult::Content(ref delta) if delta == "tail\n"),
            "expected the scrolled tail, got {result:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "a large window must not fall into the quadratic scan; took {elapsed:?}"
        );
    }

    /// ft-r5xkf: the plausibility rule is a property of the overlap text, so it
    /// is checkable directly. A single byte is never enough; two or more bytes
    /// need something that is not whitespace; long overlaps are taken on faith.
    #[test]
    fn ft_r5xkf_overlap_plausibility_rule() {
        assert!(!overlap_is_plausible(""));
        assert!(!overlap_is_plausible("\n"));
        assert!(!overlap_is_plausible("a"));
        assert!(!overlap_is_plausible("\r\n"));
        assert!(!overlap_is_plausible("   "));
        assert!(overlap_is_plausible("ab"));
        assert!(overlap_is_plausible(" x"));
        assert!(overlap_is_plausible("🚀"));
        // Eight bytes is taken on faith even when it is all whitespace: at that
        // length a coincidental match is no longer boundary noise.
        assert!(overlap_is_plausible("        "));
    }

    #[test]
    fn extract_delta_gap_on_in_place_edit() {
        let prev = "hello\nworld\n";
        let cur = "hello\nthere\n";
        let result = extract_delta(prev, cur, 1024);
        assert!(matches!(result, DeltaResult::Gap { .. }));
    }

    #[test]
    fn extract_delta_sliding_window_cyrillic() {
        // Cyrillic chars are 2 bytes each — overlap boundary can land mid-codepoint
        let prev = "строка1\nстрока2\n";
        let cur = "строка2\nстрока3\n";
        let result = extract_delta(prev, cur, 1024);
        assert!(matches!(result, DeltaResult::Content(ref s) if s == "строка3\n"));
    }

    #[test]
    fn extract_delta_sliding_window_box_drawing() {
        // Box-drawing chars like ─ (U+2500) are 3 bytes — tests 3-byte boundary
        let prev = "┌──────┐\n│ test │\n";
        let cur = "│ test │\n└──────┘\n";
        let result = extract_delta(prev, cur, 1024);
        assert!(matches!(result, DeltaResult::Content(ref s) if s == "└──────┘\n"));
    }

    #[test]
    fn extract_delta_sliding_window_emoji() {
        // Emoji like 🌍 are 4 bytes — tests 4-byte boundary
        let prev = "line🌍\nline🌎\n";
        let cur = "line🌎\nline🌏\n";
        let result = extract_delta(prev, cur, 1024);
        assert!(matches!(result, DeltaResult::Content(ref s) if s == "line🌏\n"));
    }

    #[test]
    fn extract_delta_small_overlap_mid_codepoint() {
        // prev = "abc🌍def" (10 bytes), cur = "def" (3 bytes), overlap_size = 4
        // max_overlap = min(4, 10, 3) = 3  (clamped by cur.len())
        // search_start = 10 - 3 = 7 ('d') — lands on a valid boundary here,
        // but verifies no panic with emoji in the overlap region.
        let prev = "abc🌍def";
        let cur = "def";
        let result = extract_delta(prev, cur, 4);
        // Should not panic — may return Gap or Content depending on match
        assert!(!matches!(result, DeltaResult::NoChange));
    }

    #[test]
    fn extract_delta_search_start_snaps_past_emoji() {
        // prev = "a🌍bcdef" (10 bytes: a=0, 🌍=1..4, b=5, c=6, d=7, e=8, f=9)
        // overlap_size=7, cur.len()=8 → max_overlap=7, search_start=10-7=3
        // Byte 3 is inside the 4-byte emoji — the snapping logic must advance
        // search_start forward to byte 5 ('b') to avoid a char boundary panic.
        let prev = "a\u{1F30D}bcdef";
        let cur = "bcdefXYZ";
        let result = extract_delta(prev, cur, 7);
        // After snapping, search_window="bcdef" matches cur[..5], delta="XYZ"
        assert!(matches!(result, DeltaResult::Content(ref s) if s == "XYZ"));
    }

    #[test]
    fn extract_delta_small_overlap_falls_back_when_probe_anchor_absent() {
        // Regression guard for f2c41fbd: if a future overlap-probe path picks
        // a prefix byte that never appears in the search window, it still has
        // to find the real, short suffix/prefix overlap instead of reporting a
        // gap. `tailxy` → `xynext` exercises that shape directly: the overlap's
        // first byte occurs exactly once in the search window, at its end.
        //
        // ft-r5xkf changed the vehicle, not the guard. This case used to use a
        // one-byte overlap (`tailx` → `xnext`), and a one-byte border is now
        // deliberately refused as boundary noise — see
        // `ft_r5xkf_single_newline_coincidence_is_a_gap`. Two contentful bytes
        // exercise the same probe path while staying an acceptable border.
        let prev = "tailxy";
        let cur = "xynext";
        let result = extract_delta(prev, cur, 1024);
        assert!(matches!(result, DeltaResult::Content(ref s) if s == "next"));
    }

    fn enumerate_utf8_corpus(
        corpus: &mut Vec<String>,
        current: &mut String,
        alphabet: &[&str],
        remaining: usize,
    ) {
        if remaining == 0 {
            corpus.push(current.clone());
            return;
        }

        for symbol in alphabet {
            current.push_str(symbol);
            enumerate_utf8_corpus(corpus, current, alphabet, remaining - 1);
            let new_len = current.len() - symbol.len();
            current.truncate(new_len);
        }
    }

    fn utf8_boundaries(text: &str) -> Vec<usize> {
        let mut boundaries = Vec::with_capacity(text.chars().count() + 1);
        boundaries.push(0);
        let mut offset = 0;
        for ch in text.chars() {
            offset += ch.len_utf8();
            boundaries.push(offset);
        }
        boundaries
    }

    fn extract_delta_reference(previous: &str, current: &str, overlap_size: usize) -> DeltaResult {
        if previous == current {
            return DeltaResult::NoChange;
        }

        if previous.is_empty() {
            return DeltaResult::Content(current.to_string());
        }

        if current.len() > previous.len()
            && current.starts_with(previous)
            && current.is_char_boundary(previous.len())
        {
            return DeltaResult::Content(current[previous.len()..].to_string());
        }

        // br-ft-baaex: mirror the production split — current_empty
        // first (diagnosable downstream symptom), overlap_size_zero
        // second.
        if current.is_empty() {
            return DeltaResult::Gap {
                reason: "current_empty".to_string(),
                content: String::new(),
            };
        }
        if overlap_size == 0 {
            return DeltaResult::Gap {
                reason: "overlap_size_zero".to_string(),
                content: current.to_string(),
            };
        }

        let max_overlap = overlap_size.min(previous.len()).min(current.len());
        // ft-li2hc: mirror the production distinction between "no border exists"
        // and "the window could not reach far enough back".
        let no_border_reason = if max_overlap < previous.len().min(current.len()) {
            "overlap_window_exhausted"
        } else {
            "overlap_not_found"
        };
        let current_boundaries = utf8_boundaries(current);
        let previous_boundaries = utf8_boundaries(previous);
        let mut best_overlap: Option<usize> = None;

        for start in previous_boundaries {
            let overlap_len = previous.len() - start;
            if overlap_len == 0
                || overlap_len > max_overlap
                || overlap_len > current.len()
                || !current_boundaries.contains(&overlap_len)
            {
                continue;
            }

            if previous[start..] == current[..overlap_len] {
                best_overlap =
                    Some(best_overlap.map_or(overlap_len, |best: usize| best.max(overlap_len)));
            }
        }

        match best_overlap {
            // ft-r5xkf: mirrors the production plausibility rule. `best_overlap`
            // is the maximal border, so if it is boundary noise there is no
            // acceptable one.
            Some(overlap_len) if !super::overlap_is_plausible(&current[..overlap_len]) => {
                DeltaResult::Gap {
                    reason: "overlap_implausible".to_string(),
                    content: current.to_string(),
                }
            }
            Some(overlap_len) => {
                let delta = &current[overlap_len..];
                if delta.is_empty() {
                    DeltaResult::Gap {
                        reason: "content_changed_without_append".to_string(),
                        content: current.to_string(),
                    }
                } else {
                    DeltaResult::Content(delta.to_string())
                }
            }
            None => DeltaResult::Gap {
                reason: no_border_reason.to_string(),
                content: current.to_string(),
            },
        }
    }

    fn assert_same_delta_result(actual: &DeltaResult, expected: &DeltaResult) {
        match (actual, expected) {
            (DeltaResult::NoChange, DeltaResult::NoChange) => {}
            (DeltaResult::Content(actual), DeltaResult::Content(expected)) => {
                assert_eq!(actual, expected);
            }
            (
                DeltaResult::Gap {
                    reason: actual_reason,
                    content: actual_content,
                },
                DeltaResult::Gap {
                    reason: expected_reason,
                    content: expected_content,
                },
            ) => {
                assert_eq!(actual_reason, expected_reason);
                assert_eq!(actual_content, expected_content);
            }
            _ => panic!("delta result mismatch: actual={actual:?} expected={expected:?}"),
        }
    }

    fn stitch_one_snapshot(previous: &str, current: &str, overlap_size: usize) -> String {
        match extract_delta(previous, current, overlap_size) {
            DeltaResult::Content(delta) => {
                let mut stitched = previous.to_string();
                stitched.push_str(&delta);
                stitched
            }
            DeltaResult::NoChange => previous.to_string(),
            DeltaResult::Gap { content, .. } => content,
        }
    }

    fn stitch_snapshots(snapshots: &[&str], overlap_size: usize) -> String {
        snapshots.iter().fold(String::new(), |stitched, snapshot| {
            stitch_one_snapshot(&stitched, snapshot, overlap_size)
        })
    }

    #[test]
    fn extract_delta_matches_utf8_reference_oracle() {
        // Guard the optimized memchr/UTF-8 path against a slower maximal-overlap
        // reference over a mixed ASCII/multibyte corpus.
        let alphabet = ["a", "b", "é", "🌍", "┌"];
        let overlap_sizes = [0, 1, 2, 3, 4, 5, 6, 7, 8, 16];
        let mut corpus = vec![String::new()];
        let mut scratch = String::new();
        for len in 1..=3 {
            enumerate_utf8_corpus(&mut corpus, &mut scratch, &alphabet, len);
        }

        for previous in &corpus {
            for current in &corpus {
                for overlap_size in overlap_sizes {
                    let actual = extract_delta(previous, current, overlap_size);
                    let expected = extract_delta_reference(previous, current, overlap_size);
                    assert_same_delta_result(&actual, &expected);
                }
            }
        }
    }

    #[test]
    fn conformance_ingest_overlap_stitching_is_associative() {
        let windows = [
            "00 alpha\n01 beta\n",
            "01 beta\n02 gamma\n",
            "02 gamma\n03 delta\n",
            "03 delta\n04 epsilon\n",
        ];
        let expected = "00 alpha\n01 beta\n02 gamma\n03 delta\n04 epsilon\n";
        let whole = stitch_snapshots(&windows, 1024);

        assert_eq!(whole, expected);
        for split in 1..windows.len() {
            let mut grouped = stitch_snapshots(&windows[..split], 1024);
            for window in &windows[split..] {
                grouped = stitch_one_snapshot(&grouped, window, 1024);
            }
            assert_eq!(
                grouped, whole,
                "stitching should be independent of grouping at split {split}"
            );
        }
    }

    #[test]
    fn conformance_ingest_gap_semantics_are_explicit() {
        // br-ft-baaex: previously the two distinct causes
        // (overlap_size == 0 vs current.is_empty()) shared a
        // single conflated reason `overlap_size_zero_or_current_empty`.
        // The split + ordering invariant ("current_empty wins when
        // both hold") is exercised by the four cases below: pure
        // overlap-disabled, pure capture-empty, both-hold, and
        // each of the unrelated content-discontinuity reasons.
        let cases = [
            ("abc", "bc", 1024, "content_changed_without_append", "bc"),
            ("abc", "xbc", 1024, "overlap_not_found", "xbc"),
            // Pure overlap-disabled: caller turned off overlap
            // (config). Recoverable by re-enabling overlap.
            ("abc", "zabc", 0, "overlap_size_zero", "zabc"),
            // Pure capture-empty: capture returned zero bytes.
            // Symptom of capture-source error or terminal-clear.
            ("abc", "", 1024, "current_empty", ""),
            // Both-hold: per the bead's ordering rule, prefer
            // `current_empty` (downstream symptom) over
            // `overlap_size_zero` (config flag).
            ("abc", "", 0, "current_empty", ""),
        ];

        for (previous, current, overlap_size, expected_reason, expected_content) in cases {
            match extract_delta(previous, current, overlap_size) {
                DeltaResult::Gap { reason, content } => {
                    assert_eq!(reason, expected_reason);
                    assert_eq!(content, expected_content);
                }
                other => {
                    panic!("expected explicit gap for {previous:?} -> {current:?}, got {other:?}")
                }
            }
        }

        let mut cursor = PaneCursor::new(41);
        let first = cursor
            .capture_snapshot("abc", 1024, None)
            .expect("initial snapshot");
        assert_eq!(first.kind, CapturedSegmentKind::Delta);

        let gap = cursor
            .capture_snapshot("bc", 1024, None)
            .expect("gap snapshot");
        assert_eq!(gap.seq, 1);
        assert_eq!(gap.content, "bc");
        assert!(cursor.in_gap);
        assert!(
            matches!(gap.kind, CapturedSegmentKind::Gap { ref reason } if reason == "content_changed_without_append")
        );
    }

    #[test]
    fn conformance_ingest_multi_window_stitching_preserves_ordering() {
        let windows = [
            "00 open\n01 plan\n",
            "01 plan\n02 build\n",
            "02 build\n03 test\n",
            "03 test\n04 ship\n",
        ];
        let expected = "00 open\n01 plan\n02 build\n03 test\n04 ship\n";
        let mut cursor = PaneCursor::new(9);
        let mut emitted = Vec::new();

        for window in windows {
            let segment = cursor
                .capture_snapshot(window, 1024, None)
                .expect("overlapping window should emit an ordered segment");
            assert_eq!(segment.seq as usize, emitted.len());
            assert_eq!(segment.kind, CapturedSegmentKind::Delta);
            emitted.push(segment);
        }

        let stitched: String = emitted
            .iter()
            .map(|segment| segment.content.as_str())
            .collect();

        assert_eq!(stitched, expected);
        assert_eq!(
            emitted
                .iter()
                .map(|segment| segment.seq)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(cursor.last_snapshot, windows.last().unwrap().to_string());
        assert_eq!(cursor.next_seq, windows.len() as u64);
    }

    /// ft-6lso5: after a restart the cursor resumes from a stored `next_seq`
    /// with no snapshot baseline. Without an anchor the first capture returned
    /// the pane's entire scrollback as a fresh `Delta`, so every observed pane
    /// re-stored everything it had already stored, on every restart, with no
    /// gap marker to say a discontinuity had happened.
    #[test]
    fn ft_6lso5_resumed_cursor_emits_only_output_captured_while_away() {
        // Pane already has "$ echo hello\nhello\n" stored as seq 0..=1.
        let mut cursor = PaneCursor::from_seq(3, 2).with_resume_anchor("$ echo hello\nhello\n");
        assert!(cursor.has_resume_anchor());

        let segment = cursor
            .capture_snapshot("$ echo hello\nhello\n$ echo bye\nbye\n", 1024, None)
            .expect("new output must be captured");

        assert_eq!(segment.kind, CapturedSegmentKind::Delta);
        assert_eq!(
            segment.content, "$ echo bye\nbye\n",
            "only output produced while the daemon was down may be stored"
        );
        assert_eq!(segment.seq, 2, "sequence continues from storage");
        assert!(
            !cursor.has_resume_anchor(),
            "the anchor describes resume time and must be consumed once"
        );

        // Subsequent captures use the ordinary snapshot path.
        let next = cursor
            .capture_snapshot("$ echo hello\nhello\n$ echo bye\nbye\nmore\n", 1024, None)
            .expect("second capture");
        assert_eq!(next.content, "more\n");
        assert_eq!(next.kind, CapturedSegmentKind::Delta);
    }

    /// ft-6lso5: a pane that produced nothing while the daemon was down must
    /// produce no segment at all, rather than re-storing its scrollback.
    #[test]
    fn ft_6lso5_resumed_cursor_with_no_new_output_emits_nothing() {
        let mut cursor = PaneCursor::from_seq(3, 2).with_resume_anchor("$ echo hello\nhello\n");

        assert!(
            cursor
                .capture_snapshot("$ echo hello\nhello\n", 1024, None)
                .is_none(),
            "nothing new means nothing to store"
        );
        assert_eq!(cursor.next_seq, 2, "no sequence number may be consumed");
    }

    /// ft-6lso5: if the persisted tail is no longer in the pane's scrollback the
    /// discontinuity is real, and must be recorded as a gap instead of being
    /// silently re-stored as a delta.
    #[test]
    fn ft_6lso5_resumed_cursor_reports_a_gap_when_the_anchor_scrolled_off() {
        let mut cursor = PaneCursor::from_seq(3, 2).with_resume_anchor("$ echo hello\nhello\n");

        let segment = cursor
            .capture_snapshot("totally different scrollback\n", 1024, None)
            .expect("discontinuity must be captured");

        match segment.kind {
            CapturedSegmentKind::Gap { ref reason } => {
                assert_eq!(reason, "resume_anchor_not_found");
            }
            other @ CapturedSegmentKind::Delta => {
                panic!("expected an explicit gap, got {other:?}")
            }
        }
        assert_eq!(segment.content, "totally different scrollback\n");
        assert!(cursor.in_gap, "cursor must record that it is in a gap");
    }

    #[test]
    fn generation_resync_emits_one_gap_with_only_post_anchor_bytes() {
        let mut cursor = PaneCursor::from_seq(3, 9).with_resume_anchor("durable tail\n");

        let resync = cursor.capture_generation_resync(
            "older output\ndurable tail\nvisible successor bytes\n",
            "capture_generation_resync",
        );

        assert_eq!(resync.seq, 9);
        assert_eq!(resync.content, "visible successor bytes\n");
        assert!(matches!(
            resync.kind,
            CapturedSegmentKind::Gap { ref reason }
                if reason == "capture_generation_resync"
        ));
        assert_eq!(cursor.next_seq, 10);
        assert!(cursor.in_gap);

        let next = cursor
            .capture_snapshot(
                "older output\ndurable tail\nvisible successor bytes\nnormal delta\n",
                1024,
                None,
            )
            .expect("post-resync output");
        assert_eq!(next.seq, 10);
        assert_eq!(next.content, "normal delta\n");
        assert_eq!(next.kind, CapturedSegmentKind::Delta);
    }

    #[test]
    fn generation_resync_is_honest_when_durable_anchor_is_unavailable() {
        let mut missing = PaneCursor::from_seq(7, 4).with_resume_anchor("scrolled away\n");
        let missing_gap = missing.capture_generation_resync(
            "only visible successor scrollback\n",
            "capture_generation_resync",
        );
        assert_eq!(missing_gap.content, "only visible successor scrollback\n");
        assert!(matches!(
            missing_gap.kind,
            CapturedSegmentKind::Gap { ref reason }
                if reason == "capture_generation_resync:resume_anchor_not_found"
        ));

        let mut absent = PaneCursor::from_seq(8, 0);
        let absent_gap = absent
            .capture_generation_resync("first visible snapshot\n", "capture_generation_resync");
        assert_eq!(absent_gap.content, "first visible snapshot\n");
        assert!(matches!(
            absent_gap.kind,
            CapturedSegmentKind::Gap { ref reason }
                if reason == "capture_generation_resync:durable_anchor_unavailable"
        ));
    }

    #[test]
    fn unproven_replacement_resync_keeps_full_snapshot_despite_common_prompt() {
        let mut replacement = PaneCursor::from_seq(9, 12);
        let successor = "successor banner\n$ common prompt\nvaluable successor output\n";

        let gap = replacement.capture_generation_resync(successor, "capture_generation_resync");

        assert_eq!(gap.content, successor);
        assert!(matches!(
            gap.kind,
            CapturedSegmentKind::Gap { ref reason }
                if reason == "capture_generation_resync:durable_anchor_unavailable"
        ));
    }

    /// ft-6lso5: a tail that repeats in the scrollback (a prompt line, a
    /// progress banner) must resume from its most recent occurrence, or the
    /// output in between is stored twice — the very duplication being fixed.
    #[test]
    fn ft_6lso5_repeated_anchor_resumes_from_the_last_occurrence() {
        let mut cursor = PaneCursor::from_seq(3, 5).with_resume_anchor("$ ");

        let segment = cursor
            .capture_snapshot("$ one\n$ two\n$ three\n", 1024, None)
            .expect("capture");

        assert_eq!(segment.content, "three\n");
    }

    /// ft-6lso5: an empty anchor means nothing is persisted, so the pane is
    /// genuinely new and the whole capture is legitimately fresh content.
    #[test]
    fn ft_6lso5_empty_anchor_is_ignored() {
        let mut cursor = PaneCursor::from_seq(3, 0).with_resume_anchor("");
        assert!(!cursor.has_resume_anchor());

        let segment = cursor
            .capture_snapshot("first output\n", 1024, None)
            .expect("capture");
        assert_eq!(segment.content, "first output\n");
        assert_eq!(segment.kind, CapturedSegmentKind::Delta);
    }

    /// ft-6lso5: the anchor tail is byte-bounded and must never split a
    /// multibyte character.
    #[test]
    fn ft_6lso5_resume_anchor_tail_snaps_to_char_boundary() {
        assert_eq!(resume_anchor_tail("short", 64), "short");
        assert_eq!(resume_anchor_tail("abcdef", 3), "def");
        // '🌍' is 4 bytes; a 2-byte window cannot include it partially.
        assert_eq!(resume_anchor_tail("ab🌍", 2), "");
        assert_eq!(resume_anchor_tail("ab🌍", 4), "🌍");
    }

    #[test]
    fn capture_snapshot_assigns_monotonic_seq() {
        let mut cursor = PaneCursor::new(7);

        let seg0 = cursor
            .capture_snapshot("a\n", 1024, None)
            .expect("first capture");
        assert_eq!(seg0.seq, 0);
        assert_eq!(seg0.pane_id, 7);
        assert_eq!(seg0.kind, CapturedSegmentKind::Delta);
        assert_eq!(seg0.content, "a\n");

        let seg1 = cursor
            .capture_snapshot("a\nb\n", 1024, None)
            .expect("second capture");
        assert_eq!(seg1.seq, 1);
        assert_eq!(seg1.kind, CapturedSegmentKind::Delta);
        assert_eq!(seg1.content, "b\n");

        // No change shouldn't emit a segment or advance seq
        assert!(cursor.capture_snapshot("a\nb\n", 1024, None).is_none());
        assert_eq!(cursor.next_seq, 2);

        // In-place edit triggers a gap segment with full snapshot content
        let seg2 = cursor
            .capture_snapshot("a\nc\n", 1024, None)
            .expect("gap capture");
        assert_eq!(seg2.seq, 2);
        assert!(matches!(seg2.kind, CapturedSegmentKind::Gap { .. }));
        assert_eq!(seg2.content, "a\nc\n");
    }

    #[test]
    fn persist_captured_segments_appends_rows() {
        run_async_test(async {
            let db_path = temp_db_path();
            let handle = StorageHandle::new(&db_path).await.unwrap();
            handle.upsert_pane(test_pane_record(1)).await.unwrap();

            let mut cursor = PaneCursor::new(1);
            let seg0 = cursor
                .capture_snapshot("hello\n", 1024, None)
                .expect("first capture");
            let seg1 = cursor
                .capture_snapshot("hello\nworld\n", 1024, None)
                .expect("second capture");

            let stored0 = persist_captured_segment(&handle, &seg0, TEST_MAX_PERSIST_SEGMENT_BYTES)
                .await
                .unwrap();
            let stored1 = persist_captured_segment(&handle, &seg1, TEST_MAX_PERSIST_SEGMENT_BYTES)
                .await
                .unwrap();

            assert_eq!(stored0.segment.seq, seg0.seq);
            assert_eq!(stored1.segment.seq, seg1.seq);

            let segments = handle.get_segments(1, 10).await.unwrap();
            assert_eq!(segments.len(), 2);
            assert!(segments.iter().any(|seg| seg.content == "hello\n"));
            assert!(segments.iter().any(|seg| seg.content == "world\n"));

            handle.shutdown().await.unwrap();
            cleanup_db(&db_path);
        });
    }

    #[test]
    fn persist_captured_segment_with_zone_stamps_output_segment() {
        run_async_test(async {
            let db_path = temp_db_path();
            let handle = StorageHandle::new(&db_path).await.unwrap();
            handle.upsert_pane(test_pane_record(1)).await.unwrap();

            let mut cursor = PaneCursor::new(1);
            let seg = cursor
                .capture_snapshot("semantic zone row\n", 1024, None)
                .expect("capture");

            let stored = persist_captured_segment_with_zone(
                &handle,
                &seg,
                TEST_MAX_PERSIST_SEGMENT_BYTES,
                Some("output"),
            )
            .await
            .unwrap();
            assert_eq!(stored.segment.seq, seg.seq);

            handle.shutdown().await.unwrap();
            let conn = rusqlite::Connection::open(&db_path).expect("open persisted db");
            let zone_type: Option<String> = conn
                .query_row(
                    "SELECT zone_type FROM output_segments WHERE pane_id = ?1 AND seq = ?2",
                    (1_i64, i64::try_from(stored.segment.seq).unwrap()),
                    |row| row.get(0),
                )
                .expect("read stamped zone_type");
            assert_eq!(zone_type.as_deref(), Some("output"));
            cleanup_db(&db_path);
        });
    }

    #[test]
    fn append_captured_segment_to_mmap_scrollback_redacts_bounded_payload() {
        let counter = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "ft_z4u60_ingest_mmap_{}_{}",
            std::process::id(),
            counter
        ));
        let mut writer = crate::scrollback_mmap_writer::MmapScrollback::open(
            crate::scrollback_mmap_writer::MmapScrollbackConfig::new(&dir, "pane-ingest")
                .with_cap_bytes(4096)
                .with_sync_every_appends(1),
        )
        .expect("open mmap writer");
        let captured = CapturedSegment {
            pane_id: 7,
            seq: 11,
            seq_correction: 0,
            content: "alpha sk-abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN omega".to_string(),
            kind: CapturedSegmentKind::Delta,
            captured_at: epoch_ms(),
        };

        let report = append_captured_segment_to_mmap_scrollback(
            &mut writer,
            &captured,
            TEST_MAX_PERSIST_SEGMENT_BYTES,
        )
        .expect("append captured segment to mmap writer");
        assert!(report.redaction.replacement_count > 0);
        let path = writer.path().to_path_buf();
        drop(writer);

        let max_file_bytes = std::fs::metadata(&path)
            .expect("read mmap file metadata")
            .len();
        let records = crate::scrollback_mmap_writer::read_linear_records(
            &path,
            crate::scrollback_mmap_writer::LinearRecordReadLimits {
                max_file_bytes,
                max_records: 16,
                max_payload_bytes: max_file_bytes,
            },
        )
        .expect("read records")
        .records;
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].0,
            crate::scrollback_mmap_format::RecordKind::Text
        );
        assert!(!records[0].1.windows(3).any(|window| window == b"sk-"));
        assert!(
            records[0]
                .1
                .windows(b"[REDACTED]".len())
                .any(|window| window == b"[REDACTED]")
        );
    }

    /// ft-xbnl0.2.3 Cx-first:
    /// `persist_captured_segment_with_cx` must match
    /// `persist_captured_segment` on the basic append flow.
    #[test]
    fn persist_captured_segment_with_cx_matches_legacy() {
        run_async_test(async {
            let db_path = temp_db_path();
            let handle = StorageHandle::new(&db_path).await.unwrap();
            handle.upsert_pane(test_pane_record(1)).await.unwrap();

            let mut cursor = PaneCursor::new(1);
            let seg0 = cursor
                .capture_snapshot("cx-hello\n", 1024, None)
                .expect("first capture");

            let cx = crate::cx::for_request();
            let stored = persist_captured_segment_with_cx(
                &cx,
                &handle,
                &seg0,
                TEST_MAX_PERSIST_SEGMENT_BYTES,
            )
            .await
            .unwrap();

            assert_eq!(stored.segment.seq, seg0.seq);

            let segments = handle.get_segments(1, 10).await.unwrap();
            assert_eq!(segments.len(), 1);
            assert_eq!(segments[0].content, "cx-hello\n");

            handle.shutdown().await.unwrap();
            cleanup_db(&db_path);
        });
    }

    /// ft-gd4za: production scrollback persistence path was bypassing
    /// the redactor entirely, leaving credentials at rest in the SQLite
    /// `output_segments.content` column. Regression: persist a captured
    /// segment carrying a synthetic OpenAI-style key, then read back and
    /// confirm the stored content has been masked rather than written raw.
    #[test]
    fn persist_captured_segment_redacts_secret_in_storage() {
        run_async_test(async {
            let db_path = temp_db_path();
            let handle = StorageHandle::new(&db_path).await.unwrap();
            handle.upsert_pane(test_pane_record(1)).await.unwrap();

            let mut cursor = PaneCursor::new(1);
            let secret_payload = "log line: sk-abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN trailing\n";
            let seg = cursor
                .capture_snapshot(secret_payload, 4096, None)
                .expect("capture with secret");

            let persisted = persist_captured_segment(&handle, &seg, TEST_MAX_PERSIST_SEGMENT_BYTES)
                .await
                .unwrap();

            // Returned segment content reflects the redacted bytes that
            // landed in storage (append_segment echoes what it stored).
            assert!(
                !persisted.segment.content.contains("sk-abcdefghij"),
                "PersistedCapture.segment.content still carries raw secret: {:?}",
                persisted.segment.content
            );
            assert!(
                persisted.segment.content.contains("[REDACTED]"),
                "PersistedCapture.segment.content missing [REDACTED] marker: {:?}",
                persisted.segment.content
            );

            // Read back through the storage handle to confirm the row in
            // `output_segments` is also masked (defense-at-rest).
            let segments = handle.get_segments(1, 10).await.unwrap();
            assert_eq!(segments.len(), 1);
            assert!(
                !segments[0].content.contains("sk-abcdefghij"),
                "SQLite output_segments.content carries raw secret: {:?}",
                segments[0].content
            );
            assert!(
                segments[0].content.contains("[REDACTED]"),
                "SQLite output_segments.content missing [REDACTED] marker: {:?}",
                segments[0].content
            );

            handle.shutdown().await.unwrap();
            cleanup_db(&db_path);
        });
    }

    #[test]
    fn persist_captured_gap_records_gap() {
        run_async_test(async {
            let db_path = temp_db_path();
            let handle = StorageHandle::new(&db_path).await.unwrap();
            handle.upsert_pane(test_pane_record(1)).await.unwrap();

            let mut cursor = PaneCursor::new(1);
            let seg0 = cursor
                .capture_snapshot("a\nb\n", 1024, None)
                .expect("first capture");
            persist_captured_segment(&handle, &seg0, TEST_MAX_PERSIST_SEGMENT_BYTES)
                .await
                .unwrap();

            let gap_segment = cursor
                .capture_snapshot("a\nc\n", 1024, None)
                .expect("gap capture");
            let persisted =
                persist_captured_segment(&handle, &gap_segment, TEST_MAX_PERSIST_SEGMENT_BYTES)
                    .await
                    .unwrap();

            let gap = persisted.gap.expect("gap recorded");
            let expected_reason = match &gap_segment.kind {
                CapturedSegmentKind::Gap { reason } => reason.as_str(),
                CapturedSegmentKind::Delta => "unexpected_delta",
            };

            assert_eq!(gap.pane_id, 1);
            assert_eq!(gap.reason, expected_reason);
            assert_eq!(persisted.segment.seq, gap_segment.seq);
            assert_eq!(persisted.segment.content, "a\nc\n");

            handle.shutdown().await.unwrap();
            cleanup_db(&db_path);
        });
    }

    #[test]
    fn fresh_eyes_persist_initial_gap_records_gap_after_first_segment_exists() {
        run_async_test(async {
            let db_path = temp_db_path();
            let handle = StorageHandle::new(&db_path).await.unwrap();
            handle.upsert_pane(test_pane_record(1)).await.unwrap();

            let gap_segment = CapturedSegment {
                pane_id: 1,
                seq: 0,
                seq_correction: 0,
                content: "full snapshot after missed history\n".to_string(),
                kind: CapturedSegmentKind::Gap {
                    reason: "overlap_not_found".to_string(),
                },
                captured_at: 0,
            };

            let persisted =
                persist_captured_segment(&handle, &gap_segment, TEST_MAX_PERSIST_SEGMENT_BYTES)
                    .await
                    .unwrap();

            let gap = persisted
                .gap
                .expect("initial gap should be recorded after first segment insert");
            assert_eq!(persisted.segment.seq, 0);
            assert_eq!(gap.pane_id, 1);
            assert_eq!(gap.seq_before, 0);
            assert_eq!(gap.seq_after, 1);
            assert_eq!(gap.reason, "overlap_not_found");

            handle.shutdown().await.unwrap();
            cleanup_db(&db_path);
        });
    }

    #[test]
    fn fresh_eyes_persist_initial_gap_with_cx_records_gap_after_first_segment_exists() {
        run_async_test(async {
            let db_path = temp_db_path();
            let handle = StorageHandle::new(&db_path).await.unwrap();
            handle.upsert_pane(test_pane_record(1)).await.unwrap();

            let gap_segment = CapturedSegment {
                pane_id: 1,
                seq: 0,
                seq_correction: 0,
                content: "cx full snapshot after missed history\n".to_string(),
                kind: CapturedSegmentKind::Gap {
                    reason: "stream_overflow".to_string(),
                },
                captured_at: 0,
            };

            let cx = crate::cx::for_request();
            let persisted = persist_captured_segment_with_cx(
                &cx,
                &handle,
                &gap_segment,
                TEST_MAX_PERSIST_SEGMENT_BYTES,
            )
            .await
            .unwrap();

            let gap = persisted
                .gap
                .expect("initial cx gap should be recorded after first segment insert");
            assert_eq!(persisted.segment.seq, 0);
            assert_eq!(gap.pane_id, 1);
            assert_eq!(gap.seq_before, 0);
            assert_eq!(gap.seq_after, 1);
            assert_eq!(gap.reason, "stream_overflow");

            handle.shutdown().await.unwrap();
            cleanup_db(&db_path);
        });
    }

    #[test]
    fn persist_captured_segment_records_seq_discontinuity_gap() {
        run_async_test(async {
            let db_path = temp_db_path();
            let handle = StorageHandle::new(&db_path).await.unwrap();
            handle.upsert_pane(test_pane_record(1)).await.unwrap();

            // First, create a cursor and persist some segments normally
            let mut cursor = PaneCursor::new(1);
            let seg0 = cursor
                .capture_snapshot("line1\n", 1024, None)
                .expect("first capture");
            persist_captured_segment(&handle, &seg0, TEST_MAX_PERSIST_SEGMENT_BYTES)
                .await
                .unwrap();

            let seg1 = cursor
                .capture_snapshot("line1\nline2\n", 1024, None)
                .expect("second capture");
            persist_captured_segment(&handle, &seg1, TEST_MAX_PERSIST_SEGMENT_BYTES)
                .await
                .unwrap();

            // Now simulate a desync: manually advance the cursor's seq beyond what storage expects
            cursor.next_seq = 100; // Storage expects seq=2, cursor will produce seq=100

            let seg2 = cursor
                .capture_snapshot("line1\nline2\nline3\n", 1024, None)
                .expect("third capture");
            assert_eq!(seg2.seq, 100); // Cursor produced seq=100

            // Persist should NOT error, instead record a gap
            let persisted =
                persist_captured_segment(&handle, &seg2, TEST_MAX_PERSIST_SEGMENT_BYTES)
                    .await
                    .unwrap();

            // Storage used its own seq (2), not the cursor's (100)
            assert_eq!(persisted.segment.seq, 2);
            assert_eq!(persisted.segment.content, "line3\n");

            // A gap should have been recorded for the discontinuity
            let gap = persisted.gap.expect("discontinuity gap recorded");
            assert!(
                gap.reason.starts_with("seq_discontinuity:"),
                "reason should indicate seq discontinuity: {}",
                gap.reason
            );
            assert!(
                gap.reason.contains("expected=100"),
                "reason should include expected seq: {}",
                gap.reason
            );
            assert!(
                gap.reason.contains("actual=2"),
                "reason should include actual seq: {}",
                gap.reason
            );

            handle.shutdown().await.unwrap();
            cleanup_db(&db_path);
        });
    }

    #[test]
    fn realign_after_one_dropped_segment_converges_with_a_single_shift() {
        // Storage assigns seqs densely; a dropped persist leaves it one behind
        // the producer. Sequential case: no segment is in flight at realign.
        let mut cursor = PaneCursor::from_seq(1, 5);
        let mut storage_next = 5_u64;
        let mut assign = |captured: u64| -> (u64, bool) {
            let s = storage_next;
            storage_next += 1;
            (s, s != captured)
        };
        let dropped = cursor.capture_delta("a".into(), 1);
        assert_eq!(dropped.seq, 5); // persist fails: storage stays at 5
        let seg = cursor.capture_delta("b".into(), 2);
        assert_eq!(seg.seq, 6);
        let (storage_seq, mismatch) = assign(seg.seq);
        assert!(mismatch, "storage assigned {storage_seq} for captured 6");
        assert_eq!(cursor.realign_next_seq(&seg, storage_seq), -1);
        assert_eq!(cursor.seq_correction(), -1);
        assert!(cursor.in_gap);
        // Every capture after the shift lines up with storage.
        for i in 0..5_i64 {
            let seg = cursor.capture_delta(format!("c{i}"), 3 + i);
            let (storage_seq, mismatch) = assign(seg.seq);
            assert!(
                !mismatch,
                "captured {} but storage assigned {storage_seq}",
                seg.seq
            );
        }
    }

    #[test]
    fn realign_applies_the_offset_once_while_a_queued_backlog_drains() {
        // Pipelined case: three segments were captured (5, 6, 7) before the
        // persistence loop noticed that 5 was dropped. Old behaviour
        // (`resync_seq`) reset next_seq to storage+1 on each mismatch and the
        // producer kept colliding with in-flight numbering; the offset must
        // be applied exactly once and later queued segments must not move it.
        let mut cursor = PaneCursor::from_seq(1, 5);
        let queued: Vec<CapturedSegment> = (0..3)
            .map(|i| cursor.capture_delta(format!("q{i}"), i64::from(i)))
            .collect();
        assert_eq!(
            queued.iter().map(|segment| segment.seq).collect::<Vec<_>>(),
            vec![5, 6, 7]
        );
        assert_eq!(cursor.next_seq, 8);
        // persist(5) failed; storage assigns 5 to captured 6 and 6 to captured 7
        assert_eq!(cursor.realign_next_seq(&queued[1], 5), -1);
        assert_eq!(cursor.next_seq, 7, "one shift for the backlog");
        assert_eq!(
            cursor.realign_next_seq(&queued[2], 6),
            0,
            "same offset: no second shift"
        );
        assert_eq!(cursor.next_seq, 7);
        // The next fresh capture is numbered 7 and storage's next slot is 7.
        let fresh = cursor.capture_delta("fresh".into(), 10);
        assert_eq!(fresh.seq, 7);
    }

    #[test]
    fn realign_recovers_from_repeated_losses_after_each_backlog_drains() {
        let mut cursor = PaneCursor::from_seq(1, 5);
        let mut storage_next = 5;
        for round in 0..3 {
            let dropped = cursor.capture_delta(format!("dropped {round}"), round);
            assert_eq!(dropped.seq, storage_next);
            let first = cursor.capture_delta("queued first".into(), round);
            let second = cursor.capture_delta("queued second".into(), round);
            assert_eq!(cursor.realign_next_seq(&first, storage_next), -1);
            storage_next += 1;
            assert_eq!(cursor.realign_next_seq(&second, storage_next), 0);
            storage_next += 1;
            assert_eq!(cursor.next_seq, storage_next);

            // A matching acknowledgement must preserve the cumulative stamp;
            // resetting it would invalidate captures already in flight.
            let fresh = cursor.capture_delta("fresh".into(), round);
            assert_eq!(fresh.seq, storage_next);
            assert_eq!(cursor.realign_next_seq(&fresh, storage_next), 0);
            assert_eq!(cursor.seq_correction(), -i128::from(round + 1));
            storage_next += 1;
            assert_eq!(cursor.next_seq, storage_next);
        }
    }

    #[test]
    fn realign_handles_storage_ahead_of_the_producer_and_never_underflows() {
        // A cursor resumed from a stale checkpoint numbers below what storage
        // already holds: the shift is positive.
        let mut cursor = PaneCursor::from_seq(1, 10);
        let seg = cursor.capture_delta("x".into(), 1);
        assert_eq!(seg.seq, 10);
        assert_eq!(cursor.realign_next_seq(&seg, 15), 5);
        assert_eq!(cursor.next_seq, 16);
        assert_eq!(cursor.seq_correction(), 5);
        // A shift that would push next_seq to or below zero clamps to the
        // slot after what storage assigned instead of wrapping.
        let mut low = PaneCursor::from_seq(2, 1);
        let seg = low.capture_delta("y".into(), 1);
        assert_eq!(seg.seq, 1);
        assert_eq!(low.next_seq, 2);
        assert_eq!(low.realign_next_seq(&seg, 0), -1);
        assert_eq!(low.next_seq, 1);
    }

    #[test]
    fn realign_recovers_when_first_fresh_capture_is_lost_without_matching_ack() {
        let mut cursor = PaneCursor::from_seq(1, 5);
        let mut storage_next = 5;
        for round in 0..3 {
            let dropped = cursor.capture_delta("lost".into(), round);
            assert_eq!(dropped.seq, storage_next);
            let first = cursor.capture_delta("queued first".into(), round);
            let second = cursor.capture_delta("queued second".into(), round);
            assert_eq!(cursor.realign_next_seq(&first, storage_next), -1);
            storage_next += 1;
            assert_eq!(cursor.realign_next_seq(&second, storage_next), 0);
            storage_next += 1;
            assert_eq!(cursor.next_seq, storage_next);
            // No matching acknowledgement occurs between these losses.
        }
        let fresh = cursor.capture_delta("finally durable".into(), 4);
        assert_eq!(fresh.seq, storage_next);
        assert_eq!(cursor.realign_next_seq(&fresh, storage_next), 0);
    }

    #[test]
    fn realign_late_old_backlog_loss_does_not_double_shift_new_captures() {
        let mut cursor = PaneCursor::from_seq(1, 5);
        let old: Vec<_> = (0..4)
            .map(|i| cursor.capture_delta(format!("old {i}"), i))
            .collect();
        // Lose old 5, acknowledge old 6 as storage 5.
        assert_eq!(cursor.realign_next_seq(&old[1], 5), -1);
        let first_new = cursor.capture_delta("new 8".into(), 4);
        let second_new = cursor.capture_delta("new 9".into(), 5);
        assert_eq!((first_new.seq, first_new.seq_correction), (8, -1));
        assert_eq!((second_new.seq, second_new.seq_correction), (9, -1));
        // Lose old 7 after newer captures were already issued.
        assert_eq!(cursor.realign_next_seq(&old[3], 6), -1);
        assert_eq!(cursor.next_seq, 9);
        let fresh = cursor.capture_delta("fresh 9".into(), 6);
        assert_eq!((fresh.seq, fresh.seq_correction), (9, -2));
        assert!(!cursor.in_gap);
        assert_eq!(cursor.realign_next_seq(&first_new, 7), 0);
        assert!(
            cursor.in_gap,
            "known-offset backlog still carries a durable gap"
        );
        assert_eq!(cursor.realign_next_seq(&second_new, 8), 0);
        assert_eq!(cursor.next_seq, 10);
        assert_eq!(cursor.realign_next_seq(&fresh, 9), 0);
    }

    #[test]
    fn realign_supports_corrections_beyond_i64_without_repeating_them() {
        let mut cursor = PaneCursor::from_seq(1, u64::MAX - 2);
        let first = cursor.capture_delta("old first".into(), 0);
        let second = cursor.capture_delta("old second".into(), 1);
        assert_eq!(cursor.realign_next_seq(&first, 0), -i128::from(first.seq));
        assert_eq!(cursor.next_seq, 2);
        assert_eq!(cursor.realign_next_seq(&second, 1), 0);
        assert_eq!(cursor.next_seq, 2);
        let fresh = cursor.capture_delta("fresh".into(), 2);
        assert_eq!(cursor.realign_next_seq(&fresh, 2), 0);
        assert_eq!(cursor.next_seq, 3);
    }

    #[test]
    fn resync_seq_aligns_cursor_with_storage() {
        run_async_test(async {
            let db_path = temp_db_path();
            let handle = StorageHandle::new(&db_path).await.unwrap();
            handle.upsert_pane(test_pane_record(1)).await.unwrap();

            // Create a cursor and persist some segments normally
            let mut cursor = PaneCursor::new(1);
            let seg0 = cursor
                .capture_snapshot("a\n", 1024, None)
                .expect("first capture");
            persist_captured_segment(&handle, &seg0, TEST_MAX_PERSIST_SEGMENT_BYTES)
                .await
                .unwrap();

            // Simulate desync
            cursor.next_seq = 999;

            let seg1 = cursor
                .capture_snapshot("a\nb\n", 1024, None)
                .expect("second capture");
            assert_eq!(seg1.seq, 999);

            let persisted =
                persist_captured_segment(&handle, &seg1, TEST_MAX_PERSIST_SEGMENT_BYTES)
                    .await
                    .unwrap();
            assert_eq!(persisted.segment.seq, 1); // Storage used seq=1

            // Resync cursor to storage
            cursor.resync_seq(persisted.segment.seq);
            assert_eq!(cursor.next_seq, 2); // Should be storage_seq + 1
            assert!(cursor.in_gap); // Should be marked in gap state

            // Next capture should be aligned
            let seg2 = cursor
                .capture_snapshot("a\nb\nc\n", 1024, None)
                .expect("third capture");
            assert_eq!(seg2.seq, 2);

            let persisted2 =
                persist_captured_segment(&handle, &seg2, TEST_MAX_PERSIST_SEGMENT_BYTES)
                    .await
                    .unwrap();
            assert_eq!(persisted2.segment.seq, 2);
            // No gap this time since we resynced
            assert!(persisted2.gap.is_none());

            handle.shutdown().await.unwrap();
            cleanup_db(&db_path);
        });
    }

    #[test]
    fn enforce_segment_size_for_persistence_promotes_delta_to_gap() {
        let captured = CapturedSegment {
            pane_id: 1,
            seq: 3,
            seq_correction: -7,
            content: "abc0123456789".to_string(),
            kind: CapturedSegmentKind::Delta,
            captured_at: 0,
        };

        let (bounded, enforcement) = enforce_segment_size_for_persistence(&captured, 5);
        assert_eq!(bounded.seq_correction, -7);
        let enforcement = enforcement.expect("size enforcement expected");

        assert_eq!(enforcement.original_bytes, captured.content.len());
        assert_eq!(enforcement.kept_bytes, bounded.content.len());
        assert_eq!(enforcement.max_bytes, 5);
        assert_eq!(bounded.content, "56789");
        assert_eq!(bounded.seq, captured.seq);
        assert_eq!(bounded.pane_id, captured.pane_id);
        match bounded.kind {
            CapturedSegmentKind::Gap { reason } => {
                assert!(reason.contains("segment_truncated:original_bytes="));
                assert!(reason.contains("max_bytes=5"));
            }
            CapturedSegmentKind::Delta => panic!("oversized segment must be promoted to gap"),
        }
    }

    #[test]
    fn persist_captured_oversized_delta_records_truncation_gap() {
        run_async_test(async {
            let db_path = temp_db_path();
            let handle = StorageHandle::new(&db_path).await.unwrap();
            handle.upsert_pane(test_pane_record(1)).await.unwrap();

            let max_segment_bytes = 32;
            let oversized_content = format!("HEADER:{}", "x".repeat(max_segment_bytes + 48));
            let expected_tail = trim_utf8_tail_to_max_bytes(&oversized_content, max_segment_bytes);
            let oversized = CapturedSegment {
                pane_id: 1,
                seq: 0,
                seq_correction: 0,
                content: oversized_content,
                kind: CapturedSegmentKind::Delta,
                captured_at: 0,
            };

            let persisted = persist_captured_segment(&handle, &oversized, max_segment_bytes)
                .await
                .unwrap();
            let gap = persisted.gap.expect("truncation should record gap");
            assert!(
                gap.reason.contains("segment_truncated:original_bytes="),
                "gap reason should include truncation marker: {}",
                gap.reason
            );
            assert!(gap.reason.contains("max_bytes=32"));
            assert_eq!(persisted.segment.content, expected_tail);
            assert_eq!(persisted.segment.content.len(), max_segment_bytes);

            handle.shutdown().await.unwrap();
            cleanup_db(&db_path);
        });
    }

    #[test]
    fn bounded_segment_for_persistence_uses_configured_limit() {
        let max_segment_bytes = 64;
        let oversized = CapturedSegment {
            pane_id: 9,
            seq: 1,
            seq_correction: -3,
            content: format!("prefix-{}", "x".repeat(max_segment_bytes + 17)),
            kind: CapturedSegmentKind::Delta,
            captured_at: 123,
        };

        let bounded = bounded_segment_for_persistence(&oversized, max_segment_bytes);
        assert_eq!(bounded.seq_correction, -3);
        assert_eq!(bounded.pane_id, oversized.pane_id);
        assert_eq!(bounded.seq, oversized.seq);
        assert_eq!(bounded.captured_at, oversized.captured_at);
        assert_eq!(bounded.content.len(), max_segment_bytes);

        match bounded.kind {
            CapturedSegmentKind::Gap { reason } => {
                assert!(reason.contains("segment_truncated:original_bytes="));
                assert!(reason.contains("max_bytes=64"));
            }
            CapturedSegmentKind::Delta => {
                panic!("bounded segment should promote oversized delta to gap")
            }
        }
    }

    // Helper to create a test PaneInfo
    fn make_pane(pane_id: u64, title: &str, cwd: Option<&str>) -> PaneInfo {
        PaneInfo {
            pane_id,
            tab_id: 1,
            window_id: 1,
            domain_id: None,
            domain_name: None,
            workspace: Some("default".to_string()),
            size: None,
            rows: None,
            cols: None,
            title: Some(title.to_string()),
            cwd: cwd.map(ToString::to_string),
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: true,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn lifecycle_identity_ignores_title_and_cwd() {
        let pane = make_pane(1, "vim", Some("/home/user"));
        let identity = PaneLifecycleIdentity::from_pane_info(&pane);
        let changed = make_pane(1, "nano", Some("/tmp"));
        let changed_identity = PaneLifecycleIdentity::from_pane_info(&changed);
        assert_eq!(
            identity.continuity_with(&changed_identity),
            PaneLifecycleContinuity::Same
        );
    }

    #[test]
    fn observation_decision_methods() {
        let observed = ObservationDecision::Observed;
        assert!(observed.is_observed());

        let ignored = ObservationDecision::Ignored {
            reason: "test".to_string(),
        };
        assert!(!ignored.is_observed());
    }

    #[test]
    fn pane_entry_creation_and_update() {
        let pane = make_pane(1, "bash", Some("/home"));
        let identity = PaneLifecycleIdentity::from_pane_info(&pane);
        let pane_arena = PaneArenaRegistry::new().reserve(1).arena();
        let entry = PaneEntry::new(pane, identity, ObservationDecision::Observed, pane_arena);

        assert_eq!(entry.info.pane_id, 1);
        assert!(entry.should_observe());
        assert_eq!(entry.lifecycle_revision.get(), 0);

        let mut entry = entry;
        let new_pane = make_pane(1, "vim", Some("/home/projects"));
        entry.update_info(new_pane);

        assert_eq!(entry.info.title, Some("vim".to_string()));
        assert_eq!(entry.info.cwd, Some("/home/projects".to_string()));
    }

    #[test]
    fn discovery_tick_detects_new_panes() {
        let mut registry = PaneRegistry::new();
        let panes = vec![
            make_pane(1, "bash", Some("/home")),
            make_pane(2, "vim", Some("/tmp")),
        ];

        let diff = registry.discovery_tick(panes);

        assert_eq!(diff.new_panes.len(), 2);
        assert!(diff.new_panes.contains(&1));
        assert!(diff.new_panes.contains(&2));
        assert_eq!(diff.closed_panes, [] as [u64; 0]);
        assert!(diff.metadata_changes.is_empty());
        assert_eq!(diff.lifecycle_replacements, [] as [u64; 0]);

        // Registry now tracks both panes
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn discovery_tick_detects_closed_panes() {
        let mut registry = PaneRegistry::new();

        // First tick: 2 panes
        let panes = vec![
            make_pane(1, "bash", Some("/home")),
            make_pane(2, "vim", Some("/tmp")),
        ];
        registry.discovery_tick(panes);
        assert_eq!(registry.len(), 2);

        // Second tick: pane 2 is gone
        let panes = vec![make_pane(1, "bash", Some("/home"))];
        let diff = registry.discovery_tick(panes);

        assert_eq!(diff.new_panes, [] as [u64; 0]);
        assert_eq!(diff.closed_panes.len(), 1);
        assert!(diff.closed_panes.contains(&2));

        // Closed panes are removed from entries
        assert_eq!(registry.len(), 1);
        assert!(registry.get_pane(1).is_some());
        assert!(registry.get_pane(2).is_none());
    }

    #[test]
    fn discovery_tick_classifies_title_change_as_metadata() {
        let mut registry = PaneRegistry::new();

        // First tick: pane with title "bash"
        let panes = vec![make_pane(1, "bash", Some("/home"))];
        registry.discovery_tick(panes);
        let entry = registry.entries.get(&1).unwrap();
        assert_eq!(entry.lifecycle_revision.get(), 0);
        assert_eq!(entry.metadata_revision.get(), 0);

        // Second tick: same pane, title changed to "vim"
        let panes = vec![make_pane(1, "vim", Some("/home"))];
        let diff = registry.discovery_tick(panes);

        assert_eq!(diff.new_panes, [] as [u64; 0]);
        assert_eq!(diff.closed_panes, [] as [u64; 0]);
        assert_eq!(diff.metadata_changes.len(), 1);
        assert_eq!(diff.metadata_changes[0].pane_id, 1);
        assert_eq!(diff.lifecycle_replacements, [] as [u64; 0]);

        // Mutable metadata advances without rotating lifecycle authority.
        let entry = registry.entries.get(&1).unwrap();
        assert_eq!(entry.info.title, Some("vim".to_string()));
        assert_eq!(entry.lifecycle_revision.get(), 0);
        assert_eq!(entry.metadata_revision.get(), 1);
    }

    #[test]
    fn discovery_tick_detects_metadata_changes() {
        let mut registry = PaneRegistry::new();

        // First tick: pane in window 1
        let mut pane = make_pane(1, "bash", Some("/home"));
        pane.window_id = 1;
        pane.tab_id = 1;
        registry.discovery_tick(vec![pane]);

        // Second tick: same pane moved between window/tab placements.
        let mut pane = make_pane(1, "bash", Some("/home"));
        pane.window_id = 2;
        pane.tab_id = 2;
        let diff = registry.discovery_tick(vec![pane]);

        assert_eq!(diff.new_panes, [] as [u64; 0]);
        assert_eq!(diff.closed_panes, [] as [u64; 0]);
        assert_eq!(diff.metadata_changes.len(), 1);
        assert_eq!(diff.metadata_changes[0].pane_id, 1);
        assert_eq!(diff.lifecycle_replacements, [] as [u64; 0]);

        // Verify metadata was updated but lifecycle stayed the same.
        let entry = registry.entries.get(&1).unwrap();
        assert_eq!(entry.info.title, Some("bash".to_string()));
        assert_eq!(entry.info.cwd, Some("/home".to_string()));
        assert_eq!(entry.lifecycle_revision.get(), 0);
    }

    #[test]
    fn exact_lifecycle_evidence_replaces_once_and_display_name_never_does() {
        let mut registry = PaneRegistry::new();
        let mut initial = make_pane(7, "shell", Some("/home"));
        initial.domain_id = Some(11);
        initial.domain_name = Some("local-display-a".to_string());
        initial.tty_name = Some("/dev/pts/7".to_string());
        registry.discovery_tick(vec![initial.clone()]);

        let mut display_only = initial.clone();
        display_only.domain_name = Some("renamed-display".to_string());
        let display_diff = registry.discovery_tick(vec![display_only.clone()]);
        assert_eq!(display_diff.lifecycle_replacements, [] as [u64; 0]);
        assert_eq!(display_diff.metadata_changes.len(), 1);
        assert_eq!(registry.get_entry(7).unwrap().lifecycle_revision.get(), 0);

        let mut replacement = display_only;
        replacement.domain_id = Some(12);
        replacement.tty_name = Some("/dev/pts/8".to_string());
        let replacement_diff = registry.discovery_tick(vec![replacement.clone()]);
        assert_eq!(replacement_diff.lifecycle_replacements, vec![7]);
        assert_eq!(registry.get_entry(7).unwrap().lifecycle_revision.get(), 1);

        let stable_diff = registry.discovery_tick(vec![replacement]);
        assert_eq!(stable_diff.lifecycle_replacements, [] as [u64; 0]);
        assert!(stable_diff.metadata_changes.is_empty());
        assert_eq!(registry.get_entry(7).unwrap().lifecycle_revision.get(), 1);
    }

    #[test]
    fn newly_available_exact_evidence_is_metadata_without_replacement() {
        let mut registry = PaneRegistry::new();
        let initial = make_pane(8, "shell", Some("/home"));
        registry.discovery_tick(vec![initial.clone()]);

        let mut enriched = initial;
        enriched.domain_id = Some(44);
        enriched.tty_name = Some("/dev/pts/8".to_string());
        let diff = registry.discovery_tick(vec![enriched]);

        assert_eq!(diff.lifecycle_replacements, [] as [u64; 0]);
        assert_eq!(diff.metadata_changes.len(), 1);
        assert_ne!(
            diff.metadata_changes[0].diff.bits() & PaneMetadataDiff::IDENTITY_EVIDENCE,
            0
        );
        let entry = registry.get_entry(8).unwrap();
        assert_eq!(entry.lifecycle_revision.get(), 0);
        assert_eq!(entry.metadata_revision.get(), 1);
        assert_eq!(entry.lifecycle_identity.domain_id, Some(44));
        assert_eq!(
            entry.lifecycle_identity.tty_name.as_deref(),
            Some("/dev/pts/8")
        );
    }

    #[test]
    fn losing_exact_lifecycle_evidence_fails_closed_without_rotation() {
        let mut registry = PaneRegistry::new();
        let mut exact = make_pane(9, "shell", Some("/home"));
        exact.domain_id = Some(2);
        exact.tty_name = Some("/dev/pts/9".to_string());
        registry.discovery_tick(vec![exact.clone()]);

        let mut ambiguous = exact;
        ambiguous.domain_id = None;
        ambiguous.tty_name = None;
        let diff = registry.discovery_tick(vec![ambiguous]);

        assert_eq!(diff.ambiguous_lifecycle_panes, vec![9]);
        assert_eq!(diff.lifecycle_replacements, [] as [u64; 0]);
        let entry = registry.get_entry(9).unwrap();
        assert_eq!(entry.lifecycle_revision.get(), 0);
        assert!(!entry.should_observe());
        assert_eq!(
            entry.observation.ignore_reason(),
            Some("lifecycle_identity_ambiguous")
        );
    }

    #[test]
    fn duplicate_numeric_ids_fail_closed_once_without_arbitrary_replacement() {
        let pane_id = 13;
        let mut registry = PaneRegistry::new();
        let mut proven = make_pane(pane_id, "shell", Some("/home"));
        proven.domain_id = Some(8);
        proven.tty_name = Some("tty-proven".to_string());
        registry.discovery_tick(vec![proven.clone()]);
        registry.get_cursor_mut(pane_id).unwrap().next_seq = 7;

        let mut contradictory = proven.clone();
        contradictory.domain_id = Some(9);
        contradictory.tty_name = Some("tty-contradictory".to_string());
        let diff = registry.discovery_tick(vec![proven.clone(), contradictory]);

        assert_eq!(diff.ambiguous_lifecycle_panes, vec![pane_id]);
        assert_eq!(diff.lifecycle_replacements, [] as [u64; 0]);
        assert_eq!(diff.metadata_changes.len(), 1);
        let entry = registry.get_entry(pane_id).unwrap();
        assert_eq!(entry.lifecycle_identity.domain_id, Some(8));
        assert_eq!(
            entry.lifecycle_identity.tty_name.as_deref(),
            Some("tty-proven")
        );
        assert_eq!(entry.lifecycle_revision.get(), 0);
        assert_eq!(entry.resume_next_seq, 7);
        assert_eq!(
            entry.observation.ignore_reason(),
            Some("duplicate_pane_identity")
        );
        assert!(registry.get_cursor(pane_id).is_none());

        let repeated = registry.discovery_tick(vec![proven.clone(), proven]);
        assert_eq!(repeated.ambiguous_lifecycle_panes, vec![pane_id]);
        assert_eq!(repeated.lifecycle_replacements, [] as [u64; 0]);
        assert!(repeated.metadata_changes.is_empty());
        assert_eq!(
            registry
                .get_entry(pane_id)
                .unwrap()
                .lifecycle_revision
                .get(),
            0
        );
    }

    #[test]
    fn checked_revision_exhaustion_fails_closed() {
        let mut lifecycle_registry = PaneRegistry::new();
        let mut initial = make_pane(10, "shell", Some("/home"));
        initial.tty_name = Some("tty-a".to_string());
        lifecycle_registry.discovery_tick(vec![initial.clone()]);
        lifecycle_registry
            .entries
            .get_mut(&10)
            .unwrap()
            .lifecycle_revision = PaneLifecycleRevision::new(u32::MAX);
        let mut replacement = initial;
        replacement.tty_name = Some("tty-b".to_string());
        let lifecycle_diff = lifecycle_registry.discovery_tick(vec![replacement]);
        assert_eq!(lifecycle_diff.revision_exhausted_panes, vec![10]);
        assert_eq!(lifecycle_diff.lifecycle_replacements, [] as [u64; 0]);
        assert!(!lifecycle_registry.get_entry(10).unwrap().should_observe());
        let lifecycle_repeat = lifecycle_registry.discovery_tick(vec![make_pane(
            10,
            "changed-again",
            Some("/still-terminal"),
        )]);
        assert!(lifecycle_repeat.is_empty());

        let mut metadata_registry = PaneRegistry::new();
        metadata_registry.discovery_tick(vec![make_pane(11, "shell", Some("/home"))]);
        metadata_registry
            .entries
            .get_mut(&11)
            .unwrap()
            .metadata_revision = PaneMetadataRevision(u64::MAX);
        let metadata_diff =
            metadata_registry.discovery_tick(vec![make_pane(11, "editor", Some("/tmp"))]);
        assert_eq!(metadata_diff.revision_exhausted_panes, vec![11]);
        assert!(metadata_diff.metadata_changes.is_empty());
        assert!(!metadata_registry.get_entry(11).unwrap().should_observe());
        let metadata_repeat = metadata_registry.discovery_tick(vec![make_pane(
            11,
            "changed-again",
            Some("/still-terminal"),
        )]);
        assert!(metadata_repeat.is_empty());
        let _ = metadata_registry.set_filter(PaneFilterConfig::default());
        assert!(
            !metadata_registry.get_entry(11).unwrap().should_observe(),
            "filter refresh cannot bypass checked revision exhaustion"
        );
        assert!(metadata_registry.get_cursor(11).is_none());
    }

    #[test]
    fn q600_revision_exhaustion_emits_once_per_member_without_duplicate_scan() {
        let mut registry = PaneRegistry::new();
        let initial = (0..600_u64)
            .map(|pane_id| make_pane(pane_id, "shell", Some("/home")))
            .collect::<Vec<_>>();
        registry.discovery_tick(initial);
        for pane_id in 0..600_u64 {
            registry.get_entry_mut(pane_id).unwrap().metadata_revision =
                PaneMetadataRevision::new(u64::MAX);
        }

        let changed = (0..600_u64)
            .map(|pane_id| make_pane(pane_id, "editor", Some("/tmp")))
            .collect::<Vec<_>>();
        let exhausted = registry.discovery_tick(changed.clone());
        assert_eq!(exhausted.revision_exhausted_panes.len(), 600);
        assert_eq!(
            exhausted.revision_exhausted_panes,
            (0..600_u64).collect::<Vec<_>>()
        );
        assert!(exhausted.metadata_changes.is_empty());

        let repeated = registry.discovery_tick(changed);
        assert!(
            repeated.is_empty(),
            "terminal saturation is one-shot for every member"
        );
    }

    #[test]
    fn filter_rule_id_cannot_impersonate_revision_exhaustion() {
        for (pane_id, rule_id) in [
            (12, "metadata_revision_exhausted"),
            (13, "lifecycle_revision_exhausted"),
        ] {
            let mut registry = PaneRegistry::with_filter(PaneFilterConfig {
                include: Vec::new(),
                exclude: vec![crate::config::PaneFilterRule {
                    id: rule_id.to_string(),
                    domain: None,
                    title: Some("blocked".to_string()),
                    cwd: None,
                }],
            });
            registry.discovery_tick(vec![make_pane(pane_id, "blocked", Some("/home"))]);
            let entry = registry.get_entry(pane_id).unwrap();
            assert_eq!(entry.observation.ignore_reason(), Some(rule_id));
            assert_eq!(entry.lifecycle_revision.get(), 0);
            assert_eq!(entry.metadata_revision.get(), 0);

            let _ = registry.set_filter(PaneFilterConfig::default());
            assert!(registry.get_entry(pane_id).unwrap().should_observe());
            assert!(registry.get_cursor(pane_id).is_some());
        }
    }

    #[test]
    fn metadata_churn_scales_with_changed_members_not_lifecycle() {
        for pane_count in [2_u64, 20, 200, 600] {
            let mut registry = PaneRegistry::new();
            registry.discovery_tick(
                (0..pane_count)
                    .map(|pane_id| make_pane(pane_id, "shell", Some("/home")))
                    .collect(),
            );
            let before = registry
                .entries
                .values()
                .map(|entry| entry.lifecycle_revision)
                .collect::<Vec<_>>();

            let diff = registry.discovery_tick(
                (0..pane_count)
                    .map(|pane_id| {
                        let mut pane = make_pane(pane_id, "editor", Some("/tmp"));
                        pane.window_id = pane_id.saturating_add(10);
                        pane.is_zoomed = pane_id % 2 == 0;
                        pane
                    })
                    .collect(),
            );

            assert_eq!(
                diff.metadata_changes.len(),
                usize::try_from(pane_count).expect("fixture pane count fits usize")
            );
            assert_eq!(diff.lifecycle_replacements, [] as [u64; 0]);
            assert_eq!(diff.revision_exhausted_panes, [] as [u64; 0]);
            assert_eq!(
                registry
                    .entries
                    .values()
                    .map(|entry| entry.lifecycle_revision)
                    .collect::<Vec<_>>(),
                before
            );
        }
    }

    #[test]
    fn rapid_title_cwd_oscillation_never_rotates_lifecycle_or_cursor() {
        let pane_id = 612;
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(pane_id, "alpha", Some("/alpha"))]);
        registry.get_cursor_mut(pane_id).unwrap().next_seq = 41;

        for tick in 1_u64..=128 {
            let (title, cwd) = if tick % 2 == 0 {
                ("alpha", "/alpha")
            } else {
                ("beta", "/beta")
            };
            let diff = registry.discovery_tick(vec![make_pane(pane_id, title, Some(cwd))]);
            assert_eq!(diff.metadata_changes.len(), 1, "metadata tick {tick}");
            assert!(
                diff.lifecycle_replacements.is_empty(),
                "lifecycle tick {tick}"
            );
            assert!(diff.re_observed_panes.is_empty(), "admission tick {tick}");
            let entry = registry.get_entry(pane_id).unwrap();
            assert_eq!(entry.lifecycle_revision.get(), 0);
            assert_eq!(entry.metadata_revision.get(), tick);
            assert_eq!(registry.get_cursor(pane_id).unwrap().next_seq, 41);
        }
    }

    #[test]
    fn discovery_tick_cursors_for_observed_panes() {
        let mut registry = PaneRegistry::new();
        let panes = vec![make_pane(1, "bash", Some("/home"))];

        registry.discovery_tick(panes);

        // Observed panes should have cursors
        assert!(registry.get_cursor(1).is_some());
    }

    /// ft-c87rx: the registry's cursors are what `ipc.rs` and `plan.rs` read,
    /// and the capture pipeline advances a different map. Publishing is the
    /// only thing that keeps them from reporting their initial values forever.
    #[test]
    fn publish_live_cursor_state_updates_capture_advanced_fields() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "vim", Some("/home"))]);

        let before = registry.get_cursor(1).expect("observed pane has a cursor");
        assert!(!before.in_gap, "fixture starts clean");
        assert!(!before.in_alt_screen);
        assert_eq!(before.next_seq, 0);

        registry.publish_live_cursor_state(&[LiveCursorState {
            pane_id: 1,
            next_seq: 42,
            in_gap: true,
            in_alt_screen: true,
        }]);

        let after = registry.get_cursor(1).expect("cursor still tracked");
        assert!(
            after.in_gap,
            "a capture-emitted gap must reach the policy-facing cursor"
        );
        assert!(after.in_alt_screen);
        assert_eq!(after.next_seq, 42);
    }

    /// The policy gate reads these two fields through
    /// `PaneCapabilities::from_ingest_state`. Pin the end-to-end mapping so a
    /// pane with a live capture gap cannot present as safe to write to.
    #[test]
    fn published_gap_state_reaches_policy_capabilities() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(7, "vim", Some("/home"))]);

        let clean = registry.get_cursor(7).expect("cursor");
        let clean_caps = crate::policy::PaneCapabilities::from_ingest_state(
            None,
            Some(clean.in_alt_screen),
            clean.in_gap,
        );
        assert!(!clean_caps.has_recent_gap);
        assert_eq!(clean_caps.alt_screen, Some(false));

        registry.publish_live_cursor_state(&[LiveCursorState {
            pane_id: 7,
            next_seq: 3,
            in_gap: true,
            in_alt_screen: true,
        }]);

        let live = registry.get_cursor(7).expect("cursor");
        let caps = crate::policy::PaneCapabilities::from_ingest_state(
            None,
            Some(live.in_alt_screen),
            live.in_gap,
        );
        assert!(
            caps.has_recent_gap,
            "policy must see the capture gap, not a permanent false"
        );
        assert_eq!(caps.alt_screen, Some(true));
    }

    /// A snapshot entry for a pane the registry has already retired must not
    /// resurrect a cursor — the registry owns the observation lifecycle.
    #[test]
    fn publish_live_cursor_state_ignores_untracked_panes() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "bash", Some("/home"))]);

        registry.publish_live_cursor_state(&[LiveCursorState {
            pane_id: 999,
            next_seq: 5,
            in_gap: true,
            in_alt_screen: true,
        }]);

        assert!(registry.get_cursor(999).is_none());
        assert!(registry.get_cursor(1).is_some());
    }

    #[test]
    fn discovery_tick_initializes_trauma_state_for_new_panes() {
        let mut registry = PaneRegistry::new();
        let panes = vec![
            make_pane(1, "bash", Some("/home")),
            make_pane(2, "vim", Some("/tmp")),
        ];

        registry.discovery_tick(panes);

        assert_eq!(registry.trauma_state_count(), 2);
        assert!(registry.get_trauma_state(1).is_some());
        assert!(registry.get_trauma_state(2).is_some());
    }

    #[test]
    fn record_trauma_command_result_tracks_recurrence() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "bash", Some("/home"))]);

        let signatures = vec!["core.codex:error_loop".to_string()];
        let first = registry
            .record_trauma_command_result(1, 1_000, "cargo test", &signatures)
            .unwrap();
        let second = registry
            .record_trauma_command_result(1, 1_100, "cargo test", &signatures)
            .unwrap();
        let third = registry
            .record_trauma_command_result(1, 1_200, "cargo test", &signatures)
            .unwrap();

        assert!(!first.should_intervene);
        assert!(!second.should_intervene);
        assert!(third.should_intervene);
        assert_eq!(third.reason_code.as_deref(), Some("recurring_failure_loop"));
    }

    #[test]
    fn record_trauma_command_result_skips_intervention_when_disabled() {
        let trauma_guard = crate::config::TraumaGuardConfig {
            enabled: false,
            ..crate::config::TraumaGuardConfig::default()
        };
        let mut registry =
            PaneRegistry::with_filter_and_trauma(PaneFilterConfig::default(), trauma_guard);
        registry.discovery_tick(vec![make_pane(1, "bash", Some("/home"))]);

        let signatures = vec!["core.codex:error_loop".to_string()];
        let first = registry
            .record_trauma_command_result(1, 1_000, "cargo test", &signatures)
            .unwrap();
        let second = registry
            .record_trauma_command_result(1, 1_100, "cargo test", &signatures)
            .unwrap();
        let third = registry
            .record_trauma_command_result(1, 1_200, "cargo test", &signatures)
            .unwrap();

        assert!(!first.should_intervene);
        assert!(!second.should_intervene);
        assert!(!third.should_intervene);
        assert_eq!(third.reason_code, None);
    }

    #[test]
    fn set_trauma_guard_config_reloads_thresholds() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "bash", Some("/home"))]);

        let signatures = vec!["core.codex:error_loop".to_string()];
        let _ = registry
            .record_trauma_command_result(1, 1_000, "cargo test", &signatures)
            .unwrap();
        let _ = registry
            .record_trauma_command_result(1, 1_100, "cargo test", &signatures)
            .unwrap();

        registry.set_trauma_guard_config(crate::config::TraumaGuardConfig {
            max_consecutive_failures: 2,
            ..crate::config::TraumaGuardConfig::default()
        });

        let first_after_reload = registry
            .record_trauma_command_result(1, 1_200, "cargo test", &signatures)
            .unwrap();
        let second_after_reload = registry
            .record_trauma_command_result(1, 1_300, "cargo test", &signatures)
            .unwrap();

        assert!(!first_after_reload.should_intervene);
        assert_eq!(first_after_reload.repeat_count, 1);
        assert!(second_after_reload.should_intervene);
        assert_eq!(second_after_reload.repeat_count, 2);
    }

    #[test]
    fn discovery_tick_removes_trauma_state_for_closed_panes() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "bash", Some("/home"))]);

        assert_eq!(registry.trauma_state_count(), 1);
        assert!(registry.get_trauma_state(1).is_some());

        registry.discovery_tick(vec![]);

        assert_eq!(registry.trauma_state_count(), 0);
        assert!(registry.get_trauma_state(1).is_none());
    }

    #[test]
    fn observation_decision_with_filters() {
        use crate::config::{PaneFilterConfig, PaneFilterRule};

        let mut filter_config = PaneFilterConfig::default();
        // Title matching uses substring (case-insensitive), not glob
        // "ignore-" as substring will match "ignore-me"
        filter_config.exclude.push(PaneFilterRule {
            id: "exclude-ignore".to_string(),
            domain: None,
            title: Some("ignore-".to_string()),
            cwd: None,
        });

        let mut registry = PaneRegistry::with_filter(filter_config);

        let panes = vec![
            make_pane(1, "bash", Some("/home")),
            make_pane(2, "ignore-me", Some("/tmp")),
        ];

        let diff = registry.discovery_tick(panes);

        // Both are new
        assert_eq!(diff.new_panes.len(), 2);

        // Pane 1 is observed (has cursor), pane 2 is ignored (no cursor)
        assert!(registry.get_cursor(1).is_some());
        assert!(registry.get_cursor(2).is_none());

        // Check observation status
        let entry1 = registry.entries.get(&1).unwrap();
        assert!(entry1.should_observe());

        let entry2 = registry.entries.get(&2).unwrap();
        assert!(!entry2.should_observe());
    }

    #[test]
    fn re_evaluate_observation_updates_cursors() {
        use crate::config::{PaneFilterConfig, PaneFilterRule};

        let filter_config = PaneFilterConfig::default();
        let mut registry = PaneRegistry::with_filter(filter_config);

        // Add a pane (initially observed)
        let panes = vec![make_pane(1, "bash", Some("/home"))];
        registry.discovery_tick(panes);
        assert!(registry.get_cursor(1).is_some());

        // Change filter to exclude this pane
        let mut new_filter = PaneFilterConfig::default();
        new_filter.exclude.push(PaneFilterRule {
            id: "exclude-bash".to_string(),
            domain: None,
            title: Some("bash".to_string()),
            cwd: None,
        });
        registry.filter_config = new_filter;

        // Re-evaluate
        registry.re_evaluate_observation(1);

        // Now should be ignored (no cursor)
        assert!(registry.get_cursor(1).is_none());
        let entry = registry.entries.get(&1).unwrap();
        assert!(!entry.should_observe());
    }

    #[test]
    fn set_filter_re_evaluates_existing_panes() {
        use crate::config::{PaneFilterConfig, PaneFilterRule};

        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "bash", Some("/home"))]);
        assert!(registry.get_cursor(1).is_some());

        let mut filter = PaneFilterConfig::default();
        filter.exclude.push(PaneFilterRule {
            id: "exclude-bash".to_string(),
            domain: None,
            title: Some("bash".to_string()),
            cwd: None,
        });

        let changes = registry.set_filter(filter);

        assert!(registry.get_cursor(1).is_none());
        let entry = registry.entries.get(&1).unwrap();
        assert!(!entry.should_observe());
        assert_eq!(entry.observation.ignore_reason(), Some("exclude-bash"),);
        assert_eq!(entry.metadata_revision.get(), 1);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].pane_id, 1);
        assert_eq!(
            changes[0].lifecycle_revision,
            PaneLifecycleRevision::INITIAL
        );
        assert_eq!(changes[0].metadata_revision.get(), 1);
        assert_ne!(changes[0].diff.bits() & PaneMetadataDiff::OBSERVATION, 0);

        let repeated = registry.set_filter(PaneFilterConfig {
            include: Vec::new(),
            exclude: vec![PaneFilterRule {
                id: "exclude-bash".to_string(),
                domain: None,
                title: Some("bash".to_string()),
                cwd: None,
            }],
        });
        assert!(repeated.is_empty(), "an identical policy state is a no-op");
        assert_eq!(registry.get_entry(1).unwrap().metadata_revision.get(), 1);

        let changed_reason = registry.set_filter(PaneFilterConfig {
            include: Vec::new(),
            exclude: vec![PaneFilterRule {
                id: "exclude-shells".to_string(),
                domain: None,
                title: Some("bash".to_string()),
                cwd: None,
            }],
        });
        assert_eq!(changed_reason.len(), 1);
        assert_eq!(changed_reason[0].metadata_revision.get(), 2);
        assert_eq!(
            registry.get_entry(1).unwrap().observation.ignore_reason(),
            Some("exclude-shells")
        );
    }

    #[test]
    fn re_evaluate_observation_resumes_monotonic_sequence() {
        use crate::config::{PaneFilterConfig, PaneFilterRule};

        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "bash", Some("/home"))]);
        registry.get_cursor_mut(1).unwrap().next_seq = 7;

        let mut exclude_bash = PaneFilterConfig::default();
        exclude_bash.exclude.push(PaneFilterRule {
            id: "exclude-bash".to_string(),
            domain: None,
            title: Some("bash".to_string()),
            cwd: None,
        });

        let _ = registry.set_filter(exclude_bash);
        assert!(registry.get_cursor(1).is_none());

        let _ = registry.set_filter(PaneFilterConfig::default());

        let cursor = registry.get_cursor(1).unwrap();
        assert_eq!(cursor.next_seq, 7);
    }

    #[test]
    fn discovery_tick_re_evaluates_observation_after_title_change() {
        use crate::config::{PaneFilterConfig, PaneFilterRule};

        let mut filter = PaneFilterConfig::default();
        filter.exclude.push(PaneFilterRule {
            id: "exclude-vim".to_string(),
            domain: None,
            title: Some("vim".to_string()),
            cwd: None,
        });

        let mut registry = PaneRegistry::with_filter(filter);
        registry.discovery_tick(vec![make_pane(1, "bash", Some("/home"))]);
        assert!(registry.get_cursor(1).is_some());

        registry.discovery_tick(vec![make_pane(1, "vim", Some("/home"))]);

        assert!(registry.get_cursor(1).is_none());
        let entry = registry.entries.get(&1).unwrap();
        assert!(!entry.should_observe());
        assert_eq!(entry.info.title.as_deref(), Some("vim"));
        assert_eq!(entry.observation.ignore_reason(), Some("exclude-vim"));
    }

    /// ft-0kdi9: an Observed -> Ignored -> Observed round trip must report the
    /// resumption in the diff and resume at the retired sequence number.
    ///
    /// The diff signal is the load-bearing half: the observation runtime keeps
    /// its own cursor map, compacts it against the observed set, and only
    /// creates cursors for `new_panes`. A re-observed pane is not a new pane, so
    /// without `re_observed_panes` the runtime never re-creates the cursor and
    /// every subsequent poll for that pane is discarded.
    #[test]
    fn ft_0kdi9_discovery_tick_reports_re_observed_pane_and_resumes_next_seq() {
        use crate::config::{PaneFilterConfig, PaneFilterRule};

        let mut filter = PaneFilterConfig::default();
        filter.exclude.push(PaneFilterRule {
            id: "exclude-vim".to_string(),
            domain: None,
            title: Some("vim".to_string()),
            cwd: None,
        });

        let mut registry = PaneRegistry::with_filter(filter);

        // Tick 1: first sighting is a new pane, not a resumption.
        let diff = registry.discovery_tick(vec![make_pane(1, "bash", Some("/home"))]);
        assert_eq!(diff.new_panes, vec![1]);
        assert_eq!(diff.re_observed_panes, [] as [u64; 0]);
        registry.get_cursor_mut(1).unwrap().next_seq = 7;

        // Tick 2: the title matches an exclude rule, so the pane retires.
        let diff = registry.discovery_tick(vec![make_pane(1, "vim", Some("/home"))]);
        assert_eq!(diff.re_observed_panes, [] as [u64; 0]);
        assert!(registry.get_cursor(1).is_none());
        assert_eq!(registry.entries.get(&1).unwrap().resume_next_seq, 7);

        // Tick 3: the title reverts. The pane is observed again but is NOT in
        // `new_panes`, which is exactly the case that used to be invisible.
        let diff = registry.discovery_tick(vec![make_pane(1, "bash", Some("/home"))]);
        assert!(
            diff.new_panes.is_empty(),
            "a re-observed pane must not be reported as newly discovered"
        );
        assert_eq!(
            diff.re_observed_panes,
            vec![1],
            "Ignored -> Observed must be reported so capture state can be rebuilt"
        );
        assert!(!diff.is_empty());
        assert!(
            diff.lifecycle_replacements.is_empty(),
            "a filter-driven title change is not a pane/process restart"
        );
        assert_eq!(diff.metadata_changes.len(), 1);
        assert_eq!(diff.change_count(), 2);
        assert!(registry.entries.get(&1).unwrap().should_observe());
        assert_eq!(
            registry.get_cursor(1).map(|cursor| cursor.next_seq),
            Some(7),
            "capture must resume at the retired sequence number, not restart at 0"
        );
    }

    /// ft-0kdi9: `re_evaluate_observation` reports the transition so callers
    /// that own capture-side state outside this registry can mirror it.
    #[test]
    fn ft_0kdi9_re_evaluate_observation_reports_transition() {
        use crate::config::{PaneFilterConfig, PaneFilterRule};

        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(1, "vim", Some("/home"))]);
        assert_eq!(
            registry.re_evaluate_observation(1),
            ObservationTransition::Unchanged
        );

        let mut filter = PaneFilterConfig::default();
        filter.exclude.push(PaneFilterRule {
            id: "exclude-vim".to_string(),
            domain: None,
            title: Some("vim".to_string()),
            cwd: None,
        });
        registry.filter_config = filter;
        assert_eq!(
            registry.re_evaluate_observation(1),
            ObservationTransition::Retired
        );
        assert_eq!(registry.get_entry(1).unwrap().metadata_revision.get(), 1);
        assert_eq!(
            registry.pane_arena_stats(1).unwrap().tracked_bytes,
            registry.get_entry(1).unwrap().estimated_bytes(),
            "direct filter re-evaluation must refresh logical arena accounting"
        );

        registry.filter_config = PaneFilterConfig::default();
        assert_eq!(
            registry.re_evaluate_observation(1),
            ObservationTransition::Resumed
        );
        assert_eq!(registry.get_entry(1).unwrap().metadata_revision.get(), 2);

        assert_eq!(
            registry.re_evaluate_observation(404),
            ObservationTransition::Unchanged,
            "an untracked pane has no transition to report"
        );
    }

    #[test]
    fn filter_re_evaluation_exhaustion_retires_capture_without_unversioned_revive() {
        use crate::config::{PaneFilterConfig, PaneFilterRule};

        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![make_pane(2, "shell", Some("/home"))]);
        registry.get_entry_mut(2).unwrap().metadata_revision = PaneMetadataRevision::new(u64::MAX);

        let filter = PaneFilterConfig {
            include: Vec::new(),
            exclude: vec![PaneFilterRule {
                id: "exclude-shell".to_string(),
                domain: None,
                title: Some("shell".to_string()),
                cwd: None,
            }],
        };
        let changes = registry.set_filter(filter);

        assert_eq!(
            changes.len(),
            1,
            "the saturated terminal state still needs one durable publication"
        );
        assert_eq!(changes[0].metadata_revision.get(), u64::MAX);
        let entry = registry.get_entry(2).unwrap();
        assert_eq!(entry.metadata_revision.get(), u64::MAX);
        assert_eq!(
            entry.observation.ignore_reason(),
            Some("metadata_revision_exhausted")
        );
        assert!(registry.get_cursor(2).is_none());
        assert_eq!(
            registry.re_evaluate_observation(2),
            ObservationTransition::Unchanged,
            "the terminal exhaustion fence cannot be revived"
        );
    }

    #[test]
    fn pane_entry_to_pane_record_observed() {
        let pane = make_pane(1, "bash", Some("/home/user"));
        let identity = PaneLifecycleIdentity::from_pane_info(&pane);
        let pane_arena = PaneArenaRegistry::new().reserve(1).arena();
        let entry = PaneEntry::new(pane, identity, ObservationDecision::Observed, pane_arena);

        let record = entry.to_pane_record();

        assert_eq!(record.pane_id, 1);
        assert_eq!(record.domain, "local");
        assert_eq!(record.title, Some("bash".to_string()));
        assert_eq!(record.cwd, Some("/home/user".to_string()));
        assert!(record.observed);
        assert!(record.ignore_reason.is_none());
        assert!(record.last_decision_at.is_some());
    }

    #[test]
    fn pane_entry_to_pane_record_ignored() {
        let pane = make_pane(2, "vim", Some("/tmp"));
        let identity = PaneLifecycleIdentity::from_pane_info(&pane);
        let pane_arena = PaneArenaRegistry::new().reserve(2).arena();
        let entry = PaneEntry::new(
            pane,
            identity,
            ObservationDecision::Ignored {
                reason: "exclude-vim".to_string(),
            },
            pane_arena,
        );

        let record = entry.to_pane_record();

        assert_eq!(record.pane_id, 2);
        assert!(!record.observed);
        assert_eq!(record.ignore_reason, Some("exclude-vim".to_string()));
    }

    #[test]
    fn registry_to_pane_records() {
        use crate::config::{PaneFilterConfig, PaneFilterRule};

        let mut filter_config = PaneFilterConfig::default();
        filter_config.exclude.push(PaneFilterRule {
            id: "skip-vim".to_string(),
            domain: None,
            title: Some("vim".to_string()),
            cwd: None,
        });

        let mut registry = PaneRegistry::with_filter(filter_config);

        let panes = vec![
            make_pane(1, "bash", Some("/home")),
            make_pane(2, "vim", Some("/tmp")),
            make_pane(3, "zsh", Some("/root")),
        ];

        registry.discovery_tick(panes);

        // All panes should be tracked
        let all_records = registry.to_pane_records();
        assert_eq!(all_records.len(), 3);

        // 2 observed (bash, zsh), 1 ignored (vim)
        let observed = registry.observed_pane_records();
        assert_eq!(observed.len(), 2);
        assert!(observed.iter().all(|r| r.observed));
        assert!(observed.iter().any(|r| r.pane_id == 1));
        assert!(observed.iter().any(|r| r.pane_id == 3));

        let ignored = registry.ignored_pane_records();
        assert_eq!(ignored.len(), 1);
        assert!(!ignored[0].observed);
        assert_eq!(ignored[0].pane_id, 2);
        assert_eq!(ignored[0].ignore_reason, Some("skip-vim".to_string()));
    }

    #[test]
    fn discovery_tick_tracks_pane_arena_lifecycle() {
        let mut registry = PaneRegistry::new();
        registry.discovery_tick(vec![
            make_pane(1, "bash", Some("/home")),
            make_pane(2, "vim", Some("/tmp")),
        ]);

        assert_eq!(registry.pane_arena_count(), 2);
        let first = registry.pane_arena(1).expect("pane 1 arena should exist");
        let second = registry.pane_arena(2).expect("pane 2 arena should exist");
        assert_eq!(first.pane_id(), 1);
        assert_eq!(second.pane_id(), 2);
        assert_ne!(first.arena_id(), second.arena_id());
        let first_stats = registry
            .pane_arena_stats(1)
            .expect("pane 1 stats should exist");
        let second_stats = registry
            .pane_arena_stats(2)
            .expect("pane 2 stats should exist");
        assert!(first_stats.tracked_bytes > 0);
        assert_eq!(first_stats.tracked_bytes, first_stats.peak_tracked_bytes);
        assert_eq!(first_stats.updates, 1);
        assert!(second_stats.tracked_bytes > 0);
        assert_eq!(second_stats.tracked_bytes, second_stats.peak_tracked_bytes);
        assert_eq!(second_stats.updates, 1);

        registry.discovery_tick(vec![make_pane(1, "bash", Some("/home"))]);

        assert_eq!(registry.pane_arena_count(), 1);
        assert!(registry.pane_arena(2).is_none());
        assert!(registry.pane_arena_stats(2).is_none());
        let snapshot = registry.pane_arenas_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].pane_id(), 1);
        let stats_snapshot = registry.pane_arena_stats_snapshot();
        assert_eq!(stats_snapshot.len(), 1);
        assert_eq!(stats_snapshot[0].arena.pane_id(), 1);
        let remaining_stats = registry
            .pane_arena_stats(1)
            .expect("pane 1 stats should still exist");
        assert!(remaining_stats.tracked_bytes > 0);
        assert!(remaining_stats.peak_tracked_bytes >= remaining_stats.tracked_bytes);
        assert!(remaining_stats.updates >= 1);
        assert_eq!(stats_snapshot[0].stats, remaining_stats);
    }

    // =========================================================================
    // OSC 133 Parser Tests
    // =========================================================================

    #[test]
    fn osc133_parse_prompt_start_bel() {
        // BEL terminator
        let markers = parse_osc133_markers("\x1b]133;A\x07");
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0], Osc133Marker::PromptStart);
    }

    #[test]
    fn osc133_parse_prompt_start_st() {
        // ESC \ terminator (ST)
        let markers = parse_osc133_markers("\x1b]133;A\x1b\\");
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0], Osc133Marker::PromptStart);
    }

    #[test]
    fn osc133_parse_command_start() {
        let markers = parse_osc133_markers("\x1b]133;B\x07");
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0], Osc133Marker::CommandStart);
    }

    #[test]
    fn osc133_parse_command_executed() {
        let markers = parse_osc133_markers("\x1b]133;C\x07");
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0], Osc133Marker::CommandExecuted);
    }

    #[test]
    fn osc133_parse_command_finished() {
        let markers = parse_osc133_markers("\x1b]133;D\x07");
        assert_eq!(markers.len(), 1);
        assert_eq!(
            markers[0],
            Osc133Marker::CommandFinished { exit_code: None }
        );
    }

    #[test]
    fn osc133_parse_command_finished_with_exit_code() {
        let markers = parse_osc133_markers("\x1b]133;D;0\x07");
        assert_eq!(markers.len(), 1);
        assert_eq!(
            markers[0],
            Osc133Marker::CommandFinished { exit_code: Some(0) }
        );

        let markers = parse_osc133_markers("\x1b]133;D;127\x07");
        assert_eq!(markers.len(), 1);
        assert_eq!(
            markers[0],
            Osc133Marker::CommandFinished {
                exit_code: Some(127)
            }
        );
    }

    #[test]
    fn osc133_parse_multiple_markers() {
        // Simulate full command cycle
        let input = "\x1b]133;A\x07$ ls\x1b]133;B\x07\x1b]133;C\x07file1 file2\n\x1b]133;D;0\x07";
        let markers = parse_osc133_markers(input);
        assert_eq!(markers.len(), 4);
        assert_eq!(markers[0], Osc133Marker::PromptStart);
        assert_eq!(markers[1], Osc133Marker::CommandStart);
        assert_eq!(markers[2], Osc133Marker::CommandExecuted);
        assert_eq!(
            markers[3],
            Osc133Marker::CommandFinished { exit_code: Some(0) }
        );
    }

    #[test]
    fn osc133_parse_ignores_malformed() {
        // Unknown marker type
        let markers = parse_osc133_markers("\x1b]133;X\x07");
        assert!(markers.is_empty());

        // Missing terminator (text ends before terminator)
        let markers = parse_osc133_markers("\x1b]133;A");
        assert!(markers.is_empty());

        // Wrong OSC number
        let markers = parse_osc133_markers("\x1b]7;A\x07");
        assert!(markers.is_empty());

        // Not an OSC sequence
        let markers = parse_osc133_markers("[133;A");
        assert!(markers.is_empty());
    }

    #[test]
    fn osc133_parse_no_panic_on_arbitrary_input() {
        // Fuzzy test: shouldn't panic on random input
        let inputs = [
            "",
            "hello world",
            "\x1b]",
            "\x1b]133",
            "\x1b]133;",
            "\x1b]133;A",
            "\x07\x07\x07",
            "\x1b\x1b\x1b",
            "normal\x1b]133;A\x07text\x1b]133;D;1\x07more",
            "\x00\x01\x02\x7f",
        ];
        for input in inputs {
            let _ = parse_osc133_markers(input);
        }
    }

    #[test]
    fn osc133_state_transitions() {
        let mut state = Osc133State::new();
        assert_eq!(state.state, ShellState::Unknown);
        assert!(state.last_exit_code.is_none());

        state.process_marker(Osc133Marker::PromptStart);
        assert_eq!(state.state, ShellState::PromptActive);
        assert!(state.state.is_at_prompt());
        assert!(!state.state.is_command_running());

        state.process_marker(Osc133Marker::CommandStart);
        assert_eq!(state.state, ShellState::InputActive);
        assert!(state.state.is_at_prompt());

        state.process_marker(Osc133Marker::CommandExecuted);
        assert_eq!(state.state, ShellState::CommandRunning);
        assert!(!state.state.is_at_prompt());
        assert!(state.state.is_command_running());

        state.process_marker(Osc133Marker::CommandFinished { exit_code: Some(0) });
        assert!(matches!(
            state.state,
            ShellState::CommandFinished { exit_code: Some(0) }
        ));
        assert!(state.state.is_at_prompt());
        assert!(!state.state.is_command_running());
        assert_eq!(state.last_exit_code, Some(0));
    }

    #[test]
    fn osc133_state_counts_markers() {
        let mut state = Osc133State::new();
        assert_eq!(state.markers_seen, 0);

        state.process_marker(Osc133Marker::PromptStart);
        assert_eq!(state.markers_seen, 1);

        state.process_marker(Osc133Marker::CommandStart);
        state.process_marker(Osc133Marker::CommandExecuted);
        assert_eq!(state.markers_seen, 3);
    }

    #[test]
    fn osc133_process_output_convenience() {
        let mut state = Osc133State::new();
        let text = "\x1b]133;A\x07prompt\x1b]133;B\x07ls\x1b]133;C\x07";

        process_osc133_output(&mut state, text);

        assert_eq!(state.state, ShellState::CommandRunning);
        assert_eq!(state.markers_seen, 3);
    }

    // =========================================================================
    // Alt-Screen Detection Tests
    // =========================================================================

    #[test]
    fn detect_alt_screen_enter_1049() {
        // DECSET 1049 - most common alternate screen sequence
        let changes = detect_alt_screen_changes("\x1b[?1049h");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0], AltScreenChange::Entered);
    }

    #[test]
    fn detect_alt_screen_exit_1049() {
        let changes = detect_alt_screen_changes("\x1b[?1049l");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0], AltScreenChange::Exited);
    }

    #[test]
    fn detect_alt_screen_enter_47() {
        // DECSET 47 - older alternate screen sequence
        let changes = detect_alt_screen_changes("\x1b[?47h");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0], AltScreenChange::Entered);
    }

    #[test]
    fn detect_alt_screen_exit_47() {
        let changes = detect_alt_screen_changes("\x1b[?47l");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0], AltScreenChange::Exited);
    }

    #[test]
    fn detect_alt_screen_embedded_in_text() {
        // vim startup: clears screen, enters alt screen, then displays content
        let text = "some output\x1b[?1049hvim content here";
        let changes = detect_alt_screen_changes(text);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0], AltScreenChange::Entered);
    }

    #[test]
    fn detect_alt_screen_multiple_transitions() {
        // Rapidly entering and exiting (e.g., quick peek with less then quit)
        let text = "before\x1b[?1049hcontent\x1b[?1049lafter";
        let changes = detect_alt_screen_changes(text);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0], AltScreenChange::Entered);
        assert_eq!(changes[1], AltScreenChange::Exited);
    }

    #[test]
    fn detect_alt_screen_multiple_transitions_with_st() {
        // Rapidly entering and exiting (e.g., quick peek with less then quit)
        let text = "before\x1b[?1049hcontent\x1b[?1049l\rafter";
        let changes = detect_alt_screen_changes(text);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0], AltScreenChange::Entered);
        assert_eq!(changes[1], AltScreenChange::Exited);
    }

    #[test]
    fn has_alt_screen_change_positive() {
        assert!(has_alt_screen_change("\x1b[?1049h"));
        assert!(has_alt_screen_change("\x1b[?1049l"));
        assert!(has_alt_screen_change("\x1b[?47h"));
        assert!(has_alt_screen_change("\x1b[?47l"));
        assert!(has_alt_screen_change("text\x1b[?1049hmore"));
    }

    #[test]
    fn has_alt_screen_change_negative() {
        assert!(!has_alt_screen_change(""));
        assert!(!has_alt_screen_change("hello world"));
        assert!(!has_alt_screen_change("\x1b[H")); // cursor home, not alt screen
        assert!(!has_alt_screen_change("\x1b[2J")); // clear screen, not alt screen
    }

    #[test]
    fn cursor_detects_alt_screen_enter_as_gap() {
        let mut cursor = PaneCursor::new(1);
        assert!(!cursor.in_alt_screen);

        // Initial content
        let seg0 = cursor
            .capture_snapshot("hello\n", 1024, None)
            .expect("first capture");
        assert_eq!(seg0.kind, CapturedSegmentKind::Delta);
        assert_eq!(seg0.content, "hello\n");

        // Simulate entering vim (alt screen)
        let seg1 = cursor
            .capture_snapshot("hello\n\x1b[?1049hvim window", 1024, None)
            .expect("alt screen capture");

        // Should be detected as a gap
        assert!(
            matches!(seg1.kind, CapturedSegmentKind::Gap { ref reason } if reason == "alt_screen_entered")
        );
        assert!(cursor.in_alt_screen);
        assert!(cursor.in_gap);
    }

    #[test]
    fn cursor_detects_alt_screen_exit_as_gap() {
        let mut cursor = PaneCursor::new(1);

        // Start in alt screen
        cursor.in_alt_screen = true;

        let _seg0 = cursor
            .capture_snapshot("vim content", 1024, None)
            .expect("first capture in alt screen");

        // Exit vim (alt screen exit)
        let seg1 = cursor
            .capture_snapshot("vim content\x1b[?1049l$ prompt", 1024, None)
            .expect("alt screen exit capture");

        assert!(
            matches!(seg1.kind, CapturedSegmentKind::Gap { ref reason } if reason == "alt_screen_exited")
        );
        assert!(!cursor.in_alt_screen);
    }

    #[test]
    fn cursor_tracks_alt_screen_state() {
        let mut cursor = PaneCursor::new(1);
        assert!(!cursor.in_alt_screen);

        // Enter alt screen
        cursor.capture_snapshot("\x1b[?1049hcontent", 1024, None);
        assert!(cursor.in_alt_screen);

        // Still in alt screen
        cursor.capture_snapshot("\x1b[?1049hcontent update", 1024, None);
        assert!(cursor.in_alt_screen);

        // Exit alt screen
        cursor.capture_snapshot("\x1b[?1049hcontent update\x1b[?1049l$ prompt", 1024, None);
        assert!(!cursor.in_alt_screen);
    }

    // ─── br-ft-6tevg: balanced toggle pair within single tick ──────────
    //
    // Defect (dormant in production, hardening for raw-capture paths):
    // a snapshot containing BOTH ESC[?1049h and ESC[?1049l in one tick
    // computes final state == self.in_alt_screen, and the prior
    // implementation skipped the gap check. Content was disrupted —
    // delta extraction across the boundary is unsound — but no Gap
    // was forced. The fix forces a Gap with a dedicated
    // `alt_screen_toggled` reason whenever any alt-screen change
    // marker appears, even if the net state is unchanged.

    #[test]
    fn cursor_balanced_toggle_pair_forces_gap_with_toggled_reason() {
        // br-ft-6tevg load-bearing test: a snapshot with BOTH
        // Entered and Exited markers in a single tick must force
        // a Gap with reason `alt_screen_toggled`.
        let mut cursor = PaneCursor::new(1);
        // Starting in main screen.
        assert!(!cursor.in_alt_screen);
        // Initial content (establishes a snapshot to compare against).
        cursor.capture_snapshot("hello\n", 1024, None);
        // Now feed a snapshot that ENTERED + EXITED alt-screen
        // within one tick (e.g., a quick `clear; vim --version;
        // exit` style fragment).
        let seg = cursor
            .capture_snapshot(
                "hello\n\x1b[?1049htransient alt content\x1b[?1049l$ prompt",
                1024,
                None,
            )
            .expect("balanced-toggle capture must produce a segment");
        // Net state unchanged (started false, ended false).
        assert!(
            !cursor.in_alt_screen,
            "balanced toggle must net to original state"
        );
        // But the segment MUST be a Gap with the new dedicated reason.
        assert!(
            matches!(&seg.kind, CapturedSegmentKind::Gap { reason } if reason == "alt_screen_toggled"),
            "br-ft-6tevg: balanced toggle pair must force Gap with `alt_screen_toggled` reason; got {:?}",
            seg.kind,
        );
        assert!(cursor.in_gap, "in_gap must be set on toggle gap");
    }

    #[test]
    fn cursor_balanced_toggle_pair_in_alt_screen_state_also_forces_gap() {
        // Symmetric case: starting IN alt-screen, a balanced
        // exit + re-enter pair within one tick must also force
        // a Gap.
        let mut cursor = PaneCursor::new(1);
        cursor.in_alt_screen = true;
        cursor.capture_snapshot("vim window", 1024, None);
        let seg = cursor
            .capture_snapshot(
                "vim window\x1b[?1049ltransient main\x1b[?1049hvim restored",
                1024,
                None,
            )
            .expect("balanced-toggle capture in alt screen must produce a segment");
        assert!(
            cursor.in_alt_screen,
            "balanced toggle nets to original alt state"
        );
        assert!(
            matches!(&seg.kind, CapturedSegmentKind::Gap { reason } if reason == "alt_screen_toggled"),
            "br-ft-6tevg symmetric case: balanced exit+enter must force `alt_screen_toggled` Gap; got {:?}",
            seg.kind,
        );
    }

    #[test]
    fn cursor_clean_enter_still_uses_entered_reason() {
        // br-ft-6tevg regression-protection: a CLEAN single
        // Entered transition (no balancing Exit) must still emit
        // `alt_screen_entered`, not `alt_screen_toggled`.
        let mut cursor = PaneCursor::new(1);
        cursor.capture_snapshot("hello\n", 1024, None);
        let seg = cursor
            .capture_snapshot("hello\n\x1b[?1049hvim", 1024, None)
            .expect("alt-screen enter capture");
        assert!(cursor.in_alt_screen);
        assert!(
            matches!(&seg.kind, CapturedSegmentKind::Gap { reason } if reason == "alt_screen_entered"),
            "clean Entered must keep alt_screen_entered reason (not alt_screen_toggled); got {:?}",
            seg.kind,
        );
    }

    #[test]
    fn cursor_clean_exit_still_uses_exited_reason() {
        // Symmetric regression-protection for the Exited path.
        let mut cursor = PaneCursor::new(1);
        cursor.in_alt_screen = true;
        cursor.capture_snapshot("vim", 1024, None);
        let seg = cursor
            .capture_snapshot("vim\x1b[?1049l$ prompt", 1024, None)
            .expect("alt-screen exit capture");
        assert!(!cursor.in_alt_screen);
        assert!(
            matches!(&seg.kind, CapturedSegmentKind::Gap { reason } if reason == "alt_screen_exited"),
            "clean Exited must keep alt_screen_exited reason (not alt_screen_toggled); got {:?}",
            seg.kind,
        );
    }

    // =========================================================================
    // OutputCache Tests
    // =========================================================================

    #[test]
    fn output_cache_repeated_content_returns_false() {
        let mut cache = OutputCache::with_defaults();

        // First time seeing content: is_new returns true
        assert!(cache.is_new(1, "hello world\n"));

        // Same content again: is_new returns false
        assert!(!cache.is_new(1, "hello world\n"));

        // Same content third time: still false
        assert!(!cache.is_new(1, "hello world\n"));
    }

    #[test]
    fn output_cache_different_content_returns_true() {
        let mut cache = OutputCache::with_defaults();

        assert!(cache.is_new(1, "content A\n"));
        assert!(cache.is_new(1, "content B\n"));
        assert!(cache.is_new(1, "content C\n"));

        // Each unique content should be new
        let stats = cache.stats();
        assert_eq!(stats.misses, 3);
        assert_eq!(stats.hits, 0);
    }

    #[test]
    fn output_cache_per_pane_deduplication() {
        let mut cache = OutputCache::with_defaults();

        // Pane 1 sees content first
        assert!(cache.is_new(1, "$ ls\nfile1\nfile2\n"));
        assert!(!cache.is_new(1, "$ ls\nfile1\nfile2\n"));

        // Pane 2 sees same content - should be false (global LRU dedup)
        assert!(!cache.is_new(2, "$ ls\nfile1\nfile2\n"));
    }

    #[test]
    fn output_cache_global_lru_deduplicates_across_panes() {
        let mut cache = OutputCache::with_defaults();

        let shared_content = "common output across panes\n";

        // Pane 1 sees content first
        assert!(cache.is_new(1, shared_content));

        // Panes 2, 3, 4 see same content - global LRU should detect
        assert!(!cache.is_new(2, shared_content));
        assert!(!cache.is_new(3, shared_content));
        assert!(!cache.is_new(4, shared_content));

        let stats = cache.stats();
        assert_eq!(stats.misses, 1); // Only first was a miss
        assert_eq!(stats.hits, 3); // Three hits from global LRU
    }

    #[test]
    fn output_cache_lru_generation_recycles_only_after_discarding_prior_epoch() {
        let mut cache = OutputCache::with_defaults();
        cache.update_global_lru(11, 100);
        cache.lru_generation = u64::MAX;

        cache.update_global_lru(22, 200);

        assert_eq!(cache.lru_generation, 1);
        assert!(!cache.global_hashes.contains_key(&11));
        assert_eq!(cache.global_hashes[&22].generation, 1);
        assert_eq!(
            cache.lru_order.iter().copied().collect::<Vec<_>>(),
            [(22, 1)]
        );
    }

    #[test]
    fn zero_capacity_disables_cross_pane_global_cache() {
        let config = OutputCacheConfig {
            global_lru_capacity: 0,
            per_pane_max_age_ms: 60_000,
        };
        let mut cache = OutputCache::new(config);

        assert!(cache.is_new(1, "same content"));
        assert!(cache.is_new(2, "same content"));
        assert!(cache.global_hashes.is_empty());
        assert!(cache.lru_order.is_empty());
        assert_eq!(cache.lru_generation, 0);
    }

    #[test]
    fn output_cache_telemetry_saturates_without_changing_dedup_behavior() {
        let mut cache = OutputCache::with_defaults();
        cache.hits = u64::MAX;
        cache.misses = u64::MAX;

        assert!(cache.is_new(1, "first"));
        assert!(!cache.is_new(1, "first"));

        assert_eq!(cache.hits, u64::MAX);
        assert_eq!(cache.misses, u64::MAX);
        assert_eq!(cache.hit_rate().to_bits(), 0.5f64.to_bits());
    }

    #[test]
    fn output_cache_lru_eviction() {
        // Create cache with small LRU capacity
        let config = OutputCacheConfig {
            global_lru_capacity: 3,
            per_pane_max_age_ms: 60_000,
        };
        let mut cache = OutputCache::new(config);

        // Fill LRU with 3 distinct hashes
        assert!(cache.is_new(1, "content A\n"));
        assert!(cache.is_new(1, "content B\n"));
        assert!(cache.is_new(1, "content C\n"));

        // Cache should have 3 global entries
        assert_eq!(cache.stats().global_entries, 3);

        // Add 4th - should evict oldest (content A)
        assert!(cache.is_new(1, "content D\n"));
        assert_eq!(cache.stats().global_entries, 3);

        // Content A should be treated as new again (evicted from global)
        assert!(cache.is_new(2, "content A\n"));
    }

    #[test]
    fn output_cache_lru_refreshes_on_access() {
        // ft-fesg7: eviction must be LRU, not FIFO. Re-accessing an early hash
        // must keep it alive when a new hash forces an eviction; the colder,
        // un-accessed hash should be evicted instead.
        let config = OutputCacheConfig {
            global_lru_capacity: 3,
            per_pane_max_age_ms: 60_000,
        };
        let mut cache = OutputCache::new(config);

        // Insert A, B, C (A is the oldest by insertion).
        assert!(cache.is_new(1, "content A\n"));
        assert!(cache.is_new(1, "content B\n"));
        assert!(cache.is_new(1, "content C\n"));
        assert_eq!(cache.stats().global_entries, 3);

        // Access A again via a fresh pane (bypasses the per-pane fast path so
        // the global-LRU recency is refreshed). A is now most-recently-used.
        assert!(!cache.is_new(2, "content A\n"));

        // Insert D at capacity: the least-recently-used is now B, not A.
        assert!(cache.is_new(3, "content D\n"));
        assert_eq!(cache.stats().global_entries, 3);

        // A survived because it was refreshed on access (would be a miss under
        // FIFO eviction).
        assert!(!cache.is_new(4, "content A\n"));
        // B was evicted as the genuine least-recently-used entry.
        assert!(cache.is_new(5, "content B\n"));
    }

    #[test]
    fn output_cache_lru_order_stays_bounded_under_repeated_hits() {
        // ft-zo4hw: refreshing recency on a cache HIT is O(1) amortized via
        // lazy token invalidation + periodic compaction. The order deque must
        // not grow without bound when the same hot content is seen repeatedly
        // (the old code paid an O(n) position-scan + remove per hit instead).
        let capacity = 8usize;
        let config = OutputCacheConfig {
            global_lru_capacity: capacity,
            per_pane_max_age_ms: 60_000,
        };
        let mut cache = OutputCache::new(config);

        // Prime one hot hash, then hammer it from a fresh pane each iteration so
        // the per-pane fast path is bypassed and the global-LRU refresh runs.
        assert!(cache.is_new(0, "hot\n"));
        for pane in 1..10_000u64 {
            assert!(!cache.is_new(pane, "hot\n"));
        }

        // Exactly one live global entry, and the order deque stayed bounded to
        // <= 2x capacity instead of accumulating a permanent token per hit.
        assert_eq!(cache.stats().global_entries, 1);
        assert!(
            cache.lru_order.len() <= capacity * 2,
            "lru_order must stay bounded by lazy compaction, got {}",
            cache.lru_order.len()
        );
    }

    // ft-wo323: perf-regression guards for the ft-zo4hw amortized-O(1) refresh.

    #[test]
    fn output_cache_lru_refresh_does_no_scan_below_compaction_threshold() {
        // A correct O(1) refresh examines ZERO lru_order tokens — no position
        // scan, no remove. Under the old code each hit scanned the deque.
        reset_output_cache_lru_maintenance_steps();
        let capacity = 64usize;
        let config = OutputCacheConfig {
            global_lru_capacity: capacity,
            per_pane_max_age_ms: 60_000,
        };
        let mut cache = OutputCache::new(config);

        // One miss to seed, then refreshes that stay below the 2x-capacity
        // compaction threshold and never evict (live count stays 1).
        assert!(cache.is_new(0, "hot\n"));
        for pane in 1..capacity as u64 {
            assert!(!cache.is_new(pane, "hot\n"));
        }
        assert_eq!(
            output_cache_lru_maintenance_steps(),
            0,
            "refresh below the compaction threshold must scan zero tokens"
        );
    }

    #[test]
    fn output_cache_lru_refresh_is_amortized_o1_under_repeated_hits() {
        // Total maintenance work over N refreshes of one hot hash must be
        // linear in N (compaction amortizes to ~O(1) per access). The old O(n)
        // refresh would cost ~N * deque_len.
        reset_output_cache_lru_maintenance_steps();
        let capacity = 16usize;
        let config = OutputCacheConfig {
            global_lru_capacity: capacity,
            per_pane_max_age_ms: 60_000,
        };
        let mut cache = OutputCache::new(config);

        let n = 10_000u64;
        assert!(cache.is_new(0, "hot\n"));
        for pane in 1..n {
            assert!(!cache.is_new(pane, "hot\n"));
        }
        let steps = output_cache_lru_maintenance_steps();
        assert!(
            steps <= 4 * n,
            "maintenance steps {steps} must be linear in {n} (amortized O(1))"
        );
        assert_eq!(cache.stats().global_entries, 1);
    }

    #[test]
    fn output_cache_lru_eviction_churn_is_amortized_o1() {
        // Inserting N distinct new hashes into a full cache forces N evictions;
        // total work must stay linear (each eviction pops O(1) live tokens).
        let capacity = 16usize;
        let config = OutputCacheConfig {
            global_lru_capacity: capacity,
            per_pane_max_age_ms: 60_000,
        };
        let mut cache = OutputCache::new(config);
        for i in 0..capacity as u64 {
            assert!(cache.is_new(i, &format!("fill-{i}\n")));
        }

        reset_output_cache_lru_maintenance_steps();
        let n = 10_000u64;
        for i in 0..n {
            assert!(cache.is_new(1_000_000 + i, &format!("churn-{i}\n")));
        }
        let steps = output_cache_lru_maintenance_steps();
        assert!(
            steps <= 6 * n,
            "eviction-churn steps {steps} must be linear in {n} (amortized O(1))"
        );
        assert_eq!(cache.stats().global_entries, capacity);
    }

    #[test]
    fn output_cache_lru_matches_reference_model_under_random_access() {
        // Property test: against a deterministic pseudo-random access stream
        // (fresh pane per access to bypass the per-pane fast path), the cache's
        // hit/miss decisions and live-entry count must match a straightforward
        // reference LRU model, and the order deque must stay bounded.
        let capacity = 8usize;
        let config = OutputCacheConfig {
            global_lru_capacity: capacity,
            per_pane_max_age_ms: 60_000,
        };
        let mut cache = OutputCache::new(config);

        let key_space = 20u64; // > capacity so evictions happen
        let mut order: Vec<u64> = Vec::new(); // front = least-recently-used
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;

        for step in 0..5_000u64 {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let idx = seed % key_space;
            let content = format!("ref-key-{idx}\n");
            let pane = 1_000_000 + step;

            // Reference LRU: hit refreshes recency; miss evicts the front.
            let ref_hit = if let Some(pos) = order.iter().position(|&x| x == idx) {
                order.remove(pos);
                order.push(idx);
                true
            } else {
                if order.len() >= capacity {
                    order.remove(0);
                }
                order.push(idx);
                false
            };

            let is_new = cache.is_new(pane, &content);
            assert_eq!(
                is_new, !ref_hit,
                "step {step} idx {idx}: hit/miss disagrees with reference LRU"
            );
            assert_eq!(
                cache.stats().global_entries,
                order.len(),
                "step {step}: live-entry count diverged from reference LRU"
            );
            assert!(
                cache.lru_order.len() <= capacity * 2,
                "step {step}: lru_order exceeded 2x capacity ({})",
                cache.lru_order.len()
            );
        }
    }

    #[test]
    fn output_cache_prune_stale_panes() {
        let config = OutputCacheConfig {
            global_lru_capacity: 1024,
            per_pane_max_age_ms: 100, // 100ms max age
        };
        let mut cache = OutputCache::new(config);

        // Add entries for multiple panes
        assert!(cache.is_new(1, "pane 1 content\n"));
        assert!(cache.is_new(2, "pane 2 content\n"));
        assert!(cache.is_new(3, "pane 3 content\n"));

        assert_eq!(cache.stats().pane_entries, 3);

        // Sleep briefly to make entries stale
        std::thread::sleep(std::time::Duration::from_millis(150));

        // Prune should remove stale entries
        cache.prune_stale();

        assert_eq!(cache.stats().pane_entries, 0);
    }

    #[test]
    fn output_cache_prune_with_custom_max_age() {
        let mut cache = OutputCache::with_defaults();

        assert!(cache.is_new(1, "content\n"));
        assert_eq!(cache.stats().pane_entries, 1);

        // Prune with 0 max_age should remove everything
        cache.prune(0);
        assert_eq!(cache.stats().pane_entries, 0);
    }

    #[test]
    fn output_cache_remove_pane() {
        let mut cache = OutputCache::with_defaults();

        assert!(cache.is_new(1, "content\n"));
        assert!(cache.is_new(2, "other content\n"));
        assert_eq!(cache.stats().pane_entries, 2);

        cache.remove_pane(1);
        assert_eq!(cache.stats().pane_entries, 1);

        // Pane 1 content should be new again (per-pane state removed)
        // But global LRU still has it, so it's a hit
        assert!(!cache.is_new(1, "content\n"));
    }

    #[test]
    fn output_cache_clear() {
        let mut cache = OutputCache::with_defaults();

        assert!(cache.is_new(1, "content A\n"));
        assert!(cache.is_new(2, "content B\n"));
        assert!(cache.is_new(3, "content C\n"));

        let stats = cache.stats();
        assert!(stats.global_entries > 0);
        assert!(stats.pane_entries > 0);

        cache.clear();

        let stats = cache.stats();
        assert_eq!(stats.global_entries, 0);
        assert_eq!(stats.pane_entries, 0);
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 0);
    }

    #[test]
    fn output_cache_hit_rate_calculation() {
        let mut cache = OutputCache::with_defaults();

        // No hits/misses yet - hit rate is 0
        assert!(cache.hit_rate().abs() < f64::EPSILON);

        // 1 miss
        assert!(cache.is_new(1, "content\n"));
        assert!(cache.hit_rate().abs() < f64::EPSILON);

        // 1 hit, 1 miss = 50%
        assert!(!cache.is_new(1, "content\n"));
        assert!((cache.hit_rate() - 0.5).abs() < 0.01);

        // 2 hits, 1 miss = 66.67%
        assert!(!cache.is_new(1, "content\n"));
        assert!((cache.hit_rate() - 0.666).abs() < 0.01);
    }

    #[test]
    fn output_cache_stats_reset() {
        let mut cache = OutputCache::with_defaults();

        assert!(cache.is_new(1, "content\n"));
        assert!(!cache.is_new(1, "content\n"));

        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);

        cache.reset_stats();

        let stats = cache.stats();
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 0);
        // Global/pane entries should still exist
        assert!(stats.global_entries > 0);
        assert!(stats.pane_entries > 0);
    }

    #[test]
    fn output_cache_empty_content() {
        let mut cache = OutputCache::with_defaults();

        // Empty content should work
        assert!(cache.is_new(1, ""));
        assert!(!cache.is_new(1, ""));

        // Different pane with empty content - global dedup
        assert!(!cache.is_new(2, ""));
    }

    #[test]
    fn output_cache_hash_collision_resistance() {
        let mut cache = OutputCache::with_defaults();

        // Test with content that might have hash collisions in weak hashers
        // Good hashers (xxhash, cityhash, etc.) should handle these fine
        let contents = [
            "a".repeat(1000),
            "b".repeat(1000),
            "ab".repeat(500),
            "ba".repeat(500),
        ];

        for (i, content) in contents.iter().enumerate() {
            assert!(cache.is_new(1, content), "content {i} should be new");
        }

        // All should be cached now
        for (i, content) in contents.iter().enumerate() {
            assert!(!cache.is_new(1, content), "content {i} should be cached");
        }
    }

    // =========================================================================
    // pane_uuid stability tests (wa-upg.4.5)
    // =========================================================================

    /// Helper: build a minimal PaneInfo for testing.
    fn make_pane_info(pane_id: u64, window_id: u64, tab_id: u64) -> PaneInfo {
        PaneInfo {
            pane_id,
            tab_id,
            window_id,
            domain_id: None,
            domain_name: Some("local".to_string()),
            workspace: None,
            size: None,
            rows: None,
            cols: None,
            title: Some("bash".to_string()),
            cwd: Some("/home/user".to_string()),
            tty_name: Some(format!("/dev/pts/{pane_id}")),
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: false,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn pane_uuid_format_is_32_hex_chars() {
        let uuid = generate_pane_uuid("local", 1, 1_700_000_000_000);
        assert_eq!(uuid.len(), 32, "uuid should be 32 chars: {uuid}");
        assert!(
            uuid.chars().all(|c| c.is_ascii_hexdigit()),
            "uuid should be hex: {uuid}"
        );
        // Must be lowercase hex
        assert_eq!(uuid, uuid.to_ascii_lowercase());
    }

    #[test]
    fn pane_uuid_includes_random_entropy() {
        // Two calls with identical inputs should produce different UUIDs
        // because generate_pane_uuid adds random entropy.
        let a = generate_pane_uuid("local", 1, 1_000);
        let b = generate_pane_uuid("local", 1, 1_000);
        assert_ne!(a, b, "UUIDs should differ due to random entropy");
    }

    #[test]
    fn registry_assigns_uuid_on_discovery() {
        let mut reg = PaneRegistry::new();
        let pane = make_pane_info(1, 100, 10);
        let diff = reg.discovery_tick(vec![pane]);

        assert_eq!(diff.new_panes, vec![1]);
        let entry = reg.get_entry(1).expect("pane should be registered");
        assert_eq!(entry.pane_uuid.len(), 32);
        assert_eq!(entry.lifecycle_revision.get(), 0);
    }

    #[test]
    fn registry_uuid_stable_across_title_change() {
        let mut reg = PaneRegistry::new();
        let pane = make_pane_info(1, 100, 10);
        reg.discovery_tick(vec![pane]);

        let uuid_before = reg.get_entry(1).unwrap().pane_uuid.clone();

        // Change the title; lifecycle identity and UUID stay stable.
        let mut changed = make_pane_info(1, 100, 10);
        changed.title = Some("vim".to_string());
        let diff = reg.discovery_tick(vec![changed]);

        assert_eq!(diff.lifecycle_replacements, [] as [u64; 0]);
        assert_eq!(diff.metadata_changes.len(), 1);
        assert!(diff.new_panes.is_empty(), "should not be new pane");
        let uuid_after = reg.get_entry(1).unwrap().pane_uuid.clone();
        assert_eq!(
            uuid_before, uuid_after,
            "UUID must be stable across title change"
        );
    }

    #[test]
    fn registry_uuid_stable_across_cwd_change() {
        let mut reg = PaneRegistry::new();
        let pane = make_pane_info(1, 100, 10);
        reg.discovery_tick(vec![pane]);

        let uuid_before = reg.get_entry(1).unwrap().pane_uuid.clone();

        // Change the cwd; lifecycle identity and UUID stay stable.
        let mut changed = make_pane_info(1, 100, 10);
        changed.cwd = Some("/tmp".to_string());
        let diff = reg.discovery_tick(vec![changed]);

        assert_eq!(diff.lifecycle_replacements, [] as [u64; 0]);
        assert_eq!(diff.metadata_changes.len(), 1);
        let uuid_after = reg.get_entry(1).unwrap().pane_uuid.clone();
        assert_eq!(
            uuid_before, uuid_after,
            "UUID must be stable across cwd change"
        );
    }

    #[test]
    fn registry_uuid_stable_across_tab_move() {
        let mut reg = PaneRegistry::new();
        let pane = make_pane_info(1, 100, 10);
        reg.discovery_tick(vec![pane]);

        let uuid_before = reg.get_entry(1).unwrap().pane_uuid.clone();

        // Move pane to different tab and window (metadata only).
        let mut moved = make_pane_info(1, 200, 20);
        moved.title = Some("bash".to_string());
        moved.cwd = Some("/home/user".to_string());
        let diff = reg.discovery_tick(vec![moved]);

        assert_eq!(diff.metadata_changes.len(), 1);
        assert_eq!(diff.metadata_changes[0].pane_id, 1);
        assert_eq!(diff.lifecycle_replacements, [] as [u64; 0]);
        let uuid_after = reg.get_entry(1).unwrap().pane_uuid.clone();
        assert_eq!(
            uuid_before, uuid_after,
            "UUID must be stable across tab/window move"
        );
    }

    #[test]
    fn registry_uuid_removed_on_close() {
        let mut reg = PaneRegistry::new();
        let pane = make_pane_info(1, 100, 10);
        reg.discovery_tick(vec![pane]);

        let uuid = reg.get_entry(1).unwrap().pane_uuid.clone();
        assert!(reg.get_pane_id_by_uuid(&uuid).is_some());

        // Pane disappears (not in next tick)
        let diff = reg.discovery_tick(vec![]);
        assert_eq!(diff.closed_panes, vec![1]);

        // UUID should be removed from reverse index
        assert!(reg.get_entry(1).is_none());
        assert!(reg.get_pane_id_by_uuid(&uuid).is_none());
    }

    #[test]
    fn registry_new_uuid_on_reappearance() {
        let mut reg = PaneRegistry::new();
        let pane = make_pane_info(1, 100, 10);
        reg.discovery_tick(vec![pane]);

        let uuid_first = reg.get_entry(1).unwrap().pane_uuid.clone();

        // Pane disappears
        reg.discovery_tick(vec![]);
        assert!(reg.get_entry(1).is_none());

        // Same pane_id reappears (new shell session)
        let reappear = make_pane_info(1, 100, 10);
        let diff = reg.discovery_tick(vec![reappear]);
        assert_eq!(diff.new_panes, vec![1]);

        let uuid_second = reg.get_entry(1).unwrap().pane_uuid.clone();
        assert_ne!(
            uuid_first, uuid_second,
            "reappeared pane should get a new UUID"
        );
    }

    #[test]
    fn registry_uuid_reverse_index_consistent() {
        let mut reg = PaneRegistry::new();
        let panes = vec![
            make_pane_info(1, 100, 10),
            make_pane_info(2, 100, 10),
            make_pane_info(3, 200, 20),
        ];
        reg.discovery_tick(panes);

        // All 3 panes should have distinct UUIDs accessible via reverse index
        for pane_id in [1, 2, 3] {
            let entry = reg.get_entry(pane_id).unwrap();
            let looked_up = reg.get_pane_id_by_uuid(&entry.pane_uuid);
            assert_eq!(
                looked_up,
                Some(pane_id),
                "reverse index should map UUID back to pane_id"
            );
        }

        // UUIDs should be distinct
        let uuids: Vec<_> = [1, 2, 3]
            .iter()
            .map(|id| reg.get_entry(*id).unwrap().pane_uuid.clone())
            .collect();
        let unique: std::collections::HashSet<_> = uuids.iter().collect();
        assert_eq!(unique.len(), 3, "all UUIDs should be distinct");
    }

    #[test]
    fn registry_lifecycle_revision_changes_only_with_exact_identity() {
        let mut reg = PaneRegistry::new();
        let pane = make_pane_info(1, 100, 10);
        reg.discovery_tick(vec![pane]);
        assert_eq!(reg.get_entry(1).unwrap().lifecycle_revision.get(), 0);

        // Mutable title and cwd changes advance metadata only.
        let mut v2 = make_pane_info(1, 100, 10);
        v2.title = Some("vim".to_string());
        reg.discovery_tick(vec![v2]);
        assert_eq!(reg.get_entry(1).unwrap().lifecycle_revision.get(), 0);
        assert_eq!(reg.get_entry(1).unwrap().metadata_revision.get(), 1);

        let mut v3 = make_pane_info(1, 100, 10);
        v3.title = Some("vim".to_string());
        v3.cwd = Some("/tmp".to_string());
        reg.discovery_tick(vec![v3]);
        assert_eq!(reg.get_entry(1).unwrap().lifecycle_revision.get(), 0);
        assert_eq!(reg.get_entry(1).unwrap().metadata_revision.get(), 2);

        // A changed exact TTY witness is an authoritative replacement.
        let mut replacement = make_pane_info(1, 100, 10);
        replacement.tty_name = Some("/dev/pts/replacement".to_string());
        let diff = reg.discovery_tick(vec![replacement]);
        assert_eq!(diff.lifecycle_replacements, vec![1]);
        assert_eq!(reg.get_entry(1).unwrap().lifecycle_revision.get(), 1);
        assert_eq!(reg.get_entry(1).unwrap().metadata_revision.get(), 0);
    }

    #[test]
    fn registry_lookup_by_uuid_returns_correct_info() {
        let mut reg = PaneRegistry::new();
        let pane = make_pane_info(42, 100, 10);
        reg.discovery_tick(vec![pane]);

        let uuid = reg.get_entry(42).unwrap().pane_uuid.clone();
        let info = reg
            .get_pane_by_uuid(&uuid)
            .expect("should find pane by UUID");
        assert_eq!(info.pane_id, 42);
        assert_eq!(info.title.as_deref(), Some("bash"));
    }

    #[test]
    fn lifecycle_identity_same_when_unchanged() {
        let pane = make_pane_info(1, 100, 10);
        let left = PaneLifecycleIdentity::from_pane_info(&pane);
        let right = PaneLifecycleIdentity::from_pane_info(&pane);
        assert_eq!(left.continuity_with(&right), PaneLifecycleContinuity::Same);
    }

    #[test]
    fn lifecycle_identity_ignores_title_change() {
        let pane = make_pane_info(1, 100, 10);
        let identity = PaneLifecycleIdentity::from_pane_info(&pane);

        let mut changed = make_pane_info(1, 100, 10);
        changed.title = Some("ssh session".to_string());
        let changed_identity = PaneLifecycleIdentity::from_pane_info(&changed);
        assert_eq!(
            identity.continuity_with(&changed_identity),
            PaneLifecycleContinuity::Same
        );
    }

    #[test]
    fn lifecycle_identity_ignores_cwd_change() {
        let pane = make_pane_info(1, 100, 10);
        let identity = PaneLifecycleIdentity::from_pane_info(&pane);

        let mut changed = make_pane_info(1, 100, 10);
        changed.cwd = Some("/var/log".to_string());
        let changed_identity = PaneLifecycleIdentity::from_pane_info(&changed);
        assert_eq!(
            identity.continuity_with(&changed_identity),
            PaneLifecycleContinuity::Same
        );
    }

    #[test]
    fn lifecycle_identity_ignores_domain_display_name_change() {
        let pane = make_pane_info(1, 100, 10);
        let identity = PaneLifecycleIdentity::from_pane_info(&pane);

        let mut changed = make_pane_info(1, 100, 10);
        changed.domain_name = Some("SSH:remote.example.com".to_string());
        let changed_identity = PaneLifecycleIdentity::from_pane_info(&changed);
        assert_eq!(
            identity.continuity_with(&changed_identity),
            PaneLifecycleContinuity::Same
        );
    }

    #[test]
    fn lifecycle_identity_ignores_tab_window_change() {
        let pane = make_pane_info(1, 100, 10);
        let identity = PaneLifecycleIdentity::from_pane_info(&pane);

        let moved = make_pane_info(1, 200, 20);
        let moved_identity = PaneLifecycleIdentity::from_pane_info(&moved);

        assert_eq!(
            identity.continuity_with(&moved_identity),
            PaneLifecycleContinuity::Same
        );
    }

    #[test]
    fn registry_multi_pane_churn_stability() {
        // Simulate a realistic session: 3 panes, various changes
        let mut reg = PaneRegistry::new();

        // T0: 3 panes discovered
        let panes = vec![
            make_pane_info(1, 100, 10),
            make_pane_info(2, 100, 10),
            make_pane_info(3, 100, 11),
        ];
        reg.discovery_tick(panes);

        let uuid1 = reg.get_entry(1).unwrap().pane_uuid.clone();
        let uuid2 = reg.get_entry(2).unwrap().pane_uuid.clone();
        let uuid3 = reg.get_entry(3).unwrap().pane_uuid.clone();

        // T1: Pane 1 changes title, Pane 2 moves tab, Pane 3 unchanged
        let mut p1 = make_pane_info(1, 100, 10);
        p1.title = Some("vim".to_string());
        let p2 = make_pane_info(2, 100, 12); // tab changed
        let p3 = make_pane_info(3, 100, 11);
        reg.discovery_tick(vec![p1, p2, p3]);

        assert_eq!(
            reg.get_entry(1).unwrap().pane_uuid,
            uuid1,
            "UUID1 stable after title change"
        );
        assert_eq!(
            reg.get_entry(2).unwrap().pane_uuid,
            uuid2,
            "UUID2 stable after tab move"
        );
        assert_eq!(
            reg.get_entry(3).unwrap().pane_uuid,
            uuid3,
            "UUID3 stable when unchanged"
        );

        // T2: Pane 2 closes, pane 4 appears
        let mut p1_v2 = make_pane_info(1, 100, 10);
        p1_v2.title = Some("vim".to_string());
        let p3_v2 = make_pane_info(3, 100, 11);
        let p4 = make_pane_info(4, 100, 13);
        let diff = reg.discovery_tick(vec![p1_v2, p3_v2, p4]);

        assert!(diff.closed_panes.contains(&2), "pane 2 should close");
        assert!(diff.new_panes.contains(&4), "pane 4 should be new");
        assert_eq!(
            reg.get_entry(1).unwrap().pane_uuid,
            uuid1,
            "UUID1 still stable"
        );
        assert_eq!(
            reg.get_entry(3).unwrap().pane_uuid,
            uuid3,
            "UUID3 still stable"
        );
        assert!(
            reg.get_pane_id_by_uuid(&uuid2).is_none(),
            "UUID2 removed after close"
        );
        assert!(reg.get_entry(4).is_some(), "pane 4 should exist");
        let uuid4 = reg.get_entry(4).unwrap().pane_uuid.clone();
        assert_ne!(uuid4, uuid1, "new pane gets distinct UUID");
        assert_ne!(uuid4, uuid3, "new pane gets distinct UUID");
    }

    #[test]
    fn emit_overflow_gap_creates_gap_segment() {
        let mut cursor = PaneCursor::new(7);
        // Advance to seq 3
        cursor.next_seq = 3;

        let seg = cursor.emit_overflow_gap("backpressure_overflow");
        assert_eq!(seg.pane_id, 7);
        assert_eq!(seg.seq, 3);
        assert_eq!(seg.content, "");
        assert!(matches!(
            seg.kind,
            CapturedSegmentKind::Gap { ref reason } if reason == "backpressure_overflow"
        ));
        assert!(seg.captured_at > 0);
    }

    #[test]
    fn emit_overflow_gap_advances_seq() {
        let mut cursor = PaneCursor::new(1);
        assert_eq!(cursor.next_seq, 0);

        let seg = cursor.emit_overflow_gap("test_overflow");
        assert_eq!(seg.seq, 0);
        assert_eq!(cursor.next_seq, 1);

        let seg2 = cursor.emit_overflow_gap("test_overflow_2");
        assert_eq!(seg2.seq, 1);
        assert_eq!(cursor.next_seq, 2);
    }

    #[test]
    fn emit_overflow_gap_sets_in_gap_flag() {
        let mut cursor = PaneCursor::new(1);
        assert!(!cursor.in_gap);

        cursor.emit_overflow_gap("backpressure_overflow");
        assert!(cursor.in_gap);
    }

    #[test]
    fn emit_overflow_gap_then_normal_capture_works() {
        let mut cursor = PaneCursor::new(1);

        // First: emit overflow gap
        let gap = cursor.emit_overflow_gap("backpressure_overflow");
        assert_eq!(gap.seq, 0);
        assert!(cursor.in_gap);

        // Second: normal capture after gap
        let seg = cursor
            .capture_snapshot("hello world\n", 1024, None)
            .expect("should produce a segment after gap");
        assert_eq!(seg.seq, 1);
        // After an overflow gap, the cursor is in_gap state.
        // The next capture with content change may produce a Delta or Gap
        // depending on overlap extraction.  Either is valid.
        assert_eq!(seg.pane_id, 1);
    }

    // =========================================================================
    // Streaming Design Tests (wa-nu4.4.2.1)
    // =========================================================================

    // --- StreamEvent construction ---

    #[test]
    fn stream_event_output_data_fields() {
        let event = StreamEvent::OutputData {
            pane_id: 42,
            data: "hello\n".to_string(),
            received_at: 1_700_000_000_000,
            overflow: false,
        };
        if let StreamEvent::OutputData {
            pane_id,
            data,
            received_at,
            overflow,
        } = event
        {
            assert_eq!(pane_id, 42);
            assert_eq!(data, "hello\n");
            assert_eq!(received_at, 1_700_000_000_000);
            assert!(!overflow);
        } else {
            panic!("expected OutputData");
        }
    }

    #[test]
    fn stream_event_pane_closed() {
        let event = StreamEvent::PaneClosed { pane_id: 7 };
        assert!(matches!(event, StreamEvent::PaneClosed { pane_id: 7 }));
    }

    #[test]
    fn stream_event_disconnected() {
        let event = StreamEvent::Disconnected {
            reason: "mux gone".to_string(),
        };
        if let StreamEvent::Disconnected { reason } = event {
            assert_eq!(reason, "mux gone");
        } else {
            panic!("expected Disconnected");
        }
    }

    // --- OverflowPolicy defaults ---

    #[test]
    fn overflow_policy_default_is_emit_gap() {
        assert_eq!(OverflowPolicy::default(), OverflowPolicy::EmitGap);
    }

    #[test]
    fn stream_channel_config_default() {
        let cfg = StreamChannelConfig::default();
        assert_eq!(cfg.capacity, 4096);
        assert_eq!(cfg.overflow_policy, OverflowPolicy::EmitGap);
    }

    // --- StreamIngester: basic delta ---

    #[test]
    fn ingester_single_delta_produces_one_segment() {
        let mut ingester = StreamIngester::new();
        let event = StreamEvent::OutputData {
            pane_id: 1,
            data: "line1\n".to_string(),
            received_at: 100,
            overflow: false,
        };

        let segs = ingester.process(event);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].pane_id, 1);
        assert_eq!(segs[0].seq, 0);
        assert_eq!(segs[0].content, "line1\n");
        assert_eq!(segs[0].kind, CapturedSegmentKind::Delta);
        assert_eq!(segs[0].captured_at, 100);
    }

    #[test]
    fn ingester_empty_nonoverflow_output_emits_nothing_and_does_not_create_cursor() {
        let mut ingester = StreamIngester::new();
        let segments = ingester.process(StreamEvent::OutputData {
            pane_id: 41,
            data: String::new(),
            received_at: 100,
            overflow: false,
        });

        assert!(segments.is_empty());
        assert_eq!(ingester.active_panes(), 0);
        assert_eq!(ingester.total_segments(), 0);
        assert_eq!(ingester.total_gaps(), 0);
    }

    #[test]
    fn ingester_diagnostic_counters_saturate() {
        let mut ingester = StreamIngester::new();
        ingester.segments_emitted = u64::MAX;
        ingester.gaps_emitted = u64::MAX;

        let segments = ingester.process(StreamEvent::OutputData {
            pane_id: 43,
            data: "payload".to_string(),
            received_at: 100,
            overflow: true,
        });

        assert_eq!(segments.len(), 2);
        assert_eq!(ingester.total_segments(), u64::MAX);
        assert_eq!(ingester.total_gaps(), u64::MAX);
    }

    // --- Property: seq monotonicity ---

    #[test]
    fn ingester_seq_monotonicity_single_pane() {
        let mut ingester = StreamIngester::new();

        let mut last_seq: Option<u64> = None;
        for i in 0..100 {
            let event = StreamEvent::OutputData {
                pane_id: 1,
                data: format!("line {i}\n"),
                received_at: i as i64,
                overflow: false,
            };
            let segs = ingester.process(event);
            for seg in &segs {
                if let Some(prev) = last_seq {
                    assert!(
                        seg.seq > prev,
                        "seq must be strictly increasing: prev={prev}, got={}",
                        seg.seq
                    );
                }
                last_seq = Some(seg.seq);
            }
        }
        assert_eq!(last_seq, Some(99));
    }

    #[test]
    fn ingester_seq_monotonicity_multi_pane() {
        let mut ingester = StreamIngester::new();
        let mut last_seq_per_pane: HashMap<u64, u64> = HashMap::new();

        // Interleave events from 3 panes
        for i in 0..60 {
            let pane_id = (i % 3) + 1;
            let event = StreamEvent::OutputData {
                pane_id,
                data: format!("data {i}\n"),
                received_at: i as i64,
                overflow: false,
            };
            let segs = ingester.process(event);
            for seg in &segs {
                if let Some(&prev) = last_seq_per_pane.get(&seg.pane_id) {
                    assert!(
                        seg.seq > prev,
                        "pane {} seq must increase: prev={prev}, got={}",
                        seg.pane_id,
                        seg.seq
                    );
                }
                last_seq_per_pane.insert(seg.pane_id, seg.seq);
            }
        }

        // Each pane should have received 20 events (60/3)
        for pane_id in 1..=3 {
            assert_eq!(last_seq_per_pane[&pane_id], 19);
        }
    }

    // --- Property: overflow always produces GAP ---

    #[test]
    fn ingester_overflow_emits_gap_before_delta() {
        let mut ingester = StreamIngester::new();

        // First: normal event to establish cursor
        let normal = StreamEvent::OutputData {
            pane_id: 1,
            data: "first\n".to_string(),
            received_at: 100,
            overflow: false,
        };
        let segs = ingester.process(normal);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].seq, 0);
        assert_eq!(segs[0].kind, CapturedSegmentKind::Delta);

        // Second: event with overflow=true
        let overflow = StreamEvent::OutputData {
            pane_id: 1,
            data: "after_drop\n".to_string(),
            received_at: 200,
            overflow: true,
        };
        let segs = ingester.process(overflow);
        assert_eq!(segs.len(), 2, "overflow must produce GAP + Delta");

        // First segment is GAP
        assert!(
            matches!(segs[0].kind, CapturedSegmentKind::Gap { ref reason } if reason == "stream_overflow")
        );
        assert_eq!(segs[0].seq, 1);
        assert_eq!(segs[0].pane_id, 1);

        // Second segment is Delta
        assert_eq!(segs[1].seq, 2);
        assert_eq!(segs[1].pane_id, 1);
        assert_eq!(segs[1].kind, CapturedSegmentKind::Delta);
        assert_eq!(segs[1].content, "after_drop\n");
    }

    #[test]
    fn ingester_overflow_no_double_gap() {
        let mut ingester = StreamIngester::new();

        // Normal event
        ingester.process(StreamEvent::OutputData {
            pane_id: 1,
            data: "a".to_string(),
            received_at: 100,
            overflow: false,
        });

        // Overflow event — emits GAP + Delta
        let segs = ingester.process(StreamEvent::OutputData {
            pane_id: 1,
            data: "b".to_string(),
            received_at: 200,
            overflow: true,
        });
        assert_eq!(segs.len(), 2);

        // Next normal event should NOT produce another GAP
        let segs = ingester.process(StreamEvent::OutputData {
            pane_id: 1,
            data: "c".to_string(),
            received_at: 300,
            overflow: false,
        });
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].kind, CapturedSegmentKind::Delta);
    }

    #[test]
    fn ingester_empty_overflow_event_emits_gap_without_empty_delta() {
        let mut ingester = StreamIngester::new();

        let segs = ingester.process(StreamEvent::OutputData {
            pane_id: 1,
            data: "before\n".to_string(),
            received_at: 100,
            overflow: false,
        });
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].seq, 0);

        let segs = ingester.process(StreamEvent::OutputData {
            pane_id: 1,
            data: String::new(),
            received_at: 200,
            overflow: true,
        });
        assert_eq!(segs.len(), 1, "explicit upstream gaps should emit only GAP");
        assert!(
            matches!(segs[0].kind, CapturedSegmentKind::Gap { ref reason } if reason == "stream_overflow")
        );
        assert_eq!(segs[0].seq, 1);

        let segs = ingester.process(StreamEvent::OutputData {
            pane_id: 1,
            data: "after-gap\n".to_string(),
            received_at: 300,
            overflow: false,
        });
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].kind, CapturedSegmentKind::Delta);
        assert_eq!(segs[0].seq, 2);
        assert_eq!(segs[0].content, "after-gap\n");
    }

    // --- PaneClosed ---

    #[test]
    fn ingester_pane_closed_emits_gap() {
        let mut ingester = StreamIngester::new();

        // Establish cursor
        ingester.process(StreamEvent::OutputData {
            pane_id: 5,
            data: "hello\n".to_string(),
            received_at: 100,
            overflow: false,
        });
        assert_eq!(ingester.active_panes(), 1);

        // Close pane
        let segs = ingester.process(StreamEvent::PaneClosed { pane_id: 5 });
        assert_eq!(segs.len(), 1);
        assert!(
            matches!(&segs[0].kind, CapturedSegmentKind::Gap { reason } if reason == "pane_closed")
        );
        assert_eq!(segs[0].pane_id, 5);
        assert_eq!(ingester.active_panes(), 0);
    }

    #[test]
    fn ingester_pane_closed_unknown_pane_is_noop() {
        let mut ingester = StreamIngester::new();
        let segs = ingester.process(StreamEvent::PaneClosed { pane_id: 999 });
        assert!(segs.is_empty());
    }

    // --- Disconnected ---

    #[test]
    fn ingester_disconnected_emits_gap_per_pane() {
        let mut ingester = StreamIngester::new();

        // Establish 3 panes
        for pid in [1, 2, 3] {
            ingester.process(StreamEvent::OutputData {
                pane_id: pid,
                data: "init\n".to_string(),
                received_at: 100,
                overflow: false,
            });
        }
        assert_eq!(ingester.active_panes(), 3);

        let segs = ingester.process(StreamEvent::Disconnected {
            reason: "mux_restart".to_string(),
        });
        assert_eq!(segs.len(), 3);

        for seg in &segs {
            assert!(matches!(
                &seg.kind,
                CapturedSegmentKind::Gap { reason } if reason == "stream_disconnected:mux_restart"
            ));
        }

        // All panes should now have pending overflow
        for pid in [1, 2, 3] {
            assert!(ingester.has_pending_overflow(pid));
        }
    }

    #[test]
    fn ingester_reconnect_after_disconnect_emits_gap() {
        let mut ingester = StreamIngester::new();

        // Establish pane
        ingester.process(StreamEvent::OutputData {
            pane_id: 1,
            data: "before\n".to_string(),
            received_at: 100,
            overflow: false,
        });

        // Disconnect
        ingester.process(StreamEvent::Disconnected {
            reason: "network".to_string(),
        });

        // Reconnect with new data — should get GAP + Delta
        let segs = ingester.process(StreamEvent::OutputData {
            pane_id: 1,
            data: "after\n".to_string(),
            received_at: 300,
            overflow: false,
        });
        assert_eq!(segs.len(), 2);
        assert!(matches!(
            &segs[0].kind,
            CapturedSegmentKind::Gap { reason } if reason == "stream_overflow"
        ));
        assert_eq!(segs[1].kind, CapturedSegmentKind::Delta);
        assert_eq!(segs[1].content, "after\n");
    }

    // --- Ingester counters ---

    #[test]
    fn ingester_counters_track_segments_and_gaps() {
        let mut ingester = StreamIngester::new();
        assert_eq!(ingester.total_segments(), 0);
        assert_eq!(ingester.total_gaps(), 0);

        // 1 delta
        ingester.process(StreamEvent::OutputData {
            pane_id: 1,
            data: "a".to_string(),
            received_at: 100,
            overflow: false,
        });
        assert_eq!(ingester.total_segments(), 1);
        assert_eq!(ingester.total_gaps(), 0);

        // 1 overflow → GAP + Delta = 2 segments, 1 gap
        ingester.process(StreamEvent::OutputData {
            pane_id: 1,
            data: "b".to_string(),
            received_at: 200,
            overflow: true,
        });
        assert_eq!(ingester.total_segments(), 3);
        assert_eq!(ingester.total_gaps(), 1);

        // Close pane → 1 gap
        ingester.process(StreamEvent::PaneClosed { pane_id: 1 });
        assert_eq!(ingester.total_segments(), 4);
        assert_eq!(ingester.total_gaps(), 2);
    }

    // --- StreamChannel: bounded channel with overflow ---

    #[test]
    fn stream_channel_basic_send_recv() {
        let cfg = StreamChannelConfig {
            capacity: 4,
            overflow_policy: OverflowPolicy::EmitGap,
        };
        let mut ch = StreamChannel::new(&cfg);

        assert!(ch.is_empty());
        assert!(!ch.is_full());

        let ok = ch.send(StreamEvent::OutputData {
            pane_id: 1,
            data: "a".to_string(),
            received_at: 100,
            overflow: false,
        });
        assert!(ok);
        assert_eq!(ch.len(), 1);

        let event = ch.recv().expect("should have event");
        assert!(matches!(event, StreamEvent::OutputData { pane_id: 1, .. }));
        assert!(ch.is_empty());
    }

    #[test]
    fn stream_channel_emit_gap_drops_on_full() {
        let cfg = StreamChannelConfig {
            capacity: 2,
            overflow_policy: OverflowPolicy::EmitGap,
        };
        let mut ch = StreamChannel::new(&cfg);

        // Fill channel
        ch.send(StreamEvent::OutputData {
            pane_id: 1,
            data: "a".to_string(),
            received_at: 100,
            overflow: false,
        });
        ch.send(StreamEvent::OutputData {
            pane_id: 1,
            data: "b".to_string(),
            received_at: 200,
            overflow: false,
        });
        assert!(ch.is_full());

        // Third send should fail (dropped)
        let ok = ch.send(StreamEvent::OutputData {
            pane_id: 1,
            data: "c".to_string(),
            received_at: 300,
            overflow: false,
        });
        assert!(!ok, "should drop when full with EmitGap policy");
        assert_eq!(ch.events_dropped, 1);
        assert_eq!(ch.len(), 2); // still 2

        // Already-buffered events predate the dropped event, so neither should
        // be tagged with the overflow marker.
        let event = ch.recv().unwrap();
        if let StreamEvent::OutputData { overflow, .. } = event {
            assert!(
                !overflow,
                "pre-drop buffered events must not be retroactively tagged"
            );
        }
        let event = ch.recv().unwrap();
        if let StreamEvent::OutputData { overflow, .. } = event {
            assert!(
                !overflow,
                "all pre-drop buffered events must drain before the gap marker"
            );
        }

        // The next accepted event for the pane is the first event after the
        // dropped data, so it carries overflow=true.
        let ok = ch.send(StreamEvent::OutputData {
            pane_id: 1,
            data: "d".to_string(),
            received_at: 400,
            overflow: false,
        });
        assert!(ok);
        let event = ch.recv().unwrap();
        if let StreamEvent::OutputData { data, overflow, .. } = event {
            assert_eq!(data, "d");
            assert!(
                overflow,
                "first post-drop accepted event should carry the overflow marker"
            );
        }
    }

    #[test]
    fn stream_channel_drop_telemetry_saturates_without_changing_overflow_policy() {
        let cfg = StreamChannelConfig {
            capacity: 1,
            overflow_policy: OverflowPolicy::EmitGap,
        };
        let mut channel = StreamChannel::new(&cfg);
        assert!(channel.send(StreamEvent::OutputData {
            pane_id: 1,
            data: "first".to_string(),
            received_at: 1,
            overflow: false,
        }));
        channel.events_dropped = u64::MAX;

        assert!(!channel.send(StreamEvent::OutputData {
            pane_id: 1,
            data: "dropped".to_string(),
            received_at: 2,
            overflow: false,
        }));
        assert_eq!(channel.events_dropped, u64::MAX);
    }

    #[test]
    fn stream_channel_drop_oldest_evicts() {
        let cfg = StreamChannelConfig {
            capacity: 2,
            overflow_policy: OverflowPolicy::DropOldest,
        };
        let mut ch = StreamChannel::new(&cfg);

        // Fill
        ch.send(StreamEvent::OutputData {
            pane_id: 1,
            data: "a".to_string(),
            received_at: 100,
            overflow: false,
        });
        ch.send(StreamEvent::OutputData {
            pane_id: 1,
            data: "b".to_string(),
            received_at: 200,
            overflow: false,
        });

        // Third: evicts "a", inserts "c"
        let ok = ch.send(StreamEvent::OutputData {
            pane_id: 1,
            data: "c".to_string(),
            received_at: 300,
            overflow: false,
        });
        assert!(ok, "DropOldest should always accept");
        assert_eq!(ch.events_dropped, 1);
        assert_eq!(ch.len(), 2);

        // First recv should be "b" (oldest remaining)
        let event = ch.recv().unwrap();
        if let StreamEvent::OutputData { data, overflow, .. } = event {
            assert_eq!(data, "b");
            assert!(
                overflow,
                "oldest remaining event follows the evicted data and must carry overflow"
            );
        }

        let event = ch.recv().unwrap();
        if let StreamEvent::OutputData { data, overflow, .. } = event {
            assert_eq!(data, "c");
            assert!(
                !overflow,
                "new event should not get the marker when an earlier buffered event can carry it"
            );
        }
    }

    #[test]
    fn stream_channel_drop_oldest_tags_new_event_when_no_buffered_successor_exists() {
        let cfg = StreamChannelConfig {
            capacity: 1,
            overflow_policy: OverflowPolicy::DropOldest,
        };
        let mut ch = StreamChannel::new(&cfg);

        ch.send(StreamEvent::OutputData {
            pane_id: 1,
            data: "first".to_string(),
            received_at: 100,
            overflow: false,
        });

        let ok = ch.send(StreamEvent::OutputData {
            pane_id: 1,
            data: "second".to_string(),
            received_at: 200,
            overflow: false,
        });
        assert!(ok);
        assert_eq!(ch.events_dropped, 1);

        let event = ch.recv().unwrap();
        if let StreamEvent::OutputData { data, overflow, .. } = event {
            assert_eq!(data, "second");
            assert!(
                overflow,
                "new event is the first per-pane successor after the eviction"
            );
        }
    }

    // --- Integration: fake stream through channel + ingester ---

    #[test]
    fn integration_fake_stream_no_drops() {
        let cfg = StreamChannelConfig {
            capacity: 128,
            overflow_policy: OverflowPolicy::EmitGap,
        };
        let mut channel = StreamChannel::new(&cfg);
        let mut ingester = StreamIngester::new();

        // Simulate a stream of 50 events for 2 panes
        for i in 0u64..50 {
            let pane_id = (i % 2) + 1;
            channel.send(StreamEvent::OutputData {
                pane_id,
                data: format!("line {i}\n"),
                received_at: i as i64 * 10,
                overflow: false,
            });
        }

        // Drain channel through ingester
        let mut all_segments: Vec<CapturedSegment> = Vec::new();
        while let Some(event) = channel.recv() {
            all_segments.extend(ingester.process(event));
        }

        assert_eq!(channel.events_dropped, 0);
        assert_eq!(all_segments.len(), 50);

        // Verify seq monotonicity per pane
        let mut seqs_per_pane: HashMap<u64, Vec<u64>> = HashMap::new();
        for seg in &all_segments {
            seqs_per_pane.entry(seg.pane_id).or_default().push(seg.seq);
        }

        for (pid, seqs) in &seqs_per_pane {
            for window in seqs.windows(2) {
                assert!(
                    window[1] > window[0],
                    "pane {pid}: seq not monotonic: {} -> {}",
                    window[0],
                    window[1]
                );
            }
        }

        // Each pane should have 25 segments, seqs 0..24
        assert_eq!(seqs_per_pane[&1].len(), 25);
        assert_eq!(seqs_per_pane[&2].len(), 25);
        assert_eq!(*seqs_per_pane[&1].last().unwrap(), 24);
        assert_eq!(*seqs_per_pane[&2].last().unwrap(), 24);
    }

    #[test]
    fn integration_slow_consumer_overflow() {
        // Tiny channel to force overflow quickly
        let cfg = StreamChannelConfig {
            capacity: 3,
            overflow_policy: OverflowPolicy::EmitGap,
        };
        let mut channel = StreamChannel::new(&cfg);

        // Send 10 events without consuming — 7 should be dropped
        for i in 0u64..10 {
            channel.send(StreamEvent::OutputData {
                pane_id: 1,
                data: format!("line {i}\n"),
                received_at: i as i64 * 10,
                overflow: false,
            });
        }
        assert_eq!(channel.events_dropped, 7);
        assert_eq!(channel.len(), 3);

        // Drain through ingester
        let mut ingester = StreamIngester::new();
        let mut all_segments: Vec<CapturedSegment> = Vec::new();
        while let Some(event) = channel.recv() {
            all_segments.extend(ingester.process(event));
        }

        assert!(
            all_segments
                .iter()
                .all(|segment| segment.kind == CapturedSegmentKind::Delta),
            "pre-drop buffered events should drain before any overflow GAP"
        );

        assert!(channel.send(StreamEvent::OutputData {
            pane_id: 1,
            data: "after-overflow\n".to_string(),
            received_at: 10_000,
            overflow: false,
        }));
        while let Some(event) = channel.recv() {
            all_segments.extend(ingester.process(event));
        }

        // Should have GAP(s) + Deltas once the first post-drop event arrives.
        let gaps: Vec<_> = all_segments
            .iter()
            .filter(|s| matches!(s.kind, CapturedSegmentKind::Gap { .. }))
            .collect();
        let delta_count = all_segments
            .iter()
            .filter(|s| s.kind == CapturedSegmentKind::Delta)
            .count();

        // At least one gap must exist (overflow occurred)
        assert!(
            !gaps.is_empty(),
            "overflow must produce at least one GAP segment before post-drop data"
        );

        // All segments for pane 1 must have monotonic seq
        let mut prev_seq: Option<u64> = None;
        for seg in &all_segments {
            assert_eq!(seg.pane_id, 1);
            if let Some(p) = prev_seq {
                assert!(seg.seq > p, "seq not monotonic: {p} -> {}", seg.seq);
            }
            prev_seq = Some(seg.seq);
        }

        // Total = gaps + deltas = all segments
        assert_eq!(gaps.len() + delta_count, all_segments.len());
    }

    #[test]
    fn integration_bounded_channel_multi_pane_overflow() {
        let cfg = StreamChannelConfig {
            capacity: 3,
            overflow_policy: OverflowPolicy::EmitGap,
        };
        let mut channel = StreamChannel::new(&cfg);
        let mut ingester = StreamIngester::new();

        // Interleave 3 panes, 10 events each (30 total into capacity=3)
        // Consumer only drains every 10 events (very slow)
        for i in 0u64..30 {
            let pane_id = (i % 3) + 1;
            channel.send(StreamEvent::OutputData {
                pane_id,
                data: format!("data {i}\n"),
                received_at: i as i64,
                overflow: false,
            });

            // Consumer runs every 10 events (slow consumer simulation)
            if (i + 1) % 10 == 0 {
                while let Some(event) = channel.recv() {
                    ingester.process(event);
                }
            }
        }

        // Drain remainder
        while let Some(event) = channel.recv() {
            ingester.process(event);
        }

        // Verify seq monotonicity for all panes
        for pid in 1..=3 {
            if let Some(cursor) = ingester.cursor_for(pid) {
                assert!(cursor.next_seq > 0, "pane {pid} should have segments");
            }
        }

        // Some drops should have occurred (30 events, capacity 3, drained every 10)
        assert!(channel.events_dropped > 0, "should have drops");
        assert!(
            ingester.total_gaps() > 0,
            "drops must manifest as GAP segments"
        );
    }

    #[test]
    fn integration_cancellation_reconnect() {
        let mut ingester = StreamIngester::new();

        // Phase 1: normal streaming
        for i in 0u64..5 {
            ingester.process(StreamEvent::OutputData {
                pane_id: 1,
                data: format!("phase1:{i}\n"),
                received_at: i as i64,
                overflow: false,
            });
        }
        assert_eq!(ingester.cursor_for(1).unwrap().next_seq, 5);

        // Phase 2: disconnect (simulating cancellation)
        let disconnect_segs = ingester.process(StreamEvent::Disconnected {
            reason: "cancelled".to_string(),
        });
        assert_eq!(disconnect_segs.len(), 1);
        assert!(matches!(
            &disconnect_segs[0].kind,
            CapturedSegmentKind::Gap { .. }
        ));

        // Phase 3: reconnect with new data
        let reconnect_segs = ingester.process(StreamEvent::OutputData {
            pane_id: 1,
            data: "phase3:0\n".to_string(),
            received_at: 1000,
            overflow: false,
        });
        // Should be GAP (from pending overflow) + Delta
        assert_eq!(reconnect_segs.len(), 2);
        assert!(matches!(
            &reconnect_segs[0].kind,
            CapturedSegmentKind::Gap { .. }
        ));
        assert_eq!(reconnect_segs[1].kind, CapturedSegmentKind::Delta);

        // Verify overall seq monotonicity
        let cursor = ingester.cursor_for(1).unwrap();
        // 5 (phase1) + 1 (disconnect gap) + 1 (overflow gap) + 1 (reconnect delta) = 8
        assert_eq!(cursor.next_seq, 8);
    }

    // --- Property: no silent drops ---

    #[test]
    fn property_drop_burst_manifests_as_gap_on_next_accepted_event() {
        // For various channel sizes and event counts, verify that a dropped
        // burst produces a GAP before the first subsequently accepted event.
        for capacity in [1, 2, 5, 10] {
            let cfg = StreamChannelConfig {
                capacity,
                overflow_policy: OverflowPolicy::EmitGap,
            };
            let mut channel = StreamChannel::new(&cfg);
            let mut ingester = StreamIngester::new();
            let total_events = 50;

            // Send all events without consuming (worst case)
            for i in 0u64..total_events {
                channel.send(StreamEvent::OutputData {
                    pane_id: 1,
                    data: format!("{i}\n"),
                    received_at: i as i64,
                    overflow: false,
                });
            }

            let dropped = channel.events_dropped;
            assert_eq!(
                dropped,
                total_events.saturating_sub(capacity as u64),
                "capacity={capacity}"
            );

            // Drain through ingester
            let mut all_segs = Vec::new();
            while let Some(event) = channel.recv() {
                all_segs.extend(ingester.process(event));
            }

            if dropped > 0 {
                assert!(channel.send(StreamEvent::OutputData {
                    pane_id: 1,
                    data: "post-drop-recovery\n".to_string(),
                    received_at: i64::try_from(total_events).unwrap_or(i64::MAX),
                    overflow: false,
                }));
                while let Some(event) = channel.recv() {
                    all_segs.extend(ingester.process(event));
                }

                let gap_count = all_segs
                    .iter()
                    .filter(|s| matches!(s.kind, CapturedSegmentKind::Gap { .. }))
                    .count();
                assert!(
                    gap_count >= 1,
                    "capacity={capacity}: dropped={dropped} but gap_count={gap_count}"
                );
            }

            // Seq monotonicity
            let mut prev: Option<u64> = None;
            for seg in &all_segs {
                if let Some(p) = prev {
                    assert!(seg.seq > p);
                }
                prev = Some(seg.seq);
            }
        }
    }

    // --- StreamIngester Default trait ---

    #[test]
    fn stream_ingester_default() {
        let ingester = StreamIngester::default();
        assert_eq!(ingester.active_panes(), 0);
        assert_eq!(ingester.total_segments(), 0);
        assert_eq!(ingester.total_gaps(), 0);
    }

    // --- OverflowPolicy serialization ---

    #[test]
    fn overflow_policy_serde_roundtrip() {
        let emit_gap = OverflowPolicy::EmitGap;
        let json = serde_json::to_string(&emit_gap).unwrap();
        assert_eq!(json, "\"emit_gap\"");
        let parsed: OverflowPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, OverflowPolicy::EmitGap);

        let drop_oldest = OverflowPolicy::DropOldest;
        let json = serde_json::to_string(&drop_oldest).unwrap();
        let parsed: OverflowPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, OverflowPolicy::DropOldest);
    }

    #[test]
    fn stream_channel_config_serde_roundtrip() {
        let cfg = StreamChannelConfig {
            capacity: 256,
            overflow_policy: OverflowPolicy::DropOldest,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: StreamChannelConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.capacity, 256);
        assert_eq!(parsed.overflow_policy, OverflowPolicy::DropOldest);
    }

    // --- Channel minimum capacity enforcement ---

    #[test]
    fn stream_channel_min_capacity_is_one() {
        let cfg = StreamChannelConfig {
            capacity: 0, // should be clamped to 1
            overflow_policy: OverflowPolicy::EmitGap,
        };
        let mut ch = StreamChannel::new(&cfg);

        // Should accept at least 1 event
        let ok = ch.send(StreamEvent::OutputData {
            pane_id: 1,
            data: "a".to_string(),
            received_at: 100,
            overflow: false,
        });
        assert!(ok);
        assert!(ch.is_full());
    }

    // =========================================================================
    // Batch: DarkBadger wa-1u90p.7.1 — trait & edge coverage
    // =========================================================================

    // --- PaneLifecycleIdentity ---

    #[test]
    fn pane_lifecycle_identity_debug_clone() {
        let identity = PaneLifecycleIdentity {
            pane_id: 7,
            domain_id: Some(2),
            tty_name: Some("/dev/pts/7".to_string()),
        };
        let cloned = identity.clone();
        assert_eq!(cloned.domain_id, Some(2));
        let dbg = format!("{identity:?}");
        assert!(dbg.contains("PaneLifecycleIdentity"));
    }

    #[test]
    fn pane_lifecycle_identity_hash_in_hashset() {
        use std::collections::HashSet;
        let first = PaneLifecycleIdentity {
            pane_id: 1,
            domain_id: Some(2),
            tty_name: Some("tty-a".into()),
        };
        let duplicate = first.clone();
        let replacement = PaneLifecycleIdentity {
            tty_name: Some("tty-b".into()),
            ..first.clone()
        };
        let mut set = HashSet::new();
        set.insert(first);
        set.insert(duplicate);
        set.insert(replacement);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn pane_lifecycle_identity_detects_exact_domain_mismatch() {
        let first = PaneLifecycleIdentity {
            pane_id: 1,
            domain_id: Some(7),
            tty_name: None,
        };
        let replacement = PaneLifecycleIdentity {
            domain_id: Some(8),
            ..first.clone()
        };
        assert_eq!(
            first.continuity_with(&replacement),
            PaneLifecycleContinuity::Replaced
        );
    }

    // --- ObservationDecision ---

    #[test]
    fn observation_decision_debug_clone_eq() {
        let obs = ObservationDecision::Observed;
        let ign = ObservationDecision::Ignored {
            reason: "test".into(),
        };
        assert_eq!(obs.clone(), ObservationDecision::Observed);
        assert_ne!(obs, ign);
        assert!(obs.is_observed());
        assert!(!ign.is_observed());
        assert_eq!(ign.ignore_reason(), Some("test"));
        assert_eq!(obs.ignore_reason(), None);
        let dbg = format!("{:?}", ign);
        assert!(dbg.contains("Ignored"));
    }

    // --- PanePriorityOverride ---

    #[test]
    fn pane_priority_override_debug_clone_serde() {
        let ov = PanePriorityOverride {
            priority: 10,
            set_at: 1000,
            expires_at: Some(2000),
        };
        let cloned = ov.clone();
        assert_eq!(cloned.priority, 10);
        let dbg = format!("{:?}", ov);
        assert!(dbg.contains("PanePriorityOverride"));

        let json = serde_json::to_string(&ov).unwrap();
        let parsed: PanePriorityOverride = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.priority, 10);
        assert_eq!(parsed.expires_at, Some(2000));
    }

    #[test]
    fn pane_priority_override_no_expiry_serde() {
        let ov = PanePriorityOverride {
            priority: 0,
            set_at: 500,
            expires_at: None,
        };
        let json = serde_json::to_string(&ov).unwrap();
        let parsed: PanePriorityOverride = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.expires_at, None);
    }

    // --- DiscoveryDiff ---

    #[test]
    fn discovery_diff_default_is_empty() {
        let d = DiscoveryDiff::default();
        assert!(d.is_empty());
        assert_eq!(d.change_count(), 0);
    }

    #[test]
    fn discovery_diff_debug_clone() {
        let mut d = DiscoveryDiff::default();
        d.new_panes.push(1);
        d.closed_panes.push(2);
        let cloned = d.clone();
        assert_eq!(cloned.change_count(), 2);
        assert!(!cloned.is_empty());
        let dbg = format!("{:?}", d);
        assert!(dbg.contains("DiscoveryDiff"));
    }

    // --- PaneCursor ---

    #[test]
    fn pane_cursor_from_seq() {
        let c = PaneCursor::from_seq(42, 10);
        assert_eq!(c.pane_id, 42);
        assert_eq!(c.next_seq, 10);
        assert_eq!(c.last_seq(), 9);
    }

    #[test]
    fn pane_cursor_last_seq_at_zero() {
        let c = PaneCursor::new(1);
        assert_eq!(c.last_seq(), -1);
    }

    #[test]
    fn pane_cursor_debug_clone() {
        let c = PaneCursor::new(5);
        let cloned = c.clone();
        assert_eq!(cloned.pane_id, 5);
        assert_eq!(cloned.next_seq, 0);
        let dbg = format!("{:?}", c);
        assert!(dbg.contains("PaneCursor"));
    }

    // --- CapturedSegment ---

    #[test]
    fn captured_segment_debug_clone_eq() {
        let seg = CapturedSegment {
            pane_id: 1,
            seq: 0,
            seq_correction: 0,
            content: "hello".to_string(),
            kind: CapturedSegmentKind::Delta,
            captured_at: 1000,
        };
        let cloned = seg.clone();
        assert_eq!(seg, cloned);
        let dbg = format!("{:?}", seg);
        assert!(dbg.contains("CapturedSegment"));
    }

    // --- CapturedSegmentKind ---

    #[test]
    fn captured_segment_kind_eq_variants() {
        assert_eq!(CapturedSegmentKind::Delta, CapturedSegmentKind::Delta);
        let g1 = CapturedSegmentKind::Gap { reason: "a".into() };
        let g2 = CapturedSegmentKind::Gap { reason: "a".into() };
        let g3 = CapturedSegmentKind::Gap { reason: "b".into() };
        assert_eq!(g1, g2);
        assert_ne!(g1, g3);
        assert_ne!(CapturedSegmentKind::Delta, g1);
    }

    // --- PersistedCapture ---

    #[test]
    fn persisted_capture_debug_clone() {
        let pc = PersistedCapture {
            segment: Segment {
                id: 0,
                pane_id: 1,
                seq: 0,
                content: "data".into(),
                content_len: 4,
                content_hash: None,
                captured_at: 100,
            },
            gap: None,
        };
        let cloned = pc.clone();
        assert_eq!(cloned.segment.pane_id, 1);
        assert!(cloned.gap.is_none());
        let dbg = format!("{:?}", pc);
        assert!(dbg.contains("PersistedCapture"));
    }

    // --- ShellState ---

    #[test]
    fn shell_state_default_is_unknown() {
        assert_eq!(ShellState::default(), ShellState::Unknown);
    }

    #[test]
    fn shell_state_is_at_prompt_all_variants() {
        assert!(!ShellState::Unknown.is_at_prompt());
        assert!(ShellState::PromptActive.is_at_prompt());
        assert!(ShellState::InputActive.is_at_prompt());
        assert!(!ShellState::CommandRunning.is_at_prompt());
        assert!(ShellState::CommandFinished { exit_code: Some(0) }.is_at_prompt());
    }

    #[test]
    fn shell_state_is_command_running_all() {
        assert!(ShellState::CommandRunning.is_command_running());
        assert!(!ShellState::PromptActive.is_command_running());
        assert!(!ShellState::Unknown.is_command_running());
    }

    #[test]
    fn shell_state_copy_eq() {
        let s = ShellState::CommandRunning;
        let c = s; // Copy
        assert_eq!(s, c);
    }

    // --- AltScreenChange ---

    #[test]
    fn alt_screen_change_debug_clone_copy_eq() {
        let e = AltScreenChange::Entered;
        let x = AltScreenChange::Exited;
        let c = e; // Copy
        assert_eq!(e, c);
        assert_ne!(e, x);
        let dbg = format!("{:?}", e);
        assert!(dbg.contains("Entered"));
    }

    // --- Osc133Marker ---

    #[test]
    fn osc133_marker_debug_clone_copy_eq() {
        let m = Osc133Marker::PromptStart;
        let c = m; // Copy
        assert_eq!(m, c);
        assert_ne!(Osc133Marker::PromptStart, Osc133Marker::CommandStart);
        assert_ne!(Osc133Marker::CommandExecuted, Osc133Marker::PromptStart);
        let dbg = format!("{:?}", m);
        assert!(dbg.contains("PromptStart"));
    }

    // --- OverflowPolicy ---

    #[test]
    fn overflow_policy_debug_clone_copy_eq() {
        let e = OverflowPolicy::EmitGap;
        let d = OverflowPolicy::DropOldest;
        let c = e; // Copy
        assert_eq!(e, c);
        assert_ne!(e, d);
        assert_eq!(OverflowPolicy::default(), OverflowPolicy::EmitGap);
    }

    // --- StreamEvent ---

    #[test]
    fn stream_event_debug_clone_eq() {
        let e1 = StreamEvent::OutputData {
            pane_id: 1,
            data: "hello".into(),
            received_at: 100,
            overflow: false,
        };
        let e2 = e1.clone();
        assert_eq!(e1, e2);

        let e3 = StreamEvent::PaneClosed { pane_id: 1 };
        assert_ne!(e1, e3);

        let e4 = StreamEvent::Disconnected {
            reason: "gone".into(),
        };
        let dbg = format!("{:?}", e4);
        assert!(dbg.contains("Disconnected"));
    }

    // --- hex_encode ---

    #[test]
    fn hex_encode_empty() {
        assert_eq!(hex_encode(&[]), "");
    }

    #[test]
    fn hex_encode_known_values() {
        assert_eq!(hex_encode(&[0xff, 0x00, 0x01]), "ff0001");
        assert_eq!(hex_encode(&[0xab, 0xcd]), "abcd");
    }

    // --- trim_utf8_tail_to_max_bytes ---

    #[test]
    fn trim_utf8_tail_within_limit() {
        assert_eq!(trim_utf8_tail_to_max_bytes("hello", 10), "hello");
    }

    #[test]
    fn trim_utf8_tail_zero_max() {
        assert_eq!(trim_utf8_tail_to_max_bytes("hello", 0), "");
    }

    #[test]
    fn trim_utf8_tail_truncates_to_char_boundary() {
        // "é" is 2 bytes; "café" is 5 bytes
        let result = trim_utf8_tail_to_max_bytes("café", 4);
        // Should be 4 bytes from the tail, staying on char boundary
        assert!(result.is_char_boundary(0));
        assert!(result.len() <= 4);
    }

    /// [ft-a0up5] When the last character is wider than max_bytes and
    /// no shorter suffix exists, the function MUST return empty
    /// rather than the previous "last full character" fallback that
    /// returned the entire 3-byte 中 even when only 1 or 2 bytes were
    /// permitted. Pin the cap-respecting contract for all such cases.
    #[test]
    fn ft_a0up5_trim_utf8_tail_returns_at_most_max_bytes_for_multibyte_only_text() {
        let result = trim_utf8_tail_to_max_bytes("中", 1);
        assert!(
            result.len() <= 1,
            "max_bytes=1 must not return {} bytes (the bare 3-byte 中)",
            result.len()
        );
        let result = trim_utf8_tail_to_max_bytes("中", 2);
        assert!(
            result.len() <= 2,
            "max_bytes=2 must not return {} bytes",
            result.len()
        );
    }

    /// Symmetric: "a中" with max_bytes=2 must not return "中" (3 bytes).
    /// Pre-fix returned the full 中 character via the
    /// last-full-character fallback even though the cap was 2.
    #[test]
    fn ft_a0up5_trim_utf8_tail_handles_2_byte_cap_with_3_byte_trailing_char() {
        let result = trim_utf8_tail_to_max_bytes("a中", 2);
        assert!(
            result.len() <= 2,
            "max_bytes=2 must not return {} bytes (was returning the bare 中)",
            result.len()
        );
        // Acceptable post-fix outputs: "" or "a" — both are <= 2 bytes
        // and on char boundaries.
        assert!(result.is_char_boundary(0));
        assert!(result.is_char_boundary(result.len()));
    }

    /// Arbitrary unicode text including multi-byte code points and emoji,
    /// to exercise the char-boundary snapping in trim_utf8_tail_to_max_bytes.
    fn arb_unicode_text() -> impl proptest::prelude::Strategy<Value = String> {
        use proptest::prelude::*;
        prop::collection::vec(any::<char>(), 0..30).prop_map(|cs| cs.into_iter().collect())
    }

    proptest::proptest! {
        /// Core contract (ft-a0up5): the result never exceeds max_bytes,
        /// regardless of multi-byte boundaries — the byte cap is a hard
        /// ceiling, not a "last full character" best-effort.
        #[test]
        fn prop_trim_utf8_tail_respects_byte_cap(
            text in arb_unicode_text(),
            max_bytes in 0usize..40,
        ) {
            let result = trim_utf8_tail_to_max_bytes(&text, max_bytes);
            proptest::prop_assert!(
                result.len() <= max_bytes,
                "result {} bytes exceeds cap {} for {:?}", result.len(), max_bytes, text
            );
        }

        /// The kept content is always a suffix of the input — the function
        /// trims from the front to retain the tail, never reorders or
        /// substitutes bytes.
        #[test]
        fn prop_trim_utf8_tail_is_suffix_of_input(
            text in arb_unicode_text(),
            max_bytes in 0usize..40,
        ) {
            let result = trim_utf8_tail_to_max_bytes(&text, max_bytes);
            proptest::prop_assert!(
                text.ends_with(result.as_str()),
                "result {:?} is not a suffix of input {:?}", result, text
            );
        }

        /// When the input already fits, trimming is a no-op: the text is
        /// returned byte-for-byte unchanged.
        #[test]
        fn prop_trim_utf8_tail_noop_within_limit(
            text in arb_unicode_text(),
            slack in 0usize..8,
        ) {
            let max_bytes = text.len() + slack; // guaranteed >= text.len()
            let result = trim_utf8_tail_to_max_bytes(&text, max_bytes);
            proptest::prop_assert_eq!(
                result.as_str(), text.as_str(),
                "within-limit trim must be a no-op"
            );
        }

        /// Idempotence: trimming an already-trimmed result with the same
        /// cap yields the same value (the first trim's output already
        /// satisfies the cap, so the second pass cannot shrink it further).
        #[test]
        fn prop_trim_utf8_tail_idempotent(
            text in arb_unicode_text(),
            max_bytes in 0usize..40,
        ) {
            let once = trim_utf8_tail_to_max_bytes(&text, max_bytes);
            let twice = trim_utf8_tail_to_max_bytes(&once, max_bytes);
            proptest::prop_assert_eq!(
                once.as_str(), twice.as_str(),
                "trim must be idempotent under a fixed cap"
            );
        }
    }

    /// The wrapper enforce_segment_size_for_persistence must NEVER emit
    /// a segment whose content exceeds max_segment_bytes — the gap
    /// reason already captures the oversize signal, and downstream
    /// caps (FTS column truncation, DB CHECK constraints) treat
    /// max_segment_bytes as a hard ceiling.
    #[test]
    fn ft_a0up5_enforce_segment_size_respects_kept_bytes_le_max_bytes() {
        let captured = CapturedSegment {
            pane_id: 1,
            seq: 0,
            seq_correction: 0,
            content: "中".to_string(),
            kind: CapturedSegmentKind::Delta,
            captured_at: 0,
        };
        let (bounded, detail) = enforce_segment_size_for_persistence(&captured, 1);
        assert!(
            bounded.content.len() <= 1,
            "enforce returned {} bytes for max_bytes=1; persistence cap must be a ceiling",
            bounded.content.len()
        );
        let detail = detail.expect("oversize input must report enforcement");
        assert_eq!(detail.max_bytes, 1);
        assert_eq!(detail.original_bytes, 3);
        assert!(detail.kept_bytes <= 1);
    }

    #[test]
    fn registry_adopt_uuid_updates_index_and_entry() {
        let mut reg = PaneRegistry::new();
        let pane = make_pane_info(1, 100, 10);
        reg.discovery_tick(vec![pane]);

        let old_uuid = reg.get_entry(1).unwrap().pane_uuid.clone();
        let new_uuid = "00000000000000000000000000000001".to_string();

        assert!(reg.get_pane_id_by_uuid(&old_uuid).is_some());
        assert!(reg.get_pane_id_by_uuid(&new_uuid).is_none());

        let success = reg.adopt_uuid(1, new_uuid.clone());
        assert!(success);

        let entry = reg.get_entry(1).unwrap();
        assert_eq!(entry.pane_uuid, new_uuid);

        // Check index updates
        assert_eq!(reg.get_pane_id_by_uuid(&new_uuid), Some(1));
        assert!(reg.get_pane_id_by_uuid(&old_uuid).is_none());
    }

    #[test]
    fn registry_adopt_uuid_rejects_collision_without_corrupting_index() {
        let mut reg = PaneRegistry::new();
        reg.discovery_tick(vec![make_pane_info(1, 100, 10), make_pane_info(2, 100, 11)]);

        let uuid_one = reg.get_entry(1).unwrap().pane_uuid.clone();
        let uuid_two = reg.get_entry(2).unwrap().pane_uuid.clone();

        let success = reg.adopt_uuid(1, uuid_two.clone());
        assert!(!success);

        assert_eq!(reg.get_entry(1).unwrap().pane_uuid, uuid_one);
        assert_eq!(reg.get_entry(2).unwrap().pane_uuid, uuid_two);
        assert_eq!(reg.get_pane_id_by_uuid(&uuid_one), Some(1));
        assert_eq!(reg.get_pane_id_by_uuid(&uuid_two), Some(2));
    }
}
