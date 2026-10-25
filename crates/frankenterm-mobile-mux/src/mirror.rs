//! One remote pane mirrored onto a phone terminal of the pane's size.
//!
//! The server describes the pane by stable row: the viewport starts at
//! `physical_top` and every row keeps its index as it scrolls into history.
//! The mirror compares that with what the phone last drew and writes the
//! difference: rows that changed in place, or a scroll that pushes the rows
//! leaving the top into the phone's own scrollback, each in its final form.

use crate::render;
use crate::wire::{CursorPosition, Dimensions, LineEntries, RenderHead, StableRowIndex};
use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use termwiz::surface::{CursorVisibility, Line};

/// Remote history the phone's scrollback receives when a pane is first
/// drawn, and the most rows one catch-up after a burst of output replays.
pub const HISTORY_ROWS: usize = 1000;
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
        self.lines.clear();
        self.absent.clear();
        self.dirty.clear();
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
            if let Some(previous) = &self.target {
                if previous.alt != target.alt {
                    self.lines.clear();
                } else if previous.top() != target.top() {
                    // The server reports the rows that changed on the screen
                    // only. Output lands at and below the cursor, so a row
                    // from there may have been written and scrolled off since
                    // the last update, and what is cached for it is out of
                    // date: fetch it again. Rows above the cursor were done.
                    let unsettled = previous.cursor.y.max(previous.top()).min(target.top());
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
                self.dirty.extend(start..end);
                self.lines.retain(|row, _| !(start..end).contains(row));
            }
            for (row, line) in lines.map(|entries| entries.lines).unwrap_or_default() {
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
            return;
        }
        let mut returned = BTreeSet::new();
        for (row, line) in entries.lines {
            returned.insert(row);
            self.lines.insert(row, line);
        }
        for range in requested {
            self.absent
                .extend(range.clone().filter(|row| !returned.contains(row)));
        }
    }

    pub fn set_fetching(&mut self) {
        self.fetching = true;
        self.fetch_epoch = self.epoch;
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
        match self.shown.primary {
            None => Plan::Stream {
                from: top
                    .saturating_sub(offset(0, HISTORY_ROWS))
                    .max(target.dims.scrollback_top)
                    .min(top),
            },
            Some(old_top) if resized || top < old_top || (top == old_top && returning) => {
                Plan::Full
            }
            Some(old_top) if top == old_top => Plan::Dirty,
            Some(old_top) => {
                let rows = target.rows();
                let entering = if distance(old_top, top) <= rows + HISTORY_ROWS {
                    offset(old_top, rows)
                } else {
                    top.saturating_sub(offset(0, HISTORY_ROWS))
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
        match plan {
            Plan::Stream { from } => std::iter::once(from..viewport.end).collect(),
            Plan::Full => vec![viewport],
            Plan::Scroll {
                old_top,
                entering,
                all,
            } => {
                let leaving_end = viewport.start.min(offset(old_top, target.rows()));
                let mut needed = vec![old_top..leaving_end, entering..viewport.end];
                if all {
                    needed.push(viewport);
                } else {
                    needed.extend(dirty());
                }
                needed
            }
            Plan::Dirty => dirty(),
        }
    }

    /// The rows to fetch before the pane can be drawn, as GetLines ranges.
    pub fn wanted(&self) -> Vec<Range<StableRowIndex>> {
        let Some(target) = self
            .target
            .as_ref()
            .filter(|target| self.visible && !self.fetching && !self.waiting_for_size(target))
        else {
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

    /// Draws what changed once every row it needs is here.
    pub fn render(&mut self) -> Option<Output> {
        if !self.visible || self.fetching || !self.wanted().is_empty() {
            return None;
        }
        let target = self.target.clone()?;
        if self.waiting_for_size(&target) {
            return None;
        }
        let plan = self.plan(&target);
        let viewport = target.viewport();
        let changed_rows = self.dirty.range(viewport.clone()).count();
        if plan == Plan::Dirty && changed_rows == 0 && self.drawn.as_ref() == Some(&target) {
            self.lines.clear();
            self.absent.clear();
            self.dirty.clear();
            return None;
        }

        let rows = target.rows();
        let cols = target.dims.cols;
        let top = target.top();
        let mut out = String::from("\x1b[?25l");
        if !self.shown.painted {
            // Whatever the terminal showed before (another pane, perhaps in
            // its alternate screen) goes: back to the primary screen, wiped
            // with its history. Pastes are bracketed from here on, so the
            // session can hand each one to the server whole, which brackets
            // it again only if the pane's program asked.
            out.push_str("\x1b[?1049l\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[?2004h");
        }
        if target.alt && self.shown.alt.is_none() {
            out.push_str("\x1b[?1049h");
        } else if !target.alt && self.shown.alt.is_some() {
            out.push_str("\x1b[?1049l");
        }
        let line = |row: StableRowIndex| self.lines.get(&row);
        let at = |row: StableRowIndex, from: StableRowIndex| distance(from, row);
        match plan {
            Plan::Stream { from } => {
                out.push_str("\x1b[H");
                let mut first = true;
                for row in from..viewport.end {
                    if row < top && line(row).is_none() {
                        continue;
                    }
                    if !first {
                        out.push_str("\r\n");
                    }
                    first = false;
                    render::paint_line(&mut out, line(row), cols);
                }
            }
            Plan::Full => {
                for row in viewport.clone() {
                    render::move_to(&mut out, at(row, top), 0);
                    render::paint_line(&mut out, line(row), cols);
                }
            }
            Plan::Scroll {
                old_top,
                entering,
                all,
            } => {
                for row in old_top..top.min(offset(old_top, rows)) {
                    render::move_to(&mut out, at(row, old_top), 0);
                    render::paint_line(&mut out, line(row), cols);
                }
                render::move_to(&mut out, rows.saturating_sub(1), 0);
                for row in entering..viewport.end {
                    out.push('\n');
                    render::paint_line(&mut out, line(row), cols);
                }
                for row in top..entering.min(viewport.end) {
                    if all || self.dirty.contains(&row) {
                        render::move_to(&mut out, at(row, top), 0);
                        render::paint_line(&mut out, line(row), cols);
                    }
                }
            }
            Plan::Dirty => {
                for row in self.dirty.range(viewport.clone()) {
                    render::move_to(&mut out, at(*row, top), 0);
                    render::paint_line(&mut out, line(*row), cols);
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
        Some(Output {
            rows,
            cols,
            bytes: out.into_bytes(),
        })
    }
}
