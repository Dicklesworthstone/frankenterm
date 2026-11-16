//! `frankenterm-mobile-attach`: run over an SSH PTY on a remote machine to
//! attach the phone's terminal to one pane of that machine's running
//! FrankenTerm mux server, the way the desktop GUI attaches.
//!
//! The helper speaks the mux protocol through the vendored
//! `DirectMuxClient` (codec version negotiation included), mirrors one pane
//! onto stdout as ordinary terminal output ([`render`], [`session`]), and
//! turns stdin keystrokes into mux key events so the server encodes them for
//! the pane's own modes.
//!
//! Private messages between the phone app and the helper use DCS strings
//! that terminals ignore:
//!
//! * helper to app: `ESC P ft-mobile;panes;<json> ESC \` with
//!   `{"attached":<id>,"panes":[{"id":..,"title":..,"tab":..,"window":..,
//!   "workspace":..,"active":..,"cols":..,"rows":..}]}` (compact, printable
//!   ASCII; see [`panes`]),
//! * app to helper: `ESC P ft-mobile;attach;<pane_id> ESC \` and
//!   `ESC P ft-mobile;panes ESC \` (see [`control`]).

pub mod control;
pub mod panes;
pub mod render;
#[cfg(unix)]
pub mod session;

use std::path::PathBuf;

/// The mux socket to dial when none is given: the socket the mux exported
/// to its own panes (`FRANKENTERM_UNIX_SOCKET`/`WEZTERM_UNIX_SOCKET`), else
/// the default unix domain's path, `$XDG_RUNTIME_DIR/frankenterm/sock`
/// (`~/.local/share/frankenterm/sock` without a runtime dir).
#[must_use]
pub fn default_socket_path() -> PathBuf {
    for name in ["FRANKENTERM_UNIX_SOCKET", "WEZTERM_UNIX_SOCKET"] {
        if let Some(path) = std::env::var_os(name).filter(|path| !path.is_empty()) {
            return PathBuf::from(path);
        }
    }
    config::UnixDomain::default_unix_domains()
        .first()
        .map_or_else(
            || config::RUNTIME_DIR.join("sock"),
            |domain| domain.socket_path(),
        )
}
