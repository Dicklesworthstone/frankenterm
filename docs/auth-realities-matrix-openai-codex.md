# Auth Realities Matrix: OpenAI/Codex Device Auth

> **Purpose**: Document the factual, repeatable understanding of which OpenAI/Codex auth paths can be fully automated and which require human intervention.
>
> **Resolves**: PLAN.md Open Question #3 for the OpenAI/Codex path.
>
> **Last Updated**: 2026-01-25
> **Author**: claude-opus

---

## 1. Overview

When a Codex CLI session hits its usage limit, `ft` needs to decide whether to:
1. Proceed automatically with credential failover, or
2. Pause and request human action

This matrix documents the real-world auth states and provides deterministic guidance for the `handle_usage_limits` workflow.

---

## 2. OpenAI/Codex Device Auth Flow

### 2.1 Normal Flow Sequence

```
1. Codex CLI: "Usage limit reached, try again at <reset_time>"
2. ft detects: rule_id = "codex.usage.reached"
3. Workflow: start `cod login --device-auth`
4. Codex CLI outputs: "Please open https://auth.openai.com/codex/device and enter code: XXXX-XXXXX"
5. ft detects: rule_id = "codex.auth.device_code"
6. Browser automation: Navigate to auth.openai.com/codex/device
7. Browser state determines outcome (see matrix below)
8. On success: Codex CLI proceeds, ft resumes session
```

### 2.2 Key URLs

| URL | Purpose |
|-----|---------|
| `https://auth.openai.com/codex/device` | Device code entry page |
| `https://auth.openai.com/authorize` | OAuth authorization page |
| `https://auth.openai.com/u/login` | Login page (email/password) |
| `https://auth.openai.com/u/mfa` | MFA verification page |

---

## 3. Auth States Matrix

### 3.1 State Detection Signals

| State Name | Detection Signals | URL Patterns | DOM Selectors |
|------------|-------------------|--------------|---------------|
| **Already Authenticated** | Session cookies valid, no login prompts | Redirect to `/codex/device` directly | `input[name='user_code']` visible without login |
| **Device Code Entry** | User code input field present | `/codex/device` | `input[name='user_code']`, `button[type='submit']` |
| **Email Entry** | Email input field visible | `/u/login/identifier` | `input[name='email']`, `input[type='email']` |
| **Password Required** | Password input field visible | `/u/login/password` | `input[type='password']`, `input[name='password']` |
| **MFA Required** | OTP/TOTP input visible | `/u/mfa-*` | `input[name='code']`, `input[inputmode='numeric']` |
| **SSO Redirect** | Redirect to enterprise IdP | Non-OpenAI domain | `button[data-sso]`, redirect to `*.okta.com`, `login.microsoftonline.com`, etc. |
| **Captcha Challenge** | reCAPTCHA or similar | Any | `iframe[src*='recaptcha']`, `.g-recaptcha`, `#captcha` |
| **Rate Limited** | Error message about too many attempts | Any | Text: "too many requests", "try again later" |
| **Authorization Consent** | OAuth consent screen | `/authorize` | `button[name='consent']`, text: "Authorize", "Allow" |
| **Success** | "Device connected" or similar success message | `/codex/device/success` | Text: "connected", "authorized", "success" |
| **Error/Invalid Code** | Error message about invalid/expired code | `/codex/device` | Text: "invalid", "expired", "incorrect" |

### 3.2 Automation Outcome Matrix

| State | Outcome | Automation Steps | Human Requirement |
|-------|---------|------------------|-------------------|
| **Already Authenticated** | `Automated` | 1. Enter device code<br>2. Click authorize<br>3. Verify success | None |
| **Device Code Entry** | `Automated` | 1. Fill user_code field<br>2. Submit form | None |
| **Email Entry** | `Automated` | 1. Fill email from account profile<br>2. Submit | None |
| **Password Required** | `NeedsHuman` | 1. Open browser for human<br>2. Wait for completion | Enter password manually |
| **MFA Required** | `NeedsHuman` | 1. Open browser for human<br>2. Wait for OTP entry | Enter MFA code manually |
| **SSO Redirect** | `NeedsHuman` | 1. Open browser for human<br>2. Wait for SSO completion | Complete SSO flow manually |
| **Captcha Challenge** | `NeedsHuman` | 1. Open browser for human<br>2. Wait for captcha solve | Solve captcha manually |
| **Rate Limited** | `Fail` | 1. Log failure<br>2. Wait and retry later | Wait for rate limit reset |
| **Authorization Consent** | `Automated` | 1. Click "Authorize" button<br>2. Verify redirect | None |
| **Success** | `Automated` | 1. Return success to workflow | None |
| **Error/Invalid Code** | `Fail` | 1. Log error<br>2. Retry with new code | None (automatic retry) |

---

## 4. Outcome Taxonomy

### 4.1 Outcome Types

```rust
/// Result of an auth automation attempt
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthOutcome {
    /// Auth completed automatically without human intervention
    Automated,
    /// Auth requires human intervention to complete
    NeedsHuman,
    /// Auth failed and cannot proceed (retry or escalate)
    Fail,
}
```

### 4.2 Detailed Outcome States

```rust
/// Detailed auth result with context
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResult {
    /// High-level outcome
    pub outcome: AuthOutcome,
    /// Specific state that was reached
    pub state: AuthState,
    /// Human-readable message for operators
    pub message: String,
    /// Recommended next action
    pub next_action: NextAction,
    /// Time spent on auth attempt (for diagnostics)
    pub elapsed_ms: u64,
}

/// Specific auth states
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthState {
    AlreadyAuthenticated,
    DeviceCodeEntry,
    EmailEntry,
    PasswordRequired,
    MfaRequired,
    SsoRedirect,
    CaptchaChallenge,
    RateLimited,
    AuthorizationConsent,
    Success,
    InvalidCode,
    Timeout,
    NetworkError,
}

/// Recommended actions for workflow fallback
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextAction {
    /// Auth succeeded, continue workflow
    Continue,
    /// Open browser for human to complete
    OpenBrowserForHuman { url: String },
    /// Retry with backoff
    RetryAfter { delay_seconds: u32 },
    /// Switch to a different account
    TryAlternateAccount,
    /// Pause and wait for external signal
    PauseAndWait { reason: String },
    /// Abort workflow with error
    Abort { reason: String },
}
```

### 4.3 Workflow Integration

The `handle_usage_limits` workflow should:

1. **On `Automated` outcome**: Log success, continue to session resume
2. **On `NeedsHuman` outcome**:
   - Emit event: `codex.auth.needs_human`
   - Persist next-step plan in DB
   - Open browser (non-headless) for human
   - Wait for success signal or timeout
   - Message format:
     ```
     [ft] Auth requires human intervention

     State: password_required
     Account: user@example.com
     Action: Complete login in the opened browser window

     Once complete, ft will automatically detect success and resume.
     Timeout: 5 minutes
     ```
3. **On `Fail` outcome**:
   - Log failure with context
   - Determine retry eligibility
   - If retryable: schedule retry with backoff
   - If not retryable: pause workflow, emit actionable event

---

## 5. Profile Persistence Strategy

### 5.1 Profile Directory Structure

```
~/.local/share/ft/browser_profiles/
└── openai/
    └── <account_name>/
        ├── Default/
        │   ├── Cookies
        │   ├── Local Storage/
        │   └── ...
        └── profile.json  # Metadata (created_at, last_used, etc.)
```

Legacy path (older versions): `~/.local/share/wa/browser_profiles/`.

### 5.2 Profile Lifecycle

| Event | Action |
|-------|--------|
| First auth for account | Create profile directory, persist after success |
| Subsequent auth | Load existing profile (session cookies) |
| Auth failure with profile | Mark profile stale, retry fresh |
| Profile > 30 days old | Consider refresh on next use |

### 5.3 Bootstrap Flow

For new accounts that require password/MFA:

1. Run auth in non-headless mode
2. Human completes password/MFA
3. Profile persisted on success
4. Future auths use persisted session

---

## 6. Redaction Rules

### 6.1 Secrets to NEVER Log/Store

| Secret Type | Example Pattern | Redaction |
|-------------|-----------------|-----------|
| Device Code | `XXXX-XXXXX`, `ABCD-12345` | Replace with `[DEVICE_CODE]` |
| Auth Tokens | `sk-...`, `Bearer ...` | Replace with `[AUTH_TOKEN]` |
| Session IDs | `sess_...`, `sid=...` | Replace with `[SESSION_ID]` |
| Passwords | `password=...` | Never capture, replace with `[PASSWORD]` |
| OTP/MFA Codes | 6-digit codes | Replace with `[MFA_CODE]` |
| Cookie Values | `__Secure-...=...` | Replace with `[COOKIE_VALUE]` |

### 6.2 Redaction Implementation

```rust
/// Patterns that must be redacted in all auth-related logging
pub static REDACTION_PATTERNS: &[(&str, &str)] = &[
    // Device codes (XXXX-XXXXX format)
    (r"[A-Z0-9]{4}-[A-Z0-9]{5}", "[DEVICE_CODE]"),
    // OpenAI tokens
    (r"sk-[a-zA-Z0-9]{48}", "[AUTH_TOKEN]"),
    // Bearer tokens
    (r"Bearer\s+[a-zA-Z0-9\-_\.]+", "Bearer [TOKEN]"),
    // Session IDs
    (r"sess_[a-zA-Z0-9]+", "[SESSION_ID]"),
    // Cookie values (conservative)
    (r"__Secure-[^=]+=\S+", "__Secure-*=[REDACTED]"),
    // MFA codes (6 digits surrounded by whitespace)
    (r"\b\d{6}\b", "[MFA_CODE]"),
];

/// Redact sensitive data from text before logging
pub fn redact_auth_secrets(text: &str) -> String {
    let mut result = text.to_string();
    for (pattern, replacement) in REDACTION_PATTERNS {
        let re = Regex::new(pattern).unwrap();
        result = re.replace_all(&result, *replacement).to_string();
    }
    result
}
```

### 6.3 Artifact Redaction Checklist

Before persisting or logging auth-related artifacts:

- [ ] Device codes redacted from terminal output captures
- [ ] Auth tokens redacted from browser network logs
- [ ] Session cookies redacted from profile metadata
- [ ] MFA codes redacted from any captured input
- [ ] URL query parameters with tokens redacted
- [ ] Browser console output redacted

---

## 7. Safe Retry Guidance

### 7.1 Retry Strategy by State

| State | Retry? | Delay | Max Attempts |
|-------|--------|-------|--------------|
| Network Error | Yes | Exponential (1s, 2s, 4s, 8s) | 5 |
| Rate Limited | Yes | Wait for reset time + 1 minute | 3 |
| Invalid Code | Yes (get new code) | Immediate | 3 |
| Timeout | Yes | Linear (30s) | 3 |
| Password Required | No | N/A | 0 (escalate to human) |
| MFA Required | No | N/A | 0 (escalate to human) |
| SSO Redirect | No | N/A | 0 (escalate to human) |

### 7.2 Cooldown Between Accounts

When switching accounts due to auth failure:
- Minimum delay: 5 seconds
- Purpose: Avoid rate limit triggers
- Log: "Switching from account A to account B after auth state: X"

---

## 8. Testing/Validation Notes

### 8.1 Manual Test Scenarios

| Scenario | Setup | Expected Outcome |
|----------|-------|------------------|
| Fresh profile, already logged in browser | Pre-login via browser, then run auth | `Automated` |
| Fresh profile, never logged in | Clear all OpenAI cookies | `NeedsHuman` (password) |
| Existing profile with valid session | Use persisted profile | `Automated` |
| Existing profile with expired session | Wait for session expiry | `NeedsHuman` or `Automated` (depends on SSO) |
| Enterprise SSO account | Use SSO-enabled org account | `NeedsHuman` (SSO) |
| Invalid/expired device code | Wait >15 minutes after code generation | `Fail` (retry with new code) |

### 8.2 Redaction Verification

After each test run, verify:
1. Grep logs for device code patterns - should find only `[DEVICE_CODE]`
2. Grep artifacts for `sk-` patterns - should find only `[AUTH_TOKEN]`
3. Check browser profile metadata - no raw cookie values

---

## 9. Acceptance Criteria

- [x] Auth states matrix covers all observed OpenAI/Codex auth scenarios
- [x] Detection signals documented with URL patterns and DOM selectors
- [x] Outcome taxonomy defined with Rust types
- [x] Redaction rules explicit and implementable
- [x] Retry guidance provided for each state
- [x] Profile persistence strategy documented
- [x] Testing scenarios defined for validation

---

## 10. References

- PLAN.md Section 9: Browser Automation Layer
- PLAN.md Section 22: Open Questions (#3)
- Task: wa-nu4.1.4.5
- Related: wa-nu4.1.4.2 (Implement device auth flow)
- Related: wa-nu4.1.4.4 (Browser smoke-test)
