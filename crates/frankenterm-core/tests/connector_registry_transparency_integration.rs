use std::collections::BTreeMap;

use frankenterm_core::connector_host_runtime::ConnectorCapability;
use frankenterm_core::connector_registry::{
    ConnectorManifest, ConnectorRegistryClient, ConnectorRegistryConfig,
    TRANSPARENCY_TOKEN_SCHEMA_VERSION, TransparencyProofToken, compute_digest,
};
use frankenterm_core::merkle_tree::MerkleTree;

fn manifest(package_id: &str, payload: &[u8]) -> ConnectorManifest {
    ConnectorManifest {
        schema_version: 1,
        package_id: package_id.to_string(),
        version: "1.0.0".to_string(),
        display_name: package_id.to_string(),
        description: "Integration test connector".to_string(),
        author: "integration-author".to_string(),
        min_ft_version: None,
        sha256_digest: compute_digest(payload),
        required_capabilities: vec![ConnectorCapability::Invoke],
        publisher_signature: Some("deadbeef".to_string()),
        transparency_token: None,
        created_at_ms: 1_000,
        metadata: BTreeMap::new(),
    }
}

fn make_transparency_token(manifest: &ConnectorManifest, log_index: u64) -> (String, String) {
    let key = format!(
        "connector-manifest/{}/{}",
        manifest.package_id, manifest.version
    )
    .into_bytes();
    let value = manifest.sha256_digest.to_ascii_lowercase().into_bytes();
    let tree = MerkleTree::from_entries([(key.clone(), value)]);
    let proof = tree.proof(&key).expect("proof should exist");
    let root_hash = tree.root_hash().to_string();
    let token = serde_json::to_string(&TransparencyProofToken {
        schema_version: TRANSPARENCY_TOKEN_SCHEMA_VERSION,
        package_id: manifest.package_id.clone(),
        version: manifest.version.clone(),
        sha256_digest: manifest.sha256_digest.clone(),
        log_index,
        proof,
    })
    .expect("token serialization");
    (root_hash, token)
}

#[test]
fn connector_registry_transparency_integration_policy_required_proof_fails_closed() {
    let payload = b"connector payload";
    let mut manifest = manifest("pkg", payload);
    manifest.transparency_token = Some("not-json".to_string());

    let mut config = ConnectorRegistryConfig::default();
    config.trust_policy.require_transparency_proof = true;
    config.trust_policy.trusted_transparency_roots = vec!["deadbeef".to_string()];
    let mut client = ConnectorRegistryClient::new(config);

    let err = client
        .register_package(manifest, payload, 2_000)
        .unwrap_err();
    assert!(err.to_string().contains("invalid transparency token JSON"));
}

#[test]
fn connector_registry_transparency_integration_policy_required_proof_accepts_valid_token() {
    let payload = b"connector payload";
    let mut manifest = manifest("pkg", payload);
    let (root_hash, token) = make_transparency_token(&manifest, 17);
    manifest.transparency_token = Some(token);

    let mut config = ConnectorRegistryConfig::default();
    config.trust_policy.require_transparency_proof = true;
    config.trust_policy.trusted_transparency_roots = vec![root_hash];
    let mut client = ConnectorRegistryClient::new(config);

    let entry = client
        .register_package(manifest, payload, 2_000)
        .expect("valid transparency token should register");
    assert_eq!(entry.manifest.package_id, "pkg");
    assert_eq!(client.active_packages().len(), 1);
}
