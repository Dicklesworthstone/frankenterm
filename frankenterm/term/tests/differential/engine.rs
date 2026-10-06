//! The engines the harness compares, behind one trait.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use frankenterm_escape_parser::parser::{AsciiScan, Parser};
use frankenterm_escape_parser::{Action, ControlCode, Esc, EscCode};
use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Clipboard, FeedGate, Terminal, TerminalConfiguration, TerminalSize};

use super::snapshot::{self, EngineSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub rows: usize,
    pub cols: usize,
    pub scrollback: usize,
}

/// Geometries the runners cycle through: a tiny screen that wraps and
/// scrolls constantly, a small one, and the classic 24x80.
pub const GEOMETRIES: [Geometry; 3] = [
    Geometry {
        rows: 5,
        cols: 9,
        scrollback: 8,
    },
    Geometry {
        rows: 12,
        cols: 40,
        scrollback: 16,
    },
    Geometry {
        rows: 24,
        cols: 80,
        scrollback: 32,
    },
];

pub trait Engine {
    /// Feeds one chunk, as one read from the PTY would deliver it.
    fn feed(&mut self, bytes: &[u8]);
    /// The normalized, comparable state after everything fed so far.
    fn snapshot(&self) -> EngineSnapshot;
    /// Blocks until every reply queued so far has reached the engine's
    /// writer. Terminals write replies on a background thread.
    fn wait_for_replies(&mut self);
}

/// Where an engine's terminal sends replies to queries (the PTY input side)
/// and OSC 52 clipboard writes.
pub struct EngineIo {
    pub writer: Box<dyn Write + Send>,
    pub clipboard: Option<Arc<dyn Clipboard>>,
}

impl EngineIo {
    /// Discards replies and has no clipboard.
    pub fn sink() -> Self {
        EngineIo {
            writer: Box::new(std::io::sink()),
            clipboard: None,
        }
    }
}

pub trait EngineFactory {
    fn name(&self) -> &'static str;
    /// A fresh engine whose terminal writes to `io`.
    fn build_with(&self, geometry: &Geometry, io: EngineIo) -> Box<dyn Engine>;

    fn build(&self, geometry: &Geometry) -> Box<dyn Engine> {
        self.build_with(geometry, EngineIo::sink())
    }
}

#[derive(Debug)]
struct HarnessConfig {
    scrollback: usize,
}

impl TerminalConfiguration for HarnessConfig {
    fn scrollback_size(&self) -> usize {
        self.scrollback
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

pub fn new_terminal(geometry: &Geometry) -> Terminal {
    new_terminal_with(geometry, EngineIo::sink())
}

pub fn new_terminal_with(geometry: &Geometry, io: EngineIo) -> Terminal {
    let mut terminal = Terminal::new(
        TerminalSize {
            rows: geometry.rows,
            cols: geometry.cols,
            pixel_width: geometry.cols * 8,
            pixel_height: geometry.rows * 16,
            dpi: 96,
        },
        Arc::new(HarnessConfig {
            scrollback: geometry.scrollback,
        }),
        "frankenterm-differential",
        "0",
        io.writer,
    );
    if let Some(clipboard) = io.clipboard {
        terminal.set_clipboard(&clipboard);
    }
    terminal
}

/// Waits for the terminal's writer thread to write every queued reply.
pub fn drain_replies(terminal: &mut Terminal) {
    terminal
        .writer_barrier()
        .wait(Duration::from_secs(10))
        .expect("the terminal writer drains within 10 s");
}

/// The oracle: `Terminal::advance_bytes`, the fused single-stage path. Since
/// ft-yccm0.3.2.1 its parser drives the performer through `Handler`, with no
/// `Action` values in between; the two-stage candidates below keep the
/// `Action` path, so every check compares the two.
pub struct Legacy;

struct LegacyEngine {
    terminal: Terminal,
}

impl Engine for LegacyEngine {
    fn feed(&mut self, bytes: &[u8]) {
        self.terminal.advance_bytes(bytes);
    }

    fn snapshot(&self) -> EngineSnapshot {
        snapshot::capture(&self.terminal)
    }

    fn wait_for_replies(&mut self) {
        drain_replies(&mut self.terminal);
    }
}

impl EngineFactory for Legacy {
    fn name(&self) -> &'static str {
        "legacy"
    }

    fn build_with(&self, geometry: &Geometry, io: EngineIo) -> Box<dyn Engine> {
        Box::new(LegacyEngine {
            terminal: new_terminal_with(geometry, io),
        })
    }
}

/// The mux parse thread's shape: `Parser::parse` into a `Vec<Action>`, then
/// `Terminal::perform_actions`, with or without the parser's ground-state
/// printable-run batching, and with or without its CSI fast path
/// (ft-yccm0.3.2.4), which runs only with batching. Unbatched, every byte
/// goes through the state machine and every CSI through `CSI::parse`.
pub struct TwoStage {
    pub print_batching: bool,
    pub csi_fast_path: bool,
}

struct TwoStageEngine {
    terminal: Terminal,
    parser: Parser,
}

impl Engine for TwoStageEngine {
    fn feed(&mut self, bytes: &[u8]) {
        let mut actions = Vec::new();
        self.parser.parse(bytes, |action| actions.push(action));
        self.terminal.perform_actions(actions);
    }

    fn snapshot(&self) -> EngineSnapshot {
        snapshot::capture(&self.terminal)
    }

    fn wait_for_replies(&mut self) {
        drain_replies(&mut self.terminal);
    }
}

impl EngineFactory for TwoStage {
    fn name(&self) -> &'static str {
        match (self.print_batching, self.csi_fast_path) {
            (true, true) => "mux_two_stage",
            (true, false) => "two_stage_no_csi_fast_path",
            (false, _) => "two_stage_unbatched",
        }
    }

    fn build_with(&self, geometry: &Geometry, io: EngineIo) -> Box<dyn Engine> {
        let mut parser = Parser::new();
        parser.set_print_batching(self.print_batching);
        parser.set_csi_fast_path(self.csi_fast_path);
        Box::new(TwoStageEngine {
            terminal: new_terminal_with(geometry, io),
            parser,
        })
    }
}

/// The mux's fused path (ft-yccm0.3.2.1): an external parser feeds the
/// terminal through `Terminal::feed`, and the actions the gate diverts are
/// applied afterwards with `perform_actions`, as the mux applies them after
/// admission. The gate diverts what the mux diverts (alert sources) and,
/// with `divert_every`, also every n-th other action, so diversion lands at
/// arbitrary points. `ascii_scan` pins the parser's ground-state ASCII scan
/// (ft-yccm0.3.2.2); `None` keeps the parser's default.
pub struct FusedFeed {
    pub divert_every: Option<usize>,
    pub ascii_scan: Option<AsciiScan>,
}

struct FusedFeedGate {
    divert_every: Option<usize>,
    seen: usize,
}

impl FeedGate for FusedFeedGate {
    fn diverts(&mut self, action: &Action) -> bool {
        self.seen += 1;
        let alert_source = matches!(
            action,
            Action::Control(ControlCode::Bell)
                | Action::OperatingSystemCommand(_)
                | Action::KittyImage(_)
                | Action::Esc(Esc::Code(EscCode::StringTerminator | EscCode::FullReset))
        );
        alert_source
            || self
                .divert_every
                .is_some_and(|n| self.seen.is_multiple_of(n))
    }
}

struct FusedFeedEngine {
    terminal: Terminal,
    parser: Parser,
    gate: FusedFeedGate,
}

impl Engine for FusedFeedEngine {
    fn feed(&mut self, bytes: &[u8]) {
        let mut diverted = Vec::new();
        self.terminal
            .feed(&mut self.parser, bytes, &mut self.gate, &mut diverted);
        self.terminal.perform_actions(diverted);
    }

    fn snapshot(&self) -> EngineSnapshot {
        snapshot::capture(&self.terminal)
    }

    fn wait_for_replies(&mut self) {
        drain_replies(&mut self.terminal);
    }
}

impl EngineFactory for FusedFeed {
    fn name(&self) -> &'static str {
        match (self.divert_every, self.ascii_scan) {
            (None, None) => "fused_feed",
            (Some(_), None) => "fused_feed_frequent_diversion",
            (None, Some(AsciiScan::Scalar)) => "fused_feed_scalar_scan",
            (None, Some(AsciiScan::Simd16)) => "fused_feed_simd16",
            (None, Some(AsciiScan::Simd32)) => "fused_feed_simd32",
            (None, Some(AsciiScan::Simd64)) => "fused_feed_simd64",
            (Some(_), Some(_)) => "fused_feed_frequent_diversion_pinned_scan",
        }
    }

    fn build_with(&self, geometry: &Geometry, io: EngineIo) -> Box<dyn Engine> {
        let mut parser = Parser::new();
        if let Some(scan) = self.ascii_scan {
            parser.set_ascii_scan(scan);
        }
        Box::new(FusedFeedEngine {
            terminal: new_terminal_with(geometry, io),
            parser,
            gate: FusedFeedGate {
                divert_every: self.divert_every,
                seen: 0,
            },
        })
    }
}

/// Every candidate checked against [`Legacy`]. The fused parser (B2), the
/// SIMD scanner (B2.2/B2.3) and the page grid (B3) register here when they
/// land; nothing else in the harness changes.
pub fn candidates() -> Vec<Box<dyn EngineFactory>> {
    vec![
        Box::new(TwoStage {
            print_batching: true,
            csi_fast_path: true,
        }),
        Box::new(TwoStage {
            print_batching: true,
            csi_fast_path: false,
        }),
        Box::new(TwoStage {
            print_batching: false,
            csi_fast_path: false,
        }),
        Box::new(FusedFeed {
            divert_every: None,
            ascii_scan: None,
        }),
        Box::new(FusedFeed {
            divert_every: Some(7),
            ascii_scan: None,
        }),
        // ft-yccm0.3.2.2: the scalar oracle and the wider `std::simd` scans.
        // Legacy and the other candidates run the default scan.
        Box::new(FusedFeed {
            divert_every: None,
            ascii_scan: Some(AsciiScan::Scalar),
        }),
        Box::new(FusedFeed {
            divert_every: None,
            ascii_scan: Some(AsciiScan::Simd32),
        }),
        Box::new(FusedFeed {
            divert_every: None,
            ascii_scan: Some(AsciiScan::Simd64),
        }),
    ]
}
