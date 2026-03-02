fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(target_os = "macos")]
    {
        // Future: copy Info.plist for macOS app bundle support.
        // Once the window crate is vendored (ft-1memj.2), this will handle
        // icon embedding and notification center integration.
    }
}
