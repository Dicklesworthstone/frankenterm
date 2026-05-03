use crate::selection::{
    Selection, SelectionCoordinate, SelectionMode, SelectionRange, SelectionX, SmartSelectionPick,
};
use crate::smart_selection_a11y::emit_smart_selection_pick;
use mux::pane::{LogicalLine, Pane, PaneId};
use std::cell::RefMut;
use std::sync::Arc;
use termwiz::surface::Line;
use wezterm_term::StableRowIndex;
use window::WindowOps;

/// Emit the AT-tree announcement for a picked smart-selection span.
/// Called from the `SelectionMode::Word` and `SelectionMode::Line`
/// mouse-handler branches after `smart_or_word_around` /
/// `smart_or_line_around` resolves to a smart pattern. No-op when
/// the legacy word- / line-boundary fallback fired (pick is `None`)
/// so screen readers stay quiet on plain word / line picks
/// (ft-cnil8.4 / ft-weglh / ft-t5j0a).
fn announce_pick_if_smart(pick: Option<SmartSelectionPick>) {
    if let Some(p) = pick {
        emit_smart_selection_pick(p.kind, &p.text);
    }
}

impl super::TermWindow {
    pub fn selection(&self, pane_id: PaneId) -> RefMut<'_, Selection> {
        RefMut::map(self.pane_state(pane_id), |state| &mut state.selection)
    }

    /// Returns the selection region as a series of Line
    pub fn selection_lines(&self, pane: &Arc<dyn Pane>) -> Vec<Line> {
        let mut result = vec![];

        let rectangular = self.selection(pane.pane_id()).rectangular;
        if let Some(sel) = self
            .selection(pane.pane_id())
            .range
            .as_ref()
            .map(|r| r.normalize())
        {
            let mut last_was_wrapped = false;
            let first_row = sel.rows().start;
            let last_row = sel.rows().end;

            for line in pane.get_logical_lines(sel.rows()) {
                if result.is_empty() || !last_was_wrapped {
                    result.push(Line::with_width(0, line.physical_lines[0].current_seqno()));
                }
                let last_idx = line.physical_lines.len().saturating_sub(1);
                for (idx, phys) in line.physical_lines.iter().enumerate() {
                    let this_row = line.first_row + idx as StableRowIndex;
                    if this_row >= first_row && this_row < last_row {
                        let last_phys_idx = phys.len().saturating_sub(1);
                        let cols = sel.cols_for_row(this_row, rectangular);
                        let last_col_idx = cols.end.saturating_sub(1).min(last_phys_idx);
                        let mut col_span = phys.columns_as_line(cols);
                        let seqno = col_span.current_seqno();
                        // Only trim trailing whitespace if we are the last line
                        // in a wrapped sequence
                        if idx == last_idx {
                            col_span.prune_trailing_blanks(seqno);
                        }

                        result
                            .last_mut()
                            .map(|line| line.append_line(col_span, seqno));

                        last_was_wrapped = last_col_idx == last_phys_idx
                            && phys
                                .get_cell(last_col_idx)
                                .map(|c| c.attrs().wrapped())
                                .unwrap_or(false);
                    }
                }
            }
        }

        result
    }

    /// Returns the selection text only
    pub fn selection_text(&self, pane: &Arc<dyn Pane>) -> String {
        let (rectangular, sel) = {
            let selection = self.selection(pane.pane_id());
            let Some(sel) = selection.range.as_ref().map(|r| r.normalize()) else {
                return String::new();
            };
            (selection.rectangular, sel)
        };

        selected_text_from_logical_lines(&pane.get_logical_lines(sel.rows()), sel, rectangular)
    }

    pub fn clear_selection(&mut self, pane: &Arc<dyn Pane>) {
        let mut selection = self.selection(pane.pane_id());
        selection.clear();
        selection.seqno = pane.get_current_seqno();
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    pub fn extend_selection_at_mouse_cursor(&mut self, mode: SelectionMode, pane: &Arc<dyn Pane>) {
        self.selection(pane.pane_id()).seqno = pane.get_current_seqno();
        let (position, y) = match self.pane_state(pane.pane_id()).mouse_terminal_coords {
            Some(coords) => coords,
            None => return,
        };
        let x = position.column;
        match mode {
            SelectionMode::Cell | SelectionMode::Block => {
                // Origin is the cell in which the selection action started. E.g. the cell
                // that had the mouse over it when the left mouse button was pressed
                let origin = self
                    .selection(pane.pane_id())
                    .origin
                    .unwrap_or(SelectionCoordinate::x_y(x, y));
                self.selection(pane.pane_id()).origin = Some(origin);
                self.selection(pane.pane_id()).rectangular = mode == SelectionMode::Block;

                // Compute the start and end horizontall cell of the selection.
                // The selection extent depends on the mouse cursor position in relation
                // to the origin.
                let (start_x, end_x) = if mode == SelectionMode::Block {
                    if x >= origin.x {
                        // If the selection is extending forwards from the origin,
                        // it includes the origin
                        (origin.x, SelectionX::Cell(x).saturating_sub(1))
                    } else {
                        // If the selection is extending backwards from the origin,
                        // it doesn't include the origin
                        (origin.x.saturating_sub(1), SelectionX::Cell(x))
                    }
                } else {
                    if (x >= origin.x && y == origin.y) || y > origin.y {
                        // If the selection is extending forwards from the origin, it includes the
                        // origin and doesn't include the cell under the cursor. Note that the
                        // reported cell here is offset by -50% from the real cell you see on the
                        // screen, so this causes a visual cell on the screen to be selected when
                        // the mouse moves over 50% of its width, which effectively means the next
                        // cell is being reported here, hence it's excluded
                        (origin.x, SelectionX::Cell(x).saturating_sub(1))
                    } else {
                        // If the selection is extending backwards from the origin, it doesn't
                        // include the origin and includes the cell under the cursor, which has
                        // the same effect as described above when going backwards
                        (origin.x.saturating_sub(1), SelectionX::Cell(x))
                    }
                };

                self.selection(pane.pane_id()).range =
                    if mode == SelectionMode::Block && origin.x == x {
                        // Ignore rectangle selections with a width of zero
                        None
                    } else if origin.x != x || origin.y != y {
                        // Only considers a selection if the cursor moved from the origin point
                        Some(
                            SelectionRange::start(SelectionCoordinate {
                                x: start_x,
                                y: origin.y,
                            })
                            .extend(SelectionCoordinate { x: end_x, y }),
                        )
                    } else {
                        None
                    };
            }
            SelectionMode::Word => {
                let (end_word, end_pick) =
                    SelectionRange::smart_or_word_around(SelectionCoordinate::x_y(x, y), &**pane);

                let start_coord = self
                    .selection(pane.pane_id())
                    .origin
                    .clone()
                    .unwrap_or(end_word.start);
                // Anchor-side pick is intentionally discarded: the
                // user gets one selection, and the announcement
                // tracks the cursor (moving endpoint) so screen
                // readers don't double-fire on drag.
                let (start_word, _) = SelectionRange::smart_or_word_around(start_coord, &**pane);

                let selection_range = start_word.extend_with(end_word);
                self.selection(pane.pane_id()).range = Some(selection_range);
                self.selection(pane.pane_id()).rectangular = false;
                announce_pick_if_smart(end_pick);
            }
            SelectionMode::Line => {
                let (end_line, end_pick) =
                    SelectionRange::smart_or_line_around(SelectionCoordinate::x_y(x, y), &**pane);

                let start_coord = self
                    .selection(pane.pane_id())
                    .origin
                    .clone()
                    .unwrap_or(end_line.start);
                // Anchor-side pick is intentionally discarded so a
                // drag-select doesn't double-fire the announcement;
                // the cursor (moving endpoint) drives the AT cue.
                let (start_line, _) = SelectionRange::smart_or_line_around(start_coord, &**pane);

                let selection_range = start_line.extend_with(end_line);
                self.selection(pane.pane_id()).range = Some(selection_range);
                self.selection(pane.pane_id()).rectangular = false;
                announce_pick_if_smart(end_pick);
            }
            SelectionMode::SemanticZone => {
                let end_word = SelectionRange::zone_around(SelectionCoordinate::x_y(x, y), &**pane);

                let start_coord = self
                    .selection(pane.pane_id())
                    .origin
                    .clone()
                    .unwrap_or(end_word.start);
                let start_word = SelectionRange::zone_around(start_coord, &**pane);

                let selection_range = start_word.extend_with(end_word);
                self.selection(pane.pane_id()).range = Some(selection_range);
                self.selection(pane.pane_id()).rectangular = false;
            }
        }

        let dims = pane.get_dimensions();

        // Scroll viewport when mouse mouves out of its vertical bounds
        if position.row == 0 && position.y_pixel_offset < 0 {
            self.set_viewport(pane.pane_id(), Some(y.saturating_sub(1)), dims);
        } else if position.row >= dims.viewport_rows as i64 {
            let top = self
                .get_viewport(pane.pane_id())
                .unwrap_or(dims.physical_top);
            self.set_viewport(pane.pane_id(), Some(top + 1), dims);
        }

        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    pub fn select_text_at_mouse_cursor(&mut self, mode: SelectionMode, pane: &Arc<dyn Pane>) {
        let (x, y) = match self.pane_state(pane.pane_id()).mouse_terminal_coords {
            Some(coords) => (coords.0.column, coords.1),
            None => return,
        };
        match mode {
            SelectionMode::Line => {
                let start = SelectionCoordinate::x_y(x, y);
                let (selection_range, pick) = SelectionRange::smart_or_line_around(start, &**pane);

                self.selection(pane.pane_id()).origin = Some(start);
                self.selection(pane.pane_id()).range = Some(selection_range);
                self.selection(pane.pane_id()).rectangular = false;
                announce_pick_if_smart(pick);
            }
            SelectionMode::Word => {
                let (selection_range, pick) =
                    SelectionRange::smart_or_word_around(SelectionCoordinate::x_y(x, y), &**pane);

                self.selection(pane.pane_id()).origin = Some(selection_range.start);
                self.selection(pane.pane_id()).range = Some(selection_range);
                self.selection(pane.pane_id()).rectangular = false;
                announce_pick_if_smart(pick);
            }
            SelectionMode::SemanticZone => {
                let selection_range =
                    SelectionRange::zone_around(SelectionCoordinate::x_y(x, y), &**pane);

                self.selection(pane.pane_id()).origin = Some(selection_range.start);
                self.selection(pane.pane_id()).range = Some(selection_range);
                self.selection(pane.pane_id()).rectangular = false;
            }
            SelectionMode::Cell | SelectionMode::Block => {
                self.selection(pane.pane_id())
                    .begin(SelectionCoordinate::x_y(x, y));
                self.selection(pane.pane_id()).rectangular = mode == SelectionMode::Block;
            }
        }

        self.selection(pane.pane_id()).seqno = pane.get_current_seqno();
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }
}

fn selected_text_from_logical_lines(
    logical_lines: &[LogicalLine],
    sel: SelectionRange,
    rectangular: bool,
) -> String {
    let mut s = String::new();
    let sel = sel.normalize();
    let mut last_was_wrapped = false;
    let first_row = sel.rows().start;
    let last_row = sel.rows().end;

    for line in logical_lines {
        if !s.is_empty() && !last_was_wrapped {
            s.push('\n');
        }
        let last_idx = line.physical_lines.len().saturating_sub(1);
        for (idx, phys) in line.physical_lines.iter().enumerate() {
            let this_row = line.first_row + idx as StableRowIndex;
            if this_row >= first_row && this_row < last_row {
                let last_phys_idx = phys.len().saturating_sub(1);
                let cols = sel.cols_for_row(this_row, rectangular);
                let last_col_idx = cols.end.saturating_sub(1).min(last_phys_idx);
                let col_span = phys.columns_as_str(cols);
                // Only trim trailing whitespace if we are the last line
                // in a wrapped sequence
                if idx == last_idx {
                    s.push_str(col_span.trim_end());
                } else {
                    s.push_str(&col_span);
                }

                last_was_wrapped = last_col_idx == last_phys_idx
                    && phys
                        .get_cell(last_col_idx)
                        .map(|c| c.attrs().wrapped())
                        .unwrap_or(false);
            }
        }
    }

    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smart_selection_a11y::shared_smart_selection_recorder;
    use frankenterm_core::a11y_tree::{AccessibilityEvent, AnnouncePriority};
    use frankenterm_core::smart_selection::SelectionPatternKind;
    use proptest::prelude::*;
    use termwiz::cell::{CellAttributes, unicode_column_width};
    use termwiz::surface::SEQ_ZERO;

    fn logical_line_from_physical(physical_lines: Vec<Line>) -> LogicalLine {
        let logical_text = physical_lines
            .iter()
            .map(Line::as_str)
            .collect::<Vec<_>>()
            .join("");
        LogicalLine {
            physical_lines,
            logical: Line::from_text(&logical_text, &CellAttributes::default(), SEQ_ZERO, None),
            first_row: 0,
        }
    }

    fn arb_selection_glyph() -> impl Strategy<Value = &'static str> {
        prop_oneof![
            Just("A"),
            Just("z"),
            Just("0"),
            Just("-"),
            Just("\u{00e9}"),
            Just("e\u{0301}"),
            Just("a\u{0308}"),
            Just("\u{03bb}"),
            Just("\u{4e2d}"),
            Just("\u{754c}"),
            Just("\u{8a9e}"),
            Just("\u{1f480}"),
            Just("\u{1f9ea}"),
        ]
    }

    fn arb_selection_payload() -> impl Strategy<Value = String> {
        proptest::collection::vec(arb_selection_glyph(), 1..32).prop_map(|glyphs| glyphs.concat())
    }

    fn arb_wrapped_selection_payload() -> impl Strategy<Value = (String, String)> {
        proptest::collection::vec(arb_selection_glyph(), 2..32)
            .prop_flat_map(|glyphs| {
                let split_range = 1..glyphs.len();
                (Just(glyphs), split_range)
            })
            .prop_map(|(glyphs, split)| {
                let head = glyphs[..split].concat();
                let tail = glyphs[split..].concat();
                (head, tail)
            })
    }

    fn arb_double_width_anchor() -> impl Strategy<Value = (String, &'static str, String)> {
        (
            proptest::collection::vec(arb_selection_glyph(), 0..12),
            prop_oneof![
                Just("\u{4e2d}"),
                Just("\u{754c}"),
                Just("\u{8a9e}"),
                Just("\u{1f480}"),
                Just("\u{1f9ea}"),
            ],
            proptest::collection::vec(arb_selection_glyph(), 0..12),
        )
            .prop_map(|(prefix, wide, suffix)| (prefix.concat(), wide, suffix.concat()))
    }

    fn arb_selection_glyphs() -> impl Strategy<Value = Vec<&'static str>> {
        proptest::collection::vec(arb_selection_glyph(), 1..32)
    }

    fn selected_text_for_range(line: Line, start_col: usize, end_col: usize) -> String {
        let selected = SelectionRange::start(SelectionCoordinate::x_y(start_col, 0))
            .extend(SelectionCoordinate::x_y(end_col, 0));
        selected_text_from_logical_lines(&[logical_line_from_physical(vec![line])], selected, false)
    }

    fn wrapped_logical_line_from_glyphs(
        glyphs: &[&'static str],
        width: usize,
    ) -> (LogicalLine, Vec<(StableRowIndex, usize, usize)>) {
        let attrs = CellAttributes::default();
        let mut physical_lines = Vec::new();
        let mut mappings = Vec::with_capacity(glyphs.len());
        let mut current_text = String::new();
        let mut current_col = 0usize;
        let mut current_row = 0isize;

        for glyph in glyphs {
            let glyph_width = unicode_column_width(glyph, None).max(1);
            if current_col > 0 && current_col + glyph_width > width {
                physical_lines.push(Line::from_text(&current_text, &attrs, SEQ_ZERO, None));
                current_text.clear();
                current_col = 0;
                current_row += 1;
            }

            mappings.push((current_row, current_col, glyph_width));
            current_text.push_str(glyph);
            current_col += glyph_width;
        }

        physical_lines.push(Line::from_text(&current_text, &attrs, SEQ_ZERO, None));
        let last_idx = physical_lines.len().saturating_sub(1);
        for line in physical_lines.iter_mut().take(last_idx) {
            line.set_last_cell_was_wrapped(true, SEQ_ZERO);
        }

        (logical_line_from_physical(physical_lines), mappings)
    }

    fn select_glyph_span_after_wrapping(
        glyphs: &[&'static str],
        width: usize,
        start_idx: usize,
        end_idx: usize,
    ) -> String {
        let (logical, mappings) = wrapped_logical_line_from_glyphs(glyphs, width);
        let (start_row, start_col, _) = mappings[start_idx];
        let (end_row, end_col, end_width) = mappings[end_idx];
        let selected =
            SelectionRange::start(SelectionCoordinate::x_y(start_col, start_row)).extend(
                SelectionCoordinate::x_y(end_col + end_width.saturating_sub(1), end_row),
            );

        selected_text_from_logical_lines(&[logical], selected, false)
    }

    #[test]
    fn selection_clipboard_text_preserves_wide_and_combining_glyphs() {
        let payload = "A界e\u{0301}\u{1f480}Z";
        let line = Line::from_text(payload, &CellAttributes::default(), SEQ_ZERO, None);
        assert!(
            line.len() > payload.chars().count(),
            "fixture must include at least one multi-column glyph"
        );
        let selected = SelectionRange::start(SelectionCoordinate::x_y(0, 0))
            .extend(SelectionCoordinate::x_y(line.len().saturating_sub(1), 0));

        let text = selected_text_from_logical_lines(
            &[logical_line_from_physical(vec![line])],
            selected,
            false,
        );

        assert_eq!(text, payload);
    }

    #[test]
    fn selection_clipboard_text_preserves_unicode_across_wrapped_rows() {
        let attrs = CellAttributes::default();
        let mut wrapped = Line::from_text_with_wrapped_last_col("A界", &attrs, SEQ_ZERO);
        let tail_payload = "e\u{0301}\u{1f480}Z";
        let tail = Line::from_text(tail_payload, &attrs, SEQ_ZERO, None);
        let selected = SelectionRange::start(SelectionCoordinate::x_y(0, 0))
            .extend(SelectionCoordinate::x_y(tail.len().saturating_sub(1), 1));
        wrapped.set_last_cell_was_wrapped(true, SEQ_ZERO);

        let text = selected_text_from_logical_lines(
            &[logical_line_from_physical(vec![wrapped, tail])],
            selected,
            false,
        );

        assert_eq!(text, format!("A界{tail_payload}"));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn selection_clipboard_roundtrip_preserves_generated_unicode_glyphs(
            payload in arb_selection_payload()
        ) {
            let attrs = CellAttributes::default();
            let line = Line::from_text(&payload, &attrs, SEQ_ZERO, None);
            let first_copy = selected_text_for_range(line, 0, payload.len().max(1));
            let copied_line = Line::from_text(&first_copy, &attrs, SEQ_ZERO, None);
            let second_copy = selected_text_for_range(copied_line, 0, first_copy.len().max(1));

            prop_assert_eq!(&first_copy, &payload);
            prop_assert_eq!(&second_copy, &payload);
        }

        #[test]
        fn wrapped_selection_clipboard_roundtrip_preserves_generated_unicode_glyphs(
            (head, tail) in arb_wrapped_selection_payload()
        ) {
            let attrs = CellAttributes::default();
            let mut wrapped = Line::from_text_with_wrapped_last_col(&head, &attrs, SEQ_ZERO);
            let tail_line = Line::from_text(&tail, &attrs, SEQ_ZERO, None);
            let selected = SelectionRange::start(SelectionCoordinate::x_y(0, 0))
                .extend(SelectionCoordinate::x_y(tail_line.len().saturating_sub(1), 1));
            wrapped.set_last_cell_was_wrapped(true, SEQ_ZERO);

            let copied = selected_text_from_logical_lines(
                &[logical_line_from_physical(vec![wrapped, tail_line])],
                selected,
                false,
            );
            let expected = format!("{head}{tail}");
            let copied_line = Line::from_text(&copied, &attrs, SEQ_ZERO, None);
            let recopied = selected_text_for_range(copied_line, 0, copied.len().max(1));

            prop_assert_eq!(&copied, &expected);
            prop_assert_eq!(&recopied, &expected);
        }

        #[test]
        fn selection_clipboard_double_width_boundaries_never_emit_half_glyphs(
            (prefix, wide, suffix) in arb_double_width_anchor()
        ) {
            let attrs = CellAttributes::default();
            let payload = format!("{prefix}{wide}{suffix}");
            let line = Line::from_text(&payload, &attrs, SEQ_ZERO, None);
            let wide_start = unicode_column_width(&prefix, None);
            let wide_width = unicode_column_width(wide, None);
            prop_assert_eq!(wide_width, 2, "fixture must generate double-width anchors");

            let selected_wide =
                selected_text_for_range(line.clone(), wide_start, wide_start + wide_width - 1);
            let selected_from_inside =
                selected_text_for_range(line, wide_start + 1, wide_start + wide_width - 1);

            prop_assert_eq!(selected_wide, wide);
            prop_assert!(
                selected_from_inside.is_empty(),
                "selection starting inside a double-width glyph must not emit a partial glyph"
            );
        }

        #[test]
        fn selection_text_is_stable_when_same_logical_span_is_rewrapped_by_resize(
            glyphs in arb_selection_glyphs(),
            first_width in 2usize..=16,
            second_width in 2usize..=16,
        ) {
            let last_idx = glyphs.len() - 1;
            let start_idx = last_idx / 3;
            let end_idx = (start_idx + (glyphs.len().max(2) / 2)).min(last_idx);
            let expected = glyphs[start_idx..=end_idx].concat();

            let first = select_glyph_span_after_wrapping(&glyphs, first_width, start_idx, end_idx);
            let second = select_glyph_span_after_wrapping(&glyphs, second_width, start_idx, end_idx);

            prop_assert_eq!(
                &first,
                &expected,
                "selection changed while projecting logical span into first resized width={} glyphs={:?}",
                first_width,
                glyphs
            );
            prop_assert_eq!(
                &second,
                &expected,
                "selection changed while projecting logical span into second resized width={} glyphs={:?}",
                second_width,
                glyphs
            );
            prop_assert_eq!(
                &first,
                &second,
                "selection text must survive rewrap from width {} to {} for glyphs={:?}",
                first_width,
                second_width,
                glyphs
            );
        }
    }

    #[test]
    fn announce_pick_if_smart_emits_mouse_selection_announcement() {
        let sentinel = "https://example.com/gui-mouse-selection-sentinel";
        let _ = shared_smart_selection_recorder().take();

        announce_pick_if_smart(Some(SmartSelectionPick {
            kind: SelectionPatternKind::Url,
            text: sentinel.to_string(),
        }));

        let event = shared_smart_selection_recorder()
            .find_announcement_for_kind(SelectionPatternKind::Url)
            .expect("URL announcement from GUI mouse selection bridge");

        match event {
            AccessibilityEvent::AnnounceMessage {
                value, priority, ..
            } => {
                assert_eq!(value, format!("URL selected: {sentinel}"));
                assert_eq!(priority, AnnouncePriority::Polite);
            }
            other => panic!("expected AnnounceMessage, got {other:?}"),
        }

        let _ = shared_smart_selection_recorder().take();
    }
}
