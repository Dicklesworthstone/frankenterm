#!/bin/bash

# Epic 1: Panic Eradication
EPIC1=$(br create --title "Epic: Eradicate Panic Surfaces in Library Code" --type epic --priority 1 --description "
# Background
The UBS scan identified hundreds of \`panic!\`, \`unwrap()\`, and \`expect()\` calls in library code, specifically within \`frankenterm-core\` and legacy crates. As a swarm-native terminal platform running as a daemon (\`ft watch\`), panics in library code bring down the entire control plane, severing observation and orchestration for all connected AI agents.

# Reasoning
We must uphold strict zero-panic invariants in the core library. Errors must be propagated using \`Result\` and \`thiserror\` so the runtime can recover, shed load, or cleanly restart failing subsystems without taking down the main watcher process.

# Goals
1. Replace all explicit \`panic!\`, \`todo!\`, and \`unimplemented!\` macros in \`frankenterm-core/src/\`.
2. Replace \`unwrap()\` and \`expect()\` with \`?\` propagation.
3. Handle Mutex poison errors gracefully instead of unwrapping.
4. Ensure JSON parsing (\`serde_json::from_str\`) validates and returns structured errors instead of panicking on malformed input.
")

T1_1=$(br create --title "Remove panic! macros from replay_capture.rs" --type task --priority 1 --description "
# Background
\`replay_capture.rs\` contains at least 18 explicit \`panic!\` macros (e.g., \`panic!(\"expected EgressOutput payload\")\`).

# Justification
During replay or flight-recorder egress, encountering unexpected marker types or payload structures should result in a skipped event, a logged warning, or a propagated error, but NEVER a process abort. A malformed recording file could currently crash the entire terminal platform.

# Implementation Notes
- Audit \`crates/frankenterm-core/src/replay_capture.rs\`.
- Convert functions returning these values to return \`crate::Result<T>\`.
- Use \`crate::error::ReplayError\` or similar variants.
")

T1_2=$(br create --title "Replace Mutex::lock().unwrap() poison handling" --type task --priority 1 --description "
# Background
UBS flagged 13 instances of \`Mutex::lock().unwrap()\` or \`.expect()\`, notably in \`replay_capture.rs\`.

# Justification
If a thread panics while holding a standard library Mutex, the mutex is \"poisoned\". Subsequent threads calling \`.unwrap()\` on the lock result will also panic, causing a cascading failure that takes down the process.

# Implementation Notes
- Replace \`.unwrap()\` with \`match lock.lock() { Ok(g) => g, Err(poisoned) => poisoned.into_inner() }\` to recover the guard, OR return a designated error if the subsystem cannot safely continue with potentially corrupted state.
")

T1_3=$(br create --title "Audit and remove serde_json::from_str(...).unwrap()" --type task --priority 2 --description "
# Background
There are 34 instances of JSON parsing that blindly unwrap the result.

# Justification
JSON input (especially from external agents, state files, or the wire) is fundamentally untrusted. Malformed JSON will cause a panic.

# Implementation Notes
- Audit all \`serde_json::from_str\` and \`to_string\` calls.
- Propagate errors via \`?\`.
- Add contextual error messages (e.g., using \`anyhow::Context\` or custom \`thiserror\` variants) to aid debugging when parsing fails.
")

br dep add $T1_1 $EPIC1
br dep add $T1_2 $EPIC1
br dep add $T1_3 $EPIC1


# Epic 2: Concurrency and Async Safety
EPIC2=$(br create --title "Epic: Concurrency and Async Safety" --type epic --priority 0 --description "
# Background
The architecture of \`frankenterm\` relies heavily on Tokio for asynchronous I/O and orchestration. UBS detected critical concurrency anti-patterns:
1. \`std::thread::spawn\` used without \`join()\` handles, leading to detached zombie threads.
2. Potential async lock guards (\`tokio::sync::MutexGuard\`) held across \`.await\` points.
3. Unclosed \`TcpStream\`s.

# Reasoning
In a high-throughput multi-agent orchestration system, resource leaks (zombie threads, leaked file descriptors/sockets) will eventually cause the OS to kill the process or reject new connections (e.g., \`EMFILE\`). Furthermore, holding a lock across an \`.await\` point can easily lead to deadlocks if the awaited future depends on another task that needs the same lock, or it can stall the executor thread.

# Goals
1. Ensure all \`std::thread::spawn\` calls either store the \`JoinHandle\` and join it during shutdown, or are explicitly documented and managed as detached daemon threads.
2. Refactor async functions to drop lock guards before calling \`.await\`.
3. Ensure explicit \`shutdown()\` or scoped cleanup for \`TcpStream\`s.
")

T2_1=$(br create --title "Eliminate async lock guards held across await points" --type task --priority 0 --description "
# Background
UBS warns of 3 potential async lock guards held across await points.

# Justification
Holding a \`tokio::sync::MutexGuard\` (or worse, a \`std::sync::MutexGuard\`) across an \`.await\` can stall other tasks waiting for the lock, increasing latency. If the awaited operation tries to re-acquire the lock or waits on a task that needs the lock, it deadlocks.

# Implementation Notes
- Identify the 3 locations flagged by UBS.
- Refactor the code to acquire the lock, clone/copy the necessary data, explicitly drop the guard (e.g., using a block \`{ ... }\` or \`drop(guard)\`), and then perform the \`.await\` operation.
")

T2_2=$(br create --title "Fix unjoined std::thread::spawn calls" --type task --priority 1 --description "
# Background
Numerous crates (\`mux\`, \`pty\`, \`ssh\`, \`term\`, \`legacy_rio\`, \`legacy_wezterm\`) spawn threads without retaining the \`JoinHandle\`.

# Justification
Unjoined threads can outlive the subsystem that spawned them, continuing to consume memory and CPU, or attempting to access dropped resources. During shutdown or restart sequences, this prevents clean teardown.

# Implementation Notes
- For each \`std::thread::spawn\`, capture the \`JoinHandle\`.
- If the thread is part of a long-lived service, store the handle in the service's state struct and \`join()\` it in the \`Drop\` implementation or a dedicated \`shutdown()\` method.
- If the thread is intentionally detached, use \`std::thread::Builder::new().spawn().unwrap()\` and document *why* it is detached and how its lifecycle is managed.
")

T2_3=$(br create --title "Enforce TcpStream shutdown" --type task --priority 2 --description "
# Background
\`TcpStream\` without \`shutdown()\` found in \`distributed.rs\`, \`metrics.rs\`, \`runtime_compat.rs\`.

# Justification
Relying solely on \`Drop\` for sockets can lead to lingering TIME_WAIT states or un-flushed buffers. Explicit \`shutdown(Shutdown::Both)\` ensures the TCP connection is terminated cleanly at the protocol level before the file descriptor is closed.

# Implementation Notes
- Audit the flagged locations.
- Ensure that \`stream.shutdown(...)\` is called when the connection is logically finished, before the stream is dropped.
")

br dep add $T2_1 $EPIC2
br dep add $T2_2 $EPIC2
br dep add $T2_3 $EPIC2


# Epic 3: Memory Safety and Legacy Cleanup
EPIC3=$(br create --title "Epic: Memory Safety and Legacy Cleanup" --type epic --priority 0 --description "
# Background
The project integrates legacy code from Rio and WezTerm. The UBS scan found critical memory safety hazards in these legacy areas, including \`mem::transmute\` and \`mem::forget\`. Additionally, there are \`mem::forget\` calls in \`crates/frankenterm-core/src/session_restore.rs\`.

# Reasoning
\`mem::transmute\` bypasses the compiler's type safety checks and can easily cause Undefined Behavior (UB) if the types have different sizes or layouts. \`mem::forget\` deliberately leaks memory or prevents destructors from running, which is dangerous in a long-running daemon unless strictly necessary for FFI.

# Goals
1. Remove or safely encapsulate \`mem::transmute\` usages in \`legacy_rio\` and \`legacy_wezterm\`.
2. Refactor \`mem::forget\` in \`session_restore.rs\` to use safe ownership transfer or \`ManuallyDrop\` if FFI is involved, ensuring no actual memory leaks occur in pure Rust code.
")

T3_1=$(br create --title "Refactor mem::transmute in legacy teletypewriter and vtparse" --type task --priority 0 --description "
# Background
\`mem::transmute\` is used in \`legacy_rio/rio/teletypewriter/src/windows/spsc.rs\` and \`legacy_wezterm/vtparse/src/enums.rs\`.

# Justification
Transmuting between types is highly unsafe. In \`spsc.rs\`, transmuting a buffer reference can lead to aliasing violations or invalid lifetimes. In \`vtparse\`, transmuting to enums can cause UB if the integer value doesn't perfectly match a valid enum discriminant.

# Implementation Notes
- In \`spsc.rs\`, use safe slice conversions, \`UnsafeCell\`, or \`MaybeUninit\` depending on what the code is actually trying to achieve. If it's converting raw pointers, use pointer casts (\`as\`) instead of \`transmute\`.
- In \`vtparse/src/enums.rs\`, use \`TryFrom\` or \`num_derive::FromPrimitive\` to safely convert integers to enums, rejecting invalid values instead of transmuting them.
")

T3_2=$(br create --title "Remove mem::forget from session_restore.rs" --type task --priority 1 --description "
# Background
\`mem::forget\` is used in \`crates/frankenterm-core/src/session_restore.rs:894\`.

# Justification
Leaking memory in the session restore path means every time a session is restored, the daemon's memory footprint grows permanently. This is a classic memory leak.

# Implementation Notes
- Investigate line 894 of \`session_restore.rs\`.
- Determine why \`mem::forget\` was used (often a misguided attempt to prevent a double-free or bypass a destructor).
- Use proper ownership semantics. If interacting with C APIs, use \`IntoRawFd\` / \`from_raw_fd\` or \`Box::into_raw\`. For pure Rust, restructure the code so the value is either consumed or allowed to drop naturally.
")

br dep add $T3_1 $EPIC3
br dep add $T3_2 $EPIC3


# Epic 4: Robustness and Performance Hotspots
EPIC4=$(br create --title "Epic: Robustness and Performance Hotspots" --type epic --priority 2 --description "
# Background
The UBS scan identified several code quality and performance issues:
1. Regex compilation (\`Regex::new\`) inside loops.
2. Division and modulo by variables without zero checks.
3. Hardcoded secrets heuristics triggering in \`policy.rs\`.

# Reasoning
Compiling a Regex in a loop is an O(N) operation inside an O(M) loop, leading to massive CPU waste and potential DoS vectors. Unchecked division by variables can cause panics in production. The secrets heuristic in \`policy.rs\` is likely a false positive (it's matching the regex patterns designed to *detect* secrets), but it needs verification to ensure actual secrets aren't checked in.

# Goals
1. Hoist \`Regex::new\` out of loops using \`LazyLock\` or \`OnceLock\`.
2. Add explicit non-zero guard clauses before division/modulo operations.
3. Audit and document the \`policy.rs\` secrets.
")

T4_1=$(br create --title "Hoist Regex::new out of loops" --type task --priority 2 --description "
# Background
UBS detected 2 instances of \`Regex::new\` in the codebase. If these are inside loops or hot paths, they kill performance.

# Justification
Regex compilation is expensive. It should happen exactly once per pattern.

# Implementation Notes
- Find the \`Regex::new\` calls.
- Wrap them in \`std::sync::LazyLock\` (since the project uses Rust 2024 / 1.85+) to ensure they are compiled statically at first use.
")

T4_2=$(br create --title "Guard division and modulo operations against zero" --type task --priority 2 --description "
# Background
49 warnings for division by variable and 98 for modulo by variable.

# Justification
If the divisor becomes zero (e.g., due to an empty collection, a zero-sized terminal pane, or a 0ms time delta), the program will panic.

# Implementation Notes
- Audit math operations.
- Add \`if divisor == 0 { return 0; }\` or use \`checked_div()\` / \`checked_rem()\`.
")

br dep add $T4_1 $EPIC4
br dep add $T4_2 $EPIC4

br sync --flush-only
