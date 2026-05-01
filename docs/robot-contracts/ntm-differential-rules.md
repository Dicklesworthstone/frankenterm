# NTM Differential — Normalization Rules

**Bead:** [BR-RC-ROBOT-CONTRACT.0.1] / `ft-hac7w.1.1`
**Sibling:** `crates/frankenterm-core/src/robot_ntm_differential.rs`
**Audience:** Anyone wiring a robot family into the differential harness.

## Why this document exists

The bridge plan requires that every robot family with an `ntm`
equivalent shows zero observable divergence on a fuzz corpus.
"Zero divergence" is meaningless without specifying *what is
observable* — timestamps, process IDs, ephemeral file paths, and
log line ordering all naturally diverge between two independent
runs of equivalent logic. This document is the canonical list of
fields the differential harness MUST normalize before the
assertion fires, and the categories of difference it must NEVER
normalize away (those are real bugs).

The harness in `robot_ntm_differential.rs` consumes this list
mechanically: each rule is a `(json_pointer_pattern, replacement)`
pair the harness applies to both responses before equality
comparison.

## Layered structure

The rules below are organized into three layers:

1. **Trivial drift** — values that change every run by design
   (timestamps, PIDs, monotonic counters). Always normalize.
2. **Operational drift** — values that depend on the host
   environment (paths, hostnames, user-id). Normalize when the
   harness runs in CI; preserve when the harness runs in a real
   integration test that intends to assert on host state.
3. **Real divergence** — values that *must* match exactly.
   Anything not listed above falls here by default.

The bead's acceptance criterion ("one example family consumes it
end-to-end") is satisfied by **profile** (`ft-hac7w.2`); future
families add to the rule table below as they discover new
trivial-drift fields.

## Layer 1 — Trivial drift (always normalize)

| Field name (JSON pointer pattern) | Replacement | Rationale |
| --- | --- | --- |
| `/timestamp`, `/created_at`, `/updated_at`, `/started_at`, `/completed_at`, `/last_used_at`, `/last_seen_at`, `/first_seen_at`, `/closed_at`, `/expires_at` | `"<NORMALIZED:ts>"` | Wall-clock divergence between the two runs. |
| `/duration_ms`, `/elapsed_ms`, `/duration_us`, `/elapsed_us`, `/took_ms`, `/wait_ms` | `"<NORMALIZED:duration>"` | Wall-clock-derived; depends on scheduler jitter. |
| `/pid`, `/process_id`, `/parent_pid`, `/child_pid`, `/runner_pid` | `"<NORMALIZED:pid>"` | OS-assigned process identifier. |
| `/uuid`, `/session_uuid`, `/correlation_id`, `/request_id`, `/execution_id`, `/run_id`, `/trace_id` | `"<NORMALIZED:uuid>"` | Random-generated; the harness asserts presence + format only. |
| `/hostname`, `/host_name`, `/host` | `"<NORMALIZED:host>"` | Host-machine name. |
| `/version`, `/build_sha`, `/git_sha`, `/commit_sha` | `"<NORMALIZED:version>"` | Two binaries built from different commits will differ here even when the protocol layer matches. |

## Layer 2 — Operational drift (normalize in CI; preserve elsewhere)

These rules only fire when the harness is invoked with
`mode = HarnessMode::Ci`. In `HarnessMode::HostState` mode they
are bypassed because a host-state assertion may legitimately
depend on the *real* path / user / cwd.

| Field name (JSON pointer pattern) | Replacement | Rationale |
| --- | --- | --- |
| `/cwd`, `/working_dir`, `/work_dir`, `/working_directory` | `"<NORMALIZED:cwd>"` | CI runner path differs from developer machine. |
| `/home_dir`, `/home`, `/user_home` | `"<NORMALIZED:home>"` | `$HOME` differs across runners. |
| `/temp_dir`, `/tmp`, `/runtime_dir` | `"<NORMALIZED:tmp>"` | Per-process tempdir is non-deterministic. |
| `/uid`, `/gid`, `/euid`, `/egid` | `"<NORMALIZED:uid>"` | OS-user identifier; CI runner uid ≠ developer uid. |
| `/socket_path`, `/ipc_socket_path`, `/sock` | `"<NORMALIZED:sock>"` | UNIX socket path is per-runtime. |

## Layer 3 — Real divergence (NEVER normalize)

Anything not matched by Layer 1 or 2 is a real divergence and the
harness asserts on it byte-for-byte. The bridge plan stance is:
**if a field divergence is acceptable, it MUST be in the table
above; otherwise it is a bug**.

Examples of fields that explicitly do NOT belong on this list and
**must** match exactly:

- The action's `result_type` (`continue` / `done` / `abort` / `wait_for`).
- Every field in the request envelope that the handler echoes back
  (so a corrupted echo is loud).
- Policy decision summaries (the gating decision is the contract).
- Workflow/step/sequence ordering — the harness's input stream is
  deterministic and the output ordering must be too.
- Error-code strings (e.g. `"WatcherNotRunning"`) — these are part
  of the protocol contract.

## Multi-element arrays — order semantics

For arrays in the response (e.g. event histories, audit-action
lists), the harness applies a **stable sort** before comparison
when the family contract declares the array is set-valued (no
total order required) and a **direct slice compare** when the
contract declares it sequence-valued (order is meaningful).

The contract's `concurrency` field drives this:

| `concurrency` | Array compare mode |
| --- | --- |
| `Serializable` | Direct slice compare. |
| `PerPaneSerial` | Stable sort by `pane_id` (then by stable secondary key). |
| `Parallel` | Stable sort by `correlation_id` (the per-request key). |

## Adding a new rule

1. Discover a new trivial- or operational-drift field via a
   harness failure. (The error message points at the JSON pointer
   that diverged.)
2. Decide whether the field belongs on Layer 1 or Layer 2 by
   asking: "Could two correct implementations of the same protocol
   ever produce different values here?" If yes → Layer 1; if only
   in different host environments → Layer 2; if no → Layer 3.
3. Add the row to the matching table above.
4. Add the matching `(pointer, replacement)` entry to
   `robot_ntm_differential::default_normalization_rules()`.
5. Re-run the corpus to verify the rule closes the divergence.
6. Land the rule + corpus update in the same commit so reviewers
   can audit the rationale.

## Cross-references

- `crates/frankenterm-core/src/robot_family_contract.rs` — the
  schema-DSL the differential harness consumes.
- `crates/frankenterm-core/tests/robot_family_conformance.rs` —
  the per-family conformance harness; the differential harness is
  a strict superset.
- `docs/robot-contracts/meta-schema.md` — the meta-schema for
  family contracts (defines the `concurrency` field referenced
  above).
- ft-hac7w.2 (profile family) — the first family to consume this
  harness end-to-end; provides the proof-of-concept invocation.
