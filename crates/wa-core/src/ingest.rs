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

use std::collections::{HashMap, HashSet, hash_map::Entry};
use std::hash::{Hash, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use rand::Rng;
use sha2::{Digest, Sha256};

use crate::config::PaneFilterConfig;
use crate::error::Result;
use crate::storage::{Gap, PaneRecord, Segment, StorageHandle};
use crate::wezterm::PaneInfo;

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
/// - Is idempotent: calling with same inputs produces same output
/// - Is bounded: computed once at pane discovery, never updated
/// - Is safe: purely read-based, no writes to WezTerm
/// - Is auditable: deterministic from inputs
#[must_use]
pub fn generate_pane_uuid(domain: &str, pane_id: u64, created_at: i64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update(pane_id.to_le_bytes());
    hasher.update(created_at.to_le_bytes());

    // Add random entropy to ensure uniqueness even if same pane_id reappears
    let entropy: [u8; 8] = rand::thread_rng().r#gen();
    hasher.update(entropy);

    let hash = hasher.finalize();

    // Take first 16 bytes and encode as lowercase hex (32 chars)
    hex_encode(&hash[..16])
}

/// Encode bytes as lowercase hex string
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
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
    /// Whether pane is in alternate screen buffer (from Lua status updates)
    pub is_alt_screen: bool,
    /// Timestamp of last status update (epoch ms)
    pub last_status_at: Option<i64>,
}

impl PaneEntry {
    /// Create a new pane entry
    ///
    /// Generates a stable `pane_uuid` based on domain, pane_id, and creation time.
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
        }
    }

    /// Update with new pane info (preserves fingerprint and first_seen)
    pub fn update_info(&mut self, info: PaneInfo) {
        self.info = info;
        self.last_seen_at = epoch_ms();
    }

    /// Update from a status update (from Lua hooks)
    ///
    /// Updates title, dimensions, cursor, and alt-screen state from the IPC payload.
    /// Returns whether the alt-screen state changed.
    pub fn update_from_status(
        &mut self,
        title: Option<String>,
        dimensions: Option<(u32, u32)>,
        cursor: Option<(u32, u32)>,
        is_alt_screen: bool,
        ts: i64,
    ) -> bool {
        let alt_changed = self.is_alt_screen != is_alt_screen;

        // Update title if provided
        if let Some(new_title) = title {
            self.info.title = Some(new_title);
        }

        // Update dimensions if provided
        if let Some((cols, rows)) = dimensions {
            self.info.cols = Some(cols);
            self.info.rows = Some(rows);
        }

        // Update cursor if provided
        if let Some((col, row)) = cursor {
            self.info.cursor_x = Some(col);
            self.info.cursor_y = Some(row);
        }

        self.is_alt_screen = is_alt_screen;
        self.last_status_at = Some(ts);
        self.last_seen_at = epoch_ms();

        alt_changed
    }

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
    pub fn capture_snapshot(
        &mut self,
        current_snapshot: &str,
        overlap_size: usize,
    ) -> Option<CapturedSegment> {
        if current_snapshot == self.last_snapshot {
            return None;
        }

        let current_hash = hash_text(current_snapshot);

        // Check for alt-screen changes before delta extraction
        let alt_screen_changes = detect_alt_screen_changes(current_snapshot);
        let had_alt_screen_change = !alt_screen_changes.is_empty();

        // Update alt-screen tracking based on final state
        for change in &alt_screen_changes {
            match change {
                AltScreenChange::Entered => self.in_alt_screen = true,
                AltScreenChange::Exited => self.in_alt_screen = false,
            }
        }

        let delta = extract_delta(&self.last_snapshot, current_snapshot, overlap_size);

        // Update snapshot state regardless; capture is derived from these snapshots.
        self.last_snapshot = current_snapshot.to_string();
        self.last_hash = Some(current_hash);

        // If alt-screen changed, force a gap even if delta extraction succeeded
        // because the content relationship is broken
        if had_alt_screen_change {
            self.in_gap = true;
            let seq = self.next_seq;
            self.next_seq = self.next_seq.saturating_add(1);

            let reason = if alt_screen_changes
                .last()
                .is_some_and(|c| *c == AltScreenChange::Entered)
            {
                "alt_screen_entered".to_string()
            } else {
                "alt_screen_exited".to_string()
            };

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

    /// Alias for `capture_snapshot` for backward compatibility.
    pub fn capture(&mut self, content: &str, overlap_size: usize) -> Option<CapturedSegment> {
        self.capture_snapshot(content, overlap_size)
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

                    // Reset cursor for new generation
                    self.cursors.insert(pane_id, PaneCursor::new(pane_id));
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

    /// Update pane state from a status update (from Lua IPC hooks).
    ///
    /// This updates the in-memory pane entry with title, dimensions, cursor,
    /// and alt-screen state from the status update payload.
    ///
    /// Returns `Some(alt_changed)` if the pane was updated, or `None` if:
    /// - The pane is unknown (not tracked)
    /// - The pane is ignored (not observed)
    ///
    /// # Arguments
    /// * `pane_id` - The pane ID to update
    /// * `title` - New title (if provided)
    /// * `dimensions` - New (cols, rows) (if provided)
    /// * `cursor` - New (col, row) cursor position (if provided)
    /// * `is_alt_screen` - Whether pane is in alt-screen mode
    /// * `ts` - Timestamp of the status update (epoch ms)
    pub fn update_from_status(
        &mut self,
        pane_id: u64,
        title: Option<String>,
        dimensions: Option<(u32, u32)>,
        cursor: Option<(u32, u32)>,
        is_alt_screen: bool,
        ts: i64,
    ) -> Option<bool> {
        let entry = self.entries.get_mut(&pane_id)?;

        // Don't update ignored panes
        if !entry.should_observe() {
            return None;
        }

        Some(entry.update_from_status(title, dimensions, cursor, is_alt_screen, ts))
    }

    /// Get the alt-screen state for a pane
    #[must_use]
    pub fn is_alt_screen(&self, pane_id: u64) -> Option<bool> {
        self.entries.get(&pane_id).map(|e| e.is_alt_screen)
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
        CapturedSegmentKind::Gap { reason } => {
            Some(storage.record_gap(captured.pane_id, reason).await?)
        }
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
            gap = Some(discontinuity_gap);
        }
    }

    Ok(PersistedCapture {
        segment: stored,
        gap,
    })
}

fn hash_text(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
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

    if overlap_size == 0 || current.is_empty() {
        return DeltaResult::Gap {
            reason: "overlap_size_zero_or_current_empty".to_string(),
            content: current.to_string(),
        };
    }

    // Limit overlap search to a bounded suffix/prefix window.
    let max_overlap = overlap_size.min(previous.len()).min(current.len());

    for overlap_len in (1..=max_overlap).rev() {
        let prev_start = previous.len() - overlap_len;
        if !previous.is_char_boundary(prev_start) || !current.is_char_boundary(overlap_len) {
            continue;
        }

        if previous[prev_start..] == current[..overlap_len] {
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
    lru_order: Vec<u64>,
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
            lru_order: Vec::new(),
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

        while self.lru_order.len() >= self.config.global_lru_capacity {
            if let Some(oldest_hash) = self.lru_order.first().copied() {
                self.lru_order.remove(0);
                self.global_hashes.remove(&oldest_hash);
            }
        }

        self.global_hashes.insert(hash, now);
        self.lru_order.push(hash);
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

        let seg0 = cursor.capture_snapshot("a\n", 1024).expect("first capture");
        assert_eq!(seg0.seq, 0);
        assert_eq!(seg0.pane_id, 7);
        assert_eq!(seg0.kind, CapturedSegmentKind::Delta);
        assert_eq!(seg0.content, "a\n");

        let seg1 = cursor
            .capture_snapshot("a\nb\n", 1024)
            .expect("second capture");
        assert_eq!(seg1.seq, 1);
        assert_eq!(seg1.kind, CapturedSegmentKind::Delta);
        assert_eq!(seg1.content, "b\n");

        // No change shouldn't emit a segment or advance seq
        assert!(cursor.capture_snapshot("a\nb\n", 1024).is_none());
        assert_eq!(cursor.next_seq, 2);

        // In-place edit triggers a gap segment with full snapshot content
        let seg2 = cursor
            .capture_snapshot("a\nc\n", 1024)
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
            .capture_snapshot("hello\n", 1024)
            .expect("first capture");
        let seg1 = cursor
            .capture_snapshot("hello\nworld\n", 1024)
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
            .capture_snapshot("a\nb\n", 1024)
            .expect("first capture");
        persist_captured_segment(&handle, &seg0).await.unwrap();

        let gap_segment = cursor
            .capture_snapshot("a\nc\n", 1024)
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
            .capture_snapshot("line1\n", 1024)
            .expect("first capture");
        persist_captured_segment(&handle, &seg0).await.unwrap();

        let seg1 = cursor
            .capture_snapshot("line1\nline2\n", 1024)
            .expect("second capture");
        persist_captured_segment(&handle, &seg1).await.unwrap();

        // Now simulate a desync: manually advance the cursor's seq beyond what storage expects
        cursor.next_seq = 100; // Storage expects seq=2, cursor will produce seq=100

        let seg2 = cursor
            .capture_snapshot("line1\nline2\nline3\n", 1024)
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
        let seg0 = cursor.capture_snapshot("a\n", 1024).expect("first capture");
        persist_captured_segment(&handle, &seg0).await.unwrap();

        // Simulate desync
        cursor.next_seq = 999;

        let seg1 = cursor
            .capture_snapshot("a\nb\n", 1024)
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
            .capture_snapshot("a\nb\nc\n", 1024)
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
            .capture_snapshot("$ ls\nfile1\nfile2\n", 1024)
            .expect("first capture");
        assert_eq!(seg0.kind, CapturedSegmentKind::Delta);

        // Simulate entering vim (alt screen)
        let seg1 = cursor
            .capture_snapshot("$ ls\nfile1\nfile2\n\x1b[?1049hvim window", 1024)
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
            .capture_snapshot("vim content", 1024)
            .expect("first capture in alt screen");

        // Exit vim (alt screen exit)
        let seg1 = cursor
            .capture_snapshot("vim content\x1b[?1049l$ ", 1024)
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
        cursor.capture_snapshot("\x1b[?1049hcontent", 1024);
        assert!(cursor.in_alt_screen);

        // Still in alt screen
        cursor.capture_snapshot("\x1b[?1049hcontent update", 1024);
        assert!(cursor.in_alt_screen);

        // Exit alt screen
        cursor.capture_snapshot("\x1b[?1049hcontent update\x1b[?1049l$ prompt", 1024);
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
}
