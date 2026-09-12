//! Regression guard for the ft-e87u6.6 attestation closing template.

use std::fs;
use std::path::{Path, PathBuf};

const TEMPLATE_REL_PATH: &str = "docs/release/attestation-bead-closing-template.md";
const CHECKLIST_REL_PATH: &str = "docs/release/attestation-checklist.md";
const RELEASE_GATES_REL_PATH: &str = "scripts/release-gates.sh";
const RELEASE_VERIFIER_REL_PATH: &str = "scripts/release/verify-release.sh";

const REQUIRED_TEMPLATE_LINES: &[&str] = &[
    "Manifest slot category: `<category>`",
    "Artifact path: `<path>` (sha256 `<hash>`)",
    "Build smoke: `bash scripts/attestation-build.sh --version 0.0.0-dev --channel dev --sign unsigned` exit `<code>`",
    "Strict-deferred build: `bash scripts/attestation-build.sh ... --strict-deferred` exit `<code>`",
    "Verify round-trip: `bash scripts/attestation-verify.sh <bundle>` exit `<code>`",
    "Hedge alignment: `cargo test -p frankenterm-core --test readme_hedge_alignment` exit `<code>`",
    "Manifest completeness: `cargo test -p frankenterm-core --test attestation_manifest_completeness` exit `<code>`",
    "RCH artifact bundle: `<path>`",
];

const REQUIRED_FIELD_NAMES: &[&str] = &[
    "Manifest slot category: ",
    "Artifact path: ",
    "Build smoke:",
    "Strict-deferred build:",
    "Verify round-trip:",
    "Hedge alignment:",
    "Manifest completeness:",
    "RCH artifact bundle:",
];

const REQUIRED_BEAD_REFS: &[&str] = &["ft-187kv", "ft-e87u6.4", "ft-e87u6.5"];

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf()
}

fn read_workspace_file(rel_path: &str) -> String {
    let path = workspace_root().join(rel_path);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
}

#[test]
fn attestation_closing_template_contains_required_placeholder_lines() {
    let template = read_workspace_file(TEMPLATE_REL_PATH);

    for required in REQUIRED_TEMPLATE_LINES {
        assert!(
            template.contains(required),
            "attestation closing template is missing required placeholder line: {required}"
        );
    }
}

#[test]
fn attestation_closing_template_field_names_stay_grep_stable() {
    let template = read_workspace_file(TEMPLATE_REL_PATH);

    for required in REQUIRED_FIELD_NAMES {
        assert!(
            template.contains(required),
            "attestation closing template renamed required field name: {required}"
        );
    }
}

#[test]
fn attestation_closing_template_mentions_required_beads() {
    let template = read_workspace_file(TEMPLATE_REL_PATH);

    for bead_id in REQUIRED_BEAD_REFS {
        assert!(
            template.contains(bead_id),
            "attestation closing template must reference sibling bead {bead_id}"
        );
    }
}

#[test]
fn attestation_checklist_points_producing_beads_at_the_template_and_test() {
    let checklist = read_workspace_file(CHECKLIST_REL_PATH);

    for required in [
        "Producing-bead closing convention",
        TEMPLATE_REL_PATH,
        "ft-e87u6.5",
        "attestation_manifest_completeness",
    ] {
        assert!(
            checklist.contains(required),
            "{CHECKLIST_REL_PATH} is missing required closing-convention breadcrumb: {required}"
        );
    }
}

#[test]
fn dsr_attestation_gates_and_authenticated_release_bindings_are_wired() {
    let gates = read_workspace_file(RELEASE_GATES_REL_PATH);
    let dev_gate = gates
        .lines()
        .find(|line| line.starts_with("gate \"attestation dev bundle build+verify\""))
        .expect("DSR quality must register the development attestation gate");
    assert!(dev_gate.contains("scripts/attestation-build.sh"));
    assert!(dev_gate.contains("&& bash scripts/attestation-verify.sh"));
    // Development allows explicitly deferred claims. Publication authenticates
    // a separate operator policy; this wiring test is not release evidence.
    let verifier = read_workspace_file(RELEASE_VERIFIER_REL_PATH);
    for required in [
        "scripts/attestation-verify.sh",
        "--release-policy",
        ".publisher_authenticated == true",
        ".git.commit == $sha",
        ".build.profile == \"release-interactive\"",
        "(.build.targets | sort) == $targets",
        "finish_failed",
    ] {
        assert!(
            verifier.contains(required),
            "{RELEASE_VERIFIER_REL_PATH} is missing release-policy binding: {required}"
        );
    }
}

#[test]
fn windows_release_inventory_requires_the_complete_application_family() {
    let verifier = read_workspace_file(RELEASE_VERIFIER_REL_PATH);
    let marker = "if python3 - \"$ASSETS_DIR/$name\" \"$archive_kind\" \"$manifest_name\" <<'PY'\n";
    let inventory = verifier
        .split_once(marker)
        .expect("production archive inventory validator")
        .1
        .split_once("\nPY\n")
        .expect("inventory validator terminator")
        .0;
    let dir = tempfile::tempdir().expect("inventory fixtures");
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            r#"
import pathlib, stat, sys, zipfile
root = pathlib.Path(sys.argv[1])
validator = compile(sys.argv[2], 'production-release-inventory', 'exec')
manifest = 'ft-windows-amd64.component-manifest.json'
required = ['ft.exe', 'frankenterm-mux-server.exe', 'frankenterm-pty-guardian.exe',
            'frankenterm-gui.exe', 'verify-components.sh', manifest]
cases = [('complete', required, True)]
cases += [('missing-' + str(i), required[:i] + required[i+1:], False)
          for i in range(len(required))]
cases += [('extra', required + ['unexpected.exe'], False),
          ('duplicate', required + ['ft.exe'], False),
          ('symlink', required, False), ('fifo', required, False),
          ('directory', required + ['extra/'], False)]
for name, names, accepted in cases:
    path = root / (name + '.zip')
    with zipfile.ZipFile(path, 'w') as archive:
        for member in names:
            info = zipfile.ZipInfo(member)
            if name in ('symlink', 'fifo') and member == 'ft.exe':
                info.create_system = 3
                kind = stat.S_IFLNK if name == 'symlink' else stat.S_IFIFO
                info.external_attr = (kind | 0o777) << 16
            archive.writestr(info, b'fixture')
    sys.argv = ['inventory', str(path), 'process-zip', manifest]
    try:
        exec(validator, {})
    except SystemExit as error:
        if accepted:
            raise AssertionError((name, error)) from error
    else:
        assert accepted, name + ' unexpectedly accepted'
print('WINDOWS_INVENTORY_CONTROLS_PASSED', len(cases))
"#,
        ])
        .arg(dir.path())
        .arg(inventory)
        .output()
        .expect("run production Python inventory validator");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("WINDOWS_INVENTORY_CONTROLS_PASSED 12")
    );
}
