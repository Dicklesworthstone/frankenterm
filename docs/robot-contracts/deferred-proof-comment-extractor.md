# Deferred Proof Comment Extractor Contract

`ft.deferred_proof_comment_extraction.v1` is the static contract for the
Beads/comment extractor that feeds `ft.deferred_proof_receipt.v1`. The extractor
turns structured closeout footers and static-proof notes into deterministic
JSONL records. It must not treat vague prose as proof.

The schema is `docs/json-schema/ft-deferred-proof-comment-extraction.json`; the
retained fixture corpus lives under `fixtures/deferred-proof-replay/extractor/`.

## Required Semantics

- Every output line is one deterministic JSON object with a source digest,
  extraction state, reason codes, and either a receipt projection or `null`.
- Source provenance is mandatory: Beads comment id, timestamp, author, and
  SHA-256 digest of the exact source text.
- Raw pane text is forbidden. Fixtures and records may include only structured
  closeout text or redacted static-proof notes.
- Material Cargo commands must parse as real argv with
  `RCH_REQUIRE_REMOTE=1`, `RCH_NO_SELF_HEALING=1`, and
  `rch --no-self-healing exec --`.
- Static verifier notes may emit `static-verifier-v1` receipts only when the
  structured footer says material Cargo proof is not required.
- Duplicate comments are ineligible by `(bead_id, source_text_sha256)`.
- Ambiguous prose, missing commands, stale command shapes, local fallback
  evidence, and empty owned paths fail closed with actionable reason codes.
- Mixed static/RCH closeouts may preserve static-clean evidence but must still
  emit `wait_rch` when material remote proof remains blocked.
- Selected-worker topology preflight failures are RCH deferrals, not source
  proof. They must emit `wait_rch`, keep the extractor/queue-surface coarse
  `blocked_worker_pressure` projection, preserve `rch.topology_preflight_failed`
  as the eligibility reason, and keep `replay_allowed: false`.
- Active-project exclusion is also a deferral: a valid remote-required command
  is waiting behind another active FrankenTerm proof lane. The extractor must
  preserve `rch.active_project_exclusion`, emit `wait_rch`, and keep
  `replay_allowed: false`.
- Insufficient slots and telemetry gaps are deferrals too. The extractor must
  preserve `rch.insufficient_slots` or `rch.telemetry_gap`, emit `wait_rch`,
  keep the coarse `blocked_worker_pressure` projection for queue compatibility,
  and keep `replay_allowed: false`.

## Ineligibility Failure Classes

The extractor distinguishes deferred-but-replayable proof from sources that must
never be auto-queued. Each maps to a distinct reason code so an operator or
robot can act without re-reading prose:

| Reason code | Trigger | Why it is not a deferred receipt |
| --- | --- | --- |
| `ambiguous_comment` | No structured command in the footer. | Prose is not proof. |
| `stale_command_shape` | Material Cargo command missing `RCH_REQUIRE_REMOTE=1`, `RCH_NO_SELF_HEALING=1`, or the `rch --no-self-healing exec --` shape. | Replaying a stale shape risks a non-conforming or local run. |
| `operator_cancelled` | Footer `Blocker: operator_cancelled`. | The operator deliberately stopped this replay; honor it even when RCH is otherwise blocked. |
| `code_test_failure` | Footer `Blocker: code_failure`/`test_failure` or `Proof-State: failing`/`red`/`failed`. | A remote worker reached Cargo and the proof went red — a real failing result, not infra deferral. |
| `dirty_overlap` | Captured `Dirty-Paths` includes a path outside `Owned-Paths`. | Replaying would bundle unrelated dirty work; resolve the tree first. |
| `duplicate_comment` | Same `(bead_id, source_text_sha256)` seen earlier. | The receipt already exists. |

RCH admission failure and worker pressure remain *deferral* signals (the receipt
is emitted with `wait_rch`/`blocked_worker_pressure`). The same deferral rule
applies when a worker is selected but remote topology preflight fails before
Cargo/test, when RCH reports active-project exclusion, insufficient slots, or
telemetry gaps. Agent Mail outage is recorded in the receipt's
`coordination.agent_mail_state` rather than blocking extraction. Only the table
above renders a source ineligible.

## Extraction States

| State | Meaning |
| --- | --- |
| `receipt_emitted` | A deterministic receipt projection was emitted. |
| `ineligible` | The source was parseable enough to reject with reason codes. |
| `duplicate` | The source duplicates an earlier `(bead_id, digest)` record. |

## Golden Corpus

The static verifier freezes source comments and expected JSONL records for:

- `remote-rch-blocked-closeout`
- `static-only-closeout`
- `mixed-static-rch-closeout`
- `selected-worker-topology-preflight-closeout`
- `active-project-exclusion-closeout`
- `ambiguous-prose-ineligible`
- `stale-command-ineligible`
- `duplicate-comment-ineligible`
- `operator-cancelled-ineligible`
- `dirty-overlap-ineligible`
- `code-failure-ineligible`

It also rejects malformed fixture fragments for missing source text, raw pane
text storage, and unknown expected states.

Run:

```bash
bash tests/e2e/test_deferred_proof_comment_extractor_contract.sh
```

This verifier is static. Future Rust or Robot/MCP implementation proof must run
through remote-required RCH.
