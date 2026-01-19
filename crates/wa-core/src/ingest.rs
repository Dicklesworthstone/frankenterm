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

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::PaneFilterConfig;
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
// Fingerprinting
// =============================================================================

/// A fingerprint uniquely identifies a pane "generation" across restarts.
///
/// Components:
/// - domain name
/// - title + cwd (at first observation)
/// - hash of initial content (first N lines)
///
/// When a pane's fingerprint changes significantly, it may indicate:
/// - A new shell session in the same pane ID
/// - Connection to a different host
/// - Substantial context switch
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PaneFingerprint {
    /// Domain name (e.g., "local", "SSH:hostname")
    pub domain: String,
    /// Initial title when first observed
    pub initial_title: String,
    /// Initial cwd when first observed
    pub initial_cwd: String,
    /// Hash of initial content (first ~50 lines)
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
    /// First seen timestamp (epoch ms)
    pub first_seen_at: i64,
    /// Last seen timestamp (epoch ms)
    pub last_seen_at: i64,
    /// When observation decision was made (epoch ms)
    pub decision_at: i64,
    /// Generation number (increments when fingerprint changes)
    pub generation: u32,
}

impl PaneEntry {
    /// Create a new pane entry
    #[must_use]
    pub fn new(
        info: PaneInfo,
        fingerprint: PaneFingerprint,
        observation: ObservationDecision,
    ) -> Self {
        let now = epoch_ms();
        Self {
            info,
            fingerprint,
            observation,
            first_seen_at: now,
            last_seen_at: now,
            decision_at: now,
            generation: 0,
        }
    }

    /// Update with new pane info (preserves fingerprint and first_seen)
    pub fn update_info(&mut self, info: PaneInfo) {
        self.info = info;
        self.last_seen_at = epoch_ms();
    }

    /// Check if this pane should be observed
    #[must_use]
    pub fn should_observe(&self) -> bool {
        self.observation.is_observed()
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
        }
    }

    /// Process a new pane snapshot and return a captured segment if something changed.
    ///
    /// This assigns a monotonically increasing per-pane sequence number (`seq`).
    pub fn capture_snapshot(
        &mut self,
        current_snapshot: &str,
        overlap_size: usize,
    ) -> Option<CapturedSegment> {
        if current_snapshot == self.last_snapshot {
            return None;
        }

        let current_hash = hash_text(current_snapshot);

        let delta = extract_delta(&self.last_snapshot, current_snapshot, overlap_size);

        // Update snapshot state regardless; capture is derived from these snapshots.
        self.last_snapshot = current_snapshot.to_string();
        self.last_hash = Some(current_hash);

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
                })
            }
        }
    }
}

/// Pane registry for tracking discovered panes with lifecycle management
pub struct PaneRegistry {
    /// Extended pane entries with fingerprints and observation state
    entries: HashMap<u64, PaneEntry>,
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
            cursors: HashMap::new(),
            filter_config: PaneFilterConfig::default(),
        }
    }

    /// Create a registry with filter configuration
    #[must_use]
    pub fn with_filter(filter_config: PaneFilterConfig) -> Self {
        Self {
            entries: HashMap::new(),
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

    /// Check if pane metadata has changed
    fn has_metadata_changed(old: &PaneInfo, new: &PaneInfo) -> bool {
        old.title != new.title
            || old.cwd != new.cwd
            || old.window_id != new.window_id
            || old.tab_id != new.tab_id
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapturedSegmentKind {
    /// Delta extracted from overlap
    Delta,
    /// Full snapshot emitted due to discontinuity
    Gap { reason: String },
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
