# wa-oegrb.7.6 Security/Privacy Validation Suite

This document describes the validation slice implemented for `wa-oegrb.7.6`.

## Scope

- Synthetic sensitive corpus checks (secrets + credential-like PII fields)
- Authorization checks for recorder access tiers (`A1`, `A3`) by actor type
- Audit integrity verification via tamper-evident hash chain checks
- Leak-regression alarms that fail when sensitive text appears in audit payloads

## Test Module

- `crates/frankenterm-core/tests/recorder_security_privacy_validation.rs`

## What Is Covered

1. `synthetic_sensitive_corpus_redacts_expected_patterns`
- Exercises OpenAI/Anthropic/GitHub/AWS/database/password-shaped payloads.
- Requires `Redactor` detection + replacement and asserts secret fragments are removed.

2. `authorization_redaction_and_audit_integrity_for_sensitive_access`
- Verifies:
  - Robot `A1` recorder query is allowed.
  - Robot `A3` privileged query is denied.
  - Human `A3` path requires elevation/approval and justification.
- Asserts audit stats, hash-chain integrity, and no leak regressions in persisted query/details fields.

3. `leak_regression_alarm_and_hash_chain_tamper_detection`
- Starts from clean redacted audit entries.
- Simulates tampering by injecting unredacted query text.
- Requires both:
  - leak-regression detector to trigger
  - hash-chain verification to fail

## Run

```bash
cargo test -p frankenterm-core --test recorder_security_privacy_validation -- --nocapture
```

## Notes

- The leak-regression helper intentionally checks for both `Redactor` secrets and basic PII-like markers (email/SSN/phone strings) in audit query/details fields.
- This suite is focused on validation behavior and regression detection; it does not change runtime policy rules by itself.
