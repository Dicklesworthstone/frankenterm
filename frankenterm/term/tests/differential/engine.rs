//! The engines the harness compares, behind one trait.

use std::sync::Arc;

use frankenterm_escape_parser::parser::Parser;
use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Terminal, TerminalConfiguration, TerminalSize};

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
}

pub trait EngineFactory {
    fn name(&self) -> &'static str;
    fn build(&self, geometry: &Geometry) -> Box<dyn Engine>;
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
    Terminal::new(
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
        Box::new(std::io::sink()),
    )
}

/// The oracle: `Terminal::advance_bytes`, the fused single-stage path.
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
}

impl EngineFactory for Legacy {
    fn name(&self) -> &'static str {
        "legacy"
    }

    fn build(&self, geometry: &Geometry) -> Box<dyn Engine> {
        Box::new(LegacyEngine {
            terminal: new_terminal(geometry),
        })
    }
}

/// The mux parse thread's shape: `Parser::parse` into a `Vec<Action>`, then
/// `Terminal::perform_actions`, with or without the parser's ground-state
/// printable-run batching.
pub struct TwoStage {
    pub print_batching: bool,
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
}

impl EngineFactory for TwoStage {
    fn name(&self) -> &'static str {
        if self.print_batching {
            "mux_two_stage"
        } else {
            "two_stage_unbatched"
        }
    }

    fn build(&self, geometry: &Geometry) -> Box<dyn Engine> {
        let mut parser = Parser::new();
        parser.set_print_batching(self.print_batching);
        Box::new(TwoStageEngine {
            terminal: new_terminal(geometry),
            parser,
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
        }),
        Box::new(TwoStage {
            print_batching: false,
        }),
    ]
}
