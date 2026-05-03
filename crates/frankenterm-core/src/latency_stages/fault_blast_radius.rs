use super::FaultDomain;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};

/// Edge in the domain dependency graph: `source` faults can cascade to `target`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DomainDependency {
    /// Source domain whose failure can cascade.
    pub source: FaultDomain,
    /// Target domain affected by the source failure.
    pub target: FaultDomain,
    /// Cascade probability (0.0 = never, 1.0 = always). Deterministic: used
    /// as a threshold, not a random draw.
    pub cascade_weight: u32, // out of 100
}

/// Result of a blast-radius analysis for a given source fault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlastRadiusReport {
    /// The domain that originally faulted.
    pub origin: FaultDomain,
    /// Domains directly at risk (one hop).
    pub direct_risk: Vec<FaultDomain>,
    /// Domains transitively at risk (multi-hop reachability).
    pub transitive_risk: Vec<FaultDomain>,
    /// Total domains at risk (direct + transitive, deduplicated).
    pub total_at_risk: usize,
}

/// Blast-radius analyzer using a static dependency graph.
pub struct BlastRadiusAnalyzer {
    edges: Vec<DomainDependency>,
}

impl BlastRadiusAnalyzer {
    /// Create with explicit dependency edges.
    pub fn new(edges: Vec<DomainDependency>) -> Self {
        Self { edges }
    }

    /// Create with the default dependency graph.
    /// Default edges: Io -> Storage (IO failures often cascade to storage),
    /// Scheduler -> Budget (scheduling failures break budget tracking),
    /// Recovery -> Scheduler (recovery failures leave scheduler in bad state).
    pub fn default_graph() -> Self {
        Self::new(vec![
            DomainDependency {
                source: FaultDomain::Io,
                target: FaultDomain::Storage,
                cascade_weight: 80,
            },
            DomainDependency {
                source: FaultDomain::Scheduler,
                target: FaultDomain::Budget,
                cascade_weight: 60,
            },
            DomainDependency {
                source: FaultDomain::Recovery,
                target: FaultDomain::Scheduler,
                cascade_weight: 40,
            },
        ])
    }

    /// Analyze blast radius from a source domain failure.
    pub fn analyze(&self, origin: FaultDomain) -> BlastRadiusReport {
        let direct_risk: Vec<FaultDomain> = self
            .edges
            .iter()
            .filter(|e| e.source == origin)
            .map(|e| e.target)
            .collect();

        // BFS for transitive reachability (excluding origin and direct).
        let mut visited = HashSet::new();
        visited.insert(origin);
        for d in &direct_risk {
            visited.insert(*d);
        }
        let mut queue: VecDeque<FaultDomain> = direct_risk.iter().copied().collect();
        let mut transitive = Vec::new();
        while let Some(current) = queue.pop_front() {
            for edge in &self.edges {
                if edge.source == current && !visited.contains(&edge.target) {
                    visited.insert(edge.target);
                    transitive.push(edge.target);
                    queue.push_back(edge.target);
                }
            }
        }

        let total_at_risk = direct_risk.len() + transitive.len();
        BlastRadiusReport {
            origin,
            direct_risk,
            transitive_risk: transitive,
            total_at_risk,
        }
    }

    /// Edges in the dependency graph.
    pub fn edges(&self) -> &[DomainDependency] {
        &self.edges
    }

    /// Add an edge.
    pub fn add_edge(&mut self, edge: DomainDependency) {
        self.edges.push(edge);
    }

    /// All domains reachable from origin (direct + transitive), sorted.
    pub fn reachable_from(&self, origin: FaultDomain) -> Vec<FaultDomain> {
        let report = self.analyze(origin);
        let mut all: Vec<FaultDomain> = report
            .direct_risk
            .into_iter()
            .chain(report.transitive_risk)
            .collect();
        all.sort_by_key(|d| *d as u8);
        all.dedup();
        all
    }
}
