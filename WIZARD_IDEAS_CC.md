# WIZARD_IDEAS_CC.md — Top Improvement Ideas for FrankenTerm (Top 5 + Next 10)

> Produced by Claude Code (Opus 4.8) on 2026-06-06 after a full read of AGENTS.md
> and README.md plus a six-stream deep code investigation (runtime/Cx discipline,
> Robot/MCP surfaces, policy/mission/tx engines, operating envelope + attestation,
> and a caller-graph audit of the built-but-dormant subsystems).
>
> Method: 30 candidate ideas were generated and evaluated against five criteria —
> (1) addresses *empirically documented* pain (AGENTS.md playbook rules, memory
> corpus, beads backlog), (2) leverage per unit of new code, (3) alignment with
> the project's stated philosophy (event-driven, fail-closed, attested honesty),
> (4) perceivability by both human operators and AI meta-agents, and
> (5) confidence that the building blocks already exist in-tree. The full
> candidate pool with dispositions is in the appendix.

---

## Idea 1 (Best): Closed-Loop Dispatch — Verified-Submit Send with Delivery Receipts

### The idea in one sentence

Upgrade `ft robot send` from a *write receipt* ("bytes reached the pty") to a
*delivery receipt* ("the agent CLI actually accepted and submitted the input"),
with agent-aware auto-remediation and a typed, durable `SubmitReceipt`.

### The problem, with evidence

The single most empirically documented operational failure in this project's own
corpus is **silent non-submission**. AGENTS.md codifies it as playbook rules:

- **SO-2**: "CC panes do *not* auto-submit on `--robot-send`; the message lands in
  the cc input area and stays buffered. Codex panes also need an Enter, and
  frequently need a *second* Enter ~2 seconds after the first."
- **SO-1**: a malformed interrupt can crash a codex pane back to a bare zsh prompt.
- The dispatch-gotcha lesson: codex showing "Messages to be submitted" with an
  empty composer means the input is *queued behind a background operation* — and
  hammering Enter makes it worse.

These rules exist because every operator and meta-agent rediscovers them the hard
way ("~30 minutes of rediscovery per new operator" per AGENTS.md — and far worse
when a swarm runs for an hour with a prompt silently parked in a composer).
Today, `ft robot send` returns an `injection` blob proving bytes were written,
and `--wait-for` can check an *output* pattern — but nothing verifies the
specific thing that actually fails in practice: **submission**.

### How it works

```bash
ft robot send 7 "long dispatch prompt..." --verify-submit
ft robot send 7 "..." --verify-submit --idempotency-key dispatch-ft-abc12-r3
```

Pipeline (all stages already exist as primitives):

1. **Policy gate** (unchanged) → inject text via the existing paste-mode path.
2. **Echo verification**: watch the capture delta (native event bridge when
   available, poll fallback) until the text is observed in the pane's composer
   region.
3. **Submit keystroke as a separate, profile-driven step** — not glued to the
   paste. Submit profiles are *pattern-pack data*, not code: per-agent anchor
   sets for "composer non-empty", "composer cleared", "agent working"
   (cc: spinner / "esc to interrupt"; codex: "Working"; gemini equivalents).
   The live `agent_correlator` (one of the few already-wired inventory
   subsystems) supplies `agent_type` per pane, selecting the profile.
4. **Verification + remediation loop**: if the composer still holds the text
   after T1, apply the profile's remediation (codex: one additional Enter after
   2s; bounded retries). Crucially, the verifier distinguishes four terminal
   states instead of a boolean:
   - `submitted` — composer cleared and working-signal observed
   - `queued_behind_operation` — codex "Messages to be submitted" + empty
     composer → **do not retry Enter**; return the typed state with a hint
   - `stuck_in_composer` — retries exhausted; text still visible
   - `pane_crashed_to_shell` — shell-prompt pattern detected (the SO-1 failure);
     hint suggests re-launching the agent CLI
5. **Durable receipt**: `SubmitReceipt { state, attempts, evidence_rule_id,
   elapsed_ms, polls }` persisted alongside the existing audit row, replayable
   via `--idempotency-key` exactly like `profiles_applied_log` receipts.
6. **Workflow integration**: expose the same primitive to handlers
   (`ctx.send_verified(...)`) so `HandleCompaction` / `HandleUsageLimits` — the
   flagship auto-recovery features — stop being vulnerable to the same silent
   failure they exist to fix.

### Implementation path

- `crates/frankenterm-core/src/robot_types.rs`: add `SubmitReceipt` +
  `SendData.submit` field (additive, schema-compatible; `schema_version`
  machinery already exists for this).
- Submit profiles as a new rule-pack section (`builtin:core` additions with
  fixture coverage — the fixture-first rule-drift workflow and
  `ft robot rules lint --fixtures --strict` already enforce exactly this kind of
  pattern hygiene, so CLI-version drift has a paved maintenance road).
- Send path in `crates/frankenterm/src/main.rs` (~28158–28523) and the MCP
  `WaSendTool` mirror: insert the verification stage; it reuses the wait-for
  poller verbatim.
- Estimated scope: one coherent feature bead with sub-beads per agent profile.
  No new dependencies, no schema migration beyond one receipt column/table.

### How users and AI agents will perceive it

- **Meta-agents** get a trustworthy boolean where today they have superstition.
  The entire SO-1/SO-2 double-Enter bash folklore collapses into one flag. A
  meta-agent that dispatches 200 prompts/day currently eats a few-percent silent
  failure rate that manifests as "pane idle for an hour"; that class disappears
  or becomes a *typed, actionable* error.
- **Operators** stop writing `sleep 2 && tmux send-keys Enter` and stop staring
  at panes wondering if the prompt landed.
- **Competitively**, this is a moat feature: tmux/zellij/raw WezTerm *cannot* do
  this because they don't own capture + pattern detection. ft uniquely closes
  the actuation loop with its own observation pipeline — which is the project's
  thesis ("observe, detect, react") applied to its own control surface.

### Why I'm confident this is the best idea

Highest (pain × uniqueness × feasibility) product of all 30 candidates. The pain
is not hypothetical — it's codified in this repo's own operator doctrine as the
top two playbook rules. Every required mechanism (policy-gated injection,
capture deltas, wait-for polling, pattern packs with lint + fixtures, live
agent-type classification, durable idempotent receipts) ships today; this is
composition, not invention. And it converts the platform's core promise —
"event-driven, deterministic, no sleep-and-pray" — into reality at the exact
point where swarms currently bleed the most compute.

**Risks & mitigations**: agent-CLI UI drift breaks composer detection → that is
precisely what the fixture-first rule-drift workflow exists for; additionally
the verifier fails *open with honesty*: if no profile matches, return
`verification_unavailable` in the receipt and behave exactly as today, so the
flag can never make sends less reliable than the status quo.

---

## Idea 2: `ft robot watch-events` — A First-Class Event Subscription Surface for Agents

### The idea in one sentence

Give Robot Mode a streaming/blocking event-consumption primitive — NDJSON
`--follow` with resume cursors, plus a composite-condition `ft robot await` —
so meta-agents stop polling in shell loops.

### The problem, with evidence

Design Philosophy #2 declares "Event-Driven, Not Time-Based… No `sleep(5)` loops
hoping the agent is ready." Yet the README's own flagship scenarios (1 and 5)
teach meta-agents to write:

```bash
while sleep 5; do
  ft robot --format json events --unhandled --limit 50 | jq -c '.data[]' | ...
done
```

That's the anti-pattern the project was built to kill, surviving at the most
important integration point — the meta-agent main loop. Costs: mean detection
latency of half the poll interval; a fresh process spawn + DB open + full
envelope serialization per poll (token cost when an LLM is the consumer);
and per-operator reinvented fusion glue. `ft robot wait-for` exists but is
single-pane/single-pattern. SSE streaming exists but only behind
`--features web` + an HTTP server — not where CLI-driven agents live.

### How it works

```bash
# Streaming: one NDJSON envelope per event, heartbeats when idle, cursor in every record
ft robot watch-events --follow --severity warn --rule-id '*.usage.reached' --cursor <tok>

# One-shot composite blocking (generalizes wait-for):
ft robot await --any 'pane:7 rule:codex.usage.reached' \
               --any 'pane:7 state:stuck' --timeout-secs 3600
```

- **Transport**: when the watcher is alive, subscribe to the in-process
  `EventBus` over the existing local IPC layer (`ipc.rs` was built precisely for
  push-based local subscription, token-authed, local-only scope). When only the
  DB is reachable, fall back transparently to storage-cursor tailing on the
  `events` table (monotone `id` cursor).
- **Resume semantics**: every emitted record carries a cursor token;
  `--cursor` resumes after disconnect/watcher restart with documented
  at-least-once delivery. `--claim` optionally marks events handled on emission,
  composing with the existing `--unhandled` flow.
- **Safety**: redaction-before-emission and `max_hz` bounding are lifted
  verbatim from the SSE streamer; the bus is already a *bounded* broadcast, so
  lag is surfaced as an explicit gap event rather than silent loss — matching
  the "no silent gaps" guarantee capture already makes.
- **Composite await**: any/all conditions over rules, pane-state classifications
  (Active/Thinking/Stuck/Idle are already computed continuously), and
  quiescence (the atomic-CAS quiescence gauges exist in `wait.rs`).
- **MCP mirror**: `wa.await_event` long-poll tool now; resource subscription on
  `wa://events` where MCP hosts support it.

### Implementation path

Pure composition: `EventBus` (bounded broadcast) + SSE fanout logic (filtering,
max_hz, redaction) + IPC server + `events` table cursors all exist. New code is
the CLI verbs, the NDJSON framing, the cursor contract, and a golden-matrix
robot-contract doc per the established robot-contract methodology
(`docs/robot-contracts/profile.md` is the canonical template to copy).

### How users and AI agents will perceive it

The meta-agent main loop becomes `for event in stream: react` — the
`kubectl get --watch` analog of the README's own "Kubernetes for terminal-based
AI agents" pitch. Latency drops from seconds to milliseconds (with the native
push bridge) and the steady-state token/process cost of an *idle* swarm drops to
~zero. Every scenario in the README gets rewritten shorter and more correct,
which is exactly how prospective users will judge the project's ergonomics.

### Why I'm confident

This closes a self-acknowledged contradiction between the project's philosophy
and its primary documented usage pattern, benefits *every* meta-agent
integration (the broadest audience of any candidate), and carries low technical
risk because the fanout, bounding, and redaction machinery is already proven in
the SSE path. The main design risk — delivery semantics across watcher
restarts — has a clean, honest answer (cursored at-least-once with explicit lag
events) that matches guarantees the platform already makes elsewhere.

---

## Idea 3: Dead-Wire Closure — Activate BOCPD, Connector Reliability/Governor, and the Capacity Governor; Install a Permanent "Wiring Attestation" Gate

### The idea in one sentence

Finish the last 5% of three fully-built, fully-tested decision subsystems by
wiring them into live dispatch behind shadow mode — and add a CI/attestation
gate that makes "built-but-never-consulted" a *detectable, fail-closed defect
class* so it can never silently recur.

### The problem, with evidence

A caller-graph audit confirms a recurring organizational failure mode — major
decision subsystems get built, property-tested, benchmarked… and never consulted
on a production path:

- **BOCPD** (`bocpd.rs`): complete Adams–MacKay implementation with a <50µs/obs
  target, proptests and benches — *zero production feeders*. Meanwhile the
  README's capability table and data-flow ("Detect — pattern engine evaluates
  new content (BOCPD parallel)") claim it's live. **Under the project's own
  reality-check doctrine, this is an unattested headline claim.**
- **Connector reliability + governor** (ft-x3211): instantiated inside
  `PolicyEngine` but `allow_operation()` / `evaluate()` are never invoked on
  dispatch — telemetry snapshots only. All the circuit-breaker/DLQ/rate-budget
  logic is inert in prod.
- **Capacity governor** (ft-3681t.7.3): only instantiated in the test-gated
  chaos harness, never by the envelope.
- Same family, config flavor: `SearchConfig.models_dir` parsed but ignored
  (ft-jl09u). And the intervention console + fleet dashboard have zero
  production constructors.

This is also the same root pattern as the thrice-repeated frankenterm-core
deletion incident: work whose *presence* isn't structurally enforced gets lost.

### How it works

**Phase 0 — Shadow mode (zero behavior risk).** Run each engine
observe-only: compute the decision it *would* make on live traffic, log
divergence, surface counts in `ft doctor --json`. The `shadow_mode_evaluator`
module already exists for exactly this evaluation shape.

**Phase 1 — BOCPD.** The scan pipeline's Stage-1 metrics (newline rate, ANSI
density, chunk cadence) are already computed per chunk; feed them as
`OutputFeatures` into a per-pane `BocpdManager` in watcher Stage 7. Emit
`bocpd.change_point` events at **info severity**, deduped via the existing
notification identity keys, triggering the already-built context-snapshot hook.
Workflows opt in explicitly. This makes the README claim true and delivers the
"catch novel failures regex misses" capability — infinite-loop output cadence,
silent degradation, behavioral drift across multi-hour sessions.

**Phase 2 — Connector reliability/governor.** At `ConnectorOutboundBridge`
dispatch sites, consult `reliability.allow_operation()` and
`governor.evaluate()`; denials become typed `connector.governor_denied`
envelopes (start per-connector in log-only mode, flip to enforcing per
connector). The instances already live inside `PolicyEngine` — this is adding
the consult call at identified call sites, not new architecture.

**Phase 3 — Capacity governor → envelope actuator.** Register
`GovernorDecision` (Allow/Throttle/Offload/Block) as an operating-envelope
input for heavy workload classes, preserving the envelope's fail-closed
posture and reason-code taxonomy (`capacity.*`).

**The permanent gate — `doctrine/wiring-status`.** Extend `frankenterm-topo`
(which already does workspace-graph analysis for cycle detection) with a
*deadwire check*: a registry of designated decision-API entry points
(`allow_operation`, `evaluate`, `execute`, `observe`, …) where CI fails if an
entry point has zero non-test production callers — unless it is explicitly
declared `dormant` in a manifest slot with a bead reference. Publish the
inventory as an attestation artifact so README capability claims can cite
wiring status the same way perf claims cite benchmark slots. This is the same
move the project already made successfully three times: cycles → enforced by
`frankenterm-topo`; README numbers → enforced by the count-stamper; tokio →
enforced by cargo-deny + sealed trait. "Is this subsystem actually consulted?"
becomes the fourth structurally-enforced invariant.

### How users and AI agents will perceive it

Operators gain real protections they reasonably believe they already have
(circuit breakers on outbound connectors; novel-failure detection; capacity
admission informed by workload class). Auditors — the README explicitly courts
them ("Auditing the claims" persona) — see the trust story get *stronger*:
either a claim is wired and attested, or it's marked dormant with a bead. AI
agents working in the repo get a CI signal that stops them from repeating the
build-and-forget pattern.

### Why I'm confident

This is the highest capability-per-new-line idea possible: the expensive 95%
(algorithms, tests, benches, telemetry types) is finished and rotting. The
integration sites are small and identified. Shadow mode removes the
threshold-tuning risk for BOCPD; per-connector log-only rollout removes the
breakage risk for the governor. And the meta-gate attacks the *generating
process* of the defect, which the project's history shows is worth more than
any single wiring. Honestly, by leverage this could be ranked #1; it sits at #3
only because Ideas 1–2 have sharper single-feature user-visible payoffs.

**Risks & mitigations**: BOCPD false-positive storms → conservative default
hazard (1/200), info-only severity, identity-key dedup, shadow-mode calibration
first. Governor regressions → permissive/log-only default, per-connector
enforcement flips, typed envelopes with hints.

---

## Idea 4: `ft robot next` — The Meta-Agent's Single Decision Call

### The idea in one sentence

One read-only, token-cheap robot endpoint that fuses envelope verdict, unhandled
events, stuck-pane classification, ready work, and pending approvals into a
ranked, explained "here is what deserves attention right now" envelope.

### The problem, with evidence

A meta-agent's decision loop today requires 4–6 separate calls
(`robot state`, `robot events --unhandled`, `robot work ready`,
`mission objective-plan`, `doctor --json`, approvals) plus bespoke fusion logic
that every operator rewrites — at full token cost per call. The project already
*proved* the fix pattern on the issue-graph side: AGENTS.md anoints
`bv --robot-triage` as "THE MEGA-COMMAND: start here," with `quick_ref`,
ranked `recommendations`, and copy-paste `commands`. The live fleet has no
equivalent; the most important consumer (the meta-agent) gets the least
ergonomic surface.

### How it works

```bash
ft robot next                      # full ranked attention list
ft robot next --budget-tokens 800  # deterministic elision to a size budget
ft robot --format toon next        # AI-to-AI
```

Response shape (additive, golden-matrixed like every robot family):

```json
{
  "ok": true,
  "data": {
    "fleet": {"panes": 42, "active": 31, "thinking": 6, "stuck": 2, "idle": 3},
    "admission": {"verdict": "envelope.admit", "max_parallel_agents": 4},
    "attention": [
      {
        "kind": "detection",
        "pane_id": 7,
        "rule_id": "codex.usage.reached",
        "urgency": 0.92,
        "reasons": ["severity=warn", "unhandled for 214s", "pane was active"],
        "suggested_command": "ft robot send 7 \"/compact\" --verify-submit"
      },
      {
        "kind": "stuck_pane",
        "pane_id": 13,
        "urgency": 0.71,
        "reasons": ["no output 312s after input", "watchdog flagged"],
        "suggested_command": "ft robot get-text 13 --tail 80"
      }
    ],
    "approvals_pending": 1,
    "work_ready": 3,
    "cursor": "evt:18421"
  }
}
```

Every input is an existing shipped read path: pane-state classification
(Active/Thinking/Stuck/Idle), the unhandled-events queue, the operating-envelope
verdict, the `work_claims` queue, the approvals table, and doctor red flags. New
code is a ranking module (severity × age × pane-priority, with mandatory
`reasons[]` so the ranking is advisory and explainable, never a black box),
deterministic ordering for golden-fixture testing, and the contract doc.

### How users and AI agents will perceive it

It flattens the meta-agent on-ramp from "read six API families and write fusion
glue" to "call `next`, act, repeat" — and pairs naturally with Idea 2
(`watch-events` for reactivity, `next` for periodic re-orientation and
post-compaction context rebuild). For the README's "Building a meta-agent"
persona, this becomes the first command they learn. The `suggested_command`
field makes it self-teaching, the same trick that makes `bv --robot-triage`
beloved. Token cost per orientation drops several-fold.

### Why I'm confident — and why it's #4 not #1

Confidence is high because it's read-only, purely additive, composed entirely
of shipped surfaces, and replicates a pattern (`--robot-triage`) the project has
already validated with its actual users (agents). It ranks below Ideas 1–3 only
because it's a *convenience composite* — a determined operator can approximate
it today — whereas 1–3 create capabilities that cannot be approximated from
outside the platform.

**Risks & mitigations**: ranking-quality disputes → keep it advisory with
explicit `reasons[]`, tune via golden fixtures; envelope-call latency → all
inputs are local reads, well within the <5 ms robot-response budget except the
envelope synthesis, which can be cached with an explicit freshness stamp
(matching the envelope's own provenance/freshness discipline).

---

## Idea 5: Time-Travel CI — Replay-Driven Regression Gating with Incident Promotion

### The idea in one sentence

Put the 25k-LOC replay subsystem to continuous work: replay a curated corpus of
recorded swarm sessions against every PR, diff decision graphs and policy
decisions against goldens, fail on unexplained behavioral drift, publish the
verdict as an attestation slot — and let every real incident be promoted into a
permanent regression fixture.

### The problem, with evidence

`frankenterm-core-replay` (~25k LOC: normalized causal DAG, virtual clock, diff
engine, pass/fail/degraded assessment scorer) and the recorder exist, and
"byte-equal replay" is the project's *stated definition* of determinism — but
nothing exercises this continuously. Meanwhile the substrate-audit history shows
exactly the bug classes unit tests miss and replay catches: rubber-stamp
`is_safe()` (×17), policy decisions drifting on cold starts, workflow
nondeterminism (wall-clock reads, unseeded RNG, HashMap iteration order),
off-by-N event causality under load. Today a PR that subtly changes a policy
decision path ships if no unit test pinned that exact path.

### How it works

1. **Corpus**: start from the demo-lab deterministic fixtures (`quickstart`,
   `usage_limit`, `compaction` already ship with manifests and retained
   artifacts) plus recorded sessions covering: a rate-limit storm with
   auto-handling, a policy-denial + approval flow, a tx run with injected
   commit failure (the `--fail-step` injection hook exists for exactly this),
   and a gap/restore cycle.
2. **CI lane**: for each fixture, replay under the virtual clock against the PR
   build; diff (a) the decision DAG, (b) every policy decision + reason code,
   (c) workflow outcomes and receipts against the golden. The diff engine and
   scorer already produce pass/fail/degraded verdicts.
3. **Drift classifier**: separate *expected* drift (a rule pack or policy
   default legitimately changed — the diff cites which rule) from *behavioral*
   drift (same inputs, different decision path). Expected drift requires an
   explicit bless, reusing the `BLESS=1` re-bless convention the repo already
   uses for the unix-coupling ratchet baseline.
4. **Attestation**: publish the run as a `proofs/replay-regression` manifest
   slot, so "behavior is stable across releases" becomes a *verifiable* claim
   in the bundle rather than an implied one.
5. **Incident promotion**: `ft replay promote <incident-bundle> --as-fixture` —
   incident bundles already capture flight-recorder source sets; promoting one
   turns every production incident into a permanent regression test, the
   highest-value test-generation flywheel a platform like this can have.

### How users and AI agents will perceive it

Contributors (largely AI agents, dozens concurrent per the repo's own doctrine)
gain the confidence to touch policy/workflow/tx code — currently the scariest
surfaces — because the CI lane proves end-to-end decision stability, not just
unit-level invariants. Release auditors get a living, signed artifact that the
swarm's *behavior*, not merely its code, is regression-controlled. And the
replay crate stops being a beautifully-engineered museum piece.

### Why I'm confident — and the honest caveat

Confidence in mechanics is high: determinism is already defined, implemented,
and invariant-tested (VirtualClock NaN rejection, byte-equality definition);
the diff engine and demo fixtures exist; the attestation pipeline is mature.
This ranks #5 because it carries the largest *ongoing* cost of the five:
fixture maintenance when rule packs legitimately evolve. The drift classifier +
bless workflow bounds that cost, but it never reaches zero — which is the one
respect in which this idea is less "obviously" accretive than the four above
it. It earns its slot because the protection compounds: every release, every
incident promoted, the corpus gets more valuable while every other idea's value
stays constant.

---

## Appendix: The Full 30-Candidate Pool and Dispositions

| # | Candidate | Disposition |
|---|---|---|
| 1 | Verified-submit dispatch with delivery receipts | **Top 5 — #1** |
| 2 | `ft robot watch-events --follow` NDJSON streaming | **Top 5 — #2** |
| 3 | `ft robot await` composite any/all conditions | Folded into #2 |
| 4 | Event cursor/resume tokens for exactly-once-ish consumption | Folded into #2 |
| 5 | Wire BOCPD into the live watcher detect stage | **Top 5 — #3** |
| 6 | Consult connector reliability+governor on outbound dispatch (ft-x3211) | **Top 5 — #3** |
| 7 | Capacity governor as operating-envelope actuator | **Top 5 — #3** |
| 8 | Dead-wire CI gate + `doctrine/wiring-status` attestation slot | **Top 5 — #3** |
| 9 | Config dead-key honesty gate (the `models_dir` class, ft-jl09u) | Folded into #3's gate |
| 10 | `ft robot next` unified triage endpoint | **Top 5 — #4** |
| 11 | Replay-driven regression CI lane | **Top 5 — #5** |
| 12 | Incident-bundle → replay-fixture promotion | Folded into #5 |
| 13 | Wire intervention console to `ft intervene` CLI (pause/takeover/quarantine) | Strong; narrowly cut — operator-only audience, natural follow-on to #3 |
| 14 | Wire fleet dashboard into `ftui` (`ft dashboard`) | Cut — feature-gated audience; after #13 |
| 15 | Approval-queue UX + notification deep-links | Cut — partially covered by #13's ApprovalQueue |
| 16 | wasmtime runtime wiring for the extension substrate | Cut — too heavy for the pragmatic bar this round; types are frozen, runtime is a quarter-scale project |
| 17 | `ft doctor --fix` guided safe remediation with receipts | Good; smaller blast radius than the five |
| 18 | Token-budget-aware responses (`--budget-tokens` elision + continuation cursors) | Partial fold into #4; standalone value niche |
| 19 | Idempotency keys on `robot send` | Folded into #1 |
| 20 | Per-agent submit profiles as pattern-pack data | Folded into #1 |
| 21 | SQLite writer group-commit / explicit-txn hardening (`append_segment_sync`) | Cut — real but invisible; already bead-tracked (ft-6utac family) |
| 22 | Native push-event bridge default-on completion + doctor visibility | Cut — partially shipped; incremental |
| 23 | Stable pane aliases (bookmark-first addressing everywhere) | Cut — bookmarks exist; incremental ergonomics |
| 24 | MCP resource subscription on `wa://events` | Folded into #2's MCP mirror |
| 25 | Prometheus default-on + bundled Grafana dashboard JSON | Cut — packaging, not capability |
| 26 | Multi-pane `wait-for` any/all | Folded into #2's `await` |
| 27 | `ft swarm autopilot` built-in orchestrator loop | Cut — ambitious but scope-collides with ntm's role; revisit after #2/#4 mature |
| 28 | Tx dry-run sandbox against recorded pane behavior | Cut — compelling, but depends on #5's substrate landing first |
| 29 | Complete cass-backed swarm learning index | Cut — blocked on an external tool's indexing backend |
| 30 | Instance-specific `ft why <correlation_id>` decision-path explainer | Good; smaller than the five; pairs well with #4's `reasons[]` |

---

---

# Part II — Ideas 6 through 15: Complementary Angles

> Appended 2026-06-06, same session. These ten ideas deliberately cover the
> angles the top 5 did not: the desired-state orchestration layer, fleet
> economics, conversational-state durability, cross-pane security, the human
> operator's cockpit, the unproven target-class gate, the extension ecosystem,
> forensics ergonomics, the integration surface, and operator toil. Ideas 6, 8,
> 9, and 12 are the radically ambitious entries; each is grounded in primitives
> that already ship. Several promote candidates that Part I's appendix cut
> (#13/14/15/16/17/21) — promoted here either because they merge into something
> larger than their parts or because the explicit ambition mandate changes the
> cost/benefit calculus. Ordered best → worst within this tier.

---

## Idea 6: Declarative Fleet Reconciliation — `ft fleet apply` and the Missing Control Loop

### The idea in one sentence

Add the one thing the "Kubernetes for terminal-based AI agents" pitch is
missing — a reconciler: operators declare a desired fleet (`fleet.yaml`), and a
control loop inside the watcher continuously converges observed reality to it,
through the policy gate, the tx engine, and the operating envelope.

### The problem, with evidence

The README's closest-analogy claim is Kubernetes, and the platform has built
nearly every Kubernetes-shaped primitive: **profiles** (pod templates),
**fleet templates** (deployments — "a composition of profiles with counts and
dependencies"), **`ft robot profile apply --count N`** with idempotent receipts
and mid-apply rollback, **`ft robot fleet scale/rebalance`** with durable
mutation receipts, **operating-envelope admission** (scheduler admission
control), continuous **pane-state classification** (liveness/readiness), and a
**watchdog**. What it conspicuously lacks is Kubernetes' actual soul: the
reconciliation loop. Today every primitive is imperative and one-shot. A pane
crashes at 02:14 → the fleet stays degraded until a human or meta-agent
notices. The platform can *detect* "agent died" in milliseconds and can *spawn*
agents transactionally — and connects those two abilities with nothing.

### How it works

```yaml
# fleet.yaml
fleet_version: 1
name: pricing-refactor-swarm
template: swarm_5x_codex_1x_claude       # existing fleet-template composition
overrides:
  codex_ws: { count: 5, restart_policy: on_failure, max_restarts: 3 }
  claude_reviewer: { count: 1, restart_policy: always }
placement:
  envelope_strictness: strict             # admission via ft.operating_envelope.v1
reconcile:
  max_actions_per_tick: 2                 # actuation budget
  backoff: { initial_secs: 10, max_secs: 600 }   # CrashLoopBackOff-equivalent
```

```bash
ft fleet apply fleet.yaml          # validate against envelope, persist desired state
ft fleet status                    # desired vs observed diff, per-profile
ft fleet delete pricing-refactor-swarm --keep-panes
```

The reconciler runs as a maintenance-stage task in the existing watcher tick:
**observe** (pane registry + agent-state classification) → **diff** against the
persisted desired state → **plan** (respawn missing/crashed panes per
`restart_policy`, scale down extras, optionally replace `Stuck` panes) →
**execute** each action through the tx engine (prepare/commit/compensate — so a
half-failed respawn batch rolls back cleanly) with durable receipts, exactly as
`profile apply` does today. Every action is policy-gated and envelope-checked;
under `Critical`/`Emergency` fleet pressure the reconciler pauses actuation and
emits `fleet.reconcile.suspended` (the fail-closed posture, inherited rather
than reinvented). Drift and convergence are first-class events on the bus —
which means Idea 2's `watch-events` and Idea 4's `next` surface them for free.

### Implementation path

- New `fleet_reconciler.rs` module + a `desired_fleets` table (desired-state
  spec, content-hashed like plans).
- Planner reuses the native `fleet scale` plan builders; executor reuses
  profile-apply spawn paths and tx receipts; admission reuses
  `mission_objective_plan`'s envelope-consultation pattern.
- Kill-switch integration: `SafeMode` restricts the reconciler to read-only
  diff reporting; `HardStop` halts it — semantics already defined in
  `MissionKillSwitchLevel`.

### How users and AI agents will perceive it

Operators get `kubectl apply` for agent swarms — the single most legible
feature imaginable for the platform's stated audience, with semantics
(restart policies, backoff, crash-loop states) they already know from
Kubernetes. Meta-agents stop spending their own loop iterations on babysitting
topology and spend them on work. The README's analogy stops being an analogy.

### Why I'm confident, and the risks

Every load-bearing piece (templates, transactional spawn, receipts, admission,
state classification, kill switches) is shipped and tested; the new code is the
diff/plan loop and a table. The risks are classic controller risks with classic
answers: **flapping** → actuation budgets per tick + asymmetric
hysteresis (the fleet memory controller already establishes this house
pattern); **runaway respawn** of a crash-looping agent → exponential backoff
with a visible `crash_loop_backoff` state; **fighting the operator** → desired
state is only ever changed by explicit `apply`, and manual panes outside any
fleet are never touched. This is the boldest idea in Part II and also one of
the best-supported.

---

## Idea 7: Rate-Limit-Aware Scheduling and the Fleet Economics Ledger

### The idea in one sentence

Turn the rate-limit detections ft already produces into a forward-looking
capacity ledger — who is limited, when each window resets, what the fleet's
usable capacity looks like over the next N hours — and make dispatch surfaces
(work assignment, workflows, the Idea-6 reconciler) consult it.

### The problem, with evidence

The TL;DR's very first concrete pain is economic: "a single undetected rate
limit wastes hours of compute." The platform's answer so far is *reactive*
(detect the limit, fire `/compact`). But the detection rules already capture
the *forward-looking* fact — reset timestamps ("Usage limit reached. Try again
at 2026-01-20 12:34 UTC" is literally the canonical fixture, and the
rate-limit retry-duration regex was specifically tightened in 2026-04) — and
then throw it away. Nothing in the platform knows that pane 7 becomes useful
again at 14:00, so meta-agents either poll dead panes or forget them. At
50–200 panes across multiple accounts, the gap between "fleet size" and
"usable fleet size over time" is the operator's real capacity number, and no
tool in this space computes it.

### How it works

1. **Limit-window ledger**: when a `*.usage.reached` / `*.rate_limit.detected`
   rule fires, parse the reset time (the regex machinery exists) into a
   `limit_windows` table keyed by pane — and by account, via the existing
   `accounts` table linkage.
2. **`ft robot limits`**: a read surface returning current limits, per-window
   reset times, and a fleet capacity forecast timeline ("usable panes: now 31,
   at 14:00 → 36, at 16:30 → 42"). TOON-friendly; feeds Idea 4's `next` and
   Idea 10's deck.
3. **Scheduling hooks**: `ft robot work assign` declines (with a typed reason +
   the reset time) to assign to limited panes; workflows can declare
   `requires_unlimited_pane`; the Idea-6 reconciler treats limited panes as
   temporarily unschedulable rather than dead.
4. **Reset events**: at window expiry the watcher emits `limit.window.reset` on
   the bus — so with Idea 2, a meta-agent's dispatcher resumes the moment
   capacity returns, event-driven rather than polled.
5. **Economics attribution (estimates, clearly labeled)**: per-pane activity
   proxies (bytes captured, active-state seconds, detection counts) aggregated
   by the correlation/bead IDs already present on audit rows → "cost per bead /
   per mission" panels in the cockpit and Prometheus. Never attested as exact —
   the honesty doctrine applies; these are labeled estimates.

### How users and AI agents will perceive it

This is the feature operators describe in money. A swarm that previously ran at
~70% effective capacity because limited panes sat idle past their resets — or
got hammered with retries while limited — runs near its true ceiling.
Meta-agents get the single most decision-relevant fact ft can offer ("where is
capacity *going to be*") in one call. And it composes multiplicatively with
Ideas 2/4/6 rather than overlapping them.

### Why I'm confident, and the risks

The hard sensing problem is already solved — the rules detect limits today,
and the reset-time regex exists and is fixture-tested. The new work is a
table, a read surface, three consult hooks, and a timer-to-event bridge.
Risks: reset-timestamp formats drift across agent-CLI versions → exactly what
the fixture-first rule-drift workflow is for, and unparseable resets degrade
gracefully to "limited, reset unknown" with a conservative TTL; cost proxies
get over-trusted → keep them out of attestation slots and label them
estimates in every surface.

---

## Idea 8: Durable Agent Sessions — Crash-Respawn with Agent-Native Resume

### The idea in one sentence

When ft restores or respawns a pane, don't just restore the terminal — resume
the *agent*: re-launch the agent CLI with its native session-resume invocation
(`claude --resume <id>`, codex's resume flow) so the conversation, context,
and in-flight task survive pane death.

### The problem, with evidence

Session persistence today restores topology and scrollback — the *terminal*
comes back, but the agent inside it comes back amnesiac, and an amnesiac agent
mid-refactor is often worse than no agent. Meanwhile every piece of the
solution exists separately: the agent correlator (verified live, wired into
`snapshot_engine.rs`) already extracts per-pane agent type **and session IDs**
into checkpoint `AgentMetadata`; the context registry persists rotation
metadata; the agent CLIs themselves all support native resume; and restore
already maps old pane IDs to new ones. Nothing connects "I know pane 7 was
claude session `abc-123`" to "spawn the replacement with `--resume abc-123`."

### How it works

1. **Capture**: checkpoints already record agent type + session identifiers per
   pane (no new collection; no conversation content stored — same privacy
   stance as the existing context registry).
2. **Resume profiles as pack data**: per-agent-type resume invocations and
   their success/failure markers, maintained exactly like Idea 1's submit
   profiles (fixtures, lint, drift workflow).
3. **Restore/respawn hook**: `ft snapshot restore --resume-agents`, and the
   Idea-6 reconciler's respawn path, launch the profile's resume command
   instead of a bare agent, then verify via Idea 1's verified-submit machinery
   that the agent actually came back in-session.
4. **Honest fallback ladder** with a typed receipt: `resumed` (verified) →
   `fresh_with_context` (native resume failed; inject a generated context
   preamble built from the pane's work claim, last events, and bead ID — "you
   were working on ft-xxxxx; last detection was …") → `fresh_blind`. The
   receipt never claims `resumed` without observed evidence.

### How users and AI agents will perceive it

This changes what a pane *is*: from a pet that dies with its memory to a
migratable workload. Host reboot, OOM kill, upgrade, or accidental window
close stop being incidents that destroy hours of agent context — with Idea 6,
the fleet self-heals *with memory*. Operators of long-running swarms (this
repo's own development model) will recognize it instantly as the feature they
lose the most real work to lacking.

### Why I'm confident, and the risks

The riskiest input — "do we reliably know the session ID?" — is already
answered by shipped, wired code (the one dormant-audit subsystem that proved
LIVE). The genuinely uncertain part, agent-CLI resume behavior varying across
versions, is contained by the fallback ladder: the worst case equals today's
behavior plus an honest receipt, so the feature cannot regress anything. It
ranks below 6–7 because its ceiling depends on third-party CLI resume quality,
which ft can verify but not control.

---

## Idea 9: Cross-Pane Taint & Provenance — Prompt-Injection Defense at the Control Plane, plus Canary Secrets

### The idea in one sentence

Make ft the first terminal platform with a cross-agent information-flow
defense: label panes with trust levels, track (heuristically) when text read
from a low-trust pane flows into input sent to a high-trust pane, escalate
policy on such flows — and plant canary secrets whose appearance anywhere
outbound is a tripwire.

### The problem, with evidence

Prompt injection between agents is *the* security problem of the swarm era,
and the threat model already half-names it: "low-trust pane output" is in
scope, and ft-j0ufc closed one instance of the class (low-trust output firing
high-trust *workflows*). But the general channel remains wide open: a
meta-agent reads pane 13's output (`get-text`), pane 13's output contains
adversarial instructions, and the meta-agent dutifully forwards a derived
command to high-trust pane 2. No terminal, mux, or agent framework defends
this hop. ft is in a structurally unique position to: it mediates **every**
read (policy-evaluated, redacted) and **every** write (policy-gated) — it is
the man-in-the-middle by design.

### How it works

1. **Trust labels**: panes get a trust level (declared in profiles, settable
   live), generalizing ft-j0ufc's per-workflow allowlists into a fleet-wide
   property.
2. **Provenance receipts on reads**: when `get-text`/`search` returns content
   from pane A to caller C, record bounded content fingerprints (n-gram or
   MinHash sketches) tagged (caller, source pane, trust, timestamp). Sketches,
   not raw text — consistent with the registry's no-content stance.
3. **Flow check on writes**: when caller C sends text to pane B across a trust
   boundary (low → high), fingerprint-match the send against C's recent
   low-trust reads. Above threshold, the policy verdict escalates —
   `Allow` becomes `RequireApproval` with reason
   `policy.tainted_flow { source_pane, similarity }`, riding the existing
   approval-token machinery unchanged.
4. **Canary secrets**: `ft canary plant` mints unique decoy tokens registered
   in the redactor catalog as critical patterns; a canary appearing in any
   outbound surface (send text, connector egress, webhook, notification) fires
   `security.canary_tripped` at critical severity with full provenance. Cheap,
   zero-false-positive exfiltration tripwires.
5. **Observe-first rollout**: phase one only *annotates* audit rows with taint
   metadata (zero behavior change, calibration data for thresholds); phase two
   enforces on explicitly-declared trust boundaries.

### How users and AI agents will perceive it

Security-conscious adopters get an articulable, demoable answer to the first
question any security review of a 200-agent swarm asks. The approval prompt is
self-explaining ("this text appears derived from low-trust pane 13's output —
approve?"). And it deepens the platform's moat: this defense is only buildable
by the component that owns both the read and write paths.

### Why I'm confident, and the risks — stated honestly

Confidence in the *plumbing* is high (labels, policy escalation, redactor
patterns, audit annotations are all extensions of shipped machinery). The
honest caveat is that sketch-based taint is heuristic, not information-flow
complete — a paraphrasing meta-agent defeats n-gram matching. That's why the
design is defense-in-depth (canaries catch what taint misses; both are
attested as heuristic layers, per the house honesty rule that overclaiming is
worse than scoping), why observe-mode precedes enforcement, and why it ranks
#9 rather than higher despite being the most novel idea in the document.
False-positive cost is bounded by enforcement applying only at declared trust
boundaries; CPU cost is bounded by sketching only reads/writes that cross one.

---

## Idea 10: `ft deck` — The Operator's Command Deck (Activating the Intervention Console and Fleet Dashboard)

### The idea in one sentence

Wire the two remaining dormant human-facing subsystems — the intervention
console (pause/takeover/quarantine + ApprovalQueue + AuditTrail, fully built,
zero production constructors) and the fleet dashboard crate (alert manager +
dedup + dashboard view, zero importers) — into one ftui surface plus matching
`ft intervene` CLI verbs.

### The problem, with evidence

The platform's asymmetry is stark: AI agents got a meticulously-contracted
control plane (Robot Mode, MCP, TOON), while the human operator got scattered
one-shot commands across four terminals — `status --health`, `events`,
`audit`, `approve <code>` — and two complete, tested cockpit subsystems
sitting unreachable in the tree. Approvals are the sharpest pain: the
RequireApproval flow is a cornerstone safety feature, yet the operator
experience is "notice an 8-char code in scrollback, retype it elsewhere."
Part I's appendix cut these as three separate ideas (#13/14/15); merged, they
are one coherent product surface with most of its engine already written.

### How it works

```bash
ft deck            # ftui: fleet grid + attention + approvals + interventions
ft intervene pause 7 --reason "runaway loop"     # scriptable non-TUI verbs
ft intervene quarantine 13
ft approvals list / approve <id>                 # queue, not scrollback archaeology
```

One ftui screen composed of four panels, each backed by existing machinery:
a **fleet grid** colored by the live Active/Thinking/Stuck/Idle classification
(the states that "drive GUI pane border colors" today); an **attention list**
(Idea 4's ranked output if landed, else events+stuck fusion); an **approval
queue** rendering the intervention console's ApprovalQueue with one-key
approve writing the same one-shot approval tokens; and an **interventions
panel** routing PausePane/TakeoverPane/QuarantinePane through the policy gate
with the console's own AuditTrail. Desktop notifications for new approvals
deep-link back to the deck. Rollout: read-only grid → approvals → live
interventions.

### Why it earns its slot, and the risks

It completes Idea 3's dead-wire ethos for the human side, and it is the
single highest-leverage *adoption* surface: the deck is what every demo,
screenshot, and first-run experience will show. Implementation cost is modest
because the decision logic, queue semantics, and audit trail are finished —
the work is rendering and wiring. Risks: TUI scope creep → the deck is
constitutionally a *composition of existing read/act surfaces*, never a place
where new logic lives; ftui being feature-gated → ship the `ft intervene` /
`ft approvals` CLI verbs unconditionally so the capability lands even in
minimal builds. It ranks mid-tier only because it serves the human minority of
the platform's users — the agents outnumber us.

---

## Idea 11: Operation Target-Class — Make the 200-Pane Claim Provably True

### The idea in one sentence

A focused campaign to flip the project's biggest asterisk: harden the storage
hot path, build a reproducible 200-pane load rig, run the benchmark lane once
on true target-class hardware, and sign the non-skipped artifact that the
operating envelope and README are both explicitly waiting for.

### The problem, with evidence

The platform's headline differentiator — fleet scale — is the one claim its
own attestation system refuses to bless. `resource-cockpit-target-class.json`
says `skipped_not_proven` ("the current local host and RCH worker pool do not
satisfy the 64 logical CPU / 256 GiB predicate"); the envelope dutifully emits
`capacity.target_class_unproven` and defers high-scale admission; the README
holds back its 200+-pane memory wording. Meanwhile the storage architecture
notes flag the exact bottleneck a 200-pane run would hit: the
`append_segment_sync` single-writer hot path with no explicit transaction
batching (already bead-tracked as ft-6utac / ft-8sjv4 / ft-7yq2z). The
honesty machinery did its job — it found the gap. Nobody has closed it.

### How it works

Three prongs, in order:

1. **Hot-path hardening**: writer-thread group commit — batch queued segments
   into one WAL transaction per flush window, prepared-statement reuse, and
   re-derive the Lindley/min-plus latency bound (the model and its attestation
   slot already exist) against the improved service curve. This work is
   useful at *every* scale, not just 200 panes.
2. **Reproducible load rig**: promote the test-gated `chaos_scale_harness`
   into a runnable 200-pane traffic generator, using replay-corpus sessions
   (Idea 5 synergy) as realistic per-pane output sources rather than synthetic
   noise — so the rig exercises detection, FTS indexing, scrollback tiering,
   and backpressure exactly as production would.
3. **The target-class run**: rent one 64-core/256 GiB cloud instance for a
   day, execute the existing benchmark lane + cockpit conformance suite,
   capture the artifacts, and sign the non-skipped target-class attestation.
   The envelope's gate flips green; the README wording unlocks; the
   per-major-SKU rule (already published for the release bundle) starts being
   satisfiable.

### Why it matters, and the risks

This is the robustness-and-trust idea of this tier: it converts the project's
most prominent fail-closed deferral into its strongest proof point, using
entirely pre-built measurement machinery — the campaign's only novel parts
are the group-commit patch and a few hours of rented hardware. The realistic
"risk" is that the target-class run *finds real regressions* under load —
which is the point; the campaign should expect to file and fix beads, not to
rubber-stamp. The honest framing: budget it as find-and-fix, with the signed
artifact as the exit criterion rather than the first milestone.

---

## Idea 12: WASM Extension Runtime, Phase-Shipped — Detection Rules First

### The idea in one sentence

Wire wasmtime into the frozen extension-type substrate in deliberately narrow
phases — sandboxed *detection rules* first, search rewriters second, workflow
handlers last — turning ft from a product into a platform without taking the
full extension-system risk in one bite.

### The problem, with evidence — and why it's back after being cut

Part I cut wasmtime wiring as "too heavy for the pragmatic bar"; the ambition
mandate changes the calculus, and phasing changes the risk. The substrate is
unusually ready: `extensions.rs` ships a complete, tested capability model
(SandboxCapabilities, FileAccessScope, CapabilityLevel tiers, ExtensionManifest
with 64 MiB / 5 s defaults), the ABI was deliberately frozen early so external
authors could build against it, and the docs sketch the host functions. The
strategic gap it closes is real: today, anyone whose detection needs exceed
regex (custom agents, internal CLIs, proprietary failure signatures) must fork
the rule packs or fork ft. A plugin surface is how observation platforms
(think: editors, browsers, CI systems) historically escape the ceiling of
their built-ins.

### How it works — the phase discipline is the idea

**Phase 1 — detection rules only.** A WASM rule is a pure function:
`(chunk bytes, prior state) → detections`. No host I/O, fuel-budgeted,
deterministic by construction — which means WASM rules are *replay-compatible
for free* (Idea 5 synergy) and the blast radius of a malicious or broken
extension is "wasted fuel," nothing else. They run post-Bloom, only on
candidate chunks, with a per-extension time budget and fail-open-with-telemetry
semantics so the hot detect loop cannot be held hostage. This phase alone
delivers most of the ecosystem value: community and private rule packs with
real logic, no fork, no supply-chain exposure.

**Phase 2 — search rewriters** (query expansion/rerank hooks; read-only).
**Phase 3 — workflow handlers**, where effects exist but only through
host-mediated, policy-gated APIs — every send a WASM handler attempts goes
through the same PolicyEngine as everything else, by construction. Extension
install reuses the connector fabric's certification-probe pattern (signature
validation, capability round-trip, certification table, refuse-to-start).

### Perception, confidence, and risks

Authors get a stable, versioned ABI that was frozen for exactly this moment;
operators get extensibility with a security story stronger than any
plugin-by-subprocess design (capabilities + fuel + no ambient authority).
Confidence in Phase 1 is high — it is a bounded interpreter loop over a frozen
contract; wasmtime is already an optional workspace dependency. The idea
ranks #12 not for doubt about value but for cost: even phased, it is the
largest engineering lift in this document, and Phases 2–3 deserve their own
design reviews. Mitigations are baked in: ship Phase 1 alone, gate everything
behind the existing feature-flag discipline, and let real extension demand
pull Phases 2–3 rather than schedule them.

---

## Idea 13: Fleet Timeline Forensics — `ft timeline --around <event>`

### The idea in one sentence

One command that reconstructs the synchronized, multi-pane state of the world
around any event: every pane a lane on a shared time axis, with segments,
detections, gaps, audit actions, and workflow executions aligned.

### The problem, with evidence

Scenario 6's stated pain — "no way to correlate with the other panes' state at
the moment of failure" — is only half-solved. The data is all there (every
segment, event, and audit row carries timestamps, pane IDs, and correlation
IDs), but reconstructing "what was happening *everywhere* at 02:14" takes N
manual `search`/`get-text`/`audit` invocations and a human doing mental
join-on-timestamp. Post-incident review is the moment operators most need the
platform to shine, and it currently hands them raw materials instead of a
picture.

### How it works

```bash
ft timeline --around event:1247 --window 30s            # aligned lanes, all panes
ft timeline --around "2026-05-16T02:14:00" --panes 7,13 --correlate
ft robot timeline --around event:1247 --window 30s      # structured lanes for AI forensics
```

Output is per-pane lanes on a common axis — output bursts, detections (with
rule IDs), gap markers, policy decisions, workflow starts/ends — as aligned
text, JSON, or (with Idea 10) a deck panel. `--correlate` ranks other panes by
anomaly density in the window (output-rate spikes, gaps, detection clusters)
so the suspicious neighbor surfaces instead of being hunted. The robot variant
gives AI agents the same reconstruction power, which composes with
`HandleOnErrorCassSearch`-style automated diagnosis.

### Why it earns its slot, and the risks

It is pure read-path composition over already-indexed columns — low risk, a
few days of focused work, and disproportionately demoable: the timeline view
is the screenshot that makes the forensics story legible in a way tables of
events never will. Incident bundles can embed a rendered timeline, making
every bug report better. Risks are minor: unbounded windows → enforce window
caps and reuse pagination cursors; rendering ambition → text/JSON first, deck
later. Ranked #13 because it creates convenience over existing capability
rather than new capability — but it is the best convenience-per-effort buy in
the pool.

---

## Idea 14: Contract SDKs and a Verified Schema Registry

### The idea in one sentence

Make the robot/MCP contracts first-class published artifacts: JSON Schemas
generated from the Rust types, CI-verified against the golden envelope
matrices, and thin auto-generated typed clients (`ft-sdk-python`,
`ft-sdk-ts`) so integrators stop hand-rolling envelope parsing.

### The problem, with evidence

The envelope contracts are the platform's actual product for its primary
audience, and they are rigorously tested *internally* (golden matrices,
robot-contract doctrine docs) — but externally they are prose plus examples.
Every meta-agent author re-implements envelope parsing, error-code routing,
and `schema_version` handling by hand; `docs/json-schema/` covers only a few
contracts. For LLM consumers specifically, schema precision has a second
payoff the project is unusually positioned to exploit: MCP tool definitions
with tight schemas measurably improve tool-call accuracy.

### How it works

1. **Schema generation**: derive JSON Schemas from the existing serde types
   (schemars) for every `RobotResponse<T>` payload and MCP envelope — the
   types are already the single source of truth; this publishes them.
2. **The verification loop that makes it trustworthy**: a CI step validates
   every golden-matrix fixture against its generated schema. Schema drift
   that the goldens don't catch, or golden drift the schemas don't allow,
   fails the build — the schemas become *attested* contract artifacts (a
   natural `proofs/robot-contracts` extension), not aspirational docs.
3. **Thin generated clients**: typed Python/TS wrappers (subprocess + JSON
   today; IPC/HTTP transports later) with typed error-code enums and the
   documented format-precedence behavior baked in. Generated, never
   hand-maintained — the contract is the schema; the SDK is mechanical.

### Why it earns its slot, and the risks

Integration time for new meta-agents drops from days to minutes, which is the
cheapest adoption lever available; and the CI loop strengthens the contract
discipline the project already prizes, rather than adding a parallel artifact
that can rot. Risks: SDK maintenance gravity → resisted structurally by
keeping clients generated-and-thin; schema expressiveness gaps for clever
serde shapes (the custom `RobotResponse` serializer that moves `error_data`
into wire `data`) → those few hand-written schema overrides are exactly what
the golden-validation step exists to keep honest. Ranked #14 because it
multiplies the value of other surfaces rather than adding capability itself.

---

## Idea 15: `ft doctor --fix` — Receipted, Reversible Auto-Remediation

### The idea in one sentence

Give doctor hands: a remediation registry mapping known check failures to
safe, evidence-gated, receipted fixes — stale-lock cleanup after a real
liveness probe, WAL checkpoint, FTS rebuild, permission repair — with
dry-run plans, tx-engine execution, and a hard constitutional line against
anything destructive.

### The problem, with evidence

The Troubleshooting section and operator playbook are a catalog of manual
incantations for *known-benign* states: check `lsof` then remove
`watcher.lock`; run `ft db repair` for FTS drift; checkpoint the WAL after
SIGKILL. Operators and (especially) AI agents re-derive these constantly —
this repo's own memory corpus is full of stale-lock and recovery rituals.
Every remediation already has a detection (doctor) and a mechanism
(`ft db repair`, lock handling, restore-on-startup); what's missing is the
supervised connection between them.

### How it works

```bash
ft doctor --fix                 # propose: show plan per finding, ask consent
ft doctor --fix --yes           # execute allowlisted-safe class unattended (CI/agents)
ft doctor --fix --dry-run       # plans only, no execution
```

Each registry entry is a five-part contract: **precondition probe** (e.g., the
lock's recorded PID is verifiably dead — fail-closed if the probe can't run),
**dry-run plan**, **execution** (multi-step fixes run through the tx engine,
inheriting compensation for free), **post-verification** (the doctor check
must now pass), and a **receipt** in the audit trail with the cause evidence
attached. The constitutional line: the destructive class (anything deleting
data, any `rm -rf`-shaped action) is *never* executable — those entries print
the suggested command and stop, mirroring the repo's own irreversible-actions
doctrine. Every fix also emits an event, so chronic re-fixing (a fix masking
a root cause) becomes visible as a pattern rather than hidden as success.

### Why it earns its slot, and the risks

MTTR on the most common operational annoyances drops to one consented
command; unattended agents (the platform's heaviest users) self-heal
known-benign states in CI instead of wedging. The design risk — auto-fixes
papering over root causes — is addressed structurally: evidence-gated
preconditions, mandatory receipts, and re-fix-frequency visibility make this
a *diagnosis-preserving* convenience rather than a suppressor. Ranked #15 not
because it's weak but because its blast radius is deliberately the smallest
in the document — which is also exactly why it can ship first.

---

*Part II appended 2026-06-06. No code or configuration was modified in
producing this document; it remains the sole file written by this session.*
