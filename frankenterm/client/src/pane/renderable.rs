use crate::client::{RpcConsumerKind, RpcGenerationScope};
use crate::domain::ClientInner;
use codec::*;
use config::{configuration, ConfigHandle};
use futures::future::{select, Either};
use futures::pin_mut;
use futures::stream::{self, StreamExt};
use lru::LruCache;
use mux::pane::PaneId;
use mux::renderable::{PaneTieredScrollbackStatus, RenderableDimensions, StableCursorPosition};
use mux::{PaneRegistrationHandle, PaneRegistrationSlot};
use promise::BrokenPromise;
use rangeset::*;
use ratelim::RateLimiter;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::future::Future;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};
use termwiz::cell::{grapheme_column_width, Cell, CellAttributes, Underline};
use termwiz::color::AnsiColor;
use termwiz::hyperlink::Rule;
use termwiz::image::{ImageCell, ImageData, ImageDataValidationError, ImageDataValidationLimits};
use termwiz::surface::{SequenceNo, SEQ_ZERO};
use url::Url;
use wezterm_term::{KeyCode, KeyModifiers, Line, StableRowIndex};

fn max_poll_interval() -> Duration {
    Duration::from_millis(configuration().render_max_poll_interval_ms)
}

fn base_poll_interval() -> Duration {
    Duration::from_millis(configuration().render_base_poll_interval_ms)
}

fn initial_last_poll(now: Instant) -> Instant {
    now.checked_sub(base_poll_interval()).unwrap_or(now)
}

/// Prediction confidence score bounds + thresholds. The score rises when a
/// prediction is confirmed correct and falls (harder) when wrong. [3b/3c/3d]
const PREDICT_SCORE_MAX: i32 = 12;
const PREDICT_SCORE_MIN: i32 = -6;
/// At/above this score, predictions are confident: shown without the underline
/// uncertainty cue (glitchless). [3d]
const PREDICT_CONFIDENT_SCORE: i32 = 6;
/// At/below this score, stop predicting: the app is suppressing echo (raw-mode
/// readline, password prompt, etc.) so our guesses keep missing. [3c]
const PREDICT_SUPPRESS_SCORE: i32 = -4;
/// Once suppressed, stay suppressed at least this long after the most recent
/// misprediction before re-arming to re-test -- otherwise prediction would
/// re-fire on the very next keystroke and keep painting secret characters in an
/// echo-off prompt. [3c, review F1]
const PREDICT_SUPPRESS_COOLDOWN: Duration = Duration::from_secs(2);
/// Hard memory/work bound for speculative cell overlays on one pane.
///
/// Paste and key predictions are serial-correlated with a dispatch fence, but
/// an input stream can still outrun acknowledgement under disconnect or
/// extreme latency. Refuse further prediction instead of allowing pane-local
/// speculative state to grow without bound.
const MAX_PENDING_PREDICTIONS: usize = 4096;

/// Best-effort heuristic: does this line look like a secret prompt (password /
/// passphrase) where we must not predict/echo the typed-or-pasted secret? Not a
/// security boundary -- the confidence model also suppresses prediction after
/// repeated misses -- but it avoids painting the obvious cases. [review F8]
fn looks_like_secret_prompt(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("sword")        // password / passwd / [sudo] password
        || t.contains("passphrase")
        || t.contains("passcode")
        || t.contains("pin:") // "Enter PIN:" -- colon keeps false matches down
}

fn should_apply_unilateral_delta(current_seqno: SequenceNo, delta_seqno: SequenceNo) -> bool {
    delta_seqno >= current_seqno
}

fn render_line_cache_capacity(
    config: &ConfigHandle,
    dimensions: &RenderableDimensions,
) -> NonZeroUsize {
    render_line_cache_capacity_for_values(
        config.scrollback_lines,
        config.scrollback_tiered_enabled,
        config.scrollback_hot_lines,
        dimensions.viewport_rows,
    )
}

fn render_line_cache_capacity_for_values(
    scrollback_lines: usize,
    tiered_enabled: bool,
    scrollback_hot_lines: usize,
    viewport_rows: usize,
) -> NonZeroUsize {
    // Size the floor for the prefetch working set: the visible viewport plus ~one
    // viewport of read-ahead above and below (get_lines prefetches +/- span where
    // span == the viewport). Without this, a small-scrollback + tall-viewport
    // config could thrash, evicting just-rendered on-screen rows in favor of
    // speculative off-screen ones. Default (large scrollback) configs are
    // unaffected -- scrollback dominates the budget. [review 3f]
    let responsive_floor = viewport_rows.saturating_mul(3).max(128);
    let scrollback_budget = scrollback_lines.max(responsive_floor);
    let capacity = if tiered_enabled {
        scrollback_hot_lines
            .max(responsive_floor)
            .min(scrollback_budget)
    } else {
        scrollback_budget
    };
    NonZeroUsize::new(capacity).expect("render line cache capacity is clamped above zero")
}

#[derive(Debug)]
struct FetchIdentity {
    started_at: Instant,
}

#[derive(Clone, Debug)]
struct FetchToken(Arc<FetchIdentity>);

impl FetchToken {
    fn new(started_at: Instant) -> Self {
        Self(Arc::new(FetchIdentity { started_at }))
    }

    fn started_at(&self) -> Instant {
        self.0.started_at
    }

    fn same_request(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Debug)]
enum LineEntry {
    // Up to date wrt. server and has been rendered at least once
    Line(Line),
    // Currently being downloaded from the server
    Fetching(FetchToken),
    // We have a version of the line locally and are treating it
    // as needing rendering because we are also in the process of
    // downloading a newer version from the server
    LineAndFetching(Line, FetchToken),
    // We have a local copy but it is stale and will need to be
    // fetched again
    Stale(Line),
}

impl LineEntry {
    fn kind(&self) -> (&'static str, Option<Instant>) {
        match self {
            Self::Line(_) => ("Line", None),
            Self::Fetching(token) => ("Fetching", Some(token.started_at())),
            Self::LineAndFetching(_, token) => ("LineAndFetching", Some(token.started_at())),
            Self::Stale(_) => ("Stale", None),
        }
    }
}

fn fresh_cached_line(entry: Option<&LineEntry>) -> Option<&Line> {
    match entry {
        Some(LineEntry::Line(line)) => Some(line),
        Some(LineEntry::Fetching(_) | LineEntry::LineAndFetching(_, _) | LineEntry::Stale(_))
        | None => None,
    }
}

fn hyperlink_rules_equal(left: &[Rule], right: &[Rule]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.highlight == right.highlight
                && left.format == right.format
                && left.regex.as_str() == right.regex.as_str()
        })
}

fn rebuild_cache_as_stale(lines: &mut LruCache<StableRowIndex, LineEntry>, capacity: NonZeroUsize) {
    let mut stale_lines = LruCache::new(capacity);
    while let Some((stable_row, entry)) = lines.pop_lru() {
        let entry = match entry {
            LineEntry::Stale(old) | LineEntry::Line(old) | LineEntry::LineAndFetching(old, _) => {
                Some(LineEntry::Stale(old))
            }
            // A geometry epoch change invalidates the request identity. A late
            // old-width completion must find no matching token and be dropped.
            LineEntry::Fetching(_) => None,
        };
        if let Some(entry) = entry {
            stale_lines.put(stable_row, entry);
        }
    }
    *lines = stale_lines;
}

/// A single predicted cell, kept as an overlay record instead of being baked into
/// the cached server `Line`. Storing predictions separately is what lets us
/// validate each against the authoritative server cell (to drive prediction
/// confidence), rewind a wrong one for free (the overlay simply stops painting,
/// revealing the authoritative cached content), and resolve the underline cue
/// from the pane's confidence at render time.
#[derive(Debug, Clone)]
struct Prediction {
    row: StableRowIndex,
    col: usize,
    /// The plain predicted glyph (no underline); the underline cue is applied at
    /// render time only while prediction confidence is low.
    predicted: Cell,
    /// Serial of the keystroke that produced this prediction.
    input_serial: InputSerial,
    /// Terminal sequence sampled by the server after dispatching this input.
    ///
    /// The forced response carrying `input_serial` is only a protocol-dispatch
    /// acknowledgement. It does not prove that the PTY or application echoed the
    /// input. A later authoritative delta must advance beyond this fence before
    /// its cells can confirm or reject the prediction.
    dispatch_seqno: Option<SequenceNo>,
    born: Instant,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PredictionReconciliation {
    confirmed: usize,
    rejected: usize,
}

fn mark_predictions_dispatched(
    predictions: &mut [Prediction],
    serial: InputSerial,
    dispatch_seqno: SequenceNo,
) {
    for prediction in predictions {
        if prediction.input_serial <= serial && prediction.dispatch_seqno.is_none() {
            prediction.dispatch_seqno = Some(dispatch_seqno);
        }
    }
}

fn reconcile_predictions_after_terminal_change(
    predictions: &mut Vec<Prediction>,
    terminal_seqno: SequenceNo,
    bonus_lines: &[(StableRowIndex, Line)],
) -> PredictionReconciliation {
    reconcile_predictions_with_authoritative_lines(predictions, terminal_seqno, |row| {
        bonus_lines
            .iter()
            .find_map(|(candidate, line)| (*candidate == row).then_some(line))
    })
}

fn reconcile_predictions_after_cached_terminal_change(
    predictions: &mut Vec<Prediction>,
    terminal_seqno: SequenceNo,
    lines: &LruCache<StableRowIndex, LineEntry>,
) -> PredictionReconciliation {
    reconcile_predictions_with_authoritative_lines(predictions, terminal_seqno, |row| {
        match lines.peek(&row) {
            Some(LineEntry::Line(line)) => Some(line),
            Some(LineEntry::Fetching(_) | LineEntry::LineAndFetching(..) | LineEntry::Stale(_))
            | None => None,
        }
    })
}

fn reconcile_predictions_with_authoritative_lines<'a>(
    predictions: &mut Vec<Prediction>,
    terminal_seqno: SequenceNo,
    mut line_for_row: impl FnMut(StableRowIndex) -> Option<&'a Line>,
) -> PredictionReconciliation {
    let mut reconciliation = PredictionReconciliation::default();
    let mut pending = Vec::with_capacity(predictions.len());

    for prediction in std::mem::take(predictions) {
        let Some(dispatch_seqno) = prediction.dispatch_seqno else {
            pending.push(prediction);
            continue;
        };
        if terminal_seqno <= dispatch_seqno {
            pending.push(prediction);
            continue;
        }

        let Some(line) = line_for_row(prediction.row) else {
            // The terminal advanced, but this delta does not carry the predicted
            // row. Retaining the overlay is safer than treating missing evidence
            // as either confirmation or rejection; bounded expiry handles a row
            // that never becomes observable.
            pending.push(prediction);
            continue;
        };
        if line.current_seqno() <= dispatch_seqno {
            // A later snapshot can carry an unchanged line whose contents predate
            // the input. Matching that line would be correlation, not evidence.
            pending.push(prediction);
            continue;
        }

        let matches = match line.get_cell(prediction.col) {
            Some(cell) => cell.str() == prediction.predicted.str(),
            None => prediction.predicted.str() == " ",
        };
        if matches {
            reconciliation.confirmed += 1;
        } else {
            reconciliation.rejected += 1;
        }
    }

    *predictions = pending;
    reconciliation
}

fn expire_predictions(predictions: &mut Vec<Prediction>, now: Instant, ttl: Duration) -> usize {
    let before = predictions.len();
    predictions.retain(|prediction| now.saturating_duration_since(prediction.born) < ttl);
    before.saturating_sub(predictions.len())
}

fn reset_prediction_state(predictions: &mut Vec<Prediction>, prediction_score: &mut i32) {
    predictions.clear();
    *prediction_score = 0;
}

fn push_bounded_prediction(predictions: &mut Vec<Prediction>, prediction: Prediction) -> bool {
    if predictions.len() >= MAX_PENDING_PREDICTIONS {
        return false;
    }
    predictions.push(prediction);
    true
}

fn paste_fits_prediction_budget(predictions_len: usize, text: &str) -> bool {
    let remaining = MAX_PENDING_PREDICTIONS.saturating_sub(predictions_len);
    text.chars().take(remaining.saturating_add(1)).count() <= remaining
}

fn apply_prediction_reconciliation_to_score(
    prediction_score: &mut i32,
    last_prediction_miss: &mut Instant,
    reconciliation: PredictionReconciliation,
    now: Instant,
) {
    if reconciliation.confirmed > 0 {
        let reward = i32::try_from(reconciliation.confirmed).unwrap_or(i32::MAX);
        *prediction_score = prediction_score
            .saturating_add(reward)
            .min(PREDICT_SCORE_MAX);
    }

    if reconciliation.rejected > 0 {
        let rejected = i32::try_from(reconciliation.rejected).unwrap_or(i32::MAX);
        let penalty = rejected.saturating_mul(2);
        // A miss drops the score AND forces it below the confident threshold,
        // so secret characters in an echo-off prompt never render plain even
        // if the pane was confident a moment ago. [F2]
        *prediction_score = prediction_score
            .saturating_sub(penalty)
            .clamp(PREDICT_SCORE_MIN, PREDICT_CONFIDENT_SCORE - 1);
        *last_prediction_miss = now;
    }
}

pub struct RenderableInner {
    pub client: Arc<ClientInner>,
    remote_pane_id: PaneId,
    local_pane_id: PaneId,
    mux_registration: Arc<PaneRegistrationSlot>,
    renderable: Weak<parking_lot::Mutex<RenderableState>>,
    last_poll: Instant,
    pub dead: bool,
    poll_in_progress: AtomicBool,
    poll_interval: Duration,

    cursor_position: StableCursorPosition,
    pub dimensions: RenderableDimensions,
    pub tiered_scrollback_status: Option<PaneTieredScrollbackStatus>,

    lines: LruCache<StableRowIndex, LineEntry>,
    pub title: String,
    pub working_dir: Option<Url>,
    pub seqno: SequenceNo,

    /// Exact rules used for the persisted implicit links in `lines`. GUI
    /// windows can supply different rule sets for the same remote pane, so this
    /// cannot be inferred from process-global configuration. Comparing these
    /// borrowed fields avoids allocating a signature on every paint.
    implicit_hyperlink_rules: Vec<Rule>,

    fetch_limiter: RateLimiter,

    last_send_time: Instant,
    pub last_recv_time: Instant,
    last_late_dirty: Instant,
    last_input_rtt: u64,

    pub input_serial: InputSerial,

    /// Active speculative cell predictions (mosh-grade local echo), kept as an
    /// overlay applied at render time rather than mutated into the cached lines.
    predictions: Vec<Prediction>,
    /// Whether the pane is currently showing the alternate screen (vim/TUI). We
    /// never predict there: those keystrokes are commands, not echoed text. [3e]
    alt_screen_active: bool,
    /// Confidence score for predictive echo on this pane: bumped up when a
    /// prediction is confirmed correct by the server, down (harder) when wrong.
    /// High => show predictions without the underline cue (glitchless, 3d);
    /// very low => stop predicting (the app suppresses echo, 3c). [3b/3c/3d]
    prediction_score: i32,
    /// When the most recent misprediction happened, used to hold suppression for
    /// a cooldown so a password prompt stays suppressed instead of re-firing on
    /// the next keystroke. [3c, review F1]
    last_prediction_miss: Instant,
}

pub struct RenderableState {
    pub inner: RefCell<RenderableInner>,
}

pub(crate) struct RenderablePaneBinding {
    client: Arc<ClientInner>,
    remote_pane_id: PaneId,
    local_pane_id: PaneId,
    mux_registration: Arc<PaneRegistrationSlot>,
}

impl RenderablePaneBinding {
    pub(crate) fn new(
        client: &Arc<ClientInner>,
        remote_pane_id: PaneId,
        local_pane_id: PaneId,
        mux_registration: Arc<PaneRegistrationSlot>,
    ) -> Self {
        Self {
            client: Arc::clone(client),
            remote_pane_id,
            local_pane_id,
            mux_registration,
        }
    }
}

impl RenderableInner {
    pub(crate) fn new(
        binding: RenderablePaneBinding,
        dimensions: RenderableDimensions,
        title: &str,
        alt_screen_active: bool,
        fetch_limiter: RateLimiter,
        renderable: Weak<parking_lot::Mutex<RenderableState>>,
    ) -> Self {
        let now = Instant::now();
        let config = configuration();
        let line_cache_capacity = render_line_cache_capacity(&config, &dimensions);
        let RenderablePaneBinding {
            client,
            remote_pane_id,
            local_pane_id,
            mux_registration,
        } = binding;

        Self {
            client,
            remote_pane_id,
            local_pane_id,
            mux_registration,
            renderable,
            last_poll: initial_last_poll(now),
            dead: false,
            poll_in_progress: AtomicBool::new(false),
            poll_interval: base_poll_interval(),
            cursor_position: StableCursorPosition::default(),
            dimensions,
            tiered_scrollback_status: None,
            lines: LruCache::new(line_cache_capacity),
            title: title.to_string(),
            working_dir: None,
            fetch_limiter,
            last_send_time: now,
            last_recv_time: now,
            last_late_dirty: now,
            last_input_rtt: 0,
            input_serial: InputSerial::empty(),
            seqno: SEQ_ZERO,
            implicit_hyperlink_rules: config.hyperlink_rules.clone(),
            predictions: Vec::new(),
            alt_screen_active,
            prediction_score: 0,
            last_prediction_miss: now,
        }
    }

    pub fn registration_did_bind(&mut self) {
        self.poll_in_progress.store(false, Ordering::Release);

        let capacity = self.lines.cap();
        let mut stale_lines = LruCache::new(capacity);
        while let Some((stable_row, entry)) = self.lines.pop_lru() {
            match entry {
                LineEntry::Line(line)
                | LineEntry::Stale(line)
                | LineEntry::LineAndFetching(line, _) => {
                    stale_lines.put(stable_row, LineEntry::Stale(line));
                }
                LineEntry::Fetching(_) => {}
            }
        }
        self.lines = stale_lines;
    }

    /// Returns true if we think we should display the laggy connection
    /// indicator.  If we're past our poll interval and more recently
    /// tried to send something than receive something, the UI is worth
    /// showing.
    pub fn is_tardy(&self) -> bool {
        let elapsed = self.last_recv_time.elapsed();
        // Fixed threshold, decoupled from poll_interval: the liveness poll now sits
        // at the long backstop interval, but the "laggy connection" cue should still
        // appear ~3s after we sent something without hearing back (the value the old
        // `.max(3s)` floor produced right after input). [zero-poll]
        if elapsed > Duration::from_secs(3) {
            self.last_send_time > self.last_recv_time
        } else {
            false
        }
    }

    /// Predictive echo can be noisy when the link is working well,
    /// so we only employ it when it looks like the latency is high.
    fn should_predict(&mut self) -> bool {
        // A suppressed pane must eventually get a bounded opportunity to prove
        // that echo behavior has changed. This check belongs on the input path:
        // an echo-suppressing application may produce no render delta capable of
        // re-arming the old reconciliation-only path.
        if self.prediction_score <= PREDICT_SUPPRESS_SCORE
            && self.last_prediction_miss.elapsed() > PREDICT_SUPPRESS_COOLDOWN
        {
            self.prediction_score = PREDICT_SUPPRESS_SCORE + 1;
        }

        // Never predict into a full-screen / alt-screen app (vim, less, TUIs):
        // those keystrokes are editor commands, not echoed text. [3e]
        if self.alt_screen_active {
            return false;
        }
        // Stop predicting if recent predictions were consistently wrong -- the app
        // is suppressing echo (raw-mode readline, password prompt). [3c]
        if self.prediction_score <= PREDICT_SUPPRESS_SCORE {
            return false;
        }
        // Predict when the link is laggy enough for echo to matter. The configured
        // local_echo_threshold_ms is the activation point, honored as-is (the
        // default is now 20ms -- low enough to activate on ~25ms remote links;
        // explicit per-domain values are respected). `None` disables predictive
        // echo entirely. [3c, review F7]
        match self.client.local_echo_threshold_ms {
            Some(thresh) => self.last_input_rtt >= thresh,
            None => false,
        }
    }

    fn suppress_prediction_after_local_failure(&mut self) {
        self.prediction_score = PREDICT_SUPPRESS_SCORE;
        self.last_prediction_miss = Instant::now();
    }

    /// Compute a "prediction" and apply it to the line data that we
    /// have available, marking it as dirty so that it gets rendered.
    /// The prediction is basically just local echo.
    /// Open questions:
    /// how do we tell if the intent is to suppress local echo during eg:
    ///  * password prompt?  One option is to look back and see if the line
    ///    looks like a password prompt.
    ///  * normal mode in vim: letter presses are typically movement or
    ///    other editor commands
    ///
    /// There are bound to be a number of other edge cases that we should
    /// handle.
    /// Record an overlay prediction for the plain glyph `predicted` at (row, col).
    fn record_prediction(&mut self, row: StableRowIndex, col: usize, predicted: Cell) -> bool {
        let Some(end_col) = col.checked_add(predicted.width()) else {
            self.suppress_prediction_after_local_failure();
            return false;
        };
        if predicted.width() == 0 || col >= self.dimensions.cols || end_col > self.dimensions.cols {
            // A prediction is eventually painted with `Line::set_cell`, which
            // can materialize storage up to the requested column. Never let a
            // malformed cursor or a wide glyph past the right edge turn local
            // echo into an out-of-bounds allocation or an incorrect wrap.
            self.suppress_prediction_after_local_failure();
            return false;
        }
        let now = Instant::now();
        let admitted = push_bounded_prediction(
            &mut self.predictions,
            Prediction {
                row,
                col,
                predicted,
                input_serial: self.input_serial,
                dispatch_seqno: None,
                born: now,
            },
        );
        if !admitted {
            self.prediction_score = PREDICT_SUPPRESS_SCORE;
            self.last_prediction_miss = now;
        }
        admitted
    }

    /// Record the terminal sequence at which the server had dispatched every
    /// input through `serial`.
    ///
    /// This is intentionally not prediction settlement: `pane.key_down` can
    /// return before the PTY or application emits any output.
    fn record_input_dispatch(&mut self, serial: InputSerial, dispatch_seqno: SequenceNo) {
        mark_predictions_dispatched(&mut self.predictions, serial, dispatch_seqno);
    }

    fn apply_prediction_reconciliation(
        &mut self,
        reconciliation: PredictionReconciliation,
        now: Instant,
    ) {
        apply_prediction_reconciliation_to_score(
            &mut self.prediction_score,
            &mut self.last_prediction_miss,
            reconciliation,
            now,
        );
    }

    fn reconcile_predictions(
        &mut self,
        terminal_seqno: SequenceNo,
        bonus_lines: &[(StableRowIndex, Line)],
        now: Instant,
    ) {
        let reconciliation = reconcile_predictions_after_terminal_change(
            &mut self.predictions,
            terminal_seqno,
            bonus_lines,
        );
        self.apply_prediction_reconciliation(reconciliation, now);
    }

    fn reconcile_predictions_against_cached_state(&mut self, now: Instant) {
        let reconciliation = reconcile_predictions_after_cached_terminal_change(
            &mut self.predictions,
            self.seqno,
            &self.lines,
        );
        self.apply_prediction_reconciliation(reconciliation, now);
    }

    fn prediction_ttl(&self) -> Duration {
        Duration::from_millis(250).max(Duration::from_millis(self.last_input_rtt.saturating_mul(4)))
    }

    fn expire_stale_predictions(&mut self, now: Instant) -> RangeSet<StableRowIndex> {
        let ttl = self.prediction_ttl();
        let mut dirty_rows = RangeSet::new();
        for prediction in &self.predictions {
            if now.saturating_duration_since(prediction.born) >= ttl {
                dirty_rows.add(prediction.row);
            }
        }
        let expired = expire_predictions(&mut self.predictions, now, ttl);
        self.apply_prediction_reconciliation(
            PredictionReconciliation {
                confirmed: 0,
                rejected: expired,
            },
            now,
        );
        dirty_rows
    }

    fn apply_prediction(&mut self, c: KeyCode, line: &Line) {
        let text = line.as_str();
        if looks_like_secret_prompt(&text) {
            // This line might be a password/passphrase prompt. Don't predict, as we
            // don't want to reveal the secret the user is typing. [review F8]
            return;
        }

        match c {
            KeyCode::Enter => {
                let Some(next_row) = self.cursor_position.y.checked_add(1) else {
                    self.suppress_prediction_after_local_failure();
                    return;
                };
                self.cursor_position.x = 0;
                self.cursor_position.y = next_row;
            }
            KeyCode::UpArrow => {
                self.cursor_position.y = self.cursor_position.y.saturating_sub(1);
            }
            KeyCode::DownArrow => {
                let Some(next_row) = self.cursor_position.y.checked_add(1) else {
                    self.suppress_prediction_after_local_failure();
                    return;
                };
                self.cursor_position.y = next_row;
            }
            KeyCode::RightArrow => {
                let Some(next_col) = self.cursor_position.x.checked_add(1) else {
                    self.suppress_prediction_after_local_failure();
                    return;
                };
                if next_col >= self.dimensions.cols {
                    self.suppress_prediction_after_local_failure();
                    return;
                }
                self.cursor_position.x = next_col;
            }
            KeyCode::LeftArrow => {
                self.cursor_position.x = self.cursor_position.x.saturating_sub(1);
            }
            KeyCode::Delete => {
                let row = self.cursor_position.y;
                let col = self.cursor_position.x;
                let _ = self.record_prediction(row, col, Cell::new(' ', CellAttributes::default()));
            }
            KeyCode::Backspace if self.cursor_position.x > 0 => {
                let row = self.cursor_position.y;
                let col = self.cursor_position.x - 1;
                if self.record_prediction(row, col, Cell::new(' ', CellAttributes::default())) {
                    self.cursor_position.x -= 1;
                }
            }
            KeyCode::Char(c) => {
                let mut encoded = [0_u8; 4];
                let scalar_width = grapheme_column_width(c.encode_utf8(&mut encoded), None);
                // A control scalar or zero-width scalar is stateful: it may
                // move the cursor, alter terminal state, or combine with an
                // adjacent glyph. Check the original scalar before `Cell`
                // compact storage normalizes width zero to one.
                if c.is_control() || scalar_width == 0 {
                    self.suppress_prediction_after_local_failure();
                    return;
                }
                // Store the plain glyph; the underline uncertainty cue is applied at
                // render time only while confidence is low (glitchless, 3d).
                let cell = Cell::new(c, CellAttributes::default());
                let width = cell.width();
                let row = self.cursor_position.y;
                let col = self.cursor_position.x;
                let Some(next_col) = col.checked_add(width) else {
                    self.suppress_prediction_after_local_failure();
                    return;
                };
                // Reaching the final column sets terminal pending-wrap state,
                // which this overlay does not represent. Leave that edge case
                // to the authoritative stream rather than moving the synthetic
                // cursor one column beyond the viewport.
                if next_col >= self.dimensions.cols {
                    self.suppress_prediction_after_local_failure();
                    return;
                }
                if self.record_prediction(row, col, cell) {
                    // Adjust the cursor to reflect the width of this new cell
                    self.cursor_position.x = next_col;
                }
            }
            _ => {}
        }
    }

    /// Based on a keypress, apply a "prediction" of what the terminal
    /// content will look like once we receive the response from the
    /// remote system.  The prediction helps to reduce perceived latency
    /// when a user is typing at any reasonable velocity.
    pub fn predict_from_key_event(&mut self, key: KeyCode, mods: KeyModifiers) {
        if !self.should_predict() {
            return;
        }

        let c = match key {
            KeyCode::LeftArrow
            | KeyCode::RightArrow
            | KeyCode::UpArrow
            | KeyCode::DownArrow
            | KeyCode::Delete
            | KeyCode::Backspace
            | KeyCode::Enter
            | KeyCode::Char(_) => key,
            _ => return,
        };
        if mods != KeyModifiers::NONE && mods != KeyModifiers::SHIFT {
            return;
        }

        let row = self.cursor_position.y;
        // Read (clone) the current authoritative line without disturbing its cache
        // state; the prediction is recorded as an overlay rather than mutated in.
        let line = self.lines.peek(&row).and_then(|e| match e {
            LineEntry::Line(l) | LineEntry::Stale(l) | LineEntry::LineAndFetching(l, _) => {
                Some(l.clone())
            }
            LineEntry::Fetching(_) => None,
        });
        if let Some(line) = line {
            self.apply_prediction(c, &line);
        }
    }

    fn apply_paste_prediction(
        &mut self,
        row: StableRowIndex,
        starting_col: usize,
        text_line: &Line,
    ) -> bool {
        let Some(final_col) = text_line
            .visible_cells()
            .try_fold(starting_col, |col, cell| col.checked_add(cell.width()))
        else {
            self.suppress_prediction_after_local_failure();
            return false;
        };
        if final_col > self.dimensions.cols {
            self.suppress_prediction_after_local_failure();
            return false;
        }
        let original_prediction_len = self.predictions.len();

        let mut col = starting_col;
        for cell in text_line.visible_cells() {
            if !self.record_prediction(row, col, cell.as_cell()) {
                self.predictions.truncate(original_prediction_len);
                return false;
            }
            let Some(next_col) = col.checked_add(cell.width()) else {
                // The preflight above makes this unreachable unless cell width
                // semantics change between the two bounded iterations. Keep
                // the transaction fail-closed rather than relying on a panic.
                self.predictions.truncate(original_prediction_len);
                self.suppress_prediction_after_local_failure();
                return false;
            };
            col = next_col;
        }
        self.cursor_position.x = final_col;
        true
    }

    pub fn predict_from_paste(&mut self, text: &str) {
        if !self.should_predict() {
            return;
        }
        // Don't predict a paste into a password/passphrase prompt. [review F8]
        let cursor_row = self.cursor_position.y;
        let secret = matches!(
            self.lines.peek(&cursor_row),
            Some(LineEntry::Line(l) | LineEntry::Stale(l) | LineEntry::LineAndFetching(l, _))
                if looks_like_secret_prompt(&l.as_str())
        );
        if secret {
            return;
        }

        // Preflight before allocating line models or traversing an arbitrarily large
        // paste. A Unicode scalar can produce at most one prediction record
        // here, so fitting the scalar count into the remaining record budget guarantees
        // all-or-nothing admission. `take(remaining + 1)` keeps refusal work bounded
        // by the pane-local prediction ceiling rather than by the paste size.
        if !paste_fits_prediction_budget(self.predictions.len(), text) {
            self.suppress_prediction_after_local_failure();
            return;
        }
        // Tabs, carriage returns, and other controls depend on terminal/application
        // state that this speculative overlay does not own. Rendering them as glyphs
        // would be visibly wrong, so leave those pastes to the authoritative stream.
        if text.chars().any(|c| c != '\n' && c.is_control()) {
            self.suppress_prediction_after_local_failure();
            return;
        }

        // Preserve explicit line boundaries exactly. `textwrap::fill` performs word
        // wrapping, whereas a terminal hard-wraps by cell at the right margin; the
        // former can move spaces and words to different rows. Until the speculative
        // state carries the terminal's exact wrap mode, reject implicit wrapping
        // rather than painting a plausible-but-wrong layout.
        let attrs = CellAttributes::default();
        let lines: Vec<Line> = text
            .split('\n')
            .map(|line| Line::from_text(line, &attrs, SEQ_ZERO, None))
            .collect();
        let Some(final_cursor_row) = StableRowIndex::try_from(lines.len().saturating_sub(1))
            .ok()
            .and_then(|additional_rows| self.cursor_position.y.checked_add(additional_rows))
        else {
            self.suppress_prediction_after_local_failure();
            return;
        };

        let original_cursor = self.cursor_position;
        let original_prediction_len = self.predictions.len();
        let mut final_columns = Vec::with_capacity(lines.len());
        for (idx, line) in lines.iter().enumerate() {
            let starting_col = if idx == 0 { original_cursor.x } else { 0 };
            if line
                .visible_cells()
                .any(|cell| grapheme_column_width(cell.str(), None) == 0)
            {
                // A leading/non-composed combining cell depends on neighboring
                // terminal state. Inspect the original grapheme because compact
                // `Cell` storage intentionally normalizes width zero to one; do
                // not advance the synthetic cursor while omitting context that
                // this overlay cannot represent exactly.
                self.suppress_prediction_after_local_failure();
                return;
            }
            let Some(final_col) = line
                .visible_cells()
                .try_fold(starting_col, |col, cell| col.checked_add(cell.width()))
            else {
                self.suppress_prediction_after_local_failure();
                return;
            };
            if line.visible_cells().next().is_some() && final_col >= self.dimensions.cols {
                self.suppress_prediction_after_local_failure();
                return;
            }
            final_columns.push(final_col);
        }

        for (idx, (row, paste_line)) in (original_cursor.y..=final_cursor_row)
            .zip(lines.iter())
            .enumerate()
        {
            // Only predict for rows we already have cached; recorded as an overlay.
            let cached = matches!(
                self.lines.peek(&row),
                Some(LineEntry::Line(_) | LineEntry::Stale(_) | LineEntry::LineAndFetching(..))
            );
            let starting_col = if idx == 0 { original_cursor.x } else { 0 };
            if cached && !self.apply_paste_prediction(row, starting_col, paste_line) {
                self.predictions.truncate(original_prediction_len);
                self.cursor_position = original_cursor;
                return;
            }
        }
        self.cursor_position.y = final_cursor_row;
        self.cursor_position.x = final_columns.last().copied().unwrap_or(original_cursor.x);
    }

    pub fn update_last_send(&mut self) {
        self.last_send_time = Instant::now();
        // Deliberately does NOT re-pin poll_interval to base. The poll is now a slow
        // liveness backstop (see poll()); input echo and updates arrive via the
        // server push, and is_tardy() uses a fixed threshold — so re-pinning here
        // would only re-trigger the poll ramp on every keystroke/mouse event,
        // exactly the round-trip churn this work removes. [zero-poll]
    }

    pub fn apply_changes_to_surface(
        &mut self,
        delta: GetPaneRenderChangesResponse,
        bonus_lines: Vec<(StableRowIndex, Line)>,
    ) -> bool {
        self.apply_changes_to_surface_inner(delta, bonus_lines, false, false)
    }

    pub fn apply_render_application_to_surface(
        &mut self,
        delta: GetPaneRenderChangesResponse,
        bonus_lines: Vec<(StableRowIndex, Line)>,
        kind: RenderApplicationKind,
    ) -> bool {
        if delta.dirty_lines.len() > MAX_RENDER_APPLICATION_DIRTY_RANGES {
            return false;
        }
        let supplied_rows = if delta.dirty_lines.is_empty() {
            None
        } else {
            Some(
                bonus_lines
                    .iter()
                    .map(|(stable_row, _)| *stable_row)
                    .collect::<HashSet<_>>(),
            )
        };
        let mut dirty_rows = 0usize;
        for range in &delta.dirty_lines {
            let Some(span) = range
                .end
                .checked_sub(range.start)
                .and_then(|span| usize::try_from(span).ok())
            else {
                return false;
            };
            let Some(total) = dirty_rows.checked_add(span) else {
                return false;
            };
            dirty_rows = total;
            if dirty_rows > MAX_RENDER_APPLICATION_LINES
                || range.clone().any(|row| {
                    supplied_rows
                        .as_ref()
                        .is_none_or(|rows| !rows.contains(&row))
                })
            {
                return false;
            }
        }
        self.apply_changes_to_surface_inner(
            delta,
            bonus_lines,
            kind == RenderApplicationKind::Snapshot,
            true,
        )
    }

    fn apply_changes_to_surface_inner(
        &mut self,
        delta: GetPaneRenderChangesResponse,
        bonus_lines: Vec<(StableRowIndex, Line)>,
        authoritative_snapshot: bool,
        complete_application: bool,
    ) -> bool {
        log::trace!(
            "apply_changes_to_surface local={} remote={}",
            self.local_pane_id,
            self.remote_pane_id
        );
        let now = Instant::now();
        // Intentionally do NOT reset poll_interval to base here. This delta arrived
        // via the server's unilateral PUSH, which already delivered the update; the
        // liveness poll's GetPaneRenderChanges would return only liveness (no data,
        // since the push already advanced the seqno). Re-pinning the poll to base on
        // every push fires a redundant uplink RPC per push — roughly 2x the
        // round-trip/PDU traffic of an active pane on a wait-bound link, tightest
        // exactly when the pane is busiest. Let the poll back off toward max (30s)
        // while output streams: updates still arrive via push, `last_recv_time` below
        // keeps liveness fresh, death detection stays bounded at the same 30s the idle
        // path already uses, and `update_last_send()` still snaps the poll tight on
        // user input for echo/tardy responsiveness. [mux round-trip optimization]
        self.last_recv_time = now;

        let input_dispatch_serial = delta.input_serial;
        if let Some(serial) = input_dispatch_serial {
            // This round trip ends when the server has dispatched the input. It is
            // useful transport latency, but it is not PTY/application echo latency.
            self.last_input_rtt = serial.elapsed_millis();
        }

        if !authoritative_snapshot && !should_apply_unilateral_delta(self.seqno, delta.seqno) {
            if let Some(serial) = input_dispatch_serial {
                // Preserve the causal fence even when the associated surface
                // snapshot arrived after a newer delta. Missing the fence is safe
                // (TTL eventually removes the overlay), but recording it allows a
                // later in-order delta to settle the prediction without conflating
                // this stale snapshot with application output.
                self.record_input_dispatch(serial, delta.seqno);
                self.reconcile_predictions_against_cached_state(now);
            }
            log::trace!(
                "ignoring stale render delta for local={} remote={} seqno {} < {}",
                self.local_pane_id,
                self.remote_pane_id,
                delta.seqno,
                self.seqno
            );
            return false;
        }
        let alt_screen_changed = self.alt_screen_active != delta.alt_screen_active;
        if authoritative_snapshot || alt_screen_changed {
            // Stable row coordinates belong to the active screen epoch. Never
            // combine cached main-screen rows with an alternate-screen delta
            // (or vice versa), and never carry a speculative main-screen glyph
            // into a full-screen application that deliberately suppresses local
            // prediction.
            self.lines.clear();
            reset_prediction_state(&mut self.predictions, &mut self.prediction_score);
            self.last_prediction_miss = now;
        } else {
            if let Some(serial) = input_dispatch_serial {
                self.record_input_dispatch(serial, delta.seqno);
            }
            // A forced input response records only the dispatch fence above. The
            // same response has `terminal_seqno == dispatch_seqno`, so it cannot
            // retire a prediction. Only later authoritative state that advances
            // beyond that fence and carries the predicted row can settle it.
            self.reconcile_predictions(delta.seqno, &bonus_lines, now);
        }

        let mut dirty = RangeSet::new();
        for r in delta.dirty_lines {
            dirty.add_range(r.clone());
        }
        // Legacy deltas mark cursor rows for an on-demand refetch because they
        // may omit changed row content. A render application is already a
        // complete atomic unit: all content-dirty rows were validated above,
        // and cursor movement alone does not change line content. Refetching
        // those rows after ACK would reintroduce an untracked network
        // dependency and make the supposedly complete application only
        // partially applied.
        if !complete_application && delta.cursor_position != self.cursor_position {
            dirty.add(self.cursor_position.y);
            // But note that the server may have sent this in bonus_lines;
            // we'll address that below
            dirty.add(delta.cursor_position.y);
        }

        // Track alt-screen state so we never predict into a full-screen TUI. [3e]
        self.alt_screen_active = delta.alt_screen_active;

        // When it comes to updating the cursor position, if the update was tagged
        // with keyboard input, we'll only take the position if the update comes from
        // the most recent key event.  This helps to prevent the cursor wiggling if the
        // user is typing more than one character per roundtrip interval--the wiggle
        // manifests because we may have already predicted a local cursor move forwards
        // by one character, and we may receive the response to the prior update after
        // we have rendered that, and then shortly receive the most recent response.
        // The result of that is that the cursor moves right one, left one and then
        // finally right one in quick succession.
        // If the delta was not from an input event then we trust it; this is most
        // like due to a unilateral movement by the application on the other end.
        if delta.input_serial.is_none()
            || delta.input_serial.unwrap_or(InputSerial::empty()) >= self.input_serial
        {
            self.cursor_position = delta.cursor_position;
        }
        self.dimensions = delta.dimensions;
        self.tiered_scrollback_status = delta.tiered_scrollback_status;
        self.title = delta.title;
        self.working_dir = delta.working_dir.map(Into::into);
        log::trace!(
            "server says: seqno from {} -> {} for local_pane_id={}",
            self.seqno,
            delta.seqno,
            self.local_pane_id
        );
        self.seqno = delta.seqno;

        let mut hyperlink_rows = Vec::with_capacity(bonus_lines.len());
        for (stable_row, line) in bonus_lines {
            log::trace!("bonus line {} seqno={}", stable_row, line.current_seqno());
            if self.put_line(stable_row, line, None) {
                hyperlink_rows.push(stable_row);
            }
            dirty.remove(stable_row);
        }
        self.normalize_current_implicit_hyperlinks_for_rows(hyperlink_rows);

        let mut to_fetch = RangeSet::new();
        let mut fetch_token = None;
        log::trace!("dirty as of seq {} -> {:?}", delta.seqno, dirty);
        for r in dirty.iter() {
            for stable_row in r.clone() {
                // If a line is in the (probable) viewport region,
                // then we'll likely want to fetch it.
                // If it is outside that region, remove it from our cache
                // so that we'll fetch it on demand later.
                let fetchable = stable_row >= delta.dimensions.physical_top;
                let prior = self.lines.pop(&stable_row);
                let prior_kind = prior.as_ref().map(|e| e.kind());
                if !fetchable {
                    log::trace!(
                        "evict {} because it is outside the fetchable viewport",
                        stable_row
                    );
                    continue;
                }
                to_fetch.add(stable_row);
                let fetch_token = fetch_token
                    .get_or_insert_with(|| FetchToken::new(now))
                    .clone();
                let entry = match prior {
                    Some(LineEntry::Fetching(_)) | None => LineEntry::Fetching(fetch_token),
                    Some(LineEntry::LineAndFetching(old, ..))
                    | Some(LineEntry::Stale(old))
                    | Some(LineEntry::Line(old)) => LineEntry::LineAndFetching(old, fetch_token),
                };
                log::trace!(
                    "row {} {:?} -> {:?} due to dirty and IN viewport",
                    stable_row,
                    prior_kind,
                    entry.kind()
                );
                self.lines.put(stable_row, entry);
            }
        }
        if !to_fetch.is_empty() {
            if self.fetch_limiter.non_blocking_admittance_check(1) {
                self.schedule_fetch_lines(
                    to_fetch,
                    fetch_token.expect("a non-empty fetch batch has an exact token"),
                );
            } else {
                log::warn!(
                    "exceeded fetch throttle, drop {:?} and mark stale",
                    to_fetch
                );
                for r in to_fetch.iter() {
                    for stable_row in r.clone() {
                        self.make_stale(stable_row);
                    }
                }
            }
        }
        true
    }

    pub fn make_all_stale(&mut self) {
        let config = configuration();
        let capacity = render_line_cache_capacity(&config, &self.dimensions);
        rebuild_cache_as_stale(&mut self.lines, capacity);
    }

    fn make_stale(&mut self, stable_row: StableRowIndex) {
        match self.lines.pop(&stable_row) {
            Some(LineEntry::Stale(old))
            | Some(LineEntry::Line(old))
            | Some(LineEntry::LineAndFetching(old, _)) => {
                self.lines.put(stable_row, LineEntry::Stale(old));
            }
            Some(LineEntry::Fetching(_)) | None => {}
        }
    }

    /// Release only the reservations created by one exact line-fetch request.
    /// A response may omit rows and an RPC may fail after an old line was kept
    /// visible; those rows must remain stale rather than being promoted back to
    /// fresh merely because the in-flight marker is gone.
    fn release_exact_fetch_reservations(
        &mut self,
        requested: &RangeSet<StableRowIndex>,
        fetch_token: &FetchToken,
    ) {
        for range in requested.iter() {
            for stable_row in range.clone() {
                let replacement = match self.lines.pop(&stable_row) {
                    Some(LineEntry::Fetching(current)) if fetch_token.same_request(&current) => {
                        None
                    }
                    Some(LineEntry::LineAndFetching(line, current))
                        if fetch_token.same_request(&current) =>
                    {
                        Some(LineEntry::Stale(line))
                    }
                    other => other,
                };
                if let Some(entry) = replacement {
                    self.lines.put(stable_row, entry);
                }
            }
        }
    }

    pub(crate) fn mark_image_hydration_incomplete_rows(
        &mut self,
        incomplete_rows: &HashSet<StableRowIndex>,
    ) {
        for stable_row in incomplete_rows {
            self.make_stale(*stable_row);
        }
    }

    /// Resolve the complete fresh logical line containing `stable_row`.
    ///
    /// Hyperlink matching must fail closed when a boundary row is absent or
    /// stale. Scanning a partial wrapped line can manufacture a truncated URI
    /// and, once marked scanned, prevent the completed line from being
    /// reconsidered. Both the cell and physical-row limits bound adversarial
    /// chains of empty or extremely long wrapped rows.
    fn complete_cached_logical_group(
        &self,
        stable_row: StableRowIndex,
    ) -> Option<Range<StableRowIndex>> {
        if stable_row < self.dimensions.scrollback_top {
            return None;
        }
        fresh_cached_line(self.lines.peek(&stable_row))?;

        let mut start = stable_row;
        let mut backward_rows = 0usize;
        while start > self.dimensions.scrollback_top {
            let prior_row = start.checked_sub(1)?;
            let prior = fresh_cached_line(self.lines.peek(&prior_row))?;
            if !prior.last_cell_was_wrapped() {
                break;
            }
            backward_rows = backward_rows.checked_add(1)?;
            if backward_rows > mux::pane::MAX_LOGICAL_LINE_LEN {
                return None;
            }
            start = prior_row;
        }

        let mut row = start;
        let mut physical_rows = 0usize;
        let mut cells = 0usize;
        loop {
            let line = fresh_cached_line(self.lines.peek(&row))?;
            physical_rows = physical_rows.checked_add(1)?;
            cells = cells.checked_add(line.len())?;
            if physical_rows > mux::pane::MAX_LOGICAL_LINE_LEN
                || (physical_rows > 1 && cells > mux::pane::MAX_LOGICAL_LINE_LEN)
            {
                return None;
            }

            let end = row.checked_add(1)?;
            if !line.last_cell_was_wrapped() {
                return Some(start..end);
            }
            row = end;
        }
    }

    fn normalize_cached_logical_group(
        &mut self,
        group: Range<StableRowIndex>,
        rules: &[Rule],
    ) -> bool {
        let mut extracted = Vec::new();
        for stable_row in group.clone() {
            match self.lines.pop(&stable_row) {
                Some(LineEntry::Line(line)) => extracted.push((stable_row, line)),
                Some(entry) => {
                    self.lines.put(stable_row, entry);
                    for (row, line) in extracted {
                        self.lines.put(row, LineEntry::Line(line));
                    }
                    return false;
                }
                None => {
                    for (row, line) in extracted {
                        self.lines.put(row, LineEntry::Line(line));
                    }
                    return false;
                }
            }
        }

        let mut line_refs: Vec<&mut Line> = extracted.iter_mut().map(|(_, line)| line).collect();
        Line::apply_hyperlink_rules(rules, &mut line_refs);
        drop(line_refs);

        for (stable_row, line) in extracted {
            self.lines.put(stable_row, LineEntry::Line(line));
        }
        true
    }

    /// Select the exact rule set for this cache epoch. Switching rule sets is
    /// rare but must invalidate every cached representation before any group is
    /// rescanned; otherwise rows outside the immediate paint range can retain
    /// links produced by a different window's configuration.
    fn select_implicit_hyperlink_rules(&mut self, rules: &[Rule]) -> bool {
        if hyperlink_rules_equal(&self.implicit_hyperlink_rules, rules) {
            return false;
        }

        let stable_rows = self
            .lines
            .iter()
            .map(|(stable_row, _)| *stable_row)
            .collect::<Vec<_>>();
        for stable_row in stable_rows {
            let Some(entry) = self.lines.get_mut(&stable_row) else {
                continue;
            };
            let line = match entry {
                LineEntry::Line(line)
                | LineEntry::LineAndFetching(line, _)
                | LineEntry::Stale(line) => line,
                LineEntry::Fetching(_) => continue,
            };
            let seqno = line.current_seqno();
            line.invalidate_implicit_hyperlinks(seqno);
        }
        self.implicit_hyperlink_rules = rules.to_vec();
        true
    }

    fn normalize_implicit_hyperlinks_for_rows(
        &mut self,
        touched_rows: impl IntoIterator<Item = StableRowIndex>,
        rules: &[Rule],
    ) {
        if rules.is_empty() {
            return;
        }
        let mut seen = HashSet::new();
        let mut groups = Vec::new();
        for stable_row in touched_rows {
            // Complete logical groups are normalized as a unit. Once a fresh
            // touched row is marked scanned for the selected rule epoch,
            // rediscovering both wrapped boundaries on every unchanged paint
            // is pure allocation and LRU churn.
            let Some(line) = fresh_cached_line(self.lines.peek(&stable_row)) else {
                continue;
            };
            if line.implicit_hyperlinks_are_scanned() {
                continue;
            }
            let Some(group) = self.complete_cached_logical_group(stable_row) else {
                continue;
            };
            if seen.insert((group.start, group.end)) {
                groups.push(group);
            }
        }
        groups.sort_unstable_by_key(|group| group.start);
        for group in groups {
            let _ = self.normalize_cached_logical_group(group, rules);
        }
    }

    fn normalize_all_implicit_hyperlinks(&mut self, rules: &[Rule]) {
        let stable_rows = self
            .lines
            .iter()
            .filter_map(|(stable_row, entry)| {
                matches!(entry, LineEntry::Line(_)).then_some(*stable_row)
            })
            .collect::<Vec<_>>();
        self.normalize_implicit_hyperlinks_for_rows(stable_rows, rules);
    }

    fn normalize_implicit_hyperlinks_for_request(
        &mut self,
        touched_rows: impl IntoIterator<Item = StableRowIndex>,
        rules: &[Rule],
    ) {
        if self.select_implicit_hyperlink_rules(rules) {
            self.normalize_all_implicit_hyperlinks(rules);
        } else {
            self.normalize_implicit_hyperlinks_for_rows(touched_rows, rules);
        }
    }

    fn normalize_current_implicit_hyperlinks_for_rows(
        &mut self,
        touched_rows: impl IntoIterator<Item = StableRowIndex>,
    ) {
        let rules = self.implicit_hyperlink_rules.clone();
        self.normalize_implicit_hyperlinks_for_rows(touched_rows, &rules);
    }

    fn put_line(
        &mut self,
        stable_row: StableRowIndex,
        mut line: Line,
        fetch_token: Option<&FetchToken>,
    ) -> bool {
        // The remote endpoint may have scanned this physical row under a
        // different rule set. Preserve explicit OSC-8 links, but clear all
        // implicit state until the complete logical group is present locally.
        // Scanning one physical fragment eagerly can create a clickable but
        // truncated URL and gives the fragment a false "already scanned" bit.
        let seqno = line.current_seqno();
        line.invalidate_implicit_hyperlinks(seqno);

        let entry = if let Some(fetch_token) = fetch_token {
            // If we're completing a fetch, only replace entries that were
            // set to fetching as part of our fetch.  If they are now longer
            // tagged that way, then someone came along after us and changed
            // the state, so we should leave it alone

            match self.lines.pop(&stable_row) {
                Some(LineEntry::LineAndFetching(_, current))
                | Some(LineEntry::Fetching(current))
                    if fetch_token.same_request(&current) =>
                {
                    log::trace!(
                        "row {} fetch done -> Line seq={} vs self.seq={}",
                        stable_row,
                        line.current_seqno(),
                        self.seqno
                    );
                    line.update_last_change_seqno(self.seqno);
                    LineEntry::Line(line)
                }
                Some(e) => {
                    // It changed since we started: leave it alone!
                    log::trace!(
                        "row {} {:?} changed since fetch started at {:?}, so leave it be",
                        stable_row,
                        e.kind(),
                        fetch_token.started_at()
                    );
                    self.lines.put(stable_row, e);
                    return false;
                }
                None => return false,
            }
        } else {
            LineEntry::Line(line)
        };
        self.lines.put(stable_row, entry);
        true
    }

    fn schedule_fetch_lines(
        &mut self,
        to_fetch: RangeSet<StableRowIndex>,
        fetch_token: FetchToken,
    ) {
        if to_fetch.is_empty() {
            return;
        }
        if self.dead {
            self.release_exact_fetch_reservations(&to_fetch, &fetch_token);
            return;
        }
        let reservation = match promise::spawn::try_reserve_main_thread(
            promise::spawn::MainThreadServiceClass::Render,
            64 * 1024,
        ) {
            promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
            rejected => {
                self.release_exact_fetch_reservations(&to_fetch, &fetch_token);
                log::error!(
                    "main-thread scheduler rejected render-line fetch; released exact fetch reservations for retry: {rejected:?}"
                );
                return;
            }
        };

        let Some(registration) = self.mux_registration.load() else {
            for range in to_fetch.iter() {
                for stable_row in range.clone() {
                    self.make_stale(stable_row);
                }
            }
            return;
        };
        let Some(renderable) = self.renderable.upgrade() else {
            for range in to_fetch.iter() {
                for stable_row in range.clone() {
                    self.make_stale(stable_row);
                }
            }
            return;
        };
        let local_pane_id = self.local_pane_id;
        log::trace!(
            "will fetch lines {:?} for remote tab id {} at {:?}",
            to_fetch,
            self.remote_pane_id,
            fetch_token.started_at(),
        );

        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let rpc = client.client.rpc_scope();
        let request = rpc.get_lines(GetLines {
            pane_id: remote_pane_id,
            lines: to_fetch.clone().into(),
        });

        reservation
            .spawn_local(async move {
                let result = request.await;

                let result = match result {
                    Ok(result) if result.pane_id == remote_pane_id => {
                        hydrate_lines(&rpc, remote_pane_id, result.lines).await
                    }
                    Ok(result) => Err(anyhow::anyhow!(
                        "GetLines response pane mismatch: expected {remote_pane_id}, got {}",
                        result.pane_id
                    )),
                    Err(err) => Err(err),
                };
                Self::apply_lines(
                    registration,
                    renderable,
                    local_pane_id,
                    rpc,
                    result,
                    to_fetch,
                    fetch_token,
                )
            })
            .detach();
    }

    fn apply_lines(
        registration: PaneRegistrationHandle,
        renderable: Arc<parking_lot::Mutex<RenderableState>>,
        local_pane_id: PaneId,
        rpc: RpcGenerationScope,
        result: anyhow::Result<HydratedLines>,
        to_fetch: RangeSet<StableRowIndex>,
        fetch_token: FetchToken,
    ) -> anyhow::Result<()> {
        // Fetch cleanup is intentionally allowed after this RPC generation has
        // retired: it releases only the exact reservation created by this
        // request. Pointer identity, rather than timestamp equality, prevents a
        // stale G1 completion from clearing a successor request that happened
        // to start at the same clock instant.
        let release_exact_fetch_reservations = || {
            registration.try_with_current_output(|_| {
                let renderable = renderable.lock();
                let mut inner = renderable.inner.borrow_mut();
                inner.release_exact_fetch_reservations(&to_fetch, &fetch_token);
            })
        };

        let applied = match result {
            Ok(hydrated) => match rpc.commit_sync(RpcConsumerKind::FetchedLines, || {
                registration.try_with_current_output(|_| {
                    let renderable = renderable.lock();
                    let mut inner = renderable.inner.borrow_mut();
                    let (lines, incomplete_rows) = hydrated.into_parts();
                    log::trace!(
                        "fetch complete for {:?} at {:?}",
                        to_fetch,
                        fetch_token.started_at()
                    );
                    let mut hyperlink_rows = Vec::with_capacity(lines.len());
                    for (stable_row, line) in lines.into_iter() {
                        if incomplete_rows.contains(&stable_row) {
                            // Image-bearing lines are a row-level transaction.
                            // Keep a previously complete row visible while the
                            // replacement is incomplete; publishing a text-only
                            // or partial z-stack causes flicker and can expose a
                            // composition that never existed on the server.
                            inner.make_stale(stable_row);
                            continue;
                        }
                        if inner.put_line(stable_row, line, Some(&fetch_token)) {
                            hyperlink_rows.push(stable_row);
                        }
                    }
                    // A successful response is still allowed to be partial.
                    // Sweep exact markers after applying returned rows so an
                    // omitted row cannot remain Fetching forever.
                    inner.release_exact_fetch_reservations(&to_fetch, &fetch_token);
                    inner.normalize_current_implicit_hyperlinks_for_rows(hyperlink_rows);
                })
            }) {
                Ok(applied) => applied,
                Err(error) => {
                    let _ = release_exact_fetch_reservations();
                    return Err(anyhow::Error::new(error));
                }
            },
            Err(err) => {
                log::error!("get_lines failed: {}", err);
                release_exact_fetch_reservations()
            }
        };
        if applied.is_none() {
            log::trace!(
                "discarding fetched lines for stale client pane registration {}",
                local_pane_id
            );
        }
        log::trace!(
            "exact-generation PaneOutput reservation completed for local_pane_id={}",
            local_pane_id,
        );
        Ok(())
    }

    fn poll(&mut self) -> anyhow::Result<()> {
        if self.poll_in_progress.load(Ordering::SeqCst) {
            // We have a poll in progress
            return Ok(());
        }

        if self.last_poll.elapsed() < self.poll_interval {
            return Ok(());
        }

        // Liveness backstop only: jump straight to the max interval after the
        // bootstrap poll instead of ramping 20ms -> ... -> max. Real updates arrive
        // via the server's unilateral push, and a dead connection is detected at
        // ~RTT by the transport reader thread + the PaneRemoved push, so the poll is
        // just a slow backstop. The first poll still fires immediately (the pane is
        // constructed with last_poll = initial_last_poll(now)) to register the
        // server-side push tracking that all subsequent pushes depend on. [zero-poll]
        self.poll_interval = max_poll_interval();

        let Some(registration) = self.mux_registration.load() else {
            return Ok(());
        };
        let Some(renderable) = self.renderable.upgrade() else {
            return Ok(());
        };
        let reservation = match promise::spawn::try_reserve_main_thread(
            promise::spawn::MainThreadServiceClass::Render,
            16 * 1024,
        ) {
            promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
            rejected => {
                log::error!(
                    "main-thread scheduler rejected liveness poll before state mutation; next poll remains eligible: {rejected:?}"
                );
                return Ok(());
            }
        };

        self.last_poll = Instant::now();
        self.poll_in_progress.store(true, Ordering::SeqCst);
        let remote_pane_id = self.remote_pane_id;
        let local_pane_id = self.local_pane_id;
        let client = Arc::clone(&self.client);
        let rpc = client.client.rpc_scope();
        let request = rpc.get_pane_render_changes(GetPaneRenderChanges {
            pane_id: remote_pane_id,
        });
        reservation
            .spawn_local(async move {
                let response = request.await;
            let cleared = registration.try_with_current(|_| {
                let renderable = renderable.lock();
                let inner = renderable.inner.borrow();
                inner.poll_in_progress.store(false, Ordering::SeqCst);
            });
            if cleared.is_none() {
                log::trace!(
                    "discarding liveness poll completion for stale client pane registration {}",
                    local_pane_id
                );
            }
            let alive = match response {
                Ok(response) if response.pane_id == remote_pane_id => response.is_alive,
                Ok(response) => {
                    return Err(anyhow::anyhow!(
                        "liveness response pane mismatch: expected {remote_pane_id}, got {}",
                        response.pane_id
                    ));
                }
                // Preserve the established liveness policy: a transient
                // transport failure cannot declare a reconnectable pane dead,
                // while a non-reconnectable pane has no successor that could
                // revive it. The generation commit below still prevents a G1
                // result from mutating state after G2 publication.
                Err(_) => client.client.is_reconnectable,
            };
            let updated = rpc
                .commit_sync(RpcConsumerKind::Liveness, || {
                    registration.try_with_current(|_| {
                        let renderable = renderable.lock();
                        let mut inner = renderable.inner.borrow_mut();
                        inner.dead = !alive;
                        inner.last_recv_time = Instant::now();
                    })
                })
                .map_err(anyhow::Error::new)?;
            if updated.is_none() {
                log::trace!(
                    "discarding liveness state update for stale client pane registration {}",
                    local_pane_id
                );
            }
                Ok::<(), anyhow::Error>(())
            })
            .detach();
        Ok(())
    }
}

const IMAGE_LRU_MAX_ENTRIES: usize = 128;
const IMAGE_LRU_MAX_BYTES: usize = 256 * 1024 * 1024;
const MAX_ORDINARY_IMAGE_BATCH_BYTES: usize = MAX_IMAGE_HYDRATION_DECODED_BYTES;
const MAX_ENCODED_IMAGE_BYTES: usize = MAX_IMAGE_HYDRATION_DECODED_BYTES;
const MAX_CONCURRENT_IMAGE_HYDRATIONS: usize = 8;
const MAX_IMAGE_LOCATOR_ATTEMPTS_PER_REVISION: usize = 8;
const MAX_IMAGE_VALIDATION_WORKERS: usize = 2;
const MAX_PENDING_IMAGE_VALIDATIONS: usize = 8;
const MAX_DECODED_IMAGE_FRAMES: usize = 4_096;
const MAX_RENDERABLE_IMAGE_AXIS: u32 = 16_384;
const ORDINARY_IMAGE_HYDRATION_TIMEOUT: Duration = Duration::from_secs(2);
// One admitted hydration can temporarily retain the compressed frame, its
// decompressed typed response, the accepted decoded-frame aggregate, and the
// next fully decoded frame that crosses the aggregate limit before validation
// rejects it. Reserve that full worst-case lifetime before issuing the RPC.
// Keeping a single global slot prevents multiple panes from multiplying these
// large replies on the connection reader and bounds damage until image transfer
// moves to a cancellable out-of-band channel.
const IMAGE_HYDRATION_WORKING_SET_RESERVATION_BYTES: usize =
    MAX_GET_IMAGE_CELL_RESPONSE_DECOMPRESSED_BYTES * 2 + MAX_IMAGE_HYDRATION_DECODED_BYTES * 2;
const MAX_GLOBAL_IMAGE_HYDRATION_WORKING_SET_BYTES: usize =
    IMAGE_HYDRATION_WORKING_SET_RESERVATION_BYTES;

type ImageCacheKey = (RenderConnectionIdentity, PaneId, [u8; 32]);

#[derive(Clone, Debug)]
struct ValidatedImageData {
    data: Arc<ImageData>,
    decoded_bytes: usize,
    source_revision: [u8; 32],
}

#[derive(Debug, thiserror::Error)]
enum ImageValidationFailure {
    #[error(transparent)]
    Invalid(ImageDataValidationError),
    #[error("image validation unavailable: {0}")]
    Unavailable(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CachedImageFailure {
    Permanent,
    Transient { retry_after: Instant },
}

#[derive(Debug)]
enum ImageHydrationAttempt {
    Validated(ValidatedImageData),
    PermanentFailure,
    TransientFailure,
}

#[derive(Debug)]
struct ImageLru {
    cache: LruCache<ImageCacheKey, ValidatedImageData>,
    failures: LruCache<ImageCacheKey, CachedImageFailure>,
    retained_bytes: usize,
    max_bytes: usize,
}

impl ImageLru {
    fn new(max_entries: NonZeroUsize, max_bytes: usize) -> Self {
        Self {
            cache: LruCache::new(max_entries),
            failures: LruCache::new(max_entries),
            retained_bytes: 0,
            max_bytes,
        }
    }

    fn recompute_retained_bytes(&mut self) {
        self.retained_bytes = self
            .cache
            .iter()
            .try_fold(0usize, |total, (_, image)| {
                total.checked_add(image.decoded_bytes)
            })
            .unwrap_or(usize::MAX);
    }

    fn forget_retained_bytes(&mut self, bytes: usize) {
        if let Some(retained) = self.retained_bytes.checked_sub(bytes) {
            self.retained_bytes = retained;
        } else {
            self.recompute_retained_bytes();
        }
    }

    #[cfg(test)]
    fn get(
        &mut self,
        connection: RenderConnectionIdentity,
        pane_id: PaneId,
        hash: &[u8; 32],
    ) -> Option<ValidatedImageData> {
        let key = (connection, pane_id, *hash);
        let is_current = self
            .cache
            .peek(&key)
            .is_some_and(|validated| validated.data.current_content_hash() == *hash);
        if !is_current {
            if let Some(stale) = self.cache.pop(&key) {
                self.forget_retained_bytes(stale.decoded_bytes);
            }
            return None;
        }
        self.cache.get(&key).cloned()
    }

    fn get_candidate(
        &mut self,
        connection: RenderConnectionIdentity,
        pane_id: PaneId,
        hash: &[u8; 32],
    ) -> Option<ValidatedImageData> {
        self.cache.get(&(connection, pane_id, *hash)).cloned()
    }

    fn remove_candidate_if_same(
        &mut self,
        connection: RenderConnectionIdentity,
        pane_id: PaneId,
        hash: [u8; 32],
        candidate: &ValidatedImageData,
    ) {
        let key = (connection, pane_id, hash);
        let is_same = self.cache.peek(&key).is_some_and(|current| {
            current.source_revision == candidate.source_revision
                && Arc::ptr_eq(&current.data, &candidate.data)
        });
        if is_same {
            if let Some(stale) = self.cache.pop(&key) {
                self.forget_retained_bytes(stale.decoded_bytes);
            }
        }
    }

    #[cfg(test)]
    fn put(
        &mut self,
        connection: RenderConnectionIdentity,
        pane_id: PaneId,
        validated: ValidatedImageData,
    ) {
        if validated.decoded_bytes > self.max_bytes
            || validated.data.current_content_hash() != validated.source_revision
        {
            return;
        }
        self.put_trusted(connection, pane_id, validated);
    }

    fn put_trusted(
        &mut self,
        connection: RenderConnectionIdentity,
        pane_id: PaneId,
        validated: ValidatedImageData,
    ) {
        if validated.decoded_bytes > self.max_bytes {
            return;
        }
        let key = (connection, pane_id, validated.source_revision);
        let _ = self.failures.pop(&key);
        if let Some(old) = self.cache.pop(&key) {
            self.forget_retained_bytes(old.decoded_bytes);
        }

        let decoded_bytes = validated.decoded_bytes;
        let retained_with_new = self.retained_bytes.checked_add(decoded_bytes);
        let evicted = self.cache.push(key, validated);
        if let Some(retained_with_new) = retained_with_new {
            self.retained_bytes = retained_with_new;
            if let Some((_, evicted)) = evicted {
                self.forget_retained_bytes(evicted.decoded_bytes);
            }
        } else {
            // The cache already contains the new entry at this point, so this
            // repair observes exactly the authoritative post-insert contents.
            self.recompute_retained_bytes();
        }
        self.enforce_byte_budget();
    }

    fn get_failure(
        &mut self,
        connection: RenderConnectionIdentity,
        pane_id: PaneId,
        revision: &[u8; 32],
        now: Instant,
    ) -> Option<CachedImageFailure> {
        let key = (connection, pane_id, *revision);
        let failure = self.failures.get(&key).copied()?;
        if matches!(failure, CachedImageFailure::Transient { retry_after } if retry_after <= now) {
            self.failures.pop(&key);
            None
        } else {
            Some(failure)
        }
    }

    fn record_failure(
        &mut self,
        connection: RenderConnectionIdentity,
        pane_id: PaneId,
        revision: [u8; 32],
        failure: CachedImageFailure,
    ) {
        let key = (connection, pane_id, revision);
        if self.cache.contains(&key) {
            return;
        }
        self.failures.push(key, failure);
    }

    fn enforce_byte_budget(&mut self) {
        while self.retained_bytes > self.max_bytes {
            let Some((_, evicted)) = self.cache.pop_lru() else {
                self.recompute_retained_bytes();
                break;
            };
            self.forget_retained_bytes(evicted.decoded_bytes);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.cache.len()
    }

    #[cfg(test)]
    fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
}

lazy_static::lazy_static! {
    static ref IMAGE_LRU: Mutex<ImageLru> = Mutex::new(ImageLru::new(
        NonZeroUsize::new(IMAGE_LRU_MAX_ENTRIES).unwrap(),
        IMAGE_LRU_MAX_BYTES,
    ));
    static ref IMAGE_VALIDATION_POOL: Result<rayon::ThreadPool, String> =
        rayon::ThreadPoolBuilder::new()
            .num_threads(MAX_IMAGE_VALIDATION_WORKERS)
            .thread_name(|index| format!("ft-image-validation-{index}"))
            .build()
            .map_err(|error| error.to_string());
}

static IMAGE_VALIDATION_JOBS: AtomicUsize = AtomicUsize::new(0);
static IMAGE_HYDRATION_RESERVED_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Atomically reserve `amount` without wrapping and without crossing the
/// caller-owned resource ceiling. A failed or contended compare/exchange
/// retries from the observed value so overflow and saturation are checked
/// against the state that actually won the race.
fn try_reserve_bounded_atomic(counter: &AtomicUsize, amount: usize, limit: usize) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(amount).filter(|next| *next <= limit) else {
            return false;
        };
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

/// Release exactly `amount` while leaving the counter unchanged on an
/// invariant-breaking underflow instead of wrapping into counterfeit budget.
fn try_release_atomic(counter: &AtomicUsize, amount: usize) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_sub(amount) else {
            return false;
        };
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

struct ImageHydrationBytePermit {
    bytes: usize,
}

impl ImageHydrationBytePermit {
    fn try_acquire(bytes: usize) -> Option<Self> {
        try_reserve_bounded_atomic(
            &IMAGE_HYDRATION_RESERVED_BYTES,
            bytes,
            MAX_GLOBAL_IMAGE_HYDRATION_WORKING_SET_BYTES,
        )
        .then_some(Self { bytes })
    }
}

impl Drop for ImageHydrationBytePermit {
    fn drop(&mut self) {
        let bytes = self.bytes;
        let released = try_release_atomic(&IMAGE_HYDRATION_RESERVED_BYTES, bytes);
        debug_assert!(released);
    }
}

struct ImageValidationJobPermit;

impl ImageValidationJobPermit {
    fn try_acquire() -> Option<Self> {
        try_reserve_bounded_atomic(&IMAGE_VALIDATION_JOBS, 1, MAX_PENDING_IMAGE_VALIDATIONS)
            .then_some(Self)
    }
}

impl Drop for ImageValidationJobPermit {
    fn drop(&mut self) {
        let released = try_release_atomic(&IMAGE_VALIDATION_JOBS, 1);
        debug_assert!(released);
    }
}

struct CancelImageValidationOnDrop {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

impl CancelImageValidationOnDrop {
    fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self {
            cancelled,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelImageValidationOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

fn lock_image_lru() -> MutexGuard<'static, ImageLru> {
    IMAGE_LRU.lock().unwrap_or_else(|poisoned| {
        log::warn!("recovering poisoned client image cache lock");
        IMAGE_LRU.clear_poison();
        poisoned.into_inner()
    })
}

fn get_cached_validated_image(
    connection: RenderConnectionIdentity,
    pane_id: PaneId,
    hash: &[u8; 32],
) -> Option<ValidatedImageData> {
    let candidate = {
        let mut cache = lock_image_lru();
        cache.get_candidate(connection, pane_id, hash)?
    };
    if candidate.data.current_content_hash() == *hash {
        return Some(candidate);
    }

    // Hashing a mutable payload can scan tens of MiB. It deliberately occurs
    // outside the global LRU mutex; remove only the exact candidate observed
    // before hashing so a concurrent replacement cannot be evicted.
    lock_image_lru().remove_candidate_if_same(connection, pane_id, *hash, &candidate);
    None
}

fn put_cached_validated_image(
    connection: RenderConnectionIdentity,
    pane_id: PaneId,
    validated: ValidatedImageData,
) -> bool {
    if validated.data.current_content_hash() != validated.source_revision {
        return false;
    }
    lock_image_lru().put_trusted(connection, pane_id, validated);
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OrdinaryImageBatchAdmission {
    Accepted { next_decoded_bytes: usize },
    BatchDecodedByteLimit,
    DecodedByteOverflow,
    RevisionChangedBeforeCachePublish,
}

/// Publish an individually valid image before deciding whether this particular
/// line batch still has room to attach it. The global cache has its own strict
/// entry and decoded-byte bounds, while the batch limit governs only the set
/// retained by this hydration result. Keeping those authorities separate
/// avoids fetching and decoding the same valid overflow-tail revision on every
/// retry of an oversized aggregate.
fn cache_and_admit_ordinary_image(
    connection: Option<RenderConnectionIdentity>,
    pane_id: PaneId,
    accepted_decoded_bytes: usize,
    validated: &ValidatedImageData,
) -> OrdinaryImageBatchAdmission {
    if connection
        .is_some_and(|identity| !put_cached_validated_image(identity, pane_id, validated.clone()))
    {
        return OrdinaryImageBatchAdmission::RevisionChangedBeforeCachePublish;
    }
    let Some(next_decoded_bytes) = accepted_decoded_bytes.checked_add(validated.decoded_bytes)
    else {
        return OrdinaryImageBatchAdmission::DecodedByteOverflow;
    };
    if next_decoded_bytes > MAX_ORDINARY_IMAGE_BATCH_BYTES {
        return OrdinaryImageBatchAdmission::BatchDecodedByteLimit;
    }
    OrdinaryImageBatchAdmission::Accepted { next_decoded_bytes }
}

fn record_image_hydration_rejection(reason: &'static str) {
    metrics::counter!(
        "mux.client.image_hydration.rejected.total",
        "reason" => reason
    )
    .increment(1);
}

fn push_image_locator(
    requests: &mut Vec<([u8; 32], Vec<GetImageCell>)>,
    request_indices: &mut HashMap<[u8; 32], usize>,
    hash: [u8; 32],
    request: GetImageCell,
) {
    if let Some(index) = request_indices.get(&hash).copied() {
        let locators = &mut requests[index].1;
        if locators.len() < MAX_IMAGE_LOCATOR_ATTEMPTS_PER_REVISION {
            locators.push(request);
        } else {
            record_image_hydration_rejection("locator_attempt_limit");
        }
        return;
    }

    request_indices.insert(hash, requests.len());
    requests.push((hash, vec![request]));
}

fn rows_requiring_image_retry(
    unavailable_rows: &HashSet<StableRowIndex>,
    rows_with_permanent_failure: &HashSet<StableRowIndex>,
) -> HashSet<StableRowIndex> {
    // Row publication is atomic. If any layer is permanently malformed, the
    // server composition can never be reconstructed exactly, so settle that
    // entire row to its text-only fallback even when another layer on the same
    // row failed transiently. Rows with only transient failures retain their
    // prior complete cache entry and retry later.
    unavailable_rows
        .difference(rows_with_permanent_failure)
        .copied()
        .collect()
}

async fn await_image_work_until<F>(future: F, deadline: Instant) -> Option<F::Output>
where
    F: Future,
{
    let remaining = deadline.checked_duration_since(Instant::now())?;
    if remaining.is_zero() {
        return None;
    }
    let timeout = async move {
        promise::spawn::sleep(remaining).await;
    };
    pin_mut!(future);
    pin_mut!(timeout);
    match select(future, timeout).await {
        Either::Left((output, _)) => Some(output),
        Either::Right(((), _)) => None,
    }
}

async fn acquire_image_hydration_bytes_until(
    deadline: Instant,
) -> Option<ImageHydrationBytePermit> {
    loop {
        if let Some(permit) =
            ImageHydrationBytePermit::try_acquire(IMAGE_HYDRATION_WORKING_SET_RESERVATION_BYTES)
        {
            return Some(permit);
        }
        let remaining = deadline.checked_duration_since(Instant::now())?;
        if remaining.is_zero() {
            return None;
        }
        promise::spawn::sleep(remaining.min(Duration::from_millis(2))).await;
    }
}

async fn validate_image_off_main_thread(
    data: Arc<ImageData>,
    expected_revision: [u8; 32],
    limits: ImageDataValidationLimits,
) -> Result<ValidatedImageData, ImageValidationFailure> {
    let pool = IMAGE_VALIDATION_POOL.as_ref().map_err(|error| {
        ImageValidationFailure::Unavailable(format!(
            "image validation pool is unavailable: {error}"
        ))
    })?;
    let permit = ImageValidationJobPermit::try_acquire().ok_or_else(|| {
        ImageValidationFailure::Unavailable(format!(
            "image validation queue is full ({MAX_PENDING_IMAGE_VALIDATIONS} running or pending jobs)"
        ))
    })?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut cancel_on_drop = CancelImageValidationOnDrop::new(Arc::clone(&cancelled));
    let (sender, receiver) = futures::channel::oneshot::channel();
    pool.spawn(move || {
        let _permit = permit;
        let result = (|| {
            let normalized = data.normalize_for_content_revision_with_limits(
                expected_revision,
                MAX_ENCODED_IMAGE_BYTES,
                limits,
                &|| cancelled.load(Ordering::Acquire),
            )?;
            let immutable_data = match normalized.replacement {
                Some(replacement) => Arc::new(replacement),
                None => {
                    // A response payload is expected to be uniquely owned at
                    // this trust boundary. Rewrap the same validated ImageData
                    // after proving uniqueness so its local, non-serializable
                    // revision authority survives into the renderer; rebuilding
                    // it from `into_data()` would discard that proof and force a
                    // second full-buffer hash pass on the GUI thread.
                    let owned = Arc::try_unwrap(data)
                        .map_err(|_| ImageDataValidationError::SharedMutablePayload)?;
                    Arc::new(owned)
                }
            };
            debug_assert_eq!(
                immutable_data.validated_summary_for_content_revision(expected_revision, limits),
                Some(normalized.summary),
                "validated image rewrap must preserve local renderer authority"
            );
            Ok(ValidatedImageData {
                data: immutable_data,
                decoded_bytes: normalized.summary.decoded_bytes,
                source_revision: expected_revision,
            })
        })()
        .map_err(|error| match error {
            ImageDataValidationError::DecodeCancelled => {
                ImageValidationFailure::Unavailable("image validation was cancelled".into())
            }
            ImageDataValidationError::SharedMutablePayload => ImageValidationFailure::Unavailable(
                "validated image payload remained externally shared and mutable".into(),
            ),
            error @ (ImageDataValidationError::EncodedResourceIo { .. }
            | ImageDataValidationError::EncodedLeaseUnavailable { .. }) => {
                // A blob lease or its backing store can become available
                // again without the requested image revision changing.
                // Never permanently poison that revision in the negative
                // cache merely because one resource read failed.
                ImageValidationFailure::Unavailable(error.to_string())
            }
            error => ImageValidationFailure::Invalid(error),
        });
        let _ = sender.send(result);
    });
    let result = receiver.await.map_err(|_| {
        ImageValidationFailure::Unavailable(
            "image validation worker exited without a result".to_string(),
        )
    })?;
    cancel_on_drop.disarm();
    result
}

#[derive(Debug, Default)]
pub(crate) struct HydratedLines {
    lines: Vec<(StableRowIndex, Line)>,
    incomplete_rows: HashSet<StableRowIndex>,
}

impl HydratedLines {
    fn complete(lines: Vec<(StableRowIndex, Line)>) -> Self {
        Self {
            lines,
            incomplete_rows: HashSet::new(),
        }
    }

    pub(crate) fn into_parts(self) -> (Vec<(StableRowIndex, Line)>, HashSet<StableRowIndex>) {
        (self.lines, self.incomplete_rows)
    }
}

pub(crate) async fn hydrate_lines(
    rpc: &RpcGenerationScope,
    pane_id: PaneId,
    serialized_lines: SerializedLines,
) -> anyhow::Result<HydratedLines> {
    let started_at = Instant::now();
    let deadline = started_at
        .checked_add(ORDINARY_IMAGE_HYDRATION_TIMEOUT)
        .unwrap_or(started_at);
    let counts = serialized_lines
        .validate_structure()
        .map_err(anyhow::Error::new)?;
    if counts.lines > MAX_RENDER_APPLICATION_LINES
        || counts.cells > MAX_RENDER_APPLICATION_CELLS
        || counts.hyperlink_spans > MAX_RENDER_APPLICATION_HYPERLINK_SPANS
        || counts.images > MAX_RENDER_APPLICATION_IMAGE_REFERENCES
    {
        record_image_hydration_rejection("serialized_resource_limit");
        anyhow::bail!("GetLines payload exceeds bounded render resources: {counts:?}");
    }
    // The payload cannot change between validation and extraction because it
    // is consumed here. Avoid a second O(lines + cells + references) pass on
    // this latency-sensitive path.
    let (mut lines, image_cells) = serialized_lines.extract_data();

    if image_cells.is_empty() {
        return Ok(HydratedLines::complete(lines));
    }

    let connection_identity = rpc.render_connection_identity();
    let mut requests = Vec::<([u8; 32], Vec<GetImageCell>)>::new();
    let mut request_indices = HashMap::<[u8; 32], usize>::new();
    let mut data_by_hash = HashMap::<[u8; 32], ValidatedImageData>::new();
    let mut permanently_failed_hashes = HashSet::<[u8; 32]>::new();
    let mut accepted_decoded_bytes = 0usize;
    for image in &image_cells {
        if data_by_hash.contains_key(&image.data_hash) {
            continue;
        }
        if request_indices.contains_key(&image.data_hash) {
            push_image_locator(
                &mut requests,
                &mut request_indices,
                image.data_hash,
                GetImageCell {
                    pane_id,
                    line_idx: image.line_idx,
                    cell_idx: image.cell_idx,
                    data_hash: image.data_hash,
                },
            );
            continue;
        }
        let cached = connection_identity
            .and_then(|identity| get_cached_validated_image(identity, pane_id, &image.data_hash));
        if let Some(validated) = cached {
            accepted_decoded_bytes = accepted_decoded_bytes
                .checked_add(validated.decoded_bytes)
                .ok_or_else(|| anyhow::anyhow!("decoded image byte accounting overflowed"))?;
            if accepted_decoded_bytes > MAX_ORDINARY_IMAGE_BATCH_BYTES {
                record_image_hydration_rejection("batch_decoded_byte_limit");
                break;
            }
            data_by_hash.insert(image.data_hash, validated);
        } else if let Some(failure) = connection_identity.and_then(|identity| {
            lock_image_lru().get_failure(identity, pane_id, &image.data_hash, Instant::now())
        }) {
            match failure {
                CachedImageFailure::Permanent => {
                    permanently_failed_hashes.insert(image.data_hash);
                }
                CachedImageFailure::Transient { .. } => {}
            }
        } else {
            // Keep every already-admitted alternate locator for this hash. A row
            // can change or scroll out between the line snapshot and this
            // legacy coordinate lookup; a stale first locator must not make
            // a later still-valid occurrence unavailable. The enclosing
            // SerializedLines trust boundary caps all image references at
            // MAX_RENDER_APPLICATION_IMAGE_REFERENCES, while the hydration
            // deadline caps attempted RPC work.
            push_image_locator(
                &mut requests,
                &mut request_indices,
                image.data_hash,
                GetImageCell {
                    pane_id,
                    line_idx: image.line_idx,
                    cell_idx: image.cell_idx,
                    data_hash: image.data_hash,
                },
            );
        }
    }

    let validation_limits = ImageDataValidationLimits {
        max_decoded_bytes: MAX_ORDINARY_IMAGE_BATCH_BYTES,
        max_frame_count: MAX_DECODED_IMAGE_FRAMES,
        max_width: MAX_RENDERABLE_IMAGE_AXIS,
        max_height: MAX_RENDERABLE_IMAGE_AXIS,
    };
    let mut unresolved_hashes = requests
        .iter()
        .map(|(hash, _)| *hash)
        .collect::<HashSet<_>>();
    let hydration = stream::iter(requests)
        .map(|(expected_hash, locators)| async move {
            for request in locators {
                let Some(_byte_permit) = acquire_image_hydration_bytes_until(deadline).await else {
                    record_image_hydration_rejection("global_working_set_limit");
                    return (expected_hash, ImageHydrationAttempt::TransientFailure);
                };
                let response = match rpc.get_image_cell(request).await {
                    Ok(response) => response,
                    Err(error) => {
                        log::warn!(
                            "image hydration RPC failed for pane {pane_id}; row remains stale: {error:#}"
                        );
                        // A transport failure is connection-scoped, not a stale
                        // coordinate. Retrying every alternate locator would
                        // multiply the same failure and monopolize the global
                        // byte reservation until the deadline.
                        return (expected_hash, ImageHydrationAttempt::TransientFailure);
                    }
                };
                if response.pane_id != pane_id {
                    record_image_hydration_rejection("response_pane_mismatch");
                    log::warn!(
                        "discarding image hydration response for pane {}; expected {pane_id}",
                        response.pane_id
                    );
                    continue;
                }
                let Some(data) = response.data else {
                    continue;
                };
                return match validate_image_off_main_thread(
                    data,
                    expected_hash,
                    validation_limits,
                )
                .await
                {
                    Ok(validated) => {
                        (expected_hash, ImageHydrationAttempt::Validated(validated))
                    }
                    Err(ImageValidationFailure::Invalid(
                        ImageDataValidationError::ContentRevisionMismatch,
                    )) => {
                        // The coordinate changed after the line snapshot. Try
                        // another occurrence of the same requested revision.
                        continue;
                    }
                    Err(ImageValidationFailure::Invalid(error)) => {
                        record_image_hydration_rejection("decoded_validation");
                        log::warn!(
                            "permanently rejecting invalid decoded image for pane {pane_id}: {error}"
                        );
                        (expected_hash, ImageHydrationAttempt::PermanentFailure)
                    }
                    Err(ImageValidationFailure::Unavailable(error)) => {
                        log::warn!(
                            "image validation unavailable for pane {pane_id}; retrying after backoff: {error}"
                        );
                        (expected_hash, ImageHydrationAttempt::TransientFailure)
                    }
                };
            }
            (expected_hash, ImageHydrationAttempt::TransientFailure)
        })
        .buffer_unordered(MAX_CONCURRENT_IMAGE_HYDRATIONS);
    let mut hydration = Box::pin(hydration);
    loop {
        let Some(next) = await_image_work_until(hydration.next(), deadline).await else {
            record_image_hydration_rejection("deadline");
            log::warn!(
                "image hydration exceeded {:?} for pane {pane_id}; unresolved rows remain stale",
                ORDINARY_IMAGE_HYDRATION_TIMEOUT
            );
            break;
        };
        let Some((expected_hash, attempt)) = next else {
            break;
        };
        unresolved_hashes.remove(&expected_hash);
        let validated = match attempt {
            ImageHydrationAttempt::Validated(validated) => validated,
            ImageHydrationAttempt::PermanentFailure => {
                permanently_failed_hashes.insert(expected_hash);
                if let Some(identity) = connection_identity {
                    lock_image_lru().record_failure(
                        identity,
                        pane_id,
                        expected_hash,
                        CachedImageFailure::Permanent,
                    );
                }
                continue;
            }
            ImageHydrationAttempt::TransientFailure => {
                if let Some(identity) = connection_identity {
                    lock_image_lru().record_failure(
                        identity,
                        pane_id,
                        expected_hash,
                        CachedImageFailure::Transient {
                            retry_after: Instant::now() + Duration::from_millis(500),
                        },
                    );
                }
                continue;
            }
        };
        match cache_and_admit_ordinary_image(
            connection_identity,
            pane_id,
            accepted_decoded_bytes,
            &validated,
        ) {
            OrdinaryImageBatchAdmission::Accepted { next_decoded_bytes } => {
                accepted_decoded_bytes = next_decoded_bytes;
            }
            OrdinaryImageBatchAdmission::BatchDecodedByteLimit => {
                record_image_hydration_rejection("batch_decoded_byte_limit");
                break;
            }
            OrdinaryImageBatchAdmission::DecodedByteOverflow => {
                record_image_hydration_rejection("decoded_byte_overflow");
                break;
            }
            OrdinaryImageBatchAdmission::RevisionChangedBeforeCachePublish => {
                record_image_hydration_rejection("revision_changed_before_cache_publish");
                continue;
            }
        }
        if validated.data.current_content_hash() != expected_hash {
            record_image_hydration_rejection("revision_changed_before_row_publish");
            continue;
        }
        data_by_hash.insert(expected_hash, validated);
    }

    drop(hydration);
    for unresolved in unresolved_hashes {
        if let Some(identity) = connection_identity {
            lock_image_lru().record_failure(
                identity,
                pane_id,
                unresolved,
                CachedImageFailure::Transient {
                    retry_after: Instant::now() + Duration::from_millis(500),
                },
            );
        }
    }

    let line_index_by_row = lines
        .iter()
        .enumerate()
        .map(|(index, (stable_row, _))| (*stable_row, index))
        .collect::<HashMap<_, _>>();
    let unavailable_rows = image_cells
        .iter()
        .filter(|image| !data_by_hash.contains_key(&image.data_hash))
        .map(|image| image.line_idx)
        .collect::<HashSet<_>>();
    let terminal_rows = image_cells
        .iter()
        .filter(|image| permanently_failed_hashes.contains(&image.data_hash))
        .map(|image| image.line_idx)
        .collect::<HashSet<_>>();
    let mut incomplete_rows = rows_requiring_image_retry(&unavailable_rows, &terminal_rows);

    for image in image_cells {
        if unavailable_rows.contains(&image.line_idx) {
            continue;
        }
        let Some(validated) = data_by_hash.get(&image.data_hash) else {
            // Constructing incomplete_rows above made this unreachable. Keep
            // the row transaction fail-closed if lookup semantics evolve.
            incomplete_rows.insert(image.line_idx);
            continue;
        };
        let Some(line_index) = line_index_by_row.get(&image.line_idx).copied() else {
            // validate_structure made this unreachable, but keep the trust
            // boundary fail-closed if its invariants evolve independently.
            record_image_hydration_rejection("validated_row_missing");
            incomplete_rows.insert(image.line_idx);
            continue;
        };
        let Some(cell) = lines[line_index]
            .1
            .cells_mut_for_attr_changes_only()
            .get_mut(image.cell_idx)
        else {
            record_image_hydration_rejection("validated_cell_missing");
            incomplete_rows.insert(image.line_idx);
            continue;
        };
        let (top_left, bottom_right) = image.canonical_texture_coordinates();
        cell.attrs_mut()
            .attach_image(Box::new(ImageCell::with_z_index(
                top_left,
                bottom_right,
                Arc::clone(&validated.data),
                image.z_index,
                image.padding_left,
                image.padding_top,
                image.padding_right,
                image.padding_bottom,
                image.image_id,
                image.placement_id,
            )));
    }

    Ok(HydratedLines {
        lines,
        incomplete_rows,
    })
}

#[allow(
    dead_code,
    reason = "the render-application endpoint is activated by ft-interactive-systems-performance-4tenz.5.5.10"
)]
pub(crate) async fn hydrate_render_application_lines(
    rpc: &RpcGenerationScope,
    pane_id: PaneId,
    serialized_lines: SerializedLines,
    max_unique_image_bytes: usize,
    application_deadline: Instant,
) -> Result<Vec<(StableRowIndex, Line)>, RenderApplicationNackReason> {
    let structure_error = |error| {
        let component = match error {
            SerializedLinesStructureError::HyperlinkLineOutOfRange
            | SerializedLinesStructureError::HyperlinkCellRangeOutOfRange => {
                RenderApplicationComponent::Hyperlinks
            }
            SerializedLinesStructureError::ImageLineMissing
            | SerializedLinesStructureError::ImageCellOutOfRange
            | SerializedLinesStructureError::ImageTextureCoordinatesInvalid => {
                RenderApplicationComponent::Images
            }
            SerializedLinesStructureError::DuplicateStableRow
            | SerializedLinesStructureError::CellCountOverflow => RenderApplicationComponent::Lines,
        };
        RenderApplicationNackReason::MalformedOrIncomplete { component }
    };
    let counts = serialized_lines
        .validate_structure()
        .map_err(structure_error)?;
    let bounded_resource = |resource, requested: usize, limit: usize| {
        RenderApplicationNackReason::BoundedResourceRejected {
            resource,
            requested: u64::try_from(requested).unwrap_or(u64::MAX),
            limit: u64::try_from(limit).unwrap_or(u64::MAX),
        }
    };
    if counts.lines > MAX_RENDER_APPLICATION_LINES {
        return Err(bounded_resource(
            RenderApplicationResource::Lines,
            counts.lines,
            MAX_RENDER_APPLICATION_LINES,
        ));
    }
    if counts.cells > MAX_RENDER_APPLICATION_CELLS {
        return Err(bounded_resource(
            RenderApplicationResource::Cells,
            counts.cells,
            MAX_RENDER_APPLICATION_CELLS,
        ));
    }
    if counts.hyperlink_spans > MAX_RENDER_APPLICATION_HYPERLINK_SPANS {
        return Err(bounded_resource(
            RenderApplicationResource::Hyperlinks,
            counts.hyperlink_spans,
            MAX_RENDER_APPLICATION_HYPERLINK_SPANS,
        ));
    }
    if counts.images > MAX_RENDER_APPLICATION_IMAGE_REFERENCES {
        return Err(bounded_resource(
            RenderApplicationResource::Images,
            counts.images,
            MAX_RENDER_APPLICATION_IMAGE_REFERENCES,
        ));
    }
    let (mut lines, image_cells) = serialized_lines.extract_data();

    if Instant::now() >= application_deadline {
        return Err(RenderApplicationNackReason::ApplicationFailure {
            stage: RenderApplicationStage::Hydrate,
        });
    }
    if image_cells.is_empty() {
        return Ok(lines);
    }

    let connection_identity = rpc.render_connection_identity();
    let mut requests = Vec::<([u8; 32], Vec<GetImageCell>)>::new();
    let mut request_indices = HashMap::<[u8; 32], usize>::new();
    let mut data_by_hash = HashMap::<[u8; 32], ValidatedImageData>::new();
    let mut unique_image_bytes = 0usize;
    for image in &image_cells {
        if Instant::now() >= application_deadline {
            return Err(RenderApplicationNackReason::ApplicationFailure {
                stage: RenderApplicationStage::Hydrate,
            });
        }
        if data_by_hash.contains_key(&image.data_hash) {
            continue;
        }
        if request_indices.contains_key(&image.data_hash) {
            push_image_locator(
                &mut requests,
                &mut request_indices,
                image.data_hash,
                GetImageCell {
                    pane_id,
                    line_idx: image.line_idx,
                    cell_idx: image.cell_idx,
                    data_hash: image.data_hash,
                },
            );
            continue;
        }
        let cached = connection_identity
            .and_then(|identity| get_cached_validated_image(identity, pane_id, &image.data_hash));
        if let Some(validated) = cached {
            unique_image_bytes = unique_image_bytes
                .checked_add(validated.decoded_bytes)
                .ok_or(RenderApplicationNackReason::BoundedResourceRejected {
                    resource: RenderApplicationResource::Images,
                    requested: u64::MAX,
                    limit: u64::try_from(max_unique_image_bytes).unwrap_or(u64::MAX),
                })?;
            if unique_image_bytes > max_unique_image_bytes {
                return Err(RenderApplicationNackReason::BoundedResourceRejected {
                    resource: RenderApplicationResource::Images,
                    requested: u64::try_from(unique_image_bytes).unwrap_or(u64::MAX),
                    limit: u64::try_from(max_unique_image_bytes).unwrap_or(u64::MAX),
                });
            }
            data_by_hash.insert(image.data_hash, validated);
        } else if let Some(failure) = connection_identity.and_then(|identity| {
            lock_image_lru().get_failure(identity, pane_id, &image.data_hash, Instant::now())
        }) {
            return Err(match failure {
                CachedImageFailure::Permanent => {
                    RenderApplicationNackReason::MalformedOrIncomplete {
                        component: RenderApplicationComponent::Images,
                    }
                }
                CachedImageFailure::Transient { .. } => {
                    RenderApplicationNackReason::ApplicationFailure {
                        stage: RenderApplicationStage::Hydrate,
                    }
                }
            });
        } else {
            push_image_locator(
                &mut requests,
                &mut request_indices,
                image.data_hash,
                GetImageCell {
                    pane_id,
                    line_idx: image.line_idx,
                    cell_idx: image.cell_idx,
                    data_hash: image.data_hash,
                },
            );
        }
    }

    let validation_limits = ImageDataValidationLimits {
        max_decoded_bytes: max_unique_image_bytes,
        max_frame_count: MAX_DECODED_IMAGE_FRAMES,
        max_width: MAX_RENDERABLE_IMAGE_AXIS,
        max_height: MAX_RENDERABLE_IMAGE_AXIS,
    };
    let mut unresolved_hashes = requests
        .iter()
        .map(|(hash, _)| *hash)
        .collect::<HashSet<_>>();
    let hydration = stream::iter(requests)
        .map(|(hash, locators)| async move {
            let result = async {
                for request in locators {
                    let Some(_byte_permit) =
                        acquire_image_hydration_bytes_until(application_deadline).await
                    else {
                        return Err((
                            RenderApplicationNackReason::ApplicationFailure {
                                stage: RenderApplicationStage::Hydrate,
                            },
                            CachedImageFailure::Transient {
                                retry_after: Instant::now() + Duration::from_millis(500),
                            },
                        ));
                    };
                    let response = rpc.get_image_cell(request).await.map_err(|_| {
                        (
                            RenderApplicationNackReason::ApplicationFailure {
                                stage: RenderApplicationStage::Hydrate,
                            },
                            CachedImageFailure::Transient {
                                retry_after: Instant::now() + Duration::from_millis(500),
                            },
                        )
                    })?;
                    if response.pane_id != pane_id {
                        return Err((
                            RenderApplicationNackReason::MalformedOrIncomplete {
                                component: RenderApplicationComponent::Images,
                            },
                            CachedImageFailure::Transient {
                                retry_after: Instant::now() + Duration::from_millis(500),
                            },
                        ));
                    }
                    let Some(data) = response.data else {
                        continue;
                    };
                    let validated =
                        match validate_image_off_main_thread(data, hash, validation_limits).await {
                            Ok(validated) => validated,
                            Err(ImageValidationFailure::Invalid(
                                ImageDataValidationError::ContentRevisionMismatch,
                            )) => continue,
                            Err(ImageValidationFailure::Invalid(
                                ImageDataValidationError::DecodedByteLimitExceeded {
                                    requested,
                                    limit,
                                }
                                | ImageDataValidationError::EncodedByteLimitExceeded {
                                    requested,
                                    limit,
                                }
                                | ImageDataValidationError::FrameCountLimitExceeded {
                                    requested,
                                    limit,
                                },
                            )) => {
                                return Err((
                                    RenderApplicationNackReason::BoundedResourceRejected {
                                        resource: RenderApplicationResource::Images,
                                        requested: u64::try_from(requested).unwrap_or(u64::MAX),
                                        limit: u64::try_from(limit).unwrap_or(u64::MAX),
                                    },
                                    CachedImageFailure::Permanent,
                                ));
                            }
                            Err(ImageValidationFailure::Invalid(_)) => {
                                return Err((
                                    RenderApplicationNackReason::MalformedOrIncomplete {
                                        component: RenderApplicationComponent::Images,
                                    },
                                    CachedImageFailure::Permanent,
                                ));
                            }
                            Err(ImageValidationFailure::Unavailable(_)) => {
                                return Err((
                                    RenderApplicationNackReason::ApplicationFailure {
                                        stage: RenderApplicationStage::Hydrate,
                                    },
                                    CachedImageFailure::Transient {
                                        retry_after: Instant::now() + Duration::from_millis(500),
                                    },
                                ));
                            }
                        };
                    return Ok(validated);
                }
                Err((
                    RenderApplicationNackReason::MalformedOrIncomplete {
                        component: RenderApplicationComponent::Images,
                    },
                    CachedImageFailure::Transient {
                        retry_after: Instant::now() + Duration::from_millis(500),
                    },
                ))
            }
            .await;
            (hash, result)
        })
        .buffer_unordered(MAX_CONCURRENT_IMAGE_HYDRATIONS);
    let mut hydration = Box::pin(hydration);
    let mut fetched = Vec::new();
    loop {
        let Some(next) = await_image_work_until(hydration.next(), application_deadline).await
        else {
            if let Some(identity) = connection_identity {
                for hash in unresolved_hashes {
                    lock_image_lru().record_failure(
                        identity,
                        pane_id,
                        hash,
                        CachedImageFailure::Transient {
                            retry_after: Instant::now() + Duration::from_millis(500),
                        },
                    );
                }
            }
            return Err(RenderApplicationNackReason::ApplicationFailure {
                stage: RenderApplicationStage::Hydrate,
            });
        };
        let Some((hash, result)) = next else {
            break;
        };
        unresolved_hashes.remove(&hash);
        let validated = match result {
            Ok(validated) => validated,
            Err((reason, failure)) => {
                if let Some(identity) = connection_identity {
                    lock_image_lru().record_failure(identity, pane_id, hash, failure);
                }
                return Err(reason);
            }
        };
        unique_image_bytes = unique_image_bytes
            .checked_add(validated.decoded_bytes)
            .ok_or(RenderApplicationNackReason::BoundedResourceRejected {
                resource: RenderApplicationResource::Images,
                requested: u64::MAX,
                limit: u64::try_from(max_unique_image_bytes).unwrap_or(u64::MAX),
            })?;
        if unique_image_bytes > max_unique_image_bytes {
            return Err(RenderApplicationNackReason::BoundedResourceRejected {
                resource: RenderApplicationResource::Images,
                requested: u64::try_from(unique_image_bytes).unwrap_or(u64::MAX),
                limit: u64::try_from(max_unique_image_bytes).unwrap_or(u64::MAX),
            });
        }
        fetched.push((hash, validated));
    }
    drop(hydration);

    // Publish only after the complete fetched set has passed validation and
    // the aggregate byte budget. A rejected attempt must not leave a
    // nondeterministic partial cache prefix.
    for (hash, validated) in fetched {
        if validated.data.current_content_hash() != hash {
            return Err(RenderApplicationNackReason::MalformedOrIncomplete {
                component: RenderApplicationComponent::Images,
            });
        }
        if let Some(identity) = connection_identity {
            if !put_cached_validated_image(identity, pane_id, validated.clone()) {
                return Err(RenderApplicationNackReason::MalformedOrIncomplete {
                    component: RenderApplicationComponent::Images,
                });
            }
        }
        data_by_hash.insert(hash, validated);
    }

    let line_index_by_row = lines
        .iter()
        .enumerate()
        .map(|(index, (stable_row, _))| (*stable_row, index))
        .collect::<HashMap<_, _>>();
    for image in image_cells {
        let validated = data_by_hash.get(&image.data_hash).ok_or(
            RenderApplicationNackReason::MalformedOrIncomplete {
                component: RenderApplicationComponent::Images,
            },
        )?;
        let line_index = *line_index_by_row.get(&image.line_idx).ok_or(
            RenderApplicationNackReason::MalformedOrIncomplete {
                component: RenderApplicationComponent::Images,
            },
        )?;
        let cell = lines[line_index]
            .1
            .cells_mut_for_attr_changes_only()
            .get_mut(image.cell_idx)
            .ok_or(RenderApplicationNackReason::MalformedOrIncomplete {
                component: RenderApplicationComponent::Images,
            })?;
        let (top_left, bottom_right) = image.canonical_texture_coordinates();
        cell.attrs_mut()
            .attach_image(Box::new(ImageCell::with_z_index(
                top_left,
                bottom_right,
                Arc::clone(&validated.data),
                image.z_index,
                image.padding_left,
                image.padding_top,
                image.padding_right,
                image.padding_bottom,
                image.image_id,
                image.placement_id,
            )));
    }

    Ok(lines)
}

impl RenderableState {
    pub fn get_cursor_position(&self) -> StableCursorPosition {
        self.inner.borrow().cursor_position
    }

    /// Return a single coherent cache snapshot after normalizing every complete
    /// logical group touched by `lines` with the caller's exact rule set.
    pub fn get_lines_with_hyperlinks(
        &self,
        lines: Range<StableRowIndex>,
        rules: &[Rule],
    ) -> (StableRowIndex, Vec<Line>) {
        self.inner
            .borrow_mut()
            .normalize_implicit_hyperlinks_for_request(lines.clone(), rules);
        self.get_lines(lines)
    }

    /// Retain renderer-produced line appdata on the authoritative remote cache
    /// only when the projected line still exactly matches the cached content.
    ///
    /// Remote panes render independent `Line` clones. Without this explicit
    /// write-back, shape hashes installed on those clones disappear after each
    /// paint. Equality guards reject predictive-echo, lag-indicator, overlay,
    /// or stale-response projections, so metadata computed for modified output
    /// can never poison the source line.
    pub fn write_back_unchanged_line_appdata(
        &self,
        first_row: StableRowIndex,
        rendered_lines: &[Line],
    ) {
        let mut inner = self.inner.borrow_mut();
        for (offset, rendered) in rendered_lines.iter().enumerate() {
            let Some(stable_row) = StableRowIndex::try_from(offset)
                .ok()
                .and_then(|offset| first_row.checked_add(offset))
            else {
                break;
            };
            let Some(entry) = inner.lines.get_mut(&stable_row) else {
                continue;
            };
            let cached = match entry {
                LineEntry::Line(line)
                | LineEntry::LineAndFetching(line, _)
                | LineEntry::Stale(line) => line,
                LineEntry::Fetching(_) => continue,
            };
            if cached == rendered {
                cached.copy_appdata_from(rendered);
            }
        }
    }

    pub fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let mut inner = self.inner.borrow_mut();
        let mut result = vec![];
        let mut to_fetch = RangeSet::new();
        let now = Instant::now();
        let mut fetch_token = None;

        for idx in lines.clone() {
            let entry = match inner.lines.pop(&idx) {
                Some(LineEntry::Line(line)) => {
                    result.push(line.clone());
                    if line.changed_since(inner.seqno) {
                        to_fetch.add(idx);
                        let token = fetch_token
                            .get_or_insert_with(|| FetchToken::new(now))
                            .clone();
                        LineEntry::LineAndFetching(line, token)
                    } else {
                        LineEntry::Line(line)
                    }
                }
                Some(LineEntry::LineAndFetching(line, then)) => {
                    result.push(line.clone());
                    LineEntry::LineAndFetching(line, then)
                }
                Some(LineEntry::Fetching(then)) => {
                    result.push(Line::with_width(inner.dimensions.cols, SEQ_ZERO));
                    LineEntry::Fetching(then)
                }
                Some(LineEntry::Stale(line)) => {
                    result.push(line.clone());
                    to_fetch.add(idx);
                    let token = fetch_token
                        .get_or_insert_with(|| FetchToken::new(now))
                        .clone();
                    LineEntry::LineAndFetching(line, token)
                }
                None => {
                    result.push(Line::with_width(inner.dimensions.cols, SEQ_ZERO));
                    to_fetch.add(idx);
                    let token = fetch_token
                        .get_or_insert_with(|| FetchToken::new(now))
                        .clone();
                    LineEntry::Fetching(token)
                }
            };

            if inner.client.overlay_lag_indicator
                && idx == inner.dimensions.physical_top
                && inner.is_tardy()
            {
                let status = format!(
                    "FrankenTerm: {:.0?}⏳since last response",
                    inner.last_recv_time.elapsed()
                );
                // Right align it in the tab
                let col = inner
                    .dimensions
                    .cols
                    .saturating_sub(wezterm_term::unicode_column_width(&status, None));

                let mut attr = CellAttributes::default();
                attr.set_foreground(AnsiColor::White);
                attr.set_background(AnsiColor::Blue);

                if let Some(line) = result.last_mut() {
                    line.overlay_text_with_attribute(col, &status, attr, SEQ_ZERO);
                }
            }

            inner.lines.put(idx, entry);
        }

        // Drop stale predictions that were never confirmed (e.g. an app that
        // suppresses echo) so the overlay can't get stuck, then paint the live
        // predictions onto the cloned result lines. Predictions live separately
        // from cached server content (recorded, not mutated in), which is what
        // later lets 3b validate/rewind them and 3d resolve the underline cue from
        // prediction state at render time. [3a]
        // Expire never-confirmed predictions, scaled by the measured RTT: a fixed 1s
        // would drop them before the echo arrives on a >1s link (the high-latency
        // case the feature is for), while on fast links a short TTL keeps the
        // stale-overpaint window (a unilateral redraw under a stale prediction)
        // small. [review F3/F4]
        let _ = inner.expire_stale_predictions(now);
        if !inner.predictions.is_empty() {
            let (start, end) = (lines.start, lines.end);
            // Glitchless flagging: once predictions are reliably confirmed correct
            // (high confidence) paint them plain so correct echo is seamless; while
            // confidence is still building, flag each with the underline cue. [3d]
            let confident = inner.prediction_score >= PREDICT_CONFIDENT_SCORE;
            for p in &inner.predictions {
                if p.row >= start && p.row < end {
                    let Some(offset) = p
                        .row
                        .checked_sub(start)
                        .and_then(|offset| usize::try_from(offset).ok())
                    else {
                        continue;
                    };
                    if let Some(line) = result.get_mut(offset) {
                        let mut cell = p.predicted.clone();
                        if !confident {
                            cell.attrs_mut().set_underline(Underline::Double);
                        }
                        line.set_cell(p.col, cell, SEQ_ZERO);
                    }
                }
            }
        }

        // Speculative read-ahead: prefetch ~one viewport above and below the visible
        // range so a page scroll finds those rows already cached instead of stalling
        // ~1 RTT per viewport-fill on a high-latency link. Reuses the same LineEntry
        // state machine (rows already fresh or in flight are skipped: dedupe) and the
        // seqno staleness guard, so a widened fetch stays one range-batched GetLines
        // RPC. A cheap peek decides whether any read-ahead row is actually missing
        // before spending a fetch-limiter token, so a stationary viewport (everything
        // cached) costs nothing and never steals fetch budget from on-screen updates.
        // Off-screen rows join `to_fetch` but never `result` (they are not displayed).
        // [prefetch]
        let viewport_span = StableRowIndex::try_from(inner.dimensions.viewport_rows)
            .unwrap_or(StableRowIndex::MAX)
            .max(1);
        let span = lines
            .end
            .saturating_sub(lines.start)
            .clamp(1, viewport_span);
        let lo = lines.start.saturating_sub(span);
        let hi = lines.end.saturating_add(span);
        let needs_prefetch =
            (lo..lines.start)
                .chain(lines.end..hi)
                .any(|idx| match inner.lines.peek(&idx) {
                    None | Some(LineEntry::Stale(_)) => true,
                    Some(LineEntry::Line(line)) => line.changed_since(inner.seqno),
                    Some(LineEntry::Fetching(_)) | Some(LineEntry::LineAndFetching(..)) => false,
                });
        if needs_prefetch && inner.fetch_limiter.non_blocking_admittance_check(1) {
            for idx in (lo..lines.start).chain(lines.end..hi) {
                match inner.lines.pop(&idx) {
                    Some(LineEntry::Line(line)) => {
                        if line.changed_since(inner.seqno) {
                            to_fetch.add(idx);
                            let token = fetch_token
                                .get_or_insert_with(|| FetchToken::new(now))
                                .clone();
                            inner
                                .lines
                                .put(idx, LineEntry::LineAndFetching(line, token));
                        } else {
                            inner.lines.put(idx, LineEntry::Line(line));
                        }
                    }
                    Some(LineEntry::Stale(line)) => {
                        to_fetch.add(idx);
                        let token = fetch_token
                            .get_or_insert_with(|| FetchToken::new(now))
                            .clone();
                        inner
                            .lines
                            .put(idx, LineEntry::LineAndFetching(line, token));
                    }
                    // Already in flight (Fetching / LineAndFetching): dedupe -> leave.
                    Some(other) => {
                        inner.lines.put(idx, other);
                    }
                    None => {
                        to_fetch.add(idx);
                        let token = fetch_token
                            .get_or_insert_with(|| FetchToken::new(now))
                            .clone();
                        inner.lines.put(idx, LineEntry::Fetching(token));
                    }
                }
            }
        }

        log::trace!(
            "get_lines: {:?}, num result lines={}, will fetch {:?}",
            lines,
            result.len(),
            to_fetch
        );

        if let Some(fetch_token) = fetch_token {
            inner.schedule_fetch_lines(to_fetch, fetch_token);
        } else {
            debug_assert!(to_fetch.is_empty());
        }
        (lines.start, result)
    }

    pub fn get_current_seqno(&self) -> SequenceNo {
        self.inner.borrow().seqno
    }

    pub fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        let mut inner = self.inner.borrow_mut();
        Self::poll_before_changed_query(&mut inner);
        Self::collect_changed_since(&mut inner, lines, seqno)
    }

    /// Poll the remote source, capture its post-poll sequence fence, and scan
    /// changed rows within the same `RenderableInner` borrow. Capturing the
    /// fence before `poll` would make every delta applied by that poll appear
    /// dirty again on the next renderer query.
    pub fn get_changed_since_with_source_fence(
        &self,
        lines: Range<StableRowIndex>,
        last_observed_source_end: SequenceNo,
    ) -> (SequenceNo, RangeSet<StableRowIndex>) {
        let mut inner = self.inner.borrow_mut();
        Self::poll_before_changed_query(&mut inner);
        let source_end = inner.seqno;
        let baseline =
            mux::pane::changed_since_query_baseline(last_observed_source_end, source_end);
        let changed = Self::collect_changed_since(&mut inner, lines, baseline);
        (source_end, changed)
    }

    fn poll_before_changed_query(inner: &mut RenderableInner) {
        if let Err(err) = inner.poll() {
            // We allow for BrokenPromise here for now; for a TLS backed
            // session it indicates that we'll retry.  For a local unix
            // domain session it is terminal... but we will detect that
            // terminal condition elsewhere
            if let Err(err) = err.downcast::<BrokenPromise>() {
                log::error!("remote tab poll failed: {}, marking as dead", err);
                inner.dead = true;
            }
        }
    }

    fn collect_changed_since(
        inner: &mut RenderableInner,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        let mut result = RangeSet::new();
        let expired_prediction_rows = inner.expire_stale_predictions(Instant::now());
        for expired_range in expired_prediction_rows.iter() {
            for row in expired_range.clone() {
                if lines.contains(&row) {
                    result.add(row);
                }
            }
        }
        for r in lines {
            match inner.lines.get(&r) {
                None => {
                    result.add(r);
                }
                Some(
                    LineEntry::Line(line)
                    | LineEntry::Stale(line)
                    | LineEntry::LineAndFetching(line, _),
                ) if line.changed_since(seqno) => {
                    result.add(r);
                }
                _ => {}
            }
        }

        // If we're behind receiving an update, invalidate the top row so
        // that the indicator will update in a more timely fashion
        if inner.is_tardy() {
            // ... but take care to avoid always reporting it as dirty, so
            // that we don't end up busy looping just to repaint it
            if inner.last_late_dirty.elapsed() >= Duration::from_secs(1) {
                result.add(inner.dimensions.physical_top);
                inner.last_late_dirty = Instant::now();
            }
        }

        if !result.is_empty() {
            log::trace!("get_changed_since: {} -> {:?}", seqno, result);
        }

        result
    }

    pub fn get_dimensions(&self) -> RenderableDimensions {
        self.inner.borrow().dimensions
    }

    pub fn get_tiered_scrollback_status(&self) -> Option<PaneTieredScrollbackStatus> {
        self.inner.borrow().tiered_scrollback_status
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_prediction_reconciliation_to_score, base_poll_interval,
        cache_and_admit_ordinary_image, expire_predictions, get_cached_validated_image,
        initial_last_poll, mark_predictions_dispatched, paste_fits_prediction_budget,
        push_bounded_prediction, push_image_locator, rebuild_cache_as_stale,
        reconcile_predictions_after_cached_terminal_change,
        reconcile_predictions_after_terminal_change, render_line_cache_capacity_for_values,
        reset_prediction_state, rows_requiring_image_retry, should_apply_unilateral_delta,
        try_release_atomic, try_reserve_bounded_atomic, CachedImageFailure, FetchToken, ImageLru,
        LineEntry, OrdinaryImageBatchAdmission, Prediction, PredictionReconciliation,
        ValidatedImageData, IMAGE_HYDRATION_WORKING_SET_RESERVATION_BYTES,
        MAX_GLOBAL_IMAGE_HYDRATION_WORKING_SET_BYTES, MAX_IMAGE_LOCATOR_ATTEMPTS_PER_REVISION,
        MAX_ORDINARY_IMAGE_BATCH_BYTES, MAX_PENDING_PREDICTIONS, PREDICT_CONFIDENT_SCORE,
    };
    use crate::client::Client;
    use crate::client::TEST_RENDER_CONNECTION_IDENTITY;
    use crate::domain::{ClientDomainConfig, ClientInner};
    use crate::pane::ClientPane;
    use codec::{
        GetImageCell, InputSerial, RenderConnectionIdentity, TopologyStreamId,
        MAX_GET_IMAGE_CELL_RESPONSE_DECOMPRESSED_BYTES, MAX_IMAGE_HYDRATION_DECODED_BYTES,
    };
    use config::UnixDomain;
    use lru::LruCache;
    use mux::MuxSessionIncarnation;
    use std::collections::{HashMap, HashSet};
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use termwiz::cell::{Cell, CellAttributes};
    use termwiz::hyperlink::Rule;
    use termwiz::image::{ImageData, ImageDataType};
    use termwiz::surface::{SequenceNo, SEQ_ZERO};
    use wezterm_term::Line;
    use wezterm_term::{KeyCode, KeyModifiers};

    #[test]
    fn bounded_atomic_reservation_preserves_limit_overflow_and_release_boundaries() {
        let counter = AtomicUsize::new(0);

        assert!(try_reserve_bounded_atomic(&counter, 2, 2));
        assert_eq!(counter.load(Ordering::Acquire), 2);
        assert!(
            !try_reserve_bounded_atomic(&counter, 1, 2),
            "a counter already at its limit must reject another reservation"
        );
        assert_eq!(counter.load(Ordering::Acquire), 2);
        assert!(
            !try_release_atomic(&counter, 3),
            "an underflowing release must fail without wrapping"
        );
        assert_eq!(counter.load(Ordering::Acquire), 2);
        assert!(try_release_atomic(&counter, 2));
        assert_eq!(counter.load(Ordering::Acquire), 0);

        assert!(try_reserve_bounded_atomic(&counter, 0, 0));
        assert!(try_release_atomic(&counter, 0));
        assert_eq!(counter.load(Ordering::Acquire), 0);

        counter.store(usize::MAX, Ordering::Release);
        assert!(
            !try_reserve_bounded_atomic(&counter, 1, usize::MAX),
            "checked addition must reject arithmetic overflow"
        );
        assert_eq!(counter.load(Ordering::Acquire), usize::MAX);
    }

    fn test_renderable_state_with_echo_threshold(
        local_echo_threshold_ms: Option<u64>,
    ) -> Arc<parking_lot::Mutex<super::RenderableState>> {
        let domain_id = 731;
        let unix = UnixDomain {
            name: "renderable-hyperlink-test".to_string(),
            ..UnixDomain::default()
        };
        let client = Arc::new(ClientInner::new(
            domain_id,
            Client::new_test_client(Some(domain_id), ClientDomainConfig::Unix(unix)),
            None,
            local_echo_threshold_ms,
            false,
        ));
        let pane = ClientPane::new(
            &client,
            733,
            739,
            743,
            wezterm_term::TerminalSize {
                cols: 80,
                rows: 24,
                pixel_width: 800,
                pixel_height: 480,
                dpi: 96,
            },
            "hyperlink-test",
            false,
        );
        Arc::clone(&pane.renderable)
    }

    fn test_renderable_state() -> Arc<parking_lot::Mutex<super::RenderableState>> {
        test_renderable_state_with_echo_threshold(None)
    }

    fn test_image(width: u32, height: u32, fill: u8) -> Arc<ImageData> {
        Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            width,
            height,
            vec![fill; (width * height * 4) as usize],
        )))
    }

    fn validated_test_image(width: u32, height: u32, fill: u8) -> ValidatedImageData {
        let data = test_image(width, height, fill);
        ValidatedImageData {
            decoded_bytes: data.len(),
            source_revision: data.current_content_hash(),
            data,
        }
    }

    fn input_serial(millis: u64) -> InputSerial {
        InputSerial::from_millis_since_epoch(millis)
    }

    fn prediction(
        serial: InputSerial,
        row: isize,
        col: usize,
        glyph: char,
        born: Instant,
    ) -> Prediction {
        Prediction {
            row,
            col,
            predicted: Cell::new(glyph, CellAttributes::default()),
            input_serial: serial,
            dispatch_seqno: None,
            born,
        }
    }

    #[test]
    fn initial_last_poll_allows_immediate_first_poll() {
        let now = Instant::now();
        let initial = initial_last_poll(now);

        assert!(now.duration_since(initial) >= base_poll_interval());
    }

    #[test]
    fn paste_prediction_rejects_row_domain_overflow_before_mutation() {
        let renderable = test_renderable_state_with_echo_threshold(Some(0));
        let state = renderable.lock();
        let mut inner = state.inner.borrow_mut();
        inner.last_input_rtt = 1;
        inner.cursor_position.y = wezterm_term::StableRowIndex::MAX;

        inner.predict_from_paste("x\nx");

        assert!(inner.predictions.is_empty());
        assert_eq!(inner.cursor_position.y, wezterm_term::StableRowIndex::MAX);
        assert_eq!(inner.prediction_score, super::PREDICT_SUPPRESS_SCORE);
    }

    #[test]
    fn key_prediction_rejects_cursor_domain_overflow_before_mutation() {
        let renderable = test_renderable_state_with_echo_threshold(Some(0));
        let state = renderable.lock();
        let mut inner = state.inner.borrow_mut();
        inner.last_input_rtt = 1;
        inner.cursor_position.x = 7;
        inner.cursor_position.y = wezterm_term::StableRowIndex::MAX;
        inner.lines.put(
            wezterm_term::StableRowIndex::MAX,
            LineEntry::Line(Line::from("ordinary prompt")),
        );

        inner.predict_from_key_event(KeyCode::Enter, KeyModifiers::NONE);

        assert!(inner.predictions.is_empty());
        assert_eq!(inner.cursor_position.x, 7);
        assert_eq!(inner.cursor_position.y, wezterm_term::StableRowIndex::MAX);
        assert_eq!(inner.prediction_score, super::PREDICT_SUPPRESS_SCORE);
    }

    #[test]
    fn typed_key_prediction_rejects_column_domain_overflow_before_recording() {
        let renderable = test_renderable_state_with_echo_threshold(Some(0));
        let state = renderable.lock();
        let mut inner = state.inner.borrow_mut();
        inner.last_input_rtt = 1;
        inner.cursor_position.x = usize::MAX;
        inner.cursor_position.y = 0;
        inner
            .lines
            .put(0, LineEntry::Line(Line::from("ordinary prompt")));

        inner.predict_from_key_event(KeyCode::Char('x'), KeyModifiers::NONE);

        assert!(inner.predictions.is_empty());
        assert_eq!(inner.cursor_position.x, usize::MAX);
        assert_eq!(inner.cursor_position.y, 0);
        assert_eq!(inner.prediction_score, super::PREDICT_SUPPRESS_SCORE);
    }

    #[test]
    fn typed_key_prediction_rejects_unrepresentable_pending_wrap_at_right_margin() {
        let renderable = test_renderable_state_with_echo_threshold(Some(0));
        let state = renderable.lock();
        let mut inner = state.inner.borrow_mut();
        inner.last_input_rtt = 1;
        inner.cursor_position.x = inner.dimensions.cols.saturating_sub(1);
        inner.cursor_position.y = 0;
        let original_cursor = inner.cursor_position;
        inner
            .lines
            .put(0, LineEntry::Line(Line::from("ordinary prompt")));

        inner.predict_from_key_event(KeyCode::Char('x'), KeyModifiers::NONE);

        assert!(inner.predictions.is_empty());
        assert_eq!(inner.cursor_position, original_cursor);
        assert_eq!(inner.prediction_score, super::PREDICT_SUPPRESS_SCORE);
    }

    #[test]
    fn typed_key_prediction_rejects_stateful_zero_width_scalar() {
        let renderable = test_renderable_state_with_echo_threshold(Some(0));
        let state = renderable.lock();
        let mut inner = state.inner.borrow_mut();
        inner.last_input_rtt = 1;
        inner.cursor_position.x = 1;
        inner.cursor_position.y = 0;
        let original_cursor = inner.cursor_position;
        inner
            .lines
            .put(0, LineEntry::Line(Line::from("ordinary prompt")));

        inner.predict_from_key_event(KeyCode::Char('\u{0301}'), KeyModifiers::NONE);

        assert!(inner.predictions.is_empty());
        assert_eq!(inner.cursor_position, original_cursor);
        assert_eq!(inner.prediction_score, super::PREDICT_SUPPRESS_SCORE);
    }

    #[test]
    fn paste_prediction_rejects_column_domain_overflow_transactionally() {
        let renderable = test_renderable_state_with_echo_threshold(Some(0));
        let state = renderable.lock();
        let mut inner = state.inner.borrow_mut();
        inner.last_input_rtt = 1;
        inner.cursor_position.x = usize::MAX;
        inner.cursor_position.y = 0;
        inner
            .lines
            .put(0, LineEntry::Line(Line::from("ordinary prompt")));

        inner.predict_from_paste("x");

        assert!(inner.predictions.is_empty());
        assert_eq!(inner.cursor_position.x, usize::MAX);
        assert_eq!(inner.cursor_position.y, 0);
        assert_eq!(inner.prediction_score, super::PREDICT_SUPPRESS_SCORE);
    }

    #[test]
    fn paste_prediction_preserves_explicit_rows_and_tracks_uncached_final_column() {
        let renderable = test_renderable_state_with_echo_threshold(Some(0));
        let state = renderable.lock();
        let mut inner = state.inner.borrow_mut();
        inner.last_input_rtt = 1;
        inner
            .lines
            .put(0, LineEntry::Line(Line::from("ordinary prompt")));

        inner.predict_from_paste("a\nbc");

        assert_eq!(inner.predictions.len(), 1);
        assert_eq!(inner.predictions[0].row, 0);
        assert_eq!(inner.predictions[0].predicted.str(), "a");
        assert_eq!(inner.cursor_position.x, 2);
        assert_eq!(inner.cursor_position.y, 1);
    }

    #[test]
    fn paste_prediction_rejects_implicit_word_wrap_instead_of_reflowing_text() {
        let renderable = test_renderable_state_with_echo_threshold(Some(0));
        let state = renderable.lock();
        let mut inner = state.inner.borrow_mut();
        inner.last_input_rtt = 1;
        inner.cursor_position.x = inner.dimensions.cols.saturating_sub(2);
        let original_cursor = inner.cursor_position;
        inner
            .lines
            .put(0, LineEntry::Line(Line::from("ordinary prompt")));

        inner.predict_from_paste("a b");

        assert!(inner.predictions.is_empty());
        assert_eq!(inner.cursor_position, original_cursor);
        assert_eq!(inner.prediction_score, super::PREDICT_SUPPRESS_SCORE);
    }

    #[test]
    fn paste_prediction_rejects_unrepresentable_pending_wrap_at_right_margin() {
        let renderable = test_renderable_state_with_echo_threshold(Some(0));
        let state = renderable.lock();
        let mut inner = state.inner.borrow_mut();
        inner.last_input_rtt = 1;
        inner.cursor_position.x = inner.dimensions.cols.saturating_sub(1);
        let original_cursor = inner.cursor_position;
        inner
            .lines
            .put(0, LineEntry::Line(Line::from("ordinary prompt")));

        inner.predict_from_paste("x");

        assert!(inner.predictions.is_empty());
        assert_eq!(inner.cursor_position, original_cursor);
        assert_eq!(inner.prediction_score, super::PREDICT_SUPPRESS_SCORE);
    }

    #[test]
    fn paste_prediction_rejects_stateful_control_characters() {
        let renderable = test_renderable_state_with_echo_threshold(Some(0));
        let state = renderable.lock();
        let mut inner = state.inner.borrow_mut();
        inner.last_input_rtt = 1;
        inner
            .lines
            .put(0, LineEntry::Line(Line::from("ordinary prompt")));

        inner.predict_from_paste("left\tright");

        assert!(inner.predictions.is_empty());
        assert_eq!(inner.cursor_position.x, 0);
        assert_eq!(inner.cursor_position.y, 0);
        assert_eq!(inner.prediction_score, super::PREDICT_SUPPRESS_SCORE);
    }

    #[test]
    fn paste_prediction_rejects_unattached_zero_width_cell() {
        let renderable = test_renderable_state_with_echo_threshold(Some(0));
        let state = renderable.lock();
        let mut inner = state.inner.borrow_mut();
        inner.last_input_rtt = 1;
        inner.cursor_position.x = 1;
        let original_cursor = inner.cursor_position;
        inner
            .lines
            .put(0, LineEntry::Line(Line::from("ordinary prompt")));

        inner.predict_from_paste("\u{0301}");

        assert!(inner.predictions.is_empty());
        assert_eq!(inner.cursor_position, original_cursor);
        assert_eq!(inner.prediction_score, super::PREDICT_SUPPRESS_SCORE);
    }

    #[test]
    fn paste_prediction_accepts_combining_mark_attached_to_visible_base() {
        let renderable = test_renderable_state_with_echo_threshold(Some(0));
        let state = renderable.lock();
        let mut inner = state.inner.borrow_mut();
        inner.last_input_rtt = 1;
        inner
            .lines
            .put(0, LineEntry::Line(Line::from("ordinary prompt")));

        inner.predict_from_paste("e\u{0301}");

        assert_eq!(inner.predictions.len(), 1);
        assert_eq!(inner.predictions[0].predicted.str(), "e\u{0301}");
        assert_eq!(inner.cursor_position.x, 1);
        assert_eq!(inner.cursor_position.y, 0);
    }

    #[test]
    fn cached_hyperlinks_wait_for_complete_logical_group_and_honor_exact_rules() {
        let renderable = test_renderable_state();
        let first_rules =
            vec![Rule::new(r"https://example\.com", "first:$0").expect("valid hyperlink rule")];
        let second_rules =
            vec![Rule::new(r"https://example\.com", "second:$0").expect("valid hyperlink rule")];

        {
            let state = renderable.lock();
            let mut inner = state.inner.borrow_mut();
            inner.seqno = 7;
            let mut first = Line::from_text(
                "https://exam",
                &CellAttributes::default(),
                inner.seqno,
                None,
            );
            first.set_last_cell_was_wrapped(true, inner.seqno);
            assert!(inner.put_line(0, first, None));
            inner.normalize_implicit_hyperlinks_for_request(0..1, &first_rules);
        }

        let (_, incomplete) = renderable.lock().get_lines(0..1);
        assert!(
            incomplete[0]
                .get_cell(0)
                .and_then(|cell| cell.attrs().hyperlink().cloned())
                .is_none(),
            "a wrapped fragment must fail closed until its successor is cached",
        );

        {
            let state = renderable.lock();
            let mut inner = state.inner.borrow_mut();
            let second = Line::from_text("ple.com", &CellAttributes::default(), inner.seqno, None);
            assert!(inner.put_line(1, second, None));
        }

        let (_, first_view) = renderable
            .lock()
            .get_lines_with_hyperlinks(0..2, &first_rules);
        let first_row_link = first_view[0]
            .get_cell(0)
            .and_then(|cell| cell.attrs().hyperlink().cloned())
            .expect("complete logical line should be linked");
        let second_row_link = first_view[1]
            .get_cell(0)
            .and_then(|cell| cell.attrs().hyperlink().cloned())
            .expect("link should span the wrapped successor");
        assert!(Arc::ptr_eq(&first_row_link, &second_row_link));
        assert_eq!(first_row_link.uri(), "first:https://example.com");

        let (_, repeated_view) = renderable
            .lock()
            .get_lines_with_hyperlinks(0..2, &first_rules);
        let repeated_link = repeated_view[0]
            .get_cell(0)
            .and_then(|cell| cell.attrs().hyperlink().cloned())
            .expect("same rules should retain the cached link");
        assert!(
            Arc::ptr_eq(&first_row_link, &repeated_link),
            "same-rule paint and hover snapshots must retain Arc identity",
        );

        let (_, second_view) = renderable
            .lock()
            .get_lines_with_hyperlinks(0..2, &second_rules);
        let replaced_link = second_view[0]
            .get_cell(0)
            .and_then(|cell| cell.attrs().hyperlink().cloned())
            .expect("replacement rule should produce a link");
        assert_eq!(replaced_link.uri(), "second:https://example.com");
        assert!(!Arc::ptr_eq(&first_row_link, &replaced_link));
    }

    #[test]
    fn switching_implicit_hyperlink_rules_to_empty_clears_cached_shape_appdata() {
        let renderable = test_renderable_state();
        let rules =
            vec![Rule::new(r"https://example\.com", "linked:$0").expect("valid hyperlink rule")];
        {
            let state = renderable.lock();
            let mut inner = state.inner.borrow_mut();
            inner.seqno = 13;
            let seqno = inner.seqno;
            assert!(inner.put_line(
                0,
                Line::from_text(
                    "https://example.com",
                    &CellAttributes::default(),
                    seqno,
                    None,
                ),
                None,
            ));
        }

        let (first, linked) = renderable.lock().get_lines_with_hyperlinks(0..1, &rules);
        assert!(
            linked[0]
                .get_cell(0)
                .and_then(|cell| cell.attrs().hyperlink().cloned())
                .is_some(),
            "the non-empty rule epoch must first install an implicit link",
        );
        let cached_shape = Arc::new(linked[0].compute_shape_hash());
        linked[0].set_appdata(Arc::clone(&cached_shape));
        renderable
            .lock()
            .write_back_unchanged_line_appdata(first, &linked);

        let (_, unlinked) = renderable.lock().get_lines_with_hyperlinks(0..1, &[]);
        assert!(
            unlinked[0]
                .get_cell(0)
                .and_then(|cell| cell.attrs().hyperlink().cloned())
                .is_none(),
            "an empty replacement rule epoch must remove the implicit link",
        );
        assert!(
            unlinked[0].get_appdata().is_none(),
            "removing links without advancing the terminal seqno must invalidate cached shape appdata",
        );
    }

    #[test]
    fn rule_change_clears_appdata_when_wrapped_group_is_temporarily_incomplete() {
        let renderable = test_renderable_state();
        let first_rules =
            vec![Rule::new(r"https://example\.com", "first:$0").expect("valid hyperlink rule")];
        let second_rules =
            vec![Rule::new(r"https://example\.com", "second:$0").expect("valid hyperlink rule")];
        let cached_shape = Arc::new([0x5a_u8; 16]);

        {
            let state = renderable.lock();
            let mut inner = state.inner.borrow_mut();
            inner.seqno = 17;
            let seqno = inner.seqno;
            let mut first = Line::from_text(
                "https://exam",
                &CellAttributes::default(),
                inner.seqno,
                None,
            );
            first.set_last_cell_was_wrapped(true, inner.seqno);
            assert!(inner.put_line(0, first, None));
            assert!(inner.put_line(
                1,
                Line::from_text("ple.com", &CellAttributes::default(), seqno, None,),
                None,
            ));
            inner.normalize_implicit_hyperlinks_for_request(0..2, &first_rules);

            for stable_row in 0..2 {
                let Some(LineEntry::Line(line)) = inner.lines.get_mut(&stable_row) else {
                    panic!("complete logical group row {} must be fresh", stable_row);
                };
                assert!(line.has_hyperlink());
                line.set_appdata(Arc::clone(&cached_shape));
            }

            let Some(LineEntry::Line(successor)) = inner.lines.pop(&1) else {
                panic!("wrapped successor must still be cached");
            };
            inner.lines.put(1, LineEntry::Stale(successor));
        }

        let (_, first_row) = renderable
            .lock()
            .get_lines_with_hyperlinks(0..1, &second_rules);
        assert!(
            first_row[0]
                .get_cell(0)
                .and_then(|cell| cell.attrs().hyperlink().cloned())
                .is_none(),
            "an incomplete group must fail closed instead of retaining the prior rule epoch's link",
        );
        assert!(
            first_row[0].get_appdata().is_none(),
            "the fresh fragment must not retain prior-epoch shape appdata",
        );

        let state = renderable.lock();
        let inner = state.inner.borrow();
        let Some(LineEntry::Stale(successor)) = inner.lines.peek(&1) else {
            panic!("wrapped successor must remain stale until refetched");
        };
        assert!(!successor.has_hyperlink());
        assert!(
            successor.get_appdata().is_none(),
            "the unavailable successor must also lose prior-epoch shape appdata",
        );
    }

    #[test]
    fn unchanged_remote_projection_writes_renderer_appdata_back_to_cache() {
        let renderable = test_renderable_state();
        {
            let state = renderable.lock();
            let mut inner = state.inner.borrow_mut();
            inner.seqno = 17;
            assert!(inner.put_line(
                0,
                Line::from_text("stable", &CellAttributes::default(), 17, None),
                None,
            ));
        }

        let (first, projected) = renderable.lock().get_lines(0..1);
        let marker = Arc::new(0x5eed_u64);
        projected[0].set_appdata(Arc::clone(&marker));
        renderable
            .lock()
            .write_back_unchanged_line_appdata(first, &projected);

        let (_, repeated) = renderable.lock().get_lines(0..1);
        let retained = repeated[0]
            .get_appdata()
            .expect("unchanged cached row should retain renderer appdata")
            .downcast::<u64>()
            .expect("test marker type should be preserved");
        assert!(Arc::ptr_eq(&marker, &retained));
    }

    #[test]
    fn modified_remote_projection_cannot_poison_authoritative_appdata() {
        let renderable = test_renderable_state();
        {
            let state = renderable.lock();
            let mut inner = state.inner.borrow_mut();
            inner.seqno = 19;
            assert!(inner.put_line(
                0,
                Line::from_text("source", &CellAttributes::default(), 19, None),
                None,
            ));
        }

        let (first, mut projected) = renderable.lock().get_lines(0..1);
        projected[0].set_cell(0, Cell::new('X', CellAttributes::default()), SEQ_ZERO);
        projected[0].set_appdata(Arc::new(0xbad_u64));
        renderable
            .lock()
            .write_back_unchanged_line_appdata(first, &projected);

        let (_, repeated) = renderable.lock().get_lines(0..1);
        assert_eq!(repeated[0].as_str(), "source");
        assert!(
            repeated[0].get_appdata().is_none(),
            "metadata computed for modified output must not reach the source row",
        );
    }

    #[test]
    fn unilateral_deltas_must_not_rewind_seqno() {
        let current: SequenceNo = 10;

        assert!(should_apply_unilateral_delta(current, 10));
        assert!(should_apply_unilateral_delta(current, 11));
        assert!(!should_apply_unilateral_delta(current, 9));
    }

    #[test]
    fn forced_dispatch_ack_does_not_retire_prediction_before_terminal_change() {
        let born = Instant::now();
        let serial = input_serial(100);
        let mut predictions = vec![prediction(serial, 0, 0, 'x', born)];
        mark_predictions_dispatched(&mut predictions, serial, 10);
        let lines = vec![(
            0,
            Line::from_text("x", &CellAttributes::default(), 10, None),
        )];

        let reconciliation =
            reconcile_predictions_after_terminal_change(&mut predictions, 10, &lines);

        assert_eq!(reconciliation, PredictionReconciliation::default());
        assert_eq!(predictions.len(), 1);
        assert_eq!(predictions[0].dispatch_seqno, Some(10));
    }

    #[test]
    fn paste_only_prediction_receives_dispatch_fence_and_reconciles_after_echo() {
        let renderable = test_renderable_state_with_echo_threshold(Some(0));
        let state = renderable.lock();
        let mut inner = state.inner.borrow_mut();
        let serial = input_serial(100);
        inner.last_input_rtt = 1;
        inner.input_serial = serial;
        inner
            .lines
            .put(0, LineEntry::Line(Line::from("ordinary prompt")));

        inner.predict_from_paste("x");

        assert_eq!(inner.predictions.len(), 1);
        assert_eq!(inner.predictions[0].input_serial, serial);
        assert_eq!(inner.predictions[0].dispatch_seqno, None);

        mark_predictions_dispatched(&mut inner.predictions, serial, 10);
        assert_eq!(inner.predictions[0].dispatch_seqno, Some(10));

        let authoritative_echo = vec![(
            0,
            Line::from_text("x", &CellAttributes::default(), 11, None),
        )];
        let reconciliation = reconcile_predictions_after_terminal_change(
            &mut inner.predictions,
            11,
            &authoritative_echo,
        );

        assert_eq!(
            reconciliation,
            PredictionReconciliation {
                confirmed: 1,
                rejected: 0,
            }
        );
        assert!(inner.predictions.is_empty());
    }

    #[test]
    fn delayed_authoritative_echo_retires_only_after_dispatch_fence_advances() {
        let born = Instant::now();
        let serial = input_serial(100);
        let mut predictions = vec![prediction(serial, 0, 0, 'x', born)];
        mark_predictions_dispatched(&mut predictions, serial, 10);
        let lines = vec![(
            0,
            Line::from_text("x", &CellAttributes::default(), 11, None),
        )];

        let reconciliation =
            reconcile_predictions_after_terminal_change(&mut predictions, 11, &lines);

        assert_eq!(
            reconciliation,
            PredictionReconciliation {
                confirmed: 1,
                rejected: 0,
            }
        );
        assert!(predictions.is_empty());
    }

    #[test]
    fn missing_authoritative_row_is_negative_evidence_not_a_false_verdict() {
        let born = Instant::now();
        let serial = input_serial(100);
        let mut predictions = vec![prediction(serial, 7, 3, 'x', born)];
        mark_predictions_dispatched(&mut predictions, serial, 10);
        let unrelated_lines = vec![(
            0,
            Line::from_text("other", &CellAttributes::default(), 11, None),
        )];

        let reconciliation =
            reconcile_predictions_after_terminal_change(&mut predictions, 11, &unrelated_lines);

        assert_eq!(reconciliation, PredictionReconciliation::default());
        assert_eq!(predictions.len(), 1);
    }

    #[test]
    fn reordered_output_waits_for_dispatch_fence_then_settles_in_order() {
        let born = Instant::now();
        let serial = input_serial(100);
        let mut predictions = vec![prediction(serial, 0, 0, 'x', born)];
        let mut lines = LruCache::new(NonZeroUsize::new(1).unwrap());
        lines.put(
            0,
            LineEntry::Line(Line::from_text("x", &CellAttributes::default(), 11, None)),
        );

        let before_ack =
            reconcile_predictions_after_cached_terminal_change(&mut predictions, 11, &lines);
        assert_eq!(before_ack, PredictionReconciliation::default());
        assert_eq!(predictions.len(), 1);

        mark_predictions_dispatched(&mut predictions, serial, 10);
        let after_ack =
            reconcile_predictions_after_cached_terminal_change(&mut predictions, 11, &lines);
        assert_eq!(
            after_ack,
            PredictionReconciliation {
                confirmed: 1,
                rejected: 0,
            }
        );
        assert!(predictions.is_empty());
    }

    #[test]
    fn unchanged_cached_row_after_dispatch_is_not_false_echo_evidence() {
        let born = Instant::now();
        let serial = input_serial(100);
        let mut predictions = vec![prediction(serial, 0, 0, 'x', born)];
        mark_predictions_dispatched(&mut predictions, serial, 10);
        let mut lines = LruCache::new(NonZeroUsize::new(1).unwrap());
        lines.put(
            0,
            LineEntry::Line(Line::from_text("x", &CellAttributes::default(), 10, None)),
        );

        let reconciliation =
            reconcile_predictions_after_cached_terminal_change(&mut predictions, 11, &lines);

        assert_eq!(reconciliation, PredictionReconciliation::default());
        assert_eq!(predictions.len(), 1);
    }

    #[test]
    fn no_echo_expires_with_bounded_confidence_degradation() {
        let born = Instant::now();
        let serial = input_serial(100);
        let mut predictions = vec![prediction(serial, 0, 0, 'x', born)];
        mark_predictions_dispatched(&mut predictions, serial, 10);
        let now = born + Duration::from_millis(251);

        let expired = expire_predictions(&mut predictions, now, Duration::from_millis(250));
        let mut score = PREDICT_CONFIDENT_SCORE;
        let mut last_miss = born;
        apply_prediction_reconciliation_to_score(
            &mut score,
            &mut last_miss,
            PredictionReconciliation {
                confirmed: 0,
                rejected: expired,
            },
            now,
        );

        assert_eq!(expired, 1);
        assert!(predictions.is_empty());
        assert!(score < PREDICT_CONFIDENT_SCORE);
        assert_eq!(last_miss, now);
    }

    #[test]
    fn authoritative_reconnect_snapshot_resets_predictions_and_confidence() {
        let born = Instant::now();
        let mut predictions = vec![prediction(input_serial(100), 0, 0, 'x', born)];
        let mut score = PREDICT_CONFIDENT_SCORE;

        reset_prediction_state(&mut predictions, &mut score);

        assert!(predictions.is_empty());
        assert_eq!(score, 0);
    }

    #[test]
    fn speculative_prediction_admission_is_hard_bounded_per_pane() {
        let born = Instant::now();
        let sample = prediction(input_serial(100), 0, 0, 'x', born);
        let mut predictions = vec![sample.clone(); MAX_PENDING_PREDICTIONS];

        assert!(!push_bounded_prediction(&mut predictions, sample));
        assert_eq!(predictions.len(), MAX_PENDING_PREDICTIONS);
    }

    #[test]
    fn paste_prediction_preflight_is_all_or_nothing_and_bounded() {
        assert!(paste_fits_prediction_budget(
            MAX_PENDING_PREDICTIONS - 3,
            "abc"
        ));
        assert!(!paste_fits_prediction_budget(
            MAX_PENDING_PREDICTIONS - 3,
            "abcd"
        ));
        assert!(!paste_fits_prediction_budget(MAX_PENDING_PREDICTIONS, "x"));
    }

    #[test]
    fn fetch_tokens_use_request_identity_even_at_the_same_clock_instant() {
        let started_at = Instant::now();
        let first = FetchToken::new(started_at);
        let first_clone = first.clone();
        let successor = FetchToken::new(started_at);

        assert!(first.same_request(&first_clone));
        assert!(!first.same_request(&successor));
        assert_eq!(first.started_at(), successor.started_at());
    }

    #[test]
    fn exact_fetch_cleanup_stales_old_lines_and_preserves_successor_requests() {
        let renderable = test_renderable_state();
        let exact = FetchToken::new(Instant::now());
        let successor = FetchToken::new(Instant::now());
        let mut requested = rangeset::RangeSet::new();
        requested.add_range(0..3);

        {
            let state = renderable.lock();
            let mut inner = state.inner.borrow_mut();
            inner.lines.put(
                0,
                LineEntry::LineAndFetching(Line::with_width(1, 7), exact.clone()),
            );
            inner.lines.put(1, LineEntry::Fetching(exact.clone()));
            inner.lines.put(2, LineEntry::Fetching(successor.clone()));
            inner.release_exact_fetch_reservations(&requested, &exact);

            assert!(matches!(inner.lines.peek(&0), Some(LineEntry::Stale(_))));
            assert!(inner.lines.peek(&1).is_none());
            assert!(matches!(
                inner.lines.peek(&2),
                Some(LineEntry::Fetching(current)) if current.same_request(&successor)
            ));
        }
    }

    #[test]
    fn dead_pane_fetch_admission_releases_its_exact_reservations() {
        let renderable = test_renderable_state();
        let exact = FetchToken::new(Instant::now());
        let mut requested = rangeset::RangeSet::new();
        requested.add_range(0..2);

        let state = renderable.lock();
        let mut inner = state.inner.borrow_mut();
        inner.dead = true;
        inner.lines.put(
            0,
            LineEntry::LineAndFetching(Line::with_width(1, 7), exact.clone()),
        );
        inner.lines.put(1, LineEntry::Fetching(exact.clone()));
        inner.schedule_fetch_lines(requested, exact);

        assert!(matches!(inner.lines.peek(&0), Some(LineEntry::Stale(_))));
        assert!(inner.lines.peek(&1).is_none());
    }

    #[test]
    fn tiered_render_cache_uses_hot_budget() {
        assert_eq!(
            render_line_cache_capacity_for_values(100_000, true, 2_000, 48).get(),
            2_000
        );
        assert_eq!(
            render_line_cache_capacity_for_values(100_000, false, 2_000, 48).get(),
            100_000
        );
        // Small scrollback + tall viewport: floor sizes to the prefetch working
        // set (3 * viewport), not just the viewport. [review 3f]
        assert_eq!(
            render_line_cache_capacity_for_values(64, true, 8, 240).get(),
            720
        );
    }

    #[test]
    fn stale_rebuild_keeps_lru_capacity_and_mru_lines() {
        let mut lines = LruCache::new(NonZeroUsize::new(3).unwrap());
        for stable_row in 0..3 {
            lines.put(stable_row, LineEntry::Line(Line::with_width(1, SEQ_ZERO)));
        }

        rebuild_cache_as_stale(&mut lines, NonZeroUsize::new(2).unwrap());

        assert_eq!(lines.cap().get(), 2);
        assert_eq!(lines.len(), 2);
        assert!(!lines.contains(&0));
        for stable_row in [1, 2] {
            assert!(
                matches!(lines.peek(&stable_row), Some(LineEntry::Stale(_))),
                "row {} should be retained as stale",
                stable_row
            );
        }
    }

    #[test]
    fn geometry_invalidation_retires_old_fetch_tokens() {
        let mut lines = LruCache::new(NonZeroUsize::new(3).unwrap());
        let token = FetchToken::new(Instant::now());
        lines.put(
            0,
            LineEntry::LineAndFetching(Line::with_width(80, 1), token.clone()),
        );
        lines.put(1, LineEntry::Fetching(token));

        rebuild_cache_as_stale(&mut lines, NonZeroUsize::new(3).unwrap());

        assert!(matches!(lines.peek(&0), Some(LineEntry::Stale(_))));
        assert!(lines.peek(&1).is_none());
    }

    #[test]
    fn image_lru_evicts_by_decoded_bytes() {
        let mut cache = ImageLru::new(NonZeroUsize::new(8).unwrap(), 32);
        let pane_id = 7;
        let first = validated_test_image(2, 2, 1);
        let second = validated_test_image(2, 2, 2);
        let third = validated_test_image(2, 2, 3);
        let first_hash = first.data.hash();
        let second_hash = second.data.hash();
        let third_hash = third.data.hash();

        cache.put(TEST_RENDER_CONNECTION_IDENTITY, pane_id, first);
        cache.put(TEST_RENDER_CONNECTION_IDENTITY, pane_id, second);
        cache.put(TEST_RENDER_CONNECTION_IDENTITY, pane_id, third);

        assert_eq!(cache.len(), 2);
        assert_eq!(cache.retained_bytes(), 32);
        assert!(cache
            .get(TEST_RENDER_CONNECTION_IDENTITY, pane_id, &first_hash)
            .is_none());
        assert!(cache
            .get(TEST_RENDER_CONNECTION_IDENTITY, pane_id, &second_hash)
            .is_some());
        assert!(cache
            .get(TEST_RENDER_CONNECTION_IDENTITY, pane_id, &third_hash)
            .is_some());
    }

    #[test]
    fn image_lru_refuses_single_image_over_budget() {
        let mut cache = ImageLru::new(NonZeroUsize::new(8).unwrap(), 8);
        let pane_id = 7;
        let oversized = validated_test_image(2, 2, 4);
        let hash = oversized.data.hash();

        cache.put(TEST_RENDER_CONNECTION_IDENTITY, pane_id, oversized);

        assert_eq!(cache.len(), 0);
        assert_eq!(cache.retained_bytes(), 0);
        assert!(cache
            .get(TEST_RENDER_CONNECTION_IDENTITY, pane_id, &hash)
            .is_none());
    }

    #[test]
    fn image_lru_oversized_replacement_preserves_valid_entry() {
        let mut cache = ImageLru::new(NonZeroUsize::new(8).unwrap(), 16);
        let pane_id = 7;
        let valid = validated_test_image(2, 2, 4);
        let hash = valid.data.hash();
        cache.put(TEST_RENDER_CONNECTION_IDENTITY, pane_id, valid);

        let mut forged_accounting = validated_test_image(2, 2, 4);
        forged_accounting.decoded_bytes = 17;
        cache.put(TEST_RENDER_CONNECTION_IDENTITY, pane_id, forged_accounting);

        assert_eq!(cache.len(), 1);
        assert_eq!(cache.retained_bytes(), 16);
        assert!(cache
            .get(TEST_RENDER_CONNECTION_IDENTITY, pane_id, &hash)
            .is_some());
    }

    #[test]
    fn image_lru_rejects_and_evicts_payloads_that_changed_revision() {
        let mut cache = ImageLru::new(NonZeroUsize::new(8).unwrap(), 32);
        let pane_id = 7;

        let changed_before_insert = validated_test_image(2, 2, 4);
        let old_revision = changed_before_insert.source_revision;
        *changed_before_insert.data.data_mut() =
            ImageDataType::new_single_frame(2, 2, vec![0x44; 16]);
        assert_ne!(
            old_revision,
            changed_before_insert.data.current_content_hash()
        );
        cache.put(
            TEST_RENDER_CONNECTION_IDENTITY,
            pane_id,
            changed_before_insert,
        );
        assert_eq!(cache.len(), 0, "a stale cache key must not be published");

        let changed_after_insert = validated_test_image(2, 2, 5);
        let cached_revision = changed_after_insert.source_revision;
        let mutable_data = Arc::clone(&changed_after_insert.data);
        cache.put(
            TEST_RENDER_CONNECTION_IDENTITY,
            pane_id,
            changed_after_insert,
        );
        *mutable_data.data_mut() = ImageDataType::new_single_frame(2, 2, vec![0x55; 16]);
        assert!(
            cache
                .get(TEST_RENDER_CONNECTION_IDENTITY, pane_id, &cached_revision,)
                .is_none(),
            "a cache hit must fail closed after the mutable payload changes"
        );
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.retained_bytes(), 0);
    }

    #[test]
    fn image_lru_isolates_connection_and_pane_namespaces() {
        let mut cache = ImageLru::new(NonZeroUsize::new(8).unwrap(), 32);
        let pane_id = 7;
        let image = validated_test_image(2, 2, 9);
        let hash = image.data.hash();
        let other_connection = RenderConnectionIdentity::new(
            TopologyStreamId::from_bytes([0x91; 16]),
            MuxSessionIncarnation::from_bytes([0x92; 16]),
        );

        cache.put(TEST_RENDER_CONNECTION_IDENTITY, pane_id, image);

        assert!(cache
            .get(TEST_RENDER_CONNECTION_IDENTITY, pane_id, &hash)
            .is_some());
        assert!(cache
            .get(TEST_RENDER_CONNECTION_IDENTITY, pane_id + 1, &hash)
            .is_none());
        assert!(cache.get(other_connection, pane_id, &hash).is_none());
    }

    #[test]
    fn image_locator_collection_preserves_source_order_and_caps_retry_amplification() {
        let pane_id = 7;
        let first_hash = [1; 32];
        let second_hash = [2; 32];
        let mut requests = Vec::new();
        let mut indices = HashMap::new();
        for cell_idx in 0..MAX_IMAGE_LOCATOR_ATTEMPTS_PER_REVISION.saturating_add(4) {
            push_image_locator(
                &mut requests,
                &mut indices,
                first_hash,
                GetImageCell {
                    pane_id,
                    line_idx: 3,
                    cell_idx,
                    data_hash: first_hash,
                },
            );
        }
        push_image_locator(
            &mut requests,
            &mut indices,
            second_hash,
            GetImageCell {
                pane_id,
                line_idx: 4,
                cell_idx: 0,
                data_hash: second_hash,
            },
        );

        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0, first_hash);
        assert_eq!(requests[0].1.len(), MAX_IMAGE_LOCATOR_ATTEMPTS_PER_REVISION);
        assert_eq!(requests[1].0, second_hash);
    }

    #[test]
    fn any_permanent_image_failure_settles_the_whole_row_to_text_fallback() {
        let unavailable = HashSet::from([10, 11]);
        let rows_with_permanent_failure = HashSet::from([11]);

        assert_eq!(
            rows_requiring_image_retry(&unavailable, &rows_with_permanent_failure),
            HashSet::from([10])
        );
    }

    #[test]
    fn image_lru_negative_cache_is_revision_scoped_and_transiently_expires() {
        let mut cache = ImageLru::new(NonZeroUsize::new(8).unwrap(), 32);
        let pane_id = 7;
        let revision = [0x31; 32];
        let other_revision = [0x32; 32];
        let now = Instant::now();

        cache.record_failure(
            TEST_RENDER_CONNECTION_IDENTITY,
            pane_id,
            revision,
            CachedImageFailure::Permanent,
        );
        assert_eq!(
            cache.get_failure(TEST_RENDER_CONNECTION_IDENTITY, pane_id, &revision, now,),
            Some(CachedImageFailure::Permanent)
        );
        assert!(cache
            .get_failure(
                TEST_RENDER_CONNECTION_IDENTITY,
                pane_id,
                &other_revision,
                now,
            )
            .is_none());

        cache.record_failure(
            TEST_RENDER_CONNECTION_IDENTITY,
            pane_id,
            other_revision,
            CachedImageFailure::Transient {
                retry_after: now + Duration::from_millis(10),
            },
        );
        assert!(
            cache
                .get_failure(
                    TEST_RENDER_CONNECTION_IDENTITY,
                    pane_id,
                    &other_revision,
                    now + Duration::from_millis(11),
                )
                .is_none(),
            "expired transient failures must permit a retry"
        );
    }

    #[test]
    fn valid_overflow_tail_is_cached_without_joining_the_oversized_batch() {
        let pane_id = 0x5eed;
        let connection = RenderConnectionIdentity::new(
            TopologyStreamId::from_bytes([0xd7; 16]),
            MuxSessionIncarnation::from_bytes([0xe8; 16]),
        );
        let validated = validated_test_image(2, 2, 0x6a);
        let revision = validated.source_revision;
        let accepted_decoded_bytes = MAX_ORDINARY_IMAGE_BATCH_BYTES
            .checked_sub(validated.decoded_bytes)
            .expect("ordinary image limit must admit one tiny image")
            .saturating_add(1);

        assert_eq!(
            cache_and_admit_ordinary_image(
                Some(connection),
                pane_id,
                accepted_decoded_bytes,
                &validated,
            ),
            OrdinaryImageBatchAdmission::BatchDecodedByteLimit,
        );
        assert!(
            get_cached_validated_image(connection, pane_id, &revision).is_some(),
            "an individually valid overflow-tail revision must be reusable without another RPC or decode"
        );
    }

    #[test]
    fn image_hydration_reservation_covers_the_rejected_overflow_frame() {
        assert_eq!(
            IMAGE_HYDRATION_WORKING_SET_RESERVATION_BYTES,
            MAX_GET_IMAGE_CELL_RESPONSE_DECOMPRESSED_BYTES * 2
                + MAX_IMAGE_HYDRATION_DECODED_BYTES * 2
        );
        assert_eq!(
            MAX_GLOBAL_IMAGE_HYDRATION_WORKING_SET_BYTES,
            IMAGE_HYDRATION_WORKING_SET_RESERVATION_BYTES,
            "the current envelope intentionally admits one worst-case hydration at a time"
        );
    }
}
