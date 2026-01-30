use std::env;
use std::fs;
use std::path::Path;

#[derive(Default, Debug, Clone)]
struct PackageInfo {
    name: Option<String>,
    version: Option<String>,
    source: Option<String>,
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let lock_path = Path::new(&manifest_dir)
        .join("..")
        .join("..")
        .join("Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock_path.display());

    if let Ok(lock_contents) = fs::read_to_string(&lock_path) {
        let packages = parse_lock_packages(&lock_contents);
        let mux = packages.get("mux");
        let term = packages.get("wezterm-term");

        let (commit, version, source) = select_vendored_metadata(mux, term);

        if let Some(commit) = commit {
            println!("cargo:rustc-env=WA_WEZTERM_VENDORED_REV={commit}");
        }
        if let Some(version) = version {
            println!("cargo:rustc-env=WA_WEZTERM_VENDORED_VERSION={version}");
        }
        if let Some(source) = source {
            println!("cargo:rustc-env=WA_WEZTERM_VENDORED_SOURCE={source}");
        }
    }
}

fn parse_lock_packages(contents: &str) -> std::collections::HashMap<String, PackageInfo> {
    let mut packages = std::collections::HashMap::new();
    let mut current = PackageInfo::default();

    for line in contents.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            commit_package(&mut packages, &current);
            current = PackageInfo::default();
            continue;
        }

        if let Some(value) = parse_kv(line, "name") {
            current.name = Some(value);
        } else if let Some(value) = parse_kv(line, "version") {
            current.version = Some(value);
        } else if let Some(value) = parse_kv(line, "source") {
            current.source = Some(value);
        }
    }

    commit_package(&mut packages, &current);
    packages
}

fn commit_package(map: &mut std::collections::HashMap<String, PackageInfo>, pkg: &PackageInfo) {
    if let Some(name) = pkg.name.as_ref() {
        map.insert(name.clone(), pkg.clone());
    }
}

fn parse_kv(line: &str, key: &str) -> Option<String> {
    let prefix = format!("{key} = ");
    if line.starts_with(&prefix) {
        let value = line[prefix.len()..].trim();
        return Some(trim_quotes(value));
    }
    None
}

fn trim_quotes(value: &str) -> String {
    value.trim_matches('"').to_string()
}

fn select_vendored_metadata(
    mux: Option<&PackageInfo>,
    term: Option<&PackageInfo>,
) -> (Option<String>, Option<String>, Option<String>) {
    let mux_commit = mux.and_then(|pkg| pkg.source.as_deref().and_then(extract_commit));
    let term_commit = term.and_then(|pkg| pkg.source.as_deref().and_then(extract_commit));

    let commit = mux_commit.clone().or_else(|| term_commit.clone());
    let version = mux
        .and_then(|pkg| pkg.version.clone())
        .or_else(|| term.and_then(|pkg| pkg.version.clone()));
    let source = mux
        .and_then(|pkg| pkg.source.clone())
        .or_else(|| term.and_then(|pkg| pkg.source.clone()));

    // If commits differ, prefer mux but keep deterministic choice.
    if mux_commit.is_some() && term_commit.is_some() && mux_commit != term_commit {
        if let Some(commit) = mux_commit {
            return (Some(commit), version, source);
        }
    }

    (commit, version, source)
}

fn extract_commit(source: &str) -> Option<String> {
    let hash = source.split('#').nth(1)?;
    if hash.is_empty() {
        None
    } else {
        Some(hash.to_string())
    }
}
