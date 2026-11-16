//! Golden checks for deterministic scrubbed incident-bundle fixtures.
//!
//! These fixtures are intentionally committed as files rather than generated at
//! test time: the verifier surface needs stable reviewable artifacts for normal,
//! degraded, and sensitive-transcript cases.

use frankenterm_core::crash::{
    IncidentBundleResult, IncidentEvidenceState, IncidentSourceStatus, ReplayMode,
    replay_incident_bundle,
};
use frankenterm_core::redactor::Redactor;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

const FIXTURE_TS: &str = "2026-05-10T11:12:40Z";
const REQUIRED_CASES: &[&str] = &[
    "normal",
    "degraded-source",
    "sensitive-transcript",
    "rch-timeout",
    "beads-blocked",
    "resource-pressure",
    "sampler-unavailable",
];

#[derive(Debug)]
struct FixtureReport {
    covered_cases: BTreeSet<String>,
}

#[test]
fn committed_incident_bundle_goldens_validate() {
    let mut covered_cases = BTreeSet::new();

    for bundle in fixture_dirs() {
        let report = validate_fixture_bundle(&bundle)
            .unwrap_or_else(|err| panic!("{} failed fixture validation: {err}", bundle.display()));
        covered_cases.extend(report.covered_cases);

        let replay = replay_incident_bundle(&bundle, ReplayMode::Policy)
            .unwrap_or_else(|err| panic!("{} failed replay: {err}", bundle.display()));
        assert_eq!(
            replay.status,
            "pass",
            "{} should pass policy replay: {:#?}",
            bundle.display(),
            replay.checks
        );
    }

    for required in REQUIRED_CASES {
        assert!(
            covered_cases.contains(*required),
            "incident-bundle fixture suite missing required case {required}; covered={covered_cases:?}"
        );
    }
}

#[test]
fn fixture_validation_rejects_raw_nested_secret() {
    let temp = copy_fixture_to_temp("sensitive_transcript");
    let leaked = format!(
        "{{\"pane_id\":41,\"tail\":\"{}\"}}\n",
        synthetic_aws_access_key()
    );
    fs::write(
        temp.path().join("sources").join("pane_text_summaries.json"),
        leaked,
    )
    .expect("write leaked nested source");

    let err = validate_fixture_bundle(temp.path()).expect_err("raw nested secret should fail");
    assert!(
        err.contains("raw secret"),
        "expected raw-secret validation error, got: {err}"
    );
}

#[test]
fn fixture_validation_rejects_missing_source_provenance() {
    let temp = copy_fixture_to_temp("normal");
    let mut manifest = read_manifest_value(temp.path());
    let sources = manifest["swarm"]["sources"]
        .as_array_mut()
        .expect("fixture sources array");
    sources.retain(|source| source["name"] != "robot_state");
    write_manifest_value(temp.path(), manifest);

    let err = validate_fixture_bundle(temp.path()).expect_err("orphaned source should fail");
    assert!(
        err.contains("lacks manifest provenance"),
        "expected provenance validation error, got: {err}"
    );
}

#[test]
fn fixture_validation_rejects_nondeterministic_timestamps() {
    let temp = copy_fixture_to_temp("normal");
    let mut manifest = read_manifest_value(temp.path());
    manifest["swarm"]["created_at"] = Value::String("2026-05-10T18:33:30.566074Z".to_string());
    write_manifest_value(temp.path(), manifest);

    let err = validate_fixture_bundle(temp.path()).expect_err("dynamic timestamp should fail");
    assert!(
        err.contains("nondeterministic timestamp"),
        "expected timestamp validation error, got: {err}"
    );
}

#[test]
fn fixture_validation_rejects_degraded_source_without_warning() {
    let temp = copy_fixture_to_temp("degraded");
    let mut manifest = read_manifest_value(temp.path());
    let sources = manifest["swarm"]["sources"]
        .as_array_mut()
        .expect("fixture sources array");
    let agent_mail = sources
        .iter_mut()
        .find(|source| source["name"] == "agent_mail_snapshot")
        .expect("agent-mail source");
    agent_mail
        .as_object_mut()
        .expect("agent-mail source object")
        .remove("warning_ids");
    write_manifest_value(temp.path(), manifest);

    let err = validate_fixture_bundle(temp.path()).expect_err("missing warning should fail");
    assert!(
        err.contains("missing warning_ids"),
        "expected degraded-source warning validation error, got: {err}"
    );
}

fn fixture_dirs() -> Vec<PathBuf> {
    ["normal", "degraded", "sensitive_transcript"]
        .into_iter()
        .map(|name| fixture_root().join(name))
        .collect()
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("incident_bundle_goldens")
}

fn validate_fixture_bundle(bundle: &Path) -> Result<FixtureReport, String> {
    let manifest_path = bundle.join("incident_manifest.json");
    let manifest_json = fs::read_to_string(&manifest_path)
        .map_err(|err| format!("cannot read {}: {err}", manifest_path.display()))?;
    let manifest: IncidentBundleResult = serde_json::from_str(&manifest_json)
        .map_err(|err| format!("manifest schema drift: {err}"))?;
    let manifest_value: Value = serde_json::from_str(&manifest_json)
        .map_err(|err| format!("manifest JSON invalid: {err}"))?;
    let swarm = manifest
        .swarm
        .as_ref()
        .ok_or_else(|| "manifest missing swarm extension".to_string())?;

    assert_fixed_timestamp(&manifest.exported_at, "manifest.exported_at")?;
    assert_fixed_timestamp(&swarm.created_at, "swarm.created_at")?;
    if swarm.contract_id != "ft.swarm_incident_bundle.v1" {
        return Err(format!("unexpected contract id {}", swarm.contract_id));
    }
    if swarm.schema_version != 1 || swarm.format_version != "1.0" {
        return Err(format!(
            "unexpected schema version {} / {}",
            swarm.schema_version, swarm.format_version
        ));
    }
    if swarm.collection_policy.mutating_actions_allowed {
        return Err("fixture collector must remain read-only".to_string());
    }

    let manifest_files = manifest
        .files
        .iter()
        .map(|file| {
            validate_bundle_relative_path(file)?;
            let path = bundle.join(file);
            if !path.is_file() {
                return Err(format!("manifest lists missing file {file}"));
            }
            Ok(file.as_str())
        })
        .collect::<Result<HashSet<_>, String>>()?;

    for required in [
        "incident_manifest.json",
        "README.md",
        "redaction_report.json",
        "warnings.jsonl",
    ] {
        if !manifest_files.contains(required) {
            return Err(format!("manifest.files missing required file {required}"));
        }
    }

    let warning_ids = swarm
        .warnings
        .iter()
        .map(|warning| warning.id.clone())
        .collect::<HashSet<_>>();
    let jsonl_warning_ids = read_warning_jsonl_ids(bundle)?;
    if jsonl_warning_ids != warning_ids {
        return Err(format!(
            "warnings.jsonl ids {jsonl_warning_ids:?} do not match manifest warnings {warning_ids:?}"
        ));
    }

    let mut source_files = HashMap::new();
    let mut covered_cases = BTreeSet::new();
    if bundle.file_name().and_then(|name| name.to_str()) == Some("normal") {
        covered_cases.insert("normal".to_string());
    }
    if bundle.file_name().and_then(|name| name.to_str()) == Some("sensitive_transcript") {
        covered_cases.insert("sensitive-transcript".to_string());
    }

    for source in &swarm.sources {
        if source.source_surface.trim().is_empty() {
            return Err(format!("source {} missing source_surface", source.name));
        }
        if source.mutates_state {
            return Err(format!("source {} mutates state", source.name));
        }
        if let Some(generated_at) = &source.generated_at {
            assert_fixed_timestamp(
                generated_at,
                &format!("source {} generated_at", source.name),
            )?;
        }
        for warning_id in &source.warning_ids {
            if !warning_ids.contains(warning_id) {
                return Err(format!(
                    "source {} references missing warning id {warning_id}",
                    source.name
                ));
            }
        }

        match source.status {
            IncidentSourceStatus::Collected => {
                let file = source
                    .file
                    .as_ref()
                    .ok_or_else(|| format!("collected source {} missing file", source.name))?;
                validate_bundle_relative_path(file)?;
                if !manifest_files.contains(file.as_str()) {
                    return Err(format!(
                        "source {} file {file} missing from manifest.files",
                        source.name
                    ));
                }
                if !bundle.join(file).is_file() {
                    return Err(format!("source {} file {file} is absent", source.name));
                }
                if matches!(source.evidence_state, IncidentEvidenceState::Unavailable) {
                    return Err(format!(
                        "collected source {} cannot be unavailable evidence",
                        source.name
                    ));
                }
                source_files.insert(file.clone(), source.name.clone());
            }
            IncidentSourceStatus::Skipped
            | IncidentSourceStatus::Unavailable
            | IncidentSourceStatus::Failed
            | IncidentSourceStatus::Stale => {
                covered_cases.insert("degraded-source".to_string());
                if source.warning_ids.is_empty() {
                    return Err(format!(
                        "degraded source {} missing warning_ids",
                        source.name
                    ));
                }
                if source.file.is_some() && source.status == IncidentSourceStatus::Unavailable {
                    return Err(format!(
                        "unavailable source {} should not write a payload",
                        source.name
                    ));
                }
                if let Some(file) = &source.file {
                    validate_bundle_relative_path(file)?;
                    if !manifest_files.contains(file.as_str()) {
                        return Err(format!(
                            "source {} file {file} missing from manifest.files",
                            source.name
                        ));
                    }
                    if !bundle.join(file).is_file() {
                        return Err(format!("source {} file {file} is absent", source.name));
                    }
                    source_files.insert(file.clone(), source.name.clone());
                }
            }
        }

        match source.name.as_str() {
            "resource_pressure_snapshot" if source.status == IncidentSourceStatus::Collected => {
                covered_cases.insert("resource-pressure".to_string());
            }
            "rch_timeout_evidence" if source.warning_ids.iter().any(|id| id == "rch.timeout") => {
                covered_cases.insert("rch-timeout".to_string());
            }
            "beads_blocker_snapshot" if source.status == IncidentSourceStatus::Collected => {
                covered_cases.insert("beads-blocked".to_string());
            }
            "process_sample" if source.status == IncidentSourceStatus::Unavailable => {
                covered_cases.insert("sampler-unavailable".to_string());
            }
            _ => {}
        }
    }

    let source_payloads = collect_source_payloads(bundle)?;
    for payload in source_payloads {
        let rel = path_relative_to_bundle(bundle, &payload)?;
        if !source_files.contains_key(&rel) {
            return Err(format!("source payload {rel} lacks manifest provenance"));
        }
    }

    scan_for_raw_secrets(bundle)?;
    validate_redaction_report(bundle, swarm.redaction_summary.total_redactions)?;
    validate_redaction_markers(bundle, &manifest_value)?;

    Ok(FixtureReport { covered_cases })
}

fn validate_redaction_report(bundle: &Path, expected_total: usize) -> Result<(), String> {
    let report_path = bundle.join("redaction_report.json");
    let report: Value = serde_json::from_str(
        &fs::read_to_string(&report_path)
            .map_err(|err| format!("cannot read {}: {err}", report_path.display()))?,
    )
    .map_err(|err| format!("invalid redaction_report.json: {err}"))?;
    let total = report
        .get("total_redactions")
        .and_then(Value::as_u64)
        .ok_or_else(|| "redaction_report.json missing total_redactions".to_string())?;
    if total != expected_total as u64 {
        return Err(format!(
            "redaction_report total {total} does not match swarm redaction total {expected_total}"
        ));
    }
    Ok(())
}

fn validate_redaction_markers(bundle: &Path, manifest: &Value) -> Result<(), String> {
    let total_redactions = manifest["swarm"]["redaction_summary"]["total_redactions"]
        .as_u64()
        .ok_or_else(|| "swarm redaction_summary missing total_redactions".to_string())?;
    if total_redactions == 0 {
        return Ok(());
    }

    let mut marker_seen = false;
    let mut truncation_marker_seen = false;
    for path in collect_files(&bundle.join("sources"))? {
        let content = fs::read_to_string(&path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        marker_seen |= content.contains("[REDACTED]");
        truncation_marker_seen |= content.contains("[PANE_TEXT_TRUNCATED]");
    }
    if !marker_seen {
        return Err("redacted fixture missing [REDACTED] marker".to_string());
    }
    if bundle.file_name().and_then(|name| name.to_str()) == Some("sensitive_transcript")
        && !truncation_marker_seen
    {
        return Err("sensitive transcript fixture missing truncation marker".to_string());
    }
    Ok(())
}

fn scan_for_raw_secrets(bundle: &Path) -> Result<(), String> {
    let redactor = Redactor::new();
    for path in collect_files(bundle)? {
        if !matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("json" | "toml" | "md" | "jsonl")
        ) {
            continue;
        }
        let content = fs::read_to_string(&path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        let detections = redactor.detect(&content);
        if !detections.is_empty() {
            return Err(format!(
                "raw secret detected in {}",
                path_relative_to_bundle(bundle, &path)?
            ));
        }
    }
    Ok(())
}

fn read_warning_jsonl_ids(bundle: &Path) -> Result<HashSet<String>, String> {
    let warnings_path = bundle.join("warnings.jsonl");
    let warnings = fs::read_to_string(&warnings_path)
        .map_err(|err| format!("cannot read {}: {err}", warnings_path.display()))?;
    let mut ids = HashSet::new();
    for (index, line) in warnings.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let warning: Value = serde_json::from_str(trimmed)
            .map_err(|err| format!("warnings.jsonl line {} invalid: {err}", index + 1))?;
        let id = warning
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("warnings.jsonl line {} missing id", index + 1))?;
        ids.insert(id.to_string());
    }
    Ok(ids)
}

fn collect_source_payloads(bundle: &Path) -> Result<Vec<PathBuf>, String> {
    let sources = bundle.join("sources");
    if !sources.is_dir() {
        return Ok(Vec::new());
    }
    collect_files(&sources)
}

fn collect_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    if !root.exists() {
        return Ok(files);
    }
    collect_files_inner(root, &mut files)?;
    Ok(files)
}

fn collect_files_inner(root: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in
        fs::read_dir(root).map_err(|err| format!("cannot read {}: {err}", root.display()))?
    {
        let entry = entry.map_err(|err| format!("cannot read directory entry: {err}"))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| format!("cannot read file type for {}: {err}", path.display()))?;
        if file_type.is_dir() {
            collect_files_inner(&path, files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(())
}

fn assert_fixed_timestamp(value: &str, field: &str) -> Result<(), String> {
    if value == FIXTURE_TS {
        return Ok(());
    }
    Err(format!(
        "nondeterministic timestamp in {field}: expected {FIXTURE_TS}, got {value}"
    ))
}

fn validate_bundle_relative_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("bundle-relative path is empty".to_string());
    }
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(format!(
            "bundle-relative path {} is absolute",
            path.display()
        ));
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(format!(
                "bundle-relative path {} contains non-normal component",
                path.display()
            ));
        }
    }
    Ok(())
}

fn path_relative_to_bundle(bundle: &Path, path: &Path) -> Result<String, String> {
    let rel = path
        .strip_prefix(bundle)
        .map_err(|err| format!("{} is outside {}: {err}", path.display(), bundle.display()))?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

fn copy_fixture_to_temp(name: &str) -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    copy_dir_recursive(&fixture_root().join(name), temp.path()).expect("copy fixture to tempdir");
    temp
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|err| format!("cannot create {}: {err}", dst.display()))?;
    for entry in fs::read_dir(src).map_err(|err| format!("cannot read {}: {err}", src.display()))? {
        let entry = entry.map_err(|err| format!("cannot read directory entry: {err}"))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|err| format!("cannot read file type for {}: {err}", src_path.display()))?;
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dst_path).map_err(|err| {
                format!(
                    "cannot copy {} to {}: {err}",
                    src_path.display(),
                    dst_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn read_manifest_value(bundle: &Path) -> Value {
    serde_json::from_str(
        &fs::read_to_string(bundle.join("incident_manifest.json")).expect("read manifest"),
    )
    .expect("manifest value")
}

fn write_manifest_value(bundle: &Path, value: Value) {
    let serialized = serde_json::to_string_pretty(&value).expect("serialize manifest");
    fs::write(
        bundle.join("incident_manifest.json"),
        format!("{serialized}\n"),
    )
    .expect("write manifest");
}

fn synthetic_aws_access_key() -> String {
    let prefix = ["AK", "IA"].concat();
    format!("{prefix}ABCDEFGHIJKLMNOP")
}
