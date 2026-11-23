fn main() {
    // A build script runs on the host: cfg!(target_os) would describe the
    // host, so a cross build from macOS to Windows or Linux linked the macOS
    // framework and failed. Cargo reports the target in CARGO_CFG_TARGET_OS.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=framework=UserNotifications");
    }
}
