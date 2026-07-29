use crate::client::RpcGenerationScope;
use crate::domain::ClientInner;
use codec::*;
use config::{configuration, ConfigHandle};
use lru::LruCache;
use mux::pane::PaneId;
use mux::renderable::{PaneTieredScrollbackStatus, RenderableDimensions, StableCursorPosition};
use mux::{PaneRegistrationHandle, PaneRegistrationSlot};
use promise::BrokenPromise;
use rangeset::*;
use ratelim::RateLimiter;
use std::cell::RefCell;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};
use termwiz::cell::{Cell, CellAttributes, Underline};
use termwiz::color::AnsiColor;
use termwiz::image::{ImageCell, ImageData};
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
enum LineEntry {
    // Up to date wrt. server and has been rendered at least once
    Line(Line),
    // Currently being downloaded from the server
    Fetching(Instant),
    // We have a version of the line locally and are treating it
    // as needing rendering because we are also in the process of
    // downloading a newer version from the server
    LineAndFetching(Line, Instant),
    // We have a local copy but it is stale and will need to be
    // fetched again
    Stale(Line),
}

impl LineEntry {
    fn kind(&self) -> (&'static str, Option<Instant>) {
        match self {
            Self::Line(_) => ("Line", None),
            Self::Fetching(since) => ("Fetching", Some(*since)),
            Self::LineAndFetching(_, since) => ("LineAndFetching", Some(*since)),
            Self::Stale(_) => ("Stale", None),
        }
    }
}

fn rebuild_cache_as_stale(lines: &mut LruCache<StableRowIndex, LineEntry>, capacity: NonZeroUsize) {
    let mut stale_lines = LruCache::new(capacity);
    while let Some((stable_row, entry)) = lines.pop_lru() {
        let entry = match entry {
            LineEntry::Stale(old) | LineEntry::Line(old) => LineEntry::Stale(old),
            entry => entry,
        };
        stale_lines.put(stable_row, entry);
    }
    *lines = stale_lines;
}

/// A single predicted cell, kept as an overlay record instead of being baked into
/// the cached server `Line`. Storing predictions separately is what lets us
/// validate each against the authoritative server cell (to drive prediction
/// confidence), rewind a wrong one for free (the overlay simply stops painting,
/// revealing the authoritative cached content), and resolve the underline cue
/// from the pane's confidence at render time. The cursor is still predicted in
/// place (cursor_position) and reconciled by the existing input_serial gate.
#[derive(Debug, Clone)]
struct Prediction {
    row: StableRowIndex,
    col: usize,
    /// The plain predicted glyph (no underline); the underline cue is applied at
    /// render time only while prediction confidence is low.
    predicted: Cell,
    /// Serial of the keystroke that produced this prediction; the server echoes
    /// it back, letting us tell which predictions a delta confirms.
    input_serial: InputSerial,
    born: Instant,
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
            predictions: Vec::new(),
            alt_screen_active: false,
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
    fn should_predict(&self) -> bool {
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
    fn record_prediction(&mut self, row: StableRowIndex, col: usize, predicted: Cell) {
        self.predictions.push(Prediction {
            row,
            col,
            predicted,
            input_serial: self.input_serial,
            born: Instant::now(),
        });
    }

    /// Retire the predictions the server has now confirmed (input_serial <= serial),
    /// validating each against the authoritative cached cell to drive the pane's
    /// prediction confidence score. Rewind is automatic: a wrong prediction simply
    /// stops being painted, revealing the authoritative content beneath it. [3b]
    fn validate_and_retire_predictions(
        &mut self,
        serial: InputSerial,
        bonus_lines: &[(StableRowIndex, Line)],
    ) {
        let (confirmed, pending): (Vec<Prediction>, Vec<Prediction>) =
            std::mem::take(&mut self.predictions)
                .into_iter()
                .partition(|p| p.input_serial <= serial);
        self.predictions = pending;
        let now = Instant::now();
        for p in confirmed {
            // Validate against the authoritative content the server sent inline with
            // this delta: the cursor row (where typed echo lands) always rides in
            // bonus_lines, which has NOT yet been written to the cache at this point.
            // Rows not present in bonus_lines can't be judged -> verdict None. A
            // missing cell counts as a blank, so a predicted blank (backspace/delete)
            // at a compressed trailing position reads as correct, not no-event, and a
            // predicted glyph the server didn't echo reads as a miss. [review F9]
            let verdict = bonus_lines.iter().find(|(r, _)| *r == p.row).map(|(_, l)| {
                match l.get_cell(p.col) {
                    Some(c) => c.str() == p.predicted.str(),
                    None => p.predicted.str() == " ",
                }
            });
            match verdict {
                Some(true) => {
                    self.prediction_score = (self.prediction_score + 1).min(PREDICT_SCORE_MAX);
                }
                Some(false) => {
                    // A miss drops the score AND forces it below the confident
                    // threshold, so secret characters in an echo-off prompt never
                    // render plain even if the pane was confident a moment ago. [F2]
                    self.prediction_score = (self.prediction_score - 2)
                        .clamp(PREDICT_SCORE_MIN, PREDICT_CONFIDENT_SCORE - 1);
                    self.last_prediction_miss = now;
                }
                None => {}
            }
        }
        // Recover from suppression -- but only after a quiet cooldown since the last
        // miss, so an echo-off prompt stays suppressed instead of re-firing (and
        // re-painting a secret char) on the very next keystroke. Once suppressed the
        // pane stops predicting, so nothing validates it back up; this re-arms it to
        // re-test after the bad patch has passed. [review F1]
        if self.prediction_score <= PREDICT_SUPPRESS_SCORE
            && self.last_prediction_miss.elapsed() > PREDICT_SUPPRESS_COOLDOWN
        {
            self.prediction_score = PREDICT_SUPPRESS_SCORE + 1;
        }
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
                self.cursor_position.x = 0;
                self.cursor_position.y += 1;
            }
            KeyCode::UpArrow => {
                self.cursor_position.y = self.cursor_position.y.saturating_sub(1);
            }
            KeyCode::DownArrow => {
                self.cursor_position.y += 1;
            }
            KeyCode::RightArrow => {
                self.cursor_position.x += 1;
            }
            KeyCode::LeftArrow => {
                self.cursor_position.x = self.cursor_position.x.saturating_sub(1);
            }
            KeyCode::Delete => {
                let row = self.cursor_position.y;
                let col = self.cursor_position.x;
                self.record_prediction(row, col, Cell::new(' ', CellAttributes::default()));
            }
            KeyCode::Backspace if self.cursor_position.x > 0 => {
                let row = self.cursor_position.y;
                let col = self.cursor_position.x - 1;
                self.record_prediction(row, col, Cell::new(' ', CellAttributes::default()));
                self.cursor_position.x -= 1;
            }
            KeyCode::Char(c) => {
                // Store the plain glyph; the underline uncertainty cue is applied at
                // render time only while confidence is low (glitchless, 3d).
                let cell = Cell::new(c, CellAttributes::default());
                let width = cell.width();
                let row = self.cursor_position.y;
                let col = self.cursor_position.x;
                self.record_prediction(row, col, cell);
                // Adjust the cursor to reflect the width of this new cell
                self.cursor_position.x += width;
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

    fn apply_paste_prediction(&mut self, paste_idx: usize, text: &str) {
        // Plain glyphs; the underline cue is applied at render time (3d).
        let attrs = CellAttributes::default();
        let text_line = Line::from_text(text, &attrs, SEQ_ZERO, None);
        let target_row = self.cursor_position.y + paste_idx as StableRowIndex;

        if paste_idx == 0 {
            // First pasted line is appended at the cursor.
            for cell in text_line.visible_cells() {
                let col = self.cursor_position.x;
                self.record_prediction(target_row, col, cell.as_cell());
                self.cursor_position.x += cell.width();
            }
        } else {
            // Subsequent pasted lines replace the row content from column 0.
            let mut col = 0;
            for cell in text_line.visible_cells() {
                self.record_prediction(target_row, col, cell.as_cell());
                col += cell.width();
            }
            self.cursor_position.x = col;
        }
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

        let text = textwrap::fill(text, self.dimensions.cols);
        let lines: Vec<&str> = text.split("\n").collect();

        for (idx, paste_line) in lines.iter().enumerate() {
            let row = self.cursor_position.y + idx as StableRowIndex;
            // Only predict for rows we already have cached; recorded as an overlay.
            let cached = matches!(
                self.lines.peek(&row),
                Some(LineEntry::Line(_) | LineEntry::Stale(_) | LineEntry::LineAndFetching(..))
            );
            if cached {
                self.apply_paste_prediction(idx, paste_line);
            }
        }
        self.cursor_position.y += lines.len().saturating_sub(1) as StableRowIndex;
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

        if !should_apply_unilateral_delta(self.seqno, delta.seqno) {
            log::trace!(
                "ignoring stale render delta for local={} remote={} seqno {} < {}",
                self.local_pane_id,
                self.remote_pane_id,
                delta.seqno,
                self.seqno
            );
            return false;
        }

        let mut dirty = RangeSet::new();
        for r in delta.dirty_lines {
            dirty.add_range(r.clone());
        }
        if delta.cursor_position != self.cursor_position {
            dirty.add(self.cursor_position.y);
            // But note that the server may have sent this in bonus_lines;
            // we'll address that below
            dirty.add(delta.cursor_position.y);
        }

        // Keep track of the approximate round trip time by recording how
        // long it took for this response to come back
        // Track alt-screen state so we never predict into a full-screen TUI. [3e]
        self.alt_screen_active = delta.alt_screen_active;
        if let Some(serial) = delta.input_serial {
            self.last_input_rtt = serial.elapsed_millis();
            // The server has processed every keystroke up to `serial`; validate the
            // predictions it confirms against the authoritative content and retire
            // them. Rewinding a wrong prediction is automatic -- removing it from the
            // overlay reveals the authoritative cell underneath. [3b]
            self.validate_and_retire_predictions(serial, &bonus_lines);
        }

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

        let config = configuration();
        for (stable_row, line) in bonus_lines {
            log::trace!("bonus line {} seqno={}", stable_row, line.current_seqno());
            self.put_line(stable_row, line, &config, None);
            dirty.remove(stable_row);
        }

        let mut to_fetch = RangeSet::new();
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
                    log::trace!("make {} stale bcos not fetchable", stable_row);
                    self.make_stale(stable_row);
                    continue;
                }
                to_fetch.add(stable_row);
                let entry = match prior {
                    Some(LineEntry::Fetching(_)) | None => LineEntry::Fetching(now),
                    Some(LineEntry::LineAndFetching(old, ..))
                    | Some(LineEntry::Stale(old))
                    | Some(LineEntry::Line(old)) => LineEntry::LineAndFetching(old, now),
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
                self.schedule_fetch_lines(to_fetch, now);
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

    fn put_line(
        &mut self,
        stable_row: StableRowIndex,
        mut line: Line,
        config: &ConfigHandle,
        fetch_start: Option<Instant>,
    ) {
        line.scan_and_create_hyperlinks(&config.hyperlink_rules);

        let entry = if let Some(fetch_start) = fetch_start {
            // If we're completing a fetch, only replace entries that were
            // set to fetching as part of our fetch.  If they are now longer
            // tagged that way, then someone came along after us and changed
            // the state, so we should leave it alone

            match self.lines.pop(&stable_row) {
                Some(LineEntry::LineAndFetching(_, then)) | Some(LineEntry::Fetching(then))
                    if fetch_start == then =>
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
                        fetch_start
                    );
                    self.lines.put(stable_row, e);
                    return;
                }
                None => return,
            }
        } else {
            LineEntry::Line(line)
        };
        self.lines.put(stable_row, entry);
    }

    fn schedule_fetch_lines(&mut self, to_fetch: RangeSet<StableRowIndex>, now: Instant) {
        if to_fetch.is_empty() || self.dead {
            return;
        }

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
            now,
        );

        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;
        let rpc = client.client.rpc_scope();
        let request = rpc.get_lines(GetLines {
            pane_id: remote_pane_id,
            lines: to_fetch.clone().into(),
        });

        promise::spawn::spawn(async move {
            let result = request.await;

            let result = match result {
                Ok(result) => {
                    let lines = hydrate_lines(&rpc, remote_pane_id, result.lines).await;
                    Ok(lines)
                }
                Err(err) => Err(err),
            };
            Self::apply_lines(
                registration,
                renderable,
                local_pane_id,
                result,
                to_fetch,
                now,
            )
        })
        .detach();
    }

    fn apply_lines(
        registration: PaneRegistrationHandle,
        renderable: Arc<parking_lot::Mutex<RenderableState>>,
        local_pane_id: PaneId,
        result: anyhow::Result<Vec<(StableRowIndex, Line)>>,
        to_fetch: RangeSet<StableRowIndex>,
        now: Instant,
    ) -> anyhow::Result<()> {
        let applied = registration.try_with_current_output(|_| {
            let renderable = renderable.lock();
            let mut inner = renderable.inner.borrow_mut();

            match result {
                Ok(lines) => {
                    let config = configuration();

                    log::trace!("fetch complete for {:?} at {:?}", to_fetch, now);
                    for (stable_row, line) in lines.into_iter() {
                        inner.put_line(stable_row, line, &config, Some(now));
                    }
                }
                Err(err) => {
                    log::error!("get_lines failed: {}", err);
                    for r in to_fetch.iter() {
                        for stable_row in r.clone() {
                            let entry = match inner.lines.pop(&stable_row) {
                                Some(LineEntry::Fetching(then)) if then == now => {
                                    // leave it popped
                                    continue;
                                }
                                Some(LineEntry::LineAndFetching(line, then)) if then == now => {
                                    // revert to just a line
                                    LineEntry::Line(line)
                                }
                                Some(entry) => entry,
                                None => continue,
                            };
                            inner.lines.put(stable_row, entry);
                        }
                    }
                }
            }
            drop(inner);
            drop(renderable);
        });
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

        self.last_poll = Instant::now();
        self.poll_in_progress.store(true, Ordering::SeqCst);
        let remote_pane_id = self.remote_pane_id;
        let local_pane_id = self.local_pane_id;
        let client = Arc::clone(&self.client);
        let request = client
            .client
            .get_pane_render_changes(GetPaneRenderChanges {
                pane_id: remote_pane_id,
            });
        promise::spawn::spawn(async move {
            let alive = match request.await {
                Ok(resp) => resp.is_alive,
                // if we got a timeout on a reconnectable, don't
                // consider the tab to be dead; that helps to
                // avoid having a tab get shuffled around
                Err(_) => client.client.is_reconnectable,
            };

            let updated = registration.try_with_current(|_| {
                let renderable = renderable.lock();
                let mut inner = renderable.inner.borrow_mut();

                inner.dead = !alive;
                inner.last_recv_time = Instant::now();
                inner.poll_in_progress.store(false, Ordering::SeqCst);
            });
            if updated.is_none() {
                log::trace!(
                    "discarding liveness poll completion for stale client pane registration {}",
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

#[derive(Debug)]
struct CachedImageData {
    data: Arc<ImageData>,
    bytes: usize,
}

#[derive(Debug)]
struct ImageLru {
    cache: LruCache<[u8; 32], CachedImageData>,
    retained_bytes: usize,
    max_bytes: usize,
}

impl ImageLru {
    fn new(max_entries: NonZeroUsize, max_bytes: usize) -> Self {
        Self {
            cache: LruCache::new(max_entries),
            retained_bytes: 0,
            max_bytes,
        }
    }

    fn get(&mut self, hash: &[u8; 32]) -> Option<Arc<ImageData>> {
        self.cache.get(hash).map(|cached| Arc::clone(&cached.data))
    }

    fn put(&mut self, data: Arc<ImageData>) {
        let hash = data.hash();
        if let Some(old) = self.cache.pop(&hash) {
            self.retained_bytes = self.retained_bytes.saturating_sub(old.bytes);
        }

        let bytes = data.len();
        if bytes > self.max_bytes {
            return;
        }

        if let Some((_, old)) = self.cache.push(hash, CachedImageData { data, bytes }) {
            self.retained_bytes = self.retained_bytes.saturating_sub(old.bytes);
        }
        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        self.enforce_byte_budget();
    }

    fn enforce_byte_budget(&mut self) {
        while self.retained_bytes > self.max_bytes {
            let Some((_, old)) = self.cache.pop_lru() else {
                self.retained_bytes = 0;
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(old.bytes);
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
}

fn lock_image_lru() -> MutexGuard<'static, ImageLru> {
    IMAGE_LRU.lock().unwrap_or_else(|poisoned| {
        log::warn!("recovering poisoned client image cache lock");
        poisoned.into_inner()
    })
}

pub(crate) async fn hydrate_lines(
    rpc: &RpcGenerationScope,
    pane_id: PaneId,
    serialized_lines: SerializedLines,
) -> Vec<(StableRowIndex, Line)> {
    let (lines, image_cells) = serialized_lines.extract_data();

    if image_cells.is_empty() {
        return lines;
    }

    let mut requests = HashMap::new();
    let mut data_by_hash = HashMap::new();
    for im in &image_cells {
        if let Some(data) = lock_image_lru().get(&im.data_hash) {
            data_by_hash.insert(im.data_hash, data);
        } else {
            requests
                .entry(&im.data_hash)
                .or_insert_with(|| GetImageCell {
                    pane_id,
                    line_idx: im.line_idx,
                    cell_idx: im.cell_idx,
                    data_hash: im.data_hash,
                });
        }
    }

    for (_, request) in requests {
        match rpc.get_image_cell(request).await {
            Ok(GetImageCellResponse {
                data: Some(data), ..
            }) => {
                lock_image_lru().put(Arc::clone(&data));
                data_by_hash.insert(data.hash(), data);
            }
            Ok(GetImageCellResponse { data: None, .. }) => {
                log::error!("no image data!");
            }

            Err(err) => {
                log::error!("failed to retrieve image {err:#}");
            }
        }
    }

    let mut line_by_idx = HashMap::new();
    for (line_idx, line) in lines {
        line_by_idx.insert(line_idx, line);
    }

    for im in image_cells {
        if let Some(data) = data_by_hash.get(&im.data_hash) {
            if let Some(line) = line_by_idx.get_mut(&im.line_idx) {
                if let Some(cell) = line.cells_mut_for_attr_changes_only().get_mut(im.cell_idx) {
                    cell.attrs_mut()
                        .attach_image(Box::new(ImageCell::with_z_index(
                            im.top_left,
                            im.bottom_right,
                            Arc::clone(data),
                            im.z_index,
                            im.padding_left,
                            im.padding_top,
                            im.padding_right,
                            im.padding_bottom,
                            im.image_id,
                            im.placement_id,
                        )));
                }
            }
        }
    }

    line_by_idx.into_iter().collect()
}

impl RenderableState {
    pub fn get_cursor_position(&self) -> StableCursorPosition {
        self.inner.borrow().cursor_position
    }

    pub fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let mut inner = self.inner.borrow_mut();
        let mut result = vec![];
        let mut to_fetch = RangeSet::new();
        let now = Instant::now();

        for idx in lines.clone() {
            let entry = match inner.lines.pop(&idx) {
                Some(LineEntry::Line(line)) => {
                    result.push(line.clone());
                    if line.changed_since(inner.seqno) {
                        to_fetch.add(idx);
                        LineEntry::Stale(line)
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
                    LineEntry::LineAndFetching(line, now)
                }
                None => {
                    result.push(Line::with_width(inner.dimensions.cols, SEQ_ZERO));
                    to_fetch.add(idx);
                    LineEntry::Fetching(now)
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
        let predict_ttl = Duration::from_millis(250).max(Duration::from_millis(
            inner.last_input_rtt.saturating_mul(4),
        ));
        inner.predictions.retain(|p| p.born.elapsed() < predict_ttl);
        if !inner.predictions.is_empty() {
            let (start, end) = (lines.start, lines.end);
            // Glitchless flagging: once predictions are reliably confirmed correct
            // (high confidence) paint them plain so correct echo is seamless; while
            // confidence is still building, flag each with the underline cue. [3d]
            let confident = inner.prediction_score >= PREDICT_CONFIDENT_SCORE;
            for p in &inner.predictions {
                if p.row >= start && p.row < end {
                    if let Some(line) = result.get_mut((p.row - start) as usize) {
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
        let span = (lines.end - lines.start).max(1);
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
                            inner.lines.put(idx, LineEntry::LineAndFetching(line, now));
                        } else {
                            inner.lines.put(idx, LineEntry::Line(line));
                        }
                    }
                    Some(LineEntry::Stale(line)) => {
                        to_fetch.add(idx);
                        inner.lines.put(idx, LineEntry::LineAndFetching(line, now));
                    }
                    // Already in flight (Fetching / LineAndFetching): dedupe -> leave.
                    Some(other) => {
                        inner.lines.put(idx, other);
                    }
                    None => {
                        to_fetch.add(idx);
                        inner.lines.put(idx, LineEntry::Fetching(now));
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

        inner.schedule_fetch_lines(to_fetch, now);
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

        let mut result = RangeSet::new();
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
        base_poll_interval, initial_last_poll, rebuild_cache_as_stale,
        render_line_cache_capacity_for_values, should_apply_unilateral_delta, ImageLru, LineEntry,
    };
    use lru::LruCache;
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::time::Instant;
    use termwiz::image::{ImageData, ImageDataType};
    use termwiz::surface::{SequenceNo, SEQ_ZERO};
    use wezterm_term::Line;

    fn test_image(width: u32, height: u32, fill: u8) -> Arc<ImageData> {
        Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            width,
            height,
            vec![fill; (width * height * 4) as usize],
        )))
    }

    #[test]
    fn initial_last_poll_allows_immediate_first_poll() {
        let now = Instant::now();
        let initial = initial_last_poll(now);

        assert!(now.duration_since(initial) >= base_poll_interval());
    }

    #[test]
    fn unilateral_deltas_must_not_rewind_seqno() {
        let current: SequenceNo = 10;

        assert!(should_apply_unilateral_delta(current, 10));
        assert!(should_apply_unilateral_delta(current, 11));
        assert!(!should_apply_unilateral_delta(current, 9));
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
    fn image_lru_evicts_by_decoded_bytes() {
        let mut cache = ImageLru::new(NonZeroUsize::new(8).unwrap(), 32);
        let first = test_image(2, 2, 1);
        let second = test_image(2, 2, 2);
        let third = test_image(2, 2, 3);
        let first_hash = first.hash();
        let second_hash = second.hash();
        let third_hash = third.hash();

        cache.put(first);
        cache.put(second);
        cache.put(third);

        assert_eq!(cache.len(), 2);
        assert_eq!(cache.retained_bytes(), 32);
        assert!(cache.get(&first_hash).is_none());
        assert!(cache.get(&second_hash).is_some());
        assert!(cache.get(&third_hash).is_some());
    }

    #[test]
    fn image_lru_refuses_single_image_over_budget() {
        let mut cache = ImageLru::new(NonZeroUsize::new(8).unwrap(), 8);
        let oversized = test_image(2, 2, 4);
        let hash = oversized.hash();

        cache.put(oversized);

        assert_eq!(cache.len(), 0);
        assert_eq!(cache.retained_bytes(), 0);
        assert!(cache.get(&hash).is_none());
    }
}
