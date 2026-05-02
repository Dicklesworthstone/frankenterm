//! Connector extraction boundary crate.
//!
//! The connector DTO leaf crate is already independent. This crate gives the
//! runtime connector migration a cycle-free workspace member that can absorb
//! connector modules as their remaining `frankenterm-core` back-edges are cut.

pub use frankenterm_core_connector_types::*;

/// Core modules still owning connector runtime logic after the first boundary
/// crate pass.
pub const CONNECTOR_RUNTIME_IMPORTERS_REMAINING: &[&str] = &[
    "policy.rs",
    "config.rs",
    "chaos_scale_harness.rs",
    "runtime_telemetry.rs",
];

/// Current extraction status for diagnostics and follow-up beads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectorExtractionStatus {
    pub type_crate: &'static str,
    pub runtime_importers_remaining: &'static [&'static str],
}

impl ConnectorExtractionStatus {
    /// Return the status of the connector boundary after this extraction pass.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            type_crate: "frankenterm-core-connector-types",
            runtime_importers_remaining: CONNECTOR_RUNTIME_IMPORTERS_REMAINING,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConnectorExtractionStatus, CredentialBrokerTelemetry, CONNECTOR_RUNTIME_IMPORTERS_REMAINING,
    };

    #[test]
    fn reexports_connector_leaf_types() {
        let telemetry = CredentialBrokerTelemetry::default();

        assert_eq!(telemetry.leases_issued, 0);
    }

    #[test]
    fn status_tracks_remaining_runtime_edges() {
        let status = ConnectorExtractionStatus::current();

        assert_eq!(status.type_crate, "frankenterm-core-connector-types");
        assert_eq!(
            status.runtime_importers_remaining,
            CONNECTOR_RUNTIME_IMPORTERS_REMAINING
        );
    }
}
