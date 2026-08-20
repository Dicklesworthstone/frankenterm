use crate::termwindow::{OverlayCancellationTicket, TermWindow};
use mux::pane::{Pane, PaneId};
use mux::tab::{Tab, TabId};
use mux::termwiztermtab::{TermWizTerminal, allocate};
use std::pin::Pin;
use std::sync::Arc;
use wezterm_term::{TerminalConfiguration, TerminalSize};

pub mod confirm;
pub mod confirm_close_pane;
pub mod copy;
pub mod debug;
pub mod launcher;
pub mod prompt;
pub mod quickselect;
pub mod selector;

pub use confirm_close_pane::{
    confirm_close_pane, confirm_close_tab, confirm_close_window, confirm_quit_program,
};
pub use copy::{CopyModeParams, CopyOverlay};
pub use debug::show_debug_overlay;
pub use launcher::{LauncherArgs, LauncherFlags, launcher};
pub use quickselect::QuickSelectOverlay;

pub(crate) const OVERLAY_MAIN_THREAD_ESTIMATED_BYTES: usize = 4 * 1024;

pub(crate) fn reserve_overlay_main_thread(
    service_class: promise::spawn::MainThreadServiceClass,
    estimated_bytes: usize,
    operation: &'static str,
) -> anyhow::Result<promise::spawn::MainThreadSpawnReservation> {
    match promise::spawn::try_reserve_main_thread(service_class, estimated_bytes) {
        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
            metrics::counter!(
                "gui.overlay.main_thread_admission",
                "operation" => operation,
                "outcome" => "admitted"
            )
            .increment(1);
            Ok(reservation)
        }
        rejected => {
            metrics::counter!(
                "gui.overlay.main_thread_admission",
                "operation" => operation,
                "outcome" => "terminal_rejection"
            )
            .increment(1);
            Err(anyhow::anyhow!(
                "main-thread scheduler rejected overlay {operation} before task construction: {rejected:?}"
            ))
        }
    }
}

#[derive(Clone, Copy)]
enum OverlayCancellationTarget {
    Tab {
        tab_id: TabId,
        overlay_pane_id: PaneId,
    },
    Pane {
        pane_id: PaneId,
    },
}

/// Schedules exact overlay cleanup when a worker returns, unwinds, or is
/// abandoned before it starts. The ticket was minted from the newly allocated
/// TermWiz pane registration and is never reconstructed from a numeric ID.
struct OverlayCancellationDispatch {
    window: ::window::Window,
    target: OverlayCancellationTarget,
    ticket: Option<OverlayCancellationTicket>,
}

impl Drop for OverlayCancellationDispatch {
    fn drop(&mut self) {
        let Some(ticket) = self.ticket.take() else {
            return;
        };
        match self.target {
            OverlayCancellationTarget::Tab {
                tab_id,
                overlay_pane_id,
            } => TermWindow::schedule_cancel_overlay(
                self.window.clone(),
                tab_id,
                overlay_pane_id,
                ticket,
            ),
            OverlayCancellationTarget::Pane { pane_id } => {
                TermWindow::schedule_cancel_overlay_for_pane(self.window.clone(), pane_id, ticket);
            }
        }
    }
}

fn retire_failed_overlay_allocation(pane: &Arc<dyn Pane>) {
    if let Some(registration) = pane.mux_registration_slot().load() {
        let _ = registration.retire_if_current();
    }
}

pub(crate) fn start_overlay<T, F>(
    term_window: &TermWindow,
    tab: &Arc<Tab>,
    func: F,
) -> anyhow::Result<(
    Arc<dyn Pane>,
    OverlayCancellationTicket,
    Pin<Box<dyn std::future::Future<Output = anyhow::Result<T>>>>,
)>
where
    T: Send + 'static,
    F: Send + 'static + FnOnce(TabId, TermWizTerminal) -> anyhow::Result<T>,
{
    let tab_id = tab.tab_id();
    let tab_size = tab.get_size();
    let window = term_window
        .window
        .clone()
        .ok_or_else(|| anyhow::anyhow!("cannot start overlay without a GUI window"))?;
    let term_config: Arc<dyn TerminalConfiguration + Send + Sync> =
        Arc::new(config::TermConfig::with_config(term_window.config.clone()));
    let (tw_term, tw_tab) = allocate(tab_size, term_config)?;
    let overlay_pane_id = tw_tab.pane_id();
    let cancellation_ticket =
        match TermWindow::mint_tab_overlay_cancellation_ticket(tab_id, &tw_tab) {
            Ok(ticket) => ticket,
            Err(err) => {
                retire_failed_overlay_allocation(&tw_tab);
                return Err(err);
            }
        };
    let worker_ticket = cancellation_ticket.clone();

    let future = promise::spawn::spawn_into_new_thread(move || {
        let _cancellation = OverlayCancellationDispatch {
            window,
            target: OverlayCancellationTarget::Tab {
                tab_id,
                overlay_pane_id,
            },
            ticket: Some(worker_ticket),
        };
        func(tab_id, tw_term)
    });

    Ok((tw_tab, cancellation_ticket, Box::pin(future)))
}

pub(crate) fn start_overlay_pane<T, F>(
    term_window: &TermWindow,
    pane: &Arc<dyn Pane>,
    func: F,
) -> anyhow::Result<(
    Arc<dyn Pane>,
    OverlayCancellationTicket,
    Pin<Box<dyn std::future::Future<Output = anyhow::Result<T>>>>,
)>
where
    T: Send + 'static,
    F: Send + 'static + FnOnce(PaneId, TermWizTerminal) -> anyhow::Result<T>,
{
    let pane_id = pane.pane_id();
    let dims = pane.get_dimensions();
    let window = term_window
        .window
        .clone()
        .ok_or_else(|| anyhow::anyhow!("cannot start pane overlay without a GUI window"))?;
    let size = TerminalSize {
        cols: dims.cols,
        rows: dims.viewport_rows,
        pixel_width: term_window.render_metrics.cell_size.width as usize * dims.cols,
        pixel_height: term_window.render_metrics.cell_size.height as usize * dims.viewport_rows,
        dpi: dims.dpi,
    };
    let term_config: Arc<dyn TerminalConfiguration + Send + Sync> =
        Arc::new(config::TermConfig::with_config(term_window.config.clone()));
    let (tw_term, tw_tab) = allocate(size, term_config)?;
    let cancellation_ticket =
        match TermWindow::mint_pane_overlay_cancellation_ticket(pane_id, &tw_tab) {
            Ok(ticket) => ticket,
            Err(err) => {
                retire_failed_overlay_allocation(&tw_tab);
                return Err(err);
            }
        };
    let worker_ticket = cancellation_ticket.clone();

    let future = promise::spawn::spawn_into_new_thread(move || {
        let _cancellation = OverlayCancellationDispatch {
            window,
            target: OverlayCancellationTarget::Pane { pane_id },
            ticket: Some(worker_ticket),
        };
        func(pane_id, tw_term)
    });

    Ok((tw_tab, cancellation_ticket, Box::pin(future)))
}
