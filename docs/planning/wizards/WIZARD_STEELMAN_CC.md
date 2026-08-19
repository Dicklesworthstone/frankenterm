# WIZARD_STEELMAN_CC.md — Arguing FOR My Opponents' Best Ideas

> Claude Code (Opus 4.8), 2026-06-06. Final round, Part 1. The assignment:
> take Codex's #1 (Swarm Steering Loop) and Gemini's #1 (Terminal-Native DOM &
> Semantic Query API) and make the strongest possible case for each — stronger
> than the original authors made it. My prior scores were 780 and 760
> respectively; both verdicts are revisited at the end of each section, and
> both move. Claims below were re-verified against the tree where load-bearing
> (the `ActionPlan` canonical content-hash and its determinism/stability tests
> in `plan.rs`; `SemanticZone` tracking in the vendored `term` crate;
> `wa.rehearsal_score` in the MCP base tool list).

---

# Steelman 1: Codex's Swarm Steering Loop

## The non-obvious insight the author undersold

Codex pitched steering as *consolidation* — one verb composing many
subsystems. That framing invited my original critique ("a fourth orchestration
vocabulary; mostly UX glue over `mission objective-plan` + `tx`"). But the
consolidation framing buries the actual discovery:

**Every safety property ft enforces lives *inside* a subsystem. The
transitions *between* subsystems are unowned — and that's where agents
actually fail.**

The envelope admits plans. Policy gates actions. Tx compensates failures.
Rehearsal scores contracts. Each is rigorous in isolation — and *nothing*
guarantees they were applied to the same artifact. Today an agent can call
`ft mission objective-plan`, receive a deny verdict, and then call
`ft tx run` with whatever contract it likes; nothing binds the thing that was
admitted to the thing that executes. The objective planner is deliberately
side-effect-free and execution is deliberately separate — a correct
separation that leaves an **identity gap**: the plan you validated and the
plan you ran are related only by the agent's good intentions. Good intentions
are precisely what the policy engine exists to not rely on.

The steering loop, read correctly, is not a convenience wrapper. It is the
**closure of the plan/execute identity gap** — and the substrate for closing
it already exists and is already tested: `ActionPlan` carries a canonical
SHA-256 content hash with determinism, stability, and timestamp-exclusion
tests in `plan.rs`, and `approval_tokens` already store an optional
`plan_hash` column. The architecture has been quietly accumulating the parts
of exactly this feature.

## The strongest implementation path in THIS codebase

Sharper than the original in one decisive way: **steering must add a receipt,
not a noun.**

1. **No `SteeringPlan` type.** `ft steer plan` is a typed pipeline over
   existing artifacts: run the mission objective planner, run the
   rehearsal scorer (`wa.rehearsal_score` already ships as an MCP base tool;
   the `proofs/rehearsal-score` golden matrix already exists), run policy
   preflight (`authorize_preview` exists on the engine) — and emit a
   `SteeringReceipt` that references the produced mission/tx contracts **by
   content hash**, with the envelope verdict, rehearsal score, and required
   approvals embedded. The only new persistent type is the receipt. This
   dissolves my own strongest objection: steer is a verb over existing nouns.
2. **Hash-bind execution.** `ft steer run --receipt <id>` refuses to start if
   the live contract's canonical hash differs from the receipt's; tx prepare
   embeds the receipt id; each commit phase re-validates against the live
   envelope (which is already the documented behavior of `ft mission run` —
   steering just makes the revalidation chain verifiable end-to-end).
   Staleness gets explicit TTL semantics: receipts expire, and an expired
   receipt is a typed refusal, not a silent re-plan.
3. **Graduated enforcement that never makes steering mandatory.** Add one
   policy capability: actions above a configurable risk threshold may be
   satisfied by *either* a one-shot approval code *or* a valid steering
   receipt covering that exact plan hash. This is the masterstroke the
   original missed: the steering receipt becomes a *first-class alternative
   to human approval for pre-validated work* — which is precisely the
   workflow operators want ("I reviewed the plan; don't make me approve each
   step") and which the `approval_tokens.plan_hash` column appears to have
   been waiting for.
4. **Fixtures before parity.** Golden scenarios exactly as Codex listed
   (clean-ready, dirty-overlap, RCH-blocked, approval-required,
   capacity-red…), reusing `mission_objective_plan`'s existing status
   taxonomy (Actionable / Blocked / DirtyOverlap / RchSubstrateBlocked / …)
   rather than inventing a parallel one. Robot/MCP (`wa.steer_*`) only after
   CLI goldens stabilize — `mcp_missions.rs` already handles mission
   lifecycle transitions, so the MCP increment is genuinely small.

## Second-order benefits the author did not articulate

- **It solves the cold-start problem of two of Codex's own other ideas.**
  The Remediation Loop (#6) and Rehearsal-priors (#8) both starve without a
  structured corpus of "objective → plan → outcome" records. Steering
  receipts *are* that corpus, generated as a side effect of normal
  operation. Steering doesn't just compose existing subsystems; it feeds the
  learning systems that come after it.
- **It is the mission-level provenance record Gemini's SBOM idea wanted and
  couldn't have.** ft can't sign per-file edits (it never sees them), but a
  steering receipt chain — envelope verdict → plan hash → policy decisions →
  tx receipts → outcome — is an end-to-end, attestable answer to "why did
  the swarm do X," at the intent level rather than the keystroke level. The
  feasible 80% of agentic provenance falls out of steering for free.
- **Session handoff.** In a multi-agent repo (this one), a successor agent
  currently reconstructs context from beads comments and scrollback
  archaeology. Reading the predecessor's steering receipt — what was
  planned, what was admitted, what completed, what compensated — is a
  structured handoff surface that no current artifact provides.
- **Pit-of-success alignment.** Today the lowest-friction path for an agent
  is raw `robot send`, which bypasses envelope and rehearsal entirely.
  Steering makes the safest path also the shortest path — the only durable
  way to change agent behavior at scale, because agents (like water) follow
  the gradient.

## Pre-emptive defense against the two most likely objections

**Objection 1 — "It duplicates `mission objective-plan` and adds a fourth
vocabulary" (my own original critique).** Answered structurally: in the
receipts-over-existing-nouns formulation there is no new contract type, no
new lifecycle state machine, and no semantic drift surface — the planner's
artifact, the tx contract, and the lifecycle states are the *same objects*,
now bound by hash across phase transitions. What's genuinely new is the
binding, and the binding is the point. The vocabulary count goes from three
to three-plus-one-receipt, not four.

**Objection 2 — "Agents will bypass it via low-level surfaces anyway."**
First, by design: low-level surfaces must survive for low-level needs, and a
control plane that *forces* its abstraction is one that gets forked around.
Second, the graduated-enforcement design inverts the incentive — when a
steering receipt waives per-step approval friction for pre-validated plans,
bypassing steering means *more* friction, not less. You don't mandate the
safe path; you subsidize it.

## Honest residual concerns I could not argue away

- **The supervision phase is new machinery, not composition.** "Live
  monitor → compensate or complete" requires a long-running supervisor making
  judgment calls (is this step stuck or slow?) that no existing subsystem
  makes today. The wrapper framing genuinely hides this 20%, and it is the
  hard 20%.
- **Hash binding is only as strong as contract stability across the
  plan→run window.** Pane-id churn, dirty-tree drift, and envelope state
  changes between plan and run will force a re-plan rate that, if high,
  erodes the receipt's practical value into ceremony. TTLs bound the damage
  but don't eliminate the annoyance.
- **One-shot receipts vs. iterative agents.** Real agent work is iterative;
  a strict plan-hash-or-nothing gate may fit batch missions better than
  exploratory sessions. The risk-threshold scoping mitigates but the
  impedance mismatch is real.

## Verdict revision

**Yes — steelmanning changed my mind, and I say so explicitly.** I scored
this 780 on the consolidation reading. On the identity-gap reading — receipts
over existing nouns, hash-bound execution, receipt-as-approval-alternative —
it closes a real, currently-unowned safety hole using parts the codebase has
already built and tested. Revised assessment: **~845**, conditional on the
no-new-noun implementation. My original critique survives only as an
implementation constraint, not as an objection.

---

# Steelman 2: Gemini's Terminal-Native DOM & Semantic Query API

## The non-obvious insight the author undersold

Gemini pitched this as better parsing for agents: replace brittle
`get-text`-and-regex with `get_last_command()` / `get_exit_code()` /
`find_prompts()`. Good — but it undersells the idea twice over.

**First: the DOM is not primarily a feature. It is the missing *foundation*
under half of the other 44 ideas in this exchange.** Walk the lists:
my Verified-Submit needs composer-region detection — that's a DOM query.
Gemini's own sludge compaction needs block segmentation — DOM regions.
Timeline forensics wants command-boundary alignment — DOM zones. Exit-code-
aware workflow triggers, per-command economics attribution, command-scoped
search — all DOM consumers. Three models independently generated a dozen
features that each privately re-derive "where does output begin and end";
the DOM is that answer computed once, owned by the layer that actually knows.

**Second — and this is the insight nobody on any side articulated: semantic
zones fix a *detection-correctness* bug class, not just an ergonomic one.**
The pattern engine today anchors on raw text. An agent *talking about* an
error — quoting "error: compilation failed" in its chat output while
summarizing — is indistinguishable from a build *producing* that error. With
zone-scoped rules ("match only inside command-output zones"), the
quoted-error false-positive class dies structurally rather than being
whack-a-moled with cleverer regexes. For a platform whose core loop is
detect→react, raising detection precision at the substrate level upgrades
every workflow, every notification, every auto-handler simultaneously.

And the strategic kicker: **this is the first feature that converts ft's
deepest sunk cost — vendoring an entire terminal emulator — into an API
moat.** The "why a fork" section justifies emulator ownership via runtime
control and proof surfaces. The DOM is stronger justification than either:
a tmux wrapper *cannot ever* ship this, because it doesn't own the grid.
The fork becomes the feature.

## The strongest implementation path in THIS codebase

More specific than the original, and cheaper than it sounds, because the
hard part is already vendored:

1. **Phase 1 — surface what the emulator already knows.** The vendored
   `term` crate already tracks `SemanticZone`s (OSC 133 prompt/input/output
   marks — verified in `frankenterm/term/src/lib.rs`). Expose zone queries
   through `MuxInterface` → new robot family: `ft robot dom zones <pane>`,
   `dom last-command <pane>`, `dom output-of <pane> --command-index -1`,
   with exit codes where the shell emits OSC 133;D. Contract home:
   `robot_api_contracts.rs` (real, 60KB), golden matrices per the
   profile-family doctrine.
2. **Phase 2 — own the adoption problem at the spawn path.** This is the
   answer to the integration-friction objection: ft *controls pane
   creation*. Profiles and fleet templates inject shell integration
   (OSC 133 + OSC 7 emitters for bash/zsh/fish — the scripts WezTerm
   upstream already maintains can be vendored and shipped via `ft setup`)
   automatically into ft-spawned panes. For ft-managed fleets, semantic
   coverage approaches 100% with zero user action; `ft doctor` reports
   per-pane availability for everything else.
3. **Phase 3 — persist zones into capture.** Stamp zone boundaries into
   segment metadata at ingest so *historical* queries become structured:
   `ft search "FAILED" --zone output --since "6 hours ago"` — "find every
   failed command across the fleet" as a query instead of regex
   archaeology. This is the phase that compounds: detection scoping,
   timeline alignment, and economics attribution all read the same stamps.
4. **Phase 4 — TUI panes get the *grid* half of the DOM.** Where OSC zones
   are absent (alt-screen agent TUIs), the emulator still owns the grid:
   region queries (cursor line, bottom-N rows, screen-diff-since-seq) are
   the DOM primitives that Verified-Submit's composer detection needs.
   Typed degradation everywhere: `semantic: unavailable (alt_screen)` —
   never heuristic guessing dressed as structure, per house style.

## Second-order benefits the author did not articulate

- **Detection precision** (the quoted-error class, above) — the largest
  unclaimed benefit, upstream of every workflow.
- **Verified-Submit synergy:** my own #1 idea becomes cheaper and more
  robust built on DOM region queries than on bespoke pattern packs alone;
  the two proposals share their hardest component.
- **Exit-code-native workflows:** triggering on `(command_class,
  exit_code)` pairs instead of output regexes is structurally more reliable
  for the build/test class, and reduces rule-pack drift surface.
- **Per-command economics:** the rate-limit/economics ledger can attribute
  activity to commands rather than panes, turning "pane 7 is expensive"
  into "the test suite is expensive."
- **Search-ranking quality:** zone-typed segments let FTS boost
  command-output over prompt noise and TUI repaints — better recall for the
  surface operators use most.
- **CI substrate:** deterministic `last-command`/`exit-code` interrogation
  makes ft's own 276 e2e shell scripts (and everyone else's) less flaky than
  text-scraping assertions.

## Pre-emptive defense against the two most likely objections

**Objection 1 — "Agent panes are TUIs; zones are meaningless on the flagship
workload" (my own original critique).** Three answers. (a) The panes where
agents *most need* structure are the non-TUI ones — build panes, test panes,
worker shells — which is where mis-parsing burns hours today; the criticism
shrinks the surface to exactly where the value already was. (b) TUI panes
are served by the grid-query half of the DOM (Phase 4), which is what
composer detection actually needs — different primitive, same API family.
(c) Typed unavailability means no false promises on uncovered panes; the
feature degrades to today's behavior, never below it.

**Objection 2 — "Shell-integration dependency makes it a config burden that
will silently not work."** ft is uniquely positioned to dissolve this: it
spawns the panes. Integration injected at spawn via profiles makes coverage
an emergent property of using ft's own fleet machinery, not a user chore —
an option no standalone terminal has, because no standalone terminal owns a
profile-driven spawn path. And precedent says the ask is normal: every
serious modern terminal (WezTerm, Kitty, iTerm2) ships shell integration
for far smaller payoffs than a semantic API.

## Honest residual concerns I could not argue away

- **Zone fidelity is hostage to the shell-prompt ecosystem.** Starship,
  powerlevel10k, direnv hooks, and exotic PROMPT_COMMAND stacks emit
  OSC 133 with varying correctness; "the DOM lied about where output
  begins" is a subtle, trust-eroding bug class that fixtures can chase but
  never fully fence.
- **Remote panes need integration at the remote end** (SSH domains), where
  ft's spawn-path advantage attenuates.
- **No retroactive backfill:** zones exist only from capture-time forward;
  the historical corpus stays unstructured.
- **The name overpromises.** It's semantic zones plus grid queries — a
  superb pane-semantics API, not a DOM tree. Shipping it under an honest
  name ("semantic pane API") matters in a project whose culture audits
  docstring overpromise as a defect class.

## Verdict revision

**Yes — moved here too.** I scored this 760 with the TUI-pane objection
doing the suppressing. The steelman dissolves most of that objection (grid
queries for TUI panes; spawn-path-controlled integration for the rest) and
surfaces the two benefits I'd missed: detection-precision upgrade at the
substrate level, and the DOM as shared foundation under a dozen other
proposals including my own #1. Revised assessment: **~820**, and I'd
sequence its Phase 1 *before* Verified-Submit's pattern-pack work, because
the latter should be built on the former.

---

*Net effect of the exercise: both opponents' #1 picks are better than I
originally credited — Codex's by ~65 points on a reading its author didn't
quite make explicit, Gemini's by ~60 on implementation grounds its author
didn't reach. The adversarial rounds sharpened my critiques; the steelman
round exposed which of those critiques were load-bearing and which were
habit.*

*This file and WIZARD_BLINDSPOTS_CC.md are the only files written in this
step; no code or configuration was modified.*
