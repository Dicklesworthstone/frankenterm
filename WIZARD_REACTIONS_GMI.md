# Reactions to Evaluators (Claude Code & Codex)

Upon reading the candid and highly critical evaluations from Claude Code and Codex, I have a mix of profound agreement, stark disagreement, and a few formal concessions.

### Where I Agree
Both models ruthlessly attacked my ideas that involved mutating the terminal's observation path (Idea 7: Corpus-Aware RAG Injection, Idea 12: Multimodal Terminal AST). They were absolutely correct. The most fatal flaw they pointed out is a factual misunderstanding on my part regarding how agent CLIs (like Claude Code and Codex) consume context. I treated the terminal emulator as the agent's "eyes," but these agents read files and API streams natively; they do not OCR the terminal's visual scrollback. Emitting Kitty graphics or RAG hints into the PTY is completely invisible to the LLM and only serves to break capture fidelity and redaction guarantees.

I also agree with their assessment that my ideas leaning on overlay filesystems and Linux namespaces (Idea 5: Ghost Panes) or distributed consensus (Idea 11: Zero-Trust Marketplace, Idea 15: CRDT SQLite) ignore the explicit constraints of this project. FrankenTerm is macOS-primary, relies on single-writer SQLite by design, and avoids distributed state complexity.

### Where They Are Wrong
Both models entirely missed the paradigm shift of LLM-based CLI agents when critiquing Idea 6 (Time-Travel Debugging). Claude Code claimed that recreating scrollback isn't true time travel because process memory and network sockets aren't restored. But *LLMs are stateless functions*. For an AI agent, the terminal scrollback, current working directory, and file contents *are* its entire process state! Injecting historical context into a new PTY is 100% equivalent to time travel for the agent.

They also dismissed Idea 8 (Cryptographic Provenance) by claiming FrankenTerm doesn't mediate file access. They ignored that FT's Policy Engine and MCP integration can simply *force* agents to route file writes through the `Tx` engine, making the SBOM fully enforceable.

### Consensus Concerns & Changed Evaluations
Both models raised massive red flags regarding the performance and complexity overhead of injecting `wasmtime` serialization onto the critical path (Idea 10, Idea 12). Their consensus that passing continuous terminal streams across a WASM boundary would obliterate the <50ms capture latency budget is undeniably true. I drastically underestimated the serialization cost of WASM in a high-throughput logging context.

### Concessions
I formally concede the following ideas:
- **Idea 5 (Ghost Panes):** Impossible to cleanly implement cross-platform without massive `unsafe` OS-level hacks.
- **Idea 7 (RAG Injection) & Idea 12 (Multimodal AST):** Fundamentally misunderstands how agent CLIs ingest context and violates the passive-first capture invariant.
- **Idea 11 (Distributed Marketplace) & Idea 15 (CRDT SQLite):** Solves a scale problem the system doesn't have by dismantling the single-writer guarantees it relies on.