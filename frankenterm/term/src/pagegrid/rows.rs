//! The rows of a `Screen` (B3.4): legacy `Line`s, or PageGrid pages behind
//! `FT_GRID_ENGINE=page`.
//!
//! [`Rows`] stands in for the `VecDeque<Line>` the screen held, with the
//! same method names, so legacy code reads and writes it unchanged. In page
//! mode ([`PageRows`]):
//! - reads see legacy `Line` views ([`super::view::line_view`]), built when
//!   first read and kept until the row changes;
//! - a row edited through `&mut Line` keeps that view as its content until a
//!   native write stores it back into the page ([`PageRows::page_row`]);
//! - an insertion or removal of rows other than the page engine's own scroll
//!   operations turns the rows back into legacy `Line`s for good
//!   ([`Rows::legacy_mut`]). Those come from resize, reflow, cold seams,
//!   recovery and erasing the scrollback, which later B3 beads make
//!   page-native.

use super::list::{PageList, Scrolled};
use super::page::Page;
use super::view::{line_view, store_line};
use crate::StableRowIndex;
use frankenterm_surface::{Line, SequenceNo};
use std::collections::vec_deque;
use std::collections::VecDeque;
use std::ops::{Bound, Index, IndexMut, Range, RangeBounds};
use std::sync::OnceLock;

/// A retained row's legacy view.
#[derive(Debug, Default)]
struct RowView {
    /// Built when first read; dropped when the page row changes.
    line: OnceLock<Line>,
    /// A legacy operation edited the row through `&mut Line`: the view, not
    /// the page row, is the row's content.
    edited: bool,
}

/// The rows of a page-engine screen: every retained row in a [`PageList`],
/// oldest first, and a view slot for each.
#[derive(Debug)]
pub struct PageRows {
    list: PageList,
    views: VecDeque<RowView>,
}

impl PageRows {
    /// No rows yet; [`Self::scroll`] adds them.
    pub fn new(cols: u16, viewport_rows: usize, hot_cap: usize) -> Self {
        Self {
            list: PageList::new(cols, viewport_rows, hot_cap, 1),
            views: VecDeque::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.views.len()
    }

    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }

    pub fn list(&self) -> &PageList {
        &self.list
    }

    fn stable(&self, index: usize) -> StableRowIndex {
        self.list.first_stable() + index as StableRowIndex
    }

    fn build_view(&self, index: usize) -> Line {
        let stable = self.stable(index);
        let row = self.list.row(stable).expect("a retained row");
        let seqno = self
            .list
            .effective_seqno(stable)
            .expect("a retained row has a seqno");
        line_view(row.page, row.row, seqno)
    }

    fn ensure_view(&mut self, index: usize) {
        if self.views[index].line.get().is_none() {
            let line = self.build_view(index);
            let _ = self.views[index].line.set(line);
        }
    }

    /// Row `index` as a legacy `Line`.
    pub fn line(&self, index: usize) -> &Line {
        self.views[index]
            .line
            .get_or_init(|| self.build_view(index))
    }

    /// Row `index` for a legacy edit: its view becomes the row's content.
    pub fn line_mut(&mut self, index: usize) -> &mut Line {
        self.ensure_view(index);
        let view = &mut self.views[index];
        view.edited = true;
        view.line.get_mut().expect("the view was just built")
    }

    /// Rows `range` for legacy edits, as [`Self::line_mut`].
    pub fn lines_mut(&mut self, range: Range<usize>) -> Vec<&mut Line> {
        for index in range.clone() {
            self.ensure_view(index);
        }
        self.views
            .range_mut(range)
            .map(|view| {
                view.edited = true;
                view.line.get_mut().expect("the view was built above")
            })
            .collect()
    }

    /// Row `index`'s legacy `Line::len()`, without building its view.
    pub fn row_len(&self, index: usize) -> usize {
        let view = &self.views[index];
        if view.edited {
            return view.line.get().map_or(0, Line::len);
        }
        let row = self.list.row(self.stable(index)).expect("a retained row");
        row.page.row_len(row.row)
    }

    /// Whether a legacy edit left row `index`'s content in its view.
    pub fn is_edited(&self, index: usize) -> bool {
        self.views[index].edited
    }

    /// Row `index` for a native read, with no view built or dropped. `None`
    /// when an edit left the row's content in its view.
    pub fn page_row_ref(&self, index: usize) -> Option<(&Page, u32)> {
        if self.views[index].edited {
            return None;
        }
        let row = self.list.row(self.stable(index))?;
        Some((row.page, row.row))
    }

    /// Row `index` for a native write. A view edited through
    /// [`Self::line_mut`] is stored back into the page first, and the cached
    /// view is dropped, since the write changes the row. Returns `None`,
    /// leaving the view as the row's content, when an edited view does not
    /// fit the page (see [`store_line`]), or holds a hyperlink after an
    /// implicit-link scan: legacy's next edit strips implicit links
    /// (`Line::invalidate_implicit_hyperlinks`), which a page row cannot
    /// record, so the row stays a view until an edit through it has.
    pub fn page_row(&mut self, index: usize) -> Option<(&mut Page, u32)> {
        let stable = self.stable(index);
        let view = &mut self.views[index];
        let line = view.line.take();
        if std::mem::take(&mut view.edited) {
            let line = line.expect("an edited row has its view");
            let scanned_links = line.implicit_hyperlinks_are_scanned() && line.has_hyperlink();
            let (page, row) = self.list.row_mut(stable).expect("a retained row");
            if scanned_links || !store_line(page, row, &line) {
                let view = &mut self.views[index];
                let _ = view.line.set(line);
                view.edited = true;
                return None;
            }
        }
        self.list.row_mut(stable)
    }

    /// Appends `n` empty rows carrying `seqno`, as legacy's full-screen
    /// scroll does, trimming the rows past the hot cap from the front. The
    /// caller has offered those rows to the cold sink already: the first
    /// `admitted` were admitted and the next was refused (see
    /// [`PageList::scroll`]). With `hold`, nothing is trimmed.
    pub fn scroll(&mut self, n: usize, seqno: SequenceNo, hold: bool, admitted: usize) -> Scrolled {
        let mut left = admitted;
        let scrolled = self.list.scroll(n, seqno, hold, |_| {
            if left > 0 {
                left -= 1;
                true
            } else {
                false
            }
        });
        for _ in 0..n {
            self.views.push_back(RowView::default());
        }
        for _ in 0..scrolled.trimmed {
            self.views.pop_front();
        }
        debug_assert_eq!(self.views.len(), self.list.retained_rows());
        scrolled
    }

    /// Legacy's `physical_rows + hot_scrollback_size()`, which can change
    /// between scrolls (a config change or a resize).
    pub fn set_hot_cap(&mut self, hot_cap: usize) {
        self.list.set_hot_cap(hot_cap);
    }

    /// [`Self::scroll`] for a scroll region with top margin 0 above `footer`
    /// rows (ADR D5): scrollback is fed the same way, and the `n` new rows
    /// end up above the footer, which moves down with its views.
    pub fn scroll_above_footer(
        &mut self,
        n: usize,
        footer: usize,
        seqno: SequenceNo,
        hold: bool,
        admitted: usize,
    ) -> Scrolled {
        let mut left = admitted;
        let scrolled = self.list.scroll_above_footer(n, footer, seqno, hold, |_| {
            if left > 0 {
                left -= 1;
                true
            } else {
                false
            }
        });
        for _ in 0..n {
            self.views.push_back(RowView::default());
        }
        for _ in 0..scrolled.trimmed {
            self.views.pop_front();
        }
        if footer > 0 && n > 0 {
            // The list moved the n new rows above the footer.
            let len = self.views.len();
            let start = len.saturating_sub(footer + n);
            for _ in 0..n {
                let view = self.views.pop_back().expect("a new row's view");
                self.views.insert(start, view);
            }
        }
        debug_assert_eq!(self.views.len(), self.list.retained_rows());
        scrolled
    }

    /// The rows `range` scroll up by `n` within the range: the top `n` are
    /// cleared with `seqno` and move to the bottom, the others move up with
    /// their views. Nothing enters scrollback.
    pub fn rotate_up(&mut self, range: Range<usize>, n: usize, seqno: SequenceNo) {
        let n = n.min(range.len());
        if n == 0 {
            return;
        }
        let stable = self.stable(range.start)..self.stable(range.end);
        self.list.rotate_up(stable, n, seqno);
        for _ in 0..n {
            self.views.remove(range.start);
            self.views.insert(range.end - 1, RowView::default());
        }
    }

    /// The reverse of [`Self::rotate_up`]: the bottom `n` rows of `range` are
    /// cleared and move to its top.
    pub fn rotate_down(&mut self, range: Range<usize>, n: usize, seqno: SequenceNo) {
        let n = n.min(range.len());
        if n == 0 {
            return;
        }
        let stable = self.stable(range.start)..self.stable(range.end);
        self.list.rotate_down(stable, n, seqno);
        for _ in 0..n {
            self.views.remove(range.end - 1);
            self.views.insert(range.start, RowView::default());
        }
    }

    /// Raises the seqno of every row to at least `seqno`, as a palette change
    /// does; cached views take it too.
    pub fn mark_all_changed(&mut self, seqno: SequenceNo) {
        self.list.mark_all_changed(seqno);
        for view in self.views.iter_mut() {
            if view.edited {
                if let Some(line) = view.line.get_mut() {
                    line.update_last_change_seqno(seqno);
                }
            } else {
                view.line.take();
            }
        }
    }

    /// Every row as a legacy `Line`, oldest first.
    pub fn into_lines(mut self) -> VecDeque<Line> {
        let mut lines = VecDeque::with_capacity(self.views.len());
        for index in 0..self.views.len() {
            let line = match self.views[index].line.take() {
                Some(line) => line,
                None => self.build_view(index),
            };
            lines.push_back(line);
        }
        lines
    }

    /// Every row as a legacy `Line`, oldest first, leaving the rows as they
    /// are.
    pub fn to_lines(&self) -> VecDeque<Line> {
        (0..self.len())
            .map(|index| self.line(index).clone())
            .collect()
    }
}

/// The rows of a `Screen`; see the module documentation.
#[derive(Debug)]
pub enum Rows {
    Legacy(VecDeque<Line>),
    Page(Box<PageRows>),
}

impl Default for Rows {
    fn default() -> Self {
        Rows::Legacy(VecDeque::new())
    }
}

/// A page-mode clone is a legacy clone of the same rows: active pages are
/// never shared (B3.3), and only tests clone a screen.
impl Clone for Rows {
    fn clone(&self) -> Self {
        match self {
            Rows::Legacy(lines) => Rows::Legacy(lines.clone()),
            Rows::Page(rows) => Rows::Legacy(rows.to_lines()),
        }
    }
}

/// Rows compare as the legacy `Line`s they hold or stand for.
impl PartialEq for Rows {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Rows::Legacy(left), Rows::Legacy(right)) => left == right,
            _ => self.len() == other.len() && self.iter().zip(other.iter()).all(|(a, b)| a == b),
        }
    }
}

impl Rows {
    pub fn len(&self) -> usize {
        match self {
            Rows::Legacy(lines) => lines.len(),
            Rows::Page(rows) => rows.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn page(&self) -> Option<&PageRows> {
        match self {
            Rows::Legacy(_) => None,
            Rows::Page(rows) => Some(rows),
        }
    }

    pub fn page_mut(&mut self) -> Option<&mut PageRows> {
        match self {
            Rows::Legacy(_) => None,
            Rows::Page(rows) => Some(rows),
        }
    }

    /// The legacy `Line`s, while the rows are held that way.
    pub fn legacy(&self) -> Option<&VecDeque<Line>> {
        match self {
            Rows::Legacy(lines) => Some(lines),
            Rows::Page(_) => None,
        }
    }

    /// The rows as legacy `Line`s, for an edit the page engine does not
    /// perform itself. Page-mode rows become legacy `Line`s for good.
    pub fn legacy_mut(&mut self) -> &mut VecDeque<Line> {
        if matches!(self, Rows::Page(_)) {
            if let Rows::Page(rows) = std::mem::take(self) {
                log::debug!(
                    target: "frankenterm_term::screen::grid_engine",
                    "page engine: {} rows return to legacy storage",
                    rows.len()
                );
                *self = Rows::Legacy(rows.into_lines());
            }
        }
        match self {
            Rows::Legacy(lines) => lines,
            Rows::Page(_) => unreachable!("page rows were just converted"),
        }
    }

    /// The legacy `Line`s that resize reflow reads. Resize returns page rows
    /// to legacy storage before it reflows, and the off-thread reflow
    /// preparation is never captured from page rows, so reflow only ever
    /// meets legacy rows (until page-wise reflow, B3.6).
    pub fn reflow_source(&self) -> &VecDeque<Line> {
        self.legacy()
            .expect("resize reflows page-engine rows through legacy storage")
    }

    /// The rows as two slices, while they are held as legacy `Line`s.
    pub fn legacy_slices(&self) -> Option<(&[Line], &[Line])> {
        self.legacy().map(VecDeque::as_slices)
    }

    pub fn get(&self, index: usize) -> Option<&Line> {
        match self {
            Rows::Legacy(lines) => lines.get(index),
            Rows::Page(rows) => (index < rows.len()).then(|| rows.line(index)),
        }
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut Line> {
        match self {
            Rows::Legacy(lines) => lines.get_mut(index),
            Rows::Page(rows) => {
                if index < rows.len() {
                    Some(rows.line_mut(index))
                } else {
                    None
                }
            }
        }
    }

    pub fn front(&self) -> Option<&Line> {
        self.get(0)
    }

    pub fn back(&self) -> Option<&Line> {
        self.len().checked_sub(1).and_then(|index| self.get(index))
    }

    pub fn back_mut(&mut self) -> Option<&mut Line> {
        let index = self.len().checked_sub(1)?;
        self.get_mut(index)
    }

    pub fn iter(&self) -> RowsIter<'_> {
        match self {
            Rows::Legacy(lines) => RowsIter::Legacy(lines.iter()),
            Rows::Page(rows) => RowsIter::Page {
                rows,
                range: 0..rows.len(),
            },
        }
    }

    pub fn range<R: RangeBounds<usize>>(&self, range: R) -> RowsIter<'_> {
        match self {
            Rows::Legacy(lines) => RowsIter::Legacy(lines.range(range)),
            Rows::Page(rows) => RowsIter::Page {
                rows,
                range: bounded(range, rows.len()),
            },
        }
    }

    pub fn iter_mut(&mut self) -> RowsIterMut<'_> {
        match self {
            Rows::Legacy(lines) => RowsIterMut::Legacy(lines.iter_mut()),
            Rows::Page(rows) => {
                let len = rows.len();
                RowsIterMut::Page(rows.lines_mut(0..len).into_iter())
            }
        }
    }

    /// Rows `range` for editing. Page-mode rows outside it keep their
    /// state; those inside become edited views.
    pub fn range_mut(&mut self, range: Range<usize>) -> RowsIterMut<'_> {
        match self {
            Rows::Legacy(lines) => RowsIterMut::Legacy(lines.range_mut(range)),
            Rows::Page(rows) => RowsIterMut::Page(rows.lines_mut(range).into_iter()),
        }
    }

    pub fn capacity(&self) -> usize {
        match self {
            Rows::Legacy(lines) => lines.capacity(),
            Rows::Page(rows) => rows.len(),
        }
    }

    pub fn shrink_to_fit(&mut self) {
        if let Rows::Legacy(lines) = self {
            lines.shrink_to_fit();
        }
    }

    pub fn reserve(&mut self, additional: usize) {
        if let Rows::Legacy(lines) = self {
            lines.reserve(additional);
        }
    }

    pub fn insert(&mut self, index: usize, line: Line) {
        self.legacy_mut().insert(index, line);
    }

    pub fn remove(&mut self, index: usize) -> Option<Line> {
        self.legacy_mut().remove(index)
    }

    pub fn push_back(&mut self, line: Line) {
        self.legacy_mut().push_back(line);
    }

    pub fn push_front(&mut self, line: Line) {
        self.legacy_mut().push_front(line);
    }

    pub fn pop_front(&mut self) -> Option<Line> {
        self.legacy_mut().pop_front()
    }

    pub fn pop_back(&mut self) -> Option<Line> {
        self.legacy_mut().pop_back()
    }

    pub fn truncate(&mut self, len: usize) {
        self.legacy_mut().truncate(len);
    }

    pub fn clear(&mut self) {
        self.legacy_mut().clear();
    }

    pub fn drain<R: RangeBounds<usize>>(&mut self, range: R) -> vec_deque::Drain<'_, Line> {
        self.legacy_mut().drain(range)
    }
}

impl Extend<Line> for Rows {
    fn extend<T: IntoIterator<Item = Line>>(&mut self, iter: T) {
        self.legacy_mut().extend(iter);
    }
}

impl Index<usize> for Rows {
    type Output = Line;

    fn index(&self, index: usize) -> &Line {
        match self {
            Rows::Legacy(lines) => &lines[index],
            Rows::Page(rows) => rows.line(index),
        }
    }
}

impl IndexMut<usize> for Rows {
    fn index_mut(&mut self, index: usize) -> &mut Line {
        match self {
            Rows::Legacy(lines) => &mut lines[index],
            Rows::Page(rows) => rows.line_mut(index),
        }
    }
}

impl<'a> IntoIterator for &'a mut Rows {
    type Item = &'a mut Line;
    type IntoIter = RowsIterMut<'a>;

    fn into_iter(self) -> RowsIterMut<'a> {
        self.iter_mut()
    }
}

impl<'a> IntoIterator for &'a Rows {
    type Item = &'a Line;
    type IntoIter = RowsIter<'a>;

    fn into_iter(self) -> RowsIter<'a> {
        self.iter()
    }
}

/// `range` as `start..end` within `0..len`.
fn bounded<R: RangeBounds<usize>>(range: R, len: usize) -> Range<usize> {
    let start = match range.start_bound() {
        Bound::Included(&start) => start,
        Bound::Excluded(&start) => start + 1,
        Bound::Unbounded => 0,
    };
    let end = match range.end_bound() {
        Bound::Included(&end) => end + 1,
        Bound::Excluded(&end) => end,
        Bound::Unbounded => len,
    };
    assert!(
        start <= end && end <= len,
        "row range {}..{} out of {}",
        start,
        end,
        len
    );
    start..end
}

/// Rows, oldest first.
pub enum RowsIter<'a> {
    Legacy(vec_deque::Iter<'a, Line>),
    Page {
        rows: &'a PageRows,
        range: Range<usize>,
    },
}

impl<'a> Clone for RowsIter<'a> {
    fn clone(&self) -> Self {
        match self {
            RowsIter::Legacy(iter) => RowsIter::Legacy(iter.clone()),
            RowsIter::Page { rows, range } => RowsIter::Page {
                rows,
                range: range.clone(),
            },
        }
    }
}

impl<'a> Iterator for RowsIter<'a> {
    type Item = &'a Line;

    fn next(&mut self) -> Option<&'a Line> {
        match self {
            RowsIter::Legacy(iter) => iter.next(),
            RowsIter::Page { rows, range } => {
                let rows: &'a PageRows = rows;
                range.next().map(|index| rows.line(index))
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            RowsIter::Legacy(iter) => iter.size_hint(),
            RowsIter::Page { range, .. } => range.size_hint(),
        }
    }
}

impl<'a> DoubleEndedIterator for RowsIter<'a> {
    fn next_back(&mut self) -> Option<&'a Line> {
        match self {
            RowsIter::Legacy(iter) => iter.next_back(),
            RowsIter::Page { rows, range } => {
                let rows: &'a PageRows = rows;
                range.next_back().map(|index| rows.line(index))
            }
        }
    }
}

impl<'a> ExactSizeIterator for RowsIter<'a> {}

/// Rows for editing, oldest first.
pub enum RowsIterMut<'a> {
    Legacy(vec_deque::IterMut<'a, Line>),
    Page(std::vec::IntoIter<&'a mut Line>),
}

impl<'a> Iterator for RowsIterMut<'a> {
    type Item = &'a mut Line;

    fn next(&mut self) -> Option<&'a mut Line> {
        match self {
            RowsIterMut::Legacy(iter) => iter.next(),
            RowsIterMut::Page(iter) => iter.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            RowsIterMut::Legacy(iter) => iter.size_hint(),
            RowsIterMut::Page(iter) => iter.size_hint(),
        }
    }
}

impl<'a> DoubleEndedIterator for RowsIterMut<'a> {
    fn next_back(&mut self) -> Option<&'a mut Line> {
        match self {
            RowsIterMut::Legacy(iter) => iter.next_back(),
            RowsIterMut::Page(iter) => iter.next_back(),
        }
    }
}

impl<'a> ExactSizeIterator for RowsIterMut<'a> {}

#[cfg(test)]
mod tests {
    use crate::color::ColorPalette;
    use crate::config::GridEngine;
    use crate::{Terminal, TerminalConfiguration, TerminalSize};
    use frankenterm_bidi::ParagraphDirectionHint;
    use frankenterm_cell::CellAttributes;
    use std::sync::Arc;

    #[derive(Debug)]
    struct Config(GridEngine);

    impl TerminalConfiguration for Config {
        fn scrollback_size(&self) -> usize {
            20
        }

        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn grid_engine(&self) -> GridEngine {
            self.0
        }
    }

    fn terminal(engine: GridEngine) -> Terminal {
        Terminal::new(
            TerminalSize {
                rows: 6,
                cols: 10,
                pixel_width: 80,
                pixel_height: 96,
                dpi: 96,
            },
            Arc::new(Config(engine)),
            "frankenterm-pagegrid-rows",
            "0",
            Box::new(std::io::sink()),
        )
    }

    type DecodedRow = (
        Vec<(String, usize, CellAttributes)>,
        bool,
        (bool, ParagraphDirectionHint),
        (bool, bool, bool),
    );

    /// Every retained row as its visible cells, wrap, bidi and double size.
    fn decoded(terminal: &Terminal) -> Vec<DecodedRow> {
        terminal
            .screen()
            .all_lines()
            .iter()
            .map(|line| {
                (
                    line.visible_cells()
                        .map(|cell| (cell.str().to_string(), cell.width(), cell.attrs().clone()))
                        .collect(),
                    line.last_cell_was_wrapped(),
                    line.bidi_info(),
                    (
                        line.is_double_width(),
                        line.is_double_height_top(),
                        line.is_double_height_bottom(),
                    ),
                )
            })
            .collect()
    }

    fn seq(lines: usize) -> Vec<u8> {
        (1..=lines)
            .flat_map(|n| format!("{}\n", n).into_bytes())
            .collect()
    }

    /// ft-yccm0.3.3.4: page-engine rows decode exactly as legacy rows after
    /// the hot path's streams, whole and in 7-byte chunks, and the screen
    /// keeps its rows in pages; a resize returns them to legacy storage and
    /// they still decode the same.
    #[test]
    fn page_rows_decode_as_legacy_rows() {
        let streams: Vec<(&str, Vec<u8>)> = vec![
            ("seq_staircase", seq(120)),
            (
                "t0_cells",
                (0..90)
                    .flat_map(|n| {
                        format!("\x1b[38;5;{}m\x1b[48;5;{}m\u{1f600}", n % 256, (n * 7) % 256)
                            .into_bytes()
                    })
                    .collect(),
            ),
            (
                "wrapping_crlf",
                b"the quick brown fox jumps over the lazy dog\r\nand again, longer than a row\r\n"
                    .to_vec(),
            ),
            (
                "regions",
                b"a\r\nb\r\nc\r\nd\r\ne\r\nf\x1b[2;5r\x1b[5;1Hx\ny\nz\n\x1b[2;1H\x1bM\x1bMq\x1b[r\x1b[1;4rtop\n\n\n\nbottom"
                    .to_vec(),
            ),
            (
                "erases",
                b"0123456789abc\r\x1b[3C\x1b[K\x1b[2;1Hxyz\x1b[1K\x1b[44m\x1b[2X\x1b[2K\x1b[m\x1b[J\x1b[1J"
                    .to_vec(),
            ),
            (
                "edits",
                b"abcdefgh\r\x1b[2@\x1b[3P\x1b[2L\x1b[M\x1b#8\x1b[3;3H\x1b[4hins\x1b[4l\tq"
                    .to_vec(),
            ),
            (
                "insert_delete_styled",
                "\x1b[44mab\u{4e2d}cd\x1b[m\r\x1b[C\x1b[@\x1b[41m\x1b[2P\x1b[m\x1b[2;1H\u{4e2d}\u{6587}x\x1b[2;2H\x1b[4hy\u{1f600}\x1b[4l\x1b[3;9Hq\x1b[1;1H\x1b[9@\x1b[99P"
                    .as_bytes()
                    .to_vec(),
            ),
            (
                "left_right_margins",
                b"0123456789\r\nabcdefghij\r\nklmnopqrst\r\nuvwxyzABCD\r\nEFGHIJKLMN\x1b[?69h\x1b[3;8s\x1b[2;5r\x1b[3;4H\x1b[S\x1b[44m\x1b[T\x1b[2L\x1b[m\x1b[M\x1b[2P\x1b[@\x1b[5;8Hz\nw\x1b[?69l\x1b[r"
                    .to_vec(),
            ),
            (
                "alignment_and_sizes",
                b"x\x1b#8\x1b[2;1H\x1b#3\x1b[3;1H\x1b#4\x1b[4;1H\x1b#6\x1b[4;1H\x1b#5\x1b[1;1H\x1b#6\x1b[2J"
                    .to_vec(),
            ),
            (
                "tabs_and_origin",
                b"\x1b[3g\x1b[1;4H\x1bH\x1b[1;8H\x1bH\r\ta\tb\tc\x1b[2;5r\x1b[?6h\x1b[Ho\x1b[2Z\x1b[3Ip\x1b[?6l\x1b[r\x1b[g"
                    .to_vec(),
            ),
            (
                "alternate_screen",
                b"main\r\n\x1b[?1049halt one\r\nalt two\n\n\n\n\n\nscrolled\x1b[?1049lback".to_vec(),
            ),
            (
                "alternate_screen_active",
                b"main\r\n\x1b[?1049halt\x1b[2;3r\n\n\n\x1bMx\x1b[r\x1b[?47l\x1b[?47h".to_vec(),
            ),
            (
                "osc133_zones",
                b"\x1b]133;A\x07$ \x1b]133;B\x07ls -l\x1b]133;C\x07\r\nout one\r\nout two\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ "
                    .to_vec(),
            ),
            (
                "repeat",
                "ab\x1b[3b\r\n\u{4e2d} \x1b[2b\r\n\x1b[5b\x1b[44mz\x1b[20b\x1b[m"
                    .as_bytes()
                    .to_vec(),
            ),
            (
                "graphemes_at_the_margin",
                "\x1b[1;9H\u{1f468}\u{200d}\u{1f469}\x1b[2;10He\u{301}\u{301}\x1b[3;1H\u{2764}\u{fe0f}\u{200d}"
                    .as_bytes()
                    .to_vec(),
            ),
            (
                // ft-b35o7: wide graphemes on the last column wrap whole,
                // over a cleared spacer; without autowrap they are dropped
                // (or print narrow without their VS16); at the bottom row
                // the wrap scrolls.
                "wide_at_the_right_margin",
                "\x1b]1337;UnicodeVersion=14\x07\x1b[1;10H\u{1f600}\x1b[3;1HZZZZZZZZZZ\rAAAAAAAAA\u{1f600}B\x1b[?7l\x1b[5;10H\u{1f600}\x1b[5;9Hx\u{2764}\u{fe0f}\x1b[?7h\x1b[6;10H\u{2764}\u{fe0f}"
                    .as_bytes()
                    .to_vec(),
            ),
            (
                // ft-0b8ux: without autowrap, continuations of the last
                // column's grapheme (split across reads by the 7-byte
                // chunks) join it; a CUP onto the last column's text, and
                // onto the column before it, picks the cell as Ghostty does.
                "continuations_at_the_right_margin_without_autowrap",
                "\x1b[?7l\x1b[1;10He\u{301}\x1b[2;9H\u{1f468}\u{200d}\u{1f469}\x1b[3;10HZ\x1b[3;10H\u{301}\x1b[4;8Hxy\x1b[4;9H\u{301}"
                    .as_bytes()
                    .to_vec(),
            ),
            (
                "graphemes",
                "e\u{301} \u{4e2d}\u{6587} \u{1f468}\u{200d}\u{1f469} \u{2764}\u{fe0f} ab\u{301}"
                    .as_bytes()
                    .to_vec(),
            ),
        ];
        for (name, bytes) in &streams {
            for chunk in [bytes.len().max(1), 7] {
                let mut legacy = terminal(GridEngine::Legacy);
                let mut page = terminal(GridEngine::Page);
                for piece in bytes.chunks(chunk) {
                    legacy.advance_bytes(piece);
                    page.advance_bytes(piece);
                }
                let what = format!("{} in {}-byte chunks", name, chunk);
                assert_eq!(decoded(&page), decoded(&legacy), "{}", what);
                assert_eq!(page.cursor_pos(), legacy.cursor_pos(), "{}", what);
                assert_eq!(
                    page.is_alt_screen_active(),
                    legacy.is_alt_screen_active(),
                    "{}",
                    what
                );
                assert_eq!(page.screen().grid_engine(), GridEngine::Page, "{}", what);
                // Every operation in these streams runs natively: no row is
                // left held by an edited view.
                assert_eq!(page.screen().rows_held_by_views(), 0, "{}", what);
                assert_eq!(
                    page.get_semantic_zones().unwrap(),
                    legacy.get_semantic_zones().unwrap(),
                    "{} zones",
                    what
                );

                let size = TerminalSize {
                    rows: 4,
                    cols: 7,
                    pixel_width: 56,
                    pixel_height: 64,
                    dpi: 96,
                };
                legacy.resize(size);
                page.resize(size);
                assert_eq!(page.screen().grid_engine(), GridEngine::Legacy, "{}", what);
                assert_eq!(decoded(&page), decoded(&legacy), "{} after resize", what);
            }
        }
    }
}
