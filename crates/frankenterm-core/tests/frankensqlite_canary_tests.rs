//! E5.F1.T2: Canary progressive rollout and rollback runbook automation.
//!
//! Tests canary workspace selection, soak monitoring, auto-rollback
//! thresholds, progressive expansion, and runbook step execution.

// No external imports needed — all types are local.

// ═══════════════════════════════════════════════════════════════════════
// Canary rollout model
// ═══════════════════════════════════════════════════════════════════════

/// State of a canary rollout.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum CanaryState {
    Pending,
    MigrationRunning,
    SoakMonitoring,
    SoakComplete,
    RolledBack,
    Promoted,
}

/// A workspace participating in canary rollout.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CanaryWorkspace {
    workspace_id: String,
    state: CanaryState,
    migration_complete: bool,
    soak_start_ms: Option<u64>,
    soak_end_ms: Option<u64>,
}

/// Health observation during soak window.
#[derive(Debug, Clone)]
struct SoakObservation {
    timestamp_ms: u64,
    healthy: bool,
    lag_ms: f64,
    error_count: u32,
}

/// Thresholds for auto-rollback during soak.
#[derive(Debug, Clone)]
struct SoakThresholds {
    max_unhealthy_consecutive: u32,
    max_lag_p99_ms: f64,
    max_error_rate: f64, // errors per observation
    min_soak_duration_ms: u64,
}

impl Default for SoakThresholds {
    fn default() -> Self {
        Self {
            max_unhealthy_consecutive: 3,
            max_lag_p99_ms: 50.0,
            max_error_rate: 0.01,
            min_soak_duration_ms: 12 * 3600 * 1000, // 12 hours
        }
    }
}

/// Soak monitoring result.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SoakVerdict {
    Continue,
    AutoRollback(String),
    SoakComplete,
}

/// Evaluate soak observations against thresholds.
fn evaluate_soak(observations: &[SoakObservation], thresholds: &SoakThresholds) -> SoakVerdict {
    if observations.is_empty() {
        return SoakVerdict::Continue;
    }

    // Check consecutive unhealthy
    let mut consecutive_unhealthy = 0u32;
    for obs in observations.iter().rev() {
        if !obs.healthy {
            consecutive_unhealthy += 1;
        } else {
            break;
        }
    }
    if consecutive_unhealthy >= thresholds.max_unhealthy_consecutive {
        return SoakVerdict::AutoRollback(format!(
            "consecutive unhealthy: {consecutive_unhealthy} >= {}",
            thresholds.max_unhealthy_consecutive
        ));
    }

    // Check lag p99 (use max as proxy)
    let max_lag = observations.iter().map(|o| o.lag_ms).fold(0.0f64, f64::max);
    if max_lag > thresholds.max_lag_p99_ms {
        return SoakVerdict::AutoRollback(format!(
            "lag {max_lag:.1}ms > {:.1}ms budget",
            thresholds.max_lag_p99_ms
        ));
    }

    // Check error rate
    let total_errors: u32 = observations.iter().map(|o| o.error_count).sum();
    let error_rate = total_errors as f64 / observations.len() as f64;
    if error_rate > thresholds.max_error_rate {
        return SoakVerdict::AutoRollback(format!(
            "error rate {error_rate:.4} > {:.4} threshold",
            thresholds.max_error_rate
        ));
    }

    // Check duration
    let first_ts = observations.first().unwrap().timestamp_ms;
    let last_ts = observations.last().unwrap().timestamp_ms;
    let duration_ms = last_ts.saturating_sub(first_ts);
    if duration_ms >= thresholds.min_soak_duration_ms {
        return SoakVerdict::SoakComplete;
    }

    SoakVerdict::Continue
}

// ═══════════════════════════════════════════════════════════════════════
// Rollback runbook model
// ═══════════════════════════════════════════════════════════════════════

/// A step in the rollback runbook.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RunbookStep {
    step_number: u32,
    action: String,
    verification: String,
    rollback_safe: bool,
}

/// The complete rollback runbook.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RollbackRunbook {
    version: String,
    steps: Vec<RunbookStep>,
}

/// Build the canonical rollback runbook.
fn build_rollback_runbook() -> RollbackRunbook {
    RollbackRunbook {
        version: "1.0".to_string(),
        steps: vec![
            RunbookStep {
                step_number: 1,
                action: "Switch backend_kind back to append_log in config".to_string(),
                verification: "Health endpoint shows backend=append_log".to_string(),
                rollback_safe: true,
            },
            RunbookStep {
                step_number: 2,
                action: "Clear FrankenSqlite target database if exists".to_string(),
                verification: "Target DB file removed or empty".to_string(),
                rollback_safe: true,
            },
            RunbookStep {
                step_number: 3,
                action: "Restart recorder with AppendLog backend".to_string(),
                verification: "Health check returns healthy=true, backend=append_log".to_string(),
                rollback_safe: true,
            },
            RunbookStep {
                step_number: 4,
                action: "Verify source data integrity (digest check)".to_string(),
                verification: "Source event count matches pre-migration count".to_string(),
                rollback_safe: true,
            },
            RunbookStep {
                step_number: 5,
                action: "Reset migration checkpoint state".to_string(),
                verification: "Migration state file deleted or reset".to_string(),
                rollback_safe: true,
            },
            RunbookStep {
                step_number: 6,
                action: "Resume normal capture operations".to_string(),
                verification: "New events appending successfully to AppendLog".to_string(),
                rollback_safe: true,
            },
        ],
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Progressive rollout model
// ═══════════════════════════════════════════════════════════════════════

/// Progressive rollout cohort.
#[derive(Debug, Clone)]
struct RolloutCohort {
    name: String,
    workspace_ids: Vec<String>,
    percentage: f64,
}

/// Build progressive rollout plan.
fn build_progressive_plan(total_workspaces: usize) -> Vec<RolloutCohort> {
    // 5% → 25% → 50% → 100%
    let percentages = [0.05, 0.25, 0.50, 1.0];
    let mut plan = Vec::new();
    let mut covered = 0;

    for (i, pct) in percentages.iter().enumerate() {
        let target = (total_workspaces as f64 * pct).ceil() as usize;
        let count = target.saturating_sub(covered);
        let ids: Vec<String> = (covered..covered + count)
            .map(|j| format!("ws-{j:03}"))
            .collect();
        plan.push(RolloutCohort {
            name: format!("cohort-{}", i + 1),
            workspace_ids: ids,
            percentage: *pct,
        });
        covered += count;
    }
    plan
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Canary state machine
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_canary_initial_state_pending() {
    let ws = CanaryWorkspace {
        workspace_id: "ws-001".to_string(),
        state: CanaryState::Pending,
        migration_complete: false,
        soak_start_ms: None,
        soak_end_ms: None,
    };
    assert_eq!(ws.state, CanaryState::Pending);
}

#[test]
fn test_canary_state_transitions() {
    let valid_transitions = vec![
        (CanaryState::Pending, CanaryState::MigrationRunning),
        (CanaryState::MigrationRunning, CanaryState::SoakMonitoring),
        (CanaryState::SoakMonitoring, CanaryState::SoakComplete),
        (CanaryState::SoakComplete, CanaryState::Promoted),
        (CanaryState::MigrationRunning, CanaryState::RolledBack),
        (CanaryState::SoakMonitoring, CanaryState::RolledBack),
    ];
    // All transitions should be representable
    for (from, to) in &valid_transitions {
        assert_ne!(from, to);
    }
    assert_eq!(valid_transitions.len(), 6);
}

#[test]
fn test_canary_state_serde_roundtrip() {
    for state in &[
        CanaryState::Pending,
        CanaryState::MigrationRunning,
        CanaryState::SoakMonitoring,
        CanaryState::SoakComplete,
        CanaryState::RolledBack,
        CanaryState::Promoted,
    ] {
        let json = serde_json::to_string(state).unwrap();
        let back: CanaryState = serde_json::from_str(&json).unwrap();
        assert_eq!(*state, back);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Soak evaluation
// ═══════════════════════════════════════════════════════════════════════

fn healthy_obs(ts: u64) -> SoakObservation {
    SoakObservation {
        timestamp_ms: ts,
        healthy: true,
        lag_ms: 10.0,
        error_count: 0,
    }
}

fn unhealthy_obs(ts: u64) -> SoakObservation {
    SoakObservation {
        timestamp_ms: ts,
        healthy: false,
        lag_ms: 10.0,
        error_count: 0,
    }
}

#[test]
fn test_soak_empty_observations_continue() {
    let verdict = evaluate_soak(&[], &SoakThresholds::default());
    assert_eq!(verdict, SoakVerdict::Continue);
}

#[test]
fn test_soak_healthy_short_duration_continue() {
    let obs = vec![healthy_obs(1000), healthy_obs(2000), healthy_obs(3000)];
    let verdict = evaluate_soak(&obs, &SoakThresholds::default());
    assert_eq!(verdict, SoakVerdict::Continue);
}

#[test]
fn test_soak_healthy_long_duration_complete() {
    let thresholds = SoakThresholds {
        min_soak_duration_ms: 1000,
        ..Default::default()
    };
    let obs = vec![healthy_obs(0), healthy_obs(500), healthy_obs(1500)];
    let verdict = evaluate_soak(&obs, &thresholds);
    assert_eq!(verdict, SoakVerdict::SoakComplete);
}

#[test]
fn test_soak_consecutive_unhealthy_triggers_rollback() {
    let thresholds = SoakThresholds {
        max_unhealthy_consecutive: 3,
        ..Default::default()
    };
    let obs = vec![
        healthy_obs(0),
        unhealthy_obs(100),
        unhealthy_obs(200),
        unhealthy_obs(300),
    ];
    let verdict = evaluate_soak(&obs, &thresholds);
    let is_rollback = matches!(verdict, SoakVerdict::AutoRollback(_));
    assert!(
        is_rollback,
        "should auto-rollback after 3 consecutive unhealthy"
    );
}

#[test]
fn test_soak_two_unhealthy_no_rollback() {
    let thresholds = SoakThresholds {
        max_unhealthy_consecutive: 3,
        ..Default::default()
    };
    let obs = vec![healthy_obs(0), unhealthy_obs(100), unhealthy_obs(200)];
    let verdict = evaluate_soak(&obs, &thresholds);
    assert_eq!(verdict, SoakVerdict::Continue);
}

#[test]
fn test_soak_lag_over_budget_triggers_rollback() {
    let thresholds = SoakThresholds {
        max_lag_p99_ms: 50.0,
        ..Default::default()
    };
    let obs = vec![
        healthy_obs(0),
        SoakObservation {
            timestamp_ms: 100,
            healthy: true,
            lag_ms: 75.0,
            error_count: 0,
        },
    ];
    let verdict = evaluate_soak(&obs, &thresholds);
    let is_rollback = matches!(verdict, SoakVerdict::AutoRollback(_));
    assert!(is_rollback, "should auto-rollback on lag over budget");
}

#[test]
fn test_soak_error_rate_triggers_rollback() {
    let thresholds = SoakThresholds {
        max_error_rate: 0.01,
        ..Default::default()
    };
    let obs: Vec<_> = (0..100)
        .map(|i| SoakObservation {
            timestamp_ms: i * 100,
            healthy: true,
            lag_ms: 5.0,
            error_count: if i < 5 { 1 } else { 0 }, // 5% error rate > 1%
        })
        .collect();
    let verdict = evaluate_soak(&obs, &thresholds);
    let is_rollback = matches!(verdict, SoakVerdict::AutoRollback(_));
    assert!(is_rollback, "should auto-rollback on high error rate");
}

#[test]
fn test_soak_low_error_rate_ok() {
    let thresholds = SoakThresholds {
        max_error_rate: 0.05,
        min_soak_duration_ms: 100,
        ..Default::default()
    };
    let mut obs: Vec<_> = (0..100)
        .map(|i| SoakObservation {
            timestamp_ms: i * 10,
            healthy: true,
            lag_ms: 5.0,
            error_count: 0,
        })
        .collect();
    // One error in 100 = 1% < 5% threshold
    obs[50].error_count = 1;
    let verdict = evaluate_soak(&obs, &thresholds);
    assert_eq!(verdict, SoakVerdict::SoakComplete);
}

#[test]
fn test_soak_rollback_reason_contains_detail() {
    let thresholds = SoakThresholds {
        max_unhealthy_consecutive: 2,
        ..Default::default()
    };
    let obs = vec![unhealthy_obs(0), unhealthy_obs(100)];
    let verdict = evaluate_soak(&obs, &thresholds);
    if let SoakVerdict::AutoRollback(reason) = verdict {
        assert!(reason.contains("consecutive unhealthy"));
    } else {
        panic!("expected AutoRollback");
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Rollback runbook
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_runbook_has_six_steps() {
    let runbook = build_rollback_runbook();
    assert_eq!(runbook.steps.len(), 6);
}

#[test]
fn test_runbook_steps_numbered_sequentially() {
    let runbook = build_rollback_runbook();
    for (i, step) in runbook.steps.iter().enumerate() {
        assert_eq!(step.step_number, (i + 1) as u32);
    }
}

#[test]
fn test_runbook_all_steps_rollback_safe() {
    let runbook = build_rollback_runbook();
    for step in &runbook.steps {
        assert!(
            step.rollback_safe,
            "step {} should be rollback-safe",
            step.step_number
        );
    }
}

#[test]
fn test_runbook_first_step_switches_backend() {
    let runbook = build_rollback_runbook();
    assert!(runbook.steps[0].action.contains("backend_kind"));
    assert!(runbook.steps[0].action.contains("append_log"));
}

#[test]
fn test_runbook_has_verification_for_each_step() {
    let runbook = build_rollback_runbook();
    for step in &runbook.steps {
        assert!(
            !step.verification.is_empty(),
            "step {} has no verification",
            step.step_number
        );
    }
}

#[test]
fn test_runbook_serde_roundtrip() {
    let runbook = build_rollback_runbook();
    let json = serde_json::to_string_pretty(&runbook).unwrap();
    let back: RollbackRunbook = serde_json::from_str(&json).unwrap();
    assert_eq!(runbook.steps.len(), back.steps.len());
    assert_eq!(runbook.version, back.version);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Progressive rollout plan
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_progressive_plan_4_cohorts() {
    let plan = build_progressive_plan(100);
    assert_eq!(plan.len(), 4);
}

#[test]
fn test_progressive_plan_first_cohort_5_percent() {
    let plan = build_progressive_plan(100);
    assert_eq!(plan[0].workspace_ids.len(), 5);
    assert!((plan[0].percentage - 0.05).abs() < 0.001);
}

#[test]
fn test_progressive_plan_covers_all_workspaces() {
    let plan = build_progressive_plan(100);
    let total: usize = plan.iter().map(|c| c.workspace_ids.len()).sum();
    assert_eq!(total, 100);
}

#[test]
fn test_progressive_plan_no_duplicate_workspaces() {
    let plan = build_progressive_plan(100);
    let all_ids: Vec<&str> = plan
        .iter()
        .flat_map(|c| c.workspace_ids.iter().map(|s| s.as_str()))
        .collect();
    let unique: std::collections::HashSet<&str> = all_ids.iter().copied().collect();
    assert_eq!(all_ids.len(), unique.len());
}

#[test]
fn test_progressive_plan_small_workspace_count() {
    let plan = build_progressive_plan(5);
    let total: usize = plan.iter().map(|c| c.workspace_ids.len()).sum();
    assert_eq!(total, 5);
}

#[test]
fn test_progressive_plan_single_workspace() {
    let plan = build_progressive_plan(1);
    let total: usize = plan.iter().map(|c| c.workspace_ids.len()).sum();
    assert_eq!(total, 1);
}

#[test]
fn test_progressive_plan_cohort_names_unique() {
    let plan = build_progressive_plan(100);
    let names: Vec<&str> = plan.iter().map(|c| c.name.as_str()).collect();
    let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
    assert_eq!(names.len(), unique.len());
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Soak thresholds
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_default_thresholds_3_unhealthy() {
    let t = SoakThresholds::default();
    assert_eq!(t.max_unhealthy_consecutive, 3);
}

#[test]
fn test_default_thresholds_50ms_lag() {
    let t = SoakThresholds::default();
    assert!((t.max_lag_p99_ms - 50.0).abs() < 0.001);
}

#[test]
fn test_default_thresholds_12h_soak() {
    let t = SoakThresholds::default();
    assert_eq!(t.min_soak_duration_ms, 12 * 3600 * 1000);
}

#[test]
fn test_custom_thresholds() {
    let t = SoakThresholds {
        max_unhealthy_consecutive: 5,
        max_lag_p99_ms: 100.0,
        max_error_rate: 0.05,
        min_soak_duration_ms: 3600_000,
    };
    assert_eq!(t.max_unhealthy_consecutive, 5);
}

#[test]
fn test_runbook_last_step_resumes_capture() {
    let runbook = build_rollback_runbook();
    let last = runbook.steps.last().unwrap();
    assert!(last.action.contains("Resume"));
    assert!(last.verification.contains("appending"));
}
