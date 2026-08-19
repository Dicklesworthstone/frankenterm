# Codex Scores On WIZARD_IDEAS_CC.md

Scoring scale: 0 is actively bad, 500 is interesting but not clearly worth the implementation burden, 750 is strong and probably worth doing, 900+ is exceptional. I weighted practical value for human operators and AI coding agents, fit with FrankenTerm's actual architecture, implementation tractability, and whether the utility justifies added complexity.

## 1. Closed-Loop Dispatch - Verified-Submit Send with Delivery Receipts

**Score: 930 / 1000**

This is the strongest idea in the CC list because it attacks a real daily failure mode at the exact actuation point where FrankenTerm has unique leverage: it owns both injection and observation. Verified submission would be immediately useful to meta-agents, workflows, and humans because "bytes written to the PTY" is much weaker than "the agent accepted the prompt and started work." The implementation path is credible: submit profiles, pattern fixtures, receipt types, Robot/MCP parity, and workflow integration are all aligned with existing machinery. The strongest argument against it is maintenance drag from agent-CLI UI drift; composer detection will break unless profile fixtures are actively curated. Even with that caveat, failing honestly with `verification_unavailable` still improves the status quo.

## 2. `ft robot watch-events` - First-Class Event Subscription For Agents

**Score: 900 / 1000**

This is an excellent correction to a mismatch between FrankenTerm's event-driven philosophy and its current CLI-agent polling patterns. A cursorable NDJSON stream plus composite `await` would reduce process churn, token cost, latency, and bespoke loop code for every meta-agent. It is also practical because the event bus, event table, SSE filtering concepts, redaction, and bounded fanout already exist. The strongest argument against it is delivery semantics: once agents depend on it, disconnect/restart behavior and at-least-once duplicates must be specified very precisely. That is manageable, and the payoff is broad.

## 3. Dead-Wire Closure And Wiring Attestation Gate

**Score: 875 / 1000**

This is smart because it fixes both specific dormant capabilities and the process failure that created them. Wiring BOCPD, connector reliability/governor, and capacity governor in shadow mode would convert already-paid engineering cost into real runtime value, while a `doctrine/wiring-status` gate would make future dormant decision systems visible. It fits the repository's attestation culture unusually well. The strongest argument against it is that "dead-wire" analysis can become bureaucratic and noisy: legitimate substrate, experimental APIs, and feature-gated code can be misclassified unless the manifest discipline is very clear. Still, the idea is highly accretive and addresses a recurring architecture smell.

## 4. `ft robot next` - The Meta-Agent's Single Decision Call

**Score: 830 / 1000**

This is a strong ergonomic idea with a low implementation risk profile. A ranked, token-cheap "what deserves attention now" envelope would make meta-agent loops much simpler and would likely become the default orientation call. It is read-only, additive, and mostly composed from existing state, events, approvals, and envelope verdicts. The strongest argument against it is that ranking quality can become contentious and may hide important lower-ranked context if consumers over-trust it. The mitigation is exactly what CC suggests: deterministic ordering, explicit `reasons[]`, and budgeted elision rather than opaque prioritization.

## 5. Time-Travel CI - Replay-Driven Regression Gating

**Score: 800 / 1000**

Replay-driven regression gating is one of the best ways to make changes to policy, workflow, mission, and tx behavior less scary. It leverages a large existing replay subsystem and turns incident promotion into a compounding test corpus. The idea is technically aligned with FrankenTerm's determinism and attestation story. The strongest argument against it is fixture maintenance: behavior fixtures for workflows and rule packs can become expensive to bless and easy to ossify around accidental behavior. This is still worth doing, but only with a narrow initial corpus and a disciplined expected-drift workflow.

## 6. Declarative Fleet Reconciliation - `ft fleet apply`

**Score: 765 / 1000**

This is the most product-defining idea in CC's second tier: it would make the "Kubernetes for terminal agents" analogy much more literal. Desired-state reconciliation is genuinely useful for long-lived swarms because panes crash, rate-limit, wedge, and drift. The architecture has many of the needed parts: templates, state classification, policy gates, receipts, tx execution, and operating-envelope admission. The strongest argument against it is controller complexity: reconcilers can fight humans, create crash loops, and amplify bad desired state unless backoff, ownership, and kill-switch semantics are excellent. I would do it after attention/proof/verified-submit surfaces mature.

## 7. Rate-Limit-Aware Scheduling And Fleet Economics Ledger

**Score: 815 / 1000**

This is one of the most practically useful CC ideas because rate limits are common, costly, and already detectable. Persisting reset windows and making assignment/reconciliation consult them would turn reactive quota handling into usable capacity planning. It is implementable incrementally: parse reset times, store a ledger, emit reset events, and expose a Robot-friendly forecast. The strongest argument against it is data quality: agent CLIs change wording, reset times may be approximate, and "cost" attribution can easily look more precise than it is. As long as economics are labeled estimates and unparseable windows degrade conservatively, this is a high-value feature.

## 8. Durable Agent Sessions - Crash-Respawn With Agent-Native Resume

**Score: 745 / 1000**

This idea solves a real pain: restoring a terminal without restoring the agent conversation is often not enough. Using agent correlator metadata and per-agent resume profiles is a plausible way to make respawn much more valuable. The fallback ladder is also honest: verified resume, fresh with context, or fresh blind. The strongest argument against it is dependence on third-party CLI resume behavior, which may be inconsistent, unauditable, or unavailable for some agents. I like it, but its reliability ceiling is partly outside FrankenTerm's control.

## 9. Cross-Pane Taint And Provenance With Canary Secrets

**Score: 690 / 1000**

This is novel and security-relevant, and FrankenTerm is one of the few layers that could even attempt it because it mediates pane reads and sends. Canary secrets are especially pragmatic: they are cheap, concrete tripwires with low ambiguity. The taint-flow half is weaker because sketch-based provenance is heuristic and can be defeated by paraphrase, summarization, or model-mediated transformation. The strongest argument against it is false confidence: users may believe they have information-flow security when they really have a useful but incomplete warning layer. I would ship canaries earlier and keep taint as observe-mode advisory for a long time.

## 10. `ft deck` - Operator Command Deck

**Score: 720 / 1000**

This is a good adoption and operations idea because human-facing safety controls are currently less cohesive than Robot/MCP surfaces. A deck that brings approvals, fleet state, interventions, and attention together would make demos and real operations much clearer. The implementation risk is moderate if it stays a composition layer over existing CLI/control surfaces. The strongest argument against it is scope creep: a TUI can quickly become a parallel product with its own logic, inconsistencies, and maintenance burden. The CLI verbs should land first so the feature is not trapped behind UI polish.

## 11. Operation Target-Class - Make The 200-Pane Claim Provably True

**Score: 835 / 1000**

This is not flashy, but it is strategically important. FrankenTerm's largest differentiating claim is fleet scale, and the current attestation discipline explicitly holds back target-class confidence until real target-class proof exists. Hardening the storage hot path, building a realistic load rig, and running on actual target hardware would make the README and operating envelope more honest and more compelling. The strongest argument against it is that it may uncover a long chain of performance bugs rather than produce a clean proof quickly. That is not a reason to avoid it; it is a reason to budget it as an engineering campaign, not a single feature.

## 12. WASM Extension Runtime, Detection Rules First

**Score: 665 / 1000**

The phased version is much better than a general plugin pitch. Pure detection rules first, with no host I/O and replay compatibility, is the right minimal slice for a WASM extension system. The idea could eventually make FrankenTerm a platform for custom terminal intelligence rather than a fixed set of built-ins. The strongest argument against it is security and maintenance surface: even "safe" plugins require ABI versioning, fuel accounting, error isolation, packaging, documentation, and supply-chain policy. I would not start this until replay, contract-doctor, and policy-envelope proof surfaces are stronger.

## 13. Fleet Timeline Forensics - `ft timeline --around`

**Score: 795 / 1000**

This is a very solid read-path feature with high operator value and relatively low risk. Aligning pane output, detections, gaps, audit actions, and workflow executions around a timestamp or event would make incident diagnosis much less painful. It also creates better source material for AI summaries and future remediation learning. The strongest argument against it is that it is convenience over capability: determined users can reconstruct the same facts manually today. That said, the convenience is exactly where forensics tools win or fail, and this should be cheaper than many more ambitious ideas.

## 14. Contract SDKs And Verified Schema Registry

**Score: 810 / 1000**

This is a pragmatic adoption multiplier. FrankenTerm's machine-facing contracts are core product surface, and publishing verified schemas plus thin generated clients would reduce integrator friction substantially. Golden validation against generated schemas fits the repo's existing proof style and would help agents call tools more reliably. The strongest argument against it is SDK maintenance gravity: once Python and TypeScript clients exist, users expect support, examples, releases, and compatibility promises. Keeping clients generated, thin, and explicitly subordinate to the schemas is essential.

## 15. `ft doctor --fix` - Receipted, Reversible Auto-Remediation

**Score: 700 / 1000**

This is useful if kept extremely conservative. Receipted fixes for known-benign states could reduce operator toil and help unattended agents recover from routine local problems. The proposal wisely includes dry-run plans, precondition probes, post-verification, receipts, and a hard line against destructive actions. The strongest argument against it is cultural and safety risk in this repo: AGENTS.md is deliberately hostile to autonomous repair or cleanup because agents have caused damage before, and "doctor fix" features tend to grow from safe repair into questionable mutation. I would only ship a tiny allowlist first, with no deletion, no Agent Mail service repair, and very loud receipts.
