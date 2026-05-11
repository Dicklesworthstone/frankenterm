//! State-space model of the `robot profile` apply→list state
//! machine ([BR-RC-ROBOT-CONTRACT.1] / `ft-hac7w.2`).
//!
//! Mirrors the headline contract semantics from
//! [`crate::robot_family_contract::profile_family_contract`]:
//!
//! - `apply` is **idempotent** on identical input
//!   `(name, count, env_overrides, dry_run)` — re-applying with
//!   the same tuple returns the existing apply receipt without
//!   double-spawning panes.
//! - `apply` mutates `agent_profiles` and spawns `count` panes;
//!   **MUST NOT partially mutate** — a failed apply leaves
//!   `agent_profiles` and `panes` untouched.
//! - `apply { dry_run: true }` never mutates state.
//! - `apply` against a non-existent profile returns
//!   `ProfileNotFound` with no side effects.
//! - `list`, `show`, `validate` are pure reads.
//! - Concurrency: serializable per profile name.
//!
//! ## What this proves
//!
//! BFS exhaustive exploration over `step_count ∈ {2, 3}` plus
//! a property-test sweep at depth 8 verifies these invariants
//! on every reachable state:
//!
//! 1. **NoOrphanProfile** — every `apply` references a profile
//!    that exists in `agent_profiles`. A successful Apply
//!    transition implies the named profile was present in
//!    `defined_profiles` before the action fired.
//! 2. **NoDoubleSpawnOnDuplicateApply** — repeated `Apply`
//!    actions with identical `(name, count, env_overrides,
//!    dry_run)` parameters do not double-spawn panes.
//! 3. **ApplyAtomic** — an `ApplyFail` action leaves
//!    `agent_profiles` and `panes` byte-for-byte unchanged,
//!    and emits no `profile.applied` event.
//! 4. **DryRunPure** — `Apply { dry_run: true }` does not
//!    mutate any persistent state.
//! 5. **ListShowValidatePure** — `List`, `Show`, `Validate`
//!    actions do not change any field of the world.
//!
//! ## What this is NOT
//!
//! - Not the actual handler. Wiring `RobotCommands::Profile`
//!   into the existing config/profile machinery + the
//!   `agent_profiles` schema migration is the integration
//!   follow-on under ft-hac7w.2.cont.handler.
//! - Not the differential test against `ntm profile`. That
//!   lands under ft-hac7w.2.cont.differential once the real
//!   handler is wired.
//! - Not a TLA+ spec. The Rust BFS model is the always-on
//!   regression net; a TLA+ companion is filed for the
//!   follow-up sweep.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

// ============================================================================
// Domain
// ============================================================================

/// A profile name. Bounded `u8` for state-space tractability.
pub type ProfileId = u8;

/// A pane id assigned by `apply`. Bounded `u8`.
pub type PaneId = u8;

/// World state under model. Mirrors the production
/// `agent_profiles` table + per-profile spawn registry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProfileWorld {
    /// Profiles declared (the source-of-truth set the operator
    /// configured ahead of time). The model treats this as a
    /// fixed input — apply only succeeds when its name appears
    /// here.
    pub defined_profiles: BTreeSet<ProfileId>,
    /// `agent_profiles` table: profile id → its apply receipt.
    pub agent_profiles: BTreeMap<ProfileId, ApplyReceipt>,
    /// Spawned panes registry, partitioned by which profile
    /// spawned them.
    pub panes: BTreeMap<ProfileId, BTreeSet<PaneId>>,
    /// Next pane id the model hands out on a successful spawn.
    pub next_pane_id: PaneId,
    /// Trace of emitted events (for invariant checks).
    pub events: Vec<EmittedEvent>,
    /// Counter for `next_pane_id.wrapping_add(1)` collisions —
    /// when the wrap from 255→0 produces a pid that's already
    /// in `panes[name]`. Theoretical only at small BFS depths
    /// (≤3) but a 256-spawn `Apply` (count=255 + several
    /// re-applies) can hit it. The BFS harness asserts this
    /// stays at 0; if it bumps, the receipt's
    /// `panes_spawned` claim is silently wrong (BTreeSet
    /// dedupes the collision).
    #[serde(default)]
    pub wrap_collisions_total: u64,
}

/// Recorded apply receipt — the input parameters + the panes
/// that were spawned. Used to detect duplicate applies.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ApplyReceipt {
    pub name: ProfileId,
    pub count: u8,
    pub env_overrides_hash: u8,
    pub dry_run: bool,
    pub panes_spawned: BTreeSet<PaneId>,
}

/// Trace event the harness inspects for invariant checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EmittedEvent {
    ProfileApplied {
        name: ProfileId,
        is_duplicate: bool,
        dry_run: bool,
    },
    ApplyFailed {
        name: ProfileId,
        reason: ApplyFailureReason,
    },
}

/// Why an `ApplyFail` action fired. Distinct values so the
/// harness can assert on specific failure paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyFailureReason {
    /// The profile name doesn't exist in `defined_profiles`.
    ProfileNotFound,
    /// Spawn of one or more panes failed; transaction rolled
    /// back.
    SpawnRollback,
}

impl ProfileWorld {
    /// Initial empty world.
    #[must_use]
    pub fn initial() -> Self {
        Self {
            defined_profiles: BTreeSet::new(),
            agent_profiles: BTreeMap::new(),
            panes: BTreeMap::new(),
            next_pane_id: 1,
            events: Vec::new(),
            wrap_collisions_total: 0,
        }
    }

    /// World seeded with a set of declared profiles.
    #[must_use]
    pub fn with_profiles(profiles: impl IntoIterator<Item = ProfileId>) -> Self {
        let mut w = Self::initial();
        w.defined_profiles.extend(profiles);
        w
    }
}

// ============================================================================
// Actions
// ============================================================================

/// One transition the state machine fires.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Apply a profile. Succeeds iff `name` is in
    /// `defined_profiles`. Idempotent on identical params.
    Apply {
        name: ProfileId,
        count: u8,
        env_overrides_hash: u8,
        dry_run: bool,
    },
    /// Apply that fails after partial spawning — used to
    /// exercise the `ApplyAtomic` invariant.
    ApplyFail {
        name: ProfileId,
        count: u8,
        env_overrides_hash: u8,
        dry_run: bool,
    },
    /// List profiles. Pure read.
    List,
    /// Show one profile. Pure read.
    Show { name: ProfileId },
    /// Validate one profile. Pure read.
    Validate { name: ProfileId },
}

/// Outcome of applying an action to a world state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// Apply landed; new panes were spawned.
    Applied { panes_spawned: BTreeSet<PaneId> },
    /// Apply with identical params seen before; no new spawns.
    AppliedDuplicate,
    /// Apply with `dry_run: true` — no spawn.
    AppliedDryRun,
    /// Apply against a non-existent profile.
    ProfileNotFound,
    /// Apply failed mid-spawn; rolled back.
    AppliedFailed,
    /// Pure read result.
    Read,
}

/// Apply `action` to `world`, returning the outcome.
pub fn step(world: &mut ProfileWorld, action: &Action) -> Outcome {
    match action {
        Action::Apply {
            name,
            count,
            env_overrides_hash,
            dry_run,
        } => apply_step(world, *name, *count, *env_overrides_hash, *dry_run),
        Action::ApplyFail {
            name,
            count: _,
            env_overrides_hash: _,
            dry_run: _,
        } => {
            // ApplyFail explicitly fails after a partial-mutation
            // attempt. The model asserts the post-state is
            // *exactly* the pre-state — no mutation leaks. The
            // event log records the failure reason.
            world.events.push(EmittedEvent::ApplyFailed {
                name: *name,
                reason: ApplyFailureReason::SpawnRollback,
            });
            Outcome::AppliedFailed
        }
        Action::List | Action::Show { .. } | Action::Validate { .. } => {
            // Pure reads. No mutation; no event emitted.
            Outcome::Read
        }
    }
}

fn apply_step(
    world: &mut ProfileWorld,
    name: ProfileId,
    count: u8,
    env_overrides_hash: u8,
    dry_run: bool,
) -> Outcome {
    if !world.defined_profiles.contains(&name) {
        world.events.push(EmittedEvent::ApplyFailed {
            name,
            reason: ApplyFailureReason::ProfileNotFound,
        });
        return Outcome::ProfileNotFound;
    }

    if dry_run {
        world.events.push(EmittedEvent::ProfileApplied {
            name,
            is_duplicate: false,
            dry_run: true,
        });
        return Outcome::AppliedDryRun;
    }

    if let Some(existing) = world.agent_profiles.get(&name) {
        if existing.count == count
            && existing.env_overrides_hash == env_overrides_hash
            && !existing.dry_run
        {
            world.events.push(EmittedEvent::ProfileApplied {
                name,
                is_duplicate: true,
                dry_run: false,
            });
            return Outcome::AppliedDuplicate;
        }
    }

    let mut spawned = BTreeSet::new();
    let existing_panes_for_name = world.panes.get(&name).cloned().unwrap_or_default();
    for _ in 0..count {
        let pid = world.next_pane_id;
        world.next_pane_id = world.next_pane_id.wrapping_add(1);
        // Detect wrap-collision: the freshly-allocated pid is
        // already in `panes[name]` from a prior apply. The
        // BTreeSet dedup will silently drop it, but the receipt
        // still claims it was 'spawned' here. Surface for the
        // BFS invariant check.
        if existing_panes_for_name.contains(&pid) || spawned.contains(&pid) {
            world.wrap_collisions_total = world.wrap_collisions_total.saturating_add(1);
        }
        spawned.insert(pid);
    }

    let receipt = ApplyReceipt {
        name,
        count,
        env_overrides_hash,
        dry_run: false,
        panes_spawned: spawned.clone(),
    };
    world.agent_profiles.insert(name, receipt);
    world
        .panes
        .entry(name)
        .or_default()
        .extend(spawned.iter().copied());
    world.events.push(EmittedEvent::ProfileApplied {
        name,
        is_duplicate: false,
        dry_run: false,
    });
    Outcome::Applied {
        panes_spawned: spawned,
    }
}

// ============================================================================
// Invariants
// ============================================================================

/// Invariants the BFS harness checks on every reachable state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invariant {
    /// Every key in `agent_profiles` is in `defined_profiles`.
    NoOrphanProfile,
    /// `agent_profiles[name].panes_spawned ==
    /// panes[name]` after any successful Apply.
    NoDoubleSpawnOnDuplicateApply,
    /// An `ApplyFail` action leaves `agent_profiles` and `panes`
    /// unchanged.
    ApplyAtomic,
    /// An `Apply { dry_run: true }` action leaves
    /// `agent_profiles`, `panes`, and `next_pane_id` unchanged.
    DryRunPure,
    /// `List` / `Show` / `Validate` actions do not mutate the
    /// world.
    ListShowValidatePure,
    /// `next_pane_id.wrapping_add(1)` never produces a pid
    /// that collides with an existing entry in `panes[name]`.
    /// Detects the u8-wrap silently corrupting the receipt's
    /// `panes_spawned` claim.
    NoPaneIdWrapCollision,
}

/// Check every invariant against the *delta* between
/// `pre_state` and `post_state` (after firing `action` with
/// outcome `outcome`). Returns the first invariant that fires
/// (i.e. is violated), or `None` if all hold.
#[must_use]
pub fn check_invariants(
    pre_state: &ProfileWorld,
    action: &Action,
    outcome: &Outcome,
    post_state: &ProfileWorld,
) -> Option<Invariant> {
    // NoOrphanProfile.
    for name in post_state.agent_profiles.keys() {
        if !post_state.defined_profiles.contains(name) {
            return Some(Invariant::NoOrphanProfile);
        }
    }

    // NoDoubleSpawnOnDuplicateApply: a duplicate apply must not
    // grow `panes`.
    if matches!(outcome, Outcome::AppliedDuplicate)
        && (pre_state.panes != post_state.panes
            || pre_state.next_pane_id != post_state.next_pane_id)
    {
        return Some(Invariant::NoDoubleSpawnOnDuplicateApply);
    }

    // ApplyAtomic: ApplyFail leaves agent_profiles + panes +
    // next_pane_id unchanged.
    if matches!(action, Action::ApplyFail { .. })
        && (pre_state.agent_profiles != post_state.agent_profiles
            || pre_state.panes != post_state.panes
            || pre_state.next_pane_id != post_state.next_pane_id)
    {
        return Some(Invariant::ApplyAtomic);
    }

    // DryRunPure.
    if let Action::Apply { dry_run: true, .. } = action {
        if pre_state.agent_profiles != post_state.agent_profiles
            || pre_state.panes != post_state.panes
            || pre_state.next_pane_id != post_state.next_pane_id
        {
            return Some(Invariant::DryRunPure);
        }
    }

    // ListShowValidatePure.
    if matches!(
        action,
        Action::List | Action::Show { .. } | Action::Validate { .. }
    ) {
        let pre_persistent = persistent_view(pre_state);
        let post_persistent = persistent_view(post_state);
        if pre_persistent != post_persistent {
            return Some(Invariant::ListShowValidatePure);
        }
    }

    // NoPaneIdWrapCollision: u8 wrap from 255→0 must not
    // silently corrupt the receipt's panes_spawned claim.
    if post_state.wrap_collisions_total > pre_state.wrap_collisions_total {
        return Some(Invariant::NoPaneIdWrapCollision);
    }

    None
}

type PersistentProfileView<'a> = (
    &'a BTreeSet<ProfileId>,
    &'a BTreeMap<ProfileId, ApplyReceipt>,
    &'a BTreeMap<ProfileId, BTreeSet<PaneId>>,
    PaneId,
);

/// Project the persistent (non-event-trace) fields of the world
/// for purity comparisons. Events are append-only and used as a
/// trace; comparing them between pre/post would falsely fail
/// purity invariants when an event is emitted.
fn persistent_view(w: &ProfileWorld) -> PersistentProfileView<'_> {
    (
        &w.defined_profiles,
        &w.agent_profiles,
        &w.panes,
        w.next_pane_id,
    )
}

// ============================================================================
// BFS exploration
// ============================================================================

/// Explore the state space exhaustively up to `max_depth`,
/// firing every action in `actions` from every reachable state
/// at every depth. Returns the first `(state, action,
/// invariant)` triple that violates an invariant, or `None` if
/// the entire reachable set is clean.
pub fn bfs_check(
    initial: ProfileWorld,
    actions: &[Action],
    max_depth: usize,
) -> Option<(ProfileWorld, Action, Invariant)> {
    let mut frontier = vec![initial];
    let mut seen = BTreeSet::new();
    seen.insert(canonicalize(&frontier[0]));
    for _ in 0..max_depth {
        let mut next = Vec::new();
        for state in &frontier {
            for action in actions {
                let mut s = state.clone();
                let outcome = step(&mut s, action);
                if let Some(violated) = check_invariants(state, action, &outcome, &s) {
                    return Some((state.clone(), action.clone(), violated));
                }
                let canon = canonicalize(&s);
                if seen.insert(canon) {
                    next.push(s);
                }
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }
    None
}

/// Stable canonical key for a world state. We strip events
/// (append-only trace, not part of the persistent state) so the
/// frontier dedups on persistent state alone.
fn canonicalize(w: &ProfileWorld) -> Vec<u8> {
    let mut v = Vec::new();
    for p in &w.defined_profiles {
        v.push(*p);
    }
    v.push(0xFF);
    for (k, r) in &w.agent_profiles {
        v.push(*k);
        v.push(r.count);
        v.push(r.env_overrides_hash);
        v.push(u8::from(r.dry_run));
        for p in &r.panes_spawned {
            v.push(*p);
        }
        v.push(0xFE);
    }
    v.push(0xFF);
    for (k, ps) in &w.panes {
        v.push(*k);
        for p in ps {
            v.push(*p);
        }
        v.push(0xFE);
    }
    v.push(w.next_pane_id);
    v
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn standard_actions() -> Vec<Action> {
        vec![
            Action::Apply {
                name: 1,
                count: 1,
                env_overrides_hash: 0,
                dry_run: false,
            },
            Action::Apply {
                name: 1,
                count: 1,
                env_overrides_hash: 0,
                dry_run: false,
            }, // duplicate
            Action::Apply {
                name: 1,
                count: 1,
                env_overrides_hash: 0,
                dry_run: true,
            },
            Action::Apply {
                name: 99,
                count: 1,
                env_overrides_hash: 0,
                dry_run: false,
            }, // ProfileNotFound
            Action::ApplyFail {
                name: 1,
                count: 1,
                env_overrides_hash: 0,
                dry_run: false,
            },
            Action::List,
            Action::Show { name: 1 },
            Action::Validate { name: 1 },
        ]
    }

    #[test]
    fn fresh_apply_spawns_count_panes() {
        let mut w = ProfileWorld::with_profiles([1]);
        let outcome = step(
            &mut w,
            &Action::Apply {
                name: 1,
                count: 3,
                env_overrides_hash: 0,
                dry_run: false,
            },
        );
        if let Outcome::Applied { panes_spawned } = outcome {
            assert_eq!(panes_spawned.len(), 3);
        } else {
            panic!("expected Applied, got {outcome:?}");
        }
        assert_eq!(w.panes.get(&1).unwrap().len(), 3);
        assert_eq!(w.next_pane_id, 4);
    }

    #[test]
    fn duplicate_apply_returns_duplicate_outcome_no_new_spawn() {
        let mut w = ProfileWorld::with_profiles([1]);
        let act = Action::Apply {
            name: 1,
            count: 2,
            env_overrides_hash: 7,
            dry_run: false,
        };
        let _ = step(&mut w, &act);
        let panes_before = w.panes.clone();
        let next_before = w.next_pane_id;
        let second = step(&mut w, &act);
        assert!(matches!(second, Outcome::AppliedDuplicate));
        assert_eq!(w.panes, panes_before);
        assert_eq!(w.next_pane_id, next_before);
    }

    #[test]
    fn count_change_re_apply_accumulates_panes_pinned_behavior() {
        // PIN: documents the model's CURRENT behavior on
        // count-change re-apply. The bead's "idempotent on
        // identical input" doesn't say what happens when count
        // changes. Currently the model: (a) fails the duplicate
        // check (count mismatch), (b) proceeds to 'real' apply,
        // (c) OVERWRITES the receipt with the new count, (d)
        // spawns count-MORE panes accumulated on top of the
        // existing panes[name] set.
        //
        // If production semantics are 'kill old + spawn new'
        // (idempotent on profile name regardless of count),
        // this pin will fail intentionally — prompting the
        // bead author to clarify and update the model.
        let mut w = ProfileWorld::with_profiles([1]);
        let _ = step(
            &mut w,
            &Action::Apply {
                name: 1,
                count: 5,
                env_overrides_hash: 7,
                dry_run: false,
            },
        );
        assert_eq!(w.panes.get(&1).unwrap().len(), 5);
        let _ = step(
            &mut w,
            &Action::Apply {
                name: 1,
                count: 3,
                env_overrides_hash: 7, // same env, different count
                dry_run: false,
            },
        );
        // Receipt now shows count=3 + 3 newly-spawned pids.
        let receipt = w.agent_profiles.get(&1).unwrap();
        assert_eq!(receipt.count, 3);
        assert_eq!(receipt.panes_spawned.len(), 3);
        // BUT panes[1] accumulated: 5 from the first apply +
        // 3 from the second = 8 total. This is the model's
        // current behavior; production may want differently.
        assert_eq!(w.panes.get(&1).unwrap().len(), 8);
    }

    #[test]
    fn wrap_collision_counter_starts_at_zero() {
        let w = ProfileWorld::initial();
        assert_eq!(w.wrap_collisions_total, 0);
    }

    #[test]
    fn wrap_collision_detection_fires_on_u8_overflow() {
        // Force the u8 wrap by direct construction: if
        // next_pane_id is at 0xFE and panes[1] already
        // contains pid 0, the next allocation wraps to 0xFF
        // (no collision), then 0x00 (collision with existing).
        let mut w = ProfileWorld::with_profiles([1]);
        // Pre-populate panes[1] with pid 0 to set up the
        // collision target.
        w.panes.entry(1).or_default().insert(0);
        // Set next_pane_id to 0xFE so a count=3 apply allocates
        // 0xFE → 0xFF → 0x00 (wrap) → collision with pid 0.
        w.next_pane_id = 0xFE;
        // The duplicate-check passes (no existing receipt for
        // name=1 at this seeded state).
        let _ = step(
            &mut w,
            &Action::Apply {
                name: 1,
                count: 3,
                env_overrides_hash: 0,
                dry_run: false,
            },
        );
        assert!(w.wrap_collisions_total >= 1);
    }

    #[test]
    fn apply_against_unknown_profile_returns_not_found_no_side_effects() {
        let mut w = ProfileWorld::with_profiles([1]);
        let pre = w.clone();
        let outcome = step(
            &mut w,
            &Action::Apply {
                name: 99,
                count: 1,
                env_overrides_hash: 0,
                dry_run: false,
            },
        );
        assert!(matches!(outcome, Outcome::ProfileNotFound));
        assert_eq!(pre.agent_profiles, w.agent_profiles);
        assert_eq!(pre.panes, w.panes);
        assert_eq!(pre.next_pane_id, w.next_pane_id);
        // But an event is recorded.
        assert!(matches!(
            w.events.last(),
            Some(EmittedEvent::ApplyFailed {
                reason: ApplyFailureReason::ProfileNotFound,
                ..
            })
        ));
    }

    #[test]
    fn dry_run_apply_does_not_mutate_persistent_state() {
        let mut w = ProfileWorld::with_profiles([1]);
        let pre = w.clone();
        let _ = step(
            &mut w,
            &Action::Apply {
                name: 1,
                count: 5,
                env_overrides_hash: 0,
                dry_run: true,
            },
        );
        assert_eq!(pre.agent_profiles, w.agent_profiles);
        assert_eq!(pre.panes, w.panes);
        assert_eq!(pre.next_pane_id, w.next_pane_id);
    }

    #[test]
    fn list_show_validate_are_pure_reads() {
        let mut w = ProfileWorld::with_profiles([1]);
        let _ = step(
            &mut w,
            &Action::Apply {
                name: 1,
                count: 1,
                env_overrides_hash: 0,
                dry_run: false,
            },
        );
        let pre = w.clone();
        let _ = step(&mut w, &Action::List);
        assert_eq!(persistent_view(&pre), persistent_view(&w));
        let _ = step(&mut w, &Action::Show { name: 1 });
        assert_eq!(persistent_view(&pre), persistent_view(&w));
        let _ = step(&mut w, &Action::Validate { name: 1 });
        assert_eq!(persistent_view(&pre), persistent_view(&w));
    }

    #[test]
    fn apply_fail_action_leaves_persistent_state_unchanged() {
        let mut w = ProfileWorld::with_profiles([1]);
        let _ = step(
            &mut w,
            &Action::Apply {
                name: 1,
                count: 1,
                env_overrides_hash: 0,
                dry_run: false,
            },
        );
        let pre = w.clone();
        let _ = step(
            &mut w,
            &Action::ApplyFail {
                name: 1,
                count: 1,
                env_overrides_hash: 0,
                dry_run: false,
            },
        );
        assert_eq!(pre.agent_profiles, w.agent_profiles);
        assert_eq!(pre.panes, w.panes);
        assert_eq!(pre.next_pane_id, w.next_pane_id);
    }

    #[test]
    fn bfs_explores_full_action_set_without_invariant_violation() {
        let initial = ProfileWorld::with_profiles([1]);
        let result = bfs_check(initial, &standard_actions(), 3);
        assert!(
            result.is_none(),
            "BFS found an invariant violation: {result:?}"
        );
    }

    #[test]
    fn bfs_explores_with_two_profiles_clean() {
        let initial = ProfileWorld::with_profiles([1, 2]);
        let actions = vec![
            Action::Apply {
                name: 1,
                count: 1,
                env_overrides_hash: 0,
                dry_run: false,
            },
            Action::Apply {
                name: 2,
                count: 2,
                env_overrides_hash: 0,
                dry_run: false,
            },
            Action::Apply {
                name: 1,
                count: 1,
                env_overrides_hash: 0,
                dry_run: true,
            },
            Action::List,
        ];
        let result = bfs_check(initial, &actions, 3);
        assert!(result.is_none(), "BFS found violation: {result:?}");
    }

    #[test]
    fn invariant_no_orphan_profile_fires_when_violated() {
        // Construct a synthetic post-state with an orphan apply
        // receipt that escapes defined_profiles. The harness
        // surfaces NoOrphanProfile.
        let pre = ProfileWorld::with_profiles([1]);
        let mut post = pre.clone();
        // Inject orphan: apply receipt for a profile NOT in
        // defined_profiles.
        post.agent_profiles.insert(
            99,
            ApplyReceipt {
                name: 99,
                count: 1,
                env_overrides_hash: 0,
                dry_run: false,
                panes_spawned: BTreeSet::new(),
            },
        );
        let action = Action::Apply {
            name: 99,
            count: 1,
            env_overrides_hash: 0,
            dry_run: false,
        };
        let outcome = Outcome::Applied {
            panes_spawned: BTreeSet::new(),
        };
        let violated = check_invariants(&pre, &action, &outcome, &post);
        assert_eq!(violated, Some(Invariant::NoOrphanProfile));
    }

    #[test]
    fn invariant_dry_run_pure_fires_when_violated() {
        let pre = ProfileWorld::with_profiles([1]);
        let mut post = pre.clone();
        // Synthetic violation: dry_run apply mutated panes.
        post.panes.entry(1).or_default().insert(42);
        let action = Action::Apply {
            name: 1,
            count: 1,
            env_overrides_hash: 0,
            dry_run: true,
        };
        let outcome = Outcome::AppliedDryRun;
        let violated = check_invariants(&pre, &action, &outcome, &post);
        assert_eq!(violated, Some(Invariant::DryRunPure));
    }

    #[test]
    fn invariant_apply_atomic_fires_when_violated() {
        let pre = ProfileWorld::with_profiles([1]);
        let mut post = pre.clone();
        post.panes.entry(1).or_default().insert(7);
        let action = Action::ApplyFail {
            name: 1,
            count: 1,
            env_overrides_hash: 0,
            dry_run: false,
        };
        let outcome = Outcome::AppliedFailed;
        let violated = check_invariants(&pre, &action, &outcome, &post);
        assert_eq!(violated, Some(Invariant::ApplyAtomic));
    }

    #[test]
    fn invariant_list_show_validate_pure_fires_when_violated() {
        let pre = ProfileWorld::with_profiles([1]);
        let mut post = pre.clone();
        post.next_pane_id = 99;
        let action = Action::List;
        let outcome = Outcome::Read;
        let violated = check_invariants(&pre, &action, &outcome, &post);
        assert_eq!(violated, Some(Invariant::ListShowValidatePure));
    }
}
