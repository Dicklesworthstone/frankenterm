# Robot Mode ↔ MCP Surface Matrix

**Beads:** `ft-og9pc`.
**Enforced by:** `crates/frankenterm-core/tests/mcp_robot_surface_matrix.rs`.

MCP does **not** mirror Robot Mode one-to-one. This matrix is the scoped
guarantee: every `ft robot` family (`RobotCommands` in
`crates/frankenterm/src/main.rs`) and every registered MCP tool (the golden
`crates/frankenterm-core/tests/fixtures/mcp_manifest.json`, itself checked
against the live server) appears here exactly once. Adding, removing, or
renaming a family or tool without updating this table fails the test.

Parity levels:

- `mirrored` — every Robot verb in the family has an MCP tool with the same
  payload semantics (per-tool capability notes in `docs/mcp-api-spec.md` still
  apply).
- `partial` — some verbs have MCP tools; the rest are named in the notes.
- `robot-only` — no MCP tool. MCP callers must not assume one exists.
- `mcp-only` — an MCP tool with no `ft robot` family.

MCP policy column: `mutation gate` means the tool calls
`mcp_authorize_mcp_mutation` (policy deny/approval plus a denial record);
`policy gate` means a direct `PolicyEngine::authorize` call in the handler;
`read` means no mutation. Robot-side gates are not restated here.

| Robot family | Robot command / verbs | MCP tools | Parity | MCP policy / notes |
|---|---|---|---|---|
| `Scan` | `scan` | — | robot-only | Discovery helper; MCP clients use `tools/list`. |
| `Help` | `help` | — | robot-only | Discovery helper; MCP clients use `tools/list`. |
| `QuickStart` | `quick-start` | — | robot-only | Discovery helper. |
| `State` | `state` | `wa.state` | mirrored | read |
| `Gui` | `gui` | — | robot-only | GUI socket inspection is local-operator only. |
| `GetText` | `get-text` | `wa.get_text` | mirrored | policy gate (read) |
| `Dom` | `dom` | `wa.dom` | mirrored | read |
| `EvaluateAnomaly` | `evaluate-anomaly` | — | robot-only | |
| `Send` | `send` | `wa.send` | mirrored | policy gate; `PolicyGatedInjector` records `audit_actions` |
| `WaitFor` | `wait-for` | `wa.wait_for` | mirrored | read |
| `Search` | `search` | `wa.search` | mirrored | policy gate (read) |
| `SearchExplain` | `search-explain` | — | robot-only | |
| `SearchIndex` | `search-index` | — | robot-only | Index maintenance is operator-side. |
| `Cass` | `search`, `view`, `status` | `wa.cass_search`, `wa.cass_view`, `wa.cass_status` | mirrored | read |
| `Events` | `events`, `annotate`, `triage`, `label` | `wa.events`, `wa.events_annotate`, `wa.events_triage`, `wa.events_label` | partial | Annotate/triage/label: mutation gate. `wa.events` is a non-resumable snapshot; Robot cursor modes have no MCP form. |
| `WatchEvents` | `watch-events` | — | robot-only | Streaming; MCP callers use `wa.await_event`. |
| `Await` | `await` | `wa.await_event` | partial | Storage-derived quiescence is CLI-only. |
| `Workflow` | `run`, `list`, `status`, `abort` | `wa.workflow_run`, `wa.workflow_status` | partial | Run: policy gate with approval tokens. `list` and `abort` are robot-only. |
| `Rules` | `list`, `test`, `show`, `lint` | `wa.rules_list`, `wa.rules_test` | partial | `show` and `lint` are robot-only. |
| `Why` | `why` | — | robot-only | |
| `Agents` | `agents` | — | robot-only | |
| `SessionResume` | `session-resume` | — | robot-only | |
| `Accounts` | `list`, `refresh` | `wa.accounts`, `wa.accounts_refresh` | mirrored | Refresh: policy gate. |
| `Reservations` | `reserve`, `release`, `list` | `wa.reserve`, `wa.release`, `wa.reservations` | mirrored | Reserve/release: policy gate. |
| `Mission` | `objective-plan`, `state`, `decisions`, `pause`, `resume`, `abort` | `wa.mission_objective_plan`, `wa.mission_state`, `wa.mission_explain`, `wa.mission_pause`, `wa.mission_resume`, `wa.mission_abort` | partial | Pause/resume/abort: mutation gate on MCP; Robot verbs pass the Robot policy gate (denials audited) and share the transition + commit path with human `ft mission pause/resume/abort`. `wa.mission_explain` returns transitions and failure catalog, not Robot `decisions` payloads. |
| `Tx` | `plan`, `run`, `rollback`, `show` | `wa.tx_plan`, `wa.tx_run`, `wa.tx_rollback`, `wa.tx_show` | mirrored | Run/rollback: mutation gate. |
| `Health` | `health` | — | robot-only | |
| `Limits` | `limits` | — | robot-only | |
| `Capacity` | `capacity` | — | robot-only | |
| `Swarm` | `envelope` | `wa.operating_envelope` | mirrored | read |
| `SwarmCapacity` | `swarm-capacity` | — | robot-only | |
| `HerdWave` | `herd-wave` | — | robot-only | |
| `CoordinationRisk` | `coordination-risk` | — | robot-only | |
| `AgentMailOutbox` | `agent-mail-outbox` | — | robot-only | |
| `BlockerRadar` | `blocker-radar` | — | robot-only | |
| `Attention` | `attention` | `wa.attention` | mirrored | read |
| `Rehearsal` | `score`, `explain` | `wa.rehearsal_score` | partial | `explain` is robot-only. |
| `Incidents` | `incidents` | — | robot-only | |
| `Resource` | `resource` | — | robot-only | |
| `Perf` | `perf` | — | robot-only | |
| `Approve` | `approve` | — | robot-only | |
| `Checkpoint` | `checkpoint` | — | robot-only | |
| `Context` | `context` | — | robot-only | |
| `Work` | `claim`, `release`, `complete`, `list`, `ready`, `assign`, `define` | — | robot-only | |
| `Fleet` | `status`, `scale`, `rebalance`, `agents` | — | robot-only | Intentional: scale/rebalance spawn and stop agents; an MCP agent must not resize its own fleet without an approval-token path. |
| `Profile` | `list`, `show`, `apply`, `validate`, `create` | — | robot-only | Intentional: `apply`/`create` provision fleets (same reason as `Fleet`). |
| `Connector` | `connector` | — | robot-only | |
| `KillSwitch` | `kill-switch` | — | robot-only | |
| `ProofDoctor` | `proof-doctor` | — | robot-only | |
| `ProofCloseoutLint` | `proof-closeout-lint` | — | robot-only | |
| `ProofHistory` | `proof-history` | — | robot-only | |
| `Proof` | `proof` | — | robot-only | |
| — | — | `wa.steer_plan` | mcp-only | read; counterpart is the human `ft steer plan`. |

## Mission lifecycle

`ft robot mission pause|resume|abort` (`ft-oi92j`) closed the former gap where
only MCP and the human CLI could mutate mission lifecycle. MCP applies the
single canonical transition; Robot and the human CLI apply the same
multi-step transition plan (for example, resume from `retry_pending` goes
through `dispatching`).
