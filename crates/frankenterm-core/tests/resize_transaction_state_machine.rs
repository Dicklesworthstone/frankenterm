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
