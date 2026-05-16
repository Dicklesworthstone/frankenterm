# Operator Runbook — Driving a frankenterm Swarm

**Audience:** A new operator (human or agent) about to run a multi-agent
swarm session against this repo. Optimized so you can drive a productive
session without consulting chat history from any prior session.

**Prerequisites assumed:** ntm is configured, `br` and `bv` are on
`$PATH`, you can read AGENTS.md.

---

## Quick Reference (10-line drive-by)

```text
PRE:    rch healthy? disk <90%? beads not corrupted? ntm session up?
TICK 1: dispatch initial marching orders to each pane (slug-based target dirs)
LOOP:   every ~4 min — swarm-tick.sh → tail panes → classify → nudge idle
NUDGE:  ntm --robot-send -t S:0.N "msg" + tmux Enter (twice for codex)
RESET:  in_progress >2h with no commits → broadcast first, force-release after silence
DISK:   >96% → run scripts/clean-stale-targets.sh; nudge agents to rm release/
MODE:   CLAIM → REVIEW → FINAL → DRAIN; pivot when the relevant trigger fires
DONE:   commits-1h ≤ 4 + open=0 + ready=0 + ≥2 cc panes "converged"
END:    write SESSION SUMMARY → close beads → push → notify
```

Read the rest only when the loop deviates from steady-state.

---

For 64+ CPU / 256 GiB hosts, follow
[`docs/high-core-swarm-runbook.md`](high-core-swarm-runbook.md) before
claiming high-scale proof or changing large-fleet tuning.

---

## 1. Pre-flight

Before sending the first marching order, verify the environment.

| Check | Command | Pass condition |
|-------|---------|----------------|
| rch workers healthy | `rch status` | ≥1 worker `online` |
| Disk space | `df -h /System/Volumes/Data` | `Capacity` < 90% |
| Beads DB walkable | `br ready --json \| jq length` | Returns an integer (no errors) |
| ntm session exists | `tmux has-session -t frankenterm` | Exit 0 |
| Agent panes responding | `ntm --robot-snapshot -t frankenterm` | All panes named (cc_1, cc_2, cod_1...) |
| Agent Mail usable | Agent Mail MCP macro/tool call | Succeeds, or fails once and succeeds on one retry |

**If any pre-flight check fails:**
- rch unhealthy → for RCH-required proof lanes, record an infra-blocked
  proof-doctor verdict; use `scripts/cargo-local.sh` only as explicitly
  labeled local smoke when the Bead allows non-proof diagnostics.
- Disk >90% → run `scripts/clean-stale-targets.sh` *before* dispatching work, not after.
- Beads DB locked → wait 10s; if persistent, `lsof .beads/beads.db` to find writer; if no writer, the DB is corrupted (use `bv` for triage instead, per MEMORY.md note `br-db-corruption`).
- ntm pane missing → relaunch via the project's spawn script before the swarm tick begins.
- Agent Mail red/unreachable → retry once after a few seconds; if it still fails, do not repair, restart, or kill the shared service. Continue with a Beads-only handoff snapshot:
  ```bash
  ft robot coordination-risk
  ```
  This is the preferred robot-mode entry point for agents. It wraps the same
  read-only fallback producer below and returns the existing snapshot under the
  standard robot envelope. Operators can still run the producer directly when
  debugging the script itself:
  ```bash
  scripts/swarm-tick.sh --agent-mail-fallback frankenterm
  ```
  The snapshot includes the red-mail marker, active assignees from in-progress
  Beads, freshness/staleness, ready work, and dirty-file conflict hints. Its
  `.git` object is the coordination contract while mail is red:
  - `dirty_count`, `tracked_dirty_count`, `untracked_dirty_count`, and
    `high_risk_count` give the fast numeric triage.
  - `risk_level` is `clean`, `low`, `medium`, or `high`; `high` means at
    least one tracked file or shared coordination tracker is dirty.
  - `risk_reason` is short human text suitable for Beads handoff comments.
  - `dirty_domains[]` groups paths by `category`, `severity`, `count`, and
    `paths[]` so an agent can avoid parsing prose.
  - `dirty_paths[]` and `conflict_hints[]` preserve per-path `category` and
    `severity`.

  The stable categories are:
  - `shared_tracker` / `high`: `.beads/*`, especially `.beads/issues.jsonl`.
  - `tracked_overlap_risk` / `high`: any tracked file with local changes.
  - `untracked_review_required` / `medium`: untracked paths outside known
    janitor scratch space.
  - `janitor_untracked` / `low`: untracked `.stash_janitor_workspace/*`
    artifacts.

  Stale-bead reopen decisions while Agent Mail is red are conservative by
  default. Use `.beads.stale_reopen`, not age alone:
  - `default_action` is always `do_not_reopen`; an empty ready queue does not
    weaken that default.
  - `active_not_stale[]` means the Bead was updated inside the two-hour
    threshold. Treat it as active, even if it blocks the work you wanted. In
    the May 9 red-mail scenario, this is how recent `ft-269nf` and `ft-3yptk`
    activity should be classified.
  - `candidates[]` means the Bead crossed the two-hour threshold, but it still
    requires a status check before reopening. Inspect recent comments and
    handoffs with `br show <id> --json`, refresh the fallback snapshot, and
    verify dirty paths do not overlap the Bead's likely files.
  - `dirty_overlap_unknown[]` means tracked/shared or untracked-review paths
    already exist in the worktree. Do not reopen related work until ownership
    is clear.

  Prefer a visible status-check comment before any reopen:
  ```bash
  br comments add <id> --author <agent> --message 'status check: still active? Agent Mail is unavailable; please comment if this bead is still owned.'
  ```

  Only after those checks show stale ownership and no dirty-path signal, reopen
  explicitly:
  ```bash
  br update <id> --status open --assignee "" --actor <agent>
  ```

  To produce a reviewed Beads handoff/closeout block while Agent Mail is red,
  use the read-only handoff formatter:
  ```bash
  scripts/swarm-tick.sh --agent-mail-handoff --bead <id> \
    --touched-path scripts/swarm-tick.sh \
    --avoided-path crates/frankenterm/src/main.rs \
    --proof-command 'bash -n scripts/swarm-tick.sh' \
    frankenterm
  ```

  The formatter prints Markdown only. It does not post comments, mutate Beads,
  touch Agent Mail, or infer proof. Pass every touched path, intentionally
  avoided path, and proof command explicitly, then review the block before:
  ```bash
  br comments add <id> --author <agent> --file <reviewed-handoff.md>
  ```

  The script fixture remains the producer-level compatibility gate:
  `tests/fixtures/swarm-tick/agent-mail-fallback/expected.json` and
  `tests/swarm_tick_tests.bats` pin the fallback payload. The robot wrapper's
  data contract is `docs/json-schema/wa-robot-coordination-risk.json`; keep the
  schema, fixture, and `ft robot coordination-risk` parser in sync.

## 2. Proof-Doctor Gate For Proof Lanes

Use proof-doctor for every Bead whose closeout depends on RCH, Cargo, clippy,
tests, benches, E2E, high-scale worker predicates, or proof-lane evidence. It
is the operator vocabulary for separating RCH/tooling blockers, source
failures, dirty-tree ownership, invalid command shapes, and inconclusive logs.
When retained evidence is ready for durable closeout, pass
`--proof-record-output <path>.jsonl` so proof-doctor appends a validated
`ProofAttemptRecord` row instead of leaving future agents to parse prose. Use
`--proof-record-redaction-status none-needed` or `redacted` when the retained
artifact bundle has been reviewed; the default `unknown` keeps closeout
eligibility conservative.

Primary anchors:

- `docs/proposals/ft-wik9p-proof-doctor-verdict-schema.md` for the status,
  reason-code, handoff, and robot-mode envelope contract.
- `docs/proposals/ft-tn6cw-proof-lane-evidence-taxonomy.md` for proof-state
  truthfulness rules and invalid command-shape examples.
- `crates/frankenterm-core-audit-types/src/proof_doctor.rs` for the classifier
  DTOs, JSON/TOON golden coverage, and the E2E fixture scenarios.
- `crates/frankenterm-core-audit-types/src/proof_handoff.rs` and
  `crates/frankenterm-core-audit-types/src/proof_lane.rs` for Beads/Agent Mail
  handoff text and durable ledger projections.

### 2.1 Pre-proof checklist

Before launching or claiming a proof lane:

1. Read the Bead and dependency state with `br show <id> --json`.
2. Confirm the exact proof backend, command, package/test filter, and
   `CARGO_TARGET_DIR`. RCH-required lanes use direct remote Cargo argv, for
   example:
   ```bash
   rch exec -- env CARGO_TARGET_DIR=/tmp/<bead>-<purpose>-target cargo test -p <crate> <filter> -- --nocapture
   ```
3. Inspect `git status --short` and active ownership. If a dirty path overlaps
   the proof scope and belongs to another Bead, agent, or reservation, classify
   `dirty_tree_blocked` instead of running over it.
4. Record the RCH binary/tool state, effective timeout setting, selected worker
   if any, and worker predicate if the proof depends on hardware capacity.
5. Keep the intended command as argv in the verdict. Do not translate a shell
   string into proof after the fact.

Invalid for an RCH-required closeout unless retained metadata proves remote
Cargo started:

```bash
cargo test ...
scripts/cargo-local.sh test ...
rch exec -- bash -lc 'cargo test ...'
rch exec -- env CARGO_TARGET_DIR=/tmp/foo bash -lc 'cargo test ...'
```

Local Cargo can be cited only as local smoke or docs-static validation, never
as remote proof for a Bead whose proof lane requires RCH.

### 2.2 Classify the evidence before claiming it

RCH transfer logs are transfer evidence only:

- "Selected worker" means worker selection happened.
- "Sync completed" means workspace transfer happened.
- "Remote Cargo reached" requires retained logs proving Cargo or rustc started
  on the remote worker.
- "Tests passed" requires a terminal pass for the intended test or E2E scope
  plus retained artifacts.

Current scenario mapping:

| Evidence | Proof-doctor status | Required wording |
| --- | --- | --- |
| Installed RCH still emits the stale external-timeout wrapper and fails before Cargo | `infra_blocked` | "RCH wrapper/tooling blocked before Cargo; no source verdict." |
| Patched RCH reaches remote Cargo/rustc, then first-party code fails to compile | `source_blocked` | "Remote Cargo/rustc reached first-party source; source is blocked in `<path>` and owned by `<bead-or-agent>`." |
| Dirty active file overlaps the proof path and another Bead/agent owns it | `dirty_tree_blocked` | "Dirty owned path blocks attribution; do not run or close this proof without owner release." |
| RCH selected a worker or synced but there is no retained Cargo/rustc/test evidence | `inconclusive` | "RCH sync completed, but no remote Cargo proof was retained; rerun with fail-closed logging." |
| Direct RCH Cargo lane exits 0 with complete artifacts and ledger validation | `passed` | "Remote proof passed with retained RCH/Cargo evidence and complete artifacts." |

For `source_blocked`, include the first compiler/test diagnostic path and the
owner source. For `infra_blocked` before Cargo, do not file source findings
against the package under test. For `dirty_tree_blocked`, do not edit the
overlapping file unless the owner releases it or the user explicitly assigns
the conflict.

### 2.3 Closeout adoption gate

Every future proof-lane Bead closeout must include either:

```text
Proof-doctor: <status>; phase <phase>; reason <reason_code>; verdict <verdict_id or artifact>; remote Cargo <reached|not reached>; owner <owner or none>; target_dir <path|none>; target_lifecycle <kept|cleanup_requested|deletion_authorized|cleaned|not_applicable>; target_size <size|unknown>; closeout <safe|blocked>.
Proof-record: <written|refused|write_failed|not_requested>; path <jsonl artifact path|none>; validation <ok|warning|error>; closeout <safe|blocked>.
```

or an explicit non-applicability sentence:

```text
Proof-doctor: not applicable; docs-static change only; no Cargo/RCH proof lane claimed.
Target-dir lifecycle: not applicable; no Cargo/RCH target dir created.
```

Closeout rules:

- A green claim requires `passed` plus proof-lane ledger validation or equivalent
  retained artifact evidence.
- `runnable` is only a preflight result; it is not a pass.
- `infra_blocked`, `dirty_tree_blocked`, `invalid`, `skipped_not_proven`, and
  `inconclusive` do not prove source health.
- `source_blocked` and `test_blocked` are real red results only after remote
  Cargo/rustc/test execution is positively observed.
- Beads comments and Agent Mail handoffs should carry the same status,
  reason code, command, worker/sync/Cargo evidence, owner, and next action.
- If `--proof-record-output` is used, cite the JSONL path in the Beads comment.
  A `refused` or `write_failed` proof record is evidence that the lane is not
  closeout-ready; do not rewrite it by hand to make the closeout green.
- RCH-heavy closeouts without target-dir lifecycle fields are incomplete even
  when the proof itself passed. Disk pressure is a shared resource issue, not
  cleanup trivia.

### 2.4 Generated handoff and proof-record workflow

Use the generated handoff package whenever the lane is more than a docs-static
or script-syntax check. The operator flow is:

1. Run proof-doctor as a classifier, not as the proof command executor. Keep the
   intended proof command after `--` as argv:
   ```bash
   ft proof-doctor -f json \
     --bead <id> \
     --agent <agent> \
     --scope <cargo-test|cargo-check|cargo-clippy|e2e|high-scale|static> \
     --phase <preflight|launch-observed|remote-cargo-observed|terminal-classified|evidence-gap> \
     --required-backend <rch|static|local> \
     --target-dir /tmp/<bead>-<purpose>-target \
     --evidence-artifact <retained-rch-or-harness-summary.json> \
     --proof-record-output docs/attestations/proof-ledger/<id>.jsonl \
     --proof-record-redaction-status <none-needed|redacted> \
     -- rch exec -- env CARGO_TARGET_DIR=/tmp/<bead>-<purpose>-target cargo test -p <crate> <filter> -- --nocapture \
     > /tmp/<id>-proof-doctor.json
   ```
2. Review `/tmp/<id>-proof-doctor.json` before posting it. The fields that
   matter for closeout are:
   - `.verdict.status`, `.verdict.phase`, `.handoff.reason_code`, and
     `.handoff.safe_to_close`.
   - `.handoff.beads_comment`, which is the Beads comment body after human
     review.
   - `.handoff.agent_mail`, which is the targeted Agent Mail body when the
     owner is another agent and Agent Mail is usable.
   - `.proof_record.write_status`, `.proof_record.resolved_path`, and
     `.proof_record.safe_to_close_source_bead`.
3. Post the generated Beads handoff text without rewriting the status:
   ```bash
   jq -r '.handoff.beads_comment' /tmp/<id>-proof-doctor.json \
     | br comments add <id> --author <agent> --file -
   ```
4. If Agent Mail is reachable and `.handoff.agent_mail` is not null, send that
   targeted message to the named owner. If Agent Mail is red, keep the
   generated Beads comment as the handoff contract and do not repair or restart
   Agent Mail.
5. Cite the proof-record JSONL path in the closeout only when
   `.proof_record.write_status == "written"`. A `refused` record is the correct
   fail-closed result for missing remote evidence, unsafe redaction, invalid
   local fallback, or incomplete artifacts.

The generated Beads comment has this shape:

```text
Proof-doctor handoff for <bead>: <status>. Verdict <verdict_id>; phase <phase>; reason <reason_code>; remote Cargo <reached|not reached>; RCH tool state <state>; owner <owner>; <safe to close from this verdict|closeout blocked by this verdict>. Command: `<argv>`. Affected paths: <paths>. Summary: <summary>. Next action: <next action>.
```

### 2.5 Status examples

Use these examples as closeout language, not as substitutes for the JSON
artifact. The exact verdict id, target dir, owner, command, and artifact paths
must come from the generated proof-doctor payload for the current run.

```text
Proof-doctor: passed; phase terminal_classified; reason proof.runnable; verdict /tmp/ft-abcd-proof-doctor.json; remote Cargo reached; owner none; target_dir /tmp/ft-abcd-test-target; target_lifecycle kept; target_size 6.2G; closeout safe.
Proof-record: written; path docs/attestations/proof-ledger/ft-abcd.jsonl; validation ok; closeout safe.
Meaning: the intended remote Cargo/test lane exited 0, retained artifacts are complete, redaction status is safe, and ledger validation allows the Bead to close green.
```

```text
Proof-doctor: source_blocked; phase terminal_classified; reason proof.source.remote_compile_error; verdict /tmp/ft-abcd-proof-doctor.json; remote Cargo reached; owner ft-lmg3g.6/MagentaFalcon; target_dir /tmp/ft-abcd-test-target; target_lifecycle kept; target_size unknown; closeout blocked.
Proof-record: written; path docs/attestations/proof-ledger/ft-abcd.jsonl; validation ok; closeout blocked.
Meaning: rustc reached first-party source on the remote worker and reported a source diagnostic. Handoff goes to the source owner; the proof Bead remains red.
```

```text
Proof-doctor: test_blocked; phase terminal_classified; reason proof.test.remote_assertion_failed; verdict /tmp/ft-abcd-proof-doctor.json; remote Cargo reached; owner ft-hme39.5/SageRobin; target_dir /tmp/ft-abcd-e2e-target; target_lifecycle kept; target_size unknown; closeout blocked.
Proof-record: written; path docs/attestations/proof-ledger/ft-abcd.jsonl; validation ok; closeout blocked.
Meaning: the intended test, bench, or E2E assertion started remotely and failed. Fix the behavior or harness before claiming pass.
```

```text
Proof-doctor: infra_blocked; phase launch_observed; reason proof.rch.pre_cargo_timeout_exec_missing; verdict /tmp/ft-abcd-proof-doctor.json; remote Cargo not reached; owner none; target_dir /tmp/ft-abcd-test-target; target_lifecycle cleanup_requested; target_size unknown; closeout blocked.
Proof-record: refused; path docs/attestations/proof-ledger/ft-abcd.jsonl; validation error; closeout blocked.
Meaning: RCH selected a worker or synced, but the wrapper failed before Cargo. This is pre-Cargo infrastructure, not a source verdict.
```

```text
Proof-doctor: infra_blocked; phase remote_cargo_observed; reason proof.artifact.required_log_missing; verdict /tmp/ft-abcd-proof-doctor.json; remote Cargo reached; owner none; target_dir /tmp/ft-abcd-test-target; target_lifecycle cleanup_requested; target_size unknown; closeout blocked.
Proof-record: refused; path docs/attestations/proof-ledger/ft-abcd.jsonl; validation error; closeout blocked.
Meaning: Cargo or the harness started, but worker substrate, timeout, artifact retrieval, or missing logs prevented complete evidence. Preserve what was reached and rerun with complete retention.
```

```text
Proof-doctor: dirty_tree_blocked; phase preflight; reason proof.dirty.active_owned_path_overlap; verdict /tmp/ft-abcd-proof-doctor.json; remote Cargo not reached; owner ft-1grhq.2/CoralBeaver; target_dir /tmp/ft-abcd-test-target; target_lifecycle not_applicable; target_size unknown; closeout blocked.
Proof-record: not_requested; path none; validation not_applicable; closeout blocked.
Meaning: a dirty tracked or untracked path overlaps the intended proof and belongs to another owner. Do not run over it or stage it.
```

```text
Proof-doctor: invalid; phase preflight; reason proof.command.local_cargo_invalid; verdict /tmp/ft-abcd-proof-doctor.json; remote Cargo not reached; owner current agent; target_dir none; target_lifecycle not_applicable; target_size unknown; closeout blocked.
Proof-record: refused; path docs/attestations/proof-ledger/ft-abcd.jsonl; validation error; closeout blocked.
Meaning: a local Cargo command, local fallback script, or fail-open wrapper was offered for an RCH-required proof lane. It can be local smoke only.
```

```text
Proof-doctor: skipped_not_proven; phase terminal_classified; reason proof.high_scale.predicate_absent; verdict /tmp/ft-abcd-proof-doctor.json; remote Cargo reached; owner none; target_dir /tmp/ft-abcd-high-scale-target; target_lifecycle kept; target_size unknown; closeout blocked.
Proof-record: written; path docs/attestations/proof-ledger/ft-abcd.jsonl; validation ok; closeout blocked.
Meaning: the reduced or skipped run may be useful evidence, but it did not satisfy the required 64+ CPU / 256 GiB predicate and cannot prove a target-class claim.
```

```text
Proof-doctor: inconclusive; phase evidence_gap; reason proof.rch.sync_not_proof; verdict /tmp/ft-abcd-proof-doctor.json; remote Cargo not reached; owner current agent; target_dir /tmp/ft-abcd-test-target; target_lifecycle cleanup_requested; target_size unknown; closeout blocked.
Proof-record: refused; path docs/attestations/proof-ledger/ft-abcd.jsonl; validation error; closeout blocked.
Meaning: retained logs show transfer, worker selection, or partial output, but not enough remote Cargo/rustc/test evidence to classify the lane. Rerun with fail-closed logging.
```

### 2.6 RCH target-dir lifecycle fields

Every RCH-heavy proof comment must account for the target directory it used.
Use these fields verbatim so later operators can grep for them:

```text
Target-dir lifecycle: CARGO_TARGET_DIR=/tmp/ft-<bead>-<purpose>-target; lifecycle=<kept|cleanup_requested|deletion_authorized|cleaned>; approx_size=<size|unknown>; deletion_authorized=<yes|no>; cleanup_note=<why it remains or what was removed>.
```

Allowed lifecycle values:

| Value | Meaning |
| --- | --- |
| `kept` | Target dir remains intentionally for incremental reuse. Include `approx_size` when known. |
| `cleanup_requested` | Operator should review inventory/dry-run output and decide whether deletion is authorized. |
| `deletion_authorized` | A human explicitly authorized deletion, but the agent has not performed it yet. Quote the authorization in the Beads comment. |
| `cleaned` | Cleanup already ran under explicit authorization. Include the command, time, and affected path. |
| `not_applicable` | No Cargo/RCH target dir was created by this bead. Use only for docs/static/non-Cargo work. |

Examples:

```text
Proof-doctor: passed; phase cargo-test; reason none; verdict /tmp/ft-abcd-test.log; remote Cargo reached; owner none; target_dir /tmp/ft-abcd-test-target; target_lifecycle kept; target_size 6.2G; closeout safe.
Target-dir lifecycle: CARGO_TARGET_DIR=/tmp/ft-abcd-test-target; lifecycle=kept; approx_size=6.2G; deletion_authorized=no; cleanup_note=kept for follow-up clippy reuse.
```

```text
Proof-doctor: infra_blocked; phase rch-wrapper; reason RCH-E127; verdict /tmp/ft-abcd-rch.log; remote Cargo not reached; owner none; target_dir /tmp/ft-abcd-test-target; target_lifecycle cleanup_requested; target_size unknown; closeout blocked.
Target-dir lifecycle: CARGO_TARGET_DIR=/tmp/ft-abcd-test-target; lifecycle=cleanup_requested; approx_size=unknown; deletion_authorized=no; cleanup_note=run scripts/clean-stale-targets.sh --inventory before requesting deletion.
```

Do not turn a lifecycle note into implicit deletion permission. The AGENTS.md
no-file-deletion rule still applies to `/tmp` target directories and `release`
subdirectories unless the user gives explicit written authorization.

### 2.7 Owned-file clippy attribution

When broad clippy is red from inherited workspace debt, use
`scripts/filter-clippy-owned-files.sh` to separate full-command status from
owned-file attribution. This helper is evidence extraction only. It cannot turn
a failed clippy command into a workspace-green claim.

Capture the full JSONL stream and original exit status:

```bash
set +e
rch exec -- env CARGO_TARGET_DIR=/tmp/<bead>-clippy-target \
  cargo clippy --no-deps -p <crate> --lib --message-format=json -- -D warnings \
  > /tmp/<bead>-clippy.jsonl
cargo_status=$?
set -e

scripts/filter-clippy-owned-files.sh \
  --cargo-status "$cargo_status" \
  --owned-file crates/frankenterm-core/src/color_management.rs \
  --owned-file crates/frankenterm-core/src/replay_fixture_harvest.rs \
  --input /tmp/<bead>-clippy.jsonl \
  --format json
```

Beads comment template:

```text
Clippy attribution: cargo_status=<status>; workspace_green=<true|false>; owned_error_count=<n>; owned_warning_count=<n>; attribution_verdict=<owned_files_clean|owned_non_error_diagnostics|owned_errors>; owned_files=<paths>. This is not a workspace-green substitute when cargo_status != 0.
```

Use `owned_files_clean` only to say the touched slice had no clippy diagnostics
in the retained JSONL stream. Keep the full command failure and first unrelated
diagnostic in the same Beads comment when `cargo_status != 0`.

### 2.8 Proof-surface adoption points

Use proof-doctor handoffs as the front door for proof closeouts in these
surfaces:

| Surface | Required proof-doctor adoption |
| --- | --- |
| Terminal conformance (`docs/terminal-conformance-contract.md`) | RCH-backed closeouts cite the generated handoff and proof-record JSONL before prose. `LOCAL_INVALID` and pre-Cargo infra states cannot close terminal rows. |
| Resource cockpit (`docs/resource-pressure-cockpit-contract.md`) | Worker-pool and target-class rows cite proof-doctor status plus proof-record artifacts; cockpit snapshots do not replace remote proof evidence. |
| Capture fairness (`docs/capture-fairness-slo-contract.md`) | Reduced and target-class runs map `remote_reduced`, `target_class`, and `docs_static` proof strength to proof-doctor status and high-scale predicate evidence. |
| Release attestations (`docs/attestations/README.md`, `docs/release/attestation-checklist.md`) | Attestation bundles include proof-record JSONL paths and generated handoff summaries for required verification categories. |
| Proof taxonomy (`docs/proposals/ft-tn6cw-proof-lane-evidence-taxonomy.md`) | New proof lanes reuse `ProofAttemptRecord` states and reason codes instead of inventing release-only labels. |

When updating these downstream docs or closing their Beads, replace ad-hoc
"RCH looked green" prose with the generated proof-doctor status, reason code,
phase, remote Cargo evidence, and proof-record write status.

### 2.9 Static stale-wording check

Run this check after editing operator, conformance, resource, fairness, or
attestation proof docs. It is intentionally wording-focused; it does not prove
Rust behavior.

```bash
BAD_SYNC='(sync completed|selected worker|workspace transfer)'
BAD_SYNC_CLAIM='(proved|proves|green|source pass|tests passed|closed green)'
BAD_LOCAL='(local Cargo|scripts/cargo-local[.]sh|cargo test [.]{3})'
BAD_REMOTE_CLAIM='(remote proof|RCH proof|proof lane passed|green claim|closeout safe)'
DOCS=(
  docs/operator-runbook.md
  docs/terminal-conformance-contract.md
  docs/resource-pressure-cockpit-contract.md
  docs/capture-fairness-slo-contract.md
  docs/attestations/README.md
  docs/release/attestation-checklist.md
  docs/proposals/ft-tn6cw-proof-lane-evidence-taxonomy.md
  docs/proposals/ft-wik9p-proof-doctor-verdict-schema.md
)
rg -n --pcre2 "${BAD_SYNC}.{0,80}${BAD_SYNC_CLAIM}|${BAD_SYNC_CLAIM}.{0,80}${BAD_SYNC}|${BAD_LOCAL}.{0,80}${BAD_REMOTE_CLAIM}|${BAD_REMOTE_CLAIM}.{0,80}${BAD_LOCAL}" "${DOCS[@]}"
```

The command should return no matches. If it flags a negative example that is
deliberately teaching the anti-pattern, rewrite that example so the bad claim
and the forbidden evidence are not on the same line, then keep the nearby prose
saying the claim is invalid.

---

## 2A. Operating-Envelope Admission Gate

The operating-envelope contract is the dry-run gate for deciding what kind of
swarm work is safe right now. Its v1 schema is
`docs/json-schema/ft-operating-envelope.json` and its source contract is
[`docs/robot-contracts/operating-envelope.md`](robot-contracts/operating-envelope.md).
The controller is read-only: it ranks admission windows, records why the
window is reduced, and lists forbidden action classes. It is never permission
to mutate panes, claim Beads, restart services, cancel RCH jobs, or substitute
local Cargo proof.

Current status: the contract, planner, and fixtures exist, but the operator,
robot, and MCP explain surfaces are still tracked by `ft-booek.4`. Until that
surface bead lands, use the existing fallback commands in this runbook and cite
the retained fixture or command artifact that most closely matches the live
state.

### 2A.1 Trust boundaries

Treat each input domain as evidence with a separate failure mode:

| Domain | Trust boundary |
| --- | --- |
| Beads/BV | Source of intended work, dependencies, assignees, and stale candidates. It is not proof that an owner is idle, that code is clean, or that a blocked Bead is safe to bypass. |
| Agent Mail | Coordination channel only. Unavailable or degraded mail is an envelope input, not an instruction to repair, restart, or kill the shared service. |
| RCH | Remote proof substrate. Worker selection, sync, and topology preflight are not source proof; closeout proof requires retained remote Cargo/rustc/test evidence for the intended lane. |
| Git | Local dirty-tree risk. Tracked dirty overlap blocks attribution; untracked files require review before claiming they are harmless. A clean `git status` is not a test result. |
| Pane inventory | Optional redacted metadata for occupancy and ownership. Raw pane content is forbidden by default and must not be captured to make an envelope more convenient. |
| Capacity/resource telemetry | Admission pressure signal. Missing, stale, contradictory, or privacy-redacted telemetry lowers the envelope instead of permitting more work by guess. |
| Proof artifacts | Durable evidence only when command, worker, target dir, exit code, redaction status, and remote Cargo/test reachability are retained. Prose summaries and RCH transfer chatter are secondary. |

### 2A.2 Safe workflows by envelope state

Use the fixture states as examples of the operator posture:

| State | Example artifact | Safe next action |
| --- | --- | --- |
| Normal green window | `fixtures/operating-envelope/valid/healthy.json` | Claim only unblocked Beads with clean or non-overlapping paths, run static checks, and run RCH proof if the Bead requires it. Forbidden actions still stay forbidden. |
| Agent Mail outage | `fixtures/operating-envelope/valid/agent-mail-unavailable.json` | Retry mail once, then continue Beads-only with `scripts/swarm-tick.sh --agent-mail-fallback frankenterm`; post Beads comments instead of attempting service remediation. |
| RCH proof outage | `fixtures/operating-envelope/valid/rch-no-worker.json` | Do docs/static work or wait. For proof Beads, record `infra_blocked` or the matching reason code; do not run local Cargo as closeout proof. |
| RCH topology failure | `fixtures/operating-envelope/valid/rch-topology-failure.json` | Preserve the pre-Cargo artifact and classify it as infrastructure. Do not repoint workers, restart daemons, or mutate remote mirrors without explicit operator authorization. |
| Dirty shared tree | `fixtures/operating-envelope/valid/dirty-overlap.json` | Wait, ask for ownership, or choose a non-overlapping docs/static slice. Do not run proof over another agent's dirty files. |
| Stale in-progress Bead | `scripts/swarm-tick.sh --agent-mail-fallback frankenterm` stale fields | Comment a status check first, refresh Beads and dirty-path state, and reopen only after ownership is stale and no overlap signal remains. |
| No ready Beads | `br ready --json` returns `[]` plus `bv --robot-triage` | Do not infer "no work." Use robot-mode `bv`, then pick a blocked-but-static planning/docs slice only if it does not pretend dependencies are complete. |
| Red or black pressure | capacity/resource source says red, black, stale, or unavailable | Pause admission, shed new proof lanes, and keep only read-only/status work. Escalate with the retained telemetry artifact, not a service restart. |

### 2A.3 Surface examples

These are the target operator shapes for `ft-booek.4`; do not present them as
live commands until that bead ships. The expected degraded fields are pinned by
the fixture JSON today.

Human surface:

```bash
ft swarm envelope --explain rch.no_workers_passed_health
```

Expected degraded summary:

```text
outcome=defer tier=red confidence=unavailable
permitted=read_status,add_beads_comment,wait
forbidden=claim_bead,edit_files,run_rch_proof,local_cargo_proof,service_restart,agent_mail_repair,worker_drain,build_cancellation,raw_pane_content
reason=rch.no_workers_passed_health
remote_cargo_reached=false
```

Robot surface:

```bash
ft robot --format toon swarm envelope
```

Expected degraded fields:

```text
contract_id: ft.operating_envelope.v1
decision:
  outcome: defer
  envelope_tier: red
  rch_proof_state: unavailable
  reason_codes: [rch.no_workers_passed_health, rch.remote_cargo_reached_false, local_cargo.forbidden]
admission_windows[0]:
  window_class: defer
  permitted_action_classes: [read_status, add_beads_comment, wait]
```

MCP surface:

```text
resource: wa://swarm/operating-envelope
tool: wa.swarm_envelope_explain {"reason_code":"dirty_overlap.present"}
```

Expected degraded fields:

```json
{
  "contract_id": "ft.operating_envelope.v1",
  "decision": {
    "outcome": "wait",
    "envelope_tier": "orange",
    "dirty_tree_state": "dirty_overlap",
    "reason_codes": ["dirty_overlap.present", "assignee_overlap.active", "envelope.wait"]
  }
}
```

### 2A.4 Artifact citation rules

Every Beads comment or release-attestation note that cites an
operating-envelope decision should include:

```text
Operating-envelope: contract ft.operating_envelope.v1; outcome <outcome>; tier <green|yellow|orange|red|black>; reason_codes <codes>; sources <beads,rch,agent_mail,git,capacity,robot>; artifact <path-or-command>; forbidden_actions <action_classes>; closeout <safe|blocked>.
```

For docs/static work, pair it with the non-proof closeout sentence from the
proof-doctor section:

```text
Proof-doctor: not applicable; docs-static change only; no Cargo/RCH proof lane claimed.
Target-dir lifecycle: not applicable; no Cargo/RCH target dir created.
```

For release attestations, cite the retained envelope artifact from the
producing Bead and keep it separate from the proof artifact. An envelope may
explain why a proof lane was deferred, but it does not prove the code under
test.

---

## 3. Tick #1 — establish baseline

The first tick sets the contract for the session. Skip steps and you
will pay 30+ minutes recovering shared context later.

1. **Snapshot pane state.**
   ```bash
   ntm --robot-snapshot -t frankenterm > /tmp/swarm-baseline.json
   ```
   This is your "before" picture; you will diff against it later when
   classifying panes.

2. **Read AGENTS.md sections RULE 0 / 0.5 / 1 / 2 + Swarm Orchestration Playbook.**
   These are the durable hard rules. If your dispatch contradicts any
   of them, fix the dispatch.

3. **Decide the tick cadence.** Default: 4 minutes. Faster is wasteful;
   slower lets stuck panes drift. Each tick spends ~30 seconds of your
   own context, so 4 minutes is the sweet spot.

4. **Dispatch initial marching orders.** Each pane gets:
   - Their slug (cc_1, cod_1, etc.) — used as `CARGO_TARGET_DIR=/tmp/ft-<slug>-target`
     to avoid lock contention.
   - The setup checklist (read AGENTS.md, identify slug, claim a
     ready bead, confirm rch usage, ship-or-surface within an hour).
   - A reminder that committed changes ship via `br close + sync +
     git push origin main`.

5. **Record session-start timestamp.** Used later for `commits-1h`
   computations during convergence detection.

---

## 4. Steady-state tick (the 4-minute loop)

Every 4 minutes, run this loop. Each step in order; the order matters.

### 4.1 — Run swarm-tick.sh

```bash
scripts/swarm-tick.sh frankenterm > /tmp/swarm-tick.json
```

This emits a compact JSON snapshot: per-pane state, recent commit
attribution, ready/in_progress bead counts, disk/usb-nvme percentages.
It acquires an operator lock so two concurrent operator scripts can't
corrupt shared state.

### 4.2 — Tail each pane

Read the last ~30 lines from each pane via `tmux capture-pane -p
-t S:0.N`. Don't skip this — `--robot-is-working` alone won't catch
the case where a pane is staring at a confirm prompt.

### 4.3 — Classify each pane

| State | Signal | Operator action |
|-------|--------|-----------------|
| WORKING | New commits in last tick OR active tool calls visible | Leave alone |
| IDLE | No tool calls, no new commits, but pane prompt shows agent UI ready | Send a fresh marching order |
| STUCK | Identical TOOL-OUTPUT lines for 2+ ticks AND no commits | Nudge with a status check; if still stuck after 1 tick, force-reset |
| DEAD | Pane fell back to bare zsh prompt | Relaunch agent (`tmux send-keys -t S:0.N "cc" Enter` or similar) |
| AUTH_FAILED | Auth-error in pane output | Rotate account via caam, then re-dispatch |

**Trap:** Codex panes show *idle-placeholder text* ("Find and fix a bug
in @", "Explain this codebase", etc.). That is **not** stuck. See
AGENTS.md Rule SO-3.

### 4.4 — Dispatch nudges to idle/stuck panes

```bash
ntm --robot-send -t frankenterm:0.N "your message here"
tmux send-keys -t frankenterm:0.N Enter
# For codex panes, second Enter ~2s later:
sleep 2 && tmux send-keys -t frankenterm:0.N Enter
```

Why two Enters for codex: see AGENTS.md Rule SO-2.

**Do not** use `ntm --robot-interrupt --interrupt-msg` for cooperative
nudges; it can crash codex panes (Rule SO-1).

### 4.5 — Reset stalled in_progress beads

Beads in_progress >2h with no commit linkage in the last 30 min are
candidates for force-release. Per AGENTS.md Rule SO-8:

1. Broadcast a status check to the assignee first:
   ```text
   "<slug>: ft-XXXX in_progress 2h+ with no commits. Commit and close,
    OR `br update --status open ft-XXXX --assignee=''`."
   ```
2. Wait one full tick.
3. If silent, force-release: `br update ft-XXXX --status open --assignee ''`.

Preserving agent autonomy first is what makes the broadcast work.
Force-releasing immediately erodes trust and wastes work-in-progress.

### 4.6 — Disk pressure check

If `swarm-tick.json` reports `disk_used_pct >= 96`:

```bash
scripts/clean-stale-targets.sh --inventory --threshold-hours 12
scripts/clean-stale-targets.sh --inventory --format json --threshold-hours 12
scripts/clean-stale-targets.sh --dry-run --threshold-hours 12
# Review the would-remove list; if it looks safe:
scripts/clean-stale-targets.sh --threshold-hours 12
```

The inventory commands are read-only. Use the text form for human review and
the JSON form in Beads comments when requesting deletion authorization; it
reports per-target age, size, active-skip status, and total reclaimable bytes.

Also nudge any agent whose bead is closed to clean their own
`/tmp/ft-<slug>-target/release` directory. Per AGENTS.md Rule SO-6,
keep the `debug` subdirectory for incremental rebuilds.

---

## 5. Mode transitions

A swarm session has four modes. The trigger for each transition is
listed below. Do not skip modes; each one prepares the swarm for the
next.

| Mode | Goal | Triggers transition out |
|------|------|-------------------------|
| **CLAIM** | Drain the ready queue | `br ready --json \| jq length` ≤ 2 |
| **REVIEW** | Catch defects in shipped beads, file follow-ons | All in_progress beads have commits in last 30m |
| **FINAL** | Push remaining in_progress to closed | ≥2 cc panes report "converged" |
| **DRAIN** | Wind down: clean disk, write summary, close session | (terminal mode) |

Reverse transitions are possible (e.g., REVIEW → CLAIM if a bug review
files 5 new P1 beads), but rare. Most sessions move forward
monotonically.

---

## 6. Recovery recipes

When a pane shows non-steady-state, match the symptom to the recipe.

### 6.1 — Pane shows bare zsh prompt

The agent process exited (typically codex after a bad
`--robot-interrupt`). Recover:

```bash
tmux send-keys -t frankenterm:0.N "cod" Enter   # or "cc"
sleep 1
tmux send-keys -t frankenterm:0.N Enter          # second Enter for codex
ntm --robot-send -t frankenterm:0.N "Re-dispatching: <original prompt>"
tmux send-keys -t frankenterm:0.N Enter
```

### 6.2 — Pane is rate-limited

The pane's tail shows "rate limited" or similar. Rotate via caam
(`caam list`, `caam switch <profile>`) and re-dispatch. Save the
rate-limited account for a future session.

### 6.3 — Context saturation

Pane's tail shows the agent referring to early-session context
ambiguously, or repeats the same approach despite explicit corrections.
Restart the pane:

```bash
tmux send-keys -t frankenterm:0.N C-d            # exit current agent
sleep 2
tmux send-keys -t frankenterm:0.N "cc" Enter     # relaunch
ntm --robot-send -t frankenterm:0.N "<setup prompt + current bead context>"
```

### 6.4 — Abandoned in_progress bead

Apply the broadcast-first protocol from §4.5. Do not reset the bead's
assignee until you have given the agent one full tick to respond.

### 6.5 — Beads DB busy / corrupted

`br` returns `database is busy` or `database disk image is malformed`:

- Busy: wait 10s, retry. The DB has internal locking that resolves under load.
- Malformed: per MEMORY.md `br-db-corruption`, fall back to `bv --robot-triage`
  for read-only triage. Do not attempt `br close` until the DB is rebuilt.

---

## 7. Convergence detection

Stop when *all four* hold for one full tick:

1. `commits-1h` ≤ 4 (loosened from the spec's `≤ 2`; see AGENTS.md Rule SO-5).
2. `br ready --json | jq length` == 0.
3. `br list --status=in_progress --json | jq '.issues | length'` ≤ 1
   (the surviving in_progress is allowed if it's an EPIC or an
   long-running strategic bead with active commit linkage).
4. ≥ 2 cc panes have replied "converged" or "converged." to a single
   CONFIRM nudge.

Codex panes will not generally reply "converged"; treat their idle-no-
defects state as implicit agreement (Rule SO-4).

**False-positive guard:** Before declaring convergence, verify the
last 30 minutes of commits include at least one bead-closure commit
(`chore(beads):`). If not, the swarm is stalled, not converged.

---

## 8. Wind-down — generating the SESSION SUMMARY

Once convergence is detected:

1. Write a SESSION SUMMARY note (markdown, in your operator scratch
   space — *not* committed). Include:
   - Tick count, session duration, distinct beads touched.
   - Beads opened, closed, and their priorities.
   - Anomalies: pane crashes, force-releases, disk events.
   - Lessons: any rule that fired but didn't fit cleanly.
2. Push the final beads commit and any pending operator-script
   commits.
3. Stop the tick loop.
4. Notify any human stakeholders if applicable.
5. Save the SESSION SUMMARY as a memory entry under `feedback_*` only
   if it contains a *new* lesson; otherwise the runbook covers it.

---

## 9. Anti-patterns

These are the failure modes most often observed. Avoid each one.

- **`--robot-interrupt --interrupt-msg "<text>"` for cooperative agents.**
  Crashes codex panes (Rule SO-1). Use `--robot-send` + tmux Enter.
- **Polling more often than every 4 minutes.** Burns operator context
  for no benefit. Agents need time between nudges to actually work.
- **Trusting `--robot-is-working` alone for stuck-pane detection.**
  Always tail the pane content; idle-placeholder text fools the
  working-bit (Rule SO-3).
- **Force-releasing in_progress beads at a 2h cutoff without a broadcast.**
  Erodes agent autonomy, wastes work-in-progress (Rule SO-8).
- **Skipping per-agent `CARGO_TARGET_DIR`.** Concurrent agents stomp
  each other's lock files; build times balloon.
- **Sending the same nudge text via `ntm send` rather than `--robot-send`.**
  CASS dedup blocks repeats; the agent never sees the second nudge.
- **Committing ad-hoc operator decisions to `MEMORY.md`.** Memory is
  for durable, generalizable lessons. Tick-specific notes belong in
  the SESSION SUMMARY only.

---

## Cross-references

- **AGENTS.md** — Rule 0, 0.5, 1, 2, and the **Swarm Orchestration
  Playbook** section (Rules SO-1 through SO-8). The runbook applies
  those rules; it does not redefine them.
- **`vibing-with-ntm` skill** — operator-tick playbook with concrete
  command sequences for individual primitives.
- **`ntm` skill** — primitive reference for `--robot-send` /
  `--robot-interrupt` / `send`.
- **`scripts/swarm-tick.sh`** — operator script run every tick;
  emits the JSON snapshot this runbook consumes.
- **`scripts/clean-stale-targets.sh`** — disk-pressure relief.
- **`scripts/memory_staleness_check.py`** — monthly memory hygiene.

If anything in this runbook contradicts AGENTS.md, AGENTS.md wins —
file a bead to reconcile.
