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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneControlState {
    /// Normal operation — agent is active.
    #[default]
    Active,
    /// Paused by operator — agent output buffered but not acted on.
    Paused,
    /// Manual takeover — operator has exclusive control.
    ManualTakeover,
    /// Quarantined — all I/O blocked pending review.
    Quarantined,
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
        self.ttl_ms > 0 && now_ms >= self.created_at_ms.saturating_add(self.ttl_ms)
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
                    self.emergency_activation_block(*pane_id)
                        .unwrap_or_else(|| {
                            self.set_pane_state(*pane_id, PaneControlState::Active, now_ms)
                        })
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
                    self.emergency_activation_block(*pane_id)
                        .unwrap_or_else(|| {
                            self.set_pane_state(*pane_id, PaneControlState::Active, now_ms)
                        })
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
                    self.emergency_activation_block(*pane_id)
                        .unwrap_or_else(|| {
                            self.set_pane_state(*pane_id, PaneControlState::Active, now_ms)
                        })
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
            InterventionAction::RejectRequest { request_id, reason } => {
                self.process_approval(*request_id, false, Some(reason.clone()), now_ms)
            }
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
        self.pane_states.get(&pane_id).copied().unwrap_or_default()
    }

    /// Register a pane for tracking.
    pub fn register_pane(&mut self, pane_id: u64) {
        self.pane_states.entry(pane_id).or_default();
    }

    /// Unregister a pane (pane closed).
    ///
    /// br-ft-fmeic: tie approval lifecycle to pane lifecycle. Any
    /// pending approval requests originating from this pane are
    /// marked Expired so they cannot be acted on after the pane has
    /// closed — a closed pane's approvals are detached from the
    /// context that made them meaningful, and approving them later
    /// would leave the operator with stale capability/evidence-
    /// ledger risk for destructive actions.
    ///
    /// Returns the number of pending approvals expired by this
    /// call (0 if the pane had none).
    pub fn unregister_pane(&mut self, pane_id: u64) -> usize {
        self.pane_states.remove(&pane_id);
        self.expire_approvals_for_pane(pane_id)
    }

    /// Expire all pending approval requests for `pane_id` in place.
    /// br-ft-fmeic: shared helper between unregister_pane and any
    /// future operator-driven "this pane is gone, drop its
    /// approvals" surface (e.g. mux-server pane-closed event
    /// handler).
    fn expire_approvals_for_pane(&mut self, pane_id: u64) -> usize {
        let mut count = 0;
        for approval in &mut self.approval_queue {
            if approval.pane_id == pane_id && approval.status == ApprovalStatus::Pending {
                approval.status = ApprovalStatus::Expired;
                count += 1;
            }
        }
        count
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
        let prev = self
            .pane_states
            .insert(pane_id, new_state)
            .unwrap_or_default();
        InterventionResult {
            success: true,
            message: format!("pane {} {:?} → {:?}", pane_id, prev, new_state),
            previous_state: Some(prev),
            new_state: Some(new_state),
        }
    }

    fn emergency_activation_block(&self, pane_id: u64) -> Option<InterventionResult> {
        if !self.emergency_stop {
            return None;
        }

        match self.emergency_scope {
            Some(EmergencyScope::Global) => Some(self.emergency_activation_block_result(pane_id)),
            Some(EmergencyScope::Pane(blocked_pane)) if blocked_pane == pane_id => {
                Some(self.emergency_activation_block_result(pane_id))
            }
            _ => None,
        }
    }

    fn emergency_activation_block_result(&self, pane_id: u64) -> InterventionResult {
        InterventionResult {
            success: false,
            message: format!(
                "pane {} cannot become Active while emergency stop is active: {:?}",
                pane_id, self.emergency_scope
            ),
            previous_state: Some(self.pane_state(pane_id)),
            new_state: None,
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
    use proptest::{prop_assert, strategy::Strategy};

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

        let r = console.execute(
            "admin",
            InterventionAction::ApproveRequest { request_id: id },
        );
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
        console.execute(
            "admin",
            InterventionAction::ApproveRequest { request_id: id },
        );
        // Try to approve again.
        let r = console.execute(
            "admin",
            InterventionAction::ApproveRequest { request_id: id },
        );
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
    fn pending_approval_unrepresentable_expiry_saturates() {
        let approval = PendingApproval {
            request_id: 1,
            pane_id: 1,
            description: "test".into(),
            risk_level: RiskLevel::Low,
            created_at_ms: u64::MAX - 10,
            ttl_ms: 100,
            status: ApprovalStatus::Pending,
        };

        assert!(!approval.is_expired(u64::MAX - 1));
        assert!(approval.is_expired(u64::MAX));
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

    #[test]
    fn global_emergency_stop_blocks_reactivation_actions() {
        let mut console = InterventionConsole::new();
        console.register_pane(1);
        console.register_pane(2);
        console.register_pane(3);
        console.execute("admin", InterventionAction::PausePane { pane_id: 1 });
        console.execute("admin", InterventionAction::TakeoverPane { pane_id: 2 });
        console.execute(
            "admin",
            InterventionAction::QuarantinePane {
                pane_id: 3,
                reason: "suspicious".into(),
            },
        );
        console.execute(
            "admin",
            InterventionAction::EmergencyStop {
                scope: EmergencyScope::Global,
            },
        );

        let resume = console.execute("admin", InterventionAction::ResumePane { pane_id: 1 });
        assert!(!resume.success);
        assert!(resume.message.contains("emergency stop"));
        assert_eq!(console.pane_state(1), PaneControlState::Paused);

        let takeover_release =
            console.execute("admin", InterventionAction::ReleaseTakeover { pane_id: 2 });
        assert!(!takeover_release.success);
        assert!(takeover_release.message.contains("emergency stop"));
        assert_eq!(console.pane_state(2), PaneControlState::ManualTakeover);

        let quarantine_release = console.execute(
            "admin",
            InterventionAction::ReleaseQuarantine { pane_id: 3 },
        );
        assert!(!quarantine_release.success);
        assert!(quarantine_release.message.contains("emergency stop"));
        assert_eq!(console.pane_state(3), PaneControlState::Quarantined);
    }

    #[test]
    fn pane_scoped_emergency_stop_blocks_only_target_resume() {
        let mut console = InterventionConsole::new();
        console.register_pane(1);
        console.register_pane(2);
        console.execute("admin", InterventionAction::PausePane { pane_id: 1 });
        console.execute("admin", InterventionAction::PausePane { pane_id: 2 });
        console.execute(
            "admin",
            InterventionAction::EmergencyStop {
                scope: EmergencyScope::Pane(1),
            },
        );

        let blocked = console.execute("admin", InterventionAction::ResumePane { pane_id: 1 });
        assert!(!blocked.success);
        assert_eq!(console.pane_state(1), PaneControlState::Paused);

        let allowed = console.execute("admin", InterventionAction::ResumePane { pane_id: 2 });
        assert!(allowed.success);
        assert_eq!(console.pane_state(2), PaneControlState::Active);
    }

    #[test]
    fn pane_scoped_emergency_stop_blocks_only_target_takeover_release() {
        let mut console = InterventionConsole::new();
        console.register_pane(1);
        console.register_pane(2);
        console.execute(
            "admin",
            InterventionAction::EmergencyStop {
                scope: EmergencyScope::Pane(1),
            },
        );
        console.execute("admin", InterventionAction::TakeoverPane { pane_id: 1 });
        console.execute("admin", InterventionAction::TakeoverPane { pane_id: 2 });

        let blocked = console.execute("admin", InterventionAction::ReleaseTakeover { pane_id: 1 });
        assert!(!blocked.success);
        assert_eq!(console.pane_state(1), PaneControlState::ManualTakeover);

        let allowed = console.execute("admin", InterventionAction::ReleaseTakeover { pane_id: 2 });
        assert!(allowed.success);
        assert_eq!(console.pane_state(2), PaneControlState::Active);
    }

    #[test]
    fn pane_scoped_emergency_stop_blocks_only_target_quarantine_release() {
        let mut console = InterventionConsole::new();
        console.register_pane(1);
        console.register_pane(2);
        console.execute(
            "admin",
            InterventionAction::EmergencyStop {
                scope: EmergencyScope::Pane(1),
            },
        );
        console.execute(
            "admin",
            InterventionAction::QuarantinePane {
                pane_id: 1,
                reason: "target".into(),
            },
        );
        console.execute(
            "admin",
            InterventionAction::QuarantinePane {
                pane_id: 2,
                reason: "other".into(),
            },
        );

        let blocked = console.execute(
            "admin",
            InterventionAction::ReleaseQuarantine { pane_id: 1 },
        );
        assert!(!blocked.success);
        assert_eq!(console.pane_state(1), PaneControlState::Quarantined);

        let allowed = console.execute(
            "admin",
            InterventionAction::ReleaseQuarantine { pane_id: 2 },
        );
        assert!(allowed.success);
        assert_eq!(console.pane_state(2), PaneControlState::Active);
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
        assert_eq!(
            counts.get(&PaneControlState::Active).copied().unwrap_or(0),
            1
        );
        assert_eq!(
            counts.get(&PaneControlState::Paused).copied().unwrap_or(0),
            1
        );
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
        console.execute(
            "operator-1",
            InterventionAction::TakeoverPane { pane_id: 3 },
        );

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
        assert_eq!(
            counts.get(&PaneControlState::Paused).copied().unwrap_or(0),
            3
        );
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

    // ─── br-ft-fmeic: closed-pane approval invalidation ──────────────────

    #[test]
    fn unregister_pane_expires_pending_approvals_for_that_pane_ft_fmeic() {
        let mut console = InterventionConsole::new();
        console.register_pane(1);
        console.register_pane(2);

        let req_a = console.submit_approval(1, "destructive op A", RiskLevel::High, 0);
        let req_b = console.submit_approval(1, "destructive op B", RiskLevel::Critical, 0);
        let req_other = console.submit_approval(2, "different pane", RiskLevel::Medium, 0);

        // Pre-condition: all three pending and visible.
        assert_eq!(console.pending_approvals().len(), 3);

        // Close pane 1.
        let expired = console.unregister_pane(1);
        assert_eq!(expired, 2, "br-ft-fmeic: must expire 2 pane-1 approvals");

        // Pane-1 approvals are no longer visible to pending_approvals.
        let pending: Vec<u64> = console
            .pending_approvals()
            .iter()
            .map(|a| a.request_id)
            .collect();
        assert_eq!(pending, vec![req_other]);
        assert!(!pending.contains(&req_a));
        assert!(!pending.contains(&req_b));
    }

    #[test]
    fn unregister_pane_does_not_revive_already_terminal_approvals_ft_fmeic() {
        // An approval that was already approved/rejected/expired
        // must NOT have its status overwritten by unregister_pane.
        // The expire-on-unregister sweep targets only Pending entries.
        let mut console = InterventionConsole::new();
        console.register_pane(1);
        let req_id = console.submit_approval(1, "op", RiskLevel::Low, 0);

        // Approve it.
        let result = console.execute(
            "operator",
            InterventionAction::ApproveRequest { request_id: req_id },
        );
        assert!(result.success);

        // Now close the pane.
        console.unregister_pane(1);

        // The approved request is still recorded as Approved (not
        // overwritten to Expired by the unregister sweep).
        let entry = console
            .approval_queue
            .iter()
            .find(|a| a.request_id == req_id)
            .expect("entry preserved");
        assert_eq!(entry.status, ApprovalStatus::Approved);
    }

    #[test]
    fn process_approval_on_expired_after_unregister_reports_not_pending_ft_fmeic() {
        // After unregister_pane expires a pending approval, an
        // operator who tries to ApproveRequest it gets a clear
        // "not Pending" failure rather than a silent success.
        let mut console = InterventionConsole::new();
        console.register_pane(1);
        let req_id = console.submit_approval(1, "destructive", RiskLevel::High, 0);
        console.unregister_pane(1);

        let result = console.execute(
            "operator",
            InterventionAction::ApproveRequest { request_id: req_id },
        );
        assert!(
            !result.success,
            "br-ft-fmeic: post-unregister approve must fail; got {result:?}"
        );
        assert!(
            result.message.contains("Expired") || result.message.contains("expired"),
            "br-ft-fmeic: failure message must reference expired status; got {}",
            result.message
        );
    }

    #[test]
    fn unregister_pane_with_no_pending_approvals_returns_zero_ft_fmeic() {
        let mut console = InterventionConsole::new();
        console.register_pane(7);
        let expired = console.unregister_pane(7);
        assert_eq!(expired, 0);
    }

    #[test]
    fn unregister_unknown_pane_returns_zero_ft_fmeic() {
        // Defensive: unregister of a pane that was never tracked
        // returns 0 (no approvals to expire) and does not panic.
        let mut console = InterventionConsole::new();
        let expired = console.unregister_pane(999);
        assert_eq!(expired, 0);
    }

    // br-ft-fmeic: property test — for any sequence of register +
    // submit-approval + unregister calls, pending_approvals MUST
    // never include an approval whose pane was unregistered after
    // its submission.
    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 64,
            ..proptest::test_runner::Config::default()
        })]

        #[test]
        fn pending_approvals_excludes_unregistered_panes_ft_fmeic(
            ops in proptest::collection::vec(
                proptest::prop_oneof![
                    (1u64..=8).prop_map(|p| ConsoleOp::Register(p)),
                    (1u64..=8, ".{0,16}").prop_map(|(p, d)| ConsoleOp::Submit(p, d)),
                    (1u64..=8).prop_map(|p| ConsoleOp::Unregister(p)),
                ],
                0..32,
            ),
        ) {
            let mut console = InterventionConsole::new();
            // Track which panes have been unregistered AFTER any
            // submission for them (i.e., should-be-expired set).
            let mut unregistered: std::collections::HashSet<u64> =
                std::collections::HashSet::new();

            for op in ops {
                match op {
                    ConsoleOp::Register(p) => {
                        console.register_pane(p);
                        // A re-register clears the unregister flag —
                        // a fresh registration is a fresh lifecycle.
                        unregistered.remove(&p);
                    }
                    ConsoleOp::Submit(p, d) => {
                        // Submit only succeeds (yields visible
                        // pending) if pane is currently registered
                        // AND not in the unregistered set.
                        let _ = console.submit_approval(p, d, RiskLevel::Low, 0);
                    }
                    ConsoleOp::Unregister(p) => {
                        console.unregister_pane(p);
                        unregistered.insert(p);
                    }
                }
            }

            // INVARIANT: no pending_approval references a pane that
            // was unregistered after its submission and not
            // re-registered.
            for approval in console.pending_approvals() {
                prop_assert!(
                    !unregistered.contains(&approval.pane_id),
                    "br-ft-fmeic: pending approval for pane {} survived unregister: {:?}",
                    approval.pane_id,
                    approval
                );
            }
        }
    }

    #[derive(Debug, Clone)]
    enum ConsoleOp {
        Register(u64),
        Submit(u64, String),
        Unregister(u64),
    }
}
