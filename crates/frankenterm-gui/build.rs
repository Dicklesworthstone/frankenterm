use frankenterm_build_identity::{
    AtomicComponentIdentityError, AtomicComponentRole, emit_cargo_atomic_component_marker,
    emit_cargo_source_revision,
};

fn main() -> Result<(), AtomicComponentIdentityError> {
    println!("cargo:rerun-if-changed=build.rs");

    // Pass the target triple to the binary via cfg
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=FRANKENTERM_TARGET_TRIPLE={target}");
    // The source commit goes into the version string the GUI announces to mux
    // clients, so `ft doctor` and `--version` can tell two builds of the same
    // semver apart (ft's own build script records the same revision).
    emit_cargo_source_revision("FRANKENTERM_GIT_HASH");
    emit_cargo_atomic_component_marker(AtomicComponentRole::FrankenTermGui)?;

    #[cfg(target_os = "macos")]
    {
        // Future: copy Info.plist for macOS app bundle support.
    }
    Ok(())
}
