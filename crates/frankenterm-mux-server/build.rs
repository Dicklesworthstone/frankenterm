use frankenterm_build_identity::{
    AtomicComponentIdentityError, AtomicComponentRole, emit_cargo_atomic_component_marker,
    emit_cargo_source_revision,
};

fn main() -> Result<(), AtomicComponentIdentityError> {
    println!("cargo:rerun-if-changed=build.rs");
    // The commit goes into --version and the version announced to clients.
    emit_cargo_source_revision("FRANKENTERM_GIT_HASH");
    emit_cargo_atomic_component_marker(AtomicComponentRole::FrankenTermMuxServer)?;
    Ok(())
}
