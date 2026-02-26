//! Vendored types for `beads_rust` (`br`) CLI integration.
//!
//! These mirror the JSON output of `br list --json` and `br show --json`
//! without depending on the `beads_rust` crate directly.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// Bead issue status values (matches br's status column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeadStatus {
    Open,
    InProgress,
    Blocked,
    Deferred,
    Closed,
}

impl std::fmt::Display for BeadStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open => f.write_str("open"),
            Self::InProgress => f.write_str("in_progress"),
            Self::Blocked => f.write_str("blocked"),
            Self::Deferred => f.write_str("deferred"),
            Self::Closed => f.write_str("closed"),
        }
    }
}

/// Bead issue type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeadIssueType {
    Epic,
    Feature,
    Task,
    Bug,
}

/// Priority level (0 = highest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BeadPriority(pub u8);

impl BeadPriority {
    pub const P0: Self = Self(0);
    pub const P1: Self = Self(1);
    pub const P2: Self = Self(2);
    pub const P3: Self = Self(3);
    pub const P4: Self = Self(4);

    /// Human label (e.g. "P0", "P1").
    #[must_use]
    pub fn label(&self) -> String {
        format!("P{}", self.0)
    }
}

impl std::fmt::Display for BeadPriority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "P{}", self.0)
    }
}

/// Summary of a bead from `br list --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeadSummary {
    pub id: String,
    pub title: String,
    pub status: BeadStatus,
    pub priority: u8,
    pub issue_type: BeadIssueType,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub dependency_count: usize,
    #[serde(default)]
    pub dependent_count: usize,
    /// Forward-compatibility for new br output fields.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl BeadSummary {
    /// Typed priority accessor.
    #[must_use]
    pub fn bead_priority(&self) -> BeadPriority {
        BeadPriority(self.priority)
    }

    /// Whether this bead is actionable (open, not blocked, not deferred).
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        matches!(self.status, BeadStatus::Open | BeadStatus::InProgress)
    }
}

/// Degraded-mode reason codes for DAG readiness resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeadResolverReasonCode {
    MissingDependencyNode,
    CyclicDependencyGraph,
    PartialGraphData,
}

/// Dependency or dependent edge reference from `br show --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeadDependencyRef {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub priority: Option<u8>,
    #[serde(default)]
    pub dependency_type: Option<String>,
}

impl BeadDependencyRef {
    /// Whether this edge should block readiness.
    ///
    /// `parent-child` is treated as a taxonomy edge and does not block.
    #[must_use]
    pub fn blocks_readiness(&self) -> bool {
        !matches!(self.dependency_type.as_deref(), Some("parent-child"))
    }
}

/// Detailed issue snapshot from `br show --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeadIssueDetail {
    pub id: String,
    pub title: String,
    pub status: BeadStatus,
    pub priority: u8,
    pub issue_type: BeadIssueType,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<BeadDependencyRef>,
    #[serde(default)]
    pub dependents: Vec<BeadDependencyRef>,
    #[serde(default)]
    pub parent: Option<String>,
    /// Optional ingest warning set by local fallback ingestion paths.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ingest_warning: Option<BeadResolverReasonCode>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl BeadIssueDetail {
    /// Build a degraded detail from a summary when `br show` is unavailable.
    #[must_use]
    pub fn from_summary(summary: BeadSummary) -> Self {
        Self {
            id: summary.id,
            title: summary.title,
            status: summary.status,
            priority: summary.priority,
            issue_type: summary.issue_type,
            assignee: summary.assignee,
            labels: summary.labels,
            dependencies: Vec::new(),
            dependents: Vec::new(),
            parent: None,
            ingest_warning: Some(BeadResolverReasonCode::PartialGraphData),
            extra: summary.extra,
        }
    }

    /// Whether this issue is in a state that can be considered for readiness.
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        matches!(self.status, BeadStatus::Open | BeadStatus::InProgress)
    }
}

/// Readiness candidate with graph-derived hints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeadReadyCandidate {
    pub id: String,
    pub title: String,
    pub status: BeadStatus,
    pub priority: u8,
    pub blocker_count: usize,
    pub blocker_ids: Vec<String>,
    pub transitive_unblock_count: usize,
    pub critical_path_depth_hint: usize,
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded_reasons: Vec<BeadResolverReasonCode>,
}

/// Full resolver output for actionable issues.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeadReadinessReport {
    pub candidates: Vec<BeadReadyCandidate>,
    pub ready_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded_reason_codes: Vec<BeadResolverReasonCode>,
}

impl BeadReadinessReport {
    /// Number of actionable items that are currently ready/unblocked.
    #[must_use]
    pub fn ready_count(&self) -> usize {
        self.ready_ids.len()
    }
}

/// Resolve actionable/ready issue candidates from detailed Beads DAG data.
#[must_use]
pub fn resolve_bead_readiness(issues: &[BeadIssueDetail]) -> BeadReadinessReport {
    let mut issue_by_id: HashMap<String, &BeadIssueDetail> = HashMap::new();
    for issue in issues {
        issue_by_id.insert(issue.id.clone(), issue);
    }

    // Build reverse graph: dependency -> dependents (blocking edges only).
    let mut downstream: HashMap<String, Vec<String>> = HashMap::new();
    for issue in issues {
        downstream.entry(issue.id.clone()).or_default();
    }
    for issue in issues {
        for dep in &issue.dependencies {
            if dep.blocks_readiness() && issue_by_id.contains_key(&dep.id) {
                downstream
                    .entry(dep.id.clone())
                    .or_default()
                    .push(issue.id.clone());
            }
        }
    }
    for children in downstream.values_mut() {
        children.sort();
        children.dedup();
    }

    let mut depth_memo: HashMap<String, usize> = HashMap::new();
    let mut cycle_seen = false;
    for issue in issues {
        let mut visiting = HashSet::new();
        let _ = compute_depth(
            &issue.id,
            &downstream,
            &mut depth_memo,
            &mut visiting,
            &mut cycle_seen,
        );
    }

    let mut candidates = Vec::new();
    let mut ready_ids = Vec::new();
    let mut global_degraded: HashSet<BeadResolverReasonCode> = HashSet::new();

    for issue in issues {
        if !issue.is_actionable() {
            continue;
        }

        let mut blockers = Vec::new();
        let mut degraded: HashSet<BeadResolverReasonCode> = HashSet::new();

        if let Some(reason) = issue.ingest_warning {
            degraded.insert(reason);
        }

        for dep in &issue.dependencies {
            if !dep.blocks_readiness() {
                continue;
            }
            match issue_by_id.get(&dep.id) {
                Some(dep_issue) if dep_issue.status == BeadStatus::Closed => {}
                Some(_) => blockers.push(dep.id.clone()),
                None => {
                    blockers.push(dep.id.clone());
                    degraded.insert(BeadResolverReasonCode::MissingDependencyNode);
                }
            }
        }

        blockers.sort();
        blockers.dedup();

        if cycle_seen {
            degraded.insert(BeadResolverReasonCode::CyclicDependencyGraph);
        }

        let ready = blockers.is_empty();
        if ready {
            ready_ids.push(issue.id.clone());
        }

        let transitive_unblock_count = count_transitive_descendants(&issue.id, &downstream);
        let critical_path_depth_hint = *depth_memo.get(&issue.id).unwrap_or(&0);

        let mut degraded_reasons: Vec<BeadResolverReasonCode> = degraded.into_iter().collect();
        degraded_reasons.sort();

        for reason in &degraded_reasons {
            global_degraded.insert(*reason);
        }

        candidates.push(BeadReadyCandidate {
            id: issue.id.clone(),
            title: issue.title.clone(),
            status: issue.status,
            priority: issue.priority,
            blocker_count: blockers.len(),
            blocker_ids: blockers,
            transitive_unblock_count,
            critical_path_depth_hint,
            ready,
            degraded_reasons,
        });
    }

    candidates.sort_by_key(|c| (c.priority, c.id.clone()));
    ready_ids.sort();

    let mut degraded_reason_codes: Vec<BeadResolverReasonCode> =
        global_degraded.into_iter().collect();
    degraded_reason_codes.sort();

    BeadReadinessReport {
        candidates,
        ready_ids,
        degraded_reason_codes,
    }
}

fn compute_depth(
    issue_id: &str,
    downstream: &HashMap<String, Vec<String>>,
    memo: &mut HashMap<String, usize>,
    visiting: &mut HashSet<String>,
    cycle_seen: &mut bool,
) -> usize {
    if let Some(depth) = memo.get(issue_id) {
        return *depth;
    }

    let key = issue_id.to_string();
    if !visiting.insert(key.clone()) {
        *cycle_seen = true;
        return 0;
    }

    let children = downstream.get(issue_id).cloned().unwrap_or_default();
    let depth = if children.is_empty() {
        0
    } else {
        let mut max_child = 0usize;
        for child in children {
            max_child = max_child.max(compute_depth(
                &child, downstream, memo, visiting, cycle_seen,
            ));
        }
        1 + max_child
    };

    visiting.remove(&key);
    memo.insert(key, depth);
    depth
}

fn count_transitive_descendants(
    issue_id: &str,
    downstream: &HashMap<String, Vec<String>>,
) -> usize {
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = downstream.get(issue_id).cloned().unwrap_or_default();

    while let Some(node) = stack.pop() {
        if !seen.insert(node.clone()) {
            continue;
        }
        if let Some(children) = downstream.get(&node) {
            for child in children {
                stack.push(child.clone());
            }
        }
    }

    seen.len()
}

/// Counts of beads by status (returned by `bead_count_by_status`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BeadStatusCounts {
    pub open: usize,
    pub in_progress: usize,
    pub blocked: usize,
    pub deferred: usize,
    pub closed: usize,
}

impl BeadStatusCounts {
    /// Build counts from a list of bead summaries.
    pub fn from_summaries(beads: &[BeadSummary]) -> Self {
        let mut counts = Self::default();
        for bead in beads {
            match bead.status {
                BeadStatus::Open => counts.open += 1,
                BeadStatus::InProgress => counts.in_progress += 1,
                BeadStatus::Blocked => counts.blocked += 1,
                BeadStatus::Deferred => counts.deferred += 1,
                BeadStatus::Closed => counts.closed += 1,
            }
        }
        counts
    }

    /// Total beads across all statuses.
    #[must_use]
    pub fn total(&self) -> usize {
        self.open + self.in_progress + self.blocked + self.deferred + self.closed
    }

    /// Beads needing attention (open + in-progress).
    #[must_use]
    pub fn actionable(&self) -> usize {
        self.open + self.in_progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_bead(id: &str, status: BeadStatus, priority: u8) -> BeadSummary {
        BeadSummary {
            id: id.to_string(),
            title: format!("Bead {}", id),
            status,
            priority,
            issue_type: BeadIssueType::Task,
            assignee: None,
            labels: vec![],
            dependency_count: 0,
            dependent_count: 0,
            extra: HashMap::new(),
        }
    }

    fn sample_detail(
        id: &str,
        status: BeadStatus,
        priority: u8,
        dependency_ids: &[(&str, &str)],
    ) -> BeadIssueDetail {
        BeadIssueDetail {
            id: id.to_string(),
            title: format!("Bead {}", id),
            status,
            priority,
            issue_type: BeadIssueType::Task,
            assignee: None,
            labels: Vec::new(),
            dependencies: dependency_ids
                .iter()
                .map(|(dep_id, dep_type)| BeadDependencyRef {
                    id: (*dep_id).to_string(),
                    title: None,
                    status: None,
                    priority: None,
                    dependency_type: Some((*dep_type).to_string()),
                })
                .collect(),
            dependents: Vec::new(),
            parent: None,
            ingest_warning: None,
            extra: HashMap::new(),
        }
    }

    // -------------------------------------------------------------------------
    // BeadStatus
    // -------------------------------------------------------------------------

    #[test]
    fn test_bead_status_display() {
        assert_eq!(BeadStatus::Open.to_string(), "open");
        assert_eq!(BeadStatus::InProgress.to_string(), "in_progress");
        assert_eq!(BeadStatus::Blocked.to_string(), "blocked");
        assert_eq!(BeadStatus::Deferred.to_string(), "deferred");
        assert_eq!(BeadStatus::Closed.to_string(), "closed");
    }

    #[test]
    fn test_bead_status_serde_roundtrip() {
        for status in [
            BeadStatus::Open,
            BeadStatus::InProgress,
            BeadStatus::Blocked,
            BeadStatus::Deferred,
            BeadStatus::Closed,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: BeadStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
    }

    #[test]
    fn test_bead_status_deserialize_in_progress() {
        let status: BeadStatus = serde_json::from_str("\"in_progress\"").unwrap();
        assert_eq!(status, BeadStatus::InProgress);
    }

    // -------------------------------------------------------------------------
    // BeadPriority
    // -------------------------------------------------------------------------

    #[test]
    fn test_bead_priority_label() {
        assert_eq!(BeadPriority::P0.label(), "P0");
        assert_eq!(BeadPriority::P1.label(), "P1");
        assert_eq!(BeadPriority::P2.label(), "P2");
        assert_eq!(BeadPriority::P3.label(), "P3");
        assert_eq!(BeadPriority::P4.label(), "P4");
    }

    #[test]
    fn test_bead_priority_display() {
        assert_eq!(format!("{}", BeadPriority::P0), "P0");
        assert_eq!(format!("{}", BeadPriority(7)), "P7");
    }

    #[test]
    fn test_bead_priority_ord() {
        assert!(BeadPriority::P0 < BeadPriority::P1);
        assert!(BeadPriority::P1 < BeadPriority::P4);
    }

    #[test]
    fn test_bead_priority_serde_roundtrip() {
        let p = BeadPriority::P2;
        let json = serde_json::to_string(&p).unwrap();
        let back: BeadPriority = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    // -------------------------------------------------------------------------
    // BeadIssueType
    // -------------------------------------------------------------------------

    #[test]
    fn test_bead_issue_type_serde_roundtrip() {
        for issue_type in [
            BeadIssueType::Epic,
            BeadIssueType::Feature,
            BeadIssueType::Task,
            BeadIssueType::Bug,
        ] {
            let json = serde_json::to_string(&issue_type).unwrap();
            let back: BeadIssueType = serde_json::from_str(&json).unwrap();
            assert_eq!(back, issue_type);
        }
    }

    // -------------------------------------------------------------------------
    // BeadSummary
    // -------------------------------------------------------------------------

    #[test]
    fn test_bead_summary_bead_priority() {
        let bead = sample_bead("x", BeadStatus::Open, 2);
        assert_eq!(bead.bead_priority(), BeadPriority::P2);
    }

    #[test]
    fn test_bead_summary_is_actionable_open() {
        let bead = sample_bead("x", BeadStatus::Open, 1);
        assert!(bead.is_actionable());
    }

    #[test]
    fn test_bead_summary_is_actionable_in_progress() {
        let bead = sample_bead("x", BeadStatus::InProgress, 1);
        assert!(bead.is_actionable());
    }

    #[test]
    fn test_bead_summary_not_actionable_blocked() {
        let bead = sample_bead("x", BeadStatus::Blocked, 1);
        assert!(!bead.is_actionable());
    }

    #[test]
    fn test_bead_summary_not_actionable_closed() {
        let bead = sample_bead("x", BeadStatus::Closed, 0);
        assert!(!bead.is_actionable());
    }

    #[test]
    fn test_bead_summary_not_actionable_deferred() {
        let bead = sample_bead("x", BeadStatus::Deferred, 3);
        assert!(!bead.is_actionable());
    }

    #[test]
    fn test_bead_summary_serde_roundtrip() {
        let bead = BeadSummary {
            id: "ft-abc".to_string(),
            title: "Test bead".to_string(),
            status: BeadStatus::Open,
            priority: 1,
            issue_type: BeadIssueType::Task,
            assignee: Some("TestAgent".to_string()),
            labels: vec!["search".to_string(), "integration".to_string()],
            dependency_count: 2,
            dependent_count: 1,
            extra: HashMap::new(),
        };
        let json = serde_json::to_string(&bead).unwrap();
        let back: BeadSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "ft-abc");
        assert_eq!(back.status, BeadStatus::Open);
        assert_eq!(back.priority, 1);
        assert_eq!(back.assignee, Some("TestAgent".to_string()));
        assert_eq!(back.labels.len(), 2);
    }

    #[test]
    fn test_bead_summary_deserialize_real_br_output() {
        let json = r#"{
            "id": "ft-1u90p.7.7",
            "title": "Alt-screen conformance suite",
            "description": "Build e2e for alt-screen apps",
            "status": "in_progress",
            "priority": 0,
            "issue_type": "task",
            "assignee": "StormySnow",
            "estimated_minutes": 300,
            "created_at": "2026-02-13T00:52:30Z",
            "created_by": "jemanuel",
            "updated_at": "2026-02-20T17:23:43Z",
            "source_repo": ".",
            "compaction_level": 0,
            "original_size": 0,
            "labels": ["alt-screen", "e2e"],
            "dependency_count": 6,
            "dependent_count": 2
        }"#;
        let bead: BeadSummary = serde_json::from_str(json).unwrap();
        assert_eq!(bead.id, "ft-1u90p.7.7");
        assert_eq!(bead.status, BeadStatus::InProgress);
        assert_eq!(bead.priority, 0);
        assert_eq!(bead.assignee, Some("StormySnow".to_string()));
        assert_eq!(bead.labels, vec!["alt-screen", "e2e"]);
        // Extra fields from br output preserved in `extra`
        assert!(bead.extra.contains_key("description"));
        assert!(bead.extra.contains_key("created_at"));
    }

    #[test]
    fn test_bead_summary_deserialize_minimal() {
        let json = r#"{
            "id": "x",
            "title": "Minimal",
            "status": "open",
            "priority": 3,
            "issue_type": "bug"
        }"#;
        let bead: BeadSummary = serde_json::from_str(json).unwrap();
        assert_eq!(bead.id, "x");
        assert_eq!(bead.issue_type, BeadIssueType::Bug);
        assert!(bead.assignee.is_none());
        assert!(bead.labels.is_empty());
    }

    #[test]
    fn test_bead_summary_forward_compat_extra_fields() {
        let json = r#"{
            "id": "y",
            "title": "Future",
            "status": "open",
            "priority": 1,
            "issue_type": "task",
            "new_field_2027": "surprise"
        }"#;
        let bead: BeadSummary = serde_json::from_str(json).unwrap();
        assert_eq!(bead.extra.get("new_field_2027").unwrap(), "surprise");
    }

    // -------------------------------------------------------------------------
    // BeadStatusCounts
    // -------------------------------------------------------------------------

    #[test]
    fn test_bead_status_counts_from_summaries() {
        let beads = vec![
            sample_bead("a", BeadStatus::Open, 1),
            sample_bead("b", BeadStatus::Open, 2),
            sample_bead("c", BeadStatus::InProgress, 1),
            sample_bead("d", BeadStatus::Blocked, 1),
            sample_bead("e", BeadStatus::Closed, 0),
            sample_bead("f", BeadStatus::Closed, 0),
            sample_bead("g", BeadStatus::Closed, 0),
        ];
        let counts = BeadStatusCounts::from_summaries(&beads);
        assert_eq!(counts.open, 2);
        assert_eq!(counts.in_progress, 1);
        assert_eq!(counts.blocked, 1);
        assert_eq!(counts.closed, 3);
        assert_eq!(counts.deferred, 0);
    }

    #[test]
    fn test_bead_status_counts_total() {
        let beads = vec![
            sample_bead("a", BeadStatus::Open, 1),
            sample_bead("b", BeadStatus::Closed, 0),
        ];
        let counts = BeadStatusCounts::from_summaries(&beads);
        assert_eq!(counts.total(), 2);
    }

    #[test]
    fn test_bead_status_counts_actionable() {
        let beads = vec![
            sample_bead("a", BeadStatus::Open, 1),
            sample_bead("b", BeadStatus::InProgress, 1),
            sample_bead("c", BeadStatus::Blocked, 1),
        ];
        let counts = BeadStatusCounts::from_summaries(&beads);
        assert_eq!(counts.actionable(), 2);
    }

    #[test]
    fn test_bead_status_counts_empty() {
        let counts = BeadStatusCounts::from_summaries(&[]);
        assert_eq!(counts.total(), 0);
        assert_eq!(counts.actionable(), 0);
    }

    #[test]
    fn test_bead_status_counts_default() {
        let counts = BeadStatusCounts::default();
        assert_eq!(counts.open, 0);
        assert_eq!(counts.total(), 0);
    }

    #[test]
    fn test_bead_status_counts_serde_roundtrip() {
        let counts = BeadStatusCounts {
            open: 5,
            in_progress: 3,
            blocked: 2,
            deferred: 1,
            closed: 10,
        };
        let json = serde_json::to_string(&counts).unwrap();
        let back: BeadStatusCounts = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total(), 21);
    }

    // -------------------------------------------------------------------------
    // Readiness resolver
    // -------------------------------------------------------------------------

    #[test]
    fn beads_readiness_resolver_marks_ready_when_blockers_closed() {
        let issues = vec![
            sample_detail("dep", BeadStatus::Closed, 1, &[]),
            sample_detail("a", BeadStatus::Open, 0, &[("dep", "blocks")]),
        ];

        let report = resolve_bead_readiness(&issues);
        assert_eq!(report.ready_count(), 1);
        assert_eq!(report.ready_ids, vec!["a"]);

        let a = report.candidates.iter().find(|c| c.id == "a").unwrap();
        assert!(a.ready);
        assert_eq!(a.blocker_count, 0);
    }

    #[test]
    fn beads_readiness_resolver_honors_parent_child_non_blocking_edges() {
        let issues = vec![
            sample_detail("parent", BeadStatus::Open, 2, &[]),
            sample_detail("child", BeadStatus::Open, 1, &[("parent", "parent-child")]),
        ];

        let report = resolve_bead_readiness(&issues);
        let child = report.candidates.iter().find(|c| c.id == "child").unwrap();
        assert!(child.ready, "parent-child edge must not block readiness");
        assert_eq!(child.blocker_count, 0);
    }

    #[test]
    fn beads_readiness_resolver_counts_blockers_and_transitive_unblocks() {
        // Graph:
        //   root (open) -> mid (open) -> leaf (open)
        //   blocker (open) -> root (open)
        let issues = vec![
            sample_detail("blocker", BeadStatus::Open, 0, &[]),
            sample_detail("root", BeadStatus::Open, 1, &[("blocker", "blocks")]),
            sample_detail("mid", BeadStatus::Open, 2, &[("root", "blocks")]),
            sample_detail("leaf", BeadStatus::Open, 3, &[("mid", "blocks")]),
        ];

        let report = resolve_bead_readiness(&issues);
        let root = report.candidates.iter().find(|c| c.id == "root").unwrap();
        assert_eq!(root.blocker_count, 1);
        assert_eq!(root.blocker_ids, vec!["blocker".to_string()]);
        assert_eq!(root.transitive_unblock_count, 2); // mid + leaf
        assert_eq!(root.critical_path_depth_hint, 2);
    }

    #[test]
    fn beads_readiness_resolver_marks_missing_dependency_as_degraded() {
        let issues = vec![sample_detail(
            "a",
            BeadStatus::Open,
            0,
            &[("missing-node", "blocks")],
        )];

        let report = resolve_bead_readiness(&issues);
        let a = report.candidates.iter().find(|c| c.id == "a").unwrap();
        assert!(!a.ready);
        assert_eq!(a.blocker_count, 1);
        assert!(
            a.degraded_reasons
                .contains(&BeadResolverReasonCode::MissingDependencyNode)
        );
        assert!(
            report
                .degraded_reason_codes
                .contains(&BeadResolverReasonCode::MissingDependencyNode)
        );
    }

    #[test]
    fn beads_readiness_resolver_propagates_partial_graph_warning() {
        let mut summary = sample_bead("fallback", BeadStatus::Open, 1);
        summary.dependency_count = 2;
        let detail = BeadIssueDetail::from_summary(summary);
        let report = resolve_bead_readiness(&[detail]);
        let fallback = report
            .candidates
            .iter()
            .find(|candidate| candidate.id == "fallback")
            .unwrap();
        assert!(
            fallback
                .degraded_reasons
                .contains(&BeadResolverReasonCode::PartialGraphData)
        );
    }

    // -------------------------------------------------------------------------
    // Readiness resolver — extended coverage (ft-1i2ge.2.1)
    // -------------------------------------------------------------------------

    #[test]
    fn readiness_empty_input() {
        let report = resolve_bead_readiness(&[]);
        assert!(report.candidates.is_empty());
        assert!(report.ready_ids.is_empty());
        assert!(report.degraded_reason_codes.is_empty());
        assert_eq!(report.ready_count(), 0);
    }

    #[test]
    fn readiness_single_open_issue_is_ready() {
        let issues = vec![sample_detail("solo", BeadStatus::Open, 1, &[])];
        let report = resolve_bead_readiness(&issues);
        assert_eq!(report.ready_count(), 1);
        assert_eq!(report.ready_ids, vec!["solo"]);
        let c = &report.candidates[0];
        assert!(c.ready);
        assert_eq!(c.blocker_count, 0);
        assert!(c.blocker_ids.is_empty());
        assert_eq!(c.transitive_unblock_count, 0);
        assert_eq!(c.critical_path_depth_hint, 0);
    }

    #[test]
    fn readiness_single_in_progress_is_ready() {
        let issues = vec![sample_detail("wip", BeadStatus::InProgress, 0, &[])];
        let report = resolve_bead_readiness(&issues);
        assert_eq!(report.ready_count(), 1);
        assert_eq!(report.ready_ids, vec!["wip"]);
    }

    #[test]
    fn readiness_non_actionable_statuses_excluded_from_candidates() {
        let issues = vec![
            sample_detail("blocked", BeadStatus::Blocked, 1, &[]),
            sample_detail("deferred", BeadStatus::Deferred, 2, &[]),
            sample_detail("closed", BeadStatus::Closed, 0, &[]),
        ];
        let report = resolve_bead_readiness(&issues);
        assert!(report.candidates.is_empty());
        assert!(report.ready_ids.is_empty());
    }

    #[test]
    fn readiness_open_blocker_prevents_readiness() {
        let issues = vec![
            sample_detail("blocker", BeadStatus::Open, 0, &[]),
            sample_detail("blocked", BeadStatus::Open, 1, &[("blocker", "blocks")]),
        ];
        let report = resolve_bead_readiness(&issues);
        let blocked = report
            .candidates
            .iter()
            .find(|c| c.id == "blocked")
            .unwrap();
        assert!(!blocked.ready);
        assert_eq!(blocked.blocker_count, 1);
        assert_eq!(blocked.blocker_ids, vec!["blocker".to_string()]);
        // blocker itself is ready
        let blocker = report
            .candidates
            .iter()
            .find(|c| c.id == "blocker")
            .unwrap();
        assert!(blocker.ready);
    }

    #[test]
    fn readiness_in_progress_blocker_still_blocks() {
        let issues = vec![
            sample_detail("dep", BeadStatus::InProgress, 0, &[]),
            sample_detail("a", BeadStatus::Open, 1, &[("dep", "blocks")]),
        ];
        let report = resolve_bead_readiness(&issues);
        let a = report.candidates.iter().find(|c| c.id == "a").unwrap();
        assert!(!a.ready);
        assert_eq!(a.blocker_count, 1);
    }

    #[test]
    fn readiness_multiple_blockers_all_must_close() {
        let issues = vec![
            sample_detail("d1", BeadStatus::Closed, 0, &[]),
            sample_detail("d2", BeadStatus::Open, 0, &[]),
            sample_detail("d3", BeadStatus::Closed, 0, &[]),
            sample_detail(
                "target",
                BeadStatus::Open,
                1,
                &[("d1", "blocks"), ("d2", "blocks"), ("d3", "blocks")],
            ),
        ];
        let report = resolve_bead_readiness(&issues);
        let target = report.candidates.iter().find(|c| c.id == "target").unwrap();
        assert!(!target.ready, "d2 is still open");
        assert_eq!(target.blocker_count, 1);
        assert_eq!(target.blocker_ids, vec!["d2".to_string()]);
    }

    #[test]
    fn readiness_all_blockers_closed_means_ready() {
        let issues = vec![
            sample_detail("d1", BeadStatus::Closed, 0, &[]),
            sample_detail("d2", BeadStatus::Closed, 0, &[]),
            sample_detail(
                "target",
                BeadStatus::Open,
                1,
                &[("d1", "blocks"), ("d2", "blocks")],
            ),
        ];
        let report = resolve_bead_readiness(&issues);
        let target = report.candidates.iter().find(|c| c.id == "target").unwrap();
        assert!(target.ready);
        assert_eq!(target.blocker_count, 0);
    }

    #[test]
    fn readiness_parent_child_mixed_with_blocking_edge() {
        // parent-child should not block, but the "blocks" edge should
        let issues = vec![
            sample_detail("parent", BeadStatus::Open, 0, &[]),
            sample_detail("dep", BeadStatus::Open, 0, &[]),
            sample_detail(
                "child",
                BeadStatus::Open,
                1,
                &[("parent", "parent-child"), ("dep", "blocks")],
            ),
        ];
        let report = resolve_bead_readiness(&issues);
        let child = report.candidates.iter().find(|c| c.id == "child").unwrap();
        assert!(!child.ready, "dep blocks");
        assert_eq!(child.blocker_count, 1);
        assert_eq!(child.blocker_ids, vec!["dep".to_string()]);
    }

    #[test]
    fn readiness_diamond_dependency_graph() {
        // Diamond: A depends on B and C, both depend on D
        //   D (open) -> B (open) -> A (open)
        //   D (open) -> C (open) -> A (open)
        let issues = vec![
            sample_detail("D", BeadStatus::Open, 0, &[]),
            sample_detail("B", BeadStatus::Open, 1, &[("D", "blocks")]),
            sample_detail("C", BeadStatus::Open, 1, &[("D", "blocks")]),
            sample_detail(
                "A",
                BeadStatus::Open,
                2,
                &[("B", "blocks"), ("C", "blocks")],
            ),
        ];
        let report = resolve_bead_readiness(&issues);

        let d = report.candidates.iter().find(|c| c.id == "D").unwrap();
        assert!(d.ready);
        assert_eq!(d.transitive_unblock_count, 3); // B, C, A

        let a = report.candidates.iter().find(|c| c.id == "A").unwrap();
        assert!(!a.ready);
        assert_eq!(a.blocker_count, 2);
    }

    #[test]
    fn readiness_cycle_detected_sets_degraded_flag() {
        // A depends on B, B depends on A (cycle)
        let issues = vec![
            sample_detail("A", BeadStatus::Open, 0, &[("B", "blocks")]),
            sample_detail("B", BeadStatus::Open, 0, &[("A", "blocks")]),
        ];
        let report = resolve_bead_readiness(&issues);
        // Both blocked by each other
        for c in &report.candidates {
            assert!(!c.ready);
            assert!(
                c.degraded_reasons
                    .contains(&BeadResolverReasonCode::CyclicDependencyGraph),
                "candidate {} missing cycle degraded reason",
                c.id
            );
        }
        assert!(
            report
                .degraded_reason_codes
                .contains(&BeadResolverReasonCode::CyclicDependencyGraph)
        );
    }

    #[test]
    fn readiness_three_node_cycle() {
        // A -> B -> C -> A
        let issues = vec![
            sample_detail("A", BeadStatus::Open, 0, &[("C", "blocks")]),
            sample_detail("B", BeadStatus::Open, 0, &[("A", "blocks")]),
            sample_detail("C", BeadStatus::Open, 0, &[("B", "blocks")]),
        ];
        let report = resolve_bead_readiness(&issues);
        assert!(
            report
                .degraded_reason_codes
                .contains(&BeadResolverReasonCode::CyclicDependencyGraph)
        );
    }

    #[test]
    fn readiness_candidates_sorted_by_priority_then_id() {
        let issues = vec![
            sample_detail("z", BeadStatus::Open, 2, &[]),
            sample_detail("a", BeadStatus::Open, 2, &[]),
            sample_detail("m", BeadStatus::Open, 0, &[]),
            sample_detail("b", BeadStatus::Open, 1, &[]),
        ];
        let report = resolve_bead_readiness(&issues);
        let ids: Vec<&str> = report.candidates.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["m", "b", "a", "z"]);
    }

    #[test]
    fn readiness_ready_ids_sorted() {
        let issues = vec![
            sample_detail("z", BeadStatus::Open, 0, &[]),
            sample_detail("a", BeadStatus::Open, 0, &[]),
            sample_detail("m", BeadStatus::Open, 0, &[]),
        ];
        let report = resolve_bead_readiness(&issues);
        assert_eq!(report.ready_ids, vec!["a", "m", "z"]);
    }

    #[test]
    fn readiness_transitive_chain_depth() {
        // Linear chain: A -> B -> C -> D -> E
        let issues = vec![
            sample_detail("A", BeadStatus::Open, 0, &[]),
            sample_detail("B", BeadStatus::Open, 1, &[("A", "blocks")]),
            sample_detail("C", BeadStatus::Open, 2, &[("B", "blocks")]),
            sample_detail("D", BeadStatus::Open, 3, &[("C", "blocks")]),
            sample_detail("E", BeadStatus::Open, 4, &[("D", "blocks")]),
        ];
        let report = resolve_bead_readiness(&issues);
        let a = report.candidates.iter().find(|c| c.id == "A").unwrap();
        assert_eq!(a.critical_path_depth_hint, 4); // 4 levels deep
        assert_eq!(a.transitive_unblock_count, 4); // B, C, D, E
    }

    #[test]
    fn readiness_multiple_missing_deps() {
        let issues = vec![sample_detail(
            "a",
            BeadStatus::Open,
            0,
            &[("ghost1", "blocks"), ("ghost2", "blocks")],
        )];
        let report = resolve_bead_readiness(&issues);
        let a = &report.candidates[0];
        assert!(!a.ready);
        assert_eq!(a.blocker_count, 2);
        assert!(
            a.degraded_reasons
                .contains(&BeadResolverReasonCode::MissingDependencyNode)
        );
    }

    #[test]
    fn readiness_mixed_missing_and_present_deps() {
        let issues = vec![
            sample_detail("present", BeadStatus::Closed, 0, &[]),
            sample_detail(
                "a",
                BeadStatus::Open,
                0,
                &[("present", "blocks"), ("missing", "blocks")],
            ),
        ];
        let report = resolve_bead_readiness(&issues);
        let a = &report.candidates[0];
        assert!(!a.ready);
        assert_eq!(a.blocker_count, 1); // only "missing" blocks
        assert!(
            a.degraded_reasons
                .contains(&BeadResolverReasonCode::MissingDependencyNode)
        );
    }

    #[test]
    fn readiness_from_summary_degraded_detail() {
        let summary = sample_bead("partial", BeadStatus::Open, 1);
        let detail = BeadIssueDetail::from_summary(summary);
        assert_eq!(
            detail.ingest_warning,
            Some(BeadResolverReasonCode::PartialGraphData)
        );
        assert!(detail.dependencies.is_empty());
        assert!(detail.dependents.is_empty());
        assert!(detail.parent.is_none());
    }

    #[test]
    fn readiness_issue_detail_is_actionable() {
        assert!(sample_detail("a", BeadStatus::Open, 0, &[]).is_actionable());
        assert!(sample_detail("b", BeadStatus::InProgress, 0, &[]).is_actionable());
        assert!(!sample_detail("c", BeadStatus::Blocked, 0, &[]).is_actionable());
        assert!(!sample_detail("d", BeadStatus::Deferred, 0, &[]).is_actionable());
        assert!(!sample_detail("e", BeadStatus::Closed, 0, &[]).is_actionable());
    }

    #[test]
    fn readiness_dependency_ref_blocks_readiness() {
        let blocking = BeadDependencyRef {
            id: "x".to_string(),
            title: None,
            status: None,
            priority: None,
            dependency_type: Some("blocks".to_string()),
        };
        assert!(blocking.blocks_readiness());

        let parent_child = BeadDependencyRef {
            id: "y".to_string(),
            title: None,
            status: None,
            priority: None,
            dependency_type: Some("parent-child".to_string()),
        };
        assert!(!parent_child.blocks_readiness());

        let no_type = BeadDependencyRef {
            id: "z".to_string(),
            title: None,
            status: None,
            priority: None,
            dependency_type: None,
        };
        assert!(
            no_type.blocks_readiness(),
            "None type should block by default"
        );
    }

    #[test]
    fn readiness_report_serde_roundtrip() {
        let issues = vec![
            sample_detail("dep", BeadStatus::Closed, 0, &[]),
            sample_detail("a", BeadStatus::Open, 1, &[("dep", "blocks")]),
            sample_detail("b", BeadStatus::Open, 0, &[("missing", "blocks")]),
        ];
        let report = resolve_bead_readiness(&issues);
        let json = serde_json::to_string(&report).unwrap();
        let back: BeadReadinessReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.ready_ids, report.ready_ids);
        assert_eq!(back.candidates.len(), report.candidates.len());
        assert_eq!(back.degraded_reason_codes, report.degraded_reason_codes);
    }

    #[test]
    fn readiness_candidate_serde_roundtrip() {
        let candidate = BeadReadyCandidate {
            id: "test".to_string(),
            title: "Test Bead".to_string(),
            status: BeadStatus::Open,
            priority: 1,
            blocker_count: 2,
            blocker_ids: vec!["x".to_string(), "y".to_string()],
            transitive_unblock_count: 5,
            critical_path_depth_hint: 3,
            ready: false,
            degraded_reasons: vec![BeadResolverReasonCode::MissingDependencyNode],
        };
        let json = serde_json::to_string(&candidate).unwrap();
        let back: BeadReadyCandidate = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "test");
        assert_eq!(back.blocker_count, 2);
        assert_eq!(back.transitive_unblock_count, 5);
        assert_eq!(back.critical_path_depth_hint, 3);
        assert!(!back.ready);
        assert_eq!(back.degraded_reasons.len(), 1);
    }

    #[test]
    fn readiness_wide_fan_out_transitive_count() {
        // root -> a, b, c, d, e (5 direct children)
        let issues = vec![
            sample_detail("root", BeadStatus::Open, 0, &[]),
            sample_detail("a", BeadStatus::Open, 1, &[("root", "blocks")]),
            sample_detail("b", BeadStatus::Open, 1, &[("root", "blocks")]),
            sample_detail("c", BeadStatus::Open, 1, &[("root", "blocks")]),
            sample_detail("d", BeadStatus::Open, 1, &[("root", "blocks")]),
            sample_detail("e", BeadStatus::Open, 1, &[("root", "blocks")]),
        ];
        let report = resolve_bead_readiness(&issues);
        let root = report.candidates.iter().find(|c| c.id == "root").unwrap();
        assert!(root.ready);
        assert_eq!(root.transitive_unblock_count, 5);
        assert_eq!(root.critical_path_depth_hint, 1);
    }

    #[test]
    fn readiness_closed_issues_not_in_candidates_but_resolve_deps() {
        // dep is closed, a depends on it — a should be ready
        // dep itself should NOT appear in candidates
        let issues = vec![
            sample_detail("dep", BeadStatus::Closed, 0, &[]),
            sample_detail("a", BeadStatus::Open, 1, &[("dep", "blocks")]),
        ];
        let report = resolve_bead_readiness(&issues);
        assert_eq!(report.candidates.len(), 1);
        assert_eq!(report.candidates[0].id, "a");
        assert!(report.candidates[0].ready);
    }

    #[test]
    fn readiness_deduplicates_blocker_ids() {
        // Same dep listed twice should be deduped
        let issues = vec![
            sample_detail("dep", BeadStatus::Open, 0, &[]),
            sample_detail(
                "a",
                BeadStatus::Open,
                1,
                &[("dep", "blocks"), ("dep", "blocks")],
            ),
        ];
        let report = resolve_bead_readiness(&issues);
        let a = report.candidates.iter().find(|c| c.id == "a").unwrap();
        assert_eq!(a.blocker_count, 1);
        assert_eq!(a.blocker_ids, vec!["dep".to_string()]);
    }

    #[test]
    fn readiness_reason_code_ordering() {
        // Verify enum ordering is stable for sort (follows declaration order)
        assert!(
            BeadResolverReasonCode::MissingDependencyNode
                < BeadResolverReasonCode::CyclicDependencyGraph
        );
        assert!(
            BeadResolverReasonCode::CyclicDependencyGraph
                < BeadResolverReasonCode::PartialGraphData
        );
    }

    #[test]
    fn readiness_reason_code_serde_roundtrip() {
        for code in [
            BeadResolverReasonCode::MissingDependencyNode,
            BeadResolverReasonCode::CyclicDependencyGraph,
            BeadResolverReasonCode::PartialGraphData,
        ] {
            let json = serde_json::to_string(&code).unwrap();
            let back: BeadResolverReasonCode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, code);
        }
    }
}
