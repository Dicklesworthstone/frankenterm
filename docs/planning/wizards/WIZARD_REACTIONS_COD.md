# Codex Reactions To Peer Scores

I agree with more of the criticism than I expected. The useful distinction is that CC mostly evaluated architectural fit and duplication against the current repo, while GMI mostly evaluated risk, novelty, and whether an idea felt like "real product capability" versus tooling. CC's critique is sharper overall because it noticed existing or in-flight surfaces; GMI is more willing to call out false confidence and heuristic danger, but sometimes overstates that danger by assuming my proposals would auto-mutate when I explicitly intended advisory/read-only first phases.

## Overall Score Pattern

The peer models converged on a few judgments:

- **Strong:** Deferred Proof Conveyor, Swarm Steering Loop, Attention/Intervention, and some form of ownership/attention/timeline tooling.
- **Mixed:** Extension Workbench, Swarm Learning, Mission Rehearsal, Golden Replay, Attestation Graph, and Agent Mail spool.
- **Weak:** Operator First-Run Guided Tour.
- **Most disputed:** Adaptive Governor Mesh. CC scored it 560 as an over-unified coupling hazard; GMI scored it 810 as the cleanest fail-closed realization.

That spread changes my own view. I still think my top five direction is broadly right, but I would rewrite the build order and reframe several ideas more narrowly.

## Where I Agree With CC

CC is right that **Swarm Steering Loop** risks becoming a wrapper vocabulary around Mission/Tx rather than a clearer productization of Mission/Tx. My original writeup said "introduce `SteeringPlan`"; after reading the critique, I think that was the wrong emphasis. The better version is: productize the mission objective planner and Tx executor behind a friendlier steering command, but keep the canonical underlying artifacts as Mission and MissionTxContract wherever possible.

CC is right that **Deferred Proof Conveyor** is inward-facing dev infrastructure. I still think it is important because this repo lives or dies by remote proof discipline, but it is not a general FrankenTerm user feature. Its priority should be justified as "protect the project and release pipeline," not as a flagship product feature.

CC is right that **Unified Attention and Intervention Console** partly overlaps with existing `wa.attention` and attention-router work. The novelty is not "invent attention"; it is consolidation, ranking quality, intervention wiring, and making the operator-facing path coherent. That is still valuable, but my wording overstated greenfield novelty.

CC is right that **Mission Rehearsal Scorer** already partially exists and that a numeric readiness score can become a rubber stamp. I should have emphasized structured findings and blocking reasons over a single score.

CC is right that **Agent Mail Outage Spool** may already be in flight. If exact-once replay verifier and outbox work are already landing, my idea should collapse into reviewing and hardening that existing work rather than creating a parallel artifact.

CC is right that **Golden Replay Studio** hides the hardest part: recording live fixtures with strict redaction and minimization. I still like replay as a substrate, but a "studio" is too broad until the capture/sanitize path is brutally scoped.

## Where I Agree With GMI

GMI is right that **Policy-Safe Extension Workbench** is heavier than my ranking admitted. Continuous terminal streams through WASM are dangerous for latency if implemented naively, and even non-mutating extensions bring ABI, packaging, fuel, and supply-chain complexity. Detection-only, replay-first remains the only defensible first phase.

GMI is right that **Swarm Learning Remediation Loop** must never blindly execute vector-retrieved fixes. My proposal did say typed evidence and advisory first, but the criticism is still important: the user-facing copy must make "similar prior incident" feel like evidence to inspect, not authority to obey.

GMI is right that **Mission Rehearsal Scorer** can generate false confidence if framed as a dry-run for arbitrary shell commands. The useful subset is checking known contracts, policy gates, dependencies, approvals, compensation, envelope admissibility, and replay-backed behavior. It should not pretend to simulate arbitrary terminal side effects.

GMI is right that **Incident Bundle Timeline Explorer** can imply causality from timestamps. The right product name may be "timeline" not "causality timeline"; inferred correlations need confidence labels and should never be rendered as causal proof.

GMI is right that **Operator First-Run Guided Tour** is not an architectural innovation. It is onboarding polish, and in this repo it likely duplicates README, `ft demo`, `ft setup`, and `ft doctor`.

## Where I Think CC Is Wrong

CC underweights **Swarm Steering Loop** by treating it too much as "just another wrapper." The project has many powerful primitives, and productized composition is not decorative. For AI agents, a single safe, explainable objective-to-execution path is a real capability because it reduces improvisation around invisible constraints. That said, CC's warning means the implementation must avoid new canonical plan types unless absolutely necessary.

CC underweights **Swarm Learning Remediation Loop** because it treats cold start as disqualifying. Cold start is real, but this repo already has a memory-rich workflow culture: Beads comments, proof receipts, incident bundles, policy denials, event streams, and session indexing. The feature can start as "attach prior evidence to attention items" long before it proposes mission skeletons.

CC underweights **Attestation Graph Explorer** if the target is only human release managers. The more important audience is AI agents editing claims: a graph/resource that says "this claim is unsupported or stale" prevents overclaiming. Still, I accept this is not top-tier product work.

CC may overstate the problem with **Pane Ownership Firewall** depending on implementation. It should not enforce Agent Mail file reservations directly as hard truth; it should enforce pane ownership and mission/action ownership in FrankenTerm, while ingesting external reservations as advisory context.

## Where I Think GMI Is Wrong

GMI is wrong to call **Robot/MCP Contract Doctor** "just a test suite." For this project, the machine contract is product surface. Agents break on subtle schema, error, TOON/JSON, redaction, and policy drift; a contract doctor is not glamorous, but it directly improves real agent reliability.

GMI is wrong to dismiss **RCH Admission Explainer** as pollution of the core domain. FrankenTerm's own development and release workflow explicitly depends on RCH proof semantics, and the operating envelope already models proof availability. The right boundary is to keep RCH-specific logic in proof/admission tooling, not pretend it is irrelevant.

GMI is wrong to score **Agent Mail Outage Spool** as if it must coordinate locks during outages. I would not use a spool to grant exclusive file reservations. The safe version is a messaging/intent ledger with receipts and explicit "not delivered, not authoritative" semantics.

GMI is wrong to treat **Golden Replay Studio** as primarily a UI or separate product. The useful version is a replay corpus and runner matrix; any studio UI is optional. Replay is one of the few realistic ways to test workflow/policy behavior against representative terminal sequences.

## Consensus Concerns That Changed My Evaluation

**Adaptive Governor Mesh:** Both models raised the same practical concern: a unified governor can over-throttle and couple unlike signals. CC was especially persuasive that envelope planning, connector quotas, BOCPD posterior, fleet memory, and storage backpressure should not collapse into one universal abstraction. I still want cross-signal budget visibility, but I now concede the enforcement design should be "natural consult points plus shared explanations," not a single mesh choke point. My confidence in the original end-state drops materially.

**Policy-Safe Extension Workbench:** Both models flagged heaviness and implementation cost. I still believe in detection-only WASM eventually, but I no longer think it belongs in the first implementation wave. It should wait for replay, contract validation, and budget controls.

**Swarm Learning Remediation Loop:** Both models warned about wrong or stale recommendations. That consensus changes the product framing: it should start as evidence retrieval and comparison, not remediation. "Suggested fix" should require a provenance grade and should never auto-create a mutating Tx without separate steering/rehearsal/policy gates.

**Mission Rehearsal Scorer:** Both models challenged false confidence. I concede the word "score" is risky. The feature should be a checklist/receipt of specific pass/fail/unknown findings, with any numeric rollup treated as presentation only.

**Incident Bundle Timeline Explorer:** Both models warned about false causality. I accept that fully. The explorer should align evidence, not claim causation unless there is an explicit trace/receipt relationship.

**Operator First-Run Guided Tour:** Both models scored this low and I now concede. It is useful polish, but not worthy of a top-15 architecture list unless reframed as consolidation of `demo`, `doctor`, and capability receipts.

**Agent Mail Outage Spool:** Both models raised serious caveats: CC said it is already in flight; GMI said asynchronous lock coordination can split-brain. I concede the idea is not independently strong as written. The safe remaining kernel is durable, non-authoritative message intent and replay receipts.

## Ideas I Now Concede Or Downgrade

1. **Operator First-Run Guided Tour:** Conceded as over-ranked. Fold into `ft demo` / `ft doctor` / capability receipts.
2. **Agent Mail Outage Spool:** Conceded as likely duplicate/in-flight and unsafe if used for coordination authority. Keep only non-authoritative outbox semantics.
3. **Adaptive Governor Mesh:** Downgraded. Keep shared explanations and budget visibility, but enforce at natural subsystem boundaries.
4. **Mission Rehearsal Scorer:** Downgraded unless renamed/reframed away from numeric scoring.
5. **Policy-Safe Extension Workbench:** Still good, but later than I placed it.

## Ideas I Still Stand Behind

1. **Swarm Steering Loop:** But implemented as Mission/Tx productization, not a new canonical orchestration layer.
2. **Deferred Proof Conveyor:** Narrowly for this repo's proof discipline and release pipeline, not as broad product capability.
3. **Unified Attention and Intervention Console:** Especially if built as a read-only attention schema first.
4. **Robot/MCP Contract Doctor:** Despite GMI's low score, this is one of the most important agent-reliability ideas.
5. **Golden Replay Studio:** Not as a UI-heavy studio; as a replay corpus, runner, and receipt system.
