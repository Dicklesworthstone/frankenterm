# Wizard Ideas - Codex Pass

Date: 2026-06-07
Scope: idea generation only. No code changes were made for these proposals.

## Frame

FrankenTerm already has the hard parts of a serious swarm control plane: a passive capture loop, typed Robot Mode and MCP surfaces, fail-closed operating-envelope planning, Mission and Tx contracts, policy-gated mutation, idempotency receipts, structured runtime/capacity telemetry, release attestations, and several built-but-dormant subsystems. The biggest improvement opportunity is not to add random features. It is to connect the existing pieces into fewer, clearer operator and agent workflows.

I evaluated 30 ideas against robustness, reliability, performance, intuitiveness, user-friendliness, ergonomics, usefulness, compellingness, accretive value, and pragmatism. I weighted usefulness and pragmatism most heavily. The best ideas share a pattern: they make the system more self-explaining and self-correcting without bypassing the fail-closed discipline.

## The 30 Ideas Considered

1. **Swarm Steering Loop** - one objective-to-execution control loop that compiles an operator goal into an operating-envelope-admitted mission, prepares a Tx, supervises execution, compensates on failure, and records learning.
2. **Deferred Proof Conveyor** - a durable queue that replays RCH-blocked proof lanes when workers recover, classifies proof quality, and wires results into Beads and attestations.
3. **Unified Attention and Intervention Console** - a single live cockpit for stalled panes, approval queues, reservations, operating-envelope status, kill switches, and operator runbook actions.
4. **Adaptive Governor Mesh** - unify operating envelope, capacity governor, connector reliability/governor, BOCPD anomaly signals, cost/quota budgets, and fleet memory into one predictive admission layer.
5. **Policy-Safe Extension Workbench** - wire the dormant WASM type substrate into a replay-tested extension SDK for detection rules, search rewriters, and workflow handlers.
6. **Swarm Learning Remediation Loop** - make CASS/swarm-learning index first-class in error handling: detect a failure, retrieve prior fixes, propose a mission, and verify it.
7. **Robot/MCP Contract Doctor** - a command that checks every Robot and MCP envelope for schema drift, policy coverage, redaction coverage, and golden fixture parity.
8. **Mission Rehearsal Scorer** - score candidate missions in dry-run against historical failures, operating-envelope constraints, policy gates, and replay fixtures before execution.
9. **RCH Admission Explainer** - before an agent starts cargo proof, explain exactly why RCH will accept, defer, or refuse the lane.
10. **Agent Mail Outage Spool** - durable coordination outbox for when Agent Mail is red, with replay receipts when service recovers.
11. **Attestation Graph UI** - local UI/resource that shows every claim, artifact hash, producer bead, retraction, freshness, and release-blocking gap.
12. **Operator First-Run Guided Tour** - an interactive `ft quickstart` that verifies capture, redaction, Robot Mode, MCP, policy, and a read-only mission.
13. **Pane Ownership Firewall** - reservations plus attention router plus policy denial explanations to prevent agents from stepping on each other.
14. **Incident Bundle Timeline Explorer** - turn incident bundles into a causality timeline with panes, resource pressure, policy denials, events, and receipts.
15. **Golden Replay Studio** - record a pane/fleet incident once and replay it against new patterns, workflows, policies, and MCP contracts.
16. **Connector Dispatch Graduation** - finish wiring connector reliability/governor into production dispatch with circuit breakers, quota budgets, and dead-letter queues.
17. **Capacity-Aware Work Picker** - combine Beads, operating envelope, RCH, dirty-tree risk, and skill requirements to recommend the next safe task.
18. **Policy Explainability Everywhere** - every denial or approval request includes the exact policy rule, evidence, remediation, and safe alternatives.
19. **Typed Runbook Compiler** - convert operator runbooks into machine-readable checklists that Robot/MCP can render and execute in read-only or approved modes.
20. **Fleet Health SLO Dashboard** - target-class dashboard for capture lag, input-to-photon, memory pressure, RCH, storage, and proof freshness.
21. **BOCPD-Driven Anomaly Narratives** - turn BOCPD change points into operator-readable "what changed" stories with linked evidence.
22. **Search Quality Feedback Loop** - capture accepted/rejected search results and use them to improve hybrid/CASS ranking with attested evaluation.
23. **MCP Safety Profiles** - named capability bundles for "observer", "operator", "mission planner", and "tx executor" roles.
24. **Workflow Handler Lab** - no-code-ish authoring and replay validation for workflow handlers before they are enabled in live fleets.
25. **Crash-Safe Scrollback Recovery Assistant** - guided recovery for scrollback and storage corruption, including orphan lock probes and bundle export.
26. **Resource-Aware UI Rendering Governor** - feed GUI renderer SLO signals into the fleet memory/capacity model to degrade gracefully.
27. **Distributed Swarm Trace IDs** - end-to-end trace IDs across pane capture, events, workflows, policy, Tx, MCP, and Beads comments.
28. **Human-Friendly TOON Inspector** - ergonomic viewer for TOON/JSON robot outputs, with schema-aware diffs and copyable commands.
29. **Agent Capability Cards** - live inventory of agent types, accounts, limits, current task, prompt state, and safe actions.
30. **Self-Hosted Release Readiness Gate** - one local command that says whether the repo is releasable, proof-complete, attested, and operator-runbook current.

## Winnowing

Several cut ideas are still good. The reason they did not make the top 5 is usually that they are narrower instances of a better umbrella idea. For example, the RCH admission explainer, mission rehearsal scorer, attestation graph UI, and release readiness gate are all valuable, but the Deferred Proof Conveyor gives them a stronger home. Similarly, connector dispatch graduation is important, but the Adaptive Governor Mesh captures the full value by tying connector safety to fleet admission, anomaly detection, and operating-envelope budgets.

The top 5 are the ideas I would bet on because they convert existing technical depth into visible product power. They also fit the repo's doctrine: side-effect-free planning first, policy gates before mutation, Cx-aware runtime boundaries, redaction and audit everywhere, and remote proof/attestation as the source of truth.

## 1. Swarm Steering Loop

**Idea:** Add a first-class "steering" workflow that turns an operator or AI-agent objective into a supervised, explainable, capacity-aware mission lifecycle:

`objective -> current context -> operating envelope -> candidate plan -> rehearsal score -> policy/approval plan -> Tx prepare -> Tx commit -> live monitor -> compensate or complete -> learning/attestation receipts`

This would surface as a human CLI, a Robot Mode family, and an MCP tool/resource set, for example:

- `ft steer plan --objective "..."`
- `ft steer run --plan .ft/mission/steer-...json`
- `ft robot steer plan --format toon ...`
- `wa.steer_plan`, `wa.steer_run`, `wa.steer_status`, `resource://wa/steer/current`

The important point is that `steer plan` is read-only. It consumes Beads, dirty-tree state, pane state, resource pressure, RCH availability, policy rules, and optional user constraints. It emits a deterministic plan with reason codes, denied alternatives, required approvals, expected proof lanes, expected resource cost, and rollback strategy. `steer run` then revalidates everything against the live operating envelope before each mutation phase and delegates execution to the Tx engine.

**How users and agents would perceive it:** This would make FrankenTerm feel much less like a large toolbox and much more like a control plane. A human could say "get this bead to a remotely proven closeout" and receive a concrete, auditable sequence. An AI agent could use the same surface to ask "what is safe to do next?" instead of manually stitching together Beads, RCH, policy, Robot Mode, and git status. The user experience becomes: one command explains the safe path, then one command executes it with supervision.

For agents, this is especially powerful because it reduces ambiguity. Agents fail most often when they improvise around invisible constraints: dirty files, unavailable proof workers, forbidden Agent Mail repairs, local Cargo proof that does not count, or policy gates. A steering loop would make those constraints explicit before work begins.

**How it would work technically:** The implementation can be staged without a risky rewrite:

1. Introduce a `SteeringPlan` type that references existing Mission, MissionTxContract, operating-envelope, policy, and rehearsal-score data instead of replacing them.
2. Build `ft steer plan` as a side-effect-free wrapper around existing source adapters: operating envelope, mission objective planner, Beads snapshot, git dirty state, reservations, RCH proof availability, policy preflight, and optional CASS/swarm-learning hints.
3. Add golden fixtures for common scenarios: clean ready bead, dirty overlap, RCH blocked, approval required, capacity red, Agent Mail red, existing active owner, and target-class proof missing.
4. Wire `ft steer run` as a thin executor that calls Tx prepare/commit/compensate and subscribes to events. It should not create a parallel execution model.
5. Publish Robot/MCP parity only after CLI fixtures stabilize, with schema tests and redaction tests.
6. Feed completed steering sessions into the swarm learning index and incident/rehearsal corpus.

**Why it is obviously better:** This is the highest leverage improvement because it turns many already-built systems into one user-facing capability. The operating envelope becomes more than a status artifact. Mission planning becomes more than a JSON contract. Tx becomes more than an advanced primitive. Policy denials become actionable plan branches. Robot/MCP become safer for autonomous agents because they get a single high-level path instead of many low-level footguns.

**Why I am confident:** The architecture already points here. The mission objective planner is side-effect-free; Tx already has prepare/commit/compensate and idempotency; Robot/MCP already expose mission, tx, policy, events, and operating envelope surfaces; the README already teaches this mental model. The missing piece is the productized loop. This is ambitious, but it is accretive because it mostly composes existing contracts.

## 2. Deferred Proof Conveyor

**Idea:** Build a durable proof-replay conveyor for all work that lands source changes but cannot obtain terminal remote proof because RCH is unavailable, overloaded, or contradictory. The system would queue proof intents, replay them when the operating envelope admits proof work, classify the result, attach receipts to Beads, and update release-attestation readiness when appropriate.

This should be more than "retry cargo later." It should understand proof quality:

- remote worker reached vs local fallback
- package-scoped vs workspace-wide proof
- test, check, clippy, fmt, schema, fuzz, replay, or attestation verification
- stale source hash vs current tree
- valid proof artifact vs terminal infra blocker
- release-blocking vs supplemental evidence

**How users and agents would perceive it:** Today, proof-blocked work creates cognitive debt. An agent can do the right code work, but if RCH has no admissible workers, the final state is awkward: code may be committed, the bead may be blocked, and a future agent has to reconstruct what proof was intended. A proof conveyor would make that state normal and tractable. Users would see "source landed, proof queued, remote worker unavailable, replay will resume under these conditions" rather than a vague blocker.

Agents would stop wasting time polling or accidentally treating local Cargo as proof. They would submit a proof intent and move on to permitted work. When remote proof recovers, the conveyor would replay the exact command with the correct target dir and environment, compare the source hash, and record the terminal result.

**How it would work technically:**

1. Define a `ProofIntent` schema with command, scope, source hash, expected artifact path, required remote semantics, bead id, attestation slot if any, and redaction policy.
2. Add `ft proof queue`, `ft proof status`, `ft proof replay`, and `ft proof attach` commands. Robot/MCP can expose read-only status first.
3. Feed admission through the operating envelope. If RCH proof state is unavailable, the conveyor records a deferred receipt instead of attempting local fallback.
4. Store immutable proof receipts with classification: pass, fail, infra-blocked, stale-source, inadmissible, contradiction, or retracted.
5. Integrate with Beads comments and release attestation only through explicit receipt-producing steps. No automatic closeout without a terminal valid proof.
6. Add a corpus of synthetic RCH outcomes: no workers, local fallback attempted, worker reached/pass, worker reached/fail, stale command, command typo, contradictory health, and missing artifact.

**Why it is obviously better:** FrankenTerm's development workflow is currently constrained by remote proof availability. This idea converts that weakness into a disciplined queue. It improves reliability by preserving exact intent, improves ergonomics by reducing repeated proof archaeology, improves robustness by preventing local proof substitution, and improves release quality by making proof gaps machine-visible.

**Why I am confident:** This fits the existing doctrine perfectly. The repo already distinguishes local hygiene from remote proof, already has an operating-envelope proof state, already has attestation slots, and already has many blocked beads whose notes describe source landed but RCH unavailable. The implementation can begin as metadata and CLI status before any automation runs commands.

## 3. Unified Attention and Intervention Console

**Idea:** Build a single operator/agent attention surface that combines the dormant intervention console, fleet dashboard, reservations, approval queues, policy denials, stalled-work ledger, operating-envelope status, and operator runbook hints. The goal is to answer one question immediately:

"What needs human or agent attention right now, what is safe to do about it, and who owns it?"

This should exist in three forms:

- a compact CLI: `ft attention current`
- a Robot/MCP resource for agents: `resource://wa/attention/current`
- an optional TUI/web console for human operators

The console should not be a decorative dashboard. It should be an action queue with typed items: approval required, pane stalled, policy denied, RCH blocked, envelope red, dirty overlap, reservation conflict, connector circuit open, mission paused, Tx compensation needed, proof stale, attestation gap, auth required, or incident bundle available.

**How users and agents would perceive it:** Operators should stop needing to inspect ten commands to decide what is going on. Agents should stop inferring ownership from partial pane text or stale comments. A good attention console would make the fleet feel calmer: every interruption has a reason, severity, owner, safe next action, and "do not do this" guardrail.

For AI agents, this can become the default starting point. Instead of "look around and guess," an agent can read attention items sorted by severity and admitted action class. That creates a shared language between humans and agents.

**How it would work technically:**

1. Define an `AttentionItem` schema with stable id, severity, domain, owner, source evidence, redacted summary, recommended actions, forbidden actions, freshness, and related resources.
2. Build adapters from existing sources: policy denial audit, approval store, pane reservations, operating envelope, mission/tx status, events, Beads snapshot, RCH proof conveyor, connector breaker state, and incident bundles.
3. Start read-only. The first version should simply aggregate and rank attention items.
4. Add intervention commands after read-only trust is established: pause pane, resume pane, request approval, reject approval, quarantine pane, release reservation, trigger incident bundle, or switch mission kill level.
5. Every intervention must go through policy and Tx where appropriate. The console must not become a side door around existing gates.
6. Add golden scenarios so the ranking is deterministic and agents can rely on it.

**Why it is obviously better:** This would directly improve intuitiveness and user-friendliness. FrankenTerm's strongest technical concepts are currently spread across commands and docs. A unified attention surface would make the system self-explanatory under stress, which is exactly when operators and agents need help most.

**Why I am confident:** Many ingredients already exist: intervention console types, fleet dashboard structures, approval and reservation surfaces, policy audit, operating envelope resources, mission/tx lifecycle, runbooks, and Robot/MCP resources. The pragmatic path is read-only aggregation first, then policy-gated intervention.

## 4. Adaptive Governor Mesh

**Idea:** Turn the operating envelope from a mostly admission-time planner into the front door of an adaptive governor mesh. This mesh would combine capacity governor signals, fleet memory pressure, RCH availability, connector reliability/governor state, BOCPD anomaly detection, storage pressure, network pressure, and policy kill switches into a single budget model for work admission.

The mesh would govern not only pane spawns and proof lanes, but also connector calls, workflow concurrency, search/replay intensity, background indexing, WASM extension fuel, and optional renderer work.

**How users and agents would perceive it:** The system would feel less brittle under load. Instead of a swarm suddenly hitting rate limits, memory pressure, storage stalls, or connector failures, FrankenTerm would progressively degrade: fewer optional workflows, lower indexing priority, connector backoff, proof deferral, search fast-mode, or mission safe mode. Users would see clear reasons, not random slowness.

Agents would get explicit capacity budgets. A robot planner could ask "can I run this action now?" and receive not only yes/no but admitted concurrency, retry-after, degraded mode, and proof expectations.

**How it would work technically:**

1. Define a common `BudgetVerdict` abstraction that can represent allow, throttle, defer, shed, block, retry-after, and degraded-mode hints.
2. Adapt existing sources into that abstraction: operating envelope, capacity governor, connector governor, connector circuit breakers, BOCPD change points, fleet memory, RCH, storage writer depth, network/process snapshots, and policy kill switches.
3. Start with read-only budget explanations exposed via CLI/Robot/MCP.
4. Wire enforcement in narrow places first: connector dispatch, workflow concurrency, background search/indexing, and proof conveyor admission.
5. Add hysteresis and cooldown rules so budgets do not flap.
6. Add attested target-class artifacts only after retained proof shows stable behavior under load.

**Why it is obviously better:** It improves robustness, reliability, and performance at the same time. The current architecture already treats pressure as first-class, but several subsystems are isolated. The mesh would prevent local optimization from harming fleet health. Connector reliability becomes part of mission safety. BOCPD becomes an early-warning signal rather than just telemetry. Capacity governance becomes visible to Robot/MCP.

**Why I am confident:** This is a natural extension of the fail-closed operating-envelope philosophy. The main implementation risk is overreach, so the first phase should be explanatory and advisory. Once the budget model is trusted, enforcement can be attached subsystem by subsystem.

## 5. Policy-Safe Extension Workbench

**Idea:** Activate the WASM extension type substrate through a tightly controlled extension workbench. The goal is not "plugins can do anything." The goal is "operators can safely add custom detection, search rewriting, and workflow logic, and prove it against replay before it touches live panes."

The workbench would include:

- a minimal WASM host with fuel, memory, timeout, capability, and policy gates
- a manifest format tied to existing extension capability types
- replay-first validation against golden incidents and pane traces
- redaction and policy tests for every extension
- signed extension bundles with attestation hooks
- a local development loop: `ft extension test`, `ft extension replay`, `ft extension explain`, `ft extension install --disabled`

**How users and agents would perceive it:** Advanced users would see FrankenTerm as extensible without being fragile. They could encode local agent patterns, company-specific error detectors, custom connector workflows, or search rewrite logic without patching core Rust. AI agents could propose extensions as artifacts, but the system would force them through replay, fuel, redaction, and policy checks before activation.

This would also make FrankenTerm more compelling as a platform. A swarm terminal is most powerful when it can learn new local operations without every customization becoming a core-code change.

**How it would work technically:**

1. Keep the first capability set tiny: read redacted event metadata, emit detection labels, rewrite search queries, and propose workflow metadata. No live pane mutation at first.
2. Reuse the existing extension capability and sandbox policy types. Add an executable host only behind a feature gate.
3. Require every extension to pass replay fixtures before it can be enabled.
4. Use the adaptive governor mesh for fuel/concurrency budgets.
5. Route all extension outputs through policy and redaction. If an extension proposes a mutating action, it must become a Mission/Tx candidate, not a direct send.
6. Add signed bundles and attestation slot support once the runtime is stable.

**Why it is obviously better:** It opens a large customization surface while preserving the core safety model. It also reduces pressure to keep adding one-off built-in handlers for every agent, connector, and workflow variant.

**Why it ranks fifth:** This is highly compelling but less immediately pragmatic than the first four. WASM execution and extension security are inherently tricky. The reason it still belongs in the top 5 is that FrankenTerm already has the right substrate: typed capabilities, policy gates, replay, redaction, attestation discipline, and a strong runtime model. The first phase can be deliberately small and non-mutating.

## Recommended Build Order

1. Start with the **Unified Attention and Intervention Console** read-only schema because it gives every later system a common status surface.
2. Build the **Deferred Proof Conveyor** metadata and read-only status next because it addresses a current daily bottleneck and strengthens release discipline.
3. Layer the **Swarm Steering Loop** on top of attention plus proof status, beginning with side-effect-free planning only.
4. Add **Adaptive Governor Mesh** advisory verdicts, then enforce them in connector dispatch and background work.
5. Activate the **Policy-Safe Extension Workbench** only after replay and budget controls are strong enough to keep extension behavior bounded.

If only one idea is pursued, choose the **Swarm Steering Loop**. It is the clearest path from FrankenTerm as a powerful system to FrankenTerm as an indispensable system.

## Next Best 10 Ideas

These are ideas 6 through 15 from the same winnowing pass. They are not "lesser versions" of the top 5. They are complementary angles: some make the top ideas easier to trust, some make FrankenTerm easier to adopt, and some are more radical bets that should wait until the core steering/proof/attention loop is stronger.

## 6. Swarm Learning Remediation Loop

**Idea:** Promote the existing CASS and `HandleSwarmLearningIndex` substrate into a closed-loop remediation system. When FrankenTerm detects an error, stuck state, quota problem, proof failure, crash, policy denial, or recurring workflow stall, it should search prior swarm history, retrieve the most similar successful fixes, and propose a policy-safe mission or runbook step.

This is not just "search old logs." It should produce a typed remediation packet:

- detected condition and source evidence
- nearest prior incidents and why they match
- candidate fix path, with confidence and known caveats
- required approvals and forbidden shortcuts
- suggested proof lane
- rollback/compensation strategy if the fix mutates state
- whether the recommendation is based on a proven historical outcome or only a partial analogy

**How users and agents would perceive it:** Operators would experience the system as remembering what worked. An agent seeing a familiar failure would no longer need to rediscover the same command, caveat, proof blocker, or runbook note from scratch. The UI effect should be modest but powerful: next to an error or attention item, FrankenTerm says, "This looks like three prior incidents; the successful one used this plan, and here is the proof receipt."

AI agents would perceive it as a context oracle with guardrails. Instead of asking a model to infer from raw pane text, the agent receives a structured, redacted, source-linked remediation candidate. That should reduce hallucinated fixes and reduce repeated mistakes.

**How it would work technically:**

1. Define a `RemediationCandidate` schema with evidence, match features, retrieved examples, plan skeleton, proof requirements, and confidence.
2. Feed the index from completed missions, Tx receipts, proof conveyor receipts, incident bundles, policy denials, and workflow outcomes.
3. Build retrieval around typed features first: error codes, command family, crate/package, pane agent type, operating-envelope reason codes, policy reason codes, RCH classification, and workflow handler id.
4. Use semantic search only as a secondary signal. The system should not trust vector similarity more than typed evidence.
5. Expose the first version as read-only: `ft remediate suggest --event <id>` and MCP/Robot equivalents.
6. Only later allow "turn suggestion into mission," and even then through the Swarm Steering Loop and Tx prepare path.

**Why it would make FrankenTerm better:** This turns the fleet's history into operational leverage. FrankenTerm already captures rich events; without a remediation loop, that history is mostly passive observability. With it, every resolved incident improves the next one. It also makes the product more compelling because it moves from "terminal observability" toward "fleet memory that actively helps you recover."

**Why I am confident:** The repo already has CASS, session indexing, workflow event metadata, policy/audit records, and mission/tx receipts. The hard part is not inventing a memory system from nothing; it is making recommendations evidence-honest and policy-safe. That is achievable if the first version is advisory and typed-feature-driven.

## 7. Robot/MCP Contract Doctor

**Idea:** Add a contract doctor that continuously verifies Robot Mode and MCP parity, schema stability, redaction coverage, policy gating, and golden envelope behavior across all machine-facing control surfaces.

This should answer:

- Does every Robot command have the expected envelope shape?
- Does the equivalent MCP tool/resource expose compatible fields and errors?
- Are mutating paths policy-gated?
- Are pane-content paths redacted?
- Are error codes stable and useful?
- Are TOON and JSON outputs semantically equivalent?
- Did a hidden feature flag or fixture drift make a documented surface lie?

**How users and agents would perceive it:** Human users would get more confidence that the control plane can be automated safely. AI agents would benefit even more: they are brittle consumers of schemas, error codes, and output shapes. When a field changes silently or a tool reports a generic internal error, agent workflows degrade fast. A doctor command gives both humans and agents a crisp "this interface is safe to automate" verdict.

This also makes FrankenTerm easier to recommend. A swarm-native terminal should be unusually reliable for machine consumers; a contract doctor makes that reliability visible.

**How it would work technically:**

1. Inventory all Robot commands and MCP tools/resources from the existing typed registries.
2. Generate a command/tool matrix with declared mutability, policy surface, redaction requirement, output envelope type, error code family, and fixture coverage.
3. Execute no-mock subprocess checks for a safe subset and fixture-based checks for dangerous surfaces.
4. Compare JSON and TOON canonical forms using existing robot envelope canonicalization.
5. Fail on missing policy gates for mutation, missing redaction on read paths, incompatible schema drift, unknown error code families, or undocumented MCP-only/Robot-only gaps.
6. Publish the doctor output as an attestation-producing artifact once stable.

**Why it would make FrankenTerm better:** Robot/MCP are core differentiators. If those surfaces are inconsistent, every ambitious agent workflow becomes fragile. This idea improves reliability and ergonomics without adding much product complexity. It also gives future contributors a concrete guardrail: add a tool, update the matrix, prove the contract.

**Why I am confident:** There is already a large amount of typed infrastructure: Robot response envelopes, MCP tool structs, fuzz parsers, golden matrices, error code families, redaction tests, and attestation slots. The doctor mostly needs to assemble and enforce what the repo already knows.

## 8. Mission Rehearsal Scorer

**Idea:** Create an objective rehearsal scorer that evaluates a mission before it runs and returns a quantified, explainable risk profile. It would simulate or dry-run the mission against operating-envelope state, policy gates, historical failures, resource budgets, connector quotas, proof availability, replay fixtures, and compensation coverage.

The output should not be a vague "looks good." It should include:

- readiness score and blocking reasons
- predicted resource pressure
- policy approvals required
- steps without compensation
- steps likely to fail based on history
- proof lanes needed and whether RCH can currently admit them
- rehearsal evidence references
- suggested plan edits that would improve safety

**How users and agents would perceive it:** This would make mission execution feel less like a leap of faith. Users could compare two possible mission plans and pick the safer one. Agents could optimize plans before asking for approval. When a mission is denied, the scorer would explain how to make it admissible: reduce parallelism, split proof phases, add compensation, wait for RCH, or switch to read-only mode.

Radically, this could become the equivalent of a "compiler optimizer" for swarm work: not only checking that a mission is valid, but rewriting it into a safer execution shape.

**How it would work technically:**

1. Start with deterministic scoring rules over existing mission/tx structs: missing compensation, mutating steps, approval gates, dependency cycles, stale target panes, and envelope denial.
2. Add resource estimation by borrowing capacity governor and operating-envelope budgets.
3. Add historical priors from the Swarm Learning Remediation Loop: similar missions failed at step N, connector X was rate limited, proof lanes were blocked, etc.
4. Add replay-backed checks for workflows and policy behavior.
5. Emit a stable `MissionRehearsalReceipt` that can be stored, diffed, and attached to later Tx receipts.
6. Eventually add plan transformation suggestions, but require humans or steering logic to accept changes explicitly.

**Why it would make FrankenTerm better:** It improves safety before execution, where fixes are cheapest. Tx compensation is valuable after failure; rehearsal reduces avoidable failure before commit. For users, it makes the system feel trustworthy. For agents, it provides a concrete optimization target rather than a pile of implicit constraints.

**Why I am confident:** Mission and Tx already have typed contracts, dependency logic, kill switches, failure injection, and receipts. Operating envelope and policy already know many blocking conditions. The first useful version can be simple and deterministic, then become more predictive as historical data accumulates.

## 9. RCH Admission Explainer

**Idea:** Add a focused proof-lane admission explainer that tells an agent whether a proposed proof command is admissible before it attempts to run it. It should classify the command, estimate worker needs, check current RCH health and operating-envelope proof windows, and produce a precise reason if the answer is no.

This is narrower than the Deferred Proof Conveyor. The conveyor manages queued proof over time. The admission explainer prevents bad proof attempts up front.

**How users and agents would perceive it:** Agents would stop discovering RCH refusal only after they have already committed to a proof lane. They would get a crisp answer:

- "Remote proof admitted: 1 worker, package-scoped test, expected target dir ok."
- "Deferred: no admissible workers; queue proof intent instead."
- "Invalid proof: command is local-only hygiene and cannot satisfy remote proof."
- "Blocked: command asks for local fallback, which violates repo policy."
- "Too broad under current pressure: use package-scoped check first."

For human users, this reduces noisy session endings. The final report becomes "proof was inadmissible for reason X, queued as Y" instead of "I tried RCH and it failed mysteriously."

**How it would work technically:**

1. Parse cargo/proof commands into a typed `ProofCommandAnalysis`.
2. Classify package scope, target kind, estimated build pressure, local-fallback risk, target-dir hygiene, and whether the command can produce material proof.
3. Query operating-envelope proof state and RCH selector diagnostics.
4. Emit human, JSON, and TOON output with reason codes matching the operating-envelope vocabulary.
5. Integrate with `ft proof queue`: a deferred result can become a proof intent directly.
6. Add fixtures for every known refusal pattern: no workers, critical pressure, local fallback, malformed command, broad workspace proof under red pressure, contradictory evidence, and target-class skipped proof.

**Why it would make FrankenTerm better:** This is a high-pragmatism idea because it addresses an existing source of wasted time. It makes the remote-proof doctrine easier to follow and harder to accidentally violate. It also gives the top-5 Proof Conveyor cleaner inputs.

**Why I am confident:** The repo already has operating-envelope proof reason codes, RCH pressure semantics, and documented fail-closed proof rules. The current Beads list also shows repeated RCH-blocked work. A preflight explainer is a small surface with large workflow impact.

## 10. Agent Mail Outage Spool

**Idea:** Build a durable coordination spool for periods when Agent Mail is unavailable. Instead of agents repeatedly retrying or losing coordination messages, they write signed, local, replayable coordination intents that can be inspected through Beads/git snapshots and delivered once Agent Mail recovers.

This must respect the repo's Agent Mail process protection rule. It should never repair, restart, or kill Agent Mail. It is a client-side fallback and replay layer only.

**How users and agents would perceive it:** Agent Mail outages would become less disruptive. Agents would still coordinate through a visible local queue: "I intended to notify X about reservation Y; delivery deferred because Agent Mail was red; here is the replay receipt." Humans could inspect the spool without needing the service to be healthy.

Agents would perceive it as a safe handoff surface. Instead of silently skipping coordination, they would record intent, proceed with admitted work, and later reconcile delivery.

**How it would work technically:**

1. Define a local `CoordinationIntent` schema with recipients, subject, body, related bead, file reservations, timestamp, sender identity, and delivery state.
2. Store intents under a repo-local coordination directory or Beads-compatible artifact with strict redaction rules.
3. Expose `ft coordination spool status`, `ft coordination spool add`, and `ft coordination spool replay`.
4. Integrate with `scripts/swarm-tick.sh --agent-mail-fallback frankenterm` so fallback snapshots include pending intents.
5. On recovery, send messages through normal Agent Mail APIs and persist delivery receipts.
6. Add duplicate detection so replay does not spam recipients if a send actually succeeded before the outage was observed.

**Why it would make FrankenTerm better:** Coordination failures are operationally expensive in a multi-agent repo. This idea improves reliability without violating the hard rule against touching Agent Mail service processes. It also improves auditability: "message not delivered yet" becomes a first-class state, not an invisible gap.

**Why I am confident:** The workflow already has a documented fallback path and the memory of prior sessions shows Agent Mail degradation as a recurring condition. A local outbox with receipts is a mature pattern and can be implemented incrementally.

## 11. Attestation Graph Explorer

**Idea:** Build an interactive local explorer for the attestation graph: every claim, manifest slot, producer bead, artifact hash, signing status, freshness, retraction, skipped-proof caveat, and release-blocking gap. It should be available as CLI JSON/TOON, MCP resources, and eventually a compact human UI.

This is more ambitious than a pretty dashboard. It should let users ask:

- "Which README claims are currently release-blocking?"
- "Which artifacts are stale relative to HEAD?"
- "Which target-class claims are still skipped_not_proven?"
- "What changed since the last release bundle?"
- "If I retract this slot, what claims become unsupported?"
- "Which Beads must close before the release is honest?"

**How users and agents would perceive it:** The attestation discipline would become much easier to understand. Today, the manifest and release checklist are powerful but somewhat expert-facing. An explorer turns them into an operational map. Users would see why a claim is trusted, or exactly why it is not.

For agents, this provides a safe way to reason about claims. Instead of editing README wording optimistically, an agent can query whether a claim has a live artifact and whether that artifact is release-grade.

**How it would work technically:**

1. Parse `docs/attestations/manifest.json`, release bundles, retractions, producer-bead metadata, and artifact hashes into a graph model.
2. Add graph queries for missing artifacts, stale hashes, skipped proofs, retracted slots, unlinked claims, and strict-required failures.
3. Expose `ft attestation graph`, `ft attestation why <claim-or-slot>`, and `ft attestation diff <bundle-a> <bundle-b>`.
4. Add MCP resources for release-blocking status and per-slot evidence.
5. Wire release checklist output into the graph so humans get next actions, not just validation failures.
6. Eventually use it as the release-readiness front door.

**Why it would make FrankenTerm better:** FrankenTerm's claims are only compelling if they are easy to verify. This idea improves trust, contributor ergonomics, and release discipline. It also reduces accidental overclaiming, which is especially important in a project with ambitious performance and safety language.

**Why I am confident:** The attestation system already exists, including manifest slots, release bundles, checklist docs, proof artifacts, and verification commands. The explorer is mostly a better query and explanation layer.

## 12. Operator First-Run Guided Tour

**Idea:** Create a guided first-run experience that exercises FrankenTerm's core supported surfaces in a safe order: config discovery, passive capture, redaction, Robot Mode state/get-text/search, MCP manifest, policy denial explanation, operating-envelope readout, read-only mission objective plan, and attestation verification.

This should be an actual guided command, not a long README section:

`ft quickstart --guided`

It should produce a local receipt saying which surfaces passed, which were skipped because a feature was disabled, and which require operator action.

**How users and agents would perceive it:** New users would get to "I understand what this does" much faster. FrankenTerm is technically deep; without a guided path, the first impression can be intimidating. A good quickstart would make the value obvious without pretending every advanced feature is required on day one.

Agents would also benefit. A robot could run the quickstart receipt to understand which capabilities are available in the current checkout: MCP feature disabled, no live mux, no storage DB, RCH unavailable, etc.

**How it would work technically:**

1. Implement a read-mostly quickstart runner with explicit phases and skip reasons.
2. Use existing supported commands where possible instead of hidden setup logic.
3. Include no mutating pane sends by default. Any mutation must be opt-in and policy-gated.
4. Emit a `QuickstartReceipt` with schema version, timings, feature availability, redaction check result, policy check result, and suggested next command.
5. Add golden receipts for no-mux, no-db, MCP-disabled, full local demo, and Agent Mail unavailable cases.
6. Keep the output concise; link to deeper docs only after proving the local state.

**Why it would make FrankenTerm better:** It improves adoption and user-friendliness. Many of the top ideas make the system more powerful, but this makes it approachable. It also doubles as a smoke test for supported surfaces.

**Why I am confident:** README already has a strong onboarding narrative, Robot Mode and attestation commands already exist, and the system already has standard envelopes. The work is mostly sequencing and receipt design.

## 13. Pane Ownership Firewall

**Idea:** Strengthen pane reservations into a full ownership firewall that prevents accidental cross-agent interference. The firewall would combine reservations, agent correlator identity, policy denials, dirty-tree overlap, attention items, and operating-envelope action classes to answer: "Who may touch this pane, and what exactly are they allowed to do?"

This should cover reads, writes, workflow actions, mission steps, and emergency interventions differently. A pane might allow redacted reads to observers, deny sends to non-owners, require approval for takeover, and allow hard-stop only under emergency policy.

**How users and agents would perceive it:** Multi-agent sessions would become calmer and more predictable. Agents would no longer need to infer ownership from titles, cwd, or recent text. If an agent tries to send into a pane it does not own, the denial should say who owns it, how to request takeover, and what safe read-only actions remain.

Humans would see fewer accidental stomps. This is a major trust improvement for swarms because a single mistaken send can waste hours.

**How it would work technically:**

1. Define a `PaneAccessVerdict` that combines reservation status, correlated agent identity, policy action, mission ownership, and emergency state.
2. Integrate with the policy engine for `ActionKind::SendText`, workflow mutation, mission/tx commit, and intervention-console actions.
3. Add a read-only `ft pane access <pane-id>` and Robot/MCP equivalent.
4. Add takeover/request-release flows through attention/intervention, not direct bypasses.
5. Persist denials in policy audit with ownership-specific reason codes.
6. Build golden scenarios for no owner, active owner, stale owner, mission-owned pane, emergency takeover, and read-only observer.

**Why it would make FrankenTerm better:** It directly improves safety and multi-agent ergonomics. FrankenTerm's core domain is large AI agent fleets; ownership mistakes are not edge cases. This idea makes a central swarm hazard explicit and enforceable.

**Why I am confident:** Reservations, agent correlator, policy engine, mission ownership, and intervention concepts already exist. The firewall is a composition layer, not a new concurrency model.

## 14. Incident Bundle Timeline Explorer

**Idea:** Turn incident bundles into causality timelines. Given a crash, stuck pane, policy denial storm, storage stall, RCH outage, or capacity emergency, FrankenTerm should render the sequence of relevant events across panes, processes, resource pressure, workflows, policy decisions, Tx receipts, and proof attempts.

This should answer:

- What changed first?
- Which pane or subsystem started the cascade?
- What did policy deny or allow?
- Was resource pressure a cause or an effect?
- Which workflows ran?
- Which proof or attestation states were stale?
- What would the operator runbook recommend at this point?

**How users and agents would perceive it:** Debugging would feel less like archaeology. Instead of opening raw logs and snapshots, users get a timeline with evidence links. Agents could summarize incidents accurately because the timeline constrains them to retained facts.

This is radically ambitious if taken far enough: FrankenTerm could become an incident forensics engine for AI-agent fleets, not just a terminal.

**How it would work technically:**

1. Define a normalized `TimelineEvent` over existing event/audit/telemetry/receipt sources.
2. Add trace/correlation ids where already available, and infer weak correlations only with explicit confidence.
3. Build timeline generation from incident bundle contents first, so it works offline and does not mutate live state.
4. Add causality hints: same pane, same mission, same Tx, same proof intent, same resource pressure window, same policy actor.
5. Expose CLI JSON/TOON and a human compact view.
6. Add fixture incidents: crash, stuck workflow, RCH outage, capacity red, connector breaker open, and policy denial loop.

**Why it would make FrankenTerm better:** It improves operability and trust. The system already captures a lot; the timeline makes that evidence usable during and after incidents. It also creates better training data for the Swarm Learning Remediation Loop.

**Why I am confident:** Incident bundles already collect many relevant sources. The first version can be offline and read-only, which keeps the risk low. The value compounds as more subsystems attach stable event ids and receipts.

## 15. Golden Replay Studio

**Idea:** Build a replay studio for terminal-fleet behavior. Operators and developers should be able to capture a representative incident or workflow once, then replay it against new pattern rules, workflow handlers, policy rules, Robot/MCP contracts, redaction changes, and extension candidates.

This is complementary to the Policy-Safe Extension Workbench and Mission Rehearsal Scorer. It provides the deterministic lab where new behavior proves itself before live deployment.

**How users and agents would perceive it:** Users would gain confidence that changes will not regress real-world behavior. An agent proposing a new workflow handler could include a replay receipt: "This handler detects the quota event in replay A, does not fire on replay B, redacts secret C, and never sends text without policy approval."

For developers, this would make subtle terminal/control-plane behavior easier to test than broad live integration runs.

**How it would work technically:**

1. Define a compact replay package format: pane deltas, timestamps, detected patterns, expected events, policy fixtures, optional resource-pressure traces, and redaction expectations.
2. Add tools to record from live data with strict redaction and minimization.
3. Build replay runners for pattern detection, workflows, search indexing, policy decisions, Robot/MCP envelope generation, and extension outputs.
4. Support golden assertions and differential comparisons between HEAD and a baseline artifact.
5. Attach replay receipts to proof conveyor and attestation graph when a replay is release-relevant.
6. Add a small curated corpus first: auth prompt, usage limit, rate limit, stuck command, secret-containing pane output, and mission/tx failure.

**Why it would make FrankenTerm better:** Replay is the bridge between ambitious automation and safety. It improves robustness because changes can be tested against realistic sequences, not only isolated unit cases. It improves performance work because replay can benchmark capture and workflow latency. It improves extension safety because third-party logic has to survive known scenarios before activation.

**Why I am confident:** The repo already has replay concepts, golden fixtures, redaction discipline, workflow handlers, event storage, and attestation culture. A replay studio would unify those into a developer/operator workflow with clear incremental milestones.

## How These 10 Complement The Top 5

- The **Swarm Learning Remediation Loop** makes the **Swarm Steering Loop** smarter without giving it unchecked autonomy.
- The **Robot/MCP Contract Doctor** makes every machine-facing top-5 idea safer to automate.
- The **Mission Rehearsal Scorer** gives steering and Tx a pre-execution quality gate.
- The **RCH Admission Explainer** is the immediate ergonomic front end for the **Deferred Proof Conveyor**.
- The **Agent Mail Outage Spool** strengthens coordination while respecting Agent Mail process-protection rules.
- The **Attestation Graph Explorer** makes the proof and claim system understandable enough to trust.
- The **Operator First-Run Guided Tour** makes the whole project easier to adopt.
- The **Pane Ownership Firewall** hardens the swarm safety model underneath attention, steering, and intervention.
- The **Incident Bundle Timeline Explorer** turns captured evidence into useful diagnosis.
- The **Golden Replay Studio** gives extensions, workflows, policies, and mission plans a realistic proving ground.

If the top 5 define the control loop, these 10 define the learning, proof, onboarding, ownership, and forensics layers that make the control loop durable.
