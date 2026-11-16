//! The normalized, comparable view of an engine's state.
//!
//! Seqnos are left out on purpose: they count feed calls, which engines may
//! legitimately count differently (an empty action batch does not bump it).

use std::fmt::Write as _;

use frankenterm_term::{CellAttributes, CursorPosition, Line, Terminal};

/// Scrollback rows above the viewport included in each snapshot.
pub const SCROLLBACK_WINDOW: usize = 8;

#[derive(Clone, Debug, PartialEq)]
pub struct EngineSnapshot {
    /// Position, shape and visibility; the seqno is zeroed.
    pub cursor: CursorPosition,
    pub modes: Vec<(&'static str, String)>,
    /// Rows held in memory: the viewport plus scrollback.
    pub retained_rows: usize,
    /// The last `physical_rows + SCROLLBACK_WINDOW` rows, oldest first.
    pub rows: Vec<RowSnapshot>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RowSnapshot {
    pub wrapped: bool,
    /// Double width, double height (top, bottom) and the bidi settings.
    pub line_bits: String,
    /// Visible cells as runs of equal attributes. Trailing blanks with
    /// default attributes are dropped: they render like absent cells.
    pub runs: Vec<CellRun>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CellRun {
    /// Column of the run's first cell.
    pub start: usize,
    /// The cells' graphemes, concatenated.
    pub text: String,
    /// One width per visible cell, so a combining cluster stays one cell.
    pub widths: Vec<u8>,
    pub attrs: CellAttributes,
}

pub fn capture(terminal: &Terminal) -> EngineSnapshot {
    let screen = terminal.screen();
    let retained_rows = screen.scrollback_rows();
    let first = retained_rows.saturating_sub(screen.physical_rows + SCROLLBACK_WINDOW);
    let mut rows = Vec::with_capacity(retained_rows - first);
    screen.with_phys_lines(first..retained_rows, |lines| {
        for line in lines.iter() {
            rows.push(row_snapshot(line));
        }
    });
    let mut cursor = terminal.cursor_pos();
    cursor.seqno = 0;
    EngineSnapshot {
        cursor,
        modes: terminal.mode_snapshot(),
        retained_rows,
        rows,
    }
}

fn row_snapshot(line: &Line) -> RowSnapshot {
    let mut runs: Vec<CellRun> = Vec::new();
    let mut run_end = 0;
    for cell in line.visible_cells() {
        let index = cell.cell_index();
        let width = cell.width() as u8;
        let extends_last = match runs.last() {
            Some(run) => run_end == index && run.attrs == *cell.attrs(),
            None => false,
        };
        if extends_last {
            let run = runs.last_mut().expect("checked above");
            run.text.push_str(cell.str());
            run.widths.push(width);
        } else {
            runs.push(CellRun {
                start: index,
                text: cell.str().to_string(),
                widths: vec![width],
                attrs: cell.attrs().clone(),
            });
        }
        run_end = index + cell.width();
    }
    trim_trailing_blanks(&mut runs);
    let (bidi_enabled, bidi_hint) = line.bidi_info();
    RowSnapshot {
        wrapped: line.last_cell_was_wrapped(),
        line_bits: format!(
            "double_width={} double_height_top={} double_height_bottom={} bidi={}/{:?}",
            line.is_double_width(),
            line.is_double_height_top(),
            line.is_double_height_bottom(),
            bidi_enabled,
            bidi_hint
        ),
        runs,
    }
}

fn trim_trailing_blanks(runs: &mut Vec<CellRun>) {
    let default_attrs = CellAttributes::default();
    while let Some(run) = runs.last_mut() {
        if run.attrs != default_attrs {
            return;
        }
        while run.text.ends_with(' ') && run.widths.last() == Some(&1) {
            run.text.pop();
            run.widths.pop();
        }
        if !run.widths.is_empty() {
            return;
        }
        runs.pop();
    }
}

fn row_text(row: &RowSnapshot) -> String {
    let mut text = String::new();
    let mut column = 0;
    for run in &row.runs {
        while column < run.start {
            text.push(' ');
            column += 1;
        }
        text.push_str(&run.text);
        column = run.start + run.widths.iter().map(|&w| usize::from(w)).sum::<usize>();
    }
    text
}

/// A human-readable account of how `candidate` differs from `oracle`, or
/// `None` when they are equal.
pub fn describe_difference(oracle: &EngineSnapshot, candidate: &EngineSnapshot) -> Option<String> {
    if oracle == candidate {
        return None;
    }
    let mut out = String::new();
    if oracle.cursor != candidate.cursor {
        let _ = writeln!(
            out,
            "cursor: oracle {:?} | candidate {:?}",
            oracle.cursor, candidate.cursor
        );
    }
    for (oracle_mode, candidate_mode) in oracle.modes.iter().zip(candidate.modes.iter()) {
        if oracle_mode != candidate_mode {
            let _ = writeln!(
                out,
                "mode {}: oracle {} | candidate {}",
                oracle_mode.0, oracle_mode.1, candidate_mode.1
            );
        }
    }
    if oracle.modes.len() != candidate.modes.len() {
        let _ = writeln!(
            out,
            "mode list length: oracle {} | candidate {}",
            oracle.modes.len(),
            candidate.modes.len()
        );
    }
    if oracle.retained_rows != candidate.retained_rows {
        let _ = writeln!(
            out,
            "retained rows: oracle {} | candidate {}",
            oracle.retained_rows, candidate.retained_rows
        );
    }
    // Rows are aligned from the bottom of the screen.
    let depth = oracle.rows.len().max(candidate.rows.len());
    let mut reported = 0;
    for above_bottom in (0..depth).rev() {
        let oracle_row = oracle
            .rows
            .len()
            .checked_sub(above_bottom + 1)
            .and_then(|index| oracle.rows.get(index));
        let candidate_row = candidate
            .rows
            .len()
            .checked_sub(above_bottom + 1)
            .and_then(|index| candidate.rows.get(index));
        if oracle_row == candidate_row {
            continue;
        }
        reported += 1;
        if reported > 5 {
            let _ = writeln!(out, "(more rows differ)");
            break;
        }
        let _ = writeln!(out, "row {} above the bottom:", above_bottom);
        match (oracle_row, candidate_row) {
            (Some(a), Some(b)) => {
                let _ = writeln!(out, "  oracle    text {:?}", row_text(a));
                let _ = writeln!(out, "  candidate text {:?}", row_text(b));
                if a.wrapped != b.wrapped || a.line_bits != b.line_bits {
                    let _ = writeln!(
                        out,
                        "  wrapped/bits: oracle {} {} | candidate {} {}",
                        a.wrapped, a.line_bits, b.wrapped, b.line_bits
                    );
                }
                let first_run = a
                    .runs
                    .iter()
                    .zip(b.runs.iter())
                    .position(|(x, y)| x != y)
                    .unwrap_or_else(|| a.runs.len().min(b.runs.len()));
                let _ = writeln!(
                    out,
                    "  first differing run {}: oracle {:?} | candidate {:?}",
                    first_run,
                    a.runs.get(first_run),
                    b.runs.get(first_run)
                );
            }
            (a, b) => {
                let _ = writeln!(
                    out,
                    "  oracle {:?} | candidate {:?}",
                    a.map(row_text),
                    b.map(row_text)
                );
            }
        }
    }
    if out.is_empty() {
        let _ = writeln!(out, "snapshots differ: {:?} | {:?}", oracle, candidate);
    }
    Some(out)
}
