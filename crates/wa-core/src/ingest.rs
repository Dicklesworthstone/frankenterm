//! Ingest pipeline for pane output capture
//!
//! Handles delta extraction, sequence numbering, and gap detection.

use crate::wezterm::PaneInfo;

/// Per-pane state for tracking capture position
#[derive(Debug, Clone)]
pub struct PaneCursor {
    /// Pane ID
    pub pane_id: u64,
    /// Last captured sequence number
    pub last_seq: u64,
    /// Hash of last captured content (for overlap detection)
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
            last_seq: 0,
            last_hash: None,
            in_gap: false,
        }
    }
}

/// Pane registry for tracking discovered panes
pub struct PaneRegistry {
    /// Known panes
    panes: std::collections::HashMap<u64, PaneInfo>,
    /// Cursors for each pane
    cursors: std::collections::HashMap<u64, PaneCursor>,
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
            panes: std::collections::HashMap::new(),
            cursors: std::collections::HashMap::new(),
        }
    }

    /// Update the registry with new pane information
    pub fn update(&mut self, panes: Vec<PaneInfo>) {
        // Track which panes we've seen
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();

        for pane in panes {
            seen.insert(pane.pane_id);

            // Create cursor if new pane
            self.cursors
                .entry(pane.pane_id)
                .or_insert_with(|| PaneCursor::new(pane.pane_id));

            // Update pane info
            self.panes.insert(pane.pane_id, pane);
        }

        // TODO: Handle disappeared panes
    }

    /// Get all tracked pane IDs
    #[must_use]
    pub fn pane_ids(&self) -> Vec<u64> {
        self.panes.keys().copied().collect()
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
}

/// Delta extraction result
#[derive(Debug)]
pub enum DeltaResult {
    /// New content extracted
    Content(String),
    /// No new content
    NoChange,
    /// Gap detected - overlap failed
    Gap { reason: String },
}

/// Extract delta from current vs previous content
#[must_use]
pub fn extract_delta(_previous: &str, _current: &str, _overlap_size: usize) -> DeltaResult {
    // TODO: Implement overlap-based delta extraction
    DeltaResult::NoChange
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_starts_at_zero() {
        let cursor = PaneCursor::new(42);
        assert_eq!(cursor.pane_id, 42);
        assert_eq!(cursor.last_seq, 0);
        assert!(!cursor.in_gap);
    }

    #[test]
    fn registry_tracks_panes() {
        let registry = PaneRegistry::new();
        assert!(registry.pane_ids().is_empty());
    }
}
