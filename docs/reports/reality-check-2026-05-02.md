# Reality-Check Drumbeat — 2026-05-02

_Generated 2026-05-02T07:05:21Z by [`scripts/reality-check-status.sh`](../../scripts/reality-check-status.sh)._
_Bead: `ft-at08r` (BR-RC-WEEKLY-DRUMBEAT)._

## Headline rollup

| Status | Count |
| ------ | ----- |
| open | 4 |
| in_progress | 1 |
| closed | 46 |
| **total** | **51** |

## By epic

### BR-RC-ATTESTATION-CLOSURE — 0 open / 0 in_progress / 1 closed (1 total)

| Status | ID | Title |
| ------ | -- | ----- |
| closed | ft-187kv | [BR-RC-ATTESTATION-CLOSURE] Per-epic closer beads — verify attestation bundle complete & signed |

### BR-RC-CUTOVERS — 4 open / 0 in_progress / 2 closed (6 total)

| Status | ID | Title |
| ------ | -- | ----- |
| closed | ft-35yac.1 | [BR-RC-CUTOVERS.G5.1] Differential render oracle harness for ftui vs ratatui parity |
| closed | ft-35yac.3 | [BR-RC-CUTOVERS.G7] Replace single unimplemented!() in mux-server-impl/sessionhandler.rs:1731 |
| open | ft-35yac | [BR-RC-CUTOVERS] Reality-check Cutovers epic — finish ftui migration + clear single sessionhandler stub |
| open | ft-35yac.1.1 | [BR-RC-CUTOVERS.G5.1.1] Record TUI parity test corpus from real user sessions |
| open | ft-35yac.1.2 | [BR-RC-CUTOVERS.G5.1.2] Headless GPU-renderer parity test (visual regression catch) |
| open | ft-35yac.2 | [BR-RC-CUTOVERS.G5.2] Default ftui in shipped binaries; quarantine ratatui as tui-oracle dev-feature |

### BR-RC-DEMO — 0 open / 0 in_progress / 1 closed (1 total)

| Status | ID | Title |
| ------ | -- | ----- |
| closed | ft-jjvxg | [BR-RC-DEMO] Reality-check live demo recording |

### BR-RC-DOCTRINE — 0 open / 0 in_progress / 7 closed (7 total)

| Status | ID | Title |
| ------ | -- | ----- |
| closed | ft-i2eni | [BR-RC-DOCTRINE] Reality-check Doctrine epic — make code+docs match stated doctrine |
| closed | ft-i2eni.1 | [BR-RC-DOCTRINE.G1.1] RuntimeProof sealed trait + tokio type-level seal |
| closed | ft-i2eni.2 | [BR-RC-DOCTRINE.G1.2] asupersync_test! proc-macro replacing 60 #[tokio::test] sites |
| closed | ft-i2eni.3 | [BR-RC-DOCTRINE.G1.3] cargo-deny ban on tokio in first-party crates |
| closed | ft-i2eni.4 | [BR-RC-DOCTRINE.G4] Rename 4 vendored wezterm-* crates to frankenterm-* |
| closed | ft-i2eni.5 | [BR-RC-DOCTRINE.G6] Auto-stamp README/AGENTS counts via build-time queries |
| closed | ft-i2eni.6 | [BR-RC-DOCTRINE.G15] Vendored fork provenance manifest |

### BR-RC-FOUNDATION — 0 open / 0 in_progress / 10 closed (10 total)

| Status | ID | Title |
| ------ | -- | ----- |
| closed | ft-rq13w | [BR-RC-FOUNDATION.G3.4.cont] Latency-derivation doc + latency_stages.rs wiring + empirical-vs-bound cross-check + attestation publishing |
| closed | ft-syqcz | [BR-RC-FOUNDATION] Reality-check Foundation epic — release attestation graph + Loom proofs |
| closed | ft-syqcz.1 | [BR-RC-FOUNDATION.G3.1] Define attestation graph schema + signing pipeline |
| closed | ft-syqcz.1.1 | [BR-RC-FOUNDATION.G3.1.1] ft attestation verify CLI command — user-facing offline verification |
| closed | ft-syqcz.2 | [BR-RC-FOUNDATION.G3.2] Bench harness statistical-rigor uplift (sequential testing + distributions) |
| closed | ft-syqcz.3 | [BR-RC-FOUNDATION.G3.3] Headline-claim manifest + 5 must-prove benches |
| closed | ft-syqcz.4 | [BR-RC-FOUNDATION.G3.4] Network-calculus latency derivation linking latency_stages.rs to headline claim |
| closed | ft-syqcz.5 | [BR-RC-FOUNDATION.G3.5] Differential bench matrix vs WezTerm/Zellij/Ghostty |
| closed | ft-syqcz.6 | [BR-RC-FOUNDATION.G8.1] Loom dev-dep + harness skeleton |
| closed | ft-syqcz.7 | [BR-RC-FOUNDATION.G8.2] Loom proofs for every runtime_async primitive + Mazurkiewicz trace doc |

### BR-RC-METHODOLOGY-PLAYBOOK — 0 open / 0 in_progress / 1 closed (1 total)

| Status | ID | Title |
| ------ | -- | ----- |
| closed | ft-2fctn | [BR-RC-METHODOLOGY-PLAYBOOK] Methodology playbook — how to use Loom/TLA+/Stateright/dylint/cargo-deny in this repo |

### BR-RC-ROBOT-CONTRACT — 0 open / 0 in_progress / 8 closed (8 total)

| Status | ID | Title |
| ------ | -- | ----- |
| closed | ft-hac7w | [BR-RC-ROBOT-CONTRACT] Reality-check Robot Family Closure epic — contract-driven closure of 5 not_implemented families |
| closed | ft-hac7w.1 | [BR-RC-ROBOT-CONTRACT.0] Schema-driven contract infrastructure (proptest+OpenAPI generator) |
| closed | ft-hac7w.1.1 | [BR-RC-ROBOT-CONTRACT.0.1] ntm differential test harness for robot family parity |
| closed | ft-hac7w.2 | [BR-RC-ROBOT-CONTRACT.1] Family: profile (show/list/set) — easiest, ship-first proof of methodology |
| closed | ft-hac7w.3 | [BR-RC-ROBOT-CONTRACT.2] Family: checkpoint (save/rollback/list) — wire into existing snapshot machinery |
| closed | ft-hac7w.4 | [BR-RC-ROBOT-CONTRACT.3] Family: context (status/rotate/history) — integrate cass + session-resume |
| closed | ft-hac7w.5 | [BR-RC-ROBOT-CONTRACT.4] Family: work (claim/complete/status/list) — Stateright-proven queue |
| closed | ft-hac7w.6 | [BR-RC-ROBOT-CONTRACT.5] Family: fleet (status/launch/stop/describe) — surface frankenterm-core-fleet through TX engine |

### BR-RC-RUNTIME-SEMANTICS — 0 open / 0 in_progress / 4 closed (4 total)

| Status | ID | Title |
| ------ | -- | ----- |
| closed | ft-t9a6q | [BR-RC-RUNTIME-SEMANTICS] Reality-check Runtime Semantics epic — finish Cx-first migration |
| closed | ft-t9a6q.1 | [BR-RC-RUNTIME-SEMANTICS.G14.1] Custom dylint flagging async fns missing &Cx |
| closed | ft-t9a6q.2 | [BR-RC-RUNTIME-SEMANTICS.G14.2] Cx-propagation burn-down dashboard + sprint |
| closed | ft-t9a6q.3 | [BR-RC-RUNTIME-SEMANTICS.G14.0] LabRuntime virtual-time test framework infra |

### BR-RC-SAFETY-PROOFS — 0 open / 0 in_progress / 6 closed (6 total)

| Status | ID | Title |
| ------ | -- | ----- |
| closed | ft-x0666 | [BR-RC-SAFETY-PROOFS] Reality-check Safety Proofs epic — turn security claims into signed attestations |
| closed | ft-x0666.1 | [BR-RC-SAFETY-PROOFS.G9] Passive-watch read-only fuzz proof |
| closed | ft-x0666.2 | [BR-RC-SAFETY-PROOFS.G10] Secret redactor recall/precision matrix vs gitleaks/trufflehog corpora |
| closed | ft-x0666.3 | [BR-RC-SAFETY-PROOFS.G11] Distributed wire protocol threat model + diff fuzz + Stateright dedup |
| closed | ft-x0666.4 | [BR-RC-SAFETY-PROOFS.G13] Mission/TX kill-switch state-space proof (TLA+ + Stateright) |
| closed | ft-x0666.5 | [BR-RC-SAFETY-PROOFS.G11.1] Reed-Solomon erasure encoding spec for cross-host audit ledger |

### BR-RC-WEEKLY-DRUMBEAT — 0 open / 1 in_progress / 0 closed (1 total)

| Status | ID | Title |
| ------ | -- | ----- |
| in_progress | ft-at08r | [BR-RC-WEEKLY-DRUMBEAT] Weekly reality-check progress drumbeat report |

### uncategorized — 0 open / 0 in_progress / 6 closed (6 total)

| Status | ID | Title |
| ------ | -- | ----- |
| closed | ft-2guux | reality-check gaps |
| closed | ft-4ackg | [MEDIUM] docs/integration-guide.md typed-client list is frozen at earlier CLI — missing Mission/Tx/Accounts/Reservations/Agents/Health/SearchIndex/Cass data types |
| closed | ft-e9ue4 | [MEDIUM] docs/ contains 40+ uncanon'd COMPREHENSIVE_ANALYSIS_OF_*.md / PLAN_TO_DEEPLY_INTEGRATE_* files with no freshness/aspirational marker |
| closed | ft-j3ayu | [LOW] Top-level repo clutter not in AGENTS.md workspace structure / .gitignore (test_*.rs, ubs_*.txt, storage.sqlite3*, clippy_output.json) |
| closed | ft-v2ifl | [HIGH] AGENTS.md does not disclose NTM-gap command families (checkpoint/context/work/fleet/profile all return robot.not_implemented) |
| closed | ft-yllta | [MEDIUM] ft robot agents silently feature-gated behind 'agent-detection' — --no-default-features builds get robot.feature_not_available |

## Attestation status

The release attestation manifest at
[`docs/attestations/manifest.json`](../attestations/manifest.json)
declares one slot per reality-check claim. A slot with a non-null
`path` means the producing bead has shipped its artifact and the
release-bundle build picks it up; `_pending_` means the bead is
still open.

| Category | Producing bead | Artifact |
| -------- | -------------- | -------- |
| perf/headline-claims | ft-syqcz.3 | _pending_ |
| perf/competitor-matrix | ft-syqcz.4 | _pending_ |
| perf/lindley-bounds | ft-syqcz.5 | _pending_ |
| tui/render-parity | ft-35yac.2 | _pending_ |
| security/passive-watch | ft-x0666.1 | _pending_ |
| security/redactor-coverage | ft-x0666.2 | _pending_ |
| security/distributed-threat-model | ft-x0666.3 | _pending_ |
| proofs/loom-runtime-async | ft-syqcz.6 | _pending_ |
| proofs/runtime-proof-trait | ft-i2eni.1 | _pending_ |
| proofs/robot-contracts | ft-hac7w.1 | _pending_ |
| doctrine/agents-md-counts | ft-i2eni.5 | _pending_ |
| doctrine/vendored-provenance | ft-i2eni.6 | [present](frankenterm/PROVENANCE.json) |
| doctrine/cx-propagation | ft-q0tz3 | [present](docs/runtime/cx-propagation.json) |

## Cross-references

- Bead board (open RC work): `br list --label reality-check --status=open`
- Bridge plan: [`docs/reality-check-bridge-plan.md`](../reality-check-bridge-plan.md)
- Attestation schema + verifier: [`docs/attestations/schema.json`](../attestations/schema.json), [`scripts/attestation-verify.sh`](../../scripts/attestation-verify.sh)
- Cadence: weekly via `.github/workflows/reality-check-drumbeat.yml`
