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
//!   ([`Rows::legacy_mut`]). Those come from width reflow, cold seams,
//!   recovery and other structural edits, which later B3 beads make
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

/// A retained row's legacy view. Boxed: every scroll pushes one view and pops
/// another, and every native write takes the line out, so the slot stays a
/// pointer wide rather than a whole `Line`.
#[derive(Debug, Default)]
struct RowView {
    /// Built when first read; dropped when the page row changes.
    line: OnceLock<Box<Line>>,
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
        let mut line = line_view(row.page, row.row, seqno);
        // A row enters scrollback only through a scroll, which legacy
        // compresses as it goes and the page engine compresses here, when the
        // row is first read (`Screen::page_compress_for_scrollback`).
        if index + self.list.viewport_rows() < self.views.len()
            && crate::screen::scrollback_compression_enabled()
        {
            line.compress_for_scrollback();
        }
        line
    }

    /// Drops row `index`'s cached view, unless an edit made it the row's
    /// content; the next read builds it again.
    pub fn drop_view(&mut self, index: usize) {
        let view = &mut self.views[index];
        if !view.edited {
            view.line.take();
        }
    }

    fn ensure_view(&mut self, index: usize) {
        if self.views[index].line.get().is_none() {
            let line = Box::new(self.build_view(index));
            let _ = self.views[index].line.set(line);
        }
    }

    /// Row `index` as a legacy `Line`.
    pub fn line(&self, index: usize) -> &Line {
        self.views[index]
            .line
            .get_or_init(|| Box::new(self.build_view(index)))
    }

    /// Charge the cell decoding a line snapshot must do before allocating any
    /// new views. A cached or edited row already owns its legacy storage.
    #[cfg(feature = "use_serde")]
    pub(crate) fn charge_snapshot_views(
        &self,
        range: Range<usize>,
        bytes_left: &mut usize,
        work_left: &mut usize,
    ) -> Option<()> {
        use super::cell::{Glyph, PackedCell};
        use frankenterm_cell::{image::ImageCell, Cell};

        for index in range {
            if self.views[index].line.get().is_some() {
                continue;
            }
            let (page, row) = self.page_row_ref(index)?;
            let words = page.row_words(row);
            *work_left = work_left.checked_sub(words.len().max(1))?;
            let cell_bytes = words.len().checked_mul(Cell::MAX_DECODED_BASE_BYTES)?;
            *bytes_left = bytes_left.checked_sub(size_of::<Line>().checked_add(cell_bytes)?)?;
            for (col, &word) in words.iter().enumerate() {
                let cell = PackedCell::from_bits(word);
                if cell.has_grapheme() {
                    if let Glyph::Cluster(text) = page.glyph(row, col) {
                        *bytes_left = bytes_left.checked_sub(text.len())?;
                        *work_left = work_left.checked_sub(text.len())?;
                    }
                }
                if let Some(images) = page.cell_images(row, col) {
                    // Decoding clones the already-ordered placement slice once.
                    let per_image = size_of::<ImageCell>() + size_of::<Box<ImageCell>>();
                    let bytes = images.len().checked_mul(per_image)?;
                    *bytes_left = bytes_left.checked_sub(bytes)?;
                    *work_left = work_left.checked_sub(images.len())?;
                }
            }
        }
        Some(())
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
                &mut **view.line.get_mut().expect("the view was built above")
            })
            .collect()
    }

    /// Row `index`'s legacy `Line::len()`, without building its view.
    pub fn row_len(&self, index: usize) -> usize {
        let view = &self.views[index];
        if view.edited {
            return view.line.get().map_or(0, |line| line.len());
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

    /// The effective change sequence without constructing a legacy view.
    /// List-wide floors (selection damage, for example) are included.
    pub fn effective_seqno(&self, index: usize) -> Option<SequenceNo> {
        let view = self.views.get(index)?;
        if view.edited {
            return view.line.get().map(|line| line.current_seqno());
        }
        self.list.effective_seqno(self.stable(index))
    }

    #[cfg(test)]
    pub(crate) fn cached_view_count(&self) -> usize {
        self.views
            .iter()
            .filter(|view| view.line.get().is_some())
            .count()
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
        let view = &self.views[index];
        if view.edited || view.line.get().is_some() {
            let stable = self.stable(index);
            return self.page_row_from_view(index, stable);
        }
        if index + 1 == self.views.len() {
            // The newest row, where every flood writes, is the tail page's
            // last: no need to locate it.
            return self.list.tail_row_mut();
        }
        self.list.row_mut(self.stable(index))
    }

    /// [`Self::page_row`] of a row with a view, out of line: storing an
    /// edited view back and dropping a cached one made the frame of every
    /// printed run's lookup, which almost never has a view.
    #[inline(never)]
    fn page_row_from_view(
        &mut self,
        index: usize,
        stable: StableRowIndex,
    ) -> Option<(&mut Page, u32)> {
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
        self.scroll_with_row_flags(n, seqno, hold, admitted, 0)
    }

    /// [`Self::scroll`] whose new rows carry the row flags `flags` (see
    /// [`PageList::scroll_with_row_flags`]).
    pub fn scroll_with_row_flags(
        &mut self,
        n: usize,
        seqno: SequenceNo,
        hold: bool,
        admitted: usize,
        flags: u64,
    ) -> Scrolled {
        let mut left = admitted;
        let scrolled = self.list.scroll_with_row_flags(n, seqno, hold, flags, |_| {
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

    /// [`Self::scroll_with_row_flags`] of one row with nothing held and
    /// every trimmed row admitted ([`PageList::scroll_line`]).
    pub fn scroll_line(&mut self, seqno: SequenceNo, flags: u64) -> Scrolled {
        let scrolled = self.list.scroll_line(seqno, flags);
        self.views.push_back(RowView::default());
        for _ in 0..scrolled.trimmed {
            self.views.pop_front();
        }
        debug_assert_eq!(self.views.len(), self.list.retained_rows());
        scrolled
    }

    /// The row [`Self::scroll_line`] just appended, before anything built
    /// its view: what [`Self::page_row`] of the last row returns then.
    pub fn fresh_tail_row_mut(&mut self) -> Option<(&mut Page, u32)> {
        debug_assert!(self
            .views
            .back()
            .is_some_and(|view| !view.edited && view.line.get().is_none()));
        self.list.tail_row_mut()
    }

    /// Legacy's `physical_rows + hot_scrollback_size()`, which can change
    /// between scrolls (a config change or a resize).
    pub fn set_hot_cap(&mut self, hot_cap: usize) {
        self.list.set_hot_cap(hot_cap);
    }

    /// Applies a same-width resize's retention limits without evicting rows.
    pub(crate) fn set_resize_geometry(&mut self, viewport_rows: usize, hot_cap: usize) {
        self.list.set_hot_cap(hot_cap);
        self.list.set_viewport_rows(viewport_rows);
    }

    /// Discards only a resize's trailing rows and their views. Edited views in
    /// the retained prefix remain authoritative; resize does not flush them.
    pub(crate) fn truncate_for_resize(&mut self, len: usize) {
        self.list.truncate_for_resize(len);
        self.views.truncate(len);
        debug_assert_eq!(self.views.len(), self.list.retained_rows());
    }

    /// Stores a resize's padding directly in its new page row. Preserve an
    /// exceptional legacy view if its content cannot be represented there.
    fn push_for_resize(&mut self, line: Line) {
        self.list.push_for_resize(line.current_seqno());
        let (page, row) = self.list.tail_row_mut().expect("a row was just appended");
        let mut view = RowView::default();
        if (line.implicit_hyperlinks_are_scanned() && line.has_hyperlink())
            || !store_line(page, row, &line)
        {
            let _ = view.line.set(Box::new(line));
            view.edited = true;
        }
        self.views.push_back(view);
        debug_assert_eq!(self.views.len(), self.list.retained_rows());
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
            let len = self.views.len();
            let start = len.saturating_sub(footer + n);
            self.shift_views_down(start..len, n.min(len - start));
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
        if n == 1 {
            self.views.remove(range.start);
            self.views.insert(range.end - 1, RowView::default());
        } else {
            for source in range.start + n..range.end {
                self.views.swap(source - n, source);
            }
            for view in self.views.range_mut(range.end - n..range.end) {
                *view = RowView::default();
            }
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
        self.shift_views_down(range, n);
    }

    /// Move edited and cached views with the rows. Swapping only within the
    /// region leaves the history prefix in place even if the deque wraps.
    fn shift_views_down(&mut self, range: Range<usize>, n: usize) {
        if n == 1 {
            self.views.remove(range.end - 1);
            self.views.insert(range.start, RowView::default());
        } else {
            for source in (range.start..range.end - n).rev() {
                self.views.swap(source + n, source);
            }
            for view in self.views.range_mut(range.start..range.start + n) {
                *view = RowView::default();
            }
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
                Some(line) => *line,
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

    /// Inspect damage without materializing a page row's `Line` view.
    /// Edited views remain authoritative, and page/list sequence floors are
    /// applied before interpreting zero as "always changed", just as a
    /// materialized view would do.
    pub fn row_changed_since(&self, index: usize, seqno: SequenceNo) -> Option<bool> {
        match self {
            Rows::Legacy(lines) => lines.get(index).map(|line| line.changed_since(seqno)),
            Rows::Page(rows) => rows
                .effective_seqno(index)
                .map(|current| current == 0 || current > seqno),
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
        match self {
            Rows::Legacy(lines) => lines.shrink_to_fit(),
            Rows::Page(rows) => {
                rows.views.shrink_to_fit();
                rows.list.shrink_to_fit();
            }
        }
    }

    pub fn reserve(&mut self, additional: usize) {
        if let Rows::Legacy(lines) = self {
            lines.reserve(additional);
        }
    }

    /// Resize-only structural edits preserve packed rows when the width stays
    /// unchanged. Other edits retain their existing legacy fallback.
    pub(crate) fn truncate_for_resize(&mut self, len: usize) {
        match self {
            Rows::Legacy(lines) => lines.truncate(len),
            Rows::Page(rows) => rows.truncate_for_resize(len),
        }
    }

    pub(crate) fn push_for_resize(&mut self, line: Line) {
        match self {
            Rows::Legacy(lines) => lines.push_back(line),
            Rows::Page(rows) => rows.push_for_resize(line),
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

    /// Drop a prefix without constructing compatibility views or disturbing
    /// authoritative edited views in the retained suffix.
    pub(crate) fn discard_prefix(&mut self, count: usize) {
        match self {
            Rows::Legacy(lines) => {
                lines.drain(..count);
            }
            Rows::Page(rows) => {
                rows.list.discard_prefix(count);
                rows.views.drain(..count);
                debug_assert_eq!(rows.views.len(), rows.list.retained_rows());
            }
        }
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

    fn nth(&mut self, n: usize) -> Option<&'a Line> {
        match self {
            RowsIter::Legacy(iter) => iter.nth(n),
            RowsIter::Page { rows, range } => {
                let rows: &'a PageRows = rows;
                range.nth(n).map(|index| rows.line(index))
            }
        }
    }

    fn last(mut self) -> Option<&'a Line> {
        self.next_back()
    }

    fn count(self) -> usize {
        self.len()
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

    fn nth_back(&mut self, n: usize) -> Option<&'a Line> {
        match self {
            RowsIter::Legacy(iter) => iter.nth_back(n),
            RowsIter::Page { rows, range } => {
                let rows: &'a PageRows = rows;
                range.nth_back(n).map(|index| rows.line(index))
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
    use super::{PageRows, Rows};
    use crate::color::ColorPalette;
    use crate::config::GridEngine;
    use crate::{Terminal, TerminalConfiguration, TerminalSize};
    use frankenterm_bidi::ParagraphDirectionHint;
    use frankenterm_cell::CellAttributes;
    use frankenterm_surface::{Line, SequenceNo};
    use std::sync::Arc;

    fn packed_rows(sequences: &[SequenceNo]) -> Rows {
        let mut rows = PageRows::new(8, 3, sequences.len());
        for &sequence in sequences {
            rows.scroll(1, sequence, false, 0);
        }
        Rows::Page(Box::new(rows))
    }

    #[test]
    fn bulk_region_scroll_moves_edited_and_cached_views_with_their_packed_rows() {
        for up in [true, false] {
            for count in [0, 1, 2, 4, 7, 9, 12] {
                let mut rows = PageRows {
                    list: super::PageList::with_page_rows(8, 3, 5, 11, 1),
                    views: Default::default(),
                };
                for seqno in 1..=19 {
                    rows.scroll(1, seqno, false, 1);
                }
                let region = 1..10;
                for index in 0..rows.len() {
                    if index % 3 == 1 {
                        *rows.line_mut(index) = Line::from_text(
                            &format!("edited row {index} beyond the page width"),
                            &CellAttributes::blank(),
                            100 + index,
                            None,
                        );
                    } else if index % 3 == 2 {
                        let _ = rows.line(index);
                    }
                }
                let n = count.min(region.len());
                if up {
                    rows.rotate_up(region.clone(), count, 200);
                } else {
                    rows.rotate_down(region.clone(), count, 200);
                }
                rows.list.check_invariants().unwrap();
                for index in 0..rows.len() {
                    let source = if !region.contains(&index) {
                        Some(index)
                    } else if up {
                        (index + n < region.end).then_some(index + n)
                    } else {
                        index.checked_sub(n).filter(|src| region.contains(src))
                    };
                    let edited = source.is_some_and(|src| src % 3 == 1);
                    let cached = source.is_some_and(|src| src % 3 != 0);
                    assert_eq!(rows.is_edited(index), edited);
                    assert_eq!(rows.views[index].line.get().is_some(), cached);
                    let seqno = source.map_or(200, |src| if edited { 100 + src } else { 9 + src });
                    assert_eq!(rows.effective_seqno(index), Some(seqno));
                    if let Some(src) = source.filter(|_| edited) {
                        assert_eq!(
                            rows.line(index).as_str(),
                            format!("edited row {src} beyond the page width")
                        );
                    }
                }
            }
        }
    }

    #[test]
    // Exercise these iterator methods themselves, including lazy view creation.
    #[allow(
        clippy::iter_count,
        clippy::double_ended_iterator_last,
        clippy::reversed_empty_ranges
    )]
    fn page_row_iterators_materialize_only_yielded_rows() {
        let rows = packed_rows(&(0..128).collect::<Vec<_>>());
        assert_eq!(rows.iter().count(), 128);
        assert_eq!(rows.page().unwrap().cached_view_count(), 0);

        let viewport: Vec<_> = rows
            .iter()
            .enumerate()
            .skip(120)
            .take(3)
            .map(|(index, line)| (index, line.current_seqno()))
            .collect();
        assert_eq!(viewport, [(120, 120), (121, 121), (122, 122)]);
        assert_eq!(rows.page().unwrap().cached_view_count(), 3);
        assert_eq!(rows.range(8..40).last().map(Line::current_seqno), Some(39));
        assert_eq!(rows.page().unwrap().cached_view_count(), 4);

        let mut iter = rows.range(8..40);
        assert_eq!(iter.nth(4).map(Line::current_seqno), Some(12));
        assert_eq!(iter.nth_back(3).map(Line::current_seqno), Some(36));
        assert_eq!(iter.next().map(Line::current_seqno), Some(13));
        assert_eq!(iter.next_back().map(Line::current_seqno), Some(35));
        assert_eq!(iter.len(), 21);
        assert_eq!(iter.clone().count(), 21);
        assert_eq!(rows.page().unwrap().cached_view_count(), 8);

        let mut beyond_end = iter.clone();
        assert!(beyond_end.nth(21).is_none());
        assert_eq!(beyond_end.len(), 0);
        assert!(beyond_end.next_back().is_none());
        let mut from_back = iter.clone();
        assert!(from_back.nth_back(usize::MAX).is_none());
        assert_eq!(from_back.len(), 0);
        assert!(from_back.next().is_none());
        assert!(iter.nth(usize::MAX).is_none());
        assert_eq!(iter.len(), 0);
        assert!(iter.next().is_none());
        assert!(rows.range(8..8).last().is_none());
        assert_eq!(rows.page().unwrap().cached_view_count(), 8);
    }

    #[test]
    fn page_damage_metadata_honors_zero_edits_and_floors_without_new_views() {
        let sequences = [0, 1, 7, SequenceNo::MAX];
        let mut page = packed_rows(&sequences);
        let mut legacy = Rows::Legacy(sequences.iter().copied().map(Line::new).collect());
        for index in 0..sequences.len() {
            for since in [0, 1, 6, 7, SequenceNo::MAX] {
                assert_eq!(
                    page.row_changed_since(index, since),
                    legacy.row_changed_since(index, since)
                );
            }
        }
        assert_eq!(page.row_changed_since(0, SequenceNo::MAX), Some(true));
        assert_eq!(page.page().unwrap().cached_view_count(), 0);
        for index in [sequences.len(), usize::MAX] {
            assert_eq!(page.row_changed_since(index, 0), None);
            assert_eq!(legacy.row_changed_since(index, 0), None);
        }

        // A read-only cached view is disposable; an edited view owns content
        // and must supply the sequence even while its packed row is stale.
        let _ = page.get(1).unwrap();
        page.get_mut(2).unwrap().update_last_change_seqno(11);
        legacy.get_mut(2).unwrap().update_last_change_seqno(11);
        assert_eq!(page.row_changed_since(2, 7), Some(true));
        assert_eq!(page.row_changed_since(2, 11), Some(false));
        assert_eq!(page.page().unwrap().cached_view_count(), 2);

        page.page_mut().unwrap().mark_all_changed(13);
        for line in legacy.iter_mut() {
            line.update_last_change_seqno(13);
        }
        assert_eq!(page.page().unwrap().cached_view_count(), 1);
        assert_eq!(page.row_changed_since(0, 13), Some(false));
        for index in 0..sequences.len() {
            for since in [0, 7, 11, 12, 13, SequenceNo::MAX] {
                assert_eq!(
                    page.row_changed_since(index, since),
                    legacy.row_changed_since(index, since)
                );
            }
        }
        assert_eq!(page.page().unwrap().cached_view_count(), 1);
    }

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

    /// Rows that scroll into scrollback are compressed there: legacy as they
    /// go, the page engine when a row is first read. Whatever the text, each
    /// row reads back as the same `Line` in the same storage form and
    /// length, and the lazy compression leaves no row held by a view.
    #[test]
    fn scrollback_rows_read_back_in_legacy_storage_form() {
        let texts = [
            "plain ascii",
            "e\u{301} combining",
            "\u{4e2d}\u{6587}wide",
            "\u{1f468}\u{200d}\u{1f469} zwj",
            "\u{2500}\u{2502}\u{250c} box",
            // A wide character on the last column wraps whole.
            "abcdefghi\u{4e2d}",
            "\u{2764}\u{fe0f}\u{200d}",
            "x\u{301}\u{301}y",
        ];
        let mut bytes = Vec::new();
        for round in 0..6 {
            for text in texts {
                bytes.extend_from_slice(format!("{} {}\r\n", round, text).as_bytes());
            }
        }
        let mut legacy = terminal(GridEngine::Legacy);
        let mut page = terminal(GridEngine::Page);
        legacy.advance_bytes(&bytes);
        page.advance_bytes(&bytes);
        assert_eq!(page.screen().grid_engine(), GridEngine::Page);
        assert_eq!(page.screen().rows_held_by_views(), 0);
        assert_eq!(decoded(&page), decoded(&legacy));
        let forms = |terminal: &Terminal| {
            terminal
                .screen()
                .all_lines()
                .iter()
                .map(|line| (line.has_clustered_storage(), line.len()))
                .collect::<Vec<_>>()
        };
        let page_forms = forms(&page);
        assert_eq!(page_forms, forms(&legacy));
        let scrollback = page_forms.len() - 6;
        assert!(scrollback > texts.len());
        assert!(page_forms[..scrollback]
            .iter()
            .any(|(clustered, _)| *clustered));
    }

    /// ft-yccm0.3.3.4: page-engine rows decode exactly as legacy rows after
    /// the hot path's streams, whole and in 7-byte chunks, and the screen
    /// keeps its rows in pages through height/DPI changes. A width change
    /// returns them to legacy storage, where they still decode the same.
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
                "saved_cursor_in_pruned_blank_rows",
                b"\x1b[6;10H\x1b7\x1b[2;1Hprompt".to_vec(),
            ),
            (
                "saved_wide_cursor_at_wrap",
                "\x1b[1;9H界\x1b7\x1b[2;1Hprompt".as_bytes().to_vec(),
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
                // SL and SR within the top and bottom margins, then within
                // DECLRMM margins with a wide character at the left margin,
                // and an SL from outside the margins, which does nothing.
                "scroll_left_right",
                "0123456789\r\nabcdefghij\r\nklmnopqrst\r\nuvwxyz0123\r\n\u{4e2d}\u{6587}456789\x1b[2;5r\x1b[3;1H\x1b[2 @\x1b[44m\x1b[0 A\x1b[m\x1b[ A\x1b[?69h\x1b[1;7s\x1b[5;2H\x1b[1 @\x1b[3 A\x1b[s\x1b[?69l\x1b[r\x1b[1;1H\x1b[2;4r\x1b[1;1H\x1b[ @\x1b[r"
                    .as_bytes()
                    .to_vec(),
            ),
            (
                // DECSCA cells, wide and narrow, with SGR 0 inside the
                // protected run, kept by DECSEL and DECSED and erased by ED
                // in DEC mode; then DECSTR, after which DECSEL clears them.
                "protection_decsca",
                "\x1b[1\"qAB\x1b[1;44mC\x1b[0mD\u{4e2d}\x1b[0\"qxy\x1b[2;1H\x1b[1\"qPQ\x1b[2\"qrs\x1b[1;3H\x1b[?K\x1b[2;3H\x1b[?1K\x1b[?2J\x1b[3;1H\x1b[1\"qkeep\x1b[0\"q\x1b[3;3H\x1b[?0J\x1b[2J\x1b[4;1H\x1b[1\"qGONE\x1b[!p\x1b[?2K"
                    .as_bytes()
                    .to_vec(),
            ),
            (
                // An SPA box that ED, EL and ECH keep, with a coloured pen;
                // DECSCA then switches to DEC mode and ED erases it. A new
                // SPA over a screen an ED 2 empties turns protection off, so
                // EL erases what follows.
                "protection_spa",
                "\x1b[2J\x1bV\x1b[2;3HPROT\x1b[3;3HPROT\x1bW\x1b[2;1Hfree\x1b[3;9Hmore\x1b[44m\x1b[2;4H\x1b[J\x1b[1J\x1b[3;1H\x1b[2K\x1b[2;1H\x1b[8X\x1b[2J\x1b[m\x1b[0\"q\x1b[2J\x1bV\x1b[2Jlost\x1b[1;1H\x1b[2K"
                    .as_bytes()
                    .to_vec(),
            ),
            ("dec_graphics_5f", b"\x1b(0_q_\x1b(B_".to_vec()),
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

                for (rows, dpi) in [(3, 96), (9, 96), (9, 144), (6, 96)] {
                    let size = TerminalSize {
                        rows,
                        cols: 10,
                        pixel_width: 80,
                        pixel_height: rows * 16,
                        dpi,
                    };
                    legacy.resize(size);
                    page.resize(size);
                    assert_eq!(
                        page.screen().grid_engine(),
                        GridEngine::Page,
                        "{what}, rows={rows}, dpi={dpi}"
                    );
                    assert_eq!(
                        decoded(&page),
                        decoded(&legacy),
                        "{what}, rows={rows}, dpi={dpi}"
                    );
                    assert_eq!(
                        page.cursor_pos(),
                        legacy.cursor_pos(),
                        "{what}, rows={rows}, dpi={dpi}"
                    );
                    assert_eq!(
                        page.get_semantic_zones().unwrap(),
                        legacy.get_semantic_zones().unwrap(),
                        "{what} zones"
                    );
                }
                if name.starts_with("saved_") {
                    legacy.advance_bytes(b"\x1b8!");
                    page.advance_bytes(b"\x1b8!");
                    assert_eq!(
                        page.cursor_pos(),
                        legacy.cursor_pos(),
                        "{what} restored cursor"
                    );
                    assert_eq!(decoded(&page), decoded(&legacy), "{what} restored content");
                    assert_eq!(page.screen().grid_engine(), GridEngine::Page);
                }

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
