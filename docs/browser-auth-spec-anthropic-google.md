# Browser Auth Spec: Anthropic + Google (Gemini)

> **Purpose**: Define a stable, testable browser-auth contract for Anthropic/Claude Code and Google/Gemini flows.
>
> **Scope**: Profile-based Playwright automation with safe fallback to human completion.
>
> **Related**: `docs/auth-realities-matrix-openai-codex.md` (OpenAI/Codex device auth matrix)
>
> **Last Updated**: 2026-01-29

---

## 1. Goals

- Provide a **single contract** for multi-service browser auth.
- Keep the contract **stable and versioned**.
- Ensure **safe failure** with explicit Next Steps.
- Persist **profiles only** (no secrets in wa DB).

## 2. Non-Goals (v0.1)

- Storing passwords or OTP secrets.
- Fully headless auth for MFA/SSO paths.
- Arbitrary scripting beyond the defined steps.

## 3. Inputs

### 3.1 Required

- `service`: `anthropic` | `google`
- `account_key`: stable identifier tied to `accounts` table primary key

### 3.2 Optional

- `auth_url`: URL printed by CLI (if present)
- `device_code` / `user_code`: if flow uses explicit code entry

## 4. Outputs

### 4.1 Result Types

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthResult {
    Success {
        persisted_profile: bool,
    },
    NeedsHuman {
        reason: String,
        next_steps: Vec<String>,
        opened_url: Option<String>,
    },
    Failed {
        reason: String,
        retriable: bool,
    },
}
```

### 4.2 Required Output Fields

- **Success**: `persisted_profile` indicates whether storage state was saved.
- **NeedsHuman**: explicit `next_steps` for operators.
- **Failed**: `retriable` must be set deterministically.

## 5. Profile Strategy

- **Profile root**: `~/<data_dir>/wa/browser_profiles/<service>/<account_key>/`
- Profiles are treated as **opaque** by wa.
- If a profile exists, try **already-authenticated fast path** first.
- The only persisted data in wa DB:
  - profile path
  - timestamps
  - last_success_at
  - last_failure_reason (redacted)

## 6. Safety & Redaction

- Never log:
  - passwords
  - OTP/MFA codes
  - session cookies
  - tokenized URLs
- Redact:
  - `auth_url` query params
  - device/user codes

## 7. Reliability Requirements

- **Timeouts**: every wait step has a hard timeout; errors are actionable.
- **Deterministic logs**: only high-level milestones, no secrets.
- **Idempotence**: if already authenticated, return `Success` without mutation.
- **Safe failure**: any ambiguous auth state returns `NeedsHuman`.

## 8. Service-Specific Notes

### 8.1 Anthropic (Claude Code)

- Expected CLI trigger: auth required or usage-limit flow requiring re-auth.
- Primary path: `/login`.
- If password or MFA is required: return `NeedsHuman` and open browser non-headless.

### 8.2 Google (Gemini)

- Expected CLI trigger: `/auth` workflow from Gemini CLI.
- Primary path: Google OAuth flow.
- If SSO/MFA encountered: return `NeedsHuman`.

## 9. Recommended Trigger Points

- **Manual**: `ft status --refresh-auth` (explicit operator request).
- **Workflow**: `handle_usage_limits` for Anthropic/Gemini.
- **Periodic**: optional background refresh with a strict cooldown.

## 10. Testing Requirements

### 10.1 Unit Tests (offline)

- Redaction for URLs/codes.
- Result serialization stability.

### 10.2 Integration Tests (offline)

- Playwright against local HTML fixtures:
  - already-authenticated
  - needs-human (password/MFA)

### 10.3 Manual Smoke Tests

- Run controlled browser auth against real services **without** logging secrets.

## 11. Acceptance Checklist

- [ ] Inputs/outputs are explicit and stable.
- [ ] Profile storage path is documented and deterministic.
- [ ] Failure modes return actionable `NeedsHuman` guidance.
- [ ] Testing plan is sufficient to implement without re-reading PLAN.md.
