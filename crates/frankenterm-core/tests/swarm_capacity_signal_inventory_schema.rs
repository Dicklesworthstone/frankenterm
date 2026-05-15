//! Schema and coverage guards for the ft-b94bx.1 capacity signal inventory.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use jsonschema::{Draft, Validator};
use serde_json::Value;

const REQUIRED_GAPS: [&str; 6] = [
    "cpu_core_saturation",
    "per_agent_workload_class",
    "build_pressure",
    "mux_render_pressure",
    "disk_sqlite_pressure",
    "child_process_pressure",
];

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf()
}

fn schema_path() -> PathBuf {
    workspace_root()
        .join("docs")
        .join("json-schema")
        .join("ft-swarm-capacity-signal-inventory.json")
}

fn inventory_doc_path() -> PathBuf {
    workspace_root()
        .join("docs")
        .join("swarm-capacity-signal-inventory.md")
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("swarm_capacity_signal_inventory")
        .join("complete.json")
}

fn load_json(path: &Path) -> Value {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("failed to parse JSON {}: {err}", path.display()))
}

fn inventory_validator() -> Validator {
    let schema = load_json(&schema_path());
    Validator::options()
        .with_draft(Draft::Draft202012)
        .build(&schema)
        .unwrap_or_else(|err| panic!("inventory schema failed to compile: {err}"))
}

fn validation_errors(validator: &Validator, value: &Value) -> Vec<String> {
    match validator.validate(value) {
        Ok(()) => Vec::new(),
        Err(errors) => errors
            .map(|err| format!("{} at {}", err, err.instance_path))
            .collect(),
    }
}

fn complete_fixture() -> Value {
    load_json(&fixture_path())
}

fn string_array<'a>(value: &'a Value, key: &str) -> Vec<&'a str> {
    value
        .get(key)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("{key} should be an array"))
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .unwrap_or_else(|| panic!("{key} entries should be strings"))
        })
        .collect()
}

#[test]
fn inventory_schema_compiles_and_accepts_complete_fixture() {
    let validator = inventory_validator();
    let fixture = complete_fixture();
    let errors = validation_errors(&validator, &fixture);
    assert!(
        errors.is_empty(),
        "complete fixture failed signal inventory schema validation:\n{}",
        errors.join("\n")
    );
}

#[test]
fn inventory_doc_mentions_every_signal_and_gap() {
    let fixture = complete_fixture();
    let doc_path = inventory_doc_path();
    let doc = fs::read_to_string(&doc_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", doc_path.display()));

    for signal in fixture["signals"]
        .as_array()
        .expect("fixture signals should be an array")
    {
        let signal_id = signal["signal_id"]
            .as_str()
            .expect("signal_id should be a string");
        assert!(
            doc.contains(signal_id),
            "inventory doc omits signal_id {signal_id}"
        );
    }

    for gap in fixture["gap_map"]
        .as_array()
        .expect("fixture gap_map should be an array")
    {
        let gap_id = gap["gap_id"].as_str().expect("gap_id should be a string");
        assert!(doc.contains(gap_id), "inventory doc omits gap_id {gap_id}");
    }
}

#[test]
fn inventory_source_refs_are_existing_repo_relative_paths() {
    let root = workspace_root();
    let fixture = complete_fixture();
    for signal in fixture["signals"]
        .as_array()
        .expect("fixture signals should be an array")
    {
        let signal_id = signal["signal_id"]
            .as_str()
            .expect("signal_id should be a string");
        let source_refs = signal["source_refs"]
            .as_array()
            .expect("source_refs should be an array");
        assert!(
            !source_refs.is_empty(),
            "signal {signal_id} must cite at least one source"
        );
        for source_ref in source_refs {
            let rel = source_ref["path"]
                .as_str()
                .expect("source_ref.path should be a string");
            let rel_path = Path::new(rel);
            assert!(
                !rel_path.is_absolute(),
                "signal {signal_id} source path must be repo-relative: {rel}"
            );
            assert!(
                !rel_path
                    .components()
                    .any(|component| { matches!(component, std::path::Component::ParentDir) }),
                "signal {signal_id} source path must not escape repo root: {rel}"
            );
            let absolute = root.join(rel_path);
            assert!(
                absolute.exists(),
                "signal {signal_id} source path does not exist: {rel}"
            );
        }
    }
}

#[test]
fn inventory_gap_map_covers_required_capacity_gaps() {
    let fixture = complete_fixture();
    let actual: BTreeSet<&str> = fixture["gap_map"]
        .as_array()
        .expect("gap_map should be an array")
        .iter()
        .map(|gap| gap["gap_id"].as_str().expect("gap_id should be a string"))
        .collect();
    let expected: BTreeSet<&str> = REQUIRED_GAPS.into_iter().collect();
    assert_eq!(
        actual, expected,
        "gap_map must cover the exact ft-b94bx.1 required gap set"
    );
}

#[test]
fn inventory_keeps_privacy_contract_no_raw_pane_content() {
    let fixture = complete_fixture();
    assert_eq!(
        fixture["raw_pane_content_stored"],
        Value::Bool(false),
        "inventory fixture must explicitly reject raw pane content storage"
    );

    let mut scanned = serde_json::to_string(&fixture).expect("fixture serializes");
    scanned.push_str(
        &fs::read_to_string(inventory_doc_path()).expect("inventory doc should be readable"),
    );

    let forbidden = [
        concat!("Bearer ", "ft-b94bx-", "private-token"),
        concat!("Cookie: ", "ft_session=pri", "vate"),
        concat!("PROMPT", "_BODY:"),
        concat!("raw pane ", "excerpt with secret"),
    ];
    for sentinel in forbidden {
        assert!(
            !scanned.contains(sentinel),
            "inventory leaked raw-content sentinel {sentinel}"
        );
    }

    let postures: BTreeSet<&str> = fixture["signals"]
        .as_array()
        .expect("signals should be an array")
        .iter()
        .map(|signal| {
            signal["privacy_posture"]
                .as_str()
                .expect("privacy_posture should be a string")
        })
        .collect();
    assert!(
        postures.contains("no_raw_content"),
        "at least one signal must assert no_raw_content posture"
    );
}

#[test]
fn inventory_artifact_paths_match_checked_in_files() {
    let root = workspace_root();
    let fixture = complete_fixture();
    for rel in string_array(&fixture, "artifact_paths") {
        let path = root.join(rel);
        assert!(path.exists(), "artifact path does not exist: {rel}");
    }
}
