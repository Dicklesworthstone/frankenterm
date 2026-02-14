use crate::cellcluster::CellCluster;
use crate::hyperlink::Rule;
use crate::line::cellref::CellRef;
use crate::line::clusterline::ClusteredLine;
use crate::line::linebits::LineBits;
use crate::line::storage::{CellStorage, VisibleCellIter};
use crate::line::vecstorage::{VecStorage, VecStorageIter};
use crate::{Change, SequenceNo, SEQ_ZERO};
use alloc::borrow::Cow;
#[cfg(feature = "appdata")]
use alloc::sync::{Arc, Weak};
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
use std::sync::Mutex;

extern crate alloc;
use crate::alloc::string::ToString;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneRange {
    pub semantic_type: SemanticType,
    pub range: Range<u16>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DoubleClickRange {
    Range(Range<usize>),
    RangeWithWrap(Range<usize>),
}

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug)]
pub struct Line {
    pub(crate) cells: CellStorage,
    zones: Vec<ZoneRange>,
    seqno: SequenceNo,
    bits: LineBits,
    #[cfg(feature = "appdata")]
    #[cfg_attr(feature = "use_serde", serde(skip))]
    appdata: Mutex<Option<Weak<dyn Any + Send + Sync>>>,
}

impl Clone for Line {
    fn clone(&self) -> Self {
        Self {
            cells: self.cells.clone(),
            zones: self.zones.clone(),
            seqno: self.seqno,
            bits: self.bits,
            #[cfg(feature = "appdata")]
            appdata: Mutex::new(self.appdata.lock().unwrap().clone()),
        }
    }
}

impl PartialEq for Line {
    fn eq(&self, other: &Self) -> bool {
        self.seqno == other.seqno && self.bits == other.bits && self.cells == other.cells
    }
}

impl Line {
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
            cells: CellStorage::C(ClusteredLine::new()),
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
    /// doesn't every possible bit of internal state, and we don't want to
    /// encourage using Line directly as a hash key.
    pub fn compute_shape_hash(&self) -> [u8; 16] {
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
        line.cells_mut()
            .last_mut()
            .map(|cell| cell.attrs_mut().set_wrapped(true));
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
        self.coerce_vec_storage().resize_with(width, Cell::blank);
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

    /// Wrap the line and return a scorecard that can be used by
    /// resize-time readability gates.
    pub fn wrap_with_report(
        self,
        width: usize,
        seqno: SequenceNo,
        cost_model: MonospaceKpCostModel,
    ) -> LineWrapReport {
        let mut cells: Vec<CellRef> = self.visible_cells().collect();
        if let Some(end_idx) = cells.iter().rposition(|c| c.str() != " ") {
            cells.truncate(end_idx + 1);
            let tokens: Vec<Cell> = cells.into_iter().map(|cell| cell.as_cell()).collect();
            let plan = bounded_monospace_wrap_plan(&tokens, width, cost_model);
            let selected_candidate =
                evaluate_break_offsets(&tokens, &plan.break_offsets, width, cost_model);
            let greedy_offsets = greedy_break_offsets_from_tokens(&tokens, width);
            let greedy_candidate =
                evaluate_break_offsets(&tokens, &greedy_offsets, width, cost_model);

            LineWrapReport {
                lines: materialize_wrap_lines_from_tokens(&tokens, &plan.break_offsets, seqno),
                scorecard: LineWrapScorecard {
                    mode: plan.mode,
                    greedy_total_cost: greedy_candidate.total_cost,
                    selected_total_cost: selected_candidate.total_cost,
                    badness_delta: saturating_diff_i64(
                        selected_candidate.total_cost,
                        greedy_candidate.total_cost,
                    ),
                    greedy_forced_breaks: greedy_candidate.forced_breaks,
                    selected_forced_breaks: selected_candidate.forced_breaks,
                    line_count: selected_candidate.line_count,
                    estimated_states: plan.estimated_states,
                    evaluated_states: plan.evaluated_states,
                },
            }
        } else {
            LineWrapReport {
                lines: vec![self],
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
        self.appdata
            .lock()
            .unwrap()
            .replace(Arc::downgrade(&appdata));
    }

    #[cfg(feature = "appdata")]
    pub fn clear_appdata(&self) {
        self.appdata.lock().unwrap().take();
    }

    /// Retrieve the appdata for the line, if any.
    /// This may return None in the case where the underlying data has
    /// been released: Line only stores a Weak reference to it.
    #[cfg(feature = "appdata")]
    pub fn get_appdata(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.appdata
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|data| data.upgrade())
    }

    /// Returns true if the line's last changed seqno is more recent
    /// than the provided seqno parameter
    pub fn changed_since(&self, seqno: SequenceNo) -> bool {
        self.seqno == SEQ_ZERO || self.seqno > seqno
    }

    pub fn current_seqno(&self) -> SequenceNo {
        self.seqno
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
            .set(LineBits::RTL, direction == Direction::LeftToRight);
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
                    range: grapheme_idx..grapheme_idx + 1,
                    semantic_type,
                });
            }

            if let Some(zone) = current_zone.as_mut() {
                zone.range.end = grapheme_idx;
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
        if (self.bits & LineBits::HAS_IMPLICIT_HYPERLINKS) == LineBits::NONE {
            return;
        }

        self.invalidate_implicit_hyperlinks_impl(seqno);
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

        // FIXME: let's build a string and a byte-to-cell map here, and
        // use this as an opportunity to rebuild HAS_HYPERLINK, skip matching
        // cells with existing non-implicit hyperlinks, and avoid matching
        // text with zero-width cells.
        self.bits |= LineBits::SCANNED_IMPLICIT_HYPERLINKS;
        self.bits &= !LineBits::HAS_IMPLICIT_HYPERLINKS;
        let line = self.as_str();

        let matches = Rule::match_hyperlinks(&line, rules);
        if matches.is_empty() {
            return;
        }

        let line = line.into_owned();
        let cells = self.coerce_vec_storage();
        if cells.scan_and_create_hyperlinks(&line, matches) {
            self.bits |= LineBits::HAS_IMPLICIT_HYPERLINKS;
        }
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

        let mut lower = click_col;
        let mut upper = click_col;

        // TODO: look back and look ahead for cells that are hidden by
        // a preceding multi-wide cell
        let cells = self.visible_cells().collect::<Vec<_>>();
        for cell in &cells {
            if cell.cell_index() < click_col {
                continue;
            }
            if !is_word(cell.str()) {
                break;
            }
            upper = cell.cell_index() + 1;
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
                cl.append_grapheme(" ", 1, CellAttributes::blank());
            }
            if idx == cl.len() {
                cl.append_grapheme(text, width, attr);
                self.invalidate_implicit_hyperlinks(seqno);
                self.invalidate_zones();
                self.update_last_change_seqno(seqno);
                return;
            }
        }

        self.set_cell(idx, Cell::new_grapheme_with_width(text, width, attr), seqno);
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
        let width = cell.width().max(1);

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
                cl.append_grapheme(" ", 1, CellAttributes::blank());
            }
            if idx == cl.len() {
                cl.append(cell);
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
            if idx + width > cells.len() {
                cells.resize_with(idx + width, Cell::blank);
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
            let width = cells[prior].width();
            if width > 1 {
                let attrs = cells[prior].attrs().clone();
                for nerf in prior..prior + width {
                    cells[nerf] = Cell::blank_with_attrs(attrs.clone());
                }
            }
        }
    }

    pub fn insert_cell(&mut self, x: usize, cell: Cell, right_margin: usize, seqno: SequenceNo) {
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
        self.invalidate_implicit_hyperlinks(seqno);
        if x < self.len() {
            self.invalidate_grapheme_at_or_before(x);
            self.coerce_vec_storage().remove(x);
        }
        if right_margin <= self.len() + 1
        /* we just removed one */
        {
            self.coerce_vec_storage()
                .insert(right_margin - 1, Cell::blank_with_attrs(blank_attr));
        }
        self.update_last_change_seqno(seqno);
        self.invalidate_zones();
    }

    pub fn prune_trailing_blanks(&mut self, seqno: SequenceNo) {
        if let CellStorage::C(cl) = &mut self.cells {
            if cl.prune_trailing_blanks() {
                self.update_last_change_seqno(seqno);
                self.invalidate_zones();
            }
            return;
        }

        let def_attr = CellAttributes::blank();
        let cells = self.coerce_vec_storage();
        if let Some(end_idx) = cells
            .iter()
            .rposition(|c| c.str() != " " || c.attrs() != &def_attr)
        {
            cells.resize_with(end_idx + 1, Cell::blank);
            self.update_last_change_seqno(seqno);
            self.invalidate_zones();
        }
    }

    pub fn fill_range(&mut self, cols: Range<usize>, cell: &Cell, seqno: SequenceNo) {
        if self.len() == 0 && *cell == Cell::blank() {
            // We would be filling it with blanks only to prune
            // them all away again before we return; NOP
            return;
        }
        for x in cols {
            // FIXME: we can skip the look-back for second and subsequent iterations
            self.set_cell_impl(x, cell.clone(), true, seqno);
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
            CellStorage::V(cells) => VisibleCellIter::V(VecStorageIter {
                cells: cells.iter(),
                idx: 0,
                skip_width: 0,
            }),
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
        self.cells = CellStorage::C(cv);
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
        self.visible_cells()
            .last()
            .map(|c| c.attrs().wrapped())
            .unwrap_or(false)
    }

    /// Adjust the value of the wrapped attribute on the last cell of this
    /// line.
    pub fn set_last_cell_was_wrapped(&mut self, wrapped: bool, seqno: SequenceNo) {
        self.update_last_change_seqno(seqno);
        if let CellStorage::C(cl) = &mut self.cells {
            if cl.len() == 0 {
                // Need to mark that implicit space as wrapped, so
                // explicitly add it
                cl.append(Cell::blank());
            }
            cl.set_last_cell_was_wrapped(wrapped);
            return;
        }

        let cells = self.coerce_vec_storage();
        if let Some(cell) = cells.last_mut() {
            cell.attrs_mut().set_wrapped(wrapped);
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
                for cell in other.visible_cells() {
                    cl.append(cell.as_cell());
                }
            }
        }
        self.update_last_change_seqno(seqno);
        self.invalidate_zones();
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        token_count.saturating_mul(lookahead)
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

impl LineWrapReport {
    fn into_parts(self) -> (Vec<Line>, MonospaceWrapMode) {
        let mode = self.scorecard.mode;
        (self.lines, mode)
    }
}

#[inline]
fn greedy_break_offsets_from_tokens(tokens: &[Cell], width: usize) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut current_width = 0usize;

    for (idx, token) in tokens.iter().enumerate() {
        let token_width = token.width();
        let need_new_line = current_width > 0 && current_width.saturating_add(token_width) > width;
        if need_new_line {
            offsets.push(idx);
            current_width = 0;
        }
        current_width = current_width.saturating_add(token_width);
    }

    offsets.push(tokens.len());
    offsets
}

#[inline]
fn fallback_wrap_plan(
    tokens: &[Cell],
    width: usize,
    estimated_states: usize,
    evaluated_states: usize,
) -> MonospaceWrapPlan {
    MonospaceWrapPlan {
        mode: MonospaceWrapMode::Fallback,
        break_offsets: greedy_break_offsets_from_tokens(tokens, width),
        estimated_states,
        evaluated_states,
    }
}

/// Compute a bounded DP wrap plan and deterministically fall back to greedy wrapping
/// when the configured state budget would be exceeded.
pub(crate) fn bounded_monospace_wrap_plan(
    tokens: &[Cell],
    width: usize,
    model: MonospaceKpCostModel,
) -> MonospaceWrapPlan {
    if tokens.is_empty() {
        return MonospaceWrapPlan {
            mode: MonospaceWrapMode::Dp,
            break_offsets: vec![],
            estimated_states: 0,
            evaluated_states: 0,
        };
    }

    let token_count = tokens.len();
    let estimated_states = model.estimated_dp_states(token_count);
    if width == 0 || model.should_fallback(token_count) {
        return fallback_wrap_plan(tokens, width, estimated_states, 0);
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

    for start in 0..token_count {
        let Some(prefix) = best[start].clone() else {
            continue;
        };

        let mut line_width = 0usize;
        let max_end = (start + model.lookahead_limit).min(token_count);

        for end in (start + 1)..=max_end {
            line_width = line_width.saturating_add(tokens[end - 1].width());
            evaluated_states = evaluated_states.saturating_add(1);
            if evaluated_states > model.max_dp_states {
                return fallback_wrap_plan(tokens, width, estimated_states, evaluated_states);
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
                (model.line_badness(slack, width, is_last_line), 0usize)
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
        None => fallback_wrap_plan(tokens, width, estimated_states, evaluated_states),
    }
}

#[inline]
fn evaluate_break_offsets(
    tokens: &[Cell],
    break_offsets: &[usize],
    width: usize,
    model: MonospaceKpCostModel,
) -> MonospaceBreakCandidate {
    let mut total_cost = 0u64;
    let mut forced_breaks = 0usize;
    let mut max_line_badness = 0u64;
    let mut line_count = 0usize;
    let mut normalized_breaks = Vec::new();
    let mut start = 0usize;

    for &raw_end in break_offsets {
        let end = raw_end.min(tokens.len());
        if end <= start {
            continue;
        }

        let line_width = tokens[start..end]
            .iter()
            .fold(0usize, |acc, token| acc.saturating_add(token.width()));
        let is_last_line = end == tokens.len();

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
            (model.line_badness(slack, width, is_last_line), 0usize)
        };

        total_cost = total_cost.saturating_add(line_cost);
        forced_breaks = forced_breaks.saturating_add(forced_inc);
        max_line_badness = max_line_badness.max(line_cost);
        line_count = line_count.saturating_add(1);
        normalized_breaks.push(end);
        start = end;
    }

    if start < tokens.len() {
        let line_width = tokens[start..]
            .iter()
            .fold(0usize, |acc, token| acc.saturating_add(token.width()));
        let (line_cost, forced_inc) = if line_width > width {
            if tokens.len() == start + 1 {
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
        normalized_breaks.push(tokens.len());
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

        let mut current_cells: Vec<Cell> = Vec::new();
        for token in &tokens[start..end] {
            let grapheme = token.clone();
            let fill_count = grapheme.width().saturating_sub(1);
            let fill_attr = grapheme.attrs().clone();
            current_cells.push(grapheme);

            for _ in 0..fill_count {
                current_cells.push(Cell::blank_with_attrs(fill_attr.clone()));
            }
        }

        let mut line = Line::from_cells(current_cells, seqno);
        if end < tokens.len() {
            line.set_last_cell_was_wrapped(true, seqno);
        }
        lines.push(line);
        start = end;
    }

    if lines.is_empty() {
        lines.push(Line::from_cells(vec![], seqno));
    }

    lines
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
    use alloc::collections::BTreeSet;
    use alloc::format;
    use frankenterm_cell::{Cell, CellAttributes, SemanticType};

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
        let kp_fallback_ratio_percent = if sample_count == 0 {
            0
        } else {
            kp_fallback_lines.saturating_mul(100) / sample_count
        };
        let fallback_fallback_ratio_percent = if sample_count == 0 {
            0
        } else {
            fallback_fallback_lines.saturating_mul(100) / sample_count
        };

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
    fn line_fill_range() {
        let mut line = Line::with_width(5, SEQ_ZERO);
        let cell = Cell::new('X', CellAttributes::default());
        line.fill_range(1..4, &cell, 1);
        assert_eq!(line.columns_as_str(1..4), "XXX");
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
    fn line_insert_cell() {
        let mut line: Line = "abde".into();
        line.insert_cell(2, Cell::new('c', CellAttributes::default()), 5, 1);
        assert_eq!(line.columns_as_str(0..5), "abcde");
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
        // Default text should produce at least one zone
        assert!(!zones.is_empty());
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
