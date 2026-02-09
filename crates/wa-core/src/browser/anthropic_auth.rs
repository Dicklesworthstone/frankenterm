//! Anthropic/Claude Code browser auth flow via Playwright.
//!
//! Automates the login flow for Anthropic accounts, supporting both
//! profile-based "already authenticated" fast paths and fallback to
//! interactive bootstrap when password/MFA/SSO is required.
//!
//! # Flow
//!
//! ```text
//! navigate → login_url (or console.anthropic.com)
//!        │
//!        ├─ already logged in → detect dashboard/console → Success
//!        │
//!        ├─ email prompt → fill email → continue
//!        │     ├─ password/MFA → InteractiveBootstrapRequired
//!        │     └─ SSO redirect → InteractiveBootstrapRequired
//!        │
//!        └─ unknown page state → capture artifacts → Failed
//! ```
//!
//! # Safety
//!
//! - Passwords, tokens, cookies, and session data are **never** logged.
//! - On failure, artifacts (screenshot, redacted DOM snippet) are saved to the
//!   workspace artifacts directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::openai_device::{ArtifactCapture, ArtifactKind, AuthFlowFailureKind, AuthFlowResult};
use super::{BrowserContext, BrowserStatus};

// =============================================================================
// Auth flow configuration
// =============================================================================

/// Configuration for the Anthropic login auth flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnthropicAuthConfig {
    /// Default login URL (used when no URL is captured from CLI output).
    pub login_url: String,

    /// Timeout for the entire flow in milliseconds (default: 60s).
    pub flow_timeout_ms: u64,

    /// CSS selectors for page elements.
    pub selectors: AnthropicPageSelectors,
}

impl Default for AnthropicAuthConfig {
    fn default() -> Self {
        Self {
            login_url: "https://console.anthropic.com/login".to_string(),
            flow_timeout_ms: 60_000,
            selectors: AnthropicPageSelectors::default(),
        }
    }
}

/// CSS selectors used to identify page elements during the Anthropic auth flow.
///
/// These are separated into a struct so they can be updated when Anthropic
/// changes their UI without modifying flow logic.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnthropicPageSelectors {
    /// Element indicating the user is already logged in (dashboard/console).
    pub logged_in_marker: String,
    /// Email input field on the login page.
    pub email_input: String,
    /// Continue/submit button on the email form.
    pub email_submit: String,
    /// Element indicating password entry is required.
    pub password_prompt: String,
    /// Element indicating SSO/enterprise redirect.
    pub sso_indicator: String,
    /// Element indicating a captcha or bot challenge.
    pub captcha_indicator: String,
}

impl Default for AnthropicPageSelectors {
    fn default() -> Self {
        Self {
            logged_in_marker:
                "text=Dashboard, text=API Keys, text=Welcome back, [data-testid='dashboard']"
                    .to_string(),
            email_input: "input[name='email'], input[type='email']".to_string(),
            email_submit: "button[type='submit']".to_string(),
            password_prompt: "input[type='password']".to_string(),
            sso_indicator: "text=SSO, text=Single Sign-On, text=Continue with SSO".to_string(),
            captcha_indicator:
                "iframe[src*='captcha'], iframe[src*='recaptcha'], [class*='captcha']".to_string(),
        }
    }
}

// =============================================================================
// Auth flow execution
// =============================================================================

/// Orchestrates the Anthropic/Claude Code login auth flow.
///
/// This struct holds the configuration and provides the `execute()` method
/// that drives the browser automation via a Playwright subprocess.
pub struct AnthropicAuthFlow {
    config: AnthropicAuthConfig,
    artifacts: Option<ArtifactCapture>,
}

impl AnthropicAuthFlow {
    /// Create a new flow with the given configuration.
    #[must_use]
    pub fn new(config: AnthropicAuthConfig) -> Self {
        Self {
            config,
            artifacts: None,
        }
    }

    /// Create a new flow with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(AnthropicAuthConfig::default())
    }

    /// Set the artifacts directory for failure debugging.
    #[must_use]
    pub fn with_artifacts(mut self, artifacts_root: impl Into<PathBuf>) -> Self {
        self.artifacts = Some(ArtifactCapture::new(artifacts_root));
        self
    }

    /// Current configuration.
    #[must_use]
    pub fn config(&self) -> &AnthropicAuthConfig {
        &self.config
    }

    /// Execute the Anthropic login auth flow.
    ///
    /// # Arguments
    ///
    /// * `ctx` - Browser context (must be in `Ready` state).
    /// * `account` - Account identifier for profile selection.
    /// * `login_url` - Optional login URL captured from CLI output. Falls back
    ///   to `config.login_url` if not provided.
    /// * `email` - Optional email for auto-fill if an email prompt appears.
    ///
    /// # Returns
    ///
    /// An [`AuthFlowResult`] indicating success, interactive-bootstrap-required,
    /// or failure with details.
    pub fn execute(
        &self,
        ctx: &BrowserContext,
        account: &str,
        login_url: Option<&str>,
        email: Option<&str>,
    ) -> AuthFlowResult {
        // Step 1: Verify browser context is ready
        if *ctx.status() != BrowserStatus::Ready {
            return AuthFlowResult::Failed {
                error: format!("Browser context not ready: {:?}", ctx.status()),
                kind: AuthFlowFailureKind::BrowserNotReady,
                artifacts_dir: None,
            };
        }

        // Step 2: Resolve the browser profile
        let profile = ctx.profile("anthropic", account);
        let profile_dir = profile.path();

        let target_url = login_url.unwrap_or(&self.config.login_url);

        tracing::info!(
            profile_dir = %profile_dir.display(),
            account = %account,
            login_url = %target_url,
            "Starting Anthropic auth flow"
        );

        // Step 3: Build and run the Playwright script
        let start = std::time::Instant::now();
        let artifacts_dir = self.prepare_artifacts_dir();

        let result =
            self.run_playwright_flow(&profile_dir, target_url, email, artifacts_dir.as_deref());

        let elapsed_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(outcome) => match outcome {
                PlaywrightOutcome::Success => {
                    tracing::info!(elapsed_ms, "Anthropic auth flow completed successfully");
                    AuthFlowResult::Success { elapsed_ms }
                }
                PlaywrightOutcome::InteractiveRequired(reason) => {
                    tracing::warn!(
                        elapsed_ms,
                        reason = %reason,
                        "Anthropic auth flow requires interactive login"
                    );
                    AuthFlowResult::InteractiveBootstrapRequired {
                        reason,
                        artifacts_dir,
                    }
                }
            },
            Err(e) => {
                tracing::error!(
                    elapsed_ms,
                    error = %e.error,
                    kind = ?e.kind,
                    "Anthropic auth flow failed"
                );
                // Write failure report artifact if we have an artifacts dir
                if let Some(ref dir) = artifacts_dir {
                    let report = format!(
                        "Anthropic Auth Flow Failure Report\n\
                         ===================================\n\
                         Error: {}\n\
                         Kind: {:?}\n\
                         Elapsed: {elapsed_ms}ms\n\
                         Login URL: {target_url}\n\
                         Profile dir: {}\n\
                         Account: {account}\n",
                        e.error,
                        e.kind,
                        profile_dir.display(),
                    );
                    let _ = ArtifactCapture::write_artifact(
                        dir,
                        ArtifactKind::FailureReport,
                        report.as_bytes(),
                    );
                }
                AuthFlowResult::Failed {
                    error: e.error,
                    kind: e.kind,
                    artifacts_dir,
                }
            }
        }
    }

    /// Prepare the artifacts directory for this invocation, if configured.
    fn prepare_artifacts_dir(&self) -> Option<PathBuf> {
        self.artifacts
            .as_ref()
            .and_then(|a| match a.ensure_invocation_dir("anthropic_auth") {
                Ok(dir) => Some(dir),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Failed to create artifacts directory; continuing without artifacts"
                    );
                    None
                }
            })
    }

    /// Run the Playwright subprocess that performs the actual browser automation.
    fn run_playwright_flow(
        &self,
        profile_dir: &Path,
        login_url: &str,
        email: Option<&str>,
        artifacts_dir: Option<&Path>,
    ) -> Result<PlaywrightOutcome, PlaywrightFlowError> {
        let script = self.build_playwright_script(profile_dir, login_url, email, artifacts_dir);

        let output = std::process::Command::new("node")
            .arg("-e")
            .arg(&script)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|e| PlaywrightFlowError {
                error: format!("Failed to spawn node process: {e}"),
                kind: AuthFlowFailureKind::PlaywrightError,
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Log stderr at debug level (may contain Playwright progress info)
        // but NEVER log stdout which may contain page content with secrets
        if !stderr.is_empty() {
            tracing::debug!(
                stderr_lines = stderr.lines().count(),
                "Playwright subprocess stderr (content redacted in logs)"
            );
        }

        if !output.status.success() {
            return Err(Self::parse_playwright_error(
                &stdout,
                &stderr,
                output.status,
            ));
        }

        Self::parse_playwright_result(&stdout)
    }

    /// Build the Node.js/Playwright script for the Anthropic auth flow.
    ///
    /// The script outputs a JSON result to stdout with one of:
    /// - `{"status":"success"}`
    /// - `{"status":"interactive_required","reason":"..."}`
    /// - `{"status":"error","kind":"...","message":"..."}`
    fn build_playwright_script(
        &self,
        profile_dir: &Path,
        login_url: &str,
        email: Option<&str>,
        artifacts_dir: Option<&Path>,
    ) -> String {
        let profile_dir_str = profile_dir.display();
        let timeout = self.config.flow_timeout_ms;

        let sel = &self.config.selectors;
        let logged_in_sel = &sel.logged_in_marker;
        let email_input_sel = &sel.email_input;
        let email_submit_sel = &sel.email_submit;
        let password_sel = &sel.password_prompt;
        let sso_sel = &sel.sso_indicator;
        let captcha_sel = &sel.captcha_indicator;

        let email_js = email
            .map(|e| format!("'{}'", e.replace('\'', "\\'")))
            .unwrap_or_else(|| "null".to_string());

        let artifacts_js = artifacts_dir
            .map(|d| format!("'{}'", d.display()))
            .unwrap_or_else(|| "null".to_string());

        let login_url_escaped = login_url.replace('\\', "\\\\").replace('\'', "\\'");

        format!(
            r#"
const {{ chromium }} = require('playwright');

(async () => {{
  const TIMEOUT = {timeout};
  const profileDir = '{profile_dir_str}';
  const loginUrl = '{login_url_escaped}';
  const email = {email_js};
  const artifactsDir = {artifacts_js};

  let browser, context, page;
  try {{
    browser = await chromium.launchPersistentContext(profileDir, {{
      headless: false,
      timeout: TIMEOUT,
    }});
    page = browser.pages()[0] || await browser.newPage();
    page.setDefaultTimeout(TIMEOUT);

    // Navigate to login page
    await page.goto(loginUrl, {{ waitUntil: 'domcontentloaded', timeout: TIMEOUT }});

    // Wait a moment for any redirects to settle
    await page.waitForTimeout(2000);

    // Check if already logged in (dashboard/console visible)
    const loggedInSelectors = "{logged_in_sel}".split(', ');
    let alreadyLoggedIn = false;
    for (const sel of loggedInSelectors) {{
      try {{
        const el = await page.$(sel);
        if (el) {{ alreadyLoggedIn = true; break; }}
      }} catch (_) {{}}
    }}

    if (alreadyLoggedIn) {{
      console.log(JSON.stringify({{ status: 'success' }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for captcha / bot challenge
    const captchaEl = await page.$('{captcha_sel}');
    if (captchaEl) {{
      if (artifactsDir) {{
        await page.screenshot({{ path: artifactsDir + '/screenshot.png', fullPage: true }});
      }}
      console.log(JSON.stringify({{
        status: 'interactive_required',
        reason: 'Captcha or bot challenge detected — human intervention required'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for SSO redirect
    const ssoSelectors = "{sso_sel}".split(', ');
    let ssoDetected = false;
    for (const sel of ssoSelectors) {{
      try {{
        const el = await page.$(sel);
        if (el) {{ ssoDetected = true; break; }}
      }} catch (_) {{}}
    }}

    if (ssoDetected) {{
      if (artifactsDir) {{
        await page.screenshot({{ path: artifactsDir + '/screenshot.png', fullPage: true }});
      }}
      console.log(JSON.stringify({{
        status: 'interactive_required',
        reason: 'SSO/enterprise login detected — human must complete SSO flow'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for password prompt (before email, in case of pre-filled email)
    const passwordEl = await page.$('{password_sel}');
    if (passwordEl) {{
      if (artifactsDir) {{
        await page.screenshot({{ path: artifactsDir + '/screenshot.png', fullPage: true }});
      }}
      console.log(JSON.stringify({{
        status: 'interactive_required',
        reason: 'Password or MFA prompt detected — interactive bootstrap required'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for email prompt
    const emailEl = await page.$('{email_input_sel}');
    if (emailEl && email) {{
      await emailEl.fill(email);
      const emailSubmit = await page.$('{email_submit_sel}');
      if (emailSubmit) await emailSubmit.click();

      // Wait for navigation after email submission
      await page.waitForLoadState('domcontentloaded', {{ timeout: TIMEOUT }});
      await page.waitForTimeout(2000);

      // After email: check for password/MFA/SSO
      const postEmailPassword = await page.$('{password_sel}');
      if (postEmailPassword) {{
        if (artifactsDir) {{
          await page.screenshot({{ path: artifactsDir + '/screenshot.png', fullPage: true }});
        }}
        console.log(JSON.stringify({{
          status: 'interactive_required',
          reason: 'Password required after email entry — interactive bootstrap required'
        }}));
        await browser.close();
        process.exit(0);
      }}

      // Check for SSO redirect after email
      let postEmailSso = false;
      for (const sel of ssoSelectors) {{
        try {{
          const el = await page.$(sel);
          if (el) {{ postEmailSso = true; break; }}
        }} catch (_) {{}}
      }}

      if (postEmailSso) {{
        if (artifactsDir) {{
          await page.screenshot({{ path: artifactsDir + '/screenshot.png', fullPage: true }});
        }}
        console.log(JSON.stringify({{
          status: 'interactive_required',
          reason: 'SSO redirect after email entry — human must complete SSO flow'
        }}));
        await browser.close();
        process.exit(0);
      }}

      // Re-check if we landed on a logged-in page
      for (const sel of loggedInSelectors) {{
        try {{
          const el = await page.$(sel);
          if (el) {{ alreadyLoggedIn = true; break; }}
        }} catch (_) {{}}
      }}

      if (alreadyLoggedIn) {{
        console.log(JSON.stringify({{ status: 'success' }}));
        await browser.close();
        process.exit(0);
      }}
    }} else if (emailEl && !email) {{
      if (artifactsDir) {{
        await page.screenshot({{ path: artifactsDir + '/screenshot.png', fullPage: true }});
      }}
      console.log(JSON.stringify({{
        status: 'interactive_required',
        reason: 'Email prompt detected but no email provided — interactive bootstrap required'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // If we reach here, we're in an unrecognized page state
    if (artifactsDir) {{
      await page.screenshot({{ path: artifactsDir + '/screenshot.png', fullPage: true }});
    }}
    console.log(JSON.stringify({{
      status: 'error',
      kind: 'SelectorMismatch',
      message: 'Could not determine page state — no recognized selectors matched'
    }}));

    await browser.close();
  }} catch (err) {{
    if (page && artifactsDir) {{
      try {{
        await page.screenshot({{ path: artifactsDir + '/screenshot.png', fullPage: true }});
      }} catch (_) {{}}
    }}
    console.log(JSON.stringify({{
      status: 'error',
      kind: 'PlaywrightError',
      message: err.message
    }}));
    if (browser) await browser.close().catch(() => {{}});
    process.exit(1);
  }}
}})();
"#
        )
    }

    /// Parse a successful Playwright script result from stdout JSON.
    fn parse_playwright_result(stdout: &str) -> Result<PlaywrightOutcome, PlaywrightFlowError> {
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return Err(PlaywrightFlowError {
                error: "Playwright script produced no output".to_string(),
                kind: AuthFlowFailureKind::PlaywrightError,
            });
        }

        let json_line = trimmed
            .lines()
            .rev()
            .find(|line| line.starts_with('{'))
            .unwrap_or(trimmed);

        let parsed: serde_json::Value =
            serde_json::from_str(json_line).map_err(|e| PlaywrightFlowError {
                error: format!("Failed to parse Playwright output as JSON: {e}"),
                kind: AuthFlowFailureKind::PlaywrightError,
            })?;

        match parsed.get("status").and_then(|s| s.as_str()) {
            Some("success") => Ok(PlaywrightOutcome::Success),
            Some("interactive_required") => {
                let reason = parsed
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("interactive login required")
                    .to_string();
                Ok(PlaywrightOutcome::InteractiveRequired(reason))
            }
            Some("error") => {
                let kind_str = parsed
                    .get("kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or("Unknown");
                let message = parsed
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
                    .to_string();
                let kind = match kind_str {
                    "VerificationFailed" => AuthFlowFailureKind::VerificationFailed,
                    "SelectorMismatch" => AuthFlowFailureKind::SelectorMismatch,
                    "NavigationFailed" => AuthFlowFailureKind::NavigationFailed,
                    "BotDetected" => AuthFlowFailureKind::BotDetected,
                    _ => AuthFlowFailureKind::PlaywrightError,
                };
                Err(PlaywrightFlowError {
                    error: message,
                    kind,
                })
            }
            _ => Err(PlaywrightFlowError {
                error: format!("Unexpected Playwright output status: {json_line}"),
                kind: AuthFlowFailureKind::Unknown,
            }),
        }
    }

    /// Parse error information from a failed Playwright subprocess.
    fn parse_playwright_error(
        stdout: &str,
        stderr: &str,
        status: std::process::ExitStatus,
    ) -> PlaywrightFlowError {
        if let Some(json_line) = stdout.trim().lines().rev().find(|l| l.starts_with('{')) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_line) {
                if let Some(message) = parsed.get("message").and_then(|m| m.as_str()) {
                    let kind_str = parsed
                        .get("kind")
                        .and_then(|k| k.as_str())
                        .unwrap_or("PlaywrightError");
                    let kind = match kind_str {
                        "VerificationFailed" => AuthFlowFailureKind::VerificationFailed,
                        "SelectorMismatch" => AuthFlowFailureKind::SelectorMismatch,
                        "NavigationFailed" => AuthFlowFailureKind::NavigationFailed,
                        "BotDetected" => AuthFlowFailureKind::BotDetected,
                        _ => AuthFlowFailureKind::PlaywrightError,
                    };
                    return PlaywrightFlowError {
                        error: message.to_string(),
                        kind,
                    };
                }
            }
        }

        let stderr_summary = stderr.lines().take(5).collect::<Vec<_>>().join("; ");

        PlaywrightFlowError {
            error: format!("Playwright process exited with {status}: {stderr_summary}"),
            kind: AuthFlowFailureKind::PlaywrightError,
        }
    }
}

/// Internal outcome from the Playwright subprocess.
enum PlaywrightOutcome {
    /// Flow completed successfully (already authenticated).
    Success,
    /// Interactive login is required (password/MFA/SSO/captcha).
    InteractiveRequired(String),
}

/// Internal error from the Playwright subprocess.
struct PlaywrightFlowError {
    error: String,
    kind: AuthFlowFailureKind,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Config & selectors
    // =========================================================================

    #[test]
    fn default_config_has_anthropic_url() {
        let config = AnthropicAuthConfig::default();
        assert!(config.login_url.contains("anthropic.com"));
        assert_eq!(config.flow_timeout_ms, 60_000);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = AnthropicAuthConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AnthropicAuthConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.login_url, config.login_url);
        assert_eq!(parsed.flow_timeout_ms, config.flow_timeout_ms);
    }

    #[test]
    fn selectors_have_sensible_defaults() {
        let sel = AnthropicPageSelectors::default();
        assert!(!sel.logged_in_marker.is_empty());
        assert!(!sel.email_input.is_empty());
        assert!(!sel.password_prompt.is_empty());
        assert!(!sel.sso_indicator.is_empty());
        assert!(!sel.captcha_indicator.is_empty());
    }

    // =========================================================================
    // AuthFlowResult serde (reused from openai_device)
    // =========================================================================

    #[test]
    fn success_result_serializes() {
        let result = AuthFlowResult::Success { elapsed_ms: 1234 };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("success"));
        assert!(json.contains("1234"));
    }

    #[test]
    fn interactive_required_serializes() {
        let result = AuthFlowResult::InteractiveBootstrapRequired {
            reason: "Password required".to_string(),
            artifacts_dir: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("interactive_required"));
        assert!(json.contains("Password required"));
    }

    #[test]
    fn failed_result_serializes() {
        let result = AuthFlowResult::Failed {
            error: "Selector mismatch".to_string(),
            kind: AuthFlowFailureKind::SelectorMismatch,
            artifacts_dir: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("failed"));
        assert!(json.contains("SelectorMismatch"));
    }

    // =========================================================================
    // Flow construction
    // =========================================================================

    #[test]
    fn flow_with_defaults_creates_valid_config() {
        let flow = AnthropicAuthFlow::with_defaults();
        assert!(flow.config().login_url.contains("anthropic.com"));
    }

    #[test]
    fn flow_with_custom_config() {
        let config = AnthropicAuthConfig {
            login_url: "https://custom.example.com/login".to_string(),
            flow_timeout_ms: 30_000,
            selectors: AnthropicPageSelectors::default(),
        };
        let flow = AnthropicAuthFlow::new(config);
        assert_eq!(flow.config().login_url, "https://custom.example.com/login");
        assert_eq!(flow.config().flow_timeout_ms, 30_000);
    }

    #[test]
    fn flow_with_artifacts_sets_capture() {
        let flow = AnthropicAuthFlow::with_defaults().with_artifacts("/tmp/test_artifacts");
        assert!(flow.artifacts.is_some());
    }

    // =========================================================================
    // Flow execution guards
    // =========================================================================

    #[test]
    fn execute_fails_when_browser_not_ready() {
        let flow = AnthropicAuthFlow::with_defaults();
        let ctx = BrowserContext::new(
            super::super::BrowserConfig::default(),
            Path::new("/tmp/test_data"),
        );
        // Context starts as NotInitialized
        let result = flow.execute(&ctx, "test-account", None, None);
        match result {
            AuthFlowResult::Failed { kind, .. } => {
                assert_eq!(kind, AuthFlowFailureKind::BrowserNotReady);
            }
            _ => panic!("Expected Failed result for uninitialized browser"),
        }
    }

    // =========================================================================
    // Playwright result parsing
    // =========================================================================

    #[test]
    fn parse_success_result() {
        let stdout = r#"{"status":"success"}"#;
        let result = AnthropicAuthFlow::parse_playwright_result(stdout);
        assert!(matches!(result, Ok(PlaywrightOutcome::Success)));
    }

    #[test]
    fn parse_interactive_required_result() {
        let stdout =
            r#"{"status":"interactive_required","reason":"Password required after email entry"}"#;
        let result = AnthropicAuthFlow::parse_playwright_result(stdout);
        match result {
            Ok(PlaywrightOutcome::InteractiveRequired(reason)) => {
                assert!(reason.contains("Password required"));
            }
            _ => panic!("Expected InteractiveRequired"),
        }
    }

    #[test]
    fn parse_error_result() {
        let stdout =
            r#"{"status":"error","kind":"SelectorMismatch","message":"No selectors matched"}"#;
        let result = AnthropicAuthFlow::parse_playwright_result(stdout);
        match result {
            Err(e) => {
                assert_eq!(e.kind, AuthFlowFailureKind::SelectorMismatch);
                assert!(e.error.contains("No selectors matched"));
            }
            _ => panic!("Expected error"),
        }
    }

    #[test]
    fn parse_empty_stdout_returns_error() {
        let result = AnthropicAuthFlow::parse_playwright_result("");
        assert!(result.is_err());
    }

    #[test]
    fn parse_result_finds_last_json_line() {
        let stdout = "some debug output\nmore output\n{\"status\":\"success\"}";
        let result = AnthropicAuthFlow::parse_playwright_result(stdout);
        assert!(matches!(result, Ok(PlaywrightOutcome::Success)));
    }

    #[test]
    fn parse_bot_detected_error() {
        let stdout = r#"{"status":"error","kind":"BotDetected","message":"rate limited"}"#;
        let result = AnthropicAuthFlow::parse_playwright_result(stdout);
        match result {
            Err(e) => {
                assert_eq!(e.kind, AuthFlowFailureKind::BotDetected);
            }
            _ => panic!("Expected BotDetected error"),
        }
    }

    // =========================================================================
    // Playwright script generation
    // =========================================================================

    #[test]
    fn script_contains_login_url() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://console.anthropic.com/login",
            None,
            None,
        );
        assert!(script.contains("console.anthropic.com/login"));
    }

    #[test]
    fn script_contains_email_when_provided() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://console.anthropic.com/login",
            Some("user@example.com"),
            None,
        );
        assert!(script.contains("user@example.com"));
    }

    #[test]
    fn script_has_null_email_when_not_provided() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://console.anthropic.com/login",
            None,
            None,
        );
        assert!(script.contains("const email = null;"));
    }

    #[test]
    fn script_uses_custom_login_url() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://custom.example.com/auth",
            None,
            None,
        );
        assert!(script.contains("custom.example.com/auth"));
    }

    #[test]
    fn script_escapes_single_quotes_in_url() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://example.com/login?foo='bar'",
            None,
            None,
        );
        // Single quotes should be escaped
        assert!(script.contains("\\'bar\\'"));
    }

    #[test]
    fn script_checks_for_logged_in_markers() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://console.anthropic.com/login",
            None,
            None,
        );
        assert!(script.contains("Dashboard"));
        assert!(script.contains("alreadyLoggedIn"));
    }

    #[test]
    fn script_checks_for_password_prompt() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://console.anthropic.com/login",
            None,
            None,
        );
        assert!(script.contains("password"));
        assert!(script.contains("interactive_required"));
    }

    #[test]
    fn script_checks_for_sso() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://console.anthropic.com/login",
            None,
            None,
        );
        assert!(script.contains("SSO"));
        assert!(script.contains("ssoDetected"));
    }

    #[test]
    fn script_checks_for_captcha() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://console.anthropic.com/login",
            None,
            None,
        );
        assert!(script.contains("captcha"));
    }

    // =========================================================================
    // Playwright error parsing
    // =========================================================================

    #[test]
    fn parse_playwright_error_from_json() {
        let stdout = r#"{"status":"error","kind":"NavigationFailed","message":"timeout"}"#;
        let error = AnthropicAuthFlow::parse_playwright_error(
            stdout,
            "",
            std::process::ExitStatus::default(),
        );
        assert_eq!(error.kind, AuthFlowFailureKind::NavigationFailed);
        assert!(error.error.contains("timeout"));
    }

    #[test]
    fn parse_playwright_error_from_stderr_fallback() {
        let error = AnthropicAuthFlow::parse_playwright_error(
            "",
            "Error: Browser closed unexpectedly",
            std::process::ExitStatus::default(),
        );
        assert_eq!(error.kind, AuthFlowFailureKind::PlaywrightError);
        assert!(error.error.contains("Browser closed"));
    }
}
