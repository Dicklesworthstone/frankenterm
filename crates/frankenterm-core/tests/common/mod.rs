//! Shared test infrastructure for frankenterm-core integration tests.
//!
//! ## Unified harness (ft-e34d9.10.6.5)
//!
//! The `reason_codes` and `test_event_logger` modules implement the ADR-0012
//! structured logging contract at the Rust level, giving unit/integration
//! tests the same evidence format as e2e shell scripts.
//!
//! ```ignore
//! mod common;
//! use common::test_event_logger::{TestEventLogger, ScenarioRunner};
//! use common::reason_codes::{Outcome, ReasonCode, ErrorCode};
//! ```
//!
//! ## LabRuntime helpers (asupersync-runtime feature)
//!
//! ```ignore
//! mod common;
//! use common::lab;
//! use common::fixtures;
//! ```

// -- Always-available modules (no feature gate) --

/// Structured reason/error code taxonomy for test evidence.
pub mod reason_codes;

/// Structured test event logger matching the ADR-0012 contract.
pub mod test_event_logger;

/// Hermetic `wezterm-mux-server` subprocess fixture for no-mocks integration
/// tests (ft-dvgzi, ft-2funa).
pub mod wezterm_subprocess;

/// Hermetic local HTTP server fixture for no-mocks webhook integration
/// tests (ft-geg03).
pub mod local_http_server;

// -- Feature-gated modules --

#[cfg(feature = "asupersync-runtime")]
pub mod lab;

#[cfg(feature = "asupersync-runtime")]
pub mod fixtures;

/// `asupersync_test!` declarative macro (ft-i2eni.2). Wraps an
/// async-bodied test in `RuntimeFixture::current_thread().block_on(...)`
/// without per-call boilerplate. See `asupersync_test.rs` for usage.
///
/// The macro itself is feature-agnostic at definition time — the
/// `RuntimeFixture` path in its expansion is resolved at the call
/// site, where `asupersync-runtime` is already required.
#[cfg(feature = "asupersync-runtime")]
pub mod asupersync_test;
