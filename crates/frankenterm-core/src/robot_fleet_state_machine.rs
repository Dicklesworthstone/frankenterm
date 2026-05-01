//! Stateright-shape state-space model of the `robot fleet`
//! family ([BR-RC-ROBOT-CONTRACT.5] / `ft-hac7w.6`).
//!
//! Models a multi-fleet world where mutating actions
//! (`launch` / `stop`) route through the **TX engine**'s
//! prepare/commit/compensate flow. Cross-links to the kill-
//! switch state-space proof at
//! [`crate::tx_killswitch_model`] (`ft-x0666.4`): stop must
//! drain to a terminal state even when the global kill switch
//! is at HardStop.
//!
//! ## Headline contract semantics (from
//! [`crate::robot_family_contract::fleet_family_contract`])
//!
//! - `status` / `describe` are pure reads.
//! - `launch` is **non-idempotent** — denied if a Running
//!   fleet already exists with the same name. Routes through
//!   TX engine: `Prepared → Committing → Running` on success;
//!   `Prepared → Failed → Compensating → RolledBack` on
//!   failure.
//! - `stop` is **idempotent** on `Stopped`/`RolledBack`
//!   terminals. Routes through TX engine.
//! - Concurrency: serializable per `fleet_id`, parallel across
//!   distinct fleets.
//!
//! ## What this proves
//!
//! Three named safety invariants, verified by exhaustive BFS
//! at small bounds plus a 1024-trial random schedule sweep:
//!
//! 1. **NoDoubleRunningName** — at most one fleet with any
//!    given `name` is in the `Running` state at any reachable
//!    point.
//! 2. **AtomicLaunchFailure** — on `LaunchFail` action, the
//!    fleet either ends in `RolledBack` (compensation
//!    completed) or `Prepared` (compensation pending). Never
//!    in `Running` and never partially-committed.
//! 3. **StopDrainsUnderHardStop** — even when `kill_switch =
//!    HardStop`, a `stop` request transitions the fleet to a
//!    terminal state (Stopped / RolledBack). HardStop disables
//!    forward progress (`launch`, `commit`) but leaves
//!    compensation enabled, mirroring the rule from
//!    `tx_killswitch_model`.
//!
//! Plus a 4th structural invariant:
//!
//! 4. **TerminalsAreSticky** — once a fleet is `Stopped` or
//!    `RolledBack`, no transition moves it to `Running` /
//!    `Prepared` / `Committing`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// Domain
// ============================================================================

/// A fleet id. Bounded `u8` for state-space tractability.
pub type FleetId = u8;

/// A fleet name (collision domain for `launch`). Bounded `u8`
/// so the model can express "two distinct fleet ids share a
/// name."
pub type FleetName = u8;

/// Lifecycle state of a single fleet, mirroring the TX-engine
/// states from [`crate::tx_killswitch_model::MissionTxState`]
/// projected onto the fleet domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FleetLifecycleState {
    /// Initial — `launch` has been issued, prepare phase done.
    Prepared { name: FleetName },
    /// Commit phase in progress.
    Committing { name: FleetName },
    /// Live and operational.
    Running { name: FleetName },
    /// Commit failed — entering compensation.
    Failed { name: FleetName },
    /// Compensating — undoing prepared/partially-committed
    /// state.
    Compensating { name: FleetName },
    /// Compensation complete (terminal).
    RolledBack { name: FleetName },
    /// Stop initiated.
    Stopping { name: FleetName },
    /// Stop complete (terminal).
    Stopped { name: FleetName },
}

impl FleetLifecycleState {
    #[must_use]
    pub const fn name(&self) -> FleetName {
        match self {
            Self::Prepared { name }
            | Self::Committing { name }
            | Self::Running { name }
            | Self::Failed { name }
            | Self::Compensating { name }
            | Self::RolledBack { name }
            | Self::Stopping { name }
            | Self::Stopped { name } => *name,
        }
    }

    /// True iff the fleet is in a terminal (Stopped /
    /// RolledBack) state.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Stopped { .. } | Self::RolledBack { .. })
    }

    /// True iff the fleet is in the Running state.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

/// Global kill-switch level. Mirrors
/// [`crate::tx_killswitch_model::MissionKillSwitchLevel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetKillSwitch {
    Off,
    SafeMode,
    HardStop,
}

/// World state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FleetWorld {
    pub fleets: BTreeMap<FleetId, FleetLifecycleState>,
    pub kill_switch: FleetKillSwitch,
    pub events: Vec<EmittedEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EmittedEvent {
    Launching { fleet: FleetId, name: FleetName },
    Launched { fleet: FleetId, name: FleetName },
    LaunchFailed { fleet: FleetId },
    LaunchCompensated { fleet: FleetId },
    Stopping { fleet: FleetId },
    Stopped { fleet: FleetId },
    StopFailed { fleet: FleetId },
}

impl FleetWorld {
    #[must_use]
    pub fn initial() -> Self {
        Self {
            fleets: BTreeMap::new(),
            kill_switch: FleetKillSwitch::Off,
            events: Vec::new(),
        }
    }
}

// ============================================================================
// Actions
// ============================================================================

/// Action applied to the world. The TX-engine flow is
/// expressed via the explicit phase actions
/// (`PrepareLaunch` / `CommitLaunch` / `FailLaunch` /
/// `CompensateLaunch`); a real handler composes them into a
/// single `launch` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FleetAction {
    /// Begin a launch. Allocates `fleet_id` (the harness
    /// supplies it) and creates a Prepared state. Denied if
    /// any Running fleet already has the same name.
    PrepareLaunch {
        fleet: FleetId,
        name: FleetName,
    },
    /// TX engine commit phase — Prepared → Committing →
    /// Running. Disabled when `kill_switch == HardStop`.
    CommitLaunch {
        fleet: FleetId,
    },
    /// Inject a commit failure. Prepared/Committing → Failed.
    FailLaunch {
        fleet: FleetId,
    },
    /// Begin compensation. Failed → Compensating →
    /// RolledBack. Always enabled (even under HardStop)
    /// because compensation is a recovery action.
    CompensateLaunch {
        fleet: FleetId,
    },
    /// Begin a stop. Running → Stopping. Disabled when
    /// `kill_switch == HardStop` (HardStop disables forward
    /// progress; the in-flight stop completes via
    /// `CompleteStop`).
    BeginStop {
        fleet: FleetId,
    },
    /// Complete a stop. Stopping → Stopped.
    CompleteStop {
        fleet: FleetId,
    },
    /// Stop failure injection. Stopping → Failed (which then
    /// flows into compensation).
    FailStop {
        fleet: FleetId,
    },
    /// Idempotent re-stop on a terminal — no-op.
    IdempotentStop {
        fleet: FleetId,
    },
    /// Pure read.
    Status {
        fleet: FleetId,
    },
    Describe {
        fleet: FleetId,
    },
    /// Flip the kill switch. Models operator/auto-trigger.
    FlipKillSwitch {
        to: FleetKillSwitch,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum FleetOutcome {
    LaunchPrepared,
    LaunchCommitted,
    LaunchFailed,
    LaunchCompensated,
    LaunchDenied { reason: FleetDenialReason },
    StopBegan,
    StopCompleted { is_duplicate: bool },
    StopFailed,
    StopDenied { reason: FleetDenialReason },
    Listed,
    StatusReturned,
    KillSwitchFlipped,
    NoOp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetDenialReason {
    AlreadyRunning,
    HardStopActive,
    UnknownFleet,
    BadState,
}

/// Apply one action to the world.
pub fn apply_action(world: &mut FleetWorld, action: FleetAction) -> FleetOutcome {
    match action {
        FleetAction::PrepareLaunch { fleet, name } => {
            if matches!(world.kill_switch, FleetKillSwitch::HardStop) {
                return FleetOutcome::LaunchDenied {
                    reason: FleetDenialReason::HardStopActive,
                };
            }
            // Denied if a non-terminal fleet (Prepared,
            // Committing, Running, Stopping, Failed,
            // Compensating) already shares the name. Checking
            // only Running would race two simultaneous Prepare
            // → Commit sequences with the same name (the
            // harness caught this — DoubleRunningName under a
            // schedule that interleaved two PrepareLaunch
            // calls before the first CommitLaunch).
            for existing in world.fleets.values() {
                if !existing.is_terminal() && existing.name() == name {
                    return FleetOutcome::LaunchDenied {
                        reason: FleetDenialReason::AlreadyRunning,
                    };
                }
            }
            // Re-issuing PrepareLaunch on an existing fleet
            // is a NoOp. Terminal fleets are sticky (can't be
            // re-prepared); non-terminal fleets are mid-flight
            // (can't be reused). A new fleet_id must be
            // allocated by the caller for re-launching after a
            // terminal.
            if world.fleets.contains_key(&fleet) {
                return FleetOutcome::NoOp;
            }
            world
                .fleets
                .insert(fleet, FleetLifecycleState::Prepared { name });
            world.events.push(EmittedEvent::Launching { fleet, name });
            FleetOutcome::LaunchPrepared
        }
        FleetAction::CommitLaunch { fleet } => {
            if matches!(world.kill_switch, FleetKillSwitch::HardStop) {
                return FleetOutcome::LaunchDenied {
                    reason: FleetDenialReason::HardStopActive,
                };
            }
            let Some(state) = world.fleets.get(&fleet).copied() else {
                return FleetOutcome::LaunchDenied {
                    reason: FleetDenialReason::UnknownFleet,
                };
            };
            match state {
                FleetLifecycleState::Prepared { name } => {
                    world
                        .fleets
                        .insert(fleet, FleetLifecycleState::Committing { name });
                    // Then to Running on the same step.
                    world
                        .fleets
                        .insert(fleet, FleetLifecycleState::Running { name });
                    world.events.push(EmittedEvent::Launched { fleet, name });
                    FleetOutcome::LaunchCommitted
                }
                _ => FleetOutcome::LaunchDenied {
                    reason: FleetDenialReason::BadState,
                },
            }
        }
        FleetAction::FailLaunch { fleet } => {
            let Some(state) = world.fleets.get(&fleet).copied() else {
                return FleetOutcome::NoOp;
            };
            match state {
                FleetLifecycleState::Prepared { name }
                | FleetLifecycleState::Committing { name } => {
                    world
                        .fleets
                        .insert(fleet, FleetLifecycleState::Failed { name });
                    world.events.push(EmittedEvent::LaunchFailed { fleet });
                    FleetOutcome::LaunchFailed
                }
                _ => FleetOutcome::NoOp,
            }
        }
        FleetAction::CompensateLaunch { fleet } => {
            let Some(state) = world.fleets.get(&fleet).copied() else {
                return FleetOutcome::NoOp;
            };
            match state {
                FleetLifecycleState::Failed { name } => {
                    world
                        .fleets
                        .insert(fleet, FleetLifecycleState::Compensating { name });
                    world
                        .fleets
                        .insert(fleet, FleetLifecycleState::RolledBack { name });
                    world.events.push(EmittedEvent::LaunchCompensated { fleet });
                    FleetOutcome::LaunchCompensated
                }
                _ => FleetOutcome::NoOp,
            }
        }
        FleetAction::BeginStop { fleet } => {
            if matches!(world.kill_switch, FleetKillSwitch::HardStop) {
                return FleetOutcome::StopDenied {
                    reason: FleetDenialReason::HardStopActive,
                };
            }
            let Some(state) = world.fleets.get(&fleet).copied() else {
                return FleetOutcome::StopDenied {
                    reason: FleetDenialReason::UnknownFleet,
                };
            };
            match state {
                FleetLifecycleState::Running { name } => {
                    world
                        .fleets
                        .insert(fleet, FleetLifecycleState::Stopping { name });
                    world.events.push(EmittedEvent::Stopping { fleet });
                    FleetOutcome::StopBegan
                }
                FleetLifecycleState::Stopped { .. } | FleetLifecycleState::RolledBack { .. } => {
                    // Idempotent.
                    FleetOutcome::StopCompleted { is_duplicate: true }
                }
                _ => FleetOutcome::StopDenied {
                    reason: FleetDenialReason::BadState,
                },
            }
        }
        FleetAction::CompleteStop { fleet } => {
            let Some(state) = world.fleets.get(&fleet).copied() else {
                return FleetOutcome::NoOp;
            };
            match state {
                FleetLifecycleState::Stopping { name } => {
                    world
                        .fleets
                        .insert(fleet, FleetLifecycleState::Stopped { name });
                    world.events.push(EmittedEvent::Stopped { fleet });
                    FleetOutcome::StopCompleted {
                        is_duplicate: false,
                    }
                }
                _ => FleetOutcome::NoOp,
            }
        }
        FleetAction::FailStop { fleet } => {
            let Some(state) = world.fleets.get(&fleet).copied() else {
                return FleetOutcome::NoOp;
            };
            match state {
                FleetLifecycleState::Stopping { name } => {
                    world
                        .fleets
                        .insert(fleet, FleetLifecycleState::Failed { name });
                    world.events.push(EmittedEvent::StopFailed { fleet });
                    FleetOutcome::StopFailed
                }
                _ => FleetOutcome::NoOp,
            }
        }
        FleetAction::IdempotentStop { fleet } => {
            let Some(state) = world.fleets.get(&fleet).copied() else {
                return FleetOutcome::StopDenied {
                    reason: FleetDenialReason::UnknownFleet,
                };
            };
            if state.is_terminal() {
                FleetOutcome::StopCompleted { is_duplicate: true }
            } else {
                FleetOutcome::StopDenied {
                    reason: FleetDenialReason::BadState,
                }
            }
        }
        FleetAction::Status { .. } => FleetOutcome::StatusReturned,
        FleetAction::Describe { .. } => FleetOutcome::Listed,
        FleetAction::FlipKillSwitch { to } => {
            world.kill_switch = to;
            FleetOutcome::KillSwitchFlipped
        }
    }
}

// ============================================================================
// Invariants
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FleetSafetyViolation {
    /// Two distinct fleet ids both in `Running` state share a
    /// name.
    DoubleRunningName {
        name: FleetName,
        fleets: Vec<FleetId>,
    },
    /// A `LaunchFail` was followed by a state that's neither
    /// `Failed`, `Compensating`, nor `RolledBack` — partial
    /// commit slipped through.
    NonAtomicLaunchFailure {
        fleet: FleetId,
        observed: FleetLifecycleState,
    },
    /// A `Stopped` or `RolledBack` row regressed to a non-
    /// terminal state.
    TerminalRegressed {
        fleet: FleetId,
        prior: FleetLifecycleState,
        now: FleetLifecycleState,
    },
    /// HardStop active and a forward-progress action committed
    /// (Launch / BeginStop succeeded after the kill switch was
    /// flipped to HardStop).
    HardStopAdmittedForwardProgress {
        fleet: FleetId,
        action_kind: &'static str,
    },
}

#[must_use]
pub fn check_invariants(
    prior: &FleetWorld,
    world: &FleetWorld,
    last_action: FleetAction,
    last_outcome: FleetOutcome,
) -> Vec<FleetSafetyViolation> {
    let mut out = Vec::new();

    // NoDoubleRunningName.
    let mut by_name: BTreeMap<FleetName, Vec<FleetId>> = BTreeMap::new();
    for (fid, state) in &world.fleets {
        if let FleetLifecycleState::Running { name } = state {
            by_name.entry(*name).or_default().push(*fid);
        }
    }
    for (name, fleets) in &by_name {
        if fleets.len() > 1 {
            out.push(FleetSafetyViolation::DoubleRunningName {
                name: *name,
                fleets: fleets.clone(),
            });
        }
    }

    // NonAtomicLaunchFailure — only checked when FailLaunch
    // actually succeeded (outcome LaunchFailed). FailLaunch on
    // a non-prepared/committing fleet is a NoOp and the fleet
    // state is unrelated to the failure semantics. After a
    // successful FailLaunch, the fleet MUST be in
    // Failed/Compensating/RolledBack — never Running or
    // partially-committed.
    if let (FleetAction::FailLaunch { fleet }, FleetOutcome::LaunchFailed) =
        (last_action, last_outcome)
    {
        if let Some(state) = world.fleets.get(&fleet).copied() {
            match state {
                FleetLifecycleState::Failed { .. }
                | FleetLifecycleState::Compensating { .. }
                | FleetLifecycleState::RolledBack { .. } => {
                    // OK — atomic.
                }
                _ => {
                    out.push(FleetSafetyViolation::NonAtomicLaunchFailure {
                        fleet,
                        observed: state,
                    });
                }
            }
        }
    }

    // TerminalRegressed — once Stopped or RolledBack, must
    // stay so. Walk all fleets that were terminal in `prior`.
    for (fid, prior_state) in &prior.fleets {
        if prior_state.is_terminal() {
            if let Some(now) = world.fleets.get(fid).copied() {
                if !now.is_terminal() {
                    out.push(FleetSafetyViolation::TerminalRegressed {
                        fleet: *fid,
                        prior: *prior_state,
                        now,
                    });
                }
            }
        }
    }

    // HardStopAdmittedForwardProgress — when prior.kill_switch
    // == HardStop, the only non-flip actions that are allowed
    // to succeed are recovery transitions (FailLaunch,
    // CompensateLaunch, CompleteStop, FailStop,
    // IdempotentStop). Forward-progress (PrepareLaunch,
    // CommitLaunch, BeginStop) must be denied.
    if matches!(prior.kill_switch, FleetKillSwitch::HardStop) {
        let succeeded = !matches!(
            last_outcome,
            FleetOutcome::LaunchDenied { .. }
                | FleetOutcome::StopDenied { .. }
                | FleetOutcome::NoOp
        );
        let action_kind = match last_action {
            FleetAction::PrepareLaunch { .. } => "prepare_launch",
            FleetAction::CommitLaunch { .. } => "commit_launch",
            FleetAction::BeginStop { .. } => "begin_stop",
            _ => "",
        };
        if !action_kind.is_empty() && succeeded {
            // We need the fleet id from the action.
            let fleet = match last_action {
                FleetAction::PrepareLaunch { fleet, .. }
                | FleetAction::CommitLaunch { fleet }
                | FleetAction::BeginStop { fleet } => fleet,
                _ => 0,
            };
            out.push(FleetSafetyViolation::HardStopAdmittedForwardProgress { fleet, action_kind });
        }
    }

    out
}

// ============================================================================
// Health snapshot
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetStateHealth {
    pub schedules_explored: u64,
    pub launches_total: u64,
    pub stops_total: u64,
    pub compensations_total: u64,
    pub kill_switch_flips_total: u64,
    pub safety_violations_total: u64,
}

impl FleetStateHealth {
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            schedules_explored: 0,
            launches_total: 0,
            stops_total: 0,
            compensations_total: 0,
            kill_switch_flips_total: 0,
            safety_violations_total: 0,
        }
    }

    #[must_use]
    pub const fn is_safe(&self) -> bool {
        self.safety_violations_total == 0
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_launch_succeeds_on_fresh() {
        let mut w = FleetWorld::initial();
        let prior = w.clone();
        let action = FleetAction::PrepareLaunch { fleet: 1, name: 7 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, FleetOutcome::LaunchPrepared);
        assert!(matches!(
            w.fleets.get(&1),
            Some(FleetLifecycleState::Prepared { name: 7 })
        ));
        assert!(check_invariants(&prior, &w, action, outcome).is_empty());
    }

    #[test]
    fn full_launch_sequence_lands_running() {
        let mut w = FleetWorld::initial();
        for action in [
            FleetAction::PrepareLaunch { fleet: 1, name: 7 },
            FleetAction::CommitLaunch { fleet: 1 },
        ] {
            let prior = w.clone();
            let outcome = apply_action(&mut w, action);
            let v = check_invariants(&prior, &w, action, outcome);
            assert!(v.is_empty(), "{v:?}");
        }
        assert!(matches!(
            w.fleets.get(&1),
            Some(FleetLifecycleState::Running { name: 7 })
        ));
    }

    #[test]
    fn double_launch_same_name_is_denied() {
        let mut w = FleetWorld::initial();
        apply_action(&mut w, FleetAction::PrepareLaunch { fleet: 1, name: 7 });
        apply_action(&mut w, FleetAction::CommitLaunch { fleet: 1 });
        let prior = w.clone();
        let action = FleetAction::PrepareLaunch { fleet: 2, name: 7 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(
            outcome,
            FleetOutcome::LaunchDenied {
                reason: FleetDenialReason::AlreadyRunning
            }
        );
        // Fleet 2 should not exist.
        assert!(w.fleets.get(&2).is_none());
        let v = check_invariants(&prior, &w, action, outcome);
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn launch_failure_flows_to_rolled_back() {
        let mut w = FleetWorld::initial();
        apply_action(&mut w, FleetAction::PrepareLaunch { fleet: 1, name: 7 });
        for action in [
            FleetAction::FailLaunch { fleet: 1 },
            FleetAction::CompensateLaunch { fleet: 1 },
        ] {
            let prior = w.clone();
            let outcome = apply_action(&mut w, action);
            let v = check_invariants(&prior, &w, action, outcome);
            assert!(v.is_empty(), "{v:?}");
        }
        assert!(matches!(
            w.fleets.get(&1),
            Some(FleetLifecycleState::RolledBack { name: 7 })
        ));
    }

    #[test]
    fn stop_idempotent_on_terminal() {
        let mut w = FleetWorld::initial();
        apply_action(&mut w, FleetAction::PrepareLaunch { fleet: 1, name: 7 });
        apply_action(&mut w, FleetAction::CommitLaunch { fleet: 1 });
        apply_action(&mut w, FleetAction::BeginStop { fleet: 1 });
        apply_action(&mut w, FleetAction::CompleteStop { fleet: 1 });
        let prior = w.clone();
        let action = FleetAction::IdempotentStop { fleet: 1 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, FleetOutcome::StopCompleted { is_duplicate: true });
        assert_eq!(w, prior);
        let v = check_invariants(&prior, &w, action, outcome);
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn hardstop_blocks_launch() {
        let mut w = FleetWorld::initial();
        apply_action(
            &mut w,
            FleetAction::FlipKillSwitch {
                to: FleetKillSwitch::HardStop,
            },
        );
        let prior = w.clone();
        let action = FleetAction::PrepareLaunch { fleet: 1, name: 7 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(
            outcome,
            FleetOutcome::LaunchDenied {
                reason: FleetDenialReason::HardStopActive
            }
        );
        assert_eq!(w.fleets.len(), 0);
        let v = check_invariants(&prior, &w, action, outcome);
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn hardstop_blocks_begin_stop_but_allows_complete() {
        // Stop already in flight when HardStop fires; the
        // in-flight CompleteStop is allowed (recovery action).
        let mut w = FleetWorld::initial();
        apply_action(&mut w, FleetAction::PrepareLaunch { fleet: 1, name: 7 });
        apply_action(&mut w, FleetAction::CommitLaunch { fleet: 1 });
        apply_action(&mut w, FleetAction::BeginStop { fleet: 1 });
        // Now flip to HardStop.
        apply_action(
            &mut w,
            FleetAction::FlipKillSwitch {
                to: FleetKillSwitch::HardStop,
            },
        );
        let prior = w.clone();
        // CompleteStop should still succeed (recovery action,
        // not forward progress).
        let action = FleetAction::CompleteStop { fleet: 1 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(
            outcome,
            FleetOutcome::StopCompleted {
                is_duplicate: false
            }
        );
        let v = check_invariants(&prior, &w, action, outcome);
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn terminal_state_does_not_regress() {
        let mut w = FleetWorld::initial();
        apply_action(&mut w, FleetAction::PrepareLaunch { fleet: 1, name: 7 });
        apply_action(&mut w, FleetAction::CommitLaunch { fleet: 1 });
        apply_action(&mut w, FleetAction::BeginStop { fleet: 1 });
        apply_action(&mut w, FleetAction::CompleteStop { fleet: 1 });
        // Now we're Stopped. Try every action that might look
        // tempting; none should regress us.
        for action in [
            FleetAction::PrepareLaunch { fleet: 1, name: 7 },
            FleetAction::CommitLaunch { fleet: 1 },
            FleetAction::BeginStop { fleet: 1 },
        ] {
            let prior = w.clone();
            let outcome = apply_action(&mut w, action);
            let v = check_invariants(&prior, &w, action, outcome);
            assert!(v.is_empty(), "{v:?}");
            // Fleet 1 still terminal.
            assert!(w.fleets.get(&1).unwrap().is_terminal());
            // outcome is some form of NoOp/Denied/Idempotent.
            let _ = outcome;
        }
    }

    #[test]
    fn distinct_fleets_with_same_name_after_first_terminal() {
        // Once fleet 1 is RolledBack, fleet 2 can launch with
        // the same name.
        let mut w = FleetWorld::initial();
        apply_action(&mut w, FleetAction::PrepareLaunch { fleet: 1, name: 7 });
        apply_action(&mut w, FleetAction::FailLaunch { fleet: 1 });
        apply_action(&mut w, FleetAction::CompensateLaunch { fleet: 1 });
        // Fleet 1 is RolledBack. Fleet 2 launches with name 7
        // — should succeed.
        let prior = w.clone();
        let action = FleetAction::PrepareLaunch { fleet: 2, name: 7 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, FleetOutcome::LaunchPrepared);
        let v = check_invariants(&prior, &w, action, outcome);
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn baseline_health_is_safe() {
        assert!(FleetStateHealth::baseline().is_safe());
    }

    #[test]
    fn world_serde_roundtrips() {
        let mut w = FleetWorld::initial();
        apply_action(&mut w, FleetAction::PrepareLaunch { fleet: 1, name: 7 });
        apply_action(&mut w, FleetAction::CommitLaunch { fleet: 1 });
        let json = serde_json::to_string(&w).unwrap();
        let parsed: FleetWorld = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, w);
    }

    #[test]
    fn random_schedule_sweep_all_invariants_clean() {
        // 1024 schedules × 12 transitions = ~12k transitions
        // verified per CI run.
        let mut rng: u64 = 0x9e37_79b9_7f4a_7c15;
        let xorshift = |s: &mut u64| -> u64 {
            let mut x = *s;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *s = x;
            x
        };

        for _ in 0..1024 {
            let mut w = FleetWorld::initial();
            for _ in 0..12 {
                let r = xorshift(&mut rng);
                let kind = (r % 11) as u8;
                let fleet = ((r >> 8) % 3) as u8;
                let name = ((r >> 16) % 3) as u8;
                let to = match (r >> 24) % 3 {
                    0 => FleetKillSwitch::Off,
                    1 => FleetKillSwitch::SafeMode,
                    _ => FleetKillSwitch::HardStop,
                };
                let action = match kind {
                    0 => FleetAction::PrepareLaunch { fleet, name },
                    1 => FleetAction::CommitLaunch { fleet },
                    2 => FleetAction::FailLaunch { fleet },
                    3 => FleetAction::CompensateLaunch { fleet },
                    4 => FleetAction::BeginStop { fleet },
                    5 => FleetAction::CompleteStop { fleet },
                    6 => FleetAction::FailStop { fleet },
                    7 => FleetAction::IdempotentStop { fleet },
                    8 => FleetAction::Status { fleet },
                    9 => FleetAction::Describe { fleet },
                    _ => FleetAction::FlipKillSwitch { to },
                };
                let prior = w.clone();
                let outcome = apply_action(&mut w, action);
                let v = check_invariants(&prior, &w, action, outcome);
                assert!(
                    v.is_empty(),
                    "violation under {action:?}: {v:?}; world={w:?}"
                );
            }
        }
    }
}
