//! Headless FrankenTerm throughput driver (profiling-only, out of tree).
//!
//! Usage: ftbench <mode> <file>
//!   mode = parse   : escape-parser only (Parser::parse -> Vec<Action>, cleared per chunk)
//!   mode = term    : Terminal::advance_bytes per chunk (single-stage)
//!   mode = mux     : parse chunk into Vec<Action>, then Terminal::perform_actions (the
//!                    two-stage path the mux parse thread uses)
//! Env: ROWS (80) COLS (120) SCROLLBACK (3500) CHUNK (131072) CONFIG=min|prodlike
//! Processes the file once per process so hyperfine compares whole runs, like ghostty-bench.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use frankenterm_escape_parser::parser::Parser;
use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Terminal, TerminalConfiguration, TerminalSize};

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[derive(Debug)]
struct MinConfig {
    scrollback: usize,
}

impl TerminalConfiguration for MinConfig {
    fn scrollback_size(&self) -> usize {
        self.scrollback
    }
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

/// Mimics the GUI's `TermConfig::configuration()`: every config read locks a mutex and
/// clones an `Arc` shared with other threads.
#[derive(Debug)]
struct Shared {
    scrollback: usize,
    max_title: usize,
}

#[derive(Debug)]
struct ProdLikeConfig {
    inner: Mutex<Option<Arc<Shared>>>,
}

impl ProdLikeConfig {
    fn cfg(&self) -> Arc<Shared> {
        self.inner.lock().unwrap().as_ref().cloned().unwrap()
    }
}

impl TerminalConfiguration for ProdLikeConfig {
    fn scrollback_size(&self) -> usize {
        self.cfg().scrollback
    }
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
    fn max_accumulating_title_len(&self) -> usize {
        self.cfg().max_title
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: ftbench <parse|term|mux> <file>");
        std::process::exit(2);
    }
    let mode = args[1].as_str();
    let data = std::fs::read(&args[2]).expect("read input");
    let rows = env_usize("ROWS", 80);
    let cols = env_usize("COLS", 120);
    let scrollback = env_usize("SCROLLBACK", 3500);
    let chunk = env_usize("CHUNK", 128 * 1024).max(1);
    let config_kind = std::env::var("CONFIG").unwrap_or_else(|_| "min".to_string());

    let config: Arc<dyn TerminalConfiguration + Send + Sync> = if config_kind == "prodlike" {
        Arc::new(ProdLikeConfig {
            inner: Mutex::new(Some(Arc::new(Shared { scrollback, max_title: 8192 }))),
        })
    } else {
        Arc::new(MinConfig { scrollback })
    };

    let mut terminal = Terminal::new(
        TerminalSize { rows, cols, pixel_width: cols * 8, pixel_height: rows * 16, dpi: 96 },
        config,
        "ftbench",
        "0",
        Box::new(Vec::new()),
    );

    let start = Instant::now();
    let mut actions_total: usize = 0;
    match mode {
        "parse" => {
            let mut parser = Parser::new();
            let mut actions = Vec::with_capacity(chunk / 4);
            for piece in data.chunks(chunk) {
                parser.parse(piece, |a| actions.push(a));
                actions_total += actions.len();
                actions.clear();
            }
        }
        "term" => {
            for piece in data.chunks(chunk) {
                terminal.advance_bytes(piece);
            }
        }
        "mux" => {
            let mut parser = Parser::new();
            for piece in data.chunks(chunk) {
                let mut actions = Vec::with_capacity(chunk / 4);
                parser.parse(piece, |a| actions.push(a));
                actions_total += actions.len();
                terminal.perform_actions(actions);
            }
        }
        other => {
            eprintln!("unknown mode {other}");
            std::process::exit(2);
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let mb = data.len() as f64 / (1024.0 * 1024.0);
    let cursor = terminal.cursor_pos();
    eprintln!(
        "ftbench mode={mode} config={config_kind} rows={rows} cols={cols} scrollback={scrollback} chunk={chunk} bytes={} actions={actions_total} secs={elapsed:.3} MiB/s={:.1} cursor=({},{})",
        data.len(),
        mb / elapsed,
        cursor.x,
        cursor.y
    );
}
