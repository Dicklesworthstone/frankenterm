//! Google/Gemini browser auth flow via Playwright.
//!
//! Automates the Google OAuth flow for Gemini CLI accounts, supporting
//! profile-based "already authenticated" fast paths and fallback to
//! interactive bootstrap when MFA/SSO/security-key is required.
//!
//! # Flow
//!
//! ```text
//! navigate → auth_url (or accounts.google.com)
//!        │
//!        ├─ already signed in → detect account avatar/profile → Success
//!        │
//!        ├─ email prompt → fill email → continue
//!        │     ├─ password → InteractiveBootstrapRequired
//!        │     ├─ MFA/security key → InteractiveBootstrapRequired
//!        │     └─ SSO/enterprise IdP → InteractiveBootstrapRequired
//!        │
//!        ├─ "Verify it's you" / captcha → InteractiveBootstrapRequired
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

/// Configuration for the Google OAuth auth flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GoogleAuthConfig {
    /// Default auth URL (used when no URL is captured from CLI output).
    pub auth_url: String,

    /// Timeout for the entire flow in milliseconds (default: 60s).
    pub flow_timeout_ms: u64,

    /// CSS selectors for page elements.
    pub selectors: GooglePageSelectors,
}

impl Default for GoogleAuthConfig {
    fn default() -> Self {
        Self {
            auth_url: "https://accounts.google.com/".to_string(),
            flow_timeout_ms: 60_000,
            selectors: GooglePageSelectors::default(),
        }
    }
}

/// CSS selectors used to identify page elements during the Google auth flow.
///
/// These are separated into a struct so they can be updated when Google
/// changes their UI without modifying flow logic.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GooglePageSelectors {
    /// Element indicating the user is already signed in.
    pub signed_in_marker: String,
    /// Email input field on the sign-in page.
    pub email_input: String,
    /// Next/continue button on the email form.
    pub email_next: String,
    /// Element indicating password entry is required.
    pub password_prompt: String,
    /// Element indicating MFA / 2-step verification.
    pub mfa_indicator: String,
    /// Element indicating security key prompt.
    pub security_key_indicator: String,
    /// Element indicating SSO/enterprise IdP redirect.
    pub sso_indicator: String,
    /// Element indicating captcha or "Verify it's you" challenge.
    pub verify_indicator: String,
}

impl Default for GooglePageSelectors {
    fn default() -> Self {
        Self {
            signed_in_marker:
                "[data-ogsr-up], [data-profileimagecssurl], img[data-src*='googleusercontent']"
                    .to_string(),
            email_input: "input[type='email']".to_string(),
            email_next: "#identifierNext, button[type='button']".to_string(),
            password_prompt: "input[type='password']".to_string(),
            mfa_indicator: "text=2-Step Verification, text=Verify it's you, #totpPin".to_string(),
            security_key_indicator: "text=Use your security key, text=Insert your security key"
                .to_string(),
            sso_indicator: "text=Sign in with your identity provider, [data-sso-redirect]"
                .to_string(),
            verify_indicator:
                "text=Verify it's you, iframe[src*='captcha'], iframe[src*='recaptcha']".to_string(),
        }
    }
}

// =============================================================================
// Auth flow execution
// =============================================================================

/// Orchestrates the Google/Gemini OAuth auth flow.
///
/// This struct holds the configuration and provides the `execute()` method
/// that drives the browser automation via a Playwright subprocess.
pub struct GoogleAuthFlow {
    config: GoogleAuthConfig,
    artifacts: Option<ArtifactCapture>,
}

impl GoogleAuthFlow {
    /// Create a new flow with the given configuration.
    #[must_use]
    pub fn new(config: GoogleAuthConfig) -> Self {
        Self {
            config,
            artifacts: None,
        }
    }

    /// Create a new flow with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(GoogleAuthConfig::default())
    }

    /// Set the artifacts directory for failure debugging.
    #[must_use]
    pub fn with_artifacts(mut self, artifacts_root: impl Into<PathBuf>) -> Self {
        self.artifacts = Some(ArtifactCapture::new(artifacts_root));
        self
    }

    /// Current configuration.
    #[must_use]
    pub fn config(&self) -> &GoogleAuthConfig {
        &self.config
    }

    /// Execute the Google OAuth auth flow.
    ///
    /// # Arguments
    ///
    /// * `ctx` - Browser context (must be in `Ready` state).
    /// * `account` - Account identifier for profile selection.
    /// * `auth_url` - Optional OAuth URL captured from CLI output. Falls back
    ///   to `config.auth_url` if not provided.
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
        auth_url: Option<&str>,
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
        let profile = ctx.profile("google", account);
        let profile_dir = profile.path();

        let target_url = auth_url.unwrap_or(&self.config.auth_url);

        tracing::info!(
            profile_dir = %profile_dir.display(),
            account = %account,
            "Starting Google OAuth auth flow"
        );
        // NOTE: auth_url is intentionally NOT logged (may contain OAuth tokens)

        // Step 3: Build and run the Playwright script
        let start = std::time::Instant::now();
        let artifacts_dir = self.prepare_artifacts_dir();

        let result =
            self.run_playwright_flow(&profile_dir, target_url, email, artifacts_dir.as_deref());

        let elapsed_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(outcome) => match outcome {
                PlaywrightOutcome::Success => {
                    tracing::info!(elapsed_ms, "Google auth flow completed successfully");
                    AuthFlowResult::Success { elapsed_ms }
                }
                PlaywrightOutcome::InteractiveRequired(reason) => {
                    tracing::warn!(
                        elapsed_ms,
                        reason = %reason,
                        "Google auth flow requires interactive login"
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
                    "Google auth flow failed"
                );
                if let Some(ref dir) = artifacts_dir {
                    let report = format!(
                        "Google Auth Flow Failure Report\n\
                         ================================\n\
                         Error: {}\n\
                         Kind: {:?}\n\
                         Elapsed: {elapsed_ms}ms\n\
                         Profile dir: {}\n\
                         Account: {account}\n\
                         Note: auth_url redacted for security\n",
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
            .and_then(|a| match a.ensure_invocation_dir("google_auth") {
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
        auth_url: &str,
        email: Option<&str>,
        artifacts_dir: Option<&Path>,
    ) -> Result<PlaywrightOutcome, PlaywrightFlowError> {
        let script = self.build_playwright_script(profile_dir, auth_url, email, artifacts_dir);

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

    /// Build the Node.js/Playwright script for the Google OAuth flow.
    fn build_playwright_script(
        &self,
        profile_dir: &Path,
        auth_url: &str,
        email: Option<&str>,
        artifacts_dir: Option<&Path>,
    ) -> String {
        let profile_dir_str = profile_dir.display();
        let timeout = self.config.flow_timeout_ms;

        let sel = &self.config.selectors;
        let signed_in_sel = &sel.signed_in_marker;
        let email_input_sel = &sel.email_input;
        let email_next_sel = &sel.email_next;
        let password_sel = &sel.password_prompt;
        let mfa_sel = &sel.mfa_indicator;
        let security_key_sel = &sel.security_key_indicator;
        let sso_sel = &sel.sso_indicator;
        let verify_sel = &sel.verify_indicator;

        let email_js = email
            .map(|e| format!("'{}'", e.replace('\'', "\\'")))
            .unwrap_or_else(|| "null".to_string());

        let artifacts_js = artifacts_dir
            .map(|d| format!("'{}'", d.display()))
            .unwrap_or_else(|| "null".to_string());

        let auth_url_escaped = auth_url.replace('\\', "\\\\").replace('\'', "\\'");

        format!(
            r#"
const {{ chromium }} = require('playwright');

(async () => {{
  const TIMEOUT = {timeout};
  const profileDir = '{profile_dir_str}';
  const authUrl = '{auth_url_escaped}';
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

    // Navigate to auth page
    await page.goto(authUrl, {{ waitUntil: 'domcontentloaded', timeout: TIMEOUT }});

    // Wait for any redirects to settle
    await page.waitForTimeout(2000);

    // Check if already signed in (account avatar/profile visible)
    const signedInSelectors = "{signed_in_sel}".split(', ');
    let alreadySignedIn = false;
    for (const sel of signedInSelectors) {{
      try {{
        const el = await page.$(sel);
        if (el) {{ alreadySignedIn = true; break; }}
      }} catch (_) {{}}
    }}

    if (alreadySignedIn) {{
      console.log(JSON.stringify({{ status: 'success' }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for "Verify it's you" / captcha
    const verifySelectors = "{verify_sel}".split(', ');
    let verifyDetected = false;
    for (const sel of verifySelectors) {{
      try {{
        const el = await page.$(sel);
        if (el) {{ verifyDetected = true; break; }}
      }} catch (_) {{}}
    }}

    if (verifyDetected) {{
      if (artifactsDir) {{
        await page.screenshot({{ path: artifactsDir + '/screenshot.png', fullPage: true }});
      }}
      console.log(JSON.stringify({{
        status: 'interactive_required',
        reason: 'Verification challenge or captcha detected — human intervention required'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for SSO/enterprise IdP redirect
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
        reason: 'SSO/enterprise identity provider detected — human must complete SSO flow'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for security key prompt
    const securityKeySelectors = "{security_key_sel}".split(', ');
    let securityKeyDetected = false;
    for (const sel of securityKeySelectors) {{
      try {{
        const el = await page.$(sel);
        if (el) {{ securityKeyDetected = true; break; }}
      }} catch (_) {{}}
    }}

    if (securityKeyDetected) {{
      if (artifactsDir) {{
        await page.screenshot({{ path: artifactsDir + '/screenshot.png', fullPage: true }});
      }}
      console.log(JSON.stringify({{
        status: 'interactive_required',
        reason: 'Security key prompt detected — human must use physical security key'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for MFA / 2-step verification
    const mfaSelectors = "{mfa_sel}".split(', ');
    let mfaDetected = false;
    for (const sel of mfaSelectors) {{
      try {{
        const el = await page.$(sel);
        if (el) {{ mfaDetected = true; break; }}
      }} catch (_) {{}}
    }}

    if (mfaDetected) {{
      if (artifactsDir) {{
        await page.screenshot({{ path: artifactsDir + '/screenshot.png', fullPage: true }});
      }}
      console.log(JSON.stringify({{
        status: 'interactive_required',
        reason: 'MFA / 2-step verification required — human must complete verification'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for password prompt
    const passwordEl = await page.$('{password_sel}');
    if (passwordEl) {{
      if (artifactsDir) {{
        await page.screenshot({{ path: artifactsDir + '/screenshot.png', fullPage: true }});
      }}
      console.log(JSON.stringify({{
        status: 'interactive_required',
        reason: 'Password prompt detected — interactive bootstrap required'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for email prompt
    const emailEl = await page.$('{email_input_sel}');
    if (emailEl && email) {{
      await emailEl.fill(email);
      const emailNext = await page.$('{email_next_sel}');
      if (emailNext) await emailNext.click();

      // Wait for navigation after email submission
      await page.waitForLoadState('domcontentloaded', {{ timeout: TIMEOUT }});
      await page.waitForTimeout(2000);

      // After email: check for password/MFA/SSO/security key
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

      // Check for MFA after email
      let postEmailMfa = false;
      for (const sel of mfaSelectors) {{
        try {{
          const el = await page.$(sel);
          if (el) {{ postEmailMfa = true; break; }}
        }} catch (_) {{}}
      }}

      if (postEmailMfa) {{
        if (artifactsDir) {{
          await page.screenshot({{ path: artifactsDir + '/screenshot.png', fullPage: true }});
        }}
        console.log(JSON.stringify({{
          status: 'interactive_required',
          reason: 'MFA / 2-step verification after email — human must complete verification'
        }}));
        await browser.close();
        process.exit(0);
      }}

      // Re-check if we landed on a signed-in page (e.g., OAuth consent)
      for (const sel of signedInSelectors) {{
        try {{
          const el = await page.$(sel);
          if (el) {{ alreadySignedIn = true; break; }}
        }} catch (_) {{}}
      }}

      if (alreadySignedIn) {{
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

    // Unrecognized page state
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
    /// Interactive login is required (password/MFA/SSO/captcha/security key).
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
    fn default_config_has_google_url() {
        let config = GoogleAuthConfig::default();
        assert!(config.auth_url.contains("accounts.google.com"));
        assert_eq!(config.flow_timeout_ms, 60_000);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = GoogleAuthConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: GoogleAuthConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.auth_url, config.auth_url);
        assert_eq!(parsed.flow_timeout_ms, config.flow_timeout_ms);
    }

    #[test]
    fn selectors_have_sensible_defaults() {
        let sel = GooglePageSelectors::default();
        assert!(!sel.signed_in_marker.is_empty());
        assert!(!sel.email_input.is_empty());
        assert!(!sel.password_prompt.is_empty());
        assert!(!sel.mfa_indicator.is_empty());
        assert!(!sel.security_key_indicator.is_empty());
        assert!(!sel.sso_indicator.is_empty());
        assert!(!sel.verify_indicator.is_empty());
    }

    // =========================================================================
    // AuthFlowResult serde (reused from openai_device)
    // =========================================================================

    #[test]
    fn success_result_serializes() {
        let result = AuthFlowResult::Success { elapsed_ms: 500 };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("success"));
        assert!(json.contains("500"));
    }

    #[test]
    fn interactive_required_serializes() {
        let result = AuthFlowResult::InteractiveBootstrapRequired {
            reason: "MFA required".to_string(),
            artifacts_dir: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("interactive_required"));
        assert!(json.contains("MFA required"));
    }

    #[test]
    fn failed_result_serializes() {
        let result = AuthFlowResult::Failed {
            error: "Navigation timeout".to_string(),
            kind: AuthFlowFailureKind::NavigationFailed,
            artifacts_dir: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("failed"));
        assert!(json.contains("NavigationFailed"));
    }

    // =========================================================================
    // Flow construction
    // =========================================================================

    #[test]
    fn flow_with_defaults_creates_valid_config() {
        let flow = GoogleAuthFlow::with_defaults();
        assert!(flow.config().auth_url.contains("accounts.google.com"));
    }

    #[test]
    fn flow_with_custom_config() {
        let config = GoogleAuthConfig {
            auth_url: "https://custom.google.com/oauth".to_string(),
            flow_timeout_ms: 30_000,
            selectors: GooglePageSelectors::default(),
        };
        let flow = GoogleAuthFlow::new(config);
        assert_eq!(flow.config().auth_url, "https://custom.google.com/oauth");
        assert_eq!(flow.config().flow_timeout_ms, 30_000);
    }

    #[test]
    fn flow_with_artifacts_sets_capture() {
        let flow = GoogleAuthFlow::with_defaults().with_artifacts("/tmp/test_artifacts");
        assert!(flow.artifacts.is_some());
    }

    // =========================================================================
    // Flow execution guards
    // =========================================================================

    #[test]
    fn execute_fails_when_browser_not_ready() {
        let flow = GoogleAuthFlow::with_defaults();
        let ctx = BrowserContext::new(
            super::super::BrowserConfig::default(),
            Path::new("/tmp/test_data"),
        );
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
        let result = GoogleAuthFlow::parse_playwright_result(stdout);
        assert!(matches!(result, Ok(PlaywrightOutcome::Success)));
    }

    #[test]
    fn parse_interactive_required_result() {
        let stdout =
            r#"{"status":"interactive_required","reason":"MFA / 2-step verification required"}"#;
        let result = GoogleAuthFlow::parse_playwright_result(stdout);
        match result {
            Ok(PlaywrightOutcome::InteractiveRequired(reason)) => {
                assert!(reason.contains("MFA"));
            }
            _ => panic!("Expected InteractiveRequired"),
        }
    }

    #[test]
    fn parse_error_result() {
        let stdout =
            r#"{"status":"error","kind":"SelectorMismatch","message":"No selectors matched"}"#;
        let result = GoogleAuthFlow::parse_playwright_result(stdout);
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
        let result = GoogleAuthFlow::parse_playwright_result("");
        assert!(result.is_err());
    }

    #[test]
    fn parse_result_finds_last_json_line() {
        let stdout = "debug output\n{\"status\":\"success\"}";
        let result = GoogleAuthFlow::parse_playwright_result(stdout);
        assert!(matches!(result, Ok(PlaywrightOutcome::Success)));
    }

    #[test]
    fn parse_bot_detected_error() {
        let stdout = r#"{"status":"error","kind":"BotDetected","message":"suspicious activity"}"#;
        let result = GoogleAuthFlow::parse_playwright_result(stdout);
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
    fn script_contains_auth_url() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://accounts.google.com/",
            None,
            None,
        );
        assert!(script.contains("accounts.google.com"));
    }

    #[test]
    fn script_contains_email_when_provided() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://accounts.google.com/",
            Some("user@gmail.com"),
            None,
        );
        assert!(script.contains("user@gmail.com"));
    }

    #[test]
    fn script_has_null_email_when_not_provided() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://accounts.google.com/",
            None,
            None,
        );
        assert!(script.contains("const email = null;"));
    }

    #[test]
    fn script_checks_for_signed_in_markers() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://accounts.google.com/",
            None,
            None,
        );
        assert!(script.contains("alreadySignedIn"));
        assert!(script.contains("googleusercontent"));
    }

    #[test]
    fn script_checks_for_password_prompt() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://accounts.google.com/",
            None,
            None,
        );
        assert!(script.contains("password"));
        assert!(script.contains("interactive_required"));
    }

    #[test]
    fn script_checks_for_mfa() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://accounts.google.com/",
            None,
            None,
        );
        assert!(script.contains("2-Step Verification"));
        assert!(script.contains("mfaDetected"));
    }

    #[test]
    fn script_checks_for_security_key() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://accounts.google.com/",
            None,
            None,
        );
        assert!(script.contains("security key"));
        assert!(script.contains("securityKeyDetected"));
    }

    #[test]
    fn script_checks_for_sso() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://accounts.google.com/",
            None,
            None,
        );
        assert!(script.contains("identity provider"));
        assert!(script.contains("ssoDetected"));
    }

    #[test]
    fn script_uses_custom_oauth_url() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow.build_playwright_script(
            Path::new("/tmp/profile"),
            "https://accounts.google.com/o/oauth2/auth?client_id=123",
            None,
            None,
        );
        assert!(script.contains("oauth2/auth?client_id=123"));
    }

    // =========================================================================
    // Playwright error parsing
    // =========================================================================

    #[test]
    fn parse_playwright_error_from_json() {
        let stdout = r#"{"status":"error","kind":"NavigationFailed","message":"timeout"}"#;
        let error =
            GoogleAuthFlow::parse_playwright_error(stdout, "", std::process::ExitStatus::default());
        assert_eq!(error.kind, AuthFlowFailureKind::NavigationFailed);
        assert!(error.error.contains("timeout"));
    }

    #[test]
    fn parse_playwright_error_from_stderr_fallback() {
        let error = GoogleAuthFlow::parse_playwright_error(
            "",
            "Error: Browser closed unexpectedly",
            std::process::ExitStatus::default(),
        );
        assert_eq!(error.kind, AuthFlowFailureKind::PlaywrightError);
        assert!(error.error.contains("Browser closed"));
    }
}
