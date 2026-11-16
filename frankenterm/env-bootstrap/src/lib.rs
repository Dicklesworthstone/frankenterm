//! Environment bootstrap for FrankenTerm binaries.
//!
//! Provides early-startup initialization: logging setup (via `env_logger`),
//! version metadata registration, and environment sanitization. Designed to
//! be called once from `main()` before any async runtime or worker threads
//! are spawned.
//!
//! Binary crates that configure their own logging (e.g., via
//! `frankenterm_core::logging::init_logging`) can skip `setup_logger()` —
//! the idempotent guard ensures no conflict if both are called.

pub mod ringlog;
pub use ringlog::setup_logger;

/// Bootstrap the runtime environment.
///
/// Performs three initialization steps in order:
/// 1. Registers version metadata with the `config` crate.
/// 2. Initializes `env_logger` for stderr logging (respects `RUST_LOG`).
/// 3. Removes inherited environment variables that confuse terminal emulators.
///
/// Safe to call multiple times — logging init and version registration are
/// idempotent, and the env-var cleanup is harmless on repeated runs.
///
/// # Safety
///
/// Must be called from the main thread before spawning worker threads.
/// The `unsafe` blocks remove environment variables, which is safe when
/// no other threads are reading the environment concurrently.
pub fn bootstrap() {
    config::assign_version_info(env!("CARGO_PKG_VERSION"), std::env::consts::ARCH);
    setup_logger();

    // Remove inherited env vars that confuse terminal emulators.
    // SAFETY: Called from main thread before any threads are spawned.
    unsafe {
        std::env::remove_var("WINDOWID");
        std::env::remove_var("VTE_VERSION");
        std::env::remove_var("SHELL");
    }
}
