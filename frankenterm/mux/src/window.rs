use crate::pane::CloseReason;
use crate::tab::{TabStackEntry, TabStackError, TabStackId, TabStackState};
use crate::{Mux, MuxNotification, Tab, TabId, DEFAULT_WORKSPACE};
use config::GuiPosition;
use std::sync::Arc;

static WIN_ID: ::std::sync::atomic::AtomicUsize = ::std::sync::atomic::AtomicUsize::new(0);
pub type WindowId = usize;

fn notify_window(notification: MuxNotification) {
    if promise::spawn::is_scheduler_configured() {
        promise::spawn::spawn_into_main_thread(async move {
            if let Some(mux) = Mux::try_get() {
                mux.notify(notification);
            }
        })
        .detach();
    } else if let Some(mux) = Mux::try_get() {
        mux.notify(notification);
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

pub struct Window {
    id: WindowId,
    tabs: Vec<Arc<Tab>>,
    active: usize,
    last_active: Option<TabId>,
    tab_stacks: TabStackState,
    workspace: String,
    title: String,
    initial_position: Option<GuiPosition>,
}

impl Window {
    pub fn new(workspace: Option<String>, initial_position: Option<GuiPosition>) -> Self {
        Self {
            id: WIN_ID.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed),
            tabs: vec![],
            active: 0,
            last_active: None,
            tab_stacks: TabStackState::default(),
            title: String::new(),
            workspace: workspace.unwrap_or_else(|| {
                Mux::try_get()
                    .map(|mux| mux.active_workspace())
                    .unwrap_or_else(|| DEFAULT_WORKSPACE.to_string())
            }),
            initial_position,
        }
    }

    pub fn get_initial_position(&self) -> &Option<GuiPosition> {
        &self.initial_position
    }

    pub fn get_workspace(&self) -> &str {
        &self.workspace
    }

    pub fn set_title(&mut self, title: &str) {
        if self.title != title {
            self.title = title.to_string();
            notify_window(MuxNotification::WindowTitleChanged {
                window_id: self.id,
                title: title.to_string(),
            });
        }
    }

    pub fn get_title(&self) -> &str {
        &self.title
    }

    pub fn set_workspace(&mut self, workspace: &str) {
        if workspace == self.workspace {
            return;
        }
        self.workspace = workspace.to_string();
        notify_window(MuxNotification::WindowWorkspaceChanged(self.id));
    }

    pub fn window_id(&self) -> WindowId {
        self.id
    }

    fn check_that_tab_isnt_already_in_window(&self, tab: &Arc<Tab>) {
        for t in &self.tabs {
            assert_ne!(t.tab_id(), tab.tab_id(), "tab already added to this window");
        }
    }

    fn invalidate(&self) {
        notify_window(MuxNotification::WindowInvalidated(self.id));
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

    fn fixup_active_tab_after_removal(&mut self, active: Option<Arc<Tab>>) {
        let len = self.tabs.len();
        if let Some(active) = active {
            for (idx, tab) in self.tabs.iter().enumerate() {
                if tab.tab_id() == active.tab_id() {
                    self.set_active_without_saving(idx);
                    return;
                }
            }
        }

        if len > 0 && self.active >= len {
            self.set_active_without_saving(len - 1);
        } else {
            self.invalidate();
        }
    }

    pub fn remove_by_idx(&mut self, idx: usize) -> Arc<Tab> {
        self.invalidate();
        let active = self.get_active().map(Arc::clone);
        self.do_remove_idx(idx, active)
    }

    pub fn remove_by_id(&mut self, id: TabId) {
        let active = self.get_active().map(Arc::clone);
        if let Some(idx) = self.idx_by_id(id) {
            self.do_remove_idx(idx, active);
        }
    }

    fn do_remove_idx(&mut self, idx: usize, active: Option<Arc<Tab>>) -> Arc<Tab> {
        if let (Some(active), Some(removing)) = (&active, self.tabs.get(idx)) {
            if active.tab_id() == removing.tab_id()
                && config::configuration().switch_to_last_active_tab_when_closing_tab
            {
                // If we are removing the active tab, switch back to
                // the previously active tab
                if let Some(last_active) = self.get_last_active_idx() {
                    self.set_active_without_saving(last_active);
                }
            }
        }
        let tab = self.tabs.remove(idx);
        self.tab_stacks.remove_tab(tab.tab_id());
        self.fixup_active_tab_after_removal(active);
        tab
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
            if let Some(tab) = self.tabs.get(self.active) {
                if let Some(pane) = tab.get_active_pane() {
                    pane.focus_changed(false);
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

    pub fn prune_dead_tabs(&mut self, live_tab_ids: &[TabId]) {
        let mut invalidated = false;
        let dead: Vec<TabId> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                if tab.prune_dead_panes() {
                    invalidated = true;
                }
                if tab.is_dead() {
                    Some(tab.tab_id())
                } else {
                    None
                }
            })
            .collect();

        for tab_id in dead {
            log::trace!("Window::prune_dead_tabs: tab_id {} is dead", tab_id);
            self.remove_by_id(tab_id);
            invalidated = true;
        }

        let dead: Vec<TabId> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                if live_tab_ids
                    .iter()
                    .find(|&&id| id == tab.tab_id())
                    .is_none()
                {
                    Some(tab.tab_id())
                } else {
                    None
                }
            })
            .collect();
        for tab_id in dead {
            log::trace!("Window::prune_dead_tabs: (live) tab_id {} is dead", tab_id);
            self.remove_by_id(tab_id);
        }

        if invalidated {
            self.invalidate();
        }
    }
}
