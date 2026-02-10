//! Interactive browser bootstrap for one-time MFA/password login.
//!
//! When the automated device auth flow detects a password or MFA prompt,
//! it exits with `InteractiveBootstrapRequired`. This module provides the
//! fallback: launch a visible browser window for the human operator to
//! complete login once, then persist the session profile for future
//! automated runs.
//!
//! # Flow
//!
//! ```text
//! InteractiveBootstrapRequired (from openai_device)
//!        │
//!        ▼
//! Launch visible browser → navigate to login URL
//!        │
//!        ▼
//! Operator completes login (password + MFA)
//!        │
//!        ▼
//! Detect success (URL change / page marker)
//!        │
//!        ▼
//! Export storageState() → save to profile
//!        │
//!        ▼
//! Update ProfileMetadata (bootstrapped_at, method=interactive)
//! ```
//!
//! # Safety
//!
//! - The browser is launched in **visible** (non-headless) mode.
//! - No passwords or MFA codes are captured or logged.
//! - Only the storage state (cookies + localStorage) is persisted.
//! - The operator must physically interact with the browser.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{BootstrapMethod, BrowserContext, BrowserProfile, BrowserStatus, ProfileMetadata};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the interactive bootstrap flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BootstrapConfig {
    /// URL to navigate to for login (e.g., `https://auth.openai.com/authorize`).
    pub login_url: String,

    /// Maximum time to wait for the operator to complete login (ms).
    /// Default: 5 minutes.
    pub timeout_ms: u64,

    /// Interval between success-detection polls (ms).
    /// Default: 2 seconds.
    pub poll_interval_ms: u64,

    /// URLs that indicate successful login (prefix match).
    /// When the browser navigates to any of these, the bootstrap is complete.
    pub success_url_prefixes: Vec<String>,

    /// Page text markers that indicate successful login.
    pub success_text_markers: Vec<String>,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            login_url: "https://auth.openai.com/authorize".to_string(),
            timeout_ms: 300_000, // 5 minutes
            poll_interval_ms: 2_000,
            success_url_prefixes: vec![
                "https://platform.openai.com".to_string(),
                "https://chatgpt.com".to_string(),
            ],
            success_text_markers: vec!["Successfully logged in".to_string(), "Welcome".to_string()],
        }
    }
}

// =============================================================================
// Result types
// =============================================================================

/// Result of the interactive bootstrap flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum BootstrapResult {
    /// Operator completed login; profile has been persisted.
    #[serde(rename = "success")]
    Success {
        /// Wall-clock time the operator took to complete login (ms).
        elapsed_ms: u64,
        /// Path to the persisted profile directory.
        profile_dir: PathBuf,
    },

    /// Operator did not complete login within the timeout.
    #[serde(rename = "timeout")]
    Timeout {
        /// How long we waited (ms).
        waited_ms: u64,
    },

    /// Operator cancelled the bootstrap (closed the browser).
    #[serde(rename = "cancelled")]
    Cancelled {
        /// Reason for cancellation.
        reason: String,
    },

    /// Bootstrap failed due to an error.
    #[serde(rename = "failed")]
    Failed {
        /// Human-readable error description.
        error: String,
    },
}

// =============================================================================
// Interactive bootstrap flow
// =============================================================================

/// Orchestrates one-time interactive login for browser profile bootstrap.
///
/// This flow is designed to be invoked when automated auth fails with
/// `InteractiveBootstrapRequired`. It opens a visible browser window
/// for the operator to complete login, then persists the session.
pub struct InteractiveBootstrap {
    config: BootstrapConfig,
}

impl InteractiveBootstrap {
    /// Create a new bootstrap flow with the given configuration.
    #[must_use]
    pub fn new(config: BootstrapConfig) -> Self {
        Self { config }
    }

    /// Create a new bootstrap flow with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(BootstrapConfig::default())
    }

    /// Current configuration.
    #[must_use]
    pub fn config(&self) -> &BootstrapConfig {
        &self.config
    }

    /// Execute the interactive bootstrap flow.
    ///
    /// This launches a visible browser window, navigates to the login URL,
    /// and waits for the operator to complete authentication. On success,
    /// the browser's storage state is saved and the profile metadata is
    /// updated.
    ///
    /// # Arguments
    ///
    /// * `ctx` - Browser context (must be in `Ready` state).
    /// * `profile` - The browser profile to bootstrap.
    /// * `service_url` - Optional override for the login URL.
    pub fn execute(
        &self,
        ctx: &BrowserContext,
        profile: &BrowserProfile,
        service_url: Option<&str>,
    ) -> BootstrapResult {
        // Verify browser context is ready
        if *ctx.status() != BrowserStatus::Ready {
            return BootstrapResult::Failed {
                error: format!("Browser context not ready: {:?}", ctx.status()),
            };
        }

        // Ensure profile directory exists
        let profile_dir = match profile.ensure_dir() {
            Ok(dir) => dir,
            Err(e) => {
                return BootstrapResult::Failed {
                    error: format!("Failed to create profile directory: {e}"),
                };
            }
        };

        let login_url = service_url.unwrap_or(&self.config.login_url);

        tracing::info!(
            profile_dir = %profile_dir.display(),
            login_url = %login_url,
            timeout_ms = self.config.timeout_ms,
            "Starting interactive bootstrap — operator action required"
        );

        let start = std::time::Instant::now();

        // Run the Playwright script that opens the browser and waits
        let result = self.run_bootstrap_script(&profile_dir, login_url);

        let elapsed_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(ScriptOutcome::Success { storage_state }) => {
                // Save storage state
                if let Err(e) = profile.save_storage_state(&storage_state) {
                    tracing::warn!(error = %e, "Failed to save storage state");
                }

                // Update metadata
                let mut metadata =
                    profile.read_metadata().ok().flatten().unwrap_or_else(|| {
                        ProfileMetadata::new(&profile.service, &profile.account)
                    });
                metadata.record_bootstrap(BootstrapMethod::Interactive);
                if let Err(e) = profile.write_metadata(&metadata) {
                    tracing::warn!(error = %e, "Failed to write profile metadata");
                }

                tracing::info!(
                    elapsed_ms,
                    profile_dir = %profile_dir.display(),
                    "Interactive bootstrap completed successfully"
                );

                BootstrapResult::Success {
                    elapsed_ms,
                    profile_dir,
                }
            }
            Ok(ScriptOutcome::Timeout) => {
                tracing::warn!(
                    waited_ms = elapsed_ms,
                    "Interactive bootstrap timed out — operator did not complete login"
                );
                BootstrapResult::Timeout {
                    waited_ms: elapsed_ms,
                }
            }
            Ok(ScriptOutcome::BrowserClosed) => {
                tracing::info!(
                    elapsed_ms,
                    "Interactive bootstrap cancelled — browser was closed"
                );
                BootstrapResult::Cancelled {
                    reason: "Browser window was closed before login completed".to_string(),
                }
            }
            Err(e) => {
                tracing::error!(
                    elapsed_ms,
                    error = %e,
                    "Interactive bootstrap failed"
                );
                BootstrapResult::Failed { error: e }
            }
        }
    }

    /// Run the Playwright script for interactive bootstrap.
    ///
    /// The script:
    /// 1. Launches a visible browser with the profile directory
    /// 2. Navigates to the login URL
    /// 3. Polls for success (URL change or page marker)
    /// 4. Exports storageState on success
    fn run_bootstrap_script(
        &self,
        profile_dir: &Path,
        login_url: &str,
    ) -> Result<ScriptOutcome, String> {
        let script = self.build_bootstrap_script(profile_dir, login_url);

        let output = std::process::Command::new("node")
            .arg("-e")
            .arg(&script)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|e| format!("Failed to spawn node process: {e}"))?;

        let stdout = String::from_utf8_lossy(&output.stdout);

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "Playwright bootstrap script failed (exit {}): {}",
                output.status,
                stderr.lines().take(5).collect::<Vec<_>>().join("; ")
            ));
        }

        Self::parse_bootstrap_result(&stdout)
    }

    /// Build the Node.js/Playwright script for interactive bootstrap.
    fn build_bootstrap_script(&self, profile_dir: &Path, login_url: &str) -> String {
        let profile_dir_str = profile_dir.display();
        let timeout = self.config.timeout_ms;
        let poll_interval = self.config.poll_interval_ms;

        let success_urls = serde_json::to_string(&self.config.success_url_prefixes)
            .unwrap_or_else(|_| "[]".to_string());
        let success_texts = serde_json::to_string(&self.config.success_text_markers)
            .unwrap_or_else(|_| "[]".to_string());

        let login_url_escaped = login_url.replace('\'', "\\'");

        format!(
            r"
const {{ chromium }} = require('playwright');

(async () => {{
  const TIMEOUT = {timeout};
  const POLL_INTERVAL = {poll_interval};
  const profileDir = '{profile_dir_str}';
  const loginUrl = '{login_url_escaped}';
  const successUrls = {success_urls};
  const successTexts = {success_texts};

  let browser;
  try {{
    // Launch visible browser for operator interaction
    browser = await chromium.launchPersistentContext(profileDir, {{
      headless: false,
      timeout: TIMEOUT,
    }});

    const page = browser.pages()[0] || await browser.newPage();
    page.setDefaultTimeout(TIMEOUT);

    // Navigate to login page
    await page.goto(loginUrl, {{ waitUntil: 'domcontentloaded', timeout: 30000 }});

    // Poll for success indicators
    const startTime = Date.now();
    let success = false;

    while (Date.now() - startTime < TIMEOUT) {{
      try {{
        const currentUrl = page.url();

        // Check URL-based success
        for (const prefix of successUrls) {{
          if (currentUrl.startsWith(prefix)) {{
            success = true;
            break;
          }}
        }}
        if (success) break;

        // Check text-based success
        const bodyText = await page.textContent('body').catch(() => '');
        for (const marker of successTexts) {{
          if (bodyText && bodyText.includes(marker)) {{
            success = true;
            break;
          }}
        }}
        if (success) break;
      }} catch (e) {{
        // Page might be navigating, ignore transient errors
      }}

      await new Promise(r => setTimeout(r, POLL_INTERVAL));
    }}

    if (success) {{
      // Export storage state
      const state = await browser.storageState();
      console.log(JSON.stringify({{
        status: 'success',
        storage_state: JSON.stringify(state)
      }}));
    }} else {{
      console.log(JSON.stringify({{ status: 'timeout' }}));
    }}

    await browser.close();
  }} catch (err) {{
    if (err.message && err.message.includes('Browser closed')) {{
      console.log(JSON.stringify({{ status: 'browser_closed' }}));
    }} else {{
      console.log(JSON.stringify({{
        status: 'error',
        message: err.message
      }}));
      if (browser) await browser.close().catch(() => {{}});
      process.exit(1);
    }}
  }}
}})();
"
        )
    }

    /// Parse the result from the bootstrap Playwright script.
    fn parse_bootstrap_result(stdout: &str) -> Result<ScriptOutcome, String> {
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return Err("Bootstrap script produced no output".to_string());
        }

        let json_line = trimmed
            .lines()
            .rev()
            .find(|line| line.starts_with('{'))
            .unwrap_or(trimmed);

        let parsed: serde_json::Value =
            serde_json::from_str(json_line).map_err(|e| format!("Failed to parse output: {e}"))?;

        match parsed.get("status").and_then(|s| s.as_str()) {
            Some("success") => {
                let state = parsed
                    .get("storage_state")
                    .and_then(|s| s.as_str())
                    .unwrap_or("{}")
                    .as_bytes()
                    .to_vec();
                Ok(ScriptOutcome::Success {
                    storage_state: state,
                })
            }
            Some("timeout") => Ok(ScriptOutcome::Timeout),
            Some("browser_closed") => Ok(ScriptOutcome::BrowserClosed),
            Some("error") => {
                let msg = parsed
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error");
                Err(msg.to_string())
            }
            _ => Err(format!("Unexpected bootstrap output: {json_line}")),
        }
    }
}

/// Internal outcome from the bootstrap Playwright script.
#[derive(Debug)]
enum ScriptOutcome {
    /// Login succeeded; storage state exported.
    Success { storage_state: Vec<u8> },
    /// Timeout waiting for login.
    Timeout,
    /// Browser was closed by the operator.
    BrowserClosed,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // BootstrapConfig tests
    // =========================================================================

    #[test]
    fn config_defaults() {
        let cfg = BootstrapConfig::default();
        assert_eq!(cfg.timeout_ms, 300_000);
        assert_eq!(cfg.poll_interval_ms, 2_000);
        assert!(!cfg.login_url.is_empty());
        assert!(!cfg.success_url_prefixes.is_empty());
        assert!(!cfg.success_text_markers.is_empty());
    }

    #[test]
    fn config_serde_round_trip() {
        let cfg = BootstrapConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let deserialized: BootstrapConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.timeout_ms, cfg.timeout_ms);
        assert_eq!(deserialized.login_url, cfg.login_url);
        assert_eq!(
            deserialized.success_url_prefixes.len(),
            cfg.success_url_prefixes.len()
        );
    }

    #[test]
    fn config_custom_values() {
        let cfg = BootstrapConfig {
            login_url: "https://custom.auth/login".to_string(),
            timeout_ms: 60_000,
            poll_interval_ms: 1_000,
            success_url_prefixes: vec!["https://app.custom.com".to_string()],
            success_text_markers: vec!["Logged in".to_string()],
        };
        assert_eq!(cfg.timeout_ms, 60_000);
        assert_eq!(cfg.success_url_prefixes.len(), 1);
    }

    // =========================================================================
    // BootstrapResult serde tests
    // =========================================================================

    #[test]
    fn result_success_serde() {
        let result = BootstrapResult::Success {
            elapsed_ms: 5000,
            profile_dir: PathBuf::from("/tmp/profile"),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"status\":\"success\""));
        assert!(json.contains("\"elapsed_ms\":5000"));
        let deserialized: BootstrapResult = serde_json::from_str(&json).unwrap();
        match deserialized {
            BootstrapResult::Success { elapsed_ms, .. } => assert_eq!(elapsed_ms, 5000),
            _ => panic!("Expected Success"),
        }
    }

    #[test]
    fn result_timeout_serde() {
        let result = BootstrapResult::Timeout { waited_ms: 300_000 };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"status\":\"timeout\""));
        let deserialized: BootstrapResult = serde_json::from_str(&json).unwrap();
        match deserialized {
            BootstrapResult::Timeout { waited_ms } => assert_eq!(waited_ms, 300_000),
            _ => panic!("Expected Timeout"),
        }
    }

    #[test]
    fn result_cancelled_serde() {
        let result = BootstrapResult::Cancelled {
            reason: "closed by user".to_string(),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"status\":\"cancelled\""));
    }

    #[test]
    fn result_failed_serde() {
        let result = BootstrapResult::Failed {
            error: "some error".to_string(),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"status\":\"failed\""));
    }

    // =========================================================================
    // InteractiveBootstrap construction tests
    // =========================================================================

    #[test]
    fn bootstrap_with_defaults() {
        let bootstrap = InteractiveBootstrap::with_defaults();
        assert_eq!(bootstrap.config().timeout_ms, 300_000);
    }

    #[test]
    fn bootstrap_custom_config() {
        let cfg = BootstrapConfig {
            timeout_ms: 60_000,
            ..Default::default()
        };
        let bootstrap = InteractiveBootstrap::new(cfg);
        assert_eq!(bootstrap.config().timeout_ms, 60_000);
    }

    // =========================================================================
    // Flow execution guard tests
    // =========================================================================

    #[test]
    fn execute_rejects_not_ready_context() {
        let bootstrap = InteractiveBootstrap::with_defaults();
        let data_dir = std::env::temp_dir().join("wa_bootstrap_test_nr");
        let ctx =
            super::super::BrowserContext::new(super::super::BrowserConfig::default(), &data_dir);
        let profile = ctx.profile("openai", "test-account");

        let result = bootstrap.execute(&ctx, &profile, None);
        match result {
            BootstrapResult::Failed { error } => {
                assert!(error.contains("not ready"));
            }
            _ => panic!("Expected Failed with not ready"),
        }
    }

    // =========================================================================
    // Script generation tests
    // =========================================================================

    #[test]
    fn script_contains_login_url() {
        let bootstrap = InteractiveBootstrap::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        let script =
            bootstrap.build_bootstrap_script(&profile_dir, "https://auth.openai.com/authorize");
        assert!(script.contains("auth.openai.com/authorize"));
        assert!(script.contains("/tmp/profile"));
        assert!(script.contains("headless: false")); // Must be visible
    }

    #[test]
    fn script_contains_success_markers() {
        let bootstrap = InteractiveBootstrap::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        let script = bootstrap.build_bootstrap_script(&profile_dir, "https://example.com/login");
        assert!(script.contains("platform.openai.com"));
        assert!(script.contains("Successfully logged in"));
    }

    #[test]
    fn script_exports_storage_state() {
        let bootstrap = InteractiveBootstrap::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        let script = bootstrap.build_bootstrap_script(&profile_dir, "https://example.com/login");
        assert!(script.contains("storageState"));
    }

    // =========================================================================
    // Result parsing tests
    // =========================================================================

    #[test]
    fn parse_success_with_state() {
        let stdout = r#"{"status":"success","storage_state":"{\"cookies\":[]}"}"#;
        let result = InteractiveBootstrap::parse_bootstrap_result(stdout);
        match result {
            Ok(ScriptOutcome::Success { storage_state }) => {
                let state_str = String::from_utf8(storage_state).unwrap();
                assert!(state_str.contains("cookies"));
            }
            _ => panic!("Expected Success"),
        }
    }

    #[test]
    fn parse_timeout() {
        let result = InteractiveBootstrap::parse_bootstrap_result(r#"{"status":"timeout"}"#);
        assert!(matches!(result, Ok(ScriptOutcome::Timeout)));
    }

    #[test]
    fn parse_browser_closed() {
        let result = InteractiveBootstrap::parse_bootstrap_result(r#"{"status":"browser_closed"}"#);
        assert!(matches!(result, Ok(ScriptOutcome::BrowserClosed)));
    }

    #[test]
    fn parse_error() {
        let result =
            InteractiveBootstrap::parse_bootstrap_result(r#"{"status":"error","message":"crash"}"#);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("crash"));
    }

    #[test]
    fn parse_empty_output() {
        let result = InteractiveBootstrap::parse_bootstrap_result("");
        assert!(result.is_err());
    }

    #[test]
    fn parse_with_preceding_output() {
        let stdout = "Debugger attached.\n{\"status\":\"timeout\"}";
        let result = InteractiveBootstrap::parse_bootstrap_result(stdout);
        assert!(matches!(result, Ok(ScriptOutcome::Timeout)));
    }
}
