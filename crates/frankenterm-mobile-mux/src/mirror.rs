//! One remote pane mirrored onto a phone terminal of the pane's size.
//!
//! The server describes the pane by stable row: the viewport starts at
//! `physical_top` and every row keeps its index as it scrolls into history.
//! The mirror compares that with what the phone last drew and writes the
//! difference: rows that changed in place, or a scroll that pushes the rows
//! leaving the top into the phone's own scrollback, each in its final form.
//! A row that wraps is drawn so the phone's terminal wraps it too: copied
//! text and addresses run on across the rows as they do on the desktop.

use crate::render;
use crate::wire::{
    CursorPosition, Dimensions, LineEntries, LineReadLayout, RenderHead, StableRowIndex,
};
use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use termwiz::surface::{CursorVisibility, Line};

/// The most history the phone's terminal keeps (`NativeTerminalModel`'s
/// `scrollbackRows`).
const PHONE_HISTORY_ROWS: usize = 256;
/// The cells the phone's terminal holds: both screens and the history
/// beside them (`native::MAX_RESIDENT_CELLS`).
const PHONE_RESIDENT_CELLS: usize = 65_536;

/// Remote history the phone's scrollback receives when a `rows` by `cols`
/// pane is first drawn, and the most rows one catch-up after a burst of
/// output replays: what the phone's terminal keeps, no more.
pub fn history_rows(rows: usize, cols: usize) -> usize {
    PHONE_HISTORY_ROWS.min(PHONE_RESIDENT_CELLS.saturating_sub(2 * rows * cols) / cols.max(1))
}
/// The largest pane, either way, a server may report; anything bigger is a
/// broken or hostile server, whose rows the mirror would walk one by one.
pub const MAX_PANE_CELLS_EACH_WAY: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    dims: Dimensions,
    cursor: CursorPosition,
    alt: bool,
    title: String,
}

impl Target {
    fn top(&self) -> StableRowIndex {
        self.dims.physical_top
    }

    fn rows(&self) -> usize {
        self.dims.viewport_rows
    }

    fn viewport(&self) -> Range<StableRowIndex> {
        self.top()..offset(self.top(), self.rows())
    }
}

fn offset(row: StableRowIndex, by: usize) -> StableRowIndex {
    row.saturating_add(StableRowIndex::try_from(by).unwrap_or(StableRowIndex::MAX))
}

fn distance(from: StableRowIndex, to: StableRowIndex) -> usize {
    usize::try_from(to.saturating_sub(from)).unwrap_or(0)
}

/// Draws `line`, above `next`, on screen row `y`, where the cursor is; true
/// when it wraps (see `render::paint_line`). A row `blank` already (it
/// scrolled in, or the screen was just wiped) is not erased: each erase
/// costs the phone's terminal a row's width of the work budget it has for a
/// piece of output.
fn paint(
    out: &mut String,
    line: Option<&Line>,
    next: Option<&Line>,
    cols: usize,
    y: usize,
    blank: bool,
) -> bool {
    if blank {
        render::paint_blank_line(out, line, next, cols)
    } else if y == 0 {
        render::paint_top_line(out, line, next, cols)
    } else {
        render::paint_line(out, line, next, cols)
    }
}

/// What the phone's terminal currently shows.
#[derive(Debug, Default)]
struct Shown {
    painted: bool,
    rows: usize,
    cols: usize,
    /// The top row of the alternate screen while the phone shows it.
    alt: Option<StableRowIndex>,
    /// The top row of the primary screen as last drawn.
    primary: Option<StableRowIndex>,
    title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// The primary screen was never drawn: write history, then the screen,
    /// line after line so the history scrolls into the phone's scrollback.
    Stream { from: StableRowIndex },
    /// Redraw every row in place.
    Full,
    /// The primary screen scrolled down from `old_top`; the rows from
    /// `entering` on enter at the bottom one at a time.
    Scroll {
        old_top: StableRowIndex,
        entering: StableRowIndex,
        all: bool,
    },
    /// Redraw the rows that changed.
    Dirty,
}

/// Bytes for the phone terminal, which must be `rows` by `cols` first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub rows: usize,
    pub cols: usize,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct PaneMirror {
    target: Option<Target>,
    drawn: Option<Target>,
    shown: Shown,
    visible: bool,
    fetching: bool,
    lines: HashMap<StableRowIndex, Line>,
    /// Rows the server was asked for and does not have.
    absent: BTreeSet<StableRowIndex>,
    dirty: BTreeSet<StableRowIndex>,
    /// A requested size (rows, cols) the server has not answered yet: the
    /// pane is not drawn at any other size until it does.
    resizing: Option<(usize, usize)>,
    /// Bumped by every show and hide. Rows fetched in an earlier epoch may
    /// predate changes pushed while the pane was hidden, which the mirror
    /// did not track, so their reply is dropped.
    epoch: u64,
    fetch_epoch: u64,
    observation: Option<LineReadLayout>,
    awaiting_observation: bool,
    observation_request: Option<u64>,
    /// Only the one outstanding read's rows can be invalidated by a push.
    /// This stays bounded by the already bounded viewport/history request.
    fetch_ranges: Vec<Range<StableRowIndex>>,
    fetch_invalidated: BTreeSet<StableRowIndex>,
}

impl PaneMirror {
    pub fn new(visible: bool) -> Self {
        Self {
            visible,
            ..Self::default()
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn title(&self) -> Option<&str> {
        self.target.as_ref().map(|target| target.title.as_str())
    }

    /// One past the pane's newest row, as last reported.
    pub fn end_row(&self) -> Option<StableRowIndex> {
        self.target.as_ref().map(|target| target.viewport().end)
    }

    /// Starts drawing again; the phone terminal is redrawn from scratch.
    pub fn show(&mut self) {
        self.visible = true;
        self.shown = Shown::default();
        self.drawn = None;
        self.forget_rows();
    }

    /// Stops drawing; later changes only update what the pane looks like.
    pub fn hide(&mut self) {
        self.visible = false;
        self.forget_rows();
    }

    fn forget_rows(&mut self) {
        self.epoch += 1;
        self.observation_request = None;
        self.lines.clear();
        self.absent.clear();
        self.dirty.clear();
    }

    pub fn observation(&self) -> Option<LineReadLayout> {
        self.observation
    }

    pub fn require_observation(&mut self) {
        self.awaiting_observation = true;
        self.observation_request = None;
    }

    pub fn expect_observation(&mut self, serial: u64) {
        self.awaiting_observation = true;
        self.observation_request = Some(serial);
    }

    pub fn expects_observation(&self, serial: u64) -> bool {
        self.observation_request == Some(serial)
    }

    /// A newer push may overtake a correlated reply. It must not rewind the
    /// target, but the reply still settles the pending observation request.
    pub fn observe(&mut self, layout: LineReadLayout, correlated: bool) -> bool {
        if correlated {
            self.awaiting_observation = false;
            self.observation_request = None;
        }
        if self
            .observation
            .is_some_and(|current| current.seqno > layout.seqno)
        {
            return false;
        }
        self.observation = Some(layout);
        true
    }

    fn invalidate_fetched_rows(&mut self, range: Range<StableRowIndex>) {
        if self.fetching {
            for requested in &self.fetch_ranges {
                let start = range.start.max(requested.start);
                let end = range.end.min(requested.end);
                self.fetch_invalidated.extend(start..end);
            }
        }
    }

    /// A pushed update. Without its lines (they did not decode) every row
    /// of the screen is fetched again. A size no terminal has is refused.
    /// True when the pane is hidden and what it shows changed: anything but
    /// a new title alone, which the server sends with the cursor's row.
    pub fn apply_changes(
        &mut self,
        head: RenderHead,
        lines: Option<LineEntries>,
    ) -> Result<bool, crate::Error> {
        let dims = head.dimensions;
        let sane = 1..=MAX_PANE_CELLS_EACH_WAY;
        if !sane.contains(&dims.viewport_rows) || !sane.contains(&dims.cols) {
            return Err(crate::Error::Frame("a pane size no terminal has"));
        }
        let target = Target {
            dims,
            cursor: head.cursor_position,
            alt: head.alt_screen_active,
            title: head.title,
        };
        let changed_hidden = !self.visible
            && self.target.as_ref().is_none_or(|previous| {
                previous.title == target.title
                    || previous.dims != target.dims
                    || previous.cursor != target.cursor
                    || previous.alt != target.alt
                    || !head.dirty_lines.is_empty()
                    || lines.as_ref().is_none_or(|entries| {
                        entries.lines.iter().any(|(row, _)| *row != target.cursor.y)
                    })
            });
        if self.visible {
            let viewport = target.viewport();
            if let Some(previous) = self.target.clone() {
                if previous.alt != target.alt || !previous.dims.same_line_layout(&target.dims) {
                    self.epoch = self
                        .epoch
                        .checked_add(1)
                        .ok_or(crate::Error::Frame("pane layout epoch exhausted"))?;
                    self.lines.clear();
                    self.absent.clear();
                } else if previous.top() != target.top() {
                    // The server reports the rows that changed on the screen
                    // only. Output lands at and below the cursor, so a row
                    // from there may have been written and scrolled off since
                    // the last update, and what is cached for it is out of
                    // date: fetch it again. Rows above the cursor were done.
                    let unsettled = previous.cursor.y.max(previous.top()).min(target.top());
                    self.invalidate_fetched_rows(unsettled..target.top());
                    self.lines
                        .retain(|row, _| *row < unsettled || viewport.contains(row));
                }
            }
            let dirty = match lines {
                Some(_) => head.dirty_lines,
                None => vec![viewport.clone()],
            };
            for range in dirty {
                let start = range.start.max(viewport.start);
                let end = range.end.min(viewport.end);
                self.invalidate_fetched_rows(start..end);
                self.dirty.extend(start..end);
                self.lines.retain(|row, _| !(start..end).contains(row));
            }
            for (row, line) in lines.map(LineEntries::linked_lines).unwrap_or_default() {
                self.invalidate_fetched_rows(row..row.saturating_add(1));
                self.dirty.insert(row);
                self.absent.remove(&row);
                self.lines.insert(row, line);
            }
        }
        self.target = Some(target);
        Ok(changed_hidden)
    }

    /// Stores a GetLines reply; rows asked for and not returned are gone
    /// from the server and draw as blank.
    pub fn apply_lines(&mut self, entries: LineEntries, requested: &[Range<StableRowIndex>]) {
        self.fetching = false;
        if self.fetch_epoch != self.epoch {
            self.fetch_ranges.clear();
            self.fetch_invalidated.clear();
            return;
        }
        let mut returned = BTreeSet::new();
        for (row, line) in entries.linked_lines() {
            if self.fetch_invalidated.contains(&row)
                || !requested.iter().any(|range| range.contains(&row))
            {
                continue;
            }
            returned.insert(row);
            // A newer pushed row always wins over this asynchronous read.
            self.lines.entry(row).or_insert(line);
        }
        for range in requested {
            self.absent.extend(range.clone().filter(|row| {
                !returned.contains(row)
                    && !self.fetch_invalidated.contains(row)
                    && !self.lines.contains_key(row)
            }));
        }
        self.fetch_ranges.clear();
        self.fetch_invalidated.clear();
    }

    pub fn set_fetching(&mut self, ranges: &[Range<StableRowIndex>]) {
        self.fetching = true;
        self.fetch_epoch = self.epoch;
        self.fetch_ranges = ranges.to_vec();
        self.fetch_invalidated.clear();
    }

    /// A rejected fenced read says nothing about whether its rows exist.
    /// Keep the last display and request fresh metadata before another fetch.
    pub fn reject_lines(&mut self) {
        self.fetching = false;
        self.fetch_ranges.clear();
        self.fetch_invalidated.clear();
        self.require_observation();
    }

    /// The pane was asked to become `rows` by `cols`.
    pub fn expect_size(&mut self, rows: usize, cols: usize) {
        self.resizing = Some((rows, cols));
    }

    /// The server answered the resize; draw at whatever size it reports.
    pub fn size_settled(&mut self) {
        self.resizing = None;
    }

    /// Drawing waits while a requested size is pending and not yet reported.
    fn waiting_for_size(&self, target: &Target) -> bool {
        self.resizing
            .is_some_and(|size| size != (target.rows(), target.dims.cols))
    }

    pub fn is_fetching(&self) -> bool {
        self.fetching
    }

    /// A successfully observed unchanged target needs no output bytes, but
    /// still settles a prior busy/waiting state once every row is available.
    pub fn is_drawable(&self) -> bool {
        self.visible
            && !self.fetching
            && !self.awaiting_observation
            && self
                .target
                .as_ref()
                .is_some_and(|target| !self.waiting_for_size(target))
            && self.wanted().is_empty()
    }

    fn plan(&self, target: &Target) -> Plan {
        let top = target.top();
        let resized = (target.rows(), target.dims.cols) != (self.shown.rows, self.shown.cols);
        if target.alt {
            return match self.shown.alt {
                Some(old_top) if !resized && old_top == top => Plan::Dirty,
                _ => Plan::Full,
            };
        }
        let returning = self.shown.alt.is_some();
        let history = history_rows(target.rows(), target.dims.cols);
        match self.shown.primary {
            None => Plan::Stream {
                from: top
                    .saturating_sub(offset(0, history))
                    .max(target.dims.scrollback_top)
                    .min(top),
            },
            Some(old_top) if resized || top < old_top || (top == old_top && returning) => {
                Plan::Full
            }
            Some(old_top) if top == old_top => Plan::Dirty,
            Some(old_top) => {
                let rows = target.rows();
                let entering = if distance(old_top, top) <= rows + history {
                    offset(old_top, rows)
                } else {
                    top.saturating_sub(offset(0, history))
                };
                Plan::Scroll {
                    old_top,
                    entering,
                    all: returning,
                }
            }
        }
    }

    fn needed(&self, target: &Target, plan: Plan) -> Vec<Range<StableRowIndex>> {
        let viewport = target.viewport();
        let dirty = || -> Vec<Range<StableRowIndex>> {
            self.dirty
                .range(viewport.clone())
                .map(|row| *row..row + 1)
                .collect()
        };
        let mut needed = match plan {
            Plan::Stream { from } => std::iter::once(from..viewport.end).collect(),
            Plan::Full => vec![viewport.clone()],
            Plan::Scroll {
                old_top,
                entering,
                all,
            } => {
                let leaving_end = viewport.start.min(offset(old_top, target.rows()));
                // With the row above the entering ones while the phone's
                // bottom row shows it, in case it wraps into them.
                let from = if entering == offset(old_top, target.rows()) {
                    entering.saturating_sub(1)
                } else {
                    entering
                };
                let mut needed = vec![old_top..leaving_end, from..viewport.end];
                if all {
                    needed.push(viewport.clone());
                } else {
                    needed.extend(dirty());
                }
                needed
            }
            Plan::Dirty => dirty(),
        };
        // A row drawn in place that wraps carries on into the row below,
        // whose first cell is then needed as well.
        if matches!(plan, Plan::Scroll { .. } | Plan::Dirty) {
            let below: Vec<_> = needed
                .iter()
                .flat_map(Clone::clone)
                .filter(|row| self.lines.get(row).is_some_and(Line::last_cell_was_wrapped))
                .map(|row| offset(row, 1))
                .filter(|row| viewport.contains(row))
                .map(|row| row..offset(row, 1))
                .collect();
            needed.extend(below);
        }
        needed
    }

    /// The rows to fetch before the pane can be drawn, as GetLines ranges.
    pub fn wanted(&self) -> Vec<Range<StableRowIndex>> {
        let Some(target) = self.target.as_ref().filter(|target| {
            self.visible
                && !self.fetching
                && !self.awaiting_observation
                && !self.waiting_for_size(target)
        }) else {
            return Vec::new();
        };
        let mut missing = BTreeSet::new();
        for range in self.needed(target, self.plan(target)) {
            missing.extend(
                range.filter(|row| !self.lines.contains_key(row) && !self.absent.contains(row)),
            );
        }
        let mut ranges: Vec<Range<StableRowIndex>> = Vec::new();
        for row in missing {
            match ranges.last_mut() {
                Some(last) if last.end == row => last.end = row + 1,
                _ => ranges.push(row..row + 1),
            }
        }
        ranges
    }

    /// Draws what changed once every row it needs is here, as outputs for
    /// the phone's terminal to take in order, each a piece of its own.
    pub fn render(&mut self) -> Vec<Output> {
        let mut outputs = Vec::new();
        if !self.visible || self.fetching || self.awaiting_observation || !self.wanted().is_empty()
        {
            return outputs;
        }
        let Some(target) = self.target.clone() else {
            return outputs;
        };
        if self.waiting_for_size(&target) {
            return outputs;
        }
        let plan = self.plan(&target);
        let viewport = target.viewport();
        let changed_rows = self.dirty.range(viewport.clone()).count();
        if plan == Plan::Dirty && changed_rows == 0 && self.drawn.as_ref() == Some(&target) {
            self.lines.clear();
            self.absent.clear();
            self.dirty.clear();
            return outputs;
        }

        let rows = target.rows();
        let cols = target.dims.cols;
        let top = target.top();
        if !self.shown.painted {
            // Whatever the terminal showed before (another pane, perhaps in
            // its alternate screen) goes, with its history: a full reset,
            // which the phone's terminal charges both screens and the
            // history it keeps, all one piece of output may cost. Pastes are
            // bracketed from here on, so the session can hand each one to
            // the server whole, which brackets it again only if the pane's
            // program asked.
            outputs.push(Output {
                rows,
                cols,
                bytes: b"\x1bc\x1b[?25l\x1b[?2004h".to_vec(),
            });
        }
        let mut out = String::from("\x1b[?25l");
        // The screen drawn on is blank now: wiped above, or an alternate
        // screen entered here, which clears it.
        let wiped = !self.shown.painted || (target.alt && self.shown.alt.is_none());
        if target.alt && self.shown.alt.is_none() {
            out.push_str("\x1b[?1049h");
        } else if !target.alt && self.shown.alt.is_some() {
            out.push_str("\x1b[?1049l");
        }
        let line = |row: StableRowIndex| self.lines.get(&row);
        let at = |row: StableRowIndex, from: StableRowIndex| distance(from, row);
        // Draws `row` on screen row `y`. A row that wraps carries on into
        // the first cell of the row below, so the phone marks the wrap too;
        // never from the bottom row, where that would scroll: true when the
        // cursor waits past it there instead.
        let in_place = |out: &mut String, row: StableRowIndex, y: usize| {
            render::move_to(out, y, 0);
            let wraps = paint(out, line(row), line(offset(row, 1)), cols, y, wiped);
            if wraps && y + 1 < rows {
                render::continue_onto(out, line(offset(row, 1)));
                return false;
            }
            wraps
        };
        match plan {
            Plan::Stream { from } => {
                // The primary screen was not drawn since the wipe, so every
                // row lands on a blank one, or scrolls in blank.
                out.push_str("\x1b[H");
                // The row written last, and whether it wraps.
                let mut previous: Option<(StableRowIndex, bool)> = None;
                // The screen row the next one lands on.
                let mut y = 0;
                for row in from..viewport.end {
                    if row < top && line(row).is_none() {
                        continue;
                    }
                    match previous {
                        Some((above, true)) if offset(above, 1) == row => {
                            render::carry_on(&mut out, line(row));
                        }
                        Some(_) => out.push_str("\r\n"),
                        None => {}
                    }
                    let wraps = paint(&mut out, line(row), line(offset(row, 1)), cols, y, true);
                    previous = Some((row, wraps));
                    y = (y + 1).min(rows.saturating_sub(1));
                }
            }
            Plan::Full => {
                for row in viewport.clone() {
                    in_place(&mut out, row, at(row, top));
                }
            }
            Plan::Scroll {
                old_top,
                entering,
                all,
            } => {
                let redrawn = |row: &StableRowIndex| all || self.dirty.contains(row);
                let leaving = old_top..top.min(offset(old_top, rows));
                let mut wraps = false;
                for row in leaving.clone() {
                    wraps = in_place(&mut out, row, at(row, old_top));
                }
                // The phone's bottom row shows the row above the first one
                // entering; if that row wraps, the first entering row
                // carries it on, from the cursor waiting past it. Drawn there
                // just now if it left; else drawn again, unless it is drawn
                // in place below, where it carries on.
                let above = entering.saturating_sub(1);
                let bottom = rows.saturating_sub(1);
                let continued = entering == offset(old_top, rows);
                if !(continued && leaving.contains(&above)) {
                    render::move_to(&mut out, bottom, 0);
                    wraps = continued
                        && !redrawn(&above)
                        && line(above).is_some_and(Line::last_cell_was_wrapped)
                        && paint(&mut out, line(above), line(entering), cols, bottom, false);
                }
                // Each entering row scrolls in blank.
                for row in entering..viewport.end {
                    if wraps {
                        render::carry_on(&mut out, line(row));
                    } else {
                        out.push('\n');
                    }
                    wraps = paint(
                        &mut out,
                        line(row),
                        line(offset(row, 1)),
                        cols,
                        bottom,
                        true,
                    );
                }
                for row in top..entering.min(viewport.end) {
                    if redrawn(&row) {
                        in_place(&mut out, row, at(row, top));
                    }
                }
            }
            Plan::Dirty => {
                for row in self.dirty.range(viewport.clone()) {
                    in_place(&mut out, *row, at(*row, top));
                }
            }
        }

        let cursor = target.cursor;
        let mut visibility = cursor.visibility;
        if viewport.contains(&cursor.y) {
            render::move_to(
                &mut out,
                at(cursor.y, top),
                cursor.x.min(cols.saturating_sub(1)),
            );
        } else {
            visibility = CursorVisibility::Hidden;
        }
        render::cursor_style(&mut out, cursor.shape, visibility);
        if target.title != self.shown.title {
            render::title(&mut out, &target.title);
        }

        self.shown.painted = true;
        self.shown.rows = rows;
        self.shown.cols = cols;
        self.shown.title.clone_from(&target.title);
        if target.alt {
            self.shown.alt = Some(top);
        } else {
            self.shown.alt = None;
            self.shown.primary = Some(top);
        }
        self.drawn = Some(target);
        self.lines.clear();
        self.absent.clear();
        self.dirty.clear();
        outputs.push(Output {
            rows,
            cols,
            bytes: out.into_bytes(),
        });
        outputs
    }
}
