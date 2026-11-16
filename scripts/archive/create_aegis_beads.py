import subprocess
import json
import os

env = os.environ.copy()
env["CARGO_TARGET_DIR"] = "/tmp/br-target"

def run_br(title, type, priority, desc, labels=None, parent=None):
    args = [
        "br", "create", "--json",
        "--title", title,
        "--type", type,
        "--priority", str(priority),
        "--description", desc
    ]
    if labels:
        args.extend(["--labels", labels])
    if parent:
        args.extend(["--parent", parent])
    
    res = subprocess.run(args, capture_output=True, text=True, env=env)
    if res.returncode != 0:
        print("Error creating " + title)
        print("STDOUT: " + res.stdout)
        print("STDERR: " + res.stderr)
        exit(1)
    
    try:
        data = json.loads(res.stdout.strip())
        return data["id"]
    except Exception as e:
        print("Failed to parse JSON: " + res.stdout.strip() + " (" + str(e) + ")")
        exit(1)

epic_desc = """## Background
Current terminal multiplexers and agent supervisors rely on standard Mutex locking for state synchronization and brittle heuristics (e.g., magic numbers, arbitrary string distances) for safety. To support massive scale (200+ parallel agents) while remaining perfectly responsive, we must move from heuristics to formal mathematics ("Alien Artifact Coding") and from lock-contention to mechanical sympathy ("Extreme Software Optimization").

## Objective
Implement the Aegis Engine: a radically optimized, mathematically formal runtime core for `ft`.
1. **Data Plane:** Zero-copy, lock-free PTY concurrency (SPMC).
2. **Compute Plane:** SIMD-accelerated byte scanning.
3. **Control Plane:** PAC-Bayesian adaptive backpressure scheduling.
4. **Semantic Plane:** Anytime-valid (e-process) entropy anomaly detection.

## Justification & Philosophy
If we want to build the most reactive and robust terminal system in the world, we must profile first, prove isomorphism, and leverage advanced probability theory. We replace "magic numbers" with statistical bounds. We replace blocking synchronization with atomic ring buffers. This elevates `ft` from a basic tool to a mathematically provable, high-performance runtime.

## Acceptance Criteria
1. The epic is fully delivered across child beads with zero loss of existing terminal capabilities.
2. Every change requires an explicit "Isomorphism Proof" documented in PRs/commits, verifying golden checksums match exactly.
3. Extensive profiling (`cargo flamegraph`, `hyperfine`) artifacts are captured and documented before and after.
4. Mathematical interventions (e-values, bounds) are fully documented, backed by test suites, and visually explained in the UI.
5. Structured logs capture all mathematical state and metrics."""

t1_desc = """## Objective
Eliminate Mutex contention on the PTY output stream via lock-free SPMC ring buffers.

## Details
**Extreme Optimization (Tier 3):** Currently, reading from the PTY and dispatching to consumers (UI, PatternEngine) involves synchronization overhead. Implement a Single-Producer Multi-Consumer (SPMC) ring buffer using atomics (or integrate `crossbeam` / `ringbuf`). The PTY reader thread is the sole producer; all downstream systems consume without locks.

## Goals
- Zero-lock contention between agent PTY writes and terminal rendering.
- Massively increased throughput for bursty command outputs.

## Acceptance Criteria
1. Capture baseline with `hyperfine` and profile with `cargo flamegraph`.
2. Implement change and re-profile to prove lock elimination in the hot path.
3. Provide an Isomorphism Proof: `sha256sum` golden checksums of PTY output prove exact data fidelity.
4. Unit tests validate concurrent read/write invariants under extreme contention."""

t2_desc = """## Objective
Maximize throughput for ANSI escape sequence and anchor detection using SIMD.

## Details
**Extreme Optimization (Tier 1):** Scanning for `\\x1b` or `\\n` byte-by-byte is a severe bottleneck during high-throughput ingest. Utilize `std::simd` or `memchr` to process 16-32 bytes simultaneously within the new SPMC buffer.

## Goals
- Accelerate the pre-processing pipeline.
- Reduce CPU cycles spent in the `PatternEngine` and terminal state machine.

## Acceptance Criteria
1. Profile hot path to confirm string scanning is a top-5 bottleneck.
2. Implement SIMD-accelerated scanning.
3. Isomorphism Proof: Golden outputs mathematically prove zero parsing regressions.
4. Benchmarks prove >2x throughput on dense logs."""

t3_desc = """## Objective
Mathematically guarantee UI responsiveness (60fps) under extreme load using PAC-Bayes bounds.

## Details
**Alien Artifact Math (Calibration):** Spikiness starves the event loop. Instead of hardcoded limits (e.g., max 10MB/s), maintain a Bayesian posterior of the UI drop-frame probability given aggregate byte rates. Compute the optimal throttling multiplier `a*` to minimize expected loss (agent velocity vs UI responsiveness). Dynamically yield the PTY read loop based on this formal bound.

## Goals
- Formal bounds on error rates and frame drops.
- Graceful degradation under load; never fails catastrophically.

## Acceptance Criteria
1. Mathematical formalization (Loss Matrix, Posteriors) documented explicitly in code comments.
2. E2E test simulating 100 active panes proving the UI maintains target FPS via telemetry.
3. Throttling engages dynamically without magic threshold configurations."""

t4_desc = """## Objective
Replace heuristic death-spiral detection with formal e-processes on Shannon entropy.

## Details
**Alien Artifact Math (Testing):** "3 failures" or "85% edit distance" are brittle. Calculate the Shannon entropy of the incoming byte stream using a sliding window. Implement an e-process to test the null hypothesis that the stream is stationary/productive. If an agent loops (spamming errors), entropy collapses and the e-value grows exponentially. When `E_t >= 1/alpha`, formally reject the null and trigger the Trauma Guard block.

## Goals
- Eliminate magic numbers entirely from loop detection.
- Provide mathematically optimal, anytime-valid detection of anomalous output.

## Acceptance Criteria
1. Algorithm implemented with explicitly stated formal error bounds (alpha value).
2. Unit tests proving the e-value spikes on repeating payloads and remains low on diverse text.
3. Integration with `CommandGate` to physically block execution upon statistical rejection."""

t5_desc = """## Objective
Expose the sophisticated mathematical state in the UI for complete explainability.

## Details
**Alien Artifact UX:** The math must be accessible. When the Aegis engine throttles an agent or blocks a loop, display a "Galaxy-Brain" overlay card in the terminal. Show the exact equation, substituted values, and a plain-English intuition (e.g., `[Aegis] E-value: 405.2 > 100. Null rejected. Loop detected.`).

## Goals
- Provide complete explainability for every mathematical intervention.
- Make sophisticated theory visible and debuggable.

## Acceptance Criteria
1. UI components built to render mathematical proofs inline.
2. Structured logs include the full Bayesian evidence ledger / e-value computation state.
3. E2E validation verifies the overlay renders without corrupting ANSI state."""

print("Creating beads...")
epic_id = run_br("[EPIC] The Aegis Engine: Lock-Free PTY Concurrency & Mathematical Robustness", "epic", 0, epic_desc, "aegis,alien-artifact,extreme-optimization")
print("Epic created: " + epic_id)

t1_id = run_br("Aegis: Data Plane - SPMC Lock-Free PTY Ring Buffer", "task", 0, t1_desc, "aegis,performance,concurrency", epic_id)
print("Task 1 created: " + t1_id)

t2_id = run_br("Aegis: Compute Plane - SIMD-Accelerated Byte Scanning", "task", 0, t2_desc, "aegis,performance,simd", epic_id)
print("Task 2 created: " + t2_id)

t3_id = run_br("Aegis: Control Plane - PAC-Bayesian Adaptive Backpressure", "task", 0, t3_desc, "aegis,math,scheduling", epic_id)
print("Task 3 created: " + t3_id)

t4_id = run_br("Aegis: Semantic Plane - Anytime-Valid Entropy Anomaly Detection", "task", 0, t4_desc, "aegis,math,safety", epic_id)
print("Task 4 created: " + t4_id)

t5_id = run_br("Aegis: Integration - Galaxy-Brain UX and Telemetry", "task", 0, t5_desc, "aegis,ux,telemetry", epic_id)
print("Task 5 created: " + t5_id)

# Dependencies
def add_dep(iss, dep):
    subprocess.run(["br", "dep", "add", iss, dep], env=env, check=True)

# Order of execution logic
add_dep(t2_id, t1_id) # SIMD scanning relies on the new buffer design
add_dep(t3_id, t1_id) # Backpressure relies on the new buffer performance
add_dep(t4_id, t2_id) # Entropy scanning relies on the efficient parsing logic
add_dep(t5_id, t3_id) # UX needs the PAC-Bayes state
add_dep(t5_id, t4_id) # UX needs the E-value state

# Sync
subprocess.run(["br", "sync", "--flush-only"], env=env, check=True)
print("Sync complete.")
