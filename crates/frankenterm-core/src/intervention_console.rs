//! Live intervention and approval console for operator control (ft-3681t.9.5).
//!
//! Unified intervention surface for pause/resume, manual takeover, approval
//! queue management, quarantine actions, and emergency controls. All actions
//! produce audit records for forensic review.
//!
//! # Architecture
//!
//! ```text
//! Operator ──► InterventionConsole
//!                     │
//!       ┌─────────────┼──────────────┐
//!       ▼             ▼              ▼
//!   PaneControl   ApprovalQueue   EmergencyPanel
//!       │             │              │
//!       └─────────────┼──────────────┘
//!                     ▼
//!               AuditTrail
//! ```
//!
//! # Bead
//!
//! Implements ft-3681t.9.5 — live intervention and approval console.

use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// =============================================================================
// Pane control state
// =============================================================================

/// Operational state of a pane under operator control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneControlState {
    /// Normal operation — agent is active.
    Active,
    /// Paused by operator — agent output buffered but not acted on.
    Paused,
    /// Manual takeover — operator has exclusive control.
    ManualTakeover,
    /// Quarantined — all I/O blocked pending review.
    Quarantined,
}

impl Default for PaneControlState {
    fn default() -> Self {
        Self::Active
    }
}

impl PaneControlState {
    /// Whether the agent is allowed to execute actions.
    pub fn agent_can_act(self) -> bool {
        self == Self::Active
    }
}

// =============================================================================
// Intervention action
// =============================================================================

/// An intervention action an operator can take.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum InterventionAction {
    /// Pause a pane (buffer agent I/O).
    PausePane { pane_id: u64 },
    /// Resume a paused pane.
    ResumePane { pane_id: u64 },
    /// Take manual control of a pane.
    TakeoverPane { pane_id: u64 },
    /// Release manual control back to agent.
    ReleaseTakeover { pane_id: u64 },
    /// Quarantine a pane (block all I/O).
    QuarantinePane { pane_id: u64, reason: String },
    /// Release a quarantined pane.
    ReleaseQuarantine { pane_id: u64 },
    /// Approve a pending approval request.
    ApproveRequest { request_id: u64 },
    /// Reject a pending approval request.
    RejectRequest { request_id: u64, reason: String },
    /// Trip the emergency kill switch.
    EmergencyStop { scope: EmergencyScope },
    /// Release the emergency stop.
    ReleaseEmergencyStop,
}

/// Scope of an emergency stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmergencyScope {
    /// Stop all agent activity across all panes.
    Global,
    /// Stop activity for a specific pane.
    Pane(u64),
}

// =============================================================================
// Intervention result
// =============================================================================

/// Result of executing an intervention action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterventionResult {
    /// Whether the action succeeded.
    pub success: bool,
    /// Human-readable description of what happened.
    pub message: String,
    /// Previous state (if a state change occurred).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_state: Option<PaneControlState>,
    /// New state (if a state change occurred).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_state: Option<PaneControlState>,
}

// =============================================================================
// Approval queue
// =============================================================================

/// A pending approval request in the queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApproval {
    /// Unique request ID.
    pub request_id: u64,
    /// Pane that requested approval.
    pub pane_id: u64,
    /// Description of what is being requested.
    pub description: String,
    /// Severity/risk level.
    pub risk_level: RiskLevel,
    /// When the request was created (epoch ms).
    pub created_at_ms: u64,
    /// Time-to-live in ms (0 = no expiry).
    pub ttl_ms: u64,
    /// Current status.
    pub status: ApprovalStatus,
}

/// Risk level for an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

/// Status of an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
}

impl PendingApproval {
    /// Check if this request has expired.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.ttl_ms > 0 && now_ms >= self.created_at_ms + self.ttl_ms
    }
}

// =============================================================================
// Audit record
// =============================================================================

/// Audit record for an intervention action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterventionAuditRecord {
    /// When the action was taken (epoch ms).
    pub timestamp_ms: u64,
    /// Operator identity.
    pub operator: String,
    /// The action taken.
    pub action: InterventionAction,
    /// Result of the action.
    pub result: InterventionResult,
    /// Sequence number for ordering.
    pub sequence: u64,
}

// =============================================================================
// Console snapshot
// =============================================================================

/// Serializable snapshot of the intervention console state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterventionConsoleSnapshot {
    pub pane_states: HashMap<u64, PaneControlState>,
    pub pending_approvals: usize,
    pub total_approvals_processed: u64,
    pub emergency_stop_active: bool,
    pub emergency_scope: Option<EmergencyScope>,
    pub audit_log_size: usize,
    pub captured_at_ms: u64,
}

// =============================================================================
// Console
// =============================================================================

/// The live intervention console.
///
/// Manages pane control states, approval queues, emergency controls,
/// and an append-only audit trail.
pub struct InterventionConsole {
    /// Per-pane control states.
    pane_states: HashMap<u64, PaneControlState>,
    /// Pending approval queue (FIFO).
    approval_queue: VecDeque<PendingApproval>,
    /// Next approval request ID.
    next_request_id: u64,
    /// Whether emergency stop is active.
    emergency_stop: bool,
    /// Scope of the emergency stop.
    emergency_scope: Option<EmergencyScope>,
    /// Audit trail.
    audit_log: Vec<InterventionAuditRecord>,
    /// Next audit sequence number.
    audit_sequence: u64,
    /// Max audit log entries to retain.
    max_audit_entries: usize,
    /// Counters.
    total_approvals_processed: u64,
}

impl InterventionConsole {
    /// Create a new intervention console.
    pub fn new() -> Self {
        Self {
            pane_states: HashMap::new(),
            approval_queue: VecDeque::new(),
            next_request_id: 1,
            emergency_stop: false,
            emergency_scope: None,
            audit_log: Vec::new(),
            audit_sequence: 0,
            max_audit_entries: 10_000,
            total_approvals_processed: 0,
        }
    }

    /// Execute an intervention action.
    pub fn execute(
        &mut self,
        operator: impl Into<String>,
        action: InterventionAction,
    ) -> InterventionResult {
        let operator = operator.into();
        let now_ms = epoch_ms();

        let result = match &action {
            InterventionAction::PausePane { pane_id } => {
                self.set_pane_state(*pane_id, PaneControlState::Paused, now_ms)
            }
            InterventionAction::ResumePane { pane_id } => {
                let state = self.pane_state(*pane_id);
                if state == PaneControlState::Paused {
                    self.set_pane_state(*pane_id, PaneControlState::Active, now_ms)
                } else {
                    InterventionResult {
                        success: false,
                        message: format!(
                            "pane {} is {:?}, not Paused — cannot resume",
                            pane_id, state
                        ),
                        previous_state: Some(state),
                        new_state: None,
                    }
                }
            }
            InterventionAction::TakeoverPane { pane_id } => {
                self.set_pane_state(*pane_id, PaneControlState::ManualTakeover, now_ms)
            }
            InterventionAction::ReleaseTakeover { pane_id } => {
                let state = self.pane_state(*pane_id);
                if state == PaneControlState::ManualTakeover {
                    self.set_pane_state(*pane_id, PaneControlState::Active, now_ms)
                } else {
                    InterventionResult {
                        success: false,
                        message: format!(
                            "pane {} is {:?}, not ManualTakeover — cannot release",
                            pane_id, state
                        ),
                        previous_state: Some(state),
                        new_state: None,
                    }
                }
            }
            InterventionAction::QuarantinePane { pane_id, .. } => {
                self.set_pane_state(*pane_id, PaneControlState::Quarantined, now_ms)
            }
            InterventionAction::ReleaseQuarantine { pane_id } => {
                let state = self.pane_state(*pane_id);
                if state == PaneControlState::Quarantined {
                    self.set_pane_state(*pane_id, PaneControlState::Active, now_ms)
                } else {
                    InterventionResult {
                        success: false,
                        message: format!(
                            "pane {} is {:?}, not Quarantined — cannot release",
                            pane_id, state
                        ),
                        previous_state: Some(state),
                        new_state: None,
                    }
                }
            }
            InterventionAction::ApproveRequest { request_id } => {
                self.process_approval(*request_id, true, None, now_ms)
            }
            InterventionAction::RejectRequest {
                request_id,
                reason,
            } => self.process_approval(*request_id, false, Some(reason.clone()), now_ms),
            InterventionAction::EmergencyStop { scope } => {
                self.emergency_stop = true;
                self.emergency_scope = Some(*scope);
                // If global, pause all active panes.
                if *scope == EmergencyScope::Global {
                    let pane_ids: Vec<u64> = self.pane_states.keys().copied().collect();
                    for pid in pane_ids {
                        if self.pane_states[&pid] == PaneControlState::Active {
                            self.pane_states.insert(pid, PaneControlState::Paused);
                        }
                    }
                } else if let EmergencyScope::Pane(pid) = scope {
                    self.pane_states.insert(*pid, PaneControlState::Paused);
                }
                InterventionResult {
                    success: true,
                    message: format!("emergency stop activated: {:?}", scope),
                    previous_state: None,
                    new_state: None,
                }
            }
            InterventionAction::ReleaseEmergencyStop => {
                if self.emergency_stop {
                    self.emergency_stop = false;
                    self.emergency_scope = None;
                    InterventionResult {
                        success: true,
                        message: "emergency stop released".into(),
                        previous_state: None,
                        new_state: None,
                    }
                } else {
                    InterventionResult {
                        success: false,
                        message: "no emergency stop active".into(),
                        previous_state: None,
                        new_state: None,
                    }
                }
            }
        };

        // Record to audit trail.
        self.record_audit(&operator, action, &result, now_ms);
        result
    }

    /// Get the control state of a pane.
    pub fn pane_state(&self, pane_id: u64) -> PaneControlState {
        self.pane_states
            .get(&pane_id)
            .copied()
            .unwrap_or_default()
    }

    /// Register a pane for tracking.
    pub fn register_pane(&mut self, pane_id: u64) {
        self.pane_states.entry(pane_id).or_default();
    }

    /// Unregister a pane (pane closed).
    pub fn unregister_pane(&mut self, pane_id: u64) {
        self.pane_states.remove(&pane_id);
    }

    /// Whether the emergency stop is active.
    pub fn is_emergency_stop_active(&self) -> bool {
        self.emergency_stop
    }

    /// Submit an approval request to the queue.
    pub fn submit_approval(
        &mut self,
        pane_id: u64,
        description: impl Into<String>,
        risk_level: RiskLevel,
        ttl_ms: u64,
    ) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.approval_queue.push_back(PendingApproval {
            request_id: id,
            pane_id,
            description: description.into(),
            risk_level,
            created_at_ms: epoch_ms(),
            ttl_ms,
            status: ApprovalStatus::Pending,
        });
        id
    }

    /// Get all pending approval requests (not expired).
    pub fn pending_approvals(&self) -> Vec<&PendingApproval> {
        let now = epoch_ms();
        self.approval_queue
            .iter()
            .filter(|a| a.status == ApprovalStatus::Pending && !a.is_expired(now))
            .collect()
    }

    /// Expire stale approval requests and return count expired.
    pub fn expire_stale_approvals(&mut self) -> usize {
        let now = epoch_ms();
        let mut expired = 0;
        for approval in &mut self.approval_queue {
            if approval.status == ApprovalStatus::Pending && approval.is_expired(now) {
                approval.status = ApprovalStatus::Expired;
                expired += 1;
            }
        }
        expired
    }

    /// Get the audit log.
    pub fn audit_log(&self) -> &[InterventionAuditRecord] {
        &self.audit_log
    }

    /// Number of tracked panes.
    pub fn tracked_pane_count(&self) -> usize {
        self.pane_states.len()
    }

    /// Count panes in each control state.
    pub fn state_counts(&self) -> HashMap<PaneControlState, usize> {
        let mut counts = HashMap::new();
        for state in self.pane_states.values() {
            *counts.entry(*state).or_insert(0) += 1;
        }
        counts
    }

    /// Produce a serializable snapshot.
    pub fn snapshot(&self) -> InterventionConsoleSnapshot {
        InterventionConsoleSnapshot {
            pane_states: self.pane_states.clone(),
            pending_approvals: self.pending_approvals().len(),
            total_approvals_processed: self.total_approvals_processed,
            emergency_stop_active: self.emergency_stop,
            emergency_scope: self.emergency_scope,
            audit_log_size: self.audit_log.len(),
            captured_at_ms: epoch_ms(),
        }
    }

    // -------------------------------------------------------------------------
    // Internal helpers
    // -------------------------------------------------------------------------

    fn set_pane_state(
        &mut self,
        pane_id: u64,
        new_state: PaneControlState,
        _now_ms: u64,
    ) -> InterventionResult {
        let prev = self.pane_states.insert(pane_id, new_state).unwrap_or_default();
        InterventionResult {
            success: true,
            message: format!("pane {} {:?} → {:?}", pane_id, prev, new_state),
            previous_state: Some(prev),
            new_state: Some(new_state),
        }
    }

    fn process_approval(
        &mut self,
        request_id: u64,
        approve: bool,
        reason: Option<String>,
        now_ms: u64,
    ) -> InterventionResult {
        let entry = self
            .approval_queue
            .iter_mut()
            .find(|a| a.request_id == request_id);

        match entry {
            Some(approval) if approval.status == ApprovalStatus::Pending => {
                if approval.is_expired(now_ms) {
                    approval.status = ApprovalStatus::Expired;
                    return InterventionResult {
                        success: false,
                        message: format!("request {} has expired", request_id),
                        previous_state: None,
                        new_state: None,
                    };
                }
                if approve {
                    approval.status = ApprovalStatus::Approved;
                    self.total_approvals_processed += 1;
                    InterventionResult {
                        success: true,
                        message: format!("request {} approved", request_id),
                        previous_state: None,
                        new_state: None,
                    }
                } else {
                    approval.status = ApprovalStatus::Rejected;
                    self.total_approvals_processed += 1;
                    InterventionResult {
                        success: true,
                        message: format!(
                            "request {} rejected: {}",
                            request_id,
                            reason.as_deref().unwrap_or("no reason given")
                        ),
                        previous_state: None,
                        new_state: None,
                    }
                }
            }
            Some(approval) => InterventionResult {
                success: false,
                message: format!(
                    "request {} is {:?}, not Pending",
                    request_id, approval.status
                ),
                previous_state: None,
                new_state: None,
            },
            None => InterventionResult {
                success: false,
                message: format!("request {} not found", request_id),
                previous_state: None,
                new_state: None,
            },
        }
    }

    fn record_audit(
        &mut self,
        operator: &str,
        action: InterventionAction,
        result: &InterventionResult,
        now_ms: u64,
    ) {
        self.audit_sequence += 1;
        self.audit_log.push(InterventionAuditRecord {
            timestamp_ms: now_ms,
            operator: operator.to_string(),
            action,
            result: result.clone(),
            sequence: self.audit_sequence,
        });
        if self.audit_log.len() > self.max_audit_entries {
            let excess = self.audit_log.len() - self.max_audit_entries;
            self.audit_log.drain(..excess);
        }
    }
}

impl Default for InterventionConsole {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Helpers
// =============================================================================

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- PaneControlState --

    #[test]
    fn default_pane_state_is_active() {
        assert_eq!(PaneControlState::default(), PaneControlState::Active);
    }

    #[test]
    fn agent_can_act_only_when_active() {
        assert!(PaneControlState::Active.agent_can_act());
        assert!(!PaneControlState::Paused.agent_can_act());
        assert!(!PaneControlState::ManualTakeover.agent_can_act());
        assert!(!PaneControlState::Quarantined.agent_can_act());
    }

    // -- Pause/Resume --

    #[test]
    fn pause_and_resume_pane() {
        let mut console = InterventionConsole::new();
        console.register_pane(1);

        let r = console.execute("admin", InterventionAction::PausePane { pane_id: 1 });
        assert!(r.success);
        assert_eq!(r.new_state, Some(PaneControlState::Paused));
        assert_eq!(console.pane_state(1), PaneControlState::Paused);

        let r = console.execute("admin", InterventionAction::ResumePane { pane_id: 1 });
        assert!(r.success);
        assert_eq!(r.new_state, Some(PaneControlState::Active));
    }

    #[test]
    fn resume_non_paused_pane_fails() {
        let mut console = InterventionConsole::new();
        console.register_pane(1);
        let r = console.execute("admin", InterventionAction::ResumePane { pane_id: 1 });
        assert!(!r.success);
    }

    // -- Takeover --

    #[test]
    fn takeover_and_release() {
        let mut console = InterventionConsole::new();
        console.register_pane(2);

        let r = console.execute("admin", InterventionAction::TakeoverPane { pane_id: 2 });
        assert!(r.success);
        assert_eq!(console.pane_state(2), PaneControlState::ManualTakeover);
        assert!(!console.pane_state(2).agent_can_act());

        let r = console.execute("admin", InterventionAction::ReleaseTakeover { pane_id: 2 });
        assert!(r.success);
        assert_eq!(console.pane_state(2), PaneControlState::Active);
    }

    #[test]
    fn release_non_takeover_fails() {
        let mut console = InterventionConsole::new();
        console.register_pane(1);
        let r = console.execute("admin", InterventionAction::ReleaseTakeover { pane_id: 1 });
        assert!(!r.success);
    }

    // -- Quarantine --

    #[test]
    fn quarantine_and_release() {
        let mut console = InterventionConsole::new();
        console.register_pane(3);

        let r = console.execute(
            "admin",
            InterventionAction::QuarantinePane {
                pane_id: 3,
                reason: "suspicious output".into(),
            },
        );
        assert!(r.success);
        assert_eq!(console.pane_state(3), PaneControlState::Quarantined);

        let r = console.execute(
            "admin",
            InterventionAction::ReleaseQuarantine { pane_id: 3 },
        );
        assert!(r.success);
        assert_eq!(console.pane_state(3), PaneControlState::Active);
    }

    #[test]
    fn release_non_quarantined_fails() {
        let mut console = InterventionConsole::new();
        console.register_pane(1);
        let r = console.execute(
            "admin",
            InterventionAction::ReleaseQuarantine { pane_id: 1 },
        );
        assert!(!r.success);
    }

    // -- Approval queue --

    #[test]
    fn submit_and_approve() {
        let mut console = InterventionConsole::new();
        let id = console.submit_approval(1, "deploy to prod", RiskLevel::High, 0);
        assert_eq!(console.pending_approvals().len(), 1);

        let r = console.execute("admin", InterventionAction::ApproveRequest { request_id: id });
        assert!(r.success);
        assert_eq!(console.pending_approvals().len(), 0);
        assert_eq!(console.total_approvals_processed, 1);
    }

    #[test]
    fn submit_and_reject() {
        let mut console = InterventionConsole::new();
        let id = console.submit_approval(1, "risky action", RiskLevel::Critical, 0);

        let r = console.execute(
            "admin",
            InterventionAction::RejectRequest {
                request_id: id,
                reason: "too risky".into(),
            },
        );
        assert!(r.success);
        assert!(r.message.contains("rejected"));
        assert_eq!(console.pending_approvals().len(), 0);
    }

    #[test]
    fn approve_nonexistent_request_fails() {
        let mut console = InterventionConsole::new();
        let r = console.execute(
            "admin",
            InterventionAction::ApproveRequest { request_id: 999 },
        );
        assert!(!r.success);
        assert!(r.message.contains("not found"));
    }

    #[test]
    fn approve_already_approved_fails() {
        let mut console = InterventionConsole::new();
        let id = console.submit_approval(1, "action", RiskLevel::Low, 0);
        console.execute("admin", InterventionAction::ApproveRequest { request_id: id });
        // Try to approve again.
        let r = console.execute("admin", InterventionAction::ApproveRequest { request_id: id });
        assert!(!r.success);
        assert!(r.message.contains("Approved"));
    }

    #[test]
    fn pending_approval_expiry() {
        let _console = InterventionConsole::new();
        // TTL of 1ms — will be expired by the time we check.
        let approval = PendingApproval {
            request_id: 1,
            pane_id: 1,
            description: "test".into(),
            risk_level: RiskLevel::Low,
            created_at_ms: 1000,
            ttl_ms: 100,
            status: ApprovalStatus::Pending,
        };
        assert!(approval.is_expired(1200));
        assert!(!approval.is_expired(1050));
    }

    #[test]
    fn risk_level_ordering() {
        assert!(RiskLevel::Low < RiskLevel::Medium);
        assert!(RiskLevel::Medium < RiskLevel::High);
        assert!(RiskLevel::High < RiskLevel::Critical);
    }

    // -- Emergency stop --

    #[test]
    fn global_emergency_stop() {
        let mut console = InterventionConsole::new();
        console.register_pane(1);
        console.register_pane(2);

        let r = console.execute(
            "admin",
            InterventionAction::EmergencyStop {
                scope: EmergencyScope::Global,
            },
        );
        assert!(r.success);
        assert!(console.is_emergency_stop_active());
        // All active panes should be paused.
        assert_eq!(console.pane_state(1), PaneControlState::Paused);
        assert_eq!(console.pane_state(2), PaneControlState::Paused);
    }

    #[test]
    fn pane_scoped_emergency_stop() {
        let mut console = InterventionConsole::new();
        console.register_pane(1);
        console.register_pane(2);

        let r = console.execute(
            "admin",
            InterventionAction::EmergencyStop {
                scope: EmergencyScope::Pane(1),
            },
        );
        assert!(r.success);
        assert_eq!(console.pane_state(1), PaneControlState::Paused);
        assert_eq!(console.pane_state(2), PaneControlState::Active); // Unaffected.
    }

    #[test]
    fn release_emergency_stop() {
        let mut console = InterventionConsole::new();
        console.execute(
            "admin",
            InterventionAction::EmergencyStop {
                scope: EmergencyScope::Global,
            },
        );
        let r = console.execute("admin", InterventionAction::ReleaseEmergencyStop);
        assert!(r.success);
        assert!(!console.is_emergency_stop_active());
    }

    #[test]
    fn release_inactive_emergency_stop_fails() {
        let mut console = InterventionConsole::new();
        let r = console.execute("admin", InterventionAction::ReleaseEmergencyStop);
        assert!(!r.success);
    }

    // -- Audit trail --

    #[test]
    fn audit_trail_records_actions() {
        let mut console = InterventionConsole::new();
        console.register_pane(1);
        console.execute("alice", InterventionAction::PausePane { pane_id: 1 });
        console.execute("bob", InterventionAction::ResumePane { pane_id: 1 });

        assert_eq!(console.audit_log().len(), 2);
        assert_eq!(console.audit_log()[0].operator, "alice");
        assert_eq!(console.audit_log()[1].operator, "bob");
        assert_eq!(console.audit_log()[0].sequence, 1);
        assert_eq!(console.audit_log()[1].sequence, 2);
    }

    #[test]
    fn audit_trail_captures_failures() {
        let mut console = InterventionConsole::new();
        console.register_pane(1);
        // Resume without pause — fails.
        console.execute("admin", InterventionAction::ResumePane { pane_id: 1 });
        assert_eq!(console.audit_log().len(), 1);
        assert!(!console.audit_log()[0].result.success);
    }

    // -- Pane registration --

    #[test]
    fn register_and_unregister_panes() {
        let mut console = InterventionConsole::new();
        console.register_pane(1);
        console.register_pane(2);
        assert_eq!(console.tracked_pane_count(), 2);

        console.unregister_pane(1);
        assert_eq!(console.tracked_pane_count(), 1);
    }

    #[test]
    fn unregistered_pane_defaults_to_active() {
        let console = InterventionConsole::new();
        assert_eq!(console.pane_state(999), PaneControlState::Active);
    }

    // -- State counts --

    #[test]
    fn state_counts_reflect_reality() {
        let mut console = InterventionConsole::new();
        console.register_pane(1);
        console.register_pane(2);
        console.register_pane(3);
        console.execute("admin", InterventionAction::PausePane { pane_id: 1 });
        console.execute("admin", InterventionAction::TakeoverPane { pane_id: 2 });

        let counts = console.state_counts();
        assert_eq!(counts.get(&PaneControlState::Active).copied().unwrap_or(0), 1);
        assert_eq!(counts.get(&PaneControlState::Paused).copied().unwrap_or(0), 1);
        assert_eq!(
            counts
                .get(&PaneControlState::ManualTakeover)
                .copied()
                .unwrap_or(0),
            1
        );
    }

    // -- Snapshot --

    #[test]
    fn snapshot_serde_roundtrip() {
        let mut console = InterventionConsole::new();
        console.register_pane(1);
        console.execute("admin", InterventionAction::PausePane { pane_id: 1 });
        console.submit_approval(1, "test", RiskLevel::Low, 0);

        let snap = console.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let restored: InterventionConsoleSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.pending_approvals, snap.pending_approvals);
        assert_eq!(restored.emergency_stop_active, snap.emergency_stop_active);
    }

    // -- InterventionAction serde --

    #[test]
    fn intervention_action_serde_roundtrip() {
        let actions = vec![
            InterventionAction::PausePane { pane_id: 1 },
            InterventionAction::EmergencyStop {
                scope: EmergencyScope::Global,
            },
            InterventionAction::RejectRequest {
                request_id: 5,
                reason: "nope".into(),
            },
        ];
        let json = serde_json::to_string(&actions).unwrap();
        let restored: Vec<InterventionAction> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), 3);
    }

    // -- Complex scenario: E2E lifecycle --

    #[test]
    fn e2e_intervention_lifecycle() {
        let mut console = InterventionConsole::new();

        // Set up fleet.
        for pane_id in 0..5 {
            console.register_pane(pane_id);
        }

        // Pane 2 behaves suspiciously → quarantine.
        let r = console.execute(
            "operator-1",
            InterventionAction::QuarantinePane {
                pane_id: 2,
                reason: "unexpected rm -rf command".into(),
            },
        );
        assert!(r.success);

        // Operator takes over pane 3 for manual investigation.
        console.execute("operator-1", InterventionAction::TakeoverPane { pane_id: 3 });

        // Agent on pane 1 requests approval for a destructive action.
        let req_id = console.submit_approval(1, "drop database", RiskLevel::Critical, 0);

        // Operator rejects it.
        let r = console.execute(
            "operator-2",
            InterventionAction::RejectRequest {
                request_id: req_id,
                reason: "not during maintenance window".into(),
            },
        );
        assert!(r.success);

        // Situation escalates → global emergency stop.
        console.execute(
            "operator-1",
            InterventionAction::EmergencyStop {
                scope: EmergencyScope::Global,
            },
        );

        // Verify state.
        assert!(console.is_emergency_stop_active());
        let counts = console.state_counts();
        // Panes 0,1,4 were Active → now Paused. Pane 2 was Quarantined (stays).
        // Pane 3 was ManualTakeover (stays, not affected by emergency pause of Active panes).
        assert_eq!(counts.get(&PaneControlState::Paused).copied().unwrap_or(0), 3);
        assert_eq!(
            counts
                .get(&PaneControlState::Quarantined)
                .copied()
                .unwrap_or(0),
            1
        );
        assert_eq!(
            counts
                .get(&PaneControlState::ManualTakeover)
                .copied()
                .unwrap_or(0),
            1
        );

        // Release emergency.
        console.execute("operator-1", InterventionAction::ReleaseEmergencyStop);

        // Full audit trail.
        assert!(console.audit_log().len() >= 5);
        assert_eq!(console.total_approvals_processed, 1); // The rejection.
    }

    // -- Default constructor --

    #[test]
    fn default_impl_same_as_new() {
        let a = InterventionConsole::new();
        let b = InterventionConsole::default();
        assert_eq!(a.tracked_pane_count(), b.tracked_pane_count());
        assert_eq!(a.is_emergency_stop_active(), b.is_emergency_stop_active());
    }
}
