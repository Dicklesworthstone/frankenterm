//! OpenAI/Codex device auth flow via Playwright.
//!
//! Automates the device-code authorization flow at `auth.openai.com/codex/device`.
//! The flow assumes a persistent Playwright browser profile already exists for the
//! target account (created via [`super::BrowserProfile`]).
//!
//! # Flow
//!
//! ```text
//! validate_user_code(code)
//!        │
//!        ▼
//! navigate → auth.openai.com/codex/device
//!        │
//!        ├─ already logged in → fill code → submit → verify success
//!        │
//!        ├─ email prompt → fill email → continue → fill code → submit → verify
//!        │
//!        └─ password/MFA prompt → exit with InteractiveBootstrapRequired
//! ```
//!
//! # Safety
//!
//! - Passwords, tokens, cookies, and session data are **never** logged.
//! - On failure, artifacts (screenshot, redacted DOM snippet) are saved to the
//!   workspace artifacts directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{BrowserContext, BrowserStatus};

// =============================================================================
// User code validation
// =============================================================================

/// Validate and normalize an OpenAI device user code.
///
/// Returns the uppercase-normalized code on success.
///
/// # Errors
///
/// Returns `UserCodeError` if the code is empty, has an invalid format, or
/// contains non-ASCII characters.
pub fn validate_user_code(code: &str) -> Result<String, UserCodeError> {
    let trimmed = code.trim();

    if trimmed.is_empty() {
        return Err(UserCodeError::Empty);
    }

    // Normalize to uppercase
    let normalized = trimmed.to_ascii_uppercase();

    // Check format: XXXX-XXXX where X is an ASCII letter
    let parts: Vec<&str> = normalized.split('-').collect();
    if parts.len() != 2 {
        return Err(UserCodeError::InvalidFormat {
            code: trimmed.to_string(),
            expected: "XXXX-XXXX (4 letters, hyphen, 4 letters)".to_string(),
        });
    }

    for part in &parts {
        if part.len() != 4 {
            return Err(UserCodeError::InvalidFormat {
                code: trimmed.to_string(),
                expected: "XXXX-XXXX (4 letters, hyphen, 4 letters)".to_string(),
            });
        }
        if !part.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(UserCodeError::InvalidCharacters {
                code: trimmed.to_string(),
            });
        }
    }

    Ok(normalized)
}

/// Errors from user code validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserCodeError {
    /// Code string was empty or whitespace-only.
    Empty,
    /// Code did not match the expected XXXX-XXXX format.
    InvalidFormat { code: String, expected: String },
    /// Code contained non-ASCII-alphabetic characters in letter positions.
    InvalidCharacters { code: String },
}

impl std::fmt::Display for UserCodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "user code is empty"),
            Self::InvalidFormat { code, expected } => {
                write!(f, "invalid user code format '{code}': expected {expected}")
            }
            Self::InvalidCharacters { code } => {
                write!(
                    f,
                    "user code '{code}' contains invalid characters (expected ASCII letters only)"
                )
            }
        }
    }
}

impl std::error::Error for UserCodeError {}

// =============================================================================
// Auth flow types
// =============================================================================

/// Result of executing the device auth flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum AuthFlowResult {
    /// Device code was successfully submitted and verified.
    #[serde(rename = "success")]
    Success {
        /// Wall-clock time the flow took in milliseconds.
        elapsed_ms: u64,
    },

    /// The browser session requires interactive login (password/MFA).
    ///
    /// The caller should direct the user to the fallback flow
    /// (wa-nu4.1.4.3: interactive bootstrap).
    #[serde(rename = "interactive_required")]
    InteractiveBootstrapRequired {
        /// Why interactive login is needed.
        reason: String,
        /// Path to failure artifacts directory, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        artifacts_dir: Option<PathBuf>,
    },

    /// The flow failed due to an unexpected condition.
    #[serde(rename = "failed")]
    Failed {
        /// Human-readable error description.
        error: String,
        /// Failure classification for programmatic handling.
        kind: AuthFlowFailureKind,
        /// Path to failure artifacts directory, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        artifacts_dir: Option<PathBuf>,
    },
}

/// Classification of auth flow failures for programmatic handling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthFlowFailureKind {
    /// User code validation failed before browser automation started.
    InvalidUserCode,
    /// Browser context was not ready (Playwright not available, etc.).
    BrowserNotReady,
    /// Navigation to the auth page failed or timed out.
    NavigationFailed,
    /// Could not find expected page elements (selectors changed).
    SelectorMismatch,
    /// Bot detection or rate limiting by OpenAI.
    BotDetected,
    /// The success marker was not found after submission.
    VerificationFailed,
    /// Playwright subprocess exited with an error.
    PlaywrightError,
    /// An unexpected/unclassified error occurred.
    Unknown,
}

// =============================================================================
// Auth flow configuration
// =============================================================================

/// Configuration for the OpenAI device auth flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiDeviceAuthConfig {
    /// Target URL for the device code page.
    pub device_url: String,

    /// Timeout for the entire flow in milliseconds (default: 60s).
    pub flow_timeout_ms: u64,

    /// CSS selectors for page elements.
    pub selectors: DevicePageSelectors,
}

impl Default for OpenAiDeviceAuthConfig {
    fn default() -> Self {
        Self {
            device_url: "https://auth.openai.com/codex/device".to_string(),
            flow_timeout_ms: 60_000,
            selectors: DevicePageSelectors::default(),
        }
    }
}

/// CSS selectors used to identify page elements during the device auth flow.
///
/// These are separated into a struct so they can be updated when OpenAI
/// changes their UI without modifying flow logic.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DevicePageSelectors {
    /// Input field for the device user code.
    pub code_input: String,
    /// Submit button for the code form.
    pub submit_button: String,
    /// Element indicating the user needs to enter an email.
    pub email_prompt: String,
    /// Input field for email address.
    pub email_input: String,
    /// Continue/submit button on the email form.
    pub email_submit: String,
    /// Element indicating password or MFA is required.
    pub password_prompt: String,
    /// Marker text or selector indicating successful authorization.
    pub success_marker: String,
}

impl Default for DevicePageSelectors {
    fn default() -> Self {
        Self {
            code_input: "input[name='user_code'], input[type='text'][autocomplete='off']"
                .to_string(),
            submit_button: "button[type='submit']".to_string(),
            email_prompt: "input[name='email'], input[type='email']".to_string(),
            email_input: "input[name='email'], input[type='email']".to_string(),
            email_submit: "button[type='submit']".to_string(),
            password_prompt: "input[type='password']".to_string(),
            success_marker:
                "text=Successfully logged in, text=Device connected, text=You're all set"
                    .to_string(),
        }
    }
}

// =============================================================================
// Failure artifacts
// =============================================================================

/// Captures failure artifacts for debugging without leaking secrets.
#[derive(Debug, Clone)]
pub struct ArtifactCapture {
    /// Root directory for artifacts (e.g., `<workspace>/.wa/artifacts/`).
    artifacts_root: PathBuf,
}

/// Kind of artifact captured on failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactKind {
    /// Browser screenshot (PNG).
    Screenshot,
    /// Redacted DOM snippet (HTML with secrets stripped).
    RedactedDom,
    /// Short human-readable failure report (text).
    FailureReport,
}

impl ArtifactCapture {
    /// Create a new artifact capture rooted at the given directory.
    #[must_use]
    pub fn new(artifacts_root: impl Into<PathBuf>) -> Self {
        Self {
            artifacts_root: artifacts_root.into(),
        }
    }

    /// Create the artifacts directory for a specific flow invocation.
    ///
    /// Returns the path to the per-invocation directory.
    pub fn ensure_invocation_dir(&self, flow_name: &str) -> Result<PathBuf, std::io::Error> {
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let pid = std::process::id();
        let dir = self
            .artifacts_root
            .join(flow_name)
            .join(format!("{timestamp}_{pid}"));
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Write a text artifact to the invocation directory.
    pub fn write_artifact(
        dir: &Path,
        kind: ArtifactKind,
        content: &[u8],
    ) -> Result<PathBuf, std::io::Error> {
        let filename = match kind {
            ArtifactKind::Screenshot => "screenshot.png",
            ArtifactKind::RedactedDom => "redacted_dom.html",
            ArtifactKind::FailureReport => "failure_report.txt",
        };
        let path = dir.join(filename);
        std::fs::write(&path, content)?;
        tracing::debug!(
            artifact_kind = ?kind,
            path = %path.display(),
            bytes = content.len(),
            "Wrote failure artifact"
        );
        Ok(path)
    }
}

// =============================================================================
// Auth flow execution
// =============================================================================

/// Orchestrates the OpenAI device code authorization flow.
///
/// This struct holds the configuration and provides the `execute()` method
/// that drives the browser automation via a Playwright subprocess.
pub struct OpenAiDeviceAuthFlow {
    config: OpenAiDeviceAuthConfig,
    artifacts: Option<ArtifactCapture>,
}

impl OpenAiDeviceAuthFlow {
    /// Create a new flow with the given configuration.
    #[must_use]
    pub fn new(config: OpenAiDeviceAuthConfig) -> Self {
        Self {
            config,
            artifacts: None,
        }
    }

    /// Create a new flow with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(OpenAiDeviceAuthConfig::default())
    }

    /// Set the artifacts directory for failure debugging.
    #[must_use]
    pub fn with_artifacts(mut self, artifacts_root: impl Into<PathBuf>) -> Self {
        self.artifacts = Some(ArtifactCapture::new(artifacts_root));
        self
    }

    /// Current configuration.
    #[must_use]
    pub fn config(&self) -> &OpenAiDeviceAuthConfig {
        &self.config
    }

    /// Execute the device auth flow.
    ///
    /// # Arguments
    ///
    /// * `ctx` - Browser context (must be in `Ready` state).
    /// * `user_code` - The device code obtained from the Codex pane.
    /// * `account` - Account identifier for profile selection.
    /// * `email` - Optional email for auto-fill if an email prompt appears.
    ///
    /// # Returns
    ///
    /// An [`AuthFlowResult`] indicating success, interactive-bootstrap-required,
    /// or failure with details.
    pub fn execute(
        &self,
        ctx: &BrowserContext,
        user_code: &str,
        account: &str,
        email: Option<&str>,
    ) -> AuthFlowResult {
        // Step 1: Validate user code before touching the browser
        let normalized_code = match validate_user_code(user_code) {
            Ok(code) => code,
            Err(e) => {
                return AuthFlowResult::Failed {
                    error: format!("User code validation failed: {e}"),
                    kind: AuthFlowFailureKind::InvalidUserCode,
                    artifacts_dir: None,
                };
            }
        };

        // Step 2: Verify browser context is ready
        if *ctx.status() != BrowserStatus::Ready {
            return AuthFlowResult::Failed {
                error: format!("Browser context not ready: {:?}", ctx.status()),
                kind: AuthFlowFailureKind::BrowserNotReady,
                artifacts_dir: None,
            };
        }

        // Step 3: Resolve the browser profile
        let profile = ctx.profile("openai", account);
        let profile_dir = profile.path();

        tracing::info!(
            profile_dir = %profile_dir.display(),
            account = %account,
            device_url = %self.config.device_url,
            "Starting OpenAI device auth flow"
        );
        // NOTE: user_code is intentionally NOT logged (secret material)

        // Step 4: Build and run the Playwright script
        let start = std::time::Instant::now();
        let artifacts_dir = self.prepare_artifacts_dir();

        let result = self.run_playwright_flow(
            &profile_dir,
            &normalized_code,
            email,
            artifacts_dir.as_deref(),
        );

        let elapsed_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(outcome) => match outcome {
                PlaywrightOutcome::Success => {
                    tracing::info!(elapsed_ms, "Device auth flow completed successfully");
                    AuthFlowResult::Success { elapsed_ms }
                }
                PlaywrightOutcome::InteractiveRequired(reason) => {
                    tracing::warn!(
                        elapsed_ms,
                        reason = %reason,
                        "Device auth flow requires interactive login"
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
                    "Device auth flow failed"
                );
                // Write failure report artifact if we have an artifacts dir
                if let Some(ref dir) = artifacts_dir {
                    let report = format!(
                        "OpenAI Device Auth Flow Failure Report\n\
                         =======================================\n\
                         Error: {}\n\
                         Kind: {:?}\n\
                         Elapsed: {elapsed_ms}ms\n\
                         Device URL: {}\n\
                         Profile dir: {}\n\
                         Account: {account}\n\
                         Note: user_code redacted for security\n",
                        e.error,
                        e.kind,
                        self.config.device_url,
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
            .and_then(|a| match a.ensure_invocation_dir("openai_device") {
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
    ///
    /// This generates an inline Node.js script and executes it via `node -e`.
    /// The script:
    /// 1. Launches a browser with the given profile directory
    /// 2. Navigates to the device auth URL
    /// 3. Detects the page state (logged in, email prompt, or password/MFA)
    /// 4. Fills and submits the user code form
    /// 5. Verifies success
    fn run_playwright_flow(
        &self,
        profile_dir: &Path,
        user_code: &str,
        email: Option<&str>,
        artifacts_dir: Option<&Path>,
    ) -> Result<PlaywrightOutcome, PlaywrightFlowError> {
        let script = self.build_playwright_script(profile_dir, user_code, email, artifacts_dir);

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
            // Parse structured error from stdout if possible
            return Err(Self::parse_playwright_error(
                &stdout,
                &stderr,
                output.status,
            ));
        }

        // Parse the JSON result from stdout
        Self::parse_playwright_result(&stdout)
    }

    /// Build the Node.js/Playwright script for the device auth flow.
    ///
    /// The script outputs a JSON result to stdout with one of:
    /// - `{"status":"success"}`
    /// - `{"status":"interactive_required","reason":"..."}`
    /// - `{"status":"error","kind":"...","message":"..."}`
    fn build_playwright_script(
        &self,
        profile_dir: &Path,
        user_code: &str,
        email: Option<&str>,
        artifacts_dir: Option<&Path>,
    ) -> String {
        let profile_dir_str = profile_dir.display();
        let device_url = &self.config.device_url;
        let timeout = self.config.flow_timeout_ms;
        let headless = serde_json::to_string(&false).unwrap(); // always visible for device auth

        let sel = &self.config.selectors;
        let code_input_sel = &sel.code_input;
        let submit_sel = &sel.submit_button;
        let email_prompt_sel = &sel.email_prompt;
        let email_submit_sel = &sel.email_submit;
        let password_sel = &sel.password_prompt;
        let success_sel = &sel.success_marker;

        let email_js = email
            .map(|e| format!("'{}'", e.replace('\'', "\\'")))
            .unwrap_or_else(|| "null".to_string());

        let artifacts_js = artifacts_dir
            .map(|d| format!("'{}'", d.display()))
            .unwrap_or_else(|| "null".to_string());

        // Note: user_code is passed as a variable, not interpolated into selectors,
        // to prevent injection. The script handles it as a value-only parameter.
        let user_code_escaped = user_code.replace('\\', "\\\\").replace('\'', "\\'");

        format!(
            r#"
const {{ chromium }} = require('playwright');

(async () => {{
  const TIMEOUT = {timeout};
  const profileDir = '{profile_dir_str}';
  const deviceUrl = '{device_url}';
  const userCode = '{user_code_escaped}';
  const email = {email_js};
  const artifactsDir = {artifacts_js};

  let browser, context, page;
  try {{
    browser = await chromium.launchPersistentContext(profileDir, {{
      headless: {headless},
      timeout: TIMEOUT,
    }});
    page = browser.pages()[0] || await browser.newPage();
    page.setDefaultTimeout(TIMEOUT);

    // Navigate to device auth page
    await page.goto(deviceUrl, {{ waitUntil: 'domcontentloaded', timeout: TIMEOUT }});

    // Detect page state
    const passwordEl = await page.$("{password_sel}");
    if (passwordEl) {{
      // Password/MFA required — cannot automate
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
    const emailEl = await page.$("{email_prompt_sel}");
    if (emailEl && email) {{
      await emailEl.fill(email);
      const emailSubmit = await page.$("{email_submit_sel}");
      if (emailSubmit) await emailSubmit.click();
      // Wait for navigation after email submission
      await page.waitForLoadState('domcontentloaded', {{ timeout: TIMEOUT }});
    }} else if (emailEl && !email) {{
      if (artifactsDir) {{
        await page.screenshot({{ path: artifactsDir + '/screenshot.png', fullPage: true }});
      }}
      console.log(JSON.stringify({{
        status: 'interactive_required',
        reason: 'Email prompt detected but no email provided'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Fill the user code
    const codeInput = await page.waitForSelector("{code_input_sel}", {{ timeout: TIMEOUT }});
    await codeInput.fill(userCode);

    // Submit
    const submitBtn = await page.$("{submit_sel}");
    if (submitBtn) {{
      await submitBtn.click();
    }} else {{
      // Try pressing Enter as fallback
      await codeInput.press('Enter');
    }}

    // Verify success
    const successSelectors = "{success_sel}".split(', ');
    let found = false;
    for (const sel of successSelectors) {{
      try {{
        await page.waitForSelector(sel, {{ timeout: 10000 }});
        found = true;
        break;
      }} catch (_) {{}}
    }}

    if (!found) {{
      // Try checking page text content
      const bodyText = await page.textContent('body');
      const markers = ['Successfully', 'Device connected', "You're all set", 'authorized'];
      found = markers.some(m => bodyText && bodyText.includes(m));
    }}

    if (found) {{
      console.log(JSON.stringify({{ status: 'success' }}));
    }} else {{
      if (artifactsDir) {{
        await page.screenshot({{ path: artifactsDir + '/screenshot.png', fullPage: true }});
      }}
      console.log(JSON.stringify({{
        status: 'error',
        kind: 'VerificationFailed',
        message: 'Success marker not found after form submission'
      }}));
    }}

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

        // Find the last JSON line (script may produce other output before)
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
        // Try to get structured error from stdout first
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

        // Fallback: use stderr
        let stderr_summary = stderr.lines().take(5).collect::<Vec<_>>().join("; ");

        PlaywrightFlowError {
            error: format!("Playwright process exited with {status}: {stderr_summary}"),
            kind: AuthFlowFailureKind::PlaywrightError,
        }
    }
}

/// Internal outcome from the Playwright subprocess.
enum PlaywrightOutcome {
    /// Flow completed successfully.
    Success,
    /// Interactive login is required (password/MFA or missing email).
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
    // User code validation tests
    // =========================================================================

    #[test]
    fn validate_valid_code_uppercase() {
        let result = validate_user_code("ABCD-EFGH");
        assert_eq!(result.unwrap(), "ABCD-EFGH");
    }

    #[test]
    fn validate_valid_code_lowercase() {
        let result = validate_user_code("abcd-efgh");
        assert_eq!(result.unwrap(), "ABCD-EFGH");
    }

    #[test]
    fn validate_valid_code_mixed_case() {
        let result = validate_user_code("AbCd-EfGh");
        assert_eq!(result.unwrap(), "ABCD-EFGH");
    }

    #[test]
    fn validate_code_with_whitespace() {
        let result = validate_user_code("  ABCD-EFGH  ");
        assert_eq!(result.unwrap(), "ABCD-EFGH");
    }

    #[test]
    fn validate_empty_code() {
        let result = validate_user_code("");
        assert_eq!(result.unwrap_err(), UserCodeError::Empty);
    }

    #[test]
    fn validate_whitespace_only() {
        let result = validate_user_code("   ");
        assert_eq!(result.unwrap_err(), UserCodeError::Empty);
    }

    #[test]
    fn validate_no_hyphen() {
        let result = validate_user_code("ABCDEFGH");
        assert!(matches!(result, Err(UserCodeError::InvalidFormat { .. })));
    }

    #[test]
    fn validate_too_short_parts() {
        let result = validate_user_code("ABC-EFGH");
        assert!(matches!(result, Err(UserCodeError::InvalidFormat { .. })));
    }

    #[test]
    fn validate_too_long_parts() {
        let result = validate_user_code("ABCDE-FGHIJ");
        assert!(matches!(result, Err(UserCodeError::InvalidFormat { .. })));
    }

    #[test]
    fn validate_digits_in_code() {
        let result = validate_user_code("AB12-CD34");
        assert!(matches!(
            result,
            Err(UserCodeError::InvalidCharacters { .. })
        ));
    }

    #[test]
    fn validate_special_chars() {
        let result = validate_user_code("AB@D-EF!H");
        assert!(matches!(
            result,
            Err(UserCodeError::InvalidCharacters { .. })
        ));
    }

    #[test]
    fn validate_multiple_hyphens() {
        let result = validate_user_code("AB-CD-EF");
        assert!(matches!(result, Err(UserCodeError::InvalidFormat { .. })));
    }

    #[test]
    fn validate_unicode_letters() {
        // Unicode letters should fail (only ASCII allowed)
        let result = validate_user_code("ÀBCD-ÉFGH");
        assert!(result.is_err());
    }

    // =========================================================================
    // UserCodeError display tests
    // =========================================================================

    #[test]
    fn user_code_error_display_empty() {
        let err = UserCodeError::Empty;
        assert_eq!(err.to_string(), "user code is empty");
    }

    #[test]
    fn user_code_error_display_format() {
        let err = UserCodeError::InvalidFormat {
            code: "BAD".to_string(),
            expected: "XXXX-XXXX".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("BAD"));
        assert!(msg.contains("XXXX-XXXX"));
    }

    #[test]
    fn user_code_error_display_chars() {
        let err = UserCodeError::InvalidCharacters {
            code: "AB12-CD34".to_string(),
        };
        assert!(err.to_string().contains("AB12-CD34"));
    }

    // =========================================================================
    // AuthFlowResult serde tests
    // =========================================================================

    #[test]
    fn auth_flow_result_success_serde() {
        let result = AuthFlowResult::Success { elapsed_ms: 1234 };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"status\":\"success\""));
        assert!(json.contains("\"elapsed_ms\":1234"));

        let deserialized: AuthFlowResult = serde_json::from_str(&json).unwrap();
        match deserialized {
            AuthFlowResult::Success { elapsed_ms } => assert_eq!(elapsed_ms, 1234),
            _ => panic!("Expected Success variant"),
        }
    }

    #[test]
    fn auth_flow_result_interactive_serde() {
        let result = AuthFlowResult::InteractiveBootstrapRequired {
            reason: "password required".to_string(),
            artifacts_dir: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"status\":\"interactive_required\""));
        assert!(json.contains("password required"));
        // artifacts_dir should be absent (skip_serializing_if)
        assert!(!json.contains("artifacts_dir"));
    }

    #[test]
    fn auth_flow_result_failed_serde() {
        let result = AuthFlowResult::Failed {
            error: "timeout".to_string(),
            kind: AuthFlowFailureKind::NavigationFailed,
            artifacts_dir: Some(PathBuf::from("/tmp/artifacts")),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"status\":\"failed\""));
        assert!(json.contains("NavigationFailed"));
        assert!(json.contains("/tmp/artifacts"));
    }

    // =========================================================================
    // Config tests
    // =========================================================================

    #[test]
    fn default_config() {
        let cfg = OpenAiDeviceAuthConfig::default();
        assert_eq!(cfg.device_url, "https://auth.openai.com/codex/device");
        assert_eq!(cfg.flow_timeout_ms, 60_000);
    }

    #[test]
    fn config_serde_round_trip() {
        let cfg = OpenAiDeviceAuthConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let deserialized: OpenAiDeviceAuthConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.device_url, cfg.device_url);
        assert_eq!(deserialized.flow_timeout_ms, cfg.flow_timeout_ms);
    }

    #[test]
    fn selectors_default_populated() {
        let sel = DevicePageSelectors::default();
        assert!(!sel.code_input.is_empty());
        assert!(!sel.submit_button.is_empty());
        assert!(!sel.email_prompt.is_empty());
        assert!(!sel.password_prompt.is_empty());
        assert!(!sel.success_marker.is_empty());
    }

    #[test]
    fn selectors_serde_round_trip() {
        let sel = DevicePageSelectors::default();
        let json = serde_json::to_string(&sel).unwrap();
        let deserialized: DevicePageSelectors = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.code_input, sel.code_input);
        assert_eq!(deserialized.success_marker, sel.success_marker);
    }

    // =========================================================================
    // AuthFlowFailureKind tests
    // =========================================================================

    #[test]
    fn failure_kind_serde() {
        let kinds = vec![
            AuthFlowFailureKind::InvalidUserCode,
            AuthFlowFailureKind::BrowserNotReady,
            AuthFlowFailureKind::NavigationFailed,
            AuthFlowFailureKind::SelectorMismatch,
            AuthFlowFailureKind::BotDetected,
            AuthFlowFailureKind::VerificationFailed,
            AuthFlowFailureKind::PlaywrightError,
            AuthFlowFailureKind::Unknown,
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).unwrap();
            let deserialized: AuthFlowFailureKind = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, kind);
        }
    }

    // =========================================================================
    // Flow construction tests
    // =========================================================================

    #[test]
    fn flow_with_defaults() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        assert_eq!(
            flow.config().device_url,
            "https://auth.openai.com/codex/device"
        );
    }

    #[test]
    fn flow_with_artifacts() {
        let flow = OpenAiDeviceAuthFlow::with_defaults().with_artifacts("/tmp/artifacts");
        assert!(flow.artifacts.is_some());
    }

    #[test]
    fn flow_custom_config() {
        let cfg = OpenAiDeviceAuthConfig {
            device_url: "https://custom.auth/device".to_string(),
            flow_timeout_ms: 30_000,
            ..Default::default()
        };
        let flow = OpenAiDeviceAuthFlow::new(cfg);
        assert_eq!(flow.config().device_url, "https://custom.auth/device");
        assert_eq!(flow.config().flow_timeout_ms, 30_000);
    }

    // =========================================================================
    // Flow execution tests (unit level)
    // =========================================================================

    #[test]
    fn execute_rejects_invalid_code() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let data_dir = std::env::temp_dir().join("wa_test_auth_flow");
        let mut ctx =
            super::super::BrowserContext::new(super::super::BrowserConfig::default(), &data_dir);
        // Force status to Ready for testing
        ctx.status = BrowserStatus::Ready;

        let result = flow.execute(&ctx, "BAD", "test-account", None);
        match result {
            AuthFlowResult::Failed { kind, .. } => {
                assert_eq!(kind, AuthFlowFailureKind::InvalidUserCode);
            }
            _ => panic!("Expected Failed with InvalidUserCode"),
        }
    }

    #[test]
    fn execute_rejects_not_ready_context() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let data_dir = std::env::temp_dir().join("wa_test_auth_flow_nr");
        let ctx =
            super::super::BrowserContext::new(super::super::BrowserConfig::default(), &data_dir);
        // ctx is NotInitialized by default

        let result = flow.execute(&ctx, "ABCD-EFGH", "test-account", None);
        match result {
            AuthFlowResult::Failed { kind, .. } => {
                assert_eq!(kind, AuthFlowFailureKind::BrowserNotReady);
            }
            _ => panic!("Expected Failed with BrowserNotReady"),
        }
    }

    // =========================================================================
    // Playwright result parsing tests
    // =========================================================================

    #[test]
    fn parse_success_result() {
        let result = OpenAiDeviceAuthFlow::parse_playwright_result(r#"{"status":"success"}"#);
        assert!(matches!(result, Ok(PlaywrightOutcome::Success)));
    }

    #[test]
    fn parse_interactive_required_result() {
        let result = OpenAiDeviceAuthFlow::parse_playwright_result(
            r#"{"status":"interactive_required","reason":"password needed"}"#,
        );
        match result {
            Ok(PlaywrightOutcome::InteractiveRequired(reason)) => {
                assert_eq!(reason, "password needed");
            }
            _ => panic!("Expected InteractiveRequired"),
        }
    }

    #[test]
    fn parse_error_result() {
        let result = OpenAiDeviceAuthFlow::parse_playwright_result(
            r#"{"status":"error","kind":"VerificationFailed","message":"no marker"}"#,
        );
        match result {
            Err(e) => {
                assert_eq!(e.kind, AuthFlowFailureKind::VerificationFailed);
                assert_eq!(e.error, "no marker");
            }
            _ => panic!("Expected error"),
        }
    }

    #[test]
    fn parse_empty_output() {
        let result = OpenAiDeviceAuthFlow::parse_playwright_result("");
        assert!(result.is_err());
    }

    #[test]
    fn parse_output_with_preceding_lines() {
        let output = "Debugger attached.\nSome warning\n{\"status\":\"success\"}";
        let result = OpenAiDeviceAuthFlow::parse_playwright_result(output);
        assert!(matches!(result, Ok(PlaywrightOutcome::Success)));
    }

    #[test]
    fn parse_malformed_json() {
        let result = OpenAiDeviceAuthFlow::parse_playwright_result("not json at all");
        assert!(result.is_err());
    }

    #[test]
    fn parse_unknown_status() {
        let result = OpenAiDeviceAuthFlow::parse_playwright_result(r#"{"status":"unexpected"}"#);
        assert!(result.is_err());
    }

    // =========================================================================
    // Artifact tests
    // =========================================================================

    #[test]
    fn artifact_capture_creates_dir() {
        let temp = std::env::temp_dir().join(format!("wa_artifact_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);

        let capture = ArtifactCapture::new(&temp);
        let dir = capture.ensure_invocation_dir("openai_device").unwrap();
        assert!(dir.is_dir());
        assert!(dir.starts_with(&temp));

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn artifact_write_and_read() {
        let temp =
            std::env::temp_dir().join(format!("wa_artifact_write_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let content = b"Test failure report";
        let path =
            ArtifactCapture::write_artifact(&temp, ArtifactKind::FailureReport, content).unwrap();
        assert_eq!(path.file_name().unwrap(), "failure_report.txt");
        assert_eq!(std::fs::read(&path).unwrap(), content);

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn artifact_write_screenshot() {
        let temp = std::env::temp_dir().join(format!(
            "wa_artifact_screenshot_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let content = b"\x89PNG fake screenshot data";
        let path =
            ArtifactCapture::write_artifact(&temp, ArtifactKind::Screenshot, content).unwrap();
        assert_eq!(path.file_name().unwrap(), "screenshot.png");

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn artifact_write_redacted_dom() {
        let temp =
            std::env::temp_dir().join(format!("wa_artifact_dom_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let content = b"<html><body>[REDACTED]</body></html>";
        let path =
            ArtifactCapture::write_artifact(&temp, ArtifactKind::RedactedDom, content).unwrap();
        assert_eq!(path.file_name().unwrap(), "redacted_dom.html");

        let _ = std::fs::remove_dir_all(&temp);
    }

    // =========================================================================
    // ArtifactKind serde tests
    // =========================================================================

    #[test]
    fn artifact_kind_serde() {
        let kinds = vec![
            ArtifactKind::Screenshot,
            ArtifactKind::RedactedDom,
            ArtifactKind::FailureReport,
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).unwrap();
            let deserialized: ArtifactKind = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, kind);
        }
    }

    // =========================================================================
    // Playwright error parsing tests
    // =========================================================================

    #[test]
    fn parse_playwright_error_with_json() {
        let stdout = r#"{"status":"error","kind":"NavigationFailed","message":"timeout"}"#;
        let status = std::process::Command::new("false")
            .status()
            .unwrap_or_else(|_| std::process::ExitStatus::default());
        let err = OpenAiDeviceAuthFlow::parse_playwright_error(stdout, "", status);
        assert_eq!(err.kind, AuthFlowFailureKind::NavigationFailed);
        assert_eq!(err.error, "timeout");
    }

    #[test]
    fn parse_playwright_error_fallback_to_stderr() {
        let stderr = "Error: browser not found\nsome detail";
        let status = std::process::Command::new("false")
            .status()
            .unwrap_or_else(|_| std::process::ExitStatus::default());
        let err = OpenAiDeviceAuthFlow::parse_playwright_error("", stderr, status);
        assert_eq!(err.kind, AuthFlowFailureKind::PlaywrightError);
        assert!(err.error.contains("browser not found"));
    }

    // =========================================================================
    // Playwright script generation tests
    // =========================================================================

    #[test]
    fn script_contains_device_url() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        let script = flow.build_playwright_script(&profile_dir, "ABCD-EFGH", None, None);
        assert!(script.contains("auth.openai.com/codex/device"));
        assert!(script.contains("ABCD-EFGH"));
        assert!(script.contains("/tmp/profile"));
    }

    #[test]
    fn script_with_email() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        let script =
            flow.build_playwright_script(&profile_dir, "ABCD-EFGH", Some("user@example.com"), None);
        assert!(script.contains("user@example.com"));
    }

    #[test]
    fn script_with_artifacts_dir() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        let artifacts_dir = PathBuf::from("/tmp/artifacts");
        let script =
            flow.build_playwright_script(&profile_dir, "ABCD-EFGH", None, Some(&artifacts_dir));
        assert!(script.contains("/tmp/artifacts"));
    }

    #[test]
    fn script_escapes_single_quotes_in_code() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        // This shouldn't normally happen but tests defensive coding
        let script = flow.build_playwright_script(&profile_dir, "AB'D-EF'H", None, None);
        // Single quotes should be escaped
        assert!(script.contains("\\'"));
    }

    #[test]
    fn script_null_email_when_none() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        let script = flow.build_playwright_script(&profile_dir, "ABCD-EFGH", None, None);
        assert!(script.contains("const email = null"));
    }

    #[test]
    fn script_null_artifacts_when_none() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        let script = flow.build_playwright_script(&profile_dir, "ABCD-EFGH", None, None);
        assert!(script.contains("const artifactsDir = null"));
    }
}
