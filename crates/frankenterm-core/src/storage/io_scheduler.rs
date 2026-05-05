//! Bounded storage IO scheduler substrate.
//!
//! This module is intentionally pure logic. It defines the admission,
//! queueing, ordering, fairness, and snapshot contracts that later storage
//! routing beads can wire into the SQLite writer, FTS catch-up, cold-tier
//! hydration, and resource cockpit surfaces.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};

/// Stable storage IO work classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageIoClass {
    PaneSegmentDurable,
    GapAndContinuity,
    PolicyAudit,
    WorkflowEvent,
    FtsIncremental,
    FtsRebuild,
    SearchIndexState,
    ColdTierWrite,
    ColdTierRead,
    StorageMaintenance,
}

impl StorageIoClass {
    pub const ALL: [Self; 10] = [
        Self::PaneSegmentDurable,
        Self::GapAndContinuity,
        Self::PolicyAudit,
        Self::WorkflowEvent,
        Self::FtsIncremental,
        Self::FtsRebuild,
        Self::SearchIndexState,
        Self::ColdTierWrite,
        Self::ColdTierRead,
        Self::StorageMaintenance,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PaneSegmentDurable => "pane_segment_durable",
            Self::GapAndContinuity => "gap_and_continuity",
            Self::PolicyAudit => "policy_audit",
            Self::WorkflowEvent => "workflow_event",
            Self::FtsIncremental => "fts_incremental",
            Self::FtsRebuild => "fts_rebuild",
            Self::SearchIndexState => "search_index_state",
            Self::ColdTierWrite => "cold_tier_write",
            Self::ColdTierRead => "cold_tier_read",
            Self::StorageMaintenance => "storage_maintenance",
        }
    }
}

/// Admission outcome emitted by the scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageIoAdmissionOutcome {
    Admit,
    Batch,
    Defer,
    Degrade,
    Shed,
    FailClosed,
}

impl StorageIoAdmissionOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admit => "admit",
            Self::Batch => "batch",
            Self::Defer => "defer",
            Self::Degrade => "degrade",
            Self::Shed => "shed",
            Self::FailClosed => "fail_closed",
        }
    }

    #[must_use]
    pub const fn accepted(self) -> bool {
        matches!(self, Self::Admit | Self::Batch)
    }
}

/// Stable low-cardinality reason for an admission decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageIoReason {
    WithinBudget,
    QueueFull,
    ClassBudgetExhausted,
    OldestAgeExceeded,
    IoError,
    SearchFreshnessLag,
    ColdTierUnavailable,
    AuditRequired,
    OperatorDisabled,
    ChaosInjection,
}

impl StorageIoReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WithinBudget => "within_budget",
            Self::QueueFull => "queue_full",
            Self::ClassBudgetExhausted => "class_budget_exhausted",
            Self::OldestAgeExceeded => "oldest_age_exceeded",
            Self::IoError => "io_error",
            Self::SearchFreshnessLag => "search_freshness_lag",
            Self::ColdTierUnavailable => "cold_tier_unavailable",
            Self::AuditRequired => "audit_required",
            Self::OperatorDisabled => "operator_disabled",
            Self::ChaosInjection => "chaos_injection",
        }
    }
}

/// Coarse storage IO pressure tier for operator surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageIoPressureTier {
    Green,
    Yellow,
    Red,
    Black,
}

impl StorageIoPressureTier {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Yellow => "yellow",
            Self::Red => "red",
            Self::Black => "black",
        }
    }
}

/// Ordering token for work that must preserve stream order across classes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StorageIoOrdering {
    pub stream: String,
    pub sequence: u64,
}

impl StorageIoOrdering {
    #[must_use]
    pub fn new(stream: impl Into<String>, sequence: u64) -> Self {
        Self {
            stream: stream.into(),
            sequence,
        }
    }
}

/// Work item admitted by the storage IO scheduler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageIoWorkItem {
    pub id: u64,
    pub class: StorageIoClass,
    pub estimated_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ordering: Option<StorageIoOrdering>,
}

impl StorageIoWorkItem {
    #[must_use]
    pub const fn new(id: u64, class: StorageIoClass, estimated_bytes: u64) -> Self {
        Self {
            id,
            class,
            estimated_bytes,
            ordering: None,
        }
    }

    #[must_use]
    pub fn with_ordering(mut self, stream: impl Into<String>, sequence: u64) -> Self {
        self.ordering = Some(StorageIoOrdering::new(stream, sequence));
        self
    }
}

/// Per-class budget and admission policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageIoClassBudget {
    pub max_items: u64,
    pub max_bytes: u64,
    pub priority: u8,
    pub allow_batch: bool,
    pub defer_when_full: bool,
    pub shed_when_full: bool,
    pub fail_closed_when_full: bool,
}

impl StorageIoClassBudget {
    #[must_use]
    pub const fn strict(max_items: u64, max_bytes: u64, priority: u8) -> Self {
        Self {
            max_items,
            max_bytes,
            priority,
            allow_batch: false,
            defer_when_full: false,
            shed_when_full: false,
            fail_closed_when_full: true,
        }
    }

    #[must_use]
    pub const fn deferrable(max_items: u64, max_bytes: u64, priority: u8) -> Self {
        Self {
            max_items,
            max_bytes,
            priority,
            allow_batch: true,
            defer_when_full: true,
            shed_when_full: false,
            fail_closed_when_full: false,
        }
    }

    #[must_use]
    pub const fn sheddable(max_items: u64, max_bytes: u64, priority: u8) -> Self {
        Self {
            max_items,
            max_bytes,
            priority,
            allow_batch: true,
            defer_when_full: true,
            shed_when_full: true,
            fail_closed_when_full: false,
        }
    }
}

/// Scheduler configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageIoSchedulerConfig {
    pub aggregate_max_items: u64,
    pub aggregate_max_bytes: u64,
    pub max_consecutive_per_class: usize,
    pub class_budgets: HashMap<StorageIoClass, StorageIoClassBudget>,
}

impl Default for StorageIoSchedulerConfig {
    fn default() -> Self {
        let mib = 1_048_576;
        let mut class_budgets = HashMap::new();
        class_budgets.insert(
            StorageIoClass::GapAndContinuity,
            StorageIoClassBudget::strict(1024, 16 * mib, 0),
        );
        class_budgets.insert(
            StorageIoClass::PolicyAudit,
            StorageIoClassBudget::strict(2048, 16 * mib, 0),
        );
        class_budgets.insert(
            StorageIoClass::PaneSegmentDurable,
            StorageIoClassBudget::deferrable(8192, 256 * mib, 1),
        );
        class_budgets.insert(
            StorageIoClass::ColdTierRead,
            StorageIoClassBudget::deferrable(2048, 128 * mib, 2),
        );
        class_budgets.insert(
            StorageIoClass::WorkflowEvent,
            StorageIoClassBudget::deferrable(4096, 64 * mib, 3),
        );
        class_budgets.insert(
            StorageIoClass::FtsIncremental,
            StorageIoClassBudget::deferrable(4096, 128 * mib, 4),
        );
        class_budgets.insert(
            StorageIoClass::FtsRebuild,
            StorageIoClassBudget::deferrable(128, 256 * mib, 5),
        );
        class_budgets.insert(
            StorageIoClass::SearchIndexState,
            StorageIoClassBudget::deferrable(4096, 128 * mib, 5),
        );
        class_budgets.insert(
            StorageIoClass::ColdTierWrite,
            StorageIoClassBudget::deferrable(4096, 512 * mib, 6),
        );
        class_budgets.insert(
            StorageIoClass::StorageMaintenance,
            StorageIoClassBudget::sheddable(512, 64 * mib, 7),
        );

        Self {
            aggregate_max_items: 16_384,
            aggregate_max_bytes: 1024 * mib,
            max_consecutive_per_class: 8,
            class_budgets,
        }
    }
}

impl StorageIoSchedulerConfig {
    #[must_use]
    pub fn class_budget(&self, class: StorageIoClass) -> StorageIoClassBudget {
        self.class_budgets
            .get(&class)
            .copied()
            .unwrap_or_else(|| StorageIoClassBudget::deferrable(1024, 64 * 1024 * 1024, 7))
    }
}

/// Admission decision returned to callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageIoAdmissionDecision {
    pub class: StorageIoClass,
    pub outcome: StorageIoAdmissionOutcome,
    pub reason: StorageIoReason,
    pub queued: bool,
    pub queue_depth: u64,
    pub bytes_pending: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl StorageIoAdmissionDecision {
    #[must_use]
    pub fn reason_code(&self) -> String {
        format!(
            "storage_io.{}.{}",
            self.outcome.as_str(),
            self.reason.as_str()
        )
    }
}

/// Work item returned by the scheduler for dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageIoDispatchedWork {
    pub item: StorageIoWorkItem,
    pub queued_for_ms: u64,
}

#[derive(Debug, Clone)]
struct QueuedWork {
    item: StorageIoWorkItem,
    admitted_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DispatchCandidate {
    class: StorageIoClass,
    index: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageIoClassCounters {
    pub admitted_total: u64,
    pub batched_total: u64,
    pub deferred_total: u64,
    pub degraded_total: u64,
    pub shed_total: u64,
    pub fail_closed_total: u64,
    pub write_error_total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_reason_code: Option<String>,
}

/// Per-class snapshot row for operator and test surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageIoClassSnapshot {
    pub class: StorageIoClass,
    pub class_name: String,
    pub queue_depth: u64,
    pub queue_capacity: u64,
    pub bytes_pending: u64,
    pub bytes_capacity: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_queued_age_ms: Option<u64>,
    pub admitted_total: u64,
    pub batched_total: u64,
    pub deferred_total: u64,
    pub degraded_total: u64,
    pub shed_total: u64,
    pub fail_closed_total: u64,
    pub write_error_total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_reason_code: Option<String>,
}

/// Global scheduler snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageIoSchedulerSnapshot {
    pub schema_version: u32,
    pub aggregate_queue_depth: u64,
    pub aggregate_bytes_pending: u64,
    pub io_pressure_tier: StorageIoPressureTier,
    pub io_pressure_reason: String,
    pub search_lag_segments: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_lag_oldest_age_ms: Option<u64>,
    pub hydration_lag_pages: u64,
    pub audit_fail_closed_total: u64,
    pub durability_pending_total: u64,
    pub classes: Vec<StorageIoClassSnapshot>,
}

/// Dominant IO class surfaced in operator summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageIoDominantClassSummary {
    pub class: StorageIoClass,
    pub class_name: String,
    pub queue_depth: u64,
    pub bytes_pending: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_queued_age_ms: Option<u64>,
    pub fail_closed_total: u64,
    pub write_error_total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
}

/// Stable IO-pressure summary for resource cockpit and structured logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageIoOperatorSummary {
    pub schema_version: u32,
    pub pressure_domain: String,
    pub io_pressure_tier: StorageIoPressureTier,
    pub io_pressure_reason: String,
    pub operator_action: String,
    pub aggregate_queue_depth: u64,
    pub aggregate_bytes_pending: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_queued_age_ms: Option<u64>,
    pub durability_pending_total: u64,
    pub search_lag_segments: u64,
    pub hydration_lag_pages: u64,
    pub audit_fail_closed_total: u64,
    pub write_error_total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dominant_class: Option<StorageIoDominantClassSummary>,
}

impl StorageIoSchedulerSnapshot {
    #[must_use]
    pub fn operator_summary(&self) -> StorageIoOperatorSummary {
        let write_error_total = self.classes.iter().fold(0_u64, |total, row| {
            total.saturating_add(row.write_error_total)
        });

        StorageIoOperatorSummary {
            schema_version: self.schema_version,
            pressure_domain: "storage_io".to_string(),
            io_pressure_tier: self.io_pressure_tier,
            io_pressure_reason: self.io_pressure_reason.clone(),
            operator_action: operator_action_for_summary(
                self.io_pressure_tier,
                self.audit_fail_closed_total,
                write_error_total,
            )
            .to_string(),
            aggregate_queue_depth: self.aggregate_queue_depth,
            aggregate_bytes_pending: self.aggregate_bytes_pending,
            oldest_queued_age_ms: self
                .classes
                .iter()
                .filter_map(|row| row.oldest_queued_age_ms)
                .max(),
            durability_pending_total: self.durability_pending_total,
            search_lag_segments: self.search_lag_segments,
            hydration_lag_pages: self.hydration_lag_pages,
            audit_fail_closed_total: self.audit_fail_closed_total,
            write_error_total,
            dominant_class: self.dominant_class_summary(),
        }
    }

    fn dominant_class_summary(&self) -> Option<StorageIoDominantClassSummary> {
        self.classes
            .iter()
            .filter(|row| class_has_operator_signal(row))
            .max_by_key(|row| {
                (
                    class_operator_rank(row),
                    row.oldest_queued_age_ms.unwrap_or(0),
                    row.queue_depth,
                    row.bytes_pending,
                )
            })
            .map(|row| StorageIoDominantClassSummary {
                class: row.class,
                class_name: row.class_name.clone(),
                queue_depth: row.queue_depth,
                bytes_pending: row.bytes_pending,
                oldest_queued_age_ms: row.oldest_queued_age_ms,
                fail_closed_total: row.fail_closed_total,
                write_error_total: row.write_error_total,
                reason_code: row.last_reason_code.clone(),
            })
    }
}

/// Pure in-memory storage IO scheduler.
#[derive(Debug, Clone)]
pub struct StorageIoScheduler {
    config: StorageIoSchedulerConfig,
    queues: HashMap<StorageIoClass, VecDeque<QueuedWork>>,
    counters: HashMap<StorageIoClass, StorageIoClassCounters>,
    aggregate_items: u64,
    aggregate_bytes: u64,
    last_served_class: Option<StorageIoClass>,
    consecutive_from_last_class: usize,
}

impl StorageIoScheduler {
    #[must_use]
    pub fn new(config: StorageIoSchedulerConfig) -> Self {
        let mut queues = HashMap::new();
        let mut counters = HashMap::new();
        for class in StorageIoClass::ALL {
            queues.insert(class, VecDeque::new());
            counters.insert(class, StorageIoClassCounters::default());
        }
        Self {
            config,
            queues,
            counters,
            aggregate_items: 0,
            aggregate_bytes: 0,
            last_served_class: None,
            consecutive_from_last_class: 0,
        }
    }

    #[must_use]
    pub fn config(&self) -> &StorageIoSchedulerConfig {
        &self.config
    }

    #[must_use]
    pub const fn aggregate_items(&self) -> u64 {
        self.aggregate_items
    }

    #[must_use]
    pub fn aggregate_bytes(&self) -> u64 {
        self.aggregate_bytes_pending_saturating()
    }

    pub fn admit(&mut self, item: StorageIoWorkItem, now_ms: u64) -> StorageIoAdmissionDecision {
        let class = item.class;
        let budget = self.config.class_budget(class);
        let item_bytes = item.estimated_bytes.max(1);
        let class_items = self.class_queue_depth(class);
        let class_bytes = self.class_bytes_pending(class);

        if class_items.saturating_add(1) > budget.max_items
            || class_bytes.saturating_add(item_bytes) > budget.max_bytes
        {
            return self.reject(class, budget, StorageIoReason::ClassBudgetExhausted);
        }

        if self.aggregate_items.saturating_add(1) > self.config.aggregate_max_items
            || self.aggregate_bytes.saturating_add(item_bytes) > self.config.aggregate_max_bytes
        {
            return self.reject(class, budget, StorageIoReason::QueueFull);
        }

        let outcome = if budget.allow_batch && class_items > 0 {
            StorageIoAdmissionOutcome::Batch
        } else {
            StorageIoAdmissionOutcome::Admit
        };

        self.queues.entry(class).or_default().push_back(QueuedWork {
            item,
            admitted_at_ms: now_ms,
        });
        self.aggregate_items = self.aggregate_items.saturating_add(1);
        self.aggregate_bytes = self.aggregate_bytes_pending_saturating();

        let queued_depth = self.class_queue_depth(class);
        let bytes_pending = self.class_bytes_pending(class);
        let decision = StorageIoAdmissionDecision {
            class,
            outcome,
            reason: StorageIoReason::WithinBudget,
            queued: true,
            queue_depth: queued_depth,
            bytes_pending,
            retry_after_ms: None,
        };
        self.record_decision(class, outcome, decision.reason_code());
        decision
    }

    pub fn pop_next(&mut self, now_ms: u64) -> Option<StorageIoDispatchedWork> {
        let candidate = self.select_next_candidate()?;
        let queued = self
            .queues
            .get_mut(&candidate.class)?
            .remove(candidate.index)?;
        self.aggregate_items = self.aggregate_items.saturating_sub(1);
        self.aggregate_bytes = self.aggregate_bytes_pending_saturating();

        if self.last_served_class == Some(candidate.class) {
            self.consecutive_from_last_class = self.consecutive_from_last_class.saturating_add(1);
        } else {
            self.last_served_class = Some(candidate.class);
            self.consecutive_from_last_class = 1;
        }

        Some(StorageIoDispatchedWork {
            queued_for_ms: now_ms.saturating_sub(queued.admitted_at_ms),
            item: queued.item,
        })
    }

    #[must_use]
    pub fn snapshot(&self, now_ms: u64) -> StorageIoSchedulerSnapshot {
        let mut classes = Vec::with_capacity(StorageIoClass::ALL.len());
        for class in StorageIoClass::ALL {
            let budget = self.config.class_budget(class);
            let counters = self.counters.get(&class).cloned().unwrap_or_default();
            classes.push(StorageIoClassSnapshot {
                class,
                class_name: class.as_str().to_string(),
                queue_depth: self.class_queue_depth(class),
                queue_capacity: budget.max_items,
                bytes_pending: self.class_bytes_pending(class),
                bytes_capacity: budget.max_bytes,
                oldest_queued_age_ms: self.oldest_queued_age_ms(class, now_ms),
                admitted_total: counters.admitted_total,
                batched_total: counters.batched_total,
                deferred_total: counters.deferred_total,
                degraded_total: counters.degraded_total,
                shed_total: counters.shed_total,
                fail_closed_total: counters.fail_closed_total,
                write_error_total: counters.write_error_total,
                last_reason_code: counters.last_reason_code,
            });
        }

        let pressure = self.pressure_tier();
        StorageIoSchedulerSnapshot {
            schema_version: 1,
            aggregate_queue_depth: self.aggregate_items,
            aggregate_bytes_pending: self.aggregate_bytes_pending_saturating(),
            io_pressure_tier: pressure,
            io_pressure_reason: pressure_reason(pressure).to_string(),
            search_lag_segments: 0,
            search_lag_oldest_age_ms: None,
            hydration_lag_pages: 0,
            audit_fail_closed_total: self
                .counters
                .get(&StorageIoClass::PolicyAudit)
                .map_or(0, |c| c.fail_closed_total),
            durability_pending_total: self.durability_pending_total(),
            classes,
        }
    }

    #[must_use]
    pub fn class_queue_depth(&self, class: StorageIoClass) -> u64 {
        self.queues
            .get(&class)
            .map_or(0, |q| usize_to_u64_saturating(q.len()))
    }

    #[must_use]
    pub fn class_bytes_pending(&self, class: StorageIoClass) -> u64 {
        self.queues
            .get(&class)
            .map_or(0, queue_estimated_bytes_saturating)
    }

    #[must_use]
    pub fn oldest_queued_age_ms(&self, class: StorageIoClass, now_ms: u64) -> Option<u64> {
        self.queues
            .get(&class)
            .and_then(|queue| queue.front())
            .map(|queued| now_ms.saturating_sub(queued.admitted_at_ms))
    }

    pub fn record_write_error(&mut self, class: StorageIoClass, reason: StorageIoReason) {
        let counters = self.counters.entry(class).or_default();
        counters.write_error_total = counters.write_error_total.saturating_add(1);
        counters.last_reason_code = Some(format!("storage_io.write_error.{}", reason.as_str()));
    }

    fn reject(
        &mut self,
        class: StorageIoClass,
        budget: StorageIoClassBudget,
        reason: StorageIoReason,
    ) -> StorageIoAdmissionDecision {
        let outcome = if budget.fail_closed_when_full {
            StorageIoAdmissionOutcome::FailClosed
        } else if budget.shed_when_full {
            StorageIoAdmissionOutcome::Shed
        } else if budget.defer_when_full {
            StorageIoAdmissionOutcome::Defer
        } else {
            StorageIoAdmissionOutcome::FailClosed
        };

        let reason = if outcome == StorageIoAdmissionOutcome::FailClosed
            && class == StorageIoClass::PolicyAudit
        {
            StorageIoReason::AuditRequired
        } else {
            reason
        };

        let decision = StorageIoAdmissionDecision {
            class,
            outcome,
            reason,
            queued: false,
            queue_depth: self.class_queue_depth(class),
            bytes_pending: self.class_bytes_pending(class),
            retry_after_ms: if outcome == StorageIoAdmissionOutcome::Defer {
                Some(100)
            } else {
                None
            },
        };
        self.record_decision(class, outcome, decision.reason_code());
        decision
    }

    fn record_decision(
        &mut self,
        class: StorageIoClass,
        outcome: StorageIoAdmissionOutcome,
        reason_code: String,
    ) {
        let counters = self.counters.entry(class).or_default();
        match outcome {
            StorageIoAdmissionOutcome::Admit => {
                counters.admitted_total = counters.admitted_total.saturating_add(1);
            }
            StorageIoAdmissionOutcome::Batch => {
                counters.batched_total = counters.batched_total.saturating_add(1);
            }
            StorageIoAdmissionOutcome::Defer => {
                counters.deferred_total = counters.deferred_total.saturating_add(1);
            }
            StorageIoAdmissionOutcome::Degrade => {
                counters.degraded_total = counters.degraded_total.saturating_add(1);
            }
            StorageIoAdmissionOutcome::Shed => {
                counters.shed_total = counters.shed_total.saturating_add(1);
            }
            StorageIoAdmissionOutcome::FailClosed => {
                counters.fail_closed_total = counters.fail_closed_total.saturating_add(1);
            }
        }
        counters.last_reason_code = Some(reason_code);
    }

    fn select_next_candidate(&self) -> Option<DispatchCandidate> {
        let mut candidates = self.dispatch_candidates();
        candidates.sort_by_key(|candidate| self.config.class_budget(candidate.class).priority);

        if candidates.len() > 1
            && self
                .last_served_class
                .is_some_and(|class| candidates.iter().any(|candidate| candidate.class == class))
            && self.consecutive_from_last_class >= self.config.max_consecutive_per_class
        {
            if let Some(last) = self.last_served_class {
                if let Some(candidate) = candidates
                    .iter()
                    .copied()
                    .find(|candidate| candidate.class != last)
                {
                    return Some(candidate);
                }
            }
        }

        candidates.into_iter().next()
    }

    fn dispatch_candidates(&self) -> Vec<DispatchCandidate> {
        StorageIoClass::ALL
            .iter()
            .copied()
            .filter_map(|class| {
                self.first_dispatchable_index(class)
                    .map(|index| DispatchCandidate { class, index })
            })
            .collect()
    }

    fn first_dispatchable_index(&self, class: StorageIoClass) -> Option<usize> {
        self.queues
            .get(&class)?
            .iter()
            .position(|queued| !self.has_earlier_ordered_work(queued))
    }

    fn has_earlier_ordered_work(&self, candidate: &QueuedWork) -> bool {
        let Some(ordering) = &candidate.item.ordering else {
            return false;
        };

        self.queues.values().any(|queue| {
            queue.iter().any(|queued| {
                queued.item.ordering.as_ref().is_some_and(|other| {
                    other.stream == ordering.stream && other.sequence < ordering.sequence
                })
            })
        })
    }

    fn pressure_tier(&self) -> StorageIoPressureTier {
        if ratio_at_least(
            self.aggregate_items,
            self.config.aggregate_max_items,
            95,
            100,
        ) || ratio_at_least(
            self.aggregate_bytes_pending_saturating(),
            self.config.aggregate_max_bytes,
            95,
            100,
        ) {
            StorageIoPressureTier::Black
        } else if ratio_at_least(
            self.aggregate_items,
            self.config.aggregate_max_items,
            80,
            100,
        ) || ratio_at_least(
            self.aggregate_bytes_pending_saturating(),
            self.config.aggregate_max_bytes,
            80,
            100,
        ) {
            StorageIoPressureTier::Red
        } else if ratio_at_least(
            self.aggregate_items,
            self.config.aggregate_max_items,
            60,
            100,
        ) || ratio_at_least(
            self.aggregate_bytes_pending_saturating(),
            self.config.aggregate_max_bytes,
            60,
            100,
        ) {
            StorageIoPressureTier::Yellow
        } else {
            StorageIoPressureTier::Green
        }
    }

    fn durability_pending_total(&self) -> u64 {
        self.class_queue_depth(StorageIoClass::PaneSegmentDurable)
            .saturating_add(self.class_queue_depth(StorageIoClass::GapAndContinuity))
            .saturating_add(self.class_queue_depth(StorageIoClass::PolicyAudit))
    }

    fn aggregate_bytes_pending_saturating(&self) -> u64 {
        self.queues.values().fold(0, |total, queue| {
            total.saturating_add(queue_estimated_bytes_saturating(queue))
        })
    }
}

fn usize_to_u64_saturating(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn queue_estimated_bytes_saturating(queue: &VecDeque<QueuedWork>) -> u64 {
    queue.iter().fold(0, |total, queued| {
        total.saturating_add(queued.item.estimated_bytes.max(1))
    })
}

fn ratio_at_least(value: u64, cap: u64, numerator: u64, denominator: u64) -> bool {
    if denominator == 0 {
        return false;
    }
    if cap == 0 {
        value > 0
    } else {
        u128::from(value) * u128::from(denominator) >= u128::from(cap) * u128::from(numerator)
    }
}

fn pressure_reason(tier: StorageIoPressureTier) -> &'static str {
    match tier {
        StorageIoPressureTier::Green => "storage_io.admit.within_budget",
        StorageIoPressureTier::Yellow => "storage_io.defer.queue_full",
        StorageIoPressureTier::Red => "storage_io.degrade.oldest_age_exceeded",
        StorageIoPressureTier::Black => "storage_io.fail_closed.queue_full",
    }
}

fn operator_action_for_summary(
    tier: StorageIoPressureTier,
    audit_fail_closed_total: u64,
    write_error_total: u64,
) -> &'static str {
    if audit_fail_closed_total > 0 || write_error_total > 0 {
        "investigate_io_fail_closed"
    } else {
        match tier {
            StorageIoPressureTier::Green => "continue",
            StorageIoPressureTier::Yellow => "watch_io_queue",
            StorageIoPressureTier::Red => "throttle_or_defer_io",
            StorageIoPressureTier::Black => "fail_closed_or_shed_optional_io",
        }
    }
}

fn class_has_operator_signal(row: &StorageIoClassSnapshot) -> bool {
    row.queue_depth > 0
        || row.deferred_total > 0
        || row.degraded_total > 0
        || row.shed_total > 0
        || row.fail_closed_total > 0
        || row.write_error_total > 0
}

fn class_operator_rank(row: &StorageIoClassSnapshot) -> u8 {
    if row.write_error_total > 0 {
        5
    } else if row.fail_closed_total > 0 {
        4
    } else if row.shed_total > 0 {
        3
    } else if row.degraded_total > 0 {
        2
    } else if row.deferred_total > 0 {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_config() -> StorageIoSchedulerConfig {
        let mut cfg = StorageIoSchedulerConfig {
            aggregate_max_items: 4,
            aggregate_max_bytes: 4096,
            max_consecutive_per_class: 1,
            ..StorageIoSchedulerConfig::default()
        };
        cfg.class_budgets.insert(
            StorageIoClass::PaneSegmentDurable,
            StorageIoClassBudget::deferrable(1, 1024, 1),
        );
        cfg.class_budgets.insert(
            StorageIoClass::PolicyAudit,
            StorageIoClassBudget::strict(1, 1024, 0),
        );
        cfg.class_budgets.insert(
            StorageIoClass::FtsIncremental,
            StorageIoClassBudget::deferrable(4, 4096, 4),
        );
        cfg.class_budgets.insert(
            StorageIoClass::StorageMaintenance,
            StorageIoClassBudget::sheddable(4, 4096, 7),
        );
        cfg
    }

    #[test]
    fn class_budget_defers_segment_when_full() {
        let mut scheduler = StorageIoScheduler::new(tiny_config());
        let first = scheduler.admit(
            StorageIoWorkItem::new(1, StorageIoClass::PaneSegmentDurable, 100),
            10,
        );
        let second = scheduler.admit(
            StorageIoWorkItem::new(2, StorageIoClass::PaneSegmentDurable, 100),
            11,
        );

        assert_eq!(first.outcome, StorageIoAdmissionOutcome::Admit);
        assert_eq!(second.outcome, StorageIoAdmissionOutcome::Defer);
        assert_eq!(second.reason, StorageIoReason::ClassBudgetExhausted);
        assert!(!second.queued);
        assert_eq!(
            scheduler.class_queue_depth(StorageIoClass::PaneSegmentDurable),
            1
        );
    }

    #[test]
    fn aggregate_budget_sheds_optional_maintenance() {
        let mut cfg = tiny_config();
        cfg.aggregate_max_items = 1;
        let mut scheduler = StorageIoScheduler::new(cfg);

        assert_eq!(
            scheduler
                .admit(
                    StorageIoWorkItem::new(1, StorageIoClass::PaneSegmentDurable, 100),
                    10
                )
                .outcome,
            StorageIoAdmissionOutcome::Admit
        );
        let decision = scheduler.admit(
            StorageIoWorkItem::new(2, StorageIoClass::StorageMaintenance, 100),
            11,
        );

        assert_eq!(decision.outcome, StorageIoAdmissionOutcome::Shed);
        assert_eq!(decision.reason, StorageIoReason::QueueFull);
        assert_eq!(
            scheduler
                .snapshot(20)
                .classes
                .iter()
                .find(|row| row.class == StorageIoClass::StorageMaintenance)
                .unwrap()
                .shed_total,
            1
        );
    }

    #[test]
    fn policy_audit_fails_closed_when_budget_exhausted() {
        let mut scheduler = StorageIoScheduler::new(tiny_config());
        assert_eq!(
            scheduler
                .admit(
                    StorageIoWorkItem::new(1, StorageIoClass::PolicyAudit, 100),
                    10
                )
                .outcome,
            StorageIoAdmissionOutcome::Admit
        );
        let decision = scheduler.admit(
            StorageIoWorkItem::new(2, StorageIoClass::PolicyAudit, 100),
            11,
        );

        assert_eq!(decision.outcome, StorageIoAdmissionOutcome::FailClosed);
        assert_eq!(decision.reason, StorageIoReason::AuditRequired);
        assert_eq!(
            decision.reason_code(),
            "storage_io.fail_closed.audit_required"
        );
    }

    #[test]
    fn dispatch_preserves_ordering_key_across_classes() {
        let mut scheduler = StorageIoScheduler::new(tiny_config());
        scheduler.admit(
            StorageIoWorkItem::new(2, StorageIoClass::GapAndContinuity, 100)
                .with_ordering("pane:7", 2),
            10,
        );
        scheduler.admit(
            StorageIoWorkItem::new(1, StorageIoClass::PaneSegmentDurable, 100)
                .with_ordering("pane:7", 1),
            11,
        );

        let first = scheduler.pop_next(20).unwrap();
        let second = scheduler.pop_next(21).unwrap();

        assert_eq!(first.item.id, 1);
        assert_eq!(second.item.id, 2);
    }

    #[test]
    fn dispatch_reorders_same_class_ordered_work_to_avoid_deadlock() {
        let mut cfg = tiny_config();
        cfg.class_budgets
            .get_mut(&StorageIoClass::FtsIncremental)
            .unwrap()
            .max_items = 4;
        let mut scheduler = StorageIoScheduler::new(cfg);
        scheduler.admit(
            StorageIoWorkItem::new(2, StorageIoClass::FtsIncremental, 100)
                .with_ordering("pane:7", 2),
            10,
        );
        scheduler.admit(
            StorageIoWorkItem::new(1, StorageIoClass::FtsIncremental, 100)
                .with_ordering("pane:7", 1),
            11,
        );

        let first = scheduler.pop_next(20).unwrap();
        let second = scheduler.pop_next(21).unwrap();

        assert_eq!(first.item.id, 1);
        assert_eq!(second.item.id, 2);
    }

    #[test]
    fn class_bytes_pending_saturates_instead_of_overflowing() {
        let mut cfg = tiny_config();
        cfg.aggregate_max_bytes = u64::MAX;
        cfg.class_budgets.insert(
            StorageIoClass::FtsIncremental,
            StorageIoClassBudget::deferrable(4, u64::MAX, 4),
        );
        let mut scheduler = StorageIoScheduler::new(cfg);

        scheduler.admit(
            StorageIoWorkItem::new(1, StorageIoClass::FtsIncremental, u64::MAX),
            10,
        );
        scheduler.admit(
            StorageIoWorkItem::new(2, StorageIoClass::FtsIncremental, u64::MAX),
            11,
        );

        assert_eq!(
            scheduler.class_bytes_pending(StorageIoClass::FtsIncremental),
            u64::MAX
        );
    }

    #[test]
    fn aggregate_bytes_pending_recomputes_after_saturated_pop() {
        let mut cfg = tiny_config();
        cfg.aggregate_max_bytes = u64::MAX;
        cfg.class_budgets.insert(
            StorageIoClass::FtsIncremental,
            StorageIoClassBudget::deferrable(4, u64::MAX, 4),
        );
        let mut scheduler = StorageIoScheduler::new(cfg);

        scheduler.admit(
            StorageIoWorkItem::new(1, StorageIoClass::FtsIncremental, u64::MAX),
            10,
        );
        scheduler.admit(
            StorageIoWorkItem::new(2, StorageIoClass::FtsIncremental, u64::MAX),
            11,
        );

        assert_eq!(scheduler.aggregate_bytes(), u64::MAX);
        assert_eq!(scheduler.pop_next(20).unwrap().item.id, 1);
        assert_eq!(scheduler.aggregate_bytes(), u64::MAX);
        assert_eq!(scheduler.snapshot(21).aggregate_bytes_pending, u64::MAX);
    }

    #[test]
    fn ratio_check_uses_wide_math_for_large_caps() {
        assert!(!ratio_at_least(10, u64::MAX, 95, 100));
        assert!(ratio_at_least(u64::MAX - 1, u64::MAX, 95, 100));
    }

    #[test]
    fn fair_service_interleaves_two_ready_classes() {
        let mut cfg = tiny_config();
        cfg.class_budgets
            .get_mut(&StorageIoClass::PolicyAudit)
            .unwrap()
            .max_items = 4;
        let mut scheduler = StorageIoScheduler::new(cfg);

        scheduler.admit(
            StorageIoWorkItem::new(1, StorageIoClass::PolicyAudit, 100),
            10,
        );
        scheduler.admit(
            StorageIoWorkItem::new(2, StorageIoClass::PolicyAudit, 100),
            11,
        );
        scheduler.admit(
            StorageIoWorkItem::new(3, StorageIoClass::FtsIncremental, 100),
            12,
        );
        scheduler.admit(
            StorageIoWorkItem::new(4, StorageIoClass::FtsIncremental, 100),
            13,
        );

        assert_eq!(
            scheduler.pop_next(20).unwrap().item.class,
            StorageIoClass::PolicyAudit
        );
        assert_eq!(
            scheduler.pop_next(21).unwrap().item.class,
            StorageIoClass::FtsIncremental
        );
        assert_eq!(
            scheduler.pop_next(22).unwrap().item.class,
            StorageIoClass::PolicyAudit
        );
    }

    #[test]
    fn snapshot_reports_depth_age_bytes_and_pressure() {
        let mut cfg = tiny_config();
        cfg.aggregate_max_items = 2;
        let mut scheduler = StorageIoScheduler::new(cfg);
        scheduler.admit(
            StorageIoWorkItem::new(1, StorageIoClass::PaneSegmentDurable, 512),
            10,
        );
        scheduler.admit(
            StorageIoWorkItem::new(2, StorageIoClass::FtsIncremental, 512),
            20,
        );

        let snapshot = scheduler.snapshot(70);
        assert_eq!(snapshot.schema_version, 1);
        assert_eq!(snapshot.aggregate_queue_depth, 2);
        assert_eq!(snapshot.aggregate_bytes_pending, 1024);
        assert_eq!(snapshot.io_pressure_tier, StorageIoPressureTier::Black);
        assert_eq!(snapshot.durability_pending_total, 1);

        let segment = snapshot
            .classes
            .iter()
            .find(|row| row.class == StorageIoClass::PaneSegmentDurable)
            .unwrap();
        assert_eq!(segment.queue_depth, 1);
        assert_eq!(segment.bytes_pending, 512);
        assert_eq!(segment.oldest_queued_age_ms, Some(60));

        let summary = snapshot.operator_summary();
        assert_eq!(summary.pressure_domain, "storage_io");
        assert_eq!(summary.operator_action, "fail_closed_or_shed_optional_io");
        assert_eq!(summary.oldest_queued_age_ms, Some(60));
        assert_eq!(
            summary.dominant_class.as_ref().map(|row| row.class),
            Some(StorageIoClass::PaneSegmentDurable)
        );
    }

    #[test]
    fn operator_summary_reports_no_dominant_class_when_idle() {
        let scheduler = StorageIoScheduler::new(tiny_config());
        let summary = scheduler.snapshot(10).operator_summary();

        assert_eq!(summary.io_pressure_tier, StorageIoPressureTier::Green);
        assert_eq!(summary.operator_action, "continue");
        assert_eq!(summary.write_error_total, 0);
        assert!(summary.dominant_class.is_none());
    }

    #[test]
    fn write_error_updates_operator_summary_reason_code() {
        let mut scheduler = StorageIoScheduler::new(tiny_config());
        scheduler.record_write_error(StorageIoClass::SearchIndexState, StorageIoReason::IoError);

        let summary = scheduler.snapshot(10).operator_summary();
        let dominant = summary.dominant_class.unwrap();

        assert_eq!(summary.write_error_total, 1);
        assert_eq!(summary.operator_action, "investigate_io_fail_closed");
        assert_eq!(dominant.class, StorageIoClass::SearchIndexState);
        assert_eq!(dominant.write_error_total, 1);
        assert_eq!(
            dominant.reason_code.as_deref(),
            Some("storage_io.write_error.io_error")
        );
    }
}
