# Robot Fleet Spec Mapping

Spec: `robot-fleet.tla`

## Rust Correspondence

| TLA+ symbol | Rust target | Notes |
|---|---|---|
| `FleetLifecycleState` | `crates/frankenterm-core/src/robot_fleet_state_machine.rs:72` | Prepared, committing, running, failure, compensation, and terminal states. |
| `FleetWorld` | `crates/frankenterm-core/src/robot_fleet_state_machine.rs:133` | Fleet map, kill-switch level, and emitted events. |
| `FleetAction` | `crates/frankenterm-core/src/robot_fleet_state_machine.rs:173` | Launch, stop, recovery, read, and kill-switch actions. |

## Action Mapping

| TLA+ action | Rust target | Notes |
|---|---|---|
| `PrepareLaunch` / `CommitLaunch` | `crates/frankenterm-core/src/robot_fleet_state_machine.rs:257` | Launch prepare and commit transitions, gated by HardStop. |
| `FailLaunch` / `CompensateLaunch` | `crates/frankenterm-core/src/robot_fleet_state_machine.rs:257` | Recovery transitions after failed launch. |
| `BeginStop` / `CompleteStop` / `FailStop` | `crates/frankenterm-core/src/robot_fleet_state_machine.rs:257` | Stop lifecycle and stop failure handling. |
| `FlipKillSwitch` | `crates/frankenterm-core/src/robot_fleet_state_machine.rs:257` | Operator or automatic kill-switch transition. |

## Invariant Mapping

| TLA+ invariant | Rust target | Notes |
|---|---|---|
| `NoDoubleRunningName` | `crates/frankenterm-core/src/robot_fleet_state_machine.rs:477` | No two running fleets share a name. |
| `TerminalsAreSticky` | `crates/frankenterm-core/src/robot_fleet_state_machine.rs:477` | Stopped and rolled-back fleets do not regress. |
| `EventuallyDrains` | `crates/frankenterm-core/src/robot_fleet_state_machine.rs:477` | Recovery actions remain available for non-terminal fleets. |

## TLC Configuration

Config: `robot-fleet.cfg`

The deterministic smoke model uses two fleet ids and two names. It checks safety
with `SafetyInvariants`; liveness/fairness runs can reuse the same constants and
enable temporal properties in a recorded proof artifact.
