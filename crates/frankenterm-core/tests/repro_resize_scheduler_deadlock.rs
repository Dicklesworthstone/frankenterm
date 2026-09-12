#[cfg(test)]
mod tests {
    use frankenterm_core::resize_scheduler::{
        ResizeDomain, ResizeIntent, ResizeScheduler, ResizeSchedulerConfig, ResizeWorkClass,
    };

    fn intent(pane_id: u64, seq: u64) -> ResizeIntent {
        ResizeIntent {
            pane_id,
            intent_seq: seq,
            scheduler_class: ResizeWorkClass::Interactive,
            work_units: 1,
            submitted_at_ms: 100,
            domain: ResizeDomain::Local,
            tab_id: None,
        }
    }

    #[test]
    fn superseded_completion_requires_cancellation_before_rescheduling() {
        let config = ResizeSchedulerConfig::default();
        let mut scheduler = ResizeScheduler::new(config);

        // 1. Submit Intent 1
        scheduler.submit_intent(intent(1, 1));

        // 2. Schedule Frame -> Intent 1 becomes Active
        let result = scheduler.schedule_frame();
        assert_eq!(result.scheduled.len(), 1);
        assert_eq!(result.scheduled[0].intent_seq, 1);

        let snap = scheduler.snapshot();
        let pane = snap.panes.iter().find(|p| p.pane_id == 1).unwrap();
        assert_eq!(pane.active_seq, Some(1));

        // A newer intent supersedes the result, but the old worker still owns
        // the active slot until it acknowledges cancellation at a boundary.
        scheduler.submit_intent(intent(1, 2));

        let snap = scheduler.snapshot();
        assert_eq!(snap.panes[0].active_seq, Some(1));
        assert_eq!(snap.panes[0].pending_seq, Some(2));
        assert_eq!(snap.panes[0].latest_seq, Some(2));

        // Stale work must not commit or implicitly relinquish worker ownership.
        assert!(!scheduler.complete_active(1, 1));
        let snap_after = scheduler.snapshot();
        assert_eq!(snap_after.panes[0].active_seq, Some(1));
        assert!(scheduler.schedule_frame().scheduled.is_empty());
        assert_eq!(scheduler.metrics().completed_active, 0);

        // The documented cancellation handshake frees the slot exactly once.
        assert!(scheduler.cancel_active_if_superseded(1));
        assert!(!scheduler.cancel_active_if_superseded(1));
        assert_eq!(scheduler.metrics().cancelled_active, 1);

        // 5. Try to schedule Intent 2
        let frame2 = scheduler.schedule_frame();
        assert_eq!(frame2.scheduled.len(), 1);
        assert_eq!(frame2.scheduled[0].intent_seq, 2);
        // A late result from the cancelled worker cannot clear the new owner.
        assert!(!scheduler.complete_active(1, 1));
        assert_eq!(scheduler.snapshot().panes[0].active_seq, Some(2));
        assert!(scheduler.complete_active(1, 2));
        assert_eq!(scheduler.metrics().completed_active, 1);
        assert_eq!(scheduler.snapshot().panes[0].active_seq, None);
    }
}
