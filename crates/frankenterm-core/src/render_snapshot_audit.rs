//! Render-thread snapshot migration audit
//! ([BR-TERM-EMULATOR-UPLIFT-2.3.2.cont] / `ft-q6x91`).
//!
//! The render-snapshot substrate already lives at
//! `render_snapshot_guard.rs` (`25e095d10`, 31 tests).
//! This module ships the **integration substrate** the
//! bead's continuation work consumes:
//!
//! 1. **Static-audit invariant** (sub-task 6) — a pure-
//!    logic checker that confirms `SnapshotKind::Mutation`
//!    is never reachable from a registered render-thread
//!    call path.
//! 2. **Call-site registry** (sub-task 1) — typed registry
//!    of `triple_buffer.read()` call sites the integration
//!    populates as it migrates each `RwLock<TerminalState>::
//!    read()` site.
//! 3. **Lock-wait instrumentation contract** (sub-task 5) —
//!    typed-state `LockAcquireProbe` capturing
//!    Instant-before / Instant-after with delta_ns
//!    projection onto `LockWaitDistribution` (substrate).
//! 4. **JSON-line log row contract** (sub-task 7) —
//!    `tests/render_snapshot/logs/<scenario>.jsonl` shape.
//! 5. **Migration-readiness rubric** — pure-logic decision
//!    tree consuming the telemetry to answer "can we land
//!    the migration?" The substrate provides
//!    `meets_p99_target()`; this module composes that
//!    with call-site coverage + drift-from-baseline.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// Static-audit invariant (sub-task 6)
// ============================================================================

/// Closed list of call-path classifications. The
/// integration tags every `triple_buffer.read()` call
/// site with one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallPathClass {
    /// Render thread (paint.rs frame loop). MUST only see
    /// `SnapshotKind::ReadOnly`.
    RenderThread,
    /// Input thread / PTY-driven dirty events. May acquire
    /// `SnapshotKind::Mutation` guards.
    InputThread,
    /// A11Y tree updates. MUST see a consistent snapshot
    /// (read-only).
    A11yThread,
    /// Per-line dirty (BR-TERM-EMULATOR-UPLIFT.1.2) reads
    /// dirty-set from snapshot. Read-only.
    DirtyLineReader,
}

impl CallPathClass {
    /// True iff this call path is allowed to acquire
    /// `Mutation` guards. Bead's stated DO NOT BREAK rule:
    ///
    /// > Mutation paths (input thread, PTY-driven dirty
    /// > events) STILL go through writer side.
    #[must_use]
    pub const fn may_acquire_mutation_kind(self) -> bool {
        matches!(self, Self::InputThread)
    }

    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::RenderThread => "render_thread",
            Self::InputThread => "input_thread",
            Self::A11yThread => "a11y_thread",
            Self::DirtyLineReader => "dirty_line_reader",
        }
    }
}

/// One registered call site.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CallSite {
    /// Stable id (e.g., `paint::draw_frame`,
    /// `a11y::announce_diff`).
    pub id: String,
    /// File path the integration's static-audit script
    /// resolved.
    pub file: String,
    /// Line number.
    pub line: u32,
    /// Class as tagged at registration.
    pub class: CallPathClass,
    /// Whether the site acquires a `Mutation` guard. The
    /// integration must set this honestly; the audit
    /// invariant catches misclassifications.
    pub acquires_mutation: bool,
}

impl CallSite {
    /// Per-site invariant. Returns `Ok(())` if the site
    /// is well-formed.
    pub fn validate(&self) -> Result<(), AuditViolation> {
        if self.acquires_mutation && !self.class.may_acquire_mutation_kind() {
            return Err(AuditViolation::MutationFromForbiddenClass {
                site_id: self.id.clone(),
                class: self.class.slug().to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditViolation {
    /// A non-`InputThread` site acquires a `Mutation`
    /// guard. The bead's DO NOT BREAK rule is broken.
    MutationFromForbiddenClass { site_id: String, class: String },
    /// A render-thread site is missing lock-wait
    /// instrumentation.
    MissingInstrumentation { site_id: String },
    /// Two sites registered with the same id (dup).
    DuplicateSiteId { site_id: String },
}

/// Registry of every migrated `triple_buffer.read()` call
/// site. The integration populates this at first launch
/// (e.g., via `ctor`-style lazy init or an explicit
/// startup pass); the audit invariant runs at startup +
/// in tests.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSiteRegistry {
    pub sites: Vec<CallSite>,
    /// Sites known to require lock-wait instrumentation
    /// (sub-task 5). Integration sets this when it adds
    /// the `Instant::now()` probe.
    pub instrumented_site_ids: Vec<String>,
}

impl CallSiteRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, site: CallSite) {
        self.sites.push(site);
    }

    pub fn mark_instrumented(&mut self, site_id: impl Into<String>) {
        self.instrumented_site_ids.push(site_id.into());
    }

    /// Run the audit invariant. Returns the list of
    /// violations (empty `Vec` on success).
    #[must_use]
    pub fn audit(&self) -> Vec<AuditViolation> {
        let mut out = Vec::new();
        let mut seen_ids = std::collections::BTreeSet::new();
        for site in &self.sites {
            if !seen_ids.insert(&site.id) {
                out.push(AuditViolation::DuplicateSiteId {
                    site_id: site.id.clone(),
                });
            }
            if let Err(v) = site.validate() {
                out.push(v);
            }
            if site.class == CallPathClass::RenderThread
                && !self.instrumented_site_ids.contains(&site.id)
            {
                out.push(AuditViolation::MissingInstrumentation {
                    site_id: site.id.clone(),
                });
            }
        }
        out
    }

    /// Count of registered sites, by class.
    #[must_use]
    pub fn count_by_class(&self) -> BTreeMap<String, u32> {
        let mut out = BTreeMap::new();
        for site in &self.sites {
            *out.entry(site.class.slug().to_string()).or_insert(0) += 1;
        }
        out
    }
}

// ============================================================================
// Lock-wait instrumentation contract (sub-task 5)
// ============================================================================

/// Typed-state probe — `LockAcquireProbe<Pending>` is
/// what `Instant::now()` produces *before* the lock
/// acquire; consuming it with the post-acquire timestamp
/// yields a `LockAcquireProbe<Recorded>` carrying the
/// `delta_ns` ready for `LockWaitDistribution::record`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pending;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Recorded;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockAcquireProbe<Stage> {
    pub site_id_hash: u64,
    pub before_ns: u64,
    pub after_ns: u64,
    _stage: std::marker::PhantomData<Stage>,
}

impl LockAcquireProbe<Pending> {
    #[must_use]
    pub fn before(site_id_hash: u64, before_ns: u64) -> Self {
        Self {
            site_id_hash,
            before_ns,
            after_ns: 0,
            _stage: std::marker::PhantomData,
        }
    }

    /// Consume the pending probe with the post-acquire
    /// timestamp; produces the `Recorded` typed-state.
    #[must_use]
    pub fn record_after(self, after_ns: u64) -> LockAcquireProbe<Recorded> {
        LockAcquireProbe {
            site_id_hash: self.site_id_hash,
            before_ns: self.before_ns,
            after_ns,
            _stage: std::marker::PhantomData,
        }
    }
}

impl LockAcquireProbe<Recorded> {
    /// Wait time in nanoseconds. Saturates at zero on
    /// clock skew.
    #[must_use]
    pub fn delta_ns(&self) -> u64 {
        self.after_ns.saturating_sub(self.before_ns)
    }
}

// ============================================================================
// JSON-line log row (sub-task 7)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StructuredLogRow {
    /// Per-frame: ts_ns, frame_idx, lock_wait_ns,
    /// snapshot_kind, classification.
    FrameAcquire {
        ts_ns: u64,
        frame_idx: u64,
        lock_wait_ns: u64,
        snapshot_kind_slug: String,
        classification_slug: String,
    },
    /// Per-frame end: ts_ns, frame_idx, render_ns,
    /// over_budget.
    FrameEnd {
        ts_ns: u64,
        frame_idx: u64,
        render_ns: u64,
        over_budget: bool,
    },
    /// Per-session summary: total_frames, p50_ns, p95_ns,
    /// p99_ns, meets_p99_target.
    SessionSummary {
        total_frames: u64,
        p50_ns: u64,
        p95_ns: u64,
        p99_ns: u64,
        meets_p99_target: bool,
    },
}

#[must_use]
pub fn render_log_jsonl(rows: &[StructuredLogRow]) -> String {
    let mut out = String::new();
    for r in rows {
        let line = serde_json::to_string(r).expect("StructuredLogRow always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn parse_log_jsonl(jsonl: &str) -> Result<Vec<StructuredLogRow>, serde_json::Error> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

// ============================================================================
// Migration-readiness rubric
// ============================================================================

/// Inputs for the rubric. The integration populates these
/// from the call-site registry + telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationReadinessInputs {
    pub registry_audit_violations: u32,
    pub render_thread_call_sites: u32,
    pub render_thread_call_sites_instrumented: u32,
    pub lock_wait_p99_meets_target: bool,
    pub e2e_heavy_burst_passing: bool,
    pub visual_regression_passing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MigrationReadinessVerdict {
    /// All gates pass; integration may land.
    Ready,
    /// Some gate failed. Closed list of reasons.
    NotReady { reason: ReadinessBlocker },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessBlocker {
    /// Audit found violations (e.g., misclassified call
    /// site).
    AuditViolations,
    /// Some render-thread site lacks lock-wait
    /// instrumentation.
    PartialInstrumentation,
    /// `meets_p99_target` returned false on telemetry.
    P99TargetMissed,
    /// Heavy-burst E2E test failed.
    HeavyBurstFailing,
    /// Visual regression test failed (output differs from
    /// pre-migration baseline).
    VisualRegressionFailing,
}

#[must_use]
pub fn evaluate_migration_readiness(
    inputs: &MigrationReadinessInputs,
) -> MigrationReadinessVerdict {
    if inputs.registry_audit_violations > 0 {
        return MigrationReadinessVerdict::NotReady {
            reason: ReadinessBlocker::AuditViolations,
        };
    }
    if inputs.render_thread_call_sites_instrumented < inputs.render_thread_call_sites {
        return MigrationReadinessVerdict::NotReady {
            reason: ReadinessBlocker::PartialInstrumentation,
        };
    }
    if !inputs.lock_wait_p99_meets_target {
        return MigrationReadinessVerdict::NotReady {
            reason: ReadinessBlocker::P99TargetMissed,
        };
    }
    if !inputs.e2e_heavy_burst_passing {
        return MigrationReadinessVerdict::NotReady {
            reason: ReadinessBlocker::HeavyBurstFailing,
        };
    }
    if !inputs.visual_regression_passing {
        return MigrationReadinessVerdict::NotReady {
            reason: ReadinessBlocker::VisualRegressionFailing,
        };
    }
    MigrationReadinessVerdict::Ready
}

// ============================================================================
// Health snapshot
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RenderSnapshotAuditHealth {
    pub registered_sites_total: u32,
    pub instrumented_sites_total: u32,
    /// **Cumulative** count of violations ever recorded across
    /// every `record_audit_run` call. Used as a telemetry/trend
    /// signal — does NOT gate `is_safe()`. See
    /// [`Self::last_audit_violations`] for the most-recent run's
    /// count which is the load-bearing safety signal.
    ///
    /// Bug fix per `ft-ftepl` (renamed semantically from gating
    /// to telemetry; `is_safe` no longer consults this field
    /// because monotonic-false breaks recovery from transient
    /// violations).
    pub audit_violations_total: u64,
    /// Violations from the **most recent** `record_audit_run` call.
    /// Resets each run. `is_safe()` uses this — recovers cleanly
    /// when a transient violation is fixed.
    pub last_audit_violations: u64,
    pub last_verdict: Option<String>,
    pub sites_by_class: BTreeMap<String, u32>,
    pub violations_by_kind: BTreeMap<String, u64>,
}

impl RenderSnapshotAuditHealth {
    #[must_use]
    pub fn baseline() -> Self {
        Self::default()
    }

    pub fn record_audit_run(&mut self, registry: &CallSiteRegistry) {
        let violations = registry.audit();
        self.registered_sites_total = registry.sites.len() as u32;
        self.instrumented_sites_total = registry.instrumented_site_ids.len() as u32;
        self.audit_violations_total = self
            .audit_violations_total
            .saturating_add(violations.len() as u64);
        // Per ft-ftepl: track the most-recent run's count
        // separately so `is_safe()` recovers from transient
        // violations once the registry is fixed.
        self.last_audit_violations = violations.len() as u64;
        self.sites_by_class = registry.count_by_class();
        for v in &violations {
            let slug = match v {
                AuditViolation::MutationFromForbiddenClass { .. } => {
                    "mutation_from_forbidden_class"
                }
                AuditViolation::MissingInstrumentation { .. } => "missing_instrumentation",
                AuditViolation::DuplicateSiteId { .. } => "duplicate_site_id",
            };
            *self.violations_by_kind.entry(slug.to_string()).or_insert(0) += 1;
        }
    }

    pub fn record_verdict(&mut self, verdict: &MigrationReadinessVerdict) {
        let slug = match verdict {
            MigrationReadinessVerdict::Ready => "ready".to_string(),
            MigrationReadinessVerdict::NotReady { reason } => {
                format!(
                    "not_ready:{}",
                    match reason {
                        ReadinessBlocker::AuditViolations => "audit",
                        ReadinessBlocker::PartialInstrumentation => "instrumentation",
                        ReadinessBlocker::P99TargetMissed => "p99",
                        ReadinessBlocker::HeavyBurstFailing => "heavy_burst",
                        ReadinessBlocker::VisualRegressionFailing => "visual_regression",
                    }
                )
            }
        };
        self.last_verdict = Some(slug);
    }

    /// True iff the most recent verdict was Ready AND the most
    /// recent audit run produced zero violations.
    ///
    /// Uses `last_audit_violations` (per-run, resets each call to
    /// `record_audit_run`) rather than `audit_violations_total`
    /// (cumulative across runs). Bug fix per `ft-ftepl`: cumulative
    /// gating made `is_safe()` monotonically false after the
    /// first violation, so a transient bad-site registration
    /// would permanently lock out the safety signal even after
    /// the registry was fixed.
    #[must_use]
    pub fn is_safe(&self) -> bool {
        self.last_audit_violations == 0
            && self.last_verdict.as_deref().is_some_and(|s| s == "ready")
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn render_site(id: &str) -> CallSite {
        CallSite {
            id: id.to_string(),
            file: "paint.rs".to_string(),
            line: 1,
            class: CallPathClass::RenderThread,
            acquires_mutation: false,
        }
    }

    fn input_site(id: &str) -> CallSite {
        CallSite {
            id: id.to_string(),
            file: "input.rs".to_string(),
            line: 1,
            class: CallPathClass::InputThread,
            acquires_mutation: true,
        }
    }

    // ------------------------------------------------------------------------
    // Static-audit invariant
    // ------------------------------------------------------------------------

    #[test]
    fn render_thread_class_cannot_acquire_mutation() {
        assert!(!CallPathClass::RenderThread.may_acquire_mutation_kind());
    }

    #[test]
    fn input_thread_class_can_acquire_mutation() {
        assert!(CallPathClass::InputThread.may_acquire_mutation_kind());
    }

    #[test]
    fn a11y_thread_class_cannot_acquire_mutation() {
        assert!(!CallPathClass::A11yThread.may_acquire_mutation_kind());
    }

    #[test]
    fn dirty_line_reader_class_cannot_acquire_mutation() {
        assert!(!CallPathClass::DirtyLineReader.may_acquire_mutation_kind());
    }

    #[test]
    fn well_formed_render_site_validates() {
        let site = render_site("draw_frame");
        assert!(site.validate().is_ok());
    }

    #[test]
    fn render_site_acquiring_mutation_fails_validation() {
        let mut site = render_site("draw_frame");
        site.acquires_mutation = true; // invariant break
        let err = site.validate().unwrap_err();
        assert!(matches!(
            err,
            AuditViolation::MutationFromForbiddenClass { .. }
        ));
    }

    #[test]
    fn input_site_acquiring_mutation_validates() {
        let site = input_site("ingest_pty");
        assert!(site.validate().is_ok());
    }

    #[test]
    fn registry_audit_catches_misclassification() {
        let mut reg = CallSiteRegistry::new();
        let mut site = render_site("draw_frame");
        site.acquires_mutation = true;
        reg.register(site);
        reg.mark_instrumented("draw_frame");
        let violations = reg.audit();
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, AuditViolation::MutationFromForbiddenClass { .. }))
        );
    }

    #[test]
    fn registry_audit_catches_missing_instrumentation() {
        let mut reg = CallSiteRegistry::new();
        reg.register(render_site("draw_frame"));
        // no mark_instrumented call
        let violations = reg.audit();
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, AuditViolation::MissingInstrumentation { .. }))
        );
    }

    #[test]
    fn registry_audit_catches_duplicate_site_ids() {
        let mut reg = CallSiteRegistry::new();
        reg.register(render_site("draw_frame"));
        reg.register(render_site("draw_frame")); // dup
        reg.mark_instrumented("draw_frame");
        let violations = reg.audit();
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, AuditViolation::DuplicateSiteId { .. }))
        );
    }

    #[test]
    fn registry_audit_clean_when_well_formed() {
        let mut reg = CallSiteRegistry::new();
        reg.register(render_site("draw_frame"));
        reg.register(input_site("ingest_pty"));
        reg.mark_instrumented("draw_frame");
        let violations = reg.audit();
        assert!(violations.is_empty());
    }

    #[test]
    fn count_by_class_returns_per_class_totals() {
        let mut reg = CallSiteRegistry::new();
        reg.register(render_site("a"));
        reg.register(render_site("b"));
        reg.register(input_site("c"));
        let counts = reg.count_by_class();
        assert_eq!(counts.get("render_thread"), Some(&2));
        assert_eq!(counts.get("input_thread"), Some(&1));
    }

    // ------------------------------------------------------------------------
    // Lock-wait probe
    // ------------------------------------------------------------------------

    #[test]
    fn probe_typed_state_round_trip() {
        let pending = LockAcquireProbe::<Pending>::before(0xCAFE, 1_000);
        let recorded = pending.record_after(1_500);
        assert_eq!(recorded.delta_ns(), 500);
    }

    #[test]
    fn probe_clock_skew_saturates_to_zero() {
        let pending = LockAcquireProbe::<Pending>::before(0, 1_500);
        let recorded = pending.record_after(1_000); // before > after
        assert_eq!(recorded.delta_ns(), 0);
    }

    // ------------------------------------------------------------------------
    // JSON-line log
    // ------------------------------------------------------------------------

    #[test]
    fn structured_log_jsonl_roundtrip() {
        let rows = vec![
            StructuredLogRow::FrameAcquire {
                ts_ns: 1_000,
                frame_idx: 1,
                lock_wait_ns: 50,
                snapshot_kind_slug: "read_only".to_string(),
                classification_slug: "fast".to_string(),
            },
            StructuredLogRow::FrameEnd {
                ts_ns: 2_000,
                frame_idx: 1,
                render_ns: 10_000_000,
                over_budget: false,
            },
            StructuredLogRow::SessionSummary {
                total_frames: 1_000,
                p50_ns: 30,
                p95_ns: 60,
                p99_ns: 90,
                meets_p99_target: true,
            },
        ];
        let jsonl = render_log_jsonl(&rows);
        let parsed = parse_log_jsonl(&jsonl).unwrap();
        assert_eq!(parsed, rows);
    }

    // ------------------------------------------------------------------------
    // Migration-readiness rubric
    // ------------------------------------------------------------------------

    fn ready_inputs() -> MigrationReadinessInputs {
        MigrationReadinessInputs {
            registry_audit_violations: 0,
            render_thread_call_sites: 5,
            render_thread_call_sites_instrumented: 5,
            lock_wait_p99_meets_target: true,
            e2e_heavy_burst_passing: true,
            visual_regression_passing: true,
        }
    }

    #[test]
    fn readiness_ready_when_all_gates_pass() {
        let v = evaluate_migration_readiness(&ready_inputs());
        assert_eq!(v, MigrationReadinessVerdict::Ready);
    }

    #[test]
    fn readiness_blocked_by_audit_violations() {
        let mut i = ready_inputs();
        i.registry_audit_violations = 1;
        let v = evaluate_migration_readiness(&i);
        assert_eq!(
            v,
            MigrationReadinessVerdict::NotReady {
                reason: ReadinessBlocker::AuditViolations
            }
        );
    }

    #[test]
    fn readiness_blocked_by_partial_instrumentation() {
        let mut i = ready_inputs();
        i.render_thread_call_sites_instrumented = 4; // 1 missing
        let v = evaluate_migration_readiness(&i);
        assert_eq!(
            v,
            MigrationReadinessVerdict::NotReady {
                reason: ReadinessBlocker::PartialInstrumentation
            }
        );
    }

    #[test]
    fn readiness_blocked_by_p99_miss() {
        let mut i = ready_inputs();
        i.lock_wait_p99_meets_target = false;
        let v = evaluate_migration_readiness(&i);
        assert_eq!(
            v,
            MigrationReadinessVerdict::NotReady {
                reason: ReadinessBlocker::P99TargetMissed
            }
        );
    }

    #[test]
    fn readiness_blocked_by_heavy_burst_fail() {
        let mut i = ready_inputs();
        i.e2e_heavy_burst_passing = false;
        let v = evaluate_migration_readiness(&i);
        assert_eq!(
            v,
            MigrationReadinessVerdict::NotReady {
                reason: ReadinessBlocker::HeavyBurstFailing
            }
        );
    }

    #[test]
    fn readiness_blocked_by_visual_regression() {
        let mut i = ready_inputs();
        i.visual_regression_passing = false;
        let v = evaluate_migration_readiness(&i);
        assert_eq!(
            v,
            MigrationReadinessVerdict::NotReady {
                reason: ReadinessBlocker::VisualRegressionFailing
            }
        );
    }

    // ------------------------------------------------------------------------
    // Health snapshot
    // ------------------------------------------------------------------------

    #[test]
    fn health_baseline_unsafe_until_verdict_recorded() {
        // Default — no verdict yet means we haven't run
        // the migration-readiness check; treat as unsafe.
        let h = RenderSnapshotAuditHealth::baseline();
        assert!(!h.is_safe());
    }

    #[test]
    fn health_records_audit_violations() {
        let mut reg = CallSiteRegistry::new();
        let mut site = render_site("bad");
        site.acquires_mutation = true;
        reg.register(site);
        reg.mark_instrumented("bad");
        let mut h = RenderSnapshotAuditHealth::baseline();
        h.record_audit_run(&reg);
        assert!(h.audit_violations_total > 0);
        assert!(!h.is_safe());
    }

    #[test]
    fn health_safe_after_ready_verdict_with_clean_audit() {
        let mut reg = CallSiteRegistry::new();
        reg.register(render_site("draw_frame"));
        reg.mark_instrumented("draw_frame");
        let mut h = RenderSnapshotAuditHealth::baseline();
        h.record_audit_run(&reg);
        h.record_verdict(&MigrationReadinessVerdict::Ready);
        assert!(h.is_safe());
    }

    // ------------------------------------------------------------------------
    // Headline scenario
    // ------------------------------------------------------------------------

    #[test]
    fn migration_complete_scenario() {
        // Bead's headline: render thread migrated to
        // triple_buffer.read(); audit clean; p99 met;
        // E2E + visual pass → Ready verdict.
        let mut reg = CallSiteRegistry::new();
        for site_id in ["paint::draw_frame", "paint::draw_overlay", "a11y::announce"] {
            let class = if site_id.starts_with("a11y") {
                CallPathClass::A11yThread
            } else {
                CallPathClass::RenderThread
            };
            reg.register(CallSite {
                id: site_id.to_string(),
                file: "x.rs".to_string(),
                line: 1,
                class,
                acquires_mutation: false,
            });
            if class == CallPathClass::RenderThread {
                reg.mark_instrumented(site_id);
            }
        }
        let violations = reg.audit();
        assert!(violations.is_empty());

        let inputs = MigrationReadinessInputs {
            registry_audit_violations: violations.len() as u32,
            render_thread_call_sites: 2,
            render_thread_call_sites_instrumented: 2,
            lock_wait_p99_meets_target: true,
            e2e_heavy_burst_passing: true,
            visual_regression_passing: true,
        };
        let verdict = evaluate_migration_readiness(&inputs);
        assert_eq!(verdict, MigrationReadinessVerdict::Ready);
    }

    /// ft-ftepl regression guard: `is_safe()` must recover from
    /// a transient violation once the registry is fixed and
    /// re-audited. Before the fix, `audit_violations_total` was
    /// cumulative and monotonically false-locked the signal.
    #[test]
    fn is_safe_recovers_after_transient_violation_is_fixed() {
        // Step 1: register a misclassified site (RenderThread
        // acquiring Mutation guard) — first audit run records
        // 1 violation.
        let mut reg = CallSiteRegistry::new();
        let mut bad = render_site("draw_frame");
        bad.acquires_mutation = true; // forbidden for RenderThread
        reg.register(bad);
        reg.mark_instrumented("draw_frame");

        let mut h = RenderSnapshotAuditHealth::baseline();
        h.record_audit_run(&reg);
        h.record_verdict(&MigrationReadinessVerdict::Ready);
        // First audit caught the violation.
        assert_eq!(h.last_audit_violations, 1);
        assert!(
            !h.is_safe(),
            "is_safe() must be false while violation present"
        );

        // Step 2: fix the registry — replace the misclassified
        // site with a clean one. The sequence below mirrors what
        // an integration would do after detecting the violation:
        // wipe the registry + re-register clean sites.
        let mut clean_reg = CallSiteRegistry::new();
        clean_reg.register(render_site("draw_frame"));
        clean_reg.mark_instrumented("draw_frame");

        // Step 3: re-record the audit on the clean registry.
        h.record_audit_run(&clean_reg);

        // Step 4: is_safe() must now recover.
        assert_eq!(
            h.last_audit_violations, 0,
            "fixed registry must produce 0 last-run violations"
        );
        // audit_violations_total stays cumulative for telemetry.
        assert!(
            h.audit_violations_total >= 1,
            "audit_violations_total preserved as cumulative trend signal"
        );
        assert!(
            h.is_safe(),
            "is_safe() must recover after the violation is fixed; \
             prior bug locked it false-monotonic"
        );
    }

    #[test]
    fn last_audit_violations_resets_each_run_independently() {
        // Direct guard on the per-run reset semantic: two
        // back-to-back runs with different registries produce
        // independent counts, not a sum.
        let mut h = RenderSnapshotAuditHealth::baseline();

        let mut bad_reg = CallSiteRegistry::new();
        bad_reg.register({
            let mut s = render_site("a");
            s.acquires_mutation = true;
            s
        });
        bad_reg.mark_instrumented("a");

        h.record_audit_run(&bad_reg);
        assert_eq!(h.last_audit_violations, 1);

        let mut clean_reg = CallSiteRegistry::new();
        clean_reg.register(render_site("a"));
        clean_reg.mark_instrumented("a");
        h.record_audit_run(&clean_reg);
        assert_eq!(
            h.last_audit_violations, 0,
            "second run resets last-run count"
        );
    }

    #[test]
    fn registry_serde_roundtrip() {
        let mut reg = CallSiteRegistry::new();
        reg.register(render_site("draw"));
        reg.register(input_site("ingest"));
        reg.mark_instrumented("draw");
        let json = serde_json::to_string(&reg).unwrap();
        let parsed: CallSiteRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, reg);
    }
}
