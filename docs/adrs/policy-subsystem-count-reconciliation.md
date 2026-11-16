# Policy Subsystem Count Reconciliation

Status: accepted
Date: 2026-05-09
Bead: `ft-5eqd4.2`

## Context

`README.md` currently describes the Policy Engine as a "21-subsystem policy framework". The audit in `docs/adrs/policy-subsystem-count-audit.md` found that the wording was introduced by README-only commit `cc166c971`, while the runtime diagnostics surface had already been implemented as fourteen checks by `0f4f4cebf`.

Current `policy_diagnostics::check_policy_engine_health` enumerates fourteen operator-visible health checks. The audit was able to account for all twenty-one historical policy/control surfaces: fourteen remain `policy_diagnostics` checks, and seven are live policy gates or peer domains that do not currently produce independent per-subsystem health verdicts.

The parent epic defines the decision rule: a user-facing "subsystem" count should mean something an operator can see as a per-domain verdict in `ft doctor` / policy health output.

## Decision

Choose strategy (A): the operationally-correct PolicyEngine subsystem count is fourteen.

Follow-on reconciliation work should update user-facing claims from "21-subsystem policy framework" to a fourteen-check operator-visible diagnostics claim. The seven additional historical rows should not be presented as PolicyEngine health subsystems unless they are first given real `policy_diagnostics` health checks.

The single source of truth for the count should be a constant in `policy_diagnostics.rs`, named `POLICY_SUBSYSTEM_COUNT`, with value `14`. `check_policy_engine_health()` should enumerate exactly that many `RuntimeHealthCheck` values, and the README regression guard should pin its headline claim to the constant.

## Decision Rules

This keeps the user-facing meaning of "subsystem" intact. Operators reading README expect a runtime health surface, not an archaeology count of policy fields, configuration knobs, and decision-path gates.

This preserves the marketing intent without inflating the number. The corrected claim still says the Policy Engine has broad capability gates, rate limiting, audit trails, approval tokens, connector checks, namespace isolation, and ingestion health. It no longer implies that `ft doctor` reports twenty-one policy subsystem verdicts.

This tees up the regression test cleanly. A single `POLICY_SUBSYSTEM_COUNT: usize = 14` constant can be asserted against both the diagnostics vector length and README text. Strategy (B) would require inventing or delegating seven additional health probes before the count could be pinned honestly.

## Consequences

Subtask `ft-5eqd4.3` should update README's three live "21" claims and the stale unified telemetry comment to reflect the fourteen-check diagnostics surface. The audit did not find a live `AGENTS.md` "21" claim, so AGENTS.md only needs editing if a fresh grep in that subtask finds one.

Subtask `ft-5eqd4.3` should add `POLICY_SUBSYSTEM_COUNT` near `check_policy_engine_health` and assert, at minimum in debug/test code, that the returned vector length matches the constant. This is not a behavior change to policy enforcement or `ft doctor`; it makes the existing runtime enumeration explicit.

Subtask `ft-5eqd4.4` should add the deterministic regression guard that checks README's policy subsystem headline against `POLICY_SUBSYSTEM_COUNT` and verifies `check_policy_engine_health()` returns exactly that count.

Any future desire to market the broader twenty-one historical policy/control surfaces should use separate wording, such as "policy control surfaces", and should link to the audit matrix rather than overloading the operator-visible health-check count.

## Alternative Considered

Strategy (B) would preserve the "21" headline by adding seven more entries to `check_policy_engine_health` or by explaining those rows as peer domains. The audit did not find seven existing operator-visible policy health checks to delegate to. Some candidates are inline decision gates (`policy.rate_limit`, `policy.command_gate`, `policy.trauma_guard`, policy rules); others are peer modules with their own telemetry or health shape (`connector_data_classification`, `connector_host_runtime`); the credential broker is currently aggregated through metrics/telemetry rather than a diagnostics verdict.

Reject strategy (B) for now. It would either create new runtime health semantics as part of a docs correction, or it would keep a misleading README number by counting things operators cannot inspect as peer policy health verdicts. If later product work adds real health checks for those seven surfaces, the source-of-truth constant and README can be raised in that feature's own bead with proof.
