//! Vendored types for `beads_rust` (`br`) CLI integration.
//!
//! These mirror the JSON output of `br list --json` and `br show --json`
//! without depending on the `beads_rust` crate directly.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BeadIssueType {
    Epic,
    Feature,
    Task,
    Bug,
    Chore,
    Docs,
    Question,
    Test,
    #[serde(untagged)]
    Custom(String),
}

impl<'de> Deserialize<'de> for BeadIssueType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Ok(match name.as_str() {
            "epic" => Self::Epic,
            "feature" => Self::Feature,
            "task" => Self::Task,
            "bug" => Self::Bug,
            "chore" => Self::Chore,
            "docs" => Self::Docs,
            "question" => Self::Question,
            "test" => Self::Test,
            _ => Self::Custom(name),
        })
    }
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
    InvalidGraphData,
    ResourceBudgetExceeded,
    ActiveOwnership,
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
        self.dependency_type
            .as_deref()
            .is_none_or(|kind| work_graph_blocks(kind).unwrap_or(true))
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
    let edge_count = issues.iter().try_fold(0usize, |count, issue| {
        count.checked_add(issue.dependencies.len())
    });
    if issues.len() > BEAD_WORK_GRAPH_MAX_NODES
        || edge_count.is_none_or(|edges| {
            edges > BEAD_WORK_GRAPH_MAX_EDGES
                || issues
                    .len()
                    .saturating_mul(issues.len().saturating_add(edges))
                    > 20_000_000
        })
    {
        return BeadReadinessReport {
            candidates: Vec::new(),
            ready_ids: Vec::new(),
            degraded_reason_codes: vec![BeadResolverReasonCode::ResourceBudgetExceeded],
        };
    }
    let mut issue_by_id: HashMap<String, &BeadIssueDetail> = HashMap::new();
    for issue in issues {
        if issue_by_id.insert(issue.id.clone(), issue).is_some() {
            return BeadReadinessReport {
                candidates: Vec::new(),
                ready_ids: Vec::new(),
                degraded_reason_codes: vec![BeadResolverReasonCode::InvalidGraphData],
            };
        }
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

    let (depth_memo, cycle_seen) = compute_depths(&downstream);

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
        if issue.status == BeadStatus::InProgress
            || issue
                .assignee
                .as_ref()
                .is_some_and(|owner| !owner.trim().is_empty())
        {
            degraded.insert(BeadResolverReasonCode::ActiveOwnership);
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

        let ready = blockers.is_empty() && degraded.is_empty();
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

fn compute_depths(downstream: &HashMap<String, Vec<String>>) -> (HashMap<String, usize>, bool) {
    let mut remaining = HashMap::new();
    let mut parents: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut ready = VecDeque::new();
    let mut depths = HashMap::new();
    for (id, children) in downstream {
        remaining.insert(id.as_str(), children.len());
        if children.is_empty() {
            ready.push_back(id.as_str());
        }
        for child in children {
            parents.entry(child.as_str()).or_default().push(id.as_str());
        }
    }
    let mut visited = 0;
    while let Some(id) = ready.pop_front() {
        visited += 1;
        let depth = *depths.entry(id.to_string()).or_insert(0usize);
        for parent in parents.get(id).into_iter().flatten() {
            let parent_depth = depths.entry((*parent).to_string()).or_insert(0);
            *parent_depth = (*parent_depth).max(depth + 1);
            if let Some(count) = remaining.get_mut(parent) {
                *count -= 1;
                if *count == 0 {
                    ready.push_back(parent);
                }
            }
        }
    }
    (depths, visited != downstream.len())
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

pub const BEAD_WORK_GRAPH_SCHEMA_VERSION: u16 = 1;
pub const BEAD_WORK_GRAPH_MAX_BYTES: usize = 64 * 1024 * 1024;
pub const BEAD_WORK_GRAPH_MAX_LINE_BYTES: usize = 1024 * 1024;
pub const BEAD_WORK_GRAPH_MAX_NODES: usize = 8_192;
pub const BEAD_WORK_GRAPH_MAX_EDGES: usize = 65_536;
pub const BEAD_WORK_GRAPH_MAX_AGE_MS: u64 = 300_000;

/// A graph refusal never supplies an empty, apparently healthy ready queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BeadWorkSelectionError {
    #[error("unsupported Beads JSONL snapshot schema")]
    UnsupportedSchema,
    #[error("Beads snapshot SHA256 does not match the requested revision")]
    RevisionMismatch,
    #[error("Beads snapshot modification time is stale or in the future")]
    StaleSnapshot,
    #[error("Beads snapshot exceeds the byte, node, edge or scoring budget")]
    BudgetExceeded,
    #[error("Beads snapshot contains malformed or unsupported records")]
    InvalidRecord,
    #[error("Beads snapshot declares partial or count-inconsistent graph records")]
    PartialGraph,
    #[error("Beads snapshot contains duplicate node or edge identities")]
    DuplicateIdentity,
    #[error("Beads snapshot refers to a missing dependency node")]
    MissingDependency,
    #[error("Beads snapshot contains a blocking dependency cycle")]
    CyclicGraph,
    #[error("Beads snapshot has unsupported dependency semantics")]
    UnsupportedDependency,
    #[error("Beads graph scoring did not produce a converged finite result")]
    ScoringUnavailable,
    #[error("Beads snapshot is not an available regular file")]
    FileUnavailable,
    #[error("Beads snapshot changed during the bounded read")]
    ChangedDuringRead,
    #[error("bounded regular-file snapshot reading is unavailable on this platform")]
    UnsupportedPlatform,
}

/// Minimal source projection: descriptions, notes, labels and titles are neither
/// retained nor emitted by work selection. Every issue type remains in scope.
#[derive(Debug, Deserialize)]
struct WorkGraphIssue {
    id: String,
    status: BeadStatus,
    priority: u8,
    issue_type: String,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    dependencies: Vec<WorkGraphDependency>,
    #[serde(default)]
    dependency_count: Option<usize>,
    #[serde(default)]
    ingest_warning: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct WorkGraphDependency {
    issue_id: String,
    depends_on_id: String,
    #[serde(rename = "type")]
    dependency_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BeadWorkExclusion {
    Closed,
    Deferred,
    BlockedStatus,
    ActiveOwnership,
    UnfinishedDependency,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BeadWorkCandidate {
    pub id: String,
    pub status: BeadStatus,
    pub issue_type: String,
    pub priority: u8,
    /// Rounded PageRank mass, in billionths. This exact integer is the ranking
    /// component, so displayed equality also means a deterministic ID tie.
    pub pagerank_billionths: u64,
    pub blocker_ids: Vec<String>,
    pub exclusions: Vec<BeadWorkExclusion>,
}

/// Decision from all records of the supplied JSONL bytes. This is advisory
/// snapshot selection, never a live database check, ownership claim or dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BeadWorkSelection {
    pub(crate) schema_version: u16,
    pub(crate) input_format: String,
    pub(crate) input_sha256: String,
    pub(crate) source_bytes: usize,
    pub(crate) snapshot_modified_at_ms: u64,
    pub(crate) evaluated_at_ms: u64,
    pub(crate) live_database_validated: bool,
    pub(crate) node_count: usize,
    pub(crate) edge_count: usize,
    pub(crate) type_population: BTreeMap<String, usize>,
    pub(crate) status_population: BTreeMap<String, usize>,
    pub(crate) exclusion_population: BTreeMap<BeadWorkExclusion, usize>,
    pub(crate) selected_id: Option<String>,
    pub(crate) ordered_ready_ids: Vec<String>,
    pub(crate) candidates: Vec<BeadWorkCandidate>,
    pub(crate) pagerank_iterations: usize,
    pub(crate) tie_break: String,
}

impl BeadWorkSelection {
    pub fn selected_id(&self) -> Option<&str> {
        self.selected_id.as_deref()
    }
    pub fn input_sha256(&self) -> &str {
        &self.input_sha256
    }
    pub fn candidates(&self) -> &[BeadWorkCandidate] {
        &self.candidates
    }
    pub fn ordered_ready_ids(&self) -> &[String] {
        &self.ordered_ready_ids
    }
}

struct WorkScoringGraph {
    successors: Vec<Vec<usize>>,
    predecessors: Vec<Vec<usize>>,
}

impl crate::graph_scoring::GraphView for WorkScoringGraph {
    fn node_count(&self) -> usize {
        self.successors.len()
    }
    fn nodes(&self) -> Vec<usize> {
        (0..self.successors.len()).collect()
    }
    fn successors(&self, node: usize) -> Vec<usize> {
        self.successors[node].clone()
    }
    fn predecessors(&self, node: usize) -> Vec<usize> {
        self.predecessors[node].clone()
    }
}

fn work_graph_identifier(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

fn work_graph_blocks(kind: &str) -> Result<bool, BeadWorkSelectionError> {
    match kind {
        "blocks" => Ok(true),
        // This project explicitly treats hierarchy as taxonomy, not readiness.
        "parent-child" | "related" | "discovered-from" | "replies-to" | "relates-to"
        | "duplicates" | "supersedes" | "caused-by" => Ok(false),
        // Conditional/wait predicates need their own evaluation; treating them
        // as satisfied or silently dropping them would invent readiness.
        _ => Err(BeadWorkSelectionError::UnsupportedDependency),
    }
}

/// Validate and rank one complete supplied `br` JSONL snapshot. The hash binds
/// exact bytes, including record order and omitted display-only fields. The
/// five-minute check is file-age evidence only; it does not attest DB freshness.
pub fn select_bead_work_from_jsonl(
    bytes: &[u8],
    expected_sha256: &str,
    schema_version: u16,
    snapshot_modified_at_ms: u64,
    now_ms: u64,
) -> Result<BeadWorkSelection, BeadWorkSelectionError> {
    use sha2::{Digest, Sha256};
    if schema_version != BEAD_WORK_GRAPH_SCHEMA_VERSION {
        return Err(BeadWorkSelectionError::UnsupportedSchema);
    }
    if bytes.len() > BEAD_WORK_GRAPH_MAX_BYTES {
        return Err(BeadWorkSelectionError::BudgetExceeded);
    }
    if now_ms
        .checked_sub(snapshot_modified_at_ms)
        .is_none_or(|age| age > BEAD_WORK_GRAPH_MAX_AGE_MS)
    {
        return Err(BeadWorkSelectionError::StaleSnapshot);
    }
    let input_sha256 = hex::encode(Sha256::digest(bytes));
    if input_sha256 != expected_sha256 {
        return Err(BeadWorkSelectionError::RevisionMismatch);
    }
    let mut by_id = BTreeMap::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        if line.len() > BEAD_WORK_GRAPH_MAX_LINE_BYTES || by_id.len() >= BEAD_WORK_GRAPH_MAX_NODES {
            return Err(BeadWorkSelectionError::BudgetExceeded);
        }
        let issue: WorkGraphIssue =
            serde_json::from_slice(line).map_err(|_| BeadWorkSelectionError::InvalidRecord)?;
        if issue.ingest_warning.is_some()
            || issue
                .dependency_count
                .is_some_and(|count| count != issue.dependencies.len())
        {
            return Err(BeadWorkSelectionError::PartialGraph);
        }
        if !work_graph_identifier(&issue.id, 128)
            || !work_graph_identifier(&issue.issue_type, 64)
            || issue.priority > 4
            || issue
                .assignee
                .as_ref()
                .is_some_and(|owner| owner.len() > 1024)
        {
            return Err(BeadWorkSelectionError::InvalidRecord);
        }
        if by_id.insert(issue.id.clone(), issue).is_some() {
            return Err(BeadWorkSelectionError::DuplicateIdentity);
        }
    }
    let issues: Vec<_> = by_id.into_values().collect();
    let indexes: BTreeMap<_, _> = issues
        .iter()
        .enumerate()
        .map(|(index, issue)| (issue.id.as_str(), index))
        .collect();
    let mut graph = WorkScoringGraph {
        successors: vec![Vec::new(); issues.len()],
        predecessors: vec![Vec::new(); issues.len()],
    };
    let mut blockers = vec![Vec::new(); issues.len()];
    let mut edge_count = 0usize;
    for (index, issue) in issues.iter().enumerate() {
        let mut seen = BTreeSet::new();
        for dependency in &issue.dependencies {
            edge_count += 1;
            if edge_count > BEAD_WORK_GRAPH_MAX_EDGES {
                return Err(BeadWorkSelectionError::BudgetExceeded);
            }
            if dependency.issue_id != issue.id {
                return Err(BeadWorkSelectionError::InvalidRecord);
            }
            let target = *indexes
                .get(dependency.depends_on_id.as_str())
                .ok_or(BeadWorkSelectionError::MissingDependency)?;
            if !seen.insert((target, dependency.dependency_type.as_str())) {
                return Err(BeadWorkSelectionError::DuplicateIdentity);
            }
            if work_graph_blocks(&dependency.dependency_type)? {
                // Dependent -> prerequisite gives unfinished unblockers more
                // PageRank. Closed prerequisites still resolve blocker state.
                graph.successors[index].push(target);
                graph.predecessors[target].push(index);
                if issues[target].status != BeadStatus::Closed {
                    blockers[index].push(dependency.depends_on_id.clone());
                }
            }
        }
        blockers[index].sort();
    }
    // Iterative Kahn traversal covers all nodes, including closed nodes. No
    // recursive walk can overflow the stack on an admissible long chain.
    let mut incoming: Vec<_> = graph.predecessors.iter().map(Vec::len).collect();
    let mut queue: VecDeque<_> = incoming
        .iter()
        .enumerate()
        .filter_map(|(index, count)| (*count == 0).then_some(index))
        .collect();
    let mut visited = 0;
    while let Some(node) = queue.pop_front() {
        visited += 1;
        for &next in &graph.successors[node] {
            incoming[next] -= 1;
            if incoming[next] == 0 {
                queue.push_back(next);
            }
        }
    }
    if visited != issues.len() {
        return Err(BeadWorkSelectionError::CyclicGraph);
    }
    let scores =
        crate::graph_scoring::pagerank(&graph, &Default::default()).map_err(
            |error| match error {
                crate::graph_scoring::GraphScoreError::BudgetExceeded => {
                    BeadWorkSelectionError::BudgetExceeded
                }
                _ => BeadWorkSelectionError::ScoringUnavailable,
            },
        )?;
    if !scores.converged {
        return Err(BeadWorkSelectionError::ScoringUnavailable);
    }
    let mut type_population = BTreeMap::new();
    let mut status_population = BTreeMap::new();
    let mut exclusion_population = BTreeMap::new();
    let mut candidates = Vec::with_capacity(issues.len());
    for (index, issue) in issues.iter().enumerate() {
        *type_population.entry(issue.issue_type.clone()).or_insert(0) += 1;
        *status_population
            .entry(issue.status.to_string())
            .or_insert(0) += 1;
        let score = *scores
            .scores
            .get(&index)
            .ok_or(BeadWorkSelectionError::ScoringUnavailable)?;
        if !score.is_finite() || !(0.0..=1.0).contains(&score) {
            return Err(BeadWorkSelectionError::ScoringUnavailable);
        }
        let mut exclusions = Vec::new();
        match issue.status {
            BeadStatus::Closed => exclusions.push(BeadWorkExclusion::Closed),
            BeadStatus::Deferred => exclusions.push(BeadWorkExclusion::Deferred),
            BeadStatus::Blocked => exclusions.push(BeadWorkExclusion::BlockedStatus),
            BeadStatus::InProgress => exclusions.push(BeadWorkExclusion::ActiveOwnership),
            BeadStatus::Open => {}
        }
        if issue
            .assignee
            .as_ref()
            .is_some_and(|owner| !owner.trim().is_empty())
            && !exclusions.contains(&BeadWorkExclusion::ActiveOwnership)
        {
            exclusions.push(BeadWorkExclusion::ActiveOwnership);
        }
        if !blockers[index].is_empty() {
            exclusions.push(BeadWorkExclusion::UnfinishedDependency);
        }
        for reason in &exclusions {
            *exclusion_population.entry(*reason).or_insert(0) += 1;
        }
        candidates.push(BeadWorkCandidate {
            id: issue.id.clone(),
            status: issue.status,
            issue_type: issue.issue_type.clone(),
            priority: issue.priority,
            pagerank_billionths: (score * 1_000_000_000.0).round() as u64,
            blocker_ids: std::mem::take(&mut blockers[index]),
            exclusions,
        });
    }
    let mut ready: Vec<_> = candidates
        .iter()
        .filter(|candidate| candidate.exclusions.is_empty())
        .collect();
    ready.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| right.pagerank_billionths.cmp(&left.pagerank_billionths))
            .then_with(|| left.id.cmp(&right.id))
    });
    let ordered_ready_ids: Vec<_> = ready.iter().map(|candidate| candidate.id.clone()).collect();
    let selected_id = ordered_ready_ids.first().cloned();
    tracing::info!(
        schema_version,
        input_sha256,
        nodes = candidates.len(),
        edge_count,
        ready = ordered_ready_ids.len(),
        selected_id = selected_id.as_deref().unwrap_or("none"),
        live_database_validated = false,
        "Beads snapshot work selection"
    );
    Ok(BeadWorkSelection {
        schema_version,
        input_format: "br-jsonl-v1".to_string(),
        input_sha256,
        source_bytes: bytes.len(),
        snapshot_modified_at_ms,
        evaluated_at_ms: now_ms,
        live_database_validated: false,
        node_count: candidates.len(),
        edge_count,
        type_population,
        status_population,
        exclusion_population,
        selected_id,
        ordered_ready_ids,
        candidates,
        pagerank_iterations: scores.iterations,
        tie_break: "priority_ascending,pagerank_billionths_descending,id_ascending".to_string(),
    })
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

    fn work_issue(
        id: &str,
        status: &str,
        issue_type: &str,
        priority: u8,
        dependencies: &[&str],
    ) -> serde_json::Value {
        serde_json::json!({
            "id": id, "status": status, "issue_type": issue_type, "priority": priority,
            "description": "private-body-canary-must-not-be-retained",
            "dependencies": dependencies.iter().map(|dependency| serde_json::json!({
                "issue_id": id, "depends_on_id": dependency, "type": "blocks"
            })).collect::<Vec<_>>()
        })
    }

    fn work_bytes(issues: &[serde_json::Value]) -> (Vec<u8>, String) {
        use sha2::{Digest, Sha256};
        let mut bytes = Vec::new();
        for issue in issues {
            serde_json::to_writer(&mut bytes, issue).unwrap();
            bytes.push(b'\n');
        }
        let hash = hex::encode(Sha256::digest(&bytes));
        (bytes, hash)
    }

    fn select_work(
        issues: &[serde_json::Value],
    ) -> Result<BeadWorkSelection, BeadWorkSelectionError> {
        let (bytes, hash) = work_bytes(issues);
        select_bead_work_from_jsonl(&bytes, &hash, 1, 100, 100)
    }

    #[test]
    fn bead_work_selection_filters_blocked_owned_and_closed_before_scoring() {
        let mut owned = work_issue("owned", "open", "feature", 0, &[]);
        owned["assignee"] = "active-owner-canary".into();
        let issues = vec![
            work_issue("ready", "open", "test", 2, &["closed"]),
            work_issue("blocked", "blocked", "bug", 0, &[]),
            work_issue("downstream", "open", "docs", 0, &["blocked"]),
            work_issue("closed", "closed", "epic", 0, &[]),
            work_issue("deferred", "deferred", "question", 0, &[]),
            work_issue("in-progress", "in_progress", "chore", 0, &[]),
            owned,
            work_issue("custom", "blocked", "custom-review", 4, &[]),
        ];
        let report = select_work(&issues).unwrap();
        // Independent readiness oracle: the fixture has exactly one open,
        // unassigned row whose sole prerequisite is explicitly closed.
        assert_eq!(report.ordered_ready_ids, ["ready"]);
        assert_eq!(report.selected_id.as_deref(), Some("ready"));
        assert_eq!(report.candidates.len(), issues.len());
        assert_eq!(report.type_population.len(), 8);
        let candidate = |id: &str| {
            report
                .candidates
                .iter()
                .find(|candidate| candidate.id == id)
                .unwrap()
        };
        assert!(candidate("blocked").pagerank_billionths > candidate("ready").pagerank_billionths);
        assert_eq!(candidate("downstream").blocker_ids, ["blocked"]);
        assert!(
            candidate("owned")
                .exclusions
                .contains(&BeadWorkExclusion::ActiveOwnership)
        );
        assert!(
            candidate("in-progress")
                .exclusions
                .contains(&BeadWorkExclusion::ActiveOwnership)
        );
        assert!(
            candidate("closed")
                .exclusions
                .contains(&BeadWorkExclusion::Closed)
        );
        assert!(!report.live_database_validated);
        let output = serde_json::to_string(&report).unwrap();
        assert!(!output.contains("private-body-canary"));
        assert!(!output.contains("active-owner-canary"));
        println!(
            "BEAD_GRAPH_SELECTION hash={} nodes={} selected=ready blocked_high_score=true scope=snapshot_only",
            report.input_sha256, report.node_count
        );
    }

    #[test]
    fn bead_work_selection_graph_structure_changes_choice_and_order_is_deterministic() {
        let base = vec![
            work_issue("a", "open", "task", 1, &[]),
            work_issue("b", "open", "docs", 1, &[]),
        ];
        let before = select_work(&base).unwrap();
        assert_eq!(
            before.selected_id.as_deref(),
            Some("a"),
            "equal score uses the documented lexical tie"
        );
        let mut after_rows = base;
        after_rows.push(work_issue("b-dependent", "open", "test", 1, &["b"]));
        let after = select_work(&after_rows).unwrap();
        assert_eq!(
            after.selected_id.as_deref(),
            Some("b"),
            "a real graph edge changes selected work"
        );
        assert_eq!(after.ordered_ready_ids, ["b", "a"]);
        after_rows.reverse();
        let reordered = select_work(&after_rows).unwrap();
        assert_eq!(after.candidates, reordered.candidates);
        assert_eq!(after.ordered_ready_ids, reordered.ordered_ready_ids);
        assert_ne!(
            after.input_sha256, reordered.input_sha256,
            "input identity binds exact bytes, including order"
        );
        println!(
            "BEAD_GRAPH_SELECTION before={} after={} selected_before=a selected_after=b permutation_stable=true",
            before.input_sha256, after.input_sha256
        );
    }

    #[test]
    fn bead_work_selection_rejects_stale_version_cycle_missing_and_partial_input() {
        let rows = vec![work_issue("a", "open", "docs", 1, &[])];
        let (bytes, hash) = work_bytes(&rows);
        assert_eq!(
            select_bead_work_from_jsonl(&bytes, &hash, 2, 100, 100).unwrap_err(),
            BeadWorkSelectionError::UnsupportedSchema
        );
        assert_eq!(
            select_bead_work_from_jsonl(&bytes, &"0".repeat(64), 1, 100, 100).unwrap_err(),
            BeadWorkSelectionError::RevisionMismatch
        );
        for (modified, now) in [(100, 100 + BEAD_WORK_GRAPH_MAX_AGE_MS + 1), (101, 100)] {
            assert_eq!(
                select_bead_work_from_jsonl(&bytes, &hash, 1, modified, now).unwrap_err(),
                BeadWorkSelectionError::StaleSnapshot
            );
        }
        let cycle = [
            work_issue("a", "open", "task", 1, &["b"]),
            work_issue("b", "closed", "task", 1, &["a"]),
        ];
        assert_eq!(
            select_work(&cycle).unwrap_err(),
            BeadWorkSelectionError::CyclicGraph
        );
        assert_eq!(
            select_work(&[work_issue("a", "open", "task", 1, &["missing"])]).unwrap_err(),
            BeadWorkSelectionError::MissingDependency
        );
        assert_eq!(
            select_work(&[rows[0].clone(), rows[0].clone()]).unwrap_err(),
            BeadWorkSelectionError::DuplicateIdentity
        );
        let envelope = serde_json::json!({"issues": rows, "has_more": true});
        assert_eq!(
            select_work(&[envelope]).unwrap_err(),
            BeadWorkSelectionError::InvalidRecord
        );
        let mut summary = work_issue("summary", "open", "test", 0, &[]);
        summary["dependency_count"] = 1.into();
        assert_eq!(
            select_work(&[summary]).unwrap_err(),
            BeadWorkSelectionError::PartialGraph
        );
        let mut degraded = work_issue("fallback", "open", "docs", 0, &[]);
        degraded["ingest_warning"] = "partial_graph_data".into();
        assert_eq!(
            select_work(&[degraded]).unwrap_err(),
            BeadWorkSelectionError::PartialGraph
        );
        let unsupported_status = work_issue("a", "unsupported-status", "task", 1, &[]);
        assert_eq!(
            select_work(&[unsupported_status]).unwrap_err(),
            BeadWorkSelectionError::InvalidRecord
        );
        let mut conditional = work_issue("a", "open", "task", 1, &["b"]);
        conditional["dependencies"][0]["type"] = "conditional-blocks".into();
        assert_eq!(
            select_work(&[conditional, work_issue("b", "closed", "task", 1, &[])]).unwrap_err(),
            BeadWorkSelectionError::UnsupportedDependency
        );
        let invalid_utf8 = [0xff];
        use sha2::{Digest, Sha256};
        let hash = hex::encode(Sha256::digest(invalid_utf8));
        assert_eq!(
            select_bead_work_from_jsonl(&invalid_utf8, &hash, 1, 0, 0).unwrap_err(),
            BeadWorkSelectionError::InvalidRecord
        );
    }

    #[test]
    fn bead_work_selection_rejects_byte_line_and_node_overflow_without_truncation() {
        let oversized = vec![b' '; BEAD_WORK_GRAPH_MAX_BYTES + 1];
        assert_eq!(
            select_bead_work_from_jsonl(&oversized, "", 1, 0, 0).unwrap_err(),
            BeadWorkSelectionError::BudgetExceeded
        );
        drop(oversized);
        use sha2::{Digest, Sha256};
        let oversized_line = vec![b' '; BEAD_WORK_GRAPH_MAX_LINE_BYTES + 1];
        let hash = hex::encode(Sha256::digest(&oversized_line));
        assert_eq!(
            select_bead_work_from_jsonl(&oversized_line, &hash, 1, 0, 0).unwrap_err(),
            BeadWorkSelectionError::BudgetExceeded
        );
        let nodes: Vec<_> = (0..=BEAD_WORK_GRAPH_MAX_NODES)
            .map(|index| work_issue(&format!("n-{index}"), "open", "test", 2, &[]))
            .collect();
        assert_eq!(
            select_work(&nodes).unwrap_err(),
            BeadWorkSelectionError::BudgetExceeded
        );
        let clean = select_work(&[work_issue("after-refusal", "open", "test", 2, &[])]).unwrap();
        assert_eq!(clean.selected_id.as_deref(), Some("after-refusal"));
    }

    #[test]
    fn readiness_partial_duplicate_owned_and_deep_inputs_fail_closed_or_settle_iteratively() {
        let partial = BeadIssueDetail::from_summary(sample_bead("partial", BeadStatus::Open, 0));
        assert!(resolve_bead_readiness(&[partial]).ready_ids.is_empty());
        let mut owned = sample_detail("owned", BeadStatus::Open, 0, &[]);
        owned.assignee = Some("current-owner".to_string());
        assert!(resolve_bead_readiness(&[owned]).ready_ids.is_empty());
        let duplicate = sample_detail("duplicate", BeadStatus::Open, 0, &[]);
        let report = resolve_bead_readiness(&[duplicate.clone(), duplicate]);
        assert!(report.ready_ids.is_empty());
        assert_eq!(
            report.degraded_reason_codes,
            [BeadResolverReasonCode::InvalidGraphData]
        );
        let chain: Vec<_> = (0..1_000)
            .map(|index| {
                let mut issue = sample_detail(&format!("n-{index}"), BeadStatus::Open, 2, &[]);
                if index > 0 {
                    issue.dependencies.push(BeadDependencyRef {
                        id: format!("n-{}", index - 1),
                        title: None,
                        status: None,
                        priority: None,
                        dependency_type: Some("blocks".to_string()),
                    });
                }
                issue
            })
            .collect();
        let report = resolve_bead_readiness(&chain);
        assert_eq!(report.ready_ids, ["n-0"]);
        assert_eq!(
            report
                .candidates
                .iter()
                .find(|candidate| candidate.id == "n-0")
                .unwrap()
                .critical_path_depth_hint,
            999
        );
    }

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
            BeadIssueType::Chore,
            BeadIssueType::Docs,
            BeadIssueType::Question,
            BeadIssueType::Test,
            BeadIssueType::Custom("verification-special".to_string()),
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
        assert_eq!(bead.labels, [] as [std::string::String; 0]);
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
        assert_eq!(report.ready_ids, [] as [std::string::String; 0]);
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
        assert_eq!(c.blocker_ids, [] as [std::string::String; 0]);
        assert_eq!(c.transitive_unblock_count, 0);
        assert_eq!(c.critical_path_depth_hint, 0);
    }

    #[test]
    fn readiness_single_in_progress_retains_owner_exclusion() {
        let issues = vec![sample_detail("wip", BeadStatus::InProgress, 0, &[])];
        let report = resolve_bead_readiness(&issues);
        assert_eq!(report.ready_count(), 0);
        assert!(report.ready_ids.is_empty());
        assert!(
            report.candidates[0]
                .degraded_reasons
                .contains(&BeadResolverReasonCode::ActiveOwnership)
        );
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
        assert_eq!(report.ready_ids, [] as [std::string::String; 0]);
    }

    #[test]
    fn readiness_open_blocker_prevents_readiness() {
        let issues = vec![
            sample_detail("blocker", BeadStatus::Open, 0, &[]),
            sample_detail("blocked", BeadStatus::Open, 1, &[("blocker", "blocks")]),
        ];
        let report = resolve_bead_readiness(&issues);
        let blocked_candidate = report
            .candidates
            .iter()
            .find(|c| c.id == "blocked")
            .unwrap();
        assert!(!blocked_candidate.ready);
        assert_eq!(blocked_candidate.blocker_count, 1);
        assert_eq!(blocked_candidate.blocker_ids, vec!["blocker".to_string()]);
        // blocker itself is ready
        let blocker_entry = report
            .candidates
            .iter()
            .find(|c| c.id == "blocker")
            .unwrap();
        assert!(blocker_entry.ready);
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

        for relation in ["related", "relates-to", "discovered-from"] {
            let mut nonblocking = parent_child.clone();
            nonblocking.dependency_type = Some(relation.to_string());
            assert!(
                !nonblocking.blocks_readiness(),
                "descriptive relation {relation} must not block ready work"
            );
        }

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
