# WIZARD_SCORES_CC_ON_GMI.md — Claude Code's Evaluation of the Gemini Idea List

> Evaluator: Claude Code (Opus 4.8), 2026-06-06. Basis: full read of
> WIZARD_IDEAS_GMI.md against my six-stream code investigation of this repo,
> plus verification greps for specific claims (I confirmed, among others, that
> `robot_api_contracts.rs` exists, that the vendored `term` crate already
> tracks `SemanticZone`s, and that no SIGSTOP/pause-on-match machinery exists
> today). Scores are 0–1000, deliberately spread; bands: **800+** build it,
> **650–799** strong with real caveats, **500–649** plausible but caveats
> bite, **350–499** weak utility-to-complexity, **<350** architectural misfit.
>
> Overall read of this list: high variance — more conceptually original than
> the Codex list at the top, but it repeatedly collides with this codebase's
> actual doctrine and platform. Three recurring failure modes: (1) violating
> the passive-first observe/act split by mutating panes or capture from the
> observation path (#7, #12, partially #3); (2) a factual misunderstanding of
> how agent CLIs consume context — Claude Code and codex read files and API
> streams through their own tooling, **not** by looking at the terminal
> scrollback, which guts the core rationale of #7 and #12; (3) proposing
> Linux-flavored or distributed-systems machinery for a project whose primary
> platform is macOS and whose distributed design deliberately keeps authority
> local (#5, #11, #15). Mean ≈ 449; range 180–760.

| # | Idea | Score |
|---|---|---|
| 1 | Terminal-Native DOM & Semantic Query API | 760 |
| 2 | Automated "Sludge" Compaction via BOCPD | 640 |
| 3 | Semantic Breakpoints & Intervention Console | 520 |
| 4 | Cross-Swarm "Scent" via Agent Correlator | 680 |
| 5 | "Ghost Panes" Copy-on-Write Speculation | 380 |
| 6 | Time-Travel Debugging: Replay-to-Live Handoff | 450 |
| 7 | Corpus-Aware RAG Injection at the PTY Layer | 280 |
| 8 | Cryptographic Provenance / Agentic SBOM | 340 |
| 9 | Adversarial Consensus Engine | 480 |
| 10 | Predictive Capacity Shedding via WASM | 460 |
| 11 | Zero-Trust Distributed Mission Marketplace | 220 |
| 12 | Multimodal Terminal Emulation (Visual-AST) | 250 |
| 13 | Stochastic Chaos Monkey (Cx-Fuzzing) | 540 |
| 14 | Continuous Formal Verification for Agent Txs | 300 |
| 15 | CRDT-Backed Active-Active Mission State | 180 |

---

## 1. Terminal-Native DOM & Semantic Query API — **760**

The best idea on this list, and the rationale ("the only terminal that speaks
agent natively") is the right strategic frame. Feasibility is better than even
the author argues: the vendored `term` crate *already tracks semantic zones*
(OSC 133 prompt/input/output marks), so `get_last_command()` /
`find_prompts()` / `get_exit_code()` are projections of existing in-memory
state, and `robot_api_contracts.rs` is real and the right home. **Strongest
argument against:** the flagship workload of ft panes is agent-CLI TUIs —
codex, claude-code, gemini render full-screen interfaces where semantic zones
don't exist and "last command / exit code" is meaningless — so the DOM's
value concentrates in plain build/test/shell panes, a real but minority slice
of the fleet; and without shell integration emitting OSC 133, the API
degrades to heuristics that recreate exactly the brittleness it promises to
eradicate. The honest version ships with explicit
`semantic_data_unavailable` states per pane and a bundled shell-integration
installer; the "10/10 confidence" is overclaimed, but the idea is genuinely
strong.

## 2. Automated "Sludge" Compaction via BOCPD — **640**

Attacks a real and expensive problem — repetitive loop output flooding agent
context windows — and putting the fix at the platform layer is the right
instinct. **Strongest argument against:** the mechanism is doubly misfit.
First, BOCPD detects *distributional change-points* via run-length
posteriors; it does not identify *cyclic repetition* — recurring
near-identical blocks are precisely what the codebase's existing dedup
primitives (SHA-256/FNV-1a fingerprints, XOR filters) detect deterministically
and cheaply, so the statistically glamorous tool is the wrong one for the
named job. Second, "replaces the repetitive scrollback blocks" as written
threatens capture fidelity — `output_segments` is the canonical, auditable
record, and lossy compaction at storage would break replay, forensics, and
the "never invents data" guarantee; the only acceptable design is
presentation-layer elision at `get-text` serving time (opt-in flag, raw bytes
retained), which the writeup doesn't distinguish. The risk that compaction
swallows the one *changed* line in iteration ten — the line that matters — is
exactly why hash-identity, not statistical similarity, must drive collapse.
Salvageable and valuable once redesigned; scored as proposed.

## 3. Semantic Breakpoints & The Intervention Console — **520**

The product concept — pattern-triggered auto-pause routed to a
human-in-the-loop console — is genuinely valuable, and wiring the dormant
intervention console is the right destination. The mechanism is where it
bleeds. **Strongest argument against:** SIGSTOP'ing the PTY child is the
wrong actuator: agent CLIs hold live streaming connections to model APIs, and
a process frozen mid-stream for minutes resumes into dropped connections and
wedged TUI state, converting "paused for safety" into "killed with extra
steps." Worse, an *automatic* mutation triggered by pane output reopens the
exact privilege-amplification channel ft-j0ufc closed for workflows — any
pane that can print a matching pattern can freeze panes, i.e., DoS via output
injection — so the trigger must pass the same trust-policy gate, which the
proposal omits. And detection happens *after* output is captured, so "catching
bad outcomes mid-flight" oversells post-hoc pausing of mostly-completed
actions. The salvage path (policy-gated, per-pane opt-in, pause = withhold
further policy-gated sends rather than freeze the process) keeps the value
and drops the hazards; the SIGSTOP design as written does not survive
contact with this codebase's threat model.

## 4. Cross-Swarm "Scent" via Agent Correlator — **680**

A good, cheap, read-only idea: aggregate CWDs, reservations, work claims, and
TxIntents into a low-latency awareness endpoint so siblings divide instead of
collide. The correlator is one of the few audited subsystems that's *actually
live*, and a `wa://swarm/scent` resource is a thin projection over existing
sources — low risk, real coordination value, honest about needing prompt
tuning for adoption. **Strongest argument against:** the informative part of
"scent" (what an agent is *actually working on*) already lives in beads
assignments and Agent Mail reservations, which working swarms — including
the dozen-agent swarms that built this repo — successfully coordinate on
today, so the marginal value is aggregation convenience rather than new
signal; and stale scent is worse than none (an abandoned trail parks
siblings off `policy.rs` for an hour after the owner moved on), which demands
freshness/TTL discipline the proposal doesn't specify. The "breaking past the
2-agent-per-repo limit" framing is empirically false for this repo and
oversells the bottleneck. Good idea, modest ceiling.

## 5. "Ghost Panes" via Copy-on-Write Speculative Execution — **380**

Conceptually the most seductive idea here, and "systemic rollback for *any*
action" is a real gap that TxCompensation's known-undo-paths don't fill.
**Strongest argument against:** the implementation premise doesn't run where
this product lives. The primary platform is macOS (FrankenTerm.app; the dev
host is Darwin) and macOS has no overlayfs and no network namespaces — APFS
clones don't give you a transparent process-scoped union mount without
root/VM machinery that the `#![forbid(unsafe_code)]`, Command-based-API
codebase is deliberately structured to avoid. The natural 80% substitute —
isolated git worktrees — is *explicitly forbidden by repo rule #2*. And even
on Linux, "the Policy Engine reviews the diff and commits the overlay" hides
the hard part: concurrent host mutations during speculation, SQLite WAL
state, and unrollbackable external side effects (network calls, API spend)
leak straight through the sandbox illusion. The author's own 7/10 confidence
is the most honest number in the file; mine is lower. File under "endgame
vision," not under "accretive and pragmatic."

## 6. Time-Travel Debugging: Replay-to-Live Handoff — **450**

The demo writes itself, which is exactly the danger. **Strongest argument
against:** the proposal conflates restoring the *appearance* of a past state
with restoring the *state*. A decision-graph node does not checkpoint the
processes — the agent CLI's in-memory conversation, the filesystem, network
sessions, model-side context — so "branching" instantiates a fresh pane with
historical scrollback injected, into which a *new* agent process arrives with
none of the original's actual state. That's not resuming a branched reality;
it's a context-preloaded recreation, and presenting it as time travel
invites false confidence in reproductions ("the fix worked in the branch")
that the original causal conditions never tested. The honest, smaller feature
— `ft replay seed-pane <node>` to spawn a what-if pane preloaded with
historical context — is worth building someday and is what this idea actually
describes; the VirtualClock-to-realtime handoff and "exact historical
context" framing are aspiration wearing implementation's clothes. The 8.5/10
confidence is the least calibrated number on the list.

## 7. Corpus-Aware RAG Injection at the PTY Layer — **280**

This one fails on three independent grounds, any one of which is
disqualifying as designed. First, it breaks the platform's most fundamental
invariant — the observation pipeline "has no side effects" — by suspending
PTY output and injecting synthetic content from *inside* the capture path;
the entire policy/audit edifice exists so that nothing writes to panes except
gated actions. Second, **the mechanism misunderstands how agent CLIs consume
context**: claude-code and codex compose their model context from their own
tool calls and API streams — they do not read the terminal scrollback — so a
"Terminal Hint" rendered into the pane reaches the human observer and the
capture DB, not the model; the centerpiece perception ("the terminal
miraculously gives the agent the solution") simply doesn't happen for the
agents this product targets. Third, even where it could work (raw shell
REPLs), injecting retrieved text into command streams is a designed-in
prompt-injection channel — a poisoned cass corpus becomes instructions
delivered into every erroring session. **Strongest argument against:** all of
the above; the salvageable kernel (surface cass matches as *event metadata*)
already ships as `HandleOnErrorCassSearch`. Score reflects a good motivation
attached to a mechanism that is wrong about the system and wrong about the
agents.

## 8. Cryptographic Provenance / Agentic SBOM — **340**

The enterprise problem is real and well-chosen; the architectural premise is
false. **Strongest argument against:** the justification asserts that
"FrankenTerm already mediates all file access via Agent Mail and the Tx
engine" — it does not. Agents edit files through their own in-process tools
(Edit/Write/apply-patch), entirely invisible to ft; Agent Mail reservations
are *advisory* leases in an external service, not a mediation layer; and the
`storage_writer` captures terminal output deltas, not filesystem diffs. ft
therefore cannot sign per-edit diffs it never observes, and reconstructing
file mutations from scrollback is hopeless. The honest implementation of
agentic provenance is a git-layer concern — signed commits, commit-time
attribution hooks — which needs none of ft's machinery, while the slice ft
*can* attest (policy-gated sends, Tx receipts, already audit-logged) is not
"who wrote this code." Reusing the Sigstore plumbing is a nice touch on top
of a foundation that isn't there.

## 9. Adversarial Consensus Engine (Multi-Model Debate Gate) — **480**

There's a defensible kernel: the policy engine already returns
`RequireApproval`, and "an adversarial AI reviewer as an *optional approval
authority*" is a legitimate, even compelling, extension of that machinery —
multi-model review demonstrably catches single-model failures. **Strongest
argument against:** the proposed verdict channel is load-bearing scrollback
scraping. The spawned Reviewer is an agent CLI in a pane; extracting a
security-grade allow/deny from its TUI output reintroduces — at the policy
boundary, the worst possible place — precisely the text-parsing brittleness
ft exists to eliminate, and a misparsed verdict here is a silent policy
bypass or a stuck Tx. Add latency and token cost in the approval hot path,
the deadlock surface of policy waiting on an agent that may itself hit a
rate limit, and the "infallible supervisor" framing that this repo's
attestation culture would never let past review. Buildable someday atop a
structured reviewer protocol (robot-mode-speaking reviewer, not scraped
panes); as designed, the weakest link sits at the strongest gate.

## 10. Predictive Capacity Shedding via WASM — **460**

Predictive admission ("this command *will* cost 4 GB") is a sound
complement to the reactive tier model, and routing predictions into the
dormant capacity governor is the right consumer. **Strongest argument
against:** the WASM vehicle inverts the cost/benefit. A static cost-model
table for the small universe of relevant commands (cargo verbs × scope ×
crate-graph size) is a few hundred lines of Rust achieving the core value;
instead the proposal gates it behind activating the entire wasmtime runtime
— fuel accounting, sandbox hosting, module distribution — to make the
*lookup table pluggable*, putting a sandbox round-trip in the Tx-prepare hot
path for flexibility nobody has asked for. Prediction accuracy is also
shakier than presented (cargo's footprint depends on cache state and
parallelism more than on the command string), so mispredictions will both
under-shed and over-defer until a feedback loop exists. Decouple the good
small idea from the big dormant subsystem and it scores 150 points higher.

## 11. Zero-Trust Distributed Mission Marketplace — **220**

The most untethered idea on the list. **Strongest argument against:** it
proposes a distributed-systems research program — bidding consensus,
distributed `Cx` cancellation, cross-host trust tiers, partition-tolerant
mission handoff — for a project whose distributed layer was *deliberately*
designed around the opposite principle: local receipt-clock authority,
untrusted remote clocks, aggregator-local decisions (the wire protocol's
documented defensive core). "Extending the Cx lifecycle across the network"
is a sentence that costs a paper, not a sprint; cancellation correctness
under partition is precisely what the structured-concurrency model has never
promised. And it solves a scale problem the project provably doesn't have —
single-host 200-pane capacity is still `skipped_not_proven` in the
attestation graph, making "infinite scalability" an aspiration stacked on an
unproven foundation. The honest near-term version (remote hosts *advertise*
envelope headroom; a human or meta-agent picks) needs none of the
marketplace machinery.

## 12. Multimodal Terminal Emulation (Visual-AST Rendering) — **250**

**Strongest argument against:** it shares idea #7's fatal misunderstanding
and adds its own. Intercepting `cat`/`bat`/`tree` and substituting rendered
AST images means the pane no longer displays what the process emitted —
breaking capture fidelity, search (you can't FTS5 a PNG), redaction (the
redactor matches text, not pixels — a rendered image of a file *bypasses the
secret-redaction pipeline entirely*, a regression of a load-bearing security
guarantee), and the observer doctrine, all at once. And the beneficiary
doesn't exist as described: cc/codex agents read files through their own
tools and never see the pane, while "vision agents screenshotting panes" is
a workflow ft has no pipeline for. The defensible remnant — an explicit
`ft viz <file>` command rendering diagrams for *humans*, never intercepting
anything — is a pleasant minor feature unrelated to the stated rationale.
The kitty-graphics citation (ft-t9ydu) is real but is sanitization work,
evidence of the attack surface this idea would widen.

## 13. Stochastic Chaos Monkey (Continuous Cx-Fuzzing) — **540**

The sharpest technical observation on the list: `runtime_async` *is* a
single chokepoint where jitter, delayed wakes, and injected `Error::Cancelled`
can be threaded across the entire workspace, and the implementation-ease
claim is credible. **Strongest argument against:** the marginal value over
existing machinery is thinner than it looks — LabRuntime already does
seeded, deterministic schedule exploration, Loom already model-checks the
cancellation interleavings (with an attestation slot), and `chaos.rs` plus
the chaos-scale harness already exist for fault injection — so the FT-
hardening half of the pitch is substantially "promote existing harnesses to
a scheduled CI lane," which is good but not new. The novel half ("train
agents by degrading their terminal") faces an adoption wall: operators
optimize for throughput, and a flag that makes your own swarm flaky on
purpose is the first flag nobody enables. Deterministic, seeded, CI-scoped:
yes. "Continuous" in any live environment: no.

## 14. Continuous Formal Verification Lane for Agent Txs — **300**

**Strongest argument against:** the proposal concedes its own core — "automatically
generating TLA+ or Lean specs for arbitrary agent code is an unsolved
research problem" — and what remains after that concession is "run the
repo's existing loom harnesses against agent modifications," which is a
description of CI, which exists. It also misroutes the gate: ft's Tx engine
orchestrates *pane operations*; it does not mediate code merges, so
"PolicyEngine routes the patch to rch for verification before the Tx
commits" grafts a code-review concern onto a terminal-transaction mechanism
that never sees patches. The "reject with mathematical counterexample"
experience requires per-change specs that nobody — agent or human — is
writing. The legitimate sliver (auto-run concurrency-relevant existing
harnesses when agent diffs touch `runtime_async`-adjacent code) is a CI
path-filter rule, not a subsystem. Lovely banner; no load-bearing wall
behind it.

## 15. CRDT-Backed Active-Active Mission State — **180**

Lowest score in either list, because it proposes to spend the most effort
reversing the most deliberate decision. **Strongest argument against:**
single-writer integrity is not an accident to be engineered away — it's
Design Philosophy #4, with the trade-off explicitly accepted in writing
("at fleet-of-thousands scale write throughput would become a bottleneck.
We're not there.") — and the data this idea would CRDT-merge is precisely
the data that *cannot* tolerate eventual consistency: approval tokens are
one-shot (concurrent consumption on two hosts is a double-spend of a
security primitive), audit chains require total order, reservations are
mutual exclusion — none of which CRDTs provide without the consensus layer
they exist to avoid. Append-only output segments could merge; nobody's
problem is merging output segments. This is a storage-engine rewrite with a
security regression at its center, solving tomorrow's hypothetical
bottleneck by dismantling today's actual guarantee. The distributed write
path that already exists (agents stream to one aggregator that owns the
write lock) is the correct shape; finish proving *that* at target class
first.

---

## Summary judgment

A high-variance list: #1 is excellent and closer to shippable than its
author knew (semantic zones already exist in the vendored term crate), #4
and #2 are good ideas needing mechanism corrections, and #3's product
concept deserves rescue from its actuator. But six of fifteen ideas (5, 7,
8, 11, 12, 15) fail against facts of this specific system — the passive-
first capture invariant, the redaction pipeline's text-only reach, macOS as
the primary platform, advisory-not-mediating Agent Mail, local-authority
distributed design, and single-writer-by-doctrine storage — and two of those
(7, 12) additionally rest on a false model of how agent CLIs consume
context. The list optimizes for "compelling in a keynote" over "correct in
this tree"; where those align (#1), it's the best idea either external list
produced.

*This file and WIZARD_SCORES_CC_ON_COD.md are the only files written by this
evaluation; no code or configuration was modified.*
