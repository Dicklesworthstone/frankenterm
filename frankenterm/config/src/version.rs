use std::sync::OnceLock;

static VERSION: OnceLock<&'static str> = OnceLock::new();
static TRIPLE: OnceLock<&'static str> = OnceLock::new();

pub fn assign_version_info(version: &'static str, triple: &'static str) {
    VERSION.set(version).unwrap();
    TRIPLE.set(triple).unwrap();
}

pub fn wezterm_version() -> &'static str {
    VERSION
        .get()
        .unwrap_or(&"someone forgot to call assign_version_info")
}

pub fn wezterm_target_triple() -> &'static str {
    TRIPLE
        .get()
        .unwrap_or(&"someone forgot to call assign_version_info")
}

pub fn running_under_wsl() -> bool {
    #[cfg(unix)]
    unsafe {
        let mut name: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut name) == 0 {
            // WSL1 names "Microsoft" in the kernel version; WSL2 reports
            // lowercase "microsoft" in the release instead, which the old
            // case-sensitive version check missed (upstream WezTerm eeb809729).
            let version = std::ffi::CStr::from_ptr(name.version.as_ptr()).to_string_lossy();
            let release = std::ffi::CStr::from_ptr(name.release.as_ptr()).to_string_lossy();
            return names_wsl_kernel(&version, &release);
        }
    };

    false
}

/// Whether a `uname` version/release pair names a WSL kernel.
#[cfg(any(unix, test))]
fn names_wsl_kernel(version: &str, release: &str) -> bool {
    version.to_ascii_lowercase().contains("microsoft")
        || release.to_ascii_lowercase().contains("microsoft")
}

#[cfg(test)]
mod wsl_tests {
    use super::names_wsl_kernel;

    #[test]
    fn wsl1_and_wsl2_kernels_are_both_detected() {
        // WSL1
        assert!(names_wsl_kernel(
            "#1-Microsoft Mon Jan 01 00:00:00 PST 2024",
            "4.4.0-19041-Microsoft"
        ));
        // WSL2: lowercase, release only
        assert!(names_wsl_kernel(
            "#1 SMP Fri Jan 27 02:56:13 UTC 2023",
            "5.15.90.1-microsoft-standard-WSL2"
        ));
        assert!(!names_wsl_kernel(
            "#1 SMP PREEMPT_DYNAMIC",
            "6.8.0-45-generic"
        ));
    }
}
