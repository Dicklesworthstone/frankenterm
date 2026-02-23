import subprocess

def run_br(*args):
    cmd = ["br", "--lock-timeout", "5000", "--no-auto-flush"] + list(args)
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, check=True)
        return result.stdout.strip()
    except subprocess.CalledProcessError as e:
        print(f"Error running command: {' '.join(cmd)}")
        print(f"STDOUT: {e.stdout}")
        print(f"STDERR: {e.stderr}")
        raise

# Map of existing tasks
tasks = {
    "T1": "ft-tkpgy.1",
    "T2": "ft-tkpgy.2",
    "T3": "ft-tkpgy.3",
    "T4": "ft-tkpgy.4",
    "T5": "ft-tkpgy.5"
}

# --- Task 1 Subtasks ---

t1_1_title = "[ARS][T1.1] Implement Lock-Free Wait-Free State Ring Buffer"
t1_1_desc = """## Background & Justification
To track terminal state transitions without impacting the ultra-low latency requirements of the PTY read path, we cannot use standard Mutex-backed collections.
We need a lock-free, wait-free ring buffer (sliding window) that tracks the last `N` state events per `pane_id`.

## Considerations
This structure must gracefully drop stale events to avoid memory leaks. It should be bounded in size, pre-allocated, and use atomic operations for head/tail pointers.
"""
t1_1_ac = """1. Unit tests verify the ring buffer under highly concurrent read/write loads using `loom` or `shuttle`.
2. Memory profiling proves zero allocations on the hot path after initialization.
3. Overflows cleanly overwrite the oldest entries without panics or memory leaks.
4. Emits StatsD/Prometheus metrics for `dropped_states` and `ring_utilization`."""

t1_2_title = "[ARS][T1.2] Integrate SIMD-Accelerated MinHash for Terminal Output"
t1_2_desc = """## Background & Justification
Terminal output is noisy. We need a way to cluster identical semantic errors even if timestamps or PIDs change.
MinHash Locality-Sensitive Hashing (LSH) provides this, but standard implementations are too slow for the PTY path.

## Considerations
Use AVX2/NEON SIMD intrinsics to accelerate the k-hash generation over terminal output segments. This creates a highly robust `ClusterID` in microseconds.
"""
t1_2_ac = """1. Benchmarks (`criterion`) prove SIMD MinHash executes in < 50µs for typical terminal segments.
2. Unit tests verify clustering correctness: two error traces differing only by a timestamp resolve to the same MinHash signature.
3. Fallback logic exists for non-SIMD architectures (e.g., standard scalar hashing), though performance drops."""

t1_3_title = "[ARS][T1.3] Implement Streaming Bayesian Online Change-Point Detection (BOCPD)"
t1_3_desc = """## Background & Justification
We need formal mathematical proof of a regime shift from "Healthy" to "Error" rather than crude regex matching (which suffers from false positives).
BOCPD calculates $P(	ext{RegimeShift} | 	ext{Stream})$ at each timestep.

## Considerations
The BOCPD model should track output entropy, exit codes, and error pattern density. When the posterior probability of an error regime exceeds $1-\alpha$, it emits an `Infection` trigger to the ARS pipeline.
"""
t1_3_ac = """1. Unit tests against a historical corpus of terminal traces prove BOCPD detects the exact line where an error began.
2. False Positive Rate (FPR) is mathematically bounded and verified empirically over 1,000 mock healthy traces.
3. Calculates posteriors incrementally in $O(1)$ time per new terminal segment."""

t1_4_title = "[ARS][T1.4] Context & Duration Snapshotting Mechanics"
t1_4_desc = """## Background & Justification
When BOCPD signals a regime shift, or `OSC 133` signals recovery, we must precisely snapshot the environment (CWD, Env Vars) and track the execution duration.

## Considerations
The context snapshot is critical for future evaluation gates. If the environment changes, the reflex may be invalid. Duration tracking (with $\mu s$ precision) feeds the dynamic timeout calculator.
"""
t1_4_ac = """1. Integration tests verify `OSC 133` boundaries correctly slice the elapsed wall-clock time of the recovery command.
2. Environment snapshotting deep-copies CWD and critical ENV vars without blocking the PTY thread.
3. Emits `ReflexTrace` structured logs containing the `pane_id`, `cluster_id`, duration, and context dump."""

# --- Task 2 Subtasks ---

t2_1_title = "[ARS][T2.1] Minimum Description Length (MDL) Command Extraction"
t2_1_desc = """## Background & Justification
Agents flail. They type `ls`, typo commands, and try things that fail. 
We must extract only the *minimal* causal sequence of commands that successfully transitioned the state, discarding the noise.
We use Minimum Description Length (Kolmogorov complexity approximation) rather than simple edit distance.

## Considerations
The algorithm must scan the input buffer history between Infection and Recovery, utilizing `OSC 133` to identify discrete command blocks. It isolates the shortest logical pipeline that correlates with the `ProcessExit(0)`.
"""
t2_1_ac = """1. Unit tests prove the algorithm extracts `npm install` and discards `npn intsall` and `ls -la` from a messy simulated trace.
2. Supports multi-command pipelines (e.g., extracting `rm -rf dist && build` as a single logical unit).
3. Evaluates in < 5ms for traces containing up to 100 intermediate commands."""

t2_2_title = "[ARS][T2.2] High-Speed Secret & PII Redaction via Aho-Corasick"
t2_2_desc = """## Background & Justification
Extracted commands might inadvertently contain leaked secrets (AWS keys, passwords). These must never be cached or federated.

## Considerations
Implement an Aho-Corasick automaton loaded with hundreds of known high-entropy token patterns. It runs over the MDL-extracted command. If a secret is found, the ARS pipeline for this cluster is permanently aborted (we do not attempt to scrub and guess, we simply refuse to learn an unsafe reflex).
"""
t2_2_ac = """1. Aho-Corasick scanner evaluates in < 1ms.
2. Hard-aborts ARS synthesis if mock AWS keys, JWTs, or Slack tokens are detected in the command string.
3. Unit tests verify zero false negatives against a standardized secret-corpus."""

t2_3_title = "[ARS][T2.3] Lightweight AST Symbolic Execution for Traversal/Destruction Safety"
t2_3_desc = """## Background & Justification
A regex blocklist is not enough. To formally prove a reflex is safe, we parse its Abstract Syntax Tree (AST) using a fast shell lexer and symbolically execute it against a mock filesystem tree.

## Considerations
We must mathematically prove the command does not mutate paths outside the snapshotted CWD (e.g., rejecting `rm -rf ../*`) and does not invoke catastrophic binaries (`mkfs`, `dd`).
"""
t2_3_ac = """1. The AST lexer correctly parses standard `sh`/`bash` syntax.
2. Symbolic execution engine emits a formal safety proof failure if it detects directory traversal or recursive deletion targeting root.
3. Safely passes benign commands (e.g., `cargo build`, `npm install`)."""

t2_4_title = "[ARS][T2.4] Parameter Generalization and PAC-Bayesian Bounds Generation"
t2_4_desc = """## Background & Justification
A hardcoded fix for `/app/src/main.rs` is useless if the error happens in `/app/src/lib.rs`. We must generalize parameters using regex capture groups.
Furthermore, we must generate strict Regex safety bounds for the variables (PAC-Bayesian bounded) to prevent injection vulnerabilities.

## Considerations
Compare the error text against the extracted command. If substrings match (e.g., filenames, line numbers), convert them to Jinja templates `{{cap.file}}`. Automatically attach a safety constraint like `^[a-zA-Z0-9_./-]+$`.
"""
t2_4_ac = """1. Correctly identifies and templates dynamic variables shared between the MinHash cluster sample and the MDL command.
2. Auto-generates regex bounds that strictly reject path traversals, wildcards, or subshell injections (`$(...)`, `` `...` ``).
3. Provides trace logs documenting the exact generalization map and bounded constraints."""

# --- Task 3 Subtasks ---

t3_1_title = "[ARS][T3.1] Expected Loss Dynamic Timeout Calculator"
t3_1_desc = """## Background & Justification
Reflexes must not hang. We use the wall-clock execution duration captured in T1.4 to calculate a dynamic timeout.
Using an expected loss minimization function: $\arg\min_t L(t) = 	ext{Cost(Hang)} \cdot P(T > t) + 	ext{Cost(PrematureKill)} \cdot P(T \le t)$.

## Considerations
Allows the system to derive an optimal timeout (e.g., 2.5x the observed duration, clamped between 500ms and 60s) formally rather than using magic numbers.
"""
t3_1_ac = """1. Unit tests calculate the optimal timeout given a distribution of observed durations and specific cost weights.
2. Clamps correctly to minimum (500ms) and maximum (60s) bounds.
3. Timeout value is made available to the YAML generation step."""

t3_2_title = "[ARS][T3.2] Evidence Ledger Generation & Cryptographic Structuring"
t3_2_desc = """## Background & Justification
Every learned reflex must carry a verifiable proof of *why* it was synthesized.
The Evidence Ledger bundles the BOCPD Bayes Factors, the MDL extraction score, the Symbolic Execution AST safety proof, and the parameter bounds into a cryptographic struct.

## Considerations
This ledger will be serialized alongside the YAML. It provides the data payload for the "Galaxy Brain" UX (`ft why`).
"""
t3_2_ac = """1. Defines the `EvidenceLedger` struct with serialization (JSON/YAML).
2. Successfully aggregates data from T1, T2, and T3 pipelines into a unified, immutable record.
3. Includes a SHA256 hash of the inputs to prove the ledger hasn't been tampered with."""

t3_3_title = "[ARS][T3.3] Dynamic YAML Compilation & Injection (Snapshot/WaitFor)"
t3_3_desc = """## Background & Justification
Compile the templated command, the timeout, and the constraints into a `WorkflowDescriptor`.

## Considerations
Must auto-inject a `DescriptorStep::SnapshotTerminal` before the action, and a `DescriptorStep::WaitFor` (or check exit code) after.
Also handles the **Operator Override Gate**: abort compilation if an operator has locked a manual YAML for this cluster.
"""
t3_3_ac = """1. Emits schema-valid `WorkflowDescriptor` YAML containing the injected pre/post steps.
2. Honors the operator override lock, cleanly aborting synthesis and logging the bypass.
3. Variables mapped via Jinja match the generalized parameters from T2.4."""

t3_4_title = "[ARS][T3.4] O(1) Finite State Transducer (FST) Compiler for Reflex Signatures"
t3_4_desc = """## Background & Justification
As the ARS database grows to millions of reflexes across the federation, linear or hash-map lookups might stall.
Compiling the MinHash signatures and trigger contexts into a Finite State Transducer (FST) guarantees $O(K)$ lookup time where $K$ is the length of the query, utterly independent of the total number of stored reflexes.

## Considerations
Whenever a new reflex is promoted to Active, the background compiler thread rebuilds the FST and hot-swaps it atomically via an `ArcSwap`.
"""
t3_4_ac = """1. `cargo bench` proves lookup time is bounded by $O(K)$ and unaffected by scaling the reflex cache from 10 to 1,000,000 entries.
2. Background thread successfully rebuilds and atomically swaps the FST without locking readers.
3. Correctly maps MinHash queries to their respective `WorkflowDescriptor` IDs."""

# --- Task 4 Subtasks ---

t4_1_title = "[ARS][T4.1] FST Interception & PTY Hot-Path Integration"
t4_1_desc = """## Background & Justification
Wire the compiled FST directly into the PTY event loop. When BOCPD identifies an error regime and generates a MinHash, query the FST.

## Considerations
If a match is found, immediately halt the standard LLM dispatch pipeline. We are entering the subconscious execution mode.
Evaluate the Context Gate (do the target pane's CWD/Env match the reflex's required bounds?). If context fails, gracefully fall back to LLM.
"""
t4_1_ac = """1. Integration tests prove standard LLM dispatch is bypassed when the FST registers a hit.
2. Context Gate successfully blocks execution and falls back if the pane's CWD differs from the reflex requirements.
3. Hot-path overhead remains < 1ms."""

t4_2_title = "[ARS][T4.2] Swarm-Wide Token-Bucket Blast Radius Controller"
t4_2_desc = """## Background & Justification
If a reflex is subtly wrong, it must not destroy 200 agents simultaneously. We need swarm-wide concurrency limits.

## Considerations
Implement a distributed Token-Bucket rate limiter (or local shard equivalent). A newly learned reflex might be capped at `5 executions / minute / swarm`. Excess requests fall back to LLM.
"""
t4_2_ac = """1. Rate limiter accurately caps concurrency according to configured limits.
2. E2E test simulating 50 simultaneous identical errors allows exactly 5 reflex executions, routing 45 to LLM fallback.
3. Emits `ars_rate_limited` Prometheus metrics."""

t4_3_title = "[ARS][T4.3] Anytime-Valid Conformal Drift Detection (e-values)"
t4_3_desc = """## Background & Justification
Instead of arbitrary moving averages for success rate, use formal e-values (anytime-valid testing) to detect semantic drift (e.g., underlying CLI updated, reflex now fails).

## Considerations
The e-value formally bounds the false-alarm rate $\alpha$. If the e-value exceeds $1/\alpha$, we have mathematical proof the reflex has drifted. The reflex is instantly demoted to Shadow Mode.
"""
t4_3_ac = """1. Unit tests prove the e-value correctly bounds false-positive demotions to the configured $\alpha$ level over 10,000 simulations.
2. Automatically demotes reflexes and emits a high-priority warning log when semantic drift is proven.
3. Integrates the e-value tracking state into persistent storage."""

t4_4_title = "[ARS][T4.4] Reflex Evolution Engine (v1 -> v2 synthesis mapping)"
t4_4_desc = """## Background & Justification
When a reflex is demoted via T4.3, the agent resolves the new error via LLM. The ARS must recognize this as an evolution.

## Considerations
Synthesize the new sequence, tag it as `ars_version: 2`, map its parent to `v1`, and promote it. Deprecate `v1` completely. This allows the immune system to continuously adapt without human intervention.
"""
t4_4_ac = """1. Integration tests prove that a demoted `v1` reflex is correctly superseded by a newly synthesized `v2` reflex.
2. Version lineage is durably recorded in the Evidence Ledger and the YAML metadata.
3. `v1` is prevented from executing once `v2` is promoted."""

# --- Task 5 Subtasks ---

t5_1_title = "[ARS][T5.1] Historical SQLite Replay Validation Harness"
t5_1_desc = """## Background & Justification
Before promoting any reflex to Active, the subconscious should "dream". Dry-run the FST logic and AST commands against historical `output_segments` to mathematically prove it *would* have worked on past incidents.

## Considerations
Hooks into Epic `ft-og6q6` (Deterministic Replay). If the historical replay fails, the reflex is marked flawed and discarded.
"""
t5_1_ac = """1. Integration tests verify a newly synthesized reflex is evaluated against a mock historical session.
2. Blocks promotion if historical verification indicates the AST would fail or cause a divergence.
3. Emits `ars_historical_validation_status` telemetry."""

t5_2_title = "[ARS][T5.2] 'Galaxy Brain' Evidence Ledger UI for `ft why`"
t5_2_desc = """## Background & Justification
Operators need to trust the subconscious. `ft why <execution_id>` should render a beautiful terminal UI card.

## Considerations
The card must display the Evidence Ledger: the BOCPD Bayes Factor, the MDL score, the Symbolic Execution safety proof, and the e-value drift chart. Make the math accessible and awe-inspiring.
"""
t5_2_ac = """1. `ft why` parses the serialized Evidence Ledger without panicking.
2. Renders a visually distinct TUI card displaying the mathematical logic (Bayes factors, AST bounds).
3. UX tests confirm text wrapping and styling work across standard terminal widths."""

t5_3_title = "[ARS][T5.3] Disk Serialization & Auto-Pruning Garbage Collector"
t5_3_desc = """## Background & Justification
Reflexes must survive restarts. We serialize the FST sources and YAMLs to disk (`~/.config/ft/reflexes/`).

## Considerations
Include a garbage collector that permanently deletes/blacklists reflexes whose e-values collapse to 0 or repeatedly fail replay validation. Implement CLI tools (`ft reflexes list`, `ft reflexes prune`).
"""
t5_3_ac = """1. Serialization cleanly writes/reads from disk, gracefully handling schema version migrations.
2. Auto-pruner correctly identifies and deletes blacklisted/stale reflexes.
3. CLI commands accurately reflect the disk cache state."""

t5_4_title = "[ARS][T5.4] GitOps Cross-Swarm Federation & Webhook Dispatcher"
t5_4_desc = """## Background & Justification
A fix discovered by the Dev swarm should instantly inoculate the CI/CD swarm. We achieve this by syncing Active reflexes to a central Git repository.

## Considerations
Emit Slack/Discord webhooks for critical state changes (e.g., "🧬 ARS learned v2 reflex for Cluster X"). The GitOps hook pushes YAMLs and Evidence Ledgers as standard PRs or direct commits.
"""
t5_4_ac = """1. Unit tests verify the GitOps hook correctly formats a commit and pushes to a mock repository.
2. Webhook dispatcher fires non-blocking HTTP requests with correct JSON payloads upon Active promotion.
3. Pull logic (`ft reflexes sync`) correctly imports new federated reflexes and triggers an FST rebuild."""

# --- Script Execution ---

tasks_data = [
    (tasks["T1"], t1_1_title, t1_1_desc, t1_1_ac),
    (tasks["T1"], t1_2_title, t1_2_desc, t1_2_ac),
    (tasks["T1"], t1_3_title, t1_3_desc, t1_3_ac),
    (tasks["T1"], t1_4_title, t1_4_desc, t1_4_ac),
    
    (tasks["T2"], t2_1_title, t2_1_desc, t2_1_ac),
    (tasks["T2"], t2_2_title, t2_2_desc, t2_2_ac),
    (tasks["T2"], t2_3_title, t2_3_desc, t2_3_ac),
    (tasks["T2"], t2_4_title, t2_4_desc, t2_4_ac),
    
    (tasks["T3"], t3_1_title, t3_1_desc, t3_1_ac),
    (tasks["T3"], t3_2_title, t3_2_desc, t3_2_ac),
    (tasks["T3"], t3_3_title, t3_3_desc, t3_3_ac),
    (tasks["T3"], t3_4_title, t3_4_desc, t3_4_ac),
    
    (tasks["T4"], t4_1_title, t4_1_desc, t4_1_ac),
    (tasks["T4"], t4_2_title, t4_2_desc, t4_2_ac),
    (tasks["T4"], t4_3_title, t4_3_desc, t4_3_ac),
    (tasks["T4"], t4_4_title, t4_4_desc, t4_4_ac),
    
    (tasks["T5"], t5_1_title, t5_1_desc, t5_1_ac),
    (tasks["T5"], t5_2_title, t5_2_desc, t5_2_ac),
    (tasks["T5"], t5_3_title, t5_3_desc, t5_3_ac),
    (tasks["T5"], t5_4_title, t5_4_desc, t5_4_ac),
]

print("Generating 20 granular sub-tasks for the ARS Epic...")

prev_id = None
for i, (parent, title, desc, ac) in enumerate(tasks_data):
    # Determine dependencies:
    # 1. Blocked by parent of previous subtask if we are crossing Task boundaries (e.g. T2.1 depends on T1.4) - actually, they already have parent dependencies.
    # We will make them strictly sequential within their tracks, or just let them be children.
    # To keep it simple and perfectly ordered: each subtask depends on the previous subtask.
    deps_arg = []
    if prev_id:
        deps_arg = ["--deps", f"blocks:{prev_id}"]
        
    print(f"Creating: {title}")
    
    args_create = [
        "create", "--type", "task", "--priority", "1", "--silent",
        "--title", title,
        "--description", desc,
        "--parent", parent
    ] + deps_arg
    
    new_id = run_br(*args_create)
    print(f"  -> ID: {new_id}")
    
    args_update = [
        "update", new_id, "--acceptance-criteria", ac
    ]
    run_br(*args_update)
    
    prev_id = new_id

print("Syncing...")
run_br("sync", "--flush-only")
print("Successfully generated ultra-granular Alien Artifact execution plan.")
