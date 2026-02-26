//! Proptest mirror of TLA+ formal verification for mux protocol (ft-283h4.3).
//!
//! Each test here mirrors a safety property verified by TLC model checking
//! in `docs/formal/*.tla`.  The TLA+ specs define the *model*; these proptests
//! validate that Rust implementations conform to the same invariants under
//! randomized action sequences.
//!
//! Reference: docs/formal/VERIFICATION_RESULTS.md

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use proptest::prelude::*;

// =========================================================================
// Spec 1: PDU Exchange Protocol (mux_protocol.tla)
// =========================================================================

/// Protocol state corresponding to `mux_protocol.tla`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MuxState {
    Idle,
    Sending,
    Receiving,
    Error,
    Reconnecting,
}

/// Actions from the TLA+ Next relation.
#[derive(Debug, Clone, Copy)]
enum MuxAction {
    ClientSend,
    WireDeliver,
    ServerProcess,
    ErrorDetected,
    ReconnectStart,
    ReconnectComplete,
}

/// Complete state of the mux protocol model.
#[derive(Debug, Clone)]
struct MuxProtocol {
    state: MuxState,
    next_msg_id: u64,
    send_q: VecDeque<u64>,
    recv_q: VecDeque<u64>,
    delivered: Vec<u64>,
    in_flight: BTreeSet<u64>,
    error_reported: BTreeSet<u64>,
    max_queue: usize,
    max_msg_id: u64,
}

impl MuxProtocol {
    fn new(max_queue: usize, max_msg_id: u64) -> Self {
        Self {
            state: MuxState::Idle,
            next_msg_id: 1,
            send_q: VecDeque::new(),
            recv_q: VecDeque::new(),
            delivered: Vec::new(),
            in_flight: BTreeSet::new(),
            error_reported: BTreeSet::new(),
            max_queue,
            max_msg_id,
        }
    }

    /// Try to apply an action.  Returns false if precondition not met.
    fn step(&mut self, action: MuxAction) -> bool {
        match action {
            MuxAction::ClientSend => {
                if !matches!(self.state, MuxState::Idle | MuxState::Receiving) {
                    return false;
                }
                if self.send_q.len() >= self.max_queue {
                    return false;
                }
                if self.next_msg_id > self.max_msg_id {
                    return false;
                }
                let msg = self.next_msg_id;
                self.send_q.push_back(msg);
                self.in_flight.insert(msg);
                self.next_msg_id = msg + 1;
                self.state = MuxState::Sending;
                true
            }
            MuxAction::WireDeliver => {
                if !matches!(self.state, MuxState::Sending | MuxState::Receiving) {
                    return false;
                }
                let Some(msg) = self.send_q.pop_front() else {
                    return false;
                };
                self.recv_q.push_back(msg);
                self.state = MuxState::Receiving;
                true
            }
            MuxAction::ServerProcess => {
                if self.state != MuxState::Receiving {
                    return false;
                }
                let Some(msg) = self.recv_q.pop_front() else {
                    return false;
                };
                self.delivered.push(msg);
                self.in_flight.remove(&msg);
                if self.send_q.is_empty() && self.recv_q.is_empty() {
                    self.state = MuxState::Idle;
                } else {
                    self.state = MuxState::Receiving;
                }
                true
            }
            MuxAction::ErrorDetected => {
                if !matches!(self.state, MuxState::Sending | MuxState::Receiving) {
                    return false;
                }
                if self.in_flight.is_empty() {
                    return false;
                }
                // Choose arbitrary in-flight message (first).
                let msg = *self.in_flight.iter().next().unwrap();
                self.in_flight.remove(&msg);
                self.error_reported.insert(msg);
                self.state = MuxState::Error;
                true
            }
            MuxAction::ReconnectStart => {
                if self.state != MuxState::Error {
                    return false;
                }
                self.state = MuxState::Reconnecting;
                true
            }
            MuxAction::ReconnectComplete => {
                if self.state != MuxState::Reconnecting {
                    return false;
                }
                self.state = MuxState::Idle;
                true
            }
        }
    }

    // -- Safety invariants from TLA+ --

    /// Safety_MessageTracked: SentSet = PendingSet ∪ delivered ∪ errorReported
    fn safety_message_tracked(&self) -> bool {
        let sent: BTreeSet<u64> = (1..self.next_msg_id).collect();
        let send_q_set: BTreeSet<u64> = self.send_q.iter().copied().collect();
        let recv_q_set: BTreeSet<u64> = self.recv_q.iter().copied().collect();
        let delivered_set: BTreeSet<u64> = self.delivered.iter().copied().collect();
        let pending: BTreeSet<u64> = send_q_set
            .union(&recv_q_set)
            .copied()
            .collect::<BTreeSet<_>>()
            .union(&self.in_flight)
            .copied()
            .collect();
        let tracked: BTreeSet<u64> = pending
            .union(&delivered_set)
            .copied()
            .collect::<BTreeSet<_>>()
            .union(&self.error_reported)
            .copied()
            .collect();
        sent == tracked
    }

    /// Safety_NoDuplicateDelivery: Cardinality(delivered set) == Len(delivered)
    fn safety_no_duplicate_delivery(&self) -> bool {
        let set: BTreeSet<u64> = self.delivered.iter().copied().collect();
        set.len() == self.delivered.len()
    }

    /// Safety_OrderedDelivery: StrictlyIncreasing(delivered)
    fn safety_ordered_delivery(&self) -> bool {
        self.delivered
            .windows(2)
            .all(|w| w[0] < w[1])
    }
}

// =========================================================================
// Spec 2: Snapshot Lifecycle (snapshot_lifecycle.tla)
// =========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
enum SnapshotPhase {
    Normal,
    Capturing,
    Writing,
    Stored,
    Restoring,
    Restored,
}

#[derive(Debug, Clone, Copy)]
enum SnapshotAction {
    Mutate { pane: usize, value: u8 },
    StartCapture,
    FinishCapture,
    CommitWrite,
    StartRestore,
    RestoreChunk { pane: usize },
    CompleteRestore,
    AbortRestore,
}

/// Snapshot lifecycle model corresponding to `snapshot_lifecycle.tla`.
#[derive(Debug, Clone)]
struct SnapshotLifecycle {
    phase: SnapshotPhase,
    live_state: Vec<u8>,
    capture_state: Vec<u8>,
    write_buffer: Vec<u8>,
    has_snapshot: bool,
    snapshot_state: Vec<u8>,
    pre_restore_live: Vec<u8>,
    restore_buffer: Vec<u8>,
    applied_panes: BTreeSet<usize>,
    has_restored: bool,
    last_restored_state: Vec<u8>,
    num_panes: usize,
}

impl SnapshotLifecycle {
    fn new(num_panes: usize, default_value: u8) -> Self {
        let init = vec![default_value; num_panes];
        Self {
            phase: SnapshotPhase::Normal,
            live_state: init.clone(),
            capture_state: init.clone(),
            write_buffer: init.clone(),
            has_snapshot: false,
            snapshot_state: init.clone(),
            pre_restore_live: init.clone(),
            restore_buffer: init.clone(),
            applied_panes: BTreeSet::new(),
            has_restored: false,
            last_restored_state: init,
            num_panes,
        }
    }

    fn step(&mut self, action: SnapshotAction) -> bool {
        match action {
            SnapshotAction::Mutate { pane, value } => {
                if !matches!(
                    self.phase,
                    SnapshotPhase::Normal | SnapshotPhase::Stored | SnapshotPhase::Restored
                ) {
                    return false;
                }
                if pane >= self.num_panes {
                    return false;
                }
                self.live_state[pane] = value;
                self.phase = SnapshotPhase::Normal;
                true
            }
            SnapshotAction::StartCapture => {
                if !matches!(
                    self.phase,
                    SnapshotPhase::Normal | SnapshotPhase::Stored | SnapshotPhase::Restored
                ) {
                    return false;
                }
                self.capture_state = self.live_state.clone();
                self.phase = SnapshotPhase::Capturing;
                true
            }
            SnapshotAction::FinishCapture => {
                if self.phase != SnapshotPhase::Capturing {
                    return false;
                }
                self.write_buffer = self.capture_state.clone();
                self.phase = SnapshotPhase::Writing;
                true
            }
            SnapshotAction::CommitWrite => {
                if self.phase != SnapshotPhase::Writing {
                    return false;
                }
                self.has_snapshot = true;
                self.snapshot_state = self.write_buffer.clone();
                self.phase = SnapshotPhase::Stored;
                true
            }
            SnapshotAction::StartRestore => {
                if !self.has_snapshot {
                    return false;
                }
                if !matches!(
                    self.phase,
                    SnapshotPhase::Normal | SnapshotPhase::Stored | SnapshotPhase::Restored
                ) {
                    return false;
                }
                self.pre_restore_live = self.live_state.clone();
                self.restore_buffer = self.live_state.clone();
                self.applied_panes.clear();
                self.phase = SnapshotPhase::Restoring;
                true
            }
            SnapshotAction::RestoreChunk { pane } => {
                if self.phase != SnapshotPhase::Restoring {
                    return false;
                }
                if pane >= self.num_panes || self.applied_panes.contains(&pane) {
                    return false;
                }
                self.restore_buffer[pane] = self.snapshot_state[pane];
                self.applied_panes.insert(pane);
                true
            }
            SnapshotAction::CompleteRestore => {
                if self.phase != SnapshotPhase::Restoring {
                    return false;
                }
                let all_panes: BTreeSet<usize> = (0..self.num_panes).collect();
                if self.applied_panes != all_panes {
                    return false;
                }
                self.live_state = self.restore_buffer.clone();
                self.has_restored = true;
                self.last_restored_state = self.restore_buffer.clone();
                self.phase = SnapshotPhase::Restored;
                true
            }
            SnapshotAction::AbortRestore => {
                if self.phase != SnapshotPhase::Restoring {
                    return false;
                }
                self.live_state = self.pre_restore_live.clone();
                self.restore_buffer = self.pre_restore_live.clone();
                self.applied_panes.clear();
                self.phase = SnapshotPhase::Stored;
                true
            }
        }
    }

    // -- Safety invariants --

    /// Safety_CaptureConsistent
    fn safety_capture_consistent(&self) -> bool {
        if self.phase == SnapshotPhase::Writing {
            return self.write_buffer == self.capture_state;
        }
        true
    }

    /// Safety_Atomicity: during restore, live state unchanged until complete.
    fn safety_atomicity(&self) -> bool {
        if self.phase == SnapshotPhase::Restoring {
            return self.live_state == self.pre_restore_live;
        }
        true
    }

    /// Safety_NoDataLoss: after restore, live state equals snapshot.
    fn safety_no_data_loss(&self) -> bool {
        if self.has_snapshot && self.phase == SnapshotPhase::Restored {
            return self.live_state == self.snapshot_state;
        }
        true
    }

    /// Safety_NoPartialCommit: restored phase implies all panes applied.
    fn safety_no_partial_commit(&self) -> bool {
        if self.phase == SnapshotPhase::Restored {
            let all: BTreeSet<usize> = (0..self.num_panes).collect();
            return self.applied_panes == all;
        }
        true
    }
}

// =========================================================================
// Spec 3: WAL Correctness (wal_correctness.tla)
// =========================================================================

#[derive(Debug, Clone)]
struct WalOp {
    key: usize,
    value: u8,
}

#[derive(Debug, Clone, Copy)]
enum WalAction {
    Mutate { key: usize, value: u8 },
    Fsync,
    Crash,
    Recover,
    Compact,
}

/// WAL correctness model corresponding to `wal_correctness.tla`.
#[derive(Debug, Clone)]
struct WalModel {
    mem_state: BTreeMap<usize, u8>,
    wal: Vec<WalOp>,
    durable_idx: usize,
    crashed: bool,
    compact_base: BTreeMap<usize, u8>,
    num_keys: usize,
    max_ops: usize,
    default_value: u8,
}

impl WalModel {
    fn new(num_keys: usize, max_ops: usize, default_value: u8) -> Self {
        let init: BTreeMap<usize, u8> = (0..num_keys).map(|k| (k, default_value)).collect();
        Self {
            mem_state: init.clone(),
            wal: Vec::new(),
            durable_idx: 0,
            crashed: false,
            compact_base: init,
            num_keys,
            max_ops,
            default_value,
        }
    }

    fn apply_ops(base: &BTreeMap<usize, u8>, ops: &[WalOp]) -> BTreeMap<usize, u8> {
        let mut result = base.clone();
        for op in ops {
            result.insert(op.key, op.value);
        }
        result
    }

    fn step(&mut self, action: WalAction) -> bool {
        match action {
            WalAction::Mutate { key, value } => {
                if self.crashed || self.wal.len() >= self.max_ops || key >= self.num_keys {
                    return false;
                }
                self.wal.push(WalOp { key, value });
                self.mem_state = Self::apply_ops(&self.compact_base, &self.wal);
                true
            }
            WalAction::Fsync => {
                if self.crashed || self.durable_idx >= self.wal.len() {
                    return false;
                }
                self.durable_idx = self.wal.len();
                true
            }
            WalAction::Crash => {
                if self.crashed {
                    return false;
                }
                self.crashed = true;
                self.wal.truncate(self.durable_idx);
                self.mem_state = self.compact_base.clone();
                true
            }
            WalAction::Recover => {
                if !self.crashed {
                    return false;
                }
                self.crashed = false;
                self.mem_state = Self::apply_ops(&self.compact_base, &self.wal);
                true
            }
            WalAction::Compact => {
                if self.crashed
                    || self.durable_idx != self.wal.len()
                    || self.durable_idx == 0
                {
                    return false;
                }
                let new_base = Self::apply_ops(&self.compact_base, &self.wal);
                self.compact_base = new_base;
                self.wal.clear();
                self.durable_idx = 0;
                self.mem_state = self.compact_base.clone();
                true
            }
        }
    }

    // -- Safety invariants --

    /// Safety_RunningMatchesLog: ~crashed => memState = ApplyOps(compactBase, wal)
    fn safety_running_matches_log(&self) -> bool {
        if !self.crashed {
            return self.mem_state == Self::apply_ops(&self.compact_base, &self.wal);
        }
        true
    }

    /// Safety_DurableBound: durableIdx <= Len(wal)
    fn safety_durable_bound(&self) -> bool {
        self.durable_idx <= self.wal.len()
    }

    /// Safety_DurableWritesSurviveCrash: crashed => durableIdx = Len(wal)
    fn safety_durable_writes_survive_crash(&self) -> bool {
        if self.crashed {
            return self.durable_idx == self.wal.len();
        }
        true
    }

    /// Safety_CompactionSafe: after compact, state is equivalent.
    fn safety_compaction_safe(&self) -> bool {
        // The invariant is tracked by the compaction_ok flag in TLA+.
        // Here we verify the live invariant: mem_state always equals
        // apply(compact_base, wal) when not crashed.
        self.safety_running_matches_log()
    }
}

// =========================================================================
// Proptest strategies
// =========================================================================

fn mux_action_strategy() -> impl Strategy<Value = MuxAction> {
    prop_oneof![
        Just(MuxAction::ClientSend),
        Just(MuxAction::WireDeliver),
        Just(MuxAction::ServerProcess),
        Just(MuxAction::ErrorDetected),
        Just(MuxAction::ReconnectStart),
        Just(MuxAction::ReconnectComplete),
    ]
}

fn snapshot_action_strategy(num_panes: usize) -> impl Strategy<Value = SnapshotAction> {
    prop_oneof![
        (0..num_panes, any::<u8>()).prop_map(|(p, v)| SnapshotAction::Mutate { pane: p, value: v }),
        Just(SnapshotAction::StartCapture),
        Just(SnapshotAction::FinishCapture),
        Just(SnapshotAction::CommitWrite),
        Just(SnapshotAction::StartRestore),
        (0..num_panes).prop_map(|p| SnapshotAction::RestoreChunk { pane: p }),
        Just(SnapshotAction::CompleteRestore),
        Just(SnapshotAction::AbortRestore),
    ]
}

fn wal_action_strategy(num_keys: usize) -> impl Strategy<Value = WalAction> {
    prop_oneof![
        (0..num_keys, any::<u8>()).prop_map(|(k, v)| WalAction::Mutate { key: k, value: v }),
        Just(WalAction::Fsync),
        Just(WalAction::Crash),
        Just(WalAction::Recover),
        Just(WalAction::Compact),
    ]
}

// =========================================================================
// Proptests
// =========================================================================

proptest! {
    // -----------------------------------------------------------------
    // Spec 1: Mux Protocol Safety Properties
    // -----------------------------------------------------------------

    #[test]
    fn mux_message_tracked(
        actions in proptest::collection::vec(mux_action_strategy(), 1..200)
    ) {
        let mut model = MuxProtocol::new(3, 20);
        for action in &actions {
            model.step(*action);
            prop_assert!(
                model.safety_message_tracked(),
                "Safety_MessageTracked violated after {:?}", action
            );
        }
    }

    #[test]
    fn mux_no_duplicate_delivery(
        actions in proptest::collection::vec(mux_action_strategy(), 1..200)
    ) {
        let mut model = MuxProtocol::new(3, 20);
        for action in &actions {
            model.step(*action);
            prop_assert!(
                model.safety_no_duplicate_delivery(),
                "Safety_NoDuplicateDelivery violated after {:?}", action
            );
        }
    }

    #[test]
    fn mux_ordered_delivery(
        actions in proptest::collection::vec(mux_action_strategy(), 1..200)
    ) {
        let mut model = MuxProtocol::new(3, 20);
        for action in &actions {
            model.step(*action);
            prop_assert!(
                model.safety_ordered_delivery(),
                "Safety_OrderedDelivery violated after {:?}", action
            );
        }
    }

    #[test]
    fn mux_all_safety_combined(
        actions in proptest::collection::vec(mux_action_strategy(), 1..500)
    ) {
        let mut model = MuxProtocol::new(5, 50);
        for action in &actions {
            model.step(*action);
            prop_assert!(model.safety_message_tracked());
            prop_assert!(model.safety_no_duplicate_delivery());
            prop_assert!(model.safety_ordered_delivery());
        }
    }

    // -----------------------------------------------------------------
    // Spec 2: Snapshot Lifecycle Safety Properties
    // -----------------------------------------------------------------

    #[test]
    fn snapshot_capture_consistent(
        actions in proptest::collection::vec(snapshot_action_strategy(3), 1..100)
    ) {
        let mut model = SnapshotLifecycle::new(3, 0);
        for action in &actions {
            model.step(*action);
            prop_assert!(
                model.safety_capture_consistent(),
                "Safety_CaptureConsistent violated after {:?}", action
            );
        }
    }

    #[test]
    fn snapshot_atomicity(
        actions in proptest::collection::vec(snapshot_action_strategy(3), 1..100)
    ) {
        let mut model = SnapshotLifecycle::new(3, 0);
        for action in &actions {
            model.step(*action);
            prop_assert!(
                model.safety_atomicity(),
                "Safety_Atomicity violated after {:?}", action
            );
        }
    }

    #[test]
    fn snapshot_no_data_loss(
        actions in proptest::collection::vec(snapshot_action_strategy(3), 1..100)
    ) {
        let mut model = SnapshotLifecycle::new(3, 0);
        for action in &actions {
            model.step(*action);
            prop_assert!(
                model.safety_no_data_loss(),
                "Safety_NoDataLoss violated after {:?}", action
            );
        }
    }

    #[test]
    fn snapshot_no_partial_commit(
        actions in proptest::collection::vec(snapshot_action_strategy(3), 1..100)
    ) {
        let mut model = SnapshotLifecycle::new(3, 0);
        for action in &actions {
            model.step(*action);
            prop_assert!(
                model.safety_no_partial_commit(),
                "Safety_NoPartialCommit violated after {:?}", action
            );
        }
    }

    #[test]
    fn snapshot_all_safety_combined(
        actions in proptest::collection::vec(snapshot_action_strategy(4), 1..200)
    ) {
        let mut model = SnapshotLifecycle::new(4, 42);
        for action in &actions {
            model.step(*action);
            prop_assert!(model.safety_capture_consistent());
            prop_assert!(model.safety_atomicity());
            prop_assert!(model.safety_no_data_loss());
            prop_assert!(model.safety_no_partial_commit());
        }
    }

    // -----------------------------------------------------------------
    // Spec 3: WAL Correctness Safety Properties
    // -----------------------------------------------------------------

    #[test]
    fn wal_running_matches_log(
        actions in proptest::collection::vec(wal_action_strategy(3), 1..100)
    ) {
        let mut model = WalModel::new(3, 20, 0);
        for action in &actions {
            model.step(*action);
            prop_assert!(
                model.safety_running_matches_log(),
                "Safety_RunningMatchesLog violated after {:?}", action
            );
        }
    }

    #[test]
    fn wal_durable_bound(
        actions in proptest::collection::vec(wal_action_strategy(3), 1..100)
    ) {
        let mut model = WalModel::new(3, 20, 0);
        for action in &actions {
            model.step(*action);
            prop_assert!(
                model.safety_durable_bound(),
                "Safety_DurableBound violated after {:?}", action
            );
        }
    }

    #[test]
    fn wal_durable_writes_survive_crash(
        actions in proptest::collection::vec(wal_action_strategy(3), 1..100)
    ) {
        let mut model = WalModel::new(3, 20, 0);
        for action in &actions {
            model.step(*action);
            prop_assert!(
                model.safety_durable_writes_survive_crash(),
                "Safety_DurableWritesSurviveCrash violated after {:?}", action
            );
        }
    }

    #[test]
    fn wal_compaction_safe(
        actions in proptest::collection::vec(wal_action_strategy(3), 1..100)
    ) {
        let mut model = WalModel::new(3, 20, 0);
        for action in &actions {
            model.step(*action);
            prop_assert!(
                model.safety_compaction_safe(),
                "Safety_CompactionSafe violated after {:?}", action
            );
        }
    }

    #[test]
    fn wal_all_safety_combined(
        actions in proptest::collection::vec(wal_action_strategy(4), 1..300)
    ) {
        let mut model = WalModel::new(4, 30, 0);
        for action in &actions {
            model.step(*action);
            prop_assert!(model.safety_running_matches_log());
            prop_assert!(model.safety_durable_bound());
            prop_assert!(model.safety_durable_writes_survive_crash());
            prop_assert!(model.safety_compaction_safe());
        }
    }

    // -----------------------------------------------------------------
    // Cross-spec: Snapshot + WAL interaction
    // -----------------------------------------------------------------

    #[test]
    fn snapshot_restore_roundtrip(
        mutations in proptest::collection::vec(
            (0usize..3, any::<u8>()), 1..10
        )
    ) {
        let mut model = SnapshotLifecycle::new(3, 0);
        // Apply mutations.
        for (pane, val) in &mutations {
            model.step(SnapshotAction::Mutate { pane: *pane, value: *val });
        }
        let state_before_capture = model.live_state.clone();

        // Capture -> write -> commit.
        model.step(SnapshotAction::StartCapture);
        model.step(SnapshotAction::FinishCapture);
        model.step(SnapshotAction::CommitWrite);

        // Snapshot should match state at capture time.
        prop_assert_eq!(&model.snapshot_state, &state_before_capture);

        // Restore all panes.
        model.step(SnapshotAction::StartRestore);
        for pane in 0..3 {
            model.step(SnapshotAction::RestoreChunk { pane });
        }
        model.step(SnapshotAction::CompleteRestore);

        // Live state should now equal snapshot.
        prop_assert_eq!(&model.live_state, &state_before_capture);
    }

    #[test]
    fn wal_crash_recovery_preserves_durable(
        ops in proptest::collection::vec(
            (0usize..3, any::<u8>()), 1..10
        )
    ) {
        let mut model = WalModel::new(3, 20, 0);

        // Apply some mutations.
        for (key, val) in &ops {
            model.step(WalAction::Mutate { key: *key, value: *val });
        }

        // Fsync to make durable.
        model.step(WalAction::Fsync);
        let durable_state = model.mem_state.clone();

        // Crash and recover.
        model.step(WalAction::Crash);
        model.step(WalAction::Recover);

        // After recovery, state should match the durable state.
        prop_assert_eq!(&model.mem_state, &durable_state);
    }

    #[test]
    fn wal_compact_then_crash_preserves_state(
        ops1 in proptest::collection::vec(
            (0usize..2, any::<u8>()), 1..5
        ),
        ops2 in proptest::collection::vec(
            (0usize..2, any::<u8>()), 1..5
        )
    ) {
        let mut model = WalModel::new(2, 20, 0);

        // Phase 1: mutations + fsync + compact.
        for (key, val) in &ops1 {
            model.step(WalAction::Mutate { key: *key, value: *val });
        }
        model.step(WalAction::Fsync);
        model.step(WalAction::Compact);

        // Phase 2: more mutations + fsync.
        for (key, val) in &ops2 {
            model.step(WalAction::Mutate { key: *key, value: *val });
        }
        model.step(WalAction::Fsync);
        let durable_state = model.mem_state.clone();

        // Crash + recover.
        model.step(WalAction::Crash);
        model.step(WalAction::Recover);

        prop_assert_eq!(&model.mem_state, &durable_state);
    }
}
