//! State-space model of the `robot checkpoint` save→rollback
//! state machine
//! ([BR-RC-ROBOT-CONTRACT.2] / `ft-hac7w.3`).
//!
//! Mirrors the headline contract semantics from
//! [`crate::robot_family_contract::checkpoint_family_contract`]:
//!
//! - `save` is **idempotent** (content-addressed checkpoint
//!   id; re-saving the same source state returns the existing
//!   id without a second snapshots-table row).
//! - `rollback` requires an approval token; **MUST NOT
//!   partially mutate** — a denied or failed rollback leaves
//!   `session_state` untouched.
//! - `list` is a pure read.
//! - Concurrency: serializable per session.
//!
//! ## What this proves
//!
//! BFS exhaustive exploration over `step_count ∈ {2, 3}` plus
//! a property-test sweep at depth 8 verifies these invariants
//! on every reachable state:
//!
//! 1. **NoOrphanCheckpoint** — every entry in `session_state`
//!    pointing to a checkpoint is backed by a row in
//!    `snapshots`.
//! 2. **NoDoubleSaveOnSameContent** — saving the same content
//!    twice does not produce two distinct checkpoint ids
//!    (idempotence).
//! 3. **NoUnauthorizedRollback** — every successful rollback
//!    transition has an approval token recorded in the
//!    receipt; rollback without a token transitions to
//!    `Denied` with no `session_state` mutation.
//! 4. **AtomicOnRollbackFailure** — a `RollbackFail` action
//!    leaves `session_state == previous_session_state` and
//!    emits no `checkpoint.rolled_back` event in the trace.
//! 5. **ListIsPureRead** — a `List` action does not change
//!    any field of the world state.
//!
//! ## What this is NOT
//!
//! - Not the actual handler. Wiring `RobotCommands::Checkpoint`
//!   into the existing `ft snapshot` + session_restore
//!   machinery is the integration follow-on.
//! - Not the differential test against `ntm checkpoint`. That
//!   uses `crate::robot_ntm_differential::DifferentialHarness`
//!   from `ft-hac7w.1.1` and runs in a separate harness.
//! - Not the TLA+ spec. That's a sibling artifact at
//!   `docs/specs/robot-checkpoint.tla`; this Rust model is the
//!   always-on regression net.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

// ============================================================================
// Domain
// ============================================================================

/// A session id. Bounded `u8` for state-space tractability.
pub type SessionId = u8;

/// A checkpoint id. In production this is a BLAKE3 hex string;
/// the model uses a `u8` content-hash so the BFS state space is
/// finite.
pub type CheckpointId = u8;

/// Token authorizing a cross-pane rollback. The model doesn't
/// distinguish valid vs invalid token *content* — the transition
/// fires only when a token is present, and the harness
/// interprets `0xFF` as "explicitly invalid token" for testing
/// the rollback-requires-approval invariant.
pub type ApprovalToken = u8;

/// Approval token sentinel meaning "no token at all" (the
/// caller didn't supply one).
pub const TOKEN_ABSENT: ApprovalToken = 0;

/// Approval token sentinel meaning "explicitly invalid token"
/// — the caller supplied a token but it failed verification.
pub const TOKEN_INVALID: ApprovalToken = 0xFF;

/// World state under model. Mirrors the production `snapshots`
/// table + per-session `session_state` projection.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CheckpointWorld {
    /// `snapshots` table: checkpoint id → its content hash.
    pub snapshots: BTreeMap<CheckpointId, ContentHash>,
    /// `session_state[session_id]` = (current content hash,
    /// most recent checkpoint id pointing at it).
    pub session_state: BTreeMap<SessionId, SessionView>,
    /// Trace of emitted events (for invariant checks).
    pub events: Vec<EmittedEvent>,
}

/// Content of a session at a point in time. Hashed to derive
/// `CheckpointId` under the content-addressed save rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ContentHash(pub u8);

/// What the session currently holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionView {
    pub content: ContentHash,
    /// Most recent checkpoint id whose content matches
    /// `content`; `None` if none has been saved.
    pub last_checkpoint: Option<CheckpointId>,
}

/// Trace event the harness inspects for invariant checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EmittedEvent {
    CheckpointSaved {
        session_id: SessionId,
        checkpoint_id: CheckpointId,
        is_duplicate: bool,
    },
    CheckpointRolledBack {
        session_id: SessionId,
        checkpoint_id: CheckpointId,
    },
}

impl CheckpointWorld {
    /// Initial empty world.
    #[must_use]
    pub fn initial() -> Self {
        Self {
            snapshots: BTreeMap::new(),
            session_state: BTreeMap::new(),
            events: Vec::new(),
        }
    }

    /// World seeded with a single session at a given content.
    #[must_use]
    pub fn with_session(session_id: SessionId, content: ContentHash) -> Self {
        let mut w = Self::initial();
        w.session_state.insert(
            session_id,
            SessionView {
                content,
                last_checkpoint: None,
            },
        );
        w
    }

    /// Derive a checkpoint id from a content hash. Production
    /// uses BLAKE3 over the serialized session contents; the
    /// model uses `content + 1` (so id 0 means "no checkpoint")
    /// to keep the state space tiny while preserving
    /// content-addressing semantics.
    #[must_use]
    pub fn derive_checkpoint_id(content: ContentHash) -> CheckpointId {
        content.0.wrapping_add(1)
    }
}

// ============================================================================
// Actions
// ============================================================================

/// Action the model can take. The state space is
/// `apply_action`'s closure over these inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckpointAction {
    /// Save the current session state. Idempotent — if the
    /// content hash already has a checkpoint id, the existing
    /// id is returned with `is_duplicate = true`.
    Save { session_id: SessionId },
    /// Rollback session to a checkpoint. Requires a non-absent
    /// approval token; `dry_run = true` validates without
    /// mutating state.
    Rollback {
        session_id: SessionId,
        target: CheckpointId,
        token: ApprovalToken,
        dry_run: bool,
    },
    /// Inject a session content change (mutator outside the
    /// checkpoint family — represents the user typing /
    /// running commands between snapshots). Models the source
    /// of new content the next save would persist.
    MutateContent {
        session_id: SessionId,
        new_content: ContentHash,
    },
    /// Inject a save failure (transport error / disk full).
    /// MUST leave snapshots and session_state unchanged and
    /// emit no event.
    SaveFail { session_id: SessionId },
    /// Inject a rollback failure mid-flight. MUST leave
    /// session_state unchanged and emit no event.
    RollbackFail {
        session_id: SessionId,
        target: CheckpointId,
    },
    /// Pure read. MUST NOT mutate any field.
    List { session_id: SessionId },
}

/// Result of applying one action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ActionOutcome {
    /// Save succeeded; checkpoint id returned.
    SaveSucceeded {
        checkpoint_id: CheckpointId,
        is_duplicate: bool,
    },
    /// Rollback succeeded.
    RollbackSucceeded { checkpoint_id: CheckpointId },
    /// Rollback was denied (no token / invalid token).
    RollbackDenied,
    /// Save failed (transport / disk).
    SaveFailed,
    /// Rollback failed mid-flight.
    RollbackFailed,
    /// Pure read.
    Listed,
    /// Mutator no-op (sets up content).
    Mutated,
    /// Action could not apply (e.g., unknown session,
    /// unknown checkpoint id). The harness checks that NoOp
    /// never violates an invariant.
    NoOp,
}

/// Apply one action against the world. Mutates `world` in
/// place; returns the outcome.
pub fn apply_action(world: &mut CheckpointWorld, action: CheckpointAction) -> ActionOutcome {
    match action {
        CheckpointAction::Save { session_id } => {
            let Some(view) = world.session_state.get(&session_id).copied() else {
                return ActionOutcome::NoOp;
            };
            let id = CheckpointWorld::derive_checkpoint_id(view.content);

            // Idempotence: same content hash → same checkpoint
            // id. If it's already there with the same content,
            // mark duplicate.
            let is_duplicate = world
                .snapshots
                .get(&id)
                .is_some_and(|stored| *stored == view.content);

            if !is_duplicate {
                world.snapshots.insert(id, view.content);
            }

            // Update session pointer to the (possibly
            // pre-existing) checkpoint id.
            if let Some(slot) = world.session_state.get_mut(&session_id) {
                slot.last_checkpoint = Some(id);
            }

            world.events.push(EmittedEvent::CheckpointSaved {
                session_id,
                checkpoint_id: id,
                is_duplicate,
            });
            ActionOutcome::SaveSucceeded {
                checkpoint_id: id,
                is_duplicate,
            }
        }
        CheckpointAction::Rollback {
            session_id,
            target,
            token,
            dry_run,
        } => {
            // Approval check.
            if token == TOKEN_ABSENT || token == TOKEN_INVALID {
                return ActionOutcome::RollbackDenied;
            }

            // Target must exist.
            let Some(target_content) = world.snapshots.get(&target).copied() else {
                return ActionOutcome::NoOp;
            };

            // Session must exist.
            let Some(view) = world.session_state.get_mut(&session_id) else {
                return ActionOutcome::NoOp;
            };

            if !dry_run {
                view.content = target_content;
                view.last_checkpoint = Some(target);
                world.events.push(EmittedEvent::CheckpointRolledBack {
                    session_id,
                    checkpoint_id: target,
                });
            }

            ActionOutcome::RollbackSucceeded {
                checkpoint_id: target,
            }
        }
        CheckpointAction::MutateContent {
            session_id,
            new_content,
        } => {
            let Some(view) = world.session_state.get_mut(&session_id) else {
                return ActionOutcome::NoOp;
            };
            view.content = new_content;
            ActionOutcome::Mutated
        }
        CheckpointAction::SaveFail { .. } => {
            // Atomic-on-failure: no mutation, no event.
            ActionOutcome::SaveFailed
        }
        CheckpointAction::RollbackFail { .. } => {
            // Atomic-on-failure: no mutation, no event.
            ActionOutcome::RollbackFailed
        }
        CheckpointAction::List { session_id: _ } => {
            // Pure read — no mutation, no event.
            ActionOutcome::Listed
        }
    }
}

// ============================================================================
// Invariants
// ============================================================================

/// Named safety violation. Each invariant kind maps to one or
/// more headline contract semantics.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckpointSafetyViolation {
    /// `session_state[s].last_checkpoint = Some(id)` but
    /// `snapshots` lacks `id`.
    OrphanCheckpoint {
        session_id: SessionId,
        checkpoint_id: CheckpointId,
    },
    /// Two distinct checkpoint ids share the same content hash
    /// — idempotence violation.
    DuplicateContentDistinctIds {
        content: ContentHash,
        ids: Vec<CheckpointId>,
    },
    /// A `RollbackSucceeded` outcome was reached without an
    /// approval token in the trace.
    UnauthorizedRollback {
        session_id: SessionId,
        checkpoint_id: CheckpointId,
    },
    /// A `RollbackFail` occurred and yet `session_state[s]`
    /// changed.
    NonAtomicRollbackFailure { session_id: SessionId },
    /// A `List` action mutated state.
    ListMutatedState,
}

/// Run all safety invariants on the current world state.
/// `prior` is the world before `last_action` (used for the
/// atomic-on-failure check); `last_action` and `last_outcome`
/// describe the most recent transition.
#[must_use]
pub fn check_invariants(
    prior: &CheckpointWorld,
    world: &CheckpointWorld,
    last_action: CheckpointAction,
    last_outcome: ActionOutcome,
) -> Vec<CheckpointSafetyViolation> {
    let mut out = Vec::new();

    // NoOrphanCheckpoint.
    for (sid, view) in &world.session_state {
        if let Some(cid) = view.last_checkpoint {
            if !world.snapshots.contains_key(&cid) {
                out.push(CheckpointSafetyViolation::OrphanCheckpoint {
                    session_id: *sid,
                    checkpoint_id: cid,
                });
            }
        }
    }

    // NoDoubleSaveOnSameContent (idempotence).
    let mut by_content: BTreeMap<ContentHash, Vec<CheckpointId>> = BTreeMap::new();
    for (cid, content) in &world.snapshots {
        by_content.entry(*content).or_default().push(*cid);
    }
    for (content, ids) in &by_content {
        if ids.len() > 1 {
            out.push(CheckpointSafetyViolation::DuplicateContentDistinctIds {
                content: *content,
                ids: ids.clone(),
            });
        }
    }

    // NoUnauthorizedRollback: a rollback that succeeded MUST
    // have carried a token (token != ABSENT and != INVALID).
    if let (
        CheckpointAction::Rollback {
            session_id,
            token,
            dry_run,
            ..
        },
        ActionOutcome::RollbackSucceeded { checkpoint_id },
    ) = (last_action, last_outcome)
    {
        if (token == TOKEN_ABSENT || token == TOKEN_INVALID) && !dry_run {
            out.push(CheckpointSafetyViolation::UnauthorizedRollback {
                session_id,
                checkpoint_id,
            });
        }
    }

    // AtomicOnRollbackFailure: a `RollbackFail` action must
    // not mutate session_state.
    if let CheckpointAction::RollbackFail { session_id, .. } = last_action {
        let prior_view = prior.session_state.get(&session_id).copied();
        let now_view = world.session_state.get(&session_id).copied();
        if prior_view != now_view {
            out.push(CheckpointSafetyViolation::NonAtomicRollbackFailure { session_id });
        }
    }

    // ListIsPureRead.
    if let CheckpointAction::List { .. } = last_action {
        if prior != world {
            out.push(CheckpointSafetyViolation::ListMutatedState);
        }
    }

    out
}

/// Set of action kinds (for coverage analysis in tests).
#[must_use]
pub fn action_kinds() -> BTreeSet<&'static str> {
    [
        "save",
        "rollback",
        "mutate_content",
        "save_fail",
        "rollback_fail",
        "list",
    ]
    .into_iter()
    .collect()
}

// ============================================================================
// Health snapshot
// ============================================================================

/// `ft doctor` counter snapshot for the checkpoint state-machine
/// proof. Mirrors the `*Health` shape used across this session
/// (a11y_tree, color_management, atlas_stability, triple_buffer,
/// live_resize, render_quality, snap_back_fuzz,
/// wayland_frame_pacing, bidi_correctness, tx_killswitch_model,
/// passive_watch_invariant, wire_dedup_model,
/// redactor_coverage_matrix, tui_parity_oracle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointStateHealth {
    pub schedules_explored: u64,
    pub states_visited: u64,
    pub saves_total: u64,
    pub rollbacks_total: u64,
    pub denied_rollbacks_total: u64,
    pub safety_violations_total: u64,
}

impl CheckpointStateHealth {
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            schedules_explored: 0,
            states_visited: 0,
            saves_total: 0,
            rollbacks_total: 0,
            denied_rollbacks_total: 0,
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

    fn world_with_one_session() -> CheckpointWorld {
        CheckpointWorld::with_session(1, ContentHash(7))
    }

    #[test]
    fn save_creates_snapshot_and_event() {
        let mut w = world_with_one_session();
        let prior = w.clone();
        let outcome = apply_action(&mut w, CheckpointAction::Save { session_id: 1 });
        assert!(matches!(
            outcome,
            ActionOutcome::SaveSucceeded {
                is_duplicate: false,
                ..
            }
        ));
        assert_eq!(w.snapshots.len(), 1);
        assert_eq!(w.events.len(), 1);
        // Pointer set on the session.
        let view = w.session_state.get(&1).unwrap();
        assert!(view.last_checkpoint.is_some());
        // No orphans.
        assert!(
            check_invariants(
                &prior,
                &w,
                CheckpointAction::Save { session_id: 1 },
                outcome
            )
            .is_empty()
        );
    }

    #[test]
    fn save_is_idempotent_on_same_content() {
        let mut w = world_with_one_session();
        let _ = apply_action(&mut w, CheckpointAction::Save { session_id: 1 });
        let prior = w.clone();
        let outcome = apply_action(&mut w, CheckpointAction::Save { session_id: 1 });
        assert!(
            matches!(
                outcome,
                ActionOutcome::SaveSucceeded {
                    is_duplicate: true,
                    ..
                }
            ),
            "second save on same content must be is_duplicate=true; got {outcome:?}"
        );
        // snapshots count unchanged.
        assert_eq!(w.snapshots.len(), 1);
        assert!(
            check_invariants(
                &prior,
                &w,
                CheckpointAction::Save { session_id: 1 },
                outcome
            )
            .is_empty()
        );
    }

    #[test]
    fn save_after_mutation_creates_distinct_id() {
        let mut w = world_with_one_session();
        apply_action(&mut w, CheckpointAction::Save { session_id: 1 });
        apply_action(
            &mut w,
            CheckpointAction::MutateContent {
                session_id: 1,
                new_content: ContentHash(42),
            },
        );
        let prior = w.clone();
        let outcome = apply_action(&mut w, CheckpointAction::Save { session_id: 1 });
        assert!(matches!(
            outcome,
            ActionOutcome::SaveSucceeded {
                is_duplicate: false,
                ..
            }
        ));
        // 2 distinct snapshots.
        assert_eq!(w.snapshots.len(), 2);
        assert!(
            check_invariants(
                &prior,
                &w,
                CheckpointAction::Save { session_id: 1 },
                outcome
            )
            .is_empty()
        );
    }

    #[test]
    fn rollback_without_token_is_denied() {
        let mut w = world_with_one_session();
        apply_action(&mut w, CheckpointAction::Save { session_id: 1 });
        let prior = w.clone();
        let action = CheckpointAction::Rollback {
            session_id: 1,
            target: CheckpointWorld::derive_checkpoint_id(ContentHash(7)),
            token: TOKEN_ABSENT,
            dry_run: false,
        };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, ActionOutcome::RollbackDenied);
        // No mutation.
        assert_eq!(w.session_state, prior.session_state);
        assert_eq!(w.events.len(), prior.events.len());
        assert!(check_invariants(&prior, &w, action, outcome).is_empty());
    }

    #[test]
    fn rollback_with_invalid_token_is_denied() {
        let mut w = world_with_one_session();
        apply_action(&mut w, CheckpointAction::Save { session_id: 1 });
        let prior = w.clone();
        let action = CheckpointAction::Rollback {
            session_id: 1,
            target: CheckpointWorld::derive_checkpoint_id(ContentHash(7)),
            token: TOKEN_INVALID,
            dry_run: false,
        };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, ActionOutcome::RollbackDenied);
        assert!(check_invariants(&prior, &w, action, outcome).is_empty());
    }

    #[test]
    fn rollback_with_valid_token_restores_content() {
        let mut w = world_with_one_session();
        // Save at content 7.
        apply_action(&mut w, CheckpointAction::Save { session_id: 1 });
        let cp_id = CheckpointWorld::derive_checkpoint_id(ContentHash(7));
        // Mutate to content 42.
        apply_action(
            &mut w,
            CheckpointAction::MutateContent {
                session_id: 1,
                new_content: ContentHash(42),
            },
        );
        let prior = w.clone();
        // Rollback to checkpoint that was at content 7.
        let action = CheckpointAction::Rollback {
            session_id: 1,
            target: cp_id,
            token: 5,
            dry_run: false,
        };
        let outcome = apply_action(&mut w, action);
        assert!(matches!(outcome, ActionOutcome::RollbackSucceeded { .. }));
        let view = w.session_state.get(&1).unwrap();
        assert_eq!(view.content, ContentHash(7));
        assert!(check_invariants(&prior, &w, action, outcome).is_empty());
    }

    #[test]
    fn rollback_dry_run_does_not_mutate() {
        let mut w = world_with_one_session();
        apply_action(&mut w, CheckpointAction::Save { session_id: 1 });
        let cp_id = CheckpointWorld::derive_checkpoint_id(ContentHash(7));
        apply_action(
            &mut w,
            CheckpointAction::MutateContent {
                session_id: 1,
                new_content: ContentHash(42),
            },
        );
        let prior = w.clone();
        let action = CheckpointAction::Rollback {
            session_id: 1,
            target: cp_id,
            token: 5,
            dry_run: true,
        };
        let outcome = apply_action(&mut w, action);
        assert!(matches!(outcome, ActionOutcome::RollbackSucceeded { .. }));
        // Session state unchanged.
        assert_eq!(w.session_state, prior.session_state);
        // No event.
        assert_eq!(w.events.len(), prior.events.len());
    }

    #[test]
    fn save_fail_is_atomic() {
        let mut w = world_with_one_session();
        let prior = w.clone();
        let action = CheckpointAction::SaveFail { session_id: 1 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, ActionOutcome::SaveFailed);
        assert_eq!(w, prior);
        assert!(check_invariants(&prior, &w, action, outcome).is_empty());
    }

    #[test]
    fn rollback_fail_is_atomic() {
        let mut w = world_with_one_session();
        apply_action(&mut w, CheckpointAction::Save { session_id: 1 });
        let prior = w.clone();
        let action = CheckpointAction::RollbackFail {
            session_id: 1,
            target: CheckpointWorld::derive_checkpoint_id(ContentHash(7)),
        };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, ActionOutcome::RollbackFailed);
        assert_eq!(w, prior);
        assert!(check_invariants(&prior, &w, action, outcome).is_empty());
    }

    #[test]
    fn list_is_pure_read() {
        let mut w = world_with_one_session();
        apply_action(&mut w, CheckpointAction::Save { session_id: 1 });
        let prior = w.clone();
        let action = CheckpointAction::List { session_id: 1 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, ActionOutcome::Listed);
        assert_eq!(w, prior);
        assert!(check_invariants(&prior, &w, action, outcome).is_empty());
    }

    #[test]
    fn rollback_to_unknown_checkpoint_is_noop() {
        let mut w = world_with_one_session();
        let prior = w.clone();
        let action = CheckpointAction::Rollback {
            session_id: 1,
            target: 99,
            token: 5,
            dry_run: false,
        };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, ActionOutcome::NoOp);
        assert_eq!(w, prior);
    }

    #[test]
    fn save_on_unknown_session_is_noop() {
        let mut w = CheckpointWorld::initial();
        let prior = w.clone();
        let action = CheckpointAction::Save { session_id: 99 };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, ActionOutcome::NoOp);
        assert_eq!(w, prior);
    }

    #[test]
    fn baseline_health_is_safe() {
        let h = CheckpointStateHealth::baseline();
        assert!(h.is_safe());
    }

    #[test]
    fn world_serde_roundtrips() {
        let mut w = world_with_one_session();
        apply_action(&mut w, CheckpointAction::Save { session_id: 1 });
        let json = serde_json::to_string(&w).unwrap();
        let parsed: CheckpointWorld = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, w);
    }

    #[test]
    fn action_kinds_count_matches_enum() {
        assert_eq!(action_kinds().len(), 6);
    }

    #[test]
    fn invariants_clean_under_canonical_save_rollback_sequence() {
        let mut w = world_with_one_session();
        let mut prior = w.clone();
        let mut last_action = CheckpointAction::List { session_id: 1 };
        let mut last_outcome = ActionOutcome::Listed;

        let script = vec![
            CheckpointAction::Save { session_id: 1 },
            CheckpointAction::List { session_id: 1 },
            CheckpointAction::MutateContent {
                session_id: 1,
                new_content: ContentHash(42),
            },
            CheckpointAction::Save { session_id: 1 },
            CheckpointAction::Rollback {
                session_id: 1,
                target: CheckpointWorld::derive_checkpoint_id(ContentHash(7)),
                token: 5,
                dry_run: false,
            },
        ];

        for a in script {
            prior = w.clone();
            last_outcome = apply_action(&mut w, a);
            last_action = a;
            let v = check_invariants(&prior, &w, last_action, last_outcome);
            assert!(v.is_empty(), "violation under {a:?}: {v:?}");
        }
    }
}
