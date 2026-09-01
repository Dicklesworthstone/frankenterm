# Reality-Check Drumbeat — 2026-09-01

_Generated 2026-09-01T19:27:22Z by [`scripts/reality-check-status.sh`](../../scripts/reality-check-status.sh)._
_Bead: `ft-at08r` (BR-RC-WEEKLY-DRUMBEAT)._

## Headline rollup

| Status | Count |
| ------ | ----- |
| open | 36 |
| in_progress | 19 |
| closed | 206 |
| **total** | **261** |

## By epic

### BR-RC-ATTESTATION-CLOSURE — 0 open / 0 in_progress / 1 closed (1 total)

| Status | ID | Title |
| ------ | -- | ----- |
| closed | ft-187kv | [BR-RC-ATTESTATION-CLOSURE] Per-epic closer beads — verify attestation bundle complete & signed |

### BR-RC-CUTOVERS — 0 open / 0 in_progress / 6 closed (6 total)

| Status | ID | Title |
| ------ | -- | ----- |
| closed | ft-35yac | [BR-RC-CUTOVERS] Reality-check Cutovers epic — finish ftui migration + clear single sessionhandler stub |
| closed | ft-35yac.1 | [BR-RC-CUTOVERS.G5.1] Differential render oracle harness for ftui vs ratatui parity |
| closed | ft-35yac.1.1 | [BR-RC-CUTOVERS.G5.1.1] Record TUI parity test corpus from real user sessions |
| closed | ft-35yac.1.2 | [BR-RC-CUTOVERS.G5.1.2] Headless GPU-renderer parity test (visual regression catch) |
| closed | ft-35yac.2 | [BR-RC-CUTOVERS.G5.2] Default ftui in shipped binaries; quarantine ratatui as tui-oracle dev-feature |
| closed | ft-35yac.3 | [BR-RC-CUTOVERS.G7] Replace single unimplemented!() in mux-server-impl/sessionhandler.rs:1731 |

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

### BR-RC-WEEKLY-DRUMBEAT — 0 open / 0 in_progress / 1 closed (1 total)

| Status | ID | Title |
| ------ | -- | ----- |
| closed | ft-at08r | [BR-RC-WEEKLY-DRUMBEAT] Weekly reality-check progress drumbeat report |

### uncategorized — 36 open / 19 in_progress / 161 closed (216 total)

| Status | ID | Title |
| ------ | -- | ----- |
| closed | ft-023t1 | Search daemon accepts unbounded hash embedding dimensions |
| closed | ft-10i8s | web/extractors::redact_json_value can stack-overflow on deeply nested JSON |
| closed | ft-1b0rn | [docs][README] workspace stats stale: claims 13 sub-crates (actual 17), 349 core modules (actual 496) |
| closed | ft-1o34x | Search daemon rebuilds FastEmbed model on every request |
| closed | ft-1wy8d | mcp proxy destructive counter tests race on global reset |
| closed | ft-2guux | reality-check gaps |
| closed | ft-3aiu5 | [mock-code-finder][tantivy] Post-filters run after capped TopDocs and corrupt results/counts |
| closed | ft-3e8mv | MissionJournal::compact_before drops correlation_index → post-compaction duplicate correlation IDs re-accepted |
| closed | ft-4ackg | [MEDIUM] docs/integration-guide.md typed-client list is frozen at earlier CLI — missing Mission/Tx/Accounts/Reservations/Agents/Health/SearchIndex/Cass data types |
| closed | ft-4hm95 | Wire WASM scripting _start stdout capture or remove the advertised path |
| closed | ft-4x6de | fleet_launcher::allocate_weighted u32 weight sum overflows on hostile AgentMix |
| closed | ft-58osz | MCP bridge strict no-db error points at private API |
| closed | ft-5eqd4 | [reality-check] Reconcile README policy "21-subsystem" claim with actual diagnostics enumeration |
| closed | ft-5eqd4.1 | [reality-check][policy-21] Subtask 1: Audit "21" provenance and produce reconciliation matrix |
| closed | ft-5eqd4.2 | [reality-check][policy-21] Subtask 2: Decide reconciliation strategy and ship ADR |
| closed | ft-5eqd4.3 | [reality-check][policy-21] Subtask 3: Apply the reconciliation per ADR |
| closed | ft-5eqd4.4 | [reality-check][policy-21] Subtask 4: CI regression guard pinning README claim to subsystem count |
| closed | ft-5eqd4.5 | [reality-check][policy-21] Subtask 5: Epic convergence harness — definition-of-done verification |
| closed | ft-69gwh.9 | [unwired] rch_admission live collector never populated in production; diagnosis surface has no real inputs |
| closed | ft-6h1rv | MCP mutation RequireApproval path returns tokenless dead end |
| closed | ft-6n7hs | [reality-check][runtime] fleet pressure worst_budget hardcoded to BudgetLevel::Normal — per-pane memory budget never drives scrollback eviction |
| closed | ft-71dap | MCP tests still expect removed build_server_with_db(None) path |
| closed | ft-7a9pd | canary rollout health gates treat NaN metrics as healthy |
| closed | ft-7dk63 | tx_execution dispatches commit/compensation side effects BEFORE writing cross-instance dedup record (crash window double-applies) |
| closed | ft-7h5da.5.10 | Connector outbound bridge is not wired to production dispatch |
| closed | ft-7h5da.5.11 | Connector lifecycle and mesh managers have no production operation path |
| closed | ft-7h5da.5.9 | Connector inbound bridge has no production caller |
| closed | ft-94cdu | [Audit] crash.rs silently drops corrupted crash bundle manifest+report serde failures |
| closed | ft-9dy83 | [test][regression-guard] pin wired-but-inert class — production-callsite guard for FilteredEventStream / kill-switch tiers / retention size-cap + tiers config (RetentionManager=ft-y7x56 + segment_embeddings=ft-xx5cl still UNWIRED) |
| closed | ft-a58cq | Validate extension manifest names before filesystem joins |
| closed | ft-ao0i5 | canary cohort selection accepts invalid fractions and stale allowlist agents |
| closed | ft-bhm8r | Planner runtime config lets invalid extraction/scorer floats reach ranking |
| closed | ft-bkn2g | [test][security] consolidated session security-conformance suite — kill-switch tiers, read-path redaction, capture/persist redaction, connector classifier gate, WASM sandbox, extension path-traversal (ft-l59nq/ft-ps9fu/ft-pfton/ft-cdxrr/ft-0dki4/ft-a58cq) |
| closed | ft-bu09o | mcp proxy over-reports mounted tools when remote names collide |
| closed | ft-cnxnr | Replay checkpointer resumes foreign or stale-schema checkpoints |
| closed | ft-crpvd | recording.rs and wire_protocol.rs epoch_ms_now silently substitute 0 on clock-before-1970 |
| closed | ft-diccw | FleetMemoryConfig accepts escalation_threshold=0 which silently disables tier escalation |
| closed | ft-doivv | [docs][AGENTS.md] Not-Yet-Implemented table claims ft robot profile dispatches NTM-not-implemented; actually has real handler (ft-b0g7g) |
| closed | ft-e87u6.1 | [reality-check][attest] Subtask 1: Per-slot reconciliation pass — locate every null-slot artifact |
| closed | ft-e87u6.10 | [reality-check][attest][recovery] Publish full TUI render-parity JSON artifact |
| closed | ft-e87u6.11 | [reality-check][attest][recovery] Publish passive-watch JSON attestation artifact |
| closed | ft-e87u6.12 | [reality-check][attest][recovery] Publish runtime_async Loom JSON attestation artifact |
| closed | ft-e87u6.13 | [reality-check][attest][recovery] Publish RuntimeProof seal JSON attestation artifact |
| closed | ft-e87u6.14 | [reality-check][attest][recovery] Publish auto-stamped counts JSON attestation artifact |
| closed | ft-e87u6.15 | [reality-check][attest][wiring] Wire 4 closed-recovery artifacts into manifest paths |
| closed | ft-e87u6.2 | [reality-check][attest] Subtask 2: Update manifest.json + schema for deferred-slot semantics |
| closed | ft-e87u6.3 | [reality-check][attest] Subtask 3: Build/verify round-trip + RCH-attested artifact bundle |
| closed | ft-e87u6.4 | [reality-check][attest] Subtask 4: Lift README + AGENTS.md hedge text and pin manifest cross-references |
| closed | ft-e87u6.5 | [reality-check][attest] Subtask 5: Release-build regression test for manifest sloppiness |
| closed | ft-e87u6.6 | [reality-check][attest] Subtask 6: Extend closing-checklist for future attestation-producing beads |
| closed | ft-e87u6.7 | [reality-check][attest] Subtask 7: Cross-link "Why Use ft?" rows to verified-numbers artifacts |
| closed | ft-e87u6.9 | [reality-check][attest][recovery] Publish perf competitor-matrix JSON artifact |
| closed | ft-e9ue4 | [MEDIUM] docs/ contains 40+ uncanon'd COMPREHENSIVE_ANALYSIS_OF_*.md / PLAN_TO_DEEPLY_INTEGRATE_* files with no freshness/aspirational marker |
| closed | ft-efxr6 | Planner anti-thrash governor is resolved but not wired into mission loop |
| closed | ft-eljxp | MCP bridge composes remote proxy tools in no-db degraded mode without audit |
| closed | ft-f1vcd | [docs][AGENTS.md] sub-crate cluster module counts stale: ARS claims 15 modules (actual 16), replay claims 24 (actual 25) |
| closed | ft-fkxin | Tantivy quality InTopN assertion accepts n=0 which is unsatisfiable |
| closed | ft-gmt1c | mcp proxy mutating opt-in admits unknown annotation shapes |
| closed | ft-gweu3 | Mission loop pin overrides bypass excluded-agent safety |
| closed | ft-gzhv3 | Search daemon client leaks blocking transport workers after timeout |
| closed | ft-i4brq | Mission loop active operator overrides are unbounded |
| closed | ft-interactive-systems-performance-4tenz.4.1 | [soak] Audit and reconcile existing long-haul, leak, load-rig, and GUI-soak substrates |
| closed | ft-interactive-systems-performance-4tenz.6.1 | [snapshot] Reconcile closed snapshot, triple-buffer, dirty-line, quad, and GPU-delta claims with live call graph |
| closed | ft-interactive-systems-performance-4tenz.8.1 | [render] Reconcile closed display-link, VRR, dedup, Metal, adaptive-FPS, and telemetry claims with live call graph |
| closed | ft-j3ayu | [LOW] Top-level repo clutter not in AGENTS.md workspace structure / .gitignore (test_*.rs, ubs_*.txt, storage.sqlite3*, clippy_output.json) |
| closed | ft-jb5l7 | MCP degraded skipped-entry telemetry is hardcoded and already contradicts registered surface |
| closed | ft-kske1 | MCP bridge degraded counter tests race through global reset |
| closed | ft-l18f1 | [mock-code-finder][tantivy] Indexer checkpoints skipped/rejected events as if indexed |
| closed | ft-lqj5g | [Audit] storage.rs workflow_executions row reader silently drops 3 serde failures per row |
| closed | ft-m5bne | Tantivy SearchFilter range filters silently match nothing on misordered min > max |
| closed | ft-nj0mq | solve_assignments drops candidates after max_assignments |
| closed | ft-oxjlo | ReplayScheduler resume accepts impossible decision checkpoints |
| closed | ft-p4y8d | MCP degraded bridge still exposes mutating mission/tx tools without audited wrapper |
| closed | ft-pgwv9 | [mock-code-finder][search] Indexing pipeline drops ingest errors into normal reports |
| closed | ft-qv48g | [docs][robot-contracts] Refresh reality-check bridge robot family action matrix |
| closed | ft-r3d4e | [Audit] backup.rs silently drops corrupted backup manifest serde failures |
| closed | ft-se2ep | ReplaySession ordering ignores pane/event tie-breakers |
| closed | ft-sr5pq | mcp proxy safe default allows readOnly=false mutating tools |
| closed | ft-tf6g3.10 | [reality-check-2026-05-12 G25] SPRT (always-valid) regression gating for headline-claim metrics |
| closed | ft-tf6g3.11 | [reality-check-2026-05-12 G26] Conformal-prediction SLO bands for headline metrics |
| closed | ft-tf6g3.12 | [reality-check-2026-05-12 G27] TLA+ TX-killswitch spec + TLC model check |
| closed | ft-tf6g3.13 | [reality-check-2026-05-12 G28] Stateright model for robot work-family claim/release atomicity |
| closed | ft-tf6g3.14 | [reality-check-2026-05-12 G29] Resource-cockpit target-class hardware proof gate |
| closed | ft-tf6g3.15 | [reality-check-2026-05-12 G30] Live demo recording (scripts/demo.tape + GIF + asciinema) |
| closed | ft-tf6g3.16 | [reality-check-2026-05-12 G31] Reality-check periodic re-run discipline doc + cron |
| closed | ft-tf6g3.17 | [reality-check-2026-05-12 G32] Proof-artifact taxonomy registry (docs/proof-taxonomy.json + per-bead category tagging) |
| closed | ft-tf6g3.18 | [reality-check-2026-05-12 G33] TLA+ doctrine: formal-method category 6 slot for every subsystem with a serializability/atomicity contract |
| closed | ft-tf6g3.18.1 | [formal-methods] Inventory category-6 spec coverage and gaps |
| closed | ft-tf6g3.18.2 | [formal-methods] TLA+ runtime_async cancel semantics per primitive |
| closed | ft-tf6g3.18.3 | [formal-methods] TLA+ durable_state checkpoint rollback |
| closed | ft-tf6g3.18.4 | [formal-methods] TLA+ mux session reentry invariants |
| closed | ft-tf6g3.18.5 | [formal-methods] TLA+ blocker-radar source merge |
| closed | ft-tf6g3.18.6 | [formal-methods] TLA+ herd-wave admission control |
| closed | ft-tf6g3.18.7 | [formal-methods] TLA+ capture-fairness scheduler liveness |
| closed | ft-tf6g3.19 | [reality-check-2026-05-12 G34] Adversarial fuzzing for the 5 contract-family operator surfaces |
| closed | ft-tf6g3.2 | [reality-check-2026-05-12 G17] AGENTS.md count auto-stamp + drift gate |
| closed | ft-tf6g3.20 | [reality-check-2026-05-12 G35] TLA+/Stateright state-space coverage measurement |
| closed | ft-tf6g3.21 | [reality-check-2026-05-12 G36] Continuous differential check against WezTerm upstream rendering |
| closed | ft-tf6g3.22 | [reality-check-2026-05-12 G37] Sigstore/cosign keyless signing for release attestation bundles |
| closed | ft-tf6g3.23 | [reality-check-2026-05-12 G38] Stochastic network calculus for p99.99 latency bounds under heavy-tail arrivals |
| closed | ft-tf6g3.24 | [reality-check-2026-05-12 G39] Persistent homology for SSIM-complement visual regression detection |
| closed | ft-tf6g3.25 | [reality-check-2026-05-12 G40] Submodular set-cover for benchmark corpus minimization |
| closed | ft-tf6g3.26 | [reality-check-2026-05-12 G41] PAC-Bayes generalization bound for semantic-search recall |
| closed | ft-tf6g3.27 | [reality-check-2026-05-12 G42] KL-divergence-based regime-shift detector to gate SPRT decisions |
| closed | ft-tf6g3.28 | [reality-check-2026-05-12 G43] Causal DAG attribution for perf regressions |
| closed | ft-tf6g3.29 | [reality-check-2026-05-12 G44] Coq/Lean mechanized proof of RuntimeProof sealed-trait soundness |
| closed | ft-tf6g3.3.1 | [reality-check-2026-05-12 G18.1] Resize FPS SLO — bench + bound + attestation slot |
| closed | ft-tf6g3.3.10 | [reality-check][renderer-slo] RQ-S10 atlas rebuild retained target-run evidence |
| closed | ft-tf6g3.3.2 | [reality-check-2026-05-12 G18.2] Input-to-photon latency SLO — instrumented trace + bound + attestation |
| closed | ft-tf6g3.3.3 | [reality-check-2026-05-12 G18.3] SSIM-parity SLO — oracle render + bottleneck (G39 topology cross-check) |
| closed | ft-tf6g3.3.4 | [reality-check-2026-05-12 G18.4] Atlas stability SLO — cache-evict / recover cycle assertion |
| closed | ft-tf6g3.3.5 | [reality-check-2026-05-12 G18.5] Idle GPU power SLO — powermetrics/intel_gpu_top median watts |
| closed | ft-tf6g3.3.6 | [reality-check][renderer-slo] Complete five-SLO retained evidence hashes before closing G18 umbrella |
| closed | ft-tf6g3.3.7 | [reality-check][renderer-slo] RQ-S1 resize FPS retained target-run evidence |
| closed | ft-tf6g3.30 | [reality-check-2026-05-12 G45] crates/ft-perf-gate substrate: SPRT, conformal, KL-divergence, causal-DAG |
| closed | ft-tf6g3.31 | [reality-check-2026-05-12 G46] docs/specs/ formal-methods substrate: naming, mapping-doc, TLC config conventions |
| closed | ft-tf6g3.32 | [reality-check-2026-05-12 G47] Per-claim evidence-stream schema (substrate for G19/G23/G25/G26/G38/G42) |
| closed | ft-tf6g3.33 | [reality-check-2026-05-12 G48] Bitmap-corpus contract for renderer parity (G18/G36/G39 substrate) |
| closed | ft-tf6g3.34 | [reality-check-2026-05-12 G49] Confidence-format schema for mechanized + statistical proofs (G35/G44 substrate) |
| closed | ft-tf6g3.37 | [reality-check-2026-05-12 G50] Audit silent closures of reality-check epic children |
| closed | ft-tf6g3.38 | [doctrine] Require close comments with artifact path and verifier command |
| closed | ft-tf6g3.39 | [reality-check-2026-05-12 G51] Verify-the-verifier: self-test for ft attestation verify |
| closed | ft-tf6g3.4 | [reality-check-2026-05-12 G19] Wire 5 unproven headline claims to signed attestation slots |
| closed | ft-tf6g3.4.1 | [reality-check-2026-05-12 G19.1] FTS5 query <10ms headline-claim → signed artifact |
| closed | ft-tf6g3.4.2 | [reality-check-2026-05-12 G19.2] Robot Mode response <5ms headline-claim → signed artifact |
| closed | ft-tf6g3.4.3 | [reality-check-2026-05-12 G19.3] Memory per pane HOT ~200 bytes/line headline-claim → signed artifact |
| closed | ft-tf6g3.4.4 | [reality-check-2026-05-12 G19.4] Memory per pane WARM ~40 bytes/line @ 5:1 zstd headline-claim → signed artifact |
| closed | ft-tf6g3.4.5 | [reality-check-2026-05-12 G19.5] Bloom filter 10-100x speedup headline-claim → signed artifact |
| closed | ft-tf6g3.40 | [reality-check-2026-05-12 G52] Centralized test-logging convention substrate |
| closed | ft-tf6g3.41 | [reality-check-2026-05-12 G53] Bundle retraction & corrigendum surface for shipped-but-wrong claims |
| closed | ft-tf6g3.42 | [reality-check-2026-05-12 G54] Evidence-stream fixture corpus for downstream test consumers |
| closed | ft-tf6g3.43 | [reality-check-2026-05-12 G55] [AUDIT-IN-PROGRESS — DO NOT CLOSE UNTIL ALL 14 PHANTOM-DELIVERABLE CHECKS POSTED AS COMMENTS] Silent-closure forensic audit |
| closed | ft-tf6g3.44 | [reality-check-2026-05-12 G56] Meta-test for reality-check epic bead structural conformance |
| closed | ft-tf6g3.45 | [reality-check-2026-05-12 G57] Implement ft reality-check {status,next,silent-close-audit,structure-audit} CLI verbs |
| closed | ft-tf6g3.46 | [reality-check-2026-05-12 G58] Cross-family contract integration matrix (context-horizon × capture-fairness × herd-wave × blocker-radar × resource-cockpit) |
| closed | ft-tf6g3.47 | [reality-check-2026-05-12 G59] G56 structural-validator canary: fixture-based correctness gate |
| closed | ft-tf6g3.48 | [reality-check-2026-05-12 G60] Re-file proof-artifact taxonomy registry (G32 silently closed without docs/proof-taxonomy.json shipping) |
| closed | ft-tf6g3.49 | [reality-check-2026-05-12 G61] Trigger-validation tests for G31 reality-check discipline cron |
| closed | ft-tf6g3.5 | [reality-check-2026-05-12 G20] Cx propagation custom lint + ≥99% burn-down dashboard |
| closed | ft-tf6g3.50 | [reality-check-2026-05-12 G62] br update --notes is REPLACE-not-APPEND — doctrine bead to prevent recurrence |
| closed | ft-tf6g3.51 | [reality-check][latency] Complete Lindley coverage for capture tail, Robot, FTS5, and renderer SLO paths |
| closed | ft-tf6g3.53 | [reality-check][bug] fixture_corpus_integration.rs: 2 causal-attribution tests fail on main |
| closed | ft-tf6g3.56 | [mock-code-finder][perf-gate] Reconcile SNC service-curve API with direct-delay quantile implementation |
| closed | ft-tf6g3.6 | [reality-check-2026-05-12 G21] Mazurkiewicz cancel-trace equivalence classes for runtime_async primitives |
| closed | ft-tf6g3.6.1 | [attestation][runtime-async] Mark cancel-trace binary hashes as transient provenance |
| closed | ft-tf6g3.7 | [reality-check-2026-05-12 G22] cargo-deny tokio CI step + supported-path #[tokio::test] purge |
| closed | ft-tf6g3.8 | [reality-check-2026-05-12 G23] Lindley-equation latency derivation finalize + per-release publish |
| closed | ft-tf6g3.9 | [reality-check-2026-05-12 G24] Fano-inequality redaction recall lower bound + per-release publish |
| closed | ft-ux9kb | [test][rch-admission] e2e: live collector populates from real probes + doctor surface is accurate/data-driven (ft-69gwh.5/.9) |
| closed | ft-v2ifl | [HIGH] AGENTS.md does not disclose NTM-gap command families (checkpoint/context/work/fleet/profile all return robot.not_implemented) |
| closed | ft-v3gd9 | FleetMemoryConfig.max_audit_trail unbounded — caller can grow audit_trail without limit |
| closed | ft-wdb0q | wa.events_label list is denied by mutation policy gate |
| closed | ft-wi6ph | Harden scripting filesystem permission checks against prefix bypass |
| closed | ft-womif | [incident][canonicalization] EvidenceLifecycleReceipt content-address digest is delimiter-injectable -> receipt_id collisions |
| closed | ft-wztvw | MCP event mutation strings lack emptiness and size bounds |
| closed | ft-xm4eb | canary healthy streak leaks across phase transitions |
| closed | ft-ykb2h | PlannerExtractionConfig bad denominators poison feature ranking |
| closed | ft-yllta | [MEDIUM] ft robot agents silently feature-gated behind 'agent-detection' — --no-default-features builds get robot.feature_not_available |
| closed | ft-yoeqd | planner scorer accepts NaN scores as assignable |
| closed | ft-zdpou | Mission loop stores raw unbounded external trigger payloads |
| closed | ft-zzzi7 | Planner solver silently drops candidates after max_assignments cap |
| in_progress | ft-0kplz | [reality-check][incident] replay_post_incident execute_pipeline is a simulated stub (hardcoded success) + unwired, but doc claims it 'automates' and 'ensures every resolved incident becomes a regression test' |
| in_progress | ft-0yuxe | [snapshot][retention] Wire configured cleanup into the production scheduler and prove it |
| in_progress | ft-1t0za | Conformance + metamorphic tests: target-class signing/flip fail-closed invariants (W9.4) |
| in_progress | ft-1t4o4 | [reality-check][safety] four [safety.*] config blocks parsed but never consumed (redaction/capabilities/semantic_shock/reservations ignored) |
| in_progress | ft-7h5da.10.3.2 | [reality-check] Load rig emits SYNTHETIC per-mode metrics; exercises no production capture/storage/detection pipeline |
| in_progress | ft-7h5da.10.4.4 | [unwired] No production reader flips target_class_proof_state from the signed artifact; envelope gate inert after signing |
| in_progress | ft-dkxp4 | [event_stream][lost-message] Unwired EventWaiter silently swallows RecvError::Lagged in wait_with_cx (latent false-timeout) |
| in_progress | ft-interactive-systems-performance-4tenz.1 | [performance] Freeze campaign truth, proof boundaries, and negative ledger |
| in_progress | ft-l3m07 | [reality-check] minor configured-but-ignored + dead-docstring cluster (audit_steps/workspace/sync direction/pack extra/SubscriptionRegistry/pane-priority) |
| in_progress | ft-rrqhm | [reality-check][storage] retention_max_mb configured-but-ignored: no size-based eviction exists (unbounded DB growth) |
| in_progress | ft-ske0k | [reality-check][incident] IncidentAutopsyCompiler fully built+tested but never wired to any command — ft-1650n.4 closed with 'add ft incident compile surface' criterion unmet |
| in_progress | ft-ta5as | [reality-check][ingest] 4 ingest config fields parsed but ignored (backpressure_threshold/gap_detection/gap_detection_threshold_percent/max_segment_bytes) |
| in_progress | ft-u6zfw | [reality-check][runtime] HealthSnapshot crash-loop fields hardcoded to healthy zeros — crash loops invisible to ft status / robot health |
| in_progress | ft-ubuw2 | [event_stream][unwired] FilteredEventStream is dead (tests-only) and silently swallows RecvError::Lagged (latent silent-gap) |
| in_progress | ft-v46vj | [reality-check][search] 4 search.daemon config fields parsed but ignored (only daemon.enabled is consumed) |
| in_progress | ft-wbm46 | replay artifact prune() retain() drops ALL manifest entries sharing a pruned path -> loses active registrations |
| in_progress | ft-wtd5g | native_events capture path drops/truncates pane output with NO explicit gap (replay fidelity hole) |
| in_progress | ft-x8e67 | [reality-check][incident] incident_bundle.rs 'canonical schema' is unadopted — crash.rs uses a parallel ad-hoc bundle impl; only CURRENT_FORMAT_VERSION is consumed; README advertises unsupported --mode workflow |
| in_progress | ft-yjfke | [reality-check][patterns][security] pattern-pack supply-chain verification / observe-only gate never consulted on production load |
| open | ft-e87u6 | [reality-check] Close-the-loop attestation manifest — fill stale null-path slots and lift README hedges |
| open | ft-interactive-swarm-product-convergence-7xqz4.1.5 | [product-truth] Map every README and product promise to live producer, Bead, and target claim slot |
| open | ft-interactive-systems-performance-4tenz.6.8.1 | [gui][test][P1] Make binary-owned renderer and TermWindow behavior executable under normal test gates |
| open | ft-interactive-systems-performance-4tenz.9.1 | [perf-cert] Enforce atomic GUI, CLI, mux, protocol, config, font, and evidence identity preflight |
| open | ft-tf6g3 | [reality-check-2026-05-12] Final-mile convergence epic — close attestation graph, headline-claim artifact links, renderer SLO suite, round-3 elevations |
| open | ft-xxfwy | [reality-check-2026-09-01] Entry-ramp truth epic — signed install, GUI attach, live-loop proof, real attestation bundle, program-health reset |
| open | ft-xxfwy.1 | [RC3.G50] Sign DSR release artifacts with minisign; manifest signed:true; release verify fails on unsigned |
| open | ft-xxfwy.10 | [RC3.G52] Live-loop proof tiers 2/3: 20 and 50 live panes with authoritative gates; retire false-green 50-pane assertions |
| open | ft-xxfwy.11 | [RC3.G53] Dogfood status gate: dogfood-status.sh, launchd watcher template, doctrine/dogfood-status attestation slot |
| open | ft-xxfwy.12 | [RC3.G55] Prompt-active evidence everywhere: close ft-zhwa6 CLI capability gap, ft setup shell-integration, honest envelopes and tour |
| open | ft-xxfwy.13 | [RC3.G55.T] Prompt-capability proptest + two-pane evidence e2e + tx precondition e2e |
| open | ft-xxfwy.14 | [RC3.G56] Kill-switch tier enforcement: green RCH proof for ft-l59nq + tier x action conformance artifact + persisted doctor state |
| open | ft-xxfwy.15 | [RC3.G54] First real signed release attestation bundle (0.15.2.json) + DSR quality verifier wiring + README bundle-path fix |
| open | ft-xxfwy.16 | [RC3.G54] Retire .github/workflows under Rule 0.1 (owner authorization) and port residual gates to DSR |
| open | ft-xxfwy.17 | [RC3.G59] Test-suite status artifact per release; fix ft-ziprn TMPDIR at source; burn down ft-nam3s 85-failure baseline |
| open | ft-xxfwy.18 | [RC3.G57] Web /stream/events carries live events: storage-tail bridge + ft watch --web in-process mode (closes ft-zeo5o) |
| open | ft-xxfwy.19 | [RC3.G57.T] SSE live-events e2e: both modes, lag frames, filters, redaction |
| open | ft-xxfwy.2 | [RC3.G50] Publish signed v0.15.2 as latest + clean-host installer e2e (macOS arm64 + Linux amd64) |
| open | ft-xxfwy.20 | [RC3.G58] Measured headline claims per release: headline-run.sh, real-pipeline load rig, cockpit bench lane |
| open | ft-xxfwy.21 | [RC3.G60] README demo recordings: render demo.gif, gate demo-full embed, check-readme-assets.sh |
| open | ft-xxfwy.22 | [RC3.G61] Documentation truth sweep (schema v45, reality-check verbs, detector, metrics feature, DB path, counts, Stateright wording) |
| open | ft-xxfwy.23 | [RC3.G62] Restore weekly WezTerm upstream backport cadence: May-13 to now batch, PROVENANCE backport_batches, due-check script |
| open | ft-xxfwy.24 | [RC3.G63] Program-health reset: P0 renormalization (<=25), stale in_progress release, first-vertical-slice rule, health report |
| open | ft-xxfwy.25 | [RC3.G64] Remove orphaned mcp_helpers.rs with owner authorization + no-orphan-source guard test |
| open | ft-xxfwy.26 | [RC3.G65] Split 129k-line main.rs isomorphically into commands/<family> modules with golden-matrix proof |
| open | ft-xxfwy.27 | [RC3.G67] Rule-pack currency: fixture provenance, corpus-age check, monthly refresh, BOCPD candidate-rule queue |
| open | ft-xxfwy.28 | [RC3.G68/G69] Verification sweep of unverified Supported surfaces -> surface-verification-2026-09.json + README matrix truth |
| open | ft-xxfwy.29 | [RC3.G70] Formal-method lanes in DSR quality (Lean, TLC, Loom) + formal-lanes.json + Stateright wording |
| open | ft-xxfwy.3 | [RC3.G50] Installer activation on idle hosts: promote candidate to current; --activate subcommand; live-host next_action |
| open | ft-xxfwy.30 | [RC3.FINAL] README Quick Install + 10-Minute Tour run verbatim on clean macOS and Linux hosts (acceptance gate) |
| open | ft-xxfwy.4 | [RC3.G50.T] Installer activation e2e: idle activation, live-host hold, failpoint rollback, --activate |
| open | ft-xxfwy.5 | [RC3.G51] ft discovers the running FrankenTerm.app mux: ranked socket discovery incl. GUI default-class symlink |
| open | ft-xxfwy.6 | [RC3.G51.T] Socket discovery tests: unit ranking, proptest, differential GUI vs mux-server, native attach e2e |
| open | ft-xxfwy.7 | [RC3.G51/G66] Typed robot.mux_version_skew + doctor pairing row + installer pairing guidance |
| open | ft-xxfwy.8 | [RC3.G51] Native macOS e2e: ft attaches to running FrankenTerm.app (state/get-text/search) with retained artifact |
| open | ft-xxfwy.9 | [RC3.G52] Live-loop proof tier 1: 3 real agent panes observe->detect->react->audit with signed artifact |

## Attestation status

The release attestation manifest at
[`docs/attestations/manifest.json`](../attestations/manifest.json)
declares one slot per reality-check claim. A slot with a non-null
`path` means the producing bead has shipped its artifact and the
release-bundle build picks it up; `_pending_` means the bead is
still open.

| Category | Producing bead | Artifact |
| -------- | -------------- | -------- |
| perf/headline-claims | ft-syqcz.3 | [present](docs/perf/headline-claims.json) |
| perf/headline-claims | ft-b94bx.8 | [present](docs/attestations/perf/swarm-capacity-envelope.json) |
| perf/competitor-matrix | ft-e87u6.9 | [present](docs/perf/competitor-matrix.json) |
| perf/lindley-bounds | — | _pending_ |
| tui/render-parity | ft-35yac.2 | [present](docs/attestations/tui/render-parity.json) |
| tui/render-parity | ft-35yac.1.2 | [present](docs/attestations/tui/render-parity-gpu.json) |
| tui/render-parity | ft-tf6g3.21 | [present](docs/attestations/tui/wezterm-divergence.json) |
| tui/render-parity | ft-tf6g3.24 | [present](docs/attestations/tui/topology-parity.json) |
| security/passive-watch | ft-x0666.1 | [present](docs/security/passive-watch-attestation.json) |
| security/redactor-coverage | ft-x0666.2 | [present](docs/security/redactor-coverage.json) |
| security/redaction-hygiene | — | _pending_ |
| security/distributed-threat-model | ft-x0666.3 | [present](docs/security/distributed-threat-model.md) |
| proofs/loom-runtime-async | ft-e87u6.12 | [present](docs/attestations/proofs/loom-runtime-async.json) |
| proofs/loom-runtime-async | ft-tf6g3.6 | [present](docs/attestations/proofs/runtime-async-cancel-traces.json) |
| proofs/runtime-proof-trait | ft-i2eni.1 | [present](docs/attestations/proofs/runtime-proof-trait.json) |
| proofs/runtime-proof-trait | ft-tf6g3.7 | [present](docs/attestations/doctrine/tokio-eradication-status.json) |
| proofs/robot-contracts | ft-0elb9 | [present](crates/frankenterm-core/tests/golden_robot_envelope/control_plane_golden_matrix.json) |
| proofs/robot-contracts | ft-auy2g.7 | [present](docs/attestations/proofs/mission-objective-plan.json) |
| proofs/robot-contracts | — | _pending_ |
| proofs/robot-contracts | ft-u7r37.8 | [present](docs/attestations/proofs/mission-twin.json) |
| proofs/robot-contracts | ft-b94bx.10 | [present](docs/attestations/proofs/swarm-capacity-readiness.json) |
| proofs/robot-contracts | ft-ogr3n.8 | [present](docs/attestations/proofs/flight-recorder-incident-replay.json) |
| proofs/robot-contracts | ft-zbnz4.7 | [present](docs/attestations/proofs/deferred-proof-replay.json) |
| proofs/robot-contracts | ft-7h5da.5.5 | [present](docs/attestations/doctrine/wiring-status.json) |
| proofs/robot-contracts | — | _pending_ |
| proofs/rehearsal-score | ft-oohsx.5 | [present](crates/frankenterm-core/tests/fixtures/rehearsal_score_receipt_golden_matrix.json) |
| proofs/tx-killswitch | ft-tf6g3.12 | [present](docs/attestations/proofs/tx-killswitch.json) |
| doctrine/agents-md-counts | ft-tf6g3.2 | [present](docs/attestations/doctrine/agents-md-counts.json) |
| doctrine/vendored-provenance | ft-i2eni.6 | [present](frankenterm/PROVENANCE.json) |
| doctrine/cx-propagation | ft-q0tz3 | [present](docs/runtime/cx-propagation.json) |
| perf/atlas-packing | ft-gtcm9.5 | [present](docs/perf/atlas-packing.json) |

## Cross-references

- Bead board (open RC work): `br list --label reality-check --status=open`
- Bridge plan: [`docs/reality-check-bridge-plan.md`](../reality-check-bridge-plan.md)
- Attestation schema + verifier: [`docs/attestations/schema.json`](../attestations/schema.json), [`scripts/attestation-verify.sh`](../../scripts/attestation-verify.sh)
- Cadence: weekly via `.github/workflows/reality-check-drumbeat.yml`
