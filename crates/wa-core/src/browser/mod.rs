//! Browser automation scaffolding for Playwright-based auth flows.
//!
//! Provides lazy Playwright initialization, profile directory management,
//! and safe logging for browser automation tasks.
//!
//! # Architecture
//!
//! ```text
//! BrowserConfig (headless, profiles dir, timeouts)
//!       │
//!       ▼
//! BrowserContext (lazy init, profile isolation)
//!       │
//!       ▼
//! Playwright CLI (subprocess: npx playwright ...)
//! ```
//!
//! # Profiles
//!
//! Browser profiles are stored under the data directory:
//! ```text
//! <data_dir>/browser_profiles/<service>/<account>/
//!   ├── Default/          # Chromium profile data
//!   └── .wa_profile.json  # wa metadata
//! ```
//!
//! Each service+account pair gets an isolated browser profile to prevent
//! cookie/session cross-contamination.
//!
//! # Safety
//!
//! - Device codes, tokens, and secrets are NEVER logged.
//! - Profile paths are logged, but not their contents.
//! - All browser operations are behind the `browser` feature flag.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, StorageError};

pub mod openai_device;

// =============================================================================
// Configuration
// =============================================================================

/// Browser automation configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserConfig {
    /// Run browser in headless mode (default: false for early development).
    pub headless: bool,

    /// Navigation timeout in milliseconds (default: 30s).
    pub navigation_timeout_ms: u64,

    /// Page load timeout in milliseconds (default: 60s).
    pub page_load_timeout_ms: u64,

    /// Browser type to use (default: "chromium").
    pub browser_type: String,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            headless: false,
            navigation_timeout_ms: 30_000,
            page_load_timeout_ms: 60_000,
            browser_type: "chromium".to_string(),
        }
    }
}

// =============================================================================
// Profile Management
// =============================================================================

/// Resolved browser profile directory for a service+account pair.
#[derive(Debug, Clone)]
pub struct BrowserProfile {
    /// Root profiles directory (e.g. `~/.local/share/wa/browser_profiles`)
    pub profiles_root: PathBuf,
    /// Service identifier (e.g. "openai", "anthropic", "google")
    pub service: String,
    /// Account identifier (e.g. account name or hash)
    pub account: String,
}

impl BrowserProfile {
    /// Create a new profile reference.
    ///
    /// Does NOT create the directory on disk — call `ensure_dir()` for that.
    #[must_use]
    pub fn new(profiles_root: impl Into<PathBuf>, service: &str, account: &str) -> Self {
        Self {
            profiles_root: profiles_root.into(),
            service: sanitize_path_component(service),
            account: sanitize_path_component(account),
        }
    }

    /// Full path to this profile's directory.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.profiles_root
            .join(&self.service)
            .join(&self.account)
    }

    /// Ensure the profile directory exists on disk.
    pub fn ensure_dir(&self) -> Result<PathBuf> {
        let dir = self.path();
        std::fs::create_dir_all(&dir).map_err(|e| {
            StorageError::Database(format!(
                "Failed to create browser profile directory {}: {e}",
                dir.display()
            ))
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(&dir, perms).map_err(|e| {
                StorageError::Database(format!(
                    "Failed to set permissions on browser profile {}: {e}",
                    dir.display()
                ))
            })?;
        }

        tracing::debug!(
            profile_dir = %dir.display(),
            service = %self.service,
            account = %self.account,
            "Browser profile directory ensured"
        );

        Ok(dir)
    }

    /// Check if this profile directory exists on disk.
    #[must_use]
    pub fn exists(&self) -> bool {
        self.path().is_dir()
    }
}

/// Resolve the profiles root directory from the data directory.
///
/// Returns `<data_dir>/browser_profiles`.
#[must_use]
pub fn profiles_root_from_data_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("browser_profiles")
}

/// Sanitize a string for use as a filesystem path component.
///
/// Replaces any character that is not alphanumeric, `-`, `_`, or `.`
/// with `_`. This prevents path traversal and special-character issues.
#[must_use]
fn sanitize_path_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// =============================================================================
// Browser Context (lazy initialization)
// =============================================================================

/// Status of the browser automation runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserStatus {
    /// Not yet initialized.
    NotInitialized,
    /// Ready to use.
    Ready,
    /// Failed to initialize.
    Failed(String),
}

/// Browser automation context with lazy initialization.
///
/// The actual Playwright process is not started until `ensure_ready()` is called.
/// This avoids unnecessary overhead when browser features are not used.
pub struct BrowserContext {
    config: BrowserConfig,
    profiles_root: PathBuf,
    status: BrowserStatus,
}

impl BrowserContext {
    /// Create a new browser context (does NOT start Playwright).
    #[must_use]
    pub fn new(config: BrowserConfig, data_dir: &Path) -> Self {
        Self {
            config,
            profiles_root: profiles_root_from_data_dir(data_dir),
            status: BrowserStatus::NotInitialized,
        }
    }

    /// Current browser status.
    #[must_use]
    pub fn status(&self) -> &BrowserStatus {
        &self.status
    }

    /// Current configuration.
    #[must_use]
    pub fn config(&self) -> &BrowserConfig {
        &self.config
    }

    /// Profiles root directory.
    #[must_use]
    pub fn profiles_root(&self) -> &Path {
        &self.profiles_root
    }

    /// Get a profile reference for a service+account.
    #[must_use]
    pub fn profile(&self, service: &str, account: &str) -> BrowserProfile {
        BrowserProfile::new(&self.profiles_root, service, account)
    }

    /// Lazily initialize the browser automation runtime.
    ///
    /// Checks that the Playwright CLI is available and the profiles root
    /// directory can be created. Does NOT launch a browser — that happens
    /// on first use.
    pub fn ensure_ready(&mut self) -> Result<()> {
        if self.status == BrowserStatus::Ready {
            return Ok(());
        }

        tracing::info!(
            headless = self.config.headless,
            browser_type = %self.config.browser_type,
            profiles_root = %self.profiles_root.display(),
            "Initializing browser automation context"
        );

        // Ensure profiles root exists
        std::fs::create_dir_all(&self.profiles_root).map_err(|e| {
            let msg = format!(
                "Failed to create profiles directory {}: {e}",
                self.profiles_root.display()
            );
            self.status = BrowserStatus::Failed(msg.clone());
            StorageError::Database(msg)
        })?;

        // Check Playwright CLI availability
        match check_playwright_available() {
            Ok(version) => {
                tracing::info!(
                    playwright_version = %version,
                    "Playwright CLI available"
                );
            }
            Err(e) => {
                let msg = format!("Playwright CLI not available: {e}");
                tracing::warn!(%msg);
                self.status = BrowserStatus::Failed(msg.clone());
                return Err(StorageError::Database(msg).into());
            }
        }

        self.status = BrowserStatus::Ready;
        Ok(())
    }
}

/// Check if the Playwright CLI is available and return its version.
fn check_playwright_available() -> std::result::Result<String, String> {
    let output = std::process::Command::new("npx")
        .args(["playwright", "--version"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to run npx playwright: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "npx playwright --version failed (exit {}): {}",
            output.status,
            stderr.trim()
        ));
    }

    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(version)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // BrowserConfig tests
    // =========================================================================

    #[test]
    fn config_defaults() {
        let cfg = BrowserConfig::default();
        assert!(!cfg.headless);
        assert_eq!(cfg.navigation_timeout_ms, 30_000);
        assert_eq!(cfg.page_load_timeout_ms, 60_000);
        assert_eq!(cfg.browser_type, "chromium");
    }

    #[test]
    fn config_serde_round_trip() {
        let cfg = BrowserConfig {
            headless: true,
            navigation_timeout_ms: 15_000,
            page_load_timeout_ms: 45_000,
            browser_type: "firefox".to_string(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let deserialized: BrowserConfig = serde_json::from_str(&json).unwrap();
        assert!(deserialized.headless);
        assert_eq!(deserialized.navigation_timeout_ms, 15_000);
        assert_eq!(deserialized.browser_type, "firefox");
    }

    #[test]
    fn config_serde_defaults_on_missing() {
        let json = "{}";
        let cfg: BrowserConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.headless);
        assert_eq!(cfg.navigation_timeout_ms, 30_000);
    }

    // =========================================================================
    // Profile path resolution tests
    // =========================================================================

    #[test]
    fn profile_path_resolution() {
        let root = PathBuf::from("/home/user/.local/share/wa");
        let profiles_root = profiles_root_from_data_dir(&root);
        let profile = BrowserProfile::new(&profiles_root, "openai", "my-account");

        let expected = PathBuf::from(
            "/home/user/.local/share/wa/browser_profiles/openai/my-account",
        );
        assert_eq!(profile.path(), expected);
    }

    #[test]
    fn profile_path_different_services() {
        let profiles_root = PathBuf::from("/data/browser_profiles");

        let p1 = BrowserProfile::new(&profiles_root, "openai", "account-1");
        let p2 = BrowserProfile::new(&profiles_root, "anthropic", "account-1");
        let p3 = BrowserProfile::new(&profiles_root, "google", "work-acct");

        assert_ne!(p1.path(), p2.path());
        assert_ne!(p2.path(), p3.path());
        assert!(p1.path().to_string_lossy().contains("openai"));
        assert!(p2.path().to_string_lossy().contains("anthropic"));
        assert!(p3.path().to_string_lossy().contains("google"));
    }

    #[test]
    fn profile_path_sanitization() {
        let profiles_root = PathBuf::from("/data/profiles");

        // Slashes and special chars should be sanitized
        let profile = BrowserProfile::new(&profiles_root, "my/service", "acct@email.com");
        let path = profile.path();
        let path_str = path.to_string_lossy();

        // Should NOT contain raw slash from service name
        assert!(!path_str.contains("my/service"));
        // Should contain sanitized version
        assert!(path_str.contains("my_service"));
        // @ becomes _
        assert!(path_str.contains("acct_email.com"));
    }

    #[test]
    fn profile_path_traversal_prevention() {
        let profiles_root = PathBuf::from("/data/profiles");

        // Path traversal attempts should be sanitized
        let profile = BrowserProfile::new(&profiles_root, "../etc", "passwd");
        let path = profile.path();

        // Must still be under profiles_root
        assert!(path.starts_with("/data/profiles"));
        // .. should be sanitized to __
        assert!(!path.to_string_lossy().contains("../"));
    }

    #[test]
    fn sanitize_path_component_alphanumeric() {
        assert_eq!(sanitize_path_component("hello-world_123"), "hello-world_123");
    }

    #[test]
    fn sanitize_path_component_special_chars() {
        assert_eq!(sanitize_path_component("a/b\\c:d"), "a_b_c_d");
        assert_eq!(sanitize_path_component("user@host"), "user_host");
        assert_eq!(sanitize_path_component("name with spaces"), "name_with_spaces");
    }

    #[test]
    fn sanitize_path_component_dots_preserved() {
        assert_eq!(sanitize_path_component("file.name"), "file.name");
        assert_eq!(sanitize_path_component("v1.2.3"), "v1.2.3");
    }

    #[test]
    fn sanitize_path_component_empty() {
        assert_eq!(sanitize_path_component(""), "");
    }

    // =========================================================================
    // Profile directory tests
    // =========================================================================

    #[test]
    fn profile_ensure_dir_creates_directory() {
        let temp = std::env::temp_dir().join(format!(
            "wa_browser_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);

        let profile = BrowserProfile::new(&temp, "openai", "test-account");
        assert!(!profile.exists());

        let dir = profile.ensure_dir().unwrap();
        assert!(dir.is_dir());
        assert!(profile.exists());

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn profile_ensure_dir_idempotent() {
        let temp = std::env::temp_dir().join(format!(
            "wa_browser_test_idempotent_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);

        let profile = BrowserProfile::new(&temp, "openai", "test");
        profile.ensure_dir().unwrap();
        profile.ensure_dir().unwrap(); // Should not fail

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[cfg(unix)]
    #[test]
    fn profile_dir_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = std::env::temp_dir().join(format!(
            "wa_browser_test_perms_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);

        let profile = BrowserProfile::new(&temp, "openai", "secure");
        let dir = profile.ensure_dir().unwrap();

        let perms = std::fs::metadata(&dir).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o700);

        let _ = std::fs::remove_dir_all(&temp);
    }

    // =========================================================================
    // BrowserContext tests
    // =========================================================================

    #[test]
    fn context_new_is_not_initialized() {
        let temp = std::env::temp_dir().join("wa_browser_ctx_test");
        let ctx = BrowserContext::new(BrowserConfig::default(), &temp);
        assert_eq!(*ctx.status(), BrowserStatus::NotInitialized);
    }

    #[test]
    fn context_profile_resolution() {
        let data_dir = PathBuf::from("/home/user/.local/share/wa");
        let ctx = BrowserContext::new(BrowserConfig::default(), &data_dir);

        let profile = ctx.profile("openai", "acct-1");
        assert_eq!(
            profile.path(),
            PathBuf::from("/home/user/.local/share/wa/browser_profiles/openai/acct-1")
        );
    }

    #[test]
    fn context_profiles_root() {
        let data_dir = PathBuf::from("/data/wa");
        let ctx = BrowserContext::new(BrowserConfig::default(), &data_dir);
        assert_eq!(
            ctx.profiles_root(),
            Path::new("/data/wa/browser_profiles")
        );
    }

    #[test]
    fn context_config_accessible() {
        let cfg = BrowserConfig {
            headless: true,
            ..Default::default()
        };
        let ctx = BrowserContext::new(cfg, Path::new("/tmp"));
        assert!(ctx.config().headless);
    }

    // =========================================================================
    // profiles_root_from_data_dir tests
    // =========================================================================

    #[test]
    fn profiles_root_linux() {
        let root = profiles_root_from_data_dir(Path::new("/home/user/.local/share/wa"));
        assert_eq!(
            root,
            PathBuf::from("/home/user/.local/share/wa/browser_profiles")
        );
    }

    #[test]
    fn profiles_root_custom() {
        let root = profiles_root_from_data_dir(Path::new("/opt/wa-data"));
        assert_eq!(root, PathBuf::from("/opt/wa-data/browser_profiles"));
    }
}
