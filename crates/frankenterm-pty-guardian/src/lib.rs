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
//! behind typed, identity-bound transport operations. Production guardian
//! selection, Genesis recovery-base creation, topology publication, service
//! activation, and automated mux migration remain intentionally rejected until
//! their anti-rollback and recovery authorities are integrated. The
//! production-disabled broker separately exposes a content-free, paginated
//! view of authenticated recovered Spawn journals. A live pre-acknowledgement
//! Spawn also mints one plaintext recovery capability while persisting only its
//! pane-bound verifier; recovered-journal Census entries still grant no PTY,
//! lease, output-replay, or mutation authority. The service can be stopped
//! through an authenticated guarded
//! transaction only while it owns no panes; a successful stop deliberately
//! retains the socket path, so restart remains fail-closed until an explicit
//! non-overwriting retirement design lands. Successor Claim/Query/Ack is live
//! and effect-fenced in process, but its lease transitions are not yet in the
//! authenticated WAL; production activation therefore remains disabled.

pub use frankenterm_build_identity::{
    AtomicBuildIdentity, AtomicComponentIdentityError, SealedAtomicBuildIdentity,
};

use frankenterm_build_identity::{
    AtomicComponentRole, parse_atomic_component_marker, parse_sealed_atomic_component_marker,
};

const GUARDIAN_ATOMIC_COMPONENT_MARKER: &str = env!("FT_ATOMIC_COMPONENT_MARKER");

/// Return the exact marker embedded by this guardian binary's build script.
///
/// This value is public identity evidence, not a secret or a capability. The
/// caller must still authenticate the live guardian connection before binding
/// it into a spawn or adoption transaction.
#[must_use]
pub const fn guardian_atomic_component_marker() -> &'static str {
    GUARDIAN_ATOMIC_COMPONENT_MARKER
}

/// Validate the embedded guardian marker while preserving explicit development
/// state. `UnsealedDevelopment` is never interchangeable with a runtime build
/// authority.
pub fn guardian_embedded_build_identity()
-> Result<AtomicBuildIdentity, AtomicComponentIdentityError> {
    parse_atomic_component_marker(
        GUARDIAN_ATOMIC_COMPONENT_MARKER,
        AtomicComponentRole::FrankenTermPtyGuardian,
    )
}

/// Return the exact decoded 32-byte build authority for this running guardian.
///
/// An ordinary development build returns
/// [`AtomicComponentIdentityError::UnsealedDevelopmentBuild`]. The function
/// never synthesizes authority from the package version, executable path,
/// inode, process ID, or a runtime environment variable.
pub fn guardian_runtime_build_identity()
-> Result<SealedAtomicBuildIdentity, AtomicComponentIdentityError> {
    parse_sealed_atomic_component_marker(
        GUARDIAN_ATOMIC_COMPONENT_MARKER,
        AtomicComponentRole::FrankenTermPtyGuardian,
    )
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
pub use runtime::{GuardianRuntime, GuardianRuntimeConfig, GuardianRuntimeCounters};
#[cfg(unix)]
pub use transport::{
    GuardianClaimedPaneLease, GuardianClient, GuardianClientError, GuardianProbeReport,
    GuardianService, GuardianServiceConfig, GuardianServiceError, ProvisionTokenOutcome,
    provision_guardian_token,
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
    use frankenterm_build_identity::{AtomicComponentRole, parse_atomic_component_marker};

    #[test]
    fn embedded_marker_is_pinned_to_the_guardian_role() {
        let marker = guardian_atomic_component_marker();
        assert!(
            parse_atomic_component_marker(marker, AtomicComponentRole::FrankenTermPtyGuardian)
                .is_ok()
        );
        assert!(
            parse_atomic_component_marker(marker, AtomicComponentRole::FrankenTermMuxServer)
                .is_err()
        );
    }

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
