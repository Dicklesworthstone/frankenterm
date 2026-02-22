//! Vendored types for `beads_rust` (`br`) CLI integration.
//!
//! These mirror the JSON output of `br list --json` and `br show --json`
//! without depending on the `beads_rust` crate directly.

use std::collections::HashMap;

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
}
