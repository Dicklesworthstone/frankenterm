//! Golden + metamorphic tests for `ArtifactRegistry::prune` (ft-wbm46;
//! bead ft-gqmju).
//!
//! ft-wbm46 was a data-loss fail-open: prune built its removal set from
//! retired+aged entries but then ran `retain(|a| !pruned_paths.contains(&a.path))`
//! — removing EVERY manifest entry whose path equals a pruned path. Since
//! `from_toml` does not enforce path uniqueness, a manifest with an Active entry
//! and a retired+aged entry sharing a path lost the Active entry's record
//! (label/sha256/sensitivity/status) when the retired duplicate was pruned. The
//! fix prunes by a single `is_prunable` predicate (status==Retired AND aged) for
//! BOTH the file-removal targets and the manifest `retain`.
//!
//! These tests pin the invariant the fix establishes — prune/retain never drops
//! a live (Active) manifest entry — as metamorphic relations + a golden repro.
//! They use a real registry over a temp base dir with NO artifact files on disk:
//! prune's best-effort file removal is skipped (the path does not resolve), but
//! the manifest mutation — the ft-wbm46 data-loss surface — runs regardless.

use std::collections::BTreeSet;

use frankenterm_core_replay::replay_artifact_registry::{
    ArtifactEntry, ArtifactManifest, ArtifactRegistry, ArtifactSensitivityTier, ArtifactStatus,
    PruneOptions,
};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const RETENTION_DAYS: u64 = 1;

fn entry(path: &str, label: &str, status: ArtifactStatus, retired_at_ms: Option<u64>) -> ArtifactEntry {
    ArtifactEntry {
        path: path.to_string(),
        label: label.to_string(),
        // Distinct, label-derived digest so a dropped/kept record is identifiable.
        sha256: format!("sha-{label}"),
        event_count: 1,
        decision_count: 1,
        created_at_ms: 0,
        // Non-default tier so we can prove the full provenance record survives.
        sensitivity_tier: ArtifactSensitivityTier::T2,
        status,
        size_bytes: 100,
        retire_reason: retired_at_ms.map(|_| "drill".to_string()),
        retired_at_ms,
    }
}

fn registry(entries: Vec<ArtifactEntry>) -> ArtifactRegistry {
    let mut manifest = ArtifactManifest::new();
    manifest.artifacts = entries;
    ArtifactRegistry::new(manifest, std::env::temp_dir())
}

fn prune_opts(now_ms: u64, dry_run: bool) -> PruneOptions {
    PruneOptions {
        dry_run,
        max_age_days: RETENTION_DAYS,
        now_ms,
    }
}

/// The intended prune predicate, restated independently: status==Retired AND
/// retired at least `RETENTION_DAYS` before `now_ms`.
fn is_prunable(e: &ArtifactEntry, now_ms: u64) -> bool {
    e.status == ArtifactStatus::Retired
        && e.retired_at_ms
            .is_some_and(|r| now_ms.saturating_sub(r) >= RETENTION_DAYS * DAY_MS)
}

fn labels(entries: &[ArtifactEntry], pred: impl Fn(&ArtifactEntry) -> bool) -> BTreeSet<String> {
    entries
        .iter()
        .filter(|e| pred(e))
        .map(|e| e.label.clone())
        .collect()
}

/// A diverse fixture: Active + retired entries, several sharing a path, mixing
/// aged and recent retirements.
fn diverse_manifest(now_ms: u64) -> Vec<ArtifactEntry> {
    vec![
        entry("a.ftreplay", "a-active", ArtifactStatus::Active, None),
        // shares path with a-active; retired + aged => prunable
        entry("a.ftreplay", "a-retired-aged", ArtifactStatus::Retired, Some(1000)),
        // unique path; retired + aged => prunable
        entry("b.ftreplay", "b-retired-aged", ArtifactStatus::Retired, Some(0)),
        entry("c.ftreplay", "c-active", ArtifactStatus::Active, None),
        // shares path with c-active; retired but RECENT => not prunable
        entry(
            "c.ftreplay",
            "c-retired-recent",
            ArtifactStatus::Retired,
            Some(now_ms - DAY_MS / 2),
        ),
        entry("d.ftreplay", "d-active", ArtifactStatus::Active, None),
    ]
}

// ===========================================================================
// Metamorphic: prune never drops a live (Active) entry.
// ===========================================================================

#[test]
fn prune_never_drops_active_entries() {
    let now = 1000 + 5 * DAY_MS;
    let before = diverse_manifest(now);
    let active_before = labels(&before, |e| e.status == ArtifactStatus::Active);

    let mut reg = registry(before);
    reg.prune(&prune_opts(now, false));

    let active_after: BTreeSet<String> = reg
        .manifest()
        .artifacts
        .iter()
        .filter(|e| e.status == ArtifactStatus::Active)
        .map(|e| e.label.clone())
        .collect();
    assert_eq!(
        active_after, active_before,
        "prune must never drop an Active manifest entry (ft-wbm46 data-loss)"
    );

    // Each surviving Active entry keeps its full provenance record.
    for label in &active_before {
        let surviving = reg
            .manifest()
            .artifacts
            .iter()
            .find(|e| e.label == *label)
            .expect("active entry survives");
        assert_eq!(surviving.status, ArtifactStatus::Active);
        assert_eq!(surviving.sha256, format!("sha-{label}"));
        assert_eq!(
            surviving.sensitivity_tier,
            ArtifactSensitivityTier::T2,
            "active entry's sensitivity/provenance must survive"
        );
    }
}

#[test]
fn prune_removes_exactly_the_retired_and_aged_set() {
    let now = 1000 + 5 * DAY_MS;
    let before = diverse_manifest(now);
    let prunable_before = labels(&before, |e| is_prunable(e, now));
    let kept_before = labels(&before, |e| !is_prunable(e, now));

    let mut reg = registry(before);
    let result = reg.prune(&prune_opts(now, false));

    let kept_after: BTreeSet<String> = reg
        .manifest()
        .artifacts
        .iter()
        .map(|e| e.label.clone())
        .collect();
    assert_eq!(
        kept_after, kept_before,
        "exactly the non-prunable entries survive (Active + recent-Retired)"
    );
    assert_eq!(
        usize::try_from(result.pruned_count).unwrap(),
        prunable_before.len(),
        "pruned_count equals the retired+aged count"
    );
    assert!(
        reg.manifest()
            .artifacts
            .iter()
            .all(|e| !is_prunable(e, now)),
        "no prunable entry may survive prune"
    );
}

#[test]
fn prune_is_idempotent() {
    let now = 1000 + 5 * DAY_MS;
    let mut reg = registry(diverse_manifest(now));

    reg.prune(&prune_opts(now, false));
    let after_first = reg.manifest().artifacts.clone();

    let second = reg.prune(&prune_opts(now, false));
    assert_eq!(second.pruned_count, 0, "a second prune removes nothing (idempotent)");
    assert_eq!(
        reg.manifest().artifacts,
        after_first,
        "the manifest is unchanged by a second prune"
    );
}

#[test]
fn prune_dry_run_leaves_manifest_unchanged() {
    let now = 1000 + 5 * DAY_MS;
    let before = diverse_manifest(now);
    let mut reg = registry(before.clone());

    let result = reg.prune(&prune_opts(now, true));
    assert!(result.dry_run);
    assert!(result.pruned_count > 0, "dry-run still REPORTS the prunable count");
    assert_eq!(
        reg.manifest().artifacts,
        before,
        "dry-run must not mutate the manifest"
    );
}

// ===========================================================================
// Golden: the exact ft-wbm46 repro (active + pruned retired duplicate path).
// ===========================================================================

#[test]
fn golden_active_survives_pruned_retired_duplicate_path() {
    // 2 days after retirement, past the 1-day retention.
    let now = 1000 + 2 * DAY_MS;
    let mut reg = registry(vec![
        entry("dup.ftreplay", "active", ArtifactStatus::Active, None),
        entry("dup.ftreplay", "retired", ArtifactStatus::Retired, Some(1000)),
    ]);

    let result = reg.prune(&prune_opts(now, false));
    assert_eq!(result.pruned_count, 1, "only the retired+aged duplicate is pruned");

    let at_path: Vec<&ArtifactEntry> = reg
        .manifest()
        .artifacts
        .iter()
        .filter(|e| e.path == "dup.ftreplay")
        .collect();
    assert_eq!(at_path.len(), 1, "the Active duplicate-path entry survives");
    assert_eq!(at_path[0].status, ArtifactStatus::Active);
    assert_eq!(at_path[0].label, "active");
    assert_eq!(
        at_path[0].sha256, "sha-active",
        "the surviving record is the Active one, with its checksum intact"
    );
}
