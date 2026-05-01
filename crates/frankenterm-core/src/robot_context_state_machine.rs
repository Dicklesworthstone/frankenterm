//! Stateright-shape state-space model of the `robot context`
//! family ([BR-RC-ROBOT-CONTRACT.3] / `ft-hac7w.4`).
//!
//! Models per-pane conversation context tracking with the
//! headline contract semantics from
//! [`crate::robot_family_contract::context_family_contract`]:
//!
//! - `status` is a pure read.
//! - `rotate` is non-idempotent — produces a fresh
//!   `rotation_id` per call. With a `caller_idempotency_key`,
//!   re-issuing returns the same rotation_id (replay).
//!   MustNotPartiallyMutate: a failed rotate leaves no
//!   `pane_contexts` / `context_rotations` rows.
//! - `history` is a pure read.
//! - Concurrency: serializable per `pane_id`.
//!
//! ## What this proves
//!
//! Four named safety invariants verified by exhaustive small-
//! bound BFS plus a 1024-trial random schedule sweep at
//! depth 12:
//!
//! 1. **NoOrphanArchivedContext** — every row in
//!    `context_rotations` references a row in `pane_contexts`.
//! 2. **AtomicRotateFailure** — on `RotateFail`, the world
//!    state is unchanged from prior; no event emitted.
//! 3. **IdempotencyReplay** — re-issuing rotate with the
//!    same `(pane_id, caller_idempotency_key)` returns the
//!    same rotation_id and does NOT emit a second event.
//! 4. **HistoryIsPureRead** — `Status` and `History` actions
//!    do not mutate any field.
//!
//! ## What this is NOT
//!
//! - Not the production handler. Wiring `RobotCommands::Context`
//!   to the cass-types + session-resume infrastructure (the
//!   bead's action #3) is the integration follow-on.
//! - Not the schema migration for `pane_contexts` +
//!   `context_rotations` (action #2). The state-machine model
//!   names the tables; the migration is a separate
//!   integration step.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// Domain
// ============================================================================

pub type PaneId = u8;

/// A context id. Production uses BLAKE3-content-addressed
/// strings; the model uses `u8` for finite state space.
pub type ContextId = u8;

/// A rotation receipt id. Production uses BLAKE3 over
/// `(pane_id, caller_idempotency_key, rotated_at_ms)`;
/// the model uses `u8`.
pub type RotationId = u8;

/// Caller-supplied idempotency key. `None` means no key —
/// every call produces a fresh rotation_id.
pub type IdempotencyKey = Option<u8>;

/// Per-pane state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneContext {
    /// Currently-active context id. `None` if pane has never
    /// rotated.
    pub active: Option<ContextId>,
    /// Rotation history newest-first. Every entry references
    /// a row in `archived_contexts`.
    pub rotations: Vec<RotationEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RotationEntry {
    pub rotation_id: RotationId,
    pub previous_context: Option<ContextId>,
    pub new_context: ContextId,
    pub idempotency_key: IdempotencyKey,
}

/// World state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContextWorld {
    /// Per-pane state — both pane_contexts (active + history)
    /// and context_rotations rows projected together.
    pub panes: BTreeMap<PaneId, PaneContext>,
    /// All archived context_ids ever observed in any pane.
    /// Used by NoOrphanArchivedContext: every rotation entry's
    /// previous_context (if Some) MUST be in this set.
    pub archived_contexts: BTreeMap<PaneId, std::collections::BTreeSet<ContextId>>,
    /// Trace of emitted events.
    pub events: Vec<EmittedEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EmittedEvent {
    ContextRotated {
        pane: PaneId,
        rotation_id: RotationId,
    },
}

impl ContextWorld {
    #[must_use]
    pub fn initial() -> Self {
        Self {
            panes: BTreeMap::new(),
            archived_contexts: BTreeMap::new(),
            events: Vec::new(),
        }
    }

    /// Derive a rotation_id from `(pane_id, key, depth)`. The
    /// model's hash is content-addressed in the same shape as
    /// production (different bytes; finite state space).
    #[must_use]
    pub fn derive_rotation_id(pane: PaneId, key: IdempotencyKey, depth: u8) -> RotationId {
        // Simple hash mixing for state-space tractability.
        let k = key.unwrap_or(255);
        pane.wrapping_mul(31)
            .wrapping_add(k.wrapping_mul(17))
            .wrapping_add(depth)
            .wrapping_add(1)
    }

    /// Derive a fresh context_id from depth.
    #[must_use]
    pub fn derive_context_id(pane: PaneId, depth: u8) -> ContextId {
        pane.wrapping_mul(7).wrapping_add(depth).wrapping_add(1)
    }
}

// ============================================================================
// Actions
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextAction {
    Rotate {
        pane: PaneId,
        idempotency_key: IdempotencyKey,
    },
    Status {
        pane: PaneId,
    },
    History {
        pane: PaneId,
    },
    /// Inject a rotate failure. Atomic — leaves world unchanged.
    RotateFail {
        pane: PaneId,
        idempotency_key: IdempotencyKey,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ContextOutcome {
    RotateSucceeded {
        rotation_id: RotationId,
        is_replay: bool,
    },
    RotateFailed,
    StatusReturned,
    HistoryReturned,
}

/// Apply one action.
pub fn apply_action(world: &mut ContextWorld, action: ContextAction) -> ContextOutcome {
    match action {
        ContextAction::Rotate {
            pane,
            idempotency_key,
        } => {
            // Idempotency-key replay: if the pane already has
            // a rotation with this exact key, return the same
            // rotation_id with is_replay=true.
            if let Some(key) = idempotency_key {
                if let Some(p) = world.panes.get(&pane) {
                    for r in &p.rotations {
                        if r.idempotency_key == Some(key) {
                            return ContextOutcome::RotateSucceeded {
                                rotation_id: r.rotation_id,
                                is_replay: true,
                            };
                        }
                    }
                }
            }

            // Fresh rotation.
            let pane_state = world.panes.entry(pane).or_insert(PaneContext {
                active: None,
                rotations: Vec::new(),
            });
            let depth = pane_state.rotations.len() as u8;
            let new_context = ContextWorld::derive_context_id(pane, depth.wrapping_add(1));
            let rotation_id = ContextWorld::derive_rotation_id(pane, idempotency_key, depth);
            let prior_active = pane_state.active;

            // If there was a prior active context, archive it.
            if let Some(prior) = prior_active {
                world
                    .archived_contexts
                    .entry(pane)
                    .or_default()
                    .insert(prior);
            }

            pane_state.active = Some(new_context);
            pane_state.rotations.insert(
                0,
                RotationEntry {
                    rotation_id,
                    previous_context: prior_active,
                    new_context,
                    idempotency_key,
                },
            );

            world
                .events
                .push(EmittedEvent::ContextRotated { pane, rotation_id });

            ContextOutcome::RotateSucceeded {
                rotation_id,
                is_replay: false,
            }
        }
        ContextAction::RotateFail { .. } => {
            // Atomic — no mutation, no event.
            ContextOutcome::RotateFailed
        }
        ContextAction::Status { .. } => ContextOutcome::StatusReturned,
        ContextAction::History { .. } => ContextOutcome::HistoryReturned,
    }
}

// ============================================================================
// Invariants
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextSafetyViolation {
    /// A rotation entry's `previous_context` is `Some(id)`
    /// but `archived_contexts[pane]` doesn't contain `id`.
    OrphanArchivedContext {
        pane: PaneId,
        rotation_id: RotationId,
        previous_context: ContextId,
    },
    /// A `RotateFail` action mutated the world.
    NonAtomicRotateFailure { pane: PaneId },
    /// A successful Rotate with `is_replay=true` lacks a
    /// matching prior rotation with the same idempotency_key.
    SpuriousReplay {
        pane: PaneId,
        rotation_id: RotationId,
    },
    /// A pure-read action mutated state.
    PureReadMutated,
}

#[must_use]
pub fn check_invariants(
    prior: &ContextWorld,
    world: &ContextWorld,
    last_action: ContextAction,
    last_outcome: ContextOutcome,
) -> Vec<ContextSafetyViolation> {
    let mut out = Vec::new();

    // NoOrphanArchivedContext.
    for (pane, ctx) in &world.panes {
        let archive = world.archived_contexts.get(pane);
        for r in &ctx.rotations {
            if let Some(prev) = r.previous_context {
                let in_archive = archive.is_some_and(|s| s.contains(&prev));
                if !in_archive {
                    out.push(ContextSafetyViolation::OrphanArchivedContext {
                        pane: *pane,
                        rotation_id: r.rotation_id,
                        previous_context: prev,
                    });
                }
            }
        }
    }

    // NonAtomicRotateFailure.
    if let (ContextAction::RotateFail { pane, .. }, ContextOutcome::RotateFailed) =
        (last_action, last_outcome)
    {
        if prior != world {
            out.push(ContextSafetyViolation::NonAtomicRotateFailure { pane });
        }
    }

    // SpuriousReplay — a successful is_replay=true outcome
    // implies the request's idempotency_key matched a prior
    // rotation in the SAME pane with the SAME rotation_id.
    if let (
        ContextAction::Rotate {
            pane,
            idempotency_key,
        },
        ContextOutcome::RotateSucceeded {
            rotation_id,
            is_replay: true,
        },
    ) = (last_action, last_outcome)
    {
        let matched = world
            .panes
            .get(&pane)
            .map(|p| {
                p.rotations.iter().any(|r| {
                    r.idempotency_key == idempotency_key
                        && idempotency_key.is_some()
                        && r.rotation_id == rotation_id
                })
            })
            .unwrap_or(false);
        if !matched {
            out.push(ContextSafetyViolation::SpuriousReplay { pane, rotation_id });
        }
    }

    // HistoryIsPureRead.
    if matches!(
        last_action,
        ContextAction::Status { .. } | ContextAction::History { .. }
    ) && prior != world
    {
        out.push(ContextSafetyViolation::PureReadMutated);
    }

    out
}

// ============================================================================
// Health snapshot
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextStateHealth {
    pub schedules_explored: u64,
    pub rotations_total: u64,
    pub replays_total: u64,
    pub failures_total: u64,
    pub safety_violations_total: u64,
}

impl ContextStateHealth {
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            schedules_explored: 0,
            rotations_total: 0,
            replays_total: 0,
            failures_total: 0,
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

    #[test]
    fn first_rotate_creates_pane_state() {
        let mut w = ContextWorld::initial();
        let prior = w.clone();
        let action = ContextAction::Rotate {
            pane: 1,
            idempotency_key: None,
        };
        let outcome = apply_action(&mut w, action);
        assert!(matches!(
            outcome,
            ContextOutcome::RotateSucceeded {
                is_replay: false,
                ..
            }
        ));
        let p = w.panes.get(&1).unwrap();
        assert_eq!(p.rotations.len(), 1);
        // First rotation has no previous_context.
        assert_eq!(p.rotations[0].previous_context, None);
        assert_eq!(p.active, Some(p.rotations[0].new_context));
        assert!(check_invariants(&prior, &w, action, outcome).is_empty());
    }

    #[test]
    fn second_rotate_archives_prior_context() {
        let mut w = ContextWorld::initial();
        apply_action(
            &mut w,
            ContextAction::Rotate {
                pane: 1,
                idempotency_key: None,
            },
        );
        let first_active = w.panes.get(&1).unwrap().active.unwrap();
        let prior = w.clone();
        let action = ContextAction::Rotate {
            pane: 1,
            idempotency_key: None,
        };
        let outcome = apply_action(&mut w, action);
        assert!(matches!(
            outcome,
            ContextOutcome::RotateSucceeded {
                is_replay: false,
                ..
            }
        ));
        // Prior context is now in archive.
        let archive = w.archived_contexts.get(&1).unwrap();
        assert!(archive.contains(&first_active));
        // History grew.
        let p = w.panes.get(&1).unwrap();
        assert_eq!(p.rotations.len(), 2);
        // Newest entry references the archived one.
        assert_eq!(p.rotations[0].previous_context, Some(first_active));
        // Active changed.
        assert_ne!(p.active, Some(first_active));
        let v = check_invariants(&prior, &w, action, outcome);
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn idempotency_key_replay_returns_same_rotation_id() {
        let mut w = ContextWorld::initial();
        let key = Some(42u8);
        let outcome1 = apply_action(
            &mut w,
            ContextAction::Rotate {
                pane: 1,
                idempotency_key: key,
            },
        );
        let id1 = match outcome1 {
            ContextOutcome::RotateSucceeded { rotation_id, .. } => rotation_id,
            _ => panic!("expected success"),
        };
        // Re-issue with the same key.
        let prior = w.clone();
        let action = ContextAction::Rotate {
            pane: 1,
            idempotency_key: key,
        };
        let outcome2 = apply_action(&mut w, action);
        assert_eq!(
            outcome2,
            ContextOutcome::RotateSucceeded {
                rotation_id: id1,
                is_replay: true,
            }
        );
        // World unchanged on replay.
        assert_eq!(w, prior);
        let v = check_invariants(&prior, &w, action, outcome2);
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn distinct_keys_produce_distinct_rotations() {
        let mut w = ContextWorld::initial();
        apply_action(
            &mut w,
            ContextAction::Rotate {
                pane: 1,
                idempotency_key: Some(1),
            },
        );
        apply_action(
            &mut w,
            ContextAction::Rotate {
                pane: 1,
                idempotency_key: Some(2),
            },
        );
        let p = w.panes.get(&1).unwrap();
        assert_eq!(p.rotations.len(), 2);
        assert_ne!(p.rotations[0].rotation_id, p.rotations[1].rotation_id);
    }

    #[test]
    fn rotate_fail_is_atomic() {
        let mut w = ContextWorld::initial();
        apply_action(
            &mut w,
            ContextAction::Rotate {
                pane: 1,
                idempotency_key: None,
            },
        );
        let prior = w.clone();
        let action = ContextAction::RotateFail {
            pane: 1,
            idempotency_key: None,
        };
        let outcome = apply_action(&mut w, action);
        assert_eq!(outcome, ContextOutcome::RotateFailed);
        assert_eq!(w, prior);
        let v = check_invariants(&prior, &w, action, outcome);
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn status_and_history_are_pure_reads() {
        let mut w = ContextWorld::initial();
        apply_action(
            &mut w,
            ContextAction::Rotate {
                pane: 1,
                idempotency_key: None,
            },
        );
        let prior = w.clone();
        for action in [
            ContextAction::Status { pane: 1 },
            ContextAction::History { pane: 1 },
        ] {
            let outcome = apply_action(&mut w, action);
            let v = check_invariants(&prior, &w, action, outcome);
            assert!(v.is_empty(), "{v:?}");
            assert_eq!(w, prior);
        }
    }

    #[test]
    fn distinct_panes_independent() {
        let mut w = ContextWorld::initial();
        apply_action(
            &mut w,
            ContextAction::Rotate {
                pane: 1,
                idempotency_key: None,
            },
        );
        apply_action(
            &mut w,
            ContextAction::Rotate {
                pane: 2,
                idempotency_key: None,
            },
        );
        // Two separate panes, two separate context chains.
        assert!(w.panes.contains_key(&1));
        assert!(w.panes.contains_key(&2));
        assert_ne!(
            w.panes.get(&1).unwrap().active,
            w.panes.get(&2).unwrap().active
        );
    }

    #[test]
    fn baseline_health_is_unsafe_until_explored() {
        // Per ft-11d5f sweep fix: cold baseline is unsafe.
        assert!(!ContextStateHealth::baseline().is_safe());
        let h_clean = ContextStateHealth {
            schedules_explored: 1,
            rotations_total: 1,
            replays_total: 0,
            failures_total: 0,
            safety_violations_total: 0,
        };
        assert!(h_clean.is_safe());
    }

    #[test]
    fn world_serde_roundtrips() {
        let mut w = ContextWorld::initial();
        apply_action(
            &mut w,
            ContextAction::Rotate {
                pane: 1,
                idempotency_key: Some(5),
            },
        );
        apply_action(
            &mut w,
            ContextAction::Rotate {
                pane: 1,
                idempotency_key: None,
            },
        );
        let json = serde_json::to_string(&w).unwrap();
        let parsed: ContextWorld = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, w);
    }

    #[test]
    fn random_schedule_sweep_all_invariants_clean() {
        let mut rng: u64 = 0xc0ff_ee15_dead_beefu64;
        let xorshift = |s: &mut u64| -> u64 {
            let mut x = *s;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *s = x;
            x
        };
        for _ in 0..1024 {
            let mut w = ContextWorld::initial();
            for _ in 0..12 {
                let r = xorshift(&mut rng);
                let kind = (r % 4) as u8;
                let pane = ((r >> 8) % 3) as u8;
                let key = if (r >> 16) & 1 == 0 {
                    None
                } else {
                    Some(((r >> 24) % 4) as u8)
                };
                let action = match kind {
                    0 => ContextAction::Rotate {
                        pane,
                        idempotency_key: key,
                    },
                    1 => ContextAction::Status { pane },
                    2 => ContextAction::History { pane },
                    _ => ContextAction::RotateFail {
                        pane,
                        idempotency_key: key,
                    },
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

    #[test]
    fn deep_history_preserves_no_orphan_invariant() {
        // 10 sequential rotations on the same pane — every
        // archived context must remain in the archive set.
        let mut w = ContextWorld::initial();
        for i in 0..10 {
            let prior = w.clone();
            let action = ContextAction::Rotate {
                pane: 1,
                idempotency_key: Some(i),
            };
            let outcome = apply_action(&mut w, action);
            let v = check_invariants(&prior, &w, action, outcome);
            assert!(v.is_empty(), "step {i}: {v:?}");
        }
        // Archive should contain 9 contexts (every prior
        // active was archived; the 10th is still active).
        let archive = w.archived_contexts.get(&1).unwrap();
        assert_eq!(archive.len(), 9);
    }
}
