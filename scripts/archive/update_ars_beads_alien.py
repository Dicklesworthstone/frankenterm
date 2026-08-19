import subprocess
import json

def run_br(*args):
    cmd = ["br", "--lock-timeout", "5000"] + list(args)
    result = subprocess.run(cmd, capture_output=True, text=True, check=True)
    return result.stdout.strip()

epic_desc = """## Background & Justification
FrankenTerm orchestrates agents via conscious, pre-written workflows. When a novel error is encountered, it routes through an expensive, high-latency LLM loop. The ARS Engine ('Swarm Immune System') provides a 'subconscious' layer to bypass this.

However, heuristic macro-recording is dangerous at scale. To build a truly world-class, enterprise-grade immune system, we must elevate ARS from empirical heuristics to **formal mathematical guarantees** (Alien Artifact Coding) and **extreme performance** (O(1) execution paths).

This Epic defines a PAC-Bayesian bounded, mathematically rigorous Swarm Immune System. It learns minimal fix sequences via Information Theory, mathematically proves safety via Symbolic Execution, and guarantees nanosecond latency via Finite State Transducers.

## Goals & Intentions
- **Zero-Shot Swarm Scaling:** Inoculate the 200+ agent swarm instantly upon resolution of a novel error.
- **Microsecond Latency Collapse:** Execute reflexes in < 1ms using an FST (Finite State Transducer) cache, bypassing the LLM entirely.
- **Formal Safety Guarantees:** Use lightweight Symbolic Execution on the AST to mathematically prove a synthesized reflex cannot mutate files outside the authorized CWD or leak secrets.
- **Minimum Description Length (MDL) Extraction:** Use Information Theory to isolate the absolute minimal sequence of keystrokes required to recover state, eliminating all agent "flailing" or noise.
- **Anytime-Valid Drift Detection:** Replace fragile moving averages with e-values and Conformal Prediction to formally bound the false-alarm rate ($\alpha$) of reflex demotion.
- **Complete Explainability (Galaxy Brain):** Every execution generates an Evidence Ledger with explicit Bayes Factors. `ft why` visualizes the exact mathematical rationale for the action.

## High-Level Execution Plan
1. **The Infection (BOCPD Trigger):** Identify error regime shifts using Bayesian Online Change-Point Detection and SIMD-accelerated MinHash.
2. **The Extraction (MDL & Symbolic Safety):** Isolate the minimal fix using MDL, redact secrets, and formally prove mutation bounds via Symbolic Execution.
3. **The Inoculation (FST Compilation):** Compile the generalized templates into an FST for O(1) evaluation, injecting snapshot/verification steps and Evidence Ledgers.
4. **The Reflex (Conformal Execution):** Intercept errors via the FST, evaluate e-values for semantic drift, and control blast radius.
5. **Persistence & UX (Galaxy Brain):** Durable GitOps federation, auto-pruning, and "Galaxy Brain" UX for operator trust and auditability.
"""

t1_desc = """## Objective
Establish the `ReflexSynthesizer` using formal mathematical state tracking. Instead of relying on crude regex triggers, use Bayesian Online Change-Point Detection (BOCPD) to formally identify regime shifts from "Healthy" to "Error".

## Subtasks
- **BOCPD Integration:** Implement a streaming Bayesian classifier to detect when the terminal output entropy/structure shifts to an error state, calculating $P(	ext{Error} | 	ext{TerminalStream})$.
- **SIMD-Accelerated MinHash:** When an error regime is detected, compute its MinHash LSH signature using SIMD intrinsics to ensure the tracker never blocks the main PTY read loop.
- **Lock-Free State Ring:** Implement a wait-free, bounded ring buffer to track the last $N$ state transitions per `pane_id` without locks or allocations on the hot path.
- **Duration & Context Snapshotting:** Capture the exact CWD, environment variables, and execution duration (with $\mu s$ precision) at the exact point the BOCPD probability crosses the confidence threshold.

## Considerations & Future-Proofing
The tracker must operate in $O(1)$ amortized time. By using BOCPD, we mathematically eliminate false-positive error triggers caused by random warning logs that don't actually halt agent progress.
"""

t1_ac = """1. **Isomorphism & Performance:** Profiling (`cargo flamegraph`) proves the lock-free ring buffer and SIMD MinHash add < 50µs overhead to the PTY read path.
2. **Mathematical Rigor:** Unit tests verify the BOCPD algorithm correctly identifies regime shifts in historical terminal output with a bounded false-positive rate.
3. **Safety:** E2E tests prove context snapshotting (CWD/Env) is absolutely perfectly aligned with the regime shift boundary.
4. **Telemetry:** Prometheus metrics correctly track `bocpd_confidence_levels` and `ars_tracker_dropped_states`."""

t2_desc = """## Objective
Isolate the absolute minimal, mathematically safe command sequence required to transition the terminal back to a healthy state, generalizing it into a robust template.

## Subtasks
- **Minimum Description Length (MDL) Extraction:** Instead of simple edit distance, calculate the Kolmogorov complexity approximation of the recovery. Extract *only* the sequence of keystrokes that maximally compressed the error state back to the healthy state, discarding all intermediate agent flailing or failed retries.
- **Lightweight Symbolic Execution (Safety Guard):** Parse the extracted command AST. Run a fast symbolic execution pass to evaluate side effects. **Formally prove** the command does not traverse outside the captured CWD (rejecting `rm -rf ../*`) and does not leak captured entropy.
- **Secret & PII Redaction:** Run a highly optimized Aho-Corasick automaton over the sequence to unconditionally redact high-entropy secrets or PII.
- **Parameter Generalization & PAC Bounds:** Generalize parameters (e.g., file paths) using regex. Generate safety bounding regexes for the parameters and calculate the PAC-Bayesian bound on the risk of the generalization failing in similar contexts.

## Considerations & Future-Proofing
MDL extraction ensures we synthesize the *cure*, not the agent's *panic*. Symbolic Execution provides a formal guarantee that the machine won't write a virus, vastly increasing operator trust over simple string-matching blocklists.
"""

t2_ac = """1. **MDL Verification:** Unit tests prove the MDL algorithm successfully isolates a single `npm install` fix out of a simulated trace containing 5 failed typo attempts, achieving minimal description length.
2. **Symbolic Safety:** The Symbolic Execution engine mathematically rejects path traversal (`../../`) and recursive destructive commands (`rm -rf /`), emitting a formal safety proof failure.
3. **Redaction:** Aho-Corasick scanner guarantees 100% redaction of injected mock AWS keys in < 1ms.
4. **Parameter Bounds:** Generalization correctly templates variables and attaches strict Regex bounds (e.g., `^[a-zA-Z0-9_./-]+$`)."""

t3_desc = """## Objective
Compile the safe, minimal template into an ultra-fast execution artifact and an immutable Evidence Ledger.

## Subtasks
- **Finite State Transducer (FST) Compilation:** Compile the MinHash signatures and trigger rules of all active reflexes into a Finite State Transducer. This guarantees that checking an incoming error against the entire ARS database takes $O(Length)$ time, completely independent of the number of reflexes stored.
- **Dynamic YAML Generation:** Construct the `WorkflowDescriptor` injecting `SnapshotTerminal` and `WaitFor` verification steps.
- **Dynamic Timeout via Expected Loss:** Calculate the dynamic timeout using an expected loss minimization function based on the tracked duration: $\arg\min_t L(t) = 	ext{Cost(Hang)} \cdot P(T > t) + 	ext{Cost(PrematureKill)} \cdot P(T \le t)$.
- **Evidence Ledger Generation:** Append a cryptographic Evidence Ledger to the compiled reflex, detailing the exact Bayes Factors, MDL scores, and Symbolic Execution proofs that justified its synthesis.

## Considerations & Future-Proofing
FST compilation is an extreme software optimization that allows the immune system to scale to millions of known errors without ever slowing down the terminal processing loop.
"""

t3_ac = """1. **Performance:** Benchmarks (`hyperfine` / `cargo bench`) prove the FST lookup evaluates against 100,000 mock reflex signatures in < 100µs.
2. **Compilation Integrity:** YAML serialization unit tests verify structural perfection, correct dynamic timeout calculations via the loss function, and snapshot/verification injections.
3. **Evidence Ledger:** The generated reflex contains a verifiable Evidence Ledger documenting the formal proofs of safety and minimal extraction."""

t4_desc = """## Objective
Execute reflexes at microsecond latency while maintaining strict blast radius control and anytime-valid semantic drift detection.

## Subtasks
- **O(1) Interception:** Wire the FST evaluation into the hot path. If an error matches, halt LLM dispatch immediately.
- **Conformal Drift Detection (e-values):** Replace arbitrary moving averages. Use e-values to perform continuous, anytime-valid testing of the reflex's success rate against a null hypothesis of degradation. If the e-value exceeds $1/\alpha$ (e.g., $\alpha = 0.01$), formally declare semantic drift and demote the reflex, strictly bounding the false-alarm rate.
- **Reflex Evolution:** If demoted, let the agent resolve the drifted error. Synthesize `v2` via MDL, deprecate `v1`, and record the transition in the Evidence Ledger.
- **Blast Radius Control:** Enforce a token-bucket rate limit on reflex concurrency across the swarm to contain unknown unknowns.
- **Context Evaluation:** Evaluate the target pane's CWD/Env against the reflex's required context bounds.

## Considerations & Future-Proofing
By using e-values, the system never demotes a reflex due to "bad luck" (statistical noise). It only demotes when there is formal mathematical proof that the underlying environment has shifted (Semantic Drift).
"""

t4_ac = """1. **Anytime-Valid Demotion:** Unit tests prove the e-value drift detector correctly bounds false-positive demotions to exactly the configured $\alpha$ level over 10,000 simulated executions.
2. **Evolution:** Integration tests verify a demoted `v1` reflex is correctly superseded by a newly synthesized `v2` reflex when the agent discovers the updated fix.
3. **Blast Radius:** Token-bucket rate limiting strictly prevents swarm-wide cascading executions in simulated stress tests.
4. **Context Gates:** The reflex is hard-rejected if the target pane's environment variables violate the compiled context preconditions."""

t5_desc = """## Objective
Provide beautiful, "Galaxy Brain" UX for operators, historical replay validation, and GitOps federation.

## Subtasks
- **Galaxy Brain UX (`ft why`):** When operators query `ft why <execution_id>`, do not just show logs. Render a beautiful terminal UI card displaying the Evidence Ledger: the Bayes Factor equation, the Symbolic Execution proof, and the e-value drift chart. Make the sophisticated math accessible and awe-inspiring.
- **Historical Replay Validation:** Before promoting any reflex to Active, dry-run its FST logic against historical SQLite `output_segments` to mathematically prove it *would* have worked on past incidents.
- **GitOps Federation:** Serialize Active reflexes (with their Evidence Ledgers) to disk and automatically sync them to a central Git repository, allowing cross-swarm immunity sharing.
- **Auto-Pruning:** Eradicate reflexes whose e-values collapse to 0.

## Considerations & Future-Proofing
The "Galaxy Brain" UI builds profound trust. Operators don't just see that a machine did something; they see the exact mathematical proof of *why* it was safe and optimal to do so.
"""

t5_ac = """1. **Galaxy Brain UX:** `ft why` correctly parses the Evidence Ledger and renders a structured, visually distinct TUI card containing the Bayesian logic and safety proofs without panicking.
2. **Replay Validation:** Integration tests verify a newly synthesized reflex is dry-run against a mock historical SQLite session, blocking promotion if the historical verification fails.
3. **GitOps:** E2E tests confirm promoted reflexes are durably serialized to disk and successfully trigger the local Git sync hook.
4. **Auditability:** All actions emit JSONL structured logs containing the full mathematical context of the decisions made."""

print("Updating Epic...")
run_br("update", "ft-tkpgy", "--description", epic_desc)

print("Updating Task 1...")
run_br("update", "ft-tkpgy.1", "--title", "[ARS] Trigger: BOCPD & SIMD Lock-Free Tracking", "--description", t1_desc, "--acceptance-criteria", t1_ac)

print("Updating Task 2...")
run_br("update", "ft-tkpgy.2", "--title", "[ARS] Extraction: MDL Sequencing & Symbolic Safety Proofs", "--description", t2_desc, "--acceptance-criteria", t2_ac)

print("Updating Task 3...")
run_br("update", "ft-tkpgy.3", "--title", "[ARS] Compilation: FST Generation & Evidence Ledgers", "--description", t3_desc, "--acceptance-criteria", t3_ac)

print("Updating Task 4...")
run_br("update", "ft-tkpgy.4", "--title", "[ARS] Execution: O(1) Interception & Conformal Drift Detection", "--description", t4_desc, "--acceptance-criteria", t4_ac)

print("Updating Task 5...")
run_br("update", "ft-tkpgy.5", "--title", "[ARS] UX & Federation: Galaxy Brain Explainability & GitOps", "--description", t5_desc, "--acceptance-criteria", t5_ac)

print("Syncing...")
run_br("sync", "--flush-only")
print("All ARS beads upgraded to Alien Artifact & Extreme Optimization standards.")
