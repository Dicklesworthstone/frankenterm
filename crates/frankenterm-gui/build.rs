use frankenterm_build_identity::{
    AtomicComponentIdentityError, AtomicComponentRole, emit_cargo_atomic_component_marker,
};

fn main() -> Result<(), AtomicComponentIdentityError> {
    println!("cargo:rerun-if-changed=build.rs");

    // Pass the target triple to the binary via cfg
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=FRANKENTERM_TARGET_TRIPLE={target}");
    emit_cargo_atomic_component_marker(AtomicComponentRole::FrankenTermGui)?;

    #[cfg(target_os = "macos")]
    {
        // Future: copy Info.plist for macOS app bundle support.
    }
    Ok(())
}
