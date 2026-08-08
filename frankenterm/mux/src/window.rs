use crate::pane::CloseReason;
use crate::tab::{TabStackEntry, TabStackError, TabStackId, TabStackState};
use crate::{Mux, MuxNotification, Pane, Tab, TabId, DEFAULT_WORKSPACE};
use config::GuiPosition;
use frankenterm_sigpipe::{catch_recoverable, RecoverablePanicSite};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use thiserror::Error;

static WIN_ID: ::std::sync::atomic::AtomicUsize = ::std::sync::atomic::AtomicUsize::new(0);
pub type WindowId = usize;

/// Maximum tab count represented by the v1 ordered-window authority.
///
/// `codec` depends on `mux`, so this foundational crate cannot import the
/// corresponding wire constant. Keep this value equal to
/// `codec::MAX_ORDERED_TABS_PER_WINDOW`; the server adapter must reject a
/// disagreement before advertising the protocol capability.
pub const MAX_TABS_PER_ORDERED_WINDOW: usize = 4_096;

/// Per-window revision of membership, order, and active-tab identity.
///
/// `u64::MAX` is a terminal sentinel and is never a valid published revision.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WindowOrderRevision(u64);

impl WindowOrderRevision {
    pub const INITIAL: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    fn checked_successor(self) -> Result<Self, WindowOrderRevisionExhausted> {
        let next = self.0.checked_add(1).ok_or(WindowOrderRevisionExhausted)?;
        if next == u64::MAX {
            return Err(WindowOrderRevisionExhausted);
        }
        Ok(Self(next))
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error(
    "window order revision space is exhausted; refusing to wrap, saturate, reset, or reuse a revision"
)]
pub struct WindowOrderRevisionExhausted;

/// Immutable, pointer-preserving state of one exact mux window.
///
/// Cloning this value clones only `Arc`s. It never reconstructs a `Tab`, pane,
/// or tab stack and is therefore safe to retain in a delayed notification.
#[derive(Clone)]
pub struct FrozenWindowOrder {
    window_id: WindowId,
    order_revision: WindowOrderRevision,
    ordered_tabs: Arc<[Arc<Tab>]>,
    active_tab: Option<Arc<Tab>>,
}

impl FrozenWindowOrder {
    pub const fn window_id(&self) -> WindowId {
        self.window_id
    }

    pub const fn order_revision(&self) -> WindowOrderRevision {
        self.order_revision
    }

    pub fn ordered_tabs(&self) -> &[Arc<Tab>] {
        &self.ordered_tabs
    }

    pub fn ordered_tab_ids(&self) -> impl ExactSizeIterator<Item = TabId> + '_ {
        self.ordered_tabs.iter().map(|tab| tab.tab_id())
    }

    pub fn active_tab(&self) -> Option<&Arc<Tab>> {
        self.active_tab.as_ref()
    }

    pub fn active_tab_id(&self) -> Option<TabId> {
        self.active_tab.as_ref().map(|tab| tab.tab_id())
    }
}

impl fmt::Debug for FrozenWindowOrder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FrozenWindowOrder")
            .field("window_id", &self.window_id)
            .field("order_revision", &self.order_revision)
            .field(
                "ordered_tab_ids",
                &self.ordered_tab_ids().collect::<Vec<_>>(),
            )
            .field("active_tab_id", &self.active_tab_id())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum WindowOrderSnapshotError {
    #[error("window {window_id} has {count} tabs, exceeding ordered-window limit {max}")]
    TooManyTabs {
        window_id: WindowId,
        count: usize,
        max: usize,
    },
    #[error("window {window_id} contains duplicate tab id {tab_id}")]
    DuplicateTabId { window_id: WindowId, tab_id: TabId },
    #[error("non-empty window {window_id} has no valid active tab")]
    MissingActiveTab { window_id: WindowId },
}

pub(crate) struct ValidatedWindowOrder {
    window_id: WindowId,
    prior_revision: WindowOrderRevision,
    ordered_tabs: Vec<Arc<Tab>>,
    active_index: usize,
}

impl fmt::Debug for ValidatedWindowOrder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidatedWindowOrder")
            .field("window_id", &self.window_id)
            .field("prior_revision", &self.prior_revision)
            .field(
                "ordered_tab_ids",
                &self
                    .ordered_tabs
                    .iter()
                    .map(|tab| tab.tab_id())
                    .collect::<Vec<_>>(),
            )
            .field("active_index", &self.active_index)
            .finish()
    }
}

#[derive(Debug)]
pub(crate) struct PreparedWindowOrder {
    validated: ValidatedWindowOrder,
    next_revision: WindowOrderRevision,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PrepareWindowOrderError {
    #[error(transparent)]
    RevisionExhausted(#[from] WindowOrderRevisionExhausted),
    #[error(transparent)]
    InvalidCurrentState(#[from] WindowOrderSnapshotError),
    #[error("desired order contains duplicate tab id {tab_id}")]
    DuplicateTabId { tab_id: TabId },
    #[error("desired order is missing current tab id {tab_id}")]
    MissingTabId { tab_id: TabId },
    #[error("desired order contains non-member tab id {tab_id}")]
    ForeignTabId { tab_id: TabId },
    #[error(
        "desired active tab {desired_active_tab_id:?} does not preserve current active tab {current_active_tab_id:?}"
    )]
    ActiveTabChanged {
        current_active_tab_id: Option<TabId>,
        desired_active_tab_id: Option<TabId>,
    },
}

pub struct Window {
    id: WindowId,
    owner: std::sync::Weak<Mux>,
    tabs: Vec<Arc<Tab>>,
    active: usize,
    order_revision: WindowOrderRevision,
    last_active: Option<TabId>,
    tab_stacks: TabStackState,
    workspace: String,
    title: String,
    initial_position: Option<GuiPosition>,
}

impl Window {
    /// Construct an ownerless standalone window.
    ///
    /// Production mux windows are created through [`Mux::new_empty_window`],
    /// which uses `new_for_owner` and binds all deferred notifications to that
    /// exact mux.  An unregistered standalone value must not borrow authority
    /// from the mutable process-global mux singleton.
    pub fn new(workspace: Option<String>, initial_position: Option<GuiPosition>) -> Self {
        Self::new_for_owner(workspace, initial_position, std::sync::Weak::new())
    }

    pub(crate) fn new_for_owner(
        workspace: Option<String>,
        initial_position: Option<GuiPosition>,
        owner: std::sync::Weak<Mux>,
    ) -> Self {
        let workspace = workspace.unwrap_or_else(|| {
            owner
                .upgrade()
                .map(|mux| mux.active_workspace())
                .unwrap_or_else(|| DEFAULT_WORKSPACE.to_string())
        });
        Self {
            id: crate::next_unique_usize_id(&WIN_ID, "mux window"),
            owner,
            tabs: vec![],
            active: 0,
            order_revision: WindowOrderRevision::INITIAL,
            last_active: None,
            tab_stacks: TabStackState::default(),
            title: String::new(),
            workspace,
            initial_position,
        }
    }

    fn notify(&mut self, notification: MuxNotification) {
        let Some(mux) = self.owner.upgrade() else {
            return;
        };
        // Window mutations normally occur while the mux window-map write lock
        // is held. Queue on a disjoint lock and let the owning mux guard flush
        // after unlocking, so synchronous subscribers can safely re-enter.
        mux.enqueue_window_notification(notification);
    }

    pub fn get_initial_position(&self) -> &Option<GuiPosition> {
        &self.initial_position
    }

    pub fn get_workspace(&self) -> &str {
        &self.workspace
    }

    pub fn set_title(&mut self, title: &str) {
        if self.set_title_without_notify(title) {
            self.notify(MuxNotification::WindowTitleChanged {
                window_id: self.id,
                title: title.to_string(),
            });
        }
    }

    pub(crate) fn set_title_without_notify(&mut self, title: &str) -> bool {
        if self.title == title {
            return false;
        }
        self.title = title.to_string();
        true
    }

    pub fn get_title(&self) -> &str {
        &self.title
    }

    pub fn set_workspace(&mut self, workspace: &str) {
        if workspace == self.workspace {
            return;
        }
        self.workspace = workspace.to_string();
        self.notify(MuxNotification::WindowWorkspaceChanged {
            window_id: self.id,
            workspace: self.workspace.clone(),
        });
    }

    pub fn window_id(&self) -> WindowId {
        self.id
    }

    pub const fn order_revision(&self) -> WindowOrderRevision {
        self.order_revision
    }

    pub(crate) fn next_order_revision(
        &self,
    ) -> Result<WindowOrderRevision, WindowOrderRevisionExhausted> {
        self.order_revision.checked_successor()
    }

    pub(crate) fn ensure_tab_insert_available(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.tabs.len() < MAX_TABS_PER_ORDERED_WINDOW,
            "cannot insert tab in window {}: ordered-window limit {} reached",
            self.id,
            MAX_TABS_PER_ORDERED_WINDOW,
        );
        self.next_order_revision()?;
        Ok(())
    }

    fn next_order_revision_or_panic(&self) -> WindowOrderRevision {
        self.next_order_revision()
            .unwrap_or_else(|err| panic!("window {} cannot mutate ordered state: {err}", self.id))
    }

    /// Freeze the exact ordered `Arc<Tab>` identities and active identity.
    ///
    /// This performs no pane callbacks or I/O and never consults a mutable
    /// global. A malformed legacy window fails closed rather than publishing
    /// an ambiguous ordered snapshot.
    pub fn order_snapshot(&self) -> Result<FrozenWindowOrder, WindowOrderSnapshotError> {
        if self.tabs.len() > MAX_TABS_PER_ORDERED_WINDOW {
            return Err(WindowOrderSnapshotError::TooManyTabs {
                window_id: self.id,
                count: self.tabs.len(),
                max: MAX_TABS_PER_ORDERED_WINDOW,
            });
        }
        let mut seen = HashSet::with_capacity(self.tabs.len());
        for tab in &self.tabs {
            if !seen.insert(tab.tab_id()) {
                return Err(WindowOrderSnapshotError::DuplicateTabId {
                    window_id: self.id,
                    tab_id: tab.tab_id(),
                });
            }
        }
        let active_tab = if self.tabs.is_empty() {
            None
        } else {
            Some(
                self.tabs
                    .get(self.active)
                    .cloned()
                    .ok_or(WindowOrderSnapshotError::MissingActiveTab { window_id: self.id })?,
            )
        };
        Ok(FrozenWindowOrder {
            window_id: self.id,
            order_revision: self.order_revision,
            ordered_tabs: Arc::from(self.tabs.clone()),
            active_tab,
        })
    }

    /// Validate and stage one exact permutation without consuming revision
    /// capacity or mutating the window.
    ///
    /// The returned value contains the same `Arc<Tab>` identities in their
    /// requested order. Keeping capacity reservation separate preserves the
    /// protocol outcome order: semantic malformation precedes revision
    /// conflict, which precedes counter exhaustion.
    pub(crate) fn validate_exact_order(
        &self,
        desired_tab_ids: &[TabId],
        desired_active_tab_id: Option<TabId>,
    ) -> Result<ValidatedWindowOrder, PrepareWindowOrderError> {
        if self.tabs.len() > MAX_TABS_PER_ORDERED_WINDOW {
            return Err(WindowOrderSnapshotError::TooManyTabs {
                window_id: self.id,
                count: self.tabs.len(),
                max: MAX_TABS_PER_ORDERED_WINDOW,
            }
            .into());
        }
        let current_active_tab_id = if self.tabs.is_empty() {
            None
        } else {
            Some(
                self.tabs
                    .get(self.active)
                    .ok_or(WindowOrderSnapshotError::MissingActiveTab { window_id: self.id })?
                    .tab_id(),
            )
        };
        let mut current_by_id = HashMap::with_capacity(self.tabs.len());
        for tab in &self.tabs {
            let replaced = current_by_id.insert(tab.tab_id(), (Arc::clone(tab), false));
            if replaced.is_some() {
                return Err(WindowOrderSnapshotError::DuplicateTabId {
                    window_id: self.id,
                    tab_id: tab.tab_id(),
                }
                .into());
            }
        }
        let mut ordered_tabs = Vec::with_capacity(desired_tab_ids.len());
        for &tab_id in desired_tab_ids {
            let Some((tab, already_used)) = current_by_id.get_mut(&tab_id) else {
                return Err(PrepareWindowOrderError::ForeignTabId { tab_id });
            };
            if *already_used {
                return Err(PrepareWindowOrderError::DuplicateTabId { tab_id });
            }
            *already_used = true;
            ordered_tabs.push(Arc::clone(tab));
        }
        if let Some(tab_id) = self
            .tabs
            .iter()
            .map(|tab| tab.tab_id())
            .find(|tab_id| current_by_id.get(tab_id).is_some_and(|(_, used)| !*used))
        {
            return Err(PrepareWindowOrderError::MissingTabId { tab_id });
        }

        if desired_active_tab_id != current_active_tab_id {
            return Err(PrepareWindowOrderError::ActiveTabChanged {
                current_active_tab_id,
                desired_active_tab_id,
            });
        }

        let active_index = desired_active_tab_id
            .and_then(|active_tab_id| {
                desired_tab_ids
                    .iter()
                    .position(|tab_id| *tab_id == active_tab_id)
            })
            .unwrap_or(0);
        Ok(ValidatedWindowOrder {
            window_id: self.id,
            prior_revision: self.order_revision,
            ordered_tabs,
            active_index,
        })
    }

    /// Consume counter capacity only after identity, membership, active-state,
    /// and expected-revision validation have succeeded.
    pub(crate) fn prepare_validated_order(
        &self,
        validated: ValidatedWindowOrder,
    ) -> Result<PreparedWindowOrder, WindowOrderRevisionExhausted> {
        assert_eq!(
            validated.window_id, self.id,
            "validated order changed windows"
        );
        assert_eq!(
            validated.prior_revision, self.order_revision,
            "validated order revision changed before preparation"
        );
        Ok(PreparedWindowOrder {
            validated,
            next_revision: self.next_order_revision()?,
        })
    }

    /// Commit a prevalidated order while preserving tabs, panes, active
    /// identity, last-active identity, and tab-stack state.
    ///
    /// This intentionally does not notify. The owning mux transaction must
    /// reserve one global topology revision and publish one frozen event.
    pub(crate) fn commit_prepared_order(
        &mut self,
        prepared: PreparedWindowOrder,
    ) -> FrozenWindowOrder {
        let PreparedWindowOrder {
            validated,
            next_revision,
        } = prepared;
        assert_eq!(
            validated.window_id, self.id,
            "prepared order changed windows"
        );
        assert_eq!(
            validated.prior_revision, self.order_revision,
            "prepared order revision changed before commit"
        );
        self.tabs = validated.ordered_tabs;
        self.active = validated.active_index;
        self.order_revision = next_revision;
        self.order_snapshot()
            .expect("a prevalidated exact order must remain snapshot-valid after commit")
    }

    #[cfg(test)]
    pub(crate) fn set_order_revision_for_test(&mut self, revision: WindowOrderRevision) {
        assert_ne!(revision.get(), u64::MAX);
        self.order_revision = revision;
    }

    fn ensure_tab_isnt_already_in_window(&self, tab: &Arc<Tab>) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.tabs
                .iter()
                .all(|existing| existing.tab_id() != tab.tab_id()),
            "tab {} is already attached to window {}",
            tab.tab_id(),
            self.id,
        );
        Ok(())
    }

    fn invalidate(&mut self) {
        self.notify(MuxNotification::WindowInvalidated(self.id));
    }

    /// Insert `tab` at `index` without changing the exact active tab.
    ///
    /// An empty window activates its first inserted tab. Invalid indices fail
    /// before any window state changes.
    pub fn insert(&mut self, index: usize, tab: &Arc<Tab>) -> anyhow::Result<()> {
        anyhow::ensure!(
            index <= self.tabs.len(),
            "cannot insert tab at index {index} in window {} with {} tabs",
            self.id,
            self.tabs.len(),
        );
        self.ensure_tab_insert_available()?;
        self.ensure_tab_isnt_already_in_window(tab)?;
        let next_revision = self.next_order_revision()?;
        let prior_active_index = (self.active < self.tabs.len()).then_some(self.active);
        self.tabs.insert(index, Arc::clone(tab));
        self.active = prior_active_index
            .map(|active| if index <= active { active + 1 } else { active })
            .unwrap_or(0);
        self.order_revision = next_revision;
        self.invalidate();
        Ok(())
    }

    /// Reorder one exact tab while preserving active and tab-stack identity.
    ///
    /// `source_index` must still name `expected`; `destination_index` is the
    /// tab's final index. Missing exact identity and invalid destination
    /// indices fail without mutation.
    pub(crate) fn reorder_tab_if_same(
        &mut self,
        expected: &Arc<Tab>,
        source_index: usize,
        destination_index: usize,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            destination_index < self.tabs.len(),
            "cannot move tab to index {destination_index} in window {} with {} tabs",
            self.id,
            self.tabs.len(),
        );
        let Some(source) = self.tabs.get(source_index) else {
            return Ok(false);
        };
        if !Arc::ptr_eq(source, expected) {
            return Ok(false);
        }
        if source_index == destination_index {
            return Ok(true);
        }

        let next_revision = self.next_order_revision()?;
        let prior_active_index = (self.active < self.tabs.len()).then_some(self.active);
        let tab = self.tabs.remove(source_index);
        let active_after_removal = prior_active_index.map(|active| {
            if active == source_index {
                destination_index
            } else {
                let shifted = if source_index < active {
                    active - 1
                } else {
                    active
                };
                if destination_index <= shifted {
                    shifted + 1
                } else {
                    shifted
                }
            }
        });
        self.tabs.insert(destination_index, tab);
        self.active = active_after_removal.unwrap_or(0);
        self.order_revision = next_revision;
        self.invalidate();
        Ok(true)
    }

    pub fn push(&mut self, tab: &Arc<Tab>) -> anyhow::Result<()> {
        self.insert(self.tabs.len(), tab)
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    pub fn get_by_idx(&self, idx: usize) -> Option<&Arc<Tab>> {
        self.tabs.get(idx)
    }

    pub fn can_close_without_prompting(&self) -> bool {
        for tab in &self.tabs {
            if !tab.can_close_without_prompting(CloseReason::Window) {
                return false;
            }
        }
        true
    }

    pub fn idx_by_id(&self, id: TabId) -> Option<usize> {
        for (idx, t) in self.tabs.iter().enumerate() {
            if t.tab_id() == id {
                return Some(idx);
            }
        }
        None
    }

    pub fn remove_by_idx(&mut self, idx: usize) -> Arc<Tab> {
        assert!(
            idx < self.tabs.len(),
            "cannot remove tab index {idx} from window {} with {} tabs",
            self.id,
            self.tabs.len()
        );
        let active = self.get_active().map(Arc::clone);
        self.do_remove_idx(idx, active)
    }

    pub fn remove_by_id(&mut self, id: TabId) {
        let active = self.get_active().map(Arc::clone);
        if let Some(idx) = self.idx_by_id(id) {
            self.do_remove_idx(idx, active);
        }
    }

    pub(crate) fn remove_tab_if_same(&mut self, expected: &Arc<Tab>) -> bool {
        let Some(idx) = self.tabs.iter().position(|tab| Arc::ptr_eq(tab, expected)) else {
            return false;
        };
        let active = self.get_active().map(Arc::clone);
        self.do_remove_idx(idx, active);
        true
    }

    pub(crate) fn remove_tabs_by_exact_identity_set(&mut self, removals: &HashSet<usize>) -> bool {
        if removals.is_empty() {
            return false;
        }
        let prior_active = self.get_active().map(Arc::clone);
        let prior_active_pane = prior_active
            .as_ref()
            .and_then(|tab| tab.get_active_pane_callback_free());
        let old_active_idx = self.active;
        let removed_ids = self
            .tabs
            .iter()
            .filter(|tab| removals.contains(&(Arc::as_ptr(tab) as usize)))
            .map(|tab| tab.tab_id())
            .collect::<HashSet<_>>();
        if removed_ids.is_empty() {
            return false;
        }
        let next_revision = self.next_order_revision_or_panic();
        self.tabs
            .retain(|tab| !removals.contains(&(Arc::as_ptr(tab) as usize)));

        for &tab_id in &removed_ids {
            self.tab_stacks.remove_tab(tab_id);
        }
        if self
            .last_active
            .is_some_and(|last_active| removed_ids.contains(&last_active))
        {
            self.last_active = None;
        }

        self.active = prior_active
            .as_ref()
            .and_then(|prior| {
                self.tabs
                    .iter()
                    .position(|candidate| Arc::ptr_eq(candidate, prior))
            })
            .unwrap_or_else(|| {
                if self.tabs.is_empty() {
                    0
                } else {
                    old_active_idx.min(self.tabs.len() - 1)
                }
            });

        let active_changed = prior_active.as_ref().is_some_and(|prior| {
            self.get_active()
                .is_none_or(|current| !Arc::ptr_eq(current, prior))
        });
        if active_changed {
            if let Some(pane) = prior_active_pane {
                self.enqueue_focus_lost(pane);
            }
        }
        self.order_revision = next_revision;
        self.invalidate();
        true
    }

    fn do_remove_idx(&mut self, idx: usize, active: Option<Arc<Tab>>) -> Arc<Tab> {
        let next_revision = self.next_order_revision_or_panic();
        let prior_active_pane = active
            .as_ref()
            .and_then(|tab| tab.get_active_pane_callback_free());
        let removing_is_active = active.as_ref().is_some_and(|active| {
            self.tabs
                .get(idx)
                .is_some_and(|removing| Arc::ptr_eq(active, removing))
        });
        let preferred_after_removal = if removing_is_active
            && config::configuration().switch_to_last_active_tab_when_closing_tab
        {
            self.get_last_active_idx()
                .and_then(|last_active| self.tabs.get(last_active))
                .filter(|candidate| {
                    self.tabs
                        .get(idx)
                        .is_none_or(|removing| !Arc::ptr_eq(candidate, removing))
                })
                .map(Arc::clone)
        } else {
            active.as_ref().map(Arc::clone)
        };
        let old_active_idx = self.active;
        let tab = self.tabs.remove(idx);
        if self.last_active == Some(tab.tab_id()) {
            self.last_active = None;
        }
        self.tab_stacks.remove_tab(tab.tab_id());
        self.active = preferred_after_removal
            .as_ref()
            .and_then(|preferred| {
                self.tabs
                    .iter()
                    .position(|candidate| Arc::ptr_eq(candidate, preferred))
            })
            .unwrap_or_else(|| {
                if self.tabs.is_empty() {
                    0
                } else {
                    old_active_idx.min(self.tabs.len() - 1)
                }
            });
        let active_changed = active.as_ref().is_some_and(|prior| {
            self.get_active()
                .is_none_or(|current| !Arc::ptr_eq(current, prior))
        });
        if active_changed {
            if let Some(pane) = prior_active_pane {
                self.enqueue_focus_lost(pane);
            }
        }
        self.order_revision = next_revision;
        self.invalidate();
        tab
    }

    fn enqueue_focus_lost(&self, pane: Arc<dyn Pane>) {
        if let Some(mux) = self.owner.upgrade() {
            mux.enqueue_window_focus_lost(pane);
        } else if catch_recoverable(
            RecoverablePanicSite::MuxPaneCallback,
            std::panic::AssertUnwindSafe(|| pane.focus_changed(false)),
        )
        .is_err()
        {
            log::error!(
                "pane focus-loss callback panicked for standalone window pane {:p}",
                Arc::as_ptr(&pane)
            );
        }
    }

    pub fn get_active(&self) -> Option<&Arc<Tab>> {
        self.get_by_idx(self.active)
    }

    #[inline]
    pub fn get_active_idx(&self) -> usize {
        self.active
    }

    pub fn save_last_active(&mut self) {
        self.last_active = self.get_by_idx(self.active).map(|tab| tab.tab_id());
    }

    #[inline]
    pub fn get_last_active_idx(&self) -> Option<usize> {
        if let Some(tab_id) = self.last_active {
            self.idx_by_id(tab_id)
        } else {
            None
        }
    }

    /// If `idx` is different from the current active tab,
    /// save the current tabid and then make `idx` the active
    /// tab position.
    pub fn save_and_then_set_active(&mut self, idx: usize) {
        assert!(idx < self.tabs.len());
        if idx == self.get_active_idx() {
            return;
        }
        let next_revision = self.next_order_revision_or_panic();
        self.save_last_active();
        self.set_active_without_saving_at_revision(idx, next_revision);
    }

    /// Make `idx` the active tab position.
    /// The saved tab id is not changed.
    pub fn set_active_without_saving(&mut self, idx: usize) {
        assert!(idx < self.tabs.len());
        if self.active == idx {
            return;
        }
        let next_revision = self.next_order_revision_or_panic();
        self.set_active_without_saving_at_revision(idx, next_revision);
    }

    fn set_active_without_saving_at_revision(
        &mut self,
        idx: usize,
        next_revision: WindowOrderRevision,
    ) {
        debug_assert!(idx < self.tabs.len());
        debug_assert_eq!(
            self.order_revision.checked_successor(),
            Ok(next_revision),
            "active-tab revision must be reserved from current state"
        );
        if let Some(tab) = self.tabs.get(self.active).map(Arc::clone) {
            if let Some(pane) = tab.get_active_pane_callback_free() {
                self.enqueue_focus_lost(pane);
            }
        }
        self.active = idx;
        self.order_revision = next_revision;
        self.invalidate();
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<Tab>> {
        self.tabs.iter()
    }

    pub fn create_tab_stack(
        &mut self,
        stack_id: TabStackId,
        tab_ids: Vec<TabId>,
    ) -> Result<(), TabStackError> {
        for tab_id in &tab_ids {
            if self.idx_by_id(*tab_id).is_none() {
                return Err(TabStackError::MissingTab(*tab_id));
            }
        }
        self.tab_stacks.create_stack(stack_id, tab_ids)?;
        self.invalidate();
        Ok(())
    }

    pub fn remove_tab_stack(&mut self, stack_id: TabStackId) -> Option<Vec<TabId>> {
        let tabs = self.tab_stacks.remove_stack(stack_id)?;
        self.invalidate();
        Some(tabs)
    }

    pub fn cycle_tab_stack(&mut self, stack_id: TabStackId, delta: isize) -> Option<TabId> {
        // Check terminal revision authority before mutating the stack's
        // visible cursor. `&mut self` makes the subsequent reservation stable.
        self.next_order_revision_or_panic();
        let tab_id = self.tab_stacks.cycle_visible(stack_id, delta)?;
        let idx = self.idx_by_id(tab_id)?;
        self.save_and_then_set_active(idx);
        Some(tab_id)
    }

    pub fn tab_stack_for_tab(&self, tab_id: TabId) -> Option<TabStackId> {
        self.tab_stacks.stack_for_tab(tab_id)
    }

    pub fn tab_stack_visible_tab(&self, stack_id: TabStackId) -> Option<TabId> {
        self.tab_stacks.visible_tab(stack_id)
    }

    pub fn tab_stack_entries(&self) -> Vec<TabStackEntry> {
        self.tab_stacks.overview_entries()
    }

    pub fn tab_stack_count(&self) -> usize {
        self.tab_stacks.stack_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_term::TerminalSize;
    use std::collections::{BTreeMap, HashMap, VecDeque};
    use std::convert::TryFrom;

    const CONTRACT_MAX_WINDOWS: usize = 4_096;
    const CONTRACT_MAX_TABS_PER_WINDOW: usize = 4_096;
    const CONTRACT_MAX_TOTAL_TABS: usize = 16_384;
    const CONTRACT_MAX_SERVER_RECEIPTS: usize = 4_096;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ContractWindow {
        revision: u64,
        tabs: Vec<u64>,
        active: Option<u64>,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ReorderIntent {
        session: u128,
        mutation: u128,
        window: u64,
        expected_revision: u64,
        tabs: Vec<u64>,
        active: Option<u64>,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum ContractDecision {
        Applied {
            topology_revision: u64,
            window_revision: u64,
        },
        Conflict {
            window_revision: u64,
        },
        StaleIncarnation,
        Malformed,
        Exhausted,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum ContractReply {
        Decision(ContractDecision),
        Replay(ContractDecision),
    }

    #[derive(Clone, Debug)]
    struct TabOrderContractModel {
        session: u128,
        topology_revision: u64,
        windows: BTreeMap<u64, ContractWindow>,
        tombstones: HashSet<u64>,
        receipts: HashMap<u128, (ReorderIntent, ContractDecision)>,
        receipt_order: VecDeque<u128>,
    }

    impl TabOrderContractModel {
        fn new(
            session: u128,
            topology_revision: u64,
            windows: impl IntoIterator<Item = (u64, ContractWindow)>,
        ) -> Self {
            Self {
                session,
                topology_revision,
                windows: windows.into_iter().collect(),
                tombstones: HashSet::new(),
                receipts: HashMap::new(),
                receipt_order: VecDeque::new(),
            }
        }

        fn retain_receipt(&mut self, intent: ReorderIntent, decision: ContractDecision) {
            if self.receipts.len() == CONTRACT_MAX_SERVER_RECEIPTS {
                let expired = self
                    .receipt_order
                    .pop_front()
                    .expect("a full receipt map has an insertion-order entry");
                let removed = self.receipts.remove(&expired);
                debug_assert!(removed.is_some());
            }
            let mutation = intent.mutation;
            let replaced = self.receipts.insert(mutation, (intent, decision));
            debug_assert!(replaced.is_none());
            self.receipt_order.push_back(mutation);
        }

        fn apply_reorder(&mut self, intent: ReorderIntent) -> ContractReply {
            if intent.tabs.len() > CONTRACT_MAX_TABS_PER_WINDOW {
                return ContractReply::Decision(ContractDecision::Malformed);
            }
            if intent.session != self.session {
                return ContractReply::Decision(ContractDecision::StaleIncarnation);
            }
            if !self.windows.contains_key(&intent.window) {
                return ContractReply::Decision(ContractDecision::StaleIncarnation);
            }
            if let Some((prior_intent, prior_decision)) = self.receipts.get(&intent.mutation) {
                return if prior_intent == &intent {
                    ContractReply::Replay(prior_decision.clone())
                } else {
                    ContractReply::Decision(ContractDecision::Malformed)
                };
            }

            let decision = match self.windows.get(&intent.window) {
                None => unreachable!("window identity was validated before receipt lookup"),
                Some(window) => {
                    let desired = intent.tabs.iter().copied().collect::<HashSet<_>>();
                    let current = window.tabs.iter().copied().collect::<HashSet<_>>();
                    let exact_permutation = desired.len() == intent.tabs.len()
                        && current.len() == window.tabs.len()
                        && desired == current;
                    let active_is_valid = intent.active == window.active
                        && match intent.active {
                            Some(active) => desired.contains(&active),
                            None => intent.tabs.is_empty(),
                        };

                    if !exact_permutation || !active_is_valid {
                        ContractDecision::Malformed
                    } else if intent.expected_revision != window.revision {
                        ContractDecision::Conflict {
                            window_revision: window.revision,
                        }
                    } else {
                        match (
                            self.topology_revision.checked_add(1),
                            window.revision.checked_add(1),
                        ) {
                            (Some(topology_revision), Some(window_revision))
                                if topology_revision != u64::MAX && window_revision != u64::MAX =>
                            {
                                ContractDecision::Applied {
                                    topology_revision,
                                    window_revision,
                                }
                            }
                            _ => ContractDecision::Exhausted,
                        }
                    }
                }
            };

            if let ContractDecision::Applied {
                topology_revision,
                window_revision,
            } = &decision
            {
                let window = self
                    .windows
                    .get_mut(&intent.window)
                    .expect("an applied reorder has a validated window");
                window.tabs.clone_from(&intent.tabs);
                window.active = intent.active;
                window.revision = *window_revision;
                self.topology_revision = *topology_revision;
            }

            self.retain_receipt(intent, decision.clone());
            ContractReply::Decision(decision)
        }

        fn add_tab(&mut self, window_id: u64, tab_id: u64) -> Result<(), &'static str> {
            if self.tombstones.contains(&tab_id)
                || self
                    .windows
                    .values()
                    .any(|window| window.tabs.contains(&tab_id))
            {
                return Err("tab identity reused within one session incarnation");
            }
            let window = self.windows.get(&window_id).ok_or("missing window")?;
            if window.tabs.len() >= CONTRACT_MAX_TABS_PER_WINDOW {
                return Err("destination window is at the tab bound");
            }
            let topology_revision = self
                .topology_revision
                .checked_add(1)
                .ok_or("topology revision exhausted")?;
            let window_revision = window
                .revision
                .checked_add(1)
                .ok_or("window revision exhausted")?;

            let window = self
                .windows
                .get_mut(&window_id)
                .expect("window was validated before mutation");
            window.tabs.push(tab_id);
            window.active.get_or_insert(tab_id);
            window.revision = window_revision;
            self.topology_revision = topology_revision;
            Ok(())
        }

        fn close_tab(&mut self, tab_id: u64) -> Result<(), &'static str> {
            let (window_id, tab_index) = self
                .windows
                .iter()
                .find_map(|(window_id, window)| {
                    window
                        .tabs
                        .iter()
                        .position(|candidate| *candidate == tab_id)
                        .map(|index| (*window_id, index))
                })
                .ok_or("missing tab")?;
            let window = self
                .windows
                .get(&window_id)
                .expect("containing window was just resolved");
            let topology_revision = self
                .topology_revision
                .checked_add(1)
                .ok_or("topology revision exhausted")?;
            let window_revision = window
                .revision
                .checked_add(1)
                .ok_or("window revision exhausted")?;

            let window = self
                .windows
                .get_mut(&window_id)
                .expect("containing window was just resolved");
            let closing_active = window.active == Some(tab_id);
            window.tabs.remove(tab_index);
            if closing_active {
                window.active = window
                    .tabs
                    .get(tab_index)
                    .or_else(|| window.tabs.last())
                    .copied();
            }
            window.revision = window_revision;
            self.topology_revision = topology_revision;
            self.tombstones.insert(tab_id);
            Ok(())
        }

        fn move_tab(
            &mut self,
            tab_id: u64,
            source_id: u64,
            destination_id: u64,
            destination_index: usize,
        ) -> Result<(), &'static str> {
            if source_id == destination_id {
                return Err("same-window moves use reorder CAS");
            }
            let source = self
                .windows
                .get(&source_id)
                .ok_or("missing source window")?;
            let destination = self
                .windows
                .get(&destination_id)
                .ok_or("missing destination window")?;
            let source_index = source
                .tabs
                .iter()
                .position(|candidate| *candidate == tab_id)
                .ok_or("source does not contain tab")?;
            if destination.tabs.contains(&tab_id) {
                return Err("destination already contains tab");
            }
            if destination.tabs.len() >= CONTRACT_MAX_TABS_PER_WINDOW {
                return Err("destination window is at the tab bound");
            }
            if destination_index > destination.tabs.len() {
                return Err("destination index is out of bounds");
            }
            let topology_revision = self
                .topology_revision
                .checked_add(1)
                .ok_or("topology revision exhausted")?;
            let source_revision = source
                .revision
                .checked_add(1)
                .ok_or("source revision exhausted")?;
            let destination_revision = destination
                .revision
                .checked_add(1)
                .ok_or("destination revision exhausted")?;

            let mut source = (*source).clone();
            let mut destination = (*destination).clone();
            let moving_active = source.active == Some(tab_id);
            source.tabs.remove(source_index);
            if moving_active {
                source.active = source
                    .tabs
                    .get(source_index)
                    .or_else(|| source.tabs.last())
                    .copied();
            }
            destination.tabs.insert(destination_index, tab_id);
            destination.active.get_or_insert(tab_id);
            source.revision = source_revision;
            destination.revision = destination_revision;

            self.windows.insert(source_id, source);
            self.windows.insert(destination_id, destination);
            self.topology_revision = topology_revision;
            Ok(())
        }
    }

    fn validate_contract_snapshot(windows: &[(u64, ContractWindow)]) -> Result<(), &'static str> {
        if windows.len() > CONTRACT_MAX_WINDOWS {
            return Err("snapshot exceeds window bound");
        }
        let mut window_ids = HashSet::with_capacity(windows.len());
        let mut tab_ids = HashSet::new();
        let mut total_tabs = 0usize;
        for (window_id, window) in windows {
            if !window_ids.insert(*window_id) {
                return Err("snapshot contains a duplicate window");
            }
            if window.tabs.len() > CONTRACT_MAX_TABS_PER_WINDOW {
                return Err("snapshot window exceeds tab bound");
            }
            total_tabs = total_tabs
                .checked_add(window.tabs.len())
                .ok_or("snapshot total tab count overflowed")?;
            if total_tabs > CONTRACT_MAX_TOTAL_TABS {
                return Err("snapshot exceeds total tab bound");
            }
            for tab_id in &window.tabs {
                if !tab_ids.insert(*tab_id) {
                    return Err("snapshot contains a duplicate tab");
                }
            }
            match window.active {
                Some(active) if window.tabs.contains(&active) => {}
                Some(_) => return Err("snapshot active tab is not a member"),
                None if window.tabs.is_empty() => {}
                None => return Err("non-empty snapshot window has no active tab"),
            }
        }
        Ok(())
    }

    fn rebase_conflicted_reorder_once(
        base: &[u64],
        desired: &[u64],
        desired_active: Option<u64>,
        current: &[u64],
        current_active: Option<u64>,
        prior_rebase_attempts: usize,
    ) -> Option<(Vec<u64>, Option<u64>)> {
        if prior_rebase_attempts != 0
            || base.len() > CONTRACT_MAX_TABS_PER_WINDOW
            || desired.len() > CONTRACT_MAX_TABS_PER_WINDOW
            || current.len() > CONTRACT_MAX_TABS_PER_WINDOW
        {
            return None;
        }
        let base_set = base.iter().copied().collect::<HashSet<_>>();
        let desired_set = desired.iter().copied().collect::<HashSet<_>>();
        let current_set = current.iter().copied().collect::<HashSet<_>>();
        if base_set.len() != base.len()
            || desired_set.len() != desired.len()
            || current_set.len() != current.len()
            || desired_set != base_set
            || desired_active.is_some_and(|active| !desired_set.contains(&active))
            || (desired_active.is_none() && !desired.is_empty())
            || current_active.is_some_and(|active| !current_set.contains(&active))
            || (current_active.is_none() && !current.is_empty())
        {
            return None;
        }

        let surviving_base = base
            .iter()
            .copied()
            .filter(|tab| current_set.contains(tab))
            .collect::<Vec<_>>();
        let surviving_current = current
            .iter()
            .copied()
            .filter(|tab| base_set.contains(tab))
            .collect::<Vec<_>>();
        if surviving_base != surviving_current {
            return None;
        }

        let mut rebased = desired
            .iter()
            .copied()
            .filter(|tab| current_set.contains(tab))
            .collect::<Vec<_>>();
        let mut seen = rebased.iter().copied().collect::<HashSet<_>>();
        for tab in current {
            if seen.insert(*tab) {
                rebased.push(*tab);
            }
        }
        let active = desired_active
            .filter(|active| current_set.contains(active))
            .or(current_active)
            .or_else(|| rebased.first().copied());
        Some((rebased, active))
    }

    #[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
    struct StableRemoteSlot {
        binding: u128,
        session: u128,
        window: u64,
        tab: u64,
    }

    #[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
    enum StableSlot {
        Remote(StableRemoteSlot),
        Local { session: u128, tab: u128 },
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum OrderAuthority {
        RemoteServer,
        ClientOverlay,
    }

    fn classify_order_authority(
        projected: &[StableSlot],
        complete_remote_window: &[StableRemoteSlot],
    ) -> OrderAuthority {
        if projected.len() != complete_remote_window.len() || projected.is_empty() {
            return OrderAuthority::ClientOverlay;
        }
        let Some(StableSlot::Remote(first)) = projected.first() else {
            return OrderAuthority::ClientOverlay;
        };
        let same_remote_window = projected.iter().all(|slot| {
            matches!(
                slot,
                StableSlot::Remote(remote)
                    if remote.binding == first.binding
                        && remote.session == first.session
                        && remote.window == first.window
            )
        });
        let projected = projected
            .iter()
            .filter_map(|slot| match slot {
                StableSlot::Remote(remote) => Some(*remote),
                StableSlot::Local { .. } => None,
            })
            .collect::<HashSet<_>>();
        let authoritative = complete_remote_window
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        if same_remote_window
            && projected.len() == complete_remote_window.len()
            && projected == authoritative
        {
            OrderAuthority::RemoteServer
        } else {
            OrderAuthority::ClientOverlay
        }
    }

    fn reconcile_mixed_overlay(
        persisted: &[StableSlot],
        persisted_active: Option<StableSlot>,
        live: &[StableSlot],
    ) -> Result<(Vec<StableSlot>, Option<StableSlot>), &'static str> {
        if persisted.len() > CONTRACT_MAX_TABS_PER_WINDOW {
            return Err("persisted overlay exceeds tab bound");
        }
        if live.len() > CONTRACT_MAX_TABS_PER_WINDOW {
            return Err("live overlay exceeds tab bound");
        }
        let persisted_set = persisted.iter().copied().collect::<HashSet<_>>();
        if persisted_set.len() != persisted.len() {
            return Err("persisted overlay contains duplicate slots");
        }
        if persisted_active.is_some_and(|active| !persisted_set.contains(&active)) {
            return Err("persisted active slot is not a member");
        }
        if persisted_active.is_none() && !persisted.is_empty() {
            return Err("non-empty persisted overlay has no active slot");
        }
        let live_set = live.iter().copied().collect::<HashSet<_>>();
        if live_set.len() != live.len() {
            return Err("live overlay contains duplicate slots");
        }

        let mut reconciled = persisted
            .iter()
            .copied()
            .filter(|slot| live_set.contains(slot))
            .collect::<Vec<_>>();
        let mut seen = reconciled.iter().copied().collect::<HashSet<_>>();
        for slot in live {
            if seen.insert(*slot) {
                reconciled.push(*slot);
            }
        }

        let active = match persisted_active {
            Some(active) if live_set.contains(&active) => Some(active),
            Some(active) => {
                let active_index = persisted
                    .iter()
                    .position(|candidate| *candidate == active)
                    .expect("persisted active membership was validated");
                persisted[active_index + 1..]
                    .iter()
                    .find(|candidate| live_set.contains(candidate))
                    .or_else(|| {
                        persisted[..active_index]
                            .iter()
                            .rev()
                            .find(|candidate| live_set.contains(candidate))
                    })
                    .copied()
                    .or_else(|| reconciled.first().copied())
            }
            None => reconciled.first().copied(),
        };

        Ok((reconciled, active))
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum OrderSupport {
        Durable,
        LegacyBestEffort,
    }

    fn negotiated_order_support(capability_present: bool) -> OrderSupport {
        if capability_present {
            OrderSupport::Durable
        } else {
            OrderSupport::LegacyBestEffort
        }
    }

    fn test_tab() -> Arc<Tab> {
        Arc::new(Tab::new(&TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 640,
            pixel_height: 480,
            dpi: 96,
        }))
    }

    #[test]
    fn contract_model_initial_attach_and_reorder_preserve_active_identity() {
        let mut model = TabOrderContractModel::new(
            0xaaaa,
            11,
            [(
                7,
                ContractWindow {
                    revision: 5,
                    tabs: vec![10, 20, 30],
                    active: Some(20),
                },
            )],
        );

        let reply = model.apply_reorder(ReorderIntent {
            session: 0xaaaa,
            mutation: 1,
            window: 7,
            expected_revision: 5,
            tabs: vec![30, 20, 10],
            active: Some(20),
        });

        assert_eq!(
            reply,
            ContractReply::Decision(ContractDecision::Applied {
                topology_revision: 12,
                window_revision: 6,
            })
        );
        assert_eq!(model.windows[&7].tabs, vec![30, 20, 10]);
        assert_eq!(model.windows[&7].active, Some(20));
    }

    #[test]
    fn contract_model_concurrent_reorder_conflicts_and_reconnect_replays() {
        let mut model = TabOrderContractModel::new(
            0xbbbb,
            0,
            [(
                8,
                ContractWindow {
                    revision: 0,
                    tabs: vec![1, 2, 3],
                    active: Some(1),
                },
            )],
        );
        let winner = ReorderIntent {
            session: 0xbbbb,
            mutation: 41,
            window: 8,
            expected_revision: 0,
            tabs: vec![2, 1, 3],
            active: Some(1),
        };
        let loser = ReorderIntent {
            session: 0xbbbb,
            mutation: 42,
            window: 8,
            expected_revision: 0,
            tabs: vec![3, 2, 1],
            active: Some(1),
        };

        assert!(matches!(
            model.apply_reorder(winner.clone()),
            ContractReply::Decision(ContractDecision::Applied { .. })
        ));
        assert_eq!(
            model.apply_reorder(loser),
            ContractReply::Decision(ContractDecision::Conflict { window_revision: 1 })
        );
        let first_receipt = ContractDecision::Applied {
            topology_revision: 1,
            window_revision: 1,
        };
        assert_eq!(
            model.apply_reorder(winner.clone()),
            ContractReply::Replay(first_receipt)
        );

        let mut reused_mutation = winner;
        reused_mutation.tabs = vec![1, 2, 3];
        assert_eq!(
            model.apply_reorder(reused_mutation),
            ContractReply::Decision(ContractDecision::Malformed),
            "one MutationId cannot authorize two payloads"
        );
        assert_eq!(model.windows[&8].tabs, vec![2, 1, 3]);
    }

    #[test]
    fn contract_model_expired_receipt_rechecks_revision_instead_of_reapplying() {
        let mut model = TabOrderContractModel::new(
            0xbbbc,
            0,
            [(
                8,
                ContractWindow {
                    revision: 0,
                    tabs: vec![1],
                    active: Some(1),
                },
            )],
        );
        let receipt_limit =
            u64::try_from(CONTRACT_MAX_SERVER_RECEIPTS).expect("receipt bound fits u64");
        let first = ReorderIntent {
            session: 0xbbbc,
            mutation: 0,
            window: 8,
            expected_revision: 0,
            tabs: vec![1],
            active: Some(1),
        };

        for mutation in 0_u64..=receipt_limit {
            assert!(matches!(
                model.apply_reorder(ReorderIntent {
                    session: 0xbbbc,
                    mutation: u128::from(mutation),
                    window: 8,
                    expected_revision: mutation,
                    tabs: vec![1],
                    active: Some(1),
                }),
                ContractReply::Decision(ContractDecision::Applied { .. })
            ));
        }

        assert_eq!(model.receipts.len(), CONTRACT_MAX_SERVER_RECEIPTS);
        assert_eq!(
            model.apply_reorder(first),
            ContractReply::Decision(ContractDecision::Conflict {
                window_revision: receipt_limit + 1,
            }),
            "an expired successful receipt must fall through normal CAS validation"
        );
    }

    #[test]
    fn contract_model_rebases_membership_only_conflicts_at_most_once() {
        let base = [1, 2];
        let desired = [2, 1];

        assert_eq!(
            rebase_conflicted_reorder_once(&base, &desired, Some(2), &[1, 2, 3], Some(1), 0),
            Some((vec![2, 1, 3], Some(2))),
            "a server-only append preserves the user's reorder and appends the unseen tab"
        );
        assert_eq!(
            rebase_conflicted_reorder_once(&base, &desired, Some(2), &[1], Some(1), 0),
            Some((vec![1], Some(1))),
            "a removed desired active tab adopts the current server active identity"
        );
        assert_eq!(
            rebase_conflicted_reorder_once(&base, &desired, Some(2), &[2, 1, 3], Some(1), 0),
            None,
            "a concurrent reorder changes common-tab order and cannot be overwritten"
        );
        assert_eq!(
            rebase_conflicted_reorder_once(&base, &desired, Some(2), &[1, 2, 3], Some(1), 1),
            None,
            "automatic rebasing has a one-attempt budget"
        );
    }

    #[test]
    fn contract_model_stale_client_and_server_restart_fail_closed() {
        let old_intent = ReorderIntent {
            session: 0x1111,
            mutation: 50,
            window: 9,
            expected_revision: 0,
            tabs: vec![2, 1],
            active: Some(1),
        };
        let mut old_server = TabOrderContractModel::new(
            0x1111,
            0,
            [(
                9,
                ContractWindow {
                    revision: 0,
                    tabs: vec![1, 2],
                    active: Some(1),
                },
            )],
        );
        let mut restarted_server = TabOrderContractModel::new(
            0x2222,
            0,
            [(
                9,
                ContractWindow {
                    revision: 0,
                    tabs: vec![1, 2],
                    active: Some(1),
                },
            )],
        );

        assert!(matches!(
            old_server.apply_reorder(old_intent.clone()),
            ContractReply::Decision(ContractDecision::Applied { .. })
        ));
        assert_eq!(
            restarted_server.apply_reorder(old_intent),
            ContractReply::Decision(ContractDecision::StaleIncarnation)
        );
        assert_eq!(restarted_server.windows[&9].tabs, vec![1, 2]);
    }

    #[test]
    fn contract_model_new_close_and_id_reuse_follow_server_policy() {
        let mut model = TabOrderContractModel::new(
            7,
            0,
            [(
                1,
                ContractWindow {
                    revision: 0,
                    tabs: vec![10, 20, 30],
                    active: Some(20),
                },
            )],
        );

        model.add_tab(1, 40).expect("new tabs append");
        assert_eq!(model.windows[&1].tabs, vec![10, 20, 30, 40]);
        model.close_tab(20).expect("close active tab");
        assert_eq!(model.windows[&1].tabs, vec![10, 30, 40]);
        assert_eq!(
            model.windows[&1].active,
            Some(30),
            "the right neighbor becomes active"
        );
        assert_eq!(
            model.add_tab(1, 20),
            Err("tab identity reused within one session incarnation")
        );
    }

    #[test]
    fn contract_model_cross_window_move_is_one_identity_preserving_transition() {
        let mut model = TabOrderContractModel::new(
            8,
            3,
            [
                (
                    1,
                    ContractWindow {
                        revision: 4,
                        tabs: vec![10, 20],
                        active: Some(20),
                    },
                ),
                (
                    2,
                    ContractWindow {
                        revision: 7,
                        tabs: vec![30],
                        active: Some(30),
                    },
                ),
            ],
        );

        assert_eq!(
            model.move_tab(20, 1, 2, 2),
            Err("destination index is out of bounds")
        );
        assert_eq!(model.topology_revision, 3);
        assert_eq!(model.windows[&1].tabs, vec![10, 20]);
        assert_eq!(model.windows[&2].tabs, vec![30]);

        model.move_tab(20, 1, 2, 0).expect("cross-window move");

        assert_eq!(model.topology_revision, 4);
        assert_eq!(model.windows[&1].tabs, vec![10]);
        assert_eq!(model.windows[&1].active, Some(10));
        assert_eq!(model.windows[&1].revision, 5);
        assert_eq!(model.windows[&2].tabs, vec![20, 30]);
        assert_eq!(model.windows[&2].active, Some(30));
        assert_eq!(model.windows[&2].revision, 8);
    }

    #[test]
    fn contract_model_rejects_malformed_permutations_and_revision_exhaustion() {
        let mut model = TabOrderContractModel::new(
            9,
            u64::MAX,
            [(
                3,
                ContractWindow {
                    revision: 4,
                    tabs: vec![1, 2],
                    active: Some(1),
                },
            )],
        );
        let malformed = ReorderIntent {
            session: 9,
            mutation: 1,
            window: 3,
            expected_revision: 4,
            tabs: vec![1, 1],
            active: Some(1),
        };
        let exhausted = ReorderIntent {
            session: 9,
            mutation: 2,
            window: 3,
            expected_revision: 4,
            tabs: vec![2, 1],
            active: Some(1),
        };

        assert_eq!(
            model.apply_reorder(malformed),
            ContractReply::Decision(ContractDecision::Malformed)
        );
        assert_eq!(
            model.apply_reorder(exhausted),
            ContractReply::Decision(ContractDecision::Exhausted)
        );
        let no_active = ReorderIntent {
            session: 9,
            mutation: 3,
            window: 3,
            expected_revision: 4,
            tabs: vec![2, 1],
            active: None,
        };
        assert_eq!(
            model.apply_reorder(no_active),
            ContractReply::Decision(ContractDecision::Malformed)
        );
        assert_eq!(model.windows[&3].tabs, vec![1, 2]);
    }

    #[test]
    fn contract_model_enforces_tab_bounds_before_mutation() {
        let full_tabs = (0_u64..4_096).collect::<Vec<_>>();
        let oversized_tabs = (0_u64..=4_096).collect::<Vec<_>>();
        let mut model = TabOrderContractModel::new(
            10,
            12,
            [
                (
                    1,
                    ContractWindow {
                        revision: 3,
                        tabs: vec![10_000],
                        active: Some(10_000),
                    },
                ),
                (
                    2,
                    ContractWindow {
                        revision: 4,
                        tabs: full_tabs.clone(),
                        active: Some(0),
                    },
                ),
            ],
        );

        assert_eq!(
            model.add_tab(2, 20_000),
            Err("destination window is at the tab bound")
        );
        assert_eq!(
            model.move_tab(10_000, 1, 2, 0),
            Err("destination window is at the tab bound")
        );
        assert_eq!(
            model.apply_reorder(ReorderIntent {
                session: 10,
                mutation: 1,
                window: 2,
                expected_revision: 4,
                tabs: oversized_tabs,
                active: Some(0),
            }),
            ContractReply::Decision(ContractDecision::Malformed)
        );
        assert_eq!(model.topology_revision, 12);
        assert_eq!(model.windows[&1].tabs, vec![10_000]);
        assert_eq!(model.windows[&2].tabs, full_tabs);
        assert_eq!(model.windows[&2].revision, 4);
    }

    #[test]
    fn contract_model_rejects_invalid_and_oversized_snapshots_before_projection() {
        let duplicate_tab = [
            (
                1,
                ContractWindow {
                    revision: 0,
                    tabs: vec![10],
                    active: Some(10),
                },
            ),
            (
                2,
                ContractWindow {
                    revision: 0,
                    tabs: vec![10],
                    active: Some(10),
                },
            ),
        ];
        assert_eq!(
            validate_contract_snapshot(&duplicate_tab),
            Err("snapshot contains a duplicate tab")
        );

        let too_many_windows = (0..=CONTRACT_MAX_WINDOWS)
            .map(|window_id| {
                (
                    u64::try_from(window_id).expect("contract window bound fits u64"),
                    ContractWindow {
                        revision: 0,
                        tabs: Vec::new(),
                        active: None,
                    },
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            validate_contract_snapshot(&too_many_windows),
            Err("snapshot exceeds window bound")
        );

        let too_many_tabs = (0..5)
            .map(|window_id| {
                let first_tab = window_id * CONTRACT_MAX_TABS_PER_WINDOW;
                let tabs = (first_tab..first_tab + CONTRACT_MAX_TABS_PER_WINDOW)
                    .map(|tab_id| u64::try_from(tab_id).expect("contract tab bound fits u64"))
                    .collect::<Vec<_>>();
                (
                    u64::try_from(window_id).expect("contract window bound fits u64"),
                    ContractWindow {
                        revision: 0,
                        active: tabs.first().copied(),
                        tabs,
                    },
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            validate_contract_snapshot(&too_many_tabs),
            Err("snapshot exceeds total tab bound")
        );
    }

    #[test]
    fn contract_model_classifies_only_complete_single_remote_windows_as_pure() {
        let first = StableRemoteSlot {
            binding: 1,
            session: 2,
            window: 3,
            tab: 10,
        };
        let second = StableRemoteSlot { tab: 20, ..first };
        let foreign_window = StableRemoteSlot {
            window: 4,
            tab: 30,
            ..first
        };
        let complete = [first, second];

        assert_eq!(
            classify_order_authority(
                &[StableSlot::Remote(second), StableSlot::Remote(first)],
                &complete,
            ),
            OrderAuthority::RemoteServer,
            "local order may be optimistic, but exact membership has one remote authority"
        );
        assert_eq!(
            classify_order_authority(
                &[
                    StableSlot::Remote(first),
                    StableSlot::Local { session: 5, tab: 6 },
                ],
                &complete,
            ),
            OrderAuthority::ClientOverlay
        );
        assert_eq!(
            classify_order_authority(
                &[
                    StableSlot::Remote(first),
                    StableSlot::Remote(foreign_window),
                ],
                &complete,
            ),
            OrderAuthority::ClientOverlay
        );
    }

    #[test]
    fn contract_model_mixed_overlay_retains_relative_order_and_appends_unseen_tabs() {
        let first = StableSlot::Local {
            session: 1,
            tab: 10,
        };
        let missing_active = StableSlot::Local {
            session: 1,
            tab: 20,
        };
        let right_neighbor = StableSlot::Local {
            session: 1,
            tab: 30,
        };
        let unseen = StableSlot::Local {
            session: 1,
            tab: 40,
        };
        let persisted = [first, missing_active, right_neighbor];
        let live = [right_neighbor, first, unseen];

        let (order, active) = reconcile_mixed_overlay(&persisted, Some(missing_active), &live)
            .expect("valid overlay");

        assert_eq!(order, vec![first, right_neighbor, unseen]);
        assert_eq!(
            active,
            Some(right_neighbor),
            "missing active falls right before left or appended slots"
        );
    }

    #[test]
    fn contract_model_corrupt_overlay_and_legacy_peer_never_gain_authority() {
        let slot = StableSlot::Local { session: 1, tab: 2 };
        let live = vec![slot];
        let before = live.clone();

        assert_eq!(
            reconcile_mixed_overlay(&[slot, slot], Some(slot), &live),
            Err("persisted overlay contains duplicate slots")
        );
        assert_eq!(
            reconcile_mixed_overlay(&[slot], None, &live),
            Err("non-empty persisted overlay has no active slot")
        );
        let oversized_live = (0..=CONTRACT_MAX_TABS_PER_WINDOW)
            .map(|tab| StableSlot::Local {
                session: 1,
                tab: u128::try_from(tab).expect("contract tab bound fits u128"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            reconcile_mixed_overlay(&[], None, &oversized_live),
            Err("live overlay exceeds tab bound")
        );
        assert_eq!(live, before, "validation is side-effect free");
        assert_eq!(
            negotiated_order_support(false),
            OrderSupport::LegacyBestEffort
        );
        assert_eq!(negotiated_order_support(true), OrderSupport::Durable);
    }

    #[test]
    fn insert_preserves_exact_active_identity_and_rejects_invalid_index() {
        let first = test_tab();
        let active = test_tab();
        let third = test_tab();
        let before_active = test_tab();
        let at_active = test_tab();
        let after_active = test_tab();

        let mut window = Window::new(None, None);
        window.push(&first).expect("append first tab");
        window.push(&active).expect("append active tab");
        window.push(&third).expect("append third tab");
        window.set_active_without_saving(1);

        window
            .insert(0, &before_active)
            .expect("insert before active");
        assert_eq!(window.get_active_idx(), 2);
        assert!(window
            .get_active()
            .is_some_and(|tab| Arc::ptr_eq(tab, &active)));

        let active_index = window.get_active_idx();
        window
            .insert(active_index, &at_active)
            .expect("insert at the active numeric index");
        assert_eq!(window.get_active_idx(), active_index + 1);
        assert!(window
            .get_active()
            .is_some_and(|tab| Arc::ptr_eq(tab, &active)));

        window
            .insert(window.len(), &after_active)
            .expect("insert after active");
        assert!(window
            .get_active()
            .is_some_and(|tab| Arc::ptr_eq(tab, &active)));

        let prior_order = window.iter().map(Arc::as_ptr).collect::<Vec<_>>();
        let prior_active_index = window.get_active_idx();
        let prior_revision = window.order_revision();
        let duplicate_error = window
            .push(&active)
            .expect_err("duplicate append must fail without panicking");
        assert!(duplicate_error.to_string().contains("already attached"));
        assert_eq!(
            window.iter().map(Arc::as_ptr).collect::<Vec<_>>(),
            prior_order
        );
        assert_eq!(window.get_active_idx(), prior_active_index);
        assert_eq!(window.order_revision(), prior_revision);

        let rejected = test_tab();
        let error = window
            .insert(window.len() + 1, &rejected)
            .expect_err("out-of-range insertion must fail");
        assert!(error.to_string().contains("cannot insert tab at index"));
        assert_eq!(
            window.iter().map(Arc::as_ptr).collect::<Vec<_>>(),
            prior_order,
            "failed insertion must preserve exact order",
        );
        assert_eq!(window.get_active_idx(), prior_active_index);
        assert!(window
            .get_active()
            .is_some_and(|tab| Arc::ptr_eq(tab, &active)));

        let only = test_tab();
        let mut empty = Window::new(None, None);
        empty.insert(0, &only).expect("insert into empty window");
        assert!(empty
            .get_active()
            .is_some_and(|tab| Arc::ptr_eq(tab, &only)));
    }

    #[test]
    fn same_window_reorder_preserves_active_identity_and_tab_stack() {
        let first = test_tab();
        let active = test_tab();
        let third = test_tab();
        let absent = test_tab();
        let stack_id = TabStackId(41);

        let mut window = Window::new(None, None);
        window.push(&first).expect("append first tab");
        window.push(&active).expect("append active tab");
        window.push(&third).expect("append third tab");
        window.set_active_without_saving(1);
        window
            .create_tab_stack(
                stack_id,
                vec![first.tab_id(), active.tab_id(), third.tab_id()],
            )
            .expect("create stack before reorder");

        assert!(window
            .reorder_tab_if_same(&first, 0, 2)
            .expect("inactive reorder must validate"));
        assert_eq!(
            window.iter().map(|tab| tab.tab_id()).collect::<Vec<_>>(),
            vec![active.tab_id(), third.tab_id(), first.tab_id()],
        );
        assert!(window
            .get_active()
            .is_some_and(|tab| Arc::ptr_eq(tab, &active)));

        assert!(window
            .reorder_tab_if_same(&active, 0, 2)
            .expect("active reorder must validate"));
        assert_eq!(
            window.iter().map(|tab| tab.tab_id()).collect::<Vec<_>>(),
            vec![third.tab_id(), first.tab_id(), active.tab_id()],
        );
        assert_eq!(window.get_active_idx(), 2);
        assert!(window
            .get_active()
            .is_some_and(|tab| Arc::ptr_eq(tab, &active)));
        for tab in [&first, &active, &third] {
            assert_eq!(
                window.tab_stack_for_tab(tab.tab_id()),
                Some(stack_id),
                "reorder must not detach tab {} from its stack",
                tab.tab_id(),
            );
        }

        let prior_order = window.iter().map(Arc::as_ptr).collect::<Vec<_>>();
        assert!(!window
            .reorder_tab_if_same(&absent, 1, 1)
            .expect("missing exact identity is not a malformed index"));
        assert_eq!(
            window.iter().map(Arc::as_ptr).collect::<Vec<_>>(),
            prior_order,
        );
        let invalid_destination = window.len();
        assert!(window
            .reorder_tab_if_same(&active, 2, invalid_destination)
            .expect_err("out-of-range reorder must fail")
            .to_string()
            .contains("cannot move tab to index"));
        assert_eq!(
            window.iter().map(Arc::as_ptr).collect::<Vec<_>>(),
            prior_order,
            "invalid reorder must preserve exact order",
        );
        assert!(window
            .get_active()
            .is_some_and(|tab| Arc::ptr_eq(tab, &active)));
    }

    #[test]
    fn tab_stack_cycle_updates_active_window_tab() {
        let first = test_tab();
        let second = test_tab();
        let third = test_tab();
        let first_id = first.tab_id();
        let second_id = second.tab_id();
        let third_id = third.tab_id();

        let mut window = Window::new(None, None);
        window.push(&first).expect("append first tab");
        window.push(&second).expect("append second tab");
        window.push(&third).expect("append third tab");

        window
            .create_tab_stack(TabStackId(1), vec![first_id, second_id, third_id])
            .expect("create window tab stack");

        assert_eq!(window.get_active().map(|tab| tab.tab_id()), Some(first_id));
        assert_eq!(window.cycle_tab_stack(TabStackId(1), 1), Some(second_id));
        assert_eq!(
            window.get_active().map(|tab| tab.tab_id()),
            Some(second_id),
            "cycling a tab stack should activate the newly visible tab"
        );
        assert_eq!(window.get_last_active_idx(), Some(0));
        assert_eq!(window.cycle_tab_stack(TabStackId(1), -1), Some(first_id));
        assert_eq!(window.get_active().map(|tab| tab.tab_id()), Some(first_id));
    }

    #[test]
    fn removing_tab_prunes_tab_stack_membership() {
        let first = test_tab();
        let second = test_tab();
        let third = test_tab();
        let first_id = first.tab_id();
        let second_id = second.tab_id();
        let third_id = third.tab_id();

        let mut window = Window::new(None, None);
        window.push(&first).expect("append first tab");
        window.push(&second).expect("append second tab");
        window.push(&third).expect("append third tab");

        window
            .create_tab_stack(TabStackId(7), vec![first_id, second_id, third_id])
            .expect("create window tab stack");
        assert_eq!(window.tab_stack_count(), 1);

        window.remove_by_id(second_id);

        assert_eq!(window.tab_stack_for_tab(second_id), None);
        assert_eq!(
            window
                .tab_stack_entries()
                .into_iter()
                .map(|entry| entry.tab_id)
                .collect::<Vec<_>>(),
            vec![first_id, third_id]
        );

        window.remove_by_id(first_id);
        window.remove_by_id(third_id);
        assert_eq!(
            window.tab_stack_count(),
            0,
            "stack should disappear after its final tab is removed"
        );
    }

    #[test]
    fn removing_tab_clears_stale_last_active_reference() {
        let first = test_tab();
        let second = test_tab();
        let third = test_tab();
        let second_id = second.tab_id();

        let mut window = Window::new(None, None);
        window.push(&first).expect("append first tab");
        window.push(&second).expect("append second tab");
        window.push(&third).expect("append third tab");
        window.save_and_then_set_active(1);
        window.save_and_then_set_active(2);
        assert_eq!(window.last_active, Some(second_id));

        window.remove_by_id(second_id);

        assert_eq!(
            window.last_active, None,
            "remove_by_id must not retain a removed tab as last_active session state",
        );
        assert_eq!(
            window.get_active().map(|tab| tab.tab_id()),
            Some(third.tab_id()),
            "removing stale last_active must not disturb the current active tab",
        );
    }

    #[test]
    fn tab_stack_creation_rejects_tabs_outside_window() {
        let first = test_tab();
        let missing = test_tab();
        let first_id = first.tab_id();
        let missing_id = missing.tab_id();

        let mut window = Window::new(None, None);
        window.push(&first).expect("append first tab");

        assert_eq!(
            window.create_tab_stack(TabStackId(9), vec![first_id, missing_id]),
            Err(TabStackError::MissingTab(missing_id))
        );
        assert!(window.tab_stack_entries().is_empty());
    }

    #[test]
    fn frozen_order_and_prepared_commit_preserve_exact_identity_and_stack_state() {
        let first = test_tab();
        let active = test_tab();
        let third = test_tab();
        let stack_id = TabStackId(101);
        let mut window = Window::new(None, None);
        for tab in [&first, &active, &third] {
            window.push(tab).expect("append exact tab");
        }
        window.set_active_without_saving(1);
        window
            .create_tab_stack(
                stack_id,
                vec![first.tab_id(), active.tab_id(), third.tab_id()],
            )
            .expect("create stack before exact reorder");
        let stack_before = window.tab_stack_entries();
        let revision_before = window.order_revision();

        let before = window.order_snapshot().expect("valid frozen order");
        assert_eq!(before.order_revision(), revision_before);
        assert_eq!(
            before
                .ordered_tabs()
                .iter()
                .map(Arc::as_ptr)
                .collect::<Vec<_>>(),
            vec![
                Arc::as_ptr(&first),
                Arc::as_ptr(&active),
                Arc::as_ptr(&third),
            ]
        );
        assert!(before
            .active_tab()
            .is_some_and(|tab| Arc::ptr_eq(tab, &active)));

        let desired = [third.tab_id(), first.tab_id(), active.tab_id()];
        let validated = window
            .validate_exact_order(&desired, Some(active.tab_id()))
            .expect("validate exact permutation");
        let prepared = window
            .prepare_validated_order(validated)
            .expect("reserve exact permutation revision");
        let committed = window.commit_prepared_order(prepared);
        assert_eq!(committed.order_revision().get(), revision_before.get() + 1);
        assert_eq!(
            committed
                .ordered_tabs()
                .iter()
                .map(Arc::as_ptr)
                .collect::<Vec<_>>(),
            vec![
                Arc::as_ptr(&third),
                Arc::as_ptr(&first),
                Arc::as_ptr(&active),
            ]
        );
        assert!(committed
            .active_tab()
            .is_some_and(|tab| Arc::ptr_eq(tab, &active)));
        assert_eq!(window.tab_stack_entries(), stack_before);
    }

    #[test]
    fn production_exact_reorder_matches_contract_model() {
        let first = test_tab();
        let active = test_tab();
        let third = test_tab();
        let mut window = Window::new(None, None);
        for tab in [&first, &active, &third] {
            window.push(tab).expect("append exact tab");
        }
        window.set_active_without_saving(1);
        let revision_before = window.order_revision();
        let desired = vec![third.tab_id(), active.tab_id(), first.tab_id()];
        let mut model = TabOrderContractModel::new(
            0x5151,
            17,
            [(
                window.window_id() as u64,
                ContractWindow {
                    revision: revision_before.get(),
                    tabs: window.iter().map(|tab| tab.tab_id() as u64).collect(),
                    active: Some(active.tab_id() as u64),
                },
            )],
        );
        let model_reply = model.apply_reorder(ReorderIntent {
            session: 0x5151,
            mutation: 0x6161,
            window: window.window_id() as u64,
            expected_revision: revision_before.get(),
            tabs: desired.iter().map(|tab_id| *tab_id as u64).collect(),
            active: Some(active.tab_id() as u64),
        });

        let validated = window
            .validate_exact_order(&desired, Some(active.tab_id()))
            .expect("production reorder validates");
        let prepared = window
            .prepare_validated_order(validated)
            .expect("production reorder reserves revision");
        let frozen = window.commit_prepared_order(prepared);
        let ContractReply::Decision(ContractDecision::Applied {
            window_revision, ..
        }) = model_reply
        else {
            panic!("contract model must apply the same exact permutation");
        };
        assert_eq!(frozen.order_revision().get(), window_revision);
        assert_eq!(
            frozen
                .ordered_tab_ids()
                .map(|tab_id| tab_id as u64)
                .collect::<Vec<_>>(),
            model.windows[&(window.window_id() as u64)].tabs
        );
        assert_eq!(
            frozen.active_tab_id().map(|tab_id| tab_id as u64),
            model.windows[&(window.window_id() as u64)].active
        );
    }

    #[test]
    fn exhausted_window_revision_rejects_before_any_order_mutation() {
        let first = test_tab();
        let active = test_tab();
        let added = test_tab();
        let mut window = Window::new(None, None);
        window.push(&first).expect("append first tab");
        window.push(&active).expect("append active tab");
        window.set_active_without_saving(1);
        window.set_order_revision_for_test(WindowOrderRevision::new(u64::MAX - 1));
        let pointers_before = window.iter().map(Arc::as_ptr).collect::<Vec<_>>();
        let active_before = window.get_active().map(Arc::as_ptr);

        let insert_error = window
            .insert(window.len(), &added)
            .expect_err("terminal order revision must reject membership change");
        assert!(insert_error
            .to_string()
            .contains("revision space is exhausted"));
        let push_error = window
            .push(&added)
            .expect_err("fallible append must reject terminal order revision");
        assert!(push_error
            .to_string()
            .contains("revision space is exhausted"));
        let validated = window
            .validate_exact_order(&[active.tab_id(), first.tab_id()], Some(active.tab_id()))
            .expect("terminal order revision does not outrank semantic validity");
        let prepare_error = window
            .prepare_validated_order(validated)
            .expect_err("terminal order revision must reject exact reorder");
        assert_eq!(prepare_error, WindowOrderRevisionExhausted);
        assert_eq!(
            window.iter().map(Arc::as_ptr).collect::<Vec<_>>(),
            pointers_before
        );
        assert_eq!(window.get_active().map(Arc::as_ptr), active_before);
        assert_eq!(window.order_revision().get(), u64::MAX - 1);
    }

    #[test]
    fn every_legacy_order_state_mutator_advances_exactly_once() {
        let first = test_tab();
        let second = test_tab();
        let third = test_tab();
        let mut window = Window::new(None, None);

        window.push(&first).expect("append first tab");
        assert_eq!(window.order_revision().get(), 1);
        window.insert(1, &second).expect("insert second tab");
        assert_eq!(window.order_revision().get(), 2);
        window.push(&third).expect("append third tab");
        assert_eq!(window.order_revision().get(), 3);

        window
            .reorder_tab_if_same(&first, 0, 2)
            .expect("valid legacy exact reorder");
        assert_eq!(window.order_revision().get(), 4);
        window
            .reorder_tab_if_same(&first, 2, 2)
            .expect("same-index reorder is a no-op");
        assert_eq!(window.order_revision().get(), 4);

        window.set_active_without_saving(0);
        assert_eq!(window.order_revision().get(), 5);
        window.set_active_without_saving(0);
        assert_eq!(window.order_revision().get(), 5);

        window.remove_by_id(third.tab_id());
        assert_eq!(window.order_revision().get(), 6);
        window.remove_by_id(third.tab_id());
        assert_eq!(window.order_revision().get(), 6);

        let removals = std::iter::once(Arc::as_ptr(&first) as usize).collect::<HashSet<_>>();
        assert!(window.remove_tabs_by_exact_identity_set(&removals));
        assert_eq!(window.order_revision().get(), 7);
        assert!(!window.remove_tabs_by_exact_identity_set(&removals));
        assert_eq!(window.order_revision().get(), 7);
        assert!(window
            .get_active()
            .is_some_and(|tab| Arc::ptr_eq(tab, &second)));
    }
}
