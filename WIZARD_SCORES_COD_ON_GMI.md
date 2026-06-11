# Codex Scores On WIZARD_IDEAS_GMI.md

Scoring scale: 0 is actively bad, 500 is interesting but not clearly worth the implementation burden, 750 is strong and probably worth doing, 900+ is exceptional. I weighted practical value for human operators and AI coding agents, fit with FrankenTerm's actual architecture, implementation tractability, and whether the utility justifies added complexity.

## 1. Terminal-Native DOM And Semantic Query API

**Score: 780 / 1000**

This is a good idea because raw terminal text is a brittle interface for agents, and FrankenTerm's internal terminal model can expose more structure than a generic mux. Semantic queries such as prompts, command regions, exit status, and unscrolled regions would materially improve agent reliability and token efficiency. The strongest argument against it is that "the state already exists in memory" is too optimistic: a real terminal semantic DOM requires hard boundaries around shell prompts, alternate screen apps, wrapped lines, partial commands, OSC markers, and agent-specific composers. I would pursue a narrow semantic-cell/zone API first, not a full DOM promise.

## 2. Automated Sludge Compaction Via BOCPD And Swarm Learning

**Score: 690 / 1000**

The practical pain is real: repetitive low-signal output wastes tokens and can degrade agent reasoning. BOCPD plus learning-index signatures could help detect repeated build/error regimes and present compact summaries to agents. The strongest argument against it is safety: compaction can hide exactly the novel line that matters, and "mathematical rigor" does not automatically distinguish low-signal repetition from important repeated failures. This should be opt-in, reversible, hash-linked, and always allow raw expansion; otherwise it risks making the terminal less truthful.

## 3. Semantic Breakpoints And The Intervention Console

**Score: 560 / 1000**

The goal is valid: catch dangerous agent behavior before it causes damage and route it to human intervention. Integrating pattern detections with pause/takeover/quarantine flows could be valuable, especially for high-risk missions. The strongest argument against it is the proposed mechanism: sending `SIGSTOP` to PTY child processes is blunt, platform-sensitive, and can corrupt or deadlock interactive programs in ways that policy-level prevention would avoid. This idea is safer if reframed as policy-gated mission pause, input quarantine, or tx safe mode rather than OS-level process suspension as the default.

## 4. Cross-Swarm Scent And Spatial Awareness

**Score: 725 / 1000**

Ambient awareness of sibling agents is very useful in shared repos, and the agent correlator plus reservations/events can support it. A compact "who is working where, with what ownership and risk" resource would reduce collisions and duplicated work. The strongest argument against it is vagueness: "scent" is evocative but underspecified, and agents will only benefit if the endpoint is structured, trusted, and tied into policy/ownership decisions. This overlaps heavily with attention routing and pane ownership, but the core concept is strong.

## 5. Ghost Panes Via Copy-On-Write Speculative Execution

**Score: 430 / 1000**

The ambition is understandable: safe speculative command execution with rollback would be extremely valuable for AI coding agents. In this repo, though, the proposed implementation is a cross-platform isolation project involving overlay filesystems, namespaces, hidden mux panes, diff review, merge semantics, and policy integration. The strongest argument against it is that it is effectively building a sandbox/workspace system inside a repo whose rules explicitly forbid worktrees and destructive filesystem ambiguity. The payoff is high, but the complexity and platform risk are too large for a pragmatic near-term FrankenTerm improvement.

## 6. Time-Travel Debugging - Replay-To-Live Handoff

**Score: 390 / 1000**

Interactive replay branching is compelling as a debugging dream. However, the proposed mechanism conflates terminal scrollback with process state: injecting historical scrollback into a new PTY does not recreate the filesystem, environment, process memory, sockets, job control state, or agent internal context. The strongest argument against it is that it may create an illusion of resuming from history while actually running in a synthetic and misleading state. Replay-to-simulation and replay-to-mission-rehearsal are much more defensible than "resume live execution from that state."

## 7. Corpus-Aware RAG Injection At The PTY Layer

**Score: 480 / 1000**

Surfacing relevant prior fixes when errors occur is a good goal, and FrankenTerm has search, CASS, events, and workflow hooks that can support it. The weak part is injecting RAG hints directly into hot scrollback before the prompt returns. The strongest argument against it is that it pollutes the agent's observed terminal reality, can become prompt-injection-like noise, and may confuse tools or humans expecting raw command output. This idea is much better as an attached event, attention item, or remediation candidate than as PTY-layer pseudo-output.

## 8. Cryptographic Provenance Tracking - Agentic SBOM

**Score: 540 / 1000**

Provenance for agent-written code is valuable for enterprise trust, and FrankenTerm's attestation machinery gives this idea a plausible foundation. The proposed version overstates the current mediation model: agents can modify files through ordinary shell commands, editors, and tools that FrankenTerm observes but does not necessarily intercept as tx file writes. The strongest argument against it is false completeness; an `.ft/provenance.jsonl` ledger is only meaningful if it can honestly say what it did and did not observe. A narrower receipt model for mission/tx-mediated changes would be useful, but a full agentic SBOM needs much more enforcement than the proposal admits.

## 9. Adversarial Consensus Engine - Multi-Model Debate Gate

**Score: 590 / 1000**

Independent review for high-risk operations can improve safety, especially when tied to policy gates and mission/tx approval. The idea is useful if scoped to "request an additional reviewer and attach critique" rather than "AI governs AI infallibly." The strongest argument against it is latency, cost, and reliability: spawning another model can fail, produce shallow objections, or add noise while the original tx is paused. This should be an optional high-risk approval strategy, not a central enforcement primitive.

## 10. Predictive Capacity Shedding Via WASM Substrate

**Score: 555 / 1000**

Predictive admission is a strong direction, and command analysis before local resource spikes is clearly useful. The WASM plugin angle is less convincing as the first implementation: most near-term value would come from a built-in command analyzer for known expensive commands and RCH routing, not arbitrary plugins. The strongest argument against it is that custom predictive models are hard to validate and can block good work or admit bad work with false authority. This belongs behind an adaptive governor and proof corpus, not as an early WASM use case.

## 11. Zero-Trust Distributed Mission Marketplace

**Score: 385 / 1000**

Federated bidding is a radical distributed-systems vision and would be powerful if FrankenTerm were already a mature multi-host scheduler. In the current codebase, it would require large changes to connector mesh, wire protocol, trust, cancellation propagation, mission assignment, state consistency, and failure semantics. The strongest argument against it is that it adds a distributed consensus and scheduling problem before the single-host control loop is fully productized. It is interesting as a long-term research direction, but not pragmatic enough for the current architecture.

## 12. Multimodal Terminal Emulation - Visual-AST Rendering

**Score: 300 / 1000**

This is creative, but it is the weakest GMI idea. Intercepting commands like `cat`, `bat`, or `tree` and replacing their output with rendered AST images changes terminal semantics, breaks expectations, and depends on vision-enabled agents interpreting screenshots correctly. The strongest argument against it is that it optimizes for a flashy modality while harming the text-first, machine-readable Robot/MCP contract discipline that makes FrankenTerm valuable. A separate `ft visualize` command could be useful; transparent terminal output substitution is not a good default.

## 13. Stochastic Chaos Monkey - Continuous Cx-Fuzzing

**Score: 610 / 1000**

Chaos testing for cancellation, timeouts, and channel pressure is valuable in a Cx-first runtime. FrankenTerm should absolutely have controlled stress modes for tx compensation, runtime cancellation, and workflow resilience. The strongest argument against the proposal as written is that injecting packet drops or arbitrary delays into `runtime_async` can violate primitive contracts and create nondeterministic failures that are hard to debug. This should be a deterministic lab/runtime harness with seeds and receipts, not a casual config mode that randomly perturbs production-like sessions.

## 14. Continuous Formal Verification Lane For Agent Txs

**Score: 500 / 1000**

Running existing Loom or formal harnesses when an agent touches concurrency code is a good idea. The broader proposal to generate TLA+ or Lean specs for arbitrary agent-produced code is not currently practical and risks becoming theater. The strongest argument against it is that formal verification only helps when the model is correct and maintained; auto-generating meaningful models for arbitrary refactors is close to an unsolved research problem. A narrower "proof-relevant file changed, run the existing formal lane" feature would score much higher.

## 15. CRDT-Backed Active-Active Mission State

**Score: 360 / 1000**

Active-active state sync could matter for a future distributed FrankenTerm, and append-only logs are more CRDT-friendly than mutable state. However, replacing the SQLite consistency model with CRDT-backed distributed merge semantics is an enormous storage and audit redesign. The strongest argument against it is correctness risk: policy decisions, audit ledgers, tx receipts, and mission state need strong ordering and trust semantics, not just eventual convergence. This is a possible far-future research project, but it is not justified before the existing single-writer architecture is proven at target scale.
