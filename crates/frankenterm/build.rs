// Build script: captures git commit, build timestamp, rustc version, target triple,
// and enabled features as compile-time environment variables for `ft --version`.

use std::process::Command;

fn main() {
    // Git commit hash
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short=9", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=FT_GIT_HASH={git_hash}");

    // Git dirty flag
    let git_dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    if git_dirty {
        println!("cargo:rustc-env=FT_GIT_DIRTY=+dirty");
    } else {
        println!("cargo:rustc-env=FT_GIT_DIRTY=");
    }

    // Build timestamp (UTC)
    let build_ts = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
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

    // Rerun triggers
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");
}
