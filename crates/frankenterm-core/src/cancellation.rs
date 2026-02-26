//! Two-phase cancellation and shutdown protocol for structured concurrency.
//!
//! Implements the request→drain→finalize shutdown protocol that replaces
//! implicit drop-based cancellation. Each scope in the tree gets:
//!
//! - A **cancellation token** (hierarchical, propagating, reason-aware)
//! - A **shutdown policy** (grace period, escalation, cascading)
//! - A **finalizer registry** (ordered cleanup descriptors)
//!
//! # Two-phase protocol
//!
//! **Phase 1 — Draining:**
//! - Stop accepting new work
//! - Drain in-flight operations
//! - Cascade cancellation to children
//! - Grace period timer starts
//!
//! **Phase 2 — Finalizing:**
//! - Run registered finalizers in priority order
//! - Persist state, flush buffers, close connections
//! - Emit structured shutdown events
//! - Transition to Closed
//!
//! # Shutdown ordering
//!
//! Follows scope_tree's bottom-up ordering: children drain before parents.
//! Within a tier, LIFO registration order is preserved. The coordinator
//! enforces that a parent cannot finalize until all children are closed.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::scope_tree::{ScopeId, ScopeState, ScopeTier, ScopeTree, ScopeTreeError};

// ── Shutdown Reason ────────────────────────────────────────────────────────

/// Why a scope is being shut down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShutdownReason {
    /// User-initiated (Ctrl+C, CLI command, SIGTERM).
    UserRequested,
    /// Normal process exit / graceful termination.
    GracefulTermination,
    /// Grace period expired during drain phase.
    Timeout { deadline_ms: i64, elapsed_ms: i64 },
    /// A child scope encountered an unrecoverable error.
    ChildError {
        child_id: ScopeId,
        error_msg: String,
    },
    /// Cascading failure propagating from another scope.
    CascadingFailure { origin_id: ScopeId },
    /// Resource budget exhausted (memory, FDs, connections).
    ResourceExhausted { resource: String },
    /// Safety policy triggered shutdown.
    PolicyViolation { rule: String },
    /// Parent scope is shutting down (propagated cancellation).
    ParentShutdown { parent_id: ScopeId },
}

impl fmt::Display for ShutdownReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UserRequested => write!(f, "user-requested"),
            Self::GracefulTermination => write!(f, "graceful-termination"),
            Self::Timeout {
                deadline_ms,
                elapsed_ms,
            } => write!(
                f,
                "timeout(deadline={deadline_ms}ms, elapsed={elapsed_ms}ms)"
            ),
            Self::ChildError {
                child_id,
                error_msg,
            } => write!(f, "child-error({child_id}: {error_msg})"),
            Self::CascadingFailure { origin_id } => {
                write!(f, "cascading-failure(origin={origin_id})")
            }
            Self::ResourceExhausted { resource } => {
                write!(f, "resource-exhausted({resource})")
            }
            Self::PolicyViolation { rule } => write!(f, "policy-violation({rule})"),
            Self::ParentShutdown { parent_id } => {
                write!(f, "parent-shutdown({parent_id})")
            }
        }
    }
}

// ── Escalation Action ──────────────────────────────────────────────────────

/// What to do when a grace period expires during drain phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EscalationAction {
    /// Skip remaining drain, force-close immediately.
    ForceClose,
    /// Extend the grace period by the given amount.
    ExtendGrace { extra_ms: u64 },
    /// Log the expiry but keep waiting.
    LogAndWait,
}

impl fmt::Display for EscalationAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForceClose => write!(f, "force-close"),
            Self::ExtendGrace { extra_ms } => write!(f, "extend-grace(+{extra_ms}ms)"),
            Self::LogAndWait => write!(f, "log-and-wait"),
        }
    }
}

// ── Shutdown Policy ────────────────────────────────────────────────────────

/// Per-scope configuration for graceful shutdown behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownPolicy {
    /// Maximum time (ms) to spend in Draining before escalation.
    pub grace_period_ms: u64,
    /// What happens when the grace period expires.
    pub escalation: EscalationAction,
    /// Whether to cascade shutdown to child scopes.
    pub cascade_to_children: bool,
    /// Whether to run registered finalizers.
    pub run_finalizers: bool,
    /// Maximum time (ms) for all finalizers to complete.
    pub finalizer_timeout_ms: u64,
}

impl ShutdownPolicy {
    /// Default policy for a given tier.
    #[must_use]
    pub fn for_tier(tier: ScopeTier) -> Self {
        match tier {
            ScopeTier::Root => Self {
                grace_period_ms: 30_000,
                escalation: EscalationAction::ForceClose,
                cascade_to_children: true,
                run_finalizers: true,
                finalizer_timeout_ms: 10_000,
            },
            ScopeTier::Daemon => Self {
                grace_period_ms: 15_000,
                escalation: EscalationAction::ForceClose,
                cascade_to_children: true,
                run_finalizers: true,
                finalizer_timeout_ms: 5_000,
            },
            ScopeTier::Watcher => Self {
                grace_period_ms: 10_000,
                escalation: EscalationAction::ForceClose,
                cascade_to_children: true,
                run_finalizers: true,
                finalizer_timeout_ms: 3_000,
            },
            ScopeTier::Worker => Self {
                grace_period_ms: 5_000,
                escalation: EscalationAction::ForceClose,
                cascade_to_children: false,
                run_finalizers: true,
                finalizer_timeout_ms: 2_000,
            },
            ScopeTier::Ephemeral => Self {
                grace_period_ms: 1_000,
                escalation: EscalationAction::ForceClose,
                cascade_to_children: false,
                run_finalizers: false,
                finalizer_timeout_ms: 500,
            },
        }
    }
}

impl Default for ShutdownPolicy {
    fn default() -> Self {
        Self::for_tier(ScopeTier::Worker)
    }
}

// ── Cancellation Token ─────────────────────────────────────────────────────

/// A runtime-agnostic, hierarchical cancellation token.
///
/// Tasks hold clones of this token and poll `is_cancelled()` in their loops.
/// When a parent is cancelled, all children are cancelled automatically.
///
/// Thread-safe: all operations use atomic orderings.
#[derive(Debug, Clone)]
pub struct CancellationToken {
    inner: Arc<CancellationTokenInner>,
}

#[derive(Debug)]
struct CancellationTokenInner {
    /// Whether cancellation has been requested.
    cancelled: AtomicBool,
    /// Monotonic generation counter — incremented on each state change.
    generation: AtomicU64,
    /// The scope this token belongs to.
    scope_id: ScopeId,
    /// The reason for cancellation (set once, read many).
    reason: std::sync::Mutex<Option<ShutdownReason>>,
    /// Child tokens that propagate cancellation.
    children: std::sync::Mutex<Vec<Arc<CancellationTokenInner>>>,
}

impl CancellationToken {
    /// Create a new root cancellation token for a scope.
    #[must_use]
    pub fn new(scope_id: ScopeId) -> Self {
        Self {
            inner: Arc::new(CancellationTokenInner {
                cancelled: AtomicBool::new(false),
                generation: AtomicU64::new(0),
                scope_id,
                reason: std::sync::Mutex::new(None),
                children: std::sync::Mutex::new(Vec::new()),
            }),
        }
    }

    /// Create a child token that will be cancelled when this token is cancelled.
    #[must_use]
    pub fn child(&self, child_scope_id: ScopeId) -> Self {
        let child = CancellationToken::new(child_scope_id);
        // If parent is already cancelled, cancel child immediately
        if self.is_cancelled() {
            let reason = self.reason().map(|_| ShutdownReason::ParentShutdown {
                parent_id: self.inner.scope_id.clone(),
            });
            child.inner.cancelled.store(true, Ordering::Release);
            if let Some(r) = reason {
                *child.inner.reason.lock().expect("lock not poisoned") = Some(r);
            }
        }
        let mut children = self.inner.children.lock().expect("lock not poisoned");
        children.push(Arc::clone(&child.inner));
        child
    }

    /// Cancel this token and all descendant tokens.
    pub fn cancel(&self, reason: ShutdownReason) {
        if self
            .inner
            .cancelled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            *self.inner.reason.lock().expect("lock not poisoned") = Some(reason);
            self.inner.generation.fetch_add(1, Ordering::Relaxed);
            self.propagate_to_children();
        }
    }

    /// Propagate cancellation to all registered children.
    fn propagate_to_children(&self) {
        let children = self.inner.children.lock().expect("lock not poisoned");
        for child in children.iter() {
            if child
                .cancelled
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                *child.reason.lock().expect("lock not poisoned") =
                    Some(ShutdownReason::ParentShutdown {
                        parent_id: self.inner.scope_id.clone(),
                    });
                child.generation.fetch_add(1, Ordering::Relaxed);
                // Recursively propagate
                let grandchildren = child.children.lock().expect("lock not poisoned");
                for gc in grandchildren.iter() {
                    Self::propagate_inner(gc, &child.scope_id);
                }
            }
        }
    }

    fn propagate_inner(inner: &CancellationTokenInner, parent_id: &ScopeId) {
        if inner
            .cancelled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            *inner.reason.lock().expect("lock not poisoned") =
                Some(ShutdownReason::ParentShutdown {
                    parent_id: parent_id.clone(),
                });
            inner.generation.fetch_add(1, Ordering::Relaxed);
            let children = inner.children.lock().expect("lock not poisoned");
            for child in children.iter() {
                Self::propagate_inner(child, &inner.scope_id);
            }
        }
    }

    /// Check if cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// The reason for cancellation, if set.
    #[must_use]
    pub fn reason(&self) -> Option<ShutdownReason> {
        self.inner.reason.lock().expect("lock not poisoned").clone()
    }

    /// The scope this token belongs to.
    #[must_use]
    pub fn scope_id(&self) -> &ScopeId {
        &self.inner.scope_id
    }

    /// Current generation counter.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Relaxed)
    }

    /// Number of registered child tokens.
    #[must_use]
    pub fn child_count(&self) -> usize {
        self.inner.children.lock().expect("lock not poisoned").len()
    }

    /// Remove all cancelled children from the registry (GC sweep).
    pub fn prune_cancelled_children(&self) -> usize {
        let mut children = self.inner.children.lock().expect("lock not poisoned");
        let before = children.len();
        children.retain(|c| !c.cancelled.load(Ordering::Acquire));
        before - children.len()
    }
}

// ── Finalizer Registry ─────────────────────────────────────────────────────

/// A registered cleanup action to run during the Finalizing phase.
///
/// Finalizers are descriptors, not closures — the actual execution happens
/// at the callsite using the action name and metadata for dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finalizer {
    /// Human-readable name (e.g. "flush-capture-channel").
    pub name: String,
    /// Execution priority (higher = runs first).
    pub priority: u32,
    /// The action to perform.
    pub action: FinalizerAction,
    /// Current status.
    pub status: FinalizerStatus,
}

/// What kind of cleanup action to perform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinalizerAction {
    /// Flush an internal channel/queue.
    FlushChannel { channel_name: String },
    /// Persist state to storage.
    PersistState { key: String },
    /// Close a network connection.
    CloseConnection { conn_id: u64 },
    /// Release a resource reservation.
    ReleaseResource { resource_id: String },
    /// Cancel pending timers.
    CancelTimers { scope_prefix: String },
    /// Custom action with key-value metadata.
    Custom {
        action_name: String,
        metadata: HashMap<String, String>,
    },
}

impl fmt::Display for FinalizerAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FlushChannel { channel_name } => write!(f, "flush({channel_name})"),
            Self::PersistState { key } => write!(f, "persist({key})"),
            Self::CloseConnection { conn_id } => write!(f, "close-conn({conn_id})"),
            Self::ReleaseResource { resource_id } => write!(f, "release({resource_id})"),
            Self::CancelTimers { scope_prefix } => write!(f, "cancel-timers({scope_prefix})"),
            Self::Custom { action_name, .. } => write!(f, "custom({action_name})"),
        }
    }
}

/// Status of a finalizer during the shutdown sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinalizerStatus {
    /// Not yet executed.
    Pending,
    /// Currently executing.
    Running,
    /// Completed successfully.
    Completed { duration_ms: u64 },
    /// Failed during execution.
    Failed { error: String, duration_ms: u64 },
    /// Skipped (e.g. policy says no finalizers, or timeout expired).
    Skipped { reason: String },
}

impl fmt::Display for FinalizerStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Running => write!(f, "running"),
            Self::Completed { duration_ms } => write!(f, "completed({duration_ms}ms)"),
            Self::Failed { error, duration_ms } => {
                write!(f, "failed({error}, {duration_ms}ms)")
            }
            Self::Skipped { reason } => write!(f, "skipped({reason})"),
        }
    }
}

// ── Shutdown Event ─────────────────────────────────────────────────────────

/// Structured log event emitted during the shutdown protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShutdownEvent {
    /// Epoch milliseconds when the event occurred.
    pub timestamp_ms: i64,
    /// The scope this event relates to.
    pub scope_id: ScopeId,
    /// What happened.
    pub event_type: ShutdownEventType,
    /// Optional correlation ID for tracing.
    pub correlation_id: Option<String>,
}

/// The specific shutdown event that occurred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShutdownEventType {
    /// Shutdown was requested for a scope.
    ShutdownRequested { reason: ShutdownReason },
    /// Phase 1: drain started.
    DrainStarted { grace_period_ms: u64 },
    /// Phase 1: drain completed.
    DrainCompleted { elapsed_ms: i64 },
    /// Grace period expired before drain completed.
    GracePeriodExpired {
        grace_ms: u64,
        action: EscalationAction,
    },
    /// Cancellation cascaded to a child scope.
    CascadeTriggered { target_id: ScopeId },
    /// Phase 2: a finalizer started.
    FinalizerStarted { name: String },
    /// Phase 2: a finalizer completed.
    FinalizerCompleted { name: String, duration_ms: u64 },
    /// Phase 2: a finalizer failed.
    FinalizerFailed {
        name: String,
        error: String,
        duration_ms: u64,
    },
    /// Phase 2: a finalizer was skipped.
    FinalizerSkipped { name: String, reason: String },
    /// Escalation was triggered (grace period expired).
    EscalationTriggered { action: EscalationAction },
    /// Scope fully closed.
    ScopeClosed { total_shutdown_ms: i64 },
}

impl fmt::Display for ShutdownEventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShutdownRequested { reason } => {
                write!(f, "shutdown-requested({reason})")
            }
            Self::DrainStarted { grace_period_ms } => {
                write!(f, "drain-started(grace={grace_period_ms}ms)")
            }
            Self::DrainCompleted { elapsed_ms } => {
                write!(f, "drain-completed({elapsed_ms}ms)")
            }
            Self::GracePeriodExpired { grace_ms, action } => {
                write!(f, "grace-expired({grace_ms}ms, {action})")
            }
            Self::CascadeTriggered { target_id } => {
                write!(f, "cascade({target_id})")
            }
            Self::FinalizerStarted { name } => write!(f, "finalizer-start({name})"),
            Self::FinalizerCompleted { name, duration_ms } => {
                write!(f, "finalizer-done({name}, {duration_ms}ms)")
            }
            Self::FinalizerFailed {
                name,
                error,
                duration_ms,
            } => write!(f, "finalizer-fail({name}: {error}, {duration_ms}ms)"),
            Self::FinalizerSkipped { name, reason } => {
                write!(f, "finalizer-skip({name}: {reason})")
            }
            Self::EscalationTriggered { action } => {
                write!(f, "escalation({action})")
            }
            Self::ScopeClosed { total_shutdown_ms } => {
                write!(f, "closed(total={total_shutdown_ms}ms)")
            }
        }
    }
}

// ── Shutdown Coordinator ───────────────────────────────────────────────────

/// Orchestrates the two-phase shutdown protocol across the scope tree.
///
/// Maintains cancellation tokens, policies, and finalizer registries for all
/// registered scopes. Emits structured shutdown events for observability.
#[derive(Debug)]
pub struct ShutdownCoordinator {
    /// Cancellation tokens keyed by scope ID.
    tokens: HashMap<ScopeId, CancellationToken>,
    /// Shutdown policies keyed by scope ID.
    policies: HashMap<ScopeId, ShutdownPolicy>,
    /// Registered finalizers keyed by scope ID (ordered by priority, desc).
    finalizers: HashMap<ScopeId, Vec<Finalizer>>,
    /// Structured event log.
    events: Vec<ShutdownEvent>,
    /// Default policy applied when no scope-specific policy is set.
    default_policy: ShutdownPolicy,
    /// Optional correlation ID prefix for event tracing.
    correlation_prefix: Option<String>,
}

/// Errors from the shutdown coordinator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownCoordinatorError {
    /// The scope is not registered with the coordinator.
    ScopeNotRegistered { scope_id: ScopeId },
    /// The scope is already registered.
    ScopeAlreadyRegistered { scope_id: ScopeId },
    /// Scope tree error during state transitions.
    TreeError(ScopeTreeError),
    /// Finalizer not found.
    FinalizerNotFound {
        scope_id: ScopeId,
        finalizer_name: String,
    },
    /// Scope is not in expected state for this operation.
    InvalidState {
        scope_id: ScopeId,
        expected: &'static str,
        actual: ScopeState,
    },
}

impl fmt::Display for ShutdownCoordinatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScopeNotRegistered { scope_id } => {
                write!(f, "scope not registered: {scope_id}")
            }
            Self::ScopeAlreadyRegistered { scope_id } => {
                write!(f, "scope already registered: {scope_id}")
            }
            Self::TreeError(e) => write!(f, "scope tree error: {e}"),
            Self::FinalizerNotFound {
                scope_id,
                finalizer_name,
            } => write!(
                f,
                "finalizer {finalizer_name} not found for scope {scope_id}"
            ),
            Self::InvalidState {
                scope_id,
                expected,
                actual,
            } => write!(f, "scope {scope_id} in state {actual}, expected {expected}"),
        }
    }
}

impl From<ScopeTreeError> for ShutdownCoordinatorError {
    fn from(e: ScopeTreeError) -> Self {
        Self::TreeError(e)
    }
}

/// Summary of a completed shutdown sequence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShutdownSummary {
    /// Scope that was shut down.
    pub scope_id: ScopeId,
    /// Why the shutdown was triggered.
    pub reason: ShutdownReason,
    /// Time spent in drain phase (ms).
    pub drain_elapsed_ms: i64,
    /// Time spent in finalize phase (ms).
    pub finalize_elapsed_ms: i64,
    /// Total shutdown time (ms).
    pub total_elapsed_ms: i64,
    /// Number of finalizers that ran.
    pub finalizers_run: usize,
    /// Number of finalizers that succeeded.
    pub finalizers_succeeded: usize,
    /// Number of finalizers that failed.
    pub finalizers_failed: usize,
    /// Number of finalizers that were skipped.
    pub finalizers_skipped: usize,
    /// Number of child scopes that were cascade-cancelled.
    pub cascaded_children: usize,
    /// Whether escalation was triggered.
    pub escalated: bool,
}

impl ShutdownCoordinator {
    /// Create a new coordinator with a default policy.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tokens: HashMap::new(),
            policies: HashMap::new(),
            finalizers: HashMap::new(),
            events: Vec::new(),
            default_policy: ShutdownPolicy::default(),
            correlation_prefix: None,
        }
    }

    /// Create a coordinator with a custom default policy.
    #[must_use]
    pub fn with_default_policy(default_policy: ShutdownPolicy) -> Self {
        Self {
            default_policy,
            ..Self::new()
        }
    }

    /// Set the correlation prefix for event tracing.
    pub fn set_correlation_prefix(&mut self, prefix: impl Into<String>) {
        self.correlation_prefix = Some(prefix.into());
    }

    /// Register a scope with the coordinator. Creates a cancellation token and
    /// applies the tier-default policy unless overridden.
    pub fn register_scope(
        &mut self,
        scope_id: &ScopeId,
        tier: ScopeTier,
        parent_scope_id: Option<&ScopeId>,
    ) -> Result<CancellationToken, ShutdownCoordinatorError> {
        if self.tokens.contains_key(scope_id) {
            return Err(ShutdownCoordinatorError::ScopeAlreadyRegistered {
                scope_id: scope_id.clone(),
            });
        }

        // Create token — child of parent if one exists
        let token = if let Some(pid) = parent_scope_id {
            if let Some(parent_token) = self.tokens.get(pid) {
                parent_token.child(scope_id.clone())
            } else {
                CancellationToken::new(scope_id.clone())
            }
        } else {
            CancellationToken::new(scope_id.clone())
        };

        self.tokens.insert(scope_id.clone(), token.clone());
        self.policies
            .insert(scope_id.clone(), ShutdownPolicy::for_tier(tier));
        self.finalizers.insert(scope_id.clone(), Vec::new());

        Ok(token)
    }

    /// Set a custom shutdown policy for a scope.
    pub fn set_policy(
        &mut self,
        scope_id: &ScopeId,
        policy: ShutdownPolicy,
    ) -> Result<(), ShutdownCoordinatorError> {
        if !self.tokens.contains_key(scope_id) {
            return Err(ShutdownCoordinatorError::ScopeNotRegistered {
                scope_id: scope_id.clone(),
            });
        }
        self.policies.insert(scope_id.clone(), policy);
        Ok(())
    }

    /// Get the shutdown policy for a scope.
    #[must_use]
    pub fn policy(&self, scope_id: &ScopeId) -> &ShutdownPolicy {
        self.policies.get(scope_id).unwrap_or(&self.default_policy)
    }

    /// Get the cancellation token for a scope.
    #[must_use]
    pub fn token(&self, scope_id: &ScopeId) -> Option<&CancellationToken> {
        self.tokens.get(scope_id)
    }

    /// Register a finalizer for a scope.
    pub fn register_finalizer(
        &mut self,
        scope_id: &ScopeId,
        finalizer: Finalizer,
    ) -> Result<(), ShutdownCoordinatorError> {
        let finalizers = self.finalizers.get_mut(scope_id).ok_or_else(|| {
            ShutdownCoordinatorError::ScopeNotRegistered {
                scope_id: scope_id.clone(),
            }
        })?;

        // Insert sorted by priority (descending — highest priority first)
        let pos = finalizers
            .iter()
            .position(|f| f.priority < finalizer.priority)
            .unwrap_or(finalizers.len());
        finalizers.insert(pos, finalizer);
        Ok(())
    }

    /// List finalizers for a scope in execution order.
    #[must_use]
    pub fn finalizers(&self, scope_id: &ScopeId) -> &[Finalizer] {
        self.finalizers
            .get(scope_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Phase 1: Request shutdown of a scope (transition to Draining).
    ///
    /// Cancels the scope's token, cascades to children if policy allows,
    /// and transitions the scope in the tree.
    pub fn request_shutdown(
        &mut self,
        tree: &mut ScopeTree,
        scope_id: &ScopeId,
        reason: ShutdownReason,
        timestamp_ms: i64,
    ) -> Result<Vec<ScopeId>, ShutdownCoordinatorError> {
        // Validate scope is registered
        let token = self
            .tokens
            .get(scope_id)
            .ok_or_else(|| ShutdownCoordinatorError::ScopeNotRegistered {
                scope_id: scope_id.clone(),
            })?
            .clone();

        // Validate scope is in a state that can be shut down
        let node = tree.get(scope_id).ok_or_else(|| {
            ShutdownCoordinatorError::TreeError(ScopeTreeError::ScopeNotFound {
                scope_id: scope_id.clone(),
            })
        })?;
        if node.state.is_shutting_down() || node.state.is_terminal() {
            return Err(ShutdownCoordinatorError::InvalidState {
                scope_id: scope_id.clone(),
                expected: "Created or Running",
                actual: node.state,
            });
        }

        // Cancel the token
        token.cancel(reason.clone());

        // Transition in tree
        tree.request_shutdown(scope_id, timestamp_ms)?;

        // Emit event
        let policy = self.policy(scope_id).clone();
        self.emit_event(
            scope_id.clone(),
            ShutdownEventType::ShutdownRequested {
                reason: reason.clone(),
            },
            timestamp_ms,
        );
        self.emit_event(
            scope_id.clone(),
            ShutdownEventType::DrainStarted {
                grace_period_ms: policy.grace_period_ms,
            },
            timestamp_ms,
        );

        // Cascade to children if policy allows
        let mut cascaded = Vec::new();
        if policy.cascade_to_children {
            let children: Vec<ScopeId> = tree
                .get(scope_id)
                .map(|n| n.children.clone())
                .unwrap_or_default();

            for child_id in children {
                if let Some(child_node) = tree.get(&child_id) {
                    if !child_node.state.is_shutting_down() && !child_node.state.is_terminal() {
                        let child_reason = ShutdownReason::ParentShutdown {
                            parent_id: scope_id.clone(),
                        };
                        // Recursively cascade
                        if self
                            .request_shutdown(tree, &child_id, child_reason, timestamp_ms)
                            .is_ok()
                        {
                            self.emit_event(
                                scope_id.clone(),
                                ShutdownEventType::CascadeTriggered {
                                    target_id: child_id.clone(),
                                },
                                timestamp_ms,
                            );
                            cascaded.push(child_id);
                        }
                    }
                }
            }
        }

        Ok(cascaded)
    }

    /// Check if a scope's grace period has expired.
    #[must_use]
    pub fn is_grace_expired(&self, tree: &ScopeTree, scope_id: &ScopeId, current_ms: i64) -> bool {
        let node = match tree.get(scope_id) {
            Some(n) => n,
            None => return false,
        };
        if node.state != ScopeState::Draining {
            return false;
        }
        let requested_at = match node.shutdown_requested_at_ms {
            Some(t) => t,
            None => return false,
        };
        let policy = self.policy(scope_id);
        let deadline = requested_at + policy.grace_period_ms as i64;
        current_ms >= deadline
    }

    /// Handle grace period expiry by applying the escalation action.
    pub fn handle_grace_expiry(
        &mut self,
        tree: &mut ScopeTree,
        scope_id: &ScopeId,
        current_ms: i64,
    ) -> Result<EscalationAction, ShutdownCoordinatorError> {
        let policy = self.policy(scope_id).clone();
        let action = policy.escalation.clone();

        self.emit_event(
            scope_id.clone(),
            ShutdownEventType::GracePeriodExpired {
                grace_ms: policy.grace_period_ms,
                action: action.clone(),
            },
            current_ms,
        );
        self.emit_event(
            scope_id.clone(),
            ShutdownEventType::EscalationTriggered {
                action: action.clone(),
            },
            current_ms,
        );

        match &action {
            EscalationAction::ForceClose => {
                // Force-close: skip remaining drain, force children closed,
                // then finalize and close this scope.
                self.force_close_subtree(tree, scope_id, current_ms)?;
            }
            EscalationAction::ExtendGrace { extra_ms } => {
                // Extend by updating the policy
                let updated_policy = ShutdownPolicy {
                    grace_period_ms: policy.grace_period_ms + extra_ms,
                    ..policy
                };
                self.policies.insert(scope_id.clone(), updated_policy);
            }
            EscalationAction::LogAndWait => {
                // Just logged above, no action needed
            }
        }

        Ok(action)
    }

    /// Force-close a scope and all its descendants (skip finalizers).
    fn force_close_subtree(
        &mut self,
        tree: &mut ScopeTree,
        scope_id: &ScopeId,
        timestamp_ms: i64,
    ) -> Result<(), ShutdownCoordinatorError> {
        // First, force-close all children
        let children: Vec<ScopeId> = tree
            .get(scope_id)
            .map(|n| n.children.clone())
            .unwrap_or_default();

        for child_id in children {
            if let Some(child_node) = tree.get(&child_id) {
                if !child_node.state.is_terminal() {
                    self.force_close_subtree(tree, &child_id, timestamp_ms)?;
                }
            }
        }

        // Now close this scope (skip through states as needed)
        let node = tree.get(scope_id).ok_or_else(|| {
            ShutdownCoordinatorError::TreeError(ScopeTreeError::ScopeNotFound {
                scope_id: scope_id.clone(),
            })
        })?;

        match node.state {
            ScopeState::Created | ScopeState::Running => {
                tree.request_shutdown(scope_id, timestamp_ms)?;
                tree.finalize(scope_id)?;
                tree.close(scope_id, timestamp_ms)?;
            }
            ScopeState::Draining => {
                // Mark all finalizers as skipped
                self.skip_all_finalizers(scope_id, "force-close escalation");
                tree.finalize(scope_id)?;
                tree.close(scope_id, timestamp_ms)?;
            }
            ScopeState::Finalizing => {
                tree.close(scope_id, timestamp_ms)?;
            }
            ScopeState::Closed => {
                // Already closed
            }
        }

        self.emit_event(
            scope_id.clone(),
            ShutdownEventType::ScopeClosed {
                total_shutdown_ms: 0,
            },
            timestamp_ms,
        );

        Ok(())
    }

    /// Phase 2: Transition to Finalizing and execute finalizer descriptors.
    ///
    /// All children must be closed before this can proceed (enforced by the tree).
    pub fn begin_finalize(
        &mut self,
        tree: &mut ScopeTree,
        scope_id: &ScopeId,
        drain_elapsed_ms: i64,
        timestamp_ms: i64,
    ) -> Result<(), ShutdownCoordinatorError> {
        // Emit drain completed
        self.emit_event(
            scope_id.clone(),
            ShutdownEventType::DrainCompleted {
                elapsed_ms: drain_elapsed_ms,
            },
            timestamp_ms,
        );

        // Transition in tree (validates children are closed)
        tree.finalize(scope_id)?;

        // Check if we should run finalizers
        let policy = self.policy(scope_id).clone();
        if !policy.run_finalizers {
            self.skip_all_finalizers(scope_id, "policy: run_finalizers=false");
        }

        Ok(())
    }

    /// Mark a finalizer as started.
    pub fn mark_finalizer_started(
        &mut self,
        scope_id: &ScopeId,
        name: &str,
        timestamp_ms: i64,
    ) -> Result<(), ShutdownCoordinatorError> {
        let finalizers = self.finalizers.get_mut(scope_id).ok_or_else(|| {
            ShutdownCoordinatorError::ScopeNotRegistered {
                scope_id: scope_id.clone(),
            }
        })?;

        let finalizer = finalizers
            .iter_mut()
            .find(|f| f.name == name)
            .ok_or_else(|| ShutdownCoordinatorError::FinalizerNotFound {
                scope_id: scope_id.clone(),
                finalizer_name: name.to_string(),
            })?;

        finalizer.status = FinalizerStatus::Running;
        self.emit_event(
            scope_id.clone(),
            ShutdownEventType::FinalizerStarted {
                name: name.to_string(),
            },
            timestamp_ms,
        );
        Ok(())
    }

    /// Mark a finalizer as completed.
    pub fn mark_finalizer_completed(
        &mut self,
        scope_id: &ScopeId,
        name: &str,
        duration_ms: u64,
        timestamp_ms: i64,
    ) -> Result<(), ShutdownCoordinatorError> {
        let finalizers = self.finalizers.get_mut(scope_id).ok_or_else(|| {
            ShutdownCoordinatorError::ScopeNotRegistered {
                scope_id: scope_id.clone(),
            }
        })?;

        let finalizer = finalizers
            .iter_mut()
            .find(|f| f.name == name)
            .ok_or_else(|| ShutdownCoordinatorError::FinalizerNotFound {
                scope_id: scope_id.clone(),
                finalizer_name: name.to_string(),
            })?;

        finalizer.status = FinalizerStatus::Completed { duration_ms };
        self.emit_event(
            scope_id.clone(),
            ShutdownEventType::FinalizerCompleted {
                name: name.to_string(),
                duration_ms,
            },
            timestamp_ms,
        );
        Ok(())
    }

    /// Mark a finalizer as failed.
    pub fn mark_finalizer_failed(
        &mut self,
        scope_id: &ScopeId,
        name: &str,
        error: &str,
        duration_ms: u64,
        timestamp_ms: i64,
    ) -> Result<(), ShutdownCoordinatorError> {
        let finalizers = self.finalizers.get_mut(scope_id).ok_or_else(|| {
            ShutdownCoordinatorError::ScopeNotRegistered {
                scope_id: scope_id.clone(),
            }
        })?;

        let finalizer = finalizers
            .iter_mut()
            .find(|f| f.name == name)
            .ok_or_else(|| ShutdownCoordinatorError::FinalizerNotFound {
                scope_id: scope_id.clone(),
                finalizer_name: name.to_string(),
            })?;

        finalizer.status = FinalizerStatus::Failed {
            error: error.to_string(),
            duration_ms,
        };
        self.emit_event(
            scope_id.clone(),
            ShutdownEventType::FinalizerFailed {
                name: name.to_string(),
                error: error.to_string(),
                duration_ms,
            },
            timestamp_ms,
        );
        Ok(())
    }

    /// Complete the shutdown sequence and close the scope.
    pub fn complete_shutdown(
        &mut self,
        tree: &mut ScopeTree,
        scope_id: &ScopeId,
        timestamp_ms: i64,
    ) -> Result<ShutdownSummary, ShutdownCoordinatorError> {
        let node = tree.get(scope_id).ok_or_else(|| {
            ShutdownCoordinatorError::TreeError(ScopeTreeError::ScopeNotFound {
                scope_id: scope_id.clone(),
            })
        })?;

        if node.state != ScopeState::Finalizing {
            return Err(ShutdownCoordinatorError::InvalidState {
                scope_id: scope_id.clone(),
                expected: "Finalizing",
                actual: node.state,
            });
        }

        let shutdown_requested_at = node.shutdown_requested_at_ms.unwrap_or(timestamp_ms);
        let drain_elapsed = node
            .started_at_ms
            .map(|s| timestamp_ms - s.max(shutdown_requested_at))
            .unwrap_or(0);

        // Collect finalizer stats
        let finalizer_stats = self.finalizer_stats(scope_id);

        // Close in tree
        tree.close(scope_id, timestamp_ms)?;

        let total_elapsed = timestamp_ms - shutdown_requested_at;

        self.emit_event(
            scope_id.clone(),
            ShutdownEventType::ScopeClosed {
                total_shutdown_ms: total_elapsed,
            },
            timestamp_ms,
        );

        // Determine reason
        let reason = self
            .tokens
            .get(scope_id)
            .and_then(|t| t.reason())
            .unwrap_or(ShutdownReason::GracefulTermination);

        // Count cascaded children
        let cascaded = self
            .events
            .iter()
            .filter(|e| {
                e.scope_id == *scope_id
                    && matches!(e.event_type, ShutdownEventType::CascadeTriggered { .. })
            })
            .count();

        let escalated = self.events.iter().any(|e| {
            e.scope_id == *scope_id
                && matches!(e.event_type, ShutdownEventType::EscalationTriggered { .. })
        });

        Ok(ShutdownSummary {
            scope_id: scope_id.clone(),
            reason,
            drain_elapsed_ms: drain_elapsed,
            finalize_elapsed_ms: total_elapsed - drain_elapsed,
            total_elapsed_ms: total_elapsed,
            finalizers_run: finalizer_stats.0,
            finalizers_succeeded: finalizer_stats.1,
            finalizers_failed: finalizer_stats.2,
            finalizers_skipped: finalizer_stats.3,
            cascaded_children: cascaded,
            escalated,
        })
    }

    /// Skip all pending finalizers for a scope.
    fn skip_all_finalizers(&mut self, scope_id: &ScopeId, reason: &str) {
        if let Some(finalizers) = self.finalizers.get_mut(scope_id) {
            for f in finalizers.iter_mut() {
                if matches!(f.status, FinalizerStatus::Pending) {
                    f.status = FinalizerStatus::Skipped {
                        reason: reason.to_string(),
                    };
                }
            }
        }
    }

    /// Count finalizers by status: (run, succeeded, failed, skipped).
    fn finalizer_stats(&self, scope_id: &ScopeId) -> (usize, usize, usize, usize) {
        let finalizers = match self.finalizers.get(scope_id) {
            Some(fs) => fs,
            None => return (0, 0, 0, 0),
        };

        let run = finalizers
            .iter()
            .filter(|f| {
                !matches!(
                    f.status,
                    FinalizerStatus::Pending | FinalizerStatus::Skipped { .. }
                )
            })
            .count();
        let succeeded = finalizers
            .iter()
            .filter(|f| matches!(f.status, FinalizerStatus::Completed { .. }))
            .count();
        let failed = finalizers
            .iter()
            .filter(|f| matches!(f.status, FinalizerStatus::Failed { .. }))
            .count();
        let skipped = finalizers
            .iter()
            .filter(|f| matches!(f.status, FinalizerStatus::Skipped { .. }))
            .count();

        (run, succeeded, failed, skipped)
    }

    /// Emit a shutdown event.
    fn emit_event(&mut self, scope_id: ScopeId, event_type: ShutdownEventType, timestamp_ms: i64) {
        let correlation_id = self
            .correlation_prefix
            .as_ref()
            .map(|prefix| format!("{prefix}-{scope_id}-{timestamp_ms}"));

        self.events.push(ShutdownEvent {
            timestamp_ms,
            scope_id,
            event_type,
            correlation_id,
        });
    }

    /// All shutdown events (oldest first).
    #[must_use]
    pub fn events(&self) -> &[ShutdownEvent] {
        &self.events
    }

    /// Shutdown events for a specific scope.
    #[must_use]
    pub fn events_for_scope(&self, scope_id: &ScopeId) -> Vec<&ShutdownEvent> {
        self.events
            .iter()
            .filter(|e| e.scope_id == *scope_id)
            .collect()
    }

    /// Number of scopes registered with the coordinator.
    #[must_use]
    pub fn registered_scope_count(&self) -> usize {
        self.tokens.len()
    }

    /// Number of scopes currently cancelled.
    #[must_use]
    pub fn cancelled_count(&self) -> usize {
        self.tokens.values().filter(|t| t.is_cancelled()).count()
    }

    /// Prune cancelled child tokens from all registered tokens.
    pub fn prune_cancelled(&mut self) -> usize {
        self.tokens
            .values()
            .map(|t| t.prune_cancelled_children())
            .sum()
    }

    /// Deterministic canonical string for testing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "coordinator|scopes={}|cancelled={}|events={}|finalizers={}",
            self.tokens.len(),
            self.cancelled_count(),
            self.events.len(),
            self.finalizers.values().map(Vec::len).sum::<usize>(),
        )
    }
}

impl Default for ShutdownCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope_tree::{register_standard_scopes, well_known};

    fn setup_tree_and_coordinator() -> (ScopeTree, ShutdownCoordinator) {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();
        register_standard_scopes(&mut tree, 1000).unwrap();

        let mut coord = ShutdownCoordinator::new();

        // Register root
        coord
            .register_scope(&ScopeId::root(), ScopeTier::Root, None)
            .unwrap();

        // Register standard scopes
        let daemons = [
            well_known::discovery(),
            well_known::capture(),
            well_known::relay(),
            well_known::persistence(),
            well_known::maintenance(),
        ];
        for id in &daemons {
            coord
                .register_scope(id, ScopeTier::Daemon, Some(&ScopeId::root()))
                .unwrap();
            tree.start(id, 1100).unwrap();
        }

        let watchers = [
            well_known::native_events(),
            well_known::snapshot(),
            well_known::config_reload(),
        ];
        for id in &watchers {
            coord
                .register_scope(id, ScopeTier::Watcher, Some(&ScopeId::root()))
                .unwrap();
            tree.start(id, 1100).unwrap();
        }

        (tree, coord)
    }

    #[test]
    fn shutdown_reason_display() {
        assert_eq!(ShutdownReason::UserRequested.to_string(), "user-requested");
        assert_eq!(
            ShutdownReason::Timeout {
                deadline_ms: 5000,
                elapsed_ms: 6000
            }
            .to_string(),
            "timeout(deadline=5000ms, elapsed=6000ms)"
        );
    }

    #[test]
    fn shutdown_policy_tier_defaults() {
        let root = ShutdownPolicy::for_tier(ScopeTier::Root);
        assert_eq!(root.grace_period_ms, 30_000);
        assert!(root.cascade_to_children);

        let eph = ShutdownPolicy::for_tier(ScopeTier::Ephemeral);
        assert_eq!(eph.grace_period_ms, 1_000);
        assert!(!eph.run_finalizers);
    }

    #[test]
    fn cancellation_token_basic() {
        let token = CancellationToken::new(ScopeId("test".into()));
        assert!(!token.is_cancelled());
        assert_eq!(token.generation(), 0);
        assert!(token.reason().is_none());

        token.cancel(ShutdownReason::UserRequested);
        assert!(token.is_cancelled());
        assert_eq!(token.generation(), 1);
        assert_eq!(token.reason(), Some(ShutdownReason::UserRequested));
    }

    #[test]
    fn cancellation_propagates_to_children() {
        let parent = CancellationToken::new(ScopeId("parent".into()));
        let child = parent.child(ScopeId("child".into()));
        let grandchild = child.child(ScopeId("grandchild".into()));

        assert!(!parent.is_cancelled());
        assert!(!child.is_cancelled());
        assert!(!grandchild.is_cancelled());

        parent.cancel(ShutdownReason::UserRequested);

        assert!(parent.is_cancelled());
        assert!(child.is_cancelled());
        assert!(grandchild.is_cancelled());

        // Child/grandchild should have ParentShutdown reason
        let child_reason = child.reason().unwrap();
        assert!(
            matches!(child_reason, ShutdownReason::ParentShutdown { parent_id } if parent_id.0 == "parent")
        );

        let gc_reason = grandchild.reason().unwrap();
        assert!(
            matches!(gc_reason, ShutdownReason::ParentShutdown { parent_id } if parent_id.0 == "child")
        );
    }

    #[test]
    fn cancellation_idempotent() {
        let token = CancellationToken::new(ScopeId("test".into()));
        token.cancel(ShutdownReason::UserRequested);
        token.cancel(ShutdownReason::GracefulTermination);

        // First cancel wins
        assert_eq!(token.reason(), Some(ShutdownReason::UserRequested));
        assert_eq!(token.generation(), 1); // Only incremented once
    }

    #[test]
    fn child_of_cancelled_parent_starts_cancelled() {
        let parent = CancellationToken::new(ScopeId("parent".into()));
        parent.cancel(ShutdownReason::UserRequested);

        let child = parent.child(ScopeId("child".into()));
        assert!(child.is_cancelled());
        assert!(matches!(
            child.reason(),
            Some(ShutdownReason::ParentShutdown { .. })
        ));
    }

    #[test]
    fn prune_cancelled_children() {
        let parent = CancellationToken::new(ScopeId("parent".into()));
        let _c1 = parent.child(ScopeId("c1".into()));
        let c2 = parent.child(ScopeId("c2".into()));
        let _c3 = parent.child(ScopeId("c3".into()));

        assert_eq!(parent.child_count(), 3);

        c2.cancel(ShutdownReason::UserRequested);
        let pruned = parent.prune_cancelled_children();
        assert_eq!(pruned, 1);
        assert_eq!(parent.child_count(), 2);
    }

    #[test]
    fn finalizer_ordering_by_priority() {
        let mut coord = ShutdownCoordinator::new();
        let scope = ScopeId("test".into());
        coord
            .register_scope(&scope, ScopeTier::Worker, None)
            .unwrap();

        // Register in non-priority order
        coord
            .register_finalizer(
                &scope,
                Finalizer {
                    name: "low".into(),
                    priority: 10,
                    action: FinalizerAction::Custom {
                        action_name: "low".into(),
                        metadata: HashMap::new(),
                    },
                    status: FinalizerStatus::Pending,
                },
            )
            .unwrap();
        coord
            .register_finalizer(
                &scope,
                Finalizer {
                    name: "high".into(),
                    priority: 100,
                    action: FinalizerAction::Custom {
                        action_name: "high".into(),
                        metadata: HashMap::new(),
                    },
                    status: FinalizerStatus::Pending,
                },
            )
            .unwrap();
        coord
            .register_finalizer(
                &scope,
                Finalizer {
                    name: "medium".into(),
                    priority: 50,
                    action: FinalizerAction::Custom {
                        action_name: "medium".into(),
                        metadata: HashMap::new(),
                    },
                    status: FinalizerStatus::Pending,
                },
            )
            .unwrap();

        let fns = coord.finalizers(&scope);
        assert_eq!(fns[0].name, "high");
        assert_eq!(fns[1].name, "medium");
        assert_eq!(fns[2].name, "low");
    }

    #[test]
    fn coordinator_register_and_policy() {
        let mut coord = ShutdownCoordinator::new();
        let scope = ScopeId("test".into());

        let token = coord
            .register_scope(&scope, ScopeTier::Daemon, None)
            .unwrap();
        assert!(!token.is_cancelled());

        // Default daemon policy
        assert_eq!(coord.policy(&scope).grace_period_ms, 15_000);

        // Override policy
        coord
            .set_policy(
                &scope,
                ShutdownPolicy {
                    grace_period_ms: 60_000,
                    ..ShutdownPolicy::for_tier(ScopeTier::Daemon)
                },
            )
            .unwrap();
        assert_eq!(coord.policy(&scope).grace_period_ms, 60_000);
    }

    #[test]
    fn coordinator_duplicate_scope_rejected() {
        let mut coord = ShutdownCoordinator::new();
        let scope = ScopeId("test".into());
        coord
            .register_scope(&scope, ScopeTier::Worker, None)
            .unwrap();

        let err = coord
            .register_scope(&scope, ScopeTier::Worker, None)
            .unwrap_err();
        assert!(matches!(
            err,
            ShutdownCoordinatorError::ScopeAlreadyRegistered { .. }
        ));
    }

    #[test]
    fn full_shutdown_lifecycle() {
        let (mut tree, mut coord) = setup_tree_and_coordinator();

        // Register a worker under capture
        tree.register(
            well_known::capture_worker(0),
            ScopeTier::Worker,
            &well_known::capture(),
            "w0",
            1200,
        )
        .unwrap();
        tree.start(&well_known::capture_worker(0), 1300).unwrap();
        coord
            .register_scope(
                &well_known::capture_worker(0),
                ScopeTier::Worker,
                Some(&well_known::capture()),
            )
            .unwrap();

        // Register a finalizer on the worker
        coord
            .register_finalizer(
                &well_known::capture_worker(0),
                Finalizer {
                    name: "flush-buffer".into(),
                    priority: 100,
                    action: FinalizerAction::FlushChannel {
                        channel_name: "capture-out".into(),
                    },
                    status: FinalizerStatus::Pending,
                },
            )
            .unwrap();

        // Phase 1: Request shutdown of worker
        let cascaded = coord
            .request_shutdown(
                &mut tree,
                &well_known::capture_worker(0),
                ShutdownReason::UserRequested,
                2000,
            )
            .unwrap();
        assert!(cascaded.is_empty()); // Workers don't cascade

        assert_eq!(
            tree.get(&well_known::capture_worker(0)).unwrap().state,
            ScopeState::Draining
        );
        assert!(
            coord
                .token(&well_known::capture_worker(0))
                .unwrap()
                .is_cancelled()
        );

        // Phase 2: Begin finalize
        coord
            .begin_finalize(&mut tree, &well_known::capture_worker(0), 500, 2500)
            .unwrap();
        assert_eq!(
            tree.get(&well_known::capture_worker(0)).unwrap().state,
            ScopeState::Finalizing
        );

        // Execute finalizer
        coord
            .mark_finalizer_started(&well_known::capture_worker(0), "flush-buffer", 2500)
            .unwrap();
        coord
            .mark_finalizer_completed(&well_known::capture_worker(0), "flush-buffer", 50, 2550)
            .unwrap();

        // Complete shutdown
        let summary = coord
            .complete_shutdown(&mut tree, &well_known::capture_worker(0), 2600)
            .unwrap();
        assert_eq!(
            tree.get(&well_known::capture_worker(0)).unwrap().state,
            ScopeState::Closed
        );
        assert_eq!(summary.finalizers_run, 1);
        assert_eq!(summary.finalizers_succeeded, 1);
        assert_eq!(summary.finalizers_failed, 0);
        assert!(!summary.escalated);
    }

    #[test]
    fn cascade_shutdown_to_children() {
        let (mut tree, mut coord) = setup_tree_and_coordinator();

        // Add workers under capture
        for i in 0..3 {
            tree.register(
                well_known::capture_worker(i),
                ScopeTier::Worker,
                &well_known::capture(),
                format!("w{i}"),
                1200,
            )
            .unwrap();
            tree.start(&well_known::capture_worker(i), 1300).unwrap();
            coord
                .register_scope(
                    &well_known::capture_worker(i),
                    ScopeTier::Worker,
                    Some(&well_known::capture()),
                )
                .unwrap();
        }

        // Shut down capture daemon — should cascade to all workers
        let cascaded = coord
            .request_shutdown(
                &mut tree,
                &well_known::capture(),
                ShutdownReason::UserRequested,
                3000,
            )
            .unwrap();

        assert_eq!(cascaded.len(), 3);
        for i in 0..3 {
            assert_eq!(
                tree.get(&well_known::capture_worker(i)).unwrap().state,
                ScopeState::Draining
            );
        }
    }

    #[test]
    fn grace_period_check() {
        let (mut tree, mut coord) = setup_tree_and_coordinator();

        // Set a short grace period on discovery
        coord
            .set_policy(
                &well_known::discovery(),
                ShutdownPolicy {
                    grace_period_ms: 500,
                    ..ShutdownPolicy::for_tier(ScopeTier::Daemon)
                },
            )
            .unwrap();

        // Request shutdown
        coord
            .request_shutdown(
                &mut tree,
                &well_known::discovery(),
                ShutdownReason::UserRequested,
                5000,
            )
            .unwrap();

        // Not expired yet
        assert!(!coord.is_grace_expired(&tree, &well_known::discovery(), 5400));

        // Expired
        assert!(coord.is_grace_expired(&tree, &well_known::discovery(), 5600));
    }

    #[test]
    fn escalation_force_close() {
        let (mut tree, mut coord) = setup_tree_and_coordinator();

        coord
            .set_policy(
                &well_known::discovery(),
                ShutdownPolicy {
                    grace_period_ms: 100,
                    escalation: EscalationAction::ForceClose,
                    ..ShutdownPolicy::for_tier(ScopeTier::Daemon)
                },
            )
            .unwrap();

        coord
            .request_shutdown(
                &mut tree,
                &well_known::discovery(),
                ShutdownReason::UserRequested,
                5000,
            )
            .unwrap();

        // Grace expired → force close
        let action = coord
            .handle_grace_expiry(&mut tree, &well_known::discovery(), 5200)
            .unwrap();
        assert_eq!(action, EscalationAction::ForceClose);
        assert_eq!(
            tree.get(&well_known::discovery()).unwrap().state,
            ScopeState::Closed
        );
    }

    #[test]
    fn escalation_extend_grace() {
        let (mut tree, mut coord) = setup_tree_and_coordinator();

        coord
            .set_policy(
                &well_known::relay(),
                ShutdownPolicy {
                    grace_period_ms: 100,
                    escalation: EscalationAction::ExtendGrace { extra_ms: 500 },
                    ..ShutdownPolicy::for_tier(ScopeTier::Daemon)
                },
            )
            .unwrap();

        coord
            .request_shutdown(
                &mut tree,
                &well_known::relay(),
                ShutdownReason::UserRequested,
                5000,
            )
            .unwrap();

        let action = coord
            .handle_grace_expiry(&mut tree, &well_known::relay(), 5200)
            .unwrap();
        assert!(matches!(action, EscalationAction::ExtendGrace { .. }));

        // Grace period extended to 600ms total
        assert_eq!(coord.policy(&well_known::relay()).grace_period_ms, 600);
        assert!(!coord.is_grace_expired(&tree, &well_known::relay(), 5500));
        assert!(coord.is_grace_expired(&tree, &well_known::relay(), 5700));
    }

    #[test]
    fn finalizer_failure_tracked() {
        let mut coord = ShutdownCoordinator::new();
        let scope = ScopeId("test".into());
        coord
            .register_scope(&scope, ScopeTier::Worker, None)
            .unwrap();

        coord
            .register_finalizer(
                &scope,
                Finalizer {
                    name: "risky-op".into(),
                    priority: 50,
                    action: FinalizerAction::PersistState {
                        key: "state".into(),
                    },
                    status: FinalizerStatus::Pending,
                },
            )
            .unwrap();

        coord
            .mark_finalizer_started(&scope, "risky-op", 1000)
            .unwrap();
        coord
            .mark_finalizer_failed(&scope, "risky-op", "disk full", 200, 1200)
            .unwrap();

        let fns = coord.finalizers(&scope);
        assert!(matches!(
            fns[0].status,
            FinalizerStatus::Failed { ref error, .. } if error == "disk full"
        ));
    }

    #[test]
    fn events_emitted_during_lifecycle() {
        let (mut tree, mut coord) = setup_tree_and_coordinator();
        coord.set_correlation_prefix("test-run");

        coord
            .request_shutdown(
                &mut tree,
                &well_known::discovery(),
                ShutdownReason::UserRequested,
                5000,
            )
            .unwrap();

        let events = coord.events_for_scope(&well_known::discovery());
        assert!(events.len() >= 2); // ShutdownRequested + DrainStarted

        // Check correlation IDs are set
        for event in &events {
            assert!(event.correlation_id.is_some());
        }

        // Verify event types
        assert!(matches!(
            events[0].event_type,
            ShutdownEventType::ShutdownRequested { .. }
        ));
        assert!(matches!(
            events[1].event_type,
            ShutdownEventType::DrainStarted { .. }
        ));
    }

    #[test]
    fn shutdown_summary_accuracy() {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let scope = ScopeId("worker:test:0".into());
        tree.register(
            scope.clone(),
            ScopeTier::Worker,
            &ScopeId::root(),
            "test-worker",
            1000,
        )
        .unwrap();
        tree.start(&scope, 1100).unwrap();

        let mut coord = ShutdownCoordinator::new();
        coord
            .register_scope(&ScopeId::root(), ScopeTier::Root, None)
            .unwrap();
        coord
            .register_scope(&scope, ScopeTier::Worker, Some(&ScopeId::root()))
            .unwrap();

        // Register 2 finalizers
        coord
            .register_finalizer(
                &scope,
                Finalizer {
                    name: "f1".into(),
                    priority: 100,
                    action: FinalizerAction::FlushChannel {
                        channel_name: "ch".into(),
                    },
                    status: FinalizerStatus::Pending,
                },
            )
            .unwrap();
        coord
            .register_finalizer(
                &scope,
                Finalizer {
                    name: "f2".into(),
                    priority: 50,
                    action: FinalizerAction::PersistState {
                        key: "state".into(),
                    },
                    status: FinalizerStatus::Pending,
                },
            )
            .unwrap();

        // Full lifecycle
        coord
            .request_shutdown(&mut tree, &scope, ShutdownReason::GracefulTermination, 2000)
            .unwrap();
        coord.begin_finalize(&mut tree, &scope, 800, 2800).unwrap();

        coord.mark_finalizer_started(&scope, "f1", 2800).unwrap();
        coord
            .mark_finalizer_completed(&scope, "f1", 50, 2850)
            .unwrap();
        coord.mark_finalizer_started(&scope, "f2", 2850).unwrap();
        coord
            .mark_finalizer_failed(&scope, "f2", "io error", 30, 2880)
            .unwrap();

        let summary = coord.complete_shutdown(&mut tree, &scope, 2900).unwrap();
        assert_eq!(summary.reason, ShutdownReason::GracefulTermination);
        assert_eq!(summary.finalizers_run, 2);
        assert_eq!(summary.finalizers_succeeded, 1);
        assert_eq!(summary.finalizers_failed, 1);
        assert_eq!(summary.finalizers_skipped, 0);
        assert!(!summary.escalated);
        assert_eq!(summary.total_elapsed_ms, 900); // 2900 - 2000
    }

    #[test]
    fn canonical_string_deterministic() {
        let coord = ShutdownCoordinator::new();
        let s1 = coord.canonical_string();
        let s2 = coord.canonical_string();
        assert_eq!(s1, s2);
        assert_eq!(s1, "coordinator|scopes=0|cancelled=0|events=0|finalizers=0");
    }

    #[test]
    fn serde_roundtrip_shutdown_reason() {
        let reasons = vec![
            ShutdownReason::UserRequested,
            ShutdownReason::GracefulTermination,
            ShutdownReason::Timeout {
                deadline_ms: 5000,
                elapsed_ms: 6000,
            },
            ShutdownReason::ChildError {
                child_id: ScopeId("child".into()),
                error_msg: "panic".into(),
            },
            ShutdownReason::CascadingFailure {
                origin_id: ScopeId("origin".into()),
            },
            ShutdownReason::ResourceExhausted {
                resource: "fds".into(),
            },
            ShutdownReason::PolicyViolation {
                rule: "max-rate".into(),
            },
            ShutdownReason::ParentShutdown {
                parent_id: ScopeId("parent".into()),
            },
        ];

        for reason in reasons {
            let json = serde_json::to_string(&reason).unwrap();
            let restored: ShutdownReason = serde_json::from_str(&json).unwrap();
            assert_eq!(reason, restored);
        }
    }

    #[test]
    fn serde_roundtrip_shutdown_policy() {
        let policy = ShutdownPolicy::for_tier(ScopeTier::Daemon);
        let json = serde_json::to_string(&policy).unwrap();
        let restored: ShutdownPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(policy, restored);
    }

    #[test]
    fn serde_roundtrip_shutdown_event() {
        let event = ShutdownEvent {
            timestamp_ms: 1234,
            scope_id: ScopeId("test".into()),
            event_type: ShutdownEventType::ShutdownRequested {
                reason: ShutdownReason::UserRequested,
            },
            correlation_id: Some("test-123".into()),
        };

        let json = serde_json::to_string(&event).unwrap();
        let restored: ShutdownEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.timestamp_ms, restored.timestamp_ms);
        assert_eq!(event.scope_id, restored.scope_id);
    }

    #[test]
    fn serde_roundtrip_finalizer() {
        let finalizer = Finalizer {
            name: "flush".into(),
            priority: 100,
            action: FinalizerAction::FlushChannel {
                channel_name: "capture".into(),
            },
            status: FinalizerStatus::Completed { duration_ms: 50 },
        };

        let json = serde_json::to_string(&finalizer).unwrap();
        let restored: Finalizer = serde_json::from_str(&json).unwrap();
        assert_eq!(finalizer, restored);
    }

    #[test]
    fn serde_roundtrip_shutdown_summary() {
        let summary = ShutdownSummary {
            scope_id: ScopeId("test".into()),
            reason: ShutdownReason::UserRequested,
            drain_elapsed_ms: 500,
            finalize_elapsed_ms: 200,
            total_elapsed_ms: 700,
            finalizers_run: 3,
            finalizers_succeeded: 2,
            finalizers_failed: 1,
            finalizers_skipped: 0,
            cascaded_children: 2,
            escalated: false,
        };

        let json = serde_json::to_string(&summary).unwrap();
        let restored: ShutdownSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(summary.scope_id, restored.scope_id);
        assert_eq!(summary.total_elapsed_ms, restored.total_elapsed_ms);
    }

    #[test]
    fn unregistered_scope_errors() {
        let mut coord = ShutdownCoordinator::new();
        let scope = ScopeId("missing".into());

        assert!(matches!(
            coord.set_policy(&scope, ShutdownPolicy::default()),
            Err(ShutdownCoordinatorError::ScopeNotRegistered { .. })
        ));

        assert!(matches!(
            coord.register_finalizer(
                &scope,
                Finalizer {
                    name: "f".into(),
                    priority: 0,
                    action: FinalizerAction::Custom {
                        action_name: "noop".into(),
                        metadata: HashMap::new(),
                    },
                    status: FinalizerStatus::Pending,
                }
            ),
            Err(ShutdownCoordinatorError::ScopeNotRegistered { .. })
        ));
    }

    #[test]
    fn finalizer_action_display() {
        assert_eq!(
            FinalizerAction::FlushChannel {
                channel_name: "out".into()
            }
            .to_string(),
            "flush(out)"
        );
        assert_eq!(
            FinalizerAction::CloseConnection { conn_id: 42 }.to_string(),
            "close-conn(42)"
        );
    }
}
