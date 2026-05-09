# Policy Subsystem Count Audit

Status: complete for `ft-5eqd4.1`
Date: 2026-05-09
Scope: archaeology and reconciliation input only. This document does not change README, AGENTS.md, or policy code.

## Finding

The current README claim that the Policy Engine is a "21-subsystem policy framework" is not backed by a 21-row runtime diagnostics surface.

The claim was introduced by `cc166c971` (`docs: rewrite README with current architecture, test metrics, and feature catalog`). That commit changed only `README.md`, so it did not add or modify policy diagnostics. The diagnostics implementation had already been introduced by `0f4f4cebf` on 2026-03-11; that commit message explicitly described `policy_diagnostics.rs` as "14 runtime health checks for all PolicyEngine subsystems". The current source matches that: `check_policy_engine_health` returns exactly the fourteen checks listed in `crates/frankenterm-core/src/policy_diagnostics.rs:37`.

The most defensible reconstruction of the original "21" is the set of PolicyEngine-owned runtime/control surfaces present around the README-origin commit, excluding fields that were only configuration scalars:

- Excluded as config/scalar fields: `require_prompt_active`, `risk_config`, `credential_broker_config`, and `namespace_isolation_enabled`.
- Counted as policy subsystems/control surfaces: the fourteen current diagnostics rows plus seven policy gates or peer policy domains that are still real code paths, but are not per-subsystem `policy_diagnostics` health checks.

No row below is unidentified. No row is marked `deleted-by`, so no deletion bead citation is required.

## Evidence

- README origin: `cc166c971` is README-only and introduced the "21-subsystem" wording.
- Diagnostics origin: `0f4f4cebf` added `policy_diagnostics.rs` with fourteen checks.
- Current diagnostics list: `crates/frankenterm-core/src/policy_diagnostics.rs:11` through `crates/frankenterm-core/src/policy_diagnostics.rs:24`.
- Current diagnostics constructor: `crates/frankenterm-core/src/policy_diagnostics.rs:37` through `crates/frankenterm-core/src/policy_diagnostics.rs:56`.
- Current PolicyEngine field layout: `crates/frankenterm-core/src/policy.rs:4406` through `crates/frankenterm-core/src/policy.rs:4460`.
- Current telemetry aggregate is fifteen fields plus a namespace flag, not twenty-one independent health checks: `crates/frankenterm-core/src/policy.rs:4361` through `crates/frankenterm-core/src/policy.rs:4399`.
- The unified telemetry comment still says "all 21 subsystems" while wrapping the aggregate policy payload: `crates/frankenterm-core/src/unified_telemetry.rs:242`.

## Reconciliation Matrix

| Original subsystem name | Original location | Current location | Check function or extracted module | Disposition |
|---|---|---|---|---|
| Rate limiter | `cc166c971:crates/frankenterm-core/src/policy.rs:3425` | `crates/frankenterm-core/src/policy.rs:4411`; `crates/frankenterm-core/src/policy.rs:6340` | No `policy_diagnostics` check; live decision rule `policy.rate_limit` records verdicts in `authorize` | `moved-to:policy.authorize.rate_limit` |
| Command safety gate | `cc166c971:crates/frankenterm-core/src/policy.rs:3429` | `crates/frankenterm-core/src/policy.rs:4415`; `crates/frankenterm-core/src/policy.rs:6645` | No `policy_diagnostics` check; live decision rule `policy.command_gate` | `moved-to:policy.authorize.command_gate` |
| Trauma guard | `cc166c971:crates/frankenterm-core/src/policy.rs:3431` | `crates/frankenterm-core/src/policy.rs:4417`; `crates/frankenterm-core/src/policy.rs:6571` | No `policy_diagnostics` check; live decision rule `policy.trauma_guard` / `policy.trauma_guard.loop_block` | `moved-to:policy.authorize.trauma_guard` |
| Policy rules / DSL | `cc166c971:crates/frankenterm-core/src/policy.rs:3433` | `crates/frankenterm-core/src/policy.rs:4419`; `crates/frankenterm-core/src/policy.rs:6754` | No `policy_diagnostics` check; evaluator is `evaluate_policy_rules` at `crates/frankenterm-core/src/policy.rs:3934`; `policy_dsl` stays in core at `crates/frankenterm-core/src/lib.rs:424` | `moved-to:policy.authorize.policy_rules` |
| Decision log | `cc166c971:crates/frankenterm-core/src/policy.rs:3437` | `crates/frankenterm-core/src/policy.rs:4423` | `check_decision_log` at `crates/frankenterm-core/src/policy_diagnostics.rs:63` | `kept-as-is` |
| Quarantine registry | `cc166c971:crates/frankenterm-core/src/policy.rs:3439` | `crates/frankenterm-core/src/policy.rs:4425`; types extracted in `crates/frankenterm-core-policy-types/src/lib.rs:11` | `check_quarantine` at `crates/frankenterm-core/src/policy_diagnostics.rs:113` | `kept-as-is` |
| Audit chain | `cc166c971:crates/frankenterm-core/src/policy.rs:3441` | `crates/frankenterm-core/src/policy.rs:4427`; types extracted in `crates/frankenterm-core-policy-types/src/lib.rs:6` | `check_audit_chain` at `crates/frankenterm-core/src/policy_diagnostics.rs:167` | `kept-as-is` |
| Compliance engine | `cc166c971:crates/frankenterm-core/src/policy.rs:3443` | `crates/frankenterm-core/src/policy.rs:4429`; types extracted in `crates/frankenterm-core-policy-types/src/lib.rs:8` | `check_compliance` at `crates/frankenterm-core/src/policy_diagnostics.rs:206` | `kept-as-is` |
| Credential broker | `cc166c971:crates/frankenterm-core/src/policy.rs:3445` | `crates/frankenterm-core/src/policy.rs:4431`; `crates/frankenterm-core/src/policy.rs:6271`; `crates/frankenterm-core/src/policy.rs:5460` | No `policy_diagnostics` check; telemetry is aggregated through `PolicyEngine::metrics_dashboard` and `PolicyEngine::telemetry_snapshot` | `moved-to:policy.metrics_dashboard.credential_broker` |
| Connector lifecycle manager | `cc166c971:crates/frankenterm-core/src/policy.rs:3449` | `crates/frankenterm-core/src/policy.rs:4435` | `check_connector_lifecycle` at `crates/frankenterm-core/src/policy_diagnostics.rs:445` | `kept-as-is` |
| Connector data classifier | `cc166c971:crates/frankenterm-core/src/policy.rs:3451` | `crates/frankenterm-core/src/policy.rs:4437`; `crates/frankenterm-core/src/connector_data_classification.rs:646`; `crates/frankenterm-core/src/connector_data_classification.rs:1117` | No `policy_diagnostics` check; the peer domain exposes classification telemetry directly | `moved-to:connector_data_classification` |
| Connector governor | `cc166c971:crates/frankenterm-core/src/policy.rs:3453` | `crates/frankenterm-core/src/policy.rs:4439` | `check_connector_governor` at `crates/frankenterm-core/src/policy_diagnostics.rs:368` | `kept-as-is` |
| Connector registry | `cc166c971:crates/frankenterm-core/src/policy.rs:3455` | `crates/frankenterm-core/src/policy.rs:4441` | `check_connector_registry` at `crates/frankenterm-core/src/policy_diagnostics.rs:409` | `kept-as-is` |
| Connector host runtime | `cc166c971:crates/frankenterm-core/src/policy.rs:3457` | `crates/frankenterm-core/src/policy.rs:4443`; `crates/frankenterm-core/src/connector_host_runtime.rs:619`; `crates/frankenterm-core/src/connector_host_runtime.rs:1004` | No `policy_diagnostics` check; peer runtime exposes `health_snapshot` | `moved-to:connector_host_runtime` |
| Connector reliability registry | `cc166c971:crates/frankenterm-core/src/policy.rs:3459` | `crates/frankenterm-core/src/policy.rs:4445` | `check_connector_reliability` at `crates/frankenterm-core/src/policy_diagnostics.rs:488` | `kept-as-is` |
| Bundle registry | `cc166c971:crates/frankenterm-core/src/policy.rs:3461` | `crates/frankenterm-core/src/policy.rs:4447` | `check_bundles` at `crates/frankenterm-core/src/policy_diagnostics.rs:565` | `kept-as-is` |
| Connector mesh | `cc166c971:crates/frankenterm-core/src/policy.rs:3463` | `crates/frankenterm-core/src/policy.rs:4449` | `check_connector_mesh` at `crates/frankenterm-core/src/policy_diagnostics.rs:530` | `kept-as-is` |
| Ingestion pipeline | `cc166c971:crates/frankenterm-core/src/policy.rs:3465` | `crates/frankenterm-core/src/policy.rs:4451` | `check_ingestion` at `crates/frankenterm-core/src/policy_diagnostics.rs:588` | `kept-as-is` |
| Namespace registry / namespace isolation | `cc166c971:crates/frankenterm-core/src/policy.rs:3467` | `crates/frankenterm-core/src/policy.rs:4453` | `check_namespace_isolation` at `crates/frankenterm-core/src/policy_diagnostics.rs:330` | `kept-as-is` |
| Approval tracker | `cc166c971:crates/frankenterm-core/src/policy.rs:3471` | `crates/frankenterm-core/src/policy.rs:4457` | `check_approvals` at `crates/frankenterm-core/src/policy_diagnostics.rs:263` | `kept-as-is` |
| Revocation registry | `cc166c971:crates/frankenterm-core/src/policy.rs:3473` | `crates/frankenterm-core/src/policy.rs:4459` | `check_revocations` at `crates/frankenterm-core/src/policy_diagnostics.rs:301` | `kept-as-is` |

## Recommendation

Recommend strategy (A): treat fourteen as the operationally-correct PolicyEngine diagnostics count, and update user-facing claims in the follow-on reconciliation subtask to say that the operator-visible diagnostics surface has fourteen policy health checks. The seven remaining rows are real policy/control surfaces, but they are not `policy_diagnostics::check_policy_engine_health` rows and they do not currently produce independent `ft doctor` / policy health verdicts. Preserving "21" as the headline number would require either adding seven new diagnostics checks or documenting a broader "policy control surfaces" concept separately from the operator-visible health count.
