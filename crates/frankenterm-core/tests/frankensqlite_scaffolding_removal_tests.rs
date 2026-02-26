//! E5.F2.T3: Transitional scaffolding removal and single-backend architecture.
//!
//! Tests the cleanup plan: AppendLog removal, dispatch simplification,
//! migration engine archival, and steady-state operations runbook.

use std::collections::BTreeSet;

// ═══════════════════════════════════════════════════════════════════════
// Cleanup tracking model
// ═══════════════════════════════════════════════════════════════════════

/// Modules that exist during transitional period and should be removed.
const TRANSITIONAL_MODULES: &[&str] = &[
    "append_log_storage",
    "recorder_backend_kind",
    "recorder_storage_instance",
    "migration_engine",
    "dual_backend_dispatch",
];

/// Modules that remain in steady-state single-backend architecture.
const STEADY_STATE_MODULES: &[&str] = &[
    "recorder_storage",      // trait definition
    "frankensqlite_storage", // sole implementation
    "recording",             // event types
    "recorder_migration",    // archived, not default build
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum CleanupAction {
    Remove,
    Archive,
    Simplify,
    Keep,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CleanupItem {
    module: String,
    action: CleanupAction,
    reason: String,
    verified: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CleanupPlan {
    items: Vec<CleanupItem>,
}

impl CleanupPlan {
    fn new() -> Self {
        Self { items: Vec::new() }
    }

    fn add(&mut self, module: &str, action: CleanupAction, reason: &str) {
        self.items.push(CleanupItem {
            module: module.to_string(),
            action,
            reason: reason.to_string(),
            verified: false,
        });
    }

    fn pending_removals(&self) -> Vec<&CleanupItem> {
        self.items
            .iter()
            .filter(|i| i.action == CleanupAction::Remove && !i.verified)
            .collect()
    }

    fn pending_archives(&self) -> Vec<&CleanupItem> {
        self.items
            .iter()
            .filter(|i| i.action == CleanupAction::Archive && !i.verified)
            .collect()
    }

    fn all_verified(&self) -> bool {
        self.items.iter().all(|i| i.verified)
    }

    fn verify(&mut self, module: &str) -> bool {
        if let Some(item) = self.items.iter_mut().find(|i| i.module == module) {
            item.verified = true;
            true
        } else {
            false
        }
    }

    fn total_items(&self) -> usize {
        self.items.len()
    }

    fn verified_count(&self) -> usize {
        self.items.iter().filter(|i| i.verified).count()
    }

    fn progress_pct(&self) -> f64 {
        if self.items.is_empty() {
            return 100.0;
        }
        (self.verified_count() as f64 / self.total_items() as f64) * 100.0
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Steady-state operations runbook
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RunbookStep {
    order: u32,
    title: String,
    description: String,
    automated: bool,
    verified: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct OperationsRunbook {
    name: String,
    steps: Vec<RunbookStep>,
}

impl OperationsRunbook {
    fn steady_state_runbook() -> Self {
        Self {
            name: "FrankenSqlite Steady-State Operations".to_string(),
            steps: vec![
                RunbookStep {
                    order: 1,
                    title: "Verify single backend bootstrap".to_string(),
                    description: "bootstrap_recorder_storage() creates FrankenSqlite directly"
                        .to_string(),
                    automated: true,
                    verified: false,
                },
                RunbookStep {
                    order: 2,
                    title: "Confirm no AppendLog references".to_string(),
                    description: "grep for AppendLog usage in non-test, non-archive code"
                        .to_string(),
                    automated: true,
                    verified: false,
                },
                RunbookStep {
                    order: 3,
                    title: "Run full test suite".to_string(),
                    description: "All existing tests pass with single-backend".to_string(),
                    automated: true,
                    verified: false,
                },
                RunbookStep {
                    order: 4,
                    title: "Verify migration module not in default build".to_string(),
                    description: "Migration engine behind feature flag or removed".to_string(),
                    automated: true,
                    verified: false,
                },
                RunbookStep {
                    order: 5,
                    title: "Health check endpoint".to_string(),
                    description: "Storage health returns healthy with single backend".to_string(),
                    automated: true,
                    verified: false,
                },
                RunbookStep {
                    order: 6,
                    title: "Archive migration code".to_string(),
                    description: "Confirm migration code is in VCS history".to_string(),
                    automated: false,
                    verified: false,
                },
            ],
        }
    }

    fn automated_steps(&self) -> Vec<&RunbookStep> {
        self.steps.iter().filter(|s| s.automated).collect()
    }

    fn manual_steps(&self) -> Vec<&RunbookStep> {
        self.steps.iter().filter(|s| !s.automated).collect()
    }

    fn is_complete(&self) -> bool {
        self.steps.iter().all(|s| s.verified)
    }

    fn next_unverified(&self) -> Option<&RunbookStep> {
        self.steps.iter().find(|s| !s.verified)
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Reference scanning model
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ReferenceHit {
    file: String,
    line: u32,
    pattern: String,
    is_test: bool,
    is_archive: bool,
}

impl ReferenceHit {
    fn is_production_reference(&self) -> bool {
        !self.is_test && !self.is_archive
    }
}

fn scan_references<'a>(hits: &'a [ReferenceHit], pattern: &str) -> Vec<&'a ReferenceHit> {
    hits.iter().filter(|h| h.pattern == pattern).collect()
}

fn production_references(hits: &[ReferenceHit]) -> Vec<&ReferenceHit> {
    hits.iter()
        .filter(|h| h.is_production_reference())
        .collect()
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Transitional modules
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_transitional_modules_count() {
    assert_eq!(TRANSITIONAL_MODULES.len(), 5);
}

#[test]
fn test_steady_state_modules_count() {
    assert_eq!(STEADY_STATE_MODULES.len(), 4);
}

#[test]
fn test_no_overlap_transitional_steady_state() {
    let trans: BTreeSet<&str> = TRANSITIONAL_MODULES.iter().copied().collect();
    let steady: BTreeSet<&str> = STEADY_STATE_MODULES.iter().copied().collect();
    assert!(trans.intersection(&steady).next().is_none());
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: CleanupPlan
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_cleanup_plan_add_items() {
    let mut plan = CleanupPlan::new();
    plan.add("append_log", CleanupAction::Remove, "No longer needed");
    assert_eq!(plan.total_items(), 1);
}

#[test]
fn test_cleanup_plan_pending_removals() {
    let mut plan = CleanupPlan::new();
    plan.add("append_log", CleanupAction::Remove, "");
    plan.add("migration", CleanupAction::Archive, "");
    assert_eq!(plan.pending_removals().len(), 1);
    assert_eq!(plan.pending_archives().len(), 1);
}

#[test]
fn test_cleanup_plan_verify() {
    let mut plan = CleanupPlan::new();
    plan.add("append_log", CleanupAction::Remove, "");
    assert!(!plan.all_verified());
    plan.verify("append_log");
    assert!(plan.all_verified());
}

#[test]
fn test_cleanup_plan_verify_unknown_module() {
    let mut plan = CleanupPlan::new();
    assert!(!plan.verify("nonexistent"));
}

#[test]
fn test_cleanup_plan_progress() {
    let mut plan = CleanupPlan::new();
    plan.add("a", CleanupAction::Remove, "");
    plan.add("b", CleanupAction::Remove, "");
    assert!((plan.progress_pct() - 0.0).abs() < f64::EPSILON);
    plan.verify("a");
    assert!((plan.progress_pct() - 50.0).abs() < f64::EPSILON);
    plan.verify("b");
    assert!((plan.progress_pct() - 100.0).abs() < f64::EPSILON);
}

#[test]
fn test_cleanup_plan_empty_is_verified() {
    let plan = CleanupPlan::new();
    assert!(plan.all_verified());
    assert!((plan.progress_pct() - 100.0).abs() < f64::EPSILON);
}

#[test]
fn test_cleanup_plan_serde_roundtrip() {
    let mut plan = CleanupPlan::new();
    plan.add("test_mod", CleanupAction::Simplify, "simplify dispatch");
    let json = serde_json::to_string(&plan).unwrap();
    let back: CleanupPlan = serde_json::from_str(&json).unwrap();
    assert_eq!(plan.items.len(), back.items.len());
    assert_eq!(back.items[0].action, CleanupAction::Simplify);
}

#[test]
fn test_cleanup_plan_full_transitional() {
    let mut plan = CleanupPlan::new();
    for module in TRANSITIONAL_MODULES {
        plan.add(module, CleanupAction::Remove, "transitional");
    }
    assert_eq!(plan.pending_removals().len(), 5);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: CleanupAction
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_cleanup_action_serde_roundtrip() {
    for action in &[
        CleanupAction::Remove,
        CleanupAction::Archive,
        CleanupAction::Simplify,
        CleanupAction::Keep,
    ] {
        let json = serde_json::to_string(action).unwrap();
        let back: CleanupAction = serde_json::from_str(&json).unwrap();
        assert_eq!(*action, back);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: OperationsRunbook
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_runbook_has_6_steps() {
    let rb = OperationsRunbook::steady_state_runbook();
    assert_eq!(rb.steps.len(), 6);
}

#[test]
fn test_runbook_automated_steps() {
    let rb = OperationsRunbook::steady_state_runbook();
    assert_eq!(rb.automated_steps().len(), 5);
}

#[test]
fn test_runbook_manual_steps() {
    let rb = OperationsRunbook::steady_state_runbook();
    assert_eq!(rb.manual_steps().len(), 1);
}

#[test]
fn test_runbook_starts_incomplete() {
    let rb = OperationsRunbook::steady_state_runbook();
    assert!(!rb.is_complete());
}

#[test]
fn test_runbook_next_unverified_is_first() {
    let rb = OperationsRunbook::steady_state_runbook();
    assert_eq!(rb.next_unverified().unwrap().order, 1);
}

#[test]
fn test_runbook_complete_when_all_verified() {
    let mut rb = OperationsRunbook::steady_state_runbook();
    for step in &mut rb.steps {
        step.verified = true;
    }
    assert!(rb.is_complete());
}

#[test]
fn test_runbook_serde_roundtrip() {
    let rb = OperationsRunbook::steady_state_runbook();
    let json = serde_json::to_string_pretty(&rb).unwrap();
    let back: OperationsRunbook = serde_json::from_str(&json).unwrap();
    assert_eq!(rb.steps.len(), back.steps.len());
    assert_eq!(rb.name, back.name);
}

#[test]
fn test_runbook_steps_ordered() {
    let rb = OperationsRunbook::steady_state_runbook();
    for window in rb.steps.windows(2) {
        assert!(window[0].order < window[1].order);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Reference scanning
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_scan_references_filters_by_pattern() {
    let hits = vec![
        ReferenceHit {
            file: "lib.rs".to_string(),
            line: 10,
            pattern: "AppendLog".to_string(),
            is_test: false,
            is_archive: false,
        },
        ReferenceHit {
            file: "test.rs".to_string(),
            line: 20,
            pattern: "AppendLog".to_string(),
            is_test: true,
            is_archive: false,
        },
        ReferenceHit {
            file: "lib.rs".to_string(),
            line: 30,
            pattern: "FrankenSqlite".to_string(),
            is_test: false,
            is_archive: false,
        },
    ];
    let results = scan_references(&hits, "AppendLog");
    assert_eq!(results.len(), 2);
}

#[test]
fn test_production_references_excludes_tests() {
    let hits = vec![
        ReferenceHit {
            file: "lib.rs".to_string(),
            line: 10,
            pattern: "AppendLog".to_string(),
            is_test: false,
            is_archive: false,
        },
        ReferenceHit {
            file: "test.rs".to_string(),
            line: 20,
            pattern: "AppendLog".to_string(),
            is_test: true,
            is_archive: false,
        },
    ];
    let prod = production_references(&hits);
    assert_eq!(prod.len(), 1);
    assert_eq!(prod[0].file, "lib.rs");
}

#[test]
fn test_production_references_excludes_archives() {
    let hits = vec![ReferenceHit {
        file: "archive/old.rs".to_string(),
        line: 5,
        pattern: "MigrationEngine".to_string(),
        is_test: false,
        is_archive: true,
    }];
    assert!(production_references(&hits).is_empty());
}

#[test]
fn test_no_append_log_references_remaining_simulation() {
    // Simulate post-cleanup: only test/archive references remain
    let hits = vec![
        ReferenceHit {
            file: "tests/fixture.rs".to_string(),
            line: 10,
            pattern: "AppendLog".to_string(),
            is_test: true,
            is_archive: false,
        },
        ReferenceHit {
            file: "archive/legacy.rs".to_string(),
            line: 20,
            pattern: "AppendLog".to_string(),
            is_test: false,
            is_archive: true,
        },
    ];
    let prod = production_references(&hits);
    assert!(
        prod.is_empty(),
        "No production AppendLog references should remain"
    );
}

#[test]
fn test_reference_hit_serde_roundtrip() {
    let hit = ReferenceHit {
        file: "src/storage.rs".to_string(),
        line: 42,
        pattern: "AppendLogRecorderStorage".to_string(),
        is_test: false,
        is_archive: false,
    };
    let json = serde_json::to_string(&hit).unwrap();
    let back: ReferenceHit = serde_json::from_str(&json).unwrap();
    assert_eq!(hit.file, back.file);
    assert_eq!(hit.line, back.line);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Dry-run drill
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DryRunResult {
    tests_total: u32,
    tests_passed: u32,
    tests_failed: u32,
    compile_success: bool,
    single_backend_only: bool,
}

impl DryRunResult {
    fn is_clean(&self) -> bool {
        self.compile_success && self.tests_failed == 0 && self.single_backend_only
    }
}

#[test]
fn test_dry_run_clean_result() {
    let result = DryRunResult {
        tests_total: 500,
        tests_passed: 500,
        tests_failed: 0,
        compile_success: true,
        single_backend_only: true,
    };
    assert!(result.is_clean());
}

#[test]
fn test_dry_run_fails_with_failures() {
    let result = DryRunResult {
        tests_total: 500,
        tests_passed: 498,
        tests_failed: 2,
        compile_success: true,
        single_backend_only: true,
    };
    assert!(!result.is_clean());
}

#[test]
fn test_dry_run_fails_compile_error() {
    let result = DryRunResult {
        tests_total: 0,
        tests_passed: 0,
        tests_failed: 0,
        compile_success: false,
        single_backend_only: true,
    };
    assert!(!result.is_clean());
}

#[test]
fn test_dry_run_fails_dual_backend() {
    let result = DryRunResult {
        tests_total: 500,
        tests_passed: 500,
        tests_failed: 0,
        compile_success: true,
        single_backend_only: false,
    };
    assert!(!result.is_clean());
}

#[test]
fn test_dry_run_serde_roundtrip() {
    let result = DryRunResult {
        tests_total: 100,
        tests_passed: 100,
        tests_failed: 0,
        compile_success: true,
        single_backend_only: true,
    };
    let json = serde_json::to_string(&result).unwrap();
    let back: DryRunResult = serde_json::from_str(&json).unwrap();
    assert_eq!(result.tests_total, back.tests_total);
    assert_eq!(result.is_clean(), back.is_clean());
}
