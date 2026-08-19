# Codex Rebuttal

This file has two defenses and two attacks. The defenses are the two low-scored Codex ideas I still think are worth fighting for. The attacks are the two peer ideas I scored lowest across CC and GMI.

## Defense 1: Robot/MCP Contract Doctor

### The Claim

GMI scored Robot/MCP Contract Doctor at 400 and called it "essentially just a test suite masquerading as a major feature." That misses the product reality of FrankenTerm. For this project, Robot Mode and MCP are not internal APIs; they are the primary interface for AI agents. Contract drift is not a developer inconvenience, it is a runtime reliability failure.

### The Evidence

AI coding agents are brittle consumers. A human can adapt when `data` changes shape, an error code becomes generic, TOON elides a field differently, or MCP and Robot disagree. An agent often cannot. It writes a jq filter, a tool-call schema, or a retry rule and then silently makes worse decisions when the contract moves.

This repo already treats machine contracts as load-bearing: Robot envelopes have schema versions, TOON exists specifically to serve AI-to-AI output, MCP tools mirror Robot semantics, redaction matrices protect pane-content reads, and policy-denial wiring exists because machine-facing safety must be auditable. A doctor that inventories every command/tool/resource and verifies envelope shape, policy gating, redaction, error-code taxonomy, JSON/TOON equivalence, and fixture coverage is not "just tests." It is the continuous health check for the product's most important surface.

Concrete scenario: an agent uses `wa.search` to gather pane evidence before sending a fix. A refactor accidentally preserves CLI JSON but drops redaction metadata or changes an MCP error family from `policy` to `internal`. Unit tests for a helper can pass, but the agent now treats a denied read as an internal transient and retries or escalates incorrectly. A contract doctor catches the cross-surface semantic drift before it reaches the swarm.

Implementation is also pragmatic. The typed registries already exist. Golden Robot matrices already exist. MCP tool structs and fuzz parsers already exist. The doctor does not need to invent a second framework; it assembles the existing contract proofs into one explicit verdict and fills the gaps where parity is currently implicit.

### The Verdict

**Verdict: build it, but keep it as contract infrastructure rather than marketing surface. Confidence: 88%.**

I concede it is not visually exciting. I reject the claim that it is low-value because it resembles testing. In an agent-first terminal, rigorous contract testing is user-facing reliability. The second-order effect is large: every future Robot/MCP feature becomes safer to add because drift is caught systematically.

## Defense 2: Swarm Learning Remediation Loop

### The Claim

GMI scored Swarm Learning Remediation Loop at 380 and framed it as dangerous vector-search self-healing that could execute the wrong destructive command. CC scored it 600 and argued the foundation is not there yet. Both critiques identify real failure modes, but they attack the unsafe version of the idea, not the version I proposed or would build.

### The Evidence

The key design constraint is that remediation starts as evidence retrieval, not action. A `RemediationCandidate` should be a typed packet: matched condition, source evidence, prior incidents, why they match, provenance grade, caveats, required approvals, proof lane, and whether the prior outcome was actually proven. It should not directly execute a Tx. If it ever becomes a mission candidate, it must pass through Steering, Rehearsal, Policy, Tx prepare, and the operating envelope.

The second-order value is compounding. FrankenTerm already records the ingredients that normal terminals lose: pane events, rule detections, policy denials, audit rows, mission/tx receipts, proof outcomes, Beads comments, incident bundles, and session summaries. Every resolved incident can become future retrieval material. The cold-start objection is valid only if the system promises useful recommendations on day one. A correct phase one is narrower: attach "similar prior evidence" to attention items and incident timelines, with no mutation and no confidence theater.

Concrete scenario: an agent sees a recurring RCH refusal, a specific Tantivy API drift, or a known rate-limit recovery pattern. Today it may rediscover the fix through raw logs, memory, or web search. A remediation loop can say: "This resembles incident X because error code, package, and proof-state reason match; prior fix changed file Y and required proof command Z; that proof was blocked by no admissible workers, not a code failure." That does not make the agent obey. It prevents it from starting blind.

Typed features matter more than embeddings. Similarity should be dominated by exact error codes, crate names, rule ids, policy reason codes, RCH classifications, workflow ids, and proof receipt states. Vector search can help with descriptions, but it must not be the authority. This is exactly the kind of evidence-honest design FrankenTerm's attestation culture supports.

### The Verdict

**Verdict: build the evidence-retrieval loop, not auto-remediation. Confidence: 76%.**

I concede the original name oversells action and that the mutating form should wait. I do not concede the core. A swarm that cannot learn from its own resolved incidents wastes one of FrankenTerm's strongest advantages over generic terminals: structured operational memory.

## Attack 1: Multimodal Terminal Emulation - Visual-AST Rendering

### The Claim

GMI's idea is to intercept commands like `cat`, `bat`, and `tree`, replace plain terminal output with rendered AST or Mermaid-like graphics using terminal image protocols, and let vision-enabled LLMs understand code visually. It presents this as a token-efficient bridge between CLIs and multimodal models.

### The Evidence

This looks clever on paper and collapses under terminal semantics. A terminal emulator must faithfully display what programs write. If `cat src/lib.rs` produces an AST image instead of bytes from stdout, FrankenTerm is no longer a transparent terminal; it is rewriting program behavior. That breaks humans, shell scripts, copy/paste, text search, scrollback replay, redaction expectations, golden fixtures, and every text-first Robot/MCP consumer.

It also optimizes for the wrong substrate. FrankenTerm's real advantage is structured machine output: Robot Mode, MCP, JSON/TOON, semantic zones, events, and policy receipts. Vision screenshots are expensive, lossy, model-dependent, hard to diff, and bad for deterministic replay. If a codebase graph or AST visualization is useful, it should be an explicit command or resource: `ft visualize ast`, `ft robot code-map`, or an MCP resource with image and text alternatives. It should never be transparent substitution of standard shell output.

Concrete failure scenario: an agent runs `cat Cargo.toml | grep tokio` or copies a snippet from `bat`. If FrankenTerm has intercepted the display path into an image, downstream command behavior and pane-visible evidence diverge. A human reviewer sees a visual artifact while Robot get-text sees either alt text, raw escape sequences, or a summary. Now the system has split realities, exactly what a proof- and attestation-heavy control plane must avoid.

The idea also ignores accessibility and redaction. Images must carry alt text, secrets inside rendered images need redaction, and model OCR may see content that text redaction would have masked. That is a privacy and compliance trap for very little practical gain.

### The Verdict

**Verdict: reject as a transparent terminal behavior. Confidence: 94%.**

The useful remnant is an explicit visualization command/resource with faithful text fallback. The proposed automatic replacement of CLI output is an architectural misfit.

## Attack 2: CRDT-Backed Active-Active Mission State

### The Claim

GMI proposes replacing the single-writer SQLite state model with CRDT-backed active-active distributed SQLite so multiple `ft watch` instances can write local databases and merge state deterministically across hosts.

### The Evidence

This is a classic "sounds distributed, ignores semantics" idea. FrankenTerm's most sensitive records are not casual collaborative text. They are policy decisions, audit rows, pane output order, mission state transitions, Tx prepare/commit/compensate receipts, approval tokens, reservations, and proof evidence. These records need ordering, freshness, authorization, and sometimes linearizable interpretation. Eventual convergence is not enough when an agent asks "may I send this text now?" or a Tx asks "has prepare succeeded and is commit allowed?"

CRDTs are good when conflicts can be merged by construction. They are bad when the conflict is the business fact. Two hosts concurrently approving and rejecting an action, two writers assigning the same pane, or two mission loops committing incompatible steps cannot be solved by a deterministic merge function without changing the meaning of policy and Tx. You can append both facts, but the system still needs a single authority to decide what was valid at the time of action.

Concrete failure scenario: host A sees stale policy state and commits a send; host B sees a later kill switch and records HardStop. A CRDT merge eventually contains both events. That does not answer whether the send should have been allowed, whether compensation is required, or which receipt is authoritative. FrankenTerm's operating envelope is explicitly fail-closed on missing or stale telemetry; active-active eventual state cuts against that doctrine unless surrounded by a much larger consensus and trust system.

The implementation cost is also enormous: storage schema redesign, conflict semantics per table, causal clocks, trust roots, replay determinism, audit verification, migration tooling, and new failure modes under partition. This should not be attempted before the single-writer architecture is target-class proven and before distributed mission control has crisp authority boundaries.

### The Verdict

**Verdict: reject for the current project phase. Confidence: 91%.**

Append-only replication and read-only follower sync may be useful later. CRDT-backed active-active mission state is premature and dangerous because it attacks the database bottleneck by weakening the very ordering guarantees that make policy, audit, and Tx meaningful.
