// Build script: captures git commit, build timestamp, rustc version, target triple,
// and enabled features as compile-time environment variables for `ft --version`.
//
// Reproducibility: honors SOURCE_DATE_EPOCH (https://reproducible-builds.org/specs/source-date-epoch/).
// When set, FT_BUILD_TS is derived from that epoch. Tracked source changes still set
// FT_GIT_DIRTY so reproducibility metadata cannot suppress candidate-integrity evidence.

use std::process::Command;

fn main() {
    // Treat SOURCE_DATE_EPOCH="" (exported but empty) the same as unset. The spec requires a
    // non-negative integer; an empty string is not one, and honoring it would produce a
    // meaningless `built: epoch:` line and silently suppress the git-dirty check.
    let source_date_epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .filter(|s| !s.trim().is_empty());

    // Git commit hash
    let git_hash = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=FT_GIT_HASH={git_hash}");

    // Git dirty flag: tracked changes invalidate candidate identity even under
    // SOURCE_DATE_EPOCH. Untracked artifacts are excluded so they cannot perturb
    // reproducible metadata. If Git cannot establish cleanliness, fail closed.
    let git_dirty = Command::new("git")
        .args(["diff-index", "--quiet", "HEAD", "--"])
        .status()
        .map_or(true, |status| !status.success());
    if git_dirty {
        println!("cargo:rustc-env=FT_GIT_DIRTY=+dirty");
    } else {
        println!("cargo:rustc-env=FT_GIT_DIRTY=");
    }

    // Build timestamp (UTC). Under SOURCE_DATE_EPOCH, format that epoch; otherwise read wall clock.
    let build_ts = match source_date_epoch.as_deref() {
        Some(epoch) => format_epoch_utc(epoch),
        None => Command::new("date")
            .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".to_string()),
    };
    println!("cargo:rustc-env=FT_BUILD_TS={build_ts}");

    // Rustc version
    let rustc_ver = Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=FT_RUSTC_VERSION={rustc_ver}");

    // Enabled features (collected via cfg)
    let mut features = Vec::new();
    for feat in &[
        "vendored",
        "browser",
        "mcp",
        "web",
        "tui",
        "metrics",
        "distributed",
    ] {
        println!(
            "cargo:rerun-if-env-changed=CARGO_FEATURE_{}",
            feat.to_uppercase()
        );
        if std::env::var(format!("CARGO_FEATURE_{}", feat.to_uppercase())).is_ok() {
            features.push(*feat);
        }
    }
    let feature_list = if features.is_empty() {
        "none".to_string()
    } else {
        features.join(",")
    };
    println!("cargo:rustc-env=FT_FEATURES={feature_list}");

    // Target triple
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=FT_TARGET={target}");

    emit_atomic_component_marker("ft");

    // Rerun triggers
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}

/// Embed the package-level identity fence consumed by
/// `scripts/atomic-component-manifest.sh`.  Ordinary development builds carry
/// `unsealed`; release packaging derives and supplies a 64-hex build identity
/// shared by GUI, CLI, and mux-server in one Cargo invocation.
fn emit_atomic_component_marker(component: &str) {
    let build_id = atomic_build_id();
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string());
    let marker = format!(
        "FT_ATOMIC_COMPONENT_IDENTITY_V1:{build_id}:{component}:{target}:{profile}:{version};"
    );
    println!("cargo:rustc-env=FT_ATOMIC_COMPONENT_MARKER={marker}");
    println!("cargo:rerun-if-env-changed=FT_ATOMIC_BUILD_IDENTITY");
}

fn atomic_build_id() -> String {
    match std::env::var("FT_ATOMIC_BUILD_IDENTITY") {
        Ok(value) if is_lower_hex_sha256(&value) => value,
        Ok(value) => panic!(
            "FT_ATOMIC_BUILD_IDENTITY must be exactly 64 lowercase hexadecimal characters; got {value:?}"
        ),
        Err(std::env::VarError::NotPresent) => "unsealed".to_string(),
        Err(std::env::VarError::NotUnicode(_)) => {
            panic!("FT_ATOMIC_BUILD_IDENTITY must be valid UTF-8")
        }
    }
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

/// Format a unix epoch (seconds since 1970-01-01 UTC) as `YYYY-MM-DDTHH:MM:SSZ`.
/// Tries GNU `date -d @<epoch>` and BSD `date -r <epoch>` in order; falls back to
/// `epoch:<n>` so the build never fails for a valid SOURCE_DATE_EPOCH value.
fn format_epoch_utc(epoch: &str) -> String {
    let epoch_at = format!("@{epoch}");
    let gnu_args: [&str; 4] = ["-u", "-d", &epoch_at, "+%Y-%m-%dT%H:%M:%SZ"];
    let bsd_args: [&str; 4] = ["-u", "-r", epoch, "+%Y-%m-%dT%H:%M:%SZ"];
    for args in [&gnu_args[..], &bsd_args[..]] {
        let Ok(out) = Command::new("date").args(args).output() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let ts = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // Sanity: output should start with a 4-digit year. Rejects BSD's garbage-on-unknown-flag
        // and any locale-mangled output.
        if ts.len() >= 4 && ts.as_bytes()[..4].iter().all(u8::is_ascii_digit) {
            return ts;
        }
    }
    format!("epoch:{epoch}")
}
