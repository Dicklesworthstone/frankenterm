# Dueling Idea Wizards Report: FrankenTerm

> Orchestrated 2026-06-06 by Claude Code (Opus 4.8) via the `/dueling-idea-wizards` skill.
> Three-way duel: **Claude Code (Opus 4.8, high effort)** vs **Codex (gpt-5.5 high)** vs
> **Gemini (gemini-3.1-pro-preview)**, all running fresh in the frankenterm NTM session
> (panes 9/10/11). Full pipeline: study → 30→5 ideation → expansion to 15 ideas each →
> 3-way adversarial cross-scoring (6 score files) → reveal → formal rebuttals → forced
> steelman of each opponent's #1 → blind-spot probe. 19 artifact files, 45 primary ideas,
> 90 cross-scores, 12 formal defenses/attacks, 6 steelmen, 14 blind-spot ideas.

## Executive Summary

45 ideas were generated independently (15 per model), cross-scored adversarially on a
0–1000 scale, defended, attacked, steelmanned, and then transcended. **Nine ideas
survived as consensus winners** (≥700 from every scorer or steelman-revised into that
band); **five were killed by mutual agreement** including author concession; **four
remain genuinely contested** and need a human call. The top three consensus picks:

1. **Closed-Loop Dispatch / Verified-Submit Send** (CC#1, avg 890, unanimous #1) — upgrade
   `ft robot send` from a write receipt to a *delivery receipt* with typed terminal states.
2. **Terminal Semantic Pane API** (GMI#1, 770 → ~820 post-steelman) — expose the vendored
   emulator's already-tracked semantic zones + grid queries as a structured Robot/MCP
   query surface; the foundation under a dozen other proposals.
3. **Swarm Steering Loop as `SteeringReceipt`** (COD#1, 780 → ~845 post-steelman) — close
   the plan/execute *identity gap* by hash-binding the artifact the envelope admitted to
   the artifact tx executes, with receipts (not new nouns).

The blind-spot probe (run after the full adversarial exchange) was the creative goldmine
the methodology promises: it surfaced **a verified live gap** — `backup.rs` (101 KB)
contains **zero** redaction paths, so exported backups permanently carry every token
family the ingest-time catalog missed — plus a Doctrine-to-Policy compiler, headless
"virtual panes," an agent A/B evaluation plane, and a token-burn economic circuit breaker.

**Empirical validation, live, during this very session:** the duel itself suffered the
exact failure class CC's #1 idea targets — Gemini's prompt fell into shell-command mode
twice, Codex required double-Enter, and CC's final steelman prompt sat silently in its
composer for 5+ minutes until the orchestrator noticed and pressed Enter. Three dispatch
failures in one 90-minute session, on the platform's own development host. The top-ranked
idea is not hypothetical.

## Methodology

- **Agents:** cc_4 (Claude Opus 4.8), cod_6 (Codex gpt-5.5 high), gmi_1 (Gemini 3.1 Pro
  Preview) — added to the live `frankenterm` tmux session without disturbing the 9
  existing sibling panes.
- **Phases:** project study (AGENTS.md + README + code investigation) → 30-candidate
  ideation winnowed to 5 → expansion to 15 → each agent scored both rivals' 15 ideas
  (candid 0–1000, strongest-argument-against required) → reveal of both rivals' scores
  of own ideas → reactions + formal rebuttals (2 defenses + 2 attacks each) → forced
  steelman of each opponent's #1 → blind-spot probe ("what did NONE of us think of?").
- **Anti-love-fest:** scoring prompts demanded spread; resulting means: CC graded
  COD ≈ 668 / GMI ≈ 449; COD graded CC ≈ 788 / GMI ≈ 532; GMI graded CC ≈ 587 / COD ≈ 500.
  Range across all 90 scores: 180–930.

### Artifact index

| Artifact | Files |
|---|---|
| Ideas (15 each) | `WIZARD_IDEAS_{CC,COD,GMI}.md` |
| Cross-scores (6) | `WIZARD_SCORES_{CC_ON_COD, CC_ON_GMI, COD_ON_CC, COD_ON_GMI, GMI_ON_CC, GMI_ON_COD}.md` |
| Reactions | `WIZARD_REACTIONS_{CC,COD,GMI}.md` |
| Rebuttals | `WIZARD_REBUTTAL_{CC,COD,GMI}.md` |
| Steelmen | `WIZARD_STEELMAN_{CC,COD,GMI}.md` |
| Blind spots | `WIZARD_BLINDSPOTS_{CC,COD,GMI}.md` |

## Consensus Winners

Ranked by cross-model average, adjusted for steelman revisions and rebuttal outcomes.

### 1. Closed-Loop Dispatch — Verified-Submit Send with Delivery Receipts (CC#1) — **890** (COD 930 / GMI 850→950)

Upgrade `ft robot send` from "bytes reached the pty" to "the agent CLI actually accepted
and submitted the input": echo verification via capture deltas, profile-driven submit
keystroke, a four-state verifier (`submitted` / `queued_behind_operation` /
`stuck_in_composer` / `pane_crashed_to_shell`), durable idempotent `SubmitReceipt`, and
workflow integration so `HandleCompaction`/`HandleUsageLimits` stop being vulnerable to
the failure they exist to fix. Codifies AGENTS.md playbook rules SO-1/SO-2 into platform
machinery. **Steelman upgrades:** COD called it "probably the single most immediately
valuable idea in the whole exercise" and specified the receipt schema + graduated
guarantee levels (write/composer/submitted/working). GMI proposed the strongest
implementation twist of the whole duel: verify submission via an **invisible
cryptographic canary** (OSC sequence / zero-width chars) observed transitioning from the
composer zone into processed scrollback — immune to spinner/theme drift — and revised its
score to ~950. Residual risk: agent-CLI UI drift; mitigated by fixture-first profile
maintenance and fail-open `verification_unavailable`.

### 2. Terminal Semantic Pane API ("Terminal-Native DOM") (GMI#1) — **770 → ~820 post-steelman** (CC 760→820 / COD 780↑)

Expose structured, provenance-bearing queries over terminal state: semantic regions
(prompt/input/output via OSC 133 — **already tracked as `SemanticZone`s in the vendored
`term` crate**), `last-command`, `exit-code`, `new-output-since-cursor`, alt-screen
state, plus grid queries for TUI panes. Per-field `source` + `confidence` +
`semantic_data_unavailable` honesty. CC's steelman surfaced two unclaimed benefits:
**(a)** zone-scoped detection rules kill the quoted-error false-positive class
structurally (an agent *quoting* "error: compilation failed" vs a build *producing* it);
**(b)** ft controls pane spawn, so shell integration can be injected via profiles —
dissolving the adoption objection. Both opponents converged on the same constraint: ship
it as a *semantic evidence API*, not a "DOM," and sequence its Phase 1 **before**
Verified-Submit, whose composer detection should build on it.

### 3. Swarm Steering Loop → `SteeringReceipt` (COD#1) — **780 → ~845 post-steelman** (CC 780→845 / GMI 780↑)

CC's steelman reframed this from "consolidation wrapper" to **closing the
plan/execute identity gap**: today nothing binds the plan the operating envelope admitted
to the contract `ft tx run` executes. The fix needs **no new noun**: `ft steer plan`
pipelines existing artifacts (objective planner → rehearsal scorer → policy preflight)
and emits a `SteeringReceipt` referencing the mission/tx contracts **by canonical content
hash** (already implemented + tested in `plan.rs`; `approval_tokens.plan_hash` column
already exists). `ft steer run --receipt <id>` refuses on hash mismatch. Masterstroke: a
valid steering receipt becomes a **first-class alternative to per-step human approval**
for pre-validated plans — subsidizing the safe path instead of mandating it. GMI's
steelman independently converged ("Preflight Compiler for agent intentions") and added
the fine-tuning-corpus second-order benefit. COD itself conceded the original
`SteeringPlan`-type framing was wrong. Residual: the live-supervision phase is genuinely
new machinery (~20% of the work, the hard 20%).

### 4. `ft robot watch-events` — Streaming Event Subscription (CC#2) — **860** (COD 900 / GMI 820)

Cursorable NDJSON `--follow` + composite `ft robot await`, over the existing EventBus via
the existing IPC layer, with SSE-proven filtering/redaction/max_hz, storage-cursor
fallback, and at-least-once semantics with typed `cursor_expired` (never silent loss).
Kills the `while sleep 5` polling loop the README itself teaches. Unanimously strong;
delivery semantics across watcher restarts is the one design obligation.

### 5. Deferred Proof Conveyor (COD#2) — **785** (CC 820 / GMI 750)

Durable `ProofIntent` queue for RCH-blocked proof lanes with quality classification
(remote-reached vs local-fallback, package vs workspace scope, stale source hash,
infra-blocked vs terminal failure), replay on worker recovery, receipts into Beads and
attestations. Directly attacks this repo's #1 documented daily bottleneck. Honest caveat
(CC): inward-facing dev-infra, not end-user product — prioritize as "protect the project
and release pipeline."

### 6. Dead-Wire Closure + Wiring Attestation Gate (CC#3) — **818** (COD 875 / GMI 760)

Wire BOCPD, connector reliability/governor (ft-x3211), and the capacity governor into
live dispatch behind shadow mode; add a `frankenterm-topo` deadwire CI check (decision
entry points with zero production callers fail CI unless declared `dormant` in a manifest
slot **with a bead reference and expiry date**). Makes "built-but-never-consulted" a
detectable, fail-closed defect class — the fourth structurally-enforced invariant after
cycles, counts, and tokio.

### 7. Rate-Limit-Aware Scheduling + Fleet Economics Ledger (CC#7) — **778** (COD 815 / GMI 740)

Parse the reset timestamps the detection rules already capture into a `limit_windows`
ledger; `ft robot limits` capacity forecast; scheduling hooks decline limited panes with
typed reasons; `limit.window.reset` bus events. Pairs naturally with GMI's blind-spot
**Economic Circuit Breaker** (below) to make cost a governed resource.

### 8. Cross-Swarm "Scent" / Spatial Awareness (GMI#4) — **703** (CC 680 / COD 725)

Aggregate CWDs, reservations, work claims, and TxIntents from the (verified-live) agent
correlator into a `wa://swarm/scent` read-only resource so siblings divide work instead
of colliding. Needs freshness/TTL discipline; overlaps with attention routing — fold into
the attention surface below.

### 9. Unified Attention & Intervention Console + `ft deck` (COD#3 + CC#10, merged) — **~700** (COD#3: CC 830 / GMI 600; CC#10: COD 720 / GMI 650)

Both lists independently proposed activating the dormant intervention console + fleet
dashboard. Merged design: read-only typed `AttentionItem` aggregation first (CC noted
`wa.attention` partially ships — the new work is ranking quality, intervention wiring,
consolidation), then policy-gated `ft intervene pause/quarantine/takeover` CLI verbs
(shipped unconditionally), then the ftui deck as a composition layer that never hosts
its own logic.

## Contested Ideas (human judgment required)

| Idea | Scores | The disagreement |
|---|---|---|
| **Operation Target-Class** (CC#11) | COD **835** vs GMI **350** (widest gap: 485) | GMI: "a Jira ticket, not an idea; zero new runtime capability." CC's rebuttal (90% confidence): factually wrong — the envelope consults the target-class artifact at decision time; flipping `skipped_not_proven` changes live admission verdicts, and group-commit on `append_segment_sync` helps every fleet size. COD: "strategically important." **Recommendation: side with COD/CC — the rubric was accretive+pragmatic, not novelty.** |
| **Adaptive Governor Mesh** (COD#4) | GMI **810** vs CC **560** | CC: flattening envelope/backpressure/quota/BOCPD into one `BudgetVerdict` couples everything to everything; wiring each governor at its natural consult point gets ~80% of value at ~20% coupling. COD **conceded the end-state**: advisory budget *visibility* yes, single enforcement chokepoint no. **Resolved by concession: build the visibility layer only.** |
| **Robot/MCP Contract Doctor** (COD#7) | CC **760** vs GMI **400** | GMI: "a test suite masquerading as a feature." COD's defense (88%): in an agent-first platform, machine-contract stability *is* user-facing reliability. **Recommendation: build as contract infrastructure, not marketing surface.** |
| **Agentic SBOM** (GMI#8) + **Replay-to-Live** (GMI#6) | 440 / 420 avgs, but GMI defended both at 90–95% | GMI's stateless-LLM argument for replay-to-live ("scrollback + cwd + files *are* the agent's process state") is genuinely interesting and CC's steelman partially validated the underlying instinct elsewhere. Both need scope cuts (SBOM → tx-mediated writes only; replay-to-live → `ft replay seed-pane` honest framing). **Park as P3 with the reframed scopes.** |

## Killed Ideas (author conceded after adversarial pressure)

| Idea | Scores | Cause of death |
|---|---|---|
| CRDT Active-Active Mission State (GMI#15) | 180/360 | One-shot approval tokens, mutual-exclusion reservations, and hash-chained audit ledgers cannot tolerate eventual consistency; reverses a documented, signed design decision. GMI conceded. |
| Zero-Trust Mission Marketplace (GMI#11) | 220/385 | Inverts the wire protocol's local-authority doctrine; mid-mission partition breaks Tx atomicity; its own retry story double-executes. GMI conceded. |
| Multimodal Visual-AST Rendering (GMI#12) | 250/300 | Breaks capture fidelity, FTS, and **bypasses the redactor entirely** (images aren't text); rests on a false model of how agent CLIs consume context. GMI conceded. |
| PTY RAG Injection (GMI#7) | 280/480 | Violates the passive-first observe/act split; agents don't read scrollback; designed-in prompt-injection channel. GMI conceded; salvageable kernel already ships as `HandleOnErrorCassSearch`. |
| Operator First-Run Guided Tour (COD#12) | 450/250 | Fourth overlapping onboarding surface; the fix is consolidating `ft demo`/`ft doctor`. COD conceded. |
| Agent Mail Outage Spool (COD#10) | 610/320 | Already in flight (ft-dezx8.3, last week's commits); GMI's split-brain attack on queued mutual exclusion landed at 100% confidence. COD conceded everything but the non-authoritative-intent kernel. |
| `ft doctor --fix` unattended mode (CC#15) | 700/410 | COD's cultural argument (this repo's agents have destroyed real work; auto-repair grows into mutation) + GMI's dual-writer corruption scenario. CC conceded: no `--yes` in v1, independent double-probes required. |

## Blind-Spot Round (14 new ideas — the highest-novelty output)

Generated *after* the full adversarial exchange. Not cross-scored; orchestrator-assessed.

**Tier 1 — act on these:**

1. **Retroactive Redaction & Corpus Hygiene** (CC-BS1) — *the only entry that is a live,
   verified gap today*: ingest-time redaction uses the catalog as-of-capture; read-path
   redaction hides the residue; but **`backup.rs` has zero redaction paths**, so backups
   and replay fixtures exported since before the 2026-05 catalog expansion permanently
   carry JWT/GitLab/Twilio/SendGrid/Datadog tokens. Needs `ft redact backfill`,
   `ft redact purge --secret-hash`, and a redacting backup-export path. *The "you won't
   leak a JWT" headline promise is currently false in the artifact most likely to travel.*
2. **Doctrine-to-Policy Compiler** (COD-BS1) — compile AGENTS.md's hard rules (no
   worktrees, no Agent Mail repair, no local-cargo-as-proof, protected crates, no
   `master`) into a versioned `DoctrinePolicy` bundle enforced by Robot/MCP/Tx/policy.
   The repo's worst historical failures are doctrine violations, not runtime bugs.
3. **Virtual Panes / headless-agent ingestion** (CC-BS3) — every idea in the duel assumed
   agent ≡ pane while the ecosystem drifts headless (`claude -p`, `codex exec`, SDK/CI
   agents). `ft virtual run -- <cmd>` (ft-owned PTY) is the cheap grade; a local adapter
   speaking the existing distributed wire protocol is the structural one. Existential hedge.
4. **Economic Circuit Breaker** (GMI-BS1) — token/cost budgets on `MissionTxContract`;
   a runaway loop burning API credits without advancing mission state trips `HardStop`.
   All three models governed CPU/RAM/workers and forgot the metered resource that
   actually bankrupts operators. Natural extension of consensus winner #7.
5. **Agent Compatibility Certification Matrix** (COD-BS3) — `ft agent certify` producing
   per-agent/version receipts (composer detection, second-Enter, reset parsing, resume
   support). The management layer that de-risks Verified-Submit and Durable Sessions.

**Tier 2 — strong, schedule after Tier 1:**

6. **Zero-Downtime Watcher Handoff** (CC-BS5) — drain-and-takeover lock handoff; the 4 KB
   overlap matcher already dedupes the successor's first capture for free. Upgrades stop
   blinding 24/7 fleets.
7. **Evidence Lifecycle & Privacy Budget Manager** (COD-BS2) — retention/minimization/
   promotion-to-fixture discipline; the enabling safety layer for replay/learning ideas.
   Overlaps CC-BS1; build together.
8. **Degraded-Mode Contracts** (COD-BS4) — typed "what's still safe when X is red"
   surfaces (Agent Mail down, RCH down, MCP off). Reduces doctrine-violating improvisation.
9. **Agent A/B Evaluation Plane** (CC-BS2) — randomized work assignment across fleet
   templates, scored by the already-shipped SPRT/conformal `ft-perf-gate`. ft as the
   instrument for optimizing the agents themselves.
10. **Pre-Approval Cross-Examination** (GMI-BS3) — side-channel chat with the paused
    agent before approving (`"why rm -rf instead of git clean?"`). Cheap, humane, novel.

**Also noted:** Terminal-Bypass RPC / Agent Subspace (GMI-BS2), Swarm Janitor ephemeral
leases (GMI-BS4), Control-Plane Cognitive Load shedding (COD-BS5), Governed Subtraction /
parking discipline (CC-BS4 — names the repo's deletion-trauma → addition-ratchet →
dead-wire causal chain; politically delicate but the analysis is correct).

## Score Matrix (all 45 primary ideas)

| # | Idea | Origin | Self-rank | CC | COD | GMI | Avg | Verdict |
|---|---|---|---|---|---|---|---|---|
| 1 | Verified-Submit Dispatch | CC | 1 | — | 930 | 850→950 | **890** | **BUILD (consensus #1)** |
| 2 | watch-events / await | CC | 2 | — | 900 | 820 | **860** | **BUILD** |
| 3 | Dead-Wire Closure + gate | CC | 3 | — | 875 | 760 | **818** | **BUILD** |
| 4 | Steering Loop → receipts | COD | 1 | 780→845 | — | 780↑ | **780→845** | **BUILD (post-steelman)** |
| 5 | Deferred Proof Conveyor | COD | 2 | 820 | — | 750 | **785** | **BUILD (dev-infra lane)** |
| 6 | Rate-Limit Economics | CC | 7 | — | 815 | 740 | **778** | **BUILD** |
| 7 | Terminal Semantic API | GMI | 1 | 760→820 | 780↑ | — | **770→820** | **BUILD (sequence first)** |
| 8 | robot next | CC | 4 | — | 830 | 680 | 755 | Strong; advisory-only |
| 9 | Fleet Reconciliation | CC | 6 | — | 765 | 710 | 738 | Strong; after attention+verified-send |
| 10 | Attention Console | COD | 3 | 830 | — | 600 | 715 | **BUILD (merged w/ deck)** |
| 11 | Time-Travel CI | CC | 5 | — | 800 | 620 | 710 | Strong; tiny corpus first |
| 12 | Swarm Scent | GMI | 4 | 680 | 725 | — | 703 | Fold into attention |
| 13 | Timeline Forensics | CC | 13 | — | 795 | 590 | 693 | Good; cheap |
| 14 | ft deck | CC | 10 | — | 720 | 650 | 685 | Merged with #10 |
| 15 | Governor Mesh | COD | 4 | 560 | — | 810 | 685 | Contested→conceded: visibility only |
| 16 | Sludge Compaction | GMI | 2 | 640 | 690 | — | 665 | Redesign: hash-identity, serve-time |
| 17 | Durable Agent Sessions | CC | 8 | — | 745 | 580 | 663 | Good; fallback-ladder design |
| 18 | Contract SDKs | CC | 14 | — | 810 | 490 | 650 | Contested; generated-thin only |
| 19 | Ownership Firewall | COD | 13 | 660 | — | 640 | 650 | After reservation unification |
| 20 | Incident Timeline | COD | 14 | 720 | — | 570 | 645 | Good; offline-first |
| 21 | RCH Explainer | COD | 9 | 700 | — | 500 | 600 | Front-end to #5 |
| 22 | Target-Class Campaign | CC | 11 | — | 835 | 350 | 593 | **Contested — recommend build** |
| 23 | WASM phased | CC | 12 | — | 665 | 520 | 593 | Later wave |
| 24 | Rehearsal Scorer | COD | 8 | 620 | — | 550 | 585 | Reframe: findings, not score |
| 25 | Extension Workbench | COD | 5 | 680 | — | 480 | 580 | Later wave |
| 26 | Contract Doctor | COD | 7 | 760 | — | 400 | 580 | Contested — recommend build |
| 27 | Chaos Monkey (Cx-fuzz) | GMI | 13 | 540 | 610 | — | 575 | CI-scoped, seeded only |
| 28 | Taint + Canaries | CC | 9 | — | 690 | 450 | 570 | Canaries+labels now; taint observe-mode |
| 29 | doctor --fix | CC | 15 | — | 700 | 410 | 555 | Conceded down: no unattended v1 |
| 30 | Semantic Breakpoints | GMI | 3 | 520 | 560 | — | 540 | Rescue: policy-pause, not SIGSTOP |
| 31 | Adversarial Consensus | GMI | 9 | 480 | 590 | — | 535 | Needs structured reviewer protocol |
| 32 | Predictive Shedding | GMI | 10 | 460 | 555 | — | 508 | Static cost table first, no WASM |
| 33 | Swarm Learning Remediation | COD | 6 | 600 | — | 380 | 490 | Evidence-retrieval only |
| 34 | Attestation Explorer | COD | 11 | 590 | — | 390 | 490 | Background QoL |
| 35 | Agent Mail Spool | COD | 10 | 610 | — | 320 | 465 | Conceded (in-flight ft-dezx8) |
| 36 | Agentic SBOM | GMI | 8 | 340 | 540 | — | 440 | Parked; tx-mediated scope only |
| 37 | Replay-to-Live | GMI | 6 | 450 | 390 | — | 420 | Parked as `replay seed-pane` |
| 38 | Ghost Panes | GMI | 5 | 380 | 430 | — | 405 | Conceded (macOS, no-worktree rule) |
| 39 | Formal Verification Lane | GMI | 14 | 300 | 500 | — | 400 | CI path-filter sliver only |
| 40 | PTY RAG Injection | GMI | 7 | 280 | 480 | — | 380 | **KILLED** (conceded) |
| 41 | First-Run Tour | COD | 12 | 450 | — | 250 | 350 | **KILLED** (conceded) |
| 42 | Mission Marketplace | GMI | 11 | 220 | 385 | — | 303 | **KILLED** (conceded) |
| 43 | Visual-AST Rendering | GMI | 12 | 250 | 300 | — | 275 | **KILLED** (conceded) |
| 44 | CRDT Active-Active | GMI | 15 | 180 | 360 | — | 270 | **KILLED** (conceded) |

## Meta-Analysis

**Model personalities, as revealed by 90 scores and the rebuttals:**

- **Claude Code** anchored everything in verified tree facts (caller-graph audits,
  grep-verified claims like `SemanticZone` in the vendored term crate, `backup.rs`'s
  missing redaction) and was by far the **harshest grader** (means 668/449 vs receiving
  788/615). Bias: composition-over-invention; its list optimizes "pain × uniqueness ×
  feasibility" and occasionally underweights visionary ceiling — its two steelman
  revisions (+65, +60) show its initial skepticism of the others' #1s was partly habit.
- **Codex** is the systems integrator: its center of gravity drifted toward dev-infra
  meta-tooling for working *on* ft (proof conveyor, RCH explainer, attestation explorer) —
  CC's sharpest structural critique. It was the **most concessive** under pressure
  (formally downgraded 5 of 15) and the most generous grader of CC. Its blind-spot round
  was arguably the best of the three (doctrine compiler, evidence lifecycle, compatibility
  matrix all target *assumptions*, not features).
- **Gemini** was the most conceptually original and the least doctrine-grounded: 6 of 15
  ideas collided with documented facts of this tree (passive-first invariant, macOS
  primary, advisory Agent Mail, single-writer-by-doctrine, how agent CLIs actually consume
  context), and it scored on a novelty axis the brief didn't ask for (Target-Class at 350
  as "just a Jira ticket"). But it conceded 5 ideas with complete intellectual honesty,
  produced the duel's single best implementation insight (the cryptographic submit canary),
  and its #1 (Terminal Semantic API) was the only externally-generated idea both rivals
  ranked top-tier *and* revised upward after steelmanning.

**Where adversarial pressure demonstrably improved output:** the steelman round flipped
three #1 ideas into stronger forms than their authors wrote (canary-verified submit;
receipt-hash steering; spawn-path shell integration for the semantic API). The blind-spot
round then exposed the four framing assumptions all three models shared — data-as-asset,
platform-as-ambient, agent-as-pane, addition-as-improvement — which no amount of
cross-scoring within the frame had touched.

**Convergence signal:** zero idea-level overlap in the initial top-5s (maximum diversity),
yet all three models independently proposed activating the same dormant subsystems
(intervention console, BOCPD, governors) and independently identified verified-submit +
semantic-structure + plan/execute-binding as the three load-bearing gaps. That triple
convergence from maximally divergent lists is the strongest validation this methodology
can produce.

## Recommended Next Steps

Dependency-ordered build sequence synthesized from all rounds:

1. **Retroactive Redaction backfill + redacting backup export** (CC-BS1) — close the
   verified live gap first; it is silently violating a README promise today.
2. **Terminal Semantic Pane API, Phase 1** (GMI#1) — expose existing `SemanticZone`s +
   grid queries; per-pane availability in `ft doctor`. Foundation for #3.
3. **Verified-Submit Send** (CC#1) — built on #2's composer/region queries, with GMI's
   canary verification and COD's receipt schema + idempotency keys; adopt into
   `HandleCompaction`/`HandleUsageLimits`.
4. **`ft robot watch-events` + `await`** (CC#2) — the event-driven meta-agent loop.
5. **Dead-Wire Closure** (CC#3) — shadow-mode BOCPD/governor wiring + the deadwire CI
   gate (with bead-referenced, expiring dormant exemptions).
6. **SteeringReceipt** (COD#1, CC-steelman form) — hash-bound plan→execute binding;
   receipt-as-approval-alternative policy capability.
7. **Attention Console (read-only) → `ft intervene` verbs → deck** (COD#3 + CC#10).
8. **Rate-Limit Ledger + Economic Circuit Breaker** (CC#7 + GMI-BS1).
9. **Deferred Proof Conveyor** (COD#2) — in the dev-infra lane, in parallel.
10. **Target-Class Campaign** (CC#11) — group-commit hardening + the rented 64-core run;
    schedule when a release window calls for it.

---

*Report compiled by the orchestrator from 19 agent-written artifacts. Scores and
arguments are reported faithfully; orchestrator opinion is confined to the Meta-Analysis
and the contested-idea recommendations. No code or configuration was modified during the
duel; the only files written are the `WIZARD_*.md` artifacts and this report.*
