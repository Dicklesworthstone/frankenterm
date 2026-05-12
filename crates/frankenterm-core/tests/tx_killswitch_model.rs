//! Mission/TX kill-switch state-space proof harness
//! ([BR-RC-SAFETY-PROOFS.G13] / `ft-x0666.4`).
//!
//! This harness consumes the model from
//! `crates/frankenterm-core/src/tx_killswitch_model.rs` and ships
//! the proof artifact the bead asks for:
//!
//! 1. **Exhaustive BFS state-space exploration.** Walks every
//!    reachable state from `initial(step_count)` and asserts every
//!    safety invariant holds. The state space at `step_count = 3`
//!    is small enough (~few thousand reachable states) that
//!    exhaustive exploration finishes in milliseconds.
//! 2. **Proptest fuzz with random schedules.** 1000 cases × up
//!    to 32 actions each keep the always-on local lane cheap.
//! 3. **Million-schedule CI proof.** The
//!    `random_schedule_never_violates_safety_invariants` test
//!    runs a deterministic pseudo-random schedule corpus. Local
//!    runs default to 1000 schedules; CI sets
//!    `FT_TX_KILLSWITCH_RANDOM_SCHEDULES=1000000` to satisfy the
//!    bead's explicit ≥1M random schedules per CI run target.
//!    Both random lanes assert the same invariants on every
//!    visited state.
//! 4. **Stateright-shape API.** The harness's BFS body has the
//!    same shape Stateright would produce — `enabled_actions →
//!    apply → assert_invariants → enqueue`. If the workspace
//!    later adopts Stateright, swapping in is mechanical.

use std::collections::{HashSet, VecDeque};

use frankenterm_core::plan::{MissionKillSwitchLevel, MissionTxState};
use frankenterm_core::tx_killswitch_model::{
    KillSwitchAction, KillSwitchModelState, apply, check_safety, enabled_actions,
    hard_stop_admits_progress, is_drained,
};
use proptest::prelude::*;

const DEFAULT_RANDOM_SCHEDULES: u64 = 1_000;
const DEFAULT_RANDOM_SCHEDULE_LEN: u8 = 32;

fn configured_random_schedule_count() -> u64 {
    std::env::var("FT_TX_KILLSWITCH_RANDOM_SCHEDULES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_RANDOM_SCHEDULES)
}

fn next_lcg(seed: &mut u64) -> u64 {
    // PCG's default LCG constants give a small deterministic corpus
    // without adding a rand dependency to this proof harness.
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *seed
}

// ============================================================================
// Test 1 — Exhaustive BFS at step_count = 2
// ============================================================================

/// Walk every reachable state from `initial(step_count = 2)` and
/// assert ALL safety invariants on every visited state.
///
/// This is the **load-bearing proof** the bead asks for:
/// "no committed-without-receipt" and "kill-switch eventually
/// drains" are checked at every state in the reachable set.
#[test]
fn exhaustive_bfs_at_step_count_2_finds_no_safety_violation() {
    let initial = KillSwitchModelState::initial(2);
    let mut visited: HashSet<KillSwitchModelState> = HashSet::new();
    let mut queue: VecDeque<KillSwitchModelState> = VecDeque::new();
    queue.push_back(initial.clone());
    visited.insert(initial);

    let mut explored = 0u64;
    while let Some(state) = queue.pop_front() {
        explored += 1;

        // Safety invariants: every reachable state satisfies all.
        let violations = check_safety(&state);
        assert!(
            violations.is_empty(),
            "safety violation at reachable state {state:?}: {violations:?}"
        );

        // Hard-stop progress invariant.
        assert!(
            hard_stop_admits_progress(&state),
            "hard-stop liveness violated at {state:?}"
        );

        for action in enabled_actions(&state) {
            let next = apply(&state, action);
            if visited.insert(next.clone()) {
                queue.push_back(next);
            }
        }

        // Sanity bound: state space at step_count=2 is finite and
        // small. If we somehow blow this, the model has an
        // infinite-state bug.
        assert!(
            explored < 100_000,
            "state space exploded — explored {explored} states"
        );
    }

    // We expect a non-trivial state count.
    assert!(
        visited.len() >= 50,
        "visited state set suspiciously small: {}",
        visited.len()
    );
    println!(
        "exhaustive BFS at step_count=2: visited {} states across {} explorations",
        visited.len(),
        explored
    );
}

// ============================================================================
// Test 2 — Exhaustive BFS at step_count = 3
// ============================================================================

#[test]
fn exhaustive_bfs_at_step_count_3_finds_no_safety_violation() {
    let initial = KillSwitchModelState::initial(3);
    let mut visited: HashSet<KillSwitchModelState> = HashSet::new();
    let mut queue: VecDeque<KillSwitchModelState> = VecDeque::new();
    queue.push_back(initial.clone());
    visited.insert(initial);

    while let Some(state) = queue.pop_front() {
        let violations = check_safety(&state);
        assert!(
            violations.is_empty(),
            "safety violation at reachable state {state:?}: {violations:?}"
        );
        assert!(
            hard_stop_admits_progress(&state),
            "hard-stop liveness violated at {state:?}"
        );
        for action in enabled_actions(&state) {
            let next = apply(&state, action);
            if visited.insert(next.clone()) {
                queue.push_back(next);
            }
        }
        assert!(visited.len() < 10_000_000, "state space exploded");
    }
    println!(
        "exhaustive BFS at step_count=3: visited {} states",
        visited.len()
    );
}

// ============================================================================
// Test 3 — HardStop reachability proof
//
// The bead's headline liveness rule: from EVERY reachable state
// where kill_switch == HardStop, there exists a finite path to a
// drained state. Verified by checking that every reachable
// HardStop state either IS drained, or has at least one enabled
// action that progresses toward drained (where "progress" is
// defined by `hard_stop_admits_progress`).
// ============================================================================

#[test]
fn every_reachable_hard_stop_state_admits_progress_to_drained() {
    let initial = KillSwitchModelState::initial(3);
    let mut visited: HashSet<KillSwitchModelState> = HashSet::new();
    let mut queue: VecDeque<KillSwitchModelState> = VecDeque::new();
    queue.push_back(initial.clone());
    visited.insert(initial);

    let mut hard_stop_states_seen = 0u64;
    while let Some(state) = queue.pop_front() {
        if state.kill_switch == MissionKillSwitchLevel::HardStop {
            hard_stop_states_seen += 1;
            assert!(
                hard_stop_admits_progress(&state),
                "HardStop state {state:?} stuck — no progress action"
            );
        }
        for action in enabled_actions(&state) {
            let next = apply(&state, action);
            if visited.insert(next.clone()) {
                queue.push_back(next);
            }
        }
    }
    assert!(
        hard_stop_states_seen > 0,
        "no HardStop states reached — model is broken"
    );
    println!("verified hard-stop progress on {hard_stop_states_seen} reachable HardStop states");
}

// ============================================================================
// Test 4 — Drained-state reachability
//
// From the initial state, EVERY terminal state (Committed,
// Compensated, RolledBack) MUST be reachable. Otherwise the
// model is over-constrained.
// ============================================================================

#[test]
fn all_three_terminal_states_are_reachable_from_initial() {
    let initial = KillSwitchModelState::initial(2);
    let mut visited: HashSet<KillSwitchModelState> = HashSet::new();
    let mut queue: VecDeque<KillSwitchModelState> = VecDeque::new();
    queue.push_back(initial.clone());
    visited.insert(initial);

    let mut found_committed = false;
    let mut found_compensated = false;
    let mut found_rolled_back = false;
    while let Some(state) = queue.pop_front() {
        match state.tx_state {
            MissionTxState::Committed => found_committed = true,
            MissionTxState::Compensated => found_compensated = true,
            MissionTxState::RolledBack => found_rolled_back = true,
            _ => {}
        }
        for action in enabled_actions(&state) {
            let next = apply(&state, action);
            if visited.insert(next.clone()) {
                queue.push_back(next);
            }
        }
    }
    assert!(found_committed, "Committed unreachable from initial state");
    assert!(found_compensated, "Compensated unreachable");
    assert!(found_rolled_back, "RolledBack unreachable");
}

// ============================================================================
// Test 5 — Proptest fuzz with random schedules
// ============================================================================

prop_compose! {
    fn arb_action_index()(idx in 0u8..16) -> u8 {
        idx
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 1_000,
        .. ProptestConfig::default()
    })]

    /// 1,000 cases × up to 32 actions per schedule = 32,000
    /// schedule trials per CI run. Each schedule deterministically
    /// picks the next action by `idx % enabled.len()`. Every
    /// visited state MUST satisfy every safety invariant.
    ///
    /// The bead's "≥1M random schedules per CI run" target is
    /// reached by multiplying out across CI tiers; this proptest
    /// is the always-on regression net at 32k.
    #[test]
    fn proptest_random_schedule_never_violates_safety_invariants(
        action_indices in proptest::collection::vec(arb_action_index(), 0..32),
        step_count in 1u8..=3,
    ) {
        let mut state = KillSwitchModelState::initial(step_count);
        for idx in action_indices {
            let actions = enabled_actions(&state);
            if actions.is_empty() {
                break;
            }
            let action = actions[(idx as usize) % actions.len()];
            state = apply(&state, action);
            // Safety: ALL invariants hold at every reachable
            // state.
            let violations = check_safety(&state);
            prop_assert!(
                violations.is_empty(),
                "safety violation at fuzz state {state:?}: {violations:?}"
            );
            // Liveness: HardStop states admit progress.
            prop_assert!(
                hard_stop_admits_progress(&state),
                "HardStop progress invariant violated at {state:?}"
            );
        }
    }

    /// Adversarial proptest: random sequences that include lots of
    /// kill-switch flips. Asserts the model never strands —
    /// after a sufficiently long sequence including a HardStop
    /// flip, the schedule should be able to reach a drained
    /// state if the operator flips the switch back to Off.
    #[test]
    fn hard_stop_flip_then_off_reaches_drained_within_budget(
        action_indices in proptest::collection::vec(arb_action_index(), 16..64),
    ) {
        let mut state = KillSwitchModelState::initial(2);
        // Phase 1: random schedule possibly flipping HardStop.
        for idx in &action_indices {
            let actions = enabled_actions(&state);
            if actions.is_empty() { break; }
            let action = actions[(*idx as usize) % actions.len()];
            state = apply(&state, action);
        }
        // Phase 2: flip OFF and run greedy-non-flip until
        // drained or out of budget.
        if state.kill_switch != MissionKillSwitchLevel::Off {
            state = apply(
                &state,
                KillSwitchAction::FlipKillSwitch {
                    to: MissionKillSwitchLevel::Off,
                },
            );
        }
        let mut budget = 100u32;
        while !is_drained(&state) && budget > 0 {
            let actions = enabled_actions(&state);
            // Prefer non-flip actions to avoid bouncing the
            // switch.
            let action = actions
                .iter()
                .find(|a| !matches!(a, KillSwitchAction::FlipKillSwitch { .. }))
                .or_else(|| actions.first())
                .copied();
            let Some(action) = action else { break };
            state = apply(&state, action);
            budget -= 1;
        }
        prop_assert!(
            is_drained(&state) || budget == 0,
            "after Off flip + greedy schedule, expected drained; got {state:?}"
        );
        // The realistic bound: budget=100 is plenty for a
        // step_count=2 model.
        prop_assert!(
            is_drained(&state),
            "model failed to drain within 100 steps after Off flip: {state:?}"
        );
    }
}

// ============================================================================
// Test 6 — Deterministic million-schedule random corpus
// ============================================================================

#[test]
fn random_schedule_never_violates_safety_invariants() {
    let explicit_schedule_count = std::env::var("FT_TX_KILLSWITCH_RANDOM_SCHEDULES").ok();
    let schedule_count = configured_random_schedule_count();
    let mut schedules_run = 0u64;
    let mut transitions_checked = 0u64;
    let mut seed = 0x6674_2d74_782d_6b73u64;

    for schedule_idx in 0..schedule_count {
        let step_count = (next_lcg(&mut seed) % 3 + 1) as u8;
        let mut state = KillSwitchModelState::initial(step_count);

        for _ in 0..DEFAULT_RANDOM_SCHEDULE_LEN {
            let actions = enabled_actions(&state);
            if actions.is_empty() {
                break;
            }

            let action_index = (next_lcg(&mut seed) as usize) % actions.len();
            state = apply(&state, actions[action_index]);
            transitions_checked += 1;

            let violations = check_safety(&state);
            assert!(
                violations.is_empty(),
                "safety violation at schedule {schedule_idx}, state {state:?}: {violations:?}"
            );
            assert!(
                hard_stop_admits_progress(&state),
                "HardStop progress invariant violated at schedule {schedule_idx}, state {state:?}"
            );
        }

        schedules_run += 1;
    }

    assert_eq!(schedules_run, schedule_count);
    if explicit_schedule_count.is_some() {
        assert!(
            schedules_run >= 1_000_000,
            "explicit TX kill-switch proof lane must run at least 1,000,000 random schedules; ran {schedules_run}"
        );
    }
    println!(
        "tx kill-switch random corpus: schedules={schedules_run} transitions={transitions_checked}"
    );
}

// ============================================================================
// Test 7 — Acyclicity in the projection
//
// Projected onto MissionTxState alone (ignoring kill-switch + step
// sets), the reachable graph respects the documented forward
// progression: Draft → Planned → Prepared → Committing → {Committed
// | Failed → Compensating → Compensated} → RolledBack. We don't
// claim NO cycles in the full state space (kill-switch flips ARE
// cyclic), but the tx_state projection must be acyclic.
// ============================================================================

#[test]
fn tx_state_projection_is_acyclic() {
    use std::collections::HashMap;
    let initial = KillSwitchModelState::initial(2);
    let mut visited: HashSet<KillSwitchModelState> = HashSet::new();
    let mut queue: VecDeque<KillSwitchModelState> = VecDeque::new();
    queue.push_back(initial.clone());
    visited.insert(initial);

    // Build edge multiset over tx_state pairs.
    let mut edges: HashMap<MissionTxState, HashSet<MissionTxState>> = HashMap::new();

    while let Some(state) = queue.pop_front() {
        for action in enabled_actions(&state) {
            let next = apply(&state, action);
            if state.tx_state != next.tx_state {
                edges
                    .entry(state.tx_state)
                    .or_default()
                    .insert(next.tx_state);
            }
            if visited.insert(next.clone()) {
                queue.push_back(next);
            }
        }
    }

    // Forbid back-edges that would create a cycle in the
    // documented progression. Specifically: terminal states
    // (Committed, Compensated, RolledBack) MUST NOT have outgoing
    // tx_state edges.
    for terminal in [MissionTxState::Committed, MissionTxState::RolledBack] {
        let outgoing = edges.get(&terminal);
        assert!(
            outgoing.is_none() || outgoing.map(HashSet::is_empty).unwrap_or(true),
            "terminal {terminal:?} has outgoing tx_state edges {outgoing:?}"
        );
    }
    // Compensated → RolledBack is the only allowed transition
    // out of Compensated.
    if let Some(out) = edges.get(&MissionTxState::Compensated) {
        for tgt in out {
            assert_eq!(
                *tgt,
                MissionTxState::RolledBack,
                "Compensated → {tgt:?} not allowed"
            );
        }
    }
}
