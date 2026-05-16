use crate::selection::{SelectionCoordinate, SelectionRange};
use crate::termwindow::{TermWindow, TermWindowNotif};
use config::ConfigHandle;
use config::keyassignment::{ClipboardCopyDestination, QuickSelectArguments, ScrollbackEraseMode};
use mux::domain::DomainId;
use mux::pane::{
    CachePolicy, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId, Pattern, SearchResult,
    WithPaneLines,
};
use mux::renderable::*;
use parking_lot::{MappedMutexGuard, Mutex};
use rangeset::RangeSet;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use termwiz::cell::{Cell, CellAttributes};
use termwiz::color::AnsiColor;
use termwiz::surface::{SEQ_ZERO, SequenceNo};
use url::Url;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    Clipboard, Intensity, KeyCode, KeyModifiers, Line, MouseEvent, StableRowIndex, TerminalSize,
};
use window::WindowOps;

const PATTERNS: [&str; 14] = [
    // markdown_url
    r"\[[^]]*\]\(([^)]+)\)",
    // url
    r"(?:https?://|git@|git://|ssh://|ftp://|file://)\S+",
    // diff_a
    r"--- a/(\S+)",
    // diff_b
    r"\+\+\+ b/(\S+)",
    // docker
    r"sha256:([0-9a-f]{64})",
    // path
    r"(?:[.\w\-@~]+)?(?:/+[.\w\-@]+)+",
    // color
    r"#[0-9a-fA-F]{6}",
    // uuid
    r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
    // ipfs
    r"Qm[0-9a-zA-Z]{44}",
    // sha
    r"[0-9a-f]{7,40}",
    // ip
    r"\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}",
    // ipv6
    r"[A-f0-9:]+:+[A-f0-9:]+[%\w\d]+",
    // address
    r"0x[0-9a-fA-F]+",
    // number
    r"[0-9]{4,}",
];

/// This function computes a set of labels for a given alphabet.
/// It is derived from https://github.com/fcsonline/tmux-thumbs/blob/ae91d5f7c0d989933e86409833c46a1eca521b6a/src/alphabets.rs
/// which is Copyright (c) 2019 Ferran Basora and provided under the MIT license
pub fn compute_labels_for_alphabet(alphabet: &str, num_matches: usize) -> Vec<String> {
    compute_labels_for_alphabet_impl(alphabet, num_matches, true)
}

pub fn compute_labels_for_alphabet_with_preserved_case(
    alphabet: &str,
    num_matches: usize,
) -> Vec<String> {
    compute_labels_for_alphabet_impl(alphabet, num_matches, false)
}

fn compute_labels_for_alphabet_impl(
    alphabet: &str,
    num_matches: usize,
    make_lowercase: bool,
) -> Vec<String> {
    let alphabet = if make_lowercase {
        alphabet
            .chars()
            .map(|c| c.to_lowercase().to_string())
            .collect::<Vec<String>>()
    } else {
        alphabet
            .chars()
            .map(|c| c.to_string())
            .collect::<Vec<String>>()
    };
    // Prefer to use single character matches to represent everything
    let mut primary = alphabet.clone();
    let mut secondary = vec![];

    loop {
        if primary.len() + secondary.len() >= num_matches {
            break;
        }

        // We have more matches than can be represented by alphabet,
        // so steal one of the single character options from the end
        // of the alphabet and use it to generate a two character
        // label
        let prefix = match primary.pop() {
            Some(p) => p,
            None => break,
        };

        // Generate a two character label for each of the alphabet
        // characters.  This ignores later alphabet characters;
        // since we popped our prefix from the end of alphabet,
        // length limiting this iteration ensures that we don't
        // end up with a duplicate letters in the result.
        let prefixed: Vec<String> = alphabet
            .iter()
            .take(num_matches - primary.len() - secondary.len())
            .map(|s| format!("{}{}", prefix, s))
            .collect();

        secondary.splice(0..0, prefixed);
    }

    let len = secondary.len();

    primary
        .drain(0..)
        .take(num_matches - len)
        .chain(secondary.drain(0..))
        .collect()
}

fn merge_dirty_results(
    lines: Range<StableRowIndex>,
    mut delegate_dirty: RangeSet<StableRowIndex>,
    overlay_dirty: &RangeSet<StableRowIndex>,
) -> RangeSet<StableRowIndex> {
    delegate_dirty.add_set(overlay_dirty);
    delegate_dirty.intersection_with_range(lines)
}

fn dirty_rows_for_search_refresh(
    previous_match_rows: impl IntoIterator<Item = StableRowIndex>,
    last_bar_pos: Option<StableRowIndex>,
    next_bar_pos: StableRowIndex,
) -> RangeSet<StableRowIndex> {
    let mut dirty_rows = RangeSet::default();

    for idx in previous_match_rows {
        dirty_rows.add(idx);
    }
    if let Some(idx) = last_bar_pos {
        dirty_rows.add(idx);
    }
    dirty_rows.add(next_bar_pos);

    dirty_rows
}

fn clear_rendered_dirty_result(
    dirty_results: &mut RangeSet<StableRowIndex>,
    stable_idx: StableRowIndex,
) {
    dirty_results.remove(stable_idx);
}

fn compute_search_row_from_viewport(
    viewport: Option<StableRowIndex>,
    dims: RenderableDimensions,
) -> StableRowIndex {
    let top = viewport.unwrap_or(dims.physical_top);
    (top + dims.viewport_rows as StableRowIndex).saturating_sub(1)
}

#[cfg(test)]
mod alphabet_test {
    use super::*;
    use std::ops::Range;

    #[test]
    fn simple_alphabet() {
        assert_eq!(compute_labels_for_alphabet("abcd", 3), vec!["a", "b", "c"]);
    }

    #[test]
    fn more_matches_than_alphabet_can_represent() {
        assert_eq!(
            compute_labels_for_alphabet("asdfqwerzxcvjklmiuopghtybn", 792).len(),
            676
        );
    }

    #[test]
    fn composed_single() {
        assert_eq!(
            compute_labels_for_alphabet("abcd", 6),
            vec!["a", "b", "c", "da", "db", "dc"]
        );
    }

    #[test]
    fn composed_multiple() {
        assert_eq!(
            compute_labels_for_alphabet("abcd", 8),
            vec!["a", "b", "ca", "cb", "da", "db", "dc", "dd"]
        );
    }

    #[test]
    fn composed_max() {
        // The number of chars in the alphabet limits the potential matches to fewer
        // than the number of matches that we requested
        assert_eq!(
            compute_labels_for_alphabet("ab", 5),
            vec!["aa", "ab", "ba", "bb"]
        );
    }

    #[test]
    fn composed_capital() {
        assert_eq!(
            compute_labels_for_alphabet_with_preserved_case("AB", 4),
            vec!["AA", "AB", "BA", "BB"]
        );
    }

    #[test]
    fn composed_mixed() {
        assert_eq!(
            compute_labels_for_alphabet_with_preserved_case("aA", 4),
            vec!["aa", "aA", "Aa", "AA"]
        );
    }

    #[test]
    fn lowercase_alphabet_equal() {
        assert_eq!(
            compute_labels_for_alphabet_with_preserved_case("abc123", 12),
            compute_labels_for_alphabet("abc123", 12)
        );
    }

    fn make_dirty_ranges(
        ranges: impl IntoIterator<Item = Range<StableRowIndex>>,
    ) -> RangeSet<StableRowIndex> {
        let mut dirty = RangeSet::default();
        for range in ranges {
            dirty.add_range(range);
        }
        dirty
    }

    fn collect_ranges(set: &RangeSet<StableRowIndex>) -> Vec<Range<StableRowIndex>> {
        set.iter().cloned().collect()
    }

    #[test]
    fn test_dirty_rect_merge() {
        let visible = 9..15;
        let delegate_dirty = make_dirty_ranges(std::iter::once(10..12));
        let overlay_dirty = make_dirty_ranges(std::iter::once(12..14));

        let merged = merge_dirty_results(visible, delegate_dirty, &overlay_dirty);

        assert_eq!(collect_ranges(&merged), vec![10..14]);
    }

    #[test]
    fn dirty_rect_non_adjacent_ranges_stay_separate() {
        // [10..12] and [14..16] share no boundary — there's a gap at row 12,13.
        // The merge MUST NOT collapse them into one rect, otherwise rows 12-13
        // would be needlessly repainted on every overlay refresh.
        let visible = 0..30;
        let delegate_dirty = make_dirty_ranges(std::iter::once(10..12));
        let overlay_dirty = make_dirty_ranges(std::iter::once(14..16));

        let merged = merge_dirty_results(visible, delegate_dirty, &overlay_dirty);

        assert_eq!(collect_ranges(&merged), vec![10..12, 14..16]);
    }

    #[test]
    fn dirty_rect_overlapping_ranges_collapse_to_one() {
        // delegate [10..14] and overlay [12..16] overlap at 12,13.
        // RangeSet semantics: overlap collapses into [10..16].
        let visible = 0..30;
        let delegate_dirty = make_dirty_ranges(std::iter::once(10..14));
        let overlay_dirty = make_dirty_ranges(std::iter::once(12..16));

        let merged = merge_dirty_results(visible, delegate_dirty, &overlay_dirty);

        assert_eq!(collect_ranges(&merged), vec![10..16]);
    }

    #[test]
    fn dirty_rect_clips_to_visible_viewport() {
        // delegate touches rows 5..8 (entirely above the visible window) and
        // 12..14 (inside). overlay touches 18..22 which spills past the
        // bottom edge. After clipping to visible 10..20, only the inside
        // portions survive. This is the invariant that prevents the
        // renderer from invalidating off-screen geometry.
        let visible = 10..20;
        let delegate_dirty = make_dirty_ranges([5..8, 12..14]);
        let overlay_dirty = make_dirty_ranges(std::iter::once(18..22));

        let merged = merge_dirty_results(visible, delegate_dirty, &overlay_dirty);

        assert_eq!(collect_ranges(&merged), vec![12..14, 18..20]);
    }

    #[test]
    fn dirty_rect_empty_overlay_leaves_delegate_clipped() {
        // Common case: overlay is dormant and contributes nothing. The
        // result should be exactly the delegate, clipped to the viewport.
        let visible = 0..10;
        let delegate_dirty = make_dirty_ranges([2..4, 7..15]);
        let overlay_dirty = RangeSet::default();

        let merged = merge_dirty_results(visible, delegate_dirty, &overlay_dirty);

        assert_eq!(collect_ranges(&merged), vec![2..4, 7..10]);
    }

    #[test]
    fn dirty_rect_both_empty_produces_empty_set() {
        let visible = 0..10;
        let merged = merge_dirty_results(visible, RangeSet::default(), &RangeSet::default());
        assert!(collect_ranges(&merged).is_empty());
    }

    #[test]
    fn dirty_rect_back_to_back_single_rows_merge_into_one() {
        // Adjacent SINGLE-row dirty marks (e.g. cursor moved one row, then
        // the next was also dirtied) must collapse into one rect. RangeSet
        // treats {[5..6], [6..7], [7..8]} as the contiguous range [5..8].
        let visible = 0..20;
        let delegate_dirty = make_dirty_ranges([5..6, 6..7]);
        let overlay_dirty = make_dirty_ranges(std::iter::once(7..8));

        let merged = merge_dirty_results(visible, delegate_dirty, &overlay_dirty);

        assert_eq!(collect_ranges(&merged), vec![5..8]);
    }

    #[test]
    fn dirty_rect_clear_on_present_removes_only_specified_row() {
        // The render loop calls clear_rendered_dirty_result(idx) once per
        // row it actually paints. Clearing idx=5 must remove ONLY row 5
        // from the dirty set; the rest of the marked rows stay queued
        // for the renderer's next pass.
        let mut dirty = make_dirty_ranges([3..4, 5..6, 7..8, 10..11]);

        clear_rendered_dirty_result(&mut dirty, 5);

        assert_eq!(collect_ranges(&dirty), vec![3..4, 7..8, 10..11]);
    }

    #[test]
    fn dirty_rect_clear_on_present_splits_contiguous_range_around_cleared_row() {
        // A contiguous dirty range [10..15] with idx 12 cleared must split
        // into [10..12, 13..15]. This is the bookkeeping the renderer
        // depends on when only some of a contiguous range has been
        // painted in a partial frame.
        let mut dirty = make_dirty_ranges(std::iter::once(10..15));

        clear_rendered_dirty_result(&mut dirty, 12);

        assert_eq!(collect_ranges(&dirty), vec![10..12, 13..15]);
    }

    #[test]
    fn dirty_rect_clear_on_present_idempotent_for_already_clean_row() {
        // Clearing a row that was never dirty must be a no-op — no panic,
        // no spurious mutation. Defends against the renderer
        // double-clearing rows after a redraw without re-marking.
        let mut dirty = make_dirty_ranges([3..4, 7..8]);
        let before = collect_ranges(&dirty);

        clear_rendered_dirty_result(&mut dirty, 99);
        clear_rendered_dirty_result(&mut dirty, 5);

        assert_eq!(collect_ranges(&dirty), before);
    }

    #[test]
    fn dirty_rect_clear_on_present_full_visible_loop_empties_viewport() {
        // Simulate the renderer's actual present sequence: it iterates
        // every visible row and clears each one. After the full loop
        // every in-viewport dirty marker is gone; off-viewport rows
        // (beyond the viewport bottom) remain queued for the next frame.
        let visible: Range<StableRowIndex> = 10..15;
        let mut dirty = make_dirty_ranges([10..15, 20..22]);

        for idx in visible {
            clear_rendered_dirty_result(&mut dirty, idx);
        }

        // Every visible row is now clean; the off-screen [20..22] must
        // still be marked dirty for a future frame. If any row in
        // [10..15] survived, collect_ranges would return more than
        // [20..22].
        assert_eq!(collect_ranges(&dirty), vec![20..22]);
    }

    #[test]
    fn dirty_rect_clear_on_present_leaves_offscreen_dirty_for_next_frame() {
        // Rows outside the rendered viewport must NOT be cleared by the
        // present loop. They stay queued so the next frame still paints
        // them when they scroll into view.
        let visible: Range<StableRowIndex> = 100..110;
        let mut dirty = make_dirty_ranges([50..52, 105..107, 200..201]);

        for idx in visible {
            clear_rendered_dirty_result(&mut dirty, idx);
        }

        // The two out-of-viewport ranges survive; the in-viewport portion
        // (105..107) is gone.
        assert_eq!(collect_ranges(&dirty), vec![50..52, 200..201]);
    }

    #[test]
    fn dirty_rect_out_of_viewport_ranges_drop_completely() {
        // If every dirty range sits entirely outside the visible window,
        // the merge result is empty — nothing to paint.
        let visible = 100..110;
        let delegate_dirty = make_dirty_ranges(std::iter::once(5..8));
        let overlay_dirty = make_dirty_ranges(std::iter::once(200..205));

        let merged = merge_dirty_results(visible, delegate_dirty, &overlay_dirty);

        assert!(collect_ranges(&merged).is_empty());
    }

    #[test]
    fn search_refresh_marks_previous_match_rows_and_new_search_row_dirty() {
        let dirty = dirty_rows_for_search_refresh([20, 21], Some(24), 30);

        assert_eq!(collect_ranges(&dirty), vec![20..22, 24..25, 30..31]);
    }

    #[test]
    fn compute_search_row_tracks_viewport_bottom_edge() {
        let dims = RenderableDimensions {
            cols: 80,
            viewport_rows: 5,
            scrollback_rows: 100,
            physical_top: 40,
            scrollback_top: 0,
            dpi: 96,
            pixel_width: 640,
            pixel_height: 80,
            reverse_video: false,
        };

        assert_eq!(compute_search_row_from_viewport(None, dims), 44);
        assert_eq!(compute_search_row_from_viewport(Some(12), dims), 16);
    }
}

pub struct QuickSelectOverlay {
    renderer: Mutex<QuickSelectRenderable>,
    delegate: Arc<dyn Pane>,
}

#[derive(Debug)]
struct MatchResult {
    range: Range<usize>,
    label: String,
}

struct QuickSelectRenderable {
    delegate: Arc<dyn Pane>,
    /// The text that the user entered
    pattern: Pattern,
    /// The most recently queried set of matches
    results: Vec<SearchResult>,
    by_line: HashMap<StableRowIndex, Vec<MatchResult>>,
    by_label: HashMap<String, usize>,
    selection: String,

    viewport: Option<StableRowIndex>,
    last_bar_pos: Option<StableRowIndex>,

    dirty_results: RangeSet<StableRowIndex>,
    result_pos: Option<usize>,
    width: usize,
    height: usize,

    /// We use this to cancel ourselves later
    window: ::window::Window,

    config: ConfigHandle,
    args: QuickSelectArguments,
}

impl QuickSelectOverlay {
    pub fn with_pane(
        term_window: &TermWindow,
        pane: &Arc<dyn Pane>,
        args: &QuickSelectArguments,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let viewport = term_window.get_viewport(pane.pane_id());
        let dims = pane.get_dimensions();

        let config = term_window.config.clone();

        let mut pattern = "(?m)(".to_string();
        let mut have_patterns = false;
        if !args.patterns.is_empty() {
            for p in &args.patterns {
                if have_patterns {
                    pattern.push('|');
                }
                pattern.push_str(p);
                have_patterns = true;
            }
        } else {
            // User-provided patterns take precedence over built-ins
            for p in &config.quick_select_patterns {
                if have_patterns {
                    pattern.push('|');
                }
                pattern.push_str(p);
                have_patterns = true;
            }
            if !config.disable_default_quick_select_patterns {
                for p in &PATTERNS {
                    if have_patterns {
                        pattern.push('|');
                    }
                    pattern.push_str(p);
                    have_patterns = true;
                }
            }
        }
        pattern.push(')');

        let pattern = Pattern::Regex(pattern);

        let window = term_window.window.clone().ok_or_else(|| {
            anyhow::anyhow!("cannot start quick-select overlay without a GUI window")
        })?;
        let mut renderer = QuickSelectRenderable {
            delegate: Arc::clone(pane),
            pattern,
            selection: "".to_string(),
            results: vec![],
            by_line: HashMap::new(),
            by_label: HashMap::new(),
            dirty_results: RangeSet::default(),
            viewport,
            last_bar_pos: None,
            window,
            result_pos: None,
            width: dims.cols,
            height: dims.viewport_rows,
            config,
            args: args.clone(),
        };

        let search_row = renderer.compute_search_row();
        renderer.dirty_results.add(search_row);
        renderer.update_search(true);

        Ok(Arc::new(QuickSelectOverlay {
            renderer: Mutex::new(renderer),
            delegate: Arc::clone(pane),
        }))
    }

    pub fn viewport_changed(&self, viewport: Option<StableRowIndex>) {
        let mut render = self.renderer.lock();
        if render.viewport != viewport {
            if let Some(last) = render.last_bar_pos.take() {
                render.dirty_results.add(last);
            }
            if let Some(pos) = viewport.as_ref() {
                render.dirty_results.add(*pos);
            }
            render.viewport = viewport;
        }
    }
}

impl Pane for QuickSelectOverlay {
    fn pane_id(&self) -> PaneId {
        self.delegate.pane_id()
    }

    fn get_title(&self) -> String {
        self.delegate.get_title()
    }

    fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
        // Ignore
        Ok(())
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        self.delegate.writer()
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        self.delegate.resize(size)
    }

    fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
        Ok(())
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        let mods = mods.remove_positional_mods();
        match (key, mods) {
            (KeyCode::Escape, KeyModifiers::NONE) => self.renderer.lock().close(),
            (KeyCode::UpArrow, KeyModifiers::NONE)
            | (KeyCode::Enter, KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CTRL) => {
                // Move to prior match
                let mut r = self.renderer.lock();
                if let Some(cur) = r.result_pos.as_ref() {
                    let prior = if *cur > 0 {
                        cur - 1
                    } else {
                        r.results.len().saturating_sub(1)
                    };
                    r.activate_match_number(prior);
                }
            }
            (KeyCode::PageUp, KeyModifiers::NONE) => {
                // Skip this page of matches and move up to the first match from
                // the prior page.
                let dims = self.delegate.get_dimensions();
                let mut r = self.renderer.lock();
                if let Some(cur) = r.result_pos {
                    let top = r.viewport.unwrap_or(dims.physical_top);
                    let prior = top - dims.viewport_rows as isize;
                    if let Some(pos) = r
                        .results
                        .iter()
                        .position(|res| res.start_y > prior && res.start_y < top)
                    {
                        r.activate_match_number(pos);
                    } else {
                        r.activate_match_number(cur.saturating_sub(1));
                    }
                }
            }
            (KeyCode::PageDown, KeyModifiers::NONE) => {
                // Skip this page of matches and move down to the first match from
                // the next page.
                let dims = self.delegate.get_dimensions();
                let mut r = self.renderer.lock();
                if let Some(cur) = r.result_pos {
                    let top = r.viewport.unwrap_or(dims.physical_top);
                    let bottom = top + dims.viewport_rows as isize;
                    if let Some(pos) = r.results.iter().position(|res| res.start_y >= bottom) {
                        r.activate_match_number(pos);
                    } else {
                        let len = r.results.len().saturating_sub(1);
                        r.activate_match_number(cur.min(len));
                    }
                }
            }
            (KeyCode::DownArrow, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CTRL) => {
                // Move to next match
                let mut r = self.renderer.lock();
                if let Some(cur) = r.result_pos.as_ref() {
                    let next = if *cur + 1 >= r.results.len() {
                        0
                    } else {
                        *cur + 1
                    };
                    r.activate_match_number(next);
                }
            }
            (KeyCode::Char(c), KeyModifiers::NONE) | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                // Type to add to the selection
                let mut r = self.renderer.lock();
                r.selection.push(c);
                let lowered = r.selection.to_lowercase();
                let paste = lowered != r.selection;
                if let Some(result_index) = r.by_label.get(&lowered).cloned() {
                    r.select_and_copy_match_number(result_index, paste);
                    r.close();
                }
            }
            (KeyCode::Backspace, KeyModifiers::NONE) => {
                // Backspace to edit the selection
                let mut r = self.renderer.lock();
                r.selection.pop();
            }
            (KeyCode::Char('u'), KeyModifiers::CTRL) => {
                // CTRL-u to clear the selection
                let mut r = self.renderer.lock();
                r.selection.clear();
            }
            _ => {}
        }
        Ok(())
    }

    fn mouse_event(&self, event: MouseEvent) -> anyhow::Result<()> {
        self.delegate.mouse_event(event)
    }

    fn perform_actions(&self, actions: Vec<termwiz::escape::Action>) {
        self.delegate.perform_actions(actions)
    }

    fn is_dead(&self) -> bool {
        self.delegate.is_dead()
    }

    fn palette(&self) -> ColorPalette {
        self.delegate.palette()
    }
    fn domain_id(&self) -> DomainId {
        self.delegate.domain_id()
    }

    fn erase_scrollback(&self, erase_mode: ScrollbackEraseMode) {
        self.delegate.erase_scrollback(erase_mode)
    }

    fn is_mouse_grabbed(&self) -> bool {
        // Force grabbing off while we're searching
        false
    }

    fn is_alt_screen_active(&self) -> bool {
        false
    }

    fn set_clipboard(&self, clipboard: &Arc<dyn Clipboard>) {
        self.delegate.set_clipboard(clipboard)
    }

    fn get_current_working_dir(&self, policy: CachePolicy) -> Option<Url> {
        self.delegate.get_current_working_dir(policy)
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        // move to the search box
        let renderer = self.renderer.lock();
        StableCursorPosition {
            x: 8 + wezterm_term::unicode_column_width(&renderer.selection, None),
            y: renderer.compute_search_row(),
            shape: termwiz::surface::CursorShape::SteadyBlock,
            visibility: termwiz::surface::CursorVisibility::Visible,
        }
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.delegate.get_current_seqno()
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        let dirty = self.delegate.get_changed_since(lines.clone(), seqno);
        merge_dirty_results(lines, dirty, &self.renderer.lock().dirty_results)
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        self.delegate
            .for_each_logical_line_in_stable_range_mut(lines, for_line);
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        self.delegate.get_logical_lines(lines)
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        let mut renderer = self.renderer.lock();
        // Take care to access self.delegate methods here before we get into
        // calling into its own with_lines_mut to avoid a runtime
        // borrow erro!
        renderer.check_for_resize();
        let dims = self.get_dimensions();
        let search_row = renderer.compute_search_row();

        struct OverlayLines<'a> {
            with_lines: &'a mut dyn WithPaneLines,
            dims: RenderableDimensions,
            search_row: StableRowIndex,
            renderer: &'a mut QuickSelectRenderable,
        }

        self.delegate.with_lines_mut(
            lines,
            &mut OverlayLines {
                with_lines,
                dims,
                search_row,
                renderer: &mut *renderer,
            },
        );

        impl<'a> WithPaneLines for OverlayLines<'a> {
            fn with_lines_mut(&mut self, first_row: StableRowIndex, lines: &mut [&mut Line]) {
                let mut overlay_lines = vec![];

                let config = &self.renderer.config;
                let colors = config.resolved_palette.clone();
                let disable_attr = config.quick_select_remove_styling;

                // Process the lines; for the search row we want to render instead
                // the search UI.
                // For rows with search results, we want to highlight the matching ranges

                for (idx, line) in lines.iter_mut().enumerate() {
                    let mut line: Line = line.clone();
                    if disable_attr {
                        line.cells_mut_for_attr_changes_only()
                            .iter_mut()
                            .for_each(|cell| cell.attrs_mut().clear());
                        line.clear_appdata();
                    }
                    let stable_idx = idx as StableRowIndex + first_row;
                    clear_rendered_dirty_result(&mut self.renderer.dirty_results, stable_idx);
                    if stable_idx == self.search_row {
                        // Replace with search UI
                        let rev = CellAttributes::default().set_reverse(true).clone();
                        line.fill_range(0..self.dims.cols, &Cell::new(' ', rev.clone()), SEQ_ZERO);
                        line.overlay_text_with_attribute(
                            0,
                            &format!(
                                "Select: {}  (type highlighted prefix to {}, uppercase pastes, ESC to cancel)",
                                self.renderer.selection,
                                if self.renderer.args.label.is_empty() {
                                    "copy"
                                } else {
                                    &self.renderer.args.label
                                },
                            ),
                            rev,
                            SEQ_ZERO,
                        );
                        self.renderer.last_bar_pos = Some(self.search_row);
                        line.clear_appdata();
                    } else if let Some(matches) = self.renderer.by_line.get(&stable_idx) {
                        for m in matches {
                            // highlight
                            for cell_idx in m.range.clone() {
                                if let Some(cell) =
                                    line.cells_mut_for_attr_changes_only().get_mut(cell_idx)
                                {
                                    cell.attrs_mut()
                                        .set_background(
                                            colors
                                                .quick_select_match_bg
                                                .unwrap_or(AnsiColor::Black.into()),
                                        )
                                        .set_foreground(
                                            colors
                                                .quick_select_match_fg
                                                .unwrap_or(AnsiColor::Green.into()),
                                        )
                                        .set_reverse(false)
                                        .set_intensity(Intensity::Bold);
                                }
                            }
                            for (idx, c) in m.label.chars().enumerate() {
                                let mut attr = line
                                    .get_cell(idx)
                                    .map(|cell| cell.attrs().clone())
                                    .unwrap_or_else(|| CellAttributes::default());
                                attr.set_background(
                                    colors
                                        .quick_select_label_bg
                                        .unwrap_or(AnsiColor::Black.into()),
                                )
                                .set_foreground(
                                    colors
                                        .quick_select_label_fg
                                        .unwrap_or(AnsiColor::Olive.into()),
                                )
                                .set_reverse(false)
                                .set_intensity(Intensity::Bold);
                                line.set_cell(m.range.start + idx, Cell::new(c, attr), SEQ_ZERO);
                            }
                        }
                        line.clear_appdata();
                    }
                    overlay_lines.push(line);
                }

                let mut overlay_refs: Vec<&mut Line> = overlay_lines.iter_mut().collect();
                self.with_lines.with_lines_mut(first_row, &mut overlay_refs);
            }
        }
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let mut renderer = self.renderer.lock();
        renderer.check_for_resize();
        let dims = self.get_dimensions();

        let (top, mut lines) = self.delegate.get_lines(lines);
        let colors = renderer.config.resolved_palette.clone();
        let disable_attr = renderer.config.quick_select_remove_styling;

        // Process the lines; for the search row we want to render instead
        // the search UI.
        // For rows with search results, we want to highlight the matching ranges
        let search_row = renderer.compute_search_row();
        for (idx, line) in lines.iter_mut().enumerate() {
            if disable_attr {
                line.cells_mut_for_attr_changes_only()
                    .iter_mut()
                    .for_each(|cell| cell.attrs_mut().clear());
            }
            let stable_idx = idx as StableRowIndex + top;
            clear_rendered_dirty_result(&mut renderer.dirty_results, stable_idx);
            if stable_idx == search_row {
                // Replace with search UI
                let rev = CellAttributes::default().set_reverse(true).clone();
                line.fill_range(0..dims.cols, &Cell::new(' ', rev.clone()), SEQ_ZERO);
                line.overlay_text_with_attribute(
                    0,
                    &format!(
                        "Select: {}  (type highlighted prefix to {}, uppercase pastes, ESC to cancel)",
                        renderer.selection,
                        if renderer.args.label.is_empty() {
                            "copy"
                        } else {
                            &renderer.args.label
                        },
                    ),
                    rev,
                    SEQ_ZERO,
                );
                renderer.last_bar_pos = Some(search_row);
            } else if let Some(matches) = renderer.by_line.get(&stable_idx) {
                for m in matches {
                    // highlight
                    for cell_idx in m.range.clone() {
                        if let Some(cell) = line.cells_mut_for_attr_changes_only().get_mut(cell_idx)
                        {
                            cell.attrs_mut()
                                .set_background(
                                    colors
                                        .quick_select_match_bg
                                        .unwrap_or(AnsiColor::Black.into()),
                                )
                                .set_foreground(
                                    colors
                                        .quick_select_match_fg
                                        .unwrap_or(AnsiColor::Green.into()),
                                )
                                .set_reverse(false)
                                .set_intensity(Intensity::Bold);
                        }
                    }
                    for (idx, c) in m.label.chars().enumerate() {
                        let mut attr = line
                            .get_cell(idx)
                            .map(|cell| cell.attrs().clone())
                            .unwrap_or_else(|| CellAttributes::default());
                        attr.set_background(
                            colors
                                .quick_select_label_bg
                                .unwrap_or(AnsiColor::Black.into()),
                        )
                        .set_foreground(
                            colors
                                .quick_select_label_fg
                                .unwrap_or(AnsiColor::Olive.into()),
                        )
                        .set_reverse(false)
                        .set_intensity(Intensity::Bold);
                        line.set_cell(m.range.start + idx, Cell::new(c, attr), SEQ_ZERO);
                    }
                }
            }
        }

        (top, lines)
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        self.delegate.get_dimensions()
    }
}

impl QuickSelectRenderable {
    fn compute_search_row(&self) -> StableRowIndex {
        compute_search_row_from_viewport(self.viewport, self.delegate.get_dimensions())
    }

    fn close(&self) {
        TermWindow::schedule_cancel_overlay_for_pane(self.window.clone(), self.delegate.pane_id());
    }

    fn set_viewport(&self, row: Option<StableRowIndex>) {
        let dims = self.delegate.get_dimensions();
        let pane_id = self.delegate.pane_id();
        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.set_viewport(pane_id, row, dims);
            })));
    }

    fn check_for_resize(&mut self) {
        let dims = self.delegate.get_dimensions();
        if dims.cols == self.width && dims.viewport_rows == self.height {
            return;
        }

        self.width = dims.cols;
        self.height = dims.viewport_rows;

        let pos = self.result_pos;
        self.update_search(false);
        self.result_pos = pos;
    }

    fn recompute_results(&mut self) {
        /// Produce the sorted seq of unique match_ids from the results
        fn compute_uniq_results(results: &[SearchResult]) -> Vec<usize> {
            let mut ids: Vec<usize> = results.iter().map(|sr| sr.match_id).collect();
            ids.sort();
            ids.dedup();
            ids
        }

        let uniq_results = compute_uniq_results(&self.results);

        // Label each unique result
        let labels = compute_labels_for_alphabet(
            if !self.args.alphabet.is_empty() {
                &self.args.alphabet
            } else {
                &self.config.quick_select_alphabet
            },
            uniq_results.len(),
        );
        self.by_label.clear();

        // Keep track of match_id -> label
        let mut assigned_labels: HashMap<usize, usize> = HashMap::new();

        // Work through the results in reverse order, so that we assign eg: `a` to the
        // bottom-right-most result first and so on
        for (result_index, res) in self.results.iter().enumerate().rev() {
            // Figure out which label to use based on the match_id
            let label_index = match assigned_labels.get(&res.match_id).copied() {
                Some(idx) => idx,
                None => {
                    let idx = assigned_labels.len();
                    assigned_labels.insert(res.match_id, idx);
                    idx
                }
            };
            let label = match labels.get(label_index) {
                Some(l) => l,
                None => {
                    // There are more result candidates than the alphabet
                    // can support, so we skip this one and keep looking:
                    // we may still have matches that have an assigned
                    // label, so we keep going rather than breaking
                    // out of the loop.
                    continue;
                }
            };

            self.by_label.entry(label.clone()).or_insert(result_index);
            for idx in res.start_y..=res.end_y {
                let range = if idx == res.start_y && idx == res.end_y {
                    // Range on same line
                    res.start_x..res.end_x
                } else if idx == res.end_y {
                    // final line of multi-line
                    0..res.end_x
                } else if idx == res.start_y {
                    // first line of multi-line
                    res.start_x..self.width
                } else {
                    // a middle line
                    0..self.width
                };

                let result = MatchResult {
                    range,
                    label: label.clone(),
                };

                let matches = self.by_line.entry(idx).or_insert_with(|| vec![]);
                matches.push(result);

                self.dirty_results.add(idx);
            }
        }
    }

    fn update_search(&mut self, is_initial_run: bool) {
        let bar_pos = self.compute_search_row();
        let dirty_rows =
            dirty_rows_for_search_refresh(self.by_line.keys().copied(), self.last_bar_pos, bar_pos);
        self.dirty_results.add_set(&dirty_rows);

        self.results.clear();
        self.by_line.clear();
        self.result_pos.take();

        if !self.pattern.is_empty() {
            let pane: Arc<dyn Pane> = self.delegate.clone();
            let window = self.window.clone();
            let pattern = self.pattern.clone();
            let scope = self.args.scope_lines;
            let viewport = self.viewport;
            promise::spawn::spawn(async move {
                let dims = pane.get_dimensions();
                let scope = scope.unwrap_or(1000).max(dims.viewport_rows);
                let top = viewport.unwrap_or(dims.physical_top);
                let range = top.saturating_sub(scope as StableRowIndex)
                    ..top + (dims.viewport_rows + scope) as StableRowIndex;
                let limit = None;
                let mut results = pane.search(pattern, range, limit).await?;
                results.sort();

                let pane_id = pane.pane_id();
                let mut results = Some(results);
                window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    let state = term_window.pane_state(pane_id);
                    if let Some(overlay) = state.overlay.as_ref() {
                        if let Some(search_overlay) =
                            overlay.pane.downcast_ref::<QuickSelectOverlay>()
                        {
                            let mut r = search_overlay.renderer.lock();
                            let Some(search_results) = results.take() else {
                                log::warn!(
                                    "quick-select search results already consumed for pane {pane_id}"
                                );
                                return;
                            };
                            r.results = search_results;
                            r.recompute_results();
                            let num_results = r.results.len();

                            if !r.results.is_empty() {
                                match &r.viewport {
                                    Some(y) if is_initial_run => {
                                        r.result_pos = r
                                            .results
                                            .iter()
                                            .position(|result| result.start_y >= *y);
                                    }
                                    _ => {
                                        r.activate_match_number(num_results - 1);
                                    }
                                }
                            } else {
                                if !is_initial_run {
                                    r.set_viewport(None);
                                }
                                r.clear_selection();
                            }
                        }
                    }
                })));
                anyhow::Result::<()>::Ok(())
            })
            .detach();
        } else {
            if !is_initial_run {
                self.set_viewport(None);
            }
            self.clear_selection();
        }
    }

    fn clear_selection(&mut self) {
        let pane_id = self.delegate.pane_id();
        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let mut selection = term_window.selection(pane_id);
                selection.origin.take();
                selection.range.take();
            })));
    }

    fn select_and_copy_match_number(&mut self, n: usize, paste: bool) {
        let Some(result) = self.results.get(n).cloned() else {
            return;
        };

        let pane_id = self.delegate.pane_id();
        let action = self.args.action.clone();
        let skip_action_on_paste = self.args.skip_action_on_paste;
        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let Some(mux) = mux::Mux::try_get() else {
                    log::warn!("cannot quick-select pane {pane_id}: mux is no longer active");
                    return;
                };
                if let Some(pane) = mux.get_pane(pane_id) {
                    {
                        let mut selection = term_window.selection(pane_id);
                        let start = SelectionCoordinate::x_y(result.start_x, result.start_y);
                        selection.origin = Some(start);
                        selection.range = Some(SelectionRange {
                            start,
                            // inclusive range for selection, but the result
                            // range is exclusive
                            end: SelectionCoordinate::x_y(
                                result.end_x.saturating_sub(1),
                                result.end_y,
                            ),
                        });
                        // Ensure that selection doesn't get invalidated when
                        // the overlay is closed
                        selection.seqno = pane.get_current_seqno();
                    }

                    let text = term_window.selection_text(&pane);
                    if !text.is_empty() {
                        if paste {
                            let _ = pane.send_paste(&text);
                        }
                        if let Some(action) = action {
                            if !paste || !skip_action_on_paste {
                                let _ = term_window.perform_key_assignment(&pane, &action);
                            }
                        } else {
                            term_window.copy_to_clipboard(
                                ClipboardCopyDestination::ClipboardAndPrimarySelection,
                                text,
                            );
                        }
                    }
                }
            })));
    }

    fn activate_match_number(&mut self, n: usize) {
        if let Some(result) = self.results.get(n) {
            self.result_pos.replace(n);
            let start_y = result.start_y;
            self.set_viewport(Some(start_y));
        }
    }
}
