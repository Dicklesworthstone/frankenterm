//! The render mirror (ft-yccm0.4.4): a render-side copy of a pane's visible
//! rows as compact per-cell data, refreshed under one short terminal lock by
//! copying only the rows that changed since the previous capture.
//!
//! A [`MirrorCell`] holds what drawing the cell needs, unresolved:
//! - its text (a range of its row's text) and width;
//! - its foreground, background and underline colors as the cell names them
//!   (default, palette index, or 8-bit RGBA, which is lossless for 8-bit
//!   output because a true color always wins over its palette fallback);
//! - packed attribute flags and a hyperlink id.
//!
//! A [`MirrorRow`] also keeps its cells' images ([`MirrorImage`]): shared
//! handles to their payloads, with each cell's slice and placement.
//!
//! Style and color resolution, implicit hyperlink rules, selection, hyperlink
//! hover and GPU instance building happen after the lock is released. Native
//! rows remain packed while captured; already-authoritative legacy Lines are
//! copied only when their logical group changes. Change detection uses exact
//! source sequence numbers, including wrapped context outside the viewport.
//!
//! A row is recaptured when:
//! - its line's sequence number moved past the one it was captured at, or is
//!   `SEQ_ZERO`, which always counts as changed;
//! - it holds images, whose payloads change behind shared handles;
//! - it enters the viewport;
//! - it belongs to a logical line whose implicit hyperlinks were rescanned
//!   (a rescan can change a clean continuation row without a new seqno).
//!
//! Every row is recaptured when the screen's coordinate witness changes
//! (resize, reflow, scrollback erase, the other screen becoming active), the
//! alternate screen flips, the grid size or hyperlink rules change, or the
//! sequence number runs backwards. Ordinary output and scrolling keep the
//! witness, so a flood recaptures only the rows it writes.

use crate::pane::Pane;
use crate::renderable::{
    terminal_get_cursor_position, terminal_get_dimensions, RenderableDimensions,
    StableCursorPosition,
};
pub use frankenterm_term::render::{
    MirrorCell, MirrorColor, MirrorImage, MirrorLineMode, MirrorRow,
};
use frankenterm_term::render::{MirrorHyperlinkScanner, RenderRowRef, RenderRowSnapshot};
use frankenterm_term::screen::ScreenCoordinateWitness;
use frankenterm_term::{Line, StableRowIndex, Terminal};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::ops::Range;
use termwiz::hyperlink::Rule;
use termwiz::surface::{SequenceNo, SEQ_ZERO};

/// What a capture copied: the counters prove that a capture does work
/// proportional to the rows that changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureStats {
    /// Viewport rows checked against the mirror.
    pub rows_examined: usize,
    /// Rows copied into the mirror.
    pub rows_captured: usize,
    /// Cells copied into the mirror.
    pub cells_captured: usize,
    /// Rows copied as packed page cells and resolved after the terminal lock.
    pub packed_rows: usize,
    /// Logical groups whose hyperlink rules ran after source capture.
    pub hyperlink_scans: usize,
    /// Every row was copied (first capture, or an invalidation).
    pub full: bool,
}

/// The pane parameters of one capture.
#[derive(Debug, Clone, Copy)]
pub struct CaptureRequest<'a> {
    /// The first row of a scrolled-back viewport; `None` follows the live
    /// screen.
    pub viewport_top: Option<StableRowIndex>,
    /// Implicit hyperlink rules applied to the rows being copied.
    pub rules: &'a [Rule],
    /// Changes whenever `rules` does (the configuration generation).
    pub rules_generation: usize,
}

/// The source facts that determine one mirror's logical-line result. Terminal
/// scan bits are observations, never a substitute for this mirror's rule epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceRow {
    seqno: SequenceNo,
    len: usize,
    wrapped: bool,
    native: bool,
    scanned: bool,
    compress: bool,
}

impl SourceRow {
    fn from_row(row: &RenderRowRef<'_>) -> Self {
        Self {
            seqno: row.seqno(),
            len: row.len(),
            wrapped: row.last_cell_was_wrapped(),
            native: matches!(row, RenderRowRef::Page { .. }),
            scanned: row.implicit_hyperlinks_are_scanned(),
            compress: matches!(row, RenderRowRef::Page { compress: true, .. }),
        }
    }
}

/// A resolved group belongs to one mirror/rule epoch. Exact source facts cover
/// continuations outside the viewport and are pruned after every capture.
#[derive(Debug, Clone, Default)]
struct LogicalScan {
    end: StableRowIndex,
    source: Vec<SourceRow>,
    generation: u64,
}

#[derive(Debug, Clone)]
struct PendingGroup {
    stable: Range<StableRowIndex>,
    snapshots: Range<usize>,
    planted_skip: Option<StableRowIndex>,
}

/// The mirror of a pane's viewport, refreshed by [`capture_terminal_rows`].
#[derive(Debug, Clone, Default)]
pub struct RenderMirror {
    rows: Vec<MirrorRow>,
    /// Scratch damage flags retained across captures; never part of row data.
    dirty: Vec<bool>,
    /// Owned source snapshots reused across captures. Logical context can
    /// extend outside the viewport, but never retains unrelated scrollback.
    snapshots: Vec<RenderRowSnapshot>,
    snapshot_count: usize,
    pending: Vec<PendingGroup>,
    logical_scans: HashMap<StableRowIndex, LogicalScan>,
    scan_source: Vec<SourceRow>,
    resolved_rows: Vec<MirrorRow>,
    hyperlink_scanner: MirrorHyperlinkScanner,
    first: StableRowIndex,
    dimensions: Option<RenderableDimensions>,
    cursor: StableCursorPosition,
    seqno: SequenceNo,
    alt_screen: bool,
    rules_generation: Option<usize>,
    witness: Option<ScreenCoordinateWitness>,
    generation: u64,
    last: CaptureStats,
}

impl RenderMirror {
    pub fn new() -> Self {
        Self::default()
    }

    /// The viewport's rows, top first.
    pub fn rows(&self) -> &[MirrorRow] {
        &self.rows
    }

    /// The stable index of the viewport's first row.
    pub fn first(&self) -> StableRowIndex {
        self.first
    }

    /// The pane's dimensions at the last capture.
    pub fn dimensions(&self) -> Option<RenderableDimensions> {
        self.dimensions
    }

    /// The cursor at the last capture.
    pub fn cursor(&self) -> StableCursorPosition {
        self.cursor
    }

    /// Hides the cursor of the last capture (a tmux-controlled pane).
    pub fn hide_cursor(&mut self) {
        self.cursor.visibility = termwiz::surface::CursorVisibility::Hidden;
    }

    /// The terminal's sequence number at the last capture.
    pub fn seqno(&self) -> SequenceNo {
        self.seqno
    }

    /// Increments with every capture; rows record the one that copied them.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// What the last capture copied.
    pub fn last_capture(&self) -> CaptureStats {
        self.last
    }

    /// A mirror built from full copies of the viewport's lines, `lines[0]`
    /// being the row at `first`: the Line-based reference that incremental
    /// captures must match, and what a renderer that deep-clones every
    /// visible line each frame works from. Hyperlink rules are not applied.
    pub fn from_lines(
        dimensions: RenderableDimensions,
        cursor: StableCursorPosition,
        first: StableRowIndex,
        lines: &[Line],
    ) -> Self {
        let rows = lines
            .iter()
            .enumerate()
            .map(|(offset, line)| MirrorRow::from_line(first + offset as StableRowIndex, line, 1))
            .collect();
        Self {
            rows,
            first,
            dimensions: Some(dimensions),
            cursor,
            generation: 1,
            ..Self::default()
        }
    }

    /// Forgets every row, so the next capture copies all of them.
    pub fn invalidate(&mut self) {
        self.rows.clear();
        for row in &mut self.snapshots {
            row.clear();
        }
        self.snapshot_count = 0;
        self.pending.clear();
        self.logical_scans.clear();
        self.witness = None;
    }

    /// Finishes native capture once its caller has released the terminal
    /// lock. Only owned raw cells and referenced side entries are read here.
    pub(crate) fn resolve_pending(&mut self, rules: &[Rule]) {
        for group in self.pending.drain(..) {
            let snapshots = &mut self.snapshots[group.snapshots.clone()];
            if self.resolved_rows.len() < snapshots.len() {
                self.resolved_rows
                    .resize_with(snapshots.len(), || MirrorRow::empty(0, 0));
            }
            let resolved = &mut self.resolved_rows[..snapshots.len()];
            let native = snapshots.iter().all(RenderRowSnapshot::is_packed);
            if rules.is_empty() || native {
                for (offset, (source, row)) in snapshots.iter().zip(resolved.iter_mut()).enumerate()
                {
                    source.resolve(
                        row,
                        group.stable.start + offset as StableRowIndex,
                        self.generation,
                    );
                }
            }

            if rules.is_empty() {
                for row in resolved.iter_mut() {
                    row.clear_implicit_hyperlinks();
                }
            } else {
                self.last.hyperlink_scans += 1;
                if !native || !self.hyperlink_scanner.apply(resolved, rules) {
                    // Storage-sensitive Unicode concatenation, unusual widths
                    // and authoritative legacy edits use the existing scanner
                    // on owned Lines. None of these edits reaches the terminal.
                    let mut lines: Vec<_> = snapshots
                        .iter_mut()
                        .map(RenderRowSnapshot::take_line)
                        .collect();
                    for line in &mut lines {
                        line.invalidate_implicit_hyperlinks(line.current_seqno());
                    }
                    Line::apply_hyperlink_rules(rules, &mut lines.iter_mut().collect::<Vec<_>>());
                    for (offset, (line, row)) in lines.iter().zip(resolved.iter_mut()).enumerate() {
                        row.update_from_line(
                            group.stable.start + offset as StableRowIndex,
                            line,
                            self.generation,
                        );
                    }
                }
            }

            for (offset, row) in resolved.iter_mut().enumerate() {
                let stable = group.stable.start + offset as StableRowIndex;
                if let Ok(index) = usize::try_from(stable - self.first) {
                    if let Some(slot) = self.rows.get_mut(index) {
                        if group.planted_skip != Some(stable) {
                            std::mem::swap(slot, row);
                        } else if slot.stable() != stable {
                            slot.clear(stable, 0);
                        }
                    }
                }
                // Do not retain image or hyperlink owners from offscreen
                // context or the viewport row displaced by the swap.
                row.clear(0, 0);
            }
            for source in snapshots {
                source.clear();
            }
        }
        // Keep empty buffers across idle frames and one-row updates. Context
        // extends by at most one capped group on either side of the viewport.
        self.snapshots
            .truncate(self.rows.len().saturating_add(2 * MAX_LOGICAL_LINE_LEN));
        self.snapshot_count = 0;
    }

    /// The mirror's row at `stable`, if it holds one.
    fn row_index(&self, stable: StableRowIndex) -> Option<usize> {
        let offset = usize::try_from(stable.checked_sub(self.first)?).ok()?;
        (self.rows.get(offset)?.stable() == stable).then_some(offset)
    }

    /// Aligns the existing row storage with a new viewport. Scrolling rotates
    /// the owned rows, so overlap keeps both its contents and allocations;
    /// exposed slots retain reusable buffers but fail the stable-row check.
    /// With a stationary viewport this neither moves rows nor allocates.
    fn prepare_rows(&mut self, first: StableRowIndex, rows: usize) {
        if self.rows.len() == rows {
            if let Some(delta) = first.checked_sub(self.first) {
                if delta > 0 && (delta as usize) < rows {
                    self.rows.rotate_left(delta as usize);
                } else if delta < 0 && delta.unsigned_abs() < rows {
                    self.rows.rotate_right(delta.unsigned_abs());
                }
            }
        }
        self.rows.resize_with(rows, || MirrorRow::empty(0, 0));
        self.first = first;
        self.dirty.resize(rows, false);
    }
}

/// Refreshes `mirror` from `term`, which the caller has locked: copies only
/// the viewport rows that changed since the mirror's last capture. `None`
/// when the viewport reaches into cold (non-resident) scrollback, which this
/// capture never hydrates under the lock; use [`capture_pane_rows`] then.
pub fn capture_terminal_rows(
    term: &mut Terminal,
    mirror: &mut RenderMirror,
    request: &CaptureRequest<'_>,
) -> Option<CaptureStats> {
    capture_rows(term, mirror, request, None)
}

/// The lock-held phase used by `LocalPane`: native rows are copied as packed
/// cells, with only referenced style/grapheme/link/image entries. The caller
/// must release its terminal lock and call `RenderMirror::resolve_pending`
/// with the same rules before exposing the mirror to rendering. Legacy edited
/// rows copy their authoritative Line; all hyperlink mutations are render-side.
pub(crate) fn capture_terminal_rows_deferred(
    term: &mut Terminal,
    mirror: &mut RenderMirror,
    request: &CaptureRequest<'_>,
) -> Option<CaptureStats> {
    capture_rows_deferred(term, mirror, request, None)
}

/// [`capture_terminal_rows`], optionally failing to copy one changed row:
/// the planted fault that proves the equality tests can fail.
fn capture_rows(
    term: &mut Terminal,
    mirror: &mut RenderMirror,
    request: &CaptureRequest<'_>,
    planted_skip: Option<StableRowIndex>,
) -> Option<CaptureStats> {
    capture_rows_deferred(term, mirror, request, planted_skip)?;
    mirror.resolve_pending(request.rules);
    Some(mirror.last)
}

fn capture_rows_deferred(
    term: &mut Terminal,
    mirror: &mut RenderMirror,
    request: &CaptureRequest<'_>,
    planted_skip: Option<StableRowIndex>,
) -> Option<CaptureStats> {
    assert!(
        mirror.pending.is_empty(),
        "finish the preceding capture first"
    );
    let dimensions = terminal_get_dimensions(term);
    let seqno = term.current_seqno();
    let alt_screen = term.is_alt_screen_active();
    let witness_holds = mirror
        .witness
        .as_ref()
        .is_some_and(|witness| term.screen().matches_coordinate_witness(witness));
    let full = mirror.rows.is_empty()
        || !witness_holds
        || mirror
            .dimensions
            .map(|dims| (dims.cols, dims.viewport_rows))
            != Some((dimensions.cols, dimensions.viewport_rows))
        || mirror.alt_screen != alt_screen
        || mirror.rules_generation != Some(request.rules_generation)
        || seqno < mirror.seqno;

    let rows = dimensions.viewport_rows;
    let first = request
        .viewport_top
        .unwrap_or(dimensions.physical_top)
        .max(dimensions.scrollback_top)
        .min(dimensions.physical_top);
    let end = first.checked_add(StableRowIndex::try_from(rows).ok()?)?;
    if first < term.screen().phys_to_stable_row_index(0) {
        return None;
    }

    // Keep the row and damage tables in place. In a large pane layout even
    // unchanged panes are captured when another pane wakes the window.
    mirror.prepare_rows(first, rows);
    // Missing screen rows must become blanks rather than retaining an exposed
    // slot's old contents; only a matched clean row clears its flag.
    mirror.dirty.fill(true);
    // Which viewport rows need a copy, by seqno, membership and images.
    if !full {
        let screen = term.screen();
        let phys = screen.stable_range(&(first..end));
        for physical in phys {
            if let Some(line) = screen.render_row(physical) {
                let stable = screen.phys_to_stable_row_index(physical);
                let Some(row) = usize::try_from(stable - first).ok() else {
                    continue;
                };
                let Some(flag) = mirror.dirty.get_mut(row) else {
                    continue;
                };
                *flag = match mirror.rows.get(row) {
                    Some(held) if held.stable() == stable => {
                        let current = line.seqno();
                        current == SEQ_ZERO || current > held.seqno() || held.always_recapture()
                    }
                    _ => true,
                };
            }
        }
    }

    let generation = mirror.generation.wrapping_add(1);
    let mut stats = CaptureStats {
        rows_examined: rows,
        full,
        ..CaptureStats::default()
    };
    if request.rules.is_empty() {
        mirror.logical_scans.clear();
        let screen = term.screen();
        let phys = screen.stable_range(&(first..end));
        for physical in phys {
            let stable = screen.phys_to_stable_row_index(physical);
            let Ok(index) = usize::try_from(stable - first) else {
                continue;
            };
            if mirror.dirty.get(index) != Some(&true) {
                continue;
            }
            capture_group(screen, stable..stable + 1, mirror, &mut stats, planted_skip);
        }
    } else {
        if full {
            mirror.logical_scans.clear();
        }
        let screen = term.screen();
        // One whole-viewport traversal matches Screen's bounded grouping.
        // Rewalking backwards separately for each cap-split group can produce
        // different overlaps, so each group is captured and resolved once.
        for logical in logical_line_groups(screen, first..end) {
            let visible = logical.start.max(first)..logical.end.min(end);
            if visible.is_empty() {
                continue;
            }
            mirror.scan_source.clear();
            for physical in screen.stable_range(&logical) {
                if let Some(row) = screen.render_row(physical) {
                    mirror.scan_source.push(SourceRow::from_row(&row));
                }
            }
            let unchanged = mirror
                .logical_scans
                .get(&logical.start)
                .is_some_and(|known| {
                    known.end == logical.end
                        && known.source == mirror.scan_source
                        && known.source.iter().all(|row| row.seqno != SEQ_ZERO)
                });
            let needs_copy = visible.clone().any(|stable| {
                mirror
                    .row_index(stable)
                    .is_none_or(|index| mirror.rows[index].always_recapture())
            });
            if !unchanged || needs_copy {
                capture_group(screen, logical.clone(), mirror, &mut stats, planted_skip);
            } else {
                for stable in visible {
                    mirror.dirty[(stable - first) as usize] = false;
                }
            }
            let known = mirror.logical_scans.entry(logical.start).or_default();
            known.end = logical.end;
            known.generation = generation;
            if !unchanged {
                std::mem::swap(&mut known.source, &mut mirror.scan_source);
            }
        }
        mirror
            .logical_scans
            .retain(|_, known| known.generation == generation);
    }
    // Rows the screen does not have (none for a resident viewport) stay
    // blank rather than leaving the mirror short.
    for (row, dirty) in mirror.dirty.iter().enumerate() {
        if *dirty {
            mirror.rows[row].clear(first + row as StableRowIndex, generation);
        }
    }
    mirror.dimensions = Some(dimensions);
    mirror.cursor = terminal_get_cursor_position(term);
    mirror.seqno = seqno;
    mirror.alt_screen = alt_screen;
    mirror.rules_generation = Some(request.rules_generation);
    mirror.witness = Some(term.screen().capture_coordinate_witness());
    mirror.generation = generation;
    mirror.last = stats;
    Some(stats)
}

/// Copies only source-owned bytes and handles. Unicode segmentation, regex
/// matching, legacy normalization and semantic conversion happen in resolution.
fn capture_group(
    screen: &frankenterm_term::screen::Screen,
    stable: Range<StableRowIndex>,
    mirror: &mut RenderMirror,
    stats: &mut CaptureStats,
    planted_skip: Option<StableRowIndex>,
) {
    let start = mirror.snapshot_count;
    for physical in screen.stable_range(&stable) {
        let Some(source) = screen.render_row(physical) else {
            continue;
        };
        let index = mirror.snapshot_count;
        if index == mirror.snapshots.len() {
            mirror.snapshots.push(RenderRowSnapshot::default());
        }
        let cells = mirror.snapshots[index].capture(source);
        mirror.snapshot_count += 1;
        let row = screen.phys_to_stable_row_index(physical);
        if let Ok(index_in_viewport) = usize::try_from(row - mirror.first) {
            if let Some(dirty) = mirror.dirty.get_mut(index_in_viewport) {
                *dirty = false;
                if planted_skip != Some(row) {
                    stats.rows_captured += 1;
                    stats.cells_captured += cells;
                    stats.packed_rows += usize::from(mirror.snapshots[index].is_packed());
                }
            }
        }
    }
    mirror.pending.push(PendingGroup {
        stable,
        snapshots: start..mirror.snapshot_count,
        planted_skip,
    });
}

/// Rows grouped into one logical line at most, in cells: the cap
/// `Screen::for_each_logical_line_in_stable_range_mut` applies, mirrored so
/// both find the same logical lines.
const MAX_LOGICAL_LINE_LEN: usize = 1024;

/// Rows of a screen read on demand, in small chunks, without mutable
/// access, so clean page-engine rows stay native and a capture reads beyond
/// the viewport only while logical lines continue.
struct RowReader<'a> {
    screen: &'a frankenterm_term::screen::Screen,
    start: usize,
    rows: std::collections::VecDeque<SourceRow>,
    /// The screen has no rows past the last one read.
    at_end: bool,
}

impl<'a> RowReader<'a> {
    const CHUNK: usize = 16;

    fn new(screen: &'a frankenterm_term::screen::Screen, phys: Range<usize>) -> Self {
        let mut reader = Self {
            screen,
            start: phys.start,
            rows: std::collections::VecDeque::new(),
            at_end: false,
        };
        let wanted = phys.len();
        let rows = reader.read(phys);
        reader.at_end = rows.len() < wanted;
        reader.rows.extend(rows);
        reader
    }

    fn read(&self, phys: Range<usize>) -> Vec<SourceRow> {
        let mut rows = Vec::with_capacity(phys.len());
        for physical in phys {
            if let Some(line) = self.screen.render_row(physical) {
                rows.push(SourceRow::from_row(&line));
            }
        }
        rows
    }

    fn get(&mut self, phys: usize) -> Option<&SourceRow> {
        while phys < self.start {
            let from = self.start.saturating_sub(Self::CHUNK);
            let rows = self.read(from..self.start);
            if rows.is_empty() {
                return None;
            }
            for row in rows.into_iter().rev() {
                self.rows.push_front(row);
            }
            self.start = from;
        }
        while phys >= self.start + self.rows.len() {
            if self.at_end {
                return None;
            }
            let from = self.start + self.rows.len();
            let rows = self.read(from..from + Self::CHUNK);
            self.at_end = rows.len() < Self::CHUNK;
            if rows.is_empty() {
                return None;
            }
            self.rows.extend(rows);
        }
        self.rows.get(phys - self.start)
    }
}

/// The exact bounded groups Screen's whole-viewport logical iterator visits.
/// Metadata-only traversal keeps native rows packed even when rules match.
fn logical_line_groups(
    screen: &frankenterm_term::screen::Screen,
    viewport: Range<StableRowIndex>,
) -> Vec<Range<StableRowIndex>> {
    let phys = screen.stable_range(&viewport);
    if phys.is_empty() {
        return Vec::new();
    }
    let mut reader = RowReader::new(screen, phys.clone());

    // Back to the start of the first logical line, as the screen walks.
    let mut start = phys.start;
    let mut back_len = 0usize;
    while start > 0 {
        let Some(prior) = reader.get(start - 1) else {
            break;
        };
        if !prior.wrapped || logical_len_exceeds(back_len, prior.len) {
            break;
        }
        back_len = back_len.saturating_add(prior.len);
        start -= 1;
    }

    let mut rescan = Vec::new();
    let mut row = start;
    while row < phys.end {
        if reader.get(row).is_none() {
            break;
        }
        let mut total = 0usize;
        let mut end_inclusive = row;
        let mut index = row;
        while let Some(line) = reader.get(index) {
            if total > 0 && logical_len_exceeds(total, line.len) {
                break;
            }
            end_inclusive = index;
            total = total.saturating_add(line.len);
            if !line.wrapped {
                break;
            }
            index += 1;
        }
        let logical = screen.phys_to_stable_row_index(row)
            ..screen.phys_to_stable_row_index(end_inclusive + 1);
        rescan.push(logical);
        row = end_inclusive + 1;
    }
    rescan
}

fn logical_len_exceeds(current: usize, additional: usize) -> bool {
    current
        .checked_add(additional)
        .is_none_or(|total| total > MAX_LOGICAL_LINE_LEN)
}

/// [`capture_terminal_rows`] for any pane, through its public row API: for a
/// pane whose terminal this process does not hold (a mux client), and for a
/// local viewport in cold scrollback, whose rows `Pane::get_lines` hydrates.
/// Each call may lock the pane separately, so rows can come from consecutive
/// states, as such panes have always been rendered. Implicit hyperlink rules
/// are applied to the copied rows by logical line within each copied run.
pub fn capture_pane_rows(
    pane: &dyn Pane,
    mirror: &mut RenderMirror,
    request: &CaptureRequest<'_>,
) -> CaptureStats {
    mirror.logical_scans.clear();
    let dimensions = pane.get_dimensions();
    let seqno = pane.get_current_seqno();
    let alt_screen = pane.render_facts().alt_screen_active;
    let full = mirror.rows.is_empty()
        || mirror
            .dimensions
            .map(|dims| (dims.cols, dims.viewport_rows))
            != Some((dimensions.cols, dimensions.viewport_rows))
        || mirror.alt_screen != alt_screen
        || mirror.rules_generation != Some(request.rules_generation)
        || seqno < mirror.seqno;
    let rows = dimensions.viewport_rows;
    let first = request
        .viewport_top
        .unwrap_or(dimensions.physical_top)
        .max(dimensions.scrollback_top)
        .min(dimensions.physical_top);
    let end = first.saturating_add(StableRowIndex::try_from(rows).unwrap_or(0));
    let changed = if full {
        rangeset::RangeSet::new()
    } else {
        pane.get_changed_since(first..end, mirror.seqno)
    };
    mirror.prepare_rows(first, rows);
    for row in 0..rows {
        let stable = first + row as StableRowIndex;
        mirror.dirty[row] = full
            || changed.contains(stable)
            || mirror
                .row_index(stable)
                .is_none_or(|index| mirror.rows[index].always_recapture());
    }

    let generation = mirror.generation.wrapping_add(1);
    let mut stats = CaptureStats {
        rows_examined: rows,
        full,
        ..CaptureStats::default()
    };
    let mut row = 0;
    while row < rows {
        if !mirror.dirty[row] {
            row += 1;
            continue;
        }
        let run_start = row;
        while row < rows && mirror.dirty[row] {
            row += 1;
        }
        let run = first + run_start as StableRowIndex..first + row as StableRowIndex;
        let (lines_first, mut lines) = pane.get_lines(run);
        stats.hyperlink_scans += apply_rules_by_logical_line(request.rules, &mut lines);
        for (offset, line) in lines.iter().enumerate() {
            let stable = lines_first + offset as StableRowIndex;
            let Some(index) = usize::try_from(stable - first).ok() else {
                continue;
            };
            let Some(slot) = mirror.rows.get_mut(index) else {
                continue;
            };
            slot.update_from_line(stable, line, generation);
            mirror.dirty[index] = false;
            stats.rows_captured += 1;
            stats.cells_captured += slot.cells().len();
        }
    }
    for (row, dirty) in mirror.dirty.iter().enumerate() {
        if *dirty {
            mirror.rows[row].clear(first + row as StableRowIndex, generation);
        }
    }
    mirror.dimensions = Some(dimensions);
    mirror.cursor = pane.get_cursor_position();
    mirror.seqno = seqno;
    mirror.alt_screen = alt_screen;
    mirror.rules_generation = Some(request.rules_generation);
    mirror.witness = None;
    mirror.generation = generation;
    mirror.last = stats;
    stats
}

/// Applies `rules` to copied rows, one logical (wrapped) line at a time.
fn apply_rules_by_logical_line(rules: &[Rule], lines: &mut [Line]) -> usize {
    // The source may have been scanned by another view with another rule set.
    // Removing rules must also remove old implicit links from these copies.
    for line in lines.iter_mut() {
        line.invalidate_implicit_hyperlinks(line.current_seqno());
    }
    if rules.is_empty() {
        return 0;
    }
    let mut start = 0;
    let mut scanned = 0;
    while start < lines.len() {
        let mut end = start;
        while end + 1 < lines.len() && lines[end].last_cell_was_wrapped() {
            end += 1;
        }
        let mut logical: Vec<&mut Line> = lines[start..=end].iter_mut().collect();
        Line::apply_hyperlink_rules(rules, &mut logical);
        scanned += 1;
        start = end + 1;
    }
    scanned
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_term::color::ColorPalette;
    use frankenterm_term::config::GridEngine;
    use frankenterm_term::{TerminalConfiguration, TerminalSize};
    use std::sync::Arc;
    use termwiz::cell::{
        Blink, Cell, CellAttributes, Intensity, SemanticType, Underline, VerticalAlign,
    };
    use termwiz::cellcluster::CellCluster;
    use termwiz::color::{ColorAttribute, SrgbaTuple};
    use termwiz::hyperlink::Hyperlink;
    use termwiz::image::ImageCell;

    fn unorm8(component: f32) -> u8 {
        (component.clamp(0.0, 1.0) * 255.0).round() as u8
    }

    #[derive(Debug)]
    struct TestConfig(GridEngine);

    impl TerminalConfiguration for TestConfig {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn grid_engine(&self) -> GridEngine {
            self.0
        }
    }

    fn size(rows: usize, cols: usize) -> TerminalSize {
        TerminalSize {
            rows,
            cols,
            pixel_width: cols * 8,
            pixel_height: rows * 16,
            dpi: 96,
        }
    }

    fn terminal(rows: usize, cols: usize) -> Terminal {
        terminal_with_engine(rows, cols, GridEngine::Legacy)
    }

    fn terminal_with_engine(rows: usize, cols: usize, engine: GridEngine) -> Terminal {
        Terminal::new(
            size(rows, cols),
            Arc::new(TestConfig(engine)),
            "FrankenTerm",
            "render-mirror-test",
            Box::new(Vec::<u8>::new()),
        )
    }

    const LIVE: CaptureRequest<'static> = CaptureRequest {
        viewport_top: None,
        rules: &[],
        rules_generation: 0,
    };

    fn capture(term: &mut Terminal, mirror: &mut RenderMirror) -> CaptureStats {
        capture_terminal_rows(term, mirror, &LIVE).expect("resident viewport")
    }

    /// Independent oracle: Screen's real immutable logical iterator assembles
    /// owned Lines, and the existing Line scanner resolves each group. It does
    /// not use the mirror's grouping, snapshots or semantic rule matcher.
    fn fresh(term: &mut Terminal, request: &CaptureRequest<'_>) -> RenderMirror {
        let dimensions = terminal_get_dimensions(term);
        let first = request
            .viewport_top
            .unwrap_or(dimensions.physical_top)
            .max(dimensions.scrollback_top)
            .min(dimensions.physical_top);
        let end = first + dimensions.viewport_rows as StableRowIndex;
        let mut mirror = RenderMirror::new();
        mirror.first = first;
        mirror.dimensions = Some(dimensions);
        mirror.cursor = terminal_get_cursor_position(term);
        let screen = term.screen();
        screen.for_each_logical_line_in_stable_range(first..end, |logical, source| {
            let mut lines: Vec<_> = source.iter().map(|line| (**line).clone()).collect();
            for line in &mut lines {
                line.invalidate_implicit_hyperlinks(line.current_seqno());
            }
            Line::apply_hyperlink_rules(request.rules, &mut lines.iter_mut().collect::<Vec<_>>());
            for (offset, line) in lines.iter().enumerate() {
                let stable = logical.start + offset as StableRowIndex;
                if (first..end).contains(&stable) {
                    mirror.rows.push(MirrorRow::from_line(stable, line, 0));
                }
            }
            true
        });
        mirror
    }

    type RowContent<'a> = (
        StableRowIndex,
        SequenceNo,
        MirrorLineMode,
        &'a [MirrorCell],
        &'a str,
        &'a [Arc<Hyperlink>],
        bool,
        &'a [MirrorImage],
    );

    /// What a row shows, without the capture bookkeeping (its generation).
    fn content(row: &MirrorRow) -> RowContent<'_> {
        (
            row.stable(),
            row.seqno(),
            row.line_mode(),
            row.cells(),
            row.all_text(),
            row.hyperlinks(),
            row.has_images(),
            row.images(),
        )
    }

    /// ft-yccm0.4.7.2: a row keeps its cells' images by column, and each
    /// cell's in its z-index order (the order its attributes keep them,
    /// whatever order they were attached in), as a renderer draws them. A
    /// row without images keeps none.
    #[test]
    fn a_row_keeps_its_cells_images_in_drawing_order() {
        use termwiz::image::{ImageData, ImageDataType, TextureCoordinate};
        let data = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            2,
            1,
            vec![255, 0, 0, 255, 0, 0, 255, 255],
        )));
        let slice = |left: f32, z_index: i32| {
            ImageCell::with_z_index(
                TextureCoordinate::new_f32(left, 0.0),
                TextureCoordinate::new_f32(left + 0.5, 1.0),
                Arc::clone(&data),
                z_index,
                1,
                2,
                3,
                4,
                Some(7),
                None,
            )
        };
        let mut line = Line::from_text("abcd", &CellAttributes::blank(), 1, None);
        let mut over = CellAttributes::blank();
        over.attach_image(Box::new(slice(0.5, 2)));
        over.attach_image(Box::new(slice(0.0, -1)));
        line.set_cell(3, Cell::new('d', over), 1);
        let mut under = CellAttributes::blank();
        under.attach_image(Box::new(slice(0.0, -5)));
        line.set_cell(1, Cell::new('b', under), 1);

        let mut row = MirrorRow::from_line(0, &line, 1);
        assert!(row.has_images());
        let images: Vec<(usize, i32, f32)> = row
            .images()
            .iter()
            .map(|image| {
                (
                    image.col,
                    image.image.z_index(),
                    image.image.top_left().x.into_inner(),
                )
            })
            .collect();
        assert_eq!(images, [(1, -5, 0.0), (3, -1, 0.0), (3, 2, 0.5)]);
        assert_eq!(row.images()[0].image, slice(0.0, -5));
        assert_eq!(row.images()[0].image.padding(), (1, 2, 3, 4));
        assert!(row.cells()[1].has_image() && !row.cells()[2].has_image());

        // A changed row reuses its storage but must release every old image,
        // reset the image flag and expose only its new text and cell count.
        let cells = row.cells().as_ptr();
        let text = row.all_text().as_ptr();
        let plain = Line::from_text("x", &CellAttributes::blank(), 2, None);
        row.update_from_line(1, &plain, 3);
        assert_eq!(row, MirrorRow::from_line(1, &plain, 3));
        assert_eq!(row.cells().as_ptr(), cells);
        assert_eq!(row.all_text().as_ptr(), text);
        assert!(!row.has_images() && row.images().is_empty());
        row.clear(2, 4);
        assert_eq!(row, MirrorRow::empty(2, 4));
        assert_eq!(row.cells().as_ptr(), cells);
        assert_eq!(row.all_text().as_ptr(), text);
    }

    /// The first difference between what two mirrors show, if any.
    fn difference(mirror: &RenderMirror, reference: &RenderMirror) -> Option<String> {
        if (mirror.first, mirror.cursor, mirror.dimensions)
            != (reference.first, reference.cursor, reference.dimensions)
        {
            return Some(format!(
                "viewport {:?} vs {:?}",
                (mirror.first, mirror.cursor, mirror.dimensions),
                (reference.first, reference.cursor, reference.dimensions)
            ));
        }
        if mirror.rows.len() != reference.rows.len() {
            return Some(format!(
                "{} rows vs {}",
                mirror.rows.len(),
                reference.rows.len()
            ));
        }
        mirror
            .rows
            .iter()
            .zip(&reference.rows)
            .find(|(row, expected)| content(row) != content(expected))
            .map(|(row, expected)| format!("row {:?}\n vs {:?}", row, expected))
    }

    /// xorshift64*: deterministic, so a failing seed replays.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
        }

        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }

        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len() as u64) as usize]
        }
    }

    const WORDS: &[&str] = &[
        "alpha",
        "beta",
        "γάμμα",
        "你好",
        "世界",
        "e\u{301}",
        "👍",
        "👩\u{200d}💻",
        "❤\u{fe0f}",
        "🇯🇵",
        "x",
        "  ",
        "\t",
    ];

    const SGR: &[&str] = &[
        "0",
        "1",
        "2",
        "3",
        "4",
        "4:2",
        "4:3",
        "4:4",
        "4:5",
        "5",
        "7",
        "8",
        "9",
        "53",
        "22",
        "23",
        "24",
        "27",
        "38;5;196",
        "48;5;21",
        "38;2;10;200;30",
        "48;2;250;128;7",
        "58;5;46",
        "58;2;1;2;3",
        "39",
        "49",
        "59",
        "73",
        "74",
    ];

    /// Terminal output exercising text (wide, clusters, emoji), styles,
    /// cursor moves, scrolling, scroll regions with insert/delete/scroll,
    /// erases, insert/delete/erase characters, both screens, hyperlinks
    /// explicit and implicit, autowrap and resets.
    fn random_output(rng: &mut Rng, rows: usize, cols: usize) -> Vec<u8> {
        let mut out = String::new();
        let (rows, cols) = (rows as u64, cols as u64);
        match rng.below(18) {
            0..=4 => {
                for _ in 0..1 + rng.below(8) {
                    out.push_str(rng.pick(WORDS));
                    out.push(' ');
                }
            }
            5 | 6 => out.push_str(&format!("\x1b[{}m", rng.pick(SGR))),
            7 => out.push_str(&format!(
                "\x1b[{};{}H",
                1 + rng.below(rows + 2),
                1 + rng.below(cols + 2)
            )),
            8 => {
                for _ in 0..1 + rng.below(4) {
                    out.push_str("\r\n");
                }
            }
            9 => {
                let top = 1 + rng.below(rows);
                let bottom = top + rng.below(rows + 1 - top);
                out.push_str(&format!("\x1b[{top};{bottom}r\x1b[{top};1H"));
                out.push_str(rng.pick(&[
                    "\x1b[2L",
                    "\x1b[M",
                    "\x1b[3S",
                    "\x1b[T",
                    "line\r\n\r\n",
                    "\x1bM\x1bM",
                ]));
                out.push_str("\x1b[r");
            }
            10 => out.push_str(rng.pick(&[
                "\x1b[J", "\x1b[1J", "\x1b[2J", "\x1b[K", "\x1b[1K", "\x1b[2K",
            ])),
            11 => out.push_str(rng.pick(&["\x1b[3@", "\x1b[2P", "\x1b[4X"])),
            12 => out.push_str(rng.pick(&[
                "\x1b[?1049h",
                "\x1b[?1049l",
                "\x1b[?47h",
                "\x1b[?47l",
                "\x1b[?1047h",
                "\x1b[?1047l",
            ])),
            13 => out.push_str("\x1b]8;;https://example.com/a\x1b\\link\x1b]8;;\x1b\\"),
            14 | 15 => {
                out.push_str("see https://example.com/some/long/path/that/wraps/across/the/edge ")
            }
            16 => out.push_str(rng.pick(&["\x1b[3J", "\x1b[?7l", "\x1b[?7h"])),
            _ => {
                if rng.below(4) == 0 {
                    out.push_str("\x1bc");
                }
            }
        }
        out.into_bytes()
    }

    /// Feeds random output in steps, capturing incrementally now and then,
    /// sometimes into a scrolled-back viewport, and after a planted fault
    /// when `plant` is set. Returns how many captures differed from a fresh
    /// full capture.
    fn random_session(seed: u64, rules: &[Rule], plant: bool, engine: GridEngine) -> usize {
        let mut rng = Rng::new(seed);
        let (mut rows, mut cols) = (8, 24);
        let mut term = terminal_with_engine(rows, cols, engine);
        let mut mirror = RenderMirror::new();
        let mut differences = 0;
        for step in 0..400 {
            term.advance_bytes(random_output(&mut rng, rows, cols));
            if rng.below(40) == 0 {
                rows = 4 + rng.below(9) as usize;
                cols = 10 + rng.below(31) as usize;
                term.resize(size(rows, cols));
            }
            if rng.below(3) != 0 {
                continue;
            }
            let dims = terminal_get_dimensions(&mut term);
            let viewport_top = (rng.below(4) == 0).then(|| {
                let back = dims.physical_top - dims.scrollback_top;
                dims.physical_top - rng.below(back as u64 + 1) as StableRowIndex
            });
            let request = CaptureRequest {
                viewport_top,
                rules,
                rules_generation: 0,
            };
            let planted = plant.then(|| terminal_get_cursor_position(&mut term).y);
            let stats =
                capture_rows(&mut term, &mut mirror, &request, planted).expect("resident viewport");
            assert!(stats.rows_captured <= stats.rows_examined);
            let reference = fresh(&mut term, &request);
            if let Some(difference) = difference(&mirror, &reference) {
                assert!(
                    plant,
                    "seed {} step {}: incremental capture differs from a full one: {}",
                    seed, step, difference
                );
                differences += 1;
                mirror.invalidate();
            }
        }
        differences
    }

    #[test]
    fn incremental_captures_equal_full_captures_under_random_output() {
        let rules = [Rule::new(r"https?://\S+", "$0").expect("rule")];
        for engine in [GridEngine::Legacy, GridEngine::Page] {
            for seed in 0..12 {
                let rules: &[Rule] = if seed % 2 == 0 { &[] } else { &rules };
                assert_eq!(random_session(seed, rules, false, engine), 0);
            }
        }
    }

    #[test]
    fn the_randomized_check_catches_a_capture_that_misses_changed_rows() {
        // Planted negative: each capture skips the cursor's row, the row
        // output most often changes; the check must notice.
        for engine in [GridEngine::Legacy, GridEngine::Page] {
            let caught: usize = (0..4)
                .map(|seed| random_session(seed, &[], true, engine))
                .sum();
            assert!(caught > 0, "a missed dirty row went unnoticed");
        }
    }

    #[test]
    fn native_rows_resolve_after_the_terminal_has_changed() {
        let mut term = terminal_with_engine(3, 24, GridEngine::Page);
        term.advance_bytes(
            "\x1b[38;2;17;34;51m界e\u{301}\r\n\x1b]8;;https://example.test/old\x1b\\linked\x1b]8;;\x1b\\\r\nlast",
        );
        let mut mirror = RenderMirror::new();
        let stats = capture_terminal_rows_deferred(&mut term, &mut mirror, &LIVE).unwrap();
        assert_eq!((stats.rows_captured, stats.packed_rows), (3, 3));
        assert_eq!(mirror.pending.len(), 3);
        assert!(mirror.rows.iter().all(|row| row.cells().is_empty()));
        let reference = fresh(&mut term, &LIVE);

        // The parser is free to replace page side tables before rendering
        // finishes: the first phase must own all its referenced payloads.
        term.advance_bytes("\x1bc\x1b[38;2;200;100;0mreplacement");
        mirror.resolve_pending(LIVE.rules);
        assert!(mirror.pending.is_empty());
        assert_eq!(difference(&mirror, &reference), None);
        assert_eq!(mirror.rows()[0].text(&mirror.rows()[0].cells()[0]), "界");
        assert_eq!(
            mirror.rows()[1].hyperlinks()[0].uri(),
            "https://example.test/old"
        );
        capture(&mut term, &mut mirror);
        assert_eq!(difference(&mirror, &fresh(&mut term, &LIVE)), None);
    }

    #[test]
    fn implicit_rules_run_only_after_owned_capture_and_survive_source_replacement() {
        let rules = [Rule::new(r"https?://\S+", "$0").unwrap()];
        let request = CaptureRequest {
            rules: &rules,
            ..LIVE
        };
        let mut term = terminal_with_engine(3, 10, GridEngine::Page);
        term.advance_bytes("https://example.test/path");
        let expected = fresh(&mut term, &request);
        let mut mirror = RenderMirror::new();
        let stats = capture_terminal_rows_deferred(&mut term, &mut mirror, &request).unwrap();
        assert_eq!((stats.rows_captured, stats.packed_rows), (3, 3));
        assert_eq!(stats.hyperlink_scans, 0, "capture cannot execute rules");
        assert!(mirror.rows().iter().all(|row| row.cells().is_empty()));
        assert!((0..3).all(|row| matches!(
            term.screen().render_row(row),
            Some(RenderRowRef::Page { .. })
        )));

        term.advance_bytes("\x1bcreplaced");
        mirror.resolve_pending(request.rules);
        assert_eq!(mirror.last_capture().hyperlink_scans, 1);
        assert_eq!(difference(&mirror, &expected), None);
        assert_eq!(
            mirror.rows()[0].hyperlinks()[0].uri(),
            "https://example.test/path"
        );
    }

    #[test]
    fn each_mirror_resolves_its_rules_and_disabling_removes_previously_scanned_links() {
        let first_rules = [Rule::new(r"issue-\d+", "https://first.test/$0").unwrap()];
        let second_rules = [Rule::new(r"issue-\d+", "https://second.test/$0").unwrap()];
        for engine in [GridEngine::Legacy, GridEngine::Page] {
            let mut term = terminal_with_engine(1, 64, engine);
            term.advance_bytes("issue-17 \x1b]8;;https://explicit.test\x1b\\OSC8\x1b]8;;\x1b\\");
            term.screen_mut()
                .for_each_logical_line_in_stable_range_mut(0..1, |_, lines| {
                    Line::apply_hyperlink_rules(&first_rules, lines);
                    true
                });
            let source = term.screen().phys_line(0).clone();
            assert!(source.implicit_hyperlinks_are_scanned());
            let sequence = term.current_seqno();
            let first_request = CaptureRequest {
                rules: &first_rules,
                ..LIVE
            };
            let second_request = CaptureRequest {
                rules: &second_rules,
                ..LIVE
            };
            let mut first = RenderMirror::new();
            let mut second = RenderMirror::new();
            capture_terminal_rows(&mut term, &mut first, &first_request).unwrap();
            capture_terminal_rows(&mut term, &mut second, &second_request).unwrap();
            assert_eq!(
                first.rows()[0].hyperlinks()[0].uri(),
                "https://first.test/issue-17"
            );
            assert_eq!(
                second.rows()[0].hyperlinks()[0].uri(),
                "https://second.test/issue-17"
            );
            assert_eq!(
                term.screen().phys_line(0),
                &source,
                "rendering cannot change shared rule state"
            );

            let disabled = CaptureRequest {
                rules_generation: 1,
                ..LIVE
            };
            capture_terminal_rows(&mut term, &mut first, &disabled).unwrap();
            assert_eq!(first.rows()[0].hyperlinks().len(), 1);
            assert_eq!(
                first.rows()[0].hyperlinks()[0].uri(),
                "https://explicit.test"
            );
            assert!(first.rows()[0]
                .hyperlink(&first.rows()[0].cells()[0])
                .is_none());
            assert_eq!(difference(&first, &fresh(&mut term, &disabled)), None);
            let idle = capture_terminal_rows(&mut term, &mut second, &second_request).unwrap();
            assert_eq!((idle.rows_captured, idle.hyperlink_scans), (0, 0));

            let enabled_again = CaptureRequest {
                rules_generation: 2,
                ..second_request
            };
            capture_terminal_rows(&mut term, &mut first, &enabled_again).unwrap();
            assert_eq!(difference(&first, &second), None);
            assert_eq!(term.current_seqno(), sequence);
            assert_eq!(term.screen().phys_line(0), &source);

            // Remote/cold pane copies also discard a source's stale rule epoch.
            let mut copies = vec![source.clone()];
            apply_rules_by_logical_line(&second_rules, &mut copies);
            assert_eq!(
                MirrorRow::from_line(0, &copies[0], 0).hyperlinks()[0].uri(),
                "https://second.test/issue-17"
            );
            apply_rules_by_logical_line(&[], &mut copies);
            assert!(copies[0].visible_cells().all(|cell| cell
                .attrs()
                .hyperlink()
                .is_none_or(|link| !link.is_implicit())));
        }
    }

    #[test]
    fn owned_group_boundaries_match_the_screen_iterator_across_cap_and_viewport_shifts() {
        let rules = [Rule::new(r"issue-\d+", "https://tracker.test/$0").unwrap()];
        for cols in [400, 600, 1200] {
            let mut term = terminal_with_engine(4, cols, GridEngine::Page);
            term.advance_bytes(format!(
                "{}issue-17{}",
                "x".repeat(cols - 3),
                "x".repeat(cols * 6)
            ));
            let dimensions = terminal_get_dimensions(&mut term);
            let mut mirror = RenderMirror::new();
            for first in 0..=dimensions.physical_top {
                let viewport = first..first + 4;
                let mut expected = Vec::new();
                term.screen().for_each_logical_line_in_stable_range(
                    viewport.clone(),
                    |range, _| {
                        expected.push(range);
                        true
                    },
                );
                assert_eq!(logical_line_groups(term.screen(), viewport), expected);
                let request = CaptureRequest {
                    viewport_top: Some(first),
                    rules: &rules,
                    ..LIVE
                };
                capture_terminal_rows(&mut term, &mut mirror, &request).unwrap();
                assert_eq!(difference(&mirror, &fresh(&mut term, &request)), None);
                assert!(mirror.snapshots.len() <= 4 + 2 * MAX_LOGICAL_LINE_LEN);
            }
        }
    }

    #[test]
    fn native_damage_is_independent_for_each_mirror_and_includes_selection_floors() {
        let mut term = terminal_with_engine(4, 20, GridEngine::Page);
        term.advance_bytes("one\r\ntwo\r\nthree\r\nfour");
        let mut first = RenderMirror::new();
        let mut second = RenderMirror::new();
        assert_eq!(capture(&mut term, &mut first).packed_rows, 4);
        assert_eq!(capture(&mut term, &mut second).packed_rows, 4);

        term.advance_bytes("\x1b[1;1HONE\x1b[3;1HTHREE");
        assert_eq!(capture(&mut term, &mut first).packed_rows, 2);
        assert_eq!(capture(&mut term, &mut second).packed_rows, 2);
        assert_eq!(difference(&first, &second), None);

        term.increment_seqno();
        let selection_seqno = term.current_seqno();
        term.screen_mut().touch_all_rows(selection_seqno);
        assert_eq!(capture(&mut term, &mut first).packed_rows, 4);
        assert_eq!(capture(&mut term, &mut second).packed_rows, 4);
        assert!(first
            .rows()
            .iter()
            .all(|row| row.seqno() == selection_seqno));
        assert_eq!(difference(&first, &fresh(&mut term, &LIVE)), None);
        assert_eq!(difference(&first, &second), None);
        assert_eq!(capture(&mut term, &mut first).rows_captured, 0);
        assert_eq!(capture(&mut term, &mut second).rows_captured, 0);
    }

    #[test]
    fn native_capture_preserves_edited_lines_and_implicit_link_rescans() {
        let mut term = terminal_with_engine(4, 20, GridEngine::Page);
        term.advance_bytes("https://example.com/abcdefghijklmnop\r\nthird\r\nfourth");
        let mut mirror = RenderMirror::new();
        assert_eq!(capture(&mut term, &mut mirror).packed_rows, 4);
        let rules = [Rule::new(r"https?://\S+", "$0").expect("rule")];
        let request = CaptureRequest {
            rules: &rules,
            rules_generation: 1,
            ..LIVE
        };
        let stats = capture_terminal_rows(&mut term, &mut mirror, &request).unwrap();
        assert_eq!(
            stats.packed_rows, 4,
            "matching hyperlinks must not materialize authoritative Line views"
        );
        assert!(!mirror.rows()[0].hyperlinks().is_empty());
        assert!(!mirror.rows()[1].hyperlinks().is_empty());
        assert_eq!(difference(&mirror, &fresh(&mut term, &request)), None);

        term.advance_bytes("\x1b[1;1Hhttp ");
        let stats = capture_terminal_rows(&mut term, &mut mirror, &request).unwrap();
        assert_eq!(
            stats.rows_captured, 2,
            "the unchanged wrapped continuation also changes"
        );
        assert!(mirror.rows()[0].hyperlinks().is_empty());
        assert!(mirror.rows()[1].hyperlinks().is_empty());
        assert_eq!(difference(&mirror, &fresh(&mut term, &request)), None);

        // An unrelated authoritative edit still wins over the stale page.
        // Disabling scanning must handle both source forms in one capture.
        let seqno = term.current_seqno();
        term.screen_mut()
            .line_mut(2)
            .set_cell(0, Cell::new('!', CellAttributes::blank()), seqno);
        term.advance_bytes("\x1b[4;1Hnative");
        let request = CaptureRequest {
            rules_generation: 2,
            ..LIVE
        };
        let stats = capture_terminal_rows(&mut term, &mut mirror, &request).unwrap();
        assert_eq!(stats.packed_rows, 3);
        assert_eq!(difference(&mirror, &fresh(&mut term, &request)), None);
    }

    #[test]
    fn default_rules_keep_native_rows_packed_and_skip_idle_resolution() {
        let rules = config::default_hyperlink_rules();
        assert_eq!(rules.len(), 6);
        let request = CaptureRequest {
            rules: &rules,
            ..LIVE
        };
        let mut term = terminal_with_engine(4, 64, GridEngine::Page);
        term.advance_bytes(
            "first ordinary log row\r\n界面 ready 🚀\r\nhttps://example.test\r\nfourth row",
        );
        let mut first = RenderMirror::new();
        let mut second = RenderMirror::new();
        for mirror in [&mut first, &mut second] {
            let stats = capture_terminal_rows(&mut term, mirror, &request).unwrap();
            assert_eq!((stats.rows_captured, stats.packed_rows), (4, 4));
            assert_eq!(mirror.logical_scans.len(), 4);
            assert_eq!(stats.hyperlink_scans, 4);
            assert!(!mirror.rows()[2].hyperlinks().is_empty());
            let row_storage: Vec<_> = mirror
                .rows()
                .iter()
                .map(|row| (row.cells().as_ptr(), row.all_text().as_ptr()))
                .collect();
            let stats = capture_terminal_rows(&mut term, mirror, &request).unwrap();
            assert_eq!((stats.rows_captured, stats.packed_rows), (0, 0));
            assert_eq!(
                stats.hyperlink_scans, 0,
                "idle positive and negative results must not rebuild text or run regexes"
            );
            assert_eq!(
                mirror
                    .rows()
                    .iter()
                    .map(|row| (row.cells().as_ptr(), row.all_text().as_ptr()))
                    .collect::<Vec<_>>(),
                row_storage
            );
        }
        assert!((0..4).all(|physical| matches!(
            term.screen().render_row(physical),
            Some(RenderRowRef::Page { .. })
        )));
        term.advance_bytes("\x1b[3;1Hchanged third row");
        for mirror in [&mut first, &mut second] {
            let stats = capture_terminal_rows(&mut term, mirror, &request).unwrap();
            assert_eq!((stats.rows_captured, stats.packed_rows), (1, 1));
        }
        assert_eq!(difference(&first, &second), None);
        assert_eq!(difference(&first, &fresh(&mut term, &request)), None);
    }

    #[test]
    fn logical_scan_certificates_are_pruned_as_output_scrolls() {
        let rules = config::default_hyperlink_rules();
        let request = CaptureRequest {
            rules: &rules,
            ..LIVE
        };
        let mut term = terminal_with_engine(4, 40, GridEngine::Page);
        let mut mirror = RenderMirror::new();
        for step in 0..80 {
            term.advance_bytes(format!("ordinary log row {step}\r\n"));
            capture_terminal_rows(&mut term, &mut mirror, &request).unwrap();
            let end = mirror.first() + 4;
            assert!(mirror.logical_scans.len() <= 4);
            assert!(
                mirror.snapshots.len() <= 4,
                "owned context stays viewport-bounded"
            );
            assert!(mirror
                .logical_scans
                .iter()
                .all(|(start, known)| *start >= mirror.first() && known.end <= end));
        }
        mirror.invalidate();
        assert!(mirror.logical_scans.is_empty());
    }

    #[test]
    fn native_rule_epoch_is_invalidated_when_new_rules_match_unchanged_text() {
        let defaults = config::default_hyperlink_rules();
        let mut term = terminal_with_engine(1, 40, GridEngine::Page);
        term.advance_bytes("issue-17");
        let mut mirror = RenderMirror::new();
        let initial = CaptureRequest {
            rules: &defaults,
            ..LIVE
        };
        assert_eq!(
            capture_terminal_rows(&mut term, &mut mirror, &initial)
                .unwrap()
                .packed_rows,
            1
        );
        assert_eq!(mirror.logical_scans.len(), 1);
        let seqno = term.current_seqno();
        let changed = [Rule::new(r"issue-\d+", "https://tracker.test/$0").unwrap()];
        let request = CaptureRequest {
            rules: &changed,
            rules_generation: 1,
            ..LIVE
        };
        let stats = capture_terminal_rows(&mut term, &mut mirror, &request).unwrap();
        assert!(stats.full);
        assert_eq!(stats.packed_rows, 1);
        assert_eq!(term.current_seqno(), seqno, "no output forced the rescan");
        assert_eq!(mirror.logical_scans.len(), 1);
        assert_eq!(
            mirror.rows()[0].hyperlinks()[0].uri(),
            "https://tracker.test/issue-17"
        );
    }

    #[test]
    fn native_link_matches_resolve_owned_wrapped_urls_wide_padding_and_email() {
        let rules = config::default_hyperlink_rules();
        let request = CaptureRequest {
            rules: &rules,
            ..LIVE
        };
        for (rows, cols, text) in [
            (4, 12, "see https://example.com/a_long_path"),
            (1, 48, "界https://example.com"),
            (1, 48, "mail user@example.com"),
        ] {
            let mut term = terminal_with_engine(rows, cols, GridEngine::Page);
            term.advance_bytes(text);
            let mut mirror = RenderMirror::new();
            capture_terminal_rows(&mut term, &mut mirror, &request).unwrap();
            assert!(mirror.rows().iter().any(|row| !row.hyperlinks().is_empty()));
            assert!(matches!(
                term.screen().render_row(0),
                Some(RenderRowRef::Page { .. })
            ));
            if text.starts_with('界') {
                let row = &mirror.rows()[0];
                assert!(row.hyperlink(&row.cells()[0]).is_none());
                assert_eq!(
                    row.hyperlink(&row.cells()[1]).unwrap().uri(),
                    "https://example.com"
                );
            }
            assert_eq!(difference(&mirror, &fresh(&mut term, &request)), None);
        }
    }

    #[test]
    fn logical_scan_certificate_includes_a_continuation_below_the_viewport() {
        let rules = config::default_hyperlink_rules();
        let mut term = terminal_with_engine(4, 10, GridEngine::Page);
        term.advance_bytes("a\r\nb\r\nc\r\nsee https:zzzz\r\n\r\n\r\n");
        assert_eq!(terminal_get_dimensions(&mut term).physical_top, 4);
        let request = CaptureRequest {
            viewport_top: Some(0),
            rules: &rules,
            rules_generation: 0,
        };
        let mut mirror = RenderMirror::new();
        let stats = capture_terminal_rows(&mut term, &mut mirror, &request).unwrap();
        assert_eq!(stats.packed_rows, 4);
        let proof = mirror.logical_scans.get(&3).unwrap();
        assert_eq!((proof.end, proof.source.len()), (5, 2));
        assert!(mirror.rows()[3].hyperlinks().is_empty());
        let unchanged = mirror.rows()[3].seqno();

        term.advance_bytes("\x1b[1;1H//ab.cd");
        assert_eq!(term.screen().render_row(3).unwrap().seqno(), unchanged);
        let stats = capture_terminal_rows(&mut term, &mut mirror, &request).unwrap();
        assert_eq!((stats.rows_captured, stats.packed_rows), (1, 1));
        assert_eq!(mirror.rows()[3].hyperlinks()[0].uri(), "https://ab.cd");
        assert!(mirror.logical_scans.contains_key(&3));
        assert_eq!(difference(&mirror, &fresh(&mut term, &request)), None);
    }

    #[test]
    fn owned_scans_preserve_cap_split_lines_and_authoritative_edits() {
        let rules = config::default_hyperlink_rules();
        let request = CaptureRequest {
            rules: &rules,
            ..LIVE
        };
        let mut term = terminal_with_engine(4, 600, GridEngine::Page);
        term.advance_bytes("x".repeat(1500));
        let mut mirror = RenderMirror::new();
        capture_terminal_rows(&mut term, &mut mirror, &request).unwrap();
        for row in 0..3 {
            assert!(matches!(
                term.screen().render_row(row),
                Some(RenderRowRef::Page { .. })
            ));
        }
        assert_eq!(difference(&mirror, &fresh(&mut term, &request)), None);

        let mut term = terminal_with_engine(1, 40, GridEngine::Page);
        term.advance_bytes("ordinary text");
        let mut mirror = RenderMirror::new();
        capture_terminal_rows(&mut term, &mut mirror, &request).unwrap();
        let seqno = term.current_seqno();
        let mut attrs = CellAttributes::blank();
        attrs.set_hyperlink(Some(Arc::new(Hyperlink::new("https://explicit.test"))));
        term.screen_mut()
            .line_mut(0)
            .set_cell(0, Cell::new('x', attrs), seqno);
        let stats = capture_terminal_rows(&mut term, &mut mirror, &request).unwrap();
        assert_eq!((stats.rows_captured, stats.packed_rows), (1, 0));
        assert_eq!(
            mirror.rows()[0].hyperlinks()[0].uri(),
            "https://explicit.test"
        );
        assert!(!mirror.logical_scans.get(&0).unwrap().source[0].native);
    }

    #[test]
    fn the_line_oracle_catches_a_forged_logical_scan_certificate() {
        let rules = config::default_hyperlink_rules();
        let request = CaptureRequest {
            rules: &rules,
            ..LIVE
        };
        let mut term = terminal_with_engine(1, 64, GridEngine::Page);
        let mut oracle = terminal_with_engine(1, 64, GridEngine::Page);
        term.advance_bytes("ordinary text");
        oracle.advance_bytes("ordinary text");
        let mut mirror = RenderMirror::new();
        capture_terminal_rows(&mut term, &mut mirror, &request).unwrap();
        let output = "\x1b[1;1Hhttps://example.com\x1b[K";
        term.advance_bytes(output);
        oracle.advance_bytes(output);
        // Planted fault: falsely claim the old negative scan covered the
        // new contents. The independent Line scanner must expose the error.
        mirror.logical_scans.get_mut(&0).unwrap().source[0] =
            SourceRow::from_row(&term.screen().render_row(0).unwrap());
        capture_terminal_rows(&mut term, &mut mirror, &request).unwrap();
        assert!(mirror.rows()[0].hyperlinks().is_empty());
        assert!(difference(&mirror, &fresh(&mut oracle, &request)).is_some());
    }

    #[test]
    fn a_capture_that_misses_a_changed_row_fails_the_equality_check() {
        let mut term = terminal(6, 20);
        term.advance_bytes(b"\x1b[3;1Hbefore");
        let mut mirror = RenderMirror::new();
        capture(&mut term, &mut mirror);
        term.advance_bytes(b"\x1b[3;1Hafter!");
        let changed = mirror.first() + 2;
        let stats = capture_rows(&mut term, &mut mirror, &LIVE, Some(changed)).expect("resident");
        assert_eq!(
            stats.rows_captured, 0,
            "the fault skipped the only changed row"
        );
        let reference = fresh(&mut term, &LIVE);
        assert!(
            difference(&mirror, &reference).is_some(),
            "the check must catch the missed row"
        );
        // An honest capture repairs it: the stale row's seqno is behind.
        assert_eq!(capture(&mut term, &mut mirror).rows_captured, 1);
        assert_eq!(difference(&mirror, &fresh(&mut term, &LIVE)), None);
    }

    #[test]
    fn a_capture_copies_only_the_rows_that_changed() {
        let mut term = terminal(10, 40);
        term.advance_bytes(b"one\r\ntwo\r\nthree");
        let mut mirror = RenderMirror::new();
        let stats = capture(&mut term, &mut mirror);
        assert!(stats.full);
        assert_eq!(stats.rows_captured, 10);

        let stats = capture(&mut term, &mut mirror);
        assert_eq!(
            (stats.full, stats.rows_captured, stats.cells_captured),
            (false, 0, 0)
        );

        term.advance_bytes(b"\x1b[6;3Hx");
        let stats = capture(&mut term, &mut mirror);
        assert_eq!((stats.full, stats.rows_captured), (false, 1));
        let row = &mirror.rows()[5];
        assert_eq!(row.text(&row.cells()[2]), "x");

        term.advance_bytes(b"\x1b[2;1Hab\x1b[9;1Hcd");
        assert_eq!(capture(&mut term, &mut mirror).rows_captured, 2);

        // A scroll at the bottom: the written row and the new one. The rows
        // that only moved keep their stable index and are not copied.
        term.advance_bytes(b"\x1b[10;1Hlast\r\n");
        let stats = capture(&mut term, &mut mirror);
        assert_eq!((stats.full, stats.rows_captured), (false, 2));

        term.advance_bytes(b"\x1b[?1049h");
        let stats = capture(&mut term, &mut mirror);
        assert_eq!((stats.full, stats.rows_captured), (true, 10));
        assert_eq!(difference(&mirror, &fresh(&mut term, &LIVE)), None);
    }

    #[test]
    fn stationary_capture_reuses_row_tables_and_dirty_row_buffers() {
        let mut term = terminal(4, 24);
        for row in 1..=4 {
            term.advance_bytes(format!("\x1b[{row};1Habcdefghijklmnopqrstuvwx"));
        }
        let mut mirror = RenderMirror::new();
        capture(&mut term, &mut mirror);
        let table = mirror.rows.as_ptr();
        let damage = mirror.dirty.as_ptr();
        let buffers: Vec<_> = mirror
            .rows()
            .iter()
            .map(|row| (row.cells().as_ptr(), row.all_text().as_ptr()))
            .collect();

        for frame in 0..24 {
            let changed = frame % 4;
            term.advance_bytes(format!(
                "\x1b[{};1Hframe {frame:02}                ",
                changed + 1
            ));
            assert_eq!(capture(&mut term, &mut mirror).rows_captured, 1);
            assert_eq!(difference(&mirror, &fresh(&mut term, &LIVE)), None);
            assert_eq!(mirror.rows.as_ptr(), table);
            assert_eq!(mirror.dirty.as_ptr(), damage);
            for (row, expected) in mirror.rows().iter().zip(&buffers) {
                assert_eq!((row.cells().as_ptr(), row.all_text().as_ptr()), *expected);
            }

            // An idle pane still participates in another pane's window frame.
            // It neither copies cells nor rebuilds the owned row tables.
            assert_eq!(capture(&mut term, &mut mirror).rows_captured, 0);
            assert_eq!(mirror.rows.as_ptr(), table);
            assert_eq!(mirror.dirty.as_ptr(), damage);
        }
    }

    #[test]
    fn scrolling_rotates_reusable_buffers_with_their_stable_rows() {
        let mut term = terminal(5, 24);
        for row in 0..30 {
            term.advance_bytes(format!("row {row:02} abcdefghijklmnop\r\n"));
        }
        let top = terminal_get_dimensions(&mut term).physical_top;
        let mut mirror = RenderMirror::new();
        let at = |viewport_top| CaptureRequest {
            viewport_top: Some(viewport_top),
            ..LIVE
        };
        capture_terminal_rows(&mut term, &mut mirror, &at(top - 10)).expect("resident");
        let table = mirror.rows.as_ptr();
        let damage = mirror.dirty.as_ptr();

        for offset in [9, 8, 10, 7, 11, 12, 13, 9, 14, 2, 15, 10] {
            let previous: Vec<_> = mirror
                .rows()
                .iter()
                .map(|row| {
                    (
                        row.stable(),
                        row.generation(),
                        row.cells().as_ptr(),
                        row.all_text().as_ptr(),
                    )
                })
                .collect();
            let request = at(top - offset);
            let stats = capture_terminal_rows(&mut term, &mut mirror, &request).expect("resident");
            assert_eq!(difference(&mirror, &fresh(&mut term, &request)), None);
            assert_eq!(mirror.rows.as_ptr(), table);
            assert_eq!(mirror.dirty.as_ptr(), damage);
            let mut overlap = 0;
            for row in mirror.rows() {
                if let Some((_, generation, cells, text)) =
                    previous.iter().find(|(stable, ..)| *stable == row.stable())
                {
                    overlap += 1;
                    assert_eq!(row.generation(), *generation);
                    assert_eq!(row.cells().as_ptr(), *cells);
                    assert_eq!(row.all_text().as_ptr(), *text);
                }
            }
            assert_eq!(stats.rows_captured, 5 - overlap);
        }
    }

    #[test]
    fn reused_rows_still_recapture_images_and_zero_sequence_contents() {
        use termwiz::image::{ImageData, ImageDataType, TextureCoordinate};

        let mut term = terminal(3, 16);
        term.advance_bytes(b"ordinary");
        let data = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![255, 0, 0, 255],
        )));
        let mut attrs = CellAttributes::blank();
        attrs.attach_image(Box::new(ImageCell::with_z_index(
            TextureCoordinate::new_f32(0.0, 0.0),
            TextureCoordinate::new_f32(1.0, 1.0),
            data,
            0,
            0,
            0,
            0,
            0,
            None,
            None,
        )));
        let seqno = term.current_seqno();
        term.screen_mut()
            .set_cell(0, 1, &Cell::new('x', attrs), seqno);
        term.screen_mut().with_phys_lines_mut(2..3, |lines| {
            *lines[0] = Line::from_text("zero 0", &CellAttributes::blank(), SEQ_ZERO, None);
        });
        let mut mirror = RenderMirror::new();
        capture(&mut term, &mut mirror);
        let ordinary_generation = mirror.rows()[0].generation();
        let image_cells = mirror.rows()[1].cells().as_ptr();
        let zero_cells = mirror.rows()[2].cells().as_ptr();

        for frame in 1..=3 {
            // A SEQ_ZERO producer need not advance the terminal sequence to
            // force a refresh. Images likewise refresh behind shared handles.
            term.screen_mut().with_phys_lines_mut(2..3, |lines| {
                *lines[0] = Line::from_text(
                    &format!("zero {frame}"),
                    &CellAttributes::blank(),
                    SEQ_ZERO,
                    None,
                );
            });
            assert_eq!(capture(&mut term, &mut mirror).rows_captured, 2);
            assert_eq!(mirror.rows()[0].generation(), ordinary_generation);
            assert_eq!(mirror.rows()[1].generation(), mirror.generation());
            assert_eq!(mirror.rows()[2].generation(), mirror.generation());
            assert_eq!(mirror.rows()[1].cells().as_ptr(), image_cells);
            assert_eq!(mirror.rows()[2].cells().as_ptr(), zero_cells);
            assert_eq!(mirror.rows()[2].all_text(), format!("zero {frame}"));
            assert_eq!(difference(&mirror, &fresh(&mut term, &LIVE)), None);
        }
    }

    #[test]
    fn scrolling_the_viewport_copies_only_the_rows_it_exposes() {
        let mut term = terminal(5, 20);
        for row in 0..20 {
            term.advance_bytes(format!("row {row}\r\n"));
        }
        let top = terminal_get_dimensions(&mut term).physical_top;
        let at = |viewport_top| CaptureRequest {
            viewport_top: Some(viewport_top),
            ..LIVE
        };
        let mut mirror = RenderMirror::new();
        capture_terminal_rows(&mut term, &mut mirror, &at(top - 2)).expect("resident");
        let stats = capture_terminal_rows(&mut term, &mut mirror, &at(top - 3)).expect("resident");
        assert_eq!((stats.full, stats.rows_captured), (false, 1));
        assert_eq!(difference(&mirror, &fresh(&mut term, &at(top - 3))), None);
        let stats = capture_terminal_rows(&mut term, &mut mirror, &LIVE).expect("resident");
        assert_eq!((stats.full, stats.rows_captured), (false, 3));
        assert_eq!(difference(&mirror, &fresh(&mut term, &LIVE)), None);
    }

    #[test]
    fn switching_screens_recaptures_rows_whose_seqnos_did_not_move() {
        let mut term = terminal(4, 20);
        term.advance_bytes(b"primary");
        let mut mirror = RenderMirror::new();
        capture(&mut term, &mut mirror);
        // 47 switches without clearing: the alternate rows keep old seqnos.
        for switch in [
            &b"\x1b[?47h"[..],
            b"\x1b[?47l",
            b"\x1b[?1047h",
            b"\x1b[?1047l",
        ] {
            term.advance_bytes(switch);
            let stats = capture(&mut term, &mut mirror);
            assert!(stats.full);
            assert_eq!(difference(&mirror, &fresh(&mut term, &LIVE)), None);
        }
    }

    #[test]
    fn a_rescan_of_a_wrapped_link_recaptures_its_clean_continuation_row() {
        let rules = [Rule::new(r"https?://\S+", "$0").expect("rule")];
        let request = CaptureRequest {
            rules: &rules,
            ..LIVE
        };
        let mut term = terminal(4, 20);
        term.advance_bytes(b"https://example.com/abcdefghijklmnop");
        let mut mirror = RenderMirror::new();
        capture_terminal_rows(&mut term, &mut mirror, &request).expect("resident");
        // Change only the first row of the wrapped URL.
        term.advance_bytes(b"\x1b[1;1Hhttp ");
        let stats = capture_terminal_rows(&mut term, &mut mirror, &request).expect("resident");
        assert_eq!(stats.rows_captured, 2, "the continuation row is copied too");
        assert_eq!(difference(&mirror, &fresh(&mut term, &request)), None);
    }

    /// bv13 (seed 5, step 210), minimized: a wrapped logical line runs from
    /// the last row of a scrolled-back viewport into the row below it. Output
    /// there completes a URL across both rows; the rescan rewrites the
    /// visible row too, so the capture must copy it although nothing wrote to
    /// it.
    #[test]
    fn a_link_completed_below_a_scrolled_back_viewport_recaptures_the_visible_row() {
        let rules = [Rule::new(r"https?://\S+", "$0").expect("rule")];
        let mut term = terminal(4, 10);
        // Row 3 is "see https:" and wraps into row 4, "zzzz": no link yet.
        term.advance_bytes(b"a\r\nb\r\nc\r\nsee https:zzzz\r\n\r\n\r\n");
        let top = terminal_get_dimensions(&mut term).physical_top;
        assert_eq!(top, 4, "rows 4 to 7 are on screen");
        let request = CaptureRequest {
            viewport_top: Some(0),
            rules: &rules,
            rules_generation: 0,
        };
        let links_on_row_3 = |mirror: &RenderMirror| {
            let row = &mirror.rows()[3];
            assert_eq!(row.stable(), 3);
            row.cells()
                .iter()
                .filter(|cell| row.hyperlink(cell).is_some())
                .count()
        };
        let mut mirror = RenderMirror::new();
        capture_terminal_rows(&mut term, &mut mirror, &request).expect("resident");
        assert_eq!(links_on_row_3(&mirror), 0);

        // Row 4, the screen's first row and below the viewport, becomes
        // "//ab.cd": the logical line reads "see https://ab.cd".
        term.advance_bytes(b"\x1b[1;1H//ab.cd");
        let stats = capture_terminal_rows(&mut term, &mut mirror, &request).expect("resident");
        assert!(!stats.full);
        assert_eq!(
            stats.rows_captured, 1,
            "only the visible row of the rescanned line"
        );
        assert_eq!(difference(&mirror, &fresh(&mut term, &request)), None);
        assert_eq!(links_on_row_3(&mirror), "https:".len());
    }

    #[test]
    fn compaction_keeps_every_attribute_the_renderer_draws() {
        let palette = ColorPalette::default();
        let colors = [
            ColorAttribute::Default,
            ColorAttribute::PaletteIndex(3),
            ColorAttribute::PaletteIndex(200),
            ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(0.1, 0.5, 0.9, 1.0)),
            ColorAttribute::TrueColorWithPaletteFallback(SrgbaTuple(1.0, 0.0, 0.25, 0.5), 9),
        ];
        let link = Arc::new(Hyperlink::new("https://example.com"));
        let eight =
            |SrgbaTuple(r, g, b, a): SrgbaTuple| [unorm8(r), unorm8(g), unorm8(b), unorm8(a)];
        let mut rng = Rng::new(7);
        for _ in 0..64 {
            let mut line = Line::new(5);
            let mut col = 0;
            while col < 40 {
                let mut attrs = CellAttributes::default();
                attrs
                    .set_intensity(*rng.pick(&[
                        Intensity::Normal,
                        Intensity::Bold,
                        Intensity::Half,
                    ]))
                    .set_underline(*rng.pick(&[
                        Underline::None,
                        Underline::Single,
                        Underline::Double,
                        Underline::Curly,
                        Underline::Dotted,
                        Underline::Dashed,
                    ]))
                    .set_blink(*rng.pick(&[Blink::None, Blink::Slow, Blink::Rapid]))
                    .set_italic(rng.below(2) == 1)
                    .set_reverse(rng.below(2) == 1)
                    .set_strikethrough(rng.below(2) == 1)
                    .set_invisible(rng.below(2) == 1)
                    .set_overline(rng.below(2) == 1)
                    .set_vertical_align(*rng.pick(&[
                        VerticalAlign::BaseLine,
                        VerticalAlign::SuperScript,
                        VerticalAlign::SubScript,
                    ]))
                    .set_semantic_type(*rng.pick(&[
                        SemanticType::Output,
                        SemanticType::Input,
                        SemanticType::Prompt,
                    ]))
                    .set_foreground(*rng.pick(&colors))
                    .set_background(*rng.pick(&colors))
                    .set_underline_color(*rng.pick(&colors));
                if rng.below(3) == 0 {
                    attrs.set_hyperlink(Some(Arc::clone(&link)));
                }
                let text = *rng.pick(&["a", "Z", "你", "e\u{301}", "👩\u{200d}💻", "❤\u{fe0f}"]);
                let cell = Cell::new_grapheme(text, attrs, None);
                let width = cell.width().max(1);
                line.set_cell(col, cell, 5);
                col += width;
            }
            let row = MirrorRow::from_line(0, &line, 1);
            let originals: Vec<_> = line.visible_cells().collect();
            assert_eq!(row.cells().len(), originals.len());
            for (cell, original) in row.cells().iter().zip(originals) {
                let attrs = original.attrs();
                assert_eq!(row.text(cell), original.str());
                assert_eq!(
                    (cell.col(), cell.width()),
                    (original.cell_index(), original.width())
                );
                assert_eq!(cell.intensity(), attrs.intensity());
                assert_eq!(cell.underline(), attrs.underline());
                assert_eq!(cell.blink(), attrs.blink());
                assert_eq!(cell.vertical_align(), attrs.vertical_align());
                assert_eq!(cell.semantic_type(), attrs.semantic_type());
                assert_eq!(
                    [
                        cell.italic(),
                        cell.reverse(),
                        cell.strikethrough(),
                        cell.invisible(),
                        cell.overline()
                    ],
                    [
                        attrs.italic(),
                        attrs.reverse(),
                        attrs.strikethrough(),
                        attrs.invisible(),
                        attrs.overline()
                    ]
                );
                assert_eq!(
                    eight(palette.resolve_fg(cell.fg().to_attribute())),
                    eight(palette.resolve_fg(attrs.foreground()))
                );
                assert_eq!(
                    eight(palette.resolve_bg(cell.bg().to_attribute())),
                    eight(palette.resolve_bg(attrs.background()))
                );
                assert_eq!(
                    eight(palette.resolve_fg(cell.underline_color().to_attribute())),
                    eight(palette.resolve_fg(attrs.underline_color()))
                );
                assert_eq!(row.hyperlink(cell), attrs.hyperlink());
                assert!(!cell.has_image());
            }
        }
    }

    /// ft-yccm0.4.7.3: a mirror row clusters for shaping exactly as the line
    /// it was captured from: the same runs, texts, widths, first cells,
    /// presentations, attributes and byte-to-cell maps. The lines mix
    /// attribute changes, semantic types, hyperlinks, whitespace runs (which
    /// force breaks), wide characters, combining marks and emoji sequences.
    #[test]
    fn mirror_rows_cluster_exactly_as_their_lines() {
        // Colors the mirror keeps exactly: 8-bit true colors name the
        // default fallback, as SGR 38;2 does.
        let colors = [
            ColorAttribute::Default,
            ColorAttribute::PaletteIndex(3),
            ColorAttribute::PaletteIndex(200),
            ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(
                51.0 / 255.0,
                128.0 / 255.0,
                1.0,
                1.0,
            )),
        ];
        let link = Arc::new(Hyperlink::new("https://example.com"));
        let texts = [
            "a",
            "-",
            ">",
            "=",
            "f",
            "i",
            " ",
            " ",
            "你",
            "e\u{301}",
            "👩\u{200d}💻",
            "❤\u{fe0f}",
        ];
        let mut rng = Rng::new(11);
        let mut clusters_seen = 0;
        for _ in 0..200 {
            let mut line = Line::new(5);
            let mut col = 0;
            let mut attrs = CellAttributes::default();
            while col < 40 {
                // Runs of equal attributes, so clusters span several cells.
                if rng.below(4) == 0 {
                    attrs = CellAttributes::default();
                    attrs
                        .set_intensity(*rng.pick(&[Intensity::Normal, Intensity::Bold]))
                        .set_italic(rng.below(3) == 0)
                        .set_underline(*rng.pick(&[Underline::None, Underline::Curly]))
                        .set_semantic_type(*rng.pick(&[
                            SemanticType::Output,
                            SemanticType::Input,
                            SemanticType::Prompt,
                        ]))
                        .set_foreground(*rng.pick(&colors))
                        .set_background(*rng.pick(&colors))
                        .set_underline_color(*rng.pick(&colors));
                    if rng.below(4) == 0 {
                        attrs.set_hyperlink(Some(Arc::clone(&link)));
                    }
                }
                let cell = Cell::new_grapheme(rng.pick(&texts), attrs.clone(), None);
                let width = cell.width().max(1);
                line.set_cell(col, cell, 5);
                col += width;
            }
            let row = MirrorRow::from_line(0, &line, 1);
            let theirs = line.cluster(None);
            assert_same_clusters(&row.clusters(), &theirs, &line);
            // The row made a line again clusters the same, and so does it
            // with an IME composition overlaid, as the WebGpu renderer
            // overlays one on the captured line.
            assert_same_clusters(&row.to_line().cluster(None), &theirs, &line);
            let at = rng.below(44) as usize;
            let composing = *rng.pick(&["x", "日本", "e\u{301}a", "→ ok"]);
            let mut composed = line.clone();
            composed.overlay_text_with_attribute(at, composing, CellAttributes::blank(), 5);
            let mut ours = row.to_line();
            ours.overlay_text_with_attribute(at, composing, CellAttributes::blank(), 5);
            assert_same_clusters(&ours.cluster(None), &composed.cluster(None), &composed);
            clusters_seen += theirs.len();
        }
        assert!(clusters_seen > 1000, "{}", clusters_seen);
    }

    fn assert_same_clusters(ours: &[CellCluster], theirs: &[CellCluster], line: &Line) {
        assert_eq!(ours.len(), theirs.len(), "{:?}", line);
        for (ours, theirs) in ours.iter().zip(theirs) {
            assert_eq!(ours.text, theirs.text);
            assert_eq!(
                (ours.width, ours.first_cell_idx),
                (theirs.width, theirs.first_cell_idx)
            );
            assert_eq!(ours.presentation, theirs.presentation);
            assert_eq!(ours.direction, theirs.direction);
            assert_eq!(ours.attrs, theirs.attrs, "{:?}", theirs.text);
            for byte in 0..theirs.text.len() {
                assert_eq!(
                    (ours.byte_to_cell_idx(byte), ours.byte_to_cell_width(byte)),
                    (
                        theirs.byte_to_cell_idx(byte),
                        theirs.byte_to_cell_width(byte)
                    )
                );
            }
        }
    }
}
