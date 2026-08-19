# WIZARD_SCORES_CC_ON_COD.md — Claude Code's Evaluation of the Codex Idea List

> Evaluator: Claude Code (Opus 4.8), 2026-06-06. Basis: full read of
> WIZARD_IDEAS_COD.md against my own six-stream code investigation of this repo
> (runtime/Cx, Robot/MCP, policy/mission/tx, envelope/attestation, dormant-
> subsystem caller-graph audit), plus targeted verification greps for claims I
> doubted. Scores are 0–1000 and deliberately spread; my bands: **800+** build
> it, **650–799** strong with real caveats, **500–649** plausible but the
> caveats bite, **350–499** weak utility-to-complexity ratio, **<350**
> architectural misfit.
>
> Overall read of this list: Codex clearly knows this repo's actual operating
> reality — proof lanes, fail-closed doctrine, attestation discipline, the
> swarm-tick fallback. The list's systematic weakness is the mirror image of
> that strength: several ideas are *dev-infra meta-tooling for working on ft*
> rather than product capability for ft's users, and several others partially
> duplicate surfaces that already ship (`wa.attention`, `wa.rehearsal_score`,
> the in-flight ft-dezx8.3 agent-mail outbox). Mean score ≈ 668; range 450–830.

| # | Idea | Score |
|---|---|---|
| 1 | Swarm Steering Loop | 780 |
| 2 | Deferred Proof Conveyor | 820 |
| 3 | Unified Attention and Intervention Console | 830 |
| 4 | Adaptive Governor Mesh | 560 |
| 5 | Policy-Safe Extension Workbench | 680 |
| 6 | Swarm Learning Remediation Loop | 600 |
| 7 | Robot/MCP Contract Doctor | 760 |
| 8 | Mission Rehearsal Scorer | 620 |
| 9 | RCH Admission Explainer | 700 |
| 10 | Agent Mail Outage Spool | 610 |
| 11 | Attestation Graph Explorer | 590 |
| 12 | Operator First-Run Guided Tour | 450 |
| 13 | Pane Ownership Firewall | 660 |
| 14 | Incident Bundle Timeline Explorer | 720 |
| 15 | Golden Replay Studio | 690 |

---

## 1. Swarm Steering Loop — **780**

This is the right product instinct: ft's parts (objective planner, envelope,
tx, policy, rehearsal) are individually excellent and collectively unmarketed,
and the staged plan (read-only `steer plan` first, thin executor delegating to
Tx, fixtures before Robot/MCP parity) respects every house rule. The "agents
fail by improvising around invisible constraints" diagnosis is exactly correct
and matches this repo's own memory corpus. **Strongest argument against:** it
substantially re-describes `ft mission objective-plan` — which already does
objective → envelope verdict → deterministic plan artifact → audit trail — and
then adds a *fourth* orchestration vocabulary (mission, tx, workflow, now
steer) with a new `SteeringPlan` type, a new robot family, and new MCP tools,
each of which the contract doctrine obligates to golden matrices, redaction
tests, and TOON parity forever. The honest version of this idea is "finish and
productize the mission surface under one verb," and the proposal flirts with
building a wrapper layer instead; semantic drift between steer-plan and
mission-plan would be a permanent tax. Scored high because even the wrapper
reading is valuable; not higher because the novelty-to-surface-area ratio is
worse than it looks.

## 2. Deferred Proof Conveyor — **820**

The most grounded idea on the list. Proof-lane unavailability is *the*
documented daily bottleneck of this repo (whole days of "rch lane down" in the
session history; exit-143 fail-closed doctrine; the ft-zbnz4 deferred-proof
static-verifier family was invented precisely to cope), and a durable
`ProofIntent` queue with quality classification (remote-reached vs local
fallback, stale source hash, package vs workspace scope) converts a chronic
ambient failure into a tractable state machine. The proof-quality taxonomy is
the genuinely smart part — "retry cargo later" would have been the naive
version. **Strongest argument against:** this is meta-tooling for *developing
ft*, not capability for ft's users — every hour spent here improves one
specific, arguably broken build infrastructure (8 Contabo workers) and ships
nothing to anyone who installs ft; if RCH were fixed or replaced, the conveyor
mostly evaporates. There is also real replay risk in re-running stale commands
against moved trees, though the source-hash gating addresses it. Within its
deliberately inward-facing scope, this is close to optimal.

## 3. Unified Attention and Intervention Console — **830**

Top score on the list. It names the correct single question ("what needs
attention, what's safe to do, who owns it"), the read-only-aggregation-first /
policy-gated-interventions-later phasing is exactly right, and it activates
two dormant subsystems (intervention console, fleet dashboard) whose decision
logic is finished and tested — the cheapest capability acquisition available
in this codebase. The insistence that it be "an action queue with typed items,
not a decorative dashboard" is the design sentence that saves it.
**Strongest argument against:** more of this exists than the writeup admits —
`wa.attention` already ships as an MCP base tool and the attention-router
contract (`ft.attention_router.v1`) is already specified in the runbook, so
the genuinely new work is narrower than presented: ranking quality,
intervention wiring, and consolidation. There's also the classic risk that a
ranked attention feed agents *trust* becomes a single point of judgment
failure; deterministic golden scenarios (which it proposes) are necessary but
not sufficient for that. Still: highest utility-to-risk ratio here.

## 4. Adaptive Governor Mesh — **560**

The diagnosis (pressure subsystems are isolated; dormant governors should
feed admission) is right, but the prescription — one `BudgetVerdict`
abstraction unifying envelope, capacity governor, connector governor, circuit
breakers, BOCPD, fleet memory, storage, network, and kill switches — is the
riskiest architecture move proposed by anyone in either list. **Strongest
argument against:** these systems have deliberately *different* semantics
(the envelope is fail-closed admission *planning*; backpressure is a fast-path
reflex; the connector governor is per-connector quota; BOCPD is a statistical
anomaly posterior, not a pressure signal), and flattening them into one budget
model couples everything to everything — one miscalibrated adapter degrades
the whole fleet, and treating change-point anomalies as admission inputs is a
recipe for flapping that no hysteresis rule fully cures. The fleet memory
controller already does worst-of synthesis *within* its domain; wiring each
dormant governor at its natural consult point gets ~80% of this value with
~20% of the coupling. The advisory-first phasing keeps this from scoring
lower, but the end-state design itself is the problem, not just the rollout.

## 5. Policy-Safe Extension Workbench — **680**

A disciplined version of WASM activation: tiny non-mutating capability set
first, replay-gated enablement, mutations only as Mission/Tx candidates —
that last constraint is genuinely elegant and better than my own framing of
the same idea. The substrate is real (frozen capability types, fuel/memory
budgets in `extensions.rs`). **Strongest argument against:** the
cost-to-demand ratio. WASM hosting, capability enforcement, signed bundles,
and a dev loop is a sub-project measured in months, undertaken on speculative
demand (no queue of users blocked on extensibility), and the proposal
hard-depends on the Golden Replay Studio (#15) existing first — an internal
dependency chain where slipping one slips both. Right shape, expensive bet,
honest #5 placement by its own author.

## 6. Swarm Learning Remediation Loop — **600**

The design instincts are commendable — typed features over vector similarity,
advisory-only first, evidence-honest "proven outcome vs partial analogy"
labeling — and the vision (every resolved incident improves the next one) is
the correct long-term destination. **Strongest argument against:** the
foundation isn't there. The in-repo cass integration is stub-level (the
`HandleSwarmLearningIndex` handler fires but the indexing backend doesn't
deliver), the structured corpus of receipted resolutions it wants to retrieve
from doesn't exist yet, and cold-start means the feature ships useless and
accretes value only after months of disciplined receipt capture. Worse, its
failure mode is actively harmful: a confident, stale remediation suggestion
misleads agents in exactly the high-stress moments it targets. Right idea,
two layers too early in the dependency stack.

## 7. Robot/MCP Contract Doctor — **760**

Directly protective of the project's core differentiator, and the checklist
(envelope shape, Robot/MCP parity, policy gating on mutation, redaction on
reads, TOON/JSON semantic equivalence, error-code stability) is precisely the
set of properties agent consumers are brittle against. The
inventory-from-typed-registries approach is feasible because the registries
genuinely exist. **Strongest argument against:** marginal detection power —
much of this is already enforced piecewise (golden envelope matrices, the
read-path redaction matrix, the policy-denial wiring matrix, schema_version
tests), so the doctor is substantially an *aggregation* of existing checks,
and its genuinely new muscle (live no-mock subprocess checks) is the part
most likely to be slow and flaky in CI. Score reflects high value, low risk,
and moderate — not transformative — novelty. The attestation-artifact endgame
is the right touch.

## 8. Mission Rehearsal Scorer — **620**

Sensible, and the staging (deterministic struct-level rules first, priors
later) is right. But the writeup misses that this surface *partially ships*:
`wa.rehearsal_score` is already an MCP base tool and the
`proofs/rehearsal-score` attestation slot with its golden matrix is already
described in the README — so a chunk of this idea is extending an existing
feature it doesn't acknowledge. **Strongest argument against:** beyond the
duplication, a numeric "readiness score" is exactly the artifact this repo's
own audit culture warns about — the rubber-stamp class with seventeen closed
findings — and a score computed before the predictive inputs exist (the
historical priors depend on idea #6's nonexistent corpus) is a vacuous gate
wearing a quantitative costume. The deterministic checks (missing
compensation, stale panes, cycles) are real but small. Useful increment,
oversold as new.

## 9. RCH Admission Explainer — **700**

High pragmatism, tight scope, real time saved: preflight classification of
proof commands against documented refusal patterns kills a class of wasted
session endings, and the typed `ProofCommandAnalysis` with envelope-vocabulary
reason codes is the right contract. As the front door to idea #2 it earns its
keep. **Strongest argument against:** same inward-facing ceiling as the
conveyor (zero product value to external users), plus an intrinsic freshness
problem — RCH health changes minute-to-minute, so "admitted" preflights will
still fail at run time, and an explainer that's right 85% of the time risks
becoming false assurance that agents stop double-checking. It must be framed
as "predicted verdict with timestamp," never as a promise, or it recreates
the local-proof-counts-as-proof confusion it exists to prevent.

## 10. Agent Mail Outage Spool — **610**

Correct problem (chronic, documented outages), correct constraint discipline
(never touch the service; client-side spool only), mature pattern (durable
outbox with replay receipts and dedup). **Strongest argument against:** this
is already being built — the repo's last eight commits include
"agent-mail outbox: add exact-once replay verifier (ft-dezx8.3)" and a
secondary-recipient/attachment-blocker follow-up, which a careful pass over
`git log` would have caught; as submitted, the idea substantially describes
in-flight work rather than proposing new work. Beyond that, the spool only
helps if *other* agents adopt the convention of reading it during outages —
a coordination-adoption problem the proposal doesn't address — and the
half-delivered-then-spooled split-brain case is trickier than the dedup
bullet implies. Sound engineering, weak novelty.

## 11. Attestation Graph Explorer — **590**

Real ergonomic gap (the manifest/checklist machinery is expert-facing), and
the query set — stale-vs-HEAD, retraction blast radius, release-blocking
claims — is well-chosen; `ft attestation why <claim>` is a genuinely nice
verb. **Strongest argument against:** the audience is a handful of release
managers and repo agents, the existing `verify`/`show`/`retractions` +
checklist already cover the load-bearing paths, and the marquee queries
(retraction impact analysis) are needed perhaps a few times a year. This is
quality-of-life tooling for the project's most expert users, priced at a
moderate graph-model implementation; the utility-per-effort is decent but
the ceiling is inherently low. Fine as a background task, wrong as a
priority.

## 12. Operator First-Run Guided Tour — **450**

Weakest of the fifteen. The intent (approachability) is fair, but the
proposal stacks a *fourth* onboarding surface onto `ft demo` (manifest-backed
scenario validation with retained artifacts — already side-effect-free,
already machine-readable), `ft setup`, `ft doctor`, and the README's
10-minute tour, which collectively cover nearly everything listed.
**Strongest argument against:** fragmenting onboarding across four
overlapping surfaces makes first-run *more* confusing, not less — the actual
fix is consolidation (teach `ft demo` + `ft doctor` better, or merge them),
and a `QuickstartReceipt` with schema versioning is process ceremony for a
flow whose users, by definition, haven't committed to the tool yet. The one
novel kernel — a capability-availability receipt agents can read to learn
what this build supports — is good, and is essentially `ft doctor --json`
plus feature flags, which exists.

## 13. Pane Ownership Firewall — **660**

Addresses a real swarm hazard (cross-agent stomps are not edge cases — the
repo's own doctrine note about "changes created by a dozen other agents"
proves it), and `PaneAccessVerdict` composing reservations + correlated
identity + policy is a legitimate composition layer over shipped parts, with
denial messages that teach ("who owns it, how to request takeover").
**Strongest argument against:** the reservation ground truth is split between
two systems — ft's own `pane_reservations` table and Agent Mail's *advisory*
file reservations, which live in an external, chronically flaky service — so
the firewall either enforces locks it doesn't own (consistency and TTL
nightmares when an agent dies holding one) or enforces only its own table,
which the actual swarm doesn't primarily use. Stale-owner deadlock is where
this design goes to suffer, and the proposal waves at it rather than solving
it. Worth building after the reservation story is unified; risky before.

## 14. Incident Bundle Timeline Explorer — **720**

Cheap, honest, and useful: incident bundles already collect the sources, the
normalized `TimelineEvent` over existing event/audit/receipt streams is pure
read-path composition, and offline-first scoping keeps risk near zero. The
explicit-confidence rule for inferred correlations shows the right
epistemics. **Strongest argument against:** "causality hints" from temporal
adjacency will be wrong often enough to matter — same-window correlation in a
200-pane fleet is mostly coincidence — and a timeline that *looks* causal
anchors investigators on the wrong cascade faster than raw logs would; the
confidence annotations mitigate but don't eliminate narrative bias. Also
modest novelty: it's a rendering/query layer, and its value depends on
correlation IDs being populated more consistently than they are today. Solid
mid-list pick.

## 15. Golden Replay Studio — **690**

The "deterministic lab where new behavior proves itself" framing is correct,
the replay crate (~25k LOC) is real and underused, and the curated starter
corpus list (auth prompt, usage limit, secret-bearing output, tx failure) is
well-chosen. As the enabling substrate for #5 and #8 it's load-bearing.
**Strongest argument against:** the proposal's hardest component is buried in
one bullet — "record from live data with strict redaction and minimization"
is a privacy-critical capture/sanitize pipeline that's harder than all the
replay runners combined, and getting it wrong leaks real secrets into shared
fixtures. The runner matrix (patterns, workflows, search, policy, envelopes,
extensions) is also scope-creep-shaped: each runner is its own determinism
fight. And the permanent cost is fixture maintenance — every legitimate rule
pack change ripples through goldens, requiring a bless workflow the proposal
doesn't specify. Strong idea that needs a much more brutal phase-1 cut.

---

## Summary judgment

The list's top tier (#3, #2, #1) is genuinely strong and could be adopted
nearly as-is; #3 and #2 in particular convert documented, daily pain into
disciplined machinery. The persistent blind spots: (a) under-checking what
already ships or is in flight (#3's `wa.attention`, #8's rehearsal-score
slot, #10's outbox commits), (b) a center of gravity that drifts toward
tooling for ft's own development rather than ft's product (#2, #9, #10, #11),
and (c) one over-unified architecture bet (#4) that mistakes "these systems
should talk" for "these systems should be one system."

*This file and WIZARD_SCORES_CC_ON_GMI.md are the only files written by this
evaluation; no code or configuration was modified.*
