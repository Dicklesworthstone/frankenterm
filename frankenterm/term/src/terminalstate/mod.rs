// The range_plus_one lint can't see when the LHS is not compatible with
// and inclusive range
#![allow(clippy::range_plus_one)]
use super::*;
use crate::color::{ColorPalette, RgbColor};
use crate::config::{BidiMode, NewlineCanon};
use frankenterm_bidi::ParagraphDirectionHint;
use frankenterm_cell::image::ImageData;
use frankenterm_cell::UnicodeVersion;
use frankenterm_escape_parser::csi::{
    Cursor, CursorStyle, DecPrivateMode, DecPrivateModeCode, Device, Edit, EraseInDisplay,
    EraseInLine, Mode, Sgr, TabulationClear, TerminalMode, TerminalModeCode, Window, XtSmGraphics,
    XtSmGraphicsAction, XtSmGraphicsItem, XtSmGraphicsStatus, XtermKeyModifierResource,
};
use frankenterm_escape_parser::{OneBased, OperatingSystemCommand, CSI};
use frankenterm_surface::{CursorShape, CursorVisibility, SequenceNo};
use log::debug;
use num_traits::ToPrimitive;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::io::{BufWriter, Write};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use terminfo::{Database, Value};
use termwiz::input::KeyboardEncoding;
use url::Url;

#[cfg(feature = "use_serde")]
pub mod checkpoint;
mod image;
mod iterm;
mod keyboard;
mod kitty;
mod mouse;
pub(crate) mod performer;
mod sixel;
use crate::terminalstate::image::*;
use crate::terminalstate::kitty::*;

lazy_static::lazy_static! {
    static ref DB: Database = {
        let data = include_bytes!("../../../termwiz/data/wezterm");
        Database::from_buffer(&data[..]).unwrap()
    };
}

pub(crate) struct TabStop {
    tabs: Vec<bool>,
    tab_width: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CharSet {
    Ascii,
    Uk,
    DecLineDrawing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseEncoding {
    X10,
    Utf8,
    SGR,
    SgrPixels,
}

fn usize_to_i64_saturating(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn usize_to_u32_saturating(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn i64_to_u32_saturating(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(if value < 0 { 0 } else { u32::MAX })
}

fn add_u32_to_usize_saturating(value: usize, delta: u32) -> usize {
    value.saturating_add(delta as usize)
}

fn add_u32_to_i64_saturating(value: i64, delta: u32) -> i64 {
    value.saturating_add(i64::from(delta))
}

fn next_col_saturating(col: usize) -> usize {
    col.saturating_add(1)
}

fn next_row_saturating(row: i64) -> i64 {
    row.saturating_add(1)
}

fn last_col_in(range: &std::ops::Range<usize>) -> usize {
    range.end.saturating_sub(1)
}

fn last_row_in(range: &std::ops::Range<i64>) -> i64 {
    // Clamp to 0 so an empty range yields a valid (non-negative) row index,
    // matching the usize sibling `last_col_in` (which saturates at 0). For all
    // real, non-empty scroll regions `end >= 1`, so this `.max(0)` is a no-op;
    // it only guards the degenerate empty-range edge against a negative index.
    range.end.saturating_sub(1).max(0)
}

fn last_col_for_width(cols: usize) -> usize {
    cols.saturating_sub(1)
}

fn last_row_for_height(rows: usize) -> i64 {
    // Clamp to 0 to mirror the usize sibling `last_col_for_width`; a 0-height
    // screen yields row 0 rather than a negative index. Real screens have
    // `rows >= 1`, so this is a no-op outside the degenerate zero-height case.
    usize_to_i64_saturating(rows).saturating_sub(1).max(0)
}

fn next_sequence_no(seqno: SequenceNo) -> SequenceNo {
    // MAX is a permanent exhausted sentinel, never a valid recovery witness.
    // Keep rendering operational after exhaustion; checkpoint capture and its
    // final validation reject this sentinel instead of reusing its identity.
    seqno.checked_add(1).unwrap_or(SequenceNo::MAX)
}

impl TabStop {
    fn new(screen_width: usize, tab_width: usize) -> Self {
        // Clamp to >= 1: `i % tab_width` here (and in `resize`) is a
        // mod-by-zero panic for a zero width. The only production caller
        // passes the hardcoded default of 8 today, but the invariant lives
        // with the field so future callers can't reintroduce the trap.
        let tab_width = tab_width.max(1);
        let mut tabs = Vec::with_capacity(screen_width);

        for i in 0..screen_width {
            tabs.push((i % tab_width) == 0);
        }
        Self { tabs, tab_width }
    }

    fn set_tab_stop(&mut self, col: usize) {
        if let Some(tab) = self.tabs.get_mut(col) {
            *tab = true;
        }
    }

    fn find_prev_tab_stop(&self, col: usize) -> Option<usize> {
        for i in (0..col.min(self.tabs.len())).rev() {
            if self.tabs[i] {
                return Some(i);
            }
        }
        None
    }

    fn find_next_tab_stop(&self, col: usize) -> Option<usize> {
        let start = col.saturating_add(1).min(self.tabs.len());
        for i in start..self.tabs.len() {
            if self.tabs[i] {
                return Some(i);
            }
        }
        None
    }

    /// Respond to the terminal resizing.
    /// If the screen got bigger, we need to expand the tab stops
    /// into the new columns with the appropriate width.
    fn resize(&mut self, screen_width: usize) {
        let current = self.tabs.len();
        if screen_width > current {
            for i in current..screen_width {
                self.tabs.push((i % self.tab_width) == 0);
            }
        }
    }

    fn clear(&mut self, to_clear: TabulationClear, col: usize, log_unknown_escape_sequences: bool) {
        match to_clear {
            TabulationClear::ClearCharacterTabStopAtActivePosition => {
                if let Some(t) = self.tabs.get_mut(col) {
                    *t = false;
                }
            }
            // If we want to exactly match VT100/xterm behavior, then
            // we cannot honor ClearCharacterTabStopsAtActiveLine.
            TabulationClear::ClearAllCharacterTabStops => {
                // | TabulationClear::ClearCharacterTabStopsAtActiveLine
                self.tabs.fill(false);
            }
            _ => {
                if log_unknown_escape_sequences {
                    log::warn!("unhandled TabulationClear {:?}", to_clear);
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SavedCursor {
    pub(crate) position: CursorPosition,
    pub(crate) wrap_next: bool,
    pub(crate) wrap_next_column: Option<usize>,
    pen: CellAttributes,
    dec_origin_mode: bool,
    g0_charset: CharSet,
    g1_charset: CharSet,
    // TODO: selective_erase when supported
}

struct ScreenOrAlt {
    /// The primary screen + scrollback
    screen: Screen,
    /// The alternate screen; no scrollback
    alt_screen: Screen,
    /// Tells us which screen is active
    alt_screen_is_active: bool,
}

impl Deref for ScreenOrAlt {
    type Target = Screen;

    fn deref(&self) -> &Screen {
        if self.alt_screen_is_active {
            &self.alt_screen
        } else {
            &self.screen
        }
    }
}

impl DerefMut for ScreenOrAlt {
    fn deref_mut(&mut self) -> &mut Screen {
        if self.alt_screen_is_active {
            &mut self.alt_screen
        } else {
            &mut self.screen
        }
    }
}

impl ScreenOrAlt {
    pub fn new(
        size: TerminalSize,
        config: &Arc<dyn TerminalConfiguration>,
        seqno: SequenceNo,
        bidi_mode: BidiMode,
    ) -> Self {
        let screen = Screen::new(size, config, true, seqno, bidi_mode);
        let alt_screen = Screen::new(size, config, false, seqno, bidi_mode);

        Self {
            screen,
            alt_screen,
            alt_screen_is_active: false,
        }
    }

    pub fn resize(
        &mut self,
        size: TerminalSize,
        cursor_main: CursorPosition,
        cursor_alt: CursorPosition,
        seqno: SequenceNo,
        is_conpty: bool,
        prepared: Option<&mut ScreenReflowPreparation>,
    ) -> (CursorPosition, CursorPosition) {
        let cursor_main =
            self.screen
                .resize_with_prepared_reflow(size, cursor_main, seqno, is_conpty, prepared);
        let cursor_alt = self.alt_screen.resize(size, cursor_alt, seqno, is_conpty);
        (cursor_main, cursor_alt)
    }

    pub fn activate_alt_screen(&mut self, seqno: SequenceNo) {
        if !self.alt_screen_is_active {
            self.screen.invalidate_coordinate_witnesses();
            self.alt_screen.invalidate_coordinate_witnesses();
        }
        self.alt_screen_is_active = true;
        self.dirty_top_phys_rows(seqno);
    }

    pub fn activate_primary_screen(&mut self, seqno: SequenceNo) {
        if self.alt_screen_is_active {
            self.screen.invalidate_coordinate_witnesses();
            self.alt_screen.invalidate_coordinate_witnesses();
        }
        self.alt_screen_is_active = false;
        self.dirty_top_phys_rows(seqno);
    }

    // When switching between alt and primary screen, we implicitly change
    // the content associated with StableRowIndex 0..num_rows.  The muxer
    // use case needs to know to invalidate its cache, so we mark those rows
    // as dirty.
    fn dirty_top_phys_rows(&mut self, seqno: SequenceNo) {
        let num_rows = self.screen.physical_rows;
        for line_idx in 0..num_rows {
            self.screen.touch_phys_row(line_idx, seqno);
        }
    }

    pub fn is_alt_screen_active(&self) -> bool {
        self.alt_screen_is_active
    }

    pub fn saved_cursor(&mut self) -> &mut Option<SavedCursor> {
        if self.alt_screen_is_active {
            &mut self.alt_screen.saved_cursor
        } else {
            &mut self.screen.saved_cursor
        }
    }

    pub fn full_reset(&mut self) {
        self.screen.full_reset();
        self.alt_screen.full_reset();
    }

    pub fn set_config(&mut self, config: &Arc<dyn TerminalConfiguration>) {
        self.screen.set_config(config);
        self.alt_screen.set_config(config);
    }

    #[cfg(feature = "use_serde")]
    fn install_prepared_config(
        &mut self,
        config: &Arc<dyn TerminalConfiguration>,
        resize_wrap_policy: crate::screen::ResizeWrapPolicy,
    ) {
        self.screen
            .install_prepared_config(config, resize_wrap_policy);
        self.alt_screen
            .install_prepared_config(config, resize_wrap_policy);
    }

    #[cfg(feature = "use_serde")]
    fn activate_recovered_scrollback(
        &mut self,
        config: &Arc<dyn TerminalConfiguration>,
    ) -> Result<(), crate::config::ScrollbackActivationError> {
        // Validate every non-primary invariant before the primary screen may
        // publish a durable replacement. After that publication succeeds, the
        // remaining operations are infallible marker/config transitions.
        self.alt_screen.preflight_recovery_without_scrollback()?;
        self.screen.activate_recovered_scrollback(config)?;
        self.alt_screen.finish_recovery_without_scrollback();
        Ok(())
    }
}

/// Configuration that per-character and per-row ingest paths read, captured
/// at the start of every parse batch (ft-yccm0.2.4). The GUI's `TermConfig`
/// locks a mutex and clones an `Arc` on every read, which `print` used to pay
/// per character. A change takes effect at the next batch boundary.
#[derive(Clone, Debug)]
struct BatchConfig {
    max_accumulating_title_len: usize,
    normalize_output_to_unicode_nfc: bool,
    bidi_mode: BidiMode,
}

impl BatchConfig {
    fn capture(config: &dyn TerminalConfiguration) -> Self {
        Self {
            max_accumulating_title_len: config.max_accumulating_title_len(),
            normalize_output_to_unicode_nfc: config.normalize_output_to_unicode_nfc(),
            bidi_mode: config.bidi_mode(),
        }
    }
}

/// Manages the state for the terminal
pub struct TerminalState {
    config: Arc<dyn TerminalConfiguration>,
    batch_config: BatchConfig,

    screen: ScreenOrAlt,
    /// The current set of attributes in effect for the next
    /// attempt to print to the display
    pen: CellAttributes,
    /// The current cursor position, relative to the top left
    /// of the screen.  0-based index.
    cursor: CursorPosition,

    /// if true, implicitly move to the next line on the next
    /// printed character
    wrap_next: bool,

    clear_semantic_attribute_on_newline: bool,
    last_semantic_command_status: Option<i32>,

    /// If true, writing a character inserts a new cell
    insert: bool,

    /// https://vt100.net/docs/vt510-rm/DECAWM.html
    dec_auto_wrap: bool,

    /// One-level XTSAVE/XTRESTORE cache for DEC private mode values.
    saved_dec_private_modes: HashMap<u16, bool>,

    /// Reverse Wraparound Mode
    reverse_wraparound_mode: bool,

    /// Reverse video mode
    reverse_video_mode: bool,

    /// DEC private mode 2026 — synchronized output / atomic frame
    /// buffering. When enabled, applications signal that a multi-line
    /// redraw is in progress; renderers should hold presentation
    /// until the mode is reset, eliminating tearing on fast redraws
    /// from Neovim, lazygit, btop, ranger, etc.
    ///
    /// This flag is the term-layer source of truth (ft-d7af6). The
    /// renderer presentation-hold integration consumes
    /// `TerminalState::synchronized_output()` and is tracked under
    /// the continuation bead.
    synchronized_output: bool,

    /// https://vt100.net/docs/vt510-rm/DECOM.html
    /// When OriginMode is enabled, cursor is constrained to the
    /// scroll region and its position is relative to the scroll
    /// region.
    dec_origin_mode: bool,

    /// The scroll region
    top_and_bottom_margins: Range<VisibleRowIndex>,
    left_and_right_margins: Range<usize>,
    left_and_right_margin_mode: bool,

    /// When set, modifies the sequence of bytes sent for keys
    /// designated as cursor keys.  This includes various navigation
    /// keys.  The code in key_down() is responsible for interpreting this.
    application_cursor_keys: bool,
    modify_other_keys: Option<i64>,

    dec_ansi_mode: bool,

    /// https://vt100.net/dec/ek-vt38t-ug-001.pdf#page=132 has a
    /// discussion on what sixel dispay mode (DECSDM) does.
    sixel_display_mode: bool,
    use_private_color_registers_for_each_graphic: bool,

    /// Graphics mode color register map.
    color_map: HashMap<u16, RgbColor>,

    /// When set, modifies the sequence of bytes sent for keys
    /// in the numeric keypad portion of the keyboard.
    application_keypad: bool,

    /// When set, pasting the clipboard should bracket the data with
    /// designated marker characters.
    bracketed_paste: bool,

    /// Movement events enabled
    any_event_mouse: bool,
    focus_tracking: bool,
    /// X10 (legacy), SGR, and SGR-Pixels style mouse tracking and
    /// reporting is enabled
    mouse_encoding: MouseEncoding,
    mouse_tracking: bool,
    /// Button events enabled
    button_event_mouse: bool,
    current_mouse_buttons: Vec<MouseButton>,
    last_mouse_move: Option<MouseEvent>,
    cursor_visible: bool,

    keyboard_encoding: KeyboardEncoding,
    /// Support for US, UK, and DEC Special Graphics
    g0_charset: CharSet,
    g1_charset: CharSet,
    shift_out: bool,

    newline_mode: bool,

    tabs: TabStop,

    /// The terminal title string (OSC 2)
    title: String,
    /// The icon title string (OSC 1)
    icon_title: Option<String>,
    progress: Progress,

    palette: Option<ColorPalette>,

    pixel_width: usize,
    pixel_height: usize,
    dpi: u32,

    clipboard: Option<Arc<dyn Clipboard>>,
    osc52_prompt: crate::terminal::Osc52PromptSlot,
    device_control_handler: Option<Box<dyn DeviceControlHandler>>,
    alert_handler: Option<Box<dyn AlertHandler>>,
    download_handler: Option<Arc<dyn DownloadHandler>>,

    current_dir: Option<Url>,

    term_program: String,
    term_version: String,

    writer: BufWriter<ThreadedWriter>,
    #[cfg_attr(not(feature = "use_serde"), allow(dead_code))]
    writer_is_inert: bool,

    image_cache: lru::LruCache<[u8; 32], Arc<ImageData>>,
    sixel_scrolls_right: bool,

    user_vars: HashMap<String, String>,

    kitty_img: KittyImageState,
    seqno: SequenceNo,

    /// The unicode version that is in effect
    unicode_version: UnicodeVersion,
    unicode_version_stack: Vec<UnicodeVersionStackEntry>,

    /// Whether any cell of either screen may hold text ending in U+200D (ZWJ).
    /// New grapheme text reaches cells only through the performer, so this
    /// becomes true the first time it writes such a cell and stays true. Other
    /// writers copy or re-lay-out existing cells (reflow, rectangle copies, and
    /// cold-history layouts read back from this pane's own spill store), which
    /// cannot introduce a ZWJ tail this performer never wrote. Restored
    /// checkpoints start true because their cells came from elsewhere.
    /// While false, a multi-byte grapheme cannot continue its left neighbour,
    /// so `print` skips the cluster-continuation scan entirely.
    zwj_tail_cell_possible: bool,

    enable_conpty_quirks: bool,
    /// On Windows, the ConPTY layer emits an OSC sequence to
    /// set the title shortly after it starts up.
    /// We don't want that, so we use this flag to remember
    /// whether we want to skip it or not.
    suppress_initial_title_change: bool,

    accumulating_title: Option<String>,

    /// seqno when we last lost focus
    lost_focus_seqno: SequenceNo,
    /// seqno when we last emitted Alert::OutputSinceFocusLost
    lost_focus_alerted_seqno: SequenceNo,
    focused: bool,

    /// True if lines should be marked as bidi-enabled, and thus
    /// have the renderer apply the bidi algorithm.
    /// true is equivalent to "implicit" bidi mode as described in
    /// <https://terminal-wg.pages.freedesktop.org/bidi/recommendation/basic-modes.html>
    /// If none, then the default value specified by the config is used.
    bidi_enabled: Option<bool>,
    /// When set, specifies the bidi direction information that should be
    /// applied to lines.
    /// If none, then the default value specified by the config is used.
    bidi_hint: Option<ParagraphDirectionHint>,
}

#[derive(Debug)]
struct UnicodeVersionStackEntry {
    vers: UnicodeVersion,
    label: Option<String>,
}

fn default_color_map() -> HashMap<u16, RgbColor> {
    let mut color_map = HashMap::new();
    // Match colors to the VT340 color table:
    // https://github.com/hackerb9/vt340test/blob/main/colormap/showcolortable.png
    for (idx, r, g, b) in [
        (0, 0, 0, 0),
        (1, 0x33, 0x33, 0xcc),
        (2, 0xcc, 0x23, 0x23),
        (3, 0x33, 0xcc, 0x33),
        (4, 0xcc, 0x33, 0xcc),
        (5, 0x33, 0xcc, 0xcc),
        (6, 0xcc, 0xcc, 0xcc),
        (7, 0x77, 0x77, 0x77),
        (8, 0x44, 0x44, 0x44),
        (9, 0x56, 0x56, 0x99),
        (10, 0x99, 0x44, 0x44),
        (11, 0x56, 0x99, 0x56),
        (12, 0x99, 0x56, 0x99),
        (13, 0x56, 0x99, 0x99),
        (14, 0x99, 0x99, 0x56),
        (15, 0xcc, 0xcc, 0xcc),
    ] {
        color_map.insert(idx, RgbColor::new_8bpc(r, g, b));
    }
    color_map
}

/// This struct implements a writer that sends the data across
/// to another thread so that the write side of the terminal
/// processing never blocks.
///
/// This is important for example when processing large pastes into
/// vim.  In that scenario, we can fill up the data pending
/// on vim's input buffer, while it is busy trying to send
/// output to the terminal.  A deadlock is reached because
/// send_paste blocks on the writer, but it is unable to make
/// progress until we're able to read the output from vim.
///
/// We either need input or output to be non-blocking.
/// Output seems safest because we want to be able to exert
/// back-pressure when there is a lot of data to read,
/// and we're in control of the write side, which represents
/// input from the interactive user, or pastes.
///
/// Neither `write` nor `flush` ever waits for the writer thread: a child that
/// stops reading its input must not stall the parser or the GUI while they
/// hold the terminal lock (ft-yccm0.2.2.5). Completion is observable only
/// through [`TerminalState::writer_barrier`].
enum ThreadedWriter {
    Live {
        sender: Sender<WriterMessage>,
        counters: Arc<WriterCounters>,
        /// The class of every byte written until the next class change.
        class: WriteClass,
        /// Whether the reply unit in progress (the writes since the last
        /// flush) is being dropped. Decided once per unit, so a reply reaches
        /// the child whole or not at all.
        reply_unit_dropped: Option<bool>,
    },
    #[cfg(feature = "use_serde")]
    Inert,
    Failed,
}

#[cfg(feature = "use_serde")]
pub(crate) struct PreparedTerminalWriter(BufWriter<ThreadedWriter>);

#[cfg(feature = "use_serde")]
pub(crate) struct PreparedRecoveryConfiguration {
    config: Arc<dyn TerminalConfiguration>,
    resize_wrap_policy: crate::screen::ResizeWrapPolicy,
    kitty_image_budget_bytes: usize,
    kitty_image_max_transmission_bytes: usize,
    refreshed_unicode_version: Option<UnicodeVersion>,
}

/// Who produced bytes headed for the child's input.
///
/// Both classes share one FIFO, so replies stay ordered relative to user
/// input; they differ only under backpressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteClass {
    /// Keystrokes, pastes, mouse and focus reports. Never dropped.
    UserInput,
    /// Answers to the child's own queries (DA, DSR/CPR, DECRQSS, DECRQM,
    /// XTVERSION, OSC color queries, kitty graphics acknowledgements). Dropped
    /// whole, and counted, once [`REPLY_BACKLOG_LIMIT`] is reached.
    Reply,
}

/// Unwritten reply bytes at which further replies are dropped. A child with
/// this many unanswered query replies has stopped reading its input, and a
/// backlog of stale answers is useless to it. User input is never dropped.
pub const REPLY_BACKLOG_LIMIT: usize = 64 * 1024;

/// The largest paste [`TerminalState::send_paste`] accepts. A larger paste is
/// refused whole with [`PasteTooLarge`] rather than truncated.
pub const MAX_PASTE_BYTES: usize = 64 * 1024 * 1024;

/// A paste refused because it is larger than [`MAX_PASTE_BYTES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteTooLarge {
    pub len: usize,
    pub limit: usize,
}

impl std::fmt::Display for PasteTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "paste of {} bytes refused: the limit is {} bytes; nothing was sent",
            self.len, self.limit
        )
    }
}

impl std::error::Error for PasteTooLarge {}

/// Writer queue accounting, as reported by [`TerminalState::writer_backlog`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriterBacklog {
    /// User-input bytes enqueued but not yet written to the child.
    pub pending_input_bytes: usize,
    /// Reply bytes enqueued but not yet written to the child.
    pub pending_reply_bytes: usize,
    /// Reply units dropped because the reply backlog was full.
    pub dropped_replies: u64,
    /// Bytes in those dropped replies.
    pub dropped_reply_bytes: u64,
}

#[derive(Debug, Default)]
struct WriterCounters {
    pending_input_bytes: AtomicUsize,
    pending_reply_bytes: AtomicUsize,
    dropped_replies: AtomicU64,
    dropped_reply_bytes: AtomicU64,
}

impl WriterCounters {
    fn pending(&self, class: WriteClass) -> &AtomicUsize {
        match class {
            WriteClass::UserInput => &self.pending_input_bytes,
            WriteClass::Reply => &self.pending_reply_bytes,
        }
    }

    fn snapshot(&self) -> WriterBacklog {
        WriterBacklog {
            pending_input_bytes: self.pending_input_bytes.load(Ordering::Relaxed),
            pending_reply_bytes: self.pending_reply_bytes.load(Ordering::Relaxed),
            dropped_replies: self.dropped_replies.load(Ordering::Relaxed),
            dropped_reply_bytes: self.dropped_reply_bytes.load(Ordering::Relaxed),
        }
    }
}

/// Completion of everything a terminal enqueued for its child before the
/// barrier was taken; see [`TerminalState::writer_barrier`].
#[must_use = "a barrier does nothing unless waited on"]
pub struct WriterBarrier {
    state: WriterBarrierState,
}

enum WriterBarrierState {
    Pending(Receiver<std::io::Result<()>>),
    /// An inert (checkpoint-restored, not yet activated) writer has nothing
    /// to drain.
    #[cfg_attr(not(feature = "use_serde"), allow(dead_code))]
    Complete,
    Unavailable(std::io::ErrorKind),
}

impl WriterBarrier {
    /// Blocks until the writer thread has written and flushed every byte
    /// enqueued before the barrier, or until `timeout` elapses.
    ///
    /// A child that stops reading its input blocks this indefinitely, so it
    /// must never run on the GUI main thread or while the terminal lock is
    /// held: take the barrier under the lock, release the lock, then wait.
    pub fn wait(self, timeout: std::time::Duration) -> std::io::Result<()> {
        match self.state {
            WriterBarrierState::Complete => Ok(()),
            WriterBarrierState::Unavailable(kind) => {
                Err(std::io::Error::new(kind, "terminal writer is unavailable"))
            }
            WriterBarrierState::Pending(ack) => match ack.recv_timeout(timeout) {
                Ok(result) => result,
                Err(RecvTimeoutError::Timeout) => Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "terminal writer did not drain before the timeout",
                )),
                Err(RecvTimeoutError::Disconnected) => Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "terminal writer stopped before the barrier",
                )),
            },
        }
    }
}

enum WriterMessage {
    Data { bytes: Vec<u8>, class: WriteClass },
    Flush,
    Barrier(Sender<std::io::Result<()>>),
}

impl ThreadedWriter {
    fn new(writer: Box<dyn std::io::Write + Send>) -> Self {
        match Self::try_new(writer) {
            Ok(writer) => writer,
            Err(err) => {
                log::error!("failed to spawn terminal threaded writer: {err:#}");
                Self::Failed
            }
        }
    }

    fn try_new(mut writer: Box<dyn std::io::Write + Send>) -> std::io::Result<Self> {
        let (sender, receiver) = channel::<WriterMessage>();
        const THREAD_NAME: &str = "terminal-threaded-writer";
        let mut thread_name = String::new();
        thread_name
            .try_reserve_exact(THREAD_NAME.len())
            .map_err(|_| std::io::Error::other("terminal writer name allocation failed"))?;
        thread_name.push_str(THREAD_NAME);

        let counters = Arc::new(WriterCounters::default());
        let thread_counters = Arc::clone(&counters);
        std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                while let Ok(msg) = receiver.recv() {
                    match msg {
                        WriterMessage::Data { bytes, class } => {
                            let result = writer.write_all(&bytes);
                            thread_counters
                                .pending(class)
                                .fetch_sub(bytes.len(), Ordering::Relaxed);
                            if result.is_err() {
                                break;
                            }
                        }
                        WriterMessage::Flush => {
                            if writer.flush().is_err() {
                                break;
                            }
                        }
                        WriterMessage::Barrier(ack) => {
                            let result = writer.flush();
                            let should_break = result.is_err();
                            let _ = ack.send(result);
                            if should_break {
                                break;
                            }
                        }
                    }
                }
                // The child's input is gone. Discard what is still queued so
                // the backlog counters stay truthful; pending barriers see a
                // disconnect and report BrokenPipe.
                for msg in receiver.try_iter() {
                    if let WriterMessage::Data { bytes, class } = msg {
                        thread_counters
                            .pending(class)
                            .fetch_sub(bytes.len(), Ordering::Relaxed);
                    }
                }
            })?;

        Ok(Self::Live {
            sender,
            counters,
            class: WriteClass::Reply,
            reply_unit_dropped: None,
        })
    }

    #[cfg(feature = "use_serde")]
    const fn inert() -> Self {
        Self::Inert
    }

    fn class(&self) -> WriteClass {
        match self {
            Self::Live { class, .. } => *class,
            #[cfg(feature = "use_serde")]
            Self::Inert => WriteClass::Reply,
            Self::Failed => WriteClass::Reply,
        }
    }

    /// Changes the class of subsequent writes. The caller flushes first, so
    /// no buffered byte changes class.
    fn set_class(&mut self, new_class: WriteClass) {
        if let Self::Live {
            class,
            reply_unit_dropped,
            ..
        } = self
        {
            *class = new_class;
            *reply_unit_dropped = None;
        }
    }

    fn backlog(&self) -> WriterBacklog {
        match self {
            Self::Live { counters, .. } => counters.snapshot(),
            #[cfg(feature = "use_serde")]
            Self::Inert => WriterBacklog::default(),
            Self::Failed => WriterBacklog::default(),
        }
    }

    fn barrier(&self) -> WriterBarrier {
        let state = match self {
            Self::Live { sender, .. } => {
                let (ack_sender, ack_receiver) = channel();
                match sender.send(WriterMessage::Barrier(ack_sender)) {
                    Ok(()) => WriterBarrierState::Pending(ack_receiver),
                    Err(_) => WriterBarrierState::Unavailable(std::io::ErrorKind::BrokenPipe),
                }
            }
            #[cfg(feature = "use_serde")]
            Self::Inert => WriterBarrierState::Complete,
            Self::Failed => WriterBarrierState::Unavailable(std::io::ErrorKind::BrokenPipe),
        };
        WriterBarrier { state }
    }
}

impl std::io::Write for ThreadedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Live {
                sender,
                counters,
                class,
                reply_unit_dropped,
            } => {
                if *class == WriteClass::Reply {
                    let dropped = *reply_unit_dropped.get_or_insert_with(|| {
                        let full = counters.pending_reply_bytes.load(Ordering::Relaxed)
                            >= REPLY_BACKLOG_LIMIT;
                        if full {
                            let dropped = counters.dropped_replies.fetch_add(1, Ordering::Relaxed) + 1;
                            if dropped.is_power_of_two() {
                                log::warn!(
                                    "terminal reply backlog is full ({REPLY_BACKLOG_LIMIT} bytes); the child is not reading its input, {dropped} replies dropped so far"
                                );
                            }
                        }
                        full
                    });
                    if dropped {
                        counters
                            .dropped_reply_bytes
                            .fetch_add(buf.len() as u64, Ordering::Relaxed);
                        return Ok(buf.len());
                    }
                }
                let mut owned = Vec::new();
                owned
                    .try_reserve_exact(buf.len())
                    .map_err(|_| std::io::Error::other("terminal writer allocation failed"))?;
                owned.extend_from_slice(buf);
                let pending = counters.pending(*class);
                pending.fetch_add(buf.len(), Ordering::Relaxed);
                if let Err(err) = sender.send(WriterMessage::Data {
                    bytes: owned,
                    class: *class,
                }) {
                    pending.fetch_sub(buf.len(), Ordering::Relaxed);
                    return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, err));
                }
                Ok(buf.len())
            }
            #[cfg(feature = "use_serde")]
            Self::Inert => Ok(buf.len()),
            Self::Failed => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "terminal writer is unavailable",
            )),
        }
    }

    /// Enqueues a flush and returns at once. It never waits for the writer
    /// thread; use [`TerminalState::writer_barrier`] to observe completion.
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Live {
                sender,
                reply_unit_dropped,
                ..
            } => {
                // A flush ends the reply unit in progress.
                *reply_unit_dropped = None;
                sender
                    .send(WriterMessage::Flush)
                    .map_err(|err| std::io::Error::new(std::io::ErrorKind::BrokenPipe, err))
            }
            #[cfg(feature = "use_serde")]
            Self::Inert => Ok(()),
            Self::Failed => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "terminal writer is unavailable",
            )),
        }
    }
}

impl TerminalState {
    /// Constructs the terminal state.
    /// You generally want the `Terminal` struct rather than this one;
    /// Terminal contains and dereferences to `TerminalState`.
    pub fn new(
        size: TerminalSize,
        config: Arc<dyn TerminalConfiguration>,
        term_program: &str,
        term_version: &str,
        writer: Box<dyn std::io::Write + Send>,
    ) -> TerminalState {
        Self::new_with_writer(
            size,
            config,
            term_program,
            term_version,
            ThreadedWriter::new(writer),
            false,
        )
    }

    fn new_with_writer(
        size: TerminalSize,
        config: Arc<dyn TerminalConfiguration>,
        term_program: &str,
        term_version: &str,
        writer: ThreadedWriter,
        writer_is_inert: bool,
    ) -> TerminalState {
        let seqno = 1;
        let screen = ScreenOrAlt::new(size, &config, seqno, config.bidi_mode());
        Self::new_with_prebuilt_screen(
            size,
            config,
            term_program,
            term_version,
            writer,
            writer_is_inert,
            seqno,
            screen,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_prebuilt_screen(
        size: TerminalSize,
        config: Arc<dyn TerminalConfiguration>,
        term_program: &str,
        term_version: &str,
        writer: ThreadedWriter,
        writer_is_inert: bool,
        seqno: SequenceNo,
        screen: ScreenOrAlt,
    ) -> TerminalState {
        let writer = BufWriter::new(writer);

        let color_map = default_color_map();

        let unicode_version = config.unicode_version();
        let kitty_budget = config.kitty_image_budget_bytes();
        let kitty_max_transmission = config.kitty_image_max_transmission_bytes();
        let batch_config = BatchConfig::capture(config.as_ref());

        TerminalState {
            config,
            batch_config,
            screen,
            pen: CellAttributes::default(),
            cursor: CursorPosition::default(),
            top_and_bottom_margins: 0..size.rows as VisibleRowIndex,
            left_and_right_margins: 0..size.cols,
            left_and_right_margin_mode: false,
            wrap_next: false,
            clear_semantic_attribute_on_newline: false,
            last_semantic_command_status: None,
            // We default auto wrap to true even though the default for
            // a dec terminal is false, because it is more useful this way.
            dec_auto_wrap: true,
            saved_dec_private_modes: HashMap::new(),
            reverse_wraparound_mode: false,
            reverse_video_mode: false,
            synchronized_output: false,
            dec_origin_mode: false,
            insert: false,
            application_cursor_keys: false,
            modify_other_keys: None,
            dec_ansi_mode: false,
            sixel_display_mode: false,
            use_private_color_registers_for_each_graphic: false,
            color_map,
            application_keypad: false,
            bracketed_paste: false,
            focus_tracking: false,
            mouse_encoding: MouseEncoding::X10,
            keyboard_encoding: KeyboardEncoding::Xterm,
            sixel_scrolls_right: false,
            any_event_mouse: false,
            button_event_mouse: false,
            mouse_tracking: false,
            last_mouse_move: None,
            cursor_visible: true,
            g0_charset: CharSet::Ascii,
            g1_charset: CharSet::Ascii,
            shift_out: false,
            newline_mode: false,
            current_mouse_buttons: vec![],
            tabs: TabStop::new(size.cols, 8),
            title: "frankenterm".to_string(),
            icon_title: None,
            palette: None,
            pixel_height: size.pixel_height,
            pixel_width: size.pixel_width,
            dpi: size.dpi,
            clipboard: None,
            osc52_prompt: crate::terminal::Osc52PromptSlot::default(),
            device_control_handler: None,
            alert_handler: None,
            download_handler: None,
            current_dir: None,
            term_program: term_program.to_string(),
            term_version: term_version.to_string(),
            writer,
            writer_is_inert,
            image_cache: lru::LruCache::new(NonZeroUsize::new(16).unwrap()),
            user_vars: HashMap::new(),
            kitty_img: {
                let mut kitty = KittyImageState::default();
                kitty.image_budget_bytes = kitty_budget;
                kitty.set_max_transmission_bytes(kitty_max_transmission);
                kitty
            },
            seqno,
            unicode_version,
            unicode_version_stack: vec![],
            zwj_tail_cell_possible: false,
            suppress_initial_title_change: false,
            enable_conpty_quirks: false,
            accumulating_title: None,
            lost_focus_seqno: seqno,
            lost_focus_alerted_seqno: seqno,
            focused: true,
            bidi_enabled: None,
            bidi_hint: None,
            progress: Progress::default(),
        }
    }

    #[cfg(feature = "use_serde")]
    pub(crate) fn prepare_inert_writer(
        &self,
        writer: Box<dyn std::io::Write + Send>,
    ) -> std::io::Result<PreparedTerminalWriter> {
        if !self.writer_is_inert {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "terminal writer is already live",
            ));
        }
        // Capacity zero avoids an infallible allocation in the activation
        // transaction. ThreadedWriter already owns the asynchronous buffering
        // boundary and fallibly allocates each outbound message.
        Ok(PreparedTerminalWriter(BufWriter::with_capacity(
            0,
            ThreadedWriter::try_new(writer)?,
        )))
    }

    #[cfg(feature = "use_serde")]
    pub(crate) fn prepare_recovery_configuration(
        &self,
        config: Arc<dyn TerminalConfiguration>,
    ) -> PreparedRecoveryConfiguration {
        let previous_unicode_version = self.config.unicode_version();
        let should_refresh_unicode_version = self.unicode_version_stack.is_empty()
            && self.unicode_version == previous_unicode_version;
        let refreshed_unicode_version =
            should_refresh_unicode_version.then(|| config.unicode_version());
        let resize_wrap_policy =
            crate::screen::ResizeWrapPolicy::from_terminal_configuration(config.as_ref());
        let kitty_image_budget_bytes = config.kitty_image_budget_bytes();
        let kitty_image_max_transmission_bytes = config.kitty_image_max_transmission_bytes();
        PreparedRecoveryConfiguration {
            config,
            resize_wrap_policy,
            kitty_image_budget_bytes,
            kitty_image_max_transmission_bytes,
            refreshed_unicode_version,
        }
    }

    #[cfg(feature = "use_serde")]
    pub(crate) fn finish_recovery_activation(
        &mut self,
        prepared_config: PreparedRecoveryConfiguration,
        prepared_writer: PreparedTerminalWriter,
    ) -> Result<(), crate::config::ScrollbackActivationError> {
        self.screen
            .activate_recovered_scrollback(&prepared_config.config)?;
        // Refused activation retains the exact retryable canonical model,
        // including its semantic generation. Advance only after the fallible
        // scrollback transaction commits, before installing the live config.
        self.increment_seqno();
        self.screen
            .install_prepared_config(&prepared_config.config, prepared_config.resize_wrap_policy);
        self.kitty_img.image_budget_bytes = prepared_config.kitty_image_budget_bytes;
        self.kitty_img
            .set_max_transmission_bytes(prepared_config.kitty_image_max_transmission_bytes);
        if let Some(unicode_version) = prepared_config.refreshed_unicode_version {
            self.unicode_version = unicode_version;
        }
        self.config = prepared_config.config;
        let inert = std::mem::replace(&mut self.writer, prepared_writer.0);
        self.writer_is_inert = false;
        drop(inert);
        Ok(())
    }

    #[cfg(all(test, feature = "use_serde"))]
    pub(crate) fn writer_is_inert_for_test(&self) -> bool {
        self.writer_is_inert
    }

    pub fn enable_conpty_quirks(&mut self) {
        self.increment_seqno();
        self.enable_conpty_quirks = true;
        self.suppress_initial_title_change = true;
    }

    pub fn current_seqno(&self) -> SequenceNo {
        self.seqno
    }

    pub fn increment_seqno(&mut self) {
        self.seqno = next_sequence_no(self.seqno);
    }

    /// Move the primary screen's resident warm scrollback to the cold tier
    /// (the fleet memory-pressure action). Returns the rows moved; the
    /// sequence number advances only when rows moved, so renderers re-read
    /// the changed scrollback extent.
    pub fn evict_warm_scrollback(&mut self) -> usize {
        self.refresh_batch_config();
        let seqno = next_sequence_no(self.seqno);
        let evicted = self.screen.screen.evict_warm_scrollback(seqno);
        if evicted > 0 {
            self.seqno = seqno;
        }
        evicted
    }

    pub fn set_config(&mut self, config: Arc<dyn TerminalConfiguration>) {
        self.increment_seqno();
        self.osc52_prompt.replace(None);
        let previous_unicode_version = self.config.unicode_version();
        let should_refresh_unicode_version = self.unicode_version_stack.is_empty()
            && self.unicode_version == previous_unicode_version;

        self.screen.set_config(&config);
        self.kitty_img.image_budget_bytes = config.kitty_image_budget_bytes();
        self.kitty_img
            .set_max_transmission_bytes(config.kitty_image_max_transmission_bytes());
        if should_refresh_unicode_version {
            self.unicode_version = config.unicode_version();
        }
        self.config = config;
        self.refresh_batch_config();
    }

    /// Re-reads the configuration the ingest hot paths cache: this state's
    /// [`BatchConfig`] and both screens' scrollback policy. `Terminal` calls
    /// it at the start of every parse batch, and the entry points that size
    /// scrollback outside a batch call it too. The refresh is unconditional:
    /// `TermConfig` folds the live pane count into the tier config without
    /// bumping its revision.
    pub(crate) fn refresh_batch_config(&mut self) {
        self.batch_config = BatchConfig::capture(self.config.as_ref());
        self.screen.screen.refresh_scrollback_policy();
        self.screen.alt_screen.refresh_scrollback_policy();
    }

    pub fn get_config(&self) -> Arc<dyn TerminalConfiguration> {
        Arc::clone(&self.config)
    }

    pub fn set_clipboard(&mut self, clipboard: &Arc<dyn Clipboard>) {
        self.osc52_prompt.replace(None);
        self.clipboard.replace(Arc::clone(clipboard));
    }

    pub fn set_device_control_handler(&mut self, handler: Box<dyn DeviceControlHandler>) {
        self.device_control_handler.replace(handler);
    }

    pub fn set_notification_handler(&mut self, handler: Box<dyn AlertHandler>) {
        self.alert_handler.replace(handler);
    }

    pub fn set_download_handler(&mut self, handler: &Arc<dyn DownloadHandler>) {
        self.download_handler.replace(handler.clone());
    }

    /// Returns the title text associated with the terminal session.
    /// The title can be changed by the application using a number
    /// of escape sequences:
    /// OSC 2 is used to set the window title.
    /// OSC 1 is used to set the "icon title", which some terminal
    /// emulators interpret as a shorter title string for use when
    /// showing the tab title.
    /// Here in wezterm the terminalstate is isolated from other
    /// tabs; we process escape sequences without knowledge of other
    /// tabs, so we maintain both title strings here.
    /// The gui layer doesn't currently have a concept of what the
    /// overall window title should be beyond the title for the
    /// active tab with some decoration about the number of tabs.
    /// Shell toolkits such as oh-my-zsh prefer OSC 1 titles for
    /// abbreviated information.
    /// What we do here is prefer to return the OSC 1 icon title
    /// if it is set, otherwise return the OSC 2 window title.
    pub fn get_title(&self) -> &str {
        self.icon_title.as_ref().unwrap_or(&self.title)
    }

    pub fn get_progress(&self) -> Progress {
        self.progress.clone()
    }

    /// Returns the current working directory associated with the
    /// terminal session.  The working directory can be changed by
    /// the applicaiton using the OSC 7 escape sequence.
    pub fn get_current_dir(&self) -> Option<&Url> {
        self.current_dir.as_ref()
    }

    /// Returns a copy of the palette.
    /// By default we don't keep a copy in the terminal state,
    /// preferring to take the config values from the users
    /// config file and updating to changes live.
    /// However, if they have used dynamic color scheme escape
    /// sequences we'll fork a copy of the palette at that time
    /// so that we can start tracking those changes.
    pub fn palette(&self) -> ColorPalette {
        self.palette
            .as_ref()
            .cloned()
            .unwrap_or_else(|| self.config.color_palette())
    }

    /// Called in response to dynamic color scheme escape sequences.
    /// Will make a copy of the palette from the config file if this
    /// is the first of these escapes we've seen.
    pub fn palette_mut(&mut self) -> &mut ColorPalette {
        self.increment_seqno();
        if self.palette.is_none() {
            self.palette.replace(self.config.color_palette());
        }
        self.palette.as_mut().unwrap()
    }

    /// If the current overridden palette is effectively the same as
    /// the configured palette, remove the override and treat it as
    /// being the same as the configured state.
    /// This allows runtime changes to the configuration to take effect.
    pub fn implicit_palette_reset_if_same_as_configured(&mut self) {
        if self
            .palette
            .as_ref()
            .map(|p| *p == self.config.color_palette())
            .unwrap_or(false)
        {
            self.increment_seqno();
            self.palette.take();
        }
    }

    /// Returns a reference to the active screen (either the primary or
    /// the alternate screen).
    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// Returns a mutable reference to the active screen (either the primary or
    /// the alternate screen). A mutable borrow also serves read-side cache
    /// maintenance and selection observation, so it must not advance seqno.
    /// Actual model mutations advance it at their action, resize, or explicit
    /// mutation boundary before publishing changed rows or coordinates.
    pub fn screen_mut(&mut self) -> &mut Screen {
        &mut self.screen
    }

    #[cfg(feature = "use_serde")]
    fn cold_seam_resident_end(&self) -> usize {
        let active = self.screen.phys_row(self.cursor.y);
        self.screen.saved_cursor.as_ref().map_or(active, |saved| {
            active.min(self.screen.phys_row(saved.position.y))
        })
    }

    /// Prefer completed rows before both cursors. An active paragraph may
    /// include the cursor only with an explicit logical-offset mapping and
    /// exact cursor validation at commit. Saved cursors and pending autowrap
    /// currently retain the completed-row-only path.
    #[cfg(feature = "use_serde")]
    pub fn capture_cold_seam_reflow(
        &self,
    ) -> anyhow::Result<Option<crate::screen::ColdSeamReflow>> {
        let completed = self
            .screen
            .capture_cold_seam_reflow_before(self.cold_seam_resident_end())?;
        if completed.is_some() || self.wrap_next || self.screen.saved_cursor.is_some() {
            return Ok(completed);
        }
        let physical_row = self.screen.phys_row(self.cursor.y);
        let mut active = self
            .screen
            .capture_cold_seam_reflow_before(physical_row.saturating_add(1))?;
        if let Some(seam) = &mut active {
            seam.cursor = Some(crate::screen::ColdSeamCursor {
                before: self.cursor,
                physical_row,
                after: None,
            });
        }
        Ok(active)
    }

    /// Recheck cursor authority after off-lock preparation. Moving or saving
    /// a cursor in the paragraph rejects it even when its text is unchanged.
    #[cfg(feature = "use_serde")]
    pub fn install_cold_seam_reflow(
        &mut self,
        prepared: &mut crate::screen::ColdSeamReflow,
        seqno: SequenceNo,
    ) -> anyhow::Result<bool> {
        let mut resident_end = self.cold_seam_resident_end();
        let mapped_cursor = if let Some(cursor) = &prepared.cursor {
            if self.wrap_next
                || self.screen.saved_cursor.is_some()
                || self.cursor.x != cursor.before.x
                || self.cursor.y != cursor.before.y
                || self.cursor.seqno != cursor.before.seqno
            {
                return Ok(false);
            }
            let Some((column, row, pending_wrap)) = cursor.after else {
                return Ok(false);
            };
            let visible_start = self.screen.phys_row(0);
            if row < visible_start
                || column >= self.screen.physical_cols
                || (pending_wrap && !self.dec_auto_wrap)
            {
                return Ok(false);
            }
            resident_end = self.screen.phys_row(self.cursor.y).saturating_add(1);
            Some((column, (row - visible_start) as i64, pending_wrap))
        } else {
            None
        };
        let installed =
            self.screen
                .install_cold_seam_reflow_before(prepared, seqno, resident_end)?;
        if installed {
            if let Some((column, row, pending_wrap)) = mapped_cursor {
                self.cursor.x = column;
                self.cursor.y = row;
                self.cursor.seqno = seqno;
                self.wrap_next = pending_wrap;
            }
        }
        Ok(installed)
    }

    /// Parser-side maintenance of the primary screen, including while the
    /// alternate screen is active. The caller releases the terminal between
    /// slices and drains the sink only when blocked or settled after progress.
    pub fn trim_deferred_scrollback(&mut self) -> DeferredScrollbackTrim {
        self.refresh_batch_config();
        let result = self.screen.screen.trim_deferred_scrollback(self.seqno);
        if result.moved() {
            self.increment_seqno();
        }
        result
    }

    fn set_clipboard_contents(
        &self,
        selection: ClipboardSelection,
        text: Option<String>,
    ) -> anyhow::Result<()> {
        if let Some(clip) = self.clipboard.as_ref() {
            clip.set_contents(selection, text)?;
        }
        Ok(())
    }

    pub fn erase_scrollback_and_viewport(&mut self) {
        // Since we may be called outside of perform_actions,
        // we need to ensure that we increment the seqno in
        // order to correctly invalidate the display
        self.increment_seqno();
        if let Err(error) = self.screen_mut().erase_scrollback() {
            log::error!("refused scrollback-and-viewport erase: {error}");
            return;
        }

        let row_index = self.screen.phys_row(self.cursor.y);
        let rows = self
            .screen
            .lines_in_phys_range(row_index..row_index.saturating_add(1));

        self.erase_in_display(EraseInDisplay::EraseDisplay);

        for (idx, row) in rows.into_iter().enumerate() {
            *self.screen.line_mut(idx) = row;
        }

        self.cursor.y = 0;
    }

    /// Discards the scrollback, leaving only the data that is present
    /// in the viewport.
    pub fn erase_scrollback(&mut self) {
        // Since we may be called outside of perform_actions,
        // we need to ensure that we increment the seqno in
        // order to correctly invalidate the display
        self.increment_seqno();
        if let Err(error) = self.screen_mut().erase_scrollback() {
            log::error!("refused scrollback erase: {error}");
        }
    }

    /// Returns true if the associated application has enabled any of the
    /// supported mouse reporting modes.
    /// This is useful for the hosting GUI application to decide how best
    /// to dispatch mouse events to the terminal.
    pub fn is_mouse_grabbed(&self) -> bool {
        self.mouse_tracking || self.button_event_mouse || self.any_event_mouse
    }

    pub fn is_alt_screen_active(&self) -> bool {
        self.screen.is_alt_screen_active()
    }

    /// Returns true if the associated application has enabled
    /// bracketed paste mode, which can be helpful to the hosting
    /// GUI application to decide about fragmenting a large paste.
    pub fn bracketed_paste_enabled(&self) -> bool {
        self.bracketed_paste
    }

    /// Returns true if the application asked for focus in/out reports
    /// (DECSET 1004).
    pub fn focus_tracking_enabled(&self) -> bool {
        self.focus_tracking
    }

    /// Advise the terminal about a change in its focus state
    pub fn focus_changed(&mut self, focused: bool) {
        if focused == self.focused {
            return;
        }
        self.increment_seqno();
        if !focused {
            // notify app of release of buttons
            let buttons = self.current_mouse_buttons.clone();
            for b in buttons {
                self.mouse_event(MouseEvent {
                    kind: MouseEventKind::Release,
                    button: b,
                    modifiers: KeyModifiers::NONE,
                    x: 0,
                    y: 0,
                    x_pixel_offset: 0,
                    y_pixel_offset: 0,
                })
                .ok();
            }
        }
        if self.focus_tracking {
            let report = if focused { "\x1b[I" } else { "\x1b[O" };
            self.write_user_input(report.as_bytes()).ok();
        }
        self.focused = focused;
        if !focused {
            self.lost_focus_seqno = self.seqno;
        }
    }

    /// Returns true if there is new output since the terminal
    /// lost focus
    pub fn has_unseen_output(&self) -> bool {
        !self.focused && self.seqno > self.lost_focus_seqno
    }

    pub(crate) fn trigger_unseen_output_notif(&mut self) {
        if self.has_unseen_output() {
            // We want to avoid over-notifying about output events,
            // so here we gate the notification to the case where
            // we have lost the focus more recently than the last
            // time we notified about it
            if self.lost_focus_seqno > self.lost_focus_alerted_seqno {
                self.increment_seqno();
                self.lost_focus_alerted_seqno = self.seqno;
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::OutputSinceFocusLost);
                }
            }
        }
    }

    /// Send text to the terminal that is the result of pasting.
    /// If bracketed paste mode is enabled, the paste is enclosed
    /// in the bracketing, otherwise it is fed to the writer as-is.
    /// De-fang the text by removing any embedded bracketed paste
    /// sequence that may be present.
    ///
    /// A paste larger than [`MAX_PASTE_BYTES`] is refused whole with
    /// [`PasteTooLarge`]; nothing is sent. Otherwise the paste is queued in
    /// full behind earlier input and never dropped, however slowly the child
    /// reads it.
    pub fn send_paste(&mut self, text: &str) -> Result<(), Error> {
        if text.len() > MAX_PASTE_BYTES {
            return Err(PasteTooLarge {
                len: text.len(),
                limit: MAX_PASTE_BYTES,
            }
            .into());
        }
        let mut buf = String::new();
        if self.bracketed_paste {
            buf.push_str("\x1b[200~");
        }

        let canon = if self.bracketed_paste {
            NewlineCanon::None
        } else {
            self.config.canonicalize_pasted_newlines()
        };

        let canon = canon.canonicalize(text);
        let de_fanged = canon.replace("\x1b[200~", "").replace("\x1b[201~", "");
        buf.push_str(&de_fanged);

        if self.bracketed_paste {
            buf.push_str("\x1b[201~");
        }

        self.write_user_input(buf.as_bytes())?;
        Ok(())
    }

    /// Runs `f` with every write classed as user input (keys, pastes, mouse
    /// and focus reports), which backpressure never drops. Bytes buffered
    /// before the call go out first under their own class; nesting is safe.
    pub(crate) fn with_user_input<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let previous = self.writer.get_ref().class();
        if previous != WriteClass::UserInput {
            // Hand buffered replies over as replies before the class changes.
            // A failure here means the writer is gone, which `f` reports.
            self.writer.flush().ok();
            self.writer.get_mut().set_class(WriteClass::UserInput);
        }
        let result = f(self);
        if previous != WriteClass::UserInput {
            self.writer.flush().ok();
            self.writer.get_mut().set_class(previous);
        }
        result
    }

    /// Queues user input (keys, pastes, composed text) behind everything
    /// already written. Never blocks and is never dropped under backpressure.
    pub fn write_user_input(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.with_user_input(|term| {
            term.writer.write_all(bytes)?;
            term.writer.flush()
        })
    }

    /// Queues a reply to one of the child's queries behind everything already
    /// written, without waiting. Replies are dropped whole once
    /// [`REPLY_BACKLOG_LIMIT`] unwritten reply bytes accumulate.
    pub fn enqueue_reply(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    /// Hands every buffered byte to the writer thread and returns a barrier
    /// that completes once the writer has written and flushed all of them.
    ///
    /// This is the only way to observe write completion. Never wait on the
    /// barrier on the GUI main thread or while holding the terminal lock:
    /// take it under the lock, release the lock, then wait.
    pub fn writer_barrier(&mut self) -> WriterBarrier {
        if let Err(err) = self.writer.flush() {
            return WriterBarrier {
                state: WriterBarrierState::Unavailable(err.kind()),
            };
        }
        self.writer.get_ref().barrier()
    }

    /// The writer queue's current backlog and reply-drop counters.
    pub fn writer_backlog(&self) -> WriterBacklog {
        self.writer.get_ref().backlog()
    }

    /// Informs the terminal that the viewport of the window has resized to the
    /// specified dimensions.
    /// We need to resize both the primary and alt screens, adjusting
    /// the cursor positions of both accordingly.
    pub fn resize(&mut self, size: TerminalSize) {
        self.resize_with_prepared_reflow(size, None);
    }

    /// Capture immutable wrapping inputs; call `prepare` after releasing the
    /// terminal lock, then pass the result to `resize_with_prepared_reflow`.
    pub fn capture_reflow_preparation(
        &self,
        size: TerminalSize,
    ) -> Option<ScreenReflowPreparation> {
        let (cursor_main, _) = self.resize_cursors();
        self.screen
            .screen
            .capture_reflow_preparation(size, cursor_main)
    }

    fn resize_cursors(&self) -> (CursorPosition, CursorPosition) {
        // A pending autowrap denotes the insertion point after the rightmost
        // cell. Reflow must map that offset, not the displayed cursor cell,
        // otherwise the next character overwrites the last printed glyph.
        let mut active = self.cursor;
        if self.wrap_next && !self.screen.alt_screen_is_active {
            active.x = self.left_and_right_margins.end;
        }
        if self.screen.alt_screen_is_active {
            (
                self.screen
                    .screen
                    .saved_cursor
                    .as_ref()
                    .map(|s| CursorPosition {
                        x: s.wrap_next_column.unwrap_or(s.position.x),
                        ..s.position
                    })
                    .unwrap_or_else(CursorPosition::default),
                active,
            )
        } else {
            (
                active,
                self.screen
                    .alt_screen
                    .saved_cursor
                    .as_ref()
                    .map(|s| CursorPosition {
                        x: s.wrap_next_column.unwrap_or(s.position.x),
                        ..s.position
                    })
                    .unwrap_or_else(CursorPosition::default),
            )
        }
    }

    /// Apply a resize using prepared wraps only when their exact source still
    /// matches. The caller retains the preparation until after releasing its
    /// locks, including any displaced cache held by the preparation.
    pub fn resize_with_prepared_reflow(
        &mut self,
        size: TerminalSize,
        prepared: Option<&mut ScreenReflowPreparation>,
    ) {
        self.increment_seqno();
        self.refresh_batch_config();
        let (cursor_main, cursor_alt) = self.resize_cursors();

        let (adjusted_cursor_main, adjusted_cursor_alt) = self.screen.resize(
            size,
            cursor_main,
            cursor_alt,
            self.seqno,
            self.enable_conpty_quirks,
            prepared,
        );
        self.top_and_bottom_margins = 0..size.rows as i64;
        self.left_and_right_margins = 0..size.cols;
        self.pixel_height = size.pixel_height;
        self.pixel_width = size.pixel_width;
        self.dpi = size.dpi;
        self.tabs.resize(size.cols);

        if self.screen.alt_screen_is_active {
            self.set_cursor_pos(
                &Position::Absolute(adjusted_cursor_alt.x as i64),
                &Position::Absolute(adjusted_cursor_alt.y),
            );
        } else {
            self.set_cursor_pos(
                &Position::Absolute(adjusted_cursor_main.x as i64),
                &Position::Absolute(adjusted_cursor_main.y),
            );
        }
    }

    pub fn get_size(&self) -> TerminalSize {
        let screen = self.screen();
        TerminalSize {
            dpi: self.dpi,
            pixel_width: self.pixel_width,
            pixel_height: self.pixel_height,
            rows: screen.physical_rows,
            cols: screen.physical_cols,
        }
    }

    fn palette_did_change(&mut self) {
        self.make_all_lines_dirty();
        if let Some(handler) = self.alert_handler.as_mut() {
            handler.alert(Alert::PaletteChanged);
        }
    }

    /// When dealing with selection, mark a range of lines as dirty
    pub fn make_all_lines_dirty(&mut self) {
        self.increment_seqno();
        let seqno = self.seqno;
        // One floor for page-engine rows (ft-yccm0.3.3.4).
        self.screen.touch_all_rows(seqno);
    }

    /// Returns the 0-based cursor position relative to the top left of
    /// the visible screen
    pub fn cursor_pos(&self) -> CursorPosition {
        CursorPosition {
            x: self.cursor.x,
            y: self.cursor.y,
            shape: self.cursor.shape,
            visibility: if self.cursor_visible {
                CursorVisibility::Visible
            } else {
                CursorVisibility::Hidden
            },
            seqno: self.cursor.seqno,
        }
    }

    /// Returns the current cell attributes of the screen
    pub fn pen(&self) -> CellAttributes {
        self.pen.clone()
    }

    pub fn user_vars(&self) -> &HashMap<String, String> {
        &self.user_vars
    }

    fn clear_semantic_attribute_due_to_movement(&mut self) {
        if self.clear_semantic_attribute_on_newline {
            self.clear_semantic_attribute_on_newline = false;
            self.pen.set_semantic_type(SemanticType::default());
        }
    }

    /// Sets the cursor position to precisely the x and values provided
    fn set_cursor_position_absolute(&mut self, x: usize, y: VisibleRowIndex) {
        if self.cursor.y != y {
            self.clear_semantic_attribute_due_to_movement();
        }
        self.cursor.y = y;
        self.cursor.x = x;
        self.cursor.seqno = self.seqno;
        self.wrap_next = false;
    }

    /// Sets the cursor position. x and y are 0-based and relative to the
    /// top left of the visible screen.
    fn set_cursor_pos(&mut self, x: &Position, y: &Position) {
        let x = match *x {
            Position::Relative(x) => usize_to_i64_saturating(self.cursor.x)
                .saturating_add(x)
                .min(
                    if self.dec_origin_mode {
                        usize_to_i64_saturating(self.left_and_right_margins.end)
                    } else {
                        usize_to_i64_saturating(self.screen().physical_cols)
                    }
                    .saturating_sub(1),
                )
                .max(0),
            Position::Absolute(x) => x
                .saturating_add(if self.dec_origin_mode {
                    usize_to_i64_saturating(self.left_and_right_margins.start)
                } else {
                    0
                })
                .min(if self.dec_origin_mode {
                    usize_to_i64_saturating(self.left_and_right_margins.end).saturating_sub(1)
                } else {
                    // We allow 1 extra for the cursor x position
                    // to account for some resize/rewrap scenarios
                    // where we don't want to forget that the
                    // cursor belongs to a wrapped line.
                    usize_to_i64_saturating(self.screen().physical_cols)
                })
                .max(0),
        };

        let y = match *y {
            Position::Relative(y) => self
                .cursor
                .y
                .saturating_add(y)
                .min(
                    if self.dec_origin_mode {
                        self.top_and_bottom_margins.end
                    } else {
                        usize_to_i64_saturating(self.screen().physical_rows)
                    }
                    .saturating_sub(1),
                )
                .max(0),
            Position::Absolute(y) => y
                .saturating_add(if self.dec_origin_mode {
                    self.top_and_bottom_margins.start
                } else {
                    0
                })
                .min(
                    if self.dec_origin_mode {
                        self.top_and_bottom_margins.end
                    } else {
                        usize_to_i64_saturating(self.screen().physical_rows)
                    }
                    .saturating_sub(1),
                )
                .max(0),
        };

        self.set_cursor_position_absolute(x as usize, y);
    }

    fn scroll_up(&mut self, num_rows: usize) {
        let seqno = self.seqno;
        let blank_attr = self.pen.clone_sgr_only();
        let top_and_bottom_margins = self.top_and_bottom_margins.clone();
        let left_and_right_margins = self.left_and_right_margins.clone();
        let bidi_mode = self.get_bidi_mode();
        self.screen_mut().scroll_up_within_margins(
            &top_and_bottom_margins,
            &left_and_right_margins,
            num_rows,
            seqno,
            blank_attr,
            bidi_mode,
        )
    }

    fn scroll_down(&mut self, num_rows: usize) {
        let seqno = self.seqno;
        let blank_attr = self.pen.clone_sgr_only();
        let top_and_bottom_margins = self.top_and_bottom_margins.clone();
        let left_and_right_margins = self.left_and_right_margins.clone();
        let bidi_mode = self.get_bidi_mode();
        self.screen_mut().scroll_down_within_margins(
            &top_and_bottom_margins,
            &left_and_right_margins,
            num_rows,
            seqno,
            blank_attr,
            bidi_mode,
        )
    }

    /// Defined by FinalTermSemanticPrompt; a fresh-line is a NOP if the
    /// cursor is already at the left margin, otherwise it is the same as
    /// a new line.
    fn fresh_line(&mut self) {
        if self.cursor.x == self.left_and_right_margins.start {
            return;
        }
        self.new_line(true);
    }

    fn new_line(&mut self, move_to_first_column: bool) {
        let x = if move_to_first_column {
            self.left_and_right_margins.start
        } else {
            self.cursor.x
        };
        let y = self.cursor.y;
        let y = if y == last_row_in(&self.top_and_bottom_margins) {
            self.scroll_up(1);
            y
        } else {
            next_row_saturating(y)
        };
        self.set_cursor_pos(&Position::Absolute(x as i64), &Position::Absolute(y as i64));
    }

    /// Moves the cursor down one line in the same column.
    /// If the cursor is at the bottom margin, the page scrolls up.
    fn c1_index(&mut self) {
        if self.left_and_right_margins.contains(&self.cursor.x) {
            if self.cursor.y == last_row_in(&self.top_and_bottom_margins) {
                self.scroll_up(1);
            } else {
                self.set_cursor_pos(&Position::Relative(0), &Position::Relative(1));
            }
        }
    }

    /// Sets a horizontal tab stop at the current cursor position.
    fn c1_hts(&mut self) {
        self.tabs.set_tab_stop(self.cursor.x);
    }

    /// Moves the cursor to the next tab stop. If there are no more tab stops,
    /// the cursor moves to the right margin. HT does not cause text to auto
    /// wrap.
    fn c0_horizontal_tab(&mut self) {
        let seqno = self.seqno;
        let x = match self.tabs.find_next_tab_stop(self.cursor.x) {
            Some(x) => x,
            None => last_col_in(&self.left_and_right_margins),
        };
        self.cursor.x = x.min(last_col_in(&self.left_and_right_margins));
        self.cursor.seqno = seqno;
    }

    /// Move the cursor up 1 line.  If the position is at the top scroll margin,
    /// scroll the region down.
    fn c1_reverse_index(&mut self) {
        if self.left_and_right_margins.contains(&self.cursor.x) {
            if self.cursor.y == self.top_and_bottom_margins.start {
                self.scroll_down(1);
            } else {
                self.set_cursor_pos(&Position::Relative(0), &Position::Relative(-1));
            }
        }
    }

    fn c1_nel(&mut self) {
        self.new_line(true);
    }

    fn set_hyperlink(&mut self, link: Option<Hyperlink>) {
        self.pen.set_hyperlink(match link {
            Some(hyperlink) => Some(Arc::new(hyperlink)),
            None => None,
        });
    }

    /// <https://invisible-island.net/xterm/ctlseqs/ctlseqs.html#h4-Device-Control-functions:DCS-plus-q-Pt-ST.F95>
    /// XTGETTCAP
    fn xt_get_tcap(&mut self, names: Vec<String>) {
        let mut res = String::new();

        for name in &names {
            res.push_str("\x1bP");

            let encoded_name = hex::encode_upper(&name);
            match name.as_str() {
                "TN" | "name" => {
                    res.push_str("1+r");
                    res.push_str(&encoded_name);
                    res.push('=');

                    let encoded_val = hex::encode_upper(&self.term_program);
                    res.push_str(&encoded_val);
                }

                "Co" | "colors" => {
                    res.push_str("1+r");
                    res.push_str(&encoded_name);
                    res.push('=');
                    let encoded_val = hex::encode_upper("256");
                    res.push_str(&encoded_val);
                }

                "RGB" => {
                    res.push_str("1+r");
                    res.push_str(&encoded_name);
                    res.push('=');
                    let encoded_val = hex::encode_upper("8/8/8");
                    res.push_str(&encoded_val);
                }

                _ => {
                    if let Some(value) = DB.raw(name) {
                        res.push_str("1+r");
                        res.push_str(&encoded_name);
                        res.push('=');
                        let value = match value {
                            Value::True => hex::encode_upper("1"),
                            Value::Number(n) => hex::encode_upper(&n.to_string()),
                            Value::String(s) => hex::encode_upper(s),
                        };
                        res.push_str(&value);
                    } else {
                        log::trace!("xt_get_tcap: unknown name {}", name);
                        res.push_str("0+r");
                        res.push_str(&encoded_name);
                    }
                }
            }
            res.push_str("\x1b\\");
        }

        log::trace!(
            "XTGETTCAP {:?} responding with {}",
            names,
            res.escape_debug()
        );
        self.writer.write_all(res.as_bytes()).ok();
        self.writer.flush().ok();
    }

    fn perform_device(&mut self, dev: Device) {
        match dev {
            Device::DeviceAttributes(a) => {
                if self.config.log_unknown_escape_sequences() {
                    log::warn!("unhandled: {:?}", a);
                }
            }
            Device::SoftReset => {
                // TODO: see https://vt100.net/docs/vt510-rm/DECSTR.html
                self.pen = CellAttributes::default();
                self.insert = false;
                self.dec_origin_mode = false;
                // Note that xterm deviates from the documented DECSTR
                // setting for dec_auto_wrap, so we do too
                self.dec_auto_wrap = true;
                self.application_cursor_keys = false;
                self.modify_other_keys = None;
                self.application_keypad = false;
                self.top_and_bottom_margins = 0..self.screen().physical_rows as i64;
                self.left_and_right_margins = 0..self.screen().physical_cols;
                self.left_and_right_margin_mode = false;
                self.screen.activate_alt_screen(self.seqno);
                self.screen.saved_cursor().take();
                self.screen.activate_primary_screen(self.seqno);
                self.screen.saved_cursor().take();
                self.kitty_remove_all_placements(true);

                self.reverse_wraparound_mode = false;
                self.reverse_video_mode = false;
                self.bidi_enabled.take();
                self.bidi_hint.take();

                self.g0_charset = CharSet::Ascii;
                self.g1_charset = CharSet::Ascii;
            }
            Device::RequestPrimaryDeviceAttributes => {
                let mut ident = "\x1b[?65".to_string(); // Vt500
                ident.push_str(";4"); // Sixel graphics
                ident.push_str(";18"); // windowing extensions
                ident.push_str(";22"); // ANSI color, vt525
                ident.push_str(";52"); // Clipboard access
                ident.push('c');

                self.writer.write(ident.as_bytes()).ok();
                self.writer.flush().ok();
            }
            Device::RequestSecondaryDeviceAttributes => {
                // Response is: Pp ; Pv ; Pc
                // Where Pp=1 means vt220
                // and Pv is the firmware version.
                // Pc is always 0.
                // Because our default TERM is xterm, the firmware
                // version will be considered to be equialent to xterm's
                // patch levels, with the following effects:
                // pv < 95 -> ttymouse=xterm
                // pv >= 95 < 277 -> ttymouse=xterm2
                // pv >= 277 -> ttymouse=sgr
                // pv >= 279 - xterm will probe for additional device settings.
                self.writer.write(b"\x1b[>1;277;0c").ok();
                self.writer.flush().ok();
            }
            Device::RequestTertiaryDeviceAttributes => {
                self.writer
                    .write(format!("\x1bP!|00000000{}", ST).as_bytes())
                    .ok();
                self.writer.flush().ok();
            }
            Device::RequestTerminalNameAndVersion => {
                self.writer.write(DCS.as_bytes()).ok();
                self.writer
                    .write(
                        format!(">|{} {}{}", self.term_program, self.term_version, ST).as_bytes(),
                    )
                    .ok();
                self.writer.flush().ok();
            }
            Device::RequestTerminalParameters(a) => {
                self.writer
                    .write(format!("\x1b[{};1;1;128;128;1;0x", a + 2).as_bytes())
                    .ok();
                self.writer.flush().ok();
            }
            Device::StatusReport => {
                self.writer.write(b"\x1b[0n").ok();
                self.writer.flush().ok();
            }
            Device::XtSmGraphics(g) => {
                let response = if matches!(g.item, XtSmGraphicsItem::Unspecified(_)) {
                    XtSmGraphics {
                        item: g.item,
                        action_or_status: XtSmGraphicsStatus::InvalidItem.to_i64(),
                        value: vec![],
                    }
                } else {
                    match g.action() {
                        None | Some(XtSmGraphicsAction::SetToValue) => XtSmGraphics {
                            item: g.item,
                            action_or_status: XtSmGraphicsStatus::InvalidAction.to_i64(),
                            value: vec![],
                        },
                        Some(XtSmGraphicsAction::ResetToDefault) => XtSmGraphics {
                            item: g.item,
                            action_or_status: XtSmGraphicsStatus::Success.to_i64(),
                            value: vec![],
                        },
                        Some(XtSmGraphicsAction::ReadMaximumAllowedValue)
                        | Some(XtSmGraphicsAction::ReadAttribute) => match g.item {
                            XtSmGraphicsItem::Unspecified(_) => unreachable!("checked above"),
                            XtSmGraphicsItem::NumberOfColorRegisters => XtSmGraphics {
                                item: g.item,
                                action_or_status: XtSmGraphicsStatus::Success.to_i64(),
                                value: vec![65536],
                            },
                            XtSmGraphicsItem::RegisGraphicsGeometry
                            | XtSmGraphicsItem::SixelGraphicsGeometry => XtSmGraphics {
                                item: g.item,
                                action_or_status: XtSmGraphicsStatus::Success.to_i64(),
                                value: vec![self.pixel_width as i64, self.pixel_height as i64],
                            },
                        },
                    }
                };

                let dev = Device::XtSmGraphics(response);

                write!(self.writer, "\x1b[{}", dev).ok();
                self.writer.flush().ok();
            }
        }
    }

    /// Indicates that mode is permanently enabled
    fn decqrm_response_permanent(&mut self, mode: Mode) {
        let (is_dec, number) = match &mode {
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(code)) => (true, code.to_u16().unwrap()),
            Mode::QueryDecPrivateMode(DecPrivateMode::Unspecified(code)) => (true, *code),
            Mode::QueryMode(TerminalMode::Code(code)) => (false, code.to_u16().unwrap()),
            Mode::QueryMode(TerminalMode::Unspecified(_code)) => {
                unreachable!("unhandled {:?}", mode);
            }
            _ => unreachable!(),
        };

        let prefix = if is_dec { "?" } else { "" };

        write!(self.writer, "\x1b[{prefix}{number};3$y").ok();
        self.writer.flush().ok();
    }

    fn decqrm_response(&mut self, mode: Mode, mut recognized: bool, enabled: bool) {
        let (is_dec, number) = match &mode {
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(code)) => (true, code.to_u16().unwrap()),
            Mode::QueryDecPrivateMode(DecPrivateMode::Unspecified(code)) => {
                recognized = false;
                (true, *code)
            }
            Mode::QueryMode(TerminalMode::Code(code)) => (false, code.to_u16().unwrap()),
            Mode::QueryMode(TerminalMode::Unspecified(code)) => {
                recognized = false;
                (false, *code)
            }
            _ => unreachable!(),
        };

        let prefix = if is_dec { "?" } else { "" };

        let status = if recognized {
            if enabled {
                1 // set
            } else {
                2 // reset
            }
        } else {
            0
        };

        log::trace!("{:?} -> recognized={} status={}", mode, recognized, status);
        write!(self.writer, "\x1b[{}{};{}$y", prefix, number, status).ok();
        self.writer.flush().ok();
    }

    fn current_dec_private_mode(&self, code: &DecPrivateModeCode) -> Option<bool> {
        match code {
            DecPrivateModeCode::Win32InputMode => {
                Some(self.keyboard_encoding == KeyboardEncoding::Win32)
            }
            DecPrivateModeCode::ReverseWraparound => Some(self.reverse_wraparound_mode),
            DecPrivateModeCode::LeftRightMarginMode => Some(self.left_and_right_margin_mode),
            DecPrivateModeCode::AutoWrap => Some(self.dec_auto_wrap),
            DecPrivateModeCode::OriginMode => Some(self.dec_origin_mode),
            DecPrivateModeCode::UsePrivateColorRegistersForEachGraphic => {
                Some(self.use_private_color_registers_for_each_graphic)
            }
            DecPrivateModeCode::SynchronizedOutput => Some(self.synchronized_output),
            DecPrivateModeCode::ReverseVideo => Some(self.reverse_video_mode),
            DecPrivateModeCode::BracketedPaste => Some(self.bracketed_paste),
            DecPrivateModeCode::OptEnableAlternateScreen
            | DecPrivateModeCode::EnableAlternateScreen
            | DecPrivateModeCode::ClearAndEnableAlternateScreen => {
                Some(self.screen.is_alt_screen_active())
            }
            DecPrivateModeCode::ApplicationCursorKeys => Some(self.application_cursor_keys),
            DecPrivateModeCode::SixelDisplayMode => Some(self.sixel_display_mode),
            DecPrivateModeCode::DecAnsiMode => Some(self.dec_ansi_mode),
            DecPrivateModeCode::ShowCursor => Some(self.cursor_visible),
            DecPrivateModeCode::MouseTracking => Some(self.mouse_tracking),
            DecPrivateModeCode::ButtonEventMouse => Some(self.button_event_mouse),
            DecPrivateModeCode::AnyEventMouse => Some(self.any_event_mouse),
            DecPrivateModeCode::FocusTracking => Some(self.focus_tracking),
            DecPrivateModeCode::SGRMouse => Some(matches!(self.mouse_encoding, MouseEncoding::SGR)),
            DecPrivateModeCode::SGRPixelsMouse => {
                Some(matches!(self.mouse_encoding, MouseEncoding::SgrPixels))
            }
            DecPrivateModeCode::Utf8Mouse => {
                Some(matches!(self.mouse_encoding, MouseEncoding::Utf8))
            }
            DecPrivateModeCode::SixelScrollsRight => Some(self.sixel_scrolls_right),
            _ => None,
        }
    }

    fn save_dec_private_mode(&mut self, code: DecPrivateModeCode) {
        let Some(number) = code.to_u16() else {
            log::warn!("save dec mode {:?} unimplemented", code);
            return;
        };

        if let Some(enabled) = self.current_dec_private_mode(&code) {
            self.saved_dec_private_modes.insert(number, enabled);
        } else {
            log::warn!("save dec mode {:?} unimplemented", code);
        }
    }

    fn restore_dec_private_mode(&mut self, code: DecPrivateModeCode) {
        let Some(number) = code.to_u16() else {
            log::warn!("restore dec mode {:?} unimplemented", code);
            return;
        };

        if self.current_dec_private_mode(&code).is_none() {
            log::warn!("restore dec mode {:?} unimplemented", code);
            return;
        }

        let Some(enabled) = self.saved_dec_private_modes.get(&number).copied() else {
            return;
        };

        let mode = DecPrivateMode::Code(code);
        let mode = if enabled {
            Mode::SetDecPrivateMode(mode)
        } else {
            Mode::ResetDecPrivateMode(mode)
        };
        self.perform_csi_mode(mode);
    }

    fn perform_csi_mode(&mut self, mode: Mode) {
        match mode {
            Mode::SetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::StartBlinkingCursor,
            ))
            | Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::StartBlinkingCursor,
            )) => {}
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::StartBlinkingCursor,
            )) => {
                self.decqrm_response(mode, true, false);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::AutoRepeat))
            | Mode::ResetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::AutoRepeat)) => {
                // We leave key repeat to the GUI layer prefs
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::Win32InputMode)) => {
                self.keyboard_encoding = KeyboardEncoding::Win32;
            }

            Mode::ResetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::Win32InputMode)) => {
                self.keyboard_encoding = KeyboardEncoding::Xterm;
            }

            Mode::QueryDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::Win32InputMode)) => {
                self.decqrm_response(
                    mode,
                    true,
                    self.keyboard_encoding == KeyboardEncoding::Win32,
                );
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::ReverseWraparound,
            )) => {
                self.reverse_wraparound_mode = true;
            }

            Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::ReverseWraparound,
            )) => {
                self.reverse_wraparound_mode = false;
            }

            Mode::QueryDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::ReverseWraparound,
            )) => {
                self.decqrm_response(mode, true, self.reverse_wraparound_mode);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::LeftRightMarginMode,
            )) => {
                self.left_and_right_margin_mode = true;
            }

            Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::LeftRightMarginMode,
            )) => {
                self.left_and_right_margin_mode = false;
                self.left_and_right_margins = 0..self.screen().physical_cols;
            }

            Mode::QueryDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::LeftRightMarginMode,
            )) => {
                self.decqrm_response(mode, true, self.left_and_right_margin_mode);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::GraphemeClustering,
            ))
            | Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::GraphemeClustering,
            )) => {
                // Permanently enabled
            }

            Mode::QueryDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::GraphemeClustering,
            )) => {
                self.decqrm_response_permanent(mode);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::SaveCursor)) => {
                self.dec_save_cursor();
            }

            Mode::ResetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::SaveCursor)) => {
                self.dec_restore_cursor();
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::AutoWrap)) => {
                self.dec_auto_wrap = true;
            }

            Mode::ResetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::AutoWrap)) => {
                self.dec_auto_wrap = false;
            }

            Mode::QueryDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::AutoWrap)) => {
                self.decqrm_response(mode, true, self.dec_auto_wrap);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::OriginMode)) => {
                self.dec_origin_mode = true;
                self.set_cursor_pos(&Position::Absolute(0), &Position::Absolute(0));
            }

            Mode::ResetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::OriginMode)) => {
                self.dec_origin_mode = false;
                self.set_cursor_pos(&Position::Absolute(0), &Position::Absolute(0));
            }

            Mode::QueryDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::OriginMode)) => {
                self.decqrm_response(mode, true, self.dec_origin_mode);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::UsePrivateColorRegistersForEachGraphic,
            )) => {
                self.use_private_color_registers_for_each_graphic = true;
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::UsePrivateColorRegistersForEachGraphic,
            )) => {
                self.use_private_color_registers_for_each_graphic = false;
            }
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::UsePrivateColorRegistersForEachGraphic,
            )) => {
                self.decqrm_response(
                    mode,
                    true,
                    self.use_private_color_registers_for_each_graphic,
                );
            }

            // DEC private mode 2026 — synchronized output. The flag
            // is tracked at this layer (ft-d7af6); the renderer
            // consumes it via `synchronized_output()` to hold
            // presentation while a multi-line redraw is in progress.
            // Replaces the prior "handled in wezterm's mux" stub
            // (the mux delegation no longer applies in the
            // frankenterm fork).
            Mode::SetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::SynchronizedOutput,
            )) => {
                self.synchronized_output = true;
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::SynchronizedOutput,
            )) => {
                self.synchronized_output = false;
            }
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::SynchronizedOutput,
            )) => {
                self.decqrm_response(mode, true, self.synchronized_output);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::SmoothScroll))
            | Mode::ResetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::SmoothScroll)) => {
                // We always output at our "best" rate
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::ReverseVideo)) => {
                // Turn on reverse video for all of the lines on the
                // display.
                self.reverse_video_mode = true;
            }

            Mode::ResetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::ReverseVideo)) => {
                // Turn off reverse video for all of the lines on the
                // display.
                self.reverse_video_mode = false;
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::Select132Columns))
            | Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::Select132Columns,
            )) => {
                // Note: we don't support 132 column mode so we treat
                // both set/reset as the same and we're really just here
                // for the other side effects of this sequence
                // https://vt100.net/docs/vt510-rm/DECCOLM.html

                self.top_and_bottom_margins = 0..self.screen().physical_rows as i64;
                self.left_and_right_margins = 0..self.screen().physical_cols;
                self.set_cursor_pos(&Position::Absolute(0), &Position::Absolute(0));
                self.erase_in_display(EraseInDisplay::EraseDisplay);
            }
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::Select132Columns,
            )) => {
                self.decqrm_response(mode, true, false);
            }

            Mode::SetMode(TerminalMode::Code(TerminalModeCode::BiDirectionalSupportMode)) => {
                self.bidi_enabled.replace(true);
            }
            Mode::ResetMode(TerminalMode::Code(TerminalModeCode::BiDirectionalSupportMode)) => {
                self.bidi_enabled.replace(false);
            }
            Mode::QueryMode(TerminalMode::Code(TerminalModeCode::BiDirectionalSupportMode)) => {
                self.decqrm_response(
                    mode,
                    true,
                    self.bidi_enabled
                        .unwrap_or(self.batch_config.bidi_mode.enabled),
                );
            }

            Mode::SetMode(TerminalMode::Code(TerminalModeCode::Insert)) => {
                self.insert = true;
            }
            Mode::ResetMode(TerminalMode::Code(TerminalModeCode::Insert)) => {
                self.insert = false;
            }
            Mode::QueryMode(TerminalMode::Code(TerminalModeCode::Insert)) => {
                self.decqrm_response(mode, true, self.insert);
            }

            Mode::SetMode(TerminalMode::Code(TerminalModeCode::AutomaticNewline)) => {
                self.newline_mode = true;
            }
            Mode::ResetMode(TerminalMode::Code(TerminalModeCode::AutomaticNewline)) => {
                self.newline_mode = false;
            }
            Mode::QueryMode(TerminalMode::Code(TerminalModeCode::AutomaticNewline)) => {
                self.decqrm_response(mode, true, self.newline_mode);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::BracketedPaste)) => {
                self.bracketed_paste = true;
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::BracketedPaste)) => {
                self.bracketed_paste = false;
            }
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::BracketedPaste)) => {
                self.decqrm_response(mode, true, self.bracketed_paste);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::OptEnableAlternateScreen,
            ))
            | Mode::SetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::EnableAlternateScreen,
            )) => {
                if !self.screen.is_alt_screen_active() {
                    self.screen.activate_alt_screen(self.seqno);
                    self.pen = CellAttributes::default();
                }
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::OptEnableAlternateScreen,
            )) => {
                if self.screen.is_alt_screen_active() {
                    self.pen = CellAttributes::default();
                    self.erase_in_display(EraseInDisplay::EraseDisplay);
                    self.screen.activate_primary_screen(self.seqno);
                }
            }

            Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::EnableAlternateScreen,
            )) => {
                if self.screen.is_alt_screen_active() {
                    self.screen.activate_primary_screen(self.seqno);
                    self.pen = CellAttributes::default();
                }
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::ApplicationCursorKeys,
            )) => {
                self.application_cursor_keys = true;
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::ApplicationCursorKeys,
            )) => {
                self.application_cursor_keys = false;
            }
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::ApplicationCursorKeys,
            )) => {
                self.decqrm_response(mode, true, self.application_cursor_keys);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::SixelDisplayMode)) => {
                self.sixel_display_mode = true;
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::SixelDisplayMode,
            )) => {
                self.sixel_display_mode = false;
            }
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::SixelDisplayMode,
            )) => {
                self.decqrm_response(mode, true, self.sixel_display_mode);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::DecAnsiMode)) => {
                self.dec_ansi_mode = true;
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::DecAnsiMode)) => {
                self.dec_ansi_mode = false;
            }
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::DecAnsiMode)) => {
                self.decqrm_response(mode, true, self.dec_ansi_mode);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::ShowCursor)) => {
                self.cursor_visible = true;
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::ShowCursor)) => {
                self.cursor_visible = false;
            }
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::ShowCursor)) => {
                self.decqrm_response(mode, true, self.cursor_visible);
            }
            Mode::SetMode(TerminalMode::Code(TerminalModeCode::ShowCursor)) => {
                self.cursor_visible = true;
            }
            Mode::ResetMode(TerminalMode::Code(TerminalModeCode::ShowCursor)) => {
                self.cursor_visible = false;
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::MouseTracking)) => {
                self.mouse_tracking = true;
                self.last_mouse_move.take();
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::MouseTracking)) => {
                self.mouse_tracking = false;
                self.last_mouse_move.take();
            }
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::MouseTracking)) => {
                self.decqrm_response(mode, true, self.mouse_tracking);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::HighlightMouseTracking,
            ))
            | Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::HighlightMouseTracking,
            )) => {}

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::ButtonEventMouse)) => {
                self.button_event_mouse = true;
                self.last_mouse_move.take();
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::ButtonEventMouse,
            )) => {
                self.button_event_mouse = false;
                self.last_mouse_move.take();
            }
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::ButtonEventMouse,
            )) => {
                self.decqrm_response(mode, true, self.button_event_mouse);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::AnyEventMouse)) => {
                self.any_event_mouse = true;
                self.last_mouse_move.take();
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::AnyEventMouse)) => {
                self.any_event_mouse = false;
                self.last_mouse_move.take();
            }
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::AnyEventMouse)) => {
                self.decqrm_response(mode, true, self.any_event_mouse);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::FocusTracking)) => {
                self.focus_tracking = true;
                self.last_mouse_move.take();
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::FocusTracking)) => {
                self.focus_tracking = false;
                self.last_mouse_move.take();
            }
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::FocusTracking)) => {
                self.decqrm_response(mode, true, self.focus_tracking);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::SGRMouse)) => {
                self.mouse_encoding = MouseEncoding::SGR;
                self.last_mouse_move.take();
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::SGRMouse)) => {
                self.mouse_encoding = MouseEncoding::X10;
                self.last_mouse_move.take();
            }
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::SGRMouse)) => {
                self.decqrm_response(
                    mode,
                    true,
                    match self.mouse_encoding {
                        MouseEncoding::SGR => true,
                        _ => false,
                    },
                );
            }
            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::SGRPixelsMouse)) => {
                self.mouse_encoding = MouseEncoding::SgrPixels;
                self.last_mouse_move.take();
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::SGRPixelsMouse)) => {
                self.mouse_encoding = MouseEncoding::X10;
                self.last_mouse_move.take();
            }
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::SGRPixelsMouse)) => {
                self.decqrm_response(
                    mode,
                    true,
                    match self.mouse_encoding {
                        MouseEncoding::SgrPixels => true,
                        _ => false,
                    },
                );
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::Utf8Mouse)) => {
                self.mouse_encoding = MouseEncoding::Utf8;
                self.last_mouse_move.take();
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::Utf8Mouse)) => {
                self.mouse_encoding = MouseEncoding::X10;
                self.last_mouse_move.take();
            }
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(DecPrivateModeCode::Utf8Mouse)) => {
                self.decqrm_response(
                    mode,
                    true,
                    match self.mouse_encoding {
                        MouseEncoding::Utf8 => true,
                        _ => false,
                    },
                );
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::SixelScrollsRight,
            )) => {
                self.sixel_scrolls_right = true;
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::SixelScrollsRight,
            )) => {
                self.sixel_scrolls_right = false;
            }
            Mode::QueryDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::SixelScrollsRight,
            )) => {
                self.decqrm_response(mode, true, self.sixel_scrolls_right);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::ClearAndEnableAlternateScreen,
            )) => {
                if !self.screen.is_alt_screen_active() {
                    self.dec_save_cursor();
                    self.screen.activate_alt_screen(self.seqno);
                    self.set_cursor_pos(&Position::Absolute(0), &Position::Absolute(0));
                    self.pen = CellAttributes::default();
                    self.erase_in_display(EraseInDisplay::EraseDisplay);
                }
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::ClearAndEnableAlternateScreen,
            )) => {
                if self.screen.is_alt_screen_active() {
                    self.screen.activate_primary_screen(self.seqno);
                    self.dec_restore_cursor();
                }
            }
            Mode::SaveDecPrivateMode(DecPrivateMode::Code(n)) => {
                self.save_dec_private_mode(n);
            }
            Mode::RestoreDecPrivateMode(DecPrivateMode::Code(n)) => {
                self.restore_dec_private_mode(n);
            }

            Mode::SetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::MinTTYApplicationEscapeKeyMode,
            ))
            | Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::MinTTYApplicationEscapeKeyMode,
            )) => {}

            Mode::SetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::XTermMetaSendsEscape,
            ))
            | Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::XTermMetaSendsEscape,
            )) => {}

            Mode::SetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::XTermAltSendsEscape,
            ))
            | Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::XTermAltSendsEscape,
            )) => {}

            Mode::SetDecPrivateMode(DecPrivateMode::Unspecified(_))
            | Mode::ResetDecPrivateMode(DecPrivateMode::Unspecified(_))
            | Mode::SaveDecPrivateMode(DecPrivateMode::Unspecified(_))
            | Mode::RestoreDecPrivateMode(DecPrivateMode::Unspecified(_)) => {
                if self.config.log_unknown_escape_sequences() {
                    log::warn!("unhandled DecPrivateMode {:?}", mode);
                }
            }

            mode @ Mode::SetMode(_) | mode @ Mode::ResetMode(_) => {
                if self.config.log_unknown_escape_sequences() {
                    log::warn!("unhandled {:?}", mode);
                }
            }

            Mode::XtermKeyMode {
                resource: XtermKeyModifierResource::OtherKeys,
                value,
            } => {
                self.modify_other_keys = match value {
                    Some(0) => None,
                    _ => value,
                };
                log::debug!("XtermKeyMode OtherKeys -> {:?}", self.modify_other_keys);
            }

            Mode::XtermKeyMode { resource, value } => {
                if self.config.log_unknown_escape_sequences() {
                    log::warn!("unhandled XtermKeyMode {:?} {:?}", resource, value);
                }
            }

            Mode::QueryDecPrivateMode(_) | Mode::QueryMode(_) => {
                self.decqrm_response(mode, false, false);
            }
        }
    }

    fn checksum_rectangle(&mut self, left: u32, top: u32, right: u32, bottom: u32) -> u16 {
        let y_origin = if self.dec_origin_mode {
            i64_to_u32_saturating(self.top_and_bottom_margins.start)
        } else {
            0
        };
        let x_origin = if self.dec_origin_mode {
            self.left_and_right_margins.start
        } else {
            0
        };
        let screen = self.screen_mut();
        // checksum is u16 — the function returns u16 below (32u16 fallback).
        // Without an explicit type the integer literal is `{integer}`,
        // which makes `.wrapping_add(u16::from(...))` ambiguous.
        let mut checksum: u16 = 0;
        /*
        debug!(
            "checksum left={} top={} right={} bottom={}",
            left as usize + x_origin,
            top + y_origin,
            right as usize + x_origin,
            bottom + y_origin
        );
        */

        for y in top..=bottom {
            let line_idx = screen.phys_row(VisibleRowIndex::from(y_origin.saturating_add(y)));
            let line = screen.phys_line(line_idx);
            let left = x_origin.saturating_add(left as usize);
            let right = x_origin.saturating_add(right as usize);
            for cell in line.visible_cells().skip(left) {
                if cell.cell_index() > right {
                    break;
                }

                let ch = cell.str().chars().next().unwrap_or(' ') as u32;
                // debug!("y={} col={} ch={:x} cell={:?}", y + y_origin, col, ch, cell);

                checksum = checksum.wrapping_add(u16::from(ch as u8));
            }
        }

        // Treat uninitialized cells as spaces.
        // The concept of uninitialized cells in wezterm is not the same as that on VT520 or that
        // on xterm, so, to prevent a lot of noise in esctest, treat them as spaces, at least when
        // asking for the checksum of a single cell (which is what esctest does).
        // See: https://github.com/wezterm/wezterm/pull/4565
        if checksum == 0 {
            32u16
        } else {
            checksum
        }
    }

    fn perform_csi_window(&mut self, window: Window) {
        match window {
            Window::ReportTextAreaSizeCells => {
                let screen = self.screen();
                let height = Some(screen.physical_rows as i64);
                let width = Some(screen.physical_cols as i64);

                let response = Box::new(Window::ResizeWindowCells { width, height });
                write!(self.writer, "{}", CSI::Window(response)).ok();
                self.writer.flush().ok();
            }

            Window::ReportCellSizePixels => {
                let screen = self.screen();
                let height = screen.physical_rows;
                let width = screen.physical_cols;
                let response = Box::new(Window::ReportCellSizePixelsResponse {
                    width: Some((self.pixel_width / width) as i64),
                    height: Some((self.pixel_height / height) as i64),
                });
                write!(self.writer, "{}", CSI::Window(response)).ok();
                self.writer.flush().ok();
            }

            Window::ReportTextAreaSizePixels => {
                let response = Box::new(Window::ResizeWindowPixels {
                    width: Some(self.pixel_width as i64),
                    height: Some(self.pixel_height as i64),
                });
                write!(self.writer, "{}", CSI::Window(response)).ok();
                self.writer.flush().ok();
            }

            Window::ReportWindowTitle => {
                if self.config.enable_title_reporting() {
                    write!(
                        self.writer,
                        "{}",
                        OperatingSystemCommand::SetWindowTitleSun(self.title.clone())
                    )
                    .ok();
                    self.writer.flush().ok();
                }
            }

            Window::ChecksumRectangularArea {
                request_id,
                top,
                left,
                bottom,
                right,
                ..
            } => {
                if self.config.enable_checksum_rectangular_area() {
                    let checksum = self.checksum_rectangle(
                        left.as_zero_based(),
                        top.as_zero_based(),
                        right.as_zero_based(),
                        bottom.as_zero_based(),
                    );
                    write!(self.writer, "\x1bP{}!~{:04x}\x1b\\", request_id, checksum).ok();
                    self.writer.flush().ok();
                }
            }
            Window::ResizeWindowCells { .. } => {
                // We don't allow the application to change the window size; that's
                // up to the user!
            }
            Window::Iconify | Window::DeIconify => {}
            Window::PopIconAndWindowTitle
            | Window::PopWindowTitle
            | Window::PopIconTitle
            | Window::PushIconAndWindowTitle
            | Window::PushIconTitle
            | Window::PushWindowTitle => {}

            _ => {
                if self.config.log_unknown_escape_sequences() {
                    log::warn!("unhandled Window CSI {:?}", window);
                }
            }
        }
    }

    fn erase_in_display(&mut self, erase: EraseInDisplay) {
        let seqno = self.seqno;
        let cy = self.cursor.y;
        let pen = self.pen.clone_sgr_only();
        let rows = self.screen().physical_rows as VisibleRowIndex;
        let col_range = 0..self.screen().physical_cols;
        let row_range = match erase {
            EraseInDisplay::EraseToEndOfDisplay => {
                self.perform_csi_edit(Edit::EraseInLine(EraseInLine::EraseToEndOfLine));
                cy.saturating_add(1)..rows
            }
            EraseInDisplay::EraseToStartOfDisplay => {
                self.perform_csi_edit(Edit::EraseInLine(EraseInLine::EraseToStartOfLine));
                0..cy
            }
            EraseInDisplay::EraseDisplay => 0..rows,
            EraseInDisplay::EraseScrollback => {
                if let Err(error) = self.screen_mut().erase_scrollback() {
                    log::error!("refused escape-sequence scrollback erase: {error}");
                }
                return;
            }
        };

        {
            let bidi_mode = self.get_bidi_mode();
            let screen = self.screen_mut();
            for y in row_range {
                screen.clear_line(y, col_range.clone(), &pen, seqno, bidi_mode);
                let line_idx = screen.phys_row(y);
                screen.set_line_size(line_idx, LineSize::Single, seqno);
            }
        }
    }

    fn get_bidi_mode(&self) -> BidiMode {
        let mut mode = self.batch_config.bidi_mode;
        if let Some(enabled) = &self.bidi_enabled {
            mode.enabled = *enabled;
        }
        if let Some(hint) = &self.bidi_hint {
            mode.hint = *hint;
        }
        mode
    }

    fn perform_csi_edit(&mut self, edit: Edit) {
        let seqno = self.seqno;
        match edit {
            Edit::DeleteCharacter(n) => {
                let y = self.cursor.y;
                let x = self.cursor.x;

                if x >= self.left_and_right_margins.start && x < self.left_and_right_margins.end {
                    let right_margin = self.left_and_right_margins.end;
                    let limit = add_u32_to_usize_saturating(x, n).min(right_margin);

                    let blank_attr = self.pen.clone_sgr_only();
                    let screen = self.screen_mut();
                    for _ in x..limit as usize {
                        screen.erase_cell(x, y, right_margin, seqno, blank_attr.clone());
                    }
                }
            }
            Edit::DeleteLine(n) => {
                if self.top_and_bottom_margins.contains(&self.cursor.y)
                    && self.left_and_right_margins.contains(&self.cursor.x)
                {
                    let bidi_mode = self.get_bidi_mode();
                    let top_and_bottom_margins = self.cursor.y..self.top_and_bottom_margins.end;
                    let left_and_right_margins = self.left_and_right_margins.clone();
                    let blank_attr = self.pen.clone_sgr_only();
                    self.screen_mut().scroll_up_within_margins(
                        &top_and_bottom_margins,
                        &left_and_right_margins,
                        n as usize,
                        seqno,
                        blank_attr,
                        bidi_mode,
                    );
                }
            }
            Edit::EraseCharacter(n) => {
                let y = self.cursor.y;
                let x = self.cursor.x;
                let limit = add_u32_to_usize_saturating(x, n).min(self.screen().physical_cols);
                {
                    let blank = Cell::blank_with_attrs(self.pen.clone_sgr_only());
                    let screen = self.screen_mut();
                    for x in x..limit as usize {
                        screen.set_cell(x, y, &blank, seqno);
                    }
                }
            }

            Edit::EraseInLine(erase) => {
                let cx = self.cursor.x;
                let cy = self.cursor.y;
                let pen = self.pen.clone_sgr_only();
                let cols = self.screen().physical_cols;
                let bidi_mode = self.get_bidi_mode();
                let range = match erase {
                    // If wrap_next is true, then cx is effectively 1 column to the right.
                    // It feels wrong to handle this here, but in trying to centralize
                    // the logic for updating the cursor position, it causes regressions
                    // in the test suite.
                    // So this is here for now until a better solution is found.
                    // <https://github.com/wezterm/wezterm/issues/3548>
                    EraseInLine::EraseToEndOfLine => {
                        // Bind the start of the range to a variable: without
                        // the binding (or extra parens), the parser treats
                        // the `if` block as a statement and then the trailing
                        // `..cols` becomes a RangeTo, which then doesn't unify
                        // with the other match arms that produce Range<usize>.
                        let start = if self.wrap_next {
                            next_col_saturating(cx)
                        } else {
                            cx
                        };
                        start..cols
                    }
                    EraseInLine::EraseToStartOfLine => 0..next_col_saturating(cx),
                    EraseInLine::EraseLine => 0..cols,
                };

                self.screen_mut()
                    .clear_line(cy, range, &pen, seqno, bidi_mode);
            }
            Edit::InsertCharacter(n) => {
                // https://vt100.net/docs/vt510-rm/ICH.html
                // The ICH sequence inserts Pn blank characters with the normal character
                // attribute. The cursor remains at the beginning of the blank characters. Text
                // between the cursor and right margin moves to the right. Characters scrolled past
                // the right margin are lost. ICH has no effect outside the scrolling margins.

                let y = self.cursor.y;
                let x = self.cursor.x;
                if self.top_and_bottom_margins.contains(&y)
                    && self.left_and_right_margins.contains(&x)
                {
                    let right_margin = self.left_and_right_margins.end;
                    let screen = self.screen_mut();
                    for _ in 0..n as usize {
                        screen.insert_cell(x, y, right_margin, seqno);
                    }
                }
            }
            Edit::InsertLine(n) => {
                if self.top_and_bottom_margins.contains(&self.cursor.y)
                    && self.left_and_right_margins.contains(&self.cursor.x)
                {
                    let bidi_mode = self.get_bidi_mode();
                    let top_and_bottom_margins = self.cursor.y..self.top_and_bottom_margins.end;
                    let left_and_right_margins = self.left_and_right_margins.clone();
                    let blank_attr = self.pen.clone_sgr_only();
                    self.screen_mut().scroll_down_within_margins(
                        &top_and_bottom_margins,
                        &left_and_right_margins,
                        n as usize,
                        seqno,
                        blank_attr,
                        bidi_mode,
                    );
                }
            }
            Edit::ScrollDown(n) => self.scroll_down(n as usize),
            Edit::ScrollUp(n) => self.scroll_up(n as usize),
            Edit::EraseInDisplay(erase) => self.erase_in_display(erase),
            Edit::Repeat(n) => {
                let mut y = self.cursor.y;
                let mut x = self.cursor.x;
                let left_and_right_margins = self.left_and_right_margins.clone();
                let top_and_bottom_margins = self.top_and_bottom_margins.clone();

                // Resolve the source cell.  It may be a double-wide character.
                // Page-engine rows are read and written natively
                // (ft-yccm0.3.3.4).
                let cell = {
                    let screen = self.screen_mut();
                    let to_copy = x.saturating_sub(1);
                    let line_idx = screen.phys_row(y);

                    match screen.repeat_source(line_idx, to_copy) {
                        (None, _) => Cell::blank(),
                        (Some(candidate), prior) => {
                            if candidate.str() == " " && to_copy > 0 {
                                // It's a blank.  It may be the second part of
                                // a double-wide pair; look ahead of it.
                                match prior {
                                    Some(prior) if prior.width() > 1 => prior,
                                    _ => candidate,
                                }
                            } else {
                                candidate
                            }
                        }
                    }
                };

                for _ in 0..n {
                    {
                        let screen = self.screen_mut();
                        let line_idx = screen.phys_row(y);
                        screen.repeat_cell(line_idx, x, &cell, seqno);
                    }
                    x = next_col_saturating(x);
                    if x > last_col_in(&left_and_right_margins) {
                        x = left_and_right_margins.start;
                        if y == last_row_in(&top_and_bottom_margins) {
                            self.scroll_up(1);
                        } else {
                            y = next_row_saturating(y);
                            if y > last_row_in(&top_and_bottom_margins) {
                                y = top_and_bottom_margins.end;
                            }
                        }
                    }
                }
                self.cursor.x = x;
                self.cursor.y = y;
            }
        }
    }

    /// https://vt100.net/docs/vt510-rm/DECSLRM.html
    fn set_left_and_right_margins(&mut self, left: OneBased, right: OneBased) {
        // The terminal only recognizes this control function if vertical split
        // screen mode (DECLRMM) is set.
        if self.left_and_right_margin_mode {
            let cols = usize_to_u32_saturating(self.screen().physical_cols);
            let left = left.as_zero_based().min(cols.saturating_sub(1)) as usize;
            let right = right.as_zero_based().min(cols.saturating_sub(1)) as usize;

            // The value of the left margin (Pl) must be less than the right margin (Pr).
            if left >= right {
                return;
            }

            // The minimum size of the scrolling region is two columns per DEC,
            // but xterm allows 1.
            /*
            if right - left < 2 {
                return;
            }
            */

            self.left_and_right_margins = left..next_col_saturating(right);

            // DECSLRM moves the cursor to column 1, line 1 of the page.
            self.set_cursor_position_absolute(self.left_and_right_margins.start, 0);
            log::debug!(
                "SetLeftAndRightMargins {:?} (and move cursor to top left: {:?})",
                self.left_and_right_margins,
                self.cursor
            );
        }
    }

    fn perform_csi_cursor(&mut self, cursor: Cursor) {
        let seqno = self.seqno;
        match cursor {
            Cursor::SetTopAndBottomMargins { top, bottom } => {
                let rows = self.screen().physical_rows;
                let last_row = last_row_for_height(rows);
                let top = i64::from(top.as_zero_based()).min(last_row).max(0);
                let bottom = i64::from(bottom.as_zero_based()).min(last_row).max(0);
                if top >= bottom {
                    return;
                }
                self.top_and_bottom_margins = top..next_row_saturating(bottom);
                self.set_cursor_pos(&Position::Absolute(0), &Position::Absolute(0));
                log::debug!(
                    "SetTopAndBottomMargins {:?} (and move cursor to top left: {:?})",
                    self.top_and_bottom_margins,
                    self.cursor
                );
            }

            Cursor::SetLeftAndRightMargins { left, right } => {
                self.set_left_and_right_margins(left, right);
            }

            Cursor::ForwardTabulation(n) => {
                for _ in 0..n {
                    self.c0_horizontal_tab();
                }
            }
            Cursor::BackwardTabulation(n) => {
                for _ in 0..n {
                    let x = match self.tabs.find_prev_tab_stop(self.cursor.x) {
                        Some(x) => x,
                        None => 0,
                    };
                    self.set_cursor_pos(&Position::Absolute(x as i64), &Position::Relative(0));
                }
            }

            Cursor::TabulationClear(to_clear) => {
                self.tabs.clear(
                    to_clear,
                    self.cursor.x,
                    self.config.log_unknown_escape_sequences(),
                );
            }

            Cursor::TabulationControl(_) => {}
            Cursor::LineTabulation(_) => {}

            Cursor::Left(_n) => {
                // https://vt100.net/docs/vt510-rm/CUB.html
                unreachable!(
                    "Actually handled in Performer::csi_dispatch by rewriting as ControlCode::Backspace"
                );
            }

            Cursor::Right(n) => {
                // https://vt100.net/docs/vt510-rm/CUF.html
                let cols = self.screen().physical_cols;
                let new_x = if self.cursor.x >= self.left_and_right_margins.end {
                    // outside the margin, so allow movement to screen edge
                    add_u32_to_usize_saturating(self.cursor.x, n).min(last_col_for_width(cols))
                } else {
                    // Else constrain to margin
                    add_u32_to_usize_saturating(self.cursor.x, n)
                        .min(last_col_in(&self.left_and_right_margins))
                };

                self.cursor.x = new_x;
                self.cursor.seqno = seqno;
                self.wrap_next = false;
            }

            Cursor::Up(n) => {
                // https://vt100.net/docs/vt510-rm/CUU.html

                let candidate = self.cursor.y.saturating_sub(i64::from(n));
                let new_y = if self.cursor.y < self.top_and_bottom_margins.start {
                    // above the top margin, so allow movement to
                    // top of screen
                    candidate
                } else {
                    // Else constrain to top margin
                    if candidate < self.top_and_bottom_margins.start {
                        self.top_and_bottom_margins.start
                    } else {
                        candidate
                    }
                };

                let new_y = new_y.max(0);

                self.cursor.y = new_y;
                self.cursor.seqno = seqno;
                self.wrap_next = false;
            }
            Cursor::Down(n) => {
                // https://vt100.net/docs/vt510-rm/CUD.html
                let rows = self.screen().physical_rows;
                let new_y = if self.cursor.y >= self.top_and_bottom_margins.end {
                    // below the bottom margin, so allow movement to
                    // bottom of screen
                    add_u32_to_i64_saturating(self.cursor.y, n).min(last_row_for_height(rows))
                } else {
                    // Else constrain to bottom margin
                    add_u32_to_i64_saturating(self.cursor.y, n)
                        .min(last_row_in(&self.top_and_bottom_margins))
                };

                self.cursor.y = new_y;
                self.cursor.seqno = seqno;
                self.wrap_next = false;
            }

            Cursor::CharacterAndLinePosition { line, col } | Cursor::Position { line, col } => self
                .set_cursor_pos(
                    &Position::Absolute(i64::from(col.as_zero_based())),
                    &Position::Absolute(i64::from(line.as_zero_based())),
                ),
            Cursor::CharacterAbsolute(col) => self.set_cursor_pos(
                &Position::Absolute(i64::from(col.as_zero_based())),
                &Position::Relative(0),
            ),

            Cursor::CharacterPositionAbsolute(col) => {
                let col = col.as_zero_based() as usize;
                let col = if self.dec_origin_mode {
                    col.saturating_add(self.left_and_right_margins.start)
                } else {
                    col
                };
                self.cursor.x = col.min(last_col_for_width(self.screen().physical_cols));
                self.cursor.seqno = seqno;
                self.wrap_next = false;
            }

            Cursor::CharacterPositionBackward(col) => self.set_cursor_pos(
                &Position::Relative(-(i64::from(col))),
                &Position::Relative(0),
            ),
            Cursor::CharacterPositionForward(col) => {
                self.set_cursor_pos(&Position::Relative(i64::from(col)), &Position::Relative(0))
            }
            Cursor::LinePositionAbsolute(line) => self.set_cursor_pos(
                &Position::Relative(0),
                &Position::Absolute((i64::from(line)).saturating_sub(1)),
            ),
            Cursor::LinePositionBackward(line) => self.set_cursor_pos(
                &Position::Relative(0),
                &Position::Relative(-(i64::from(line))),
            ),
            Cursor::LinePositionForward(line) => {
                self.set_cursor_pos(&Position::Relative(0), &Position::Relative(i64::from(line)))
            }
            Cursor::NextLine(n) => {
                // https://vt100.net/docs/vt510-rm/CNL.html
                let rows = self.screen().physical_rows;
                let new_y = if self.cursor.y >= self.top_and_bottom_margins.end {
                    // below the bottom margin, so allow movement to
                    // bottom of screen
                    add_u32_to_i64_saturating(self.cursor.y, n).min(last_row_for_height(rows))
                } else {
                    // Else constrain to bottom margin
                    add_u32_to_i64_saturating(self.cursor.y, n)
                        .min(last_row_in(&self.top_and_bottom_margins))
                };

                self.cursor.y = new_y;
                self.cursor.x = self.left_and_right_margins.start;
                self.cursor.seqno = seqno;
                self.wrap_next = false;
            }
            Cursor::PrecedingLine(n) => {
                // https://vt100.net/docs/vt510-rm/CPL.html
                let candidate = self.cursor.y.saturating_sub(i64::from(n));
                let new_y = if self.cursor.y < self.top_and_bottom_margins.start {
                    // above the top margin, so allow movement to
                    // top of screen
                    candidate
                } else {
                    // Else constrain to top margin
                    if candidate < self.top_and_bottom_margins.start {
                        self.top_and_bottom_margins.start
                    } else {
                        candidate
                    }
                };

                let new_y = new_y.max(0);

                self.cursor.y = new_y;
                self.cursor.x = self.left_and_right_margins.start;
                self.cursor.seqno = seqno;
                self.wrap_next = false;
            }

            Cursor::ActivePositionReport { .. } => {
                // This is really a response from the terminal, and
                // we don't need to process it as a terminal command
            }
            Cursor::RequestActivePositionReport => {
                let line = OneBased::from_zero_based(i64_to_u32_saturating(
                    self.cursor.y.saturating_sub(if self.dec_origin_mode {
                        self.top_and_bottom_margins.start
                    } else {
                        0
                    }),
                ));
                let col = OneBased::from_zero_based(usize_to_u32_saturating(
                    self.cursor
                        .x
                        .min(self.screen().physical_cols.saturating_sub(1))
                        .saturating_sub(if self.dec_origin_mode {
                            self.left_and_right_margins.start
                        } else {
                            0
                        }),
                ));
                let report = CSI::Cursor(Cursor::ActivePositionReport { line, col });
                write!(self.writer, "{}", report).ok();
                self.writer.flush().ok();
            }
            Cursor::SaveCursor => {
                // The `CSI s` SaveCursor sequence is ambiguous with DECSLRM
                // with default parameters.  To resolve the ambiguity, DECSLRM
                // is recognized if DECLRMM mode is active which we do here
                // where we have the context!
                if self.left_and_right_margin_mode {
                    // https://vt100.net/docs/vt510-rm/DECSLRM.html
                    self.set_left_and_right_margins(
                        OneBased::new(1),
                        OneBased::new(
                            usize_to_u32_saturating(self.screen().physical_cols).saturating_add(1),
                        ),
                    );
                } else {
                    self.dec_save_cursor();
                }
            }
            Cursor::RestoreCursor => self.dec_restore_cursor(),
            Cursor::CursorStyle(style) => {
                self.cursor.shape = match style {
                    CursorStyle::Default => CursorShape::Default,
                    CursorStyle::BlinkingBlock => CursorShape::BlinkingBlock,
                    CursorStyle::SteadyBlock => CursorShape::SteadyBlock,
                    CursorStyle::BlinkingUnderline => CursorShape::BlinkingUnderline,
                    CursorStyle::SteadyUnderline => CursorShape::SteadyUnderline,
                    CursorStyle::BlinkingBar => CursorShape::BlinkingBar,
                    CursorStyle::SteadyBar => CursorShape::SteadyBar,
                };
                log::debug!("Cursor shape is now {:?}", self.cursor.shape);
            }
        }
    }

    /// https://vt100.net/docs/vt510-rm/DECSC.html
    fn dec_save_cursor(&mut self) {
        let saved = SavedCursor {
            position: self.cursor,
            wrap_next: self.wrap_next,
            wrap_next_column: self.wrap_next.then_some(self.left_and_right_margins.end),
            pen: self.pen.clone(),
            dec_origin_mode: self.dec_origin_mode,
            g0_charset: self.g0_charset,
            g1_charset: self.g1_charset,
        };
        debug!(
            "saving cursor {:?} is_alt={}",
            saved,
            self.screen.is_alt_screen_active()
        );
        *self.screen.saved_cursor() = Some(saved);
    }

    /// https://vt100.net/docs/vt510-rm/DECRC.html
    fn dec_restore_cursor(&mut self) {
        let saved = self
            .screen
            .saved_cursor()
            .clone()
            .unwrap_or_else(|| SavedCursor {
                position: CursorPosition::default(),
                wrap_next: false,
                wrap_next_column: None,
                pen: Default::default(),
                dec_origin_mode: false,
                g0_charset: CharSet::Ascii,
                g1_charset: CharSet::Ascii,
            });
        debug!(
            "restore cursor {:?} is_alt={}",
            saved,
            self.screen.is_alt_screen_active()
        );
        let x = saved.position.x;
        let y = saved.position.y;
        // Disable origin mode so that we can set the cursor position directly
        self.dec_origin_mode = false;
        self.set_cursor_pos(&Position::Absolute(x as i64), &Position::Absolute(y));
        self.cursor.shape = saved.position.shape;
        self.wrap_next = saved.wrap_next;
        self.pen = saved.pen;
        self.dec_origin_mode = saved.dec_origin_mode;
        self.g0_charset = saved.g0_charset;
        self.g1_charset = saved.g1_charset;
        self.shift_out = false;
        self.newline_mode = false;
    }

    fn perform_csi_sgr(&mut self, sgr: Sgr) {
        debug!("{:?}", sgr);
        match sgr {
            Sgr::Reset => {
                let link = self.pen.hyperlink().map(Arc::clone);
                let semantic_type = self.pen.semantic_type();
                self.pen = CellAttributes::default();
                self.pen.set_hyperlink(link);
                self.pen.set_semantic_type(semantic_type);
            }
            Sgr::Intensity(intensity) => {
                self.pen.set_intensity(intensity);
            }
            Sgr::Underline(underline) => {
                self.pen.set_underline(underline);
            }
            Sgr::Overline(overline) => {
                self.pen.set_overline(overline);
            }
            Sgr::VerticalAlign(align) => {
                self.pen.set_vertical_align(align);
            }
            Sgr::Blink(blink) => {
                self.pen.set_blink(blink);
            }
            Sgr::Italic(italic) => {
                self.pen.set_italic(italic);
            }
            Sgr::Inverse(inverse) => {
                self.pen.set_reverse(inverse);
            }
            Sgr::Invisible(invis) => {
                self.pen.set_invisible(invis);
            }
            Sgr::StrikeThrough(strike) => {
                self.pen.set_strikethrough(strike);
            }
            Sgr::Foreground(col) => {
                self.pen.set_foreground(col);
            }
            Sgr::Background(col) => {
                self.pen.set_background(col);
            }
            Sgr::UnderlineColor(col) => {
                self.pen.set_underline_color(col);
            }
            Sgr::Font(_) => {}
        }
    }

    /// Computes the set of `SemanticZone`s for the current terminal screen.
    /// Semantic zones are contiguous runs of cells that have the same
    /// `SemanticType` (Prompt, Input, Output).
    /// Due to the way that the terminal clears the screen, the raw, literal
    /// set of zones is overly fragmented by blanks.  This method will ignore
    /// trailing Output regions when computing the SemanticZone bounds.
    ///
    /// By default, all screen data is of type Output.  The shell needs to
    /// employ OSC 133 escapes to markup its output.
    pub fn get_semantic_zones(&mut self) -> anyhow::Result<Vec<SemanticZone>> {
        // Only memoized zone ranges change; querying them does not mutate the
        // canonical terminal model or consume a recovery generation.
        let screen = &mut self.screen;

        let mut current_zone: Option<SemanticZone> = None;
        let mut zones = vec![];

        let first_stable_row = screen.phys_to_stable_row_index(0);
        // Page-engine rows are read natively (ft-yccm0.3.3.4).
        screen.for_each_semantic_zone_range(|idx, semantic_type, range| {
            let stable_row = first_stable_row + idx as StableRowIndex;
            let zone_end_x = range.end.saturating_sub(1) as usize;
            let new_zone = match current_zone.as_ref() {
                None => true,
                Some(zone) => zone.semantic_type != semantic_type,
            };

            if new_zone {
                if let Some(zone) = current_zone.take() {
                    zones.push(zone);
                }

                current_zone.replace(SemanticZone {
                    start_x: range.start as usize,
                    start_y: stable_row,
                    end_x: zone_end_x,
                    end_y: stable_row,
                    semantic_type,
                });
            }

            if let Some(zone) = current_zone.as_mut() {
                zone.end_x = zone_end_x;
                zone.end_y = stable_row;
            }
        });
        if let Some(zone) = current_zone.take() {
            zones.push(zone);
        }

        Ok(zones)
    }

    /// Return the latest OSC 133 command status retained by the terminal.
    pub fn last_semantic_command_status(&self) -> Option<i32> {
        self.last_semantic_command_status
    }

    #[inline]
    pub fn get_reverse_video(&self) -> bool {
        self.reverse_video_mode
    }

    /// Whether DEC private mode 2026 (synchronized output) is
    /// currently set. Renderers should hold presentation while this
    /// is true and flush the accumulated frame when it transitions
    /// back to false. Term-layer source of truth shipped under
    /// ft-d7af6; renderer integration tracked under the
    /// continuation bead.
    #[inline]
    pub fn synchronized_output(&self) -> bool {
        self.synchronized_output
    }

    pub fn get_keyboard_encoding(&self) -> KeyboardEncoding {
        self.screen()
            .keyboard_stack
            .last()
            .copied()
            .unwrap_or(self.keyboard_encoding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ColorPalette;
    use std::sync::Arc;

    #[derive(Debug)]
    struct TestTermConfig {
        kitty_budget: usize,
        unicode_version: UnicodeVersion,
        scorecard_enabled: bool,
        checksum_rectangular_area: bool,
    }

    impl TerminalConfiguration for TestTermConfig {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn kitty_image_budget_bytes(&self) -> usize {
            self.kitty_budget
        }

        fn unicode_version(&self) -> UnicodeVersion {
            self.unicode_version.clone()
        }

        fn resize_wrap_scorecard_enabled(&self) -> bool {
            self.scorecard_enabled
        }

        fn enable_checksum_rectangular_area(&self) -> bool {
            self.checksum_rectangular_area
        }
    }

    fn test_terminal_state(config: Arc<dyn TerminalConfiguration>) -> TerminalState {
        TerminalState::new(
            TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            },
            config,
            "test-program",
            "1.0",
            Box::new(std::io::sink()),
        )
    }

    /// RIS is the recovery after a TUI exits uncleanly with modifyOtherKeys
    /// on; it must clear that, DECLRMM and the bidi overrides like DECSTR
    /// does (upstream WezTerm fe3006aef).
    #[test]
    fn full_reset_clears_modify_other_keys_margin_mode_and_bidi() {
        let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            kitty_budget: 1024,
            unicode_version: UnicodeVersion::new(14),
            scorecard_enabled: false,
            checksum_rectangular_area: false,
        });
        let mut terminal = crate::Terminal::new(
            TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            },
            config,
            "test",
            "1",
            Box::new(std::io::sink()),
        );
        // modifyOtherKeys=2, DECLRMM on, bidi enabled with an RTL hint.
        terminal.advance_bytes(b"\x1b[>4;2m\x1b[?69h\x1b[8h\x1b[?2501h");
        assert_eq!(terminal.modify_other_keys, Some(2));
        assert!(terminal.left_and_right_margin_mode);

        terminal.advance_bytes(b"\x1bc");
        assert_eq!(terminal.modify_other_keys, None);
        assert!(!terminal.left_and_right_margin_mode);
        assert_eq!(terminal.bidi_enabled, None);
        assert!(terminal.bidi_hint.is_none());
    }

    #[test]
    fn resize_saved_pending_wrap_preserves_subsequent_typing() {
        for (alternate, prepared) in [(false, false), (true, false), (false, true), (true, true)] {
            for (old_cols, new_cols, setup, alternate_setup, expected) in [
                (5, 10, "ABCDE", "", "ABCDEZ"),
                (10, 12, "\x1b[?69h\x1b[3;6sABCD", "", "  ABCDZ"),
                (10, 10, "\x1b[?69h\x1b[3;6sABCD", "", "  ABCDZ"),
                (5, 10, "abc界", "", "abc界Z"),
                (
                    10,
                    8,
                    "abcdefghijklmnopqrst",
                    "",
                    "abcdefgh\nijklmnop\nqrstZ",
                ),
                (10, 12, "\x1b[?69h\x1b[3;6sABCD", "\x1b[2;9s", "  ABCDZ"),
            ] {
                let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
                    kitty_budget: 1024,
                    unicode_version: UnicodeVersion::new(14),
                    scorecard_enabled: true,
                    checksum_rectangular_area: false,
                });
                let original_size = TerminalSize {
                    rows: 4,
                    cols: old_cols,
                    pixel_width: old_cols * 10,
                    pixel_height: 80,
                    dpi: 96,
                };
                let mut terminal = crate::Terminal::new(
                    original_size,
                    config,
                    "test",
                    "1",
                    Box::new(std::io::sink()),
                );
                terminal.advance_bytes(setup.as_bytes());
                if alternate {
                    terminal.advance_bytes(b"\x1b[?1049h");
                    terminal.advance_bytes(alternate_setup.as_bytes());
                } else {
                    terminal.advance_bytes(b"\x1b7\x1b[H");
                }
                assert!(
                    terminal
                        .screen
                        .screen
                        .saved_cursor
                        .as_ref()
                        .expect("the primary cursor was saved")
                        .wrap_next
                );

                let resized = TerminalSize {
                    cols: new_cols,
                    pixel_width: new_cols * 10,
                    ..original_size
                };
                if prepared {
                    let mut preparation = terminal.capture_reflow_preparation(resized);
                    if let Some(candidate) = preparation.as_mut() {
                        assert!(candidate.prepare(|| false));
                    }
                    terminal.resize_with_prepared_reflow(resized, preparation.as_mut());
                    if expected.contains('\n') {
                        assert!(preparation.as_ref().unwrap().was_applied());
                    }
                } else {
                    terminal.resize(resized);
                }
                terminal.advance_bytes(if alternate {
                    b"\x1b[?1049lZ".as_slice()
                } else {
                    b"\x1b8Z".as_slice()
                });

                // Restoring a pending wrap after widening must insert after the
                // original final glyph, including when it reached a narrow margin.
                // An independent text oracle catches two resize paths agreeing on
                // the same erroneous overwrite or premature newline.
                let lines = terminal.screen().all_lines();
                let nonempty: Vec<String> = lines
                    .iter()
                    .map(|line| line.as_str().trim_end_matches(' ').to_owned())
                    .filter(|line| !line.is_empty())
                    .collect();
                assert_eq!(
                    nonempty,
                    expected.split('\n').map(str::to_owned).collect::<Vec<_>>(),
                    "old_cols={old_cols} alternate={alternate} prepared={prepared}"
                );
            }
        }
    }

    #[test]
    fn resize_clipped_alternate_saved_cursor_does_not_invent_pending_wrap() {
        let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            kitty_budget: 1024,
            unicode_version: UnicodeVersion::new(14),
            scorecard_enabled: true,
            checksum_rectangular_area: false,
        });
        let size = TerminalSize {
            rows: 4,
            cols: 10,
            pixel_width: 100,
            pixel_height: 80,
            dpi: 96,
        };
        let mut terminal =
            crate::Terminal::new(size, config, "test", "1", Box::new(std::io::sink()));
        terminal.advance_bytes(b"\x1b[?1049hABCDEFG\x1b7");
        assert!(
            !terminal
                .screen
                .alt_screen
                .saved_cursor
                .as_ref()
                .unwrap()
                .wrap_next
        );
        terminal.resize(TerminalSize {
            cols: 5,
            pixel_width: 50,
            ..size
        });
        terminal.advance_bytes(b"\x1b8Z");
        let text: Vec<String> = terminal
            .screen()
            .all_lines()
            .iter()
            .map(|line| line.as_str().trim_end_matches(' ').to_owned())
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(text, vec!["ABCDZ"]);
    }

    #[test]
    fn prepared_terminal_resize_preserves_primary_alternate_and_saved_cursor() {
        for alternate_active in [false, true] {
            let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
                kitty_budget: 1024,
                unicode_version: UnicodeVersion::new(14),
                scorecard_enabled: true,
                checksum_rectangular_area: false,
            });
            let original_size = TerminalSize {
                rows: 4,
                cols: 12,
                pixel_width: 120,
                pixel_height: 80,
                dpi: 96,
            };
            let new_terminal = || {
                crate::Terminal::new(
                    original_size,
                    config.clone(),
                    "test",
                    "1",
                    Box::new(std::io::sink()),
                )
            };
            let mut actual = new_terminal();
            let mut expected = new_terminal();
            for terminal in [&mut actual, &mut expected] {
                terminal.advance_bytes("main ab界e\u{301}🦀cdefghijklmnop\r\nnext\x1b7\x1b[?1049hALTERNATE ab界cdef\x1b7".as_bytes());
                if !alternate_active {
                    terminal.advance_bytes(b"\x1b[?1049l");
                }
            }
            for (cols, rows, dpi) in [(5, 6, 144), (17, 3, 96), (7, 4, 144)] {
                let size = TerminalSize {
                    cols,
                    rows,
                    dpi,
                    pixel_width: cols * 10,
                    pixel_height: rows * 20,
                };
                let mut prepared = actual.capture_reflow_preparation(size).unwrap();
                assert!(prepared.prepare(|| false));
                expected.resize(size);
                actual.resize_with_prepared_reflow(size, Some(&mut prepared));
                assert!(
                    prepared.was_applied(),
                    "unchanged input must use prepared wraps"
                );
                assert_eq!(actual.get_size(), expected.get_size());
                assert_eq!(actual.cursor_pos(), expected.cursor_pos());
                assert_eq!(
                    actual.screen.screen.all_lines(),
                    expected.screen.screen.all_lines()
                );
                assert_eq!(
                    actual.screen.alt_screen.all_lines(),
                    expected.screen.alt_screen.all_lines()
                );
                assert_eq!(
                    actual.screen.alt_screen_is_active,
                    expected.screen.alt_screen_is_active
                );
                for (actual, expected) in [
                    (
                        &actual.screen.screen.saved_cursor,
                        &expected.screen.screen.saved_cursor,
                    ),
                    (
                        &actual.screen.alt_screen.saved_cursor,
                        &expected.screen.alt_screen.saved_cursor,
                    ),
                ] {
                    assert_eq!(
                        actual
                            .as_ref()
                            .map(|c| (c.position, c.wrap_next, c.wrap_next_column)),
                        expected
                            .as_ref()
                            .map(|c| (c.position, c.wrap_next, c.wrap_next_column))
                    );
                }
                assert_eq!(
                    actual.top_and_bottom_margins,
                    expected.top_and_bottom_margins
                );
                assert_eq!(
                    actual.left_and_right_margins,
                    expected.left_and_right_margins
                );
            }
            actual.advance_bytes(b"\x1b[?1049l\x1b8Z");
            expected.advance_bytes(b"\x1b[?1049l\x1b8Z");
            assert_eq!(actual.cursor_pos(), expected.cursor_pos());
            assert_eq!(
                actual.screen.screen.all_lines(),
                expected.screen.screen.all_lines()
            );
        }
    }

    #[test]
    fn prepared_terminal_resize_rejects_output_parsed_after_capture() {
        let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            kitty_budget: 1024,
            unicode_version: UnicodeVersion::new(14),
            scorecard_enabled: true,
            checksum_rectangular_area: false,
        });
        let original_size = TerminalSize {
            rows: 4,
            cols: 12,
            pixel_width: 120,
            pixel_height: 80,
            dpi: 96,
        };
        for output in ["\r\nnew 界e\u{301}🦀 output", "\x1b[2J\x1b[Hreplacement"] {
            let new_terminal = || {
                crate::Terminal::new(
                    original_size,
                    config.clone(),
                    "test",
                    "1",
                    Box::new(std::io::sink()),
                )
            };
            let mut actual = new_terminal();
            let mut expected = new_terminal();
            for terminal in [&mut actual, &mut expected] {
                terminal.advance_bytes(b"a logical line spanning several physical rows\r\ntail");
            }
            let size = TerminalSize {
                cols: 5,
                pixel_width: 50,
                ..original_size
            };
            let mut prepared = actual.capture_reflow_preparation(size).unwrap();
            assert!(prepared.prepare(|| false));
            actual.advance_bytes(output.as_bytes());
            expected.advance_bytes(output.as_bytes());
            expected.resize(size);
            actual.resize_with_prepared_reflow(size, Some(&mut prepared));
            assert!(
                !prepared.was_applied(),
                "new output must invalidate the snapshot"
            );
            assert_eq!(actual.get_size(), expected.get_size());
            assert_eq!(actual.cursor_pos(), expected.cursor_pos());
            assert_eq!(
                actual.screen.screen.all_lines(),
                expected.screen.screen.all_lines()
            );
            assert_eq!(
                actual.screen.alt_screen.all_lines(),
                expected.screen.alt_screen.all_lines()
            );
        }
    }

    #[derive(Clone, Debug, Default)]
    struct CaptureWriter {
        data: Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.data.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn terminal_state_with_capture(
        checksum_rectangular_area: bool,
    ) -> (TerminalState, Arc<std::sync::Mutex<Vec<u8>>>) {
        let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            kitty_budget: 1024,
            unicode_version: UnicodeVersion::new(14),
            scorecard_enabled: false,
            checksum_rectangular_area,
        });
        let capture = CaptureWriter::default();
        let data = capture.data.clone();
        let terminal = TerminalState::new(
            TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            },
            config,
            "test-program",
            "1.0",
            Box::new(capture),
        );
        (terminal, data)
    }

    /// Waits until the writer thread has written everything the terminal
    /// queued for its child; the writer never blocks the terminal itself.
    fn drain_writer(terminal: &mut TerminalState) {
        terminal
            .writer_barrier()
            .wait(std::time::Duration::from_secs(10))
            .expect("terminal writer drained");
    }

    /// A terminal whose child is the write end of a real pipe. Nothing reads
    /// the pipe until the test starts a reader, so a write larger than the
    /// pipe buffer blocks the writer thread exactly as a stalled child does.
    fn terminal_with_paused_reader() -> (TerminalState, std::io::PipeReader) {
        let (reader, writer) = std::io::pipe().expect("pipe");
        let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            kitty_budget: 1024,
            unicode_version: UnicodeVersion::new(14),
            scorecard_enabled: false,
            checksum_rectangular_area: false,
        });
        let terminal = TerminalState::new(
            TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            },
            config,
            "test-program",
            "1.0",
            Box::new(writer),
        );
        (terminal, reader)
    }

    /// Resumes the child: reads the pipe to EOF on another thread.
    fn resume_reader(mut reader: std::io::PipeReader) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut all = Vec::new();
            std::io::Read::read_to_end(&mut reader, &mut all).expect("read pipe");
            all
        })
    }

    /// Runs `f` on its own thread and fails if it takes more than ten
    /// seconds: a writer that blocked the caller would otherwise hang forever.
    fn finishes_promptly<T: Send + 'static>(
        what: &str,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(f());
        });
        finished
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("{} blocked on the terminal writer", what))
    }

    /// More than any pipe buffer holds, so the writer thread blocks in
    /// `write_all` until the reader resumes.
    const STALLING_PASTE_BYTES: usize = 1 << 20;

    #[test]
    fn input_and_replies_never_block_on_a_child_that_stops_reading() {
        let (terminal, reader) = terminal_with_paused_reader();
        let paste = "x".repeat(STALLING_PASTE_BYTES);
        let reply: &[u8] = b"\x1b[0n";
        let mut terminal = finishes_promptly("queueing for a stalled child", move || {
            let mut terminal = terminal;
            terminal.send_paste(&paste).unwrap();
            for _ in 0..100 {
                terminal
                    .key_down(KeyCode::Char('k'), KeyModifiers::NONE)
                    .unwrap();
                terminal.perform_device(Device::StatusReport);
            }
            terminal
                .mouse_event(MouseEvent {
                    kind: MouseEventKind::Press,
                    button: MouseButton::Left,
                    modifiers: KeyModifiers::NONE,
                    x: 0,
                    y: 0,
                    x_pixel_offset: 0,
                    y_pixel_offset: 0,
                })
                .unwrap();
            terminal.focus_changed(false);
            terminal
        });

        let backlog = terminal.writer_backlog();
        assert_eq!(
            backlog.pending_input_bytes,
            STALLING_PASTE_BYTES + 100,
            "the stalled paste and every keystroke stay queued: {backlog:?}"
        );
        assert_eq!(
            backlog.pending_reply_bytes,
            100 * reply.len(),
            "{backlog:?}"
        );
        assert_eq!(backlog.dropped_replies, 0, "{backlog:?}");

        let read = resume_reader(reader);
        drain_writer(&mut terminal);
        assert_eq!(terminal.writer_backlog(), WriterBacklog::default());
        drop(terminal);
        let output = read.join().unwrap();

        let mut expected = "x".repeat(STALLING_PASTE_BYTES).into_bytes();
        for _ in 0..100 {
            expected.push(b'k');
            expected.extend_from_slice(reply);
        }
        // Mouse reporting and focus tracking are off, so neither writes.
        assert_eq!(output.len(), expected.len());
        assert!(
            output == expected,
            "replies and keystrokes must keep FIFO order"
        );
    }

    #[test]
    fn replies_are_dropped_whole_once_the_child_stops_reading_but_input_is_kept() {
        let (mut terminal, reader) = terminal_with_paused_reader();
        terminal
            .send_paste(&"x".repeat(STALLING_PASTE_BYTES))
            .unwrap();
        // A reply written in two pieces must reach the child whole or not at
        // all: the drop decision is made once per flushed unit.
        let reply: [&[u8]; 2] = [b"\x1b[?2026;", b"2$y"];
        let reply_len = reply[0].len() + reply[1].len();
        let replies = REPLY_BACKLOG_LIMIT / reply_len + 100;
        let mut expected_tail = Vec::new();
        let mut kept_replies = 0usize;
        for i in 0..replies {
            if i % 1000 == 0 {
                terminal
                    .key_down(KeyCode::Char('k'), KeyModifiers::NONE)
                    .unwrap();
                expected_tail.push(b'k');
            }
            let kept = terminal.writer_backlog().pending_reply_bytes < REPLY_BACKLOG_LIMIT;
            // Bypass the BufWriter so each piece is a separate queue write:
            // the unit straddling the limit must not be cut in half.
            let threaded = terminal.writer.get_mut();
            threaded.write_all(reply[0]).unwrap();
            threaded.write_all(reply[1]).unwrap();
            threaded.flush().unwrap();
            if kept {
                kept_replies += 1;
                expected_tail.extend_from_slice(reply[0]);
                expected_tail.extend_from_slice(reply[1]);
            }
        }

        let backlog = terminal.writer_backlog();
        assert!(
            kept_replies < replies,
            "the backlog limit was never reached"
        );
        assert_eq!(
            backlog.pending_reply_bytes,
            kept_replies * reply_len,
            "{backlog:?}"
        );
        assert!(
            backlog.pending_reply_bytes >= REPLY_BACKLOG_LIMIT,
            "{:?}",
            backlog
        );
        assert_eq!(
            backlog.dropped_replies,
            (replies - kept_replies) as u64,
            "{backlog:?}"
        );
        assert_eq!(
            backlog.dropped_reply_bytes,
            ((replies - kept_replies) * reply_len) as u64,
            "{backlog:?}"
        );
        let keystrokes = replies.div_ceil(1000);
        assert_eq!(
            backlog.pending_input_bytes,
            STALLING_PASTE_BYTES + keystrokes,
            "user input is never dropped: {backlog:?}"
        );

        let read = resume_reader(reader);
        drain_writer(&mut terminal);
        drop(terminal);
        let output = read.join().unwrap();
        let (paste, tail) = output.split_at(STALLING_PASTE_BYTES);
        assert!(paste.iter().all(|&b| b == b'x'));
        assert!(
            tail == expected_tail.as_slice(),
            "every kept reply arrives whole and in order with the keystrokes"
        );
    }

    #[test]
    fn writer_barrier_times_out_while_the_child_is_stalled_then_completes() {
        let (mut terminal, reader) = terminal_with_paused_reader();
        terminal
            .send_paste(&"x".repeat(STALLING_PASTE_BYTES))
            .unwrap();
        let err = terminal
            .writer_barrier()
            .wait(std::time::Duration::from_millis(50))
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");

        let read = resume_reader(reader);
        drain_writer(&mut terminal);
        drop(terminal);
        assert_eq!(read.join().unwrap().len(), STALLING_PASTE_BYTES);
    }

    #[test]
    fn oversized_paste_is_refused_whole_and_later_input_still_flows() {
        let (mut terminal, output) = terminal_state_with_capture(false);
        let err = terminal
            .send_paste(&"y".repeat(MAX_PASTE_BYTES + 1))
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<PasteTooLarge>(),
            Some(&PasteTooLarge {
                len: MAX_PASTE_BYTES + 1,
                limit: MAX_PASTE_BYTES,
            })
        );
        assert!(err.to_string().contains("nothing was sent"), "{}", err);
        terminal.send_paste("ok").unwrap();
        drain_writer(&mut terminal);
        assert_eq!(output.lock().unwrap().as_slice(), b"ok");
    }

    #[test]
    fn user_input_class_nests_and_is_restored_after_every_input_path() {
        let (mut terminal, output) = terminal_state_with_capture(false);
        assert_eq!(terminal.writer.get_ref().class(), WriteClass::Reply);
        terminal.with_user_input(|terminal| {
            assert_eq!(terminal.writer.get_ref().class(), WriteClass::UserInput);
            terminal.with_user_input(|terminal| {
                assert_eq!(terminal.writer.get_ref().class(), WriteClass::UserInput);
            });
            assert_eq!(terminal.writer.get_ref().class(), WriteClass::UserInput);
        });
        assert_eq!(terminal.writer.get_ref().class(), WriteClass::Reply);

        terminal
            .key_down(KeyCode::Char('a'), KeyModifiers::NONE)
            .unwrap();
        assert_eq!(terminal.writer.get_ref().class(), WriteClass::Reply);
        terminal.send_paste("b").unwrap();
        assert_eq!(terminal.writer.get_ref().class(), WriteClass::Reply);
        terminal.focus_tracking = true;
        terminal.focus_changed(false);
        assert_eq!(terminal.writer.get_ref().class(), WriteClass::Reply);
        terminal.enqueue_reply(b"R").unwrap();

        drain_writer(&mut terminal);
        assert_eq!(output.lock().unwrap().as_slice(), b"ab\x1b[OR");
        assert_eq!(terminal.writer_backlog(), WriterBacklog::default());
    }

    #[test]
    fn checksum_rectangular_area_suppressed_by_default() {
        let (mut terminal, output) = terminal_state_with_capture(false);

        terminal.perform_csi_window(Window::ChecksumRectangularArea {
            request_id: 42,
            page_number: 0,
            top: OneBased::new(1),
            left: OneBased::new(1),
            bottom: OneBased::new(1),
            right: OneBased::new(1),
        });
        drain_writer(&mut terminal);

        assert!(
            output.lock().unwrap().is_empty(),
            "DECRQCRA must not write a screen checksum unless explicitly enabled",
        );
    }

    #[test]
    fn checksum_rectangular_area_can_be_enabled() {
        let (mut terminal, output) = terminal_state_with_capture(true);

        terminal.perform_csi_window(Window::ChecksumRectangularArea {
            request_id: 42,
            page_number: 0,
            top: OneBased::new(1),
            left: OneBased::new(1),
            bottom: OneBased::new(1),
            right: OneBased::new(1),
        });
        drain_writer(&mut terminal);

        let output = output.lock().unwrap().clone();
        assert!(
            output.starts_with(b"\x1bP42!~"),
            "DECRQCRA opt-in should emit a checksum response, got {:?}",
            String::from_utf8_lossy(&output),
        );
        assert!(output.ends_with(b"\x1b\\"));
    }

    #[test]
    fn checksum_rectangular_area_wraps_large_sums() {
        let (mut terminal, _output) = terminal_state_with_capture(true);

        for _ in 0..(80 * 24) {
            crate::terminalstate::performer::Performer::new(&mut terminal)
                .perform(frankenterm_escape_parser::Action::Print('A'));
        }

        let checksum = terminal.checksum_rectangle(0, 0, 79, 23);

        assert_eq!(checksum, ((u32::from(b'A') * 80 * 24) & 0xffff) as u16);
    }

    #[test]
    fn next_sequence_no_exhaustion_never_reuses_a_valid_witness() {
        assert_eq!(next_sequence_no(0), 1);
        assert_eq!(next_sequence_no(usize::MAX - 1), usize::MAX);
        assert_eq!(next_sequence_no(usize::MAX), usize::MAX);
    }

    #[test]
    fn semantic_generation_rejects_config_focus_and_mouse_aba() {
        let (mut terminal, _output) = terminal_state_with_capture(false);
        let initial_config = terminal.get_config();
        let original_version = terminal.unicode_version.clone();
        let before_config = terminal.current_seqno();
        terminal.set_config(Arc::new(TestTermConfig {
            kitty_budget: 2048,
            unicode_version: UnicodeVersion::new(14),
            scorecard_enabled: true,
            checksum_rectangular_area: false,
        }));
        terminal.set_config(initial_config);
        assert_eq!(terminal.unicode_version, original_version);
        assert!(terminal.current_seqno() > before_config);

        let original_focus = terminal.focused;
        let before_focus = terminal.current_seqno();
        terminal.focus_changed(!original_focus);
        terminal.focus_changed(original_focus);
        assert_eq!(terminal.focused, original_focus);
        assert!(terminal.current_seqno() > before_focus);

        let before_input = terminal.current_seqno();
        let event = MouseEvent {
            kind: MouseEventKind::Press,
            button: MouseButton::Left,
            modifiers: KeyModifiers::NONE,
            x: 0,
            y: 0,
            x_pixel_offset: 0,
            y_pixel_offset: 0,
        };
        assert!(terminal.current_mouse_buttons.is_empty());
        terminal.mouse_event(event).unwrap();
        assert_eq!(terminal.current_mouse_buttons, vec![MouseButton::Left]);
        terminal
            .mouse_event(MouseEvent {
                kind: MouseEventKind::Release,
                ..event
            })
            .unwrap();
        assert!(terminal.current_mouse_buttons.is_empty());
        assert!(terminal.current_seqno() > before_input);
    }

    /// Per ft-mv27v (cont of ft-d1pv3): TerminalConfiguration test
    /// double that lets the kitty cap-rejection integration test
    /// override the per-image transmission cap below the 16 MiB
    /// default, plus enables Kitty graphics so `kitty_img` doesn't
    /// short-circuit at the feature flag.
    #[derive(Debug)]
    struct KittyCapTestConfig {
        max_transmission: usize,
    }

    impl TerminalConfiguration for KittyCapTestConfig {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn enable_kitty_graphics(&self) -> bool {
            true
        }

        fn kitty_image_max_transmission_bytes(&self) -> usize {
            self.max_transmission
        }
    }

    /// ft-mv27v: oversized Kitty image payload is rejected before
    /// the image cache allocates an id. The test lowers the cap
    /// to 1 KiB and submits a 2 KiB DirectBin payload — kitty_img
    /// must return Err and id_to_data must remain empty.
    #[test]
    fn kitty_oversized_image_rejected_no_id_allocated() {
        use frankenterm_escape_parser::apc::{
            KittyImage, KittyImageCompression, KittyImageData, KittyImageFormat,
            KittyImageTransmit, KittyImageVerbosity,
        };

        let config: Arc<dyn TerminalConfiguration> = Arc::new(KittyCapTestConfig {
            max_transmission: 1024,
        });
        let mut terminal = test_terminal_state(config);

        // 2 KiB Rgba payload. Width/height claim 16x16 (= 1024
        // bytes by the substrate's width*height*4 ensure check)
        // but the actual data is 2 KiB, so bounded source loading rejects it
        // before width/height validation or image-id allocation.
        let oversized = vec![0u8; 2048];
        let img = KittyImage::TransmitData {
            transmit: KittyImageTransmit {
                format: Some(KittyImageFormat::Rgba),
                data: KittyImageData::DirectBin(oversized),
                width: Some(16),
                height: Some(16),
                image_id: Some(42),
                image_number: None,
                compression: KittyImageCompression::None,
                more_data_follows: false,
                alt_text: None,
            },
            verbosity: KittyImageVerbosity::Quiet,
        };

        let result = terminal.kitty_img(img);
        assert!(result.is_err(), "oversized Kitty payload must return Err",);
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("per-image cap") && err_msg.contains("2048"),
            "error message must explain the cap violation, got: {}",
            err_msg,
        );

        // The bead's privacy-relevant assertion: no image id was
        // allocated, so a malicious source cannot use a rejected
        // payload to consume an id-space slot.
        assert!(
            terminal.kitty_img.id_to_data_len() == 0,
            "rejected payload must not allocate an image_id",
        );
        assert_eq!(
            terminal.kitty_img.max_image_id(),
            0,
            "max_image_id must not advance for a rejected payload",
        );
    }

    /// ft-mv27v positive control: a payload below the cap is
    /// accepted and DOES allocate an image id. Confirms the
    /// gate isn't simply broken into refusing every input.
    #[test]
    fn kitty_under_cap_image_allocates_id() {
        use frankenterm_escape_parser::apc::{
            KittyImage, KittyImageCompression, KittyImageData, KittyImageFormat,
            KittyImageTransmit, KittyImageVerbosity,
        };

        let config: Arc<dyn TerminalConfiguration> = Arc::new(KittyCapTestConfig {
            max_transmission: 1024,
        });
        let mut terminal = test_terminal_state(config);

        // 256-byte Rgba payload claiming 8x8 (= 256 bytes). Fits
        // both the cap and the width*height*4 ensure check.
        let payload = vec![0u8; 8 * 8 * 4];
        let img = KittyImage::TransmitData {
            transmit: KittyImageTransmit {
                format: Some(KittyImageFormat::Rgba),
                data: KittyImageData::DirectBin(payload),
                width: Some(8),
                height: Some(8),
                image_id: Some(7),
                image_number: None,
                compression: KittyImageCompression::None,
                more_data_follows: false,
                alt_text: None,
            },
            verbosity: KittyImageVerbosity::Quiet,
        };

        let result = terminal.kitty_img(img);
        assert!(
            result.is_ok(),
            "under-cap Kitty payload must succeed: {:?}",
            result,
        );
        assert!(
            terminal.kitty_img.id_to_data_len() > 0,
            "under-cap payload must allocate an image_id",
        );
    }

    /// ft-mv27v: dropping the cap to 0 rejects every payload
    /// (substrate's `data.len() > cap` is strict-greater so a
    /// zero-byte payload would technically slip through, but that
    /// requires a separate path; this test covers the operator
    /// 'effectively disable Kitty graphics via cap=1' deployment).
    #[test]
    fn kitty_cap_one_byte_rejects_any_meaningful_payload() {
        use frankenterm_escape_parser::apc::{
            KittyImage, KittyImageCompression, KittyImageData, KittyImageFormat,
            KittyImageTransmit, KittyImageVerbosity,
        };

        let config: Arc<dyn TerminalConfiguration> = Arc::new(KittyCapTestConfig {
            max_transmission: 1,
        });
        let mut terminal = test_terminal_state(config);

        let img = KittyImage::TransmitData {
            transmit: KittyImageTransmit {
                format: Some(KittyImageFormat::Rgba),
                data: KittyImageData::DirectBin(vec![0u8; 64]),
                width: Some(4),
                height: Some(4),
                image_id: Some(99),
                image_number: None,
                compression: KittyImageCompression::None,
                more_data_follows: false,
                alt_text: None,
            },
            verbosity: KittyImageVerbosity::Quiet,
        };

        assert!(terminal.kitty_img(img).is_err());
        assert!(terminal.kitty_img.id_to_data_len() == 0);
    }

    // ── TabStop ────────────────────────────────────────────────

    #[test]
    fn tabstop_new_default_spacing() {
        let ts = TabStop::new(80, 8);
        assert!(ts.tabs[0], "column 0 should be a tab stop");
        assert!(ts.tabs[8], "column 8 should be a tab stop");
        assert!(ts.tabs[16], "column 16 should be a tab stop");
        assert!(!ts.tabs[1], "column 1 should not be a tab stop");
        assert!(!ts.tabs[7], "column 7 should not be a tab stop");
    }

    #[test]
    fn tabstop_new_width_four() {
        let ts = TabStop::new(20, 4);
        for i in 0..20 {
            assert_eq!(ts.tabs[i], i % 4 == 0, "col {} mismatch", i);
        }
    }

    #[test]
    fn tabstop_zero_width_clamps_to_one_instead_of_mod_by_zero() {
        // Pre-clamp, `i % tab_width` in the constructor (and `resize`) was a
        // mod-by-zero panic for tab_width == 0. The clamp treats it as 1:
        // every column is a tab stop, and resize stays panic-free.
        let mut ts = TabStop::new(8, 0);
        assert!(ts.tabs.iter().all(|&t| t), "width 1 → every column tabbed");
        ts.resize(16);
        assert_eq!(ts.tabs.len(), 16);
        assert!(ts.tabs.iter().all(|&t| t));
    }

    #[test]
    fn tabstop_find_next_from_zero() {
        let ts = TabStop::new(80, 8);
        assert_eq!(ts.find_next_tab_stop(0), Some(8));
    }

    #[test]
    fn tabstop_find_next_from_middle() {
        let ts = TabStop::new(80, 8);
        assert_eq!(ts.find_next_tab_stop(5), Some(8));
    }

    #[test]
    fn tabstop_find_next_past_last_returns_none() {
        let ts = TabStop::new(80, 8);
        // Last tab stop is at 72 (8*9)
        assert_eq!(ts.find_next_tab_stop(72), None);
    }

    #[test]
    fn tabstop_find_next_extreme_column_returns_none() {
        let ts = TabStop::new(80, 8);
        assert_eq!(ts.find_next_tab_stop(usize::MAX), None);
    }

    #[test]
    fn tabstop_find_prev_from_column_ten() {
        let ts = TabStop::new(80, 8);
        assert_eq!(ts.find_prev_tab_stop(10), Some(8));
    }

    #[test]
    fn tabstop_find_prev_from_column_eight() {
        let ts = TabStop::new(80, 8);
        assert_eq!(ts.find_prev_tab_stop(8), Some(0));
    }

    #[test]
    fn tabstop_find_prev_from_column_zero() {
        let ts = TabStop::new(80, 8);
        assert_eq!(ts.find_prev_tab_stop(0), None);
    }

    #[test]
    fn tabstop_set_tab_stop() {
        let mut ts = TabStop::new(80, 8);
        assert!(!ts.tabs[5]);
        ts.set_tab_stop(5);
        assert!(ts.tabs[5]);
    }

    #[test]
    fn tabstop_set_out_of_range_is_noop() {
        let mut ts = TabStop::new(80, 8);
        ts.set_tab_stop(usize::MAX);
        assert_eq!(ts.tabs.len(), 80);
    }

    #[test]
    fn tabstop_set_then_find_custom_stop() {
        let mut ts = TabStop::new(80, 8);
        ts.set_tab_stop(3);
        assert_eq!(ts.find_next_tab_stop(1), Some(3));
    }

    #[test]
    fn tabstop_resize_larger() {
        let mut ts = TabStop::new(40, 8);
        assert_eq!(ts.tabs.len(), 40);
        ts.resize(80);
        assert_eq!(ts.tabs.len(), 80);
        assert!(ts.tabs[40], "column 40 should be a tab stop after resize");
        assert!(ts.tabs[48], "column 48 should be a tab stop after resize");
    }

    #[test]
    fn tabstop_resize_smaller_no_shrink() {
        let mut ts = TabStop::new(80, 8);
        ts.resize(40);
        // Resize doesn't shrink, only grows
        assert_eq!(ts.tabs.len(), 80);
    }

    #[test]
    fn tabstop_clear_at_position() {
        let mut ts = TabStop::new(80, 8);
        assert!(ts.tabs[8]);
        ts.clear(
            TabulationClear::ClearCharacterTabStopAtActivePosition,
            8,
            false,
        );
        assert!(!ts.tabs[8]);
        // Other tab stops unaffected
        assert!(ts.tabs[16]);
    }

    #[test]
    fn tabstop_clear_all() {
        let mut ts = TabStop::new(80, 8);
        ts.clear(TabulationClear::ClearAllCharacterTabStops, 0, false);
        for i in 0..80 {
            assert!(!ts.tabs[i], "col {} should be cleared", i);
        }
    }

    #[test]
    fn tabstop_find_next_after_clear_all() {
        let mut ts = TabStop::new(80, 8);
        ts.clear(TabulationClear::ClearAllCharacterTabStops, 0, false);
        assert_eq!(ts.find_next_tab_stop(0), None);
    }

    #[test]
    fn tabstop_find_prev_beyond_width() {
        let ts = TabStop::new(10, 4);
        // col > width should still work due to .min()
        assert_eq!(ts.find_prev_tab_stop(100), Some(8));
    }

    #[test]
    fn set_config_refreshes_cached_defaults_and_screen_policy() {
        let initial: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            kitty_budget: 1024,
            unicode_version: UnicodeVersion::new(9),
            scorecard_enabled: false,
            checksum_rectangular_area: false,
        });
        let updated: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            kitty_budget: 2048,
            unicode_version: UnicodeVersion::new(14),
            scorecard_enabled: true,
            checksum_rectangular_area: false,
        });

        let mut terminal = test_terminal_state(initial);
        terminal.set_config(updated);

        assert_eq!(terminal.kitty_img.image_budget_bytes, 2048);
        assert_eq!(terminal.unicode_version, UnicodeVersion::new(14));
        assert!(
            terminal
                .screen
                .screen
                .resize_wrap_policy()
                .scorecard_enabled,
            "primary screen should refresh resize-wrap policy from the new config"
        );
        assert!(
            terminal
                .screen
                .alt_screen
                .resize_wrap_policy()
                .scorecard_enabled,
            "alternate screen should refresh resize-wrap policy from the new config"
        );
    }

    #[test]
    fn set_config_preserves_runtime_unicode_override() {
        let initial: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            kitty_budget: 1024,
            unicode_version: UnicodeVersion::new(9),
            scorecard_enabled: false,
            checksum_rectangular_area: false,
        });
        let updated: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            kitty_budget: 2048,
            unicode_version: UnicodeVersion::new(14),
            scorecard_enabled: true,
            checksum_rectangular_area: false,
        });

        let mut terminal = test_terminal_state(initial);
        terminal.unicode_version = UnicodeVersion::new(15);

        terminal.set_config(updated);

        assert_eq!(
            terminal.unicode_version,
            UnicodeVersion::new(15),
            "config reload should not discard an application-provided unicode override"
        );
        assert_eq!(terminal.kitty_img.image_budget_bytes, 2048);
    }

    #[test]
    fn dec_private_mode_restore_reapplies_saved_set_state() {
        let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            kitty_budget: 1024,
            unicode_version: UnicodeVersion::new(14),
            scorecard_enabled: false,
            checksum_rectangular_area: false,
        });
        let mut terminal = test_terminal_state(config);

        assert!(terminal.cursor_visible);
        terminal.perform_csi_mode(Mode::SaveDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::ShowCursor,
        )));
        terminal.perform_csi_mode(Mode::ResetDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::ShowCursor,
        )));
        assert!(!terminal.cursor_visible);

        terminal.perform_csi_mode(Mode::RestoreDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::ShowCursor,
        )));

        assert!(terminal.cursor_visible);
    }

    #[test]
    fn dec_private_mode_restore_reapplies_saved_reset_state() {
        let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            kitty_budget: 1024,
            unicode_version: UnicodeVersion::new(14),
            scorecard_enabled: false,
            checksum_rectangular_area: false,
        });
        let mut terminal = test_terminal_state(config);

        assert!(!terminal.bracketed_paste);
        terminal.perform_csi_mode(Mode::SaveDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::BracketedPaste,
        )));
        terminal.perform_csi_mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::BracketedPaste,
        )));
        assert!(terminal.bracketed_paste);

        terminal.perform_csi_mode(Mode::RestoreDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::BracketedPaste,
        )));

        assert!(!terminal.bracketed_paste);
    }

    #[test]
    fn dec_private_mode_restore_handles_alternate_screen_state() {
        let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            kitty_budget: 1024,
            unicode_version: UnicodeVersion::new(14),
            scorecard_enabled: false,
            checksum_rectangular_area: false,
        });
        let mut terminal = test_terminal_state(config);

        assert!(!terminal.screen.is_alt_screen_active());
        terminal.perform_csi_mode(Mode::SaveDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::ClearAndEnableAlternateScreen,
        )));
        terminal.perform_csi_mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::ClearAndEnableAlternateScreen,
        )));
        assert!(terminal.screen.is_alt_screen_active());

        terminal.perform_csi_mode(Mode::RestoreDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::ClearAndEnableAlternateScreen,
        )));

        assert!(!terminal.screen.is_alt_screen_active());
    }

    #[test]
    fn unsaved_dec_private_mode_restore_is_noop() {
        let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            kitty_budget: 1024,
            unicode_version: UnicodeVersion::new(14),
            scorecard_enabled: false,
            checksum_rectangular_area: false,
        });
        let mut terminal = test_terminal_state(config);

        terminal.perform_csi_mode(Mode::ResetDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::ShowCursor,
        )));
        terminal.perform_csi_mode(Mode::RestoreDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::ShowCursor,
        )));

        assert!(!terminal.cursor_visible);
    }

    #[test]
    fn full_reset_clears_saved_dec_private_modes() {
        let config: Arc<dyn TerminalConfiguration> = Arc::new(TestTermConfig {
            kitty_budget: 1024,
            unicode_version: UnicodeVersion::new(14),
            scorecard_enabled: false,
            checksum_rectangular_area: false,
        });
        let mut terminal = test_terminal_state(config);

        terminal.perform_csi_mode(Mode::SaveDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::ShowCursor,
        )));
        terminal.perform_csi_mode(Mode::ResetDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::ShowCursor,
        )));
        assert!(!terminal.cursor_visible);

        crate::terminalstate::performer::Performer::new(&mut terminal).perform(
            frankenterm_escape_parser::Action::Esc(frankenterm_escape_parser::Esc::Code(
                frankenterm_escape_parser::EscCode::FullReset,
            )),
        );

        assert!(terminal.cursor_visible);
        terminal.perform_csi_mode(Mode::RestoreDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::ShowCursor,
        )));

        assert!(terminal.cursor_visible);
    }

    // ── CharSet ────────────────────────────────────────────────

    #[test]
    fn charset_eq() {
        assert_eq!(CharSet::Ascii, CharSet::Ascii);
        assert_ne!(CharSet::Ascii, CharSet::Uk);
        assert_ne!(CharSet::Uk, CharSet::DecLineDrawing);
    }

    #[test]
    fn charset_debug() {
        let debug = format!("{:?}", CharSet::DecLineDrawing);
        assert!(debug.contains("DecLineDrawing"));
    }

    #[test]
    fn charset_clone() {
        let a = CharSet::Uk;
        let b = a;
        assert_eq!(a, b);
    }

    // ── MouseEncoding ──────────────────────────────────────────

    #[test]
    fn mouse_encoding_eq() {
        assert_eq!(MouseEncoding::X10, MouseEncoding::X10);
        assert_ne!(MouseEncoding::X10, MouseEncoding::SGR);
        assert_ne!(MouseEncoding::SGR, MouseEncoding::SgrPixels);
    }

    #[test]
    fn mouse_encoding_debug() {
        let debug = format!("{:?}", MouseEncoding::SgrPixels);
        assert!(debug.contains("SgrPixels"));
    }

    #[test]
    fn usize_to_i64_conversion_saturates_extreme_values() {
        assert_eq!(usize_to_i64_saturating(42), 42);
        assert_eq!(usize_to_i64_saturating(usize::MAX), i64::MAX);
        assert_eq!(usize_to_u32_saturating(42), 42);
        assert_eq!(usize_to_u32_saturating(usize::MAX), u32::MAX);
        assert_eq!(i64_to_u32_saturating(-1), 0);
        assert_eq!(i64_to_u32_saturating(42), 42);
        assert_eq!(i64_to_u32_saturating(i64::MAX), u32::MAX);
    }

    #[test]
    fn cursor_add_helpers_saturate_extreme_values() {
        assert_eq!(add_u32_to_usize_saturating(5, 3), 8);
        assert_eq!(add_u32_to_usize_saturating(usize::MAX, 3), usize::MAX);
        assert_eq!(add_u32_to_i64_saturating(5, 3), 8);
        assert_eq!(add_u32_to_i64_saturating(i64::MAX, 3), i64::MAX);
        assert_eq!(next_col_saturating(5), 6);
        assert_eq!(next_col_saturating(usize::MAX), usize::MAX);
        assert_eq!(next_row_saturating(5), 6);
        assert_eq!(next_row_saturating(i64::MAX), i64::MAX);
        assert_eq!(last_col_in(&(0..0)), 0);
        assert_eq!(last_col_in(&(2..5)), 4);
        assert_eq!(last_row_in(&(0..0)), 0);
        assert_eq!(last_row_in(&(2..5)), 4);
        assert_eq!(last_col_for_width(0), 0);
        assert_eq!(last_col_for_width(5), 4);
        assert_eq!(last_row_for_height(0), 0);
        assert_eq!(last_row_for_height(5), 4);
    }

    // ── ScreenOrAlt ────────────────────────────────────────────

    #[test]
    fn saved_cursor_debug() {
        let sc = SavedCursor {
            position: CursorPosition::default(),
            wrap_next: false,
            wrap_next_column: None,
            pen: CellAttributes::default(),
            dec_origin_mode: false,
            g0_charset: CharSet::Ascii,
            g1_charset: CharSet::Ascii,
        };
        let debug = format!("{sc:?}");
        assert!(debug.contains("SavedCursor"));
    }

    #[test]
    fn saved_cursor_clone() {
        let sc = SavedCursor {
            position: CursorPosition::default(),
            wrap_next: true,
            wrap_next_column: Some(80),
            pen: CellAttributes::default(),
            dec_origin_mode: true,
            g0_charset: CharSet::DecLineDrawing,
            g1_charset: CharSet::Uk,
        };
        let sc2 = sc.clone();
        assert!(sc2.wrap_next);
        assert_eq!(sc2.wrap_next_column, Some(80));
        assert!(sc2.dec_origin_mode);
        assert_eq!(sc2.g0_charset, CharSet::DecLineDrawing);
        assert_eq!(sc2.g1_charset, CharSet::Uk);
    }
}
