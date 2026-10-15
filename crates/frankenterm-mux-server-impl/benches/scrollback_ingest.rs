//! Parser-side cost of the durable scrollback store on line output
//! (ft-yccm0.2.1.8).
//!
//! Feeds line-oriented output through `Terminal::advance_bytes`, as the
//! parse thread applies a pane's output, so every line past the viewport
//! scrolls into scrollback, and times it in three arms, ABBA-interleaved in
//! one process, each run on a fresh terminal:
//!
//! - `store`: the default configuration with the production deferred sink
//!   (`open_scrollback_spill_sink` in a fresh temporary directory, the
//!   process-wide durability writer thread), as a mux-server or GUI local
//!   pane has it;
//! - `no_sink`: the same configuration with no store (tiers on, as a pane
//!   whose store could not open);
//! - `untiered`: `scrollback_tiered_enabled = false`.
//!
//! After each chunk the store arm does what `LocalPane` does after a parse
//! batch (`drain_scrollback_outside_terminal`, without its lock handoffs or
//! capacity wait): it trims hot-tier overflow into the sink's queue and
//! hands the queue to the durability writer. Without that handoff the
//! writer never drains, the queue reaches its budget a few hundred thousand
//! rows in, and every later row takes the overload (gap) path; each run
//! prints the gap rows it caused.
//!
//! Lines are `seq`-like (`1\r\n2\r\n...`, as a pty delivers `seq 1 N`) or,
//! with a width, each row's number padded with `x` to that many columns.
//! Each run's store directory is kept and printed (no deletion here).
//!
//! usage: cargo bench -p frankenterm-mux-server-impl --profile release-perf \
//!            --bench scrollback_ingest -- [ROWS] [ROUNDS] [WIDTH]
//! defaults: 2000000 rows, 4 rounds, width 0 (seq-like); 120x40 terminal.

use std::sync::Arc;
use std::time::Instant;

use frankenterm_alloc::resource_ledger::DurabilityLedger;
use wezterm_term::config::ScrollbackSpillSink;
use wezterm_term::{DeferredScrollbackTrim, Terminal, TerminalConfiguration, TerminalSize};

const ROWS: usize = 40;
const COLS: usize = 120;
/// Bytes per `advance_bytes` call, about one pty read batch.
const CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arm {
    Store,
    NoSink,
    Untiered,
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Arm::Store => "store",
            Arm::NoSink => "no_sink",
            Arm::Untiered => "untiered",
        }
    }
}

fn corpus(rows: usize, width: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(rows * (width.max(8) + 2));
    for row in 1..=rows {
        let number = row.to_string();
        out.extend_from_slice(number.as_bytes());
        out.resize(out.len() + width.saturating_sub(number.len()), b'x');
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// `LocalPane::drain_scrollback_outside_terminal` after a parse batch, minus
/// its lock handoffs and its wait for queue capacity.
fn drain_like_a_pane(terminal: &mut Terminal, sink: &Arc<dyn ScrollbackSpillSink>) {
    while matches!(
        terminal.trim_deferred_scrollback(),
        DeferredScrollbackTrim::Yielded
    ) {}
    // Errors are counted by the sink; the bench only needs the handoff.
    let _ = sink.request_scrollback_flush();
}

struct RunResult {
    secs: f64,
    kept: Option<std::path::PathBuf>,
    gap_rows: u64,
}

fn run(arm: Arm, data: &[u8], serial: u8) -> RunResult {
    let mut kept = None;
    let mut handoff: Option<Arc<dyn ScrollbackSpillSink>> = None;
    let term_config: Arc<dyn TerminalConfiguration + Send + Sync> = match arm {
        Arm::Store => {
            config::use_this_configuration(config::Config::default_config());
            let dir = tempfile::Builder::new()
                .prefix("ft-scrollback-ingest-")
                .tempdir()
                .expect("store directory")
                .keep();
            let context = config::ScrollbackSpillSinkContext {
                pane_id: usize::from(serial),
                domain_id: 0,
                durable_pane_id: [serial; 16],
                command_description: "scrollback-ingest-bench".to_string(),
            };
            let sink =
                frankenterm_mux_server_impl::open_scrollback_spill_sink(dir.clone(), &context)
                    .expect("open the durable scrollback store");
            kept = Some(dir);
            handoff = Some(Arc::clone(&sink));
            Arc::new(config::TermConfig::with_config_and_scrollback_spill_sink(
                config::configuration(),
                Some(sink),
            ))
        }
        Arm::NoSink => {
            config::use_this_configuration(config::Config::default_config());
            Arc::new(config::TermConfig::with_config(config::configuration()))
        }
        Arm::Untiered => {
            let mut untiered = config::Config::default_config();
            untiered.scrollback_tiered_enabled = false;
            config::use_this_configuration(untiered);
            Arc::new(config::TermConfig::with_config(config::configuration()))
        }
    };
    let mut terminal = Terminal::new(
        TerminalSize {
            rows: ROWS,
            cols: COLS,
            pixel_width: COLS * 8,
            pixel_height: ROWS * 16,
            dpi: 96,
        },
        term_config,
        "FrankenTerm",
        "scrollback-ingest-bench",
        Box::new(std::io::sink()),
    );
    let gaps_before = DurabilityLedger::global().snapshot().gap_rows_total;
    let start = Instant::now();
    for chunk in data.chunks(CHUNK) {
        terminal.advance_bytes(chunk);
        if let Some(sink) = &handoff {
            drain_like_a_pane(&mut terminal, sink);
        }
    }
    let secs = start.elapsed().as_secs_f64();
    RunResult {
        secs,
        kept,
        gap_rows: DurabilityLedger::global().snapshot().gap_rows_total - gaps_before,
    }
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values[mid]
    } else {
        values[mid - 1].midpoint(values[mid])
    }
}

fn main() {
    // `cargo bench` passes `--bench`; anything else numeric is ours.
    let numbers: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|arg| arg.parse().ok())
        .collect();
    let rows = numbers.first().copied().unwrap_or(2_000_000);
    let rounds = numbers.get(1).copied().unwrap_or(4).max(1);
    let width = numbers.get(2).copied().unwrap_or(0);
    let data = corpus(rows, width);
    let arms = [Arm::Store, Arm::NoSink, Arm::Untiered];
    let mut secs = vec![Vec::new(); arms.len()];
    let mut serial = 0u8;
    for round in 0..rounds {
        let order: Vec<usize> = if round % 2 == 0 {
            (0..arms.len()).collect()
        } else {
            (0..arms.len()).rev().collect()
        };
        for index in order {
            serial = serial.wrapping_add(1);
            let result = run(arms[index], &data, serial);
            eprintln!(
                "round {round} {:>8}: {:.3} s{}",
                arms[index].name(),
                result.secs,
                result
                    .kept
                    .map(|dir| format!(" ({} gap rows; store {})", result.gap_rows, dir.display()))
                    .unwrap_or_default()
            );
            secs[index].push(result.secs);
        }
    }
    let base = median(&mut secs[2].clone());
    for (arm, values) in arms.iter().zip(secs.iter_mut()) {
        let med = median(values);
        println!(
            "scrollback_ingest rows={rows} width={width} {:>8}: median {med:.3} s, {:.0} rows/s, {:.3}x untiered",
            arm.name(),
            rows as f64 / med,
            med / base
        );
    }
}
