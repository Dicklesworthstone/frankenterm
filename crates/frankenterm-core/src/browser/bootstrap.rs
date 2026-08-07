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

use super::{BootstrapMethod, BrowserContext, BrowserProfile, BrowserStatus};

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
            success_text_markers: vec!["Successfully logged in".to_string()],
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
    pub fn execute(
        &self,
        ctx: &BrowserContext,
        profile: &BrowserProfile,
        service_url: Option<&str>,
    ) -> BootstrapResult {
        if *ctx.status() != BrowserStatus::Ready {
            return BootstrapResult::Failed {
                error: "The browser automation context is not ready".to_string(),
            };
        }

        let profile_dir = match profile.ensure_dir() {
            Ok(dir) => dir,
            Err(_) => {
                return BootstrapResult::Failed {
                    error: "The browser profile directory could not be prepared safely"
                        .to_string(),
                };
            }
        };

        let login_url = service_url.unwrap_or(&self.config.login_url);

        tracing::info!(
            timeout_ms = self.config.timeout_ms,
            "Starting interactive bootstrap — operator action required"
        );

        let start = std::time::Instant::now();
        let result = self.run_bootstrap_script(&profile_dir, login_url);
        let elapsed_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(ScriptOutcome::Success { storage_state }) => {
                if profile
                    .record_authenticated_state(&storage_state, BootstrapMethod::Interactive)
                    .is_err()
                {
                    tracing::error!(elapsed_ms, "Interactive bootstrap storage persistence failed");
                    return BootstrapResult::Failed {
                        error: "Interactive login completed, but authenticated browser profile state could not be persisted safely"
                            .to_string(),
                    };
                }

                tracing::info!(
                    elapsed_ms,
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
            Err(error) => {
                tracing::error!(elapsed_ms, "Interactive bootstrap failed");
                BootstrapResult::Failed { error }
            }
        }
    }

    fn run_bootstrap_script(
        &self,
        profile_dir: &Path,
        login_url: &str,
    ) -> Result<ScriptOutcome, String> {
        let script = self
            .build_bootstrap_script(profile_dir, login_url)
            .map_err(|failure| failure.detail().to_string())?;
        let output = super::run_node_script_bounded(
            script,
            self.config.timeout_ms,
            super::BROWSER_BOOTSTRAP_MAX_STDOUT_BYTES,
        )
        .map_err(|failure| failure.detail().to_string())?;
        let std::process::Output {
            status,
            stdout,
            stderr,
        } = output;
        if !stderr.is_empty() {
            tracing::debug!(
                stderr_bytes = stderr.len(),
                "Bootstrap subprocess stderr (content redacted in logs)"
            );
        }
        drop(stderr);
        let stdout = String::from_utf8(stdout)
            .map_err(|_| "Bootstrap returned invalid result text".to_string())?;

        if !status.success() {
            return Err("The browser bootstrap subprocess failed".to_string());
        }

        Self::parse_bootstrap_result(&stdout)
    }

    fn build_bootstrap_script(
        &self,
        profile_dir: &Path,
        login_url: &str,
    ) -> Result<String, super::BrowserNodeCommandFailure> {
        super::admit_browser_timeout(self.config.timeout_ms)?;
        super::admit_browser_poll_interval(
            self.config.poll_interval_ms,
            self.config.timeout_ms,
        )?;
        super::admit_browser_url(login_url)?;
        if self.config.success_url_prefixes.is_empty()
            && self.config.success_text_markers.is_empty()
        {
            return Err(super::BrowserNodeCommandFailure::InvalidConfiguration);
        }
        for success_url in &self.config.success_url_prefixes {
            super::admit_browser_url(success_url)?;
        }
        if self
            .config
            .success_text_markers
            .iter()
            .any(|marker| marker.trim().is_empty())
        {
            return Err(super::BrowserNodeCommandFailure::InvalidConfiguration);
        }
        super::admit_node_script_input_parts(
            &[Some(login_url)],
            &[Some(profile_dir)],
            &[
                self.config.success_url_prefixes.as_slice(),
                self.config.success_text_markers.as_slice(),
            ],
        )?;
        let input = serde_json::json!({
            "profile_dir": profile_dir.to_string_lossy(),
            "login_url": login_url,
            "timeout_ms": self.config.timeout_ms,
            "poll_interval_ms": self.config.poll_interval_ms,
            "success_url_prefixes": &self.config.success_url_prefixes,
            "success_text_markers": &self.config.success_text_markers,
        });
        let input_base64 = super::encode_node_script_input(&input)?;

        super::admit_node_script_source(format!(
            r"
const {{ chromium }} = require('playwright');
	const input = JSON.parse(Buffer.from('{input_base64}', 'base64').toString('utf8'));

	function sameOrigin(left, right) {{
	  try {{ return new URL(left).origin === new URL(right).origin; }} catch (_) {{ return false; }}
	}}

	function matchesSuccessUrl(currentValue, prefixValue) {{
	  try {{
	    const current = new URL(currentValue);
	    const expected = new URL(prefixValue);
	    if (current.origin !== expected.origin) return false;
	    const expectedPath = expected.pathname.endsWith('/')
	      ? expected.pathname
	      : expected.pathname + '/';
	    return current.pathname === expected.pathname || current.pathname.startsWith(expectedPath);
	  }} catch (_) {{
	    return false;
	  }}
	}}

(async () => {{
  const TIMEOUT = input.timeout_ms;
  const POLL_INTERVAL = input.poll_interval_ms;
  const profileDir = input.profile_dir;
  const loginUrl = input.login_url;
  const successUrls = input.success_url_prefixes;
  const successTexts = input.success_text_markers;

  let browser;
  try {{
    browser = await chromium.launchPersistentContext(profileDir, {{
      headless: false,
      timeout: TIMEOUT,
    }});

    const page = browser.pages()[0] || await browser.newPage();
    page.setDefaultTimeout(TIMEOUT);

	    await page.goto(loginUrl, {{ waitUntil: 'domcontentloaded', timeout: Math.min(30000, TIMEOUT) }});

    const startTime = Date.now();
    let success = false;

    while (Date.now() - startTime < TIMEOUT) {{
      try {{
        const currentUrl = page.url();

	        for (const prefix of successUrls) {{
	          if (matchesSuccessUrl(currentUrl, prefix)) {{
            success = true;
            break;
          }}
        }}
        if (success) break;

	        const markerOriginAllowed = successUrls.some(prefix => sameOrigin(currentUrl, prefix))
	          || sameOrigin(currentUrl, loginUrl);
	        if (markerOriginAllowed) {{
	          const bodyText = await page.textContent('body').catch(() => '');
	          for (const marker of successTexts) {{
	            if (bodyText && bodyText.includes(marker)) {{
	              success = true;
	              break;
	            }}
	          }}
	        }}
        if (success) break;
      }} catch (e) {{
        // Page might be navigating, ignore transient errors
      }}

	      const remaining = TIMEOUT - (Date.now() - startTime);
	      if (remaining <= 0) break;
	      await new Promise(r => setTimeout(r, Math.min(POLL_INTERVAL, remaining)));
    }}

    if (success) {{
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
	    const message = String(err && err.message || '').toLowerCase();
	    if (message.includes('browser') && message.includes('closed')) {{
      console.log(JSON.stringify({{ status: 'browser_closed' }}));
    }} else {{
      console.log(JSON.stringify({{
        status: 'error'
      }}));
      if (browser) await browser.close().catch(() => {{}});
      process.exit(1);
    }}
  }}
}})();
"
        ))
    }

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

        let parsed: serde_json::Value = serde_json::from_str(json_line)
            .map_err(|_| "Bootstrap returned malformed result JSON".to_string())?;

        match parsed.get("status").and_then(|s| s.as_str()) {
            Some("success") => {
                let state = parsed
                    .get("storage_state")
                    .and_then(|s| s.as_str())
                    .ok_or_else(|| {
                        "Bootstrap success result omitted browser storage state".to_string()
                    })?
                    .as_bytes()
                    .to_vec();
                Ok(ScriptOutcome::Success {
                    storage_state: state,
                })
            }
            Some("timeout") => Ok(ScriptOutcome::Timeout),
            Some("browser_closed") => Ok(ScriptOutcome::BrowserClosed),
            Some("error") => Err("The browser bootstrap subprocess failed".to_string()),
            _ => Err("Bootstrap returned an unrecognized result".to_string()),
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

    #[test]
    fn script_transports_login_url_without_plaintext_literal() {
        let bootstrap = InteractiveBootstrap::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        let script = bootstrap
            .build_bootstrap_script(&profile_dir, "https://auth.openai.com/authorize")
            .expect("bounded bootstrap script");
        assert!(!script.contains("auth.openai.com/authorize"));
        assert!(!script.contains("/tmp/profile"));
        assert!(script.contains("headless: false"));
        let input = super::super::decode_node_script_input(&script);
        assert_eq!(input["login_url"], "https://auth.openai.com/authorize");
        assert_eq!(input["profile_dir"], "/tmp/profile");
    }

    #[test]
    fn script_contains_success_markers() {
        let bootstrap = InteractiveBootstrap::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        let script = bootstrap
            .build_bootstrap_script(&profile_dir, "https://example.com/login")
            .expect("bounded bootstrap script");
        let input = super::super::decode_node_script_input(&script);
        assert!(
            input["success_url_prefixes"]
                .as_array()
                .is_some_and(|values| values.iter().any(|value| value.as_str() == Some("https://platform.openai.com")))
        );
        assert!(
            input["success_text_markers"]
                .as_array()
                .is_some_and(|values| values.iter().any(|value| value.as_str() == Some("Successfully logged in")))
        );
    }

    #[test]
    fn script_exports_storage_state() {
        let bootstrap = InteractiveBootstrap::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        let script = bootstrap
            .build_bootstrap_script(&profile_dir, "https://example.com/login")
            .expect("bounded bootstrap script");
        assert!(script.contains("storageState"));
    }

    #[test]
    fn parse_success_with_state() {
        let stdout = r#"{"status":"success","storage_state":"{\"cookies\":[],\"origins\":[]}"}"#;
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
        let error = result.unwrap_err();
        assert_eq!(error, "The browser bootstrap subprocess failed");
        assert!(!error.contains("crash"));
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

    // -- Additional coverage: bootstrap expanded tests --

    #[test]
    fn config_default_login_url_is_openai() {
        let cfg = BootstrapConfig::default();
        assert!(cfg.login_url.contains("openai.com"));
    }

    #[test]
    fn config_default_success_prefixes_count() {
        let cfg = BootstrapConfig::default();
        assert_eq!(cfg.success_url_prefixes.len(), 2);
    }

    #[test]
    fn config_default_text_markers_count() {
        let cfg = BootstrapConfig::default();
        assert_eq!(cfg.success_text_markers.len(), 1);
    }

    #[test]
    fn result_all_variants_debug() {
        let variants: Vec<BootstrapResult> = vec![
            BootstrapResult::Success {
                elapsed_ms: 1000,
                profile_dir: PathBuf::from("/tmp/test"),
            },
            BootstrapResult::Timeout { waited_ms: 5000 },
            BootstrapResult::Cancelled {
                reason: "closed".to_string(),
            },
            BootstrapResult::Failed {
                error: "err".to_string(),
            },
        ];
        for v in &variants {
            let dbg = format!("{:?}", v);
            assert!(!dbg.is_empty());
        }
    }

    #[test]
    fn bootstrap_config_accessor_returns_ref() {
        let cfg = BootstrapConfig {
            timeout_ms: 42_000,
            ..Default::default()
        };
        let bootstrap = InteractiveBootstrap::new(cfg);
        assert_eq!(bootstrap.config().timeout_ms, 42_000);
        assert_eq!(
            bootstrap.config().login_url,
            BootstrapConfig::default().login_url
        );
    }

    #[test]
    fn script_round_trips_hostile_values_without_javascript_literal_injection() {
        let bootstrap = InteractiveBootstrap::with_defaults();
        let hostile_marker = "logged'in\\\n\u{2028}with token=secret";
        let profile_dir = PathBuf::from("/tmp/profile'\\\n\u{2028}");
        let mut config = BootstrapConfig::default();
        config.success_text_markers = vec![hostile_marker.to_string()];
        let bootstrap = InteractiveBootstrap::new(config);
        let script = bootstrap
            .build_bootstrap_script(&profile_dir, "https://example.com/login")
            .expect("bounded hostile bootstrap script");
        assert!(!script.contains(hostile_marker));
        assert!(!script.contains("token=secret"));
        assert!(!script.contains('\u{2028}'));
        let input = super::super::decode_node_script_input(&script);
        assert_eq!(input["login_url"], "https://example.com/login");
        assert_eq!(input["profile_dir"], profile_dir.to_string_lossy().as_ref());
        assert_eq!(input["success_text_markers"][0], hostile_marker);
    }

    #[test]
    fn script_contains_timeout_value() {
        let cfg = BootstrapConfig {
            timeout_ms: 99_000,
            ..Default::default()
        };
        let bootstrap = InteractiveBootstrap::new(cfg);
        let script = bootstrap
            .build_bootstrap_script(&PathBuf::from("/tmp"), "https://example.com")
            .expect("bounded bootstrap script");
        assert_eq!(
            super::super::decode_node_script_input(&script)["timeout_ms"],
            99_000
        );
    }

    #[test]
    fn script_contains_poll_interval() {
        let cfg = BootstrapConfig {
            poll_interval_ms: 3_500,
            ..Default::default()
        };
        let bootstrap = InteractiveBootstrap::new(cfg);
        let script = bootstrap
            .build_bootstrap_script(&PathBuf::from("/tmp"), "https://example.com")
            .expect("bounded bootstrap script");
        assert_eq!(
            super::super::decode_node_script_input(&script)["poll_interval_ms"],
            3_500
        );
    }

    #[test]
    fn bootstrap_script_input_enforces_exact_and_one_over_field_limit() {
        let exact = "x".repeat(super::super::BROWSER_NODE_INPUT_MAX_FIELD_BYTES);
        let mut config = BootstrapConfig::default();
        config.success_text_markers = vec![exact.clone()];
        let exact_bootstrap = InteractiveBootstrap::new(config);
        assert!(exact_bootstrap
            .build_bootstrap_script(Path::new("/tmp/profile"), "https://example.com/login")
            .is_ok());
        let one_over = format!("{exact}x");
        let mut config = BootstrapConfig::default();
        config.success_text_markers = vec![one_over];
        let oversized_bootstrap = InteractiveBootstrap::new(config);
        assert_eq!(
            oversized_bootstrap
                .build_bootstrap_script(Path::new("/tmp/profile"), "https://example.com/login"),
            Err(super::super::BrowserNodeCommandFailure::ScriptOversized)
        );
    }

    #[test]
    fn bootstrap_rejects_invalid_polling_and_unsafe_urls_before_execution() {
        let mut zero_poll = BootstrapConfig::default();
        zero_poll.poll_interval_ms = 0;
        assert_eq!(
            InteractiveBootstrap::new(zero_poll)
                .build_bootstrap_script(Path::new("/tmp/profile"), "https://example.com/login"),
            Err(super::super::BrowserNodeCommandFailure::InvalidPollInterval)
        );

        let mut over_timeout = BootstrapConfig::default();
        over_timeout.timeout_ms = 1_000;
        over_timeout.poll_interval_ms = 1_001;
        assert_eq!(
            InteractiveBootstrap::new(over_timeout)
                .build_bootstrap_script(Path::new("/tmp/profile"), "https://example.com/login"),
            Err(super::super::BrowserNodeCommandFailure::InvalidPollInterval)
        );

        assert_eq!(
            InteractiveBootstrap::with_defaults()
                .build_bootstrap_script(Path::new("/tmp/profile"), "file:///tmp/login"),
            Err(super::super::BrowserNodeCommandFailure::InvalidConfiguration)
        );
    }

    #[test]
    fn bootstrap_script_uses_origin_aware_url_matching_and_bounded_final_sleep() {
        let script = InteractiveBootstrap::with_defaults()
            .build_bootstrap_script(
                Path::new("/tmp/profile"),
                "https://auth.openai.com/authorize",
            )
            .expect("bounded bootstrap script");
        assert!(script.contains("current.origin !== expected.origin"));
        assert!(!script.contains("currentUrl.startsWith(prefix)"));
        assert!(script.contains("Math.min(POLL_INTERVAL, remaining)"));
        assert!(script.contains("markerOriginAllowed"));
        assert!(script.contains("message.includes('browser') && message.includes('closed')"));
    }

    #[test]
    fn parse_unknown_status_is_error() {
        let result = InteractiveBootstrap::parse_bootstrap_result(r#"{"status":"unknown_status"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn parse_whitespace_only_is_error() {
        let result = InteractiveBootstrap::parse_bootstrap_result("   \n  \n  ");
        assert!(result.is_err());
    }

    #[test]
    fn parse_invalid_json_is_error() {
        let result = InteractiveBootstrap::parse_bootstrap_result("{not valid json}");
        assert!(result.is_err());
    }

    #[test]
    fn result_success_roundtrip_profile_dir() {
        let result = BootstrapResult::Success {
            elapsed_ms: 12345,
            profile_dir: PathBuf::from("/home/user/.config/profile"),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: BootstrapResult = serde_json::from_str(&json).unwrap();
        match back {
            BootstrapResult::Success {
                elapsed_ms,
                profile_dir,
            } => {
                assert_eq!(elapsed_ms, 12345);
                assert_eq!(profile_dir, PathBuf::from("/home/user/.config/profile"));
            }
            _ => panic!("Expected Success"),
        }
    }

    #[test]
    fn node_runner_is_stdin_bounded_and_never_uses_inline_argv() {
        let source = include_str!("bootstrap.rs");
        let start = source.find("fn run_bootstrap_script(").expect("runner source");
        let tail = &source[start..];
        let end = tail.find("\n    fn build_bootstrap_script(").expect("runner boundary");
        let body = &tail[..end];
        assert!(body.contains("run_node_script_bounded"));
        assert!(!body.contains("std::process::Command"));
        assert!(!body.contains(".arg(\"-e\")"));
        assert!(!body.contains("stderr_summary"));
    }
}
