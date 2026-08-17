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

use super::{
    BootstrapMethod, BrowserAuthService, BrowserContext, BrowserProfile, BrowserStatus,
    UnsupportedBrowserAuthService,
};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the interactive bootstrap flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BootstrapConfig {
    /// Auth service whose fixed origin/path policy governs this flow.
    pub service: BrowserAuthService,

    /// URL to navigate to for login (e.g., `https://auth.openai.com/authorize`).
    pub login_url: String,

    /// Maximum time to wait for the operator to complete login (ms).
    /// Default: 5 minutes.
    pub timeout_ms: u64,

    /// Interval between success-detection polls (ms).
    /// Default: 2 seconds.
    pub poll_interval_ms: u64,

    /// Service-admitted URLs that may indicate successful login.
    ///
    /// A non-root path admits that path and its descendants. An origin root is
    /// admitted only at the exact `/` path and must also match one of
    /// [`Self::success_text_markers`].
    pub success_url_prefixes: Vec<String>,

    /// Page text markers that indicate successful login.
    pub success_text_markers: Vec<String>,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self::for_service(BrowserAuthService::OpenAi)
    }
}

impl BootstrapConfig {
    /// Build the supported, service-specific interactive-login policy.
    #[must_use]
    pub fn for_service(service: BrowserAuthService) -> Self {
        let (login_url, success_url_prefixes, success_text_markers) = match service {
            BrowserAuthService::OpenAi => (
                "https://auth.openai.com/authorize",
                vec![
                    "https://platform.openai.com".to_string(),
                    "https://chatgpt.com".to_string(),
                ],
                vec!["Successfully logged in".to_string()],
            ),
            BrowserAuthService::Google => (
                "https://accounts.google.com/",
                vec!["https://myaccount.google.com".to_string()],
                vec!["Manage your Google Account".to_string()],
            ),
            BrowserAuthService::Anthropic => (
                "https://console.anthropic.com/login",
                vec![
                    "https://console.anthropic.com/dashboard".to_string(),
                    "https://console.anthropic.com/settings".to_string(),
                    "https://console.anthropic.com/workspaces".to_string(),
                ],
                vec!["API Keys".to_string()],
            ),
        };
        Self {
            service,
            login_url: login_url.to_string(),
            timeout_ms: 300_000,
            poll_interval_ms: 2_000,
            success_url_prefixes,
            success_text_markers,
        }
    }

    /// Parse a CLI/config service name and build its fixed bootstrap policy.
    pub fn try_for_service_name(
        service: &str,
    ) -> std::result::Result<Self, UnsupportedBrowserAuthService> {
        BrowserAuthService::try_from(service).map(Self::for_service)
    }

    /// Replace the preset login URL after enforcing this service's exact
    /// origin/path policy and the browser-input size limit.
    ///
    /// Validation happens before `self` is changed, so rejection preserves the
    /// prior known-good URL. The returned error never contains caller input.
    pub fn set_login_url_override(
        &mut self,
        login_url: &str,
    ) -> std::result::Result<(), BrowserBootstrapConfigError> {
        super::admit_bootstrap_login_url(self.service, login_url)
            .map_err(|_| BrowserBootstrapConfigError)?;
        super::admit_node_script_input_parts(&[Some(login_url)], &[], &[])
            .map_err(|_| BrowserBootstrapConfigError)?;
        self.login_url = login_url.to_string();
        Ok(())
    }
}

/// Content-free rejection for an invalid interactive-bootstrap configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserBootstrapConfigError;

impl std::fmt::Display for BrowserBootstrapConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Browser bootstrap configuration is invalid")
    }
}

impl std::error::Error for BrowserBootstrapConfigError {}

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
        if profile.service() != self.config.service.as_str() {
            return BootstrapResult::Failed {
                error: "The browser profile does not match the selected authentication service"
                    .to_string(),
            };
        }
        if profile.validate_identity().is_err() {
            return BootstrapResult::Failed {
                error: "The browser profile identity is invalid".to_string(),
            };
        }

        let login_url = service_url.unwrap_or(&self.config.login_url);
        let profile_path = profile.path();
        let script = match self.build_bootstrap_script_with_browser_config(
            &profile_path,
            login_url,
            ctx.config(),
        ) {
            Ok(script) => script,
            Err(failure) => {
                return BootstrapResult::Failed {
                    error: failure.detail().to_string(),
                };
            }
        };

        let profile_dir = match profile.ensure_dir() {
            Ok(dir) => dir,
            Err(_) => {
                return BootstrapResult::Failed {
                    error: "The browser profile directory could not be prepared safely".to_string(),
                };
            }
        };
        let operation_lock = match profile
            .acquire_operation_lock(ctx.config().profile_lock_timeout_ms)
        {
            Ok(operation_lock) => operation_lock,
            Err(_) => {
                return BootstrapResult::Failed {
                    error: "The browser profile is already in use or could not be locked safely"
                        .to_string(),
                };
            }
        };

        tracing::info!(
            timeout_ms = self.config.timeout_ms,
            "Starting interactive bootstrap — operator action required"
        );

        if !operation_lock.is_current_for(profile) {
            return BootstrapResult::Failed {
                error: "The browser profile changed before interactive login could start"
                    .to_string(),
            };
        }

        let start = std::time::Instant::now();
        let result = self.run_bootstrap_script(script);
        if !operation_lock.is_current_for(profile) {
            return BootstrapResult::Failed {
                error: "The browser profile changed during interactive login".to_string(),
            };
        }
        let elapsed_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(ScriptOutcome::Success { storage_state }) => {
                if profile
                    .record_authenticated_state_with_lock(
                        &storage_state,
                        BootstrapMethod::Interactive,
                        &operation_lock,
                    )
                    .is_err()
                {
                    tracing::error!(
                        elapsed_ms,
                        "Interactive bootstrap storage persistence failed"
                    );
                    return BootstrapResult::Failed {
                        error: "Interactive login completed, but authenticated browser profile state could not be persisted safely"
                            .to_string(),
                    };
                }

                tracing::info!(elapsed_ms, "Interactive bootstrap completed successfully");

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

    fn run_bootstrap_script(&self, script: String) -> Result<ScriptOutcome, String> {
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

    fn build_bootstrap_script_with_browser_config(
        &self,
        profile_dir: &Path,
        login_url: &str,
        browser_config: &super::BrowserConfig,
    ) -> Result<String, super::BrowserNodeCommandFailure> {
        super::admit_browser_timeout(self.config.timeout_ms)?;
        super::admit_browser_timeout(browser_config.navigation_timeout_ms)?;
        super::admit_browser_timeout(browser_config.page_load_timeout_ms)?;
        super::admit_browser_poll_interval(self.config.poll_interval_ms, self.config.timeout_ms)?;
        super::admit_bootstrap_login_url(self.config.service, login_url)?;
        // Page text by itself is not authentication evidence: an attacker or
        // ordinary login page can render the same words. Every success must
        // first be bound to a service-admitted origin/path.
        if self.config.success_url_prefixes.is_empty() {
            return Err(super::BrowserNodeCommandFailure::InvalidConfiguration);
        }
        for success_url in &self.config.success_url_prefixes {
            super::admit_bootstrap_success_url(self.config.service, success_url)?;
        }
        let root_success_requires_marker = self
            .config
            .success_url_prefixes
            .iter()
            .any(|url| url::Url::parse(url).is_ok_and(|parsed| parsed.path() == "/"));
        if root_success_requires_marker && self.config.success_text_markers.is_empty() {
            return Err(super::BrowserNodeCommandFailure::InvalidConfiguration);
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
            "navigation_timeout_ms": browser_config.navigation_timeout_ms,
            "page_load_timeout_ms": browser_config.page_load_timeout_ms,
            "success_url_prefixes": &self.config.success_url_prefixes,
            "success_text_markers": &self.config.success_text_markers,
        });
        let input_base64 = super::encode_node_script_input(&input)?;

        super::admit_node_script_source(format!(
            r"
const {{ chromium }} = require(process.argv[2]);
	const input = JSON.parse(Buffer.from('{input_base64}', 'base64').toString('utf8'));

	function matchesSuccessUrl(currentValue, prefixValue) {{
	  try {{
	    const current = new URL(currentValue);
	    const expected = new URL(prefixValue);
	    if (current.origin !== expected.origin) return false;
	    if (expected.pathname === '/') return current.pathname === '/';
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
  const NAVIGATION_TIMEOUT = Math.min(input.navigation_timeout_ms, TIMEOUT);
  const PAGE_LOAD_TIMEOUT = Math.min(input.page_load_timeout_ms, TIMEOUT);
  const profileDir = input.profile_dir;
  const loginUrl = input.login_url;
  const successUrls = input.success_url_prefixes;
  const successTexts = input.success_text_markers;
  const startTime = Date.now();

  let browser;
  try {{
    browser = await chromium.launchPersistentContext(profileDir, {{
      headless: false,
      timeout: TIMEOUT,
    }});

    const page = browser.pages()[0] || await browser.newPage();
    page.setDefaultTimeout(PAGE_LOAD_TIMEOUT);

    await page.goto(loginUrl, {{ waitUntil: 'domcontentloaded', timeout: NAVIGATION_TIMEOUT }});

    let success = false;

    while (Date.now() - startTime < TIMEOUT) {{
      try {{
        const currentUrl = page.url();
        const matchedSuccessUrl = successUrls.find(prefix =>
          matchesSuccessUrl(currentUrl, prefix)
        );
        if (matchedSuccessUrl) {{
          const expectedPath = new URL(matchedSuccessUrl).pathname;
          if (expectedPath !== '/') {{
            success = true;
          }} else {{
            const bodyText = await page.textContent('body').catch(() => '');
            for (const marker of successTexts) {{
              if (bodyText && bodyText.includes(marker)) {{
                success = true;
                break;
              }}
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
        storage_state: state
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

    #[cfg(test)]
    fn build_bootstrap_script(
        &self,
        profile_dir: &Path,
        login_url: &str,
    ) -> Result<String, super::BrowserNodeCommandFailure> {
        self.build_bootstrap_script_with_browser_config(
            profile_dir,
            login_url,
            &super::BrowserConfig::default(),
        )
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
                    .filter(|value| value.is_object())
                    .ok_or_else(|| {
                        "Bootstrap success result omitted browser storage state".to_string()
                    })?;
                let state = serde_json::to_vec(state)
                    .map_err(|_| "Bootstrap returned invalid browser storage state".to_string())?;
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
        assert_eq!(cfg.service, BrowserAuthService::OpenAi);
        assert_eq!(cfg.timeout_ms, 300_000);
        assert_eq!(cfg.poll_interval_ms, 2_000);
        assert!(!cfg.login_url.is_empty());
        assert!(!cfg.success_url_prefixes.is_empty());
        assert!(!cfg.success_text_markers.is_empty());
    }

    #[test]
    fn service_presets_are_complete_and_pass_their_exact_url_policy() {
        for service in [
            BrowserAuthService::OpenAi,
            BrowserAuthService::Google,
            BrowserAuthService::Anthropic,
        ] {
            let config = BootstrapConfig::for_service(service);
            assert_eq!(config.service, service);
            assert_eq!(
                BootstrapConfig::try_for_service_name(service.as_str())
                    .expect("supported service")
                    .service,
                service
            );
            assert!(super::super::admit_bootstrap_login_url(service, &config.login_url).is_ok());
            assert!(
                !config.success_url_prefixes.is_empty() || !config.success_text_markers.is_empty()
            );
            for success_url in &config.success_url_prefixes {
                assert!(super::super::admit_bootstrap_success_url(service, success_url).is_ok());
            }
        }

        let unsupported = BootstrapConfig::try_for_service_name("unknown-service")
            .expect_err("unsupported service must fail closed");
        assert_eq!(
            unsupported.to_string(),
            "Unsupported browser authentication service"
        );
    }

    #[test]
    fn login_url_override_is_atomic_service_scoped_and_content_free_on_rejection() {
        let mut config = BootstrapConfig::for_service(BrowserAuthService::Google);
        config
            .set_login_url_override("https://accounts.google.com/o/oauth2/v2/auth?client_id=public")
            .expect("admitted Google bootstrap URL");
        assert_eq!(
            config.login_url,
            "https://accounts.google.com/o/oauth2/v2/auth?client_id=public"
        );

        let retained = config.login_url.clone();
        let hostile = "https://auth.openai.com/authorize?token=secret";
        let error = config
            .set_login_url_override(hostile)
            .expect_err("cross-service URL must fail closed");
        assert_eq!(config.login_url, retained);
        assert_eq!(
            error.to_string(),
            "Browser bootstrap configuration is invalid"
        );
        assert!(!error.to_string().contains(hostile));
        assert!(!error.to_string().contains("secret"));
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
    fn arbitrary_origin_config_is_rejected_before_script_execution() {
        let cfg = BootstrapConfig {
            service: BrowserAuthService::OpenAi,
            login_url: "https://custom.auth/login".to_string(),
            timeout_ms: 60_000,
            poll_interval_ms: 1_000,
            success_url_prefixes: vec!["https://app.custom.com".to_string()],
            success_text_markers: vec!["Logged in".to_string()],
        };
        assert_eq!(cfg.timeout_ms, 60_000);
        assert_eq!(cfg.success_url_prefixes.len(), 1);
        assert_eq!(
            InteractiveBootstrap::new(cfg)
                .build_bootstrap_script(Path::new("/tmp/profile"), "https://custom.auth/login",),
            Err(super::super::BrowserNodeCommandFailure::InvalidConfiguration)
        );
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
    fn execute_rejects_invalid_configuration_before_profile_directory_creation() {
        let temp = tempfile::tempdir().expect("isolated bootstrap root");
        let mut ctx =
            super::super::BrowserContext::new(super::super::BrowserConfig::default(), temp.path());
        ctx.status = BrowserStatus::Ready;
        let profile = ctx.profile("openai", "invalid-config");
        let mut config = BootstrapConfig::default();
        config.poll_interval_ms = 0;
        let result = InteractiveBootstrap::new(config).execute(&ctx, &profile, None);
        assert!(matches!(result, BootstrapResult::Failed { .. }));
        assert!(!profile.path().exists());
    }

    #[test]
    fn execute_rejects_profile_from_another_service_before_any_side_effect() {
        let temp = tempfile::tempdir().expect("isolated bootstrap root");
        let mut ctx =
            super::super::BrowserContext::new(super::super::BrowserConfig::default(), temp.path());
        ctx.status = BrowserStatus::Ready;
        let google_profile = ctx.profile("google", "service-mismatch");

        let result = InteractiveBootstrap::with_defaults().execute(&ctx, &google_profile, None);

        assert!(matches!(result, BootstrapResult::Failed { .. }));
        assert!(!google_profile.path().exists());
    }

    #[test]
    fn execute_rejects_invalid_profile_identity_before_any_side_effect() {
        let temp = tempfile::tempdir().expect("isolated bootstrap identity root");
        let mut ctx =
            super::super::BrowserContext::new(super::super::BrowserConfig::default(), temp.path());
        ctx.status = BrowserStatus::Ready;
        let invalid_profile = ctx.profile("openai", "spoof\u{202e}txt");

        let result = InteractiveBootstrap::with_defaults().execute(&ctx, &invalid_profile, None);

        assert!(matches!(result, BootstrapResult::Failed { .. }));
        assert!(!ctx.profiles_root().exists());
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
            .build_bootstrap_script(&profile_dir, "https://auth.openai.com/authorize")
            .expect("bounded bootstrap script");
        let input = super::super::decode_node_script_input(&script);
        assert!(
            input["success_url_prefixes"]
                .as_array()
                .is_some_and(|values| values
                    .iter()
                    .any(|value| value.as_str() == Some("https://platform.openai.com")))
        );
        assert!(
            input["success_text_markers"]
                .as_array()
                .is_some_and(|values| values
                    .iter()
                    .any(|value| value.as_str() == Some("Successfully logged in")))
        );
    }

    #[test]
    fn script_exports_storage_state() {
        let bootstrap = InteractiveBootstrap::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        let script = bootstrap
            .build_bootstrap_script(&profile_dir, "https://auth.openai.com/authorize")
            .expect("bounded bootstrap script");
        assert!(script.contains("storageState"));
    }

    #[test]
    fn parse_success_with_state() {
        let stdout = r#"{"status":"success","storage_state":{"cookies":[],"origins":[]}}"#;
        let result = InteractiveBootstrap::parse_bootstrap_result(stdout);
        match result {
            Ok(ScriptOutcome::Success { storage_state }) => {
                let state_str = String::from_utf8(storage_state).unwrap();
                assert!(state_str.contains("cookies"));
            }
            _ => panic!("Expected Success"),
        }
        assert!(
            InteractiveBootstrap::parse_bootstrap_result(
                r#"{"status":"success","storage_state":"{\"cookies\":[],\"origins\":[]}"}"#
            )
            .is_err()
        );
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
        let hostile_marker = "logged'in\\\n\u{2028}with token=secret";
        let profile_dir = PathBuf::from("/tmp/profile'\\\n\u{2028}");
        let mut config = BootstrapConfig::default();
        config.success_text_markers = vec![hostile_marker.to_string()];
        let bootstrap = InteractiveBootstrap::new(config);
        let script = bootstrap
            .build_bootstrap_script(
                &profile_dir,
                "https://auth.openai.com/authorize?token=secret",
            )
            .expect("bounded hostile bootstrap script");
        assert!(!script.contains(hostile_marker));
        assert!(!script.contains("token=secret"));
        assert!(!script.contains('\u{2028}'));
        let input = super::super::decode_node_script_input(&script);
        assert_eq!(
            input["login_url"],
            "https://auth.openai.com/authorize?token=secret"
        );
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
            .build_bootstrap_script(&PathBuf::from("/tmp"), "https://auth.openai.com/authorize")
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
            .build_bootstrap_script(&PathBuf::from("/tmp"), "https://auth.openai.com/authorize")
            .expect("bounded bootstrap script");
        assert_eq!(
            super::super::decode_node_script_input(&script)["poll_interval_ms"],
            3_500
        );
    }

    #[test]
    fn bootstrap_script_propagates_and_validates_browser_operation_timeouts() {
        let bootstrap = InteractiveBootstrap::with_defaults();
        let browser_config = super::super::BrowserConfig {
            navigation_timeout_ms: 12_345,
            page_load_timeout_ms: 23_456,
            ..super::super::BrowserConfig::default()
        };
        let script = bootstrap
            .build_bootstrap_script_with_browser_config(
                Path::new("/tmp/profile"),
                "https://auth.openai.com/authorize",
                &browser_config,
            )
            .expect("bounded bootstrap timeout configuration");
        let input = super::super::decode_node_script_input(&script);
        assert_eq!(input["navigation_timeout_ms"], 12_345);
        assert_eq!(input["page_load_timeout_ms"], 23_456);
        assert!(script.contains("timeout: NAVIGATION_TIMEOUT"));
        assert!(script.contains("page.setDefaultTimeout(PAGE_LOAD_TIMEOUT)"));

        let invalid = super::super::BrowserConfig {
            page_load_timeout_ms: 0,
            ..super::super::BrowserConfig::default()
        };
        assert_eq!(
            bootstrap.build_bootstrap_script_with_browser_config(
                Path::new("/tmp/profile"),
                "https://auth.openai.com/authorize",
                &invalid,
            ),
            Err(super::super::BrowserNodeCommandFailure::InvalidTimeout)
        );
    }

    #[test]
    fn bootstrap_script_input_enforces_exact_and_one_over_field_limit() {
        let exact = "x".repeat(super::super::BROWSER_NODE_INPUT_MAX_FIELD_BYTES);
        let mut config = BootstrapConfig::default();
        config.success_text_markers = vec![exact.clone()];
        let exact_bootstrap = InteractiveBootstrap::new(config);
        assert!(
            exact_bootstrap
                .build_bootstrap_script(
                    Path::new("/tmp/profile"),
                    "https://auth.openai.com/authorize",
                )
                .is_ok()
        );
        let one_over = format!("{exact}x");
        let mut config = BootstrapConfig::default();
        config.success_text_markers = vec![one_over];
        let oversized_bootstrap = InteractiveBootstrap::new(config);
        assert_eq!(
            oversized_bootstrap.build_bootstrap_script(
                Path::new("/tmp/profile"),
                "https://auth.openai.com/authorize",
            ),
            Err(super::super::BrowserNodeCommandFailure::ScriptOversized)
        );
    }

    #[test]
    fn bootstrap_rejects_invalid_polling_and_unsafe_urls_before_execution() {
        let mut zero_poll = BootstrapConfig::default();
        zero_poll.poll_interval_ms = 0;
        assert_eq!(
            InteractiveBootstrap::new(zero_poll).build_bootstrap_script(
                Path::new("/tmp/profile"),
                "https://auth.openai.com/authorize",
            ),
            Err(super::super::BrowserNodeCommandFailure::InvalidPollInterval)
        );

        let mut over_timeout = BootstrapConfig::default();
        over_timeout.timeout_ms = 1_000;
        over_timeout.poll_interval_ms = 1_001;
        assert_eq!(
            InteractiveBootstrap::new(over_timeout).build_bootstrap_script(
                Path::new("/tmp/profile"),
                "https://auth.openai.com/authorize",
            ),
            Err(super::super::BrowserNodeCommandFailure::InvalidPollInterval)
        );

        assert_eq!(
            InteractiveBootstrap::with_defaults()
                .build_bootstrap_script(Path::new("/tmp/profile"), "file:///tmp/login"),
            Err(super::super::BrowserNodeCommandFailure::InvalidConfiguration)
        );
    }

    #[test]
    fn bootstrap_success_evidence_is_bound_to_an_admitted_destination() {
        let mut marker_only = BootstrapConfig::default();
        marker_only.success_url_prefixes.clear();
        assert_eq!(
            InteractiveBootstrap::new(marker_only).build_bootstrap_script(
                Path::new("/tmp/profile"),
                "https://auth.openai.com/authorize",
            ),
            Err(super::super::BrowserNodeCommandFailure::InvalidConfiguration)
        );

        let mut unproven_root = BootstrapConfig::default();
        unproven_root.success_text_markers.clear();
        assert_eq!(
            InteractiveBootstrap::new(unproven_root).build_bootstrap_script(
                Path::new("/tmp/profile"),
                "https://auth.openai.com/authorize",
            ),
            Err(super::super::BrowserNodeCommandFailure::InvalidConfiguration)
        );

        let mut specific_authenticated_path =
            BootstrapConfig::for_service(BrowserAuthService::Anthropic);
        specific_authenticated_path.success_text_markers.clear();
        assert!(
            InteractiveBootstrap::new(specific_authenticated_path)
                .build_bootstrap_script(
                    Path::new("/tmp/profile"),
                    "https://console.anthropic.com/login",
                )
                .is_ok()
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
        assert!(script.contains("expected.pathname === '/'"));
        assert!(script.contains("current.pathname === '/'"));
        assert!(script.contains("const matchedSuccessUrl"));
        assert!(script.contains("expectedPath !== '/'"));
        assert!(!script.contains("currentUrl.startsWith(prefix)"));
        assert!(script.contains("Math.min(POLL_INTERVAL, remaining)"));
        assert!(!script.contains("markerOriginAllowed"));
        assert!(!script.contains("sameOrigin"));
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
        let start = source
            .find("fn run_bootstrap_script(")
            .expect("runner source");
        let tail = &source[start..];
        let end = tail
            .find("\n    fn build_bootstrap_script_with_browser_config(")
            .expect("runner boundary");
        let body = &tail[..end];
        assert!(body.contains("run_node_script_bounded"));
        assert!(!body.contains("std::process::Command"));
        assert!(!body.contains(".arg(\"-e\")"));
        assert!(!body.contains("stderr_summary"));
    }
}
