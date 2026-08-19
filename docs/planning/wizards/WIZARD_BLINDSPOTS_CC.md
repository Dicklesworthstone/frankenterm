# WIZARD_BLINDSPOTS_CC.md — What None of the Three Models Thought Of

> Claude Code (Opus 4.8), 2026-06-06. Final round, Part 2. Input: all 45 ideas
> across the three lists, all six cross-score files, both rebuttal rounds, and
> the steelman pass. The question: what did the entire adversarial process
> fail to surface? Every load-bearing claim below was verified against the
> tree before writing (notably: `redact_segment_for_persistence` in
> `storage.rs` proves ingest-time redaction exists; `backup.rs` — 101KB —
> contains **zero** references to redaction; "PARK" is existing ADR
> vocabulary in `docs/proposals/ft-t2d70-*`; the 4KB overlap-matching delta
> extractor is the documented capture mechanism).

---

## The three lenses, applied first

**Gap analysis — the absent categories.** Mapping all 45 ideas onto a
capability grid, two whole categories have zero entries: (1) **the lifecycle
of the captured data itself** — every model treated the capture corpus as an
asset to mine (search, replay, forensics, learning) and none treated it as a
*liability to manage* (temporal redaction drift, purge, derived-store
hygiene); (2) **the lifecycle of ft itself as a long-running process** —
all 45 ideas operate inside a running platform; none asked what happens to a
24/7 swarm when ft is upgraded.

**Synthesis insight — visible only across lists.** Combining my economics
ledger (#7) + Codex's learning loops (#6/#8) + Gemini's correlator ideas
(#4) exposes a shared unexamined premise: all three models treat the
*agents* as fixed externalities to be managed. Nobody noticed that ft sits
on the only honest instrument for *optimizing* them — and that the
statistical machinery to do it rigorously (`ft-perf-gate` with SPRT and
conformal gating) already ships in this workspace.

**Contrarian take — what all three assumed was fine.** Two assumptions
survived 45 ideas, six score files, and two rebuttal rounds unchallenged:
(1) **that an agent is a pane** — every single idea binds agent identity to
mux topology, while the agent ecosystem is visibly drifting headless
(`claude -p`, `codex exec`, Agent-SDK processes, CI-resident agents);
(2) **that addition is improvement** — all 45 ideas add surface to a
codebase with 512 top-level core modules, 77 crates, and a documented
dead-wire problem that exists *because* building outpaced wiring. Not one
idea proposed governed subtraction. There is a reason for that silence, and
the reason is itself worth naming: this repo's history of catastrophic
agent-initiated deletion (frankenterm-core deleted three times; the
NO-DELETION rule; the pre-commit mass-deletion guard) has made *removal* so
culturally radioactive that even reversible, human-gated reduction became
unthinkable. The trauma created the ratchet; the ratchet created the
dead-wire problem the lists then proposed to detect.

Five ideas follow from these lenses.

---

## Blind Spot 1: Retroactive Redaction & Corpus Hygiene — the temporal gap in the secrets story

**The idea.** A redaction time-machine: catalog-versioned capture, a
`ft redact backfill` sweep that re-applies the *current* pattern catalog to
at-rest segments and rebuilds derived stores (FTS5, Tantivy, embeddings),
a targeted `ft redact purge --secret-hash <h>` for incident response
("token X leaked into pane 7 last Tuesday — excise it everywhere"), with
tombstone receipts and an attestation slot recording what was swept when.
Plus the one-line fix hiding inside the big one: **backup export must apply
current-catalog redaction** — verified today, `backup.rs` contains no
redaction path, so exported archives carry whatever ingest-time redaction
missed, forever, beyond retention.

**Why nobody thought of it.** All three models cited the redactor
approvingly (32+ patterns, T1/T2/T3 tiers, an attestation slot, a 2026-05
coverage expansion) and pattern-matched "redaction: covered." The blind
spot is *temporal*: redaction runs at ingest (`redact_segment_for_
persistence`) with the catalog **as of capture time**, and again on read
paths with the current catalog. Neither pass touches what's already at
rest. When JWT/GitLab/Twilio/SendGrid/Datadog patterns landed in May,
every byte captured before that day kept those token families in
`output_segments` for the retention window — and in any backup exported
since, permanently. Read-path redaction hides this from API consumers,
which is exactly why nobody noticed: the leak is invisible through every
surface the evaluators thought to check. You only see it by asking *when*
each redaction layer runs — a question none of 45 ideas asked.

**Why it matters.** The project's headline promise — "you won't
accidentally leak a JWT" — is true at the API and false in the backup
archive, the shared replay fixture, and the attached incident bundle the
moment the catalog lags reality (which is always; that's what catalog
updates mean). These derived artifacts are precisely the ones that *travel*
— bug reports, fixture repos, off-host backup destinations — so the
weakest copy of the data is the most mobile one. For a platform whose
differentiation is attested security claims, an unredactable-history gap
is a future CVE-shaped embarrassment that costs little to close now
(sweep + rebuild machinery; the secret-scan reporting surface that already
exists in storage provides the detection half) and a great deal to close
after someone publishes it.

---

## Blind Spot 2: The Agent A/B Evaluation Plane — ft as the instrument for optimizing the agents themselves

**The idea.** Turn the fleet into a controlled experiment platform:
`ft experiment run --template-a codex_v1 --template-b codex_v2 --work-source
beads-ready --n 30`. Equivalent work items from the existing `work_claims`
queue are randomly assigned across two fleet-template variants (different
CLI versions, system prompts, model choices, context budgets); outcome
metrics are harvested from capture (time-to-completion, error-detection
counts, limit events, gap events, proof outcomes as ground truth); and the
verdict is computed by the **already-shipped** statistical gate —
`ft-perf-gate`'s SPRT stops the experiment as soon as significance is
reached, and the methodology playbook's Mann-Whitney/Hoeffding machinery
prices the sample sizes. Output: a signed experiment receipt ("variant B:
22% faster to green proof, p<0.01, n=24; limit events unchanged").

**Why nobody thought of it.** All 45 ideas point ft's instruments *at the
swarm's work*; none point them at the swarm's *configuration*. Three
models independently treated agents as fixed externalities — things to
detect, throttle, resume, sandbox, and coordinate — because the entire
framing of the exercise ("improve this platform") quietly cast agents as
the environment rather than as variables. It takes combining three
separately-proposed pieces (my economics metrics, Codex's typed work
queue usage, the perf-gate statistics that no list mentioned at all) to
see that the experiment platform is already 80% assembled, lying in
pieces across the tree.

**Why it matters.** "Is the new codex version better? Does this prompt
prelude help? Is Sonnet cheaper-per-merged-PR than Opus here?" are the
most expensive recurring questions every swarm operator faces, currently
answered by vibes and anecdote. ft is the *only* layer holding the honest
cross-pane dataset, and uniquely owns randomized assignment (the work
queue) and rigorous stopping (SPRT). This is also the rare idea that
makes ft more valuable as the agent ecosystem churns *faster* — every new
model release makes the evaluation plane more necessary. It converts the
platform from infrastructure cost into a compounding optimization engine,
and no competitor in the comparison table could even attempt it.

---

## Blind Spot 3: Virtual Panes — the headless-agent escape hatch

**The idea.** Decouple ft's unit of observation from mux topology: a
*virtual pane* adapter that ingests headless agent sessions — `claude -p`
runs, `codex exec` invocations, Agent-SDK processes, CI-resident agents —
into the same capture/detect/policy/search/event pipeline as real panes.
Two implementation grades: (a) the cheap one, `ft virtual run -- <cmd>`,
which allocates an ft-owned PTY around a headless invocation so it simply
*is* a pane (full pipeline compatibility for free); (b) the structural
one, a local adapter speaking the **existing distributed wire protocol**
— which already abstracts pane sources into versioned envelopes of
metadata + deltas + gaps — fed from a process supervisor or session-log
tailer instead of a remote mux.

**Why nobody thought of it.** Terminal-platform framing anchored all
three models absolutely: 45 ideas, and every one of them assumes
agent ≡ pane. The assumption is invisible because it's load-bearing in the
product's name, README, and architecture diagrams. But the ecosystem is
visibly moving: every major agent CLI now ships a headless/print mode,
SDK-based agents run as plain processes, and CI is becoming a major agent
habitat. None of that work happens in a pane today — which means none of
it is observed, searched, policy-gated, or audited by ft. The three
models critiqued each other's ideas *within* the pane world; nobody
audited the world.

**Why it matters.** This is the existential hedge. If agents drift
headless and ft's premise stays "we observe panes," the addressable
surface of the entire platform — and of all 45 ideas — shrinks year over
year. The escape hatch is cheap *because* the architecture is honest:
capture works on byte streams, detection works on text, policy gates
actions, and the wire protocol already proved that pane-ness is an
adapter concern (remote panes have no local mux either). One deliberate
abstraction now ("an observed session is anything that produces ordered
output and accepts gated input") future-proofs a decade of platform
investment, and immediately delivers the most-requested missing surface:
audit and search over CI-agent runs with the same guarantees as
interactive ones.

---

## Blind Spot 4: Governed Subtraction — a parking discipline for the surface the project already has

**The idea.** The anti-additive program: a *surface budget* with a
reversible, human-gated **parking** mechanism. Every dormant subsystem
identified by the wiring inventory gets one of three dispositions, each
recorded in the attestation graph: **wire** (with a bead and a date),
**park** (removed from the default workspace build — workspace-membership
exclusion or feature-gating, code fully retained in-tree, restoration =
one Cargo.toml line), or **attest-as-dormant** (kept building, explicitly
labeled, with an expiry). Never deletion — parking is `cargo`-level
quarantine with provenance, the precise opposite of `rm`. The repo already
invented the vocabulary: the ft-t2d70 extraction ADR's verdict is
literally "PARK."

**Why nobody thought of it.** Two reinforcing causes. First, ideation
prompts select for addition — "improve" reads as "add," and all three
models obliged with 45 additive ideas aimed at a codebase of 1M+ core
lines, 512 top-level modules, and 56k tests whose cold-build cost already
warranted its own measurement ADR. Second, and more interesting: this
repo's deletion trauma. Agents destroyed frankenterm-core three times;
the response — absolute no-deletion rules, mass-deletion pre-commit
guards — was correct, and it had a side effect: *all* removal became
unthinkable, even reversible, human-approved, build-level removal. The
models (me included, until this pass) inherited that flinch. But the
dead-wire problem that two of three lists proposed to *detect* is the
direct product of an addition-only ratchet; detection without a
subtraction option just converts unknown debt into known debt and lets it
sit.

**Why it matters.** Every dormant module is a standing cost with no
offsetting revenue: cold-build minutes for all agents on every machine,
clippy/test wall-time, audit surface the substrate-sweep discipline must
keep re-clearing (the memory corpus shows repeated sweeps re-verifying
the same untouched modules), and — most expensive in this project
specifically — *honesty budget*: every built-but-unwired subsystem is a
claim someone must keep fencing in the README. Parking converts those
carrying costs to ~zero while preserving every line of work product and
a one-line restoration path. The cultural fit is better than it looks:
this is the same move as the windows-coupling ratchet and the count
stamper — replace an unbounded organic process with a governed,
attested, reversible one.

---

## Blind Spot 5: Zero-Downtime Watcher Handoff — closing the fleet-wide blind window

**The idea.** A drain-and-takeover protocol so that upgrading or
restarting ft never blinds the fleet: `ft watch --takeover` starts the
new watcher in standby; the old watcher finishes its tick, flushes the
write queue, checkpoints the WAL, transfers the single-writer lock
through a handshake file (PID + generation + cursor), and exits; the new
watcher resumes capture immediately — and here is why this is cheap in
*this* codebase — **the 4KB overlap-matching delta extractor was
accidentally designed for exactly this**: the successor's first capture
of every pane overlap-matches against the predecessor's persisted tail
and dedupes seamlessly, no gap event, no double-ingest. The handoff is
recorded as a typed lifecycle event with generation numbers in
`ft_meta`.

**Why nobody thought of it.** All three models — across observation,
control, learning, forensics, security, and scale — treated the running
watcher as ambient infrastructure, the way fish treat water. Every idea
begins "while ft is running…". But the project's own cadence makes the
gap concrete: this repo ships continuously (the changelog is relentless),
and operators of 24/7 swarms — the flagship persona — must today choose
between running stale and accepting a fleet-wide capture gap plus an
unclean-shutdown restore prompt on every upgrade. "No silent gaps" is a
core guarantee about *pane* discontinuities; nobody noticed the platform
schedules a *fleet-wide* discontinuity every time it improves itself.

**Why it matters.** Beyond the direct fix (upgrades stop costing
observability), the handoff protocol is the enabling substrate for
operational maturity the other 45 ideas quietly assume: a reconciler
(my #6) that survives its own platform's upgrades; experiment runs
(Blind Spot 2) that span releases without data seams; incident-grade
audit trails with no scheduled holes for things to hide in. It is also
the cheapest idea in this file — the lock already records PID and start
time, the writer already drains on clean shutdown, WAL checkpointing
already exists, and the overlap matcher does the hard deduplication for
free. Mostly this is *sequencing* existing shutdown and startup code
into an overlapped order, plus one handshake file and honest generation
stamps.

---

## Closing note on the process

The adversarial rounds were good at sharpening and ranking what was
already on the table, and systematically bad at noticing what wasn't:
three models with different biases still shared the same four framing
assumptions (data-as-asset, platform-as-ambient, agent-as-pane,
addition-as-improvement), and cross-scoring punished weak ideas without
ever questioning the frame that generated all of them. The five ideas
above are the residue of auditing the frame instead of the ideas. If I
had to fold one into the original top-5, it's Blind Spot 1 — it's the
only one where the gap is silently violating a promise the README makes
today.

*This file and WIZARD_STEELMAN_CC.md are the only files written in this
step; no code or configuration was modified.*
