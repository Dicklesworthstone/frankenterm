use crate::termwindow::{OverlayCancellationTicket, TermWindow};
use mux::pane::{Pane, PaneId, PaneRenderFacts};
use mux::renderable::RenderableDimensions;
use mux::tab::{Tab, TabId};
use mux::termwiztermtab::{TermWizTerminal, allocate};
use std::pin::Pin;
use std::sync::Arc;
use termwiz::surface::SequenceNo;
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

/// Display metadata for one overlay operation. Local panes publish these
/// fields together after applying output, so reading them cannot wait behind
/// the parser's terminal lock. Retaining this observation also keeps search
/// sequence/geometry in the same publication.
///
/// This is not selection or row-read authority: the overlays must still use
/// their existing layout capabilities and source fences before acting on
/// content. Other pane implementations keep their individual accessors; the
/// default `Pane::render_facts` may itself fetch rows and take terminal locks.
enum PaneUiMetadata<'a> {
    Published(Arc<PaneRenderFacts>),
    Direct(&'a dyn Pane),
}

impl<'a> PaneUiMetadata<'a> {
    fn capture(pane: &'a dyn Pane) -> Self {
        if mux::localpane::lock_free_mouse_enabled()
            && pane.downcast_ref::<mux::localpane::LocalPane>().is_some()
        {
            Self::Published(pane.render_facts())
        } else {
            Self::Direct(pane)
        }
    }

    fn dimensions(&self) -> RenderableDimensions {
        match self {
            Self::Published(facts) => facts.dimensions,
            Self::Direct(pane) => pane.get_dimensions(),
        }
    }

    fn sequence(&self) -> SequenceNo {
        match self {
            Self::Published(facts) => facts.seqno,
            Self::Direct(pane) => pane.get_current_seqno(),
        }
    }
}

fn pane_dimensions(pane: &dyn Pane) -> RenderableDimensions {
    PaneUiMetadata::capture(pane).dimensions()
}

fn pane_sequence(pane: &dyn Pane) -> SequenceNo {
    PaneUiMetadata::capture(pane).sequence()
}

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
    start_overlay_pane_with_deadline(term_window, pane, None, func)
}

pub(crate) fn start_bounded_overlay_pane<T, F>(
    term_window: &TermWindow,
    pane: &Arc<dyn Pane>,
    deadline: std::time::Instant,
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
    start_overlay_pane_with_deadline(term_window, pane, Some(deadline), func)
}

fn start_overlay_pane_with_deadline<T, F>(
    term_window: &TermWindow,
    pane: &Arc<dyn Pane>,
    deadline: Option<std::time::Instant>,
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
    let dims = pane_dimensions(&**pane);
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
    let (tw_term, tw_tab) = match deadline {
        Some(deadline) => mux::termwiztermtab::allocate_bounded(size, term_config, deadline)?,
        None => allocate(size, term_config)?,
    };
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

#[cfg(all(test, unix))]
mod metadata_tests {
    use super::*;
    use mux::pane::{CachePolicy, ForEachPaneLogicalLine, LogicalLine, WithPaneLines};
    use mux::renderable::StableCursorPosition;
    use std::ops::Range;
    use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
    use std::time::Duration;
    use wezterm_term::{KeyCode, KeyModifiers, Line, MouseEvent, StableRowIndex};

    struct LocalFixture(Arc<dyn Pane>);

    impl LocalFixture {
        fn new(pane_id: PaneId) -> Self {
            #[derive(Debug)]
            struct TestConfig;
            impl TerminalConfiguration for TestConfig {
                fn color_palette(&self) -> wezterm_term::color::ColorPalette {
                    Default::default()
                }
            }
            let mut terminal = wezterm_term::Terminal::new(
                TerminalSize {
                    rows: 4,
                    cols: 20,
                    dpi: 96,
                    pixel_width: 160,
                    pixel_height: 64,
                },
                Arc::new(TestConfig),
                "overlay-metadata-test",
                "test",
                Box::new(Vec::<u8>::new()),
            );
            terminal.advance_bytes(b"\x1b[3;7H");
            let pair = portable_pty::native_pty_system()
                .openpty(portable_pty::PtySize::default())
                .unwrap();
            let writer = pair.master.take_writer().unwrap();
            let child = pair
                .slave
                .spawn_command(portable_pty::CommandBuilder::new("/bin/cat"))
                .unwrap();
            Self(Arc::new(mux::localpane::LocalPane::new(
                pane_id,
                terminal,
                child,
                pair.master,
                writer,
                pane_id,
                [0x48; 16],
                "overlay metadata test".to_owned(),
            )))
        }
    }

    impl Drop for LocalFixture {
        fn drop(&mut self) {
            self.0.kill();
        }
    }

    #[test]
    fn overlay_metadata_retains_one_publication_across_output() {
        let fixture = LocalFixture::new(998_601);
        let pane = &*fixture.0;
        let first = PaneUiMetadata::capture(pane);
        let sequence = first.sequence();
        let dimensions = first.dimensions();
        assert!(matches!(&first, PaneUiMetadata::Published(_)));
        assert_eq!(first.dimensions().cols, 20);
        assert_eq!(first.dimensions().viewport_rows, 4);

        let mut actions = Vec::new();
        termwiz::escape::parser::Parser::new()
            .parse(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n", |action| {
                actions.push(action)
            });
        pane.perform_actions(actions).unwrap();
        let next = PaneUiMetadata::capture(pane);
        assert!(next.sequence() > sequence);
        assert!(next.dimensions().physical_top > dimensions.physical_top);
        assert_eq!(first.sequence(), sequence);
        assert_eq!(first.dimensions(), dimensions);
        assert_eq!(next.sequence(), pane.get_current_seqno());
        assert_eq!(next.dimensions(), pane.get_dimensions());
    }

    #[test]
    fn overlay_metadata_does_not_wait_for_a_held_terminal() {
        struct HoldTerminal {
            held: SyncSender<()>,
            release: Receiver<()>,
        }
        impl WithPaneLines for HoldTerminal {
            fn with_lines_mut(&mut self, _: StableRowIndex, _: &mut [&mut Line]) {
                self.held.send(()).unwrap();
                self.release
                    .recv_timeout(Duration::from_secs(10))
                    .expect("test must release its terminal holder");
            }
        }

        let fixture = LocalFixture::new(998_602);
        let pane = &*fixture.0;
        let expected = (pane.get_dimensions(), pane.get_current_seqno());
        let (held_tx, held_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let (result_tx, result_rx) = sync_channel(1);
        let result = std::thread::scope(|scope| {
            scope.spawn(move || {
                pane.with_lines_mut(
                    0..1,
                    &mut HoldTerminal {
                        held: held_tx,
                        release: release_rx,
                    },
                );
            });
            held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            scope.spawn(move || {
                result_tx
                    .send((pane_dimensions(pane), pane_sequence(pane)))
                    .unwrap();
            });
            let result = result_rx.recv_timeout(Duration::from_secs(5));
            // Release before asserting, so a regression fails instead of
            // hanging the test runner while scoped threads are joined.
            release_tx.send(()).unwrap();
            result
        });
        assert_eq!(
            result.expect("metadata waited for the terminal lock"),
            expected
        );
    }

    /// A non-local pane must keep its own getters. In particular, using the
    /// default render_facts here would perform a row read that UI metadata
    /// never needed before this optimization.
    struct GetterOnlyPane(Arc<dyn Pane>);

    impl Pane for GetterOnlyPane {
        fn pane_id(&self) -> PaneId {
            self.0.pane_id()
        }
        fn get_cursor_position(&self) -> StableCursorPosition {
            self.0.get_cursor_position()
        }
        fn get_current_seqno(&self) -> SequenceNo {
            self.0.get_current_seqno()
        }
        fn get_dimensions(&self) -> RenderableDimensions {
            self.0.get_dimensions()
        }
        fn get_changed_since(
            &self,
            rows: Range<StableRowIndex>,
            sequence: SequenceNo,
        ) -> rangeset::RangeSet<StableRowIndex> {
            self.0.get_changed_since(rows, sequence)
        }
        fn get_lines(&self, _: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            panic!("overlay metadata must not read rows from a non-local pane")
        }
        fn with_lines_mut(&self, rows: Range<StableRowIndex>, f: &mut dyn WithPaneLines) {
            self.0.with_lines_mut(rows, f)
        }
        fn for_each_logical_line_in_stable_range_mut(
            &self,
            rows: Range<StableRowIndex>,
            f: &mut dyn ForEachPaneLogicalLine,
        ) {
            self.0.for_each_logical_line_in_stable_range_mut(rows, f)
        }
        fn get_logical_lines(&self, rows: Range<StableRowIndex>) -> Vec<LogicalLine> {
            self.0.get_logical_lines(rows)
        }
        fn get_title(&self) -> String {
            self.0.get_title()
        }
        fn send_paste(&self, text: &str) -> anyhow::Result<()> {
            self.0.send_paste(text)
        }
        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(None)
        }
        fn writer(&self) -> parking_lot::MappedMutexGuard<'_, dyn std::io::Write> {
            self.0.writer()
        }
        fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
            self.0.resize(size)
        }
        fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
            self.0.key_down(key, mods)
        }
        fn key_up(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
            self.0.key_up(key, mods)
        }
        fn mouse_event(&self, event: MouseEvent) -> anyhow::Result<()> {
            self.0.mouse_event(event)
        }
        fn is_dead(&self) -> bool {
            self.0.is_dead()
        }
        fn palette(&self) -> wezterm_term::color::ColorPalette {
            self.0.palette()
        }
        fn domain_id(&self) -> mux::domain::DomainId {
            self.0.domain_id()
        }
        fn is_mouse_grabbed(&self) -> bool {
            self.0.is_mouse_grabbed()
        }
        fn is_alt_screen_active(&self) -> bool {
            self.0.is_alt_screen_active()
        }
        fn mux_registration_slot(&self) -> &Arc<mux::PaneRegistrationSlot> {
            self.0.mux_registration_slot()
        }
        fn get_current_working_dir(&self, policy: CachePolicy) -> Option<url::Url> {
            self.0.get_current_working_dir(policy)
        }
    }

    #[test]
    fn overlay_metadata_preserves_non_local_getters_without_reading_rows() {
        let fixture = LocalFixture::new(998_603);
        let pane = GetterOnlyPane(Arc::clone(&fixture.0));
        let metadata = PaneUiMetadata::capture(&pane);
        assert!(matches!(&metadata, PaneUiMetadata::Direct(_)));
        assert_eq!(metadata.dimensions(), fixture.0.get_dimensions());
        assert_eq!(metadata.sequence(), fixture.0.get_current_seqno());
        assert_eq!(pane_dimensions(&pane), metadata.dimensions());
        assert_eq!(pane_sequence(&pane), metadata.sequence());
    }
}
