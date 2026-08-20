use super::confirm;
use mux::tab::Tab;
use mux::termwiztermtab::TermWizTerminal;
use mux::window::WindowId;
use mux::{Mux, PaneRegistrationHandle};
use std::sync::Arc;

fn reserve_confirmed_action(
    operation: &'static str,
) -> anyhow::Result<promise::spawn::MainThreadSpawnReservation> {
    super::reserve_overlay_main_thread(
        promise::spawn::MainThreadServiceClass::Input,
        super::OVERLAY_MAIN_THREAD_ESTIMATED_BYTES,
        operation,
    )
}

pub fn confirm_close_pane(
    mut term: TermWizTerminal,
    registration: PaneRegistrationHandle,
    tab: Arc<Tab>,
) -> anyhow::Result<()> {
    if confirm::run_confirmation("🛑 Really kill this pane?", &mut term)? {
        reserve_confirmed_action("close pane")?
            .spawn(async move {
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
        reserve_confirmed_action("close tab")?
            .spawn(async move {
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
        reserve_confirmed_action("close window")?
            .spawn(async move {
                if !mux.kill_window_if_contains_exact_tab(mux_window_id, &tab, &witness) {
                    log::warn!(
                        "cannot close window {mux_window_id}: exact originating tab generation is no longer attached"
                    );
                }
            })
            .detach();
    }
    Ok(())
}

pub fn confirm_quit_program(mut term: TermWizTerminal) -> anyhow::Result<()> {
    if confirm::run_confirmation("🛑 Really Quit FrankenTerm?", &mut term)? {
        reserve_confirmed_action("quit program")?
            .spawn(async move {
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
