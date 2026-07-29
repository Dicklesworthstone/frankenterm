use crate::pane::CloseReason;
use crate::tab::{TabStackEntry, TabStackError, TabStackId, TabStackState};
use crate::{Mux, MuxNotification, Pane, Tab, TabId, DEFAULT_WORKSPACE};
use config::GuiPosition;
use std::collections::HashSet;
use std::sync::Arc;

static WIN_ID: ::std::sync::atomic::AtomicUsize = ::std::sync::atomic::AtomicUsize::new(0);
pub type WindowId = usize;

pub struct Window {
    id: WindowId,
    owner: std::sync::Weak<Mux>,
    tabs: Vec<Arc<Tab>>,
    active: usize,
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
            id: crate::next_saturating_usize_id(&WIN_ID),
            owner,
            tabs: vec![],
            active: 0,
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
        self.notify(MuxNotification::WindowWorkspaceChanged(self.id));
    }

    pub fn window_id(&self) -> WindowId {
        self.id
    }

    fn check_that_tab_isnt_already_in_window(&self, tab: &Arc<Tab>) {
        for t in &self.tabs {
            assert_ne!(t.tab_id(), tab.tab_id(), "tab already added to this window");
        }
    }

    fn invalidate(&mut self) {
        self.notify(MuxNotification::WindowInvalidated(self.id));
    }

    pub fn insert(&mut self, index: usize, tab: &Arc<Tab>) {
        self.check_that_tab_isnt_already_in_window(tab);
        self.tabs.insert(index, Arc::clone(tab));
        self.invalidate();
    }

    pub fn push(&mut self, tab: &Arc<Tab>) {
        self.check_that_tab_isnt_already_in_window(tab);
        self.tabs.push(Arc::clone(tab));
        self.invalidate();
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
        let mut removed_ids = HashSet::new();
        self.tabs.retain(|tab| {
            let remove = removals.contains(&(Arc::as_ptr(tab) as usize));
            if remove {
                removed_ids.insert(tab.tab_id());
            }
            !remove
        });
        if removed_ids.is_empty() {
            return false;
        }

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
        self.invalidate();
        true
    }

    fn do_remove_idx(&mut self, idx: usize, active: Option<Arc<Tab>>) -> Arc<Tab> {
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
        self.invalidate();
        tab
    }

    fn enqueue_focus_lost(&self, pane: Arc<dyn Pane>) {
        if let Some(mux) = self.owner.upgrade() {
            mux.enqueue_window_focus_lost(pane);
        } else if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pane.focus_changed(false);
        }))
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
        if idx == self.get_active_idx() {
            return;
        }
        self.save_last_active();
        self.set_active_without_saving(idx);
    }

    /// Make `idx` the active tab position.
    /// The saved tab id is not changed.
    pub fn set_active_without_saving(&mut self, idx: usize) {
        assert!(idx < self.tabs.len());
        if self.active != idx {
            if let Some(tab) = self.tabs.get(self.active).map(Arc::clone) {
                if let Some(pane) = tab.get_active_pane_callback_free() {
                    self.enqueue_focus_lost(pane);
                }
            }
        }
        self.active = idx;
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
    fn tab_stack_cycle_updates_active_window_tab() {
        let first = test_tab();
        let second = test_tab();
        let third = test_tab();
        let first_id = first.tab_id();
        let second_id = second.tab_id();
        let third_id = third.tab_id();

        let mut window = Window::new(None, None);
        window.push(&first);
        window.push(&second);
        window.push(&third);

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
        window.push(&first);
        window.push(&second);
        window.push(&third);

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
        window.push(&first);
        window.push(&second);
        window.push(&third);
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
        window.push(&first);

        assert_eq!(
            window.create_tab_stack(TabStackId(9), vec![first_id, missing_id]),
            Err(TabStackError::MissingTab(missing_id))
        );
        assert!(window.tab_stack_entries().is_empty());
    }
}
