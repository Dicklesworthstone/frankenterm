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
| `batch-get-text` | pane | R | none | ✓ | n/a | n/a | ~ | ✓ | ✓ |
| `send-text` | pane | M | `wa.send` | ✓ | ✓ | ~ | n/a | ✓ | ✓ |
| `state` | pane | R | `wa.state` | ✓ | ✓ | n/a | ✓ | ✓ | ✓ |
| `dom` | pane | R | `wa.dom` | ✓ | ~ | n/a | GAP | ✓ | ✓ |
| `search` | search | R | `wa.search` | ✓ | ✓ | n/a | ✓ | ✓ | ✓ |
| `search-explain` | search | R | none | ✓ | n/a | n/a | ✓ | ✓ | ✓ |
| `search-pipeline-status` | search | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `events` | events | R | `wa.events` | ✓ | ✓ | n/a | ✓ | ✓ | ✓ |
| `event-stream` | events | R | none | ✓ | n/a | n/a | ✓ | n/a | ✓ |
| `events-mutate` | events | M | `wa.events_annotate/triage/label` | ✓ | ✓ | GAP | ✓ | ✓ | ✓ |
| `workflow-run` | workflow | M | `wa.workflow_run` | ✓ | ~ | ~ | n/a | ✓ | ✓ |
| `workflow-list` | workflow | R | `wa.workflow_list` | ✓ | ~ | n/a | n/a | ✓ | ✓ |
| `workflow-status` | workflow | R | `wa.workflow_status` | ✓ | ~ | n/a | n/a | ✓ | ✓ |
| `workflow-abort` | workflow | M | `wa.workflow_abort` | ✓ | ~ | GAP | n/a | ✓ | ✓ |
| `rules-list` | rules | R | `wa.rules_list` | ✓ | ✓ | n/a | ✓ | ✓ | ✓ |
| `rules-test` | rules | R | `wa.rules_test` | ✓ | ✓ | n/a | ✓ | ✓ | ✓ |
| `rules-lint` | rules | R | `wa.rules_lint` | ✓ | ~ | n/a | n/a | ✓ | ✓ |
| `agent-inventory` | agent | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `agent-configure` | agent | M | none | ✓ | n/a | GAP | n/a | ✓ | ✓ |
| `agent-subspace-rpc` | agent | M | `wa://subspace/rpc` | ✓ | ~ | GAP | n/a | ✓ | ✓ |
| `accounts-list` | accounts | R | `wa.accounts` | ✓ | ~ | n/a | n/a | ✓ | ✓ |
| `accounts-refresh` | accounts | M | `wa.accounts_refresh` | ✓ | ~ | ~ | n/a | ✓ | ✓ |
| `reserve` | reservations | M | `wa.reserve` | ✓ | ✓ | ~ | n/a | ✓ | ✓ |
| `release` | reservations | M | `wa.release` | ✓ | ✓ | ~ | n/a | ✓ | ✓ |
| `mission-state` | mission | R | `wa.mission_state` | ✓ | ✓ | n/a | n/a | ✓ | ✓ |
| `mission-decisions` | mission | R | `wa.mission_explain` | ✓ | ✓ | n/a | n/a | ✓ | ✓ |
| `tx-plan` | tx | R | `wa.tx_plan` | ✓ | ✓ | n/a | n/a | ✓ | ✓ |
| `tx-run` | tx | M | `wa.tx_run` | ✓ | ✓ | ~ | n/a | ✓ | ✓ |
| `tx-rollback` | tx | M | `wa.tx_rollback` | ✓ | ✓ | ~ | n/a | ✓ | ✓ |
| `tx-show` | tx | R | `wa.tx_show` | ✓ | ✓ | n/a | n/a | ✓ | ✓ |
| `replay-inspect` | replay | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `replay-diff` | replay | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `replay-regression` | replay | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `health` | diagnostics | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `coordination-risk` | diagnostics | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `quickstart` | meta | R | none | ✓ | n/a | n/a | n/a | ✓ | ✓ |
| `why` | meta | R | `wa.why` | ✓ | ~ | n/a | n/a | ✓ | ✓ |
| `approve` | meta | M | `wa.approve` | ✓ | ~ | GAP | n/a | ✓ | ✓ |

> ENV is COVERED for all 39 (existing `api-surface-coverage.md`). ERR and TOON
> are covered generically via the shared envelope (taxonomy + `proptest_toon_roundtrip`);
> per-surface focused TOON goldens exist only for pane-state / mission / tx / rules.

## GAP SUMMARY (what the join exposes — the Doctor's real value)

### G1 — Policy-denial **audit-row** wiring incomplete on mutations (highest value)
The policy **gate** returns `MCP_ERR_POLICY` on Deny everywhere, but the
`policy_denied_audit` row is missing/partial (`docs/security/policy-denial-audit-wiring-matrix.md`, `ft-6mmyp`):
- `~` (gate ok, no audit row): `tx-run`, `tx-rollback`, `reserve`, `release`, `accounts-refresh`.
- `~` (audits to a *different* table on success, deny path unaudited): `send-text`, `workflow-run`.
- `GAP` (not in the matrix at all — verify gating + audit): `events-mutate`, `workflow-abort`, `agent-configure`, `agent-subspace-rpc`, `approve`.

### G2 — Robot↔MCP **parity** has no dedicated assertion for several MCP twins
Dedicated `*_mcp_conformance` proofs exist for core(get-text/send/state/search),
events, event-mutations, mission, tx, rules-test, reservations. `~` = MCP twin
exists but relies on shared schema with **no dedicated parity test**:
`workflow-run/list/status/abort`, `rules-lint`, `accounts-list/refresh`, `why`,
`approve`, `dom`, `agent-subspace-rpc`.

### G3 — `dom` redaction not in the read-path matrix (tracked: `ft-5puf0`)
`wa.dom` returns OSC-133 semantic zones that can carry pane-sourced text, but
`dom` is absent from `docs/security/read-path-redaction-matrix.md`. Confirm the
dom handler runs zone text through `Redactor::redact`, then add the row (or fix).
Filed as **`ft-5puf0`** (security) — a read surface serving unredacted pane
content would be a fail-closed redaction violation.

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
