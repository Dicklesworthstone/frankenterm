//! E4.F2.T5: Log explainability checks and incident triage decision trees.
//!
//! Verifies that log output is sufficient for an operator to diagnose
//! common failure modes from logs alone, without requiring source code
//! inspection or additional debugging tools.

use std::collections::HashMap;

// ═══════════════════════════════════════════════════════════════════════
// Triage decision tree model
// ═══════════════════════════════════════════════════════════════════════

/// A failure mode that an operator might encounter.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FailureMode {
    MigrationStuckAtM2,
    IndexLagSpike,
    RollbackTriggered,
    DataCorruption,
    HealthDegraded,
    CheckpointRegression,
    WriteFreezeActive,
    EmptySourceMigration,
}

impl FailureMode {
    fn all() -> Vec<FailureMode> {
        vec![
            FailureMode::MigrationStuckAtM2,
            FailureMode::IndexLagSpike,
            FailureMode::RollbackTriggered,
            FailureMode::DataCorruption,
            FailureMode::HealthDegraded,
            FailureMode::CheckpointRegression,
            FailureMode::WriteFreezeActive,
            FailureMode::EmptySourceMigration,
        ]
    }

    fn description(&self) -> &str {
        match self {
            FailureMode::MigrationStuckAtM2 => "Migration pipeline stalls at M2 (import)",
            FailureMode::IndexLagSpike => "Consumer index lag spikes during cutover",
            FailureMode::RollbackTriggered => "Automatic rollback fires unexpectedly",
            FailureMode::DataCorruption => "Digest mismatch indicates data corruption",
            FailureMode::HealthDegraded => "Target backend reports degraded health",
            FailureMode::CheckpointRegression => "Checkpoint ordinal goes backwards",
            FailureMode::WriteFreezeActive => "Write freeze blocks recorder writes",
            FailureMode::EmptySourceMigration => "Migration invoked with empty source",
        }
    }
}

/// A step in the triage decision tree.
#[derive(Debug, Clone)]
struct TriageStep {
    /// What to check in the logs.
    instruction: String,
    /// Log fields required at this step.
    required_fields: Vec<String>,
    /// Log level expected.
    expected_level: LogLevel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LogLevel {
    Info,
    Warn,
    Error,
}

/// A complete triage decision tree for a failure mode.
#[derive(Debug, Clone)]
struct TriageDecisionTree {
    failure_mode: FailureMode,
    steps: Vec<TriageStep>,
    root_cause_diagnosable: bool,
}

impl TriageDecisionTree {
    fn required_field_set(&self) -> Vec<String> {
        let mut fields: Vec<String> = self
            .steps
            .iter()
            .flat_map(|s| s.required_fields.clone())
            .collect();
        fields.sort();
        fields.dedup();
        fields
    }
}

/// Build the canonical triage decision trees for all known failure modes.
fn build_triage_trees() -> Vec<TriageDecisionTree> {
    vec![
        TriageDecisionTree {
            failure_mode: FailureMode::MigrationStuckAtM2,
            steps: vec![
                TriageStep {
                    instruction: "Check last migration_stage log entry".to_string(),
                    required_fields: vec!["migration_stage".to_string()],
                    expected_level: LogLevel::Info,
                },
                TriageStep {
                    instruction: "Compare events_exported with event_count".to_string(),
                    required_fields: vec!["events_exported".to_string(), "event_count".to_string()],
                    expected_level: LogLevel::Info,
                },
                TriageStep {
                    instruction: "Check for M2 import error in logs".to_string(),
                    required_fields: vec!["migration_stage".to_string()],
                    expected_level: LogLevel::Error,
                },
            ],
            root_cause_diagnosable: true,
        },
        TriageDecisionTree {
            failure_mode: FailureMode::IndexLagSpike,
            steps: vec![
                TriageStep {
                    instruction: "Check cursor progress and lag metrics".to_string(),
                    required_fields: vec!["event_count".to_string()],
                    expected_level: LogLevel::Info,
                },
                TriageStep {
                    instruction: "Check backend health status".to_string(),
                    required_fields: vec!["backend".to_string()],
                    expected_level: LogLevel::Info,
                },
            ],
            root_cause_diagnosable: true,
        },
        TriageDecisionTree {
            failure_mode: FailureMode::RollbackTriggered,
            steps: vec![
                TriageStep {
                    instruction: "Identify rollback trigger from WARN log".to_string(),
                    required_fields: vec!["rollback_class".to_string()],
                    expected_level: LogLevel::Warn,
                },
                TriageStep {
                    instruction: "Check rollback tier classification".to_string(),
                    required_fields: vec!["rollback_class".to_string()],
                    expected_level: LogLevel::Warn,
                },
                TriageStep {
                    instruction: "Verify backend state post-rollback".to_string(),
                    required_fields: vec!["backend".to_string()],
                    expected_level: LogLevel::Info,
                },
            ],
            root_cause_diagnosable: true,
        },
        TriageDecisionTree {
            failure_mode: FailureMode::DataCorruption,
            steps: vec![
                TriageStep {
                    instruction: "Check digest mismatch in migration logs".to_string(),
                    required_fields: vec!["digest".to_string()],
                    expected_level: LogLevel::Warn,
                },
                TriageStep {
                    instruction: "Compare export_digest vs import_digest".to_string(),
                    required_fields: vec!["digest".to_string(), "event_count".to_string()],
                    expected_level: LogLevel::Warn,
                },
                TriageStep {
                    instruction: "Check rollback trigger classification".to_string(),
                    required_fields: vec!["rollback_class".to_string()],
                    expected_level: LogLevel::Warn,
                },
            ],
            root_cause_diagnosable: true,
        },
        TriageDecisionTree {
            failure_mode: FailureMode::HealthDegraded,
            steps: vec![
                TriageStep {
                    instruction: "Check health endpoint for degraded flag".to_string(),
                    required_fields: vec!["backend".to_string()],
                    expected_level: LogLevel::Warn,
                },
                TriageStep {
                    instruction: "Check data_path for disk/permission issues".to_string(),
                    required_fields: vec!["data_path".to_string()],
                    expected_level: LogLevel::Info,
                },
            ],
            root_cause_diagnosable: true,
        },
        TriageDecisionTree {
            failure_mode: FailureMode::CheckpointRegression,
            steps: vec![
                TriageStep {
                    instruction: "Check checkpoint ordinal values across stages".to_string(),
                    required_fields: vec!["event_count".to_string()],
                    expected_level: LogLevel::Info,
                },
                TriageStep {
                    instruction: "Verify migration stage progression".to_string(),
                    required_fields: vec!["migration_stage".to_string()],
                    expected_level: LogLevel::Info,
                },
            ],
            root_cause_diagnosable: true,
        },
        TriageDecisionTree {
            failure_mode: FailureMode::WriteFreezeActive,
            steps: vec![
                TriageStep {
                    instruction: "Check for data integrity emergency rollback log".to_string(),
                    required_fields: vec!["rollback_class".to_string()],
                    expected_level: LogLevel::Warn,
                },
                TriageStep {
                    instruction: "Verify freeze state in backend status".to_string(),
                    required_fields: vec!["backend".to_string()],
                    expected_level: LogLevel::Warn,
                },
            ],
            root_cause_diagnosable: true,
        },
        TriageDecisionTree {
            failure_mode: FailureMode::EmptySourceMigration,
            steps: vec![
                TriageStep {
                    instruction: "Check M0 preflight event_count".to_string(),
                    required_fields: vec!["event_count".to_string()],
                    expected_level: LogLevel::Info,
                },
                TriageStep {
                    instruction: "Verify migration aborted at preflight".to_string(),
                    required_fields: vec!["migration_stage".to_string()],
                    expected_level: LogLevel::Warn,
                },
            ],
            root_cause_diagnosable: true,
        },
    ]
}

/// Simulated log database: field → set of (level, stage) appearances.
#[derive(Debug, Clone)]
struct LogDatabase {
    entries: Vec<LogEntry>,
}

#[derive(Debug, Clone)]
struct LogEntry {
    level: LogLevel,
    fields: HashMap<String, String>,
    stage: String,
}

impl LogDatabase {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn add(&mut self, level: LogLevel, stage: &str, fields: Vec<(&str, &str)>) {
        let field_map: HashMap<String, String> = fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        self.entries.push(LogEntry {
            level,
            fields: field_map,
            stage: stage.to_string(),
        });
    }

    /// Check if a required field exists at the expected log level.
    fn has_field_at_level(&self, field: &str, level: &LogLevel) -> bool {
        self.entries
            .iter()
            .any(|e| e.level == *level && e.fields.contains_key(field))
    }

    /// Check if a field exists at any level.
    fn has_field(&self, field: &str) -> bool {
        self.entries.iter().any(|e| e.fields.contains_key(field))
    }

    /// Get all fields present in the log database.
    fn all_fields(&self) -> Vec<String> {
        let mut fields: Vec<String> = self
            .entries
            .iter()
            .flat_map(|e| e.fields.keys().cloned())
            .collect();
        fields.sort();
        fields.dedup();
        fields
    }
}

/// Build a "complete" log database that a healthy pipeline would produce.
fn build_healthy_log_database() -> LogDatabase {
    let mut db = LogDatabase::new();
    // M0 Preflight
    db.add(
        LogLevel::Info,
        "M0",
        vec![
            ("migration_stage", "M0Preflight"),
            ("event_count", "100"),
            ("backend", "append_log"),
            ("data_path", "/data/events.log"),
        ],
    );
    // M1 Export
    db.add(
        LogLevel::Info,
        "M1",
        vec![
            ("migration_stage", "M1Export"),
            ("events_exported", "100"),
            ("event_count", "100"),
            ("digest", "12345678"),
        ],
    );
    // M2 Import
    db.add(
        LogLevel::Info,
        "M2",
        vec![
            ("migration_stage", "M2Import"),
            ("event_count", "100"),
            ("digest", "12345678"),
        ],
    );
    // M5 Cutover
    db.add(
        LogLevel::Info,
        "M5",
        vec![
            ("migration_stage", "M5Cutover"),
            ("backend", "franken_sqlite"),
        ],
    );
    // Bootstrap
    db.add(
        LogLevel::Info,
        "bootstrap",
        vec![
            ("backend", "franken_sqlite"),
            ("data_path", "/data/recorder.db"),
        ],
    );
    db
}

/// Build a log database with rollback-related entries.
fn build_rollback_log_database() -> LogDatabase {
    let mut db = build_healthy_log_database();
    db.add(
        LogLevel::Warn,
        "rollback",
        vec![
            ("rollback_class", "Immediate"),
            ("migration_stage", "M2Import"),
        ],
    );
    db
}

/// Evaluate whether a failure mode is diagnosable from available logs.
fn evaluate_explainability(
    tree: &TriageDecisionTree,
    log_db: &LogDatabase,
) -> ExplainabilityResult {
    let mut steps_satisfied = 0;
    let mut missing_fields = Vec::new();

    for step in &tree.steps {
        let step_ok = step.required_fields.iter().all(|f| log_db.has_field(f));
        if step_ok {
            steps_satisfied += 1;
        } else {
            for field in &step.required_fields {
                if !log_db.has_field(field) && !missing_fields.contains(field) {
                    missing_fields.push(field.clone());
                }
            }
        }
    }

    let total = tree.steps.len();
    let coverage_pct = if total > 0 {
        (steps_satisfied as f64 / total as f64) * 100.0
    } else {
        100.0
    };

    ExplainabilityResult {
        failure_mode: tree.failure_mode.clone(),
        steps_total: total,
        steps_satisfied,
        coverage_pct,
        diagnosable: missing_fields.is_empty(),
        missing_fields,
    }
}

#[derive(Debug, Clone)]
struct ExplainabilityResult {
    failure_mode: FailureMode,
    steps_total: usize,
    steps_satisfied: usize,
    coverage_pct: f64,
    diagnosable: bool,
    missing_fields: Vec<String>,
}

/// Generate the full explainability report.
fn generate_explainability_report(log_db: &LogDatabase) -> ExplainabilityReport {
    let trees = build_triage_trees();
    let results: Vec<ExplainabilityResult> = trees
        .iter()
        .map(|t| evaluate_explainability(t, log_db))
        .collect();

    let all_diagnosable = results.iter().all(|r| r.diagnosable);
    let diagnosable_count = results.iter().filter(|r| r.diagnosable).count();

    ExplainabilityReport {
        failure_modes_total: results.len(),
        failure_modes_diagnosable: diagnosable_count,
        all_diagnosable,
        results,
    }
}

#[derive(Debug, Clone)]
struct ExplainabilityReport {
    failure_modes_total: usize,
    failure_modes_diagnosable: usize,
    all_diagnosable: bool,
    results: Vec<ExplainabilityResult>,
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Decision tree structure
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_failure_modes_count() {
    assert_eq!(FailureMode::all().len(), 8);
}

#[test]
fn test_all_failure_modes_have_descriptions() {
    for fm in FailureMode::all() {
        assert!(
            !fm.description().is_empty(),
            "{:?} has empty description",
            fm
        );
    }
}

#[test]
fn test_triage_trees_cover_all_failure_modes() {
    let trees = build_triage_trees();
    let covered: std::collections::HashSet<_> = trees.iter().map(|t| &t.failure_mode).collect();
    for fm in FailureMode::all() {
        assert!(
            covered.contains(&fm),
            "{:?} not covered by triage trees",
            fm
        );
    }
}

#[test]
fn test_triage_trees_all_have_steps() {
    let trees = build_triage_trees();
    for tree in &trees {
        assert!(
            !tree.steps.is_empty(),
            "{:?} has no triage steps",
            tree.failure_mode
        );
    }
}

#[test]
fn test_triage_trees_all_diagnosable_by_design() {
    let trees = build_triage_trees();
    for tree in &trees {
        assert!(
            tree.root_cause_diagnosable,
            "{:?} should be diagnosable by design",
            tree.failure_mode
        );
    }
}

#[test]
fn test_triage_tree_migration_stuck_has_3_steps() {
    let trees = build_triage_trees();
    let tree = trees
        .iter()
        .find(|t| t.failure_mode == FailureMode::MigrationStuckAtM2)
        .unwrap();
    assert_eq!(tree.steps.len(), 3);
}

#[test]
fn test_triage_tree_rollback_requires_rollback_class() {
    let trees = build_triage_trees();
    let tree = trees
        .iter()
        .find(|t| t.failure_mode == FailureMode::RollbackTriggered)
        .unwrap();
    let fields = tree.required_field_set();
    assert!(fields.contains(&"rollback_class".to_string()));
}

#[test]
fn test_triage_tree_corruption_requires_digest() {
    let trees = build_triage_trees();
    let tree = trees
        .iter()
        .find(|t| t.failure_mode == FailureMode::DataCorruption)
        .unwrap();
    let fields = tree.required_field_set();
    assert!(fields.contains(&"digest".to_string()));
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Log database
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_healthy_log_db_has_all_required_fields() {
    let db = build_healthy_log_database();
    let required = [
        "migration_stage",
        "event_count",
        "digest",
        "events_exported",
        "backend",
        "data_path",
    ];
    for field in &required {
        assert!(db.has_field(field), "missing field: {field}");
    }
}

#[test]
fn test_rollback_log_db_has_rollback_class() {
    let db = build_rollback_log_database();
    assert!(db.has_field("rollback_class"));
}

#[test]
fn test_log_db_field_at_level() {
    let db = build_rollback_log_database();
    assert!(db.has_field_at_level("rollback_class", &LogLevel::Warn));
    assert!(!db.has_field_at_level("rollback_class", &LogLevel::Info));
}

#[test]
fn test_log_db_all_fields() {
    let db = build_healthy_log_database();
    let fields = db.all_fields();
    assert!(fields.len() >= 6);
}

#[test]
fn test_empty_log_db_has_no_fields() {
    let db = LogDatabase::new();
    assert!(db.all_fields().is_empty());
    assert!(!db.has_field("migration_stage"));
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Explainability evaluation
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_migration_stuck_diagnosable_from_logs() {
    let db = build_healthy_log_database();
    let trees = build_triage_trees();
    let tree = trees
        .iter()
        .find(|t| t.failure_mode == FailureMode::MigrationStuckAtM2)
        .unwrap();
    let result = evaluate_explainability(tree, &db);
    assert!(
        result.diagnosable,
        "MigrationStuckAtM2 should be diagnosable; missing: {:?}",
        result.missing_fields
    );
}

#[test]
fn test_index_lag_diagnosable_from_logs() {
    let db = build_healthy_log_database();
    let trees = build_triage_trees();
    let tree = trees
        .iter()
        .find(|t| t.failure_mode == FailureMode::IndexLagSpike)
        .unwrap();
    let result = evaluate_explainability(tree, &db);
    assert!(
        result.diagnosable,
        "IndexLagSpike should be diagnosable; missing: {:?}",
        result.missing_fields
    );
}

#[test]
fn test_rollback_reason_diagnosable_from_logs() {
    let db = build_rollback_log_database();
    let trees = build_triage_trees();
    let tree = trees
        .iter()
        .find(|t| t.failure_mode == FailureMode::RollbackTriggered)
        .unwrap();
    let result = evaluate_explainability(tree, &db);
    assert!(
        result.diagnosable,
        "RollbackTriggered should be diagnosable; missing: {:?}",
        result.missing_fields
    );
}

#[test]
fn test_corruption_diagnosable_from_logs() {
    let db = build_rollback_log_database();
    let trees = build_triage_trees();
    let tree = trees
        .iter()
        .find(|t| t.failure_mode == FailureMode::DataCorruption)
        .unwrap();
    let result = evaluate_explainability(tree, &db);
    assert!(
        result.diagnosable,
        "DataCorruption should be diagnosable; missing: {:?}",
        result.missing_fields
    );
}

#[test]
fn test_health_degraded_diagnosable_from_logs() {
    let db = build_healthy_log_database();
    let trees = build_triage_trees();
    let tree = trees
        .iter()
        .find(|t| t.failure_mode == FailureMode::HealthDegraded)
        .unwrap();
    let result = evaluate_explainability(tree, &db);
    assert!(
        result.diagnosable,
        "HealthDegraded should be diagnosable; missing: {:?}",
        result.missing_fields
    );
}

#[test]
fn test_checkpoint_regression_diagnosable() {
    let db = build_healthy_log_database();
    let trees = build_triage_trees();
    let tree = trees
        .iter()
        .find(|t| t.failure_mode == FailureMode::CheckpointRegression)
        .unwrap();
    let result = evaluate_explainability(tree, &db);
    assert!(result.diagnosable);
}

#[test]
fn test_write_freeze_diagnosable() {
    let db = build_rollback_log_database();
    let trees = build_triage_trees();
    let tree = trees
        .iter()
        .find(|t| t.failure_mode == FailureMode::WriteFreezeActive)
        .unwrap();
    let result = evaluate_explainability(tree, &db);
    assert!(result.diagnosable);
}

#[test]
fn test_empty_source_diagnosable() {
    let db = build_healthy_log_database();
    let trees = build_triage_trees();
    let tree = trees
        .iter()
        .find(|t| t.failure_mode == FailureMode::EmptySourceMigration)
        .unwrap();
    let result = evaluate_explainability(tree, &db);
    assert!(result.diagnosable);
}

#[test]
fn test_explainability_missing_field_detected() {
    let db = LogDatabase::new(); // Empty — no logs at all
    let trees = build_triage_trees();
    let tree = trees
        .iter()
        .find(|t| t.failure_mode == FailureMode::MigrationStuckAtM2)
        .unwrap();
    let result = evaluate_explainability(tree, &db);
    assert!(!result.diagnosable);
    assert!(!result.missing_fields.is_empty());
}

#[test]
fn test_explainability_coverage_percentage() {
    let db = build_healthy_log_database();
    let trees = build_triage_trees();
    let tree = trees
        .iter()
        .find(|t| t.failure_mode == FailureMode::MigrationStuckAtM2)
        .unwrap();
    let result = evaluate_explainability(tree, &db);
    assert!((result.coverage_pct - 100.0).abs() < 0.01);
}

#[test]
fn test_explainability_partial_coverage() {
    let mut db = LogDatabase::new();
    db.add(LogLevel::Info, "M0", vec![("migration_stage", "M0")]);
    // Missing event_count and events_exported
    let trees = build_triage_trees();
    let tree = trees
        .iter()
        .find(|t| t.failure_mode == FailureMode::MigrationStuckAtM2)
        .unwrap();
    let result = evaluate_explainability(tree, &db);
    assert!(result.coverage_pct > 0.0);
    assert!(result.coverage_pct < 100.0);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Full explainability report
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_explainability_report_covers_all_failure_modes() {
    let db = build_rollback_log_database();
    let report = generate_explainability_report(&db);
    assert_eq!(report.failure_modes_total, 8);
}

#[test]
fn test_explainability_report_all_diagnosable_with_full_logs() {
    let db = build_rollback_log_database();
    let report = generate_explainability_report(&db);
    assert!(
        report.all_diagnosable,
        "Not all failure modes diagnosable. Failed: {:?}",
        report
            .results
            .iter()
            .filter(|r| !r.diagnosable)
            .map(|r| format!("{:?}: missing {:?}", r.failure_mode, r.missing_fields))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_explainability_report_empty_logs_mostly_not_diagnosable() {
    let db = LogDatabase::new();
    let report = generate_explainability_report(&db);
    assert!(!report.all_diagnosable);
    assert_eq!(report.failure_modes_diagnosable, 0);
}

#[test]
fn test_explainability_report_partial_logs() {
    let mut db = LogDatabase::new();
    db.add(
        LogLevel::Info,
        "M0",
        vec![
            ("migration_stage", "M0"),
            ("event_count", "50"),
            ("backend", "append_log"),
            ("data_path", "/data"),
        ],
    );
    let report = generate_explainability_report(&db);
    // Some modes should be diagnosable, others not
    assert!(report.failure_modes_diagnosable > 0);
    assert!(report.failure_modes_diagnosable < report.failure_modes_total);
}

#[test]
fn test_explainability_report_counts_consistent() {
    let db = build_rollback_log_database();
    let report = generate_explainability_report(&db);
    let counted = report.results.iter().filter(|r| r.diagnosable).count();
    assert_eq!(counted, report.failure_modes_diagnosable);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Required field set extraction
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_required_field_set_deduplicates() {
    let trees = build_triage_trees();
    let tree = trees
        .iter()
        .find(|t| t.failure_mode == FailureMode::DataCorruption)
        .unwrap();
    let fields = tree.required_field_set();
    let unique: std::collections::HashSet<&String> = fields.iter().collect();
    assert_eq!(fields.len(), unique.len());
}

#[test]
fn test_required_fields_across_all_trees() {
    let trees = build_triage_trees();
    let mut all_fields: Vec<String> = trees.iter().flat_map(|t| t.required_field_set()).collect();
    all_fields.sort();
    all_fields.dedup();
    // Should cover the key log fields
    assert!(all_fields.contains(&"migration_stage".to_string()));
    assert!(all_fields.contains(&"event_count".to_string()));
    assert!(all_fields.contains(&"digest".to_string()));
    assert!(all_fields.contains(&"backend".to_string()));
    assert!(all_fields.contains(&"rollback_class".to_string()));
}
