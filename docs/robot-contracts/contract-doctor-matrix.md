# Robot/MCP Contract-Doctor Coverage Matrix (W11.3a / ft-7h5da.13.5)

**Decision:** `ft-7h5da.12.3` (GO — build the Contract Doctor as contract infrastructure).
**Oracle to build against this spec:** `ft-7h5da.13.6` (completeness oracle).
**Unified verdict + attestation:** `ft-7h5da.13.7`.

**Purpose.** The existing ledger `docs/robot-contracts/api-surface-coverage.md`
(`ft-b7ysg`, oracle `conformance_robot_api_surface_coverage.rs` over
`robot_api_contracts::ApiSurface::ALL`) proves **envelope shape** per surface
(schema + golden + proof lane). It does **not** cross-cut the other five
contract dimensions, which today live in *separate* ledgers/tests. This matrix
joins all six dimensions per surface so the Contract Doctor can fail on any
uncovered cell — and surfaces the real gaps that join exposes.

This doc is verifiable with `rg` + `jq` only: **no `cargo`, no RCH.**

## The six dimensions and their backing checks

| Dim | Meaning | Backing check(s) |
|-----|---------|------------------|
| **ENV** | Envelope shape (ok/data/error schema) | `docs/json-schema/wa-robot-*.json` + `conformance_robot_api_surface_coverage.rs` + `conformance_robot_envelope_schema.rs` + `golden_robot_envelope/*` / `control_plane_golden_matrix.json` |
| **PAR** | Robot↔MCP parity (same envelope/error across CLI + MCP twin) | `mcp_conformance{,_core_tools,_additional_tools,_mission_tx,_rules_test}.rs`, `wa_{state,events,event_mutations,reservations,mission_toon,tx_toon}_mcp_conformance.rs`, `metamorphic_robot_envelope_canon.rs` |
| **POL** | Policy gating + audit row on mutation | `docs/security/policy-denial-audit-wiring-matrix.md` (`ft-6mmyp`); `mcp_authorize_mcp_mutation` (`mcp_tools.rs:194`) |
| **RED** | Redaction on every pane-content read | `docs/security/read-path-redaction-matrix.md` (`ft-h8da2`) + `redaction-coverage-map.md` |
| **TOON** | JSON↔TOON equivalence | `proptest_toon_roundtrip.rs` + `toon_golden.rs` (generic, shared envelope); focused: `golden_pane_state_toon.rs`, `wa_tx_toon_conformance.json`, `wa_{mission,tx}_toon_mcp_conformance.rs` |
| **ERR** | Error-code/hint/retryability stability | `mcp_error.rs` error taxonomy (W12.3 / `ft-7h5da.13.3`, CLOSED); `why` surface (`wa-robot-why.json`) |

**Legend:** `✓` covered by a named check · `~` partial (gate/path exists but a
sub-assertion is missing) · `GAP` no check found · `n/a` dimension does not
apply to this surface · `R`/`M` = read / mutation.

## Matrix (39 `ApiSurface::ALL` surfaces × 6 dimensions)

| Surface | Cat | R/M | MCP twin | ENV | PAR | POL | RED | TOON | ERR |
|---|---|---|---|---|---|---|---|---|---|
| `get-text` | pane | R | `wa.get_text` | ✓ | ✓ | n/a | ✓ | ✓ | ✓ |
| `batch-get-text` | pane | R | none | ✓ | n/a | n/a | ✓ | ✓ | ✓ |
| `send-text` | pane | M | `wa.send` | ✓ | ✓ | ✓ | n/a | ✓ | ✓ |
| `state` | pane | R | `wa.state` | ✓ | ✓ | n/a | ✓ | ✓ | ✓ |
| `dom` | pane | R | `wa.dom` | ✓ | ✓ | n/a | ✓ | ✓ | ✓ |
| `search` | search | R | `wa.search` | ✓ | ✓ | n/a | ✓ | ✓ | ✓ |
| `search-explain` | search | R | none | ✓ | n/a | n/a | ✓ | ✓ | ✓ |
| `search-pipeline-status` | search | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `events` | events | R | `wa.events` | ✓ | ✓ | n/a | ✓ | ✓ | ✓ |
| `event-stream` | events | R | none | ✓ | n/a | n/a | ✓ | n/a | ✓ |
| `events-mutate` | events | M | `wa.events_annotate/triage/label` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `workflow-run` | workflow | M | `wa.workflow_run` | ✓ | ✓ | ✓ | n/a | ✓ | ✓ |
| `workflow-list` | workflow | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `workflow-status` | workflow | R | `wa.workflow_status` | ✓ | ✓ | n/a | n/a | ✓ | ✓ |
| `workflow-abort` | workflow | M | none | ✓ | n/a | ✓ | n/a | ✓ | ✓ |
| `rules-list` | rules | R | `wa.rules_list` | ✓ | ✓ | n/a | ✓ | ✓ | ✓ |
| `rules-test` | rules | R | `wa.rules_test` | ✓ | ✓ | n/a | ✓ | ✓ | ✓ |
| `rules-lint` | rules | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `agent-inventory` | agent | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `agent-configure` | agent | M | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `agent-subspace-rpc` | agent | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `accounts-list` | accounts | R | `wa.accounts` | ✓ | ✓ | n/a | n/a | ✓ | ✓ |
| `accounts-refresh` | accounts | M | `wa.accounts_refresh` | ✓ | ✓ | ✓ | n/a | ✓ | ✓ |
| `reserve` | reservations | M | `wa.reserve` | ✓ | ✓ | ✓ | n/a | ✓ | ✓ |
| `release` | reservations | M | `wa.release` | ✓ | ✓ | ✓ | n/a | ✓ | ✓ |
| `mission-state` | mission | R | `wa.mission_state` | ✓ | ✓ | n/a | n/a | ✓ | ✓ |
| `mission-decisions` | mission | R | `wa.mission_explain` | ✓ | ✓ | n/a | n/a | ✓ | ✓ |
| `tx-plan` | tx | R | `wa.tx_plan` | ✓ | ✓ | n/a | n/a | ✓ | ✓ |
| `tx-run` | tx | M | `wa.tx_run` | ✓ | ✓ | ✓ | n/a | ✓ | ✓ |
| `tx-rollback` | tx | M | `wa.tx_rollback` | ✓ | ✓ | ✓ | n/a | ✓ | ✓ |
| `tx-show` | tx | R | `wa.tx_show` | ✓ | ✓ | n/a | n/a | ✓ | ✓ |
| `replay-inspect` | replay | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `replay-diff` | replay | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `replay-regression` | replay | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `health` | diagnostics | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `coordination-risk` | diagnostics | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `quickstart` | meta | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `why` | meta | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `approve` | meta | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |

> ENV is COVERED for all 39 (existing `api-surface-coverage.md`). ERR and TOON
> are covered generically via the shared envelope (taxonomy + `proptest_toon_roundtrip`);
> per-surface focused TOON goldens exist only for pane-state / mission / tx / rules.

### Applicability and causal evidence

`none` means no registered MCP twin, not an unimplemented promised endpoint.
The registered inventory is `tests/fixtures/mcp_manifest.json` under
`crates/frankenterm-core`; its conformance tests compare actual server schemas.
The obsolete names `wa.workflow_list`, `wa.workflow_abort`, `wa.rules_lint`,
`wa.why`, and `wa.approve` are not advertised as supported MCP tools.

- DOM RED: `robot_dom::tests::zone_text_is_redacted` and
  `ansi_split_secrets_are_normalized_before_dom_redaction` test the shared
  CLI/MCP output builder, including planted and ANSI-split secrets.
- Event mutation POL: the three `events_*_tool_applies_mcp_mutation_policy_gate`
  tests assert denial, unchanged event fields, one typed durable denial in the
  tool's exact database, and no write to an unrelated configured database.
  `events_denial_with_unavailable_explicit_audit_db_never_falls_back` preserves
  enforced denial when durable auditing is unavailable. The corresponding
  `events_*_audit_records_*` tests cover successful mutation receipts.
- Explicit workflow abort updates durable workflow state and settles its trigger;
  it does not execute compensation or terminal input. The earlier compensation
  claim came from a stale CLI comment. Its conditional state transition, audit,
  and undo invalidation are one transaction, tested by
  `contract_doctor_explicit_abort_is_atomic_with_audit_and_undo`.
  Operator cancellation is not blocked by a pane-input policy rule.
- Agent configure writes local configuration through its transaction and
  target-identity checks. Pane-input/MCP mutation policy is inapplicable;
  this row does not attest local-file transaction safety from envelope tests.
- Agent subspace RPC is a schema-only contract, not a registered resource or
  delivery implementation. Its receipt-validation tests do not prove live
  policy enforcement, redaction, or delivery; those dimensions are inapplicable
  to this contract-only row.
- Robot approve only validates an existing approval. The CLI tests
  `robot_approve_valid_code_stays_unconsumed`,
  `robot_approve_dry_run_matches_apply_mode_without_consuming`, and
  `approve_validation_does_not_consume_token` pin its read-only behavior;
  wrong-workspace/pane/fingerprint and expired/consumed negatives remain required.

The final PAR cell has focused runtime evidence under `ft-7h5da.13.7`. Policy evidence
for `ft-6mmyp` and batch-get-text RED evidence for `ft-5puf0` are retained below. A partial cell is not a passed
per-surface proof. The static oracle rejects new untracked partial cells and
every `GAP`, independently of whether remote suites have been run.

### Retained focused runtime evidence

RCH job `j-30016197441356132` on `ovh-b` completed successfully at
`2026-09-12T11:05:56Z`, source mirror `ac7efc675b30cc0c`, base
`f2b406a46474e29e5ade7a35c26a19d94dd32d08`. The retained transcript is
`/tmp/ft-8r43i.5kXohF/rch-cli-doctor-parity-1125.log`; its 13 source hashes are
in `cli-doctor-parity-1125-remote.sha256` in the same evidence directory.

- `contract_doctor_cli_mcp_read_and_refresh_parity` passed in the CLI integration
  target (3 passed, 0 failed). Its four cases compare actual CLI and MCP data for
  DOM, nonempty workflow status, accounts list, and accounts refresh. Refresh
  timestamps must lie within the actual call interval before normalization;
  the other returned data is compared directly.
- `tests::redact_pane_text_results_for_output_scrubs_ok_and_error_payloads`
  passed in the CLI binary target (1 passed, 0 failed). Planted secrets are
  absent from successful pane text and error/hint payloads with both escape modes.

This snapshot predates the shared workflow preview, fifth parity surface, and
recursive metadata-redaction changes. Those changes await the current-source
TS1 run (`c51394b816ffb44c`); the older success does not attest them.

The TS1 run at source `c51394b816ffb44c`, base
`8ae0a83ddd0286207b66633cbeef980b8321284f`, completed on 2026-09-12 with
31,554 core tests passed, zero failed, and five ignored. Transcript
`/tmp/ft-8r43i.5kXohF/rch-ts1-doctor-abort-final-1208.log` has SHA-256
`f27204c7bc81ece2626cd2a25a12e3ed007fe1cc835fd543850fb6ac525af62d`.
Its following named oracles passed:

- `contract_doctor_send_policy_denial_preserves_pane_and_audit`: send POL,
  denied input leaves pane text unchanged and persists the denial receipt.
- `contract_doctor_reservation_and_refresh_gates_preserve_state_and_audit`:
  reserve, release, and accounts-refresh POL, denied and approval-required
  requests preserve state and persist typed receipts.
- `contract_doctor_tx_and_mission_gates_audit_before_file_effects`: tx-run and
  tx-rollback POL, plus MCP-only mission controls, preserve input files on
  denial and unsupported approval while recording the outcome.
- `contract_doctor_explicit_abort_is_atomic_with_audit_and_undo`: workflow-abort
  POL, atomic conditional transition and audit/undo semantics.
- `contract_doctor_workflow_gate_audits_without_starting_execution` in the MCP
  additional-tools target: workflow-run POL, denied and approval-required
  calls create no workflow execution, step logs, or action plans.

The same run passed recursive metadata-redaction controls but failed preview
roundtrip and CLI parity before reaching the new preview golden. Its overall
command exited 101; the named policy successes above are focused evidence,
not a full-gate pass. The missing-warnings default and custom-send metadata
fixes were verified in the next run described below. The complete strict
producer gate and attestation closeout remain outstanding.

The next TS1 run, source `10546e15950c184a` on the same base, completed at
`2026-09-12T11:25:44Z`. Transcript
`/tmp/ft-8r43i.5kXohF/rch-ts1-preview-installer-1240.log` records 966 CLI binary
tests passed (one ignored), 86 CLI integration tests passed (seven ignored),
and 31,556 core tests passed (five ignored), all with zero failures in those
targets. `contract_doctor_cli_mcp_read_and_refresh_parity` now proves all five
surfaces, including known and unknown workflow previews, exact returned-data
equality, and unchanged workflow execution/step-log state in both databases.
This closes workflow-run PAR. The MCP additional-tools target passed its typed
preview checks but failed its obsolete compact preview golden (eight passed,
one failed); overall exit was 101. The golden was manually updated to the
validated rich report, retaining deferred policy checks, pane metadata, and
all six planned actions. Its rerun and complete producer closeout remain required.

## GAP SUMMARY (what the join exposes — the Doctor's real value)

### MCP dispatch inventory outside the typed Robot registry

The following 14 tools complete the union with the MCP twins in the matrix:
the union must equal the 37 registered tools in `mcp_manifest.json`, with no
duplicates. These are an explicit dispatch inventory, not invented additions
to `ApiSurface::ALL`. The named checks cover the stated operations; this table
does not claim CLI parity for commands outside that typed registry. Tool
definition/schema and common envelope/error checks remain required for every
entry. MCP JSON-only operations do not acquire a TOON claim from this index.

| MCP-only tool | Role | Named checks |
|---|---|---|
| `wa.attention` | Read-only attention report | `mcp_tools::tests::wa_attention_tool_is_read_only_and_explains_inline_input` |
| `wa.await_event` | Bounded event delivery and acknowledged leases | `mcp_tools::tests::await_event_claim_direct_handler_dispatch_fails_closed_without_a_lease`, `await_event_contradictory_tokens_do_not_mutate_or_restart_storage_epoch` |
| `wa.cass_search` | Search result read | `mcp_conformance_additional_tools::mcp_conformance_wa_cass_search_matches_snapshot` |
| `wa.cass_status` | Index status read | `mcp_conformance_additional_tools::mcp_conformance_wa_cass_status_matches_snapshot` |
| `wa.cass_view` | Indexed context read | `mcp_conformance_additional_tools::mcp_conformance_wa_cass_view_matches_snapshot` |
| `wa.mission_abort` | Policy-gated mission mutation | `mcp_conformance_mission_tx::mcp_conformance_wa_mission_abort_contract_matches_golden`, `mcp_tools::tests::contract_doctor_tx_and_mission_gates_audit_before_file_effects` |
| `wa.mission_objective_plan` | Read-only objective plan | `mcp_tools::tests::mission_objective_plan_tool_returns_dry_run_surface`, `mission_objective_plan_tool_rejects_execute_attempt` |
| `wa.mission_pause` | Policy-gated mission mutation | `mcp_conformance_mission_tx::mcp_conformance_wa_mission_pause_contract_matches_golden`, `mcp_tools::tests::contract_doctor_tx_and_mission_gates_audit_before_file_effects` |
| `wa.mission_resume` | Policy-gated mission mutation | `mcp_conformance_mission_tx::mcp_conformance_wa_mission_resume_contract_matches_golden`, `mcp_tools::tests::contract_doctor_tx_and_mission_gates_audit_before_file_effects` |
| `wa.operating_envelope` | Read-only operating admission report | `mcp_tools::tests::operating_envelope_tool_returns_dry_run_status`, `operating_envelope_tool_rejects_execute_attempt` |
| `wa.rehearsal_score` | Read-only rehearsal scoring | `mcp_tools::tests::wa_rehearsal_score_tool_scores_inline_manifest_and_explains_log` |
| `wa.reservations` | Reservation list read | `wa_reservations_mcp_conformance::mcp_conformance_wa_reservations_contract_matches_expected_envelope` |
| `wa.steer_plan` | Read-only steering receipt | `mcp_tools::tests::steer_plan_tool_definition_lists_all_scenarios`, `conformance_steering_receipt_chain::steer_plan_is_side_effect_free`, `conformance_steer_plan_golden::steer_plan_scenarios_match_golden` |
| `wa.wait_for` | Bounded policy-gated pane wait | `mcp_conformance_core_tools::mcp_conformance_wa_wait_for_contract_matches_golden`, `mcp_tools::tests::wait_for_tool_persists_policy_denial_audit_when_storage_is_attached`, `wait_for_pattern_output_redaction_masks_secret_tokens` |

### G1 — Verify durable denial coverage against current dispatch
The shared synchronous and asynchronous MCP authorization helpers now attempt
`policy_denied_audit` persistence. `PolicyGatedInjector` records denied as well
as successful sends in `audit_actions`. The original table's `~` entries for
missing writes are historical, not current source findings; see the updated
[wiring matrix](../security/policy-denial-audit-wiring-matrix.md).
Audit writes are best effort. Per-tool denial/approval tests and unavailable
storage tests must distinguish an enforced denial from a durable audit receipt.
Enumerate direct gates and both shared helpers so the oracle covers every
registered mutation, including tools absent from `ApiSurface::ALL`.

### G2 — Retain current-source workflow preview parity
Dedicated `*_mcp_conformance` proofs exist for core(get-text/send/state/search),
events, event-mutations, mission, tx, rules-test, reservations. `~` = MCP twin
exists with only partial coverage. No such PAR cells remain after the five
actual CLI/MCP comparisons above, including known/unknown workflow previews.
The other formerly listed names have no registered MCP twin and must not be
presented as missing supported endpoints.

### G3 — Retain DOM redaction regression evidence
Both CLI DOM and `wa.dom` are included in
`docs/security/read-path-redaction-matrix.md`; the former missing-row finding
under `ft-5puf0` is stale. Keep planted-secret tests for semantic-zone text and
the actual handler response. A coverage-table entry alone does not establish
that every returned field was redacted.

### G4 — Registry completeness: policy-gated MCP mutations absent from `ApiSurface::ALL`
`wa.mission_pause` / `wa.mission_resume` / `wa.mission_abort` are policy-gated
MCP tools (rows in `ft-6mmyp`) but are **not** in `ApiSurface::ALL` (only
`mission-state` / `mission-decisions` are). A completeness oracle built solely
on `ApiSurface::ALL` (ft-7h5da.13.6) would miss them. Also note the deferred
proof-queue surface (`ft proof …`) is intentionally not yet in `ApiSurface::ALL`
(see `api-surface-coverage.md` §"Deferred Proof Queue Surface"). The oracle must
enumerate **both** `ApiSurface::ALL` **and** the MCP tool dispatch registry.

### G5 — Per-surface focused TOON / error-family goldens are sparse
TOON and ERR pass generically (shared-envelope roundtrip + taxonomy), but
focused per-surface goldens exist only for pane-state / mission / tx / rules
(TOON) and the `why` catalog (ERR). Optional hardening, not a correctness gap.

## How ft-7h5da.13.6 (oracle) consumes this
1. Enumerate `ApiSurface::ALL` **and** the MCP tool dispatch table (closes G4).
2. For each surface, require a non-`GAP`/non-`~` cell in every applicable
   dimension, citing the named check from the tables above.
3. Fail closed on any `GAP`; treat `~` as a tracked exception keyed to its bead
   (`ft-6mmyp` for G1) until closed. New `RobotCommands`/MCP tools auto-require
   coverage.

## Verification (no `cargo`, no RCH)
```bash
# every backing artifact path resolves
rg -q 'ApiSurface::ALL' crates/frankenterm-core/src/robot_api_contracts.rs
test -f docs/robot-contracts/api-surface-coverage.md
test -f docs/security/read-path-redaction-matrix.md
test -f docs/security/policy-denial-audit-wiring-matrix.md
# the dedicated parity + redaction/policy gaps are reproducible
rg -n 'missing|partial' docs/security/policy-denial-audit-wiring-matrix.md   # G1
rg -n 'wa.dom|`dom`' docs/security/read-path-redaction-matrix.md || echo "G3 confirmed: dom absent"
rg -n 'MissionPause|mission_pause' crates/frankenterm-core/src/robot_api_contracts.rs || echo "G4 confirmed: not in ApiSurface::ALL"
```
