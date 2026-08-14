//! a tab hosting a termwiz terminal applet
//! The idea is to use these when wezterm needs to request
//! input from the user as part of eg: setting up an ssh
//! session.

use std::convert::TryFrom;

use crate::client::ClientId;
use crate::domain::{Domain, DomainId, DomainState, alloc_domain_id};
use crate::pane::{
    CachePolicy, CloseReason, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId, WithPaneLines,
    alloc_pane_id,
};
use crate::renderable::*;
use crate::tab::Tab;
use crate::window::WindowId;
use crate::{Mux, PaneRegistrationHandle, PaneRegistrationSlot};
use anyhow::{Context, bail};
use async_trait::async_trait;
use config::keyassignment::ScrollbackEraseMode;
use crossbeam::channel::{Receiver, Sender, unbounded as channel};
use filedescriptor::{FileDescriptor, Pipe};
use frankenterm_term::color::ColorPalette;
use frankenterm_term::{
    KeyCode, KeyModifiers, MouseEvent, StableRowIndex, TerminalConfiguration, TerminalSize,
};
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use portable_pty::*;
use rangeset::RangeSet;
use std::io::{BufWriter, Write};
use std::ops::Range;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use termwiz::input::{InputEvent, KeyEvent, Modifiers, MouseEvent as TermWizMouseEvent};
use termwiz::render::terminfo::TerminfoRenderer;
use termwiz::surface::{Change, Line, SequenceNo};
use termwiz::terminal::{ScreenSize, TerminalWaker};
use url::Url;

struct TermWizTerminalDomain {
    domain_id: DomainId,
}

static TERMWIZ_DOMAIN: OnceLock<Arc<dyn Domain>> = OnceLock::new();

fn termwiz_terminal_domain() -> Arc<dyn Domain> {
    Arc::clone(TERMWIZ_DOMAIN.get_or_init(|| {
        let domain_id = alloc_domain_id();
        Arc::new(TermWizTerminalDomain { domain_id })
    }))
}

fn usize_to_u16_saturating(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn row_to_u16_saturating(value: i64) -> u16 {
    u16::try_from(value.max(0)).unwrap_or(u16::MAX)
}

#[async_trait(?Send)]
impl Domain for TermWizTerminalDomain {
    async fn spawn_pane(
        &self,
        _mux: &Arc<Mux>,
        _size: TerminalSize,
        _command: Option<CommandBuilder>,
        _command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        bail!("cannot spawn panes in a TermWizTerminalPane");
    }

    fn spawnable(&self) -> bool {
        false
    }

    fn supports_floating_pane_spawn(&self) -> bool {
        false
    }

    fn domain_id(&self) -> DomainId {
        self.domain_id
    }

    fn domain_name(&self) -> &str {
        "TermWizTerminalDomain"
    }
    async fn attach(
        &self,
        _mux: &Arc<Mux>,
        _owner_client_id: Option<Arc<ClientId>>,
        _window_id: Option<WindowId>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn detachable(&self) -> bool {
        false
    }

    fn detach(&self) -> anyhow::Result<()> {
        bail!(
            "detach is unsupported for TermWizTerminalDomain because termwiz applet panes are inline UI surfaces"
        );
    }

    fn state(&self) -> DomainState {
        DomainState::Attached
    }
}

pub struct TermWizTerminalPane {
    pane_id: PaneId,
    domain_id: DomainId,
    terminal: Mutex<frankenterm_term::Terminal>,
    input_tx: Sender<InputEvent>,
    dead: Mutex<bool>,
    writer: Mutex<Vec<u8>>,
    render_rx: FileDescriptor,
    mux_registration: Arc<PaneRegistrationSlot>,
}

impl TermWizTerminalPane {
    fn new(
        domain_id: DomainId,
        size: TerminalSize,
        input_tx: Sender<InputEvent>,
        render_rx: FileDescriptor,
        term_config: Option<Arc<dyn TerminalConfiguration + Send + Sync>>,
    ) -> Result<Self, crate::IdAllocationError> {
        let pane_id = alloc_pane_id()?;

        let terminal = Mutex::new(frankenterm_term::Terminal::new(
            size,
            term_config.unwrap_or_else(|| Arc::new(config::TermConfig::new())),
            "WezTerm",
            config::wezterm_version(),
            Box::new(Vec::new()), // Sink writer; TermWiz applets use render_rx for output
        ));

        Ok(Self {
            pane_id,
            domain_id,
            terminal,
            writer: Mutex::new(Vec::new()),
            render_rx,
            input_tx,
            dead: Mutex::new(false),
            mux_registration: Arc::new(PaneRegistrationSlot::default()),
        })
    }
}

impl Pane for TermWizTerminalPane {
    fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    fn mux_registration_slot(&self) -> &Arc<PaneRegistrationSlot> {
        &self.mux_registration
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        terminal_get_cursor_position(&mut self.terminal.lock())
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.terminal.lock().current_seqno()
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        terminal_get_dirty_lines(&mut self.terminal.lock(), lines, seqno)
    }

    fn get_changed_since_with_source_fence(
        &self,
        lines: Range<StableRowIndex>,
        last_observed_source_end: SequenceNo,
    ) -> (SequenceNo, RangeSet<StableRowIndex>) {
        let mut terminal = self.terminal.lock();
        let source_end = terminal.current_seqno();
        let baseline =
            crate::pane::changed_since_query_baseline(last_observed_source_end, source_end);
        let changed = terminal_get_dirty_lines(&mut terminal, lines, baseline);
        (source_end, changed)
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        terminal_for_each_logical_line_in_stable_range_mut(
            &mut self.terminal.lock(),
            lines,
            for_line,
        );
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        crate::pane::impl_get_logical_lines_via_get_lines(self, lines)
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        terminal_with_lines_mut(&mut self.terminal.lock(), lines, with_lines)
    }

    fn with_lines_mut_and_apply_hyperlinks(
        &self,
        lines: Range<StableRowIndex>,
        rules: &[termwiz::hyperlink::Rule],
        with_lines: &mut dyn WithPaneLines,
    ) {
        terminal_with_lines_mut_and_apply_hyperlinks(
            &mut self.terminal.lock(),
            lines,
            rules,
            with_lines,
        )
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        terminal_get_lines(&mut self.terminal.lock(), lines)
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        terminal_get_dimensions(&mut self.terminal.lock())
    }

    fn get_tiered_scrollback_status(
        &self,
    ) -> Option<crate::renderable::PaneTieredScrollbackStatus> {
        Some(
            self.terminal
                .lock()
                .screen()
                .tiered_scrollback_status()
                .into(),
        )
    }

    fn get_title(&self) -> String {
        self.terminal.lock().get_title().to_string()
    }

    fn can_close_without_prompting(&self, _reason: CloseReason) -> bool {
        true
    }

    fn send_paste(&self, text: &str) -> anyhow::Result<()> {
        let paste = InputEvent::Paste(text.to_string());
        self.input_tx.send(paste)?;
        Ok(())
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(Some(Box::new(self.render_rx.try_clone()?)))
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        MutexGuard::map(self.writer.lock(), |writer| {
            let w: &mut dyn std::io::Write = writer;
            w
        })
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        self.input_tx.send(InputEvent::Resized {
            rows: size.rows as usize,
            cols: size.cols as usize,
        })?;

        self.terminal.lock().resize(size);

        Ok(())
    }

    fn key_down(&self, key: KeyCode, modifiers: KeyModifiers) -> anyhow::Result<()> {
        let event = InputEvent::Key(KeyEvent {
            key,
            modifiers: modifiers.remove_positional_mods(),
        });
        if let Err(e) = self.input_tx.send(event) {
            *self.dead.lock() = true;
            return Err(e.into());
        }
        Ok(())
    }

    fn key_up(&self, _key: KeyCode, _modifiers: KeyModifiers) -> anyhow::Result<()> {
        Ok(())
    }

    fn mouse_event(&self, event: MouseEvent) -> anyhow::Result<()> {
        use frankenterm_term::input::MouseButton;
        use termwiz::input::MouseButtons as Buttons;

        let mouse_buttons = match event.button {
            MouseButton::Left => Buttons::LEFT,
            MouseButton::Middle => Buttons::MIDDLE,
            MouseButton::Right => Buttons::RIGHT,
            MouseButton::WheelUp(_) => Buttons::VERT_WHEEL | Buttons::WHEEL_POSITIVE,
            MouseButton::WheelDown(_) => Buttons::VERT_WHEEL,
            MouseButton::WheelLeft(_) => Buttons::HORZ_WHEEL | Buttons::WHEEL_POSITIVE,
            MouseButton::WheelRight(_) => Buttons::HORZ_WHEEL,
            MouseButton::None => Buttons::NONE,
        };

        let event = InputEvent::Mouse(TermWizMouseEvent {
            x: usize_to_u16_saturating(event.x),
            y: row_to_u16_saturating(event.y),
            mouse_buttons,
            modifiers: event.modifiers,
        });
        if let Err(e) = self.input_tx.send(event) {
            *self.dead.lock() = true;
            return Err(e.into());
        }
        Ok(())
    }

    fn set_config(&self, config: Arc<dyn TerminalConfiguration>) {
        self.terminal.lock().set_config(config);
    }

    fn get_config(&self) -> Option<Arc<dyn TerminalConfiguration>> {
        Some(self.terminal.lock().get_config())
    }

    fn perform_actions(&self, actions: Vec<termwiz::escape::Action>) {
        self.terminal.lock().perform_actions(actions)
    }

    fn kill(&self) {
        *self.dead.lock() = true;
    }

    fn is_dead(&self) -> bool {
        *self.dead.lock()
    }

    fn palette(&self) -> ColorPalette {
        self.terminal.lock().palette()
    }

    fn domain_id(&self) -> DomainId {
        self.domain_id
    }

    fn is_mouse_grabbed(&self) -> bool {
        self.terminal.lock().is_mouse_grabbed()
    }

    fn is_alt_screen_active(&self) -> bool {
        self.terminal.lock().is_alt_screen_active()
    }

    fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
        self.terminal.lock().get_current_dir().cloned()
    }

    fn erase_scrollback(&self, erase_mode: ScrollbackEraseMode) {
        match erase_mode {
            ScrollbackEraseMode::ScrollbackOnly => {
                self.terminal.lock().erase_scrollback();
            }
            ScrollbackEraseMode::ScrollbackAndViewport => {
                self.terminal.lock().erase_scrollback_and_viewport();
            }
        }
    }
}

pub struct TermWizTerminal {
    render_tx: TermWizTerminalRenderTty,
    input_rx: Receiver<InputEvent>,
    renderer: TerminfoRenderer,
    grab_mouse: bool,
}

impl TermWizTerminal {
    pub fn no_grab_mouse_in_raw_mode(&mut self) {
        self.grab_mouse = false;
    }
}

struct TermWizTerminalRenderTty {
    render_tx: BufWriter<FileDescriptor>,
    screen_size: ScreenSize,
}

impl std::io::Write for TermWizTerminalRenderTty {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.render_tx.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.render_tx.flush()
    }
}

impl termwiz::render::RenderTty for TermWizTerminalRenderTty {
    fn get_size_in_cells(&mut self) -> termwiz::Result<(usize, usize)> {
        Ok((self.screen_size.cols, self.screen_size.rows))
    }
}

impl TermWizTerminal {
    fn do_input_poll(&mut self, wait: Option<Duration>) -> termwiz::Result<Option<InputEvent>> {
        if let Some(timeout) = wait {
            match self.input_rx.recv_timeout(timeout) {
                Ok(input) => Ok(Some(input)),
                Err(err) => {
                    if err.is_timeout() {
                        Ok(None)
                    } else {
                        Err(termwiz::error::Error::from(format!(
                            "receive from channel: {err}"
                        )))
                    }
                }
            }
        } else {
            let input = self.input_rx.recv().map_err(|err| {
                termwiz::error::Error::from(format!("receive from channel: {err}"))
            })?;
            Ok(Some(input))
        }
    }
}

impl termwiz::terminal::Terminal for TermWizTerminal {
    fn set_raw_mode(&mut self) -> termwiz::Result<()> {
        use termwiz::escape::csi::{CSI, DecPrivateMode, DecPrivateModeCode, Mode};

        macro_rules! decset {
            ($variant:ident) => {
                write!(
                    self.render_tx,
                    "{}",
                    CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                        DecPrivateModeCode::$variant
                    )))
                )?;
            };
        }

        decset!(BracketedPaste);
        if self.grab_mouse {
            decset!(AnyEventMouse);
            decset!(SGRMouse);
        }
        self.flush()?;

        Ok(())
    }

    fn set_cooked_mode(&mut self) -> termwiz::Result<()> {
        Ok(())
    }

    fn enter_alternate_screen(&mut self) -> termwiz::Result<()> {
        termwiz::bail!("TermWizTerminalPane has no alt screen");
    }

    fn exit_alternate_screen(&mut self) -> termwiz::Result<()> {
        termwiz::bail!("TermWizTerminalPane has no alt screen");
    }

    fn get_screen_size(&mut self) -> termwiz::Result<ScreenSize> {
        Ok(self.render_tx.screen_size)
    }

    fn set_screen_size(&mut self, _size: ScreenSize) -> termwiz::Result<()> {
        termwiz::bail!("TermWizTerminalPane cannot set screen size");
    }

    fn render(&mut self, changes: &[Change]) -> termwiz::Result<()> {
        self.renderer.render_to(changes, &mut self.render_tx)?;
        Ok(())
    }

    fn flush(&mut self) -> termwiz::Result<()> {
        self.render_tx.render_tx.flush()?;
        Ok(())
    }

    fn poll_input(&mut self, wait: Option<Duration>) -> termwiz::Result<Option<InputEvent>> {
        self.do_input_poll(wait).map(|i| {
            if let Some(InputEvent::Resized { cols, rows }) = i.as_ref() {
                self.render_tx.screen_size.cols = *cols;
                self.render_tx.screen_size.rows = *rows;
            }
            match i {
                // Urgh, we get normalized-to-lowercase CTRL-c,
                // but eg: termwiz and other terminal input expect
                // to get CTRL-C instead.  Adjust for that here.
                Some(InputEvent::Key(KeyEvent {
                    key: KeyCode::Char(c),
                    modifiers: Modifiers::CTRL,
                })) if c.is_ascii_lowercase() => Some(InputEvent::Key(KeyEvent {
                    key: KeyCode::Char(c.to_ascii_uppercase()),
                    modifiers: Modifiers::CTRL,
                })),
                i @ _ => i,
            }
        })
    }

    fn waker(&self) -> TerminalWaker {
        TerminalWaker::noop()
    }
}

pub fn allocate(
    size: TerminalSize,
    config: Arc<dyn TerminalConfiguration + Send + Sync>,
) -> anyhow::Result<(TermWizTerminal, Arc<dyn Pane>)> {
    let render_pipe = Pipe::new().context(
        "failed to create render pipe for TermWiz terminal — check file descriptor limits",
    )?;

    let (input_tx, input_rx) = channel();

    let renderer = crate::terminfo_renderer::new_frankenterm_terminfo_renderer();

    let tw_term = TermWizTerminal {
        render_tx: TermWizTerminalRenderTty {
            render_tx: BufWriter::new(render_pipe.write),
            screen_size: ScreenSize {
                cols: size.cols as usize,
                rows: size.rows as usize,
                xpixel: size
                    .pixel_width
                    .checked_div(size.cols)
                    .map_or(0, |value| value as usize),
                ypixel: size
                    .pixel_height
                    .checked_div(size.rows)
                    .map_or(0, |value| value as usize),
            },
        },
        input_rx,
        renderer,
        grab_mouse: true,
    };

    let domain = termwiz_terminal_domain();
    let pane = TermWizTerminalPane::new(
        domain.domain_id(),
        size,
        input_tx,
        render_pipe.read,
        Some(config),
    )?;

    // Add the tab to the mux so that the output is processed
    let pane: Arc<dyn Pane> = Arc::new(pane);

    let mux = Mux::try_get()
        .ok_or_else(|| anyhow::anyhow!("cannot allocate TermWiz pane: no mux configured"))?;
    mux.add_domain(&domain)?;
    mux.add_pane(&pane)
        .context("failed to add TermWiz pane to mux — pane ID collision?")?;

    Ok((tw_term, pane))
}

struct TermWizCleanupDispatch {
    registration: Option<PaneRegistrationHandle>,
}

impl TermWizCleanupDispatch {
    fn new(registration: PaneRegistrationHandle) -> Self {
        Self {
            registration: Some(registration),
        }
    }

    fn execute(mut self) {
        if let Some(registration) = self.registration.take() {
            let _ = registration.retire_and_prune_if_current();
        }
    }
}

impl Drop for TermWizCleanupDispatch {
    fn drop(&mut self) {
        if let Some(registration) = self.registration.take() {
            let _ = registration.retire_and_prune_if_current();
        }
    }
}

struct TermWizRunCleanup {
    registration: Option<PaneRegistrationHandle>,
}

impl TermWizRunCleanup {
    fn new(registration: PaneRegistrationHandle) -> Self {
        Self {
            registration: Some(registration),
        }
    }

    fn schedule(&mut self) {
        let Some(registration) = self.registration.take() else {
            return;
        };
        let dispatch = TermWizCleanupDispatch::new(registration);
        if promise::spawn::is_scheduler_configured() {
            promise::spawn::spawn_into_main_thread(async move {
                dispatch.execute();
            })
            .detach();
        } else {
            dispatch.execute();
        }
    }
}

impl Drop for TermWizRunCleanup {
    fn drop(&mut self) {
        self.schedule();
    }
}

/// This function spawns a thread and constructs a GUI window with an
/// associated termwiz Terminal object to execute the provided function.
/// The function is expected to run in a loop to manage input and output
/// from the terminal window.
/// When it completes its loop it will fulfil a promise and yield
/// the return value from the function.
pub fn run<T: Send + 'static, F: Send + 'static + FnOnce(TermWizTerminal) -> anyhow::Result<T>>(
    size: TerminalSize,
    window_id: Option<WindowId>,
    f: F,
    term_config: Option<Arc<dyn TerminalConfiguration + Send + Sync>>,
) -> impl std::future::Future<Output = anyhow::Result<T>> {
    // An async function body would not run until first poll. Capture the exact
    // origin now so a global mux swap cannot retarget registration or cleanup.
    let origin_mux = Mux::try_get();
    async move {
        let origin_mux = origin_mux
            .ok_or_else(|| anyhow::anyhow!("cannot run TermWiz applet: no mux configured"))?;
        let render_pipe = Pipe::new().context(
            "failed to create render pipe for TermWiz terminal — check file descriptor limits",
        )?;
        let render_rx = render_pipe.read;
        let (input_tx, input_rx) = channel();

        let renderer = crate::terminfo_renderer::new_frankenterm_terminfo_renderer();

        let tw_term = TermWizTerminal {
            render_tx: TermWizTerminalRenderTty {
                render_tx: BufWriter::new(render_pipe.write),
                screen_size: ScreenSize {
                    cols: size.cols as usize,
                    rows: size.rows as usize,
                    xpixel: size
                        .pixel_width
                        .checked_div(size.cols)
                        .map_or(0, |value| value as usize),
                    ypixel: size
                        .pixel_height
                        .checked_div(size.rows)
                        .map_or(0, |value| value as usize),
                },
            },
            input_rx,
            renderer,
            grab_mouse: true,
        };

        async fn register_tab(
            mux: Arc<Mux>,
            input_tx: Sender<InputEvent>,
            render_rx: FileDescriptor,
            size: TerminalSize,
            window_id: Option<WindowId>,
            term_config: Option<Arc<dyn TerminalConfiguration + Send + Sync>>,
        ) -> anyhow::Result<TermWizRunCleanup> {
            let domain = termwiz_terminal_domain();
            let pane = TermWizTerminalPane::new(
                domain.domain_id(),
                size,
                input_tx,
                render_rx,
                term_config,
            )?;
            let pane: Arc<dyn Pane> = Arc::new(pane);

            mux.add_domain(&domain)?;

            let tab = Arc::new(Tab::new(&size));
            tab.assign_pane(&pane);

            let registration = mux
                .add_tab_and_active_pane(&tab)?
                .context("TermWiz pane publication did not retain exact registration authority")?;
            let cleanup = TermWizRunCleanup::new(registration);

            // Delay allocating a new window until after pane publication, so a
            // publication failure cannot leave a provisional empty window. If
            // a later topology step fails, cancel its builder rather than
            // publishing WindowCreated for an applet that never started.
            let mut window_builder = window_id
                .is_none()
                .then(|| mux.new_empty_window(None, None));
            let window_id =
                window_id.unwrap_or_else(|| **window_builder.as_ref().expect("new window builder"));
            if let Err(error) = mux.add_tab_to_window(&tab, window_id) {
                if let Some(builder) = window_builder.take() {
                    builder.cancel();
                }
                return Err(error);
            }

            let Some(mut window) = mux.get_window_mut(window_id) else {
                if let Some(builder) = window_builder.take() {
                    builder.cancel();
                }
                return Err(anyhow::anyhow!("invalid window id {}", window_id));
            };
            let tab_idx = window.len().saturating_sub(1);
            window.save_and_then_set_active(tab_idx);
            drop(window);

            // Publish a newly-created window only after its tab is fully
            // attached. Existing-window runs have no builder.
            drop(window_builder);
            Ok(cleanup)
        }

        let mut cleanup = promise::spawn::spawn_into_main_thread(async move {
            register_tab(
                origin_mux,
                input_tx,
                render_rx,
                size,
                window_id,
                term_config,
            )
            .await
        })
        .await?;

        let result = promise::spawn::spawn_into_new_thread(move || f(tw_term)).await;

        // Since we're typically called with an outstanding Activity token active,
        // the dead status of the tab will be ignored until after the activity
        // resolves.  In the case of SSH where (currently!) several prompts may
        // be shown in succession, we don't want to leave lingering dead windows
        // on the screen. Exact cleanup is also armed in Drop for cancellation,
        // panic, executor rejection, and scheduler-free headless execution.
        cleanup.schedule();

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mux;
    use crate::domain::LocalDomain;

    struct ScopedMux {
        prior: Option<Arc<Mux>>,
    }

    impl ScopedMux {
        fn install(mux: Arc<Mux>) -> Self {
            let prior = Mux::try_get();
            Mux::set_mux(&mux);
            Self { prior }
        }
    }

    impl Drop for ScopedMux {
        fn drop(&mut self) {
            if let Some(prior) = self.prior.take() {
                Mux::set_mux(&prior);
            } else {
                Mux::shutdown();
            }
        }
    }

    #[test]
    fn termwiz_domain_detach_is_explicitly_unsupported() {
        let domain = termwiz_terminal_domain();
        assert!(!domain.detachable());

        let err = domain
            .detach()
            .expect_err("termwiz domain detach should be unsupported");
        let err = err.to_string();
        assert!(err.contains("unsupported"), "{}", err);
        assert!(err.contains("TermWizTerminalDomain"), "{}", err);
    }

    #[test]
    fn mouse_coordinate_conversions_saturate() {
        assert_eq!(usize_to_u16_saturating(7), 7);
        assert_eq!(usize_to_u16_saturating((u16::MAX as usize) + 1), u16::MAX);
        assert_eq!(row_to_u16_saturating(-1), 0);
        assert_eq!(row_to_u16_saturating(i64::from(u16::MAX) + 1), u16::MAX);
    }

    #[test]
    fn allocate_registers_overlay_pane_with_termwiz_domain() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("termwiz-overlay-default").unwrap());
        let mux = Arc::new(Mux::new(Some(default_domain)));
        let _mux = ScopedMux::install(Arc::clone(&mux));

        let config: Arc<dyn TerminalConfiguration + Send + Sync> =
            Arc::new(config::TermConfig::new());
        let size = TerminalSize {
            rows: 3,
            cols: 7,
            pixel_width: 70,
            pixel_height: 30,
            dpi: 96,
        };

        let (_terminal, pane) = allocate(size, config).expect("allocate TermWiz test pane");
        let pane_domain = pane.domain_id();

        assert_eq!(pane_domain, termwiz_terminal_domain().domain_id());
        assert_eq!(
            mux.get_domain(pane_domain)
                .map(|domain| domain.domain_name().to_string()),
            Some("TermWizTerminalDomain".to_string()),
        );
        let slot_registration = pane
            .mux_registration_slot()
            .load()
            .expect("TermWiz publication must populate its pane-owned slot");
        let registry_registration = mux
            .capture_pane_registration(&pane)
            .expect("TermWiz publication must retain exact registry authority");
        assert!(
            slot_registration.same_registration(&registry_registration),
            "TermWiz slot and mux registry must expose one exact generation"
        );
    }

    #[test]
    fn dropping_unpolled_cleanup_dispatch_future_retires_exact_pane() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let default_domain: Arc<dyn Domain> =
            Arc::new(LocalDomain::new("termwiz-cleanup-default").unwrap());
        let mux = Arc::new(Mux::new(Some(default_domain)));
        let _mux = ScopedMux::install(Arc::clone(&mux));
        let config: Arc<dyn TerminalConfiguration + Send + Sync> =
            Arc::new(config::TermConfig::new());
        let size = TerminalSize {
            rows: 3,
            cols: 7,
            pixel_width: 70,
            pixel_height: 30,
            dpi: 96,
        };

        let (_terminal, pane) = allocate(size, config).expect("allocate TermWiz test pane");
        let pane_id = pane.pane_id();
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("TermWiz pane should be registered");
        let dispatch = TermWizCleanupDispatch::new(registration);
        let unpolled = async move {
            dispatch.execute();
        };

        drop(unpolled);

        assert!(
            mux.get_pane(pane_id).is_none(),
            "dropping a never-polled scheduled cleanup must execute its Drop fallback"
        );
    }
}
