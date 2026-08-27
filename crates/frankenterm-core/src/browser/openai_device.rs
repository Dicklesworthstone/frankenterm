//! OpenAI/Codex device auth flow via Playwright.
//!
//! Automates the device-code authorization flow at `auth.openai.com/codex/device`.
//! The flow safely prepares and then reuses a persistent Playwright browser
//! profile for the target account via [`super::BrowserProfile`].
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
//! - When explicitly configured, failure artifacts are written to private
//!   per-invocation files. Screenshots can contain sensitive page content and
//!   must be handled accordingly; only textual reports are content-free.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{BootstrapMethod, BrowserContext, BrowserStatus};

const USER_CODE_MAX_BYTES: usize = 128;

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
    if trimmed.len() > USER_CODE_MAX_BYTES {
        return Err(UserCodeError::TooLong {
            max_bytes: USER_CODE_MAX_BYTES,
        });
    }

    // Normalize to uppercase
    let normalized = trimmed.to_ascii_uppercase();

    // Check format: 4+ alphanumeric, hyphen, 4+ alphanumeric
    let parts: Vec<&str> = normalized.split('-').collect();
    if parts.len() != 2 {
        return Err(UserCodeError::InvalidFormat {
            expected: "XXXX-YYYY (4+ alphanumeric, hyphen, 4+ alphanumeric)".to_string(),
        });
    }

    for part in &parts {
        if part.len() < 4 {
            return Err(UserCodeError::InvalidFormat {
                expected: "XXXX-YYYY (4+ alphanumeric, hyphen, 4+ alphanumeric)".to_string(),
            });
        }
        if !part.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(UserCodeError::InvalidCharacters);
        }
    }

    Ok(normalized)
}

/// Errors from user code validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserCodeError {
    /// Code string was empty or whitespace-only.
    Empty,
    /// Code did not match the expected XXXX-YYYY format.
    InvalidFormat { expected: String },
    /// Code contained non-ASCII or non-alphanumeric characters.
    InvalidCharacters,
    /// Code exceeded the finite pre-normalization byte limit.
    TooLong { max_bytes: usize },
}

impl std::fmt::Display for UserCodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "user code is empty"),
            Self::InvalidFormat { expected } => {
                write!(f, "invalid user code format: expected {expected}")
            }
            Self::InvalidCharacters => {
                write!(
                    f,
                    "user code contains invalid characters (expected ASCII letters or digits only)"
                )
            }
            Self::TooLong { max_bytes } => {
                write!(f, "user code exceeds the {max_bytes}-byte safety limit")
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
    /// The isolated browser profile directory could not be prepared safely.
    ProfileUnavailable,
    /// Auth succeeded in the browser but durable profile state could not be committed.
    ProfilePersistenceFailed,
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

impl AuthFlowFailureKind {
    pub(super) const fn stable_detail(&self) -> &'static str {
        match self {
            Self::InvalidUserCode => "The device user code is invalid",
            Self::BrowserNotReady => "The browser automation context is not ready",
            Self::ProfileUnavailable => "The isolated browser profile directory is unavailable",
            Self::ProfilePersistenceFailed => {
                "Authenticated browser profile state could not be persisted safely"
            }
            Self::NavigationFailed => "Browser navigation did not complete successfully",
            Self::SelectorMismatch => "The authentication page layout was not recognized",
            Self::BotDetected => "The authentication service requested human verification",
            Self::VerificationFailed => "Authentication completion could not be verified",
            Self::PlaywrightError => "The browser automation subprocess failed",
            Self::Unknown => "The browser authentication flow returned an unrecognized result",
        }
    }

    pub(super) fn from_script_label(label: Option<&str>) -> Self {
        match label {
            Some("VerificationFailed") => Self::VerificationFailed,
            Some("SelectorMismatch") => Self::SelectorMismatch,
            Some("NavigationFailed") => Self::NavigationFailed,
            Some("BotDetected") => Self::BotDetected,
            Some("PlaywrightError") => Self::PlaywrightError,
            _ => Self::Unknown,
        }
    }
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

/// Captures private failure artifacts for debugging.
///
/// Screenshots are intentionally exact diagnostic evidence and can contain
/// sensitive page content. Their 0600 mode and 0700 parent directory limit
/// access; callers must not publish them as though they were redacted.
#[derive(Debug, Clone)]
pub struct ArtifactCapture {
    /// Root directory for artifacts (e.g., `<workspace>/.ft/artifacts/`).
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

pub(super) const SCREENSHOT_ARTIFACT_MAX_BYTES: usize = 32 * 1024 * 1024;
const REDACTED_DOM_ARTIFACT_MAX_BYTES: usize = 2 * 1024 * 1024;
const FAILURE_REPORT_ARTIFACT_MAX_BYTES: usize = 64 * 1024;

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
    /// Returns a freshly created 0700 path beneath the configured root. The
    /// flow name is a finite strict component and neither the root, flow, nor
    /// invocation leaf may be a symlink while admitted. A same-UID actor can
    /// still replace a returned pathname after admission; JavaScript writers
    /// therefore use exclusive creation and Rust writers re-open the directory
    /// without following its final component.
    pub fn ensure_invocation_dir(&self, flow_name: &str) -> Result<PathBuf, std::io::Error> {
        super::create_private_invocation_directory(&self.artifacts_root, flow_name)
            .map_err(|_| std::io::Error::other("artifact invocation directory is unavailable"))
    }

    /// Write a text artifact to the invocation directory.
    pub fn write_artifact(
        dir: &Path,
        kind: ArtifactKind,
        content: &[u8],
    ) -> Result<PathBuf, std::io::Error> {
        let (filename, max_bytes) = match kind {
            ArtifactKind::Screenshot => ("screenshot.png", SCREENSHOT_ARTIFACT_MAX_BYTES),
            ArtifactKind::RedactedDom => ("redacted_dom.html", REDACTED_DOM_ARTIFACT_MAX_BYTES),
            ArtifactKind::FailureReport => {
                ("failure_report.txt", FAILURE_REPORT_ARTIFACT_MAX_BYTES)
            }
        };
        let path = super::write_private_file_create_new(dir, filename, content, max_bytes)
            .map_err(|_| std::io::Error::other("artifact could not be written safely"))?;
        tracing::debug!(
            artifact_kind = ?kind,
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
                error: AuthFlowFailureKind::BrowserNotReady
                    .stable_detail()
                    .to_string(),
                kind: AuthFlowFailureKind::BrowserNotReady,
                artifacts_dir: None,
            };
        }

        if super::admit_automated_auth_url(
            super::BrowserAuthService::OpenAi,
            &self.config.device_url,
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

        // Step 3: Resolve the browser profile
        let profile = match ctx.try_profile("openai", account) {
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
                &normalized_code,
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

        tracing::info!("Starting OpenAI device auth flow");
        // NOTE: user_code is intentionally NOT logged (secret material)

        // Step 4: Build and run the Playwright script
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
            &normalized_code,
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
            .and_then(|a| match a.ensure_invocation_dir("openai_device") {
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
    ///
    /// This generates a Node.js script and delivers it through bounded stdin
    /// to `node -`; sensitive flow values never enter argv or the environment.
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
        browser_config: &super::BrowserConfig,
    ) -> Result<PlaywrightOutcome, PlaywrightFlowError> {
        let script = self
            .build_playwright_script_with_browser_config(
                profile_dir,
                user_code,
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
            // Parse structured error from stdout if possible
            return Err(Self::parse_playwright_error(&stdout));
        }

        // Parse the JSON result from stdout
        Self::parse_playwright_result(&stdout)
    }

    /// Build the Node.js/Playwright script for the device auth flow.
    ///
    /// The script outputs a JSON result to stdout with one of:
    /// - `{"status":"success","storage_state":{...}}`
    /// - `{"status":"interactive_required","reason":"..."}`
    /// - `{"status":"error","kind":"...","message":"..."}`
    fn build_playwright_script_with_browser_config(
        &self,
        profile_dir: &Path,
        user_code: &str,
        email: Option<&str>,
        artifacts_dir: Option<&Path>,
        browser_config: &super::BrowserConfig,
    ) -> Result<String, super::BrowserNodeCommandFailure> {
        let sel = &self.config.selectors;
        super::admit_browser_timeout(self.config.flow_timeout_ms)?;
        super::admit_browser_timeout(browser_config.navigation_timeout_ms)?;
        super::admit_browser_timeout(browser_config.page_load_timeout_ms)?;
        super::admit_automated_auth_url(
            super::BrowserAuthService::OpenAi,
            &self.config.device_url,
        )?;
        for selector_group in [
            &sel.code_input,
            &sel.submit_button,
            &sel.email_prompt,
            &sel.email_input,
            &sel.email_submit,
            &sel.password_prompt,
            &sel.success_marker,
        ] {
            super::admit_selector_group(selector_group)?;
        }
        super::admit_node_script_input_parts(
            &[
                Some(&self.config.device_url),
                Some(user_code),
                email,
                Some(&sel.code_input),
                Some(&sel.submit_button),
                Some(&sel.email_prompt),
                Some(&sel.email_input),
                Some(&sel.email_submit),
                Some(&sel.password_prompt),
                Some(&sel.success_marker),
            ],
            &[Some(profile_dir), artifacts_dir],
            &[],
        )?;
        let input = serde_json::json!({
            "profile_dir": profile_dir.to_string_lossy(),
            "device_url": &self.config.device_url,
            "user_code": user_code,
            "email": email,
            "artifacts_dir": artifacts_dir.map(|path| path.to_string_lossy().into_owned()),
            "timeout_ms": self.config.flow_timeout_ms,
            "headless": browser_config.headless,
            "navigation_timeout_ms": browser_config.navigation_timeout_ms,
            "page_load_timeout_ms": browser_config.page_load_timeout_ms,
            "screenshot_max_bytes": SCREENSHOT_ARTIFACT_MAX_BYTES,
            "selectors": {
                "code_input": &sel.code_input,
                "submit_button": &sel.submit_button,
                "email_prompt": &sel.email_prompt,
                "email_input": &sel.email_input,
                "email_submit": &sel.email_submit,
                "password_prompt": &sel.password_prompt,
                "success_marker": &sel.success_marker,
            },
        });
        let input_base64 = super::encode_node_script_input(&input)?;

        super::admit_node_script_source(format!(
            r"
const {{ chromium }} = require(process.argv[2]);
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

function matchesDeviceDestination(currentValue, deviceValue) {{
  try {{
    const current = new URL(currentValue);
    const expected = new URL(deviceValue);
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
  const deviceUrl = input.device_url;
  const userCode = input.user_code;
  const email = input.email;
  const artifactsDir = input.artifacts_dir;
  const selectors = input.selectors;

  async function finishSuccess() {{
    if (!matchesDeviceDestination(page.url(), deviceUrl)) {{
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

    // Navigate to device auth page
    try {{
      await page.goto(deviceUrl, {{ waitUntil: 'domcontentloaded', timeout: NAVIGATION_TIMEOUT }});
    }} catch (_) {{
      console.log(JSON.stringify({{ status: 'error', kind: 'NavigationFailed' }}));
      await browser.close().catch(() => {{}});
      process.exit(1);
    }}

    // Detect page state
    const passwordEl = await page.$(selectors.password_prompt);
    if (passwordEl) {{
      // Password/MFA required — cannot automate
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
    const emailPromptEl = await page.$(selectors.email_prompt);
    if (emailPromptEl && email) {{
      const emailInput = await page.$(selectors.email_input);
      if (!emailInput) {{
        console.log(JSON.stringify({{ status: 'error', kind: 'SelectorMismatch' }}));
        await browser.close();
        process.exit(1);
      }}
      await emailInput.fill(email);
      const emailSubmit = await page.$(selectors.email_submit);
      if (emailSubmit) {{
        await emailSubmit.click();
      }} else {{
        await emailInput.press('Enter');
      }}
      // Wait for navigation after email submission
      await page.waitForLoadState('domcontentloaded', {{ timeout: PAGE_LOAD_TIMEOUT }});

      const postEmailPassword = await page.$(selectors.password_prompt);
      if (postEmailPassword) {{
        if (artifactsDir) await captureScreenshot(page, artifactsDir);
        console.log(JSON.stringify({{
          status: 'interactive_required',
          reason: 'Password or MFA prompt detected — interactive bootstrap required'
        }}));
        await browser.close();
        process.exit(0);
      }}
    }} else if (emailPromptEl && !email) {{
      if (artifactsDir) {{
        await captureScreenshot(page, artifactsDir);
      }}
      console.log(JSON.stringify({{
        status: 'interactive_required',
        reason: 'Email prompt detected but no email provided'
      }}));
      await browser.close();
      process.exit(0);
    }}

    // Fill the user code
    let codeInput;
    try {{
      codeInput = await page.waitForSelector(
        selectors.code_input,
        {{ timeout: PAGE_LOAD_TIMEOUT }}
      );
    }} catch (_) {{
      console.log(JSON.stringify({{ status: 'error', kind: 'SelectorMismatch' }}));
      await browser.close().catch(() => {{}});
      process.exit(1);
    }}
    await codeInput.fill(userCode);

    // Submit
    const submitBtn = await page.$(selectors.submit_button);
    if (submitBtn) {{
      await submitBtn.click();
    }} else {{
      // Try pressing Enter as fallback
      await codeInput.press('Enter');
    }}

    // Verify success
    const successSelectors = selectors.success_marker.split(', ');
    const successTimeout = Math.min(10000, TIMEOUT);
    const successChecks = successSelectors.map(sel =>
      page.waitForSelector(sel, {{ timeout: successTimeout }})
        .then(() => true)
    );
    const found = await Promise.any(successChecks).catch(() => false);

    if (found) {{
      await finishSuccess();
    }} else {{
      if (artifactsDir) {{
        await captureScreenshot(page, artifactsDir);
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
        user_code: &str,
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
            user_code,
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

        // Find the last JSON line (script may produce other output before)
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
                let reason = match parsed.get("reason").and_then(|value| value.as_str()) {
                    Some("Password or MFA prompt detected — interactive bootstrap required") => {
                        "Password or MFA prompt detected — interactive bootstrap required"
                    }
                    Some("Email prompt detected but no email provided") => {
                        "Email prompt detected but no email provided"
                    }
                    _ => "Interactive login is required to continue",
                }
                .to_string();
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
        // Try to get structured error from stdout first
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
    /// Flow completed successfully.
    Success { storage_state: Vec<u8> },
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

    fn canonical_system_temp_dir() -> PathBuf {
        std::fs::canonicalize(std::env::temp_dir())
            .expect("system temporary directory must be resolvable")
    }

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
    fn validate_valid_code_with_digits() {
        let result = validate_user_code("AB12-CD34");
        assert_eq!(result.unwrap(), "AB12-CD34");
    }

    #[test]
    fn validate_valid_code_longer_parts() {
        let result = validate_user_code("WXYZ-98765");
        assert_eq!(result.unwrap(), "WXYZ-98765");
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
    fn validate_digits_in_code_allowed() {
        let result = validate_user_code("AB12-CD34");
        assert_eq!(result.unwrap(), "AB12-CD34");
    }

    #[test]
    fn validate_special_chars() {
        let result = validate_user_code("AB@D-EF!H");
        assert!(matches!(result, Err(UserCodeError::InvalidCharacters)));
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
        assert!(matches!(result, Err(UserCodeError::InvalidCharacters)));
    }

    #[test]
    fn validate_user_code_enforces_exact_pre_normalization_limit() {
        let exact = format!("{}-{}", "a".repeat(63), "b".repeat(64));
        assert_eq!(exact.len(), USER_CODE_MAX_BYTES);
        assert!(validate_user_code(&exact).is_ok());

        let one_over = format!("{}-{}", "a".repeat(64), "b".repeat(64));
        assert_eq!(one_over.len(), USER_CODE_MAX_BYTES + 1);
        assert_eq!(
            validate_user_code(&one_over),
            Err(UserCodeError::TooLong {
                max_bytes: USER_CODE_MAX_BYTES,
            })
        );
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
            expected: "XXXX-YYYY".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("XXXX-YYYY"));
    }

    #[test]
    fn user_code_error_display_chars() {
        let err = UserCodeError::InvalidCharacters;
        assert!(err.to_string().contains("letters or digits"));
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
        assert_ne!(sel.code_input, "");
        assert_ne!(sel.submit_button, "");
        assert_ne!(sel.email_prompt, "");
        assert_ne!(sel.password_prompt, "");
        assert_ne!(sel.success_marker, "");
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
            AuthFlowFailureKind::ProfileUnavailable,
            AuthFlowFailureKind::ProfilePersistenceFailed,
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
    fn custom_origin_config_is_rejected_before_script_execution() {
        let cfg = OpenAiDeviceAuthConfig {
            device_url: "https://custom.auth/device".to_string(),
            flow_timeout_ms: 30_000,
            ..Default::default()
        };
        let flow = OpenAiDeviceAuthFlow::new(cfg);
        assert_eq!(flow.config().device_url, "https://custom.auth/device");
        assert_eq!(flow.config().flow_timeout_ms, 30_000);
        assert_eq!(
            flow.build_playwright_script_with_browser_config(
                Path::new("/tmp/profile"),
                "ABCD-EFGH",
                None,
                None,
                &super::super::BrowserConfig::default(),
            ),
            Err(super::super::BrowserNodeCommandFailure::InvalidConfiguration)
        );
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

    #[test]
    fn execute_rejects_untrusted_device_url_before_profile_creation() {
        let temp = tempfile::tempdir().expect("isolated OpenAI URL rejection root");
        let mut ctx =
            super::super::BrowserContext::new(super::super::BrowserConfig::default(), temp.path());
        ctx.status = BrowserStatus::Ready;
        let mut config = OpenAiDeviceAuthConfig::default();
        config.device_url = "https://127.0.0.1/codex/device".to_string();
        let result =
            OpenAiDeviceAuthFlow::new(config).execute(&ctx, "ABCD-EFGH", "untrusted-url", None);
        assert!(matches!(
            result,
            AuthFlowResult::Failed {
                kind: AuthFlowFailureKind::PlaywrightError,
                ..
            }
        ));
        assert!(!ctx.profile("openai", "untrusted-url").path().exists());
    }

    #[test]
    fn execute_rejects_invalid_selectors_before_profile_creation() {
        let temp = tempfile::tempdir().expect("isolated OpenAI preflight root");
        let mut ctx =
            super::super::BrowserContext::new(super::super::BrowserConfig::default(), temp.path());
        ctx.status = BrowserStatus::Ready;
        let mut config = OpenAiDeviceAuthConfig::default();
        config.selectors.success_marker.clear();
        let result =
            OpenAiDeviceAuthFlow::new(config).execute(&ctx, "ABCD-EFGH", "invalid-selectors", None);
        assert!(matches!(
            result,
            AuthFlowResult::Failed {
                kind: AuthFlowFailureKind::PlaywrightError,
                ..
            }
        ));
        assert!(!ctx.profile("openai", "invalid-selectors").path().exists());
    }

    #[test]
    fn execute_rejects_invalid_account_identity_before_profile_creation() {
        let temp = tempfile::tempdir().expect("isolated OpenAI identity root");
        let mut ctx = BrowserContext::new(super::super::BrowserConfig::default(), temp.path());
        ctx.status = BrowserStatus::Ready;
        let result = OpenAiDeviceAuthFlow::with_defaults().execute(
            &ctx,
            "ABCD-EFGH",
            "spoof\u{202e}txt",
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

    #[test]
    fn execute_rejects_an_unavailable_profile_before_subprocess_admission() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let temp = tempfile::tempdir().expect("isolated profile admission root");
        let unavailable_data_dir = temp.path().join("not-a-directory");
        std::fs::write(&unavailable_data_dir, b"profile root obstruction")
            .expect("profile root obstruction fixture");
        let mut ctx = super::super::BrowserContext::new(
            super::super::BrowserConfig::default(),
            &unavailable_data_dir,
        );
        ctx.status = BrowserStatus::Ready;

        let result = flow.execute(&ctx, "ABCD-EFGH", "test-account", None);
        match result {
            AuthFlowResult::Failed {
                error,
                kind,
                artifacts_dir,
            } => {
                assert_eq!(kind, AuthFlowFailureKind::ProfileUnavailable);
                assert_eq!(error, kind.stable_detail());
                assert!(artifacts_dir.is_none());
            }
            _ => panic!("Expected profile admission to fail before subprocess execution"),
        }
    }

    // =========================================================================
    // Playwright result parsing tests
    // =========================================================================

    #[test]
    fn parse_success_result() {
        let result = OpenAiDeviceAuthFlow::parse_playwright_result(
            r#"{"status":"success","storage_state":{"cookies":[],"origins":[]}}"#,
        );
        assert!(matches!(result, Ok(PlaywrightOutcome::Success { .. })));
    }

    #[test]
    fn parse_interactive_required_result() {
        let result = OpenAiDeviceAuthFlow::parse_playwright_result(
            r#"{"status":"interactive_required","reason":"password needed"}"#,
        );
        match result {
            Ok(PlaywrightOutcome::InteractiveRequired(reason)) => {
                assert_eq!(reason, "Interactive login is required to continue");
                assert!(!reason.contains("password needed"));
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
                assert_eq!(
                    e.error,
                    AuthFlowFailureKind::VerificationFailed.stable_detail()
                );
                assert!(!e.error.contains("no marker"));
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
        let output = "Debugger attached.\nSome warning\n{\"status\":\"success\",\"storage_state\":{\"cookies\":[],\"origins\":[]}}";
        let result = OpenAiDeviceAuthFlow::parse_playwright_result(output);
        assert!(matches!(result, Ok(PlaywrightOutcome::Success { .. })));
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
        let temp =
            canonical_system_temp_dir().join(format!("wa_artifact_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);

        let capture = ArtifactCapture::new(&temp);
        let dir = capture.ensure_invocation_dir("openai_device").unwrap();
        assert!(dir.is_dir());
        assert!(dir.starts_with(&temp));

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn artifact_write_and_read() {
        let temp = canonical_system_temp_dir()
            .join(format!("wa_artifact_write_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        let invocation = ArtifactCapture::new(&temp)
            .ensure_invocation_dir("openai_device")
            .expect("private artifact invocation");

        let content = b"Test failure report";
        let path =
            ArtifactCapture::write_artifact(&invocation, ArtifactKind::FailureReport, content)
                .unwrap();
        assert_eq!(path.file_name().unwrap(), "failure_report.txt");
        assert_eq!(std::fs::read(&path).unwrap(), content);

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn artifact_write_screenshot() {
        let temp = canonical_system_temp_dir().join(format!(
            "wa_artifact_screenshot_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);
        let invocation = ArtifactCapture::new(&temp)
            .ensure_invocation_dir("openai_device")
            .expect("private artifact invocation");

        let content = b"\x89PNG fake screenshot data";
        let path = ArtifactCapture::write_artifact(&invocation, ArtifactKind::Screenshot, content)
            .unwrap();
        assert_eq!(path.file_name().unwrap(), "screenshot.png");

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn artifact_write_redacted_dom() {
        let temp = canonical_system_temp_dir()
            .join(format!("wa_artifact_dom_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        let invocation = ArtifactCapture::new(&temp)
            .ensure_invocation_dir("openai_device")
            .expect("private artifact invocation");

        let content = b"<html><body>[REDACTED]</body></html>";
        let path = ArtifactCapture::write_artifact(&invocation, ArtifactKind::RedactedDom, content)
            .unwrap();
        assert_eq!(path.file_name().unwrap(), "redacted_dom.html");

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[cfg(unix)]
    #[test]
    fn artifact_directories_and_files_are_private_exclusive_and_nofollow() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir().expect("isolated artifact root");
        let temp_path = std::fs::canonicalize(temp.path()).expect("canonical artifact root");
        let root = temp_path.join("artifacts");
        let capture = ArtifactCapture::new(&root);
        assert!(capture.ensure_invocation_dir("").is_err());
        assert!(capture.ensure_invocation_dir("../escape").is_err());
        assert!(capture.ensure_invocation_dir(&"x".repeat(65)).is_err());

        let invocation = capture
            .ensure_invocation_dir("openai_device")
            .expect("fresh private invocation directory");
        assert_eq!(
            std::fs::metadata(&invocation)
                .expect("invocation metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let report = b"finite content-free report";
        let report_path =
            ArtifactCapture::write_artifact(&invocation, ArtifactKind::FailureReport, report)
                .expect("exclusive report creation");
        assert_eq!(
            std::fs::metadata(&report_path)
                .expect("report metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(
            ArtifactCapture::write_artifact(
                &invocation,
                ArtifactKind::FailureReport,
                b"must not overwrite",
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(&report_path).expect("retained report"),
            report
        );

        let symlink_invocation = capture
            .ensure_invocation_dir("openai_device")
            .expect("second private invocation directory");
        let outside = temp_path.join("outside.txt");
        std::fs::write(&outside, b"outside must remain unchanged").expect("outside fixture");
        symlink(&outside, symlink_invocation.join("failure_report.txt"))
            .expect("preplanted artifact symlink");
        assert!(
            ArtifactCapture::write_artifact(
                &symlink_invocation,
                ArtifactKind::FailureReport,
                b"must not follow",
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(&outside).expect("outside fixture remains readable"),
            b"outside must remain unchanged"
        );

        let oversized = vec![b'x'; FAILURE_REPORT_ARTIFACT_MAX_BYTES.saturating_add(1)];
        let oversized_invocation = capture
            .ensure_invocation_dir("openai_device")
            .expect("oversized artifact invocation directory");
        assert!(
            ArtifactCapture::write_artifact(
                &oversized_invocation,
                ArtifactKind::FailureReport,
                &oversized,
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn artifact_directory_traversal_rejects_preplanted_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("isolated artifact traversal root");
        let temp_path =
            std::fs::canonicalize(temp.path()).expect("canonical artifact traversal root");
        let root = temp_path.join("artifacts");
        let outside = temp_path.join("outside");
        std::fs::create_dir(&root).expect("artifact root fixture");
        std::fs::create_dir(&outside).expect("outside fixture");
        symlink(&outside, root.join("google_auth")).expect("flow symlink fixture");
        let capture = ArtifactCapture::new(&root);
        assert!(capture.ensure_invocation_dir("google_auth").is_err());
        assert!(
            std::fs::read_dir(&outside)
                .expect("outside directory remains readable")
                .next()
                .is_none()
        );

        let linked_root = temp_path.join("linked-root");
        symlink(&outside, &linked_root).expect("root symlink fixture");
        assert!(
            ArtifactCapture::new(linked_root)
                .ensure_invocation_dir("openai_device")
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn artifact_capture_never_tightens_permissions_on_a_caller_owned_root() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("isolated artifact permission root");
        let broad_root = temp.path().join("caller-owned");
        std::fs::create_dir(&broad_root).expect("caller-owned root fixture");
        std::fs::set_permissions(&broad_root, std::fs::Permissions::from_mode(0o755))
            .expect("broad root permissions fixture");

        assert!(
            ArtifactCapture::new(&broad_root)
                .ensure_invocation_dir("openai_device")
                .is_err()
        );
        assert_eq!(
            std::fs::metadata(&broad_root)
                .expect("caller-owned root metadata")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
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
        let stdout = r#"{"status":"error","kind":"NavigationFailed","message":"/private/secret"}"#;
        let err = OpenAiDeviceAuthFlow::parse_playwright_error(stdout);
        assert_eq!(err.kind, AuthFlowFailureKind::NavigationFailed);
        assert_eq!(
            err.error,
            AuthFlowFailureKind::NavigationFailed.stable_detail()
        );
        assert!(!err.error.contains("secret"));
    }

    #[test]
    fn parse_playwright_error_fallback_is_content_free() {
        let err = OpenAiDeviceAuthFlow::parse_playwright_error("");
        assert_eq!(err.kind, AuthFlowFailureKind::PlaywrightError);
        assert_eq!(
            err.error,
            AuthFlowFailureKind::PlaywrightError.stable_detail()
        );
    }

    // =========================================================================
    // Playwright script generation tests
    // =========================================================================

    #[test]
    fn script_transports_sensitive_input_as_structured_base64() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        let script = flow
            .build_playwright_script(&profile_dir, "ABCD-EFGH", None, None, false)
            .expect("bounded OpenAI script");
        assert!(!script.contains("auth.openai.com/codex/device"));
        assert!(!script.contains("ABCD-EFGH"));
        assert!(!script.contains("/tmp/profile"));
        let input = super::super::decode_node_script_input(&script);
        assert_eq!(input["device_url"], "https://auth.openai.com/codex/device");
        assert_eq!(input["user_code"], "ABCD-EFGH");
        assert_eq!(input["profile_dir"], "/tmp/profile");
    }

    #[test]
    fn script_with_email() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        let script = flow
            .build_playwright_script(
                &profile_dir,
                "ABCD-EFGH",
                Some("user@example.com"),
                None,
                false,
            )
            .expect("bounded OpenAI script");
        assert!(!script.contains("user@example.com"));
        assert_eq!(
            super::super::decode_node_script_input(&script)["email"],
            "user@example.com"
        );
    }

    #[test]
    fn script_with_artifacts_dir() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        let artifacts_dir = PathBuf::from("/tmp/artifacts");
        let script = flow
            .build_playwright_script(&profile_dir, "ABCD-EFGH", None, Some(&artifacts_dir), false)
            .expect("bounded OpenAI script");
        assert!(!script.contains("/tmp/artifacts"));
        assert_eq!(
            super::super::decode_node_script_input(&script)["artifacts_dir"],
            "/tmp/artifacts"
        );
    }

    #[test]
    fn script_input_round_trips_hostile_text_without_literal_injection() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let hostile = "AB'\\\n\u{2028}D-EF'H";
        let profile_dir = PathBuf::from("/tmp/hostile'\\\n\u{2028}");
        let script = flow
            .build_playwright_script(
                &profile_dir,
                hostile,
                Some("mail'\\\n\u{2028}@example.com"),
                None,
                false,
            )
            .expect("bounded hostile OpenAI script");
        assert!(!script.contains(hostile));
        assert!(!script.contains('\u{2028}'));
        let input = super::super::decode_node_script_input(&script);
        assert_eq!(input["user_code"], hostile);
        assert_eq!(input["profile_dir"], profile_dir.to_string_lossy().as_ref());
        assert_eq!(input["email"], "mail'\\\n\u{2028}@example.com");
    }

    #[test]
    fn script_null_email_when_none() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        let script = flow
            .build_playwright_script(&profile_dir, "ABCD-EFGH", None, None, false)
            .expect("bounded OpenAI script");
        assert!(super::super::decode_node_script_input(&script)["email"].is_null());
    }

    #[test]
    fn script_null_artifacts_when_none() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let profile_dir = PathBuf::from("/tmp/profile");
        let script = flow
            .build_playwright_script(&profile_dir, "ABCD-EFGH", None, None, false)
            .expect("bounded OpenAI script");
        assert!(super::super::decode_node_script_input(&script)["artifacts_dir"].is_null());
    }

    #[test]
    fn openai_script_input_enforces_exact_and_one_over_field_limit() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let exact = "x".repeat(super::super::BROWSER_NODE_INPUT_MAX_FIELD_BYTES);
        assert!(
            flow.build_playwright_script(
                Path::new("/tmp/profile"),
                "ABCD-EFGH",
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
                "ABCD-EFGH",
                Some(&one_over),
                None,
                false,
            ),
            Err(super::super::BrowserNodeCommandFailure::ScriptOversized)
        );
    }

    #[test]
    fn success_requires_storage_state() {
        let result = OpenAiDeviceAuthFlow::parse_playwright_result(r#"{"status":"success"}"#);
        match result {
            Err(error) => assert_eq!(error.kind, AuthFlowFailureKind::ProfilePersistenceFailed),
            _ => panic!("success without durable state must fail closed"),
        }
        assert!(
            OpenAiDeviceAuthFlow::parse_playwright_result(
                r#"{"status":"success","storage_state":"{\"cookies\":[],\"origins\":[]}"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn script_honors_headless_and_avoids_sequential_or_generic_success_checks() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let script = flow
            .build_playwright_script(Path::new("/tmp/profile"), "ABCD-EFGH", None, None, true)
            .expect("bounded headless OpenAI script");
        let input = super::super::decode_node_script_input(&script);
        assert_eq!(input["headless"], true);
        assert!(script.contains("headless: input.headless"));
        assert!(script.contains("Promise.any(successChecks)"));
        assert!(!script.contains("Promise.all(successChecks)"));
        assert!(script.contains("postEmailPassword"));
        assert!(script.contains("matchesDeviceDestination(page.url(), deviceUrl)"));
        assert!(script.contains("browser.storageState()"));
        assert!(script.contains("fullPage: false"));
        assert!(!script.contains("'authorized'"));
        assert!(!script.contains("page.textContent('body')"));
    }

    #[test]
    fn script_propagates_and_validates_browser_operation_timeouts() {
        let flow = OpenAiDeviceAuthFlow::with_defaults();
        let browser_config = super::super::BrowserConfig {
            navigation_timeout_ms: 12_345,
            page_load_timeout_ms: 23_456,
            ..super::super::BrowserConfig::default()
        };
        let script = flow
            .build_playwright_script_with_browser_config(
                Path::new("/tmp/profile"),
                "ABCD-EFGH",
                None,
                None,
                &browser_config,
            )
            .expect("bounded OpenAI timeout configuration");
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
                "ABCD-EFGH",
                None,
                None,
                &invalid,
            ),
            Err(super::super::BrowserNodeCommandFailure::InvalidTimeout)
        );
    }

    #[test]
    fn node_runner_is_stdin_bounded_and_never_uses_inline_argv() {
        let source = include_str!("openai_device.rs");
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
}
