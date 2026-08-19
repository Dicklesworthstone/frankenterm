use crate::client::ClientId;
use crate::domain::{Domain, DomainId, UnpublishedPane};
use crate::layout::{redistribute_panes, LayoutCycle, PaneStack, SwapLayout};
use crate::pane::*;
use crate::renderable::StableCursorPosition;
use crate::{
    DesiredPaneStructuralState, DomainAuthorityTabReplacement, ExactPaneAuthorityState,
    ExactPaneStructuralState, FrozenFloatingPaneSpawn, Mux, MuxNotification,
    MuxNotificationEnvelope, PaneOperationGuard, PaneRegistrationHandle, PaneStructuralLane,
    RelocatedPaneStructuralState, SplitCommitReceipt, StructuralRelocationTabReplacement, WindowId,
};
use anyhow::Context;
use bintree::PathBranch;
use config::configuration;
use config::keyassignment::PaneDirection;
use frankenterm_sigpipe::{catch_recoverable, RecoverablePanicSite};
use frankenterm_term::{StableRowIndex, TerminalSize};
use parking_lot::{Mutex, MutexGuard};
use rangeset::intersects_range;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
#[cfg(test)]
use std::panic::catch_unwind;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use thiserror::Error;
use url::Url;

pub type Tree = bintree::Tree<Arc<dyn Pane>, SplitDirectionAndSize>;
pub type Cursor = bintree::Cursor<Arc<dyn Pane>, SplitDirectionAndSize>;

static TAB_ID: ::std::sync::atomic::AtomicUsize = ::std::sync::atomic::AtomicUsize::new(0);
pub type TabId = usize;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct TabStackId(pub usize);

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct TabStackEntry {
    pub stack_id: TabStackId,
    pub tab_id: TabId,
    pub position: usize,
    pub is_visible: bool,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TabStackError {
    EmptyStack,
    DuplicateTab(TabId),
    TabAlreadyStacked { tab_id: TabId, stack_id: TabStackId },
    MissingStack(TabStackId),
    MissingTab(TabId),
}

#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TabStackState {
    stacks: HashMap<TabStackId, Vec<TabId>>,
    tab_to_stack: HashMap<TabId, TabStackId>,
    visible_by_stack: HashMap<TabStackId, usize>,
}

fn wrapped_index_delta(current: usize, len: usize, delta: isize) -> usize {
    debug_assert!(len > 0);
    let current = current % len;
    let offset = delta.unsigned_abs() % len;

    if delta >= 0 {
        if current >= len - offset {
            current - (len - offset)
        } else {
            current + offset
        }
    } else if current >= offset {
        current - offset
    } else {
        len - (offset - current)
    }
}

fn offset_by_resize_delta(value: usize, delta: isize) -> usize {
    if delta >= 0 {
        value.saturating_add(delta as usize)
    } else {
        value.saturating_sub(delta.unsigned_abs())
    }
}

fn pixel_span(cell_pixels: usize, cells: usize) -> usize {
    cell_pixels.saturating_mul(cells)
}

fn split_separator_offset(value: usize) -> usize {
    value.saturating_add(1)
}

fn next_pane_index(value: usize) -> usize {
    value.saturating_add(1)
}

fn split_separator_sum(first: usize, second: usize) -> usize {
    first.saturating_add(second).saturating_add(1)
}

fn checked_split_separator_sum(first: usize, second: usize) -> Option<usize> {
    first.checked_add(second)?.checked_add(1)
}

fn positive_resize_budget(value: usize) -> isize {
    value.min(isize::MAX as usize) as isize
}

fn usize_to_isize_saturating(value: usize) -> isize {
    value.min(isize::MAX as usize) as isize
}

fn negative_resize_budget(value: usize) -> isize {
    if value > isize::MAX as usize {
        isize::MIN
    } else {
        -(value as isize)
    }
}

fn resize_delta_between(next: usize, current: usize) -> isize {
    if next >= current {
        positive_resize_budget(next - current)
    } else {
        negative_resize_budget(current - next)
    }
}

fn resize_delta_for_direction(direction: PaneDirection, amount: usize) -> isize {
    match direction {
        PaneDirection::Down | PaneDirection::Right => positive_resize_budget(amount),
        PaneDirection::Up | PaneDirection::Left => negative_resize_budget(amount),
        PaneDirection::Next | PaneDirection::Prev => unreachable!(),
    }
}

impl TabStackState {
    pub fn create_stack(
        &mut self,
        stack_id: TabStackId,
        tabs: Vec<TabId>,
    ) -> Result<(), TabStackError> {
        if tabs.is_empty() {
            return Err(TabStackError::EmptyStack);
        }

        let mut seen = HashSet::new();
        for tab_id in &tabs {
            if !seen.insert(*tab_id) {
                return Err(TabStackError::DuplicateTab(*tab_id));
            }
            if let Some(existing) = self.tab_to_stack.get(tab_id).copied() {
                return Err(TabStackError::TabAlreadyStacked {
                    tab_id: *tab_id,
                    stack_id: existing,
                });
            }
        }

        for tab_id in &tabs {
            self.tab_to_stack.insert(*tab_id, stack_id);
        }
        self.visible_by_stack.insert(stack_id, 0);
        self.stacks.insert(stack_id, tabs);
        Ok(())
    }

    pub fn stack_for_tab(&self, tab_id: TabId) -> Option<TabStackId> {
        self.tab_to_stack.get(&tab_id).copied()
    }

    pub fn tabs_in_stack(&self, stack_id: TabStackId) -> Option<&[TabId]> {
        self.stacks.get(&stack_id).map(Vec::as_slice)
    }

    pub fn visible_tab(&self, stack_id: TabStackId) -> Option<TabId> {
        let tabs = self.stacks.get(&stack_id)?;
        let visible_idx = self.visible_by_stack.get(&stack_id).copied().unwrap_or(0);
        tabs.get(visible_idx).copied()
    }

    pub fn cycle_visible(&mut self, stack_id: TabStackId, delta: isize) -> Option<TabId> {
        let tabs = self.stacks.get(&stack_id)?;
        if tabs.is_empty() {
            return None;
        }
        let next = wrapped_index_delta(
            self.visible_by_stack.get(&stack_id).copied().unwrap_or(0),
            tabs.len(),
            delta,
        );
        self.visible_by_stack.insert(stack_id, next);
        tabs.get(next).copied()
    }

    pub fn move_tab_to_stack(
        &mut self,
        tab_id: TabId,
        stack_id: TabStackId,
        position: usize,
    ) -> Result<(), TabStackError> {
        if !self.stacks.contains_key(&stack_id) {
            return Err(TabStackError::MissingStack(stack_id));
        }

        if let Some(old_stack_id) = self.tab_to_stack.get(&tab_id).copied() {
            self.remove_tab_from_stack(tab_id, old_stack_id)?;
        }

        let tabs = self
            .stacks
            .get_mut(&stack_id)
            .ok_or(TabStackError::MissingStack(stack_id))?;
        let idx = position.min(tabs.len());
        tabs.insert(idx, tab_id);
        self.tab_to_stack.insert(tab_id, stack_id);
        let visible_idx = self.visible_by_stack.get(&stack_id).copied().unwrap_or(0);
        if idx <= visible_idx && tabs.len() > 1 {
            self.visible_by_stack.insert(stack_id, visible_idx + 1);
        }
        Ok(())
    }

    pub fn remove_stack(&mut self, stack_id: TabStackId) -> Option<Vec<TabId>> {
        let tabs = self.stacks.remove(&stack_id)?;
        for tab_id in &tabs {
            self.tab_to_stack.remove(tab_id);
        }
        self.visible_by_stack.remove(&stack_id);
        Some(tabs)
    }

    pub fn remove_tab(&mut self, tab_id: TabId) -> Option<TabStackId> {
        let stack_id = self.tab_to_stack.get(&tab_id).copied()?;
        self.remove_tab_from_stack(tab_id, stack_id).ok()?;
        Some(stack_id)
    }

    pub fn stack_count(&self) -> usize {
        self.stacks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stacks.is_empty()
    }

    pub fn overview_entries(&self) -> Vec<TabStackEntry> {
        let mut entries = Vec::new();
        let mut stack_ids: Vec<TabStackId> = self.stacks.keys().copied().collect();
        stack_ids.sort_unstable();

        for stack_id in stack_ids {
            let visible_idx = self.visible_by_stack.get(&stack_id).copied().unwrap_or(0);
            if let Some(tabs) = self.stacks.get(&stack_id) {
                entries.extend(
                    tabs.iter()
                        .enumerate()
                        .map(|(position, tab_id)| TabStackEntry {
                            stack_id,
                            tab_id: *tab_id,
                            position,
                            is_visible: position == visible_idx,
                        }),
                );
            }
        }

        entries
    }

    fn remove_tab_from_stack(
        &mut self,
        tab_id: TabId,
        stack_id: TabStackId,
    ) -> Result<(), TabStackError> {
        let tabs = self
            .stacks
            .get_mut(&stack_id)
            .ok_or(TabStackError::MissingStack(stack_id))?;
        let idx = tabs
            .iter()
            .position(|candidate| *candidate == tab_id)
            .ok_or(TabStackError::MissingTab(tab_id))?;
        tabs.remove(idx);
        self.tab_to_stack.remove(&tab_id);

        if tabs.is_empty() {
            self.stacks.remove(&stack_id);
            self.visible_by_stack.remove(&stack_id);
            return Ok(());
        }

        let visible_idx = self.visible_by_stack.get(&stack_id).copied().unwrap_or(0);
        let adjusted = if visible_idx >= tabs.len() {
            tabs.len() - 1
        } else if idx < visible_idx {
            visible_idx - 1
        } else {
            visible_idx
        };
        self.visible_by_stack.insert(stack_id, adjusted);
        Ok(())
    }
}

#[derive(Clone, Default)]
struct Recency {
    count: usize,
    by_idx: HashMap<usize, usize>,
}

impl Recency {
    fn tag(&mut self, idx: usize) {
        self.by_idx.insert(idx, self.count);
        self.count = self.count.saturating_add(1);
    }

    fn score(&self, idx: usize) -> usize {
        self.by_idx.get(&idx).copied().unwrap_or(0)
    }
}

#[derive(Clone)]
struct TabInner {
    id: TabId,
    /// Exact mux authority for this tab allocation. A tab may be retired and
    /// re-registered with the same mux, but it must never acquire authority
    /// from a different process-global singleton.
    mux_owner: Weak<Mux>,
    mux_owner_bound: bool,
    mux_owner_active: bool,
    mux_owner_generation: u64,
    pane: Option<Tree>,
    floating_panes: Vec<FloatingPane>,
    floating_focus: Option<PaneId>,
    size: TerminalSize,
    size_before_zoom: TerminalSize,
    active: usize,
    zoomed: Option<Arc<dyn Pane>>,
    /// Shared so callback-free snapshot coherence can retain exact title
    /// identity without cloning an attacker-controlled string before metadata
    /// byte admission.
    title: Arc<str>,
    recency: Recency,
    /// Set of pane IDs that have been collapsed because the terminal
    /// shrank below the aggregate minimum constraints.  Collapsed panes
    /// retain their tree position but are allocated zero space.
    collapsed_panes: HashSet<PaneId>,
    /// Optional layout cycle for swap-layout support.
    /// When set, the user can cycle through pre-defined arrangements.
    layout_cycle: Option<LayoutCycle>,
    /// Pane stacks: slot_index → PaneStack.  When a layout has fewer
    /// slots than panes, overflow panes are stacked in the last slot.
    /// Only the active pane in each stack is visible in the tree.
    pane_stacks: HashMap<usize, PaneStack>,
    /// Runtime overrides applied via the mux protocol.
    /// These override `Pane::pane_constraints()` without requiring the
    /// underlying pane implementation to expose mutation hooks.
    constraint_overrides: HashMap<PaneId, PaneConstraints>,
}

/// A Tab is a container of Panes
pub struct Tab {
    inner: Mutex<TabInner>,
    tab_id: TabId,
    mux_owner_generation: AtomicU64,
}

pub(crate) struct PreparedTabMuxOwnerBinding<'a> {
    tab: &'a Tab,
    inner: MutexGuard<'a, TabInner>,
    mux: Arc<Mux>,
    next_generation: u64,
}

impl PreparedTabMuxOwnerBinding<'_> {
    /// Commit the already validated binding without allocating or failing.
    /// Keeping `Tab::inner` locked in the token closes the prepare/commit race.
    pub(crate) fn commit(mut self) -> u64 {
        self.inner
            .commit_mux_owner_binding(&self.mux, self.next_generation);
        self.tab
            .mux_owner_generation
            .store(self.next_generation, Ordering::Release);
        self.next_generation
    }
}

type PaneIdentity = *const ();

fn pane_identity(pane: &Arc<dyn Pane>) -> PaneIdentity {
    Arc::as_ptr(pane).cast::<()>()
}

/// Exact callback-free and callback work admitted while producing pane snapshots.
///
/// These fields are intentionally numeric and content-free so callers can retain
/// high-water telemetry without exposing pane titles, working directories, or
/// terminal contents.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct PaneSnapshotCensusStats {
    pub tree_nodes: usize,
    pub stack_containers: usize,
    pub stack_members: usize,
    pub floating_panes: usize,
    pub zoom_carriers: usize,
    pub identity_checks: usize,
    pub pane_callbacks: usize,
    pub assembly_nodes: usize,
}

impl PaneSnapshotCensusStats {
    pub fn total(self) -> Option<usize> {
        self.tree_nodes
            .checked_add(self.stack_containers)?
            .checked_add(self.stack_members)?
            .checked_add(self.floating_panes)?
            .checked_add(self.zoom_carriers)?
            .checked_add(self.identity_checks)?
            .checked_add(self.pane_callbacks)?
            .checked_add(self.assembly_nodes)
    }

    fn checked_delta(self, prior: Self) -> anyhow::Result<Self> {
        Ok(Self {
            tree_nodes: self
                .tree_nodes
                .checked_sub(prior.tree_nodes)
                .context("pane-snapshot tree work regressed")?,
            stack_containers: self
                .stack_containers
                .checked_sub(prior.stack_containers)
                .context("pane-snapshot stack-container work regressed")?,
            stack_members: self
                .stack_members
                .checked_sub(prior.stack_members)
                .context("pane-snapshot stack-member work regressed")?,
            floating_panes: self
                .floating_panes
                .checked_sub(prior.floating_panes)
                .context("pane-snapshot floating work regressed")?,
            zoom_carriers: self
                .zoom_carriers
                .checked_sub(prior.zoom_carriers)
                .context("pane-snapshot zoom work regressed")?,
            identity_checks: self
                .identity_checks
                .checked_sub(prior.identity_checks)
                .context("pane-snapshot identity work regressed")?,
            pane_callbacks: self
                .pane_callbacks
                .checked_sub(prior.pane_callbacks)
                .context("pane-snapshot callback work regressed")?,
            assembly_nodes: self
                .assembly_nodes
                .checked_sub(prior.assembly_nodes)
                .context("pane-snapshot assembly work regressed")?,
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum PaneSnapshotCensusKind {
    TreeNode,
    StackContainer,
    StackMember,
    FloatingPane,
    ZoomCarrier,
    IdentityCheck,
    PaneCallback,
    AssemblyNode,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PaneSnapshotCensusRejection {
    AttemptOverflow,
    RequestOverflow,
    AttemptLimit,
    RequestLimit,
}

#[derive(Debug, Clone, Copy, Eq, Error, PartialEq)]
pub enum PaneSnapshotStructureRejection {
    #[error("pane-snapshot tree depth {count} exceeds limit {max}")]
    TreeDepthLimit { count: usize, max: usize },
    #[error("pane-snapshot tree node count {count} exceeds limit {max}")]
    TreeNodeLimit { count: usize, max: usize },
    #[error("pane-snapshot tree leaf count {count} exceeds limit {max}")]
    TreeLeafLimit { count: usize, max: usize },
    #[error("pane-snapshot {counter} counter overflowed")]
    ArithmeticOverflow { counter: &'static str },
}

/// One request-scoped pane-snapshot work authority.
///
/// `begin_attempt` resets only the per-attempt counter. The request counter is
/// monotonic across coherence retries, preventing a moving topology from
/// multiplying an otherwise finite snapshot budget without limit.
#[derive(Debug)]
pub struct PaneSnapshotCensusLedger {
    per_attempt_limit: usize,
    request_limit: usize,
    attempt: PaneSnapshotCensusStats,
    request: PaneSnapshotCensusStats,
    last_rejection: Option<PaneSnapshotCensusRejection>,
    callbacks_avoided: usize,
}

/// Authority-bearing UTF-8 fields emitted by PDU82/PDU87 pane snapshots.
///
/// The finite variants are also safe metric labels. Rejection errors expose
/// only one of these labels and numeric limits; rejected content is never
/// formatted or logged.
#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub enum PaneSnapshotMetadataField {
    WindowTitle,
    WindowWorkspace,
    TabTitle,
    PaneTitle,
    PaneWorkingDir,
    PaneWorkspace,
    PaneTtyName,
}

impl PaneSnapshotMetadataField {
    const COUNT: usize = 7;

    pub const ALL: [Self; Self::COUNT] = [
        Self::WindowTitle,
        Self::WindowWorkspace,
        Self::TabTitle,
        Self::PaneTitle,
        Self::PaneWorkingDir,
        Self::PaneWorkspace,
        Self::PaneTtyName,
    ];

    const fn index(self) -> usize {
        match self {
            Self::WindowTitle => 0,
            Self::WindowWorkspace => 1,
            Self::TabTitle => 2,
            Self::PaneTitle => 3,
            Self::PaneWorkingDir => 4,
            Self::PaneWorkspace => 5,
            Self::PaneTtyName => 6,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::WindowTitle => "window_title",
            Self::WindowWorkspace => "window_workspace",
            Self::TabTitle => "tab_title",
            Self::PaneTitle => "pane_title",
            Self::PaneWorkingDir => "pane_working_dir",
            Self::PaneWorkspace => "pane_workspace",
            Self::PaneTtyName => "pane_tty_name",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct PaneSnapshotMetadataUsage {
    pub values: usize,
    /// Exact dynamic allocation capacity retained by admitted snapshot fields.
    pub retained_bytes: usize,
    /// Exact varbincode bytes attributable to those fields, including each
    /// string length and optional-value tag.
    pub encoded_bytes: usize,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct PaneSnapshotMetadataStats {
    fields: [PaneSnapshotMetadataUsage; PaneSnapshotMetadataField::COUNT],
}

impl PaneSnapshotMetadataStats {
    pub const fn field(self, field: PaneSnapshotMetadataField) -> PaneSnapshotMetadataUsage {
        self.fields[field.index()]
    }

    pub fn total(self) -> Option<PaneSnapshotMetadataUsage> {
        self.fields.iter().copied().try_fold(
            PaneSnapshotMetadataUsage::default(),
            |total, field| {
                Some(PaneSnapshotMetadataUsage {
                    values: total.values.checked_add(field.values)?,
                    retained_bytes: total.retained_bytes.checked_add(field.retained_bytes)?,
                    encoded_bytes: total.encoded_bytes.checked_add(field.encoded_bytes)?,
                })
            },
        )
    }

    fn checked_delta(self, prior: Self) -> anyhow::Result<PaneSnapshotMetadataUsage> {
        for (current, prior) in self.fields.iter().zip(prior.fields.iter()) {
            current
                .values
                .checked_sub(prior.values)
                .context("pane-snapshot metadata field value count regressed")?;
            current
                .retained_bytes
                .checked_sub(prior.retained_bytes)
                .context("pane-snapshot metadata field retained bytes regressed")?;
            current
                .encoded_bytes
                .checked_sub(prior.encoded_bytes)
                .context("pane-snapshot metadata field encoded bytes regressed")?;
        }
        let current = self
            .total()
            .context("pane-snapshot metadata totals overflowed")?;
        let prior = prior
            .total()
            .context("pane-snapshot prior metadata totals overflowed")?;
        Ok(PaneSnapshotMetadataUsage {
            values: current
                .values
                .checked_sub(prior.values)
                .context("pane-snapshot metadata value count regressed")?,
            retained_bytes: current
                .retained_bytes
                .checked_sub(prior.retained_bytes)
                .context("pane-snapshot retained metadata bytes regressed")?,
            encoded_bytes: current
                .encoded_bytes
                .checked_sub(prior.encoded_bytes)
                .context("pane-snapshot encoded metadata bytes regressed")?,
        })
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct PaneSnapshotMetadataLimits {
    per_field_bytes: [usize; PaneSnapshotMetadataField::COUNT],
    pub attempt_retained_bytes: usize,
    pub attempt_encoded_bytes: usize,
    pub request_retained_bytes: usize,
    pub request_encoded_bytes: usize,
}

impl PaneSnapshotMetadataLimits {
    pub const fn new(
        per_field_bytes: [usize; PaneSnapshotMetadataField::COUNT],
        attempt_retained_bytes: usize,
        attempt_encoded_bytes: usize,
        request_retained_bytes: usize,
        request_encoded_bytes: usize,
    ) -> Self {
        Self {
            per_field_bytes,
            attempt_retained_bytes,
            attempt_encoded_bytes,
            request_retained_bytes,
            request_encoded_bytes,
        }
    }

    pub const fn per_field_bytes(self, field: PaneSnapshotMetadataField) -> usize {
        self.per_field_bytes[field.index()]
    }

    const fn unbounded() -> Self {
        Self::new(
            [usize::MAX; PaneSnapshotMetadataField::COUNT],
            usize::MAX,
            usize::MAX,
            usize::MAX,
            usize::MAX,
        )
    }
}

#[derive(Debug, Clone, Copy, Eq, Error, PartialEq)]
pub enum PaneSnapshotMetadataRejection {
    #[error("pane-snapshot {field} metadata exceeds its per-field byte limit")]
    FieldLimit { field: &'static str },
    #[error("pane-snapshot metadata accounting overflowed")]
    ArithmeticOverflow,
    #[error("pane-snapshot attempt retained-metadata byte budget exhausted")]
    AttemptRetainedLimit,
    #[error("pane-snapshot attempt encoded-metadata byte budget exhausted")]
    AttemptEncodedLimit,
    #[error("pane-snapshot request retained-metadata byte budget exhausted")]
    RequestRetainedLimit,
    #[error("pane-snapshot request encoded-metadata byte budget exhausted")]
    RequestEncodedLimit,
}

/// One request-scoped UTF-8 metadata authority shared across coherence retries.
///
/// Failed-attempt values are dropped by the producer, while request totals stay
/// monotonic. This simultaneously bounds live attempt retention and adversarial
/// retry work. The accounting is content-free and does not inspect code points:
/// Rust string length is already the exact canonical UTF-8 byte length.
#[derive(Debug)]
pub struct PaneSnapshotMetadataLedger {
    limits: PaneSnapshotMetadataLimits,
    attempt: PaneSnapshotMetadataStats,
    request: PaneSnapshotMetadataStats,
    last_rejection: Option<PaneSnapshotMetadataRejection>,
    retry_released_bytes: usize,
    attempt_peak_retained_bytes: usize,
    attempt_peak_encoded_bytes: usize,
    unreported_admitted_values: [usize; PaneSnapshotMetadataField::COUNT],
}

impl PaneSnapshotMetadataLedger {
    pub fn new(limits: PaneSnapshotMetadataLimits) -> anyhow::Result<Self> {
        if limits.per_field_bytes.contains(&0)
            || limits.attempt_retained_bytes == 0
            || limits.attempt_encoded_bytes == 0
            || limits.request_retained_bytes < limits.attempt_retained_bytes
            || limits.request_encoded_bytes < limits.attempt_encoded_bytes
        {
            anyhow::bail!(
                "pane-snapshot metadata limits require nonzero field/attempt limits not exceeding request limits"
            );
        }
        Ok(Self {
            limits,
            attempt: PaneSnapshotMetadataStats::default(),
            request: PaneSnapshotMetadataStats::default(),
            last_rejection: None,
            retry_released_bytes: 0,
            attempt_peak_retained_bytes: 0,
            attempt_peak_encoded_bytes: 0,
            unreported_admitted_values: [0; PaneSnapshotMetadataField::COUNT],
        })
    }

    pub fn begin_attempt(&mut self) {
        self.attempt = PaneSnapshotMetadataStats::default();
        self.last_rejection = None;
        self.attempt_peak_retained_bytes = 0;
        self.attempt_peak_encoded_bytes = 0;
    }

    pub const fn attempt_stats(&self) -> PaneSnapshotMetadataStats {
        self.attempt
    }

    pub const fn request_stats(&self) -> PaneSnapshotMetadataStats {
        self.request
    }

    pub const fn last_rejection(&self) -> Option<PaneSnapshotMetadataRejection> {
        self.last_rejection
    }

    pub const fn attempt_peak_retained_bytes(&self) -> usize {
        self.attempt_peak_retained_bytes
    }

    pub const fn attempt_peak_encoded_bytes(&self) -> usize {
        self.attempt_peak_encoded_bytes
    }

    pub const fn attempt_checkpoint(&self) -> PaneSnapshotMetadataStats {
        self.attempt
    }

    /// Release values retained by a failed coherence attempt while preserving
    /// monotonic request accounting. This keeps the attempt total equal to live
    /// response ownership rather than cumulative retry work.
    pub fn release_attempt_to(
        &mut self,
        checkpoint: PaneSnapshotMetadataStats,
    ) -> anyhow::Result<PaneSnapshotMetadataUsage> {
        let released = self.attempt.checked_delta(checkpoint)?;
        self.retry_released_bytes = self
            .retry_released_bytes
            .checked_add(released.retained_bytes)
            .context("pane-snapshot released metadata byte accounting overflow")?;
        self.attempt = checkpoint;
        Ok(released)
    }

    pub fn take_retry_released_bytes(&mut self) -> usize {
        std::mem::take(&mut self.retry_released_bytes)
    }

    pub fn take_unreported_admitted_values(
        &mut self,
    ) -> [(PaneSnapshotMetadataField, usize); PaneSnapshotMetadataField::COUNT] {
        let values = std::mem::take(&mut self.unreported_admitted_values);
        PaneSnapshotMetadataField::ALL.map(|field| (field, values[field.index()]))
    }

    /// Reject a field whose logical UTF-8 payload is too large before the
    /// producer allocates its owned snapshot copy. Aggregate admission still
    /// happens after allocation so retained bytes use the copy's actual
    /// capacity rather than assuming capacity equals length.
    pub fn preflight_field(
        &mut self,
        field: PaneSnapshotMetadataField,
        value: &str,
    ) -> Result<(), PaneSnapshotMetadataRejection> {
        let result = if value.len() > self.limits.per_field_bytes(field) {
            Err(PaneSnapshotMetadataRejection::FieldLimit {
                field: field.label(),
            })
        } else {
            Ok(())
        };
        if let Err(rejection) = result {
            self.last_rejection = Some(rejection);
        }
        result
    }

    pub fn preflight_required_string(
        &mut self,
        field: PaneSnapshotMetadataField,
        value: &str,
    ) -> Result<(), PaneSnapshotMetadataRejection> {
        let encoded = encoded_string_bytes(value.len()).inspect_err(|rejection| {
            self.last_rejection = Some(*rejection);
        })?;
        self.preflight_admission(field, value.len(), value.len(), encoded)
    }

    pub fn preflight_retained_only_string(
        &mut self,
        field: PaneSnapshotMetadataField,
        value: &str,
    ) -> Result<(), PaneSnapshotMetadataRejection> {
        self.preflight_admission(field, value.len(), value.len(), 0)
    }

    pub fn preflight_optional_string(
        &mut self,
        field: PaneSnapshotMetadataField,
        value: &str,
    ) -> Result<(), PaneSnapshotMetadataRejection> {
        let encoded = encoded_string_bytes(value.len())
            .and_then(|encoded| {
                encoded
                    .checked_add(1)
                    .ok_or(PaneSnapshotMetadataRejection::ArithmeticOverflow)
            })
            .inspect_err(|rejection| {
                self.last_rejection = Some(*rejection);
            })?;
        self.preflight_admission(field, value.len(), value.len(), encoded)
    }

    /// Prove that the minimum one-byte option tag can still be admitted before
    /// invoking an optional-string getter whose presence and length are not yet
    /// known. A fully exhausted encoded-byte budget therefore stops the getter
    /// rather than performing work for a value that cannot be represented.
    pub fn preflight_optional_value(
        &mut self,
        field: PaneSnapshotMetadataField,
    ) -> Result<(), PaneSnapshotMetadataRejection> {
        self.preflight_admission(field, 0, 0, 1)
    }

    fn preflight_admission(
        &mut self,
        field: PaneSnapshotMetadataField,
        utf8_bytes: usize,
        minimum_retained_bytes: usize,
        encoded_bytes: usize,
    ) -> Result<(), PaneSnapshotMetadataRejection> {
        let result = self
            .checked_admission(field, utf8_bytes, minimum_retained_bytes, encoded_bytes)
            .map(|_| ());
        if let Err(rejection) = result {
            self.last_rejection = Some(rejection);
        }
        result
    }

    pub fn admit_required_owned(
        &mut self,
        field: PaneSnapshotMetadataField,
        value: &str,
        retained_capacity: usize,
    ) -> Result<(), PaneSnapshotMetadataRejection> {
        let encoded = encoded_string_bytes(value.len()).inspect_err(|rejection| {
            self.last_rejection = Some(*rejection);
        })?;
        self.admit(field, value.len(), retained_capacity, encoded)
    }

    /// Charge an owned producer-side source string that is not itself emitted.
    /// Window workspace is retained once to provide the owner value later
    /// copied into each pane entry; those emitted copies are charged separately.
    pub fn admit_retained_only_owned(
        &mut self,
        field: PaneSnapshotMetadataField,
        value: &str,
        retained_capacity: usize,
    ) -> Result<(), PaneSnapshotMetadataRejection> {
        self.admit(field, value.len(), retained_capacity, 0)
    }

    pub fn admit_optional_owned(
        &mut self,
        field: PaneSnapshotMetadataField,
        value: &str,
        retained_capacity: usize,
    ) -> Result<(), PaneSnapshotMetadataRejection> {
        let encoded = encoded_string_bytes(value.len())
            .and_then(|encoded| {
                encoded
                    .checked_add(1)
                    .ok_or(PaneSnapshotMetadataRejection::ArithmeticOverflow)
            })
            .inspect_err(|rejection| {
                self.last_rejection = Some(*rejection);
            })?;
        self.admit(field, value.len(), retained_capacity, encoded)
    }

    /// Charge the one-byte varbincode option tag even when no UTF-8 payload is
    /// present, so admitted encoded metadata remains exact.
    pub fn admit_optional_none(
        &mut self,
        field: PaneSnapshotMetadataField,
    ) -> Result<(), PaneSnapshotMetadataRejection> {
        self.admit(field, 0, 0, 1)
    }

    /// Release a temporary producer-owned source after all response copies
    /// have been assembled. Request accounting intentionally remains
    /// monotonic; only the exact live-attempt retention drops.
    pub fn release_retained_only(
        &mut self,
        field: PaneSnapshotMetadataField,
        retained_capacity: usize,
    ) -> anyhow::Result<()> {
        let usage = &mut self.attempt.fields[field.index()];
        usage.retained_bytes = usage
            .retained_bytes
            .checked_sub(retained_capacity)
            .context("pane-snapshot temporary metadata retention regressed")?;
        Ok(())
    }

    fn admit(
        &mut self,
        field: PaneSnapshotMetadataField,
        utf8_bytes: usize,
        retained_bytes: usize,
        encoded_bytes: usize,
    ) -> Result<(), PaneSnapshotMetadataRejection> {
        let result = (|| {
            let field_index = field.index();
            let (next_attempt, next_request) =
                self.checked_admission(field, utf8_bytes, retained_bytes, encoded_bytes)?;
            let next_attempt_total = next_attempt
                .total()
                .ok_or(PaneSnapshotMetadataRejection::ArithmeticOverflow)?;
            let next_unreported_values = self.unreported_admitted_values[field_index]
                .checked_add(1)
                .ok_or(PaneSnapshotMetadataRejection::ArithmeticOverflow)?;
            self.attempt = next_attempt;
            self.request = next_request;
            self.attempt_peak_retained_bytes = self
                .attempt_peak_retained_bytes
                .max(next_attempt_total.retained_bytes);
            self.attempt_peak_encoded_bytes = self
                .attempt_peak_encoded_bytes
                .max(next_attempt_total.encoded_bytes);
            self.unreported_admitted_values[field_index] = next_unreported_values;
            Ok(())
        })();
        if let Err(rejection) = result {
            self.last_rejection = Some(rejection);
        }
        result
    }

    fn checked_admission(
        &self,
        field: PaneSnapshotMetadataField,
        utf8_bytes: usize,
        retained_bytes: usize,
        encoded_bytes: usize,
    ) -> Result<(PaneSnapshotMetadataStats, PaneSnapshotMetadataStats), PaneSnapshotMetadataRejection>
    {
        if utf8_bytes > self.limits.per_field_bytes(field) || retained_bytes < utf8_bytes {
            return Err(PaneSnapshotMetadataRejection::FieldLimit {
                field: field.label(),
            });
        }
        let field_index = field.index();
        let next_attempt =
            checked_metadata_admission(self.attempt, field_index, retained_bytes, encoded_bytes)?;
        let next_request =
            checked_metadata_admission(self.request, field_index, retained_bytes, encoded_bytes)?;
        let attempt_total = next_attempt
            .total()
            .ok_or(PaneSnapshotMetadataRejection::ArithmeticOverflow)?;
        let request_total = next_request
            .total()
            .ok_or(PaneSnapshotMetadataRejection::ArithmeticOverflow)?;
        if attempt_total.retained_bytes > self.limits.attempt_retained_bytes {
            return Err(PaneSnapshotMetadataRejection::AttemptRetainedLimit);
        }
        if attempt_total.encoded_bytes > self.limits.attempt_encoded_bytes {
            return Err(PaneSnapshotMetadataRejection::AttemptEncodedLimit);
        }
        if request_total.retained_bytes > self.limits.request_retained_bytes {
            return Err(PaneSnapshotMetadataRejection::RequestRetainedLimit);
        }
        if request_total.encoded_bytes > self.limits.request_encoded_bytes {
            return Err(PaneSnapshotMetadataRejection::RequestEncodedLimit);
        }
        Ok((next_attempt, next_request))
    }
}

fn checked_metadata_admission(
    mut stats: PaneSnapshotMetadataStats,
    field_index: usize,
    retained_bytes: usize,
    encoded_bytes: usize,
) -> Result<PaneSnapshotMetadataStats, PaneSnapshotMetadataRejection> {
    let field = &mut stats.fields[field_index];
    field.values = field
        .values
        .checked_add(1)
        .ok_or(PaneSnapshotMetadataRejection::ArithmeticOverflow)?;
    field.retained_bytes = field
        .retained_bytes
        .checked_add(retained_bytes)
        .ok_or(PaneSnapshotMetadataRejection::ArithmeticOverflow)?;
    field.encoded_bytes = field
        .encoded_bytes
        .checked_add(encoded_bytes)
        .ok_or(PaneSnapshotMetadataRejection::ArithmeticOverflow)?;
    Ok(stats)
}

fn encoded_string_bytes(len: usize) -> Result<usize, PaneSnapshotMetadataRejection> {
    let len_u64 =
        u64::try_from(len).map_err(|_| PaneSnapshotMetadataRejection::ArithmeticOverflow)?;
    let mut prefix = 1usize;
    let mut remaining = len_u64 >> 7;
    while remaining != 0 {
        prefix = prefix
            .checked_add(1)
            .ok_or(PaneSnapshotMetadataRejection::ArithmeticOverflow)?;
        remaining >>= 7;
    }
    prefix
        .checked_add(len)
        .ok_or(PaneSnapshotMetadataRejection::ArithmeticOverflow)
}

impl PaneSnapshotCensusLedger {
    pub fn new(per_attempt_limit: usize, request_limit: usize) -> anyhow::Result<Self> {
        if per_attempt_limit == 0 || request_limit < per_attempt_limit {
            anyhow::bail!(
                "pane-snapshot census limits require a nonzero attempt limit not exceeding the request limit"
            );
        }
        Ok(Self {
            per_attempt_limit,
            request_limit,
            attempt: PaneSnapshotCensusStats::default(),
            request: PaneSnapshotCensusStats::default(),
            last_rejection: None,
            callbacks_avoided: 0,
        })
    }

    pub fn begin_attempt(&mut self) {
        self.attempt = PaneSnapshotCensusStats::default();
        self.last_rejection = None;
        self.callbacks_avoided = 0;
    }

    pub fn attempt_stats(&self) -> PaneSnapshotCensusStats {
        self.attempt
    }

    pub fn request_stats(&self) -> PaneSnapshotCensusStats {
        self.request
    }

    pub fn remaining_in_attempt(&self) -> usize {
        self.per_attempt_limit
            .saturating_sub(self.attempt.total().unwrap_or(usize::MAX))
    }

    pub fn last_rejection(&self) -> Option<PaneSnapshotCensusRejection> {
        self.last_rejection
    }

    pub fn callbacks_avoided(&self) -> usize {
        self.callbacks_avoided
    }

    pub fn reserve_pane_callbacks(&mut self, count: usize) -> anyhow::Result<()> {
        self.reserve(PaneSnapshotCensusKind::PaneCallback, count)
    }

    /// Prove that an indivisible callback sequence fits without charging work
    /// that has not happened yet. Callers then reserve each callback as it is
    /// invoked, preserving exact actual-work telemetry when metadata admission
    /// stops a partially observed entry.
    pub fn preflight_pane_callbacks(&mut self, count: usize) -> anyhow::Result<()> {
        self.preflight_work_inner(count, true)
    }

    /// Prove that a minimum amount of subsequent snapshot work remains
    /// admissible without charging it before it happens. This lets callers
    /// reject an exhausted request before cloning unrelated metadata.
    pub fn preflight_work(&mut self, count: usize) -> anyhow::Result<()> {
        self.preflight_work_inner(count, false)
    }

    fn preflight_work_inner(
        &mut self,
        count: usize,
        counts_as_avoided_callbacks: bool,
    ) -> anyhow::Result<()> {
        let Some(next_attempt) = self
            .attempt
            .total()
            .and_then(|total| total.checked_add(count))
        else {
            self.last_rejection = Some(PaneSnapshotCensusRejection::AttemptOverflow);
            if counts_as_avoided_callbacks {
                self.callbacks_avoided = count;
            }
            anyhow::bail!("pane-snapshot attempt census work overflow");
        };
        let Some(next_request) = self
            .request
            .total()
            .and_then(|total| total.checked_add(count))
        else {
            self.last_rejection = Some(PaneSnapshotCensusRejection::RequestOverflow);
            if counts_as_avoided_callbacks {
                self.callbacks_avoided = count;
            }
            anyhow::bail!("pane-snapshot request census work overflow");
        };
        if next_attempt > self.per_attempt_limit {
            self.last_rejection = Some(PaneSnapshotCensusRejection::AttemptLimit);
            if counts_as_avoided_callbacks {
                self.callbacks_avoided = count;
            }
            anyhow::bail!("pane-snapshot attempt census work budget exhausted");
        }
        if next_request > self.request_limit {
            self.last_rejection = Some(PaneSnapshotCensusRejection::RequestLimit);
            if counts_as_avoided_callbacks {
                self.callbacks_avoided = count;
            }
            anyhow::bail!("pane-snapshot request census work budget exhausted");
        }
        Ok(())
    }

    pub fn reserve_assembly_nodes(&mut self, count: usize) -> anyhow::Result<()> {
        self.reserve(PaneSnapshotCensusKind::AssemblyNode, count)
    }

    fn reserve(&mut self, kind: PaneSnapshotCensusKind, count: usize) -> anyhow::Result<()> {
        let Some(next_attempt) = self
            .attempt
            .total()
            .and_then(|total| total.checked_add(count))
        else {
            self.last_rejection = Some(PaneSnapshotCensusRejection::AttemptOverflow);
            anyhow::bail!("pane-snapshot attempt census work overflow");
        };
        let Some(next_request) = self
            .request
            .total()
            .and_then(|total| total.checked_add(count))
        else {
            self.last_rejection = Some(PaneSnapshotCensusRejection::RequestOverflow);
            anyhow::bail!("pane-snapshot request census work overflow");
        };
        if next_attempt > self.per_attempt_limit {
            self.last_rejection = Some(PaneSnapshotCensusRejection::AttemptLimit);
            if matches!(kind, PaneSnapshotCensusKind::PaneCallback) {
                self.callbacks_avoided = count;
            }
            anyhow::bail!("pane-snapshot attempt census work budget exhausted");
        }
        if next_request > self.request_limit {
            self.last_rejection = Some(PaneSnapshotCensusRejection::RequestLimit);
            if matches!(kind, PaneSnapshotCensusKind::PaneCallback) {
                self.callbacks_avoided = count;
            }
            anyhow::bail!("pane-snapshot request census work budget exhausted");
        }

        let attempt = match kind {
            PaneSnapshotCensusKind::TreeNode => &mut self.attempt.tree_nodes,
            PaneSnapshotCensusKind::StackContainer => &mut self.attempt.stack_containers,
            PaneSnapshotCensusKind::StackMember => &mut self.attempt.stack_members,
            PaneSnapshotCensusKind::FloatingPane => &mut self.attempt.floating_panes,
            PaneSnapshotCensusKind::ZoomCarrier => &mut self.attempt.zoom_carriers,
            PaneSnapshotCensusKind::IdentityCheck => &mut self.attempt.identity_checks,
            PaneSnapshotCensusKind::PaneCallback => &mut self.attempt.pane_callbacks,
            PaneSnapshotCensusKind::AssemblyNode => &mut self.attempt.assembly_nodes,
        };
        *attempt = attempt
            .checked_add(count)
            .context("pane-snapshot attempt category overflow")?;
        let request = match kind {
            PaneSnapshotCensusKind::TreeNode => &mut self.request.tree_nodes,
            PaneSnapshotCensusKind::StackContainer => &mut self.request.stack_containers,
            PaneSnapshotCensusKind::StackMember => &mut self.request.stack_members,
            PaneSnapshotCensusKind::FloatingPane => &mut self.request.floating_panes,
            PaneSnapshotCensusKind::ZoomCarrier => &mut self.request.zoom_carriers,
            PaneSnapshotCensusKind::IdentityCheck => &mut self.request.identity_checks,
            PaneSnapshotCensusKind::PaneCallback => &mut self.request.pane_callbacks,
            PaneSnapshotCensusKind::AssemblyNode => &mut self.request.assembly_nodes,
        };
        *request = request
            .checked_add(count)
            .context("pane-snapshot request category overflow")?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallbackFreePaneOwner {
    TreeLeaf(usize),
    HiddenStack,
    Floating,
}

fn admit_ordered_pane_census_work(
    work: &mut usize,
    max_census_work: usize,
    tab_id: TabId,
    ledger: &mut PaneSnapshotCensusLedger,
    kind: PaneSnapshotCensusKind,
) -> anyhow::Result<()> {
    let next = work
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("tab {tab_id} ordered pane census work overflows usize"))?;
    if next > max_census_work {
        anyhow::bail!("tab {tab_id} ordered pane census exceeds {max_census_work} carrier entries");
    }
    ledger.reserve(kind, 1)?;
    *work = next;
    Ok(())
}

fn push_bounded_callback_free_pane(
    owners: &mut HashMap<PaneIdentity, CallbackFreePaneOwner>,
    panes: &mut Vec<Arc<dyn Pane>>,
    pane: &Arc<dyn Pane>,
    owner: CallbackFreePaneOwner,
    max_census_panes: usize,
    tab_id: TabId,
    ledger: &mut PaneSnapshotCensusLedger,
) -> anyhow::Result<Option<CallbackFreePaneOwner>> {
    ledger.reserve(PaneSnapshotCensusKind::IdentityCheck, 1)?;
    let identity = pane_identity(pane);
    if let Some(prior) = owners.get(&identity).copied() {
        return Ok(Some(prior));
    }
    let next_count = panes
        .len()
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("tab {tab_id} ordered pane census overflows usize"))?;
    if next_count > max_census_panes {
        anyhow::bail!(
            "tab {tab_id} ordered pane census has more than {max_census_panes} exact pane identities"
        );
    }
    owners
        .try_reserve(1)
        .map_err(|error| anyhow::anyhow!("reserve ordered pane census owners: {error}"))?;
    reserve_pane_arena_stack_push(panes, 1, max_census_panes, "ordered pane census entries")?;
    let prior = owners.insert(identity, owner);
    debug_assert!(
        prior.is_none(),
        "ordered pane census identity changed under tab lock"
    );
    panes.push(Arc::clone(pane));
    Ok(None)
}

fn callback_snapshot_matches(
    current: &[Arc<dyn Pane>],
    observed: &HashMap<PaneIdentity, PaneId>,
) -> anyhow::Result<bool> {
    if current.len() != observed.len() {
        return Ok(false);
    }
    let mut current_identities = HashSet::new();
    current_identities
        .try_reserve(current.len())
        .map_err(|error| anyhow::anyhow!("reserve callback-coherence pane identities: {error}"))?;
    Ok(current.iter().all(|pane| {
        let identity = pane_identity(pane);
        current_identities.insert(identity) && observed.contains_key(&identity)
    }))
}

fn callback_snapshot_matches_bounded(
    current: &[Arc<dyn Pane>],
    observed: &HashMap<PaneIdentity, PaneId>,
    ledger: &mut PaneSnapshotCensusLedger,
) -> anyhow::Result<bool> {
    if current.len() != observed.len() {
        return Ok(false);
    }
    let identity_work = current
        .len()
        .checked_mul(2)
        .context("callback-coherence identity work overflow")?;
    ledger.reserve(PaneSnapshotCensusKind::IdentityCheck, identity_work)?;
    callback_snapshot_matches(current, observed)
}

#[derive(Clone)]
struct ObservedPane {
    pane: Arc<dyn Pane>,
    pane_id: Option<PaneId>,
}

struct BoundedCallbackFreePaneCensus {
    panes: Vec<Arc<dyn Pane>>,
    tree_leaf_count: usize,
    tree_active: Arc<dyn Pane>,
    coherence: OrderedPaneCoherence,
    stats: PaneSnapshotCensusStats,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum OrderedPaneTreeCoherenceNode {
    Empty,
    Split(SplitDirectionAndSize),
    Leaf(PaneIdentity),
}

#[derive(Debug, Eq, PartialEq)]
struct OrderedPaneStackCoherence {
    active_index: usize,
    members: Vec<PaneIdentity>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct OrderedFloatingPaneCoherence {
    identity: PaneIdentity,
    rect: FloatingPaneRect,
    z_order: u32,
    visible: bool,
    pinned: bool,
    opacity_bits: u32,
}

#[derive(Debug, Eq, PartialEq)]
struct OrderedPaneCoherence {
    tree: Vec<OrderedPaneTreeCoherenceNode>,
    active: usize,
    stacks: HashMap<usize, OrderedPaneStackCoherence>,
    floating: Vec<OrderedFloatingPaneCoherence>,
    floating_focus: Option<PaneId>,
    zoomed: Option<PaneIdentity>,
    title: Arc<str>,
}

struct OrderedPaneEntryObservation {
    pane_id: PaneId,
    title: String,
    size: TerminalSize,
    working_dir: Option<SerdeUrl>,
    alt_screen_active: bool,
    cursor_pos: StableCursorPosition,
    physical_top: StableRowIndex,
    workspace: String,
    tty_name: Option<String>,
}

struct OrderedPaneObservation {
    pane_ids: HashMap<PaneIdentity, PaneId>,
    tree_entries: HashMap<PaneIdentity, OrderedPaneEntryObservation>,
}

fn observe_ordered_panes_bounded(
    tab_id: TabId,
    panes: Vec<Arc<dyn Pane>>,
    tree_leaf_count: usize,
    workspace: &str,
    census_ledger: &mut PaneSnapshotCensusLedger,
    metadata_ledger: &mut PaneSnapshotMetadataLedger,
) -> anyhow::Result<OrderedPaneObservation> {
    if tree_leaf_count > panes.len() {
        anyhow::bail!(
            "tab {tab_id} ordered pane observation expects {tree_leaf_count} tree leaves from {} exact panes",
            panes.len()
        );
    }

    let identity_work = panes
        .len()
        .checked_mul(2)
        .and_then(|work| work.checked_add(tree_leaf_count))
        .context("ordered pane-observation identity work overflow")?;
    census_ledger.reserve(PaneSnapshotCensusKind::IdentityCheck, identity_work)?;
    let mut pane_ids = HashMap::new();
    pane_ids
        .try_reserve(panes.len())
        .map_err(|error| anyhow::anyhow!("reserve ordered pane-id observations: {error}"))?;
    let mut pane_id_owners = HashMap::new();
    pane_id_owners
        .try_reserve(panes.len())
        .map_err(|error| anyhow::anyhow!("reserve ordered numeric pane-id owners: {error}"))?;
    let mut tree_entries = HashMap::new();
    tree_entries
        .try_reserve(tree_leaf_count)
        .map_err(|error| anyhow::anyhow!("reserve ordered tree-pane observations: {error}"))?;

    for (pane_index, pane) in panes.into_iter().enumerate() {
        let identity = pane_identity(&pane);
        census_ledger.preflight_pane_callbacks(if pane_index < tree_leaf_count { 7 } else { 1 })?;
        let workspace = if pane_index < tree_leaf_count {
            // Workspace is already borrowed from the window. Admit the exact
            // per-leaf copy before invoking arbitrary pane code so exhaustion
            // cannot trigger even the first metadata callback for this entry.
            metadata_ledger
                .preflight_required_string(PaneSnapshotMetadataField::PaneWorkspace, workspace)?;
            let workspace = workspace.to_string();
            metadata_ledger.admit_required_owned(
                PaneSnapshotMetadataField::PaneWorkspace,
                &workspace,
                workspace.capacity(),
            )?;
            Some(workspace)
        } else {
            None
        };
        let pane_id =
            observe_snapshot_pane_callback(tab_id, &pane, census_ledger, |pane| pane.pane_id())?;
        let tree_entry = if pane_index < tree_leaf_count {
            // Admit each owned UTF-8 result immediately. No subsequent getter
            // runs after a field or cumulative budget rejection.
            metadata_ledger.preflight_required_string(PaneSnapshotMetadataField::PaneTitle, "")?;
            let title = observe_snapshot_pane_callback(tab_id, &pane, census_ledger, |pane| {
                pane.get_title()
            })?;
            metadata_ledger.admit_required_owned(
                PaneSnapshotMetadataField::PaneTitle,
                &title,
                title.capacity(),
            )?;
            let dims = observe_snapshot_pane_callback(tab_id, &pane, census_ledger, |pane| {
                pane.get_dimensions()
            })?;
            metadata_ledger.preflight_optional_value(PaneSnapshotMetadataField::PaneWorkingDir)?;
            let working_dir =
                observe_snapshot_pane_callback(tab_id, &pane, census_ledger, |pane| {
                    pane.get_current_working_dir(CachePolicy::AllowStale)
                })?;
            if let Some(url) = working_dir.as_ref() {
                metadata_ledger.preflight_optional_string(
                    PaneSnapshotMetadataField::PaneWorkingDir,
                    url.as_str(),
                )?;
            }
            let working_dir = working_dir.map(SerdeUrl::from);
            match working_dir.as_ref() {
                Some(url) => metadata_ledger.admit_optional_owned(
                    PaneSnapshotMetadataField::PaneWorkingDir,
                    url.as_str(),
                    url.capacity(),
                )?,
                None => metadata_ledger
                    .admit_optional_none(PaneSnapshotMetadataField::PaneWorkingDir)?,
            }
            let alt_screen_active =
                observe_snapshot_pane_callback(tab_id, &pane, census_ledger, |pane| {
                    pane.is_alt_screen_active()
                })?;
            let cursor_pos =
                observe_snapshot_pane_callback(tab_id, &pane, census_ledger, |pane| {
                    pane.get_cursor_position()
                })?;
            metadata_ledger.preflight_optional_value(PaneSnapshotMetadataField::PaneTtyName)?;
            let tty_name = observe_snapshot_pane_callback(tab_id, &pane, census_ledger, |pane| {
                pane.tty_name()
            })?;
            match tty_name.as_ref() {
                Some(tty_name) => metadata_ledger.admit_optional_owned(
                    PaneSnapshotMetadataField::PaneTtyName,
                    tty_name,
                    tty_name.capacity(),
                )?,
                None => {
                    metadata_ledger.admit_optional_none(PaneSnapshotMetadataField::PaneTtyName)?
                }
            }
            Some(OrderedPaneEntryObservation {
                pane_id,
                title,
                size: TerminalSize {
                    cols: dims.cols,
                    rows: dims.viewport_rows,
                    pixel_height: dims.pixel_height,
                    pixel_width: dims.pixel_width,
                    dpi: dims.dpi,
                },
                working_dir,
                alt_screen_active,
                cursor_pos,
                physical_top: dims.physical_top,
                workspace: workspace.expect("tree pane workspace was prepared before callbacks"),
                tty_name,
            })
        } else {
            None
        };
        if pane_ids.insert(identity, pane_id).is_some() {
            anyhow::bail!(
                "an exact pane identity appears more than once while tab {tab_id} is being observed for ordered encoding"
            );
        }
        if let Some(prior_identity) = pane_id_owners.insert(pane_id, identity) {
            if prior_identity != identity {
                anyhow::bail!(
                    "pane id {pane_id} belongs to more than one exact pane identity while tab {tab_id} is being observed for ordered encoding"
                );
            }
        }

        let Some(tree_entry) = tree_entry else {
            continue;
        };
        if tree_entries.insert(identity, tree_entry).is_some() {
            anyhow::bail!(
                "an exact tree-pane identity appears more than once while tab {tab_id} is being observed for ordered encoding"
            );
        }
    }

    Ok(OrderedPaneObservation {
        pane_ids,
        tree_entries,
    })
}

fn observe_snapshot_pane_callback<T>(
    tab_id: TabId,
    pane: &Arc<dyn Pane>,
    ledger: &mut PaneSnapshotCensusLedger,
    callback: impl FnOnce(&Arc<dyn Pane>) -> T,
) -> anyhow::Result<T> {
    ledger.reserve(PaneSnapshotCensusKind::PaneCallback, 1)?;
    catch_recoverable(
        RecoverablePanicSite::MuxPaneCallback,
        AssertUnwindSafe(|| callback(pane)),
    )
    .map_err(|_| {
        anyhow::anyhow!(
            "a pane callback panicked while tab {tab_id} was being observed for ordered encoding"
        )
    })
}

fn build_callback_pane_id_snapshot(
    tab_id: TabId,
    observed: &[ObservedPane],
) -> anyhow::Result<HashMap<PaneIdentity, PaneId>> {
    let mut pane_ids = HashMap::new();
    pane_ids
        .try_reserve(observed.len())
        .map_err(|error| anyhow::anyhow!("reserve pane identity snapshot: {error}"))?;
    let mut pane_id_owners = HashMap::new();
    pane_id_owners
        .try_reserve(observed.len())
        .map_err(|error| anyhow::anyhow!("reserve numeric pane-id snapshot: {error}"))?;

    for pane in observed {
        let pane_id = pane.pane_id.ok_or_else(|| {
            anyhow::anyhow!("a pane callback panicked while tab {tab_id} was being encoded")
        })?;
        let identity = pane_identity(&pane.pane);
        if pane_ids.insert(identity, pane_id).is_some() {
            anyhow::bail!(
                "an exact pane identity appears more than once while tab {tab_id} is being encoded"
            );
        }
        if let Some(prior_identity) = pane_id_owners.insert(pane_id, identity) {
            if prior_identity != identity {
                anyhow::bail!(
                    "pane id {pane_id} belongs to more than one exact pane identity while tab {tab_id} is being encoded"
                );
            }
        }
    }

    Ok(pane_ids)
}

#[derive(Clone)]
struct ExactPaneRemovalCandidate {
    pane: Arc<dyn Pane>,
    pane_id: PaneId,
    expected_registration: Option<PaneRegistrationHandle>,
    expected_lane: Option<PaneStructuralLane>,
}

struct PreparedFloatingPaneAddition {
    replacement: Vec<FloatingPane>,
    floating_focus: Option<PaneId>,
    positioned: PositionedFloatingPane,
    callbacks: DeferredTabCallbacks,
}

struct PreparedExactPaneRemoval {
    replacement: TabInner,
    callbacks: DeferredTabCallbacks,
}

struct PreparedMovedSplitTab {
    baseline: TabInner,
    replacement: TabInner,
    current: Vec<ExactPaneStructuralState>,
    desired: Vec<RelocatedPaneStructuralState>,
    callbacks: DeferredTabCallbacks,
}

struct PreparedMovedSplit {
    source: Option<PreparedMovedSplitTab>,
    target: PreparedMovedSplitTab,
    source_size: TerminalSize,
    source_tab_retires: bool,
}

/// Complete off-topology successor for moving one admitted pane into a new
/// tab.  The destination tab is mux-bound but remains absent from every mux
/// registry and window until the outer transaction publishes it.
pub(crate) struct PreparedGuardedMoveToNewTab {
    source_tab: Arc<Tab>,
    destination_tab: Arc<Tab>,
    source_baseline: TabInner,
    source_replacement: TabInner,
    destination_baseline: TabInner,
    destination_replacement: TabInner,
    authority_replacements: Option<Vec<StructuralRelocationTabReplacement>>,
    source_callbacks: DeferredTabCallbacks,
    destination_callbacks: DeferredTabCallbacks,
    destination_size: TerminalSize,
    source_tab_retires: bool,
    topology_notification_count: Option<usize>,
}

/// Retained exact tab locks for the callback-free publication suffix.
///
/// The guards borrow the caller's stable `Arc<Tab>` handles rather than the
/// prepared value, allowing the outer mux transaction to consume both values
/// together after authority and window preflight succeeds.
pub(crate) struct LockedGuardedMoveToNewTab<'tabs> {
    first_inner: MutexGuard<'tabs, TabInner>,
    second_inner: MutexGuard<'tabs, TabInner>,
    source_is_first: bool,
}

/// Callback and retired-state payload released only after the outer mux cut
/// has dropped every registry, window, authority, and tab lock.
pub(crate) struct CommittedGuardedMoveToNewTab {
    source_callbacks: DeferredTabCallbacks,
    destination_callbacks: DeferredTabCallbacks,
    retired_source_inner: TabInner,
    retired_destination_inner: TabInner,
}

impl CommittedGuardedMoveToNewTab {
    pub(crate) fn execute(self, mux: &Mux) {
        let Self {
            source_callbacks,
            destination_callbacks,
            retired_source_inner,
            retired_destination_inner,
        } = self;
        drop(retired_source_inner);
        drop(retired_destination_inner);
        source_callbacks.execute(Some(mux));
        destination_callbacks.execute(Some(mux));
    }
}

/// The codec crate caps ordered pane trees at this depth, but the dependency-
/// lower mux crate cannot import that constant. Mirror the wire-authoritative
/// ceiling here so guarded relocation never enters derived recursive
/// `Tree::clone` on an adversarially skewed live topology.
const MAX_MOVED_SPLIT_TREE_DEPTH: usize = 64;

#[derive(Default)]
struct DeferredTabCallbacks {
    changed: bool,
    removed: HashSet<PaneIdentity>,
    zoom_work: Vec<(Arc<dyn Pane>, bool)>,
    resize_work: Vec<(Arc<dyn Pane>, TerminalSize)>,
    prior_focus: Option<Arc<dyn Pane>>,
    current_focus: Option<Arc<dyn Pane>>,
    current_focus_id: Option<PaneId>,
    topology_notifications: Vec<MuxNotificationEnvelope>,
}

impl DeferredTabCallbacks {
    fn focus_changed(&self) -> bool {
        match (&self.prior_focus, &self.current_focus) {
            (Some(prior), Some(current)) => !Arc::ptr_eq(prior, current),
            (None, None) => false,
            (Some(_), None) | (None, Some(_)) => true,
        }
    }

    fn topology_notification_count(&self) -> usize {
        usize::from(self.changed)
            .saturating_add(usize::from(
                self.changed && self.focus_changed() && self.current_focus_id.is_some(),
            ))
    }

    /// Allocate the complete notification payload before a relocation takes
    /// any mux or tab lock. Revisions are stamped later by the single topology
    /// cut that also publishes the successor tab state.
    fn reserve_relocation_topology_notifications(&mut self) -> anyhow::Result<usize> {
        let count = self.topology_notification_count();
        self.topology_notifications
            .try_reserve_exact(count)
            .map_err(|error| anyhow::anyhow!("reserve relocation topology notifications: {error}"))?;
        Ok(count)
    }

    /// Install already-reserved notification envelopes without allocation.
    /// `first_revision` belongs to the same authority cut that publishes the
    /// matching [`TabInner`] successor.
    fn stamp_relocation_topology_notifications(
        &mut self,
        tab_id: TabId,
        first_revision: Option<crate::TopologyRevision>,
        offset: usize,
    ) -> usize {
        let count = self.topology_notification_count();
        if count == 0 {
            return offset;
        }
        let first_revision = first_revision
            .expect("a changed relocation tab must own reserved topology authority");
        let revision_offset = u64::try_from(offset)
            .expect("relocation topology notification offset must fit u64");
        let tab_revision = crate::TopologyRevision::new(
            first_revision
                .get()
                .checked_add(revision_offset)
                .expect("reserved relocation topology range cannot overflow"),
        );
        self.topology_notifications.push(MuxNotificationEnvelope {
            notification: MuxNotification::TabResized(tab_id),
            topology: crate::MuxTopologyStamp::Revision(tab_revision),
        });
        let mut consumed = 1usize;
        if self.focus_changed() {
            if let Some(pane_id) = self.current_focus_id {
                let revision = crate::TopologyRevision::new(
                    first_revision
                        .get()
                        .checked_add(revision_offset)
                        .and_then(|revision| {
                            revision.checked_add(
                                u64::try_from(consumed)
                                    .expect("relocation notification count must fit u64"),
                            )
                        })
                        .expect("reserved relocation topology range cannot overflow"),
                );
                self.topology_notifications.push(MuxNotificationEnvelope {
                    notification: MuxNotification::PaneFocused(pane_id),
                    topology: crate::MuxTopologyStamp::Revision(revision),
                });
                consumed = consumed.saturating_add(1);
            }
        }
        debug_assert_eq!(consumed, count);
        offset.saturating_add(count)
    }

    /// Reserve topology revisions before the tab lock that protects the
    /// structural mutation is released. Subscriber callbacks remain deferred,
    /// but a coherent snapshot can no longer observe the new tree or focus
    /// under an older revision.
    fn reserve_topology_notifications(&mut self, mux: &Mux, tab_id: TabId) {
        if !self.changed {
            return;
        }
        self.topology_notifications
            .push(mux.envelope_notification(MuxNotification::TabResized(tab_id)));
        if self.focus_changed() {
            if let Some(pane_id) = self.current_focus_id {
                self.topology_notifications
                    .push(mux.envelope_notification(MuxNotification::PaneFocused(pane_id)));
            }
        }
    }

    /// Build and reserve the exact topology notification sequence before the
    /// associated tab mutation is made visible. The returned vector has all
    /// storage allocated before the mux revision is advanced, so installing it
    /// into the callback bundle is an infallible commit step.
    fn prepare_topology_notifications(
        &self,
        mux: &Mux,
        tab_id: TabId,
    ) -> anyhow::Result<Vec<MuxNotificationEnvelope>> {
        let focused_pane_id = self
            .focus_changed()
            .then_some(self.current_focus_id)
            .flatten();
        let focus_notification_count = usize::from(focused_pane_id.is_some());
        let notification_count = usize::from(self.changed)
            .checked_add(focus_notification_count)
            .ok_or_else(|| anyhow::anyhow!("tab topology notification count overflow"))?;
        let mut notifications = Vec::new();
        notifications
            .try_reserve_exact(notification_count)
            .map_err(|error| anyhow::anyhow!("reserve tab topology notifications: {error}"))?;
        if notification_count == 0 {
            return Ok(notifications);
        }

        let mut topology = mux.topology.lock();
        let first_revision = topology
            .reserve_revisions(notification_count)
            .map_err(anyhow::Error::new)?;
        notifications.push(MuxNotificationEnvelope {
            notification: MuxNotification::TabResized(tab_id),
            topology: crate::MuxTopologyStamp::Revision(first_revision),
        });
        if let Some(pane_id) = focused_pane_id {
            let revision = crate::TopologyRevision::new(
                first_revision.get().saturating_add(1),
            );
            notifications.push(MuxNotificationEnvelope {
                notification: MuxNotification::PaneFocused(pane_id),
                topology: crate::MuxTopologyStamp::Revision(revision),
            });
        }
        Ok(notifications)
    }

    fn execute(self, mux: Option<&Mux>) {
        let DeferredTabCallbacks {
            zoom_work,
            resize_work,
            prior_focus,
            current_focus,
            topology_notifications,
            ..
        } = self;
        for (pane, zoomed) in zoom_work {
            if catch_recoverable(
                RecoverablePanicSite::MuxPaneCallback,
                AssertUnwindSafe(|| pane.set_zoomed(zoomed)),
            )
            .is_err()
            {
                log::error!(
                    "pane zoom callback panicked for exact pane identity {:p}",
                    Arc::as_ptr(&pane)
                );
            }
        }
        execute_pane_resize_work(resize_work);

        let focus_changed = match (&prior_focus, &current_focus) {
            (Some(prior), Some(current)) => !Arc::ptr_eq(prior, current),
            (None, None) => false,
            (Some(_), None) | (None, Some(_)) => true,
        };
        if focus_changed {
            if let Some(prior) = prior_focus {
                if catch_recoverable(
                    RecoverablePanicSite::MuxPaneCallback,
                    AssertUnwindSafe(|| prior.focus_changed(false)),
                )
                .is_err()
                {
                    log::error!(
                        "pane focus-loss callback panicked for exact pane identity {:p}",
                        Arc::as_ptr(&prior)
                    );
                }
            }
            if let Some(current) = current_focus {
                if catch_recoverable(
                    RecoverablePanicSite::MuxPaneCallback,
                    AssertUnwindSafe(|| current.focus_changed(true)),
                )
                .is_err()
                {
                    log::error!(
                        "pane focus-gain callback panicked for exact pane identity {:p}",
                        Arc::as_ptr(&current)
                    );
                }
            }
        }

        if let Some(mux) = mux {
            for envelope in topology_notifications {
                mux.dispatch_notification_envelope(envelope);
            }
        } else {
            debug_assert!(
                topology_notifications.is_empty(),
                "ownerless deferred tab callbacks must not retain topology authority"
            );
        }
    }
}

fn observe_relocation_pane_ids(
    tab_id: TabId,
    inners: &[&TabInner],
) -> anyhow::Result<HashMap<PaneIdentity, PaneId>> {
    let pane_count = inners.iter().try_fold(0usize, |count, inner| {
        count
            .checked_add(inner.snapshot_panes_callback_free().len())
            .ok_or_else(|| anyhow::anyhow!("relocation pane census overflow"))
    })?;
    let mut pane_ids = HashMap::new();
    pane_ids
        .try_reserve(pane_count)
        .map_err(|error| anyhow::anyhow!("reserve relocation pane identities: {error}"))?;
    let mut pane_id_owners = HashMap::new();
    pane_id_owners
        .try_reserve(pane_count)
        .map_err(|error| anyhow::anyhow!("reserve relocation numeric pane ids: {error}"))?;

    for inner in inners {
        for pane in inner.snapshot_panes_callback_free() {
            let identity = pane_identity(&pane);
            let pane_id = observe_pane_id_for_mutation(&pane).with_context(|| {
                format!("observe exact pane identity while preparing tab {tab_id} relocation")
            })?;
            anyhow::ensure!(
                pane_ids.insert(identity, pane_id).is_none(),
                "one exact pane allocation appears more than once across relocation tabs"
            );
            anyhow::ensure!(
                pane_id_owners.insert(pane_id, identity).is_none(),
                "pane id {pane_id} belongs to more than one exact relocation allocation"
            );
        }
    }
    Ok(pane_ids)
}

fn observed_relocation_panes(
    inner: &TabInner,
    pane_ids: &HashMap<PaneIdentity, PaneId>,
) -> anyhow::Result<Vec<ObservedPane>> {
    let panes = inner.snapshot_panes_callback_free();
    let mut observed = Vec::new();
    observed
        .try_reserve_exact(panes.len())
        .map_err(|error| anyhow::anyhow!("reserve observed relocation panes: {error}"))?;
    for pane in panes {
        let pane_id = pane_ids
            .get(&pane_identity(&pane))
            .copied()
            .ok_or_else(|| anyhow::anyhow!("relocation pane lacks its observed numeric id"))?;
        observed.push(ObservedPane {
            pane,
            pane_id: Some(pane_id),
        });
    }
    Ok(observed)
}

fn exact_tiled_relocation_index(inner: &TabInner, pane: &Arc<dyn Pane>) -> Option<usize> {
    let mut leaves = Vec::new();
    collect_raw_tree_leaves(inner.pane.as_ref()?, &mut leaves);
    leaves.iter().position(|candidate| Arc::ptr_eq(candidate, pane))
}

fn exact_relocation_structural_state(
    inner: &TabInner,
    pane_ids: &HashMap<PaneIdentity, PaneId>,
) -> anyhow::Result<Vec<ExactPaneStructuralState>> {
    let (tiled, floating) = inner.snapshot_structural_panes_callback_free_checked()?;
    let count = tiled
        .len()
        .checked_add(floating.len())
        .ok_or_else(|| anyhow::anyhow!("relocation structural owner count overflow"))?;
    let mut states = Vec::new();
    states
        .try_reserve_exact(count)
        .map_err(|error| anyhow::anyhow!("reserve relocation structural owners: {error}"))?;
    for pane in tiled {
        let pane_id = pane_ids
            .get(&pane_identity(&pane))
            .copied()
            .ok_or_else(|| anyhow::anyhow!("tiled relocation pane lacks its observed identity"))?;
        states.push(ExactPaneStructuralState {
            pane_id,
            pane,
            lane: PaneStructuralLane::Tiled,
        });
    }
    for (retained_pane_id, pane) in floating {
        let pane_id = pane_ids
            .get(&pane_identity(&pane))
            .copied()
            .ok_or_else(|| anyhow::anyhow!("floating relocation pane lacks its observed identity"))?;
        anyhow::ensure!(
            retained_pane_id == pane_id,
            "floating relocation pane retained id {retained_pane_id}, observed {pane_id}"
        );
        states.push(ExactPaneStructuralState {
            pane_id,
            pane,
            lane: PaneStructuralLane::Floating,
        });
    }
    Ok(states)
}

fn desired_relocation_structural_state(
    inner: &TabInner,
    pane_ids: &HashMap<PaneIdentity, PaneId>,
) -> anyhow::Result<Vec<RelocatedPaneStructuralState>> {
    let exact = exact_relocation_structural_state(inner, pane_ids)?;
    let mut desired = Vec::new();
    desired
        .try_reserve_exact(exact.len())
        .map_err(|error| anyhow::anyhow!("reserve desired relocation owners: {error}"))?;
    desired.extend(exact.into_iter().map(|state| RelocatedPaneStructuralState {
            pane_id: state.pane_id,
            pane: state.pane,
            lane: state.lane,
            registration: None,
            domain_id: None,
        }));
    Ok(desired)
}

fn populate_relocation_live_metadata(
    mux: &Mux,
    desired: &mut [RelocatedPaneStructuralState],
) -> anyhow::Result<()> {
    let panes = mux.panes.read();
    for state in desired {
        match panes.get(&state.pane_id) {
            Some(registered) => {
                anyhow::ensure!(
                    Arc::ptr_eq(&registered.pane, &state.pane),
                    "pane {} relocation registry slot names another exact allocation",
                    state.pane_id
                );
                state.registration = Some(PaneRegistrationHandle::new(
                    &registered.pane,
                    &registered.generation,
                ));
                state.domain_id = Some(registered.domain_id);
            }
            None => {
                state.registration = None;
                state.domain_id = None;
            }
        }
    }
    Ok(())
}

fn relocation_tree_matches(current: &Tree, baseline: &Tree) -> anyhow::Result<bool> {
    let mut pending = Vec::new();
    pending
        .try_reserve(1)
        .map_err(|error| anyhow::anyhow!("reserve relocation tree comparison: {error}"))?;
    pending.push((current, baseline));
    while let Some((current, baseline)) = pending.pop() {
        match (current, baseline) {
            (Tree::Empty, Tree::Empty) => {}
            (Tree::Leaf(current), Tree::Leaf(baseline)) if Arc::ptr_eq(current, baseline) => {}
            (
                Tree::Node {
                    left: current_left,
                    right: current_right,
                    data: current_data,
                },
                Tree::Node {
                    left: baseline_left,
                    right: baseline_right,
                    data: baseline_data,
                },
            ) if current_data == baseline_data => {
                pending.try_reserve(2).map_err(|error| {
                    anyhow::anyhow!("grow relocation tree comparison: {error}")
                })?;
                pending.push((current_right, baseline_right));
                pending.push((current_left, baseline_left));
            }
            _ => return Ok(false),
        }
    }
    Ok(true)
}

fn admit_moved_split_tree_clone(inner: &TabInner) -> anyhow::Result<()> {
    let Some(tree) = inner.pane.as_ref() else {
        return Ok(());
    };
    let mut pending = Vec::new();
    pending
        .try_reserve_exact(MAX_MOVED_SPLIT_TREE_DEPTH.saturating_add(1))
        .map_err(|error| anyhow::anyhow!("reserve moved-split tree-depth preflight: {error}"))?;
    pending.push((tree, 1usize));
    while let Some((tree, depth)) = pending.pop() {
        anyhow::ensure!(
            depth <= MAX_MOVED_SPLIT_TREE_DEPTH,
            "moved-split pane tree depth {depth} exceeds limit {MAX_MOVED_SPLIT_TREE_DEPTH}"
        );
        if let Tree::Node { left, right, .. } = tree {
            let next_depth = depth
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("moved-split pane tree depth overflow"))?;
            pending
                .try_reserve(2)
                .map_err(|error| anyhow::anyhow!("grow moved-split depth preflight: {error}"))?;
            pending.push((right, next_depth));
            pending.push((left, next_depth));
        }
    }
    Ok(())
}

fn relocation_stack_matches(current: &PaneStack, baseline: &PaneStack) -> bool {
    current.active_index() == baseline.active_index()
        && current.panes().len() == baseline.panes().len()
        && current
            .panes()
            .iter()
            .zip(baseline.panes())
            .all(|(current, baseline)| Arc::ptr_eq(current, baseline))
}

/// Reject an off-lock successor if any callback-free tab state changed after
/// its baseline clone. This is deliberately stronger than structural-owner
/// equality: replacing a tab after a concurrent resize, stack activation,
/// floating reorder, zoom, or focus mutation would otherwise erase that work.
fn relocation_inner_matches_baseline(
    current: &TabInner,
    baseline: &TabInner,
) -> anyhow::Result<bool> {
    let tree_matches = match (&current.pane, &baseline.pane) {
        (Some(current), Some(baseline)) => relocation_tree_matches(current, baseline)?,
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    };
    let floating_matches = current.floating_panes.len() == baseline.floating_panes.len()
        && current
            .floating_panes
            .iter()
            .zip(&baseline.floating_panes)
            .all(|(current, baseline)| {
                Arc::ptr_eq(&current.pane, &baseline.pane)
                    && current.pane_id == baseline.pane_id
                    && current.rect == baseline.rect
                    && current.z_order == baseline.z_order
                    && current.visible == baseline.visible
                    && current.pinned == baseline.pinned
                    && current.opacity.to_bits() == baseline.opacity.to_bits()
            });
    let stacks_match = current.pane_stacks.len() == baseline.pane_stacks.len()
        && current.pane_stacks.iter().all(|(slot, current)| {
            baseline
                .pane_stacks
                .get(slot)
                .is_some_and(|baseline| relocation_stack_matches(current, baseline))
        });
    let layout_cycle_matches = match (&current.layout_cycle, &baseline.layout_cycle) {
        (Some(current), Some(baseline)) => {
            current.current_index() == baseline.current_index()
                && current.layouts() == baseline.layouts()
        }
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    };
    let zoom_matches = match (&current.zoomed, &baseline.zoomed) {
        (Some(current), Some(baseline)) => Arc::ptr_eq(current, baseline),
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    };

    Ok(current.id == baseline.id
        && Weak::ptr_eq(&current.mux_owner, &baseline.mux_owner)
        && current.mux_owner_bound == baseline.mux_owner_bound
        && current.mux_owner_active == baseline.mux_owner_active
        && current.mux_owner_generation == baseline.mux_owner_generation
        && tree_matches
        && floating_matches
        && current.floating_focus == baseline.floating_focus
        && current.size == baseline.size
        && current.size_before_zoom == baseline.size_before_zoom
        && current.active == baseline.active
        && zoom_matches
        && current.title == baseline.title
        && current.recency.count == baseline.recency.count
        && current.recency.by_idx == baseline.recency.by_idx
        && current.collapsed_panes == baseline.collapsed_panes
        && layout_cycle_matches
        && stacks_match
        && current.constraint_overrides == baseline.constraint_overrides)
}

fn merge_same_tab_relocation_callbacks(
    mut removal: DeferredTabCallbacks,
    mut insertion: DeferredTabCallbacks,
) -> anyhow::Result<DeferredTabCallbacks> {
    removal.zoom_work.append(&mut insertion.zoom_work);
    removal.resize_work.append(&mut insertion.resize_work);
    let mut work_by_identity = HashMap::new();
    work_by_identity
        .try_reserve(removal.resize_work.len())
        .map_err(|error| anyhow::anyhow!("reserve moved-split resize coalescing: {error}"))?;
    let mut coalesced = Vec::new();
    coalesced
        .try_reserve_exact(removal.resize_work.len())
        .map_err(|error| anyhow::anyhow!("reserve moved-split resize callbacks: {error}"))?;
    for (pane, size) in removal.resize_work.drain(..) {
        let identity = pane_identity(&pane);
        if let Some(index) = work_by_identity.get(&identity).copied() {
            coalesced[index] = (pane, size);
        } else {
            work_by_identity.insert(identity, coalesced.len());
            coalesced.push((pane, size));
        }
    }
    removal.resize_work = coalesced;
    removal.removed.extend(insertion.removed);
    removal.current_focus = insertion.current_focus;
    removal.current_focus_id = insertion.current_focus_id;
    removal.changed |= insertion.changed;
    Ok(removal)
}

fn collect_raw_tree_panes(tree: &Tree, panes: &mut Vec<Arc<dyn Pane>>) {
    let mut pending = vec![tree];
    while let Some(tree) = pending.pop() {
        match tree {
            Tree::Empty => {}
            Tree::Node { left, right, .. } => {
                // Push right first to preserve the recursive left-to-right
                // leaf order without consuming the native call stack.
                pending.push(right);
                pending.push(left);
            }
            Tree::Leaf(pane) => panes.push(Arc::clone(pane)),
        }
    }
}

fn collect_raw_tree_leaves(tree: &Tree, panes: &mut Vec<Arc<dyn Pane>>) {
    collect_raw_tree_panes(tree, panes);
}

fn split_dimension_preserving_ratio(
    total_with_separator: usize,
    old_first: usize,
    old_second: usize,
) -> (usize, usize) {
    let available = total_with_separator.saturating_sub(1);
    if available == 0 {
        return (0, 0);
    }
    if old_first == 0 {
        return (0, available);
    }
    if old_second == 0 {
        return (available, 0);
    }

    let old_total = old_first.saturating_add(old_second);
    let proportional = if old_total == 0 {
        available / 2
    } else {
        let numerator = (available as u128).saturating_mul(old_first as u128);
        usize::try_from(numerator / old_total as u128).unwrap_or(available)
    };
    if available >= 2 {
        let first = proportional.clamp(1, available - 1);
        (first, available - first)
    } else {
        (
            proportional.min(available),
            available.saturating_sub(proportional),
        )
    }
}

fn terminal_size_for_cells(parent: TerminalSize, rows: usize, cols: usize) -> TerminalSize {
    let dims = cell_dimensions(&parent);
    TerminalSize {
        rows,
        cols,
        pixel_width: pixel_span(dims.pixel_width, cols),
        pixel_height: pixel_span(dims.pixel_height, rows),
        dpi: parent.dpi,
    }
}

/// Reassign split geometry without invoking pane constraint or resize callbacks.
///
/// Dead-pane removal can collapse an interior node while `Tab::inner` is held.
/// The surviving tree still needs coherent geometry, but consulting
/// `Pane::pane_constraints` there would re-enter arbitrary pane code. Preserve
/// each split's prior ratio instead and collect resize work for execution after
/// the tab lock is released.
fn normalize_tree_sizes_callback_free(
    tree: &mut Tree,
    size: TerminalSize,
    work: &mut Vec<(Arc<dyn Pane>, TerminalSize)>,
) {
    match tree {
        Tree::Empty => {}
        Tree::Leaf(pane) => work.push((Arc::clone(pane), size)),
        Tree::Node { left, right, data } => {
            let Some(split) = data.as_mut() else {
                normalize_tree_sizes_callback_free(left, size, work);
                normalize_tree_sizes_callback_free(right, size, work);
                return;
            };
            let (first, second) = match split.direction {
                SplitDirection::Horizontal => {
                    let (first_cols, second_cols) = split_dimension_preserving_ratio(
                        size.cols,
                        split.first.cols,
                        split.second.cols,
                    );
                    (
                        terminal_size_for_cells(size, size.rows, first_cols),
                        terminal_size_for_cells(size, size.rows, second_cols),
                    )
                }
                SplitDirection::Vertical => {
                    let (first_rows, second_rows) = split_dimension_preserving_ratio(
                        size.rows,
                        split.first.rows,
                        split.second.rows,
                    );
                    (
                        terminal_size_for_cells(size, first_rows, size.cols),
                        terminal_size_for_cells(size, second_rows, size.cols),
                    )
                }
            };
            split.first = first;
            split.second = second;
            normalize_tree_sizes_callback_free(left, first, work);
            normalize_tree_sizes_callback_free(right, second, work);
        }
    }
}

fn remove_exact_panes_from_tree(
    tree: Tree,
    removals: &HashSet<PaneIdentity>,
    replacements: &HashMap<PaneIdentity, Arc<dyn Pane>>,
    removed: &mut HashSet<PaneIdentity>,
) -> (Tree, bool) {
    match tree {
        Tree::Empty => (Tree::Empty, false),
        Tree::Leaf(pane) => {
            let identity = pane_identity(&pane);
            if !removals.contains(&identity) {
                return (Tree::Leaf(pane), false);
            }
            removed.insert(identity);
            if let Some(replacement) = replacements.get(&identity) {
                (Tree::Leaf(Arc::clone(replacement)), true)
            } else {
                (Tree::Empty, true)
            }
        }
        Tree::Node { left, right, data } => {
            let (left, left_changed) =
                remove_exact_panes_from_tree(*left, removals, replacements, removed);
            let (right, right_changed) =
                remove_exact_panes_from_tree(*right, removals, replacements, removed);
            let changed = left_changed || right_changed;
            match (left, right) {
                (Tree::Empty, Tree::Empty) => (Tree::Empty, changed),
                (Tree::Empty, survivor) | (survivor, Tree::Empty) => (survivor, true),
                (left, right) => (
                    Tree::Node {
                        left: Box::new(left),
                        right: Box::new(right),
                        data,
                    },
                    changed,
                ),
            }
        }
    }
}

#[derive(Clone)]
pub struct PositionedPane {
    /// The topological pane index that can be used to reference this pane
    pub index: usize,
    /// true if this is the active pane at the time the position was computed
    pub is_active: bool,
    /// true if this pane is zoomed
    pub is_zoomed: bool,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this pane, in cells.
    pub left: usize,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this pane, in cells.
    pub top: usize,
    /// The width of this pane in cells
    pub width: usize,
    pub pixel_width: usize,
    /// The height of this pane in cells
    pub height: usize,
    pub pixel_height: usize,
    /// The pane instance
    pub pane: Arc<dyn Pane>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct FloatingPaneRect {
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
}

/// Geometry admitted before an unpublished floating pane is spawned.
///
/// The pane is created and resized directly to this size. Final commit accepts
/// it only if the destination still clamps the requested rectangle to the same
/// exact geometry, avoiding a publish-then-correct resize cycle.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct PreparedFloatingPaneGeometry {
    rect: FloatingPaneRect,
    size: TerminalSize,
}

impl PreparedFloatingPaneGeometry {
    pub(crate) fn size(self) -> TerminalSize {
        self.size
    }
}

#[derive(Clone)]
struct FloatingPane {
    pane: Arc<dyn Pane>,
    /// Stable numeric identity admitted before the pane enters the tab lock.
    /// Keeping it with the topology prevents ordinary floating-pane lookups,
    /// focus reconciliation, and resize preparation from calling reentrant
    /// `Pane::pane_id` while `Tab::inner` is held.
    pane_id: PaneId,
    rect: FloatingPaneRect,
    z_order: u32,
    visible: bool,
    pinned: bool,
    opacity: f32,
}

fn next_floating_z_order_in_replacement(
    floating_panes: &mut [FloatingPane],
) -> anyhow::Result<u32> {
    let max = floating_panes
        .iter()
        .map(|floating| floating.z_order)
        .max()
        .unwrap_or(0);
    if max != u32::MAX {
        return Ok(max + 1);
    }

    let mut order = Vec::new();
    order
        .try_reserve_exact(floating_panes.len())
        .map_err(|error| anyhow::anyhow!("reserve floating z-order compaction: {error}"))?;
    order.extend(0..floating_panes.len());
    order.sort_by_key(|index| (floating_panes[*index].z_order, *index));
    for (lane, index) in order.into_iter().enumerate() {
        floating_panes[index].z_order = u32::try_from(lane).map_err(|_| {
            anyhow::anyhow!("floating pane count exceeds the representable z-order range")
        })?;
    }
    u32::try_from(floating_panes.len())
        .map_err(|_| anyhow::anyhow!("floating pane count exceeds the representable z-order range"))
}

fn positioned_floating_pane_with_focus(
    floating: &FloatingPane,
    floating_focus: Option<PaneId>,
) -> PositionedFloatingPane {
    PositionedFloatingPane {
        pane_id: floating.pane_id,
        is_focused: floating_focus == Some(floating.pane_id),
        left: floating.rect.left,
        top: floating.rect.top,
        width: floating.rect.width,
        height: floating.rect.height,
        z_order: floating.z_order,
        visible: floating.visible,
        pinned: floating.pinned,
        opacity: floating.opacity,
        pane: Arc::clone(&floating.pane),
    }
}

/// One authoritative floating-pane state supplied by a client-domain
/// snapshot reconciler.
///
/// `pane_id` is observed before entering the mux transaction and must match
/// the exact `pane` registration. The reconciler never invokes pane resize or
/// focus callbacks: this state mirrors a remote authority and must not echo
/// local mutations back to it.
#[derive(Clone)]
pub struct DomainFloatingPaneState {
    pub tab: Arc<Tab>,
    pub pane: Arc<dyn Pane>,
    pub pane_id: PaneId,
    pub rect: FloatingPaneRect,
    pub z_order: u32,
    pub visible: bool,
    pub pinned: bool,
    pub opacity: f32,
    pub focused: bool,
}

/// Result of one domain-selective floating-pane reconciliation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DomainFloatingPaneReconcileReceipt {
    pub changed_tab_ids: Vec<TabId>,
    pub invalidated_window_ids: Vec<WindowId>,
    pub registered_pane_ids: Vec<PaneId>,
    pub retired_pane_ids: Vec<PaneId>,
}

/// Wire-authoritative snapshots admit at most one floating overlay for each
/// entry under the codec's 16,384-pane floating ceiling. Keep this mux-side
/// admission guard independent of the codec crate so the mux does not acquire
/// a dependency cycle.
const MAX_DOMAIN_FLOATING_PANES_PER_RECONCILE: usize = 16_384;
/// An authoritative domain snapshot can contain the codec's complete tiled
/// leaf set plus its independent floating-pane set.
const MAX_DOMAIN_PANES_PER_RECONCILE: usize = MAX_DOMAIN_FLOATING_PANES_PER_RECONCILE * 2;

struct PreparedDomainPanePublication<'a> {
    pane: Arc<dyn Pane>,
    pane_id: PaneId,
    preparation_claim: crate::PanePreparationClaim<'a>,
    reader_start_gate: Option<crate::PaneReaderStartGate>,
    registration_reservation: Option<crate::pane_registration_handle::PaneRegistrationReservation>,
}

struct ObservedDomainFloatingTab {
    tab: Arc<Tab>,
    panes: Vec<Arc<dyn Pane>>,
    non_floating_panes: Vec<Arc<dyn Pane>>,
    tiled_tree: Option<Tree>,
    pane_stacks: HashMap<usize, PaneStack>,
    floating_panes: Vec<FloatingPane>,
    floating_focus: Option<PaneId>,
    zoomed_pane: Option<Arc<dyn Pane>>,
    size: TerminalSize,
    parent_window_id: Option<WindowId>,
}

struct PreparedDomainFloatingTab {
    replacement: Option<Vec<FloatingPane>>,
    floating_focus: Option<PaneId>,
    changed: bool,
}

fn floating_pane_state_eq(left: &FloatingPane, right: &FloatingPane) -> bool {
    Arc::ptr_eq(&left.pane, &right.pane)
        && left.pane_id == right.pane_id
        && left.rect == right.rect
        && left.z_order == right.z_order
        && left.visible == right.visible
        && left.pinned == right.pinned
        && left.opacity.to_bits() == right.opacity.to_bits()
}

fn floating_pane_vectors_eq(left: &[FloatingPane], right: &[FloatingPane]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| floating_pane_state_eq(left, right))
}

fn exact_tiled_tree_eq(left: &Tree, right: &Tree) -> bool {
    match (left, right) {
        (Tree::Empty, Tree::Empty) => true,
        (Tree::Leaf(left), Tree::Leaf(right)) => Arc::ptr_eq(left, right),
        (
            Tree::Node {
                left: left_first,
                right: left_second,
                data: left_data,
            },
            Tree::Node {
                left: right_first,
                right: right_second,
                data: right_data,
            },
        ) => {
            left_data == right_data
                && exact_tiled_tree_eq(left_first, right_first)
                && exact_tiled_tree_eq(left_second, right_second)
        }
        _ => false,
    }
}

fn exact_optional_tiled_tree_eq(left: &Option<Tree>, right: &Option<Tree>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => exact_tiled_tree_eq(left, right),
        (None, None) => true,
        _ => false,
    }
}

fn exact_pane_stack_maps_eq(
    left: &HashMap<usize, PaneStack>,
    right: &HashMap<usize, PaneStack>,
) -> bool {
    left.len() == right.len()
        && left.iter().all(|(slot, left_stack)| {
            right.get(slot).is_some_and(|right_stack| {
                left_stack.active_index() == right_stack.active_index()
                    && left_stack.panes().len() == right_stack.panes().len()
                    && left_stack
                        .panes()
                        .iter()
                        .zip(right_stack.panes())
                        .all(|(left, right)| Arc::ptr_eq(left, right))
            })
        })
}

#[derive(Clone)]
pub struct PositionedFloatingPane {
    pub pane_id: PaneId,
    pub is_focused: bool,
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
    pub z_order: u32,
    pub visible: bool,
    pub pinned: bool,
    pub opacity: f32,
    pub pane: Arc<dyn Pane>,
}

impl std::fmt::Debug for PositionedFloatingPane {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::result::Result<(), std::fmt::Error> {
        fmt.debug_struct("PositionedFloatingPane")
            .field("pane_id", &self.pane_id)
            .field("is_focused", &self.is_focused)
            .field("left", &self.left)
            .field("top", &self.top)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("z_order", &self.z_order)
            .field("visible", &self.visible)
            .field("pinned", &self.pinned)
            .field("opacity", &self.opacity)
            .finish()
    }
}

impl std::fmt::Debug for PositionedPane {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::result::Result<(), std::fmt::Error> {
        fmt.debug_struct("PositionedPane")
            .field("index", &self.index)
            .field("is_active", &self.is_active)
            .field("left", &self.left)
            .field("top", &self.top)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("pane_id", &self.pane.pane_id())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// The size is of the (first, second) child of the split
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct SplitDirectionAndSize {
    pub direction: SplitDirection,
    pub first: TerminalSize,
    pub second: TerminalSize,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum SplitSize {
    Cells(usize),
    Percent(u8),
}

impl Default for SplitSize {
    fn default() -> Self {
        Self::Percent(50)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct SplitRequest {
    pub direction: SplitDirection,
    /// Whether the newly created item will be in the second part
    /// of the split (right/bottom)
    pub target_is_second: bool,
    /// Split across the top of the tab rather than the active pane
    pub top_level: bool,
    /// The size of the new item
    pub size: SplitSize,
}

impl Default for SplitRequest {
    fn default() -> Self {
        Self {
            direction: SplitDirection::Horizontal,
            target_is_second: true,
            top_level: false,
            size: SplitSize::default(),
        }
    }
}

impl SplitDirectionAndSize {
    fn top_of_second(&self) -> usize {
        match self.direction {
            SplitDirection::Horizontal => 0,
            SplitDirection::Vertical => split_separator_offset(self.first.rows),
        }
    }

    fn left_of_second(&self) -> usize {
        match self.direction {
            SplitDirection::Horizontal => split_separator_offset(self.first.cols),
            SplitDirection::Vertical => 0,
        }
    }

    pub fn width(&self) -> usize {
        if self.direction == SplitDirection::Horizontal {
            split_separator_sum(self.first.cols, self.second.cols)
        } else {
            self.first.cols
        }
    }

    pub fn height(&self) -> usize {
        if self.direction == SplitDirection::Vertical {
            split_separator_sum(self.first.rows, self.second.rows)
        } else {
            self.first.rows
        }
    }

    pub fn size(&self) -> TerminalSize {
        let cell_width = self
            .first
            .pixel_width
            .checked_div(self.first.cols)
            .unwrap_or(0);
        let cell_height = self
            .first
            .pixel_height
            .checked_div(self.first.rows)
            .unwrap_or(0);

        let rows = self.height();
        let cols = self.width();

        TerminalSize {
            rows,
            cols,
            pixel_height: pixel_span(cell_height, rows),
            pixel_width: pixel_span(cell_width, cols),
            dpi: self.first.dpi,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct PositionedSplit {
    /// The topological node index that can be used to reference this split
    pub index: usize,
    pub direction: SplitDirection,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this split, in cells.
    pub left: usize,
    /// The offset from the top left corner of the containing tab to the top
    /// left corner of this split, in cells.
    pub top: usize,
    /// For Horizontal splits, how tall the split should be, for Vertical
    /// splits how wide it should be
    pub size: usize,
}

fn is_pane(pane: &Arc<dyn Pane>, other: &Option<&Arc<dyn Pane>>) -> bool {
    if let Some(other) = other {
        Arc::ptr_eq(other, pane)
    } else {
        false
    }
}

fn capture_pane_arena_tree(
    tree: Option<&Tree>,
    active: Option<Arc<dyn Pane>>,
    zoomed: Option<Arc<dyn Pane>>,
    pane_ids: &HashMap<PaneIdentity, PaneId>,
    tab_title: String,
    arena_start: usize,
    max_depth: usize,
    max_total_nodes: usize,
) -> anyhow::Result<(
    Vec<CapturedPaneArenaNode>,
    Option<Arc<dyn Pane>>,
    Option<Arc<dyn Pane>>,
    String,
)> {
    let Some(tree) = tree else {
        return Ok((Vec::new(), active, zoomed, tab_title));
    };
    let remaining = max_total_nodes.checked_sub(arena_start).ok_or_else(|| {
        anyhow::anyhow!(
            "ordered pane arena already has {arena_start} nodes, above limit {max_total_nodes}"
        )
    })?;
    if remaining == 0 {
        anyhow::bail!("ordered pane arena exceeds total node limit {max_total_nodes}");
    }
    let mut captured = Vec::new();
    captured
        .try_reserve_exact(remaining.min(64))
        .map_err(|error| anyhow::anyhow!("reserve callback-free pane capture: {error}"))?;
    let mut tasks = Vec::new();
    tasks
        .try_reserve_exact(max_depth.saturating_mul(2).min(256).min(remaining))
        .map_err(|error| anyhow::anyhow!("reserve callback-free pane traversal: {error}"))?;
    reserve_pane_arena_stack_push(&mut tasks, 1, remaining, "callback-free pane traversal")?;
    tasks.push(CapturePaneArenaTask::Visit {
        tree,
        depth: 1,
        left_col: 0,
        top_row: 0,
    });

    while let Some(task) = tasks.pop() {
        match task {
            CapturePaneArenaTask::Visit {
                tree,
                depth,
                left_col,
                top_row,
            } => {
                if depth > max_depth {
                    anyhow::bail!(
                        "tab pane tree depth {depth} exceeds ordered snapshot limit {max_depth}"
                    );
                }
                if captured.len() == remaining {
                    anyhow::bail!("ordered pane arena exceeds total node limit {max_total_nodes}");
                }
                reserve_pane_arena_stack_push(
                    &mut captured,
                    1,
                    remaining,
                    "callback-free pane capture",
                )?;
                match tree {
                    Tree::Empty => captured.push(CapturedPaneArenaNode::Empty),
                    Tree::Leaf(pane) => {
                        let identity = pane_identity(pane);
                        let pane_id = pane_ids.get(&identity).copied().ok_or_else(|| {
                            anyhow::anyhow!(
                                "an exact pane identity disappeared from tab {tab_title:?} callback-free capture",
                            )
                        })?;
                        captured.push(CapturedPaneArenaNode::Leaf {
                            identity,
                            pane_id,
                            left_col,
                            top_row,
                        });
                    }
                    Tree::Node { left, right, data } => {
                        let node = data.ok_or_else(|| {
                            anyhow::anyhow!("tab {tab_title:?} has an uninitialized split node")
                        })?;
                        let right_left_col = if node.direction == SplitDirection::Vertical {
                            left_col
                        } else {
                            left_col.checked_add(node.left_of_second()).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "tab {tab_title:?} right-pane column offset overflows usize"
                                )
                            })?
                        };
                        let right_top_row = if node.direction == SplitDirection::Horizontal {
                            top_row
                        } else {
                            top_row.checked_add(node.top_of_second()).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "tab {tab_title:?} right-pane row offset overflows usize"
                                )
                            })?
                        };
                        let split_index = captured.len();
                        captured.push(CapturedPaneArenaNode::Split {
                            left: split_index.checked_add(1).ok_or_else(|| {
                                anyhow::anyhow!("ordered pane arena left-child index overflows")
                            })?,
                            right: usize::MAX,
                            node,
                        });
                        let next_depth = depth.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("ordered pane arena depth overflows usize")
                        })?;
                        reserve_pane_arena_stack_push(
                            &mut tasks,
                            2,
                            remaining,
                            "callback-free pane traversal",
                        )?;
                        tasks.push(CapturePaneArenaTask::VisitRight {
                            split_index,
                            tree: right,
                            depth: next_depth,
                            left_col: right_left_col,
                            top_row: right_top_row,
                        });
                        tasks.push(CapturePaneArenaTask::Visit {
                            tree: left,
                            depth: next_depth,
                            left_col,
                            top_row,
                        });
                    }
                }
            }
            CapturePaneArenaTask::VisitRight {
                split_index,
                tree,
                depth,
                left_col,
                top_row,
            } => {
                let right = captured.len();
                let Some(CapturedPaneArenaNode::Split {
                    right: split_right, ..
                }) = captured.get_mut(split_index)
                else {
                    anyhow::bail!("ordered pane traversal lost split placeholder {split_index}");
                };
                *split_right = right;
                reserve_pane_arena_stack_push(
                    &mut tasks,
                    1,
                    remaining,
                    "callback-free pane traversal",
                )?;
                tasks.push(CapturePaneArenaTask::Visit {
                    tree,
                    depth,
                    left_col,
                    top_row,
                });
            }
        }
    }

    Ok((captured, active, zoomed, tab_title))
}

fn pane_entry(
    pane: &Arc<dyn Pane>,
    pane_id: PaneId,
    tab_id: TabId,
    window_id: WindowId,
    active: Option<&Arc<dyn Pane>>,
    zoomed: Option<&Arc<dyn Pane>>,
    workspace: &str,
    left_col: usize,
    top_row: usize,
) -> PaneEntry {
    let dims = pane.get_dimensions();
    let working_dir = pane.get_current_working_dir(CachePolicy::AllowStale);
    let cursor_pos = pane.get_cursor_position();

    PaneEntry {
        window_id,
        tab_id,
        pane_id,
        title: pane.get_title(),
        is_active_pane: is_pane(pane, &active),
        is_zoomed_pane: is_pane(pane, &zoomed),
        size: TerminalSize {
            cols: dims.cols,
            rows: dims.viewport_rows,
            pixel_height: dims.pixel_height,
            pixel_width: dims.pixel_width,
            dpi: dims.dpi,
        },
        working_dir: working_dir.map(Into::into),
        alt_screen_active: pane.is_alt_screen_active(),
        workspace: workspace.to_string(),
        cursor_pos,
        physical_top: dims.physical_top,
        left_col,
        top_row,
        tty_name: pane.tty_name(),
    }
}

fn pane_entry_from_ordered_observation(
    observed: OrderedPaneEntryObservation,
    tab_id: TabId,
    window_id: WindowId,
    is_active_pane: bool,
    is_zoomed_pane: bool,
    left_col: usize,
    top_row: usize,
) -> PaneEntry {
    PaneEntry {
        window_id,
        tab_id,
        pane_id: observed.pane_id,
        title: observed.title,
        is_active_pane,
        is_zoomed_pane,
        size: observed.size,
        working_dir: observed.working_dir,
        alt_screen_active: observed.alt_screen_active,
        workspace: observed.workspace,
        cursor_pos: observed.cursor_pos,
        physical_top: observed.physical_top,
        left_col,
        top_row,
        tty_name: observed.tty_name,
    }
}

fn pane_tree(
    tree: &Tree,
    tab_id: TabId,
    window_id: WindowId,
    active: Option<&Arc<dyn Pane>>,
    zoomed: Option<&Arc<dyn Pane>>,
    workspace: &str,
    left_col: usize,
    top_row: usize,
) -> PaneNode {
    match tree {
        Tree::Empty => PaneNode::Empty,
        Tree::Node { left, right, data } => {
            let data = data.unwrap();
            PaneNode::Split {
                left: Box::new(pane_tree(
                    &*left, tab_id, window_id, active, zoomed, workspace, left_col, top_row,
                )),
                right: Box::new(pane_tree(
                    &*right,
                    tab_id,
                    window_id,
                    active,
                    zoomed,
                    workspace,
                    if data.direction == SplitDirection::Vertical {
                        left_col
                    } else {
                        left_col + data.left_of_second()
                    },
                    if data.direction == SplitDirection::Horizontal {
                        top_row
                    } else {
                        top_row + data.top_of_second()
                    },
                )),
                node: data,
            }
        }
        Tree::Leaf(pane) => PaneNode::Leaf(pane_entry(
            pane,
            pane.pane_id(),
            tab_id,
            window_id,
            active,
            zoomed,
            workspace,
            left_col,
            top_row,
        )),
    }
}

fn build_from_pane_tree<F>(
    tree: bintree::Tree<PaneEntry, SplitDirectionAndSize>,
    active: &mut Option<Arc<dyn Pane>>,
    zoomed: &mut Option<Arc<dyn Pane>>,
    make_pane: &mut F,
) -> anyhow::Result<Tree>
where
    F: FnMut(PaneEntry) -> anyhow::Result<Arc<dyn Pane>>,
{
    Ok(match tree {
        bintree::Tree::Empty => Tree::Empty,
        bintree::Tree::Node { left, right, data } => Tree::Node {
            left: Box::new(build_from_pane_tree(*left, active, zoomed, make_pane)?),
            right: Box::new(build_from_pane_tree(*right, active, zoomed, make_pane)?),
            data,
        },
        bintree::Tree::Leaf(entry) => {
            let is_zoomed_pane = entry.is_zoomed_pane;
            let is_active_pane = entry.is_active_pane;
            let pane = make_pane(entry)?;
            if is_zoomed_pane {
                zoomed.replace(Arc::clone(&pane));
            }
            if is_active_pane {
                active.replace(Arc::clone(&pane));
            }
            Tree::Leaf(pane)
        }
    })
}

/// One node in a canonical, contiguous preorder pane arena.
///
/// The codec owns protocol admission and resource ceilings.  The mux owns this
/// application-facing representation so a validated snapshot can reach the
/// real tab tree without first constructing a temporary recursive
/// [`PaneNode`].
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub enum PaneArenaNode {
    Empty,
    Split {
        left: u32,
        right: u32,
        node: SplitDirectionAndSize,
    },
    Leaf(PaneEntry),
}

/// One tab's contiguous range in a [`PaneArena`].
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct PaneArenaTree {
    pub root_index: Option<u32>,
    pub node_count: u32,
    pub tab_title: String,
}

/// One successfully appended tree plus the exact work consumed by all of its
/// coherence attempts.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneArenaAppendReceipt {
    pub tree: PaneArenaTree,
    pub leaf_count: usize,
    pub work: PaneSnapshotCensusStats,
}

/// One canonical remote window title carried beside a [`PaneArena`].
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct PaneArenaWindowTitle {
    pub window_id: u64,
    pub title: String,
}

enum CapturedPaneArenaNode {
    Empty,
    Split {
        left: usize,
        right: usize,
        node: SplitDirectionAndSize,
    },
    Leaf {
        identity: PaneIdentity,
        pane_id: PaneId,
        left_col: usize,
        top_row: usize,
    },
}

enum CapturePaneArenaTask<'a> {
    Visit {
        tree: &'a Tree,
        depth: usize,
        left_col: usize,
        top_row: usize,
    },
    VisitRight {
        split_index: usize,
        tree: &'a Tree,
        depth: usize,
        left_col: usize,
        top_row: usize,
    },
}

/// Owned flat pane topology used by ordered snapshot transport and direct mux
/// application.
///
/// The vectors are private to prevent consumers from accidentally treating an
/// unvalidated partial mutation as authority.  Codec admission constructs the
/// value from bounded sections and validates it before publication; server
/// producers use the same constructor followed by the same validation path.
#[derive(PartialEq, Debug, Clone)]
pub struct PaneArena {
    trees: Vec<PaneArenaTree>,
    nodes: Vec<PaneArenaNode>,
    window_titles: Vec<PaneArenaWindowTitle>,
}

impl PaneArena {
    /// Assemble arena storage before protocol validation.
    ///
    /// This constructor deliberately does not claim validity; codec-specific
    /// limits and identity rules cannot live in the dependency-lower mux
    /// crate.
    pub fn from_unvalidated_parts(
        trees: Vec<PaneArenaTree>,
        nodes: Vec<PaneArenaNode>,
        window_titles: Vec<PaneArenaWindowTitle>,
    ) -> Self {
        Self {
            trees,
            nodes,
            window_titles,
        }
    }

    pub fn trees(&self) -> &[PaneArenaTree] {
        &self.trees
    }

    pub fn nodes(&self) -> &[PaneArenaNode] {
        &self.nodes
    }

    pub fn window_titles(&self) -> &[PaneArenaWindowTitle] {
        &self.window_titles
    }

    pub fn into_parts(
        self,
    ) -> (
        Vec<PaneArenaTree>,
        Vec<PaneArenaNode>,
        Vec<PaneArenaWindowTitle>,
    ) {
        (self.trees, self.nodes, self.window_titles)
    }
}

/// A fully materialized mux pane tree that has not yet been installed in a
/// tab.  Its fields stay private so callers cannot separate the tree from its
/// active/zoomed authority while staging multiple remote tabs in forward
/// order.
pub struct PreparedPaneTree {
    tree: Tree,
    active: Option<Arc<dyn Pane>>,
    zoomed: Option<Arc<dyn Pane>>,
}

impl PreparedPaneTree {
    fn snapshot_panes_callback_free(&self) -> Vec<Arc<dyn Pane>> {
        let mut panes = Vec::new();
        collect_raw_tree_panes(&self.tree, &mut panes);
        panes
    }

    fn into_install(
        self,
        size: TerminalSize,
        panes: &[Arc<dyn Pane>],
    ) -> anyhow::Result<PreparedPaneTreeInstall> {
        let Self {
            tree,
            active,
            zoomed,
        } = self;
        let active_index = active.as_ref().and_then(|active| {
            panes
                .iter()
                .position(|candidate| Arc::ptr_eq(candidate, active))
        });
        let mut resize_work = Vec::new();
        resize_work
            .try_reserve_exact(panes.len().max(usize::from(zoomed.is_some())))
            .map_err(|error| anyhow::anyhow!("reserve prepared-tree resize work: {error}"))?;
        if size.rows != 0 && size.cols != 0 {
            if let Some(zoomed) = zoomed.as_ref() {
                resize_work.push((Arc::clone(zoomed), size));
            } else {
                collect_pane_resize_work(&tree, &size, &mut resize_work);
            }
        }
        Ok(PreparedPaneTreeInstall {
            tree,
            active_index: active_index.unwrap_or(0),
            tag_active: active_index.is_some(),
            zoomed,
            size,
            resize_work,
        })
    }
}

struct PreparedPaneTreeInstall {
    tree: Tree,
    active_index: usize,
    tag_active: bool,
    zoomed: Option<Arc<dyn Pane>>,
    size: TerminalSize,
    resize_work: Vec<(Arc<dyn Pane>, TerminalSize)>,
}

/// Deterministic work and allocation-growth accounting for direct pane-arena
/// application.
///
/// `required_final_tree_box_allocations` counts the two owned children that
/// are intrinsic to each final mux split. It deliberately excludes pane
/// implementation allocations performed by the caller's `make_pane` callback.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PaneArenaPreparationStats {
    pub trees_started: usize,
    pub trees_completed: usize,
    pub validation_node_visits: usize,
    pub application_node_visits: usize,
    pub leaf_resolutions: usize,
    pub split_materializations: usize,
    pub required_final_tree_box_allocations: usize,
    pub validation_stack_growth_events: usize,
    pub application_stack_growth_events: usize,
    pub peak_validation_stack_entries: usize,
    pub peak_application_stack_entries: usize,
}

/// Reusable fallible work storage for direct pane-arena preparation.
///
/// A connection should retain one instance while consuming all tab ranges in
/// a snapshot so leaf-heavy fleets do not allocate two temporary vectors for
/// every tab.
#[derive(Default)]
pub struct PaneArenaPreparationScratch {
    validation: Vec<usize>,
    application: Vec<(usize, Tree)>,
    stats: PaneArenaPreparationStats,
}

impl PaneArenaPreparationScratch {
    /// Cumulative accounting since construction or the last explicit reset.
    pub const fn stats(&self) -> PaneArenaPreparationStats {
        self.stats
    }

    /// Reset counters without discarding reusable stack allocations.
    pub fn reset_stats(&mut self) {
        self.stats = PaneArenaPreparationStats::default();
    }

    /// Bytes requested by the two retained stack buffers, excluding allocator
    /// metadata and any allocator-specific size-class rounding.
    pub fn requested_retained_storage_bytes(&self) -> Option<usize> {
        self.validation
            .capacity()
            .checked_mul(std::mem::size_of::<usize>())?
            .checked_add(
                self.application
                    .capacity()
                    .checked_mul(std::mem::size_of::<(usize, Tree)>())?,
            )
    }

    /// Release reusable stack storage at a connection-terminal or quarantine
    /// boundary. Ordinary successful snapshots should keep the buffers for
    /// reuse; terminal paths should not retain their high-water capacity.
    pub fn release_retained_storage(&mut self) {
        self.validation = Vec::new();
        self.application = Vec::new();
    }
}

struct ClearPaneArenaApplicationOnDrop<'a> {
    stack: &'a mut Vec<(usize, Tree)>,
}

impl Drop for ClearPaneArenaApplicationOnDrop<'_> {
    fn drop(&mut self) {
        self.stack.clear();
    }
}

struct VecAppendRollback<'a, T> {
    vector: &'a mut Vec<T>,
    original_len: usize,
    committed: bool,
}

impl<'a, T> VecAppendRollback<'a, T> {
    fn new(vector: &'a mut Vec<T>) -> Self {
        let original_len = vector.len();
        Self {
            vector,
            original_len,
            committed: false,
        }
    }

    fn vector(&mut self) -> &mut Vec<T> {
        self.vector
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl<T> Drop for VecAppendRollback<'_, T> {
    fn drop(&mut self) {
        if !self.committed {
            self.vector.truncate(self.original_len);
        }
    }
}

fn reserve_pane_arena_stack_push<T>(
    stack: &mut Vec<T>,
    additional_entries: usize,
    maximum_entries: usize,
    label: &str,
) -> anyhow::Result<bool> {
    let required = stack
        .len()
        .checked_add(additional_entries)
        .ok_or_else(|| anyhow::anyhow!("{label} length overflows usize"))?;
    if required > maximum_entries {
        anyhow::bail!("{label} requires {required} entries, exceeding limit {maximum_entries}");
    }
    if required <= stack.capacity() {
        return Ok(false);
    }
    let geometric = stack.capacity().max(4).saturating_mul(2);
    let target_capacity = required.max(geometric).min(maximum_entries);
    stack
        .try_reserve_exact(target_capacity.saturating_sub(stack.len()))
        .map_err(|error| anyhow::anyhow!("reserve {label}: {error}"))?;
    Ok(true)
}

/// Consume one validated contiguous preorder range directly into the final
/// mux tree.
///
/// `node_count` selects the trailing range of the global arena, which lets a
/// client consume complete trees in reverse descriptor order without copying
/// node storage. Child indices are checked again at the application boundary,
/// so a caller cannot use this public API to bypass the codec's
/// canonical-arena invariant. Reversing the drained range makes each child's
/// final tree available when its split is visited; no temporary recursive
/// transfer tree or per-split transfer `Box` is created.
pub fn prepare_pane_tree_from_arena<F>(
    arena: &mut Vec<PaneArenaNode>,
    node_count: usize,
    make_pane: F,
) -> anyhow::Result<PreparedPaneTree>
where
    F: FnMut(PaneEntry) -> anyhow::Result<Arc<dyn Pane>>,
{
    let mut scratch = PaneArenaPreparationScratch::default();
    prepare_pane_tree_from_arena_with_scratch(arena, node_count, &mut scratch, make_pane)
}

/// Equivalent to [`prepare_pane_tree_from_arena`] while reusing caller-owned
/// validation and application stacks across every tab in one snapshot.
pub fn prepare_pane_tree_from_arena_with_scratch<F>(
    arena: &mut Vec<PaneArenaNode>,
    node_count: usize,
    scratch: &mut PaneArenaPreparationScratch,
    mut make_pane: F,
) -> anyhow::Result<PreparedPaneTree>
where
    F: FnMut(PaneEntry) -> anyhow::Result<Arc<dyn Pane>>,
{
    scratch.validation.clear();
    scratch.application.clear();
    scratch.stats.trees_started = scratch.stats.trees_started.saturating_add(1);
    if node_count == 0 {
        scratch.stats.trees_completed = scratch.stats.trees_completed.saturating_add(1);
        return Ok(PreparedPaneTree {
            tree: Tree::Empty,
            active: None,
            zoomed: None,
        });
    }
    let arena_end = arena.len();
    let arena_start = arena_end.checked_sub(node_count).ok_or_else(|| {
        anyhow::anyhow!(
            "pane arena requests {node_count} trailing nodes from a {arena_end}-node arena"
        )
    })?;

    // Validate the complete range before invoking `make_pane`, whose caller
    // may publish pane registrations.  This keeps malformed public-API input
    // from causing a partial topology mutation even though the codec normally
    // supplies already-validated arenas.
    let validation = &mut scratch.validation;
    if reserve_pane_arena_stack_push(validation, 1, node_count, "pane arena validation stack")? {
        scratch.stats.validation_stack_growth_events = scratch
            .stats
            .validation_stack_growth_events
            .saturating_add(1);
    }
    validation.push(arena_start);
    scratch.stats.peak_validation_stack_entries = scratch
        .stats
        .peak_validation_stack_entries
        .max(validation.len());
    let mut expected = arena_start;
    let mut leaf_count = 0usize;
    let mut active_count = 0usize;
    let mut zoomed_count = 0usize;
    while let Some(node_index) = validation.pop() {
        scratch.stats.validation_node_visits =
            scratch.stats.validation_node_visits.saturating_add(1);
        if node_index != expected || node_index >= arena_end {
            anyhow::bail!(
                "pane arena node {node_index} violates contiguous preorder at {expected}"
            );
        }
        expected = expected
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("pane arena preorder index overflows usize"))?;
        match &arena[node_index] {
            PaneArenaNode::Empty => {}
            PaneArenaNode::Leaf(entry) => {
                leaf_count = leaf_count
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("pane arena leaf count overflows usize"))?;
                active_count = active_count
                    .checked_add(usize::from(entry.is_active_pane))
                    .ok_or_else(|| anyhow::anyhow!("pane arena active count overflows usize"))?;
                zoomed_count = zoomed_count
                    .checked_add(usize::from(entry.is_zoomed_pane))
                    .ok_or_else(|| anyhow::anyhow!("pane arena zoomed count overflows usize"))?;
            }
            PaneArenaNode::Split { left, right, .. } => {
                let expected_left = node_index
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("pane arena left-child index overflows"))?;
                let left_index = usize::try_from(*left)
                    .map_err(|_| anyhow::anyhow!("pane arena left-child index does not fit"))?;
                let right_index = usize::try_from(*right)
                    .map_err(|_| anyhow::anyhow!("pane arena right-child index does not fit"))?;
                if left_index != expected_left
                    || right_index <= left_index
                    || right_index >= arena_end
                {
                    anyhow::bail!(
                        "pane arena split {node_index} has non-canonical children \
                         ({left_index}, {right_index}) for range {arena_start}..{arena_end}"
                    );
                }
                if reserve_pane_arena_stack_push(
                    validation,
                    2,
                    node_count,
                    "pane arena validation stack",
                )? {
                    scratch.stats.validation_stack_growth_events = scratch
                        .stats
                        .validation_stack_growth_events
                        .saturating_add(1);
                }
                validation.push(right_index);
                validation.push(left_index);
                scratch.stats.peak_validation_stack_entries = scratch
                    .stats
                    .peak_validation_stack_entries
                    .max(validation.len());
            }
        }
    }
    if expected != arena_end || leaf_count == 0 {
        anyhow::bail!(
            "pane arena range {arena_start}..{arena_end} is not one complete non-empty tree"
        );
    }
    if active_count > 1 || zoomed_count > 1 {
        anyhow::bail!(
            "pane arena range {arena_start}..{arena_end} has {active_count} active and \
             {zoomed_count} zoomed leaves; each authority must be unique"
        );
    }

    // The application vector owns partially materialized pane subtrees. Keep
    // its reusable allocation, but never retain those subtrees if a caller's
    // pane factory unwinds instead of returning an error.
    let application_guard = ClearPaneArenaApplicationOnDrop {
        stack: &mut scratch.application,
    };
    let stack = &mut *application_guard.stack;
    let mut active = None;
    let mut zoomed = None;

    macro_rules! application_try {
        ($expression:expr) => {
            match $expression {
                Ok(value) => value,
                Err(error) => {
                    stack.clear();
                    return Err(error.into());
                }
            }
        };
    }

    for (offset, node) in arena.drain(arena_start..).enumerate().rev() {
        scratch.stats.application_node_visits =
            scratch.stats.application_node_visits.saturating_add(1);
        let node_index = arena_start
            .checked_add(offset)
            .ok_or_else(|| anyhow::anyhow!("pane arena node index overflows usize"));
        let node_index = application_try!(node_index);
        match node {
            PaneArenaNode::Empty => {
                if application_try!(reserve_pane_arena_stack_push(
                    stack,
                    1,
                    node_count,
                    "pane arena application stack",
                )) {
                    scratch.stats.application_stack_growth_events = scratch
                        .stats
                        .application_stack_growth_events
                        .saturating_add(1);
                }
                stack.push((node_index, Tree::Empty));
            }
            PaneArenaNode::Leaf(entry) => {
                let is_zoomed_pane = entry.is_zoomed_pane;
                let is_active_pane = entry.is_active_pane;
                let pane = application_try!(make_pane(entry));
                if is_zoomed_pane {
                    zoomed.replace(Arc::clone(&pane));
                }
                if is_active_pane {
                    active.replace(Arc::clone(&pane));
                }
                scratch.stats.leaf_resolutions = scratch.stats.leaf_resolutions.saturating_add(1);
                if application_try!(reserve_pane_arena_stack_push(
                    stack,
                    1,
                    node_count,
                    "pane arena application stack",
                )) {
                    scratch.stats.application_stack_growth_events = scratch
                        .stats
                        .application_stack_growth_events
                        .saturating_add(1);
                }
                stack.push((node_index, Tree::Leaf(pane)));
            }
            PaneArenaNode::Split { left, right, node } => {
                let expected_left = node_index
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("pane arena left-child index overflows"));
                let expected_left = application_try!(expected_left);
                let left_index = usize::try_from(left)
                    .map_err(|_| anyhow::anyhow!("pane arena left-child index does not fit"));
                let left_index = application_try!(left_index);
                let right_index = usize::try_from(right)
                    .map_err(|_| anyhow::anyhow!("pane arena right-child index does not fit"));
                let right_index = application_try!(right_index);
                if left_index != expected_left
                    || right_index <= left_index
                    || right_index >= arena_end
                {
                    stack.clear();
                    anyhow::bail!(
                        "pane arena split {node_index} has non-canonical children \
                         ({left_index}, {right_index}) for range {arena_start}..{arena_end}"
                    );
                }
                let left = stack.pop().ok_or_else(|| {
                    anyhow::anyhow!("pane arena split {node_index} lost its left subtree")
                });
                let (actual_left, left) = application_try!(left);
                let right = stack.pop().ok_or_else(|| {
                    anyhow::anyhow!("pane arena split {node_index} lost its right subtree")
                });
                let (actual_right, right) = application_try!(right);
                if actual_left != left_index || actual_right != right_index {
                    stack.clear();
                    anyhow::bail!(
                        "pane arena split {node_index} declared children ({left_index}, \
                         {right_index}) but produced ({actual_left}, {actual_right})"
                    );
                }
                if application_try!(reserve_pane_arena_stack_push(
                    stack,
                    1,
                    node_count,
                    "pane arena application stack",
                )) {
                    scratch.stats.application_stack_growth_events = scratch
                        .stats
                        .application_stack_growth_events
                        .saturating_add(1);
                }
                stack.push((
                    node_index,
                    Tree::Node {
                        left: Box::new(left),
                        right: Box::new(right),
                        data: Some(node),
                    },
                ));
                scratch.stats.split_materializations =
                    scratch.stats.split_materializations.saturating_add(1);
                scratch.stats.required_final_tree_box_allocations = scratch
                    .stats
                    .required_final_tree_box_allocations
                    .saturating_add(2);
            }
        }
        scratch.stats.peak_application_stack_entries = scratch
            .stats
            .peak_application_stack_entries
            .max(stack.len());
    }

    if stack.len() != 1 {
        let produced_roots = stack.len();
        stack.clear();
        anyhow::bail!(
            "pane arena range {arena_start}..{arena_end} produced {} roots instead of one",
            produced_roots
        );
    }
    let root = stack
        .pop()
        .ok_or_else(|| anyhow::anyhow!("pane arena application lost its root"));
    let (root_index, tree) = application_try!(root);
    if root_index != arena_start {
        anyhow::bail!("pane arena application produced root {root_index}, expected {arena_start}");
    }
    scratch.stats.trees_completed = scratch.stats.trees_completed.saturating_add(1);
    Ok(PreparedPaneTree {
        tree,
        active,
        zoomed,
    })
}

/// Computes the minimum (x, y) size based on the panes in this portion
/// of the tree.
fn effective_pane_constraints(
    pane: &Arc<dyn Pane>,
    overrides: &HashMap<PaneId, PaneConstraints>,
) -> PaneConstraints {
    overrides
        .get(&pane.pane_id())
        .copied()
        .unwrap_or_else(|| pane.pane_constraints())
}

fn compute_min_size(tree: &Tree, overrides: &HashMap<PaneId, PaneConstraints>) -> (usize, usize) {
    match tree {
        Tree::Node { data: None, .. } | Tree::Empty => (1, 1),
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            let (left_x, left_y) = compute_min_size(&*left, overrides);
            let (right_x, right_y) = compute_min_size(&*right, overrides);
            match data.direction {
                SplitDirection::Vertical => {
                    (left_x.max(right_x), split_separator_sum(left_y, right_y))
                }
                SplitDirection::Horizontal => {
                    (split_separator_sum(left_x, right_x), left_y.max(right_y))
                }
            }
        }
        Tree::Leaf(pane) => {
            let constraints = effective_pane_constraints(pane, overrides);
            let min_width = constraints.min_width.max(1);
            let min_height = constraints.min_height.max(1);
            if constraints.fixed {
                let dims = pane.get_dimensions();
                (min_width.max(dims.cols), min_height.max(dims.viewport_rows))
            } else {
                (min_width, min_height)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Axis {
    Width,
    Height,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AxisConstraints {
    min: usize,
    max: Option<usize>,
    preferred: Option<usize>,
}

impl AxisConstraints {
    fn normalized(self) -> Self {
        let min = self.min.max(1);
        let max = self.max.map(|value| value.max(min));
        let preferred = self.preferred.map(|value| {
            let clamped = value.max(min);
            max.map_or(clamped, |max_value| clamped.min(max_value))
        });

        Self {
            min,
            max,
            preferred,
        }
    }
}

fn axis_constraints_from_pane_constraints(
    constraints: PaneConstraints,
    axis: Axis,
    fixed_size: Option<usize>,
) -> AxisConstraints {
    let (mut min, mut max, mut preferred) = match axis {
        Axis::Width => (
            constraints.min_width.max(1),
            constraints.max_width,
            constraints.preferred_width,
        ),
        Axis::Height => (
            constraints.min_height.max(1),
            constraints.max_height,
            constraints.preferred_height,
        ),
    };

    if constraints.fixed {
        if let Some(size) = fixed_size {
            min = min.max(size);
            max = Some(size);
            preferred = Some(size);
        }
    }

    AxisConstraints {
        min,
        max,
        preferred,
    }
    .normalized()
}

fn normalize_runtime_pane_constraints(mut constraints: PaneConstraints) -> PaneConstraints {
    constraints.min_width = constraints.min_width.max(1);
    constraints.min_height = constraints.min_height.max(1);
    constraints.max_width = constraints
        .max_width
        .map(|value| value.max(constraints.min_width));
    constraints.max_height = constraints
        .max_height
        .map(|value| value.max(constraints.min_height));
    constraints.preferred_width = constraints.preferred_width.map(|value| {
        let clamped = value.max(constraints.min_width);
        constraints
            .max_width
            .map_or(clamped, |max| clamped.min(max))
    });
    constraints.preferred_height = constraints.preferred_height.map(|value| {
        let clamped = value.max(constraints.min_height);
        constraints
            .max_height
            .map_or(clamped, |max| clamped.min(max))
    });
    constraints
}

fn pane_axis_constraints(
    pane: &Arc<dyn Pane>,
    axis: Axis,
    overrides: &HashMap<PaneId, PaneConstraints>,
) -> AxisConstraints {
    let constraints = effective_pane_constraints(pane, overrides);
    let dims = pane.get_dimensions();
    let fixed_size = match axis {
        Axis::Width => Some(dims.cols),
        Axis::Height => Some(dims.viewport_rows),
    };
    axis_constraints_from_pane_constraints(constraints, axis, fixed_size)
}

fn shared_axis_constraints(left: AxisConstraints, right: AxisConstraints) -> AxisConstraints {
    let min = left.min.max(right.min);
    let max = match (left.max, right.max) {
        (Some(left_max), Some(right_max)) => Some(left_max.min(right_max)),
        (Some(left_max), None) => Some(left_max),
        (None, Some(right_max)) => Some(right_max),
        (None, None) => None,
    };
    let preferred = match (left.preferred, right.preferred) {
        (Some(left_pref), Some(right_pref)) => Some(left_pref.max(right_pref)),
        (Some(left_pref), None) => Some(left_pref),
        (None, Some(right_pref)) => Some(right_pref),
        (None, None) => None,
    };

    AxisConstraints {
        min,
        max,
        preferred,
    }
    .normalized()
}

fn additive_axis_constraints(left: AxisConstraints, right: AxisConstraints) -> AxisConstraints {
    let min = left.min.saturating_add(right.min).saturating_add(1);
    let max = match (left.max, right.max) {
        (Some(left_max), Some(right_max)) => {
            Some(left_max.saturating_add(right_max).saturating_add(1))
        }
        _ => None,
    };
    let preferred = match (left.preferred, right.preferred) {
        (Some(left_pref), Some(right_pref)) => {
            Some(left_pref.saturating_add(right_pref).saturating_add(1))
        }
        _ => None,
    };

    AxisConstraints {
        min,
        max,
        preferred,
    }
    .normalized()
}

fn compute_axis_constraints(
    tree: &Tree,
    axis: Axis,
    overrides: &HashMap<PaneId, PaneConstraints>,
) -> AxisConstraints {
    match tree {
        Tree::Empty | Tree::Node { data: None, .. } => AxisConstraints {
            min: 1,
            max: None,
            preferred: None,
        },
        Tree::Leaf(pane) => pane_axis_constraints(pane, axis, overrides),
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            let left_constraints = compute_axis_constraints(&*left, axis, overrides);
            let right_constraints = compute_axis_constraints(&*right, axis, overrides);
            match (data.direction, axis) {
                (SplitDirection::Horizontal, Axis::Width)
                | (SplitDirection::Vertical, Axis::Height) => {
                    additive_axis_constraints(left_constraints, right_constraints)
                }
                _ => shared_axis_constraints(left_constraints, right_constraints),
            }
        }
    }
}

fn split_allocation(
    total: usize,
    first: AxisConstraints,
    second: AxisConstraints,
    preferred_first: Option<usize>,
) -> Option<(usize, usize)> {
    let available = total.checked_sub(1)?;
    if first.min.saturating_add(second.min) > available {
        return None;
    }

    let first_min = second.max.map_or(first.min, |second_max| {
        first.min.max(available.saturating_sub(second_max))
    });
    let first_max = first
        .max
        .unwrap_or(available)
        .min(available.saturating_sub(second.min));
    if first_min > first_max {
        return None;
    }

    let preferred =
        preferred_first
            .or(first.preferred)
            .or_else(|| {
                second
                    .preferred
                    .map(|value| available.saturating_sub(value))
            })
            .unwrap_or(first.min.saturating_add(
                available.saturating_sub(first.min.saturating_add(second.min)) / 2,
            ));
    let first_size = preferred.clamp(first_min, first_max);
    let second_size = available.saturating_sub(first_size);
    Some((first_size, second_size))
}

fn split_dimension_for_request(
    dim: usize,
    request: SplitRequest,
    first: AxisConstraints,
    second: AxisConstraints,
) -> Option<(usize, usize)> {
    let requested = requested_split_target_axis_size(dim, request);

    if request.target_is_second {
        let preferred_first = dim.saturating_sub(1).saturating_sub(requested);
        split_allocation(dim, first, second, Some(preferred_first))
    } else {
        split_allocation(dim, first, second, Some(requested))
    }
}

fn requested_split_target_axis_size(dim: usize, request: SplitRequest) -> usize {
    match request.size {
        SplitSize::Cells(n) => n,
        SplitSize::Percent(n) => dim.saturating_mul(n as usize) / 100,
    }
    .max(1)
}

fn pane_size_satisfies_constraints(
    size: &TerminalSize,
    width: AxisConstraints,
    height: AxisConstraints,
) -> bool {
    if size.cols < width.min || size.rows < height.min {
        return false;
    }
    if let Some(max_width) = width.max {
        if size.cols > max_width {
            return false;
        }
    }
    if let Some(max_height) = height.max {
        if size.rows > max_height {
            return false;
        }
    }
    true
}

/// Collect all leaf panes from a tree, returning (pane_id, collapse_priority).
fn collect_leaf_panes(tree: &Tree) -> Vec<(PaneId, CollapsePriority)> {
    let mut result = Vec::new();
    collect_leaf_panes_recursive(tree, &mut result);
    result
}

fn collect_leaf_panes_recursive(tree: &Tree, out: &mut Vec<(PaneId, CollapsePriority)>) {
    match tree {
        Tree::Empty | Tree::Node { data: None, .. } => {}
        Tree::Leaf(pane) => {
            out.push((pane.pane_id(), pane.collapse_priority()));
        }
        Tree::Node {
            left,
            right,
            data: Some(_),
            ..
        } => {
            collect_leaf_panes_recursive(left, out);
            collect_leaf_panes_recursive(right, out);
        }
    }
}

/// Return a numeric collapse order: lower number = collapse first.
/// `Low` collapses before `Normal` before `High`; `Never` is not collapsible.
fn collapse_order(priority: CollapsePriority) -> Option<u8> {
    match priority {
        CollapsePriority::Low => Some(0),
        CollapsePriority::Normal => Some(1),
        CollapsePriority::High => Some(2),
        CollapsePriority::Never => None,
    }
}

/// Compute the minimum size of a tree when a given set of panes are
/// treated as collapsed (contributing zero space).  This is used to
/// determine whether collapsing certain panes makes the tree fit.
fn compute_min_size_with_collapsed(
    tree: &Tree,
    collapsed: &HashSet<PaneId>,
    overrides: &HashMap<PaneId, PaneConstraints>,
) -> (usize, usize) {
    match tree {
        Tree::Empty | Tree::Node { data: None, .. } => (0, 0),
        Tree::Leaf(pane) => {
            if collapsed.contains(&pane.pane_id()) {
                (0, 0)
            } else {
                let c = effective_pane_constraints(pane, overrides);
                (c.min_width.max(1), c.min_height.max(1))
            }
        }
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            let (lw, lh) = compute_min_size_with_collapsed(left, collapsed, overrides);
            let (rw, rh) = compute_min_size_with_collapsed(right, collapsed, overrides);
            match data.direction {
                SplitDirection::Horizontal => {
                    let w = if lw == 0 && rw == 0 {
                        0
                    } else if lw == 0 {
                        rw
                    } else if rw == 0 {
                        lw
                    } else {
                        split_separator_sum(lw, rw)
                    };
                    (w, lh.max(rh))
                }
                SplitDirection::Vertical => {
                    let h = if lh == 0 && rh == 0 {
                        0
                    } else if lh == 0 {
                        rh
                    } else if rh == 0 {
                        lh
                    } else {
                        split_separator_sum(lh, rh)
                    };
                    (lw.max(rw), h)
                }
            }
        }
    }
}

/// Compute the resize budget for a given split: how far in each direction
/// the split divider can be moved while respecting all constraints.
/// Returns `(max_shrink_first, max_grow_first)` — negative deltas shrink
/// the first child, positive deltas grow it.
fn compute_split_resize_budget(
    left: &Tree,
    right: &Tree,
    direction: SplitDirection,
    first_size: &TerminalSize,
    second_size: &TerminalSize,
    overrides: &HashMap<PaneId, PaneConstraints>,
) -> (isize, isize) {
    let (left_wc, left_hc) = (
        compute_axis_constraints(left, Axis::Width, overrides),
        compute_axis_constraints(left, Axis::Height, overrides),
    );
    let (right_wc, right_hc) = (
        compute_axis_constraints(right, Axis::Width, overrides),
        compute_axis_constraints(right, Axis::Height, overrides),
    );

    match direction {
        SplitDirection::Horizontal => {
            let left_can_shrink = first_size.cols.saturating_sub(left_wc.min);
            let right_can_shrink = second_size.cols.saturating_sub(right_wc.min);
            let left_can_grow = left_wc.max.map_or(right_can_shrink, |max| {
                max.saturating_sub(first_size.cols).min(right_can_shrink)
            });
            (
                negative_resize_budget(left_can_shrink),
                positive_resize_budget(left_can_grow),
            )
        }
        SplitDirection::Vertical => {
            let left_can_shrink = first_size.rows.saturating_sub(left_hc.min);
            let right_can_shrink = second_size.rows.saturating_sub(right_hc.min);
            let left_can_grow = left_hc.max.map_or(right_can_shrink, |max| {
                max.saturating_sub(first_size.rows).min(right_can_shrink)
            });
            (
                negative_resize_budget(left_can_shrink),
                positive_resize_budget(left_can_grow),
            )
        }
    }
}

/// Replace a pane in the tree by matching on PaneId.
fn replace_pane_recursive(tree: &mut Tree, old_id: PaneId, new_pane: Arc<dyn Pane>) {
    match tree {
        Tree::Empty | Tree::Node { data: None, .. } => {}
        Tree::Leaf(pane) => {
            if pane.pane_id() == old_id {
                *pane = new_pane;
            }
        }
        Tree::Node { left, right, .. } => {
            replace_pane_recursive(left, old_id, new_pane.clone());
            replace_pane_recursive(right, old_id, new_pane);
        }
    }
}

/// Returns `true` if every leaf pane in `tree` belongs to `collapsed`.
fn is_subtree_fully_collapsed(tree: &Tree, collapsed: &HashSet<PaneId>) -> bool {
    match tree {
        Tree::Empty | Tree::Node { data: None, .. } => true,
        Tree::Leaf(pane) => collapsed.contains(&pane.pane_id()),
        Tree::Node {
            left,
            right,
            data: Some(_),
        } => {
            is_subtree_fully_collapsed(left, collapsed)
                && is_subtree_fully_collapsed(right, collapsed)
        }
    }
}

/// Post-pass that redistributes space away from fully-collapsed subtrees.
/// At each split node, if one child is fully collapsed its allocated space
/// (plus the separator cell) is given to the sibling.  Collapsed leaf panes
/// receive a 1×1 allocation so that `pane.resize()` does not reject a 0-size.
fn redistribute_for_collapsed(
    tree: &mut Tree,
    collapsed: &HashSet<PaneId>,
    cell_dims: &TerminalSize,
) {
    if collapsed.is_empty() {
        return;
    }
    match tree {
        Tree::Empty | Tree::Leaf(_) | Tree::Node { data: None, .. } => {}
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            let left_collapsed = is_subtree_fully_collapsed(left, collapsed);
            let right_collapsed = is_subtree_fully_collapsed(right, collapsed);

            if left_collapsed && !right_collapsed {
                match data.direction {
                    SplitDirection::Horizontal => {
                        // Left is collapsed: give its cols + 1 separator to right
                        let freed = data.first.cols.saturating_add(1);
                        data.second.cols = data.second.cols.saturating_add(freed);
                        data.second.pixel_width =
                            data.second.cols.saturating_mul(cell_dims.pixel_width);
                        data.first.cols = 1;
                        data.first.pixel_width = cell_dims.pixel_width;
                    }
                    SplitDirection::Vertical => {
                        let freed = data.first.rows.saturating_add(1);
                        data.second.rows = data.second.rows.saturating_add(freed);
                        data.second.pixel_height =
                            data.second.rows.saturating_mul(cell_dims.pixel_height);
                        data.first.rows = 1;
                        data.first.pixel_height = cell_dims.pixel_height;
                    }
                }
            } else if right_collapsed && !left_collapsed {
                match data.direction {
                    SplitDirection::Horizontal => {
                        let freed = data.second.cols.saturating_add(1);
                        data.first.cols = data.first.cols.saturating_add(freed);
                        data.first.pixel_width =
                            data.first.cols.saturating_mul(cell_dims.pixel_width);
                        data.second.cols = 1;
                        data.second.pixel_width = cell_dims.pixel_width;
                    }
                    SplitDirection::Vertical => {
                        let freed = data.second.rows.saturating_add(1);
                        data.first.rows = data.first.rows.saturating_add(freed);
                        data.first.pixel_height =
                            data.first.rows.saturating_mul(cell_dims.pixel_height);
                        data.second.rows = 1;
                        data.second.pixel_height = cell_dims.pixel_height;
                    }
                }
            }
            // Both collapsed or neither: leave sizes as-is.

            // Recurse into non-fully-collapsed children.
            if !left_collapsed {
                redistribute_for_collapsed(left, collapsed, cell_dims);
            }
            if !right_collapsed {
                redistribute_for_collapsed(right, collapsed, cell_dims);
            }
        }
    }
}

/// Recursively walk the tree in pre-order to find the split at `target_index`
/// and compute its resize budget.
fn find_split_budget(
    tree: &Tree,
    target_index: usize,
    counter: &mut usize,
    overrides: &HashMap<PaneId, PaneConstraints>,
) -> Option<(isize, isize)> {
    match tree {
        Tree::Empty | Tree::Leaf(_) | Tree::Node { data: None, .. } => None,
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            if *counter == target_index {
                return Some(compute_split_resize_budget(
                    left,
                    right,
                    data.direction,
                    &data.first,
                    &data.second,
                    overrides,
                ));
            }
            advance_split_budget_counter(counter);
            if let Some(result) = find_split_budget(left, target_index, counter, overrides) {
                return Some(result);
            }
            find_split_budget(right, target_index, counter, overrides)
        }
    }
}

fn advance_split_budget_counter(counter: &mut usize) {
    *counter = counter.saturating_add(1);
}

fn adjust_x_size(
    tree: &mut Tree,
    mut x_adjust: isize,
    cell_dimensions: &TerminalSize,
    overrides: &HashMap<PaneId, PaneConstraints>,
) {
    let x_constraints = compute_axis_constraints(tree, Axis::Width, overrides);
    let min_x = x_constraints.min;
    let max_x = x_constraints.max;
    while x_adjust != 0 {
        match tree {
            Tree::Empty | Tree::Leaf(_) => return,
            Tree::Node { data: None, .. } => return,
            Tree::Node {
                left,
                right,
                data: Some(data),
            } => {
                data.first.dpi = cell_dimensions.dpi;
                data.second.dpi = cell_dimensions.dpi;
                match data.direction {
                    SplitDirection::Vertical => {
                        let mut new_cols = usize_to_isize_saturating(data.first.cols)
                            .saturating_add(x_adjust)
                            .max(usize_to_isize_saturating(min_x));
                        if let Some(max_cols) = max_x {
                            new_cols = new_cols.min(usize_to_isize_saturating(max_cols));
                        }
                        x_adjust =
                            new_cols.saturating_sub(usize_to_isize_saturating(data.first.cols));

                        if x_adjust != 0 {
                            adjust_x_size(&mut *left, x_adjust, cell_dimensions, overrides);
                            data.first.cols = new_cols.max(0) as usize;
                            data.first.pixel_width =
                                data.first.cols.saturating_mul(cell_dimensions.pixel_width);

                            adjust_x_size(&mut *right, x_adjust, cell_dimensions, overrides);
                            data.second.cols = data.first.cols;
                            data.second.pixel_width = data.first.pixel_width;
                        }
                        return;
                    }
                    SplitDirection::Horizontal if x_adjust > 0 => {
                        let left_max_x =
                            compute_axis_constraints(&*left, Axis::Width, overrides).max;
                        if left_max_x.map_or(true, |max_cols| data.first.cols < max_cols) {
                            adjust_x_size(&mut *left, 1, cell_dimensions, overrides);
                            data.first.cols += 1;
                            data.first.pixel_width =
                                data.first.cols.saturating_mul(cell_dimensions.pixel_width);
                            x_adjust -= 1;
                        }

                        if x_adjust > 0 {
                            let right_max_x =
                                compute_axis_constraints(&*right, Axis::Width, overrides).max;
                            if right_max_x.map_or(true, |max_cols| data.second.cols < max_cols) {
                                adjust_x_size(&mut *right, 1, cell_dimensions, overrides);
                                data.second.cols += 1;
                                data.second.pixel_width =
                                    data.second.cols.saturating_mul(cell_dimensions.pixel_width);
                                x_adjust -= 1;
                            } else {
                                return;
                            }
                        }
                    }
                    SplitDirection::Horizontal => {
                        // x_adjust is negative
                        let (left_min_x, _) = compute_min_size(&*left, overrides);
                        let (right_min_x, _) = compute_min_size(&*right, overrides);
                        if data.first.cols > left_min_x {
                            adjust_x_size(&mut *left, -1, cell_dimensions, overrides);
                            data.first.cols -= 1;
                            data.first.pixel_width =
                                data.first.cols.saturating_mul(cell_dimensions.pixel_width);
                            x_adjust += 1;
                        }
                        if x_adjust < 0 && data.second.cols > right_min_x {
                            adjust_x_size(&mut *right, -1, cell_dimensions, overrides);
                            data.second.cols -= 1;
                            data.second.pixel_width =
                                data.second.cols.saturating_mul(cell_dimensions.pixel_width);
                            x_adjust += 1;
                        }
                    }
                }
            }
        }
    }
}

fn adjust_y_size(
    tree: &mut Tree,
    mut y_adjust: isize,
    cell_dimensions: &TerminalSize,
    overrides: &HashMap<PaneId, PaneConstraints>,
) {
    let y_constraints = compute_axis_constraints(tree, Axis::Height, overrides);
    let min_y = y_constraints.min;
    let max_y = y_constraints.max;
    while y_adjust != 0 {
        match tree {
            Tree::Empty | Tree::Leaf(_) => return,
            Tree::Node { data: None, .. } => return,
            Tree::Node {
                left,
                right,
                data: Some(data),
            } => {
                data.first.dpi = cell_dimensions.dpi;
                data.second.dpi = cell_dimensions.dpi;
                match data.direction {
                    SplitDirection::Horizontal => {
                        let mut new_rows = usize_to_isize_saturating(data.first.rows)
                            .saturating_add(y_adjust)
                            .max(usize_to_isize_saturating(min_y));
                        if let Some(max_rows) = max_y {
                            new_rows = new_rows.min(usize_to_isize_saturating(max_rows));
                        }
                        y_adjust =
                            new_rows.saturating_sub(usize_to_isize_saturating(data.first.rows));

                        if y_adjust != 0 {
                            adjust_y_size(&mut *left, y_adjust, cell_dimensions, overrides);
                            data.first.rows = new_rows.max(0) as usize;
                            data.first.pixel_height =
                                data.first.rows.saturating_mul(cell_dimensions.pixel_height);

                            adjust_y_size(&mut *right, y_adjust, cell_dimensions, overrides);
                            data.second.rows = data.first.rows;
                            data.second.pixel_height = data.first.pixel_height;
                        }
                        return;
                    }
                    SplitDirection::Vertical if y_adjust > 0 => {
                        let left_max_y =
                            compute_axis_constraints(&*left, Axis::Height, overrides).max;
                        if left_max_y.map_or(true, |max_rows| data.first.rows < max_rows) {
                            adjust_y_size(&mut *left, 1, cell_dimensions, overrides);
                            data.first.rows += 1;
                            data.first.pixel_height =
                                data.first.rows.saturating_mul(cell_dimensions.pixel_height);
                            y_adjust -= 1;
                        }
                        if y_adjust > 0 {
                            let right_max_y =
                                compute_axis_constraints(&*right, Axis::Height, overrides).max;
                            if right_max_y.map_or(true, |max_rows| data.second.rows < max_rows) {
                                adjust_y_size(&mut *right, 1, cell_dimensions, overrides);
                                data.second.rows += 1;
                                data.second.pixel_height = data
                                    .second
                                    .rows
                                    .saturating_mul(cell_dimensions.pixel_height);
                                y_adjust -= 1;
                            } else {
                                return;
                            }
                        }
                    }
                    SplitDirection::Vertical => {
                        // y_adjust is negative
                        let (_, left_min_y) = compute_min_size(&*left, overrides);
                        let (_, right_min_y) = compute_min_size(&*right, overrides);
                        if data.first.rows > left_min_y {
                            adjust_y_size(&mut *left, -1, cell_dimensions, overrides);
                            data.first.rows -= 1;
                            data.first.pixel_height =
                                data.first.rows.saturating_mul(cell_dimensions.pixel_height);
                            y_adjust += 1;
                        }
                        if y_adjust < 0 && data.second.rows > right_min_y {
                            adjust_y_size(&mut *right, -1, cell_dimensions, overrides);
                            data.second.rows -= 1;
                            data.second.pixel_height = data
                                .second
                                .rows
                                .saturating_mul(cell_dimensions.pixel_height);
                            y_adjust += 1;
                        }
                    }
                }
            }
        }
    }
}

fn collect_pane_resize_work(
    tree: &Tree,
    size: &TerminalSize,
    work: &mut Vec<(Arc<dyn Pane>, TerminalSize)>,
) {
    match tree {
        Tree::Empty => {}
        Tree::Node { data: None, .. } => {}
        Tree::Node {
            left,
            right,
            data: Some(data),
        } => {
            collect_pane_resize_work(&*left, &data.first, work);
            collect_pane_resize_work(&*right, &data.second, work);
        }
        Tree::Leaf(pane) => {
            work.push((Arc::clone(pane), *size));
        }
    }
}

fn apply_sizes_from_splits(tree: &Tree, size: &TerminalSize) {
    let mut work = Vec::new();
    collect_pane_resize_work(tree, size, &mut work);
    execute_pane_resize_work(work);
}

fn execute_pane_resize_work(work: Vec<(Arc<dyn Pane>, TerminalSize)>) {
    // Preserve one deterministic effect order until the persistent bounded
    // resize executor (ft-interactive-systems-performance-4tenz.7.2) owns
    // admission and sequencing. A fresh scoped thread fanout can fail after
    // earlier buckets have already resized their panes; crossbeam then resumes
    // the spawn panic and leaves the tab's committed geometry only partially
    // applied. Serial execution cannot fail at an OS-thread admission boundary
    // and gives every collected pane its resize callback exactly once in tree
    // order.
    for (pane, pane_size) in work {
        invoke_pane_resize(&pane, pane_size);
    }
}

fn invoke_pane_resize(pane: &Arc<dyn Pane>, pane_size: TerminalSize) {
    match catch_recoverable(
        RecoverablePanicSite::MuxPaneCallback,
        AssertUnwindSafe(|| pane.resize(pane_size)),
    ) {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            // Do not interpolate the arbitrary pane error: it can contain
            // unbounded remote/provider content. The exact in-process identity
            // and target geometry are sufficient to correlate this failure.
            log::error!(
                "pane resize callback returned an error for exact pane identity {:p} at rows={} cols={} pixel_width={} pixel_height={} dpi={}",
                Arc::as_ptr(pane),
                pane_size.rows,
                pane_size.cols,
                pane_size.pixel_width,
                pane_size.pixel_height,
                pane_size.dpi,
            );
        }
        Err(_) => {
            log::error!(
                "pane resize callback panicked for exact pane identity {:p}",
                Arc::as_ptr(pane)
            );
        }
    }
}

fn observe_pane_id_for_mutation(pane: &Arc<dyn Pane>) -> anyhow::Result<PaneId> {
    catch_recoverable(
        RecoverablePanicSite::MuxPaneCallback,
        AssertUnwindSafe(|| pane.pane_id()),
    )
    .map_err(|_| {
        anyhow::anyhow!(
            "pane identity callback panicked for exact pane identity {:p}",
            Arc::as_ptr(pane)
        )
    })
}

fn observe_pane_domain_id_for_mutation(pane: &Arc<dyn Pane>) -> anyhow::Result<DomainId> {
    catch_recoverable(
        RecoverablePanicSite::MuxPaneCallback,
        AssertUnwindSafe(|| pane.domain_id()),
    )
    .map_err(|_| {
        anyhow::anyhow!(
            "pane domain callback panicked for exact pane identity {:p}",
            Arc::as_ptr(pane)
        )
    })
}

fn exact_structural_lane_in_snapshot(
    tiled: &[Arc<dyn Pane>],
    floating: &[(PaneId, Arc<dyn Pane>)],
    pane: &Arc<dyn Pane>,
) -> Option<PaneStructuralLane> {
    let tiled_matches = tiled
        .iter()
        .filter(|candidate| Arc::ptr_eq(candidate, pane))
        .count();
    let floating_matches = floating
        .iter()
        .filter(|(_, candidate)| Arc::ptr_eq(candidate, pane))
        .count();
    match (tiled_matches, floating_matches) {
        (1, 0) => Some(PaneStructuralLane::Tiled),
        (0, 1) => Some(PaneStructuralLane::Floating),
        _ => None,
    }
}

fn cell_dimensions(size: &TerminalSize) -> TerminalSize {
    TerminalSize {
        rows: 1,
        cols: 1,
        pixel_width: size.pixel_width.checked_div(size.cols).unwrap_or(0),
        pixel_height: size.pixel_height.checked_div(size.rows).unwrap_or(0),
        dpi: size.dpi,
    }
}

fn min_floating_pane_width() -> usize {
    configuration().min_floating_pane_width
}

fn min_floating_pane_height() -> usize {
    configuration().min_floating_pane_height
}

impl Tab {
    pub fn new(size: &TerminalSize) -> Self {
        let inner = TabInner::new(size);
        let tab_id = inner.id;
        Self {
            inner: Mutex::new(inner),
            tab_id,
            mux_owner_generation: AtomicU64::new(0),
        }
    }

    /// Bind a successful serialized tab registration to one exact mux.
    ///
    /// Callers must complete every other fallible registration step before
    /// invoking this method and must retain their tab-registry write guard
    /// through publication. That makes the owner transition the final
    /// fallible step and prevents a rejected registration from poisoning an
    /// otherwise reusable tab allocation.
    #[cfg(test)]
    pub(crate) fn bind_mux_owner_if_structurally_empty(
        &self,
        mux: &Arc<Mux>,
    ) -> anyhow::Result<()> {
        self.prepare_mux_owner_binding_if_structurally_empty(mux)?
            .commit();
        Ok(())
    }

    pub(crate) fn prepare_mux_owner_binding_if_structurally_empty<'a>(
        &'a self,
        mux: &Arc<Mux>,
    ) -> anyhow::Result<PreparedTabMuxOwnerBinding<'a>> {
        let inner = self.inner.lock();
        let (tiled, floating) = inner.snapshot_structural_panes_callback_free_checked()?;
        anyhow::ensure!(
            tiled.is_empty() && floating.is_empty(),
            "tab {} is not structurally empty",
            self.tab_id
        );
        let next_generation = inner.prepare_mux_owner_binding(mux)?;
        Ok(PreparedTabMuxOwnerBinding {
            tab: self,
            inner,
            mux: Arc::clone(mux),
            next_generation,
        })
    }

    pub(crate) fn prepare_mux_owner_binding_with_exact_single_tiled_pane<'a>(
        &'a self,
        mux: &Arc<Mux>,
        pane: &Arc<dyn Pane>,
    ) -> anyhow::Result<PreparedTabMuxOwnerBinding<'a>> {
        let inner = self.inner.lock();
        let (tiled, floating) = inner.snapshot_structural_panes_callback_free_checked()?;
        anyhow::ensure!(
            tiled.len() == 1 && floating.is_empty() && Arc::ptr_eq(&tiled[0], pane),
            "tab {} does not contain exactly the admitted tiled pane",
            self.tab_id
        );
        let next_generation = inner.prepare_mux_owner_binding(mux)?;
        Ok(PreparedTabMuxOwnerBinding {
            tab: self,
            inner,
            mux: Arc::clone(mux),
            next_generation,
        })
    }

    pub(crate) fn active_mux_owner_generation(&self) -> Option<u64> {
        let generation = self.mux_owner_generation.load(Ordering::Acquire);
        (generation != 0).then_some(generation)
    }

    #[cfg(test)]
    pub(crate) fn has_active_mux_owner(&self, mux: &Mux) -> bool {
        self.inner.lock().is_active_mux_owner(mux)
    }

    pub(crate) fn is_structurally_empty_for_mux(&self, mux: &Mux) -> bool {
        let inner = self.inner.lock();
        inner.is_active_mux_owner(mux)
            && inner.snapshot_non_floating_panes_callback_free().is_empty()
            && inner.floating_panes.is_empty()
    }

    pub(crate) fn contains_exact_single_tiled_pane_for_mux(
        &self,
        mux: &Mux,
        pane: &Arc<dyn Pane>,
    ) -> bool {
        let inner = self.inner.lock();
        if !inner.is_active_mux_owner(mux) {
            return false;
        }
        inner
            .snapshot_structural_panes_callback_free_checked()
            .is_ok_and(|(tiled, floating)| {
                tiled.len() == 1 && floating.is_empty() && Arc::ptr_eq(&tiled[0], pane)
            })
    }

    #[cfg(test)]
    pub(crate) fn mux_owner_generation_for_test(&self) -> u64 {
        self.inner.lock().mux_owner_generation
    }

    /// Retire the active owner generation while the exact tab registration is
    /// still topology-locked. Detached holders can continue to edit the tab,
    /// but those edits no longer publish into the former mux.
    #[cfg(test)]
    pub(crate) fn retire_mux_owner(&self, mux: &Mux) -> bool {
        let retired = self.inner.lock().retire_mux_owner(mux);
        if retired {
            self.mux_owner_generation.store(0, Ordering::Release);
        }
        retired
    }

    pub fn get_title(&self) -> String {
        self.inner.lock().title.to_string()
    }

    pub fn set_title(&self, title: &str) {
        let (mux, notification) = {
            let mut inner = self.inner.lock();
            let mux = inner.notification_owner();
            if inner.title.as_ref() == title {
                return;
            }
            let title: Arc<str> = Arc::from(title);
            inner.title = Arc::clone(&title);
            let notification = mux.as_deref().map(|mux| {
                mux.envelope_notification(MuxNotification::TabTitleChanged {
                    tab_id: self.tab_id,
                    title: title.to_string(),
                })
            });
            (mux, notification)
        };
        if let (Some(mux), Some(notification)) = (mux, notification) {
            mux.dispatch_notification_envelope(notification);
        }
    }

    pub(crate) fn set_title_for_mux(
        &self,
        title: &str,
        mux: &Mux,
    ) -> (bool, Option<MuxNotificationEnvelope>) {
        let mut inner = self.inner.lock();
        if !inner.is_active_mux_owner(mux) || inner.title.as_ref() == title {
            return (false, None);
        }
        let title: Arc<str> = Arc::from(title);
        inner.title = Arc::clone(&title);
        let notification = Some(mux.envelope_notification(MuxNotification::TabTitleChanged {
            tab_id: self.tab_id,
            title: title.to_string(),
        }));
        (true, notification)
    }

    /// Called by the multiplexer client when building a local tab to
    /// mirror a remote tab.  The supplied `root` is the information
    /// about our counterpart in the the remote server.
    /// This method builds a local tree based on the remote tree which
    /// then replaces the local tree structure.
    ///
    /// The `make_pane` function is provided by the caller, and its purpose
    /// is to lookup an existing Pane that corresponds to the provided
    /// PaneEntry, or to create a new Pane from that entry.
    /// make_pane is expected to add the pane to the mux if it creates
    /// a new pane, otherwise the pane won't poll/update in the GUI.
    pub fn sync_with_pane_tree<F>(
        &self,
        size: TerminalSize,
        root: PaneNode,
        mut make_pane: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(PaneEntry) -> anyhow::Result<Arc<dyn Pane>>,
    {
        let mut active = None;
        let mut zoomed = None;
        log::debug!("sync_with_pane_tree with size {:?}", size);
        // `make_pane` is caller-supplied and may re-enter the mux. Build the
        // complete replacement tree before acquiring the tab topology lock.
        let tree =
            build_from_pane_tree(root.into_tree(), &mut active, &mut zoomed, &mut make_pane)?;
        self.sync_with_prepared_pane_tree(
            size,
            PreparedPaneTree {
                tree,
                active,
                zoomed,
            },
        )?;
        Ok(())
    }

    /// Install a pane tree prepared directly from a validated flat arena.
    ///
    /// Preparation may happen in reverse arena order while a client consumes
    /// one global node vector; installation can then remain in the snapshot's
    /// forward tab order.
    pub fn sync_with_prepared_pane_tree(
        &self,
        size: TerminalSize,
        prepared: PreparedPaneTree,
    ) -> anyhow::Result<()> {
        let desired_panes = prepared.snapshot_panes_callback_free();
        let mut desired_observed = Vec::new();
        desired_observed
            .try_reserve_exact(desired_panes.len())
            .map_err(|error| anyhow::anyhow!("reserve prepared pane identities: {error}"))?;
        for pane in &desired_panes {
            desired_observed.push((observe_pane_id_for_mutation(pane)?, Arc::clone(pane)));
        }
        let install = prepared.into_install(size, &desired_panes)?;
        let mut topology_notifications = Vec::new();
        topology_notifications
            .try_reserve_exact(1)
            .map_err(|error| anyhow::anyhow!("reserve prepared-tree topology envelope: {error}"))?;
        let expected_mux = self.inner.lock().notification_owner();
        let current = expected_mux
            .as_ref()
            .map(|_| self.snapshot_structural_states_for_authority())
            .transpose()?;
        let (mux, callbacks) = match expected_mux {
            Some(mux) => {
                let current = current.expect("bound tabs retain a structural snapshot");
                let prior_structural_count = current.len();
                let _registration = mux.pane_registration.lock();
                let mut authority = mux.pane_authority.lock();
                let registered_tabs = mux.tabs.read();
                let registered_tab = registered_tabs
                    .get(&self.tab_id)
                    .filter(|registered| std::ptr::eq(Arc::as_ptr(registered), self))
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "tab {} lost exact mux registration before prepared-tree installation",
                            self.tab_id
                        )
                    })?;
                let mut desired = Vec::new();
                desired
                    .try_reserve_exact(desired_observed.len())
                    .map_err(|error| {
                        anyhow::anyhow!("reserve desired prepared-tree authority: {error}")
                    })?;
                {
                    let panes = mux.panes.read();
                    for (pane_id, pane) in desired_observed {
                        let registered = panes.get(&pane_id).ok_or_else(|| {
                            anyhow::anyhow!(
                                "prepared tree pane {pane_id} is not registered in the owning mux"
                            )
                        })?;
                        anyhow::ensure!(
                            Arc::ptr_eq(&registered.pane, &pane),
                            "prepared tree pane id {pane_id} names another exact mux allocation"
                        );
                        let registration = PaneRegistrationHandle::new(
                            &registered.pane,
                            &registered.generation,
                        );
                        anyhow::ensure!(
                            authority.contains_live_registration(
                                pane_id,
                                registered.domain_id,
                                &registration,
                            ),
                            "prepared tree pane {pane_id} lacks exact domain-registration authority"
                        );
                        desired.push(DesiredPaneStructuralState {
                            pane_id,
                            pane,
                            lane: PaneStructuralLane::Tiled,
                            registration,
                            domain_id: registered.domain_id,
                        });
                    }
                }
                let next_structural_count = desired.len();
                let structural = authority.prepare_tab_structural_replacement(
                    Arc::clone(&registered_tab),
                    &current,
                    desired,
                )?;
                let mut windows = mux.windows.write();
                let tab_parents = mux.tab_parents.read();
                let mut workspace_counts = mux.num_panes_by_workspace.write();
                let prepared_counts = mux.prepare_tab_pane_count_mutation_locked(
                    &windows,
                    &tab_parents,
                    &mut workspace_counts,
                    &[(
                        Arc::clone(&registered_tab),
                        prior_structural_count,
                        next_structural_count,
                    )],
                    "prepared pane-tree replacement",
                )?;
                let mut inner = self.inner.lock();
                anyhow::ensure!(
                    inner.is_active_mux_owner(&mux),
                    "tab {} lost mux-owner authority before prepared-tree installation",
                    self.tab_id
                );
                let (current_tiled, current_floating) =
                    inner.snapshot_structural_panes_callback_free_checked()?;
                let expected_tiled = current
                    .iter()
                    .filter(|state| state.lane == PaneStructuralLane::Tiled)
                    .collect::<Vec<_>>();
                let expected_floating = current
                    .iter()
                    .filter(|state| state.lane == PaneStructuralLane::Floating)
                    .collect::<Vec<_>>();
                anyhow::ensure!(
                    current_tiled.len() == expected_tiled.len()
                        && current_tiled
                            .iter()
                            .zip(&expected_tiled)
                            .all(|(pane, expected)| Arc::ptr_eq(pane, &expected.pane))
                        && current_floating.len() == expected_floating.len()
                        && current_floating
                            .iter()
                            .zip(&expected_floating)
                            .all(|((pane_id, pane), expected)| {
                                *pane_id == expected.pane_id
                                    && Arc::ptr_eq(pane, &expected.pane)
                            }),
                    "tab {} topology changed during prepared-tree installation",
                    self.tab_id
                );
                if install.tag_active {
                    inner.recency.by_idx.try_reserve(1).map_err(|error| {
                        anyhow::anyhow!("reserve prepared-tree recency authority: {error}")
                    })?;
                }
                let mut topology = mux.topology.lock();
                let revision = topology.reserve_revision().map_err(anyhow::Error::new)?;
                topology_notifications.push(MuxNotificationEnvelope {
                    notification: MuxNotification::TabResized(self.tab_id),
                    topology: crate::MuxTopologyStamp::Revision(revision),
                });
                let mut callbacks = inner.commit_prepared_pane_tree_install(install);
                authority.commit_tab_structural_replacement(structural);
                prepared_counts.commit(&mut windows, &mut workspace_counts);
                callbacks.topology_notifications = topology_notifications;
                (Some(Arc::clone(&mux)), callbacks)
            }
            None => {
                let mut inner = self.inner.lock();
                anyhow::ensure!(
                    inner.notification_owner().is_none(),
                    "tab {} acquired mux-owner authority during prepared-tree installation",
                    self.tab_id
                );
                if install.tag_active {
                    inner.recency.by_idx.try_reserve(1).map_err(|error| {
                        anyhow::anyhow!("reserve prepared-tree recency state: {error}")
                    })?;
                }
                (None, inner.commit_prepared_pane_tree_install(install))
            }
        };
        callbacks.execute(mux.as_deref());
        Ok(())
    }

    /// Encode one coherent tab snapshot using caller-supplied owner metadata.
    ///
    /// The structural tree and focus identities are cloned under the tab lock;
    /// all potentially reentrant `Pane` observations happen after unlocking.
    /// A session therefore cannot accidentally consult a replacement process
    /// singleton while encoding an exact mux.
    pub fn codec_pane_tree_in_window(
        &self,
        window_id: WindowId,
        workspace: &str,
    ) -> anyhow::Result<PaneNode> {
        const SNAPSHOT_ATTEMPTS: usize = 3;

        for _ in 0..SNAPSHOT_ATTEMPTS {
            let observed = Self::observe_panes(self.snapshot_panes_callback_free());
            let pane_ids = build_callback_pane_id_snapshot(self.tab_id, &observed)?;

            let snapshot = {
                let inner = self.inner.lock();
                let current = inner.snapshot_panes_callback_free();
                if !callback_snapshot_matches(&current, &pane_ids)? {
                    None
                } else {
                    Some((
                        inner.pane.clone(),
                        inner.raw_active_pane_callback_free(&pane_ids),
                        inner.zoomed.as_ref().map(Arc::clone),
                    ))
                }
            };
            let Some((tree, active, zoomed)) = snapshot else {
                continue;
            };

            return Ok(match tree {
                Some(tree) => pane_tree(
                    &tree,
                    self.tab_id,
                    window_id,
                    active.as_ref(),
                    zoomed.as_ref(),
                    workspace,
                    0,
                    0,
                ),
                None => PaneNode::Empty,
            });
        }

        anyhow::bail!(
            "tab {} topology changed during all {SNAPSHOT_ATTEMPTS} codec snapshot attempts",
            self.tab_id,
        )
    }

    /// Append one callback-coherent tab directly to an ordered pane arena.
    ///
    /// Unlike [`Self::codec_pane_tree_in_window`], this path never constructs
    /// a recursive [`PaneNode`] or a temporary `Box` per split. The tab lock
    /// is held only while its existing mux tree is projected into an iterative
    /// callback-free capture; pane observations happen afterward. The caller
    /// supplies the protocol depth/node/leaf ceilings and a stable per-tab
    /// census work ceiling so the dependency-lower mux crate does not own wire
    /// policy.
    /// The census ceiling is intentionally independent of the arena prefix:
    /// the same tab cannot become invalid merely because earlier tabs consumed
    /// more of the whole-snapshot node budget.
    pub fn append_codec_pane_arena_in_window(
        &self,
        window_id: WindowId,
        workspace: &str,
        arena: &mut Vec<PaneArenaNode>,
        max_depth: usize,
        max_total_nodes: usize,
        max_census_work: usize,
    ) -> anyhow::Result<PaneArenaTree> {
        let mut ledger = PaneSnapshotCensusLedger::new(usize::MAX, usize::MAX)?;
        ledger.begin_attempt();
        Ok(self
            .append_codec_pane_arena_in_window_with_census_ledger(
                window_id,
                workspace,
                arena,
                max_depth,
                max_total_nodes,
                max_census_work,
                usize::MAX,
                &mut ledger,
            )?
            .tree)
    }

    /// Append one ordered pane tree while charging one request-scoped work
    /// ledger shared by every tab and every coherence retry.
    pub fn append_codec_pane_arena_in_window_with_census_ledger(
        &self,
        window_id: WindowId,
        workspace: &str,
        arena: &mut Vec<PaneArenaNode>,
        max_depth: usize,
        max_total_nodes: usize,
        max_census_work: usize,
        max_tree_leaves: usize,
        ledger: &mut PaneSnapshotCensusLedger,
    ) -> anyhow::Result<PaneArenaAppendReceipt> {
        let mut metadata_ledger =
            PaneSnapshotMetadataLedger::new(PaneSnapshotMetadataLimits::unbounded())?;
        metadata_ledger.begin_attempt();
        self.append_codec_pane_arena_in_window_with_ledgers(
            window_id,
            workspace,
            arena,
            max_depth,
            max_total_nodes,
            max_census_work,
            max_tree_leaves,
            ledger,
            &mut metadata_ledger,
        )
    }

    /// Append one ordered pane tree while charging the shared work and UTF-8
    /// metadata authorities. The leaf allowance is enforced in the
    /// callback-free census before any pane method runs. Neither ledger is
    /// reset here: callers own the attempt boundary so all tabs and internal
    /// coherence retries share it.
    pub fn append_codec_pane_arena_in_window_with_ledgers(
        &self,
        window_id: WindowId,
        workspace: &str,
        arena: &mut Vec<PaneArenaNode>,
        max_depth: usize,
        max_total_nodes: usize,
        max_census_work: usize,
        max_tree_leaves: usize,
        ledger: &mut PaneSnapshotCensusLedger,
        metadata_ledger: &mut PaneSnapshotMetadataLedger,
    ) -> anyhow::Result<PaneArenaAppendReceipt> {
        const SNAPSHOT_ATTEMPTS: usize = 3;
        let work_before = ledger.attempt_stats();
        let arena_start = arena.len();
        let max_tree_nodes = max_total_nodes.checked_sub(arena_start).ok_or(
            PaneSnapshotStructureRejection::TreeNodeLimit {
                count: arena_start,
                max: max_total_nodes,
            },
        )?;

        for _ in 0..SNAPSHOT_ATTEMPTS {
            let metadata_checkpoint = metadata_ledger.attempt_checkpoint();
            let callback_free_snapshot = self.inner.lock().snapshot_panes_callback_free_bounded(
                max_depth,
                max_tree_nodes,
                max_census_work,
                max_tree_leaves,
                ledger,
            )?;
            let BoundedCallbackFreePaneCensus {
                panes,
                tree_leaf_count,
                coherence,
                stats: preflight_stats,
                ..
            } = callback_free_snapshot;
            debug_assert!(preflight_stats.total().is_some());
            // Keep callback failures provisional until the final callback-free
            // census proves that the callbacks did not replace or rearrange
            // the topology/focus authority that they were observing.
            let observed = observe_ordered_panes_bounded(
                self.tab_id,
                panes,
                tree_leaf_count,
                workspace,
                ledger,
                metadata_ledger,
            );

            let captured = {
                let inner = self.inner.lock();
                let current = match inner.snapshot_panes_callback_free_bounded(
                    max_depth,
                    max_tree_nodes,
                    max_census_work,
                    max_tree_leaves,
                    ledger,
                ) {
                    Ok(current) => current,
                    // A callback may transiently replace valid topology with
                    // state that fails preflight. It is not authoritative
                    // until a subsequent attempt observes it before invoking
                    // pane code, so retry instead of leaking this post-callback
                    // error across the coherence fence.
                    Err(_) => {
                        drop(observed);
                        metadata_ledger.release_attempt_to(metadata_checkpoint)?;
                        continue;
                    }
                };
                if current.coherence != coherence {
                    drop(observed);
                    metadata_ledger.release_attempt_to(metadata_checkpoint)?;
                    continue;
                }

                let observed = observed?;
                if !callback_snapshot_matches_bounded(&current.panes, &observed.pane_ids, ledger)? {
                    drop(observed);
                    metadata_ledger.release_attempt_to(metadata_checkpoint)?;
                    continue;
                }
                {
                    let active = inner.raw_active_pane_callback_free_with_tree_active(
                        &observed.pane_ids,
                        current.tree_active,
                    );
                    ledger.reserve(
                        PaneSnapshotCensusKind::AssemblyNode,
                        current.coherence.tree.len(),
                    )?;
                    metadata_ledger.preflight_required_string(
                        PaneSnapshotMetadataField::TabTitle,
                        &inner.title,
                    )?;
                    let tab_title = inner.title.to_string();
                    metadata_ledger.admit_required_owned(
                        PaneSnapshotMetadataField::TabTitle,
                        &tab_title,
                        tab_title.capacity(),
                    )?;
                    let captured = capture_pane_arena_tree(
                        inner.pane.as_ref(),
                        Some(active),
                        inner.zoomed.as_ref().map(Arc::clone),
                        &observed.pane_ids,
                        tab_title,
                        arena.len(),
                        max_depth,
                        max_total_nodes,
                    )?;
                    Some((captured, observed.tree_entries))
                }
            };
            let Some(((captured, active, zoomed, tab_title), mut tree_entries)) = captured else {
                continue;
            };

            debug_assert_eq!(arena.len(), arena_start);
            let node_count = captured.len();
            let root_index = if node_count == 0 {
                None
            } else {
                Some(
                    u32::try_from(arena_start)
                        .map_err(|_| anyhow::anyhow!("ordered pane arena root exceeds u32"))?,
                )
            };
            let node_count = u32::try_from(node_count)
                .map_err(|_| anyhow::anyhow!("ordered pane arena tree exceeds u32"))?;
            let arena_end = arena_start
                .checked_add(captured.len())
                .ok_or_else(|| anyhow::anyhow!("ordered pane arena length overflows usize"))?;
            if arena_end > (u32::MAX as usize).saturating_add(1) {
                anyhow::bail!("ordered pane arena final node exceeds u32");
            }
            arena
                .try_reserve_exact(captured.len())
                .map_err(|error| anyhow::anyhow!("reserve ordered pane arena nodes: {error}"))?;
            let mut append = VecAppendRollback::new(arena);
            for node in captured {
                append.vector().push(match node {
                    CapturedPaneArenaNode::Empty => PaneArenaNode::Empty,
                    CapturedPaneArenaNode::Split { left, right, node } => PaneArenaNode::Split {
                        left: u32::try_from(arena_start.checked_add(left).ok_or_else(|| {
                            anyhow::anyhow!("ordered pane arena left-child index overflows")
                        })?)
                        .map_err(|_| anyhow::anyhow!("ordered pane arena left child exceeds u32"))?,
                        right: u32::try_from(arena_start.checked_add(right).ok_or_else(|| {
                            anyhow::anyhow!("ordered pane arena right-child index overflows")
                        })?)
                        .map_err(|_| anyhow::anyhow!("ordered pane arena right child exceeds u32"))?,
                        node,
                    },
                    CapturedPaneArenaNode::Leaf {
                        identity,
                        pane_id,
                        left_col,
                        top_row,
                    } => {
                        let observed = tree_entries.remove(&identity).ok_or_else(|| {
                            anyhow::anyhow!(
                                "exact tree-pane identity {identity:p} lacks its pre-fence ordered observation"
                            )
                        })?;
                        if observed.pane_id != pane_id {
                            anyhow::bail!(
                                "exact tree-pane identity {identity:p} changed numeric pane id from {} to {pane_id} during callback-free assembly",
                                observed.pane_id
                            );
                        }
                        PaneArenaNode::Leaf(pane_entry_from_ordered_observation(
                            observed,
                            self.tab_id,
                            window_id,
                            active
                                .as_ref()
                                .is_some_and(|pane| pane_identity(pane) == identity),
                            zoomed
                                .as_ref()
                                .is_some_and(|pane| pane_identity(pane) == identity),
                            left_col,
                            top_row,
                        ))
                    }
                });
            }
            append.commit();
            return Ok(PaneArenaAppendReceipt {
                tree: PaneArenaTree {
                    root_index,
                    node_count,
                    tab_title,
                },
                leaf_count: tree_leaf_count,
                work: ledger.attempt_stats().checked_delta(work_before)?,
            });
        }

        anyhow::bail!(
            "tab {} topology changed during all {SNAPSHOT_ATTEMPTS} flat codec snapshot attempts",
            self.tab_id,
        )
    }

    /// Returns a count of how many panes are in this tab
    pub fn count_panes(&self) -> Option<usize> {
        self.inner.try_lock().map(|mut inner| inner.count_panes())
    }

    /// Sets the zoom state, returns the prior state
    pub fn set_zoomed(&self, zoomed: bool) -> bool {
        let (mux, prior, callbacks) = {
            let mut inner = self.inner.lock();
            let mux = inner.notification_owner();
            let (prior, mut callbacks) = inner.prepare_set_zoomed(zoomed);
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            (mux, prior, callbacks)
        };
        callbacks.execute(mux.as_deref());
        prior
    }

    pub fn toggle_zoom(&self) {
        let (mux, callbacks) = {
            let mut inner = self.inner.lock();
            let mux = inner.notification_owner();
            let mut callbacks = inner.prepare_toggle_zoom();
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            (mux, callbacks)
        };
        callbacks.execute(mux.as_deref());
    }

    pub fn contains_pane(&self, pane: PaneId) -> bool {
        self.inner.lock().contains_pane(pane)
    }

    pub fn iter_panes(&self) -> Vec<PositionedPane> {
        self.inner.lock().iter_panes()
    }

    pub fn iter_panes_ignoring_zoom(&self) -> Vec<PositionedPane> {
        self.inner.lock().iter_panes_ignoring_zoom()
    }

    /// Returns every logical pane owned by this tab exactly once, including
    /// hidden stack members and floating panes.
    pub fn iter_all_panes(&self) -> Vec<Arc<dyn Pane>> {
        self.snapshot_panes_callback_free()
    }

    pub(crate) fn snapshot_structural_states_for_authority(
        &self,
    ) -> anyhow::Result<Vec<ExactPaneStructuralState>> {
        let (tiled, floating) = {
            let inner = self.inner.lock();
            inner.snapshot_structural_panes_callback_free_checked()?
        };
        let capacity = tiled
            .len()
            .checked_add(floating.len())
            .ok_or_else(|| anyhow::anyhow!("tab structural snapshot count overflow"))?;
        let mut states = Vec::new();
        states
            .try_reserve_exact(capacity)
            .map_err(|error| anyhow::anyhow!("reserve tab structural snapshot: {error}"))?;
        for pane in tiled {
            states.push(ExactPaneStructuralState {
                pane_id: observe_pane_id_for_mutation(&pane)?,
                pane,
                lane: PaneStructuralLane::Tiled,
            });
        }
        for (stored_id, pane) in floating {
            let pane_id = observe_pane_id_for_mutation(&pane)?;
            anyhow::ensure!(
                pane_id == stored_id,
                "floating pane stored id {stored_id} disagrees with exact pane id {pane_id}"
            );
            states.push(ExactPaneStructuralState {
                pane_id,
                pane,
                lane: PaneStructuralLane::Floating,
            });
        }
        Ok(states)
    }

    pub fn add_floating_pane(
        &self,
        pane: Arc<dyn Pane>,
        rect: FloatingPaneRect,
    ) -> anyhow::Result<PositionedFloatingPane> {
        const ADMISSION_ATTEMPTS: usize = 3;
        let pane_id = observe_pane_id_for_mutation(&pane)?;

        for _ in 0..ADMISSION_ATTEMPTS {
            let (expected_mux, expected_tiled, expected_floating) = {
                let inner = self.inner.lock();
                let expected_mux = inner.notification_owner();
                let (expected_tiled, expected_floating) =
                    inner.snapshot_structural_panes_callback_free_checked()?;
                (expected_mux, expected_tiled, expected_floating)
            };
            let snapshot_len = expected_tiled
                .len()
                .checked_add(expected_floating.len())
                .ok_or_else(|| anyhow::anyhow!("floating-pane snapshot count overflow"))?;
            let mut snapshot = Vec::new();
            snapshot
                .try_reserve_exact(snapshot_len)
                .map_err(|error| anyhow::anyhow!("reserve floating-pane snapshot: {error}"))?;
            snapshot.extend(expected_tiled.iter().cloned());
            snapshot.extend(
                expected_floating
                    .iter()
                    .map(|(_, floating)| Arc::clone(floating)),
            );
            let observed = Self::observe_panes(snapshot.clone());
            if observed.iter().any(|candidate| candidate.pane_id.is_none()) {
                anyhow::bail!(
                    "cannot prove floating-pane id uniqueness because an existing pane identity callback panicked"
                );
            }
            if observed
                .iter()
                .any(|candidate| candidate.pane_id == Some(pane_id))
            {
                anyhow::bail!(
                    "pane {pane_id} is already present in tab {}; floating panes require a detached pane",
                    self.tab_id
                );
            }

            let (mux, positioned, callbacks, retired_floating) = match expected_mux {
                Some(mux) => {
                    let _registration = mux.pane_registration.lock();
                    let mut authority = mux.pane_authority.lock();
                    let registered_tabs = mux.tabs.read();
                    let registered_tab = registered_tabs
                        .get(&self.tab_id)
                        .filter(|tab| std::ptr::eq(Arc::as_ptr(tab), self))
                        .cloned()
                        .ok_or_else(|| {
                        anyhow::anyhow!(
                            "tab {} lost exact mux registration during floating-pane admission",
                            self.tab_id
                        )
                    })?;
                    let tab_mux_owner_generation = registered_tab
                        .active_mux_owner_generation()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "tab {} lacks active mux-owner generation during floating-pane admission",
                                self.tab_id
                            )
                        })?;
                    let (pane_registration, domain_id) = {
                        let panes = mux.panes.read();
                        let registered = panes.get(&pane_id).ok_or_else(|| {
                            anyhow::anyhow!(
                                "floating pane {pane_id} is not registered in the owning mux"
                            )
                        })?;
                        anyhow::ensure!(
                            Arc::ptr_eq(&registered.pane, &pane),
                            "floating pane id {pane_id} names another exact mux allocation"
                        );
                        (
                            PaneRegistrationHandle::new(
                                &registered.pane,
                                &registered.generation,
                            ),
                            registered.domain_id,
                        )
                    };
                    anyhow::ensure!(
                        authority.contains_live_registration(
                            pane_id,
                            domain_id,
                            &pane_registration,
                        ),
                        "floating pane {pane_id} lacks exact domain-registration authority"
                    );
                    let structural = authority.prepare_new_structural_bind(
                        pane_id,
                        Arc::clone(&pane),
                        Arc::clone(&registered_tab),
                        PaneStructuralLane::Floating,
                        Some((pane_registration, domain_id)),
                    )?;
                    let mut windows = mux.windows.write();
                    let parents = mux.tab_parents.read();
                    let parent = parents.get(&self.tab_id).ok_or_else(|| {
                        anyhow::anyhow!(
                            "tab {} lacks exact window-parent authority during floating-pane admission",
                            self.tab_id
                        )
                    })?;
                    anyhow::ensure!(
                        parent.is_same_tab(&registered_tab),
                        "tab {} window-parent authority names another exact generation",
                        self.tab_id
                    );
                    let window = windows.get(&parent.window_id).ok_or_else(|| {
                        anyhow::anyhow!(
                            "tab {} parent window {} is absent during floating-pane admission",
                            self.tab_id,
                            parent.window_id
                        )
                    })?;
                    anyhow::ensure!(
                        parent.matches(&registered_tab, parent.window_id)
                            && window
                                .iter()
                                .filter(|candidate| Arc::ptr_eq(candidate, &registered_tab))
                                .count()
                                == 1,
                        "tab {} exact window-parent authority disagrees with window membership",
                        self.tab_id
                    );
                    let mut workspace_counts = mux.num_panes_by_workspace.write();
                    let prior_structural_count = expected_tiled
                        .len()
                        .checked_add(expected_floating.len())
                        .ok_or_else(|| anyhow::anyhow!("floating-pane tab count overflow"))?;
                    let next_structural_count = prior_structural_count
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("floating-pane tab count overflow"))?;
                    anyhow::ensure!(
                        Mux::exact_tab_structural_pane_count(&authority, &registered_tab)?
                            == prior_structural_count,
                        "tab {} structural authority changed during floating-pane admission",
                        self.tab_id
                    );
                    let prepared_counts = mux.prepare_tab_pane_count_mutation_locked(
                        &windows,
                        &parents,
                        &mut workspace_counts,
                        &[(
                            Arc::clone(&registered_tab),
                            prior_structural_count,
                            next_structural_count,
                        )],
                        "floating-pane admission",
                    )?;
                    let mut inner = self.inner.lock();
                    anyhow::ensure!(
                        inner.is_active_mux_owner(&mux),
                        "tab {} lost mux-owner authority during floating-pane admission",
                        self.tab_id
                    );
                    let (current_tiled, current_floating) =
                        inner.snapshot_structural_panes_callback_free_checked()?;
                    if current_tiled.len() != expected_tiled.len()
                        || !current_tiled
                            .iter()
                            .zip(&expected_tiled)
                            .all(|(current, expected)| Arc::ptr_eq(current, expected))
                        || current_floating.len() != expected_floating.len()
                        || !current_floating.iter().zip(&expected_floating).all(
                            |((current_id, current), (expected_id, expected))| {
                                current_id == expected_id && Arc::ptr_eq(current, expected)
                            },
                        )
                    {
                        continue;
                    }
                    let current_tab_count = current_tiled
                        .len()
                        .checked_add(current_floating.len())
                        .ok_or_else(|| anyhow::anyhow!("floating-pane tab count overflow"))?;
                    anyhow::ensure!(
                        current_tab_count == prior_structural_count,
                        "tab {} structural count changed during floating-pane admission",
                        self.tab_id
                    );
                    let mut prepared =
                        inner.prepare_add_floating_pane(Arc::clone(&pane), pane_id, rect)?;
                    prepared.callbacks.topology_notifications = prepared
                        .callbacks
                        .prepare_topology_notifications(&mux, self.tab_id)?;
                    let (positioned, callbacks, retired_floating) =
                        inner.commit_prepared_floating_pane_addition(prepared);
                    authority
                        .commit_structural_bind(structural, tab_mux_owner_generation);
                    prepared_counts.commit(&mut windows, &mut workspace_counts);
                    (
                        Some(Arc::clone(&mux)),
                        positioned,
                        callbacks,
                        retired_floating,
                    )
                }
                None => {
                    let mut inner = self.inner.lock();
                    if inner.notification_owner().is_some() {
                        continue;
                    }
                    let (current_tiled, current_floating) =
                        inner.snapshot_structural_panes_callback_free_checked()?;
                    if current_tiled.len() != expected_tiled.len()
                        || !current_tiled
                            .iter()
                            .zip(&expected_tiled)
                            .all(|(current, expected)| Arc::ptr_eq(current, expected))
                        || current_floating.len() != expected_floating.len()
                        || !current_floating.iter().zip(&expected_floating).all(
                            |((current_id, current), (expected_id, expected))| {
                                current_id == expected_id && Arc::ptr_eq(current, expected)
                            },
                        )
                    {
                        continue;
                    }
                    let prepared =
                        inner.prepare_add_floating_pane(Arc::clone(&pane), pane_id, rect)?;
                    let (positioned, callbacks, retired_floating) =
                        inner.commit_prepared_floating_pane_addition(prepared);
                    (None, positioned, callbacks, retired_floating)
                }
            };
            drop(retired_floating);
            callbacks.execute(mux.as_deref());
            return Ok(positioned);
        }

        anyhow::bail!(
            "tab {} topology changed during all {ADMISSION_ATTEMPTS} floating-pane admission attempts",
            self.tab_id
        )
    }

    /// Freeze the initial geometry for an unpublished floating-pane spawn.
    pub(crate) fn prepare_floating_spawn_geometry(
        &self,
        requested: FloatingPaneRect,
    ) -> PreparedFloatingPaneGeometry {
        let inner = self.inner.lock();
        let rect = inner.clamp_floating_rect(requested);
        PreparedFloatingPaneGeometry {
            rect,
            size: inner.floating_pane_size(rect),
        }
    }

    /// Publish a prepared pane registration and attach it to this exact
    /// floating layer in one structural cut.
    ///
    /// All pane callbacks and fallible reader preparation finish first. The
    /// final lock order is `domain_registration -> pane_registration ->
    /// pane_authority -> tabs -> windows -> Tab::inner (stable pointer order)
    /// -> pane_preparations -> panes -> clients -> pending_pane_lifecycle ->
    /// topology`. No callback or subscriber runs inside that cut, and a
    /// rejected commit emits no pane lifecycle edge.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit_unpublished_floating_pane(
        self: &Arc<Self>,
        mux: &Arc<Mux>,
        expected_domain: &Arc<dyn Domain>,
        expected_domain_id: DomainId,
        expected_window_id: Option<WindowId>,
        target_registration: &PaneRegistrationHandle,
        unpublished: UnpublishedPane,
        geometry: PreparedFloatingPaneGeometry,
        owner_client_id: Option<&Arc<ClientId>>,
    ) -> anyhow::Result<(
        Arc<dyn Pane>,
        PositionedFloatingPane,
        PaneRegistrationHandle,
    )> {
        let pane = Arc::clone(unpublished.pane());
        anyhow::ensure!(
            target_registration
                .owner()
                .is_some_and(|owner| Arc::ptr_eq(&owner, mux)),
            "floating-pane target belongs to another mux registration"
        );

        let mut preparation_claim = mux
            .claim_pane_preparation(&pane)?
            .ok_or_else(|| anyhow::anyhow!("unpublished floating pane is already registered"))?;
        anyhow::ensure!(
            target_registration.pane_id() != preparation_claim.pane_id,
            "cannot attach floating pane {} onto itself",
            target_registration.pane_id()
        );
        let prepared = mux.prepare_claimed_pane_registration(
            &pane,
            preparation_claim.pane_id,
            &preparation_claim.generation,
        )?;
        let (mut reader_start_gate, registration_reservation) =
            mux.spawn_prepared_pane_reader(&pane, prepared, &preparation_claim.generation)?;
        let spawned_id = preparation_claim.pane_id;

        // Capture every registered or window-owned tab only after all
        // callback-bearing pane preparation. Numeric ID observations likewise
        // happen outside every mux/tab lock. The final combined lock cut below
        // proves these exact pointer snapshots did not change.
        let mut observed_tabs = mux.tabs.read().values().cloned().collect::<Vec<_>>();
        let windows = mux.windows.read();
        let window_tab_count = windows.values().try_fold(0usize, |count, window| {
            count
                .checked_add(window.len())
                .ok_or_else(|| anyhow::anyhow!("observed window tab count overflow"))
        })?;
        let observed_tab_capacity = observed_tabs
            .len()
            .checked_add(window_tab_count)
            .ok_or_else(|| anyhow::anyhow!("observed tab capacity overflow"))?;
        observed_tabs
            .try_reserve(window_tab_count)
            .map_err(|error| anyhow::anyhow!("reserve observed window tabs: {error}"))?;
        let mut observed_tab_identities = HashSet::new();
        observed_tab_identities
            .try_reserve(observed_tab_capacity)
            .map_err(|error| anyhow::anyhow!("reserve observed tab identities: {error}"))?;
        for tab in &observed_tabs {
            anyhow::ensure!(
                observed_tab_identities.insert(Arc::as_ptr(tab) as usize),
                "floating-pane admission observed a duplicate registered tab identity"
            );
        }
        for tab in windows.values().flat_map(|window| window.iter().cloned()) {
            if observed_tab_identities.insert(Arc::as_ptr(&tab) as usize) {
                observed_tabs.push(tab);
            }
        }
        drop(windows);
        observed_tabs.sort_unstable_by_key(|tab| Arc::as_ptr(tab) as usize);
        anyhow::ensure!(
            observed_tabs
                .windows(2)
                .all(|pair| !Arc::ptr_eq(&pair[0], &pair[1])),
            "floating-pane admission observed duplicate exact tab identities"
        );

        let mut observed_snapshots = Vec::new();
        observed_snapshots
            .try_reserve_exact(observed_tabs.len())
            .map_err(|error| anyhow::anyhow!("reserve observed pane snapshots: {error}"))?;
        observed_snapshots.extend(
            observed_tabs
                .iter()
                .map(|tab| tab.snapshot_panes_callback_free()),
        );
        let observed_pane_count = observed_snapshots.iter().try_fold(0usize, |count, panes| {
            count
                .checked_add(panes.len())
                .ok_or_else(|| anyhow::anyhow!("observed pane count overflow"))
        })?;
        let mut numeric_owners = HashMap::new();
        numeric_owners
            .try_reserve(observed_pane_count)
            .map_err(|error| anyhow::anyhow!("reserve numeric pane-owner census: {error}"))?;
        let mut exact_owners = HashSet::new();
        exact_owners
            .try_reserve(observed_pane_count)
            .map_err(|error| anyhow::anyhow!("reserve exact pane-owner census: {error}"))?;
        let mut target_owners = 0usize;
        let mut spawned_owners = 0usize;
        for (tab, snapshot) in observed_tabs.iter().zip(&observed_snapshots) {
            for observed in snapshot {
                let pane_id = observe_pane_id_for_mutation(observed).map_err(|error| {
                    error.context(format!(
                        "audit tab {} for floating-pane structural ownership",
                        tab.tab_id
                    ))
                })?;
                let identity = pane_identity(observed);
                anyhow::ensure!(
                    exact_owners.insert(identity),
                    "an exact pane identity has multiple structural owners before floating spawn"
                );
                if let Some(prior) = numeric_owners.insert(pane_id, identity) {
                    anyhow::ensure!(
                        prior == identity,
                        "pane id {pane_id} has multiple exact structural owners before floating spawn"
                    );
                }
                target_owners += usize::from(target_registration.is_same_pane(observed));
                spawned_owners += usize::from(Arc::ptr_eq(&pane, observed));
                anyhow::ensure!(
                    pane_id != spawned_id,
                    "unpublished pane id {spawned_id} already has a structural owner"
                );
            }
        }
        anyhow::ensure!(
            target_owners == 1
                && observed_tabs
                    .iter()
                    .zip(&observed_snapshots)
                    .any(|(tab, panes)| {
                        Arc::ptr_eq(tab, self)
                            && panes
                                .iter()
                                .any(|pane| target_registration.is_same_pane(pane))
                    }),
            "floating-pane target must have exactly one structural owner in the admitted tab"
        );
        anyhow::ensure!(spawned_owners == 0, "unpublished pane already has an owner");

        let (positioned, callbacks, retired_floating, registration, lifecycle_ticket) = {
            let _domain_registration = mux.domain_registration.lock();
            anyhow::ensure!(
                !mux.retired_domain_ids.lock().contains(&expected_domain_id)
                    && mux
                        .domains
                        .read()
                        .get(&expected_domain_id)
                        .is_some_and(|domain| Arc::ptr_eq(domain, expected_domain)),
                "floating-pane domain retired or changed identity before commit"
            );

            let _registration = mux.pane_registration.lock();
            let mut authority = mux.pane_authority.lock();
            let registered_tabs = mux.tabs.read();
            anyhow::ensure!(
                registered_tabs
                    .get(&self.tab_id)
                    .is_some_and(|registered| Arc::ptr_eq(registered, self)),
                "destination tab {} retired or changed identity before floating-pane commit",
                self.tab_id
            );
            let tab_mux_owner_generation = self.active_mux_owner_generation().ok_or_else(|| {
                anyhow::anyhow!(
                    "destination tab {} lacks active mux-owner generation before floating-pane commit",
                    self.tab_id
                )
            })?;
            anyhow::ensure!(
                registered_tabs.len()
                    == observed_tabs
                        .iter()
                        .filter(|tab| {
                            registered_tabs
                                .get(&tab.tab_id)
                                .is_some_and(|registered| Arc::ptr_eq(registered, tab))
                        })
                        .count(),
                "registered tab set changed during floating-pane spawn"
            );

            let mut windows = mux.windows.write();
            anyhow::ensure!(
                windows
                    .values()
                    .flat_map(|window| window.iter())
                    .all(|tab| observed_tab_identities.contains(&(Arc::as_ptr(tab) as usize))),
                "window tab set changed during floating-pane spawn"
            );
            let tab_parents = mux.tab_parents.read();
            anyhow::ensure!(
                tab_parents
                    .get(&self.tab_id)
                    .is_some_and(|parent| parent.matches(self, expected_window_id))
                    && windows
                        .get(&expected_window_id)
                        .is_some_and(|window| window.iter().any(|tab| Arc::ptr_eq(tab, self))),
                "destination tab {} no longer has exactly one admitted window parent",
                self.tab_id
            );
            let mut workspace_counts = mux.num_panes_by_workspace.write();
            let destination_is_active = windows
                .get(&expected_window_id)
                .and_then(|window| window.get_active())
                .is_some_and(|active| Arc::ptr_eq(active, self));
            let target_pane = {
                let panes = mux.panes.read();
                let registered = panes
                    .get(&target_registration.pane_id())
                    .filter(|registered| {
                        target_registration.matches_live_registration(registered)
                    })
                    .ok_or_else(|| {
                        anyhow::anyhow!("floating-pane target registration retired before commit")
                    })?;
                Arc::clone(&registered.pane)
            };
            authority.validate_structural_owner_exact(
                target_registration.pane_id(),
                &target_pane,
                self,
                None,
                Some(target_registration),
            )?;

            let registration =
                PaneRegistrationHandle::new(&pane, &preparation_claim.generation);
            let prepared_authority = authority.prepare_live_registration_insert(
                spawned_id,
                &pane,
                expected_domain_id,
                Some(expected_domain),
            )?;
            let structural = authority.prepare_new_structural_bind(
                spawned_id,
                Arc::clone(&pane),
                Arc::clone(self),
                PaneStructuralLane::Floating,
                Some((registration.clone(), expected_domain_id)),
            )?;

            let destination_index = observed_tabs
                .iter()
                .position(|tab| Arc::ptr_eq(tab, self))
                .ok_or_else(|| anyhow::anyhow!("destination tab left the observed topology"))?;
            let current_structural_count = observed_snapshots[destination_index].len();
            let final_structural_count = current_structural_count
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("floating destination pane count overflow"))?;
            anyhow::ensure!(
                Mux::exact_tab_structural_pane_count(&authority, self)?
                    == current_structural_count,
                "floating destination structural authority changed before commit"
            );
            let prepared_counts = mux.prepare_tab_pane_count_mutation_locked(
                &windows,
                &tab_parents,
                &mut workspace_counts,
                &[(Arc::clone(self), current_structural_count, final_structural_count)],
                "floating pane publication",
            )?;

            let mut tab_guards = observed_tabs
                .iter()
                .map(|tab| tab.inner.lock())
                .collect::<Vec<_>>();
            for (guard, observed) in tab_guards.iter().zip(&observed_snapshots) {
                let current = guard.snapshot_panes_callback_free();
                anyhow::ensure!(
                    current.len() == observed.len()
                        && current
                            .iter()
                            .zip(observed)
                            .all(|(current, observed)| Arc::ptr_eq(current, observed)),
                    "tab topology changed during floating-pane spawn"
                );
            }
            anyhow::ensure!(
                tab_guards[destination_index]
                    .snapshot_panes_callback_free()
                    .iter()
                    .filter(|pane| target_registration.is_same_pane(pane))
                    .count()
                    == 1,
                "floating-pane target left or duplicated inside the destination tab"
            );
            anyhow::ensure!(
                tab_guards.iter().all(|guard| guard
                    .snapshot_panes_callback_free()
                    .iter()
                    .all(|candidate| !Arc::ptr_eq(candidate, &pane))),
                "unpublished floating pane acquired a structural owner before commit"
            );
            let inner = &mut tab_guards[destination_index];
            anyhow::ensure!(
                preparation_claim.is_authoritative_locked(),
                "unpublished floating-pane preparation was cancelled"
            );

            let mut panes = mux.panes.write();
            anyhow::ensure!(
                panes
                    .get(&target_registration.pane_id())
                    .is_some_and(|registered| {
                        target_registration.matches_live_registration(registered)
                    }),
                "floating-pane target registration retired before commit"
            );
            anyhow::ensure!(
                !panes.contains_key(&spawned_id),
                "floating-pane id {spawned_id} became registered before commit"
            );
            panes
                .try_reserve(1)
                .map_err(|error| anyhow::anyhow!("reserve floating-pane registry slot: {error}"))?;

            let mut clients = owner_client_id.map(|_| mux.clients.write());
            let client_info = match owner_client_id {
                Some(client_id) => Some(
                    clients
                        .as_mut()
                        .and_then(|clients| clients.get_mut(client_id.as_ref()))
                        .filter(|info| Arc::ptr_eq(&info.client_id, client_id))
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "client registration retired before floating-pane commit"
                            )
                        })?,
                ),
                None => None,
            };
            let target_is_active = inner
                .raw_active_pane_retained_id()
                .is_some_and(|active| target_registration.is_same_pane(&active));
            let client_focus_is_current =
                client_info
                    .as_ref()
                    .is_none_or(|info| match info.focused_pane_registration() {
                        Some(focused) => focused.same_registration(target_registration),
                        None => info
                            .focused_pane_id
                            .is_none_or(|pane_id| pane_id == target_registration.pane_id()),
                    });
            // Zoom is an explicit exclusive-view state. Preserve it and attach
            // the new float unfocused rather than claiming focus that active
            // pane resolution would still give to the zoomed pane.
            let focus = destination_is_active
                && target_is_active
                && client_focus_is_current
                && inner.zoomed.is_none();
            let current_rect = inner.clamp_floating_rect(geometry.rect);
            anyhow::ensure!(
                current_rect == geometry.rect
                    && inner.floating_pane_size(current_rect) == geometry.size,
                "destination geometry changed while floating pane was spawning"
            );
            let prepared_floating = inner.prepare_add_presized_floating_pane(
                Arc::clone(&pane),
                spawned_id,
                geometry.rect,
                focus,
            )?;
            let lifecycle_enqueue = mux.prepare_pane_lifecycle_enqueue(spawned_id)?;

            let mut topology = mux.topology.lock();
            let topology_stamp = crate::MuxTopologyStamp::Revision(
                topology.reserve_revision().map_err(anyhow::Error::new)?,
            );

            // Every operation after topology reservation is an infallible,
            // allocation-free commit of state that was fully preflighted
            // above. The reservation is exclusive, so losing it here is an
            // internal invariant violation rather than a recoverable race.
            let commit_guard = registration_reservation
                .commit()
                .expect("validated exclusive pane registration reservation must commit");
            let prior = panes.insert(
                spawned_id,
                crate::LivePaneRegistration {
                    pane: Arc::clone(&pane),
                    generation: Arc::clone(&preparation_claim.generation),
                    domain_id: expected_domain_id,
                },
            );
            debug_assert!(prior.is_none());
            let (positioned, callbacks, retired_floating) =
                inner.commit_prepared_floating_pane_addition(prepared_floating);
            authority.insert_live_registration(
                spawned_id,
                &pane,
                expected_domain_id,
                registration.clone(),
                prepared_authority,
            );
            authority.commit_structural_bind(structural, tab_mux_owner_generation);
            prepared_counts.commit(&mut windows, &mut workspace_counts);
            if focus {
                if let Some(info) = client_info {
                    info.replace_focused_pane(spawned_id, Some(registration.clone()));
                }
            }
            let frozen = FrozenFloatingPaneSpawn::from_exact_parts(
                registration.clone(),
                self.tab_id,
                expected_window_id,
                &positioned,
            );
            let lifecycle_ticket = lifecycle_enqueue.enqueue(
                crate::PaneLifecycleNotification::FloatingSpawnCommitted(frozen),
                topology_stamp,
                reader_start_gate.take(),
                None,
                crate::PaneRemovalFollowUp::None,
            );
            let finalized = commit_guard.finalize();
            debug_assert!(registration.same_registration(&finalized));
            preparation_claim.retire_locked();
            (
                positioned,
                callbacks,
                retired_floating,
                registration,
                lifecycle_ticket,
            )
        };

        // The exact registration and structural owner are now committed. Disarm
        // the unpublished kill guard before any external callback can unwind;
        // rollback after this linearization point belongs to normal mux pane
        // retirement, never to construction-time RAII.
        let pane = unpublished.into_pane();
        drop(retired_floating);
        callbacks.execute(None);
        mux.complete_pane_lifecycle_notification(lifecycle_ticket);
        mux.notify_pane_registration_did_bind(&pane, &registration);
        Ok((pane, positioned, registration))
    }

    pub fn set_floating_pane_rect(
        &self,
        pane_id: PaneId,
        rect: FloatingPaneRect,
    ) -> Option<PositionedFloatingPane> {
        let (mux, positioned, callbacks) = {
            let mut inner = self.inner.lock();
            let mux = inner.notification_owner();
            let (positioned, mut callbacks) = inner.prepare_set_floating_pane_rect(pane_id, rect);
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            (mux, positioned, callbacks)
        };
        callbacks.execute(mux.as_deref());
        positioned
    }

    pub fn set_floating_pane_visible(&self, pane_id: PaneId, visible: bool) -> bool {
        self.inner
            .lock()
            .set_floating_pane_visible(pane_id, visible)
    }

    pub fn set_floating_pane_focus(&self, pane_id: PaneId) -> bool {
        self.inner.lock().set_floating_pane_focus(pane_id)
    }

    pub fn bring_floating_pane_to_front(&self, pane_id: PaneId) -> bool {
        self.inner.lock().bring_floating_pane_to_front(pane_id)
    }

    pub fn set_floating_pane_z_order(&self, pane_id: PaneId, z_order: u32) -> bool {
        self.inner
            .lock()
            .set_floating_pane_z_order(pane_id, z_order)
    }

    /// Terminally remove a floating pane.
    ///
    /// For an admitted tab this retires the exact live registration and its
    /// domain, structural-owner, output, lifecycle, and workspace-count state
    /// in the same callback-free cut. Move transactions must instead use the
    /// crate-private exact detach seam so the live generation remains fenced
    /// across its destination attachment.
    pub fn remove_floating_pane(&self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        let (mux, pane) = {
            let inner = self.inner.lock();
            let mux = inner.notification_owner();
            let pane = inner
                .floating_panes
                .iter()
                .find(|floating| floating.pane_id == pane_id)
                .map(|floating| Arc::clone(&floating.pane));
            (mux, pane)
        };
        let pane = pane?;
        match mux {
            Some(mux) => match self.remove_exact_floating_pane_terminal(&mux, &pane) {
                Ok(removed) => removed,
                Err(error) => {
                    log::error!(
                        "refusing terminal removal of floating pane {pane_id} from tab {}: {error:#}",
                        self.tab_id
                    );
                    None
                }
            },
            None => self.remove_exact_pane_without_mux(
                &pane,
                Some(PaneStructuralLane::Floating),
            ),
        }
    }

    fn remove_exact_floating_pane_terminal(
        &self,
        mux: &Mux,
        expected: &Arc<dyn Pane>,
    ) -> anyhow::Result<Option<Arc<dyn Pane>>> {
        let observed = Self::observe_panes(self.snapshot_panes_callback_free());
        let Some(observed_pane_id) = observed.iter().find_map(|observed| {
            Arc::ptr_eq(&observed.pane, expected).then_some(observed.pane_id).flatten()
        }) else {
            return Ok(None);
        };
        let mut pane_candidates = Vec::new();
        pane_candidates
            .try_reserve_exact(1)
            .map_err(|error| anyhow::anyhow!("reserve floating-pane retirement candidate: {error}"))?;
        pane_candidates.push((observed_pane_id, Arc::clone(expected)));

        let (committed, callbacks, retired_inner) = {
            // Terminal floating removal follows the same global order as tab
            // and window retirement, with the exact parent/count indexes
            // inserted before the tab and pane-state locks they describe.
            let _domain_registration = mux.domain_registration.lock();
            let _registration = mux.pane_registration.lock();
            let mut authority = mux.pane_authority.lock();
            let registered_tabs = mux.tabs.read();
            let Some(registered_tab) = registered_tabs
                .get(&self.tab_id)
                .filter(|tab| std::ptr::eq(Arc::as_ptr(tab), self))
                .cloned()
            else {
                return Ok(None);
            };
            let mut windows = mux.windows.write();
            let parents = mux.tab_parents.read();
            let parent = parents.get(&self.tab_id).ok_or_else(|| {
                anyhow::anyhow!(
                    "tab {} lacks exact window-parent authority during floating-pane retirement",
                    self.tab_id
                )
            })?;
            anyhow::ensure!(
                parent.is_same_tab(&registered_tab),
                "tab {} window-parent authority names another exact generation",
                self.tab_id
            );
            let window = windows.get(&parent.window_id).ok_or_else(|| {
                anyhow::anyhow!(
                    "tab {} parent window {} is absent during floating-pane retirement",
                    self.tab_id,
                    parent.window_id
                )
            })?;
            anyhow::ensure!(
                parent.matches(&registered_tab, parent.window_id)
                    && window
                        .iter()
                        .filter(|candidate| Arc::ptr_eq(candidate, &registered_tab))
                        .count()
                        == 1,
                "tab {} exact window-parent authority disagrees with window membership",
                self.tab_id
            );
            let mut workspace_counts = mux.num_panes_by_workspace.write();
            let prior_structural_count =
                Mux::exact_tab_structural_pane_count(&authority, &registered_tab)?;
            let next_structural_count = prior_structural_count.checked_sub(1).ok_or_else(|| {
                anyhow::anyhow!("floating-pane structural authority count underflow")
            })?;
            let prepared_counts = mux.prepare_tab_pane_count_mutation_locked(
                &windows,
                &parents,
                &mut workspace_counts,
                &[(
                    Arc::clone(&registered_tab),
                    prior_structural_count,
                    next_structural_count,
                )],
                "terminal floating-pane removal",
            )?;
            let mut inner = self.inner.lock();
            anyhow::ensure!(
                inner.is_active_mux_owner(mux),
                "tab {} lost mux-owner authority during floating-pane retirement",
                self.tab_id
            );
            let (current_tiled, current_floating) =
                inner.snapshot_structural_panes_callback_free_checked()?;
            let current_count = current_tiled
                .len()
                .checked_add(current_floating.len())
                .ok_or_else(|| anyhow::anyhow!("floating-pane tab count overflow"))?;
            anyhow::ensure!(
                current_count == prior_structural_count,
                "floating-pane tab changed after count preparation"
            );
            let mut current_identities = HashSet::new();
            current_identities
                .try_reserve(current_count)
                .map_err(|error| anyhow::anyhow!("reserve current floating-pane identities: {error}"))?;
            current_identities.extend(current_tiled.iter().map(pane_identity));
            current_identities.extend(
                current_floating
                    .iter()
                    .map(|(_, pane)| pane_identity(pane)),
            );
            let mut observed_identities = HashSet::new();
            observed_identities
                .try_reserve(observed.len())
                .map_err(|error| anyhow::anyhow!("reserve observed floating-pane identities: {error}"))?;
            observed_identities.extend(
                observed
                    .iter()
                    .map(|observed| pane_identity(&observed.pane)),
            );
            anyhow::ensure!(
                current_identities == observed_identities,
                "tab {} topology changed during floating-pane retirement",
                self.tab_id
            );
            anyhow::ensure!(
                exact_structural_lane_in_snapshot(
                    &current_tiled,
                    &current_floating,
                    expected,
                ) == Some(PaneStructuralLane::Floating),
                "pane {observed_pane_id} is no longer the exact floating allocation in tab {}",
                self.tab_id
            );

            let registration = {
                let panes = mux.panes.read();
                let registered = panes.get(&observed_pane_id).ok_or_else(|| {
                    anyhow::anyhow!(
                        "floating pane {observed_pane_id} lacks a live mux registration"
                    )
                })?;
                anyhow::ensure!(
                    Arc::ptr_eq(&registered.pane, expected),
                    "floating pane id {observed_pane_id} names another exact live generation"
                );
                PaneRegistrationHandle::new(&registered.pane, &registered.generation)
            };
            authority.validate_structural_owner_exact(
                observed_pane_id,
                expected,
                &registered_tab,
                Some(PaneStructuralLane::Floating),
                Some(&registration),
            )?;

            let candidate = ExactPaneRemovalCandidate {
                pane: Arc::clone(expected),
                pane_id: observed_pane_id,
                expected_registration: Some(registration),
                expected_lane: Some(PaneStructuralLane::Floating),
            };
            let mut prepared_inner =
                inner.prepare_exact_pane_removal(&observed, std::slice::from_ref(&candidate));
            anyhow::ensure!(
                prepared_inner.callbacks.changed
                    && prepared_inner
                        .callbacks
                        .removed
                        .contains(&pane_identity(expected)),
                "floating pane {observed_pane_id} produced an incomplete structural successor"
            );
            let focused_pane_id = prepared_inner
                .callbacks
                .focus_changed()
                .then_some(prepared_inner.callbacks.current_focus_id)
                .flatten();
            let structural_revision_count = 1usize
                .checked_add(usize::from(focused_pane_id.is_some()))
                .ok_or_else(|| anyhow::anyhow!("floating-pane notification count overflow"))?;
            let structural_revision_offset = u64::try_from(structural_revision_count)
                .map_err(|_| anyhow::anyhow!("floating-pane revision offset overflow"))?;
            let mut topology_notifications = Vec::new();
            topology_notifications
                .try_reserve_exact(structural_revision_count)
                .map_err(|error| anyhow::anyhow!("reserve floating-pane topology notifications: {error}"))?;

            let prepared_retirement = mux.prepare_tab_pane_candidates_for_removal_locked(
                &authority,
                std::slice::from_ref(&pane_candidates),
            )?;
            anyhow::ensure!(
                prepared_retirement.revision_count() == 1,
                "floating-pane retirement did not retain exactly one live generation"
            );
            let total_revision_count = structural_revision_count
                .checked_add(prepared_retirement.revision_count())
                .ok_or_else(|| anyhow::anyhow!("floating-pane revision count overflow"))?;
            let mut topology = mux.topology.lock();
            let first_revision = topology
                .reserve_revisions(total_revision_count)
                .map_err(anyhow::Error::new)?;
            topology_notifications.push(MuxNotificationEnvelope {
                notification: MuxNotification::TabResized(self.tab_id),
                topology: crate::MuxTopologyStamp::Revision(first_revision),
            });
            if let Some(pane_id) = focused_pane_id {
                topology_notifications.push(MuxNotificationEnvelope {
                    notification: MuxNotification::PaneFocused(pane_id),
                    topology: crate::MuxTopologyStamp::Revision(crate::TopologyRevision::new(
                        first_revision
                            .get()
                            .checked_add(1)
                            .expect("reserved floating-pane structural revisions cannot overflow"),
                    )),
                });
            }
            prepared_inner.callbacks.topology_notifications = topology_notifications;
            let first_lifecycle_revision = crate::TopologyRevision::new(
                first_revision
                    .get()
                    .checked_add(structural_revision_offset)
                    .expect("reserved floating-pane lifecycle revision cannot overflow"),
            );

            // Everything after the contiguous reservation is an infallible,
            // allocation-free swap/removal against identities revalidated
            // under the guards retained above.
            let committed = prepared_retirement.commit(
                &mut authority,
                Some(first_lifecycle_revision),
            );
            let removed_owner = authority.remove_structural_owner_exact(
                observed_pane_id,
                expected,
                &registered_tab,
            );
            debug_assert!(removed_owner);
            let (callbacks, retired_inner) =
                inner.commit_prepared_exact_pane_removal(prepared_inner);
            prepared_counts.commit(&mut windows, &mut workspace_counts);
            (committed, callbacks, retired_inner)
        };

        let crate::CommittedTabPaneRetirement {
            mut removed_panes_by_group,
            mut output_batches_by_group,
            removed_live,
            removed_ids,
        } = committed;
        let removed_panes = removed_panes_by_group
            .pop()
            .expect("one floating-pane retirement retains one removed-pane group");
        debug_assert!(removed_panes_by_group.is_empty());
        let output_batches = output_batches_by_group
            .pop()
            .expect("one floating-pane retirement retains one output-batch group");
        debug_assert!(output_batches_by_group.is_empty());

        // Live-registration destruction, output sealing, pane callbacks, kill,
        // and lifecycle fanout are all deliberately outside every transaction
        // guard. Reentrant subscribers therefore see only the final state.
        drop(retired_inner);
        drop(removed_live);
        mux.finish_taken_tab_pane_state(&removed_ids, output_batches);
        callbacks.execute(Some(mux));
        debug_assert_eq!(removed_panes.len(), 1);
        for removed in removed_panes {
            mux.finish_pane_removal(removed, true);
        }
        Ok(Some(Arc::clone(expected)))
    }

    pub fn iter_floating_panes(&self) -> Vec<PositionedFloatingPane> {
        self.inner.lock().iter_floating_panes()
    }

    /// Return the title only when this tab currently has no tiled pane tree.
    /// This preserves the legacy PDU82 empty-tab representation without
    /// invoking pane code or weakening the ordered producer's stricter rules.
    pub fn empty_pane_tree_title_callback_free(&self) -> Option<String> {
        let inner = self.inner.lock();
        match inner.pane.as_ref() {
            None | Some(Tree::Empty) => Some(inner.title.to_string()),
            Some(Tree::Leaf(_) | Tree::Node { .. }) => None,
        }
    }

    /// Budgeted empty-tree title projection used by authoritative snapshot
    /// producers. Admission happens while the title is still borrowed and
    /// before the only owned copy is created.
    pub fn empty_pane_tree_title_callback_free_with_metadata(
        &self,
        metadata_ledger: &mut PaneSnapshotMetadataLedger,
    ) -> anyhow::Result<Option<String>> {
        let inner = self.inner.lock();
        match inner.pane.as_ref() {
            None | Some(Tree::Empty) => {
                metadata_ledger
                    .preflight_required_string(PaneSnapshotMetadataField::TabTitle, &inner.title)?;
                let title = inner.title.to_string();
                metadata_ledger.admit_required_owned(
                    PaneSnapshotMetadataField::TabTitle,
                    &title,
                    title.capacity(),
                )?;
                Ok(Some(title))
            }
            Some(Tree::Leaf(_) | Tree::Node { .. }) => Ok(None),
        }
    }

    /// Snapshot a bounded floating-pane projection without invoking `Pane`
    /// callbacks. Work is admitted before allocating the output vector.
    pub fn snapshot_floating_panes_with_census_ledger(
        &self,
        max_panes: usize,
        ledger: &mut PaneSnapshotCensusLedger,
    ) -> anyhow::Result<Vec<PositionedFloatingPane>> {
        let inner = self.inner.lock();
        let count = inner.floating_panes.len();
        if count > max_panes {
            anyhow::bail!("floating pane snapshot exceeds its pane-count budget");
        }
        ledger.reserve(PaneSnapshotCensusKind::AssemblyNode, count)?;
        let mut panes = Vec::new();
        panes
            .try_reserve_exact(count)
            .context("reserving bounded floating-pane projection")?;
        panes.extend(
            inner
                .floating_panes
                .iter()
                .map(|floating| inner.positioned_floating_pane(floating)),
        );
        panes.sort_by(|left, right| {
            let left_key = (left.z_order, u8::from(left.is_focused));
            let right_key = (right.z_order, u8::from(right.is_focused));
            left_key.cmp(&right_key)
        });
        Ok(panes)
    }

    pub fn has_panes_in_domain(&self, domain_id: DomainId) -> bool {
        self.inner.lock().has_panes_in_domain(domain_id)
    }

    pub fn domain_id_for_pane(&self, pane_id: PaneId) -> Option<DomainId> {
        self.inner.lock().domain_id_for_pane(pane_id)
    }

    pub fn has_floating_pane(&self, pane_id: PaneId) -> bool {
        self.inner.lock().has_floating_pane(pane_id)
    }

    pub fn rotate_counter_clockwise(&self) {
        let (mux, callbacks) = {
            let mut inner = self.inner.lock();
            let mux = inner.notification_owner();
            let mut callbacks = inner.prepare_rotate_counter_clockwise();
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            (mux, callbacks)
        };
        callbacks.execute(mux.as_deref());
    }

    pub fn rotate_clockwise(&self) {
        let (mux, callbacks) = {
            let mut inner = self.inner.lock();
            let mux = inner.notification_owner();
            let mut callbacks = inner.prepare_rotate_clockwise();
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            (mux, callbacks)
        };
        callbacks.execute(mux.as_deref());
    }

    pub fn iter_splits(&self) -> Vec<PositionedSplit> {
        self.inner.lock().iter_splits()
    }

    pub fn tab_id(&self) -> TabId {
        self.tab_id
    }

    pub fn get_size(&self) -> TerminalSize {
        self.inner.lock().get_size()
    }

    /// Apply the new size of the tab to the panes contained within.
    /// The delta between the current and the new size is computed,
    /// and is distributed between the splits.  For small resizes
    /// this algorithm biases towards adjusting the left/top nodes
    /// first.  For large resizes this tends to proportionally adjust
    /// the relative sizes of the elements in a split.
    pub fn resize(&self, size: TerminalSize) {
        let (mux, callbacks) = {
            let mut inner = self.inner.lock();
            let mux = inner.notification_owner();
            let mut callbacks = inner.prepare_resize(size);
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            (mux, callbacks)
        };
        callbacks.execute(mux.as_deref());
    }

    /// Called when running in the mux server after an individual pane
    /// has been resized.
    /// Because the split manipulation happened on the GUI we "lost"
    /// the information that would have allowed us to call resize_split_by()
    /// and instead need to back-infer the split size information.
    /// We rely on the client to have resized (or be in the process
    /// of resizing) affected panes consistently with its own Tab
    /// tree model.
    /// This method does a simple tree walk to the leaves to back-propagate
    /// the size of the panes up to their containing node split data.
    /// Without this step, disconnecting and reconnecting would cause
    /// the GUI to use stale size information for the window it spawns
    /// to attach this tab.
    pub fn rebuild_splits_sizes_from_contained_panes(&self) {
        let (mux, notification) = {
            let mut inner = self.inner.lock();
            let mux = inner.notification_owner();
            let changed = inner.rebuild_splits_sizes_from_contained_panes();
            let notification = if changed {
                mux.as_deref()
                    .map(|mux| mux.envelope_notification(MuxNotification::TabResized(self.tab_id)))
            } else {
                None
            };
            (mux, notification)
        };
        if let (Some(mux), Some(notification)) = (mux, notification) {
            mux.dispatch_notification_envelope(notification);
        }
    }

    /// Given split_index, the topological index of a split returned by
    /// iter_splits() as PositionedSplit::index, revised the split position
    /// by the provided delta; positive values move the split to the right/bottom,
    /// and negative values to the left/top.
    /// The adjusted size is propogated downwards to contained children and
    /// their panes are resized accordingly.
    pub fn resize_split_by(&self, split_index: usize, delta: isize) {
        let (mux, callbacks) = {
            let mut inner = self.inner.lock();
            let mux = inner.notification_owner();
            let mut callbacks = inner.prepare_resize_split_by(split_index, delta);
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            (mux, callbacks)
        };
        callbacks.execute(mux.as_deref());
    }

    /// Returns `true` if the given pane is currently collapsed (hidden
    /// because the terminal shrank below minimum constraints).
    pub fn is_pane_collapsed(&self, pane_id: PaneId) -> bool {
        self.inner.lock().is_pane_collapsed(pane_id)
    }

    /// Returns the set of currently collapsed pane IDs.
    pub fn collapsed_pane_ids(&self) -> HashSet<PaneId> {
        self.inner.lock().collapsed_pane_ids().clone()
    }

    /// Returns the effective constraints for a pane after runtime overrides.
    pub fn effective_pane_constraints(&self, pane_id: PaneId) -> Option<PaneConstraints> {
        self.inner.lock().effective_pane_constraints_for(pane_id)
    }

    /// Apply runtime constraint overrides to an existing pane.
    pub fn update_pane_constraints(
        &self,
        pane_id: PaneId,
        min_width: Option<usize>,
        max_width: Option<usize>,
        min_height: Option<usize>,
        max_height: Option<usize>,
    ) -> Option<PaneConstraints> {
        let (mux, updated, callbacks) = {
            let mut inner = self.inner.lock();
            let mux = inner.notification_owner();
            let (updated, mut callbacks) = inner.prepare_update_pane_constraints(
                pane_id, min_width, max_width, min_height, max_height,
            );
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            (mux, updated, callbacks)
        };
        callbacks.execute(mux.as_deref());
        updated
    }

    /// Set the layout cycle for swap-layout support.
    pub fn set_layout_cycle(&self, cycle: LayoutCycle) {
        self.inner.lock().set_layout_cycle(cycle)
    }

    /// Swap to the next layout in the cycle.
    /// Returns the name of the new layout, or None if no cycle is configured.
    pub fn swap_to_next_layout(&self) -> Option<String> {
        let (mux, name, callbacks) = {
            let mut inner = self.inner.lock();
            let mux = inner.notification_owner();
            let (name, mut callbacks) = inner.prepare_swap_to_next_layout();
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            (mux, name, callbacks)
        };
        callbacks.execute(mux.as_deref());
        name
    }

    /// Swap to the previous layout in the cycle.
    pub fn swap_to_prev_layout(&self) -> Option<String> {
        let (mux, name, callbacks) = {
            let mut inner = self.inner.lock();
            let mux = inner.notification_owner();
            let (name, mut callbacks) = inner.prepare_swap_to_prev_layout();
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            (mux, name, callbacks)
        };
        callbacks.execute(mux.as_deref());
        name
    }

    /// Swap to a specific layout by index in the cycle.
    pub fn swap_to_layout_index(&self, index: usize) -> Option<String> {
        let (mux, name, callbacks) = {
            let mut inner = self.inner.lock();
            let mux = inner.notification_owner();
            let (name, mut callbacks) = inner.prepare_swap_to_layout_index(index);
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            (mux, name, callbacks)
        };
        callbacks.execute(mux.as_deref());
        name
    }

    /// Cycle to the next pane in a stack at the given slot index.
    pub fn cycle_stack(&self, slot_index: usize) -> Option<PaneId> {
        self.inner.lock().cycle_stack(slot_index)
    }

    /// Cycle to the previous pane in a stack at the given slot index.
    pub fn cycle_stack_backward(&self, slot_index: usize) -> Option<PaneId> {
        self.inner.lock().cycle_stack_backward(slot_index)
    }

    /// Select a specific pane in a stack by index.
    pub fn select_stack_pane(&self, slot_index: usize, pane_index: usize) -> Option<PaneId> {
        self.inner.lock().select_stack_pane(slot_index, pane_index)
    }

    /// Returns the current layout name, if a cycle is active.
    pub fn current_layout_name(&self) -> Option<String> {
        self.inner.lock().current_layout_name()
    }

    /// Returns the number of pane stacks.
    pub fn stack_count(&self) -> usize {
        self.inner.lock().stack_count()
    }

    /// Returns the first stack slot index that has more than one pane.
    pub fn first_nontrivial_stack_slot_index(&self) -> Option<usize> {
        self.inner.lock().first_nontrivial_stack_slot_index()
    }

    /// Returns all stacked pane IDs across all slots.
    pub fn all_stacked_pane_ids(&self) -> Vec<PaneId> {
        self.inner.lock().all_stacked_pane_ids()
    }

    /// Compute the resize budget for a split identified by its topological
    /// index.  Returns `None` if the index is out of range, otherwise
    /// `(max_shrink, max_grow)` where max_shrink is negative (how far
    /// the first child can shrink) and max_grow is positive (how far
    /// the first child can grow).
    pub fn compute_split_budget(&self, split_index: usize) -> Option<(isize, isize)> {
        self.inner.lock().compute_split_budget(split_index)
    }

    /// Adjusts the size of the active pane in the specified direction
    /// by the specified amount.
    pub fn adjust_pane_size(&self, direction: PaneDirection, amount: usize) {
        let (mux, callbacks) = {
            let mut inner = self.inner.lock();
            let mux = inner.notification_owner();
            let mut callbacks = inner.prepare_adjust_pane_size(direction, amount);
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            (mux, callbacks)
        };
        callbacks.execute(mux.as_deref());
    }

    /// Activate an adjacent pane in the specified direction.
    /// In cases where there are multiple adjacent panes in the
    /// intended direction, we take the pane that has the largest
    /// edge intersection.
    pub fn activate_pane_direction(&self, direction: PaneDirection) {
        if self.get_zoomed_pane().is_some() {
            if !configuration().unzoom_on_switch_pane {
                return;
            }
            self.toggle_zoom();
        }
        let target = {
            let mut inner = self.inner.lock();
            inner
                .get_pane_direction(direction, false)
                .and_then(|index| inner.raw_tree_pane_at_index(index))
        };
        let Some(target) = target else {
            return;
        };
        if !self.set_active_pane(&target) {
            return;
        }

        let mux = self.inner.lock().notification_owner();
        if let Some(mux) = mux {
            if let Some(window_id) = mux.window_containing_tab(self.tab_id) {
                mux.notify(MuxNotification::WindowInvalidated(window_id));
            }
        }
    }

    /// Returns an adjacent pane in the specified direction.
    /// In cases where there are multiple adjacent panes in the
    /// intended direction, we take the pane that has the largest
    /// edge intersection.
    pub fn get_pane_direction(&self, direction: PaneDirection, ignore_zoom: bool) -> Option<usize> {
        self.inner.lock().get_pane_direction(direction, ignore_zoom)
    }

    fn snapshot_panes_callback_free(&self) -> Vec<Arc<dyn Pane>> {
        self.inner.lock().snapshot_panes_callback_free()
    }

    /// Commit exact tab retirement while retaining both structural state and
    /// owner-generation authority. Any mux registry guards must be acquired
    /// before entering, and the callback must remain callback-free. Returning
    /// `None` aborts with the owner generation still active.
    pub(crate) fn with_pane_snapshot_for_retirement<R>(
        &self,
        mux: &Mux,
        f: impl FnOnce(Vec<Arc<dyn Pane>>) -> Option<R>,
    ) -> Option<R> {
        let mut inner = self.inner.lock();
        if !inner.is_active_mux_owner(mux) {
            return None;
        }
        let result = f(inner.snapshot_panes_callback_free())?;
        let retired = inner.retire_mux_owner(mux);
        debug_assert!(retired);
        self.mux_owner_generation.store(0, Ordering::Release);
        Some(result)
    }

    /// Freeze several exact tabs in a stable identity order and execute one
    /// callback-free mux transaction derived from all of their pane snapshots.
    ///
    /// Window retirement needs a single structural cut across every tab it
    /// owns. Locking only the tab that supplied a delayed-operation witness
    /// would leave sibling tabs mutable between window-map retirement and the
    /// pane-registry census. `tabs` must not contain the same exact `Arc<Tab>`
    /// more than once. Any mux registry guards must be acquired before calling
    /// this method, and `f` must not invoke pane code or reacquire any tab.
    /// Multi-tab retirement variant that retires every exact owner generation
    /// in the same frozen cut as the caller's window/tab registry commit.
    pub(crate) fn with_pane_snapshots_for_retirement<R>(
        mux: &Mux,
        tabs: &[Arc<Self>],
        expected: Option<(&Arc<Self>, &PaneOperationGuard)>,
        f: impl FnOnce(Vec<Vec<Arc<dyn Pane>>>) -> Option<R>,
    ) -> Option<R> {
        let mut lock_order = tabs.iter().enumerate().collect::<Vec<_>>();
        lock_order.sort_unstable_by_key(|(_, tab)| Arc::as_ptr(tab) as usize);
        debug_assert!(lock_order
            .windows(2)
            .all(|pair| !Arc::ptr_eq(pair[0].1, pair[1].1)));

        let mut guards = lock_order
            .iter()
            .map(|(_, tab)| tab.inner.lock())
            .collect::<Vec<_>>();
        if guards.iter().any(|inner| !inner.is_active_mux_owner(mux)) {
            return None;
        }
        let mut snapshots = vec![Vec::new(); tabs.len()];
        for ((original_index, _), guard) in lock_order.iter().zip(&guards) {
            snapshots[*original_index] = guard.snapshot_panes_callback_free();
        }

        if let Some((expected_tab, operation)) = expected {
            let expected_index = tabs.iter().position(|tab| Arc::ptr_eq(tab, expected_tab))?;
            if !snapshots[expected_index]
                .iter()
                .any(|pane| operation.is_same_pane(pane))
            {
                return None;
            }
        }

        let result = f(snapshots)?;
        for (inner, (_, tab)) in guards.iter_mut().zip(&lock_order) {
            let retired = inner.retire_mux_owner(mux);
            debug_assert!(retired);
            tab.mux_owner_generation.store(0, Ordering::Release);
        }
        Some(result)
    }

    /// Execute `f` only while this exact tab has no structural pane entries.
    ///
    /// The tab topology lock remains held throughout `f`, so the callback must
    /// not invoke pane code or attempt to reacquire this tab. Callers that also
    /// need mux registration authority must acquire it before entering here.
    pub(crate) fn with_structurally_empty<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        let inner = self.inner.lock();
        if inner.snapshot_panes_callback_free().is_empty() {
            Some(f())
        } else {
            None
        }
    }

    /// Empty-tab retirement variant that deactivates the exact owner only
    /// after the registry callback commits successfully.
    pub(crate) fn with_structurally_empty_for_retirement<R>(
        &self,
        mux: &Mux,
        f: impl FnOnce() -> Option<R>,
    ) -> Option<R> {
        let mut inner = self.inner.lock();
        if !inner.is_active_mux_owner(mux) || !inner.snapshot_panes_callback_free().is_empty() {
            return None;
        }
        let result = f()?;
        let retired = inner.retire_mux_owner(mux);
        debug_assert!(retired);
        self.mux_owner_generation.store(0, Ordering::Release);
        Some(result)
    }

    /// Execute `f` only while this tab still structurally contains the exact
    /// pane held by `operation`.
    ///
    /// The callback-free tab lock remains held throughout `f`, so a delayed
    /// destructive transaction can bind its commit to both tab identity and
    /// pane-generation authority. Any mux registry guards must be acquired
    /// before entering; the callback must not invoke pane code or attempt to
    /// reacquire this tab.
    pub(crate) fn with_exact_pane_operation_for_retirement<R>(
        &self,
        mux: &Mux,
        operation: &PaneOperationGuard,
        f: impl FnOnce(Vec<Arc<dyn Pane>>) -> Option<R>,
    ) -> Option<R> {
        let mut inner = self.inner.lock();
        if !inner.is_active_mux_owner(mux) {
            return None;
        }
        let panes = inner.snapshot_panes_callback_free();
        if !panes.iter().any(|pane| operation.is_same_pane(pane)) {
            return None;
        }
        let result = f(panes)?;
        let retired = inner.retire_mux_owner(mux);
        debug_assert!(retired);
        self.mux_owner_generation.store(0, Ordering::Release);
        Some(result)
    }

    fn observe_panes(panes: Vec<Arc<dyn Pane>>) -> Vec<ObservedPane> {
        panes
            .into_iter()
            .map(|pane| {
                let pane_id = match catch_recoverable(
                    RecoverablePanicSite::MuxPaneCallback,
                    AssertUnwindSafe(|| pane.pane_id()),
                ) {
                    Ok(pane_id) => Some(pane_id),
                    Err(_) => {
                        log::error!(
                            "Pane::pane_id panicked for exact pane identity {:p}; \
                             retaining it conservatively",
                            Arc::as_ptr(&pane)
                        );
                        None
                    }
                };
                ObservedPane { pane, pane_id }
            })
            .collect()
    }

    fn apply_exact_removal_plan(
        &self,
        mux: &Mux,
        observed: &[ObservedPane],
        candidates: &[ExactPaneRemovalCandidate],
    ) -> (bool, Vec<PaneRegistrationHandle>) {
        if candidates.is_empty() {
            return (false, Vec::new());
        }

        let (callbacks, registrations, retired_inner) = {
            // Registry publication/retirement is serialized before topology
            // mutation. No pane trait method is invoked in this scope.
            let _registration = mux.pane_registration.lock();
            let mut authority = mux.pane_authority.lock();
            let registered_tabs = mux.tabs.read();
            let Some(registered_tab) = registered_tabs
                .get(&self.tab_id)
                .filter(|registered| std::ptr::eq(Arc::as_ptr(registered), self))
                .cloned()
            else {
                return (false, Vec::new());
            };
            let mut windows = mux.windows.write();
            let tab_parents = mux.tab_parents.read();
            let mut workspace_counts = mux.num_panes_by_workspace.write();
            let mut registration_authorized = Vec::new();
            if registration_authorized
                .try_reserve_exact(candidates.len())
                .is_err()
            {
                return (false, Vec::new());
            }
            {
                let registered = mux.panes.read();
                for candidate in candidates {
                    let current = registered.get(&candidate.pane_id);
                    let registration_matches = match &candidate.expected_registration {
                        Some(expected) => current.is_some_and(|current| {
                            Arc::ptr_eq(&current.pane, &candidate.pane)
                                && expected.same_registration(&PaneRegistrationHandle::new(
                                    &current.pane,
                                    &current.generation,
                                ))
                                && authority.contains_live_registration(
                                    candidate.pane_id,
                                    current.domain_id,
                                    expected,
                                )
                        }),
                        None => current
                            .is_none_or(|current| !Arc::ptr_eq(&current.pane, &candidate.pane)),
                    };
                    if !registration_matches {
                        continue;
                    }
                    registration_authorized.push(candidate.clone());
                }
            }
            if registration_authorized.is_empty() {
                return (false, Vec::new());
            }

            let mut inner = self.inner.lock();
            if !inner.is_active_mux_owner(mux) {
                return (false, Vec::new());
            }
            let (current_tiled, current_floating) =
                match inner.snapshot_structural_panes_callback_free_checked() {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        log::error!(
                            "refusing exact removal from structurally invalid tab {}: {error:#}",
                            self.tab_id
                        );
                        return (false, Vec::new());
                    }
                };
            let current_count = match current_tiled.len().checked_add(current_floating.len()) {
                Some(count) => count,
                None => return (false, Vec::new()),
            };
            let mut current_identities = HashSet::new();
            if current_identities.try_reserve(current_count).is_err() {
                return (false, Vec::new());
            }
            current_identities.extend(current_tiled.iter().map(pane_identity));
            current_identities.extend(
                current_floating
                    .iter()
                    .map(|(_, pane)| pane_identity(pane)),
            );
            let mut observed_identities = HashSet::new();
            if observed_identities.try_reserve(observed.len()).is_err() {
                return (false, Vec::new());
            }
            observed_identities.extend(
                observed
                    .iter()
                    .map(|observed| pane_identity(&observed.pane)),
            );
            if current_identities != observed_identities {
                return (false, Vec::new());
            }

            let mut authorized = Vec::new();
            if authorized
                .try_reserve_exact(registration_authorized.len())
                .is_err()
            {
                return (false, Vec::new());
            }
            for candidate in registration_authorized {
                let Some(lane) = exact_structural_lane_in_snapshot(
                    &current_tiled,
                    &current_floating,
                    &candidate.pane,
                ) else {
                    continue;
                };
                if candidate
                    .expected_lane
                    .is_some_and(|expected| expected != lane)
                {
                    continue;
                }
                if let Err(error) = authority.validate_structural_owner_exact(
                    candidate.pane_id,
                    &candidate.pane,
                    &registered_tab,
                    Some(lane),
                    candidate.expected_registration.as_ref(),
                ) {
                    log::error!(
                        "refusing exact removal of pane {} from tab {}: {error:#}",
                        candidate.pane_id,
                        self.tab_id
                    );
                    continue;
                }
                authorized.push(candidate);
            }
            if authorized.is_empty() {
                return (false, Vec::new());
            }

            let mut registrations = Vec::new();
            if registrations.try_reserve_exact(authorized.len()).is_err() {
                return (false, Vec::new());
            }
            for candidate in &authorized {
                if let Some(registration) = &candidate.expected_registration {
                    registrations.push(registration.clone());
                }
            }

            let mut prepared = inner.prepare_exact_pane_removal(observed, &authorized);
            if !prepared.callbacks.changed
                || authorized.iter().any(|candidate| {
                    !prepared
                        .callbacks
                        .removed
                        .contains(&pane_identity(&candidate.pane))
                })
            {
                log::error!(
                    "refusing incomplete exact-removal successor for tab {}",
                    self.tab_id
                );
                return (false, Vec::new());
            }
            prepared.callbacks.topology_notifications =
                match prepared
                    .callbacks
                    .prepare_topology_notifications(mux, self.tab_id)
                {
                    Ok(notifications) => notifications,
                    Err(error) => {
                        log::error!(
                            "refusing exact removal from tab {} without topology revision: {error:#}",
                            self.tab_id
                        );
                        return (false, Vec::new());
                    }
                };

            let next_count = match current_count.checked_sub(authorized.len()) {
                Some(count) => count,
                None => return (false, Vec::new()),
            };
            let prepared_counts = match mux.prepare_tab_pane_count_mutation_locked(
                &windows,
                &tab_parents,
                &mut workspace_counts,
                &[(Arc::clone(&registered_tab), current_count, next_count)],
                "exact pane removal",
            ) {
                Ok(prepared) => prepared,
                Err(error) => {
                    log::error!(
                        "refusing exact removal count commit for tab {}: {error:#}",
                        self.tab_id
                    );
                    return (false, Vec::new());
                }
            };

            let (callbacks, retired_inner) =
                inner.commit_prepared_exact_pane_removal(prepared);
            for candidate in &authorized {
                authority.remove_structural_owner_exact(
                    candidate.pane_id,
                    &candidate.pane,
                    &registered_tab,
                );
            }
            prepared_counts.commit(&mut windows, &mut workspace_counts);
            (callbacks, registrations, retired_inner)
        };

        let changed = callbacks.changed;
        drop(retired_inner);
        callbacks.execute(Some(mux));
        (changed, registrations)
    }

    #[cfg(test)]
    fn prune_dead_panes_without_mux(&self) -> bool {
        let panes = self.snapshot_panes_callback_free();
        let observed = Self::observe_panes(panes);
        let candidates = observed
            .iter()
            .filter_map(|observed| {
                let pane_id = observed.pane_id?;
                match catch_recoverable(
                    RecoverablePanicSite::MuxPaneCallback,
                    AssertUnwindSafe(|| observed.pane.is_dead()),
                ) {
                    Ok(true) => Some(ExactPaneRemovalCandidate {
                        pane: Arc::clone(&observed.pane),
                        pane_id,
                        expected_registration: None,
                        expected_lane: None,
                    }),
                    Ok(false) => None,
                    Err(_) => {
                        log::error!(
                            "Pane::is_dead panicked for pane {pane_id}; retaining it conservatively"
                        );
                        None
                    }
                }
            })
            .collect::<Vec<_>>();
        let mut inner = self.inner.lock();
        let prepared = inner.prepare_exact_pane_removal(&observed, &candidates);
        let (callbacks, retired_inner) =
            inner.commit_prepared_exact_pane_removal(prepared);
        let changed = callbacks.changed;
        drop(inner);
        drop(retired_inner);
        callbacks.execute(None);
        changed
    }

    pub(crate) fn prune_dead_panes_deferred(
        &self,
        mux: &Mux,
    ) -> (bool, Vec<PaneRegistrationHandle>) {
        let panes = self.snapshot_panes_callback_free();
        let observed = Self::observe_panes(panes);
        let mut candidates = Vec::new();

        for observed in &observed {
            let Some(pane_id) = observed.pane_id else {
                continue;
            };
            let registered = mux.get_pane(pane_id);
            let registered_exact = registered
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &observed.pane));
            let should_remove = if registered_exact {
                match catch_recoverable(
                    RecoverablePanicSite::MuxPaneCallback,
                    AssertUnwindSafe(|| observed.pane.is_dead()),
                ) {
                    Ok(dead) => {
                        log::trace!("prune_dead_panes: pane_id={pane_id} dead={dead} in_mux=true");
                        dead
                    }
                    Err(_) => {
                        log::error!(
                            "Pane::is_dead panicked for pane {pane_id}; retaining it conservatively"
                        );
                        false
                    }
                }
            } else {
                let detached_operation_in_flight =
                    match catch_recoverable(
                        RecoverablePanicSite::MuxPaneCallback,
                        AssertUnwindSafe(|| {
                            observed.pane.mux_registration_slot().load().is_some_and(
                                |registration| {
                                    registration.guards_detached_topology(mux, &observed.pane)
                                },
                            )
                        }),
                    ) {
                        Ok(in_flight) => in_flight,
                        Err(_) => {
                            log::error!(
                                "pane registration lookup panicked for pane {pane_id}; \
                                 retaining its detached topology conservatively"
                            );
                            true
                        }
                    };
                if detached_operation_in_flight {
                    log::trace!(
                        "prune_dead_panes: pane_id={pane_id} retained by admitted detached operation"
                    );
                    continue;
                }
                // A topology entry that is not the exact pane registered in
                // the mux and is not fenced by an admitted exact operation is
                // stale regardless of its self-reported liveness. Final
                // authorization below rechecks absence while holding the
                // registration lock, so a concurrent exact re-registration is
                // never removed.
                log::trace!("prune_dead_panes: pane_id={pane_id} dead=not-queried in_mux=false");
                true
            };
            if !should_remove {
                continue;
            }

            let expected_registration = if registered_exact {
                match catch_recoverable(
                    RecoverablePanicSite::MuxPaneCallback,
                    AssertUnwindSafe(|| mux.capture_pane_registration(&observed.pane)),
                ) {
                    Ok(registration) => registration,
                    Err(_) => {
                        log::error!(
                            "pane registration capture panicked for pane {pane_id}; \
                             retaining it conservatively"
                        );
                        continue;
                    }
                }
            } else {
                None
            };
            candidates.push(ExactPaneRemovalCandidate {
                pane: Arc::clone(&observed.pane),
                pane_id,
                expected_registration,
                expected_lane: None,
            });
        }

        self.apply_exact_removal_plan(mux, &observed, &candidates)
    }

    /// Structurally remove only the supplied exact pane objects.
    ///
    /// This is the domain-detach counterpart to dead-pane pruning. Numeric
    /// pane IDs are observed outside `Tab::inner`, then the mux registration
    /// and exact `Arc` identity are revalidated under the registration lock.
    /// Pane resize/focus callbacks are deferred until every lock is released.
    pub(crate) fn remove_exact_panes_deferred(
        &self,
        mux: &Mux,
        panes: &[Arc<dyn Pane>],
    ) -> (bool, Vec<PaneRegistrationHandle>) {
        let requested = panes
            .iter()
            .map(pane_identity)
            .collect::<HashSet<PaneIdentity>>();
        let observed = Self::observe_panes(self.snapshot_panes_callback_free());
        let mut candidates = Vec::new();
        for observed in &observed {
            if !requested.contains(&pane_identity(&observed.pane)) {
                continue;
            }
            let Some(pane_id) = observed.pane_id else {
                continue;
            };
            let expected_registration = match catch_recoverable(
                RecoverablePanicSite::MuxPaneCallback,
                AssertUnwindSafe(|| mux.capture_pane_registration(&observed.pane)),
            ) {
                Ok(registration) => registration,
                Err(_) => {
                    log::error!(
                        "pane registration capture panicked for exact pane {pane_id}; \
                         retaining it conservatively"
                    );
                    continue;
                }
            };
            candidates.push(ExactPaneRemovalCandidate {
                pane: Arc::clone(&observed.pane),
                pane_id,
                expected_registration,
                expected_lane: None,
            });
        }
        self.apply_exact_removal_plan(mux, &observed, &candidates)
    }

    pub fn kill_pane_registration(&self, registration: &PaneRegistrationHandle) -> bool {
        let observed = Self::observe_panes(self.snapshot_panes_callback_free());
        let candidate = observed.iter().find_map(|observed| {
            let pane_id = observed.pane_id?;
            registration
                .try_with_current(|current| current.is_same_pane(&observed.pane))
                .unwrap_or(false)
                .then(|| ExactPaneRemovalCandidate {
                    pane: Arc::clone(&observed.pane),
                    pane_id,
                    expected_registration: Some(registration.clone()),
                    expected_lane: None,
                })
        });
        let Some(candidate) = candidate else {
            return false;
        };
        let Some(mux) = registration.owner() else {
            return false;
        };
        let (changed, registrations) = self.apply_exact_removal_plan(&mux, &observed, &[candidate]);
        changed && PaneRegistrationHandle::retire_batch_if_current(registrations) == 1
    }

    /// Remove pane from tab.
    /// The pane is still live in the mux; the intent is for the pane to
    /// be added to a different tab.
    pub fn remove_pane(&self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        let (mux, floating, snapshot) = {
            let inner = self.inner.lock();
            let floating = inner
                .floating_panes
                .iter()
                .find(|floating| floating.pane_id == pane_id)
                .map(|floating| Arc::clone(&floating.pane));
            (
                inner.notification_owner(),
                floating,
                inner.snapshot_panes_callback_free(),
            )
        };
        let pane = floating.or_else(|| {
            Self::observe_panes(snapshot)
                .into_iter()
                .find(|observed| observed.pane_id == Some(pane_id))
                .map(|observed| observed.pane)
        })?;
        match mux {
            Some(mux) => self.remove_exact_pane_for_move(&mux, &pane),
            None => self.remove_exact_pane_without_mux(&pane, None),
        }
    }

    /// Remove only the supplied pane allocation for an admitted move.
    ///
    /// The caller must hold the pane's operation guard. Structural mutation
    /// happens under the tab lock, while pane resize/focus callbacks and mux
    /// notifications are deferred until after that lock is released.
    pub(crate) fn remove_exact_pane_for_move(
        &self,
        mux: &Mux,
        expected: &Arc<dyn Pane>,
    ) -> Option<Arc<dyn Pane>> {
        self.remove_exact_pane_for_move_in_lane(mux, expected, None)
    }

    fn remove_exact_pane_for_move_in_lane(
        &self,
        mux: &Mux,
        expected: &Arc<dyn Pane>,
        expected_lane: Option<PaneStructuralLane>,
    ) -> Option<Arc<dyn Pane>> {
        let observed = Self::observe_panes(self.snapshot_panes_callback_free());
        let mut candidate = observed.iter().find_map(|observed| {
            observed
                .pane_id
                .filter(|_| Arc::ptr_eq(&observed.pane, expected))
                .map(|pane_id| ExactPaneRemovalCandidate {
                    pane: Arc::clone(&observed.pane),
                    pane_id,
                    expected_registration: None,
                    expected_lane,
                })
        })?;
        candidate.expected_registration = Some({
            let panes = mux.panes.read();
            let registered = panes.get(&candidate.pane_id)?;
            if !Arc::ptr_eq(&registered.pane, &candidate.pane) {
                return None;
            }
            PaneRegistrationHandle::new(&registered.pane, &registered.generation)
        });
        let (changed, _) =
            self.apply_exact_removal_plan(mux, &observed, std::slice::from_ref(&candidate));
        changed.then_some(candidate.pane)
    }

    fn remove_exact_pane_without_mux(
        &self,
        expected: &Arc<dyn Pane>,
        expected_lane: Option<PaneStructuralLane>,
    ) -> Option<Arc<dyn Pane>> {
        let observed = Self::observe_panes(self.snapshot_panes_callback_free());
        let candidate = observed.iter().find_map(|observed| {
            observed
                .pane_id
                .filter(|_| Arc::ptr_eq(&observed.pane, expected))
                .map(|pane_id| ExactPaneRemovalCandidate {
                    pane: Arc::clone(&observed.pane),
                    pane_id,
                    expected_registration: None,
                    expected_lane,
                })
        })?;
        let (callbacks, retired_inner) = {
            let mut inner = self.inner.lock();
            if inner.notification_owner().is_some() {
                return None;
            }
            let (current_tiled, current_floating) = inner
                .snapshot_structural_panes_callback_free_checked()
                .ok()?;
            let lane = exact_structural_lane_in_snapshot(
                &current_tiled,
                &current_floating,
                &candidate.pane,
            )?;
            if candidate
                .expected_lane
                .is_some_and(|expected| expected != lane)
            {
                return None;
            }
            let current_count = current_tiled.len().checked_add(current_floating.len())?;
            let mut current_identities = HashSet::new();
            current_identities.try_reserve(current_count).ok()?;
            current_identities.extend(current_tiled.iter().map(pane_identity));
            current_identities.extend(
                current_floating
                    .iter()
                    .map(|(_, pane)| pane_identity(pane)),
            );
            let mut observed_identities = HashSet::new();
            observed_identities.try_reserve(observed.len()).ok()?;
            observed_identities.extend(
                observed
                    .iter()
                    .map(|observed| pane_identity(&observed.pane)),
            );
            if current_identities != observed_identities {
                return None;
            }

            let prepared =
                inner.prepare_exact_pane_removal(&observed, std::slice::from_ref(&candidate));
            if !prepared.callbacks.changed
                || !prepared
                    .callbacks
                    .removed
                    .contains(&pane_identity(&candidate.pane))
            {
                return None;
            }
            inner.commit_prepared_exact_pane_removal(prepared)
        };
        drop(retired_inner);
        callbacks.execute(None);
        Some(candidate.pane)
    }

    pub fn can_close_without_prompting(&self, reason: CloseReason) -> bool {
        self.snapshot_panes_callback_free().into_iter().all(|pane| {
            match catch_recoverable(
                RecoverablePanicSite::MuxPaneCallback,
                AssertUnwindSafe(|| pane.can_close_without_prompting(reason)),
            ) {
                Ok(can_close) => can_close,
                Err(_) => {
                    log::error!(
                        "Pane::can_close_without_prompting panicked for exact pane identity {:p}; \
                         requiring a close prompt conservatively",
                        Arc::as_ptr(&pane)
                    );
                    false
                }
            }
        })
    }

    pub fn is_dead(&self) -> bool {
        // Make sure we account for all panes, so that we don't kill the
        // whole tab if the zoomed pane is dead. A panicking liveness callback
        // is conservatively treated as live.
        self.snapshot_panes_callback_free().into_iter().all(|pane| {
            match catch_recoverable(
                RecoverablePanicSite::MuxPaneCallback,
                AssertUnwindSafe(|| pane.is_dead()),
            ) {
                Ok(dead) => dead,
                Err(_) => {
                    log::error!(
                        "Pane::is_dead panicked for exact pane identity {:p}; \
                         retaining its tab conservatively",
                        Arc::as_ptr(&pane)
                    );
                    false
                }
            }
        })
    }

    pub fn get_active_pane(&self) -> Option<Arc<dyn Pane>> {
        self.inner.lock().get_active_pane()
    }

    /// Resolve the active pane without invoking a `Pane` method while the tab
    /// topology lock is held.
    pub(crate) fn get_active_pane_callback_free(&self) -> Option<Arc<dyn Pane>> {
        self.inner.lock().raw_active_pane_retained_id()
    }

    #[cfg(test)]
    pub(crate) fn topology_lock_is_available_for_test(&self) -> bool {
        self.inner.try_lock().is_some()
    }

    #[allow(unused)]
    pub fn get_active_idx(&self) -> usize {
        self.inner.lock().get_active_idx()
    }

    pub fn set_active_pane(&self, pane: &Arc<dyn Pane>) -> bool {
        let pane_id = match observe_pane_id_for_mutation(pane) {
            Ok(pane_id) => pane_id,
            Err(error) => {
                log::error!("refusing to focus a pane whose identity callback failed: {error:#}");
                return false;
            }
        };
        let (mux, selected, callbacks) = {
            let mut inner = self.inner.lock();
            let mux = inner.notification_owner();
            let (selected, mut callbacks) = inner.prepare_set_active_pane(pane, pane_id);
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            (mux, selected, callbacks)
        };
        callbacks.execute(mux.as_deref());
        selected
    }

    /// Select a pane while routing the resulting notification through the
    /// exact mux that owns the surrounding topology.
    pub(crate) fn set_active_pane_for_mux(&self, pane: &Arc<dyn Pane>, mux: &Mux) -> bool {
        {
            let inner = self.inner.lock();
            if !inner.is_active_mux_owner(mux) {
                return false;
            }
        }
        let pane_id = match observe_pane_id_for_mutation(pane) {
            Ok(pane_id) => pane_id,
            Err(error) => {
                log::error!("refusing to focus a pane whose identity callback failed: {error:#}");
                return false;
            }
        };
        let (owner, selected, callbacks) = {
            let mut inner = self.inner.lock();
            if !inner.is_active_mux_owner(mux) {
                return false;
            }
            let owner = inner.notification_owner();
            let (selected, mut callbacks) = inner.prepare_set_active_pane(pane, pane_id);
            if let Some(owner) = owner.as_deref() {
                callbacks.reserve_topology_notifications(owner, self.tab_id);
            }
            (owner, selected, callbacks)
        };
        callbacks.execute(owner.as_deref());
        selected
    }

    pub fn set_active_idx(&self, pane_index: usize) {
        let pane = self.inner.lock().raw_tree_pane_at_index(pane_index);
        if let Some(pane) = pane {
            self.set_active_pane(&pane);
        }
    }

    /// Assigns the root pane.
    /// This is suitable when creating a new tab and then assigning
    /// the initial pane
    pub fn assign_pane(&self, pane: &Arc<dyn Pane>) {
        let pane_id = match observe_pane_id_for_mutation(pane) {
            Ok(pane_id) => pane_id,
            Err(error) => {
                log::error!("refusing root-pane assignment: {error:#}");
                return;
            }
        };
        let owner = self.inner.lock().notification_owner();
        let Some(mux) = owner else {
            self.inner.lock().assign_pane(pane);
            return;
        };

        let _registration = mux.pane_registration.lock();
        let mut authority = mux.pane_authority.lock();
        let registered_tabs = mux.tabs.read();
        let Some(registered_tab) = registered_tabs
            .get(&self.tab_id)
            .filter(|registered| std::ptr::eq(Arc::as_ptr(registered), self))
            .cloned()
        else {
            log::error!(
                "refusing pane {pane_id} assignment: tab {} lost exact mux registration",
                self.tab_id
            );
            return;
        };
        let Some(tab_mux_owner_generation) = registered_tab.active_mux_owner_generation() else {
            log::error!(
                "refusing pane {pane_id} assignment: tab {} lacks active mux-owner generation",
                self.tab_id
            );
            return;
        };
        let mut windows = mux.windows.write();
        let tab_parents = mux.tab_parents.read();
        let mut workspace_counts = mux.num_panes_by_workspace.write();
        let (pane_registration, domain_id) = {
            let panes = mux.panes.read();
            let Some(registered) = panes.get(&pane_id) else {
                log::error!(
                    "refusing pane {pane_id} assignment: pane is not registered in the owning mux"
                );
                return;
            };
            if !Arc::ptr_eq(&registered.pane, pane) {
                log::error!(
                    "refusing pane {pane_id} assignment: numeric slot names another exact allocation"
                );
                return;
            }
            (
                PaneRegistrationHandle::new(&registered.pane, &registered.generation),
                registered.domain_id,
            )
        };
        if !authority.contains_live_registration(pane_id, domain_id, &pane_registration) {
            log::error!(
                "refusing pane {pane_id} assignment: exact domain-registration authority is absent"
            );
            return;
        }
        match Mux::exact_tab_structural_pane_count(&authority, &registered_tab) {
            Ok(0) => {}
            Ok(count) => {
                log::error!(
                    "refusing pane {pane_id} assignment: empty tab {} retains {count} structural owners",
                    self.tab_id
                );
                return;
            }
            Err(error) => {
                log::error!("refusing pane {pane_id} assignment count validation: {error:#}");
                return;
            }
        }
        let structural = match authority.prepare_new_structural_bind(
            pane_id,
            Arc::clone(pane),
            Arc::clone(&registered_tab),
            PaneStructuralLane::Tiled,
            Some((pane_registration, domain_id)),
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                log::error!("refusing pane {pane_id} assignment: {error:#}");
                return;
            }
        };
        let prepared_counts = match mux.prepare_tab_pane_count_mutation_locked(
            &windows,
            &tab_parents,
            &mut workspace_counts,
            &[(Arc::clone(&registered_tab), 0, 1)],
            "bound root-pane assignment",
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                log::error!("refusing pane {pane_id} assignment count commit: {error:#}");
                return;
            }
        };
        let mut inner = self.inner.lock();
        if !inner.is_active_mux_owner(&mux) {
            return;
        }
        if !inner.snapshot_panes_callback_free().is_empty() {
            log::error!(
                "refusing pane {pane_id} root assignment: bound tab {} is not structurally empty",
                self.tab_id
            );
            return;
        }
        inner.assign_pane(pane);
        authority.commit_structural_bind(structural, tab_mux_owner_generation);
        prepared_counts.commit(&mut windows, &mut workspace_counts);
    }

    /// Swap the active pane with the specified pane_index
    pub fn swap_active_with_index(&self, pane_index: usize, keep_focus: bool) -> Option<()> {
        self.inner
            .lock()
            .swap_active_with_index(pane_index, keep_focus)
    }

    /// Computes the size of the pane that would result if the specified
    /// pane was split in a particular direction.
    /// The intent is to call this prior to spawning the new pane so that
    /// you can create it with the correct size.
    /// May return None if the specified pane_index is invalid.
    pub fn compute_split_size(
        &self,
        pane_index: usize,
        request: SplitRequest,
    ) -> Option<SplitDirectionAndSize> {
        // A non-top-level split cannot be computed against zoom geometry.
        // Perform the unzoom transaction through the outer deferred-callback
        // boundary before taking the topology lock for the pure computation.
        if !request.top_level {
            self.set_zoomed(false);
        }
        self.inner.lock().compute_split_size(pane_index, request)
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_unregistered_tiled_pane(
        self: &Arc<Self>,
        mux: &Arc<Mux>,
        expected_domain: &Arc<dyn Domain>,
        expected_domain_id: DomainId,
        expected_window_id: WindowId,
        split: Option<(&PaneRegistrationHandle, SplitRequest)>,
        pane: &Arc<dyn Pane>,
    ) -> anyhow::Result<(PaneRegistrationHandle, usize)> {
        anyhow::ensure!(
            split.is_some() || expected_window_id.is_none(),
            "root pane publication must target an unattached tab"
        );
        let pane_id = observe_pane_id_for_mutation(pane)?;
        let mut preparation_claim = mux
            .claim_pane_preparation(pane)?
            .ok_or_else(|| anyhow::anyhow!("unregistered tiled pane is already registered"))?;
        anyhow::ensure!(
            preparation_claim.domain_id == expected_domain_id,
            "prepared tiled pane {pane_id} belongs to domain {} rather than {expected_domain_id}",
            preparation_claim.domain_id
        );
        let prepared = mux.prepare_claimed_pane_registration(
            pane,
            preparation_claim.pane_id,
            &preparation_claim.generation,
        )?;
        let (mut reader_start_gate, registration_reservation) =
            mux.spawn_prepared_pane_reader(pane, prepared, &preparation_claim.generation)?;

        // All pane callbacks, geometry work, successor allocation, and
        // notification allocation complete before the combined registry and
        // topology cut. The exact baseline is revalidated under Tab::inner.
        let mut baseline = {
            let inner = self.inner.lock();
            anyhow::ensure!(
                inner.is_active_mux_owner(mux),
                "tab {} lost active mux-owner authority",
                self.tab_id
            );
            if split.is_some() {
                admit_moved_split_tree_clone(&inner)?;
            }
            inner.clone()
        };
        let prior_structural_count = {
            let (tiled, floating) = baseline.snapshot_structural_panes_callback_free_checked()?;
            tiled
                .len()
                .checked_add(floating.len())
                .ok_or_else(|| anyhow::anyhow!("tiled publication pane count overflow"))?
        };
        let mut replacement = baseline.clone();
        let (mut callbacks, target_registration, inserted_index) = match split {
            Some((target_registration, request)) => {
                anyhow::ensure!(
                    target_registration
                        .owner()
                        .is_some_and(|owner| Arc::ptr_eq(&owner, mux)),
                    "tiled split target belongs to another mux registration"
                );
                anyhow::ensure!(
                    target_registration.pane_id() != pane_id,
                    "cannot split pane {pane_id} beside itself"
                );
                let pane_index = baseline
                    .iter_panes_ignoring_zoom()
                    .into_iter()
                    .find(|positioned| target_registration.is_same_pane(&positioned.pane))
                    .map(|positioned| positioned.index)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "exact split target registration {} is not tiled in tab {}",
                            target_registration.pane_id(),
                            self.tab_id
                        )
                    })?;
                let mut observed_panes = baseline.snapshot_panes_callback_free();
                observed_panes.push(Arc::clone(pane));
                let observed = Self::observe_panes(observed_panes);
                let pane_ids = build_callback_pane_id_snapshot(self.tab_id, &observed)?;
                let (inserted, callbacks) = replacement.prepare_split_and_insert(
                    pane_index,
                    request,
                    Arc::clone(pane),
                    &pane_ids,
                )?;
                (callbacks, Some(target_registration), inserted)
            }
            None => {
                anyhow::ensure!(
                    prior_structural_count == 0,
                    "root-pane publication requires structurally empty tab {}",
                    self.tab_id
                );
                replacement.assign_pane(pane);
                (DeferredTabCallbacks::default(), None, 0)
            }
        };
        let next_structural_count = {
            let (tiled, floating) = replacement.snapshot_structural_panes_callback_free_checked()?;
            tiled
                .len()
                .checked_add(floating.len())
                .ok_or_else(|| anyhow::anyhow!("tiled successor pane count overflow"))?
        };
        anyhow::ensure!(
            next_structural_count == prior_structural_count.checked_add(1).ok_or_else(|| {
                anyhow::anyhow!("tiled publication structural count overflow")
            })?,
            "tiled publication successor did not add exactly one structural pane"
        );
        let topology_notification_count =
            callbacks.reserve_relocation_topology_notifications()?;
        let revision_count = topology_notification_count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("tiled publication revision count overflow"))?;

        let (registration, lifecycle_ticket, retired_inner) = {
            let _domain_registration = mux.domain_registration.lock();
            anyhow::ensure!(
                !mux.retired_domain_ids.lock().contains(&expected_domain_id)
                    && mux
                        .domains
                        .read()
                        .get(&expected_domain_id)
                        .is_some_and(|domain| Arc::ptr_eq(domain, expected_domain)),
                "tiled-pane domain retired or changed identity before commit"
            );
            let _pane_registration = mux.pane_registration.lock();
            let mut authority = mux.pane_authority.lock();
            let registered_tabs = mux.tabs.read();
            let registered_tab = registered_tabs
                .get(&self.tab_id)
                .filter(|registered| Arc::ptr_eq(registered, self))
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "destination tab {} retired or changed identity before tiled commit",
                        self.tab_id
                    )
                })?;
            let tab_mux_owner_generation = self.active_mux_owner_generation().ok_or_else(|| {
                anyhow::anyhow!(
                    "destination tab {} lacks active mux-owner generation before tiled commit",
                    self.tab_id
                )
            })?;
            let mut windows = mux.windows.write();
            let tab_parents = mux.tab_parents.read();
            match expected_window_id {
                Some(expected_window_id) => anyhow::ensure!(
                    tab_parents
                        .get(&self.tab_id)
                        .is_some_and(|parent| parent.matches(self, expected_window_id))
                        && windows
                            .get(&expected_window_id)
                            .is_some_and(|window| window.iter().any(|tab| Arc::ptr_eq(tab, self))),
                    "destination tab {} changed exact window parent before tiled commit",
                    self.tab_id
                ),
                None => anyhow::ensure!(
                    !tab_parents.contains_key(&self.tab_id)
                        && windows
                            .values()
                            .all(|window| window.iter().all(|tab| !Arc::ptr_eq(tab, self))),
                    "unattached destination tab {} acquired a window parent before root-pane commit",
                    self.tab_id
                ),
            }
            let mut workspace_counts = mux.num_panes_by_workspace.write();

            if let Some(target_registration) = target_registration {
                let target_pane = {
                    let panes = mux.panes.read();
                    let registered = panes
                        .get(&target_registration.pane_id())
                        .filter(|registered| {
                            target_registration.matches_live_registration(registered)
                        })
                        .ok_or_else(|| {
                            anyhow::anyhow!("tiled split target registration retired before commit")
                        })?;
                    Arc::clone(&registered.pane)
                };
                authority.validate_structural_owner_exact(
                    target_registration.pane_id(),
                    &target_pane,
                    &registered_tab,
                    Some(PaneStructuralLane::Tiled),
                    Some(target_registration),
                )?;
            }

            let registration =
                PaneRegistrationHandle::new(pane, &preparation_claim.generation);
            let prepared_authority = authority.prepare_live_registration_insert(
                pane_id,
                pane,
                expected_domain_id,
                Some(expected_domain),
            )?;
            let structural = authority.prepare_new_structural_bind(
                pane_id,
                Arc::clone(pane),
                Arc::clone(&registered_tab),
                PaneStructuralLane::Tiled,
                Some((registration.clone(), expected_domain_id)),
            )?;
            anyhow::ensure!(
                Mux::exact_tab_structural_pane_count(&authority, &registered_tab)?
                    == prior_structural_count,
                "tab {} structural authority changed before tiled commit",
                self.tab_id
            );
            let prepared_counts = mux.prepare_tab_pane_count_mutation_locked(
                &windows,
                &tab_parents,
                &mut workspace_counts,
                &[(
                    Arc::clone(&registered_tab),
                    prior_structural_count,
                    next_structural_count,
                )],
                "atomic unregistered tiled pane publication",
            )?;

            let mut inner = self.inner.lock();
            anyhow::ensure!(
                inner.is_active_mux_owner(mux)
                    && relocation_inner_matches_baseline(&inner, &baseline)?,
                "tab {} changed after tiled successor preparation",
                self.tab_id
            );
            if let Some(target_registration) = target_registration {
                anyhow::ensure!(
                    inner
                        .snapshot_panes_callback_free()
                        .iter()
                        .filter(|candidate| target_registration.is_same_pane(candidate))
                        .count()
                        == 1,
                    "tiled split target left or duplicated inside destination tab"
                );
            }
            anyhow::ensure!(
                inner
                    .snapshot_panes_callback_free()
                    .iter()
                    .all(|candidate| !Arc::ptr_eq(candidate, pane)),
                "unregistered tiled pane acquired a structural owner before commit"
            );
            anyhow::ensure!(
                preparation_claim.is_authoritative_locked(),
                "unregistered tiled-pane preparation was cancelled"
            );
            let mut panes = mux.panes.write();
            anyhow::ensure!(
                !panes.contains_key(&pane_id),
                "tiled pane id {pane_id} became registered before commit"
            );
            panes
                .try_reserve(1)
                .map_err(|error| anyhow::anyhow!("reserve tiled pane registry slot: {error}"))?;
            let lifecycle_enqueue = mux.prepare_pane_lifecycle_enqueue(pane_id)?;
            let mut topology = mux.topology.lock();
            let first_revision = topology
                .reserve_revisions(revision_count)
                .map_err(anyhow::Error::new)?;
            let topology_stamp = crate::MuxTopologyStamp::Revision(first_revision);
            let callback_first_revision = (topology_notification_count != 0).then(|| {
                crate::TopologyRevision::new(
                    first_revision
                        .get()
                        .checked_add(1)
                        .expect("reserved tiled topology range cannot overflow"),
                )
            });

            let commit_guard = registration_reservation
                .commit()
                .expect("validated exclusive tiled registration reservation must commit");
            let prior = panes.insert(
                pane_id,
                crate::LivePaneRegistration {
                    pane: Arc::clone(pane),
                    generation: Arc::clone(&preparation_claim.generation),
                    domain_id: expected_domain_id,
                },
            );
            debug_assert!(prior.is_none());
            let retired_inner = std::mem::replace(&mut *inner, replacement);
            authority.insert_live_registration(
                pane_id,
                pane,
                expected_domain_id,
                registration.clone(),
                prepared_authority,
            );
            authority.commit_structural_bind(structural, tab_mux_owner_generation);
            prepared_counts.commit(&mut windows, &mut workspace_counts);
            let consumed = callbacks.stamp_relocation_topology_notifications(
                self.tab_id,
                callback_first_revision,
                0,
            );
            debug_assert_eq!(consumed, topology_notification_count);
            let lifecycle_ticket = lifecycle_enqueue.enqueue(
                crate::PaneLifecycleNotification::Added(pane_id),
                topology_stamp,
                reader_start_gate.take(),
                None,
                crate::PaneRemovalFollowUp::None,
            );
            let finalized = commit_guard.finalize();
            debug_assert!(registration.same_registration(&finalized));
            preparation_claim.retire_locked();
            (registration, lifecycle_ticket, retired_inner)
        };

        drop(retired_inner);
        callbacks.execute(Some(mux));
        mux.complete_pane_lifecycle_notification(lifecycle_ticket);
        mux.notify_pane_registration_did_bind(pane, &registration);
        Ok((registration, inserted_index))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit_unregistered_root_pane(
        self: &Arc<Self>,
        mux: &Arc<Mux>,
        expected_domain: &Arc<dyn Domain>,
        expected_domain_id: DomainId,
        pane: &Arc<dyn Pane>,
    ) -> anyhow::Result<PaneRegistrationHandle> {
        self.commit_unregistered_tiled_pane(
            mux,
            expected_domain,
            expected_domain_id,
            None,
            None,
            pane,
        )
        .map(|(registration, _)| registration)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit_unregistered_split_pane(
        self: &Arc<Self>,
        mux: &Arc<Mux>,
        expected_domain: &Arc<dyn Domain>,
        expected_domain_id: DomainId,
        expected_window_id: WindowId,
        target: &PaneRegistrationHandle,
        request: SplitRequest,
        pane: &Arc<dyn Pane>,
    ) -> anyhow::Result<PaneRegistrationHandle> {
        self.commit_unregistered_tiled_pane(
            mux,
            expected_domain,
            expected_domain_id,
            Some(expected_window_id),
            Some((target, request)),
            pane,
        )
        .map(|(registration, _)| registration)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit_unregistered_unattached_split_pane(
        self: &Arc<Self>,
        mux: &Arc<Mux>,
        expected_domain: &Arc<dyn Domain>,
        expected_domain_id: DomainId,
        target: &PaneRegistrationHandle,
        request: SplitRequest,
        pane: &Arc<dyn Pane>,
    ) -> anyhow::Result<(PaneRegistrationHandle, usize)> {
        self.commit_unregistered_tiled_pane(
            mux,
            expected_domain,
            expected_domain_id,
            None,
            Some((target, request)),
            pane,
        )
    }

    /// Split the pane that has pane_index in the given direction and assign
    /// the right/bottom pane of the newly created split to the provided Pane
    /// instance.  Returns the resultant index of the newly inserted pane.
    /// Both the split and the inserted pane will be resized.
    pub fn split_and_insert(
        &self,
        pane_index: usize,
        request: SplitRequest,
        pane: Arc<dyn Pane>,
    ) -> anyhow::Result<usize> {
        let pane_id = observe_pane_id_for_mutation(&pane)?;
        let owner = self.inner.lock().notification_owner();
        let Some(mux) = owner else {
            return self
                .inner
                .lock()
                .split_and_insert(pane_index, request, pane);
        };

        // Perform every callback-bearing geometry/constraint observation and
        // every fallible successor allocation before taking mux authority.
        // The final cut only revalidates and swaps this complete successor.
        let baseline = {
            let inner = self.inner.lock();
            anyhow::ensure!(
                inner.is_active_mux_owner(&mux),
                "tab {} lost active mux-owner authority",
                self.tab_id
            );
            admit_moved_split_tree_clone(&inner)?;
            inner.clone()
        };
        let mut observed_panes = baseline.snapshot_panes_callback_free();
        observed_panes.push(Arc::clone(&pane));
        let observed = Self::observe_panes(observed_panes);
        let pane_ids = build_callback_pane_id_snapshot(self.tab_id, &observed)?;
        let mut replacement = baseline.clone();
        let (inserted, mut callbacks) = replacement.prepare_split_and_insert(
            pane_index,
            request,
            Arc::clone(&pane),
            &pane_ids,
        )?;
        let structural_count = |inner: &TabInner| -> anyhow::Result<usize> {
            let (tiled, floating) = inner.snapshot_structural_panes_callback_free_checked()?;
            tiled
                .len()
                .checked_add(floating.len())
                .ok_or_else(|| anyhow::anyhow!("tab structural pane count overflow"))
        };
        let prior_count = structural_count(&baseline)?;
        let next_count = structural_count(&replacement)?;

        let _registration = mux.pane_registration.lock();
        let mut authority = mux.pane_authority.lock();
        let registered_tabs = mux.tabs.read();
        let registered_tab = registered_tabs
            .get(&self.tab_id)
            .filter(|registered| std::ptr::eq(Arc::as_ptr(registered), self))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("tab {} lost exact mux registration", self.tab_id))?;
        let tab_mux_owner_generation = registered_tab
            .active_mux_owner_generation()
            .ok_or_else(|| {
                anyhow::anyhow!("tab {} lacks active mux-owner generation", self.tab_id)
            })?;
        let mut windows = mux.windows.write();
        let tab_parents = mux.tab_parents.read();
        let mut workspace_counts = mux.num_panes_by_workspace.write();
        let (pane_registration, domain_id) = {
            let panes = mux.panes.read();
            let registered = panes.get(&pane_id).ok_or_else(|| {
                anyhow::anyhow!("split pane {pane_id} is not registered in the owning mux")
            })?;
            anyhow::ensure!(
                Arc::ptr_eq(&registered.pane, &pane),
                "split pane {pane_id} was replaced by another exact allocation"
            );
            (
                PaneRegistrationHandle::new(&registered.pane, &registered.generation),
                registered.domain_id,
            )
        };
        anyhow::ensure!(
            authority.contains_live_registration(
                pane_id,
                domain_id,
                &pane_registration,
            ),
            "split pane {pane_id} lacks exact domain-registration authority"
        );
        let structural = authority.prepare_new_structural_bind(
            pane_id,
            Arc::clone(&pane),
            Arc::clone(&registered_tab),
            PaneStructuralLane::Tiled,
            Some((pane_registration, domain_id)),
        )?;
        anyhow::ensure!(
            Mux::exact_tab_structural_pane_count(&authority, &registered_tab)? == prior_count,
            "tab {} structural authority count changed after split preparation",
            self.tab_id
        );
        let prepared_counts = mux.prepare_tab_pane_count_mutation_locked(
            &windows,
            &tab_parents,
            &mut workspace_counts,
            &[(Arc::clone(&registered_tab), prior_count, next_count)],
            "bound split insertion",
        )?;
        let mut inner = self.inner.lock();
        anyhow::ensure!(
            inner.is_active_mux_owner(&mux)
                && relocation_inner_matches_baseline(&inner, &baseline)?,
            "tab {} changed after split preparation",
            self.tab_id
        );
        callbacks.topology_notifications =
            callbacks.prepare_topology_notifications(&mux, self.tab_id)?;
        let retired_inner = std::mem::replace(&mut *inner, replacement);
        authority.commit_structural_bind(structural, tab_mux_owner_generation);
        prepared_counts.commit(&mut windows, &mut workspace_counts);
        drop(inner);
        drop(workspace_counts);
        drop(tab_parents);
        drop(windows);
        drop(registered_tabs);
        drop(authority);
        drop(_registration);
        drop(retired_inner);
        callbacks.execute(Some(&mux));
        Ok(inserted)
    }

    pub fn get_zoomed_pane(&self) -> Option<Arc<dyn Pane>> {
        self.inner.lock().get_zoomed_pane()
    }

    /// Prepare an admitted exact pane move into a new, still-unpublished tab.
    ///
    /// Pane callbacks and every fallible successor allocation complete before
    /// the outer mux transaction takes its registry cut.  The returned
    /// destination tab is bound to this exact mux so structural authority can
    /// name its generation, but it is absent from the tab/window registries
    /// and its one-pane successor is not installed until `commit` below.
    pub(crate) fn prepare_guarded_move_to_new_tab(
        self: &Arc<Self>,
        mux: &Arc<Mux>,
        target: &PaneOperationGuard,
        destination_size: TerminalSize,
    ) -> anyhow::Result<PreparedGuardedMoveToNewTab> {
        anyhow::ensure!(
            target.belongs_to(mux),
            "move target registration {} belongs to another mux",
            target.pane_id()
        );
        let (_domain_id, _window_id, indexed_tab, source_lane) = mux
            .indexed_pane_location_for_operation(target)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "exact move registration {} is not attached to an indexed tab",
                    target.pane_id()
                )
            })?;
        anyhow::ensure!(
            Arc::ptr_eq(self, &indexed_tab),
            "exact move registration {} changed source tabs before preparation",
            target.pane_id()
        );

        let source_baseline = {
            let inner = self.inner.lock();
            anyhow::ensure!(
                inner.is_active_mux_owner(mux),
                "move source tab {} lost active mux authority",
                self.tab_id()
            );
            admit_moved_split_tree_clone(&inner)
                .context("admit move-to-new-tab source tree clone")?;
            inner.clone()
        };
        let pane_ids = observe_relocation_pane_ids(self.tab_id(), &[&source_baseline])?;
        anyhow::ensure!(
            pane_ids.get(&pane_identity(target.pane())) == Some(&target.pane_id()),
            "move source exact identity disagrees with its admitted pane id"
        );
        let source_current = exact_relocation_structural_state(&source_baseline, &pane_ids)?;
        anyhow::ensure!(
            source_current.iter().any(|state| {
                state.pane_id == target.pane_id()
                    && state.lane == source_lane
                    && Arc::ptr_eq(&state.pane, target.pane())
            }),
            "move source exact allocation left its admitted structural lane"
        );
        let source_observed = observed_relocation_panes(&source_baseline, &pane_ids)?;
        let source_candidate = ExactPaneRemovalCandidate {
            pane: Arc::clone(target.pane()),
            pane_id: target.pane_id(),
            expected_registration: Some(target.registration()),
            expected_lane: Some(source_lane),
        };
        let source_removal = source_baseline
            .prepare_exact_pane_removal(&source_observed, std::slice::from_ref(&source_candidate));
        anyhow::ensure!(
            source_removal.callbacks.changed
                && source_removal
                    .callbacks
                    .removed
                    .contains(&pane_identity(target.pane())),
            "exact move source was not removed from its admitted tab"
        );
        let source_replacement = source_removal.replacement;
        let source_desired =
            desired_relocation_structural_state(&source_replacement, &pane_ids)?;
        let source_tab_retires = source_desired.is_empty();
        let mut source_callbacks = source_removal.callbacks;
        if source_tab_retires {
            // The enclosing window transaction publishes the terminal source
            // tab retirement; never emit a stale TabResized for that ID.
            source_callbacks.changed = false;
        }

        let destination_tab = Arc::new(Tab::new(&destination_size));
        destination_tab
            .prepare_mux_owner_binding_if_structurally_empty(mux)?
            .commit();
        let destination_baseline = destination_tab.inner.lock().clone();
        let mut destination_replacement = destination_baseline.clone();
        destination_replacement.assign_pane(target.pane());
        let destination_desired =
            desired_relocation_structural_state(&destination_replacement, &pane_ids)?;
        anyhow::ensure!(
            destination_desired.len() == 1
                && destination_desired[0].pane_id == target.pane_id()
                && destination_desired[0].lane == PaneStructuralLane::Tiled
                && Arc::ptr_eq(&destination_desired[0].pane, target.pane()),
            "unpublished destination tab did not retain exactly the admitted tiled pane"
        );

        let mut destination_callbacks = DeferredTabCallbacks::default();
        destination_callbacks
            .resize_work
            .try_reserve_exact(1)
            .map_err(|error| anyhow::anyhow!("reserve moved-pane destination resize: {error}"))?;
        destination_callbacks
            .resize_work
            .push((Arc::clone(target.pane()), destination_size));

        let mut authority_replacements = Vec::new();
        authority_replacements
            .try_reserve_exact(2)
            .map_err(|error| anyhow::anyhow!("reserve move-to-new-tab authority tabs: {error}"))?;
        authority_replacements.push(StructuralRelocationTabReplacement {
            tab: Arc::clone(self),
            current: source_current,
            desired: source_desired,
        });
        authority_replacements.push(StructuralRelocationTabReplacement {
            tab: Arc::clone(&destination_tab),
            current: Vec::new(),
            desired: destination_desired,
        });

        Ok(PreparedGuardedMoveToNewTab {
            source_tab: Arc::clone(self),
            destination_tab,
            source_baseline,
            source_replacement,
            destination_baseline,
            destination_replacement,
            authority_replacements: Some(authority_replacements),
            source_callbacks,
            destination_callbacks,
            destination_size,
            source_tab_retires,
            topology_notification_count: None,
        })
    }
}

impl PreparedGuardedMoveToNewTab {
    pub(crate) fn destination_tab(&self) -> &Arc<Tab> {
        &self.destination_tab
    }

    pub(crate) const fn destination_size(&self) -> TerminalSize {
        self.destination_size
    }

    pub(crate) const fn source_size_at_preparation(&self) -> TerminalSize {
        self.source_baseline.size
    }

    pub(crate) const fn source_tab_retires(&self) -> bool {
        self.source_tab_retires
    }

    /// Allocate callback notification storage before the mux cut. The outer
    /// window transaction reserves one contiguous global revision range and
    /// supplies its trailing first revision to the commit token.
    pub(crate) fn reserve_topology_notifications(&mut self) -> anyhow::Result<usize> {
        anyhow::ensure!(
            self.topology_notification_count.is_none(),
            "move-to-new-tab topology notifications were prepared twice"
        );
        let source_count = self
            .source_callbacks
            .reserve_relocation_topology_notifications()?;
        let destination_count = self
            .destination_callbacks
            .reserve_relocation_topology_notifications()?;
        let count = source_count
            .checked_add(destination_count)
            .ok_or_else(|| anyhow::anyhow!("move-to-new-tab notification count overflow"))?;
        self.topology_notification_count = Some(count);
        Ok(count)
    }

    pub(crate) fn take_authority_replacements(
        &mut self,
    ) -> anyhow::Result<Vec<StructuralRelocationTabReplacement>> {
        self.authority_replacements.take().ok_or_else(|| {
            anyhow::anyhow!("move-to-new-tab authority replacements were already consumed")
        })
    }

    /// Retain both exact tab locks in stable pointer order and reject any
    /// resize, zoom, focus, stack, title, or structural change since the
    /// off-lock successors were prepared.
    pub(crate) fn lock_for_commit<'tabs>(
        &self,
        mux: &Mux,
        source_tab: &'tabs Arc<Tab>,
        destination_tab: &'tabs Arc<Tab>,
    ) -> anyhow::Result<LockedGuardedMoveToNewTab<'tabs>> {
        anyhow::ensure!(
            Arc::ptr_eq(source_tab, &self.source_tab)
                && Arc::ptr_eq(destination_tab, &self.destination_tab)
                && !Arc::ptr_eq(source_tab, destination_tab),
            "move-to-new-tab commit tabs do not match the prepared exact allocations"
        );
        let source_is_first = (Arc::as_ptr(source_tab) as usize)
            < (Arc::as_ptr(destination_tab) as usize);
        let (first_tab, second_tab) = if source_is_first {
            (source_tab, destination_tab)
        } else {
            (destination_tab, source_tab)
        };
        let first_inner = first_tab.inner.lock();
        let second_inner = second_tab.inner.lock();
        let (source_inner, destination_inner) = if source_is_first {
            (&*first_inner, &*second_inner)
        } else {
            (&*second_inner, &*first_inner)
        };
        anyhow::ensure!(
            source_inner.is_active_mux_owner(mux)
                && destination_inner.is_active_mux_owner(mux)
                && relocation_inner_matches_baseline(source_inner, &self.source_baseline)?
                && relocation_inner_matches_baseline(
                    destination_inner,
                    &self.destination_baseline,
                )?,
            "move-to-new-tab source or unpublished destination changed after preparation"
        );
        Ok(LockedGuardedMoveToNewTab {
            first_inner,
            second_inner,
            source_is_first,
        })
    }
}

impl LockedGuardedMoveToNewTab<'_> {
    /// Install both prepared tab successors without allocation or callback.
    /// Structural authority and mux/window registries are committed by the
    /// enclosing transaction while these exact locks remain held.
    pub(crate) fn commit(
        mut self,
        mux: &Mux,
        mut prepared: PreparedGuardedMoveToNewTab,
        first_revision: Option<crate::TopologyRevision>,
    ) -> CommittedGuardedMoveToNewTab {
        debug_assert!(prepared.authority_replacements.is_none());
        let expected_count = prepared
            .topology_notification_count
            .expect("move-to-new-tab notifications must be reserved before commit");
        let mut consumed = prepared
            .source_callbacks
            .stamp_relocation_topology_notifications(
                prepared.source_tab.tab_id(),
                first_revision,
                0,
            );
        consumed = prepared
            .destination_callbacks
            .stamp_relocation_topology_notifications(
                prepared.destination_tab.tab_id(),
                first_revision,
                consumed,
            );
        debug_assert_eq!(consumed, expected_count);

        let (source_inner, destination_inner) = if self.source_is_first {
            (&mut *self.first_inner, &mut *self.second_inner)
        } else {
            (&mut *self.second_inner, &mut *self.first_inner)
        };
        let retired_source_inner =
            std::mem::replace(source_inner, prepared.source_replacement);
        let retired_destination_inner =
            std::mem::replace(destination_inner, prepared.destination_replacement);
        if prepared.source_tab_retires {
            let retired = source_inner.retire_mux_owner(mux);
            debug_assert!(retired);
            prepared
                .source_tab
                .mux_owner_generation
                .store(0, Ordering::Release);
        }

        CommittedGuardedMoveToNewTab {
            source_callbacks: prepared.source_callbacks,
            destination_callbacks: prepared.destination_callbacks,
            retired_source_inner,
            retired_destination_inner,
        }
    }
}

impl Mux {
    /// Relocate one admitted exact pane into a split beside another admitted
    /// exact pane in one indivisible mux topology cut.
    ///
    /// Both [`PaneOperationGuard`] values remain authoritative after their
    /// numeric registry slots are detached. The transaction preserves that
    /// detached state rather than re-registering either pane, prepares full
    /// same-tab or cross-tab successors before taking mux locks, and folds an
    /// empty source-tab/window retirement into the same revision reservation.
    pub(crate) fn commit_guarded_moved_split(
        self: &Arc<Self>,
        target_guard: &PaneOperationGuard,
        source_guard: &PaneOperationGuard,
        request: SplitRequest,
    ) -> anyhow::Result<SplitCommitReceipt> {
        anyhow::ensure!(
            target_guard.belongs_to(self) && source_guard.belongs_to(self),
            "split source and target must belong to the originating mux"
        );
        anyhow::ensure!(
            !target_guard.same_registration(source_guard),
            "cannot move pane {} into a split of itself",
            target_guard.pane_id()
        );

        let (_target_domain_id, target_window_id, target_tab, target_lane) = self
            .indexed_pane_location_for_operation(target_guard)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "exact split target registration {} is not attached to an indexed tab",
                    target_guard.pane_id()
                )
            })?;
        let (_source_domain_id, source_window_id, source_tab, source_lane) = self
            .indexed_pane_location_for_operation(source_guard)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "exact split source registration {} is not attached to an indexed tab",
                    source_guard.pane_id()
                )
            })?;
        anyhow::ensure!(
            target_lane == PaneStructuralLane::Tiled,
            "exact split target registration {} is not tiled",
            target_guard.pane_id()
        );

        let same_tab = Arc::ptr_eq(&target_tab, &source_tab);
        let (target_baseline, source_baseline) = if same_tab {
            let inner = target_tab.inner.lock();
            anyhow::ensure!(
                inner.is_active_mux_owner(self),
                "split target tab {} lost active mux authority",
                target_tab.tab_id()
            );
            admit_moved_split_tree_clone(&inner)?;
            (inner.clone(), None)
        } else {
            let target_first = (Arc::as_ptr(&target_tab) as usize)
                < (Arc::as_ptr(&source_tab) as usize);
            let (first_tab, second_tab) = if target_first {
                (&target_tab, &source_tab)
            } else {
                (&source_tab, &target_tab)
            };
            let first = first_tab.inner.lock();
            let second = second_tab.inner.lock();
            anyhow::ensure!(
                first.is_active_mux_owner(self) && second.is_active_mux_owner(self),
                "split source or target tab lost active mux authority"
            );
            admit_moved_split_tree_clone(&first)?;
            admit_moved_split_tree_clone(&second)?;
            if target_first {
                (first.clone(), Some(second.clone()))
            } else {
                (second.clone(), Some(first.clone()))
            }
        };

        let pane_ids = match source_baseline.as_ref() {
            Some(source_baseline) => observe_relocation_pane_ids(
                target_tab.tab_id(),
                &[&target_baseline, source_baseline],
            )?,
            None => observe_relocation_pane_ids(target_tab.tab_id(), &[&target_baseline])?,
        };
        anyhow::ensure!(
            pane_ids.get(&pane_identity(target_guard.pane())) == Some(&target_guard.pane_id()),
            "split target exact identity disagrees with its admitted pane id"
        );
        anyhow::ensure!(
            pane_ids.get(&pane_identity(source_guard.pane())) == Some(&source_guard.pane_id()),
            "split source exact identity disagrees with its admitted pane id"
        );

        let source_observed = observed_relocation_panes(
            source_baseline.as_ref().unwrap_or(&target_baseline),
            &pane_ids,
        )?;
        let source_candidate = ExactPaneRemovalCandidate {
            pane: Arc::clone(source_guard.pane()),
            pane_id: source_guard.pane_id(),
            expected_registration: Some(source_guard.registration()),
            expected_lane: Some(source_lane),
        };

        let mut prepared = if same_tab {
            let current = exact_relocation_structural_state(&target_baseline, &pane_ids)?;
            anyhow::ensure!(
                current.iter().any(|state| {
                    state.pane_id == source_guard.pane_id()
                        && state.lane == source_lane
                        && Arc::ptr_eq(&state.pane, source_guard.pane())
                }),
                "split source exact allocation left its admitted structural lane"
            );
            admit_moved_split_tree_clone(&target_baseline)?;
            let removal = target_baseline
                .prepare_exact_pane_removal(&source_observed, std::slice::from_ref(&source_candidate));
            anyhow::ensure!(
                removal.callbacks.changed
                    && removal
                        .callbacks
                        .removed
                        .contains(&pane_identity(source_guard.pane())),
                "exact split source was not removed from its admitted tab"
            );
            let mut replacement = removal.replacement;
            let target_index = exact_tiled_relocation_index(&replacement, target_guard.pane())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "exact split target registration {} is not a tiled leaf",
                        target_guard.pane_id()
                    )
                })?;
            let (_inserted, insertion_callbacks) = replacement.prepare_split_and_insert(
                target_index,
                request,
                Arc::clone(source_guard.pane()),
                &pane_ids,
            )?;
            let source_size = insertion_callbacks
                .resize_work
                .iter()
                .rev()
                .find_map(|(pane, size)| {
                    Arc::ptr_eq(pane, source_guard.pane()).then_some(*size)
                })
                .ok_or_else(|| anyhow::anyhow!("prepared split omitted source resize geometry"))?;
            let desired = desired_relocation_structural_state(&replacement, &pane_ids)?;
            PreparedMovedSplit {
                source: None,
                target: PreparedMovedSplitTab {
                    baseline: target_baseline,
                    replacement,
                    current,
                    desired,
                    callbacks: merge_same_tab_relocation_callbacks(
                        removal.callbacks,
                        insertion_callbacks,
                    )?,
                },
                source_size,
                source_tab_retires: false,
            }
        } else {
            let source_baseline = source_baseline
                .expect("cross-tab relocation prepared an exact source baseline");
            let source_current = exact_relocation_structural_state(&source_baseline, &pane_ids)?;
            anyhow::ensure!(
                source_current.iter().any(|state| {
                    state.pane_id == source_guard.pane_id()
                        && state.lane == source_lane
                        && Arc::ptr_eq(&state.pane, source_guard.pane())
                }),
                "split source exact allocation left its admitted structural lane"
            );
            admit_moved_split_tree_clone(&source_baseline)?;
            let source_removal = source_baseline
                .prepare_exact_pane_removal(&source_observed, std::slice::from_ref(&source_candidate));
            anyhow::ensure!(
                source_removal.callbacks.changed
                    && source_removal
                        .callbacks
                        .removed
                        .contains(&pane_identity(source_guard.pane())),
                "exact split source was not removed from its admitted tab"
            );
            let source_replacement = source_removal.replacement;
            let source_desired =
                desired_relocation_structural_state(&source_replacement, &pane_ids)?;

            let target_current = exact_relocation_structural_state(&target_baseline, &pane_ids)?;
            admit_moved_split_tree_clone(&target_baseline)?;
            let mut target_replacement = target_baseline.clone();
            let target_index = exact_tiled_relocation_index(&target_replacement, target_guard.pane())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "exact split target registration {} is not a tiled leaf",
                        target_guard.pane_id()
                    )
                })?;
            let (_inserted, target_callbacks) = target_replacement.prepare_split_and_insert(
                target_index,
                request,
                Arc::clone(source_guard.pane()),
                &pane_ids,
            )?;
            let source_size = target_callbacks
                .resize_work
                .iter()
                .rev()
                .find_map(|(pane, size)| {
                    Arc::ptr_eq(pane, source_guard.pane()).then_some(*size)
                })
                .ok_or_else(|| anyhow::anyhow!("prepared split omitted source resize geometry"))?;
            let target_desired =
                desired_relocation_structural_state(&target_replacement, &pane_ids)?;
            let source_tab_retires = source_desired.is_empty();
            PreparedMovedSplit {
                source: Some(PreparedMovedSplitTab {
                    baseline: source_baseline,
                    replacement: source_replacement,
                    current: source_current,
                    desired: source_desired,
                    callbacks: source_removal.callbacks,
                }),
                target: PreparedMovedSplitTab {
                    baseline: target_baseline,
                    replacement: target_replacement,
                    current: target_current,
                    desired: target_desired,
                    callbacks: target_callbacks,
                },
                source_size,
                source_tab_retires,
            }
        };

        if prepared.source_tab_retires {
            // WindowTopologyChanged owns the structural retirement revision;
            // retain resize/focus effects without publishing a stale TabResized.
            if let Some(source) = prepared.source.as_mut() {
                source.callbacks.changed = false;
            }
        }
        let source_notification_count = match prepared.source.as_mut() {
            Some(source) => source
                .callbacks
                .reserve_relocation_topology_notifications()?,
            None => 0,
        };
        let target_notification_count = prepared
            .target
            .callbacks
            .reserve_relocation_topology_notifications()?;
        let trailing_revision_count = source_notification_count
            .checked_add(target_notification_count)
            .ok_or_else(|| anyhow::anyhow!("moved-split topology notification count overflow"))?;

        let PreparedMovedSplit {
            source,
            target,
            source_size,
            source_tab_retires,
        } = prepared;
        let PreparedMovedSplitTab {
            baseline: target_baseline,
            replacement: target_replacement,
            current: target_current,
            desired: target_desired,
            callbacks: mut target_callbacks,
        } = target;
        let (
            source_baseline,
            source_replacement,
            source_current,
            source_desired,
            mut source_callbacks,
        ) = match source {
            Some(source) => (
                Some(source.baseline),
                Some(source.replacement),
                Some(source.current),
                Some(source.desired),
                Some(source.callbacks),
            ),
            None => (None, None, None, None, None),
        };

        let mut authority_replacements = Vec::new();
        authority_replacements
            .try_reserve_exact(usize::from(source_current.is_some()).saturating_add(1))
            .map_err(|error| anyhow::anyhow!("reserve moved-split authority tabs: {error}"))?;
        if let (Some(current), Some(desired)) = (source_current, source_desired) {
            authority_replacements.push(StructuralRelocationTabReplacement {
                tab: Arc::clone(&source_tab),
                current,
                desired,
            });
        }
        authority_replacements.push(StructuralRelocationTabReplacement {
            tab: Arc::clone(&target_tab),
            current: target_current,
            desired: target_desired,
        });

        let mut pane_count_deltas = Vec::new();
        pane_count_deltas
            .try_reserve_exact(if source_window_id == target_window_id { 1 } else { 2 })
            .map_err(|error| anyhow::anyhow!("reserve moved-split pane-count deltas: {error}"))?;
        if source_window_id == target_window_id {
            pane_count_deltas.push(crate::WindowPaneCountDelta::identity(source_window_id));
        } else {
            pane_count_deltas.push(crate::WindowPaneCountDelta::new(source_window_id, 1, 0));
            pane_count_deltas.push(crate::WindowPaneCountDelta::new(target_window_id, 0, 1));
        }

        let mut retired_source_inner = None;
        let mut retired_target_inner = None;
        {
            let _domain_registration = self.domain_registration.lock();
            let _pane_registration = self.pane_registration.lock();
            let mut authority = self.pane_authority.lock();
            let mut tabs = self.tabs.write();
            anyhow::ensure!(
                tabs.get(&target_tab.tab_id())
                    .is_some_and(|tab| Arc::ptr_eq(tab, &target_tab)),
                "exact moved-split target tab left the mux before commit"
            );
            anyhow::ensure!(
                tabs.get(&source_tab.tab_id())
                    .is_some_and(|tab| Arc::ptr_eq(tab, &source_tab)),
                "exact moved-split source tab left the mux before commit"
            );
            let mut windows = self.windows.write();
            let parents = self.tab_parents.read();
            {
                let target_parent = parents.get(&target_tab.tab_id()).ok_or_else(|| {
                    anyhow::anyhow!("moved-split target tab lost its indexed window parent")
                })?;
                anyhow::ensure!(
                    target_parent.matches(&target_tab, target_window_id)
                        && windows.get(&target_window_id).is_some_and(|window| {
                            window.iter().any(|tab| Arc::ptr_eq(tab, &target_tab))
                        }),
                    "moved-split target tab changed exact window parent before commit"
                );
                let source_parent = parents.get(&source_tab.tab_id()).ok_or_else(|| {
                    anyhow::anyhow!("moved-split source tab lost its indexed window parent")
                })?;
                anyhow::ensure!(
                    source_parent.matches(&source_tab, source_window_id)
                        && windows.get(&source_window_id).is_some_and(|window| {
                            window.iter().any(|tab| Arc::ptr_eq(tab, &source_tab))
                        }),
                    "moved-split source tab changed exact window parent before commit"
                );
            }
            drop(parents);

            let (prepared_windows, removed_windows) = if source_tab_retires {
                let (prepared_windows, detach_count_deltas) = self.prepare_exact_tab_detach_locked(
                    &authority,
                    &windows,
                    std::slice::from_ref(&source_tab),
                    None,
                    false,
                    "guarded moved-split source retirement",
                )?;
                anyhow::ensure!(
                    detach_count_deltas.iter().any(|delta| {
                        delta.window_id == source_window_id && delta.removals == 1
                    }),
                    "retiring moved-split source tab did not carry its exact one-pane count"
                );
                let mut removed_windows = Vec::new();
                removed_windows
                    .try_reserve_exact(prepared_windows.len())
                    .map_err(|error| {
                        anyhow::anyhow!("reserve moved-split empty-window receipts: {error}")
                    })?;
                {
                    let provisional = self.provisional_windows.lock();
                    removed_windows.extend(prepared_windows.iter().filter_map(
                        |(window_id, state)| {
                            (state.frozen().ordered_tabs().is_empty()
                                && !provisional.contains(window_id))
                            .then_some(*window_id)
                        },
                    ));
                }
                (prepared_windows, removed_windows)
            } else {
                (Vec::new(), Vec::new())
            };

            let mut tab_parents = self.tab_parents.write();
            let target_parent = tab_parents.get(&target_tab.tab_id()).ok_or_else(|| {
                anyhow::anyhow!("moved-split target tab lost its indexed window parent")
            })?;
            anyhow::ensure!(
                target_parent.matches(&target_tab, target_window_id)
                    && windows.get(&target_window_id).is_some_and(|window| {
                        window.iter().any(|tab| Arc::ptr_eq(tab, &target_tab))
                    }),
                "moved-split target tab changed exact window parent before commit"
            );
            let source_parent = tab_parents.get(&source_tab.tab_id()).ok_or_else(|| {
                anyhow::anyhow!("moved-split source tab lost its indexed window parent")
            })?;
            anyhow::ensure!(
                source_parent.matches(&source_tab, source_window_id)
                    && windows.get(&source_window_id).is_some_and(|window| {
                        window.iter().any(|tab| Arc::ptr_eq(tab, &source_tab))
                    }),
                "moved-split source tab changed exact window parent before commit"
            );
            let mut workspace_counts = self.num_panes_by_workspace.write();
            let prepared_counts = self.prepare_pane_count_mutation_locked(
                &windows,
                &mut workspace_counts,
                &pane_count_deltas,
                "guarded moved split",
            )?;

            if same_tab {
                let mut target_inner = target_tab.inner.lock();
                anyhow::ensure!(
                    target_inner.is_active_mux_owner(self)
                        && relocation_inner_matches_baseline(
                            &target_inner,
                            &target_baseline,
                        )?,
                    "moved-split target tab changed after successor preparation"
                );
                populate_relocation_live_metadata(
                    self,
                    &mut authority_replacements[0].desired,
                )?;
                let prepared_authority = authority.prepare_structural_relocation(
                    self,
                    &[target_guard, source_guard],
                    authority_replacements,
                )?;
                self.commit_with_reserved_pane_retirement_revisions(
                    trailing_revision_count,
                    |first_revision| {
                        let consumed = target_callbacks
                            .stamp_relocation_topology_notifications(
                                target_tab.tab_id(),
                                first_revision,
                                0,
                            );
                        debug_assert_eq!(consumed, trailing_revision_count);
                        retired_target_inner = Some(std::mem::replace(
                            &mut *target_inner,
                            target_replacement,
                        ));
                        prepared_authority.commit();
                    },
                )?;
                prepared_counts.commit(&mut windows, &mut workspace_counts);
            } else {
                let source_first = (Arc::as_ptr(&source_tab) as usize)
                    < (Arc::as_ptr(&target_tab) as usize);
                let (first_tab, second_tab) = if source_first {
                    (&source_tab, &target_tab)
                } else {
                    (&target_tab, &source_tab)
                };
                let mut first_inner = first_tab.inner.lock();
                let mut second_inner = second_tab.inner.lock();
                let (source_inner, target_inner) = if source_first {
                    (&mut *first_inner, &mut *second_inner)
                } else {
                    (&mut *second_inner, &mut *first_inner)
                };
                anyhow::ensure!(
                    source_inner.is_active_mux_owner(self)
                        && target_inner.is_active_mux_owner(self)
                        && relocation_inner_matches_baseline(
                            source_inner,
                            source_baseline.as_ref().expect(
                                "cross-tab moved split retained its source baseline",
                            ),
                        )?
                        && relocation_inner_matches_baseline(
                            target_inner,
                            &target_baseline,
                        )?,
                    "moved-split source or target tab changed after successor preparation"
                );
                populate_relocation_live_metadata(
                    self,
                    &mut authority_replacements[0].desired,
                )?;
                populate_relocation_live_metadata(
                    self,
                    &mut authority_replacements[1].desired,
                )?;
                let prepared_authority = authority.prepare_structural_relocation(
                    self,
                    &[target_guard, source_guard],
                    authority_replacements,
                )?;
                let source_replacement = source_replacement
                    .expect("cross-tab moved split prepared a source successor");
                let commit = |first_revision| {
                    let mut consumed = source_callbacks
                        .as_mut()
                        .expect("cross-tab moved split retained source callbacks")
                        .stamp_relocation_topology_notifications(
                            source_tab.tab_id(),
                            first_revision,
                            0,
                        );
                    consumed = target_callbacks.stamp_relocation_topology_notifications(
                        target_tab.tab_id(),
                        first_revision,
                        consumed,
                    );
                    debug_assert_eq!(consumed, trailing_revision_count);
                    retired_source_inner = Some(std::mem::replace(
                        source_inner,
                        source_replacement,
                    ));
                    retired_target_inner = Some(std::mem::replace(
                        target_inner,
                        target_replacement,
                    ));
                    prepared_authority.commit();
                    if source_tab_retires {
                        let retired = source_inner.retire_mux_owner(self);
                        debug_assert!(retired);
                        source_tab.mux_owner_generation.store(0, Ordering::Release);
                        let removed = tabs.remove(&source_tab.tab_id());
                        debug_assert!(removed
                            .is_some_and(|tab| Arc::ptr_eq(&tab, &source_tab)));
                    }
                };
                if source_tab_retires {
                    self.commit_prepared_window_states_with_prepared_authorities_locked(
                        &mut windows,
                        &mut tab_parents,
                        &mut workspace_counts,
                        prepared_counts,
                        prepared_windows,
                        Vec::new(),
                        Vec::new(),
                        removed_windows,
                        trailing_revision_count,
                        commit,
                    )?;
                } else {
                    debug_assert!(prepared_windows.is_empty());
                    self.commit_with_reserved_pane_retirement_revisions(
                        trailing_revision_count,
                        commit,
                    )?;
                    prepared_counts.commit(&mut windows, &mut workspace_counts);
                }
            }
        }
        drop(retired_source_inner);
        drop(retired_target_inner);

        if source_tab_retires {
            self.flush_window_notifications();
        }
        let target_config = catch_recoverable(
            RecoverablePanicSite::MuxPaneCallback,
            AssertUnwindSafe(|| target_guard.with_pane(|pane| pane.get_config())),
        );
        match target_config {
            Ok(Some(config)) => {
                if catch_recoverable(
                    RecoverablePanicSite::MuxPaneCallback,
                    AssertUnwindSafe(|| source_guard.with_pane(|pane| pane.set_config(config))),
                )
                .is_err()
                {
                    log::error!(
                        "pane configuration callback panicked for moved exact pane identity {:p}",
                        Arc::as_ptr(source_guard.pane())
                    );
                }
            }
            Ok(None) => {}
            Err(_) => {
                log::error!(
                    "split target configuration callback panicked for exact pane identity {:p}",
                    Arc::as_ptr(target_guard.pane())
                );
            }
        }
        if let Some(callbacks) = source_callbacks {
            callbacks.execute(Some(self));
        }
        target_callbacks.execute(Some(self));

        Ok(SplitCommitReceipt::from_exact_parts(
            Arc::clone(source_guard.pane()),
            source_guard.registration(),
            target_tab,
            target_window_id,
            source_size,
        ))
    }

    /// Reconcile one domain's complete floating-pane overlay against an
    /// authoritative remote snapshot.
    ///
    /// `authoritative_panes` names every pane from that domain which survives
    /// the snapshot, tiled or floating. `desired` names the floating subset.
    /// Existing foreign-domain floats are preserved. New floating-only panes
    /// are registered in the same structural cut that attaches them, while
    /// stale ownerless registrations are retired without invoking
    /// `Pane::kill`, `Pane::resize`, or `Pane::focus_changed`.
    ///
    /// Any callback, allocation, identity, geometry, domain, registration,
    /// tab, window, or topology-revision failure occurs before primary
    /// topology mutation; staged registration-slot commits remain armed for
    /// rollback until that cut becomes infallible. The final callback-free cut
    /// follows the mux lock order `domain_registration -> pane_registration ->
    /// pane_authority -> tabs -> windows -> workspace pane counts -> Tab::inner
    /// (stable pointer order) -> panes -> retiring panes -> pending output ->
    /// pending lifecycle -> topology`.
    pub fn reconcile_domain_floating_panes(
        self: &Arc<Self>,
        domain_id: DomainId,
        authoritative_panes: Vec<Arc<dyn Pane>>,
        desired: Vec<DomainFloatingPaneState>,
    ) -> anyhow::Result<DomainFloatingPaneReconcileReceipt> {
        anyhow::ensure!(
            authoritative_panes.len() <= MAX_DOMAIN_PANES_PER_RECONCILE,
            "authoritative domain snapshot has {} panes; maximum is {}",
            authoritative_panes.len(),
            MAX_DOMAIN_PANES_PER_RECONCILE
        );
        anyhow::ensure!(
            desired.len() <= MAX_DOMAIN_FLOATING_PANES_PER_RECONCILE,
            "authoritative domain snapshot has {} floating panes; maximum is {}",
            desired.len(),
            MAX_DOMAIN_FLOATING_PANES_PER_RECONCILE
        );
        let expected_domain = self
            .get_domain(domain_id)
            .ok_or_else(|| anyhow::anyhow!("domain {domain_id} is not registered"))?;

        let mut authoritative_by_identity = HashMap::new();
        authoritative_by_identity
            .try_reserve(authoritative_panes.len())
            .map_err(|error| anyhow::anyhow!("reserve authoritative pane identities: {error}"))?;
        let mut authoritative_ids = HashSet::new();
        authoritative_ids
            .try_reserve(authoritative_panes.len())
            .map_err(|error| anyhow::anyhow!("reserve authoritative pane ids: {error}"))?;
        for pane in authoritative_panes {
            let pane_id = observe_pane_id_for_mutation(&pane)?;
            let observed_domain_id = observe_pane_domain_id_for_mutation(&pane)?;
            anyhow::ensure!(
                observed_domain_id == domain_id,
                "authoritative pane {pane_id} belongs to domain {observed_domain_id}, not {domain_id}"
            );
            let identity = pane_identity(&pane);
            anyhow::ensure!(
                authoritative_by_identity
                    .insert(identity, (pane_id, Arc::clone(&pane)))
                    .is_none(),
                "authoritative pane {pane_id} appears more than once by exact identity"
            );
            anyhow::ensure!(
                authoritative_ids.insert(pane_id),
                "authoritative domain snapshot contains duplicate pane id {pane_id}"
            );
        }

        let mut desired_by_identity = HashMap::new();
        desired_by_identity
            .try_reserve(desired.len())
            .map_err(|error| anyhow::anyhow!("reserve desired floating identities: {error}"))?;
        let mut desired_ids = HashSet::new();
        desired_ids
            .try_reserve(desired.len())
            .map_err(|error| anyhow::anyhow!("reserve desired floating pane ids: {error}"))?;
        let mut desired_by_tab: HashMap<usize, Vec<DomainFloatingPaneState>> = HashMap::new();
        desired_by_tab
            .try_reserve(desired.len())
            .map_err(|error| anyhow::anyhow!("reserve desired floating tab index: {error}"))?;
        for state in desired {
            anyhow::ensure!(
                state.rect.width > 0 && state.rect.height > 0,
                "floating pane {} has an empty rectangle",
                state.pane_id
            );
            anyhow::ensure!(
                state.opacity.is_finite() && (0.0..=1.0).contains(&state.opacity),
                "floating pane {} has invalid opacity {}",
                state.pane_id,
                state.opacity
            );
            anyhow::ensure!(
                !state.focused || state.visible,
                "hidden floating pane {} cannot be focused",
                state.pane_id
            );
            let observed_id = observe_pane_id_for_mutation(&state.pane)?;
            anyhow::ensure!(
                observed_id == state.pane_id,
                "floating pane state names id {}, but its exact pane reports {observed_id}",
                state.pane_id
            );
            let observed_domain_id = observe_pane_domain_id_for_mutation(&state.pane)?;
            anyhow::ensure!(
                observed_domain_id == domain_id,
                "floating pane {} belongs to domain {observed_domain_id}, not {domain_id}",
                state.pane_id
            );
            let identity = pane_identity(&state.pane);
            let Some((authoritative_id, authoritative_pane)) =
                authoritative_by_identity.get(&identity)
            else {
                anyhow::bail!(
                    "floating pane {} is absent from the authoritative domain pane set",
                    state.pane_id
                );
            };
            anyhow::ensure!(
                *authoritative_id == state.pane_id && Arc::ptr_eq(authoritative_pane, &state.pane),
                "floating pane {} does not match its authoritative exact pane identity",
                state.pane_id
            );
            anyhow::ensure!(
                desired_by_identity
                    .insert(identity, state.pane_id)
                    .is_none(),
                "floating pane {} has more than one desired tab owner",
                state.pane_id
            );
            anyhow::ensure!(
                desired_ids.insert(state.pane_id),
                "floating pane id {} appears more than once",
                state.pane_id
            );
            desired_by_tab
                .entry(Arc::as_ptr(&state.tab) as usize)
                .or_default()
                .push(state);
        }

        let mut existing_authoritative = HashMap::new();
        existing_authoritative
            .try_reserve(authoritative_by_identity.len())
            .map_err(|error| anyhow::anyhow!("reserve authoritative registrations: {error}"))?;
        let mut prepared_new = Vec::new();
        prepared_new
            .try_reserve_exact(desired_by_identity.len())
            .map_err(|error| anyhow::anyhow!("reserve new floating publications: {error}"))?;

        for (&identity, (pane_id, pane)) in &authoritative_by_identity {
            if let Some(registration) = self.capture_pane_registration(pane) {
                anyhow::ensure!(
                    registration.pane_id() == *pane_id,
                    "authoritative pane registration changed numeric identity"
                );
                existing_authoritative.insert(identity, registration);
                continue;
            }
            anyhow::ensure!(
                desired_by_identity.contains_key(&identity),
                "authoritative tiled pane {pane_id} is not registered"
            );
            let Some(preparation_claim) = self.claim_pane_preparation(pane)? else {
                let registration = self.capture_pane_registration(pane).ok_or_else(|| {
                    anyhow::anyhow!(
                        "pane {pane_id} became registered without exact capture authority"
                    )
                })?;
                existing_authoritative.insert(identity, registration);
                continue;
            };
            anyhow::ensure!(
                preparation_claim.domain_id == domain_id,
                "new floating pane {pane_id} changed domain during preparation"
            );
            let prepared = self.prepare_claimed_pane_registration(
                pane,
                preparation_claim.pane_id,
                &preparation_claim.generation,
            )?;
            let (reader_start_gate, registration_reservation) =
                self.spawn_prepared_pane_reader(pane, prepared, &preparation_claim.generation)?;
            prepared_new.push(PreparedDomainPanePublication {
                pane: Arc::clone(pane),
                pane_id: *pane_id,
                preparation_claim,
                reader_start_gate,
                registration_reservation: Some(registration_reservation),
            });
        }
        prepared_new.sort_unstable_by_key(|prepared| prepared.pane_id);

        let (live_registrations, live_by_id, live_by_identity) = {
            let _registration = self.pane_registration.lock();
            let panes = self.panes.read();
            let mut live_registrations = Vec::new();
            live_registrations
                .try_reserve_exact(panes.len())
                .map_err(|error| anyhow::anyhow!("reserve live pane registrations: {error}"))?;
            let mut live_by_id = HashMap::new();
            live_by_id
                .try_reserve(panes.len())
                .map_err(|error| anyhow::anyhow!("reserve live pane id index: {error}"))?;
            let mut live_by_identity = HashMap::new();
            live_by_identity
                .try_reserve(panes.len())
                .map_err(|error| anyhow::anyhow!("reserve live pane identity index: {error}"))?;
            for (&pane_id, registered) in panes.iter() {
                let index = live_registrations.len();
                let registration =
                    PaneRegistrationHandle::new(&registered.pane, &registered.generation);
                live_registrations.push((
                    pane_id,
                    Arc::clone(&registered.pane),
                    registration,
                    registered.domain_id,
                ));
                anyhow::ensure!(
                    live_by_id.insert(pane_id, index).is_none(),
                    "live pane registry contains duplicate pane id {pane_id}"
                );
                anyhow::ensure!(
                    live_by_identity
                        .insert(pane_identity(&registered.pane), index)
                        .is_none(),
                    "one exact pane identity has multiple live registrations"
                );
            }
            (live_registrations, live_by_id, live_by_identity)
        };

        for (&identity, registration) in &existing_authoritative {
            let Some(&index) = live_by_identity.get(&identity) else {
                anyhow::bail!("authoritative pane registration retired during preflight");
            };
            let (pane_id, _, current, registered_domain_id) = &live_registrations[index];
            anyhow::ensure!(
                registration.same_registration(current),
                "authoritative pane {pane_id} changed registration generation during preflight"
            );
            anyhow::ensure!(
                *registered_domain_id == domain_id,
                "authoritative pane {pane_id} is registered to domain {registered_domain_id}, not {domain_id}"
            );
        }

        let registered_tabs_snapshot = self.tabs.read().clone();
        let windows_snapshot = self
            .windows
            .read()
            .iter()
            .map(|(&window_id, window)| {
                (
                    window_id,
                    window.get_workspace().to_string(),
                    window.iter().cloned().collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        let mut observed_tabs = registered_tabs_snapshot
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let window_tab_count = windows_snapshot
            .iter()
            .try_fold(0usize, |count, (_, _, tabs)| {
                count
                    .checked_add(tabs.len())
                    .ok_or_else(|| anyhow::anyhow!("observed window tab count overflow"))
            })?;
        let observed_tab_capacity = observed_tabs
            .len()
            .checked_add(window_tab_count)
            .ok_or_else(|| anyhow::anyhow!("observed tab capacity overflow"))?;
        let mut observed_tab_identities = HashSet::new();
        observed_tab_identities
            .try_reserve(observed_tab_capacity)
            .map_err(|error| anyhow::anyhow!("reserve observed tab identities: {error}"))?;
        for tab in &observed_tabs {
            anyhow::ensure!(
                observed_tab_identities.insert(Arc::as_ptr(tab) as usize),
                "registered tab map aliases one exact tab identity"
            );
        }
        for (_, _, tabs) in &windows_snapshot {
            for tab in tabs {
                if observed_tab_identities.insert(Arc::as_ptr(tab) as usize) {
                    observed_tabs.push(Arc::clone(tab));
                }
            }
        }
        observed_tabs.sort_unstable_by_key(|tab| Arc::as_ptr(tab) as usize);

        let mut parent_windows: HashMap<usize, Option<WindowId>> = HashMap::new();
        parent_windows
            .try_reserve(observed_tabs.len())
            .map_err(|error| anyhow::anyhow!("reserve tab parent index: {error}"))?;
        for tab in &observed_tabs {
            parent_windows.insert(Arc::as_ptr(tab) as usize, None);
        }
        for (window_id, _workspace, tabs) in &windows_snapshot {
            for tab in tabs {
                let identity = Arc::as_ptr(tab) as usize;
                let parent = parent_windows.get_mut(&identity).ok_or_else(|| {
                    anyhow::anyhow!("window {window_id} contains an unobserved tab")
                })?;
                anyhow::ensure!(
                    parent.is_none(),
                    "tab {} is attached to more than one window",
                    tab.tab_id
                );
                *parent = Some(*window_id);
            }
        }

        for (&tab_identity, states) in &desired_by_tab {
            let tab = &states[0].tab;
            anyhow::ensure!(
                registered_tabs_snapshot
                    .get(&tab.tab_id)
                    .is_some_and(|candidate| Arc::ptr_eq(candidate, tab)),
                "desired floating tab {} is not an exact live mux registration",
                tab.tab_id
            );
            anyhow::ensure!(
                parent_windows
                    .get(&tab_identity)
                    .copied()
                    .flatten()
                    .is_some(),
                "desired floating tab {} is not attached to exactly one window",
                tab.tab_id
            );
            anyhow::ensure!(
                states.iter().filter(|state| state.focused).count() <= 1,
                "desired tab {} has more than one focused floating pane",
                tab.tab_id
            );
        }

        let mut observed = Vec::new();
        observed
            .try_reserve_exact(observed_tabs.len())
            .map_err(|error| anyhow::anyhow!("reserve observed floating tabs: {error}"))?;
        for tab in observed_tabs {
            let inner = tab.inner.lock();
            if let Some(states) = desired_by_tab.get(&(Arc::as_ptr(&tab) as usize)) {
                for state in states {
                    anyhow::ensure!(
                        inner.clamp_floating_rect(state.rect) == state.rect,
                        "floating pane {} rectangle is outside local tab {} geometry",
                        state.pane_id,
                        tab.tab_id
                    );
                }
            }
            observed.push(ObservedDomainFloatingTab {
                parent_window_id: parent_windows
                    .get(&(Arc::as_ptr(&tab) as usize))
                    .copied()
                    .flatten(),
                panes: inner.snapshot_panes_callback_free(),
                non_floating_panes: inner.snapshot_non_floating_panes_callback_free(),
                tiled_tree: inner.pane.clone(),
                pane_stacks: inner.pane_stacks.clone(),
                floating_panes: inner.floating_panes.clone(),
                floating_focus: inner.floating_focus,
                zoomed_pane: inner.zoomed.as_ref().map(Arc::clone),
                size: inner.size,
                tab: Arc::clone(&tab),
            });
        }

        let mut structural_ids = HashMap::new();
        let structural_count = observed.iter().try_fold(0usize, |count, tab| {
            count
                .checked_add(tab.panes.len())
                .ok_or_else(|| anyhow::anyhow!("structural pane census count overflow"))
        })?;
        structural_ids
            .try_reserve(structural_count)
            .map_err(|error| anyhow::anyhow!("reserve structural pane identities: {error}"))?;
        for tab in &observed {
            for pane in &tab.panes {
                let identity = pane_identity(pane);
                if structural_ids.contains_key(&identity) {
                    continue;
                }
                let pane_id = observe_pane_id_for_mutation(pane)?;
                structural_ids.insert(identity, pane_id);
            }
        }

        let mut owner_counts: HashMap<PaneIdentity, (usize, usize)> = HashMap::new();
        owner_counts
            .try_reserve(structural_count)
            .map_err(|error| anyhow::anyhow!("reserve structural owner census: {error}"))?;
        for tab in &observed {
            for pane in &tab.non_floating_panes {
                let identity = pane_identity(pane);
                owner_counts.entry(identity).or_default().0 += 1;
            }
            for floating in &tab.floating_panes {
                let identity = pane_identity(&floating.pane);
                let observed_id = structural_ids.get(&identity).copied().ok_or_else(|| {
                    anyhow::anyhow!("floating pane is absent from its tab's structural census")
                })?;
                anyhow::ensure!(
                    observed_id == floating.pane_id,
                    "floating pane stored id {} disagrees with exact pane id {observed_id}",
                    floating.pane_id
                );
                owner_counts.entry(identity).or_default().1 += 1;
            }
        }

        for (&identity, &(non_floating, floating)) in &owner_counts {
            let pane_id = structural_ids[&identity];
            let Some(&live_index) = live_by_id.get(&pane_id) else {
                anyhow::bail!("structurally owned pane {pane_id} is not registered");
            };
            let (_, live_pane, _, registered_domain_id) = &live_registrations[live_index];
            anyhow::ensure!(
                pane_identity(live_pane) == identity,
                "structurally owned pane id {pane_id} resolves to another exact registration"
            );
            let authoritative = authoritative_by_identity.contains_key(&identity);
            let desired_floating = desired_by_identity.contains_key(&identity);
            if *registered_domain_id != domain_id {
                anyhow::ensure!(
                    non_floating.saturating_add(floating) == 1,
                    "foreign-domain pane {pane_id} has multiple structural owners"
                );
            } else if desired_floating {
                anyhow::ensure!(
                    non_floating == 0 && floating <= 1,
                    "desired floating pane {pane_id} is also tiled or multiply owned"
                );
            } else if authoritative {
                anyhow::ensure!(
                    non_floating == 1 && floating <= 1,
                    "authoritative tiled pane {pane_id} lacks one exact tiled owner or has duplicate floats"
                );
            } else {
                anyhow::ensure!(
                    non_floating == 0 && floating <= 1,
                    "stale domain pane {pane_id} remains tiled or multiply owned"
                );
            }
        }

        for (&identity, (pane_id, _)) in &authoritative_by_identity {
            let (non_floating, floating) = owner_counts.get(&identity).copied().unwrap_or_default();
            if desired_by_identity.contains_key(&identity) {
                anyhow::ensure!(
                    non_floating == 0 && floating <= 1,
                    "desired floating pane {pane_id} has an incompatible current owner"
                );
            } else {
                anyhow::ensure!(
                    non_floating == 1,
                    "authoritative tiled pane {pane_id} is missing from the prepared tiled topology"
                );
            }
        }

        let mut prepared_tabs = Vec::new();
        prepared_tabs
            .try_reserve_exact(observed.len())
            .map_err(|error| anyhow::anyhow!("reserve floating tab replacements: {error}"))?;
        let mut changed_tab_ids = Vec::new();
        changed_tab_ids
            .try_reserve_exact(observed.len())
            .map_err(|error| anyhow::anyhow!("reserve changed floating tabs: {error}"))?;
        let mut invalidated_window_ids = HashSet::new();
        invalidated_window_ids
            .try_reserve(observed.len())
            .map_err(|error| anyhow::anyhow!("reserve invalidated floating windows: {error}"))?;

        for tab in &observed {
            let states = desired_by_tab
                .get(&(Arc::as_ptr(&tab.tab) as usize))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let mut replacement = Vec::new();
            replacement
                .try_reserve_exact(tab.floating_panes.len().saturating_add(states.len()))
                .map_err(|error| {
                    anyhow::anyhow!(
                        "reserve floating replacement for tab {}: {error}",
                        tab.tab.tab_id
                    )
                })?;
            let mut foreign_focus = None;
            let mut desired_states = states.iter();
            for floating in &tab.floating_panes {
                let identity = pane_identity(&floating.pane);
                let pane_id = structural_ids[&identity];
                let live_index = live_by_id[&pane_id];
                let registered_domain_id = live_registrations[live_index].3;
                if registered_domain_id == domain_id {
                    if let Some(state) = desired_states.next() {
                        replacement.push(FloatingPane {
                            pane: Arc::clone(&state.pane),
                            pane_id: state.pane_id,
                            rect: state.rect,
                            z_order: state.z_order,
                            visible: state.visible,
                            pinned: state.pinned,
                            opacity: state.opacity,
                        });
                    }
                    continue;
                }
                if tab.floating_focus == Some(floating.pane_id) {
                    anyhow::ensure!(
                        floating.visible,
                        "foreign hidden floating pane {} is focused",
                        floating.pane_id
                    );
                    foreign_focus = Some(floating.pane_id);
                }
                replacement.push(floating.clone());
            }
            if let Some(focused) = tab.floating_focus {
                anyhow::ensure!(
                    tab.floating_panes
                        .iter()
                        .any(|floating| floating.pane_id == focused),
                    "tab {} floating focus names absent pane {focused}",
                    tab.tab.tab_id
                );
            }
            let desired_focus = states
                .iter()
                .find(|state| state.focused)
                .map(|state| state.pane_id);
            anyhow::ensure!(
                desired_focus.is_none() || tab.zoomed_pane.is_none(),
                "tab {} cannot focus a floating pane while a tiled pane is zoomed",
                tab.tab.tab_id
            );
            anyhow::ensure!(
                foreign_focus.is_none() || tab.zoomed_pane.is_none(),
                "tab {} has foreign floating focus while a tiled pane is zoomed",
                tab.tab.tab_id
            );
            anyhow::ensure!(
                foreign_focus.is_none() || desired_focus.is_none(),
                "tab {} cannot preserve foreign floating focus while applying domain focus",
                tab.tab.tab_id
            );
            for state in desired_states {
                replacement.push(FloatingPane {
                    pane: Arc::clone(&state.pane),
                    pane_id: state.pane_id,
                    rect: state.rect,
                    z_order: state.z_order,
                    visible: state.visible,
                    pinned: state.pinned,
                    opacity: state.opacity,
                });
            }
            let floating_focus = desired_focus.or(foreign_focus);
            let changed = tab.floating_focus != floating_focus
                || !floating_pane_vectors_eq(&tab.floating_panes, &replacement);
            if changed {
                changed_tab_ids.push(tab.tab.tab_id);
                if let Some(window_id) = tab.parent_window_id {
                    invalidated_window_ids.insert(window_id);
                }
            }
            prepared_tabs.push(PreparedDomainFloatingTab {
                replacement: changed.then_some(replacement),
                floating_focus,
                changed,
            });
        }
        changed_tab_ids.sort_unstable();
        let mut invalidated_window_ids = invalidated_window_ids.into_iter().collect::<Vec<_>>();
        invalidated_window_ids.sort_unstable();

        let mut pane_count_transitions = Vec::new();
        pane_count_transitions
            .try_reserve_exact(observed.len())
            .map_err(|error| anyhow::anyhow!("reserve reconciled tab pane counts: {error}"))?;
        for (observed_tab, prepared_tab) in observed.iter().zip(&prepared_tabs) {
            let prior_count = observed_tab
                .non_floating_panes
                .len()
                .checked_add(observed_tab.floating_panes.len())
                .ok_or_else(|| anyhow::anyhow!("reconciled prior pane count overflow"))?;
            let final_floating_count = prepared_tab
                .replacement
                .as_ref()
                .map_or(observed_tab.floating_panes.len(), Vec::len);
            let next_count = observed_tab
                .non_floating_panes
                .len()
                .checked_add(final_floating_count)
                .ok_or_else(|| anyhow::anyhow!("reconciled final pane count overflow"))?;
            pane_count_transitions.push((
                Arc::clone(&observed_tab.tab),
                prior_count,
                next_count,
            ));
        }

        let mut stale_registrations = live_registrations
            .iter()
            .filter(|(_, pane, _, registered_domain_id)| {
                *registered_domain_id == domain_id
                    && !authoritative_by_identity.contains_key(&pane_identity(pane))
            })
            .map(|(pane_id, pane, registration, _)| {
                (*pane_id, Arc::clone(pane), registration.clone())
            })
            .collect::<Vec<_>>();
        stale_registrations.sort_unstable_by_key(|(pane_id, _, _)| *pane_id);
        let registered_pane_ids = prepared_new
            .iter()
            .map(|prepared| prepared.pane_id)
            .collect::<Vec<_>>();
        let retired_pane_ids = stale_registrations
            .iter()
            .map(|(pane_id, _, _)| *pane_id)
            .collect::<Vec<_>>();
        let mut retired_pane_id_set = HashSet::new();
        retired_pane_id_set
            .try_reserve(retired_pane_ids.len())
            .map_err(|error| anyhow::anyhow!("reserve retired floating pane state set: {error}"))?;
        retired_pane_id_set.extend(retired_pane_ids.iter().copied());

        let mut prepared_registration_by_id = HashMap::new();
        prepared_registration_by_id
            .try_reserve(prepared_new.len())
            .map_err(|error| {
                anyhow::anyhow!("reserve prepared floating registration index: {error}")
            })?;
        for prepared in &prepared_new {
            let registration = PaneRegistrationHandle::new(
                &prepared.pane,
                &prepared.preparation_claim.generation,
            );
            anyhow::ensure!(
                prepared_registration_by_id
                    .insert(
                        prepared.pane_id,
                        (pane_identity(&prepared.pane), registration),
                    )
                    .is_none(),
                "new floating pane id {} was prepared more than once",
                prepared.pane_id
            );
        }

        let resolve_final_registration =
            |pane_id: PaneId,
             pane: &Arc<dyn Pane>|
             -> anyhow::Result<(PaneRegistrationHandle, DomainId)> {
                if let Some(&index) = live_by_id.get(&pane_id) {
                    let (_, registered_pane, registration, registered_domain_id) =
                        &live_registrations[index];
                    anyhow::ensure!(
                        Arc::ptr_eq(registered_pane, pane),
                        "pane id {pane_id} resolves to another exact live allocation"
                    );
                    return Ok((registration.clone(), *registered_domain_id));
                }
                let (identity, registration) = prepared_registration_by_id
                    .get(&pane_id)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "pane {pane_id} has neither live nor prepared registration authority"
                        )
                    })?;
                anyhow::ensure!(
                    *identity == pane_identity(pane) && registration.is_same_pane(pane),
                    "prepared pane id {pane_id} names another exact allocation"
                );
                Ok((registration.clone(), domain_id))
            };

        let mut final_domain_authority = Vec::new();
        final_domain_authority
            .try_reserve_exact(authoritative_by_identity.len())
            .map_err(|error| {
                anyhow::anyhow!("reserve final domain floating authority: {error}")
            })?;
        for (pane_id, pane) in authoritative_by_identity.values() {
            let (registration, registered_domain_id) =
                resolve_final_registration(*pane_id, pane)?;
            anyhow::ensure!(
                registered_domain_id == domain_id,
                "authoritative pane {pane_id} resolves to domain {registered_domain_id}, not {domain_id}"
            );
            final_domain_authority.push(ExactPaneAuthorityState {
                pane_id: *pane_id,
                pane: Arc::clone(pane),
                registration,
            });
        }
        final_domain_authority.sort_unstable_by_key(|state| state.pane_id);

        anyhow::ensure!(
            observed.len() == prepared_tabs.len(),
            "floating tab preparation cardinality changed before authority planning"
        );
        let mut authority_tab_replacements = Vec::new();
        authority_tab_replacements
            .try_reserve_exact(observed.len())
            .map_err(|error| {
                anyhow::anyhow!("reserve domain floating authority replacements: {error}")
            })?;
        for (observed_tab, prepared_tab) in observed.iter().zip(&prepared_tabs) {
            let current_count = observed_tab
                .non_floating_panes
                .len()
                .checked_add(observed_tab.floating_panes.len())
                .ok_or_else(|| anyhow::anyhow!("current structural pane count overflow"))?;
            let final_floating = prepared_tab
                .replacement
                .as_deref()
                .unwrap_or(&observed_tab.floating_panes);
            let desired_count = observed_tab
                .non_floating_panes
                .len()
                .checked_add(final_floating.len())
                .ok_or_else(|| anyhow::anyhow!("desired structural pane count overflow"))?;
            let mut current = Vec::new();
            current.try_reserve_exact(current_count).map_err(|error| {
                anyhow::anyhow!(
                    "reserve current structural authority for tab {}: {error}",
                    observed_tab.tab.tab_id
                )
            })?;
            let mut desired = Vec::new();
            desired.try_reserve_exact(desired_count).map_err(|error| {
                anyhow::anyhow!(
                    "reserve desired structural authority for tab {}: {error}",
                    observed_tab.tab.tab_id
                )
            })?;

            for pane in &observed_tab.non_floating_panes {
                let identity = pane_identity(pane);
                let pane_id = structural_ids.get(&identity).copied().ok_or_else(|| {
                    anyhow::anyhow!(
                        "tiled pane in tab {} is absent from its structural census",
                        observed_tab.tab.tab_id
                    )
                })?;
                let (registration, registered_domain_id) =
                    resolve_final_registration(pane_id, pane)?;
                current.push(ExactPaneStructuralState {
                    pane_id,
                    pane: Arc::clone(pane),
                    lane: PaneStructuralLane::Tiled,
                });
                desired.push(DesiredPaneStructuralState {
                    pane_id,
                    pane: Arc::clone(pane),
                    lane: PaneStructuralLane::Tiled,
                    registration,
                    domain_id: registered_domain_id,
                });
            }
            for floating in &observed_tab.floating_panes {
                let (registration, _) =
                    resolve_final_registration(floating.pane_id, &floating.pane)?;
                debug_assert!(registration.is_same_pane(&floating.pane));
                current.push(ExactPaneStructuralState {
                    pane_id: floating.pane_id,
                    pane: Arc::clone(&floating.pane),
                    lane: PaneStructuralLane::Floating,
                });
            }
            for floating in final_floating {
                let (registration, registered_domain_id) =
                    resolve_final_registration(floating.pane_id, &floating.pane)?;
                desired.push(DesiredPaneStructuralState {
                    pane_id: floating.pane_id,
                    pane: Arc::clone(&floating.pane),
                    lane: PaneStructuralLane::Floating,
                    registration,
                    domain_id: registered_domain_id,
                });
            }
            authority_tab_replacements.push(DomainAuthorityTabReplacement {
                tab: Arc::clone(&observed_tab.tab),
                current,
                desired,
            });
        }

        let mut structural_notifications = Vec::new();
        structural_notifications
            .try_reserve_exact(
                invalidated_window_ids
                    .len()
                    .checked_add(changed_tab_ids.len())
                    .ok_or_else(|| {
                        anyhow::anyhow!("floating topology notification count overflow")
                    })?,
            )
            .map_err(|error| anyhow::anyhow!("reserve floating topology notifications: {error}"))?;
        structural_notifications.extend(
            invalidated_window_ids
                .iter()
                .copied()
                .map(MuxNotification::WindowInvalidated),
        );
        structural_notifications.extend(
            changed_tab_ids
                .iter()
                .copied()
                .map(MuxNotification::TabResized),
        );

        let mut lifecycle_ids = Vec::new();
        lifecycle_ids
            .try_reserve_exact(prepared_new.len().saturating_add(stale_registrations.len()))
            .map_err(|error| anyhow::anyhow!("reserve floating lifecycle ids: {error}"))?;
        lifecycle_ids.extend(prepared_new.iter().map(|prepared| prepared.pane_id));
        lifecycle_ids.extend(stale_registrations.iter().map(|(pane_id, _, _)| *pane_id));

        let mut structural_envelopes = Vec::new();
        structural_envelopes
            .try_reserve_exact(structural_notifications.len())
            .map_err(|error| anyhow::anyhow!("reserve floating topology envelopes: {error}"))?;
        let mut published_new = Vec::new();
        published_new
            .try_reserve_exact(prepared_new.len())
            .map_err(|error| anyhow::anyhow!("reserve published floating panes: {error}"))?;
        let mut removed_components = Vec::new();
        removed_components
            .try_reserve_exact(stale_registrations.len())
            .map_err(|error| anyhow::anyhow!("reserve retired floating panes: {error}"))?;
        let mut retired_live_registrations = Vec::new();
        retired_live_registrations
            .try_reserve_exact(stale_registrations.len())
            .map_err(|error| {
                anyhow::anyhow!("reserve retired live floating registrations: {error}")
            })?;
        let mut removed_registrations = Vec::new();
        removed_registrations
            .try_reserve_exact(stale_registrations.len())
            .map_err(|error| anyhow::anyhow!("reserve removed floating registrations: {error}"))?;
        let mut new_lifecycle_tickets = Vec::new();
        new_lifecycle_tickets
            .try_reserve_exact(prepared_new.len())
            .map_err(|error| anyhow::anyhow!("reserve new floating lifecycle tickets: {error}"))?;
        let mut output_batches = Vec::new();
        output_batches
            .try_reserve_exact(stale_registrations.len())
            .map_err(|error| anyhow::anyhow!("reserve retired floating output batches: {error}"))?;
        let mut lifecycle_notifications = Vec::new();
        lifecycle_notifications
            .try_reserve_exact(lifecycle_ids.len())
            .map_err(|error| anyhow::anyhow!("reserve floating lifecycle payloads: {error}"))?;
        let mut tab_guards = Vec::new();
        tab_guards
            .try_reserve_exact(observed.len())
            .map_err(|error| anyhow::anyhow!("reserve floating tab lock guards: {error}"))?;
        let mut registration_commit_guards = Vec::new();
        registration_commit_guards
            .try_reserve_exact(prepared_new.len())
            .map_err(|error| {
                anyhow::anyhow!("reserve floating registration commit guards: {error}")
            })?;
        let (
            published_new,
            new_lifecycle_tickets,
            removed_registrations,
            retired_live_registrations,
            output_batches,
            structural_envelopes,
        ) = {
            let _domain_registration = self.domain_registration.lock();
            anyhow::ensure!(
                !self.retired_domain_ids.lock().contains(&domain_id)
                    && self
                        .domains
                        .read()
                        .get(&domain_id)
                        .is_some_and(|domain| Arc::ptr_eq(domain, &expected_domain)),
                "domain {domain_id} retired or changed identity before floating reconciliation"
            );

            let _registration = self.pane_registration.lock();
            let mut authority = self.pane_authority.lock();
            let registered_tabs = self.tabs.read();
            anyhow::ensure!(
                registered_tabs.len() == registered_tabs_snapshot.len()
                    && registered_tabs_snapshot.iter().all(|(tab_id, expected)| {
                        registered_tabs
                            .get(tab_id)
                            .is_some_and(|current| Arc::ptr_eq(current, expected))
                    }),
                "registered tab set changed during floating reconciliation"
            );
            let mut windows = self.windows.write();
            anyhow::ensure!(
                windows.len() == windows_snapshot.len()
                    && windows_snapshot.iter().all(
                        |(window_id, expected_workspace, expected_tabs)| {
                        windows.get(window_id).is_some_and(|window| {
                            window.get_workspace() == expected_workspace
                                && window.len() == expected_tabs.len()
                                && window
                                    .iter()
                                    .zip(expected_tabs)
                                    .all(|(current, expected)| Arc::ptr_eq(current, expected))
                        })
                    }),
                "window tab topology changed during floating reconciliation"
            );
            let tab_parents = self.tab_parents.read();
            let mut workspace_counts = self.num_panes_by_workspace.write();

            let prepared_authority = authority
                .prepare_domain_floating_authority_reconcile(
                    domain_id,
                    &expected_domain,
                    final_domain_authority,
                    authority_tab_replacements,
                )?;
            let prepared_counts = self.prepare_tab_pane_count_mutation_locked(
                &windows,
                &tab_parents,
                &mut workspace_counts,
                &pane_count_transitions,
                "domain floating reconciliation",
            )?;

            tab_guards.extend(observed.iter().map(|tab| tab.tab.inner.lock()));
            for (guard, expected) in tab_guards.iter().zip(&observed) {
                anyhow::ensure!(
                    guard.size == expected.size
                        && guard.floating_focus == expected.floating_focus
                        && exact_optional_tiled_tree_eq(&guard.pane, &expected.tiled_tree)
                        && exact_pane_stack_maps_eq(&guard.pane_stacks, &expected.pane_stacks)
                        && floating_pane_vectors_eq(
                            &guard.floating_panes,
                            &expected.floating_panes
                        )
                        && match (&guard.zoomed, &expected.zoomed_pane) {
                            (Some(current), Some(expected)) => Arc::ptr_eq(current, expected),
                            (None, None) => true,
                            _ => false,
                        },
                    "tab {} changed during floating reconciliation",
                    expected.tab.tab_id
                );
            }
            for prepared in &prepared_new {
                anyhow::ensure!(
                    prepared.preparation_claim.is_authoritative_locked(),
                    "new floating pane {} preparation was cancelled",
                    prepared.pane_id
                );
            }

            let mut panes = self.panes.write();
            anyhow::ensure!(
                panes
                    .values()
                    .filter(|registered| registered.domain_id == domain_id)
                    .count()
                    == live_registrations
                        .iter()
                        .filter(|(_, _, _, registered_domain_id)| {
                            *registered_domain_id == domain_id
                        })
                        .count(),
                "domain pane registration set changed during floating reconciliation"
            );
            for (pane_id, pane, registration, registered_domain_id) in &live_registrations {
                if *registered_domain_id == domain_id
                    || structural_ids.contains_key(&pane_identity(pane))
                {
                    anyhow::ensure!(
                        panes.get(pane_id).is_some_and(|current| {
                            current.domain_id == *registered_domain_id
                                && Arc::ptr_eq(&current.pane, pane)
                                && registration.matches_live_registration(current)
                        }),
                        "pane {pane_id} registration changed during floating reconciliation"
                    );
                }
            }
            for prepared in &prepared_new {
                anyhow::ensure!(
                    !panes.contains_key(&prepared.pane_id)
                        && !self.retiring_pane_ids.lock().contains(&prepared.pane_id)
                        && !self
                            .pane_retirements
                            .has_in_flight_retirement(prepared.pane_id),
                    "new floating pane id {} became unavailable before commit",
                    prepared.pane_id
                );
            }
            panes.try_reserve(prepared_new.len()).map_err(|error| {
                anyhow::anyhow!("reserve domain floating pane registry: {error}")
            })?;

            let mut retiring = self.retiring_pane_ids.lock();
            anyhow::ensure!(
                stale_registrations
                    .iter()
                    .all(|(pane_id, _, _)| !retiring.contains(pane_id)),
                "a stale floating pane began retiring before reconciliation"
            );
            retiring
                .try_reserve(stale_registrations.len())
                .map_err(|error| anyhow::anyhow!("reserve retired floating pane ids: {error}"))?;

            // Pane-output producers already hold this queue while consulting
            // topology and lifecycle state. Retain it before either of those
            // locks so reconciliation cannot form an output/lifecycle or
            // output/topology AB/BA cycle under concurrent terminal output.
            let mut pending_output = self.pending_pane_output.lock();
            for (pane_id, _, _) in &stale_registrations {
                if let Some(batch) = pending_output.queued.get(pane_id) {
                    let registered = panes
                        .get(pane_id)
                        .expect("stale pane registration was revalidated");
                    anyhow::ensure!(
                        Arc::ptr_eq(&batch.generation, &registered.generation),
                        "stale pane {pane_id} has queued output from another registration generation"
                    );
                }
            }
            let lifecycle_enqueue = (!lifecycle_ids.is_empty())
                .then(|| self.prepare_pane_lifecycle_batch_enqueue(&lifecycle_ids))
                .transpose()?;
            let total_revisions = structural_notifications
                .len()
                .checked_add(lifecycle_ids.len())
                .ok_or_else(|| {
                    anyhow::anyhow!("floating reconciliation revision count overflow")
                })?;
            for prepared in &mut prepared_new {
                let reservation = prepared.registration_reservation.take().ok_or_else(|| {
                    anyhow::anyhow!(
                        "new floating pane {} lost its registration reservation before commit",
                        prepared.pane_id
                    )
                })?;
                registration_commit_guards.push(reservation.commit()?);
            }
            let mut topology = self.topology.lock();
            let first_revision = topology
                .reserve_revisions(total_revisions)
                .map_err(anyhow::Error::new)?;
            let mut revision_offset = 0u64;
            for notification in structural_notifications {
                structural_envelopes.push(MuxNotificationEnvelope {
                    notification,
                    topology: crate::MuxTopologyStamp::Revision(crate::TopologyRevision::new(
                        first_revision
                            .get()
                            .checked_add(revision_offset)
                            .expect("reserved floating topology range cannot overflow"),
                    )),
                });
                revision_offset += 1;
            }

            for (pane_id, pane, registration) in &stale_registrations {
                let registered = panes
                    .remove(pane_id)
                    .expect("stale floating pane registration was revalidated");
                debug_assert!(Arc::ptr_eq(&registered.pane, pane));
                debug_assert!(registration.matches_live_registration(&registered));
                let removed_pane = Arc::clone(&registered.pane);
                let generation = Arc::clone(&registered.generation);
                let output_is_current = pending_output
                    .queued
                    .get(pane_id)
                    .is_some_and(|batch| Arc::ptr_eq(&batch.generation, &generation));
                if output_is_current {
                    if let Some(batch) = pending_output.queued.remove(pane_id) {
                        output_batches.push(batch);
                    }
                }
                let inserted = retiring.insert(*pane_id);
                debug_assert!(inserted);
                removed_components.push((*pane_id, removed_pane, generation));
                retired_live_registrations.push(registered);
            }

            for (prepared, commit_guard) in prepared_new
                .iter_mut()
                .zip(registration_commit_guards.into_iter())
            {
                let registration = PaneRegistrationHandle::new(
                    &prepared.pane,
                    &prepared.preparation_claim.generation,
                );
                let prior = panes.insert(
                    prepared.pane_id,
                    crate::LivePaneRegistration {
                        pane: Arc::clone(&prepared.pane),
                        generation: Arc::clone(&prepared.preparation_claim.generation),
                        domain_id,
                    },
                );
                debug_assert!(prior.is_none());
                let finalized = commit_guard.finalize();
                debug_assert!(registration.same_registration(&finalized));
                let retired = prepared.preparation_claim.retire_locked();
                debug_assert!(retired);
                published_new.push((
                    Arc::clone(&prepared.pane),
                    registration,
                    prepared.reader_start_gate.take(),
                ));
            }

            for ((guard, prepared), expected) in
                tab_guards.iter_mut().zip(&mut prepared_tabs).zip(&observed)
            {
                if !prepared.changed {
                    continue;
                }
                guard.floating_panes = prepared
                    .replacement
                    .take()
                    .expect("changed floating tab retains its prepared replacement");
                guard.floating_focus = prepared.floating_focus;
                debug_assert_eq!(guard.size, expected.size);
            }
            authority.commit_domain_floating_authority_reconcile(prepared_authority);
            prepared_counts.commit(&mut windows, &mut workspace_counts);

            for (pane, registration, reader_start_gate) in &mut published_new {
                debug_assert!(registration.is_same_pane(pane));
                lifecycle_notifications.push(crate::PreparedPaneLifecycleBatchNotification {
                    notification: crate::PaneLifecycleNotification::Added(registration.pane_id()),
                    topology: crate::MuxTopologyStamp::Revision(crate::TopologyRevision::new(
                        first_revision
                            .get()
                            .checked_add(revision_offset)
                            .expect("reserved floating topology range cannot overflow"),
                    )),
                    reader_start_gate: reader_start_gate.take(),
                    cleanup_complete: None,
                    removal_follow_up: crate::PaneRemovalFollowUp::None,
                });
                revision_offset += 1;
            }
            for (pane_id, _, generation) in &removed_components {
                lifecycle_notifications.push(crate::PreparedPaneLifecycleBatchNotification {
                    notification: crate::PaneLifecycleNotification::Removed(*pane_id),
                    topology: crate::MuxTopologyStamp::Revision(crate::TopologyRevision::new(
                        first_revision
                            .get()
                            .checked_add(revision_offset)
                            .expect("reserved floating topology range cannot overflow"),
                    )),
                    reader_start_gate: None,
                    cleanup_complete: Some(Arc::clone(&generation.cleanup_complete)),
                    removal_follow_up: crate::PaneRemovalFollowUp::None,
                });
                revision_offset += 1;
            }
            debug_assert_eq!(
                revision_offset,
                u64::try_from(total_revisions).unwrap_or(u64::MAX)
            );
            let lifecycle_tickets = lifecycle_enqueue
                .map(|enqueue| enqueue.enqueue(lifecycle_notifications))
                .unwrap_or_default();
            let mut tickets = lifecycle_tickets.into_iter();
            for _ in &published_new {
                new_lifecycle_tickets.push(
                    tickets
                        .next()
                        .expect("each new floating registration retains one lifecycle ticket"),
                );
            }
            for ((pane_id, pane, generation), ticket) in removed_components.into_iter().zip(tickets)
            {
                removed_registrations.push(crate::RemovedPaneRegistration {
                    pane_id,
                    pane,
                    generation,
                    lifecycle_notification: ticket,
                });
            }
            debug_assert_eq!(removed_registrations.len(), stale_registrations.len());

            drop(topology);
            drop(pending_output);
            drop(retiring);
            drop(panes);
            drop(tab_guards);
            drop(workspace_counts);
            drop(windows);
            drop(registered_tabs);
            drop(authority);
            drop(_registration);
            drop(_domain_registration);

            (
                published_new,
                new_lifecycle_tickets,
                removed_registrations,
                retired_live_registrations,
                output_batches,
                structural_envelopes,
            )
        };

        drop(retired_live_registrations);
        for batch in output_batches {
            metrics::histogram!("mux.notifications.pane_output.removal_forced_seal_rate")
                .record(1.0);
            batch.seal();
        }
        self.discard_removed_pane_states_set(&retired_pane_id_set);

        let new_count = published_new.len();
        for envelope in structural_envelopes {
            self.dispatch_notification_envelope(envelope);
        }

        debug_assert_eq!(new_lifecycle_tickets.len(), new_count);
        for added in new_lifecycle_tickets {
            self.complete_pane_lifecycle_notification(added);
        }
        for (pane, registration, _) in &published_new {
            self.notify_pane_registration_did_bind(pane, registration);
        }
        for removed in removed_registrations {
            self.finish_pane_removal(removed, false);
        }

        Ok(DomainFloatingPaneReconcileReceipt {
            changed_tab_ids,
            invalidated_window_ids,
            registered_pane_ids,
            retired_pane_ids,
        })
    }
}

impl TabInner {
    fn new(size: &TerminalSize) -> Self {
        Self {
            id: crate::next_unique_usize_id(&TAB_ID, "mux tab"),
            mux_owner: Weak::new(),
            mux_owner_bound: false,
            mux_owner_active: false,
            mux_owner_generation: 0,
            pane: Some(Tree::new()),
            floating_panes: vec![],
            floating_focus: None,
            size: *size,
            size_before_zoom: *size,
            active: 0,
            zoomed: None,
            title: Arc::from(""),
            recency: Recency::default(),
            collapsed_panes: HashSet::new(),
            layout_cycle: Some(crate::layout::default_cycle()),
            pane_stacks: HashMap::new(),
            constraint_overrides: HashMap::new(),
        }
    }

    fn prepare_mux_owner_binding(&self, mux: &Arc<Mux>) -> anyhow::Result<u64> {
        if self.mux_owner_bound {
            let Some(owner) = self.mux_owner.upgrade() else {
                anyhow::bail!(
                    "tab {} cannot be rebound after its exact mux owner was dropped",
                    self.id
                );
            };
            anyhow::ensure!(
                Arc::ptr_eq(&owner, mux),
                "tab {} is already bound to a different mux authority",
                self.id
            );
            anyhow::ensure!(
                !self.mux_owner_active,
                "tab {} already has an active mux-owner generation",
                self.id
            );
        }

        self.mux_owner_generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("tab {} mux-owner generation exhausted", self.id))
    }

    fn commit_mux_owner_binding(&mut self, mux: &Arc<Mux>, next_generation: u64) {
        if !self.mux_owner_bound {
            self.mux_owner = Arc::downgrade(mux);
            self.mux_owner_bound = true;
        }

        self.mux_owner_generation = next_generation;
        self.mux_owner_active = true;
    }

    fn retire_mux_owner(&mut self, mux: &Mux) -> bool {
        if !self.is_active_mux_owner(mux) {
            return false;
        }
        self.mux_owner_active = false;
        true
    }

    fn is_active_mux_owner(&self, mux: &Mux) -> bool {
        self.mux_owner_active
            && self
                .mux_owner
                .upgrade()
                .is_some_and(|owner| std::ptr::eq(owner.as_ref(), mux))
    }

    fn notification_owner(&self) -> Option<Arc<Mux>> {
        if self.mux_owner_active {
            self.mux_owner.upgrade()
        } else {
            None
        }
    }

    fn commit_prepared_pane_tree_install(
        &mut self,
        prepared: PreparedPaneTreeInstall,
    ) -> DeferredTabCallbacks {
        let PreparedPaneTreeInstall {
            tree,
            active_index,
            tag_active,
            zoomed,
            size,
            resize_work,
        } = prepared;
        self.active = active_index;
        if tag_active {
            self.recency.tag(active_index);
        }
        self.pane.replace(tree);
        self.floating_panes.clear();
        self.floating_focus = None;
        self.pane_stacks.clear();
        self.zoomed = zoomed;
        self.size = size;

        // The replacement tree was prepared from one validated remote
        // snapshot and already carries its authoritative split geometry.
        // Recomputing constraints here would both distort that snapshot and
        // invoke arbitrary Pane observations while `Tab::inner` is locked.
        // Freeze only the callbacks implied by the prepared geometry; execute
        // them after the outer synchronization boundary releases this lock.
        let callbacks = DeferredTabCallbacks {
            changed: true,
            resize_work,
            ..DeferredTabCallbacks::default()
        };

        log::debug!("sync tab: {:#?} zoomed: {}", size, self.zoomed.is_some(),);
        callbacks
    }

    /// Returns a count of how many panes are in this tab
    fn count_panes(&mut self) -> usize {
        let floating_count = self.count_floating_panes();
        let hidden_stack_count = self
            .pane_stacks
            .values()
            .map(|stack| stack.len().saturating_sub(1))
            .fold(0usize, usize::saturating_add);
        let mut count: usize = 0;
        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                count = count.saturating_add(1);
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    return count
                        .saturating_add(hidden_stack_count)
                        .saturating_add(floating_count);
                }
            }
        }
    }

    /// Sets the zoom state, returns the prior state
    fn prepare_set_zoomed(&mut self, zoomed: bool) -> (bool, DeferredTabCallbacks) {
        let prior = self.zoomed.is_some();
        if self.zoomed.is_some() == zoomed {
            // Current zoom state matches intended zoom state,
            // so we have nothing to do.
            return (prior, DeferredTabCallbacks::default());
        }
        (prior, self.prepare_toggle_zoom())
    }

    fn prepare_toggle_zoom(&mut self) -> DeferredTabCallbacks {
        let mut callbacks = DeferredTabCallbacks::default();
        let size = self.size;
        if let Some(pane) = self.zoomed.take() {
            // We were zoomed, but now we are not.
            // Re-apply the size to the panes
            callbacks.zoom_work.push((pane, false));
            callbacks.changed = true;
            self.size = self.size_before_zoom;
            let mut resize_callbacks = self.prepare_resize_for_reflow(size);
            callbacks
                .resize_work
                .append(&mut resize_callbacks.resize_work);
            callbacks.changed |= resize_callbacks.changed;
        } else {
            // We weren't zoomed, but now we want to zoom.
            // Locate the active pane
            self.size_before_zoom = size;
            if let Some(pane) = self.raw_active_pane_retained_id() {
                callbacks.zoom_work.push((Arc::clone(&pane), true));
                callbacks.resize_work.push((Arc::clone(&pane), size));
                self.zoomed.replace(pane);
                callbacks.changed = true;
            }
        }
        callbacks
    }

    fn contains_pane(&self, pane: PaneId) -> bool {
        fn contains(tree: &Tree, pane: PaneId) -> bool {
            match tree {
                Tree::Empty => false,
                Tree::Node { left, right, .. } => contains(left, pane) || contains(right, pane),
                Tree::Leaf(p) => p.pane_id() == pane,
            }
        }
        let in_tree = match &self.pane {
            Some(root) => contains(root, pane),
            None => false,
        };
        in_tree
            || self.pane_stacks.values().any(|stack| {
                stack
                    .panes()
                    .iter()
                    .any(|stacked| stacked.pane_id() == pane)
            })
            || self
                .floating_panes
                .iter()
                .any(|floating| floating.pane_id == pane)
    }

    fn has_panes_in_domain(&self, domain_id: DomainId) -> bool {
        fn tree_has_domain(tree: &Tree, domain_id: DomainId) -> bool {
            match tree {
                Tree::Empty => false,
                Tree::Node { left, right, .. } => {
                    tree_has_domain(left, domain_id) || tree_has_domain(right, domain_id)
                }
                Tree::Leaf(pane) => pane.domain_id() == domain_id,
            }
        }

        self.pane
            .as_ref()
            .is_some_and(|tree| tree_has_domain(tree, domain_id))
            || self.pane_stacks.values().any(|stack| {
                stack
                    .panes()
                    .iter()
                    .any(|pane| pane.domain_id() == domain_id)
            })
            || self
                .floating_panes
                .iter()
                .any(|floating| floating.pane.domain_id() == domain_id)
    }

    fn domain_id_for_pane(&self, pane_id: PaneId) -> Option<DomainId> {
        fn find_in_tree(tree: &Tree, pane_id: PaneId) -> Option<DomainId> {
            match tree {
                Tree::Empty => None,
                Tree::Node { left, right, .. } => {
                    find_in_tree(left, pane_id).or_else(|| find_in_tree(right, pane_id))
                }
                Tree::Leaf(pane) => (pane.pane_id() == pane_id).then(|| pane.domain_id()),
            }
        }

        self.floating_panes
            .iter()
            .find(|floating| floating.pane_id == pane_id)
            .map(|floating| floating.pane.domain_id())
            .or_else(|| {
                self.pane_stacks
                    .values()
                    .flat_map(|stack| stack.panes())
                    .find(|pane| pane.pane_id() == pane_id)
                    .map(|pane| pane.domain_id())
            })
            .or_else(|| {
                self.pane
                    .as_ref()
                    .and_then(|tree| find_in_tree(tree, pane_id))
            })
    }

    fn clamp_floating_rect(&self, rect: FloatingPaneRect) -> FloatingPaneRect {
        let max_width = self.size.cols.max(1);
        let max_height = self.size.rows.max(1);
        let min_width = min_floating_pane_width().min(max_width);
        let min_height = min_floating_pane_height().min(max_height);

        let width = rect.width.max(min_width).min(max_width);
        let height = rect.height.max(min_height).min(max_height);
        let left = rect.left.min(max_width.saturating_sub(width));
        let top = rect.top.min(max_height.saturating_sub(height));

        FloatingPaneRect {
            left,
            top,
            width,
            height,
        }
    }

    fn floating_pane_size(&self, rect: FloatingPaneRect) -> TerminalSize {
        let dims = self.cell_dimensions();
        TerminalSize {
            rows: rect.height,
            cols: rect.width,
            pixel_width: dims.pixel_width.saturating_mul(rect.width),
            pixel_height: dims.pixel_height.saturating_mul(rect.height),
            dpi: dims.dpi,
        }
    }

    fn floating_index_by_id(&self, pane_id: PaneId) -> Option<usize> {
        self.floating_panes
            .iter()
            .position(|floating| floating.pane_id == pane_id)
    }

    fn next_floating_z_order(&mut self) -> u32 {
        let max = self
            .floating_panes
            .iter()
            .map(|floating| floating.z_order)
            .max()
            .unwrap_or(0);
        if max != u32::MAX {
            return max + 1;
        }

        // Long-lived sessions can raise panes often enough to exhaust the
        // semantic lane counter even with only a handful of live panes.
        // Preserve the current total order while compacting it back to a
        // dense range, then allocate the next unique top lane.
        // Keep the physical vector order unchanged: callers use it as a stable
        // identity/iteration order independent of semantic z-order. Equal
        // lanes retain their prior vector order through the explicit index
        // tie-breaker.
        let mut order: Vec<usize> = (0..self.floating_panes.len()).collect();
        order.sort_by_key(|index| (self.floating_panes[*index].z_order, *index));
        for (lane, index) in order.into_iter().enumerate() {
            self.floating_panes[index].z_order = u32::try_from(lane).unwrap_or(u32::MAX);
        }
        u32::try_from(self.floating_panes.len()).unwrap_or(u32::MAX)
    }

    fn positioned_floating_pane(&self, floating: &FloatingPane) -> PositionedFloatingPane {
        PositionedFloatingPane {
            pane_id: floating.pane_id,
            is_focused: self.floating_focus == Some(floating.pane_id),
            left: floating.rect.left,
            top: floating.rect.top,
            width: floating.rect.width,
            height: floating.rect.height,
            z_order: floating.z_order,
            visible: floating.visible,
            pinned: floating.pinned,
            opacity: floating.opacity,
            pane: Arc::clone(&floating.pane),
        }
    }

    fn prepare_add_floating_pane(
        &self,
        pane: Arc<dyn Pane>,
        pane_id: PaneId,
        rect: FloatingPaneRect,
    ) -> anyhow::Result<PreparedFloatingPaneAddition> {
        self.prepare_add_floating_pane_with_focus(pane, pane_id, rect, true)
    }

    fn prepare_add_floating_pane_with_focus(
        &self,
        pane: Arc<dyn Pane>,
        pane_id: PaneId,
        rect: FloatingPaneRect,
        focus: bool,
    ) -> anyhow::Result<PreparedFloatingPaneAddition> {
        self.prepare_add_floating_pane_impl(pane, pane_id, rect, focus, true)
    }

    fn prepare_add_presized_floating_pane(
        &self,
        pane: Arc<dyn Pane>,
        pane_id: PaneId,
        rect: FloatingPaneRect,
        focus: bool,
    ) -> anyhow::Result<PreparedFloatingPaneAddition> {
        self.prepare_add_floating_pane_impl(pane, pane_id, rect, focus, false)
    }

    fn prepare_add_floating_pane_impl(
        &self,
        pane: Arc<dyn Pane>,
        pane_id: PaneId,
        rect: FloatingPaneRect,
        focus: bool,
        resize: bool,
    ) -> anyhow::Result<PreparedFloatingPaneAddition> {
        let prior = self.raw_active_pane_retained_id();
        let rect = self.clamp_floating_rect(rect);
        let pane_size = resize.then(|| self.floating_pane_size(rect));
        let replacement_len = self
            .floating_panes
            .len()
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("floating pane count overflow"))?;
        let mut replacement = Vec::new();
        replacement
            .try_reserve_exact(replacement_len)
            .map_err(|error| anyhow::anyhow!("reserve floating pane replacement: {error}"))?;
        replacement.extend(self.floating_panes.iter().cloned());
        let z_order = next_floating_z_order_in_replacement(&mut replacement)?;

        let floating = FloatingPane {
            pane: Arc::clone(&pane),
            pane_id,
            rect,
            z_order,
            visible: true,
            pinned: false,
            opacity: 1.0,
        };
        replacement.push(floating);
        let floating_focus = if focus {
            Some(pane_id)
        } else {
            self.floating_focus
        };
        let positioned = positioned_floating_pane_with_focus(
            replacement.last().ok_or_else(|| {
                anyhow::anyhow!("prepared floating replacement lost its admitted pane")
            })?,
            floating_focus,
        );
        let mut callbacks = DeferredTabCallbacks {
            changed: true,
            prior_focus: prior.clone(),
            current_focus: if focus {
                Some(Arc::clone(&pane))
            } else {
                prior
            },
            current_focus_id: focus.then_some(pane_id),
            ..DeferredTabCallbacks::default()
        };
        if let Some(pane_size) = pane_size {
            callbacks
                .resize_work
                .try_reserve_exact(1)
                .map_err(|error| anyhow::anyhow!("reserve floating pane resize work: {error}"))?;
            callbacks.resize_work.push((pane, pane_size));
        }
        Ok(PreparedFloatingPaneAddition {
            replacement,
            floating_focus,
            positioned,
            callbacks,
        })
    }

    fn commit_prepared_floating_pane_addition(
        &mut self,
        prepared: PreparedFloatingPaneAddition,
    ) -> (
        PositionedFloatingPane,
        DeferredTabCallbacks,
        Vec<FloatingPane>,
    ) {
        let PreparedFloatingPaneAddition {
            replacement,
            floating_focus,
            positioned,
            callbacks,
        } = prepared;
        let retired = std::mem::replace(&mut self.floating_panes, replacement);
        self.floating_focus = floating_focus;
        (positioned, callbacks, retired)
    }

    fn prepare_set_floating_pane_rect(
        &mut self,
        pane_id: PaneId,
        rect: FloatingPaneRect,
    ) -> (Option<PositionedFloatingPane>, DeferredTabCallbacks) {
        let Some(idx) = self.floating_index_by_id(pane_id) else {
            return (None, DeferredTabCallbacks::default());
        };
        let rect = self.clamp_floating_rect(rect);
        let size = self.floating_pane_size(rect);
        let floating = self
            .floating_panes
            .get_mut(idx)
            .expect("floating index remains valid");
        if floating.rect == rect {
            return (
                Some(PositionedFloatingPane {
                    pane_id: floating.pane_id,
                    is_focused: self.floating_focus == Some(floating.pane_id),
                    left: floating.rect.left,
                    top: floating.rect.top,
                    width: floating.rect.width,
                    height: floating.rect.height,
                    z_order: floating.z_order,
                    visible: floating.visible,
                    pinned: floating.pinned,
                    opacity: floating.opacity,
                    pane: Arc::clone(&floating.pane),
                }),
                DeferredTabCallbacks::default(),
            );
        }
        floating.rect = rect;
        let positioned = PositionedFloatingPane {
            pane_id: floating.pane_id,
            is_focused: self.floating_focus == Some(floating.pane_id),
            left: floating.rect.left,
            top: floating.rect.top,
            width: floating.rect.width,
            height: floating.rect.height,
            z_order: floating.z_order,
            visible: floating.visible,
            pinned: floating.pinned,
            opacity: floating.opacity,
            pane: Arc::clone(&floating.pane),
        };
        let mut callbacks = DeferredTabCallbacks {
            changed: true,
            ..DeferredTabCallbacks::default()
        };
        callbacks
            .resize_work
            .push((Arc::clone(&floating.pane), size));
        (Some(positioned), callbacks)
    }

    fn set_floating_pane_visible(&mut self, pane_id: PaneId, visible: bool) -> bool {
        let idx = match self.floating_index_by_id(pane_id) {
            Some(idx) => idx,
            None => return false,
        };
        let prior = self.get_active_pane();
        let floating = &mut self.floating_panes[idx];
        floating.visible = visible;
        if !visible && self.floating_focus == Some(pane_id) {
            self.floating_focus = None;
        }
        self.advise_focus_change(prior);
        true
    }

    fn bring_floating_pane_to_front(&mut self, pane_id: PaneId) -> bool {
        let idx = match self.floating_index_by_id(pane_id) {
            Some(idx) => idx,
            None => return false,
        };
        let next_z = self.next_floating_z_order();
        self.floating_panes[idx].z_order = next_z;
        true
    }

    fn set_floating_pane_z_order(&mut self, pane_id: PaneId, z_order: u32) -> bool {
        let idx = match self.floating_index_by_id(pane_id) {
            Some(idx) => idx,
            None => return false,
        };
        self.floating_panes[idx].z_order = z_order;
        true
    }

    fn set_floating_pane_focus(&mut self, pane_id: PaneId) -> bool {
        let idx = match self.floating_index_by_id(pane_id) {
            Some(idx) => idx,
            None => return false,
        };
        if !self.floating_panes[idx].visible {
            return false;
        }
        let prior = self.get_active_pane();
        let next_z = self.next_floating_z_order();
        self.floating_focus = Some(pane_id);
        self.floating_panes[idx].z_order = next_z;
        self.advise_focus_change(prior);
        true
    }

    fn iter_floating_panes(&self) -> Vec<PositionedFloatingPane> {
        let mut panes: Vec<PositionedFloatingPane> = self
            .floating_panes
            .iter()
            .map(|floating| self.positioned_floating_pane(floating))
            .collect();
        panes.sort_by(|left, right| {
            let left_key = (left.z_order, u8::from(left.is_focused));
            let right_key = (right.z_order, u8::from(right.is_focused));
            left_key.cmp(&right_key)
        });
        panes
    }

    /// Clone only structural tiled/stack pane identities without invoking a
    /// `Pane` trait method. Unlike `snapshot_panes_callback_free`, this census
    /// deliberately excludes floating and zoom carriers so reconciliation can
    /// detect a single exact pane that is simultaneously tiled and floating.
    /// Zoom is a view of a tiled pane rather than a second structural owner.
    fn snapshot_non_floating_panes_callback_free(&self) -> Vec<Arc<dyn Pane>> {
        let mut seen = HashSet::new();
        let mut panes = Vec::new();

        if let Some(tree) = &self.pane {
            let mut tree_panes = Vec::new();
            collect_raw_tree_panes(tree, &mut tree_panes);
            for pane in tree_panes {
                if seen.insert(pane_identity(&pane)) {
                    panes.push(pane);
                }
            }
        }
        for stack in self.pane_stacks.values() {
            for pane in stack.panes() {
                if seen.insert(pane_identity(pane)) {
                    panes.push(Arc::clone(pane));
                }
            }
        }

        panes
    }

    fn snapshot_structural_panes_callback_free_checked(
        &self,
    ) -> anyhow::Result<(Vec<Arc<dyn Pane>>, Vec<(PaneId, Arc<dyn Pane>)>)> {
        let mut tiled = Vec::new();
        if let Some(tree) = &self.pane {
            collect_raw_tree_panes(tree, &mut tiled);
        }
        let mut tree_identities = HashSet::new();
        tree_identities
            .try_reserve(tiled.len())
            .map_err(|error| anyhow::anyhow!("reserve raw tree identity census: {error}"))?;
        for pane in &tiled {
            anyhow::ensure!(
                tree_identities.insert(pane_identity(pane)),
                "one exact pane allocation occupies multiple tiled tree leaves"
            );
        }

        let stack_member_count = self
            .pane_stacks
            .values()
            .try_fold(0usize, |count, stack| {
                count
                    .checked_add(stack.len())
                    .ok_or_else(|| anyhow::anyhow!("pane stack member count overflow"))
            })?;
        tiled
            .try_reserve(stack_member_count)
            .map_err(|error| anyhow::anyhow!("reserve hidden stack pane census: {error}"))?;
        let mut stack_identities = HashSet::new();
        stack_identities
            .try_reserve(stack_member_count)
            .map_err(|error| anyhow::anyhow!("reserve stack identity census: {error}"))?;
        for stack in self.pane_stacks.values() {
            let active_identity = pane_identity(stack.active_pane());
            anyhow::ensure!(
                tree_identities.contains(&active_identity),
                "pane stack active allocation is absent from the tiled tree"
            );
            for pane in stack.panes() {
                let identity = pane_identity(pane);
                anyhow::ensure!(
                    stack_identities.insert(identity),
                    "one exact pane allocation belongs to multiple pane stacks"
                );
                if identity == active_identity {
                    continue;
                }
                anyhow::ensure!(
                    !tree_identities.contains(&identity),
                    "hidden pane stack allocation also occupies a tiled tree leaf"
                );
                tiled.push(Arc::clone(pane));
            }
        }

        let mut floating = Vec::new();
        floating
            .try_reserve_exact(self.floating_panes.len())
            .map_err(|error| anyhow::anyhow!("reserve floating structural census: {error}"))?;
        let mut floating_identities = HashSet::new();
        floating_identities
            .try_reserve(self.floating_panes.len())
            .map_err(|error| anyhow::anyhow!("reserve floating identity census: {error}"))?;
        for pane in &self.floating_panes {
            let identity = pane_identity(&pane.pane);
            anyhow::ensure!(
                floating_identities.insert(identity),
                "one exact pane allocation appears in multiple floating entries"
            );
            anyhow::ensure!(
                !tree_identities.contains(&identity) && !stack_identities.contains(&identity),
                "one exact pane allocation is simultaneously tiled and floating"
            );
            floating.push((pane.pane_id, Arc::clone(&pane.pane)));
        }
        if let Some(zoomed) = &self.zoomed {
            let identity = pane_identity(zoomed);
            let logical_owner_count = tiled
                .iter()
                .filter(|pane| pane_identity(pane) == identity)
                .count()
                .checked_add(
                    floating
                        .iter()
                        .filter(|(_, pane)| pane_identity(pane) == identity)
                        .count(),
                )
                .ok_or_else(|| anyhow::anyhow!("zoom owner count overflow"))?;
            anyhow::ensure!(
                logical_owner_count == 1,
                "zoom carrier has {logical_owner_count} logical structural owners"
            );
        }
        Ok((tiled, floating))
    }

    fn count_floating_panes(&self) -> usize {
        self.floating_panes.len()
    }

    fn focused_floating_pane(&self) -> Option<Arc<dyn Pane>> {
        let pane_id = self.floating_focus?;
        self.floating_panes
            .iter()
            .find(|floating| floating.visible && floating.pane_id == pane_id)
            .map(|floating| Arc::clone(&floating.pane))
    }

    fn clear_floating_focus(&mut self) {
        self.floating_focus = None;
    }

    fn has_floating_pane(&self, pane_id: PaneId) -> bool {
        self.floating_index_by_id(pane_id).is_some()
    }

    fn discard_removed_pane_state(&mut self, pane_id: PaneId) {
        self.constraint_overrides.remove(&pane_id);
        self.collapsed_panes.remove(&pane_id);
    }

    /// Re-key stacks after a tree mutation or rotation. Stack slot indices are
    /// topological leaf indices, so the authoritative association is the
    /// active pane that is also present in the tree.
    fn reindex_pane_stacks_from_tree(&mut self) {
        fn collect_leaf_indices(
            tree: &Tree,
            next_index: &mut usize,
            indices: &mut HashMap<PaneId, usize>,
        ) {
            match tree {
                Tree::Empty => {}
                Tree::Node { left, right, .. } => {
                    collect_leaf_indices(left, next_index, indices);
                    collect_leaf_indices(right, next_index, indices);
                }
                Tree::Leaf(pane) => {
                    indices.insert(pane.pane_id(), *next_index);
                    *next_index = next_index.saturating_add(1);
                }
            }
        }

        if self.pane_stacks.is_empty() {
            return;
        }
        let Some(tree) = self.pane.as_ref() else {
            log::error!(
                "tab {} has {} pane stacks but no pane tree",
                self.id,
                self.pane_stacks.len()
            );
            return;
        };

        let mut next_index = 0usize;
        let mut leaf_indices = HashMap::new();
        collect_leaf_indices(tree, &mut next_index, &mut leaf_indices);

        let mut targets = Vec::with_capacity(self.pane_stacks.len());
        let mut occupied = HashSet::with_capacity(self.pane_stacks.len());
        for (old_index, stack) in &self.pane_stacks {
            let active_id = stack.active_pane().pane_id();
            let Some(new_index) = leaf_indices.get(&active_id).copied() else {
                log::error!(
                    "tab {} stack slot {} has active pane {} without a representative tree leaf",
                    self.id,
                    old_index,
                    active_id
                );
                return;
            };
            if !occupied.insert(new_index) {
                log::error!(
                    "tab {} has multiple pane stacks mapped to tree slot {}",
                    self.id,
                    new_index
                );
                return;
            }
            targets.push((*old_index, new_index));
        }

        let mut stacks = std::mem::take(&mut self.pane_stacks);
        let mut remapped = HashMap::with_capacity(stacks.len());
        for (old_index, new_index) in targets {
            let stack = stacks
                .remove(&old_index)
                .expect("pane stack disappeared while reindexing");
            remapped.insert(new_index, stack);
        }
        self.pane_stacks = remapped;
    }

    fn prepare_floating_pane_resizes_to_fit(
        &mut self,
        resize_work: &mut Vec<(Arc<dyn Pane>, TerminalSize)>,
    ) {
        for idx in 0..self.floating_panes.len() {
            let rect = self.clamp_floating_rect(self.floating_panes[idx].rect);
            self.floating_panes[idx].rect = rect;
            resize_work.push((
                Arc::clone(&self.floating_panes[idx].pane),
                self.floating_pane_size(rect),
            ));
        }
        if let Some(pane_id) = self.floating_focus {
            let has_visible_focus = self
                .floating_panes
                .iter()
                .any(|floating| floating.visible && floating.pane_id == pane_id);
            if !has_visible_focus {
                self.floating_focus = None;
            }
        }
    }

    /// Determine which panes should be collapsed so that the tree fits
    /// within the given `(cols, rows)` budget.  Returns the set of pane
    /// IDs to collapse.  Panes with `CollapsePriority::Never` are exempt.
    fn select_panes_to_collapse(&self, cols: usize, rows: usize) -> HashSet<PaneId> {
        let tree = match self.pane.as_ref() {
            Some(t) => t,
            None => return HashSet::new(),
        };
        let mut collapsed = self.collapsed_panes.clone();

        // Collect candidates sorted by collapse order (Low first).
        let mut candidates: Vec<(PaneId, u8)> = collect_leaf_panes(tree)
            .into_iter()
            .filter_map(|(id, priority)| collapse_order(priority).map(|order| (id, order)))
            .filter(|(id, _)| !collapsed.contains(id))
            .collect();
        candidates.sort_by_key(|&(_, order)| order);

        for (pane_id, _) in candidates {
            let (min_w, min_h) =
                compute_min_size_with_collapsed(tree, &collapsed, &self.constraint_overrides);
            if min_w <= cols && min_h <= rows {
                break;
            }
            collapsed.insert(pane_id);
        }
        collapsed
    }

    /// Attempt to restore previously collapsed panes if the terminal has
    /// grown large enough to accommodate them.  Returns the updated
    /// collapsed set.
    fn select_panes_to_uncollapse(&self, cols: usize, rows: usize) -> HashSet<PaneId> {
        let tree = match self.pane.as_ref() {
            Some(t) => t,
            None => return HashSet::new(),
        };
        if self.collapsed_panes.is_empty() {
            return HashSet::new();
        }

        // Build restoration order: High priority panes restore first.
        let pane_priorities: HashMap<PaneId, CollapsePriority> =
            collect_leaf_panes(tree).into_iter().collect();
        let mut restore_candidates: Vec<PaneId> = self.collapsed_panes.iter().copied().collect();
        restore_candidates.sort_by(|a, b| {
            let a_order = pane_priorities
                .get(a)
                .and_then(|p| collapse_order(*p))
                .unwrap_or(3);
            let b_order = pane_priorities
                .get(b)
                .and_then(|p| collapse_order(*p))
                .unwrap_or(3);
            b_order.cmp(&a_order) // High priority (order 2) restores before Low (order 0)
        });

        let mut collapsed = self.collapsed_panes.clone();
        for pane_id in restore_candidates {
            let mut trial = collapsed.clone();
            trial.remove(&pane_id);
            let (min_w, min_h) =
                compute_min_size_with_collapsed(tree, &trial, &self.constraint_overrides);
            if min_w <= cols && min_h <= rows {
                collapsed = trial;
            }
        }
        collapsed
    }

    /// Returns `true` if the given pane is currently collapsed.
    fn is_pane_collapsed(&self, pane_id: PaneId) -> bool {
        self.collapsed_panes.contains(&pane_id)
    }

    /// Returns the set of currently collapsed pane IDs.
    fn collapsed_pane_ids(&self) -> &HashSet<PaneId> {
        &self.collapsed_panes
    }

    // --- Swap layout support ---

    /// Set the layout cycle for this tab.
    fn set_layout_cycle(&mut self, cycle: LayoutCycle) {
        self.layout_cycle = Some(cycle);
    }

    /// Swap to the next layout in the cycle.  Returns the name of the
    /// new layout, or None if no cycle is configured or the tab has no panes.
    fn prepare_swap_to_next_layout(&mut self) -> (Option<String>, DeferredTabCallbacks) {
        let Some(mut next_cycle) = self.layout_cycle.clone() else {
            return (None, DeferredTabCallbacks::default());
        };
        let layout = next_cycle.advance().clone();
        let result = self.prepare_apply_layout(&layout);
        if result.0.is_some() {
            self.layout_cycle = Some(next_cycle);
        }
        result
    }

    /// Swap to the previous layout in the cycle.
    fn prepare_swap_to_prev_layout(&mut self) -> (Option<String>, DeferredTabCallbacks) {
        let Some(mut next_cycle) = self.layout_cycle.clone() else {
            return (None, DeferredTabCallbacks::default());
        };
        let layout = next_cycle.prev().clone();
        let result = self.prepare_apply_layout(&layout);
        if result.0.is_some() {
            self.layout_cycle = Some(next_cycle);
        }
        result
    }

    /// Swap to a specific layout by index in the cycle.
    fn prepare_swap_to_layout_index(
        &mut self,
        index: usize,
    ) -> (Option<String>, DeferredTabCallbacks) {
        let Some(mut next_cycle) = self.layout_cycle.clone() else {
            return (None, DeferredTabCallbacks::default());
        };
        if !next_cycle.select(index) {
            return (None, DeferredTabCallbacks::default());
        }
        let layout = next_cycle.current().clone();
        let result = self.prepare_apply_layout(&layout);
        if result.0.is_some() {
            self.layout_cycle = Some(next_cycle);
        }
        result
    }

    /// Apply a layout, redistributing panes from the current tree.
    fn prepare_apply_layout(
        &mut self,
        layout: &SwapLayout,
    ) -> (Option<String>, DeferredTabCallbacks) {
        let prior_focus = self.raw_active_pane_retained_id();
        // Collect all panes from the current tree AND from any existing stacks.
        let all_panes = self.collect_all_panes();
        if all_panes.is_empty() {
            return (None, DeferredTabCallbacks::default());
        }

        let active_pane_id = self
            .get_active_pane()
            .map(|p| p.pane_id())
            .unwrap_or_else(|| all_panes[0].pane_id());

        let Some(result) =
            redistribute_panes(&layout.arrangement, all_panes, active_pane_id, self.size)
        else {
            return (None, DeferredTabCallbacks::default());
        };

        self.pane = Some(result.tree);
        self.pane_stacks = result.stacks;
        self.active = result.active_index;
        self.zoomed = None;
        self.collapsed_panes.clear();

        let current_focus = self.raw_active_pane_retained_id();
        if current_focus.is_some() {
            self.recency.tag(self.active);
        }
        let mut callbacks = DeferredTabCallbacks {
            changed: true,
            prior_focus,
            current_focus,
            current_focus_id: self
                .raw_active_pane_retained_id()
                .as_ref()
                .map(|pane| pane.pane_id()),
            ..DeferredTabCallbacks::default()
        };
        if let Some(tree) = self.pane.as_ref() {
            collect_pane_resize_work(tree, &self.size, &mut callbacks.resize_work);
        }

        (Some(layout.name.clone()), callbacks)
    }

    /// Collect all panes: from the tree leaves AND from stacked (hidden) panes.
    fn collect_all_panes(&mut self) -> Vec<Arc<dyn Pane>> {
        let mut panes: Vec<Arc<dyn Pane>> = Vec::new();
        let mut seen = HashSet::new();

        // Collect from tree leaves.
        let positioned = self.iter_panes_ignoring_zoom();
        for pp in &positioned {
            if seen.insert(pane_identity(&pp.pane)) {
                panes.push(Arc::clone(&pp.pane));
            }
        }

        // Snapshot hidden stack members without consuming the old topology.
        // Layout construction can fail; retaining the stacks until the new
        // tree is ready makes that failure byte-for-byte non-mutating.
        for stack in self.pane_stacks.values() {
            for pane in stack.panes() {
                if seen.insert(pane_identity(pane)) {
                    panes.push(Arc::clone(pane));
                }
            }
        }

        panes
    }

    /// Cycle to the next pane in a stack at the given slot index.
    /// Returns the newly visible pane ID, or None if no stack at that slot.
    fn cycle_stack(&mut self, slot_index: usize) -> Option<PaneId> {
        let stack = self.pane_stacks.get_mut(&slot_index)?;
        if stack.is_single() {
            return None; // nothing to cycle
        }

        let old_pane_id = stack.active_pane().pane_id();
        stack.cycle_next();
        let new_pane = stack.active_pane().clone();
        let new_pane_id = new_pane.pane_id();

        // Swap the visible pane in the tree leaf.
        self.replace_pane_in_tree(old_pane_id, new_pane);

        Some(new_pane_id)
    }

    /// Cycle to the previous pane in a stack at the given slot index.
    /// Returns the newly visible pane ID, or None if no stack at that slot.
    fn cycle_stack_backward(&mut self, slot_index: usize) -> Option<PaneId> {
        let stack = self.pane_stacks.get_mut(&slot_index)?;
        if stack.is_single() {
            return None; // nothing to cycle
        }

        let old_pane_id = stack.active_pane().pane_id();
        stack.cycle_prev();
        let new_pane = stack.active_pane().clone();
        let new_pane_id = new_pane.pane_id();

        // Swap the visible pane in the tree leaf.
        self.replace_pane_in_tree(old_pane_id, new_pane);

        Some(new_pane_id)
    }

    fn select_stack_pane(&mut self, slot_index: usize, pane_index: usize) -> Option<PaneId> {
        let stack = self.pane_stacks.get_mut(&slot_index)?;
        let old_pane_id = stack.active_pane().pane_id();
        if !stack.select(pane_index) {
            return None;
        }
        let new_pane = stack.active_pane().clone();
        let new_pane_id = new_pane.pane_id();
        self.replace_pane_in_tree(old_pane_id, new_pane);
        Some(new_pane_id)
    }

    /// Replace a pane in the tree by its ID with a new pane.
    fn replace_pane_in_tree(&mut self, old_id: PaneId, new_pane: Arc<dyn Pane>) {
        if let Some(tree) = self.pane.as_mut() {
            replace_pane_recursive(tree, old_id, new_pane);
            let size = self.size;
            apply_sizes_from_splits(tree, &size);
        }
    }

    /// Returns the current layout name, if a cycle is active.
    fn current_layout_name(&self) -> Option<String> {
        self.layout_cycle.as_ref().map(|c| c.current().name.clone())
    }

    /// Returns the number of pane stacks.
    fn stack_count(&self) -> usize {
        self.pane_stacks.len()
    }

    /// Returns the first stack slot index that has more than one pane.
    fn first_nontrivial_stack_slot_index(&self) -> Option<usize> {
        self.pane_stacks
            .iter()
            .filter_map(|(slot_index, stack)| (!stack.is_single()).then_some(*slot_index))
            .min()
    }

    /// Returns all stacked pane IDs across all slots.
    fn all_stacked_pane_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        for stack in self.pane_stacks.values() {
            ids.extend(stack.pane_ids());
        }
        ids
    }

    fn find_pane_by_id(&mut self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        if let Some(idx) = self.floating_index_by_id(pane_id) {
            return self
                .floating_panes
                .get(idx)
                .map(|floating| Arc::clone(&floating.pane));
        }
        if let Some(pane) = self
            .pane_stacks
            .values()
            .flat_map(|stack| stack.panes())
            .find(|pane| pane.pane_id() == pane_id)
        {
            return Some(Arc::clone(pane));
        }

        self.iter_panes_ignoring_zoom()
            .into_iter()
            .find(|positioned| positioned.pane.pane_id() == pane_id)
            .map(|positioned| positioned.pane)
    }

    /// Clone pane identities without invoking any `Pane` trait method.
    ///
    /// Numeric pane IDs are reusable and trait methods are arbitrary external
    /// code, so neither is suitable while `Tab::inner` is held. The erased data
    /// pointer is stable for the lifetime of these retained `Arc`s and is used
    /// only for process-local exact-identity deduplication.
    fn snapshot_panes_callback_free(&self) -> Vec<Arc<dyn Pane>> {
        let mut seen = HashSet::new();
        let mut panes = Vec::new();

        if let Some(tree) = &self.pane {
            let mut tree_panes = Vec::new();
            collect_raw_tree_panes(tree, &mut tree_panes);
            for pane in tree_panes {
                if seen.insert(pane_identity(&pane)) {
                    panes.push(pane);
                }
            }
        }
        for stack in self.pane_stacks.values() {
            for pane in stack.panes() {
                if seen.insert(pane_identity(pane)) {
                    panes.push(Arc::clone(pane));
                }
            }
        }
        for floating in &self.floating_panes {
            if seen.insert(pane_identity(&floating.pane)) {
                panes.push(Arc::clone(&floating.pane));
            }
        }
        if let Some(zoomed) = &self.zoomed {
            if seen.insert(pane_identity(zoomed)) {
                panes.push(Arc::clone(zoomed));
            }
        }

        panes
    }

    /// Fallibly census one ordered-snapshot tab before invoking pane code.
    ///
    /// The tree limit counts every `Tree` node, including `Empty`; the census
    /// limit counts every raw carrier visited across empty/split/leaf tree
    /// nodes, stack containers and members, floating panes, and the zoom
    /// carrier; it also caps the smaller set of unique pane identities. Both
    /// are deliberately enforced while `Tab::inner` is held and before
    /// `Pane::pane_id` or any rendering callback can run. This prevents a
    /// topology that will be rejected by the wire contract from first consuming
    /// unbounded native traversal, callback, or identity-snapshot work.
    fn snapshot_panes_callback_free_bounded(
        &self,
        max_depth: usize,
        max_tree_nodes: usize,
        max_census_work: usize,
        max_tree_leaves: usize,
        ledger: &mut PaneSnapshotCensusLedger,
    ) -> anyhow::Result<BoundedCallbackFreePaneCensus> {
        let stats_before = ledger.attempt_stats();
        let tree = self.pane.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "tab {} ordered pane snapshot has no pane tree; empty tabs lack size authority",
                self.id
            )
        })?;
        if matches!(tree, Tree::Empty) {
            anyhow::bail!(
                "tab {} ordered pane snapshot has an empty root; empty tabs lack size authority",
                self.id
            );
        }
        if max_tree_leaves == 0 {
            return Err(PaneSnapshotStructureRejection::TreeLeafLimit { count: 1, max: 0 }.into());
        }

        let initial_census_capacity = max_census_work.min(64);
        let mut owners = HashMap::new();
        owners
            .try_reserve(initial_census_capacity)
            .map_err(|error| anyhow::anyhow!("reserve ordered pane census owners: {error}"))?;
        let mut panes = Vec::new();
        panes
            .try_reserve_exact(initial_census_capacity)
            .map_err(|error| anyhow::anyhow!("reserve ordered pane census entries: {error}"))?;
        let mut tree_leaf_identities = Vec::new();
        tree_leaf_identities
            .try_reserve_exact(initial_census_capacity.min(max_tree_nodes))
            .map_err(|error| anyhow::anyhow!("reserve ordered pane tree identities: {error}"))?;
        let mut tree_coherence = Vec::new();
        tree_coherence
            .try_reserve_exact(max_tree_nodes.min(64))
            .map_err(|error| anyhow::anyhow!("reserve ordered pane tree coherence: {error}"))?;
        let mut census_work = 0_usize;

        let mut pending = Vec::new();
        pending
            .try_reserve_exact(max_depth.saturating_mul(2).min(128).min(max_tree_nodes))
            .map_err(|error| anyhow::anyhow!("reserve ordered pane census traversal: {error}"))?;
        reserve_pane_arena_stack_push(
            &mut pending,
            1,
            max_tree_nodes,
            "ordered pane census traversal",
        )?;
        pending.push((tree, 1_usize));
        let mut tree_nodes = 0_usize;

        while let Some((tree, depth)) = pending.pop() {
            if depth > max_depth {
                return Err(PaneSnapshotStructureRejection::TreeDepthLimit {
                    count: depth,
                    max: max_depth,
                }
                .into());
            }
            tree_nodes = tree_nodes.checked_add(1).ok_or(
                PaneSnapshotStructureRejection::ArithmeticOverflow {
                    counter: "tree_nodes",
                },
            )?;
            if tree_nodes > max_tree_nodes {
                return Err(PaneSnapshotStructureRejection::TreeNodeLimit {
                    count: tree_nodes,
                    max: max_tree_nodes,
                }
                .into());
            }
            admit_ordered_pane_census_work(
                &mut census_work,
                max_census_work,
                self.id,
                ledger,
                PaneSnapshotCensusKind::TreeNode,
            )?;

            reserve_pane_arena_stack_push(
                &mut tree_coherence,
                1,
                max_tree_nodes,
                "ordered pane tree coherence",
            )?;
            match tree {
                Tree::Empty => tree_coherence.push(OrderedPaneTreeCoherenceNode::Empty),
                Tree::Leaf(pane) => {
                    let next_leaf_count = tree_leaf_identities.len().checked_add(1).ok_or(
                        PaneSnapshotStructureRejection::ArithmeticOverflow {
                            counter: "tree_leaves",
                        },
                    )?;
                    if next_leaf_count > max_tree_leaves {
                        return Err(PaneSnapshotStructureRejection::TreeLeafLimit {
                            count: next_leaf_count,
                            max: max_tree_leaves,
                        }
                        .into());
                    }
                    tree_coherence.push(OrderedPaneTreeCoherenceNode::Leaf(pane_identity(pane)));
                    let leaf_index = tree_leaf_identities.len();
                    if push_bounded_callback_free_pane(
                        &mut owners,
                        &mut panes,
                        pane,
                        CallbackFreePaneOwner::TreeLeaf(leaf_index),
                        max_census_work,
                        self.id,
                        ledger,
                    )?
                    .is_some()
                    {
                        anyhow::bail!(
                            "an exact pane identity appears more than once in tab {} ordered pane tree",
                            self.id,
                        );
                    }
                    reserve_pane_arena_stack_push(
                        &mut tree_leaf_identities,
                        1,
                        max_census_work,
                        "ordered pane tree identities",
                    )?;
                    tree_leaf_identities.push(pane_identity(pane));
                }
                Tree::Node { left, right, data } => {
                    let node = data.ok_or_else(|| {
                        anyhow::anyhow!(
                            "tab {} ordered pane tree has an uninitialized split node",
                            self.id
                        )
                    })?;
                    tree_coherence.push(OrderedPaneTreeCoherenceNode::Split(node));
                    let next_depth = depth.checked_add(1).ok_or(
                        PaneSnapshotStructureRejection::ArithmeticOverflow {
                            counter: "tree_depth",
                        },
                    )?;
                    if next_depth > max_depth {
                        return Err(PaneSnapshotStructureRejection::TreeDepthLimit {
                            count: next_depth,
                            max: max_depth,
                        }
                        .into());
                    }
                    let discovered_nodes = tree_nodes
                        .checked_add(pending.len())
                        .and_then(|count| count.checked_add(2))
                        .ok_or(PaneSnapshotStructureRejection::ArithmeticOverflow {
                            counter: "tree_nodes",
                        })?;
                    if discovered_nodes > max_tree_nodes {
                        return Err(PaneSnapshotStructureRejection::TreeNodeLimit {
                            count: discovered_nodes,
                            max: max_tree_nodes,
                        }
                        .into());
                    }
                    reserve_pane_arena_stack_push(
                        &mut pending,
                        2,
                        max_tree_nodes,
                        "ordered pane census traversal",
                    )?;
                    pending.push((right, next_depth));
                    pending.push((left, next_depth));
                }
            }
        }

        if tree_leaf_identities.is_empty() {
            anyhow::bail!("tab {} ordered pane tree contains no pane leaves", self.id);
        }
        let active_identity = tree_leaf_identities
            .get(self.active)
            .copied()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "tab {} ordered active pane index {} is beyond {} tree leaves",
                    self.id,
                    self.active,
                    tree_leaf_identities.len()
                )
            })?;
        ledger.reserve(PaneSnapshotCensusKind::IdentityCheck, 1)?;
        if owners.get(&active_identity) != Some(&CallbackFreePaneOwner::TreeLeaf(self.active)) {
            anyhow::bail!(
                "tab {} ordered active pane index {} does not own its tree leaf",
                self.id,
                self.active
            );
        }
        let tree_active = panes.get(self.active).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "tab {} ordered active pane index {} disappeared from its census",
                self.id,
                self.active
            )
        })?;
        debug_assert_eq!(pane_identity(&tree_active), active_identity);

        let mut stack_coherence = HashMap::new();
        stack_coherence
            .try_reserve(self.pane_stacks.len().min(max_census_work))
            .map_err(|error| anyhow::anyhow!("reserve ordered pane stack coherence: {error}"))?;
        for (slot_index, stack) in &self.pane_stacks {
            admit_ordered_pane_census_work(
                &mut census_work,
                max_census_work,
                self.id,
                ledger,
                PaneSnapshotCensusKind::StackContainer,
            )?;
            if stack.is_empty() {
                anyhow::bail!(
                    "tab {} ordered pane census contains an empty stack at slot {slot_index}",
                    self.id
                );
            }
            let active_index = stack.active_index();
            if active_index >= stack.len() {
                anyhow::bail!(
                    "tab {} ordered pane stack {slot_index} has active index {active_index} beyond length {}",
                    self.id,
                    stack.len()
                );
            }
            let expected_active =
                tree_leaf_identities
                    .get(*slot_index)
                    .copied()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                    "tab {} ordered pane stack slot {slot_index} has no corresponding tree leaf",
                    self.id
                )
                    })?;
            let mut members = Vec::new();
            members
                .try_reserve_exact(stack.len().min(max_census_work))
                .map_err(|error| {
                    anyhow::anyhow!(
                        "reserve ordered pane stack {slot_index} member coherence: {error}"
                    )
                })?;

            for (stack_index, pane) in stack.panes().iter().enumerate() {
                admit_ordered_pane_census_work(
                    &mut census_work,
                    max_census_work,
                    self.id,
                    ledger,
                    PaneSnapshotCensusKind::StackMember,
                )?;
                let identity = pane_identity(pane);
                reserve_pane_arena_stack_push(
                    &mut members,
                    1,
                    max_census_work,
                    "ordered pane stack member coherence",
                )?;
                members.push(identity);
                if stack_index == active_index {
                    ledger.reserve(PaneSnapshotCensusKind::IdentityCheck, 1)?;
                    if identity != expected_active
                        || owners.get(&identity)
                            != Some(&CallbackFreePaneOwner::TreeLeaf(*slot_index))
                    {
                        anyhow::bail!(
                            "tab {} ordered pane stack {slot_index} active member does not own its tree leaf",
                            self.id
                        );
                    }
                    continue;
                }
                if let Some(prior_owner) = push_bounded_callback_free_pane(
                    &mut owners,
                    &mut panes,
                    pane,
                    CallbackFreePaneOwner::HiddenStack,
                    max_census_work,
                    self.id,
                    ledger,
                )? {
                    anyhow::bail!(
                        "tab {} ordered pane stack {slot_index} hidden member aliases {prior_owner:?}",
                        self.id
                    );
                }
            }
            if stack_coherence
                .insert(
                    *slot_index,
                    OrderedPaneStackCoherence {
                        active_index,
                        members,
                    },
                )
                .is_some()
            {
                anyhow::bail!(
                    "tab {} ordered pane census contains duplicate stack slot {slot_index}",
                    self.id
                );
            }
        }
        let mut floating_coherence = Vec::new();
        floating_coherence
            .try_reserve_exact(self.floating_panes.len().min(max_census_work))
            .map_err(|error| anyhow::anyhow!("reserve ordered floating pane coherence: {error}"))?;
        for floating in &self.floating_panes {
            admit_ordered_pane_census_work(
                &mut census_work,
                max_census_work,
                self.id,
                ledger,
                PaneSnapshotCensusKind::FloatingPane,
            )?;
            reserve_pane_arena_stack_push(
                &mut floating_coherence,
                1,
                max_census_work,
                "ordered floating pane coherence",
            )?;
            floating_coherence.push(OrderedFloatingPaneCoherence {
                identity: pane_identity(&floating.pane),
                rect: floating.rect,
                z_order: floating.z_order,
                visible: floating.visible,
                pinned: floating.pinned,
                opacity_bits: floating.opacity.to_bits(),
            });
            if let Some(prior_owner) = push_bounded_callback_free_pane(
                &mut owners,
                &mut panes,
                &floating.pane,
                CallbackFreePaneOwner::Floating,
                max_census_work,
                self.id,
                ledger,
            )? {
                anyhow::bail!(
                    "tab {} ordered floating pane aliases {prior_owner:?}",
                    self.id
                );
            }
        }
        if let Some(zoomed) = &self.zoomed {
            admit_ordered_pane_census_work(
                &mut census_work,
                max_census_work,
                self.id,
                ledger,
                PaneSnapshotCensusKind::ZoomCarrier,
            )?;
            ledger.reserve(PaneSnapshotCensusKind::IdentityCheck, 1)?;
            match owners.get(&pane_identity(zoomed)) {
                Some(CallbackFreePaneOwner::TreeLeaf(_) | CallbackFreePaneOwner::Floating) => {}
                Some(CallbackFreePaneOwner::HiddenStack) => anyhow::bail!(
                    "tab {} ordered zoom state aliases a hidden stack member",
                    self.id
                ),
                None => anyhow::bail!(
                    "tab {} ordered zoom state does not belong to its tree or floating panes",
                    self.id
                ),
            }
        }

        let tree_leaf_count = tree_leaf_identities.len();
        Ok(BoundedCallbackFreePaneCensus {
            panes,
            tree_leaf_count,
            tree_active,
            coherence: OrderedPaneCoherence {
                tree: tree_coherence,
                active: self.active,
                stacks: stack_coherence,
                floating: floating_coherence,
                floating_focus: self.floating_focus,
                zoomed: self.zoomed.as_ref().map(pane_identity),
                title: Arc::clone(&self.title),
            },
            stats: ledger.attempt_stats().checked_delta(stats_before)?,
        })
    }

    fn raw_tree_active_pane(&self) -> Option<Arc<dyn Pane>> {
        self.raw_tree_pane_at_index(self.active)
    }

    fn raw_tree_pane_at_index(&self, index: usize) -> Option<Arc<dyn Pane>> {
        let tree = self.pane.as_ref()?;
        let mut leaves = Vec::new();
        collect_raw_tree_leaves(tree, &mut leaves);
        leaves.get(index).cloned()
    }

    fn raw_active_pane_retained_id(&self) -> Option<Arc<dyn Pane>> {
        if let Some(zoomed) = &self.zoomed {
            return Some(Arc::clone(zoomed));
        }
        if let Some(focused_id) = self.floating_focus {
            if let Some(focused) = self
                .floating_panes
                .iter()
                .find(|floating| floating.visible && floating.pane_id == focused_id)
            {
                return Some(Arc::clone(&focused.pane));
            }
        }
        self.raw_tree_active_pane()
    }

    fn raw_active_pane_callback_free(
        &self,
        _pane_ids: &HashMap<PaneIdentity, PaneId>,
    ) -> Option<Arc<dyn Pane>> {
        self.raw_active_pane_retained_id()
    }

    fn raw_active_pane_callback_free_with_tree_active(
        &self,
        _pane_ids: &HashMap<PaneIdentity, PaneId>,
        tree_active: Arc<dyn Pane>,
    ) -> Arc<dyn Pane> {
        if let Some(zoomed) = &self.zoomed {
            return Arc::clone(zoomed);
        }
        if let Some(focused_id) = self.floating_focus {
            if let Some(focused) = self
                .floating_panes
                .iter()
                .find(|floating| floating.visible && floating.pane_id == focused_id)
            {
                return Arc::clone(&focused.pane);
            }
        }
        tree_active
    }

    fn reindex_pane_stacks_callback_free(&mut self) {
        if self.pane_stacks.is_empty() {
            return;
        }
        let Some(tree) = self.pane.as_ref() else {
            log::error!(
                "tab {} has {} pane stacks but no pane tree",
                self.id,
                self.pane_stacks.len()
            );
            return;
        };

        let mut leaves = Vec::new();
        collect_raw_tree_leaves(tree, &mut leaves);
        let leaf_indices = leaves
            .iter()
            .enumerate()
            .map(|(index, pane)| (pane_identity(pane), index))
            .collect::<HashMap<_, _>>();
        let mut targets = Vec::with_capacity(self.pane_stacks.len());
        let mut occupied = HashSet::with_capacity(self.pane_stacks.len());
        for (old_index, stack) in &self.pane_stacks {
            let active_identity = pane_identity(stack.active_pane());
            let Some(new_index) = leaf_indices.get(&active_identity).copied() else {
                log::error!(
                    "tab {} stack slot {} has an active pane without an exact representative \
                     tree leaf",
                    self.id,
                    old_index
                );
                return;
            };
            if !occupied.insert(new_index) {
                log::error!(
                    "tab {} has multiple pane stacks mapped to tree slot {}",
                    self.id,
                    new_index
                );
                return;
            }
            targets.push((*old_index, new_index));
        }

        let mut stacks = std::mem::take(&mut self.pane_stacks);
        let mut remapped = HashMap::with_capacity(stacks.len());
        for (old_index, new_index) in targets {
            let stack = stacks
                .remove(&old_index)
                .expect("pane stack disappeared while callback-free reindexing");
            remapped.insert(new_index, stack);
        }
        self.pane_stacks = remapped;
    }

    /// Apply an already-authorized exact-identity removal plan.
    ///
    /// This method must remain callback-free: callers hold mux registration
    /// authority while entering it. All values needed from pane trait methods
    /// were observed before acquiring `Tab::inner`.
    fn prepare_exact_pane_removal(
        &self,
        observed: &[ObservedPane],
        candidates: &[ExactPaneRemovalCandidate],
    ) -> PreparedExactPaneRemoval {
        // Build the complete successor away from the authoritative tab. Tree,
        // stack, set, map, and callback-work allocation therefore all happen
        // before the structural commit. The clone contains only callback-free
        // mux state and retained `Arc` handles; it invokes no `Pane` method.
        let mut replacement = self.clone();
        let callbacks = replacement.remove_exact_panes_callback_free(observed, candidates);
        PreparedExactPaneRemoval {
            replacement,
            callbacks,
        }
    }

    fn commit_prepared_exact_pane_removal(
        &mut self,
        prepared: PreparedExactPaneRemoval,
    ) -> (DeferredTabCallbacks, TabInner) {
        let PreparedExactPaneRemoval {
            replacement,
            callbacks,
        } = prepared;
        let retired = std::mem::replace(self, replacement);
        (callbacks, retired)
    }

    fn remove_exact_panes_callback_free(
        &mut self,
        observed: &[ObservedPane],
        candidates: &[ExactPaneRemovalCandidate],
    ) -> DeferredTabCallbacks {
        if candidates.is_empty() {
            return DeferredTabCallbacks::default();
        }

        let removals = candidates
            .iter()
            .map(|candidate| pane_identity(&candidate.pane))
            .collect::<HashSet<_>>();
        let pane_ids = observed
            .iter()
            .filter_map(|observed| {
                observed
                    .pane_id
                    .map(|pane_id| (pane_identity(&observed.pane), pane_id))
            })
            .collect::<HashMap<_, _>>();
        let prior_focus = self.raw_active_pane_callback_free(&pane_ids);
        let prior_tree_active = self.raw_tree_active_pane();
        let prior_focus_was_floating = prior_focus.as_ref().is_some_and(|prior| {
            self.floating_panes
                .iter()
                .any(|floating| Arc::ptr_eq(&floating.pane, prior))
        });

        let mut callbacks = DeferredTabCallbacks {
            prior_focus,
            ..DeferredTabCallbacks::default()
        };
        let mut tree_replacements = HashMap::new();

        // Rebuild stacks by exact Arc identity. PaneStack's ID-based removal
        // helper deliberately isn't used here: PaneId is a reusable slot.
        let old_stacks = std::mem::take(&mut self.pane_stacks);
        for (slot_index, stack) in old_stacks {
            let old_panes = stack.panes().to_vec();
            let old_active_index = stack.active_index().min(old_panes.len().saturating_sub(1));
            let old_active = old_panes.get(old_active_index).cloned();
            let mut survivors = Vec::with_capacity(old_panes.len());
            let mut survivors_before_active = 0usize;
            for (index, pane) in old_panes.into_iter().enumerate() {
                let identity = pane_identity(&pane);
                if removals.contains(&identity) {
                    callbacks.removed.insert(identity);
                    continue;
                }
                if index < old_active_index {
                    survivors_before_active = survivors_before_active.saturating_add(1);
                }
                survivors.push(pane);
            }

            if survivors.is_empty() {
                continue;
            }
            let active_survived = old_active
                .as_ref()
                .is_some_and(|pane| !removals.contains(&pane_identity(pane)));
            let new_active_index = if active_survived {
                survivors_before_active
            } else {
                survivors_before_active.min(survivors.len() - 1)
            };
            let mut rebuilt = PaneStack::new(survivors);
            let selected = rebuilt.select(new_active_index);
            debug_assert!(selected, "computed stack active index must be valid");

            if let Some(old_active) = old_active {
                let old_active_identity = pane_identity(&old_active);
                if removals.contains(&old_active_identity) {
                    tree_replacements
                        .insert(old_active_identity, Arc::clone(rebuilt.active_pane()));
                }
            }
            self.pane_stacks.insert(slot_index, rebuilt);
        }

        let mut tree_changed = false;
        if let Some(tree) = self.pane.take() {
            let (tree, changed) = remove_exact_panes_from_tree(
                tree,
                &removals,
                &tree_replacements,
                &mut callbacks.removed,
            );
            self.pane = Some(tree);
            tree_changed = changed;
        }

        let old_floating = std::mem::take(&mut self.floating_panes);
        self.floating_panes.reserve(old_floating.len());
        for floating in old_floating {
            let identity = pane_identity(&floating.pane);
            if removals.contains(&identity) {
                callbacks.removed.insert(identity);
            } else {
                self.floating_panes.push(floating);
            }
        }

        if self
            .zoomed
            .as_ref()
            .is_some_and(|zoomed| removals.contains(&pane_identity(zoomed)))
        {
            if let Some(zoomed) = self.zoomed.take() {
                callbacks.removed.insert(pane_identity(&zoomed));
            }
        }
        if prior_focus_was_floating
            && callbacks
                .prior_focus
                .as_ref()
                .is_some_and(|prior| callbacks.removed.contains(&pane_identity(prior)))
        {
            // Do not transfer focus to an unrelated pane that reused the same
            // numeric ID.
            self.floating_focus = None;
        }

        if callbacks.removed.is_empty() {
            return DeferredTabCallbacks::default();
        }
        callbacks.changed = true;

        self.reindex_pane_stacks_callback_free();

        let mut remaining_tree_panes = Vec::new();
        if let Some(tree) = &self.pane {
            collect_raw_tree_leaves(tree, &mut remaining_tree_panes);
        }
        if remaining_tree_panes.is_empty() {
            self.active = 0;
        } else if let Some(prior_tree_active) = &prior_tree_active {
            self.active = remaining_tree_panes
                .iter()
                .position(|pane| Arc::ptr_eq(pane, prior_tree_active))
                .unwrap_or_else(|| self.active.min(remaining_tree_panes.len() - 1));
        } else {
            self.active = self.active.min(remaining_tree_panes.len() - 1);
        }

        let remaining_ids = self
            .snapshot_panes_callback_free()
            .into_iter()
            .filter_map(|pane| pane_ids.get(&pane_identity(&pane)).copied())
            .collect::<HashSet<_>>();
        for candidate in candidates {
            if callbacks.removed.contains(&pane_identity(&candidate.pane))
                && !remaining_ids.contains(&candidate.pane_id)
            {
                self.discard_removed_pane_state(candidate.pane_id);
            }
        }

        if tree_changed {
            if let Some(tree) = self.pane.as_mut() {
                normalize_tree_sizes_callback_free(tree, self.size, &mut callbacks.resize_work);
            }
        }

        callbacks.current_focus = self.raw_active_pane_callback_free(&pane_ids);
        callbacks.current_focus_id = callbacks
            .current_focus
            .as_ref()
            .and_then(|pane| pane_ids.get(&pane_identity(pane)).copied());
        callbacks
    }

    fn effective_pane_constraints_for(&mut self, pane_id: PaneId) -> Option<PaneConstraints> {
        let pane = self.find_pane_by_id(pane_id)?;
        Some(effective_pane_constraints(
            &pane,
            &self.constraint_overrides,
        ))
    }

    fn prepare_update_pane_constraints(
        &mut self,
        pane_id: PaneId,
        min_width: Option<usize>,
        max_width: Option<usize>,
        min_height: Option<usize>,
        max_height: Option<usize>,
    ) -> (Option<PaneConstraints>, DeferredTabCallbacks) {
        let Some(pane) = self.find_pane_by_id(pane_id) else {
            return (None, DeferredTabCallbacks::default());
        };
        let intrinsic = pane.pane_constraints();
        let prior = self
            .constraint_overrides
            .get(&pane_id)
            .copied()
            .unwrap_or(intrinsic);
        let mut updated = prior;
        if let Some(value) = min_width {
            updated.min_width = value;
        }
        if let Some(value) = max_width {
            updated.max_width = Some(value);
        }
        if let Some(value) = min_height {
            updated.min_height = value;
        }
        if let Some(value) = max_height {
            updated.max_height = Some(value);
        }
        let updated = normalize_runtime_pane_constraints(updated);
        if updated == prior {
            return (Some(updated), DeferredTabCallbacks::default());
        }
        if updated == intrinsic {
            self.constraint_overrides.remove(&pane_id);
        } else {
            self.constraint_overrides.insert(pane_id, updated);
        }

        let size = self.size;
        (Some(updated), self.prepare_resize_for_reflow(size))
    }

    /// Compute the resize budget for a split identified by its topological
    /// index.  Returns `None` if the index is out of range, otherwise
    /// `(max_shrink, max_grow)` for the first child.
    fn compute_split_budget(&self, split_index: usize) -> Option<(isize, isize)> {
        let tree = self.pane.as_ref()?;
        let mut counter = 0usize;
        find_split_budget(tree, split_index, &mut counter, &self.constraint_overrides)
    }

    /// Walks the pane tree to produce the topologically ordered flattened
    /// list of PositionedPane instances along with their positioning information.
    fn iter_panes(&mut self) -> Vec<PositionedPane> {
        self.iter_panes_impl(true)
    }

    /// Like iter_panes, except that it will include all panes, regardless of
    /// whether one of them is currently zoomed.
    fn iter_panes_ignoring_zoom(&mut self) -> Vec<PositionedPane> {
        self.iter_panes_impl(false)
    }

    fn prepare_rotate_counter_clockwise(&mut self) -> DeferredTabCallbacks {
        let panes = self.iter_panes_ignoring_zoom();
        if panes.is_empty() {
            // Shouldn't happen, but we check for this here so that the
            // expect below cannot trigger a panic
            return DeferredTabCallbacks::default();
        }
        let mut pane_to_swap = panes
            .first()
            .map(|p| p.pane.clone())
            .expect("at least one pane");

        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                std::mem::swap(&mut pane_to_swap, cursor.leaf_mut().unwrap());
            }

            match cursor.postorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }
        self.reindex_pane_stacks_from_tree();
        let mut callbacks = DeferredTabCallbacks {
            changed: true,
            ..DeferredTabCallbacks::default()
        };
        collect_pane_resize_work(
            self.pane
                .as_ref()
                .expect("rotated pane tree remains present"),
            &self.size,
            &mut callbacks.resize_work,
        );
        callbacks
    }

    fn prepare_rotate_clockwise(&mut self) -> DeferredTabCallbacks {
        let panes = self.iter_panes_ignoring_zoom();
        if panes.is_empty() {
            // Shouldn't happen, but we check for this here so that the
            // expect below cannot trigger a panic
            return DeferredTabCallbacks::default();
        }
        let mut pane_to_swap = panes
            .last()
            .map(|p| p.pane.clone())
            .expect("at least one pane");

        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                std::mem::swap(&mut pane_to_swap, cursor.leaf_mut().unwrap());
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }
        self.reindex_pane_stacks_from_tree();
        let mut callbacks = DeferredTabCallbacks {
            changed: true,
            ..DeferredTabCallbacks::default()
        };
        collect_pane_resize_work(
            self.pane
                .as_ref()
                .expect("rotated pane tree remains present"),
            &self.size,
            &mut callbacks.resize_work,
        );
        callbacks
    }

    fn iter_panes_impl(&mut self, respect_zoom_state: bool) -> Vec<PositionedPane> {
        let mut panes = vec![];

        if respect_zoom_state {
            if let Some(zoomed) = self.zoomed.as_ref() {
                let size = self.size;
                panes.push(PositionedPane {
                    index: 0,
                    is_active: true,
                    is_zoomed: true,
                    left: 0,
                    top: 0,
                    width: size.cols.into(),
                    pixel_width: size.pixel_width.into(),
                    height: size.rows.into(),
                    pixel_height: size.pixel_height.into(),
                    pane: Arc::clone(zoomed),
                });
                return panes;
            }
        }

        let active_idx = self.active;
        let zoomed = self.zoomed.as_ref().map(Arc::clone);
        let root_size = self.size;
        let mut cursor = self.pane.take().unwrap().cursor();

        loop {
            if cursor.is_leaf() {
                let index = panes.len();
                let mut left = 0usize;
                let mut top = 0usize;
                let mut parent_size = None;
                for (branch, node) in cursor.path_to_root() {
                    if let Some(node) = node {
                        if parent_size.is_none() {
                            parent_size.replace(if branch == PathBranch::IsRight {
                                node.second
                            } else {
                                node.first
                            });
                        }
                        if branch == PathBranch::IsRight {
                            top += node.top_of_second();
                            left += node.left_of_second();
                        }
                    }
                }

                let pane = Arc::clone(cursor.leaf_mut().unwrap());
                let dims = parent_size.unwrap_or_else(|| root_size);

                panes.push(PositionedPane {
                    index,
                    is_active: index == active_idx,
                    is_zoomed: zoomed
                        .as_ref()
                        .is_some_and(|zoomed| Arc::ptr_eq(zoomed, &pane)),
                    left,
                    top,
                    width: dims.cols as _,
                    height: dims.rows as _,
                    pixel_width: dims.pixel_width as _,
                    pixel_height: dims.pixel_height as _,
                    pane,
                });
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        panes
    }

    fn iter_splits(&mut self) -> Vec<PositionedSplit> {
        let mut dividers = vec![];
        if self.zoomed.is_some() {
            return dividers;
        }

        let mut cursor = self.pane.take().unwrap().cursor();
        let mut index = 0;

        loop {
            if !cursor.is_leaf() {
                let mut left = 0usize;
                let mut top = 0usize;
                for (branch, p) in cursor.path_to_root() {
                    if let Some(p) = p {
                        if branch == PathBranch::IsRight {
                            left += p.left_of_second();
                            top += p.top_of_second();
                        }
                    }
                }
                if let Ok(Some(node)) = cursor.node_mut() {
                    match node.direction {
                        SplitDirection::Horizontal => left += node.first.cols as usize,
                        SplitDirection::Vertical => top += node.first.rows as usize,
                    }

                    dividers.push(PositionedSplit {
                        index,
                        direction: node.direction,
                        left,
                        top,
                        size: if node.direction == SplitDirection::Horizontal {
                            node.height() as usize
                        } else {
                            node.width() as usize
                        },
                    })
                }
                index += 1;
            }

            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }

        dividers
    }

    fn get_size(&self) -> TerminalSize {
        self.size
    }

    fn prepare_resize(&mut self, size: TerminalSize) -> DeferredTabCallbacks {
        self.prepare_resize_impl(size, false)
    }

    fn prepare_resize_for_reflow(&mut self, size: TerminalSize) -> DeferredTabCallbacks {
        self.prepare_resize_impl(size, true)
    }

    fn prepare_resize_impl(
        &mut self,
        size: TerminalSize,
        force_reflow: bool,
    ) -> DeferredTabCallbacks {
        let mut callbacks = DeferredTabCallbacks::default();
        if size.rows == 0 || size.cols == 0 {
            // Ignore "impossible" resize requests
            return callbacks;
        }
        if !force_reflow && self.size == size {
            return callbacks;
        }

        if let Some(zoomed) = &self.zoomed {
            self.size = size;
            callbacks.resize_work.push((Arc::clone(zoomed), size));
        } else {
            let dims = cell_dimensions(&size);
            let width_constraints = compute_axis_constraints(
                self.pane.as_ref().unwrap(),
                Axis::Width,
                &self.constraint_overrides,
            );
            let height_constraints = compute_axis_constraints(
                self.pane.as_ref().unwrap(),
                Axis::Height,
                &self.constraint_overrides,
            );
            let current_size = self.size;

            // If the tree minimum exceeds available space, collapse panes
            // in priority order to make it fit.
            if width_constraints.min > size.cols || height_constraints.min > size.rows {
                self.collapsed_panes = self.select_panes_to_collapse(size.cols, size.rows);
            } else if !self.collapsed_panes.is_empty() {
                // Terminal grew — try to restore previously collapsed panes
                self.collapsed_panes = self.select_panes_to_uncollapse(size.cols, size.rows);
            }

            // Constrain the new size to the minimum possible dimensions
            let cols = width_constraints
                .max
                .map_or(size.cols.max(width_constraints.min), |max_cols| {
                    size.cols.max(width_constraints.min).min(max_cols)
                });
            let rows = height_constraints
                .max
                .map_or(size.rows.max(height_constraints.min), |max_rows| {
                    size.rows.max(height_constraints.min).min(max_rows)
                });
            let size = TerminalSize {
                rows,
                cols,
                pixel_width: cols.saturating_mul(dims.pixel_width),
                pixel_height: rows.saturating_mul(dims.pixel_height),
                dpi: dims.dpi,
            };

            // Update the split nodes with adjusted sizes
            adjust_x_size(
                self.pane.as_mut().unwrap(),
                resize_delta_between(cols, current_size.cols),
                &dims,
                &self.constraint_overrides,
            );
            adjust_y_size(
                self.pane.as_mut().unwrap(),
                resize_delta_between(rows, current_size.rows),
                &dims,
                &self.constraint_overrides,
            );

            // Redistribute space away from collapsed subtrees so that
            // their siblings receive the freed columns/rows.
            if !self.collapsed_panes.is_empty() {
                redistribute_for_collapsed(
                    self.pane.as_mut().unwrap(),
                    &self.collapsed_panes,
                    &dims,
                );
            }

            self.size = size;

            // And then resize the individual panes to match
            collect_pane_resize_work(
                self.pane
                    .as_ref()
                    .expect("pane tree retained during resize"),
                &size,
                &mut callbacks.resize_work,
            );
        }

        self.prepare_floating_pane_resizes_to_fit(&mut callbacks.resize_work);
        callbacks.changed = true;
        callbacks
    }

    fn apply_pane_size(&mut self, pane_size: TerminalSize, cursor: &mut Cursor) {
        let cell_width = pane_size
            .pixel_width
            .checked_div(pane_size.cols)
            .unwrap_or(1);
        let cell_height = pane_size
            .pixel_height
            .checked_div(pane_size.rows)
            .unwrap_or(1);
        let (
            left_width_constraints,
            left_height_constraints,
            right_width_constraints,
            right_height_constraints,
        ) = match cursor.subtree() {
            Tree::Node {
                left,
                right,
                data: Some(_),
            } => {
                let left_width_constraints =
                    compute_axis_constraints(&**left, Axis::Width, &self.constraint_overrides);
                let left_height_constraints =
                    compute_axis_constraints(&**left, Axis::Height, &self.constraint_overrides);
                let right_width_constraints =
                    compute_axis_constraints(&**right, Axis::Width, &self.constraint_overrides);
                let right_height_constraints =
                    compute_axis_constraints(&**right, Axis::Height, &self.constraint_overrides);
                (
                    left_width_constraints,
                    left_height_constraints,
                    right_width_constraints,
                    right_height_constraints,
                )
            }
            _ => return,
        };
        if let Ok(Some(node)) = cursor.node_mut() {
            // Adjust the size of the node; we preserve the size of the first
            // child and adjust the second, so if we are split down the middle
            // and the window is made wider, the right column will grow in
            // size, leaving the left at its current width.
            if node.direction == SplitDirection::Horizontal {
                node.first.rows = pane_size.rows;
                node.second.rows = pane_size.rows;

                if let Some((first_cols, second_cols)) = split_allocation(
                    pane_size.cols,
                    left_width_constraints,
                    right_width_constraints,
                    Some(node.first.cols),
                ) {
                    node.first.cols = first_cols;
                    node.second.cols = second_cols;
                } else {
                    return;
                }
            } else {
                node.first.cols = pane_size.cols;
                node.second.cols = pane_size.cols;

                if let Some((first_rows, second_rows)) = split_allocation(
                    pane_size.rows,
                    left_height_constraints,
                    right_height_constraints,
                    Some(node.first.rows),
                ) {
                    node.first.rows = first_rows;
                    node.second.rows = second_rows;
                } else {
                    return;
                }
            }
            node.first.pixel_width = pixel_span(cell_width, node.first.cols);
            node.first.pixel_height = pixel_span(cell_height, node.first.rows);

            node.second.pixel_width = pixel_span(cell_width, node.second.cols);
            node.second.pixel_height = pixel_span(cell_height, node.second.rows);
        }
    }

    fn rebuild_splits_sizes_from_contained_panes(&mut self) -> bool {
        if self.zoomed.is_some() {
            return false;
        }

        fn compute_size(node: &mut Tree) -> Option<TerminalSize> {
            match node {
                Tree::Empty => None,
                Tree::Leaf(pane) => {
                    let dims = pane.get_dimensions();
                    let size = TerminalSize {
                        cols: dims.cols,
                        rows: dims.viewport_rows,
                        pixel_height: dims.pixel_height,
                        pixel_width: dims.pixel_width,
                        dpi: dims.dpi,
                    };
                    Some(size)
                }
                Tree::Node { left, right, data } => {
                    if let Some(data) = data {
                        if let Some(first) = compute_size(left) {
                            data.first = first;
                        }
                        if let Some(second) = compute_size(right) {
                            data.second = second;
                        }
                        Some(data.size())
                    } else {
                        None
                    }
                }
            }
        }

        let Some(root) = self.pane.as_mut() else {
            return false;
        };
        let Some(size) = compute_size(root) else {
            return false;
        };
        self.size = size;
        true
    }

    fn prepare_resize_split_by(
        &mut self,
        split_index: usize,
        delta: isize,
    ) -> DeferredTabCallbacks {
        let mut callbacks = DeferredTabCallbacks::default();
        if self.zoomed.is_some() {
            return callbacks;
        }

        let mut cursor = self.pane.take().unwrap().cursor();
        let mut index = 0;

        // Position cursor on the specified split
        loop {
            if !cursor.is_leaf() {
                if index == split_index {
                    // Found it
                    break;
                }
                index += 1;
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    // Didn't find it
                    self.pane.replace(c.tree());
                    return callbacks;
                }
            }
        }

        // Now cursor is looking at the split
        if !self.adjust_node_at_cursor(&mut cursor, delta) {
            self.pane.replace(cursor.tree());
            return callbacks;
        }
        self.cascade_size_from_cursor(cursor, &mut callbacks.resize_work);
        callbacks.changed = true;
        callbacks
    }

    fn adjust_node_at_cursor(&mut self, cursor: &mut Cursor, delta: isize) -> bool {
        let cell_dimensions = self.cell_dimensions();
        let (
            left_width_constraints,
            left_height_constraints,
            right_width_constraints,
            right_height_constraints,
        ) = match cursor.subtree() {
            Tree::Node {
                left,
                right,
                data: Some(_),
            } => {
                let left_width_constraints =
                    compute_axis_constraints(&**left, Axis::Width, &self.constraint_overrides);
                let left_height_constraints =
                    compute_axis_constraints(&**left, Axis::Height, &self.constraint_overrides);
                let right_width_constraints =
                    compute_axis_constraints(&**right, Axis::Width, &self.constraint_overrides);
                let right_height_constraints =
                    compute_axis_constraints(&**right, Axis::Height, &self.constraint_overrides);
                (
                    left_width_constraints,
                    left_height_constraints,
                    right_width_constraints,
                    right_height_constraints,
                )
            }
            _ => return false,
        };
        if let Ok(Some(node)) = cursor.node_mut() {
            let before_first = node.first;
            let before_second = node.second;
            match node.direction {
                SplitDirection::Horizontal => {
                    let width = node.width();
                    let preferred_cols = offset_by_resize_delta(node.first.cols, delta);
                    if let Some((first_cols, second_cols)) = split_allocation(
                        width,
                        left_width_constraints,
                        right_width_constraints,
                        Some(preferred_cols),
                    ) {
                        node.first.cols = first_cols;
                        node.second.cols = second_cols;
                        node.first.pixel_width =
                            node.first.cols.saturating_mul(cell_dimensions.pixel_width);
                        node.second.pixel_width =
                            node.second.cols.saturating_mul(cell_dimensions.pixel_width);
                    }
                }
                SplitDirection::Vertical => {
                    let height = node.height();
                    let preferred_rows = offset_by_resize_delta(node.first.rows, delta);
                    if let Some((first_rows, second_rows)) = split_allocation(
                        height,
                        left_height_constraints,
                        right_height_constraints,
                        Some(preferred_rows),
                    ) {
                        node.first.rows = first_rows;
                        node.second.rows = second_rows;
                        node.first.pixel_height =
                            node.first.rows.saturating_mul(cell_dimensions.pixel_height);
                        node.second.pixel_height = node
                            .second
                            .rows
                            .saturating_mul(cell_dimensions.pixel_height);
                    }
                }
            }
            return node.first != before_first || node.second != before_second;
        }
        false
    }

    fn cascade_size_from_cursor(
        &mut self,
        mut cursor: Cursor,
        resize_work: &mut Vec<(Arc<dyn Pane>, TerminalSize)>,
    ) {
        // Now we need to cascade this down to children
        match cursor.preorder_next() {
            Ok(c) => cursor = c,
            Err(c) => {
                self.pane.replace(c.tree());
                return;
            }
        }
        let root_size = self.size;

        loop {
            // Figure out the available size by looking at our immediate parent node.
            // If we are the root, look at the provided new size
            let pane_size = if let Some((branch, Some(parent))) = cursor.path_to_root().next() {
                if branch == PathBranch::IsRight {
                    parent.second
                } else {
                    parent.first
                }
            } else {
                root_size
            };

            if cursor.is_leaf() {
                if let Some(pane) = cursor.leaf_mut() {
                    resize_work.push((Arc::clone(pane), pane_size));
                }
            } else {
                self.apply_pane_size(pane_size, &mut cursor);
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    break;
                }
            }
        }
    }

    fn prepare_adjust_pane_size(
        &mut self,
        direction: PaneDirection,
        amount: usize,
    ) -> DeferredTabCallbacks {
        let mut callbacks = DeferredTabCallbacks::default();
        if self.zoomed.is_some() {
            return callbacks;
        }
        let active_index = self.active;
        let mut cursor = self.pane.take().unwrap().cursor();
        let mut index = 0;

        // Position cursor on the active leaf
        loop {
            if cursor.is_leaf() {
                if index == active_index {
                    // Found it
                    break;
                }
                index += 1;
            }
            match cursor.preorder_next() {
                Ok(c) => cursor = c,
                Err(c) => {
                    // Didn't find it
                    self.pane.replace(c.tree());
                    return callbacks;
                }
            }
        }

        // We are on the active leaf.
        // Now we go up until we find the parent node that is
        // aligned with the desired direction.
        let split_direction = match direction {
            PaneDirection::Left | PaneDirection::Right => SplitDirection::Horizontal,
            PaneDirection::Up | PaneDirection::Down => SplitDirection::Vertical,
            PaneDirection::Next | PaneDirection::Prev => unreachable!(),
        };
        let delta = resize_delta_for_direction(direction, amount);
        loop {
            match cursor.go_up() {
                Ok(mut c) => {
                    if let Ok(Some(node)) = c.node_mut() {
                        if node.direction == split_direction {
                            if !self.adjust_node_at_cursor(&mut c, delta) {
                                self.pane.replace(c.tree());
                                return callbacks;
                            }
                            self.cascade_size_from_cursor(c, &mut callbacks.resize_work);
                            callbacks.changed = true;
                            return callbacks;
                        }
                    }

                    cursor = c;
                }

                Err(c) => {
                    self.pane.replace(c.tree());
                    return callbacks;
                }
            }
        }
    }

    fn get_pane_direction(&mut self, direction: PaneDirection, ignore_zoom: bool) -> Option<usize> {
        let panes = if ignore_zoom {
            self.iter_panes_ignoring_zoom()
        } else {
            self.iter_panes()
        };

        let active = match panes.iter().find(|pane| pane.is_active) {
            Some(p) => p,
            None => {
                // No active pane somehow...
                return Some(0);
            }
        };

        if matches!(direction, PaneDirection::Next | PaneDirection::Prev) {
            let max_pane_id = panes.iter().map(|p| p.index).max().unwrap_or(active.index);

            return Some(if direction == PaneDirection::Next {
                if active.index == max_pane_id {
                    0
                } else {
                    active.index + 1
                }
            } else {
                if active.index == 0 {
                    max_pane_id
                } else {
                    active.index - 1
                }
            });
        }

        let mut best = None;

        let recency = &self.recency;

        fn edge_intersects(
            active_start: usize,
            active_size: usize,
            current_start: usize,
            current_size: usize,
        ) -> bool {
            intersects_range(
                &(active_start..active_start.saturating_add(active_size)),
                &(current_start..current_start.saturating_add(current_size)),
            )
        }

        for pane in &panes {
            let score = match direction {
                PaneDirection::Right => {
                    if checked_split_separator_sum(active.left, active.width) == Some(pane.left)
                        && edge_intersects(active.top, active.height, pane.top, pane.height)
                    {
                        recency.score(pane.index).saturating_add(1)
                    } else {
                        0
                    }
                }
                PaneDirection::Left => {
                    if checked_split_separator_sum(pane.left, pane.width) == Some(active.left)
                        && edge_intersects(active.top, active.height, pane.top, pane.height)
                    {
                        recency.score(pane.index).saturating_add(1)
                    } else {
                        0
                    }
                }
                PaneDirection::Up => {
                    if checked_split_separator_sum(pane.top, pane.height) == Some(active.top)
                        && edge_intersects(active.left, active.width, pane.left, pane.width)
                    {
                        recency.score(pane.index).saturating_add(1)
                    } else {
                        0
                    }
                }
                PaneDirection::Down => {
                    if checked_split_separator_sum(active.top, active.height) == Some(pane.top)
                        && edge_intersects(active.left, active.width, pane.left, pane.width)
                    {
                        recency.score(pane.index).saturating_add(1)
                    } else {
                        0
                    }
                }
                PaneDirection::Next | PaneDirection::Prev => unreachable!(),
            };

            if score > 0 {
                let target = match best.take() {
                    Some((best_score, best_pane)) if best_score > score => (best_score, best_pane),
                    _ => (score, pane),
                };
                best.replace(target);
            }
        }

        if let Some((_, target)) = best.take() {
            return Some(target.index);
        }
        None
    }

    fn get_active_pane(&mut self) -> Option<Arc<dyn Pane>> {
        if let Some(zoomed) = self.zoomed.as_ref() {
            return Some(Arc::clone(zoomed));
        }
        if let Some(focused) = self.focused_floating_pane() {
            return Some(focused);
        }

        self.iter_panes_ignoring_zoom()
            .iter()
            .nth(self.active)
            .map(|p| Arc::clone(&p.pane))
    }

    fn get_active_idx(&self) -> usize {
        self.active
    }

    fn prepare_set_active_pane(
        &mut self,
        pane: &Arc<dyn Pane>,
        pane_id: PaneId,
    ) -> (bool, DeferredTabCallbacks) {
        let prior = self.raw_active_pane_retained_id();

        if is_pane(pane, &prior.as_ref()) {
            return (true, DeferredTabCallbacks::default());
        }

        let mut callbacks = DeferredTabCallbacks::default();
        if self.zoomed.is_some() {
            if !configuration().unzoom_on_switch_pane {
                return (false, callbacks);
            }
            callbacks = self.prepare_toggle_zoom();
        }

        if let Some(index) = self
            .floating_panes
            .iter()
            .position(|floating| Arc::ptr_eq(&floating.pane, pane))
        {
            if !self.floating_panes[index].visible {
                return (false, callbacks);
            }
            let next_z = self.next_floating_z_order();
            self.floating_focus = Some(pane_id);
            self.floating_panes[index].z_order = next_z;
            callbacks.changed = true;
            callbacks.prior_focus = prior;
            callbacks.current_focus = Some(Arc::clone(pane));
            callbacks.current_focus_id = Some(pane_id);
            return (true, callbacks);
        }

        if let Some(item) = self
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|positioned| Arc::ptr_eq(&positioned.pane, pane))
        {
            self.active = item.index;
            self.recency.tag(item.index);
            self.clear_floating_focus();
            callbacks.changed = true;
            callbacks.prior_focus = prior;
            callbacks.current_focus = Some(Arc::clone(pane));
            callbacks.current_focus_id = Some(pane_id);
            return (true, callbacks);
        }
        (false, callbacks)
    }

    fn advise_focus_change(&mut self, prior: Option<Arc<dyn Pane>>) {
        let mux = self.notification_owner();
        self.advise_focus_change_with_mux(prior, mux.as_deref());
    }

    fn advise_focus_change_with_mux(&mut self, prior: Option<Arc<dyn Pane>>, mux: Option<&Mux>) {
        let current = self.get_active_pane();
        match (prior, current) {
            (Some(prior), Some(current)) if !Arc::ptr_eq(&prior, &current) => {
                prior.focus_changed(false);
                current.focus_changed(true);
                if let Some(mux) = mux {
                    mux.notify(MuxNotification::PaneFocused(current.pane_id()));
                }
            }
            (None, Some(current)) => {
                current.focus_changed(true);
                if let Some(mux) = mux {
                    mux.notify(MuxNotification::PaneFocused(current.pane_id()));
                }
            }
            (Some(prior), None) => {
                prior.focus_changed(false);
            }
            (Some(_), Some(_)) | (None, None) => {
                // no change
            }
        }
    }

    fn set_active_idx(&mut self, pane_index: usize) {
        let prior = self.get_active_pane();
        self.active = pane_index;
        self.recency.tag(pane_index);
        self.clear_floating_focus();
        self.advise_focus_change(prior);
    }

    fn assign_pane(&mut self, pane: &Arc<dyn Pane>) {
        let tree = self.pane.take().unwrap_or_else(Tree::new);
        match tree.cursor().assign_top(Arc::clone(pane)) {
            Ok(c) => self.pane = Some(c.tree()),
            Err(c) => {
                log::warn!("ignored root pane assignment on non-empty tab");
                self.pane = Some(c.tree());
            }
        }
    }

    fn cell_dimensions(&self) -> TerminalSize {
        cell_dimensions(&self.size)
    }

    fn swap_active_with_index(&mut self, pane_index: usize, keep_focus: bool) -> Option<()> {
        let active_idx = self.get_active_idx();
        // Validate both structural indices before taking the tree apart. The
        // old implementation performed its first swap before discovering an
        // invalid active index, which could drop the displaced pane and leave
        // a partially mutated tree. Resolve the exact tree pane rather than a
        // zoomed/floating focus overlay; this operation is explicitly about
        // swapping tree positions.
        let mut leaves = Vec::new();
        collect_raw_tree_leaves(self.pane.as_ref()?, &mut leaves);
        let mut pane = Arc::clone(leaves.get(active_idx)?);
        if pane_index >= leaves.len() {
            return None;
        }
        log::trace!(
            "swap_active_with_index: pane_index {} active {}",
            pane_index,
            active_idx
        );

        {
            let mut cursor = self.pane.take().unwrap().cursor();

            // locate the requested index
            match cursor.go_to_nth_leaf(pane_index) {
                Ok(c) => cursor = c,
                Err(c) => {
                    log::trace!("didn't find pane {pane_index}");
                    self.pane.replace(c.tree());
                    return None;
                }
            };

            std::mem::swap(&mut pane, cursor.leaf_mut().unwrap());

            // re-position to the root
            cursor = cursor.tree().cursor();

            // and now go and update the active idx
            match cursor.go_to_nth_leaf(active_idx) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    log::trace!("didn't find active {active_idx}");
                    return None;
                }
            };

            std::mem::swap(&mut pane, cursor.leaf_mut().unwrap());
            self.pane.replace(cursor.tree());

            // Advise the panes of their new sizes
            let size = self.size;
            apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size);
        }
        self.reindex_pane_stacks_from_tree();

        // And update focus
        if keep_focus {
            self.set_active_idx(pane_index);
        } else {
            self.advise_focus_change(Some(pane));
        }
        Some(())
    }

    fn compute_split_size(
        &mut self,
        pane_index: usize,
        request: SplitRequest,
    ) -> Option<SplitDirectionAndSize> {
        let cell_dims = self.cell_dimensions();
        let default_new_constraints = PaneConstraints::default();
        let default_width_constraints =
            axis_constraints_from_pane_constraints(default_new_constraints, Axis::Width, None);
        let default_height_constraints =
            axis_constraints_from_pane_constraints(default_new_constraints, Axis::Height, None);

        if request.top_level {
            let size = self.size;
            let tree_width_constraints = compute_axis_constraints(
                self.pane.as_ref().unwrap_or(&Tree::Empty),
                Axis::Width,
                &self.constraint_overrides,
            );
            let tree_height_constraints = compute_axis_constraints(
                self.pane.as_ref().unwrap_or(&Tree::Empty),
                Axis::Height,
                &self.constraint_overrides,
            );

            let ((width1, width2), (height1, height2)) = match request.direction {
                SplitDirection::Horizontal => {
                    let first_constraints = if request.target_is_second {
                        tree_width_constraints
                    } else {
                        default_width_constraints
                    };
                    let second_constraints = if request.target_is_second {
                        default_width_constraints
                    } else {
                        tree_width_constraints
                    };
                    let widths = split_dimension_for_request(
                        size.cols as usize,
                        request,
                        first_constraints,
                        second_constraints,
                    )?;
                    (widths, (size.rows as usize, size.rows as usize))
                }
                SplitDirection::Vertical => {
                    let first_constraints = if request.target_is_second {
                        tree_height_constraints
                    } else {
                        default_height_constraints
                    };
                    let second_constraints = if request.target_is_second {
                        default_height_constraints
                    } else {
                        tree_height_constraints
                    };
                    let heights = split_dimension_for_request(
                        size.rows as usize,
                        request,
                        first_constraints,
                        second_constraints,
                    )?;
                    ((size.cols as usize, size.cols as usize), heights)
                }
            };

            return Some(SplitDirectionAndSize {
                direction: request.direction,
                first: TerminalSize {
                    rows: height1 as _,
                    cols: width1 as _,
                    pixel_height: pixel_span(cell_dims.pixel_height, height1),
                    pixel_width: pixel_span(cell_dims.pixel_width, width1),
                    dpi: cell_dims.dpi,
                },
                second: TerminalSize {
                    rows: height2 as _,
                    cols: width2 as _,
                    pixel_height: pixel_span(cell_dims.pixel_height, height2),
                    pixel_width: pixel_span(cell_dims.pixel_width, width2),
                    dpi: cell_dims.dpi,
                },
            });
        }

        debug_assert!(
            self.zoomed.is_none(),
            "non-top-level split sizing must unzoom at the outer Tab boundary"
        );

        self.iter_panes().iter().nth(pane_index).and_then(|pos| {
            let existing_constraints =
                effective_pane_constraints(&pos.pane, &self.constraint_overrides);
            let existing_width_constraints = axis_constraints_from_pane_constraints(
                existing_constraints,
                Axis::Width,
                Some(pos.width),
            );
            let existing_height_constraints = axis_constraints_from_pane_constraints(
                existing_constraints,
                Axis::Height,
                Some(pos.height),
            );
            let ((width1, width2), (height1, height2)) = match request.direction {
                SplitDirection::Horizontal => {
                    let first_constraints = if request.target_is_second {
                        existing_width_constraints
                    } else {
                        default_width_constraints
                    };
                    let second_constraints = if request.target_is_second {
                        default_width_constraints
                    } else {
                        existing_width_constraints
                    };
                    let widths = split_dimension_for_request(
                        pos.width,
                        request,
                        first_constraints,
                        second_constraints,
                    )?;
                    (widths, (pos.height, pos.height))
                }
                SplitDirection::Vertical => {
                    let first_constraints = if request.target_is_second {
                        existing_height_constraints
                    } else {
                        default_height_constraints
                    };
                    let second_constraints = if request.target_is_second {
                        default_height_constraints
                    } else {
                        existing_height_constraints
                    };
                    let heights = split_dimension_for_request(
                        pos.height,
                        request,
                        first_constraints,
                        second_constraints,
                    )?;
                    ((pos.width, pos.width), heights)
                }
            };

            Some(SplitDirectionAndSize {
                direction: request.direction,
                first: TerminalSize {
                    rows: height1 as _,
                    cols: width1 as _,
                    pixel_height: pixel_span(cell_dims.pixel_height, height1),
                    pixel_width: pixel_span(cell_dims.pixel_width, width1),
                    dpi: cell_dims.dpi,
                },
                second: TerminalSize {
                    rows: height2 as _,
                    cols: width2 as _,
                    pixel_height: pixel_span(cell_dims.pixel_height, height2),
                    pixel_width: pixel_span(cell_dims.pixel_width, width2),
                    dpi: cell_dims.dpi,
                },
            })
        })
    }

    fn split_and_insert(
        &mut self,
        pane_index: usize,
        request: SplitRequest,
        pane: Arc<dyn Pane>,
    ) -> anyhow::Result<usize> {
        let mut panes = self.snapshot_panes_callback_free();
        panes.push(Arc::clone(&pane));
        let observed = Tab::observe_panes(panes);
        let pane_ids = build_callback_pane_id_snapshot(self.id, &observed)?;
        let mux = self.notification_owner();
        let (inserted, mut callbacks) =
            self.prepare_split_and_insert(pane_index, request, pane, &pane_ids)?;
        if let Some(mux) = mux.as_deref() {
            callbacks.reserve_topology_notifications(mux, self.id);
        }
        callbacks.execute(mux.as_deref());
        Ok(inserted)
    }

    /// Prepare a complete split successor without invoking resize, focus, or
    /// notification callbacks. Callers may run the fallible geometry work on a
    /// detached [`TabInner`] clone, then install that successor in one exact
    /// authority cut and execute the returned work after releasing all locks.
    fn prepare_split_and_insert(
        &mut self,
        pane_index: usize,
        request: SplitRequest,
        pane: Arc<dyn Pane>,
        pane_ids: &HashMap<PaneIdentity, PaneId>,
    ) -> anyhow::Result<(usize, DeferredTabCallbacks)> {
        if self.zoomed.is_some() {
            anyhow::bail!("cannot split while zoomed");
        }

        let mut callbacks = DeferredTabCallbacks {
            prior_focus: self.raw_active_pane_callback_free(pane_ids),
            ..DeferredTabCallbacks::default()
        };

        {
            let split_info = self
                .compute_split_size(pane_index, request)
                .ok_or_else(|| {
                    anyhow::anyhow!("invalid pane_index {}; cannot split!", pane_index)
                })?;

            let tab_size = self.size;
            if split_info.first.rows == 0
                || split_info.first.cols == 0
                || split_info.second.rows == 0
                || split_info.second.cols == 0
                || split_info.top_of_second() + split_info.second.rows > tab_size.rows
                || split_info.left_of_second() + split_info.second.cols > tab_size.cols
            {
                log::error!(
                    "No space for split!!! {:#?} height={} width={} top_of_second={} left_of_second={} tab_size={:?}",
                    split_info,
                    split_info.height(),
                    split_info.width(),
                    split_info.top_of_second(),
                    split_info.left_of_second(),
                    tab_size
                );
                anyhow::bail!("No space for split!");
            }

            if request.top_level && self.pane.as_ref().unwrap().num_leaves() > 0 {
                let existing_width_constraints = compute_axis_constraints(
                    self.pane.as_ref().unwrap(),
                    Axis::Width,
                    &self.constraint_overrides,
                );
                let existing_height_constraints = compute_axis_constraints(
                    self.pane.as_ref().unwrap(),
                    Axis::Height,
                    &self.constraint_overrides,
                );
                let new_width_constraints =
                    pane_axis_constraints(&pane, Axis::Width, &self.constraint_overrides);
                let new_height_constraints =
                    pane_axis_constraints(&pane, Axis::Height, &self.constraint_overrides);

                let (existing_size, new_size) = if request.target_is_second {
                    (split_info.first, split_info.second)
                } else {
                    (split_info.second, split_info.first)
                };
                let requested_new_axis = requested_split_target_axis_size(
                    match request.direction {
                        SplitDirection::Horizontal => tab_size.cols,
                        SplitDirection::Vertical => tab_size.rows,
                    },
                    request,
                );
                let actual_new_axis = match request.direction {
                    SplitDirection::Horizontal => new_size.cols,
                    SplitDirection::Vertical => new_size.rows,
                };

                if actual_new_axis < requested_new_axis {
                    anyhow::bail!(
                        "No space for top-level split request: requested={} actual={} existing={:?} new={:?}",
                        requested_new_axis,
                        actual_new_axis,
                        existing_size,
                        new_size
                    );
                }

                if !pane_size_satisfies_constraints(
                    &existing_size,
                    existing_width_constraints,
                    existing_height_constraints,
                ) || !pane_size_satisfies_constraints(
                    &new_size,
                    new_width_constraints,
                    new_height_constraints,
                ) {
                    anyhow::bail!(
                        "No space for top-level split constraints: existing={:?} new={:?}",
                        existing_size,
                        new_size
                    );
                }
            }

            let needs_resize = if request.top_level {
                self.pane.as_ref().unwrap().num_leaves() > 1
            } else {
                false
            };

            if needs_resize {
                // Pre-emptively resize the tab contents down to
                // match the target size; it's easier to reuse
                // existing resize logic that way
                let mut resize_callbacks = if request.target_is_second {
                    self.prepare_resize(split_info.first)
                } else {
                    self.prepare_resize(split_info.second)
                };
                callbacks
                    .resize_work
                    .append(&mut resize_callbacks.resize_work);
                callbacks
                    .zoom_work
                    .append(&mut resize_callbacks.zoom_work);
            }

            let mut cursor = self.pane.take().unwrap().cursor();

            if request.top_level && !cursor.is_leaf() {
                let result = if request.target_is_second {
                    cursor.split_node_and_insert_right(Arc::clone(&pane))
                } else {
                    cursor.split_node_and_insert_left(Arc::clone(&pane))
                };
                cursor = match result {
                    Ok(c) => {
                        cursor = match c.assign_node(Some(split_info)) {
                            Err(c) | Ok(c) => c,
                        };

                        self.pane.replace(cursor.tree());

                        let pane_index = if request.target_is_second {
                            self.pane.as_ref().unwrap().num_leaves().saturating_sub(1)
                        } else {
                            0
                        };

                        self.active = pane_index;
                        self.recency.tag(pane_index);
                        self.reindex_pane_stacks_from_tree();
                        // `prepare_resize` temporarily adopts the existing
                        // subtree's share so it can reuse the ordinary layout
                        // machinery. The newly installed top-level split once
                        // again owns the complete tab geometry.
                        self.size = tab_size;
                        callbacks.resize_work.clear();
                        collect_pane_resize_work(
                            self.pane
                                .as_ref()
                                .expect("top-level split installed its final pane tree"),
                            &self.size,
                            &mut callbacks.resize_work,
                        );
                        callbacks.current_focus =
                            self.raw_active_pane_callback_free(pane_ids);
                        callbacks.current_focus_id = callbacks
                            .current_focus
                            .as_ref()
                            .and_then(|pane| pane_ids.get(&pane_identity(pane)).copied());
                        callbacks.changed = true;
                        return Ok((pane_index, callbacks));
                    }
                    Err(cursor) => cursor,
                };
            }

            match cursor.go_to_nth_leaf(pane_index) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    anyhow::bail!("invalid pane_index {}; cannot split!", pane_index);
                }
            };

            let existing_pane = Arc::clone(cursor.leaf_mut().unwrap());

            let (pane1, pane2) = if request.target_is_second {
                (existing_pane, pane)
            } else {
                (pane, existing_pane)
            };

            let pane1_width_constraints =
                pane_axis_constraints(&pane1, Axis::Width, &self.constraint_overrides);
            let pane1_height_constraints =
                pane_axis_constraints(&pane1, Axis::Height, &self.constraint_overrides);
            let pane2_width_constraints =
                pane_axis_constraints(&pane2, Axis::Width, &self.constraint_overrides);
            let pane2_height_constraints =
                pane_axis_constraints(&pane2, Axis::Height, &self.constraint_overrides);
            if !pane_size_satisfies_constraints(
                &split_info.first,
                pane1_width_constraints,
                pane1_height_constraints,
            ) || !pane_size_satisfies_constraints(
                &split_info.second,
                pane2_width_constraints,
                pane2_height_constraints,
            ) {
                anyhow::bail!(
                    "No space for split constraints: first={:?} second={:?}",
                    split_info.first,
                    split_info.second
                );
            }

            callbacks.resize_work.push((Arc::clone(&pane1), split_info.first));
            callbacks
                .resize_work
                .push((Arc::clone(&pane2), split_info.second));

            *cursor.leaf_mut().unwrap() = pane1;

            match cursor.split_leaf_and_insert_right(pane2) {
                Ok(c) => cursor = c,
                Err(c) => {
                    self.pane.replace(c.tree());
                    anyhow::bail!("invalid pane_index {}; cannot split!", pane_index);
                }
            };

            // cursor now points to the newly created split node;
            // we need to populate its split information
            match cursor.assign_node(Some(split_info)) {
                Err(c) | Ok(c) => self.pane.replace(c.tree()),
            };

            if request.target_is_second {
                let new_pane_index = next_pane_index(pane_index);
                self.active = new_pane_index;
                self.recency.tag(new_pane_index);
            }
        }

        self.reindex_pane_stacks_from_tree();
        log::debug!("split info after split: {:#?}", self.iter_splits());
        log::debug!("pane info after split: {:#?}", self.iter_panes());

        let inserted = if request.target_is_second {
            next_pane_index(pane_index)
        } else {
            pane_index
        };
        callbacks.current_focus = self.raw_active_pane_callback_free(pane_ids);
        callbacks.current_focus_id = callbacks
            .current_focus
            .as_ref()
            .and_then(|pane| pane_ids.get(&pane_identity(pane)).copied());
        callbacks.changed = true;
        Ok((inserted, callbacks))
    }

    fn get_zoomed_pane(&self) -> Option<Arc<dyn Pane>> {
        self.zoomed.clone()
    }
}

/// This type is used directly by the codec, take care to bump
/// the codec version if you change this
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub enum PaneNode {
    Empty,
    Split {
        left: Box<PaneNode>,
        right: Box<PaneNode>,
        node: SplitDirectionAndSize,
    },
    Leaf(PaneEntry),
}

impl PaneNode {
    pub fn into_tree(self) -> bintree::Tree<PaneEntry, SplitDirectionAndSize> {
        match self {
            PaneNode::Empty => bintree::Tree::Empty,
            PaneNode::Split { left, right, node } => bintree::Tree::Node {
                left: Box::new((*left).into_tree()),
                right: Box::new((*right).into_tree()),
                data: Some(node),
            },
            PaneNode::Leaf(e) => bintree::Tree::Leaf(e),
        }
    }

    pub fn root_size(&self) -> Option<TerminalSize> {
        match self {
            PaneNode::Empty => None,
            PaneNode::Split { node, .. } => Some(node.size()),
            PaneNode::Leaf(entry) => Some(entry.size),
        }
    }

    pub fn window_and_tab_ids(&self) -> Option<(WindowId, TabId)> {
        match self {
            PaneNode::Empty => None,
            PaneNode::Split { left, right, .. } => match left.window_and_tab_ids() {
                Some(res) => Some(res),
                None => right.window_and_tab_ids(),
            },
            PaneNode::Leaf(entry) => Some((entry.window_id, entry.tab_id)),
        }
    }

    /// Return whether every leaf carries one expected window/tab identity.
    ///
    /// `None` means that the tree contains no leaves. Unlike
    /// [`Self::window_and_tab_ids`], this is an adversarial validator rather
    /// than a representative-identity accessor: a mismatched second leaf can
    /// never be hidden behind a valid first leaf.
    pub fn all_window_and_tab_ids_match(&self, expected: (WindowId, TabId)) -> Option<bool> {
        match self {
            Self::Empty => None,
            Self::Leaf(entry) => Some((entry.window_id, entry.tab_id) == expected),
            Self::Split { left, right, .. } => {
                match (
                    left.all_window_and_tab_ids_match(expected),
                    right.all_window_and_tab_ids_match(expected),
                ) {
                    (None, None) => None,
                    (Some(matches), None) | (None, Some(matches)) => Some(matches),
                    (Some(left_matches), Some(right_matches)) => {
                        Some(left_matches && right_matches)
                    }
                }
            }
        }
    }
}

/// This type is used directly by the codec, take care to bump
/// the codec version if you change this
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct PaneEntry {
    pub window_id: WindowId,
    pub tab_id: TabId,
    pub pane_id: PaneId,
    pub title: String,
    pub size: TerminalSize,
    pub working_dir: Option<SerdeUrl>,
    #[serde(default)]
    pub alt_screen_active: bool,
    pub is_active_pane: bool,
    pub is_zoomed_pane: bool,
    pub workspace: String,
    pub cursor_pos: StableCursorPosition,
    pub physical_top: StableRowIndex,
    pub top_row: usize,
    pub left_col: usize,
    pub tty_name: Option<String>,
}

#[derive(Deserialize, Clone, PartialEq, Debug)]
#[serde(try_from = "String")]
pub struct SerdeUrl {
    value: String,
}

impl SerdeUrl {
    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub fn capacity(&self) -> usize {
        self.value.capacity()
    }
}

impl std::convert::TryFrom<String> for SerdeUrl {
    type Error = url::ParseError;
    fn try_from(s: String) -> Result<SerdeUrl, url::ParseError> {
        let url = Url::parse(&s)?;
        if url.as_str() == s.as_str() {
            Ok(SerdeUrl { value: s })
        } else {
            Ok(SerdeUrl { value: url.into() })
        }
    }
}

impl From<Url> for SerdeUrl {
    fn from(url: Url) -> SerdeUrl {
        SerdeUrl { value: url.into() }
    }
}

impl From<SerdeUrl> for Url {
    fn from(value: SerdeUrl) -> Self {
        Url::parse(&value.value).expect("SerdeUrl stores a previously validated canonical URL")
    }
}

impl From<SerdeUrl> for String {
    fn from(value: SerdeUrl) -> Self {
        value.value
    }
}

impl Serialize for SerdeUrl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.value)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::renderable::*;
    use frankenterm_term::color::ColorPalette;
    use frankenterm_term::{KeyCode, KeyModifiers, Line, MouseEvent, StableRowIndex};
    use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
    use proptest::prelude::*;
    use rangeset::RangeSet;
    use std::convert::TryFrom;
    use std::ops::Range;
    use termwiz::surface::{SequenceNo, SEQ_ZERO};
    use url::Url;

    const TEST_ORDERED_PANE_CENSUS_WORK: usize = 32_767;

    /// Ensure the global Mux singleton is initialized for tests that trigger
    /// focus-change notifications (e.g. floating pane and top-level split tests).
    fn ensure_mux_initialized() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if Mux::try_get().is_none() {
            let mux = Arc::new(Mux::new(None));
            Mux::set_mux(&mux);
        }
    }

    #[test]
    fn tab_stack_state_cycles_visible_tab_with_wraparound() {
        let mut state = TabStackState::default();
        state
            .create_stack(TabStackId(7), vec![10, 20, 30])
            .expect("create tab stack");

        assert_eq!(state.visible_tab(TabStackId(7)), Some(10));
        assert_eq!(state.cycle_visible(TabStackId(7), 1), Some(20));
        assert_eq!(state.cycle_visible(TabStackId(7), 1), Some(30));
        assert_eq!(state.cycle_visible(TabStackId(7), 1), Some(10));
        assert_eq!(state.cycle_visible(TabStackId(7), -1), Some(30));
    }

    #[test]
    fn tab_stack_state_cycles_extreme_deltas_without_overflow() {
        let mut state = TabStackState::default();
        state
            .create_stack(TabStackId(7), vec![10, 20, 30])
            .expect("create tab stack");

        assert_eq!(state.cycle_visible(TabStackId(7), isize::MAX), Some(20));
        assert_eq!(state.cycle_visible(TabStackId(7), isize::MIN), Some(30));

        let mut single = TabStackState::default();
        single
            .create_stack(TabStackId(1), vec![99])
            .expect("create single-tab stack");
        assert_eq!(single.cycle_visible(TabStackId(1), isize::MIN), Some(99));
    }

    #[test]
    fn recency_counter_saturates_at_usize_max() {
        let mut recency = Recency {
            count: usize::MAX,
            by_idx: HashMap::new(),
        };

        recency.tag(7);

        assert_eq!(recency.count, usize::MAX);
        assert_eq!(recency.score(7), usize::MAX);
    }

    #[test]
    fn split_budget_counter_saturates_at_usize_max() {
        let mut counter = usize::MAX;

        advance_split_budget_counter(&mut counter);

        assert_eq!(counter, usize::MAX);
    }

    #[test]
    fn resize_delta_helpers_handle_extreme_inputs() {
        assert_eq!(offset_by_resize_delta(5, -3), 2);
        assert_eq!(offset_by_resize_delta(5, 3), 8);
        assert_eq!(offset_by_resize_delta(5, isize::MIN), 0);
        assert_eq!(pixel_span(8, 10), 80);
        assert_eq!(pixel_span(usize::MAX, 2), usize::MAX);
        assert_eq!(split_separator_offset(4), 5);
        assert_eq!(split_separator_offset(usize::MAX), usize::MAX);
        assert_eq!(next_pane_index(4), 5);
        assert_eq!(next_pane_index(usize::MAX), usize::MAX);
        assert_eq!(split_separator_sum(2, 3), 6);
        assert_eq!(split_separator_sum(usize::MAX, 3), usize::MAX);
        assert_eq!(split_separator_sum(usize::MAX - 1, 1), usize::MAX);
        assert_eq!(checked_split_separator_sum(2, 3), Some(6));
        assert_eq!(
            checked_split_separator_sum(usize::MAX - 1, 0),
            Some(usize::MAX)
        );
        assert_eq!(checked_split_separator_sum(usize::MAX, 0), None);
        assert_eq!(usize_to_isize_saturating(42), 42);
        assert_eq!(usize_to_isize_saturating(usize::MAX), isize::MAX);
        assert_eq!(positive_resize_budget(usize::MAX), isize::MAX);
        assert_eq!(negative_resize_budget(0), 0);
        assert_eq!(negative_resize_budget(isize::MAX as usize + 1), isize::MIN);
        assert_eq!(resize_delta_between(8, 5), 3);
        assert_eq!(resize_delta_between(5, 8), -3);
        assert_eq!(resize_delta_between(usize::MAX, 0), isize::MAX);
        assert_eq!(resize_delta_between(0, usize::MAX), isize::MIN);
        assert_eq!(
            resize_delta_for_direction(PaneDirection::Left, isize::MAX as usize + 1),
            isize::MIN
        );
        assert_eq!(
            resize_delta_for_direction(PaneDirection::Right, usize::MAX),
            isize::MAX
        );
    }

    #[test]
    fn tab_stack_state_rejects_duplicate_or_already_stacked_tabs() {
        let mut state = TabStackState::default();

        assert_eq!(
            state.create_stack(TabStackId(1), vec![1, 1]),
            Err(TabStackError::DuplicateTab(1))
        );

        state
            .create_stack(TabStackId(1), vec![1, 2])
            .expect("create first stack");
        assert_eq!(
            state.create_stack(TabStackId(2), vec![2, 3]),
            Err(TabStackError::TabAlreadyStacked {
                tab_id: 2,
                stack_id: TabStackId(1),
            })
        );
    }

    #[test]
    fn tab_stack_state_moves_tab_between_stacks_and_preserves_visible_tab() {
        let mut state = TabStackState::default();
        state
            .create_stack(TabStackId(1), vec![1, 2, 3])
            .expect("create source stack");
        state
            .create_stack(TabStackId(2), vec![10, 20])
            .expect("create destination stack");

        assert_eq!(state.cycle_visible(TabStackId(1), 2), Some(3));
        state
            .move_tab_to_stack(3, TabStackId(2), 1)
            .expect("move tab to destination stack");

        assert_eq!(state.stack_for_tab(3), Some(TabStackId(2)));
        assert_eq!(state.tabs_in_stack(TabStackId(1)), Some(&[1, 2][..]));
        assert_eq!(state.tabs_in_stack(TabStackId(2)), Some(&[10, 3, 20][..]));
        assert_eq!(
            state.visible_tab(TabStackId(1)),
            Some(2),
            "source visible index should clamp after removing the visible tab"
        );
        assert_eq!(
            state.visible_tab(TabStackId(2)),
            Some(10),
            "inserting after the visible tab should not change the visible tab"
        );
    }

    #[test]
    fn tab_stack_state_overview_entries_are_stable_and_mark_visible_tab() {
        let mut state = TabStackState::default();
        state
            .create_stack(TabStackId(2), vec![20, 21])
            .expect("create second stack");
        state
            .create_stack(TabStackId(1), vec![10, 11])
            .expect("create first stack");
        assert_eq!(state.cycle_visible(TabStackId(2), 1), Some(21));

        let entries = state.overview_entries();
        assert_eq!(
            entries,
            vec![
                TabStackEntry {
                    stack_id: TabStackId(1),
                    tab_id: 10,
                    position: 0,
                    is_visible: true,
                },
                TabStackEntry {
                    stack_id: TabStackId(1),
                    tab_id: 11,
                    position: 1,
                    is_visible: false,
                },
                TabStackEntry {
                    stack_id: TabStackId(2),
                    tab_id: 20,
                    position: 0,
                    is_visible: false,
                },
                TabStackEntry {
                    stack_id: TabStackId(2),
                    tab_id: 21,
                    position: 1,
                    is_visible: true,
                },
            ]
        );
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum OrderedObservationCallback {
        PaneId,
        Dimensions,
        WorkingDirectory,
        CursorPosition,
        Title,
        AltScreen,
        TtyName,
    }

    struct FakePane {
        id: PaneId,
        size: Mutex<TerminalSize>,
        domain_id: DomainId,
        constraints: PaneConstraints,
        priority: CollapsePriority,
        writes: Mutex<Vec<u8>>,
        mux_registration: Arc<crate::PaneRegistrationSlot>,
        dead: bool,
        panic_in_is_dead: bool,
        panic_in_ordered_observation: Option<(
            OrderedObservationCallback,
            Arc<std::sync::atomic::AtomicBool>,
        )>,
        callback_probe: Option<Arc<dyn Fn() + Send + Sync>>,
        pane_id_probe: Option<Arc<dyn Fn() + Send + Sync>>,
        title_override: Option<String>,
        working_dir_override: Option<Url>,
        drop_probe: Option<Arc<dyn Fn() + Send + Sync>>,
        kills: std::sync::atomic::AtomicUsize,
    }

    impl FakePane {
        fn new(id: PaneId, size: TerminalSize) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                domain_id: 1,
                constraints: PaneConstraints::default(),
                priority: CollapsePriority::default(),
                writes: Mutex::new(Vec::new()),
                mux_registration: Arc::new(crate::PaneRegistrationSlot::default()),
                dead: false,
                panic_in_is_dead: false,
                panic_in_ordered_observation: None,
                callback_probe: None,
                pane_id_probe: None,
                title_override: None,
                working_dir_override: None,
                drop_probe: None,
                kills: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn new_with_drop_probe(
            id: PaneId,
            size: TerminalSize,
            drop_probe: Arc<dyn Fn() + Send + Sync>,
        ) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                domain_id: 1,
                constraints: PaneConstraints::default(),
                priority: CollapsePriority::default(),
                writes: Mutex::new(Vec::new()),
                mux_registration: Arc::new(crate::PaneRegistrationSlot::default()),
                dead: false,
                panic_in_is_dead: false,
                panic_in_ordered_observation: None,
                callback_probe: None,
                pane_id_probe: None,
                title_override: None,
                working_dir_override: None,
                drop_probe: Some(drop_probe),
                kills: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn new_with_domain(id: PaneId, size: TerminalSize, domain_id: DomainId) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                domain_id,
                constraints: PaneConstraints::default(),
                priority: CollapsePriority::default(),
                writes: Mutex::new(Vec::new()),
                mux_registration: Arc::new(crate::PaneRegistrationSlot::default()),
                dead: false,
                panic_in_is_dead: false,
                panic_in_ordered_observation: None,
                callback_probe: None,
                pane_id_probe: None,
                title_override: None,
                working_dir_override: None,
                drop_probe: None,
                kills: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn new_with_constraints(
            id: PaneId,
            size: TerminalSize,
            constraints: PaneConstraints,
        ) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                domain_id: 1,
                constraints,
                priority: CollapsePriority::default(),
                writes: Mutex::new(Vec::new()),
                mux_registration: Arc::new(crate::PaneRegistrationSlot::default()),
                dead: false,
                panic_in_is_dead: false,
                panic_in_ordered_observation: None,
                callback_probe: None,
                pane_id_probe: None,
                title_override: None,
                working_dir_override: None,
                drop_probe: None,
                kills: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn new_with_priority(
            id: PaneId,
            size: TerminalSize,
            constraints: PaneConstraints,
            priority: CollapsePriority,
        ) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                domain_id: 1,
                constraints,
                priority,
                writes: Mutex::new(Vec::new()),
                mux_registration: Arc::new(crate::PaneRegistrationSlot::default()),
                dead: false,
                panic_in_is_dead: false,
                panic_in_ordered_observation: None,
                callback_probe: None,
                pane_id_probe: None,
                title_override: None,
                working_dir_override: None,
                drop_probe: None,
                kills: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn new_with_callback_probe(
            id: PaneId,
            size: TerminalSize,
            dead: bool,
            panic_in_is_dead: bool,
            callback_probe: Arc<dyn Fn() + Send + Sync>,
        ) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                domain_id: 1,
                constraints: PaneConstraints::default(),
                priority: CollapsePriority::default(),
                writes: Mutex::new(Vec::new()),
                mux_registration: Arc::new(crate::PaneRegistrationSlot::default()),
                dead,
                panic_in_is_dead,
                panic_in_ordered_observation: None,
                callback_probe: Some(callback_probe),
                pane_id_probe: None,
                title_override: None,
                working_dir_override: None,
                drop_probe: None,
                kills: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn new_with_pane_id_probe(
            id: PaneId,
            size: TerminalSize,
            pane_id_probe: Arc<dyn Fn() + Send + Sync>,
        ) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                domain_id: 1,
                constraints: PaneConstraints::default(),
                priority: CollapsePriority::default(),
                writes: Mutex::new(Vec::new()),
                mux_registration: Arc::new(crate::PaneRegistrationSlot::default()),
                dead: false,
                panic_in_is_dead: false,
                panic_in_ordered_observation: None,
                callback_probe: None,
                pane_id_probe: Some(pane_id_probe),
                title_override: None,
                working_dir_override: None,
                drop_probe: None,
                kills: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn new_with_ordered_observation_panic(
            id: PaneId,
            size: TerminalSize,
            callback: OrderedObservationCallback,
            armed: Arc<std::sync::atomic::AtomicBool>,
        ) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                domain_id: 1,
                constraints: PaneConstraints::default(),
                priority: CollapsePriority::default(),
                writes: Mutex::new(Vec::new()),
                mux_registration: Arc::new(crate::PaneRegistrationSlot::default()),
                dead: false,
                panic_in_is_dead: false,
                panic_in_ordered_observation: Some((callback, armed)),
                callback_probe: None,
                pane_id_probe: None,
                title_override: None,
                working_dir_override: None,
                drop_probe: None,
                kills: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn new_with_title_and_later_callback_probe(
            id: PaneId,
            size: TerminalSize,
            title: String,
            callback_probe: Arc<dyn Fn() + Send + Sync>,
        ) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                domain_id: 1,
                constraints: PaneConstraints::default(),
                priority: CollapsePriority::default(),
                writes: Mutex::new(Vec::new()),
                mux_registration: Arc::new(crate::PaneRegistrationSlot::default()),
                dead: false,
                panic_in_is_dead: false,
                panic_in_ordered_observation: None,
                callback_probe: Some(callback_probe),
                pane_id_probe: None,
                title_override: Some(title),
                working_dir_override: None,
                drop_probe: None,
                kills: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn new_with_working_dir_and_later_callback_panic(
            id: PaneId,
            size: TerminalSize,
            working_dir: Url,
            armed: Arc<std::sync::atomic::AtomicBool>,
        ) -> Arc<dyn Pane> {
            Arc::new(Self {
                id,
                size: Mutex::new(size),
                domain_id: 1,
                constraints: PaneConstraints::default(),
                priority: CollapsePriority::default(),
                writes: Mutex::new(Vec::new()),
                mux_registration: Arc::new(crate::PaneRegistrationSlot::default()),
                dead: false,
                panic_in_is_dead: false,
                panic_in_ordered_observation: Some((OrderedObservationCallback::AltScreen, armed)),
                callback_probe: None,
                pane_id_probe: None,
                title_override: None,
                working_dir_override: Some(working_dir),
                drop_probe: None,
                kills: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn panic_if_ordered_observation_callback(&self, callback: OrderedObservationCallback) {
            if let Some((configured_callback, armed)) = &self.panic_in_ordered_observation {
                assert!(
                    *configured_callback != callback
                        || !armed.load(std::sync::atomic::Ordering::Acquire),
                    "injected ordered pane observation panic for {:?}",
                    callback
                );
            }
        }
    }

    impl Drop for FakePane {
        fn drop(&mut self) {
            if let Some(probe) = &self.drop_probe {
                probe();
            }
        }
    }

    struct FloatingReconcileTestDomain {
        domain_id: DomainId,
    }

    #[async_trait::async_trait(?Send)]
    impl Domain for FloatingReconcileTestDomain {
        async fn spawn_pane(
            &self,
            _mux: &Arc<Mux>,
            _size: TerminalSize,
            _command: Option<portable_pty::CommandBuilder>,
            _command_dir: Option<String>,
        ) -> anyhow::Result<Arc<dyn Pane>> {
            anyhow::bail!("floating reconcile test domain cannot spawn panes")
        }

        fn detachable(&self) -> bool {
            false
        }

        fn domain_id(&self) -> DomainId {
            self.domain_id
        }

        fn domain_name(&self) -> &str {
            "floating-reconcile-test"
        }

        async fn attach(
            &self,
            _mux: &Arc<Mux>,
            _owner_client_id: Option<Arc<crate::client::ClientId>>,
            _window_id: Option<WindowId>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        fn detach(&self) -> anyhow::Result<()> {
            Ok(())
        }

        fn state(&self) -> crate::domain::DomainState {
            crate::domain::DomainState::Attached
        }
    }

    fn attach_floating_reconcile_test_tab(
        mux: &Arc<Mux>,
        pane: &Arc<dyn Pane>,
        size: TerminalSize,
        window_id: WindowId,
    ) -> Arc<Tab> {
        mux.add_pane(pane)
            .expect("register tiled floating-reconcile test pane");
        let tab = Arc::new(Tab::new(&size));
        tab.assign_pane(pane);
        mux.add_tab_and_active_pane(&tab)
            .expect("register tiled floating-reconcile test tab");
        mux.add_tab_to_window(&tab, window_id)
            .expect("attach floating-reconcile test tab to its window");
        tab
    }

    fn floating_reconcile_state(
        tab: &Arc<Tab>,
        pane: &Arc<dyn Pane>,
        left: usize,
    ) -> DomainFloatingPaneState {
        DomainFloatingPaneState {
            tab: Arc::clone(tab),
            pane: Arc::clone(pane),
            pane_id: pane.pane_id(),
            rect: FloatingPaneRect {
                left,
                top: 1,
                width: 12,
                height: 6,
            },
            z_order: 1,
            visible: true,
            pinned: false,
            opacity: 1.0,
            focused: false,
        }
    }

    impl Pane for FakePane {
        fn pane_id(&self) -> PaneId {
            self.panic_if_ordered_observation_callback(OrderedObservationCallback::PaneId);
            if let Some(probe) = &self.pane_id_probe {
                probe();
            }
            self.id
        }

        fn mux_registration_slot(&self) -> &Arc<crate::PaneRegistrationSlot> {
            &self.mux_registration
        }

        fn get_cursor_position(&self) -> StableCursorPosition {
            self.panic_if_ordered_observation_callback(OrderedObservationCallback::CursorPosition);
            StableCursorPosition::default()
        }

        fn get_current_seqno(&self) -> SequenceNo {
            SEQ_ZERO
        }

        fn get_changed_since(
            &self,
            _lines: Range<StableRowIndex>,
            _: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            RangeSet::new()
        }

        fn with_lines_mut(
            &self,
            _stable_range: Range<StableRowIndex>,
            _with_lines: &mut dyn WithPaneLines,
        ) {
        }

        fn for_each_logical_line_in_stable_range_mut(
            &self,
            _lines: Range<StableRowIndex>,
            _for_line: &mut dyn ForEachPaneLogicalLine,
        ) {
        }

        fn get_lines(&self, _lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            (0, Vec::new())
        }

        fn get_logical_lines(&self, _lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
            Vec::new()
        }

        fn get_dimensions(&self) -> RenderableDimensions {
            self.panic_if_ordered_observation_callback(OrderedObservationCallback::Dimensions);
            if let Some(probe) = &self.callback_probe {
                probe();
            }
            let size = *self.size.lock();
            RenderableDimensions {
                cols: size.cols,
                viewport_rows: size.rows,
                scrollback_rows: size.rows,
                physical_top: 0,
                scrollback_top: 0,
                dpi: size.dpi,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
                reverse_video: false,
            }
        }

        fn pane_constraints(&self) -> PaneConstraints {
            self.constraints
        }

        fn collapse_priority(&self) -> CollapsePriority {
            self.priority
        }

        fn get_title(&self) -> String {
            self.panic_if_ordered_observation_callback(OrderedObservationCallback::Title);
            self.title_override
                .clone()
                .unwrap_or_else(|| format!("fake-pane-{}", self.id))
        }
        fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(None)
        }
        fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
            MutexGuard::map(self.writes.lock(), |writes| {
                let writer: &mut dyn std::io::Write = writes;
                writer
            })
        }
        fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
            if let Some(probe) = &self.callback_probe {
                probe();
            }
            *self.size.lock() = size;
            Ok(())
        }

        fn focus_changed(&self, _focused: bool) {
            if let Some(probe) = &self.callback_probe {
                probe();
            }
        }

        fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            Ok(())
        }
        fn key_up(&self, _: KeyCode, _: KeyModifiers) -> anyhow::Result<()> {
            Ok(())
        }
        fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
            Ok(())
        }
        fn is_dead(&self) -> bool {
            if let Some(probe) = &self.callback_probe {
                probe();
            }
            assert!(
                !self.panic_in_is_dead,
                "intentional FakePane::is_dead panic"
            );
            self.dead
        }
        fn kill(&self) {
            self.kills.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        fn palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
        fn domain_id(&self) -> DomainId {
            self.domain_id
        }
        fn is_mouse_grabbed(&self) -> bool {
            false
        }
        fn is_alt_screen_active(&self) -> bool {
            self.panic_if_ordered_observation_callback(OrderedObservationCallback::AltScreen);
            false
        }
        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
            self.panic_if_ordered_observation_callback(
                OrderedObservationCallback::WorkingDirectory,
            );
            self.working_dir_override.clone()
        }
        fn tty_name(&self) -> Option<String> {
            self.panic_if_ordered_observation_callback(OrderedObservationCallback::TtyName);
            None
        }
    }

    #[test]
    fn guarded_moved_split_transfers_cross_workspace_count_and_preserves_same_workspace_cache() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        for (pane_base, source_workspace, target_workspace) in [
            (32_100, "moved-source", "moved-target"),
            (32_110, "moved-shared", "moved-shared"),
        ] {
            let mux = Arc::new(Mux::new(None));
            let domain: Arc<dyn Domain> =
                Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
            mux.add_domain(&domain).expect("register moved-split domain");

            let source_window =
                mux.new_empty_window(Some(source_workspace.to_string()), None);
            let source_window_id = *source_window;
            drop(source_window);
            let target_window =
                mux.new_empty_window(Some(target_workspace.to_string()), None);
            let target_window_id = *target_window;
            drop(target_window);

            let source = FakePane::new(pane_base, size);
            let target = FakePane::new(pane_base + 1, size);
            let source_tab = attach_floating_reconcile_test_tab(
                &mux,
                &source,
                size,
                source_window_id,
            );
            let target_tab = attach_floating_reconcile_test_tab(
                &mux,
                &target,
                size,
                target_window_id,
            );
            let counts_before = mux.num_panes_by_workspace.read().clone();
            mux.pane_count_recomputes
                .store(0, std::sync::atomic::Ordering::Relaxed);

            let target_guard = mux
                .capture_pane_operation(target.pane_id())
                .expect("capture exact moved-split target");
            let source_guard = mux
                .capture_pane_operation(source.pane_id())
                .expect("capture exact moved-split source");
            let receipt = mux
                .commit_guarded_moved_split(
                    &target_guard,
                    &source_guard,
                    SplitRequest::default(),
                )
                .expect("commit exact moved split");

            assert_eq!(receipt.pane_id(), source.pane_id());
            assert_eq!(receipt.tab_id(), target_tab.tab_id());
            assert_eq!(receipt.window_id(), target_window_id);
            assert!(mux.get_tab(source_tab.tab_id()).is_none());
            assert!(mux.get_window(source_window_id).is_none());
            assert_eq!(target_tab.iter_all_panes().len(), 2);
            if source_workspace == target_workspace {
                assert_eq!(
                    *mux.num_panes_by_workspace.read(),
                    counts_before,
                    "same-workspace relocation must leave the exact count cache unchanged"
                );
            } else {
                let counts = mux.num_panes_by_workspace.read();
                assert_eq!(counts.get(source_workspace), None);
                assert_eq!(counts.get(target_workspace).copied(), Some(2));
            }
            assert_eq!(
                mux.pane_count_recomputes
                    .load(std::sync::atomic::Ordering::Relaxed),
                0,
                "moved split must apply one O(1) workspace delta without a global recount"
            );
        }
    }

    #[test]
    fn guarded_moved_split_rejects_stale_same_owner_layout_without_overwrite() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let domain: Arc<dyn Domain> =
            Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
        mux.add_domain(&domain).expect("register moved-split domain");
        let source_window = mux.new_empty_window(Some("moved-stale".to_string()), None);
        let source_window_id = *source_window;
        drop(source_window);
        let target_window = mux.new_empty_window(Some("moved-stale".to_string()), None);
        let target_window_id = *target_window;
        drop(target_window);

        let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let revision_after_hook = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let hook_tab = Arc::new(Mutex::new(None::<std::sync::Weak<Tab>>));
        let weak_mux = Arc::downgrade(&mux);
        let armed_for_hook = Arc::clone(&armed);
        let fired_for_hook = Arc::clone(&fired);
        let revision_for_hook = Arc::clone(&revision_after_hook);
        let tab_for_hook = Arc::clone(&hook_tab);
        let target = FakePane::new_with_pane_id_probe(
            32_120,
            size,
            Arc::new(move || {
                if !armed_for_hook.swap(false, std::sync::atomic::Ordering::AcqRel) {
                    return;
                }
                let tab = tab_for_hook
                    .lock()
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade)
                    .expect("stale-layout hook retains its target tab");
                tab.set_title("concurrent-layout-wins");
                let mux = weak_mux.upgrade().expect("stale-layout hook retains mux");
                revision_for_hook.store(
                    mux.topology.lock().revision.get(),
                    std::sync::atomic::Ordering::Release,
                );
                fired_for_hook.store(true, std::sync::atomic::Ordering::Release);
            }),
        );
        let source = FakePane::new(32_121, size);
        let target_tab =
            attach_floating_reconcile_test_tab(&mux, &target, size, target_window_id);
        let source_tab =
            attach_floating_reconcile_test_tab(&mux, &source, size, source_window_id);
        *hook_tab.lock() = Some(Arc::downgrade(&target_tab));

        let target_guard = mux
            .capture_pane_operation(target.pane_id())
            .expect("capture stale-layout target");
        let source_guard = mux
            .capture_pane_operation(source.pane_id())
            .expect("capture stale-layout source");
        let counts_before = mux.num_panes_by_workspace.read().clone();
        mux.pane_count_recomputes
            .store(0, std::sync::atomic::Ordering::Relaxed);
        armed.store(true, std::sync::atomic::Ordering::Release);

        let error = mux
            .commit_guarded_moved_split(
                &target_guard,
                &source_guard,
                SplitRequest::default(),
            )
            .expect_err("a stale prepared successor must be rejected");

        assert!(
            format!("{error:#}").contains("changed after successor preparation"),
            "unexpected stale-successor error: {:#}",
            error
        );
        assert!(fired.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(target_tab.get_title(), "concurrent-layout-wins");
        assert_eq!(
            mux.topology.lock().revision.get(),
            revision_after_hook.load(std::sync::atomic::Ordering::Acquire),
            "rejected successor must reserve no topology revision after the winning mutation"
        );
        assert!(mux
            .get_tab(source_tab.tab_id())
            .is_some_and(|tab| Arc::ptr_eq(&tab, &source_tab)));
        assert_eq!(target_tab.iter_all_panes().len(), 1);
        assert!(Arc::ptr_eq(&target_tab.iter_all_panes()[0], &target));
        assert_eq!(source_tab.iter_all_panes().len(), 1);
        assert!(Arc::ptr_eq(&source_tab.iter_all_panes()[0], &source));
        assert_eq!(*mux.num_panes_by_workspace.read(), counts_before);
        assert_eq!(
            mux.pane_count_recomputes
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn guarded_moved_split_rejects_replaced_admitted_domain_without_mutation() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let original_domain: Arc<dyn Domain> =
            Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
        mux.add_domain(&original_domain)
            .expect("register original moved-split domain");
        let source_window = mux.new_empty_window(Some("moved-domain".to_string()), None);
        let source_window_id = *source_window;
        drop(source_window);
        let target_window = mux.new_empty_window(Some("moved-domain".to_string()), None);
        let target_window_id = *target_window;
        drop(target_window);
        let source = FakePane::new(32_130, size);
        let target = FakePane::new(32_131, size);
        let source_tab =
            attach_floating_reconcile_test_tab(&mux, &source, size, source_window_id);
        let target_tab =
            attach_floating_reconcile_test_tab(&mux, &target, size, target_window_id);
        let target_guard = mux
            .capture_pane_operation(target.pane_id())
            .expect("capture target before domain replacement");
        let source_guard = mux
            .capture_pane_operation(source.pane_id())
            .expect("capture source before domain replacement");
        let counts_before = mux.num_panes_by_workspace.read().clone();
        let topology_before = mux.topology.lock().revision;
        let authority_before = {
            let authority = mux.pane_authority.lock();
            [target.pane_id(), source.pane_id()].map(|pane_id| {
                authority
                    .structural_by_pane_id
                    .get(&pane_id)
                    .expect("test pane has structural authority")
                    .generation
            })
        };

        let replacement_domain: Arc<dyn Domain> =
            Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
        {
            let _domain_registration = mux.domain_registration.lock();
            let _pane_registration = mux.pane_registration.lock();
            mux.domains
                .write()
                .insert(1, Arc::clone(&replacement_domain));
            mux.domains_by_name.write().insert(
                replacement_domain.domain_name().to_string(),
                Arc::clone(&replacement_domain),
            );
        }

        mux.commit_guarded_moved_split(
            &target_guard,
            &source_guard,
            SplitRequest::default(),
        )
        .expect_err("a replaced admitted domain must reject exact relocation");

        assert_eq!(mux.topology.lock().revision, topology_before);
        assert_eq!(*mux.num_panes_by_workspace.read(), counts_before);
        assert!(mux
            .get_tab(source_tab.tab_id())
            .is_some_and(|tab| Arc::ptr_eq(&tab, &source_tab)));
        assert!(mux
            .get_tab(target_tab.tab_id())
            .is_some_and(|tab| Arc::ptr_eq(&tab, &target_tab)));
        assert_eq!(source_tab.iter_all_panes().len(), 1);
        assert!(Arc::ptr_eq(&source_tab.iter_all_panes()[0], &source));
        assert_eq!(target_tab.iter_all_panes().len(), 1);
        assert!(Arc::ptr_eq(&target_tab.iter_all_panes()[0], &target));
        let authority = mux.pane_authority.lock();
        let authority_after = [target.pane_id(), source.pane_id()].map(|pane_id| {
            authority
                .structural_by_pane_id
                .get(&pane_id)
                .expect("rejected relocation preserves structural authority")
                .generation
        });
        assert_eq!(authority_after, authority_before);
    }

    #[test]
    fn guarded_moved_split_rejects_overdepth_tree_before_clone_or_mutation() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let domain: Arc<dyn Domain> =
            Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
        mux.add_domain(&domain).expect("register moved-split domain");
        let source_window = mux.new_empty_window(Some("moved-depth".to_string()), None);
        let source_window_id = *source_window;
        drop(source_window);
        let target_window = mux.new_empty_window(Some("moved-depth".to_string()), None);
        let target_window_id = *target_window;
        drop(target_window);
        let source = FakePane::new(32_135, size);
        let target = FakePane::new(32_136, size);
        let source_tab =
            attach_floating_reconcile_test_tab(&mux, &source, size, source_window_id);
        let target_tab =
            attach_floating_reconcile_test_tab(&mux, &target, size, target_window_id);
        let target_guard = mux
            .capture_pane_operation(target.pane_id())
            .expect("capture overdepth target");
        let source_guard = mux
            .capture_pane_operation(source.pane_id())
            .expect("capture overdepth source");
        let topology_before = mux.topology.lock().revision;
        let counts_before = mux.num_panes_by_workspace.read().clone();

        let first = TerminalSize {
            cols: 39,
            pixel_width: 390,
            ..size
        };
        let second = TerminalSize {
            cols: 40,
            pixel_width: 400,
            ..size
        };
        let mut tree = Tree::Leaf(Arc::clone(&target));
        for _ in 0..MAX_MOVED_SPLIT_TREE_DEPTH {
            tree = Tree::Node {
                left: Box::new(tree),
                right: Box::new(Tree::Leaf(Arc::clone(&target))),
                data: Some(SplitDirectionAndSize {
                    direction: SplitDirection::Horizontal,
                    first,
                    second,
                }),
            };
        }
        target_tab.inner.lock().pane = Some(tree);

        let error = mux
            .commit_guarded_moved_split(
                &target_guard,
                &source_guard,
                SplitRequest::default(),
            )
            .expect_err("an overdepth tree must fail before derived clone");

        assert!(
            format!("{error:#}").contains("pane tree depth 65 exceeds limit 64"),
            "unexpected overdepth rejection: {:#}",
            error
        );
        assert_eq!(mux.topology.lock().revision, topology_before);
        assert_eq!(*mux.num_panes_by_workspace.read(), counts_before);
        assert!(mux
            .get_tab(source_tab.tab_id())
            .is_some_and(|tab| Arc::ptr_eq(&tab, &source_tab)));
        assert_eq!(source_tab.iter_all_panes().len(), 1);
        assert!(Arc::ptr_eq(&source_tab.iter_all_panes()[0], &source));
    }

    #[test]
    fn guarded_moved_split_retires_source_tab_and_window_before_trailing_tab_events() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let domain: Arc<dyn Domain> =
            Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
        mux.add_domain(&domain).expect("register moved-split domain");
        let source_window = mux.new_empty_window(Some("moved-order".to_string()), None);
        let source_window_id = *source_window;
        drop(source_window);
        let target_window = mux.new_empty_window(Some("moved-order".to_string()), None);
        let target_window_id = *target_window;
        drop(target_window);
        let source = FakePane::new(32_140, size);
        let target = FakePane::new(32_141, size);
        let source_tab =
            attach_floating_reconcile_test_tab(&mux, &source, size, source_window_id);
        let target_tab =
            attach_floating_reconcile_test_tab(&mux, &target, size, target_window_id);
        let target_guard = mux
            .capture_pane_operation(target.pane_id())
            .expect("capture retirement-order target");
        let source_guard = mux
            .capture_pane_operation(source.pane_id())
            .expect("capture retirement-order source");
        let before_revision = mux.topology.lock().revision;
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        let weak_mux = Arc::downgrade(&mux);
        let weak_target_tab = Arc::downgrade(&target_tab);
        let source_tab_id = source_tab.tab_id();
        let target_tab_id = target_tab.tab_id();
        let source_pane_id = source.pane_id();
        mux.subscribe_with_topology(move |envelope| {
            let kind = match &envelope.notification {
                MuxNotification::WindowTopologyChanged(change)
                    if change.removed_windows() == [source_window_id] =>
                {
                    let mux = weak_mux.upgrade().expect("retirement subscriber retains mux");
                    let target_tab = weak_target_tab
                        .upgrade()
                        .expect("retirement subscriber retains target tab");
                    assert!(mux.domain_registration.try_lock().is_some());
                    assert!(mux.pane_registration.try_lock().is_some());
                    assert!(mux.pane_authority.try_lock().is_some());
                    assert!(mux.tabs.try_read().is_some());
                    assert!(mux.windows.try_read().is_some());
                    assert!(mux.tab_parents.try_read().is_some());
                    assert!(mux.num_panes_by_workspace.try_write().is_some());
                    assert!(mux.topology.try_lock().is_some());
                    assert!(target_tab.inner.try_lock().is_some());
                    assert!(mux.get_tab(source_tab_id).is_none());
                    assert!(mux.get_window(source_window_id).is_none());
                    assert!(target_tab
                        .iter_all_panes()
                        .iter()
                        .any(|pane| pane.pane_id() == source_pane_id));
                    Some(0_u8)
                }
                MuxNotification::TabResized(tab_id) if *tab_id == target_tab_id => Some(1),
                MuxNotification::PaneFocused(pane_id) if *pane_id == source_pane_id => Some(2),
                _ => None,
            };
            if let (Some(kind), crate::MuxTopologyStamp::Revision(revision)) =
                (kind, envelope.topology)
            {
                observed_for_subscriber.lock().push((kind, revision));
            }
            true
        })
        .expect("subscribe to moved-split retirement transaction");

        let receipt = mux
            .commit_guarded_moved_split(
                &target_guard,
                &source_guard,
                SplitRequest::default(),
            )
            .expect("commit source-empty moved split");

        assert_eq!(receipt.tab_id(), target_tab_id);
        assert_eq!(receipt.window_id(), target_window_id);
        assert_eq!(
            *observed.lock(),
            vec![
                (
                    0,
                    crate::TopologyRevision::new(before_revision.get() + 1),
                ),
                (
                    1,
                    crate::TopologyRevision::new(before_revision.get() + 2),
                ),
                (
                    2,
                    crate::TopologyRevision::new(before_revision.get() + 3),
                ),
            ],
            "source retirement must publish before its contiguous target resize/focus tail"
        );
        assert!(mux.get_tab(source_tab_id).is_none());
        assert!(mux.get_window(source_window_id).is_none());
        assert_eq!(mux.window_containing_tab(target_tab_id), Some(target_window_id));
    }

    #[test]
    fn fake_pane_default_methods_do_not_panic() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let pane = FakePane::new(42, size);

        assert_eq!(pane.pane_id(), 42);
        assert_eq!(pane.get_cursor_position(), StableCursorPosition::default());
        assert_eq!(pane.get_current_seqno(), SEQ_ZERO);
        assert!(pane.get_changed_since(0..10, SEQ_ZERO).is_empty());
        assert_eq!(pane.get_lines(0..10), (0, Vec::new()));
        assert!(pane.get_logical_lines(0..10).is_empty());
        assert_eq!(pane.get_title(), "fake-pane-42");
        assert!(pane.send_paste("discarded").is_ok());
        assert!(pane
            .key_down(KeyCode::Char('x'), KeyModifiers::NONE)
            .is_ok());
        assert!(pane.key_up(KeyCode::Char('x'), KeyModifiers::NONE).is_ok());
        assert!(pane.reader().unwrap().is_none());
        assert!(!pane.is_dead());
        assert_eq!(pane.domain_id(), 1);
        assert_eq!(pane.palette(), ColorPalette::default());

        let resized = TerminalSize {
            rows: 7,
            cols: 13,
            pixel_width: 130,
            pixel_height: 70,
            dpi: 144,
        };
        pane.resize(resized).unwrap();
        let dimensions = pane.get_dimensions();
        assert_eq!(dimensions.cols, resized.cols);
        assert_eq!(dimensions.viewport_rows, resized.rows);
        assert_eq!(dimensions.scrollback_rows, resized.rows);
        assert_eq!(dimensions.pixel_width, resized.pixel_width);
        assert_eq!(dimensions.pixel_height, resized.pixel_height);
        assert_eq!(dimensions.dpi, resized.dpi);

        let mut writer = pane.writer();
        writer.write_all(b"discarded").unwrap();
        writer.flush().unwrap();
    }

    #[test]
    fn domain_floating_reconcile_noop_rejects_authority_corruption_without_repair() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let domain: Arc<dyn Domain> = Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
        mux.add_domain(&domain).expect("register test domain");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        drop(window);
        let tiled = FakePane::new(30_001, size);
        let tab = attach_floating_reconcile_test_tab(&mux, &tiled, size, window_id);
        let before_revision = mux.topology.lock().revision;

        {
            let mut authority = mux.pane_authority.lock();
            let removed = authority
                .registrations_by_domain
                .get_mut(&1)
                .expect("test domain authority")
                .pane_registrations
                .remove(&30_001);
            assert!(removed.is_some(), "plant missing reverse domain authority");
        }

        let error = mux
            .reconcile_domain_floating_panes(1, vec![Arc::clone(&tiled)], Vec::new())
            .expect_err("an unchanged snapshot must still reject authority corruption");
        assert!(
            format!("{error:#}").contains("lacks exact domain authority"),
            "unexpected reconciliation error: {:#}",
            error
        );
        assert_eq!(mux.topology.lock().revision, before_revision);
        assert!(tab.iter_floating_panes().is_empty());
        assert!(mux
            .panes
            .read()
            .get(&30_001)
            .is_some_and(|registered| Arc::ptr_eq(&registered.pane, &tiled)));
        let authority = mux.pane_authority.lock();
        assert!(authority
            .registrations_by_domain
            .get(&1)
            .is_some_and(|registrations| !registrations
                .pane_registrations
                .contains_key(&30_001)));
        assert!(authority
            .structural_by_pane_id
            .get(&30_001)
            .is_some_and(|owner| owner.matches_pane(&tiled) && owner.matches_tab(&tab)));
    }

    #[test]
    fn domain_floating_reconcile_updates_new_and_stale_authority_with_callbacks_unlocked() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let domain: Arc<dyn Domain> = Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
        mux.add_domain(&domain).expect("register test domain");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        drop(window);
        let workspace = mux
            .windows
            .read()
            .get(&window_id)
            .expect("test window")
            .get_workspace()
            .to_string();
        let tiled = FakePane::new(30_002, size);
        let floating = FakePane::new(30_003, size);
        let tab = attach_floating_reconcile_test_tab(&mux, &tiled, size, window_id);
        mux.pane_count_recomputes
            .store(0, std::sync::atomic::Ordering::Relaxed);

        mux.reconcile_domain_floating_panes(
            1,
            vec![Arc::clone(&tiled), Arc::clone(&floating)],
            vec![floating_reconcile_state(&tab, &floating, 2)],
        )
        .expect("publish exact floating authority");
        assert_eq!(
            mux.num_panes_by_workspace
                .read()
                .get(&workspace)
                .copied(),
            Some(2)
        );
        {
            let authority = mux.pane_authority.lock();
            let domain_registrations = authority
                .registrations_by_domain
                .get(&1)
                .expect("reconciled domain directory");
            assert!(domain_registrations.matches_domain(Some(&domain)));
            assert!(domain_registrations
                .pane_registrations
                .get(&30_003)
                .is_some_and(|registration| registration.is_same_pane(&floating)));
            assert!(authority
                .structural_by_pane_id
                .get(&30_003)
                .is_some_and(|owner| {
                    owner.matches_pane(&floating)
                        && owner.matches_tab(&tab)
                        && owner.lane == PaneStructuralLane::Floating
                }));
        }

        let callback_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let weak_mux = Arc::downgrade(&mux);
        let weak_tab = Arc::downgrade(&tab);
        let callback_ran_for_subscriber = Arc::clone(&callback_ran);
        mux.subscribe(move |notification| {
            if !matches!(notification, MuxNotification::PaneRemoved(30_003)) {
                return true;
            }
            let mux = weak_mux.upgrade().expect("test mux remains live");
            let tab = weak_tab.upgrade().expect("test tab remains live");
            assert!(mux.domain_registration.try_lock().is_some());
            assert!(mux.pane_registration.try_lock().is_some());
            assert!(mux.pane_authority.try_lock().is_some());
            assert!(mux.panes.try_write().is_some());
            assert!(mux.windows.try_write().is_some());
            assert!(mux.pending_pane_output.try_lock().is_some());
            assert!(mux.pending_pane_lifecycle.try_lock().is_some());
            assert!(mux.topology.try_lock().is_some());
            assert!(mux.num_panes_by_workspace.try_write().is_some());
            assert!(tab.inner.try_lock().is_some());
            callback_ran_for_subscriber.store(true, std::sync::atomic::Ordering::Release);
            true
        })
        .expect("subscribe to exact stale retirement");

        mux.reconcile_domain_floating_panes(1, vec![Arc::clone(&tiled)], Vec::new())
            .expect("retire stale floating authority");
        assert!(callback_ran.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(
            mux.num_panes_by_workspace
                .read()
                .get(&workspace)
                .copied(),
            Some(1)
        );
        assert_eq!(
            mux.pane_count_recomputes
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "domain floating reconciliation must use exact workspace deltas"
        );
        assert!(mux.panes.read().get(&30_003).is_none());
        let authority = mux.pane_authority.lock();
        assert!(authority.structural_by_pane_id.get(&30_003).is_none());
        assert!(authority
            .registrations_by_domain
            .get(&1)
            .is_some_and(|registrations| !registrations
                .pane_registrations
                .contains_key(&30_003)));
        assert!(authority
            .pane_ids_by_tab
            .get(&tab.tab_id())
                .is_some_and(|members| !members.pane_ids.contains(&30_003)));
    }

    #[test]
    fn domain_floating_reconcile_drops_retired_pane_after_unlocking_transaction() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let domain: Arc<dyn Domain> = Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
        mux.add_domain(&domain).expect("register test domain");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        drop(window);
        let tiled = FakePane::new(30_015, size);
        let tab = attach_floating_reconcile_test_tab(&mux, &tiled, size, window_id);
        let drop_state = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let weak_mux = Arc::downgrade(&mux);
        let weak_tab = Arc::downgrade(&tab);
        let drop_state_for_probe = Arc::clone(&drop_state);
        let floating = FakePane::new_with_drop_probe(
            30_016,
            size,
            Arc::new(move || {
                let (Some(mux), Some(tab)) = (weak_mux.upgrade(), weak_tab.upgrade()) else {
                    drop_state_for_probe.store(3, std::sync::atomic::Ordering::Release);
                    return;
                };
                let unlocked = mux.domain_registration.try_lock().is_some()
                    && mux.pane_registration.try_lock().is_some()
                    && mux.pane_authority.try_lock().is_some()
                    && mux.panes.try_write().is_some()
                    && mux.windows.try_write().is_some()
                    && mux.pending_pane_output.try_lock().is_some()
                    && mux.pending_pane_lifecycle.try_lock().is_some()
                    && mux.topology.try_lock().is_some()
                    && mux.num_panes_by_workspace.try_write().is_some()
                    && tab.inner.try_lock().is_some();
                drop_state_for_probe.store(
                    if unlocked { 1 } else { 2 },
                    std::sync::atomic::Ordering::Release,
                );
            }),
        );

        mux.reconcile_domain_floating_panes(
            1,
            vec![Arc::clone(&tiled), Arc::clone(&floating)],
            vec![floating_reconcile_state(&tab, &floating, 2)],
        )
        .expect("publish drop-probed floating pane");
        drop(floating);
        mux.reconcile_domain_floating_panes(1, vec![Arc::clone(&tiled)], Vec::new())
            .expect("retire drop-probed floating pane");

        assert_eq!(
            drop_state.load(std::sync::atomic::Ordering::Acquire),
            1,
            "retired pane must be destroyed synchronously while its mux remains live and after transaction guards release"
        );
    }

    #[test]
    fn domain_floating_reconcile_swaps_exact_owners_across_tabs_atomically() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let domain: Arc<dyn Domain> = Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
        mux.add_domain(&domain).expect("register test domain");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        drop(window);
        let first_tiled = FakePane::new(30_004, size);
        let second_tiled = FakePane::new(30_005, size);
        let first_tab =
            attach_floating_reconcile_test_tab(&mux, &first_tiled, size, window_id);
        let second_tab =
            attach_floating_reconcile_test_tab(&mux, &second_tiled, size, window_id);
        let first_float = FakePane::new(30_006, size);
        let second_float = FakePane::new(30_007, size);
        let authoritative = || {
            vec![
                Arc::clone(&first_tiled),
                Arc::clone(&second_tiled),
                Arc::clone(&first_float),
                Arc::clone(&second_float),
            ]
        };

        mux.reconcile_domain_floating_panes(
            1,
            authoritative(),
            vec![
                floating_reconcile_state(&first_tab, &first_float, 1),
                floating_reconcile_state(&second_tab, &second_float, 2),
            ],
        )
        .expect("publish initial cross-tab floating owners");
        mux.pane_count_recomputes
            .store(0, std::sync::atomic::Ordering::Relaxed);

        mux.reconcile_domain_floating_panes(
            1,
            authoritative(),
            vec![
                floating_reconcile_state(&first_tab, &second_float, 3),
                floating_reconcile_state(&second_tab, &first_float, 4),
            ],
        )
        .expect("swap exact floating owners in one authority cut");

        let first_current = first_tab.iter_floating_panes();
        let second_current = second_tab.iter_floating_panes();
        assert_eq!(first_current.len(), 1);
        assert_eq!(second_current.len(), 1);
        assert!(Arc::ptr_eq(&first_current[0].pane, &second_float));
        assert!(Arc::ptr_eq(&second_current[0].pane, &first_float));
        let authority = mux.pane_authority.lock();
        assert!(authority
            .structural_by_pane_id
            .get(&30_006)
            .is_some_and(|owner| owner.matches_pane(&first_float)
                && owner.matches_tab(&second_tab)
                && owner.lane == PaneStructuralLane::Floating));
        assert!(authority
            .structural_by_pane_id
            .get(&30_007)
            .is_some_and(|owner| owner.matches_pane(&second_float)
                && owner.matches_tab(&first_tab)
                && owner.lane == PaneStructuralLane::Floating));
        assert!(authority
            .pane_ids_by_tab
            .get(&first_tab.tab_id())
            .is_some_and(|members| members.pane_ids.contains(&30_007)
                && !members.pane_ids.contains(&30_006)));
        assert!(authority
            .pane_ids_by_tab
            .get(&second_tab.tab_id())
            .is_some_and(|members| members.pane_ids.contains(&30_006)
                && !members.pane_ids.contains(&30_007)));
        assert_eq!(
            mux.pane_count_recomputes
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn domain_floating_reconcile_structural_generation_exhaustion_is_zero_mutation() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let domain: Arc<dyn Domain> = Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
        mux.add_domain(&domain).expect("register test domain");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        drop(window);
        let first_tiled = FakePane::new(30_012, size);
        let second_tiled = FakePane::new(30_013, size);
        let first_tab =
            attach_floating_reconcile_test_tab(&mux, &first_tiled, size, window_id);
        let second_tab =
            attach_floating_reconcile_test_tab(&mux, &second_tiled, size, window_id);
        let floating = FakePane::new(30_014, size);
        let authoritative = || {
            vec![
                Arc::clone(&first_tiled),
                Arc::clone(&second_tiled),
                Arc::clone(&floating),
            ]
        };

        mux.reconcile_domain_floating_panes(
            1,
            authoritative(),
            vec![floating_reconcile_state(&first_tab, &floating, 1)],
        )
        .expect("publish incumbent floating owner");
        let before_revision = mux.topology.lock().revision;
        let before_workspace_counts = mux.num_panes_by_workspace.read().clone();
        let incumbent_structural_generation = {
            let mut authority = mux.pane_authority.lock();
            let generation = authority
                .structural_by_pane_id
                .get(&30_014)
                .expect("incumbent floating structural owner")
                .generation;
            authority.next_structural_generation = u64::MAX;
            generation
        };

        let error = mux
            .reconcile_domain_floating_panes(
                1,
                authoritative(),
                vec![floating_reconcile_state(&second_tab, &floating, 4)],
            )
            .expect_err("an exhausted structural generation must reject the owner move");
        assert!(
            format!("{error:#}").contains("pane structural-owner generation exhausted"),
            "unexpected exhaustion error: {:#}",
            error
        );
        assert_eq!(mux.topology.lock().revision, before_revision);
        assert_eq!(
            *mux.num_panes_by_workspace.read(),
            before_workspace_counts
        );
        let first_current = first_tab.iter_floating_panes();
        assert_eq!(first_current.len(), 1);
        assert!(Arc::ptr_eq(&first_current[0].pane, &floating));
        assert!(second_tab.iter_floating_panes().is_empty());
        assert!(mux
            .panes
            .read()
            .get(&30_014)
            .is_some_and(|registered| Arc::ptr_eq(&registered.pane, &floating)));
        let authority = mux.pane_authority.lock();
        assert_eq!(authority.next_structural_generation, u64::MAX);
        assert!(authority
            .structural_by_pane_id
            .get(&30_014)
            .is_some_and(|owner| owner.matches_pane(&floating)
                && owner.matches_tab(&first_tab)
                && owner.generation == incumbent_structural_generation));
        assert!(authority
            .pane_ids_by_tab
            .get(&first_tab.tab_id())
            .is_some_and(|members| members.pane_ids.contains(&30_014)));
        assert!(authority
            .pane_ids_by_tab
            .get(&second_tab.tab_id())
            .is_some_and(|members| !members.pane_ids.contains(&30_014)));
    }

    #[test]
    fn domain_floating_reconcile_rejects_wrong_generation_output_before_mutation() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let domain: Arc<dyn Domain> = Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
        mux.add_domain(&domain).expect("register test domain");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        drop(window);
        let tiled = FakePane::new(30_008, size);
        let floating = FakePane::new(30_009, size);
        let tab = attach_floating_reconcile_test_tab(&mux, &tiled, size, window_id);
        mux.reconcile_domain_floating_panes(
            1,
            vec![Arc::clone(&tiled), Arc::clone(&floating)],
            vec![floating_reconcile_state(&tab, &floating, 2)],
        )
        .expect("publish floating pane before output corruption");
        let before_revision = mux.topology.lock().revision;
        let wrong_generation = crate::PaneRegistrationGeneration::new(
            30_009,
            &mux.pane_retirements,
            Arc::downgrade(&mux),
        );
        let wrong_batch = Arc::new(crate::PaneOutputBatch {
            pane_id: 30_009,
            generation: wrong_generation,
            lifecycle_notification: crate::PaneLifecycleNotificationTicket {
                pane_id: 30_009,
                ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            owner: Arc::downgrade(&mux),
            state: std::sync::atomic::AtomicUsize::new(0),
            dispatch_on_main: false,
            reserved_at: std::time::Instant::now(),
        });
        mux.pending_pane_output
            .lock()
            .queued
            .insert(30_009, Arc::clone(&wrong_batch));

        let error = mux
            .reconcile_domain_floating_panes(1, vec![Arc::clone(&tiled)], Vec::new())
            .expect_err("wrong-generation output must reject before stale retirement");
        assert!(
            format!("{error:#}").contains("queued output from another registration generation"),
            "unexpected reconciliation error: {:#}",
            error
        );
        assert_eq!(mux.topology.lock().revision, before_revision);
        assert!(tab
            .iter_floating_panes()
            .iter()
            .any(|positioned| Arc::ptr_eq(&positioned.pane, &floating)));
        assert!(mux
            .panes
            .read()
            .get(&30_009)
            .is_some_and(|registered| Arc::ptr_eq(&registered.pane, &floating)));
        assert!(mux
            .pending_pane_output
            .lock()
            .queued
            .get(&30_009)
            .is_some_and(|batch| Arc::ptr_eq(batch, &wrong_batch)));
        let authority = mux.pane_authority.lock();
        assert!(authority
            .structural_by_pane_id
            .get(&30_009)
            .is_some_and(|owner| owner.matches_pane(&floating) && owner.matches_tab(&tab)));
    }

    #[test]
    fn domain_floating_reconcile_same_id_successor_fails_closed_on_incumbent() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let domain: Arc<dyn Domain> = Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
        mux.add_domain(&domain).expect("register test domain");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        drop(window);
        let tiled = FakePane::new(30_010, size);
        let incumbent = FakePane::new(30_011, size);
        let tab = attach_floating_reconcile_test_tab(&mux, &tiled, size, window_id);
        mux.reconcile_domain_floating_panes(
            1,
            vec![Arc::clone(&tiled), Arc::clone(&incumbent)],
            vec![floating_reconcile_state(&tab, &incumbent, 2)],
        )
        .expect("publish incumbent floating allocation");
        let successor = FakePane::new(30_011, size);
        let before_revision = mux.topology.lock().revision;

        let error = mux
            .reconcile_domain_floating_panes(
                1,
                vec![Arc::clone(&tiled), Arc::clone(&successor)],
                vec![floating_reconcile_state(&tab, &successor, 4)],
            )
            .expect_err("same-id exact successor must not displace a live incumbent");
        assert!(
            error
                .downcast_ref::<crate::PaneIdCollision>()
                .is_some_and(|collision| collision.pane_id == 30_011),
            "unexpected successor rejection: {:#}",
            error
        );
        assert_eq!(mux.topology.lock().revision, before_revision);
        assert!(successor.mux_registration_slot().load().is_none());
        assert!(mux
            .panes
            .read()
            .get(&30_011)
            .is_some_and(|registered| Arc::ptr_eq(&registered.pane, &incumbent)));
        let positioned = tab.iter_floating_panes();
        assert_eq!(positioned.len(), 1);
        assert!(Arc::ptr_eq(&positioned[0].pane, &incumbent));
        let authority = mux.pane_authority.lock();
        assert!(authority
            .structural_by_pane_id
            .get(&30_011)
            .is_some_and(|owner| owner.matches_pane(&incumbent) && owner.matches_tab(&tab)));
    }

    #[test]
    fn tab_splitting() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        let panes = tab.iter_panes();
        assert_eq!(1, panes.len());
        assert_eq!(0, panes[0].index);
        assert_eq!(true, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(80, panes[0].width);
        assert_eq!(24, panes[0].height);

        assert!(tab
            .compute_split_size(
                1,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                }
            )
            .is_none());

        let horz_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            horz_size,
            SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                second: TerminalSize {
                    rows: 24,
                    cols: 40,
                    pixel_width: 400,
                    pixel_height: 600,
                    dpi: 96,
                },
                first: TerminalSize {
                    rows: 24,
                    cols: 39,
                    pixel_width: 390,
                    pixel_height: 600,
                    dpi: 96,
                },
            }
        );

        let vert_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            vert_size,
            SplitDirectionAndSize {
                direction: SplitDirection::Vertical,
                second: TerminalSize {
                    rows: 12,
                    cols: 80,
                    pixel_width: 800,
                    pixel_height: 300,
                    dpi: 96,
                },
                first: TerminalSize {
                    rows: 11,
                    cols: 80,
                    pixel_width: 800,
                    pixel_height: 275,
                    dpi: 96,
                }
            }
        );

        let new_index = tab
            .split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
                FakePane::new(2, horz_size.second),
            )
            .unwrap();
        assert_eq!(new_index, 1);

        let panes = tab.iter_panes();
        assert_eq!(2, panes.len());

        assert_eq!(0, panes[0].index);
        assert_eq!(false, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(39, panes[0].width);
        assert_eq!(24, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(600, panes[0].pixel_height);
        assert_eq!(1, panes[0].pane.pane_id());

        assert_eq!(1, panes[1].index);
        assert_eq!(true, panes[1].is_active);
        assert_eq!(40, panes[1].left);
        assert_eq!(0, panes[1].top);
        assert_eq!(40, panes[1].width);
        assert_eq!(24, panes[1].height);
        assert_eq!(400, panes[1].pixel_width);
        assert_eq!(600, panes[1].pixel_height);
        assert_eq!(2, panes[1].pane.pane_id());

        let vert_size = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        let new_index = tab
            .split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    top_level: false,
                    target_is_second: true,
                    size: Default::default(),
                },
                FakePane::new(3, vert_size.second),
            )
            .unwrap();
        assert_eq!(new_index, 1);

        let panes = tab.iter_panes();
        assert_eq!(3, panes.len());

        assert_eq!(0, panes[0].index);
        assert_eq!(false, panes[0].is_active);
        assert_eq!(0, panes[0].left);
        assert_eq!(0, panes[0].top);
        assert_eq!(39, panes[0].width);
        assert_eq!(11, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(275, panes[0].pixel_height);
        assert_eq!(1, panes[0].pane.pane_id());

        assert_eq!(1, panes[1].index);
        assert_eq!(true, panes[1].is_active);
        assert_eq!(0, panes[1].left);
        assert_eq!(12, panes[1].top);
        assert_eq!(39, panes[1].width);
        assert_eq!(12, panes[1].height);
        assert_eq!(390, panes[1].pixel_width);
        assert_eq!(300, panes[1].pixel_height);
        assert_eq!(3, panes[1].pane.pane_id());

        assert_eq!(2, panes[2].index);
        assert_eq!(false, panes[2].is_active);
        assert_eq!(40, panes[2].left);
        assert_eq!(0, panes[2].top);
        assert_eq!(40, panes[2].width);
        assert_eq!(24, panes[2].height);
        assert_eq!(400, panes[2].pixel_width);
        assert_eq!(600, panes[2].pixel_height);
        assert_eq!(2, panes[2].pane.pane_id());

        tab.resize_split_by(1, 1);
        let panes = tab.iter_panes();
        assert_eq!(39, panes[0].width);
        assert_eq!(12, panes[0].height);
        assert_eq!(390, panes[0].pixel_width);
        assert_eq!(300, panes[0].pixel_height);

        assert_eq!(39, panes[1].width);
        assert_eq!(11, panes[1].height);
        assert_eq!(390, panes[1].pixel_width);
        assert_eq!(275, panes[1].pixel_height);

        assert_eq!(40, panes[2].width);
        assert_eq!(24, panes[2].height);
        assert_eq!(400, panes[2].pixel_width);
        assert_eq!(600, panes[2].pixel_height);
    }

    #[test]
    fn floating_pane_add_clamps_rect_and_takes_focus() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        let floating = tab
            .add_floating_pane(
                FakePane::new(99, size),
                FloatingPaneRect {
                    left: 78,
                    top: 23,
                    width: 1,
                    height: 1,
                },
            )
            .expect("floating pane should be detached");

        assert_eq!(99, floating.pane_id);
        assert!(floating.is_focused);
        assert_eq!(75, floating.left);
        assert_eq!(21, floating.top);
        assert_eq!(min_floating_pane_width(), floating.width);
        assert_eq!(min_floating_pane_height(), floating.height);
        assert_eq!(Some(2), tab.count_panes());
        assert_eq!(99, tab.get_active_pane().expect("floating focus").pane_id());
    }

    #[test]
    fn has_panes_in_domain_counts_floating_panes() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_domain(1, size, 1));
        tab.add_floating_pane(
            FakePane::new_with_domain(2, size, 2),
            FloatingPaneRect {
                left: 2,
                top: 2,
                width: 20,
                height: 10,
            },
        )
        .expect("floating pane should be detached");

        assert!(tab.has_panes_in_domain(1));
        assert!(tab.has_panes_in_domain(2));
        assert!(!tab.has_panes_in_domain(3));
    }

    #[test]
    fn domain_id_for_pane_counts_floating_panes() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_domain(1, size, 1));
        tab.add_floating_pane(
            FakePane::new_with_domain(2, size, 2),
            FloatingPaneRect {
                left: 2,
                top: 2,
                width: 20,
                height: 10,
            },
        )
        .expect("floating pane should be detached");

        assert_eq!(tab.domain_id_for_pane(1), Some(1));
        assert_eq!(tab.domain_id_for_pane(2), Some(2));
        assert_eq!(tab.domain_id_for_pane(3), None);
    }

    #[test]
    fn floating_pane_focus_and_visibility_fallback() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        tab.add_floating_pane(
            FakePane::new(2, size),
            FloatingPaneRect {
                left: 2,
                top: 2,
                width: 20,
                height: 10,
            },
        )
        .expect("floating pane should be detached");
        tab.add_floating_pane(
            FakePane::new(3, size),
            FloatingPaneRect {
                left: 8,
                top: 6,
                width: 25,
                height: 12,
            },
        )
        .expect("floating pane should be detached");

        let panes = tab.iter_floating_panes();
        assert_eq!(2, panes.len());
        assert_eq!(2, panes[0].pane_id);
        assert_eq!(3, panes[1].pane_id);
        assert!(panes[1].is_focused);

        assert!(tab.set_floating_pane_focus(2));
        let panes = tab.iter_floating_panes();
        assert_eq!(2, panes.last().expect("focused pane").pane_id);
        assert!(panes.last().expect("focused pane").is_focused);
        assert_eq!(
            2,
            tab.get_active_pane().expect("focused floating").pane_id()
        );

        assert!(tab.set_floating_pane_visible(2, false));
        assert_eq!(
            1,
            tab.get_active_pane()
                .expect("fallback split pane")
                .pane_id()
        );

        let pane_two = tab
            .iter_floating_panes()
            .into_iter()
            .find(|pane| pane.pane_id == 2)
            .expect("pane 2 exists");
        assert!(!pane_two.visible);
    }

    #[test]
    fn remove_floating_pane_updates_membership_and_count() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));
        tab.add_floating_pane(
            FakePane::new(42, size),
            FloatingPaneRect {
                left: 4,
                top: 4,
                width: 30,
                height: 8,
            },
        )
        .expect("floating pane should be detached");

        assert!(tab.contains_pane(42));
        let removed = tab
            .remove_floating_pane(42)
            .expect("floating pane should be removed");
        assert_eq!(42, removed.pane_id());
        assert!(!tab.contains_pane(42));
        assert_eq!(Some(1), tab.count_panes());
        assert_eq!(
            1,
            tab.get_active_pane().expect("split pane focus").pane_id()
        );
    }

    #[test]
    fn floating_add_and_remove_run_pane_callbacks_after_unlock() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Arc::new(Tab::new(&size));
        let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let weak_tab = Arc::downgrade(&tab);
        let probe: Arc<dyn Fn() + Send + Sync> = {
            let armed = Arc::clone(&armed);
            let callback_count = Arc::clone(&callback_count);
            Arc::new(move || {
                if !armed.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                let tab = weak_tab.upgrade().expect("tab retained by test");
                assert!(
                    tab.inner.try_lock().is_some(),
                    "floating-pane callback must run after Tab::inner is released"
                );
                callback_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            })
        };
        let root = FakePane::new_with_callback_probe(1, size, false, false, Arc::clone(&probe));
        let floating =
            FakePane::new_with_callback_probe(42, size, false, false, Arc::clone(&probe));
        tab.assign_pane(&root);
        armed.store(true, std::sync::atomic::Ordering::Release);

        tab.add_floating_pane(
            Arc::clone(&floating),
            FloatingPaneRect {
                left: 4,
                top: 4,
                width: 30,
                height: 8,
            },
        )
        .expect("floating pane should be admitted");
        let removed = tab
            .remove_floating_pane(42)
            .expect("floating pane should be removed");

        assert!(Arc::ptr_eq(&removed, &floating));
        assert!(
            callback_count.load(std::sync::atomic::Ordering::Acquire) >= 4,
            "floating resize and focus transitions should exercise unlocked callbacks"
        );
    }

    #[test]
    fn bound_split_callbacks_reenter_after_all_count_and_topology_authorities_unlock() {
        let _mux_guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let window = mux.new_empty_window(Some("bound-split-callback".to_string()), None);
        let window_id = *window;
        let root = FakePane::new(32_001, size);
        let tab = attach_floating_reconcile_test_tab(&mux, &root, size, window_id);
        let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let weak_mux = Arc::downgrade(&mux);
        let weak_tab = Arc::downgrade(&tab);
        let probe: Arc<dyn Fn() + Send + Sync> = {
            let armed = Arc::clone(&armed);
            let callback_count = Arc::clone(&callback_count);
            Arc::new(move || {
                if !armed.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                let mux = weak_mux.upgrade().expect("split mux retained by test");
                let tab = weak_tab.upgrade().expect("split tab retained by test");
                assert!(
                    mux.pane_registration.try_lock().is_some(),
                    "split callback must run after pane registration serialization is released"
                );
                assert!(
                    mux.pane_authority.try_lock().is_some(),
                    "split callback must run after pane authority is released"
                );
                assert!(
                    mux.tabs.try_write().is_some(),
                    "split callback must run after tab registry authority is released"
                );
                assert!(
                    mux.windows.try_write().is_some(),
                    "split callback must run after window authority is released"
                );
                assert!(
                    mux.tab_parents.try_write().is_some(),
                    "split callback must run after parent authority is released"
                );
                assert!(
                    mux.num_panes_by_workspace.try_write().is_some(),
                    "split callback must run after workspace-count authority is released"
                );
                assert!(
                    mux.topology.try_lock().is_some(),
                    "split callback must run after topology revision authority is released"
                );
                assert!(
                    tab.inner.try_lock().is_some(),
                    "split callback must run after TabInner is released"
                );
                let window_count = mux
                    .get_window(window_id)
                    .expect("split callback window remains registered")
                    .structural_pane_count();
                let workspace_count = mux
                    .num_panes_by_workspace
                    .read()
                    .get("bound-split-callback")
                    .copied();
                assert!(
                    matches!((window_count, workspace_count), (1, Some(1)) | (2, Some(2))),
                    "split callbacks must observe either the complete pre-commit or complete post-commit count cut"
                );
                if window_count == 2 {
                    callback_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                }
            })
        };
        let inserted = FakePane::new_with_callback_probe(
            32_002,
            size,
            false,
            false,
            Arc::clone(&probe),
        );
        mux.add_pane(&inserted)
            .expect("register exact bound-split pane");
        armed.store(true, std::sync::atomic::Ordering::Release);

        tab.split_and_insert(0, SplitRequest::default(), inserted)
            .expect("bound split must commit before callbacks reenter");

        assert!(
            callback_count.load(std::sync::atomic::Ordering::Acquire) >= 1,
            "bound split must exercise at least one unlocked post-commit pane callback"
        );
        assert_eq!(
            mux.get_window(window_id)
                .expect("bound split window remains registered")
                .structural_pane_count(),
            2
        );
        assert_eq!(
            mux.num_panes_by_workspace
                .read()
                .get("bound-split-callback")
                .copied(),
            Some(2)
        );
        drop(window);
    }

    #[test]
    fn public_floating_remove_retires_exact_state_with_contiguous_revisions_and_unlocked_reentry() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let domain: Arc<dyn Domain> = Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
        mux.add_domain(&domain).expect("register public-floating test domain");
        let window = mux.new_empty_window(Some("public-floating".to_string()), None);
        let window_id = *window;
        drop(window);
        let tiled = FakePane::new(31_001, size);
        let floating = FakePane::new(31_002, size);
        let tab = attach_floating_reconcile_test_tab(&mux, &tiled, size, window_id);
        mux.add_pane(&floating)
            .expect("register detached public floating pane");
        mux.pane_count_recomputes
            .store(0, std::sync::atomic::Ordering::Relaxed);
        tab.add_floating_pane(
            Arc::clone(&floating),
            FloatingPaneRect {
                left: 2,
                top: 2,
                width: 20,
                height: 10,
            },
        )
        .expect("attach exact public floating pane");
        assert_eq!(
            mux.num_panes_by_workspace
                .read()
                .get("public-floating")
                .copied(),
            Some(2)
        );
        assert_eq!(
            mux.pane_count_recomputes
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "public floating attachment must update one workspace without a global recount"
        );

        let before_revision = mux.topology.lock().revision;
        let tab_id = tab.tab_id();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let callback_reentered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let weak_mux = Arc::downgrade(&mux);
        let weak_tab = Arc::downgrade(&tab);
        let observed_for_subscriber = Arc::clone(&observed);
        let callback_reentered_for_subscriber = Arc::clone(&callback_reentered);
        mux.subscribe_with_topology(move |envelope| {
            let kind = match envelope.notification {
                MuxNotification::TabResized(observed_tab_id) if observed_tab_id == tab_id => {
                    Some("tab")
                }
                MuxNotification::PaneFocused(31_001) => Some("focus"),
                MuxNotification::PaneRemoved(31_002) => {
                    let mux = weak_mux.upgrade().expect("test mux remains live");
                    let tab = weak_tab.upgrade().expect("test tab remains live");
                    assert!(mux.domain_registration.try_lock().is_some());
                    assert!(mux.pane_registration.try_lock().is_some());
                    assert!(mux.pane_authority.try_lock().is_some());
                    assert!(mux.tabs.try_read().is_some());
                    assert!(mux.windows.try_read().is_some());
                    assert!(mux.tab_parents.try_read().is_some());
                    assert!(mux.num_panes_by_workspace.try_write().is_some());
                    assert!(mux.panes.try_write().is_some());
                    assert!(mux.pending_pane_output.try_lock().is_some());
                    assert!(mux.pending_pane_lifecycle.try_lock().is_some());
                    assert!(mux.topology.try_lock().is_some());
                    assert!(tab.inner.try_lock().is_some());
                    assert!(mux.get_pane(31_002).is_none());
                    assert!(!tab.contains_pane(31_002));
                    assert_eq!(
                        mux.num_panes_by_workspace
                            .read()
                            .get("public-floating")
                            .copied(),
                        Some(1)
                    );
                    callback_reentered_for_subscriber
                        .store(true, std::sync::atomic::Ordering::Release);
                    Some("removed")
                }
                _ => None,
            };
            if let (Some(kind), crate::MuxTopologyStamp::Revision(revision)) =
                (kind, envelope.topology)
            {
                observed_for_subscriber.lock().push((kind, revision));
            }
            true
        })
        .expect("subscribe to public floating retirement");

        let removed = tab
            .remove_floating_pane(31_002)
            .expect("terminally remove exact public floating pane");
        assert!(Arc::ptr_eq(&removed, &floating));
        assert!(callback_reentered.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(
            *observed.lock(),
            vec![
                (
                    "tab",
                    crate::TopologyRevision::new(before_revision.get() + 1),
                ),
                (
                    "focus",
                    crate::TopologyRevision::new(before_revision.get() + 2),
                ),
                (
                    "removed",
                    crate::TopologyRevision::new(before_revision.get() + 3),
                ),
            ],
            "structural and terminal lifecycle edges must occupy one contiguous revision range"
        );
        assert!(mux.panes.read().get(&31_002).is_none());
        assert!(mux.pending_pane_output.lock().queued.get(&31_002).is_none());
        let authority = mux.pane_authority.lock();
        assert!(authority.structural_by_pane_id.get(&31_002).is_none());
        assert!(authority
            .registrations_by_domain
            .get(&1)
            .is_some_and(|registrations| !registrations
                .pane_registrations
                .contains_key(&31_002)));
        assert!(authority
            .pane_ids_by_tab
            .get(&tab.tab_id())
            .is_some_and(|members| !members.pane_ids.contains(&31_002)));
        drop(authority);
        assert_eq!(
            floating
                .downcast_ref::<FakePane>()
                .expect("floating pane keeps its concrete test type")
                .kills
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            mux.pane_count_recomputes
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "public floating retirement must update one workspace without a global recount"
        );
    }

    #[test]
    fn public_floating_remove_preserves_planted_same_id_live_successor() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let domain: Arc<dyn Domain> = Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
        mux.add_domain(&domain).expect("register successor test domain");
        let window = mux.new_empty_window(Some("public-successor".to_string()), None);
        let window_id = *window;
        drop(window);
        let tiled = FakePane::new(31_003, size);
        let incumbent = FakePane::new(31_004, size);
        let successor = FakePane::new(31_004, size);
        let tab = attach_floating_reconcile_test_tab(&mux, &tiled, size, window_id);
        mux.add_pane(&incumbent)
            .expect("register incumbent floating pane");
        tab.add_floating_pane(
            Arc::clone(&incumbent),
            FloatingPaneRect {
                left: 2,
                top: 2,
                width: 20,
                height: 10,
            },
        )
        .expect("attach incumbent floating pane");
        let successor_generation = crate::PaneRegistrationGeneration::new(
            31_004,
            &mux.pane_retirements,
            Arc::downgrade(&mux),
        );
        let displaced_live = {
            let _registration = mux.pane_registration.lock();
            mux.panes
                .write()
                .insert(
                    31_004,
                    crate::LivePaneRegistration {
                        pane: Arc::clone(&successor),
                        generation: Arc::clone(&successor_generation),
                        domain_id: 1,
                    },
                )
                .expect("plant a distinct same-id live successor")
        };
        let before_revision = mux.topology.lock().revision;

        assert!(
            tab.remove_floating_pane(31_004).is_none(),
            "an incumbent structural Arc must not retire a distinct live successor"
        );
        assert_eq!(mux.topology.lock().revision, before_revision);
        assert!(tab
            .iter_floating_panes()
            .iter()
            .any(|positioned| Arc::ptr_eq(&positioned.pane, &incumbent)));
        assert!(mux
            .panes
            .read()
            .get(&31_004)
            .is_some_and(|registered| {
                Arc::ptr_eq(&registered.pane, &successor)
                    && Arc::ptr_eq(&registered.generation, &successor_generation)
            }));
        assert_eq!(
            mux.num_panes_by_workspace
                .read()
                .get("public-successor")
                .copied(),
            Some(2)
        );
        let authority = mux.pane_authority.lock();
        assert!(authority
            .structural_by_pane_id
            .get(&31_004)
            .is_some_and(|owner| owner.matches_pane(&incumbent) && owner.matches_tab(&tab)));
        drop(authority);
        assert_eq!(
            incumbent
                .downcast_ref::<FakePane>()
                .expect("incumbent concrete pane")
                .kills
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            successor
                .downcast_ref::<FakePane>()
                .expect("successor concrete pane")
                .kills
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        drop(displaced_live);
    }

    #[test]
    fn public_floating_remove_topology_exhaustion_is_zero_mutation() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let domain: Arc<dyn Domain> = Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
        mux.add_domain(&domain).expect("register exhaustion test domain");
        let window = mux.new_empty_window(Some("public-exhaustion".to_string()), None);
        let window_id = *window;
        drop(window);
        let tiled = FakePane::new(31_005, size);
        let floating = FakePane::new(31_006, size);
        let tab = attach_floating_reconcile_test_tab(&mux, &tiled, size, window_id);
        mux.add_pane(&floating)
            .expect("register exhaustion floating pane");
        tab.add_floating_pane(
            Arc::clone(&floating),
            FloatingPaneRect {
                left: 2,
                top: 2,
                width: 20,
                height: 10,
            },
        )
        .expect("attach exhaustion floating pane");
        {
            let mut topology = mux.topology.lock();
            topology.revision = crate::TopologyRevision::new(u64::MAX - 2);
            topology.exhausted = false;
        }

        assert!(tab.remove_floating_pane(31_006).is_none());
        let topology = mux.topology.lock();
        assert!(topology.exhausted);
        assert_eq!(topology.revision, crate::TopologyRevision::new(u64::MAX - 2));
        drop(topology);
        assert!(tab
            .iter_floating_panes()
            .iter()
            .any(|positioned| Arc::ptr_eq(&positioned.pane, &floating)));
        assert!(mux
            .panes
            .read()
            .get(&31_006)
            .is_some_and(|registered| Arc::ptr_eq(&registered.pane, &floating)));
        assert_eq!(
            mux.num_panes_by_workspace
                .read()
                .get("public-exhaustion")
                .copied(),
            Some(2)
        );
        assert!(mux
            .pending_pane_lifecycle
            .lock()
            .by_pane
            .get(&31_006)
            .is_none());
        let authority = mux.pane_authority.lock();
        assert!(authority
            .structural_by_pane_id
            .get(&31_006)
            .is_some_and(|owner| owner.matches_pane(&floating) && owner.matches_tab(&tab)));
        drop(authority);
        assert_eq!(
            floating
                .downcast_ref::<FakePane>()
                .expect("floating concrete pane")
                .kills
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[test]
    fn public_floating_remove_rejects_wrong_generation_output_without_mutation() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let domain: Arc<dyn Domain> = Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
        mux.add_domain(&domain)
            .expect("register output-corruption test domain");
        let window = mux.new_empty_window(Some("public-output-corruption".to_string()), None);
        let window_id = *window;
        drop(window);
        let tiled = FakePane::new(31_009, size);
        let floating = FakePane::new(31_010, size);
        let tab = attach_floating_reconcile_test_tab(&mux, &tiled, size, window_id);
        mux.add_pane(&floating)
            .expect("register output-corruption floating pane");
        tab.add_floating_pane(
            Arc::clone(&floating),
            FloatingPaneRect {
                left: 2,
                top: 2,
                width: 20,
                height: 10,
            },
        )
        .expect("attach output-corruption floating pane");
        mux.pane_count_recomputes
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let wrong_generation = crate::PaneRegistrationGeneration::new(
            31_010,
            &mux.pane_retirements,
            Arc::downgrade(&mux),
        );
        let wrong_batch = Arc::new(crate::PaneOutputBatch {
            pane_id: 31_010,
            generation: wrong_generation,
            lifecycle_notification: crate::PaneLifecycleNotificationTicket {
                pane_id: 31_010,
                ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            owner: Arc::downgrade(&mux),
            state: std::sync::atomic::AtomicUsize::new(0),
            dispatch_on_main: false,
            reserved_at: std::time::Instant::now(),
        });
        mux.pending_pane_output
            .lock()
            .queued
            .insert(31_010, Arc::clone(&wrong_batch));
        let before_revision = mux.topology.lock().revision;

        assert!(
            tab.remove_floating_pane(31_010).is_none(),
            "wrong-generation queued output must reject terminal removal"
        );
        assert_eq!(mux.topology.lock().revision, before_revision);
        assert!(
            tab.iter_floating_panes()
                .iter()
                .any(|positioned| Arc::ptr_eq(&positioned.pane, &floating))
        );
        assert!(
            mux.panes
                .read()
                .get(&31_010)
                .is_some_and(|registered| Arc::ptr_eq(&registered.pane, &floating))
        );
        assert_eq!(
            mux.num_panes_by_workspace
                .read()
                .get("public-output-corruption")
                .copied(),
            Some(2)
        );
        assert!(mux
            .pending_pane_output
            .lock()
            .queued
            .get(&31_010)
            .is_some_and(|batch| Arc::ptr_eq(batch, &wrong_batch)));
        let authority = mux.pane_authority.lock();
        assert!(
            authority
                .structural_by_pane_id
                .get(&31_010)
                .is_some_and(|owner| owner.matches_pane(&floating) && owner.matches_tab(&tab))
        );
        assert!(authority
            .registrations_by_domain
            .get(&1)
            .is_some_and(|registrations| registrations
                .pane_registrations
                .get(&31_010)
                .is_some_and(|registration| registration.is_same_pane(&floating))));
        drop(authority);
        assert_eq!(
            floating
                .downcast_ref::<FakePane>()
                .expect("floating concrete pane")
                .kills
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            mux.pane_count_recomputes
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        let removed_wrong_batch = mux.pending_pane_output.lock().queued.remove(&31_010);
        assert!(removed_wrong_batch
            .as_ref()
            .is_some_and(|batch| Arc::ptr_eq(batch, &wrong_batch)));
    }

    #[test]
    fn public_floating_add_workspace_overflow_is_zero_mutation() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let domain: Arc<dyn Domain> = Arc::new(FloatingReconcileTestDomain { domain_id: 1 });
        mux.add_domain(&domain).expect("register overflow test domain");
        let window = mux.new_empty_window(Some("public-overflow".to_string()), None);
        let window_id = *window;
        drop(window);
        let tiled = FakePane::new(31_007, size);
        let floating = FakePane::new(31_008, size);
        let tab = attach_floating_reconcile_test_tab(&mux, &tiled, size, window_id);
        mux.add_pane(&floating)
            .expect("register detached overflow pane");
        mux.num_panes_by_workspace
            .write()
            .insert("public-overflow".to_string(), usize::MAX);
        mux.pane_count_recomputes
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let before_revision = mux.topology.lock().revision;
        let before_structural_generation = mux.pane_authority.lock().next_structural_generation;

        let error = tab
            .add_floating_pane(
                Arc::clone(&floating),
                FloatingPaneRect {
                    left: 2,
                    top: 2,
                    width: 20,
                    height: 10,
                },
            )
            .expect_err("workspace count overflow must reject before attachment");
        assert_eq!(
            error.downcast_ref::<crate::WorkspacePaneCountDeltaRejection>(),
            Some(&crate::WorkspacePaneCountDeltaRejection {
                operation: "floating-pane admission",
                workspace: "public-overflow".to_string(),
                removals: 0,
                additions: 1,
                prior: usize::MAX,
            }),
        );
        assert!(tab.iter_floating_panes().is_empty());
        assert!(mux
            .panes
            .read()
            .get(&31_008)
            .is_some_and(|registered| Arc::ptr_eq(&registered.pane, &floating)));
        assert_eq!(mux.topology.lock().revision, before_revision);
        assert_eq!(
            mux.num_panes_by_workspace
                .read()
                .get("public-overflow")
                .copied(),
            Some(usize::MAX)
        );
        let authority = mux.pane_authority.lock();
        assert_eq!(
            authority.next_structural_generation,
            before_structural_generation
        );
        assert!(authority.structural_by_pane_id.get(&31_008).is_none());
        assert!(authority
            .registrations_by_domain
            .get(&1)
            .is_some_and(|registrations| registrations
                .pane_registrations
                .get(&31_008)
                .is_some_and(|registration| registration.is_same_pane(&floating))));
        drop(authority);
        assert_eq!(
            mux.pane_count_recomputes
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            floating
                .downcast_ref::<FakePane>()
                .expect("floating concrete pane")
                .kills
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[test]
    fn remove_pane_tolerates_missing_split_metadata() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        let left = FakePane::new(1, size);
        let right = FakePane::new(2, size);
        {
            let mut inner = tab.inner.lock();
            inner.pane = Some(Tree::Node {
                left: Box::new(Tree::Leaf(left)),
                right: Box::new(Tree::Leaf(right)),
                data: None,
            });
        }

        let removed = tab.remove_pane(1).expect("pane should be removed");
        assert_eq!(removed.pane_id(), 1);

        let panes = tab.iter_panes();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane.pane_id(), 2);
    }

    #[test]
    fn assign_pane_preserves_existing_root() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));
        tab.assign_pane(&FakePane::new(2, size));

        let panes = tab.iter_panes();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane.pane_id(), 1);
    }

    #[test]
    fn set_active_idx_without_mux_singleton_does_not_panic() {
        let _mux_guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Mux::shutdown();
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));
        tab.split_and_insert(0, SplitRequest::default(), FakePane::new(2, size))
            .expect("split should succeed");

        tab.set_active_idx(0);
        assert_eq!(tab.get_active_idx(), 0);
    }

    #[test]
    fn swap_active_with_index_reports_success_only_after_structural_swap() {
        let _mux_guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Mux::shutdown();
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));
        tab.split_and_insert(0, SplitRequest::default(), FakePane::new(2, size))
            .expect("split should succeed");
        tab.set_active_idx(0);

        let before = tab
            .iter_panes()
            .into_iter()
            .map(|positioned| positioned.pane.pane_id())
            .collect::<Vec<_>>();
        assert_eq!(before, vec![1, 2]);
        assert_eq!(tab.swap_active_with_index(1, true), Some(()));
        let after = tab
            .iter_panes()
            .into_iter()
            .map(|positioned| positioned.pane.pane_id())
            .collect::<Vec<_>>();
        assert_eq!(after, vec![2, 1]);
        assert_eq!(tab.get_active_idx(), 1);

        let unchanged = after;
        assert_eq!(tab.swap_active_with_index(usize::MAX, true), None);
        assert_eq!(
            tab.iter_panes()
                .into_iter()
                .map(|positioned| positioned.pane.pane_id())
                .collect::<Vec<_>>(),
            unchanged,
        );

        let before_invalid_active = tab
            .iter_panes()
            .into_iter()
            .map(|positioned| positioned.pane.pane_id())
            .collect::<Vec<_>>();
        tab.inner.lock().active = usize::MAX;
        assert_eq!(tab.swap_active_with_index(0, true), None);
        assert_eq!(
            tab.iter_panes()
                .into_iter()
                .map(|positioned| positioned.pane.pane_id())
                .collect::<Vec<_>>(),
            before_invalid_active,
            "invalid active state must not partially mutate the tree",
        );
    }

    #[test]
    fn tab_mux_optional_paths_do_not_panic_without_singleton() {
        // This test asserts the *no-singleton* path of the optional-Mux helpers
        // (notably `prune_dead_panes`, which is a no-op when `Mux::try_get()` is
        // None). The Mux singleton is a process global, so without serializing
        // against the mux-creating tests a concurrent `ensure_mux_initialized`
        // can `set_mux` between our `Mux::shutdown()` and the assertions below.
        // `Mux::try_get()` would then return Some — a mux that does NOT contain
        // these FakePanes — so `prune_dead_panes()` would treat them as
        // not-in-mux and prune them, flaking `assert!(!tab.prune_dead_panes())`.
        // Holding MUX_TEST_LOCK (the same lock `ensure_mux_initialized` takes)
        // for the whole body keeps `try_get()` None for the duration. (Safe:
        // shutdown/set_mux/try_get lock the distinct `MUX` global, not this one.)
        let _mux_guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Mux::shutdown();
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));
        tab.split_and_insert(0, SplitRequest::default(), FakePane::new(2, size))
            .expect("split should succeed");

        tab.activate_pane_direction(PaneDirection::Right);
        assert_ne!(
            tab.codec_pane_tree_in_window(7, "detached")
                .expect("codec snapshot must not require a mux singleton"),
            PaneNode::Empty,
        );
        assert!(!tab.prune_dead_panes_without_mux());
        assert_eq!(tab.count_panes(), Some(2));
    }

    #[test]
    fn callback_free_empty_tree_title_preserves_legacy_snapshot_shape() {
        let size = TerminalSize::default();
        let tab = Tab::new(&size);
        tab.set_title("empty-legacy-tab");
        assert_eq!(
            tab.empty_pane_tree_title_callback_free().as_deref(),
            Some("empty-legacy-tab")
        );

        tab.assign_pane(&FakePane::new(290, size));
        assert_eq!(tab.empty_pane_tree_title_callback_free(), None);
    }

    #[test]
    fn codec_snapshot_uses_explicit_owner_metadata_and_observes_after_unlock() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Arc::new(Tab::new(&size));
        let weak_tab = Arc::downgrade(&tab);
        let observations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observations_for_probe = Arc::clone(&observations);
        let probe: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let tab = weak_tab.upgrade().expect("tab retained by test");
            assert!(
                tab.inner.try_lock().is_some(),
                "pane observation must run after the codec snapshot releases Tab::inner",
            );
            observations_for_probe.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        });
        let pane = FakePane::new_with_callback_probe(89, size, false, false, probe);
        tab.assign_pane(&pane);

        let encoded = tab
            .codec_pane_tree_in_window(700, "origin-workspace")
            .expect("stable tab snapshot");
        let PaneNode::Leaf(entry) = encoded else {
            panic!("one-pane tab must encode as a leaf");
        };
        assert_eq!(entry.window_id, 700);
        assert_eq!(entry.workspace, "origin-workspace");
        assert_eq!(entry.pane_id, 89);
        assert!(
            observations.load(std::sync::atomic::Ordering::Acquire) > 0,
            "the codec path must exercise a reentrancy-checked pane observation",
        );
    }

    #[test]
    fn callback_snapshot_identity_match_rejects_duplicates_and_substitutions() {
        let size = TerminalSize::default();
        let first = FakePane::new(291, size);
        let second = FakePane::new(292, size);
        let replacement = FakePane::new(293, size);
        let mut observed = HashMap::new();
        observed.insert(pane_identity(&first), first.pane_id());
        observed.insert(pane_identity(&second), second.pane_id());

        assert!(
            callback_snapshot_matches(&[Arc::clone(&first), Arc::clone(&second)], &observed)
                .expect("matching callback snapshot must be comparable")
        );
        assert!(
            !callback_snapshot_matches(&[Arc::clone(&first), Arc::clone(&first)], &observed)
                .expect("duplicate callback snapshot must be comparable")
        );
        assert!(
            !callback_snapshot_matches(&[Arc::clone(&first), replacement], &observed)
                .expect("substituted callback snapshot must be comparable")
        );
    }

    fn balanced_mux_tree_for_capture(
        first_pane: PaneId,
        leaf_count: usize,
        size: TerminalSize,
    ) -> Tree {
        assert!(leaf_count > 0);
        if leaf_count == 1 {
            let pane = FakePane::new(first_pane, size);
            return Tree::Leaf(pane);
        }
        let left_leaves = leaf_count.div_ceil(2);
        let right_leaves = leaf_count - left_leaves;
        Tree::Node {
            left: Box::new(balanced_mux_tree_for_capture(first_pane, left_leaves, size)),
            right: Box::new(balanced_mux_tree_for_capture(
                first_pane + left_leaves,
                right_leaves,
                size,
            )),
            data: Some(SplitDirectionAndSize {
                direction: if leaf_count.is_multiple_of(2) {
                    SplitDirection::Horizontal
                } else {
                    SplitDirection::Vertical
                },
                first: size,
                second: size,
            }),
        }
    }

    #[test]
    fn flat_codec_capture_growth_is_bounded_across_q_scale() {
        let size = TerminalSize::default();
        for leaf_count in [1_usize, 20, 200, 4_096] {
            let node_count = leaf_count * 2 - 1;
            let tab = Tab::new(&size);
            tab.set_title(&format!("q-{leaf_count}"));
            tab.inner.lock().pane = Some(balanced_mux_tree_for_capture(1, leaf_count, size));
            let mut arena = Vec::new();
            let descriptor = tab
                .append_codec_pane_arena_in_window(
                    77,
                    "q-scale-workspace",
                    &mut arena,
                    64,
                    node_count,
                    node_count,
                )
                .unwrap_or_else(|error| {
                    panic!("q={} full flat append failed: {:#}", leaf_count, error)
                });
            assert_eq!(arena.len(), node_count);
            assert_eq!(descriptor.root_index, Some(0));
            assert_eq!(
                usize::try_from(descriptor.node_count).expect("test node count fits usize"),
                node_count
            );
            assert_eq!(descriptor.tab_title, format!("q-{leaf_count}"));
        }
    }

    #[test]
    fn pane_snapshot_census_ledger_admits_exact_work_and_rejects_plus_one_atomically() {
        let size = TerminalSize::default();
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(71, size));

        let mut exact = PaneSnapshotCensusLedger::new(19, 19).expect("valid exact ledger");
        exact.begin_attempt();
        let mut exact_arena = Vec::new();
        let receipt = tab
            .append_codec_pane_arena_in_window_with_census_ledger(
                9,
                "ledger-workspace",
                &mut exact_arena,
                64,
                1,
                TEST_ORDERED_PANE_CENSUS_WORK,
                usize::MAX,
                &mut exact,
            )
            .expect("one leaf consumes exactly nineteen work units");
        assert_eq!(receipt.work.total(), Some(19));
        assert_eq!(receipt.work.tree_nodes, 2);
        assert_eq!(receipt.work.identity_checks, 9);
        assert_eq!(receipt.work.pane_callbacks, 7);
        assert_eq!(receipt.work.assembly_nodes, 1);
        assert_eq!(receipt.leaf_count, 1);
        assert_eq!(exact.attempt_stats().total(), Some(19));
        assert_eq!(exact.request_stats().total(), Some(19));
        assert_eq!(exact_arena.len(), 1);

        let mut short = PaneSnapshotCensusLedger::new(18, 18).expect("valid short ledger");
        short.begin_attempt();
        let mut rejected_arena = Vec::new();
        let error = tab
            .append_codec_pane_arena_in_window_with_census_ledger(
                9,
                "ledger-workspace",
                &mut rejected_arena,
                64,
                1,
                TEST_ORDERED_PANE_CENSUS_WORK,
                usize::MAX,
                &mut short,
            )
            .expect_err("limit plus one must fail before arena publication");
        assert!(format!("{error:#}").contains("attempt census work budget exhausted"));
        assert!(rejected_arena.is_empty());
        assert_eq!(short.attempt_stats().total(), Some(18));
        assert_eq!(short.request_stats().total(), Some(18));
        assert_eq!(
            short.last_rejection(),
            Some(PaneSnapshotCensusRejection::AttemptLimit)
        );
    }

    #[test]
    fn pane_snapshot_leaf_limit_rejects_before_any_pane_callback() {
        let size = TerminalSize::default();
        let callback_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_count_for_probe = Arc::clone(&callback_count);
        let probe: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            callback_count_for_probe.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        });
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_callback_probe(
            7_111, size, false, false, probe,
        ));
        callback_count.store(0, std::sync::atomic::Ordering::Release);
        let mut ledger = PaneSnapshotCensusLedger::new(64, 64).expect("valid leaf-limit ledger");
        ledger.begin_attempt();
        let mut arena = Vec::new();

        let error = tab
            .append_codec_pane_arena_in_window_with_census_ledger(
                9,
                "leaf-limit-workspace",
                &mut arena,
                64,
                1,
                TEST_ORDERED_PANE_CENSUS_WORK,
                0,
                &mut ledger,
            )
            .expect_err("one leaf must exceed a zero-leaf tab allowance");

        assert_eq!(
            error.downcast_ref::<PaneSnapshotStructureRejection>(),
            Some(&PaneSnapshotStructureRejection::TreeLeafLimit { count: 1, max: 0 })
        );
        assert_eq!(callback_count.load(std::sync::atomic::Ordering::Acquire), 0);
        assert!(arena.is_empty());
        assert_eq!(ledger.attempt_stats().pane_callbacks, 0);
    }

    #[test]
    fn pane_snapshot_metadata_ledger_preserves_exact_utf8_and_framing() {
        let limits = PaneSnapshotMetadataLimits::new([4; 7], 16, 32, 32, 64);
        let mut ledger = PaneSnapshotMetadataLedger::new(limits).expect("valid metadata limits");
        ledger.begin_attempt();

        let title_value = "éé".to_string();
        ledger
            .admit_required_owned(
                PaneSnapshotMetadataField::PaneTitle,
                &title_value,
                title_value.capacity(),
            )
            .expect("four UTF-8 bytes fit the exact field limit");
        ledger
            .admit_optional_none(PaneSnapshotMetadataField::PaneTtyName)
            .expect("absent optional field still charges its tag");
        let empty = String::new();
        ledger
            .admit_required_owned(
                PaneSnapshotMetadataField::WindowTitle,
                &empty,
                empty.capacity(),
            )
            .expect("an empty required string still charges its length prefix");
        ledger
            .admit_optional_owned(
                PaneSnapshotMetadataField::PaneWorkingDir,
                &empty,
                empty.capacity(),
            )
            .expect("an empty present optional string charges tag and length prefix");
        let title = ledger
            .attempt_stats()
            .field(PaneSnapshotMetadataField::PaneTitle);
        assert_eq!(title.values, 1);
        assert_eq!(title.retained_bytes, 4);
        assert_eq!(title.encoded_bytes, 5);
        let tty = ledger
            .attempt_stats()
            .field(PaneSnapshotMetadataField::PaneTtyName);
        assert_eq!(tty.values, 1);
        assert_eq!(tty.retained_bytes, 0);
        assert_eq!(tty.encoded_bytes, 1);
        let empty_required = ledger
            .attempt_stats()
            .field(PaneSnapshotMetadataField::WindowTitle);
        assert_eq!(empty_required.retained_bytes, 0);
        assert_eq!(empty_required.encoded_bytes, 1);
        let empty_optional = ledger
            .attempt_stats()
            .field(PaneSnapshotMetadataField::PaneWorkingDir);
        assert_eq!(empty_optional.retained_bytes, 0);
        assert_eq!(empty_optional.encoded_bytes, 2);
        assert_eq!(ledger.attempt_stats().total().unwrap().encoded_bytes, 9);
        let admitted = ledger.take_unreported_admitted_values();
        assert_eq!(
            admitted[PaneSnapshotMetadataField::PaneTitle.index()],
            (PaneSnapshotMetadataField::PaneTitle, 1)
        );
        assert_eq!(
            admitted[PaneSnapshotMetadataField::PaneTtyName.index()],
            (PaneSnapshotMetadataField::PaneTtyName, 1)
        );
        assert!(ledger
            .take_unreported_admitted_values()
            .iter()
            .all(|(_, values)| *values == 0));

        let before = ledger.attempt_stats();
        let rejected = "xxxxx".to_string();
        let error = ledger
            .admit_required_owned(
                PaneSnapshotMetadataField::PaneTitle,
                &rejected,
                rejected.capacity(),
            )
            .expect_err("field limit plus one must fail");
        assert_eq!(
            error,
            PaneSnapshotMetadataRejection::FieldLimit {
                field: "pane_title"
            }
        );
        assert_eq!(ledger.attempt_stats(), before);
        assert!(!error.to_string().contains(&rejected));
    }

    #[test]
    fn pane_snapshot_metadata_field_and_varint_boundaries_are_exact() {
        let very_long = "long-metadata".repeat(80_000);
        for field in PaneSnapshotMetadataField::ALL {
            let limits = PaneSnapshotMetadataLimits::new([8; 7], 64, 256, 64, 256);
            let mut ledger =
                PaneSnapshotMetadataLedger::new(limits).expect("field limits are valid");
            ledger.begin_attempt();
            let exact = "12345678".to_string();
            ledger
                .preflight_field(field, &exact)
                .unwrap_or_else(|error| panic!("{} exact limit failed: {error}", field.label()));
            let plus_one = "123456789";
            let error = ledger
                .preflight_field(field, plus_one)
                .expect_err("field limit plus one must fail");
            assert_eq!(
                error,
                PaneSnapshotMetadataRejection::FieldLimit {
                    field: field.label()
                }
            );
            assert!(!error.to_string().contains(plus_one));
            let very_long_error = ledger
                .preflight_field(field, &very_long)
                .expect_err("very long authority-bearing metadata must fail finitely");
            assert_eq!(
                very_long_error,
                PaneSnapshotMetadataRejection::FieldLimit {
                    field: field.label()
                }
            );
            assert!(very_long_error.to_string().len() < 128);
        }

        let mut ledger = PaneSnapshotMetadataLedger::new(PaneSnapshotMetadataLimits::new(
            [128; 7], 256, 512, 256, 512,
        ))
        .expect("varint boundary limits are valid");
        ledger.begin_attempt();
        let below = "x".repeat(127);
        ledger
            .admit_required_owned(
                PaneSnapshotMetadataField::PaneTitle,
                &below,
                below.capacity(),
            )
            .expect("127-byte string fits");
        assert_eq!(
            ledger
                .attempt_stats()
                .field(PaneSnapshotMetadataField::PaneTitle)
                .encoded_bytes,
            128
        );
        let at = "y".repeat(128);
        ledger
            .admit_optional_owned(PaneSnapshotMetadataField::PaneTtyName, &at, at.capacity())
            .expect("128-byte optional string fits");
        assert_eq!(
            ledger
                .attempt_stats()
                .field(PaneSnapshotMetadataField::PaneTtyName)
                .encoded_bytes,
            131,
            "two-byte varint length plus payload plus option tag"
        );
    }

    #[test]
    fn pane_snapshot_metadata_ledger_bounds_attempts_requests_and_overflow() {
        let limits = PaneSnapshotMetadataLimits::new([16; 7], 4, 8, 7, 16);
        let mut ledger = PaneSnapshotMetadataLedger::new(limits).expect("valid retry limits");
        ledger.begin_attempt();
        let first = "1234".to_string();
        ledger
            .admit_required_owned(
                PaneSnapshotMetadataField::WindowTitle,
                &first,
                first.capacity(),
            )
            .expect("first exact attempt fits");
        assert_eq!(ledger.attempt_stats().total().unwrap().retained_bytes, 4);
        let released = ledger
            .release_attempt_to(Default::default())
            .expect("failed-attempt ownership releases to its empty checkpoint");
        assert_eq!(released.retained_bytes, 4);
        assert_eq!(ledger.take_retry_released_bytes(), 4);
        assert_eq!(ledger.attempt_stats(), PaneSnapshotMetadataStats::default());
        assert_eq!(ledger.request_stats().total().unwrap().retained_bytes, 4);
        ledger.begin_attempt();
        let prior_request = ledger.request_stats();
        let second = "5678".to_string();
        let error = ledger
            .admit_required_owned(
                PaneSnapshotMetadataField::WindowTitle,
                &second,
                second.capacity(),
            )
            .expect_err("retry must not receive a fresh request allowance");
        assert_eq!(error, PaneSnapshotMetadataRejection::RequestRetainedLimit);
        assert_eq!(ledger.attempt_stats(), PaneSnapshotMetadataStats::default());
        assert_eq!(ledger.request_stats(), prior_request);

        let mut overflow = PaneSnapshotMetadataLedger::new(PaneSnapshotMetadataLimits::new(
            [usize::MAX; 7],
            usize::MAX,
            usize::MAX,
            usize::MAX,
            usize::MAX,
        ))
        .expect("maximum checked metadata limits are valid");
        overflow.begin_attempt();
        overflow
            .admit(
                PaneSnapshotMetadataField::PaneTitle,
                usize::MAX,
                usize::MAX,
                usize::MAX,
            )
            .expect("exact maximum first admission fits");
        let error = overflow
            .admit(PaneSnapshotMetadataField::PaneTitle, 1, 1, 1)
            .expect_err("value-count and byte totals must not wrap");
        assert_eq!(error, PaneSnapshotMetadataRejection::ArithmeticOverflow);
        assert_eq!(
            overflow.last_rejection(),
            Some(PaneSnapshotMetadataRejection::ArithmeticOverflow)
        );
    }

    #[test]
    fn pane_snapshot_metadata_cumulative_exact_and_plus_one_are_atomic() {
        let mut ledger =
            PaneSnapshotMetadataLedger::new(PaneSnapshotMetadataLimits::new([8; 7], 8, 10, 8, 10))
                .expect("exact cumulative limits are valid");
        ledger.begin_attempt();
        let first = "1234".to_string();
        let second = "5678".to_string();
        ledger
            .admit_required_owned(
                PaneSnapshotMetadataField::PaneTitle,
                &first,
                first.capacity(),
            )
            .expect("first half fits");
        ledger
            .admit_required_owned(
                PaneSnapshotMetadataField::WindowTitle,
                &second,
                second.capacity(),
            )
            .expect("exact aggregate retained and encoded boundary fits");
        let exact = ledger.attempt_stats();
        assert_eq!(exact.total().unwrap().retained_bytes, 8);
        assert_eq!(exact.total().unwrap().encoded_bytes, 10);

        let preflight_error = ledger
            .preflight_required_string(PaneSnapshotMetadataField::TabTitle, "x")
            .expect_err("minimum aggregate boundary plus one must reject before cloning");
        assert_eq!(
            preflight_error,
            PaneSnapshotMetadataRejection::AttemptRetainedLimit
        );
        assert_eq!(ledger.attempt_stats(), exact);
        let plus_one = "x".to_string();
        let error = ledger
            .admit_required_owned(
                PaneSnapshotMetadataField::TabTitle,
                &plus_one,
                plus_one.capacity(),
            )
            .expect_err("aggregate boundary plus one must fail atomically");
        assert_eq!(error, PaneSnapshotMetadataRejection::AttemptRetainedLimit);
        assert_eq!(ledger.attempt_stats(), exact);
        assert_eq!(ledger.request_stats(), exact);
    }

    #[test]
    fn pane_snapshot_metadata_encoded_overhead_reaches_exact_producer_boundary() {
        const ATTEMPT_BYTES: usize = 4 * 1024 * 1024;
        const FULL_VALUE_BYTES: usize = 64 * 1024;
        const FULL_VALUE_ENCODED_BYTES: usize = FULL_VALUE_BYTES + 3;
        const FULL_VALUE_COUNT: usize = 63;
        const TAIL_VALUE_BYTES: usize = 65_344;
        const TAIL_VALUE_ENCODED_BYTES: usize = TAIL_VALUE_BYTES + 3;
        const _: () = assert!(
            FULL_VALUE_COUNT * FULL_VALUE_ENCODED_BYTES + TAIL_VALUE_ENCODED_BYTES == ATTEMPT_BYTES
        );

        let mut ledger = PaneSnapshotMetadataLedger::new(PaneSnapshotMetadataLimits::new(
            [FULL_VALUE_BYTES; PaneSnapshotMetadataField::COUNT],
            ATTEMPT_BYTES,
            ATTEMPT_BYTES,
            ATTEMPT_BYTES,
            ATTEMPT_BYTES,
        ))
        .expect("production-sized exact encoded boundary is valid");
        ledger.begin_attempt();
        let mut owners = Vec::new();
        owners
            .try_reserve_exact(FULL_VALUE_COUNT + 1)
            .expect("reserve bounded metadata owners");
        for _ in 0..FULL_VALUE_COUNT {
            let value = "x".repeat(FULL_VALUE_BYTES);
            ledger
                .admit_required_owned(
                    PaneSnapshotMetadataField::PaneTitle,
                    &value,
                    value.capacity(),
                )
                .expect("full field fits before the exact aggregate boundary");
            owners.push(value);
        }
        let tail = "y".repeat(TAIL_VALUE_BYTES);
        ledger
            .admit_required_owned(PaneSnapshotMetadataField::PaneTitle, &tail, tail.capacity())
            .expect("tail framing reaches the exact aggregate encoded boundary");
        owners.push(tail);

        let usage = ledger.attempt_stats().total().expect("bounded total");
        assert_eq!(usage.values, FULL_VALUE_COUNT + 1);
        assert_eq!(usage.encoded_bytes, ATTEMPT_BYTES);
        assert!(usage.retained_bytes < ATTEMPT_BYTES);
        assert_eq!(ledger.attempt_peak_encoded_bytes(), ATTEMPT_BYTES);
        let before = ledger.attempt_stats();
        assert_eq!(
            ledger
                .preflight_optional_value(PaneSnapshotMetadataField::PaneTtyName)
                .expect_err("one option-tag byte beyond the boundary must fail"),
            PaneSnapshotMetadataRejection::AttemptEncodedLimit
        );
        assert_eq!(ledger.attempt_stats(), before);
        assert_eq!(owners.len(), FULL_VALUE_COUNT + 1);
    }

    #[test]
    fn pane_snapshot_metadata_ledger_accounts_owned_capacity_and_temporary_release() {
        let limits = PaneSnapshotMetadataLimits::new([16; 7], 8, 32, 32, 64);
        let mut ledger = PaneSnapshotMetadataLedger::new(limits).expect("valid capacity limits");
        ledger.begin_attempt();

        let mut workspace = String::with_capacity(8);
        workspace.push_str("four");
        assert_eq!(workspace.len(), 4);
        assert_eq!(workspace.capacity(), 8);
        ledger
            .preflight_field(PaneSnapshotMetadataField::WindowWorkspace, &workspace)
            .expect("logical bytes fit the field ceiling");
        ledger
            .admit_retained_only_owned(
                PaneSnapshotMetadataField::WindowWorkspace,
                &workspace,
                workspace.capacity(),
            )
            .expect("actual allocation capacity fits the attempt ceiling");
        assert_eq!(ledger.attempt_stats().total().unwrap().retained_bytes, 8);
        assert_eq!(ledger.request_stats().total().unwrap().retained_bytes, 8);
        assert_eq!(ledger.attempt_peak_retained_bytes(), 8);
        assert_eq!(ledger.attempt_peak_encoded_bytes(), 0);

        ledger
            .release_retained_only(
                PaneSnapshotMetadataField::WindowWorkspace,
                workspace.capacity(),
            )
            .expect("temporary source retention releases exactly once");
        assert_eq!(ledger.attempt_stats().total().unwrap().retained_bytes, 0);
        assert_eq!(ledger.request_stats().total().unwrap().retained_bytes, 8);
        assert_eq!(
            ledger.attempt_peak_retained_bytes(),
            8,
            "temporary release must not erase the observed live high-water"
        );

        let mut overallocated = String::with_capacity(9);
        overallocated.push_str("four");
        let error = ledger
            .admit_required_owned(
                PaneSnapshotMetadataField::PaneTitle,
                &overallocated,
                overallocated.capacity(),
            )
            .expect_err("owned capacity, not logical length, must drive retention rejection");
        assert_eq!(error, PaneSnapshotMetadataRejection::AttemptRetainedLimit);
        assert_eq!(ledger.attempt_stats().total().unwrap().retained_bytes, 0);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn pane_snapshot_metadata_utf8_accounting_matches_owned_generated_strings(
            chars in proptest::collection::vec(any::<char>(), 1..=128),
            extra_capacity in 0_usize..=64,
        ) {
            let logical = chars.into_iter().collect::<String>();
            let mut owned = String::with_capacity(
                logical
                    .len()
                    .checked_add(extra_capacity)
                    .expect("bounded generated capacity cannot overflow"),
            );
            owned.push_str(&logical);
            let retained = owned.capacity();
            let encoded = encoded_string_bytes(owned.len())
                .expect("bounded generated string encoding is representable");
            let limits = PaneSnapshotMetadataLimits::new(
                [owned.len(); PaneSnapshotMetadataField::COUNT],
                retained.max(1),
                encoded,
                retained.max(1),
                encoded,
            );
            let mut ledger = PaneSnapshotMetadataLedger::new(limits)
                .expect("generated metadata limits are valid");
            ledger.begin_attempt();
            ledger
                .preflight_field(PaneSnapshotMetadataField::PaneTitle, &owned)
                .expect("exact generated UTF-8 field limit is admitted");
            ledger
                .admit_required_owned(
                    PaneSnapshotMetadataField::PaneTitle,
                    &owned,
                    owned.capacity(),
                )
                .expect("exact generated retained and encoded limits are admitted");
            let usage = ledger
                .attempt_stats()
                .field(PaneSnapshotMetadataField::PaneTitle);
            prop_assert_eq!(usage.values, 1);
            prop_assert_eq!(usage.retained_bytes, retained);
            prop_assert_eq!(usage.encoded_bytes, encoded);

            let mut plus_one = owned.clone();
            plus_one.push('x');
            let prior = ledger.attempt_stats();
            let rejection = ledger
                .preflight_field(PaneSnapshotMetadataField::PaneTitle, &plus_one)
                .expect_err("logical UTF-8 limit plus one must fail before admission");
            prop_assert_eq!(
                rejection,
                PaneSnapshotMetadataRejection::FieldLimit { field: "pane_title" }
            );
            prop_assert_eq!(ledger.attempt_stats(), prior);
            prop_assert!(!rejection.to_string().contains(&plus_one));
        }
    }

    #[test]
    fn ordered_snapshot_metadata_rejection_stops_later_pane_callbacks() {
        let size = TerminalSize::default();
        let later_callbacks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let later_callbacks_for_probe = Arc::clone(&later_callbacks);
        let probe: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            later_callbacks_for_probe.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        });
        let rejected_title = "secret-title".to_string();
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_title_and_later_callback_probe(
            7_201,
            size,
            rejected_title.clone(),
            probe,
        ));
        let mut census = PaneSnapshotCensusLedger::new(64, 64).expect("valid census ledger");
        census.begin_attempt();
        let mut metadata = PaneSnapshotMetadataLedger::new(PaneSnapshotMetadataLimits::new(
            [128, 128, 128, 4, 128, 128, 128],
            512,
            512,
            512,
            512,
        ))
        .expect("valid narrow pane-title limit");
        metadata.begin_attempt();
        let mut arena = Vec::new();

        let error = tab
            .append_codec_pane_arena_in_window_with_ledgers(
                9,
                "ledger-workspace",
                &mut arena,
                64,
                1,
                TEST_ORDERED_PANE_CENSUS_WORK,
                usize::MAX,
                &mut census,
                &mut metadata,
            )
            .expect_err("oversized title must fail before dimensions and later getters");

        assert!(arena.is_empty());
        assert_eq!(
            later_callbacks.load(std::sync::atomic::Ordering::Acquire),
            0
        );
        assert_eq!(census.attempt_stats().pane_callbacks, 2);
        assert_eq!(
            metadata.last_rejection(),
            Some(PaneSnapshotMetadataRejection::FieldLimit {
                field: "pane_title"
            })
        );
        assert!(!format!("{error:#}").contains(&rejected_title));
    }

    #[test]
    fn ordered_snapshot_metadata_exhausted_aggregate_stops_next_getter() {
        let size = TerminalSize::default();
        let title_panic_armed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_ordered_observation_panic(
            7_203,
            size,
            OrderedObservationCallback::Title,
            title_panic_armed,
        ));
        let mut census = PaneSnapshotCensusLedger::new(64, 64).expect("valid census ledger");
        census.begin_attempt();
        let mut metadata = PaneSnapshotMetadataLedger::new(PaneSnapshotMetadataLimits::new(
            [128; PaneSnapshotMetadataField::COUNT],
            512,
            2,
            512,
            2,
        ))
        .expect("valid exact encoded-byte limit");
        metadata.begin_attempt();
        let mut arena = Vec::new();

        let error = tab
            .append_codec_pane_arena_in_window_with_ledgers(
                9,
                "w",
                &mut arena,
                64,
                1,
                TEST_ORDERED_PANE_CENSUS_WORK,
                usize::MAX,
                &mut census,
                &mut metadata,
            )
            .expect_err("workspace framing must exhaust the encoded-byte authority");

        assert!(arena.is_empty());
        assert_eq!(census.attempt_stats().pane_callbacks, 1);
        assert_eq!(
            metadata.last_rejection(),
            Some(PaneSnapshotMetadataRejection::AttemptEncodedLimit)
        );
        assert!(format!("{error:#}").contains("encoded-metadata byte budget exhausted"));
    }

    #[test]
    fn ordered_snapshot_cwd_rejection_stops_all_later_pane_callbacks() {
        let size = TerminalSize::default();
        let later_callback_armed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let rejected_cwd = Url::parse("file:///secret-working-directory")
            .expect("test working directory is a valid URL");
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_working_dir_and_later_callback_panic(
            7_202,
            size,
            rejected_cwd.clone(),
            later_callback_armed,
        ));
        let mut census = PaneSnapshotCensusLedger::new(64, 64).expect("valid census ledger");
        census.begin_attempt();
        let mut per_field = [128; PaneSnapshotMetadataField::COUNT];
        per_field[PaneSnapshotMetadataField::PaneWorkingDir.index()] = 8;
        let mut metadata = PaneSnapshotMetadataLedger::new(PaneSnapshotMetadataLimits::new(
            per_field, 512, 512, 512, 512,
        ))
        .expect("valid narrow cwd limit");
        metadata.begin_attempt();
        let mut arena = Vec::new();

        let error = tab
            .append_codec_pane_arena_in_window_with_ledgers(
                9,
                "ledger-workspace",
                &mut arena,
                64,
                1,
                TEST_ORDERED_PANE_CENSUS_WORK,
                usize::MAX,
                &mut census,
                &mut metadata,
            )
            .expect_err("oversized cwd must fail before alt-screen and later getters");

        assert!(arena.is_empty());
        assert_eq!(census.attempt_stats().pane_callbacks, 4);
        assert_eq!(
            metadata.last_rejection(),
            Some(PaneSnapshotMetadataRejection::FieldLimit {
                field: "pane_working_dir"
            })
        );
        assert!(!format!("{error:#}").contains(rejected_cwd.as_str()));
    }

    #[test]
    fn pane_snapshot_census_ledger_rejects_before_unadmitted_callbacks() {
        let size = TerminalSize::default();
        let callback_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_count_for_probe = Arc::clone(&callback_count);
        let probe: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            callback_count_for_probe.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        });
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_callback_probe(
            72, size, false, false, probe,
        ));
        let mut ledger = PaneSnapshotCensusLedger::new(10, 10).expect("valid callback ledger");
        ledger.begin_attempt();
        let mut arena = Vec::new();

        tab.append_codec_pane_arena_in_window_with_census_ledger(
            9,
            "ledger-workspace",
            &mut arena,
            64,
            1,
            TEST_ORDERED_PANE_CENSUS_WORK,
            usize::MAX,
            &mut ledger,
        )
        .expect_err("the full callback bundle must be admitted before its first callback");

        assert_eq!(callback_count.load(std::sync::atomic::Ordering::Acquire), 0);
        assert!(arena.is_empty());
        assert_eq!(
            ledger.last_rejection(),
            Some(PaneSnapshotCensusRejection::AttemptLimit)
        );
        assert_eq!(ledger.callbacks_avoided(), 7);
    }

    #[test]
    fn pane_snapshot_census_ledger_bounds_retries_and_checked_overflow() {
        let size = TerminalSize::default();
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(73, size));
        let mut ledger = PaneSnapshotCensusLedger::new(19, 37).expect("valid retry ledger");

        for attempt in 0..2 {
            ledger.begin_attempt();
            let mut arena = Vec::new();
            let result = tab.append_codec_pane_arena_in_window_with_census_ledger(
                9,
                "ledger-workspace",
                &mut arena,
                64,
                1,
                TEST_ORDERED_PANE_CENSUS_WORK,
                usize::MAX,
                &mut ledger,
            );
            if attempt == 0 {
                result.expect("first exact attempt fits request ledger");
            } else {
                let error = result.expect_err("second attempt exceeds aggregate request budget");
                assert!(format!("{error:#}").contains("request census work budget exhausted"));
                assert!(arena.is_empty());
                assert_eq!(
                    ledger.last_rejection(),
                    Some(PaneSnapshotCensusRejection::RequestLimit)
                );
                assert_eq!(ledger.attempt_stats().total(), Some(18));
                assert_eq!(ledger.request_stats().total(), Some(37));
            }
        }

        let mut overflow = PaneSnapshotCensusLedger::new(usize::MAX, usize::MAX)
            .expect("maximum checked ledger is valid");
        overflow.begin_attempt();
        overflow
            .reserve_pane_callbacks(usize::MAX)
            .expect("exact maximum reservation succeeds");
        let error = overflow
            .reserve_pane_callbacks(1)
            .expect_err("maximum plus one must fail checked");
        assert!(format!("{error:#}").contains("attempt census work overflow"));
        assert_eq!(
            overflow.last_rejection(),
            Some(PaneSnapshotCensusRejection::AttemptOverflow)
        );
    }

    #[test]
    fn ordered_snapshot_style_shared_arena_uses_one_cross_tab_ledger() {
        let size = TerminalSize::default();
        let first = Tab::new(&size);
        first.assign_pane(&FakePane::new(74, size));
        let second = Tab::new(&size);
        second.assign_pane(&FakePane::new(75, size));

        let mut exact = PaneSnapshotCensusLedger::new(38, 38).expect("valid two-tab ledger");
        exact.begin_attempt();
        let mut exact_arena = Vec::new();
        for tab in [&first, &second] {
            tab.append_codec_pane_arena_in_window_with_census_ledger(
                10,
                "ordered-ledger-workspace",
                &mut exact_arena,
                64,
                2,
                TEST_ORDERED_PANE_CENSUS_WORK,
                usize::MAX,
                &mut exact,
            )
            .expect("both ordered-style tabs fit the exact shared budget");
        }
        assert_eq!(exact_arena.len(), 2);
        assert_eq!(exact.attempt_stats().total(), Some(38));

        let mut short = PaneSnapshotCensusLedger::new(37, 37).expect("valid short two-tab ledger");
        short.begin_attempt();
        let mut short_arena = Vec::new();
        first
            .append_codec_pane_arena_in_window_with_census_ledger(
                10,
                "ordered-ledger-workspace",
                &mut short_arena,
                64,
                2,
                TEST_ORDERED_PANE_CENSUS_WORK,
                usize::MAX,
                &mut short,
            )
            .expect("first ordered-style tab fits");
        let prefix = short_arena.clone();
        second
            .append_codec_pane_arena_in_window_with_census_ledger(
                10,
                "ordered-ledger-workspace",
                &mut short_arena,
                64,
                2,
                TEST_ORDERED_PANE_CENSUS_WORK,
                usize::MAX,
                &mut short,
            )
            .expect_err("second ordered-style tab cannot reset the shared budget");
        assert_eq!(
            short_arena, prefix,
            "failed tab append must preserve its arena prefix"
        );
    }

    #[test]
    fn pane_snapshot_census_ledger_bounds_sixteen_thousand_one_leaf_tabs() {
        const TAB_COUNT: usize = 16_384;
        const WORK_PER_TAB: usize = 19;

        let size = TerminalSize::default();
        let exact_work = TAB_COUNT
            .checked_mul(WORK_PER_TAB)
            .expect("large-session census work fits usize");
        let mut ledger =
            PaneSnapshotCensusLedger::new(exact_work, exact_work).expect("valid scale ledger");
        ledger.begin_attempt();
        let mut arena = Vec::new();
        arena
            .try_reserve_exact(TAB_COUNT)
            .expect("reserve bounded scale-test arena");

        for pane_id in 0..TAB_COUNT {
            let tab = Tab::new(&size);
            tab.assign_pane(&FakePane::new(100_000 + pane_id, size));
            tab.append_codec_pane_arena_in_window_with_census_ledger(
                11,
                "large-session-ledger-workspace",
                &mut arena,
                64,
                TAB_COUNT,
                TEST_ORDERED_PANE_CENSUS_WORK,
                usize::MAX,
                &mut ledger,
            )
            .unwrap_or_else(|error| {
                panic!(
                    "one-leaf tab {} exceeded the aggregate budget: {:#}",
                    pane_id, error
                )
            });
        }

        assert_eq!(arena.len(), TAB_COUNT);
        assert_eq!(ledger.attempt_stats().total(), Some(exact_work));
        assert_eq!(ledger.request_stats().total(), Some(exact_work));
        assert_eq!(ledger.last_rejection(), None);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn pane_snapshot_census_work_is_linear_across_generated_one_leaf_tabs(
            tab_count in 1_usize..=128,
        ) {
            const WORK_PER_TAB: usize = 19;

            let size = TerminalSize::default();
            let exact_work = tab_count * WORK_PER_TAB;
            let mut ledger = PaneSnapshotCensusLedger::new(exact_work, exact_work)
                .expect("generated ledger limits are valid");
            ledger.begin_attempt();
            let mut arena = Vec::new();

            for pane_id in 0..tab_count {
                let tab = Tab::new(&size);
                tab.assign_pane(&FakePane::new(200_000 + pane_id, size));
                let result = tab.append_codec_pane_arena_in_window_with_census_ledger(
                    12,
                    "generated-ledger-workspace",
                    &mut arena,
                    64,
                    tab_count,
                    TEST_ORDERED_PANE_CENSUS_WORK,
                    usize::MAX,
                    &mut ledger,
                );
                prop_assert!(
                    result.is_ok(),
                    "generated one-leaf tab {pane_id} failed: {:#}",
                    result.expect_err("failed result carries its error")
                );
            }

            prop_assert_eq!(arena.len(), tab_count);
            prop_assert_eq!(ledger.attempt_stats().total(), Some(exact_work));
            prop_assert_eq!(ledger.request_stats().total(), Some(exact_work));
            prop_assert_eq!(ledger.last_rejection(), None);
        }
    }

    fn flatten_legacy_pane_node_for_test(node: &PaneNode, arena: &mut Vec<PaneArenaNode>) -> u32 {
        let index = u32::try_from(arena.len()).expect("test arena fits u32");
        arena.push(PaneArenaNode::Empty);
        arena[usize::try_from(index).expect("u32 fits usize")] = match node {
            PaneNode::Empty => PaneArenaNode::Empty,
            PaneNode::Leaf(entry) => PaneArenaNode::Leaf(entry.clone()),
            PaneNode::Split { left, right, node } => {
                let left = flatten_legacy_pane_node_for_test(left, arena);
                let right = flatten_legacy_pane_node_for_test(right, arena);
                PaneArenaNode::Split {
                    left,
                    right,
                    node: *node,
                }
            }
        };
        index
    }

    #[test]
    fn flat_codec_snapshot_exactly_matches_legacy_recursive_projection() {
        let size = TerminalSize {
            rows: 36,
            cols: 120,
            pixel_width: 1_200,
            pixel_height: 720,
            dpi: 110,
        };
        let tab = Tab::new(&size);
        tab.set_title("flat-equivalence");
        tab.assign_pane(&FakePane::new(301, size));
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                target_is_second: true,
                top_level: false,
                size: SplitSize::Percent(40),
            },
            FakePane::new(302, size),
        )
        .expect("first test split");
        tab.split_and_insert(
            1,
            SplitRequest {
                direction: SplitDirection::Vertical,
                target_is_second: false,
                top_level: false,
                size: SplitSize::Percent(35),
            },
            FakePane::new(303, size),
        )
        .expect("second test split");
        tab.set_active_idx(2);
        assert!(!tab.set_zoomed(true), "set_zoomed returns the prior state");
        assert_eq!(tab.get_zoomed_pane().map(|pane| pane.pane_id()), Some(302));

        let legacy = tab
            .codec_pane_tree_in_window(77, "equivalence-workspace")
            .expect("legacy recursive projection");
        let mut expected_nodes = Vec::new();
        let expected_root = flatten_legacy_pane_node_for_test(&legacy, &mut expected_nodes);

        let mut actual_nodes = Vec::new();
        let actual_tree = tab
            .append_codec_pane_arena_in_window(
                77,
                "equivalence-workspace",
                &mut actual_nodes,
                64,
                1_024,
                TEST_ORDERED_PANE_CENSUS_WORK,
            )
            .expect("direct flat projection");

        assert_eq!(actual_tree.root_index, Some(expected_root));
        assert_eq!(
            usize::try_from(actual_tree.node_count).expect("test node count fits usize"),
            expected_nodes.len()
        );
        assert_eq!(actual_tree.tab_title, "flat-equivalence");
        assert_eq!(actual_nodes, expected_nodes);
    }

    #[test]
    fn flat_codec_snapshot_offsets_indices_and_preserves_prefix() {
        let size = TerminalSize::default();
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(401, size));
        tab.split_and_insert(0, SplitRequest::default(), FakePane::new(402, size))
            .expect("test split");
        let prefix = PaneArenaNode::Leaf(pane_arena_test_entry(999, false, false));
        let mut arena = vec![prefix.clone()];

        let tree = tab
            .append_codec_pane_arena_in_window(
                88,
                "offset-workspace",
                &mut arena,
                64,
                16,
                TEST_ORDERED_PANE_CENSUS_WORK,
            )
            .expect("offset flat projection");

        assert_eq!(arena.first(), Some(&prefix));
        assert_eq!(tree.root_index, Some(1));
        assert_eq!(tree.node_count, 3);
        let PaneArenaNode::Split { left, right, .. } = &arena[1] else {
            panic!("two-pane projection must start with a split");
        };
        assert_eq!((*left, *right), (2, 3));
    }

    #[test]
    fn flat_codec_snapshot_census_is_invariant_to_arena_prefix_position() {
        let size = TerminalSize::default();
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(450, size));
        tab.add_floating_pane(
            FakePane::new(451, size),
            FloatingPaneRect {
                left: 0,
                top: 0,
                width: 1,
                height: 1,
            },
        )
        .expect("test floating pane");

        let mut empty_prefix = Vec::new();
        let first = tab
            .append_codec_pane_arena_in_window(96, "prefix-invariant", &mut empty_prefix, 64, 1, 2)
            .expect("tab must fit at the start of an arena");

        let prefix = PaneArenaNode::Leaf(pane_arena_test_entry(997, false, false));
        let mut later_prefix = vec![prefix.clone(); 9];
        let second = tab
            .append_codec_pane_arena_in_window(96, "prefix-invariant", &mut later_prefix, 64, 10, 2)
            .expect("the same tab must fit with the same remaining node budget");

        assert_eq!(first.node_count, 1);
        assert_eq!(second.node_count, 1);
        assert_eq!(empty_prefix.len(), 1);
        assert_eq!(later_prefix.len(), 10);
        assert!(later_prefix[..9].iter().all(|node| node == &prefix));
    }

    #[test]
    fn flat_codec_snapshot_rejects_resource_limits_without_extending_arena() {
        let size = TerminalSize::default();
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(501, size));
        tab.split_and_insert(0, SplitRequest::default(), FakePane::new(502, size))
            .expect("first test split");
        tab.split_and_insert(1, SplitRequest::default(), FakePane::new(503, size))
            .expect("second test split");
        let prefix = PaneArenaNode::Leaf(pane_arena_test_entry(998, false, false));

        let mut depth_limited = vec![prefix.clone()];
        let depth_error = tab
            .append_codec_pane_arena_in_window(
                89,
                "limited-workspace",
                &mut depth_limited,
                2,
                16,
                TEST_ORDERED_PANE_CENSUS_WORK,
            )
            .expect_err("depth-three tree must exceed a depth-two ceiling");
        assert_eq!(
            depth_error.downcast_ref::<PaneSnapshotStructureRejection>(),
            Some(&PaneSnapshotStructureRejection::TreeDepthLimit { count: 3, max: 2 })
        );
        assert_eq!(depth_limited.as_slice(), std::slice::from_ref(&prefix));

        let mut node_limited = vec![prefix.clone()];
        let node_error = tab
            .append_codec_pane_arena_in_window(
                89,
                "limited-workspace",
                &mut node_limited,
                64,
                5,
                TEST_ORDERED_PANE_CENSUS_WORK,
            )
            .expect_err("five tree nodes plus one prefix must exceed a five-node ceiling");
        assert_eq!(
            node_error.downcast_ref::<PaneSnapshotStructureRejection>(),
            Some(&PaneSnapshotStructureRejection::TreeNodeLimit { count: 5, max: 4 })
        );
        assert_eq!(node_limited, [prefix]);
    }

    #[test]
    fn flat_codec_snapshot_rejects_overdepth_before_pane_callbacks_or_arena_growth() {
        let size = TerminalSize::default();
        let callback_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_probe: Arc<dyn Fn() + Send + Sync> = {
            let callback_count = Arc::clone(&callback_count);
            Arc::new(move || {
                callback_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            })
        };
        let mut tree = Tree::Leaf(FakePane::new_with_pane_id_probe(
            700,
            size,
            Arc::clone(&callback_probe),
        ));
        for pane_id in 701..=765 {
            tree = Tree::Node {
                left: Box::new(tree),
                right: Box::new(Tree::Leaf(FakePane::new_with_pane_id_probe(
                    pane_id,
                    size,
                    Arc::clone(&callback_probe),
                ))),
                data: Some(SplitDirectionAndSize {
                    direction: SplitDirection::Horizontal,
                    first: size,
                    second: size,
                }),
            };
        }
        let tab = Tab::new(&size);
        tab.inner.lock().pane = Some(tree);

        let prefix = PaneArenaNode::Leaf(pane_arena_test_entry(996, false, false));
        let mut arena = vec![prefix.clone()];
        let prior_capacity = arena.capacity();
        let error = tab
            .append_codec_pane_arena_in_window(
                91,
                "overdepth-workspace",
                &mut arena,
                64,
                256,
                TEST_ORDERED_PANE_CENSUS_WORK,
            )
            .expect_err("depth-66 tree must fail during callback-free admission");

        assert!(format!("{error:#}").contains("depth 65"));
        assert_eq!(arena, [prefix]);
        assert_eq!(arena.capacity(), prior_capacity);
        assert_eq!(
            callback_count.load(std::sync::atomic::Ordering::Acquire),
            0,
            "an overdepth tree must be rejected before Pane::pane_id"
        );
    }

    #[test]
    fn flat_codec_snapshot_accepts_exact_depth_and_node_boundaries() {
        let size = TerminalSize::default();
        let mut tree = Tree::Leaf(FakePane::new(1_000, size));
        for pane_id in 1_001..=1_063 {
            tree = Tree::Node {
                left: Box::new(tree),
                right: Box::new(Tree::Leaf(FakePane::new(pane_id, size))),
                data: Some(SplitDirectionAndSize {
                    direction: SplitDirection::Horizontal,
                    first: size,
                    second: size,
                }),
            };
        }
        let tab = Tab::new(&size);
        tab.inner.lock().pane = Some(tree);
        let mut arena = Vec::new();

        let descriptor = tab
            .append_codec_pane_arena_in_window(95, "exact-depth", &mut arena, 64, 127, 127)
            .expect("a depth-64, 127-node tree must fit exact declared limits");

        assert_eq!(descriptor.root_index, Some(0));
        assert_eq!(descriptor.node_count, 127);
        assert_eq!(arena.len(), 127);
    }

    #[test]
    fn flat_codec_snapshot_bounds_hidden_floating_and_zoom_census_before_callbacks() {
        let size = TerminalSize::default();
        let callback_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_probe: Arc<dyn Fn() + Send + Sync> = {
            let callback_count = Arc::clone(&callback_count);
            Arc::new(move || {
                callback_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            })
        };
        let visible = FakePane::new_with_pane_id_probe(800, size, Arc::clone(&callback_probe));
        let hidden = FakePane::new_with_pane_id_probe(801, size, Arc::clone(&callback_probe));
        let floating = FakePane::new_with_pane_id_probe(802, size, Arc::clone(&callback_probe));
        let tab = Tab::new(&size);
        {
            let mut inner = tab.inner.lock();
            inner.pane = Some(Tree::Leaf(Arc::clone(&visible)));
            inner.pane_stacks.insert(
                0,
                PaneStack::new(vec![Arc::clone(&visible), Arc::clone(&hidden)]),
            );
            inner.floating_panes.push(FloatingPane {
                pane: floating,
                pane_id: 802,
                rect: FloatingPaneRect {
                    left: 0,
                    top: 0,
                    width: 1,
                    height: 1,
                },
                z_order: 0,
                visible: true,
                pinned: false,
                opacity: 1.0,
            });
            inner.zoomed = Some(Arc::clone(&visible));
        }

        let prefix = PaneArenaNode::Leaf(pane_arena_test_entry(995, false, false));
        let mut arena = vec![prefix.clone()];
        let prior_capacity = arena.capacity();
        let error = tab
            .append_codec_pane_arena_in_window(92, "census-workspace", &mut arena, 64, 16, 5)
            .expect_err("six raw pane carriers must exceed a five-entry census");

        assert!(format!("{error:#}").contains("exceeds 5 carrier entries"));
        assert_eq!(arena, [prefix]);
        assert_eq!(arena.capacity(), prior_capacity);
        assert_eq!(
            callback_count.load(std::sync::atomic::Ordering::Acquire),
            0,
            "an oversized hidden/floating/zoom census must fail before Pane::pane_id"
        );
    }

    #[test]
    fn flat_codec_snapshot_rejects_duplicate_exact_and_numeric_pane_identities() {
        let size = TerminalSize::default();
        let split_data = || SplitDirectionAndSize {
            direction: SplitDirection::Horizontal,
            first: size,
            second: size,
        };

        let duplicate_exact = Tab::new(&size);
        let shared = FakePane::new(900, size);
        duplicate_exact.inner.lock().pane = Some(Tree::Node {
            left: Box::new(Tree::Leaf(Arc::clone(&shared))),
            right: Box::new(Tree::Leaf(shared)),
            data: Some(split_data()),
        });
        let prefix = PaneArenaNode::Leaf(pane_arena_test_entry(994, false, false));
        let mut exact_arena = vec![prefix.clone()];
        let exact_error = duplicate_exact
            .append_codec_pane_arena_in_window(
                93,
                "duplicate-exact-workspace",
                &mut exact_arena,
                64,
                16,
                TEST_ORDERED_PANE_CENSUS_WORK,
            )
            .expect_err("one exact pane identity cannot occupy two tree leaves");
        assert!(format!("{exact_error:#}").contains("exact pane identity appears more than once"));
        assert_eq!(exact_arena.as_slice(), std::slice::from_ref(&prefix));

        let duplicate_numeric = Tab::new(&size);
        duplicate_numeric.inner.lock().pane = Some(Tree::Node {
            left: Box::new(Tree::Leaf(FakePane::new(901, size))),
            right: Box::new(Tree::Leaf(FakePane::new(901, size))),
            data: Some(split_data()),
        });
        let mut numeric_arena = vec![prefix.clone()];
        let numeric_error = duplicate_numeric
            .append_codec_pane_arena_in_window(
                94,
                "duplicate-numeric-workspace",
                &mut numeric_arena,
                64,
                16,
                TEST_ORDERED_PANE_CENSUS_WORK,
            )
            .expect_err("one numeric pane id cannot identify two exact pane objects");
        assert!(format!("{numeric_error:#}")
            .contains("pane id 901 belongs to more than one exact pane identity"));
        assert_eq!(numeric_arena, [prefix]);
    }

    #[test]
    fn flat_codec_snapshot_rejects_raw_stack_and_floating_aliases_before_pane_ids() {
        let size = TerminalSize::default();
        let pane_id_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe: Arc<dyn Fn() + Send + Sync> = {
            let pane_id_calls = Arc::clone(&pane_id_calls);
            Arc::new(move || {
                pane_id_calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            })
        };

        let repeated_hidden = Tab::new(&size);
        let visible = FakePane::new_with_pane_id_probe(910, size, Arc::clone(&probe));
        let hidden = FakePane::new_with_pane_id_probe(911, size, Arc::clone(&probe));
        {
            let mut inner = repeated_hidden.inner.lock();
            inner.pane = Some(Tree::Leaf(Arc::clone(&visible)));
            inner.pane_stacks.insert(
                0,
                PaneStack::new(vec![visible, Arc::clone(&hidden), hidden]),
            );
        }
        let mut repeated_arena = Vec::new();
        let repeated_error = repeated_hidden
            .append_codec_pane_arena_in_window(
                97,
                "repeated-hidden",
                &mut repeated_arena,
                64,
                16,
                16,
            )
            .expect_err("one hidden exact identity cannot occupy two raw stack entries");
        assert!(format!("{repeated_error:#}").contains("aliases HiddenStack"));
        assert!(repeated_arena.is_empty());
        assert_eq!(pane_id_calls.load(std::sync::atomic::Ordering::Acquire), 0);

        let cross_owner = Tab::new(&size);
        let visible = FakePane::new_with_pane_id_probe(912, size, Arc::clone(&probe));
        let shared = FakePane::new_with_pane_id_probe(913, size, Arc::clone(&probe));
        {
            let mut inner = cross_owner.inner.lock();
            inner.pane = Some(Tree::Leaf(Arc::clone(&visible)));
            inner
                .pane_stacks
                .insert(0, PaneStack::new(vec![visible, Arc::clone(&shared)]));
            inner.floating_panes.push(FloatingPane {
                pane: shared,
                pane_id: 913,
                rect: FloatingPaneRect {
                    left: 0,
                    top: 0,
                    width: 1,
                    height: 1,
                },
                z_order: 0,
                visible: true,
                pinned: false,
                opacity: 1.0,
            });
        }
        let mut cross_arena = Vec::new();
        let cross_error = cross_owner
            .append_codec_pane_arena_in_window(98, "cross-owner", &mut cross_arena, 64, 16, 16)
            .expect_err("hidden and floating ownership cannot alias");
        assert!(format!("{cross_error:#}").contains("aliases HiddenStack"));
        assert!(cross_arena.is_empty());
        assert_eq!(pane_id_calls.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    #[test]
    fn flat_codec_snapshot_rejects_empty_stack_before_pane_ids() {
        let size = TerminalSize::default();
        let pane_id_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe: Arc<dyn Fn() + Send + Sync> = {
            let pane_id_calls = Arc::clone(&pane_id_calls);
            Arc::new(move || {
                pane_id_calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            })
        };
        let visible = FakePane::new_with_pane_id_probe(920, size, Arc::clone(&probe));
        let mut empty_stack = PaneStack::single(Arc::clone(&visible));
        assert!(empty_stack.remove(920).is_some());
        pane_id_calls.store(0, std::sync::atomic::Ordering::Release);
        let tab = Tab::new(&size);
        {
            let mut inner = tab.inner.lock();
            inner.pane = Some(Tree::Leaf(visible));
            inner.pane_stacks.insert(0, empty_stack);
        }
        let mut arena = Vec::new();
        let error = tab
            .append_codec_pane_arena_in_window(99, "empty-stack", &mut arena, 64, 16, 16)
            .expect_err("an empty stack is not a coherent tab topology");

        assert!(format!("{error:#}").contains("contains an empty stack"));
        assert!(arena.is_empty());
        assert_eq!(pane_id_calls.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    #[test]
    fn flat_codec_snapshot_rejects_malformed_tree_state_before_pane_ids() {
        let size = TerminalSize::default();
        let pane_id_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe: Arc<dyn Fn() + Send + Sync> = {
            let pane_id_calls = Arc::clone(&pane_id_calls);
            Arc::new(move || {
                pane_id_calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            })
        };

        let uninitialized = Tab::new(&size);
        uninitialized.inner.lock().pane = Some(Tree::Node {
            left: Box::new(Tree::Leaf(FakePane::new_with_pane_id_probe(
                930,
                size,
                Arc::clone(&probe),
            ))),
            right: Box::new(Tree::Leaf(FakePane::new_with_pane_id_probe(
                931,
                size,
                Arc::clone(&probe),
            ))),
            data: None,
        });
        let mut arena = Vec::new();
        let error = uninitialized
            .append_codec_pane_arena_in_window(100, "uninitialized", &mut arena, 64, 16, 16)
            .expect_err("an uninitialized split must fail preflight");
        assert!(format!("{error:#}").contains("uninitialized split node"));
        assert!(arena.is_empty());

        let invalid_active = Tab::new(&size);
        {
            let mut inner = invalid_active.inner.lock();
            inner.pane = Some(Tree::Leaf(FakePane::new_with_pane_id_probe(
                932,
                size,
                Arc::clone(&probe),
            )));
            inner.active = 1;
        }
        let error = invalid_active
            .append_codec_pane_arena_in_window(101, "invalid-active", &mut arena, 64, 16, 16)
            .expect_err("an out-of-range active index must fail preflight");
        assert!(format!("{error:#}").contains("active pane index 1 is beyond 1 tree leaves"));
        assert!(arena.is_empty());

        let no_tree = Tab::new(&size);
        {
            let mut inner = no_tree.inner.lock();
            inner.pane = None;
            inner.floating_panes.push(FloatingPane {
                pane: FakePane::new_with_pane_id_probe(933, size, Arc::clone(&probe)),
                pane_id: 933,
                rect: FloatingPaneRect {
                    left: 0,
                    top: 0,
                    width: 1,
                    height: 1,
                },
                z_order: 0,
                visible: true,
                pinned: false,
                opacity: 1.0,
            });
        }
        let error = no_tree
            .append_codec_pane_arena_in_window(102, "no-tree", &mut arena, 64, 16, 16)
            .expect_err("auxiliary panes cannot give an empty tab size authority");
        assert!(format!("{error:#}").contains("has no pane tree"));
        assert!(arena.is_empty());

        let empty_root = Tab::new(&size);
        empty_root.inner.lock().pane = Some(Tree::Empty);
        let error = empty_root
            .append_codec_pane_arena_in_window(103, "empty-root", &mut arena, 64, 16, 16)
            .expect_err("an empty root cannot be encoded as an ordered tab");
        assert!(format!("{error:#}").contains("has an empty root"));
        assert!(arena.is_empty());

        let leafless = Tab::new(&size);
        leafless.inner.lock().pane = Some(Tree::Node {
            left: Box::new(Tree::Empty),
            right: Box::new(Tree::Empty),
            data: Some(SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                first: size,
                second: size,
            }),
        });
        let error = leafless
            .append_codec_pane_arena_in_window(104, "leafless", &mut arena, 64, 16, 16)
            .expect_err("a non-empty tree must contain a pane leaf");
        assert!(format!("{error:#}").contains("contains no pane leaves"));
        assert!(arena.is_empty());
        assert_eq!(pane_id_calls.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    #[test]
    fn flat_codec_snapshot_retries_once_after_callback_topology_change() {
        let size = TerminalSize::default();
        let tab = Arc::new(Tab::new(&size));
        let replacement = FakePane::new(941, size);
        let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let weak_tab = Arc::downgrade(&tab);
        let replacement_for_probe = Arc::clone(&replacement);
        let mutations_for_probe = Arc::clone(&mutations);
        let probe: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if mutations_for_probe.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 0 {
                let tab = weak_tab.upgrade().expect("test retains tab");
                tab.inner.lock().pane = Some(Tree::Leaf(Arc::clone(&replacement_for_probe)));
            }
        });
        tab.inner.lock().pane = Some(Tree::Leaf(FakePane::new_with_pane_id_probe(
            940, size, probe,
        )));
        let mut arena = Vec::new();

        let descriptor = tab
            .append_codec_pane_arena_in_window(105, "retry-once", &mut arena, 64, 16, 16)
            .expect("one callback-time topology replacement must retry to coherence");

        assert_eq!(mutations.load(std::sync::atomic::Ordering::Acquire), 1);
        assert_eq!(descriptor.node_count, 1);
        let [PaneArenaNode::Leaf(entry)] = arena.as_slice() else {
            panic!("retried one-pane snapshot must encode one leaf");
        };
        assert_eq!(entry.pane_id, 941);
    }

    #[test]
    fn flat_codec_snapshot_retries_same_id_exact_pane_replacement_during_census() {
        let original_size = TerminalSize::default();
        let replacement_size = TerminalSize {
            rows: original_size.rows + 7,
            cols: original_size.cols + 11,
            pixel_width: original_size.pixel_width + 110,
            pixel_height: original_size.pixel_height + 70,
            dpi: original_size.dpi,
        };
        let tab = Arc::new(Tab::new(&original_size));
        let replacement = FakePane::new(946, replacement_size);
        let replacements = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let weak_tab = Arc::downgrade(&tab);
        let replacement_for_probe = Arc::clone(&replacement);
        let replacements_for_probe = Arc::clone(&replacements);
        let probe: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if replacements_for_probe.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 0 {
                let tab = weak_tab.upgrade().expect("test retains tab");
                tab.inner.lock().pane = Some(Tree::Leaf(Arc::clone(&replacement_for_probe)));
            }
        });
        tab.inner.lock().pane = Some(Tree::Leaf(FakePane::new_with_pane_id_probe(
            946,
            original_size,
            probe,
        )));
        let mut arena = Vec::new();

        let descriptor = tab
            .append_codec_pane_arena_in_window(110, "same-id-retry", &mut arena, 64, 16, 64)
            .expect("same-ID exact pane replacement must retry to the successor generation");

        assert_eq!(replacements.load(std::sync::atomic::Ordering::Acquire), 1);
        assert_eq!(descriptor.node_count, 1);
        let [PaneArenaNode::Leaf(entry)] = arena.as_slice() else {
            panic!("same-ID replacement snapshot must encode one leaf");
        };
        assert_eq!(entry.pane_id, 946);
        assert_eq!(entry.size, replacement_size);
    }

    #[test]
    fn flat_codec_snapshot_retries_normal_rendering_getter_focus_change() {
        let size = TerminalSize::default();
        let tab = Arc::new(Tab::new(&size));
        let getter_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let weak_tab = Arc::downgrade(&tab);
        let getter_calls_for_probe = Arc::clone(&getter_calls);
        let probe: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if getter_calls_for_probe.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 0 {
                let tab = weak_tab.upgrade().expect("test retains tab");
                tab.inner.lock().active = 1;
            }
        });
        let first = FakePane::new_with_callback_probe(942, size, false, false, probe);
        let second = FakePane::new(943, size);
        tab.inner.lock().pane = Some(Tree::Node {
            left: Box::new(Tree::Leaf(first)),
            right: Box::new(Tree::Leaf(second)),
            data: Some(SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                first: size,
                second: size,
            }),
        });
        let mut arena = Vec::new();

        let descriptor = tab
            .append_codec_pane_arena_in_window(107, "render-focus-retry", &mut arena, 64, 16, 16)
            .expect("one normal-return rendering getter focus change must retry to coherence");

        assert_eq!(getter_calls.load(std::sync::atomic::Ordering::Acquire), 2);
        assert_eq!(descriptor.node_count, 3);
        let [PaneArenaNode::Split { .. }, PaneArenaNode::Leaf(first), PaneArenaNode::Leaf(second)] =
            arena.as_slice()
        else {
            panic!("retried split snapshot must retain canonical preorder");
        };
        assert_eq!(first.pane_id, 942);
        assert!(!first.is_active_pane);
        assert_eq!(second.pane_id, 943);
        assert!(second.is_active_pane);
    }

    #[test]
    fn flat_codec_snapshot_exhausts_normal_rendering_getter_tree_reorders() {
        let size = TerminalSize::default();
        let tab = Arc::new(Tab::new(&size));
        let getter_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let weak_tab = Arc::downgrade(&tab);
        let getter_calls_for_probe = Arc::clone(&getter_calls);
        let probe: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            getter_calls_for_probe.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let tab = weak_tab.upgrade().expect("test retains tab");
            let mut inner = tab.inner.lock();
            let Some(Tree::Node { left, right, .. }) = inner.pane.as_mut() else {
                panic!("test topology must remain split");
            };
            std::mem::swap(left, right);
        });
        let first = FakePane::new_with_callback_probe(944, size, false, false, probe);
        let second = FakePane::new(945, size);
        tab.inner.lock().pane = Some(Tree::Node {
            left: Box::new(Tree::Leaf(first)),
            right: Box::new(Tree::Leaf(second)),
            data: Some(SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                first: size,
                second: size,
            }),
        });
        let prefix = PaneArenaNode::Leaf(pane_arena_test_entry(993, false, false));
        let mut arena = vec![prefix.clone()];
        let prior_capacity = arena.capacity();

        let error = tab
            .append_codec_pane_arena_in_window(108, "render-tree-exhaust", &mut arena, 64, 16, 16)
            .expect_err("every normal-return rendering getter tree reorder must exhaust retries");

        assert!(format!("{error:#}").contains("all 3 flat codec snapshot attempts"));
        assert_eq!(getter_calls.load(std::sync::atomic::Ordering::Acquire), 3);
        assert_eq!(arena, [prefix]);
        assert_eq!(arena.capacity(), prior_capacity);
    }

    #[test]
    fn flat_codec_snapshot_retries_callback_time_identity_error_before_fence() {
        let size = TerminalSize::default();
        let tab = Arc::new(Tab::new(&size));
        let replacement = FakePane::new(948, size);
        let callback_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let weak_tab = Arc::downgrade(&tab);
        let replacement_for_probe = Arc::clone(&replacement);
        let callback_calls_for_probe = Arc::clone(&callback_calls);
        let probe: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if callback_calls_for_probe.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 0 {
                let tab = weak_tab.upgrade().expect("test retains tab");
                let mut inner = tab.inner.lock();
                let Some(Tree::Node { right, .. }) = inner.pane.as_mut() else {
                    panic!("test topology must remain split");
                };
                **right = Tree::Leaf(Arc::clone(&replacement_for_probe));
            }
        });
        let first = FakePane::new_with_pane_id_probe(947, size, probe);
        let duplicate = FakePane::new(947, size);
        tab.inner.lock().pane = Some(Tree::Node {
            left: Box::new(Tree::Leaf(first)),
            right: Box::new(Tree::Leaf(duplicate)),
            data: Some(SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                first: size,
                second: size,
            }),
        });
        let mut arena = Vec::new();

        let descriptor = tab
            .append_codec_pane_arena_in_window(109, "identity-error-retry", &mut arena, 64, 16, 16)
            .expect("a callback-time duplicate-ID error from replaced topology must retry");

        assert_eq!(callback_calls.load(std::sync::atomic::Ordering::Acquire), 2);
        assert_eq!(descriptor.node_count, 3);
        let [PaneArenaNode::Split { .. }, PaneArenaNode::Leaf(first), PaneArenaNode::Leaf(second)] =
            arena.as_slice()
        else {
            panic!("retried identity snapshot must retain canonical preorder");
        };
        assert_eq!(first.pane_id, 947);
        assert_eq!(second.pane_id, 948);
    }

    #[test]
    fn flat_codec_snapshot_exhausts_retries_without_arena_growth() {
        let size = TerminalSize::default();
        let tab = Arc::new(Tab::new(&size));
        let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let weak_tab = Arc::downgrade(&tab);
        let mutations_for_probe = Arc::clone(&mutations);
        let probe: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let ordinal = mutations_for_probe
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
                .saturating_add(1);
            let tab = weak_tab.upgrade().expect("test retains tab");
            tab.inner.lock().floating_panes.push(FloatingPane {
                pane: FakePane::new(950_usize.saturating_add(ordinal), size),
                pane_id: 950_usize.saturating_add(ordinal),
                rect: FloatingPaneRect {
                    left: 0,
                    top: 0,
                    width: 1,
                    height: 1,
                },
                z_order: 0,
                visible: true,
                pinned: false,
                opacity: 1.0,
            });
        });
        tab.inner.lock().pane = Some(Tree::Leaf(FakePane::new_with_pane_id_probe(
            949, size, probe,
        )));
        let prefix = PaneArenaNode::Leaf(pane_arena_test_entry(996, false, false));
        let mut arena = vec![prefix.clone()];
        let prior_capacity = arena.capacity();

        let error = tab
            .append_codec_pane_arena_in_window(106, "retry-exhausted", &mut arena, 64, 16, 16)
            .expect_err("every callback-time topology mutation must exhaust bounded retries");

        assert!(format!("{error:#}").contains("all 3 flat codec snapshot attempts"));
        assert_eq!(mutations.load(std::sync::atomic::Ordering::Acquire), 3);
        assert_eq!(arena, [prefix]);
        assert_eq!(arena.capacity(), prior_capacity);
    }

    #[test]
    fn flat_codec_snapshot_contains_every_pane_observation_panic_and_preserves_arena() {
        let size = TerminalSize::default();
        let callbacks = [
            OrderedObservationCallback::PaneId,
            OrderedObservationCallback::Dimensions,
            OrderedObservationCallback::WorkingDirectory,
            OrderedObservationCallback::CursorPosition,
            OrderedObservationCallback::Title,
            OrderedObservationCallback::AltScreen,
            OrderedObservationCallback::TtyName,
        ];

        for (index, callback) in callbacks.iter().copied().enumerate() {
            let tab = Tab::new(&size);
            let first_pane_id = 601_usize.saturating_add(index.saturating_mul(2));
            tab.assign_pane(&FakePane::new(first_pane_id, size));
            let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            tab.split_and_insert(
                0,
                SplitRequest::default(),
                FakePane::new_with_ordered_observation_panic(
                    first_pane_id.saturating_add(1),
                    size,
                    callback,
                    Arc::clone(&armed),
                ),
            )
            .expect("install panicking pane after one observable predecessor");
            armed.store(true, std::sync::atomic::Ordering::Release);
            let prefix = PaneArenaNode::Leaf(pane_arena_test_entry(997, false, false));
            let mut arena = vec![prefix.clone()];
            let prior_capacity = arena.capacity();

            let result = catch_unwind(AssertUnwindSafe(|| {
                tab.append_codec_pane_arena_in_window(
                    90,
                    "panic-workspace",
                    &mut arena,
                    64,
                    16,
                    TEST_ORDERED_PANE_CENSUS_WORK,
                )
            }));
            let error = result
                .unwrap_or_else(|_| {
                    panic!("{:?} panic escaped the mux observation boundary", callback)
                })
                .expect_err("a panicking pane callback must fail the ordered snapshot");

            assert_eq!(
                format!("{error:#}"),
                format!(
                    "a pane callback panicked while tab {} was being observed for ordered encoding",
                    tab.tab_id()
                ),
                "unexpected contained error for {callback:?}"
            );
            assert_eq!(arena, [prefix], "arena changed after {callback:?} panic");
            assert_eq!(
                arena.capacity(),
                prior_capacity,
                "arena allocation changed after {callback:?} panic"
            );
        }
    }

    #[test]
    fn staged_prune_uses_exact_identity_and_runs_callbacks_after_unlock() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Arc::new(Tab::new(&size));
        let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let weak_tab = Arc::downgrade(&tab);
        let probe: Arc<dyn Fn() + Send + Sync> = {
            let armed = Arc::clone(&armed);
            let callback_count = Arc::clone(&callback_count);
            Arc::new(move || {
                if !armed.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                let tab = weak_tab.upgrade().expect("tab retained by test");
                assert!(
                    tab.inner.try_lock().is_some(),
                    "pane callback must not run while Tab::inner is held"
                );
                callback_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            })
        };
        let dead = FakePane::new_with_callback_probe(77, size, true, false, Arc::clone(&probe));
        let replacement =
            FakePane::new_with_callback_probe(77, size, false, false, Arc::clone(&probe));

        tab.assign_pane(&dead);
        tab.split_and_insert(0, SplitRequest::default(), Arc::clone(&replacement))
            .expect("same numeric ID is permitted for an adversarial exact-identity test");
        tab.set_active_idx(0);
        armed.store(true, std::sync::atomic::Ordering::Release);

        assert!(tab.prune_dead_panes_without_mux());
        let survivors = tab.iter_all_panes();
        assert_eq!(survivors.len(), 1);
        assert!(Arc::ptr_eq(&survivors[0], &replacement));
        assert!(
            callback_count.load(std::sync::atomic::Ordering::Acquire) >= 4,
            "liveness, resize, and focus effects should all exercise the unlocked callback path"
        );
    }

    #[test]
    fn kill_registration_preserves_same_id_visible_pane_when_current_is_hidden_in_stack() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let mux = Arc::new(Mux::new(None));
        let stale_visible = FakePane::new(78, size);
        mux.add_pane(&stale_visible)
            .expect("stale pane should publish its original registration");
        let stale_registration = mux
            .capture_pane_registration(&stale_visible)
            .expect("stale pane should expose its original registration");
        assert!(
            stale_registration.detach_local_if_current(),
            "retiring the original local registration creates the stale topology identity"
        );

        let current_hidden = FakePane::new(78, size);
        mux.add_pane(&current_hidden)
            .expect("same-ID successor should publish after exact local retirement");
        let current_registration = mux
            .capture_pane_registration(&current_hidden)
            .expect("successor should expose its current registration");

        let tab = Tab::new(&size);
        {
            let mut inner = tab.inner.lock();
            inner.pane = Some(Tree::Leaf(Arc::clone(&stale_visible)));
            inner.pane_stacks.insert(
                0,
                PaneStack::new(vec![
                    Arc::clone(&stale_visible),
                    Arc::clone(&current_hidden),
                ]),
            );
        }
        {
            let inner = tab.inner.lock();
            let stack = inner
                .pane_stacks
                .get(&0)
                .expect("adversarial setup should create one pane stack");
            assert_eq!(stack.len(), 2);
            assert_eq!(stack.active_index(), 0);
            assert!(Arc::ptr_eq(stack.active_pane(), &stale_visible));
            assert!(
                Arc::ptr_eq(&stack.panes()[1], &current_hidden),
                "the exact current registration must begin hidden behind its same-ID predecessor"
            );
            assert!(matches!(
                inner.pane.as_ref(),
                Some(Tree::Leaf(pane)) if Arc::ptr_eq(pane, &stale_visible)
            ));
        }

        assert!(
            !tab.kill_pane_registration(&stale_registration),
            "a retired handle must not remove either same-ID topology identity"
        );
        assert!(
            tab.kill_pane_registration(&current_registration),
            "the current hidden registration should be removed and killed exactly once"
        );

        assert_eq!(
            current_hidden
                .downcast_ref::<FakePane>()
                .expect("current pane should retain its concrete test type")
                .kills
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "only the exact current registration may be killed"
        );
        assert_eq!(
            stale_visible
                .downcast_ref::<FakePane>()
                .expect("stale pane should retain its concrete test type")
                .kills
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the same-ID visible stale pane must not be killed"
        );
        assert!(
            mux.get_pane(78).is_none(),
            "retiring the current registration must leave the reusable numeric slot empty"
        );
        assert_eq!(current_registration.try_with_current(|_| ()), None);

        let visible = tab.iter_panes();
        assert_eq!(visible.len(), 1);
        assert!(
            Arc::ptr_eq(&visible[0].pane, &stale_visible),
            "the original visible tree leaf must remain the exact stale Arc"
        );
        let all_panes = tab.iter_all_panes();
        assert_eq!(all_panes.len(), 1);
        assert!(Arc::ptr_eq(&all_panes[0], &stale_visible));

        let inner = tab.inner.lock();
        let stack = inner
            .pane_stacks
            .get(&0)
            .expect("the surviving visible pane should retain its stack slot");
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.active_index(), 0);
        assert!(
            Arc::ptr_eq(stack.active_pane(), &stale_visible),
            "exact hidden removal must preserve the visible stack selection"
        );
        assert!(matches!(
            inner.pane.as_ref(),
            Some(Tree::Leaf(pane)) if Arc::ptr_eq(pane, &stale_visible)
        ));
    }

    #[test]
    fn staged_prune_retains_a_pane_when_is_dead_panics() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        let pane = FakePane::new_with_callback_probe(88, size, false, true, Arc::new(|| {}));
        tab.assign_pane(&pane);

        assert!(!tab.prune_dead_panes_without_mux());
        assert_eq!(tab.count_panes(), Some(1));
        assert!(!tab.is_dead());
    }

    #[test]
    fn floating_pane_z_order_deterministic_after_operations() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 30,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 750,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        // Add three floating panes
        for id in 10..13 {
            tab.add_floating_pane(
                FakePane::new(id, size),
                FloatingPaneRect {
                    left: id * 2,
                    top: id,
                    width: 20,
                    height: 10,
                },
            )
            .expect("floating pane should be detached");
        }

        // Bring pane 10 to front (z_order only, not focus)
        assert!(tab.bring_floating_pane_to_front(10));

        let panes = tab.iter_floating_panes();
        assert_eq!(3, panes.len());

        // Pane 10 now has the highest z_order and sorts last,
        // but pane 12 retains focus since set_floating_pane_focus
        // was not called.
        assert_eq!(10, panes.last().unwrap().pane_id);
        // Focus remains on pane 12 (last added)
        let focused = panes.iter().find(|p| p.is_focused).unwrap();
        assert_eq!(12, focused.pane_id);

        // Verify z-orders are unique
        let z_orders: Vec<u32> = panes.iter().map(|p| p.z_order).collect();
        let mut deduped = z_orders.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(z_orders.len(), deduped.len(), "z-orders must be unique");
    }

    #[test]
    fn floating_pane_z_order_compacts_before_counter_exhaustion() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 30,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 750,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));
        for id in 10..13 {
            tab.add_floating_pane(
                FakePane::new(id, size),
                FloatingPaneRect {
                    left: id,
                    top: id,
                    width: 20,
                    height: 10,
                },
            )
            .expect("floating pane should be detached");
        }
        assert!(tab.set_floating_pane_z_order(12, u32::MAX));

        assert!(tab.bring_floating_pane_to_front(10));

        let panes = tab.iter_floating_panes();
        let z_orders: Vec<u32> = panes.iter().map(|pane| pane.z_order).collect();
        let mut unique = z_orders.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), panes.len());
        assert_eq!(panes.last().map(|pane| pane.pane_id), Some(10));
        assert!(z_orders.iter().all(|z_order| *z_order < u32::MAX));
    }

    #[test]
    fn floating_pane_reposition_updates_geometry() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 30,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 750,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        tab.add_floating_pane(
            FakePane::new(50, size),
            FloatingPaneRect {
                left: 5,
                top: 5,
                width: 30,
                height: 15,
            },
        )
        .expect("floating pane should be detached");

        let new_rect = FloatingPaneRect {
            left: 10,
            top: 10,
            width: 40,
            height: 12,
        };
        let updated = tab.set_floating_pane_rect(50, new_rect).unwrap();
        assert_eq!(10, updated.left);
        assert_eq!(10, updated.top);
        assert_eq!(40, updated.width);
        assert_eq!(12, updated.height);

        // Non-existent pane returns None
        assert!(tab.set_floating_pane_rect(999, new_rect).is_none());
    }

    #[test]
    fn floating_pane_resize_clamps_to_tab() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        // Resize tab to a very small size and check floating pane gets clamped
        tab.add_floating_pane(
            FakePane::new(60, size),
            FloatingPaneRect {
                left: 50,
                top: 10,
                width: 30,
                height: 15,
            },
        )
        .expect("floating pane should be detached");

        let small = TerminalSize {
            rows: 10,
            cols: 20,
            pixel_width: 200,
            pixel_height: 250,
            dpi: 96,
        };
        tab.resize(small);

        let panes = tab.iter_floating_panes();
        assert_eq!(1, panes.len());
        let fp = &panes[0];
        // After resize to 20 cols, floating pane should be clamped
        assert!(
            fp.left + fp.width <= 20,
            "floating pane should fit within new cols: left={} width={}",
            fp.left,
            fp.width
        );
        assert!(
            fp.top + fp.height <= 10,
            "floating pane should fit within new rows: top={} height={}",
            fp.top,
            fp.height
        );
    }

    #[test]
    fn floating_pane_remove_nonexistent_returns_none() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        assert!(tab.remove_floating_pane(999).is_none());
    }

    #[test]
    fn resize_split_by_clamps_to_horizontal_constraints() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                min_width: 5,
                ..PaneConstraints::default()
            },
        ));

        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .expect("split to compute");
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_constraints(
                2,
                split.second,
                PaneConstraints {
                    min_width: 30,
                    ..PaneConstraints::default()
                },
            ),
        )
        .expect("split insertion to succeed");

        tab.resize_split_by(0, -200);
        let panes = tab.iter_panes();
        assert_eq!(5, panes[0].width);
        assert_eq!(74, panes[1].width);

        tab.resize_split_by(0, 200);
        let panes = tab.iter_panes();
        assert_eq!(49, panes[0].width);
        assert_eq!(30, panes[1].width);
    }

    #[test]
    fn resize_split_by_clamps_to_vertical_constraints() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                min_height: 10,
                ..PaneConstraints::default()
            },
        ));

        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .expect("split to compute");
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Vertical,
                ..Default::default()
            },
            FakePane::new_with_constraints(
                2,
                split.second,
                PaneConstraints {
                    min_height: 7,
                    ..PaneConstraints::default()
                },
            ),
        )
        .expect("split insertion to succeed");

        tab.resize_split_by(0, -200);
        let panes = tab.iter_panes();
        assert_eq!(10, panes[0].height);
        assert_eq!(13, panes[1].height);

        tab.resize_split_by(0, 200);
        let panes = tab.iter_panes();
        assert_eq!(16, panes[0].height);
        assert_eq!(7, panes[1].height);
    }

    #[test]
    fn resize_clamps_to_tree_constraint_minimum() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                min_width: 30,
                ..PaneConstraints::default()
            },
        ));

        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .expect("split to compute");
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_constraints(
                2,
                split.second,
                PaneConstraints {
                    min_width: 20,
                    ..PaneConstraints::default()
                },
            ),
        )
        .expect("split insertion to succeed");

        tab.resize(TerminalSize {
            rows: 24,
            cols: 10,
            pixel_width: 100,
            pixel_height: 600,
            dpi: 96,
        });

        let resized = tab.get_size();
        assert_eq!(51, resized.cols);
        assert_eq!(24, resized.rows);

        let panes = tab.iter_panes();
        assert_eq!(30, panes[0].width);
        assert_eq!(20, panes[1].width);
    }

    #[test]
    fn compute_split_size_clamps_to_existing_constraints() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                min_width: 30,
                ..PaneConstraints::default()
            },
        ));

        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    target_is_second: true,
                    size: SplitSize::Cells(70),
                    ..Default::default()
                },
            )
            .expect("split to compute");

        assert_eq!(30, split.first.cols);
        assert_eq!(49, split.second.cols);
    }

    #[test]
    fn split_and_insert_rejects_unfittable_constraints() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                min_width: 30,
                ..PaneConstraints::default()
            },
        ));

        let result = tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                target_is_second: true,
                size: SplitSize::Cells(5),
                ..Default::default()
            },
            FakePane::new_with_constraints(
                2,
                size,
                PaneConstraints {
                    min_width: 60,
                    ..PaneConstraints::default()
                },
            ),
        );

        assert!(result.is_err());
    }

    #[test]
    fn compute_split_size_respects_existing_max_constraints() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                max_width: Some(35),
                ..PaneConstraints::default()
            },
        ));

        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    target_is_second: false,
                    size: SplitSize::Cells(10),
                    ..Default::default()
                },
            )
            .expect("split to compute");

        assert_eq!(35, split.second.cols);
        assert_eq!(44, split.first.cols);
    }

    #[test]
    fn resize_clamps_to_fixed_pane_size() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                fixed: true,
                ..PaneConstraints::default()
            },
        ));

        tab.resize(TerminalSize {
            rows: 40,
            cols: 120,
            pixel_width: 1200,
            pixel_height: 1000,
            dpi: 96,
        });

        let resized = tab.get_size();
        assert_eq!(80, resized.cols);
        assert_eq!(24, resized.rows);
    }

    #[test]
    fn resize_split_by_respects_max_constraints() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                max_width: Some(35),
                ..PaneConstraints::default()
            },
        ));

        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .expect("split to compute");
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new(2, split.second),
        )
        .expect("split insertion to succeed");

        tab.resize_split_by(0, 200);
        let panes = tab.iter_panes();
        assert_eq!(35, panes[0].width);
        assert_eq!(44, panes[1].width);
    }

    #[test]
    fn top_level_split_rejects_incompatible_new_pane_constraints() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        let first_split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .expect("initial split to compute");
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new(2, first_split.second),
        )
        .expect("initial split insertion to succeed");

        let result = tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                top_level: true,
                target_is_second: true,
                size: SplitSize::Cells(10),
            },
            FakePane::new_with_constraints(
                3,
                size,
                PaneConstraints {
                    min_width: 60,
                    ..PaneConstraints::default()
                },
            ),
        );

        assert!(result.is_err());
    }

    #[test]
    fn top_level_split_rejects_incompatible_existing_tree_constraints() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                min_width: 40,
                ..PaneConstraints::default()
            },
        ));

        let first_split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    target_is_second: true,
                    size: SplitSize::Cells(30),
                    ..Default::default()
                },
            )
            .expect("initial split to compute");
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                target_is_second: true,
                size: SplitSize::Cells(30),
                ..Default::default()
            },
            FakePane::new_with_constraints(
                2,
                first_split.second,
                PaneConstraints {
                    min_width: 30,
                    ..PaneConstraints::default()
                },
            ),
        )
        .expect("initial split insertion to succeed");

        let result = tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                top_level: true,
                target_is_second: true,
                size: SplitSize::Cells(20),
            },
            FakePane::new(3, size),
        );

        assert!(result.is_err());
    }

    #[test]
    fn pane_resize_work_runs_once_in_input_order_on_the_calling_thread() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let caller_thread = std::thread::current().id();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let mut work = Vec::new();

        for pane_id in 1..=16 {
            let observed_for_callback = Arc::clone(&observed);
            work.push((
                FakePane::new_with_callback_probe(
                    pane_id,
                    size,
                    false,
                    false,
                    Arc::new(move || {
                        observed_for_callback
                            .lock()
                            .push((pane_id, std::thread::current().id()));
                    }),
                ),
                size,
            ));
        }

        execute_pane_resize_work(work);

        let observed = observed.lock();
        assert_eq!(
            observed
                .iter()
                .map(|(pane_id, _)| *pane_id)
                .collect::<Vec<_>>(),
            (1..=16).collect::<Vec<_>>(),
            "resize effects must preserve the prepared tree order",
        );
        assert!(
            observed
                .iter()
                .all(|(_, callback_thread)| *callback_thread == caller_thread),
            "resize effects must not cross a fallible transient thread-spawn boundary",
        );
    }

    proptest! {
        #[test]
        fn resize_split_by_preserves_width_budget_and_mins(
            left_min in 1usize..40,
            right_min in 1usize..40,
            delta in -400isize..400isize,
        ) {
            let size = TerminalSize {
                rows: 30,
                cols: 160,
                pixel_width: 1600,
                pixel_height: 900,
                dpi: 96,
            };
            let tab = Tab::new(&size);
            tab.assign_pane(&FakePane::new_with_constraints(
                1,
                size,
                PaneConstraints {
                    min_width: left_min,
                    ..PaneConstraints::default()
                },
            ));

            let split = tab
                .compute_split_size(
                    0,
                    SplitRequest {
                        direction: SplitDirection::Horizontal,
                        ..Default::default()
                    },
                )
                .expect("split to compute");
            tab.split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
                FakePane::new_with_constraints(
                    2,
                    split.second,
                    PaneConstraints {
                        min_width: right_min,
                        ..PaneConstraints::default()
                    },
                ),
            )
            .expect("split insertion to succeed");

            tab.resize_split_by(0, delta);
            let panes = tab.iter_panes();
            prop_assert_eq!(2, panes.len());
            prop_assert_eq!(panes[0].width + panes[1].width + 1, tab.get_size().cols);
            prop_assert!(panes[0].width >= left_min);
            prop_assert!(panes[1].width >= right_min);
        }

        #[test]
        fn fixed_pane_ignores_resize_requests(
            target_cols in 20usize..240,
            target_rows in 8usize..120,
        ) {
            let size = TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 800,
                pixel_height: 600,
                dpi: 96,
            };
            let tab = Tab::new(&size);
            tab.assign_pane(&FakePane::new_with_constraints(
                1,
                size,
                PaneConstraints {
                    fixed: true,
                    ..PaneConstraints::default()
                },
            ));

            tab.resize(TerminalSize {
                rows: target_rows,
                cols: target_cols,
                pixel_width: target_cols.saturating_mul(10),
                pixel_height: target_rows.saturating_mul(20),
                dpi: 96,
            });

            let resized = tab.get_size();
            prop_assert_eq!(size.cols, resized.cols);
            prop_assert_eq!(size.rows, resized.rows);
        }

        /// Verify that collapsing and uncollapsing are consistent: after a
        /// shrink + grow cycle, no pane remains spuriously collapsed.
        #[test]
        fn collapse_uncollapse_cycle_is_consistent(
            target_cols in 10usize..60,
        ) {
            let initial_cols = 120usize;
            let size = TerminalSize {
                rows: 24,
                cols: initial_cols,
                pixel_width: initial_cols * 10,
                pixel_height: 600,
                dpi: 96,
            };

            let tab = Tab::new(&size);
            tab.assign_pane(&FakePane::new_with_priority(
                1,
                size,
                PaneConstraints {
                    min_width: 20,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Low,
            ));
            let split = tab
                .compute_split_size(
                    0,
                    SplitRequest {
                        direction: SplitDirection::Horizontal,
                        ..Default::default()
                    },
                )
                .expect("split");
            tab.split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
                FakePane::new_with_priority(
                    2,
                    split.second,
                    PaneConstraints {
                        min_width: 20,
                        min_height: 3,
                        ..PaneConstraints::default()
                    },
                    CollapsePriority::Normal,
                ),
            )
            .expect("insert");

            // Shrink to target_cols
            let small = TerminalSize {
                rows: 24,
                cols: target_cols,
                pixel_width: target_cols * 10,
                pixel_height: 600,
                dpi: 96,
            };
            tab.resize(small);

            // Grow back to original
            tab.resize(size);

            // After growing back, no pane should remain collapsed
            prop_assert!(
                tab.collapsed_pane_ids().is_empty(),
                "all panes should be uncollapsed after growing back to original"
            );
        }

        /// Verify that Never-priority panes are never found in the collapsed set
        /// regardless of target size.
        #[test]
        fn never_priority_never_in_collapsed_set(
            target_cols in 5usize..40,
        ) {
            let size = TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 800,
                pixel_height: 600,
                dpi: 96,
            };

            let tab = Tab::new(&size);
            tab.assign_pane(&FakePane::new_with_priority(
                1,
                size,
                PaneConstraints {
                    min_width: 15,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Low,
            ));
            let split = tab
                .compute_split_size(
                    0,
                    SplitRequest {
                        direction: SplitDirection::Horizontal,
                        ..Default::default()
                    },
                )
                .expect("split");
            tab.split_and_insert(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
                FakePane::new_with_priority(
                    2,
                    split.second,
                    PaneConstraints {
                        min_width: 15,
                        min_height: 3,
                        ..PaneConstraints::default()
                    },
                    CollapsePriority::Never,
                ),
            )
            .expect("insert");

            let small = TerminalSize {
                rows: 24,
                cols: target_cols,
                pixel_width: target_cols * 10,
                pixel_height: 600,
                dpi: 96,
            };
            tab.resize(small);

            prop_assert!(
                !tab.is_pane_collapsed(2),
                "Never-priority pane must never be collapsed"
            );
        }

        /// Verify that after adding N floating panes and bringing random
        /// ones to front, z-orders remain unique and the focused pane
        /// renders last in iteration order.
        #[test]
        fn floating_z_order_always_unique_after_focus_ops(
            bring_to_front_id in 10usize..15,
        ) {
            ensure_mux_initialized();
            let size = TerminalSize {
                rows: 40,
                cols: 120,
                pixel_width: 1200,
                pixel_height: 1000,
                dpi: 96,
            };
            let tab = Tab::new(&size);
            tab.assign_pane(&FakePane::new(1, size));

            // Add 5 floating panes (ids 10-14)
            for id in 10..15 {
                tab.add_floating_pane(
                    FakePane::new(id, size),
                    FloatingPaneRect {
                        left: id * 3,
                        top: id * 2,
                        width: 20,
                        height: 10,
                    },
                )
                .expect("floating pane should be detached");
            }

            // Bring a random one to front
            tab.set_floating_pane_focus(bring_to_front_id);

            let panes = tab.iter_floating_panes();
            prop_assert_eq!(5, panes.len());

            // Check z-orders are unique
            let mut z_orders: Vec<u32> = panes.iter().map(|p| p.z_order).collect();
            z_orders.sort();
            let before = z_orders.len();
            z_orders.dedup();
            prop_assert_eq!(before, z_orders.len(), "z-orders must be unique");

            // Focused pane must be last in iteration
            let last = panes.last().unwrap();
            prop_assert!(last.is_focused, "last in iteration must be focused");
            prop_assert_eq!(bring_to_front_id, last.pane_id);
        }
    }

    fn is_send_and_sync<T: Send + Sync>() -> bool {
        true
    }

    #[test]
    fn tab_is_send_and_sync() {
        assert!(is_send_and_sync::<Tab>());
    }

    // ── SplitDirection ───────────────────────────────────────

    #[test]
    fn split_direction_equality() {
        assert_eq!(SplitDirection::Horizontal, SplitDirection::Horizontal);
        assert_eq!(SplitDirection::Vertical, SplitDirection::Vertical);
        assert_ne!(SplitDirection::Horizontal, SplitDirection::Vertical);
    }

    #[test]
    fn split_direction_clone_copy() {
        let d = SplitDirection::Horizontal;
        let d2 = d; // Copy
        let d3 = d.clone(); // Clone
        assert_eq!(d, d2);
        assert_eq!(d, d3);
    }

    #[test]
    fn split_direction_debug() {
        assert!(format!("{:?}", SplitDirection::Horizontal).contains("Horizontal"));
        assert!(format!("{:?}", SplitDirection::Vertical).contains("Vertical"));
    }

    // ── SplitSize ────────────────────────────────────────────

    #[test]
    fn split_size_default_is_50_percent() {
        assert_eq!(SplitSize::default(), SplitSize::Percent(50));
    }

    #[test]
    fn split_size_equality() {
        assert_eq!(SplitSize::Cells(10), SplitSize::Cells(10));
        assert_eq!(SplitSize::Percent(50), SplitSize::Percent(50));
        assert_ne!(SplitSize::Cells(10), SplitSize::Cells(20));
        assert_ne!(SplitSize::Cells(50), SplitSize::Percent(50));
    }

    #[test]
    fn split_size_clone_copy() {
        let s = SplitSize::Cells(42);
        let s2 = s; // Copy
        let s3 = s.clone(); // Clone
        assert_eq!(s, s2);
        assert_eq!(s, s3);
    }

    #[test]
    fn split_percent_request_saturates_oversized_dimensions() {
        let requested = requested_split_target_axis_size(
            usize::MAX,
            SplitRequest {
                size: SplitSize::Percent(u8::MAX),
                ..SplitRequest::default()
            },
        );

        assert_eq!(requested, usize::MAX / 100);
    }

    // ── SplitRequest ─────────────────────────────────────────

    #[test]
    fn split_request_default() {
        let r = SplitRequest::default();
        assert_eq!(r.direction, SplitDirection::Horizontal);
        assert!(r.target_is_second);
        assert!(!r.top_level);
        assert_eq!(r.size, SplitSize::Percent(50));
    }

    #[test]
    fn split_request_equality() {
        let a = SplitRequest::default();
        let b = SplitRequest::default();
        assert_eq!(a, b);
        let c = SplitRequest {
            direction: SplitDirection::Vertical,
            ..Default::default()
        };
        assert_ne!(a, c);
    }

    // ── PositionedSplit ──────────────────────────────────────

    #[test]
    fn positioned_split_equality() {
        let a = PositionedSplit {
            index: 0,
            direction: SplitDirection::Horizontal,
            left: 40,
            top: 0,
            size: 24,
        };
        let b = PositionedSplit {
            index: 0,
            direction: SplitDirection::Horizontal,
            left: 40,
            top: 0,
            size: 24,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn positioned_split_inequality() {
        let a = PositionedSplit {
            index: 0,
            direction: SplitDirection::Horizontal,
            left: 40,
            top: 0,
            size: 24,
        };
        let b = PositionedSplit {
            index: 1,
            direction: SplitDirection::Vertical,
            left: 0,
            top: 12,
            size: 80,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn positioned_split_clone_copy() {
        let a = PositionedSplit {
            index: 5,
            direction: SplitDirection::Vertical,
            left: 10,
            top: 20,
            size: 30,
        };
        let b = a; // Copy
        let c = a.clone(); // Clone
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn positioned_split_debug() {
        let s = PositionedSplit {
            index: 0,
            direction: SplitDirection::Horizontal,
            left: 40,
            top: 0,
            size: 24,
        };
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("PositionedSplit"));
        assert!(dbg.contains("Horizontal"));
    }

    // ── SplitDirectionAndSize ────────────────────────────────

    #[test]
    fn split_direction_and_size_width_horizontal() {
        let s = SplitDirectionAndSize {
            direction: SplitDirection::Horizontal,
            first: TerminalSize {
                cols: 40,
                rows: 24,
                pixel_width: 400,
                pixel_height: 600,
                dpi: 96,
            },
            second: TerminalSize {
                cols: 39,
                rows: 24,
                pixel_width: 390,
                pixel_height: 600,
                dpi: 96,
            },
        };
        // Horizontal: first.cols + second.cols + 1 (for separator)
        assert_eq!(s.width(), 80);
        assert_eq!(s.height(), 24);
    }

    #[test]
    fn split_direction_and_size_height_vertical() {
        let s = SplitDirectionAndSize {
            direction: SplitDirection::Vertical,
            first: TerminalSize {
                cols: 80,
                rows: 12,
                pixel_width: 800,
                pixel_height: 300,
                dpi: 96,
            },
            second: TerminalSize {
                cols: 80,
                rows: 11,
                pixel_width: 800,
                pixel_height: 275,
                dpi: 96,
            },
        };
        // Vertical: first.rows + second.rows + 1 (for separator)
        assert_eq!(s.height(), 24);
        assert_eq!(s.width(), 80);
    }

    // ── PaneNode ─────────────────────────────────────────────

    #[test]
    fn pane_node_empty_root_size_is_none() {
        let node = PaneNode::Empty;
        assert!(node.root_size().is_none());
    }

    #[test]
    fn pane_node_empty_window_and_tab_ids_is_none() {
        let node = PaneNode::Empty;
        assert!(node.window_and_tab_ids().is_none());
    }

    #[test]
    fn pane_node_leaf_root_size() {
        let entry = PaneEntry {
            window_id: 0,
            tab_id: 0,
            pane_id: 1,
            title: "test".to_string(),
            size: TerminalSize::default(),
            working_dir: None,
            alt_screen_active: false,
            is_active_pane: true,
            is_zoomed_pane: false,
            workspace: "default".to_string(),
            cursor_pos: StableCursorPosition::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        };
        let node = PaneNode::Leaf(entry);
        let size = node.root_size();
        assert!(size.is_some());
        assert_eq!(size.unwrap().rows, 24);
        assert_eq!(size.unwrap().cols, 80);
    }

    #[test]
    fn pane_node_leaf_window_and_tab_ids() {
        let entry = PaneEntry {
            window_id: 5,
            tab_id: 10,
            pane_id: 1,
            title: "test".to_string(),
            size: TerminalSize::default(),
            working_dir: None,
            alt_screen_active: false,
            is_active_pane: false,
            is_zoomed_pane: false,
            workspace: "ws".to_string(),
            cursor_pos: StableCursorPosition::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: Some("/dev/pts/0".to_string()),
        };
        let node = PaneNode::Leaf(entry);
        assert_eq!(node.window_and_tab_ids(), Some((5, 10)));
    }

    #[test]
    fn pane_node_all_window_and_tab_ids_match_checks_every_split_leaf() {
        let entry = |window_id, tab_id, pane_id| PaneEntry {
            window_id,
            tab_id,
            pane_id,
            title: "test".to_string(),
            size: TerminalSize::default(),
            working_dir: None,
            alt_screen_active: false,
            is_active_pane: false,
            is_zoomed_pane: false,
            workspace: "ws".to_string(),
            cursor_pos: StableCursorPosition::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        };
        let node = PaneNode::Split {
            left: Box::new(PaneNode::Leaf(entry(5, 10, 1))),
            right: Box::new(PaneNode::Leaf(entry(5, 11, 2))),
            node: SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                first: TerminalSize::default(),
                second: TerminalSize::default(),
            },
        };

        assert_eq!(node.window_and_tab_ids(), Some((5, 10)));
        assert_eq!(node.all_window_and_tab_ids_match((5, 10)), Some(false));
        assert_eq!(PaneNode::Empty.all_window_and_tab_ids_match((5, 10)), None);
    }

    fn pane_arena_test_entry(pane_id: PaneId, active: bool, zoomed: bool) -> PaneEntry {
        PaneEntry {
            window_id: 5,
            tab_id: 10,
            pane_id,
            title: format!("pane-{pane_id}"),
            size: TerminalSize::default(),
            working_dir: None,
            alt_screen_active: false,
            is_active_pane: active,
            is_zoomed_pane: zoomed,
            workspace: "ws".to_string(),
            cursor_pos: StableCursorPosition::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        }
    }

    #[test]
    fn pane_arena_prepares_only_the_final_mux_tree_and_installs_focus() {
        let size = TerminalSize::default();
        let mut arena = vec![
            PaneArenaNode::Split {
                left: 1,
                right: 2,
                node: SplitDirectionAndSize {
                    direction: SplitDirection::Horizontal,
                    first: size,
                    second: size,
                },
            },
            PaneArenaNode::Leaf(pane_arena_test_entry(41, true, false)),
            PaneArenaNode::Leaf(pane_arena_test_entry(42, false, true)),
        ];
        let mut made = Vec::new();
        let prepared = prepare_pane_tree_from_arena(&mut arena, 3, |entry| {
            made.push(entry.pane_id);
            Ok(FakePane::new(entry.pane_id, entry.size))
        })
        .expect("prepare canonical pane arena");
        assert!(arena.is_empty());
        assert_eq!(made, [42, 41], "reverse consumption is deliberate");

        let tab = Tab::new(&size);
        tab.sync_with_prepared_pane_tree(size, prepared)
            .expect("install prepared pane tree");
        assert_eq!(
            tab.iter_panes_ignoring_zoom()
                .into_iter()
                .map(|positioned| positioned.pane.pane_id())
                .collect::<Vec<_>>(),
            [41, 42]
        );
        assert_eq!(tab.get_active_idx(), 0);
        assert_eq!(
            tab.get_active_pane().map(|pane| pane.pane_id()),
            Some(42),
            "the public active-pane view deliberately resolves the zoomed pane first"
        );
        assert_eq!(tab.get_zoomed_pane().map(|pane| pane.pane_id()), Some(42));
    }

    #[test]
    fn prepared_tree_install_resolves_active_exact_identity_without_pane_callback() {
        let _mux_guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Mux::shutdown();
        let size = TerminalSize::default();
        let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pane = FakePane::new_with_ordered_observation_panic(
            43,
            size,
            OrderedObservationCallback::PaneId,
            Arc::clone(&armed),
        );
        let prepared = PreparedPaneTree {
            tree: Tree::Leaf(Arc::clone(&pane)),
            active: Some(Arc::clone(&pane)),
            zoomed: None,
        };
        armed.store(true, std::sync::atomic::Ordering::Release);

        let tab = Tab::new(&size);
        tab.sync_with_prepared_pane_tree(size, prepared)
            .expect("install prepared pane tree");

        assert_eq!(tab.get_active_idx(), 0);
        let active = tab
            .get_active_pane_callback_free()
            .expect("prepared active pane must be installed");
        assert!(Arc::ptr_eq(&active, &pane));
    }

    #[test]
    fn pane_arena_rejects_bad_indices_before_invoking_pane_factory() {
        let size = TerminalSize::default();
        let mut arena = vec![
            PaneArenaNode::Split {
                left: 1,
                right: 1,
                node: SplitDirectionAndSize {
                    direction: SplitDirection::Horizontal,
                    first: size,
                    second: size,
                },
            },
            PaneArenaNode::Leaf(pane_arena_test_entry(41, true, false)),
            PaneArenaNode::Leaf(pane_arena_test_entry(42, false, false)),
        ];
        let mut make_calls = 0usize;
        let error = match prepare_pane_tree_from_arena(&mut arena, 3, |entry| {
            make_calls += 1;
            Ok(FakePane::new(entry.pane_id, entry.size))
        }) {
            Ok(_) => panic!("non-canonical children must fail before application"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("non-canonical children"));
        assert_eq!(make_calls, 0);
        assert_eq!(arena.len(), 3, "rejected arena must remain owned by caller");
    }

    fn append_balanced_split_heavy_subtree(
        nodes: &mut Vec<PaneArenaNode>,
        first_leaf: usize,
        leaves: usize,
    ) -> u32 {
        let node_index = nodes.len();
        nodes.push(PaneArenaNode::Empty);
        if leaves == 1 {
            nodes[node_index] = PaneArenaNode::Leaf(pane_arena_test_entry(
                first_leaf.saturating_add(1),
                first_leaf == 0,
                false,
            ));
            return u32::try_from(node_index).expect("test leaf index fits u32");
        }
        let left_leaves = leaves.div_ceil(2);
        let left = append_balanced_split_heavy_subtree(nodes, first_leaf, left_leaves);
        let right = append_balanced_split_heavy_subtree(
            nodes,
            first_leaf + left_leaves,
            leaves - left_leaves,
        );
        nodes[node_index] = PaneArenaNode::Split {
            left,
            right,
            node: SplitDirectionAndSize {
                direction: if leaves.is_multiple_of(2) {
                    SplitDirection::Horizontal
                } else {
                    SplitDirection::Vertical
                },
                first: TerminalSize::default(),
                second: TerminalSize::default(),
            },
        };
        u32::try_from(node_index).expect("test split index fits u32")
    }

    fn balanced_split_heavy_pane_arena(leaf_count: usize) -> Vec<PaneArenaNode> {
        assert!(leaf_count > 0);
        let node_count = leaf_count
            .checked_mul(2)
            .and_then(|count| count.checked_sub(1))
            .expect("test pane-arena node count fits usize");
        let mut nodes = Vec::with_capacity(node_count);
        assert_eq!(
            append_balanced_split_heavy_subtree(&mut nodes, 0, leaf_count),
            0,
        );
        assert_eq!(nodes.len(), node_count);
        nodes
    }

    fn append_balanced_pane_arena_slots(
        nodes: &mut Vec<PaneArenaNode>,
        first_leaf: usize,
        slots: usize,
        empty_slots: usize,
    ) -> u32 {
        assert!(slots > 0);
        assert!(empty_slots <= slots);
        let node_index = nodes.len();
        nodes.push(PaneArenaNode::Empty);
        if slots == 1 {
            if empty_slots == 0 {
                nodes[node_index] = PaneArenaNode::Leaf(pane_arena_test_entry(
                    first_leaf.saturating_add(1),
                    first_leaf == 0,
                    false,
                ));
            }
            return u32::try_from(node_index).expect("test slot index fits u32");
        }
        let left_slots = slots.div_ceil(2);
        let right_slots = slots - left_slots;
        let left_empty_slots = empty_slots.min(left_slots);
        let right_empty_slots = empty_slots - left_empty_slots;
        let left =
            append_balanced_pane_arena_slots(nodes, first_leaf, left_slots, left_empty_slots);
        let left_leaves = left_slots - left_empty_slots;
        let right = append_balanced_pane_arena_slots(
            nodes,
            first_leaf + left_leaves,
            right_slots,
            right_empty_slots,
        );
        nodes[node_index] = PaneArenaNode::Split {
            left,
            right,
            node: SplitDirectionAndSize {
                direction: if slots.is_multiple_of(2) {
                    SplitDirection::Horizontal
                } else {
                    SplitDirection::Vertical
                },
                first: TerminalSize::default(),
                second: TerminalSize::default(),
            },
        };
        u32::try_from(node_index).expect("test split index fits u32")
    }

    #[test]
    fn pane_arena_scale_work_boxes_and_scratch_storage_are_exact_and_bounded() {
        let mut scratch = PaneArenaPreparationScratch::default();
        for leaf_count in [1_usize, 20, 200, 4_096] {
            let mut arena = balanced_split_heavy_pane_arena(leaf_count);
            let node_count = leaf_count * 2 - 1;
            scratch.reset_stats();
            let prepared = prepare_pane_tree_from_arena_with_scratch(
                &mut arena,
                node_count,
                &mut scratch,
                |entry| Ok(FakePane::new(entry.pane_id, entry.size)),
            )
            .unwrap_or_else(|error| {
                panic!(
                    "split-heavy q={} application failed: {:#}",
                    leaf_count, error,
                )
            });
            assert!(arena.is_empty());
            let stats = scratch.stats();
            assert_eq!(stats.trees_started, 1);
            assert_eq!(stats.trees_completed, 1);
            assert_eq!(stats.validation_node_visits, node_count);
            assert_eq!(stats.application_node_visits, node_count);
            assert_eq!(stats.leaf_resolutions, leaf_count);
            assert_eq!(stats.split_materializations, leaf_count - 1);
            assert_eq!(
                stats.required_final_tree_box_allocations,
                (leaf_count - 1) * 2,
            );
            assert!(stats.peak_validation_stack_entries <= 16);
            assert!(stats.peak_application_stack_entries <= 16);
            assert!(stats.validation_stack_growth_events <= 3);
            assert!(stats.application_stack_growth_events <= 3);
            assert!(
                scratch
                    .requested_retained_storage_bytes()
                    .expect("test storage count fits usize")
                    < 64 * 1024,
                "balanced q={} retained an oversized traversal scratch arena",
                leaf_count,
            );
            drop(prepared);
        }
        scratch.release_retained_storage();
        assert_eq!(scratch.requested_retained_storage_bytes(), Some(0));
    }

    #[test]
    fn pane_arena_scale_exact_maximum_admitted_snapshot_reuses_one_bounded_scratch() {
        const SLOT_COUNTS: [usize; 5] = [4_096, 4_096, 4_096, 2_049, 2_049];
        const EMPTY_COUNTS: [usize; 5] = [0, 0, 0, 1, 1];
        const SNAPSHOT_LEAVES: usize = 16_384;
        const SNAPSHOT_NODES: usize = 32_767;

        let mut arena = Vec::with_capacity(SNAPSHOT_NODES);
        let mut first_leaf = 0_usize;
        let mut node_counts = Vec::with_capacity(SLOT_COUNTS.len());
        for (slots, empty_slots) in SLOT_COUNTS
            .iter()
            .copied()
            .zip(EMPTY_COUNTS.iter().copied())
        {
            let expected_root = arena.len();
            assert_eq!(
                usize::try_from(append_balanced_pane_arena_slots(
                    &mut arena,
                    first_leaf,
                    slots,
                    empty_slots,
                ))
                .expect("test root index fits usize"),
                expected_root,
            );
            node_counts.push(slots * 2 - 1);
            first_leaf += slots - empty_slots;
        }
        assert_eq!(arena.len(), SNAPSHOT_NODES);
        assert_eq!(first_leaf, SNAPSHOT_LEAVES);

        let mut scratch = PaneArenaPreparationScratch::default();
        for node_count in node_counts.into_iter().rev() {
            let prepared = prepare_pane_tree_from_arena_with_scratch(
                &mut arena,
                node_count,
                &mut scratch,
                |entry| Ok(FakePane::new(entry.pane_id, entry.size)),
            )
            .expect("each exact-maximum snapshot tree must apply");
            drop(prepared);
        }
        assert!(arena.is_empty());
        let stats = scratch.stats();
        assert_eq!(stats.trees_started, SLOT_COUNTS.len());
        assert_eq!(stats.trees_completed, SLOT_COUNTS.len());
        assert_eq!(stats.validation_node_visits, SNAPSHOT_NODES);
        assert_eq!(stats.application_node_visits, SNAPSHOT_NODES);
        assert_eq!(stats.leaf_resolutions, SNAPSHOT_LEAVES);
        let expected_splits = SLOT_COUNTS
            .iter()
            .copied()
            .map(|slots| slots - 1)
            .sum::<usize>();
        assert_eq!(stats.split_materializations, expected_splits);
        assert_eq!(
            stats.required_final_tree_box_allocations,
            expected_splits * 2,
        );
        assert!(stats.peak_validation_stack_entries <= 13);
        assert!(stats.peak_application_stack_entries <= 13);
        assert!(stats.validation_stack_growth_events <= 2);
        assert!(stats.application_stack_growth_events <= 2);
        assert!(
            scratch
                .requested_retained_storage_bytes()
                .expect("test storage count fits usize")
                < 64 * 1024,
        );
        scratch.release_retained_storage();
        assert_eq!(scratch.requested_retained_storage_bytes(), Some(0));
    }

    #[test]
    fn pane_arena_scale_malformed_preflight_releases_scratch_without_consuming_nodes() {
        let size = TerminalSize::default();
        let mut arena = vec![
            PaneArenaNode::Split {
                left: 1,
                right: 1,
                node: SplitDirectionAndSize {
                    direction: SplitDirection::Horizontal,
                    first: size,
                    second: size,
                },
            },
            PaneArenaNode::Leaf(pane_arena_test_entry(71, true, false)),
            PaneArenaNode::Leaf(pane_arena_test_entry(72, false, false)),
        ];
        let mut scratch = PaneArenaPreparationScratch::default();
        let error =
            match prepare_pane_tree_from_arena_with_scratch(&mut arena, 3, &mut scratch, |entry| {
                Ok(FakePane::new(entry.pane_id, entry.size))
            }) {
                Ok(_) => panic!("malformed pane arena must fail during preflight"),
                Err(error) => error,
            };
        assert!(format!("{error:#}").contains("non-canonical children"));
        assert_eq!(arena.len(), 3);
        assert_eq!(scratch.stats().trees_started, 1);
        assert_eq!(scratch.stats().trees_completed, 0);
        assert_eq!(scratch.stats().application_node_visits, 0);
        scratch.release_retained_storage();
        assert_eq!(scratch.requested_retained_storage_bytes(), Some(0));
    }

    #[test]
    fn pane_arena_scale_factory_error_drops_partially_prepared_subtrees() {
        let size = TerminalSize::default();
        let mut arena = vec![
            PaneArenaNode::Split {
                left: 1,
                right: 2,
                node: SplitDirectionAndSize {
                    direction: SplitDirection::Horizontal,
                    first: size,
                    second: size,
                },
            },
            PaneArenaNode::Leaf(pane_arena_test_entry(81, true, false)),
            PaneArenaNode::Leaf(pane_arena_test_entry(82, false, false)),
        ];
        let mut scratch = PaneArenaPreparationScratch::default();
        let mut prepared_pane = None;
        let error = match prepare_pane_tree_from_arena_with_scratch(
            &mut arena,
            3,
            &mut scratch,
            |entry| -> anyhow::Result<Arc<dyn Pane>> {
                if entry.pane_id == 81 {
                    anyhow::bail!("injected pane factory failure");
                }
                let pane: Arc<dyn Pane> = FakePane::new(entry.pane_id, entry.size);
                prepared_pane = Some(Arc::downgrade(&pane));
                Ok(pane)
            },
        ) {
            Ok(_) => panic!("injected pane factory failure must abort application"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("injected pane factory failure"));
        assert!(
            arena.is_empty(),
            "the failed tree range must be consumed once"
        );
        assert!(
            prepared_pane
                .expect("the reverse traversal prepares pane 82 first")
                .upgrade()
                .is_none(),
            "application scratch retained a partially prepared pane subtree",
        );
        assert_eq!(scratch.stats().trees_completed, 0);
        assert_eq!(
            scratch.stats().leaf_resolutions,
            1,
            "terminal stats must retain the successful pane callback that preceded failure",
        );
        scratch.release_retained_storage();
        assert_eq!(scratch.requested_retained_storage_bytes(), Some(0));
    }

    #[test]
    fn pane_arena_scale_factory_panic_drops_partially_prepared_subtrees() {
        let size = TerminalSize::default();
        let mut arena = vec![
            PaneArenaNode::Split {
                left: 1,
                right: 2,
                node: SplitDirectionAndSize {
                    direction: SplitDirection::Horizontal,
                    first: size,
                    second: size,
                },
            },
            PaneArenaNode::Leaf(pane_arena_test_entry(91, true, false)),
            PaneArenaNode::Leaf(pane_arena_test_entry(92, false, false)),
        ];
        let mut scratch = PaneArenaPreparationScratch::default();
        let mut prepared_pane = None;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = prepare_pane_tree_from_arena_with_scratch(
                &mut arena,
                3,
                &mut scratch,
                |entry| -> anyhow::Result<Arc<dyn Pane>> {
                    assert_ne!(entry.pane_id, 91, "injected pane factory panic");
                    let pane: Arc<dyn Pane> = FakePane::new(entry.pane_id, entry.size);
                    prepared_pane = Some(Arc::downgrade(&pane));
                    Ok(pane)
                },
            );
        }));
        assert!(result.is_err(), "the injected factory panic must propagate");
        assert!(
            arena.is_empty(),
            "the failed tree range must be consumed once"
        );
        assert!(
            prepared_pane
                .expect("the reverse traversal prepares pane 92 first")
                .upgrade()
                .is_none(),
            "application scratch retained a subtree after factory panic",
        );
        assert_eq!(scratch.stats().trees_completed, 0);
        assert_eq!(scratch.stats().leaf_resolutions, 1);
        scratch.release_retained_storage();
        assert_eq!(scratch.requested_retained_storage_bytes(), Some(0));
    }

    #[test]
    fn pane_node_debug() {
        let node = PaneNode::Empty;
        let dbg = format!("{:?}", node);
        assert!(dbg.contains("Empty"));
    }

    // ── SerdeUrl ─────────────────────────────────────────────

    #[test]
    fn serde_url_from_url() {
        let url = Url::parse("https://example.com").unwrap();
        let serde_url = SerdeUrl::from(url.clone());
        assert_eq!(serde_url.as_str(), url.as_str());
    }

    #[test]
    fn serde_url_try_from_string() {
        let serde_url = SerdeUrl::try_from("https://example.com".to_string());
        assert!(serde_url.is_ok());
        assert_eq!(serde_url.unwrap().as_str(), "https://example.com/");
    }

    #[test]
    fn serde_url_preserves_canonical_owned_string_capacity() {
        let mut canonical = String::with_capacity(64);
        canonical.push_str("https://example.com/");
        let serde_url = SerdeUrl::try_from(canonical).expect("canonical URL is valid");
        assert_eq!(serde_url.as_str(), "https://example.com/");
        assert_eq!(serde_url.capacity(), 64);
    }

    #[test]
    fn serde_url_try_from_invalid_string() {
        let result = SerdeUrl::try_from("not a url".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn serde_url_into_string() {
        let url = Url::parse("https://example.com/path").unwrap();
        let serde_url = SerdeUrl::from(url);
        let s: String = serde_url.into();
        assert_eq!(s, "https://example.com/path");
    }

    #[test]
    fn serde_url_into_url() {
        let url = Url::parse("file:///home/user").unwrap();
        let serde_url = SerdeUrl::from(url.clone());
        let back: Url = serde_url.into();
        assert_eq!(back, url);
    }

    #[test]
    fn serde_url_clone_eq() {
        let url = Url::parse("https://example.com").unwrap();
        let a = SerdeUrl::from(url);
        let b = a.clone();
        assert_eq!(a, b);
    }

    // ── PaneEntry ────────────────────────────────────────────

    #[test]
    fn pane_entry_clone_eq() {
        let entry = PaneEntry {
            window_id: 0,
            tab_id: 0,
            pane_id: 1,
            title: "shell".to_string(),
            size: TerminalSize::default(),
            working_dir: None,
            alt_screen_active: false,
            is_active_pane: true,
            is_zoomed_pane: false,
            workspace: "default".to_string(),
            cursor_pos: StableCursorPosition::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        };
        let cloned = entry.clone();
        assert_eq!(entry, cloned);
    }

    #[test]
    fn pane_entry_debug() {
        let entry = PaneEntry {
            window_id: 1,
            tab_id: 2,
            pane_id: 3,
            title: "vim".to_string(),
            size: TerminalSize::default(),
            working_dir: None,
            alt_screen_active: false,
            is_active_pane: false,
            is_zoomed_pane: true,
            workspace: "coding".to_string(),
            cursor_pos: StableCursorPosition::default(),
            physical_top: 100,
            top_row: 0,
            left_col: 5,
            tty_name: Some("/dev/pts/1".to_string()),
        };
        let dbg = format!("{:?}", entry);
        assert!(dbg.contains("PaneEntry"));
        assert!(dbg.contains("vim"));
    }

    // ── Collapse priority tests ─────────────────────────────

    #[test]
    fn collapse_low_priority_pane_on_shrink() {
        // Two horizontal panes: left has Low priority (min_width=20),
        // right has Never priority (min_width=20).  Total min = 20+1+20 = 41.
        // When we shrink to 30 cols, left should be collapsed.
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_priority(
            1,
            size,
            PaneConstraints {
                min_width: 20,
                min_height: 3,
                ..PaneConstraints::default()
            },
            CollapsePriority::Low,
        ));
        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_priority(
                2,
                split.second,
                PaneConstraints {
                    min_width: 20,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Never,
            ),
        )
        .unwrap();

        // Sanity: nothing collapsed yet
        assert!(!tab.is_pane_collapsed(1));
        assert!(!tab.is_pane_collapsed(2));

        // Shrink to 30 cols — below min of 41 — should collapse pane 1 (Low)
        let small = TerminalSize {
            rows: 24,
            cols: 30,
            pixel_width: 300,
            pixel_height: 600,
            dpi: 96,
        };
        tab.resize(small);

        assert!(
            tab.is_pane_collapsed(1),
            "Low-priority pane should be collapsed"
        );
        assert!(
            !tab.is_pane_collapsed(2),
            "Never-priority pane should NOT be collapsed"
        );

        // The non-collapsed pane should have gotten the extra space
        let panes = tab.iter_panes();
        let pane2 = panes.iter().find(|p| p.pane.pane_id() == 2).unwrap();
        // Pane 2 should use most of the 30 cols (minus 1 separator, 1 for collapsed)
        assert!(
            pane2.width >= 20,
            "Non-collapsed pane should get the freed space, got width={}",
            pane2.width
        );
    }

    #[test]
    fn collapse_priority_ordering() {
        // Three horizontal panes: Low, Normal, High priority.
        // Use a wide terminal so all three splits succeed.
        let size = TerminalSize {
            rows: 24,
            cols: 200,
            pixel_width: 2000,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_priority(
            1,
            size,
            PaneConstraints {
                min_width: 30,
                min_height: 3,
                ..PaneConstraints::default()
            },
            CollapsePriority::Low,
        ));

        // Split horizontally to add pane 2 (Normal)
        let split1 = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_priority(
                2,
                split1.second,
                PaneConstraints {
                    min_width: 30,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Normal,
            ),
        )
        .unwrap();

        // Split the right pane (index 1) to add pane 3 (High)
        let split2 = tab
            .compute_split_size(
                1,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            1,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_priority(
                3,
                split2.second,
                PaneConstraints {
                    min_width: 30,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
                CollapsePriority::High,
            ),
        )
        .unwrap();

        // Three panes at 30 min each: min total = 30+1+30+1+30 = 92.
        // Shrink to 35 cols: needs two collapsed to fit (30+1+1+1+1 = 34 ≤ 35).
        let small = TerminalSize {
            rows: 24,
            cols: 35,
            pixel_width: 350,
            pixel_height: 600,
            dpi: 96,
        };
        tab.resize(small);

        // Low should collapse first, then Normal
        assert!(
            tab.is_pane_collapsed(1),
            "Low-priority pane should be collapsed first"
        );
        assert!(
            tab.is_pane_collapsed(2),
            "Normal-priority pane should be collapsed second"
        );
        assert!(
            !tab.is_pane_collapsed(3),
            "High-priority pane should remain"
        );
    }

    #[test]
    fn never_priority_pane_exempt_from_collapse() {
        let size = TerminalSize {
            rows: 24,
            cols: 60,
            pixel_width: 600,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_priority(
            1,
            size,
            PaneConstraints {
                min_width: 25,
                min_height: 3,
                ..PaneConstraints::default()
            },
            CollapsePriority::Never,
        ));
        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_priority(
                2,
                split.second,
                PaneConstraints {
                    min_width: 25,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Never,
            ),
        )
        .unwrap();

        // Shrink below both panes' minimum — neither should collapse
        let small = TerminalSize {
            rows: 24,
            cols: 20,
            pixel_width: 200,
            pixel_height: 600,
            dpi: 96,
        };
        tab.resize(small);

        assert!(
            !tab.is_pane_collapsed(1),
            "Never-priority should never collapse"
        );
        assert!(
            !tab.is_pane_collapsed(2),
            "Never-priority should never collapse"
        );
    }

    #[test]
    fn uncollapse_panes_on_grow() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_priority(
            1,
            size,
            PaneConstraints {
                min_width: 20,
                min_height: 3,
                ..PaneConstraints::default()
            },
            CollapsePriority::Low,
        ));
        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_priority(
                2,
                split.second,
                PaneConstraints {
                    min_width: 20,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Normal,
            ),
        )
        .unwrap();

        // Shrink to cause collapse
        let small = TerminalSize {
            rows: 24,
            cols: 25,
            pixel_width: 250,
            pixel_height: 600,
            dpi: 96,
        };
        tab.resize(small);
        assert!(tab.is_pane_collapsed(1), "pane 1 should be collapsed");

        // Grow back to original size — pane should uncollapse
        tab.resize(size);
        assert!(
            !tab.is_pane_collapsed(1),
            "pane 1 should be uncollapsed after growing"
        );
        assert!(
            !tab.is_pane_collapsed(2),
            "pane 2 should be uncollapsed after growing"
        );
    }

    #[test]
    fn collapsed_pane_ids_api() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_priority(
            1,
            size,
            PaneConstraints {
                min_width: 20,
                min_height: 3,
                ..PaneConstraints::default()
            },
            CollapsePriority::Low,
        ));
        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_priority(
                2,
                split.second,
                PaneConstraints {
                    min_width: 20,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Never,
            ),
        )
        .unwrap();

        // Initially empty
        assert!(tab.collapsed_pane_ids().is_empty());

        // Shrink to trigger collapse
        let small = TerminalSize {
            rows: 24,
            cols: 25,
            pixel_width: 250,
            pixel_height: 600,
            dpi: 96,
        };
        tab.resize(small);

        let collapsed = tab.collapsed_pane_ids();
        assert!(collapsed.contains(&1));
        assert!(!collapsed.contains(&2));
    }

    #[test]
    fn compute_split_budget_basic() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_constraints(
            1,
            size,
            PaneConstraints {
                min_width: 10,
                min_height: 3,
                ..PaneConstraints::default()
            },
        ));
        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_constraints(
                2,
                split.second,
                PaneConstraints {
                    min_width: 10,
                    min_height: 3,
                    ..PaneConstraints::default()
                },
            ),
        )
        .unwrap();

        let budget = tab.compute_split_budget(0);
        assert!(budget.is_some(), "split 0 should exist");
        let (shrink, grow) = budget.unwrap();
        // Shrink is negative (how far first child can shrink)
        assert!(shrink < 0, "should be able to shrink first child");
        // Grow is positive (how far first child can grow)
        assert!(grow > 0, "should be able to grow first child");

        // Non-existent split returns None
        assert!(tab.compute_split_budget(99).is_none());
    }

    #[test]
    fn vertical_collapse_on_shrink() {
        // Vertical split: top pane Low priority, bottom pane Never.
        let size = TerminalSize {
            rows: 40,
            cols: 80,
            pixel_width: 800,
            pixel_height: 1000,
            dpi: 96,
        };

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_priority(
            1,
            size,
            PaneConstraints {
                min_width: 5,
                min_height: 15,
                ..PaneConstraints::default()
            },
            CollapsePriority::Low,
        ));
        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Vertical,
                ..Default::default()
            },
            FakePane::new_with_priority(
                2,
                split.second,
                PaneConstraints {
                    min_width: 5,
                    min_height: 15,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Never,
            ),
        )
        .unwrap();

        // Shrink rows below minimum (15+1+15 = 31)
        let small = TerminalSize {
            rows: 20,
            cols: 80,
            pixel_width: 800,
            pixel_height: 500,
            dpi: 96,
        };
        tab.resize(small);

        assert!(
            tab.is_pane_collapsed(1),
            "Top pane (Low) should be collapsed on vertical shrink"
        );
        assert!(
            !tab.is_pane_collapsed(2),
            "Bottom pane (Never) should remain"
        );
    }

    // ---- Swap layout tests ----

    /// Helper: create a tab with N panes in a horizontal split chain.
    fn make_tab_with_n_panes(n: usize) -> (Tab, TerminalSize) {
        let size = TerminalSize {
            rows: 24,
            cols: 400, // Wide enough for up to 8 panes with separators
            pixel_width: 4000,
            pixel_height: 600,
            dpi: 96,
        };
        ensure_mux_initialized();
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));
        for i in 2..=n {
            // Split the last pane (right-most) to avoid shrinking pane 0 too much.
            let last_idx = i - 2; // index of the last leaf
            let split = tab
                .compute_split_size(
                    last_idx,
                    SplitRequest {
                        direction: SplitDirection::Horizontal,
                        ..Default::default()
                    },
                )
                .unwrap();
            tab.split_and_insert(
                last_idx,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
                FakePane::new(i as PaneId, split.second),
            )
            .unwrap();
        }
        (tab, size)
    }

    #[test]
    fn swap_layout_preserves_all_panes() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(4);
        let pane_ids_before: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        assert_eq!(pane_ids_before.len(), 4);

        tab.set_layout_cycle(default_cycle());

        // Swap to main-side (3 slots, 4 panes → 1 stacked)
        let name = tab.swap_to_next_layout().unwrap();
        assert_eq!(name, "main-side");

        // All pane IDs should still be present (tree + stacks).
        let tree_ids: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        let stacked_ids: HashSet<PaneId> = tab.all_stacked_pane_ids().into_iter().collect();
        let all_ids: HashSet<PaneId> = tree_ids.union(&stacked_ids).copied().collect();
        assert_eq!(
            pane_ids_before, all_ids,
            "No panes should be lost during layout swap"
        );
    }

    #[test]
    fn swap_layout_cycle_wraps() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(2);
        tab.set_layout_cycle(default_cycle());

        // Cycle through all layouts and back to start.
        let n1 = tab.swap_to_next_layout().unwrap(); // main-side
        let n2 = tab.swap_to_next_layout().unwrap(); // stacked
        let n3 = tab.swap_to_next_layout().unwrap(); // main-bottom
        let n4 = tab.swap_to_next_layout().unwrap(); // grid-4 (wraps)
        assert_eq!(n1, "main-side");
        assert_eq!(n2, "stacked");
        assert_eq!(n3, "main-bottom");
        assert_eq!(n4, "grid-4");
    }

    #[test]
    fn swap_to_stacked_puts_all_panes_in_stack() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(3);
        tab.set_layout_cycle(default_cycle());

        // Advance to "stacked" layout (index 2).
        tab.swap_to_layout_index(2);
        let name = tab.current_layout_name().unwrap();
        assert_eq!(name, "stacked");

        // Stacked layout has 1 slot → 2 overflow panes stacked.
        let tree_panes = tab.iter_panes_ignoring_zoom();
        assert_eq!(tree_panes.len(), 1, "Stacked layout should show 1 leaf");
        assert!(
            tab.stack_count() > 0,
            "Should have at least one stack for overflow panes"
        );
        assert_eq!(
            tab.count_panes(),
            Some(3),
            "pane accounting must include hidden stack members"
        );
    }

    #[test]
    fn removing_visible_stack_member_promotes_survivor_and_preserves_count() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(3);
        tab.set_layout_cycle(default_cycle());
        assert_eq!(tab.swap_to_layout_index(2).as_deref(), Some("stacked"));

        let removed_id = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();
        let removed = tab
            .remove_pane(removed_id)
            .expect("visible stack member should be removable");
        assert_eq!(removed.pane_id(), removed_id);
        assert_eq!(tab.count_panes(), Some(2));

        let visible_after = tab.iter_panes_ignoring_zoom();
        assert_eq!(visible_after.len(), 1);
        assert_ne!(visible_after[0].pane.pane_id(), removed_id);
        assert!(!tab.contains_pane(removed_id));

        let slot = tab
            .first_nontrivial_stack_slot_index()
            .expect("two survivors should remain stacked");
        let cycled_id = tab.cycle_stack(slot).expect("survivor stack should cycle");
        assert_eq!(
            tab.iter_panes_ignoring_zoom()[slot].pane.pane_id(),
            cycled_id
        );
        assert_eq!(tab.count_panes(), Some(2));
    }

    #[test]
    fn removing_earlier_leaf_reindexes_later_stack_slot() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(4);
        tab.set_layout_cycle(default_cycle());
        assert_eq!(tab.swap_to_next_layout().as_deref(), Some("main-side"));
        assert_eq!(tab.first_nontrivial_stack_slot_index(), Some(2));

        let first_id = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();
        tab.remove_pane(first_id)
            .expect("unstacked leading pane should be removable");

        assert_eq!(tab.count_panes(), Some(3));
        assert_eq!(tab.first_nontrivial_stack_slot_index(), Some(1));
        let cycled_id = tab.cycle_stack(1).expect("reindexed stack should cycle");
        assert_eq!(tab.iter_panes_ignoring_zoom()[1].pane.pane_id(), cycled_id);
    }

    #[test]
    fn hidden_stack_members_are_members_and_cannot_be_added_as_floating() {
        use crate::layout::default_cycle;

        let (tab, size) = make_tab_with_n_panes(3);
        tab.set_layout_cycle(default_cycle());
        assert_eq!(tab.swap_to_layout_index(2).as_deref(), Some("stacked"));

        let visible_id = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();
        let hidden_id = tab
            .all_stacked_pane_ids()
            .into_iter()
            .find(|pane_id| *pane_id != visible_id)
            .expect("stack should contain a hidden pane");
        assert!(tab.contains_pane(hidden_id));
        assert_eq!(tab.domain_id_for_pane(hidden_id), Some(1));

        let hidden = tab
            .inner
            .lock()
            .find_pane_by_id(hidden_id)
            .expect("hidden pane should be discoverable");
        let result = tab.add_floating_pane(
            hidden,
            FloatingPaneRect {
                left: 2,
                top: 2,
                width: 20,
                height: 10,
            },
        );
        assert!(result.is_err());
        assert!(tab.iter_floating_panes().is_empty());
        assert_eq!(tab.count_panes(), Some(3));
        assert_eq!(tab.inner.lock().size, size);
    }

    #[test]
    fn exact_focus_rejects_a_hidden_stack_member_without_changing_selection() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(3);
        tab.set_layout_cycle(default_cycle());
        assert_eq!(tab.swap_to_layout_index(2).as_deref(), Some("stacked"));

        let visible = tab
            .get_active_pane()
            .expect("stacked layout has one visible active pane");
        let hidden_id = tab
            .all_stacked_pane_ids()
            .into_iter()
            .find(|pane_id| *pane_id != visible.pane_id())
            .expect("stacked layout has a hidden pane");
        let hidden = tab
            .inner
            .lock()
            .find_pane_by_id(hidden_id)
            .expect("hidden pane remains an exact tab member");

        assert!(
            !tab.set_active_pane(&hidden),
            "a hidden stack member cannot be accepted as the active visible pane",
        );
        assert!(
            tab.get_active_pane()
                .is_some_and(|current| Arc::ptr_eq(&current, &visible)),
            "a rejected exact focus must preserve the prior visible pane",
        );
    }

    #[test]
    fn exact_focus_rejects_a_distinct_same_id_nonmember() {
        let (tab, size) = make_tab_with_n_panes(2);
        let member = tab
            .iter_panes_ignoring_zoom()
            .into_iter()
            .find(|positioned| positioned.pane.pane_id() == 1)
            .map(|positioned| positioned.pane)
            .expect("first exact pane member");
        assert!(tab.set_active_pane(&member));

        let stale_same_id = FakePane::new(2, size);
        assert!(
            !tab.set_active_pane(&stale_same_id),
            "a distinct Arc must not focus the equal-ID pane that is actually in the tab",
        );
        assert!(
            tab.get_active_pane()
                .is_some_and(|current| Arc::ptr_eq(&current, &member)),
            "same-ID authority rejection must preserve the exact active pane",
        );
    }

    #[test]
    fn duplicate_floating_pane_identity_is_rejected() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));
        let floating = FakePane::new(2, size);
        let rect = FloatingPaneRect {
            left: 2,
            top: 2,
            width: 20,
            height: 10,
        };

        tab.add_floating_pane(Arc::clone(&floating), rect)
            .expect("first detached insertion should succeed");
        assert!(tab.add_floating_pane(floating, rect).is_err());
        assert_eq!(tab.iter_floating_panes().len(), 1);
        assert_eq!(tab.count_panes(), Some(2));
    }

    #[test]
    fn rotating_layout_reindexes_stack_to_its_visible_member() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(4);
        tab.set_layout_cycle(default_cycle());
        assert_eq!(tab.swap_to_next_layout().as_deref(), Some("main-side"));

        tab.rotate_clockwise();
        let slot = tab
            .first_nontrivial_stack_slot_index()
            .expect("rotated layout should retain its pane stack");
        let active_stack_id = {
            let inner = tab.inner.lock();
            inner
                .pane_stacks
                .get(&slot)
                .expect("stack should be keyed by its rotated slot")
                .active_pane()
                .pane_id()
        };
        assert_eq!(
            tab.iter_panes_ignoring_zoom()[slot].pane.pane_id(),
            active_stack_id
        );
        let cycled_id = tab.cycle_stack(slot).expect("rotated stack should cycle");
        assert_eq!(
            tab.iter_panes_ignoring_zoom()[slot].pane.pane_id(),
            cycled_id
        );
        assert_eq!(tab.count_panes(), Some(4));
    }

    #[test]
    fn swap_layout_focus_preserved() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(3);

        // Find pane 2 and set it as active.
        let pane_2 = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|p| p.pane.pane_id() == 2)
            .unwrap()
            .pane
            .clone();
        tab.set_active_pane(&pane_2);
        let active_before = tab.get_active_pane().unwrap().pane_id();
        assert_eq!(active_before, 2);

        tab.set_layout_cycle(default_cycle());
        tab.swap_to_next_layout(); // main-side: has main slot

        // Active pane should still be pane 2 (placed in main slot).
        let active_after = tab.get_active_pane().unwrap().pane_id();
        assert_eq!(
            active_after, active_before,
            "Focus should be preserved across layout swap"
        );
    }

    #[test]
    fn swap_layout_roundtrip_restores_pane_set() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(4);
        let ids_before: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();

        tab.set_layout_cycle(default_cycle());

        // Swap forward through entire cycle and back.
        for _ in 0..4 {
            tab.swap_to_next_layout();
        }

        // Verify all panes present.
        let tree_ids: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        let stacked_ids: HashSet<PaneId> = tab.all_stacked_pane_ids().into_iter().collect();
        let all_ids: HashSet<PaneId> = tree_ids.union(&stacked_ids).copied().collect();
        assert_eq!(
            ids_before, all_ids,
            "Full cycle swap should preserve all panes"
        );
    }

    #[test]
    fn cycle_stack_switches_visible_pane() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(3);
        tab.set_layout_cycle(default_cycle());

        // Switch to stacked layout (all 3 panes in 1 slot).
        tab.swap_to_layout_index(2);

        let visible_before = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();

        // Cycle the stack.
        let new_visible = tab.cycle_stack(0);
        if let Some(new_id) = new_visible {
            assert_ne!(
                new_id, visible_before,
                "Cycling stack should change visible pane"
            );
            // Verify new pane is now in the tree.
            let current = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();
            assert_eq!(current, new_id);
        }
    }

    #[test]
    fn cycle_stack_backward_returns_to_previous_visible_pane() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(3);
        tab.set_layout_cycle(default_cycle());
        tab.swap_to_layout_index(2); // stacked layout

        let visible_before = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();
        let visible_after_forward = tab.cycle_stack(0).expect("forward cycle should succeed");
        assert_ne!(
            visible_after_forward, visible_before,
            "Forward cycle should change visible pane"
        );

        let visible_after_backward = tab
            .cycle_stack_backward(0)
            .expect("backward cycle should succeed");
        assert_eq!(
            visible_after_backward, visible_before,
            "Backward cycle should return to the previously visible pane"
        );
        let current = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();
        assert_eq!(current, visible_before);
    }

    #[test]
    fn cycle_stack_backward_single_pane_stack_returns_none() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(1);
        tab.set_layout_cycle(default_cycle());
        tab.swap_to_layout_index(2); // stacked layout with one pane

        let visible_before = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();
        assert!(
            tab.cycle_stack_backward(0).is_none(),
            "Single-pane stack should not cycle backward"
        );
        let current = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();
        assert_eq!(current, visible_before);
    }

    #[test]
    fn cycle_stack_backward_invalid_slot_returns_none() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(3);
        tab.set_layout_cycle(default_cycle());
        tab.swap_to_layout_index(2); // stacked layout in slot 0

        let visible_before = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();
        assert!(
            tab.cycle_stack_backward(999).is_none(),
            "Unknown stack slot should return None"
        );
        let current = tab.iter_panes_ignoring_zoom()[0].pane.pane_id();
        assert_eq!(current, visible_before);
    }

    #[test]
    fn first_nontrivial_stack_slot_index_identifies_cycleable_stack() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(5);
        tab.set_layout_cycle(default_cycle());

        let mut layout_index = 0usize;
        let slot_index = loop {
            if tab.swap_to_layout_index(layout_index).is_none() {
                panic!("expected at least one layout with a non-trivial pane stack");
            }
            if let Some(slot) = tab.first_nontrivial_stack_slot_index() {
                break slot;
            }
            layout_index += 1;
        };

        let visible_before: Vec<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();

        tab.cycle_stack(slot_index)
            .expect("forward cycle should succeed");
        let visible_after_forward: Vec<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        assert_ne!(
            visible_after_forward, visible_before,
            "forward cycle should change visible tree panes"
        );

        tab.cycle_stack_backward(slot_index)
            .expect("backward cycle should succeed");
        let visible_after_backward: Vec<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        assert_eq!(
            visible_after_backward, visible_before,
            "backward cycle should restore original visible tree panes"
        );
    }

    #[test]
    fn swap_default_cycle_applies_layout() {
        let (tab, _size) = make_tab_with_n_panes(2);
        // Tabs now ship with a default layout cycle, so swap should succeed.
        let name = tab.swap_to_next_layout();
        assert!(name.is_some(), "Default layout cycle should allow swap");
        assert!(tab.current_layout_name().is_some());
    }

    #[test]
    fn layout_swap_with_single_pane() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(1);
        tab.set_layout_cycle(default_cycle());

        // Swap to grid-4 (4 slots, but only 1 pane).
        tab.swap_to_next_layout();
        let panes = tab.iter_panes_ignoring_zoom();
        // Should have the pane somewhere in the tree.
        let has_pane_1 = panes.iter().any(|p| p.pane.pane_id() == 1);
        assert!(has_pane_1, "Single pane should be placed in the layout");
    }

    // ---- Proptest: swap layout invariants ----

    proptest! {
        /// Swapping through any number of layouts never loses panes.
        #[test]
        fn swap_layout_never_loses_panes(
            num_panes in 1usize..8,
            num_swaps in 1usize..12,
        ) {
            use crate::layout::default_cycle;

            let (tab, _size) = make_tab_with_n_panes(num_panes);
            let ids_before: HashSet<PaneId> = tab
                .iter_panes_ignoring_zoom()
                .iter()
                .map(|p| p.pane.pane_id())
                .collect();

            tab.set_layout_cycle(default_cycle());

            for _ in 0..num_swaps {
                tab.swap_to_next_layout();
            }

            let tree_ids: HashSet<PaneId> = tab
                .iter_panes_ignoring_zoom()
                .iter()
                .map(|p| p.pane.pane_id())
                .collect();
            let stacked_ids: HashSet<PaneId> =
                tab.all_stacked_pane_ids().into_iter().collect();
            let all_ids: HashSet<PaneId> =
                tree_ids.union(&stacked_ids).copied().collect();

            prop_assert_eq!(
                ids_before.len(),
                all_ids.len(),
                "pane count mismatch: before={}, after={}",
                ids_before.len(),
                all_ids.len()
            );
            for id in &ids_before {
                prop_assert!(
                    all_ids.contains(id),
                    "pane {} lost during swap",
                    id
                );
            }
        }

        /// Focus is always on a valid pane after any sequence of swaps.
        #[test]
        fn swap_layout_focus_always_valid(
            num_panes in 1usize..6,
            num_swaps in 1usize..8,
        ) {
            use crate::layout::default_cycle;

            let (tab, _size) = make_tab_with_n_panes(num_panes);
            tab.set_layout_cycle(default_cycle());

            for _ in 0..num_swaps {
                tab.swap_to_next_layout();
            }

            let active = tab.get_active_pane();
            prop_assert!(
                active.is_some(),
                "Active pane should never be None after swap"
            );
        }
    }

    // ---- FrankenMux integration tests (ft-2dd4s.5) ----

    /// Integration test: floating panes + swap layouts + constraints
    /// all work together without interfering.
    #[test]
    fn frankenmux_integration_floating_and_swap() {
        use crate::layout::default_cycle;

        let size = TerminalSize {
            rows: 40,
            cols: 160,
            pixel_width: 1600,
            pixel_height: 1000,
            dpi: 96,
        };
        ensure_mux_initialized();

        let tab = Tab::new(&size);

        // Create 3 tiled panes.
        tab.assign_pane(&FakePane::new(1, size));
        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new(2, split.second),
        )
        .unwrap();
        let split2 = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Vertical,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Vertical,
                ..Default::default()
            },
            FakePane::new(3, split2.second),
        )
        .unwrap();

        // Add a floating pane.
        let float_pane = FakePane::new(
            10,
            TerminalSize {
                rows: 10,
                cols: 40,
                pixel_width: 400,
                pixel_height: 250,
                dpi: 96,
            },
        );
        tab.add_floating_pane(
            float_pane.clone(),
            FloatingPaneRect {
                left: 20,
                top: 5,
                width: 40,
                height: 10,
            },
        )
        .expect("floating pane should be detached");

        // Verify initial state: 3 tiled + 1 floating.
        let tiled = tab.iter_panes_ignoring_zoom();
        assert_eq!(tiled.len(), 3, "Should have 3 tiled panes");
        let floating = tab.iter_floating_panes();
        assert_eq!(floating.len(), 1, "Should have 1 floating pane");

        // Now swap layouts — this should only affect tiled panes, not floating.
        tab.set_layout_cycle(default_cycle());
        tab.swap_to_next_layout(); // main-side

        // Floating pane should still be there.
        let floating_after = tab.iter_floating_panes();
        assert_eq!(
            floating_after.len(),
            1,
            "Floating pane should survive layout swap"
        );
        assert_eq!(floating_after[0].pane_id, 10);

        // All 3 tiled panes should still exist (in tree + stacks).
        let tree_ids: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        let stacked_ids: HashSet<PaneId> = tab.all_stacked_pane_ids().into_iter().collect();
        let all_tiled: HashSet<PaneId> = tree_ids.union(&stacked_ids).copied().collect();
        assert!(all_tiled.contains(&1));
        assert!(all_tiled.contains(&2));
        assert!(all_tiled.contains(&3));

        // Swap to stacked layout.
        tab.swap_to_layout_index(2);
        assert_eq!(tab.current_layout_name().unwrap(), "stacked");

        // Still 1 floating pane.
        assert_eq!(tab.iter_floating_panes().len(), 1);

        // Swap back to grid-4.
        tab.swap_to_layout_index(0);

        // All tiled panes still present.
        let tree_ids: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        let stacked_ids: HashSet<PaneId> = tab.all_stacked_pane_ids().into_iter().collect();
        let all_final: HashSet<PaneId> = tree_ids.union(&stacked_ids).copied().collect();
        assert_eq!(all_final.len(), 3);
    }

    /// Integration test: constraint-based resize works after layout swap.
    #[test]
    fn frankenmux_integration_constraints_after_swap() {
        use crate::layout::default_cycle;

        let size = TerminalSize {
            rows: 40,
            cols: 200,
            pixel_width: 2000,
            pixel_height: 1000,
            dpi: 96,
        };
        ensure_mux_initialized();

        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new_with_priority(
            1,
            size,
            PaneConstraints {
                min_width: 20,
                min_height: 10,
                ..PaneConstraints::default()
            },
            CollapsePriority::Low,
        ));
        let split = tab
            .compute_split_size(
                0,
                SplitRequest {
                    direction: SplitDirection::Horizontal,
                    ..Default::default()
                },
            )
            .unwrap();
        tab.split_and_insert(
            0,
            SplitRequest {
                direction: SplitDirection::Horizontal,
                ..Default::default()
            },
            FakePane::new_with_priority(
                2,
                split.second,
                PaneConstraints {
                    min_width: 20,
                    min_height: 10,
                    ..PaneConstraints::default()
                },
                CollapsePriority::Never,
            ),
        )
        .unwrap();

        // Set layout cycle and swap.
        tab.set_layout_cycle(default_cycle());
        tab.swap_to_next_layout(); // main-side

        // Now resize the tab smaller — constraints should still work.
        let small = TerminalSize {
            rows: 40,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 1000,
            dpi: 96,
        };
        tab.resize(small);

        // Tab should not crash and panes should still exist.
        let panes = tab.iter_panes_ignoring_zoom();
        assert!(
            !panes.is_empty(),
            "Tab should have panes after resize with constraints"
        );
    }

    /// Integration test: zoom interacts correctly with layout swap.
    #[test]
    fn frankenmux_integration_zoom_and_swap() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(3);

        // Zoom a pane.
        tab.set_zoomed(true);

        // Set layout cycle.
        tab.set_layout_cycle(default_cycle());

        // Swap layout while zoomed — should still work.
        let name = tab.swap_to_next_layout();
        assert!(name.is_some(), "Swap should work even when zoomed");

        // All panes should be accounted for.
        let tree_ids: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        let stacked_ids: HashSet<PaneId> = tab.all_stacked_pane_ids().into_iter().collect();
        let all: HashSet<PaneId> = tree_ids.union(&stacked_ids).copied().collect();
        assert_eq!(all.len(), 3, "All 3 panes should survive zoom + swap");
    }

    #[test]
    fn swap_layout_pane_count_mismatch_overflow_stacks() {
        use crate::layout::{default_cycle, grid_4};

        // Create 6 panes, then swap to grid-4 (4 slots) — 2 extras must be stacked.
        let (tab, _size) = make_tab_with_n_panes(6);
        tab.set_layout_cycle(default_cycle());

        // grid-4 is the default (index 0).
        tab.swap_to_layout_index(0);
        let name = tab.current_layout_name().unwrap();
        assert_eq!(name, "grid-4");

        let tree_panes: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|p| p.pane.pane_id())
            .collect();
        let stacked_panes: HashSet<PaneId> = tab.all_stacked_pane_ids().into_iter().collect();
        // PaneStack holds ALL panes in a slot (visible + hidden), so the
        // visible pane appears in both tree_panes and stacked_panes.
        // Use union for the true unique count.
        let all_panes: HashSet<PaneId> = tree_panes.union(&stacked_panes).copied().collect();

        assert_eq!(
            all_panes.len(),
            6,
            "All 6 panes must survive (tree + stacked)"
        );
        assert_eq!(
            tree_panes.len(),
            grid_4().arrangement.slot_count(),
            "Tree should have exactly as many leaves as grid-4 slots"
        );
        // 2 overflow panes + 1 visible pane in the last slot = 3 in the stack
        assert_eq!(
            stacked_panes.len(),
            3,
            "Last slot stack should hold the visible pane + 2 overflow panes"
        );
    }

    #[test]
    fn remove_pane_prunes_hidden_layout_stack_member() {
        use crate::layout::default_cycle;

        let (tab, _size) = make_tab_with_n_panes(6);
        tab.set_layout_cycle(default_cycle());
        tab.swap_to_layout_index(0);

        let tree_ids: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|positioned| positioned.pane.pane_id())
            .collect();
        let stacked_ids: HashSet<PaneId> = tab.all_stacked_pane_ids().into_iter().collect();
        let hidden_id = *stacked_ids
            .difference(&tree_ids)
            .next()
            .expect("grid-4 overflow should leave at least one hidden stacked pane");

        let removed = tab
            .remove_pane(hidden_id)
            .expect("hidden stacked pane should be removable");
        assert_eq!(removed.pane_id(), hidden_id);
        assert!(
            !tab.all_stacked_pane_ids().contains(&hidden_id),
            "remove_pane must not leave hidden stacked pane state behind",
        );

        tab.swap_to_next_layout();
        let tree_ids_after: HashSet<PaneId> = tab
            .iter_panes_ignoring_zoom()
            .iter()
            .map(|positioned| positioned.pane.pane_id())
            .collect();
        let stacked_ids_after: HashSet<PaneId> = tab.all_stacked_pane_ids().into_iter().collect();
        let all_after: HashSet<PaneId> =
            tree_ids_after.union(&stacked_ids_after).copied().collect();
        assert!(
            !all_after.contains(&hidden_id),
            "layout swaps must not resurrect a pane removed from a hidden stack",
        );
    }

    #[test]
    fn collapse_priority_default_is_normal() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let pane = FakePane::new(1, size);
        assert_eq!(
            pane.collapse_priority(),
            CollapsePriority::Normal,
            "Default collapse priority should be Normal"
        );
    }

    #[test]
    fn floating_pane_focus_cycle_through_multiple() {
        ensure_mux_initialized();
        let size = TerminalSize {
            rows: 30,
            cols: 100,
            pixel_width: 1000,
            pixel_height: 750,
            dpi: 96,
        };
        let tab = Tab::new(&size);
        tab.assign_pane(&FakePane::new(1, size));

        // Add 3 floating panes.
        for id in [10, 20, 30] {
            tab.add_floating_pane(
                FakePane::new(id, size),
                FloatingPaneRect {
                    left: id * 2,
                    top: id,
                    width: 20,
                    height: 10,
                },
            )
            .expect("floating pane should be detached");
        }

        // Last added (30) should be focused.
        assert_eq!(tab.get_active_pane().unwrap().pane_id(), 30);

        // Cycle focus: 30 → 10 → 20 → 30
        assert!(tab.set_floating_pane_focus(10));
        assert_eq!(tab.get_active_pane().unwrap().pane_id(), 10);

        assert!(tab.set_floating_pane_focus(20));
        assert_eq!(tab.get_active_pane().unwrap().pane_id(), 20);

        assert!(tab.set_floating_pane_focus(30));
        assert_eq!(tab.get_active_pane().unwrap().pane_id(), 30);

        // Non-existent pane returns false.
        assert!(!tab.set_floating_pane_focus(999));
    }
}
