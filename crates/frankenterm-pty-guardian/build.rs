use frankenterm_build_identity::{
    AtomicBuildIdentity, AtomicComponentIdentityError, AtomicComponentRole,
    emit_cargo_atomic_component_marker, parse_atomic_component_marker,
};

fn main() -> Result<(), AtomicComponentIdentityError> {
    println!("cargo:rerun-if-changed=build.rs");
    let marker = emit_cargo_atomic_component_marker(AtomicComponentRole::FrankenTermPtyGuardian)?;
    // The library is also linked into the mux server. It needs the shared
    // family authority, but must not claim to be a second process component.
    let identity = match parse_atomic_component_marker(
        &marker,
        AtomicComponentRole::FrankenTermPtyGuardian,
    )? {
        AtomicBuildIdentity::UnsealedDevelopment => "unsealed".to_owned(),
        AtomicBuildIdentity::Sealed(identity) => identity.to_string(),
    };
    println!("cargo:rustc-env=FT_GUARDIAN_COMPILED_BUILD_IDENTITY={identity}");
    Ok(())
}
