# Modes of Reasoning: Project Analysis Report

**Project:** FrankenTerm (ft) -- Swarm-Native Terminal Platform for AI Agent Fleets
**Date:** 2026-04-07
**Modes Used:** 10 of 80 available
**Agents:** 10 Claude Opus 4.6 subagents
**Lead Agent:** RusticMaple (Claude Opus 4.6 1M context)

---

## 1. Executive Summary

Ten independent reasoning agents analyzed FrankenTerm from radically different analytical lenses: systems thinking, root-cause analysis, failure-mode enumeration, adversarial security review, inductive pattern recognition, counterfactual reasoning, stakeholder perspective-taking, edge-case analysis, second-order effects tracing, and scope control with debiasing.

The analysis reveals a project with a genuinely novel and valuable core idea -- terminal observability and orchestration for AI agent swarms -- that is being overwhelmed by the very development model that built it. The AI agent swarm development approach has produced 790k+ lines and 483 modules in what appears to be roughly a year, but the result is a monolithic crate where **57% of modules have zero internal importers**, textbook data structures with no callers consume 32k+ lines, and the test-to-production-code ratio is 2.7:1. The system cannot run without WezTerm (despite claiming to replace it), the async runtime migration is incomplete (57 failing tests), and the build infrastructure has become a first-class engineering problem.

### Key Takeaways

1. **The monolith must be split.** 7 of 10 modes independently identified the 483-module single crate as the project's most pressing structural problem. It blocks parallel compilation, causes agent coordination failures, and makes cognitive navigation impossible.

2. **The AI swarm development model has a subtraction problem.** 6 of 10 modes identified that agents create modules faster than anyone integrates, reviews, or prunes them. ~229 orphan modules (57%) with zero importers represent 250-300k lines of dead weight. The bead/task system rewards creation, not integration.

3. **The WezTerm dependency is a slowly-closing one-way door.** 5 modes identified the vendored fork as creating gravitational pull that blocks native runtime development. Each local modification increases the cost of both rebasing upstream and replacing it entirely.

4. **Critical security gaps exist.** The adversarial review found policy bypass paths (3 code paths skip the Injector), zero-auth wire protocol, inconsistent secret redaction, and caller-supplied PaneCapabilities that bypass reservation checks. The FMEA found the highest-severity failure (RPN 336) in silent message loss on distributed sender restart.

5. **A valuable ~100-150k line product is buried under ~600k lines of speculative accretion.** The core observation-detection-action pipeline is well-designed (backpressure, hysteresis, delta extraction). The surrounding modules -- BOCPD regime detection, dancing links, wavelet trees, 27 replay modules, 21-subsystem policy engine -- represent investment far beyond current needs.

### Overall Confidence: 0.85

High confidence in structural findings (module counts, import analysis, file sizes are factual). Moderate confidence in the "subtraction needed" thesis -- the alternative explanation (genuine feature breadth) cannot be fully excluded without usage data, but the 57% orphan rate is strong evidence.

---

## 2. Methodology

### Why These 10 Modes?

| # | Mode | Code | Category | Selection Rationale |
|---|------|------|----------|-------------------|
| 1 | Systems-Thinking | F7 | Causal | 54 crates, 483 modules need holistic feedback loop analysis |
| 2 | Root-Cause | F5 | Causal | Known problems (WezTerm dep, async migration) need 5-why tracing |
| 3 | Failure-Mode (FMEA) | F4 | Causal | 200+ pane scale claims require systematic failure enumeration |
| 4 | Adversarial Review | H2 | Strategic | 21-subsystem policy engine + security claims need stress testing |
| 5 | Inductive | B1 | Ampliative | 790k AI-generated LOC needs cross-codebase pattern consistency analysis |
| 6 | Counterfactual | F3 | Causal | Key decisions (WezTerm, asupersync, forbid unsafe) need alternative evaluation |
| 7 | Perspective-Taking | I4 | Dialectical | Multiple stakeholders: solo dev, AI agents, potential users, inheritors |
| 8 | Edge-Case | A8 | Formal | Scale claims (200+ panes, <50ms latency) need boundary verification |
| 9 | Second-Order Effects | F6 | Causal | Architecture decisions cascade; concurrent AI development has ripple effects |
| 10 | Scope Control + Debiasing | L5/L2 | Meta | 790k LOC in single crate raises scope/complexity questions |

### Category Coverage

| Category | Count | Modes |
|----------|-------|-------|
| A: Formal | 1 | Edge-Case (A8) |
| B: Ampliative | 1 | Inductive (B1) |
| F: Causal | 4 | Systems-Thinking (F7), Root-Cause (F5), FMEA (F4), Counterfactual (F3), Second-Order (F6) |
| H: Strategic | 1 | Adversarial Review (H2) |
| I: Dialectical | 1 | Perspective-Taking (I4) |
| L: Meta | 1 | Scope Control + Debiasing (L5/L2) |

### Axis Coverage

| Axis | Represented By |
|------|---------------|
| Ampliative vs Non-ampliative | B1 (ampliative), A8 (non-ampliative) |
| Descriptive vs Normative | F7/F5/B1 (descriptive), I4 (normative via stakeholders) |
| Uncertainty vs Vagueness | F4 (uncertainty via RPN scoring) |
| Belief vs Action | F3 (belief -- what-if), H2/F4 (action -- what to do) |
| Single-agent vs Multi-agent | H2/I4 (multi-agent), A8 (single-agent verification) |
| Truth vs Adoption | L5/L2 (truth -- is scope justified?), I4 (adoption -- can users adopt this?) |

### Modes Considered But Not Selected

- **Bayesian (B3):** Would have quantified risk probabilities, but the FMEA's RPN scoring partially covers this.
- **Game-Theoretic (H1):** Relevant for agent-agent interaction design, but less urgent than adversarial security.
- **Temporal (E3):** Would have caught race conditions, partially covered by edge-case and FMEA.
- **Design-Thinking (K4):** Relevant for UX, but perspective-taking covers the user angle.

---

## 3. Convergent Findings (High Confidence)

### Finding C1: The Monolithic Core Crate Is Unsustainable

**Supporting modes:** F7, F5, B1, F3, I4, F6, L5/L2 (7 of 10)
**Confidence:** 0.95

The single `frankenterm-core` crate contains 483 modules, ~790k lines of Rust, and a 536-line `lib.rs` that is a flat list of `pub mod` declarations. This was the most universally identified problem.

**Evidence from each mode:**
- **F7 (Systems):** "Coupling surface area grows quadratically with module count." Identified the module growth as a self-reinforcing loop with no balancing force.
- **F5 (Root-Cause):** Traced to the bead/task system: "each agent session creates a self-contained module to satisfy its bead, and no agent has incentive or ability to integrate with existing code."
- **B1 (Inductive):** Found that 181/406 modules (44.6%) have zero `use crate::` imports -- they are self-contained and could be extracted with no code changes.
- **F3 (Counterfactual):** Judged the monolith "suboptimal but pragmatically correct for now" because "any module can `use crate::other_module` without cross-crate dependency management."
- **I4 (Perspective):** From the future inheritor view: "These are not modules -- they are codebases within codebases."
- **F6 (Second-Order):** "The monolith creates a serialization bottleneck for the swarm... The build system complexity is now a first-class engineering problem."
- **L5/L2 (Scope):** "404 modules in one crate is a code smell, not an architecture. Rust's module system exists for organizing code into crates with clear API boundaries."

**Why convergence matters:** Seven independent analytical frameworks, from formal (edge-case boundaries) to meta (scope control), independently concluded the same thing via different reasoning paths. The inductive mode counted imports, the root-cause mode traced the incentive structure, the systems mode mapped feedback loops, and the scope mode compared to peer projects. This is the analysis's highest-confidence finding.

**Recommended action:** Split the monolith along existing feature-flag boundaries. The 12+ feature gates in lib.rs (`subprocess-bridge`, `mcp`, `disk-pressure`, `recorder-lexical`, etc.) are natural crate boundaries. Extract the 181 zero-import modules into a `frankenterm-ds` utility crate or archive them.

---

### Finding C2: AI Swarm Development Creates Self-Reinforcing Complexity Growth

**Supporting modes:** F7, F5, B1, I4, F6, L5/L2 (6 of 10)
**Confidence:** 0.92

The development model -- dozens of concurrent AI agents working on a single-branch monorepo -- produces code faster than it can be integrated, reviewed, or pruned.

**Evidence from each mode:**
- **F7 (Systems):** "Each agent adds modules, each module adds import surface, each import adds compilation time, longer compilation leads to more cargo lock contention... This is a classic reinforcing loop with no balancing force."
- **F5 (Root-Cause):** "There is no architectural boundary that limits the blast radius of any single agent session." AGENTS.md devotes its first 84 lines to damage-control rules from past agent disasters.
- **B1 (Inductive):** "The project grew ~400 modules in ~2 months... the data structure batch was created in a single day." Found that proptest coverage is "formulaic" -- agents follow templates rather than testing unique behavioral properties.
- **I4 (Perspective):** "Agents produce volume that outpaces human review capacity." The 501 proptest files create noise that makes finding real test gaps harder.
- **F6 (Second-Order):** "The AGENTS.md rules are defensive scar tissue from repeated agent-caused disasters." The defensive rules themselves consume agent context window.
- **L5/L2 (Scope):** "229 out of 404 modules (57%) are never `use`d by another module." Identified velocity illusion, IKEA effect, and sunk cost fallacy as driving biases.

**Kill thesis attempt:** Could these modules represent genuine feature breadth rather than bloat? Evidence against: 32k+ lines of textbook data structures (dancing links, fibonacci heap, wavelet trees) have zero callers. 27 replay modules exist but the core replay path uses only ~3. The 501 proptest files primarily test serde roundtrips on types that no production code consumes.

**Recommended action:** Freeze module creation. Establish "integration sprints" where agent sessions are tasked with wiring orphan modules into the core pipeline or archiving them. Create beads for subtraction, not just addition.

---

### Finding C3: WezTerm Vendored Dependency Is a Slowly-Closing One-Way Door

**Supporting modes:** F5, F3, I4, F6, F7 (5 of 10)
**Confidence:** 0.88

The project vendors 30+ ex-WezTerm crates (~298k lines) as a bootstrap for terminal emulation functionality. Each local modification increases the cost of both rebasing upstream and replacing it entirely.

**Evidence from each mode:**
- **F5 (Root-Cause):** Full 5-why chain: "WezTerm was never designed to be a library." The vendored crates carry async-io/smol assumptions that conflict with asupersync.
- **F3 (Counterfactual):** "Vendoring WezTerm was the correct bootstrap decision." But: "The risk is abandonment stall -- if migration stalls, the vendored code becomes dead weight."
- **I4 (Perspective):** "Currently requires WezTerm as backend bridge, which undercuts the 'replacement-class terminal runtime' positioning."
- **F6 (Second-Order):** "Cannot go forward (full native mux) or backward (re-sync with upstream) cheaply. This is a one-way door that is slowly closing."
- **F7 (Systems):** Identified the runtime migration as a system-wide delay and leverage point.

**Recommended action:** Document the exact subset of WezTerm functionality actually used. Create a WezTerm exit plan with milestones. Track vendored fork divergence cost explicitly.

---

### Finding C4: Runtime Migration (tokio -> asupersync) Is Incomplete and Taxing

**Supporting modes:** F7, F5, F3, I4, F6 (5 of 10)
**Confidence:** 0.87

The migration from tokio to asupersync is declared as policy ("direct tokio usage is forbidden") but incomplete in practice (57 failing tests, 72+ tokio references in runtime_compat.rs).

**Evidence:**
- **F5 (Root-Cause):** "The runtime migration is a rewrite of the concurrency model, not a drop-in replacement, but was treated as an incremental swap." asupersync requires explicit `Cx` context parameters incompatible with tokio's ambient runtime.
- **F3 (Counterfactual):** "The decision with the highest ongoing tax. First to revisit if asupersync development stalls."
- **I4 (Perspective):** "Stop telling agents it is 'forbidden' while 72 references remain."
- **F6 (Second-Order):** "130+ lines of migration scaffolding" in runtime_compat.rs.

**Recommended action:** Either complete the migration with a time-boxed sprint or acknowledge the dual-runtime as accepted state. The worst option is the current limbo.

---

### Finding C5: SQLite Single-Writer Is a Scaling Bottleneck

**Supporting modes:** F4, A8, F6, F7 (4 of 10)
**Confidence:** 0.83

**Evidence:**
- **F4 (FMEA):** Single-writer thread serializes all INSERTs via mpsc channel with no backpressure. RPN 252 (Severity 9 x Likelihood 7 x Detection 4). Missing `PRAGMA busy_timeout` causes read connections to hit SQLITE_BUSY.
- **A8 (Edge-Case):** `SELECT MAX(seq)` per insert creates O(log N) bottleneck at 200+ pane scale.
- **F6 (Second-Order):** "If FrankenTerm succeeds, the storage layer becomes the bottleneck that forces an architecture change."
- **F7 (Systems):** Referenced through the backpressure-memory-eviction triad.

**Recommended action:** Add `PRAGMA busy_timeout = 5000` (single-line fix). Cache `next_seq` per pane in-memory. Bound the writer channel. Move telemetry/audit writes to a separate append-only store.

---

### Finding C6: Massive Orphan/Dead Code Problem

**Supporting modes:** B1, L5/L2, F5, F3 (4 of 10)
**Confidence:** 0.90

**Evidence:**
- **B1 (Inductive):** 181/406 modules (44.6%) have zero `use crate::` imports.
- **L5/L2 (Scope):** 229/404 modules (57%) are never imported by another module. 32,672 lines of textbook data structures with zero callers.
- **F5 (Root-Cause):** "data structures like dancing links, van Emde Boas trees, wavelet trees are unlikely to be used in production."
- **F3 (Counterfactual):** "The swarm has produced data structures and replay variants that may never be called from production code paths."

**Recommended action:** Audit all modules with zero importers. Archive to `experiments/` crate or delete. The codebase should not carry 250k+ lines of unexercised code.

---

## 4. Divergent Findings (Points of Disagreement)

### Disagreement D1: Is `forbid(unsafe_code)` Net Positive?

**Position A:** F3 (Counterfactual), F7 (Systems) argue it is **strongly correct**.
- Evidence: "Total elimination of memory safety bugs in a system operated by AI agents that generate code at scale." With 50+ concurrent agents, the blast radius of a subtle UB bug is catastrophic.
- The performance cost is bounded because vendored WezTerm crates handle rendering outside this constraint.

**Position B:** F6 (Second-Order) argues it creates **safety theater at boundaries**.
- Evidence: 46+ `Command::new` call sites shell out to subprocess programs that DO use unsafe internally. "You get safety theater at the boundary: the kernel calls are still happening, just via an opaque subprocess." Each subprocess invocation adds latency, error surface, and invisible failure modes.

**Analysis:** These modes operate at different levels. F3 reasons about compile-time guarantees (correct); F6 reasons about system-level safety boundaries (also correct). The disagreement reveals a genuine tradeoff: compile-time memory safety for Rust code vs. runtime opacity of subprocess boundaries.

**Lead agent assessment:** `forbid(unsafe_code)` is correct for the core crate given the AI swarm development model. The subprocess workaround cost is real but manageable. Consider creating a small, heavily-reviewed `frankenterm-platform` crate that allows targeted unsafe for performance-critical paths (mmap, SIMD) with explicit review gates.

---

### Disagreement D2: Is the 21-Subsystem Policy Engine Appropriate?

**Position A:** H2 (Adversarial) implicitly supports it -- found bypass vulnerabilities that would be far worse without the engine. The depth of policy infrastructure enabled the adversarial review to find actionable issues.

**Position B:** F6 (Second-Order) and L5/L2 (Scope) argue it is **over-invested**.
- F6: "For a single-developer project with zero external users, this is an investment in safety infrastructure that consumes development bandwidth."
- L5/L2: "More policy subsystems than concrete use cases."

**Analysis:** Different values. H2 prioritizes security (more policy = more safety surface), while L5/L2 prioritizes sustainability (more policy = more maintenance burden). Both are correct within their frameworks.

**Lead agent assessment:** The policy engine is over-invested for the current user base (1 developer). Consider a two-tier approach: a simple allow/deny layer active by default, with the full 21-subsystem engine behind `--features full-policy`. This preserves the investment while reducing default complexity.

---

## 5. Unique Insights by Mode

### Systems-Thinking (F7) -- Unique Contributions
- **Auto-tuner meta-controller conflict:** The auto-tuner adjusts parameters that the pressure controllers use, creating a second-order control loop prone to oscillation if gain is too high. No other mode caught this inter-subsystem dynamic.
- **Continuous vs. discrete backpressure coexistence:** Two models (sigmoid continuous + 4-tier FSM) can produce conflicting signals if both are active.

### Failure-Mode (F4) -- Unique Contributions
- **Wire protocol sender restart = silent message loss (RPN 336):** Highest-severity failure found by any mode. Sender sequence resets to 0 on restart; aggregator sees `seq <= last_seq` and silently discards all messages until seq exceeds the pre-restart high-water mark. No detection mechanism exists.
- **FTS5 index divergence post-crash:** `synchronous=NORMAL` allows OS-buffered writes to be lost between content INSERT and FTS5 trigger execution. No integrity check in recovery path.

### Adversarial Review (H2) -- Unique Contributions
- **PaneCapabilities supplied by caller, not derived authoritatively:** The Injector trusts caller-provided capabilities rather than fetching from authoritative state. A malicious caller can supply `is_reserved: false` for any pane, bypassing reservation checks.
- **Approval token brute-force with no lockout:** 36^8 combinations but no failed-attempt counter, no lockout, no logging of failures.
- **ReadOutput/SearchOutput exempt from rate limiting:** Enables high-volume secret exfiltration.

### Inductive (B1) -- Unique Contributions
- **Timestamp representation anarchy:** 104 modules use `timestamp_ms: u64`, 66 use `Instant`, 73 use `SystemTime`. No project-wide convention despite serialization requirements that make `Instant` unusable.
- **Error handling bifurcation:** 17 modules use central `crate::Error`; 79 define local `XxxError` enums. Coherent split but undocumented.

### Perspective-Taking (I4) -- Unique Contributions
- **No external validation:** Zero external users, zero external contributions, zero issues from non-Jeff humans. The README markets to an audience that does not yet exist.
- **MEMORY.md IS the runbook:** Operational knowledge lives in a 346-line agent memory file, not in code, types, or documentation.

### Scope Control / Debiasing (L5/L2) -- Unique Contributions
- **Test inflation:** 54% of core crate lines are inside `#[cfg(test)]` blocks. 847k total test lines testing ~314k production lines (2.7:1 ratio). The flagship `latency_stages.rs` is 78% test code.
- **Agent incentive misalignment:** Agents are rewarded for closing beads (creating things). No beads exist for "delete 50k lines of dead code" or "prove the core loop works end to end."

### Edge-Case (A8) -- Unique Contributions
- **Wall-clock timestamp non-monotonicity:** `epoch_ms()` uses `SystemTime::now()`, susceptible to NTP adjustments. A clock jump backward creates out-of-order `captured_at` values that confuse `ORDER BY captured_at` queries.
- **FTS5 index has no size cap:** Unbounded growth under high-output panes until disk exhaustion.

---

## 6. Risk Assessment

| # | Risk | Severity | Likelihood | Modes Flagging | Confidence |
|---|------|----------|------------|---------------|------------|
| 1 | Wire protocol silent message loss on sender restart | Critical | High | F4 | 0.90 |
| 2 | Policy bypass via direct WeztermClient (3 call sites) | Critical | Medium | H2 | 0.88 |
| 3 | Monolith compile time blocks agent productivity | High | High | F7, F5, F6, L5 | 0.95 |
| 4 | Cascading blackout: storage slow -> backpressure -> paused panes -> blind monitoring | Critical | Medium | F4, F7 | 0.85 |
| 5 | Wire protocol zero authentication allows message injection | Critical | Medium | H2, F4 | 0.88 |
| 6 | Secret leak via wire protocol matched_text streaming | High | High | H2 | 0.85 |
| 7 | SQLite single-writer bottleneck at 200+ pane scale | High | High | F4, A8, F6 | 0.85 |
| 8 | WezTerm fork divergence makes exit increasingly expensive | High | High | F5, F3, F6 | 0.90 |
| 9 | Codebase grows beyond maintainability for any human | High | High | L5, F5, I4, F6 | 0.92 |
| 10 | FTS5 index divergence post-crash | High | Medium | F4 | 0.80 |
| 11 | No disk-full resilience in storage/backpressure | Medium | Medium | A8, F4 | 0.82 |
| 12 | Orphan modules accumulate indefinitely | Medium | High | B1, L5, F5 | 0.90 |

### Critical Risks (Require Immediate Attention)

**Wire protocol (Risks 1, 5):** The distributed mode has zero authentication and silent message loss on sender restart. Any network-adjacent attacker can inject messages. This is the most severe security finding.

**Policy bypass (Risk 2):** Three restore code paths call `WeztermClient::send_text()` directly without going through the Injector. This violates the project's own safety architecture.

### Strategic Risks (Compound Over Time)

**Codebase maintainability (Risks 3, 9, 12):** The monolith + swarm model creates a complexity ratchet that only turns one direction. Without deliberate subtraction mechanisms, the project will reach a state where no human or AI can navigate it.

---

## 7. Recommendations

| Priority | Recommendation | Supporting Modes | Effort | Impact |
|----------|---------------|-----------------|--------|--------|
| 1 | Split monolith along feature-flag boundaries | F7,F5,B1,F3,I4,F6,L5 | High | Critical |
| 2 | Fix wire protocol auth + sender restart dedup | F4,H2 | Medium | Critical |
| 3 | Audit and archive 229 orphan modules | B1,L5,F5,F3 | Medium | High |
| 4 | Make WeztermClient non-pub or wrap in Injector | H2 | Low | Critical |
| 5 | Add PRAGMA busy_timeout + cache next_seq | F4,A8 | Low | High |
| 6 | Complete or acknowledge runtime migration | F7,F5,F3,I4,F6 | High | High |
| 7 | Redact at storage layer, not per-consumer | H2 | Medium | High |
| 8 | Create WezTerm exit plan with milestones | F5,F3,F6 | Low | High |
| 9 | Freeze module creation; establish integration sprints | L5,F5,F6 | Low | High |
| 10 | Ship working binary (cross-compile releases) | I4 | Medium | High |

### Top 5 Recommendations (Detailed)

#### Recommendation 1: Split Monolith Along Feature-Flag Boundaries
**Supporting modes:** F7, F5, B1, F3, I4, F6, L5/L2 (7 of 10)
**Dissenting modes:** F3 notes it was "pragmatically correct for now" -- acknowledges the need to split but warns about merge conflict risk during the transition.
**What:** Extract the 12+ feature-gated subsystems into separate workspace crates. Natural split points: `policy` (8 modules), `replay` (27 modules), `connectors` (14 modules), `search` (23 modules), `data-structures` (17+), `robot-api`, `fleet-memory`.
**Why:** Enables parallel compilation, reduces blast radius of agent edits, creates meaningful API boundaries.
**Expected benefit:** 3-5x faster incremental compilation. Reduced agent coordination conflicts. Clearer module ownership.
**Effort:** High (requires Cargo.toml restructuring and import path updates across 400+ files).
**Risks of NOT doing this:** Compilation time continues to grow. Agent productivity declines. Cognitive navigation becomes impossible.

#### Recommendation 2: Fix Wire Protocol Authentication + Sender Restart Dedup
**Supporting modes:** F4, H2
**What:** Add HMAC or mutual TLS to wire protocol. Implement sender-restart detection (when received seq < last_seq, treat as restart rather than duplicate).
**Why:** Currently the highest-severity security vulnerability (zero auth) and highest-RPN failure mode (silent message loss).
**Expected benefit:** Secure distributed mode. No more silent message drops on agent restart.
**Effort:** Medium.
**Risks of NOT doing this:** Any network-adjacent attacker can inject messages. Restarted agents silently lose all messages until sequence catches up.

#### Recommendation 3: Audit and Archive 229 Orphan Modules
**Supporting modes:** B1, L5/L2, F5, F3
**What:** Run dead-code analysis. Move modules with zero importers and no CLI entry point to an `experiments/` crate or `#[cfg(feature = "experimental")]` gate.
**Why:** 250-300k lines of unexercised code consume compile time, cognitive load, and maintenance bandwidth.
**Expected benefit:** Smaller, faster-building crate. Clearer picture of what the product actually is.
**Effort:** Medium (automated import analysis + manual triage).

#### Recommendation 4: Make WeztermClient Non-Public or Wrap in Injector
**Supporting modes:** H2
**What:** Change `WeztermClient::send_text()` visibility or wrap in a newtype that enforces Injector-mediated access. Fix the three restore paths that bypass policy.
**Why:** Critical security gap. The type system should enforce the observe/act split, not rely on developer discipline.
**Expected benefit:** Eliminates policy bypass attack vector.
**Effort:** Low.

#### Recommendation 5: Add SQLite busy_timeout + Cache next_seq
**Supporting modes:** F4, A8
**What:** Add `PRAGMA busy_timeout = 5000` to connection initialization. Cache `next_seq` per pane in-memory instead of querying `MAX(seq)` per insert.
**Why:** Eliminates SQLITE_BUSY errors under read contention and removes per-insert query bottleneck.
**Expected benefit:** Immediate reliability improvement at scale.
**Effort:** Low (single-line pragma + small refactor).

---

## 8. New Ideas and Extensions

### High-Potential Ideas

#### Idea 1: "Integration Sprint" Beads
**Originating mode(s):** L5/L2, F5, F6
**Description:** Create a new bead type specifically for subtraction/integration work: "wire module X into the pipeline," "archive Y unused modules," "reduce storage.rs below 10k lines." Weight these beads higher in `bv --robot-triage` priority.
**Feasibility:** High
**Potential impact:** High -- directly addresses the accretion problem.

#### Idea 2: Pressure-Aware Agent Scheduling
**Originating mode(s):** F7
**Description:** Expose the fleet memory controller's pressure tier to the NTM agent swarm, so agents self-throttle cargo builds when the system is under pressure.
**Feasibility:** Medium
**Potential impact:** Medium -- reduces build contention cascade.

#### Idea 3: Detection Liveness Heartbeat
**Originating mode(s):** F7, F4
**Description:** When PatternEngine degrades, emit a synthetic `DetectionDisabled` event. This breaks the masking-failure loop where degraded detection looks like system health.
**Feasibility:** High
**Potential impact:** High -- prevents the cascading blackout scenario.

#### Idea 4: Working Demo Without WezTerm
**Originating mode(s):** I4
**Description:** Create a mock/demo backend that provides a simulated pane environment, allowing `ft watch`, `ft robot state`, and `ft robot search` to work on a fresh machine without WezTerm.
**Feasibility:** Medium
**Potential impact:** High -- eliminates the new-user bootstrap cliff.

### Exploratory Ideas

- **Canary-based auto-tuning validation** using beta_feedback_loop.rs cohort machinery (F7)
- **Feedback loop visualization** from actual `use crate::` imports in pressure modules (F7)
- **Encode MEMORY.md tribal knowledge as types and compiler errors** (I4)
- **`frankenterm-platform` crate** for targeted unsafe with review gates (F6/F3 synthesis)

---

## 9. Open Questions

| # | Question | Raised By | Why It Matters |
|---|----------|-----------|----------------|
| 1 | How many of the 483 modules are actually exercised in a real `ft watch` session? | L5/L2 | Determines true dead code extent |
| 2 | What is the actual user count beyond Jeff Emanuel? | I4 | Affects priority of all user-facing recommendations |
| 3 | Is asupersync development continuing actively? | F3, F5 | If it stalls, the migration tax becomes permanent |
| 4 | What is the actual write throughput of the SQLite storage path at 200 panes? | F4, A8 | Validates or invalidates the bottleneck concern |
| 5 | Have the policy bypass paths (Rec #4) ever been exploited? | H2 | Determines urgency of the fix |
| 6 | Is the distributed mode intended for production use? | F4, H2 | If yes, the zero-auth wire protocol is urgent |
| 7 | What would the target module count be after a healthy refactor? | L5/L2 | Need to know what "done" looks like |
| 8 | Why were dancing links, fibonacci heaps, etc. created? | L5/L2, B1 | Determines if they serve an unrevealed purpose |

---

## 10. Confidence Matrix

| Finding | Confidence | Supporting | Dissenting | Notes |
|---------|-----------|------------|------------|-------|
| C1: Monolith unsustainable | 0.95 | F7,F5,B1,F3,I4,F6,L5 | None | Factual: 483 modules, 57% orphans |
| C2: Swarm complexity spiral | 0.92 | F7,F5,B1,I4,F6,L5 | None | Evidence strong; alternative (genuine breadth) weak |
| C3: WezTerm migration trap | 0.88 | F5,F3,I4,F6,F7 | None | Fork divergence is factual; timeline uncertain |
| C4: Runtime migration incomplete | 0.87 | F7,F5,F3,I4,F6 | None | 57 failing tests, 72+ tokio refs are factual |
| C5: SQLite bottleneck | 0.83 | F4,A8,F6,F7 | None | Theoretical at current scale; real at claimed 200+ |
| C6: Orphan code problem | 0.90 | B1,L5,F5,F3 | None | Import analysis is factual |
| D1: forbid(unsafe) value | 0.70 | F3,F7 (positive) | F6 (negative) | Genuine tradeoff; both sides have merit |
| D2: Policy engine scope | 0.65 | H2 (appropriate) | F6,L5 (over-invested) | Value-dependent; no single answer |

### Confidence Calibration Notes

Highest confidence on structural findings (module counts, import analysis, file sizes) because they are directly measurable. Lower confidence on architectural tradeoff assessments (D1, D2) because they depend on values and future trajectory. The analysis would shift significantly with information about actual deployment usage (Question #2) and asupersync development trajectory (Question #3).

---

## 11. Mode Performance Notes

| Mode | Code | Productivity | Unique Value | Applicability | Notes |
|------|------|-------------|-------------|--------------|-------|
| Systems-Thinking | F7 | High | High | High | Found feedback loops and meta-controller conflicts |
| Root-Cause | F5 | High | Medium | High | 5-why chains were rigorous and evidence-based |
| Failure-Mode | F4 | Very High | Very High | High | RPN scoring and cascade analysis were invaluable |
| Adversarial | H2 | Very High | Very High | High | Found concrete exploitable vulnerabilities |
| Inductive | B1 | High | High | High | Pattern counting across 400+ modules was uniquely valuable |
| Counterfactual | F3 | Medium | Low | High | Confirmed existing decisions more than discovered new issues |
| Perspective-Taking | I4 | High | High | High | "No external users" finding was uniquely valuable |
| Edge-Case | A8 | High | Medium | High | Thorough boundary analysis; disk-full gap was important |
| Second-Order | F6 | High | Medium | High | Causal chains confirmed and extended other modes' findings |
| Scope/Debiasing | L5/L2 | Very High | Very High | Very High | The most provocative and potentially highest-value analysis |

### Most Productive Modes
**F4 (FMEA)** produced the most findings (13) with the highest specificity (RPN scores, specific function references). **H2 (Adversarial)** found the most actionable security issues with concrete exploit paths. **L5/L2 (Scope)** produced the most uncomfortable but important findings about the project's trajectory.

### Least Applicable Modes
**F3 (Counterfactual)** was the least surprising -- it largely confirmed existing decisions as "correct given constraints." In hindsight, a **Bayesian (B3)** or **Game-Theoretic (H1)** mode would have added more unique value. F3's lock-in assessment table was its best contribution.

---

## 12. Taxonomy Axis Analysis

### Ampliative vs Non-Ampliative
The ampliative modes (B1 Inductive, F3 Counterfactual) discovered patterns and alternatives. The non-ampliative mode (A8 Edge-Case) verified boundary conditions. Together they revealed that the project's claims (200+ panes, <50ms latency) are plausible for the core pipeline but unverified at scale, and the surrounding infrastructure is far larger than needed to support those claims.

### Descriptive vs Normative
The descriptive modes (F7, F5, B1) mapped what IS. The normative elements (I4's stakeholder values, L5's scope judgments) assessed what OUGHT TO BE. The strongest disagreements were normative: is the policy engine's complexity justified? Is the forbid(unsafe_code) constraint wise? These are genuine value tradeoffs that the analysis surfaces but cannot resolve.

### Single-Agent vs Multi-Agent
The multi-agent modes (H2, I4) revealed that FrankenTerm is itself a multi-agent system being built by a multi-agent process. This creates a recursive dynamic: the tool for managing AI agent chaos is itself subject to AI agent chaos. H2's finding that the concurrent agent codebase mutation is itself an attack surface was uniquely valuable.

### Truth vs Adoption
The project has deep truth (sophisticated algorithms, formal safety properties) but poor adoption (no working demo without WezTerm, no binary releases, no external users). Every mode that touched this axis found the same gap: the project optimizes for internal sophistication over external usability.

---

## 13. Assumptions Ledger

| # | Assumption | Surfaced By | Justified? | Risk if Wrong |
|---|-----------|-------------|-----------|---------------|
| 1 | 200+ pane scale is achievable with SQLite single-writer | F4, A8 | Partially | Core value prop invalidated |
| 2 | WezTerm bridge can be replaced incrementally | F5, F3, F6 | Partially | Fork divergence locks in dependency |
| 3 | asupersync will continue active development | F3, F5 | Unknown | Runtime migration becomes permanent tax |
| 4 | AI agents produce quality comparable to human review | L5, I4 | No | 229 orphan modules, 32k dead data structures |
| 5 | forbid(unsafe_code) is sufficient for safety | F6, H2 | Partially | Subprocess workarounds push risk to boundaries |
| 6 | External users will materialize | I4 | Unknown | Over-investment in infrastructure nobody uses |
| 7 | 45k tests ensure correctness | L5 | Partially | 2.7:1 test ratio but most test orphan types |
| 8 | Policy engine complexity is justified | H2 vs L5 | Disputed | Either critical safety or wasted effort |
| 9 | Single-branch monorepo works for AI swarm | F5, F6 | Partially | Contention and coordination costs growing |
| 10 | Module count growth is progress | L5, B1, F5 | No | 57% orphans suggest accretion, not architecture |

---

## 14. Contribution Scoreboard

| Mode | Code | Score | Findings | Unique | Evidence Quality | Velocity | Notes |
|------|------|-------|----------|--------|-----------------|----------|-------|
| Failure-Mode | F4 | 92 | 13 | 3 | Very High | High | Highest finding count, RPN scoring |
| Adversarial | H2 | 90 | 8 | 3 | Very High | High | Most actionable security findings |
| Scope/Debiasing | L5/L2 | 88 | 6 | 3 | Very High | Normal | Most provocative trajectory analysis |
| Systems-Thinking | F7 | 82 | 8 | 2 | High | Normal | Best feedback loop mapping |
| Inductive | B1 | 80 | 8 | 2 | High | Normal | Best cross-codebase pattern analysis |
| Root-Cause | F5 | 78 | 8 | 1 | Very High | Normal | Rigorous 5-why chains |
| Perspective-Taking | I4 | 76 | 6 | 1 | Medium-High | Normal | Best stakeholder blind spot detection |
| Edge-Case | A8 | 75 | 10 | 2 | Very High | Normal | Thorough boundary testing |
| Second-Order | F6 | 72 | 6 | 1 | High | Normal | Confirmed and extended other findings |
| Counterfactual | F3 | 65 | 7 | 0 | High | Normal | Confirmed decisions; least novel |

**Diversity Score:** 0.81 -- contributions were reasonably well-distributed. F4 and H2 were the clear standouts due to producing uniquely actionable findings (RPN-scored failures and exploitable vulnerabilities). F3 was the weakest contributor (no unique findings), suggesting a different mode (Bayesian or Game-Theoretic) would have added more value.

---

## 15. Mode Selection Retrospective

### Would You Choose Different Modes?

**Replace F3 (Counterfactual) with B3 (Bayesian).** F3 largely confirmed existing decisions. Bayesian reasoning would have assigned probabilities to key uncertainties (will asupersync development continue? will external users materialize?) and updated them based on evidence, producing more actionable risk calibration.

**Consider adding E3 (Temporal) or J1 (Deontic).** Temporal reasoning would have caught race conditions in the concurrent agent workflow. Deontic reasoning would have formally analyzed the policy engine's obligation/permission model, which is the project's most complex subsystem.

### Axis Coverage Assessment

The Causal axis (F category) was over-represented with 5 modes (F7, F5, F4, F3, F6). While each brought a genuinely different causal lens, replacing one with a Formal (A) or Domain-Specific (K) mode would have improved diversity.

---

## 16. Appendix: Provenance Index

| Finding ID | Source Mode(s) | Tier | Confidence | Report Section |
|------------|---------------|------|-----------|----------------|
| C1 | F7, F5, B1, F3, I4, F6, L5 | KERNEL | 0.95 | 3: Convergent |
| C2 | F7, F5, B1, I4, F6, L5 | KERNEL | 0.92 | 3: Convergent |
| C3 | F5, F3, I4, F6, F7 | KERNEL | 0.88 | 3: Convergent |
| C4 | F7, F5, F3, I4, F6 | KERNEL | 0.87 | 3: Convergent |
| C5 | F4, A8, F6, F7 | KERNEL | 0.83 | 3: Convergent |
| C6 | B1, L5, F5, F3 | KERNEL | 0.90 | 3: Convergent |
| D1 | F3+F7 vs F6 | DISPUTED | 0.70 | 4: Divergent |
| D2 | H2 vs F6+L5 | DISPUTED | 0.65 | 4: Divergent |
| U-F7a | F7 only | HYPOTHESIS | 0.75 | 5: Unique (auto-tuner conflict) |
| U-F4a | F4 only | HYPOTHESIS | 0.90 | 5: Unique (sender restart RPN 336) |
| U-F4b | F4 only | HYPOTHESIS | 0.80 | 5: Unique (FTS5 divergence) |
| U-H2a | H2 only | HYPOTHESIS | 0.85 | 5: Unique (PaneCapabilities caller-supplied) |
| U-H2b | H2 only | HYPOTHESIS | 0.80 | 5: Unique (approval brute-force) |
| U-B1a | B1 only | HYPOTHESIS | 0.85 | 5: Unique (timestamp anarchy) |
| U-I4a | I4 only | HYPOTHESIS | 0.90 | 5: Unique (no external users) |
| U-L5a | L5 only | HYPOTHESIS | 0.88 | 5: Unique (test inflation 2.7:1) |
| U-L5b | L5 only | HYPOTHESIS | 0.85 | 5: Unique (agent incentive misalignment) |
| U-A8a | A8 only | HYPOTHESIS | 0.82 | 5: Unique (wall-clock non-monotonicity) |

---

*Analysis performed by 10 Claude Opus 4.6 subagents under lead orchestration. Total analysis time: ~25 minutes. Total tokens consumed across all agents: ~690k. Each agent independently explored the codebase, read relevant source files, and applied its assigned reasoning mode without knowledge of other agents' findings. Convergence across modes represents genuine independent discovery, not information sharing.*
