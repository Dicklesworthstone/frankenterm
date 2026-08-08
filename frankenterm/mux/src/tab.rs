use crate::domain::DomainId;
use crate::layout::{redistribute_panes, LayoutCycle, PaneStack, SwapLayout};
use crate::pane::*;
use crate::renderable::StableCursorPosition;
use crate::{
    Mux, MuxNotification, MuxNotificationEnvelope, PaneOperationGuard, PaneRegistrationHandle,
    WindowId,
};
use bintree::PathBranch;
use config::configuration;
use config::keyassignment::PaneDirection;
use frankenterm_sigpipe::{catch_recoverable, RecoverablePanicSite};
use frankenterm_term::{StableRowIndex, TerminalSize};
use parking_lot::Mutex;
use rangeset::intersects_range;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
#[cfg(test)]
use std::panic::catch_unwind;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
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

#[derive(Default)]
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

struct TabInner {
    id: TabId,
    pane: Option<Tree>,
    floating_panes: Vec<FloatingPane>,
    floating_focus: Option<PaneId>,
    size: TerminalSize,
    size_before_zoom: TerminalSize,
    active: usize,
    zoomed: Option<Arc<dyn Pane>>,
    title: String,
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
}

type PaneIdentity = *const ();

fn pane_identity(pane: &Arc<dyn Pane>) -> PaneIdentity {
    Arc::as_ptr(pane).cast::<()>()
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
) -> anyhow::Result<()> {
    *work = work
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("tab {tab_id} ordered pane census work overflows usize"))?;
    if *work > max_census_work {
        anyhow::bail!(
            "tab {tab_id} ordered pane census exceeds {max_census_work} carrier entries"
        );
    }
    Ok(())
}

fn push_bounded_callback_free_pane(
    owners: &mut HashMap<PaneIdentity, CallbackFreePaneOwner>,
    panes: &mut Vec<Arc<dyn Pane>>,
    pane: &Arc<dyn Pane>,
    owner: CallbackFreePaneOwner,
    max_census_panes: usize,
    tab_id: TabId,
) -> anyhow::Result<Option<CallbackFreePaneOwner>> {
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
    reserve_pane_arena_stack_push(
        panes,
        1,
        max_census_panes,
        "ordered pane census entries",
    )?;
    let prior = owners.insert(identity, owner);
    debug_assert!(prior.is_none(), "ordered pane census identity changed under tab lock");
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
    title: String,
}

struct OrderedPaneEntryObservation {
    pane_id: PaneId,
    title: String,
    size: TerminalSize,
    working_dir: Option<SerdeUrl>,
    alt_screen_active: bool,
    cursor_pos: StableCursorPosition,
    physical_top: StableRowIndex,
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
) -> anyhow::Result<OrderedPaneObservation> {
    if tree_leaf_count > panes.len() {
        anyhow::bail!(
            "tab {tab_id} ordered pane observation expects {tree_leaf_count} tree leaves from {} exact panes",
            panes.len()
        );
    }

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
        let observation = match catch_recoverable(
            RecoverablePanicSite::MuxPaneCallback,
            AssertUnwindSafe(|| {
                let pane_id = pane.pane_id();
                let tree_entry = if pane_index < tree_leaf_count {
                    // Pane implementations are arbitrary external code. Observe
                    // every field needed by the wire entry inside the same unwind
                    // boundary, before taking the final topology/focus coherence
                    // cut. Assembly after that cut must be callback-free.
                    let dims = pane.get_dimensions();
                    let working_dir = pane
                        .get_current_working_dir(CachePolicy::AllowStale)
                        .map(Into::into);
                    let cursor_pos = pane.get_cursor_position();
                    Some(OrderedPaneEntryObservation {
                        pane_id,
                        title: pane.get_title(),
                        size: TerminalSize {
                            cols: dims.cols,
                            rows: dims.viewport_rows,
                            pixel_height: dims.pixel_height,
                            pixel_width: dims.pixel_width,
                            dpi: dims.dpi,
                        },
                        working_dir,
                        alt_screen_active: pane.is_alt_screen_active(),
                        cursor_pos,
                        physical_top: dims.physical_top,
                        tty_name: pane.tty_name(),
                    })
                } else {
                    None
                };
                (pane_id, tree_entry)
            }),
        ) {
            Ok(observation) => observation,
            Err(_) => {
                anyhow::bail!(
                    "a pane callback panicked while tab {tab_id} was being observed for ordered encoding"
                );
            }
        };
        let (pane_id, tree_entry) = observation;
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
}

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

    /// Reserve topology revisions before the tab lock that protects the
    /// structural mutation is released. Subscriber callbacks remain deferred,
    /// but a coherent snapshot can no longer observe the new tree or focus
    /// under an older revision.
    fn reserve_topology_notifications(&mut self, mux: &Mux, tab_id: TabId) {
        if !self.changed {
            return;
        }
        self.topology_notifications.push(
            mux.envelope_notification(MuxNotification::TabResized(tab_id)),
        );
        if self.focus_changed() {
            if let Some(pane_id) = self.current_focus_id {
                self.topology_notifications.push(
                    mux.envelope_notification(MuxNotification::PaneFocused(pane_id)),
                );
            }
        }
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
    reserve_pane_arena_stack_push(
        &mut tasks,
        1,
        remaining,
        "callback-free pane traversal",
    )?;
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
                    anyhow::bail!(
                        "ordered pane arena exceeds total node limit {max_total_nodes}"
                    );
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
                    right: split_right,
                    ..
                }) = captured.get_mut(split_index)
                else {
                    anyhow::bail!(
                        "ordered pane traversal lost split placeholder {split_index}"
                    );
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
    workspace: &str,
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
        workspace: workspace.to_string(),
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
        Tree::Leaf(pane) => {
            PaneNode::Leaf(pane_entry(
                pane,
                pane.pane_id(),
                tab_id,
                window_id,
                active,
                zoomed,
                workspace,
                left_col,
                top_row,
            ))
        }
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
    if reserve_pane_arena_stack_push(validation, 1, node_count, "pane arena validation stack")?
    {
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
                scratch.stats.leaf_resolutions =
                    scratch.stats.leaf_resolutions.saturating_add(1);
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
            PaneArenaNode::Split {
                left,
                right,
                node,
            } => {
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
        anyhow::bail!(
            "pane arena application produced root {root_index}, expected {arena_start}"
        );
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

fn exact_pane_identity_set(panes: &[Arc<dyn Pane>]) -> HashSet<PaneIdentity> {
    panes.iter().map(pane_identity).collect()
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
        }
    }

    pub fn get_title(&self) -> String {
        self.inner.lock().title.clone()
    }

    pub fn set_title(&self, title: &str) {
        let mux = Mux::try_get();
        let (_, notification) = self.set_title_for_mux(title, mux.as_deref());
        if let (Some(mux), Some(notification)) = (mux, notification) {
            mux.dispatch_notification_envelope(notification);
        }
    }

    pub(crate) fn set_title_for_mux(
        &self,
        title: &str,
        mux: Option<&Mux>,
    ) -> (bool, Option<MuxNotificationEnvelope>) {
        let mut inner = self.inner.lock();
        if inner.title == title {
            return (false, None);
        }
        let title = title.to_string();
        inner.title = title.clone();
        let notification = mux.map(|mux| {
            mux.envelope_notification(MuxNotification::TabTitleChanged {
                tab_id: self.tab_id,
                title,
            })
        });
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
        let tree = build_from_pane_tree(
            root.into_tree(),
            &mut active,
            &mut zoomed,
            &mut make_pane,
        )?;
        self.sync_with_prepared_pane_tree(
            size,
            PreparedPaneTree {
                tree,
                active,
                zoomed,
            },
        );
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
    ) {
        let mux = Mux::try_get();
        let callbacks = {
            let mut inner = self.inner.lock();
            let mut callbacks = inner.install_prepared_pane_tree(size, prepared);
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            callbacks
        };
        callbacks.execute(mux.as_deref());
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
    /// supplies the protocol depth/node ceilings and a stable per-tab census
    /// work ceiling so the dependency-lower mux crate does not own wire policy.
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
        const SNAPSHOT_ATTEMPTS: usize = 3;
        let arena_start = arena.len();
        let max_tree_nodes = max_total_nodes.checked_sub(arena_start).ok_or_else(|| {
            anyhow::anyhow!(
                "ordered pane arena already has {arena_start} nodes, above limit {max_total_nodes}"
            )
        })?;

        for _ in 0..SNAPSHOT_ATTEMPTS {
            let callback_free_snapshot = self
                .inner
                .lock()
                .snapshot_panes_callback_free_bounded(
                    max_depth,
                    max_tree_nodes,
                    max_census_work,
                )?;
            let BoundedCallbackFreePaneCensus {
                panes,
                tree_leaf_count,
                coherence,
                ..
            } = callback_free_snapshot;
            // Keep callback failures provisional until the final callback-free
            // census proves that the callbacks did not replace or rearrange
            // the topology/focus authority that they were observing.
            let observed = observe_ordered_panes_bounded(self.tab_id, panes, tree_leaf_count);

            let captured = {
                let inner = self.inner.lock();
                let current = match inner.snapshot_panes_callback_free_bounded(
                    max_depth,
                    max_tree_nodes,
                    max_census_work,
                ) {
                    Ok(current) => current,
                    // A callback may transiently replace valid topology with
                    // state that fails preflight. It is not authoritative
                    // until a subsequent attempt observes it before invoking
                    // pane code, so retry instead of leaking this post-callback
                    // error across the coherence fence.
                    Err(_) => continue,
                };
                if current.coherence != coherence {
                    continue;
                }

                let observed = observed?;
                if !callback_snapshot_matches(&current.panes, &observed.pane_ids)? {
                    continue;
                }
                {
                    let active = inner.raw_active_pane_callback_free_with_tree_active(
                        &observed.pane_ids,
                        current.tree_active,
                    );
                    let captured = capture_pane_arena_tree(
                        inner.pane.as_ref(),
                        Some(active),
                        inner.zoomed.as_ref().map(Arc::clone),
                        &observed.pane_ids,
                        inner.title.clone(),
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
                            workspace,
                            left_col,
                            top_row,
                        ))
                    }
                });
            }
            append.commit();
            return Ok(PaneArenaTree {
                root_index,
                node_count,
                tab_title,
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
        let mux = Mux::try_get();
        let (prior, callbacks) = {
            let mut inner = self.inner.lock();
            let (prior, mut callbacks) = inner.prepare_set_zoomed(zoomed);
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            (prior, callbacks)
        };
        callbacks.execute(mux.as_deref());
        prior
    }

    pub fn toggle_zoom(&self) {
        let mux = Mux::try_get();
        let callbacks = {
            let mut inner = self.inner.lock();
            let mut callbacks = inner.prepare_toggle_zoom();
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            callbacks
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

    pub fn add_floating_pane(
        &self,
        pane: Arc<dyn Pane>,
        rect: FloatingPaneRect,
    ) -> anyhow::Result<PositionedFloatingPane> {
        const ADMISSION_ATTEMPTS: usize = 3;
        let pane_id = observe_pane_id_for_mutation(&pane)?;

        for _ in 0..ADMISSION_ATTEMPTS {
            let snapshot = self.snapshot_panes_callback_free();
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

            let expected_identities = exact_pane_identity_set(&snapshot);
            let mux = Mux::try_get();
            let (positioned, callbacks) = {
                let mut inner = self.inner.lock();
                let current = inner.snapshot_panes_callback_free();
                if exact_pane_identity_set(&current) != expected_identities {
                    continue;
                }
                let (positioned, mut callbacks) =
                    inner.prepare_add_floating_pane(pane.clone(), pane_id, rect);
                if let Some(mux) = mux.as_deref() {
                    callbacks.reserve_topology_notifications(mux, self.tab_id);
                }
                (positioned, callbacks)
            };
            callbacks.execute(mux.as_deref());
            return Ok(positioned);
        }

        anyhow::bail!(
            "tab {} topology changed during all {ADMISSION_ATTEMPTS} floating-pane admission attempts",
            self.tab_id
        )
    }

    pub fn set_floating_pane_rect(
        &self,
        pane_id: PaneId,
        rect: FloatingPaneRect,
    ) -> Option<PositionedFloatingPane> {
        let mux = Mux::try_get();
        let (positioned, callbacks) = {
            let mut inner = self.inner.lock();
            let (positioned, mut callbacks) =
                inner.prepare_set_floating_pane_rect(pane_id, rect);
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            (positioned, callbacks)
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

    pub fn remove_floating_pane(&self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        self.inner.lock().remove_floating_pane(pane_id)
    }

    pub fn iter_floating_panes(&self) -> Vec<PositionedFloatingPane> {
        self.inner.lock().iter_floating_panes()
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
        self.inner.lock().rotate_counter_clockwise()
    }

    pub fn rotate_clockwise(&self) {
        self.inner.lock().rotate_clockwise()
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
        let mux = Mux::try_get();
        let callbacks = {
            let mut inner = self.inner.lock();
            let mut callbacks = inner.prepare_resize(size);
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            callbacks
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
        self.inner
            .lock()
            .rebuild_splits_sizes_from_contained_panes()
    }

    /// Given split_index, the topological index of a split returned by
    /// iter_splits() as PositionedSplit::index, revised the split position
    /// by the provided delta; positive values move the split to the right/bottom,
    /// and negative values to the left/top.
    /// The adjusted size is propogated downwards to contained children and
    /// their panes are resized accordingly.
    pub fn resize_split_by(&self, split_index: usize, delta: isize) {
        let mux = Mux::try_get();
        let callbacks = {
            let mut inner = self.inner.lock();
            let mut callbacks = inner.prepare_resize_split_by(split_index, delta);
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            callbacks
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
        let mux = Mux::try_get();
        let (updated, callbacks) = {
            let mut inner = self.inner.lock();
            let (updated, mut callbacks) = inner.prepare_update_pane_constraints(
                pane_id,
                min_width,
                max_width,
                min_height,
                max_height,
            );
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            (updated, callbacks)
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
        self.inner.lock().swap_to_next_layout()
    }

    /// Swap to the previous layout in the cycle.
    pub fn swap_to_prev_layout(&self) -> Option<String> {
        self.inner.lock().swap_to_prev_layout()
    }

    /// Swap to a specific layout by index in the cycle.
    pub fn swap_to_layout_index(&self, index: usize) -> Option<String> {
        self.inner.lock().swap_to_layout_index(index)
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
        let mux = Mux::try_get();
        let callbacks = {
            let mut inner = self.inner.lock();
            let mut callbacks = inner.prepare_adjust_pane_size(direction, amount);
            if let Some(mux) = mux.as_deref() {
                callbacks.reserve_topology_notifications(mux, self.tab_id);
            }
            callbacks
        };
        callbacks.execute(mux.as_deref());
    }

    /// Activate an adjacent pane in the specified direction.
    /// In cases where there are multiple adjacent panes in the
    /// intended direction, we take the pane that has the largest
    /// edge intersection.
    pub fn activate_pane_direction(&self, direction: PaneDirection) {
        self.inner.lock().activate_pane_direction(direction)
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

    /// Freeze the structural pane pointers and retain the topology lock while
    /// `f` commits a callback-free mux transaction derived from that snapshot.
    /// Any mux registry guards must be acquired before entering; the callback
    /// must not invoke pane code or attempt to reacquire this tab.
    pub(crate) fn with_pane_snapshot_callback_free<R>(
        &self,
        f: impl FnOnce(Vec<Arc<dyn Pane>>) -> R,
    ) -> R {
        let inner = self.inner.lock();
        f(inner.snapshot_panes_callback_free())
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
    pub(crate) fn with_pane_snapshots_callback_free<R>(
        tabs: &[Arc<Self>],
        expected: Option<(&Arc<Self>, &PaneOperationGuard)>,
        f: impl FnOnce(Vec<Vec<Arc<dyn Pane>>>) -> R,
    ) -> Option<R> {
        let mut lock_order = tabs.iter().enumerate().collect::<Vec<_>>();
        lock_order.sort_unstable_by_key(|(_, tab)| Arc::as_ptr(tab) as usize);
        debug_assert!(lock_order.windows(2).all(|pair| {
            !Arc::ptr_eq(pair[0].1, pair[1].1)
        }));

        let guards = lock_order
            .iter()
            .map(|(_, tab)| tab.inner.lock())
            .collect::<Vec<_>>();
        let mut snapshots = vec![Vec::new(); tabs.len()];
        for ((original_index, _), guard) in lock_order.iter().zip(&guards) {
            snapshots[*original_index] = guard.snapshot_panes_callback_free();
        }

        if let Some((expected_tab, operation)) = expected {
            let expected_index = tabs
                .iter()
                .position(|tab| Arc::ptr_eq(tab, expected_tab))?;
            if !snapshots[expected_index]
                .iter()
                .any(|pane| operation.is_same_pane(pane))
            {
                return None;
            }
        }

        let result = f(snapshots);
        drop(guards);
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

    /// Execute `f` only while this tab still structurally contains the exact
    /// pane held by `operation`.
    ///
    /// The callback-free tab lock remains held throughout `f`, so a delayed
    /// destructive transaction can bind its commit to both tab identity and
    /// pane-generation authority. Any mux registry guards must be acquired
    /// before entering; the callback must not invoke pane code or attempt to
    /// reacquire this tab.
    pub(crate) fn with_exact_pane_operation<R>(
        &self,
        operation: &PaneOperationGuard,
        f: impl FnOnce(Vec<Arc<dyn Pane>>) -> R,
    ) -> Option<R> {
        let inner = self.inner.lock();
        let panes = inner.snapshot_panes_callback_free();
        if panes.iter().any(|pane| operation.is_same_pane(pane)) {
            Some(f(panes))
        } else {
            None
        }
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

        let (callbacks, registrations) = {
            // Registry publication/retirement is serialized before topology
            // mutation. No pane trait method is invoked in this scope.
            let _registration = mux.pane_registration.lock();
            let registered = mux.panes.read();
            let authorized = candidates
                .iter()
                .filter(|candidate| {
                    let current = registered.get(&candidate.pane_id);
                    match &candidate.expected_registration {
                        Some(expected) => current.is_some_and(|current| {
                            Arc::ptr_eq(&current.pane, &candidate.pane)
                                && expected.same_registration(&PaneRegistrationHandle::new(
                                    &current.pane,
                                    &current.generation,
                                ))
                        }),
                        None => current
                            .is_none_or(|current| !Arc::ptr_eq(&current.pane, &candidate.pane)),
                    }
                })
                .cloned()
                .collect::<Vec<_>>();

            let mut inner = self.inner.lock();
            let mut callbacks =
                inner.remove_exact_panes_callback_free(observed, &authorized);
            callbacks.reserve_topology_notifications(mux, self.tab_id);
            let registrations = authorized
                .iter()
                .filter(|candidate| callbacks.removed.contains(&pane_identity(&candidate.pane)))
                .filter_map(|candidate| candidate.expected_registration.clone())
                .collect::<Vec<_>>();
            (callbacks, registrations)
        };

        let changed = callbacks.changed;
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
        let callbacks = inner.remove_exact_panes_callback_free(&observed, &candidates);
        let changed = callbacks.changed;
        drop(inner);
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
                            observed
                                .pane
                                .mux_registration_slot()
                                .load()
                                .is_some_and(|registration| {
                                    registration.guards_detached_topology(mux, &observed.pane)
                                })
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
        self.inner.lock().remove_pane(pane_id)
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
        let observed = Self::observe_panes(self.snapshot_panes_callback_free());
        let candidate = observed.iter().find_map(|observed| {
            observed
                .pane_id
                .filter(|_| Arc::ptr_eq(&observed.pane, expected))
                .map(|pane_id| ExactPaneRemovalCandidate {
                    pane: Arc::clone(&observed.pane),
                    pane_id,
                    expected_registration: None,
                })
        })?;
        let mut inner = self.inner.lock();
        let mut callbacks =
            inner.remove_exact_panes_callback_free(&observed, std::slice::from_ref(&candidate));
        callbacks.reserve_topology_notifications(mux, self.tab_id);
        drop(inner);
        let removed = callbacks.removed.contains(&pane_identity(&candidate.pane));
        callbacks.execute(Some(mux));
        removed.then_some(candidate.pane)
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
        let mux = Mux::try_get();
        self.inner.lock().set_active_pane(pane, mux.as_deref())
    }

    /// Select a pane while routing the resulting notification through the
    /// exact mux that owns the surrounding topology.
    pub(crate) fn set_active_pane_for_mux(&self, pane: &Arc<dyn Pane>, mux: &Mux) -> bool {
        self.inner.lock().set_active_pane(pane, Some(mux))
    }

    pub fn set_active_idx(&self, pane_index: usize) {
        self.inner.lock().set_active_idx(pane_index)
    }

    /// Assigns the root pane.
    /// This is suitable when creating a new tab and then assigning
    /// the initial pane
    pub fn assign_pane(&self, pane: &Arc<dyn Pane>) {
        self.inner.lock().assign_pane(pane)
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
        self.inner.lock().compute_split_size(pane_index, request)
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
        self.inner
            .lock()
            .split_and_insert(pane_index, request, pane)
    }

    pub fn get_zoomed_pane(&self) -> Option<Arc<dyn Pane>> {
        self.inner.lock().get_zoomed_pane()
    }
}

impl TabInner {
    fn new(size: &TerminalSize) -> Self {
        Self {
            id: crate::next_unique_usize_id(&TAB_ID, "mux tab"),
            pane: Some(Tree::new()),
            floating_panes: vec![],
            floating_focus: None,
            size: *size,
            size_before_zoom: *size,
            active: 0,
            zoomed: None,
            title: String::new(),
            recency: Recency::default(),
            collapsed_panes: HashSet::new(),
            layout_cycle: Some(crate::layout::default_cycle()),
            pane_stacks: HashMap::new(),
            constraint_overrides: HashMap::new(),
        }
    }

    fn install_prepared_pane_tree(
        &mut self,
        size: TerminalSize,
        prepared: PreparedPaneTree,
    ) -> DeferredTabCallbacks {
        let PreparedPaneTree {
            tree,
            active,
            zoomed,
        } = prepared;
        let mut cursor = tree.cursor();

        self.active = 0;
        if let Some(active) = active {
            // Resolve the active pane to its index
            let mut index = 0;
            loop {
                if let Some(pane) = cursor.leaf_mut() {
                    if Arc::ptr_eq(&active, pane) {
                        // Found it
                        self.active = index;
                        self.recency.tag(index);
                        break;
                    }
                    index += 1;
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(c) => {
                        // Didn't find it
                        cursor = c;
                        break;
                    }
                }
            }
        }
        self.pane.replace(cursor.tree());
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
        let mut callbacks = DeferredTabCallbacks {
            changed: true,
            ..DeferredTabCallbacks::default()
        };
        if size.rows != 0 && size.cols != 0 {
            if let Some(zoomed) = self.zoomed.as_ref() {
                callbacks.resize_work.push((Arc::clone(zoomed), size));
            } else if let Some(tree) = self.pane.as_ref() {
                collect_pane_resize_work(tree, &size, &mut callbacks.resize_work);
            }
        }

        log::debug!(
            "sync tab: {:#?} zoomed: {}",
            size,
            self.zoomed.is_some(),
        );
        assert!(self.pane.is_some());
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

    /// Legacy inner call surface retained while older compound mutations are
    /// migrated to return deferred callbacks to their outer `Tab` boundary.
    /// New code must use `prepare_set_zoomed`/`prepare_toggle_zoom`.
    fn set_zoomed(&mut self, zoomed: bool) -> bool {
        let mux = Mux::try_get();
        let (prior, mut callbacks) = self.prepare_set_zoomed(zoomed);
        if let Some(mux) = mux.as_deref() {
            callbacks.reserve_topology_notifications(mux, self.id);
        }
        callbacks.execute(mux.as_deref());
        prior
    }

    fn toggle_zoom(&mut self) {
        let mux = Mux::try_get();
        let mut callbacks = self.prepare_toggle_zoom();
        if let Some(mux) = mux.as_deref() {
            callbacks.reserve_topology_notifications(mux, self.id);
        }
        callbacks.execute(mux.as_deref());
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
        &mut self,
        pane: Arc<dyn Pane>,
        pane_id: PaneId,
        rect: FloatingPaneRect,
    ) -> (PositionedFloatingPane, DeferredTabCallbacks) {
        let prior = self.raw_active_pane_retained_id();
        let rect = self.clamp_floating_rect(rect);
        let pane_size = self.floating_pane_size(rect);
        let z_order = self.next_floating_z_order();

        let floating = FloatingPane {
            pane: Arc::clone(&pane),
            pane_id,
            rect,
            z_order,
            visible: true,
            pinned: false,
            opacity: 1.0,
        };
        self.floating_panes.push(floating);
        self.floating_focus = Some(pane_id);
        let positioned = self
            .positioned_floating_pane(self.floating_panes.last().expect("floating pane added"));
        let mut callbacks = DeferredTabCallbacks {
            changed: true,
            prior_focus: prior,
            current_focus: Some(Arc::clone(&pane)),
            current_focus_id: Some(pane_id),
            ..DeferredTabCallbacks::default()
        };
        callbacks.resize_work.push((pane, pane_size));
        (positioned, callbacks)
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
        callbacks.resize_work.push((Arc::clone(&floating.pane), size));
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

    fn remove_floating_pane(&mut self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        let idx = self.floating_index_by_id(pane_id)?;
        let prior = self.get_active_pane();
        let removed = self.floating_panes.remove(idx);
        if self.floating_focus == Some(pane_id) {
            self.floating_focus = None;
        }
        self.discard_removed_pane_state(pane_id);
        self.advise_focus_change(prior);
        Some(removed.pane)
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

    fn remove_stacked_pane(&mut self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        let mut removed = None;
        let mut empty_slot = None;
        for (slot_index, stack) in &mut self.pane_stacks {
            if let Some(pane) = stack.remove(pane_id) {
                if stack.is_empty() {
                    empty_slot = Some(*slot_index);
                }
                removed = Some(pane);
                break;
            }
        }
        if let Some(slot_index) = empty_slot {
            self.pane_stacks.remove(&slot_index);
        }
        if removed.is_some() {
            self.discard_removed_pane_state(pane_id);
        }
        removed
    }

    /// Remove the visible member of a stack while preserving the invariant
    /// that the stack's active pane is represented by the corresponding tree
    /// leaf.
    ///
    /// `None` means that the slot is not a stack whose active pane matches
    /// `pane_id`. `Some(None)` means that the removed pane was the stack's last
    /// member and the caller must remove the tree leaf. `Some(Some(pane))`
    /// supplies the survivor that must replace the removed tree leaf.
    fn remove_visible_stacked_pane(
        &mut self,
        slot_index: usize,
        pane_id: PaneId,
    ) -> Option<Option<Arc<dyn Pane>>> {
        let stack = self.pane_stacks.get_mut(&slot_index)?;
        if stack.active_pane().pane_id() != pane_id {
            return None;
        }

        stack.remove(pane_id)?;
        if stack.is_empty() {
            self.pane_stacks.remove(&slot_index);
            Some(None)
        } else {
            Some(Some(Arc::clone(stack.active_pane())))
        }
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
    fn swap_to_next_layout(&mut self) -> Option<String> {
        let cycle = self.layout_cycle.as_mut()?;
        let layout = cycle.advance().clone();
        self.apply_layout(&layout)
    }

    /// Swap to the previous layout in the cycle.
    fn swap_to_prev_layout(&mut self) -> Option<String> {
        let cycle = self.layout_cycle.as_mut()?;
        let layout = cycle.prev().clone();
        self.apply_layout(&layout)
    }

    /// Swap to a specific layout by index in the cycle.
    fn swap_to_layout_index(&mut self, index: usize) -> Option<String> {
        let cycle = self.layout_cycle.as_mut()?;
        if !cycle.select(index) {
            return None;
        }
        let layout = cycle.current().clone();
        self.apply_layout(&layout)
    }

    /// Apply a layout, redistributing panes from the current tree.
    fn apply_layout(&mut self, layout: &SwapLayout) -> Option<String> {
        // Collect all panes from the current tree AND from any existing stacks.
        let all_panes = self.collect_all_panes();
        if all_panes.is_empty() {
            return None;
        }

        let active_pane_id = self
            .get_active_pane()
            .map(|p| p.pane_id())
            .unwrap_or_else(|| all_panes[0].pane_id());

        let result = redistribute_panes(&layout.arrangement, all_panes, active_pane_id, self.size)?;

        self.pane = Some(result.tree);
        self.pane_stacks = result.stacks;
        self.active = result.active_index;
        self.zoomed = None;
        self.collapsed_panes.clear();

        // Apply sizes to the new tree.
        let size = self.size;
        if let Some(tree) = self.pane.as_mut() {
            apply_sizes_from_splits(tree, &size);
        }

        // Notify about the focus change.
        if let Some(pane) = self.get_active_pane() {
            self.recency.tag(self.active);
            Mux::try_get().map(|mux| {
                mux.notify(MuxNotification::PaneFocused(pane.pane_id()));
            });
        }

        Some(layout.name.clone())
    }

    /// Collect all panes: from the tree leaves AND from stacked (hidden) panes.
    fn collect_all_panes(&mut self) -> Vec<Arc<dyn Pane>> {
        let mut panes: Vec<Arc<dyn Pane>> = Vec::new();

        // Collect from tree leaves.
        let positioned = self.iter_panes_ignoring_zoom();
        for pp in &positioned {
            panes.push(pp.pane.clone());
        }

        // Collect from stacks (non-visible panes that aren't already in the tree).
        // Use std::mem::take for an atomic swap instead of drain(), which would
        // leave pane_stacks empty if anything accesses it before reassignment.
        let tree_ids: HashSet<PaneId> = panes.iter().map(|p| p.pane_id()).collect();
        let old_stacks = std::mem::take(&mut self.pane_stacks);
        for (_slot, stack) in old_stacks {
            for p in stack.into_panes() {
                if !tree_ids.contains(&p.pane_id()) {
                    panes.push(p);
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
    /// limit counts every raw carrier visited across tree leaves, stack
    /// containers and members, floating panes, and the zoom carrier; it also
    /// caps the smaller set of unique pane identities. Both are deliberately
    /// enforced while `Tab::inner` is held and before `Pane::pane_id` or any
    /// rendering callback can run. This prevents a topology that will be
    /// rejected by the wire contract from first consuming unbounded native
    /// traversal, callback, or identity-snapshot work.
    fn snapshot_panes_callback_free_bounded(
        &self,
        max_depth: usize,
        max_tree_nodes: usize,
        max_census_work: usize,
    ) -> anyhow::Result<BoundedCallbackFreePaneCensus> {
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
                anyhow::bail!(
                    "tab {} pane tree depth {depth} exceeds ordered snapshot limit {max_depth}",
                    self.id
                );
            }
            tree_nodes = tree_nodes.checked_add(1).ok_or_else(|| {
                anyhow::anyhow!("tab {} ordered pane tree node count overflows usize", self.id)
            })?;
            if tree_nodes > max_tree_nodes {
                anyhow::bail!(
                    "tab {} ordered pane tree has more than {max_tree_nodes} nodes",
                    self.id
                );
            }

            reserve_pane_arena_stack_push(
                &mut tree_coherence,
                1,
                max_tree_nodes,
                "ordered pane tree coherence",
            )?;
            match tree {
                Tree::Empty => tree_coherence.push(OrderedPaneTreeCoherenceNode::Empty),
                Tree::Leaf(pane) => {
                    tree_coherence.push(OrderedPaneTreeCoherenceNode::Leaf(pane_identity(pane)));
                    admit_ordered_pane_census_work(
                        &mut census_work,
                        max_census_work,
                        self.id,
                    )?;
                    let leaf_index = tree_leaf_identities.len();
                    if push_bounded_callback_free_pane(
                        &mut owners,
                        &mut panes,
                        pane,
                        CallbackFreePaneOwner::TreeLeaf(leaf_index),
                        max_census_work,
                        self.id,
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
                Tree::Node {
                    left,
                    right,
                    data,
                } => {
                    let node = data.ok_or_else(|| {
                        anyhow::anyhow!(
                            "tab {} ordered pane tree has an uninitialized split node",
                            self.id
                        )
                    })?;
                    tree_coherence.push(OrderedPaneTreeCoherenceNode::Split(node));
                    let next_depth = depth.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("tab {} ordered pane tree depth overflows usize", self.id)
                    })?;
                    if next_depth > max_depth {
                        anyhow::bail!(
                            "tab {} pane tree depth {next_depth} exceeds ordered snapshot limit {max_depth}",
                            self.id
                        );
                    }
                    let discovered_nodes = tree_nodes
                        .checked_add(pending.len())
                        .and_then(|count| count.checked_add(2))
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "tab {} ordered pane tree node count overflows usize",
                                self.id
                            )
                        })?;
                    if discovered_nodes > max_tree_nodes {
                        anyhow::bail!(
                            "tab {} ordered pane tree has more than {max_tree_nodes} nodes",
                            self.id
                        );
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
            anyhow::bail!(
                "tab {} ordered pane tree contains no pane leaves",
                self.id
            );
        }
        let active_identity = tree_leaf_identities.get(self.active).copied().ok_or_else(|| {
            anyhow::anyhow!(
                "tab {} ordered active pane index {} is beyond {} tree leaves",
                self.id,
                self.active,
                tree_leaf_identities.len()
            )
        })?;
        if owners.get(&active_identity)
            != Some(&CallbackFreePaneOwner::TreeLeaf(self.active))
        {
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
            let expected_active = tree_leaf_identities.get(*slot_index).copied().ok_or_else(|| {
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
            )?;
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
                title: self.title.clone(),
            },
        })
    }

    fn raw_tree_active_pane(&self) -> Option<Arc<dyn Pane>> {
        let tree = self.pane.as_ref()?;
        let mut leaves = Vec::new();
        collect_raw_tree_leaves(tree, &mut leaves);
        leaves.get(self.active).cloned()
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

    fn rotate_counter_clockwise(&mut self) {
        let panes = self.iter_panes_ignoring_zoom();
        if panes.is_empty() {
            // Shouldn't happen, but we check for this here so that the
            // expect below cannot trigger a panic
            return;
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
                    let size = self.size;
                    apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size);
                    break;
                }
            }
        }
        self.reindex_pane_stacks_from_tree();
    }

    fn rotate_clockwise(&mut self) {
        let panes = self.iter_panes_ignoring_zoom();
        if panes.is_empty() {
            // Shouldn't happen, but we check for this here so that the
            // expect below cannot trigger a panic
            return;
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
                    let size = self.size;
                    apply_sizes_from_splits(self.pane.as_mut().unwrap(), &size);
                    break;
                }
            }
        }
        self.reindex_pane_stacks_from_tree();
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
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
                self.pane.as_ref().expect("pane tree retained during resize"),
                &size,
                &mut callbacks.resize_work,
            );
        }

        self.prepare_floating_pane_resizes_to_fit(&mut callbacks.resize_work);
        callbacks.changed = true;
        callbacks
    }

    fn resize(&mut self, size: TerminalSize) {
        let mux = Mux::try_get();
        let mut callbacks = self.prepare_resize(size);
        if let Some(mux) = mux.as_deref() {
            callbacks.reserve_topology_notifications(mux, self.id);
        }
        callbacks.execute(mux.as_deref());
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

    fn rebuild_splits_sizes_from_contained_panes(&mut self) {
        if self.zoomed.is_some() {
            return;
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

        if let Some(root) = self.pane.as_mut() {
            if let Some(size) = compute_size(root) {
                self.size = size;
            }
        }
        Mux::try_get().map(|mux| mux.notify(MuxNotification::TabResized(self.id)));
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

    fn activate_pane_direction(&mut self, direction: PaneDirection) {
        if self.zoomed.is_some() {
            if !configuration().unzoom_on_switch_pane {
                return;
            }
            self.toggle_zoom();
        }
        if let Some(panel_idx) = self.get_pane_direction(direction, false) {
            self.set_active_idx(panel_idx);
        }
        if let Some(mux) = Mux::try_get() {
            if let Some(window_id) = mux.window_containing_tab(self.id) {
                mux.notify(MuxNotification::WindowInvalidated(window_id));
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

    fn remove_pane(&mut self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        if let Some(pane) = self.remove_floating_pane(pane_id) {
            return Some(pane);
        }
        let panes = self.remove_pane_if(|_, pane| pane.pane_id() == pane_id);
        panes
            .into_iter()
            .next()
            .or_else(|| self.remove_stacked_pane(pane_id))
    }

    fn remove_pane_if<F>(&mut self, f: F) -> Vec<Arc<dyn Pane>>
    where
        F: Fn(usize, &Arc<dyn Pane>) -> bool,
    {
        let mut dead_panes = vec![];
        let zoomed_pane = self.zoomed.as_ref().map(|p| p.pane_id());

        {
            let root_size = self.size;
            let mut cursor = self.pane.take().unwrap().cursor();
            let mut pane_index = 0;
            let mut removed_indices = vec![];
            let cell_dims = self.cell_dimensions();

            loop {
                // Figure out the available size by looking at our immediate parent node.
                // If we are the root, look at the tab size
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
                    let pane = Arc::clone(cursor.leaf_mut().unwrap());
                    if f(pane_index, &pane) {
                        if Some(pane.pane_id()) == zoomed_pane {
                            // If we removed the zoomed pane, un-zoom our state!
                            self.zoomed.take();
                        }
                        match self.remove_visible_stacked_pane(pane_index, pane.pane_id()) {
                            Some(Some(replacement)) => {
                                dead_panes.push(pane);
                                replacement.resize(pane_size).ok();
                                *cursor
                                    .leaf_mut()
                                    .expect("visible stacked pane must remain a tree leaf") =
                                    replacement;
                            }
                            Some(None) | None => {
                                removed_indices.push(pane_index);
                                let size;
                                match cursor.unsplit_leaf() {
                                    Ok((c, dead, p)) => {
                                        dead_panes.push(dead);
                                        size = if let Some(parent) = p {
                                            TerminalSize {
                                                rows: parent.height(),
                                                cols: parent.width(),
                                                pixel_width: pixel_span(
                                                    cell_dims.pixel_width,
                                                    parent.width(),
                                                ),
                                                pixel_height: pixel_span(
                                                    cell_dims.pixel_height,
                                                    parent.height(),
                                                ),
                                                dpi: cell_dims.dpi,
                                            }
                                        } else {
                                            log::warn!(
                                                "removed pane {} from split without size metadata",
                                                pane.pane_id()
                                            );
                                            pane_size
                                        };
                                        cursor = c;
                                    }
                                    Err(c) => {
                                        // We might be the root, for example
                                        if c.is_top() && c.is_leaf() {
                                            self.pane.replace(Tree::Empty);
                                            dead_panes.push(pane);
                                        } else {
                                            self.pane.replace(c.tree());
                                        }
                                        break;
                                    }
                                };

                                if let Some(unsplit) = cursor.leaf_mut() {
                                    unsplit.resize(size).ok();
                                } else {
                                    self.apply_pane_size(size, &mut cursor);
                                }
                            }
                        }
                    } else if !dead_panes.is_empty() {
                        // Apply our revised size to the tty
                        pane.resize(pane_size).ok();
                    }

                    pane_index += 1;
                } else if !dead_panes.is_empty() {
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

            // Figure out which pane should now be active.
            // If panes earlier than the active pane were closed, then we
            // need to shift the active pane down
            let active_idx = self.active;
            removed_indices.retain(|&idx| idx <= active_idx);
            self.active = active_idx.saturating_sub(removed_indices.len());
        }
        self.reindex_pane_stacks_from_tree();

        for pane in &dead_panes {
            let pid = pane.pane_id();
            self.discard_removed_pane_state(pid);
            self.remove_stacked_pane(pid);
        }
        dead_panes
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

    fn set_active_pane(&mut self, pane: &Arc<dyn Pane>, mux: Option<&Mux>) -> bool {
        let prior = self.get_active_pane();

        if is_pane(pane, &prior.as_ref()) {
            return true;
        }

        if self.zoomed.is_some() {
            if !configuration().unzoom_on_switch_pane {
                return false;
            }
            self.toggle_zoom();
        }

        if let Some(index) = self
            .floating_panes
            .iter()
            .position(|floating| Arc::ptr_eq(&floating.pane, pane))
        {
            if !self.floating_panes[index].visible {
                return false;
            }
            let next_z = self.next_floating_z_order();
            self.floating_focus = Some(pane.pane_id());
            self.floating_panes[index].z_order = next_z;
            self.advise_focus_change_with_mux(prior, mux);
            return true;
        }

        if let Some(item) = self
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|positioned| Arc::ptr_eq(&positioned.pane, pane))
        {
            self.active = item.index;
            self.recency.tag(item.index);
            self.clear_floating_focus();
            self.advise_focus_change_with_mux(prior, mux);
            return true;
        }
        false
    }

    fn advise_focus_change(&mut self, prior: Option<Arc<dyn Pane>>) {
        let mux = Mux::try_get();
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

        // Ensure that we're not zoomed, otherwise we'll end up in
        // a bogus split state (https://github.com/wezterm/wezterm/issues/723)
        self.set_zoomed(false);

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
        if self.zoomed.is_some() {
            anyhow::bail!("cannot split while zoomed");
        }

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
                if request.target_is_second {
                    self.resize(split_info.first.clone());
                } else {
                    self.resize(split_info.second.clone());
                }
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
                        return Ok(pane_index);
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

            pane1.resize(split_info.first)?;
            pane2.resize(split_info.second.clone())?;

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

        Ok(if request.target_is_second {
            next_pane_index(pane_index)
        } else {
            pane_index
        })
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

#[derive(Deserialize, Clone, Serialize, PartialEq, Debug)]
#[serde(try_from = "String", into = "String")]
pub struct SerdeUrl {
    pub url: Url,
}

impl std::convert::TryFrom<String> for SerdeUrl {
    type Error = url::ParseError;
    fn try_from(s: String) -> Result<SerdeUrl, url::ParseError> {
        let url = Url::parse(&s)?;
        Ok(SerdeUrl { url })
    }
}

impl From<Url> for SerdeUrl {
    fn from(url: Url) -> SerdeUrl {
        SerdeUrl { url }
    }
}

impl Into<Url> for SerdeUrl {
    fn into(self) -> Url {
        self.url
    }
}

impl Into<String> for SerdeUrl {
    fn into(self) -> String {
        self.url.as_str().into()
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
            self.panic_if_ordered_observation_callback(
                OrderedObservationCallback::CursorPosition,
            );
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
            format!("fake-pane-{}", self.id)
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
            None
        }
        fn tty_name(&self) -> Option<String> {
            self.panic_if_ordered_observation_callback(OrderedObservationCallback::TtyName);
            None
        }
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
            left: Box::new(balanced_mux_tree_for_capture(
                first_pane,
                left_leaves,
                size,
            )),
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
                    leaf_count,
                )
                .unwrap_or_else(|error| {
                    panic!(
                        "q={} full flat append failed: {:#}",
                        leaf_count, error
                    )
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

    fn flatten_legacy_pane_node_for_test(
        node: &PaneNode,
        arena: &mut Vec<PaneArenaNode>,
    ) -> u32 {
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
            .append_codec_pane_arena_in_window(
                96,
                "prefix-invariant",
                &mut empty_prefix,
                64,
                1,
                2,
            )
            .expect("tab must fit at the start of an arena");

        let prefix = PaneArenaNode::Leaf(pane_arena_test_entry(997, false, false));
        let mut later_prefix = vec![prefix.clone(); 9];
        let second = tab
            .append_codec_pane_arena_in_window(
                96,
                "prefix-invariant",
                &mut later_prefix,
                64,
                10,
                2,
            )
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
        assert!(format!("{depth_error:#}").contains("depth 3"));
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
        assert!(format!("{node_error:#}").contains("more than 4 nodes"));
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
            .append_codec_pane_arena_in_window(95, "exact-depth", &mut arena, 64, 127, 64)
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
        let visible = FakePane::new_with_pane_id_probe(
            800,
            size,
            Arc::clone(&callback_probe),
        );
        let hidden = FakePane::new_with_pane_id_probe(
            801,
            size,
            Arc::clone(&callback_probe),
        );
        let floating = FakePane::new_with_pane_id_probe(
            802,
            size,
            Arc::clone(&callback_probe),
        );
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
            .append_codec_pane_arena_in_window(
                92,
                "census-workspace",
                &mut arena,
                64,
                16,
                5,
            )
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
        assert!(
            format!("{exact_error:#}").contains("exact pane identity appears more than once")
        );
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
        assert!(
            format!("{numeric_error:#}")
                .contains("pane id 901 belongs to more than one exact pane identity")
        );
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
            inner.pane_stacks.insert(
                0,
                PaneStack::new(vec![visible, Arc::clone(&shared)]),
            );
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
            .append_codec_pane_arena_in_window(
                98,
                "cross-owner",
                &mut cross_arena,
                64,
                16,
                16,
            )
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
            .append_codec_pane_arena_in_window(
                101,
                "invalid-active",
                &mut arena,
                64,
                16,
                16,
            )
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
        let [
            PaneArenaNode::Split { .. },
            PaneArenaNode::Leaf(first),
            PaneArenaNode::Leaf(second),
        ] = arena.as_slice()
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
        let [
            PaneArenaNode::Split { .. },
            PaneArenaNode::Leaf(first),
            PaneArenaNode::Leaf(second),
        ] = arena.as_slice()
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
                    panic!(
                        "{:?} panic escaped the mux observation boundary",
                        callback
                    )
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
            assert_eq!(
                arena,
                [prefix],
                "arena changed after {callback:?} panic"
            );
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
            observed.iter().map(|(pane_id, _)| *pane_id).collect::<Vec<_>>(),
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
        tab.sync_with_prepared_pane_tree(size, prepared);
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
        tab.sync_with_prepared_pane_tree(size, prepared);

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
        let left = append_balanced_pane_arena_slots(
            nodes,
            first_leaf,
            left_slots,
            left_empty_slots,
        );
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
        let error = match prepare_pane_tree_from_arena_with_scratch(
            &mut arena,
            3,
            &mut scratch,
            |entry| Ok(FakePane::new(entry.pane_id, entry.size)),
        ) {
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
        assert!(arena.is_empty(), "the failed tree range must be consumed once");
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
        assert!(arena.is_empty(), "the failed tree range must be consumed once");
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
        assert_eq!(serde_url.url, url);
    }

    #[test]
    fn serde_url_try_from_string() {
        let serde_url = SerdeUrl::try_from("https://example.com".to_string());
        assert!(serde_url.is_ok());
        assert_eq!(serde_url.unwrap().url.as_str(), "https://example.com/");
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
