//! Stub types that stand in for mux-lua and url-funcs Lua wrappers.
//!
//! The full mux-lua crate depends on the entire Lua API crate tree
//! (battery, color-funcs, filesystem, logging, plugin, procinfo-funcs,
//! serde-funcs, share-data, spawn-funcs, ssh-funcs, time-funcs, etc.).
//!
//! For the minimal GUI (ft-1memj.3), we only need the newtype wrappers
//! that the termwindow and overlay code use to identify mux objects.
//! Full Lua scripting is deferred to ft-1memj.8 (config) and beyond.

use mux::domain::DomainId;
use mux::pane::PaneId;
use mux::tab::TabId;
use mux::window::WindowId;

/// Newtype wrapper around a mux PaneId.
/// Replaces `mux_lua::MuxPane` without requiring Lua UserData impl.
#[derive(Clone, Copy, Debug)]
pub struct MuxPane(pub PaneId);

/// Newtype wrapper around a mux DomainId.
/// Replaces `mux_lua::MuxDomain`.
#[derive(Clone, Copy, Debug)]
pub struct MuxDomain(pub DomainId);

/// Newtype wrapper around a mux TabId.
/// Replaces `mux_lua::MuxTab`.
#[derive(Clone, Copy, Debug)]
pub struct MuxTab(pub TabId);

/// Newtype wrapper around a mux WindowId.
/// Replaces `mux_lua::MuxWindow`.
#[derive(Clone, Copy, Debug)]
pub struct MuxWindow(pub WindowId);

/// Stub for `url_funcs::Url`.
/// The real type wraps `url::Url` with Lua serialization support.
#[derive(Clone, Debug)]
pub struct Url {
    pub url: url::Url,
}
