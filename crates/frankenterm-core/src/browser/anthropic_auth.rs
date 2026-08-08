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
//! - When explicitly configured, failure artifacts are private and bounded;
//!   screenshots can contain sensitive page content and are not redacted.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::openai_device::{ArtifactCapture, ArtifactKind, AuthFlowFailureKind, AuthFlowResult};
use super::{BootstrapMethod, BrowserContext, BrowserStatus};

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
            logged_in_marker: "text=Dashboard, text=API Keys, [data-testid='dashboard']"
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
                error: AuthFlowFailureKind::BrowserNotReady
                    .stable_detail()
                    .to_string(),
                kind: AuthFlowFailureKind::BrowserNotReady,
                artifacts_dir: None,
            };
        }

        let target_url = login_url.unwrap_or(&self.config.login_url);
        if super::admit_automated_auth_url(super::BrowserAuthService::Anthropic, target_url)
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

        // Step 2: Resolve the browser profile
        let profile = match ctx.try_profile("anthropic", account) {
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

        tracing::info!("Starting Anthropic auth flow");

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
            .and_then(|a| match a.ensure_invocation_dir("anthropic_auth") {
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
        login_url: &str,
        email: Option<&str>,
        artifacts_dir: Option<&Path>,
        browser_config: &super::BrowserConfig,
    ) -> Result<PlaywrightOutcome, PlaywrightFlowError> {
        let script = self
            .build_playwright_script_with_browser_config(
                profile_dir,
                login_url,
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

    /// Build the Node.js/Playwright script for the Anthropic auth flow.
    ///
    /// The script outputs a JSON result to stdout with one of:
    /// - `{"status":"success","storage_state":{...}}`
    /// - `{"status":"interactive_required","reason":"..."}`
    /// - `{"status":"error","kind":"...","message":"..."}`
    fn build_playwright_script_with_browser_config(
        &self,
        profile_dir: &Path,
        login_url: &str,
        email: Option<&str>,
        artifacts_dir: Option<&Path>,
        browser_config: &super::BrowserConfig,
    ) -> Result<String, super::BrowserNodeCommandFailure> {
        let sel = &self.config.selectors;
        let success_urls =
            super::bootstrap::BootstrapConfig::for_service(super::BrowserAuthService::Anthropic)
                .success_url_prefixes;
        super::admit_browser_timeout(self.config.flow_timeout_ms)?;
        super::admit_browser_timeout(browser_config.navigation_timeout_ms)?;
        super::admit_browser_timeout(browser_config.page_load_timeout_ms)?;
        super::admit_automated_auth_url(super::BrowserAuthService::Anthropic, login_url)?;
        for selector_group in [
            &sel.logged_in_marker,
            &sel.email_input,
            &sel.email_submit,
            &sel.password_prompt,
            &sel.sso_indicator,
            &sel.captcha_indicator,
        ] {
            super::admit_selector_group(selector_group)?;
        }
        for success_url in &success_urls {
            super::admit_bootstrap_success_url(super::BrowserAuthService::Anthropic, success_url)?;
        }
        super::admit_node_script_input_parts(
            &[
                Some(login_url),
                email,
                Some(&sel.logged_in_marker),
                Some(&sel.email_input),
                Some(&sel.email_submit),
                Some(&sel.password_prompt),
                Some(&sel.sso_indicator),
                Some(&sel.captcha_indicator),
            ],
            &[Some(profile_dir), artifacts_dir],
            &[success_urls.as_slice()],
        )?;
        let input = serde_json::json!({
            "profile_dir": profile_dir.to_string_lossy(),
            "login_url": login_url,
            "email": email,
            "artifacts_dir": artifacts_dir.map(|path| path.to_string_lossy().into_owned()),
            "timeout_ms": self.config.flow_timeout_ms,
            "headless": browser_config.headless,
            "navigation_timeout_ms": browser_config.navigation_timeout_ms,
            "page_load_timeout_ms": browser_config.page_load_timeout_ms,
            "success_url_prefixes": &success_urls,
            "screenshot_max_bytes": super::openai_device::SCREENSHOT_ARTIFACT_MAX_BYTES,
            "selectors": {
                "logged_in_marker": &sel.logged_in_marker,
                "email_input": &sel.email_input,
                "email_submit": &sel.email_submit,
                "password_prompt": &sel.password_prompt,
                "sso_indicator": &sel.sso_indicator,
                "captcha_indicator": &sel.captcha_indicator,
            },
        });
        let input_base64 = super::encode_node_script_input(&input)?;

        super::admit_node_script_source(format!(
            r"
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
  const NAVIGATION_TIMEOUT = Math.min(input.navigation_timeout_ms, TIMEOUT);
  const PAGE_LOAD_TIMEOUT = Math.min(input.page_load_timeout_ms, TIMEOUT);
  const profileDir = input.profile_dir;
  const loginUrl = input.login_url;
  const email = input.email;
  const artifactsDir = input.artifacts_dir;
  const selectors = input.selectors;
  const successUrls = input.success_url_prefixes;

  async function finishSuccess() {{
    if (!successUrls.some(prefix => matchesSuccessUrl(page.url(), prefix))) {{
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

    // Navigate to login page
    try {{
      await page.goto(loginUrl, {{ waitUntil: 'domcontentloaded', timeout: NAVIGATION_TIMEOUT }});
    }} catch (_) {{
      console.log(JSON.stringify({{ status: 'error', kind: 'NavigationFailed' }}));
      await browser.close().catch(() => {{}});
      process.exit(1);
    }}

    // Wait a moment for any redirects to settle
    await page.waitForTimeout(2000);

    // Check if already logged in (dashboard/console visible)
    const loggedInSelectors = selectors.logged_in_marker.split(', ');
    let alreadyLoggedIn = false;
    for (const sel of loggedInSelectors) {{
      try {{
        const el = await page.$(sel);
        if (el) {{ alreadyLoggedIn = true; break; }}
      }} catch (_) {{}}
    }}

    if (alreadyLoggedIn) {{
      await finishSuccess();
    }}

    // Check for captcha / bot challenge
    const captchaEl = await page.$(selectors.captcha_indicator);
    if (captchaEl) {{
      if (artifactsDir) {{
        await captureScreenshot(page, artifactsDir);
      }}
      console.log(JSON.stringify({{
        status: 'interactive_required',
        reason: 'Captcha or bot challenge detected — human intervention required'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for SSO redirect
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
        reason: 'SSO/enterprise login detected — human must complete SSO flow'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for password prompt (before email, in case of pre-filled email)
    const passwordEl = await page.$(selectors.password_prompt);
    if (passwordEl) {{
      if (artifactsDir) {{
        await captureScreenshot(page, artifactsDir);
      }}
      console.log(JSON.stringify({{
        status: 'interactive_required',
        reason: 'Password or MFA prompt detected — interactive bootstrap required'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Check for email prompt
    const emailEl = await page.$(selectors.email_input);
    if (emailEl && email) {{
      await emailEl.fill(email);
      const emailSubmit = await page.$(selectors.email_submit);
      if (emailSubmit) {{
        await emailSubmit.click();
      }} else {{
        await emailEl.press('Enter');
      }}

      // Wait for navigation after email submission
      await page.waitForLoadState('domcontentloaded', {{ timeout: PAGE_LOAD_TIMEOUT }});
      await page.waitForTimeout(2000);

      const postEmailCaptcha = await page.$(selectors.captcha_indicator);
      if (postEmailCaptcha) {{
        if (artifactsDir) await captureScreenshot(page, artifactsDir);
        console.log(JSON.stringify({{
          status: 'interactive_required',
          reason: 'Captcha or bot challenge detected — human intervention required'
        }}));
        await browser.close();
        process.exit(0);
      }}

      // After email: check for password/MFA/SSO
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
          await captureScreenshot(page, artifactsDir);
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

    // If we reach here, we're in an unrecognized page state
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
"
        ))
    }

    #[cfg(test)]
    fn build_playwright_script(
        &self,
        profile_dir: &Path,
        login_url: &str,
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
            login_url,
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
                    Some("Captcha or bot challenge detected — human intervention required") => "Captcha or bot challenge detected — human intervention required",
                    Some("SSO/enterprise login detected — human must complete SSO flow") => "SSO/enterprise login detected — human must complete SSO flow",
                    Some("Password or MFA prompt detected — interactive bootstrap required") => "Password or MFA prompt detected — interactive bootstrap required",
                    Some("Password required after email entry — interactive bootstrap required") => "Password required after email entry — interactive bootstrap required",
                    Some("SSO redirect after email entry — human must complete SSO flow") => "SSO redirect after email entry — human must complete SSO flow",
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
    fn custom_origin_config_is_rejected_before_script_execution() {
        let config = AnthropicAuthConfig {
            login_url: "https://custom.example.com/login".to_string(),
            flow_timeout_ms: 30_000,
            selectors: AnthropicPageSelectors::default(),
        };
        let flow = AnthropicAuthFlow::new(config);
        assert_eq!(flow.config().login_url, "https://custom.example.com/login");
        assert_eq!(flow.config().flow_timeout_ms, 30_000);
        assert_eq!(
            flow.build_playwright_script_with_browser_config(
                Path::new("/tmp/profile"),
                &flow.config().login_url,
                None,
                None,
                &super::super::BrowserConfig::default(),
            ),
            Err(super::super::BrowserNodeCommandFailure::InvalidConfiguration)
        );
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

    #[test]
    fn execute_rejects_untrusted_login_url_before_profile_creation() {
        let temp = tempfile::tempdir().expect("isolated Anthropic URL rejection root");
        let mut ctx =
            super::super::BrowserContext::new(super::super::BrowserConfig::default(), temp.path());
        ctx.status = BrowserStatus::Ready;
        let result = AnthropicAuthFlow::with_defaults().execute(
            &ctx,
            "untrusted-url",
            Some("https://127.0.0.1/login"),
            None,
        );
        assert!(matches!(
            result,
            AuthFlowResult::Failed {
                kind: AuthFlowFailureKind::PlaywrightError,
                ..
            }
        ));
        assert!(!ctx.profile("anthropic", "untrusted-url").path().exists());
    }

    #[test]
    fn execute_rejects_invalid_selectors_before_profile_creation() {
        let temp = tempfile::tempdir().expect("isolated Anthropic preflight root");
        let mut ctx =
            super::super::BrowserContext::new(super::super::BrowserConfig::default(), temp.path());
        ctx.status = BrowserStatus::Ready;
        let mut config = AnthropicAuthConfig::default();
        config.selectors.logged_in_marker.clear();
        let result = AnthropicAuthFlow::new(config).execute(&ctx, "invalid-selectors", None, None);
        assert!(matches!(
            result,
            AuthFlowResult::Failed {
                kind: AuthFlowFailureKind::PlaywrightError,
                ..
            }
        ));
        assert!(
            !ctx.profile("anthropic", "invalid-selectors")
                .path()
                .exists()
        );
    }

    #[test]
    fn execute_rejects_invalid_account_identity_before_profile_creation() {
        let temp = tempfile::tempdir().expect("isolated Anthropic identity root");
        let mut ctx = BrowserContext::new(super::super::BrowserConfig::default(), temp.path());
        ctx.status = BrowserStatus::Ready;
        let result = AnthropicAuthFlow::new(AnthropicAuthConfig::default()).execute(
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
        let result = AnthropicAuthFlow::parse_playwright_result(stdout);
        assert!(matches!(result, Ok(PlaywrightOutcome::Success { .. })));
    }

    #[test]
    fn success_requires_storage_state() {
        let result = AnthropicAuthFlow::parse_playwright_result(r#"{"status":"success"}"#);
        match result {
            Err(error) => assert_eq!(error.kind, AuthFlowFailureKind::ProfilePersistenceFailed),
            _ => panic!("success without durable state must fail closed"),
        }
        assert!(
            AnthropicAuthFlow::parse_playwright_result(
                r#"{"status":"success","storage_state":"{\"cookies\":[],\"origins\":[]}"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn parse_interactive_required_result() {
        let stdout = r#"{"status":"interactive_required","reason":"Password required after email entry — interactive bootstrap required"}"#;
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
        let result = AnthropicAuthFlow::parse_playwright_result("");
        assert!(result.is_err());
    }

    #[test]
    fn parse_result_finds_last_json_line() {
        let stdout = "some debug output\nmore output\n{\"status\":\"success\",\"storage_state\":{\"cookies\":[],\"origins\":[]}}";
        let result = AnthropicAuthFlow::parse_playwright_result(stdout);
        assert!(matches!(result, Ok(PlaywrightOutcome::Success { .. })));
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
    fn script_transports_login_url_without_plaintext_literal() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://console.anthropic.com/login",
                None,
                None,
                false,
            )
            .expect("bounded Anthropic script");
        assert!(!script.contains("console.anthropic.com/login"));
        assert_eq!(
            super::super::decode_node_script_input(&script)["login_url"],
            "https://console.anthropic.com/login"
        );
    }

    #[test]
    fn script_contains_email_when_provided() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://console.anthropic.com/login",
                Some("user@example.com"),
                None,
                false,
            )
            .expect("bounded Anthropic script");
        assert!(!script.contains("user@example.com"));
        assert_eq!(
            super::super::decode_node_script_input(&script)["email"],
            "user@example.com"
        );
    }

    #[test]
    fn script_has_null_email_when_not_provided() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://console.anthropic.com/login",
                None,
                None,
                false,
            )
            .expect("bounded Anthropic script");
        assert!(super::super::decode_node_script_input(&script)["email"].is_null());
    }

    #[test]
    fn script_accepts_trusted_oauth_path_with_query() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://console.anthropic.com/oauth/authorize?state=opaque",
                None,
                None,
                false,
            )
            .expect("bounded Anthropic script");
        assert!(!script.contains("state=opaque"));
        assert_eq!(
            super::super::decode_node_script_input(&script)["login_url"],
            "https://console.anthropic.com/oauth/authorize?state=opaque"
        );
    }

    #[test]
    fn script_round_trips_quotes_newlines_backslashes_and_unicode_separators() {
        let flow = AnthropicAuthFlow::with_defaults();
        let hostile_email = "mail'\\\n\u{2028}@example.com";
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile'\\\n"),
                "https://console.anthropic.com/login?token=secret",
                Some(hostile_email),
                None,
                false,
            )
            .expect("bounded hostile Anthropic script");
        assert!(!script.contains(hostile_email));
        assert!(!script.contains("token=secret"));
        assert!(!script.contains('\u{2028}'));
        let input = super::super::decode_node_script_input(&script);
        assert_eq!(
            input["login_url"],
            "https://console.anthropic.com/login?token=secret"
        );
        assert_eq!(input["email"], hostile_email);
    }

    #[test]
    fn script_checks_for_logged_in_markers() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://console.anthropic.com/login",
                None,
                None,
                false,
            )
            .expect("bounded Anthropic script");
        assert!(
            super::super::decode_node_script_input(&script)["selectors"]["logged_in_marker"]
                .as_str()
                .is_some_and(|value| value.contains("Dashboard"))
        );
        assert!(script.contains("alreadyLoggedIn"));
    }

    #[test]
    fn script_checks_for_password_prompt() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://console.anthropic.com/login",
                None,
                None,
                false,
            )
            .expect("bounded Anthropic script");
        assert!(
            super::super::decode_node_script_input(&script)["selectors"]["password_prompt"]
                .as_str()
                .is_some_and(|value| value.contains("password"))
        );
        assert!(script.contains("interactive_required"));
    }

    #[test]
    fn script_checks_for_sso() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://console.anthropic.com/login",
                None,
                None,
                false,
            )
            .expect("bounded Anthropic script");
        assert!(
            super::super::decode_node_script_input(&script)["selectors"]["sso_indicator"]
                .as_str()
                .is_some_and(|value| value.contains("SSO"))
        );
        assert!(script.contains("ssoDetected"));
    }

    #[test]
    fn script_checks_for_captcha() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://console.anthropic.com/login",
                None,
                None,
                false,
            )
            .expect("bounded Anthropic script");
        assert!(
            super::super::decode_node_script_input(&script)["selectors"]["captcha_indicator"]
                .as_str()
                .is_some_and(|value| value.contains("captcha"))
        );
    }

    #[test]
    fn anthropic_script_input_enforces_exact_and_one_over_field_limit() {
        let flow = AnthropicAuthFlow::with_defaults();
        let exact = "x".repeat(super::super::BROWSER_NODE_INPUT_MAX_FIELD_BYTES);
        assert!(
            flow.build_playwright_script(
                Path::new("/tmp/profile"),
                "https://console.anthropic.com/login",
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
                "https://console.anthropic.com/login",
                Some(&one_over),
                None,
                false,
            ),
            Err(super::super::BrowserNodeCommandFailure::ScriptOversized)
        );
    }

    #[test]
    fn script_propagates_headless_policy_and_uses_origin_bound_success() {
        let flow = AnthropicAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(
                Path::new("/tmp/profile"),
                "https://console.anthropic.com/login",
                None,
                None,
                true,
            )
            .expect("bounded headless Anthropic script");
        let input = super::super::decode_node_script_input(&script);
        assert_eq!(input["headless"], true);
        assert!(script.contains("headless: input.headless"));
        assert!(script.contains("matchesSuccessUrl(page.url(), prefix)"));
        assert!(
            input["success_url_prefixes"]
                .as_array()
                .is_some_and(|values| values.iter().any(|value| {
                    value.as_str() == Some("https://console.anthropic.com/dashboard")
                }))
        );
        assert!(
            !input["selectors"]["logged_in_marker"]
                .as_str()
                .is_some_and(|value| value.contains("Welcome back"))
        );
        assert!(script.contains("browser.storageState()"));
        assert!(script.contains("fullPage: false"));
    }

    #[test]
    fn script_propagates_and_validates_browser_operation_timeouts() {
        let flow = AnthropicAuthFlow::with_defaults();
        let browser_config = super::super::BrowserConfig {
            navigation_timeout_ms: 12_345,
            page_load_timeout_ms: 23_456,
            ..super::super::BrowserConfig::default()
        };
        let script = flow
            .build_playwright_script_with_browser_config(
                Path::new("/tmp/profile"),
                "https://console.anthropic.com/login",
                None,
                None,
                &browser_config,
            )
            .expect("bounded Anthropic timeout configuration");
        let input = super::super::decode_node_script_input(&script);
        assert_eq!(input["navigation_timeout_ms"], 12_345);
        assert_eq!(input["page_load_timeout_ms"], 23_456);
        assert!(script.contains("timeout: NAVIGATION_TIMEOUT"));
        assert!(script.contains("page.setDefaultTimeout(PAGE_LOAD_TIMEOUT)"));

        let invalid = super::super::BrowserConfig {
            navigation_timeout_ms: 0,
            ..super::super::BrowserConfig::default()
        };
        assert_eq!(
            flow.build_playwright_script_with_browser_config(
                Path::new("/tmp/profile"),
                "https://console.anthropic.com/login",
                None,
                None,
                &invalid,
            ),
            Err(super::super::BrowserNodeCommandFailure::InvalidTimeout)
        );
    }

    // =========================================================================
    // Playwright error parsing
    // =========================================================================

    #[test]
    fn parse_playwright_error_from_json() {
        let stdout = r#"{"status":"error","kind":"NavigationFailed","message":"/private/secret"}"#;
        let error = AnthropicAuthFlow::parse_playwright_error(stdout);
        assert_eq!(error.kind, AuthFlowFailureKind::NavigationFailed);
        assert_eq!(
            error.error,
            AuthFlowFailureKind::NavigationFailed.stable_detail()
        );
        assert!(!error.error.contains("secret"));
    }

    #[test]
    fn parse_playwright_error_fallback_is_content_free() {
        let error = AnthropicAuthFlow::parse_playwright_error("");
        assert_eq!(error.kind, AuthFlowFailureKind::PlaywrightError);
        assert_eq!(
            error.error,
            AuthFlowFailureKind::PlaywrightError.stable_detail()
        );
    }

    #[test]
    fn node_runner_is_stdin_bounded_and_never_uses_inline_argv() {
        let source = include_str!("anthropic_auth.rs");
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
    fn auth_flow_result_success_serializes() {
        let result = AuthFlowResult::Success { elapsed_ms: 500 };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("success"));
        assert!(json.contains("500"));
    }

    #[test]
    fn default_selectors_have_non_empty_fields() {
        let sel = AnthropicPageSelectors::default();
        assert!(!sel.email_input.is_empty());
        assert!(!sel.email_submit.is_empty());
    }
}
