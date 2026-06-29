//! Per-workspace persistence of the GUI window maximize/fullscreen state.
//!
//! FrankenTerm did not previously remember whether a window was maximized or
//! full-screened: maximize, quit, relaunch, and the window came back at its
//! default size. This module persists just those two bits, keyed by the
//! window's *workspace* name — the only key that is stable across restarts
//! (the mux window id is a per-process atomic counter, not stable).
//!
//! The state lives in a small JSON map at `config::DATA_DIR/window-state.json`,
//! mirroring the `recent-commands.json` persistence in `termwindow::palette`.
//! Everything here is best-effort: a missing, unreadable, or unparseable file
//! is treated as "no saved state" and never panics or blocks the GUI.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use window::WindowState;

/// The persisted maximize/fullscreen state for a single workspace.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default)]
pub struct PersistedWindowState {
    #[serde(default)]
    pub maximized: bool,
    #[serde(default)]
    pub fullscreen: bool,
}

fn state_file_name() -> PathBuf {
    config::DATA_DIR.join("window-state.json")
}

/// Load the whole `workspace -> state` map. A missing or unparseable file
/// yields an empty map (logged at debug); we never propagate the error so
/// the default/non-persisted launch path behaves exactly as before.
fn load_map() -> HashMap<String, PersistedWindowState> {
    let file_name = state_file_name();
    let f = match std::fs::File::open(&file_name) {
        Ok(f) => f,
        Err(err) => {
            log::debug!("window-state: not loading {file_name:?}: {err}");
            return HashMap::new();
        }
    };
    match serde_json::from_reader(f) {
        Ok(map) => map,
        Err(err) => {
            log::debug!("window-state: {file_name:?} is unparseable ({err}); ignoring");
            HashMap::new()
        }
    }
}

/// The saved maximize/fullscreen state for `workspace`, if any.
pub fn load_for_workspace(workspace: &str) -> Option<PersistedWindowState> {
    load_map().get(workspace).copied()
}

/// Persist the maximize/fullscreen subset of `window_state` for `workspace`,
/// preserving any other workspaces already on disk (read-modify-write).
/// Best-effort: any failure is logged at debug and swallowed.
pub fn save_for_workspace(workspace: &str, window_state: WindowState) {
    let entry = PersistedWindowState {
        maximized: window_state.contains(WindowState::MAXIMIZED),
        fullscreen: window_state.contains(WindowState::FULL_SCREEN),
    };

    let mut map = load_map();
    map.insert(workspace.to_string(), entry);

    let json = match serde_json::to_string(&map) {
        Ok(json) => json,
        Err(err) => {
            log::debug!("window-state: failed to serialize state: {err}");
            return;
        }
    };
    if let Err(err) = std::fs::write(state_file_name(), json) {
        log::debug!("window-state: failed to write state file: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn map_roundtrips_through_json() {
        let mut map = HashMap::new();
        map.insert(
            "default".to_string(),
            PersistedWindowState {
                maximized: true,
                fullscreen: false,
            },
        );
        let json = serde_json::to_string(&map).unwrap();
        let back: HashMap<String, PersistedWindowState> = serde_json::from_str(&json).unwrap();
        assert!(back["default"].maximized);
        assert!(!back["default"].fullscreen);
    }

    #[test]
    fn unparseable_json_is_empty_not_panic() {
        let back: Result<HashMap<String, PersistedWindowState>, _> =
            serde_json::from_str("not json");
        assert!(back.is_err());
    }
}
