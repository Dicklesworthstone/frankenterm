# Part 1: The Steelman Challenge

## Steelmanning Claude Code's #1: Closed-Loop Dispatch (Verified-Submit Send)

**The Non-Obvious Insight (Why it's brilliant):**
The terminal is not a pipe; it is an actuator operating a state machine. Every other terminal multiplexer treats `send-keys` as a fire-and-forget UDP packet. CC correctly identified that when dealing with AI agents, the UI *is* the API. If an agent is stuck in a composer or blocked by a modal dialogue, sending "Enter" does not submit the prompt—it just adds a newline to the buffer. By transitioning from a "write receipt" to a "delivery receipt," this idea transforms FrankenTerm from a dumb pipe into the first true **closed-loop robotic actuator** for CLI applications.

**The Strongest Implementation Path:**
Instead of relying purely on regex pattern-matching against the screen buffer (which CC proposed and I previously critiqued as brittle), we leverage FrankenTerm's vendored `termwiz` emulator and the existing `capture_pipeline`. We inject an invisible, cryptographic canary (e.g., via a harmless OSC sequence or a specific zero-width character combination) at the end of the payload. The dispatch loop does not declare `submitted` until that specific canary is observed transitioning from the "input/composer" semantic zone to the "historical scrollback/processed" zone. This bypasses agent CLI UI drift entirely: we aren't looking for the word "Working...", we are mathematically verifying that the agent's Read-Eval-Print Loop (REPL) consumed our bytes.

**Second-Order Benefits CC Missed:**
1. **Unsupervised Self-Healing:** A closed-loop dispatch enables the platform to detect when an agent is trapped in an unexpected modal (e.g., a sudden `y/n` prompt from an underlying tool). If submission fails, FT can automatically emit an `Esc` or `Ctrl-C` sequence, clear the buffer, and retry—completely shielding the meta-agent from the UI glitch.
2. **True Latency Metrics:** We finally get accurate "time-to-first-token" and "processing duration" metrics for agents, because we know precisely when the input was accepted, rather than just when it was typed.

**Pre-emptive Defense Against Objections:**
*   *Objection 1: UI drift will break the submit profiles constantly.*
    *Defense:* By using semantic zone tracking (OSC 133) and cryptographic canaries instead of regex scraping, the verification is immune to changes in spinner animations or composer colors.
*   *Objection 2: It adds too much stateful complexity to a simple `send` command.*
    *Defense:* The complexity already exists; it is currently offloaded to the meta-agents, which handle it terribly with `sleep(2)` loops. Centralizing this complexity in FT is the textbook definition of good platform engineering.

**Honest Residual Concerns:**
What happens if the agent CLI natively supports multi-line pasting and naturally buffers the canary without executing it? There will always be edge cases where the REPL boundary is ambiguous, meaning we can never achieve *provable* 100% certainty, only 99.9% statistical confidence.

*Has my mind changed?* Yes. I initially scored this 850 but penalized it for UI brittleness. With the canary/OSC implementation path, this is a 950-level idea. It is the defining feature of a "Robotic Terminal."

---

## Steelmanning Codex's #1: Swarm Steering Loop

**The Non-Obvious Insight (Why it's brilliant):**
Agents fail most catastrophically when they improvise around invisible constraints. We treat AI agents like humans, dropping them into a terminal and saying "figure it out." But humans have implicit understanding of "what is safe to do." The Steering Loop is brilliant because it formally separates **Planning** (a side-effect-free, constraint-aware compilation step) from **Execution**. It turns implicit tribal knowledge ("don't run cargo test if the tree is dirty") into explicit, machine-readable API types before a single byte of mutation occurs.

**The Strongest Implementation Path:**
Implement this entirely within the existing `MissionLoop` and `TxIntent` structures, but expose it via a new MCP resource: `wa://steering/preflight`. When an agent proposes a goal, FT runs a dry-run `TxPrepare` against a lightweight, in-memory clone of the SQLite state. It checks the `OperatingEnvelope`, the `PolicyEngine`, and current `Reservations`. The response is a compiled `MissionTxContract` with explicit `deny_reasons` (e.g., "RCH unavailable, local cargo forbidden"). The agent must attach this cryptographically signed contract to its actual execution request.

**Second-Order Benefits COD Missed:**
1. **The Training Data Goldmine:** By capturing the agent's initial naive plan and the Steering Loop's corrections, FrankenTerm generates the world's most valuable dataset for fine-tuning foundation models. We capture exactly how models misunderstand constraints and how to correct them.
2. **Interruptibility:** Because the plan is discrete and typed, human operators can seamlessly pause a swarm, edit the `MissionTxContract` JSON by hand to bypass a blocked constraint, and resume the swarm without breaking the agent's context.

**Pre-emptive Defense Against Objections:**
*   *Objection 1: This is too bureaucratic and will paralyze the agents with red tape.*
    *Defense:* Agents are currently paralyzed by silent failures and trailing errors. Front-loading the constraints actually *increases* velocity because agents stop running into walls halfway through a task.
*   *Objection 2: It overlaps with existing orchestration tools like `ntm` or `beads`.*
    *Defense:* `ntm` handles tmux pane layouts; `beads` handles issue tracking. The Steering Loop is the missing *runtime* glue that connects an issue to a safe pane execution. It is the compiler for agent actions.

**Honest Residual Concerns:**
If the Steering Loop requires the agent to perfectly articulate its goal upfront, it might break highly exploratory "archaeology" workflows where the agent doesn't know the goal until it runs a few commands. We must ensure the loop allows for "explore" vs "execute" modes.

*Has my mind changed?* Yes. I initially critiqued this as creating conflicting orchestration abstractions. But reframed as a "Preflight Compiler" for agent intentions, it perfectly aligns with FT's fail-closed, policy-first doctrine.