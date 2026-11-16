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
        assert!(
            appended.is_ok(),
            "distinct cid-{i} must be accepted: {appended:?}"
        );
    }
    assert_eq!(
        journal.len(),
        10,
        "ten distinct correlation_ids → ten entries"
    );
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

/// REGRESSION for ft-3e8mv (fixed in 2f13122d1): `compact_before` reclaims
/// entry memory WITHOUT evicting compacted correlation_ids from the dedup
/// index. The index is the journal's historical idempotency oracle, so
/// membership must survive compaction — otherwise a redelivered control command
/// whose entry was compacted passes the dedup check and is applied twice.
///
/// (This test was authored as a characterization of the pre-fix bug, but the
/// fix had already landed ~1.5h earlier, so it was RED-on-HEAD from the start;
/// flipped to lock the correct post-fix behavior.)
///
/// This locks the index-membership half; `compaction_preserves_correlation_dedup`
/// below locks the append-rejection half (the observable double-apply guard).
#[test]
fn compaction_preserves_correlation_index_membership_ft_3e8mv() {
    let mut journal = MissionJournal::new(MissionId("m-compact-fixed".into()));
    let seq_a = journal
        .append(marker(), "cid-A", "op", "test", None, 1_000)
        .unwrap();
    journal
        .append(marker(), "cid-B", "op", "test", None, 2_000)
        .unwrap();
    assert!(journal.has_correlation("cid-A"));

    // Compact away cid-A's entry (seq < seq_a + 1): the entry is reclaimed…
    journal.compact_before(seq_a + 1);
    assert_eq!(
        journal.len(),
        1,
        "cid-A's entry is compacted away, cid-B remains"
    );

    // …but its correlation_id must remain in the historical dedup index.
    assert!(
        journal.has_correlation("cid-A"),
        "ft-3e8mv: compaction must NOT drop cid-A from the correlation index"
    );
    let re = journal.append(marker(), "cid-A", "op", "test", None, 3_000);
    assert!(
        matches!(re, Err(MissionJournalError::DuplicateCorrelation(ref c)) if c.as_str() == "cid-A"),
        "ft-3e8mv: a compacted correlation_id must still be rejected, not re-accepted; got {re:?}"
    );
}

/// DESIRED behavior, pinned to ft-3e8mv (fixed in 2f13122d1) — the regression
/// proof that compaction must NOT reopen the dedup window.
#[test]
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
    // cid-X's entry was compacted and the redelivery was rejected, so only
    // cid-Y remains.
    assert_eq!(
        journal.len(),
        1,
        "only cid-Y remains: cid-X compacted, its redelivery rejected"
    );
}

#[test]
fn correlation_index_cap_refuses_new_ids_fail_closed_ft_anpt8() {
    // ft-anpt8: the dedup index only grows (compaction must not reopen
    // correlation IDs — ft-3e8mv), so a long-running mission used to grow
    // one owned String per distinct control command forever. The fix bounds
    // the index: at the cap, NEW correlation IDs are refused with a typed
    // fail-closed error, while duplicates of already-accepted IDs keep
    // deduplicating exactly (the double-apply window never reopens).
    let mut journal = MissionJournal::new(MissionId("m-cap".into())).with_correlation_index_cap(3);

    for i in 0..3_i64 {
        journal
            .append(marker(), format!("cid-{i}"), "op", "test", None, 1_000 + i)
            .expect("appends under the cap succeed");
    }

    // A NEW id at the cap is refused fail-closed…
    let overflow = journal.append(marker(), "cid-new", "op", "test", None, 2_000);
    assert!(
        matches!(overflow, Err(MissionJournalError::CorrelationIndexFull(3))),
        "new correlation id at cap must be refused with CorrelationIndexFull, got {overflow:?}"
    );
    assert_eq!(journal.len(), 3, "refused append must not add an entry");
    assert!(
        !journal.has_correlation("cid-new"),
        "refused id must not be recorded as seen"
    );

    // …but duplicates of accepted ids still dedup exactly (the invariant the
    // cap must never weaken).
    let dup = journal.append(marker(), "cid-1", "op", "test", None, 3_000);
    assert!(
        matches!(dup, Err(MissionJournalError::DuplicateCorrelation(ref c)) if c.as_str() == "cid-1"),
        "duplicates must still be detected at cap, got {dup:?}"
    );
    assert!(journal.has_correlation("cid-1"));

    // Compaction reclaims entries but neither forgets accepted ids nor frees
    // cap headroom (the index is historical by design).
    journal.compact_before(u64::MAX);
    assert_eq!(journal.len(), 0);
    assert!(journal.has_correlation("cid-0"));
    let still_full = journal.append(marker(), "cid-after-compact", "op", "test", None, 4_000);
    assert!(
        matches!(
            still_full,
            Err(MissionJournalError::CorrelationIndexFull(3))
        ),
        "compaction must not reopen index headroom, got {still_full:?}"
    );
}

#[test]
fn correlation_index_cap_zero_is_clamped_to_one_ft_anpt8() {
    let mut journal =
        MissionJournal::new(MissionId("m-cap-zero".into())).with_correlation_index_cap(0);
    journal
        .append(marker(), "cid-only", "op", "test", None, 1_000)
        .expect("cap 0 clamps to 1, so one id is accepted");
    let second = journal.append(marker(), "cid-two", "op", "test", None, 2_000);
    assert!(
        matches!(second, Err(MissionJournalError::CorrelationIndexFull(1))),
        "second distinct id must hit the clamped cap, got {second:?}"
    );
}
