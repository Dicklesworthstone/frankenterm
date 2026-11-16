# Policy Recommendation Receipts

**Status:** Deterministic dry-run receipt model is live in
`frankenterm-core-replay`.

Policy recommendation receipts answer one question: what should an operator or
agent do next if this candidate action were submitted to policy? They are not
approval records and they are not execution receipts.

The replay/proof surface lives in
`crates/frankenterm-core-replay/src/replay_policy_recommendations.rs` and emits
stable JSON/TOON summaries through checked fixtures under `fixtures/scale-lab/`.

## Contract

Recommendation mode is side-effect free:

- `executes_action` is always `false`.
- `issues_approval_token` is always `false`.
- `approval_preview.token_issued` is always `false`.
- `approval_preview.approval_code_issued` is always `false`.
- sensitive evidence is replaced with `redacted:<field>`.

Outcomes are explicit:

| Outcome | Meaning |
| --- | --- |
| `allow` | Candidate is safe in dry-run recommendation mode. |
| `deny` | Candidate is destructive or otherwise must not proceed. |
| `require_approval` | A separate mutating command must obtain a real approval. |
| `delay` | Resource pressure should clear before proceeding. |
| `degrade` | Agent Mail or RCH health is not trustworthy enough. |
| `ask_human` | Stale ownership or unknown safety needs human judgement. |

`require_approval` receipts may include an `approval_preview`, but the preview
only names the scope and approval class a future mutating path would need. A
live allow-once code is issued only by the dedicated approval flow described in
`docs/approvals.md`.

## Fixtures

The checked summary fixtures are:

- `fixtures/scale-lab/policy-recommendation-summary.v1.json`
- `fixtures/scale-lab/policy-recommendation-summary.v1.toon`

They cover safe reads, destructive command denial, approval-required sends,
resource-pressure delay, degraded Agent Mail/RCH infrastructure, stale ownership
handoff, and redacted evidence fields.
