//! Workspace deadwire CI gate (ft-7h5da.5.5).
//!
//! Runs the `analyze_decision_api_wiring` engine against the REAL workspace
//! sources so "built-but-never-consulted" decision APIs become a structurally
//! detectable defect class. CI runs `cargo test --workspace`, so a failure
//! here fails CI:
//!
//! - a curated decision-API symbol with zero non-test production callers and
//!   no dormant exemption is a violation;
//! - a dormant exemption without a bead id, without an expiry, or past its
//!   expiry is a violation (exemptions cannot rot into blanket passes);
//! - the published inventory artifact
//!   (`docs/attestations/doctrine/decision-api-wiring-status.json`) must match
//!   the computed statuses, so the doctrine record cannot silently drift.
//!
//! Engine limitations (accepted for this line-based v1): mentions inside
//! comments count as callers, so a `wired` verdict is a weaker claim than a
//! `deadwire` one; `#[cfg(test)]` regions and `tests/` paths are excluded by
//! this driver before the engine sees them.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use frankenterm_topo::{
    DecisionApiDeclaration, DecisionApiSourceFile, DecisionApiWiringInput, DecisionApiWiringStatus,
    DormantDecisionApiExemption, DormantDecisionApiExemptionEntry, analyze_decision_api_wiring,
};

const PRODUCED_BY_BEAD: &str = "ft-7h5da.5.5";
const DORMANT_MANIFEST: &str = "crates/frankenterm-topo/deadwire-dormant.json";
const WIRING_STATUS_ARTIFACT: &str = "docs/attestations/doctrine/decision-api-wiring-status.json";
const SOURCE_ROOTS: [&str; 2] = ["crates/frankenterm-core/src", "crates/frankenterm/src"];

/// Curated decision-API entry points (the W4 dead-wire family). Adding a new
/// decision API here without a production caller or a dormant exemption fails
/// CI — which is exactly the point.
fn declarations() -> Vec<DecisionApiDeclaration> {
    [
        (
            "run_connector_lifecycle_intent",
            "crates/frankenterm-core/src/policy.rs",
        ),
        (
            "route_connector_operation_through_mesh",
            "crates/frankenterm-core/src/policy.rs",
        ),
        (
            "process_connector_outbound_runtime_event",
            "crates/frankenterm-core/src/runtime.rs",
        ),
        (
            "check_dedup",
            "crates/frankenterm-core/src/tx_idempotency.rs",
        ),
        (
            "execute_with_store",
            "crates/frankenterm-core/src/tx_execution.rs",
        ),
        (
            "allow_operation",
            "crates/frankenterm-core/src/connector_reliability.rs",
        ),
        (
            "record_action_failure",
            "crates/frankenterm-core/src/connector_outbound_bridge.rs",
        ),
        (
            "evaluate_from_trackers",
            "crates/frankenterm-core/src/quota_gate.rs",
        ),
    ]
    .into_iter()
    .map(|(symbol, path)| DecisionApiDeclaration::new(symbol, path))
    .collect()
}

#[derive(serde::Deserialize)]
struct DormantManifest {
    schema_version: u32,
    exemptions: Vec<DormantManifestEntry>,
}

#[derive(serde::Deserialize)]
struct DormantManifestEntry {
    symbol: String,
    bead_id: String,
    expires_on: String,
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root must resolve from crates/frankenterm-topo")
}

/// Net `{`/`}` balance of a line. Format-string braces (`"{x}"`) pair up, so
/// they do not skew the balance; pathological unpaired braces inside string
/// literals would, which is acceptable for this v1 line-based skipper.
fn brace_delta(line: &str) -> i64 {
    let mut delta = 0_i64;
    for byte in line.bytes() {
        match byte {
            b'{' => delta += 1,
            b'}' => delta -= 1,
            _ => {}
        }
    }
    delta
}

/// Drop `#[cfg(test)]` items (test modules, test-only fns/uses) from source
/// text so in-file test callers do not count as production wiring.
fn strip_cfg_test_regions(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut kept = Vec::with_capacity(lines.len());
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.trim() == "#[cfg(test)]" {
            let mut cursor = index + 1;
            while cursor < lines.len() && lines[cursor].trim_start().starts_with("#[") {
                cursor += 1;
            }
            let mut depth = 0_i64;
            let mut opened = false;
            while cursor < lines.len() {
                let item_line = lines[cursor];
                depth += brace_delta(item_line);
                if item_line.contains('{') {
                    opened = true;
                }
                if opened && depth <= 0 {
                    break;
                }
                if !opened && item_line.trim_end().ends_with(';') {
                    break;
                }
                cursor += 1;
            }
            index = cursor + 1;
            continue;
        }
        kept.push(line);
        index += 1;
    }
    kept.join("\n")
}

fn collect_rust_files(root: &Path, dir: &Path, out: &mut Vec<DecisionApiSourceFile>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|err| panic!("read_dir {} failed: {err}", dir.display()));
    let mut paths: Vec<PathBuf> = entries
        .map(|entry| entry.expect("dir entry must be readable").path())
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_rust_files(root, &path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let relative = path
                .strip_prefix(root)
                .expect("collected path must live under the workspace root")
                .to_string_lossy()
                .replace('\\', "/");
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("read {} failed: {err}", path.display()));
            out.push(DecisionApiSourceFile::new(
                relative,
                strip_cfg_test_regions(&text),
            ));
        }
    }
}

/// Days-since-epoch to `YYYY-MM-DD` (Howard Hinnant's `civil_from_days`),
/// avoiding a date-crate dependency for one conversion.
fn today_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be at or after the UNIX epoch")
        .as_secs();
    let days = i64::try_from(secs / 86_400).expect("day count fits in i64");
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[test]
fn workspace_decision_apis_are_wired_or_dormant_with_live_exemptions() {
    let root = workspace_root();

    let manifest_text = fs::read_to_string(root.join(DORMANT_MANIFEST))
        .expect("dormant manifest must exist next to the gate");
    let manifest: DormantManifest =
        serde_json::from_str(&manifest_text).expect("dormant manifest must parse");
    assert_eq!(
        manifest.schema_version, 1,
        "unknown dormant manifest schema"
    );
    let dormant_exemptions: Vec<DormantDecisionApiExemptionEntry> = manifest
        .exemptions
        .iter()
        .map(|entry| {
            DormantDecisionApiExemptionEntry::new(
                entry.symbol.clone(),
                DormantDecisionApiExemption::new(entry.bead_id.clone(), entry.expires_on.clone()),
            )
        })
        .collect();

    let declarations = declarations();
    for declaration in &declarations {
        assert!(
            root.join(&declaration.defining_path).is_file(),
            "declared defining_path {} no longer exists — update the gate's declaration list",
            declaration.defining_path
        );
    }

    let mut source_files = Vec::new();
    for source_root in SOURCE_ROOTS {
        collect_rust_files(&root, &root.join(source_root), &mut source_files);
    }
    assert!(
        source_files.len() >= 400,
        "suspiciously few source files scanned ({}) — path wiring is broken",
        source_files.len()
    );

    let generated_at_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be at or after the UNIX epoch")
            .as_millis(),
    )
    .expect("epoch millis fit in u64");
    let today = today_utc();

    let report = analyze_decision_api_wiring(DecisionApiWiringInput {
        declarations: &declarations,
        source_files: &source_files,
        dormant_exemptions: &dormant_exemptions,
        produced_by_bead: PRODUCED_BY_BEAD,
        generated_at_ms,
        today_utc: &today,
    });

    assert!(
        report.violations.is_empty(),
        "deadwire gate violations:\n{}\nFix: wire a production caller, or add a dormant \
         exemption with bead_id + expires_on to {DORMANT_MANIFEST}.",
        report
            .violations
            .iter()
            .map(|violation| format!(
                "  {} [{:?}]: {}",
                violation.symbol, violation.reason, violation.required_action
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // The published doctrine artifact must match the computed inventory.
    let artifact_text = fs::read_to_string(root.join(WIRING_STATUS_ARTIFACT))
        .expect("wiring-status artifact must exist");
    let artifact: serde_json::Value =
        serde_json::from_str(&artifact_text).expect("wiring-status artifact must parse");
    let artifact_statuses: BTreeMap<String, String> = artifact["records"]
        .as_array()
        .expect("artifact records must be an array")
        .iter()
        .map(|record| {
            (
                record["symbol"]
                    .as_str()
                    .expect("record symbol must be a string")
                    .to_string(),
                record["status"]
                    .as_str()
                    .expect("record status must be a string")
                    .to_string(),
            )
        })
        .collect();

    let computed_statuses: BTreeMap<String, String> = report
        .records
        .iter()
        .map(|record| {
            let status = match record.status {
                DecisionApiWiringStatus::Wired => "wired",
                DecisionApiWiringStatus::Dormant => "dormant",
                DecisionApiWiringStatus::Deadwire => "deadwire",
            };
            (record.symbol.clone(), status.to_string())
        })
        .collect();

    assert_eq!(
        artifact_statuses, computed_statuses,
        "{WIRING_STATUS_ARTIFACT} is stale — update its records to match the computed \
         wiring statuses (left: artifact, right: computed)"
    );
}
