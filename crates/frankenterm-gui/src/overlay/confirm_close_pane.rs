use super::confirm;
use mux::tab::Tab;
use mux::termwiztermtab::TermWizTerminal;
use mux::window::WindowId;
use mux::{Mux, PaneRegistrationHandle};
use std::sync::Arc;

pub fn confirm_close_pane(
    mut term: TermWizTerminal,
    registration: PaneRegistrationHandle,
    tab: Arc<Tab>,
) -> anyhow::Result<()> {
    if confirm::run_confirmation("🛑 Really kill this pane?", &mut term)? {
        promise::spawn::spawn_into_main_thread(async move {
            if !tab.kill_pane_registration(&registration) {
                log::warn!(
                    "cannot close pane {}: exact registration is no longer active",
                    registration.pane_id(),
                );
            }
        })
        .detach();
    }
    Ok(())
}

pub fn confirm_close_tab(
    mut term: TermWizTerminal,
    mux: Arc<Mux>,
    tab: Arc<Tab>,
    witness: PaneRegistrationHandle,
) -> anyhow::Result<()> {
    if confirm::run_confirmation(
        "🛑 Really kill this tab and all contained panes?",
        &mut term,
    )? {
        promise::spawn::spawn_into_main_thread(async move {
            if !mux.remove_tab_if_same(&tab, &witness) {
                log::warn!(
                    "cannot close tab {}: exact tab generation is no longer active",
                    tab.tab_id(),
                );
            }
        })
        .detach();
    }
    Ok(())
}

pub fn confirm_close_window(
    mut term: TermWizTerminal,
    mux: Arc<Mux>,
    mux_window_id: WindowId,
    tab: Arc<Tab>,
    witness: PaneRegistrationHandle,
) -> anyhow::Result<()> {
    if confirm::run_confirmation(
        "🛑 Really kill this window and all contained tabs and panes?",
        &mut term,
    )? {
        promise::spawn::spawn_into_main_thread(async move {
            let Some(operation) = witness.operation_guard(&mux) else {
                log::warn!(
                    "cannot close window {mux_window_id}: exact pane generation is no longer active"
                );
                return;
            };
            if !tab
                .iter_all_panes()
                .iter()
                .any(|pane| operation.is_same_pane(pane))
            {
                log::warn!(
                    "cannot close window {mux_window_id}: witness pane left the originating tab"
                );
                return;
            }
            let tab_is_still_attached = mux.get_window(mux_window_id).is_some_and(|window| {
                window
                    .iter()
                    .any(|candidate| Arc::ptr_eq(candidate, &tab))
            });
            if !tab_is_still_attached {
                log::warn!(
                    "cannot close window {mux_window_id}: exact originating tab is no longer attached"
                );
                return;
            }
            mux.kill_window(mux_window_id);
        })
        .detach();
    }
    Ok(())
}

pub fn confirm_quit_program(mut term: TermWizTerminal) -> anyhow::Result<()> {
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
    Ok(())
}
