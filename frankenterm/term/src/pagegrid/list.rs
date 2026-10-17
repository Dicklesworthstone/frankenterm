//! The page list (PageGrid ADR D5, D6, D8 and section 3.9; B3.3).
//!
//! The primary screen's rows, oldest first, in pages of `page_rows` rows. A
//! row's coordinate is its `StableRowIndex`: [`PageList::scroll`] appends
//! rows at the tail with the next index and trims rows at the front, so
//! indices increase and are never reused (D6). A stable index maps to
//! `(page, row)` by a binary search over the pages' first indices.
//!
//! **Scrolling is O(1) per row.** A new row is the next row of the tail page,
//! or the first row of a pooled page. A front page whose rows have all been
//! trimmed is reset and pooled (at most [`MAX_POOLED_PAGES`]), so a list
//! scrolling at its cap allocates nothing in steady state.
//!
//! **Eviction keeps legacy's per-row admission (D8).** Rows past the hot cap
//! are offered to the caller's sink one at a time, oldest first. A refused
//! row stays resident and stops the eviction, and a recovery hold evicts
//! nothing. Either way the list records why it is over its cap ([`OverCap`],
//! ADR Q4), so I13 stays checkable.
//!
//! **Sealing (D8, I15).** An active page is owned outright (a `Box`), so the
//! writes every printed run and scroll make need no atomic uniqueness check.
//! A page is sealed once it is not the tail page and each of its rows is at
//! least [`SEAL_DISTANCE`] rows above the viewport top; sealing moves its
//! contents into an `Arc<Page>` (no copy of its rows), a pooled shell whose
//! reset page goes back to the pool in its place, so that at the cap sealing
//! allocates nothing either. A recycled sealed page becomes such a shell
//! while no reader holds it. Only sealed
//! pages are handed out as `Arc`s, to readers such as the durability writer
//! and the warm-tier compressor, which then read them without a lock. A
//! write to a sealed page goes through copy-on-write: in place while no
//! reader holds it, otherwise into a fork under a fresh serial. Sealed pages
//! are always a prefix of the list, because eligibility only depends on a
//! page's last stable index.
//!
//! **Change tracking (ADR Q2, Q5).** A seqno-only bump of rows in sealed
//! pages is recorded as a list-level range floor instead of a page write. A
//! palette change raises the list-wide `dirty_floor`. A row's effective
//! seqno is the numeric maximum of its own seqno and every floor covering
//! it; only then does 0 mean "always changed".

use super::grapheme::STD_ARENA_ENTRIES;
use super::page::{rows_per_page, Page};
use crate::StableRowIndex;
use frankenterm_surface::SequenceNo;
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;

/// Idle, reset pages kept for reuse (D8).
pub const MAX_POOLED_PAGES: usize = 4;

/// A page is sealed only when every row it holds is at least this many rows
/// above the viewport top, so the seam row that `clear_line` can still write
/// stays in an active page (D8).
pub const SEAL_DISTANCE: usize = 2;

/// Range floors beyond this count collapse into one covering floor.
const MAX_RANGE_FLOORS: usize = 32;

/// Why the list holds more rows than its hot cap (ADR I13 and Q4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverCap {
    /// The sink refused the last row offered for eviction; it stays
    /// resident, as in legacy.
    SpillRefused,
    /// Eviction is held while recovery rows exist.
    RecoveryHold,
    /// The cap was lowered (resize, rewrap or a config change). Legacy
    /// trims the excess at the next evicting scroll, and so does the list.
    CapLowered,
}

/// A seqno-only bump of rows that may sit in sealed pages (ADR Q2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeFloor {
    pub start: StableRowIndex,
    /// Exclusive.
    pub end: StableRowIndex,
    pub seqno: SequenceNo,
}

impl RangeFloor {
    fn covers(&self, stable: StableRowIndex) -> bool {
        self.start <= stable && stable < self.end
    }

    fn overlaps(&self, start: StableRowIndex, end: StableRowIndex) -> bool {
        self.start < end && start < self.end
    }
}

/// A retained row, for reading.
#[derive(Clone, Copy, Debug)]
pub struct RowRef<'a> {
    pub stable: StableRowIndex,
    pub page: &'a Page,
    /// The row within `page`.
    pub row: u32,
    /// The page as a shareable `Arc`, when it is sealed. An admitted row's
    /// sink can keep this instead of copying the row (D8, A1 hand-off).
    pub sealed: Option<&'a Arc<Page>>,
}

/// What [`PageList::scroll`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scrolled {
    /// Rows trimmed from the front, each admitted by the sink.
    pub trimmed: usize,
    /// The sink refused a row, which stays resident.
    pub refused: bool,
    /// The first appended row; the rest follow it.
    pub first_new: StableRowIndex,
}

struct PageSlot {
    /// Stable index of row `front_trim`, the first row still retained.
    first_stable: StableRowIndex,
    /// Rows at the front that have been trimmed, awaiting recycle.
    front_trim: u32,
    /// The page while active: owned outright, so a write needs no atomic
    /// uniqueness check (every printed run and every scroll writes one).
    active: Option<Box<Page>>,
    /// The page once sealed: shareable, readers may hold it, and a write
    /// forks it when one does (D8). Exactly one of the two is set.
    sealed: Option<Arc<Page>>,
}

impl PageSlot {
    fn new_active(first_stable: StableRowIndex, page: Box<Page>) -> Self {
        Self {
            first_stable,
            front_trim: 0,
            active: Some(page),
            sealed: None,
        }
    }

    fn page(&self) -> &Page {
        match (&self.active, &self.sealed) {
            (Some(page), _) => page,
            (None, Some(page)) => page,
            (None, None) => unreachable!("a page slot holds its page"),
        }
    }

    /// The page, writable: an active page in place, a sealed one in place
    /// while no reader holds it and forked under a fresh serial otherwise.
    #[inline]
    fn page_mut(&mut self, next_serial: &mut u64) -> &mut Page {
        if self.active.is_some() {
            return self.active.as_deref_mut().expect("checked above");
        }
        self.sealed_page_mut(next_serial)
    }

    /// [`Self::page_mut`] of a sealed page, kept out of line: every printed
    /// run writes an active page, and the fork here made each of those
    /// writes save and restore every register it might use.
    #[cold]
    #[inline(never)]
    fn sealed_page_mut(&mut self, next_serial: &mut u64) -> &mut Page {
        let sealed = self.sealed.as_mut().expect("a page slot holds its page");
        if Arc::get_mut(sealed).is_none() {
            let serial = *next_serial;
            *next_serial += 1;
            *sealed = Arc::new(sealed.fork(serial));
        }
        Arc::get_mut(sealed).expect("a page is unique once forked")
    }

    /// Seals an active page: it becomes shareable. A pooled shell takes the
    /// page's contents (its buffers move, no row is copied), and the page's
    /// box, now holding the shell's reset page, goes back to the pool, so
    /// sealing allocates nothing; without a shell the page moves into a new
    /// `Arc`.
    #[allow(clippy::vec_box)] // A pooled box moves into an active slot as is.
    fn seal(&mut self, shells: &mut Vec<Arc<Page>>, pool: &mut Vec<Box<Page>>) {
        let Some(mut page) = self.active.take() else {
            return;
        };
        self.sealed = Some(match shells.pop() {
            Some(mut shell) => {
                let reset = Arc::get_mut(&mut shell).expect("only the pool holds a shell");
                std::mem::swap(reset, &mut *page);
                if pool.len() < MAX_POOLED_PAGES {
                    pool.push(page);
                }
                shell
            }
            None => Arc::from(page),
        });
    }

    /// Unseals a sealed page: it is owned again, moved out of its `Arc`
    /// when no reader holds it and forked under a fresh serial otherwise.
    fn unseal(&mut self, next_serial: &mut u64) {
        if let Some(page) = self.sealed.take() {
            self.active = Some(match Arc::try_unwrap(page) {
                Ok(page) => Box::new(page),
                Err(shared) => {
                    let serial = *next_serial;
                    *next_serial += 1;
                    Box::new(shared.fork(serial))
                }
            });
        }
    }

    fn retained(&self) -> usize {
        (self.page().used() - self.front_trim) as usize
    }

    /// One past the last retained row.
    fn end_stable(&self) -> StableRowIndex {
        self.first_stable + self.retained() as StableRowIndex
    }

    fn is_full(&self) -> bool {
        self.page().used() == self.page().capacity()
    }
}

pub struct PageList {
    pages: VecDeque<PageSlot>,
    /// Reset pages of this list's geometry, each held only here, boxed as an
    /// active slot holds them: taking one for a new slot moves the box, with
    /// no allocation (D8).
    #[allow(clippy::vec_box)]
    pool: Vec<Box<Page>>,
    /// Reset pages of this list's geometry in `Arc`s, each held only here:
    /// sealing moves a page's contents into one ([`PageSlot::seal`]).
    shells: Vec<Arc<Page>>,
    cols: u16,
    page_rows: u32,
    retained_rows: usize,
    /// `physical_rows + hot_scrollback_size()`, as legacy caps.
    hot_cap: usize,
    viewport_rows: usize,
    /// Set exactly while `retained_rows > hot_cap`.
    over_cap: Option<OverCap>,
    /// Pages `0..sealed` are sealed; the rest are active (I15).
    sealed: usize,
    dirty_floor: SequenceNo,
    range_floors: Vec<RangeFloor>,
    next_serial: u64,
    /// The stable index the next appended row takes.
    next_stable: StableRowIndex,
    /// Pages taken from the pool for new rows, since the list was built.
    pages_reused: u64,
    /// Pages allocated for new rows because the pool was empty.
    pages_allocated: u64,
}

impl std::fmt::Debug for PageList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageList")
            .field("cols", &self.cols)
            .field("page_rows", &self.page_rows)
            .field("pages", &self.pages.len())
            .field("sealed", &self.sealed)
            .field("pooled", &self.pool.len())
            .field("shells", &self.shells.len())
            .field("retained_rows", &self.retained_rows)
            .field("hot_cap", &self.hot_cap)
            .field("over_cap", &self.over_cap)
            .field("first_stable", &self.first_stable())
            .field("next_stable", &self.next_stable)
            .finish()
    }
}

impl PageList {
    /// An empty list of standard pages ([`rows_per_page`]). Serials start at
    /// `first_serial` and are unique among this list's pages (D6, I12); the
    /// terminal's alternate-screen page takes one from
    /// [`Self::allocate_serial`].
    pub fn new(cols: u16, viewport_rows: usize, hot_cap: usize, first_serial: u64) -> Self {
        Self::with_page_rows(
            cols,
            rows_per_page(cols),
            viewport_rows,
            hot_cap,
            first_serial,
        )
    }

    /// An empty list whose pages hold `page_rows` rows each.
    pub fn with_page_rows(
        cols: u16,
        page_rows: u32,
        viewport_rows: usize,
        hot_cap: usize,
        first_serial: u64,
    ) -> Self {
        assert!(page_rows > 0, "a page holds at least one row");
        Self {
            pages: VecDeque::new(),
            pool: Vec::with_capacity(MAX_POOLED_PAGES),
            shells: Vec::with_capacity(MAX_POOLED_PAGES),
            cols,
            page_rows,
            retained_rows: 0,
            hot_cap,
            viewport_rows,
            over_cap: None,
            sealed: 0,
            dirty_floor: 0,
            range_floors: Vec::new(),
            next_serial: first_serial,
            next_stable: 0,
            pages_reused: 0,
            pages_allocated: 0,
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn page_rows(&self) -> u32 {
        self.page_rows
    }

    /// Rows held, including rows over the cap.
    pub fn retained_rows(&self) -> usize {
        self.retained_rows
    }

    pub fn hot_cap(&self) -> usize {
        self.hot_cap
    }

    pub fn viewport_rows(&self) -> usize {
        self.viewport_rows
    }

    /// Why the list is over its cap; `None` exactly when it is not.
    pub fn over_cap(&self) -> Option<OverCap> {
        self.over_cap
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn sealed_page_count(&self) -> usize {
        self.sealed
    }

    /// Idle pages in the pool right now. A list scrolling at its cap
    /// recycles its front page into the pool and takes it back out when the
    /// tail page fills, so this alternates between 0 and 1: what proves
    /// recycling is [`Self::pages_reused`] growing while
    /// [`Self::pages_allocated`] does not.
    pub fn pooled_page_count(&self) -> usize {
        self.pool.len()
    }

    /// Pages taken back out of the pool for new rows, since the list was
    /// built.
    pub fn pages_reused(&self) -> u64 {
        self.pages_reused
    }

    /// Pages allocated for new rows because the pool was empty, since the
    /// list was built.
    pub fn pages_allocated(&self) -> u64 {
        self.pages_allocated
    }

    pub fn dirty_floor(&self) -> SequenceNo {
        self.dirty_floor
    }

    pub fn range_floors(&self) -> &[RangeFloor] {
        &self.range_floors
    }

    /// The oldest retained row; [`Self::end_stable`] when none is.
    pub fn first_stable(&self) -> StableRowIndex {
        self.pages
            .front()
            .map_or(self.next_stable, |slot| slot.first_stable)
    }

    /// One past the newest row: the index the next appended row takes.
    pub fn end_stable(&self) -> StableRowIndex {
        self.next_stable
    }

    /// The first row of the viewport: the last `viewport_rows` rows, or
    /// every row while fewer are retained.
    pub fn viewport_top(&self) -> StableRowIndex {
        let rows = self.viewport_rows.min(self.retained_rows) as StableRowIndex;
        self.next_stable - rows
    }

    /// A fresh serial, for pages outside the list such as the
    /// alternate-screen page (D6).
    pub fn allocate_serial(&mut self) -> u64 {
        let serial = self.next_serial;
        self.next_serial += 1;
        serial
    }

    /// Where `stable` lives: the page's position in the list and the row
    /// within it. O(1) for the tail page, O(log pages) otherwise.
    #[inline]
    pub fn locate(&self, stable: StableRowIndex) -> Option<(usize, u32)> {
        if stable < self.first_stable() || stable >= self.next_stable {
            return None;
        }
        let tail = self.pages.len() - 1;
        let slot = &self.pages[tail];
        if stable >= slot.first_stable {
            let row = slot.front_trim as usize + (stable - slot.first_stable) as usize;
            return Some((tail, row as u32));
        }
        Some(self.locate_before_tail(stable))
    }

    /// [`Self::locate`] of a retained row before the tail page, by binary
    /// search, out of line: inline, its frame was the tail lookup's, which
    /// every printed run on the bottom row makes.
    #[inline(never)]
    fn locate_before_tail(&self, stable: StableRowIndex) -> (usize, u32) {
        let index = self
            .pages
            .partition_point(|slot| slot.end_stable() <= stable);
        let slot = &self.pages[index];
        let row = slot.front_trim as usize + (stable - slot.first_stable) as usize;
        (index, row as u32)
    }

    /// The retained row at `stable`.
    pub fn row(&self, stable: StableRowIndex) -> Option<RowRef<'_>> {
        let (index, row) = self.locate(stable)?;
        Some(self.row_ref(index, row, stable))
    }

    fn row_ref(&self, index: usize, row: u32, stable: StableRowIndex) -> RowRef<'_> {
        let slot = &self.pages[index];
        RowRef {
            stable,
            page: slot.page(),
            row,
            sealed: slot.sealed.as_ref().filter(|_| index < self.sealed),
        }
    }

    /// Every retained row, oldest first.
    pub fn rows(&self) -> impl Iterator<Item = RowRef<'_>> + '_ {
        self.pages
            .iter()
            .enumerate()
            .flat_map(move |(index, slot)| {
                (slot.front_trim..slot.page().used()).map(move |row| {
                    let stable = slot.first_stable + (row - slot.front_trim) as StableRowIndex;
                    self.row_ref(index, row, stable)
                })
            })
    }

    /// The sealed pages, oldest first, with the stable index of each one's
    /// first retained row. Readers may clone these and keep them.
    pub fn sealed_pages(&self) -> impl Iterator<Item = (StableRowIndex, &Arc<Page>)> + '_ {
        self.pages.iter().take(self.sealed).map(|slot| {
            (
                slot.first_stable,
                slot.sealed
                    .as_ref()
                    .expect("a sealed slot holds its page shared"),
            )
        })
    }

    /// The row at `stable` for writing, through copy-on-write if its page
    /// is sealed (D8). Callers bump the row's seqno through the page.
    #[inline]
    pub fn row_mut(&mut self, stable: StableRowIndex) -> Option<(&mut Page, u32)> {
        let (index, row) = self.locate(stable)?;
        Some((self.page_mut(index), row))
    }

    /// The page at `index`, writable. Active pages are owned outright. A
    /// sealed page is written in place while no reader holds it, and
    /// otherwise forked under a fresh serial, leaving readers their copy.
    #[inline]
    fn page_mut(&mut self, index: usize) -> &mut Page {
        let PageList {
            pages, next_serial, ..
        } = self;
        pages[index].page_mut(next_serial)
    }

    /// Scrolls `n` rows into the list at the tail, as legacy's full-screen
    /// `scroll_up` does. Rows past `hot_cap` are first trimmed from the front,
    /// each offered to `admit`, which records it in the cold sink. A refusal
    /// stops the trim and the row stays. With `hold` (recovery rows exist)
    /// nothing is trimmed. The new rows are empty, carry `seqno`, and are
    /// left for the caller to fill.
    pub fn scroll(
        &mut self,
        n: usize,
        seqno: SequenceNo,
        hold: bool,
        admit: impl FnMut(RowRef<'_>) -> bool,
    ) -> Scrolled {
        self.scroll_with_row_flags(n, seqno, hold, 0, admit)
    }

    /// [`Self::scroll`] whose new rows also carry the row flags `flags`, set
    /// while the list holds the tail page, so a caller that would only set
    /// them looks no new row up again.
    pub fn scroll_with_row_flags(
        &mut self,
        n: usize,
        seqno: SequenceNo,
        hold: bool,
        flags: u64,
        mut admit: impl FnMut(RowRef<'_>) -> bool,
    ) -> Scrolled {
        // Legacy's `lines.len() + num_rows >= max_allowed` count.
        let excess = (self.retained_rows + n).saturating_sub(self.hot_cap);
        let (mut trimmed, mut refused) = if hold {
            (0, false)
        } else {
            self.trim_front(excess, &mut admit)
        };
        let first_new = self.next_stable;
        for _ in 0..n {
            self.append_row(seqno, flags);
        }
        if !hold && !refused && trimmed < excess {
            // More rows scrolled in than were retained: legacy's eviction
            // loop then offers the new rows too, oldest first.
            let (more, refused_more) = self.trim_front(excess - trimmed, &mut admit);
            trimmed += more;
            refused = refused_more;
        }
        self.seal_eligible();
        self.over_cap = if self.retained_rows <= self.hot_cap {
            None
        } else if hold {
            Some(OverCap::RecoveryHold)
        } else {
            debug_assert!(refused, "an unrefused eviction reaches the cap");
            Some(OverCap::SpillRefused)
        };
        self.debug_check_counts();
        Scrolled {
            trimmed,
            refused,
            first_new,
        }
    }

    /// [`Self::scroll_with_row_flags`] of one row with nothing held and
    /// every trimmed row admitted: the line feed at the bottom of the
    /// screen, once per line of a `cat` flood (ft-yccm0.3.2.6). With at most
    /// one row over the cap coming in, and a front row to trim for it, this
    /// trims that row and appends one straight through; any other shape
    /// takes the general path.
    pub fn scroll_line(&mut self, seqno: SequenceNo, flags: u64) -> Scrolled {
        let excess = (self.retained_rows + 1).saturating_sub(self.hot_cap);
        let front_trims = excess == 1 && self.pages.front().is_some_and(|slot| slot.retained() > 0);
        if excess > 1 || (excess == 1 && !front_trims) {
            return self.scroll_with_row_flags(1, seqno, false, flags, |_| true);
        }
        let first_new = self.next_stable;
        if front_trims {
            // `trim_front` of one row.
            let slot = self.pages.front_mut().expect("checked above");
            slot.front_trim += 1;
            slot.first_stable += 1;
            self.retained_rows -= 1;
            if slot.retained() == 0 && slot.is_full() {
                self.recycle_front();
            }
            if !self.range_floors.is_empty() {
                let first = self.first_stable();
                self.range_floors.retain(|floor| floor.end > first);
            }
        }
        self.append_row(seqno, flags);
        self.seal_eligible();
        // The one row over the cap, if any, was trimmed.
        self.over_cap = None;
        self.debug_check_counts();
        Scrolled {
            trimmed: excess,
            refused: false,
            first_new,
        }
    }

    /// Legacy's scroll with top margin 0 and a bottom margin above the
    /// screen's last row (D5): scrollback is fed as by [`Self::scroll`], and
    /// the `n` new rows are inserted above the `footer` rows below the
    /// region, which move down one stable index each.
    pub fn scroll_above_footer(
        &mut self,
        n: usize,
        footer: usize,
        seqno: SequenceNo,
        hold: bool,
        admit: impl FnMut(RowRef<'_>) -> bool,
    ) -> Scrolled {
        let scrolled = self.scroll(n, seqno, hold, admit);
        if footer > 0 && n > 0 {
            let end = self.next_stable;
            let start = (end - (footer + n) as StableRowIndex).max(self.first_stable());
            self.rotate_down(start..end, n, seqno);
        }
        scrolled
    }

    /// Scrolls the rows in `range` up by `n` within the range, as a scroll
    /// region with a top margin does: the top `n` rows are cleared and move
    /// to the bottom, and the others move up. No row enters scrollback.
    /// Within a page this moves row headers only; a row crossing a page
    /// boundary is copied (D5). Moved rows keep their seqnos; cleared rows
    /// take `seqno`.
    pub fn rotate_up(&mut self, range: Range<StableRowIndex>, n: usize, seqno: SequenceNo) {
        let range = self.clamp(range);
        let n = n.min(range.len());
        if n > 0 {
            self.materialize_floors(&range);
        }
        for _ in 0..n {
            self.clear(range.start, seqno);
            for bubble in range.start..range.end - 1 {
                self.bubble_step(bubble, bubble + 1, seqno);
            }
        }
        self.debug_check_counts();
    }

    /// The reverse of [`Self::rotate_up`]: the bottom `n` rows of `range`
    /// are cleared and move to its top.
    pub fn rotate_down(&mut self, range: Range<StableRowIndex>, n: usize, seqno: SequenceNo) {
        let range = self.clamp(range);
        let n = n.min(range.len());
        if n > 0 {
            self.materialize_floors(&range);
        }
        for _ in 0..n {
            self.clear(range.end - 1, seqno);
            for bubble in (range.start + 1..range.end).rev() {
                self.bubble_step(bubble, bubble - 1, seqno);
            }
        }
        self.debug_check_counts();
    }

    /// Writes the range floors covering `range` into its rows before they
    /// move. A floor belongs to a position, but a rotated row must carry its
    /// bump with it, as a legacy `Line` carries its seqno (I10). Floors only
    /// cover rows that were sealed when marked; a taller viewport can bring
    /// those rows into a scroll region. The floors stay, so the positions
    /// keep reading as changed, which is conservative.
    fn materialize_floors(&mut self, range: &Range<StableRowIndex>) {
        if self.range_floors.is_empty() {
            return;
        }
        for stable in range.clone() {
            let floor = self
                .range_floors
                .iter()
                .filter(|floor| floor.covers(stable))
                .map(|floor| floor.seqno)
                .max();
            if let Some(seqno) = floor {
                let (page, row) = self.row_mut(stable).expect("a retained row");
                page.touch_row(row, seqno);
            }
        }
    }

    fn clamp(&self, range: Range<StableRowIndex>) -> Range<StableRowIndex> {
        let start = range.start.max(self.first_stable());
        let end = range.end.min(self.next_stable).max(start);
        start..end
    }

    fn clear(&mut self, stable: StableRowIndex, seqno: SequenceNo) {
        let (page, row) = self.row_mut(stable).expect("a retained row");
        page.clear_row(row, seqno);
    }

    /// Moves the empty row at `bubble` to `other` and the row at `other` to
    /// `bubble`. Rows in one page swap headers. Across a page boundary the
    /// other row is copied into the empty one and cleared in its own page.
    fn bubble_step(&mut self, bubble: StableRowIndex, other: StableRowIndex, seqno: SequenceNo) {
        let (bubble_index, bubble_row) = self.locate(bubble).expect("a retained row");
        let (other_index, other_row) = self.locate(other).expect("a retained row");
        if bubble_index == other_index {
            self.page_mut(bubble_index).swap_rows(bubble_row, other_row);
            return;
        }
        // The source page is held aside while the destination, a different
        // page, is written: an active page moved out of its slot and put
        // back, a sealed one shared for the moment.
        let source_slot = &mut self.pages[other_index];
        let held_active = source_slot.active.take();
        let held_sealed = source_slot.sealed.clone();
        let source: &Page = held_active
            .as_deref()
            .or(held_sealed.as_deref())
            .expect("a page slot holds its page");
        self.page_mut(bubble_index)
            .copy_row_from(bubble_row, source, other_row);
        self.pages[other_index].active = held_active;
        drop(held_sealed);
        self.page_mut(other_index).clear_row(other_row, seqno);
    }

    /// Raises every row's effective seqno to at least `seqno`, as legacy's
    /// palette change bumps every resident row. No page is written.
    pub fn mark_all_changed(&mut self, seqno: SequenceNo) {
        self.dirty_floor = self.dirty_floor.max(seqno);
    }

    /// Raises the effective seqno of the rows in `range` to at least
    /// `seqno` (ADR Q2): rows in active pages take it directly, and rows in
    /// sealed pages get a range floor instead of a page write.
    pub fn mark_rows_changed(&mut self, range: Range<StableRowIndex>, seqno: SequenceNo) {
        let range = self.clamp(range);
        if range.is_empty() {
            return;
        }
        let first_active = self
            .pages
            .get(self.sealed)
            .map_or(self.next_stable, |slot| slot.first_stable);
        let sealed_end = range.end.min(first_active);
        if range.start < sealed_end {
            self.push_floor(RangeFloor {
                start: range.start,
                end: sealed_end,
                seqno,
            });
        }
        for stable in range.start.max(first_active)..range.end {
            let (page, row) = self.row_mut(stable).expect("a retained row");
            page.touch_row(row, seqno);
        }
    }

    fn push_floor(&mut self, floor: RangeFloor) {
        // A floor covered by a newer, higher floor says nothing more.
        self.range_floors.retain(|old| {
            !(floor.start <= old.start && old.end <= floor.end && old.seqno <= floor.seqno)
        });
        self.range_floors.push(floor);
        if self.range_floors.len() > MAX_RANGE_FLOORS {
            // Conservative: one floor over every bumped row, at the highest
            // seqno. It may report more rows changed, never fewer.
            let hull = self
                .range_floors
                .iter()
                .fold(floor, |hull, old| RangeFloor {
                    start: hull.start.min(old.start),
                    end: hull.end.max(old.end),
                    seqno: hull.seqno.max(old.seqno),
                });
            self.range_floors.clear();
            self.range_floors.push(hull);
        }
    }

    /// The row's effective seqno (ADR Q5): the numeric maximum of its own
    /// seqno, `dirty_floor` and every range floor covering it. A result of 0
    /// means "always changed".
    pub fn effective_seqno(&self, stable: StableRowIndex) -> Option<SequenceNo> {
        let (index, row) = self.locate(stable)?;
        let mut seqno = self.pages[index]
            .page()
            .row_seqno(row)
            .max(self.dirty_floor);
        for floor in &self.range_floors {
            if floor.covers(stable) {
                seqno = seqno.max(floor.seqno);
            }
        }
        Some(seqno)
    }

    /// Legacy `Line::changed_since` on the effective seqno.
    pub fn changed_since(&self, stable: StableRowIndex, since: SequenceNo) -> Option<bool> {
        self.effective_seqno(stable)
            .map(|seqno| seqno == 0 || seqno > since)
    }

    /// Appends to `out` the rows of `range` changed since `since`, oldest
    /// first. A page is skipped without a row scan when its maximum seqno,
    /// `dirty_floor` and every overlapping range floor are at or below
    /// `since` (section 3.9).
    pub fn changed_rows(
        &self,
        range: Range<StableRowIndex>,
        since: SequenceNo,
        out: &mut Vec<StableRowIndex>,
    ) {
        let range = self.clamp(range);
        if range.is_empty() {
            return;
        }
        let (first, _) = self.locate(range.start).expect("a retained row");
        for slot in self.pages.iter().skip(first) {
            let start = slot.first_stable.max(range.start);
            let end = slot.end_stable().min(range.end);
            if start >= end {
                break;
            }
            let page_max = slot.page().max_seqno();
            let quiet = page_max != 0
                && page_max <= since
                && self.dirty_floor <= since
                && !self
                    .range_floors
                    .iter()
                    .any(|floor| floor.seqno > since && floor.overlaps(start, end));
            if quiet {
                continue;
            }
            for stable in start..end {
                if self.changed_since(stable, since) == Some(true) {
                    out.push(stable);
                }
            }
        }
    }

    /// Sets the hot cap. Legacy does not trim on a cap change; the next
    /// evicting scroll does, so until then the list is over its cap
    /// ([`OverCap::CapLowered`], ADR Q4).
    pub fn set_hot_cap(&mut self, hot_cap: usize) {
        self.hot_cap = hot_cap;
        self.over_cap = if self.retained_rows <= hot_cap {
            None
        } else {
            Some(self.over_cap.unwrap_or(OverCap::CapLowered))
        };
    }

    /// Sets the viewport height. Growing it can pull rows of sealed pages
    /// back within [`SEAL_DISTANCE`] of the viewport top; those pages are
    /// unsealed, by fork when a reader still holds them (D8).
    pub fn set_viewport_rows(&mut self, viewport_rows: usize) {
        self.viewport_rows = viewport_rows;
        let cutoff = self.seal_cutoff();
        while self.sealed > 0 && self.pages[self.sealed - 1].end_stable() - 1 > cutoff {
            self.sealed -= 1;
            let index = self.sealed;
            let PageList {
                pages, next_serial, ..
            } = self;
            pages[index].unseal(next_serial);
        }
        self.seal_eligible();
    }

    /// The last stable index a sealed page may hold.
    fn seal_cutoff(&self) -> StableRowIndex {
        self.viewport_top() - SEAL_DISTANCE as StableRowIndex
    }

    /// Seals every eligible page: not the tail, and every row at least
    /// [`SEAL_DISTANCE`] above the viewport top.
    #[inline]
    fn seal_eligible(&mut self) {
        let cutoff = self.seal_cutoff();
        let tail = self.pages.len().saturating_sub(1);
        if self.sealed < tail && self.pages[self.sealed].end_stable() - 1 <= cutoff {
            self.seal_through(cutoff, tail);
        }
    }

    /// [`Self::seal_eligible`]'s sealing, kept out of line: it runs once a
    /// page, and inline its `Arc` conversion made every line feed save and
    /// restore every register.
    #[cold]
    #[inline(never)]
    fn seal_through(&mut self, cutoff: StableRowIndex, tail: usize) {
        while self.sealed < tail && self.pages[self.sealed].end_stable() - 1 <= cutoff {
            self.pages[self.sealed].seal(&mut self.shells, &mut self.pool);
            self.sealed += 1;
        }
    }

    /// Appends one empty row at the tail, carrying the row flags `flags`,
    /// taking a pooled page when the tail page is full.
    fn append_row(&mut self, seqno: SequenceNo, flags: u64) {
        if self.pages.back().is_none_or(PageSlot::is_full) {
            self.push_tail_page();
        }
        let tail = self.pages.len() - 1;
        let page = self.page_mut(tail);
        let row = page.grow(seqno).expect("the tail page has room");
        if flags != 0 {
            page.set_row_flags(row, flags, true);
        }
        self.retained_rows += 1;
        self.next_stable += 1;
    }

    /// [`Self::append_row`]'s new tail page, from the pool or new. Out of
    /// line: it runs once a page, and building a page inline made the frame
    /// of every appended row.
    #[cold]
    #[inline(never)]
    fn push_tail_page(&mut self) {
        let mut page = match self.pool.pop() {
            Some(page) => {
                self.pages_reused += 1;
                page
            }
            None => {
                self.pages_allocated += 1;
                let serial = self.allocate_serial();
                Box::new(Page::new(self.cols, self.page_rows, serial))
            }
        };
        // Text full of graphemes keeps filling pages with them: a fresh
        // page takes room for as many as the page it follows, rather than
        // regrowing its map from a reset page's capacity.
        if let Some(previous) = self.pages.back() {
            if previous.page().graphemes().len() > STD_ARENA_ENTRIES {
                page.reserve_graphemes_like(previous.page());
            }
        }
        self.pages
            .push_back(PageSlot::new_active(self.next_stable, page));
    }

    /// Trims up to `count` rows from the front, offering each to `admit`
    /// first. Returns the rows trimmed and whether a row was refused.
    fn trim_front(
        &mut self,
        count: usize,
        admit: &mut impl FnMut(RowRef<'_>) -> bool,
    ) -> (usize, bool) {
        let mut trimmed = 0;
        let mut refused = false;
        while trimmed < count {
            let Some(slot) = self.pages.front() else {
                break;
            };
            if slot.retained() == 0 {
                // The tail page, fully trimmed but not yet full.
                break;
            }
            let row = self.row_ref(0, slot.front_trim, slot.first_stable);
            if !admit(row) {
                refused = true;
                break;
            }
            let slot = self.pages.front_mut().expect("a front page");
            slot.front_trim += 1;
            slot.first_stable += 1;
            self.retained_rows -= 1;
            trimmed += 1;
            if slot.retained() == 0 && slot.is_full() {
                self.recycle_front();
            }
        }
        if trimmed > 0 {
            let first = self.first_stable();
            self.range_floors.retain(|floor| floor.end > first);
        }
        (trimmed, refused)
    }

    /// Removes the fully trimmed front page. An unshared page is reset under
    /// a fresh serial and pooled: an active page in its box, a sealed one in
    /// its `Arc`, as a shell. A sealed page a reader still holds is released
    /// to that reader (the tiering hand-off).
    fn recycle_front(&mut self) {
        let slot = self.pages.pop_front().expect("a front page");
        self.sealed = self.sealed.saturating_sub(1);
        match (slot.active, slot.sealed) {
            (Some(mut page), _) => {
                if self.pool.len() < MAX_POOLED_PAGES {
                    let serial = self.allocate_serial();
                    page.reset(serial);
                    self.pool.push(page);
                }
            }
            (None, Some(mut shared)) => {
                if self.shells.len() < MAX_POOLED_PAGES && Arc::get_mut(&mut shared).is_some() {
                    let serial = self.allocate_serial();
                    Arc::get_mut(&mut shared)
                        .expect("checked above")
                        .reset(serial);
                    self.shells.push(shared);
                }
            }
            (None, None) => {}
        }
    }

    fn debug_check_counts(&self) {
        if cfg!(debug_assertions) {
            debug_assert_eq!(
                self.retained_rows > self.hot_cap,
                self.over_cap.is_some(),
                "I13: over-cap reason out of step"
            );
            debug_assert!(self.sealed < self.pages.len().max(1), "I15: tail sealed");
        }
    }

    /// Checks the list invariants of ADR section 6 (I12, I13, I15 and the
    /// page checks for every page). O(rows); for tests and debug sweeps.
    pub fn check_invariants(&self) -> Result<(), String> {
        if self.pool.len() > MAX_POOLED_PAGES {
            return Err(format!("{} pooled pages", self.pool.len()));
        }
        let mut serials = Vec::new();
        // Pooled pages are owned outright, so none can be shared.
        for page in &self.pool {
            if !page.is_clean() || page.cols() != self.cols || page.capacity() != self.page_rows {
                return Err(format!(
                    "pooled page {} is not a reset page of this geometry",
                    page.serial()
                ));
            }
            serials.push(page.serial());
        }
        if self.shells.len() > MAX_POOLED_PAGES {
            return Err(format!("{} pooled shells", self.shells.len()));
        }
        for shell in &self.shells {
            if Arc::strong_count(shell) != 1 || Arc::weak_count(shell) != 0 {
                return Err(format!("pooled shell {} is shared", shell.serial()));
            }
            if !shell.is_clean() || shell.cols() != self.cols || shell.capacity() != self.page_rows
            {
                return Err(format!(
                    "pooled shell {} is not a reset page of this geometry",
                    shell.serial()
                ));
            }
            serials.push(shell.serial());
        }
        let mut retained = 0;
        let cutoff = self.seal_cutoff();
        let last = self.pages.len().saturating_sub(1);
        for (index, slot) in self.pages.iter().enumerate() {
            let page = slot.page();
            let at = format!("page {} (serial {})", index, page.serial());
            if page.cols() != self.cols || page.capacity() != self.page_rows {
                return Err(format!("{} has the wrong geometry", at));
            }
            if slot.front_trim > page.used() || (index > 0 && slot.front_trim != 0) {
                return Err(format!(
                    "{} front trim {} is misplaced",
                    at, slot.front_trim
                ));
            }
            if index < last && (!slot.is_full() || slot.retained() == 0) {
                return Err(format!(
                    "{} is not the tail but is not full of retained rows",
                    at
                ));
            }
            if index > 0 && slot.first_stable != self.pages[index - 1].end_stable() {
                return Err(format!("{} does not follow its predecessor", at)); // I13
            }
            retained += slot.retained();
            serials.push(page.serial());
            let sealed = index < self.sealed;
            if sealed && (index == last || slot.end_stable() - 1 > cutoff) {
                return Err(format!("{} is sealed too close to the viewport", at));
                // I15
            }
            if !sealed && index < last && slot.end_stable() - 1 <= cutoff {
                return Err(format!("{} is eligible but not sealed", at));
            }
            if sealed != slot.sealed.is_some() || slot.active.is_some() == slot.sealed.is_some() {
                return Err(format!(
                    "{} is {} but held {}",
                    at,
                    if sealed { "sealed" } else { "active" },
                    if slot.active.is_some() {
                        "owned"
                    } else {
                        "shared"
                    }
                ));
            }
            page.check_invariants()
                .map_err(|err| format!("{}: {}", at, err))?;
        }
        if let Some(slot) = self.pages.back() {
            if slot.end_stable() != self.next_stable {
                return Err("the tail page does not end at the next stable index".to_string());
            }
        }
        if retained != self.retained_rows {
            return Err(format!(
                "retained_rows {} but the pages hold {}",
                self.retained_rows, retained
            )); // I13
        }
        if (self.retained_rows > self.hot_cap) != self.over_cap.is_some() {
            return Err(format!(
                "{} rows against cap {} with over-cap reason {:?}",
                self.retained_rows, self.hot_cap, self.over_cap
            )); // I13, Q4
        }
        serials.sort_unstable();
        if serials.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err("two pages share a serial".to_string()); // I12
        }
        if serials.last().is_some_and(|&max| max >= self.next_serial) {
            return Err("a page serial was not drawn from the list".to_string());
        }
        let first = self.first_stable();
        for floor in &self.range_floors {
            if floor.start >= floor.end || floor.end <= first {
                return Err(format!("stale or empty range floor {:?}", floor));
            }
        }
        if self.range_floors.len() > MAX_RANGE_FLOORS {
            return Err(format!("{} range floors", self.range_floors.len()));
        }
        Ok(())
    }
}
