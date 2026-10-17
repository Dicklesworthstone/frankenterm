//! The six ingest lanes and the end-state sanity checks.
//!
//! - `parse`: `Parser::parse` into a reused `Vec<Action>`, cleared per chunk.
//! - `term`: `Terminal::advance_bytes` per chunk (the fused single-stage path).
//! - `mux_two_stage`: `Parser::parse` into a fresh `Vec<Action>` per chunk, then
//!   `Terminal::perform_actions`, the mux parse thread's shape before
//!   ft-yccm0.3.2.1 (`parse_buffered_data` in `frankenterm/mux/src/lib.rs`).
//! - `prod_config`: `mux_two_stage` with [`ProdConfig`], which pays the GUI's
//!   per-read configuration cost (mutex lock + `Arc` clone).
//! - `mux_fused`: the mux parse thread's fused path (ft-yccm0.3.2.1): its own
//!   parser feeds the terminal through `Terminal::feed`, and actions the gate
//!   diverts (alert sources and synchronized-output controls, as the mux
//!   gate does) are applied afterwards with `perform_actions`.
//! - `prod_config_fused`: `mux_fused` with [`ProdConfig`], the GUI's path.
//!
//! Timing covers the ingest loop only: terminal construction, the end-state
//! summary and dropping the terminal fall outside it.

use std::collections::hash_map::DefaultHasher;
use std::convert::TryFrom;
use std::hash::{Hash, Hasher};
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use frankenterm_cell::UnicodeVersion;
use frankenterm_escape_parser::parser::Parser;
use frankenterm_escape_parser::{Action, ControlCode, Esc, EscCode, CSI};
use frankenterm_surface::line::MonospaceKpCostModel;
use frankenterm_term::color::ColorPalette;
use frankenterm_term::config::{
    BidiMode, GridEngine, NewlineCanon, Osc52WritePolicy, ScrollbackTierConfig,
    TerminalConfigurationRevision,
};
use frankenterm_term::{FeedGate, Terminal, TerminalConfiguration, TerminalSize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lane {
    Parse,
    Term,
    MuxTwoStage,
    ProdConfig,
    MuxFused,
    ProdConfigFused,
    /// The `term` lane with the screen's rows in PageGrid pages
    /// (ft-yccm0.3.3.4, `FT_GRID_ENGINE=page`).
    TermPage,
}

impl Lane {
    pub const ALL: [Lane; 7] = [
        Lane::Parse,
        Lane::Term,
        Lane::MuxTwoStage,
        Lane::ProdConfig,
        Lane::MuxFused,
        Lane::ProdConfigFused,
        Lane::TermPage,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Lane::Parse => "parse",
            Lane::Term => "term",
            Lane::MuxTwoStage => "mux_two_stage",
            Lane::ProdConfig => "prod_config",
            Lane::MuxFused => "mux_fused",
            Lane::ProdConfigFused => "prod_config_fused",
            Lane::TermPage => "term_page",
        }
    }

    /// Whether the lane pays the GUI's configuration cost ([`ProdConfig`]).
    pub fn uses_prod_config(self) -> bool {
        matches!(self, Lane::ProdConfig | Lane::ProdConfigFused)
    }

    /// Accepts the canonical name with `-` or `_` separators.
    pub fn from_name(name: &str) -> Option<Self> {
        let normalized = name.replace('-', "_");
        Self::ALL
            .iter()
            .copied()
            .find(|lane| lane.name() == normalized)
    }
}

/// Terminal geometry and feed size. The defaults match the planning
/// prototype (`evidence/mac-render-perf/2026-10-05/ftbench`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub rows: usize,
    pub cols: usize,
    pub scrollback: usize,
    /// Bytes handed to the terminal per call.
    pub chunk: usize,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            rows: 80,
            cols: 120,
            scrollback: 3500,
            chunk: 128 * 1024,
        }
    }
}

/// Minimal configuration: every read is a field or a trait default, so the
/// `term` and `mux_two_stage` lanes measure the terminal alone.
#[derive(Debug)]
pub struct BenchConfig {
    scrollback: usize,
    grid_engine: GridEngine,
}

impl BenchConfig {
    /// The grid engine `FT_GRID_ENGINE` selects (legacy by default).
    pub fn new(scrollback: usize) -> Self {
        Self::with_grid_engine(scrollback, GridEngine::from_env())
    }

    pub fn with_grid_engine(scrollback: usize, grid_engine: GridEngine) -> Self {
        Self {
            scrollback,
            grid_engine,
        }
    }
}

impl TerminalConfiguration for BenchConfig {
    fn scrollback_size(&self) -> usize {
        self.scrollback
    }

    fn grid_engine(&self) -> GridEngine {
        self.grid_engine
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

/// Mirrors the GUI's `TermConfig` (`frankenterm/config/src/terminal.rs`):
/// every read that `TermConfig` routes through `configuration()` locks a mutex
/// and clones the shared `Arc`, `color_palette` locks the client-palette mutex
/// first, and `revision` fences an overlay generation. The values are
/// [`BenchConfig`]'s, so the `prod_config` lane differs from `mux_two_stage`
/// only in what the reads cost. The recovery-activation lease is left at the
/// trait default because ingest never takes it.
#[derive(Debug)]
pub struct ProdConfig {
    config: Mutex<Option<Arc<BenchConfig>>>,
    client_palette: Mutex<Option<ColorPalette>>,
    overlay_generation: AtomicUsize,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ProdConfig {
    pub fn new(scrollback: usize) -> Self {
        Self {
            config: Mutex::new(Some(Arc::new(BenchConfig::new(scrollback)))),
            client_palette: Mutex::new(None),
            overlay_generation: AtomicUsize::new(0),
        }
    }

    fn configuration(&self) -> Arc<BenchConfig> {
        lock(&self.config)
            .as_ref()
            .cloned()
            .expect("ProdConfig always holds a configuration")
    }
}

impl TerminalConfiguration for ProdConfig {
    fn osc52_write_policy(&self) -> Osc52WritePolicy {
        self.configuration().osc52_write_policy()
    }

    fn osc52_write_max_bytes(&self) -> usize {
        self.configuration().osc52_write_max_bytes()
    }

    fn generation(&self) -> usize {
        self.configuration().generation()
    }

    fn revision(&self) -> TerminalConfigurationRevision {
        loop {
            let overlay_generation = self.overlay_generation.load(Ordering::Acquire);
            let base_generation = self.configuration().generation();
            if self.overlay_generation.load(Ordering::Acquire) == overlay_generation {
                return TerminalConfigurationRevision::new(base_generation, overlay_generation);
            }
        }
    }

    fn scrollback_size(&self) -> usize {
        self.configuration().scrollback_size()
    }

    fn scrollback_tier_config(&self) -> ScrollbackTierConfig {
        self.configuration().scrollback_tier_config()
    }

    fn resize_wrap_kp_cost_model(&self) -> MonospaceKpCostModel {
        self.configuration().resize_wrap_kp_cost_model()
    }

    fn resize_wrap_scorecard_enabled(&self) -> bool {
        self.configuration().resize_wrap_scorecard_enabled()
    }

    fn resize_wrap_readability_gate_enabled(&self) -> bool {
        self.configuration().resize_wrap_readability_gate_enabled()
    }

    fn resize_wrap_readability_max_line_badness_delta(&self) -> i64 {
        self.configuration()
            .resize_wrap_readability_max_line_badness_delta()
    }

    fn resize_wrap_readability_max_total_badness_delta(&self) -> i64 {
        self.configuration()
            .resize_wrap_readability_max_total_badness_delta()
    }

    fn resize_wrap_readability_max_fallback_ratio_percent(&self) -> u8 {
        self.configuration()
            .resize_wrap_readability_max_fallback_ratio_percent()
    }

    fn enable_csi_u_key_encoding(&self) -> bool {
        self.configuration().enable_csi_u_key_encoding()
    }

    fn color_palette(&self) -> ColorPalette {
        if let Some(palette) = lock(&self.client_palette).as_ref().cloned() {
            return palette;
        }
        self.configuration().color_palette()
    }

    fn alternate_buffer_wheel_scroll_speed(&self) -> u8 {
        self.configuration().alternate_buffer_wheel_scroll_speed()
    }

    fn enq_answerback(&self) -> String {
        self.configuration().enq_answerback()
    }

    fn enable_kitty_graphics(&self) -> bool {
        self.configuration().enable_kitty_graphics()
    }

    fn enable_title_reporting(&self) -> bool {
        self.configuration().enable_title_reporting()
    }

    fn enable_checksum_rectangular_area(&self) -> bool {
        self.configuration().enable_checksum_rectangular_area()
    }

    fn enable_kitty_keyboard(&self) -> bool {
        self.configuration().enable_kitty_keyboard()
    }

    fn canonicalize_pasted_newlines(&self) -> NewlineCanon {
        self.configuration().canonicalize_pasted_newlines()
    }

    fn unicode_version(&self) -> UnicodeVersion {
        self.configuration().unicode_version()
    }

    fn debug_key_events(&self) -> bool {
        self.configuration().debug_key_events()
    }

    fn log_unknown_escape_sequences(&self) -> bool {
        self.configuration().log_unknown_escape_sequences()
    }

    fn normalize_output_to_unicode_nfc(&self) -> bool {
        self.configuration().normalize_output_to_unicode_nfc()
    }

    fn bidi_mode(&self) -> BidiMode {
        self.configuration().bidi_mode()
    }

    fn max_user_vars(&self) -> usize {
        self.configuration().max_user_vars()
    }

    fn max_unicode_version_stack_depth(&self) -> usize {
        self.configuration().max_unicode_version_stack_depth()
    }

    fn max_accumulating_title_len(&self) -> usize {
        self.configuration().max_accumulating_title_len()
    }

    fn max_color_map_entries(&self) -> usize {
        self.configuration().max_color_map_entries()
    }
}

/// Builds the terminal a lane drives. Only the `prod_config` lanes use
/// [`ProdConfig`].
pub fn new_terminal(lane: Lane, geometry: &Geometry) -> Terminal {
    let config: Arc<dyn TerminalConfiguration + Send + Sync> = if lane.uses_prod_config() {
        Arc::new(ProdConfig::new(geometry.scrollback))
    } else if lane == Lane::TermPage {
        Arc::new(BenchConfig::with_grid_engine(
            geometry.scrollback,
            GridEngine::Page,
        ))
    } else {
        Arc::new(BenchConfig::new(geometry.scrollback))
    };
    Terminal::new(
        TerminalSize {
            rows: geometry.rows,
            cols: geometry.cols,
            pixel_width: geometry.cols * 8,
            pixel_height: geometry.rows * 16,
            dpi: 96,
        },
        config,
        "frankenterm-ingest-bench",
        env!("CARGO_PKG_VERSION"),
        Box::new(std::io::sink()),
    )
}

pub struct LaneRun {
    pub lane: Lane,
    /// Timed bytes.
    pub bytes: usize,
    /// Untimed bytes fed first (`--sticky-zwj`).
    pub prelude_bytes: usize,
    pub secs: f64,
    /// Parser actions produced. `None` for the fused lanes, whose parser
    /// hands each action straight to the performer.
    pub actions: Option<u64>,
    /// The final terminal; `None` for `parse`.
    pub terminal: Option<Terminal>,
}

impl LaneRun {
    pub fn mib_per_s(&self) -> f64 {
        if self.secs > 0.0 {
            mib(self.bytes) / self.secs
        } else {
            0.0
        }
    }
}

pub fn mib(bytes: usize) -> f64 {
    // Exact below 2^53 bytes.
    bytes as f64 / f64::from(1_u32 << 20)
}

fn count(actions: usize) -> u64 {
    u64::try_from(actions).expect("action count fits in u64")
}

/// Prelude for `--sticky-zwj`: a ZWJ sequence cut off by CR LF writes a cell
/// ending in U+200D, which leaves the terminal's ZWJ-tail flag set for the
/// whole run. That is the ft-yccm0.2.14 scenario, a pane that once printed a
/// split ZWJ emoji.
pub const STICKY_ZWJ_PRELUDE: &[u8] = "\u{1F468}\u{200D}\r\n".as_bytes();

/// What the mux's fused gate diverts (`FusedFeedGate` in
/// `frankenterm/mux/src/localpane.rs`): every alert source and the
/// synchronized-output controls.
struct MuxGate;

impl FeedGate for MuxGate {
    fn diverts(&mut self, action: &Action) -> bool {
        use frankenterm_escape_parser::csi::{DecPrivateMode, DecPrivateModeCode, Device, Mode};
        match action {
            Action::Control(ControlCode::Bell)
            | Action::OperatingSystemCommand(_)
            | Action::KittyImage(_)
            | Action::Esc(Esc::Code(EscCode::StringTerminator | EscCode::FullReset)) => true,
            Action::CSI(CSI::Mode(
                Mode::SetDecPrivateMode(code)
                | Mode::ResetDecPrivateMode(code)
                | Mode::QueryDecPrivateMode(code),
            )) => matches!(
                code,
                DecPrivateMode::Code(DecPrivateModeCode::SynchronizedOutput)
            ),
            Action::CSI(CSI::Device(device)) => matches!(**device, Device::SoftReset),
            _ => false,
        }
    }

    /// As the mux gate: SGR never leaves the fused path.
    fn diverts_sgr(&mut self, _sgr: &frankenterm_escape_parser::csi::Sgr) -> bool {
        false
    }
}

/// Feeds `prelude` untimed, then `data` timed, through `lane` in
/// `geometry.chunk`-sized pieces.
pub fn run_lane(lane: Lane, prelude: &[u8], data: &[u8], geometry: &Geometry) -> LaneRun {
    let chunk = geometry.chunk.max(1);
    match lane {
        Lane::Parse => {
            let mut parser = Parser::new();
            parser.parse(prelude, |_| {});
            let mut actions = Vec::new();
            let mut total = 0;
            let start = Instant::now();
            for piece in data.chunks(chunk) {
                parser.parse(piece, |action| actions.push(action));
                total += actions.len();
                black_box(&actions);
                actions.clear();
            }
            let secs = start.elapsed().as_secs_f64();
            LaneRun {
                lane,
                bytes: data.len(),
                prelude_bytes: prelude.len(),
                secs,
                actions: Some(count(total)),
                terminal: None,
            }
        }
        Lane::Term | Lane::TermPage => {
            let mut terminal = new_terminal(lane, geometry);
            if !prelude.is_empty() {
                terminal.advance_bytes(prelude);
            }
            let start = Instant::now();
            for piece in data.chunks(chunk) {
                terminal.advance_bytes(piece);
            }
            let secs = start.elapsed().as_secs_f64();
            LaneRun {
                lane,
                bytes: data.len(),
                prelude_bytes: prelude.len(),
                secs,
                actions: None,
                terminal: Some(terminal),
            }
        }
        Lane::MuxFused | Lane::ProdConfigFused => {
            let mut terminal = new_terminal(lane, geometry);
            let mut parser = Parser::new();
            let mut diverted = Vec::new();
            if !prelude.is_empty() {
                terminal.feed(&mut parser, prelude, &mut MuxGate, &mut diverted);
                terminal.perform_actions(std::mem::take(&mut diverted));
            }
            let start = Instant::now();
            for piece in data.chunks(chunk) {
                terminal.feed(&mut parser, piece, &mut MuxGate, &mut diverted);
                if !diverted.is_empty() {
                    terminal.perform_actions(std::mem::take(&mut diverted));
                }
            }
            let secs = start.elapsed().as_secs_f64();
            LaneRun {
                lane,
                bytes: data.len(),
                prelude_bytes: prelude.len(),
                secs,
                actions: None,
                terminal: Some(terminal),
            }
        }
        Lane::MuxTwoStage | Lane::ProdConfig => {
            let mut terminal = new_terminal(lane, geometry);
            let mut parser = Parser::new();
            if !prelude.is_empty() {
                terminal.perform_actions(parser.parse_as_vec(prelude));
            }
            let mut actions = Vec::new();
            let mut total = 0;
            let start = Instant::now();
            for piece in data.chunks(chunk) {
                parser.parse(piece, |action| actions.push(action));
                total += actions.len();
                // The mux parse thread hands each batch over with `mem::take`,
                // so every batch grows a fresh Vec, as here.
                terminal.perform_actions(std::mem::take(&mut actions));
            }
            let secs = start.elapsed().as_secs_f64();
            LaneRun {
                lane,
                bytes: data.len(),
                prelude_bytes: prelude.len(),
                secs,
                actions: Some(count(total)),
                terminal: Some(terminal),
            }
        }
    }
}

/// End-state facts used for the sanity checks and for cross-lane comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GridSummary {
    pub physical_rows: usize,
    pub physical_cols: usize,
    pub cursor_x: usize,
    pub cursor_y: i64,
    /// Rows held in memory: the viewport plus scrollback.
    pub retained_rows: usize,
    pub nonblank_visible_cells: usize,
    /// Hash of every retained row's text and every visible cell's colors.
    pub fingerprint: u64,
}

pub fn summarize(terminal: &Terminal) -> GridSummary {
    let screen = terminal.screen();
    let retained_rows = screen.scrollback_rows();
    let visible_start = retained_rows.saturating_sub(screen.physical_rows);
    let mut hasher = DefaultHasher::new();
    let mut nonblank_visible_cells = 0;
    screen.with_phys_lines(0..retained_rows, |lines| {
        for (idx, line) in lines.iter().enumerate() {
            line.as_str().hash(&mut hasher);
            if idx < visible_start {
                continue;
            }
            for cell in line.visible_cells() {
                if !cell.str().trim().is_empty() {
                    nonblank_visible_cells += 1;
                }
                cell.attrs().foreground().hash(&mut hasher);
                cell.attrs().background().hash(&mut hasher);
            }
        }
    });
    let cursor = terminal.cursor_pos();
    GridSummary {
        physical_rows: screen.physical_rows,
        physical_cols: screen.physical_cols,
        cursor_x: cursor.x,
        cursor_y: cursor.y,
        retained_rows,
        nonblank_visible_cells,
        fingerprint: hasher.finish(),
    }
}

/// What a lane's final state must satisfy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expectation {
    pub min_retained_rows: usize,
    /// At least one visible cell must hold text.
    pub requires_visible_text: bool,
}

impl Expectation {
    /// For a generated corpus. A `line_oriented` corpus moves the cursor only
    /// by printing, CR and LF, so every LF after the first `rows - 1` scrolls
    /// a row into scrollback (wrapping only adds more), up to the scrollback
    /// size.
    pub fn for_generated(data: &[u8], geometry: &Geometry, line_oriented: bool) -> Self {
        let min_retained_rows = if line_oriented {
            let line_feeds = data.iter().filter(|&&byte| byte == b'\n').count();
            let scrolled = line_feeds.saturating_sub(geometry.rows.saturating_sub(1));
            geometry.rows + scrolled.min(geometry.scrollback)
        } else {
            geometry.rows
        };
        Self {
            min_retained_rows,
            requires_visible_text: !data.is_empty(),
        }
    }

    /// For an arbitrary file, whose cursor movement is unknown. A non-empty
    /// file must still leave visible text, so a file that ends by clearing the
    /// screen fails the check by design.
    pub fn for_file(data: &[u8], geometry: &Geometry) -> Self {
        Self {
            min_retained_rows: geometry.rows,
            requires_visible_text: !data.is_empty(),
        }
    }
}

/// Summarizes a lane's final terminal and checks it. The `parse` lane has no
/// terminal; it only has to produce actions from non-empty input.
pub fn evaluate(
    run: &LaneRun,
    geometry: &Geometry,
    expectation: &Expectation,
) -> (Option<GridSummary>, Result<(), String>) {
    match &run.terminal {
        Some(terminal) => {
            let summary = summarize(terminal);
            let verdict = check_invariants(&summary, geometry, expectation);
            (Some(summary), verdict)
        }
        None if run.bytes > 0 && run.actions == Some(0) => {
            (None, Err("the parser produced no actions".to_string()))
        }
        None => (None, Ok(())),
    }
}

pub fn check_invariants(
    summary: &GridSummary,
    geometry: &Geometry,
    expectation: &Expectation,
) -> Result<(), String> {
    if summary.physical_rows != geometry.rows || summary.physical_cols != geometry.cols {
        return Err(format!(
            "screen is {}x{}, expected {}x{}",
            summary.physical_rows, summary.physical_cols, geometry.rows, geometry.cols
        ));
    }
    if summary.cursor_x >= geometry.cols {
        return Err(format!(
            "cursor x {} is outside {} columns",
            summary.cursor_x, geometry.cols
        ));
    }
    let rows = i64::try_from(geometry.rows).expect("row count fits in i64");
    if summary.cursor_y < 0 || summary.cursor_y >= rows {
        return Err(format!(
            "cursor y {} is outside {} rows",
            summary.cursor_y, geometry.rows
        ));
    }
    let max_rows = geometry.rows + geometry.scrollback;
    if summary.retained_rows < expectation.min_retained_rows || summary.retained_rows > max_rows {
        return Err(format!(
            "{} rows retained, expected {}..={}",
            summary.retained_rows, expectation.min_retained_rows, max_rows
        ));
    }
    if expectation.requires_visible_text && summary.nonblank_visible_cells == 0 {
        return Err("no visible text: the lane never reached the grid".to_string());
    }
    Ok(())
}
