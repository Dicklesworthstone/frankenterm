#![forbid(unsafe_code)]
//! Standalone ownership boundary for native Unix PTYs and their children.
//!
//! This crate is opt-in: adding it to the workspace does not launch it or move
//! existing mux panes into it. The current service retains real PTYs and child
//! handles when the last authenticated connection for a mux incarnation goes
//! away. Raw PTY output is encrypted and synchronized by a fixed bounded worker
//! pool before the readiness loop records a content-free durable receipt or
//! rearms that pane. Live `Input` requests run through a fixed bounded worker:
//! the guardian synchronizes durable intent before granting one PTY-write
//! permit, then synchronizes the terminal disposition before replying. A caller
//! that loses the reply can query the exact effect by its sequence, byte length,
//! and authenticated SHA-256 commitment without retaining plaintext. Output
//! delivery/replay, checkpoint publication, guardian crash recovery, durable
//! same-incarnation WAL reopening, and automated mux migration remain
//! intentionally rejected until their anti-rollback and recovery authorities
//! are integrated. The service can be stopped through an authenticated guarded
//! transaction only while it owns no panes; a successful stop deliberately
//! retains the socket path, so restart remains fail-closed until an explicit
//! non-overwriting retirement design lands.

#[cfg(unix)]
pub(crate) mod output;
#[cfg(unix)]
pub mod runtime;
#[cfg(unix)]
pub mod transport;

#[cfg(unix)]
pub use mux::guardian_protocol::{GuardianInputEffectQuery, InputEffectState};
#[cfg(unix)]
pub use runtime::{GuardianRuntime, GuardianRuntimeConfig, GuardianRuntimeCounters};
#[cfg(unix)]
pub use transport::{
    GuardianClient, GuardianClientError, GuardianProbeReport, GuardianService,
    GuardianServiceConfig, GuardianServiceError, ProvisionTokenOutcome, provision_guardian_token,
};

/// Canonical security-sensitive scratch root for Unix tests.
///
/// Test authority must not depend on an inherited `TMPDIR`: remote builders
/// may point it at a shared, group-writable build directory that production
/// correctly rejects. Canonicalizing `/tmp` also resolves macOS's `/tmp`
/// symlink to `/private/tmp` before descriptor-relative validation.
#[cfg(all(test, unix))]
fn canonical_test_temp_root() -> std::path::PathBuf {
    std::fs::canonicalize("/tmp").expect("canonical system test temp root")
}
