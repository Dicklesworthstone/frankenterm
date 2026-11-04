use crate::termwiztermtab;
use crate::Mux;
use anyhow::{anyhow, bail, Context as _};
use config::configuration;
use crossbeam::channel::{bounded, unbounded, Receiver, Sender};
use finl_unicode::grapheme_clusters::Graphemes;
use frankenterm_term::TerminalSize;
use std::convert::TryFrom;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

/// The thread behind [`ConnectionUI::new_deferred`]: hold requests until the
/// window is due, then open it and forward everything to it.
fn run_deferred<F>(rx: Receiver<UIRequest>, delay: Duration, window_shown: Arc<AtomicBool>, open: F)
where
    F: FnOnce() -> ConnectionUI,
{
    let deadline = Instant::now()
        .checked_add(delay)
        .unwrap_or_else(Instant::now);
    let mut output: Vec<Vec<Change>> = Vec::new();
    // Sleeps under way: when each ends, its reason, and its caller's reply.
    let mut sleeps: Vec<(Instant, String, Sender<anyhow::Result<()>>)> = Vec::new();
    let first = loop {
        let now = Instant::now();
        sleeps.retain(|(end, _, respond)| {
            if *end > now {
                return true;
            }
            let _ = respond.send(Ok(()));
            false
        });
        let wake = sleeps
            .iter()
            .map(|(end, ..)| *end)
            .fold(deadline, std::cmp::min);
        // This takes what is already queued even with no time left, so a close
        // that arrived before this thread woke for the deadline opens no window.
        match rx.recv_timeout(wake.saturating_duration_since(now)) {
            Ok(UIRequest::Close) => return,
            Ok(UIRequest::Output(changes)) => {
                if !changes.is_empty() {
                    output.push(changes);
                }
            }
            // From when the sleep arrives: `now` predates the wait for it, and
            // a sleep timed from that ended early by as long as the wait.
            Ok(UIRequest::Sleep {
                reason,
                duration,
                respond,
            }) => match Instant::now().checked_add(duration) {
                Some(end) => sleeps.push((end, reason, respond)),
                None => {
                    let _ = respond.send(Err(anyhow!("sleep duration is too large: {duration:?}")));
                }
            },
            Ok(input @ UIRequest::Input { .. }) => break Some(input),
            Err(err) if err.is_timeout() => {
                if now >= deadline {
                    break None;
                }
            }
            // The owner went away without closing the UI. Open the window only
            // for something to read: a title alone would be a blank window.
            Err(_)
                if output
                    .iter()
                    .flatten()
                    .all(|change| matches!(change, Change::Title(_))) =>
            {
                return
            }
            Err(_) => break None,
        }
    };

    let ui = open();
    window_shown.store(ui.is_interactive(), Ordering::Release);
    for changes in output {
        ui.output(changes);
    }
    for (end, reason, respond) in sleeps {
        let duration = end.saturating_duration_since(Instant::now());
        let sleep = UIRequest::Sleep {
            reason,
            duration,
            respond,
        };
        if ui.tx.send(sleep).is_err() {
            return;
        }
    }
    if let Some(request) = first {
        if ui.tx.send(request).is_err() {
            return;
        }
    }
    // The scheduler refused the window, so this is a headless sink that cannot
    // prompt. Ending here makes the deferred UI report itself closed, so its
    // owner's next attempt opens another, as a headless UI's `is_interactive`
    // tells it to. Otherwise a retained prompt surface would fail every prompt
    // from now on. The sink still answers what was just forwarded.
    if !ui.is_interactive() {
        return;
    }
    // A window the operator closed refuses the next request, which ends this
    // thread, so the deferred UI then reports itself closed too.
    while let Ok(request) = rx.recv() {
        let close = matches!(request, UIRequest::Close);
        if ui.tx.send(request).is_err() || close {
            return;
        }
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
    window_shown: Arc<AtomicBool>,
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
            window_shown: Arc::new(AtomicBool::new(true)),
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
            window_shown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A connection UI whose window opens only once `delay` has passed, or
    /// at once when a prompt needs the operator. Closed before then, it
    /// never shows a window, so a connect or reconnect that completes
    /// quickly (a lossless mux upgrade reconnects a few seconds in) does not
    /// flash one. Output sent while it waits is shown when the window opens,
    /// and a sleep under way then continues in the window. An owner that goes
    /// away without closing it, after sending text, gets the window opened to
    /// show that text, as an open window would have.
    pub fn new_deferred(delay: Duration) -> Self {
        Self::with_params_deferred(Default::default(), delay)
    }

    /// [`ConnectionUI::new_deferred`] with the window's params. Without a
    /// mux scheduler it is the same headless UI that
    /// [`ConnectionUI::with_params`] returns, so a caller that needs an
    /// interactive surface still sees `is_interactive()` false up front.
    pub fn with_params_deferred(params: ConnectionUIParams, delay: Duration) -> Self {
        if !promise::spawn::is_scheduler_configured() || Mux::try_get().is_none() {
            return Self::with_params(params);
        }
        Self::deferred_with_opener(delay, move || ConnectionUI::with_params(params))
    }

    fn deferred_with_opener<F>(delay: Duration, open: F) -> Self
    where
        F: FnOnce() -> ConnectionUI + Clone + Send + 'static,
    {
        let (tx, rx) = unbounded();
        let fallback = open.clone();
        let window_shown = Arc::new(AtomicBool::new(false));
        let shown = Arc::clone(&window_shown);
        let spawned = std::thread::Builder::new()
            .name("connection-ui-deferred".to_string())
            .spawn(move || run_deferred(rx, delay, shown, open));
        match spawned {
            Ok(_) => Self {
                tx,
                // Once the window is open, its replies come from the GUI's
                // main thread, so blocking requests must not come from it.
                response_requires_main_thread: true,
                window_shown,
            },
            Err(err) => {
                log::error!(
                    "failed to spawn the deferred ConnectionUI thread; opening its window now: {err:#}"
                );
                fallback()
            }
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

    /// Whether this UI has put a window in front of the operator. A deferred
    /// UI closed before its delay never has, and neither has a headless one.
    #[must_use]
    pub fn showed_window(&self) -> bool {
        self.window_shown.load(Ordering::Acquire)
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
    use std::sync::atomic::AtomicUsize;

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
            window_shown: Arc::new(AtomicBool::new(true)),
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

    /// An opener for a deferred UI whose "window" is a channel the test
    /// reads, and a count of how many times it was opened.
    fn recording_opener() -> (
        impl FnOnce() -> ConnectionUI + Clone + Send + 'static,
        Receiver<UIRequest>,
        Arc<AtomicUsize>,
    ) {
        let (tx, rx) = unbounded();
        let opened = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&opened);
        let open = move || {
            count.fetch_add(1, Ordering::SeqCst);
            ConnectionUI {
                tx,
                response_requires_main_thread: true,
                window_shown: Arc::new(AtomicBool::new(true)),
            }
        };
        (open, rx, opened)
    }

    fn next_request(window: &Receiver<UIRequest>, what: &str) -> UIRequest {
        window
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|err| panic!("{}: {}", what, err))
    }

    fn output_text(request: UIRequest) -> String {
        match request {
            UIRequest::Output(changes) => format!("{:?}", changes),
            _ => panic!("expected output"),
        }
    }

    /// The opener is gone (its thread ended) with no further request sent.
    fn nothing_more_reaches(window: &Receiver<UIRequest>) -> bool {
        window
            .recv_timeout(Duration::from_secs(10))
            .err()
            .is_some_and(|err| err.is_disconnected())
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {}", what);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_deferred_ui_without_a_mux_is_the_headless_ui_with_params_returns() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // No mux, whatever an earlier test left behind.
        Mux::shutdown();
        let params = ConnectionUIParams {
            disable_close_delay: true,
            ..Default::default()
        };
        let deferred = ConnectionUI::with_params_deferred(params, Duration::from_secs(600));
        let direct = ConnectionUI::with_params(params);
        assert!(
            !deferred.is_interactive(),
            "a background attach must still see that no prompt surface is available"
        );
        assert_eq!(deferred.is_interactive(), direct.is_interactive());
        deferred.close();
        direct.close();
    }

    #[test]
    fn a_deferred_ui_closed_before_its_delay_never_opens_a_window() {
        let (open, window, opened) = recording_opener();
        let ui = ConnectionUI::deferred_with_opener(Duration::from_secs(600), open);
        ui.title("FrankenTerm: Reconnecting...");
        ui.output_str("Connect to the mux\n");
        // Let the deferred UI wait well past the sleep's length before the
        // sleep arrives: a sleep timed from before that wait ends at once.
        std::thread::sleep(Duration::from_millis(200));
        // On another thread, so a sleep handed to a window (which nothing
        // here answers) fails the test instead of hanging it.
        let sleeper = {
            let ui = ui.clone();
            std::thread::spawn(move || {
                let started = Instant::now();
                let result = ui.sleep_with_reason("reconnecting", Duration::from_millis(20));
                (result, started.elapsed())
            })
        };
        wait_until("the sleep to finish while the window is deferred", || {
            sleeper.is_finished()
        });
        let (result, slept) = sleeper.join().expect("the sleeper finishes");
        result.expect("a sleep completes while the window is deferred");
        assert!(
            slept >= Duration::from_millis(20),
            "the 20 ms sleep ended after {:?}",
            slept
        );
        ui.close();

        wait_until("the deferred UI to close", || !ui.is_open());
        assert_eq!(opened.load(Ordering::SeqCst), 0);
        assert!(!ui.showed_window());
        assert!(nothing_more_reaches(&window));
    }

    #[test]
    fn a_close_queued_by_the_deadline_still_opens_no_window() {
        let (open, window, opened) = recording_opener();
        let (tx, rx) = unbounded();
        tx.send(UIRequest::Close).expect("the receiver is here");
        let shown = Arc::new(AtomicBool::new(false));

        run_deferred(rx, Duration::ZERO, Arc::clone(&shown), open);

        assert_eq!(opened.load(Ordering::SeqCst), 0);
        assert!(!shown.load(Ordering::SeqCst));
        assert!(nothing_more_reaches(&window));
    }

    #[test]
    fn a_deferred_ui_opens_after_its_delay_with_the_output_it_held() {
        let (open, window, opened) = recording_opener();
        let ui = ConnectionUI::deferred_with_opener(Duration::from_millis(50), open);
        ui.output_str("first\n");
        ui.output_str("second\n");

        let held = next_request(&window, "the window opens after the delay");
        assert!(output_text(held).contains("first"));
        assert!(output_text(next_request(&window, "the second line")).contains("second"));
        assert_eq!(opened.load(Ordering::SeqCst), 1);
        assert!(ui.showed_window());

        ui.output_str("third\n");
        assert!(output_text(next_request(&window, "later output")).contains("third"));
        ui.close();
        assert!(matches!(
            next_request(&window, "the close"),
            UIRequest::Close
        ));
        wait_until("the deferred UI to close", || !ui.is_open());
    }

    #[test]
    fn a_sleep_under_way_when_the_window_opens_continues_in_it() {
        let (open, window, _opened) = recording_opener();
        // Long enough that the sleep arrives before the window opens, even on
        // a busy machine: only a sleep held until then has a remainder to move.
        let ui = ConnectionUI::deferred_with_opener(Duration::from_secs(2), open);
        let sleeper = {
            let ui = ui.clone();
            std::thread::spawn(move || ui.sleep_with_reason("reconnecting", Duration::from_secs(5)))
        };

        match next_request(&window, "the window opens during the sleep") {
            UIRequest::Sleep {
                reason,
                duration,
                respond,
            } => {
                assert_eq!(reason, "reconnecting");
                assert!(
                    duration < Duration::from_secs(5),
                    "only the rest of the sleep moves to the window: {:?}",
                    duration
                );
                respond
                    .send(Ok(()))
                    .expect("the sleeper waits for the window");
            }
            _ => panic!("expected the sleep"),
        }
        sleeper
            .join()
            .expect("the sleeper finishes")
            .expect("the window completes the sleep");
        ui.close();
    }

    #[test]
    fn a_prompt_opens_a_deferred_window_at_once() {
        let (open, window, opened) = recording_opener();
        let ui = ConnectionUI::deferred_with_opener(Duration::from_secs(600), open);
        ui.output_str("The host key changed\n");
        let asker = {
            let ui = ui.clone();
            std::thread::spawn(move || ui.input("Enter [y/n]> "))
        };

        let held = next_request(&window, "a prompt opens the window");
        assert!(output_text(held).contains("The host key changed"));
        match next_request(&window, "the prompt reaches the window") {
            UIRequest::Input {
                prompt,
                echo,
                respond,
            } => {
                assert_eq!(prompt, "Enter [y/n]> ");
                assert!(echo);
                respond
                    .send(Ok("y".to_string()))
                    .expect("the asker waits for the answer");
            }
            _ => panic!("expected the prompt"),
        }
        let answer = asker.join().expect("the asker finishes");
        assert_eq!(answer.expect("the prompt was answered"), "y");
        assert_eq!(opened.load(Ordering::SeqCst), 1);
        ui.close();
    }

    #[test]
    fn a_deferred_ui_dropped_with_output_opens_to_show_it() {
        let (open, window, opened) = recording_opener();
        let ui = ConnectionUI::deferred_with_opener(Duration::from_secs(600), open);
        ui.output_str("Giving up: the local mux has not come back\n");
        drop(ui);

        let held = next_request(&window, "the window opens to show the output");
        assert!(output_text(held).contains("Giving up"));
        assert_eq!(opened.load(Ordering::SeqCst), 1);
        assert!(nothing_more_reaches(&window));

        let (open, window, opened) = recording_opener();
        drop(ConnectionUI::deferred_with_opener(
            Duration::from_secs(600),
            open,
        ));
        assert!(nothing_more_reaches(&window));
        assert_eq!(opened.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_deferred_ui_dropped_with_only_its_title_opens_no_blank_window() {
        let (open, window, opened) = recording_opener();
        let ui = ConnectionUI::deferred_with_opener(Duration::from_secs(600), open);
        ui.title("FrankenTerm: Reconnecting...");
        drop(ui);

        assert!(nothing_more_reaches(&window));
        assert_eq!(opened.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_deferred_ui_whose_window_was_refused_closes_so_its_owner_can_retry() {
        // The scheduler refusing the window leaves `with_params` a headless UI.
        let ui = ConnectionUI::deferred_with_opener(
            Duration::from_secs(600),
            ConnectionUI::new_headless,
        );
        assert!(ui.is_interactive());

        let refused = ui.input("Enter [y/n]> ");

        assert!(
            refused.is_err(),
            "a headless sink cannot answer a prompt: {:?}",
            refused
        );
        wait_until("the refused UI to report itself closed", || !ui.is_open());
        assert!(!ui.showed_window());
    }

    #[test]
    fn a_deferred_ui_whose_window_was_closed_reports_itself_closed() {
        let (open, window, _opened) = recording_opener();
        let ui = ConnectionUI::deferred_with_opener(Duration::ZERO, open);
        ui.output_str("connecting\n");
        let _ = next_request(&window, "the window opens at once");
        // The operator closes the window.
        drop(window);

        wait_until("the deferred UI to see the closed window", || !ui.is_open());
    }
}
