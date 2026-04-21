use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Default, Debug, Clone)]
struct PackageInfo {
    name: Option<String>,
    version: Option<String>,
    source: Option<String>,
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let workspace_root = Path::new(&manifest_dir).join("..").join("..");
    let lock_path = workspace_root.join("Cargo.lock");
    let vendored_root = workspace_root.join("frankenterm");

    println!("cargo:rerun-if-changed={}", lock_path.display());
    // Path-dep layout: refresh metadata when vendored mux/term manifests change.
    let mux_manifest = vendored_root.join("mux").join("Cargo.toml");
    let term_manifest = vendored_root.join("term").join("Cargo.toml");
    println!("cargo:rerun-if-changed={}", mux_manifest.display());
    println!("cargo:rerun-if-changed={}", term_manifest.display());

    let packages = fs::read_to_string(&lock_path)
        .map(|contents| parse_lock_packages(&contents))
        .unwrap_or_default();
    let mux = packages.get("mux");
    let term = packages.get("frankenterm-term");

    let (commit, version, source) = select_vendored_metadata(mux, term, &vendored_root);

    if let Some(commit) = commit {
        println!("cargo:rustc-env=FT_WEZTERM_VENDORED_REV={commit}");
    }
    if let Some(version) = version {
        println!("cargo:rustc-env=FT_WEZTERM_VENDORED_VERSION={version}");
    }
    if let Some(source) = source {
        println!("cargo:rustc-env=FT_WEZTERM_VENDORED_SOURCE={source}");
    }

    let rustc_ver = Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=FT_RUSTC_VERSION={rustc_ver}");
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
    vendored_root: &Path,
) -> (Option<String>, Option<String>, Option<String>) {
    // Primary path: Cargo.lock has a `git+...#<hash>` source (fork/override layouts).
    let mux_commit = mux.and_then(|pkg| pkg.source.as_deref().and_then(extract_commit));
    let term_commit = term.and_then(|pkg| pkg.source.as_deref().and_then(extract_commit));

    let lock_commit = mux_commit.clone().or_else(|| term_commit.clone());
    let version = mux
        .and_then(|pkg| pkg.version.clone())
        .or_else(|| term.and_then(|pkg| pkg.version.clone()));
    let source = mux
        .and_then(|pkg| pkg.source.clone())
        .or_else(|| term.and_then(|pkg| pkg.source.clone()));

    if lock_commit.is_some() {
        // Prefer mux deterministically when mux/term disagree.
        if mux_commit.is_some() && term_commit.is_some() && mux_commit != term_commit {
            return (mux_commit, version, source);
        }
        return (lock_commit, version, source);
    }

    // Fallback: default workspace layout ships `mux`/`frankenterm-term` as path
    // dependencies, so Cargo.lock has no `source` field. Resolve the vendored
    // commit by probing the vendored tree itself. If the workspace isn't a git
    // checkout (cargo-install tarball, release bundle), emit a deterministic
    // sentinel derived from the vendored manifests so `vendored.rs` doesn't
    // trip its fail-closed "commit not recorded" guard.
    let commit = git_head_commit(vendored_root).or_else(|| source_hash_sentinel(vendored_root));
    let resolved_source = source.or_else(|| Some(format!("path+{}", vendored_root.display())));

    // When falling back to the filesystem, prefer the mux manifest version
    // (matches the `mux` lockfile entry name lookup above). This keeps the
    // FT_WEZTERM_VENDORED_VERSION envelope meaningful on path-dep installs.
    let resolved_version = version.or_else(|| read_manifest_version(&vendored_root.join("mux")));

    (commit, resolved_version, resolved_source)
}

fn extract_commit(source: &str) -> Option<String> {
    let hash = source.split('#').nth(1)?;
    if hash.is_empty() {
        None
    } else {
        Some(hash.to_string())
    }
}

fn git_head_commit(dir: &Path) -> Option<String> {
    if !dir.join(".git").exists() && !dir.join("../.git").exists() {
        // Fast rejection: neither the vendored tree nor its parent is a git
        // checkout. Still try `git rev-parse` below in case the user uses a
        // worktree or unconventional `.git` location — but short-circuit the
        // usual cargo-install tarball case to avoid spawning git needlessly.
        if !has_git_ancestor(dir) {
            return None;
        }
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if commit.is_empty() || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        None
    } else {
        Some(commit)
    }
}

fn has_git_ancestor(dir: &Path) -> bool {
    let mut cursor: Option<PathBuf> = Some(dir.to_path_buf());
    while let Some(path) = cursor {
        if path.join(".git").exists() {
            return true;
        }
        cursor = path.parent().map(Path::to_path_buf);
    }
    false
}

fn read_manifest_version(crate_dir: &Path) -> Option<String> {
    let manifest = fs::read_to_string(crate_dir.join("Cargo.toml")).ok()?;
    let mut in_package = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if in_package {
            if let Some(v) = parse_kv(trimmed, "version") {
                return Some(v);
            }
        }
    }
    None
}

/// Deterministic sentinel for non-git vendored trees (cargo-install tarball,
/// shipped source archive). Hashes the mux/term manifests so the sentinel
/// flips whenever the vendored surface changes. The `local-path-build-` prefix
/// guarantees `commit_matches` in vendored.rs never false-matches a real SHA.
fn source_hash_sentinel(vendored_root: &Path) -> Option<String> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mux_manifest = fs::read_to_string(vendored_root.join("mux").join("Cargo.toml")).ok()?;
    let term_manifest = fs::read_to_string(vendored_root.join("term").join("Cargo.toml")).ok()?;
    let mut hasher = DefaultHasher::new();
    mux_manifest.hash(&mut hasher);
    term_manifest.hash(&mut hasher);
    let digest = hasher.finish();
    Some(format!("local-path-build-{digest:016x}"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Build-script harness (ft-ibtjf, gh-52). Not wired into `cargo test` because
// build.rs isn't compiled into the crate's test binary; run manually via
// `rustc --test build.rs && ./build` from the crate root when validating.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    fn pkg_with_source(name: &str, version: &str, source: Option<&str>) -> PackageInfo {
        PackageInfo {
            name: Some(name.to_string()),
            version: Some(version.to_string()),
            source: source.map(str::to_string),
        }
    }

    #[test]
    fn extract_commit_handles_git_source_urls() {
        let src = "git+https://github.com/wez/wezterm#05343b387085";
        assert_eq!(extract_commit(src).as_deref(), Some("05343b387085"));
    }

    #[test]
    fn extract_commit_returns_none_for_empty_hash() {
        assert!(extract_commit("git+https://github.com/wez/wezterm#").is_none());
        assert!(extract_commit("no-hash").is_none());
    }

    // Case: git+hash in Cargo.lock (fork/override layout).
    #[test]
    fn select_from_git_source_in_lock() {
        let mux = pkg_with_source(
            "mux",
            "0.1.0",
            Some("git+https://github.com/wez/wezterm#abcdef1234567890"),
        );
        let term = pkg_with_source(
            "frankenterm-term",
            "0.1.0",
            Some("git+https://github.com/wez/wezterm#abcdef1234567890"),
        );
        let (commit, version, source) = select_vendored_metadata(
            Some(&mux),
            Some(&term),
            Path::new("/nonexistent/frankenterm"),
        );
        assert_eq!(commit.as_deref(), Some("abcdef1234567890"));
        assert_eq!(version.as_deref(), Some("0.1.0"));
        assert!(source.as_deref().unwrap().starts_with("git+"));
    }

    // Case: mux/term disagreement → deterministic mux preference.
    #[test]
    fn select_prefers_mux_on_disagreement() {
        let mux = pkg_with_source("mux", "0.1.0", Some("git+foo#aaaaaaaa"));
        let term = pkg_with_source("frankenterm-term", "0.1.0", Some("git+foo#bbbbbbbb"));
        let (commit, _, _) = select_vendored_metadata(
            Some(&mux),
            Some(&term),
            Path::new("/nonexistent/frankenterm"),
        );
        assert_eq!(commit.as_deref(), Some("aaaaaaaa"));
    }

    // Case: path-dep + git checkout → HEAD commit.
    #[test]
    fn select_path_dep_in_git_checkout_uses_head() {
        // Use the actual project vendored tree as fixture — it's a git repo.
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
        let vendored = workspace.join("frankenterm");
        if !vendored.join("mux").join("Cargo.toml").exists() {
            eprintln!("skip: vendored tree not present in this checkout");
            return;
        }

        let mux = pkg_with_source("mux", "0.1.0", None);
        let term = pkg_with_source("frankenterm-term", "0.1.0", None);
        let (commit, version, source) =
            select_vendored_metadata(Some(&mux), Some(&term), &vendored);
        let commit = commit.expect("git rev-parse HEAD should succeed in checkout");
        assert_eq!(commit.len(), 40, "expected full SHA-1, got: {commit}");
        assert!(commit.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(version.as_deref(), Some("0.1.0"));
        assert!(source.is_some());
    }

    // Case: path-dep + non-git tree → deterministic sentinel.
    #[test]
    fn select_path_dep_non_git_uses_sentinel() {
        let tmp = std::env::temp_dir().join("ft_ibtjf_harness_non_git");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("mux")).unwrap();
        fs::create_dir_all(tmp.join("term")).unwrap();
        fs::write(
            tmp.join("mux").join("Cargo.toml"),
            "[package]\nname = \"mux\"\nversion = \"9.9.9\"\n",
        )
        .unwrap();
        fs::write(
            tmp.join("term").join("Cargo.toml"),
            "[package]\nname = \"frankenterm-term\"\nversion = \"9.9.9\"\n",
        )
        .unwrap();

        let mux = pkg_with_source("mux", "9.9.9", None);
        let term = pkg_with_source("frankenterm-term", "9.9.9", None);
        let (commit, version, source) = select_vendored_metadata(Some(&mux), Some(&term), &tmp);
        let commit = commit.expect("sentinel must be emitted");
        assert!(
            commit.starts_with("local-path-build-"),
            "sentinel prefix guarantees commit_matches() never false-matches a real SHA: {commit}"
        );
        assert_eq!(version.as_deref(), Some("9.9.9"));
        assert!(source.as_deref().unwrap().starts_with("path+"));

        // Determinism: same inputs → same digest.
        let (commit2, _, _) = select_vendored_metadata(Some(&mux), Some(&term), &tmp);
        assert_eq!(commit, commit2.unwrap());

        let _ = fs::remove_dir_all(&tmp);
    }

    // Case: nothing in lock, no vendored tree → (None, None, None) preserves
    // the fail-closed guard in vendored.rs for genuinely-missing metadata.
    #[test]
    fn select_missing_both_returns_none() {
        let (commit, _version, _source) =
            select_vendored_metadata(None, None, Path::new("/nonexistent/ft-ibtjf"));
        assert!(commit.is_none());
    }

    #[test]
    fn parse_lock_packages_extracts_name_version_source() {
        let lock = r#"
[[package]]
name = "mux"
version = "0.1.0"

[[package]]
name = "frankenterm-term"
version = "0.1.0"
source = "git+https://github.com/wez/wezterm#deadbeef"
"#;
        let packages = parse_lock_packages(lock);
        let mux = packages.get("mux").unwrap();
        assert_eq!(mux.version.as_deref(), Some("0.1.0"));
        assert!(mux.source.is_none());
        let term = packages.get("frankenterm-term").unwrap();
        assert_eq!(
            term.source.as_deref(),
            Some("git+https://github.com/wez/wezterm#deadbeef")
        );
    }
}
