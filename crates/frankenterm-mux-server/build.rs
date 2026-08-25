use frankenterm_build_identity::{
    AtomicComponentRole, emit_cargo_atomic_component_marker,
};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    emit_cargo_atomic_component_marker(AtomicComponentRole::FrankenTermMuxServer)
        .unwrap_or_else(|error| {
            panic!("cannot embed FrankenTerm mux-server atomic component identity: {error}")
        });
}
