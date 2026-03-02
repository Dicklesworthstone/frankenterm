//! Minimal env-bootstrap stub for FrankenTerm GUI.
//!
//! The full WezTerm env-bootstrap registers many Lua modules and does
//! platform-specific fixups. This stub provides the same API surface
//! but skips Lua registration (Lua is optional in FrankenTerm).

pub mod ringlog;
pub use ringlog::setup_logger;

/// Bootstrap the runtime environment.
///
/// In WezTerm this registers Lua modules, sets up logging, fixes
/// Snap/AppImage environment, etc. In FrankenTerm we do only the
/// minimal setup needed for the GUI to function.
pub fn bootstrap() {
    config::assign_version_info(env!("CARGO_PKG_VERSION"), std::env::consts::ARCH);
    setup_logger();

    // Remove inherited env vars that confuse terminal emulators
    // SAFETY: These are env var removals, not UB-triggering.
    // In Rust 2024 set_var/remove_var are unsafe but these are
    // called from main thread before any threads are spawned.
    unsafe {
        std::env::remove_var("WINDOWID");
        std::env::remove_var("VTE_VERSION");
        std::env::remove_var("SHELL");
    }
}
