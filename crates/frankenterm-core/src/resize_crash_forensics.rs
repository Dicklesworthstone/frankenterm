//! Structured crash forensics for resize/reflow paths.
//!
//! This module captures actionable crash context when a panic occurs in
//! resize or reflow code paths (`wa-1u90p.6.4`).  It records:
//!
//! - In-flight resize transaction state (pane ID, intent seq, phase)
//! - Queue depths (pending/active counts, input backlog)
//! - Per-pane metadata (domain, tab, work class, timing)
//! - Policy decisions (storm detection, domain throttle, starvation bypass)
//!
//! The [`ResizeCrashContext`] is maintained as a process-global singleton
//! (via `OnceLock<RwLock<..>>`) and is included in crash bundles written
//! by [`crate::crash::write_crash_bundle`].

use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::resize_scheduler::{
    ResizeControlPlaneGateState, ResizeDomain, ResizeExecutionPhase, ResizeWorkClass,
};

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

static GLOBAL_RESIZE_CRASH_CTX: OnceLock<RwLock<Option<ResizeCrashContext>>> = OnceLock::new();

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// Snapshot of in-flight resize state captured for crash diagnostics.
///
/// Updated by the scheduler on every frame and on transaction lifecycle
/// transitions so that the latest state is always available when a panic
/// occurs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeCrashContext {
    /// Epoch-ms when this context was last refreshed.
    pub captured_at_ms: u64,
    /// Gate state at capture time.
    pub gate: ResizeControlPlaneGateState,
    /// Aggregate queue depths.
    pub queue_depths: ResizeQueueDepths,
    /// Per-pane in-flight transaction state.
    pub in_flight: Vec<InFlightTransaction>,
    /// Recent policy decisions that affected scheduling.
    pub policy_decisions: Vec<PolicyDecision>,
    /// Storm detection state at capture time.
    pub storm_state: StormState,
    /// Per-domain budget accounting at capture time.
    pub domain_budgets: Vec<DomainBudgetEntry>,
}

impl ResizeCrashContext {
    /// Update the process-global crash context.
    pub fn update_global(ctx: Self) {
        let lock = GLOBAL_RESIZE_CRASH_CTX.get_or_init(|| RwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = Some(ctx);
        }
    }

    /// Retrieve the current process-global crash context.
    #[must_use]
    pub fn get_global() -> Option<Self> {
        let lock = GLOBAL_RESIZE_CRASH_CTX.get_or_init(|| RwLock::new(None));
        lock.read().ok().and_then(|guard| guard.clone())
    }

    /// Clear the process-global crash context (useful in tests).
    pub fn clear_global() {
        let lock = GLOBAL_RESIZE_CRASH_CTX.get_or_init(|| RwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = None;
        }
    }

    /// Produce a compact one-line summary for structured log emission.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let in_flight_count = self.in_flight.len();
        let storm_tabs = self.storm_state.tabs_in_storm;
        let decisions = self.policy_decisions.len();
        format!(
            "resize_crash_ctx captured_at={} pending={} active={} in_flight={} storm_tabs={} decisions={}",
            self.captured_at_ms,
            self.queue_depths.pending_intents,
            self.queue_depths.active_transactions,
            in_flight_count,
            storm_tabs,
            decisions,
        )
    }
}

/// Aggregate queue depth counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResizeQueueDepths {
    /// Number of pending resize intents queued for scheduling.
    pub pending_intents: u32,
    /// Number of currently active (executing) resize transactions.
    pub active_transactions: u32,
    /// Current input event backlog depth (from input guardrail).
    pub input_backlog: u32,
    /// Total panes tracked by the scheduler.
    pub tracked_panes: u32,
    /// Frame budget (work units) configured for current frame.
    pub frame_budget_units: u32,
    /// Work units consumed in the last completed frame.
    pub last_frame_spent_units: u32,
}

/// State of a single in-flight resize transaction at capture time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InFlightTransaction {
    /// Pane that owns this transaction.
    pub pane_id: u64,
    /// Intent sequence number.
    pub intent_seq: u64,
    /// Work class of this intent.
    pub work_class: ResizeWorkClass,
    /// Current execution phase (if active).
    pub phase: Option<ResizeExecutionPhase>,
    /// Phase entry timestamp (epoch ms).
    pub phase_started_at_ms: Option<u64>,
    /// Domain classification.
    pub domain: ResizeDomain,
    /// Tab grouping ID, if known.
    pub tab_id: Option<u64>,
    /// Consecutive deferrals accumulated so far.
    pub deferrals: u32,
    /// Whether this transaction was force-served by starvation protection.
    pub force_served: bool,
}

/// A recent policy decision that influenced scheduling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecision {
    /// Epoch-ms when the decision was recorded.
    pub at_ms: u64,
    /// Kind of policy decision.
    pub kind: PolicyDecisionKind,
    /// Affected pane, if any.
    pub pane_id: Option<u64>,
    /// Human-readable rationale.
    pub rationale: String,
}

/// Enumeration of policy decision kinds for forensic analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecisionKind {
    /// Pick was throttled because the tab is in storm mode.
    StormThrottle,
    /// Pick was throttled by per-domain budget cap.
    DomainBudgetThrottle,
    /// Background work was force-served due to starvation protection.
    StarvationBypass,
    /// Intent was rejected due to overload admission control.
    OverloadReject,
    /// Pending intent was evicted due to overload policy.
    OverloadEvict,
    /// Input guardrails reserved frame budget from resize.
    InputGuardrailActivated,
    /// Intent was suppressed by the control-plane gate/kill-switch.
    GateSuppressed,
}

/// Storm detection state snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StormState {
    /// Number of distinct tabs currently exceeding the storm threshold.
    pub tabs_in_storm: u32,
    /// Configured storm window duration (ms).
    pub storm_window_ms: u64,
    /// Configured storm threshold (intents per window).
    pub storm_threshold: u32,
    /// Total storm events detected since process start.
    pub total_storm_events: u64,
    /// Total picks throttled by storm logic since process start.
    pub total_storm_throttled: u64,
}

/// Per-domain budget accounting entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainBudgetEntry {
    /// Domain key (e.g. "local", "ssh:host", "mux:endpoint").
    pub domain_key: String,
    /// Budget weight for this domain.
    pub weight: u32,
    /// Allocated budget share (work units) this frame.
    pub allocated_units: u32,
    /// Work units consumed this frame.
    pub consumed_units: u32,
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Incremental builder for constructing a [`ResizeCrashContext`].
///
/// Callers can populate fields as data becomes available during a
/// scheduling frame, then finalize into a `ResizeCrashContext` and
/// push it to the global slot.
#[derive(Debug, Default)]
pub struct ResizeCrashContextBuilder {
    captured_at_ms: u64,
    gate: Option<ResizeControlPlaneGateState>,
    queue_depths: ResizeQueueDepths,
    in_flight: Vec<InFlightTransaction>,
    policy_decisions: Vec<PolicyDecision>,
    storm_state: StormState,
    domain_budgets: Vec<DomainBudgetEntry>,
}

/// Maximum policy decisions retained in the crash context to bound memory.
const MAX_POLICY_DECISIONS: usize = 64;

impl ResizeCrashContextBuilder {
    /// Create a new builder timestamped at `captured_at_ms`.
    #[must_use]
    pub fn new(captured_at_ms: u64) -> Self {
        Self {
            captured_at_ms,
            ..Default::default()
        }
    }

    /// Set the control-plane gate state.
    #[must_use]
    pub fn gate(mut self, gate: ResizeControlPlaneGateState) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Set aggregate queue depths.
    #[must_use]
    pub fn queue_depths(mut self, depths: ResizeQueueDepths) -> Self {
        self.queue_depths = depths;
        self
    }

    /// Add an in-flight transaction snapshot.
    #[must_use]
    pub fn add_in_flight(mut self, txn: InFlightTransaction) -> Self {
        self.in_flight.push(txn);
        self
    }

    /// Record a policy decision.  Oldest decisions are evicted when
    /// the buffer exceeds [`MAX_POLICY_DECISIONS`].
    #[must_use]
    pub fn add_policy_decision(mut self, decision: PolicyDecision) -> Self {
        if self.policy_decisions.len() >= MAX_POLICY_DECISIONS {
            self.policy_decisions.remove(0);
        }
        self.policy_decisions.push(decision);
        self
    }

    /// Set storm detection state.
    #[must_use]
    pub fn storm_state(mut self, state: StormState) -> Self {
        self.storm_state = state;
        self
    }

    /// Add a domain budget entry.
    #[must_use]
    pub fn add_domain_budget(mut self, entry: DomainBudgetEntry) -> Self {
        self.domain_budgets.push(entry);
        self
    }

    /// Finalize into a [`ResizeCrashContext`].
    #[must_use]
    pub fn build(self) -> ResizeCrashContext {
        ResizeCrashContext {
            captured_at_ms: self.captured_at_ms,
            gate: self.gate.unwrap_or(ResizeControlPlaneGateState {
                control_plane_enabled: false,
                emergency_disable: false,
                legacy_fallback_enabled: false,
                active: false,
            }),
            queue_depths: self.queue_depths,
            in_flight: self.in_flight,
            policy_decisions: self.policy_decisions,
            storm_state: self.storm_state,
            domain_budgets: self.domain_budgets,
        }
    }

    /// Finalize and push to the process-global slot.
    pub fn build_and_update_global(self) {
        ResizeCrashContext::update_global(self.build());
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_gate() -> ResizeControlPlaneGateState {
        ResizeControlPlaneGateState {
            control_plane_enabled: true,
            emergency_disable: false,
            legacy_fallback_enabled: true,
            active: true,
        }
    }

    fn sample_in_flight() -> InFlightTransaction {
        InFlightTransaction {
            pane_id: 42,
            intent_seq: 7,
            work_class: ResizeWorkClass::Interactive,
            phase: Some(ResizeExecutionPhase::Reflowing),
            phase_started_at_ms: Some(1000),
            domain: ResizeDomain::Local,
            tab_id: Some(1),
            deferrals: 0,
            force_served: false,
        }
    }

    #[test]
    fn global_round_trip() {
        // Verify the update→get→clear cycle.  Because tests share a
        // single process and the OnceLock is static, we cannot rely on
        // exact value matching (another test may interleave writes).
        // Instead we verify structural properties.
        let ctx = ResizeCrashContextBuilder::new(12345)
            .gate(sample_gate())
            .build();
        ResizeCrashContext::update_global(ctx);

        let got = ResizeCrashContext::get_global();
        assert!(got.is_some(), "global should be Some after update");

        ResizeCrashContext::clear_global();
        assert!(
            ResizeCrashContext::get_global().is_none(),
            "global should be None after clear"
        );
    }

    #[test]
    fn builder_populates_all_fields() {
        let ctx = ResizeCrashContextBuilder::new(9999)
            .gate(sample_gate())
            .queue_depths(ResizeQueueDepths {
                pending_intents: 3,
                active_transactions: 1,
                input_backlog: 5,
                tracked_panes: 10,
                frame_budget_units: 8,
                last_frame_spent_units: 6,
            })
            .add_in_flight(sample_in_flight())
            .add_policy_decision(PolicyDecision {
                at_ms: 9998,
                kind: PolicyDecisionKind::StormThrottle,
                pane_id: Some(42),
                rationale: "tab 1 in storm".into(),
            })
            .storm_state(StormState {
                tabs_in_storm: 1,
                storm_window_ms: 50,
                storm_threshold: 4,
                total_storm_events: 3,
                total_storm_throttled: 1,
            })
            .add_domain_budget(DomainBudgetEntry {
                domain_key: "local".into(),
                weight: 4,
                allocated_units: 6,
                consumed_units: 4,
            })
            .build();
        assert_eq!(ctx.captured_at_ms, 9999);
        assert_eq!(ctx.queue_depths.pending_intents, 3);
        assert_eq!(ctx.in_flight.len(), 1);
        assert_eq!(ctx.in_flight[0].pane_id, 42);
        assert_eq!(ctx.policy_decisions.len(), 1);
        assert_eq!(
            ctx.policy_decisions[0].kind,
            PolicyDecisionKind::StormThrottle
        );
        assert_eq!(ctx.storm_state.tabs_in_storm, 1);
        assert_eq!(ctx.domain_budgets.len(), 1);
        assert_eq!(ctx.domain_budgets[0].consumed_units, 4);
    }

    #[test]
    fn policy_decisions_bounded() {
        let mut builder = ResizeCrashContextBuilder::new(1000);
        for i in 0..(MAX_POLICY_DECISIONS + 10) {
            builder = builder.add_policy_decision(PolicyDecision {
                at_ms: i as u64,
                kind: PolicyDecisionKind::DomainBudgetThrottle,
                pane_id: None,
                rationale: format!("entry {i}"),
            });
        }
        let ctx = builder.build();
        assert_eq!(ctx.policy_decisions.len(), MAX_POLICY_DECISIONS);
        // Oldest entries should have been evicted; latest should be present.
        let last = ctx.policy_decisions.last().unwrap();
        assert_eq!(last.at_ms, (MAX_POLICY_DECISIONS + 10 - 1) as u64);
    }

    #[test]
    fn summary_line_format() {
        let ctx = ResizeCrashContextBuilder::new(5000)
            .gate(sample_gate())
            .queue_depths(ResizeQueueDepths {
                pending_intents: 2,
                active_transactions: 1,
                input_backlog: 0,
                tracked_panes: 8,
                frame_budget_units: 8,
                last_frame_spent_units: 3,
            })
            .build();

        let line = ctx.summary_line();
        assert!(line.contains("captured_at=5000"));
        assert!(line.contains("pending=2"));
        assert!(line.contains("active=1"));
        assert!(line.contains("in_flight=0"));
        assert!(line.contains("storm_tabs=0"));
    }

    #[test]
    fn build_and_update_global_works() {
        // Verify that build_and_update_global writes to the global slot.
        // We cannot assert exact values because parallel tests share the
        // global OnceLock; just confirm the write-then-read path works.
        ResizeCrashContextBuilder::new(7777)
            .gate(sample_gate())
            .build_and_update_global();

        let got = ResizeCrashContext::get_global();
        assert!(
            got.is_some(),
            "global should be set after build_and_update_global"
        );
    }

    #[test]
    fn empty_builder_produces_valid_context() {
        let ctx = ResizeCrashContextBuilder::new(0).build();
        assert_eq!(ctx.captured_at_ms, 0);
        assert!(!ctx.gate.active);
        assert!(ctx.in_flight.is_empty());
        assert!(ctx.policy_decisions.is_empty());
        assert!(ctx.domain_budgets.is_empty());
        assert_eq!(ctx.queue_depths.pending_intents, 0);
        assert_eq!(ctx.storm_state.tabs_in_storm, 0);
    }

    #[test]
    fn multiple_in_flight_transactions() {
        let mut builder = ResizeCrashContextBuilder::new(2000);
        for i in 0..5 {
            builder = builder.add_in_flight(InFlightTransaction {
                pane_id: i,
                intent_seq: i * 10,
                work_class: if i % 2 == 0 {
                    ResizeWorkClass::Interactive
                } else {
                    ResizeWorkClass::Background
                },
                phase: Some(ResizeExecutionPhase::Reflowing),
                phase_started_at_ms: Some(1900 + i),
                domain: ResizeDomain::Local,
                tab_id: Some(i / 2),
                deferrals: i as u32,
                force_served: i == 4,
            });
        }

        let ctx = builder.build();
        assert_eq!(ctx.in_flight.len(), 5);
        assert!(ctx.in_flight[4].force_served);
        assert!(!ctx.in_flight[0].force_served);
        assert_eq!(ctx.in_flight[2].deferrals, 2);
    }

    #[test]
    fn domain_budget_entries() {
        let ctx = ResizeCrashContextBuilder::new(3000)
            .add_domain_budget(DomainBudgetEntry {
                domain_key: "local".into(),
                weight: 4,
                allocated_units: 6,
                consumed_units: 5,
            })
            .add_domain_budget(DomainBudgetEntry {
                domain_key: "ssh:host-a".into(),
                weight: 2,
                allocated_units: 3,
                consumed_units: 0,
            })
            .add_domain_budget(DomainBudgetEntry {
                domain_key: "mux:endpoint-1".into(),
                weight: 1,
                allocated_units: 1,
                consumed_units: 1,
            })
            .build();
        assert_eq!(ctx.domain_budgets.len(), 3);
        let total_weight: u32 = ctx.domain_budgets.iter().map(|d| d.weight).sum();
        assert_eq!(total_weight, 7);
    }

    #[test]
    fn serialization_round_trip() {
        let ctx = ResizeCrashContextBuilder::new(4000)
            .gate(sample_gate())
            .queue_depths(ResizeQueueDepths {
                pending_intents: 1,
                active_transactions: 2,
                input_backlog: 3,
                tracked_panes: 4,
                frame_budget_units: 8,
                last_frame_spent_units: 5,
            })
            .storm_state(StormState {
                tabs_in_storm: 2,
                storm_window_ms: 100,
                storm_threshold: 8,
                total_storm_events: 10,
                total_storm_throttled: 3,
            })
            .build();

        let json = serde_json::to_string(&ctx).expect("serialize");
        let deserialized: ResizeCrashContext = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized, ctx);
    }

    #[test]
    fn all_policy_decision_kinds_serialize() {
        let kinds = [
            PolicyDecisionKind::StormThrottle,
            PolicyDecisionKind::DomainBudgetThrottle,
            PolicyDecisionKind::StarvationBypass,
            PolicyDecisionKind::OverloadReject,
            PolicyDecisionKind::OverloadEvict,
            PolicyDecisionKind::InputGuardrailActivated,
            PolicyDecisionKind::GateSuppressed,
        ];

        for kind in kinds {
            let decision = PolicyDecision {
                at_ms: 1,
                kind,
                pane_id: None,
                rationale: "test".into(),
            };
            let json = serde_json::to_string(&decision).expect("serialize");
            let rt: PolicyDecision = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(rt.kind, kind);
        }
    }

    #[test]
    fn in_flight_with_ssh_domain() {
        let txn = InFlightTransaction {
            pane_id: 99,
            intent_seq: 1,
            work_class: ResizeWorkClass::Background,
            phase: None,
            phase_started_at_ms: None,
            domain: ResizeDomain::Ssh {
                host: "remote-host".into(),
            },
            tab_id: None,
            deferrals: 5,
            force_served: true,
        };

        let json = serde_json::to_string(&txn).expect("serialize");
        let rt: InFlightTransaction = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            rt.domain,
            ResizeDomain::Ssh {
                host: "remote-host".into(),
            }
        );
        assert!(rt.force_served);
    }

    #[test]
    fn queue_depths_default() {
        let d = ResizeQueueDepths::default();
        assert_eq!(d.pending_intents, 0);
        assert_eq!(d.active_transactions, 0);
        assert_eq!(d.input_backlog, 0);
        assert_eq!(d.tracked_panes, 0);
        assert_eq!(d.frame_budget_units, 0);
        assert_eq!(d.last_frame_spent_units, 0);
    }

    #[test]
    fn storm_state_default() {
        let s = StormState::default();
        assert_eq!(s.tabs_in_storm, 0);
        assert_eq!(s.storm_window_ms, 0);
        assert_eq!(s.storm_threshold, 0);
        assert_eq!(s.total_storm_events, 0);
        assert_eq!(s.total_storm_throttled, 0);
    }
}
