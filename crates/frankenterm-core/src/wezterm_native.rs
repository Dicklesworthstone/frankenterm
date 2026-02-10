//! Vendored WezTerm native integration types.
//!
//! This module defines the trait contract that a vendored WezTerm build can use
//! to emit native events directly to wa without Lua hooks.

#![forbid(unsafe_code)]

/// Trait for receiving native events from WezTerm.
///
/// Implementations must be non-blocking and thread-safe. WezTerm will call
/// these methods from multiple threads.
pub trait WaEventSink: Send + Sync + 'static {
    /// Called when new output is received for a pane.
    fn on_pane_output(&self, pane_id: u64, data: &[u8]);

    /// Called when pane state changes (title, dimensions, alt-screen, cursor).
    fn on_pane_state_change(&self, pane_id: u64, state: &WaPaneState);

    /// Called when a user-var (OSC 1337) is set.
    fn on_user_var_changed(&self, pane_id: u64, name: &str, value: &str);

    /// Called when a new pane is created.
    fn on_pane_created(&self, pane_id: u64, domain: &str, cwd: Option<&str>);

    /// Called when a pane is destroyed.
    fn on_pane_destroyed(&self, pane_id: u64);
}

/// Snapshot of pane state for state change events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaPaneState {
    pub title: String,
    pub rows: u16,
    pub cols: u16,
    pub is_alt_screen: bool,
    pub cursor_row: u32,
    pub cursor_col: u32,
}
