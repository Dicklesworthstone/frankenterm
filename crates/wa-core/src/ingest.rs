//! Ingest pipeline for pane output capture
//!
//! Handles delta extraction, sequence numbering, gap detection, and pane discovery.
//!
//! # Discovery Loop
//!
//! The discovery system polls `wezterm cli list` to:
//! - Track pane lifecycle (new/closed/changed)
//! - Apply include/exclude filters for privacy and performance
//! - Maintain stable pane identities via fingerprinting
//!
//! # Delta Extraction
//!
//! Converts repeated snapshots into minimal deltas using overlap matching.

use std::collections::{HashMap, HashSet, VecDeque, hash_map::Entry};
use std::hash::Hash;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::PaneFilterConfig;
use crate::error::Result;
use crate::storage::{Gap, PaneRecord, Segment, StorageHandle};
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
// Fingerprinting
// =============================================================================

/// A fingerprint uniquely identifies a pane "generation".
///
/// A generation represents a logical session within a pane. When domain, title,
/// or cwd change, we consider it a new generation (possibly a new shell session,
/// connection to different host, or major context switch).
///
/// Components:
/// - domain name (e.g., "local", "SSH:hostname")
/// - title and cwd at the start of this generation
/// - optional hash of initial content (first ~50 lines)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PaneFingerprint {
    /// Domain name (e.g., "local", "SSH:hostname")
    pub domain: String,
    /// Title at the start of this generation
    pub initial_title: String,
    /// Working directory at the start of this generation
    pub initial_cwd: String,
    /// Hash of initial content (first ~50 lines), 0 if not captured
    pub content_hash: u64,
}

impl PaneFingerprint {
    /// Create a fingerprint from pane info and initial content
    #[must_use]
    pub fn new(info: &PaneInfo, initial_content: Option<&str>) -> Self {
        let domain = info.inferred_domain();
        let initial_title = info.title.clone().unwrap_or_default();
        let initial_cwd = info.cwd.clone().unwrap_or_default();

        let content_hash = initial_content.map_or(0, |content| {
            // Hash first ~50 lines to capture shell banner/prompt
            let truncated: String = content.lines().take(50).collect::<Vec<_>>().join("\n");
            hash_text(&truncated)
        });

        Self {
            domain,
            initial_title,
            initial_cwd,
            content_hash,
        }
    }

    /// Create a fingerprint without content (for quick identification)
    #[must_use]
    pub fn without_content(info: &PaneInfo) -> Self {
        Self::new(info, None)
    }

    /// Check if this fingerprint indicates the same pane generation
    #[must_use]
    pub fn is_same_generation(&self, other: &Self) -> bool {
        // Domain must match exactly
        if self.domain != other.domain {
            return false;
        }

        // Title and cwd must be close (allow some drift)
        // For now, just compare directly - future: fuzzy matching
        self.initial_title == other.initial_title && self.initial_cwd == other.initial_cwd
    }
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

/// Extended pane state with fingerprint and observation tracking
#[derive(Debug, Clone)]
pub struct PaneEntry {
    /// Current pane info from WezTerm
    pub info: PaneInfo,
    /// Stable fingerprint for this pane generation
    pub fingerprint: PaneFingerprint,
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
    /// Generation number (increments when fingerprint changes)
    pub generation: u32,
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
}

impl PaneEntry {
    /// Create a new pane entry
    ///
    /// Generates a per-runtime `pane_uuid` based on domain, pane_id, and creation time.
    /// The UUID is assigned once and never changes for this pane's lifetime.
    #[must_use]
    pub fn new(
        info: PaneInfo,
        fingerprint: PaneFingerprint,
        observation: ObservationDecision,
    ) -> Self {
        let now = epoch_ms();
        let domain = info.inferred_domain();
        let pane_uuid = generate_pane_uuid(&domain, info.pane_id, now);

        Self {
            info,
            fingerprint,
            observation,
            pane_uuid,
            first_seen_at: now,
            last_seen_at: now,
            decision_at: now,
            generation: 0,
            is_alt_screen: false,
            last_status_at: None,
            priority_override: None,
        }
    }

    /// Create a pane entry with a specific UUID (for recovery/testing)
    #[must_use]
    pub fn with_uuid(
        info: PaneInfo,
        fingerprint: PaneFingerprint,
        observation: ObservationDecision,
        pane_uuid: String,
    ) -> Self {
        let now = epoch_ms();
        Self {
            info,
            fingerprint,
            observation,
            pane_uuid,
            first_seen_at: now,
            last_seen_at: now,
            decision_at: now,
            generation: 0,
            is_alt_screen: false,
            last_status_at: None,
            priority_override: None,
        }
    }

    /// Update with new pane info (preserves fingerprint and first_seen)
    pub fn update_info(&mut self, info: PaneInfo) {
        self.info = info;
        self.last_seen_at = epoch_ms();
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
    /// Panes with changed metadata (title, cwd, etc.)
    pub changed_panes: Vec<u64>,
    /// Panes whose fingerprint changed (new generation)
    pub new_generations: Vec<u64>,
}

impl DiscoveryDiff {
    /// Check if there are any changes
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.new_panes.is_empty()
            && self.closed_panes.is_empty()
            && self.changed_panes.is_empty()
            && self.new_generations.is_empty()
    }

    /// Total number of changes
    #[must_use]
    pub fn change_count(&self) -> usize {
        self.new_panes.len()
            + self.closed_panes.len()
            + self.changed_panes.len()
            + self.new_generations.len()
    }
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
        }
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
        let actual_transition_occurred = final_state != self.in_alt_screen;

        // Update final state
        self.in_alt_screen = final_state;

        // Save old snapshot for comparison before updating
        let previous_snapshot = std::mem::take(&mut self.last_snapshot);

        let delta = extract_delta(&previous_snapshot, current_snapshot, overlap_size);

        // Update snapshot state regardless; capture is derived from these snapshots.
        self.last_snapshot = current_snapshot.to_string();
        self.last_hash = Some(current_hash);

        // If alt-screen changed, force a gap even if delta extraction succeeded
        // because the content relationship is broken
        if actual_transition_occurred {
            self.in_gap = true;
            let seq = self.next_seq;
            self.next_seq = self.next_seq.saturating_add(1);

            // Determine reason
            let reason = if self.in_alt_screen {
                "alt_screen_entered".to_string()
            } else {
                "alt_screen_exited".to_string()
            };

            // If we have text transitions, prefer their specificity, but if overridden by external,
            // use the external state to decide the reason.

            let content = match delta {
                DeltaResult::Content(c) => c,
                DeltaResult::NoChange => String::new(),
                DeltaResult::Gap { content, .. } => content,
            };

            return Some(CapturedSegment {
                pane_id: self.pane_id,
                seq,
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
                self.next_seq = self.next_seq.saturating_add(1);
                Some(CapturedSegment {
                    pane_id: self.pane_id,
                    seq,
                    content,
                    kind: CapturedSegmentKind::Delta,
                    captured_at: epoch_ms(),
                })
            }
            DeltaResult::Gap { reason, content } => {
                self.in_gap = true;
                let seq = self.next_seq;
                self.next_seq = self.next_seq.saturating_add(1);
                Some(CapturedSegment {
                    pane_id: self.pane_id,
                    seq,
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
    pub fn resync_seq(&mut self, storage_seq: u64) {
        self.next_seq = storage_seq.saturating_add(1);
        self.in_gap = true;
    }

    /// Create a captured delta segment from raw content (native event path).
    ///
    /// This bypasses snapshot-based delta extraction and simply appends the
    /// provided content as a new segment with a monotonically increasing seq.
    pub fn capture_delta(&mut self, content: String, captured_at: i64) -> CapturedSegment {
        self.in_gap = false;
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);

        CapturedSegment {
            pane_id: self.pane_id,
            seq,
            content,
            kind: CapturedSegmentKind::Delta,
            captured_at,
        }
    }

    /// Emit a gap segment with the provided reason.
    pub fn emit_gap(&mut self, reason: &str) -> CapturedSegment {
        self.in_gap = true;
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        CapturedSegment {
            pane_id: self.pane_id,
            seq,
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
    /// Extended pane entries with fingerprints and observation state
    entries: HashMap<u64, PaneEntry>,
    /// Reverse index: pane_uuid -> pane_id
    uuid_index: HashMap<String, u64>,
    /// Cursors for each pane (delta extraction state)
    cursors: HashMap<u64, PaneCursor>,
    /// Pane filter configuration (cached)
    filter_config: PaneFilterConfig,
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
            filter_config: PaneFilterConfig::default(),
        }
    }

    /// Create a registry with filter configuration
    #[must_use]
    pub fn with_filter(filter_config: PaneFilterConfig) -> Self {
        Self {
            entries: HashMap::new(),
            uuid_index: HashMap::new(),
            cursors: HashMap::new(),
            filter_config,
        }
    }

    /// Update the filter configuration
    pub fn set_filter(&mut self, filter_config: PaneFilterConfig) {
        self.filter_config = filter_config;
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
        Ok(())
    }

    /// Remove any expired priority overrides.
    ///
    /// Returns the number of overrides cleared.
    pub fn purge_expired_priority_overrides(&mut self, now_ms: i64) -> usize {
        let mut cleared = 0usize;
        for entry in self.entries.values_mut() {
            let Some(ref ov) = entry.priority_override else {
                continue;
            };
            if ov.expires_at.is_some_and(|exp| exp <= now_ms) {
                entry.priority_override = None;
                cleared = cleared.saturating_add(1);
            }
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
        let mut seen: HashSet<u64> = HashSet::new();

        for pane in panes {
            let pane_id = pane.pane_id;
            seen.insert(pane_id);

            if let Some(entry) = self.entries.get_mut(&pane_id) {
                // Existing pane - check for changes
                let new_fingerprint = PaneFingerprint::without_content(&pane);

                if !entry.fingerprint.is_same_generation(&new_fingerprint) {
                    // Fingerprint changed - new generation
                    diff.new_generations.push(pane_id);
                    entry.fingerprint = new_fingerprint;
                    entry.generation = entry.generation.saturating_add(1);
                    entry.decision_at = epoch_ms();

                    // Reset cursor for new generation - NOT SAFE due to unique constraint on (pane_id, seq)
                    // If we reset cursor, next seq is 0, which likely exists.
                    // Ideally we'd persist generation ID, but schema doesn't support it yet.
                    // For now, we keep the sequence monotonic.
                    // self.cursors.insert(pane_id, PaneCursor::new(pane_id));
                } else if Self::has_metadata_changed(&entry.info, &pane) {
                    // Metadata changed but same generation
                    diff.changed_panes.push(pane_id);
                }

                entry.update_info(pane);
            } else {
                // New pane
                diff.new_panes.push(pane_id);

                let fingerprint = PaneFingerprint::without_content(&pane);
                let observation = self.decide_observation(&pane);

                let entry = PaneEntry::new(pane, fingerprint, observation);
                self.uuid_index.insert(entry.pane_uuid.clone(), pane_id);
                self.entries.insert(pane_id, entry);

                // Only create cursor if observed
                if self
                    .entries
                    .get(&pane_id)
                    .is_some_and(PaneEntry::should_observe)
                {
                    self.cursors.insert(pane_id, PaneCursor::new(pane_id));
                }
            }
        }

        // Find closed panes
        let closed: Vec<u64> = self
            .entries
            .keys()
            .filter(|id| !seen.contains(id))
            .copied()
            .collect();

        for pane_id in &closed {
            diff.closed_panes.push(*pane_id);
            // Remove UUID from index before removing entry
            if let Some(entry) = self.entries.get(pane_id) {
                self.uuid_index.remove(&entry.pane_uuid);
            }
            self.entries.remove(pane_id);
            self.cursors.remove(pane_id);
        }

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

    /// Check if pane metadata (window/tab assignment) has changed.
    ///
    /// Note: Title and cwd changes are handled separately via `is_same_generation()`
    /// which triggers a new generation rather than a metadata change.
    fn has_metadata_changed(old: &PaneInfo, new: &PaneInfo) -> bool {
        old.window_id != new.window_id || old.tab_id != new.tab_id
    }

    /// Get all tracked pane IDs
    #[must_use]
    pub fn pane_ids(&self) -> Vec<u64> {
        self.entries.keys().copied().collect()
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

    /// Get mutable pane entry by ID
    pub fn get_entry_mut(&mut self, pane_id: u64) -> Option<&mut PaneEntry> {
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

    /// Re-evaluate observation decision for a pane (e.g., after filter change)
    pub fn re_evaluate_observation(&mut self, pane_id: u64) {
        // Clone the PaneInfo to avoid borrow conflicts
        let pane_info = match self.entries.get(&pane_id) {
            Some(entry) => entry.info.clone(),
            None => return,
        };

        let new_decision = self.decide_observation(&pane_info);

        if let Some(entry) = self.entries.get_mut(&pane_id) {
            let was_observed = entry.should_observe();
            let is_observed = new_decision.is_observed();

            entry.observation = new_decision;
            entry.decision_at = epoch_ms();

            // Update cursor state
            if is_observed && !was_observed {
                // Now observed - create cursor
                self.cursors.insert(pane_id, PaneCursor::new(pane_id));
            } else if !is_observed && was_observed {
                // Now ignored - remove cursor
                self.cursors.remove(&pane_id);
            }
        }
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
    /// Per-pane monotonic sequence number
    pub seq: u64,
    /// Captured content (delta or full snapshot when `Gap`)
    pub content: String,
    /// Segment kind
    pub kind: CapturedSegmentKind,
    /// Timestamp when the capture was taken (epoch ms)
    pub captured_at: i64,
}

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

/// Persist a captured segment and optional gap into storage.
///
/// The pane must already exist in storage (use `upsert_pane` elsewhere).
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
) -> Result<PersistedCapture> {
    // Record gap if the captured segment itself represents a discontinuity (overlap failure)
    let mut gap = match &captured.kind {
        CapturedSegmentKind::Gap { reason } => storage.record_gap(captured.pane_id, reason).await?,
        CapturedSegmentKind::Delta => None,
    };

    let stored = storage
        .append_segment(captured.pane_id, &captured.content, None)
        .await?;

    // Check for sequence discontinuity between cursor and storage
    if stored.seq != captured.seq {
        // Record gap for the discontinuity (this is in addition to any overlap-failure gap)
        let discontinuity_reason = format!(
            "seq_discontinuity:expected={},actual={}",
            captured.seq, stored.seq
        );
        let discontinuity_gap = storage
            .record_gap(captured.pane_id, &discontinuity_reason)
            .await?;

        // If we didn't already have a gap, use this one; otherwise the overlap gap takes precedence
        if gap.is_none() {
            gap = discontinuity_gap;
        }
    }

    Ok(PersistedCapture {
        segment: stored,
        gap,
    })
}

fn hash_text(text: &str) -> u64 {
    stable_hash(text.as_bytes())
}

/// Extract delta from current vs previous content.
///
/// This is designed for the "sliding window" case (polling successive snapshots):
/// it finds the largest overlap where a suffix of `previous` matches a prefix of `current`.
#[must_use]
pub fn extract_delta(previous: &str, current: &str, overlap_size: usize) -> DeltaResult {
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
    // If boundary check fails (should vary rare if starts_with matched), fall through to full check

    if overlap_size == 0 || current.is_empty() {
        return DeltaResult::Gap {
            reason: "overlap_size_zero_or_current_empty".to_string(),
            content: current.to_string(),
        };
    }

    // Limit overlap search to a bounded suffix/prefix window.
    let max_overlap = overlap_size.min(previous.len()).min(current.len());
    let search_start = previous.len() - max_overlap;
    let search_window = &previous[search_start..];

    // Safety: current is known not to be empty from check above
    let first_char = current.as_bytes()[0];

    // Find all occurrences of first_char in search_window using memchr (SIMD-optimized)
    // We iterate from left to right (smallest pos -> largest overlap)
    for pos in memchr::memchr_iter(first_char, search_window.as_bytes()) {
        // search_window[pos] == first_char
        // Candidate overlap starts at pos relative to search_window
        // Note: search_window.len() == max_overlap (since search_start = previous.len() - max_overlap)
        let overlap_len = search_window.len() - pos;

        if !current.is_char_boundary(overlap_len) {
            continue;
        }

        // Check full match
        // search_window[pos..] has length overlap_len
        // current[..overlap_len] has length overlap_len
        if search_window[pos..] == current[..overlap_len] {
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
        reason: "overlap_not_found".to_string(),
        content: current.to_string(),
    }
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

/// Memory-efficient output cache for skipping redundant processing.
///
/// Uses two complementary mechanisms:
/// 1. Global LRU of content hashes - deduplicates across panes
/// 2. Per-pane rolling hash state - fast per-pane deduplication
#[derive(Debug)]
pub struct OutputCache {
    config: OutputCacheConfig,
    global_hashes: HashMap<u64, i64>,
    /// LRU order tracking - uses VecDeque for O(1) removal from front
    lru_order: VecDeque<u64>,
    pane_states: HashMap<u64, PaneCacheState>,
    hits: u64,
    misses: u64,
}

impl OutputCache {
    /// Create a new output cache with the given configuration.
    #[must_use]
    pub fn new(config: OutputCacheConfig) -> Self {
        Self {
            config,
            global_hashes: HashMap::new(),
            lru_order: VecDeque::new(),
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
        if let Some(state) = self.pane_states.get(&pane_id) {
            if state.content_hash == hash && state.content_len == len {
                self.hits += 1;
                self.pane_states.get_mut(&pane_id).unwrap().last_updated = now;
                return false;
            }
        }

        // Check global LRU (cross-pane deduplication)
        if self.global_hashes.contains_key(&hash) {
            self.update_pane_state(pane_id, hash, len, now);
            self.update_global_lru(hash, now);
            self.hits += 1;
            return false;
        }

        // New content
        self.update_pane_state(pane_id, hash, len, now);
        self.update_global_lru(hash, now);
        self.misses += 1;
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
        if let Entry::Occupied(mut entry) = self.global_hashes.entry(hash) {
            entry.insert(now);
            return;
        }

        // Evict oldest entries if at capacity - O(1) with VecDeque
        while self.lru_order.len() >= self.config.global_lru_capacity {
            if let Some(oldest_hash) = self.lru_order.pop_front() {
                self.global_hashes.remove(&oldest_hash);
            }
        }

        self.global_hashes.insert(hash, now);
        self.lru_order.push_back(hash);
    }

    /// Prune stale per-pane entries older than max_age.
    pub fn prune(&mut self, max_age_ms: u64) {
        let now = epoch_ms();
        let max_age = i64::try_from(max_age_ms).unwrap_or(i64::MAX);
        let cutoff = now.saturating_sub(max_age);

        self.pane_states
            .retain(|_, state| state.last_updated > cutoff);

        let hashes_to_remove: Vec<u64> = self
            .global_hashes
            .iter()
            .filter(|(_, ts)| **ts < cutoff)
            .map(|(hash, _)| *hash)
            .collect();

        for hash in hashes_to_remove {
            self.global_hashes.remove(&hash);
            self.lru_order.retain(|h| *h != hash);
        }
    }

    /// Prune stale entries using the configured max_age.
    pub fn prune_stale(&mut self) {
        self.prune(self.config.per_pane_max_age_ms);
    }

    /// Get the current cache hit rate (0.0 - 1.0).
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
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
/// Reference: <https://gitlab.freedesktop.org/Per_Bothner/specifications/blob/master/proposals/semantic-prompts.md>
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
    let mut i = 0;

    while i < bytes.len() {
        // Look for ESC ] (OSC start)
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b']' {
            // Found OSC start, look for "133;"
            if let Some(marker) = try_parse_osc133(&bytes[i..]) {
                markers.push(marker.0);
                i += marker.1; // Skip past the parsed sequence
                continue;
            }
        }
        i += 1;
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
#[must_use]
#[allow(clippy::items_after_statements)]
pub fn detect_alt_screen_changes(text: &str) -> Vec<AltScreenChange> {
    use memchr::memmem;

    let mut changes = Vec::new();
    let bytes = text.as_bytes();

    // DECSET 1049 - Enable alternate screen (most common)
    // Pattern: ESC [ ? 1049 h
    static ENABLE_1049: &[u8] = b"\x1b[?1049h";
    static DISABLE_1049: &[u8] = b"\x1b[?1049l";

    // DECSET 47 - Older alternate screen
    static ENABLE_47: &[u8] = b"\x1b[?47h";
    static DISABLE_47: &[u8] = b"\x1b[?47l";

    // Find all matches and their positions
    let mut positions: Vec<(usize, AltScreenChange)> = Vec::new();

    for pos in memmem::find_iter(bytes, ENABLE_1049) {
        positions.push((pos, AltScreenChange::Entered));
    }
    for pos in memmem::find_iter(bytes, DISABLE_1049) {
        positions.push((pos, AltScreenChange::Exited));
    }
    for pos in memmem::find_iter(bytes, ENABLE_47) {
        positions.push((pos, AltScreenChange::Entered));
    }
    for pos in memmem::find_iter(bytes, DISABLE_47) {
        positions.push((pos, AltScreenChange::Exited));
    }

    // Sort by position and extract changes in order
    positions.sort_by_key(|(pos, _)| *pos);
    changes.extend(positions.into_iter().map(|(_, change)| change));

    changes
}

/// Check if text contains any alternate screen transitions.
///
/// This is a fast check that can be used before full delta extraction
/// to determine if the content might be from a different screen context.
#[must_use]
pub fn has_alt_screen_change(text: &str) -> bool {
    use memchr::memmem;

    let bytes = text.as_bytes();

    memmem::find(bytes, b"\x1b[?1049h").is_some()
        || memmem::find(bytes, b"\x1b[?1049l").is_some()
        || memmem::find(bytes, b"\x1b[?47h").is_some()
        || memmem::find(bytes, b"\x1b[?47l").is_some()
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverflowPolicy {
    /// Drop the new event and mark the pane as having overflow.
    /// The next delivered event triggers a GAP. This is the safest default:
    /// the sender never blocks, and every drop is accounted for.
    EmitGap,
    /// Remove the oldest event in the channel to make room. Both the dropped
    /// event's pane and the new event's pane are marked as overflow.
    DropOldest,
}

impl Default for OverflowPolicy {
    fn default() -> Self {
        Self::EmitGap
    }
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
    /// Returns a Vec because overflow events produce two segments (GAP + Delta).
    /// Normal deltas produce exactly one segment. PaneClosed and Disconnected
    /// may produce GAP segments for affected panes.
    pub fn process(&mut self, event: StreamEvent) -> Vec<CapturedSegment> {
        match event {
            StreamEvent::OutputData {
                pane_id,
                data,
                received_at,
                overflow,
            } => self.process_output(pane_id, data, received_at, overflow),
            StreamEvent::PaneClosed { pane_id } => self.process_pane_closed(pane_id),
            StreamEvent::Disconnected { reason } => self.process_disconnected(&reason),
        }
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

        let cursor = self
            .cursors
            .entry(pane_id)
            .or_insert_with(|| PaneCursor::new(pane_id));

        // If this pane has pending overflow, emit GAP first
        if self.overflow_pending.remove(&pane_id) {
            let gap = cursor.emit_gap("stream_overflow");
            self.gaps_emitted += 1;
            self.segments_emitted += 1;
            segments.push(gap);
        }

        // Emit the delta segment via PaneCursor (bypasses snapshot diff,
        // since streaming gives us actual deltas directly)
        let seg = cursor.capture_delta(data, received_at);
        self.segments_emitted += 1;
        segments.push(seg);

        segments
    }

    fn process_pane_closed(&mut self, pane_id: u64) -> Vec<CapturedSegment> {
        self.overflow_pending.remove(&pane_id);

        // If we have a cursor for this pane, emit a final gap marking the close
        if let Some(mut cursor) = self.cursors.remove(&pane_id) {
            let gap = cursor.emit_gap("pane_closed");
            self.gaps_emitted += 1;
            self.segments_emitted += 1;
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
            self.gaps_emitted += 1;
            self.segments_emitted += 1;
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
}

impl Default for StreamIngester {
    fn default() -> Self {
        Self::new()
    }
}

/// Simulates a bounded channel with overflow tracking for testing.
///
/// In production, this will be backed by `tokio::sync::mpsc` with
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
    /// Returns `true` if the event was buffered, `false` if it was dropped
    /// (EmitGap policy) or if an older event was evicted (DropOldest policy).
    pub fn send(&mut self, mut event: StreamEvent) -> bool {
        // Tag the event with overflow if this pane had a prior drop
        if let StreamEvent::OutputData {
            pane_id,
            ref mut overflow,
            ..
        } = event
        {
            if self.overflow_panes.remove(&pane_id) {
                *overflow = true;
            }
        }

        if self.buffer.len() < self.capacity {
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
                self.events_dropped += 1;
                false
            }
            OverflowPolicy::DropOldest => {
                // Evict oldest, mark its pane
                if let Some(StreamEvent::OutputData { pane_id, .. }) =
                    self.buffer.pop_front().as_ref()
                {
                    self.overflow_panes.insert(*pane_id);
                }
                // Mark new event's pane if it had prior drops
                if let StreamEvent::OutputData {
                    pane_id,
                    ref mut overflow,
                    ..
                } = event
                {
                    if self.overflow_panes.remove(&pane_id) {
                        *overflow = true;
                    }
                }
                self.buffer.push_back(event);
                self.events_dropped += 1;
                true
            }
        }
    }

    /// Receive the next event from the channel.
    pub fn recv(&mut self) -> Option<StreamEvent> {
        let mut event = self.buffer.pop_front()?;

        // Apply pending overflow flags on receive
        if let StreamEvent::OutputData {
            pane_id,
            ref mut overflow,
            ..
        } = event
        {
            if self.overflow_panes.remove(&pane_id) {
                *overflow = true;
            }
        }

        Some(event)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

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

    #[test]
    fn cursor_starts_at_zero() {
        let cursor = PaneCursor::new(42);
        assert_eq!(cursor.pane_id, 42);
        assert_eq!(cursor.next_seq, 0);
        assert!(!cursor.in_gap);
    }

    #[test]
    fn registry_tracks_panes() {
        let registry = PaneRegistry::new();
        assert!(registry.pane_ids().is_empty());
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

    #[test]
    fn extract_delta_gap_on_in_place_edit() {
        let prev = "hello\nworld\n";
        let cur = "hello\nthere\n";
        let result = extract_delta(prev, cur, 1024);
        assert!(matches!(result, DeltaResult::Gap { .. }));
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

    #[tokio::test]
    async fn persist_captured_segments_appends_rows() {
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

        let stored0 = persist_captured_segment(&handle, &seg0).await.unwrap();
        let stored1 = persist_captured_segment(&handle, &seg1).await.unwrap();

        assert_eq!(stored0.segment.seq, seg0.seq);
        assert_eq!(stored1.segment.seq, seg1.seq);

        let segments = handle.get_segments(1, 10).await.unwrap();
        assert_eq!(segments.len(), 2);
        assert!(segments.iter().any(|seg| seg.content == "hello\n"));
        assert!(segments.iter().any(|seg| seg.content == "world\n"));

        handle.shutdown().await.unwrap();
        cleanup_db(&db_path);
    }

    #[tokio::test]
    async fn persist_captured_gap_records_gap() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();
        handle.upsert_pane(test_pane_record(1)).await.unwrap();

        let mut cursor = PaneCursor::new(1);
        let seg0 = cursor
            .capture_snapshot("a\nb\n", 1024, None)
            .expect("first capture");
        persist_captured_segment(&handle, &seg0).await.unwrap();

        let gap_segment = cursor
            .capture_snapshot("a\nc\n", 1024, None)
            .expect("gap capture");
        let persisted = persist_captured_segment(&handle, &gap_segment)
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
    }

    #[tokio::test]
    async fn persist_captured_segment_records_seq_discontinuity_gap() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();
        handle.upsert_pane(test_pane_record(1)).await.unwrap();

        // First, create a cursor and persist some segments normally
        let mut cursor = PaneCursor::new(1);
        let seg0 = cursor
            .capture_snapshot("line1\n", 1024, None)
            .expect("first capture");
        persist_captured_segment(&handle, &seg0).await.unwrap();

        let seg1 = cursor
            .capture_snapshot("line1\nline2\n", 1024, None)
            .expect("second capture");
        persist_captured_segment(&handle, &seg1).await.unwrap();

        // Now simulate a desync: manually advance the cursor's seq beyond what storage expects
        cursor.next_seq = 100; // Storage expects seq=2, cursor will produce seq=100

        let seg2 = cursor
            .capture_snapshot("line1\nline2\nline3\n", 1024, None)
            .expect("third capture");
        assert_eq!(seg2.seq, 100); // Cursor produced seq=100

        // Persist should NOT error, instead record a gap
        let persisted = persist_captured_segment(&handle, &seg2).await.unwrap();

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
    }

    #[tokio::test]
    async fn resync_seq_aligns_cursor_with_storage() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();
        handle.upsert_pane(test_pane_record(1)).await.unwrap();

        // Create a cursor and persist some segments normally
        let mut cursor = PaneCursor::new(1);
        let seg0 = cursor
            .capture_snapshot("a\n", 1024, None)
            .expect("first capture");
        persist_captured_segment(&handle, &seg0).await.unwrap();

        // Simulate desync
        cursor.next_seq = 999;

        let seg1 = cursor
            .capture_snapshot("a\nb\n", 1024, None)
            .expect("second capture");
        assert_eq!(seg1.seq, 999);

        let persisted = persist_captured_segment(&handle, &seg1).await.unwrap();
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

        let persisted2 = persist_captured_segment(&handle, &seg2).await.unwrap();
        assert_eq!(persisted2.segment.seq, 2);
        // No gap this time since we resynced
        assert!(persisted2.gap.is_none());

        handle.shutdown().await.unwrap();
        cleanup_db(&db_path);
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
    fn fingerprint_creation_and_comparison() {
        let pane = make_pane(1, "vim", Some("/home/user"));

        let fp1 = PaneFingerprint::without_content(&pane);
        let fp2 = PaneFingerprint::without_content(&pane);

        assert_eq!(fp1.initial_title, "vim");
        assert_eq!(fp1.initial_cwd, "/home/user");
        assert!(fp1.is_same_generation(&fp2));

        // Different title = different generation
        let pane2 = make_pane(1, "nano", Some("/home/user"));
        let fp3 = PaneFingerprint::without_content(&pane2);
        assert!(!fp1.is_same_generation(&fp3));
    }

    #[test]
    fn fingerprint_with_content_hash() {
        let pane = make_pane(1, "bash", Some("/tmp"));

        let fp1 = PaneFingerprint::new(&pane, Some("$ echo hello"));
        let fp2 = PaneFingerprint::new(&pane, Some("$ echo world"));

        // Same generation (same title/cwd) but different content hashes
        assert!(fp1.is_same_generation(&fp2));
        assert_ne!(fp1.content_hash, fp2.content_hash);
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
        let fp = PaneFingerprint::without_content(&pane);
        let entry = PaneEntry::new(pane, fp, ObservationDecision::Observed);

        assert_eq!(entry.info.pane_id, 1);
        assert!(entry.should_observe());
        assert_eq!(entry.generation, 0);

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
        assert!(diff.closed_panes.is_empty());
        assert!(diff.changed_panes.is_empty());
        assert!(diff.new_generations.is_empty());

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

        assert!(diff.new_panes.is_empty());
        assert_eq!(diff.closed_panes.len(), 1);
        assert!(diff.closed_panes.contains(&2));

        // Closed panes are removed from entries
        assert_eq!(registry.len(), 1);
        assert!(registry.get_pane(1).is_some());
        assert!(registry.get_pane(2).is_none());
    }

    #[test]
    fn discovery_tick_detects_new_generation_on_title_change() {
        let mut registry = PaneRegistry::new();

        // First tick: pane with title "bash"
        let panes = vec![make_pane(1, "bash", Some("/home"))];
        registry.discovery_tick(panes);
        let entry = registry.entries.get(&1).unwrap();
        assert_eq!(entry.generation, 0);

        // Second tick: same pane, title changed to "vim"
        // This triggers a new generation (fingerprint includes title)
        let panes = vec![make_pane(1, "vim", Some("/home"))];
        let diff = registry.discovery_tick(panes);

        assert!(diff.new_panes.is_empty());
        assert!(diff.closed_panes.is_empty());
        assert!(diff.changed_panes.is_empty());
        assert!(diff.new_generations.contains(&1));

        // Verify info was updated and generation incremented
        let entry = registry.entries.get(&1).unwrap();
        assert_eq!(entry.info.title, Some("vim".to_string()));
        assert_eq!(entry.generation, 1);
    }

    #[test]
    fn discovery_tick_detects_metadata_changes() {
        let mut registry = PaneRegistry::new();

        // First tick: pane in window 1
        let mut pane = make_pane(1, "bash", Some("/home"));
        pane.window_id = 1;
        pane.tab_id = 1;
        registry.discovery_tick(vec![pane]);

        // Second tick: same pane moved to window 2 (metadata change, not new generation)
        let mut pane = make_pane(1, "bash", Some("/home"));
        pane.window_id = 2;
        pane.tab_id = 2;
        let diff = registry.discovery_tick(vec![pane]);

        // Metadata changed (window_id/tab_id) but same generation
        assert!(diff.new_panes.is_empty());
        assert!(diff.closed_panes.is_empty());
        assert!(diff.new_generations.is_empty());
        assert!(diff.changed_panes.contains(&1));

        // Verify window/tab was updated
        let info = registry.get_pane(1).unwrap();
        assert_eq!(info.window_id, 2);
        assert_eq!(info.tab_id, 2);
    }

    #[test]
    fn discovery_tick_cursors_for_observed_panes() {
        let mut registry = PaneRegistry::new();
        let panes = vec![make_pane(1, "bash", Some("/home"))];

        registry.discovery_tick(panes);

        // Observed panes should have cursors
        assert!(registry.get_cursor(1).is_some());
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
    fn pane_entry_to_pane_record_observed() {
        let pane = make_pane(1, "bash", Some("/home/user"));
        let fp = PaneFingerprint::without_content(&pane);
        let entry = PaneEntry::new(pane, fp, ObservationDecision::Observed);

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
        let fp = PaneFingerprint::without_content(&pane);
        let entry = PaneEntry::new(
            pane,
            fp,
            ObservationDecision::Ignored {
                reason: "exclude-vim".to_string(),
            },
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
            .capture_snapshot("$ ls\nfile1\nfile2\n", 1024, None)
            .expect("first capture");
        assert_eq!(seg0.kind, CapturedSegmentKind::Delta);

        // Simulate entering vim (alt screen)
        let seg1 = cursor
            .capture_snapshot("$ ls\nfile1\nfile2\n\x1b[?1049hvim window", 1024, None)
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
            .capture_snapshot("vim content\x1b[?1049l$ ", 1024, None)
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

        // Pane 1 sees content
        assert!(cache.is_new(1, "$ ls\nfile1\nfile2\n"));
        assert!(!cache.is_new(1, "$ ls\nfile1\nfile2\n"));

        // Pane 2 sees same content - should be false (global LRU dedup)
        assert!(!cache.is_new(2, "$ ls\nfile1\nfile2\n"));

        // Pane 1 sees new content
        assert!(cache.is_new(1, "$ ls\nfile1\nfile2\nfile3\n"));

        // Pane 2 still has old state, but global hash exists
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
        assert_eq!(entry.generation, 0);
    }

    #[test]
    fn registry_uuid_stable_across_title_change() {
        let mut reg = PaneRegistry::new();
        let pane = make_pane_info(1, 100, 10);
        reg.discovery_tick(vec![pane]);

        let uuid_before = reg.get_entry(1).unwrap().pane_uuid.clone();

        // Change the title (triggers new generation, but UUID stays)
        let mut changed = make_pane_info(1, 100, 10);
        changed.title = Some("vim".to_string());
        let diff = reg.discovery_tick(vec![changed]);

        assert!(
            diff.new_generations.contains(&1),
            "should be new generation"
        );
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

        // Change the cwd (triggers new generation, but UUID stays)
        let mut changed = make_pane_info(1, 100, 10);
        changed.cwd = Some("/tmp".to_string());
        let diff = reg.discovery_tick(vec![changed]);

        assert!(diff.new_generations.contains(&1));
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

        // Move pane to different tab and window (metadata change, not generation)
        let mut moved = make_pane_info(1, 200, 20);
        moved.title = Some("bash".to_string());
        moved.cwd = Some("/home/user".to_string());
        let diff = reg.discovery_tick(vec![moved]);

        // Should be changed_panes (metadata), not new_generations (same fingerprint)
        assert!(
            diff.changed_panes.contains(&1),
            "should detect metadata change"
        );
        assert!(
            diff.new_generations.is_empty(),
            "same fingerprint = no new generation"
        );
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
    fn registry_generation_increments_on_fingerprint_change() {
        let mut reg = PaneRegistry::new();
        let pane = make_pane_info(1, 100, 10);
        reg.discovery_tick(vec![pane]);
        assert_eq!(reg.get_entry(1).unwrap().generation, 0);

        // Change title → new fingerprint → generation++
        let mut v2 = make_pane_info(1, 100, 10);
        v2.title = Some("vim".to_string());
        reg.discovery_tick(vec![v2]);
        assert_eq!(reg.get_entry(1).unwrap().generation, 1);

        // Change cwd → new fingerprint → generation++
        let mut v3 = make_pane_info(1, 100, 10);
        v3.title = Some("vim".to_string());
        v3.cwd = Some("/tmp".to_string());
        reg.discovery_tick(vec![v3]);
        assert_eq!(reg.get_entry(1).unwrap().generation, 2);
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
    fn fingerprint_same_generation_when_unchanged() {
        let pane = make_pane_info(1, 100, 10);
        let fp1 = PaneFingerprint::without_content(&pane);
        let fp2 = PaneFingerprint::without_content(&pane);
        assert!(fp1.is_same_generation(&fp2));
    }

    #[test]
    fn fingerprint_new_generation_on_title_change() {
        let pane = make_pane_info(1, 100, 10);
        let fp1 = PaneFingerprint::without_content(&pane);

        let mut changed = make_pane_info(1, 100, 10);
        changed.title = Some("ssh session".to_string());
        let fp2 = PaneFingerprint::without_content(&changed);

        assert!(!fp1.is_same_generation(&fp2));
    }

    #[test]
    fn fingerprint_new_generation_on_cwd_change() {
        let pane = make_pane_info(1, 100, 10);
        let fp1 = PaneFingerprint::without_content(&pane);

        let mut changed = make_pane_info(1, 100, 10);
        changed.cwd = Some("/var/log".to_string());
        let fp2 = PaneFingerprint::without_content(&changed);

        assert!(!fp1.is_same_generation(&fp2));
    }

    #[test]
    fn fingerprint_new_generation_on_domain_change() {
        let pane = make_pane_info(1, 100, 10);
        let fp1 = PaneFingerprint::without_content(&pane);

        let mut changed = make_pane_info(1, 100, 10);
        changed.domain_name = Some("SSH:remote.example.com".to_string());
        let fp2 = PaneFingerprint::without_content(&changed);

        assert!(!fp1.is_same_generation(&fp2));
    }

    #[test]
    fn fingerprint_ignores_tab_window_change() {
        // Tab/window moves don't affect fingerprint (only metadata)
        let pane = make_pane_info(1, 100, 10);
        let fp1 = PaneFingerprint::without_content(&pane);

        let moved = make_pane_info(1, 200, 20);
        let fp2 = PaneFingerprint::without_content(&moved);

        assert!(
            fp1.is_same_generation(&fp2),
            "tab/window changes should not create new generation"
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
        assert!(seg.pane_id == 1);
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
        assert_eq!(segs[1].kind, CapturedSegmentKind::Delta);
        assert_eq!(segs[1].seq, 2);
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

        // Next recv for pane 1 should have overflow=true
        let event = ch.recv().unwrap();
        if let StreamEvent::OutputData { overflow, .. } = event {
            assert!(
                overflow,
                "recv should tag overflow on the next event for this pane"
            );
        }
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
        if let StreamEvent::OutputData { data, .. } = event {
            assert_eq!(data, "b");
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

        // Should have GAP(s) + Deltas — verify no silent drops
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
            "overflow must produce at least one GAP segment"
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
            CapturedSegmentKind::Gap { reason } if reason.contains("cancelled")
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
    fn property_every_drop_manifests_as_gap() {
        // For various channel sizes and event counts, verify that every
        // dropped event produces a GAP in the final segment stream.
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
        assert_eq!(json, "\"drop_oldest\"");
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
}
