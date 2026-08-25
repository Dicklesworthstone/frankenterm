use frankenterm_build_identity::{
    AtomicComponentIdentityError, AtomicComponentRole, emit_cargo_atomic_component_marker,
};

fn main() -> Result<(), AtomicComponentIdentityError> {
    println!("cargo:rerun-if-changed=build.rs");
    emit_cargo_atomic_component_marker(AtomicComponentRole::FrankenTermMuxServer)?;
    Ok(())
}
