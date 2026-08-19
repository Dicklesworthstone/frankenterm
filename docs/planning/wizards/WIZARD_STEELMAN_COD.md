# Codex Steelman Of Opponent Ideas

This is intentionally written against my own prior incentives. I am making the strongest case I can for CC's number 1 idea and GMI's number 1 idea, including places where the steelman changes my mind.

## 1. CC Idea: Closed-Loop Dispatch - Verified-Submit Send

### Why It Is Actually Brilliant

The non-obvious insight is that `send` is not a terminal write. In an agent swarm, `send` is an intent transfer across three boundaries: FrankenTerm to PTY, PTY to agent UI composer, and composer to the model/runtime that actually begins work. Existing terminals can only prove the first boundary. FrankenTerm is structurally positioned to prove all three because it owns both actuation and observation.

That makes this idea much bigger than "press Enter more reliably." It turns dispatch from fire-and-forget I/O into a closed control loop with typed terminal states. It also uses FrankenTerm's core thesis on itself: observe, detect, react, and audit. The feature would make the platform's own control plane safer in the exact place where AI swarms waste the most hidden time.

The real brilliance is that it converts undocumented human folklore into a durable protocol. Today, "Codex often needs a second Enter" and "Claude Code text can sit in the composer" are operational superstitions. Verified-submit would make those conditions machine-readable: `submitted`, `queued_behind_operation`, `stuck_in_composer`, `verification_unavailable`, `pane_crashed_to_shell`, or `profile_drift`.

### Strongest Implementation Path In This Codebase

The strongest path is not to add a generic send retry loop. It is to add an explicit submit-verification state machine beside the existing policy-gated send path.

1. **Receipt type and schema:** Add a `SubmitReceipt` to Robot send output and the MCP `wa.send` mirror. The receipt should include `state`, `agent_type`, `profile_id`, `profile_version`, `attempts`, `evidence_rule_ids`, `elapsed_ms`, `cursor_before`, `cursor_after`, and `idempotency_key`.
2. **Profile pack, not hard-coded logic:** Define submit profiles in rule-pack data. Each profile should declare anchors for composer-nonempty, composer-cleared, working-state, queued-behind-operation, crash-to-shell, and optional remediation steps. This keeps drift in the same maintenance lane as other pattern rules.
3. **Agent correlation first:** Use the existing agent correlator to choose a profile. If the agent is unknown, the feature should not guess; it should emit `verification_unavailable` and behave like today's send.
4. **Observation via existing read/wait machinery:** Reuse the get-text/wait-for pipeline and event cursoring where available. The verifier should watch recent deltas and semantic/pattern state, not sleep blindly.
5. **Idempotency and duplicate protection:** An idempotency key should prevent re-sending the same prompt when the prior state is `submitted` or `queued_behind_operation`. This is critical for meta-agents recovering after disconnect.
6. **Workflow adoption:** Built-in handlers such as compaction and usage-limit recovery should call the same verified send primitive. Otherwise the feature helps humans but leaves the most important automation vulnerable.
7. **Golden fixture matrix:** Fixtures should cover at least Claude Code composer hold, Codex single Enter, Codex second Enter, queued-behind-operation, unknown profile, profile drift, shell crash, and timeout.

The important implementation detail is that the state machine is bounded and honest. It should never claim success without observed evidence, and it should never turn profile uncertainty into repeated Enter spam.

### Second-Order Benefits The Original Understated

Verified-submit becomes a foundation for safe higher-level orchestration. A Swarm Steering Loop, Fleet Reconciler, or Intervention Console cannot be trustworthy if its basic actuation primitive only proves bytes were written. This receipt becomes the causal bridge between "the planner chose an action" and "the target agent accepted the work."

It also creates a new drift detector for third-party agent CLIs. If a profile starts returning `verification_unavailable` or `stuck_in_composer` at elevated rates after an agent update, FrankenTerm can surface "agent UI contract drift" before operators waste hours. That is a control-plane health signal no generic terminal can produce.

It improves audit quality. A policy audit row that says "send allowed" is incomplete; a submit receipt can say whether the allowed action actually reached the agent runtime. That matters for incident reconstruction and for any future provenance or mission receipt story.

It also reduces hidden token and compute waste. The worst failure is not a visible error; it is a pane sitting idle for 45 minutes while the orchestrator assumes it is working. Verified-submit turns silent idleness into an actionable state quickly.

### Objection 1: Agent UI Drift Will Break It

This is real, but not fatal. FrankenTerm already lives with pattern drift because agent UIs and terminal output evolve. The correct response is not to avoid profile-based features; it is to make profiles fixtured, versioned, observable, and fail-honest.

When a profile drifts, the receipt should say `profile_drift_suspected` or `verification_unavailable`, not "submitted." The failure mode then becomes visible and no worse than today's baseline. In fact, the drift signal is a benefit: today UI drift causes silent dispatch failures; verified-submit would make drift measurable.

### Objection 2: This Adds Latency To Send

It adds latency only when the caller asks for a stronger semantic guarantee. `ft robot send` can preserve the current fast path by default or expose a strict mode for automation. For meta-agent dispatch, a few hundred milliseconds of verification is cheap compared with minutes or hours of silent non-submission.

The system can also offer levels: write receipt, composer receipt, submitted receipt, and working receipt. Users can choose the strength they need. High-scale dispatch can pipeline verification with bounded concurrency.

### Honest Residual Concerns

The hardest residual concern is full-screen TUIs with unstable composers. Some states may be hard to distinguish from text alone, especially across themes, wrapping, and transient animations. Semantic terminal zones, if available, would help, but cannot be assumed.

Another concern is user expectation. If `--verify-submit` exists, users may believe every profile is equally strong. The receipt must make confidence and profile coverage explicit. Unknown profiles should be boring and honest, not magical.

### Did This Steelman Change My Mind?

Yes. I already scored this highly, but steelmanning it makes me think it is not merely CC's best idea; it is probably the single most immediately valuable idea in the whole exercise. It should be built before any high-level autonomous orchestration that sends prompts to agents.

## 2. GMI Idea: Terminal-Native DOM And Semantic Query API

### Why It Is Actually Brilliant

The non-obvious insight is that "terminal text" is the wrong abstraction for AI agents. Humans see a terminal as a visual stream; agents need a structured state query. FrankenTerm already sits beneath the agent fleet with access to terminal state, scrollback, semantic zones, escape parsing, prompt markers, pane metadata, and capture deltas. If it exposes that as structured facts, it removes a major source of brittle agent inference.

The idea is strongest when it is not framed as a universal DOM. The brilliant version is a **semantic evidence API**: a set of typed, provenance-bearing projections over terminal state, each with a confidence and availability model. Agents should be able to ask "what command just finished?", "what region is prompt input?", "what output is new since cursor X?", "is this pane in alternate screen?", "what regions are semantic zones?", "where are errors?", and "what evidence supports that answer?"

This matters because raw `get-text` forces agents to spend model tokens reconstructing structure FrankenTerm may already know. A semantic query API shifts that work from probabilistic LLM interpretation into deterministic or confidence-labeled terminal parsing.

### Strongest Implementation Path In This Codebase

The right path is layered, conservative, and availability-aware.

1. **Start with semantic regions, not a full DOM:** Expose existing terminal/capture concepts as `SemanticRegion` records: prompt, input, output, command, alternate-screen, status-line, selection, wrapped-line, gap, and unknown. Each record includes pane id, scrollback offsets or segment ids, byte/line ranges, source (`osc133`, `termwiz_zone`, `pattern`, `heuristic`, `agent_profile`), confidence, and freshness.
2. **Use OSC 133 and existing semantic zones where present:** For shells or tools that emit prompt/input/output markers, the API can be precise. For agent TUIs or unmarked shells, return partial data with `semantic_data_unavailable` or lower-confidence heuristic regions. This avoids pretending every pane has a reliable DOM.
3. **Add cursorable semantic deltas:** `ft robot semantic-deltas --cursor ...` should return new regions and changed classifications since the last cursor. This composes with watch-events and avoids repeatedly dumping scrollback.
4. **Expose high-value queries as thin projections:** `last-command`, `last-exit-status`, `new-output`, `prompt-region`, `composer-region`, `error-regions`, and `alt-screen-state` should be projections over the same region model. Do not create separate ad hoc parsers for every query.
5. **Store optional semantic indexes:** Persist enough region metadata alongside output segments to support search, replay, and incident timelines without rescanning everything. Raw output remains canonical; semantic regions are derived evidence.
6. **Robot/MCP parity:** Add Robot JSON/TOON and MCP resources only after the core region schema has fixture coverage. Agents need stable schemas more than clever prose.
7. **Shell integration installer as an accelerator, not a requirement:** Provide an optional shell integration that emits OSC 133 markers. The API must remain honest when integration is absent.

The implementation should be explicit about confidence. The top-level response should not say "last_command" unconditionally; it should say `ok`, `available`, `source`, `confidence`, `evidence_ranges`, and `fallback_hint`.

### Second-Order Benefits The Original Understated

The semantic API strengthens verified-submit. Composer detection, composer-cleared state, queued-behind-operation state, and working-state evidence become much easier if terminal regions can represent input/composer/output boundaries rather than only text tails.

It also improves redaction and privacy. Redaction can become region-aware: command input, tool output, status line, and copied secret-looking material can be treated differently. That is safer than one flat text stream.

It improves replay and forensics. Incident timelines can align semantic events rather than raw lines: command started, output burst, prompt returned, alt-screen entered, agent composer stuck, gap detected. That makes post-incident analysis much more useful.

It lowers token cost without lossy compaction. Instead of summarizing away scrollback, FrankenTerm can return the exact relevant semantic regions with raw evidence pointers. Agents get less text while preserving auditability.

It may also create a moat for FrankenTerm. Generic tmux wrappers can scrape text; few systems can expose a terminal-state API with provenance, semantic zones, redaction, Robot/MCP schemas, and replay compatibility.

### Objection 1: Agent CLI TUIs Do Not Emit Shell Semantic Zones

Correct, and the API should not pretend otherwise. This is why the right first abstraction is `SemanticRegion` with availability and source, not `get_last_command()` as a universal promise.

For agent TUIs, the API can still expose useful structure: alternate-screen state, pane title, agent type, status lines, detected composer regions from profiles, output gaps, prompt-like anchors, and working/stuck state from existing pattern classifiers. The response can say which fields are unavailable. Partial structured truth is better than full unstructured text.

### Objection 2: This Reintroduces Heuristic Parsing Under A Fancy Name

Only if implemented carelessly. The fix is to make source and confidence first-class. OSC 133-derived regions can be high confidence. Pattern-derived composer regions can be medium confidence. Guessed prompt boundaries can be low confidence or omitted. Agents can decide what to trust.

This is aligned with FrankenTerm's attestation discipline: do not overclaim, do not erase uncertainty, and keep raw evidence accessible. A semantic API with explicit unknowns is less brittle than forcing every agent to invent private heuristics over raw text.

### Honest Residual Concerns

The main residual concern is schema scope. A "DOM" can become a sprawling abstraction that tries to model every visual terminal detail and never stabilizes. The project should avoid browser-DOM ambition and ship a small semantic evidence model first.

Another concern is storage overhead. Persisting semantic regions for every pane and line could become expensive. The first version should derive live and persist only durable region markers or indexes needed for replay/search.

Finally, shell integration adoption is uncertain. The feature must deliver value without requiring every pane to install perfect OSC 133 support.

### Did This Steelman Change My Mind?

Yes, moderately. I previously scored this as strong but not top-tier because I focused on overclaiming risk. The steelman reveals that, if reframed from "Terminal DOM" to "semantic evidence API with explicit availability," it is a foundational enabler for verified-submit, attention, replay, and token-efficient agent context. I would now rank it higher than several of my own middle-tier ideas.
