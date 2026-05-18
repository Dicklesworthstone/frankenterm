# Provider Quota Assignment Contract

**Bead:** `ft-auy2g.8`
**Status:** static fixture contract only. No provider API calls, credential
mutation, account rotation, or live spend decision are implemented by this
document.

## Purpose

Large FrankenTerm swarms need assignment decisions that respect provider quota,
reset windows, observed rate limits, and cost class. This contract defines a
read-only mission-planner evidence artifact, `ft.provider_quota_assignment.v1`,
for explaining whether a task should be assigned, deferred, downgraded to a
lower model class, sent for approval, or blocked until fresh quota evidence is
available.

The JSON Schema lives at
`docs/json-schema/ft-provider-quota-assignment.json`. The reviewed fixture
manifest lives at `fixtures/mission-planner/provider-quota-assignment/cases.v1.json`.

## Contract Shape

Every assignment artifact records:

- the mission objective context: urgency, proof criticality, expected token
  class, expected cost class, required model class, and the related mission
  objective-plan contract;
- one or more provider evidence rows with provider, model class, account class,
  quota remaining, reset window, observed rate-limit state, marginal cost
  class, confidence, freshness, source artifact, redaction state, and provider
  availability;
- one operator-safe recommendation: `assign`, `defer`, `degrade_model_class`,
  `require_approval`, or `request_fresh_quota_evidence`;
- stable reason codes for both happy and fail-closed paths;
- forbidden action classes; and
- a compact `toon_projection` table with deterministic columns and rows for
  agent-to-agent consumption.

The planner is advisory. It may explain a safe assignment, but it never mutates
credentials, rotates accounts, calls providers, starts spend, repairs services,
or treats local Cargo as proof.

## Required Fail-Closed Semantics

The verifier pins these cases:

| Case | Expected recommendation | Required reason |
| --- | --- | --- |
| `healthy-quota` | `assign` | `quota.healthy` |
| `near-reset-degrade` | `degrade_model_class` | `quota.near_reset` |
| `hard-rate-limit` | `defer` | `quota.hard_rate_limit` |
| `unknown-quota` | `request_fresh_quota_evidence` | `quota.unknown` |
| `conflicting-account-evidence` | `request_fresh_quota_evidence` | `quota.conflicting_evidence` |
| `high-cost-requires-approval` | `require_approval` | `quota.high_cost_requires_approval` |
| `stale-evidence` | `request_fresh_quota_evidence` | `quota.stale_evidence` |
| `privacy-redacted-evidence` | `request_fresh_quota_evidence` | `quota.privacy_redacted` |
| `provider-unavailable` | `request_fresh_quota_evidence` | `quota.provider_unavailable` |

Missing, stale, contradictory, privacy-redacted, or provider-unavailable quota
evidence must not produce `assign`. The safe fallback is to defer, request fresh
evidence, degrade the model class, or require explicit approval.

## Safety Invariants

1. `dry_run` and `read_only` are always `true`.
2. `forbidden_actions` always includes provider API calls, credential mutation,
   account rotation, hidden spend decisions, service mutation, local Cargo proof,
   and raw secret storage.
3. `assign` is allowed only with fresh, high-confidence, redacted-summary
   evidence from an available provider.
4. `require_approval` is mandatory for high-cost frontier work unless a later
   approval-gated bead implements a stricter policy.
5. `request_fresh_quota_evidence` is mandatory when quota evidence is unknown,
   stale, contradictory, privacy-redacted, or provider-unavailable.

The exact forbidden action IDs are `provider_api_call`,
`credential_mutation`, `account_rotation`, `hidden_spend_decision`,
`service_mutation`, `local_cargo_proof`, and `raw_secret_storage`.

## Verification

Run the static verifier:

```bash
bash tests/e2e/test_provider_quota_assignment_contract.sh
```

The verifier checks schema metadata, fixture coverage, reason-code coverage,
forbidden actions, TOON-ready projections, fail-closed recommendations, and a
secret-shaped string scan. It uses only shell, `jq`, and Ruby.

Any future Rust implementation or compiled conformance proof must run through
RCH. Local Cargo output is not accepted as proof for this surface.

## Negative Fixtures

The retained negative fragment corpus lives at
`fixtures/mission-planner/provider-quota-assignment/invalid/fragments.v1.json`.
It is parseable JSON that the static verifier must reject by contract shape
rather than by syntax. The required cases are:

- `provider-api-call-permitted`
- `credential-mutation-permitted`
- `hidden-spend-decision-permitted`
- `assign-with-stale-evidence`
- `raw-secret-storage-permitted`
- `toon-row-width-mismatch`

These fragments prove that the provider quota assignment contract stays
fail-closed for provider calls, credential mutation, hidden spend decisions,
assignment from stale evidence, raw secret storage, and malformed TOON
projections.
