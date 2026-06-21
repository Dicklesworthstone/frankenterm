//! Golden + metamorphic tests for the incident/recorder subsystem (ft-r2wv3).
//!
//! Locks in three correctness properties the reality-check sweep cares about:
//!
//! * **ft-womif** — `EvidenceLifecycleReceipt::compute_receipt_id` is a
//!   content-addressed digest that must be INJECTIVE on its decision tuples: a
//!   free-text `evidence_id` containing the field delimiters must NOT let two
//!   distinct decision sets collide to the same `receipt_id` (the receipt id is
//!   used as a file-per-receipt store key, so a collision is a silent
//!   overwrite). The fix length-prefixes every field; these tests fail closed if
//!   that framing ever regresses to naive delimiter concatenation.
//! * **ft-odrq7** — mmap scrollback compaction must be crash-safe and
//!   content-preserving (write-temp + fsync + atomic rename): retained content
//!   is invariant under compaction, the physical file shrinks to exactly the
//!   retained suffix, no `.compact.tmp` leaks, and compaction is idempotent.
//! * **ft-ja9i1** — the recorder-query gate must actually ENFORCE access tiers
//!   (an under-privileged actor is denied) and audit every attempt.
//!
//! Zero-RCH: real public APIs, no mocks; pure in-process + tempdir.

use frankenterm_core::incident_bundle::{
    EvidenceArtifactKind, EvidenceLifecycleAction, EvidenceLifecycleDecision,
    EvidenceLifecycleReason, EvidenceLifecycleReceipt,
};

// ── ft-womif: receipt canonicalization injection-resistance ──────────────────

fn decision(evidence_id: &str) -> EvidenceLifecycleDecision {
    EvidenceLifecycleDecision {
        evidence_id: evidence_id.to_string(),
        kind: EvidenceArtifactKind::PaneOutput,
        action: EvidenceLifecycleAction::Retain,
        reason: EvidenceLifecycleReason::WithinPolicy,
        effective_expires_at_ms: None,
    }
}

fn receipt(decisions: Vec<EvidenceLifecycleDecision>) -> EvidenceLifecycleReceipt {
    EvidenceLifecycleReceipt {
        contract_id: "ct-test".to_string(),
        schema_version: 1,
        generated_at_ms: 1000,
        receipt_id: String::new(), // ignored by compute_receipt_id (recomputed from fields)
        decisions,
        minimize_count: 0,
        promote_count: 0,
        refused_promotion_count: 0,
        hold_count: 0,
        expire_count: 0,
    }
}

#[test]
fn womif_receipt_id_resists_delimiter_injection_metamorphic() {
    // Set A: two distinct decisions.
    let set_a = receipt(vec![decision("a"), decision("b")]);

    // Set B: ONE decision whose evidence_id is crafted to reproduce — under a
    // naive `|`/`\n`/`:`/`;`-concatenation scheme with no length prefix — the
    // exact byte tail that set A produces. Length-prefix framing must keep these
    // distinct (decisions.len() 2 vs 1 alone differs, and each field is framed).
    let injected = "a|pane_output|retain|within_policy|\nb:;";
    let set_b = receipt(vec![decision(injected)]);

    assert_ne!(
        set_a.compute_receipt_id(),
        set_b.compute_receipt_id(),
        "ft-womif: delimiter-injection in evidence_id must NOT collide two \
         distinct decision sets to the same receipt_id"
    );

    // Framing must not be aliasable: an evidence_id that mimics the length-prefix
    // encoding of another value must still hash differently.
    let plain = receipt(vec![decision("x")]);
    let framed_lookalike = receipt(vec![decision("1:x;")]);
    assert_ne!(
        plain.compute_receipt_id(),
        framed_lookalike.compute_receipt_id(),
        "ft-womif: an evidence_id mimicking the field framing must not alias"
    );

    // Distinct evidence_ids that differ only by a delimiter char are distinct.
    let id_pipe = receipt(vec![decision("a|b")]).compute_receipt_id();
    let id_split = receipt(vec![decision("a"), decision("b")]).compute_receipt_id();
    assert_ne!(
        id_pipe, id_split,
        "ft-womif: 'a|b' as one id must not equal ['a','b'] as two"
    );
}

#[test]
fn womif_receipt_id_is_deterministic_and_well_formed_golden() {
    let r1 = receipt(vec![decision("alpha"), decision("beta")]);
    let r2 = receipt(vec![decision("alpha"), decision("beta")]);

    let id1 = r1.compute_receipt_id();
    let id2 = r2.compute_receipt_id();

    // Golden: deterministic — identical inputs yield identical ids.
    assert_eq!(id1, id2, "compute_receipt_id must be deterministic");

    // Golden: stable id shape — `evidence:lifecycle:` + 32 lowercase hex chars.
    let suffix = id1
        .strip_prefix("evidence:lifecycle:")
        .expect("receipt_id must carry the documented prefix");
    assert_eq!(
        suffix.len(),
        32,
        "digest suffix must be 32 hex chars: {id1}"
    );
    assert!(
        suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "digest suffix must be lowercase hex: {id1}"
    );

    // Sensitivity: changing a single field changes the id.
    let r3 = receipt(vec![decision("alpha"), decision("BETA")]);
    assert_ne!(
        id1,
        r3.compute_receipt_id(),
        "a changed evidence_id must change the receipt_id"
    );
}

// ── ft-odrq7: crash-safe, content-preserving scrollback compaction ────────────

use frankenterm_core::storage::mmap_store::{MmapScrollbackStore, MmapStoreConfig};

#[test]
fn scrollback_compaction_preserves_retained_content_and_shrinks_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store =
        MmapScrollbackStore::new(MmapStoreConfig::new(dir.path().to_path_buf())).expect("store");

    for idx in 0..8 {
        store
            .append_line(1, &format!("line-{idx}"))
            .expect("append");
    }
    // Drop the stale prefix (keep seq 6,7) — leaves dead bytes at the file head.
    store.prune_before(1, 6).expect("prune");

    let tail_before = store.tail_lines(1, 10).expect("tail before");
    let bytes_before = store.file_bytes(1);
    assert_eq!(
        tail_before,
        vec!["line-6".to_string(), "line-7".to_string()]
    );

    assert!(
        store.compact_pane_if_stale(1, 1).expect("compact"),
        "compaction should run when a stale prefix exists"
    );

    // Metamorphic: retained content is INVARIANT under compaction.
    let tail_after = store.tail_lines(1, 10).expect("tail after");
    assert_eq!(
        tail_after, tail_before,
        "ft-odrq7: compaction must preserve the retained content exactly"
    );
    assert!(
        store.file_bytes(1) < bytes_before,
        "compaction must shrink the physical file to the retained suffix"
    );

    // Golden: the on-disk log holds EXACTLY the retained suffix (proves the file
    // was atomically replaced via rename, not re-grown from a zeroed file), and
    // no interrupted-compaction temp leaks.
    let raw = std::fs::read_to_string(dir.path().join("1.log")).expect("read log");
    assert_eq!(raw, "line-6\nline-7\n");
    assert!(
        !dir.path().join("1.log.compact.tmp").exists(),
        "ft-odrq7: compaction temp must be renamed away, never leaked"
    );
}

#[test]
fn scrollback_compaction_is_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store =
        MmapScrollbackStore::new(MmapStoreConfig::new(dir.path().to_path_buf())).expect("store");

    for idx in 0..6 {
        store.append_line(1, &format!("row-{idx}")).expect("append");
    }
    store.prune_before(1, 4).expect("prune");

    assert!(
        store.compact_pane_if_stale(1, 1).expect("first compact"),
        "first compaction runs (stale prefix present)"
    );
    let tail_once = store.tail_lines(1, 10).expect("tail");
    let bytes_once = store.file_bytes(1);

    // Metamorphic: compacting an already-compacted pane is a no-op — nothing is
    // stale, content and size are unchanged.
    assert!(
        !store.compact_pane_if_stale(1, 1).expect("second compact"),
        "ft-odrq7: a second compaction must be a no-op (idempotent)"
    );
    assert_eq!(store.tail_lines(1, 10).expect("tail2"), tail_once);
    assert_eq!(store.file_bytes(1), bytes_once);
}

// ── ft-ja9i1: recorder-query gate enforces access tiers + audits ─────────────

use frankenterm_core::policy::ActorKind;
use frankenterm_core::recorder_audit::{ActorIdentity, AuditLog, AuditLogConfig};
use frankenterm_core::recorder_query::{
    InMemoryEventStore, RecorderQueryExecutor, RecorderQueryRequest,
};

#[test]
fn recorder_query_gate_denies_underprivileged_actor_and_audits_every_attempt() {
    let executor = RecorderQueryExecutor::new(
        InMemoryEventStore::new(),
        AuditLog::new(AuditLogConfig::default()),
    );

    // A free-text search requires A2 (full query). A Robot actor defaults to A1
    // (redacted query) — strictly below A2.
    let request = RecorderQueryRequest::text_search("error");
    let robot = ActorIdentity::new(ActorKind::Robot, "robot-1");
    let human = ActorIdentity::new(ActorKind::Human, "human-1");

    // Honest gate: the under-privileged actor is DENIED (not silently allowed).
    let denied = executor.execute(&robot, &request, 1_000);
    assert!(
        denied.is_err(),
        "ft-ja9i1: an A1 actor must be denied an A2-requiring query, got {denied:?}"
    );

    // ...and an actor that meets the required tier is allowed.
    let allowed = executor.execute(&human, &request, 2_000);
    assert!(
        allowed.is_ok(),
        "ft-ja9i1: an A2 actor must be allowed the A2 query, got {allowed:?}"
    );

    // Audit honesty: EVERY attempt (deny + allow) is recorded.
    assert!(
        executor.audit_log().len() >= 2,
        "ft-ja9i1: the gate must audit every query attempt (got {} entries)",
        executor.audit_log().len()
    );
}

#[test]
fn recorder_query_metadata_only_is_authorized_for_low_tier_actor() {
    let executor = RecorderQueryExecutor::new(
        InMemoryEventStore::new(),
        AuditLog::new(AuditLogConfig::default()),
    );

    // Metadata-only (no text) requires A0 — even a Robot (A1) clears it.
    // Metamorphic counterpart to the deny case: dropping the privilege demand
    // (include_text=false) flips deny→allow for the same low-tier actor.
    let request = RecorderQueryRequest::for_panes(vec![7]).with_text(false);
    let robot = ActorIdentity::new(ActorKind::Robot, "robot-1");

    let result = executor.execute(&robot, &request, 1_000);
    assert!(
        result.is_ok(),
        "ft-ja9i1: a metadata-only (A0) query must be authorized for an A1 actor, got {result:?}"
    );
    assert!(
        !executor.audit_log().is_empty(),
        "the authorized query must still be audited"
    );
}
