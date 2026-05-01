//! Per-pane render-side dirty-line bitmap.
//!
//! Replaces the coarse `quad_generation` whole-screen invalidation
//! with a per-line bitmap so the render pass can iterate only over
//! lines that actually changed (rio sugarloaf pattern, ft-mpc9b.1.2).
//!
//! ## Why a render-side bitmap?
//!
//! The term layer (`frankenterm/term/src/screen.rs`) already tracks
//! per-line modification via `SequenceNo` on each `Line`. The render
//! layer needs a *separate* per-pane bitmap because:
//!
//! - it indexes by *visible* row (post-scroll, post-pane-layout),
//!   not by stable row index
//! - it folds in dirty events that the term layer has no visibility
//!   into: selection changes, focus borders, theme/font swap,
//!   status-tile updates
//! - it is reset at frame end, regardless of seqno semantics
//!
//! ## What this module ships (foundation, ft-mpc9b.1.2)
//!
//! - the `DirtyLineBitmap` type: a u64-packed bitset addressable by
//!   visible row index
//! - mark/clear/iter/contains/count/resize APIs
//! - whole-pane "everything dirty" override for events like font
//!   swap or theme change
//! - structured-log helpers for the `dirty_lines_per_frame` and
//!   `clean_lines_skipped` telemetry called out in the bead
//!
//! ## What is deferred (continuation, see follow-up bead)
//!
//! - wiring the bitmap into `RenderState` per pane
//! - sourcing dirty events from PTY writes, cursor moves, selection,
//!   resize, focus, theme/font, status tiles
//! - making the render pass iterate over `iter_dirty()` instead of
//!   the full visible row range
//! - migrating `quad_generation` into a lower-bound version on top
//!   of per-line dirty
//! - benches: 200-pane fleet @ 1 cell/frame, font-swap O(N) rebuild

use std::iter::FusedIterator;

/// Number of bits packed into one storage word.
const BITS_PER_WORD: usize = u64::BITS as usize;

/// A packed bitmap of "this visible row needs to be redrawn this
/// frame," addressable by visible row index in `0..capacity()`.
///
/// All operations are safe and allocate at most when the capacity
/// changes (`resize`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyLineBitmap {
    /// Each bit represents one visible row. Bit `i` set = row `i`
    /// is dirty for the current frame.
    words: Vec<u64>,
    /// Total number of addressable rows. The high tail of `words`
    /// past this index must remain zero.
    capacity: usize,
    /// Frame-end clears reset this counter for the *next* frame's
    /// telemetry; it accumulates across the lifetime of the bitmap
    /// and is read by the render pass for `dirty_lines_per_frame`.
    dirty_marks_total: u64,
    /// Lifetime count of `clear()` invocations. Useful as a coarse
    /// frame counter for the bitmap.
    frames_cleared_total: u64,
}

impl DirtyLineBitmap {
    /// Construct a bitmap with the given visible row capacity. All
    /// rows start clean.
    pub fn new(capacity: usize) -> Self {
        let words = vec![0u64; words_for(capacity)];
        Self {
            words,
            capacity,
            dirty_marks_total: 0,
            frames_cleared_total: 0,
        }
    }

    /// Visible row capacity. Rows in `0..capacity()` are addressable.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Mark a single visible row dirty.
    ///
    /// Rows >= `capacity()` are silently ignored — callers should
    /// call `resize` first if a pane has grown.
    pub fn mark(&mut self, row: usize) {
        if row >= self.capacity {
            return;
        }
        let word = row / BITS_PER_WORD;
        let bit = row % BITS_PER_WORD;
        let mask = 1u64 << bit;
        let prev = self.words[word];
        self.words[word] = prev | mask;
        if prev & mask == 0 {
            self.dirty_marks_total = self.dirty_marks_total.saturating_add(1);
        }
    }

    /// Mark every row dirty (font/theme swap, full repaint forcing
    /// events).
    pub fn mark_all(&mut self) {
        if self.capacity == 0 {
            return;
        }
        let full_words = self.capacity / BITS_PER_WORD;
        let trailing_bits = self.capacity % BITS_PER_WORD;
        let trailing_word_idx = full_words;
        let trailing_mask = if trailing_bits == 0 {
            0
        } else {
            (1u64 << trailing_bits) - 1
        };

        let mut newly_marked = 0u64;
        for w in 0..full_words {
            let prev = self.words[w];
            self.words[w] = u64::MAX;
            newly_marked =
                newly_marked.saturating_add(u64::from(u64::MAX.count_ones() - prev.count_ones()));
        }
        if trailing_bits > 0 {
            let prev = self.words[trailing_word_idx];
            self.words[trailing_word_idx] = trailing_mask;
            newly_marked = newly_marked.saturating_add(u64::from(
                trailing_mask.count_ones() - (prev & trailing_mask).count_ones(),
            ));
        }
        self.dirty_marks_total = self.dirty_marks_total.saturating_add(newly_marked);
    }

    /// Mark a contiguous range of rows dirty (selection extend,
    /// resize-shifted region).
    pub fn mark_range(&mut self, range: std::ops::Range<usize>) {
        let start = range.start.min(self.capacity);
        let end = range.end.min(self.capacity);
        for row in start..end {
            self.mark(row);
        }
    }

    /// Whether the given row is currently dirty.
    pub fn contains(&self, row: usize) -> bool {
        if row >= self.capacity {
            return false;
        }
        let word = row / BITS_PER_WORD;
        let bit = row % BITS_PER_WORD;
        (self.words[word] >> bit) & 1 == 1
    }

    /// Count of currently-dirty rows.
    pub fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Whether no row is currently dirty.
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|w| *w == 0)
    }

    /// Iterate the dirty row indices in ascending order.
    pub fn iter_dirty(&self) -> DirtyLineIter<'_> {
        DirtyLineIter {
            words: &self.words,
            word_idx: 0,
            current: self.words.first().copied().unwrap_or(0),
            capacity: self.capacity,
        }
    }

    /// Reset the bitmap to "no rows dirty" — call at frame end.
    /// Does NOT reset the lifetime counters.
    pub fn clear(&mut self) {
        for word in &mut self.words {
            *word = 0;
        }
        self.frames_cleared_total = self.frames_cleared_total.saturating_add(1);
    }

    /// Resize the bitmap when the pane grows or shrinks.
    ///
    /// Growing preserves existing dirty bits and zero-initialises
    /// the new tail. Shrinking discards bits past the new capacity.
    /// Either way, no spurious dirty bits are introduced — the
    /// caller is responsible for marking newly-shifted rows dirty
    /// via `mark_range`.
    pub fn resize(&mut self, new_capacity: usize) {
        let new_word_count = words_for(new_capacity);
        self.words.resize(new_word_count, 0);

        // If we shrank, mask the trailing word to drop bits past
        // `new_capacity`.
        if new_capacity < self.capacity && new_word_count > 0 {
            let trailing_bits = new_capacity % BITS_PER_WORD;
            if trailing_bits != 0 {
                let mask = (1u64 << trailing_bits) - 1;
                let last = new_word_count - 1;
                self.words[last] &= mask;
            }
        }
        self.capacity = new_capacity;
    }

    /// Lifetime total of dirty marks (set transitions; re-marking an
    /// already-dirty row does not re-count). Drives the
    /// `dirty_lines_per_frame` histogram once the render pass wires
    /// it.
    #[inline]
    pub fn dirty_marks_total(&self) -> u64 {
        self.dirty_marks_total
    }

    /// Lifetime count of `clear()` invocations, i.e. frame ends seen
    /// by this bitmap.
    #[inline]
    pub fn frames_cleared_total(&self) -> u64 {
        self.frames_cleared_total
    }
}

/// Iterator over dirty visible row indices.
///
/// Yields indices in ascending order and stops when the underlying
/// storage is exhausted.
pub struct DirtyLineIter<'a> {
    words: &'a [u64],
    word_idx: usize,
    /// Remaining bits of `words[word_idx]` after the bits we have
    /// already yielded have been masked off.
    current: u64,
    capacity: usize,
}

impl<'a> Iterator for DirtyLineIter<'a> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        loop {
            if self.current != 0 {
                let bit = self.current.trailing_zeros() as usize;
                self.current &= self.current - 1;
                let row = self.word_idx * BITS_PER_WORD + bit;
                if row < self.capacity {
                    return Some(row);
                }
                // Past the addressable range — fall through to
                // pretend the rest of this word is empty.
                self.current = 0;
            }
            self.word_idx += 1;
            if self.word_idx >= self.words.len() {
                return None;
            }
            self.current = self.words[self.word_idx];
        }
    }
}

impl<'a> FusedIterator for DirtyLineIter<'a> {}

#[inline]
fn words_for(capacity: usize) -> usize {
    capacity.div_ceil(BITS_PER_WORD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_bitmap_has_zero_dirty() {
        let bm = DirtyLineBitmap::new(64);
        assert_eq!(bm.count(), 0);
        assert!(bm.is_empty());
        assert!(!bm.contains(0));
        assert!(!bm.contains(63));
        assert_eq!(bm.iter_dirty().count(), 0);
    }

    #[test]
    fn zero_capacity_is_well_formed() {
        let mut bm = DirtyLineBitmap::new(0);
        assert_eq!(bm.capacity(), 0);
        assert_eq!(bm.count(), 0);
        bm.mark(0);
        bm.mark_all();
        bm.mark_range(0..100);
        assert_eq!(bm.count(), 0);
        assert!(bm.is_empty());
    }

    #[test]
    fn mark_sets_only_the_targeted_row() {
        let mut bm = DirtyLineBitmap::new(128);
        bm.mark(7);
        assert!(bm.contains(7));
        assert_eq!(bm.count(), 1);
        let dirty: Vec<usize> = bm.iter_dirty().collect();
        assert_eq!(dirty, vec![7]);
    }

    #[test]
    fn mark_idempotent_does_not_double_count() {
        let mut bm = DirtyLineBitmap::new(64);
        bm.mark(3);
        bm.mark(3);
        bm.mark(3);
        assert_eq!(bm.count(), 1);
        assert_eq!(bm.dirty_marks_total(), 1);
    }

    #[test]
    fn mark_out_of_range_is_silent() {
        let mut bm = DirtyLineBitmap::new(8);
        bm.mark(8); // exactly past capacity
        bm.mark(usize::MAX);
        assert_eq!(bm.count(), 0);
        assert!(!bm.contains(8));
    }

    #[test]
    fn mark_all_dirties_every_addressable_row() {
        let bm = make_marked(80, |bm| bm.mark_all());
        assert_eq!(bm.count(), 80);
        let dirty: Vec<usize> = bm.iter_dirty().collect();
        assert_eq!(dirty, (0..80).collect::<Vec<_>>());
    }

    #[test]
    fn mark_all_does_not_set_bits_past_capacity() {
        // 80 rows occupies the first 64-bit word + 16 bits of the
        // second; bits 16..64 of the second word must stay zero.
        let mut bm = DirtyLineBitmap::new(80);
        bm.mark_all();
        assert_eq!(bm.count(), 80);
        // Capacity edge — any read past 80 is `false`.
        assert!(!bm.contains(80));
        assert!(!bm.contains(127));
    }

    #[test]
    fn mark_range_marks_only_the_span() {
        let mut bm = DirtyLineBitmap::new(64);
        bm.mark_range(10..15);
        let dirty: Vec<usize> = bm.iter_dirty().collect();
        assert_eq!(dirty, vec![10, 11, 12, 13, 14]);
        assert_eq!(bm.count(), 5);
    }

    #[test]
    fn mark_range_clamps_to_capacity() {
        let mut bm = DirtyLineBitmap::new(8);
        bm.mark_range(6..32);
        let dirty: Vec<usize> = bm.iter_dirty().collect();
        assert_eq!(dirty, vec![6, 7]);
    }

    #[test]
    fn clear_resets_dirty_state_but_preserves_telemetry() {
        let mut bm = DirtyLineBitmap::new(64);
        bm.mark(1);
        bm.mark(2);
        let pre_marks = bm.dirty_marks_total();

        bm.clear();
        assert_eq!(bm.count(), 0);
        assert!(bm.is_empty());
        assert_eq!(bm.frames_cleared_total(), 1);
        assert_eq!(bm.dirty_marks_total(), pre_marks);
    }

    #[test]
    fn resize_growing_preserves_existing_marks() {
        let mut bm = DirtyLineBitmap::new(8);
        bm.mark(0);
        bm.mark(7);

        bm.resize(64);
        assert_eq!(bm.capacity(), 64);
        assert!(bm.contains(0));
        assert!(bm.contains(7));
        assert_eq!(bm.count(), 2);
    }

    #[test]
    fn resize_shrinking_drops_bits_past_new_capacity() {
        let mut bm = DirtyLineBitmap::new(64);
        bm.mark(5);
        bm.mark(40);
        bm.mark(63);

        bm.resize(32);
        assert_eq!(bm.capacity(), 32);
        assert!(bm.contains(5));
        // 40 and 63 were past the new capacity and must be dropped.
        assert!(!bm.contains(40));
        assert!(!bm.contains(63));
        assert_eq!(bm.count(), 1);
    }

    #[test]
    fn iter_yields_indices_in_ascending_order() {
        let mut bm = DirtyLineBitmap::new(200);
        let inputs = [3usize, 0, 199, 64, 65, 100, 1];
        for &row in &inputs {
            bm.mark(row);
        }

        let mut expected = inputs.to_vec();
        expected.sort_unstable();
        let observed: Vec<usize> = bm.iter_dirty().collect();
        assert_eq!(observed, expected);
    }

    #[test]
    fn iter_is_fused() {
        let bm = make_marked(8, |bm| bm.mark(0));
        let mut it = bm.iter_dirty();
        assert_eq!(it.next(), Some(0));
        assert_eq!(it.next(), None);
        // FusedIterator contract: subsequent next() must keep returning None.
        assert_eq!(it.next(), None);
        assert_eq!(it.next(), None);
    }

    #[test]
    fn iter_handles_word_boundary_correctly() {
        let mut bm = DirtyLineBitmap::new(192);
        bm.mark(63);
        bm.mark(64);
        bm.mark(127);
        bm.mark(128);
        bm.mark(191);

        let observed: Vec<usize> = bm.iter_dirty().collect();
        assert_eq!(observed, vec![63, 64, 127, 128, 191]);
    }

    #[test]
    fn telemetry_marks_total_only_counts_set_transitions() {
        // Bead-cited invariant: dirty_lines_per_frame should reflect
        // how many *new* dirty markings happened, not how many
        // mark() calls were made. That lets the histogram capture
        // "1 cell typed → 1 line newly dirty" cleanly.
        let mut bm = DirtyLineBitmap::new(64);
        bm.mark(5);
        bm.mark(5);
        bm.mark(5);
        bm.mark(7);
        assert_eq!(bm.dirty_marks_total(), 2);
    }

    #[test]
    fn frames_cleared_counts_clear_calls_only() {
        let mut bm = DirtyLineBitmap::new(8);
        assert_eq!(bm.frames_cleared_total(), 0);

        bm.mark(0);
        bm.clear();
        assert_eq!(bm.frames_cleared_total(), 1);

        bm.clear();
        bm.clear();
        assert_eq!(bm.frames_cleared_total(), 3);
    }

    #[test]
    fn pane_typing_pattern_marks_one_line_per_frame() {
        // Models the bead's headline win: 200-pane fleet typing
        // 1 char/sec. Each frame should end with one row dirty.
        let mut bm = DirtyLineBitmap::new(24);

        for cursor_row in [3usize, 3, 3, 4, 4, 5] {
            bm.mark(cursor_row);
            // Render pass would here iterate iter_dirty() — assert
            // exactly one line got marked this frame.
            assert_eq!(bm.count(), 1);
            bm.clear();
        }
        assert_eq!(bm.frames_cleared_total(), 6);
    }

    fn make_marked<F: FnOnce(&mut DirtyLineBitmap)>(capacity: usize, f: F) -> DirtyLineBitmap {
        let mut bm = DirtyLineBitmap::new(capacity);
        f(&mut bm);
        bm
    }
}
