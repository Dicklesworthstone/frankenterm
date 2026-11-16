use frankenterm_core::robot_work_state_machine::{
    ClaimId, ClaimState, WorkAction, WorkWorld, apply_action, check_invariants,
};
use stateright::{Model, Property};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RobotWorkState {
    pub world: WorkWorld,
    pub violation: bool,
}

#[derive(Debug, Clone)]
pub struct RobotWorkAtomicityModel {
    pub claim_count: ClaimId,
    pub agents: Vec<u8>,
    pub max_events: usize,
}

impl RobotWorkAtomicityModel {
    #[must_use]
    pub fn smoke() -> Self {
        Self {
            claim_count: 2,
            agents: vec![1, 2],
            max_events: 7,
        }
    }
}

impl Model for RobotWorkAtomicityModel {
    type State = RobotWorkState;
    type Action = WorkAction;

    fn init_states(&self) -> Vec<Self::State> {
        vec![RobotWorkState {
            world: WorkWorld::seeded(self.claim_count, &self.agents),
            violation: false,
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        actions.push(WorkAction::List);
        for claim in state.world.claims.keys().copied() {
            actions.push(WorkAction::Status { claim });
            for agent in &self.agents {
                actions.push(WorkAction::Claim {
                    claim,
                    agent: *agent,
                });
                actions.push(WorkAction::Complete {
                    claim,
                    agent: *agent,
                });
                actions.push(WorkAction::Release {
                    claim,
                    agent: *agent,
                });
                actions.push(WorkAction::ClaimFail {
                    claim,
                    agent: *agent,
                });
                actions.push(WorkAction::CompleteFail {
                    claim,
                    agent: *agent,
                });
                actions.push(WorkAction::ReleaseFail {
                    claim,
                    agent: *agent,
                });
            }
        }
        for agent in &self.agents {
            actions.push(WorkAction::CrashAndRestart { agent: *agent });
        }
    }

    fn next_state(&self, last_state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut world = last_state.world.clone();
        let outcome = apply_action(&mut world, action);
        let violations = check_invariants(&last_state.world, &world, action, outcome);
        let next = RobotWorkState {
            violation: last_state.violation || !violations.is_empty(),
            world,
        };
        if next == *last_state {
            None
        } else {
            Some(next)
        }
    }

    fn within_boundary(&self, state: &Self::State) -> bool {
        state.world.events.len() <= self.max_events
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::always("no_safety_violation", no_safety_violation),
            Property::always("completed_events_are_durable", completed_events_are_durable),
            Property::always("claimed_rows_have_one_owner", claimed_rows_have_one_owner),
            Property::sometimes("claim_is_reachable", claim_is_reachable),
            Property::sometimes("completion_is_reachable", completion_is_reachable),
            Property::sometimes(
                "crash_auto_release_is_reachable",
                crash_auto_release_is_reachable,
            ),
        ]
    }
}

fn no_safety_violation(_model: &RobotWorkAtomicityModel, state: &RobotWorkState) -> bool {
    !state.violation
}

fn completed_events_are_durable(_model: &RobotWorkAtomicityModel, state: &RobotWorkState) -> bool {
    for event in &state.world.events {
        if let frankenterm_core::robot_work_state_machine::EmittedEvent::Completed {
            claim,
            agent,
        } = event
        {
            if state.world.claims.get(claim) != Some(&ClaimState::Completed { owner: *agent }) {
                return false;
            }
        }
    }
    true
}

fn claimed_rows_have_one_owner(model: &RobotWorkAtomicityModel, state: &RobotWorkState) -> bool {
    state.world.claims.len() == usize::from(model.claim_count)
        && state.world.claims.iter().all(|(claim, row)| {
            *claim < model.claim_count
                && row_owner(row).is_none_or(|owner| model.agents.contains(&owner))
        })
}

fn row_owner(row: &ClaimState) -> Option<u8> {
    match row {
        ClaimState::Unclaimed => None,
        ClaimState::Claimed { owner } | ClaimState::Completed { owner } => Some(*owner),
    }
}

fn claim_is_reachable(_model: &RobotWorkAtomicityModel, state: &RobotWorkState) -> bool {
    state
        .world
        .claims
        .values()
        .any(|claim| matches!(claim, ClaimState::Claimed { .. }))
}

fn completion_is_reachable(_model: &RobotWorkAtomicityModel, state: &RobotWorkState) -> bool {
    state
        .world
        .claims
        .values()
        .any(|claim| matches!(claim, ClaimState::Completed { .. }))
}

fn crash_auto_release_is_reachable(
    _model: &RobotWorkAtomicityModel,
    state: &RobotWorkState,
) -> bool {
    state.world.events.iter().any(|event| {
        matches!(
            event,
            frankenterm_core::robot_work_state_machine::EmittedEvent::AutoReleasedOnCrash { .. }
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use stateright::{Checker, Model};

    #[test]
    fn stateright_bfs_verifies_robot_work_atomicity() {
        let checker = RobotWorkAtomicityModel::smoke()
            .checker()
            .threads(1)
            .target_max_depth(14)
            .spawn_bfs()
            .join();
        checker.assert_properties();
        assert!(checker.state_count() >= checker.unique_state_count());
        assert!(checker.unique_state_count() > 0);
    }
}
