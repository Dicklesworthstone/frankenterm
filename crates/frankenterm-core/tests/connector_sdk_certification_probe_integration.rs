use frankenterm_core::connector_host_runtime::ConnectorCapability;
use frankenterm_core::connector_sdk::{
    CertificationPipeline, CertificationVerdict, IntegrationProbeStatus, ManifestBuilder,
    PROBE_METADATA_FILESYSTEM_READ_TARGET, TrustPolicyBuilder,
};

fn payload() -> Vec<u8> {
    b"connector certification probe integration payload".to_vec()
}

fn trusted_policy(
    caps: &[ConnectorCapability],
) -> frankenterm_core::connector_registry::TrustPolicy {
    TrustPolicyBuilder::new()
        .allow_capabilities(caps)
        .trusted_publisher("dev@example.com")
        .build()
}

#[test]
fn connector_sdk_certification_probe_integration_round_trips_target_capability() {
    let policy = trusted_policy(&[ConnectorCapability::FilesystemRead]);
    let mut pipeline = CertificationPipeline::new(policy);
    let payload = payload();
    let probe_target = "/tmp/frankenterm-certification-probe/input.txt";

    let mut manifest = ManifestBuilder::new("probe-fs-read")
        .version("1.0.0")
        .author("dev@example.com")
        .publisher_signature("integration-sig")
        .capability(ConnectorCapability::FilesystemRead)
        .build_with_digest(&payload)
        .unwrap();
    manifest.metadata.insert(
        PROBE_METADATA_FILESYSTEM_READ_TARGET.to_string(),
        probe_target.to_string(),
    );

    let report = pipeline.certify(&manifest, &payload);
    assert_eq!(report.verdict, CertificationVerdict::Certified);
    assert!(report.passed());

    let probe = report.integration_probe.as_ref().unwrap();
    assert_eq!(probe.status, IntegrationProbeStatus::Passed);
    assert!(probe.health_live);
    assert!(probe.health_ready);
    assert!(probe.heartbeat_recorded);
    assert!(probe.stopped_cleanly);
    assert_eq!(probe.actions.len(), 1);
    assert_eq!(
        probe.actions[0].capability,
        ConnectorCapability::FilesystemRead
    );
    assert_eq!(probe.actions[0].target.as_deref(), Some(probe_target));
}

#[test]
fn connector_sdk_certification_probe_integration_skips_without_required_target() {
    let policy = trusted_policy(&[ConnectorCapability::FilesystemRead]);
    let mut pipeline = CertificationPipeline::new(policy);
    let payload = payload();

    let manifest = ManifestBuilder::new("probe-fs-read-missing-target")
        .version("1.0.0")
        .author("dev@example.com")
        .publisher_signature("integration-sig")
        .capability(ConnectorCapability::FilesystemRead)
        .build_with_digest(&payload)
        .unwrap();

    let report = pipeline.certify(&manifest, &payload);
    assert_eq!(report.verdict, CertificationVerdict::ConditionalPass);
    assert!(!report.passed());

    let probe = report.integration_probe.as_ref().unwrap();
    assert_eq!(probe.status, IntegrationProbeStatus::Skipped);
    assert!(
        probe
            .detail
            .as_deref()
            .unwrap()
            .contains(PROBE_METADATA_FILESYSTEM_READ_TARGET)
    );
}

#[test]
fn connector_sdk_certification_probe_integration_rejects_invalid_target_contract() {
    let policy = trusted_policy(&[ConnectorCapability::FilesystemRead]);
    let mut pipeline = CertificationPipeline::new(policy);
    let payload = payload();

    let mut manifest = ManifestBuilder::new("probe-fs-read-invalid-target")
        .version("1.0.0")
        .author("dev@example.com")
        .publisher_signature("integration-sig")
        .capability(ConnectorCapability::FilesystemRead)
        .build_with_digest(&payload)
        .unwrap();
    manifest.metadata.insert(
        PROBE_METADATA_FILESYSTEM_READ_TARGET.to_string(),
        "relative/path.txt".to_string(),
    );

    let report = pipeline.certify(&manifest, &payload);
    assert_eq!(report.verdict, CertificationVerdict::Rejected);
    assert!(!report.passed());

    let probe = report.integration_probe.as_ref().unwrap();
    assert_eq!(probe.status, IntegrationProbeStatus::Failed);
    assert!(
        probe
            .detail
            .as_deref()
            .unwrap()
            .contains("authorization failed")
    );
}
