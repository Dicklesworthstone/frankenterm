# Auth Realities Matrix: OpenAI + Anthropic + Google

> **Purpose**: One place to answer: "Can wa automate this auth state, or does a human have to step in?"
>
> This extends `docs/auth-realities-matrix-openai-codex.md` with Anthropic and Google, using the
> same outcome taxonomy so workflows can make deterministic decisions.
>
> **Related**
> - OpenAI/Codex: `docs/auth-realities-matrix-openai-codex.md`
> - Anthropic/Google browser auth contract: `docs/browser-auth-spec-anthropic-google.md`
>
> **Last Updated**: 2026-02-06
> **Status**: Draft (needs real-world validation for Anthropic + Google)

---

## Outcome Taxonomy

| Outcome | Meaning |
|---------|---------|
| `Automated` | wa can proceed without a human |
| `NeedsHuman` | wa must open a browser (or otherwise prompt) for a human bootstrap step |
| `Fail` | wa cannot proceed now; retry later or abort |

---

## Cross-Service Auth States Matrix

Notes:
- "Detection signals" should map to either:
  - stable `rule_id`s in `wa-core` (preferred), or
  - stable CLI output substrings/URLs for browser automation.
- "Automated steps" MUST NOT include entering passwords/OTP secrets.

| Service | State | Detection Signals | Outcome | Automated Steps | Human Requirement | Safe Retry Guidance |
|---------|-------|-------------------|---------|-----------------|------------------|---------------------|
| OpenAI (Codex) | Device code prompt | `rule_id=codex.auth.device_code_prompt` and URL like `auth.openai.com/.../device` | `Automated` | Open device URL, enter code, authorize | None | If code invalid/expired: re-run device auth to get a new code |
| OpenAI (Codex) | Already authenticated | Browser shows device-code input without login prompts | `Automated` | Enter code + authorize | None | N/A |
| OpenAI (Codex) | Password required | Browser shows password entry | `NeedsHuman` | Open browser non-headless, wait for completion | Human enters password | After success, persist profile; subsequent runs should be automated |
| OpenAI (Codex) | MFA required | Browser shows OTP/TOTP challenge | `NeedsHuman` | Open browser non-headless, wait for completion | Human enters MFA code | After success, persist profile; subsequent runs should be automated |
| OpenAI (Codex) | Captcha / bot challenge | reCAPTCHA iframe or similar | `NeedsHuman` | Open browser non-headless, wait | Human solves captcha | Backoff; repeated captcha likely indicates automation is flagged |
| OpenAI (Codex) | Rate limited | Browser/CLI indicates too many attempts | `Fail` | Abort attempt | Wait | Retry after the service-imposed cooldown |
| Anthropic (Claude Code) | API key auth error | `rule_id=claude_code.auth.api_key_error` (key missing/invalid) | `NeedsHuman` | Emit actionable Next Steps (set key / re-auth) | Human fixes key/bootstrap | After fix, rerun workflow; do not loop |
| Anthropic (Claude Code) | Login URL / browser required | CLI prints a login URL or prompt to complete auth in browser (TBD: add rule_id) | `NeedsHuman` | Open browser to provided URL, wait for completion | Human completes login/MFA/SSO | If stuck in SSO/MFA: treat as human-only, do not retry automatically |
| Anthropic (Claude Code) | Already authenticated profile | Profile-based run succeeds without browser steps (TBD validation) | `Automated` | Continue | None | N/A |
| Anthropic (Claude Code) | Captcha / bot challenge | Browser shows captcha | `NeedsHuman` | Open browser non-headless, wait | Human solves captcha | Backoff; repeated captcha likely indicates automation is flagged |
| Google (Gemini) | `/auth` prints URL | CLI prints an OAuth URL and instructs user to complete it (TBD: add rule_id) | `NeedsHuman` | Open browser to URL, wait for completion | Human completes Google OAuth | Persist profile; next run should hit "already authenticated" fast-path |
| Google (Gemini) | Already authenticated profile | Profile-based run succeeds without OAuth prompts (TBD validation) | `Automated` | Continue | None | N/A |
| Google (Gemini) | SSO / enterprise IdP | Redirect to non-google IdP (Okta/AAD/etc) | `NeedsHuman` | Open browser non-headless, wait | Human completes SSO | Do not retry automatically; show explicit Next Steps |
| Google (Gemini) | MFA required | Google OTP/TOTP or security key prompt | `NeedsHuman` | Open browser non-headless, wait | Human completes MFA | If security key required, treat as human-only |
| Google (Gemini) | Captcha / suspicious login | "Verify it's you" / captcha | `NeedsHuman` | Open browser non-headless, wait | Human completes challenge | Backoff; repeated challenges may require a clean profile |

---

## Workflow Guidance (NeedsHuman)

When a state is `NeedsHuman`, the workflow output should be consistent:
- State name (snake_case)
- Service + account_key
- What wa did (opened URL, where profile lives)
- Clear next steps
- Clear "how wa will know it's done" (pattern match / CLI success)

---

## Redaction Rules (Auth Artifacts)

Never log/store:
- tokenized URLs (OAuth `code`, `state`, `access_token`, etc.)
- session cookies
- device codes / user codes
- passwords / OTP codes

Use the shared redaction primitives (`wa_core::policy::Redactor`) for:
- CLI output excerpts recorded in incident bundles
- any explain/trace artifacts derived from auth flows

---

## Manual Validation Log (TODO)

This bead requires at least one real-world validation run per service.

- Anthropic: TODO (record observed CLI prompts + URLs without secrets)
- Google: TODO (record observed `/auth` prompts + URLs without secrets)

