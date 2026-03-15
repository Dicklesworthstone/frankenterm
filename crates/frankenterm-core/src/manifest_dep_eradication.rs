//! Workspace-wide manifest alignment plan for runtime dependency eradication (ft-e34d9.10.8.1).
//!
//! This module codifies the workspace-wide manifest alignment plan for removing
//! forbidden runtime dependencies (tokio, smol, async-io, async-executor) and
//! provides programmatic audit types for tracking progress.
//!
//! # Architecture
//!
//! ```text
//! ManifestFinding        — a forbidden dep in a specific Cargo.toml
//!   ├── DepSection       — which dependency table it lives in
//!   └── DepCondition     — whether it is gated or unconditional
//!
//! EradicationStep        — a single action to remove one finding
//!   ├── EradicationAction — what to do (Remove, FeatureGate, …)
//!   └── ManifestFinding   — the finding being addressed
//!
//! EradicationPlan        — the full workspace plan (~18 steps)
//!   ├── standard()        — factory for the current workspace state
//!   └── progress methods  — total/completed/critical_remaining/by_crate/…
//!
//! FeatureAlignment       — check that feature flag pairs exist and are correct
//!
//! AlignmentReport        — assembled report combining plan + alignments + surface status
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::dependency_eradication::{ForbiddenRuntime, SurfaceContractStatus, ViolationSeverity};

// =============================================================================
// DepSection
// =============================================================================

/// Which dependency table a forbidden dependency was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DepSection {
    /// `[dependencies]`
    Dependencies,
    /// `[dev-dependencies]`
    DevDependencies,
    /// `[build-dependencies]`
    BuildDependencies,
    /// `[target.'cfg(...)'.dependencies]`
    TargetDependencies,
}

// =============================================================================
// DepCondition
// =============================================================================

/// Whether a forbidden dependency is unconditional or sits behind a guard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DepCondition {
    /// The dependency is listed without any feature gate or platform predicate.
    Unconditional,
    /// The dependency is only active when the named feature is enabled.
    FeatureGated(String),
    /// The dependency is gated behind a `cfg(…)` target expression.
    PlatformConditional(String),
    /// The dependency is a default-on feature that can be disabled.
    DefaultFeature(String),
}

// =============================================================================
// ManifestFinding
// =============================================================================

/// A forbidden dependency finding in a specific `Cargo.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestFinding {
    /// Short crate name (e.g. `"frankenterm-core"`).
    pub crate_name: String,
    /// Workspace-relative path to the `Cargo.toml` (e.g. `"crates/frankenterm-core/Cargo.toml"`).
    pub manifest_path: String,
    /// Name of the forbidden dependency as it appears in `Cargo.toml`.
    pub dep_name: String,
    /// Which forbidden runtime this dependency belongs to.
    pub runtime: ForbiddenRuntime,
    /// Which dependency table it was found in.
    pub section: DepSection,
    /// Whether the dependency is conditional.
    pub condition: DepCondition,
    /// Cargo features explicitly enabled for this dependency.
    pub features_enabled: Vec<String>,
    /// Severity of this finding.
    pub severity: ViolationSeverity,
}

// =============================================================================
// EradicationAction
// =============================================================================

/// The action required to remove a specific `ManifestFinding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EradicationAction {
    /// Delete the dependency entry entirely.
    Remove,
    /// Move the dependency behind an opt-in feature flag.
    FeatureGate,
    /// Replace the dependency with the `asupersync` equivalent.
    MigrateToAsupersync,
    /// Keep the dependency only in `[dev-dependencies]`.
    MoveToDevOnly,
    /// Accepted in a vendored crate — tracked but not blocked.
    AcceptAsVendored,
}

// =============================================================================
// EradicationStep
// =============================================================================

/// A single step in the workspace eradication plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EradicationStep {
    /// The manifest finding this step addresses.
    pub finding: ManifestFinding,
    /// The action to take.
    pub action: EradicationAction,
    /// Rationale for choosing this action.
    pub rationale: String,
    /// If the action involves migration, the target feature flag (e.g. `"async-asupersync"`).
    pub migration_feature: Option<String>,
    /// Whether this step has been completed.
    pub completed: bool,
}

// =============================================================================
// EradicationPlan
// =============================================================================

/// Workspace-wide plan for eradicating forbidden runtime dependencies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EradicationPlan {
    /// Stable identifier for this plan (e.g. `"ft-e34d9.10.8.1"`).
    pub plan_id: String,
    /// Unix timestamp (ms) when the plan was generated.
    pub generated_at_ms: u64,
    /// Ordered list of eradication steps.
    pub steps: Vec<EradicationStep>,
}

impl EradicationPlan {
    /// Create a new, empty plan.
    #[must_use]
    pub fn new(plan_id: &str, generated_at_ms: u64) -> Self {
        Self {
            plan_id: plan_id.to_string(),
            generated_at_ms,
            steps: Vec::new(),
        }
    }

    /// Append a step to the plan.
    pub fn add_step(&mut self, step: EradicationStep) {
        self.steps.push(step);
    }

    /// Build the standard plan based on the current workspace audit.
    ///
    /// Contains ~18 steps covering all workspace crates that carry forbidden
    /// runtime dependencies (tokio, smol, async-io, async-executor).
    #[must_use]
    pub fn standard() -> Self {
        let mut plan = Self::new("ft-e34d9.10.8.1", 0);

        // -----------------------------------------------------------------
        // frankenterm-core: tokio runtime dep (Critical → feature gate)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "frankenterm-core".into(),
                manifest_path: "crates/frankenterm-core/Cargo.toml".into(),
                dep_name: "tokio".into(),
                runtime: ForbiddenRuntime::Tokio,
                section: DepSection::Dependencies,
                condition: DepCondition::Unconditional,
                features_enabled: vec!["rt".into(), "rt-multi-thread".into(), "macros".into()],
                severity: ViolationSeverity::Critical,
            },
            action: EradicationAction::FeatureGate,
            rationale: "Core runtime dep must move behind opt-in tokio-compat feature to avoid \
                        blocking asupersync migration for downstream consumers."
                .into(),
            migration_feature: Some("tokio-compat".into()),
            completed: false,
        });

        // -----------------------------------------------------------------
        // frankenterm-core: tokio dev-dep (Info → accept as vendored)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "frankenterm-core".into(),
                manifest_path: "crates/frankenterm-core/Cargo.toml".into(),
                dep_name: "tokio".into(),
                runtime: ForbiddenRuntime::Tokio,
                section: DepSection::DevDependencies,
                condition: DepCondition::Unconditional,
                features_enabled: vec!["rt".into(), "macros".into()],
                severity: ViolationSeverity::Info,
            },
            action: EradicationAction::AcceptAsVendored,
            rationale: "Test infrastructure requires tokio for legacy #[tokio::test] suites; \
                        accepted until async test migration (ft-22x4r) completes."
                .into(),
            migration_feature: None,
            completed: false,
        });

        // -----------------------------------------------------------------
        // frankenterm-core: criterion async_tokio dev-dep (Info → accept)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "frankenterm-core".into(),
                manifest_path: "crates/frankenterm-core/Cargo.toml".into(),
                dep_name: "tokio".into(),
                runtime: ForbiddenRuntime::Tokio,
                section: DepSection::DevDependencies,
                condition: DepCondition::FeatureGated("async_tokio".into()),
                features_enabled: vec!["rt".into()],
                severity: ViolationSeverity::Info,
            },
            action: EradicationAction::AcceptAsVendored,
            rationale: "Criterion bench harness pulls tokio via async_tokio feature; \
                        accepted for bench-only use."
                .into(),
            migration_feature: None,
            completed: false,
        });

        // -----------------------------------------------------------------
        // crates/frankenterm (ft binary): tokio dev-dep (Info → keep dev-only)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "frankenterm".into(),
                manifest_path: "crates/frankenterm/Cargo.toml".into(),
                dep_name: "tokio".into(),
                runtime: ForbiddenRuntime::Tokio,
                section: DepSection::DevDependencies,
                condition: DepCondition::Unconditional,
                features_enabled: vec!["rt".into(), "macros".into()],
                severity: ViolationSeverity::Info,
            },
            action: EradicationAction::MoveToDevOnly,
            rationale: "Already in dev-dependencies; confirmed dev-only, no production impact."
                .into(),
            migration_feature: None,
            completed: false,
        });

        // -----------------------------------------------------------------
        // frankenterm-gui: smol dep (Error → migrate to asupersync)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "frankenterm-gui".into(),
                manifest_path: "crates/frankenterm-gui/Cargo.toml".into(),
                dep_name: "smol".into(),
                runtime: ForbiddenRuntime::Smol,
                section: DepSection::Dependencies,
                condition: DepCondition::Unconditional,
                features_enabled: vec![],
                severity: ViolationSeverity::Error,
            },
            action: EradicationAction::MigrateToAsupersync,
            rationale: "GUI event loop uses smol for async; migrate to asupersync block_on.".into(),
            migration_feature: Some("async-asupersync".into()),
            completed: false,
        });

        // -----------------------------------------------------------------
        // frankenterm-mux-server-impl: async-io dep (Error → migrate)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "frankenterm-mux-server-impl".into(),
                manifest_path: "crates/frankenterm-mux-server-impl/Cargo.toml".into(),
                dep_name: "async-io".into(),
                runtime: ForbiddenRuntime::AsyncIo,
                section: DepSection::Dependencies,
                condition: DepCondition::Unconditional,
                features_enabled: vec![],
                severity: ViolationSeverity::Error,
            },
            action: EradicationAction::MigrateToAsupersync,
            rationale: "Mux server uses async-io reactor; replace with asupersync I/O driver."
                .into(),
            migration_feature: Some("async-asupersync".into()),
            completed: false,
        });

        // -----------------------------------------------------------------
        // frankenterm-mux-server-impl: smol dep (Error → migrate)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "frankenterm-mux-server-impl".into(),
                manifest_path: "crates/frankenterm-mux-server-impl/Cargo.toml".into(),
                dep_name: "smol".into(),
                runtime: ForbiddenRuntime::Smol,
                section: DepSection::Dependencies,
                condition: DepCondition::Unconditional,
                features_enabled: vec![],
                severity: ViolationSeverity::Error,
            },
            action: EradicationAction::MigrateToAsupersync,
            rationale: "Mux server uses smol executor; replace with asupersync runtime.".into(),
            migration_feature: Some("async-asupersync".into()),
            completed: false,
        });

        // -----------------------------------------------------------------
        // frankenterm/promise: async-executor dep (Critical → feature gate)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "promise".into(),
                manifest_path: "promise/Cargo.toml".into(),
                dep_name: "async-executor".into(),
                runtime: ForbiddenRuntime::AsyncExecutor,
                section: DepSection::Dependencies,
                condition: DepCondition::Unconditional,
                features_enabled: vec![],
                severity: ViolationSeverity::Critical,
            },
            action: EradicationAction::FeatureGate,
            rationale: "Promise crate uses async-executor for poll-based futures; \
                        gate behind async-legacy feature to allow asupersync-only builds."
                .into(),
            migration_feature: Some("async-legacy".into()),
            completed: false,
        });

        // -----------------------------------------------------------------
        // frankenterm/promise: async-io dep (Critical → feature gate)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "promise".into(),
                manifest_path: "promise/Cargo.toml".into(),
                dep_name: "async-io".into(),
                runtime: ForbiddenRuntime::AsyncIo,
                section: DepSection::Dependencies,
                condition: DepCondition::Unconditional,
                features_enabled: vec![],
                severity: ViolationSeverity::Critical,
            },
            action: EradicationAction::FeatureGate,
            rationale: "Promise crate uses async-io timer primitives; \
                        gate behind async-legacy feature."
                .into(),
            migration_feature: Some("async-legacy".into()),
            completed: false,
        });

        // -----------------------------------------------------------------
        // frankenterm/async_ossl: async-io dep (Warning → flip default)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "async_ossl".into(),
                manifest_path: "async_ossl/Cargo.toml".into(),
                dep_name: "async-io".into(),
                runtime: ForbiddenRuntime::AsyncIo,
                section: DepSection::Dependencies,
                condition: DepCondition::DefaultFeature("async-io-support".into()),
                features_enabled: vec![],
                severity: ViolationSeverity::Warning,
            },
            action: EradicationAction::FeatureGate,
            rationale: "Already feature-gated but default=on; flip default to off so \
                        asupersync-only builds exclude async-io without extra configuration."
                .into(),
            migration_feature: Some("async-io-support".into()),
            completed: false,
        });

        // -----------------------------------------------------------------
        // frankenterm/uds: async-io dep (Warning → flip default)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "uds".into(),
                manifest_path: "uds/Cargo.toml".into(),
                dep_name: "async-io".into(),
                runtime: ForbiddenRuntime::AsyncIo,
                section: DepSection::Dependencies,
                condition: DepCondition::DefaultFeature("async-io-backend".into()),
                features_enabled: vec![],
                severity: ViolationSeverity::Warning,
            },
            action: EradicationAction::FeatureGate,
            rationale: "UDS crate gated behind default-on feature; flip default to off.".into(),
            migration_feature: Some("async-io-backend".into()),
            completed: false,
        });

        // -----------------------------------------------------------------
        // frankenterm/codec: smol dep (Warning → flip default)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "codec".into(),
                manifest_path: "codec/Cargo.toml".into(),
                dep_name: "smol".into(),
                runtime: ForbiddenRuntime::Smol,
                section: DepSection::Dependencies,
                condition: DepCondition::DefaultFeature("smol-compat".into()),
                features_enabled: vec![],
                severity: ViolationSeverity::Warning,
            },
            action: EradicationAction::FeatureGate,
            rationale: "Codec crate gated behind default-on smol-compat; flip default to off."
                .into(),
            migration_feature: Some("smol-compat".into()),
            completed: false,
        });

        // -----------------------------------------------------------------
        // frankenterm/config: smol dep (Error → gate)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "config".into(),
                manifest_path: "config/Cargo.toml".into(),
                dep_name: "smol".into(),
                runtime: ForbiddenRuntime::Smol,
                section: DepSection::Dependencies,
                condition: DepCondition::Unconditional,
                features_enabled: vec![],
                severity: ViolationSeverity::Error,
            },
            action: EradicationAction::FeatureGate,
            rationale: "Config crate unconditionally depends on smol for async file loading; \
                        needs feature gate before asupersync migration."
                .into(),
            migration_feature: Some("smol-compat".into()),
            completed: false,
        });

        // -----------------------------------------------------------------
        // frankenterm/client: async-io dep (Error → migrate)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "client".into(),
                manifest_path: "client/Cargo.toml".into(),
                dep_name: "async-io".into(),
                runtime: ForbiddenRuntime::AsyncIo,
                section: DepSection::Dependencies,
                condition: DepCondition::Unconditional,
                features_enabled: vec![],
                severity: ViolationSeverity::Error,
            },
            action: EradicationAction::MigrateToAsupersync,
            rationale: "Client crate uses async-io for connection polling; \
                        migrate to asupersync async I/O."
                .into(),
            migration_feature: Some("async-asupersync".into()),
            completed: false,
        });

        // -----------------------------------------------------------------
        // frankenterm/client: smol dep (Error → migrate)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "client".into(),
                manifest_path: "client/Cargo.toml".into(),
                dep_name: "smol".into(),
                runtime: ForbiddenRuntime::Smol,
                section: DepSection::Dependencies,
                condition: DepCondition::Unconditional,
                features_enabled: vec![],
                severity: ViolationSeverity::Error,
            },
            action: EradicationAction::MigrateToAsupersync,
            rationale: "Client crate uses smol for async task scheduling; \
                        replace with asupersync spawn."
                .into(),
            migration_feature: Some("async-asupersync".into()),
            completed: false,
        });

        // -----------------------------------------------------------------
        // frankenterm/window: async-io dep (Error → migrate)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "window".into(),
                manifest_path: "window/Cargo.toml".into(),
                dep_name: "async-io".into(),
                runtime: ForbiddenRuntime::AsyncIo,
                section: DepSection::Dependencies,
                condition: DepCondition::Unconditional,
                features_enabled: vec![],
                severity: ViolationSeverity::Error,
            },
            action: EradicationAction::MigrateToAsupersync,
            rationale: "Window crate uses async-io event loop; migrate to asupersync runtime."
                .into(),
            migration_feature: Some("async-asupersync".into()),
            completed: false,
        });

        // -----------------------------------------------------------------
        // frankenterm/ssh: smol dep (Warning → flip default)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "ssh".into(),
                manifest_path: "ssh/Cargo.toml".into(),
                dep_name: "smol".into(),
                runtime: ForbiddenRuntime::Smol,
                section: DepSection::Dependencies,
                condition: DepCondition::DefaultFeature("smol-backend".into()),
                features_enabled: vec![],
                severity: ViolationSeverity::Warning,
            },
            action: EradicationAction::FeatureGate,
            rationale: "SSH crate already gated behind default-on feature; flip default to off."
                .into(),
            migration_feature: Some("smol-backend".into()),
            completed: false,
        });

        // -----------------------------------------------------------------
        // frankenterm/mux-lua: smol dep (Error → migrate)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "mux-lua".into(),
                manifest_path: "mux-lua/Cargo.toml".into(),
                dep_name: "smol".into(),
                runtime: ForbiddenRuntime::Smol,
                section: DepSection::Dependencies,
                condition: DepCondition::Unconditional,
                features_enabled: vec![],
                severity: ViolationSeverity::Error,
            },
            action: EradicationAction::MigrateToAsupersync,
            rationale: "Lua integration layer uses smol for async coroutine bridging; \
                        migrate to asupersync."
                .into(),
            migration_feature: Some("async-asupersync".into()),
            completed: false,
        });

        // -----------------------------------------------------------------
        // frankenterm/toast-notification: async-io target dep (Info → accept)
        // -----------------------------------------------------------------
        plan.add_step(EradicationStep {
            finding: ManifestFinding {
                crate_name: "toast-notification".into(),
                manifest_path: "toast-notification/Cargo.toml".into(),
                dep_name: "async-io".into(),
                runtime: ForbiddenRuntime::AsyncIo,
                section: DepSection::TargetDependencies,
                condition: DepCondition::PlatformConditional("cfg(target_os = \"linux\")".into()),
                features_enabled: vec![],
                severity: ViolationSeverity::Info,
            },
            action: EradicationAction::AcceptAsVendored,
            rationale: "Linux-only async-io dep for D-Bus notification; \
                        accepted as platform-native vendored path."
                .into(),
            migration_feature: None,
            completed: false,
        });

        plan
    }

    /// Total number of steps.
    #[must_use]
    pub fn total_steps(&self) -> usize {
        self.steps.len()
    }

    /// Number of completed steps.
    #[must_use]
    pub fn completed_steps(&self) -> usize {
        self.steps.iter().filter(|s| s.completed).count()
    }

    /// Progress as a percentage (0.0–100.0). Returns 0.0 for an empty plan.
    #[must_use]
    pub fn progress_pct(&self) -> f64 {
        if self.steps.is_empty() {
            return 0.0;
        }
        (self.completed_steps() as f64 / self.total_steps() as f64) * 100.0
    }

    /// Steps that are critical severity and not yet completed.
    #[must_use]
    pub fn critical_remaining(&self) -> Vec<&EradicationStep> {
        self.steps
            .iter()
            .filter(|s| !s.completed && s.finding.severity == ViolationSeverity::Critical)
            .collect()
    }

    /// Steps grouped by crate name.
    #[must_use]
    pub fn by_crate(&self) -> BTreeMap<String, Vec<&EradicationStep>> {
        let mut map: BTreeMap<String, Vec<&EradicationStep>> = BTreeMap::new();
        for step in &self.steps {
            map.entry(step.finding.crate_name.clone())
                .or_default()
                .push(step);
        }
        map
    }

    /// Steps grouped by action (using `Debug` representation of `EradicationAction` as key).
    #[must_use]
    pub fn by_action(&self) -> BTreeMap<String, Vec<&EradicationStep>> {
        let mut map: BTreeMap<String, Vec<&EradicationStep>> = BTreeMap::new();
        for step in &self.steps {
            map.entry(format!("{:?}", step.action))
                .or_default()
                .push(step);
        }
        map
    }

    /// Count of findings per forbidden runtime (using the runtime's label as key).
    #[must_use]
    pub fn findings_by_runtime(&self) -> BTreeMap<String, usize> {
        let mut map: BTreeMap<String, usize> = BTreeMap::new();
        for step in &self.steps {
            *map.entry(step.finding.runtime.label().to_string())
                .or_insert(0) += 1;
        }
        map
    }
}

// =============================================================================
// FeatureAlignment
// =============================================================================

/// Checks whether a crate has the expected feature flag pair for migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureAlignment {
    /// Crate name.
    pub crate_name: String,
    /// The legacy feature that enables the forbidden runtime (e.g. `"async-smol"`).
    pub legacy_feature: String,
    /// The migration feature that switches to asupersync (e.g. `"async-asupersync"`).
    pub migration_feature: String,
    /// Whether the legacy feature exists in the manifest.
    pub legacy_exists: bool,
    /// Whether the migration feature exists in the manifest.
    pub migration_exists: bool,
    /// Whether the default feature set includes the legacy feature.
    pub default_is_legacy: bool,
    /// `true` when both features exist and the migration feature is ready.
    pub aligned: bool,
}

/// Returns the standard set of feature-alignment checks for the workspace.
///
/// Covers the 7 crates that have (or should have) feature flag pairs for the
/// legacy → asupersync migration.
#[must_use]
pub fn standard_feature_alignments() -> Vec<FeatureAlignment> {
    vec![
        FeatureAlignment {
            crate_name: "codec".into(),
            legacy_feature: "smol-compat".into(),
            migration_feature: "async-asupersync".into(),
            legacy_exists: true,
            migration_exists: false,
            default_is_legacy: true,
            aligned: false,
        },
        FeatureAlignment {
            crate_name: "ssh".into(),
            legacy_feature: "smol-backend".into(),
            migration_feature: "async-asupersync".into(),
            legacy_exists: true,
            migration_exists: false,
            default_is_legacy: true,
            aligned: false,
        },
        FeatureAlignment {
            crate_name: "config".into(),
            legacy_feature: "smol-compat".into(),
            migration_feature: "async-asupersync".into(),
            legacy_exists: false,
            migration_exists: false,
            default_is_legacy: false,
            aligned: false,
        },
        FeatureAlignment {
            crate_name: "promise".into(),
            legacy_feature: "async-legacy".into(),
            migration_feature: "async-asupersync".into(),
            legacy_exists: false,
            migration_exists: false,
            default_is_legacy: false,
            aligned: false,
        },
        FeatureAlignment {
            crate_name: "uds".into(),
            legacy_feature: "async-io-backend".into(),
            migration_feature: "async-asupersync".into(),
            legacy_exists: true,
            migration_exists: false,
            default_is_legacy: true,
            aligned: false,
        },
        FeatureAlignment {
            crate_name: "async_ossl".into(),
            legacy_feature: "async-io-support".into(),
            migration_feature: "async-asupersync".into(),
            legacy_exists: true,
            migration_exists: false,
            default_is_legacy: true,
            aligned: false,
        },
        FeatureAlignment {
            crate_name: "mux".into(),
            legacy_feature: "smol-runtime".into(),
            migration_feature: "async-asupersync".into(),
            legacy_exists: false,
            migration_exists: false,
            default_is_legacy: false,
            aligned: false,
        },
    ]
}

// =============================================================================
// AlignmentReport
// =============================================================================

/// Assembled manifest-alignment report for the workspace eradication effort.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlignmentReport {
    /// Stable report identifier.
    pub report_id: String,
    /// Unix timestamp (ms) when the report was generated.
    pub generated_at_ms: u64,
    /// The full eradication plan.
    pub plan: EradicationPlan,
    /// Feature alignment checks.
    pub feature_alignments: Vec<FeatureAlignment>,
    /// Runtime-compat surface contract disposition.
    pub surface_status: SurfaceContractStatus,
    /// `true` when `readiness_score >= 0.8`.
    pub overall_aligned: bool,
    /// Weighted readiness score (0.0–1.0).
    pub readiness_score: f64,
}

impl AlignmentReport {
    /// Create a new, unfinalised report.
    #[must_use]
    pub fn new(report_id: &str, generated_at_ms: u64) -> Self {
        Self {
            report_id: report_id.to_string(),
            generated_at_ms,
            plan: EradicationPlan::new(report_id, generated_at_ms),
            feature_alignments: Vec::new(),
            surface_status: SurfaceContractStatus {
                keep_count: 0,
                replace_count: 0,
                retire_count: 0,
                replaced_count: 0,
                retired_count: 0,
            },
            overall_aligned: false,
            readiness_score: 0.0,
        }
    }

    /// Set the eradication plan.
    pub fn set_plan(&mut self, plan: EradicationPlan) {
        self.plan = plan;
    }

    /// Set the feature alignment checks.
    pub fn set_feature_alignments(&mut self, alignments: Vec<FeatureAlignment>) {
        self.feature_alignments = alignments;
    }

    /// Set the surface contract status.
    pub fn set_surface_status(&mut self, status: SurfaceContractStatus) {
        self.surface_status = status;
    }

    /// Compute `readiness_score` and `overall_aligned`.
    ///
    /// Score formula (weighted):
    /// ```text
    /// 0.4 * (completed_steps / total_steps)
    /// + 0.3 * (aligned_features / total_features)
    /// + 0.3 * (1.0 if all transitional resolved else 0.0)
    /// ```
    /// `overall_aligned` is `true` when `readiness_score >= 0.8`.
    pub fn finalize(&mut self) {
        let step_ratio = if self.plan.total_steps() == 0 {
            1.0_f64
        } else {
            self.plan.completed_steps() as f64 / self.plan.total_steps() as f64
        };

        let feature_ratio = if self.feature_alignments.is_empty() {
            1.0_f64
        } else {
            let aligned = self.feature_alignments.iter().filter(|a| a.aligned).count();
            aligned as f64 / self.feature_alignments.len() as f64
        };

        let surface_ratio = if self.surface_status.all_transitional_resolved() {
            1.0_f64
        } else {
            0.0_f64
        };

        self.readiness_score = 0.4_f64.mul_add(
            step_ratio,
            0.3_f64.mul_add(feature_ratio, 0.3 * surface_ratio),
        );
        self.overall_aligned = self.readiness_score >= 0.8;
    }

    /// Render a human-readable one-paragraph summary.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "AlignmentReport[{}]: plan {}/{} steps complete ({:.1}%), \
             {}/{} features aligned, surface transitional resolved: {}, \
             readiness {:.2} — {}.",
            self.report_id,
            self.plan.completed_steps(),
            self.plan.total_steps(),
            self.plan.progress_pct(),
            self.feature_alignments.iter().filter(|a| a.aligned).count(),
            self.feature_alignments.len(),
            self.surface_status.all_transitional_resolved(),
            self.readiness_score,
            if self.overall_aligned {
                "ALIGNED"
            } else {
                "NOT ALIGNED"
            },
        )
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // 1. dep_section_variants
    // -------------------------------------------------------------------------
    #[test]
    fn dep_section_variants() {
        let variants = [
            DepSection::Dependencies,
            DepSection::DevDependencies,
            DepSection::BuildDependencies,
            DepSection::TargetDependencies,
        ];
        for (i, a) in variants.iter().enumerate() {
            for (j, b) in variants.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b);
                }
            }
        }
    }

    // -------------------------------------------------------------------------
    // 2. dep_condition_variants
    // -------------------------------------------------------------------------
    #[test]
    fn dep_condition_variants() {
        let unconditional = DepCondition::Unconditional;
        let gated = DepCondition::FeatureGated("mcp".into());
        let platform = DepCondition::PlatformConditional("cfg(unix)".into());
        let default = DepCondition::DefaultFeature("smol-compat".into());

        assert_ne!(unconditional, gated);
        assert_ne!(gated, platform);
        assert_ne!(platform, default);
        assert_eq!(
            DepCondition::FeatureGated("mcp".into()),
            DepCondition::FeatureGated("mcp".into())
        );
    }

    // -------------------------------------------------------------------------
    // 3. manifest_finding_serde_roundtrip
    // -------------------------------------------------------------------------
    #[test]
    fn manifest_finding_serde_roundtrip() {
        let finding = ManifestFinding {
            crate_name: "frankenterm-core".into(),
            manifest_path: "crates/frankenterm-core/Cargo.toml".into(),
            dep_name: "tokio".into(),
            runtime: ForbiddenRuntime::Tokio,
            section: DepSection::Dependencies,
            condition: DepCondition::FeatureGated("tokio-compat".into()),
            features_enabled: vec!["rt".into(), "macros".into()],
            severity: ViolationSeverity::Critical,
        };

        let json = serde_json::to_string(&finding).expect("serialize");
        let restored: ManifestFinding = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.crate_name, finding.crate_name);
        assert_eq!(restored.dep_name, finding.dep_name);
        assert_eq!(restored.runtime, finding.runtime);
        assert_eq!(restored.section, finding.section);
        assert_eq!(restored.severity, finding.severity);
        assert_eq!(restored.features_enabled, finding.features_enabled);
    }

    // -------------------------------------------------------------------------
    // 4. eradication_action_variants
    // -------------------------------------------------------------------------
    #[test]
    fn eradication_action_variants() {
        let actions = [
            EradicationAction::Remove,
            EradicationAction::FeatureGate,
            EradicationAction::MigrateToAsupersync,
            EradicationAction::MoveToDevOnly,
            EradicationAction::AcceptAsVendored,
        ];
        for (i, a) in actions.iter().enumerate() {
            for (j, b) in actions.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b);
                }
            }
        }
    }

    // -------------------------------------------------------------------------
    // 5. standard_plan_step_count
    // -------------------------------------------------------------------------
    #[test]
    fn standard_plan_step_count() {
        let plan = EradicationPlan::standard();
        // Exactly 19 steps defined in standard()
        assert!(
            plan.total_steps() >= 18,
            "expected at least 18 steps, got {}",
            plan.total_steps()
        );
    }

    // -------------------------------------------------------------------------
    // 6. standard_plan_covers_all_runtimes
    // -------------------------------------------------------------------------
    #[test]
    fn standard_plan_covers_all_runtimes() {
        let plan = EradicationPlan::standard();
        let runtimes = plan.findings_by_runtime();
        assert!(runtimes.contains_key("tokio"), "plan missing tokio entries");
        assert!(runtimes.contains_key("smol"), "plan missing smol entries");
        assert!(
            runtimes.contains_key("async-io"),
            "plan missing async-io entries"
        );
        assert!(
            runtimes.contains_key("async-executor"),
            "plan missing async-executor entries"
        );
    }

    // -------------------------------------------------------------------------
    // 7. standard_plan_has_critical_items
    // -------------------------------------------------------------------------
    #[test]
    fn standard_plan_has_critical_items() {
        let plan = EradicationPlan::standard();
        let critical_count = plan
            .steps
            .iter()
            .filter(|s| s.finding.severity == ViolationSeverity::Critical)
            .count();
        assert!(
            critical_count >= 3,
            "expected at least 3 critical findings, got {}",
            critical_count
        );
    }

    // -------------------------------------------------------------------------
    // 8. standard_plan_progress_starts_at_zero
    // -------------------------------------------------------------------------
    #[test]
    fn standard_plan_progress_starts_at_zero() {
        let plan = EradicationPlan::standard();
        assert_eq!(plan.completed_steps(), 0);
        assert_eq!(plan.progress_pct(), 0.0);
    }

    // -------------------------------------------------------------------------
    // 9. step_completion_updates_progress
    // -------------------------------------------------------------------------
    #[test]
    fn step_completion_updates_progress() {
        let mut plan = EradicationPlan::standard();
        let total = plan.total_steps();
        assert!(total > 0);

        plan.steps[0].completed = true;
        assert_eq!(plan.completed_steps(), 1);

        let expected_pct = 1.0 / total as f64 * 100.0;
        let diff = (plan.progress_pct() - expected_pct).abs();
        assert!(diff < 1e-9, "progress_pct mismatch: {}", diff);
    }

    // -------------------------------------------------------------------------
    // 10. critical_remaining_filters_correctly
    // -------------------------------------------------------------------------
    #[test]
    fn critical_remaining_filters_correctly() {
        let mut plan = EradicationPlan::standard();

        // Complete all critical steps
        for step in plan.steps.iter_mut() {
            if step.finding.severity == ViolationSeverity::Critical {
                step.completed = true;
            }
        }

        let remaining = plan.critical_remaining();
        assert!(
            remaining.is_empty(),
            "expected no critical remaining, got {}",
            remaining.len()
        );
    }

    // -------------------------------------------------------------------------
    // 11. by_crate_groups_correctly
    // -------------------------------------------------------------------------
    #[test]
    fn by_crate_groups_correctly() {
        let plan = EradicationPlan::standard();
        let by_crate = plan.by_crate();

        // frankenterm-core has 3 steps
        let core_steps = by_crate
            .get("frankenterm-core")
            .expect("frankenterm-core missing");
        assert!(
            core_steps.len() >= 2,
            "expected >=2 frankenterm-core steps, got {}",
            core_steps.len()
        );

        // All crate names in the map must be non-empty
        for name in by_crate.keys() {
            assert!(!name.is_empty());
        }
    }

    // -------------------------------------------------------------------------
    // 12. by_action_groups_correctly
    // -------------------------------------------------------------------------
    #[test]
    fn by_action_groups_correctly() {
        let plan = EradicationPlan::standard();
        let by_action = plan.by_action();

        // Must have MigrateToAsupersync entries
        assert!(
            by_action.contains_key("MigrateToAsupersync"),
            "expected MigrateToAsupersync key"
        );
        // Must have AcceptAsVendored entries
        assert!(
            by_action.contains_key("AcceptAsVendored"),
            "expected AcceptAsVendored key"
        );
        // Must have FeatureGate entries
        assert!(
            by_action.contains_key("FeatureGate"),
            "expected FeatureGate key"
        );
    }

    // -------------------------------------------------------------------------
    // 13. findings_by_runtime_counts
    // -------------------------------------------------------------------------
    #[test]
    fn findings_by_runtime_counts() {
        let plan = EradicationPlan::standard();
        let by_runtime = plan.findings_by_runtime();

        // Each runtime should have at least 1 entry
        assert!(*by_runtime.get("tokio").unwrap_or(&0) >= 1);
        assert!(*by_runtime.get("smol").unwrap_or(&0) >= 1);
        assert!(*by_runtime.get("async-io").unwrap_or(&0) >= 1);
        assert!(*by_runtime.get("async-executor").unwrap_or(&0) >= 1);

        // Sum of all counts should equal total_steps
        let total: usize = by_runtime.values().sum();
        assert_eq!(total, plan.total_steps());
    }

    // -------------------------------------------------------------------------
    // 14. feature_alignment_standard_set
    // -------------------------------------------------------------------------
    #[test]
    fn feature_alignment_standard_set() {
        let alignments = standard_feature_alignments();
        // Exactly 7 crates defined
        assert_eq!(alignments.len(), 7, "expected 7 alignment checks");

        for a in &alignments {
            assert!(!a.crate_name.is_empty());
            assert!(!a.legacy_feature.is_empty());
            assert!(!a.migration_feature.is_empty());
        }
    }

    // -------------------------------------------------------------------------
    // 15. feature_alignment_has_migration_pairs
    // -------------------------------------------------------------------------
    #[test]
    fn feature_alignment_has_migration_pairs() {
        let alignments = standard_feature_alignments();
        let crate_names: Vec<&str> = alignments.iter().map(|a| a.crate_name.as_str()).collect();

        // All expected crates are present
        for expected in &[
            "codec",
            "ssh",
            "config",
            "promise",
            "uds",
            "async_ossl",
            "mux",
        ] {
            assert!(
                crate_names.contains(expected),
                "missing alignment for crate: {}",
                expected
            );
        }

        // None of the standard alignments should be aligned yet (baseline state)
        for a in &alignments {
            assert!(
                !a.aligned,
                "crate {} should not be aligned in baseline state",
                a.crate_name
            );
        }
    }

    // -------------------------------------------------------------------------
    // 16. alignment_report_finalize_score
    // -------------------------------------------------------------------------
    #[test]
    fn alignment_report_finalize_score() {
        let mut report = AlignmentReport::new("test-report", 0);
        // Empty plan, empty alignments, zero surface: finalize should yield
        // score = 0.4*1.0 + 0.3*1.0 + 0.3*1.0 = 1.0 (empty cases default to 1.0)
        // because all_transitional_resolved() is true when all counts are 0.
        report.finalize();
        assert!(
            (report.readiness_score - 1.0).abs() < 1e-9,
            "empty report readiness should be 1.0, got {}",
            report.readiness_score
        );
        assert!(report.overall_aligned);
    }

    // -------------------------------------------------------------------------
    // 17. alignment_report_fully_aligned
    // -------------------------------------------------------------------------
    #[test]
    fn alignment_report_fully_aligned() {
        let mut report = AlignmentReport::new("full-aligned", 0);

        // Plan with all steps complete
        let mut plan = EradicationPlan::standard();
        for step in plan.steps.iter_mut() {
            step.completed = true;
        }
        report.set_plan(plan);

        // All features aligned
        let mut alignments = standard_feature_alignments();
        for a in alignments.iter_mut() {
            a.aligned = true;
            a.legacy_exists = true;
            a.migration_exists = true;
        }
        report.set_feature_alignments(alignments);

        // Surface fully resolved
        report.set_surface_status(SurfaceContractStatus {
            keep_count: 5,
            replace_count: 3,
            retire_count: 2,
            replaced_count: 3,
            retired_count: 2,
        });

        report.finalize();
        assert!(
            (report.readiness_score - 1.0).abs() < 1e-9,
            "fully-aligned score should be 1.0, got {}",
            report.readiness_score
        );
        assert!(report.overall_aligned);
    }

    // -------------------------------------------------------------------------
    // 18. alignment_report_partially_aligned
    // -------------------------------------------------------------------------
    #[test]
    fn alignment_report_partially_aligned() {
        let mut report = AlignmentReport::new("partial", 0);

        // Zero steps complete
        let plan = EradicationPlan::standard();
        report.set_plan(plan);

        // Zero features aligned
        report.set_feature_alignments(standard_feature_alignments());

        // Surface not resolved
        report.set_surface_status(SurfaceContractStatus {
            keep_count: 5,
            replace_count: 3,
            retire_count: 2,
            replaced_count: 0,
            retired_count: 0,
        });

        report.finalize();

        // score = 0.4*0.0 + 0.3*0.0 + 0.3*0.0 = 0.0
        assert!(
            report.readiness_score < 0.8,
            "partially aligned score should be <0.8, got {}",
            report.readiness_score
        );
        assert!(!report.overall_aligned);
    }

    // -------------------------------------------------------------------------
    // 19. alignment_report_summary_format
    // -------------------------------------------------------------------------
    #[test]
    fn alignment_report_summary_format() {
        let mut report = AlignmentReport::new("summary-test", 0);
        report.set_plan(EradicationPlan::standard());
        report.set_feature_alignments(standard_feature_alignments());
        report.set_surface_status(SurfaceContractStatus {
            keep_count: 10,
            replace_count: 5,
            retire_count: 3,
            replaced_count: 0,
            retired_count: 0,
        });
        report.finalize();

        let summary = report.summary();
        assert!(
            summary.contains("summary-test"),
            "report_id missing from summary"
        );
        assert!(summary.contains("steps complete"), "step info missing");
        assert!(
            summary.contains("NOT ALIGNED") || summary.contains("ALIGNED"),
            "alignment verdict missing"
        );
        assert!(summary.contains("readiness"), "readiness score missing");
    }

    // -------------------------------------------------------------------------
    // 20. eradication_step_with_migration_feature
    // -------------------------------------------------------------------------
    #[test]
    fn eradication_step_with_migration_feature() {
        let step = EradicationStep {
            finding: ManifestFinding {
                crate_name: "promise".into(),
                manifest_path: "promise/Cargo.toml".into(),
                dep_name: "async-executor".into(),
                runtime: ForbiddenRuntime::AsyncExecutor,
                section: DepSection::Dependencies,
                condition: DepCondition::Unconditional,
                features_enabled: vec![],
                severity: ViolationSeverity::Critical,
            },
            action: EradicationAction::FeatureGate,
            rationale: "Gate behind async-legacy.".into(),
            migration_feature: Some("async-legacy".into()),
            completed: false,
        };

        assert_eq!(step.migration_feature.as_deref(), Some("async-legacy"));
        assert!(!step.completed);
        assert_eq!(step.action, EradicationAction::FeatureGate);
        assert_eq!(step.finding.runtime, ForbiddenRuntime::AsyncExecutor);

        // Verify serde roundtrip preserves migration_feature
        let json = serde_json::to_string(&step).expect("serialize");
        let restored: EradicationStep = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.migration_feature, Some("async-legacy".into()));
        assert_eq!(restored.finding.crate_name, "promise");
    }
}
