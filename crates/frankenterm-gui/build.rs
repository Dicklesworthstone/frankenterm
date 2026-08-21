fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Pass the target triple to the binary via cfg
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=FRANKENTERM_TARGET_TRIPLE={target}");
    emit_atomic_component_marker("frankenterm-gui");

    #[cfg(target_os = "macos")]
    {
        // Future: copy Info.plist for macOS app bundle support.
    }
}

fn emit_atomic_component_marker(component: &str) {
    let build_id = atomic_build_id();
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    let profile = atomic_build_profile();
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string());
    let marker = format!(
        "FT_ATOMIC_COMPONENT_IDENTITY_V1:{build_id}:{component}:{target}:{profile}:{version};"
    );
    println!("cargo:rustc-env=FT_ATOMIC_COMPONENT_MARKER={marker}");
    println!("cargo:rerun-if-env-changed=FT_ATOMIC_BUILD_IDENTITY");
    println!("cargo:rerun-if-env-changed=FT_ATOMIC_BUILD_PROFILE");
}

fn atomic_build_profile() -> String {
    match std::env::var("FT_ATOMIC_BUILD_PROFILE") {
        Ok(value) if is_atomic_token(&value) => value,
        Ok(value) => panic!(
            "FT_ATOMIC_BUILD_PROFILE must be a non-empty ASCII build-profile token; got {value:?}"
        ),
        Err(std::env::VarError::NotPresent) => {
            std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string())
        }
        Err(std::env::VarError::NotUnicode(_)) => {
            panic!("FT_ATOMIC_BUILD_PROFILE must be valid UTF-8")
        }
    }
}

fn is_atomic_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
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
