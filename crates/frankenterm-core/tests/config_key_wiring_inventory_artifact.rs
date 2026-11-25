//! Pins the published config-key wiring doctrine artifact to the in-code
//! inventory (ft-7h5da.5.7 / W4.6 config dead-key honesty).
//!
//! The artifact (`docs/attestations/doctrine/config-key-wiring-status.json`)
//! is the operator-facing record of which parsed config keys have a production
//! consumer and which are inert-at-default + fail-closed-when-customized. This
//! test fails CI when the artifact drifts from `CONFIG_KEY_WIRING_INVENTORY`,
//! and when the `ft config validate` report stops listing every
//! parsed-but-unconsumed key.

use std::path::{Path, PathBuf};

use frankenterm_core::config::{
    CONFIG_KEY_WIRING_STATUS_SCHEMA_VERSION, ConfigKeyWiringStatus, config_key_wiring_inventory,
    config_key_wiring_validate_report,
};

const ARTIFACT: &str = "docs/attestations/doctrine/config-key-wiring-status.json";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root must resolve from crates/frankenterm-core")
}

#[test]
fn doctrine_artifact_matches_inventory() {
    let artifact_text = std::fs::read_to_string(workspace_root().join(ARTIFACT))
        .expect("config-key wiring doctrine artifact must exist");
    let artifact: serde_json::Value =
        serde_json::from_str(&artifact_text).expect("doctrine artifact must parse");

    assert_eq!(
        artifact["schema_version"],
        serde_json::json!(CONFIG_KEY_WIRING_STATUS_SCHEMA_VERSION),
        "artifact schema_version must match CONFIG_KEY_WIRING_STATUS_SCHEMA_VERSION"
    );
    assert_eq!(
        artifact["kind"],
        serde_json::json!("config-key-wiring-status")
    );

    let expected: Vec<serde_json::Value> = config_key_wiring_inventory()
        .iter()
        .map(|record| serde_json::to_value(record).expect("inventory record serializes"))
        .collect();
    assert_eq!(
        artifact["records"],
        serde_json::Value::Array(expected),
        "{ARTIFACT} records are stale — regenerate them from \
         config::CONFIG_KEY_WIRING_INVENTORY (field-for-field)"
    );
}

#[test]
fn validate_report_lists_every_unconsumed_key() {
    // A key leaves the unconsumed list only by naming its production consumer
    // and the test that proves it; an unconsumed key names neither.
    for record in config_key_wiring_inventory() {
        match record.status {
            ConfigKeyWiringStatus::Consumed => assert!(
                !matches!(record.consumer, "" | "none") && !record.evidence.is_empty(),
                "consumed key {} must name its consumer and evidence",
                record.key
            ),
            ConfigKeyWiringStatus::ParsedButUnconsumed => assert_eq!(
                record.consumer, "none",
                "unconsumed key {} must not claim a consumer",
                record.key
            ),
        }
    }
    let unconsumed_keys: Vec<&str> = config_key_wiring_inventory()
        .iter()
        .filter(|record| record.status == ConfigKeyWiringStatus::ParsedButUnconsumed)
        .map(|record| record.key)
        .collect();

    let report = config_key_wiring_validate_report();
    assert_eq!(
        report.len(),
        unconsumed_keys.len(),
        "ft config validate must report every parsed-but-unconsumed key"
    );
    for (line, key) in report.iter().zip(unconsumed_keys) {
        assert!(
            line.starts_with(key),
            "report line must lead with its key: {line} (expected {key})"
        );
        assert!(
            line.contains("tracking ft-"),
            "report line must carry the tracking bead: {line}"
        );
    }
}
