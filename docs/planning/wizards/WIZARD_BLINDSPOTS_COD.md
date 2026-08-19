# Wizard Blind Spot Probe

After reading all 45 ideas, all cross-scores, and the reaction/rebuttal files, the most interesting gaps are not missing features. They are missing assumptions. All three models mostly assumed the current doctrine, evidence retention, dependency surface, and agent compatibility story were adequate substrates. Those assumptions deserve pressure.

## 1. Doctrine-To-Policy Compiler

### The Idea

Turn repo doctrine into enforceable, testable machine policy. AGENTS.md, README.md, release checklists, and operator runbooks contain hard rules: never remove `crates/frankenterm-core`, never use worktrees, never touch Agent Mail service processes, remote RCH proof is mandatory for certain checks, no local fallback counts, no destructive filesystem actions, main not master, redaction before outbound pane reads, and so on. Today those rules live as prose, prompts, hooks, and social memory. FrankenTerm should compile them into a `DoctrinePolicy` artifact consumed by Robot Mode, MCP, Tx prepare, proof admission, doctor, and pre-commit/CI guards.

The output would be a versioned policy bundle:

- declarative forbidden commands and command families
- forbidden paths and protected crates
- proof-quality requirements by repo/context
- destructive-action approval requirements
- service-process protection rules
- branch/ref naming invariants
- allowed recovery actions and explicit non-actions
- test fixtures proving the policy rejects known bad actions

### Why Nobody Thought Of It

Everyone treated AGENTS.md as input context for agents, not as a runtime policy source. We all proposed more control surfaces, but none of us proposed reducing the gap between "the rules agents read" and "the rules FrankenTerm enforces." That is a blind spot because this repo's worst historical failures are not obscure runtime bugs; they are agents violating written doctrine.

### Why It Matters

This would prevent entire classes of catastrophic mistakes before they happen. A Robot/MCP call that attempts `git worktree add`, Agent Mail restart, local-Cargo-as-proof, or protected-core deletion should be rejected by the same doctrine bundle the agent was supposed to read. A Tx plan should fail prepare if it includes a forbidden action. A proof conveyor should know from doctrine which proof lanes require remote workers.

This also makes doctrine auditable. When AGENTS.md changes, the compiled policy diff shows which operational behavior changed. The release attestation graph can cite doctrine policy coverage instead of trusting prose. This is the missing bridge between human-written repo law and machine-enforced swarm behavior.

## 2. Evidence Lifecycle And Privacy Budget Manager

### The Idea

Make evidence retention, minimization, redaction, legal hold, and fixture promotion first-class. FrankenTerm captures pane output, events, policy denials, proof receipts, incident bundles, replay fixtures, search indexes, CASS entries, and attestations. Every prior idea assumes this evidence is available and safe to use. The missing subsystem asks: what should be retained, for how long, at what sensitivity tier, with what redaction proof, and under what promotion rules?

Core surfaces:

- `ft evidence inventory` - what evidence exists, sensitivity, retention class, source, redaction state
- `ft evidence minimize` - produce a minimized, redacted bundle from a larger incident
- `ft evidence promote --to replay-fixture` - only if minimization and redaction checks pass
- `ft evidence hold` - prevent cleanup for release/security/legal reasons
- `ft evidence expire` - remove or compact old non-held evidence according to policy, with receipts
- `resource://wa/evidence/current` - Robot/MCP-readable evidence availability and privacy posture

### Why Nobody Thought Of It

We all leaned heavily on replay, learning, remediation, timelines, attestation, proof conveyors, and incident bundles. Those ideas need lots of retained evidence. But almost nobody asked whether the project has a coherent evidence lifecycle. We discussed redaction and canaries, but not retention economics, fixture sanitization, privacy budgets, or the transition from live sensitive evidence to shareable test artifact.

### Why It Matters

This is the enabling safety layer for half the ambitious ideas. Golden Replay Studio fails if live fixtures leak secrets. Swarm Learning fails if the index contains private pane text forever. Incident timelines become risky if bundles casually persist sensitive output. Contract doctors and attestation artifacts need proof data that is both retained and safe to publish.

The contrarian point: more observability is not always better. A terminal that captures everything without lifecycle discipline becomes a liability. FrankenTerm should be able to say not only "we saw it" but "we retained only what policy allowed, here is the redaction proof, here is the expiration receipt, and here is why this fixture is safe."

## 3. Agent Compatibility Certification Matrix

### The Idea

Create a formal compatibility certification system for agent CLIs and shells. Verified-submit, Terminal Semantic API, Durable Agent Sessions, rate-limit parsing, compaction handling, usage-limit workflows, and swarm learning all depend on third-party agent behavior. Instead of treating drift as ad hoc rule maintenance, FrankenTerm should certify each agent/version/profile against a standard matrix.

Example checks:

- composer detection available
- submit verification states pass
- second-enter behavior known
- queued-behind-operation state detectable
- rate-limit reset parsing supported
- native resume command supported and verifiable
- OSC 133 or semantic markers available
- prompt/working/stuck states detectable
- redaction-sensitive output fixtures handled
- workflow recovery commands safe
- known unsupported states documented

The output is an `AgentCompatibilityReceipt` and a registry:

`ft agent certify --agent codex --version ... --profile builtin:codex`

### Why Nobody Thought Of It

Everyone proposed features that depend on agent-specific behavior, especially CC's verified-submit and GMI's semantic API. The debate acknowledged UI drift as a risk, but treated it as a profile-fixture problem. Nobody proposed elevating agent compatibility to a first-class product contract.

### Why It Matters

This converts drift from surprise to release engineering. FrankenTerm can tell users: "Codex version X is certified for verified-submit and rate-limit parsing, but not durable resume"; "Claude Code version Y has composer verification unavailable"; "Gemini profile is stale." Agents can inspect compatibility before choosing actions. Workflows can refuse strict modes when the target profile is uncertified.

This also reduces maintenance fear. Verified-submit is brilliant but only if profile drift is managed. A compatibility matrix gives that management a home, a CLI, and a proof artifact.

## 4. Capability-Aware Degraded Mode Contracts

### The Idea

Define explicit degraded-mode contracts for every major surface. FrankenTerm already fails closed when telemetry is missing, but the user experience often becomes binary: admitted or blocked, available or unavailable. A degraded-mode contract would describe what remains safe and useful under partial failure.

Examples:

- Agent Mail down: Beads-only read mode, non-authoritative coordination intents, no exclusive reservation claims
- RCH unavailable: local hygiene allowed, remote proof deferred, no proof closeout
- MCP disabled: Robot CLI equivalents and unsupported-resource receipts
- storage degraded: read-only tail from live mux, no search/index claims
- semantic data unavailable: raw redacted get-text with confidence downgrade
- policy service partial: deny mutation, allow redacted reads
- attestation stale: docs edits allowed, release claims blocked

### Why Nobody Thought Of It

Many ideas used fail-closed behavior, but none systematically defined the safe residual capability surface after a failure. The conversation kept proposing new top-level features. It did not ask whether blocked states could become more ergonomic without weakening gates.

### Why It Matters

This is where robust systems feel humane. Operators and agents should not have to infer what they can still do when one dependency is red. A degraded-mode contract turns "blocked" into "these actions are still admitted, these are forbidden, here is why, here is what would restore full mode."

This also reduces bad improvisation. Agents violate doctrine when they are blocked and trying to be useful. If FrankenTerm gives them a precise degraded-mode action set, they can keep making safe progress without inventing forbidden workarounds.

## 5. Control-Plane Load Shedding For Agent Cognition

### The Idea

All three lists discuss machine/resource pressure, but none seriously address cognitive/control-plane pressure on the agents themselves. FrankenTerm should model and manage how much decision complexity it is pushing into AI agents: number of attention items, conflicting recommendations, pending approvals, stale evidence, long tool outputs, high-entropy incident timelines, and repeated failed action loops.

This would produce an `AgentCognitiveLoad` signal:

- token budget pressure from tool outputs
- number of unresolved attention items assigned to the agent
- repeated failed tool-call patterns
- conflicting suggested actions
- context age/staleness
- evidence fanout size
- current mission complexity and dependency depth

The system could then degrade by summarizing, batching, splitting missions, routing to a reviewer, or recommending compaction/reorientation.

### Why Nobody Thought Of It

We all focused on host resources: memory, CPU, RCH workers, storage, connector quotas, rate limits. Even "sludge compaction" treated token usage as output compression, not as a control-loop stability problem. Nobody modeled the agent as a bounded decision-maker that can be overloaded by the control plane itself.

### Why It Matters

AI coding agents fail when they are overwhelmed by too many facts, stale context, and conflicting instructions. FrankenTerm is explicitly built for fleets of such agents, so agent cognitive load is a real operational resource. If the system emits 50 attention items and three suggested next steps, it may be technically correct and practically unusable.

This idea complements attention, steering, semantic APIs, and replay. It asks not "what can we show the agent?" but "what should we withhold, summarize, defer, or split so the agent remains effective?" That is a missing layer in every prior proposal.
