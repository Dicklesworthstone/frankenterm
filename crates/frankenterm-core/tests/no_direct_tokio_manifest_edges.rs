//! ft-xxfwy.70 — Regression guard: no first-party Cargo manifest declares a
//! direct `tokio` dependency edge of any kind.
//!
//! `deny.toml`'s `[bans]` rule works on the resolved package graph, where
//! Tokio legitimately arrives through third-party parents (reqwest, hyper,
//! h2, ...) listed as `wrappers`. This guard complements it at Cargo-test
//! time by inspecting the *declared* first-party edges directly, so it needs
//! neither cargo-deny nor a resolved dependency graph.
//!
//! The edges come from `cargo metadata --no-deps`, which reports each
//! first-party package's declarations after Cargo has applied workspace
//! inheritance. That covers every declaration form without parsing TOML by
//! hand: normal, build and dev kinds; `[target.'cfg(..)'.*]` tables;
//! `package = "tokio"` renames (Cargo reports the real package name); and
//! optional dependencies. Purely transitive Tokio is not a declared edge and
//! is intentionally allowed.
//!
//! The fixture tests build small throwaway workspaces covering each form so
//! the guard is shown to fail for the intended edge-policy reason, not just
//! to pass vacuously on the real tree.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Packages that no first-party manifest may declare directly.
const FORBIDDEN_DIRECT_PACKAGES: &[&str] = &["tokio"];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ForbiddenEdge {
    package: String,
    dependency: String,
    kind: String,
    target: Option<String>,
    rename: Option<String>,
    optional: bool,
}

#[derive(Debug, Default)]
struct EdgeScan {
    packages: usize,
    declared_edges: usize,
    forbidden: Vec<ForbiddenEdge>,
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .expect("expected frankenterm-core to live under <workspace>/crates/")
}

fn cargo_metadata_no_deps(manifest_path: &Path) -> serde_json::Value {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
        ])
        .arg("--manifest-path")
        .arg(manifest_path)
        .output()
        .expect("cargo metadata must be runnable from the test harness");
    assert!(
        output.status.success(),
        "cargo metadata failed for {}: {}",
        manifest_path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata emits JSON")
}

fn scan_declared_edges(metadata: &serde_json::Value) -> EdgeScan {
    let packages = metadata["packages"]
        .as_array()
        .expect("cargo metadata has a packages array");
    let mut scan = EdgeScan {
        packages: packages.len(),
        ..EdgeScan::default()
    };
    for package in packages {
        let package_name = package["name"].as_str().expect("package name");
        let dependencies = package["dependencies"]
            .as_array()
            .expect("package dependencies array");
        scan.declared_edges += dependencies.len();
        for dependency in dependencies {
            let name = dependency["name"].as_str().expect("dependency name");
            if !FORBIDDEN_DIRECT_PACKAGES.contains(&name) {
                continue;
            }
            scan.forbidden.push(ForbiddenEdge {
                package: package_name.to_string(),
                dependency: name.to_string(),
                kind: dependency["kind"].as_str().unwrap_or("normal").to_string(),
                target: dependency["target"].as_str().map(str::to_string),
                rename: dependency["rename"].as_str().map(str::to_string),
                optional: dependency["optional"].as_bool().unwrap_or(false),
            });
        }
    }
    scan.forbidden.sort();
    scan
}

/// Every loadable first-party Cargo root: the main workspace (which includes
/// `fuzz`) and the standalone model-checking workspace. The workspace
/// `exclude`d generators under `frankenterm/` are not loadable Cargo roots
/// and `vendor/` holds third-party sources.
fn first_party_manifest_roots(root: &Path) -> Vec<PathBuf> {
    vec![
        root.join("Cargo.toml"),
        root.join("tests/robot_work_atomicity_model/Cargo.toml"),
    ]
}

#[test]
fn first_party_manifests_declare_no_direct_tokio_edges() {
    let root = workspace_root();
    let mut violations = Vec::new();
    let mut packages = 0;
    let mut declared_edges = 0;
    for manifest in first_party_manifest_roots(&root) {
        assert!(manifest.is_file(), "missing {}", manifest.display());
        let scan = scan_declared_edges(&cargo_metadata_no_deps(&manifest));
        eprintln!(
            "no_direct_tokio_manifest_edges: manifest={} packages={} declared_edges={} forbidden={}",
            manifest.strip_prefix(&root).unwrap_or(&manifest).display(),
            scan.packages,
            scan.declared_edges,
            scan.forbidden.len()
        );
        packages += scan.packages;
        declared_edges += scan.declared_edges;
        violations.extend(scan.forbidden);
    }
    // Non-vacuity: the main workspace alone has dozens of members and many
    // hundreds of declared edges.
    assert!(
        packages >= 50,
        "only {packages} first-party packages scanned"
    );
    assert!(
        declared_edges >= 500,
        "only {declared_edges} declared edges scanned"
    );
    assert!(
        violations.is_empty(),
        "first-party manifests declare forbidden direct Tokio edges; use runtime_async \
         (asupersync) instead: {violations:#?}"
    );
}

fn write_file(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().expect("fixture file has a parent")).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn fixture_member(root: &Path, name: &str, dependency_tables: &str) {
    write_file(
        &root.join(name).join("Cargo.toml"),
        &format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n{dependency_tables}"
        ),
    );
    write_file(&root.join(name).join("src/lib.rs"), "");
}

fn edge(
    package: &str,
    kind: &str,
    target: Option<&str>,
    rename: Option<&str>,
    optional: bool,
) -> ForbiddenEdge {
    ForbiddenEdge {
        package: package.to_string(),
        dependency: "tokio".to_string(),
        kind: kind.to_string(),
        target: target.map(str::to_string),
        rename: rename.map(str::to_string),
        optional,
    }
}

#[test]
fn guard_flags_every_direct_edge_form_and_allows_transitive_tokio() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let members = [
        "normal_edge",
        "workspace_dev_edge",
        "target_build_edge",
        "renamed_edge",
        "optional_edge",
        "direct_beside_allowed_parent",
        "transitive_only",
        "similar_name_only",
    ];
    write_file(
        &root.join("Cargo.toml"),
        &format!(
            "[workspace]\nresolver = \"3\"\nmembers = [{}]\n\n[workspace.dependencies]\ntokio = {{ version = \"1\", features = [\"rt\"] }}\n",
            members
                .iter()
                .map(|member| format!("\"{member}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );
    fixture_member(root, "normal_edge", "[dependencies]\ntokio = \"1\"\n");
    fixture_member(
        root,
        "workspace_dev_edge",
        "[dev-dependencies]\ntokio = { workspace = true, features = [\"test-util\"] }\n",
    );
    fixture_member(
        root,
        "target_build_edge",
        "[target.'cfg(unix)'.build-dependencies]\ntokio = \"1\"\n",
    );
    fixture_member(
        root,
        "renamed_edge",
        "[dependencies]\nruntime = { package = \"tokio\", version = \"1\" }\n",
    );
    fixture_member(
        root,
        "optional_edge",
        "[dependencies]\ntokio = { version = \"1\", optional = true }\n",
    );
    // criterion (with async_tokio) reaches Tokio transitively; depending on
    // such a parent must not excuse a direct edge declared beside it.
    fixture_member(
        root,
        "direct_beside_allowed_parent",
        "[dev-dependencies]\ncriterion = { version = \"0.8\", features = [\"async_tokio\"] }\ntokio = \"1\"\n",
    );
    fixture_member(
        root,
        "transitive_only",
        "[dev-dependencies]\ncriterion = { version = \"0.8\", features = [\"async_tokio\"] }\n",
    );
    fixture_member(
        root,
        "similar_name_only",
        "[dependencies]\ntokio-util = \"0.7\"\n",
    );

    let scan = scan_declared_edges(&cargo_metadata_no_deps(&root.join("Cargo.toml")));

    assert_eq!(scan.packages, members.len());
    assert_eq!(
        scan.forbidden,
        vec![
            edge("direct_beside_allowed_parent", "dev", None, None, false),
            edge("normal_edge", "normal", None, None, false),
            edge("optional_edge", "normal", None, None, true),
            edge("renamed_edge", "normal", None, Some("runtime"), false),
            edge("target_build_edge", "build", Some("cfg(unix)"), None, false),
            edge("workspace_dev_edge", "dev", None, None, false),
        ]
    );
}
