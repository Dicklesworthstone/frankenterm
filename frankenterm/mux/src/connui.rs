use crate::termwiztermtab;
use crate::Mux;
use anyhow::{anyhow, bail, Context as _};
use config::configuration;
use crossbeam::channel::{bounded, unbounded, Receiver, Sender};
use finl_unicode::grapheme_clusters::Graphemes;
use frankenterm_term::TerminalSize;
use std::convert::TryFrom;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use termwiz::cell::{unicode_column_width, CellAttributes};
use termwiz::lineedit::*;
use termwiz::surface::{Change, Position};
use termwiz::terminal::*;

const MAX_COUNTDOWN_PROGRESS_WIDTH: usize = 4096;

#[derive(Default)]
struct PasswordPromptHost {
    history: BasicHistory,
}
impl LineEditorHost for PasswordPromptHost {
    fn history(&mut self) -> &mut dyn History {
        &mut self.history
    }

    // Rewrite the input so that we can obscure the password
    // characters when output to the terminal widget
    fn highlight_line(&self, line: &str, cursor_position: usize) -> (Vec<OutputElement>, usize) {
        let placeholder = "🔑";
        let grapheme_count = unicode_column_width(line, None);
        let mut output = vec![];
        for _ in 0..grapheme_count {
            output.push(OutputElement::Text(placeholder.to_string()));
        }
        (
            output,
            unicode_column_width(placeholder, None).saturating_mul(cursor_position),
        )
    }
}

pub enum UIRequest {
    /// Display something
    Output(Vec<Change>),
    /// Request input
    Input {
        prompt: String,
        echo: bool,
        respond: Sender<anyhow::Result<String>>,
    },
    /// Sleep with a progress bar
    Sleep {
        reason: String,
        duration: Duration,
        respond: Sender<anyhow::Result<()>>,
    },
    Close,
}

struct ConnectionUIImpl {
    term: termwiztermtab::TermWizTerminal,
    rx: Receiver<UIRequest>,
}

#[derive(Debug, PartialEq, Eq)]
enum CloseStatus {
    Explicit,
    Implicit,
}

impl ConnectionUIImpl {
    fn run(&mut self) -> anyhow::Result<CloseStatus> {
        let poll_timeout = Duration::from_millis(configuration().connui_poll_timeout_ms);
        loop {
            match self.rx.recv_timeout(poll_timeout) {
                Ok(UIRequest::Close) => return Ok(CloseStatus::Explicit),
                Ok(UIRequest::Output(changes)) => self.term.render(&changes)?,
                Ok(UIRequest::Input {
                    prompt,
                    echo: true,
                    respond,
                }) => {
                    let _ = respond.send(self.input_prompt(&prompt));
                }
                Ok(UIRequest::Input {
                    prompt,
                    echo: false,
                    respond,
                }) => {
                    let _ = respond.send(self.password_prompt(&prompt));
                }
                Ok(UIRequest::Sleep {
                    reason,
                    duration,
                    respond,
                }) => {
                    let _ = respond.send(self.sleep(&reason, duration));
                }
                Err(err) if err.is_timeout() => {}
                Err(err) => bail!("recv_timeout: {}", err),
            }
        }
    }

    fn password_prompt(&mut self, prompt: &str) -> anyhow::Result<String> {
        let mut editor = LineEditor::new(&mut self.term);
        editor.set_prompt(prompt);

        let mut host = PasswordPromptHost::default();
        if let Some(line) = editor.read_line(&mut host)? {
            Ok(line)
        } else {
            bail!("password entry was cancelled");
        }
    }

    fn input_prompt(&mut self, prompt: &str) -> anyhow::Result<String> {
        let mut editor = LineEditor::new(&mut self.term);
        editor.set_prompt(prompt);

        let mut host = NopLineEditorHost::default();
        if let Some(line) = editor.read_line(&mut host)? {
            Ok(line)
        } else {
            bail!("prompt cancelled");
        }
    }

    fn sleep(&mut self, reason: &str, duration: Duration) -> anyhow::Result<()> {
        if duration.is_zero() {
            return Ok(());
        }

        let start = Instant::now();
        let deadline = start
            .checked_add(duration)
            .ok_or_else(|| anyhow!("sleep duration is too large: {duration:?}"))?;
        let mut last_draw = None;
        let duration_nanos = duration.as_nanos();

        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }

            // Render a progress bar underneath the countdown text by reversing
            // out the text for the elapsed portion of time.
            let remain = deadline - now;
            let term_width = bounded_countdown_progress_width(
                self.term.get_screen_size().map(|s| s.cols).unwrap_or(80),
            );
            let elapsed_nanos = duration.saturating_sub(remain).as_nanos();
            let prog_width = scaled_progress_width(term_width, elapsed_nanos, duration_nanos);
            let message = format!("{} ({:.0?})", reason, remain);

            let mut reversed_string = String::new();
            let mut default_string = String::new();
            let mut col = 0;
            for grapheme in Graphemes::new(&message) {
                // Once we've passed the elapsed column, full up the string
                // that we'll render with default attributes instead.
                if col > prog_width {
                    default_string.push_str(grapheme);
                } else {
                    reversed_string.push_str(grapheme);
                }
                col += 1;
            }

            // If we didn't reach the elapsed column yet (really short text!),
            // we need to pad out the reversed string.
            while col < prog_width {
                reversed_string.push(' ');
                col += 1;
            }

            let combined = format!("{}{}", reversed_string, default_string);

            if last_draw.as_ref() != Some(&combined) {
                self.term.render(&[
                    Change::CursorPosition {
                        x: Position::Absolute(0),
                        y: Position::Relative(0),
                    },
                    Change::AllAttributes(CellAttributes::default().set_reverse(true).clone()),
                    Change::Text(reversed_string),
                    Change::AllAttributes(CellAttributes::default()),
                    Change::Text(default_string),
                ])?;
                last_draw.replace(combined);
            }

            // We use poll_input rather than a raw sleep here so that
            // eg: resize events can be processed and reflected in the
            // dimensions reported at the top of the loop.
            // We're using a sub-second value for the delay here for a
            // slightly smoother progress bar.
            self.term
                .poll_input(Some(remain.min(Duration::from_millis(50))))?;
        }

        let message = format!("{} (done)\r\n", reason);
        self.term.render(&[
            Change::CursorPosition {
                x: Position::Absolute(0),
                y: Position::Relative(0),
            },
            Change::Text(message),
        ])?;

        Ok(())
    }
}

fn bounded_countdown_progress_width(term_width: usize) -> usize {
    term_width.min(MAX_COUNTDOWN_PROGRESS_WIDTH)
}

fn scaled_progress_width(term_width: usize, elapsed_nanos: u128, duration_nanos: u128) -> usize {
    if term_width == 0 || duration_nanos == 0 {
        return 0;
    }

    if elapsed_nanos >= duration_nanos {
        return term_width;
    }

    if let Some(scaled) = (term_width as u128).checked_mul(elapsed_nanos) {
        return usize::try_from(scaled / duration_nanos)
            .unwrap_or(usize::MAX)
            .min(term_width);
    }

    let ratio = (elapsed_nanos as f64 / duration_nanos as f64).clamp(0.0, 1.0);
    (((term_width as f64) * ratio).floor() as usize).min(term_width)
}

struct HeadlessImpl {
    rx: Receiver<UIRequest>,
}

impl HeadlessImpl {
    fn run(&mut self) -> anyhow::Result<()> {
        let poll_timeout = Duration::from_millis(configuration().connui_poll_timeout_ms);
        loop {
            match self.rx.recv_timeout(poll_timeout) {
                Ok(UIRequest::Close) => break,
                Ok(UIRequest::Output(changes)) => {
                    log::trace!("Output: {:?}", changes);
                }
                Ok(UIRequest::Input { respond, .. }) => {
                    let _ = respond.send(Err(anyhow!("Input requested from headless context")));
                }
                Ok(UIRequest::Sleep {
                    respond,
                    reason,
                    duration,
                }) => {
                    log::error!("{} (sleeping for {:?})", reason, duration);
                    std::thread::sleep(duration);
                    let _ = respond.send(Ok(()));
                }
                Err(err) if err.is_timeout() => {}
                Err(err) => bail!("recv_timeout: {}", err),
            }
        }

        Ok(())
    }
}

#[derive(Default, Clone, Copy, Debug)]
pub struct ConnectionUIParams {
    pub size: TerminalSize,
    pub disable_close_delay: bool,
    pub window_id: Option<crate::WindowId>,
}

#[derive(Clone)]
pub struct ConnectionUI {
    tx: Sender<UIRequest>,
    response_requires_main_thread: bool,
}

impl ConnectionUI {
    pub fn new() -> Self {
        Self::with_params(Default::default())
    }

    pub fn with_params(params: ConnectionUIParams) -> Self {
        if !promise::spawn::is_scheduler_configured() || Mux::try_get().is_none() {
            log::warn!(
                "ConnectionUI requested without an active mux scheduler; falling back to headless UI"
            );
            return Self::new_headless();
        }

        let reservation = match promise::spawn::try_reserve_main_thread(
            promise::spawn::MainThreadServiceClass::Background,
            32 * 1024,
        ) {
            promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
            rejected => {
                log::error!(
                    "main-thread scheduler rejected connection UI before channel construction; falling back to headless UI: {rejected:?}"
                );
                return Self::new_headless();
            }
        };

        let (tx, rx) = unbounded();
        reservation
            .handoff_to_main_thread_local(move |reservation| {
                // Reconnect UI creation also runs on the client reader thread.
                // Construct the thread-affine terminal future on its owner,
                // retaining the admission already reserved by that caller.
                reservation
                    .spawn_local(async move {
                        if let Err(err) = termwiztermtab::run(
                            params.size,
                            params.window_id,
                            move |term| {
                                let mut ui = ConnectionUIImpl { term, rx };
                                let status = ui.run().unwrap_or_else(|e| {
                                    log::error!("while running ConnectionUI loop: {:?}", e);
                                    CloseStatus::Implicit
                                });

                                if !params.disable_close_delay && status == CloseStatus::Implicit {
                                    ui.sleep(
                                        "(this window will close automatically)",
                                        Duration::new(120, 0),
                                    )
                                    .ok();
                                }
                                Ok(())
                            },
                            None,
                        )
                        .await
                        {
                            log::error!("connection UI task failed after admission: {err:#}");
                        }
                    })
                    .detach();
            })
            .detach();
        Self {
            tx,
            response_requires_main_thread: true,
        }
    }

    pub fn new_with_no_close_delay() -> Self {
        Self::with_params(ConnectionUIParams {
            disable_close_delay: true,
            ..Default::default()
        })
    }

    pub fn new_headless() -> Self {
        let (tx, rx) = unbounded();
        let spawn_result = std::thread::Builder::new()
            .name("connection-ui-headless".to_string())
            .spawn(move || {
                let mut ui = HeadlessImpl { rx };
                ui.run()
            });

        if let Err(err) = spawn_result {
            log::error!("failed to spawn headless ConnectionUI thread: {err:#}");
        }

        Self {
            tx,
            response_requires_main_thread: false,
        }
    }

    pub fn run_and_log_error<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce() -> anyhow::Result<T>,
    {
        match f() {
            Err(e) => {
                let what = format!("\r\nFailed: {:?}\r\n", e);
                log::error!("{}", what);
                self.output_str(&what);
                Err(e)
            }
            result => result,
        }
    }

    pub async fn async_run_and_log_error<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        F: std::future::Future<Output = anyhow::Result<T>>,
    {
        match f.await {
            Err(e) => {
                let what = format!("\r\nFailed: {:?}\r\n", e);
                self.output_str(&what);
                Err(e)
            }
            result => result,
        }
    }

    pub fn title(&self, title: &str) {
        self.output(vec![Change::Title(title.to_string())]);
    }

    pub fn output(&self, changes: Vec<Change>) {
        self.tx.send(UIRequest::Output(changes)).ok();
    }

    pub fn output_str(&self, s: &str) {
        let s = s.replace("\n", "\r\n");
        self.output(vec![Change::Text(s)]);
    }

    fn ensure_blocking_response_is_safe(&self, operation: &str) -> anyhow::Result<()> {
        let on_mux_main_thread = Mux::try_get().is_some_and(|mux| mux.is_main_thread());
        if self.response_requires_main_thread
            && (promise::spawn::is_in_main_thread_dispatch() || on_mux_main_thread)
        {
            bail!(
                "ConnectionUI::{operation} cannot block on the mux main thread: \
                 the GUI must service the response"
            );
        }
        Ok(())
    }

    /// Sleep (blocking!) for the specified duration, but updates
    /// the UI with the reason and a count down during that time.
    ///
    /// The reply deliberately uses a blocking channel rather than
    /// `promise::spawn::block_on`. Reconnect workers spend their backoff here;
    /// entering the shared asupersync runtime once per disconnected domain
    /// turns an otherwise idle retry fleet into a yield-heavy CPU loop.
    pub fn sleep_with_reason(&self, reason: &str, duration: Duration) -> anyhow::Result<()> {
        self.ensure_blocking_response_is_safe("sleep_with_reason")?;
        let (respond, response) = bounded(1);

        self.tx
            .send(UIRequest::Sleep {
                reason: reason.to_string(),
                duration,
                respond,
            })
            .context("send to ConnectionUI failed")?;

        response
            .recv()
            .context("ConnectionUI closed before completing sleep")?
    }

    /// Crack a multi-line prompt into an optional preamble and the prompt
    /// text on the final line.  This is needed because the line editor
    /// is only designed for a single line prompt; a multi-line prompt
    /// messes up the cursor positioning.
    fn split_multi_line_prompt(s: &str) -> (Option<String>, String) {
        let text = s.replace("\n", "\r\n");
        let bits: Vec<&str> = text.rsplitn(2, "\r\n").collect();

        if bits.len() == 2 {
            (Some(format!("{}\r\n", bits[1])), bits[0].to_owned())
        } else {
            (None, text)
        }
    }

    pub fn input(&self, prompt: &str) -> anyhow::Result<String> {
        self.ensure_blocking_response_is_safe("input")?;
        let (respond, response) = bounded(1);

        let (preamble, prompt) = Self::split_multi_line_prompt(prompt);
        if let Some(preamble) = preamble {
            self.output(vec![Change::Text(preamble)]);
        }

        self.tx
            .send(UIRequest::Input {
                prompt,
                echo: true,
                respond,
            })
            .context("send to ConnectionUI failed")?;

        response
            .recv()
            .context("ConnectionUI closed before completing input")?
    }

    pub fn password(&self, prompt: &str) -> anyhow::Result<String> {
        self.ensure_blocking_response_is_safe("password")?;
        let (respond, response) = bounded(1);

        let (preamble, prompt) = Self::split_multi_line_prompt(prompt);
        if let Some(preamble) = preamble {
            self.output(vec![Change::Text(preamble)]);
        }

        self.tx
            .send(UIRequest::Input {
                prompt,
                echo: false,
                respond,
            })
            .context("send to ConnectionUI failed")?;

        response
            .recv()
            .context("ConnectionUI closed before completing password input")?
    }

    pub fn close(&self) {
        self.tx.send(UIRequest::Close).ok();
    }

    /// Return whether the request receiver still owns this exact UI channel.
    ///
    /// The empty output is deliberately nonblocking. Background connection
    /// supervisors use this to replace a user-closed prompt surface on the
    /// next retry without creating more than one live window per domain.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.tx.send(UIRequest::Output(Vec::new())).is_ok()
    }

    /// Whether this UI can service operator input rather than rejecting it as
    /// a headless diagnostic sink.
    #[must_use]
    pub fn is_interactive(&self) -> bool {
        self.response_requires_main_thread
    }

    pub fn test_alive(&self) -> bool {
        if !self.tx.send(UIRequest::Output(vec![])).is_ok() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
        self.tx.send(UIRequest::Output(vec![])).is_ok()
    }
}

lazy_static::lazy_static! {
    static ref ERROR_WINDOW: Mutex<Option<ConnectionUI>> = Mutex::new(None);
}

fn get_error_window() -> ConnectionUI {
    let mut err = ERROR_WINDOW.lock().unwrap_or_else(|poisoned| {
        log::warn!("recovering poisoned configuration error window lock");
        poisoned.into_inner()
    });
    if let Some(ui) = err.as_ref().map(|ui| ui.clone()) {
        ui.output_str("\n");
        if ui.test_alive() {
            return ui;
        }
    }

    let ui = ConnectionUI::new_with_no_close_delay();
    ui.title("FrankenTerm Configuration Error");
    err.replace(ui.clone());
    ui
}

/// If the GUI has been started, pops up a window with the supplied error
/// message framed as a configuration error.
/// If there is no GUI front end, generates a toast notification instead.
pub fn show_configuration_error_message(err: &str) {
    log::error!("Configuration Error: {}", err);
    let ui = get_error_window();

    let mut wrapped = textwrap::fill(&err, 78);
    wrapped.push_str("\n");
    ui.output_str(&wrapped);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn reconnect_ui_created_on_reader_thread_runs_on_gui_owner() {
        let _guard = crate::MUX_TEST_LOCK.lock().unwrap();
        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let ui = ConnectionUI::with_params(ConnectionUIParams {
                size: TerminalSize {
                    rows: 24,
                    cols: 80,
                    ..Default::default()
                },
                disable_close_delay: true,
                window_id: None,
            });
            assert!(
                ui.response_requires_main_thread,
                "must exercise the GUI path"
            );
            let result = ui.sleep_with_reason("reconnect regression", Duration::ZERO);
            ui.close();
            reply_tx.send(result).unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            executor.try_tick().expect("poll GUI tasks on their owner");
            if let Ok(result) = reply_rx.try_recv() {
                result.expect("the real connection UI must service the reader's request");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "connection UI did not reply"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        reader.join().expect("reconnect reader must finish");
        while executor.try_tick().expect("drain GUI cleanup") {}
        Mux::shutdown();
    }

    #[test]
    fn split_multi_line_prompt_single_line() {
        let (preamble, prompt) = ConnectionUI::split_multi_line_prompt("Password: ");
        assert!(preamble.is_none());
        assert_eq!(prompt, "Password: ");
    }

    #[test]
    fn split_multi_line_prompt_two_lines() {
        let (preamble, prompt) = ConnectionUI::split_multi_line_prompt("Hello\nPassword: ");
        assert_eq!(preamble, Some("Hello\r\n".to_string()));
        assert_eq!(prompt, "Password: ");
    }

    #[test]
    fn split_multi_line_prompt_three_lines() {
        let (preamble, prompt) = ConnectionUI::split_multi_line_prompt("Line1\nLine2\nPassword: ");
        let pre = preamble.expect("multi-line prompt should produce a preamble");
        assert!(pre.contains("Line1"));
        assert!(pre.contains("Line2"));
        assert_eq!(prompt, "Password: ");
    }

    #[test]
    fn split_multi_line_prompt_empty_string() {
        let (preamble, prompt) = ConnectionUI::split_multi_line_prompt("");
        assert!(preamble.is_none());
        assert_eq!(prompt, "");
    }

    #[test]
    fn split_multi_line_prompt_trailing_newline() {
        let (preamble, prompt) = ConnectionUI::split_multi_line_prompt("Header\n");
        assert_eq!(preamble, Some("Header\r\n".to_string()));
        assert_eq!(prompt, "");
    }

    #[test]
    fn password_prompt_host_highlight_empty_line() {
        let host = PasswordPromptHost::default();
        let (output, cursor) = host.highlight_line("", 0);
        assert!(output.is_empty());
        assert_eq!(cursor, 0);
    }

    #[test]
    fn password_prompt_host_highlight_replaces_chars() {
        let host = PasswordPromptHost::default();
        let (output, _cursor) = host.highlight_line("abc", 0);
        // Each grapheme cluster gets replaced with the key emoji
        assert_eq!(output.len(), 3);
        for elem in &output {
            match elem {
                OutputElement::Text(t) => assert_eq!(t, "🔑"),
                _ => panic!("expected Text element"),
            }
        }
    }

    #[test]
    fn password_prompt_host_cursor_position_scales() {
        let host = PasswordPromptHost::default();
        let (_output, cursor_0) = host.highlight_line("abc", 0);
        let (_output, cursor_2) = host.highlight_line("abc", 2);
        assert_eq!(cursor_0, 0);
        assert!(cursor_2 > cursor_0, "cursor at pos 2 should be after pos 0");
    }

    #[test]
    fn password_prompt_host_cursor_position_saturates() {
        let host = PasswordPromptHost::default();
        let (_output, cursor) = host.highlight_line("abc", usize::MAX);
        assert_eq!(cursor, usize::MAX);
    }

    #[test]
    fn close_status_equality() {
        assert_eq!(CloseStatus::Explicit, CloseStatus::Explicit);
        assert_eq!(CloseStatus::Implicit, CloseStatus::Implicit);
        assert_ne!(CloseStatus::Explicit, CloseStatus::Implicit);
    }

    #[test]
    fn progress_width_scales_without_overflow() {
        assert_eq!(scaled_progress_width(80, 50, 100), 40);
        assert_eq!(scaled_progress_width(80, 100, 100), 80);
        assert_eq!(
            scaled_progress_width(usize::MAX, u128::MAX - 1, u128::MAX),
            usize::MAX
        );
    }

    #[test]
    fn countdown_progress_width_is_bounded() {
        assert_eq!(bounded_countdown_progress_width(80), 80);
        assert_eq!(
            bounded_countdown_progress_width(usize::MAX),
            MAX_COUNTDOWN_PROGRESS_WIDTH
        );
    }

    #[test]
    fn connection_ui_params_default() {
        let params = ConnectionUIParams::default();
        assert!(!params.disable_close_delay);
        assert!(params.window_id.is_none());
    }

    #[test]
    fn main_thread_serviced_blocking_reply_fails_before_enqueue() {
        let (tx, rx) = unbounded();
        let ui = ConnectionUI {
            tx,
            response_requires_main_thread: true,
        };
        let _dispatch = promise::spawn::enter_main_thread_dispatch_scope();

        let error = ui
            .sleep_with_reason("must not enqueue", Duration::ZERO)
            .expect_err("a GUI-serviced reply cannot block its own dispatcher");

        assert!(error.to_string().contains("mux main thread"));
        assert!(rx.is_empty(), "rejected work must not reach the GUI queue");
    }

    #[test]
    fn headless_blocking_sleep_does_not_depend_on_main_thread_progress() {
        let ui = ConnectionUI::new_headless();
        let _dispatch = promise::spawn::enter_main_thread_dispatch_scope();

        ui.sleep_with_reason("headless", Duration::ZERO)
            .expect("the headless worker owns its reply progress");
        ui.close();
    }
}
