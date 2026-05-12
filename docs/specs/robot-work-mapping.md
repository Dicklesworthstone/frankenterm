# Robot Work Spec Mapping

Spec: `robot-work.tla`

## Rust Correspondence

| TLA+ symbol | Rust target | Notes |
|---|---|---|
| `ClaimState` | `crates/frankenterm-core/src/robot_work_state_machine.rs:72` | Unclaimed, claimed, and completed states. |
| `WorkWorld` | `crates/frankenterm-core/src/robot_work_state_machine.rs:86` | Claim table, live agents, and emitted events. |
| `WorkAction` | `crates/frankenterm-core/src/robot_work_state_machine.rs:154` | Claim, complete, release, failure injection, list/status, crash/restart. |

## Action Mapping

| TLA+ action | Rust target | Notes |
|---|---|---|
| `Claim` / `ClaimByOwner` / `ClaimDenied` | `crates/frankenterm-core/src/robot_work_state_machine.rs:211` | Ownership acquisition and denial behavior. |
| `Complete` / `CompleteByOwnerIdempotent` / `CompleteDenied` | `crates/frankenterm-core/src/robot_work_state_machine.rs:211` | Completion durability and owner checks. |
| `Release` / `ReleaseIdempotent` | `crates/frankenterm-core/src/robot_work_state_machine.rs:211` | Owner release and idempotent release behavior. |
| `CrashAndRestart` | `crates/frankenterm-core/src/robot_work_state_machine.rs:211` | Auto-release of in-flight claims on agent restart. |

## Invariant Mapping

| TLA+ invariant | Rust target | Notes |
|---|---|---|
| `NoDoubleClaim` | `crates/frankenterm-core/src/robot_work_state_machine.rs:387` | A claim has at most one owner. |
| `CompletedDurabilityInductive` | `crates/frankenterm-core/src/robot_work_state_machine.rs:387` | Completed claims remain completed with the same owner. |
| `NoClaimLeak` | `crates/frankenterm-core/src/robot_work_state_machine.rs:387` | Crash/restart releases in-flight work for the crashed agent. |

## TLC Configuration

Config: `robot-work.cfg`

The deterministic smoke model uses two claims and two agents. That keeps the
state space small while still exercising cross-agent denial, completion, and
crash/restart paths.
