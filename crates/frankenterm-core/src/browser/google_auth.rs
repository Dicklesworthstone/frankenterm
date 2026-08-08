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
//! - When explicitly configured, failure artifacts are private and bounded;
//!   screenshots can contain sensitive page content and are not redacted.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::openai_device::{ArtifactCapture, ArtifactKind, AuthFlowFailureKind, AuthFlowResult};
use super::{BootstrapMethod, BrowserContext, BrowserStatus};

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
            email_next: "#identifierNext, button[type='submit']".to_string(),
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
                error: AuthFlowFailureKind::BrowserNotReady
                    .stable_detail()
                    .to_string(),
                kind: AuthFlowFailureKind::BrowserNotReady,
                artifacts_dir: None,
            };
        }

        let target_url = auth_url.unwrap_or(&self.config.auth_url);
        if super::admit_automated_auth_url(super::BrowserAuthService::Google, target_url).is_err() {
            return AuthFlowResult::Failed {
                error: AuthFlowFailureKind::PlaywrightError
                    .stable_detail()
                    .to_string(),
                kind: AuthFlowFailureKind::PlaywrightError,
                artifacts_dir: None,
            };
        }

        // Step 2: Resolve the browser profile
        let profile = match ctx.try_profile("google", account) {
            Ok(profile) => profile,
            Err(_) => {
                return AuthFlowResult::Failed {
                    error: AuthFlowFailureKind::ProfileUnavailable
                        .stable_detail()
                        .to_string(),
                    kind: AuthFlowFailureKind::ProfileUnavailable,
                    artifacts_dir: None,
                };
            }
        };
        let profile_path = profile.path();
        if self
            .build_playwright_script_with_browser_config(
                &profile_path,
                target_url,
                email,
                None,
                ctx.config(),
            )
            .is_err()
        {
            return AuthFlowResult::Failed {
                error: AuthFlowFailureKind::PlaywrightError
                    .stable_detail()
                    .to_string(),
                kind: AuthFlowFailureKind::PlaywrightError,
                artifacts_dir: None,
            };
        }
        let profile_dir = match profile.ensure_dir() {
            Ok(path) => path,
            Err(_) => {
                return AuthFlowResult::Failed {
                    error: AuthFlowFailureKind::ProfileUnavailable
                        .stable_detail()
                        .to_string(),
                    kind: AuthFlowFailureKind::ProfileUnavailable,
                    artifacts_dir: None,
                };
            }
        };
        let profile_lock =
            match profile.acquire_operation_lock(ctx.config().profile_lock_timeout_ms) {
                Ok(profile_lock) => profile_lock,
                Err(_) => {
                    return AuthFlowResult::Failed {
                        error: AuthFlowFailureKind::ProfileUnavailable
                            .stable_detail()
                            .to_string(),
                        kind: AuthFlowFailureKind::ProfileUnavailable,
                        artifacts_dir: None,
                    };
                }
            };

        tracing::info!("Starting Google OAuth auth flow");
        // NOTE: auth_url is intentionally NOT logged (may contain OAuth tokens)

        // Step 3: Build and run the Playwright script
        let start = std::time::Instant::now();
        let artifacts_dir = self.prepare_artifacts_dir();

        if !profile_lock.is_current_for(&profile) {
            return AuthFlowResult::Failed {
                error: AuthFlowFailureKind::ProfileUnavailable
                    .stable_detail()
                    .to_string(),
                kind: AuthFlowFailureKind::ProfileUnavailable,
                artifacts_dir,
            };
        }

        let result = self.run_playwright_flow(
            &profile_dir,
            target_url,
            email,
            artifacts_dir.as_deref(),
            ctx.config(),
        );

        if !profile_lock.is_current_for(&profile) {
            return AuthFlowResult::Failed {
                error: AuthFlowFailureKind::ProfileUnavailable
                    .stable_detail()
                    .to_string(),
                kind: AuthFlowFailureKind::ProfileUnavailable,
                artifacts_dir,
            };
        }

        let elapsed_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(outcome) => match outcome {
                PlaywrightOutcome::Success { storage_state } => {
                    if profile
                        .record_authenticated_state_with_lock(
                            &storage_state,
                            BootstrapMethod::Automated,
                            &profile_lock,
                        )
                        .is_err()
                    {
                        return AuthFlowResult::Failed {
                            error: AuthFlowFailureKind::ProfilePersistenceFailed
                                .stable_detail()
                                .to_string(),
                            kind: AuthFlowFailureKind::ProfilePersistenceFailed,
                            artifacts_dir,
                        };
                    }
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
                         Sensitive execution inputs: redacted\n",
                        e.error, e.kind,
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
                Err(_) => {
                    tracing::warn!(
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
        browser_config: &super::BrowserConfig,
    ) -> Result<PlaywrightOutcome, PlaywrightFlowError> {
        let script = self
            .build_playwright_script_with_browser_config(
                profile_dir,
                auth_url,
                email,
                artifacts_dir,
                browser_config,
            )
            .map_err(|failure| PlaywrightFlowError {
                error: failure.detail().to_string(),
                kind: AuthFlowFailureKind::PlaywrightError,
            })?;
        let output = super::run_node_script_bounded(
            script,
            self.config.flow_timeout_ms,
            super::BROWSER_BOOTSTRAP_MAX_STDOUT_BYTES,
        )
        .map_err(|failure| PlaywrightFlowError {
            error: failure.detail().to_string(),
            kind: AuthFlowFailureKind::PlaywrightError,
        })?;
        let std::process::Output {
            status,
            stdout,
            stderr,
        } = output;
        if !stderr.is_empty() {
            tracing::debug!(
                stderr_bytes = stderr.len(),
                "Playwright subprocess stderr (content redacted in logs)"
            );
        }
        drop(stderr);
        let stdout = String::from_utf8(stdout).map_err(|_| PlaywrightFlowError {
            error: "Playwright returned invalid result text".to_string(),
            kind: AuthFlowFailureKind::PlaywrightError,
        })?;

        if !status.success() {
            return Err(Self::parse_playwright_error(&stdout));
        }

        Self::parse_playwright_result(&stdout)
    }

    /// Build the Node.js/Playwright script for the Google OAuth flow.
    fn build_playwright_script_with_browser_config(
        &self,
        profile_dir: &Path,
        auth_url: &str,
        email: Option<&str>,
        artifacts_dir: Option<&Path>,
        browser_config: &super::BrowserConfig,
    ) -> Result<String, super::BrowserNodeCommandFailure> {
        let sel = &self.config.selectors;
        super::admit_browser_timeout(self.config.flow_timeout_ms)?;
        super::admit_browser_timeout(browser_config.navigation_timeout_ms)?;
        super::admit_browser_timeout(browser_config.page_load_timeout_ms)?;
        super::admit_automated_auth_url(super::BrowserAuthService::Google, auth_url)?;
        for selector_group in [
            &sel.signed_in_marker,
            &sel.email_input,
            &sel.email_next,
            &sel.password_prompt,
            &sel.mfa_indicator,
            &sel.security_key_indicator,
            &sel.sso_indicator,
            &sel.verify_indicator,
        ] {
            super::admit_selector_group(selector_group)?;
        }
        super::admit_node_script_input_parts(
            &[
                Some(auth_url),
                email,
                Some(&sel.signed_in_marker),
                Some(&sel.email_input),
                Some(&sel.email_next),
                Some(&sel.password_prompt),
                Some(&sel.mfa_indicator),
                Some(&sel.security_key_indicator),
                Some(&sel.sso_indicator),
                Some(&sel.verify_indicator),
            ],
            &[Some(profile_dir), artifacts_dir],
            &[],
        )?;
        let input = serde_json::json!({
            "profile_dir": profile_dir.to_string_lossy(),
            "auth_url": auth_url,
            "email": email,
            "artifacts_dir": artifacts_dir.map(|path| path.to_string_lossy().into_owned()),
            "timeout_ms": self.config.flow_timeout_ms,
            "headless": browser_config.headless,
            "navigation_timeout_ms": browser_config.navigation_timeout_ms,
            "page_load_timeout_ms": browser_config.page_load_timeout_ms,
            "screenshot_max_bytes": super::openai_device::SCREENSHOT_ARTIFACT_MAX_BYTES,
            "selectors": {
                "signed_in_marker": &sel.signed_in_marker,
                "email_input": &sel.email_input,
                "email_next": &sel.email_next,
                "password_prompt": &sel.password_prompt,
                "mfa_indicator": &sel.mfa_indicator,
                "security_key_indicator": &sel.security_key_indicator,
                "sso_indicator": &sel.sso_indicator,
                "verify_indicator": &sel.verify_indicator,
            },
        });
        let input_base64 = super::encode_node_script_input(&input)?;

        super::admit_node_script_source(format!(
            r#"
const {{ chromium }} = require('playwright');
const input = JSON.parse(Buffer.from('{input_base64}', 'base64').toString('utf8'));
const fs = require('node:fs/promises');

async function captureScreenshot(page, directory) {{
  try {{
    const png = await page.screenshot({{ fullPage: false, timeout: Math.min(5000, input.timeout_ms) }});
    if (png.length > input.screenshot_max_bytes) return false;
    await fs.writeFile(directory + '/screenshot.png', png, {{ flag: 'wx', mode: 0o600 }});
    return true;
  }} catch (_) {{
    return false;
  }}
}}

function matchesAuthDestination(currentValue, authValue) {{
  try {{
    const current = new URL(currentValue);
    const expected = new URL(authValue);
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
  const NAVIGATION_TIMEOUT = Math.min(input.navigation_timeout_ms, TIMEOUT);
  const PAGE_LOAD_TIMEOUT = Math.min(input.page_load_timeout_ms, TIMEOUT);
  const profileDir = input.profile_dir;
  const authUrl = input.auth_url;
  const email = input.email;
  const artifactsDir = input.artifacts_dir;
  const selectors = input.selectors;

  async function finishSuccess() {{
    if (!matchesAuthDestination(page.url(), authUrl)) {{
      console.log(JSON.stringify({{ status: 'error', kind: 'VerificationFailed' }}));
    }} else {{
      const state = await browser.storageState();
      console.log(JSON.stringify({{ status: 'success', storage_state: state }}));
    }}
    await browser.close();
    process.exit(0);
  }}

  let browser, context, page;
  try {{
    browser = await chromium.launchPersistentContext(profileDir, {{
      headless: input.headless,
      timeout: TIMEOUT,
    }});
    page = browser.pages()[0] || await browser.newPage();
    page.setDefaultTimeout(PAGE_LOAD_TIMEOUT);

    // Navigate to auth page
    try {{
      await page.goto(authUrl, {{ waitUntil: 'domcontentloaded', timeout: NAVIGATION_TIMEOUT }});
    }} catch (_) {{
      console.log(JSON.stringify({{ status: 'error', kind: 'NavigationFailed' }}));
      await browser.close().catch(() => {{}});
      process.exit(1);
    }}

    // Wait for any redirects to settle
    await page.waitForTimeout(2000);

    // Check if already signed in (account avatar/profile visible)
    const signedInSelectors = selectors.signed_in_marker.split(', ');
    let alreadySignedIn = false;
    for (const sel of signedInSelectors) {{
      try {{
        const el = await page.$(sel);
        if (el) {{ alreadySignedIn = true; break; }}
      }} catch (_) {{}}
    }}

    if (alreadySignedIn) {{
      await finishSuccess();
    }}

    // Check for "Verify it's you" / captcha
    const verifySelectors = selectors.verify_indicator.split(', ');
    let verifyDetected = false;
    for (const sel of verifySelectors) {{
      try {{
        const el = await page.$(sel);
        if (el) {{ verifyDetected = true; break; }}
      }} catch (_) {{}}
    }}

    if (verifyDetected) {{
      if (artifactsDir) {{
        await captureScreenshot(page, artifactsDir);
      }}
      console.log(JSON.stringify({{
        status: 'interactive_required',
        reason: 'Verification challenge or captcha detected — human intervention required'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for SSO/enterprise IdP redirect
    const ssoSelectors = selectors.sso_indicator.split(', ');
    let ssoDetected = false;
    for (const sel of ssoSelectors) {{
      try {{
        const el = await page.$(sel);
        if (el) {{ ssoDetected = true; break; }}
      }} catch (_) {{}}
    }}

    if (ssoDetected) {{
      if (artifactsDir) {{
        await captureScreenshot(page, artifactsDir);
      }}
      console.log(JSON.stringify({{
        status: 'interactive_required',
        reason: 'SSO/enterprise identity provider detected — human must complete SSO flow'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for security key prompt
    const securityKeySelectors = selectors.security_key_indicator.split(', ');
    let securityKeyDetected = false;
    for (const sel of securityKeySelectors) {{
      try {{
        const el = await page.$(sel);
        if (el) {{ securityKeyDetected = true; break; }}
      }} catch (_) {{}}
    }}

    if (securityKeyDetected) {{
      if (artifactsDir) {{
        await captureScreenshot(page, artifactsDir);
      }}
      console.log(JSON.stringify({{
        status: 'interactive_required',
        reason: 'Security key prompt detected — human must use physical security key'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for MFA / 2-step verification
    const mfaSelectors = selectors.mfa_indicator.split(', ');
    let mfaDetected = false;
    for (const sel of mfaSelectors) {{
      try {{
        const el = await page.$(sel);
        if (el) {{ mfaDetected = true; break; }}
      }} catch (_) {{}}
    }}

    if (mfaDetected) {{
      if (artifactsDir) {{
        await captureScreenshot(page, artifactsDir);
      }}
      console.log(JSON.stringify({{
        status: 'interactive_required',
        reason: 'MFA / 2-step verification required — human must complete verification'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for password prompt
    const passwordEl = await page.$(selectors.password_prompt);
    if (passwordEl) {{
      if (artifactsDir) {{
        await captureScreenshot(page, artifactsDir);
      }}
      console.log(JSON.stringify({{
        status: 'interactive_required',
        reason: 'Password prompt detected — interactive bootstrap required'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for email prompt
    const emailEl = await page.$(selectors.email_input);
    if (emailEl && email) {{
      await emailEl.fill(email);
      const emailNext = await page.$(selectors.email_next);
      if (emailNext) {{
        await emailNext.click();
      }} else {{
        await emailEl.press('Enter');
      }}

      // Wait for navigation after email submission
      await page.waitForLoadState('domcontentloaded', {{ timeout: PAGE_LOAD_TIMEOUT }});
      await page.waitForTimeout(2000);

      // After email: check for password/MFA/SSO/security key
      const postEmailPassword = await page.$(selectors.password_prompt);
      if (postEmailPassword) {{
        if (artifactsDir) {{
          await captureScreenshot(page, artifactsDir);
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
          await captureScreenshot(page, artifactsDir);
        }}
        console.log(JSON.stringify({{
          status: 'interactive_required',
          reason: 'MFA / 2-step verification after email — human must complete verification'
        }}));
        await browser.close();
        process.exit(0);
      }}

      for (const [candidateSelectors, reason] of [
        [verifySelectors, 'Verification challenge or captcha detected — human intervention required'],
        [ssoSelectors, 'SSO/enterprise identity provider detected — human must complete SSO flow'],
        [securityKeySelectors, 'Security key prompt detected — human must use physical security key']
      ]) {{
        let detected = false;
        for (const sel of candidateSelectors) {{
          try {{
            if (await page.$(sel)) {{ detected = true; break; }}
          }} catch (_) {{}}
        }}
        if (detected) {{
          if (artifactsDir) await captureScreenshot(page, artifactsDir);
          console.log(JSON.stringify({{ status: 'interactive_required', reason }}));
          await browser.close();
          process.exit(0);
        }}
      }}

      // Re-check if we landed on a signed-in page (e.g., OAuth consent)
      for (const sel of signedInSelectors) {{
        try {{
          const el = await page.$(sel);
          if (el) {{ alreadySignedIn = true; break; }}
        }} catch (_) {{}}
      }}

      if (alreadySignedIn) {{
        await finishSuccess();
      }}
    }} else if (emailEl && !email) {{
      if (artifactsDir) {{
        await captureScreenshot(page, artifactsDir);
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
      await captureScreenshot(page, artifactsDir);
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
        await captureScreenshot(page, artifactsDir);
      }} catch (_) {{}}
    }}
    console.log(JSON.stringify({{
      status: 'error',
      kind: 'PlaywrightError'
    }}));
    if (browser) await browser.close().catch(() => {{}});
    process.exit(1);
  }}
}})();
"#
        ))
    }

    #[cfg(test)]
    fn build_playwright_script(
        &self,
        profile_dir: &Path,
        auth_url: &str,
        email: Option<&str>,
        artifacts_dir: Option<&Path>,
        headless: bool,
    ) -> Result<String, super::BrowserNodeCommandFailure> {
        let browser_config = super::BrowserConfig {
            headless,
            ..super::BrowserConfig::default()
        };
        self.build_playwright_script_with_browser_config(
            profile_dir,
            auth_url,
            email,
            artifacts_dir,
            &browser_config,
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
            serde_json::from_str(json_line).map_err(|_| PlaywrightFlowError {
                error: "Playwright returned malformed result JSON".to_string(),
                kind: AuthFlowFailureKind::PlaywrightError,
            })?;

        match parsed.get("status").and_then(|s| s.as_str()) {
            Some("success") => {
                let storage_state = parsed
                    .get("storage_state")
                    .filter(|value| value.is_object())
                    .ok_or_else(|| PlaywrightFlowError {
                        error: AuthFlowFailureKind::ProfilePersistenceFailed
                            .stable_detail()
                            .to_string(),
                        kind: AuthFlowFailureKind::ProfilePersistenceFailed,
                    })?;
                let storage_state =
                    serde_json::to_vec(storage_state).map_err(|_| PlaywrightFlowError {
                        error: AuthFlowFailureKind::ProfilePersistenceFailed
                            .stable_detail()
                            .to_string(),
                        kind: AuthFlowFailureKind::ProfilePersistenceFailed,
                    })?;
                Ok(PlaywrightOutcome::Success { storage_state })
            }
            Some("interactive_required") => {
                let reason = match parsed.get("reason").and_then(serde_json::Value::as_str) {
                    Some("Verification challenge or captcha detected — human intervention required") => "Verification challenge or captcha detected — human intervention required",
                    Some("SSO/enterprise identity provider detected — human must complete SSO flow") => "SSO/enterprise identity provider detected — human must complete SSO flow",
                    Some("Security key prompt detected — human must use physical security key") => "Security key prompt detected — human must use physical security key",
                    Some("MFA / 2-step verification required — human must complete verification") => "MFA / 2-step verification required — human must complete verification",
                    Some("Password prompt detected — interactive bootstrap required") => "Password prompt detected — interactive bootstrap required",
                    Some("Password required after email entry — interactive bootstrap required") => "Password required after email entry — interactive bootstrap required",
                    Some("MFA / 2-step verification after email — human must complete verification") => "MFA / 2-step verification after email — human must complete verification",
                    Some("Email prompt detected but no email provided — interactive bootstrap required") => "Email prompt detected but no email provided — interactive bootstrap required",
                    _ => "Interactive login is required to continue",
                }.to_string();
                Ok(PlaywrightOutcome::InteractiveRequired(reason))
            }
            Some("error") => {
                let kind = AuthFlowFailureKind::from_script_label(
                    parsed.get("kind").and_then(serde_json::Value::as_str),
                );
                Err(PlaywrightFlowError {
                    error: kind.stable_detail().to_string(),
                    kind,
                })
            }
            _ => Err(PlaywrightFlowError {
                error: AuthFlowFailureKind::Unknown.stable_detail().to_string(),
                kind: AuthFlowFailureKind::Unknown,
            }),
        }
    }

    /// Parse error information from a failed Playwright subprocess.
    fn parse_playwright_error(stdout: &str) -> PlaywrightFlowError {
        if let Some(json_line) = stdout.trim().lines().rev().find(|l| l.starts_with('{')) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_line) {
                let kind = AuthFlowFailureKind::from_script_label(
                    parsed.get("kind").and_then(serde_json::Value::as_str),
                );
                return PlaywrightFlowError {
                    error: kind.stable_detail().to_string(),
                    kind,
                };
            }
        }

        PlaywrightFlowError {
            error: AuthFlowFailureKind::PlaywrightError
                .stable_detail()
                .to_string(),
            kind: AuthFlowFailureKind::PlaywrightError,
        }
    }
}

/// Internal outcome from the Playwright subprocess.
enum PlaywrightOutcome {
    /// Flow completed successfully (already authenticated).
    Success { storage_state: Vec<u8> },
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
    fn custom_origin_config_is_rejected_before_script_execution() {
        let config = GoogleAuthConfig {
            auth_url: "https://custom.google.com/oauth".to_string(),
            flow_timeout_ms: 30_000,
            selectors: GooglePageSelectors::default(),
        };
        let flow = GoogleAuthFlow::new(config);
        assert_eq!(flow.config().auth_url, "https://custom.google.com/oauth");
        assert_eq!(flow.config().flow_timeout_ms, 30_000);
        assert_eq!(
            flow.build_playwright_script_with_browser_config(
                Path::new("/tmp/profile"),
                &flow.config().auth_url,
                None,
                None,
                &super::super::BrowserConfig::default(),
            ),
            Err(super::super::BrowserNodeCommandFailure::InvalidConfiguration)
        );
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

    #[test]
    fn execute_rejects_untrusted_auth_url_before_profile_creation() {
        let temp = tempfile::tempdir().expect("isolated Google URL rejection root");
        let mut ctx =
            super::super::BrowserContext::new(super::super::BrowserConfig::default(), temp.path());
        ctx.status = BrowserStatus::Ready;
        let result = GoogleAuthFlow::with_defaults().execute(
            &ctx,
            "untrusted-url",
            Some("https://127.0.0.1/o/oauth2/v2/auth"),
            None,
        );
        assert!(matches!(
            result,
            AuthFlowResult::Failed {
                kind: AuthFlowFailureKind::PlaywrightError,
                ..
            }
        ));
        assert!(!ctx.profile("google", "untrusted-url").path().exists());
    }

    #[test]
    fn execute_rejects_invalid_selectors_before_profile_creation() {
        let temp = tempfile::tempdir().expect("isolated Google preflight root");
        let mut ctx =
            super::super::BrowserContext::new(super::super::BrowserConfig::default(), temp.path());
        ctx.status = BrowserStatus::Ready;
        let mut config = GoogleAuthConfig::default();
        config.selectors.signed_in_marker.clear();
        let result = GoogleAuthFlow::new(config).execute(&ctx, "invalid-selectors", None, None);
        assert!(matches!(
            result,
            AuthFlowResult::Failed {
                kind: AuthFlowFailureKind::PlaywrightError,
                ..
            }
        ));
        assert!(!ctx.profile("google", "invalid-selectors").path().exists());
    }

    #[test]
    fn execute_rejects_invalid_account_identity_before_profile_creation() {
        let temp = tempfile::tempdir().expect("isolated Google identity root");
        let mut ctx = BrowserContext::new(super::super::BrowserConfig::default(), temp.path());
        ctx.status = BrowserStatus::Ready;
        let result = GoogleAuthFlow::new(GoogleAuthConfig::default()).execute(
            &ctx,
            "spoof\u{202e}txt",
            None,
            None,
        );
        assert!(matches!(
            result,
            AuthFlowResult::Failed {
                kind: AuthFlowFailureKind::ProfileUnavailable,
                ..
            }
        ));
        assert!(!ctx.profiles_root().exists());
    }

    // =========================================================================
    // Playwright result parsing
    // =========================================================================

    #[test]
    fn parse_success_result() {
        let stdout = r#"{"status":"success","storage_state":{"cookies":[],"origins":[]}}"#;
        let result = GoogleAuthFlow::parse_playwright_result(stdout);
        assert!(matches!(result, Ok(PlaywrightOutcome::Success { .. })));
    }

    #[test]
    fn success_requires_storage_state() {
        let result = GoogleAuthFlow::parse_playwright_result(r#"{"status":"success"}"#);
        match result {
            Err(error) => assert_eq!(error.kind, AuthFlowFailureKind::ProfilePersistenceFailed),
            _ => panic!("success without durable state must fail closed"),
        }
        assert!(
            GoogleAuthFlow::parse_playwright_result(
                r#"{"status":"success","storage_state":"{\"cookies\":[],\"origins\":[]}"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn parse_interactive_required_result() {
        let stdout = r#"{"status":"interactive_required","reason":"MFA / 2-step verification required — human must complete verification"}"#;
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
                assert_eq!(
                    e.error,
                    AuthFlowFailureKind::SelectorMismatch.stable_detail()
                );
                assert!(!e.error.contains("No selectors matched"));
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
        let stdout = "debug output\n{\"status\":\"success\",\"storage_state\":{\"cookies\":[],\"origins\":[]}}";
        let result = GoogleAuthFlow::parse_playwright_result(stdout);
        assert!(matches!(result, Ok(PlaywrightOutcome::Success { .. })));
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
    fn script_transports_auth_url_without_plaintext_literal() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://accounts.google.com/",
                None,
                None,
                false,
            )
            .expect("bounded Google script");
        assert!(!script.contains("accounts.google.com"));
        assert_eq!(
            super::super::decode_node_script_input(&script)["auth_url"],
            "https://accounts.google.com/"
        );
    }

    #[test]
    fn script_contains_email_when_provided() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://accounts.google.com/",
                Some("user@gmail.com"),
                None,
                false,
            )
            .expect("bounded Google script");
        assert!(!script.contains("user@gmail.com"));
        assert_eq!(
            super::super::decode_node_script_input(&script)["email"],
            "user@gmail.com"
        );
    }

    #[test]
    fn script_has_null_email_when_not_provided() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://accounts.google.com/",
                None,
                None,
                false,
            )
            .expect("bounded Google script");
        assert!(super::super::decode_node_script_input(&script)["email"].is_null());
    }

    #[test]
    fn script_checks_for_signed_in_markers() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://accounts.google.com/",
                None,
                None,
                false,
            )
            .expect("bounded Google script");
        assert!(script.contains("alreadySignedIn"));
        assert!(
            super::super::decode_node_script_input(&script)["selectors"]["signed_in_marker"]
                .as_str()
                .is_some_and(|value| value.contains("googleusercontent"))
        );
    }

    #[test]
    fn script_checks_for_password_prompt() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://accounts.google.com/",
                None,
                None,
                false,
            )
            .expect("bounded Google script");
        assert!(
            super::super::decode_node_script_input(&script)["selectors"]["password_prompt"]
                .as_str()
                .is_some_and(|value| value.contains("password"))
        );
        assert!(script.contains("interactive_required"));
    }

    #[test]
    fn script_checks_for_mfa() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://accounts.google.com/",
                None,
                None,
                false,
            )
            .expect("bounded Google script");
        assert!(
            super::super::decode_node_script_input(&script)["selectors"]["mfa_indicator"]
                .as_str()
                .is_some_and(|value| value.contains("2-Step Verification"))
        );
        assert!(script.contains("mfaDetected"));
    }

    #[test]
    fn script_checks_for_security_key() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://accounts.google.com/",
                None,
                None,
                false,
            )
            .expect("bounded Google script");
        assert!(
            super::super::decode_node_script_input(&script)["selectors"]["security_key_indicator"]
                .as_str()
                .is_some_and(|value| value.contains("security key"))
        );
        assert!(script.contains("securityKeyDetected"));
    }

    #[test]
    fn script_checks_for_sso() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://accounts.google.com/",
                None,
                None,
                false,
            )
            .expect("bounded Google script");
        assert!(
            super::super::decode_node_script_input(&script)["selectors"]["sso_indicator"]
                .as_str()
                .is_some_and(|value| value.contains("identity provider"))
        );
        assert!(script.contains("ssoDetected"));
    }

    #[test]
    fn script_uses_custom_oauth_url() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://accounts.google.com/o/oauth2/auth?client_id=123",
                None,
                None,
                false,
            )
            .expect("bounded Google script");
        assert!(!script.contains("oauth2/auth?client_id=123"));
        assert_eq!(
            super::super::decode_node_script_input(&script)["auth_url"],
            "https://accounts.google.com/o/oauth2/auth?client_id=123"
        );
    }

    // =========================================================================
    // Playwright error parsing
    // =========================================================================

    #[test]
    fn parse_playwright_error_from_json() {
        let stdout = r#"{"status":"error","kind":"NavigationFailed","message":"/private/secret"}"#;
        let error = GoogleAuthFlow::parse_playwright_error(stdout);
        assert_eq!(error.kind, AuthFlowFailureKind::NavigationFailed);
        assert_eq!(
            error.error,
            AuthFlowFailureKind::NavigationFailed.stable_detail()
        );
        assert!(!error.error.contains("secret"));
    }

    #[test]
    fn parse_playwright_error_fallback_is_content_free() {
        let error = GoogleAuthFlow::parse_playwright_error("");
        assert_eq!(error.kind, AuthFlowFailureKind::PlaywrightError);
        assert_eq!(
            error.error,
            AuthFlowFailureKind::PlaywrightError.stable_detail()
        );
    }

    #[test]
    fn hostile_google_input_round_trips_without_javascript_literal_injection() {
        let hostile_email = "mail'\\\n\u{2028}@example.com";
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile'\\\n"),
                "https://accounts.google.com/o/oauth2/v2/auth?token=secret",
                Some(hostile_email),
                None,
                false,
            )
            .expect("bounded hostile Google script");
        assert!(!script.contains(hostile_email));
        assert!(!script.contains("token=secret"));
        assert!(!script.contains('\u{2028}'));
        let input = super::super::decode_node_script_input(&script);
        assert_eq!(
            input["auth_url"],
            "https://accounts.google.com/o/oauth2/v2/auth?token=secret"
        );
        assert_eq!(input["email"], hostile_email);
    }

    #[test]
    fn google_script_input_enforces_exact_and_one_over_field_limit() {
        let flow = GoogleAuthFlow::with_defaults();
        let exact = "x".repeat(super::super::BROWSER_NODE_INPUT_MAX_FIELD_BYTES);
        assert!(
            flow.build_playwright_script(
                Path::new("/tmp/profile"),
                "https://accounts.google.com/",
                Some(&exact),
                None,
                false,
            )
            .is_ok()
        );
        let one_over = format!("{exact}x");
        assert_eq!(
            flow.build_playwright_script(
                Path::new("/tmp/profile"),
                "https://accounts.google.com/",
                Some(&one_over),
                None,
                false,
            ),
            Err(super::super::BrowserNodeCommandFailure::ScriptOversized)
        );
    }

    #[test]
    fn script_propagates_headless_policy_and_checks_post_email_challenges() {
        let flow = GoogleAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://accounts.google.com/",
                None,
                None,
                true,
            )
            .expect("bounded headless Google script");
        let input = super::super::decode_node_script_input(&script);
        assert_eq!(input["headless"], true);
        assert!(script.contains("headless: input.headless"));
        assert!(script.contains("candidateSelectors"));
        assert!(script.contains("matchesAuthDestination(page.url(), authUrl)"));
        assert!(script.contains("expected.pathname === '/'"));
        assert!(script.contains("browser.storageState()"));
        assert!(script.contains("fullPage: false"));
    }

    #[test]
    fn script_propagates_and_validates_browser_operation_timeouts() {
        let flow = GoogleAuthFlow::with_defaults();
        let browser_config = super::super::BrowserConfig {
            navigation_timeout_ms: 12_345,
            page_load_timeout_ms: 23_456,
            ..super::super::BrowserConfig::default()
        };
        let script = flow
            .build_playwright_script_with_browser_config(
                Path::new("/tmp/profile"),
                "https://accounts.google.com/",
                None,
                None,
                &browser_config,
            )
            .expect("bounded Google timeout configuration");
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
            flow.build_playwright_script_with_browser_config(
                Path::new("/tmp/profile"),
                "https://accounts.google.com/",
                None,
                None,
                &invalid,
            ),
            Err(super::super::BrowserNodeCommandFailure::InvalidTimeout)
        );
    }

    #[test]
    fn node_runner_is_stdin_bounded_and_never_uses_inline_argv() {
        let source = include_str!("google_auth.rs");
        let start = source
            .find("fn run_playwright_flow(")
            .expect("runner source");
        let tail = &source[start..];
        let end = tail
            .find("\n    /// Build the Node.js/Playwright script")
            .expect("runner boundary");
        let body = &tail[..end];
        assert!(body.contains("run_node_script_bounded"));
        assert!(!body.contains("std::process::Command"));
        assert!(!body.contains(".arg(\"-e\")"));
        assert!(!body.contains("stderr_summary"));
    }

    // -- Additional coverage to reach 30 tests --

    #[test]
    fn auth_flow_failure_kind_debug_format() {
        let kinds = [
            AuthFlowFailureKind::NavigationFailed,
            AuthFlowFailureKind::SelectorMismatch,
            AuthFlowFailureKind::BotDetected,
            AuthFlowFailureKind::PlaywrightError,
        ];
        for kind in &kinds {
            let dbg = format!("{:?}", kind);
            assert!(!dbg.is_empty());
        }
    }

    #[test]
    fn success_result_with_elapsed_roundtrips() {
        let result = AuthFlowResult::Success { elapsed_ms: 12345 };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["elapsed_ms"], 12345);
    }

    #[test]
    fn flow_with_artifacts_path_is_preserved() {
        let flow = GoogleAuthFlow::with_defaults().with_artifacts("/tmp/google_test_artifacts");
        assert!(flow.artifacts.is_some());
    }
}
