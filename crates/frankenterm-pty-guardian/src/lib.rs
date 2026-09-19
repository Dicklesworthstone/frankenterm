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
//! and authenticated SHA-256 commitment without retaining plaintext.
//! Authenticated output replay and checkpoint Stage/catalog adoption are live
//! behind typed, identity-bound transport operations. With an explicitly
//! configured broker endpoint, the service publishes the authenticated Genesis
//! model and prepares its encrypted journals before broker Spawn, then requires
//! durable custody and real I/O handles before activating the pane. Replay
//! binds that model to its original journal chain. Explicit custody-backed
//! first-generation mux transfer preserves the live broker child within one
//! sealed build. Automatic mux selection, topology publication, cross-build
//! upgrades and guardian restart recovery still require integration; existing
//! mux panes are never migrated implicitly.
//! The separately supervised broker exposes a content-free, paginated view of
//! authenticated recovered Spawn journals. A live pre-acknowledgement
//! Spawn also mints one plaintext recovery capability while persisting only its
//! pane-bound verifier; recovered-journal Census entries still grant no PTY,
//! lease, output-replay, or mutation authority. The service can be stopped
//! through an authenticated guarded
//! transaction only while it owns no panes; a successful stop deliberately
//! retains the socket path, so restart remains fail-closed until an explicit
//! non-overwriting retirement design lands. Successor Claim/Query/Ack lease
//! transitions synchronize through the authenticated WAL before activation;
//! a failed or ambiguous append quarantines effects while retaining the child.

pub use frankenterm_build_identity::{
    AtomicBuildIdentity, AtomicComponentIdentityError, SealedAtomicBuildIdentity,
};

const GUARDIAN_COMPILED_BUILD_IDENTITY: &str = env!("FT_GUARDIAN_COMPILED_BUILD_IDENTITY");

/// Validate this library's compile-time family identity while preserving
/// explicit development state. The build script derives this value from the
/// same validated identity as the guardian executable's process marker.
/// Linking the library into another process must not embed a guardian process
/// marker there. `UnsealedDevelopment` never grants runtime build authority.
pub fn guardian_embedded_build_identity()
-> Result<AtomicBuildIdentity, AtomicComponentIdentityError> {
    if GUARDIAN_COMPILED_BUILD_IDENTITY == frankenterm_build_identity::UNSEALED_BUILD_ID {
        Ok(AtomicBuildIdentity::UnsealedDevelopment)
    } else {
        SealedAtomicBuildIdentity::from_lower_hex(GUARDIAN_COMPILED_BUILD_IDENTITY)
            .map(AtomicBuildIdentity::Sealed)
    }
}

/// Return the exact decoded 32-byte family authority compiled into this library.
///
/// An ordinary development build returns
/// [`AtomicComponentIdentityError::UnsealedDevelopmentBuild`]. The function
/// never synthesizes authority from the package version, executable path,
/// inode, process ID, or a runtime environment variable.
pub fn guardian_runtime_build_identity()
-> Result<SealedAtomicBuildIdentity, AtomicComponentIdentityError> {
    guardian_embedded_build_identity()?.require_sealed()
}

#[cfg(unix)]
pub mod broker;
#[cfg(unix)]
pub(crate) mod output;
#[cfg(unix)]
pub mod runtime;
#[cfg(unix)]
pub mod transport;

#[cfg(unix)]
pub use broker::{
    BrokerCensusDispositionV1, BrokerCensusEntryV1, BrokerCensusPageRequestV1, BrokerCensusPageV1,
    BrokerCensusV1, BrokerControlClientError, BrokerControlClientV1, BrokerControlServiceConfigV1,
    BrokerControlServiceError, BrokerControlServiceV1, BrokerExecBootstrapErrorV1,
    BrokerGuardianConnectionIdentityV1, BrokerInitialPaneClaimV1, BrokerPaneRecoverySecretV1,
    BrokerSpawnClaimQueryV1, BrokerSpawnEffectAcknowledgementV1, BrokerSpawnEffectQueryV1,
    BrokerSpawnSubmissionV1, BrokerSuccessorAcknowledgementV1, BrokerSuccessorClaimQueryV1,
    BrokerSuccessorPaneClaimV1, run_broker_exec_bootstrap,
};
#[cfg(unix)]
pub use mux::guardian_protocol::{GuardianInputEffectQuery, InputEffectState};
#[cfg(unix)]
pub use output::{
    GuardianCheckpointAdoptionSelectorV1, GuardianDurableRecoveryClaimIntentV1,
    GuardianDurableSpawnCustodyV1, GuardianDurableSuccessorCustodyV1,
    GuardianRecoveryClaimAdmission, GuardianReopenedCheckpointV1,
};
#[cfg(unix)]
pub use runtime::{GuardianRuntime, GuardianRuntimeConfig, GuardianRuntimeCounters};
#[cfg(unix)]
pub use transport::{
    GuardianClaimedPaneLease, GuardianClient, GuardianClientError, GuardianProbeReport,
    GuardianService, GuardianServiceConfig, GuardianServiceError, GuardianSuccessorRotationClient,
    ProvisionTokenOutcome, provision_guardian_token,
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

#[cfg(test)]
mod build_identity_tests {
    use super::*;

    #[test]
    fn unsealed_development_marker_cannot_become_runtime_authority() {
        match guardian_embedded_build_identity().unwrap() {
            AtomicBuildIdentity::Sealed(expected) => {
                assert_eq!(guardian_runtime_build_identity().unwrap(), expected);
            }
            AtomicBuildIdentity::UnsealedDevelopment => {
                assert_eq!(
                    guardian_runtime_build_identity(),
                    Err(AtomicComponentIdentityError::UnsealedDevelopmentBuild)
                );
            }
        }
    }
}
