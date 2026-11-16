//! Layout restoration engine for the currently supported mux topology subset.
//!
//! Given a [`TopologySnapshot`] captured by the session persistence system,
//! this module recreates windows, tabs, pane splits, working directories, and
//! active-pane/tab selection using mux spawn and split operations exposed by
//! the `MuxInterface` trait. Domain identity, titles, window appearance, exact
//! cell geometry, and terminal render state are not restored here.
//!
//! # Data flow
//!
//! ```text
//! TopologySnapshot → LayoutRestorer → WeztermInterface (spawn/split) → PaneIdMap
//! ```
//!
//! The returned [`RestoreResult`] contains a mapping from old pane IDs (in the
//! snapshot) to new pane IDs (in the live mux session). Downstream restore
//! accounting uses that map to report which historical process replacements
//! require manual action; captured scrollback is never sent through PTY input.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::error::RuntimeOperationSource;
use crate::session_topology::{
    MAX_PANE_TREE_DEPTH, PaneNode, TOPOLOGY_SCHEMA_VERSION, TabSnapshot, TopologySnapshot,
    WindowSnapshot,
};
use crate::wezterm::{SpawnTarget, SplitDirection, WeztermHandle};

fn restore_context_error(
    cx: &crate::cx::Cx,
    operation: &'static str,
    error: &crate::runtime_async::ContextError,
) -> crate::Error {
    use crate::outcome::CancelKind;
    use crate::runtime_async::ContextErrorKind;

    let source = match error.kind() {
        ContextErrorKind::DeadlineExceeded => RuntimeOperationSource::DeadlineExceeded,
        ContextErrorKind::PollQuotaExhausted => RuntimeOperationSource::PollQuotaExhausted,
        ContextErrorKind::CostQuotaExhausted => RuntimeOperationSource::CostBudgetExhausted,
        ContextErrorKind::CancelTimeout => RuntimeOperationSource::CancellationCleanupTimedOut,
        ContextErrorKind::Cancelled => match cx.root_cancel_cause().map(|reason| reason.kind) {
            Some(CancelKind::Deadline | CancelKind::Timeout) => {
                RuntimeOperationSource::DeadlineExceeded
            }
            Some(CancelKind::PollQuota) => RuntimeOperationSource::PollQuotaExhausted,
            Some(CancelKind::CostBudget) => RuntimeOperationSource::CostBudgetExhausted,
            Some(
                CancelKind::User
                | CancelKind::FailFast
                | CancelKind::RaceLost
                | CancelKind::ParentCancelled
                | CancelKind::ResourceUnavailable
                | CancelKind::Shutdown
                | CancelKind::LinkedExit,
            ) => RuntimeOperationSource::Cancelled("caller capability stopped".to_string()),
            None => {
                let budget = cx.budget_stats();
                if budget.deadline.at.is_some() && budget.deadline.remaining.is_none() {
                    RuntimeOperationSource::DeadlineExceeded
                } else if budget.polls.remaining == Some(0) {
                    RuntimeOperationSource::PollQuotaExhausted
                } else if budget.cost.remaining == Some(0) {
                    RuntimeOperationSource::CostBudgetExhausted
                } else {
                    RuntimeOperationSource::ContextFailure
                }
            }
        },
        _ => RuntimeOperationSource::ContextFailure,
    };
    crate::Error::RuntimeOperation { operation, source }
}

fn restore_checkpoint(cx: &crate::cx::Cx, operation: &'static str) -> crate::Result<()> {
    cx.checkpoint()
        .map_err(|error| restore_context_error(cx, operation, &error))
}

fn restore_layout_error(operation: &'static str, detail: &'static str) -> crate::Error {
    crate::Error::RuntimeOperation {
        operation,
        source: RuntimeOperationSource::Backend(detail.to_string()),
    }
}

fn restore_layout_integrity_error(operation: &'static str) -> crate::Error {
    crate::Error::RuntimeOperation {
        operation,
        source: RuntimeOperationSource::Backend(
            "restore layout state integrity check failed".to_string(),
        ),
    }
}

const FAILURE_SPLIT: &str = "pane split creation failed";
const FAILURE_ACTIVATION: &str = "pane activation failed";
const MAX_DETAILED_FAILURE_LOGS: usize = 20;
const MAX_RESTORED_CWD_BYTES: usize = 4 * 1024;
const MAX_ENCODED_RESTORED_CWD_BYTES: usize = MAX_RESTORED_CWD_BYTES * 3 + 64;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for layout restoration behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RestoreConfig {
    /// Restore working directories for each pane.
    pub restore_working_dirs: bool,
    /// Attempt to restore approximate split ratios.
    pub restore_split_ratios: bool,
    /// Continue after an authoritative finite per-pane rejection. When false,
    /// restoration stops and returns the truthful partial [`RestoreResult`].
    /// Uncertain, cancellation, budget, context, and integrity errors always
    /// propagate immediately regardless of this setting.
    pub continue_on_error: bool,
}

impl Default for RestoreConfig {
    fn default() -> Self {
        Self {
            restore_working_dirs: true,
            restore_split_ratios: true,
            continue_on_error: true,
        }
    }
}

// =============================================================================
// Result types
// =============================================================================

/// Result of a layout restoration operation.
#[derive(Clone)]
pub struct RestoreResult {
    /// Mapping from old pane IDs (snapshot) to new pane IDs (live session).
    pub pane_id_map: HashMap<u64, u64>,
    /// Pane failures (old pane ID → finite content-free reason code).
    pub failed_panes: Vec<(u64, String)>,
    /// Number of windows created.
    pub windows_created: usize,
    /// Number of tabs created.
    pub tabs_created: usize,
    /// Total number of panes created.
    pub panes_created: usize,
}

impl std::fmt::Debug for RestoreResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestoreResult")
            .field("pane_mapping_count", &self.pane_id_map.len())
            .field("failed_pane_count", &self.failed_panes.len())
            .field("windows_created", &self.windows_created)
            .field("tabs_created", &self.tabs_created)
            .field("panes_created", &self.panes_created)
            .finish()
    }
}

impl RestoreResult {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_capacity(0)
    }

    fn with_capacity(pane_capacity: usize) -> Self {
        Self {
            pane_id_map: HashMap::with_capacity(pane_capacity),
            // Successful restores are the common path; do not reserve one
            // failure allocation per pane up front.
            failed_panes: Vec::new(),
            windows_created: 0,
            tabs_created: 0,
            panes_created: 0,
        }
    }
}

/// Finite reason that a layout attempt stopped before the complete topology
/// settled. No backend message, cwd, title, command, or pane content is
/// retained in this classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutRestoreInterruptionReason {
    Cancelled,
    CancellationCleanupTimedOut,
    DeadlineExceeded,
    PollQuotaExhausted,
    CostQuotaExhausted,
    ContextFailure,
    ValidationFailure,
    BackendFailure,
    MuxOutcomeIndeterminate,
    IntegrityFailure,
}

/// Typed, content-free terminal state for a partial layout attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutRestoreInterruption {
    /// Finite code-owned operation label at which restoration stopped.
    pub phase: &'static str,
    /// Finite interruption class.
    pub reason: LayoutRestoreInterruptionReason,
}

/// Complete or partial receipt from one layout attempt.
///
/// `result` contains every authoritative successful spawn/split receipt that
/// was observed before `interruption`. The original crate error is retained
/// privately only so the legacy `Result` API can preserve its exact error
/// variant; diagnostics for this type are aggregate and content-free.
pub struct LayoutRestoreAttempt {
    pub result: RestoreResult,
    pub interruption: Option<LayoutRestoreInterruption>,
    terminal_error: Option<crate::Error>,
}

impl std::fmt::Debug for LayoutRestoreAttempt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LayoutRestoreAttempt")
            .field("result", &self.result)
            .field("interruption", &self.interruption)
            .finish()
    }
}

impl LayoutRestoreAttempt {
    fn complete(result: RestoreResult) -> Self {
        Self {
            result,
            interruption: None,
            terminal_error: None,
        }
    }

    fn interrupted(result: RestoreResult, terminal_error: crate::Error) -> Self {
        let interruption = Some(classify_layout_restore_error(&terminal_error));
        Self {
            result,
            interruption,
            terminal_error: Some(terminal_error),
        }
    }

    fn into_result(self) -> crate::Result<RestoreResult> {
        let Self {
            result,
            terminal_error,
            ..
        } = self;
        match terminal_error {
            Some(error) => Err(error),
            None => Ok(result),
        }
    }
}

#[allow(deprecated)]
fn classify_layout_restore_error(error: &crate::Error) -> LayoutRestoreInterruption {
    use crate::error::{RuntimeOperationSource, WeztermError};

    let (phase, reason) = match error {
        crate::Error::RuntimeOperation { operation, source } => {
            let reason = match source {
                RuntimeOperationSource::Cancelled(_) => LayoutRestoreInterruptionReason::Cancelled,
                RuntimeOperationSource::DeadlineExceeded => {
                    LayoutRestoreInterruptionReason::DeadlineExceeded
                }
                RuntimeOperationSource::PollQuotaExhausted => {
                    LayoutRestoreInterruptionReason::PollQuotaExhausted
                }
                RuntimeOperationSource::CostBudgetExhausted => {
                    LayoutRestoreInterruptionReason::CostQuotaExhausted
                }
                RuntimeOperationSource::ContextFailure => {
                    LayoutRestoreInterruptionReason::ContextFailure
                }
                RuntimeOperationSource::CancellationCleanupTimedOut => {
                    LayoutRestoreInterruptionReason::CancellationCleanupTimedOut
                }
                RuntimeOperationSource::LockPoisoned
                | RuntimeOperationSource::PolledAfterCompletion => {
                    LayoutRestoreInterruptionReason::IntegrityFailure
                }
                RuntimeOperationSource::Backend(_) if *operation == "restore_layout.preflight" => {
                    LayoutRestoreInterruptionReason::ValidationFailure
                }
                RuntimeOperationSource::Backend(_)
                    if operation.starts_with("restore_layout.pane_mapping.")
                        || operation.starts_with("restore_layout.receipt.")
                        || operation.starts_with("restore_layout.split_plan.") =>
                {
                    LayoutRestoreInterruptionReason::IntegrityFailure
                }
                RuntimeOperationSource::WatchChannelClosed
                | RuntimeOperationSource::Backend(_)
                | RuntimeOperationSource::LockTimedOut => {
                    LayoutRestoreInterruptionReason::BackendFailure
                }
            };
            (*operation, reason)
        }
        crate::Error::Wezterm(WeztermError::IndeterminateMutation { operation }) => (
            *operation,
            LayoutRestoreInterruptionReason::MuxOutcomeIndeterminate,
        ),
        crate::Error::Wezterm(_) => (
            "restore_layout.mux_operation",
            LayoutRestoreInterruptionReason::BackendFailure,
        ),
        crate::Error::PaneOperation { operation, .. } => {
            (*operation, LayoutRestoreInterruptionReason::BackendFailure)
        }
        crate::Error::Cancelled(_) => (
            "restore_layout.operation",
            LayoutRestoreInterruptionReason::Cancelled,
        ),
        crate::Error::Panicked(_) => (
            "restore_layout.operation",
            LayoutRestoreInterruptionReason::IntegrityFailure,
        ),
        crate::Error::Json(_) | crate::Error::Config(_) => (
            "restore_layout.preflight",
            LayoutRestoreInterruptionReason::ValidationFailure,
        ),
        crate::Error::CaptureAuthority(_)
        | crate::Error::Storage(_)
        | crate::Error::Pattern(_)
        | crate::Error::Workflow(_)
        | crate::Error::Policy(_)
        | crate::Error::Io(_)
        | crate::Error::WatchdogWarningRead { .. }
        | crate::Error::Runtime(_)
        | crate::Error::SetupError(_) => (
            "restore_layout.operation",
            LayoutRestoreInterruptionReason::BackendFailure,
        ),
    };
    LayoutRestoreInterruption { phase, reason }
}

struct RestoreAccumulator {
    result: RestoreResult,
    failed_pane_ids: HashSet<u64>,
    mapped_new_pane_ids: HashSet<u64>,
    failure_logs_emitted: usize,
    failure_logs_suppressed: usize,
}

impl RestoreAccumulator {
    fn with_capacity(pane_capacity: usize) -> Self {
        Self {
            result: RestoreResult::with_capacity(pane_capacity),
            failed_pane_ids: HashSet::new(),
            mapped_new_pane_ids: HashSet::with_capacity(pane_capacity),
            failure_logs_emitted: 0,
            failure_logs_suppressed: 0,
        }
    }

    fn record_failure(&mut self, pane_id: u64, reason: &'static str) -> bool {
        if !self.failed_pane_ids.insert(pane_id) {
            return false;
        }
        self.result.failed_panes.push((pane_id, reason.to_string()));
        true
    }

    fn record_unmapped_failure(&mut self, pane_id: u64, reason: &'static str) -> bool {
        if self.result.pane_id_map.contains_key(&pane_id) {
            return false;
        }
        self.record_failure(pane_id, reason)
    }

    fn claim_failure_log_slot(&mut self) -> bool {
        if self.failure_logs_emitted < MAX_DETAILED_FAILURE_LOGS {
            self.failure_logs_emitted += 1;
            true
        } else {
            self.failure_logs_suppressed = self.failure_logs_suppressed.saturating_add(1);
            false
        }
    }

    fn record_created_pane(&mut self, old_pane_id: u64, new_pane_id: u64) -> crate::Result<()> {
        if let Some(existing_new_pane_id) = self.result.pane_id_map.get(&old_pane_id) {
            if *existing_new_pane_id == new_pane_id {
                return Ok(());
            }
            return Err(restore_layout_integrity_error(
                "restore_layout.pane_mapping.old_id_conflict",
            ));
        }
        if self.mapped_new_pane_ids.contains(&new_pane_id) {
            return Err(restore_layout_integrity_error(
                "restore_layout.pane_mapping.new_id_conflict",
            ));
        }
        let panes_created = self.result.panes_created.checked_add(1).ok_or_else(|| {
            restore_layout_integrity_error("restore_layout.pane_mapping.count_overflow")
        })?;
        self.mapped_new_pane_ids.insert(new_pane_id);
        self.result.pane_id_map.insert(old_pane_id, new_pane_id);
        self.result.panes_created = panes_created;
        Ok(())
    }

    fn record_created_tab(&mut self, created_new_window: bool) -> crate::Result<()> {
        let tabs_created = self.result.tabs_created.checked_add(1).ok_or_else(|| {
            restore_layout_integrity_error("restore_layout.receipt.tab_count_overflow")
        })?;
        let windows_created = if created_new_window {
            self.result.windows_created.checked_add(1).ok_or_else(|| {
                restore_layout_integrity_error("restore_layout.receipt.window_count_overflow")
            })?
        } else {
            self.result.windows_created
        };
        self.result.tabs_created = tabs_created;
        self.result.windows_created = windows_created;
        Ok(())
    }

    fn validate_receipt_invariants(&self) -> crate::Result<()> {
        if self.result.panes_created != self.result.pane_id_map.len()
            || self.result.pane_id_map.len() != self.mapped_new_pane_ids.len()
        {
            return Err(restore_layout_integrity_error(
                "restore_layout.receipt.pane_count_mismatch",
            ));
        }
        if self.result.failed_panes.len() != self.failed_pane_ids.len() {
            return Err(restore_layout_integrity_error(
                "restore_layout.receipt.failure_count_mismatch",
            ));
        }
        if self.result.windows_created > self.result.tabs_created
            || self.result.tabs_created > self.result.panes_created
        {
            return Err(restore_layout_integrity_error(
                "restore_layout.receipt.hierarchy_count_mismatch",
            ));
        }
        Ok(())
    }
}

struct RestorePreflight {
    pane_count: usize,
    normalized_cwds: HashMap<u64, String>,
    split_percents: HashMap<u64, u8>,
}

#[derive(Debug, Clone, Copy)]
struct ValidatedPaneTree {
    pane_count: usize,
    active_leaf_id: Option<u64>,
    first_leaf_id: u64,
}

#[derive(Debug, Clone, Copy)]
struct RestoredTab {
    window_id: u64,
    active_old_pane_id: u64,
    active_new_pane_id: u64,
    flow: RestoreFlow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreFlow {
    Continue,
    Stop,
}

impl RestoreFlow {
    const fn should_stop(self) -> bool {
        matches!(self, Self::Stop)
    }
}

#[derive(Debug, Clone, Copy)]
struct RestoredWindow {
    flow: RestoreFlow,
}

// =============================================================================
// Layout restorer
// =============================================================================

/// Engine that recreates mux session layout from a topology snapshot.
pub struct LayoutRestorer {
    wezterm: WeztermHandle,
    config: RestoreConfig,
}

impl LayoutRestorer {
    /// Create a new layout restorer.
    pub fn new(wezterm: WeztermHandle, config: RestoreConfig) -> Self {
        Self { wezterm, config }
    }

    /// Restore the full topology from a snapshot.
    ///
    /// Creates windows, tabs, and pane splits to match the captured layout.
    /// Returns a mapping from old pane IDs to new pane IDs, including every
    /// authoritative successful spawn/split receipt before a finite rejection.
    /// Fatal or uncertain errors propagate through this compatibility wrapper.
    /// Callers that must retain acknowledged partial mux effects use
    /// [`Self::restore_attempt_with_cx`]; `SessionRestorer` uses that typed
    /// attempt surface before committing its durable outcome receipt.
    pub async fn restore(&self, snapshot: &TopologySnapshot) -> crate::Result<RestoreResult> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.restore_with_cx(&cx, snapshot).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`restore`].
    ///
    /// Pre-flight checkpoint gates the restoration before any
    /// mux spawn/split calls fire; additionally, a per-window
    /// checkpoint between iterations makes cancellation
    /// responsive for multi-window restores where each window
    /// involves multiple expensive spawn operations.
    pub async fn restore_with_cx(
        &self,
        cx: &crate::cx::Cx,
        snapshot: &TopologySnapshot,
    ) -> crate::Result<RestoreResult> {
        self.restore_attempt_with_cx(cx, snapshot)
            .await
            .into_result()
    }

    /// Restore a topology while retaining a truthful partial receipt if a
    /// fatal, uncertain, cancellation, budget, or integrity error occurs after
    /// one or more authoritative mux mutations.
    pub async fn restore_attempt_with_cx(
        &self,
        cx: &crate::cx::Cx,
        snapshot: &TopologySnapshot,
    ) -> LayoutRestoreAttempt {
        if let Err(error) = restore_checkpoint(cx, "restore_layout.restore.preflight") {
            return LayoutRestoreAttempt::interrupted(RestoreResult::with_capacity(0), error);
        }

        // Validate the complete immutable snapshot before the first mux mutation.
        // A malformed later window/tab must never be discovered only after earlier
        // windows have already been created.
        let preflight = match validate_restore_snapshot(
            snapshot,
            self.config.restore_working_dirs,
            self.config.restore_split_ratios,
        ) {
            Ok(preflight) => preflight,
            Err(error) => {
                return LayoutRestoreAttempt::interrupted(RestoreResult::with_capacity(0), error);
            }
        };
        let mut state = RestoreAccumulator::with_capacity(preflight.pane_count);

        info!(
            windows = snapshot.windows.len(),
            "starting layout restoration from snapshot (cx-first)"
        );

        let terminal = self
            .restore_validated_with_cx(cx, snapshot, &preflight, &mut state)
            .await;
        let terminal = match (terminal, state.validate_receipt_invariants()) {
            (Ok(()), Ok(())) => None,
            (Err(error), Ok(())) => Some(error),
            (_, Err(integrity_error)) => Some(integrity_error),
        };

        info!(
            windows = state.result.windows_created,
            tabs = state.result.tabs_created,
            panes = state.result.panes_created,
            failed = state.result.failed_panes.len(),
            interrupted = terminal.is_some(),
            suppressed_failure_logs = state.failure_logs_suppressed,
            "layout restoration attempt settled (cx-first)"
        );

        match terminal {
            Some(error) => LayoutRestoreAttempt::interrupted(state.result, error),
            None => LayoutRestoreAttempt::complete(state.result),
        }
    }

    async fn restore_validated_with_cx(
        &self,
        cx: &crate::cx::Cx,
        snapshot: &TopologySnapshot,
        preflight: &RestorePreflight,
        state: &mut RestoreAccumulator,
    ) -> crate::Result<()> {
        for (win_idx, window) in snapshot.windows.iter().enumerate() {
            restore_checkpoint(cx, "restore_layout.restore.between_windows")?;
            let restored_window = self
                .restore_window(cx, window, win_idx, preflight, state)
                .await?;
            if restored_window.flow.should_stop() {
                break;
            }
        }

        Ok(())
    }

    /// Restore a single window and all its tabs.
    async fn restore_window(
        &self,
        cx: &crate::cx::Cx,
        window: &WindowSnapshot,
        win_idx: usize,
        preflight: &RestorePreflight,
        state: &mut RestoreAccumulator,
    ) -> crate::Result<RestoredWindow> {
        debug!(
            window_id = window.window_id,
            tabs = window.tabs.len(),
            "restoring window"
        );

        let mut restored_window_id = None;
        let selected_tab_index = window.active_tab_index.unwrap_or(0);
        let mut selected_tab = None;
        let mut window_flow = RestoreFlow::Continue;

        for (tab_idx, tab) in window.tabs.iter().enumerate() {
            restore_checkpoint(cx, "restore_layout.restore_window.between_tabs")?;
            let target = SpawnTarget {
                window_id: restored_window_id,
                new_window: restored_window_id.is_none(),
            };
            let restored_tab = self
                .restore_tab(
                    cx,
                    tab,
                    win_idx,
                    tab_idx,
                    target,
                    tab_idx != selected_tab_index,
                    preflight,
                    state,
                )
                .await?;
            restored_window_id.get_or_insert(restored_tab.window_id);
            if tab_idx == selected_tab_index {
                selected_tab = Some(restored_tab);
            }
            if restored_tab.flow.should_stop() {
                window_flow = RestoreFlow::Stop;
                break;
            }
        }

        // Non-selected tabs chose their active panes during `restore_tab`.
        // Select the recorded tab exactly once, and only after later tab spawns
        // have finished changing the selected tab as a side effect.
        if let Some(selected_tab) = selected_tab {
            if window_flow.should_stop() {
                // A finite authoritative rejection already produced a useful
                // partial receipt. Cancellation, deadline, or budget expiry
                // observed immediately afterward must not overwrite it while
                // attempting best-effort focus cleanup.
                if cx.checkpoint().is_err() {
                    return Ok(RestoredWindow { flow: window_flow });
                }
            } else {
                restore_checkpoint(cx, "restore_layout.restore_window.before_activate")?;
            }
            match self
                .wezterm
                .activate_pane_with_cx(cx, selected_tab.active_new_pane_id)
                .await
            {
                Ok(()) => {}
                Err(error)
                    if is_authoritative_pane_rejection(&error, selected_tab.active_new_pane_id) =>
                {
                    state.record_failure(selected_tab.active_old_pane_id, FAILURE_ACTIVATION);
                    if state.claim_failure_log_slot() {
                        warn!(
                            pane_id = selected_tab.active_new_pane_id,
                            "failed to activate selected window tab"
                        );
                    }
                    if !self.config.continue_on_error {
                        return Ok(RestoredWindow {
                            flow: RestoreFlow::Stop,
                        });
                    }
                }
                Err(error) => return Err(error),
            }
        }

        Ok(RestoredWindow { flow: window_flow })
    }

    /// Restore a single tab with its pane tree.
    async fn restore_tab(
        &self,
        cx: &crate::cx::Cx,
        tab: &TabSnapshot,
        win_idx: usize,
        tab_idx: usize,
        spawn_target: SpawnTarget,
        activate_immediately: bool,
        preflight: &RestorePreflight,
        state: &mut RestoreAccumulator,
    ) -> crate::Result<RestoredTab> {
        debug!(
            tab_id = tab.tab_id,
            win_idx,
            tab_idx,
            ?spawn_target,
            "restoring tab"
        );
        restore_checkpoint(cx, "restore_layout.restore_tab.preflight")?;

        // Get initial CWD from the first leaf in the pane tree.
        let initial_cwd = first_leaf_cwd(&tab.pane_tree, &preflight.normalized_cwds);

        // Spawn the initial pane for this tab.
        let root_pane_id = self
            .wezterm
            .spawn_targeted_with_cx(cx, initial_cwd, None, spawn_target)
            .await?;
        // A successful targeted spawn is the authoritative receipt for both
        // the tab and, for the first tab, its window. Record those effects
        // before any follow-up lookup, pane-tree reconstruction, activation,
        // cancellation checkpoint, or backend failure can interrupt us.
        state.record_created_tab(spawn_target.new_window)?;
        record_created_first_leaf(state, &tab.pane_tree, root_pane_id)?;
        let window_id = if let Some(window_id) = spawn_target.window_id {
            window_id
        } else {
            self.wezterm
                .get_pane_with_cx(cx, root_pane_id)
                .await?
                .window_id
        };

        debug!(root_pane_id, tab_idx, "spawned root pane for tab");

        // Recursively restore the pane tree within this tab.
        let mut flow = self
            .restore_pane_tree(cx, &tab.pane_tree, root_pane_id, preflight, state)
            .await?;

        let preferred_old_pane_id = tab
            .active_pane_id
            .or_else(|| first_active_leaf_id(&tab.pane_tree))
            .or_else(|| first_leaf_id(&tab.pane_tree))
            .ok_or_else(|| {
                restore_layout_error(
                    "restore_layout.restore_tab.active",
                    "restored tab has no leaf pane",
                )
            })?;
        let (active_old_pane_id, active_new_pane_id) = state
            .result
            .pane_id_map
            .get(&preferred_old_pane_id)
            .copied()
            .map(|new_pane_id| (preferred_old_pane_id, new_pane_id))
            .or_else(|| first_mapped_leaf(&tab.pane_tree, &state.result.pane_id_map))
            .ok_or_else(|| {
                restore_layout_error(
                    "restore_layout.restore_tab.active",
                    "restored tab has no mapped pane",
                )
            })?;

        if activate_immediately && !flow.should_stop() {
            match self
                .wezterm
                .activate_pane_with_cx(cx, active_new_pane_id)
                .await
            {
                Ok(()) => {}
                Err(error) if is_authoritative_pane_rejection(&error, active_new_pane_id) => {
                    state.record_failure(active_old_pane_id, FAILURE_ACTIVATION);
                    if state.claim_failure_log_slot() {
                        warn!(
                            pane_id = active_new_pane_id,
                            "failed to activate restored tab pane"
                        );
                    }
                    if !self.config.continue_on_error {
                        flow = RestoreFlow::Stop;
                    }
                }
                Err(error) => return Err(error),
            }
        }

        Ok(RestoredTab {
            window_id,
            active_old_pane_id,
            active_new_pane_id,
            flow,
        })
    }

    /// Recursively restore a pane tree.
    ///
    /// Uses explicit `Pin<Box<..>>` return type because async recursion
    /// requires boxing the future.
    fn restore_pane_tree<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        node: &'a PaneNode,
        current_pane_id: u64,
        preflight: &'a RestorePreflight,
        state: &'a mut RestoreAccumulator,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<RestoreFlow>> + Send + 'a>>
    {
        Box::pin(async move {
            match node {
                PaneNode::Leaf { pane_id, .. } => {
                    state.record_created_pane(*pane_id, current_pane_id)?;
                    Ok(RestoreFlow::Continue)
                }

                PaneNode::HSplit { children } => {
                    restore_checkpoint(cx, "restore_layout.restore_pane_tree.split")?;
                    self.restore_split_children(
                        cx,
                        children,
                        current_pane_id,
                        SplitDirection::Bottom,
                        preflight,
                        state,
                    )
                    .await
                }

                PaneNode::VSplit { children } => {
                    restore_checkpoint(cx, "restore_layout.restore_pane_tree.split")?;
                    self.restore_split_children(
                        cx,
                        children,
                        current_pane_id,
                        SplitDirection::Right,
                        preflight,
                        state,
                    )
                    .await
                }
            }
        })
    }

    /// Restore children of a split node.
    ///
    /// The first child inherits `current_pane_id`. Each subsequent child
    /// is created by splitting from `current_pane_id` in the given direction.
    fn restore_split_children<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        children: &'a [(f64, PaneNode)],
        current_pane_id: u64,
        direction: SplitDirection,
        preflight: &'a RestorePreflight,
        state: &'a mut RestoreAccumulator,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<RestoreFlow>> + Send + 'a>>
    {
        Box::pin(async move {
            // Peel right/bottom children from the outside in. `SplitSize` is
            // the size of the newly-created second child, so forward splitting
            // of the first pane reverses siblings. Reverse-prefix construction
            // preserves both the requested ratios and the recorded order. The
            // ratio-restoring percentages were computed with scale-normalized
            // prefix sums during full preflight, avoiding catastrophic suffix
            // subtraction for ratios such as [1, 2, f64::MAX].
            let mut remaining_children = children.len();
            for (_, child) in children[1..].iter().rev() {
                restore_checkpoint(cx, "restore_layout.restore_split_children.before_split")?;
                let percent = if self.config.restore_split_ratios {
                    let child_first_leaf_id = first_leaf_id(child).ok_or_else(|| {
                        restore_layout_integrity_error(
                            "restore_layout.split_plan.missing_child_leaf",
                        )
                    })?;
                    Some(
                        *preflight
                            .split_percents
                            .get(&child_first_leaf_id)
                            .ok_or_else(|| {
                                restore_layout_integrity_error(
                                    "restore_layout.split_plan.missing_percent",
                                )
                            })?,
                    )
                } else {
                    Some(equal_split_percent(remaining_children).map_err(|_| {
                        restore_layout_integrity_error(
                            "restore_layout.split_plan.invalid_equal_prefix",
                        )
                    })?)
                };

                let cwd = first_leaf_cwd(child, &preflight.normalized_cwds);

                match self
                    .wezterm
                    .split_pane_with_cx(cx, current_pane_id, direction, cwd, percent)
                    .await
                {
                    Ok(new_pane_id) => {
                        record_created_first_leaf(state, child, new_pane_id)?;
                        debug!(
                            parent = current_pane_id,
                            new_pane = new_pane_id,
                            ?direction,
                            percent,
                            "split pane created"
                        );
                        let flow = self
                            .restore_pane_tree(cx, child, new_pane_id, preflight, state)
                            .await?;
                        if flow.should_stop() {
                            return Ok(flow);
                        }
                    }
                    Err(error) if is_authoritative_pane_rejection(&error, current_pane_id) => {
                        let affected = record_failed_tree(state, child, FAILURE_SPLIT);
                        if state.claim_failure_log_slot() {
                            warn!(
                                parent = current_pane_id,
                                affected_panes = affected,
                                "failed to create split pane"
                            );
                        }
                        if !self.config.continue_on_error {
                            return Ok(RestoreFlow::Stop);
                        }
                    }
                    Err(error) => return Err(error),
                }
                remaining_children -= 1;
            }

            let (_, first_child) = &children[0];
            self.restore_pane_tree(cx, first_child, current_pane_id, preflight, state)
                .await
        })
    }
}

// =============================================================================
// Helpers
// =============================================================================

fn validate_restore_snapshot(
    snapshot: &TopologySnapshot,
    restore_working_dirs: bool,
    restore_split_ratios: bool,
) -> crate::Result<RestorePreflight> {
    const OPERATION: &str = "restore_layout.preflight";

    if snapshot.schema_version != TOPOLOGY_SCHEMA_VERSION {
        return Err(restore_layout_error(
            OPERATION,
            "unsupported topology schema version",
        ));
    }
    if snapshot.windows.is_empty() {
        return Err(restore_layout_error(
            OPERATION,
            "topology contains no windows",
        ));
    }
    snapshot.validate_resource_limits().map_err(|_error| {
        restore_layout_error(OPERATION, "topology exceeds supported resource limits")
    })?;

    let mut seen_window_ids = HashSet::with_capacity(snapshot.windows.len());
    let mut seen_tab_ids = HashSet::new();
    let mut seen_pane_ids = HashSet::new();
    let mut normalized_cwds = HashMap::new();
    let mut split_percents = HashMap::new();
    let mut pane_count = 0usize;

    for window in &snapshot.windows {
        if !seen_window_ids.insert(window.window_id) {
            return Err(restore_layout_error(
                OPERATION,
                "topology contains a duplicate window id",
            ));
        }
        if window.tabs.is_empty() {
            return Err(restore_layout_error(
                OPERATION,
                "topology contains an empty window",
            ));
        }
        if window
            .active_tab_index
            .is_some_and(|index| index >= window.tabs.len())
        {
            return Err(restore_layout_error(
                OPERATION,
                "topology contains an invalid active tab index",
            ));
        }

        for tab in &window.tabs {
            if !seen_tab_ids.insert(tab.tab_id) {
                return Err(restore_layout_error(
                    OPERATION,
                    "topology contains a duplicate tab id",
                ));
            }
            let validated = validate_pane_tree(
                &tab.pane_tree,
                1,
                restore_working_dirs,
                restore_split_ratios,
                &mut seen_pane_ids,
                &mut normalized_cwds,
                &mut split_percents,
            )?;
            if tab
                .active_pane_id
                .is_some_and(|pane_id| !pane_tree_contains(&tab.pane_tree, pane_id))
            {
                return Err(restore_layout_error(
                    OPERATION,
                    "tab active pane is outside its pane tree",
                ));
            }
            if matches!(
                (tab.active_pane_id, validated.active_leaf_id),
                (Some(explicit), Some(marked)) if explicit != marked
            ) {
                return Err(restore_layout_error(
                    OPERATION,
                    "tab active pane contradicts its active leaf marker",
                ));
            }
            pane_count = pane_count
                .checked_add(validated.pane_count)
                .ok_or_else(|| restore_layout_error(OPERATION, "topology pane count overflowed"))?;
        }
    }

    if pane_count == 0 {
        return Err(restore_layout_error(
            OPERATION,
            "topology contains no pane leaves",
        ));
    }

    Ok(RestorePreflight {
        pane_count,
        normalized_cwds,
        split_percents,
    })
}

/// Run the exact layout-restorer preflight used by [`LayoutRestorer`] without
/// retaining its derived execution plan. Session restore calls this before it
/// persists a durable restore intent, so malformed topology can never create a
/// reconciliation-required lifecycle merely because the execution layer has a
/// stronger validator than the admission layer.
pub(crate) fn validate_restore_snapshot_for_admission(
    snapshot: &TopologySnapshot,
    restore_working_dirs: bool,
    restore_split_ratios: bool,
) -> crate::Result<()> {
    validate_restore_snapshot(snapshot, restore_working_dirs, restore_split_ratios).map(drop)
}

fn validate_pane_tree(
    node: &PaneNode,
    depth: usize,
    restore_working_dirs: bool,
    restore_split_ratios: bool,
    seen_pane_ids: &mut HashSet<u64>,
    normalized_cwds: &mut HashMap<u64, String>,
    split_percents: &mut HashMap<u64, u8>,
) -> crate::Result<ValidatedPaneTree> {
    const OPERATION: &str = "restore_layout.preflight";

    if depth > MAX_PANE_TREE_DEPTH {
        return Err(restore_layout_error(
            OPERATION,
            "topology pane tree exceeds the supported depth",
        ));
    }

    match node {
        PaneNode::Leaf {
            pane_id,
            rows,
            cols,
            cwd,
            is_active,
            ..
        } => {
            if *rows == 0 || *cols == 0 {
                return Err(restore_layout_error(
                    OPERATION,
                    "pane has zero terminal dimensions",
                ));
            }
            if !seen_pane_ids.insert(*pane_id) {
                return Err(restore_layout_error(
                    OPERATION,
                    "topology contains a duplicate pane id",
                ));
            }
            if restore_working_dirs && let Some(cwd) = cwd {
                let normalized = normalize_restored_cwd(cwd)
                    .map_err(|detail| restore_layout_error(OPERATION, detail))?;
                normalized_cwds.insert(*pane_id, normalized);
            }
            Ok(ValidatedPaneTree {
                pane_count: 1,
                active_leaf_id: is_active.then_some(*pane_id),
                first_leaf_id: *pane_id,
            })
        }
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
            if children.len() < 2 {
                return Err(restore_layout_error(
                    OPERATION,
                    "split node has fewer than two children",
                ));
            }
            let mut pane_count = 0usize;
            let mut active_leaf_id = None;
            let mut first_child_leaf_id = None;
            let mut prefix_scale = 0.0f64;
            let mut scaled_prefix_sum = 0.0f64;
            for (child_index, (ratio, child)) in children.iter().enumerate() {
                if !ratio.is_finite() || *ratio <= 0.0 {
                    return Err(restore_layout_error(
                        OPERATION,
                        "split node has a non-positive or non-finite ratio",
                    ));
                }
                if child_index == 0 {
                    prefix_scale = *ratio;
                    scaled_prefix_sum = 1.0;
                } else if *ratio > prefix_scale {
                    scaled_prefix_sum = scaled_prefix_sum.mul_add(prefix_scale / *ratio, 1.0);
                    prefix_scale = *ratio;
                } else {
                    scaled_prefix_sum += *ratio / prefix_scale;
                }
                if !prefix_scale.is_finite()
                    || prefix_scale <= 0.0
                    || !scaled_prefix_sum.is_finite()
                    || scaled_prefix_sum <= 0.0
                {
                    return Err(restore_layout_error(
                        OPERATION,
                        "split ratio normalization failed",
                    ));
                }
                let validated_child = validate_pane_tree(
                    child,
                    depth + 1,
                    restore_working_dirs,
                    restore_split_ratios,
                    seen_pane_ids,
                    normalized_cwds,
                    split_percents,
                )?;
                if child_index == 0 {
                    first_child_leaf_id = Some(validated_child.first_leaf_id);
                } else if restore_split_ratios {
                    let child_scaled_ratio = *ratio / prefix_scale;
                    let percent = split_percent(child_scaled_ratio, scaled_prefix_sum)
                        .map_err(|detail| restore_layout_error(OPERATION, detail))?;
                    if split_percents
                        .insert(validated_child.first_leaf_id, percent)
                        .is_some()
                    {
                        return Err(restore_layout_integrity_error(
                            "restore_layout.split_plan.duplicate_child_key",
                        ));
                    }
                }
                pane_count = pane_count
                    .checked_add(validated_child.pane_count)
                    .ok_or_else(|| {
                        restore_layout_error(OPERATION, "topology pane count overflowed")
                    })?;
                if let Some(child_active_leaf_id) = validated_child.active_leaf_id {
                    if active_leaf_id.replace(child_active_leaf_id).is_some() {
                        return Err(restore_layout_error(
                            OPERATION,
                            "tab contains multiple active leaf markers",
                        ));
                    }
                }
            }
            Ok(ValidatedPaneTree {
                pane_count,
                active_leaf_id,
                first_leaf_id: first_child_leaf_id.ok_or_else(|| {
                    restore_layout_integrity_error(
                        "restore_layout.preflight.missing_split_first_leaf",
                    )
                })?,
            })
        }
    }
}

fn normalize_restored_cwd(cwd: &str) -> Result<String, &'static str> {
    let raw_limit = if cwd.starts_with("file://") {
        MAX_ENCODED_RESTORED_CWD_BYTES
    } else {
        MAX_RESTORED_CWD_BYTES
    };
    if cwd.len() > raw_limit {
        return Err("working directory exceeds the restore byte limit");
    }

    let (path, is_file_uri) = if let Some(rest) = cwd.strip_prefix("file://") {
        if rest.starts_with('/') {
            (rest, true)
        } else {
            let slash = rest
                .find('/')
                .ok_or("file URI working directory has no absolute path")?;
            let authority = &rest[..slash];
            if !authority.eq_ignore_ascii_case("localhost") {
                return Err("non-local file URI working directory requires a mux domain");
            }
            (&rest[slash..], true)
        }
    } else {
        (cwd, false)
    };

    if is_file_uri && path.bytes().any(|byte| matches!(byte, b'?' | b'#')) {
        return Err("file URI working directory contains a query or fragment");
    }

    let decoded = if is_file_uri {
        strict_percent_decode(path)?
    } else {
        path.to_string()
    };
    if decoded.len() > MAX_RESTORED_CWD_BYTES {
        return Err("decoded working directory exceeds the restore byte limit");
    }
    if decoded.is_empty() || !Path::new(&decoded).is_absolute() {
        return Err("working directory is not an absolute local path");
    }
    if decoded
        .chars()
        .any(|ch| ch.is_control() || matches!(ch as u32, 0x7f..=0x9f))
    {
        return Err("working directory contains a terminal control character");
    }
    Ok(decoded)
}

fn strict_percent_decode(value: &str) -> Result<String, &'static str> {
    fn hex_value(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err("working directory contains truncated percent encoding");
            }
            let high = hex_value(bytes[index + 1])
                .ok_or("working directory contains invalid percent encoding")?;
            let low = hex_value(bytes[index + 2])
                .ok_or("working directory contains invalid percent encoding")?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| "working directory is not valid UTF-8")
}

fn first_leaf_id(node: &PaneNode) -> Option<u64> {
    match node {
        PaneNode::Leaf { pane_id, .. } => Some(*pane_id),
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
            children.first().and_then(|(_, child)| first_leaf_id(child))
        }
    }
}

fn record_created_first_leaf(
    state: &mut RestoreAccumulator,
    node: &PaneNode,
    new_pane_id: u64,
) -> crate::Result<()> {
    let old_pane_id = first_leaf_id(node).ok_or_else(|| {
        restore_layout_integrity_error("restore_layout.pane_mapping.missing_first_leaf")
    })?;
    state.record_created_pane(old_pane_id, new_pane_id)
}

fn is_authoritative_pane_rejection(error: &crate::Error, pane_id: u64) -> bool {
    matches!(
        error,
        crate::Error::Wezterm(crate::error::WeztermError::PaneNotFound(rejected_pane_id))
            if *rejected_pane_id == pane_id
    ) || matches!(
        error,
        crate::Error::PaneOperation {
            pane_id: rejected_pane_id,
            source: crate::error::PaneOperationSource::PaneNotFound,
            ..
        } if *rejected_pane_id == pane_id
    )
}

fn first_active_leaf_id(node: &PaneNode) -> Option<u64> {
    match node {
        PaneNode::Leaf {
            pane_id, is_active, ..
        } => is_active.then_some(*pane_id),
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => children
            .iter()
            .find_map(|(_, child)| first_active_leaf_id(child)),
    }
}

fn first_leaf_cwd<'a>(
    node: &PaneNode,
    normalized_cwds: &'a HashMap<u64, String>,
) -> Option<&'a str> {
    first_leaf_id(node).and_then(|pane_id| normalized_cwds.get(&pane_id).map(String::as_str))
}

fn first_mapped_leaf(node: &PaneNode, pane_id_map: &HashMap<u64, u64>) -> Option<(u64, u64)> {
    match node {
        PaneNode::Leaf { pane_id, .. } => pane_id_map
            .get(pane_id)
            .copied()
            .map(|new_pane_id| (*pane_id, new_pane_id)),
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => children
            .iter()
            .find_map(|(_, child)| first_mapped_leaf(child, pane_id_map)),
    }
}

fn pane_tree_contains(node: &PaneNode, pane_id: u64) -> bool {
    match node {
        PaneNode::Leaf {
            pane_id: candidate, ..
        } => *candidate == pane_id,
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => children
            .iter()
            .any(|(_, child)| pane_tree_contains(child, pane_id)),
    }
}

fn split_percent(child_scaled_ratio: f64, scaled_prefix_sum: f64) -> Result<u8, &'static str> {
    if !child_scaled_ratio.is_finite()
        || !scaled_prefix_sum.is_finite()
        || child_scaled_ratio < 0.0
        || scaled_prefix_sum <= 0.0
        || child_scaled_ratio > scaled_prefix_sum
    {
        return Err("split ratio percentage invariant failed");
    }
    if child_scaled_ratio <= 0.0 {
        // A validated positive ratio can underflow only after normalization by
        // an astronomically larger sibling. The backend's minimum 1% is the
        // closest representable request and is not an invariant failure.
        return Ok(1);
    }
    let percent = (child_scaled_ratio / scaled_prefix_sum * 100.0).round();
    if !percent.is_finite() {
        return Err("split ratio percentage calculation failed");
    }
    Ok((percent as u8).clamp(1, 99))
}

fn equal_split_percent(remaining_children: usize) -> Result<u8, &'static str> {
    if remaining_children < 2 {
        return Err("equal split requires at least two remaining children");
    }
    let rounded = (100usize.saturating_add(remaining_children / 2)) / remaining_children;
    Ok(u8::try_from(rounded).unwrap_or(99).clamp(1, 99))
}

fn record_failed_tree(
    state: &mut RestoreAccumulator,
    node: &PaneNode,
    reason: &'static str,
) -> usize {
    match node {
        PaneNode::Leaf { pane_id, .. } => {
            usize::from(state.record_unmapped_failure(*pane_id, reason))
        }
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => children
            .iter()
            .map(|(_, child)| record_failed_tree(state, child, reason))
            .sum(),
    }
}

/// Collect all leaf pane IDs from a pane tree.
#[cfg(test)]
fn collect_leaf_ids(node: &PaneNode) -> Vec<u64> {
    let mut ids = Vec::new();
    collect_leaf_ids_inner(node, &mut ids);
    ids
}

#[cfg(test)]
fn collect_leaf_ids_inner(node: &PaneNode, ids: &mut Vec<u64>) {
    match node {
        PaneNode::Leaf { pane_id, .. } => ids.push(*pane_id),
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
            for (_, child) in children {
                collect_leaf_ids_inner(child, ids);
            }
        }
    }
}

/// Minimal shell escaping for paths (wraps in single quotes).
#[cfg(test)]
fn shell_escape(s: &str) -> String {
    if s.contains('\'') {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        format!("'{s}'")
    }
}

/// Count total leaf panes in a snapshot.
pub fn count_panes(snapshot: &TopologySnapshot) -> usize {
    snapshot
        .windows
        .iter()
        .flat_map(|w| &w.tabs)
        .map(|t| count_leaves(&t.pane_tree))
        .sum()
}

fn count_leaves(node: &PaneNode) -> usize {
    match node {
        PaneNode::Leaf { .. } => 1,
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
            children.iter().map(|(_, c)| count_leaves(c)).sum()
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::session_topology::{PaneNode, TabSnapshot, TopologySnapshot, WindowSnapshot};
    use crate::wezterm::{
        MockWezterm, SpawnTarget, WeztermFuture, WeztermHandle, WeztermInterface,
    };

    fn test_runtime_error(operation: &'static str, detail: impl Into<String>) -> crate::Error {
        crate::Error::RuntimeOperation {
            operation,
            source: crate::error::RuntimeOperationSource::Backend(detail.into()),
        }
    }

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build restore_layout test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn make_restorer(mock: Arc<MockWezterm>) -> LayoutRestorer {
        LayoutRestorer::new(mock, RestoreConfig::default())
    }

    struct AlwaysFailSpawnWezterm;

    #[derive(Debug, Clone, Copy)]
    enum InjectedMutationFailure {
        AuthoritativePaneRejection,
        AuthoritativePaneRejectionThenCancel,
        Indeterminate,
        DeadlineExceeded,
        ContextFailure,
        LockPoisoned,
    }

    #[derive(Debug, Clone, Copy)]
    enum InjectedSplitReceipt {
        ParentPaneId,
        Fixed(u64),
    }

    impl InjectedMutationFailure {
        fn error(self, operation: &'static str, pane_id: u64) -> crate::Error {
            match self {
                Self::AuthoritativePaneRejection | Self::AuthoritativePaneRejectionThenCancel => {
                    crate::error::WeztermError::PaneNotFound(pane_id).into()
                }
                Self::Indeterminate => {
                    crate::error::WeztermError::IndeterminateMutation { operation }.into()
                }
                Self::DeadlineExceeded => crate::Error::RuntimeOperation {
                    operation,
                    source: RuntimeOperationSource::DeadlineExceeded,
                },
                Self::ContextFailure => crate::Error::RuntimeOperation {
                    operation,
                    source: RuntimeOperationSource::ContextFailure,
                },
                Self::LockPoisoned => crate::Error::RuntimeOperation {
                    operation,
                    source: RuntimeOperationSource::LockPoisoned,
                },
            }
        }
    }

    struct InstrumentedWezterm {
        inner: Arc<MockWezterm>,
        split_failure: Option<InjectedMutationFailure>,
        split_receipt: Option<InjectedSplitReceipt>,
        activation_failure: Option<InjectedMutationFailure>,
        split_attempts: Mutex<Vec<(u64, SplitDirection, Option<u8>)>>,
        activation_attempts: Mutex<Vec<u64>>,
        get_pane_attempts: Mutex<Vec<u64>>,
    }

    impl InstrumentedWezterm {
        fn new(inner: Arc<MockWezterm>, fail_splits: bool, fail_activations: bool) -> Self {
            Self {
                inner,
                split_failure: fail_splits
                    .then_some(InjectedMutationFailure::AuthoritativePaneRejection),
                split_receipt: None,
                activation_failure: fail_activations
                    .then_some(InjectedMutationFailure::AuthoritativePaneRejection),
                split_attempts: Mutex::new(Vec::new()),
                activation_attempts: Mutex::new(Vec::new()),
                get_pane_attempts: Mutex::new(Vec::new()),
            }
        }

        fn with_split_failure(
            inner: Arc<MockWezterm>,
            split_failure: InjectedMutationFailure,
        ) -> Self {
            Self {
                inner,
                split_failure: Some(split_failure),
                split_receipt: None,
                activation_failure: None,
                split_attempts: Mutex::new(Vec::new()),
                activation_attempts: Mutex::new(Vec::new()),
                get_pane_attempts: Mutex::new(Vec::new()),
            }
        }

        fn with_split_receipt(
            inner: Arc<MockWezterm>,
            split_receipt: InjectedSplitReceipt,
        ) -> Self {
            Self {
                inner,
                split_failure: None,
                split_receipt: Some(split_receipt),
                activation_failure: None,
                split_attempts: Mutex::new(Vec::new()),
                activation_attempts: Mutex::new(Vec::new()),
                get_pane_attempts: Mutex::new(Vec::new()),
            }
        }
    }

    impl WeztermInterface for AlwaysFailSpawnWezterm {
        fn list_panes(&self) -> WeztermFuture<'_, Vec<crate::wezterm::PaneInfo>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, crate::wezterm::PaneInfo> {
            Box::pin(async move {
                Err(test_runtime_error(
                    "restore_layout.test.unexpected_get_pane",
                    format!("unexpected get_pane({pane_id}) on failing spawn mock"),
                ))
            })
        }

        fn get_text(&self, pane_id: u64, _: bool) -> WeztermFuture<'_, String> {
            Box::pin(async move {
                Err(test_runtime_error(
                    "restore_layout.test.unexpected_get_text",
                    format!("unexpected get_text({pane_id}) on failing spawn mock"),
                ))
            })
        }

        fn send_text(&self, pane_id: u64, _: &str) -> WeztermFuture<'_, ()> {
            Box::pin(async move {
                Err(test_runtime_error(
                    "restore_layout.test.unexpected_send_text",
                    format!("unexpected send_text({pane_id}) on failing spawn mock"),
                ))
            })
        }

        fn send_text_no_paste(&self, pane_id: u64, _: &str) -> WeztermFuture<'_, ()> {
            self.send_text(pane_id, "")
        }

        fn send_text_with_options(
            &self,
            pane_id: u64,
            _: &str,
            _: bool,
            _: bool,
        ) -> WeztermFuture<'_, ()> {
            self.send_text(pane_id, "")
        }

        fn send_control(&self, pane_id: u64, _: &str) -> WeztermFuture<'_, ()> {
            self.send_text(pane_id, "")
        }

        fn send_ctrl_c(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.send_text(pane_id, "")
        }

        fn send_ctrl_d(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.send_text(pane_id, "")
        }

        fn spawn(&self, _: Option<&str>, _: Option<&str>) -> WeztermFuture<'_, u64> {
            Box::pin(async {
                Err(test_runtime_error(
                    "restore_layout.test.spawn",
                    "simulated spawn failure",
                ))
            })
        }

        fn spawn_targeted(
            &self,
            _: Option<&str>,
            _: Option<&str>,
            _: SpawnTarget,
        ) -> WeztermFuture<'_, u64> {
            Box::pin(async {
                Err(test_runtime_error(
                    "restore_layout.test.spawn_targeted",
                    "simulated spawn failure",
                ))
            })
        }

        fn split_pane(
            &self,
            pane_id: u64,
            _: crate::wezterm::SplitDirection,
            _: Option<&str>,
            _: Option<u8>,
        ) -> WeztermFuture<'_, u64> {
            Box::pin(async move {
                Err(test_runtime_error(
                    "restore_layout.test.unexpected_split_pane",
                    format!("unexpected split_pane({pane_id}) on failing spawn mock"),
                ))
            })
        }

        fn activate_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            Box::pin(async move {
                Err(test_runtime_error(
                    "restore_layout.test.unexpected_activate_pane",
                    format!("unexpected activate_pane({pane_id}) on failing spawn mock"),
                ))
            })
        }

        fn get_pane_direction(
            &self,
            pane_id: u64,
            _: crate::wezterm::MoveDirection,
        ) -> WeztermFuture<'_, Option<u64>> {
            Box::pin(async move {
                Err(test_runtime_error(
                    "restore_layout.test.unexpected_get_pane_direction",
                    format!("unexpected get_pane_direction({pane_id}) on failing spawn mock"),
                ))
            })
        }

        fn kill_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            Box::pin(async move {
                Err(test_runtime_error(
                    "restore_layout.test.unexpected_kill_pane",
                    format!("unexpected kill_pane({pane_id}) on failing spawn mock"),
                ))
            })
        }

        fn zoom_pane(&self, pane_id: u64, _: bool) -> WeztermFuture<'_, ()> {
            Box::pin(async move {
                Err(test_runtime_error(
                    "restore_layout.test.unexpected_zoom_pane",
                    format!("unexpected zoom_pane({pane_id}) on failing spawn mock"),
                ))
            })
        }

        fn circuit_status(&self) -> crate::circuit_breaker::CircuitBreakerStatus {
            crate::circuit_breaker::CircuitBreakerStatus::default()
        }
    }

    impl WeztermInterface for InstrumentedWezterm {
        fn list_panes(&self) -> WeztermFuture<'_, Vec<crate::wezterm::PaneInfo>> {
            self.inner.list_panes()
        }

        fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, crate::wezterm::PaneInfo> {
            self.get_pane_attempts
                .lock()
                .expect("get-pane attempt lock poisoned")
                .push(pane_id);
            self.inner.get_pane(pane_id)
        }

        fn get_text(&self, pane_id: u64, escapes: bool) -> WeztermFuture<'_, String> {
            self.inner.get_text(pane_id, escapes)
        }

        fn send_text(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
            self.inner.send_text(pane_id, text)
        }

        fn send_text_no_paste(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
            self.inner.send_text_no_paste(pane_id, text)
        }

        fn send_text_with_options(
            &self,
            pane_id: u64,
            text: &str,
            bracketed_paste: bool,
            normalize_newlines: bool,
        ) -> WeztermFuture<'_, ()> {
            self.inner
                .send_text_with_options(pane_id, text, bracketed_paste, normalize_newlines)
        }

        fn send_control(&self, pane_id: u64, control_char: &str) -> WeztermFuture<'_, ()> {
            self.inner.send_control(pane_id, control_char)
        }

        fn send_ctrl_c(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.inner.send_ctrl_c(pane_id)
        }

        fn send_ctrl_d(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.inner.send_ctrl_d(pane_id)
        }

        fn spawn(&self, cwd: Option<&str>, domain_name: Option<&str>) -> WeztermFuture<'_, u64> {
            self.inner.spawn(cwd, domain_name)
        }

        fn spawn_targeted(
            &self,
            cwd: Option<&str>,
            domain_name: Option<&str>,
            target: SpawnTarget,
        ) -> WeztermFuture<'_, u64> {
            self.inner.spawn_targeted(cwd, domain_name, target)
        }

        fn split_pane(
            &self,
            pane_id: u64,
            direction: crate::wezterm::SplitDirection,
            cwd: Option<&str>,
            percent: Option<u8>,
        ) -> WeztermFuture<'_, u64> {
            self.split_attempts
                .lock()
                .expect("split-attempt lock poisoned")
                .push((pane_id, direction, percent));
            if let Some(receipt) = self.split_receipt {
                let returned_pane_id = match receipt {
                    InjectedSplitReceipt::ParentPaneId => pane_id,
                    InjectedSplitReceipt::Fixed(returned_pane_id) => returned_pane_id,
                };
                Box::pin(async move { Ok(returned_pane_id) })
            } else if let Some(failure) = self.split_failure {
                let error = failure.error("restore_layout.test.split_pane", pane_id);
                Box::pin(async move { Err(error) })
            } else {
                self.inner.split_pane(pane_id, direction, cwd, percent)
            }
        }

        fn split_pane_with_cx<'a>(
            &'a self,
            cx: &'a crate::cx::Cx,
            pane_id: u64,
            direction: crate::wezterm::SplitDirection,
            cwd: Option<&'a str>,
            percent: Option<u8>,
        ) -> WeztermFuture<'a, u64> {
            self.split_attempts
                .lock()
                .expect("split-attempt lock poisoned")
                .push((pane_id, direction, percent));
            if let Some(receipt) = self.split_receipt {
                let returned_pane_id = match receipt {
                    InjectedSplitReceipt::ParentPaneId => pane_id,
                    InjectedSplitReceipt::Fixed(returned_pane_id) => returned_pane_id,
                };
                Box::pin(async move { Ok(returned_pane_id) })
            } else if let Some(failure) = self.split_failure {
                if matches!(
                    failure,
                    InjectedMutationFailure::AuthoritativePaneRejectionThenCancel
                ) {
                    cx.cancel_with(
                        crate::outcome::CancelKind::User,
                        Some("SECRET cancellation after authoritative split rejection"),
                    );
                }
                let error = failure.error("restore_layout.test.split_pane_with_cx", pane_id);
                Box::pin(async move { Err(error) })
            } else {
                self.inner
                    .split_pane_with_cx(cx, pane_id, direction, cwd, percent)
            }
        }

        fn activate_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.activation_attempts
                .lock()
                .expect("activation-attempt lock poisoned")
                .push(pane_id);
            if let Some(failure) = self.activation_failure {
                let error = failure.error("restore_layout.test.activate_pane", pane_id);
                Box::pin(async move { Err(error) })
            } else {
                self.inner.activate_pane(pane_id)
            }
        }

        fn get_pane_direction(
            &self,
            pane_id: u64,
            direction: crate::wezterm::MoveDirection,
        ) -> WeztermFuture<'_, Option<u64>> {
            self.inner.get_pane_direction(pane_id, direction)
        }

        fn kill_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.inner.kill_pane(pane_id)
        }

        fn zoom_pane(&self, pane_id: u64, zoomed: bool) -> WeztermFuture<'_, ()> {
            self.inner.zoom_pane(pane_id, zoomed)
        }

        fn circuit_status(&self) -> crate::circuit_breaker::CircuitBreakerStatus {
            self.inner.circuit_status()
        }
    }

    fn leaf(pane_id: u64, cwd: Option<&str>) -> PaneNode {
        PaneNode::Leaf {
            pane_id,
            rows: 24,
            cols: 80,
            cwd: cwd.map(String::from),
            title: None,
            is_active: false,
        }
    }

    fn active_leaf(pane_id: u64) -> PaneNode {
        PaneNode::Leaf {
            pane_id,
            rows: 24,
            cols: 80,
            cwd: None,
            title: None,
            is_active: true,
        }
    }

    fn hsplit(children: Vec<(f64, PaneNode)>) -> PaneNode {
        PaneNode::HSplit { children }
    }

    fn vsplit(children: Vec<(f64, PaneNode)>) -> PaneNode {
        PaneNode::VSplit { children }
    }

    fn single_tab_snapshot(pane_tree: PaneNode) -> TopologySnapshot {
        TopologySnapshot {
            schema_version: 1,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 0,
                title: None,
                position: None,
                size: None,
                tabs: vec![TabSnapshot {
                    tab_id: 0,
                    title: None,
                    pane_tree,
                    active_pane_id: None,
                }],
                active_tab_index: None,
            }],
        }
    }

    #[test]
    fn restore_checkpoint_uses_content_free_structured_cancellation() {
        let cx = crate::cx::for_testing();
        cx.cancel_with(
            crate::outcome::CancelKind::User,
            Some("SECRET restore-layout cancellation detail"),
        );
        let err = restore_checkpoint(&cx, "restore_layout.test_checkpoint")
            .expect_err("pre-cancelled checkpoint must fail");

        match err {
            crate::Error::RuntimeOperation { operation, source } => {
                assert_eq!(operation, "restore_layout.test_checkpoint");
                assert_eq!(
                    source,
                    crate::error::RuntimeOperationSource::Cancelled(
                        "caller capability stopped".to_string()
                    )
                );
            }
            other => panic!("expected structured runtime operation, got {other:?}"),
        }
    }

    #[test]
    fn restore_checkpoint_preserves_deadline_poll_and_cost_budget_classes() {
        for (budget, expected) in [
            (
                crate::cx::Budget::new().with_deadline(Default::default()),
                RuntimeOperationSource::DeadlineExceeded,
            ),
            (
                crate::cx::Budget::new().with_poll_quota(0),
                RuntimeOperationSource::PollQuotaExhausted,
            ),
            (
                crate::cx::Budget::new().with_cost_quota(0),
                RuntimeOperationSource::CostBudgetExhausted,
            ),
        ] {
            let cx = crate::cx::Cx::for_testing_with_budget(budget);
            let err = restore_checkpoint(&cx, "restore_layout.test_budget")
                .expect_err("exhausted budget must fail");
            assert!(matches!(
                err,
                crate::Error::RuntimeOperation {
                    operation: "restore_layout.test_budget",
                    source,
                } if source == expected
            ));
        }
    }

    #[test]
    fn restore_context_error_preserves_cancellation_cleanup_timeout() {
        let cx = crate::cx::for_testing();
        let error = crate::runtime_async::ContextError::new(
            crate::runtime_async::ContextErrorKind::CancelTimeout,
        )
        .with_message("raw-cleanup-detail-canary");
        let classified = restore_context_error(&cx, "restore_layout.test_cleanup_timeout", &error);
        assert!(matches!(
            classified,
            crate::Error::RuntimeOperation {
                operation: "restore_layout.test_cleanup_timeout",
                source: RuntimeOperationSource::CancellationCleanupTimedOut,
            }
        ));
        assert!(!format!("{classified:?}").contains("raw-cleanup-detail-canary"));
    }

    #[test]
    fn restore_single_pane() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let snapshot = single_tab_snapshot(leaf(42, Some("/home/user")));

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 1);
            assert_eq!(result.windows_created, 1);
            assert_eq!(result.tabs_created, 1);
            assert_eq!(result.failed_panes, [] as [(u64, std::string::String); 0]);
            assert!(result.pane_id_map.contains_key(&42));
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `restore_with_cx` must produce
    /// the same RestoreResult shape (pane/window/tab counts,
    /// pane_id_map coverage, no failures) as `restore` for an
    /// uncancelled cx.
    #[test]
    fn restore_with_cx_matches_legacy() {
        run_async_test(async {
            let tree = hsplit(vec![(0.5, leaf(101, None)), (0.5, leaf(102, None))]);
            let snapshot = single_tab_snapshot(tree);

            let legacy_mock = Arc::new(MockWezterm::new());
            let legacy_restorer = make_restorer(legacy_mock.clone());
            let legacy = legacy_restorer.restore(&snapshot).await.unwrap();

            let cx_mock = Arc::new(MockWezterm::new());
            let cx_restorer = make_restorer(cx_mock.clone());
            let cx = crate::cx::for_request();
            let cx_first = cx_restorer.restore_with_cx(&cx, &snapshot).await.unwrap();

            assert_eq!(legacy.panes_created, cx_first.panes_created);
            assert_eq!(legacy.windows_created, cx_first.windows_created);
            assert_eq!(legacy.tabs_created, cx_first.tabs_created);
            assert_eq!(legacy.failed_panes.len(), cx_first.failed_panes.len());
            assert_eq!(legacy.pane_id_map.len(), cx_first.pane_id_map.len());
            assert!(cx_first.pane_id_map.contains_key(&101));
            assert!(cx_first.pane_id_map.contains_key(&102));
        });
    }

    #[test]
    fn restore_horizontal_split() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let tree = hsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]);
            let snapshot = single_tab_snapshot(tree);

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 2);
            assert!(result.pane_id_map.contains_key(&1));
            assert!(result.pane_id_map.contains_key(&2));
            assert_ne!(result.pane_id_map[&1], result.pane_id_map[&2]);
        });
    }

    #[test]
    fn restore_vertical_split() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let tree = vsplit(vec![(0.5, leaf(10, None)), (0.5, leaf(11, None))]);
            let snapshot = single_tab_snapshot(tree);

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 2);
            assert!(result.pane_id_map.contains_key(&10));
            assert!(result.pane_id_map.contains_key(&11));
        });
    }

    #[test]
    fn restore_three_pane_l_shape() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let tree = vsplit(vec![
                (0.5, leaf(1, None)),
                (
                    0.5,
                    hsplit(vec![(0.5, leaf(2, None)), (0.5, leaf(3, None))]),
                ),
            ]);
            let snapshot = single_tab_snapshot(tree);

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 3);
            for id in [1, 2, 3] {
                assert!(result.pane_id_map.contains_key(&id));
            }
        });
    }

    #[test]
    fn restore_four_pane_grid() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let tree = hsplit(vec![
                (
                    0.5,
                    vsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]),
                ),
                (
                    0.5,
                    vsplit(vec![(0.5, leaf(3, None)), (0.5, leaf(4, None))]),
                ),
            ]);
            let snapshot = single_tab_snapshot(tree);

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 4);
            for id in 1..=4 {
                assert!(result.pane_id_map.contains_key(&id));
            }
            let new_ids: std::collections::HashSet<_> = result.pane_id_map.values().collect();
            assert_eq!(new_ids.len(), 4);
        });
    }

    #[test]
    fn restore_deeply_nested_splits() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let tree = hsplit(vec![
                (0.5, leaf(1, None)),
                (
                    0.5,
                    vsplit(vec![
                        (0.5, leaf(2, None)),
                        (
                            0.5,
                            hsplit(vec![
                                (0.5, leaf(3, None)),
                                (
                                    0.5,
                                    vsplit(vec![
                                        (0.5, leaf(4, None)),
                                        (
                                            0.5,
                                            hsplit(vec![
                                                (0.5, leaf(5, None)),
                                                (0.5, leaf(6, None)),
                                            ]),
                                        ),
                                    ]),
                                ),
                            ]),
                        ),
                    ]),
                ),
            ]);
            let snapshot = single_tab_snapshot(tree);

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 6);
            for id in 1..=6 {
                assert!(result.pane_id_map.contains_key(&id));
            }
        });
    }

    #[test]
    fn restore_multiple_tabs() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let snapshot = TopologySnapshot {
                schema_version: 1,
                captured_at: 1000,
                workspace_id: None,
                windows: vec![WindowSnapshot {
                    window_id: 0,
                    title: None,
                    position: None,
                    size: None,
                    tabs: vec![
                        TabSnapshot {
                            tab_id: 0,
                            title: None,
                            pane_tree: leaf(1, None),
                            active_pane_id: None,
                        },
                        TabSnapshot {
                            tab_id: 1,
                            title: None,
                            pane_tree: vsplit(vec![(0.5, leaf(2, None)), (0.5, leaf(3, None))]),
                            active_pane_id: Some(3),
                        },
                    ],
                    active_tab_index: Some(1),
                }],
            };

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.tabs_created, 2);
            assert_eq!(result.panes_created, 3);
            for id in 1..=3 {
                assert!(result.pane_id_map.contains_key(&id));
            }

            let pane_one = mock.pane_state(result.pane_id_map[&1]).await.unwrap();
            let pane_two = mock.pane_state(result.pane_id_map[&2]).await.unwrap();
            let pane_three = mock.pane_state(result.pane_id_map[&3]).await.unwrap();
            assert_eq!(pane_one.window_id, pane_two.window_id);
            assert_eq!(pane_two.window_id, pane_three.window_id);
            assert_ne!(pane_one.tab_id, pane_two.tab_id);
            assert_eq!(pane_two.tab_id, pane_three.tab_id);
        });
    }

    #[test]
    fn restore_multiple_windows() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let snapshot = TopologySnapshot {
                schema_version: 1,
                captured_at: 1000,
                workspace_id: None,
                windows: vec![
                    WindowSnapshot {
                        window_id: 0,
                        title: None,
                        position: None,
                        size: None,
                        tabs: vec![TabSnapshot {
                            tab_id: 0,
                            title: None,
                            pane_tree: leaf(1, None),
                            active_pane_id: None,
                        }],
                        active_tab_index: None,
                    },
                    WindowSnapshot {
                        window_id: 1,
                        title: None,
                        position: None,
                        size: None,
                        tabs: vec![TabSnapshot {
                            tab_id: 1,
                            title: None,
                            pane_tree: leaf(2, None),
                            active_pane_id: None,
                        }],
                        active_tab_index: None,
                    },
                ],
            };

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.windows_created, 2);
            assert_eq!(result.panes_created, 2);

            let pane_one = mock.pane_state(result.pane_id_map[&1]).await.unwrap();
            let pane_two = mock.pane_state(result.pane_id_map[&2]).await.unwrap();
            assert_ne!(pane_one.window_id, pane_two.window_id);
        });
    }

    #[test]
    fn restore_multiple_tabs_respects_active_tab_index() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let instrumented = Arc::new(InstrumentedWezterm::new(mock.clone(), false, false));
            let restorer = LayoutRestorer::new(instrumented.clone(), RestoreConfig::default());
            let snapshot = TopologySnapshot {
                schema_version: 1,
                captured_at: 1000,
                workspace_id: None,
                windows: vec![WindowSnapshot {
                    window_id: 0,
                    title: None,
                    position: None,
                    size: None,
                    tabs: vec![
                        TabSnapshot {
                            tab_id: 0,
                            title: None,
                            pane_tree: leaf(1, None),
                            active_pane_id: Some(1),
                        },
                        TabSnapshot {
                            tab_id: 1,
                            title: None,
                            pane_tree: vsplit(vec![(0.5, leaf(2, None)), (0.5, leaf(3, None))]),
                            active_pane_id: Some(3),
                        },
                    ],
                    active_tab_index: Some(0),
                }],
            };

            let result = restorer.restore(&snapshot).await.unwrap();

            let active_first = mock.pane_state(result.pane_id_map[&1]).await.unwrap();
            let active_second = mock.pane_state(result.pane_id_map[&3]).await.unwrap();
            assert!(active_first.is_active);
            assert!(
                active_second.is_active,
                "each tab retains its own active-pane marker"
            );
            assert_eq!(
                *instrumented
                    .activation_attempts
                    .lock()
                    .expect("activation-attempt lock poisoned"),
                vec![result.pane_id_map[&3], result.pane_id_map[&1]],
                "the non-selected tab selects its active pane during construction and the selected tab activates exactly once at final focus restoration"
            );
            assert_eq!(
                instrumented
                    .get_pane_attempts
                    .lock()
                    .expect("get-pane attempt lock poisoned")
                    .len(),
                1,
                "only the first tab needs a pane lookup to discover its new window id"
            );
        });
    }

    #[test]
    fn fail_fast_later_tab_reactivates_already_restored_selected_tab() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let instrumented = Arc::new(InstrumentedWezterm::new(mock.clone(), true, false));
            let restorer = LayoutRestorer::new(
                instrumented.clone(),
                RestoreConfig {
                    continue_on_error: false,
                    ..RestoreConfig::default()
                },
            );
            let snapshot = TopologySnapshot {
                schema_version: 1,
                captured_at: 1_000,
                workspace_id: None,
                windows: vec![WindowSnapshot {
                    window_id: 0,
                    title: None,
                    position: None,
                    size: None,
                    tabs: vec![
                        TabSnapshot {
                            tab_id: 0,
                            title: None,
                            pane_tree: leaf(1, None),
                            active_pane_id: Some(1),
                        },
                        TabSnapshot {
                            tab_id: 1,
                            title: None,
                            pane_tree: vsplit(vec![(0.5, leaf(2, None)), (0.5, leaf(3, None))]),
                            active_pane_id: Some(2),
                        },
                    ],
                    active_tab_index: Some(0),
                }],
            };

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.windows_created, 1);
            assert_eq!(result.tabs_created, 2);
            assert_eq!(result.panes_created, 2);
            assert_eq!(result.failed_panes, vec![(3, FAILURE_SPLIT.to_string())]);
            assert_eq!(
                *instrumented
                    .activation_attempts
                    .lock()
                    .expect("activation-attempt lock poisoned"),
                vec![result.pane_id_map[&1]],
                "fail-fast must still restore focus to an already-created selected tab"
            );
            assert!(
                mock.pane_state(result.pane_id_map[&1])
                    .await
                    .expect("selected pane exists")
                    .is_active
            );
        });
    }

    #[test]
    fn restore_activates_correct_pane() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let snapshot = TopologySnapshot {
                schema_version: 1,
                captured_at: 1000,
                workspace_id: None,
                windows: vec![WindowSnapshot {
                    window_id: 0,
                    title: None,
                    position: None,
                    size: None,
                    tabs: vec![TabSnapshot {
                        tab_id: 0,
                        title: None,
                        pane_tree: vsplit(vec![(0.5, leaf(1, None)), (0.5, active_leaf(2))]),
                        active_pane_id: Some(2),
                    }],
                    active_tab_index: None,
                }],
            };

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 2);
            assert!(result.pane_id_map.contains_key(&2));
            let active = mock.pane_state(result.pane_id_map[&2]).await.unwrap();
            assert!(active.is_active);
        });
    }

    #[test]
    fn restore_activation_failure_is_reported_once_for_mapped_pane() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let instrumented = Arc::new(InstrumentedWezterm::new(mock, false, true));
            let restorer = LayoutRestorer::new(instrumented.clone(), RestoreConfig::default());
            let snapshot = single_tab_snapshot(leaf(1, None));

            let result = restorer.restore(&snapshot).await.unwrap();

            assert!(result.pane_id_map.contains_key(&1));
            assert_eq!(
                result.failed_panes,
                vec![(1, FAILURE_ACTIVATION.to_string())]
            );
            assert_eq!(
                *instrumented
                    .activation_attempts
                    .lock()
                    .expect("activation-attempt lock poisoned"),
                vec![result.pane_id_map[&1]],
                "the selected tab receives exactly one final activation attempt"
            );
        });
    }

    #[test]
    fn normalize_cwd_file_uri() {
        assert_eq!(
            normalize_restored_cwd("file:///home/user").unwrap(),
            "/home/user"
        );
        assert_eq!(
            normalize_restored_cwd("file://localhost/home/user").unwrap(),
            "/home/user"
        );
        assert_eq!(normalize_restored_cwd("/home/user").unwrap(), "/home/user");
        assert_eq!(normalize_restored_cwd("file:///").unwrap(), "/");
    }

    #[test]
    fn normalize_cwd_plain_path() {
        assert_eq!(normalize_restored_cwd("/tmp/work").unwrap(), "/tmp/work");
        assert_eq!(
            normalize_restored_cwd("/tmp/100%literal").unwrap(),
            "/tmp/100%literal"
        );
        assert!(normalize_restored_cwd("relative/path").is_err());
    }

    #[test]
    fn split_percent_preserves_full_valid_range_and_rejects_broken_invariants() {
        assert_eq!(split_percent(1.0, 2.0).unwrap(), 50);
        assert_eq!(split_percent(7.0, 10.0).unwrap(), 70);
        assert_eq!(split_percent(2.0, 3.0).unwrap(), 67);
        assert_eq!(split_percent(f64::MIN_POSITIVE, 1.0).unwrap(), 1);
        assert_eq!(split_percent(1.0, 1.0).unwrap(), 99);
        assert!(split_percent(f64::NAN, 1.0).is_err());
        assert!(split_percent(1.0, 0.0).is_err());
        assert!(split_percent(2.0, 1.0).is_err());
        assert_eq!(equal_split_percent(4).unwrap(), 25);
        assert_eq!(equal_split_percent(3).unwrap(), 33);
        assert_eq!(equal_split_percent(2).unwrap(), 50);
        assert!(equal_split_percent(1).is_err());
    }

    #[test]
    fn shell_escape_simple() {
        assert_eq!(shell_escape("/home/user"), "'/home/user'");
    }

    #[test]
    fn shell_escape_with_quotes() {
        assert_eq!(shell_escape("/home/user's dir"), "'/home/user'\\''s dir'");
    }

    #[test]
    fn shell_escape_with_spaces() {
        assert_eq!(shell_escape("/home/my dir"), "'/home/my dir'");
    }

    #[test]
    fn count_panes_single() {
        let snapshot = single_tab_snapshot(leaf(1, None));
        assert_eq!(count_panes(&snapshot), 1);
    }

    #[test]
    fn count_panes_complex() {
        let tree = hsplit(vec![
            (
                0.5,
                vsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]),
            ),
            (0.5, leaf(3, None)),
        ]);
        let snapshot = single_tab_snapshot(tree);
        assert_eq!(count_panes(&snapshot), 3);
    }

    #[test]
    fn restore_empty_snapshot_fails_before_mux_effects() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let snapshot = TopologySnapshot {
                schema_version: 1,
                captured_at: 1000,
                workspace_id: None,
                windows: vec![],
            };

            restorer
                .restore(&snapshot)
                .await
                .expect_err("empty topology must fail closed");
            assert!(mock.list_panes().await.unwrap().is_empty());
        });
    }

    #[test]
    fn restore_empty_window_fails_before_mux_effects() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let snapshot = TopologySnapshot {
                schema_version: TOPOLOGY_SCHEMA_VERSION,
                captured_at: 1000,
                workspace_id: None,
                windows: vec![WindowSnapshot {
                    window_id: 0,
                    title: None,
                    position: None,
                    size: None,
                    tabs: Vec::new(),
                    active_tab_index: None,
                }],
            };

            restorer
                .restore(&snapshot)
                .await
                .expect_err("empty windows must fail closed");
            assert!(mock.list_panes().await.unwrap().is_empty());
        });
    }

    #[test]
    fn restore_invalid_cwd_fails_before_mux_effects() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let snapshot = single_tab_snapshot(leaf(1, Some("relative/path")));

            restorer
                .restore(&snapshot)
                .await
                .expect_err("relative working directories must fail closed");
            assert!(mock.list_panes().await.unwrap().is_empty());
        });
    }

    #[test]
    fn contradictory_active_authority_fails_before_mux_effects() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let mut snapshot =
                single_tab_snapshot(vsplit(vec![(0.5, leaf(1, None)), (0.5, active_leaf(2))]));
            snapshot.windows[0].tabs[0].active_pane_id = Some(1);

            restorer
                .restore(&snapshot)
                .await
                .expect_err("contradictory active-pane authorities must fail preflight");
            assert!(mock.list_panes().await.unwrap().is_empty());
        });
    }

    #[test]
    fn restore_oversized_plain_cwd_fails_before_mux_effects() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let cwd = format!("/{}", "a".repeat(MAX_RESTORED_CWD_BYTES));
            let snapshot = single_tab_snapshot(leaf(1, Some(&cwd)));

            restorer
                .restore(&snapshot)
                .await
                .expect_err("oversized working directories must fail preflight");
            assert!(mock.list_panes().await.unwrap().is_empty());
        });
    }

    #[test]
    fn restore_oversized_decoded_file_uri_fails_before_mux_effects() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let cwd = format!("file:///{}", "%61".repeat(MAX_RESTORED_CWD_BYTES));
            assert!(
                cwd.len() <= MAX_ENCODED_RESTORED_CWD_BYTES,
                "fixture must reach the decoded-size guard"
            );
            let snapshot = single_tab_snapshot(leaf(1, Some(&cwd)));

            restorer
                .restore(&snapshot)
                .await
                .expect_err("oversized decoded file URIs must fail preflight");
            assert!(mock.list_panes().await.unwrap().is_empty());
        });
    }

    #[test]
    fn restore_zero_leaf_tree_fails_before_mux_effects() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let snapshot = single_tab_snapshot(hsplit(Vec::new()));

            restorer
                .restore(&snapshot)
                .await
                .expect_err("zero-leaf pane trees must fail closed");
            assert!(mock.list_panes().await.unwrap().is_empty());
        });
    }

    #[test]
    fn restore_spawn_backend_error_is_preserved() {
        run_async_test(async {
            let wezterm: WeztermHandle = Arc::new(AlwaysFailSpawnWezterm);
            let restorer = LayoutRestorer::new(wezterm, RestoreConfig::default());
            let snapshot = single_tab_snapshot(leaf(1, None));

            let error = restorer
                .restore(&snapshot)
                .await
                .expect_err("non-authoritative spawn failures must propagate");
            assert!(matches!(
                error,
                crate::Error::RuntimeOperation {
                    operation: "restore_layout.test.spawn_targeted",
                    source: RuntimeOperationSource::Backend(_),
                }
            ));
        });
    }

    #[test]
    fn restore_fail_fast_returns_truthful_partial_report() {
        run_async_test(async {
            let inner = Arc::new(MockWezterm::new());
            let wezterm: WeztermHandle =
                Arc::new(InstrumentedWezterm::new(inner.clone(), true, false));
            let restorer = LayoutRestorer::new(
                wezterm,
                RestoreConfig {
                    continue_on_error: false,
                    ..RestoreConfig::default()
                },
            );
            let snapshot =
                single_tab_snapshot(vsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]));

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.windows_created, 1);
            assert_eq!(result.tabs_created, 1);
            assert_eq!(result.panes_created, 1);
            assert_eq!(result.pane_id_map.len(), 1);
            assert!(result.pane_id_map.contains_key(&1));
            assert_eq!(result.failed_panes, vec![(2, FAILURE_SPLIT.to_string())]);
        });
    }

    #[test]
    fn authoritative_split_rejection_is_not_overwritten_by_late_cancellation() {
        run_async_test(async {
            let inner = Arc::new(MockWezterm::new());
            let wezterm: WeztermHandle = Arc::new(InstrumentedWezterm::with_split_failure(
                inner,
                InjectedMutationFailure::AuthoritativePaneRejectionThenCancel,
            ));
            let restorer = LayoutRestorer::new(
                wezterm,
                RestoreConfig {
                    continue_on_error: false,
                    ..RestoreConfig::default()
                },
            );
            let snapshot =
                single_tab_snapshot(vsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]));
            let cx = crate::cx::for_testing();

            let result = restorer
                .restore_with_cx(&cx, &snapshot)
                .await
                .expect("authoritative rejection must produce the partial receipt");

            assert!(cx.is_cancel_requested());
            assert_eq!(result.panes_created, 1);
            assert!(result.pane_id_map.contains_key(&1));
            assert_eq!(result.failed_panes, vec![(2, FAILURE_SPLIT.to_string())]);
        });
    }

    #[test]
    fn uncertain_and_structural_split_failures_propagate_unchanged() {
        run_async_test(async {
            for failure in [
                InjectedMutationFailure::Indeterminate,
                InjectedMutationFailure::DeadlineExceeded,
                InjectedMutationFailure::ContextFailure,
                InjectedMutationFailure::LockPoisoned,
            ] {
                let inner = Arc::new(MockWezterm::new());
                let wezterm: WeztermHandle = Arc::new(InstrumentedWezterm::with_split_failure(
                    inner.clone(),
                    failure,
                ));
                let restorer = LayoutRestorer::new(wezterm, RestoreConfig::default());
                let snapshot =
                    single_tab_snapshot(vsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]));

                let error = restorer
                    .restore(&snapshot)
                    .await
                    .expect_err("non-authoritative mutation failure must propagate");
                match failure {
                    InjectedMutationFailure::Indeterminate => assert!(matches!(
                        error,
                        crate::Error::Wezterm(crate::error::WeztermError::IndeterminateMutation {
                            operation: "restore_layout.test.split_pane_with_cx"
                        })
                    )),
                    InjectedMutationFailure::DeadlineExceeded => assert!(matches!(
                        error,
                        crate::Error::RuntimeOperation {
                            operation: "restore_layout.test.split_pane_with_cx",
                            source: RuntimeOperationSource::DeadlineExceeded,
                        }
                    )),
                    InjectedMutationFailure::ContextFailure => assert!(matches!(
                        error,
                        crate::Error::RuntimeOperation {
                            operation: "restore_layout.test.split_pane_with_cx",
                            source: RuntimeOperationSource::ContextFailure,
                        }
                    )),
                    InjectedMutationFailure::LockPoisoned => assert!(matches!(
                        error,
                        crate::Error::RuntimeOperation {
                            operation: "restore_layout.test.split_pane_with_cx",
                            source: RuntimeOperationSource::LockPoisoned,
                        }
                    )),
                    InjectedMutationFailure::AuthoritativePaneRejection
                    | InjectedMutationFailure::AuthoritativePaneRejectionThenCancel => {
                        unreachable!("authoritative cases are covered separately")
                    }
                }
                assert_eq!(
                    inner.list_panes().await.unwrap().len(),
                    1,
                    "the successful root spawn must remain visible after later failure"
                );
            }
        });
    }

    #[test]
    fn typed_attempt_retains_partial_mapping_after_indeterminate_split() {
        run_async_test(async {
            let inner = Arc::new(MockWezterm::new());
            let wezterm: WeztermHandle = Arc::new(InstrumentedWezterm::with_split_failure(
                inner.clone(),
                InjectedMutationFailure::Indeterminate,
            ));
            let restorer = LayoutRestorer::new(wezterm, RestoreConfig::default());
            let snapshot =
                single_tab_snapshot(vsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]));
            let cx = crate::cx::for_testing();

            let attempt = restorer.restore_attempt_with_cx(&cx, &snapshot).await;
            assert_eq!(attempt.result.windows_created, 1);
            assert_eq!(attempt.result.tabs_created, 1);
            assert_eq!(attempt.result.panes_created, 1);
            assert_eq!(attempt.result.pane_id_map.len(), 1);
            assert!(attempt.result.pane_id_map.contains_key(&1));
            assert_eq!(
                attempt.interruption,
                Some(LayoutRestoreInterruption {
                    phase: "restore_layout.test.split_pane_with_cx",
                    reason: LayoutRestoreInterruptionReason::MuxOutcomeIndeterminate,
                })
            );
            assert_eq!(inner.list_panes().await.unwrap().len(), 1);
            let diagnostic = format!("{attempt:?}");
            assert!(!diagnostic.contains("raw-"));
            assert!(diagnostic.len() < 512);
        });
    }

    #[test]
    fn created_pane_mapping_is_idempotent_and_bijective() {
        let mut state = RestoreAccumulator::with_capacity(2);
        state.record_created_pane(1, 10).unwrap();
        state.record_created_pane(1, 10).unwrap();
        assert_eq!(state.result.panes_created, 1);
        assert_eq!(state.result.pane_id_map, HashMap::from([(1, 10)]));

        assert!(state.record_created_pane(1, 11).is_err());
        assert!(state.record_created_pane(2, 10).is_err());
        assert_eq!(state.result.panes_created, 1);
        assert_eq!(state.result.pane_id_map, HashMap::from([(1, 10)]));
    }

    #[test]
    fn parent_as_child_and_duplicate_split_receipts_fail_reconciliation() {
        run_async_test(async {
            for (receipt, tree, expected_operation) in [
                (
                    InjectedSplitReceipt::ParentPaneId,
                    vsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]),
                    "restore_layout.pane_mapping.new_id_conflict",
                ),
                (
                    InjectedSplitReceipt::Fixed(999),
                    vsplit(vec![
                        (1.0, leaf(1, None)),
                        (1.0, leaf(2, None)),
                        (1.0, leaf(3, None)),
                    ]),
                    "restore_layout.pane_mapping.new_id_conflict",
                ),
            ] {
                let inner = Arc::new(MockWezterm::new());
                let wezterm: WeztermHandle = Arc::new(InstrumentedWezterm::with_split_receipt(
                    inner.clone(),
                    receipt,
                ));
                let restorer = LayoutRestorer::new(wezterm, RestoreConfig::default());
                let snapshot = single_tab_snapshot(tree);

                let error = restorer
                    .restore(&snapshot)
                    .await
                    .expect_err("duplicate backend pane receipts must fail closed");
                assert!(matches!(
                    error,
                    crate::Error::RuntimeOperation {
                        operation,
                        source: RuntimeOperationSource::Backend(_),
                    } if operation == expected_operation
                ));
                assert_eq!(
                    inner.list_panes().await.unwrap().len(),
                    1,
                    "the malicious receipt must not create mock backend state"
                );
            }
        });
    }

    #[test]
    fn restore_partial_first_tab_reuses_created_window_for_later_tabs() {
        run_async_test(async {
            let inner = Arc::new(MockWezterm::new());
            let wezterm: WeztermHandle =
                Arc::new(InstrumentedWezterm::new(inner.clone(), true, false));
            let restorer = LayoutRestorer::new(wezterm, RestoreConfig::default());
            let snapshot = TopologySnapshot {
                schema_version: 1,
                captured_at: 1000,
                workspace_id: None,
                windows: vec![WindowSnapshot {
                    window_id: 0,
                    title: None,
                    position: None,
                    size: None,
                    tabs: vec![
                        TabSnapshot {
                            tab_id: 0,
                            title: None,
                            pane_tree: vsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]),
                            active_pane_id: None,
                        },
                        TabSnapshot {
                            tab_id: 1,
                            title: None,
                            pane_tree: leaf(3, None),
                            active_pane_id: None,
                        },
                    ],
                    active_tab_index: None,
                }],
            };

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.windows_created, 1);
            assert_eq!(result.tabs_created, 2);
            assert!(result.pane_id_map.contains_key(&1));
            assert!(result.pane_id_map.contains_key(&3));
            assert_eq!(result.failed_panes, vec![(2, FAILURE_SPLIT.to_string())]);

            let first_tab_pane = inner.pane_state(result.pane_id_map[&1]).await.unwrap();
            let second_tab_pane = inner.pane_state(result.pane_id_map[&3]).await.unwrap();
            assert_eq!(first_tab_pane.window_id, second_tab_pane.window_id);
            assert_ne!(first_tab_pane.tab_id, second_tab_pane.tab_id);
        });
    }

    #[test]
    fn first_leaf_cwd_from_leaf() {
        let node = leaf(1, Some("/home/user"));
        let cwd_map = HashMap::from([(1, "/home/user".to_string())]);
        assert_eq!(first_leaf_cwd(&node, &cwd_map), Some("/home/user"));
    }

    #[test]
    fn first_leaf_cwd_from_split() {
        let node = vsplit(vec![
            (0.5, leaf(1, Some("/tmp"))),
            (0.5, leaf(2, Some("/home"))),
        ]);
        let cwd_map = HashMap::from([(1, "/tmp".to_string()), (2, "/home".to_string())]);
        assert_eq!(first_leaf_cwd(&node, &cwd_map), Some("/tmp"));
    }

    #[test]
    fn first_leaf_cwd_none() {
        let node = leaf(1, None);
        assert_eq!(first_leaf_cwd(&node, &HashMap::new()), None);
    }

    #[test]
    fn collect_leaf_ids_from_tree() {
        let tree = hsplit(vec![
            (
                0.5,
                vsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]),
            ),
            (0.5, leaf(3, None)),
        ]);
        let ids = collect_leaf_ids(&tree);
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn restore_three_way_split() {
        run_async_test(async {
            let inner = Arc::new(MockWezterm::new());
            let instrumented = Arc::new(InstrumentedWezterm::new(inner, false, false));
            let restorer = LayoutRestorer::new(instrumented.clone(), RestoreConfig::default());
            let tree = vsplit(vec![
                (1.0, leaf(1, None)),
                (2.0, leaf(2, None)),
                (7.0, leaf(3, None)),
            ]);
            let snapshot = single_tab_snapshot(tree);

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 3);
            let new_ids: std::collections::HashSet<_> = result.pane_id_map.values().collect();
            assert_eq!(new_ids.len(), 3);
            let split_attempts = instrumented
                .split_attempts
                .lock()
                .expect("split-attempt lock poisoned")
                .clone();
            assert_eq!(
                split_attempts,
                vec![
                    (result.pane_id_map[&1], SplitDirection::Right, Some(70)),
                    (result.pane_id_map[&1], SplitDirection::Right, Some(67)),
                ]
            );
        });
    }

    #[test]
    fn restore_extreme_ratio_prefixes_without_suffix_cancellation() {
        run_async_test(async {
            let inner = Arc::new(MockWezterm::new());
            let instrumented = Arc::new(InstrumentedWezterm::new(inner, false, false));
            let restorer = LayoutRestorer::new(instrumented.clone(), RestoreConfig::default());
            let snapshot = single_tab_snapshot(vsplit(vec![
                (1.0, leaf(1, None)),
                (2.0, leaf(2, None)),
                (f64::MAX, leaf(3, None)),
            ]));

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 3);
            assert_eq!(
                *instrumented
                    .split_attempts
                    .lock()
                    .expect("split-attempt lock poisoned"),
                vec![
                    (result.pane_id_map[&1], SplitDirection::Right, Some(99)),
                    (result.pane_id_map[&1], SplitDirection::Right, Some(67)),
                ]
            );
        });
    }

    #[test]
    fn disabled_ratio_restore_builds_four_equal_siblings() {
        run_async_test(async {
            let inner = Arc::new(MockWezterm::new());
            let instrumented = Arc::new(InstrumentedWezterm::new(inner, false, false));
            let restorer = LayoutRestorer::new(
                instrumented.clone(),
                RestoreConfig {
                    restore_split_ratios: false,
                    ..RestoreConfig::default()
                },
            );
            let snapshot = single_tab_snapshot(vsplit(vec![
                (9.0, leaf(1, None)),
                (3.0, leaf(2, None)),
                (2.0, leaf(3, None)),
                (1.0, leaf(4, None)),
            ]));

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 4);
            assert_eq!(
                *instrumented
                    .split_attempts
                    .lock()
                    .expect("split-attempt lock poisoned"),
                vec![
                    (result.pane_id_map[&1], SplitDirection::Right, Some(25)),
                    (result.pane_id_map[&1], SplitDirection::Right, Some(33)),
                    (result.pane_id_map[&1], SplitDirection::Right, Some(50)),
                ]
            );
        });
    }

    #[test]
    fn pane_id_map_completeness() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let tree = hsplit(vec![
                (0.25, leaf(100, None)),
                (0.25, leaf(200, None)),
                (0.25, leaf(300, None)),
                (0.25, leaf(400, None)),
            ]);
            let snapshot = single_tab_snapshot(tree);

            let result = restorer.restore(&snapshot).await.unwrap();

            let all_leaves = collect_leaf_ids(&snapshot.windows[0].tabs[0].pane_tree);
            for id in &all_leaves {
                assert!(
                    result.pane_id_map.contains_key(id),
                    "pane {id} missing from pane_id_map"
                );
            }
            assert_eq!(result.pane_id_map.len(), all_leaves.len());
        });
    }

    #[test]
    fn restore_sets_cwd_from_file_uri() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let restorer = make_restorer(mock.clone());
            let tree = leaf(1, Some("file:///home/agent/project"));
            let snapshot = single_tab_snapshot(tree);

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 1);
            let new_id = result.pane_id_map[&1];
            let pane = mock.pane_state(new_id).await.unwrap();
            assert_eq!(pane.cwd, "/home/agent/project");
        });
    }

    #[test]
    fn config_skip_cwd_restore() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let config = RestoreConfig {
                restore_working_dirs: false,
                ..Default::default()
            };
            let restorer = LayoutRestorer::new(mock.clone(), config);
            let tree = leaf(1, Some("/captured/working-directory"));
            let snapshot = single_tab_snapshot(tree);

            let result = restorer.restore(&snapshot).await.unwrap();

            assert_eq!(result.panes_created, 1);
            let new_id = result.pane_id_map[&1];
            let pane = mock.pane_state(new_id).await.unwrap();
            assert_eq!(pane.cwd, "/home/user", "backend default cwd should remain");
        });
    }

    #[test]
    fn restore_result_new_is_empty() {
        let r = RestoreResult::new();
        assert!(r.pane_id_map.is_empty());
        assert_eq!(r.failed_panes, [] as [(u64, std::string::String); 0]);
        assert_eq!(r.windows_created, 0);
        assert_eq!(r.tabs_created, 0);
        assert_eq!(r.panes_created, 0);
    }

    #[test]
    fn restore_config_defaults() {
        let c = RestoreConfig::default();
        assert!(c.restore_working_dirs);
        assert!(c.restore_split_ratios);
        assert!(c.continue_on_error);
    }

    // =================================================================
    // RestoreConfig additional tests
    // =================================================================

    #[test]
    fn restore_config_serde_roundtrip() {
        let config = RestoreConfig {
            restore_working_dirs: false,
            restore_split_ratios: true,
            continue_on_error: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: RestoreConfig = serde_json::from_str(&json).unwrap();
        assert!(!back.restore_working_dirs);
        assert!(back.restore_split_ratios);
        assert!(!back.continue_on_error);
    }

    #[test]
    fn restore_config_serde_default_fills_missing() {
        // Empty JSON object should deserialize with defaults (#[serde(default)])
        let back: RestoreConfig = serde_json::from_str("{}").unwrap();
        assert!(back.restore_working_dirs);
        assert!(back.restore_split_ratios);
        assert!(back.continue_on_error);
    }

    #[test]
    fn restore_config_debug() {
        let c = RestoreConfig::default();
        let dbg = format!("{c:?}");
        assert!(dbg.contains("RestoreConfig"));
        assert!(dbg.contains("restore_working_dirs"));
    }

    // =================================================================
    // normalize_cwd edge cases
    // =================================================================

    #[test]
    fn normalize_cwd_empty_string() {
        assert!(normalize_restored_cwd("").is_err());
    }

    #[test]
    fn normalize_cwd_rejects_non_local_file_uri_authority() {
        assert!(normalize_restored_cwd("file://myhost/home/user").is_err());
    }

    #[test]
    fn normalize_cwd_file_uri_empty_authority() {
        assert!(normalize_restored_cwd("file://").is_err());
    }

    #[test]
    fn normalize_cwd_file_uri_root_only() {
        assert_eq!(normalize_restored_cwd("file:///").unwrap(), "/");
    }

    #[test]
    fn normalize_cwd_file_uri_with_spaces() {
        assert_eq!(
            normalize_restored_cwd("file:///home/my user/project").unwrap(),
            "/home/my user/project"
        );
    }

    #[test]
    fn normalize_cwd_rejects_tilde_path() {
        assert!(normalize_restored_cwd("~/projects").is_err());
    }

    #[test]
    fn normalize_cwd_rejects_foreign_windows_path_on_unix() {
        if cfg!(unix) {
            assert!(normalize_restored_cwd("C:\\Users\\test").is_err());
        }
    }

    #[test]
    fn normalize_cwd_strictly_decodes_percent_encoding() {
        assert_eq!(
            normalize_restored_cwd("file:///home/my%20project").unwrap(),
            "/home/my project"
        );
        assert!(normalize_restored_cwd("file:///home/%zz").is_err());
        assert!(normalize_restored_cwd("file:///home/%").is_err());
        assert!(normalize_restored_cwd("file:///home/%00bad").is_err());
    }

    #[test]
    fn normalize_cwd_rejects_file_uri_query_and_fragment_delimiters() {
        assert!(normalize_restored_cwd("file:///tmp/project?query").is_err());
        assert!(normalize_restored_cwd("file:///tmp/project#fragment").is_err());
        assert_eq!(
            normalize_restored_cwd("file:///tmp/project%3Fquery%23fragment").unwrap(),
            "/tmp/project?query#fragment"
        );
        assert_eq!(
            normalize_restored_cwd("/tmp/project?query#fragment").unwrap(),
            "/tmp/project?query#fragment"
        );
    }

    // =================================================================
    // shell_escape edge cases
    // =================================================================

    #[test]
    fn shell_escape_empty_string() {
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn shell_escape_with_dollar_sign() {
        assert_eq!(shell_escape("/home/$USER"), "'/home/$USER'");
    }

    #[test]
    fn shell_escape_with_special_chars() {
        // Shell special chars (;, &, |, >) should be safely quoted
        assert_eq!(shell_escape("cmd; rm -rf /"), "'cmd; rm -rf /'");
    }

    #[test]
    fn shell_escape_with_newline() {
        assert_eq!(shell_escape("line1\nline2"), "'line1\nline2'");
    }

    #[test]
    fn shell_escape_multiple_single_quotes() {
        assert_eq!(shell_escape("it's a 'test'"), "'it'\\''s a '\\''test'\\'''",);
    }

    #[test]
    fn shell_escape_only_single_quote() {
        assert_eq!(shell_escape("'"), "''\\'''");
    }

    // =================================================================
    // collect_leaf_ids edge cases
    // =================================================================

    #[test]
    fn collect_leaf_ids_single_leaf() {
        let node = leaf(42, None);
        assert_eq!(collect_leaf_ids(&node), vec![42]);
    }

    #[test]
    fn collect_leaf_ids_empty_hsplit() {
        let node = hsplit(vec![]);
        assert_eq!(collect_leaf_ids(&node), Vec::<u64>::new());
    }

    #[test]
    fn collect_leaf_ids_empty_vsplit() {
        let node = vsplit(vec![]);
        assert_eq!(collect_leaf_ids(&node), Vec::<u64>::new());
    }

    #[test]
    fn collect_leaf_ids_deeply_nested() {
        let tree = hsplit(vec![(
            1.0,
            vsplit(vec![(
                1.0,
                hsplit(vec![(1.0, vsplit(vec![(1.0, leaf(99, None))]))]),
            )]),
        )]);
        assert_eq!(collect_leaf_ids(&tree), vec![99]);
    }

    #[test]
    fn collect_leaf_ids_preserves_order() {
        // Left-to-right, depth-first traversal order
        let tree = vsplit(vec![
            (0.33, leaf(5, None)),
            (
                0.33,
                hsplit(vec![(0.5, leaf(3, None)), (0.5, leaf(7, None))]),
            ),
            (0.34, leaf(1, None)),
        ]);
        assert_eq!(collect_leaf_ids(&tree), vec![5, 3, 7, 1]);
    }

    // =================================================================
    // first_leaf_cwd edge cases
    // =================================================================

    #[test]
    fn first_leaf_cwd_nested_three_levels() {
        let tree = hsplit(vec![(
            1.0,
            vsplit(vec![(
                1.0,
                hsplit(vec![(1.0, leaf(1, Some("/deep/path")))]),
            )]),
        )]);
        let cwd_map = HashMap::from([(1, "/deep/path".to_string())]);
        assert_eq!(first_leaf_cwd(&tree, &cwd_map), Some("/deep/path"));
    }

    #[test]
    fn first_leaf_cwd_file_uri_normalized() {
        let node = leaf(1, Some("file:///home/agent"));
        let cwd_map = HashMap::from([(1, normalize_restored_cwd("file:///home/agent").unwrap())]);
        assert_eq!(first_leaf_cwd(&node, &cwd_map), Some("/home/agent"));
    }

    #[test]
    fn first_leaf_cwd_empty_children() {
        let node = hsplit(vec![]);
        assert_eq!(first_leaf_cwd(&node, &HashMap::new()), None);
    }

    #[test]
    fn first_leaf_cwd_first_child_no_cwd_second_has() {
        // first_leaf_cwd returns the FIRST leaf's cwd, even if None
        let tree = vsplit(vec![
            (0.5, leaf(1, None)),
            (0.5, leaf(2, Some("/home/user"))),
        ]);
        let cwd_map = HashMap::from([(2, "/home/user".to_string())]);
        assert_eq!(first_leaf_cwd(&tree, &cwd_map), None);
    }

    #[test]
    fn first_leaf_cwd_hsplit_vs_vsplit() {
        let tree_h = hsplit(vec![(1.0, leaf(1, Some("/h")))]);
        let tree_v = vsplit(vec![(1.0, leaf(1, Some("/v")))]);
        assert_eq!(
            first_leaf_cwd(&tree_h, &HashMap::from([(1, "/h".to_string())])),
            Some("/h")
        );
        assert_eq!(
            first_leaf_cwd(&tree_v, &HashMap::from([(1, "/v".to_string())])),
            Some("/v")
        );
    }

    // =================================================================
    // count_leaves / count_panes edge cases
    // =================================================================

    #[test]
    fn count_leaves_single() {
        assert_eq!(count_leaves(&leaf(1, None)), 1);
    }

    #[test]
    fn count_leaves_empty_split() {
        assert_eq!(count_leaves(&hsplit(vec![])), 0);
        assert_eq!(count_leaves(&vsplit(vec![])), 0);
    }

    #[test]
    fn count_leaves_deeply_nested() {
        let tree = vsplit(vec![(
            1.0,
            hsplit(vec![
                (0.5, leaf(1, None)),
                (
                    0.5,
                    vsplit(vec![(0.5, leaf(2, None)), (0.5, leaf(3, None))]),
                ),
            ]),
        )]);
        assert_eq!(count_leaves(&tree), 3);
    }

    #[test]
    fn count_panes_empty_snapshot() {
        let snapshot = TopologySnapshot {
            schema_version: 1,
            captured_at: 0,
            workspace_id: None,
            windows: vec![],
        };
        assert_eq!(count_panes(&snapshot), 0);
    }

    #[test]
    fn count_panes_multi_window_multi_tab() {
        let snapshot = TopologySnapshot {
            schema_version: 1,
            captured_at: 0,
            workspace_id: None,
            windows: vec![
                WindowSnapshot {
                    window_id: 0,
                    title: None,
                    position: None,
                    size: None,
                    tabs: vec![
                        TabSnapshot {
                            tab_id: 0,
                            title: None,
                            pane_tree: vsplit(vec![(0.5, leaf(1, None)), (0.5, leaf(2, None))]),
                            active_pane_id: None,
                        },
                        TabSnapshot {
                            tab_id: 1,
                            title: None,
                            pane_tree: leaf(3, None),
                            active_pane_id: None,
                        },
                    ],
                    active_tab_index: None,
                },
                WindowSnapshot {
                    window_id: 1,
                    title: None,
                    position: None,
                    size: None,
                    tabs: vec![TabSnapshot {
                        tab_id: 2,
                        title: None,
                        pane_tree: hsplit(vec![(0.5, leaf(4, None)), (0.5, leaf(5, None))]),
                        active_pane_id: None,
                    }],
                    active_tab_index: None,
                },
            ],
        };
        assert_eq!(count_panes(&snapshot), 5);
    }

    // =================================================================
    // RestoreResult tests
    // =================================================================

    #[test]
    fn restore_result_debug_is_aggregate_and_content_free() {
        let mut r = RestoreResult::new();
        r.pane_id_map.insert(777_777, 888_888);
        r.failed_panes
            .push((777_777, "SECRET backend pane failure".to_string()));
        let dbg = format!("{r:?}");
        assert!(dbg.contains("RestoreResult"));
        assert!(dbg.contains("pane_mapping_count: 1"));
        assert!(dbg.contains("failed_pane_count: 1"));
        assert!(!dbg.contains("777777"));
        assert!(!dbg.contains("888888"));
        assert!(!dbg.contains("SECRET"));
    }
}
