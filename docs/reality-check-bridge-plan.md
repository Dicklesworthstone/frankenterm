# FrankenTerm Reality-Check Bridge Plan

**Source:** `/reality-check-for-project` invocation 2026-04-30. Consolidates strategic gaps with **zero existing bead coverage** plus latent-but-unproven invariants.

**Operating principle (per AGENTS.md):** revise this document in-place across ambition rounds, never spawn parallel plans.

**Doctrine — Round 3 elevation:** every gap closes through a **proof artifact** assembled from a small inventory of formal techniques, then chained into a **release attestation graph**. Each artifact references its inputs by content hash so any third party can re-run the proof bottom-up.

**Proof-artifact taxonomy (what each gap is allowed to use):**

1. **Type-level proofs** — sealed traits, phantom types, lifetime-encoded state machines. Invalid states unrepresentable.
2. **Property/proptest proofs** — randomized verification against declared invariants. Shrinking-aware corpus stored under content hash.
3. **Loom proofs** — bounded model-checking of concurrency primitives. Mazurkiewicz traces document equivalence classes.
4. **Conformance-harness proofs** — golden artifacts + differential testing against reference oracles (insta snapshots, diff fuzz, version-skew tests).
5. **Quantitative attestation** — signed perf bundles per release with Mann-Whitney U / KS tests, p50–p99.9 distributions, hardware baselines.
6. **Formal-method proofs** — TLA+ specs (or Stateright in Rust) for distributed/atomic invariants; Kani / Creusot bounded verification on critical pure functions.
7. **Information-theoretic bounds** — provable lower bounds (e.g., space-recall tradeoff in Bloom filters via Carter–Wegman; redaction recall via PAC-style sample bounds).
8. **Network-calculus bounds** — min-plus algebra (already incarnated in `latency_stages.rs`) gives worst-case-execution-time guarantees that are stronger than empirical p99.
9. **Differential proofs** — same input → two implementations → identical observable output. Either between ft and an oracle (ratatui as ftui oracle, ntm as native-handler oracle) or across ft versions (rolling-upgrade conformance).
10. **Cryptographic attestations** — sigstore / cosign signatures on every release perf bundle, security artifact, and threat-model doc. Anyone can verify provenance offline.

**The release attestation graph.** A single per-release bundle (`docs/attestations/<version>.json`) declares:

```
release/v0.X.Y
  ├── perf/headline-claims.<hash>.json      (G3)
  ├── perf/competitor-matrix.<hash>.json    (G3)
  ├── perf/lindley-bounds.<hash>.json       (G3, network-calculus side)
  ├── tui/render-parity.<hash>.json         (G5)
  ├── security/passive-watch.<hash>.json    (G9)
  ├── security/redactor-coverage.<hash>.json(G10)
  ├── security/distributed-threat-model.<hash>.md (G11)
  ├── proofs/loom-runtime-async.<hash>.json (G8)
  ├── proofs/runtime-proof-trait.<hash>.json(G1, type-seal)
  ├── proofs/robot-contracts.<hash>.json    (G2)
  ├── proofs/tx-killswitch.<hash>.json      (G27)
  ├── doctrine/agents-md-counts.<hash>.json (G6)
  ├── doctrine/cx-propagation.<hash>.json   (G14.2)
  └── signature metadata + sigstore/ed25519 sidecar
```

**Single principle:** if it is not in the attestation graph, the claim is not shipped. The README and AGENTS.md both link to the latest attestation. CI fails any release that omits a required artifact.

**Why this matters for ft specifically.** ft is sold as a *control plane for AI agents*. AI agents cannot safely trust claims they can't verify. Putting every claim behind a content-addressed signed artifact means an AI caller can ask `ft attestation verify` and know — provably — what guarantees are active in this build. That's the durable competitive moat.

---

## G1 — Tokio Eradication w/ Type-Level Proof (BR-RC-TOKIO)

### Reality-check finding
- AGENTS.md says "direct `tokio` usage is forbidden."
- Workspace `Cargo.toml:250` still pins `tokio = { version = "1.49.0", features = ["full"] }`.
- `crates/frankenterm-core/Cargo.toml:21` declares `tokio.workspace = true` (production dep, not dev-dep).
- 60 `#[tokio::test]` annotations remain inside `crates/frankenterm-core`.
- 2 sub-crates (`frankenterm-core-fleet`, `frankenterm-core-tantivy`) still have `tokio = { workspace = true, features = ["test-util"] }`.
- `runtime_async.rs` itself only references tokio in 2 doc comments (the wrapper is clean) — the gap is in callers and test infra.

### Why it matters
The README and AGENTS doctrine sells ft as "asupersync-native." A workspace dep + 60 tokio test attributes is a credibility leak. Grep-based guards catch only what we already know to look for; what we want is **structural impossibility**.

### Bridge actions — round-2 elevated
1. Inventory every `#[tokio::test]` in core; classify (a) convertible to `asupersync_test!`, (b) requires Cx-aware refactor, (c) fundamentally tokio-only (delete).
2. Convert (a) and (b); delete (c) with documented rationale.
3. **Type-level seal**: Define a sealed `RuntimeProof` trait that only `runtime_async`'s primitives implement. Every public async API in core must consume `impl RuntimeProof` somewhere in its signature. Tokio types cannot satisfy the trait → compiler refuses tokio leakage at the type level, not just at lint time.
4. **`asupersync_test!` proc-macro**: ergonomic replacement that auto-installs the runtime handle, threads `Cx::for_request()`, and is the single way to write async tests in this workspace. Add a clippy lint that flags any `#[tokio::test]` in non-vendored crates as `error`.
5. **Forbidden-deps lint via `cargo-deny`**: hard-deny `tokio` direct dependency from any first-party crate via `[bans]` in `deny.toml`. Vendored fork crates allow-list explicitly with comment.
6. **Mazurkiewicz-trace cancel-correctness proof**: For every primitive in `runtime_async`, prove via Loom (see G8) that the cancel semantics are independent of tokio-shaped assumptions. Document the trace equivalence classes — this is what "Cx-first" actually means.
7. Drop `tokio.workspace = true` from `crates/frankenterm-core/Cargo.toml`. Test-only sub-crate uses become explicit `[dev-dependencies]` with rationale comments.
8. Update AGENTS.md to drop "always-absolute" framing and adopt: "tokio is forbidden in production code; permitted in `[dev-dependencies]` of leaf crates with documented rationale; structurally impossible in core via the `RuntimeProof` trait."

### Acceptance criteria
- `grep -rE '#\[tokio::test' crates --include='*.rs'` returns 0 outside `frankenterm/` vendored tree
- `cargo deny check bans` passes with tokio in the deny list (with explicit allow-list for vendored crates only)
- Every public async API in `crates/frankenterm-core/src/` provably consumes a `RuntimeProof` (sealed trait check passes)
- Loom test suite (G8) exercises every `runtime_async` primitive
- AGENTS.md doctrine block updated and matches reality

---

## G2 — Robot Family Closure w/ Conformance Harness (BR-RC-ROBOT-NTM)

### Reality-check finding
- Original review finding: the checkpoint, context, work, fleet, and profile
  robot families returned
  `robot.not_implemented`: `checkpoint`, `context`, `work`, `fleet`, and
  `profile`.
- Current status: those families now have native dispatch. The current source
  of truth is `docs/robot-contracts/current-ntm-gap-dispatch.md`, which records
  an empty generic NTM-gap fallback set.
- Historical context: the missing `wa-rsaf` epic identifier and old README
  "use ntm" pointer described the pre-native-dispatch state; do not treat them
  as current implementation guidance.

### Why it matters
"Robot mode is supported" is a headline claim. The original risk was that some
families silently degraded to a generic NTM fallback. The current risk is
different: after native dispatch ships, AI callers need stable contracts and
proof that those handlers behave correctly under failure, partial commit, and
adversarial input.

### Bridge actions — round-2 elevated
1. **Per-family semantic spec FIRST.** For each graduated family, keep a typed
   contract that documents:
   - Idempotency class (idempotent / commutative / sequential)
   - Failure-semantics: must-not-partially-mutate / can-partially-mutate-with-receipt / fire-and-forget
   - Observable side-effect surface (events emitted, storage rows mutated, IPC sent)
   - Concurrency model (serializable / per-pane-serial / parallel)
2. **Conformance harness skeleton** in `crates/frankenterm-core/tests/robot_family_conformance/`. For every family, the harness:
   - Generates random valid request streams via proptest strategies derived from the contract
   - Asserts idempotency: replay yields identical observable result
   - Asserts atomicity: kill-switch mid-call leaves storage in a recoverable state (per the plan's existing `MissionKillSwitchLevel`)
   - Asserts observable-effect bound: only the declared side-effects occur (events/storage diff matches manifest)
3. **Differential testing against `ntm` reference**: For families that have an
   `ntm` equivalent, run both backends against the same input stream and assert
   identical observable behavior. This preserves regression coverage after the
   "delegate to ntm" → "native impl" transition.
4. **Per-family scoping** (now contract-driven, not vibe-driven): keep this
   matrix aligned with `docs/robot-contracts/current-ntm-gap-dispatch.md` and
   the README robot-mode implementation table.
   - **checkpoint** {save, list, show, delete, rollback}: Wires into the
     native snapshot/session adapter. `save` is content-addressable,
     inspection/deletion are native session-table operations, and non-dry-run
     rollback remains approval-blocked until the robot policy gate allows the
     mutating restore path.
   - **context** {status, rotate, history}: Uses the native SQLite
     `pane_contexts` / `context_rotations` registry. Rotation records a durable
     receipt, supports optional idempotency-key replay, and stores metadata
     without persisting raw conversation content.
   - **work** {claim, release, complete, list, ready, assign}: Uses the native
     SQLite `work_claims` queue. Claim, release, assignment, and completion
     semantics compose with Beads IDs and are serialized per work item.
   - **fleet** {status, scale, rebalance, agents}: Uses native
     agent-inventory/work-queue read paths plus the fleet mutation substrate
     for scale/rebalance plans. Dry-run returns receipts without side effects;
     commit paths return durable mutation receipts or typed `robot.fleet.*`
     errors.
   - **profile** {list, show, validate, apply}: Uses
     `robot_profile_handler`. Read paths, validation, dry-run apply, and
     mux-backed non-dry-run apply are shipped; apply is idempotent on identical
     input and rolls back panes on mid-apply failure.
5. **The `robot.not_implemented` envelope STAYS** as the typed degradation
   surface for genuinely-unbuilt features, but no graduated checkpoint,
   context, work, fleet, or profile family should route through it.
6. **Approval-token integration audit**: each family must declare which actions auto-allow vs require an approval token; the `policy.rs` engine wires every shipped handler.

### Acceptance criteria
- Family contracts checked into `docs/robot-contracts/<family>.md` with typed schemas.
- Conformance harness covers the graduated families; tests pass on every commit.
- Differential test, where an `ntm` equivalent exists, shows zero observable
  divergence on a 1000-request fuzz corpus.
- Zero live call sites hit `build_ntm_not_implemented_response` for checkpoint,
  context, work, fleet, or profile.
- README support matrix stays aligned with native-dispatch reality; every
  family has at least one e2e example.
- Each family's contract is attested in the release bundle.

### Round-3 additions
- **TLA+ spec per family that mutates state** (checkpoint, work). Model the prepare/commit/compensate against the existing TX engine; check liveness + safety with TLC. Spec lives in `docs/specs/robot-<family>.tla`.
- **Stateright in-Rust model checking**: For `work` (claim/release/complete), build a Stateright model and prove:
  - No two agents can hold the same claim simultaneously
  - Every claim eventually releases (no claim leak under any failure interleaving)
  - Completed work is durable (no lost-completion under crash + restart)
- **Schema-driven proptest generator**: single source-of-truth schema (declared in the contract MD as JSON Schema or strongly-typed Rust enum) feeds proptest *and* the conformance harness *and* the OpenAPI / MCP tool registry. One change ripples consistently. Borrows from operationalizing-expertise: every family gets a quote-bank of golden examples that validate against the schema.

---

## G3 — Continuous Performance Attestation (BR-RC-PERF-EVIDENCE)

### Reality-check finding
README headline numbers:
- "<50ms capture latency" (now hedged as "target/benchmark lane rather than always-on guarantee")
- "200+ concurrent panes" (no published artifact)
- "~50MB for 100 panes" / "~200MB for 200 panes" (no published artifact)
- "5:1 to 10:1 zstd compression" (claim, no artifact)
- "10-100x" Bloom filter speedup (claim, no artifact)

100 Criterion benches exist under `crates/frankenterm-core/benches/`, but no artifact links from README.

### Why it matters
The README itself acknowledges the gap. Worse: `latency_stages.rs` is **29,300 LOC** of network-calculus / min-plus algebra machinery — there's already infrastructure to prove latency bounds *formally*, but no public artifact uses it on the headline claims. The asymmetry between "we have the math" and "we publish nothing" is the gap.

### Bridge actions — round-2 elevated
1. **Headline-claim manifest** (`docs/perf/headline-claims.json`) — one entry per claim with: the verbatim README sentence, the workload spec (panes, throughput, pack size), the benchmark file producing the number, the latency-stages proof artifact (when applicable), and the hardware baseline.
2. **Statistical rigor**, not point estimates. Every published number is a distribution: report p50/p95/p99/p99.9, sample size, confidence interval, Mann-Whitney U test against the previous release. Bench harness emits `target/criterion/ft-bench-meta.jsonl` already; extend it to emit the full distribution.
3. **Tie to existing min-plus algebra** (`latency_stages.rs`): for the <50ms latency claim, derive a Lindley-equation bound from the network-calculus stage model and publish the bound *alongside* the empirical distribution. If they disagree, that's a bug worth finding.
4. **Differential benchmarks against competitors**. Run a representative pane-capture+search workload against WezTerm, Zellij, Ghostty (where applicable) on identical hardware. Publish the comparison matrix. The README "How ft Compares" table currently asserts qualitative differences; this makes them quantitative.
5. **Signed per-release perf bundle**. On every release tag, CI runs the headline-set on a pinned hardware baseline (specific GitHub Actions runner SKU or local machine with documented spec), gpg-signs the artifact, publishes to `docs/perf/releases/<version>.json` + a release asset.
6. **Per-PR regression gate**: fail build if any headline-claim metric regresses >10% (configurable per metric). Fail-soft for noise; fail-hard for sustained regressions across 3+ recent commits.
7. **Replayable bench corpus**: capture-and-store realistic 50-pane, 1-hour swarm output as a deterministic input to capture/scan benches. Today benches use synthetic data; replay-based benches catch the perf bugs synthetic data misses.
8. **Publish a "soft-realtime SLO" doc** that turns each headline number into a queue-theory-grounded SLO with declared violation conditions. Use Little's Law to relate the throughput claim to latency claim and prove they're internally consistent.

### Acceptance criteria
- `docs/perf/headline-claims.json` machine-readable, one row per README claim
- `docs/perf/releases/<version>.json` published per release tag, signed (sigstore/cosign)
- `docs/perf/competitor-matrix.json` published quarterly (or per major release)
- Per-PR regression gate active on the 5 headline metrics with sequential-test correction (Lai-Robbins / SPRT) so noise doesn't false-flag
- README perf table links to live attestation graph entries
- For latency: published Lindley-bound + empirical distribution agree within tolerance, with a `min-plus algebra derivation` checked into `docs/perf/latency-derivation.md`
- Bench corpus stored as a replayable artifact (pinned content hash)
- **Continuous attestation invariant**: the attestation bundle for each release contains a Merkle proof linking every published number back to the source bench commit + hardware fingerprint. Any reviewer can recompute and verify offline.

### Round-3 alien-artifact additions (extreme-software-optimization × alien-artifact-coding)
- **Sequential testing for regression gating** (Lai-Robbins/SPRT or always-valid-CI from Howard et al.): traditional t-test gates with α=0.05 across 100 metrics yield ~5 false positives per CI run. Use anytime-valid p-values so the gate stays sound under repeated checks.
- **Concentration-of-measure bounds for bench sample sizing**: derive minimum sample size to detect a 10% regression at given power via Hoeffding/Bernstein. Bench harness auto-targets the bound.
- **Conformal prediction for SLO bands**: instead of fixed p99 thresholds, wrap each metric in a conformal prediction interval that adapts to drift. Detects regime shifts without hand-tuning.
- **Information-theoretic lower bound for redaction recall** (G10 cross-link): use Fano's inequality to publish the minimum sample size needed to claim a recall floor with confidence ≥99%.

---

## G4 — Vendored Crate Rename (BR-RC-CRATE-RENAME)

### Reality-check finding
4 vendored crates still use `wezterm-*` package names per their `Cargo.toml`:
- `frankenterm/client/Cargo.toml`: `name = "wezterm-client"`
- `frankenterm/font/Cargo.toml`: `name = "wezterm-font"`
- `frankenterm/open-url/Cargo.toml`: `name = "wezterm-open-url"`
- `frankenterm/toast-notification/Cargo.toml`: `name = "wezterm-toast-notification"`

ft-zoxxq made the explicit "we're a wezterm-fork, not pursuing a second mux backend" commitment. Once you commit to the fork stance, finishing the rename is debt cleanup.

### Why it matters
Brand inconsistency. Anyone running `cargo tree` or `cargo metadata` sees `wezterm-client` and gets confused about what they're depending on. Also breaks the search invariant in AGENTS.md: stale default-branch references are bugs — same logic applies to `wezterm-` package names after ft-zoxxq stance.

### Bridge actions
1. Rename each `name = "wezterm-X"` → `name = "frankenterm-X"` in the 4 Cargo.toml files.
2. Update workspace `Cargo.toml` `[workspace.dependencies]` entries to match.
3. Update every `use wezterm_X` import across the workspace.
4. Update `Cargo.lock` (will regenerate).
5. Document the rename in CHANGELOG.md so anyone with stale references knows.
6. Audit `legacy_wezterm/` and `legacy_*` directories — confirm they're not imported anywhere active.

### Acceptance criteria
- `grep -rE 'name = "wezterm-' frankenterm/*/Cargo.toml` returns empty
- `grep -rE 'use wezterm_(client|font|open_url|toast_notification)' --include='*.rs'` returns empty
- Build passes on all features

---

## G5 — ftui Cutover via Differential Render Oracle (BR-RC-FTUI-MIGRATION)

### Reality-check finding
- `crates/frankenterm-core/src/tui/` is 27,245 LOC across 13 files with both ratatui and ftui backends.
- `ftui_backend.rs` alone is 8,296 LOC — a real implementation, not a stub.
- `rollout` feature compiles both; runtime selection via `FT_TUI_BACKEND` env var.
- Multiple modules carry `// DELETION: Remove when ... (FTUI-09.3)` comments — there's a clear endpoint, but it hasn't shipped.
- README lists `tui` as a feature but doesn't expose the migration to users.

### Why it matters
Carrying two TUI backends doubles maintenance, doubles binary size, and turns every TUI bug into "which backend?" Also: ftui is one of the named integrated libraries — finishing the cutover is the proof point that the integration paid off.

### Bridge actions — round-2 elevated
1. **Differential-render oracle harness**: `tests/tui_render_parity/` drives both backends with the same input event stream (a recorded corpus of real user sessions) and asserts byte-identical render frames. Use vhs/asciinema-derived input scripts. Treat ratatui as the *reference oracle*, not the deprecated stack.
2. **Property-based parity**: proptest strategies generate arbitrary keymap event sequences (within the existing keymap.rs canonical table) and run both backends; assert state-machine equivalence on every step.
3. **Phased rollout under `rollout` feature**: dogfood + dev builds set `FT_TUI_BACKEND=ftui` by default for a soak window. Crash reports auto-correlate to backend.
4. Set `ftui` as default in stable build only after the differential harness reports zero divergence on the parity corpus for a full release cycle.
5. **Don't delete ratatui — quarantine it**. Keep ratatui as a `tui-oracle` dev-feature that runs in CI permanently as a regression catch. Production binary drops it; test infrastructure keeps it. This trades binary size for permanent regression protection.
6. Resolve all `// DELETION: Remove when ... (FTUI-09.3)` comments — either delete the module or convert it to be ftui-only.
7. **Publish a TUI consistency proof**: the differential harness publishes a JSON artifact per release (`docs/tui/render-parity-<version>.json`) as part of the perf attestation bundle from G3.

### Acceptance criteria
- Differential harness exists, runs in CI, reports byte-identical output for the parity corpus
- `ftui` is the default backend in shipped binaries
- `tui-oracle` dev-feature exists and CI runs it on every PR touching the TUI
- All `// DELETION: ...` comments resolved
- Render-parity JSON artifact published per release

---

## G6 — Doc Count Drift Auto-Generation (BR-RC-DOC-COUNTS)

### Reality-check finding
README/AGENTS both rely on hand-edited counts ("779k+ lines", "342 modules", "67 workspace crates") with explicit `(ft-d3awp)` warnings that they drift fast and instructions to verify with shell commands.

### Why it matters
Every doc edit becomes a stale-count edit. The drift warnings are fine but the underlying problem is solvable: bake the live-count commands into a CI doc-stamp step.

### Bridge actions
1. Write `scripts/stamp-readme-counts.sh` that runs all the verification commands from README and substitutes `<!-- count:workspace_members -->` style placeholders.
2. Add CI step that fails build if README/AGENTS counts diverge from current values by >5%.
3. Convert hand-edited counts to placeholders in README + AGENTS in a single sweep.

### Acceptance criteria
- `scripts/stamp-readme-counts.sh` exists and runs cleanly
- README/AGENTS hand-edited count claims gone or marked auto-generated
- CI guard active

---

## G7 — mux-server-impl `unimplemented!()` (BR-RC-SESSIONHANDLER-STUB)

### Reality-check finding
Single `unimplemented!()` macro in `crates/frankenterm-mux-server-impl/src/sessionhandler.rs:1731`. The only one in production code anywhere in the workspace.

### Why it matters
Low risk (one site), high signal (a panic-bomb in a long-running daemon). Either implement it or replace with a typed error so the daemon can degrade gracefully.

### Bridge actions
1. Read the surrounding context to determine what the unhandled case should do.
2. Either implement the missing branch or convert to a typed `Err(NotSupported{kind})` that the caller already knows how to surface.
3. Add a regression test that exercises the previously-panic path.

### Acceptance criteria
- `grep -rE 'unimplemented!\(\)' crates --include='*.rs'` returns 0 hits in production code
- New test covers the scenario

---

## G8 — Loom Concurrency Proof for `runtime_async` (BR-RC-LOOM)

### Reality-check finding
`runtime_async.rs` is the load-bearing primitive layer (Mutex, RwLock, Semaphore, mpsc, watch, broadcast, oneshot). It has unit tests but no exhaustive concurrency state-space exploration. `runtime_async` is also where `Cx::current()` thread-local lookup happens — exactly the kind of code where rare interleavings cause production bugs that don't repro in normal tests.

### Why it matters
Every other safety claim in ft transitively depends on these primitives behaving correctly under arbitrary thread interleavings. Loom (Rust crate) gives us bounded model-checking for this exact class of bug.

### Bridge actions
1. Add `loom = "0.7"` as a dev-dependency under a `loom` cfg.
2. For each primitive in `runtime_async` (Mutex, RwLock, Semaphore, mpsc, watch, broadcast, oneshot, Notify): write a Loom test exploring all interleavings of N concurrent operations, asserting:
   - No deadlock
   - No data race
   - FIFO/LIFO ordering invariants where declared
   - Cancel-safety: dropping a future at any await point leaves the primitive in a usable state
3. Capture the trace equivalence classes (Mazurkiewicz traces) — document which interleavings are observably equivalent so future audits can reason about new primitives.
4. CI lane: `cargo test --features loom` runs nightly (Loom is slow); fails build if any new primitive lacks a Loom proof.

### Acceptance criteria
- Every primitive in `runtime_async` has a corresponding Loom test
- Loom test suite passes on nightly CI
- Mazurkiewicz-trace doc published

---

## G9 — Passive-Watch Read-Only Proof (BR-RC-PASSIVE-WATCH)

### Reality-check finding
README claims: "ft watch is read-only; mutating actions must pass the Policy Engine." Currently asserted, not proven.

### Why it matters
This is the foundational safety claim. If a randomly-crafted pane output stream can drive `ft watch` into emitting a send/spawn/close action, every "safe by default" claim downstream is invalid.

### Bridge actions
1. Build a fuzz harness that drives the watch loop with adversarial pane-output corpus (mutated real terminal output, malicious escape sequences, deliberately-crafted prompts).
2. Instrument the watch process to record every outbound IPC and every storage write that is *not* a capture.
3. Assert: fuzz session produces zero outbound mutating IPC and zero non-capture storage writes (pattern detections OK; sends/spawns/closes NOT OK).
4. Run for N hours per release; publish artifact.

### Acceptance criteria
- Fuzz harness in `fuzz/passive_watch_invariant.rs`
- CI runs ≥1 hour per PR; ≥24 hours per release
- Published artifact: `docs/security/passive-watch-attestation-<version>.json`

---

## G10 — Secret Redaction Recall/Precision Matrix (BR-RC-REDACTOR-MATRIX)

### Reality-check finding
`redactor.rs` (675 LOC) has 50+ regex patterns covering OpenAI, Anthropic, GitHub, AWS, JWT, etc. Coverage is *claimed* but not measured. No published corpus, no recall/precision number.

### Why it matters
Secret redaction is a marketed safety guarantee. Industry standard is to publish recall+precision against a public test corpus (gitleaks, trufflehog) so users can calibrate trust.

### Bridge actions
1. Pull gitleaks + trufflehog test corpora into `tests/redactor_corpus/` (vendored, version-pinned).
2. Build a benchmark that streams the corpus through the redactor; computes recall (true positives) and precision (1 - false positives).
3. Publish `docs/security/redactor-coverage.json` per release.
4. Set a recall floor (e.g., ≥99% on the gitleaks corpus); fail CI if release dips below.
5. For false-positive findings, cluster and reduce by tightening contextual constraints (provider-specific prefixes, length bounds).

### Acceptance criteria
- gitleaks/trufflehog corpora vendored
- Recall/precision benchmark exists, runs per release
- Published artifact, ≥99% recall floor enforced

---

## G11 — Distributed Wire Protocol Threat Model + Diff Fuzz (BR-RC-DIST-THREAT)

### Reality-check finding
`distributed.rs` (6.1k LOC) + `wire_protocol.rs` (2.5k LOC). README documents version envelopes, sequence dedup, 1MiB cap, stale-session pruning, local receipt-clock decisions. **No explicit threat model document.** No differential fuzz against past wire versions.

### Why it matters
Distributed mode is feature-gated and off by default but documented as a real shipping mode. Without an explicit threat model, the security claims are aspirational.

### Bridge actions
1. **Threat model doc** (`docs/security/distributed-threat-model.md`): enumerate adversaries (network attacker, compromised aggregator, compromised agent, replay), assets, trust boundaries, mitigations-in-place, mitigations-pending.
2. **Differential fuzz across wire versions**: fuzz harness encodes random `UpdateMessage` under v1, decodes under v2 (and vice versa); asserts well-defined errors, no panics, no buffer overruns.
3. **Property tests** for sequence-number dedup: random reorderings must produce identical post-state.
4. **Loopback-default audit**: confirm `distributed.bind_addr` defaults to loopback in the shipped config; CI test asserts this.
5. **TLS hardening review**: when bind is non-loopback, TLS must be on; fail-closed if not.

### Acceptance criteria
- Threat model doc shipped
- Wire-protocol differential fuzz in `fuzz/wire_protocol_*.rs`
- Property tests for dedup + ordering
- CI test asserts loopback default
- Doctor check warns if non-loopback bind without TLS

### Round-3 additions — formal safety
- **Stateright/TLA+ model of the gossip + dedup protocol**: prove that under arbitrary message reordering + duplicates + drops, the aggregator state converges to the same observable view that a single-host run would have produced. CRDT-style proof sketch (PN-Counter for sequence dedup, OR-Set for pane registry).
- **Byzantine-robustness lite**: even though full Byzantine consensus is out of scope (loopback default), prove that a malicious agent cannot inject events attributed to *another* agent (origin authentication invariant). Use cryptographic identity (ed25519 per agent) and verify via property tests.
- **Reed-Solomon erasure encoding** for cross-host audit ledger replication (alien-artifact uplift): when distributed mode is on, the policy-denial audit chain replicates with k-of-n erasure so loss of any single host doesn't lose audit history. ft-coded artifact: `docs/security/audit-replication-spec.md`.

---

## G12 — Live Demo Recording (BR-RC-LIVE-DEMO)

### Reality-check finding
README is text-heavy. No animated demo of a 50-pane swarm running through real work. The closest visual is `frankenterm_illustration.webp`.

### Why it matters
A picture is worth 1000 words; a 90-second VHS recording of ft observing a multi-agent swarm is worth 10,000. This is a marketing gap, but it's also a *credibility* gap — claiming "swarm-native" without showing the swarm makes evaluators skeptical.

### Bridge actions
1. Script a realistic 5-minute scenario: 10 panes running cc/cod/gmi, ft watches, a rate limit hits, ft detects + emits event, a workflow auto-handles, search recovers a past error, mission orchestrates a recovery.
2. Record via vhs (`scripts/demo.tape`).
3. Embed in README as both GIF and asciinema link.
4. Re-record per major release.

### Acceptance criteria
- `scripts/demo.tape` checked in
- README embeds the demo
- Re-recording is part of release checklist

---

## G13 — Mission/TX Kill-Switch State-Space Proof (BR-RC-TX-KILLSWITCH)

### Reality-check finding
`tx_execution.rs` (3.8k LOC) has `MissionKillSwitchLevel` and `paused` flags. The README claims "kill switches and pause controls provide emergency intervention" and "Every transition emits an observability event with a reason code and decision path."

**Unproven:** that flipping the kill-switch *during* a commit step can never strand the system in a partial state with no compensation pathway.

### Why it matters
Mission/TX is the single most dangerous thing ft can do — multi-pane mutations with prepare/commit/compensate. A bug here is not a crash, it's *user-visible damage to other AI agents being orchestrated*. Hardest class of bug to recover from.

### Bridge actions
1. **TLA+ spec for the prepare→commit→compensate state machine**, including kill-switch flips at every transition. TLC checks safety (no committed-without-receipt) and liveness (kill-switch eventually drains to a terminal state).
2. **Stateright in-Rust model**: drive the actual `tx_execution.rs` engine through the Stateright harness; assert the same invariants on the real code.
3. **Property test using existing `fail_step` injection**: random kill-switch flips combined with random `fail_step` scheduling — assert idempotency ledger always reaches a terminal state, no orphan locks/reservations.
4. **Trauma-guard cross-link**: failures observed in this proof feed the existing trauma-guard catalog so future agents avoid known bad patterns.

### Acceptance criteria
- TLA+ spec checked into `docs/specs/tx-killswitch.tla`
- Stateright harness in `tests/tx_killswitch_model.rs`
- Property test passes; covers ≥1M random schedules per CI run
- Attestation entry in release bundle

---

## G14 — Cx Propagation Completeness Audit (BR-RC-CX-PROPAGATION)

### Reality-check finding
`runtime_async::Mutex::lock_with_cx`, `sleep_with_cx`, `timeout_with_cx` exist. README itself says: "Cancellation, time, and blocking behavior may still follow tokio-shaped semantics on those paths" and "use `rg 'runtime_async::'` to see which paths a given feature still transits."

The `Cx::current()` thread-local fallback exists *because* not every async fn threads `&Cx`. That's the gap.

### Why it matters
Cx is what makes "structured, cancel-correct async" real. Every async fn that doesn't take `&Cx` is a path where:
- Deadline propagation breaks
- Cooperative cancel breaks
- Budget accounting breaks
- LabRuntime virtual time stops working deterministically

The README hedges this honestly. The bridge plan must close it.

### Bridge actions
1. **Custom clippy lint**: flag any `pub async fn` in `crates/frankenterm-core/src/` that does not take `&Cx` (or thread one through a builder). Integrate via `dylint`.
2. **Burn-down dashboard**: weekly report (`docs/runtime/cx-propagation.json`) of how many public async fns still need conversion. Currently unknown; first report establishes baseline.
3. **Cx-first refactor sprint**: prioritized by call-path criticality (capture loop → workflow engine → web/SSE → MCP server → distributed → connectors → tx engine).
4. **Type-level check** (G1 cross-link): the sealed `RuntimeProof` trait can be tightened to *also* require Cx threading, making "fn forgot to take &Cx" a compile error in core.
5. **LabRuntime virtual-time test for every async fn**: any fn that takes `&Cx` must have a test demonstrating deterministic cancel under virtual time.

### Acceptance criteria
- Custom clippy lint exists, runs on PR
- Cx-propagation burn-down dashboard published per release
- LabRuntime test coverage tracked in attestation bundle
- Final state: 100% of public async fns in core thread `&Cx`

---

## G15 — Vendored Fork Provenance Manifest (BR-RC-PROVENANCE)

### Reality-check finding
The 42 vendored `frankenterm/*` crate directories carry 534 fork-side commits since divergence. ft-zoxxq.5 already produced a "per-crate provenance + classification table for 42 vendored subcrates" doc. **No machine-readable provenance manifest** that downstream consumers (or attestation graph) can verify.

### Why it matters
- Every vendored crate is a security-audit liability. Without a manifest of "which upstream wezterm commit are we forked from + what we changed," CVE response is slow.
- The fork-rename incomplete state (G4) is symptomatic of missing provenance discipline.
- AI agents reading the repo can't quickly answer "is this code original to ft or wezterm-derived" — affects refactoring confidence.

### Bridge actions
1. Generate `frankenterm/PROVENANCE.json`: per-vendored-crate, the upstream wezterm commit SHA at fork point, the list of fork-side commits modifying it, classification (security-fix / runtime-swap / cleanup / cosmetic), maintenance status.
2. Build `scripts/check-provenance.sh` that audits crates for residual upstream-name leakage (`wezterm-*` package names — overlaps with G4) and unrecorded modifications.
3. Wire into release attestation bundle.
4. Generalize as `scripts/audit-vendored-fork.sh` so the same machinery audits any future fork.

### Acceptance criteria
- `frankenterm/PROVENANCE.json` machine-readable, one entry per vendored crate
- `scripts/check-provenance.sh` runs in CI
- Attestation bundle includes provenance hash

---

## Cross-cutting: Test bead requirements

Every implementation bead above must spawn a companion test bead that includes:
- Unit tests with edge/empty/error cases
- E2E test scripts where the change crosses a process boundary
- Detailed structured logging so failures are diagnosable from JSON-line output
- Proptest where the change touches serde or schema

Per AGENTS.md frozen template clause: "DO NOT OVERSIMPLIFY — comprehensive unit tests and e2e test scripts with great, detailed logging."

---

## Sequencing notes

**Quick wins (parallel):** G7 (single stub), G4 (mechanical rename), G6 (doc count drift), G12 (demo recording).

**Foundation tier (blocking other work):**
- G8 (Loom) — proves runtime_async; G1 quotes it; G9 + G10 + G11 build on it.
- G3 (perf attestation infra) — establishes the proof-artifact pipeline; G5/G9/G10/G11 plug into the same publishing system.

**Strategic tier:**
- G1 (tokio eradication) — uses G8 Loom proof + G3 attestation pipeline.
- G2 (robot families) — largest scope; depends on contract specs (independent) but each family ships through G3's attestation.
- G5 (ftui cutover) — soak window after differential harness; uses G3 publishing.
- G9/G10/G11 — security proofs; each publishes through G3.

**Recommended bead epic structure:**
- **BR-RC-FOUNDATION** (epic): G3 (perf attestation infra) + G8 (Loom) — the proof infrastructure all other gaps publish into.
- **BR-RC-DOCTRINE** (epic): G1 (tokio eradication w/ type-seal) + G4 (rename) + G6 (doc-count auto-stamp) + G15 (provenance manifest) — bring code & docs back in line with the asupersync-native + wezterm-fork doctrine.
- **BR-RC-ROBOT-CONTRACT** (epic): G2 — the 5-family closure with contracts + conformance + TLA+/Stateright models.
- **BR-RC-SAFETY-PROOFS** (epic): G9 (passive watch) + G10 (redactor matrix) + G11 (distributed threat model) + G13 (TX kill-switch) — turn security claims into signed attestations.
- **BR-RC-RUNTIME-SEMANTICS** (epic): G14 (Cx propagation) — finish making "structured, cancel-correct async" actually true.
- **BR-RC-CUTOVERS** (epic): G5 (ftui) + G7 (sessionhandler stub) — finish migrations and stubs.
- **BR-RC-DEMO** (task): G12 — standalone.

**Bead dependency invariant:** Every G≥9 epic *must* declare a bead-level dependency on the relevant Foundation primitive (Loom for G14, attestation infra for G9/G10/G11/G13). This forces the foundation to ship first.

**Critical-path forecast** (using bv `--robot-forecast` once beads exist):
- Foundation tier ships first → unblocks doctrine + safety-proofs + robot-contract in parallel.
- Cutovers can run alongside since they don't depend on the proof infrastructure (only on contract clarity).
- Demo (G12) is standalone but should land *after* doctrine because it embeds version numbers.

**Dogfood discipline:** every gap closes through a publishable artifact (JSON or signed bundle) that future agents reading this repo can verify in one command. No gap is "done" if the only evidence is a passing test that disappears at green-CI time.

---

## Convergence summary (2026-04-30)

After Phase 4 (3 ambition rounds) + Phase 5 (5 refinement passes), 44 reality-check beads created and indexed:

| Epic | ID | Children | P1 / P2 / P3 |
|---|---|---|---|
| BR-RC-FOUNDATION | ft-syqcz | 8 | 5 / 2 / 1 |
| BR-RC-DOCTRINE | ft-i2eni | 6 | 3 / 3 / 0 |
| BR-RC-ROBOT-CONTRACT | ft-hac7w | 7 | 5 / 2 / 0 |
| BR-RC-SAFETY-PROOFS | ft-x0666 | 5 | 4 / 0 / 1 |
| BR-RC-RUNTIME-SEMANTICS | ft-t9a6q | 3 | 3 / 0 / 0 |
| BR-RC-CUTOVERS | ft-35yac | 4 | 0 / 2 / 2 |
| Standalone | (varies) | 4 | 0 / 3 / 1 |

**Ready-to-work entry points (10 beads with no blockers):**
- ft-i2eni.1 (RuntimeProof trait) — gates DOCTRINE chain + RUNTIME-SEMANTICS
- ft-syqcz.1 (attestation schema) — gates SAFETY proofs + provenance + verify CLI
- ft-syqcz.2 (bench statistical rigor) — gates headline-claim manifest
- ft-syqcz.6 (Loom skeleton) — gates Loom proofs
- ft-hac7w.1 (robot schema infra) — gates 5 family closures
- ft-hac7w.1.1 (ntm differential harness) — used by checkpoint family
- ft-i2eni.4 (vendored crate rename) — independent
- ft-i2eni.5 (auto-stamp counts) — independent
- ft-syqcz.1.1 (`ft attestation verify` CLI) — independent post G3.1
- ft-35yac.3 (single sessionhandler stub) — quick win

**Critical-path forecast:** FOUNDATION + DOCTRINE land first → unblock SAFETY + ROBOT-CONTRACT + RUNTIME-SEMANTICS → CUTOVERS + DEMO ride along.

Bridge plan doc revised in-place 4 times across phases. Any future revision should continue in this file (per AGENTS.md doctrine — never spawn parallel plans).
