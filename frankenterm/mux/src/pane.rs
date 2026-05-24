use crate::ExitBehavior;
use crate::domain::DomainId;
use crate::renderable::*;
use async_trait::async_trait;
use config::keyassignment::{KeyAssignment, ScrollbackEraseMode};
use downcast_rs::{Downcast, impl_downcast};
use frankenterm_dynamic::Value;
use frankenterm_term::color::ColorPalette;
use frankenterm_term::{
    Clipboard, DownloadHandler, KeyCode, KeyModifiers, MouseEvent, Progress, SemanticZone,
    StableRowIndex, TerminalConfiguration, TerminalSize,
};
use parking_lot::MappedMutexGuard;
use rangeset::RangeSet;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use termwiz::hyperlink::Rule;
use termwiz::input::KeyboardEncoding;
use termwiz::surface::{Line, SequenceNo};
use url::Url;

static PANE_ID: ::std::sync::atomic::AtomicUsize = ::std::sync::atomic::AtomicUsize::new(0);
pub type PaneId = usize;

pub fn alloc_pane_id() -> PaneId {
    PANE_ID.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct PaneConstraints {
    pub min_width: usize,
    pub min_height: usize,
    pub max_width: Option<usize>,
    pub max_height: Option<usize>,
    pub preferred_width: Option<usize>,
    pub preferred_height: Option<usize>,
    pub fixed: bool,
}

impl Default for PaneConstraints {
    fn default() -> Self {
        Self {
            min_width: 5,
            min_height: 3,
            max_width: None,
            max_height: None,
            preferred_width: None,
            preferred_height: None,
            fixed: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum CollapsePriority {
    Never,
    Low,
    Normal,
    High,
}

impl Default for CollapsePriority {
    fn default() -> Self {
        Self::Normal
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PerformAssignmentResult {
    /// Continue search for handler
    Unhandled,
    /// Found handler and acted upon the action
    Handled,
    /// Do not perform assignment, but instead treat the key event
    /// as though there was no assignment and run it as a key_down
    /// event.
    BlockAssignmentAndRouteToKeyDown,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SearchResult {
    pub start_y: StableRowIndex,
    /// The cell index into the line of the start of the match
    pub start_x: usize,
    pub end_y: StableRowIndex,
    /// The cell index into the line of the end of the match
    pub end_x: usize,
    /// An identifier that can be used to group results that have
    /// the same textual content
    pub match_id: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub enum Pattern {
    CaseSensitiveString(String),
    CaseInSensitiveString(String),
    Regex(String),
}

impl Default for Pattern {
    fn default() -> Self {
        Self::CaseSensitiveString("".to_string())
    }
}

impl std::ops::Deref for Pattern {
    type Target = String;
    fn deref(&self) -> &String {
        match self {
            Pattern::CaseSensitiveString(s) => s,
            Pattern::CaseInSensitiveString(s) => s,
            Pattern::Regex(s) => s,
        }
    }
}

impl std::ops::DerefMut for Pattern {
    fn deref_mut(&mut self) -> &mut String {
        match self {
            Pattern::CaseSensitiveString(s) => s,
            Pattern::CaseInSensitiveString(s) => s,
            Pattern::Regex(s) => s,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub enum PatternType {
    CaseSensitiveString,
    CaseInSensitiveString,
    Regex,
}

impl From<&Pattern> for PatternType {
    fn from(value: &Pattern) -> Self {
        match value {
            Pattern::CaseSensitiveString(_) => PatternType::CaseSensitiveString,
            Pattern::CaseInSensitiveString(_) => PatternType::CaseInSensitiveString,
            Pattern::Regex(_) => PatternType::Regex,
        }
    }
}

/// Why a close request is being made
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CloseReason {
    /// The containing window is being closed
    Window,
    /// The containing tab is being close
    Tab,
    /// Just this tab is being closed
    Pane,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogicalLine {
    pub physical_lines: Vec<Line>,
    pub logical: Line,
    pub first_row: StableRowIndex,
}

fn stable_row_offset(row: StableRowIndex, offset: usize) -> Option<StableRowIndex> {
    let offset = StableRowIndex::try_from(offset).ok()?;
    row.checked_add(offset)
}

fn stable_row_span(row: StableRowIndex, len: usize) -> Option<Range<StableRowIndex>> {
    let end = stable_row_offset(row, len)?;
    Some(row..end)
}

fn logical_len_exceeds_limit(current: usize, additional: usize, limit: usize) -> bool {
    match current.checked_add(additional) {
        Some(total) => total > limit,
        None => true,
    }
}

impl LogicalLine {
    pub fn contains_y(&self, y: StableRowIndex) -> bool {
        if y < self.first_row {
            return false;
        }

        match stable_row_offset(self.first_row, self.physical_lines.len()) {
            Some(end) => y < end,
            None => true,
        }
    }

    pub fn xy_to_logical_x(&self, x: usize, y: StableRowIndex) -> usize {
        let mut offset = 0;
        for (idx, line) in self.physical_lines.iter().enumerate() {
            let Some(phys_y) = stable_row_offset(self.first_row, idx) else {
                break;
            };
            if y < phys_y {
                // Eg: trying to drag off the top of the viewport.
                // Their y coordinate precedes our first line, so
                // the only logical x we can return is 0
                return 0;
            }
            if phys_y == y {
                return offset.saturating_add(x);
            }
            offset = offset.saturating_add(line.len());
        }
        // Allow selecting off the end of the line
        offset.saturating_add(x)
    }

    pub fn logical_x_to_physical_coord(&self, x: usize) -> (StableRowIndex, usize) {
        let Some(last_physical_line) = self.physical_lines.last() else {
            return (self.first_row, x);
        };

        let mut y = self.first_row;
        let mut idx = 0;
        for line in &self.physical_lines {
            let x_off = x - idx;
            let line_len = line.len();
            if x_off < line_len {
                return (y, x_off);
            }
            let Some(next_y) = y.checked_add(1) else {
                return (y, x.saturating_sub(idx).saturating_add(line_len));
            };
            y = next_y;
            idx = idx.saturating_add(line_len);
        }
        (
            y.saturating_sub(1),
            x.saturating_sub(idx)
                .saturating_add(last_physical_line.len()),
        )
    }
}

/// A Pane represents a view on a terminal
#[async_trait(?Send)]
pub trait Pane: Downcast + Send + Sync {
    fn pane_id(&self) -> PaneId;

    /// Returns the 0-based cursor position relative to the top left of
    /// the visible screen
    fn get_cursor_position(&self) -> StableCursorPosition;

    fn get_current_seqno(&self) -> SequenceNo;

    /// Returns misc metadata that is pane-specific
    fn get_metadata(&self) -> Value {
        Value::Null
    }

    /// Given a range of lines, return the subset of those lines that
    /// have changed since the supplied sequence no.
    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex>;

    /// Returns a set of lines from the scrollback or visible portion of
    /// the display.  The lines are indexed using StableRowIndex, which
    /// can be invalidated if the scrollback is busy, or when switching
    /// to the alternate screen.
    /// To deal with this, this function will adjust the input so that
    /// a range that has been scrolled off the top will return the top
    /// n rows of the scrollback (where n is the size of the input range),
    /// or the bottom n rows of the scrollback when switching to the alt
    /// screen and the index would go off the bottom.
    /// Because of this, we also return the adjusted StableRowIndex for
    /// the first row in the range.
    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>);

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines);

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    );

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine>;

    fn apply_hyperlinks(&self, lines: Range<StableRowIndex>, rules: &[Rule]) {
        struct ApplyHyperLinks<'a> {
            rules: &'a [Rule],
        }
        impl<'a> ForEachPaneLogicalLine for ApplyHyperLinks<'a> {
            fn with_logical_line_mut(
                &mut self,
                _: Range<StableRowIndex>,
                lines: &mut [&mut Line],
            ) -> bool {
                Line::apply_hyperlink_rules(self.rules, lines);

                true
            }
        }

        self.for_each_logical_line_in_stable_range_mut(lines, &mut ApplyHyperLinks { rules });
    }

    /// Returns render related dimensions
    fn get_dimensions(&self) -> RenderableDimensions;

    /// Returns live tiered scrollback telemetry for panes that maintain it.
    fn get_tiered_scrollback_status(&self) -> Option<PaneTieredScrollbackStatus> {
        None
    }

    fn pane_constraints(&self) -> PaneConstraints {
        PaneConstraints::default()
    }

    fn collapse_priority(&self) -> CollapsePriority {
        CollapsePriority::default()
    }

    fn get_title(&self) -> String;
    fn get_progress(&self) -> Progress {
        Progress::None
    }
    fn send_paste(&self, text: &str) -> anyhow::Result<()>;
    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>>;
    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write>;
    fn resize(&self, size: TerminalSize) -> anyhow::Result<()>;
    /// Called as a hint that the pane is being resized as part of
    /// a zoom-to-fill-all-the-tab-space operation.
    fn set_zoomed(&self, _zoomed: bool) {}
    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()>;
    fn key_up(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()>;
    fn perform_assignment(&self, _assignment: &KeyAssignment) -> PerformAssignmentResult {
        PerformAssignmentResult::Unhandled
    }
    fn mouse_event(&self, event: MouseEvent) -> anyhow::Result<()>;
    fn perform_actions(&self, _actions: Vec<termwiz::escape::Action>) {}
    fn is_dead(&self) -> bool;
    fn kill(&self) {}
    fn palette(&self) -> ColorPalette;
    fn domain_id(&self) -> DomainId;

    fn get_keyboard_encoding(&self) -> KeyboardEncoding {
        KeyboardEncoding::Xterm
    }

    fn copy_user_vars(&self) -> HashMap<String, String> {
        HashMap::new()
    }

    fn erase_scrollback(&self, _erase_mode: ScrollbackEraseMode) {}

    /// Called to advise on whether this tab has focus
    fn focus_changed(&self, _focused: bool) {}

    /// Called to advise remote mux that this is the active tab
    /// for the current identity
    fn advise_focus(&self) {}

    fn has_unseen_output(&self) -> bool {
        false
    }

    /// Certain panes are OK to be closed with impunity (no prompts)
    fn can_close_without_prompting(&self, _reason: CloseReason) -> bool {
        false
    }

    /// Performs a search bounded to the specified range.
    /// If the result is empty then there are no matches.
    /// Otherwise, if limit.is_none(), the result shall contain all possible
    /// matches.
    /// If limit.is_some(), then the maximum number of results that will be
    /// returned is limited to the specified number, and the
    /// SearchResult::start_y of the last item
    /// in the result can be used as the start of the next region to search.
    /// You can tell that you have reached the end of the results if the number
    /// of results is smaller than the limit you set.
    async fn search(
        &self,
        _pattern: Pattern,
        _range: Range<StableRowIndex>,
        _limit: Option<u32>,
    ) -> anyhow::Result<Vec<SearchResult>> {
        Ok(vec![])
    }

    /// Retrieve the set of semantic zones
    fn get_semantic_zones(&self) -> anyhow::Result<Vec<SemanticZone>> {
        Ok(vec![])
    }

    /// Returns true if the terminal has grabbed the mouse and wants to
    /// give the embedded application a chance to process events.
    /// In practice this controls whether the gui will perform local
    /// handling of clicks.
    fn is_mouse_grabbed(&self) -> bool;
    fn is_alt_screen_active(&self) -> bool;

    fn set_clipboard(&self, _clipboard: &Arc<dyn Clipboard>) {}
    fn set_download_handler(&self, _handler: &Arc<dyn DownloadHandler>) {}
    fn set_config(&self, _config: Arc<dyn TerminalConfiguration>) {}
    fn get_config(&self) -> Option<Arc<dyn TerminalConfiguration>> {
        None
    }

    fn get_current_working_dir(&self, policy: CachePolicy) -> Option<Url>;
    fn get_foreground_process_name(&self, _policy: CachePolicy) -> Option<String> {
        None
    }
    fn get_foreground_process_info(
        &self,
        _policy: CachePolicy,
    ) -> Option<procinfo::LocalProcessInfo> {
        None
    }

    fn tty_name(&self) -> Option<String> {
        None
    }

    fn exit_behavior(&self) -> Option<ExitBehavior> {
        None
    }
}
impl_downcast!(Pane);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    FetchImmediate,
    AllowStale,
}

/// This trait is used to implement/provide a callback that is used together
/// with the Pane::with_lines_mut method.
/// Ideally we'd simply pass an FnMut with the same signature as the trait
/// method defined here, but doing so results in Pane not being object-safe.
pub trait WithPaneLines {
    /// The `first_row` parameter is set to the StableRowIndex of the resolved
    /// first row from the Pane::with_lines_mut method. It will usually be
    /// the start of the lines range, but in case that row is no longer in
    /// a valid range (scrolled out of scrollback), it may be revised.
    ///
    /// `lines` is a mutable slice of the mutable lines in the requested
    /// stable range.
    fn with_lines_mut(&mut self, first_row: StableRowIndex, lines: &mut [&mut Line]);
}

/// This trait is used to implement/provide a callback that is used together
/// with the Pane::for_each_logical_line_in_stable_range_mut method.
/// Ideally we'd simply pass an FnMut with the same signature as the trait
/// method defined here, but doing so results in Pane not being object-safe.
pub trait ForEachPaneLogicalLine {
    /// The `stable_range` parameter is set to the range of physical lines
    /// that comprise the current logical line.
    ///
    /// `lines` is a mutable slice of the mutable physical lines that comprise
    /// the current logical line.
    ///
    /// Return `true` to continue with the next logical line in the requested
    /// range, or `false` to cease iteration.
    fn with_logical_line_mut(
        &mut self,
        stable_range: Range<StableRowIndex>,
        lines: &mut [&mut Line],
    ) -> bool;
}

/// A helper that allows you to implement Pane::with_lines_mut in terms
/// of your existing Pane::get_lines method.
///
/// The mutability is really a lie: while `with_lines` is passed something
/// that is mutable, it is operating on a copy the lines that won't persist
/// beyond the call to Pane::with_lines_mut.
pub fn impl_with_lines_via_get_lines<P: Pane + ?Sized>(
    pane: &P,
    lines: Range<StableRowIndex>,
    with_lines: &mut dyn WithPaneLines,
) {
    let (first, mut lines) = pane.get_lines(lines);
    let mut line_refs = vec![];
    for line in lines.iter_mut() {
        line_refs.push(line);
    }
    with_lines.with_lines_mut(first, &mut line_refs);
}

/// A helper that allows you to implement Pane::for_each_logical_line_in_stable_range_mut
/// in terms of your existing Pane::get_logical_lines method.
///
/// The mutability is really a lie: while `with_lines` is passed something
/// that is mutable, it is operating on a copy the lines that won't persist
/// beyond the call to Pane::with_lines_mut.
pub fn impl_for_each_logical_line_via_get_logical_lines<P: Pane + ?Sized>(
    pane: &P,
    lines: Range<StableRowIndex>,
    for_line: &mut dyn ForEachPaneLogicalLine,
) {
    let mut logical = pane.get_logical_lines(lines);

    for line in &mut logical {
        let mut line_refs = vec![];
        for phys in line.physical_lines.iter_mut() {
            line_refs.push(phys);
        }
        let Some(range) = stable_row_span(line.first_row, line.physical_lines.len()) else {
            break;
        };
        let should_continue = for_line.with_logical_line_mut(range, &mut line_refs);
        if !should_continue {
            break;
        }
    }
}

/// A helper that allows you to implement Pane::get_logical_lines in terms of
/// your Pane::get_lines method.
pub fn impl_get_logical_lines_via_get_lines<P: Pane + ?Sized>(
    pane: &P,
    lines: Range<StableRowIndex>,
) -> Vec<LogicalLine> {
    let (mut first, mut phys) = pane.get_lines(lines);

    // Avoid pathological cases where we have eg: a really long logical line
    // (such as 1.5MB of json) that we previously wrapped.  We don't want to
    // un-wrap, scan, and re-wrap that thing.
    // This is an imperfect length constraint to partially manage the cost.
    const MAX_LOGICAL_LINE_LEN: usize = 1024;
    let mut back_len = 0;

    // Look backwards to find the start of the first logical line
    while first > 0 {
        let (prior, back) = pane.get_lines(first - 1..first);
        if prior == first {
            break;
        }
        if back.is_empty() {
            break;
        }
        if !back[0].last_cell_was_wrapped() {
            break;
        }
        if logical_len_exceeds_limit(back_len, back[0].len(), MAX_LOGICAL_LINE_LEN) {
            break;
        }
        back_len = back_len.saturating_add(back[0].len());
        first = prior;
        for (idx, line) in back.into_iter().enumerate() {
            phys.insert(idx, line);
        }
    }

    // Look forwards to find the end of the last logical line
    while let Some(last) = phys.last() {
        if !last.last_cell_was_wrapped() {
            break;
        }
        if last.len() > MAX_LOGICAL_LINE_LEN {
            break;
        }

        let Some(next_row) = stable_row_offset(first, phys.len()) else {
            break;
        };
        let Some(next_row_end) = next_row.checked_add(1) else {
            break;
        };
        let (last_row, mut ahead) = pane.get_lines(next_row..next_row_end);
        if last_row != next_row {
            break;
        }
        if ahead.is_empty() {
            break;
        }
        phys.append(&mut ahead);
    }

    // Now process this stuff into logical lines
    let mut lines = vec![];
    for (idx, line) in phys.into_iter().enumerate() {
        let Some(first_row) = stable_row_offset(first, idx) else {
            break;
        };
        match lines.last_mut() {
            None => {
                let logical = line.clone();
                lines.push(LogicalLine {
                    physical_lines: vec![line],
                    logical,
                    first_row,
                });
            }
            Some(prior)
                if prior.logical.last_cell_was_wrapped()
                    && prior.logical.len() <= MAX_LOGICAL_LINE_LEN =>
            {
                let seqno = prior.logical.current_seqno().max(line.current_seqno());
                prior.logical.set_last_cell_was_wrapped(false, seqno);
                prior.logical.append_line(line.clone(), seqno);
                prior.physical_lines.push(line);
            }
            Some(_) => {
                let logical = line.clone();
                lines.push(LogicalLine {
                    physical_lines: vec![line],
                    logical,
                    first_row,
                });
            }
        }
    }
    lines
}

/// A helper that allows you to implement Pane::get_lines in terms
/// of your Pane::with_lines_mut method.
pub fn impl_get_lines_via_with_lines<P: Pane + ?Sized>(
    pane: &P,
    lines: Range<StableRowIndex>,
) -> (StableRowIndex, Vec<Line>) {
    struct LineCollector {
        first: StableRowIndex,
        lines: Vec<Line>,
    }

    let mut collector = LineCollector {
        first: 0,
        lines: vec![],
    };

    impl WithPaneLines for LineCollector {
        fn with_lines_mut(&mut self, first_row: StableRowIndex, lines: &mut [&mut Line]) {
            self.first = first_row;
            for line in lines.iter_mut() {
                self.lines.push(line.clone());
            }
        }
    }

    pane.with_lines_mut(lines, &mut collector);
    (collector.first, collector.lines)
}

#[cfg(test)]
mod test {
    use super::*;
    use k9::snapshot;
    use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
    use std::borrow::Cow;
    use termwiz::surface::SEQ_ZERO;

    struct FakePane {
        lines: Mutex<Vec<Line>>,
        size: Mutex<TerminalSize>,
        writes: Mutex<Vec<u8>>,
        base_row: StableRowIndex,
        empty_one_line_rows: Vec<StableRowIndex>,
    }

    impl FakePane {
        fn new(lines: Vec<Line>) -> Self {
            Self::new_with_empty_one_line_rows(lines, Vec::new())
        }

        fn new_with_empty_one_line_rows(
            lines: Vec<Line>,
            empty_one_line_rows: Vec<StableRowIndex>,
        ) -> Self {
            Self::new_with_base_row_and_empty_one_line_rows(lines, 0, empty_one_line_rows)
        }

        fn new_with_base_row(lines: Vec<Line>, base_row: StableRowIndex) -> Self {
            Self::new_with_base_row_and_empty_one_line_rows(lines, base_row, Vec::new())
        }

        fn new_with_base_row_and_empty_one_line_rows(
            lines: Vec<Line>,
            base_row: StableRowIndex,
            empty_one_line_rows: Vec<StableRowIndex>,
        ) -> Self {
            Self {
                lines: Mutex::new(lines),
                size: Mutex::new(TerminalSize::default()),
                writes: Mutex::new(Vec::new()),
                base_row,
                empty_one_line_rows,
            }
        }

        fn range_slice(&self, stable_range: Range<StableRowIndex>) -> Option<(usize, usize)> {
            let start_offset = stable_range.start.checked_sub(self.base_row)?;
            let end_offset = stable_range.end.checked_sub(self.base_row)?;
            if start_offset < 0 || end_offset < start_offset {
                return None;
            }

            let start = usize::try_from(start_offset).ok()?;
            let len = usize::try_from(end_offset - start_offset).ok()?;
            Some((start, len))
        }
    }

    impl Pane for FakePane {
        fn pane_id(&self) -> PaneId {
            0
        }
        fn get_cursor_position(&self) -> StableCursorPosition {
            StableCursorPosition::default()
        }

        fn get_current_seqno(&self) -> SequenceNo {
            SEQ_ZERO
        }

        fn get_changed_since(
            &self,
            _: Range<StableRowIndex>,
            _: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            RangeSet::new()
        }

        fn with_lines_mut(
            &self,
            stable_range: Range<StableRowIndex>,
            with_lines: &mut dyn WithPaneLines,
        ) {
            let mut line_refs = vec![];
            let mut lines = self.lines.lock();
            if let Some((start, len)) = self.range_slice(stable_range.clone()) {
                for line in lines.iter_mut().skip(start).take(len) {
                    line_refs.push(line);
                }
            }
            with_lines.with_lines_mut(stable_range.start, &mut line_refs);
        }

        fn for_each_logical_line_in_stable_range_mut(
            &self,
            lines: Range<StableRowIndex>,
            for_line: &mut dyn ForEachPaneLogicalLine,
        ) {
            crate::pane::impl_for_each_logical_line_via_get_logical_lines(self, lines, for_line)
        }

        fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
            crate::pane::impl_get_logical_lines_via_get_lines(self, lines)
        }

        fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            let first = lines.start;
            if lines.end == lines.start.saturating_add(1)
                && self.empty_one_line_rows.contains(&lines.start)
            {
                return (first, Vec::new());
            }
            let Some((start, len)) = self.range_slice(lines) else {
                return (first, Vec::new());
            };
            (
                first,
                self.lines
                    .lock()
                    .iter()
                    .skip(start)
                    .take(len)
                    .cloned()
                    .collect(),
            )
        }
        fn get_dimensions(&self) -> RenderableDimensions {
            let size = *self.size.lock();
            RenderableDimensions {
                cols: size.cols,
                viewport_rows: size.rows,
                scrollback_rows: size.rows,
                physical_top: 0,
                scrollback_top: 0,
                dpi: size.dpi,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
                reverse_video: false,
            }
        }

        fn get_title(&self) -> String {
            "fake-pane".to_string()
        }
        fn send_paste(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(None)
        }
        fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
            MutexGuard::map(self.writes.lock(), |writes| {
                let writer: &mut dyn std::io::Write = writes;
                writer
            })
        }
        fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
            *self.size.lock() = size;
            Ok(())
        }

        fn mouse_event(&self, _: MouseEvent) -> anyhow::Result<()> {
            Ok(())
        }
        fn is_dead(&self) -> bool {
            false
        }
        fn palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
        fn domain_id(&self) -> DomainId {
            1
        }

        fn is_mouse_grabbed(&self) -> bool {
            false
        }
        fn is_alt_screen_active(&self) -> bool {
            false
        }
        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
            None
        }
        fn key_down(&self, _: KeyCode, _: KeyModifiers) -> anyhow::Result<()> {
            Ok(())
        }
        fn key_up(&self, _: KeyCode, _: KeyModifiers) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn physical_lines_from_text(text: &str, width: usize) -> Vec<Line> {
        let mut physical_lines = vec![];
        for logical in text.split('\n') {
            let chunks = logical
                .chars()
                .collect::<Vec<char>>()
                .chunks(width)
                .map(|c| c.into_iter().collect::<String>())
                .collect::<Vec<String>>();
            let n_chunks = chunks.len();
            for (idx, chunk) in chunks.into_iter().enumerate() {
                let mut line = Line::from_text(&chunk, &Default::default(), 1, None);
                if idx < n_chunks - 1 {
                    line.set_last_cell_was_wrapped(true, 1);
                }
                physical_lines.push(line);
            }
        }
        physical_lines
    }

    fn summarize_logical_lines(lines: &[LogicalLine]) -> Vec<(StableRowIndex, Cow<'_, str>)> {
        lines
            .iter()
            .map(|l| (l.first_row, l.logical.as_str()))
            .collect::<Vec<_>>()
    }

    fn line(text: &str, wrapped: bool) -> Line {
        let mut l = Line::from_text(text, &Default::default(), SEQ_ZERO, None);
        l.set_last_cell_was_wrapped(wrapped, SEQ_ZERO);
        l
    }

    #[test]
    fn fake_pane_default_methods_do_not_panic() {
        let pane = FakePane::new(Vec::new());

        assert_eq!(pane.pane_id(), 0);
        assert_eq!(pane.get_cursor_position(), StableCursorPosition::default());
        assert_eq!(pane.get_current_seqno(), SEQ_ZERO);
        assert!(pane.get_changed_since(0..10, SEQ_ZERO).is_empty());
        assert_eq!(pane.get_title(), "fake-pane");
        assert!(pane.send_paste("discarded").is_ok());
        assert!(
            pane.key_down(KeyCode::Char('x'), KeyModifiers::NONE)
                .is_ok()
        );
        assert!(pane.key_up(KeyCode::Char('x'), KeyModifiers::NONE).is_ok());
        assert!(pane.reader().unwrap().is_none());
        assert!(!pane.is_dead());
        assert_eq!(pane.domain_id(), 1);
        assert_eq!(pane.palette(), ColorPalette::default());

        let resized = TerminalSize {
            rows: 7,
            cols: 13,
            pixel_width: 130,
            pixel_height: 70,
            dpi: 144,
        };
        pane.resize(resized).unwrap();
        let dimensions = pane.get_dimensions();
        assert_eq!(dimensions.cols, resized.cols);
        assert_eq!(dimensions.viewport_rows, resized.rows);
        assert_eq!(dimensions.scrollback_rows, resized.rows);
        assert_eq!(dimensions.pixel_width, resized.pixel_width);
        assert_eq!(dimensions.pixel_height, resized.pixel_height);
        assert_eq!(dimensions.dpi, resized.dpi);

        {
            let mut writer = pane.writer();
            writer.write_all(b"captured").unwrap();
            writer.flush().unwrap();
        }
        assert_eq!(&*pane.writes.lock(), b"captured");
    }

    #[test]
    fn logical_len_limit_check_treats_overflow_as_exceeded() {
        assert!(!logical_len_exceeds_limit(10, 5, 20));
        assert!(logical_len_exceeds_limit(10, 11, 20));
        assert!(logical_len_exceeds_limit(usize::MAX, 1, usize::MAX));
    }

    #[test]
    fn logical_lines() {
        let text = "Hello there this is a long line.\nlogical line two\nanother long line here\nlogical line four\nlogical line five\ncap it off with another long line";
        let width = 20;
        let physical_lines = physical_lines_from_text(text, width);

        fn text_from_lines(lines: &[Line]) -> Vec<Cow<'_, str>> {
            lines.iter().map(|l| l.as_str()).collect::<Vec<_>>()
        }

        let line_text = text_from_lines(&physical_lines);
        snapshot!(
            line_text,
            r#"
[
    "Hello there this is ",
    "a long line.",
    "logical line two",
    "another long line he",
    "re",
    "logical line four",
    "logical line five",
    "cap it off with anot",
    "her long line",
]
"#
        );

        let pane = FakePane::new(physical_lines);

        let logical = pane.get_logical_lines(0..30);
        snapshot!(
            summarize_logical_lines(&logical),
            r#"
[
    (
        0,
        "Hello there this is a long line.",
    ),
    (
        2,
        "logical line two",
    ),
    (
        3,
        "another long line here",
    ),
    (
        5,
        "logical line four",
    ),
    (
        6,
        "logical line five",
    ),
    (
        7,
        "cap it off with another long line",
    ),
]
"#
        );

        // Now try with offset bounds
        let offset = pane.get_logical_lines(1..3);
        snapshot!(
            summarize_logical_lines(&offset),
            r#"
[
    (
        0,
        "Hello there this is a long line.",
    ),
    (
        2,
        "logical line two",
    ),
]
"#
        );

        let offset = pane.get_logical_lines(1..4);
        snapshot!(
            summarize_logical_lines(&offset),
            r#"
[
    (
        0,
        "Hello there this is a long line.",
    ),
    (
        2,
        "logical line two",
    ),
    (
        3,
        "another long line here",
    ),
]
"#
        );

        let offset = pane.get_logical_lines(1..5);
        snapshot!(
            summarize_logical_lines(&offset),
            r#"
[
    (
        0,
        "Hello there this is a long line.",
    ),
    (
        2,
        "logical line two",
    ),
    (
        3,
        "another long line here",
    ),
]
"#
        );

        let offset = pane.get_logical_lines(1..6);
        snapshot!(
            summarize_logical_lines(&offset),
            r#"
[
    (
        0,
        "Hello there this is a long line.",
    ),
    (
        2,
        "logical line two",
    ),
    (
        3,
        "another long line here",
    ),
    (
        5,
        "logical line four",
    ),
]
"#
        );

        let offset = pane.get_logical_lines(1..7);
        snapshot!(
            summarize_logical_lines(&offset),
            r#"
[
    (
        0,
        "Hello there this is a long line.",
    ),
    (
        2,
        "logical line two",
    ),
    (
        3,
        "another long line here",
    ),
    (
        5,
        "logical line four",
    ),
    (
        6,
        "logical line five",
    ),
]
"#
        );

        let offset = pane.get_logical_lines(1..8);
        snapshot!(
            summarize_logical_lines(&offset),
            r#"
[
    (
        0,
        "Hello there this is a long line.",
    ),
    (
        2,
        "logical line two",
    ),
    (
        3,
        "another long line here",
    ),
    (
        5,
        "logical line four",
    ),
    (
        6,
        "logical line five",
    ),
    (
        7,
        "cap it off with another long line",
    ),
]
"#
        );

        let line = &offset[0];
        let coords = (0..line.logical.len())
            .map(|idx| line.logical_x_to_physical_coord(idx))
            .collect::<Vec<_>>();
        snapshot!(
            coords,
            "
[
    (
        0,
        0,
    ),
    (
        0,
        1,
    ),
    (
        0,
        2,
    ),
    (
        0,
        3,
    ),
    (
        0,
        4,
    ),
    (
        0,
        5,
    ),
    (
        0,
        6,
    ),
    (
        0,
        7,
    ),
    (
        0,
        8,
    ),
    (
        0,
        9,
    ),
    (
        0,
        10,
    ),
    (
        0,
        11,
    ),
    (
        0,
        12,
    ),
    (
        0,
        13,
    ),
    (
        0,
        14,
    ),
    (
        0,
        15,
    ),
    (
        0,
        16,
    ),
    (
        0,
        17,
    ),
    (
        0,
        18,
    ),
    (
        0,
        19,
    ),
    (
        1,
        0,
    ),
    (
        1,
        1,
    ),
    (
        1,
        2,
    ),
    (
        1,
        3,
    ),
    (
        1,
        4,
    ),
    (
        1,
        5,
    ),
    (
        1,
        6,
    ),
    (
        1,
        7,
    ),
    (
        1,
        8,
    ),
    (
        1,
        9,
    ),
    (
        1,
        10,
    ),
    (
        1,
        11,
    ),
]
"
        );
    }

    #[test]
    fn get_logical_lines_breaks_on_empty_backward_lookup() {
        let pane = FakePane::new_with_empty_one_line_rows(
            vec![
                line("prefix", false),
                line("visible", false),
                line("tail", false),
            ],
            vec![0],
        );

        let lines = pane.get_logical_lines(1..2);

        assert_eq!(
            summarize_logical_lines(&lines),
            vec![(1, Cow::Borrowed("visible"))]
        );
    }

    #[test]
    fn get_logical_lines_breaks_on_empty_forward_lookup() {
        let pane = FakePane::new_with_empty_one_line_rows(
            vec![
                line("wrapped", true),
                line("missing", false),
                line("tail", false),
            ],
            vec![1],
        );

        let lines = pane.get_logical_lines(0..1);

        assert_eq!(
            summarize_logical_lines(&lines),
            vec![(0, Cow::Borrowed("wrapped"))]
        );
    }

    #[test]
    fn get_logical_lines_stops_forward_scan_at_stable_row_limit() {
        let start = StableRowIndex::MAX - 1;
        let pane = FakePane::new_with_base_row(vec![line("boundary", true)], start);

        let lines = pane.get_logical_lines(start..StableRowIndex::MAX);

        assert_eq!(
            summarize_logical_lines(&lines),
            vec![(start, Cow::Borrowed("boundary"))]
        );
    }

    fn is_double_click_word(s: &str) -> bool {
        match s.chars().count() {
            1 => !" \t\n{[}]()\"'`".contains(s),
            0 => false,
            _ => true,
        }
    }

    #[test]
    fn double_click() {
        let attr = Default::default();
        let logical = LogicalLine {
            physical_lines: vec![
                Line::from_text("hello", &attr, SEQ_ZERO, None),
                Line::from_text("yo", &attr, SEQ_ZERO, None),
            ],
            logical: Line::from_text("helloyo", &attr, SEQ_ZERO, None),
            first_row: 0,
        };

        assert_eq!(logical.xy_to_logical_x(2, -1), 0);
        assert_eq!(logical.xy_to_logical_x(20, 1), 25);

        let start_idx = logical.xy_to_logical_x(2, 1);

        use termwiz::surface::line::DoubleClickRange;

        assert_eq!(start_idx, 7);
        match logical
            .logical
            .compute_double_click_range(start_idx, is_double_click_word)
        {
            DoubleClickRange::Range(click_range) => {
                assert_eq!(click_range, 7..7);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn logical_x_to_physical_coord_handles_empty_physical_lines() {
        let logical = LogicalLine {
            physical_lines: vec![],
            logical: Line::new(SEQ_ZERO),
            first_row: 7,
        };

        assert_eq!(logical.logical_x_to_physical_coord(3), (7, 3));
    }

    #[test]
    fn logical_line_coordinate_conversion_saturates_extreme_offsets() {
        let attr = Default::default();
        let logical = LogicalLine {
            physical_lines: vec![
                Line::from_text("hello", &attr, SEQ_ZERO, None),
                Line::from_text("0123456789", &attr, SEQ_ZERO, None),
            ],
            logical: Line::from_text("hello0123456789", &attr, SEQ_ZERO, None),
            first_row: 0,
        };

        assert_eq!(logical.xy_to_logical_x(usize::MAX, 1), usize::MAX);
        assert_eq!(
            logical.logical_x_to_physical_coord(usize::MAX),
            (1, usize::MAX - 5)
        );

        let max_row_logical = LogicalLine {
            physical_lines: vec![Line::from_text("0123456789", &attr, SEQ_ZERO, None)],
            logical: Line::from_text("0123456789", &attr, SEQ_ZERO, None),
            first_row: StableRowIndex::MAX,
        };
        assert_eq!(
            max_row_logical.logical_x_to_physical_coord(usize::MAX),
            (StableRowIndex::MAX, usize::MAX)
        );
    }

    // ── PaneConstraints ──────────────────────────────────────

    #[test]
    fn pane_constraints_default() {
        let c = PaneConstraints::default();
        assert_eq!(c.min_width, 5);
        assert_eq!(c.min_height, 3);
        assert!(c.max_width.is_none());
        assert!(c.max_height.is_none());
        assert!(c.preferred_width.is_none());
        assert!(c.preferred_height.is_none());
        assert!(!c.fixed);
    }

    #[test]
    fn pane_constraints_equality() {
        let a = PaneConstraints::default();
        let b = PaneConstraints::default();
        assert_eq!(a, b);

        let c = PaneConstraints {
            min_width: 10,
            ..Default::default()
        };
        assert_ne!(a, c);
    }

    #[test]
    fn pane_constraints_clone_copy() {
        let a = PaneConstraints {
            min_width: 20,
            min_height: 10,
            max_width: Some(200),
            max_height: Some(100),
            preferred_width: Some(80),
            preferred_height: Some(24),
            fixed: true,
        };
        let b = a; // Copy
        let c = a.clone(); // Clone
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn pane_constraints_debug() {
        let c = PaneConstraints::default();
        let dbg = format!("{:?}", c);
        assert!(dbg.contains("PaneConstraints"));
        assert!(dbg.contains("min_width"));
    }

    // ── CollapsePriority ─────────────────────────────────────

    #[test]
    fn collapse_priority_default_is_normal() {
        assert_eq!(CollapsePriority::default(), CollapsePriority::Normal);
    }

    #[test]
    fn collapse_priority_equality() {
        assert_eq!(CollapsePriority::Never, CollapsePriority::Never);
        assert_eq!(CollapsePriority::Low, CollapsePriority::Low);
        assert_eq!(CollapsePriority::Normal, CollapsePriority::Normal);
        assert_eq!(CollapsePriority::High, CollapsePriority::High);
        assert_ne!(CollapsePriority::Never, CollapsePriority::High);
        assert_ne!(CollapsePriority::Low, CollapsePriority::Normal);
    }

    #[test]
    fn collapse_priority_clone_copy() {
        let p = CollapsePriority::High;
        let p2 = p; // Copy
        let p3 = p.clone(); // Clone
        assert_eq!(p, p2);
        assert_eq!(p, p3);
    }

    // ── PerformAssignmentResult ──────────────────────────────

    #[test]
    fn perform_assignment_result_equality() {
        assert_eq!(
            PerformAssignmentResult::Unhandled,
            PerformAssignmentResult::Unhandled
        );
        assert_eq!(
            PerformAssignmentResult::Handled,
            PerformAssignmentResult::Handled
        );
        assert_eq!(
            PerformAssignmentResult::BlockAssignmentAndRouteToKeyDown,
            PerformAssignmentResult::BlockAssignmentAndRouteToKeyDown
        );
        assert_ne!(
            PerformAssignmentResult::Unhandled,
            PerformAssignmentResult::Handled
        );
    }

    #[test]
    fn perform_assignment_result_clone_copy() {
        let r = PerformAssignmentResult::Handled;
        let r2 = r; // Copy
        let r3 = r.clone(); // Clone
        assert_eq!(r, r2);
        assert_eq!(r, r3);
    }

    // ── SearchResult ─────────────────────────────────────────

    #[test]
    fn search_result_equality() {
        let a = SearchResult {
            start_y: 0,
            start_x: 5,
            end_y: 0,
            end_x: 10,
            match_id: 1,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn search_result_ordering() {
        let a = SearchResult {
            start_y: 0,
            start_x: 0,
            end_y: 0,
            end_x: 5,
            match_id: 1,
        };
        let b = SearchResult {
            start_y: 1,
            start_x: 0,
            end_y: 1,
            end_x: 5,
            match_id: 2,
        };
        assert!(a < b);

        let c = SearchResult {
            start_y: 0,
            start_x: 3,
            end_y: 0,
            end_x: 8,
            match_id: 3,
        };
        assert!(a < c);
    }

    // ── Pattern ──────────────────────────────────────────────

    #[test]
    fn pattern_default_is_empty_case_sensitive() {
        let p = Pattern::default();
        assert_eq!(p, Pattern::CaseSensitiveString("".to_string()));
    }

    #[test]
    fn pattern_deref_returns_inner_string() {
        let p = Pattern::CaseSensitiveString("hello".to_string());
        assert_eq!(&*p, "hello");

        let p = Pattern::CaseInSensitiveString("world".to_string());
        assert_eq!(&*p, "world");

        let p = Pattern::Regex("foo.*bar".to_string());
        assert_eq!(&*p, "foo.*bar");
    }

    #[test]
    fn pattern_deref_mut() {
        let mut p = Pattern::CaseSensitiveString("hello".to_string());
        p.push_str(" world");
        assert_eq!(&*p, "hello world");
    }

    #[test]
    fn pattern_equality() {
        let a = Pattern::CaseSensitiveString("test".to_string());
        let b = Pattern::CaseSensitiveString("test".to_string());
        assert_eq!(a, b);

        let c = Pattern::CaseInSensitiveString("test".to_string());
        assert_ne!(a, c);
    }

    // ── PatternType ──────────────────────────────────────────

    #[test]
    fn pattern_type_from_pattern() {
        let p = Pattern::CaseSensitiveString("x".to_string());
        assert_eq!(PatternType::from(&p), PatternType::CaseSensitiveString);

        let p = Pattern::CaseInSensitiveString("x".to_string());
        assert_eq!(PatternType::from(&p), PatternType::CaseInSensitiveString);

        let p = Pattern::Regex("x".to_string());
        assert_eq!(PatternType::from(&p), PatternType::Regex);
    }

    #[test]
    fn pattern_type_equality() {
        assert_eq!(PatternType::Regex, PatternType::Regex);
        assert_ne!(
            PatternType::CaseSensitiveString,
            PatternType::CaseInSensitiveString
        );
    }

    // ── CloseReason ──────────────────────────────────────────

    #[test]
    fn close_reason_equality() {
        assert_eq!(CloseReason::Window, CloseReason::Window);
        assert_eq!(CloseReason::Tab, CloseReason::Tab);
        assert_eq!(CloseReason::Pane, CloseReason::Pane);
        assert_ne!(CloseReason::Window, CloseReason::Tab);
        assert_ne!(CloseReason::Tab, CloseReason::Pane);
    }

    #[test]
    fn close_reason_clone_copy() {
        let r = CloseReason::Window;
        let r2 = r; // Copy
        let r3 = r.clone(); // Clone
        assert_eq!(r, r2);
        assert_eq!(r, r3);
    }
}
