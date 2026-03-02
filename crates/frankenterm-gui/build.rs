fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Pass the target triple to the binary via cfg
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=FRANKENTERM_TARGET_TRIPLE={target}");

    #[cfg(target_os = "macos")]
    {
        // Future: copy Info.plist for macOS app bundle support.
    }
}
