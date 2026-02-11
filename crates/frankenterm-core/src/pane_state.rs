//! Pane state management for session persistence.
//!
//! Provides serialization and deserialization of per-pane terminal state
//! (cursor position, attributes, alt-screen, scrollback references) for
//! crash-resilient session persistence via `mux_pane_state` storage.
//!
//! See `wa-2l27x` epic for the full session persistence design.

use serde::{Deserialize, Serialize};

/// Snapshot of a single pane's terminal state at checkpoint time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneStateSnapshot {
    /// WezTerm pane ID at capture time.
    pub pane_id: u64,
    /// Current working directory (best-effort).
    pub cwd: Option<String>,
    /// Best-effort process name.
    pub command: Option<String>,
    /// Cursor row position.
    pub cursor_row: u32,
    /// Cursor column position.
    pub cursor_col: u32,
    /// Whether the pane is in alt-screen mode.
    pub alt_screen_active: bool,
    /// Scrollback sequence reference for replay.
    pub scrollback_checkpoint_seq: Option<i64>,
    /// Epoch ms of last captured output.
    pub last_output_at: Option<i64>,
}

impl PaneStateSnapshot {
    /// Create a minimal snapshot with just pane ID.
    #[must_use]
    pub fn new(pane_id: u64) -> Self {
        Self {
            pane_id,
            cwd: None,
            command: None,
            cursor_row: 0,
            cursor_col: 0,
            alt_screen_active: false,
            scrollback_checkpoint_seq: None,
            last_output_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_new_defaults() {
        let snap = PaneStateSnapshot::new(42);
        assert_eq!(snap.pane_id, 42);
        assert!(!snap.alt_screen_active);
        assert!(snap.cwd.is_none());
    }

    #[test]
    fn snapshot_serializes_roundtrip() {
        let snap = PaneStateSnapshot {
            pane_id: 7,
            cwd: Some("/home/user".to_string()),
            command: Some("claude-code".to_string()),
            cursor_row: 24,
            cursor_col: 80,
            alt_screen_active: true,
            scrollback_checkpoint_seq: Some(1234),
            last_output_at: Some(1_700_000_000_000),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let deser: PaneStateSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.pane_id, 7);
        assert_eq!(deser.cwd.as_deref(), Some("/home/user"));
        assert!(deser.alt_screen_active);
    }
}
