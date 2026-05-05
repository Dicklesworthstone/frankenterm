//! Runtime mitigation policy and recovery state for latency enforcement.

use serde::{Deserialize, Serialize};
use std::fmt;

use super::{LatencyStage, Mitigation};

/// Mitigation ladder with ordered escalation levels.
///
/// The ladder defines a strict partial order of increasingly aggressive
/// mitigation actions. The enforcer escalates monotonically (never
/// de-escalates within a single stage evaluation).
///
/// # Ladder ordering (least to most aggressive):
/// ```text
/// None(0) -> Defer(1) -> Degrade(2) -> Shed(3) -> Skip(4)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MitigationLevel {
    /// No mitigation needed.
    None = 0,
    /// Defer to next cycle.
    Defer = 1,
    /// Degrade quality.
    Degrade = 2,
    /// Shed load.
    Shed = 3,
    /// Skip entirely.
    Skip = 4,
}

impl MitigationLevel {
    /// Convert from Mitigation enum.
    pub fn from_mitigation(m: Mitigation) -> Self {
        match m {
            Mitigation::None => Self::None,
            Mitigation::Defer => Self::Defer,
            Mitigation::Degrade => Self::Degrade,
            Mitigation::Shed => Self::Shed,
            Mitigation::Skip => Self::Skip,
        }
    }

    /// Convert back to Mitigation enum.
    pub fn to_mitigation(self) -> Mitigation {
        match self {
            Self::None => Mitigation::None,
            Self::Defer => Mitigation::Defer,
            Self::Degrade => Mitigation::Degrade,
            Self::Shed => Mitigation::Shed,
            Self::Skip => Mitigation::Skip,
        }
    }

    /// All levels in escalation order.
    pub const ALL: &[Self] = &[
        Self::None,
        Self::Defer,
        Self::Degrade,
        Self::Shed,
        Self::Skip,
    ];

    /// Numeric severity (0-4).
    pub fn severity(self) -> u8 {
        self as u8
    }
}

impl fmt::Display for MitigationLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("NONE"),
            Self::Defer => f.write_str("DEFER"),
            Self::Degrade => f.write_str("DEGRADE"),
            Self::Shed => f.write_str("SHED"),
            Self::Skip => f.write_str("SKIP"),
        }
    }
}

/// Policy constraint that limits which mitigations can be applied to a stage.
///
/// # Safety Contract
/// Some stages are critical and must never be skipped. Others can tolerate
/// degradation but not shedding. PolicyConstraint makes these rules explicit
/// and machine-enforceable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyConstraint {
    /// Stage this policy applies to.
    pub stage: LatencyStage,
    /// Maximum allowed mitigation level.
    pub max_level: MitigationLevel,
    /// Whether this stage is critical (violations generate alerts).
    pub critical: bool,
    /// Minimum observations before enforcement kicks in (warmup).
    pub warmup_count: u64,
}

impl PolicyConstraint {
    /// Check if a proposed mitigation level is allowed.
    pub fn allows(&self, level: MitigationLevel) -> bool {
        level <= self.max_level
    }

    /// Clamp a proposed level to the maximum allowed.
    pub fn clamp(&self, level: MitigationLevel) -> MitigationLevel {
        if level <= self.max_level {
            level
        } else {
            self.max_level
        }
    }
}

/// Default policy constraints for all pipeline stages.
pub fn default_policy_constraints() -> Vec<PolicyConstraint> {
    vec![
        PolicyConstraint {
            stage: LatencyStage::PtyCapture,
            max_level: MitigationLevel::Shed,
            critical: true,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::DeltaExtraction,
            max_level: MitigationLevel::Degrade,
            critical: false,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::StorageWrite,
            max_level: MitigationLevel::Defer,
            critical: true,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::PatternDetection,
            max_level: MitigationLevel::Skip,
            critical: false,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::EventEmission,
            max_level: MitigationLevel::Defer,
            critical: true,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::WorkflowDispatch,
            max_level: MitigationLevel::Skip,
            critical: false,
            warmup_count: 5,
        },
        PolicyConstraint {
            stage: LatencyStage::ActionExecution,
            max_level: MitigationLevel::Shed,
            critical: false,
            warmup_count: 10,
        },
        PolicyConstraint {
            stage: LatencyStage::ApiResponse,
            max_level: MitigationLevel::Defer,
            critical: true,
            warmup_count: 10,
        },
    ]
}

/// Recovery protocol for stepping back from degraded to full quality.
///
/// After mitigation is applied, the system should recover once latency
/// returns to acceptable levels. RecoveryProtocol defines how quickly
/// and under what conditions recovery occurs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryProtocol {
    /// Number of consecutive within-budget observations before de-escalating.
    pub cooldown_observations: u64,
    /// Maximum time in degraded state before forced recovery attempt (microseconds).
    pub max_degraded_duration_us: u64,
    /// Whether to step down one level at a time or jump to full.
    pub gradual: bool,
}

impl Default for RecoveryProtocol {
    fn default() -> Self {
        Self {
            cooldown_observations: 20,
            max_degraded_duration_us: 30_000_000, // 30 seconds
            gradual: true,
        }
    }
}

/// Per-stage enforcement state tracking mitigation and recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageEnforcementState {
    /// Current active mitigation level for this stage.
    pub current_level: MitigationLevel,
    /// Consecutive within-budget observations since last overflow.
    pub consecutive_ok: u64,
    /// Timestamp of last escalation (epoch microseconds, 0 if never escalated).
    pub last_escalation_us: u64,
    /// Total escalation count.
    pub escalation_count: u64,
    /// Total recovery count.
    pub recovery_count: u64,
}

impl StageEnforcementState {
    pub(super) fn new() -> Self {
        Self {
            current_level: MitigationLevel::None,
            consecutive_ok: 0,
            last_escalation_us: 0,
            escalation_count: 0,
            recovery_count: 0,
        }
    }
}
