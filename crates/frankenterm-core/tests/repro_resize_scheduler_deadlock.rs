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
    #[ignore = "known deadlock reproduction; enable when fixing superseded completion semantics"]
    fn test_deadlock_when_completing_superseded_work() {
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

        // 3. Submit Intent 2 (Supersedes 1 in Pending, but 1 is Active)
        // Actually, submit_intent updates 'latest_seq' and 'pending'.
        // It does NOT touch active.
        scheduler.submit_intent(intent(1, 2));

        let snap = scheduler.snapshot();
        assert_eq!(snap.panes[0].active_seq, Some(1));
        assert_eq!(snap.panes[0].pending_seq, Some(2));
        assert_eq!(snap.panes[0].latest_seq, Some(2));

        // 4. Complete Active Intent 1
        // The worker finishes processing seq 1.
        // Even though seq 2 exists, seq 1 *did* finish.
        // Logic in complete_active checks if latest > active. 2 > 1.
        let completed = scheduler.complete_active(1, 1);

        // CURRENT BUG: This returns false and FAILS to clear active_seq.
        // EXPECTED FIX: This should clear active_seq so pending work can proceed.

        println!("Completed: {}", completed);

        let snap_after = scheduler.snapshot();
        // If deadlock exists, active_seq is still Some(1).
        if snap_after.panes[0].active_seq.is_some() {
            println!("DEADLOCK DETECTED: active_seq is still present after completion attempt.");
        } else {
            println!("Slot freed.");
        }

        // 5. Try to schedule Intent 2
        let frame2 = scheduler.schedule_frame();
        // If deadlocked, scheduled is empty.
        // If fixed, scheduled contains Intent 2.
        assert!(
            !frame2.scheduled.is_empty(),
            "DEADLOCK: Failed to schedule Intent 2 because Intent 1 slot was never freed."
        );
    }
}
