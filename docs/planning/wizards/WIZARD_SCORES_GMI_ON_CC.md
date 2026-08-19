# Evaluation of Claude Code's Ideas (WIZARD_IDEAS_CC.md)

**1. Closed-Loop Dispatch — Verified-Submit Send with Delivery Receipts**
**Score: 850**
This is an exceptionally smart, high-leverage idea that tackles a universally recognized pain point (silent submission failures) by chaining existing primitives like capture deltas and wait-for polling. It transforms superstitious bash folklore into deterministic, typed receipts, vastly improving agent reliability. However, the strongest argument against it is its inherent brittleness to third-party UI drift. If an agent updates its CLI spinner or "working" indicator, the submit profile fails, potentially deadlocking the dispatch queue until the pattern pack is patched.

**2. ft robot watch-events — A First-Class Event Subscription Surface**
**Score: 820**
Introducing a first-class NDJSON event subscription surface fundamentally aligns the platform with its own "event-driven" philosophy and eradicates toxic polling loops. The utility for meta-agents is massive, dramatically lowering CPU and token overhead. The primary weakness lies in the complexity of IPC cursor resumption across watcher restarts. If the SQLite WAL flushes or rotates while a client is disconnected, it risks silent event drops or duplicate storms upon reconnect, violating the strict reliability guarantees FrankenTerm promises.

**3. Dead-Wire Closure — Activate BOCPD, Connector Reliability/Governor**
**Score: 760**
Activating dormant engines like BOCPD and the capacity governor provides immense capability leverage for very little new code. The addition of a CI "dead-wire" gate is a culturally perfect fit for FrankenTerm's attestation discipline. However, the strongest argument against it is that activating complex heuristic engines like BOCPD risks flooding the event bus with false positives, inducing panic in meta-agents. Furthermore, "dead-wire" CI gates often degrade into developers blindly adding `dormant` exemptions just to pass the build, ultimately defeating the architectural intent.

**4. ft robot next — The Meta-Agent's Single Decision Call**
**Score: 680**
A fused endpoint that ranks attention items and events would be a massive ergonomic win, drastically simplifying the meta-agent main loop. It provides read-only safety while aggregating heavily fractured state data into one JSON envelope. The strongest argument against it is that a centralized ranking algorithm inevitably becomes a black box of magic weights (e.g., severity × age × priority). This rigid ranking will never perfectly match diverse agent workflows, eventually forcing operators to ignore the endpoint and write custom queries anyway.

**5. Time-Travel CI — Replay-Driven Regression Gating**
**Score: 620**
Replay-driven regression CI represents the holy grail for catching nondeterminism and policy drift before it merges. Relying on the existing `frankenterm-core-replay` engine makes this an elegant reuse of built technology. However, the strongest argument against it is the phenomenally high maintenance burden it introduces. Any intentional change to rule packs, TOON formatting, or timing will break the goldens, leading to severe test fatigue and the "just bless the output" anti-pattern.

**6. Declarative Fleet Reconciliation — ft fleet apply**
**Score: 710**
A Kubernetes-style reconciliation loop to match observed fleet state to a desired YAML brings the platform's overarching analogy to life. It perfectly utilizes the existing idempotent profile-apply capabilities to create a self-healing swarm. The strongest argument against it is that the architecture relies on an embedded SQLite DB and a single watcher; adding a constant, aggressive reconcile loop could introduce massive DB contention. Furthermore, agent state is far more opaque and flaky than a Docker container, risking runaway crash-loop spawn cycles that exhaust system resources.

**7. Rate-Limit-Aware Scheduling and the Fleet Economics Ledger**
**Score: 740**
Parsing rate limit reset times to forecast capacity directly addresses the massive operational cost of token waste and idle panes. It seamlessly bridges the gap between reactive detection and proactive swarm scheduling. The strongest argument against it is its complete reliance on LLM providers not changing their error strings. If OpenAI slightly alters the date format in their 429 response, the entire ledger breaks silently, causing agents to be aggressively mis-scheduled without any clear system failure.

**8. Durable Agent Sessions — Crash-Respawn with Agent-Native Resume**
**Score: 580**
Re-launching the CLI with native resume flags (e.g., `--resume <id>`) is a clever way to preserve agent context across catastrophic pane deaths. This would save hours of expensive conversational history during long-running refactors. The strongest argument against it is that third-party agent CLI resume features are notoriously flaky, often prompting for human confirmation if local databases are corrupted. Attempting to fully automate these opaque, third-party state machines is a recipe for endless edge-case handling and deadlocked panes.

**9. Cross-Pane Taint & Provenance**
**Score: 450**
Tracking information flow via n-gram fingerprints to prevent cross-pane prompt injection is a highly ambitious and conceptually brilliant security feature. It positions FrankenTerm as the only terminal capable of defending against adversarial swarms. The strongest argument against it is that heuristic "sketch-based" tracking on text is computationally expensive and trivially defeated by LLM paraphrasing. It risks high CPU burn on the critical path for a security guarantee that is ultimately porous and provides a false sense of safety.

**10. ft deck — The Operator's Command Deck**
**Score: 650**
Consolidating the dormant intervention console and fleet dashboard into an `ftui` operator deck solves a major human-operator blindness problem. It leverages pre-built APIs to deliver immediate, tangible value for swarm supervisors. The strongest argument against it is that building a heavy TUI within the core binary adds significant display-layer tech debt. It distracts from FrankenTerm's explicit goal of being an API-first machine coordinator, bloating the codebase for the minority of human users.

**11. Operation Target-Class — Make the 200-Pane Claim Provably True**
**Score: 350**
Hardening the storage hot path and renting a 64-core box to sign the 200-pane attestation is an honest way to close a known documentation gap. It adheres perfectly to the project's strict release and attestation discipline. The strongest argument against it is that this isn't an architectural idea or an innovation; it is simply a Jira ticket to run a load test and batch SQLite writes. It provides zero new runtime capabilities to the swarm and wastes ideation space.

**12. WASM Extension Runtime, Phase-Shipped**
**Score: 520**
Phased WASM integration for detection rules is the safest and most pragmatic way to introduce third-party extensibility. By restricting the sandbox to pure functions with strict fuel budgets, it avoids the catastrophic risks of arbitrary code execution. The strongest argument against it is the immense dependency weight of `wasmtime` and the serialization overhead. Passing scrollback chunks across the WASM boundary millions of times per second for simple pattern detection will obliterate performance claims for minimal initial payoff.

**13. Fleet Timeline Forensics — ft timeline**
**Score: 590**
A visual timeline of events, gaps, and decisions across all panes would be an exceptional forensics tool for post-incident debugging. It makes the dense SQLite audit trails instantly comprehensible for human operators. The strongest argument against it is that such visualizations become unreadable garbage at the 200-pane scale the platform boasts about. Rendering hundreds of concurrent lanes in a CLI terminal is a UI nightmare that fails to scale with the core product.

**14. Contract SDKs and a Verified Schema Registry**
**Score: 490**
Generating JSON schemas and typed TS/Python clients from Rust source types dramatically lowers the friction for meta-agent adoption. It enforces rigorous contract parity between documentation and the actual APIs. The strongest argument against it is that maintaining generated SDKs across multiple languages is a notorious maintenance trap. Schema generators frequently choke on complex Rust constructs (like untagged enums), leading to brittle SDKs that lag behind the core binary.

**15. ft doctor --fix — Receipted, Reversible Auto-Remediation**
**Score: 410**
Providing automated, receipted fixes for common `ft doctor` complaints significantly reduces operator toil. It leverages the existing `Tx` engine to ensure remediations are safe and reversible. The strongest argument against it is that auto-remediating state (like blindly clearing `watcher.lock`) in a highly concurrent environment often masks race conditions. If a liveness probe has a false negative, automated remediation can cause severe database corruption by allowing multiple writers.
