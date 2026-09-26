use frankenterm_build_identity::{
    AtomicComponentIdentityError, AtomicComponentRole, emit_cargo_atomic_component_marker,
};

fn main() -> Result<(), AtomicComponentIdentityError> {
    println!("cargo:rerun-if-changed=build.rs");

    // Pass the target triple to the binary via cfg
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=FRANKENTERM_TARGET_TRIPLE={target}");
    // The source commit goes into the version string the GUI announces to mux
    // clients, so `ft doctor` and `--version` can tell two builds of the same
    // semver apart (ft's own build script records the same revision).
    println!("cargo:rustc-env=FRANKENTERM_GIT_HASH={}", source_revision());
    emit_cargo_atomic_component_marker(AtomicComponentRole::FrankenTermGui)?;

    #[cfg(target_os = "macos")]
    {
        // Future: copy Info.plist for macOS app bundle support.
    }
    Ok(())
}

/// `git rev-parse HEAD` of the workspace, or the DSR release revision for a
/// gitless release archive, else "unknown".
fn source_revision() -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let git = root.join(".git");
    if git.exists() {
        for name in ["HEAD", "refs", "packed-refs"] {
            let path = git.join(name);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
        if let Some(revision) = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "--verify", "HEAD"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        {
            return revision;
        }
    }
    println!("cargo:rerun-if-env-changed=DSR_RELEASE_GIT_SHA");
    std::env::var("DSR_RELEASE_GIT_SHA").unwrap_or_else(|_| "unknown".to_owned())
}
