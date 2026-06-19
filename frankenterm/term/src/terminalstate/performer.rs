use crate::terminal::{Alert, Progress};
use crate::terminalstate::{
    default_color_map, CharSet, MouseEncoding, TabStop, UnicodeVersionStackEntry,
};
use crate::{ClipboardSelection, Position, TerminalState, VisibleRowIndex, DCS, ST};
use finl_unicode::grapheme_clusters::Graphemes;
use frankenterm_bidi::ParagraphDirectionHint;
use frankenterm_cell::{
    grapheme_column_width, is_white_space_grapheme, Cell, CellAttributes, SemanticType,
};
use frankenterm_escape_parser::csi::{
    CharacterPath, EraseInDisplay, Keyboard, KittyKeyboardFlags, KittyKeyboardMode,
};
use frankenterm_escape_parser::osc::{
    ChangeColorPair, ColorOrQuery, FinalTermSemanticPrompt, ITermProprietary,
    ITermUnicodeVersionOp, Selection,
};
use frankenterm_escape_parser::{
    Action, ControlCode, DeviceControlMode, Esc, EscCode, OperatingSystemCommand, CSI,
};
use log::{debug, error};
use num_traits::FromPrimitive;
use ordered_float::NotNan;
use std::fmt::Write;
use std::io::Write as _;
use std::ops::{Deref, DerefMut};
use termwiz::input::KeyboardEncoding;
use unicode_normalization::{is_nfc_quick, IsNormalized, UnicodeNormalization};
use url::Url;

/// A helper struct for implementing `vtparse::VTActor` while compartmentalizing
/// the terminal state and the embedding/host terminal interface
pub(crate) struct Performer<'a> {
    pub state: &'a mut TerminalState,
    print: String,
}

#[cfg(test)]
static FORCE_SCALAR_PRINTABLE_ASCII_SCAN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

impl<'a> Deref for Performer<'a> {
    type Target = TerminalState;

    fn deref(&self) -> &TerminalState {
        self.state
    }
}

impl<'a> DerefMut for Performer<'a> {
    fn deref_mut(&mut self) -> &mut TerminalState {
        &mut self.state
    }
}

impl<'a> Drop for Performer<'a> {
    fn drop(&mut self) {
        self.flush_print();
    }
}

impl<'a> Performer<'a> {
    pub fn new(state: &'a mut TerminalState) -> Self {
        Self {
            state,
            print: String::new(),
        }
    }

    fn recluster_at_cursor(&mut self, suffix: &str, require_prior_trailing_zwj: bool) -> bool {
        let cursor_x = self.cursor.x;
        let cursor_y = self.cursor.y;
        let seqno = self.seqno;
        let unicode_version = self.unicode_version.clone();
        let pending_wrap = self.wrap_next;
        let dec_auto_wrap = self.dec_auto_wrap;
        let right_margin = self.left_and_right_margins.end;

        let (cursor_x, wrap_next) = {
            let screen = self.screen_mut();
            let phys = screen.phys_row(cursor_y);
            let line = screen.line_mut(phys);
            let mut candidate = None;

            for cell in line.visible_cells() {
                let cell_index = cell.cell_index();
                if pending_wrap && cell_index == cursor_x {
                    candidate = Some((
                        cell_index,
                        cell.str().to_string(),
                        cell.width(),
                        cell.attrs().clone(),
                    ));
                    break;
                }
                if cell_index < cursor_x {
                    candidate = Some((
                        cell_index,
                        cell.str().to_string(),
                        cell.width(),
                        cell.attrs().clone(),
                    ));
                } else {
                    break;
                }
            }

            let Some((idx, text, width, attrs)) = candidate else {
                return false;
            };
            let old_end = idx.saturating_add(width);
            let joins_at_cursor = old_end == cursor_x || (pending_wrap && idx == cursor_x);
            if !joins_at_cursor {
                return false;
            }
            if require_prior_trailing_zwj && !text.ends_with('\u{200d}') {
                return false;
            }

            let combined = format!("{text}{suffix}");
            if Graphemes::new(combined.as_str()).count() != 1 {
                return false;
            }

            let combined_width = grapheme_column_width(&combined, Some(&unicode_version));
            if !(width..=2).contains(&combined_width) {
                return false;
            }

            line.set_cell_grapheme(idx, &combined, combined_width, attrs, seqno);

            let next_x = idx.saturating_add(combined_width);
            if next_x >= right_margin {
                (idx, dec_auto_wrap)
            } else {
                (next_x, false)
            }
        };

        self.cursor.x = cursor_x;
        self.wrap_next = wrap_next;
        true
    }

    fn active_charset(&self) -> CharSet {
        if self.shift_out {
            self.g1_charset
        } else {
            self.g0_charset
        }
    }

    #[inline]
    fn printable_ascii_prefix_len(bytes: &[u8]) -> usize {
        #[cfg(test)]
        if FORCE_SCALAR_PRINTABLE_ASCII_SCAN.load(std::sync::atomic::Ordering::Relaxed) {
            return Self::printable_ascii_prefix_len_scalar(bytes);
        }

        #[cfg(feature = "bench-scalar-vte-scan")]
        {
            Self::printable_ascii_prefix_len_scalar(bytes)
        }

        #[cfg(all(not(feature = "bench-scalar-vte-scan"), target_pointer_width = "64"))]
        {
            Self::printable_ascii_prefix_len_swar(bytes)
        }

        #[cfg(all(
            not(feature = "bench-scalar-vte-scan"),
            not(target_pointer_width = "64")
        ))]
        {
            Self::printable_ascii_prefix_len_scalar(bytes)
        }
    }

    #[cfg(any(
        test,
        all(target_pointer_width = "64", not(feature = "bench-scalar-vte-scan"))
    ))]
    #[inline]
    fn printable_ascii_prefix_len_swar(bytes: &[u8]) -> usize {
        let mut offset = 0;
        while offset + 8 <= bytes.len() {
            let mut chunk = [0u8; 8];
            chunk.copy_from_slice(&bytes[offset..offset + 8]);
            let word = u64::from_ne_bytes(chunk);
            if Self::swar_non_printable_ascii_mask(word) != 0 {
                return offset
                    + Self::printable_ascii_prefix_len_scalar(&bytes[offset..offset + 8]);
            }
            offset += 8;
        }

        offset + Self::printable_ascii_prefix_len_scalar(&bytes[offset..])
    }

    #[cfg(any(
        test,
        all(target_pointer_width = "64", not(feature = "bench-scalar-vte-scan"))
    ))]
    #[inline]
    fn swar_non_printable_ascii_mask(word: u64) -> u64 {
        const ONES: u64 = 0x0101_0101_0101_0101;
        const HIGHS: u64 = 0x8080_8080_8080_8080;
        let below_space = word.wrapping_sub(ONES * 0x20) & !word & HIGHS;
        let above_tilde = (word.wrapping_add(ONES) | word) & HIGHS;
        below_space | above_tilde
    }

    #[inline]
    fn printable_ascii_prefix_len_scalar(bytes: &[u8]) -> usize {
        bytes
            .iter()
            .position(|byte| !matches!(*byte, 0x20..=0x7e))
            .unwrap_or(bytes.len())
    }

    fn flush_ascii_print(&mut self, text: &str, seqno: usize) -> bool {
        if self.insert
            || self.wrap_next
            || self.active_charset() != CharSet::Ascii
            || Self::printable_ascii_prefix_len(text.as_bytes()) != text.len()
        {
            return false;
        }

        let right_margin = self.left_and_right_margins.end;
        let start_x = self.cursor.x;
        let Some(available_cols) = right_margin.checked_sub(start_x) else {
            return false;
        };
        if text.is_empty() || text.len() > available_cols {
            return false;
        }

        let y = self.cursor.y;
        let pen = self.pen.clone();
        let screen = self.screen_mut();
        for (offset, _) in text.bytes().enumerate() {
            let x = start_x + offset;
            let grapheme = &text[offset..offset + 1];
            screen.set_cell_grapheme(x, y, grapheme, 1, pen.clone(), seqno);
        }

        let next_x = start_x + text.len();
        if next_x >= right_margin {
            self.cursor.x = right_margin.saturating_sub(1);
            self.wrap_next = self.dec_auto_wrap;
        } else {
            self.cursor.x = next_x;
            self.wrap_next = false;
        }
        true
    }

    /// Apply character set related remapping to the input glyph if required
    fn remap_grapheme<'b>(&self, g: &'b str) -> &'b str {
        if (self.shift_out && self.g1_charset == CharSet::DecLineDrawing)
            || (!self.shift_out && self.g0_charset == CharSet::DecLineDrawing)
        {
            match g {
                "`" => "◆",
                "a" => "▒",
                "b" => "␉",
                "c" => "␌",
                "d" => "␍",
                "e" => "␊",
                "f" => "°",
                "g" => "±",
                "h" => "␤",
                "i" => "␋",
                "j" => "┘",
                "k" => "┐",
                "l" => "┌",
                "m" => "└",
                "n" => "┼",
                "o" => "⎺",
                "p" => "⎻",
                "q" => "─",
                "r" => "⎼",
                "s" => "⎽",
                "t" => "├",
                "u" => "┤",
                "v" => "┴",
                "w" => "┬",
                "x" => "│",
                "y" => "≤",
                "z" => "≥",
                "{" => "π",
                "|" => "≠",
                "}" => "£",
                "~" => "·",
                _ => g,
            }
        } else if (self.shift_out && self.g1_charset == CharSet::Uk)
            || (!self.shift_out && self.g0_charset == CharSet::Uk)
        {
            match g {
                "#" => "£",
                _ => g,
            }
        } else {
            g
        }
    }

    fn flush_print(&mut self) {
        if self.print.is_empty() {
            return;
        }

        let seqno = self.seqno;
        let mut p = std::mem::take(&mut self.print);
        let normalized: String;
        let text = if p.is_ascii() {
            p.as_str()
        } else if self.config.normalize_output_to_unicode_nfc()
            && is_nfc_quick(p.chars()) != IsNormalized::Yes
        {
            normalized = p.as_str().nfc().collect();
            normalized.as_str()
        } else {
            p.as_str()
        };

        if self.flush_ascii_print(text, seqno) {
            std::mem::swap(&mut self.print, &mut p);
            self.print.clear();
            return;
        }

        for g in Graphemes::new(text) {
            let g = self.remap_grapheme(g);

            let mut print_width = grapheme_column_width(g, Some(&self.unicode_version));
            if print_width == 0 {
                // We got a zero-width grapheme.

                // Relevant reading:
                // <https://github.com/wezterm/wezterm/issues/1422>
                // <https://github.com/wezterm/wezterm/issues/6637>
                // <https://github.com/harfbuzz/harfbuzz/issues/4279>
                // <https://www.unicode.org/faq/unsup_char.html#2>
                //
                // For White_Space we want to ensure that we display as a space.
                // Other non-printing, zero-width characters can be elided
                // to avoid presentation problems, but may introduce potential
                // weirdness elsewhere. For example, U+2068 is a BIDI control
                // character and will be elided by this logic. A consequence
                // of that is that when the user copies the surrounding text
                // from the terminal, that BIDI control will not be present.
                // We do not currently have a solution for that.
                if is_white_space_grapheme(g) {
                    // Ensure that White_Space shows as a space
                    print_width = 1;
                } else {
                    // FND-009 / INV-TERM-2: a zero-width grapheme (combining mark,
                    // diacritic) may continue the cluster of the cell to our left.
                    // When the whole byte stream arrives in one `advance_bytes`
                    // call, `Graphemes` batches base+mark into a single grapheme and
                    // the mark renders. When the mark arrives in a SEPARATE call
                    // (its base already committed — e.g. a PTY read split a cluster),
                    // it reaches us standalone and was being dropped, so the same
                    // bytes rendered differently depending on chunk boundaries
                    // (`a` then `U+0301` -> `a` instead of `á`). Attach it to the
                    // previous cell IFF it genuinely clusters with that cell's
                    // grapheme and the resulting cluster width is representable
                    // by a terminal cell: re-running `Graphemes` matches
                    // whole-buffer semantics exactly. Non-clustering zero-width
                    // controls (for example BIDI format chars) are still elided
                    // as before, while width-changing continuations such as VS16
                    // can now expand the prior cell through the same line setter
                    // used by normal double-width graphemes.
                    let attached = self.recluster_at_cursor(g, false);
                    if !attached {
                        log::trace!("Eliding zero-width grapheme {:?}", g);
                    }
                    continue;
                }
            }

            if g.len() > 1 && self.recluster_at_cursor(g, true) {
                continue;
            }

            if self.wrap_next {
                // Since we're implicitly moving the cursor to the next
                // line, we need to tag the current position as wrapped
                // so that we can correctly reflow it if the window is
                // resized.
                {
                    let y = self.cursor.y;
                    let is_conpty = self.state.enable_conpty_quirks;
                    let screen = self.screen_mut();
                    let y = screen.phys_row(y);

                    fn makes_sense_to_wrap(s: &str) -> bool {
                        let len = s.len();
                        match (len, s.chars().next()) {
                            (1, Some(c)) => c.is_alphanumeric() || c.is_ascii_punctuation(),
                            _ => true,
                        }
                    }

                    let should_mark_wrapped = !is_conpty
                        || screen
                            .line_mut(y)
                            .visible_cells()
                            .last()
                            .map(|cell| makes_sense_to_wrap(cell.str()))
                            .unwrap_or(false);
                    if should_mark_wrapped {
                        screen.line_mut(y).set_last_cell_was_wrapped(true, seqno);
                    }
                }
                self.new_line(true);
            }

            let x = self.cursor.x;
            let y = self.cursor.y;
            let width = self.left_and_right_margins.end;

            let pen = self.pen.clone();

            let next_x = x.saturating_add(print_width);
            let wrappable = next_x >= width;

            if self.insert {
                let margin = self.left_and_right_margins.end;
                let screen = self.screen_mut();
                for _ in x..next_x {
                    screen.insert_cell(x, y, margin, seqno);
                }
            }

            // Assign the cell
            log::trace!(
                "print x={} y={} print_width={} width={} cell={} {:?}",
                x,
                y,
                print_width,
                width,
                g,
                self.pen
            );
            self.screen_mut()
                .set_cell_grapheme(x, y, g, print_width, pen, seqno);

            if !wrappable {
                self.cursor.x = next_x;
                self.wrap_next = false;
            } else {
                self.wrap_next = self.dec_auto_wrap;
            }
        }

        std::mem::swap(&mut self.print, &mut p);
        self.print.clear();
    }

    /// ConPTY, at the time of writing, does something horrible to rewrite
    /// `ESC k TITLE ST` into something completely different and out-of-order,
    /// and critically, removes the ST.
    /// The result is that our hack to accumulate the tmux title gets stuck
    /// in a mode where all printable output is accumulated for the title.
    /// To combat this, we pop_tmux_title_state when we're obviously moving
    /// to different escape sequence parsing states.
    /// <https://github.com/wezterm/wezterm/issues/2442>
    fn pop_tmux_title_state(&mut self) {
        if let Some(title) = self.accumulating_title.take() {
            log::debug!("ST never received for pending tmux title escape sequence: {title:?}");
        }
    }

    pub fn perform(&mut self, action: Action) {
        debug!("perform {:?}", action);
        if self.suppress_initial_title_change {
            match &action {
                Action::OperatingSystemCommand(osc) => match **osc {
                    OperatingSystemCommand::SetIconNameAndWindowTitle(_) => {
                        debug!("suppressed {:?}", osc);
                        self.suppress_initial_title_change = false;
                        return;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        match action {
            Action::Print(c) => self.print(c),
            Action::PrintString(s) => self.print_string(&s),
            Action::Control(code) => self.control(code),
            Action::DeviceControl(ctrl) => self.device_control(ctrl),
            Action::OperatingSystemCommand(osc) => self.osc_dispatch(*osc),
            Action::Esc(esc) => self.esc_dispatch(esc),
            Action::CSI(csi) => self.csi_dispatch(csi),
            Action::Sixel(sixel) => {
                // FND-017: a sixel renders an image at the current cursor, so any
                // buffered text must be committed (and the cursor advanced) first
                // — exactly as the KittyImage arm below already does. Without this
                // flush, a preceding `Print` still sitting in `self.print` leaves
                // the cursor un-advanced, the sixel lands on the wrong column, and
                // the same bytes render differently depending on how they were
                // chunked across `advance_bytes` calls (the print buffer is flushed
                // at each call boundary by the Performer's Drop). Flushing here
                // makes sixel placement chunk-boundary-invariant.
                self.flush_print();
                self.sixel(sixel)
            }
            Action::XtGetTcap(names) => self.xt_get_tcap(names),
            Action::KittyImage(img) => {
                self.flush_print();
                if let Err(err) = self.kitty_img(*img) {
                    log::error!("kitty_img: {:#}", err);
                }
            }
        }
    }

    fn device_control(&mut self, ctrl: DeviceControlMode) {
        self.pop_tmux_title_state();
        match &ctrl {
            DeviceControlMode::ShortDeviceControl(s) => {
                match (s.byte, s.intermediates.as_slice()) {
                    (b'q', &[b'$']) => {
                        // DECRQSS - Request Status String
                        // https://vt100.net/docs/vt510-rm/DECRQSS.html
                        // The response is described here:
                        // https://vt100.net/docs/vt510-rm/DECRPSS.html
                        // but note that *that* text has the validity value
                        // inverted; there's a note about this in the xterm
                        // ctlseqs docs.
                        match s.data.as_slice() {
                            &[b'"', b'p'] => {
                                // DECSCL - select conformance level
                                write!(self.writer, "{}1$r65;1\"p{}", DCS, ST).ok();
                                self.writer.flush().ok();
                            }
                            &[b'r'] => {
                                // DECSTBM - top and bottom margins
                                let margins = self.top_and_bottom_margins.clone();
                                write!(
                                    self.writer,
                                    "{}1$r{};{}r{}",
                                    DCS,
                                    margins.start + 1,
                                    margins.end,
                                    ST
                                )
                                .ok();
                                self.writer.flush().ok();
                            }
                            &[b's'] => {
                                // DECSLRM - left and right margins
                                let margins = self.left_and_right_margins.clone();
                                write!(
                                    self.writer,
                                    "{}1$r{};{}s{}",
                                    DCS,
                                    margins.start + 1,
                                    margins.end,
                                    ST
                                )
                                .ok();
                                self.writer.flush().ok();
                            }
                            _ => {
                                if self.config.log_unknown_escape_sequences() {
                                    log::warn!("unhandled DECRQSS {:?}", s);
                                }
                                // Reply that the request is invalid
                                write!(self.writer, "{}0$r{}", DCS, ST).ok();
                                self.writer.flush().ok();
                            }
                        }
                    }
                    _ => {
                        if self.config.log_unknown_escape_sequences() {
                            log::warn!("unhandled {:?}", s);
                        }
                    }
                }
            }
            _ => match self.device_control_handler.as_mut() {
                Some(handler) => handler.handle_device_control(ctrl),
                None => {
                    if self.config.log_unknown_escape_sequences() {
                        log::warn!("unhandled {:?}", ctrl);
                    }
                }
            },
        }
    }

    /// Draw a character to the screen
    fn print(&mut self, c: char) {
        // We buffer up the chars to increase the chances of correctly grouping graphemes into cells
        let max_title_len = self.config.max_accumulating_title_len();
        if let Some(title) = self.accumulating_title.as_mut() {
            if title.len() < max_title_len {
                title.push(c);
            } else {
                // Title exceeded cap — discard accumulation to prevent unbounded growth
                // from malicious or malformed escape sequences.
                log::warn!(
                    "accumulating_title exceeded {} byte cap, discarding",
                    max_title_len
                );
                self.accumulating_title.take();
            }
        } else {
            self.print.push(c);
        }
    }

    fn print_string(&mut self, s: &str) {
        if self.accumulating_title.is_some() {
            for c in s.chars() {
                self.print(c);
            }
        } else {
            self.print.push_str(s);
        }
    }

    fn control(&mut self, control: ControlCode) {
        let seqno = self.seqno;
        self.pop_tmux_title_state();
        self.flush_print();
        match control {
            ControlCode::LineFeed | ControlCode::VerticalTab | ControlCode::FormFeed => {
                if self.left_and_right_margins.contains(&self.cursor.x) {
                    self.new_line(false);
                } else {
                    // Do move down, but don't trigger a scroll when we're
                    // outside of the left/right margins
                    let old_y = self.cursor.y;
                    let y = if old_y == self.top_and_bottom_margins.end - 1 {
                        old_y
                    } else {
                        (old_y + 1).min(self.screen().physical_rows as i64 - 1)
                    };
                    self.screen_mut().dirty_line(old_y, seqno);
                    self.screen_mut().dirty_line(y, seqno);
                    self.cursor.y = y;
                    self.wrap_next = false;
                }
                if self.newline_mode {
                    self.cursor.x = 0;
                    self.clear_semantic_attribute_due_to_movement();
                }
            }
            ControlCode::CarriageReturn => {
                if self.cursor.x >= self.left_and_right_margins.start {
                    self.cursor.x = self.left_and_right_margins.start;
                } else {
                    self.cursor.x = 0;
                }
                let y = self.cursor.y;
                self.wrap_next = false;
                self.clear_semantic_attribute_due_to_movement();
                self.screen_mut().dirty_line(y, seqno);
            }

            ControlCode::Backspace => {
                if self.reverse_wraparound_mode
                    && self.dec_auto_wrap
                    && self.cursor.x == self.left_and_right_margins.start
                    && self.cursor.y == self.top_and_bottom_margins.start
                {
                    // Backspace off the top-left wraps around to the bottom right
                    let x_pos = Position::Absolute(self.left_and_right_margins.end as i64 - 1);
                    let y_pos = Position::Absolute(self.top_and_bottom_margins.end - 1);
                    self.set_cursor_pos(&x_pos, &y_pos);
                } else if self.reverse_wraparound_mode
                    && self.dec_auto_wrap
                    && self.cursor.x <= self.left_and_right_margins.start
                {
                    // Backspace off the left wraps around to the prior line on the right
                    let x_pos = Position::Absolute(self.left_and_right_margins.end as i64 - 1);
                    let y_pos = Position::Relative(-1);
                    self.set_cursor_pos(&x_pos, &y_pos);
                } else if self.reverse_wraparound_mode
                    && self.dec_auto_wrap
                    && self.cursor.x == self.left_and_right_margins.end - 1
                    && self.wrap_next
                {
                    // If the cursor is in the last column and a character was
                    // just output and reverse-wraparound is on then backspace
                    // by 1 cancels the pending wrap.
                    self.wrap_next = false;
                } else if self.cursor.x == self.left_and_right_margins.start {
                    // Respect the left margin and don't BS outside it
                } else {
                    self.set_cursor_pos(&Position::Relative(-1), &Position::Relative(0));
                }
            }
            ControlCode::HorizontalTab => self.c0_horizontal_tab(),
            ControlCode::HTS => self.c1_hts(),
            ControlCode::IND => self.c1_index(),
            ControlCode::NEL => self.c1_nel(),
            ControlCode::Bell => {
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::Bell);
                } else {
                    log::info!("Ding! (this is the bell)");
                }
            }
            ControlCode::RI => self.c1_reverse_index(),

            // wezterm only supports UTF-8, so does not support the
            // DEC National Replacement Character Sets.  However, it does
            // support the DEC Special Graphics character set used by
            // numerous ncurses applications.  DEC Special Graphics can be
            // selected by ASCII Shift Out (0x0E, ^N) or by setting G0
            // via ESC ( 0 .
            ControlCode::ShiftIn => {
                self.shift_out = false;
            }
            ControlCode::ShiftOut => {
                self.shift_out = true;
            }

            ControlCode::Enquiry => {
                let response = self.config.enq_answerback();
                if response.len() > 0 {
                    write!(self.writer, "{}", response).ok();
                    self.writer.flush().ok();
                }
            }

            ControlCode::Null => {}

            _ => {
                if self.config.log_unknown_escape_sequences() {
                    log::warn!("unhandled ControlCode {:?}", control);
                }
            }
        }
    }

    fn cursor_left(&mut self, n: u32) {
        if n == 0 {
            return;
        }

        if self.reverse_wraparound_mode && self.dec_auto_wrap {
            for _ in 0..n {
                self.control(ControlCode::Backspace);
            }
            return;
        }

        if self.cursor.x < self.left_and_right_margins.start {
            for _ in 0..n {
                self.control(ControlCode::Backspace);
            }
            return;
        }

        let min_x = self.left_and_right_margins.start;
        let new_x = self.cursor.x.saturating_sub(n as usize).max(min_x);
        if new_x != self.cursor.x {
            self.cursor.x = new_x;
            self.cursor.seqno = self.seqno;
            self.wrap_next = false;
        }
    }

    fn csi_dispatch(&mut self, csi: CSI) {
        self.pop_tmux_title_state();
        self.flush_print();
        match csi {
            CSI::Sgr(sgr) => self.state.perform_csi_sgr(sgr),
            CSI::Cursor(frankenterm_escape_parser::csi::Cursor::Left(n)) => {
                // We treat CUB (Cursor::Left) the same as Backspace as
                // that is what xterm does.
                // <https://github.com/wezterm/wezterm/issues/1273>
                self.cursor_left(n);
            }
            CSI::Cursor(cursor) => self.state.perform_csi_cursor(cursor),
            CSI::Edit(edit) => self.state.perform_csi_edit(edit),
            CSI::Mode(mode) => self.state.perform_csi_mode(mode),
            CSI::Device(dev) => self.state.perform_device(*dev),
            CSI::Mouse(mouse) => error!("mouse report sent by app? {:?}", mouse),
            CSI::Window(window) => self.state.perform_csi_window(*window),
            CSI::SelectCharacterPath(CharacterPath::ImplementationDefault, _) => {
                self.state.bidi_hint.take();
            }
            CSI::SelectCharacterPath(CharacterPath::LeftToRightOrTopToBottom, _) => {
                self.state
                    .bidi_hint
                    .replace(ParagraphDirectionHint::LeftToRight);
            }
            CSI::SelectCharacterPath(CharacterPath::RightToLeftOrBottomToTop, _) => {
                self.state
                    .bidi_hint
                    .replace(ParagraphDirectionHint::RightToLeft);
            }
            CSI::Keyboard(Keyboard::SetKittyState { flags, mode }) => {
                if self.config.enable_kitty_keyboard() {
                    let current_flags = match self.screen().keyboard_stack.last() {
                        Some(KeyboardEncoding::Kitty(flags)) => *flags,
                        _ => KittyKeyboardFlags::NONE,
                    };
                    let flags = match mode {
                        KittyKeyboardMode::AssignAll => flags,
                        KittyKeyboardMode::SetSpecified => current_flags | flags,
                        KittyKeyboardMode::ClearSpecified => current_flags - flags,
                    };
                    self.screen_mut().keyboard_stack.pop();
                    self.screen_mut()
                        .keyboard_stack
                        .push(KeyboardEncoding::Kitty(flags));
                }
            }
            CSI::Keyboard(Keyboard::PushKittyState { flags, mode }) => {
                if self.config.enable_kitty_keyboard() {
                    let current_flags = match self.screen().keyboard_stack.last() {
                        Some(KeyboardEncoding::Kitty(flags)) => *flags,
                        _ => KittyKeyboardFlags::NONE,
                    };
                    let flags = match mode {
                        KittyKeyboardMode::AssignAll => flags,
                        KittyKeyboardMode::SetSpecified => current_flags | flags,
                        KittyKeyboardMode::ClearSpecified => current_flags - flags,
                    };
                    let screen = self.screen_mut();
                    screen.keyboard_stack.push(KeyboardEncoding::Kitty(flags));
                    if screen.keyboard_stack.len() > 128 {
                        screen.keyboard_stack.remove(0);
                    }
                }
            }
            CSI::Keyboard(Keyboard::PopKittyState(n)) => {
                for _ in 0..n {
                    self.screen_mut().keyboard_stack.pop();
                }
            }
            CSI::Keyboard(Keyboard::QueryKittySupport) => {
                if self.config.enable_kitty_keyboard() {
                    let flags = match self.screen().keyboard_stack.last() {
                        Some(KeyboardEncoding::Kitty(flags)) => *flags,
                        _ => KittyKeyboardFlags::NONE,
                    };
                    write!(self.writer, "\x1b[?{}u", flags.bits()).ok();
                    self.writer.flush().ok();
                }
            }
            CSI::Keyboard(Keyboard::ReportKittyState(_)) => {
                // This is a response to QueryKittySupport and it is invalid for us
                // to receive it. Just ignore it.
            }
            CSI::Unspecified(unspec) => {
                if self.config.log_unknown_escape_sequences() {
                    log::warn!("unknown unspecified CSI: {:?}", format!("{}", unspec));
                }
            }
        };
    }

    fn esc_dispatch(&mut self, esc: Esc) {
        let seqno = self.seqno;
        self.flush_print();
        if esc != Esc::Code(EscCode::StringTerminator) {
            self.pop_tmux_title_state();
        }
        match esc {
            Esc::Code(EscCode::StringTerminator) => {
                // String Terminator (ST); for the most part has nothing to do here, as its purpose is
                // handled implicitly through a state transition in the vtparse state tables.
                if let Some(title) = self.accumulating_title.take() {
                    self.osc_dispatch(OperatingSystemCommand::SetIconNameAndWindowTitle(title));
                }
            }
            Esc::Code(EscCode::TmuxTitle) => {
                self.accumulating_title.replace(String::new());
            }
            Esc::Code(EscCode::DecApplicationKeyPad) => {
                debug!("DECKPAM on");
                self.application_keypad = true;
            }
            Esc::Code(EscCode::DecNormalKeyPad) => {
                debug!("DECKPAM off");
                self.application_keypad = false;
            }
            Esc::Code(EscCode::ReverseIndex) => self.c1_reverse_index(),
            Esc::Code(EscCode::Index) => self.c1_index(),
            Esc::Code(EscCode::NextLine) => self.c1_nel(),
            Esc::Code(EscCode::HorizontalTabSet) => self.c1_hts(),
            Esc::Code(EscCode::DecLineDrawingG0) => {
                self.g0_charset = CharSet::DecLineDrawing;
            }
            Esc::Code(EscCode::AsciiCharacterSetG0) => {
                self.g0_charset = CharSet::Ascii;
            }
            Esc::Code(EscCode::UkCharacterSetG0) => {
                self.g0_charset = CharSet::Uk;
            }
            Esc::Code(EscCode::DecLineDrawingG1) => {
                self.g1_charset = CharSet::DecLineDrawing;
            }
            Esc::Code(EscCode::AsciiCharacterSetG1) => {
                self.g1_charset = CharSet::Ascii;
            }
            Esc::Code(EscCode::UkCharacterSetG1) => {
                self.g1_charset = CharSet::Uk;
            }
            Esc::Code(EscCode::DecSaveCursorPosition) => self.dec_save_cursor(),
            Esc::Code(EscCode::DecRestoreCursorPosition) => self.dec_restore_cursor(),

            Esc::Code(EscCode::DecDoubleHeightTopHalfLine) => {
                let idx = self.screen.phys_row(self.cursor.y);
                self.screen.line_mut(idx).set_double_height_top(seqno);
            }
            Esc::Code(EscCode::DecDoubleHeightBottomHalfLine) => {
                let idx = self.screen.phys_row(self.cursor.y);
                self.screen.line_mut(idx).set_double_height_bottom(seqno);
            }
            Esc::Code(EscCode::DecDoubleWidthLine) => {
                let idx = self.screen.phys_row(self.cursor.y);
                self.screen.line_mut(idx).set_double_width(seqno);
            }
            Esc::Code(EscCode::DecSingleWidthLine) => {
                let idx = self.screen.phys_row(self.cursor.y);
                self.screen.line_mut(idx).set_single_width(seqno);
            }

            Esc::Code(EscCode::DecScreenAlignmentDisplay) => {
                // This one is just to make vttest happy;
                // its original purpose was for aligning the CRT.
                // https://vt100.net/docs/vt510-rm/DECALN.html

                let screen = self.screen_mut();
                let col_range = 0..screen.physical_cols;
                for y in 0..screen.physical_rows as VisibleRowIndex {
                    let line_idx = screen.phys_row(y);
                    let line = screen.line_mut(line_idx);
                    line.resize(col_range.end, seqno);
                    line.fill_range(
                        col_range.clone(),
                        &Cell::new('E', CellAttributes::default()),
                        seqno,
                    );
                }

                self.top_and_bottom_margins = 0..self.screen().physical_rows as VisibleRowIndex;
                self.left_and_right_margins = 0..self.screen().physical_cols;
                self.cursor = Default::default();
            }

            // RIS resets a device to its initial state, i.e. the state it has after it is switched
            // on. This may imply, if applicable: remove tabulation stops, remove qualified areas,
            // reset graphic rendition, erase all positions, move active position to first
            // character position of first line.
            Esc::Code(EscCode::FullReset) => {
                let seqno = self.seqno;
                self.pen = Default::default();
                self.cursor = Default::default();
                self.wrap_next = false;
                self.clear_semantic_attribute_on_newline = false;
                self.last_semantic_command_status = None;
                self.insert = false;
                self.dec_auto_wrap = true;
                self.saved_dec_private_modes.clear();
                self.reverse_wraparound_mode = false;
                self.reverse_video_mode = false;
                self.dec_origin_mode = false;
                self.use_private_color_registers_for_each_graphic = false;
                self.color_map = default_color_map();
                self.application_cursor_keys = false;
                self.sixel_display_mode = false;
                self.dec_ansi_mode = false;
                self.application_keypad = false;
                self.bracketed_paste = false;
                self.focus_tracking = false;
                self.mouse_tracking = false;
                self.mouse_encoding = MouseEncoding::X10;
                self.keyboard_encoding = KeyboardEncoding::Xterm;
                self.sixel_scrolls_right = false;
                self.any_event_mouse = false;
                self.button_event_mouse = false;
                self.current_mouse_buttons.clear();
                self.cursor_visible = true;
                self.g0_charset = CharSet::Ascii;
                self.g1_charset = CharSet::Ascii;
                self.shift_out = false;
                self.newline_mode = false;
                self.tabs = TabStop::new(self.screen().physical_cols, 8);
                self.palette.take();
                self.top_and_bottom_margins = 0..self.screen().physical_rows as VisibleRowIndex;
                self.left_and_right_margins = 0..self.screen().physical_cols;
                self.unicode_version = self.config.unicode_version();
                self.unicode_version_stack.clear();
                self.suppress_initial_title_change = false;
                self.accumulating_title.take();
                self.progress = Progress::default();

                self.screen.full_reset();
                self.screen.activate_alt_screen(seqno);
                self.erase_in_display(EraseInDisplay::EraseDisplay);
                self.screen.activate_primary_screen(seqno);
                self.erase_in_display(EraseInDisplay::EraseScrollback);
                self.erase_in_display(EraseInDisplay::EraseDisplay);
                self.palette_did_change();
            }

            _ => {
                if self.config.log_unknown_escape_sequences() {
                    log::warn!("ESC: unhandled {:?}", esc);
                }
            }
        }
    }

    fn osc_dispatch(&mut self, osc: OperatingSystemCommand) {
        self.pop_tmux_title_state();
        self.flush_print();
        match osc {
            OperatingSystemCommand::SetIconNameSun(title)
            | OperatingSystemCommand::SetIconName(title) => {
                if title.is_empty() {
                    self.icon_title = None;
                } else {
                    self.icon_title = Some(title);
                }
                let title = self.icon_title.clone();
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::IconTitleChanged(title));
                }
            }
            OperatingSystemCommand::SetIconNameAndWindowTitle(title) => {
                self.icon_title.take();
                self.title = title.clone();
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::WindowTitleChanged(title.clone()));
                    handler.alert(Alert::IconTitleChanged(Some(title)));
                }
            }

            OperatingSystemCommand::SetWindowTitleSun(title)
            | OperatingSystemCommand::SetWindowTitle(title) => {
                self.title = title.clone();
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::WindowTitleChanged(title));
                }
            }
            OperatingSystemCommand::SetHyperlink(link) => {
                self.set_hyperlink(link);
            }
            OperatingSystemCommand::Unspecified(unspec) => {
                if self.config.log_unknown_escape_sequences() {
                    let mut output = String::new();
                    write!(&mut output, "Unhandled OSC ").ok();

                    for item in unspec {
                        write!(&mut output, " {}", String::from_utf8_lossy(&item)).ok();
                    }
                    log::warn!("{}", output);
                }
            }

            OperatingSystemCommand::ClearSelection(selection) => {
                let selection = selection_to_selection(selection);
                self.set_clipboard_contents(selection, None).ok();
            }
            OperatingSystemCommand::QuerySelection(selection) => {
                // Privacy default for OSC 52 reads: this layer has no read
                // approval UI or clipboard-read API, so respond with the same
                // empty payload shape as a genuine empty clipboard.
                write!(self.writer, "\x1b]52;{selection};\x1b\\").ok();
                self.writer.flush().ok();
            }
            OperatingSystemCommand::SetSelection(selection, selection_data) => {
                // Per ft-io922 (cont of ft-2okh0.1.5): route OSC 52
                // SetSelection through the operator policy gate +
                // size cap before touching the OS clipboard.
                // Default policy is Allow with a 1 MiB cap so
                // existing yank-via-osc52 workflows are unaffected;
                // privacy-conservative deployments can override
                // via TerminalConfiguration::osc52_write_policy.
                let policy = self.config.osc52_write_policy();
                let max_bytes = self.config.osc52_write_max_bytes();
                let outcome =
                    crate::config::route_osc52_write(selection_data.as_bytes(), policy, max_bytes);
                match outcome {
                    crate::config::Osc52WriteOutcome::Allow { .. } => {
                        let selection = selection_to_selection(selection);
                        match self.set_clipboard_contents(selection, Some(selection_data)) {
                            Ok(_) => (),
                            Err(err) => {
                                error!("failed to set clipboard in response to OSC 52: {:#?}", err)
                            }
                        }
                    }
                    crate::config::Osc52WriteOutcome::Prompt { .. } => {
                        // Prompt is resolved by the GUI integration above
                        // this layer; the term-state layer must not write
                        // until the operator confirms. Today there is no
                        // GUI prompt wiring, so Prompt is effectively a
                        // deferred-deny at this layer — log and drop.
                        log::info!(
                            "OSC 52 write deferred to operator prompt \
                             (no GUI prompt wired yet — request dropped)"
                        );
                    }
                    crate::config::Osc52WriteOutcome::DenyByPolicy => {
                        // Privacy: do not log the clipboard bytes.
                        log::debug!(
                            "OSC 52 write denied by operator policy \
                             (selection={:?})",
                            selection
                        );
                    }
                    crate::config::Osc52WriteOutcome::DenyOversized {
                        decoded_len,
                        max_bytes,
                    } => {
                        log::warn!(
                            "OSC 52 write rejected: payload {decoded_len} bytes \
                             exceeds osc52_write_max_bytes={max_bytes}"
                        );
                    }
                }
            }
            OperatingSystemCommand::ITermProprietary(iterm) => match iterm {
                ITermProprietary::RequestCellSize => {
                    let screen = self.screen();
                    let height = screen.physical_rows;
                    let width = screen.physical_cols;

                    let scale = if screen.dpi == 0 {
                        1.0
                    } else {
                        // Since iTerm2 is a macOS specific piece
                        // of software, it uses the macOS default dpi
                        // if 72 for the basis of its scale, regardless
                        // of the host base dpi.
                        screen.dpi as f32 / 72.
                    };
                    let width_f = (self.pixel_width as f32 / width.max(1) as f32) / scale;
                    let height_f = (self.pixel_height as f32 / height.max(1) as f32) / scale;

                    let response = OperatingSystemCommand::ITermProprietary(
                        ITermProprietary::ReportCellSize {
                            width_pixels: NotNan::new(width_f).unwrap_or_default(),
                            height_pixels: NotNan::new(height_f).unwrap_or_default(),
                            scale: if screen.dpi == 0 {
                                None
                            } else {
                                NotNan::new(scale).ok()
                            },
                        },
                    );
                    write!(self.writer, "{}", response).ok();
                    self.writer.flush().ok();
                }
                ITermProprietary::File(image) => self.set_image(*image),
                ITermProprietary::SetUserVar { name, value } => {
                    // Cap user_vars to prevent unbounded growth from
                    // long-running sessions emitting many SetUserVar sequences.
                    if self.user_vars.len() >= self.config.max_user_vars()
                        && !self.user_vars.contains_key(&*name)
                    {
                        // Evict an arbitrary entry to make room
                        if let Some(oldest_key) = self.user_vars.keys().next().cloned() {
                            self.user_vars.remove(&oldest_key);
                        }
                    }
                    self.user_vars.insert(name.clone(), value.clone());
                    if let Some(handler) = self.alert_handler.as_mut() {
                        handler.alert(Alert::SetUserVar { name, value });
                    }
                }
                ITermProprietary::UnicodeVersion(ITermUnicodeVersionOp::Set(n)) => {
                    self.unicode_version.version = n;
                }
                ITermProprietary::UnicodeVersion(ITermUnicodeVersionOp::Push(label)) => {
                    // Cap stack depth to prevent unbounded growth from
                    // unbalanced Push operations in long-running sessions.
                    if self.unicode_version_stack.len()
                        >= self.config.max_unicode_version_stack_depth()
                    {
                        log::warn!(
                            "unicode version stack depth limit ({}) reached, \
                             dropping oldest entry",
                            self.config.max_unicode_version_stack_depth()
                        );
                        self.unicode_version_stack.remove(0);
                    }
                    let vers = self.unicode_version.clone();
                    self.unicode_version_stack
                        .push(UnicodeVersionStackEntry { vers, label });
                }
                ITermProprietary::UnicodeVersion(ITermUnicodeVersionOp::Pop(None)) => {
                    if let Some(entry) = self.unicode_version_stack.pop() {
                        self.unicode_version = entry.vers;
                    }
                }
                ITermProprietary::UnicodeVersion(ITermUnicodeVersionOp::Pop(Some(label))) => {
                    while let Some(entry) = self.unicode_version_stack.pop() {
                        self.unicode_version = entry.vers;
                        if entry.label.as_deref() == Some(&label) {
                            break;
                        }
                    }
                }
                ITermProprietary::SetProfile(name) => {
                    // Security gate (ft-fy4ty): never apply the
                    // profile switch silently. Surface the request
                    // to the embedder via Alert::SetProfileRequested
                    // so the GUI layer can show a confirmation
                    // prompt. Without an alert handler, the request
                    // is dropped — that's the safer default than
                    // legacy iTerm2's silent-switch behavior.
                    if let Some(handler) = self.alert_handler.as_mut() {
                        handler.alert(Alert::SetProfileRequested { name });
                    } else if self.config.log_unknown_escape_sequences() {
                        log::warn!(
                            "OSC 1337 SetProfile request dropped (no alert \
                             handler installed); name={:?}",
                            name,
                        );
                    }
                }
                _ => {
                    if self.config.log_unknown_escape_sequences() {
                        log::warn!("unhandled iterm2: {:?}", iterm);
                    }
                }
            },

            OperatingSystemCommand::FinalTermSemanticPrompt(FinalTermSemanticPrompt::FreshLine) => {
                self.fresh_line();
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::FreshLineAndStartPrompt { .. },
            ) => {
                self.fresh_line();
                self.pen.set_semantic_type(SemanticType::Prompt);
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::StartPrompt(_),
            ) => {
                self.pen.set_semantic_type(SemanticType::Prompt);
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::MarkEndOfCommandWithFreshLine { .. },
            ) => {
                self.fresh_line();
                self.pen.set_semantic_type(SemanticType::Prompt);
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::MarkEndOfPromptAndStartOfInputUntilNextMarker { .. },
            ) => {
                self.pen.set_semantic_type(SemanticType::Input);
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::MarkEndOfPromptAndStartOfInputUntilEndOfLine { .. },
            ) => {
                self.pen.set_semantic_type(SemanticType::Input);
                self.clear_semantic_attribute_on_newline = true;
            }
            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::MarkEndOfInputAndStartOfOutput { .. },
            ) => {
                self.pen.set_semantic_type(SemanticType::Output);
            }

            OperatingSystemCommand::FinalTermSemanticPrompt(
                FinalTermSemanticPrompt::CommandStatus { status, .. },
            ) => {
                self.last_semantic_command_status = Some(status);
            }

            OperatingSystemCommand::SystemNotification(message) => {
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::ToastNotification {
                        title: None,
                        body: message,
                        focus: true,
                    });
                } else {
                    log::info!("Application sends SystemNotification: {}", message);
                }
            }
            OperatingSystemCommand::RxvtExtension(params) => {
                if let Some("notify") = params.get(0).map(String::as_str) {
                    let title = params.get(1);
                    let body = params.get(2);
                    let (title, body) = match (title.cloned(), body.cloned()) {
                        (Some(title), None) => (None, title),
                        (Some(title), Some(body)) => (Some(title), body),
                        _ => {
                            log::warn!("malformed rxvt notify escape: {:?}", params);
                            return;
                        }
                    };
                    if let Some(handler) = self.alert_handler.as_mut() {
                        handler.alert(Alert::ToastNotification {
                            title,
                            body,
                            focus: true,
                        });
                    }
                }
            }
            OperatingSystemCommand::CurrentWorkingDirectory(url) => {
                self.current_dir = Url::parse(&url).ok();
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::CurrentWorkingDirectoryChanged);
                }
            }
            OperatingSystemCommand::ChangeColorNumber(specs) => {
                log::trace!("ChangeColorNumber: {:?}", specs);
                for pair in specs {
                    match pair.color {
                        ColorOrQuery::Query => {
                            let response =
                                OperatingSystemCommand::ChangeColorNumber(vec![ChangeColorPair {
                                    palette_index: pair.palette_index,
                                    color: ColorOrQuery::Color(
                                        self.palette().colors.0[pair.palette_index as usize],
                                    ),
                                }]);
                            write!(self.writer, "{}", response).ok();
                            self.writer.flush().ok();
                        }
                        ColorOrQuery::Color(c) => {
                            self.palette_mut().colors.0[pair.palette_index as usize] = c;
                        }
                    }
                }
                self.implicit_palette_reset_if_same_as_configured();
                self.palette_did_change();
            }

            OperatingSystemCommand::ResetColors(colors) => {
                log::trace!("ResetColors: {:?}", colors);
                if colors.is_empty() {
                    // Reset all colors
                    self.palette.take();
                } else {
                    // Reset individual colors
                    if self.palette.is_none() {
                        // Already at the defaults
                    } else {
                        let base = self.config.color_palette();
                        for c in colors {
                            let c = c as usize;
                            self.palette_mut().colors.0[c] = base.colors.0[c];
                        }
                    }
                }
                self.implicit_palette_reset_if_same_as_configured();
                self.palette_did_change();
            }

            OperatingSystemCommand::ChangeDynamicColors(first_color, colors) => {
                log::trace!("ChangeDynamicColors: {:?} {:?}", first_color, colors);
                use frankenterm_escape_parser::osc::DynamicColorNumber;
                for (idx, color) in (first_color as u8..).zip(colors) {
                    let which_color: Option<DynamicColorNumber> = FromPrimitive::from_u8(idx);
                    log::trace!("ChangeDynamicColors item: {:?}", which_color);
                    if let Some(which_color) = which_color {
                        macro_rules! set_or_query {
                            ($name:ident) => {
                                match color {
                                    ColorOrQuery::Query => {
                                        let response = OperatingSystemCommand::ChangeDynamicColors(
                                            which_color,
                                            vec![ColorOrQuery::Color(self.palette().$name.into())],
                                        );
                                        log::trace!("Color Query response {:?}", response);
                                        write!(self.writer, "{}", response).ok();
                                        self.writer.flush().ok();
                                    }
                                    ColorOrQuery::Color(c) => self.palette_mut().$name = c.into(),
                                }
                            };
                        }
                        match which_color {
                            DynamicColorNumber::TextForegroundColor => set_or_query!(foreground),
                            DynamicColorNumber::TextBackgroundColor => set_or_query!(background),
                            DynamicColorNumber::TextCursorColor => {
                                if let ColorOrQuery::Color(c) = color {
                                    // We set the border to the background color; we don't
                                    // have an escape that sets that independently, and this
                                    // way just looks better.
                                    self.palette_mut().cursor_border = c.into();
                                }
                                set_or_query!(cursor_bg)
                            }
                            DynamicColorNumber::HighlightForegroundColor => {
                                set_or_query!(selection_fg)
                            }
                            DynamicColorNumber::HighlightBackgroundColor => {
                                set_or_query!(selection_bg)
                            }
                            DynamicColorNumber::MouseForegroundColor
                            | DynamicColorNumber::MouseBackgroundColor
                            | DynamicColorNumber::TektronixForegroundColor
                            | DynamicColorNumber::TektronixBackgroundColor
                            | DynamicColorNumber::TektronixCursorColor => {}
                        }
                    }
                }
                self.implicit_palette_reset_if_same_as_configured();
                self.palette_did_change();
            }

            OperatingSystemCommand::ResetDynamicColor(color) => {
                log::trace!("ResetDynamicColor: {:?}", color);
                use frankenterm_escape_parser::osc::DynamicColorNumber;
                let which_color: Option<DynamicColorNumber> = FromPrimitive::from_u8(color as u8);
                if let Some(which_color) = which_color {
                    macro_rules! reset {
                        ($name:ident) => {
                            if self.palette.is_none() {
                                // Already at the defaults
                            } else {
                                let base = self.config.color_palette();
                                self.palette_mut().$name = base.$name;
                            }
                        };
                    }
                    match which_color {
                        DynamicColorNumber::TextForegroundColor => reset!(foreground),
                        DynamicColorNumber::TextBackgroundColor => reset!(background),
                        DynamicColorNumber::TextCursorColor => {
                            reset!(cursor_bg);
                            // Since we set the border to the bg, we consider it reset
                            // by resetting the bg too!
                            reset!(cursor_border);
                        }
                        DynamicColorNumber::HighlightForegroundColor => reset!(selection_fg),
                        DynamicColorNumber::HighlightBackgroundColor => reset!(selection_bg),
                        DynamicColorNumber::MouseForegroundColor
                        | DynamicColorNumber::MouseBackgroundColor
                        | DynamicColorNumber::TektronixForegroundColor
                        | DynamicColorNumber::TektronixBackgroundColor
                        | DynamicColorNumber::TektronixCursorColor => {}
                    }
                }
                self.implicit_palette_reset_if_same_as_configured();
                self.palette_did_change();
            }
            OperatingSystemCommand::ConEmuProgress(prog) => {
                use frankenterm_escape_parser::osc::Progress as TProg;
                let prog = match prog {
                    TProg::None => Progress::None,
                    TProg::SetPercentage(p) => Progress::Percentage(p),
                    TProg::SetError(p) => Progress::Error(p),
                    TProg::SetIndeterminate => Progress::Indeterminate,
                    TProg::Paused => Progress::None,
                };
                if prog != self.progress {
                    self.progress = prog.clone();
                    if let Some(handler) = self.alert_handler.as_mut() {
                        handler.alert(Alert::Progress(prog));
                    }
                }
            }
            OperatingSystemCommand::SetMouseShape(shape) => {
                // OSC 22 (ft-7yiu2). The term layer doesn't own a
                // native mouse cursor — it routes the requested
                // shape to the embedder. Without an alert handler
                // installed the request is dropped silently (no
                // observable terminal state changes either way).
                if let Some(handler) = self.alert_handler.as_mut() {
                    handler.alert(Alert::MouseShapeRequested { shape });
                }
            }
        }
    }
}

fn selection_to_selection(sel: Selection) -> ClipboardSelection {
    match sel {
        Selection::CLIPBOARD => ClipboardSelection::Clipboard,
        Selection::PRIMARY => ClipboardSelection::PrimarySelection,
        // xterm will use a configurable selection in the NONE case
        Selection::NONE => ClipboardSelection::Clipboard,
        // otherwise we just use clipboard.  Could potentially
        // also use the same fallback configuration as NONE,
        // if/when we add it
        _ => ClipboardSelection::Clipboard,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ColorPalette;
    use crate::config::ScrollbackTierConfig;
    use crate::terminal::{Terminal, TerminalSize};
    use crate::{CellAttributes, CursorPosition, Line, TerminalConfiguration};
    use frankenterm_escape_parser::parser::Parser;
    use std::sync::Arc;

    #[derive(Debug)]
    struct GateTermConfig;

    impl TerminalConfiguration for GateTermConfig {
        fn scrollback_size(&self) -> usize {
            64
        }

        fn scrollback_tier_config(&self) -> ScrollbackTierConfig {
            ScrollbackTierConfig::default()
        }

        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    struct LineSnapshot {
        text: String,
        wrapped: bool,
        cells: Vec<(String, usize, CellAttributes)>,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct TerminalSnapshot {
        cursor: CursorPosition,
        title: String,
        current_dir: Option<String>,
        progress: Progress,
        palette: ColorPalette,
        all_lines: Vec<LineSnapshot>,
    }

    #[derive(Debug, PartialEq)]
    struct GateRun {
        actions: Vec<Action>,
        terminal: TerminalSnapshot,
    }

    struct ScalarScanOverride;

    impl ScalarScanOverride {
        fn set(force_scalar: bool) -> Self {
            FORCE_SCALAR_PRINTABLE_ASCII_SCAN
                .store(force_scalar, std::sync::atomic::Ordering::Relaxed);
            Self
        }
    }

    impl Drop for ScalarScanOverride {
        fn drop(&mut self) {
            FORCE_SCALAR_PRINTABLE_ASCII_SCAN.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn gate_corpus() -> Vec<(&'static str, Vec<u8>)> {
        let mut cases = vec![
            ("pure_ascii_short", b"simple printable ascii run".to_vec()),
            (
                "pure_ascii_long",
                std::iter::repeat(b'x').take(96).collect(),
            ),
            ("ascii_control_mix", b"abc\r\ndef\tghi\x08!".to_vec()),
            (
                "utf8_multibyte",
                b"caf\xc3\xa9 \xe2\x82\xac A\xcc\x81 \xf0\x9f\x9a\x80 end".to_vec(),
            ),
            (
                "embedded_sgr_csi",
                b"lead\x1b[31mred\x1b[0mtail\x1b[2Jafter".to_vec(),
            ),
            ("embedded_osc", b"\x1b]0;swar gate\x07after".to_vec()),
        ];

        for boundary in [8, 16, 64] {
            cases.push(("ascii_run_boundary_lf", boundary_case(boundary, b"\n")));
            cases.push((
                "ascii_run_boundary_csi",
                boundary_case(boundary, b"\x1b[4m"),
            ));
            cases.push((
                "ascii_run_boundary_utf8",
                boundary_case(boundary, b"\xc3\xa9"),
            ));
        }

        cases
    }

    fn boundary_case(printable_prefix_len: usize, boundary_bytes: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(printable_prefix_len + boundary_bytes.len() + 8);
        bytes.extend(std::iter::repeat(b'a').take(printable_prefix_len));
        bytes.extend_from_slice(boundary_bytes);
        bytes.extend_from_slice(b"tail");
        bytes
    }

    fn make_terminal() -> Terminal {
        Terminal::new(
            TerminalSize {
                rows: 8,
                cols: 80,
                pixel_width: 640,
                pixel_height: 128,
                dpi: 96,
            },
            Arc::new(GateTermConfig),
            "WezTerm",
            "test",
            Box::new(Vec::new()),
        )
    }

    fn snapshot_line(line: &Line) -> LineSnapshot {
        LineSnapshot {
            text: line.as_str().to_string(),
            wrapped: line.last_cell_was_wrapped(),
            cells: line
                .visible_cells()
                .map(|cell| (cell.str().to_string(), cell.width(), cell.attrs().clone()))
                .collect(),
        }
    }

    fn snapshot_terminal(term: &Terminal) -> TerminalSnapshot {
        let mut cursor = term.cursor_pos();
        cursor.seqno = 0;
        TerminalSnapshot {
            cursor,
            title: term.get_title().to_string(),
            current_dir: term.get_current_dir().map(|url| url.to_string()),
            progress: term.get_progress(),
            palette: term.palette(),
            all_lines: term
                .screen()
                .all_lines()
                .iter()
                .map(snapshot_line)
                .collect(),
        }
    }

    fn run_gate_stream(bytes: &[u8], force_scalar: bool) -> GateRun {
        let _scan_override = ScalarScanOverride::set(force_scalar);
        let mut parser = Parser::new();
        let actions = parser.parse_as_vec(bytes);
        let mut term = make_terminal();
        term.advance_bytes(bytes);
        GateRun {
            actions,
            terminal: snapshot_terminal(&term),
        }
    }

    #[test]
    fn swar_terminal_effects_match_scalar_for_representative_vte_streams() {
        for (name, bytes) in gate_corpus() {
            let swar = run_gate_stream(&bytes, false);
            let scalar = run_gate_stream(&bytes, true);
            assert_eq!(swar, scalar, "SWAR diverged from scalar for {name}");
        }
    }

    /// Round-5 D1 (ft-round5-gauntlet-lw0s7.10): the parser printable-run
    /// batching optimization emits `Action::PrintString` for ground-state
    /// printable runs instead of one `Action::Print` per codepoint. The
    /// rendered terminal must be byte-identical either way: a `PrintString`
    /// and the equivalent `Print` sequence both accumulate into the print
    /// buffer before `flush_print`.
    fn render_actions(actions: Vec<Action>) -> TerminalSnapshot {
        let mut term = make_terminal();
        term.perform_actions(actions);
        snapshot_terminal(&term)
    }

    fn parse_with_batching(bytes: &[u8], on: bool) -> Vec<Action> {
        let mut parser = Parser::new();
        parser.set_print_batching(on);
        parser.parse_as_vec(bytes)
    }

    #[test]
    fn parser_print_batching_terminal_effects_match_scalar() {
        for (name, bytes) in gate_corpus() {
            let scalar = render_actions(parse_with_batching(&bytes, false));
            let batched = render_actions(parse_with_batching(&bytes, true));
            assert_eq!(
                scalar, batched,
                "print-batching diverged from scalar terminal render for {name}"
            );
            // The batched action stream must actually differ from scalar on
            // printable-bearing corpus entries (otherwise the gate is vacuous);
            // we only assert render equality above, which holds for all entries.
        }
    }

    #[test]
    fn parser_print_batching_chunked_terminal_effects_match_scalar() {
        for (name, bytes) in gate_corpus() {
            // Reference: scalar, whole buffer.
            let reference = render_actions(parse_with_batching(&bytes, false));
            for split in 0..=bytes.len() {
                let mut parser = Parser::new();
                parser.set_print_batching(true);
                let mut actions = Vec::new();
                parser.parse(&bytes[..split], |a| actions.push(a));
                parser.parse(&bytes[split..], |a| actions.push(a));
                let chunked = render_actions(actions);
                assert_eq!(
                    chunked, reference,
                    "chunked print-batching diverged from scalar render for {name} at split {split}"
                );
            }
        }
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn swar_prefix_scan_matches_scalar_on_every_gate_stream_suffix() {
        for (name, bytes) in gate_corpus() {
            for offset in 0..=bytes.len() {
                let suffix = &bytes[offset..];
                assert_eq!(
                    Performer::printable_ascii_prefix_len_swar(suffix),
                    Performer::printable_ascii_prefix_len_scalar(suffix),
                    "SWAR prefix length diverged from scalar for {name} at offset {offset}"
                );
            }
        }
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn swar_prefix_scan_matches_scalar_on_adversarial_boundaries() {
        let fixed_cases: &[(&str, &[u8])] = &[
            ("empty", b""),
            ("single_space", b" "),
            ("single_tilde", b"~"),
            ("single_nul", b"\0"),
            ("single_us", b"\x1f"),
            ("single_del", b"\x7f"),
            ("single_non_ascii", b"\x80"),
        ];
        for (name, bytes) in fixed_cases {
            assert_eq!(
                Performer::printable_ascii_prefix_len_swar(bytes),
                Performer::printable_ascii_prefix_len_scalar(bytes),
                "SWAR prefix length diverged from scalar for {name}"
            );
        }

        for run_len in 0..=17 {
            let printable_run = vec![b'x'; run_len];
            assert_eq!(
                Performer::printable_ascii_prefix_len_swar(&printable_run),
                Performer::printable_ascii_prefix_len_scalar(&printable_run),
                "SWAR prefix length diverged for printable tail length {run_len}"
            );

            for (name, marker) in [
                ("nul", b"\0".as_slice()),
                ("lf", b"\n".as_slice()),
                ("del", b"\x7f".as_slice()),
                ("non_ascii", b"\xc3".as_slice()),
            ] {
                let mut bytes = printable_run.clone();
                bytes.extend_from_slice(marker);
                assert_eq!(
                    Performer::printable_ascii_prefix_len_swar(&bytes),
                    Performer::printable_ascii_prefix_len_scalar(&bytes),
                    "SWAR prefix length diverged for {name} after printable run length {run_len}"
                );
            }
        }

        for (name, run_len, marker) in [
            ("utf8_two_byte_split_at_8", 7, b"\xc3\xa9".as_slice()),
            ("utf8_three_byte_split_at_8", 6, b"\xe2\x82\xac".as_slice()),
            (
                "utf8_four_byte_split_at_16",
                15,
                b"\xf0\x9f\x9a\x80".as_slice(),
            ),
            (
                "utf8_first_byte_at_word_boundary",
                8,
                b"\xc3\xa9".as_slice(),
            ),
            ("control_at_word_boundary", 8, b"\x1b".as_slice()),
            ("non_ascii_at_word_boundary", 16, b"\x80".as_slice()),
        ] {
            let mut bytes = vec![b'a'; run_len];
            bytes.extend_from_slice(marker);
            assert_eq!(
                Performer::printable_ascii_prefix_len_swar(&bytes),
                Performer::printable_ascii_prefix_len_scalar(&bytes),
                "SWAR prefix length diverged for {name}"
            );
        }
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn swar_non_printable_mask_matches_scalar_for_both_byte_orders() {
        assert_eq!(
            Performer::swar_non_printable_ascii_mask(u64::from_le_bytes(*b"ABCDEFGH")),
            0
        );
        assert_eq!(
            Performer::swar_non_printable_ascii_mask(u64::from_be_bytes(*b"ABCDEFGH")),
            0
        );

        for pos in 0..8 {
            for byte in [0x00, 0x1f, 0x20, 0x7e, 0x7f, 0x80, 0xc3, 0xff] {
                let mut bytes = [b'A'; 8];
                bytes[pos] = byte;
                let expected = bytes.iter().any(|byte| !matches!(*byte, 0x20..=0x7e));
                assert_eq!(
                    Performer::swar_non_printable_ascii_mask(u64::from_le_bytes(bytes)) != 0,
                    expected,
                    "little-endian SWAR mask diverged at byte position {pos} for byte {byte:#04x}"
                );
                assert_eq!(
                    Performer::swar_non_printable_ascii_mask(u64::from_be_bytes(bytes)) != 0,
                    expected,
                    "big-endian SWAR mask diverged at byte position {pos} for byte {byte:#04x}"
                );
            }
        }
    }
}
