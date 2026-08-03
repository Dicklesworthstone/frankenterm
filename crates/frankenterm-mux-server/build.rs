fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    emit_atomic_component_marker("frankenterm-mux-server");
}

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
