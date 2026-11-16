//! Metamorphic tests for durable-state snapshot diffs.
//!
//! Oracle problem:
//! For arbitrary lifecycle registries, it is tedious to hand-compute the full
//! expected `StateDiff` change set. Instead we assert relations that must hold
//! under predictable input transformations.
//!
//! Metamorphic relations covered:
//! 1. Identical snapshots produce an empty diff.
//! 2. Reversing the direction of the diff swaps added/removed and flips changed
//!    from/to polarity.
//! 3. Adding the same disjoint padding entities to both snapshots does not
//!    change the diff between them.
//! 4. Registration order does not affect the diff.
//! 5. `diff_from_current(checkpoint, registry)` matches `diff(checkpoint, cp(registry))`
//!    modulo the special `to_checkpoint = 0` marker.

use frankenterm_core::durable_state::{CheckpointTrigger, DurableStateManager, StateDiff};
use frankenterm_core::session_topology::{
    LifecycleEntityKind, LifecycleIdentity, LifecycleRegistry, LifecycleState,
    MuxPaneLifecycleState,
};
use proptest::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CanonicalChange {
    key: String,
    from_state: Option<LifecycleState>,
    to_state: Option<LifecycleState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CanonicalDiff {
    added: Vec<CanonicalChange>,
    removed: Vec<CanonicalChange>,
    changed: Vec<CanonicalChange>,
}

fn pane_identity(id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Pane, "default", "local", id, 1)
}

fn pane_state(is_closed: bool) -> LifecycleState {
    if is_closed {
        LifecycleState::Pane(MuxPaneLifecycleState::Closed)
    } else {
        LifecycleState::Pane(MuxPaneLifecycleState::Running)
    }
}

fn make_registry(specs: &[(u64, LifecycleState)]) -> LifecycleRegistry {
    let mut registry = LifecycleRegistry::new();
    for &(id, state) in specs {
        registry
            .register_entity(pane_identity(id), state, 0)
            .expect("register pane");
    }
    registry
}

fn canonicalize(diff: &StateDiff) -> CanonicalDiff {
    fn canonicalize_changes(
        changes: &[frankenterm_core::durable_state::EntityChange],
    ) -> Vec<CanonicalChange> {
        let mut out: Vec<_> = changes
            .iter()
            .map(|change| CanonicalChange {
                key: change.identity.stable_key(),
                from_state: change.from_state,
                to_state: change.to_state,
            })
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }

    CanonicalDiff {
        added: canonicalize_changes(&diff.added),
        removed: canonicalize_changes(&diff.removed),
        changed: canonicalize_changes(&diff.changed),
    }
}

fn inverted(diff: &CanonicalDiff) -> CanonicalDiff {
    fn flip(changes: &[CanonicalChange]) -> Vec<CanonicalChange> {
        changes
            .iter()
            .map(|change| CanonicalChange {
                key: change.key.clone(),
                from_state: change.to_state,
                to_state: change.from_state,
            })
            .collect()
    }

    CanonicalDiff {
        added: flip(&diff.removed),
        removed: flip(&diff.added),
        changed: flip(&diff.changed),
    }
}

fn arb_registry_spec(
    id_start: u64,
    id_end: u64,
    max_len: usize,
) -> impl Strategy<Value = Vec<(u64, LifecycleState)>> {
    prop::collection::hash_map(id_start..id_end, any::<bool>(), 0..=max_len).prop_map(|map| {
        let mut specs: Vec<_> = map
            .into_iter()
            .map(|(id, is_closed)| (id, pane_state(is_closed)))
            .collect();
        specs.sort_by_key(|(id, _)| *id);
        specs
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn metamorphic_snapshot_diff_identical_snapshots_are_empty(
        specs in arb_registry_spec(1, 128, 12),
    ) {
        let registry = make_registry(&specs);
        let mut manager = DurableStateManager::new();
        let cp1 = manager
            .checkpoint(&registry, "same-a", CheckpointTrigger::Manual, std::collections::HashMap::new())
            .id;
        let cp2 = manager
            .checkpoint(&registry, "same-b", CheckpointTrigger::Periodic, std::collections::HashMap::new())
            .id;

        let diff = manager.diff(cp1, cp2).expect("diff");
        prop_assert!(diff.is_empty(), "identical registries should diff to empty, got {:?}", canonicalize(&diff));
    }

    #[test]
    fn metamorphic_snapshot_diff_inverse_swaps_added_removed_and_changed_polarity(
        a_specs in arb_registry_spec(1, 128, 12),
        b_specs in arb_registry_spec(1, 128, 12),
    ) {
        let reg_a = make_registry(&a_specs);
        let reg_b = make_registry(&b_specs);
        let mut manager = DurableStateManager::new();
        let cp_a = manager
            .checkpoint(&reg_a, "a", CheckpointTrigger::Manual, std::collections::HashMap::new())
            .id;
        let cp_b = manager
            .checkpoint(&reg_b, "b", CheckpointTrigger::Periodic, std::collections::HashMap::new())
            .id;

        let ab = canonicalize(&manager.diff(cp_a, cp_b).expect("diff a->b"));
        let ba = canonicalize(&manager.diff(cp_b, cp_a).expect("diff b->a"));

        prop_assert_eq!(ba, inverted(&ab));
    }

    #[test]
    fn metamorphic_snapshot_diff_shared_padding_does_not_change_change_set(
        a_specs in arb_registry_spec(1, 128, 10),
        b_specs in arb_registry_spec(1, 128, 10),
        padding in arb_registry_spec(10_000, 10_128, 6),
    ) {
        let reg_a = make_registry(&a_specs);
        let reg_b = make_registry(&b_specs);
        let mut manager_plain = DurableStateManager::new();
        let plain_a = manager_plain
            .checkpoint(&reg_a, "plain-a", CheckpointTrigger::Manual, std::collections::HashMap::new())
            .id;
        let plain_b = manager_plain
            .checkpoint(&reg_b, "plain-b", CheckpointTrigger::Manual, std::collections::HashMap::new())
            .id;
        let plain = canonicalize(&manager_plain.diff(plain_a, plain_b).expect("plain diff"));

        let mut padded_a_specs = a_specs.clone();
        padded_a_specs.extend(padding.clone());
        padded_a_specs.sort_by_key(|(id, _)| *id);
        let mut padded_b_specs = b_specs.clone();
        padded_b_specs.extend(padding);
        padded_b_specs.sort_by_key(|(id, _)| *id);

        let reg_padded_a = make_registry(&padded_a_specs);
        let reg_padded_b = make_registry(&padded_b_specs);
        let mut manager_padded = DurableStateManager::new();
        let padded_a = manager_padded
            .checkpoint(&reg_padded_a, "padded-a", CheckpointTrigger::Manual, std::collections::HashMap::new())
            .id;
        let padded_b = manager_padded
            .checkpoint(&reg_padded_b, "padded-b", CheckpointTrigger::Manual, std::collections::HashMap::new())
            .id;
        let padded = canonicalize(&manager_padded.diff(padded_a, padded_b).expect("padded diff"));

        prop_assert_eq!(plain, padded);
    }

    #[test]
    fn metamorphic_snapshot_diff_registration_order_does_not_matter(
        a_specs in arb_registry_spec(1, 128, 12),
        b_specs in arb_registry_spec(1, 128, 12),
    ) {
        let reg_a = make_registry(&a_specs);
        let reg_b = make_registry(&b_specs);

        let mut reversed_a = a_specs.clone();
        reversed_a.reverse();
        let mut reversed_b = b_specs.clone();
        reversed_b.reverse();
        let reg_a_reversed = make_registry(&reversed_a);
        let reg_b_reversed = make_registry(&reversed_b);

        let mut manager_forward = DurableStateManager::new();
        let forward_a = manager_forward
            .checkpoint(&reg_a, "forward-a", CheckpointTrigger::Manual, std::collections::HashMap::new())
            .id;
        let forward_b = manager_forward
            .checkpoint(&reg_b, "forward-b", CheckpointTrigger::Manual, std::collections::HashMap::new())
            .id;
        let forward = canonicalize(&manager_forward.diff(forward_a, forward_b).expect("forward diff"));

        let mut manager_reversed = DurableStateManager::new();
        let reversed_cp_a = manager_reversed
            .checkpoint(&reg_a_reversed, "reversed-a", CheckpointTrigger::Manual, std::collections::HashMap::new())
            .id;
        let reversed_cp_b = manager_reversed
            .checkpoint(&reg_b_reversed, "reversed-b", CheckpointTrigger::Manual, std::collections::HashMap::new())
            .id;
        let reversed = canonicalize(&manager_reversed.diff(reversed_cp_a, reversed_cp_b).expect("reversed diff"));

        prop_assert_eq!(forward, reversed);
    }

    #[test]
    fn metamorphic_snapshot_diff_from_current_matches_checkpointed_current_state(
        checkpoint_specs in arb_registry_spec(1, 128, 12),
        current_specs in arb_registry_spec(1, 128, 12),
    ) {
        let checkpoint_registry = make_registry(&checkpoint_specs);
        let current_registry = make_registry(&current_specs);
        let mut manager = DurableStateManager::new();
        let checkpoint_id = manager
            .checkpoint(&checkpoint_registry, "checkpoint", CheckpointTrigger::Manual, std::collections::HashMap::new())
            .id;

        let from_current = canonicalize(
            &manager
                .diff_from_current(checkpoint_id, &current_registry)
                .expect("diff_from_current"),
        );

        let current_checkpoint_id = manager
            .checkpoint(&current_registry, "current", CheckpointTrigger::Periodic, std::collections::HashMap::new())
            .id;
        let checkpointed = canonicalize(
            &manager
                .diff(checkpoint_id, current_checkpoint_id)
                .expect("diff checkpointed current"),
        );

        prop_assert_eq!(from_current, checkpointed);
    }
}
