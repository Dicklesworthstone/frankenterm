use crate::TermWindow;
use crate::scripting::guiwin::GuiWin;
use crate::spawn::SpawnWhere;
use crate::termwindow::TermWindowNotif;
use ::window::*;
use anyhow::{Context, Error};
use config::keyassignment::{KeyAssignment, SpawnCommand};
use config::{ConfigSubscription, NotificationHandling};
use frankenterm_core::osc_protocol_integration::{CursorShapeSlug, Osc22PerPaneCursorMap};
use frankenterm_toast_notification::*;
use mux::client::ClientId;
use mux::window::WindowId as MuxWindowId;
use mux::{Mux, MuxNotification};
use promise::{Future, Promise};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use wezterm_term::{Alert, ClipboardSelection};

pub struct GuiFrontEnd {
    connection: Rc<Connection>,
    switching_workspaces: RefCell<bool>,
    spawned_mux_window: RefCell<HashSet<MuxWindowId>>,
    known_windows: RefCell<BTreeMap<Window, MuxWindowId>>,
    osc22_cursor_shapes: RefCell<Osc22PerPaneCursorMap>,
    client_id: Arc<ClientId>,
    config_subscription: RefCell<Option<ConfigSubscription>>,
}

impl Drop for GuiFrontEnd {
    fn drop(&mut self) {
        ::window::shutdown();
    }
}

impl GuiFrontEnd {
    pub fn try_new() -> anyhow::Result<Rc<GuiFrontEnd>> {
        let connection = Connection::init()?;
        connection.set_event_handler(Self::app_event_handler);

        let mux = Mux::try_get().context("mux singleton is not available")?;
        let client_id = mux
            .active_identity()
            .context("active mux identity is not set")?;

        let front_end = Rc::new(GuiFrontEnd {
            connection,
            switching_workspaces: RefCell::new(false),
            spawned_mux_window: RefCell::new(HashSet::new()),
            known_windows: RefCell::new(BTreeMap::new()),
            osc22_cursor_shapes: RefCell::new(Osc22PerPaneCursorMap::new()),
            client_id: client_id.clone(),
            config_subscription: RefCell::new(None),
        });

        mux.subscribe(move |n| {
            match n {
                MuxNotification::WorkspaceRenamed {
                    old_workspace,
                    new_workspace,
                } => {
                    if let Some(mux) = Mux::try_get() {
                        let active = mux.active_workspace();
                        if active == old_workspace || active == new_workspace {
                            if let Some(switcher) = WorkspaceSwitcher::new(&new_workspace) {
                                promise::spawn::spawn_into_main_thread(async move {
                                    drop(switcher);
                                })
                                .detach();
                            }
                        }
                    }
                }
                MuxNotification::WindowWorkspaceChanged(_)
                | MuxNotification::ActiveWorkspaceChanged(_)
                | MuxNotification::WindowCreated(_)
                | MuxNotification::WindowRemoved(_) => {
                    promise::spawn::spawn_into_main_thread(async move {
                        if let Some(fe) = crate::frontend::try_front_end()
                            && !fe.is_switching_workspace()
                        {
                            fe.reconcile_workspace();
                        }
                    })
                    .detach();
                }
                MuxNotification::PaneFocused(pane_id) => {
                    promise::spawn::spawn_into_main_thread(async move {
                        if let Some(mux) = Mux::try_get()
                            && let Err(err) = mux.focus_pane_and_containing_tab(pane_id)
                        {
                            log::error!("error reconciling PaneFocused notification: {err:#}");
                        }
                        if let Some(fe) = crate::frontend::try_front_end() {
                            fe.apply_osc22_cursor_shape_for_pane(pane_id);
                        }
                    })
                    .detach();
                }
                MuxNotification::PaneRemoved(pane_id) => {
                    if let Some(fe) = crate::frontend::try_front_end() {
                        fe.osc22_cursor_shapes
                            .borrow_mut()
                            .forget(pane_id as u64);
                    }
                }
                MuxNotification::TabTitleChanged { .. } => {}
                MuxNotification::WindowTitleChanged { .. } => {}
                MuxNotification::TabResized(_) => {}
                MuxNotification::TabAddedToWindow { .. } => {}
                MuxNotification::WindowInvalidated(_) => {}
                MuxNotification::PaneOutput(_) => {}
                MuxNotification::PaneAdded(_) => {}
                MuxNotification::Alert {
                    pane_id,
                    alert:
                        Alert::ToastNotification {
                            title,
                            body,
                            focus,
                        },
                } => {
                    let Some(mux) = Mux::try_get() else {
                        return true;
                    };

                    if let Some((_domain, window_id, tab_id)) = mux.resolve_pane_id(pane_id) {
                        let config = config::configuration();

                        if let Some((_fdomain, f_window, f_tab, f_pane)) =
                            mux.resolve_focused_pane(&client_id)
                        {
                            let show = match config.notification_handling {
                                NotificationHandling::NeverShow => false,
                                NotificationHandling::AlwaysShow => true,
                                NotificationHandling::SuppressFromFocusedPane => f_pane != pane_id,
                                NotificationHandling::SuppressFromFocusedTab => f_tab != tab_id,
                                NotificationHandling::SuppressFromFocusedWindow => {
                                    f_window != window_id
                                }
                            };

                            if show {
                                let message = if title.is_none() { "" } else { &body };
                                let title = title.as_ref().unwrap_or(&body);
                                if let Some(action) = terminal_toast_action(focus, pane_id) {
                                    persistent_toast_notification_with_action(
                                        title, message, action,
                                    );
                                } else {
                                    persistent_toast_notification(title, message);
                                }
                            }
                        }
                    }
                }
                MuxNotification::Alert {
                    pane_id: _,
                    alert: Alert::Bell | Alert::Progress(_),
                } => {
                    // Handled via TermWindowNotif; NOP it here.
                }
                MuxNotification::Alert {
                    pane_id: _,
                    alert:
                        Alert::OutputSinceFocusLost
                        | Alert::PaletteChanged
                        | Alert::CurrentWorkingDirectoryChanged
                        | Alert::WindowTitleChanged(_)
                        | Alert::TabTitleChanged(_)
                        | Alert::IconTitleChanged(_)
                        | Alert::ImageAltText { .. }
                        | Alert::SetUserVar { .. }
                        // ft-fy4ty: SetProfileRequested is dispatched
                        // to a confirmation prompt at this layer; the
                        // GUI continuation bead (ft-tzusd) wires the
                        // actual modal. Until then we accept the
                        // alert silently to preserve the safer
                        // default.
                        | Alert::SetProfileRequested { .. }
                } => {}
                MuxNotification::Alert {
                    pane_id,
                    alert: Alert::MouseShapeRequested { shape },
                } => {
                    if let Some(fe) = crate::frontend::try_front_end() {
                        fe.record_osc22_cursor_shape(pane_id, &shape);
                    }
                }
                MuxNotification::Empty => {
                    if config::configuration().quit_when_all_windows_are_closed {
                        promise::spawn::spawn_into_main_thread(async move {
                            if mux::activity::Activity::count() == 0 {
                                log::trace!("Mux is now empty, terminate gui");
                                if let Some(conn) = Connection::get() {
                                    conn.terminate_message_loop();
                                }
                            }
                        })
                        .detach();
                    }
                }
                MuxNotification::SaveToDownloads { name, data } => {
                    if !config::configuration().allow_download_protocols {
                        log::error!(
                            "Ignoring download request for {:?}, \
                                 as allow_download_protocols=false",
                            name
                        );
                    } else if let Err(err) = crate::download::save_to_downloads(name, &*data) {
                        log::error!("save_to_downloads: {:#}", err);
                    }
                }
                MuxNotification::AssignClipboard {
                    pane_id,
                    selection,
                    clipboard,
                } => {
                    promise::spawn::spawn_into_main_thread(async move {
                        log::trace!(
                            "set clipboard in pane {} {:?} {:?}",
                            pane_id,
                            selection,
                            clipboard
                        );
                        let Some(fe) = crate::frontend::try_front_end() else {
                            return;
                        };
                        if let Some(window) = fe.known_windows.borrow().keys().next() {
                            window.set_clipboard(
                                match selection {
                                    ClipboardSelection::Clipboard => Clipboard::Clipboard,
                                    ClipboardSelection::PrimarySelection => {
                                        Clipboard::PrimarySelection
                                    }
                                },
                                clipboard.unwrap_or_else(String::new),
                            );
                        } else {
                            log::error!("Cannot assign clipboard as there are no windows");
                        };
                    })
                    .detach();
                }
            }
            true
        });
        // Re-evaluate the config so that folks that are using
        // `wezterm.gui.get_appearance()` can have that take effect
        // before any windows are created
        config::reload();

        // And build the initial menu bar.
        // TODO: arrange for this to happen on config reload.
        crate::commands::CommandDef::recreate_menubar(&config::configuration());

        Ok(front_end)
    }

    fn app_event_handler(event: ApplicationEvent) {
        log::trace!("Got app event {event:?}");
        match event {
            ApplicationEvent::OpenCommandScript(file_name) => {
                let quoted_file_name = match shlex::try_quote(&file_name) {
                    Ok(name) => name.to_owned().to_string(),
                    Err(_) => {
                        log::error!(
                            "OpenCommandScript: {file_name} has embedded NUL bytes and
                             cannot be launched via the shell"
                        );
                        return;
                    }
                };
                promise::spawn::spawn(async move {
                    use config::keyassignment::SpawnTabDomain;
                    use wezterm_term::TerminalSize;

                    // We send the script to execute to the shell on stdin, rather than ask the
                    // shell to execute it directly, so that we start the shell and read in the
                    // user's rc files before running the script.  Without this, wezterm on macOS
                    // is launched with a default and very anemic path, and that is frustrating for
                    // users.

                    let Some(mux) = Mux::try_get() else {
                        log::error!("OpenCommandScript: mux singleton is not available");
                        return;
                    };
                    let window_id = None;
                    let pane_id = None;
                    let cmd = None;
                    let cwd = None;
                    let workspace = mux.active_workspace();

                    match mux
                        .spawn_tab_or_window(
                            window_id,
                            SpawnTabDomain::DomainName("local".to_string()),
                            cmd,
                            cwd,
                            TerminalSize::default(),
                            pane_id,
                            workspace,
                            None, // optional position
                        )
                        .await
                    {
                        Ok((_tab, pane, _window_id)) => {
                            log::trace!("Spawned {file_name} as pane_id {}", pane.pane_id());
                            let mut writer = pane.writer();
                            write!(writer, "{quoted_file_name} ; exit\n").ok();
                        }
                        Err(err) => {
                            log::error!("Failed to spawn {file_name}: {err:#?}");
                        }
                    };
                })
                .detach();
            }
            ApplicationEvent::PerformKeyAssignment(action) => {
                // We should only get here when there are no windows open
                // and the user picks an action from the menubar.
                // This is not currently possible, but could be in the
                // future.

                fn spawn_command(spawn: &SpawnCommand, spawn_where: SpawnWhere) {
                    let config = config::configuration();
                    let dpi = config.dpi.unwrap_or_else(::window::default_dpi);
                    let size =
                        config.initial_size(dpi as u32, crate::cell_pixel_dims(&config, dpi).ok());
                    let term_config = Arc::new(config::TermConfig::with_config(config));

                    crate::spawn::spawn_command_impl(spawn, spawn_where, size, None, term_config)
                }

                match action {
                    KeyAssignment::QuitApplication => {
                        // If we get here, there are no windows that could have received
                        // the QuitApplication command, therefore it must be ok to quit
                        // immediately
                        if let Some(conn) = Connection::get() {
                            conn.terminate_message_loop();
                        }
                    }
                    KeyAssignment::SpawnWindow => {
                        spawn_command(&SpawnCommand::default(), SpawnWhere::NewWindow);
                    }
                    KeyAssignment::SpawnTab(spawn_where) => {
                        spawn_command(
                            &SpawnCommand {
                                domain: spawn_where,
                                ..Default::default()
                            },
                            SpawnWhere::NewWindow,
                        );
                    }
                    KeyAssignment::SpawnCommandInNewTab(spawn) => {
                        spawn_command(&spawn, SpawnWhere::NewTab);
                    }
                    KeyAssignment::SpawnCommandInNewWindow(spawn) => {
                        spawn_command(&spawn, SpawnWhere::NewWindow);
                    }
                    _ => {
                        log::warn!("unhandled perform: {action:?}");
                    }
                }
            }
        }
    }

    pub fn run_forever(&self) -> anyhow::Result<()> {
        self.connection
            .run_message_loop()
            .context("running message loop")
    }

    pub fn gui_windows(&self) -> Vec<GuiWin> {
        let windows = self.known_windows.borrow();
        let mut windows: Vec<GuiWin> = windows
            .iter()
            .map(|(window, &mux_window_id)| GuiWin {
                mux_window_id,
                window: window.clone(),
            })
            .collect();
        windows.sort_by(|a, b| a.window.cmp(&b.window));
        windows
    }

    pub fn reconcile_workspace(&self) -> Future<()> {
        let mut promise = Promise::new();
        let Some(mux) = Mux::try_get() else {
            log::warn!("cannot reconcile workspace: mux singleton is not available");
            promise.ok(());
            return promise.get_future().unwrap();
        };
        let workspace = mux.active_workspace_for_client(&self.client_id);

        if mux.is_workspace_empty(&workspace) {
            // We don't want to silently kill off things that might
            // be running in other workspaces, so let's pick one
            // and activate it
            if self.is_switching_workspace() {
                promise.ok(());
                return promise.get_future().unwrap();
            }
            for workspace in mux.iter_workspaces() {
                if !mux.is_workspace_empty(&workspace) {
                    mux.set_active_workspace_for_client(&self.client_id, &workspace);
                    log::debug!("using {} instead, as it is not empty", workspace);
                    break;
                }
            }
        }

        let workspace = mux.active_workspace_for_client(&self.client_id);
        log::debug!("workspace is {}, fixup windows", workspace);

        let mut mux_windows = mux.iter_windows_in_workspace(&workspace);

        // First, repurpose existing windows.
        // Note that both iter_windows_in_workspace and self.known_windows have a
        // deterministic iteration order, so switching back and forth should result
        // in a consistent mux <-> gui window mapping.
        let known_windows = std::mem::take(&mut *self.known_windows.borrow_mut());
        let mut windows = BTreeMap::new();
        let mut unused = BTreeMap::new();

        for (window, window_id) in known_windows.into_iter() {
            if let Some(idx) = mux_windows.iter().position(|&id| id == window_id) {
                // it already points to the desired mux window
                windows.insert(window, window_id);
                mux_windows.remove(idx);
            } else {
                unused.insert(window, window_id);
            }
        }

        let mut mux_windows = mux_windows.into_iter();

        for (window, old_id) in unused.into_iter() {
            if let Some(mux_window_id) = mux_windows.next() {
                window.notify(TermWindowNotif::SwitchToMuxWindow(mux_window_id));
                windows.insert(window, mux_window_id);
            } else {
                // We have more windows than are in the new workspace;
                // we no longer need this one!
                window.close();
                self.spawned_mux_window.borrow_mut().remove(&old_id);
            }
        }

        log::trace!("reconcile: windows -> {:?}", windows);
        *self.known_windows.borrow_mut() = windows;

        let future = promise.get_future().unwrap();

        // then spawn any new windows that are needed
        promise::spawn::spawn(async move {
            while let Some(mux_window_id) = mux_windows.next() {
                let Some(fe) = try_front_end() else {
                    promise.ok(());
                    return;
                };
                if fe.has_mux_window(mux_window_id)
                    || fe.spawned_mux_window.borrow().contains(&mux_window_id)
                {
                    continue;
                }
                fe.spawned_mux_window.borrow_mut().insert(mux_window_id);
                log::trace!("Creating TermWindow for mux_window_id={}", mux_window_id);
                if let Err(err) = TermWindow::new_window(mux_window_id).await {
                    log::error!("Failed to create window: {:#}", err);
                    if let Some(mux) = Mux::try_get() {
                        mux.kill_window(mux_window_id);
                    }
                    if let Some(fe) = try_front_end() {
                        fe.spawned_mux_window.borrow_mut().remove(&mux_window_id);
                    }
                }
            }
            if let Some(fe) = try_front_end() {
                *fe.switching_workspaces.borrow_mut() = false;
            }
            promise.ok(());
        })
        .detach();
        future
    }

    fn has_mux_window(&self, mux_window_id: MuxWindowId) -> bool {
        for &mux_id in self.known_windows.borrow().values() {
            if mux_id == mux_window_id {
                return true;
            }
        }
        false
    }

    pub fn switch_workspace(&self, workspace: &str) {
        if let Some(mux) = Mux::try_get() {
            mux.set_active_workspace_for_client(&self.client_id, workspace);
        } else {
            log::warn!("cannot switch workspace to {workspace}: mux singleton is not available");
        }
        *self.switching_workspaces.borrow_mut() = false;
        self.reconcile_workspace();
    }

    pub fn record_known_window(&self, window: Window, mux_window_id: MuxWindowId) {
        self.known_windows
            .borrow_mut()
            .insert(window, mux_window_id);
        if !self.is_switching_workspace() {
            self.reconcile_workspace();
        }
    }

    pub fn forget_known_window(&self, window: &Window) {
        self.known_windows.borrow_mut().remove(window);
        if !self.is_switching_workspace() {
            self.reconcile_workspace();
        }
    }

    pub fn is_switching_workspace(&self) -> bool {
        *self.switching_workspaces.borrow()
    }

    #[allow(dead_code)]
    pub fn gui_window_for_mux_window(&self, mux_window_id: MuxWindowId) -> Option<GuiWin> {
        let windows = self.known_windows.borrow();
        for (window, v) in windows.iter() {
            if *v == mux_window_id {
                return Some(GuiWin {
                    mux_window_id,
                    window: window.clone(),
                });
            }
        }
        None
    }

    fn record_osc22_cursor_shape(&self, pane_id: mux::pane::PaneId, shape: &str) {
        let Some(slug) = cursor_shape_slug_from_osc22_request(shape) else {
            log::debug!("ignoring unsupported OSC 22 cursor shape request: {shape:?}");
            return;
        };
        let prior = self
            .osc22_cursor_shapes
            .borrow_mut()
            .set(pane_id as u64, slug);
        self.apply_osc22_cursor_shape_for_pane(pane_id);
        if prior != Some(slug) {
            persistent_toast_notification(
                "Cursor shape changed",
                osc22_accessibility_announcement(slug).as_str(),
            );
        }
        log::debug!(
            "OSC 22 cursor shape for pane {pane_id} is now {}",
            slug.slug()
        );
    }

    fn apply_osc22_cursor_shape_for_pane(&self, pane_id: mux::pane::PaneId) {
        let Some(mux) = Mux::try_get() else {
            return;
        };
        let Some((_domain, window_id, _tab_id)) = mux.resolve_pane_id(pane_id) else {
            return;
        };
        let shape = self.osc22_cursor_shapes.borrow().get(pane_id as u64);
        if let Some(gui_window) = self.gui_window_for_mux_window(window_id) {
            gui_window
                .window
                .set_cursor(Some(mouse_cursor_for_osc22_shape(shape)));
        }
    }
}

#[must_use]
fn cursor_shape_slug_from_osc22_request(shape: &str) -> Option<CursorShapeSlug> {
    let normalized = shape.trim().to_ascii_lowercase().replace(['-', ' '], "_");
    match normalized.as_str() {
        "" | "auto" | "default" | "arrow" => Some(CursorShapeSlug::Default),
        "block" | "block_blinking" | "blinking_block" => Some(CursorShapeSlug::BlockBlinking),
        "block_steady" | "steady_block" => Some(CursorShapeSlug::BlockSteady),
        "underline" | "underline_blinking" | "blinking_underline" => {
            Some(CursorShapeSlug::UnderlineBlinking)
        }
        "underline_steady" | "steady_underline" => Some(CursorShapeSlug::UnderlineSteady),
        "bar" | "beam" | "ibeam" | "text" | "bar_blinking" | "blinking_bar" => {
            Some(CursorShapeSlug::BarBlinking)
        }
        "bar_steady" | "steady_bar" => Some(CursorShapeSlug::BarSteady),
        _ => None,
    }
}

#[must_use]
fn mouse_cursor_for_osc22_shape(shape: CursorShapeSlug) -> MouseCursor {
    match shape {
        CursorShapeSlug::Default
        | CursorShapeSlug::BlockBlinking
        | CursorShapeSlug::BlockSteady => MouseCursor::Arrow,
        CursorShapeSlug::UnderlineBlinking
        | CursorShapeSlug::UnderlineSteady
        | CursorShapeSlug::BarBlinking
        | CursorShapeSlug::BarSteady => MouseCursor::Text,
    }
}

#[must_use]
fn osc22_accessibility_announcement(shape: CursorShapeSlug) -> String {
    format!("Cursor shape {}", shape.slug().replace('_', " "))
}

fn terminal_toast_action(
    focus: bool,
    pane_id: mux::pane::PaneId,
) -> Option<ToastNotificationAction> {
    focus.then(|| {
        ToastNotificationAction::new("Focus", move || {
            promise::spawn::spawn_into_main_thread(async move {
                focus_terminal_toast_source(pane_id);
            })
            .detach();
        })
    })
}

fn focus_terminal_toast_source(pane_id: mux::pane::PaneId) {
    let Some(mux) = Mux::try_get() else {
        log::warn!("cannot focus toast source pane {pane_id}: mux singleton is not available");
        return;
    };
    let Some((_domain, window_id, _tab_id)) = mux.resolve_pane_id(pane_id) else {
        log::warn!("cannot focus toast source pane {pane_id}: pane is no longer in the mux");
        return;
    };

    if let Err(err) = mux.focus_pane_and_containing_tab(pane_id) {
        log::error!("cannot focus toast source pane {pane_id}: {err:#}");
        return;
    }

    let Some(front_end) = crate::frontend::try_front_end() else {
        log::warn!("cannot raise toast source window for pane {pane_id}: frontend is unavailable");
        return;
    };
    let Some(gui_window) = front_end.gui_window_for_mux_window(window_id) else {
        log::warn!("cannot raise toast source window for pane {pane_id}: GUI window not found");
        return;
    };

    gui_window.window.focus();
    front_end.apply_osc22_cursor_shape_for_pane(pane_id);
}

#[cfg(test)]
mod tests {
    use super::{
        CursorShapeSlug, MouseCursor, cursor_shape_slug_from_osc22_request,
        mouse_cursor_for_osc22_shape, osc22_accessibility_announcement, terminal_toast_action,
    };

    #[test]
    fn osc22_request_parser_accepts_terminal_cursor_slugs() {
        assert_eq!(
            cursor_shape_slug_from_osc22_request("block-blinking"),
            Some(CursorShapeSlug::BlockBlinking)
        );
        assert_eq!(
            cursor_shape_slug_from_osc22_request("steady underline"),
            Some(CursorShapeSlug::UnderlineSteady)
        );
        assert_eq!(
            cursor_shape_slug_from_osc22_request("bar_steady"),
            Some(CursorShapeSlug::BarSteady)
        );
    }

    #[test]
    fn osc22_request_parser_accepts_common_text_aliases() {
        for alias in ["text", "beam", "ibeam"] {
            assert_eq!(
                cursor_shape_slug_from_osc22_request(alias),
                Some(CursorShapeSlug::BarBlinking),
                "alias={alias}",
            );
        }
    }

    #[test]
    fn osc22_request_parser_rejects_unsupported_css_shapes() {
        assert_eq!(cursor_shape_slug_from_osc22_request("wait"), None);
        assert_eq!(cursor_shape_slug_from_osc22_request("crosshair"), None);
        assert_eq!(cursor_shape_slug_from_osc22_request("not-a-shape"), None);
    }

    #[test]
    fn osc22_slug_maps_to_native_mouse_cursor() {
        assert_eq!(
            mouse_cursor_for_osc22_shape(CursorShapeSlug::Default),
            MouseCursor::Arrow
        );
        assert_eq!(
            mouse_cursor_for_osc22_shape(CursorShapeSlug::BlockSteady),
            MouseCursor::Arrow
        );
        assert_eq!(
            mouse_cursor_for_osc22_shape(CursorShapeSlug::UnderlineBlinking),
            MouseCursor::Text
        );
        assert_eq!(
            mouse_cursor_for_osc22_shape(CursorShapeSlug::BarSteady),
            MouseCursor::Text
        );
    }

    #[test]
    fn osc22_accessibility_announcement_names_shape() {
        assert_eq!(
            osc22_accessibility_announcement(CursorShapeSlug::UnderlineSteady),
            "Cursor shape underline steady"
        );
    }

    #[test]
    fn terminal_toast_focus_flag_controls_activation_payload() {
        assert!(terminal_toast_action(false, 42).is_none());

        let action = terminal_toast_action(true, 42).expect("focus=true should attach action");
        assert_eq!(action.label(), "Focus");
    }
}

thread_local! {
    static FRONT_END: RefCell<Option<Rc<GuiFrontEnd>>> = RefCell::new(None);
}

pub fn try_front_end() -> Option<Rc<GuiFrontEnd>> {
    FRONT_END.with(|f| f.borrow().as_ref().map(Rc::clone))
}

pub struct WorkspaceSwitcher {
    new_name: String,
}

impl WorkspaceSwitcher {
    pub fn new(new_name: &str) -> Option<Self> {
        let front_end = try_front_end()?;
        *front_end.switching_workspaces.borrow_mut() = true;
        Some(Self {
            new_name: new_name.to_string(),
        })
    }

    pub fn do_switch(self) {
        // Drop is invoked, which will complete the switch
    }
}

impl Drop for WorkspaceSwitcher {
    fn drop(&mut self) {
        if let Some(front_end) = try_front_end() {
            front_end.switch_workspace(&self.new_name);
        }
    }
}

pub fn shutdown() {
    FRONT_END.with(|f| drop(f.borrow_mut().take()));
}

pub fn try_new() -> Result<Rc<GuiFrontEnd>, Error> {
    let front_end = GuiFrontEnd::try_new()?;
    FRONT_END.with(|f| *f.borrow_mut() = Some(Rc::clone(&front_end)));

    let config_subscription = config::subscribe_to_config_reload({
        move || {
            promise::spawn::spawn_into_main_thread(async {
                crate::commands::CommandDef::recreate_menubar(&config::configuration());
            })
            .detach();
            true
        }
    });
    front_end
        .config_subscription
        .borrow_mut()
        .replace(config_subscription);

    Ok(front_end)
}
