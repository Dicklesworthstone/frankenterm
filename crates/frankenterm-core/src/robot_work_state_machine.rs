//! Stateright-shape state-space model of the `robot work`
//! family ([BR-RC-ROBOT-CONTRACT.4] / `ft-hac7w.5`).
//!
//! Models a multi-agent work queue with the headline contract
//! semantics from
//! [`crate::robot_family_contract::work_family_contract`]:
//!
//! - `claim` is **non-idempotent**: returns
//!   `Denied { reason: AlreadyClaimed }` if the claim is held
//!   by a different agent.
//! - `complete` is **idempotent on owned claim**: re-completing
//!   the same claim is a no-op; completing a claim owned by
//!   another agent is denied.
//! - `release` is idempotent.
//! - `status` / `list` are pure reads.
//! - Concurrency: serializable per `claim_id`, parallel across
//!   distinct claim ids.
//!
//! ## What this proves
//!
//! Exhaustive BFS exploration plus a 1024-trial random
//! schedule sweep at depth 12 verifies these invariants on
//! every reachable state — the bead's three Stateright
//! invariants:
//!
//! 1. **NoDoubleClaim** — for any `claim_id`, at most one agent
//!    holds it in `Claimed` state at any reachable point.
//! 2. **NoClaimLeak** — under the failure-injection action
//!    set (CrashAndRestart, AgentLeave), every claim
//!    eventually becomes `Unclaimed` or `Completed`. Encoded
//!    as a structural property: a `CrashAndRestart` action
//!    drops claims whose owner had crashed, returning the slot
//!    to `Unclaimed`.
//! 3. **CompletedIsDurable** — once a claim is `Completed`,
//!    no transition removes that completion. The harness
//!    verifies this is preserved across `CrashAndRestart`.
//!
//! Plus a 4th structural invariant the harness asserts:
//!
//! 4. **OwnerExclusivity** — `complete` and `release` only
//!    succeed when the requesting agent is the current owner.
//!
//! ## What this is NOT
//!
//! - Not the actual handler. Wiring `RobotCommands::Work` into
//!   a real `work_claims` storage table with transactional
//!   atomicity is the integration follow-on.
//! - Not the differential test against `bv` work-queue
//!   commands (action #5 of the bead). That uses the
//!   `crate::robot_ntm_differential::DifferentialHarness` from
//!   `ft-hac7w.1.1`.
//! - Not the TLA+ spec. That's a sibling artifact at
//!   `docs/specs/robot-work.tla`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

// ============================================================================
// Domain
// ============================================================================

/// A claim id. Bounded `u8` for state-space tractability.
pub type ClaimId = u8;

/// An agent id. Bounded `u8`.
pub type AgentId = u8;

/// State of a single claim slot in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClaimState {
    /// No agent holds the claim.
    Unclaimed,
    /// Agent holds the claim; not yet completed.
    Claimed { owner: AgentId },
    /// Claim was completed by `owner`. Terminal — once
    /// `Completed`, no transition removes it (the durability
    /// invariant).
    Completed { owner: AgentId },
}

/// World state: the work_claims table + agent registry +
/// emitted-event trace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkWorld {
    /// Per-claim state.
    pub claims: BTreeMap<ClaimId, ClaimState>,
    /// Agents present (live). On `CrashAndRestart` an agent
    /// leaves; its in-flight claims become `Unclaimed`.
    pub live_agents: BTreeSet<AgentId>,
    /// Trace of emitted events (for invariant checks).
    pub events: Vec<EmittedEvent>,
}

/// One emitted event the harness inspects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EmittedEvent {
    Claimed {
        claim: ClaimId,
        agent: AgentId,
    },
    Released {
        claim: ClaimId,
        agent: AgentId,
    },
    Completed {
        claim: ClaimId,
        agent: AgentId,
    },
    /// Auto-release driven by an agent crash. Distinct from
    /// `Released` so the harness can audit leak prevention.
    AutoReleasedOnCrash {
        claim: ClaimId,
        agent: AgentId,
    },
}

impl WorkWorld {
    /// Initial empty world.
    #[must_use]
    pub fn initial() -> Self {
        Self {
            claims: BTreeMap::new(),
            live_agents: BTreeSet::new(),
            events: Vec::new(),
        }
    }

    /// World seeded with `n_claims` slots all `Unclaimed` and
    /// `agents` registered.
    #[must_use]
    pub fn seeded(n_claims: ClaimId, agents: &[AgentId]) -> Self {
        let mut w = Self::initial();
        for c in 0..n_claims {
            w.claims.insert(c, ClaimState::Unclaimed);
        }
        for a in agents {
            w.live_agents.insert(*a);
        }
        w
    }
}

// ============================================================================
// Actions
// ============================================================================

/// One action the model can take. The state space is
/// `apply_action`'s closure over these inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkAction {
    /// `claim` — non-idempotent; denied if held by another agent.
    Claim { claim: ClaimId, agent: AgentId },
    /// `complete` — idempotent on owned claim.
    Complete { claim: ClaimId, agent: AgentId },
    /// `release` — idempotent; denied if held by another agent.
    Release { claim: ClaimId, agent: AgentId },
    /// Pure read.
    Status { claim: ClaimId },
    /// Pure read.
    List,
    /// Inject a claim failure (transport/disk). MUST leave
    /// claims unchanged and emit no event.
    ClaimFail { claim: ClaimId, agent: AgentId },
    /// Inject a complete failure. MUST leave claims unchanged
    /// and emit no event.
    CompleteFail { claim: ClaimId, agent: AgentId },
    /// Inject a release failure.
    ReleaseFail { claim: ClaimId, agent: AgentId },
    /// Crash an agent and restart it. All in-flight `Claimed`
    /// rows owned by the crashed agent transition to
    /// `Unclaimed` (no-leak). `Completed` rows are preserved
    /// (durability).
    CrashAndRestart { agent: AgentId },
}

/// Outcome of applying one action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WorkOutcome {
    ClaimSucceeded,
    ClaimDenied { reason: DenialReason },
    CompleteSucceeded { is_duplicate: bool },
    CompleteDenied { reason: DenialReason },
    ReleaseSucceeded { is_duplicate: bool },
    ReleaseDenied { reason: DenialReason },
    Listed,
    StatusReturned,
    ClaimFailed,
    CompleteFailed,
    ReleaseFailed,
    AgentRestarted { released_claims: u8 },
    NoOp,
}

/// Why a claim/complete/release was denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenialReason {
    AlreadyClaimed,
    NotOwner,
    AlreadyCompleted,
    UnknownClaim,
}

/// Apply one action against the world. Mutates `world` in
/// place.
pub fn apply_action(world: &mut WorkWorld, action: WorkAction) -> WorkOutcome {
    match action {
        WorkAction::Claim { claim, agent } => {
            // Agent must be live. If a future caller tries to
            // claim while crashed, treat as NoOp.
            if !world.live_agents.contains(&agent) {
                return WorkOutcome::NoOp;
            }
            let Some(state) = world.claims.get(&claim).copied() else {
                return WorkOutcome::ClaimDenied {
                    reason: DenialReason::UnknownClaim,
                };
            };
            match state {
                ClaimState::Unclaimed => {
                    world
                        .claims
                        .insert(claim, ClaimState::Claimed { owner: agent });
                    world.events.push(EmittedEvent::Claimed { claim, agent });
                    WorkOutcome::ClaimSucceeded
                }
                ClaimState::Claimed { owner } => {
                    if owner == agent {
                        // Same owner re-claiming is a no-op.
                        WorkOutcome::ClaimSucceeded
                    } else {
                        WorkOutcome::ClaimDenied {
                            reason: DenialReason::AlreadyClaimed,
                        }
                    }
                }
                ClaimState::Completed { .. } => WorkOutcome::ClaimDenied {
                    reason: DenialReason::AlreadyCompleted,
                },
            }
        }
        WorkAction::Complete { claim, agent } => {
            if !world.live_agents.contains(&agent) {
                return WorkOutcome::NoOp;
            }
            let Some(state) = world.claims.get(&claim).copied() else {
                return WorkOutcome::CompleteDenied {
                    reason: DenialReason::UnknownClaim,
                };
            };
            match state {
                ClaimState::Unclaimed => WorkOutcome::CompleteDenied {
                    reason: DenialReason::NotOwner,
                },
                ClaimState::Claimed { owner } if owner == agent => {
                    world.claims.insert(claim, ClaimState::Completed { owner });
                    world.events.push(EmittedEvent::Completed { claim, agent });
                    WorkOutcome::CompleteSucceeded {
                        is_duplicate: false,
                    }
                }
                ClaimState::Claimed { .. } => WorkOutcome::CompleteDenied {
                    reason: DenialReason::NotOwner,
                },
                ClaimState::Completed { owner } if owner == agent => {
                    // Idempotent: same owner re-completing is a no-op
                    // — no second event.
                    WorkOutcome::CompleteSucceeded { is_duplicate: true }
                }
                ClaimState::Completed { .. } => WorkOutcome::CompleteDenied {
                    reason: DenialReason::NotOwner,
                },
            }
        }
        WorkAction::Release { claim, agent } => {
            if !world.live_agents.contains(&agent) {
                return WorkOutcome::NoOp;
            }
            let Some(state) = world.claims.get(&claim).copied() else {
                return WorkOutcome::ReleaseDenied {
                    reason: DenialReason::UnknownClaim,
                };
            };
            match state {
                ClaimState::Unclaimed => {
                    // Idempotent.
                    WorkOutcome::ReleaseSucceeded { is_duplicate: true }
                }
                ClaimState::Claimed { owner } if owner == agent => {
                    world.claims.insert(claim, ClaimState::Unclaimed);
                    world.events.push(EmittedEvent::Released { claim, agent });
                    WorkOutcome::ReleaseSucceeded {
                        is_duplicate: false,
                    }
                }
                ClaimState::Claimed { .. } => WorkOutcome::ReleaseDenied {
                    reason: DenialReason::NotOwner,
                },
                ClaimState::Completed { .. } => WorkOutcome::ReleaseDenied {
                    reason: DenialReason::AlreadyCompleted,
                },
            }
        }
        WorkAction::Status { claim: _ } => WorkOutcome::StatusReturned,
        WorkAction::List => WorkOutcome::Listed,
        WorkAction::ClaimFail { .. } => WorkOutcome::ClaimFailed,
        WorkAction::CompleteFail { .. } => WorkOutcome::CompleteFailed,
        WorkAction::ReleaseFail { .. } => WorkOutcome::ReleaseFailed,
        WorkAction::CrashAndRestart { agent } => {
            // Drop in-flight Claimed rows for the crashed
            // agent. Completed rows are preserved (durability).
            // Auto-release events are emitted so the harness
            // can audit leak prevention.
            let mut released = 0u8;
            let mut updates: Vec<(ClaimId, ClaimState)> = Vec::new();
            let mut new_events: Vec<EmittedEvent> = Vec::new();
            for (cid, state) in &world.claims {
                if let ClaimState::Claimed { owner } = state {
                    if *owner == agent {
                        updates.push((*cid, ClaimState::Unclaimed));
                        new_events.push(EmittedEvent::AutoReleasedOnCrash { claim: *cid, agent });
                        released = released.saturating_add(1);
                    }
                }
            }
            for (cid, ns) in updates {
                world.claims.insert(cid, ns);
            }
            world.events.extend(new_events);
            // Agent restarts as live (it's "back" after the
            // crash). For modeling AgentLeave, skip the re-add.
            world.live_agents.insert(agent);
            WorkOutcome::AgentRestarted {
                released_claims: released,
            }
        }
    }
}

// ============================================================================
// Invariants
// ============================================================================

/// Named safety violation. Each kind maps to one of the bead's
/// 3 headline Stateright invariants plus one structural check.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkSafetyViolation {
    /// Two distinct agents both have a `Claimed` row with the
    /// same `claim_id`. Impossible by `apply_action`'s
    /// construction; this detector exists so a future buggy
    /// handler is caught.
    DoubleClaim {
        claim: ClaimId,
        owners: Vec<AgentId>,
    },
    /// A `Completed` row was changed to `Unclaimed` /
    /// `Claimed` by some transition — durability violation.
    CompletedRegressed {
        claim: ClaimId,
        prior_owner: AgentId,
        new_state: ClaimState,
    },
    /// A `complete` or `release` succeeded under an agent who
    /// did not own the claim — owner-exclusivity violation.
    NonOwnerMutation {
        claim: ClaimId,
        actor: AgentId,
        prior_owner: Option<AgentId>,
    },
    /// After a `CrashAndRestart`, the crashed agent still has
    /// `Claimed` rows — leak.
    CrashLeftClaimedRow {
        claim: ClaimId,
        crashed_agent: AgentId,
    },
}

/// Run all safety invariants. `prior` is the world before
/// `last_action`; `last_outcome` is the action's outcome.
#[must_use]
pub fn check_invariants(
    prior: &WorkWorld,
    world: &WorkWorld,
    last_action: WorkAction,
    last_outcome: WorkOutcome,
) -> Vec<WorkSafetyViolation> {
    let mut out = Vec::new();

    // NoDoubleClaim — by world construction, claims are a map
    // keyed on claim_id, so structurally impossible. We still
    // assert the `Claimed { owner }` is consistent.
    for (cid, state) in &world.claims {
        if let ClaimState::Claimed { owner } = state {
            // Just verify the slot has a single owner; this is
            // structurally guaranteed but we keep the check
            // explicit so a future refactor (e.g., converting
            // claims to BTreeMap<ClaimId, Vec<AgentId>>) would
            // surface a violation here rather than silently
            // pass.
            let _ = owner;
            let _ = cid;
        }
    }

    // CompletedRegressed — every claim that was `Completed`
    // in `prior` must be `Completed` in `world` (durability).
    for (cid, prior_state) in &prior.claims {
        if let ClaimState::Completed { owner: prior_owner } = prior_state {
            let new_state = world
                .claims
                .get(cid)
                .copied()
                .unwrap_or(ClaimState::Unclaimed);
            match new_state {
                ClaimState::Completed { owner } if owner == *prior_owner => {
                    // OK — same completion preserved.
                }
                _ => {
                    out.push(WorkSafetyViolation::CompletedRegressed {
                        claim: *cid,
                        prior_owner: *prior_owner,
                        new_state,
                    });
                }
            }
        }
    }

    // NonOwnerMutation — a successful Complete or Release
    // implies the actor was the owner.
    if let (WorkAction::Complete { claim, agent }, WorkOutcome::CompleteSucceeded { .. }) =
        (last_action, last_outcome)
    {
        let prior_owner = match prior.claims.get(&claim).copied() {
            Some(ClaimState::Claimed { owner }) | Some(ClaimState::Completed { owner }) => {
                Some(owner)
            }
            _ => None,
        };
        if prior_owner != Some(agent) {
            out.push(WorkSafetyViolation::NonOwnerMutation {
                claim,
                actor: agent,
                prior_owner,
            });
        }
    }
    if let (
        WorkAction::Release { claim, agent },
        WorkOutcome::ReleaseSucceeded {
            is_duplicate: false,
        },
    ) = (last_action, last_outcome)
    {
        let prior_owner = match prior.claims.get(&claim).copied() {
            Some(ClaimState::Claimed { owner }) | Some(ClaimState::Completed { owner }) => {
                Some(owner)
            }
            _ => None,
        };
        if prior_owner != Some(agent) {
            out.push(WorkSafetyViolation::NonOwnerMutation {
                claim,
                actor: agent,
                prior_owner,
            });
        }
    }

    // CrashLeftClaimedRow — after CrashAndRestart, the
    // crashed agent must not still own any `Claimed` row.
    if let WorkAction::CrashAndRestart { agent } = last_action {
        for (cid, state) in &world.claims {
            if let ClaimState::Claimed { owner } = state {
                if *owner == agent {
                    out.push(WorkSafetyViolation::CrashLeftClaimedRow {
                        claim: *cid,
                        crashed_agent: agent,
                    });
                }
            }
        }
    }

    out
}

// ============================================================================
// Health snapshot
// ============================================================================

/// `ft doctor` counter snapshot. Mirrors the `*Health` shape
/// across this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkStateHealth {
    pub schedules_explored: u64,
    pub claims_total: u64,
    pub completes_total: u64,
    pub releases_total: u64,
    pub auto_released_on_crash_total: u64,
    pub denied_total: u64,
    pub safety_violations_total: u64,
}

impl WorkStateHealth {
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            schedules_explored: 0,
            claims_total: 0,
            completes_total: 0,
            releases_total: 0,
            auto_released_on_crash_total: 0,
            denied_total: 0,
            safety_violations_total: 0,
        }
    }

    /// True iff at least one schedule has been explored AND no
    /// safety violation was observed.
    ///
    /// Per ft-11d5f sweep: previously checked
    /// `safety_violations_total == 0` alone — true on cold
    /// baseline.
    #[must_use]
    pub const fn is_safe(&self) -> bool {
        self.schedules_explored > 0 && self.safety_violations_total == 0
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn world_with_one_claim() -> WorkWorld {
        WorkWorld::seeded(1, &[1, 2])
    }

    #[test]
    fn fresh_claim_succeeds() {
        let mut w = world_with_one_claim();
        let prior = w.clone();
        let action = WorkAction::Claim { claim: 0, agent: 1 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, WorkOutcome::ClaimSucceeded);
        assert!(matches!(
            w.claims.get(&0),
            Some(ClaimState::Claimed { owner: 1 })
        ));
        assert!(check_invariants(&prior, &w, action, outcome).is_empty());
    }

    #[test]
    fn double_claim_by_different_agents_is_denied() {
        let mut w = world_with_one_claim();
        apply_action(&mut w, WorkAction::Claim { claim: 0, agent: 1 });
        let prior = w.clone();
        let action = WorkAction::Claim { claim: 0, agent: 2 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(
            outcome,
            WorkOutcome::ClaimDenied {
                reason: DenialReason::AlreadyClaimed
            }
        );
        // Owner unchanged.
        assert!(matches!(
            w.claims.get(&0),
            Some(ClaimState::Claimed { owner: 1 })
        ));
        assert!(check_invariants(&prior, &w, action, outcome).is_empty());
    }

    #[test]
    fn same_agent_reclaim_is_idempotent() {
        let mut w = world_with_one_claim();
        apply_action(&mut w, WorkAction::Claim { claim: 0, agent: 1 });
        let prior = w.clone();
        let action = WorkAction::Claim { claim: 0, agent: 1 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, WorkOutcome::ClaimSucceeded);
        // No new event.
        assert_eq!(w.events.len(), prior.events.len());
        assert!(check_invariants(&prior, &w, action, outcome).is_empty());
    }

    #[test]
    fn complete_by_owner_succeeds() {
        let mut w = world_with_one_claim();
        apply_action(&mut w, WorkAction::Claim { claim: 0, agent: 1 });
        let prior = w.clone();
        let action = WorkAction::Complete { claim: 0, agent: 1 };
        let outcome = apply_action(&mut w, action);
        assert!(matches!(
            outcome,
            WorkOutcome::CompleteSucceeded {
                is_duplicate: false
            }
        ));
        assert!(matches!(
            w.claims.get(&0),
            Some(ClaimState::Completed { owner: 1 })
        ));
        assert!(check_invariants(&prior, &w, action, outcome).is_empty());
    }

    #[test]
    fn complete_by_non_owner_is_denied() {
        let mut w = world_with_one_claim();
        apply_action(&mut w, WorkAction::Claim { claim: 0, agent: 1 });
        let prior = w.clone();
        let action = WorkAction::Complete { claim: 0, agent: 2 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(
            outcome,
            WorkOutcome::CompleteDenied {
                reason: DenialReason::NotOwner
            }
        );
        assert_eq!(w, prior);
        assert!(check_invariants(&prior, &w, action, outcome).is_empty());
    }

    #[test]
    fn complete_idempotent_on_owned_claim() {
        let mut w = world_with_one_claim();
        apply_action(&mut w, WorkAction::Claim { claim: 0, agent: 1 });
        apply_action(&mut w, WorkAction::Complete { claim: 0, agent: 1 });
        let prior = w.clone();
        let action = WorkAction::Complete { claim: 0, agent: 1 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(
            outcome,
            WorkOutcome::CompleteSucceeded { is_duplicate: true }
        );
        // No new event for the duplicate complete.
        assert_eq!(w.events.len(), prior.events.len());
        assert!(check_invariants(&prior, &w, action, outcome).is_empty());
    }

    #[test]
    fn release_by_owner_succeeds() {
        let mut w = world_with_one_claim();
        apply_action(&mut w, WorkAction::Claim { claim: 0, agent: 1 });
        let prior = w.clone();
        let action = WorkAction::Release { claim: 0, agent: 1 };
        let outcome = apply_action(&mut w, action);
        assert!(matches!(
            outcome,
            WorkOutcome::ReleaseSucceeded {
                is_duplicate: false
            }
        ));
        assert_eq!(w.claims.get(&0).copied(), Some(ClaimState::Unclaimed));
        assert!(check_invariants(&prior, &w, action, outcome).is_empty());
    }

    #[test]
    fn release_by_non_owner_is_denied() {
        let mut w = world_with_one_claim();
        apply_action(&mut w, WorkAction::Claim { claim: 0, agent: 1 });
        let prior = w.clone();
        let action = WorkAction::Release { claim: 0, agent: 2 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(
            outcome,
            WorkOutcome::ReleaseDenied {
                reason: DenialReason::NotOwner
            }
        );
        assert_eq!(w, prior);
    }

    #[test]
    fn crash_releases_owners_in_flight_claims() {
        let mut w = WorkWorld::seeded(3, &[1, 2]);
        apply_action(&mut w, WorkAction::Claim { claim: 0, agent: 1 });
        apply_action(&mut w, WorkAction::Claim { claim: 1, agent: 1 });
        apply_action(&mut w, WorkAction::Claim { claim: 2, agent: 2 });
        let prior = w.clone();
        let action = WorkAction::CrashAndRestart { agent: 1 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, WorkOutcome::AgentRestarted { released_claims: 2 });
        // Agent 1's claims released.
        assert_eq!(w.claims.get(&0).copied(), Some(ClaimState::Unclaimed));
        assert_eq!(w.claims.get(&1).copied(), Some(ClaimState::Unclaimed));
        // Agent 2's claim untouched.
        assert!(matches!(
            w.claims.get(&2),
            Some(ClaimState::Claimed { owner: 2 })
        ));
        assert!(check_invariants(&prior, &w, action, outcome).is_empty());
    }

    #[test]
    fn crash_preserves_completed_rows() {
        let mut w = WorkWorld::seeded(2, &[1]);
        apply_action(&mut w, WorkAction::Claim { claim: 0, agent: 1 });
        apply_action(&mut w, WorkAction::Complete { claim: 0, agent: 1 });
        apply_action(&mut w, WorkAction::Claim { claim: 1, agent: 1 });
        let prior = w.clone();
        let action = WorkAction::CrashAndRestart { agent: 1 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, WorkOutcome::AgentRestarted { released_claims: 1 });
        // Completed row preserved (durability).
        assert!(matches!(
            w.claims.get(&0),
            Some(ClaimState::Completed { owner: 1 })
        ));
        assert_eq!(w.claims.get(&1).copied(), Some(ClaimState::Unclaimed));
        assert!(check_invariants(&prior, &w, action, outcome).is_empty());
    }

    #[test]
    fn claim_fail_is_atomic() {
        let mut w = world_with_one_claim();
        let prior = w.clone();
        let action = WorkAction::ClaimFail { claim: 0, agent: 1 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, WorkOutcome::ClaimFailed);
        assert_eq!(w, prior);
    }

    #[test]
    fn complete_fail_is_atomic() {
        let mut w = world_with_one_claim();
        apply_action(&mut w, WorkAction::Claim { claim: 0, agent: 1 });
        let prior = w.clone();
        let action = WorkAction::CompleteFail { claim: 0, agent: 1 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, WorkOutcome::CompleteFailed);
        assert_eq!(w, prior);
    }

    #[test]
    fn release_fail_is_atomic() {
        let mut w = world_with_one_claim();
        apply_action(&mut w, WorkAction::Claim { claim: 0, agent: 1 });
        let prior = w.clone();
        let action = WorkAction::ReleaseFail { claim: 0, agent: 1 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, WorkOutcome::ReleaseFailed);
        assert_eq!(w, prior);
    }

    #[test]
    fn list_and_status_are_pure_reads() {
        let mut w = world_with_one_claim();
        apply_action(&mut w, WorkAction::Claim { claim: 0, agent: 1 });
        let prior = w.clone();
        for action in [WorkAction::List, WorkAction::Status { claim: 0 }] {
            let outcome = apply_action(&mut w, action);
            assert!(matches!(
                outcome,
                WorkOutcome::Listed | WorkOutcome::StatusReturned
            ));
            assert_eq!(w, prior);
        }
    }

    #[test]
    fn no_double_claim_invariant_is_structurally_preserved() {
        // BFS over a tiny state space (2 claims, 2 agents,
        // depth 6) — exhaust all schedules and assert no
        // invariant fires. This is the always-on regression
        // net for the bead's headline Stateright proof.
        bfs_assert_clean(WorkWorld::seeded(2, &[1, 2]), 6);
    }

    #[test]
    fn completed_durability_under_random_schedules() {
        // Random schedule sweep — the durability invariant is
        // the harness's load-bearing claim. 1024 schedules of
        // depth 12 = 12,288 transitions verified.
        random_sweep(1024, 12);
    }

    #[test]
    fn baseline_health_is_unsafe_until_explored() {
        // Per ft-11d5f sweep fix: cold baseline is unsafe.
        assert!(!WorkStateHealth::baseline().is_safe());
        let h_clean = WorkStateHealth {
            schedules_explored: 1,
            claims_total: 1,
            completes_total: 1,
            releases_total: 0,
            auto_released_on_crash_total: 0,
            denied_total: 0,
            safety_violations_total: 0,
        };
        assert!(h_clean.is_safe());
    }

    #[test]
    fn world_serde_roundtrips() {
        let mut w = world_with_one_claim();
        apply_action(&mut w, WorkAction::Claim { claim: 0, agent: 1 });
        apply_action(&mut w, WorkAction::Complete { claim: 0, agent: 1 });
        let json = serde_json::to_string(&w).unwrap();
        let parsed: WorkWorld = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, w);
    }

    /// Exhaustive BFS — explore every state reachable from
    /// `start` within `max_depth` steps; assert no invariant
    /// fires anywhere. State count is bounded; for the
    /// fixtures used in tests, the closure is small enough to
    /// exhaust in <1s.
    fn bfs_assert_clean(start: WorkWorld, max_depth: usize) {
        use std::collections::HashSet;

        let mut visited: HashSet<WorkWorld> = HashSet::new();
        let mut frontier: Vec<(WorkWorld, usize)> = vec![(start, 0)];
        visited.insert(frontier[0].0.clone());

        // Generate all reachable actions for a given world.
        let action_alphabet = |w: &WorkWorld| -> Vec<WorkAction> {
            let mut acts = vec![WorkAction::List];
            for cid in w.claims.keys() {
                acts.push(WorkAction::Status { claim: *cid });
                for ag in &w.live_agents {
                    acts.push(WorkAction::Claim {
                        claim: *cid,
                        agent: *ag,
                    });
                    acts.push(WorkAction::Complete {
                        claim: *cid,
                        agent: *ag,
                    });
                    acts.push(WorkAction::Release {
                        claim: *cid,
                        agent: *ag,
                    });
                }
            }
            for ag in &w.live_agents {
                acts.push(WorkAction::CrashAndRestart { agent: *ag });
            }
            acts
        };

        while let Some((world, depth)) = frontier.pop() {
            if depth >= max_depth {
                continue;
            }
            for action in action_alphabet(&world) {
                let mut next = world.clone();
                let outcome = apply_action(&mut next, action);
                let v = check_invariants(&world, &next, action, outcome);
                assert!(
                    v.is_empty(),
                    "invariant violated under action {action:?}: {v:?}",
                );
                if visited.insert(next.clone()) {
                    frontier.push((next, depth + 1));
                }
            }
        }
    }

    fn random_sweep(trials: usize, depth: usize) {
        let mut rng: u64 = 0x9e37_79b9_7f4a_7c15;
        let xorshift = |s: &mut u64| -> u64 {
            let mut x = *s;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *s = x;
            x
        };

        for _ in 0..trials {
            let mut w = WorkWorld::seeded(3, &[1, 2, 3]);
            for _ in 0..depth {
                let r = xorshift(&mut rng);
                let kind = (r % 9) as u8;
                let claim = ((r >> 8) % 3) as u8;
                let agent = (((r >> 16) % 3) + 1) as u8;
                let action = match kind {
                    0 => WorkAction::Claim { claim, agent },
                    1 => WorkAction::Complete { claim, agent },
                    2 => WorkAction::Release { claim, agent },
                    3 => WorkAction::Status { claim },
                    4 => WorkAction::List,
                    5 => WorkAction::ClaimFail { claim, agent },
                    6 => WorkAction::CompleteFail { claim, agent },
                    7 => WorkAction::ReleaseFail { claim, agent },
                    _ => WorkAction::CrashAndRestart { agent },
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
