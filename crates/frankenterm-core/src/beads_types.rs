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

    let mut degraded_reason_codes: Vec<BeadResolverReasonCode> = global_degraded.into_iter().collect();
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
            max_child = max_child.max(compute_depth(&child, downstream, memo, visiting, cycle_seen));
        }
        1 + max_child
    };

    visiting.remove(&key);
    memo.insert(key, depth);
    depth
}

fn count_transitive_descendants(issue_id: &str, downstream: &HashMap<String, Vec<String>>) -> usize {
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
        assert!(a
            .degraded_reasons
            .contains(&BeadResolverReasonCode::MissingDependencyNode));
        assert!(report
            .degraded_reason_codes
            .contains(&BeadResolverReasonCode::MissingDependencyNode));
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
        assert!(fallback
            .degraded_reasons
            .contains(&BeadResolverReasonCode::PartialGraphData));
    }
}
