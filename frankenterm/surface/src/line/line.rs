#![allow(unexpected_cfgs)]

use crate::cellcluster::CellCluster;
use crate::hyperlink::Rule;
use crate::line::cellref::CellRef;
use crate::line::clusterline::ClusteredLine;
use crate::line::linebits::LineBits;
use crate::line::storage::{CellStorage, VisibleCellIter};
use crate::line::vecstorage::{HyperlinkCellMatch, VecStorage};
use crate::{Change, SequenceNo, SEQ_ZERO};
use alloc::borrow::Cow;
use alloc::sync::Arc;
#[cfg(feature = "appdata")]
use alloc::sync::Weak;
#[cfg(feature = "appdata")]
use core::any::Any;
use core::cmp::Ordering;
use core::hash::Hash;
use core::ops::Range;
use finl_unicode::grapheme_clusters::Graphemes;
use frankenterm_bidi::{Direction, ParagraphDirectionHint};
use frankenterm_cell::{Cell, CellAttributes, SemanticType, UnicodeVersion};
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Serialize};
use siphasher::sip128::{Hasher128, SipHasher};
#[cfg(feature = "appdata")]
use std::sync::{Mutex, MutexGuard};

extern crate alloc;
use crate::alloc::string::ToString;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

// A physical terminal row cannot exceed the u16 column contract used by
// portable_pty::PtySize and the guardian/mux wire protocols.  Keeping the
// materialization fence at u32::MAX made an otherwise valid range starting at
// zero capable of allocating or iterating billions of cells before it was
// rejected.  Bound every direct Line mutation to the same representable
// physical-row width used at the PTY boundary.
const MAX_MATERIALIZED_LINE_LEN: usize = u16::MAX as usize;

#[cfg(all(test, feature = "std"))]
std::thread_local! {
    static REFLOW_CONTENT_SHAPE_HASH_SCANS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    static WRAP_PHYSICAL_ROWS_CREATED: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    static WRAP_PLANNER_CALLS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

fn normalize_cell_width(width: usize) -> usize {
    width.clamp(1, 2)
}

fn checked_materialized_end(idx: usize, width: usize) -> Option<usize> {
    let end = idx.checked_add(width)?;
    (end <= MAX_MATERIALIZED_LINE_LEN).then_some(end)
}

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneRange {
    pub semantic_type: SemanticType,
    pub range: Range<u16>,
}

#[derive(Debug)]
struct HyperlinkScanCell {
    byte_range: Range<usize>,
    cell_index: usize,
}

#[derive(Debug, Default)]
struct HyperlinkScanRun {
    text: String,
    cells: Vec<HyperlinkScanCell>,
}

impl HyperlinkScanRun {
    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn push(&mut self, cell: CellRef<'_>) {
        let start = self.text.len();
        self.text.push_str(cell.str());
        let end = self.text.len();
        if start != end {
            self.cells.push(HyperlinkScanCell {
                byte_range: start..end,
                cell_index: cell.cell_index(),
            });
            // A wide cell occupies `width` columns but contributes a single
            // grapheme to `text`; the trailing column(s) are blank spacer
            // cells that `visible_cells()` skips. Emit them as spaces so the
            // wide grapheme is not glued to the following cell's text. Without
            // this, a CJK/emoji cell immediately before an ASCII URL (e.g.
            // "中http://example.com") is swallowed into the rule's `\w+` run
            // and the whole "中http://example.com" is mis-linked onto the wide
            // cell. The padding bytes are intentionally NOT recorded in
            // `self.cells`, so they belong to no cell and can never receive an
            // implicit hyperlink (mirrors the spacer cells they stand in for).
            for _ in 1..cell.width() {
                self.text.push(' ');
            }
        }
    }

    fn finish_into(self, runs: &mut Vec<Self>) {
        if !self.is_empty() {
            runs.push(self);
        }
    }
}

fn hyperlink_cell_matches(
    scan_runs: Vec<HyperlinkScanRun>,
    rules: &[Rule],
) -> Vec<HyperlinkCellMatch> {
    let mut cell_matches = Vec::new();
    for scan_run in scan_runs {
        for matched in Rule::match_hyperlinks(&scan_run.text, rules) {
            let Some(first_cell_idx) = scan_run
                .cells
                .iter()
                .position(|cell| cell.byte_range.start == matched.range.start)
            else {
                continue;
            };

            let mut cell_indices = Vec::new();
            let mut match_end_aligned = false;
            for cell in scan_run.cells.iter().skip(first_cell_idx) {
                if cell.byte_range.start >= matched.range.end {
                    break;
                }
                if cell.byte_range.end > matched.range.end {
                    cell_indices.clear();
                    break;
                }

                cell_indices.push(cell.cell_index);
                if cell.byte_range.end == matched.range.end {
                    match_end_aligned = true;
                    break;
                }
            }

            if match_end_aligned && !cell_indices.is_empty() {
                cell_matches.push(HyperlinkCellMatch {
                    cell_indices,
                    link: matched.link,
                });
            }
        }
    }

    cell_matches
}

#[derive(Debug, Clone, PartialEq)]
pub enum DoubleClickRange {
    Range(Range<usize>),
    RangeWithWrap(Range<usize>),
}

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub struct Line {
    pub(crate) cells: CellStorage,
    zones: Vec<ZoneRange>,
    seqno: SequenceNo,
    bits: LineBits,
    #[cfg(feature = "appdata")]
    #[cfg_attr(feature = "use_serde", serde(skip))]
    appdata: Mutex<Option<Weak<dyn Any + Send + Sync>>>,
}

impl core::fmt::Debug for Line {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Line")
            .field("cells", &self.cells)
            .field("zones", &self.zones)
            .field("seqno", &self.seqno)
            .field("bits", &self.bits)
            .finish()
    }
}

#[cfg(feature = "appdata")]
fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl Clone for Line {
    fn clone(&self) -> Self {
        Self {
            cells: self.cells.clone(),
            zones: self.zones.clone(),
            seqno: self.seqno,
            bits: self.bits,
            #[cfg(feature = "appdata")]
            appdata: Mutex::new(lock_or_recover(&self.appdata).clone()),
        }
    }
}

impl PartialEq for Line {
    fn eq(&self, other: &Self) -> bool {
        self.seqno == other.seqno && self.bits == other.bits && self.cells == other.cells
    }
}

impl Line {
    /// Bounded UI snapshot. Vector cells share their immutable allocation even
    /// in the eager-clone profiling mode; clustered rows also share immutable
    /// storage, retaining conservative payload charges and metadata visit caps.
    /// Payload serialization is never
    /// used for admission. Appdata is an optional weak cache, not source data.
    pub fn try_clone_for_snapshot(
        &self,
        bytes_left: &mut usize,
        work_left: &mut usize,
    ) -> Option<Self> {
        self.charge_snapshot(bytes_left, work_left)?;
        Some(self.clone_precharged_snapshot())
    }

    /// Admit both slices before cloning any row. This is intended for a
    /// resident VecDeque window: late-row refusal cannot destroy an already
    /// cloned clustered prefix while the caller still holds its terminal lock.
    /// Both admission and materialization visits share the caller's work
    /// budget. Failed admission never refunds charges from earlier rows.
    pub fn try_clone_batch_for_snapshot(
        first: &[Self],
        second: &[Self],
        bytes_left: &mut usize,
        work_left: &mut usize,
    ) -> Option<Vec<Self>> {
        let work_before = *work_left;
        for line in first.iter().chain(second) {
            line.charge_snapshot(bytes_left, work_left)?;
        }
        let clone_work = work_before.checked_sub(*work_left)?;
        *work_left = work_left.checked_sub(clone_work)?;
        Some(
            first
                .iter()
                .chain(second)
                .map(Self::clone_precharged_snapshot)
                .collect(),
        )
    }

    fn charge_snapshot(&self, bytes_left: &mut usize, work_left: &mut usize) -> Option<()> {
        let zone_bytes = self
            .zones
            .len()
            .checked_mul(core::mem::size_of::<ZoneRange>())?;
        let base = core::mem::size_of::<Self>().checked_add(zone_bytes)?;
        let work = self.zones.len().checked_add(1)?;
        let available_work = work_left.checked_sub(work)?;
        let (payload, visits) = match &self.cells {
            CellStorage::V(_) => (0, 0),
            CellStorage::C(line) => line.snapshot_clone_cost(available_work)?,
        };
        let charge = base.checked_add(payload)?;
        let remaining = bytes_left.checked_sub(charge)?;
        let remaining_work = available_work.checked_sub(visits)?;
        *bytes_left = remaining;
        *work_left = remaining_work;
        Some(())
    }

    fn clone_precharged_snapshot(&self) -> Self {
        Self {
            cells: match &self.cells {
                CellStorage::V(cells) => CellStorage::V(cells.snapshot_clone()),
                CellStorage::C(line) => CellStorage::C(line.clone()),
            },
            zones: self.zones.clone(),
            seqno: self.seqno,
            bits: self.bits,
            #[cfg(feature = "appdata")]
            appdata: Mutex::new(self.appdata.try_lock().ok().and_then(|cache| cache.clone())),
        }
    }

    /// Retain only immutable semantic source data for a mutable callback fence.
    /// Cells stay shared even in eager-clone profiling mode; caches are omitted.
    pub fn semantic_snapshot(&self) -> Self {
        Self {
            cells: match &self.cells {
                CellStorage::V(cells) => CellStorage::V(cells.snapshot_clone()),
                CellStorage::C(line) => CellStorage::C(Arc::clone(line)),
            },
            zones: Vec::new(),
            seqno: self.seqno,
            bits: self.bits,
            #[cfg(feature = "appdata")]
            appdata: Mutex::new(None),
        }
    }

    /// Compare checkpoint-relevant row state, excluding renderer caches and
    /// hyperlink scan/presence flags derived from the actual cell attributes.
    /// Unchanged shared cells compare in constant time.
    pub fn matches_semantic_snapshot(&self, snapshot: &Self) -> bool {
        let cache_bits = LineBits::HAS_HYPERLINK
            | LineBits::HAS_IMPLICIT_HYPERLINKS
            | LineBits::SCANNED_IMPLICIT_HYPERLINKS;
        if self.seqno != snapshot.seqno || (self.bits - cache_bits) != (snapshot.bits - cache_bits)
        {
            return false;
        }
        if self.cells == snapshot.cells {
            return true;
        }
        // Compression and deferred-row materialization may change storage
        // without changing the visible cells that a checkpoint preserves.
        let mut before = snapshot.visible_cells();
        for cell in self.visible_cells() {
            let Some(prior) = before.next() else {
                return false;
            };
            if cell.cell_index() != prior.cell_index()
                || cell.str() != prior.str()
                || cell.width() != prior.width()
                || cell.attrs() != prior.attrs()
            {
                return false;
            }
        }
        before.next().is_none()
    }

    /// Conservative, exact source check for work prepared from a cloned line.
    /// Image payloads can change through shared handles, so they never qualify.
    /// Renderer appdata and a no-match hyperlink scan do not change wrapping;
    /// all cell attributes, layout bits and mutation sequence numbers do.
    pub fn is_same_reflow_source(&self, other: &Self) -> bool {
        self.seqno == other.seqno && self.is_same_reflow_content(other)
    }

    /// Exact immutable wrapping content, independent of publication seqno.
    /// Use this only when comparing an already-published target with current
    /// rows; prepared-work admission still requires `is_same_reflow_source`.
    /// Shared copy-on-write cells avoid rescanning unchanged row contents.
    /// Images, all cell attributes and the same normalized layout bits retain
    /// the conservative source-check contract above.
    pub fn is_same_reflow_content(&self, other: &Self) -> bool {
        #[cfg(feature = "use_image")]
        if self.has_image_attachments() {
            return false;
        }
        let normalized_bits = |line: &Self| {
            let mut bits = line.bits;
            if !line.has_hyperlink() {
                bits.remove(LineBits::SCANNED_IMPLICIT_HYPERLINKS);
            }
            bits
        };
        normalized_bits(self) == normalized_bits(other)
            && match (&self.cells, &other.cells) {
                (CellStorage::V(left), CellStorage::V(right)) => {
                    left.shares_cells_with(right) || left == right
                }
                _ => self.cells == other.cells,
            }
    }

    pub fn with_width_and_cell(width: usize, cell: Cell, seqno: SequenceNo) -> Self {
        let mut cells = Vec::with_capacity(width);
        cells.resize(width, cell.clone());
        let bits = LineBits::NONE;
        Self {
            bits,
            cells: CellStorage::V(VecStorage::new(cells)),
            seqno,
            zones: vec![],
            #[cfg(feature = "appdata")]
            appdata: Mutex::new(None),
        }
    }

    pub fn from_cells(cells: Vec<Cell>, seqno: SequenceNo) -> Self {
        let bits = LineBits::NONE;
        Self {
            bits,
            cells: CellStorage::V(VecStorage::new(cells)),
            seqno,
            zones: vec![],
            #[cfg(feature = "appdata")]
            appdata: Mutex::new(None),
        }
    }

    /// Create a new line using cluster storage, optimized for appending
    /// and lower memory utilization.
    /// The line will automatically switch to cell storage when necessary
    /// to apply edits.
    pub fn new(seqno: SequenceNo) -> Self {
        Self {
            bits: LineBits::NONE,
            cells: CellStorage::C(Arc::new(ClusteredLine::new())),
            seqno,
            zones: vec![],
            #[cfg(feature = "appdata")]
            appdata: Mutex::new(None),
        }
    }

    /// Computes a hash over the line that will change if the way that
    /// the line contents are shaped would change.
    /// This is independent of the seqno and is based purely on the
    /// content of the line.
    ///
    /// Line doesn't implement Hash in terms of this function as compute_shape_hash
    /// doesn't include every possible bit of internal state, and we don't want to
    /// encourage using Line directly as a hash key.
    pub fn compute_shape_hash(&self) -> [u8; 16] {
        #[cfg(feature = "std")]
        if let CellStorage::V(cells) = &self.cells {
            return cells
                .cached_shape_hash(self.bits.bits(), || self.compute_shape_hash_uncached());
        }
        self.compute_shape_hash_uncached()
    }

    fn compute_shape_hash_uncached(&self) -> [u8; 16] {
        #[cfg(all(test, feature = "std"))]
        REFLOW_CONTENT_SHAPE_HASH_SCANS.with(|count| count.set(count.get() + 1));
        let mut hasher = SipHasher::new();
        self.bits.bits().hash(&mut hasher);
        for cell in self.visible_cells() {
            cell.compute_shape_hash(&mut hasher);
        }
        hasher.finish128().as_bytes()
    }

    pub fn with_width(width: usize, seqno: SequenceNo) -> Self {
        let mut cells = Vec::with_capacity(width);
        cells.resize_with(width, Cell::blank);
        let bits = LineBits::NONE;
        Self {
            bits,
            cells: CellStorage::V(VecStorage::new(cells)),
            seqno,
            zones: vec![],
            #[cfg(feature = "appdata")]
            appdata: Mutex::new(None),
        }
    }

    pub fn from_text(
        s: &str,
        attrs: &CellAttributes,
        seqno: SequenceNo,
        unicode_version: Option<&UnicodeVersion>,
    ) -> Line {
        let mut cells = Vec::new();

        for sub in Graphemes::new(s) {
            let cell = Cell::new_grapheme(sub, attrs.clone(), unicode_version);
            let width = cell.width();
            cells.push(cell);
            for _ in 1..width {
                cells.push(Cell::new(' ', attrs.clone()));
            }
        }

        Line {
            cells: CellStorage::V(VecStorage::new(cells)),
            bits: LineBits::NONE,
            seqno,
            zones: vec![],
            #[cfg(feature = "appdata")]
            appdata: Mutex::new(None),
        }
    }

    pub fn from_text_with_wrapped_last_col(
        s: &str,
        attrs: &CellAttributes,
        seqno: SequenceNo,
    ) -> Line {
        let mut line = Self::from_text(s, attrs, seqno, None);
        line.set_last_cell_was_wrapped(true, seqno);
        line
    }

    pub fn resize_and_clear(
        &mut self,
        width: usize,
        seqno: SequenceNo,
        blank_attr: CellAttributes,
    ) {
        {
            let cells = self.coerce_vec_storage();
            for c in cells.iter_mut() {
                *c = Cell::blank_with_attrs(blank_attr.clone());
            }
            cells.resize_with(width, || Cell::blank_with_attrs(blank_attr.clone()));
            cells.shrink_to_fit();
        }
        self.update_last_change_seqno(seqno);
        self.invalidate_zones();
        self.bits = LineBits::NONE;
    }

    pub fn resize(&mut self, width: usize, seqno: SequenceNo) {
        let cells = self.coerce_vec_storage();
        let old_cap = cells.capacity();
        cells.resize_with(width, Cell::blank);
        // Reclaim excess capacity when shrinking to a smaller width.
        // Without this, lines that were once wide retain their old
        // allocation, causing fragmentation across thousands of lines.
        if width < old_cap / 2 {
            cells.shrink_to_fit();
        }
        self.update_last_change_seqno(seqno);
        self.invalidate_zones();
    }

    /// Wrap the line so that it fits within the provided width.
    /// Returns the list of resultant line(s)
    pub fn wrap(self, width: usize, seqno: SequenceNo) -> Vec<Self> {
        self.wrap_with_cost_model(width, seqno, MonospaceKpCostModel::terminal_default())
            .0
    }

    /// Wrap the line using an explicit bounded Knuth-Plass cost model.
    /// Returns wrapped lines and the execution mode (`dp` or `fallback`).
    pub fn wrap_with_cost_model(
        self,
        width: usize,
        seqno: SequenceNo,
        cost_model: MonospaceKpCostModel,
    ) -> (Vec<Self>, MonospaceWrapMode) {
        self.wrap_with_report(width, seqno, cost_model).into_parts()
    }

    /// Wrap the line using an explicit bounded Knuth-Plass cost model while
    /// reusing caller-owned width-prefix scratch.
    pub fn wrap_with_cost_model_and_width_prefix_scratch(
        self,
        width: usize,
        seqno: SequenceNo,
        cost_model: MonospaceKpCostModel,
        width_prefix_scratch: &mut LineWrapWidthPrefixScratch,
    ) -> (Vec<Self>, MonospaceWrapMode) {
        self.wrap_with_report_and_width_prefix_scratch(
            width,
            seqno,
            cost_model,
            width_prefix_scratch,
        )
        .into_parts()
    }

    /// Wrap the line and return a scorecard that can be used by
    /// resize-time readability gates.
    pub fn wrap_with_report(
        self,
        width: usize,
        seqno: SequenceNo,
        cost_model: MonospaceKpCostModel,
    ) -> LineWrapReport {
        let mut width_prefix_scratch = LineWrapWidthPrefixScratch::default();
        self.wrap_with_report_and_width_prefix_scratch(
            width,
            seqno,
            cost_model,
            &mut width_prefix_scratch,
        )
    }

    /// Wrap the line and return a scorecard while reusing caller-owned
    /// prefix-width scratch for Knuth-Plass line-width queries.
    pub fn wrap_with_report_and_width_prefix_scratch(
        self,
        width: usize,
        seqno: SequenceNo,
        cost_model: MonospaceKpCostModel,
        width_prefix_scratch: &mut LineWrapWidthPrefixScratch,
    ) -> LineWrapReport {
        self.plan_wrap_with_width_prefix_scratch(width, cost_model, width_prefix_scratch)
            .into_report(seqno)
    }

    /// Compute wrapping without allocating physical output rows. The returned
    /// plan owns its source, so callers can materialize a viewport later without
    /// borrowing mutable terminal state.
    pub fn plan_wrap_with_width_prefix_scratch(
        self,
        width: usize,
        cost_model: MonospaceKpCostModel,
        width_prefix_scratch: &mut LineWrapWidthPrefixScratch,
    ) -> LineWrapLayout {
        self.plan_wrap_with_trailing_space_policy(width, cost_model, width_prefix_scratch, false)
    }

    /// Wrap an incomplete logical prefix without discarding its final separator
    /// cells. The continuation may live in a different scrollback tier.
    pub fn plan_wrap_preserving_trailing_spaces(
        self,
        width: usize,
        cost_model: MonospaceKpCostModel,
        width_prefix_scratch: &mut LineWrapWidthPrefixScratch,
    ) -> LineWrapLayout {
        self.plan_wrap_with_trailing_space_policy(width, cost_model, width_prefix_scratch, true)
    }

    fn plan_wrap_with_trailing_space_policy(
        self,
        width: usize,
        cost_model: MonospaceKpCostModel,
        width_prefix_scratch: &mut LineWrapWidthPrefixScratch,
        preserve_trailing_spaces: bool,
    ) -> LineWrapLayout {
        #[cfg(feature = "std")]
        let mut image_free = true;
        let cells = self.visible_cells();
        #[cfg(feature = "std")]
        let cells = cells.inspect(|cell| {
            image_free &= !cell.attrs().has_image_attachments();
        });
        let mut cells: Vec<CellRef> = cells.collect();
        let end_idx = if preserve_trailing_spaces {
            cells.len().checked_sub(1)
        } else {
            cells.iter().rposition(|c| c.str() != " ")
        };
        if let Some(end_idx) = end_idx {
            cells.truncate(end_idx + 1);
            #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
            let geometry_hash = compute_wrap_geometry_hash(self.bits, &cells);
            let reusable = match &self.cells {
                CellStorage::V(storage) => storage.reusable_wrap_tokens(),
                CellStorage::C(_) => None,
            };
            let (tokens, token_range) =
                match reusable.filter(|(_, range)| range.len() >= cells.len()) {
                    Some((tokens, range)) => {
                        // Trimming trailing spaces narrows the source view; it
                        // must not copy every retained cell at each new width.
                        let end = range.start + cells.len();
                        (tokens, range.start..end)
                    }
                    None => {
                        #[cfg(feature = "std")]
                        let tokens: Arc<[Cell]> = cells.iter().map(CellRef::as_cell).collect();
                        #[cfg(not(feature = "std"))]
                        let tokens: Arc<[Cell]> =
                            cells.into_iter().map(|cell| cell.as_cell()).collect();
                        let end = tokens.len();
                        (tokens, 0..end)
                    }
                };
            let layout = plan_wrap_tokens(
                tokens,
                token_range,
                #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
                geometry_hash,
                width,
                cost_model,
                None,
                width_prefix_scratch,
            );
            #[cfg(feature = "std")]
            let layout = {
                let mut layout = layout;
                layout.image_free = image_free;
                if image_free {
                    layout.row_widths = layout.row_widths_from_cells(&cells);
                }
                layout
            };
            layout
        } else {
            width_prefix_scratch.clear();
            LineWrapLayout {
                tokens: Arc::from([]),
                token_range: 0..0,
                width_prefix: None,
                #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
                geometry_hash: [0; 16],
                break_offsets: Vec::new(),
                #[cfg(feature = "std")]
                row_widths: None,
                #[cfg(feature = "std")]
                image_free: false,
                blank: Some(self),
                scorecard: LineWrapScorecard {
                    mode: MonospaceWrapMode::Fallback,
                    greedy_total_cost: 0,
                    selected_total_cost: 0,
                    badness_delta: 0,
                    greedy_forced_breaks: 0,
                    selected_forced_breaks: 0,
                    line_count: 1,
                    estimated_states: 0,
                    evaluated_states: 0,
                },
            }
        }
    }

    /// Set arbitrary application specific data for the line.
    /// Only one piece of appdata can be tracked per line,
    /// so this is only suitable for the overall application
    /// and not for use by "middleware" crates.
    /// A Weak reference is stored.
    /// `get_appdata` is used to retrieve a previously stored reference.
    #[cfg(feature = "appdata")]
    pub fn set_appdata<T: Any + Send + Sync>(&self, appdata: Arc<T>) {
        let appdata: Arc<dyn Any + Send + Sync> = appdata;
        lock_or_recover(&self.appdata).replace(Arc::downgrade(&appdata));
    }

    #[cfg(feature = "appdata")]
    pub fn clear_appdata(&self) {
        lock_or_recover(&self.appdata).take();
    }

    /// Retrieve the appdata for the line, if any.
    /// This may return None in the case where the underlying data has
    /// been released: Line only stores a Weak reference to it.
    #[cfg(feature = "appdata")]
    pub fn get_appdata(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        lock_or_recover(&self.appdata)
            .as_ref()
            .and_then(|data| data.upgrade())
    }

    /// Copy the application-data reference from an independently cloned line.
    ///
    /// [`Line::clone`] intentionally gives each clone its own mutex so later
    /// content mutations cannot race through shared cache metadata. Copy-backed
    /// pane implementations can nevertheless project a line to a renderer and,
    /// after proving that the authoritative content is still identical, use
    /// this method to retain cache metadata produced on that projection.
    #[cfg(feature = "appdata")]
    pub fn copy_appdata_from(&self, source: &Self) {
        if std::ptr::eq(self, source) {
            return;
        }
        let source_appdata = lock_or_recover(&source.appdata).clone();
        *lock_or_recover(&self.appdata) = source_appdata;
    }

    /// Returns true if the line's last changed seqno is more recent
    /// than the provided seqno parameter
    pub fn changed_since(&self, seqno: SequenceNo) -> bool {
        self.seqno == SEQ_ZERO || self.seqno > seqno
    }

    pub fn current_seqno(&self) -> SequenceNo {
        self.seqno
    }

    /// Number of semantic-zone records serialized with this line.
    pub fn zone_count(&self) -> usize {
        self.zones.len()
    }

    /// Returns whether any raw cell or compressed attribute run carries an
    /// out-of-band image attachment.
    ///
    /// Inspecting the raw vector storage (rather than `visible_cells`) matters
    /// for validation: a malformed serialized line must not be able to conceal
    /// an image in a spacer cell skipped by the visible-cell iterator.
    #[cfg(feature = "use_image")]
    pub fn has_image_attachments(&self) -> bool {
        match &self.cells {
            CellStorage::V(cells) => cells.has_image_attachments(),
            CellStorage::C(line) => line.has_image_attachments(),
        }
    }

    /// Materialize the semantic line state used by terminal checkpoints.
    ///
    /// The result deliberately normalizes compressed/vector storage, omits
    /// derived zone and hyperlink-scan caches, and preserves each grapheme's
    /// authoritative stored width.  Consequently equivalent live lines have a
    /// single persistence representation regardless of their cache history.
    #[cfg(feature = "use_serde")]
    #[doc(hidden)]
    pub fn semantic_checkpoint_clone(&self) -> Self {
        let mut cells = Vec::with_capacity(self.len());
        for cell in self.visible_cells() {
            let width = cell.width();
            let attrs = cell.attrs().clone();
            cells.push(Cell::new_grapheme_with_width(
                cell.str(),
                width,
                attrs.clone(),
            ));
            for _ in 1..width {
                cells.push(Cell::blank_with_attrs(attrs.clone()));
            }
        }

        let mut line = Self::from_cells(cells, self.seqno);
        if self.is_double_height_top() {
            line.set_double_height_top(self.seqno);
        } else if self.is_double_height_bottom() {
            line.set_double_height_bottom(self.seqno);
        } else if self.is_double_width() {
            line.set_double_width(self.seqno);
        }
        let (bidi_enabled, bidi_hint) = self.bidi_info();
        line.set_bidi_info(bidi_enabled, bidi_hint, self.seqno);
        line.rebuild_checkpoint_hyperlink_bits();
        line
    }

    /// Rebuild hyperlink presence/scanning bits after a semantic checkpoint
    /// reconstruction.  The bits are derived from cell attributes and must not
    /// inherit storage/cache history, but preserved implicit hyperlinks need to
    /// remain marked as already scanned until the caller advances its rule
    /// epoch; otherwise a no-op rescan would clear their presence bit.
    #[cfg(feature = "use_serde")]
    #[doc(hidden)]
    pub fn rebuild_checkpoint_hyperlink_bits(&mut self) {
        let mut has_explicit = false;
        let mut has_implicit = false;
        for cell in self.visible_cells() {
            match cell.attrs().hyperlink() {
                Some(link) if link.is_implicit() => has_implicit = true,
                Some(_) => has_explicit = true,
                None => {}
            }
        }
        self.bits.set(LineBits::HAS_HYPERLINK, has_explicit);
        self.bits
            .set(LineBits::HAS_IMPLICIT_HYPERLINKS, has_implicit);
        self.bits
            .set(LineBits::SCANNED_IMPLICIT_HYPERLINKS, has_implicit);
    }

    /// Annotate the line with the sequence number of a change.
    /// This can be used together with Line::changed_since to
    /// manage caching and rendering
    #[inline]
    pub fn update_last_change_seqno(&mut self, seqno: SequenceNo) {
        self.seqno = self.seqno.max(seqno);
    }

    /// Check whether the line is single-width.
    #[inline]
    pub fn is_single_width(&self) -> bool {
        (self.bits
            & (LineBits::DOUBLE_WIDTH
                | LineBits::DOUBLE_HEIGHT_TOP
                | LineBits::DOUBLE_HEIGHT_BOTTOM))
            == LineBits::NONE
    }

    /// Force single-width.  This also implicitly sets
    /// double-height-(top/bottom) and dirty.
    #[inline]
    pub fn set_single_width(&mut self, seqno: SequenceNo) {
        self.bits.remove(LineBits::DOUBLE_WIDTH_HEIGHT_MASK);
        self.update_last_change_seqno(seqno);
    }

    /// Check whether the line is double-width and not double-height.
    #[inline]
    pub fn is_double_width(&self) -> bool {
        (self.bits & LineBits::DOUBLE_WIDTH_HEIGHT_MASK) == LineBits::DOUBLE_WIDTH
    }

    /// Force double-width.  This also implicitly sets
    /// double-height-(top/bottom) and dirty.
    #[inline]
    pub fn set_double_width(&mut self, seqno: SequenceNo) {
        self.bits
            .remove(LineBits::DOUBLE_HEIGHT_TOP | LineBits::DOUBLE_HEIGHT_BOTTOM);
        self.bits.insert(LineBits::DOUBLE_WIDTH);
        self.update_last_change_seqno(seqno);
    }

    /// Check whether the line is double-height-top.
    #[inline]
    pub fn is_double_height_top(&self) -> bool {
        (self.bits & LineBits::DOUBLE_WIDTH_HEIGHT_MASK)
            == LineBits::DOUBLE_WIDTH | LineBits::DOUBLE_HEIGHT_TOP
    }

    /// Force double-height top-half.  This also implicitly sets
    /// double-width and dirty.
    #[inline]
    pub fn set_double_height_top(&mut self, seqno: SequenceNo) {
        self.bits.remove(LineBits::DOUBLE_HEIGHT_BOTTOM);
        self.bits
            .insert(LineBits::DOUBLE_WIDTH | LineBits::DOUBLE_HEIGHT_TOP);
        self.update_last_change_seqno(seqno);
    }

    /// Check whether the line is double-height-bottom.
    #[inline]
    pub fn is_double_height_bottom(&self) -> bool {
        (self.bits & LineBits::DOUBLE_WIDTH_HEIGHT_MASK)
            == LineBits::DOUBLE_WIDTH | LineBits::DOUBLE_HEIGHT_BOTTOM
    }

    /// Force double-height bottom-half.  This also implicitly sets
    /// double-width and dirty.
    #[inline]
    pub fn set_double_height_bottom(&mut self, seqno: SequenceNo) {
        self.bits.remove(LineBits::DOUBLE_HEIGHT_TOP);
        self.bits
            .insert(LineBits::DOUBLE_WIDTH | LineBits::DOUBLE_HEIGHT_BOTTOM);
        self.update_last_change_seqno(seqno);
    }

    /// Set a flag the indicate whether the line should have the bidi
    /// algorithm applied during rendering
    pub fn set_bidi_enabled(&mut self, enabled: bool, seqno: SequenceNo) {
        self.bits.set(LineBits::BIDI_ENABLED, enabled);
        self.update_last_change_seqno(seqno);
    }

    /// Set the bidi direction for the line.
    /// This affects both the bidi algorithm (if enabled via set_bidi_enabled)
    /// and the layout direction of the line.
    /// `auto_detect` specifies whether the direction should be auto-detected
    /// before falling back to the specified direction.
    pub fn set_direction(&mut self, direction: Direction, auto_detect: bool, seqno: SequenceNo) {
        self.bits
            .set(LineBits::RTL, direction == Direction::RightToLeft);
        self.bits.set(LineBits::AUTO_DETECT_DIRECTION, auto_detect);
        self.update_last_change_seqno(seqno);
    }

    pub fn set_bidi_info(
        &mut self,
        enabled: bool,
        direction: ParagraphDirectionHint,
        seqno: SequenceNo,
    ) {
        self.bits.set(LineBits::BIDI_ENABLED, enabled);
        let (auto, rtl) = match direction {
            ParagraphDirectionHint::AutoRightToLeft => (true, true),
            ParagraphDirectionHint::AutoLeftToRight => (true, false),
            ParagraphDirectionHint::LeftToRight => (false, false),
            ParagraphDirectionHint::RightToLeft => (false, true),
        };
        self.bits.set(LineBits::AUTO_DETECT_DIRECTION, auto);
        self.bits.set(LineBits::RTL, rtl);
        self.update_last_change_seqno(seqno);
    }

    /// Returns a tuple of (BIDI_ENABLED, Direction), indicating whether
    /// the line should have the bidi algorithm applied and its base
    /// direction, respectively.
    pub fn bidi_info(&self) -> (bool, ParagraphDirectionHint) {
        (
            self.bits.contains(LineBits::BIDI_ENABLED),
            match (
                self.bits.contains(LineBits::AUTO_DETECT_DIRECTION),
                self.bits.contains(LineBits::RTL),
            ) {
                (true, true) => ParagraphDirectionHint::AutoRightToLeft,
                (false, true) => ParagraphDirectionHint::RightToLeft,
                (true, false) => ParagraphDirectionHint::AutoLeftToRight,
                (false, false) => ParagraphDirectionHint::LeftToRight,
            },
        )
    }

    fn invalidate_zones(&mut self) {
        self.zones.clear();
    }

    fn compute_zones(&mut self) {
        let blank_cell = Cell::blank();
        let mut last_cell: Option<CellRef> = None;
        let mut current_zone: Option<ZoneRange> = None;
        let mut zones = vec![];

        // Rows may have trailing space+Output cells interleaved
        // with other zones as a result of clear-to-eol and
        // clear-to-end-of-screen sequences.  We don't want
        // those to affect the zones that we compute here
        let mut last_non_blank = self.len();
        for cell in self.visible_cells() {
            if cell.str() != blank_cell.str() || cell.attrs() != blank_cell.attrs() {
                last_non_blank = cell.cell_index();
            }
        }

        for cell in self.visible_cells() {
            if cell.cell_index() > last_non_blank {
                break;
            }
            let grapheme_idx = cell.cell_index() as u16;
            let grapheme_end = grapheme_idx.saturating_add(1);
            let semantic_type = cell.attrs().semantic_type();
            let new_zone = match last_cell {
                None => true,
                Some(ref c) => c.attrs().semantic_type() != semantic_type,
            };

            if new_zone {
                if let Some(zone) = current_zone.take() {
                    zones.push(zone);
                }

                current_zone.replace(ZoneRange {
                    range: grapheme_idx..grapheme_end,
                    semantic_type,
                });
            }

            if let Some(zone) = current_zone.as_mut() {
                zone.range.end = grapheme_end;
            }

            last_cell.replace(cell);
        }

        if let Some(zone) = current_zone.take() {
            zones.push(zone);
        }
        self.zones = zones;
    }

    pub fn semantic_zone_ranges(&mut self) -> &[ZoneRange] {
        if self.zones.is_empty() {
            self.compute_zones();
        }
        &self.zones
    }

    /// If we have any cells with an implicit hyperlink, remove the hyperlink
    /// from the cell attributes but leave the remainder of the attributes alone.
    #[inline]
    pub fn invalidate_implicit_hyperlinks(&mut self, seqno: SequenceNo) {
        if (self.bits & (LineBits::SCANNED_IMPLICIT_HYPERLINKS | LineBits::HAS_IMPLICIT_HYPERLINKS))
            == LineBits::NONE
        {
            return;
        }

        self.bits &= !LineBits::SCANNED_IMPLICIT_HYPERLINKS;
        // Implicit-link state participates in `compute_shape_hash`, while
        // renderer appdata may cache that hash independently of the line
        // sequence.  Rule-epoch invalidation deliberately retains the current
        // sequence number, so leaving appdata attached here would let a stale
        // same-seqno shape hash survive removal (or later replacement) of an
        // implicit link.
        #[cfg(feature = "appdata")]
        self.clear_appdata();
        if (self.bits & LineBits::HAS_IMPLICIT_HYPERLINKS) == LineBits::NONE {
            return;
        }

        self.invalidate_implicit_hyperlinks_impl(seqno);
    }

    /// Whether this physical row has already participated in an implicit-link
    /// scan since its last content mutation or explicit invalidation.
    ///
    /// Callers that own complete logical-line assembly can use this as a cheap
    /// fast-path predicate before walking wrapped neighbors. A `true` value is
    /// meaningful only for the rule epoch selected by that caller; [`Line`]
    /// deliberately does not retain rule identity itself.
    #[inline]
    pub fn implicit_hyperlinks_are_scanned(&self) -> bool {
        self.bits.contains(LineBits::SCANNED_IMPLICIT_HYPERLINKS)
    }

    fn invalidate_implicit_hyperlinks_impl(&mut self, seqno: SequenceNo) {
        let cells = self.coerce_vec_storage();
        for cell in cells.iter_mut() {
            let replace = match cell.attrs().hyperlink() {
                Some(ref link) if link.is_implicit() => Some(Cell::new_grapheme(
                    cell.str(),
                    cell.attrs().clone().set_hyperlink(None).clone(),
                    None,
                )),
                _ => None,
            };
            if let Some(replace) = replace {
                *cell = replace;
            }
        }

        self.bits &= !LineBits::HAS_IMPLICIT_HYPERLINKS;
        self.update_last_change_seqno(seqno);
    }

    /// Scan through the line and look for sequences that match the provided
    /// rules.  Matching sequences are considered to be implicit hyperlinks
    /// and will have a hyperlink attribute associated with them.
    /// This function will only make changes if the line has been invalidated
    /// since the last time this function was called.
    /// This function does not remember the values of the `rules` slice, so it
    /// is the responsibility of the caller to call `invalidate_implicit_hyperlinks`
    /// if it wishes to call this function with different `rules`.
    pub fn scan_and_create_hyperlinks(&mut self, rules: &[Rule]) {
        if (self.bits & LineBits::SCANNED_IMPLICIT_HYPERLINKS)
            == LineBits::SCANNED_IMPLICIT_HYPERLINKS
        {
            // Has not changed since last time we scanned
            return;
        }

        let (scan_runs, has_explicit_hyperlink) = self.hyperlink_scan_runs();
        self.bits |= LineBits::SCANNED_IMPLICIT_HYPERLINKS;
        self.bits &= !LineBits::HAS_IMPLICIT_HYPERLINKS;
        self.bits
            .set(LineBits::HAS_HYPERLINK, has_explicit_hyperlink);

        let matches = hyperlink_cell_matches(scan_runs, rules);
        if matches.is_empty() {
            return;
        }

        let cells = self.coerce_vec_storage();
        if cells.scan_and_create_hyperlinks(matches) {
            self.bits |= LineBits::HAS_IMPLICIT_HYPERLINKS;
        }
    }

    fn hyperlink_scan_runs(&self) -> (Vec<HyperlinkScanRun>, bool) {
        let mut runs = Vec::new();
        let mut current = HyperlinkScanRun::default();
        let mut has_explicit_hyperlink = false;

        for cell in self.visible_cells() {
            let has_non_implicit_hyperlink =
                matches!(cell.attrs().hyperlink(), Some(link) if !link.is_implicit());
            has_explicit_hyperlink |= has_non_implicit_hyperlink;

            if has_non_implicit_hyperlink || cell.width() == 0 {
                current.finish_into(&mut runs);
                current = HyperlinkScanRun::default();
                continue;
            }

            current.push(cell);
        }

        current.finish_into(&mut runs);
        (runs, has_explicit_hyperlink)
    }

    /// Scan through a logical line that is comprised of an array of
    /// physical lines and look for sequences that match the provided
    /// rules.  Matching sequences are considered to be implicit hyperlinks
    /// and will have a hyperlink attribute associated with them.
    /// This function will only make changes if the line has been invalidated
    /// since the last time this function was called.
    /// This function does not remember the values of the `rules` slice, so it
    /// is the responsibility of the caller to call `invalidate_implicit_hyperlinks`
    /// if it wishes to call this function with different `rules`.
    ///
    /// This function will call Line::clear_appdata on lines where
    /// hyperlinks are adjusted.
    pub fn apply_hyperlink_rules(rules: &[Rule], logical_line: &mut [&mut Line]) {
        if rules.is_empty() || logical_line.is_empty() {
            return;
        }

        let mut need_scan = false;
        for line in logical_line.iter() {
            if !line.bits.contains(LineBits::SCANNED_IMPLICIT_HYPERLINKS) {
                need_scan = true;
                break;
            }
        }
        if !need_scan {
            return;
        }

        let mut logical = logical_line[0].clone();
        for line in &logical_line[1..] {
            let seqno = logical.current_seqno().max(line.current_seqno());
            logical.append_line((**line).clone(), seqno);
        }
        let seq = logical.current_seqno();

        logical.invalidate_implicit_hyperlinks(seq);
        logical.scan_and_create_hyperlinks(rules);

        if !logical.has_hyperlink() {
            for line in logical_line.iter_mut() {
                line.bits.set(LineBits::SCANNED_IMPLICIT_HYPERLINKS, true);
                #[cfg(feature = "appdata")]
                line.clear_appdata();
            }
            return;
        }

        // Re-compute the physical lines that comprise this logical line
        for phys in logical_line.iter_mut() {
            let wrapped = phys.last_cell_was_wrapped();
            let is_cluster = matches!(&phys.cells, CellStorage::C(_));
            let len = phys.len();
            let remainder = logical.split_off(len, seq);
            **phys = logical;
            logical = remainder;
            phys.set_last_cell_was_wrapped(wrapped, seq);
            #[cfg(feature = "appdata")]
            phys.clear_appdata();
            if is_cluster {
                phys.compress_for_scrollback();
            }
        }
    }

    /// Returns true if the line contains a hyperlink
    #[inline]
    pub fn has_hyperlink(&self) -> bool {
        (self.bits & (LineBits::HAS_HYPERLINK | LineBits::HAS_IMPLICIT_HYPERLINKS))
            != LineBits::NONE
    }

    /// Recompose line into the corresponding utf8 string.
    pub fn as_str(&self) -> Cow<'_, str> {
        match &self.cells {
            CellStorage::V(_) => {
                let mut s = String::new();
                for cell in self.visible_cells() {
                    s.push_str(cell.str());
                }
                Cow::Owned(s)
            }
            CellStorage::C(cl) => Cow::Borrowed(&cl.text),
        }
    }

    /// Count UTF-8 bytes in visible cells, excluding wide-cell padding.
    /// Clustered storage already owns the contiguous visible text, so this
    /// path neither allocates nor repeats Unicode grapheme segmentation.
    /// This is a text charge, not an estimate of total retained heap memory.
    /// Returns `None` if the sum of vector-cell text lengths overflows.
    pub fn visible_text_bytes(&self) -> Option<usize> {
        match &self.cells {
            CellStorage::C(line) => Some(line.text.len()),
            CellStorage::V(cells) => cells
                .visible_cells()
                .try_fold(0usize, |bytes, cell| bytes.checked_add(cell.str().len())),
        }
    }

    pub fn split_off(&mut self, idx: usize, seqno: SequenceNo) -> Self {
        let my_cells = self.coerce_vec_storage();
        // Clamp to avoid out of bounds panic if the line is shorter
        // than the requested split point
        // <https://github.com/wezterm/wezterm/issues/2355>
        let idx = idx.min(my_cells.len());
        let cells = my_cells.split_off(idx);
        Self {
            bits: self.bits,
            cells: CellStorage::V(VecStorage::new(cells)),
            seqno,
            zones: vec![],
            #[cfg(feature = "appdata")]
            appdata: Mutex::new(None),
        }
    }

    pub fn compute_double_click_range<F: Fn(&str) -> bool>(
        &self,
        click_col: usize,
        is_word: F,
    ) -> DoubleClickRange {
        let len = self.len();

        if click_col >= len {
            return DoubleClickRange::Range(click_col..click_col);
        }

        let cells = self.visible_cells().collect::<Vec<_>>();
        let click_col = match cells.iter().find(|cell| {
            let start = cell.cell_index();
            let end = start.saturating_add(cell.width().max(1));
            start <= click_col && click_col < end
        }) {
            Some(cell) if is_word(cell.str()) => cell.cell_index(),
            Some(_) | None => return DoubleClickRange::Range(click_col..click_col),
        };

        let mut lower = click_col;
        let mut upper = click_col;

        for cell in &cells {
            if cell.cell_index() < click_col {
                continue;
            }
            if !is_word(cell.str()) {
                break;
            }
            upper = cell.cell_index().saturating_add(cell.width().max(1));
        }
        for cell in cells.iter().rev() {
            if cell.cell_index() > click_col {
                continue;
            }
            if !is_word(cell.str()) {
                break;
            }
            lower = cell.cell_index();
        }

        if upper > lower
            && upper >= len
            && cells
                .last()
                .map(|cell| cell.attrs().wrapped())
                .unwrap_or(false)
        {
            DoubleClickRange::RangeWithWrap(lower..upper)
        } else {
            DoubleClickRange::Range(lower..upper)
        }
    }

    /// Returns a substring from the line.
    pub fn columns_as_str(&self, range: Range<usize>) -> String {
        let mut s = String::new();
        for c in self.visible_cells() {
            if c.cell_index() < range.start {
                continue;
            }
            if c.cell_index() >= range.end {
                break;
            }
            s.push_str(c.str());
        }
        s
    }

    pub fn columns_as_line(&self, range: Range<usize>) -> Self {
        let mut cells = vec![];
        for c in self.visible_cells() {
            if c.cell_index() < range.start {
                continue;
            }
            if c.cell_index() >= range.end {
                break;
            }
            cells.push(c.as_cell());
            // VecStorage uses physical columns, including the placeholders
            // hidden by wide graphemes. Omitting them hides the next glyph
            // when visible_cells walks the newly constructed line.
            for _ in 1..c.width() {
                cells.push(Cell::new(' ', c.attrs().clone()));
            }
        }
        Self {
            bits: LineBits::NONE,
            cells: CellStorage::V(VecStorage::new(cells)),
            seqno: self.current_seqno(),
            zones: vec![],
            #[cfg(feature = "appdata")]
            appdata: Mutex::new(None),
        }
    }

    /// If we're about to modify a cell obscured by a double-width
    /// character ahead of that cell, we need to nerf that sequence
    /// of cells to avoid partial rendering concerns.
    /// Similarly, when we assign a cell, we need to blank out those
    /// occluded successor cells.
    pub fn set_cell(&mut self, idx: usize, cell: Cell, seqno: SequenceNo) {
        self.set_cell_impl(idx, cell, false, seqno);
    }

    /// Assign a cell using grapheme text with a known width and attributes.
    /// This is a micro-optimization over first constructing a Cell from
    /// the grapheme info. If assigning this particular cell can be optimized
    /// to an append to the interal clustered storage then the cost of
    /// constructing and dropping the Cell can be avoided.
    pub fn set_cell_grapheme(
        &mut self,
        idx: usize,
        text: &str,
        width: usize,
        attr: CellAttributes,
        seqno: SequenceNo,
    ) {
        let width = normalize_cell_width(width);
        if checked_materialized_end(idx, width).is_none() {
            return;
        }

        if attr.hyperlink().is_some() {
            self.bits |= LineBits::HAS_HYPERLINK;
        }

        if let CellStorage::C(cl) = &mut self.cells {
            if idx > cl.len() && text == " " && attr == CellAttributes::blank() {
                // Appending blank beyond end of line; is already
                // implicitly blank
                return;
            }
            while cl.len() < idx {
                // Fill out any implied blanks until we can append
                // their intended cell content
                Arc::make_mut(cl).append_grapheme(" ", 1, CellAttributes::blank());
            }
            if idx == cl.len() {
                Arc::make_mut(cl).append_grapheme(text, width, attr);
                self.invalidate_implicit_hyperlinks(seqno);
                self.invalidate_zones();
                self.update_last_change_seqno(seqno);
                return;
            }
        }

        self.set_cell(idx, Cell::new_grapheme_with_width(text, width, attr), seqno);
    }

    /// Assign a contiguous printable width-1 ASCII run when it can be
    /// represented as a single append to clustered storage. Returns false for
    /// control bytes or whenever the caller must use normal per-cell assignment.
    pub fn append_ascii_cell_run(
        &mut self,
        idx: usize,
        text: &str,
        attr: CellAttributes,
        seqno: SequenceNo,
    ) -> bool {
        if text.is_empty() {
            return true;
        }
        if !text
            .as_bytes()
            .iter()
            .all(|byte| *byte == b' ' || byte.is_ascii_graphic())
            || checked_materialized_end(idx, text.len()).is_none()
        {
            return false;
        }

        let CellStorage::C(cl) = &mut self.cells else {
            return false;
        };
        if idx != cl.len() {
            return false;
        }

        if attr.hyperlink().is_some() {
            self.bits |= LineBits::HAS_HYPERLINK;
        }
        Arc::make_mut(cl).append_ascii_run(text, attr);
        self.invalidate_implicit_hyperlinks(seqno);
        self.invalidate_zones();
        self.update_last_change_seqno(seqno);
        true
    }

    pub fn set_cell_clearing_image_placements(
        &mut self,
        idx: usize,
        cell: Cell,
        seqno: SequenceNo,
    ) {
        self.set_cell_impl(idx, cell, true, seqno)
    }

    fn raw_set_cell(&mut self, idx: usize, cell: Cell, clear: bool) {
        let cells = self.coerce_vec_storage();
        cells.set_cell(idx, cell, clear);
    }

    fn set_cell_impl(&mut self, idx: usize, cell: Cell, clear: bool, seqno: SequenceNo) {
        // The .max(1) stuff is here in case we get called with a
        // zero-width cell.  That shouldn't happen: those sequences
        // should get filtered out in the terminal parsing layer,
        // but in case one does sneak through, we need to ensure that
        // we grow the cells array to hold this bogus entry.
        // https://github.com/wezterm/wezterm/issues/768
        let width = normalize_cell_width(cell.width());
        let Some(end_idx) = checked_materialized_end(idx, width) else {
            return;
        };

        self.invalidate_implicit_hyperlinks(seqno);
        self.invalidate_zones();
        self.update_last_change_seqno(seqno);
        if cell.attrs().hyperlink().is_some() {
            self.bits |= LineBits::HAS_HYPERLINK;
        }

        if let CellStorage::C(cl) = &mut self.cells {
            if idx > cl.len() && cell == Cell::blank() {
                // Appending blank beyond end of line; is already
                // implicitly blank
                return;
            }
            while cl.len() < idx {
                // Fill out any implied blanks until we can append
                // their intended cell content
                Arc::make_mut(cl).append_grapheme(" ", 1, CellAttributes::blank());
            }
            if idx == cl.len() {
                Arc::make_mut(cl).append(cell);
                return;
            }
            /*
            log::info!(
                "cannot append {cell:?} to {:?} as idx={idx} and cl.len is {}",
                cl,
                cl.len
            );
            */
        }

        // if the line isn't wide enough, pad it out with the default attributes.
        {
            let cells = self.coerce_vec_storage();
            if end_idx > cells.len() {
                cells.resize_with(end_idx, Cell::blank);
            }
        }

        self.invalidate_grapheme_at_or_before(idx);

        // For double-wide or wider chars, ensure that the cells that
        // are overlapped by this one are blanked out.
        for i in 1..=width.saturating_sub(1) {
            self.raw_set_cell(idx + i, Cell::blank_with_attrs(cell.attrs().clone()), clear);
        }

        self.raw_set_cell(idx, cell, clear);
    }

    /// Place text starting at the specified column index.
    /// Each grapheme of the text run has the same attributes.
    pub fn overlay_text_with_attribute(
        &mut self,
        mut start_idx: usize,
        text: &str,
        attr: CellAttributes,
        seqno: SequenceNo,
    ) {
        for (i, c) in Graphemes::new(text).enumerate() {
            let cell = Cell::new_grapheme(c, attr.clone(), None);
            let width = cell.width();
            self.set_cell(i + start_idx, cell, seqno);

            // Compensate for required spacing/placement of
            // double width characters
            start_idx += width.saturating_sub(1);
        }
    }

    fn invalidate_grapheme_at_or_before(&mut self, idx: usize) {
        // Assumption: that the width of a grapheme is never > 2.
        // This constrains the amount of look-back that we need to do here.
        if idx > 0 {
            let prior = idx - 1;
            let cells = self.coerce_vec_storage();
            if prior >= cells.len() {
                // Callers may materialize the target range only after this
                // look-back.  A write beyond the current logical end has no
                // preceding stored grapheme to invalidate.
                return;
            }
            let width = cells[prior].width();
            if width > 1 {
                let attrs = cells[prior].attrs().clone();
                // A resize can truncate the placeholder column(s) owned by a
                // wide grapheme while retaining its head.  Invalidate only
                // the materialized portion rather than indexing past storage.
                let end = prior.saturating_add(width).min(cells.len());
                cells[prior..end].fill(Cell::blank_with_attrs(attrs));
            }
        }
    }

    pub fn insert_cell(&mut self, x: usize, cell: Cell, right_margin: usize, seqno: SequenceNo) {
        if right_margin == 0
            || checked_materialized_end(x, normalize_cell_width(cell.width())).is_none()
        {
            return;
        }

        self.invalidate_implicit_hyperlinks(seqno);

        let cells = self.coerce_vec_storage();
        if right_margin <= cells.len() {
            cells.remove(right_margin - 1);
        }

        if x >= cells.len() {
            cells.resize_with(x, Cell::blank);
        }

        // If we're inserting a wide cell, we should also insert the overlapped cells.
        // We insert them first so that the grapheme winds up left-most.
        let width = cell.width();
        for _ in 1..=width.saturating_sub(1) {
            cells.insert(x, Cell::blank_with_attrs(cell.attrs().clone()));
        }

        cells.insert(x, cell);
        self.update_last_change_seqno(seqno);
        self.invalidate_zones();
    }

    pub fn erase_cell(&mut self, x: usize, seqno: SequenceNo) {
        if x >= self.len() {
            // Already implicitly erased
            return;
        }
        self.invalidate_implicit_hyperlinks(seqno);
        self.invalidate_grapheme_at_or_before(x);
        {
            let cells = self.coerce_vec_storage();
            cells.remove(x);
            cells.push(Cell::default());
        }
        self.update_last_change_seqno(seqno);
        self.invalidate_zones();
    }

    pub fn remove_cell(&mut self, x: usize, seqno: SequenceNo) {
        if x >= self.len() {
            // Already implicitly removed
            return;
        }
        self.invalidate_implicit_hyperlinks(seqno);
        self.invalidate_grapheme_at_or_before(x);
        self.coerce_vec_storage().remove(x);
        self.update_last_change_seqno(seqno);
        self.invalidate_zones();
    }

    pub fn erase_cell_with_margin(
        &mut self,
        x: usize,
        right_margin: usize,
        seqno: SequenceNo,
        blank_attr: CellAttributes,
    ) {
        if right_margin == 0 {
            return;
        }

        self.invalidate_implicit_hyperlinks(seqno);
        if x < self.len() {
            self.invalidate_grapheme_at_or_before(x);
            self.coerce_vec_storage().remove(x);
        }
        // We just removed one cell above, so the old inclusive right margin maps
        // to an insert index that may be exactly one past the current end.
        let insert_idx = right_margin - 1;
        if insert_idx <= self.len() {
            self.coerce_vec_storage()
                .insert(insert_idx, Cell::blank_with_attrs(blank_attr));
        }
        self.update_last_change_seqno(seqno);
        self.invalidate_zones();
    }

    pub fn prune_trailing_blanks(&mut self, seqno: SequenceNo) {
        if let CellStorage::C(cl) = &mut self.cells {
            if !cl.text.ends_with(' ') {
                return;
            }
            if Arc::make_mut(cl).prune_trailing_blanks() {
                self.update_last_change_seqno(seqno);
                self.invalidate_zones();
            }
            return;
        }

        let def_attr = CellAttributes::blank();
        let cells = self.coerce_vec_storage();
        let new_len = cells
            .iter()
            .rposition(|c| c.str() != " " || c.attrs() != &def_attr)
            .map_or(0, |end_idx| {
                // A visible wide grapheme owns its following placeholder
                // column(s).  Those placeholders are ordinary default blank
                // cells, but pruning them would make VecStorage::len disagree
                // with clustered storage and under-report the visual width.
                end_idx
                    .saturating_add(normalize_cell_width(cells[end_idx].width()))
                    .min(cells.len())
            });
        if new_len < cells.len() {
            cells.resize_with(new_len, Cell::blank);
            self.update_last_change_seqno(seqno);
            self.invalidate_zones();
        }
    }

    pub fn fill_range(&mut self, cols: Range<usize>, cell: &Cell, seqno: SequenceNo) {
        if cols.start >= cols.end {
            return;
        }

        // A cell wider than one column cannot be duplicated with a slice fill:
        // each successive assignment must first invalidate the wide cell that
        // the preceding assignment placed.  Retain the established per-column
        // semantics for that unusual public-API case; terminal erase/fill hot
        // paths use width-one cells and continue through the batched path below.
        let cell_width = normalize_cell_width(cell.width());
        if cell_width > 1 {
            // `set_cell_impl` rejects starts whose materialized cell would
            // exceed the u32-backed line limit.  Do not nevertheless walk a
            // pointer-width tail of individually rejected indices.
            let valid_start_end = MAX_MATERIALIZED_LINE_LEN
                .saturating_sub(cell_width)
                .saturating_add(1);
            let bounded_end = cols.end.min(valid_start_end);
            if cols.start >= bounded_end {
                return;
            }
            for x in cols.start..bounded_end {
                self.set_cell_impl(x, cell.clone(), true, seqno);
            }
            self.prune_trailing_blanks(seqno);
            return;
        }

        let is_default_blank = *cell == Cell::blank();
        let current_len = self.len();
        if is_default_blank && (current_len == 0 || cols.start >= current_len) {
            // `resize` can truncate the placeholder column owned by a wide
            // grapheme while retaining its head as the final stored cell.
            // Filling that implicit placeholder with a default blank is not a
            // no-op: it erases the wide grapheme that overlaps the target.
            if cols.start == current_len
                && current_len > 0
                && self
                    .get_cell(current_len - 1)
                    .is_some_and(|cell| normalize_cell_width(cell.width()) > 1)
            {
                self.invalidate_implicit_hyperlinks(seqno);
                self.invalidate_grapheme_at_or_before(cols.start);
                self.update_last_change_seqno(seqno);
                self.invalidate_zones();
                self.prune_trailing_blanks(seqno);
            }
            // We would be filling it with blanks only to prune
            // them all away again before we return; NOP
            return;
        }

        let start = cols.start;
        let end = if is_default_blank {
            cols.end.min(current_len)
        } else {
            cols.end
        };
        let Some(width) = end.checked_sub(start) else {
            return;
        };
        if width == 0 || checked_materialized_end(start, width).is_none() {
            return;
        }

        self.invalidate_implicit_hyperlinks(seqno);
        self.invalidate_zones();
        self.update_last_change_seqno(seqno);
        if cell.attrs().hyperlink().is_some() {
            self.bits |= LineBits::HAS_HYPERLINK;
        }

        self.invalidate_grapheme_at_or_before(start);
        {
            let cells = self.coerce_vec_storage();
            if end > cells.len() {
                cells.resize_with(end, Cell::blank);
            }
            cells[start..end].fill(cell.clone());
        }
        self.prune_trailing_blanks(seqno);
    }

    pub fn len(&self) -> usize {
        match &self.cells {
            CellStorage::V(cells) => cells.len(),
            CellStorage::C(cl) => cl.len(),
        }
    }

    /// Iterates the visible cells, respecting the width of the cell.
    /// For instance, a double-width cell overlaps the following (blank)
    /// cell, so that blank cell is omitted from the iterator results.
    /// The iterator yields (column_index, Cell).  Column index is the
    /// index into Self::cells, and due to the possibility of skipping
    /// the characters that follow wide characters, the column index may
    /// skip some positions.  It is returned as a convenience to the consumer
    /// as using .enumerate() on this iterator wouldn't be as useful.
    pub fn visible_cells<'a>(&'a self) -> impl Iterator<Item = CellRef<'a>> {
        match &self.cells {
            CellStorage::V(cells) => VisibleCellIter::V(cells.visible_cells()),
            CellStorage::C(cl) => VisibleCellIter::C(cl.iter()),
        }
    }

    pub fn get_cell(&self, cell_index: usize) -> Option<CellRef<'_>> {
        self.visible_cells()
            .find(|cell| cell.cell_index() == cell_index)
    }

    pub fn cluster(&self, bidi_hint: Option<ParagraphDirectionHint>) -> Vec<CellCluster> {
        CellCluster::make_cluster(self.len(), self.visible_cells(), bidi_hint)
    }

    fn make_cells(&mut self) {
        let cells = match &self.cells {
            CellStorage::V(_) => return,
            CellStorage::C(cl) => cl.to_cell_vec(),
        };
        // log::info!("make_cells\n{:?}", backtrace::Backtrace::new());
        self.cells = CellStorage::V(VecStorage::new(cells));
    }

    pub(crate) fn coerce_vec_storage(&mut self) -> &mut VecStorage {
        self.make_cells();

        match &mut self.cells {
            CellStorage::V(c) => return c,
            CellStorage::C(_) => unreachable!(),
        }
    }

    /// Adjusts the internal storage so that it occupies less
    /// space. Subsequent mutations will incur some overhead to
    /// re-materialize the storage in a form that is suitable
    /// for mutation.
    pub fn compress_for_scrollback(&mut self) {
        let cv = match &self.cells {
            CellStorage::V(v) => ClusteredLine::from_cell_vec(v.len(), self.visible_cells()),
            CellStorage::C(_) => return,
        };
        self.cells = CellStorage::C(Arc::new(cv));
    }

    pub fn cells_mut(&mut self) -> &mut [Cell] {
        self.coerce_vec_storage().as_mut_slice()
    }

    /// Return true if the line consists solely of whitespace cells
    pub fn is_whitespace(&self) -> bool {
        self.visible_cells().all(|c| c.str() == " ")
    }

    /// Return true if the last cell in the line has the wrapped attribute,
    /// indicating that the following line is logically a part of this one.
    pub fn last_cell_was_wrapped(&self) -> bool {
        match &self.cells {
            CellStorage::C(line) => line.last_cell_was_wrapped(),
            CellStorage::V(cells) => cells.cached_wrap_boundary(|| {
                // Wide cells can hide spacer cells whose attributes differ.
                // Cache the existing visible-cell oracle, not the raw tail.
                self.visible_cells()
                    .last()
                    .map(|c| c.attrs().wrapped())
                    .unwrap_or(false)
            }),
        }
    }

    /// Adjust the value of the wrapped attribute on the last cell of this
    /// line.
    pub fn set_last_cell_was_wrapped(&mut self, wrapped: bool, seqno: SequenceNo) {
        self.update_last_change_seqno(seqno);
        if let CellStorage::C(cl) = &mut self.cells {
            if cl.len() == 0 {
                if !wrapped {
                    // Clearing an absent boundary must not materialize a
                    // phantom space in an otherwise empty logical line.
                    return;
                }
                // Need to mark that implicit space as wrapped, so
                // explicitly add it
                Arc::make_mut(cl).append(Cell::blank());
            }
            Arc::make_mut(cl).set_last_cell_was_wrapped(wrapped);
            return;
        }

        // A double-width final grapheme has a trailing spacer in vector
        // storage. The reader and scrollback compressor visit the grapheme,
        // not that spacer, so writing only the last physical cell loses the
        // soft-wrap boundary on the next resize. Keep both cells consistent.
        if let Some(last_visible) = self.visible_cells().last().map(|cell| cell.cell_index()) {
            let cells = self.coerce_vec_storage();
            for cell in &mut cells[last_visible..] {
                cell.attrs_mut().set_wrapped(wrapped);
            }
        }
    }

    /// Concatenate the cells from other with this line, appending them
    /// to this line.
    /// This function is used by rewrapping logic when joining wrapped
    /// lines back together.
    pub fn append_line(&mut self, other: Line, seqno: SequenceNo) {
        match &mut self.cells {
            CellStorage::V(cells) => {
                for cell in other.visible_cells() {
                    cells.push(cell.as_cell());
                    for _ in 1..cell.width() {
                        cells.push(Cell::new(' ', cell.attrs().clone()));
                    }
                }
            }
            CellStorage::C(cl) => {
                let cl = Arc::make_mut(cl);
                for cell in other.visible_cells() {
                    cl.append(cell.as_cell());
                }
            }
        }
        self.update_last_change_seqno(seqno);
        self.invalidate_zones();
    }

    /// Join unchanged row views from the same logical source. The caller must
    /// supply exactly one logical line (ending at a hard break or screen end).
    /// Refusal leaves every source row untouched for ordinary reconstruction.
    pub fn try_join_deferred_logical_rows<'a>(
        rows: impl Iterator<Item = &'a Line> + Clone,
        seqno: SequenceNo,
    ) -> Option<Line> {
        #[cfg(feature = "std")]
        {
            static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if *DISABLED.get_or_init(|| {
                std::env::var_os("FT_DISABLE_DEFERRED_LOGICAL_JOIN")
                    .is_some_and(|value| value == "1")
            }) {
                return None;
            }
            let first = rows.clone().next()?;
            let count = rows.clone().count();
            if rows
                .clone()
                .take(count.saturating_sub(1))
                .any(|line| !line.last_cell_was_wrapped())
            {
                return None;
            }
            let cells = VecStorage::try_join_token_rows(rows.map(|line| match &line.cells {
                CellStorage::V(cells) => Some(cells),
                CellStorage::C(_) => None,
            }))?;
            let mut logical = first.clone();
            logical.cells = CellStorage::V(cells);
            logical.update_last_change_seqno(seqno);
            if count > 1 {
                logical.invalidate_zones();
            }
            Some(logical)
        }
        #[cfg(not(feature = "std"))]
        {
            let _ = (rows, seqno);
            None
        }
    }

    /// Build a first-use logical source directly from physical graphemes.
    /// Avoid the intermediate filler-expanded concatenation that would be
    /// discarded as soon as the wrap planner extracts the same tokens again.
    pub fn try_compact_logical_rows<'a>(
        rows: impl Iterator<Item = &'a Line> + Clone,
        seqno: SequenceNo,
    ) -> Option<Line> {
        #[cfg(feature = "std")]
        {
            static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if *DISABLED.get_or_init(|| {
                std::env::var_os("FT_DISABLE_COMPACT_LOGICAL_ROWS")
                    .is_some_and(|value| value == "1")
            }) {
                return None;
            }
            let first = rows.clone().next()?;
            let count = rows.clone().count();
            let capacity = rows
                .clone()
                .try_fold(0usize, |total, row| total.checked_add(row.len()))?;
            for (index, row) in rows.clone().enumerate() {
                if index + 1 < count && !row.last_cell_was_wrapped() {
                    return None;
                }
                #[cfg(feature = "use_image")]
                if row.has_image_attachments() {
                    return None;
                }
            }
            let mut tokens = Vec::with_capacity(capacity);
            for row in rows {
                let start = tokens.len();
                tokens.extend(row.visible_cells().map(|cell| cell.as_cell()));
                if row.last_cell_was_wrapped() {
                    if let Some(last) = tokens[start..].last_mut() {
                        last.attrs_mut().set_wrapped(false);
                    }
                }
            }
            let end = tokens.len();
            let mut logical = first.clone();
            logical.cells =
                CellStorage::V(VecStorage::from_token_range(tokens.into(), 0..end, false));
            logical.update_last_change_seqno(seqno);
            if count > 1 {
                logical.invalidate_zones();
            }
            Some(logical)
        }
        #[cfg(not(feature = "std"))]
        {
            let _ = (rows, seqno);
            None
        }
    }

    /// mutable access the cell data, but the caller must take care
    /// to only mutate attributes rather than the cell textual content.
    /// Use set_cell if you need to modify the textual content of the
    /// cell, so that important invariants are upheld.
    pub fn cells_mut_for_attr_changes_only(&mut self) -> &mut [Cell] {
        self.coerce_vec_storage().as_mut_slice()
    }

    /// Given a starting attribute value, produce a series of Change
    /// entries to recreate the current line
    pub fn changes(&self, start_attr: &CellAttributes) -> Vec<Change> {
        let mut result = Vec::new();
        let mut attr = start_attr.clone();
        let mut text_run = String::new();

        for cell in self.visible_cells() {
            if *cell.attrs() == attr {
                text_run.push_str(cell.str());
            } else {
                // flush out the current text run
                if !text_run.is_empty() {
                    result.push(Change::Text(text_run.clone()));
                    text_run.clear();
                }

                attr = cell.attrs().clone();
                result.push(Change::AllAttributes(attr.clone()));
                text_run.push_str(cell.str());
            }
        }

        // flush out any remaining text run
        if !text_run.is_empty() {
            // if this is just spaces then it is likely cheaper
            // to emit ClearToEndOfLine instead.
            if attr
                == CellAttributes::default()
                    .set_background(attr.background())
                    .clone()
            {
                let left = text_run.trim_end_matches(' ').to_string();
                let num_trailing_spaces = text_run.len() - left.len();

                if num_trailing_spaces > 0 {
                    if !left.is_empty() {
                        result.push(Change::Text(left));
                    } else if result.len() == 1 {
                        // if the only queued result prior to clearing
                        // to the end of the line is an attribute change,
                        // we can prune it out and return just the line
                        // clearing operation
                        if let Change::AllAttributes(_) = result[0] {
                            result.clear()
                        }
                    }

                    // Since this function is only called in the full repaint
                    // case, and we always emit a clear screen with the default
                    // background color, we don't need to emit an instruction
                    // to clear the remainder of the line unless it has a different
                    // background color.
                    if attr.background() != Default::default() {
                        result.push(Change::ClearToEndOfLine(attr.background()));
                    }
                } else {
                    result.push(Change::Text(text_run));
                }
            } else {
                result.push(Change::Text(text_run));
            }
        }

        result
    }
}

/// Sentinel badness for overflow/invalid-width lines.
pub const KP_BADNESS_INF: u64 = u64::MAX / 4;

/// Terminal defaults for bounded monospace Knuth-Plass scoring.
pub const KP_DEFAULT_LOOKAHEAD_LIMIT: usize = 64;
pub const KP_DEFAULT_MAX_DP_STATES: usize = 8_192;

/// Scoring and complexity contract for bounded Knuth-Plass line breaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonospaceKpCostModel {
    /// Cubic slack multiplier.
    pub badness_scale: u64,
    /// Added when the engine must force-break overflow content.
    pub forced_break_penalty: u64,
    /// Sliding lookahead cap used to bound DP transitions.
    pub lookahead_limit: usize,
    /// Maximum DP states evaluated before deterministic fallback.
    pub max_dp_states: usize,
}

impl Default for MonospaceKpCostModel {
    fn default() -> Self {
        Self::terminal_default()
    }
}

impl MonospaceKpCostModel {
    /// Canonical terminal-safe defaults for resize-time wrapping.
    pub const fn terminal_default() -> Self {
        Self {
            badness_scale: 10_000,
            forced_break_penalty: 5_000,
            lookahead_limit: KP_DEFAULT_LOOKAHEAD_LIMIT,
            max_dp_states: KP_DEFAULT_MAX_DP_STATES,
        }
    }

    /// Cubic slack badness used by the bounded DP scorer.
    ///
    /// - overflow => `KP_BADNESS_INF`
    /// - last line => `0` (TeX convention)
    /// - non-last lines => `(slack/target_width)^3 * badness_scale`
    #[inline]
    pub fn line_badness(self, slack: i64, target_width: usize, is_last_line: bool) -> u64 {
        if slack < 0 {
            return KP_BADNESS_INF;
        }
        if is_last_line {
            return 0;
        }
        if target_width == 0 {
            return KP_BADNESS_INF;
        }

        let slack_u64 = slack as u64;
        let width_u64 = target_width as u64;
        let slack_cubed = slack_u64
            .saturating_mul(slack_u64)
            .saturating_mul(slack_u64);
        let width_cubed = width_u64
            .saturating_mul(width_u64)
            .saturating_mul(width_u64);
        if width_cubed == 0 {
            return KP_BADNESS_INF;
        }
        slack_cubed.saturating_mul(self.badness_scale) / width_cubed
    }

    /// Upper bound on DP transition count under this model.
    pub const fn estimated_dp_states(self, token_count: usize) -> usize {
        if token_count == 0 {
            return 0;
        }
        let lookahead = if token_count < self.lookahead_limit {
            token_count
        } else {
            self.lookahead_limit
        };
        // The last `lookahead` starts have only lookahead, lookahead-1,
        // ..., 1 successors. Counting a full lookahead for each rejects
        // bounded plans unnecessarily. Divide before multiplying so the
        // triangular term saturates only when its actual value overflows.
        let tail = if lookahead % 2 == 0 {
            (lookahead / 2).saturating_mul(lookahead.saturating_add(1))
        } else {
            lookahead.saturating_mul(lookahead / 2 + 1)
        };
        (token_count - lookahead)
            .saturating_mul(lookahead)
            .saturating_add(tail)
    }

    /// Whether the DP engine should fall back to deterministic greedy wrapping.
    pub const fn should_fallback(self, token_count: usize) -> bool {
        self.estimated_dp_states(token_count) > self.max_dp_states
    }
}

/// Comparable summary for DP candidate ranking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MonospaceBreakCandidate {
    pub total_cost: u64,
    pub forced_breaks: usize,
    pub max_line_badness: u64,
    pub line_count: usize,
    pub break_offsets: Vec<usize>,
}

/// Deterministic candidate ordering for equal/near-equal DP paths.
///
/// Lower sort order is better:
/// 1. total_cost
/// 2. forced_breaks
/// 3. max_line_badness
/// 4. line_count
/// 5. lexical `break_offsets` (final stable tie-break)
#[allow(dead_code)] // Bound to wa-1u90p.3.12 integration follow-up.
#[inline]
pub(crate) fn compare_monospace_break_candidates(
    lhs: &MonospaceBreakCandidate,
    rhs: &MonospaceBreakCandidate,
) -> Ordering {
    lhs.total_cost
        .cmp(&rhs.total_cost)
        .then(lhs.forced_breaks.cmp(&rhs.forced_breaks))
        .then(lhs.max_line_badness.cmp(&rhs.max_line_badness))
        .then(lhs.line_count.cmp(&rhs.line_count))
        .then(lhs.break_offsets.cmp(&rhs.break_offsets))
}

#[allow(dead_code)] // Bound to wa-1u90p.3.12 integration follow-up.
#[inline]
pub(crate) fn choose_best_monospace_break_candidate(
    candidates: &[MonospaceBreakCandidate],
) -> Option<&MonospaceBreakCandidate> {
    candidates
        .iter()
        .min_by(|lhs, rhs| compare_monospace_break_candidates(lhs, rhs))
}

/// Execution mode used by the bounded wrap planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonospaceWrapMode {
    /// DP planner remained within configured state budget.
    Dp,
    /// Planner exceeded budget or had no viable DP plan and used greedy fallback.
    Fallback,
}

/// Wrap plan emitted by the bounded planner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonospaceWrapPlan {
    pub mode: MonospaceWrapMode,
    pub break_offsets: Vec<usize>,
    pub estimated_states: usize,
    pub evaluated_states: usize,
}

/// Per-line wrap quality scorecard suitable for resize regression gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineWrapScorecard {
    pub mode: MonospaceWrapMode,
    pub greedy_total_cost: u64,
    pub selected_total_cost: u64,
    pub badness_delta: i64,
    pub greedy_forced_breaks: usize,
    pub selected_forced_breaks: usize,
    pub line_count: usize,
    pub estimated_states: usize,
    pub evaluated_states: usize,
}

/// Full wrap result payload used by integration code in reflow paths.
#[derive(Debug, Clone, PartialEq)]
pub struct LineWrapReport {
    pub lines: Vec<Line>,
    pub scorecard: LineWrapScorecard,
}

/// Width and ordinary-space snapshot of a physical row or certified logical join.
/// No text, attributes, cells, images or output rows are retained. Capture is
/// fallible under the caller's byte budget; a refused join must use the ordinary
/// text-bearing path. In particular, clustered append can change grapheme
/// boundaries and normalize widths, so concatenating arbitrary width arrays is
/// not equivalent to `Line::append_line`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineWrapGeometry {
    widths: Vec<u8>,
    physical_len: usize,
    trimmed_tokens: usize,
    join_safe: bool,
    head_basic: bool,
    tail_basic: bool,
    head_after_basic: bool,
    tail_before_basic: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineWrapGeometryJoinError {
    UncertifiedSource,
    UncertifiedBoundary,
    LengthOverflow,
    ByteBudget,
    Allocation,
}

impl LineWrapGeometry {
    /// Capture exact visible widths and the last non-space token. `max_bytes`
    /// bounds retained metadata and temporary boundary-certificate text work.
    /// Standalone counts remain exact even when `supports_join()` is false.
    pub fn capture(line: &Line, max_bytes: usize) -> Option<Self> {
        let available = max_bytes.checked_sub(core::mem::size_of::<Self>())?;
        // Visible tokens cannot outnumber columns in canonical physical rows.
        // Reserve once, before certificate scratch exists, so a growing width
        // allocation cannot double the admitted working-memory footprint.
        if line.len() > available {
            return None;
        }
        let mut result = Self {
            widths: Vec::new(),
            physical_len: line.len(),
            trimmed_tokens: 0,
            join_safe: line.len() <= u32::MAX as usize,
            head_basic: false,
            tail_basic: false,
            head_after_basic: false,
            tail_before_basic: false,
        };
        result.widths.try_reserve_exact(line.len()).ok()?;
        if result.widths.capacity() > available {
            return None;
        }
        let boundary_budget = max_bytes.checked_sub(result.retained_bytes())?;
        let mut previous: Option<&str> = None;
        let mut previous_basic = false;
        let mut columns = 0usize;
        let mut text_work = 0usize;
        let mut boundary = zeroize::Zeroizing::new(String::new());
        for cell in line.visible_cells() {
            let width = cell.width();
            // Current cells use widths 0..=2. Refuse a future extended width
            // rather than silently truncating it in this compact encoding.
            if width > 2 || result.widths.len() == result.widths.capacity() {
                return None;
            }
            result
                .widths
                .push(width as u8 | if cell.str() == " " { 0x80 } else { 0 });
            // Preserve the source borrow beyond this temporary CellRef so the
            // next token can certify its boundary against the previous one.
            let text = match cell {
                CellRef::CellRef { cell, .. } => cell.str(),
                CellRef::ClusterRef { text, .. } => text,
            };
            if text != " " {
                result.trimmed_tokens = result.widths.len();
            }
            result.join_safe &= cell.cell_index() == columns && (1..=2).contains(&width);
            columns = columns.checked_add(width.max(1))?;
            text_work = text_work.checked_add(text.len())?;
            if text_work > max_bytes {
                return None;
            }
            // Durable scrollback normalizes storage through cells and its
            // text serializer. A width override must not authorize geometry
            // for a later default-width decoded representation.
            let ascii = text.len() == 1 && matches!(text.as_bytes()[0], b' '..=b'~');
            let mut chars = text.chars();
            let basic = chars.next().is_some_and(geometry_basic_starter) && chars.next().is_none();
            result.join_safe &= if ascii {
                width == 1
            } else {
                frankenterm_cell::grapheme_column_width(text, None) == width
            };
            if !basic {
                let mut graphemes = Graphemes::new(text);
                result.join_safe &= graphemes.next() == Some(text) && graphemes.next().is_none();
            }
            if let Some(previous) = previous {
                if !(previous_basic && basic) {
                    result.join_safe &= geometry_boundary_is_separate(
                        previous,
                        text,
                        &mut boundary,
                        boundary_budget,
                    )?;
                }
            } else {
                result.head_basic = text.chars().next().is_some_and(geometry_basic_starter);
                result.head_after_basic = basic
                    || geometry_boundary_is_separate("a", text, &mut boundary, boundary_budget)?;
            }
            previous = Some(text);
            previous_basic = basic;
        }
        result.join_safe &= columns == line.len();
        if let Some(last) = previous {
            result.tail_basic = previous_basic;
            result.tail_before_basic = previous_basic
                || geometry_boundary_is_separate(last, "a", &mut boundary, boundary_budget)?;
        }
        Some(result)
    }

    /// True when this row's widths, segmentation and physical length survive
    /// both vector and clustered storage and default-width text round trips.
    /// Appending still requires a certified boundary between the two rows.
    pub fn supports_join(&self) -> bool {
        self.join_safe
    }

    pub fn physical_len(&self) -> usize {
        self.physical_len
    }

    /// Treat every captured cell as meaningful in an incomplete logical prefix.
    /// Apply after joining its rows, before budgeting or counting its wrapping.
    pub fn preserve_trailing_spaces(&mut self) {
        self.trimmed_tokens = self.widths.len();
    }

    /// Preserve separators on text-bearing terminal lines while retaining the
    /// single-row representation of an entirely blank screen row.
    pub fn preserve_terminal_trailing_spaces(&mut self) {
        if self.trimmed_tokens != 0 {
            self.preserve_trailing_spaces();
        }
    }

    /// Owned allocation plus the inline representation; an external Arc header
    /// and collection slots must be charged separately by their owner.
    pub fn retained_bytes(&self) -> usize {
        core::mem::size_of::<Self>().saturating_add(self.widths.capacity())
    }

    /// Conservative upper bound for live planner allocations, including the
    /// caller's retained scratch. DP can retain a break-offset vector at every
    /// endpoint; a bound on width bytes alone is not a bound on planning memory.
    pub fn planning_bytes_upper_bound(
        &self,
        cols: usize,
        cost_model: MonospaceKpCostModel,
        scratch: &LineWrapWidthPrefixScratch,
    ) -> Option<usize> {
        let n = self.trimmed_tokens;
        if self.physical_len <= cols || n == 0 {
            return scratch.capacity().checked_mul(core::mem::size_of::<u128>());
        }
        let prefix = scratch
            .capacity()
            .checked_mul(2)?
            .max(n.checked_add(1)?.checked_mul(3)?)
            .max(4)
            .checked_add(scratch.capacity())?;
        // Vec push growth rounds up geometrically. Twice n (at least eight)
        // bounds each offset vector, including a cloned candidate's next push.
        let offsets = n.checked_add(1)?.checked_mul(2)?.max(8);
        let dp = cols != 0 && !cost_model.should_fallback(n);
        let offset_vectors = if dp { n.checked_add(8)? } else { 8 };
        let mut bytes = prefix
            .checked_mul(core::mem::size_of::<u128>())?
            .checked_add(
                offset_vectors
                    .checked_mul(offsets)?
                    .checked_mul(core::mem::size_of::<usize>())?,
            )?;
        if dp {
            bytes = bytes.checked_add(
                n.checked_add(1)?
                    .checked_mul(core::mem::size_of::<Option<MonospaceBreakCandidate>>())?,
            )?;
        }
        Some(bytes)
    }

    /// Budgeted form for cold-read workers. Refusal leaves scratch untouched
    /// and lets the caller preserve its existing bounded fallback contract.
    pub fn row_count_with_budget(
        &self,
        cols: usize,
        cost_model: MonospaceKpCostModel,
        scratch: &mut LineWrapWidthPrefixScratch,
        max_bytes: usize,
    ) -> Option<usize> {
        if self.planning_bytes_upper_bound(cols, cost_model, scratch)? > max_bytes {
            return None;
        }
        Some(self.row_count(cols, cost_model, scratch))
    }

    /// Join without retaining source text. A false return leaves the logical
    /// geometry unchanged (a successful reserve may increase its capacity).
    pub fn append(&mut self, other: &Self, max_bytes: usize) -> bool {
        self.try_append(other, max_bytes).is_ok()
    }

    /// Fallible join with a finite refusal reason for fallback diagnostics.
    pub fn try_append(
        &mut self,
        other: &Self,
        max_bytes: usize,
    ) -> Result<(), LineWrapGeometryJoinError> {
        if !self.join_safe || !other.join_safe {
            return Err(LineWrapGeometryJoinError::UncertifiedSource);
        }
        if !self.widths.is_empty()
            && !other.widths.is_empty()
            && !(self.tail_basic && other.head_after_basic
                || other.head_basic && self.tail_before_basic)
        {
            return Err(LineWrapGeometryJoinError::UncertifiedBoundary);
        }
        let Some(physical_len) = self.physical_len.checked_add(other.physical_len) else {
            return Err(LineWrapGeometryJoinError::LengthOverflow);
        };
        // ClusteredLine's physical length saturates at u32::MAX. Do not
        // certify a join whose vector and clustered lengths could disagree.
        if physical_len > u32::MAX as usize {
            return Err(LineWrapGeometryJoinError::LengthOverflow);
        }
        let Some(len) = self.widths.len().checked_add(other.widths.len()) else {
            return Err(LineWrapGeometryJoinError::LengthOverflow);
        };
        let Some(available) = max_bytes.checked_sub(core::mem::size_of::<Self>()) else {
            return Err(LineWrapGeometryJoinError::ByteBudget);
        };
        if len > available || self.widths.capacity() > available {
            return Err(LineWrapGeometryJoinError::ByteBudget);
        }
        if len > self.widths.capacity()
            && self
                .widths
                .capacity()
                .checked_add(len)
                .is_none_or(|peak| peak > available)
        {
            return Err(LineWrapGeometryJoinError::ByteBudget);
        }
        if self.widths.try_reserve_exact(other.widths.len()).is_err() {
            return Err(LineWrapGeometryJoinError::Allocation);
        }
        if self.widths.capacity() > available {
            return Err(LineWrapGeometryJoinError::ByteBudget);
        }
        if other.trimmed_tokens > 0 {
            self.trimmed_tokens = self.widths.len() + other.trimmed_tokens;
        }
        if self.widths.is_empty() {
            self.head_basic = other.head_basic;
            self.head_after_basic = other.head_after_basic;
        }
        if !other.widths.is_empty() {
            self.tail_basic = other.tail_basic;
            self.tail_before_basic = other.tail_before_basic;
        }
        self.widths.extend_from_slice(&other.widths);
        self.physical_len = physical_len;
        Ok(())
    }

    /// Count the rows produced by Screen's short-line bypass followed by the
    /// exact production planner. An aligned empty cold seam is handled by the
    /// caller. No cells or physical output rows are created, even on cache miss.
    pub fn row_count(
        &self,
        cols: usize,
        cost_model: MonospaceKpCostModel,
        scratch: &mut LineWrapWidthPrefixScratch,
    ) -> usize {
        if self.physical_len <= cols || self.trimmed_tokens == 0 {
            return 1;
        }
        let widths = &self.widths[..self.trimmed_tokens];
        #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
        let cache_key = {
            let mut hasher = SipHasher::new();
            widths.len().hash(&mut hasher);
            for &width in widths {
                usize::from(width & 0x7f).hash(&mut hasher);
                (width & 0x80 != 0).hash(&mut hasher);
            }
            MemoizedWrapPointCacheKey {
                geometry_hash: hasher.finish128().as_bytes(),
                width: cols,
                cost_model,
            }
        };
        #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
        if let Some(cached) = memoized_wrap_point_cache_get(cache_key) {
            return cached.scorecard.line_count;
        }
        scratch.rebuild_metadata(
            widths
                .iter()
                .map(|&width| (usize::from(width & 0x7f), width & 0x80 != 0)),
        );
        let (plan, scorecard) = wrap_plan_and_scorecard(widths.len(), cols, cost_model, scratch);
        #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
        memoized_wrap_point_cache_insert(
            cache_key,
            MemoizedWrapPointCacheEntry {
                break_offsets: plan.break_offsets,
                scorecard,
            },
        );
        #[cfg(not(all(feature = "std", not(ft_disable_memoized_wrap_points))))]
        let _ = plan;
        scorecard.line_count
    }
}

// These are ordinary starters in Unicode grapheme segmentation: no prepend,
// continuation, RI, Hangul, Indic linker or emoji-ZWJ context. The conservative
// set includes the CJK seam in the native profiling corpus. Other boundaries
// require a basic starter on the opposite side or the text-bearing fallback.
fn geometry_basic_starter(ch: char) -> bool {
    matches!(ch, ' '..='~' | '\u{4e00}'..='\u{9fff}')
}

fn geometry_boundary_is_separate(
    left: &str,
    right: &str,
    scratch: &mut zeroize::Zeroizing<String>,
    max_bytes: usize,
) -> Option<bool> {
    use zeroize::Zeroize;
    let len = left.len().checked_add(right.len())?;
    if len > max_bytes {
        return None;
    }
    scratch.zeroize();
    if len > scratch.capacity() {
        // Drop the wiped previous allocation before reserving its replacement;
        // the capture budget covers one boundary buffer, not two at once.
        *scratch = zeroize::Zeroizing::new(String::new());
        scratch.try_reserve_exact(len).ok()?;
    }
    if scratch.capacity() > max_bytes {
        return None;
    }
    scratch.push_str(left);
    scratch.push_str(right);
    let mut graphemes = Graphemes::new(scratch);
    Some(
        graphemes.next() == Some(left)
            && graphemes.next() == Some(right)
            && graphemes.next().is_none(),
    )
}

/// Owned logical cells plus width-dependent break metadata. No output `Line`
/// or wide-cell padding is allocated until a row is requested. As with `Line`,
/// attached image handles are not deep-frozen by this representation.
#[derive(Debug, Clone)]
pub struct LineWrapLayout {
    tokens: Arc<[Cell]>,
    // Break offsets and width prefixes are relative to this active range.
    // The allocation can also retain excluded trailing spaces or other rows.
    token_range: Range<usize>,
    width_prefix: Option<Arc<LineWrapWidthPrefixScratch>>,
    #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
    geometry_hash: [u8; 16],
    break_offsets: Vec<usize>,
    // Initial plans derive exact row widths from their current CellRef
    // endpoints, including on memoized geometry hits. Retention discards this
    // temporary array and uses the source-bound width prefix instead.
    #[cfg(feature = "std")]
    row_widths: Option<Vec<usize>>,
    #[cfg(feature = "std")]
    image_free: bool,
    // Preserve the existing all-whitespace behavior, including line metadata.
    blank: Option<Line>,
    scorecard: LineWrapScorecard,
}

impl LineWrapLayout {
    #[cfg(feature = "std")]
    fn row_widths_from_cells(&self, cells: &[CellRef<'_>]) -> Option<Vec<usize>> {
        if cells.len() != self.token_range.len()
            || self.break_offsets.last().copied() != Some(cells.len())
        {
            return None;
        }
        // Line::visible_cells guarantees consecutive physical positions:
        // vector and token iterators advance by max(width, 1), while clustered
        // cells have normalized widths of one or two. Use the current source's
        // endpoints, not widths cached for another line with the same hash.
        let mut widths = Vec::with_capacity(self.break_offsets.len());
        let mut start = 0;
        for &stop in &self.break_offsets {
            if stop <= start || stop > cells.len() {
                return None;
            }
            let first = &cells[start];
            let last = &cells[stop - 1];
            let width = last
                .cell_index()
                .checked_add(last.width().max(1))?
                .checked_sub(first.cell_index())?;
            if width < stop - start {
                return None;
            }
            widths.push(width);
            start = stop;
        }
        Some(widths)
    }

    /// Retain width-independent geometry for repeated planning. Ordinary eager
    /// wrapping keeps using caller-owned scratch and does not pay this storage
    /// cost. Callers retaining a source must include this allocation in their
    /// retention budget. The prefix is built from this source, never from
    /// possibly stale scratch left by a memoized-plan hit.
    pub fn retain_width_prefix(mut self) -> Self {
        #[cfg(feature = "std")]
        {
            // Release temporary row widths before allocating the retained
            // prefix. Zero-width sources keep the scan fallback.
            self.row_widths = None;
        }
        if self.blank.is_none() && self.width_prefix.is_none() {
            let mut prefix = LineWrapWidthPrefixScratch {
                widths: Vec::with_capacity(self.token_range.len().saturating_add(1)),
                spaces: Vec::with_capacity(self.token_range.len()),
                word_widths: Vec::with_capacity(self.token_range.len()),
                has_spaces: false,
                #[cfg(feature = "std")]
                all_widths_positive: false,
            };
            prefix.rebuild(&self.tokens[self.token_range.clone()]);
            self.width_prefix = Some(Arc::new(prefix));
        }
        self
    }

    /// Plan another width from the same immutable token allocation. Content
    /// extraction and its geometry hash are independent of viewport width.
    pub fn replan(
        &self,
        width: usize,
        cost_model: MonospaceKpCostModel,
        scratch: &mut LineWrapWidthPrefixScratch,
    ) -> Self {
        if self.blank.is_some() {
            scratch.clear();
            return self.clone();
        }
        let layout = plan_wrap_tokens(
            Arc::clone(&self.tokens),
            self.token_range.clone(),
            #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
            self.geometry_hash,
            width,
            cost_model,
            self.width_prefix.clone(),
            scratch,
        );
        #[cfg(feature = "std")]
        let layout = {
            let mut layout = layout;
            layout.image_free = self.image_free;
            layout
        };
        layout
    }

    pub fn row_count(&self) -> usize {
        if self.blank.is_some() {
            1
        } else {
            self.break_offsets.len()
        }
    }

    /// Quality describes the whole logical line, not a materialized subset.
    pub fn scorecard(&self) -> LineWrapScorecard {
        self.scorecard
    }

    /// Materialize only the requested physical rows. Out-of-range ends are
    /// clipped; empty, reversed, and entirely out-of-range requests are empty.
    /// Wrap markers refer to the full logical line, including offscreen rows.
    pub fn materialize_rows(&self, rows: Range<usize>, seqno: SequenceNo) -> Vec<Line> {
        let end = rows.end.min(self.row_count());
        if rows.start >= end {
            return Vec::new();
        }
        if let Some(blank) = &self.blank {
            return vec![blank.clone()];
        }
        let mut lines = Vec::with_capacity(end - rows.start);
        for row in rows.start..end {
            let start = if row == 0 {
                0
            } else {
                self.break_offsets[row - 1]
            };
            let stop = self.break_offsets[row];
            lines.push(materialize_wrap_line(
                &self.tokens[self.token_range.start + start..self.token_range.start + stop],
                stop < self.token_range.len(),
                seqno,
                self.width_prefix.as_ref().map_or(stop - start, |prefix| {
                    prefix.width_between(start, stop).max(stop - start)
                }),
            ));
        }
        lines
    }

    /// Construct physical row views sharing this logical source. Read-only
    /// geometry, shaping and visible-cell iteration do not allocate cell arrays.
    /// Mutations and contiguous-cell access materialize an independent row.
    /// Serialization retains the eager representation. no_std stays eager.
    pub fn deferred_rows(&self, rows: Range<usize>, seqno: SequenceNo) -> Vec<Line> {
        #[cfg(feature = "std")]
        {
            static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            let disabled = *DISABLED.get_or_init(|| {
                std::env::var_os("FT_DISABLE_DEFERRED_REFLOW_CELLS")
                    .is_some_and(|value| value == "1")
            });
            if !disabled && self.blank.is_none() {
                let end = rows.end.min(self.row_count());
                if rows.start >= end {
                    return Vec::new();
                }
                return (rows.start..end)
                    .map(|row| {
                        #[cfg(test)]
                        WRAP_PHYSICAL_ROWS_CREATED.with(|count| count.set(count.get() + 1));
                        let start = if row == 0 {
                            0
                        } else {
                            self.break_offsets[row - 1]
                        };
                        let stop = self.break_offsets[row];
                        let range = self.token_range.start + start..self.token_range.start + stop;
                        let exact_width = self
                            .image_free
                            .then(|| {
                                self.row_widths
                                    .as_ref()
                                    .and_then(|widths| widths.get(row).copied())
                                    .or_else(|| {
                                        self.width_prefix.as_ref().and_then(|prefix| {
                                            prefix.exact_positive_width_between(start, stop)
                                        })
                                    })
                            })
                            .flatten();
                        let cells = match exact_width {
                            Some(width) => VecStorage::from_image_free_token_range(
                                Arc::clone(&self.tokens),
                                range,
                                stop < self.token_range.len(),
                                width,
                            ),
                            None => VecStorage::from_token_range(
                                Arc::clone(&self.tokens),
                                range,
                                stop < self.token_range.len(),
                            ),
                        };
                        Line {
                            cells: CellStorage::V(cells),
                            zones: Vec::new(),
                            seqno,
                            bits: LineBits::NONE,
                            #[cfg(feature = "appdata")]
                            appdata: Mutex::new(None),
                        }
                    })
                    .collect();
            }
        }
        self.materialize_rows(rows, seqno)
    }

    fn into_report(mut self, seqno: SequenceNo) -> LineWrapReport {
        // Move the passthrough line rather than cloning its metadata/appdata.
        let lines = if let Some(blank) = self.blank.take() {
            vec![blank]
        } else {
            self.materialize_rows(0..self.row_count(), seqno)
        };
        LineWrapReport {
            lines,
            scorecard: self.scorecard,
        }
    }
}

fn plan_wrap_tokens(
    tokens: Arc<[Cell]>,
    token_range: Range<usize>,
    #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))] geometry_hash: [u8; 16],
    width: usize,
    cost_model: MonospaceKpCostModel,
    width_prefix: Option<Arc<LineWrapWidthPrefixScratch>>,
    scratch: &mut LineWrapWidthPrefixScratch,
) -> LineWrapLayout {
    #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
    let cache_key = MemoizedWrapPointCacheKey {
        geometry_hash,
        width,
        cost_model,
    };
    #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
    if let Some(cached) = memoized_wrap_point_cache_get(cache_key) {
        return LineWrapLayout {
            tokens,
            token_range,
            width_prefix,
            geometry_hash,
            break_offsets: cached.break_offsets,
            #[cfg(feature = "std")]
            row_widths: None,
            #[cfg(feature = "std")]
            image_free: false,
            blank: None,
            scorecard: cached.scorecard,
        };
    }
    let active_tokens = &tokens[token_range.clone()];
    let prefix = match width_prefix.as_deref() {
        Some(prefix) => prefix,
        None => {
            scratch.rebuild(active_tokens);
            &*scratch
        }
    };
    let (plan, scorecard) = wrap_plan_and_scorecard(active_tokens.len(), width, cost_model, prefix);
    #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
    memoized_wrap_point_cache_insert(
        cache_key,
        MemoizedWrapPointCacheEntry {
            break_offsets: plan.break_offsets.clone(),
            scorecard,
        },
    );
    LineWrapLayout {
        tokens,
        token_range,
        width_prefix,
        #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
        geometry_hash,
        break_offsets: plan.break_offsets,
        #[cfg(feature = "std")]
        row_widths: None,
        #[cfg(feature = "std")]
        image_free: false,
        blank: None,
        scorecard,
    }
}

fn wrap_plan_and_scorecard(
    token_count: usize,
    width: usize,
    cost_model: MonospaceKpCostModel,
    prefix: &LineWrapWidthPrefixScratch,
) -> (MonospaceWrapPlan, LineWrapScorecard) {
    #[cfg(all(test, feature = "std"))]
    WRAP_PLANNER_CALLS.with(|count| count.set(count.get() + 1));
    let plan =
        bounded_monospace_wrap_plan_with_width_prefix(token_count, width, cost_model, prefix);
    let selected = evaluate_break_offsets_with_width_prefix(
        token_count,
        &plan.break_offsets,
        width,
        cost_model,
        prefix,
    );
    let greedy_offsets = greedy_break_offsets_from_width_prefix(token_count, width, prefix);
    let greedy = evaluate_break_offsets_with_width_prefix(
        token_count,
        &greedy_offsets,
        width,
        cost_model,
        prefix,
    );
    let scorecard = LineWrapScorecard {
        mode: plan.mode,
        greedy_total_cost: greedy.total_cost,
        selected_total_cost: selected.total_cost,
        badness_delta: saturating_diff_i64(selected.total_cost, greedy.total_cost),
        greedy_forced_breaks: greedy.forced_breaks,
        selected_forced_breaks: selected.forced_breaks,
        line_count: selected.line_count,
        estimated_states: plan.estimated_states,
        evaluated_states: plan.evaluated_states,
    };
    (plan, scorecard)
}

#[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
const MAX_MEMOIZED_WRAP_POINT_CACHE_ENTRIES: usize = 16_384;

#[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct MemoizedWrapPointCacheKey {
    geometry_hash: [u8; 16],
    width: usize,
    cost_model: MonospaceKpCostModel,
}

#[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct MemoizedWrapPointCacheEntry {
    break_offsets: Vec<usize>,
    scorecard: LineWrapScorecard,
}

#[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
#[derive(Debug, Default)]
struct MemoizedWrapPointCache {
    entries: std::collections::HashMap<MemoizedWrapPointCacheKey, MemoizedWrapPointCacheEntry>,
    order: std::collections::VecDeque<MemoizedWrapPointCacheKey>,
    #[cfg(test)]
    key_hits: std::collections::HashMap<MemoizedWrapPointCacheKey, usize>,
}

#[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
impl MemoizedWrapPointCache {
    fn get(&mut self, key: MemoizedWrapPointCacheKey) -> Option<MemoizedWrapPointCacheEntry> {
        let entry = self.entries.get(&key).cloned();
        #[cfg(test)]
        if entry.is_some() {
            self.key_hits
                .entry(key)
                .and_modify(|hits| *hits = hits.saturating_add(1))
                .or_insert(1);
        }
        entry
    }

    fn insert(&mut self, key: MemoizedWrapPointCacheKey, entry: MemoizedWrapPointCacheEntry) {
        if let Some(existing) = self.entries.get_mut(&key) {
            *existing = entry;
            return;
        }

        while self.entries.len() >= MAX_MEMOIZED_WRAP_POINT_CACHE_ENTRIES {
            let Some(evicted) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&evicted);
            #[cfg(test)]
            self.key_hits.remove(&evicted);
        }

        self.entries.insert(key, entry);
        self.order.push_back(key);
    }

    #[cfg(test)]
    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.key_hits.clear();
    }

    #[cfg(test)]
    fn peek(&self, key: MemoizedWrapPointCacheKey) -> Option<MemoizedWrapPointCacheEntry> {
        self.entries.get(&key).cloned()
    }

    #[cfg(test)]
    fn hits_for_key(&self, key: MemoizedWrapPointCacheKey) -> usize {
        self.key_hits.get(&key).copied().unwrap_or(0)
    }
}

#[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
fn memoized_wrap_point_cache() -> &'static std::sync::Mutex<MemoizedWrapPointCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<MemoizedWrapPointCache>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(MemoizedWrapPointCache::default()))
}

#[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
fn memoized_wrap_point_cache_get(
    key: MemoizedWrapPointCacheKey,
) -> Option<MemoizedWrapPointCacheEntry> {
    memoized_wrap_point_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(key)
}

#[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
fn memoized_wrap_point_cache_insert(
    key: MemoizedWrapPointCacheKey,
    entry: MemoizedWrapPointCacheEntry,
) {
    memoized_wrap_point_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, entry);
}

#[cfg(all(test, feature = "std", not(ft_disable_memoized_wrap_points)))]
fn memoized_wrap_point_cache_clear_for_test() {
    memoized_wrap_point_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

#[cfg(all(test, feature = "std", not(ft_disable_memoized_wrap_points)))]
fn memoized_wrap_point_cache_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(all(test, feature = "std", not(ft_disable_memoized_wrap_points)))]
fn memoized_wrap_point_cache_entry_for_test(
    line: &Line,
    width: usize,
    cost_model: MonospaceKpCostModel,
) -> Option<MemoizedWrapPointCacheEntry> {
    let key = memoized_wrap_point_cache_key_for_test(line, width, cost_model)?;
    memoized_wrap_point_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .peek(key)
}

#[cfg(all(test, feature = "std", not(ft_disable_memoized_wrap_points)))]
fn memoized_wrap_point_cache_key_for_test(
    line: &Line,
    width: usize,
    cost_model: MonospaceKpCostModel,
) -> Option<MemoizedWrapPointCacheKey> {
    let mut cells: Vec<CellRef> = line.visible_cells().collect();
    let end_idx = cells.iter().rposition(|c| c.str() != " ")?;
    cells.truncate(end_idx + 1);
    Some(MemoizedWrapPointCacheKey {
        geometry_hash: compute_wrap_geometry_hash(line.bits, &cells),
        width,
        cost_model,
    })
}

#[cfg(all(test, feature = "std", not(ft_disable_memoized_wrap_points)))]
fn memoized_wrap_point_cache_key_hits_for_test(
    line: &Line,
    width: usize,
    cost_model: MonospaceKpCostModel,
) -> usize {
    let Some(key) = memoized_wrap_point_cache_key_for_test(line, width, cost_model) else {
        return 0;
    };
    memoized_wrap_point_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .hits_for_key(key)
}

/// Reusable prefix-width storage for resize-time line wrapping.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LineWrapWidthPrefixScratch {
    widths: Vec<u128>,
    spaces: Vec<bool>,
    word_widths: Vec<u128>,
    has_spaces: bool,
    #[cfg(feature = "std")]
    all_widths_positive: bool,
}

impl LineWrapWidthPrefixScratch {
    pub fn clear(&mut self) {
        self.widths.clear();
        self.spaces.clear();
        self.word_widths.clear();
        self.has_spaces = false;
        #[cfg(feature = "std")]
        {
            self.all_widths_positive = false;
        }
    }

    pub fn capacity(&self) -> usize {
        // Existing callers charge capacity in u128 units. Round up the space
        // metadata so that retained and peak-working budgets cover every Vec.
        self.widths
            .capacity()
            .saturating_add(self.word_widths.capacity())
            .saturating_add(
                self.spaces
                    .capacity()
                    .div_ceil(core::mem::size_of::<u128>()),
            )
    }

    fn rebuild(&mut self, tokens: &[Cell]) {
        self.rebuild_metadata(tokens.iter().map(|cell| (cell.width(), cell.str() == " ")));
    }

    fn rebuild_metadata(&mut self, widths: impl ExactSizeIterator<Item = (usize, bool)>) {
        self.widths.clear();
        self.spaces.clear();
        self.word_widths.clear();
        self.has_spaces = false;
        self.spaces.reserve(widths.len());
        self.widths.reserve(widths.len().saturating_add(1));
        self.widths.push(0);
        #[cfg(feature = "std")]
        {
            self.all_widths_positive = true;
        }

        let mut total = 0u128;
        for (width, space) in widths {
            self.spaces.push(space);
            self.has_spaces |= space;
            #[cfg(feature = "std")]
            {
                self.all_widths_positive &= width > 0;
            }
            total = total.saturating_add(width as u128);
            self.widths.push(total);
        }
        if self.has_spaces {
            self.word_widths.resize(self.spaces.len(), 0);
            let mut start = 0;
            while start < self.spaces.len() {
                if self.spaces[start] {
                    start += 1;
                    continue;
                }
                let end = (start..self.spaces.len())
                    .find(|&i| self.spaces[i])
                    .unwrap_or(self.spaces.len());
                let width = self.widths[end].saturating_sub(self.widths[start]);
                self.word_widths[start..end].fill(width);
                start = end;
            }
        }
    }

    fn word_boundary(&self, offset: usize) -> bool {
        offset == 0 || offset == self.spaces.len() || self.spaces[offset - 1] || self.spaces[offset]
    }

    fn allowed_word_break(&self, start: usize, end: usize, width: usize) -> bool {
        // Every separator that fits stays on the current row. Otherwise an
        // equal-cost DP tie can move it to the start of the next row, adding
        // indentation that was absent from the source. When the word fills
        // the row, preserve the separator on the following row instead.
        if end < self.spaces.len()
            && self.spaces[end]
            && self.width_between(start, end + 1) <= width
        {
            return false;
        }
        if self.word_boundary(end) || !self.has_spaces {
            return true;
        }
        // Emergency splitting is permitted only inside a word wider than a
        // complete row. Spaces remain original cells, never collapsed/glue.
        self.word_widths[end] > width as u128
            || end == start + 1 && self.width_between(start, end) > width
    }

    #[cfg(feature = "std")]
    fn exact_positive_width_between(&self, start: usize, end: usize) -> Option<usize> {
        if !self.all_widths_positive || end <= start {
            return None;
        }
        let width = self
            .widths
            .get(end)?
            .checked_sub(*self.widths.get(start)?)?;
        (width <= usize::MAX as u128).then_some(width as usize)
    }

    #[inline]
    fn width_between(&self, start: usize, end: usize) -> usize {
        let Some(&start_width) = self.widths.get(start) else {
            return 0;
        };
        let Some(&end_width) = self.widths.get(end) else {
            return 0;
        };
        if end <= start {
            return 0;
        }

        let width = end_width.saturating_sub(start_width);
        width.min(usize::MAX as u128) as usize
    }
}

impl LineWrapReport {
    fn into_parts(self) -> (Vec<Line>, MonospaceWrapMode) {
        let mode = self.scorecard.mode;
        (self.lines, mode)
    }
}

#[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
fn compute_wrap_geometry_hash(bits: LineBits, cells: &[CellRef<'_>]) -> [u8; 16] {
    static CONTENT_KEY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let use_content_key = *CONTENT_KEY.get_or_init(|| {
        std::env::var_os("FT_DISABLE_WRAP_GEOMETRY_KEY").is_some_and(|value| value == "1")
    });
    let mut hasher = SipHasher::new();
    if use_content_key {
        bits.bits().hash(&mut hasher);
    } else {
        // The planner, scorer and tie-breaker use ordered widths and spaces,
        // token count, target width and cost model. Cache only their offsets
        // and scorecard; materialization always reads the caller's current
        // cells, including colors, links and mutable image attachments.
        // Hashing display attributes here needlessly defeats geometry reuse.
        cells.len().hash(&mut hasher);
    }
    for cell in cells {
        if use_content_key {
            cell.compute_shape_hash(&mut hasher);
        }
        cell.width().hash(&mut hasher);
        (cell.str() == " ").hash(&mut hasher);
    }
    hasher.finish128().as_bytes()
}

#[cfg(test)]
#[inline]
fn greedy_break_offsets_from_tokens(tokens: &[Cell], width: usize) -> Vec<usize> {
    let mut width_prefix_scratch = LineWrapWidthPrefixScratch::default();
    width_prefix_scratch.rebuild(tokens);
    greedy_break_offsets_from_width_prefix(tokens.len(), width, &width_prefix_scratch)
}

#[inline]
fn greedy_break_offsets_from_width_prefix(
    token_count: usize,
    width: usize,
    width_prefix: &LineWrapWidthPrefixScratch,
) -> Vec<usize> {
    if width_prefix.has_spaces {
        let mut offsets = Vec::new();
        let mut start = 0;
        while start < token_count {
            let mut end = start;
            let mut last_allowed_break = None;
            while end < token_count {
                let next = end + 1;
                if end > start && width_prefix.width_between(start, next) > width {
                    break;
                }
                end = next;
                if width_prefix.allowed_word_break(start, end, width) {
                    last_allowed_break = Some(end);
                }
                if width_prefix.width_between(start, end) > width {
                    break;
                }
            }
            let stop = if end == token_count {
                end
            } else {
                last_allowed_break.unwrap_or(end)
            };
            offsets.push(stop);
            start = stop;
        }
        return offsets;
    }
    let mut offsets = Vec::new();
    let mut current_width = 0usize;

    for idx in 0..token_count {
        let token_width = width_prefix.width_between(idx, idx + 1);
        let need_new_line = current_width > 0 && current_width.saturating_add(token_width) > width;
        if need_new_line {
            offsets.push(idx);
            current_width = 0;
        }
        current_width = current_width.saturating_add(token_width);
    }

    offsets.push(token_count);
    offsets
}

#[inline]
fn fallback_wrap_plan(
    token_count: usize,
    width: usize,
    estimated_states: usize,
    evaluated_states: usize,
    width_prefix: &LineWrapWidthPrefixScratch,
) -> MonospaceWrapPlan {
    MonospaceWrapPlan {
        mode: MonospaceWrapMode::Fallback,
        break_offsets: greedy_break_offsets_from_width_prefix(token_count, width, width_prefix),
        estimated_states,
        evaluated_states,
    }
}

/// Compute a bounded DP wrap plan and deterministically fall back to greedy wrapping
/// when the configured state budget would be exceeded.
#[cfg(test)]
pub(crate) fn bounded_monospace_wrap_plan(
    tokens: &[Cell],
    width: usize,
    model: MonospaceKpCostModel,
) -> MonospaceWrapPlan {
    let mut width_prefix_scratch = LineWrapWidthPrefixScratch::default();
    width_prefix_scratch.rebuild(tokens);
    bounded_monospace_wrap_plan_with_width_prefix(tokens.len(), width, model, &width_prefix_scratch)
}

fn bounded_monospace_wrap_plan_with_width_prefix(
    token_count: usize,
    width: usize,
    model: MonospaceKpCostModel,
    width_prefix: &LineWrapWidthPrefixScratch,
) -> MonospaceWrapPlan {
    if token_count == 0 {
        return MonospaceWrapPlan {
            mode: MonospaceWrapMode::Dp,
            break_offsets: vec![],
            estimated_states: 0,
            evaluated_states: 0,
        };
    }

    let estimated_states = model.estimated_dp_states(token_count);
    if width == 0 || model.should_fallback(token_count) {
        return fallback_wrap_plan(token_count, width, estimated_states, 0, width_prefix);
    }

    let mut evaluated_states = 0usize;
    let mut best: Vec<Option<MonospaceBreakCandidate>> = vec![None; token_count + 1];
    best[0] = Some(MonospaceBreakCandidate {
        total_cost: 0,
        forced_breaks: 0,
        max_line_badness: 0,
        line_count: 0,
        break_offsets: vec![],
    });
    // A bounded lookahead can omit feasible edges, including a full row
    // wider than lookahead_limit tokens. Keep greedy as a complete feasible
    // incumbent: searching fewer edges must never make the selected layout
    // worse. The normal candidate comparator retains all tie-break rules.
    let greedy_breaks = greedy_break_offsets_from_width_prefix(token_count, width, width_prefix);
    best[token_count] = Some(evaluate_break_offsets_with_width_prefix(
        token_count,
        &greedy_breaks,
        width,
        model,
        width_prefix,
    ));

    for start in 0..token_count {
        let Some(prefix) = best[start].clone() else {
            continue;
        };

        let max_end = start.saturating_add(model.lookahead_limit).min(token_count);

        // `end` is a dynamic-programming endpoint, not a bare slice index: it is used
        // arithmetically (width_between(start, end), `end == token_count`, pushed into
        // break_offsets) and to index a different region of `best` (read+write of
        // best[end]) while best[start] is also borrowed. The needless_range_loop
        // enumerate() rewrite does not apply here.
        #[allow(clippy::needless_range_loop)]
        for end in (start + 1)..=max_end {
            let line_width = width_prefix.width_between(start, end);
            evaluated_states = evaluated_states.saturating_add(1);
            if evaluated_states > model.max_dp_states {
                return fallback_wrap_plan(
                    token_count,
                    width,
                    estimated_states,
                    evaluated_states,
                    width_prefix,
                );
            }
            if line_width > width && end > start + 1 {
                break;
            }
            if !width_prefix.allowed_word_break(start, end, width) {
                continue;
            }

            let is_last_line = end == token_count;
            let (line_cost, forced_break_inc) = if line_width > width {
                if end == start + 1 {
                    // Preserve deterministic behavior for over-wide graphemes by forcing
                    // exactly one token on this line and charging a fixed overflow penalty.
                    let overflow_cols = line_width.saturating_sub(width) as u64;
                    let overflow_penalty = model
                        .forced_break_penalty
                        .saturating_mul(overflow_cols.max(1));
                    (overflow_penalty, 1usize)
                } else {
                    break;
                }
            } else {
                let slack = width.saturating_sub(line_width) as i64;
                let forced =
                    usize::from(width_prefix.has_spaces && !width_prefix.word_boundary(end));
                (
                    model
                        .line_badness(slack, width, is_last_line)
                        .saturating_add(model.forced_break_penalty.saturating_mul(forced as u64)),
                    forced,
                )
            };

            let mut break_offsets = prefix.break_offsets.clone();
            break_offsets.push(end);
            let candidate = MonospaceBreakCandidate {
                total_cost: prefix.total_cost.saturating_add(line_cost),
                forced_breaks: prefix.forced_breaks.saturating_add(forced_break_inc),
                max_line_badness: prefix.max_line_badness.max(line_cost),
                line_count: prefix.line_count + 1,
                break_offsets,
            };

            match &best[end] {
                Some(existing) => {
                    if compare_monospace_break_candidates(&candidate, existing) == Ordering::Less {
                        best[end] = Some(candidate);
                    }
                }
                None => best[end] = Some(candidate),
            }

            if line_width > width {
                break;
            }
        }
    }

    match best[token_count].take() {
        Some(candidate) => MonospaceWrapPlan {
            mode: MonospaceWrapMode::Dp,
            break_offsets: candidate.break_offsets,
            estimated_states,
            evaluated_states,
        },
        None => fallback_wrap_plan(
            token_count,
            width,
            estimated_states,
            evaluated_states,
            width_prefix,
        ),
    }
}

#[inline]
fn evaluate_break_offsets_with_width_prefix(
    token_count: usize,
    break_offsets: &[usize],
    width: usize,
    model: MonospaceKpCostModel,
    width_prefix: &LineWrapWidthPrefixScratch,
) -> MonospaceBreakCandidate {
    let mut total_cost = 0u64;
    let mut forced_breaks = 0usize;
    let mut max_line_badness = 0u64;
    let mut line_count = 0usize;
    let mut normalized_breaks = Vec::new();
    let mut start = 0usize;

    for &raw_end in break_offsets {
        let end = raw_end.min(token_count);
        if end <= start {
            continue;
        }

        let line_width = width_prefix.width_between(start, end);
        let is_last_line = end == token_count;

        let (line_cost, forced_inc) = if line_width > width {
            if end == start + 1 {
                let overflow_cols = line_width.saturating_sub(width) as u64;
                (
                    model
                        .forced_break_penalty
                        .saturating_mul(overflow_cols.max(1)),
                    1usize,
                )
            } else {
                (KP_BADNESS_INF, 1usize)
            }
        } else {
            let slack = width.saturating_sub(line_width) as i64;
            let forced = usize::from(width_prefix.has_spaces && !width_prefix.word_boundary(end));
            let cost = if width_prefix.allowed_word_break(start, end, width) {
                model
                    .line_badness(slack, width, is_last_line)
                    .saturating_add(model.forced_break_penalty.saturating_mul(forced as u64))
            } else {
                KP_BADNESS_INF
            };
            (cost, forced)
        };

        total_cost = total_cost.saturating_add(line_cost);
        forced_breaks = forced_breaks.saturating_add(forced_inc);
        max_line_badness = max_line_badness.max(line_cost);
        line_count = line_count.saturating_add(1);
        normalized_breaks.push(end);
        start = end;
    }

    if start < token_count {
        let line_width = width_prefix.width_between(start, token_count);
        let (line_cost, forced_inc) = if line_width > width {
            if token_count == start + 1 {
                let overflow_cols = line_width.saturating_sub(width) as u64;
                (
                    model
                        .forced_break_penalty
                        .saturating_mul(overflow_cols.max(1)),
                    1usize,
                )
            } else {
                (KP_BADNESS_INF, 1usize)
            }
        } else {
            let slack = width.saturating_sub(line_width) as i64;
            (model.line_badness(slack, width, true), 0usize)
        };

        total_cost = total_cost.saturating_add(line_cost);
        forced_breaks = forced_breaks.saturating_add(forced_inc);
        max_line_badness = max_line_badness.max(line_cost);
        line_count = line_count.saturating_add(1);
        normalized_breaks.push(token_count);
    }

    MonospaceBreakCandidate {
        total_cost,
        forced_breaks,
        max_line_badness,
        line_count,
        break_offsets: normalized_breaks,
    }
}

#[inline]
fn saturating_diff_i64(lhs: u64, rhs: u64) -> i64 {
    let lhs = lhs as i128;
    let rhs = rhs as i128;
    let diff = lhs - rhs;
    diff.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

#[inline]
#[cfg(test)]
fn materialize_wrap_lines_from_tokens(
    tokens: &[Cell],
    break_offsets: &[usize],
    seqno: SequenceNo,
) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut start = 0usize;

    for &end in break_offsets {
        if end < start || end > tokens.len() {
            continue;
        }

        lines.push(materialize_wrap_line(
            &tokens[start..end],
            end < tokens.len(),
            seqno,
            end - start,
        ));
        start = end;
    }

    if lines.is_empty() {
        lines.push(Line::from_cells(vec![], seqno));
    }

    lines
}

fn materialize_wrap_line(
    tokens: &[Cell],
    wrapped: bool,
    seqno: SequenceNo,
    cell_capacity: usize,
) -> Line {
    #[cfg(all(test, feature = "std"))]
    WRAP_PHYSICAL_ROWS_CREATED.with(|count| count.set(count.get() + 1));
    // Retained geometry supplies the row's display width without another
    // Unicode scan. Eager wrapping reserves at least one slot per token.
    let mut cells = Vec::with_capacity(cell_capacity);
    for token in tokens {
        let grapheme = token.clone();
        let fill_count = grapheme.width().saturating_sub(1);
        let fill_attr = grapheme.attrs().clone();
        cells.push(grapheme);
        for _ in 0..fill_count {
            cells.push(Cell::blank_with_attrs(fill_attr.clone()));
        }
    }
    let mut line = Line::from_cells(cells, seqno);
    if wrapped {
        line.set_last_cell_was_wrapped(true, seqno);
    }
    line
}

impl<'a> From<&'a str> for Line {
    fn from(s: &str) -> Line {
        Line::from_text(s, &CellAttributes::default(), SEQ_ZERO, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SEQ_ZERO;
    use alloc::borrow::ToOwned;
    use alloc::collections::BTreeSet;
    use alloc::format;
    use frankenterm_cell::{Cell, CellAttributes, SemanticType};

    #[test]
    fn visible_text_bytes_matches_cell_accounting_across_storage_and_edits() {
        for text in [
            "",
            "plain ascii  ",
            "界面 e\u{301} 👩\u{200d}💻  ",
            "\u{301}a",
        ] {
            let mut line = Line::from_text(text, &CellAttributes::default(), 7, None);
            for clustered in [false, true, false] {
                if clustered {
                    line.compress_for_scrollback();
                    assert!(matches!(line.cells, CellStorage::C(_)));
                } else {
                    line.coerce_vec_storage();
                    assert!(matches!(line.cells, CellStorage::V(_)));
                }
                let expected = line
                    .visible_cells()
                    .try_fold(0usize, |bytes, cell| bytes.checked_add(cell.str().len()));
                assert_eq!(line.visible_text_bytes(), expected, "{text:?}");
                assert_eq!(line.visible_text_bytes(), Some(line.as_str().len()));
            }

            // A vector mutation must not leave a cached compressed-text count.
            line.set_cell(0, Cell::new('界', CellAttributes::default()), 8);
            let expected = line
                .visible_cells()
                .try_fold(0usize, |bytes, cell| bytes.checked_add(cell.str().len()));
            assert_eq!(line.visible_text_bytes(), expected);
            line.compress_for_scrollback();
            assert_eq!(line.visible_text_bytes(), expected);
        }
    }

    #[test]
    fn incomplete_logical_prefix_preserves_trailing_spaces_and_geometry() {
        for text in ["abc def ", "abc def   ", "界面 e\u{301}  ", "        "] {
            for cols in [1, 3, 4, 8] {
                let line = Line::from_text(text, &CellAttributes::default(), 7, None);
                let mut geometry = LineWrapGeometry::capture(&line, usize::MAX).unwrap();
                geometry.preserve_trailing_spaces();
                let model = MonospaceKpCostModel::terminal_default();
                let layout = line.plan_wrap_preserving_trailing_spaces(
                    cols,
                    model,
                    &mut LineWrapWidthPrefixScratch::default(),
                );
                let rows = layout.deferred_rows(0..layout.row_count(), 7);
                let actual: String = rows.iter().map(|row| row.as_str().into_owned()).collect();
                assert_eq!(actual, text, "incomplete prefix at width {cols}");
                assert_eq!(
                    geometry.row_count(cols, model, &mut LineWrapWidthPrefixScratch::default()),
                    layout.row_count(),
                    "geometry must count the preserved prefix at width {cols}",
                );
            }
        }
    }

    #[test]
    fn leading_space_before_overwide_word_does_not_waste_a_row() {
        let source = Line::from_text(" abcdef", &CellAttributes::default(), 7, None);
        for fallback in [false, true] {
            let mut model = MonospaceKpCostModel::terminal_default();
            if fallback {
                model.max_dp_states = 0;
            }
            let report = source.clone().wrap_with_report(5, 7, model);
            let rows: Vec<String> = report
                .lines
                .iter()
                .map(|row| row.as_str().into_owned())
                .collect();
            assert_eq!(rows.concat(), " abcdef");
            assert_eq!(rows.len(), 2, "an overwide word can use the first row");
            assert!(rows.iter().all(|row| row.len() <= 5));
            assert!(rows.iter().all(|row| row.chars().any(|c| c != ' ')));
            if fallback {
                assert_eq!(rows, [" abcd", "ef"]);
            }
        }
    }

    #[test]
    fn prose_wrap_has_exact_source_preserving_rows_in_dp_and_fallback() {
        let cases: &[(&str, usize, &[&str], &[usize])] = &[
            (
                "alpha beta gamma",
                8,
                &["alpha ", "beta ", "gamma"],
                &[6, 11, 16],
            ),
            (
                "alpha  beta gamma",
                8,
                &["alpha  ", "beta ", "gamma"],
                &[7, 12, 17],
            ),
            ("a       b", 4, &["a   ", "    ", "b"], &[4, 8, 9]),
            ("x aa\u{a0}bb", 5, &["x ", "aa\u{a0}bb"], &[2, 7]),
            ("  ab  cd", 4, &["  ab", "  cd"], &[4, 8]),
            ("ab  cd ef", 5, &["ab  ", "cd ef"], &[4, 9]),
            (
                "e\u{301}é 界界 ok",
                5,
                &["e\u{301}é ", "界界 ", "ok"],
                &[3, 6, 8],
            ),
            // A terminal preserves the separator as a real cell. With both
            // words filling a row, keeping those words intact requires a
            // separator row; silently eliding it would corrupt source offsets.
            ("hello world", 5, &["hello", " ", "world"], &[5, 6, 11]),
            // Entirely blank lines retain the existing passthrough contract.
            ("       ", 3, &["       "], &[]),
        ];
        for &(text, width, expected_rows, expected_offsets) in cases {
            for fallback in [false, true] {
                let mut model = MonospaceKpCostModel::terminal_default();
                if fallback {
                    model.max_dp_states = 0;
                }
                let source = Line::from_text(text, &CellAttributes::default(), 7, None);
                let mut scratch = LineWrapWidthPrefixScratch::default();
                let geometry = LineWrapGeometry::capture(&source, 1 << 20).unwrap();
                let layout = source.plan_wrap_with_width_prefix_scratch(width, model, &mut scratch);
                assert_eq!(layout.break_offsets, expected_offsets, "source {text:?}");
                let rows: Vec<String> = layout
                    .materialize_rows(0..layout.row_count(), 7)
                    .iter()
                    .map(|row| row.as_str().into_owned())
                    .collect();
                assert_eq!(rows, expected_rows, "source {text:?}");
                assert_eq!(rows.concat(), text, "every separator and grapheme survives");
                assert_eq!(
                    geometry.row_count(width, model, &mut scratch),
                    expected_rows.len()
                );
            }
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn prose_wrap_retained_prefix_replans_and_cold_join_keeps_boundaries() {
        for fallback in [false, true] {
            let mut model = MonospaceKpCostModel::terminal_default();
            if fallback {
                model.max_dp_states = 0;
            }
            let mut scratch = LineWrapWidthPrefixScratch::default();
            let source = Line::from_text("alpha beta gamma", &CellAttributes::default(), 7, None);
            let retained = source
                .plan_wrap_with_width_prefix_scratch(8, model, &mut scratch)
                .retain_width_prefix();
            let prefix = retained.width_prefix.as_ref().unwrap();
            let mut joined = LineWrapGeometry::capture(
                &Line::from_text("alpha beta ", &CellAttributes::default(), 7, None),
                1 << 20,
            )
            .unwrap();
            joined
                .try_append(
                    &LineWrapGeometry::capture(
                        &Line::from_text("gamma", &CellAttributes::default(), 7, None),
                        1 << 20,
                    )
                    .unwrap(),
                    1 << 20,
                )
                .unwrap();
            for (width, expected) in [(8, vec![6, 11, 16]), (11, vec![11, 16])] {
                scratch.rebuild(&cells_from_text("xxxxxxxxxxxxxxxx"));
                let layout = retained.replan(width, model, &mut scratch);
                assert!(Arc::ptr_eq(prefix, layout.width_prefix.as_ref().unwrap()));
                assert_eq!(layout.break_offsets, expected);
                assert_eq!(joined.row_count(width, model, &mut scratch), expected.len());
                assert_eq!(
                    layout
                        .materialize_rows(0..layout.row_count(), 7)
                        .iter()
                        .map(|row| row.as_str().into_owned())
                        .collect::<String>(),
                    "alpha beta gamma"
                );
            }
        }
    }

    #[test]
    fn prose_wrap_preserves_words_cells_and_cold_geometry_in_dp_and_fallback() {
        let mut attrs = CellAttributes::default();
        attrs.set_italic(true);
        for text in [
            "alpha beta gamma",
            "e\u{301}clair 界界 hello",
            "ab abcdefghijkl xy",
        ] {
            let source = Line::from_text(text, &attrs, 7, None);
            let expected: Vec<String> = source
                .visible_cells()
                .map(|cell| cell.str().to_owned())
                .collect();
            for cols in [4, 8] {
                for fallback in [false, true] {
                    let mut model = MonospaceKpCostModel::terminal_default();
                    if fallback {
                        model.max_dp_states = 0;
                    }
                    let mut scratch = LineWrapWidthPrefixScratch::default();
                    let layout = source.clone().plan_wrap_with_width_prefix_scratch(
                        cols,
                        model,
                        &mut scratch,
                    );
                    // A memoized hit intentionally leaves scratch untouched.
                    scratch.rebuild(
                        &source
                            .visible_cells()
                            .map(|cell| cell.as_cell())
                            .collect::<Vec<_>>(),
                    );
                    let mut start = 0;
                    for &end in &layout.break_offsets {
                        assert!(scratch.allowed_word_break(start, end, cols));
                        start = end;
                    }
                    let rows = layout.materialize_rows(0..layout.row_count(), 7);
                    let actual: Vec<String> = rows
                        .iter()
                        .flat_map(|row| row.visible_cells().map(|cell| cell.str().to_owned()))
                        .collect();
                    assert_eq!(actual, expected);
                    assert!(rows
                        .iter()
                        .all(|row| row.visible_cells().all(|cell| cell.attrs().italic())));
                    let geometry = LineWrapGeometry::capture(&source, 1 << 20).unwrap();
                    assert_eq!(geometry.row_count(cols, model, &mut scratch), rows.len());
                    assert_eq!(
                        layout.scorecard.mode,
                        if fallback {
                            MonospaceWrapMode::Fallback
                        } else {
                            MonospaceWrapMode::Dp
                        }
                    );
                }
            }
        }
    }

    #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
    #[test]
    fn prose_wrap_cache_distinguishes_equal_width_different_word_boundaries() {
        let _guard = memoized_wrap_point_cache_test_lock().lock().unwrap();
        memoized_wrap_point_cache_clear_for_test();
        let model = MonospaceKpCostModel::terminal_default();
        let first = Line::from_text("abcde fghij", &CellAttributes::default(), 1, None);
        let second = Line::from_text("ab cdefghij", &CellAttributes::default(), 1, None);
        let mut scratch = LineWrapWidthPrefixScratch::default();
        let first = first.plan_wrap_with_width_prefix_scratch(8, model, &mut scratch);
        let second = second.plan_wrap_with_width_prefix_scratch(8, model, &mut scratch);
        assert_ne!(first.geometry_hash, second.geometry_hash);
        assert_ne!(first.break_offsets, second.break_offsets);
        assert_eq!(first.break_offsets, vec![6, 11]);
        assert_eq!(second.break_offsets, vec![3, 11]);
        let left = Line::from_text("abcde ", &CellAttributes::default(), 1, None);
        let right = Line::from_text("fghij", &CellAttributes::default(), 1, None);
        let mut joined = LineWrapGeometry::capture(&left, 1 << 20).unwrap();
        joined
            .try_append(
                &LineWrapGeometry::capture(&right, 1 << 20).unwrap(),
                1 << 20,
            )
            .unwrap();
        assert_eq!(joined.row_count(8, model, &mut scratch), first.row_count());
    }

    #[test]
    fn semantic_snapshot_ignores_compression_and_scan_cache_but_detects_cells() {
        let mut line = Line::from_text("plain text", &CellAttributes::default(), 7, None);
        let before = line.semantic_snapshot();
        line.compress_for_scrollback();
        assert!(line.matches_semantic_snapshot(&before));
        let rules = [Rule::new(r"https://[a-z.]+", "$0").unwrap()];
        Line::apply_hyperlink_rules(&rules, &mut [&mut line]);
        assert!(line.matches_semantic_snapshot(&before));
        let seqno = line.current_seqno();
        line.set_double_width(seqno);
        assert!(!line.matches_semantic_snapshot(&before));
    }

    fn geometry_screen_rows(source: Line, cols: usize, model: MonospaceKpCostModel) -> Vec<Line> {
        if source.len() <= cols {
            return vec![source];
        }
        let layout = source.plan_wrap_with_width_prefix_scratch(
            cols,
            model,
            &mut LineWrapWidthPrefixScratch::default(),
        );
        layout.deferred_rows(0..layout.row_count(), 19)
    }

    #[test]
    fn width_only_geometry_preserves_short_blank_trim_and_zero_width_counts() {
        let zero = Cell::new_grapheme(
            "\u{301}\u{302}\u{303}\u{304}",
            CellAttributes::blank(),
            None,
        );
        assert_eq!(zero.width(), 0);
        let sources = vec![
            Line::new(1),
            Line::from_text("          ", &CellAttributes::blank(), 1, None),
            Line::from_text("a界e\u{301}🚀b     ", &CellAttributes::blank(), 1, None),
            Line::from_text("   a    b  ", &CellAttributes::blank(), 1, None),
            Line::from_cells(
                vec![zero.clone(), Cell::new('a', CellAttributes::blank()), zero],
                1,
            ),
            // An overhanging wide token makes physical length differ from the
            // width sum. Short-line bypass must still use physical length.
            Line::from_cells(vec![Cell::new('界', CellAttributes::blank())], 1),
        ];
        for source in sources {
            let geometry = LineWrapGeometry::capture(&source, 4096).unwrap();
            assert_eq!(geometry.physical_len(), source.len());
            assert!(geometry.retained_bytes() <= 4096);
            let mut scratch = LineWrapWidthPrefixScratch::default();
            for max_dp_states in [0, 8192] {
                for lookahead_limit in [1, 64] {
                    let model = MonospaceKpCostModel {
                        max_dp_states,
                        lookahead_limit,
                        ..MonospaceKpCostModel::terminal_default()
                    };
                    for cols in [0, 1, 2, 3, 5, 12, 100] {
                        // Poison caller scratch before both cached and uncached
                        // counts; stale prefixes must not become source data.
                        scratch.rebuild(&cells_from_text("界界abcdef"));
                        let expected = geometry_screen_rows(source.clone(), cols, model).len();
                        #[cfg(feature = "std")]
                        let created = WRAP_PHYSICAL_ROWS_CREATED.with(|count| count.get());
                        assert_eq!(geometry.row_count(cols, model, &mut scratch), expected);
                        assert_eq!(geometry.row_count(cols, model, &mut scratch), expected);
                        #[cfg(feature = "std")]
                        assert_eq!(
                            WRAP_PHYSICAL_ROWS_CREATED.with(|count| count.get()),
                            created
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn width_only_geometry_certifies_native_unicode_physical_joins() {
        let text = format!(
            "00000 {}",
            "Text reflow: ASCII ligatures ffi =>, 界面, e\u{301}, 🚀. ".repeat(9)
        );
        let source = Line::from_text(&text, &CellAttributes::blank(), 1, None);
        // Match Performer::flush_print: write even an overhanging wide glyph,
        // then wrap before the next glyph. The native corpus enters at 173
        // columns; its second physical row therefore occupies 174 columns.
        let mut physical = Vec::new();
        let mut cells = Vec::new();
        for cell in source.visible_cells() {
            if cells.len() >= 173 {
                physical.push(Line::from_cells(core::mem::take(&mut cells), 1));
            }
            cells.push(cell.as_cell());
            for _ in 1..cell.width() {
                cells.push(Cell::blank());
            }
        }
        physical.push(Line::from_cells(cells, 1));
        assert_eq!(physical.len(), 3);
        assert_eq!(
            physical.iter().map(Line::len).collect::<Vec<_>>(),
            [173, 174, 109]
        );
        assert_eq!(physical[1].visible_cells().last().unwrap().str(), "面");
        assert_eq!(physical[2].visible_cells().next().unwrap().str(), ",");
        for storage_mask in 0..8 {
            let mut rows = physical.clone();
            for (idx, row) in rows.iter_mut().enumerate() {
                if storage_mask & (1 << idx) != 0 {
                    row.compress_for_scrollback();
                }
                row.set_last_cell_was_wrapped(false, 1);
            }
            let mut logical = rows[0].clone();
            let mut geometry = LineWrapGeometry::capture(&rows[0], 4096).unwrap();
            assert!(geometry.supports_join());
            for row in &rows[1..] {
                let next = LineWrapGeometry::capture(row, 4096).unwrap();
                assert!(next.supports_join());
                assert_eq!(geometry.try_append(&next, 4096), Ok(()));
                logical.append_line(row.clone(), 1);
            }
            assert_eq!(geometry.physical_len(), logical.len());
            for cols in [0, 1, 2, 85, 100, 123, 173, 512] {
                let model = MonospaceKpCostModel::terminal_default();
                let expected = geometry_screen_rows(logical.clone(), cols, model).len();
                assert_eq!(
                    geometry.row_count(cols, model, &mut LineWrapWidthPrefixScratch::default()),
                    expected
                );
            }
            #[cfg(feature = "use_serde")]
            {
                // The durable sink serializes an explicitly materialized row;
                // pending rows can still be clustered. Both must certify the
                // same physical geometry before the snapshot is admitted.
                let mut materialized = logical.clone();
                let _ = materialized.cells_mut();
                let decoded: Line =
                    serde_json::from_value(serde_json::to_value(&materialized).unwrap()).unwrap();
                let restored = LineWrapGeometry::capture(&decoded, 4096).unwrap();
                assert_eq!(geometry.widths, restored.widths);
                assert_eq!(geometry.trimmed_tokens, restored.trimmed_tokens);
                assert_eq!(geometry.physical_len, restored.physical_len);
                assert!(restored.supports_join());
            }
        }
    }

    #[test]
    fn width_only_geometry_refuses_text_sensitive_or_storage_dependent_joins() {
        let left = Line::from_text("👩\u{200d}", &CellAttributes::blank(), 1, None);
        let right = Line::from_text("👩", &CellAttributes::blank(), 1, None);
        let mut geometry = LineWrapGeometry::capture(&left, 4096).unwrap();
        let original = geometry.clone();
        let next = LineWrapGeometry::capture(&right, 4096).unwrap();
        assert_eq!(
            geometry.try_append(&next, 4096),
            Err(LineWrapGeometryJoinError::UncertifiedBoundary)
        );
        assert_eq!(geometry, original);
        let mut vector = left.clone();
        let _ = vector.cells_mut();
        vector.append_line(right.clone(), 1);
        let mut clustered = left;
        clustered.compress_for_scrollback();
        clustered.append_line(right, 1);
        let model = MonospaceKpCostModel {
            max_dp_states: 0,
            ..MonospaceKpCostModel::terminal_default()
        };
        assert_ne!(
            geometry_screen_rows(vector, 2, model).len(),
            geometry_screen_rows(clustered, 2, model).len()
        );

        for source in [
            Line::from_cells(
                vec![Cell::new_grapheme(
                    "\u{301}\u{302}\u{303}\u{304}",
                    CellAttributes::blank(),
                    None,
                )],
                1,
            ),
            Line::from_cells(vec![Cell::new('界', CellAttributes::blank())], 1),
            Line::from_cells(
                vec![
                    Cell::new_grapheme_with_width("a", 2, CellAttributes::blank()),
                    Cell::blank(),
                ],
                1,
            ),
            Line::from_cells(
                vec![
                    Cell::new_grapheme("ab", CellAttributes::blank(), None),
                    Cell::blank(),
                ],
                1,
            ),
        ] {
            let unsafe_join = LineWrapGeometry::capture(&source, 4096).unwrap();
            assert!(!unsafe_join.supports_join());
            assert_eq!(
                geometry.try_append(&unsafe_join, 4096),
                Err(LineWrapGeometryJoinError::UncertifiedSource)
            );
            assert_eq!(geometry, original);
        }
    }

    #[test]
    fn width_only_geometry_enforces_capture_join_and_planning_budgets() {
        let accented = Line::from_text("e\u{301}", &CellAttributes::blank(), 1, None);
        let metadata_only = core::mem::size_of::<LineWrapGeometry>() + accented.len();
        assert!(LineWrapGeometry::capture(&accented, metadata_only).is_none());
        assert!(LineWrapGeometry::capture(&accented, metadata_only + "ae\u{301}".len()).is_some());
        let source = Line::from_text("abcdef   ", &CellAttributes::blank(), 1, None);
        assert!(
            LineWrapGeometry::capture(&source, core::mem::size_of::<LineWrapGeometry>()).is_none()
        );
        let mut geometry = LineWrapGeometry::capture(&source, 4096).unwrap();
        let other = geometry.clone();
        let original = geometry.clone();
        let too_small = core::mem::size_of::<LineWrapGeometry>() + 2 * geometry.widths.len() - 1;
        assert_eq!(
            geometry.try_append(&other, too_small),
            Err(LineWrapGeometryJoinError::ByteBudget)
        );
        assert_eq!(geometry, original);
        assert!(geometry.append(&other, 4096));
        let model = MonospaceKpCostModel::terminal_default();
        let mut scratch = LineWrapWidthPrefixScratch::default();
        scratch.rebuild(&cells_from_text("界abc"));
        let unchanged = scratch.clone();
        let bound = geometry
            .planning_bytes_upper_bound(3, model, &scratch)
            .unwrap();
        assert!(bound > geometry.retained_bytes());
        assert_eq!(
            geometry.row_count_with_budget(3, model, &mut scratch, bound - 1),
            None
        );
        assert_eq!(scratch, unchanged);
        assert!(geometry
            .row_count_with_budget(3, model, &mut scratch, bound)
            .is_some());
        assert_eq!(
            geometry.row_count_with_budget(100, model, &mut scratch, 0),
            None,
            "retained scratch remains charged even for a short line"
        );
        assert_eq!(
            geometry.row_count_with_budget(
                100,
                model,
                &mut LineWrapWidthPrefixScratch::default(),
                0
            ),
            Some(1)
        );
        let mut huge = geometry;
        huge.trimmed_tokens = usize::MAX;
        huge.physical_len = usize::MAX;
        assert_eq!(huge.planning_bytes_upper_bound(1, model, &scratch), None);
    }

    #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
    #[test]
    fn width_only_geometry_reuses_plans_without_materializing_rows() {
        let _guard = memoized_wrap_point_cache_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut source = Line::from_text("a界b🚀cde界f ghi  ", &CellAttributes::blank(), 1, None);
        let geometry = LineWrapGeometry::capture(&source, 4096).unwrap();
        let model = MonospaceKpCostModel {
            badness_scale: 37,
            max_dp_states: 0,
            ..MonospaceKpCostModel::terminal_default()
        };
        let mut scratch = LineWrapWidthPrefixScratch::default();
        let created = WRAP_PHYSICAL_ROWS_CREATED.with(|count| count.get());
        let expected = geometry.row_count(3, model, &mut scratch);
        let plans = WRAP_PLANNER_CALLS.with(|count| count.get());
        // A later source mutation cannot alter an admitted width snapshot.
        source.resize(1, 4);
        scratch.rebuild(&cells_from_text("abcdefghijklmnop"));
        let poisoned = scratch.clone();
        assert_eq!(geometry.row_count(3, model, &mut scratch), expected);
        assert_eq!(
            scratch, poisoned,
            "memoized count must not rebuild prefixes"
        );
        assert_eq!(WRAP_PLANNER_CALLS.with(|count| count.get()), plans);
        assert_eq!(
            WRAP_PHYSICAL_ROWS_CREATED.with(|count| count.get()),
            created
        );
        // The counter is attached to real row constructors, not this API.
        let control = Line::from_text("a界b🚀cde界f ghi  ", &CellAttributes::blank(), 1, None);
        assert_eq!(geometry_screen_rows(control, 3, model).len(), expected);
        assert!(WRAP_PHYSICAL_ROWS_CREATED.with(|count| count.get()) > created);
    }

    #[cfg(feature = "std")]
    #[test]
    fn reflow_content_equality_ignores_only_publication_sequence_and_derived_scan() {
        let source = Line::from_text("ab界e\u{301}🚀 xyz", &CellAttributes::blank(), 1, None);
        for storage in 0..3 {
            let mut original = source.clone();
            match storage {
                1 => original.compress_for_scrollback(),
                2 => {
                    original = source
                        .clone()
                        .plan_wrap_with_width_prefix_scratch(
                            7,
                            MonospaceKpCostModel::terminal_default(),
                            &mut LineWrapWidthPrefixScratch::default(),
                        )
                        .deferred_rows(0..1, 1)
                        .pop()
                        .unwrap();
                }
                _ => {}
            }
            let frozen = original.clone();
            assert!(original.is_same_reflow_source(&frozen));
            let mut published = original.clone();
            published.update_last_change_seqno(9);
            published.scan_and_create_hyperlinks(&[]);
            REFLOW_CONTENT_SHAPE_HASH_SCANS.with(|count| count.set(0));
            assert!(published.is_same_reflow_content(&frozen));
            assert!(frozen.is_same_reflow_content(&published));
            assert!(!published.is_same_reflow_source(&frozen));
            assert_eq!(REFLOW_CONTENT_SHAPE_HASH_SCANS.with(|count| count.get()), 0);
            if storage == 2 && std::env::var_os("FT_DISABLE_DEFERRED_REFLOW_CELLS").is_none() {
                let CellStorage::V(cells) = &published.cells else {
                    panic!("expected deferred cells");
                };
                assert!(cells.is_deferred_unmaterialized());
            }
            for mutation in 0..4 {
                let mut changed = original.clone();
                match mutation {
                    0 => {
                        changed.set_cell(0, Cell::new('Z', CellAttributes::blank()), 1);
                    }
                    1 => {
                        changed.cells_mut()[0].attrs_mut().set_italic(true);
                    }
                    2 => changed.set_last_cell_was_wrapped(!original.last_cell_was_wrapped(), 1),
                    _ => changed.set_double_width(1),
                }
                assert_eq!(changed.current_seqno(), frozen.current_seqno());
                assert!(!changed.is_same_reflow_content(&frozen));
                assert!(!changed.is_same_reflow_source(&frozen));
            }
        }
    }

    #[test]
    fn clustered_snapshots_share_payload_and_isolate_same_seqno_mutations() {
        let mutations: &[fn(&mut Line)] = &[
            |line| line.set_cell_grapheme(4, "界", 2, CellAttributes::blank(), 1),
            |line| assert!(line.append_ascii_cell_run(4, "tail", CellAttributes::blank(), 1)),
            |line| line.set_cell(4, Cell::new('x', CellAttributes::blank()), 1),
            |line| line.set_cell(0, Cell::new('x', CellAttributes::blank()), 1),
            |line| line.prune_trailing_blanks(1),
            |line| line.set_last_cell_was_wrapped(true, 1),
            |line| {
                line.append_line(
                    Line::from_text("tail", &CellAttributes::blank(), 1, None),
                    1,
                )
            },
            |line| line.cells_mut()[0] = Cell::new('x', CellAttributes::blank()),
        ];
        for mutate in mutations {
            let mut line = Line::from_text("abc ", &CellAttributes::blank(), 1, None);
            line.compress_for_scrollback();
            let mut bytes = usize::MAX;
            let mut work = usize::MAX;
            let frozen = line.try_clone_for_snapshot(&mut bytes, &mut work).unwrap();
            let ordinary_clone = line.clone();
            match (&line.cells, &frozen.cells, &ordinary_clone.cells) {
                (CellStorage::C(live), CellStorage::C(snapshot), CellStorage::C(cloned)) => {
                    assert!(Arc::ptr_eq(live, snapshot));
                    assert!(Arc::ptr_eq(live, cloned));
                    assert_eq!(live.text.as_ptr(), snapshot.text.as_ptr());
                }
                _ => panic!("clustered clones must retain clustered shared storage"),
            }
            assert_eq!(line, frozen);
            assert!(line.is_same_reflow_source(&frozen));
            mutate(&mut line);
            assert_eq!(
                line.seqno, frozen.seqno,
                "sequence number is deliberately unchanged"
            );
            assert_ne!(
                line, frozen,
                "exact validation must detect same-seqno edits"
            );
            assert!(!line.is_same_reflow_source(&frozen));
            assert_eq!(frozen.as_str(), "abc ");
            assert!(!frozen.last_cell_was_wrapped());
            assert_eq!(frozen, ordinary_clone);
        }
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn clustered_sharing_preserves_existing_wire_representation() {
        let mut line = Line::from_text("a界 ", &CellAttributes::blank(), 1, None);
        line.compress_for_scrollback();
        let payload = match &line.cells {
            CellStorage::C(payload) => payload,
            _ => panic!("expected clustered payload"),
        };
        // Arc serialization must remain the original enum payload, without
        // pointer identity, reference counts, or a new wrapper field.
        let expected = serde_json::json!({"C": payload.as_ref()});
        assert_eq!(serde_json::to_value(&line.cells).unwrap(), expected);
        let distinct = CellStorage::C(Arc::new(payload.as_ref().clone()));
        if let CellStorage::C(other) = &distinct {
            assert!(!Arc::ptr_eq(payload, other));
        }
        assert_eq!(
            line.cells, distinct,
            "distinct deep clones use exact equality"
        );
        let restored: CellStorage = serde_json::from_value(expected.clone()).unwrap();
        if let CellStorage::C(other) = &restored {
            assert!(!Arc::ptr_eq(payload, other));
            assert_eq!(payload.text, other.text);
            assert_eq!(payload.len(), other.len());
            assert_eq!(
                payload.last_cell_was_wrapped(),
                other.last_cell_was_wrapped()
            );
            let visible = |row: &ClusteredLine| {
                row.iter()
                    .map(|cell| (cell.cell_index(), cell.width(), cell.as_cell()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(visible(payload), visible(other));
        } else {
            panic!("wire round trip must retain clustered storage");
        }
        // The existing bitset wire representation preserves set indices,
        // not trailing zero capacity; do not confuse that with clone identity.
        assert_eq!(serde_json::to_value(&restored).unwrap(), expected);
        let frozen = line.clone();
        let original_wire = serde_json::to_value(&frozen).unwrap();
        line.set_last_cell_was_wrapped(true, 1);
        assert_eq!(serde_json::to_value(&frozen).unwrap(), original_wire);
        assert_ne!(serde_json::to_value(&line).unwrap(), original_wire);
    }

    #[cfg(feature = "use_image")]
    #[test]
    fn clustered_image_clones_share_cells_but_refuse_immutable_reflow_identity() {
        use frankenterm_cell::image::{ImageCell, ImageData, ImageDataType, TextureCoordinate};
        let image = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![1, 2, 3, 4],
        )));
        let mut attrs = CellAttributes::blank();
        attrs.set_image(alloc::boxed::Box::new(ImageCell::new(
            TextureCoordinate::new_f32(0.0, 0.0),
            TextureCoordinate::new_f32(1.0, 1.0),
            image,
        )));
        let mut line = Line::from_text("image", &attrs, 1, None);
        line.compress_for_scrollback();
        let frozen = line.clone();
        match (&line.cells, &frozen.cells) {
            (CellStorage::C(left), CellStorage::C(right)) => assert!(Arc::ptr_eq(left, right)),
            _ => panic!("expected clustered image storage"),
        }
        assert!(line.has_image_attachments());
        assert!(frozen.has_image_attachments());
        assert!(!line.is_same_reflow_source(&frozen));
        assert!(!line.is_same_reflow_content(&frozen));
        assert!(!frozen.is_same_reflow_content(&line));

        // Compare the cluster scan with the prior visible-cell oracle across
        // empty, image-free, and mixed attribute clusters, including wide text.
        let plain = CellAttributes::blank();
        for cells in [
            vec![],
            vec![Cell::new('x', plain.clone())],
            vec![Cell::new('界', attrs.clone())],
            vec![
                Cell::new('a', plain.clone()),
                Cell::new('b', attrs.clone()),
                Cell::new('c', plain.clone()),
            ],
        ] {
            let mut row = Line::from_cells(cells, 1);
            row.compress_for_scrollback();
            let expected = row
                .visible_cells()
                .any(|cell| cell.attrs().has_image_attachments());
            assert_eq!(row.has_image_attachments(), expected);
        }
    }

    #[test]
    fn bounded_snapshot_charges_clustered_text_before_clone() {
        let mut line = Line::from_text(&"x".repeat(8192), &CellAttributes::blank(), 1, None);
        line.compress_for_scrollback();
        assert!(matches!(line.cells, CellStorage::C(_)));
        let mut bytes = 128;
        let mut work = 100;
        assert!(line.try_clone_for_snapshot(&mut bytes, &mut work).is_none());
        assert_eq!(
            (bytes, work),
            (128, 100),
            "refusal allocates no snapshot and does not consume budget"
        );
        let mut bytes = 16384;
        let mut work = 100;
        let snapshot = line.try_clone_for_snapshot(&mut bytes, &mut work).unwrap();
        assert_eq!(snapshot, line);
        assert!(bytes <= 16384 - 8192);
    }

    #[test]
    fn bounded_snapshot_vector_cells_share_and_detach_on_edit() {
        let line = Line::from_cells(vec![Cell::new('x', CellAttributes::blank())], 1);
        let mut bytes = core::mem::size_of::<Line>();
        let mut work = 1;
        let mut snapshot = line.try_clone_for_snapshot(&mut bytes, &mut work).unwrap();
        assert_eq!((bytes, work), (0, 0));
        match (&line.cells, &snapshot.cells) {
            (CellStorage::V(a), CellStorage::V(b)) => assert!(a.shares_cells_with(b)),
            _ => panic!("expected vector storage"),
        }
        snapshot.set_last_cell_was_wrapped(true, 2);
        assert!(!line.last_cell_was_wrapped());
        assert!(snapshot.last_cell_was_wrapped());
    }

    #[test]
    fn clustered_tail_wrap_matches_visible_cells_without_scanning_text() {
        let mut empty = Line::new(1);
        empty.compress_for_scrollback();
        empty.set_last_cell_was_wrapped(false, 2);
        assert_eq!(empty.len(), 0);
        assert_eq!(empty.as_str(), "");
        for text in ["", "plain", "ab界", "e\u{301}🦀", &"x".repeat(131_072)] {
            let mut line = Line::from_text(text, &CellAttributes::blank(), 1, None);
            line.compress_for_scrollback();
            for wrapped in [false, true, false] {
                line.set_last_cell_was_wrapped(wrapped, 2);
                let expected = line
                    .visible_cells()
                    .last()
                    .is_some_and(|cell| cell.attrs().wrapped());
                assert_eq!(line.last_cell_was_wrapped(), expected);
            }
        }
    }

    #[test]
    fn bounded_snapshot_batch_preflights_both_slices_before_materializing() {
        let mut first = Line::from_text(&"a".repeat(2048), &CellAttributes::blank(), 1, None);
        let mut second = Line::from_text(&"b".repeat(4096), &CellAttributes::blank(), 2, None);
        first.compress_for_scrollback();
        second.compress_for_scrollback();
        assert!(matches!(first.cells, CellStorage::C(_)));
        assert!(matches!(second.cells, CellStorage::C(_)));
        let mut bytes = usize::MAX;
        let mut work = usize::MAX;
        first.charge_snapshot(&mut bytes, &mut work).unwrap();
        let first_cost = usize::MAX - bytes;
        second.charge_snapshot(&mut bytes, &mut work).unwrap();
        let byte_cost = usize::MAX - bytes;
        let work_cost = (usize::MAX - work) * 2;
        let first_slice = core::slice::from_ref(&first);
        let second_slice = core::slice::from_ref(&second);

        let mut bytes = byte_cost - 1;
        let mut work = work_cost;
        assert!(Line::try_clone_batch_for_snapshot(
            first_slice,
            second_slice,
            &mut bytes,
            &mut work,
        )
        .is_none());
        assert_eq!(
            bytes,
            byte_cost - 1 - first_cost,
            "no refund of admitted prefix"
        );

        let mut bytes = byte_cost;
        let mut work = work_cost - 1;
        assert!(
            Line::try_clone_batch_for_snapshot(first_slice, second_slice, &mut bytes, &mut work,)
                .is_none(),
            "materialization work is admitted before cloning"
        );

        let mut bytes = byte_cost;
        let mut work = work_cost;
        let snapshots =
            Line::try_clone_batch_for_snapshot(first_slice, second_slice, &mut bytes, &mut work)
                .unwrap();
        assert_eq!((bytes, work), (0, 0));
        assert_eq!(snapshots[0], first);
        assert_eq!(snapshots[1], second);
        assert_eq!(first.as_str(), "a".repeat(2048));
        assert_eq!(second.as_str(), "b".repeat(4096));
    }

    // ── ZoneRange ──────────────────────────────────────────

    #[test]
    fn zone_range_construction() {
        let zr = ZoneRange {
            semantic_type: SemanticType::Output,
            range: 0..10,
        };
        assert_eq!(zr.range, 0..10);
        assert_eq!(zr.semantic_type, SemanticType::Output);
    }

    #[test]
    fn zone_range_clone_eq() {
        let zr = ZoneRange {
            semantic_type: SemanticType::Input,
            range: 3..7,
        };
        let zr2 = zr.clone();
        assert_eq!(zr, zr2);
    }

    #[test]
    fn zone_range_debug() {
        let zr = ZoneRange {
            semantic_type: SemanticType::Output,
            range: 0..5,
        };
        let dbg = format!("{:?}", zr);
        assert!(dbg.contains("ZoneRange"));
    }

    // ── DoubleClickRange ───────────────────────────────────

    #[test]
    fn double_click_range_variants() {
        let r = DoubleClickRange::Range(0..5);
        let w = DoubleClickRange::RangeWithWrap(0..5);
        assert_ne!(r, w);
    }

    #[test]
    fn double_click_range_clone_eq() {
        let r = DoubleClickRange::Range(2..8);
        let r2 = r.clone();
        assert_eq!(r, r2);
    }

    #[cfg(feature = "appdata")]
    #[test]
    fn appdata_access_recovers_poisoned_lock() {
        let line = Line::with_width(1, SEQ_ZERO);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = line.appdata.lock().unwrap();
            panic!("poison appdata");
        }));

        let data = alloc::sync::Arc::new(42u32);
        line.set_appdata(alloc::sync::Arc::clone(&data));
        assert!(line.get_appdata().expect("stored appdata").is::<u32>());

        let cloned = line.clone();
        assert!(cloned.get_appdata().expect("cloned appdata").is::<u32>());

        line.clear_appdata();
        assert!(line.get_appdata().is_none());
    }

    #[cfg(feature = "appdata")]
    #[test]
    fn implicit_hyperlink_epoch_invalidation_clears_same_seqno_appdata() {
        let mut line: Line = "https://example.com".into();
        line.bits.insert(LineBits::SCANNED_IMPLICIT_HYPERLINKS);
        let original_seqno = line.current_seqno();
        let cached_shape = alloc::sync::Arc::new(line.compute_shape_hash());
        line.set_appdata(alloc::sync::Arc::clone(&cached_shape));
        assert!(line.get_appdata().is_some());

        line.invalidate_implicit_hyperlinks(original_seqno);

        assert_eq!(line.current_seqno(), original_seqno);
        assert!(
            line.get_appdata().is_none(),
            "same-seqno rule invalidation must not retain a pre-invalidation shape cache"
        );
    }

    #[cfg(feature = "appdata")]
    #[test]
    fn line_debug_omits_appdata_internals() {
        let line = Line::with_width(1, SEQ_ZERO);
        let debug = format!("{line:?}");
        assert!(!debug.contains("appdata"));
        assert!(!debug.contains("poisoned"));
    }

    // ── Line construction ──────────────────────────────────

    #[test]
    fn line_from_str() {
        let line: Line = "hello".into();
        assert_eq!(line.len(), 5);
        assert_eq!(line.as_str().as_ref(), "hello");
    }

    #[test]
    fn line_from_empty_str() {
        let line: Line = "".into();
        assert_eq!(line.len(), 0);
        assert_eq!(line.as_str().as_ref(), "");
    }

    #[test]
    fn line_with_width() {
        let line = Line::with_width(10, SEQ_ZERO);
        assert_eq!(line.len(), 10);
    }

    #[test]
    fn line_with_width_zero() {
        let line = Line::with_width(0, SEQ_ZERO);
        assert_eq!(line.len(), 0);
    }

    #[test]
    fn line_with_width_and_cell() {
        let cell = Cell::new('x', CellAttributes::default());
        let line = Line::with_width_and_cell(5, cell, SEQ_ZERO);
        assert_eq!(line.len(), 5);
        assert_eq!(line.as_str().as_ref(), "xxxxx");
    }

    #[test]
    fn line_from_cells() {
        let cells = vec![
            Cell::new('a', CellAttributes::default()),
            Cell::new('b', CellAttributes::default()),
        ];
        let line = Line::from_cells(cells, SEQ_ZERO);
        assert_eq!(line.len(), 2);
        assert_eq!(line.as_str().as_ref(), "ab");
    }

    #[test]
    fn line_new_starts_empty() {
        let line = Line::new(SEQ_ZERO);
        assert_eq!(line.len(), 0);
    }

    #[test]
    fn line_from_text() {
        let attrs = CellAttributes::default();
        let line = Line::from_text("abc", &attrs, 1, None);
        assert_eq!(line.len(), 3);
        assert_eq!(line.as_str().as_ref(), "abc");
        assert_eq!(line.current_seqno(), 1);
    }

    // ── Line seqno ─────────────────────────────────────────

    #[test]
    fn line_current_seqno() {
        let line = Line::with_width(5, 42);
        assert_eq!(line.current_seqno(), 42);
    }

    #[test]
    fn line_update_last_change_seqno_takes_max() {
        let mut line = Line::with_width(5, 10);
        line.update_last_change_seqno(5);
        // Should keep the higher seqno
        assert_eq!(line.current_seqno(), 10);
        line.update_last_change_seqno(20);
        assert_eq!(line.current_seqno(), 20);
    }

    #[test]
    fn line_changed_since() {
        let line = Line::with_width(5, 10);
        assert!(line.changed_since(5));
        assert!(!line.changed_since(10));
        assert!(!line.changed_since(15));
    }

    #[test]
    fn line_changed_since_seq_zero_always_true() {
        let line = Line::with_width(5, SEQ_ZERO);
        assert!(line.changed_since(0));
        assert!(line.changed_since(100));
    }

    // ── Line width/height flags ────────────────────────────

    #[test]
    fn line_default_is_single_width() {
        let line = Line::with_width(10, SEQ_ZERO);
        assert!(line.is_single_width());
        assert!(!line.is_double_width());
        assert!(!line.is_double_height_top());
        assert!(!line.is_double_height_bottom());
    }

    #[test]
    fn line_set_double_width() {
        let mut line = Line::with_width(10, SEQ_ZERO);
        line.set_double_width(1);
        assert!(line.is_double_width());
        assert!(!line.is_single_width());
    }

    #[test]
    fn line_set_double_height_top() {
        let mut line = Line::with_width(10, SEQ_ZERO);
        line.set_double_height_top(1);
        assert!(line.is_double_height_top());
        assert!(!line.is_single_width());
        assert!(!line.is_double_width());
    }

    #[test]
    fn line_set_double_height_bottom() {
        let mut line = Line::with_width(10, SEQ_ZERO);
        line.set_double_height_bottom(1);
        assert!(line.is_double_height_bottom());
        assert!(!line.is_single_width());
    }

    #[test]
    fn line_set_single_width_clears_double() {
        let mut line = Line::with_width(10, SEQ_ZERO);
        line.set_double_width(1);
        assert!(!line.is_single_width());
        line.set_single_width(2);
        assert!(line.is_single_width());
    }

    // ── Line bidi ──────────────────────────────────────────

    #[test]
    fn line_bidi_default() {
        let line = Line::with_width(5, SEQ_ZERO);
        let (enabled, hint) = line.bidi_info();
        assert!(!enabled);
        assert_eq!(hint, ParagraphDirectionHint::LeftToRight);
    }

    #[test]
    fn line_set_bidi_enabled() {
        let mut line = Line::with_width(5, SEQ_ZERO);
        line.set_bidi_enabled(true, 1);
        let (enabled, _) = line.bidi_info();
        assert!(enabled);
    }

    #[test]
    fn line_set_bidi_info_roundtrip() {
        let mut line = Line::with_width(5, SEQ_ZERO);
        for hint in [
            ParagraphDirectionHint::LeftToRight,
            ParagraphDirectionHint::RightToLeft,
            ParagraphDirectionHint::AutoLeftToRight,
            ParagraphDirectionHint::AutoRightToLeft,
        ] {
            line.set_bidi_info(true, hint, 1);
            let (enabled, got) = line.bidi_info();
            assert!(enabled);
            assert_eq!(got, hint, "roundtrip failed for {:?}", hint);
        }
    }

    #[test]
    fn line_set_direction_roundtrip() {
        for (direction, auto_detect, expected) in [
            (
                Direction::LeftToRight,
                false,
                ParagraphDirectionHint::LeftToRight,
            ),
            (
                Direction::RightToLeft,
                false,
                ParagraphDirectionHint::RightToLeft,
            ),
            (
                Direction::LeftToRight,
                true,
                ParagraphDirectionHint::AutoLeftToRight,
            ),
            (
                Direction::RightToLeft,
                true,
                ParagraphDirectionHint::AutoRightToLeft,
            ),
        ] {
            let mut line = Line::with_width(5, SEQ_ZERO);
            line.set_direction(direction, auto_detect, 1);
            let (enabled, got) = line.bidi_info();
            assert!(!enabled);
            assert_eq!(got, expected);
        }
    }

    // ── Line text operations ───────────────────────────────

    #[test]
    fn line_as_str() {
        let line: Line = "hello world".into();
        assert_eq!(line.as_str().as_ref(), "hello world");
    }

    #[test]
    fn line_columns_as_str() {
        let line: Line = "hello world".into();
        assert_eq!(line.columns_as_str(0..5), "hello");
        assert_eq!(line.columns_as_str(6..11), "world");
    }

    #[test]
    fn line_columns_as_str_empty_range() {
        let line: Line = "hello".into();
        assert_eq!(line.columns_as_str(2..2), "");
    }

    #[test]
    fn line_columns_as_line() {
        let line: Line = "abcdef".into();
        let sub = line.columns_as_line(1..4);
        assert_eq!(sub.as_str().as_ref(), "bcd");
    }

    #[test]
    fn line_columns_as_line_preserves_wide_and_combining_cells() {
        for vector_storage in [false, true] {
            let mut line = Line::new(SEQ_ZERO);
            line.append_line("A界e\u{301}💀Z".into(), SEQ_ZERO);
            if vector_storage {
                line.coerce_vec_storage();
            }
            assert_eq!(matches!(line.cells, CellStorage::V(_)), vector_storage);
            let whole = line.columns_as_line(0..line.len());
            assert_eq!(whole.as_str(), "A界e\u{301}💀Z");
            assert_eq!(whole.len(), line.len());
            let middle = line.columns_as_line(1..4);
            assert_eq!(middle.as_str(), "界e\u{301}");
            assert_eq!(middle.len(), 3);
            // Match columns_as_str: select a whole grapheme by its leading
            // column, never emit a partial wide glyph or its placeholder.
            assert_eq!(line.columns_as_line(1..2).as_str(), "界");
            assert_eq!(line.columns_as_line(2..4).as_str(), "e\u{301}");
        }
    }

    #[test]
    fn line_is_whitespace_true() {
        let line: Line = "     ".into();
        assert!(line.is_whitespace());
    }

    #[test]
    fn line_is_whitespace_false() {
        let line: Line = "  x  ".into();
        assert!(!line.is_whitespace());
    }

    #[test]
    fn line_is_whitespace_empty() {
        let line = Line::from_cells(vec![], SEQ_ZERO);
        assert!(line.is_whitespace());
    }

    // ── Line resize / mutate ───────────────────────────────

    #[test]
    fn line_resize_grow() {
        let mut line: Line = "hi".into();
        assert_eq!(line.len(), 2);
        line.resize(5, 1);
        assert_eq!(line.len(), 5);
    }

    #[test]
    fn line_resize_shrink() {
        let mut line: Line = "hello".into();
        line.resize(3, 1);
        assert_eq!(line.len(), 3);
        assert_eq!(line.as_str().as_ref(), "hel");
    }

    #[test]
    fn line_resize_and_clear() {
        let mut line: Line = "hello".into();
        line.resize_and_clear(3, 1, CellAttributes::default());
        assert_eq!(line.len(), 3);
        assert!(line.is_whitespace());
    }

    #[test]
    fn line_split_off() {
        let mut line: Line = "hello world".into();
        let remainder = line.split_off(5, 1);
        assert_eq!(line.as_str().as_ref(), "hello");
        assert_eq!(remainder.as_str().as_ref(), " world");
    }

    #[test]
    fn line_split_off_beyond_len() {
        let mut line: Line = "hi".into();
        let remainder = line.split_off(100, 1);
        assert_eq!(line.as_str().as_ref(), "hi");
        assert_eq!(remainder.len(), 0);
    }

    #[test]
    fn line_set_cell() {
        let mut line = Line::with_width(5, SEQ_ZERO);
        line.set_cell(0, Cell::new('A', CellAttributes::default()), 1);
        line.set_cell(1, Cell::new('B', CellAttributes::default()), 1);
        assert_eq!(line.columns_as_str(0..2), "AB");
    }

    #[test]
    fn line_set_cell_rejects_overflowing_index() {
        let mut line = Line::with_width(1, SEQ_ZERO);
        line.set_cell(usize::MAX, Cell::new('Z', CellAttributes::default()), 1);
        assert_eq!(line.len(), 1);
        assert_eq!(line.as_str().as_ref(), " ");

        let mut clustered = Line::new(SEQ_ZERO);
        clustered.set_cell_grapheme(usize::MAX, "Z", 1, CellAttributes::default(), 1);
        assert_eq!(clustered.len(), 0);
        assert_eq!(clustered.as_str().as_ref(), "");
    }

    #[test]
    fn line_erase_cell() {
        let mut line: Line = "abcde".into();
        line.erase_cell(2, 1);
        // After erasing index 2, cells shift left and a blank is appended
        assert_eq!(line.len(), 5);
        assert_eq!(line.columns_as_str(0..2), "ab");
        assert_eq!(line.columns_as_str(2..4), "de");
    }

    #[test]
    fn line_erase_cell_beyond_len() {
        let mut line: Line = "abc".into();
        // Should be a no-op
        line.erase_cell(10, 1);
        assert_eq!(line.as_str().as_ref(), "abc");
    }

    #[test]
    fn clustered_ascii_run_rejects_non_ascii_without_mutating_or_panicking() {
        let mut line: Line = "abc".into();
        line.compress_for_scrollback();
        let before = line.clone();

        assert!(!line.append_ascii_cell_run(line.len(), "é", CellAttributes::default(), 1,));
        assert_eq!(line, before);

        assert!(!line.append_ascii_cell_run(line.len(), "\r\n", CellAttributes::default(), 1,));
        assert_eq!(line, before, "a CRLF grapheme is not two width-1 cells");
    }

    #[test]
    fn clustered_ascii_run_preserves_prior_double_width_geometry() {
        let mut line: Line = "中".into();
        line.compress_for_scrollback();

        assert!(line.append_ascii_cell_run(line.len(), "abc", CellAttributes::default(), 1,));
        assert_eq!(line.as_str().as_ref(), "中abc");
        assert_eq!(line.len(), 5);
        assert_eq!(
            line.visible_cells()
                .map(|cell| (cell.cell_index(), cell.width(), cell.str().to_string()))
                .collect::<Vec<_>>(),
            vec![
                (0, 2, "中".to_string()),
                (2, 1, "a".to_string()),
                (3, 1, "b".to_string()),
                (4, 1, "c".to_string()),
            ],
        );
    }

    // ── Line wrap ──────────────────────────────────────────

    #[test]
    fn line_last_cell_was_wrapped_default_false() {
        let line: Line = "hello".into();
        assert!(!line.last_cell_was_wrapped());
    }

    #[test]
    fn line_set_last_cell_was_wrapped() {
        let mut line: Line = "hello".into();
        line.set_last_cell_was_wrapped(true, 1);
        assert!(line.last_cell_was_wrapped());
    }

    #[test]
    fn wide_cell_wrap_marker_survives_storage_conversion() {
        for text in ["a界", "e\u{0301}🚀", "界界"] {
            let mut line: Line = text.into();
            line.set_last_cell_was_wrapped(true, 1);
            assert!(
                line.last_cell_was_wrapped(),
                "vector storage lost {:?} wrap",
                text
            );
            line.compress_for_scrollback();
            assert!(
                line.last_cell_was_wrapped(),
                "compressed storage lost {:?} wrap",
                text
            );
            let _ = line.cells_mut();
            assert!(line.last_cell_was_wrapped());
            line.set_last_cell_was_wrapped(false, 2);
            assert!(!line.last_cell_was_wrapped());
            assert_eq!(line.as_str().as_ref(), text);

            let constructed =
                Line::from_text_with_wrapped_last_col(text, &CellAttributes::default(), 3);
            assert!(constructed.last_cell_was_wrapped());
        }
    }

    #[test]
    fn wide_cell_wrap_preserves_one_logical_line_across_width_changes() {
        let text = "界界界界界界界界";
        let mut logical: Line = text.into();
        for width in [4, 3, 7, 2, 5, 16, 4] {
            let rows = logical.wrap(width, 1);
            let last = rows.len() - 1;
            for (idx, row) in rows.iter().enumerate() {
                assert_eq!(
                    row.last_cell_was_wrapped(),
                    idx < last,
                    "width={width}, row={idx}: soft wrap became a hard newline"
                );
            }
            logical = Line::from_cells(Vec::new(), 1);
            for mut row in rows {
                row.set_last_cell_was_wrapped(false, 1);
                logical.append_line(row, 1);
            }
            assert_eq!(logical.as_str().as_ref(), text);
        }
    }

    #[test]
    fn line_wrap_single_line_fits() {
        let line: Line = "hi".into();
        let wrapped = line.wrap(10, 1);
        assert_eq!(wrapped.len(), 1);
        assert_eq!(wrapped[0].as_str().as_ref(), "hi");
    }

    #[test]
    fn line_wrap_splits_long_line() {
        let line: Line = "abcdef".into();
        let wrapped = line.wrap(3, 1);
        assert_eq!(wrapped.len(), 2);
        assert_eq!(wrapped[0].as_str().as_ref(), "abc");
        assert!(wrapped[0].last_cell_was_wrapped());
        assert_eq!(wrapped[1].as_str().as_ref(), "def");
    }

    #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
    fn wrap_report_signature(
        report: &LineWrapReport,
    ) -> (Vec<(String, bool, usize)>, LineWrapScorecard) {
        (
            report
                .lines
                .iter()
                .map(|line| {
                    (
                        line.as_str().into_owned(),
                        line.last_cell_was_wrapped(),
                        line.len(),
                    )
                })
                .collect(),
            report.scorecard,
        )
    }

    #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
    fn cached_wrap_entry_for_test(
        line: &Line,
        width: usize,
        cost_model: MonospaceKpCostModel,
        label: &str,
    ) -> MemoizedWrapPointCacheEntry {
        memoized_wrap_point_cache_entry_for_test(line, width, cost_model)
            .unwrap_or_else(|| panic!("missing memoized wrap entry after {}", label))
    }

    #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
    #[test]
    fn wrap_geometry_reuse_keeps_current_text_and_attributes() {
        let _guard = memoized_wrap_point_cache_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let model = MonospaceKpCostModel::terminal_default();
        let width = 4;
        let first = Line::from_text("alpha beta", &CellAttributes::default(), 1, None);
        let mut attrs = CellAttributes::default();
        attrs.set_italic(true);
        attrs.set_hyperlink(Some(alloc::sync::Arc::new(
            crate::hyperlink::Hyperlink::new_implicit("https://current.example/"),
        )));
        let current = Line::from_text("other text", &attrs, 2, None);
        memoized_wrap_point_cache_clear_for_test();
        let _ = first.wrap_with_report(width, 7, model);
        let hits_before = memoized_wrap_point_cache_key_hits_for_test(&current, width, model);
        let reused = current.clone().wrap_with_report(width, 7, model);
        assert!(
            memoized_wrap_point_cache_key_hits_for_test(&current, width, model) > hits_before,
            "equal widths and space boundaries must reuse geometry across text and attributes"
        );
        memoized_wrap_point_cache_clear_for_test();
        let fresh = current.wrap_with_report(width, 7, model);
        assert_eq!(
            reused.lines, fresh.lines,
            "cache must not retain the earlier cells"
        );
        assert_eq!(reused.scorecard, fresh.scorecard);
        assert_eq!(
            reused
                .lines
                .iter()
                .map(|line| line.as_str().into_owned())
                .collect::<String>(),
            "other text"
        );
        for line in reused.lines {
            for cell in line.visible_cells() {
                assert_eq!(
                    cell.attrs().hyperlink().unwrap().uri(),
                    "https://current.example/"
                );
            }
        }
    }

    #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
    #[test]
    fn memoized_wrap_points_match_fresh_recompute_after_hit_and_mutation_ft_3vdce() {
        let _cache_test_guard = memoized_wrap_point_cache_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let attrs = CellAttributes::default();
        let width = 5usize;
        let cost_model = MonospaceKpCostModel {
            max_dp_states: 0,
            ..MonospaceKpCostModel::terminal_default()
        };
        let mut line = Line::from_text("abcdefghijklmnopq", &attrs, SEQ_ZERO, None);

        memoized_wrap_point_cache_clear_for_test();
        let original_miss = line.clone().wrap_with_report(width, 1, cost_model);
        let original_miss_entry =
            cached_wrap_entry_for_test(&line, width, cost_model, "original miss");
        assert_eq!(
            memoized_wrap_point_cache_key_hits_for_test(&line, width, cost_model),
            0,
            "first wrap should populate the cache without a hit"
        );

        let hits_before_original_hit =
            memoized_wrap_point_cache_key_hits_for_test(&line, width, cost_model);
        let original_hit = line.clone().wrap_with_report(width, 2, cost_model);
        assert!(
            memoized_wrap_point_cache_key_hits_for_test(&line, width, cost_model)
                > hits_before_original_hit,
            "second wrap of unchanged content+width must hit the memoized entry"
        );
        let original_hit_entry =
            cached_wrap_entry_for_test(&line, width, cost_model, "original hit");

        memoized_wrap_point_cache_clear_for_test();
        let original_recomputed = line.clone().wrap_with_report(width, 3, cost_model);
        let original_recomputed_entry =
            cached_wrap_entry_for_test(&line, width, cost_model, "original recompute");
        assert_eq!(
            original_hit_entry.break_offsets, original_recomputed_entry.break_offsets,
            "cache hit wrap points must equal cleared-cache recompute wrap points"
        );
        assert_eq!(
            original_hit_entry.scorecard, original_recomputed_entry.scorecard,
            "cache hit scorecard must equal cleared-cache recompute scorecard"
        );
        assert_eq!(
            wrap_report_signature(&original_hit),
            wrap_report_signature(&original_recomputed),
            "cache hit materialization must equal cleared-cache recompute"
        );
        assert_eq!(
            wrap_report_signature(&original_miss),
            wrap_report_signature(&original_hit),
            "first miss and subsequent hit must materialize the same wrap"
        );
        assert_eq!(
            original_miss_entry.break_offsets, original_hit_entry.break_offsets,
            "initial miss and subsequent hit must expose identical wrap points"
        );

        memoized_wrap_point_cache_clear_for_test();
        let _ = line.clone().wrap_with_report(width, 4, cost_model);
        let original_cached_before_mutation =
            cached_wrap_entry_for_test(&line, width, cost_model, "pre-mutation original");

        line.set_cell_grapheme(0, "中", 2, attrs.clone(), 5);
        let mutated_hits_before_miss =
            memoized_wrap_point_cache_key_hits_for_test(&line, width, cost_model);
        let mutated_miss = line.clone().wrap_with_report(width, 6, cost_model);
        assert_eq!(
            memoized_wrap_point_cache_key_hits_for_test(&line, width, cost_model),
            mutated_hits_before_miss,
            "content mutation must not hit the stale cache entry for the old content"
        );
        let mutated_miss_entry =
            cached_wrap_entry_for_test(&line, width, cost_model, "mutated miss");
        assert_ne!(
            mutated_miss_entry.break_offsets, original_cached_before_mutation.break_offsets,
            "test fixture must change wrap points after mutation"
        );

        let hits_before_mutated_hit =
            memoized_wrap_point_cache_key_hits_for_test(&line, width, cost_model);
        let mutated_hit = line.clone().wrap_with_report(width, 7, cost_model);
        assert!(
            memoized_wrap_point_cache_key_hits_for_test(&line, width, cost_model)
                > hits_before_mutated_hit,
            "second wrap of mutated content+width must hit its new memoized entry"
        );
        let mutated_hit_entry = cached_wrap_entry_for_test(&line, width, cost_model, "mutated hit");

        memoized_wrap_point_cache_clear_for_test();
        let mutated_recomputed = line.clone().wrap_with_report(width, 8, cost_model);
        let mutated_recomputed_entry =
            cached_wrap_entry_for_test(&line, width, cost_model, "mutated recompute");
        assert_eq!(
            mutated_hit_entry.break_offsets, mutated_recomputed_entry.break_offsets,
            "mutated cache hit wrap points must equal cleared-cache recompute wrap points"
        );
        assert_eq!(
            mutated_hit_entry.scorecard, mutated_recomputed_entry.scorecard,
            "mutated cache hit scorecard must equal cleared-cache recompute scorecard"
        );
        assert_eq!(
            wrap_report_signature(&mutated_hit),
            wrap_report_signature(&mutated_recomputed),
            "mutated cache hit materialization must equal cleared-cache recompute"
        );
        assert_eq!(
            wrap_report_signature(&mutated_miss),
            wrap_report_signature(&mutated_hit),
            "mutated miss and subsequent hit must materialize the same wrap"
        );

        memoized_wrap_point_cache_clear_for_test();
    }

    #[test]
    fn kp_cost_model_badness_is_monotonic_for_non_last_lines() {
        let model = MonospaceKpCostModel::terminal_default();
        let width = 80usize;
        let mut prev = 0u64;
        for slack in 0..=width {
            let badness = model.line_badness(slack as i64, width, false);
            assert!(
                badness >= prev,
                "expected non-decreasing badness at slack={}: prev={} current={}",
                slack,
                prev,
                badness
            );
            prev = badness;
        }

        assert_eq!(model.line_badness(10, width, true), 0);
        assert_eq!(model.line_badness(-1, width, false), KP_BADNESS_INF);
    }

    #[test]
    fn kp_cost_model_enforces_bounded_state_budget() {
        let model = MonospaceKpCostModel::terminal_default();
        assert!(!model.should_fallback(32));
        assert!(!model.should_fallback(64));
        assert!(model.should_fallback(512));
        assert!(model.estimated_dp_states(512) > model.max_dp_states);
    }

    #[test]
    fn kp_transition_bound_matches_finite_endpoint_enumeration() {
        for token_count in 0..=200usize {
            for lookahead_limit in [0, 1, 2, 3, 63, 64, 65, 200, 201] {
                let model = MonospaceKpCostModel {
                    lookahead_limit,
                    ..MonospaceKpCostModel::terminal_default()
                };
                let enumerated: usize = (0..token_count)
                    .map(|start| (start + 1..=token_count).take(lookahead_limit).count())
                    .sum();
                assert_eq!(model.estimated_dp_states(token_count), enumerated);
            }
        }
        let mut model = MonospaceKpCostModel::terminal_default();
        assert_eq!(model.estimated_dp_states(158), 8096);
        assert!(!model.should_fallback(158));
        assert!(model.should_fallback(160));
        model.lookahead_limit = usize::MAX;
        assert_eq!(model.estimated_dp_states(usize::MAX), usize::MAX);
        model.lookahead_limit = 1;
        assert_eq!(model.estimated_dp_states(usize::MAX), usize::MAX);
    }

    #[test]
    fn kp_candidate_tiebreak_is_deterministic_on_fixed_corpora() {
        let corpus_a = vec![
            MonospaceBreakCandidate {
                total_cost: 120,
                forced_breaks: 1,
                max_line_badness: 80,
                line_count: 3,
                break_offsets: vec![10, 20, 29],
            },
            MonospaceBreakCandidate {
                total_cost: 120,
                forced_breaks: 1,
                max_line_badness: 80,
                line_count: 3,
                break_offsets: vec![10, 21, 29],
            },
            MonospaceBreakCandidate {
                total_cost: 120,
                forced_breaks: 2,
                max_line_badness: 60,
                line_count: 3,
                break_offsets: vec![11, 22, 29],
            },
        ];
        let expected_a = vec![10, 20, 29];

        let corpus_b = vec![
            MonospaceBreakCandidate {
                total_cost: 80,
                forced_breaks: 0,
                max_line_badness: 20,
                line_count: 4,
                break_offsets: vec![5, 10, 15, 20],
            },
            MonospaceBreakCandidate {
                total_cost: 80,
                forced_breaks: 0,
                max_line_badness: 20,
                line_count: 3,
                break_offsets: vec![7, 14, 20],
            },
            MonospaceBreakCandidate {
                total_cost: 81,
                forced_breaks: 0,
                max_line_badness: 10,
                line_count: 3,
                break_offsets: vec![8, 16, 20],
            },
        ];
        let expected_b = vec![7, 14, 20];

        let corpus_c = vec![
            MonospaceBreakCandidate {
                total_cost: 100,
                forced_breaks: 0,
                max_line_badness: 40,
                line_count: 3,
                break_offsets: vec![8, 16, 24],
            },
            MonospaceBreakCandidate {
                total_cost: 100,
                forced_breaks: 0,
                max_line_badness: 35,
                line_count: 3,
                break_offsets: vec![9, 18, 24],
            },
            MonospaceBreakCandidate {
                total_cost: 100,
                forced_breaks: 0,
                max_line_badness: 35,
                line_count: 3,
                break_offsets: vec![9, 19, 24],
            },
        ];
        let expected_c = vec![9, 18, 24];

        for rotation in 0..corpus_a.len() {
            let mut permuted = corpus_a.clone();
            permuted.rotate_left(rotation);
            let best = choose_best_monospace_break_candidate(&permuted).expect("candidate");
            assert_eq!(
                best.break_offsets, expected_a,
                "corpus_a rotation={rotation}"
            );
        }

        for rotation in 0..corpus_b.len() {
            let mut permuted = corpus_b.clone();
            permuted.rotate_left(rotation);
            let best = choose_best_monospace_break_candidate(&permuted).expect("candidate");
            assert_eq!(
                best.break_offsets, expected_b,
                "corpus_b rotation={rotation}"
            );
        }

        for rotation in 0..corpus_c.len() {
            let mut permuted = corpus_c.clone();
            permuted.rotate_left(rotation);
            let best = choose_best_monospace_break_candidate(&permuted).expect("candidate");
            assert_eq!(
                best.break_offsets, expected_c,
                "corpus_c rotation={rotation}"
            );
        }
    }

    fn cells_from_text(text: &str) -> Vec<Cell> {
        let line: Line = text.into();
        line.visible_cells().map(|cell| cell.as_cell()).collect()
    }

    #[test]
    fn bounded_wrap_plan_uses_dp_when_budget_allows() {
        let model = MonospaceKpCostModel::terminal_default();
        let tokens = cells_from_text("abcdefghij");
        let plan = bounded_monospace_wrap_plan(&tokens, 4, model);

        assert_eq!(plan.mode, MonospaceWrapMode::Dp);
        assert_eq!(plan.break_offsets.last(), Some(&tokens.len()));
        assert!(plan.evaluated_states > 0);
        assert!(plan.evaluated_states <= model.max_dp_states);
    }

    #[test]
    fn bounded_wrap_frozen_unicode_record_fits_default_budget() {
        let text = "FT_RECORD_00000 A  B\u{a0}C\u{2003}D e\u{301} 界面 🚀 0123456789 abcdefghijklmnopqrstuvwxyz 0123456789 abcdefghijklmnopqrstuvwxyz 0123456789 abcdefghijklmnopqrstuvwxyz FT_END_00000";
        let model = MonospaceKpCostModel::terminal_default();
        let tokens = cells_from_text(text);
        for width in [60, 70, 80, 86, 100, 120, 137] {
            let plan = bounded_monospace_wrap_plan(&tokens, width, model);
            assert_eq!(plan.mode, MonospaceWrapMode::Dp);
            assert!(plan.evaluated_states <= model.estimated_dp_states(tokens.len()));
            assert!(plan.evaluated_states <= model.max_dp_states);
            let line: Line = text.into();
            let geometry = LineWrapGeometry::capture(&line, 4096).unwrap();
            let report = line.wrap_with_report(width, SEQ_ZERO, model);
            let reconstructed: String = report
                .lines
                .iter()
                .map(|line| line.as_str().into_owned())
                .collect();
            assert_eq!(reconstructed, text);
            assert!(report.scorecard.selected_total_cost <= report.scorecard.greedy_total_cost);
            let mut scratch = LineWrapWidthPrefixScratch::default();
            let bound = geometry
                .planning_bytes_upper_bound(width, model, &scratch)
                .unwrap();
            assert_eq!(
                geometry.row_count_with_budget(width, model, &mut scratch, bound - 1),
                None
            );
            assert_eq!(scratch.capacity(), 0, "refusal must not allocate scratch");
            assert_eq!(
                geometry.row_count_with_budget(width, model, &mut scratch, bound),
                Some(report.lines.len())
            );
        }
    }

    #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
    #[test]
    fn frozen_unicode_history_reuses_geometry_without_reusing_record_text() {
        let _guard = memoized_wrap_point_cache_test_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        memoized_wrap_point_cache_clear_for_test();
        let model = MonospaceKpCostModel::terminal_default();
        for width in [60, 70, 80, 86, 100, 120, 137] {
            WRAP_PLANNER_CALLS.with(|calls| calls.set(0));
            for record in 0..10_000 {
                let text = format!(
                    "FT_RECORD_{record:05} A  B\u{a0}C\u{2003}D e\u{301} 界面 🚀 0123456789 abcdefghijklmnopqrstuvwxyz 0123456789 abcdefghijklmnopqrstuvwxyz 0123456789 abcdefghijklmnopqrstuvwxyz FT_END_{record:05}"
                );
                let line: Line = text.as_str().into();
                let report = line.wrap_with_report(width, SEQ_ZERO, model);
                assert_eq!(report.scorecard.mode, MonospaceWrapMode::Dp);
                let reconstructed: String = report
                    .lines
                    .iter()
                    .map(|line| line.as_str().into_owned())
                    .collect();
                assert_eq!(reconstructed, text);
            }
            // Another concurrent test may have populated this geometry first;
            // either way the 10k records require at most one planner call here.
            WRAP_PLANNER_CALLS.with(|calls| assert!(calls.get() <= 1));
        }
    }

    #[test]
    fn bounded_wrap_plan_keeps_full_rows_beyond_lookahead() {
        // The default 64-token lookahead cannot reach the first greedy
        // endpoint at 120. A bounded search must not replace that zero-cost
        // layout with two needlessly underfull rows.
        let line: Line = "x".repeat(121).as_str().into();
        let report = line.wrap_with_report(120, 19, MonospaceKpCostModel::terminal_default());
        assert_eq!(report.scorecard.greedy_total_cost, 0);
        assert_eq!(report.scorecard.selected_total_cost, 0);
        assert_eq!(
            report.lines.iter().map(Line::len).collect::<Vec<_>>(),
            [120, 1]
        );
        assert!(report.lines[0].last_cell_was_wrapped());
        assert!(!report.lines[1].last_cell_was_wrapped());
    }

    #[test]
    fn bounded_wrap_plan_never_loses_to_greedy() {
        let corpora = ["x".repeat(121), "界e\u{301}ab".repeat(25)];
        for text in corpora {
            let tokens = cells_from_text(&text);
            let mut widths = LineWrapWidthPrefixScratch::default();
            widths.rebuild(&tokens);
            for lookahead_limit in [1, 2, 4, 64] {
                let model = MonospaceKpCostModel {
                    lookahead_limit,
                    ..MonospaceKpCostModel::terminal_default()
                };
                for width in [0, 1, 2, 8, 63, 64, 65, 120, 128] {
                    let plan = bounded_monospace_wrap_plan(&tokens, width, model);
                    let selected = evaluate_break_offsets_with_width_prefix(
                        tokens.len(),
                        &plan.break_offsets,
                        width,
                        model,
                        &widths,
                    );
                    let greedy = evaluate_break_offsets_with_width_prefix(
                        tokens.len(),
                        &greedy_break_offsets_from_tokens(&tokens, width),
                        width,
                        model,
                        &widths,
                    );
                    assert_ne!(
                        compare_monospace_break_candidates(&selected, &greedy),
                        Ordering::Greater,
                        "width={width} lookahead={lookahead_limit} tokens={}",
                        tokens.len(),
                    );
                }
            }
        }
    }

    #[test]
    fn bounded_wrap_plan_falls_back_when_estimated_budget_exceeds_limit() {
        let mut model = MonospaceKpCostModel::terminal_default();
        model.max_dp_states = 8;
        let tokens = cells_from_text("abcdefghijklmnop");
        let plan = bounded_monospace_wrap_plan(&tokens, 4, model);

        assert_eq!(plan.mode, MonospaceWrapMode::Fallback);
        assert_eq!(plan.evaluated_states, 0);
        assert_eq!(
            plan.break_offsets,
            greedy_break_offsets_from_tokens(&tokens, 4)
        );
    }

    #[test]
    fn bounded_wrap_plan_accepts_maximum_lookahead_without_overflow() {
        let tokens = cells_from_text("a界bcdef");
        let full_span = MonospaceKpCostModel {
            lookahead_limit: tokens.len(),
            max_dp_states: usize::MAX,
            ..MonospaceKpCostModel::terminal_default()
        };
        let maximum = MonospaceKpCostModel {
            lookahead_limit: usize::MAX,
            ..full_span
        };
        assert_eq!(
            bounded_monospace_wrap_plan(&tokens, 3, maximum),
            bounded_monospace_wrap_plan(&tokens, 3, full_span),
        );
    }

    #[test]
    fn wrap_with_cost_model_fallback_matches_greedy_layout() {
        let line: Line = "abcdefghijkl".into();
        let tokens = cells_from_text("abcdefghijkl");
        let mut model = MonospaceKpCostModel::terminal_default();
        model.max_dp_states = 4;

        let expected = materialize_wrap_lines_from_tokens(
            &tokens,
            &greedy_break_offsets_from_tokens(&tokens, 3),
            1,
        );
        let (wrapped, mode) = line.wrap_with_cost_model(3, 1, model);

        assert_eq!(mode, MonospaceWrapMode::Fallback);
        assert_eq!(wrapped, expected);
    }

    #[test]
    fn wrap_with_cost_model_reports_dp_on_small_inputs() {
        let line: Line = "abcdef".into();
        let (wrapped, mode) =
            line.wrap_with_cost_model(3, 1, MonospaceKpCostModel::terminal_default());
        assert_eq!(mode, MonospaceWrapMode::Dp);
        assert_eq!(wrapped.len(), 2);
    }

    #[test]
    fn bounded_wrap_plan_is_deterministic_for_identical_inputs() {
        let model = MonospaceKpCostModel::terminal_default();
        let tokens = cells_from_text("deterministic");
        let a = bounded_monospace_wrap_plan(&tokens, 5, model);
        let b = bounded_monospace_wrap_plan(&tokens, 5, model);
        assert_eq!(a, b);
    }

    #[test]
    fn width_prefix_scratch_preserves_wrap_report_equivalence() {
        #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
        let _cache_test_guard = memoized_wrap_point_cache_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut tuned_model = MonospaceKpCostModel::terminal_default();
        tuned_model.badness_scale = 37_000;
        tuned_model.forced_break_penalty = 9_000;
        tuned_model.lookahead_limit = 6;

        let cases = [
            (
                "ascii_ties",
                "ab cd ef gh ij",
                5,
                MonospaceKpCostModel::terminal_default(),
            ),
            (
                "wide_cells",
                "ab表cd界ef",
                4,
                MonospaceKpCostModel::terminal_default(),
            ),
            (
                "overwide",
                "a表b",
                1,
                MonospaceKpCostModel::terminal_default(),
            ),
            ("tuned", "one two three four", 7, tuned_model),
        ];

        let mut scratch = LineWrapWidthPrefixScratch::default();
        for (name, text, width, model) in cases {
            let expected: Line = text.into();
            let expected = expected.wrap_with_report(width, 7, model);
            // `expected` may populate the process-wide wrap cache. Force the
            // first scratch-backed call down the computation path so this test
            // proves that the caller-owned prefix allocation is actually
            // established, rather than incorrectly requiring a cache hit to
            // allocate scratch it does not need.
            #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
            memoized_wrap_point_cache_clear_for_test();
            let actual: Line = text.into();
            let actual =
                actual.wrap_with_report_and_width_prefix_scratch(width, 7, model, &mut scratch);

            assert_eq!(actual, expected, "case {name}");
            assert!(
                scratch.capacity() >= text.chars().count().saturating_add(1),
                "case {} should retain prefix storage",
                name
            );
            let retained_capacity = scratch.capacity();

            // A subsequent memoized call must leave caller-owned storage
            // empty; a miss would rebuild the prefix and make this test fail.
            scratch.clear();
            let cached: Line = text.into();
            let cached =
                cached.wrap_with_report_and_width_prefix_scratch(width, 7, model, &mut scratch);
            assert_eq!(cached, expected, "cached case {}", name);
            #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
            assert!(
                scratch.widths.is_empty(),
                "cached case {} must be a hit",
                name
            );
            assert_eq!(
                scratch.capacity(),
                retained_capacity,
                "cached case {}",
                name
            );
        }

        #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
        memoized_wrap_point_cache_clear_for_test();
    }

    #[derive(Debug, Clone, Copy)]
    struct WrapQualityCorpusCase {
        id: &'static str,
        category: &'static str,
        text: &'static str,
        width: usize,
        note: &'static str,
        max_kp_badness_delta: i64,
        max_fallback_badness_delta: i64,
    }

    #[derive(Debug, Clone, Copy)]
    struct WrapQualityModeMetrics {
        mode: MonospaceWrapMode,
        selected_total_cost: u64,
        badness_delta: i64,
        forced_breaks: usize,
        line_count: usize,
    }

    #[derive(Debug, Clone, Copy)]
    struct WrapQualityCaseMetrics {
        id: &'static str,
        category: &'static str,
        width: usize,
        note: &'static str,
        greedy: WrapQualityModeMetrics,
        kp: WrapQualityModeMetrics,
        fallback: WrapQualityModeMetrics,
        max_kp_badness_delta: i64,
        max_fallback_badness_delta: i64,
    }

    #[derive(Debug, Clone, Copy)]
    struct WrapQualityAggregateMetrics {
        sample_count: usize,
        kp_fallback_lines: usize,
        fallback_fallback_lines: usize,
        kp_fallback_ratio_percent: usize,
        fallback_fallback_ratio_percent: usize,
        kp_total_badness_delta: i64,
        fallback_total_badness_delta: i64,
        kp_max_badness_delta: i64,
        fallback_max_badness_delta: i64,
    }

    const WRAP_QUALITY_CORPUS: &[WrapQualityCorpusCase] = &[
        WrapQualityCorpusCase {
            id: "code_render_gate",
            category: "code",
            text: "fn render_wrap_scorecard_gate(payload: &str) -> anyhow::Result<()> {",
            width: 28,
            note: "Keep function signature chunks readable under narrow widths.",
            max_kp_badness_delta: 20_000,
            max_fallback_badness_delta: 120_000,
        },
        WrapQualityCorpusCase {
            id: "log_resize_gate",
            category: "logs",
            text: "2026-02-14T02:45:33Z WARN resize_wrap_scorecard_gate fallback_ratio_exceeded pane=17",
            width: 36,
            note: "Preserve timestamp + level cohesion while wrapping long diagnostics.",
            max_kp_badness_delta: 15_000,
            max_fallback_badness_delta: 80_000,
        },
        WrapQualityCorpusCase {
            id: "prose_operator_guidance",
            category: "prose",
            text: "Readable wrapping should keep adjacent clauses together when terminal width fluctuates quickly.",
            width: 32,
            note: "Avoid ragged prose with avoidable high-slack lines.",
            max_kp_badness_delta: 10_000,
            max_fallback_badness_delta: 70_000,
        },
        WrapQualityCorpusCase {
            id: "long_token_checksum",
            category: "long_token",
            text: "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            width: 24,
            note: "Long opaque identifiers should not explode badness unexpectedly.",
            max_kp_badness_delta: 60_000,
            max_fallback_badness_delta: 150_000,
        },
        WrapQualityCorpusCase {
            id: "unicode_mixed_text",
            category: "unicode",
            text: "日本語とemoji🙂を含むログ出力でも改行品質を保つ必要がある",
            width: 18,
            note: "Unicode-heavy samples should remain stable with bounded DP.",
            max_kp_badness_delta: 25_000,
            max_fallback_badness_delta: 120_000,
        },
    ];

    fn greedy_only_model() -> MonospaceKpCostModel {
        let mut model = MonospaceKpCostModel::terminal_default();
        model.max_dp_states = 0;
        model
    }

    fn constrained_fallback_model() -> MonospaceKpCostModel {
        let mut model = MonospaceKpCostModel::terminal_default();
        model.max_dp_states = 8;
        model.lookahead_limit = 3;
        model
    }

    fn measure_wrap_mode(
        line: &Line,
        width: usize,
        model: MonospaceKpCostModel,
    ) -> WrapQualityModeMetrics {
        let report = line.clone().wrap_with_report(width, SEQ_ZERO, model);
        WrapQualityModeMetrics {
            mode: report.scorecard.mode,
            selected_total_cost: report.scorecard.selected_total_cost,
            badness_delta: report.scorecard.badness_delta,
            forced_breaks: report.scorecard.selected_forced_breaks,
            line_count: report.scorecard.line_count,
        }
    }

    fn evaluate_wrap_quality_corpus() -> (Vec<WrapQualityCaseMetrics>, WrapQualityAggregateMetrics)
    {
        let mut metrics = Vec::with_capacity(WRAP_QUALITY_CORPUS.len());
        let mut kp_fallback_lines = 0usize;
        let mut fallback_fallback_lines = 0usize;
        let mut kp_total_badness_delta = 0i64;
        let mut fallback_total_badness_delta = 0i64;
        let mut kp_max_badness_delta = i64::MIN;
        let mut fallback_max_badness_delta = i64::MIN;

        for sample in WRAP_QUALITY_CORPUS {
            let line: Line = sample.text.into();
            let greedy = measure_wrap_mode(&line, sample.width, greedy_only_model());
            let kp = measure_wrap_mode(
                &line,
                sample.width,
                MonospaceKpCostModel::terminal_default(),
            );
            let fallback = measure_wrap_mode(&line, sample.width, constrained_fallback_model());

            if matches!(kp.mode, MonospaceWrapMode::Fallback) {
                kp_fallback_lines = kp_fallback_lines.saturating_add(1);
            }
            if matches!(fallback.mode, MonospaceWrapMode::Fallback) {
                fallback_fallback_lines = fallback_fallback_lines.saturating_add(1);
            }

            kp_total_badness_delta = kp_total_badness_delta.saturating_add(kp.badness_delta);
            fallback_total_badness_delta =
                fallback_total_badness_delta.saturating_add(fallback.badness_delta);
            kp_max_badness_delta = kp_max_badness_delta.max(kp.badness_delta);
            fallback_max_badness_delta = fallback_max_badness_delta.max(fallback.badness_delta);

            metrics.push(WrapQualityCaseMetrics {
                id: sample.id,
                category: sample.category,
                width: sample.width,
                note: sample.note,
                greedy,
                kp,
                fallback,
                max_kp_badness_delta: sample.max_kp_badness_delta,
                max_fallback_badness_delta: sample.max_fallback_badness_delta,
            });
        }

        let sample_count = metrics.len();
        let kp_fallback_ratio_percent = kp_fallback_lines
            .saturating_mul(100)
            .checked_div(sample_count)
            .unwrap_or(0);
        let fallback_fallback_ratio_percent = fallback_fallback_lines
            .saturating_mul(100)
            .checked_div(sample_count)
            .unwrap_or(0);

        (
            metrics,
            WrapQualityAggregateMetrics {
                sample_count,
                kp_fallback_lines,
                fallback_fallback_lines,
                kp_fallback_ratio_percent,
                fallback_fallback_ratio_percent,
                kp_total_badness_delta,
                fallback_total_badness_delta,
                kp_max_badness_delta: if kp_max_badness_delta == i64::MIN {
                    0
                } else {
                    kp_max_badness_delta
                },
                fallback_max_badness_delta: if fallback_max_badness_delta == i64::MIN {
                    0
                } else {
                    fallback_max_badness_delta
                },
            },
        )
    }

    fn wrap_mode_str(mode: MonospaceWrapMode) -> &'static str {
        match mode {
            MonospaceWrapMode::Dp => "dp",
            MonospaceWrapMode::Fallback => "fallback",
        }
    }

    fn json_escape(raw: &str) -> String {
        raw.replace('\\', "\\\\")
            .replace('\"', "\\\"")
            .replace('\n', "\\n")
    }

    fn render_wrap_quality_metrics_json(
        samples: &[WrapQualityCaseMetrics],
        aggregate: WrapQualityAggregateMetrics,
    ) -> String {
        let mut rendered_samples = Vec::with_capacity(samples.len());
        for sample in samples {
            rendered_samples.push(format!(
                "{{\"id\":\"{}\",\"category\":\"{}\",\"width\":{},\"note\":\"{}\",\"greedy\":{{\"mode\":\"{}\",\"selected_total_cost\":{},\"badness_delta\":{},\"forced_breaks\":{},\"line_count\":{}}},\"kp\":{{\"mode\":\"{}\",\"selected_total_cost\":{},\"badness_delta\":{},\"forced_breaks\":{},\"line_count\":{}}},\"fallback\":{{\"mode\":\"{}\",\"selected_total_cost\":{},\"badness_delta\":{},\"forced_breaks\":{},\"line_count\":{}}}}}",
                json_escape(sample.id),
                json_escape(sample.category),
                sample.width,
                json_escape(sample.note),
                wrap_mode_str(sample.greedy.mode),
                sample.greedy.selected_total_cost,
                sample.greedy.badness_delta,
                sample.greedy.forced_breaks,
                sample.greedy.line_count,
                wrap_mode_str(sample.kp.mode),
                sample.kp.selected_total_cost,
                sample.kp.badness_delta,
                sample.kp.forced_breaks,
                sample.kp.line_count,
                wrap_mode_str(sample.fallback.mode),
                sample.fallback.selected_total_cost,
                sample.fallback.badness_delta,
                sample.fallback.forced_breaks,
                sample.fallback.line_count
            ));
        }
        format!(
            "{{\"samples\":[{}],\"aggregate\":{{\"sample_count\":{},\"kp_fallback_lines\":{},\"fallback_fallback_lines\":{},\"kp_fallback_ratio_percent\":{},\"fallback_fallback_ratio_percent\":{},\"kp_total_badness_delta\":{},\"fallback_total_badness_delta\":{},\"kp_max_badness_delta\":{},\"fallback_max_badness_delta\":{}}}}}",
            rendered_samples.join(","),
            aggregate.sample_count,
            aggregate.kp_fallback_lines,
            aggregate.fallback_fallback_lines,
            aggregate.kp_fallback_ratio_percent,
            aggregate.fallback_fallback_ratio_percent,
            aggregate.kp_total_badness_delta,
            aggregate.fallback_total_badness_delta,
            aggregate.kp_max_badness_delta,
            aggregate.fallback_max_badness_delta
        )
    }

    #[test]
    fn wrap_quality_corpus_covers_required_categories() {
        let categories: BTreeSet<_> = WRAP_QUALITY_CORPUS
            .iter()
            .map(|sample| sample.category)
            .collect();
        assert!(categories.contains("code"));
        assert!(categories.contains("logs"));
        assert!(categories.contains("prose"));
        assert!(categories.contains("long_token"));
        assert!(categories.contains("unicode"));
    }

    #[test]
    fn wrap_quality_scorecard_outputs_machine_readable_payload() {
        let (samples, aggregate) = evaluate_wrap_quality_corpus();
        let payload = render_wrap_quality_metrics_json(&samples, aggregate);
        assert!(
            payload.starts_with("{\"samples\":["),
            "expected machine-readable payload envelope, got: {}",
            payload
        );
        assert!(
            payload.contains("\"aggregate\":"),
            "missing aggregate metrics section: {}",
            payload
        );
        for sample in &samples {
            assert!(
                payload.contains(&format!("\"id\":\"{}\"", sample.id)),
                "missing sample id {} in payload: {payload}",
                sample.id
            );
        }
    }

    #[test]
    fn wrap_quality_regression_gate_bounds_kp_and_fallback_deltas() {
        let (samples, aggregate) = evaluate_wrap_quality_corpus();
        for sample in &samples {
            assert_eq!(
                sample.greedy.badness_delta, 0,
                "greedy baseline should have zero delta for {}",
                sample.id
            );
            assert!(
                sample.kp.badness_delta <= sample.max_kp_badness_delta,
                "kp badness delta exceeded gate for {}: {} > {}",
                sample.id,
                sample.kp.badness_delta,
                sample.max_kp_badness_delta
            );
            assert!(
                sample.fallback.badness_delta <= sample.max_fallback_badness_delta,
                "fallback badness delta exceeded gate for {}: {} > {}",
                sample.id,
                sample.fallback.badness_delta,
                sample.max_fallback_badness_delta
            );
            assert!(
                sample.kp.selected_total_cost
                    <= sample.fallback.selected_total_cost.saturating_add(120_000),
                "kp selected cost unexpectedly regressed vs fallback for {}",
                sample.id
            );
        }

        assert!(
            aggregate.kp_fallback_ratio_percent <= 40,
            "kp fallback ratio exceeded corpus gate: {}%",
            aggregate.kp_fallback_ratio_percent
        );
        assert!(
            aggregate.fallback_fallback_ratio_percent >= aggregate.kp_fallback_ratio_percent,
            "constrained fallback should not fallback less often than kp (fallback={}%, kp={}%)",
            aggregate.fallback_fallback_ratio_percent,
            aggregate.kp_fallback_ratio_percent
        );
        assert!(
            aggregate.kp_total_badness_delta
                <= aggregate
                    .fallback_total_badness_delta
                    .saturating_add(50_000),
            "kp aggregate badness drifted beyond fallback tolerance (kp={}, fallback={})",
            aggregate.kp_total_badness_delta,
            aggregate.fallback_total_badness_delta
        );
    }

    #[test]
    fn materialized_wrap_marks_non_terminal_lines_as_wrapped() {
        let tokens = cells_from_text("abcdef");
        let lines = materialize_wrap_lines_from_tokens(&tokens, &[3, 6], 1);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].last_cell_was_wrapped());
        assert!(!lines[1].last_cell_was_wrapped());
    }

    #[test]
    fn planned_wrap_materializes_only_requested_rows_with_global_wrap_markers() {
        for text in [
            "abcdef ghijkl mnopqr",
            "界面 e\u{301} 🚀 ffi אבג ".repeat(8).as_str(),
        ] {
            for width in [1, 3, 8, 17] {
                let mut source = Line::from_text(text, &CellAttributes::default(), 4, None);
                let layout = source.clone().plan_wrap_with_width_prefix_scratch(
                    width,
                    MonospaceKpCostModel::terminal_default(),
                    &mut LineWrapWidthPrefixScratch::default(),
                );
                let expected = materialize_wrap_lines_from_tokens(
                    &layout.tokens[layout.token_range.clone()],
                    &layout.break_offsets,
                    9,
                );
                assert_eq!(layout.row_count(), expected.len());
                assert_eq!(layout.scorecard().line_count, expected.len());
                // The plan owns the original text even after its source changes.
                source.set_cell(0, Cell::new('X', CellAttributes::default()), 10);
                for start in 0..=expected.len() {
                    let end = start.saturating_add(2).min(expected.len());
                    assert_eq!(layout.materialize_rows(start..end, 9), expected[start..end]);
                }
                assert_eq!(layout.materialize_rows(0..usize::MAX, 9), expected);
                assert!(layout
                    .materialize_rows(usize::MAX..usize::MAX, 9)
                    .is_empty());
                let start = 2;
                assert!(layout.materialize_rows(start..1, 9).is_empty());
            }
        }
    }

    #[test]
    fn planned_blank_wrap_preserves_passthrough_line_metadata() {
        for text in ["", "   "] {
            let source = Line::from_text(text, &CellAttributes::default(), 41, None);
            let layout = source.clone().plan_wrap_with_width_prefix_scratch(
                1,
                MonospaceKpCostModel::terminal_default(),
                &mut LineWrapWidthPrefixScratch::default(),
            );
            assert_eq!(layout.row_count(), 1);
            assert_eq!(layout.materialize_rows(0..1, 99), vec![source.clone()]);
            assert!(layout.materialize_rows(1..2, 99).is_empty());
            assert_eq!(layout.into_report(99).lines, vec![source]);
        }
    }

    #[test]
    fn replanning_widths_reuses_logical_cells_and_preserves_quality() {
        let source = Line::from_text(
            &"界面 e\u{301} 🚀 ffi אבג ".repeat(20),
            &CellAttributes::default(),
            4,
            None,
        );
        let mut scratch = LineWrapWidthPrefixScratch::default();
        let seed = source.clone().plan_wrap_with_width_prefix_scratch(
            10,
            MonospaceKpCostModel::terminal_default(),
            &mut scratch,
        );
        for width in [1, 17, 3, 31, 8, 17] {
            let layout = seed.replan(
                width,
                MonospaceKpCostModel::terminal_default(),
                &mut scratch,
            );
            assert!(Arc::ptr_eq(&seed.tokens, &layout.tokens));
            let fresh =
                source
                    .clone()
                    .wrap_with_report(width, 9, MonospaceKpCostModel::terminal_default());
            assert_eq!(layout.scorecard(), fresh.scorecard);
            assert_eq!(
                layout.materialize_rows(0..layout.row_count(), 9),
                fresh.lines
            );
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn trimmed_logical_ranges_reuse_tokens_without_retained_width_plans() {
        for text in [
            "ascii ffi -> long logical content   ",
            "界面 e\u{301} 🚀 ffi אבג trailing text   ",
        ] {
            let source = Line::from_text(text, &CellAttributes::default(), 4, None);
            // Include unrelated wide cells on both sides. All planner offsets
            // must refer to the active logical slice, not the allocation.
            let mut cells = vec![Cell::new('前', CellAttributes::default())];
            cells.extend(source.visible_cells().map(|cell| cell.as_cell()));
            let source_end = cells.len();
            cells.push(Cell::new('後', CellAttributes::default()));
            let tokens: Arc<[Cell]> = cells.into();
            let mut shared = source.clone();
            shared.cells = CellStorage::V(VecStorage::from_token_range(
                Arc::clone(&tokens),
                1..source_end,
                false,
            ));
            for width in [0, 1, 3, 7, 17, 31, 5] {
                // Deliberately reconstruct the plan each time. Replan alone
                // hides the copying bug for sources outside the token budget.
                let layout = shared.clone().plan_wrap_with_width_prefix_scratch(
                    width,
                    MonospaceKpCostModel::terminal_default(),
                    &mut LineWrapWidthPrefixScratch::default(),
                );
                assert!(Arc::ptr_eq(&tokens, &layout.tokens));
                assert_eq!(layout.token_range, 1..source_end - 3);
                assert!(layout.width_prefix.is_none());
                let expected = source.clone().wrap_with_report(
                    width,
                    9,
                    MonospaceKpCostModel::terminal_default(),
                );
                assert_eq!(layout.scorecard(), expected.scorecard);
                assert_eq!(layout.materialize_rows(0..usize::MAX, 9), expected.lines);
                assert_deferred_metadata_scan_contract(&layout, true);
                let deferred = layout.deferred_rows(0..usize::MAX, 9);
                for (row, expected) in deferred.iter().zip(&expected.lines) {
                    assert_eq!(row.len(), expected.len());
                    assert_eq!(row.compute_shape_hash(), expected.compute_shape_hash());
                    assert_eq!(
                        row.last_cell_was_wrapped(),
                        expected.last_cell_was_wrapped()
                    );
                    let CellStorage::V(storage) = &row.cells else {
                        panic!("not vector storage");
                    };
                    if std::env::var_os("FT_DISABLE_DEFERRED_REFLOW_CELLS").is_none() {
                        assert!(storage.is_deferred_unmaterialized());
                    }
                }
                assert_eq!(deferred, expected.lines);
                let retained = layout.retain_width_prefix();
                let expected_prefix = source
                    .clone()
                    .plan_wrap_with_width_prefix_scratch(
                        width,
                        MonospaceKpCostModel::terminal_default(),
                        &mut LineWrapWidthPrefixScratch::default(),
                    )
                    .retain_width_prefix();
                assert_eq!(retained.width_prefix, expected_prefix.width_prefix);
                let next_width = width + 2;
                let replanned = retained.replan(
                    next_width,
                    MonospaceKpCostModel::terminal_default(),
                    &mut LineWrapWidthPrefixScratch::default(),
                );
                let next_expected = source.clone().wrap_with_report(
                    next_width,
                    9,
                    MonospaceKpCostModel::terminal_default(),
                );
                assert!(Arc::ptr_eq(&tokens, &replanned.tokens));
                assert_eq!(replanned.scorecard(), next_expected.scorecard);
                assert_eq!(
                    replanned.materialize_rows(0..usize::MAX, 9),
                    next_expected.lines,
                );
            }
        }
    }

    #[cfg(feature = "std")]
    fn assert_deferred_metadata_scan_contract(layout: &LineWrapLayout, fast: bool) {
        use crate::line::vecstorage::DEFERRED_METADATA_TOKEN_VISITS;
        let deferred_enabled = std::env::var_os("FT_DISABLE_DEFERRED_REFLOW_CELLS")
            != Some(std::ffi::OsString::from("1"));
        let visits = || DEFERRED_METADATA_TOKEN_VISITS.with(|count| count.get());
        let before = visits();
        let actual = layout.deferred_rows(0..usize::MAX, 9);
        assert_eq!(
            visits() - before,
            if fast || !deferred_enabled {
                0
            } else {
                layout.token_range.len()
            },
            "count the actual constructor token visits"
        );
        let mut control = layout.clone();
        control.image_free = false;
        let before = visits();
        let fallback = control.deferred_rows(0..usize::MAX, 9);
        assert_eq!(
            visits() - before,
            if deferred_enabled {
                layout.token_range.len()
            } else {
                0
            },
            "suppressing the source witness must restore the original scan"
        );
        let eager = layout.materialize_rows(0..usize::MAX, 9);
        for (row, expected) in actual.iter().zip(&eager) {
            assert_eq!(row.len(), expected.len());
            assert_eq!(row.current_seqno(), expected.current_seqno());
            assert_eq!(
                row.last_cell_was_wrapped(),
                expected.last_cell_was_wrapped()
            );
            assert_eq!(row.compute_shape_hash(), expected.compute_shape_hash());
            assert_eq!(
                row.visible_cells()
                    .map(|cell| cell.cell_index())
                    .collect::<Vec<_>>(),
                expected
                    .visible_cells()
                    .map(|cell| cell.cell_index())
                    .collect::<Vec<_>>()
            );
            #[cfg(feature = "use_image")]
            assert_eq!(
                row.has_image_attachments(),
                expected.has_image_attachments()
            );
            if deferred_enabled {
                let CellStorage::V(storage) = &row.cells else {
                    panic!("expected deferred vector storage");
                };
                assert!(storage.is_deferred_unmaterialized());
            }
        }
        let mut changed = actual.clone();
        changed[0].set_cell(0, Cell::new('Z', CellAttributes::blank()), 10);
        let mut changed_eager = eager.clone();
        changed_eager[0].set_cell(0, Cell::new('Z', CellAttributes::blank()), 10);
        assert_eq!(changed, changed_eager);
        assert_eq!(actual, eager);
        assert_eq!(actual, fallback);
        #[cfg(feature = "use_serde")]
        {
            let wire = serde_json::to_value(&actual).unwrap();
            assert_eq!(wire, serde_json::to_value(&eager).unwrap());
            let restored: Vec<Line> = serde_json::from_value(wire).unwrap();
            assert_eq!(restored, eager);
        }
        assert_eq!(actual, layout.materialize_rows(0..usize::MAX, 9));
    }

    #[cfg(all(feature = "std", not(ft_disable_memoized_wrap_points)))]
    #[test]
    fn deferred_metadata_uses_current_endpoints_on_geometry_cache_hits() {
        let _guard = memoized_wrap_point_cache_test_lock().lock().unwrap();
        let model = MonospaceKpCostModel::terminal_default();
        let mut scratch = LineWrapWidthPrefixScratch::default();
        let plain = Line::from_text("ab界 cd界 ef界 gh界", &CellAttributes::blank(), 1, None);
        let mut attrs = CellAttributes::blank();
        attrs.set_italic(true);
        let source = Line::from_text("xy面 zw面 uv面 rs面", &attrs, 2, None);
        for clustered in [false, true] {
            let mut current = source.clone();
            if clustered {
                current.compress_for_scrollback();
            }
            let _ = plain
                .clone()
                .plan_wrap_with_width_prefix_scratch(7, model, &mut scratch);
            // The content-key diagnostic deliberately disables cross-content
            // geometry hits. Prime the current source there as well.
            if std::env::var_os("FT_DISABLE_WRAP_GEOMETRY_KEY")
                == Some(std::ffi::OsString::from("1"))
            {
                let _ = current
                    .clone()
                    .plan_wrap_with_width_prefix_scratch(7, model, &mut scratch);
            }
            let before = memoized_wrap_point_cache_key_hits_for_test(&current, 7, model);
            scratch.widths = vec![0, 999];
            scratch.all_widths_positive = true;
            let layout =
                current
                    .clone()
                    .plan_wrap_with_width_prefix_scratch(7, model, &mut scratch);
            assert!(memoized_wrap_point_cache_key_hits_for_test(&current, 7, model) > before);
            assert_eq!(
                scratch.widths,
                vec![0, 999],
                "the cache hit did not refresh scratch"
            );
            assert!(layout.width_prefix.is_none());
            assert!(layout.row_widths.is_some());
            assert_deferred_metadata_scan_contract(&layout, true);
            let retained = layout.retain_width_prefix();
            assert!(
                retained.row_widths.is_none(),
                "retention must discard transient row widths"
            );
            let prefix = retained.width_prefix.as_ref().unwrap();
            assert!(prefix.all_widths_positive);
            for width in [3, 7, 11] {
                scratch.widths = vec![0, 12345];
                let next = retained.replan(width, model, &mut scratch);
                assert!(Arc::ptr_eq(prefix, next.width_prefix.as_ref().unwrap()));
                assert!(next.row_widths.is_none());
                assert_deferred_metadata_scan_contract(&next, true);
                assert_eq!(
                    next.materialize_rows(0..usize::MAX, 9),
                    current.clone().wrap(width, 9)
                );
            }
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn deferred_metadata_zero_width_sources_keep_retained_scan_fallback() {
        let zero = Cell::new_grapheme(
            "\u{301}\u{301}\u{301}\u{301}",
            CellAttributes::blank(),
            None,
        );
        assert_eq!(
            zero.width(),
            0,
            "explicit width zero would normalize to one"
        );
        let source = Line::from_cells(
            vec![
                zero,
                Cell::new('界', CellAttributes::blank()),
                Cell::new(' ', CellAttributes::blank()),
                Cell::new('a', CellAttributes::blank()),
            ],
            1,
        );
        let mut scratch = LineWrapWidthPrefixScratch::default();
        let layout = source.plan_wrap_with_width_prefix_scratch(
            3,
            MonospaceKpCostModel::terminal_default(),
            &mut scratch,
        );
        assert!(layout.image_free);
        assert_eq!(layout.tokens[layout.token_range.start].width(), 0);
        assert_deferred_metadata_scan_contract(&layout, true);
        let retained = layout.retain_width_prefix();
        assert!(!retained.width_prefix.as_ref().unwrap().all_widths_positive);
        assert!(retained.row_widths.is_none());
        assert_deferred_metadata_scan_contract(&retained, false);
        for width in [1, 2, 5] {
            let next = retained.replan(
                width,
                MonospaceKpCostModel::terminal_default(),
                &mut scratch,
            );
            assert_deferred_metadata_scan_contract(&next, false);
        }
    }

    #[cfg(all(feature = "std", feature = "use_image"))]
    #[test]
    fn deferred_metadata_image_witness_is_independent_of_geometry_cache() {
        use frankenterm_cell::image::{ImageCell, ImageData, ImageDataType, TextureCoordinate};
        let image = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![1, 2, 3, 4],
        )));
        let mut attrs = CellAttributes::blank();
        attrs.set_image(Box::new(ImageCell::new(
            TextureCoordinate::new_f32(0.0, 0.0),
            TextureCoordinate::new_f32(1.0, 1.0),
            Arc::clone(&image),
        )));
        let mut scratch = LineWrapWidthPrefixScratch::default();
        let model = MonospaceKpCostModel::terminal_default();
        let plain = Line::from_text("ab界 cd界", &CellAttributes::blank(), 1, None);
        let _ = plain.plan_wrap_with_width_prefix_scratch(3, model, &mut scratch);
        let source = Line::from_text("xy面 zw面", &attrs, 1, None);
        let layout = source.plan_wrap_with_width_prefix_scratch(3, model, &mut scratch);
        assert!(!layout.image_free);
        assert!(layout.row_widths.is_none());
        assert_deferred_metadata_scan_contract(&layout, false);
        let retained = layout.retain_width_prefix();
        assert_deferred_metadata_scan_contract(&retained, false);
        {
            let mut payload = image.data_mut();
            let ImageDataType::Rgba8 { data, .. } = &mut *payload else {
                panic!("expected RGBA payload");
            };
            data[0] = 99;
        }
        assert_deferred_metadata_scan_contract(&retained.replan(5, model, &mut scratch), false);
    }

    #[cfg(feature = "std")]
    #[test]
    fn deferred_metadata_rejects_invalid_endpoint_arithmetic() {
        let source = Line::from_text("ab", &CellAttributes::blank(), 1, None);
        let layout = source.clone().plan_wrap_with_width_prefix_scratch(
            2,
            MonospaceKpCostModel::terminal_default(),
            &mut LineWrapWidthPrefixScratch::default(),
        );
        assert_eq!(layout.break_offsets, vec![2]);
        let cell = Cell::new('a', CellAttributes::blank());
        for indices in [[0, usize::MAX], [3, 0]] {
            let cells: Vec<_> = indices
                .iter()
                .map(|&cell_index| CellRef::CellRef {
                    cell_index,
                    cell: &cell,
                })
                .collect();
            assert!(layout.row_widths_from_cells(&cells).is_none());
        }
        assert!(layout.row_widths_from_cells(&[]).is_none());
    }

    #[cfg(feature = "std")]
    #[test]
    fn all_whitespace_shared_range_preserves_passthrough_metadata() {
        let tokens: Arc<[Cell]> = cells_from_text("前   後").into();
        let mut source = Line::new(41);
        source.cells = CellStorage::V(VecStorage::from_token_range(tokens, 1..4, false));
        source.bits = LineBits::DOUBLE_WIDTH | LineBits::BIDI_ENABLED;
        for width in [0, 1, 2, 8] {
            let layout = source.clone().plan_wrap_with_width_prefix_scratch(
                width,
                MonospaceKpCostModel::terminal_default(),
                &mut LineWrapWidthPrefixScratch::default(),
            );
            assert_eq!(layout.row_count(), 1);
            assert!(layout.tokens.is_empty());
            assert_eq!(layout.token_range, 0..0);
            assert_eq!(
                layout.materialize_rows(0..usize::MAX, 99),
                vec![source.clone()]
            );
            assert_eq!(
                layout.deferred_rows(0..usize::MAX, 99),
                vec![source.clone()]
            );
        }
    }

    #[test]
    fn retained_width_prefix_ignores_stale_scratch_and_survives_replanning() {
        for text in ["ascii text", "界面 e\u{301} 🚀 ffi אבג"] {
            let source = Line::from_text(text, &CellAttributes::default(), 4, None);
            let model = MonospaceKpCostModel::terminal_default();
            let mut scratch = LineWrapWidthPrefixScratch::default();
            // Prime memoization, then leave unrelated geometry in scratch.
            let _ = source
                .clone()
                .plan_wrap_with_width_prefix_scratch(7, model, &mut scratch);
            scratch.widths = vec![0, 99, 100];
            let seed = source
                .clone()
                .plan_wrap_with_width_prefix_scratch(7, model, &mut scratch)
                .retain_width_prefix();
            let retained = seed.width_prefix.as_ref().unwrap().clone();
            let again = seed.clone().retain_width_prefix();
            assert!(Arc::ptr_eq(&retained, again.width_prefix.as_ref().unwrap()));
            for width in [0, 1, 2, 3, 5, 7, 11, 19, 23] {
                scratch.widths = vec![0, 999];
                let layout = seed.replan(width, model, &mut scratch);
                assert_eq!(scratch.widths, vec![0, 999]);
                assert!(Arc::ptr_eq(
                    &retained,
                    layout.width_prefix.as_ref().unwrap()
                ));
                let fresh = source.clone().wrap_with_report(width, 9, model);
                assert_eq!(layout.scorecard(), fresh.scorecard);
                assert_eq!(
                    layout.materialize_rows(0..layout.row_count(), 9),
                    fresh.lines
                );
            }
        }
    }

    #[test]
    fn blank_wrap_source_does_not_retain_width_storage() {
        let source = Line::from_text("   ", &CellAttributes::default(), 4, None);
        let mut scratch = LineWrapWidthPrefixScratch::default();
        let layout = source
            .clone()
            .plan_wrap_with_width_prefix_scratch(
                1,
                MonospaceKpCostModel::terminal_default(),
                &mut scratch,
            )
            .retain_width_prefix();
        assert!(layout.width_prefix.is_none());
        assert_eq!(layout.materialize_rows(0..1, 9), vec![source]);
    }

    #[cfg(feature = "std")]
    #[test]
    fn deferred_reflow_geometry_reads_do_not_materialize_offscreen_cells() {
        for text in ["plain ascii text", "界面 e\u{301} 🚀 ffi אבג"] {
            let source = Line::from_text(text, &CellAttributes::default(), 4, None);
            for width in [1, 2, 3, 7, 19] {
                let layout = source.clone().plan_wrap_with_width_prefix_scratch(
                    width,
                    MonospaceKpCostModel::terminal_default(),
                    &mut LineWrapWidthPrefixScratch::default(),
                );
                let deferred = layout.deferred_rows(0..usize::MAX, 9);
                let eager = layout.materialize_rows(0..usize::MAX, 9);
                assert_eq!(deferred.len(), eager.len());
                for (row, expected) in deferred.iter().zip(&eager) {
                    assert_eq!(row.len(), expected.len());
                    assert_eq!(
                        row.last_cell_was_wrapped(),
                        expected.last_cell_was_wrapped()
                    );
                    assert_eq!(row.compute_shape_hash(), expected.compute_shape_hash());
                    assert_eq!(row.as_str(), expected.as_str());
                    #[cfg(feature = "use_image")]
                    assert_eq!(
                        row.has_image_attachments(),
                        expected.has_image_attachments()
                    );
                    let CellStorage::V(storage) = &row.cells else {
                        panic!("not vector storage")
                    };
                    if std::env::var_os("FT_DISABLE_DEFERRED_REFLOW_CELLS").is_none() {
                        assert!(storage.is_deferred_unmaterialized());
                    }
                }
                assert_eq!(deferred, eager);
            }
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn deferred_logical_join_preserves_cells_and_refuses_changed_sources() {
        if std::env::var_os("FT_DISABLE_DEFERRED_REFLOW_CELLS").is_some()
            || std::env::var_os("FT_DISABLE_DEFERRED_LOGICAL_JOIN").is_some()
        {
            return;
        }
        let source = Line::from_text(
            "界面 e\u{301} 🚀 ffi אבג long logical content",
            &CellAttributes::default(),
            4,
            None,
        );
        let layout = source.plan_wrap_with_width_prefix_scratch(
            5,
            MonospaceKpCostModel::terminal_default(),
            &mut LineWrapWidthPrefixScratch::default(),
        );
        let rows = layout.deferred_rows(0..usize::MAX, 9);
        let joined = Line::try_join_deferred_logical_rows(rows.iter(), 10).unwrap();
        for row in rows.iter().chain(core::iter::once(&joined)) {
            let CellStorage::V(storage) = &row.cells else {
                panic!("not vector storage")
            };
            assert!(storage.is_deferred_unmaterialized());
        }
        let replanned = joined.clone().plan_wrap_with_width_prefix_scratch(
            7,
            MonospaceKpCostModel::terminal_default(),
            &mut LineWrapWidthPrefixScratch::default(),
        );
        assert!(Arc::ptr_eq(&layout.tokens, &replanned.tokens));
        let mut expected: Option<Line> = None;
        for mut row in layout.materialize_rows(0..usize::MAX, 9) {
            row.set_last_cell_was_wrapped(false, 10);
            match expected.as_mut() {
                Some(line) => line.append_line(row, 10),
                None => expected = Some(row),
            }
        }
        assert_eq!(joined, expected.unwrap());
        let mut changed = rows.clone();
        changed[1].cells_mut()[0] = Cell::new('X', CellAttributes::default());
        assert!(Line::try_join_deferred_logical_rows(changed.iter(), 10).is_none());
        assert!(Line::try_join_deferred_logical_rows(
            rows.iter().take(1).chain(rows.iter().skip(2)),
            10,
        )
        .is_none());
    }

    #[cfg(feature = "std")]
    #[test]
    fn compact_physical_logical_rows_match_eager_reconstruction_and_share_tokens() {
        if std::env::var_os("FT_DISABLE_COMPACT_LOGICAL_ROWS").is_some() {
            return;
        }
        for text in [
            "ascii ffi -> long logical content",
            "界面 e\u{301} 🚀 אבג repeated text",
        ] {
            for width in [1, 5, 80] {
                let source = Line::from_text(text, &CellAttributes::default(), 4, None);
                let layout = source.plan_wrap_with_width_prefix_scratch(
                    width,
                    MonospaceKpCostModel::terminal_default(),
                    &mut LineWrapWidthPrefixScratch::default(),
                );
                let physical = layout.materialize_rows(0..usize::MAX, 9);
                let compact = Line::try_compact_logical_rows(physical.iter(), 10).unwrap();
                let CellStorage::V(storage) = &compact.cells else {
                    panic!("not vector storage");
                };
                assert!(storage.is_deferred_unmaterialized());
                let (tokens, _) = storage.reusable_wrap_tokens().unwrap();
                let mut eager: Option<Line> = None;
                for mut row in physical.clone() {
                    if row.last_cell_was_wrapped() {
                        row.set_last_cell_was_wrapped(false, 10);
                    }
                    match &mut eager {
                        Some(line) => line.append_line(row, 10),
                        None => {
                            row.update_last_change_seqno(10);
                            eager = Some(row);
                        }
                    }
                }
                let eager = eager.unwrap();
                assert_eq!(compact, eager);
                let replanned = compact.plan_wrap_with_width_prefix_scratch(
                    7,
                    MonospaceKpCostModel::terminal_default(),
                    &mut LineWrapWidthPrefixScratch::default(),
                );
                assert!(Arc::ptr_eq(&tokens, &replanned.tokens));
                let expected = eager.plan_wrap_with_width_prefix_scratch(
                    7,
                    MonospaceKpCostModel::terminal_default(),
                    &mut LineWrapWidthPrefixScratch::default(),
                );
                assert_eq!(
                    replanned.materialize_rows(0..usize::MAX, 11),
                    expected.materialize_rows(0..usize::MAX, 11)
                );
            }
        }
        let rows = [
            Line::from_text("one", &CellAttributes::default(), 1, None),
            Line::from_text("two", &CellAttributes::default(), 1, None),
        ];
        assert!(Line::try_compact_logical_rows(rows.iter(), 2).is_none());
        assert!(Line::try_compact_logical_rows(core::iter::empty(), 2).is_none());
    }

    // ── Line clone / eq ────────────────────────────────────

    #[test]
    fn line_clone_equals_original() {
        let line: Line = "hello".into();
        let cloned = line.clone();
        assert_eq!(line, cloned);
    }

    #[test]
    fn line_ne_different_content() {
        let a: Line = "hello".into();
        let b: Line = "world".into();
        assert_ne!(a, b);
    }

    // ── Line compress / changes ────────────────────────────

    #[test]
    fn line_compress_for_scrollback_roundtrip() {
        let line: Line = "test data".into();
        let mut compressed = line.clone();
        compressed.compress_for_scrollback();
        compressed.coerce_vec_storage();
        assert_eq!(line, compressed);
    }

    #[test]
    fn line_has_hyperlink_default_false() {
        let line: Line = "hello".into();
        assert!(!line.has_hyperlink());
    }

    #[test]
    fn line_changes_simple() {
        let line: Line = "abc".into();
        let changes = line.changes(&CellAttributes::default());
        // Should produce at least a Text change
        assert!(!changes.is_empty());
        match &changes[0] {
            Change::Text(t) => assert_eq!(t, "abc"),
            _ => panic!("expected Text change"),
        }
    }

    #[test]
    fn line_get_cell() {
        let line: Line = "hello".into();
        let cell = line.get_cell(0).unwrap();
        assert_eq!(cell.str(), "h");
        let cell = line.get_cell(4).unwrap();
        assert_eq!(cell.str(), "o");
    }

    #[test]
    fn line_get_cell_out_of_bounds() {
        let line: Line = "hi".into();
        assert!(line.get_cell(10).is_none());
    }

    #[test]
    fn line_visible_cells_count() {
        let line: Line = "test".into();
        assert_eq!(line.visible_cells().count(), 4);
    }

    #[test]
    fn line_prune_trailing_blanks() {
        let mut line = Line::with_width(10, SEQ_ZERO);
        line.set_cell(0, Cell::new('a', CellAttributes::default()), 1);
        line.set_cell(1, Cell::new('b', CellAttributes::default()), 1);
        // Cells 2..9 are blanks
        line.prune_trailing_blanks(2);
        assert_eq!(line.len(), 2);
        assert_eq!(line.as_str().as_ref(), "ab");
    }

    #[test]
    fn line_prune_trailing_blanks_clears_all_default_blanks() {
        let mut line = Line::with_width(10, SEQ_ZERO);
        line.prune_trailing_blanks(1);
        assert_eq!(line.len(), 0);
        assert_eq!(line.as_str().as_ref(), "");
        assert_eq!(line.current_seqno(), 1);
    }

    #[test]
    fn line_prune_trailing_blanks_preserves_non_default_blank_cells() {
        let mut attrs = CellAttributes::default();
        attrs.set_semantic_type(SemanticType::Prompt);
        let mut line = Line::with_width_and_cell(3, Cell::blank_with_attrs(attrs), SEQ_ZERO);

        line.prune_trailing_blanks(1);

        assert_eq!(line.len(), 3);
        assert_eq!(line.as_str().as_ref(), "   ");
        assert_eq!(line.current_seqno(), SEQ_ZERO);
    }

    #[test]
    fn line_prune_trailing_blanks_preserves_wide_grapheme_placeholder() {
        let mut line = Line::with_width(2, SEQ_ZERO);
        line.set_cell(
            0,
            Cell::new_grapheme_with_width("\u{4e2d}", 2, CellAttributes::default()),
            1,
        );
        assert_eq!(line.len(), 2);

        line.prune_trailing_blanks(2);

        assert_eq!(line.len(), 2);
        assert_eq!(line.as_str().as_ref(), "\u{4e2d}");
        assert_eq!(line.current_seqno(), 1, "retaining geometry is a no-op");
    }

    #[test]
    fn line_fill_range() {
        let mut line = Line::with_width(5, SEQ_ZERO);
        let cell = Cell::new('X', CellAttributes::default());
        line.fill_range(1..4, &cell, 1);
        assert_eq!(line.columns_as_str(1..4), "XXX");
    }

    #[test]
    fn line_fill_range_preserves_sequential_wide_cell_assignment() {
        let mut line = Line::with_width(4, SEQ_ZERO);
        let wide = Cell::new_grapheme_with_width("\u{4e2d}", 2, CellAttributes::default());

        line.fill_range(0..3, &wide, 1);

        assert_eq!(line.len(), 4);
        assert_eq!(line.columns_as_str(0..4), "  \u{4e2d}");
        assert_eq!(
            line.visible_cells()
                .map(|cell| (cell.cell_index(), cell.width(), cell.str().to_string()))
                .collect::<Vec<_>>(),
            vec![
                (0, 1, " ".to_string()),
                (1, 1, " ".to_string()),
                (2, 2, "\u{4e2d}".to_string()),
            ],
        );
    }

    #[test]
    fn line_fill_range_wide_cell_returns_promptly_at_or_beyond_materialized_cap() {
        let wide = Cell::new_grapheme_with_width("\u{4e2d}", 2, CellAttributes::default());
        let narrow = Cell::new('X', CellAttributes::default());
        let mut line = Line::with_width(4, SEQ_ZERO);
        let before = line.clone();

        // A range that begins at zero but exceeds the physical-row contract
        // must fail before allocating or iterating its pointer-width tail.
        line.fill_range(0..usize::MAX, &narrow, 1);
        assert_eq!(line, before);

        // The last possible single-column start cannot fit a two-column cell;
        // it must be rejected before iteration or trailing-blank pruning.
        line.fill_range(MAX_MATERIALIZED_LINE_LEN - 1..usize::MAX, &wide, 2);
        assert_eq!(line, before);

        // Starts at the exclusive line-length cap must not iterate at all.
        line.fill_range(MAX_MATERIALIZED_LINE_LEN..usize::MAX, &wide, 3);
        assert_eq!(line, before);
    }

    #[test]
    fn line_overlay_text() {
        let mut line = Line::with_width(10, SEQ_ZERO);
        line.overlay_text_with_attribute(2, "hi", CellAttributes::default(), 1);
        assert_eq!(line.columns_as_str(2..4), "hi");
    }

    // ── Line double-click ──────────────────────────────────

    #[test]
    fn line_double_click_range_word() {
        let line: Line = "hello world".into();
        let r = line.compute_double_click_range(2, |s| s.chars().all(|c| c.is_alphanumeric()));
        assert_eq!(r, DoubleClickRange::Range(0..5));
    }

    #[test]
    fn line_double_click_range_at_space() {
        let line: Line = "hello world".into();
        let r = line.compute_double_click_range(5, |s| s.chars().all(|c| c.is_alphanumeric()));
        assert_eq!(r, DoubleClickRange::Range(5..5));
    }

    #[test]
    fn line_double_click_range_inside_wide_word_cell_uses_owner() {
        let line = Line::from_text("中a ", &CellAttributes::default(), SEQ_ZERO, None);
        let first = line.visible_cells().next().expect("wide first cell");
        assert_eq!(first.width(), 2);

        let r = line.compute_double_click_range(1, |s| s != " ");
        assert_eq!(r, DoubleClickRange::Range(0..3));
    }

    #[test]
    fn line_double_click_range_wide_only_word_covers_full_cell_width() {
        let line = Line::from_text("中 ", &CellAttributes::default(), SEQ_ZERO, None);
        let first = line.visible_cells().next().expect("wide first cell");
        assert_eq!(first.width(), 2);

        let r = line.compute_double_click_range(1, |s| s != " ");
        assert_eq!(r, DoubleClickRange::Range(0..2));
    }

    #[test]
    fn line_double_click_range_inside_wide_non_word_cell_stays_empty() {
        // Use an unconditionally wide non-word character. Emoji presentation
        // width depends on the configured Unicode/variation-selector policy.
        let line = Line::from_text("。a", &CellAttributes::default(), SEQ_ZERO, None);
        let first = line.visible_cells().next().expect("wide first cell");
        assert_eq!(first.width(), 2);

        let r = line.compute_double_click_range(1, |s| s.chars().all(|c| c.is_alphanumeric()));
        assert_eq!(r, DoubleClickRange::Range(1..1));
    }

    #[test]
    fn line_insert_cell() {
        let mut line: Line = "abde".into();
        line.insert_cell(2, Cell::new('c', CellAttributes::default()), 5, 1);
        assert_eq!(line.columns_as_str(0..5), "abcde");
    }

    #[test]
    fn line_zero_right_margin_edit_is_noop() {
        let mut insert_line: Line = "abc".into();
        insert_line.insert_cell(1, Cell::new('X', CellAttributes::default()), 0, 1);
        assert_eq!(insert_line.as_str().as_ref(), "abc");

        let mut erase_line: Line = "abc".into();
        erase_line.erase_cell_with_margin(1, 0, 1, CellAttributes::blank());
        assert_eq!(erase_line.as_str().as_ref(), "abc");
    }

    #[test]
    fn line_remove_cell() {
        let mut line: Line = "abcde".into();
        line.remove_cell(2, 1);
        assert_eq!(line.len(), 4);
        assert_eq!(line.as_str().as_ref(), "abde");
    }

    #[test]
    fn line_remove_cell_beyond_len() {
        let mut line: Line = "ab".into();
        line.remove_cell(10, 1);
        assert_eq!(line.as_str().as_ref(), "ab");
    }

    #[test]
    fn line_semantic_zone_ranges() {
        let mut line: Line = "hello".into();
        let zones = line.semantic_zone_ranges();
        assert_eq!(
            zones,
            &[ZoneRange {
                semantic_type: SemanticType::Output,
                range: 0..5,
            }]
        );
    }

    #[test]
    fn line_semantic_zone_ranges_use_exclusive_end_for_single_cell_zone() {
        let mut input = CellAttributes::default();
        input.set_semantic_type(SemanticType::Input);

        let mut line = Line::from_cells(vec![Cell::new('x', input)], SEQ_ZERO);
        let zones = line.semantic_zone_ranges();

        assert_eq!(
            zones,
            &[ZoneRange {
                semantic_type: SemanticType::Input,
                range: 0..1,
            }]
        );
    }

    #[test]
    fn line_semantic_zone_ranges_preserve_mixed_run_bounds() {
        let mut prompt = CellAttributes::default();
        prompt.set_semantic_type(SemanticType::Prompt);
        let mut input = CellAttributes::default();
        input.set_semantic_type(SemanticType::Input);
        let output = CellAttributes::default();

        let mut line = Line::from_cells(
            vec![
                Cell::new('p', prompt.clone()),
                Cell::new('s', prompt),
                Cell::new('i', input),
                Cell::new('o', output),
            ],
            SEQ_ZERO,
        );
        let zones = line.semantic_zone_ranges();

        assert_eq!(
            zones,
            &[
                ZoneRange {
                    semantic_type: SemanticType::Prompt,
                    range: 0..2,
                },
                ZoneRange {
                    semantic_type: SemanticType::Input,
                    range: 2..3,
                },
                ZoneRange {
                    semantic_type: SemanticType::Output,
                    range: 3..4,
                },
            ]
        );
    }

    #[test]
    fn line_compute_shape_hash_differs() {
        let a: Line = "hello".into();
        let b: Line = "world".into();
        assert_ne!(a.compute_shape_hash(), b.compute_shape_hash());
    }

    #[test]
    fn line_compute_shape_hash_same() {
        let a: Line = "hello".into();
        let b: Line = "hello".into();
        assert_eq!(a.compute_shape_hash(), b.compute_shape_hash());
    }

    #[test]
    fn shape_hash_matches_fresh_content_after_same_sequence_mutations() {
        type Mutation = (&'static str, fn(&mut Line));
        let mutations: &[Mutation] = &[
            ("set", |l| {
                l.set_cell(0, Cell::new('X', CellAttributes::default()), 1)
            }),
            ("insert", |l| l.insert_cell(1, Cell::blank(), 20, 1)),
            ("erase", |l| l.erase_cell(1, 1)),
            ("remove", |l| l.remove_cell(0, 1)),
            ("fill", |l| l.fill_range(0..3, &Cell::blank(), 1)),
            ("resize", |l| l.resize(30, 1)),
            ("mutable cells", |l| l.cells_mut()[0] = Cell::blank()),
            ("attributes", |l| {
                l.cells_mut_for_attr_changes_only()[0]
                    .attrs_mut()
                    .set_hyperlink(Some(alloc::sync::Arc::new(
                        crate::hyperlink::Hyperlink::new("https://example.invalid"),
                    )));
            }),
            ("wrap", |l| l.set_last_cell_was_wrapped(true, 1)),
            ("bidi", |l| {
                l.set_bidi_info(true, ParagraphDirectionHint::RightToLeft, 1)
            }),
            ("double width", |l| l.set_double_width(1)),
            ("split", |l| {
                let _ = l.split_off(4, 1);
            }),
            ("append", |l| l.append_line("tail".into(), 1)),
            ("compression", |l| l.compress_for_scrollback()),
        ];
        for (name, mutate) in mutations {
            let mut line =
                Line::from_text("abc界e\u{0301}🚀אב", &CellAttributes::default(), 1, None);
            let original_hash = line.compute_shape_hash();
            let original_boundary = line.last_cell_was_wrapped();
            assert_eq!(original_hash, line.compute_shape_hash_uncached());
            let frozen = line.clone();
            mutate(&mut line);
            assert_eq!(
                line.last_cell_was_wrapped(),
                line.visible_cells()
                    .last()
                    .is_some_and(|cell| cell.attrs().wrapped()),
                "wrap boundary after {}",
                name
            );
            assert_eq!(
                frozen.last_cell_was_wrapped(),
                original_boundary,
                "snapshot wrap boundary after {}",
                name
            );
            assert_eq!(line.current_seqno(), 1);
            assert_eq!(
                line.compute_shape_hash(),
                line.compute_shape_hash_uncached(),
                "{}",
                name
            );
            assert_eq!(
                frozen.compute_shape_hash(),
                original_hash,
                "snapshot: {}",
                name
            );
            let clone = line.clone();
            assert_eq!(
                clone.compute_shape_hash(),
                clone.compute_shape_hash_uncached(),
                "clone: {}",
                name
            );
        }
    }

    #[test]
    fn line_from_text_with_wrapped_last_col() {
        let line =
            Line::from_text_with_wrapped_last_col("abc", &CellAttributes::default(), SEQ_ZERO);
        assert!(line.last_cell_was_wrapped());
        assert_eq!(line.as_str().as_ref(), "abc");
    }

    #[test]
    fn line_append_line() {
        let mut line1: Line = "hello".into();
        let line2: Line = " world".into();
        line1.append_line(line2, 1);
        assert_eq!(line1.as_str().as_ref(), "hello world");
    }
}
