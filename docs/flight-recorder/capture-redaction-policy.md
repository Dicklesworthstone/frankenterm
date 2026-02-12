# Capture-Stage Redaction Policy (Recorder v1)

Date: 2026-02-12  
Bead: `wa-oegrb.2.5`  
Status: Accepted policy baseline for implementation tracks

## Purpose

Define deterministic, testable redaction behavior for recorder capture so sensitive material is protected without breaking replay/search utility.

This document is the implementation contract for:
- `wa-oegrb.6.5` (policy-aware access + redaction in interfaces)
- `wa-oegrb.7.6` (security/privacy validation suite)
- `wa-oegrb.8.3` (governance policy for privacy and privileged access)

## Policy Outcomes

1. Secrets must be redacted before durable recorder persistence by default.
2. Redaction must be deterministic for identical input + policy version.
3. Every redaction decision must be auditable (what was redacted, by which policy version, at which stage).
4. Lexical and hybrid search must operate on redacted text by default.
5. Raw (unredacted) visibility, if allowed at all, is a privileged and explicitly gated mode.

## Threat Model

In-scope leaks:
- API keys, auth tokens, OAuth/device codes, passwords, DB credentials.
- Secrets inside ingress text and egress output.
- Secrets leaked through index projections or query responses.

Out-of-scope for v1:
- Perfect PII/entity recognition across arbitrary natural language.
- Remote attestation of client-side redaction behavior.

## Deterministic Boundary Contract

Redaction stages are mandatory and ordered:

1. Capture-stage (pre-persist): required default path
   - Owner: capture producers in ingress/egress tap paths.
   - Effect: redact payload text before enqueue/write to canonical recorder log.
2. Projection-stage (pre-index): defense-in-depth
   - Owner: lexical/semantic indexer pipeline.
   - Effect: enforce policy for any non-canonical projection fields and metadata.
3. Response-stage (pre-return): defense-in-depth + role-aware shaping
   - Owner: CLI/Robot/MCP query surfaces.
   - Effect: ensure user-visible output obeys viewer policy and role restrictions.

No downstream stage may "unredact" content that was redacted upstream.

## Canonical Redaction Classes

Policy version: `redaction.policy.v1`

Classes:
- `secret.credential`: hard credentials (API keys, bearer tokens, passwords, DB URL creds).
- `secret.auth_flow`: auth artifacts (device codes, OAuth query tokens/codes).
- `secret.session`: session-like tokens if matched by configured secret patterns.

Class behavior in v1:
- All classes use replacement marker `[REDACTED]`.
- Replacement is non-reversible.
- Matching is regex-based using `policy::Redactor` patterns as baseline.

## Recorder Event Contract Requirements

For text-bearing events (`ingress_text`, `egress_output`):
- `redaction` field semantics:
  - `none`: no match and no redaction applied.
  - `partial`: text transformed and still carries useful context.
  - `full`: payload replaced entirely because it is high-risk or near-empty after scrub.

Required `details.redaction_meta` object (new requirement for implementation):
- `policy_version` (string; required)
- `applied` (bool; required)
- `detectors` (array[string]; required, empty if none)
- `match_count` (u32; required)
- `mode` (`none|partial|full`; required)
- `input_len` (u32; required)
- `output_len` (u32; required)

Gap/control/lifecycle events:
- Must include `details.redaction_meta.applied=false` and `mode=none`.

## Implementation Ownership by Code Surface

Ingress tap (`crates/frankenterm-core/src/policy.rs`):
- Current state: ingress tap emits `redaction=partial` with redacted summary text.
- Required: compute explicit `redaction_meta` and deterministic mode assignment.

Egress tap (`crates/frankenterm-core/src/tailer.rs`):
- Current state: tap emits raw `segment.content` with `redaction=none`.
- Required: apply capture-stage redaction before `on_egress`, set mode + metadata, and preserve gap semantics.

Recorder manager (`crates/frankenterm-core/src/recording.rs`):
- Current state: has optional redaction for segments/events.
- Required: treat redaction as mandatory default for canonical recorder path; optional bypass must be explicit, audited, and disabled by default.

Interface layer (`crates/frankenterm/src/main.rs` and robot/MCP surfaces):
- Current state: output redaction exists in several report/export paths.
- Required: enforce response-stage redaction policy for recorder query/replay endpoints and privileged raw-access checks.

## Deterministic Processing Rules

Given `(text, policy_version, detector_set)`:
1. Run detector set in fixed order.
2. Collect all matches.
3. Resolve overlaps by longest-match-first, then earliest-start, then detector order.
4. Apply replacements in a single pass over original offsets.
5. Emit metadata from final transformed string.

Mode assignment:
- `none`: no replacements.
- `partial`: 1+ replacements and output has non-marker content.
- `full`: output is empty/marker-only after scrub or policy forces full wipe.

## Configuration Requirements

Required config keys (new):
- `recorder.redaction.enabled` (bool, default `true`)
- `recorder.redaction.policy_version` (string, default `redaction.policy.v1`)
- `recorder.redaction.allow_unredacted_capture` (bool, default `false`)
- `recorder.redaction.custom_patterns` (list, optional, append-only extension)

Safety constraints:
- If `allow_unredacted_capture=true`, process must log startup warning and emit audit event.
- Non-default unredacted mode cannot be enabled silently.

## Auditability Requirements

Audit events must record:
- stage (`capture|projection|response`)
- policy_version
- detector names
- match_count
- mode
- actor context (for response-stage decisions)

Audit logs must never include raw matched secret values.

## Test Matrix (Required for Bead Closure Downstream)

Unit:
- deterministic output for fixed inputs across runs.
- overlap resolution stability.
- mode assignment (`none|partial|full`) correctness.

Property/fuzz:
- idempotence: redacting already-redacted text does not leak original values.
- no panics on arbitrary UTF-8 inputs.

Integration:
- ingress path emits redacted payload + metadata.
- egress path emits redacted payload + metadata, including gaps.
- recorder persistence never stores known secret fixture values in default mode.

Golden corpus:
- true-positive secret fixtures redact correctly.
- false-positive guard fixtures remain unchanged.
- regression fixtures for provider-specific keys and OAuth/device flows.

Performance:
- redaction adds bounded overhead under swarm load; measured in capture hot path.
- overflow/backpressure behavior remains explicit and unaffected.

## Rollout and Compatibility

Rollout order:
1. Add metadata fields and dual-read compatibility.
2. Enable capture-stage redaction metadata generation.
3. Enforce projection/response stage checks.
4. Add governance gates and CI validation.

Compatibility rule:
- readers must treat absent `details.redaction_meta` as legacy and degrade gracefully until migration window closes.

## Non-Goals

- Do not attempt semantic de-identification in v1.
- Do not build reversible masking in canonical recorder data.
- Do not couple redaction correctness to external network services.

## Exit Criteria for `wa-oegrb.2.5`

1. This policy is accepted as the baseline contract.
2. Downstream beads have enough detail to implement without rediscovering boundaries.
3. Required metadata + tests are explicitly defined and traceable.
