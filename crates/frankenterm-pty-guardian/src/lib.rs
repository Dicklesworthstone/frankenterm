#![forbid(unsafe_code)]
//! Standalone ownership boundary for native Unix PTYs and their children.
//!
//! This crate is opt-in: adding it to the workspace does not launch it or move
//! existing mux panes into it. The current service retains real PTYs and child
//! handles when the last authenticated connection for a mux incarnation goes
//! away. Raw PTY output is encrypted and synchronized by a fixed bounded worker
//! pool before the readiness loop records a content-free durable receipt or
//! rearms that pane. Durable input, output delivery/replay, checkpoint
//! publication, guardian crash recovery, and automated mux migration are
//! intentionally rejected until their remaining durable mechanisms are
//! integrated. The service can be stopped through an authenticated guarded
//! transaction only while it owns no panes; a successful stop deliberately
//! retains the socket path, so restart remains fail-closed until an explicit
//! non-overwriting retirement design lands.

#[cfg(unix)]
mod output;
#[cfg(unix)]
pub mod runtime;
#[cfg(unix)]
pub mod transport;

#[cfg(unix)]
pub use runtime::{GuardianRuntime, GuardianRuntimeConfig, GuardianRuntimeCounters};
#[cfg(unix)]
pub use transport::{
    GuardianClient, GuardianClientError, GuardianProbeReport, GuardianService,
    GuardianServiceConfig, GuardianServiceError, ProvisionTokenOutcome,
    provision_guardian_token,
};
