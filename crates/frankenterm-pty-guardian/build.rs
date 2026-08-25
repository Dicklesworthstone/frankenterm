use frankenterm_build_identity::{
    AtomicComponentRole, emit_cargo_atomic_component_marker,
};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    emit_cargo_atomic_component_marker(AtomicComponentRole::FrankenTermPtyGuardian)
        .unwrap_or_else(|error| {
            panic!("cannot embed FrankenTerm PTY-guardian atomic component identity: {error}")
        });
}
