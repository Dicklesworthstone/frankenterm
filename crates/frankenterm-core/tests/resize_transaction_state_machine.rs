#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TxPhase {
    #[default]
    Idle,
    Queued,
    Preparing,
    Reflowing,
    Presenting,
    Committed,
    Cancelled,
    Failed,
}

#[derive(Debug, Default)]
struct ResizeTxnModel {
    phase: TxPhase,
    active_seq: Option<u64>,
    latest_seq: Option<u64>,
    cancelled: Vec<u64>,
    committed: Vec<u64>,
}

impl ResizeTxnModel {
    fn submit_intent(&mut self, seq: u64) {
        if let Some(latest) = self.latest_seq {
            assert!(
                seq > latest,
                "intent sequence must be monotonic: got {} after {}",
                seq,
                latest
            );
        }
        self.latest_seq = Some(seq);
        if self.active_seq.is_none() {
            self.active_seq = Some(seq);
            self.phase = TxPhase::Queued;
        }
    }

    fn cancel_if_superseded(&mut self) -> bool {
        match (self.active_seq, self.latest_seq) {
            (Some(active), Some(latest)) if latest > active => {
                self.phase = TxPhase::Cancelled;
                self.cancelled.push(active);
                self.active_seq = Some(latest);
                self.phase = TxPhase::Queued;
                true
            }
            _ => false,
        }
    }

    fn start_prepare(&mut self) -> bool {
        if self.cancel_if_superseded() {
            return false;
        }
        if self.phase != TxPhase::Queued {
            return false;
        }
        self.phase = TxPhase::Preparing;
        true
    }

    fn start_reflow(&mut self) -> bool {
        if self.cancel_if_superseded() {
            return false;
        }
        if self.phase != TxPhase::Preparing {
            return false;
        }
        self.phase = TxPhase::Reflowing;
        true
    }

    fn start_present(&mut self) -> bool {
        if self.cancel_if_superseded() {
            return false;
        }
        if self.phase != TxPhase::Reflowing {
            return false;
        }
        self.phase = TxPhase::Presenting;
        true
    }

    fn commit(&mut self) -> bool {
        if self.cancel_if_superseded() {
            return false;
        }
        if self.phase != TxPhase::Presenting {
            return false;
        }
        let seq = self
            .active_seq
            .expect("active sequence must be present while presenting");
        if self.latest_seq != Some(seq) {
            self.phase = TxPhase::Failed;
            return false;
        }

        self.phase = TxPhase::Committed;
        self.committed.push(seq);
        self.active_seq = None;
        self.latest_seq = None;
        self.phase = TxPhase::Idle;
        true
    }
}

#[test]
fn state_machine_happy_path_commits_single_intent() {
    let mut model = ResizeTxnModel::default();
    model.submit_intent(1);

    assert_eq!(model.phase, TxPhase::Queued);
    assert!(model.start_prepare());
    assert!(model.start_reflow());
    assert!(model.start_present());
    assert!(model.commit());

    assert_eq!(model.phase, TxPhase::Idle);
    assert_eq!(model.committed, vec![1]);
    assert!(model.cancelled.is_empty());
}

#[test]
fn superseded_intent_is_cancelled_at_phase_boundary() {
    let mut model = ResizeTxnModel::default();
    model.submit_intent(1);
    assert!(model.start_prepare());

    // Intent 2 supersedes in-flight intent 1.
    model.submit_intent(2);

    // First boundary check should cancel stale work and requeue latest.
    assert!(!model.start_reflow());
    assert_eq!(model.phase, TxPhase::Queued);
    assert_eq!(model.cancelled, vec![1]);

    // Run latest intent to completion.
    assert!(model.start_prepare());
    assert!(model.start_reflow());
    assert!(model.start_present());
    assert!(model.commit());
    assert_eq!(model.committed, vec![2]);
}

#[test]
fn resize_storm_coalesces_to_latest_intent_only() {
    let mut model = ResizeTxnModel::default();
    model.submit_intent(1);
    assert!(model.start_prepare());

    // Rapid storm while intent 1 is active.
    model.submit_intent(2);
    model.submit_intent(3);
    model.submit_intent(4);
    model.submit_intent(5);

    // Boundary cancellation promotes only the latest.
    assert!(!model.start_reflow());
    assert_eq!(model.phase, TxPhase::Queued);
    assert_eq!(model.cancelled, vec![1]);
    assert_eq!(model.active_seq, Some(5));

    assert!(model.start_prepare());
    assert!(model.start_reflow());
    assert!(model.start_present());
    assert!(model.commit());

    assert_eq!(model.committed, vec![5]);
}

#[test]
fn stale_commit_is_prevented_by_boundary_cancellation() {
    let mut model = ResizeTxnModel::default();
    model.submit_intent(10);
    assert!(model.start_prepare());
    assert!(model.start_reflow());

    // Newer intent arrives before present/commit.
    model.submit_intent(11);

    // Transition into present should be blocked by stale cancellation.
    assert!(!model.start_present());
    assert_eq!(model.phase, TxPhase::Queued);
    assert_eq!(model.cancelled, vec![10]);

    // Only latest can commit.
    assert!(model.start_prepare());
    assert!(model.start_reflow());
    assert!(model.start_present());
    assert!(model.commit());
    assert_eq!(model.committed, vec![11]);
    assert_ne!(model.committed, vec![10]);
}

// ── DarkBadger wa-1u90p.7.1 ──────────────────────────────────────

#[test]
fn default_model_starts_idle_with_no_sequences() {
    let model = ResizeTxnModel::default();
    assert_eq!(model.phase, TxPhase::Idle);
    assert_eq!(model.active_seq, None);
    assert_eq!(model.latest_seq, None);
    assert!(model.cancelled.is_empty());
    assert!(model.committed.is_empty());
}

#[test]
fn cannot_prepare_from_idle_without_intent() {
    let mut model = ResizeTxnModel::default();
    assert!(!model.start_prepare());
    assert_eq!(model.phase, TxPhase::Idle);
}

#[test]
fn cannot_reflow_from_idle() {
    let mut model = ResizeTxnModel::default();
    assert!(!model.start_reflow());
    assert_eq!(model.phase, TxPhase::Idle);
}

#[test]
fn cannot_present_from_idle() {
    let mut model = ResizeTxnModel::default();
    assert!(!model.start_present());
    assert_eq!(model.phase, TxPhase::Idle);
}

#[test]
fn cannot_commit_from_idle() {
    let mut model = ResizeTxnModel::default();
    assert!(!model.commit());
    assert_eq!(model.phase, TxPhase::Idle);
}

#[test]
fn cannot_skip_prepare_phase() {
    let mut model = ResizeTxnModel::default();
    model.submit_intent(1);
    // Try to jump straight to reflow without prepare
    assert!(!model.start_reflow());
    assert_eq!(model.phase, TxPhase::Queued);
}

#[test]
fn cannot_skip_reflow_phase() {
    let mut model = ResizeTxnModel::default();
    model.submit_intent(1);
    assert!(model.start_prepare());
    // Try to jump straight to present without reflow
    assert!(!model.start_present());
    assert_eq!(model.phase, TxPhase::Preparing);
}

#[test]
fn cannot_commit_from_preparing() {
    let mut model = ResizeTxnModel::default();
    model.submit_intent(1);
    assert!(model.start_prepare());
    assert!(!model.commit());
    assert_eq!(model.phase, TxPhase::Preparing);
}

#[test]
fn cannot_commit_from_reflowing() {
    let mut model = ResizeTxnModel::default();
    model.submit_intent(1);
    assert!(model.start_prepare());
    assert!(model.start_reflow());
    assert!(!model.commit());
    assert_eq!(model.phase, TxPhase::Reflowing);
}

#[test]
fn sequential_transactions_commit_independently() {
    let mut model = ResizeTxnModel::default();

    // First transaction
    model.submit_intent(1);
    assert!(model.start_prepare());
    assert!(model.start_reflow());
    assert!(model.start_present());
    assert!(model.commit());
    assert_eq!(model.committed, vec![1]);

    // Second transaction
    model.submit_intent(2);
    assert!(model.start_prepare());
    assert!(model.start_reflow());
    assert!(model.start_present());
    assert!(model.commit());
    assert_eq!(model.committed, vec![1, 2]);
    assert!(model.cancelled.is_empty());
}

#[test]
fn supersede_at_reflow_stage_cancels_and_requeues() {
    let mut model = ResizeTxnModel::default();
    model.submit_intent(1);
    assert!(model.start_prepare());
    assert!(model.start_reflow());

    model.submit_intent(2);

    // Next transition attempt detects supersession
    assert!(!model.start_present());
    assert_eq!(model.phase, TxPhase::Queued);
    assert_eq!(model.cancelled, vec![1]);
    assert_eq!(model.active_seq, Some(2));
}

#[test]
fn supersede_at_presenting_stage_cancels_on_commit() {
    let mut model = ResizeTxnModel::default();
    model.submit_intent(1);
    assert!(model.start_prepare());
    assert!(model.start_reflow());
    assert!(model.start_present());

    model.submit_intent(2);

    // Commit detects supersession
    assert!(!model.commit());
    assert_eq!(model.cancelled, vec![1]);
    assert_eq!(model.active_seq, Some(2));
    assert_eq!(model.phase, TxPhase::Queued);
}

#[test]
fn multiple_supersede_chains_accumulate_cancellations() {
    let mut model = ResizeTxnModel::default();
    model.submit_intent(1);
    assert!(model.start_prepare());

    model.submit_intent(2);
    assert!(!model.start_reflow()); // cancels 1
    assert_eq!(model.cancelled, vec![1]);

    assert!(model.start_prepare());
    model.submit_intent(3);
    assert!(!model.start_reflow()); // cancels 2
    assert_eq!(model.cancelled, vec![1, 2]);

    // Final transaction completes
    assert!(model.start_prepare());
    assert!(model.start_reflow());
    assert!(model.start_present());
    assert!(model.commit());
    assert_eq!(model.committed, vec![3]);
}

#[test]
#[should_panic(expected = "intent sequence must be monotonic")]
fn non_monotonic_sequence_panics() {
    let mut model = ResizeTxnModel::default();
    model.submit_intent(5);
    model.submit_intent(3); // violation: 3 < 5
}

#[test]
#[should_panic(expected = "intent sequence must be monotonic")]
fn duplicate_sequence_panics() {
    let mut model = ResizeTxnModel::default();
    model.submit_intent(1);
    model.submit_intent(1); // violation: 1 == 1
}

#[test]
fn tx_phase_default_is_idle() {
    assert_eq!(TxPhase::default(), TxPhase::Idle);
}

#[test]
fn tx_phase_clone_and_debug() {
    let phase = TxPhase::Reflowing;
    let cloned = phase;
    assert_eq!(phase, cloned);
    let dbg = format!("{:?}", phase);
    assert!(dbg.contains("Reflowing"));
}

#[test]
fn submit_intent_when_active_does_not_replace_active_seq() {
    let mut model = ResizeTxnModel::default();
    model.submit_intent(1);
    assert_eq!(model.active_seq, Some(1));

    model.submit_intent(2);
    // active_seq stays as 1 until a phase boundary check
    assert_eq!(model.active_seq, Some(1));
    assert_eq!(model.latest_seq, Some(2));
}

#[test]
fn cancel_if_superseded_is_noop_when_latest_equals_active() {
    let mut model = ResizeTxnModel::default();
    model.submit_intent(1);
    model.phase = TxPhase::Preparing;
    assert!(!model.cancel_if_superseded());
    assert_eq!(model.phase, TxPhase::Preparing);
    assert!(model.cancelled.is_empty());
}
