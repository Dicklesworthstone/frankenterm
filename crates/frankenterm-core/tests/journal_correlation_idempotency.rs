//! MissionJournal correlation-id idempotency e2e (ft-7kk4x).
//!
//! The journal's sole de-duplication oracle is its `correlation_index`:
//! `append()` rejects a control command whose `correlation_id` it has already
//! seen, so a redelivered/replayed command is applied at most once. These tests
//! lock that invariant in and pin the known compaction gap (ft-3e8mv).
//!
//! Scope note: the *transactional* commit/compensation dispatch idempotency
//! (double-execution-blocked, resume-after-crash, both-phase replay) is already
//! covered by `tx_correctness_suite.rs`
//! (`conformance_prepare_commit_compensate_idempotent_replay_matrix`,
//! `idempotency_full_lifecycle_fresh_commit_then_duplicate`,
//! `idempotency_resume_after_crash_mid_commit`, …, feature `subprocess-bridge`).
//! The only remaining tx gap is the ft-7dk63 execute-path write-ahead ordering,
//! which is an open design decision, not a missing test. This file fills the
//! untested journal side.

use frankenterm_core::plan::{
    MissionId, MissionJournal, MissionJournalEntryKind, MissionJournalError,
};

/// A minimal, always-valid journal entry kind for dedup tests.
fn marker() -> MissionJournalEntryKind {
    MissionJournalEntryKind::RecoveryMarker {
        recovered_through_seq: 0,
        recovery_reason: "test".into(),
    }
}

#[test]
fn append_rejects_duplicate_correlation_id() {
    let mut journal = MissionJournal::new(MissionId("m-dedup".into()));

    journal
        .append(marker(), "cid-1", "op", "test", None, 1_000)
        .expect("first append of cid-1 succeeds");
    assert_eq!(journal.len(), 1);

    let dup = journal.append(marker(), "cid-1", "op", "test", None, 2_000);
    assert!(
        matches!(dup, Err(MissionJournalError::DuplicateCorrelation(ref c)) if c.as_str() == "cid-1"),
        "a redelivered correlation_id must be rejected, got {dup:?}"
    );
    assert_eq!(
        journal.len(),
        1,
        "a rejected duplicate must not append a second entry"
    );
}

#[test]
fn distinct_correlation_ids_are_all_accepted() {
    let mut journal = MissionJournal::new(MissionId("m-distinct".into()));
    for i in 0..10_i64 {
        let appended = journal.append(marker(), format!("cid-{i}"), "op", "test", None, 1_000 + i);
        assert!(appended.is_ok(), "distinct cid-{i} must be accepted: {appended:?}");
    }
    assert_eq!(journal.len(), 10, "ten distinct correlation_ids → ten entries");
}

#[test]
fn replay_from_checkpoint_is_idempotent() {
    let mut journal = MissionJournal::new(MissionId("m-replay".into()));
    for i in 0..5_i64 {
        journal
            .append(marker(), format!("cid-{i}"), "op", "test", None, 1_000 + i)
            .unwrap();
    }
    // Replay is a pure read over the entries; running it twice must yield the
    // identical report (no hidden mutation / non-determinism).
    let first = journal.replay_from_checkpoint();
    let second = journal.replay_from_checkpoint();
    assert_eq!(first, second, "replay_from_checkpoint must be idempotent");
}

/// CHARACTERIZATION of the ft-3e8mv bug — passes TODAY.
///
/// `MissionJournal::compact_before` removes compacted entries' correlation_ids
/// from the dedup index, so a redelivered control command with an already-seen
/// (but compacted) correlation_id is WRONGLY re-accepted. This test locks the
/// bug's exact shape; DELETE or flip it once ft-3e8mv is fixed (see the
/// `#[ignore]`d desired-behavior test below, which becomes the regression proof).
#[test]
fn compaction_currently_drops_correlation_dedup_ft_3e8mv() {
    let mut journal = MissionJournal::new(MissionId("m-compact-bug".into()));
    let seq_a = journal
        .append(marker(), "cid-A", "op", "test", None, 1_000)
        .unwrap();
    journal
        .append(marker(), "cid-B", "op", "test", None, 2_000)
        .unwrap();
    assert!(journal.has_correlation("cid-A"));

    // Compact away cid-A's entry (seq < seq_a + 1).
    journal.compact_before(seq_a + 1);

    assert!(
        !journal.has_correlation("cid-A"),
        "ft-3e8mv: compaction drops cid-A from the correlation index"
    );
    let re = journal.append(marker(), "cid-A", "op", "test", None, 3_000);
    assert!(
        re.is_ok(),
        "ft-3e8mv BUG (characterized): a compacted correlation_id is re-accepted \
         instead of rejected; got {re:?}"
    );
}

/// DESIRED behavior, pinned to ft-3e8mv. Un-ignore when the fix lands — this is
/// the regression proof that compaction must NOT reopen the dedup window.
#[test]
#[ignore = "ft-3e8mv: MissionJournal::compact_before drops correlation_index entries; \
            un-ignore when fixed — correlation dedup MUST survive compaction"]
fn compaction_preserves_correlation_dedup() {
    let mut journal = MissionJournal::new(MissionId("m-compact-fixed".into()));
    let seq_x = journal
        .append(marker(), "cid-X", "op", "test", None, 1_000)
        .unwrap();
    journal
        .append(marker(), "cid-Y", "op", "test", None, 2_000)
        .unwrap();

    journal.compact_before(seq_x + 1); // compact cid-X's entry

    // A redelivered command with an already-seen correlation_id must STILL be
    // rejected after compaction — idempotency must survive journal GC.
    let re = journal.append(marker(), "cid-X", "op", "test", None, 3_000);
    assert!(
        matches!(re, Err(MissionJournalError::DuplicateCorrelation(_))),
        "compaction must preserve correlation dedup (ft-3e8mv): got {re:?}"
    );
    assert_eq!(
        journal.len(),
        2,
        "no duplicate re-appended after compaction (cid-Y + the rejected cid-X)"
    );
}
