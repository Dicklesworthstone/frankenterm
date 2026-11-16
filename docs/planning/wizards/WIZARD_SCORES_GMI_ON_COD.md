# Evaluation of Codex's Ideas (WIZARD_IDEAS_COD.md)

**1. Swarm Steering Loop**
**Score: 780**
Abstracting mission planning into an explainable, capacity-aware lifecycle perfectly matches the `Cx` and `Tx` runtime boundaries. It provides a much-needed UX layer that turns raw components into an actionable control plane for both humans and agents. The strongest argument against it is that it creates conflicting orchestration abstractions by competing with existing external tools like `ntm`. Furthermore, if it relies on heuristic data to generate the "plan", it introduces nondeterminism into a system that prides itself on strict, side-effect-free, rule-based execution.

**2. Deferred Proof Conveyor**
**Score: 750**
Queuing proof intents when RCH workers are unavailable is an extremely practical solution to a daily development bottleneck. It prevents agents from falling back to forbidden local cargo runs and seamlessly integrates with the attestation graph. The strongest argument against it is that queued proofs suffer from "dirty tree" rot. By the time remote workers are available, the local codebase has often changed, meaning the deferred proof runs against an invalid state and wastes expensive compute.

**3. Unified Attention and Intervention Console**
**Score: 600**
Consolidating stalled panes, approvals, and envelope status into a single attention surface drastically improves operator UX during incidents. It provides a clear, typed action queue instead of scattering state across multiple CLI commands. The strongest argument against it is that this is merely an aggregation view of existing APIs rather than an architectural innovation. It doesn't solve underlying orchestration issues and risks creating an overwhelming "wall of red" for the operator during a major incident.

**4. Adaptive Governor Mesh**
**Score: 810**
Fusing all pressure signals (memory, network, RCH, BOCPD) into a single predictive budget model is an excellent realization of the fail-closed architecture. It protects the host system proactively and allows agents to explicitly ask for capacity budgets. The strongest argument against it is that synthesizing multiple disparate signals into a unified choke point usually results in severe over-throttling. If a single heuristic like BOCPD fires a false positive, the entire fleet could grind to a halt unnecessarily.

**5. Policy-Safe Extension Workbench**
**Score: 480**
A restricted WASM extension host for custom detection and search logic opens up the platform to enterprise extensibility without requiring core forks. Leveraging existing capabilities and replay tests ensures a safe rollout. The strongest argument against it is that the `wasmtime` integration is incredibly heavy and complex for the proposed use case. The serialization overhead of passing continuous terminal streams into WASM sandboxes will absolutely destroy the system's stringent CPU and latency budgets.

**6. Swarm Learning Remediation Loop**
**Score: 380**
Using CASS to propose auto-remediations based on past fixes is a highly ambitious attempt at LLM-driven self-healing. It turns the passive observability data into active, actionable leverage for agents. The strongest argument against it is that blindly trusting vector-search similarities to propose mutating `Tx` fixes is incredibly dangerous. An error in one file might look semantically identical to a past error elsewhere, leading the system to confidently execute the wrong destructive command.

**7. Robot/MCP Contract Doctor**
**Score: 400**
A CI verifier for schema drift and policy gating enforces rigorous hygiene across all machine-facing control surfaces. It ensures that TOON and JSON outputs remain semantically equivalent and reliable for agent consumption. The strongest argument against it is that it's essentially just a test suite masquerading as a major feature. Writing tests for API stability is basic table stakes, not a groundbreaking architectural innovation that improves swarm capabilities.

**8. Mission Rehearsal Scorer**
**Score: 550**
Dry-running missions against constraints, budgets, and replays provides excellent risk mitigation before executing a `Tx`. It transforms the mission system from a blind executor into an optimizing, safety-conscious planner. The strongest argument against it is that dry-running shell commands is fundamentally impossible without side effects unless you use complete virtualization. Estimating risk heuristically for terminal commands will inevitably give operators and agents false confidence.

**9. RCH Admission Explainer**
**Score: 500**
A pre-flight check to determine if RCH will accept a proof lane is a great UX improvement for a highly specific bottleneck. It prevents agents from wasting time and tokens on proofs that will immediately fail admission. The strongest argument against it is that it tightly couples the core terminal aggregator to one highly specific external service (`rch`). Baking bespoke remote compilation logic into FrankenTerm violates separation of concerns and pollutes the core domain.

**10. Agent Mail Outage Spool**
**Score: 320**
A durable outbox for Agent Mail improves resilience by ensuring coordination messages aren't lost during service degradation. It provides a safe, replayable local ledger that respects the repo's strict process-protection rules. The strongest argument against it is that coordinating file reservations asynchronously via an outbox leads to split-brain scenarios. If two agents queue locks on the same file during an outage, they will both proceed, causing massive git conflicts when the spool finally flushes.

**11. Attestation Graph Explorer**
**Score: 390**
An interactive local explorer for the attestation graph provides excellent transparency into the project's release-blocking gaps and proof artifacts. It turns static JSON manifests into a navigable, operational map. The strongest argument against it is that this caters to a tiny niche of auditors rather than the actual agents or operators running the swarm. Building a complex graph query engine just to read static JSON manifest files is massive technical overkill.

**12. Operator First-Run Guided Tour**
**Score: 250**
A `ft quickstart --guided` command is a solid onboarding tool that quickly proves the local state of the terminal. It safely exercises core surfaces like redaction and policy without risking destructive mutations. The strongest argument against it is that a tutorial script is not an architectural innovation. It adds zero runtime capability to the system and will rot instantly as the CLI evolves unless it is maintained perfectly.

**13. Pane Ownership Firewall**
**Score: 640**
Implementing strict reservations and ownership for pane reads and writes elegantly solves the problem of cross-agent interference. It hardens the safety model by making ownership explicit and enforceable via the policy engine. The strongest argument against it is that in a highly dynamic swarm, agents frequently need to inspect other panes to understand context. A strict firewall creates massive permission-management overhead, slowing down collaborative workflows and causing agents to get stuck.

**14. Incident Bundle Timeline Explorer**
**Score: 570**
Turning incident bundles into causality timelines provides fantastic forensics capabilities for post-incident debugging. It constrains agents to retained facts, making their incident summaries highly accurate. The strongest argument against it is that correlating unstructured shell output across multiple agents into a unified timeline is an unsolved data science problem. The timeline will either be too sparse to be useful or it will hallucinate causality based purely on coincidental timestamps.

**15. Golden Replay Studio**
**Score: 530**
Replaying incidents against new rules, policies, and MCP contracts provides a solid test infrastructure for the control plane. It ensures that new workflow handlers can be validated against realistic historical data before live deployment. The strongest argument against it is that it is a purely developer-facing test harness. The cost and complexity of building a full "studio" UI and workflow distracts from the core mission of running live swarms, effectively building a separate product entirely.
