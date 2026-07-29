use super::confirm;
use crate::TermWindow;
use mux::Mux;
use mux::pane::PaneId;
use mux::tab::TabId;
use mux::termwiztermtab::TermWizTerminal;
use mux::window::WindowId;

pub fn confirm_close_pane(
    pane_id: PaneId,
    mut term: TermWizTerminal,
    mux_window_id: WindowId,
    window: ::window::Window,
) -> anyhow::Result<()> {
    let close_target = Mux::try_get().and_then(|mux| {
        let registration = mux.capture_current_pane(pane_id)?;
        let (_domain_id, window_id, tab_id) = mux.resolve_pane_id(pane_id)?;
        if window_id != mux_window_id {
            return None;
        }
        let tab = mux.get_tab(tab_id)?;
        Some((registration, tab))
    });

    if confirm::run_confirmation("🛑 Really kill this pane?", &mut term)? {
        promise::spawn::spawn_into_main_thread(async move {
            let Some((registration, tab)) = close_target else {
                log::warn!("cannot close pane {pane_id}: exact registration is no longer active");
                return;
            };
            let _ = tab.kill_pane_registration(&registration);
        })
        .detach();
    }
    TermWindow::schedule_cancel_overlay_for_pane(window, pane_id);

    Ok(())
}

pub fn confirm_close_tab(
    tab_id: TabId,
    mut term: TermWizTerminal,
    _mux_window_id: WindowId,
    window: ::window::Window,
) -> anyhow::Result<()> {
    if confirm::run_confirmation(
        "🛑 Really kill this tab and all contained panes?",
        &mut term,
    )? {
        promise::spawn::spawn_into_main_thread(async move {
            let Some(mux) = Mux::try_get() else {
                log::warn!("cannot close tab {tab_id}: mux is no longer active");
                return;
            };
            mux.remove_tab(tab_id);
        })
        .detach();
    }
    TermWindow::schedule_cancel_overlay(window, tab_id, None);

    Ok(())
}

pub fn confirm_close_window(
    mut term: TermWizTerminal,
    mux_window_id: WindowId,
    window: ::window::Window,
    tab_id: TabId,
) -> anyhow::Result<()> {
    if confirm::run_confirmation(
        "🛑 Really kill this window and all contained tabs and panes?",
        &mut term,
    )? {
        promise::spawn::spawn_into_main_thread(async move {
            let Some(mux) = Mux::try_get() else {
                log::warn!("cannot close window {mux_window_id}: mux is no longer active");
                return;
            };
            mux.kill_window(mux_window_id);
        })
        .detach();
    }
    TermWindow::schedule_cancel_overlay(window, tab_id, None);

    Ok(())
}

pub fn confirm_quit_program(
    mut term: TermWizTerminal,
    window: ::window::Window,
    tab_id: TabId,
) -> anyhow::Result<()> {
    if confirm::run_confirmation("🛑 Really Quit FrankenTerm?", &mut term)? {
        promise::spawn::spawn_into_main_thread(async move {
            use ::window::{Connection, ConnectionOps};
            match Connection::get() {
                Some(con) => con.terminate_message_loop(),
                None => log::warn!("cannot quit program: GUI connection is no longer active"),
            }
        })
        .detach();
    }
    TermWindow::schedule_cancel_overlay(window, tab_id, None);

    Ok(())
}
