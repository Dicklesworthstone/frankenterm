# FrankenTerm Reality-Check Bridge Plan — 2026-05-12

**Source:** `/reality-check-for-project` invocation 2026-05-12. Second-generation reality check. Successor to `docs/reality-check-bridge-plan.md` (2026-04-30 / Round 3 elevation). Pursuant to G31 discipline: do NOT overwrite the prior plan; this is the date-stamped sibling.

**Bead umbrella:** `ft-tf6g3` epic + 34 children (`ft-tf6g3.1` through `ft-tf6g3.34`).

**Operating principle (per AGENTS.md and the prior plan):** revise this document in-place across future ambition rounds within this date-stamped instance; do not spawn parallel plans. The 2026-04-30 plan remains the authoritative record of that run; this plan is the authoritative record of this run.

---

## Reality assessment

FrankenTerm at 2026-05-12 HEAD is in **final-mile convergence**, not aspirational. 3,081 beads closed, 22 open at start (now 57 after this run). 506 core modules, 1 `unimplemented!()`, 36 `TODO/FIXME` substrate-wide. All 5 BR-RC bridge-plan epics from 2026-04-30 (FOUNDATION, DOCTRINE/Tokio, ROBOT-CONTRACT, RUNTIME-SEMANTICS, SAFETY-PROOFS) closed at the bead level. FTUI cutover (G5) is done. Security artifacts (G7/G8/G9) ship. Attestation graph **skeleton** (G11) ships.

What is *not* working: zero real release attestation bundles materialized (only `0.0.0-dev.json`); AGENTS.md still claims 17 sub-crates (actual is 19); 60 `#[tokio::test]` annotations remain in dev paths with no per-test classification doc; 16 `not_implemented` dispatcher callsites linger in the three real `main.rs` files; `runtime_async.rs` is a 255KB monolith with zero in-tree `#[cfg(loom)]`; 5 headline performance claims (FTS5 <10ms, Robot Mode <5ms, ~200 b/line hot, ~40 b/line warm, Bloom 10-100x) have no link to a signed proof artifact; the new renderer SLO catalog (README:1315-1320) post-dates the prior bridge plan and has no proof bead at all; no live demo recording exists.

Velocity is converging — 871 closes wk of 4/27, 339 wk of 5/4, 35 so far this week. This is the right moment for a final-mile reality check.

---

## Gap table (G16–G49, with proof-artifact category from the prior bridge plan's 10-category taxonomy + the 2 new categories introduced by this run's Round 3)

| ID | Bead | Title | Category | Round |
|---|---|---|---|---|
| G16 | ft-tf6g3.1 | Materialize first real release attestation bundle 0.X.Y.json | 10 cryptographic | Phase 2 |
| G17 | ft-tf6g3.2 | AGENTS.md count auto-stamp + drift gate | 5 quantitative attestation | Phase 2 |
| G18 | ft-tf6g3.3 | Renderer SLO attestation suite (resize FPS / input-to-photon / SSIM / atlas / idle GPU) | 4 conformance + 5 quantitative | Phase 2 |
| G19 | ft-tf6g3.4 | Wire 5 unproven headline claims to signed attestation slots | 5 quantitative attestation | Phase 2 |
| G20 | ft-tf6g3.5 | Cx propagation custom lint + ≥99% burn-down dashboard | 1 type-level | Phase 2 |
| G21 | ft-tf6g3.6 | Mazurkiewicz cancel-trace equivalence classes for runtime_async | 3 loom + 6 formal | Phase 2 |
| G22 | ft-tf6g3.7 | cargo-deny tokio CI step + #[tokio::test] dev-path purge | 1 type-level | Phase 2 |
| G23 | ft-tf6g3.8 | Lindley-equation latency derivation finalize + publish | 8 network-calculus | Phase 2 |
| G24 | ft-tf6g3.9 | Fano-inequality redaction recall lower bound | 7 information-theoretic | Phase 2 |
| G25 | ft-tf6g3.10 | SPRT (always-valid) regression gating | 2 property + 7 info-theoretic | Phase 2 |
| G26 | ft-tf6g3.11 | Conformal-prediction SLO bands | 2 property + 5 quantitative | Phase 2 |
| G27 | ft-tf6g3.12 | TLA+ TX-killswitch spec + TLC model check | 6 formal-method | Phase 2 |
| G28 | ft-tf6g3.13 | Stateright model for robot work-family claim/release atomicity | 6 formal-method | Phase 2 |
| G29 | ft-tf6g3.14 | Resource-cockpit target-class hardware proof gate | 5 quantitative attestation | Phase 2 |
| G30 | ft-tf6g3.15 | Live demo recording | (marketing, no proof) | Phase 2 |
| G31 | ft-tf6g3.16 | Reality-check periodic re-run discipline doc + cron | (process) | Phase 2 |
| G32 | ft-tf6g3.17 | Proof-artifact taxonomy registry (per-bead category tagging) | (substrate) | Round 1 |
| G33 | ft-tf6g3.18 | TLA+ doctrine — every serializability/atomicity contract gets a spec | 6 formal-method | Round 1 |
| G34 | ft-tf6g3.19 | Adversarial fuzzing for 5 contract-family operator surfaces | 2 property + 9 differential | Round 2 |
| G35 | ft-tf6g3.20 | TLA+/Stateright state-space coverage measurement | 6 formal-method | Round 2 |
| G36 | ft-tf6g3.21 | Continuous differential check against WezTerm upstream rendering | 9 differential | Round 2 |
| G37 | ft-tf6g3.22 | Sigstore/cosign keyless signing for release attestation bundles | 10 cryptographic | Round 2 |
| G38 | ft-tf6g3.23 | Observed-delay heavy-tail p99.99 quantile gate | 7 info-theoretic tail estimation | Round 3 |
| G39 | ft-tf6g3.24 | Persistent homology for SSIM-complement visual regression detection | **11 topological** (NEW) | Round 3 |
| G40 | ft-tf6g3.25 | Submodular set-cover for benchmark corpus minimization | (infrastructure) | Round 3 |
| G41 | ft-tf6g3.26 | PAC-Bayes generalization bound for semantic-search recall | 7 information-theoretic | Round 3 |
| G42 | ft-tf6g3.27 | KL-divergence regime-shift detector to gate SPRT decisions | 7 information-theoretic | Round 3 |
| G43 | ft-tf6g3.28 | Causal DAG attribution for perf regressions | 7 information-theoretic | Round 3 |
| G44 | ft-tf6g3.29 | Coq/Lean mechanized proof of RuntimeProof sealed-trait soundness | 1 type-level + **12 mechanized** (NEW) | Round 3 |
| G45 | ft-tf6g3.30 | crates/ft-perf-gate substrate (SPRT/conformal/KL/causal) | substrate | Refinement Pass 3 |
| G46 | ft-tf6g3.31 | docs/specs/ formal-methods substrate conventions | substrate | Refinement Pass 3 |
| G47 | ft-tf6g3.32 | Per-claim evidence-stream schema | substrate | Refinement Pass 3 |
| G48 | ft-tf6g3.33 | Bitmap-corpus contract for renderer parity | substrate | Refinement Pass 3 |
| G49 | ft-tf6g3.34 | Confidence-format schema for mechanized + statistical proofs | substrate | Refinement Pass 3 |

## Two new taxonomy categories introduced by Round 3

- **Category 11 — Topological invariant** (G39 persistent homology): differs from category 9 differential testing because it compares *shape* of the rendered output, not the output itself. Specifically: bottleneck distance between persistence diagrams of super-level-set filtrations on glyph bitmaps. Catches the class of bugs that SSIM misses (smooth-but-topologically-broken renders).
- **Category 12 — Mechanized proof** (G44 Coq/Lean): differs from category 6 formal-method because the proof is checked by an external proof assistant, not by Rust's type system or a model checker on a specification.

These extend the prior bridge plan's 10-category taxonomy to 12. G32 (taxonomy registry) is the canonical record.

## Refinement-pass enrichments applied (Phase 5 Pass 1, 2, 4)

Every bead in this epic SHOULD carry, in its description body:

1. **Test companion**: how the artifact is exercised by a dedicated test (Round 1 enrichment).
2. **Operator-surface verb**: how an operator invokes / consumes the artifact via `ft robot`, `ft doctor`, `ft attestation`, or MCP resource URL (Round 2 enrichment).
3. **Degradation behavior**: what happens when the artifact's invariant is violated by reality or the external dependency (Round 4 enrichment).

The bodies of G16-G31 in br were written before these refinement passes ran; the test-companion / operator-verb / degradation requirements live in this document, not (yet) in the bead bodies. A follow-on sweep should `br update` each bead to bake them in. Filed as a *follow-up* discipline item rather than a new bead because it's an enrichment of existing beads, not a new gap.

## Dependency wiring

The release attestation bundle G16 (`ft-tf6g3.1`) blocks on G2, G3, G6, G8, G9, G12, G13, G14, G17, G18, G19, G22, G29, G37, G38, G44 — i.e., it is the single closure gate. G19 blocks on G23, G25, G29, G42, G47. G18 blocks on G10, G39, G48. G3/G18 SLO suite blocks on G25 SPRT. All declared via `br dep add`. `br dep cycles` confirms zero cycles after wiring.

## Validation

- `bv --robot-triage`: total 3,156; open 57; blocked 14; actionable 34; in_progress 2; deferred 2; closed 3,081.
- Cycle check: clean.
- 12 of the 16 Phase-2 beads + 7 of the 7 Phase-3 substrate beads ship per the schema; remaining 16 are Round-1/Round-2/Round-3 ambition expansions.

## Round-3 doctrine — the moat is the math

Every Round-3 bead (G38–G44) introduces a mathematical technique that competitors do not use. The G38 implementation is the narrower observed-delay Pareto tail gate documented in `docs/perf/snc-observed-delay-derivation.md`, not a queueing service-curve composition. The remaining Round-3 lanes cover persistent homology for shape-aware visual regression, submodular optimization for bench-set minimization, PAC-Bayes for ML-claim generalization, KL-divergence regime-shift detection, causal DAG attribution, and mechanized soundness proofs. Each one turns a vibe into a falsifiable claim. The competitive moat for ft as a control plane for AI agents is exactly this: claims that an agent can *verify* offline against a signed bundle, with bounds that hold under adversarial inputs and regime shift.

## Predecessor + successor

- **Predecessor**: `docs/reality-check-bridge-plan.md` (2026-04-30). G1–G15 substrate work, mostly closed.
- **Successor**: a future reality-check will produce `docs/reality-check-bridge-plan-<future-date>.md`. Per G31 discipline: cross-link to this plan and the 2026-04-30 plan.

The historical record is part of the discipline.
