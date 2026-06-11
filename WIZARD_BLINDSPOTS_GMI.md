# Part 2: The Blind Spot Probe

After reviewing all 45 ideas across the three models, evaluating the cross-scores, and engaging in the steelman exercise, a glaring realization emerges: **all three models suffered from the same structural blind spots.** We were trapped by the system's existing analogies (terminals, operating systems, kubernetes).

Here are 4 genuinely new ideas that expose what we collectively missed, why nobody thought of them, and why they matter.

---

## Blind Spot 1: The Economic Circuit Breaker (Token-Burn Policy Engine)
**The Gap Analysis:** We obsessed over capacity governance—CPU, RAM, RCH workers, SQLite locks, and network bandwidth. But we completely ignored the most dangerous, real-world constraint of a 200-agent swarm: **API Cost**.
**The New Idea:** Integrate an "Economic Governor" directly into the Policy Engine and Tx layer. Agents must attach a `token_budget` to their `MissionTxContract`. FrankenTerm monitors the output length and prompt injection size. If an agent gets stuck in a loop and begins burning $10/hour in Claude 3.5 Sonnet API credits without advancing the Mission state, the Economic Governor trips a `HardStop`.
**Why Nobody Thought of It:** We treat FT as a local operating system. Operating systems don't charge you per bash command. But AI agents run on metered cognitive fuel.
**Why it Matters:** A single runaway recursive loop in a 200-pane swarm can bankrupt an operator overnight. Financial safety is just as critical as filesystem safety.

---

## Blind Spot 2: Terminal-Bypass RPC (The "Agent Subspace")
**The Contrarian Take:** All 45 ideas implicitly assumed that because FrankenTerm is a terminal multiplexer, agents must communicate with *each other* via the terminal (e.g., reading pane output, sending MCP commands).
**The New Idea:** Provide a PTY-bypass RPC channel. When Agent A in Pane 1 needs to ask Agent B in Pane 2 for a code review, it shouldn't print JSON to its terminal for FT to scrape and route. It should use a dedicated MCP tool (`wa://subspace/rpc`) to send a serialized payload directly to Pane 2's event loop. FT acts as the message broker, policy-gating the RPC call, but the payload never hits the terminal emulator's render buffer.
**Why Nobody Thought of It:** We were too anchored to the concept of the "Terminal." We forgot that while the terminal is the interface to the *host*, it is a terrible interface for agent-to-agent lateral coordination.
**Why it Matters:** Terminal scrollback is incredibly noisy and token-heavy. Bypassing the PTY for inter-agent comms reduces token usage by orders of magnitude, prevents rendering glitches, and allows structured, typed data exchange between swarms.

---

## Blind Spot 3: The Pre-Approval Cross-Examination (Reverse Turing Gate)
**The Synthesis Insight:** We all loved the `RequireApproval` policy flow (Idea: Intervention Console). But when a human is asked to approve a dangerous command, they are only given the diff and the command string.
**The New Idea:** When a Tx is paused for `RequireApproval`, the human operator can open a direct, side-channel chat with the paused agent's active context window. The operator can literally ask, *"Why did you choose `rm -rf` instead of `git clean` here?"* The agent responds, explaining its reasoning, *before* the operator clicks Approve or Deny.
**Why Nobody Thought of It:** We treated paused processes like Linux `SIGSTOP`—frozen and mute. We forgot that the intelligence driving the process is still awake and capable of dialogue.
**Why it Matters:** Reading a diff doesn't tell you the agent's intent. Cross-examining the agent builds human trust and allows the operator to correct the agent's mental model without aborting the entire Mission.

---

## Blind Spot 4: Ephemerality & The Swarm Janitor (State Rot)
**The Gap Analysis:** We focused heavily on *retaining* state—Replay Studios, Provenance Ledgers, CASS Memory, and Timelines. We ignored the massive amount of garbage a 200-agent swarm generates.
**The New Idea:** The "Swarm Janitor" Protocol. Agents are forced to tag every file they create, git stash they make, or test database they spin up with an `ephemeral_lease` linked to their `TxIntent`. When the Mission completes (or fails and compensates), FT's Janitor daemon automatically sweeps the repository and filesystem, deleting any leftover artifacts tied to that lease.
**Why Nobody Thought of It:** We were focused on the "happy path" of creating software. We ignored the reality that AI agents are messy workers that leave temporary files, broken git branches, and orphaned processes everywhere.
**Why it Matters:** In a long-running swarm, "state rot" eventually paralyzes the agents. They get confused by old `test_db_v4.sqlite` files left by their siblings. Garbage collection is mandatory for horizontal scaling.