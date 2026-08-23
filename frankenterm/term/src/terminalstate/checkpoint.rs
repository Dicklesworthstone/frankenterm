//! Canonical semantic terminal-state checkpoint model.
//!
//! This module projects live terminal state into deterministic, capability-free
//! data. It does not own filesystem publication or raw-output identity; the mux
//! guardian binds encoded bytes to `GuardianCheckpointBoundary` after capture.

use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// First semantic terminal checkpoint schema.
pub const TERMINAL_CHECKPOINT_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointCharSet {
    Ascii,
    Uk,
    DecLineDrawing,
}

impl From<CharSet> for CheckpointCharSet {
    fn from(value: CharSet) -> Self {
        match value {
            CharSet::Ascii => Self::Ascii,
            CharSet::Uk => Self::Uk,
            CharSet::DecLineDrawing => Self::DecLineDrawing,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointMouseEncoding {
    X10,
    Utf8,
    Sgr,
    SgrPixels,
}

impl From<MouseEncoding> for CheckpointMouseEncoding {
    fn from(value: MouseEncoding) -> Self {
        match value {
            MouseEncoding::X10 => Self::X10,
            MouseEncoding::Utf8 => Self::Utf8,
            MouseEncoding::SGR => Self::Sgr,
            MouseEncoding::SgrPixels => Self::SgrPixels,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "flags")]
enum CheckpointKeyboardEncoding {
    Xterm,
    CsiU,
    Win32,
    Kitty(u16),
}

impl From<KeyboardEncoding> for CheckpointKeyboardEncoding {
    fn from(value: KeyboardEncoding) -> Self {
        match value {
            KeyboardEncoding::Xterm => Self::Xterm,
            KeyboardEncoding::CsiU => Self::CsiU,
            KeyboardEncoding::Win32 => Self::Win32,
            KeyboardEncoding::Kitty(flags) => Self::Kitty(flags.bits()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointBidiHint {
    LeftToRight,
    RightToLeft,
    AutoLeftToRight,
    AutoRightToLeft,
}

impl From<ParagraphDirectionHint> for CheckpointBidiHint {
    fn from(value: ParagraphDirectionHint) -> Self {
        match value {
            ParagraphDirectionHint::LeftToRight => Self::LeftToRight,
            ParagraphDirectionHint::RightToLeft => Self::RightToLeft,
            ParagraphDirectionHint::AutoLeftToRight => Self::AutoLeftToRight,
            ParagraphDirectionHint::AutoRightToLeft => Self::AutoRightToLeft,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct CheckpointUnicodeVersion {
    version: u8,
    ambiguous_are_wide: bool,
    custom_cell_widths: BTreeMap<u32, u8>,
}

impl From<&UnicodeVersion> for CheckpointUnicodeVersion {
    fn from(value: &UnicodeVersion) -> Self {
        Self {
            version: value.version,
            ambiguous_are_wide: value.ambiguous_are_wide,
            custom_cell_widths: value
                .cell_widths
                .as_ref()
                .map(|widths| widths.iter().map(|(codepoint, width)| (*codepoint, *width)).collect())
                .unwrap_or_default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct CheckpointSavedCursor {
    position: CursorPosition,
    wrap_next: bool,
    pen: CellAttributes,
    dec_origin_mode: bool,
    g0_charset: CheckpointCharSet,
    g1_charset: CheckpointCharSet,
}

impl From<&SavedCursor> for CheckpointSavedCursor {
    fn from(value: &SavedCursor) -> Self {
        Self {
            position: value.position,
            wrap_next: value.wrap_next,
            pen: value.pen.clone(),
            dec_origin_mode: value.dec_origin_mode,
            g0_charset: value.g0_charset.into(),
            g1_charset: value.g1_charset.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct CheckpointScreen {
    lines: Vec<Line>,
    stable_row_index_offset: usize,
    allow_scrollback: bool,
    keyboard_stack: Vec<CheckpointKeyboardEncoding>,
    physical_rows: usize,
    physical_cols: usize,
    dpi: u32,
    saved_cursor: Option<CheckpointSavedCursor>,
}

impl From<crate::screen::ScreenCheckpointParts> for CheckpointScreen {
    fn from(value: crate::screen::ScreenCheckpointParts) -> Self {
        Self {
            lines: value.lines,
            stable_row_index_offset: value.stable_row_index_offset,
            allow_scrollback: value.allow_scrollback,
            keyboard_stack: value
                .keyboard_stack
                .into_iter()
                .map(CheckpointKeyboardEncoding::from)
                .collect(),
            physical_rows: value.physical_rows,
            physical_cols: value.physical_cols,
            dpi: value.dpi,
            saved_cursor: value.saved_cursor.as_ref().map(CheckpointSavedCursor::from),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct CheckpointUnicodeVersionStackEntry {
    version: CheckpointUnicodeVersion,
    label: Option<String>,
}

/// Capability-free semantic projection of one terminal model.
///
/// Fields remain private so callers cannot construct an unvalidated authority;
/// serde decoding will be paired with a validating constructor before restore.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerminalCheckpointV1 {
    version: u32,
    primary_screen: CheckpointScreen,
    alternate_screen: CheckpointScreen,
    alternate_screen_active: bool,
    pen: CellAttributes,
    cursor: CursorPosition,
    wrap_next: bool,
    clear_semantic_attribute_on_newline: bool,
    last_semantic_command_status: Option<i32>,
    insert: bool,
    dec_auto_wrap: bool,
    saved_dec_private_modes: BTreeMap<u16, bool>,
    reverse_wraparound_mode: bool,
    reverse_video_mode: bool,
    synchronized_output: bool,
    dec_origin_mode: bool,
    top_margin: VisibleRowIndex,
    bottom_margin: VisibleRowIndex,
    left_margin: usize,
    right_margin: usize,
    left_and_right_margin_mode: bool,
    application_cursor_keys: bool,
    modify_other_keys: Option<i64>,
    dec_ansi_mode: bool,
    sixel_display_mode: bool,
    use_private_color_registers_for_each_graphic: bool,
    color_map: BTreeMap<u16, RgbColor>,
    application_keypad: bool,
    bracketed_paste: bool,
    any_event_mouse: bool,
    focus_tracking: bool,
    mouse_encoding: CheckpointMouseEncoding,
    mouse_tracking: bool,
    button_event_mouse: bool,
    current_mouse_buttons: Vec<MouseButton>,
    last_mouse_move: Option<MouseEvent>,
    cursor_visible: bool,
    keyboard_encoding: CheckpointKeyboardEncoding,
    g0_charset: CheckpointCharSet,
    g1_charset: CheckpointCharSet,
    shift_out: bool,
    newline_mode: bool,
    tab_stops: Vec<bool>,
    tab_width: usize,
    title: String,
    icon_title: Option<String>,
    progress: Progress,
    palette: Option<ColorPalette>,
    pixel_width: usize,
    pixel_height: usize,
    dpi: u32,
    current_dir: Option<String>,
    term_program: String,
    term_version: String,
    sixel_scrolls_right: bool,
    user_vars: BTreeMap<String, String>,
    seqno: SequenceNo,
    unicode_version: CheckpointUnicodeVersion,
    unicode_version_stack: Vec<CheckpointUnicodeVersionStackEntry>,
    enable_conpty_quirks: bool,
    suppress_initial_title_change: bool,
    accumulating_title: Option<String>,
    lost_focus_seqno: SequenceNo,
    lost_focus_alerted_seqno: SequenceNo,
    focused: bool,
    bidi_enabled: Option<bool>,
    bidi_hint: Option<CheckpointBidiHint>,
}

impl TerminalCheckpointV1 {
    /// Capture every v1 semantic field or reject unsupported graphics state.
    pub fn capture(terminal: &TerminalState) -> Result<Self, TerminalCheckpointError> {
        if !terminal.image_cache.is_empty() || !terminal.kitty_img.is_empty_for_checkpoint() {
            return Err(TerminalCheckpointError::UnsupportedGraphicsState);
        }

        Ok(Self {
            version: TERMINAL_CHECKPOINT_VERSION,
            primary_screen: terminal.screen.screen.checkpoint_parts().into(),
            alternate_screen: terminal.screen.alt_screen.checkpoint_parts().into(),
            alternate_screen_active: terminal.screen.alt_screen_is_active,
            pen: terminal.pen.clone(),
            cursor: terminal.cursor,
            wrap_next: terminal.wrap_next,
            clear_semantic_attribute_on_newline: terminal.clear_semantic_attribute_on_newline,
            last_semantic_command_status: terminal.last_semantic_command_status,
            insert: terminal.insert,
            dec_auto_wrap: terminal.dec_auto_wrap,
            saved_dec_private_modes: terminal
                .saved_dec_private_modes
                .iter()
                .map(|(mode, enabled)| (*mode, *enabled))
                .collect(),
            reverse_wraparound_mode: terminal.reverse_wraparound_mode,
            reverse_video_mode: terminal.reverse_video_mode,
            synchronized_output: terminal.synchronized_output,
            dec_origin_mode: terminal.dec_origin_mode,
            top_margin: terminal.top_and_bottom_margins.start,
            bottom_margin: terminal.top_and_bottom_margins.end,
            left_margin: terminal.left_and_right_margins.start,
            right_margin: terminal.left_and_right_margins.end,
            left_and_right_margin_mode: terminal.left_and_right_margin_mode,
            application_cursor_keys: terminal.application_cursor_keys,
            modify_other_keys: terminal.modify_other_keys,
            dec_ansi_mode: terminal.dec_ansi_mode,
            sixel_display_mode: terminal.sixel_display_mode,
            use_private_color_registers_for_each_graphic: terminal
                .use_private_color_registers_for_each_graphic,
            color_map: terminal
                .color_map
                .iter()
                .map(|(index, color)| (*index, *color))
                .collect(),
            application_keypad: terminal.application_keypad,
            bracketed_paste: terminal.bracketed_paste,
            any_event_mouse: terminal.any_event_mouse,
            focus_tracking: terminal.focus_tracking,
            mouse_encoding: terminal.mouse_encoding.into(),
            mouse_tracking: terminal.mouse_tracking,
            button_event_mouse: terminal.button_event_mouse,
            current_mouse_buttons: terminal.current_mouse_buttons.clone(),
            last_mouse_move: terminal.last_mouse_move,
            cursor_visible: terminal.cursor_visible,
            keyboard_encoding: terminal.keyboard_encoding.into(),
            g0_charset: terminal.g0_charset.into(),
            g1_charset: terminal.g1_charset.into(),
            shift_out: terminal.shift_out,
            newline_mode: terminal.newline_mode,
            tab_stops: terminal.tabs.tabs.clone(),
            tab_width: terminal.tabs.tab_width,
            title: terminal.title.clone(),
            icon_title: terminal.icon_title.clone(),
            progress: terminal.progress.clone(),
            palette: terminal.palette.clone(),
            pixel_width: terminal.pixel_width,
            pixel_height: terminal.pixel_height,
            dpi: terminal.dpi,
            current_dir: terminal.current_dir.as_ref().map(ToString::to_string),
            term_program: terminal.term_program.clone(),
            term_version: terminal.term_version.clone(),
            sixel_scrolls_right: terminal.sixel_scrolls_right,
            user_vars: terminal
                .user_vars
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            seqno: terminal.seqno,
            unicode_version: CheckpointUnicodeVersion::from(&terminal.unicode_version),
            unicode_version_stack: terminal
                .unicode_version_stack
                .iter()
                .map(|entry| CheckpointUnicodeVersionStackEntry {
                    version: CheckpointUnicodeVersion::from(&entry.vers),
                    label: entry.label.clone(),
                })
                .collect(),
            enable_conpty_quirks: terminal.enable_conpty_quirks,
            suppress_initial_title_change: terminal.suppress_initial_title_change,
            accumulating_title: terminal.accumulating_title.clone(),
            lost_focus_seqno: terminal.lost_focus_seqno,
            lost_focus_alerted_seqno: terminal.lost_focus_alerted_seqno,
            focused: terminal.focused,
            bidi_enabled: terminal.bidi_enabled,
            bidi_hint: terminal.bidi_hint.map(CheckpointBidiHint::from),
        })
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum TerminalCheckpointError {
    UnsupportedGraphicsState,
}

impl std::fmt::Display for TerminalCheckpointError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedGraphicsState => formatter.write_str(
                "terminal contains graphics state unsupported by checkpoint v1",
            ),
        }
    }
}

impl std::error::Error for TerminalCheckpointError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Terminal;
    use std::sync::Arc;

    #[derive(Debug)]
    struct CheckpointTestConfig;

    impl TerminalConfiguration for CheckpointTestConfig {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
    }

    fn terminal() -> Terminal {
        Terminal::new(
            TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            },
            Arc::new(CheckpointTestConfig),
            "FrankenTerm",
            "checkpoint-test",
            Box::new(Vec::<u8>::new()),
        )
    }

    #[test]
    fn semantic_projection_roundtrips_and_tracks_both_screens() {
        let mut terminal = terminal();
        terminal.advance_bytes(b"primary\x1b]2;checkpoint-title\x07");
        terminal.advance_bytes(b"\x1b[?1049h\x1b[31malternate");
        let checkpoint = TerminalCheckpointV1::capture(&terminal).expect("capture terminal");
        let encoded = serde_json::to_vec(&checkpoint).expect("serialize checkpoint");
        let decoded: TerminalCheckpointV1 =
            serde_json::from_slice(&encoded).expect("deserialize checkpoint");

        assert_eq!(decoded, checkpoint);
        assert!(checkpoint.alternate_screen_active);
        assert_eq!(checkpoint.title, "checkpoint-title");
        assert_ne!(checkpoint.primary_screen.lines, checkpoint.alternate_screen.lines);
    }

    #[test]
    fn unsupported_out_of_band_graphics_fail_closed() {
        let mut terminal = terminal();
        terminal.kitty_img.mark_nonempty_for_checkpoint_test();

        assert_eq!(
            TerminalCheckpointV1::capture(&terminal),
            Err(TerminalCheckpointError::UnsupportedGraphicsState)
        );
    }

    #[test]
    fn canonical_projection_sorts_terminal_maps() {
        let mut first = terminal();
        first.user_vars.insert("zeta".into(), "last".into());
        first.user_vars.insert("alpha".into(), "first".into());
        let mut second = terminal();
        second.user_vars.insert("alpha".into(), "first".into());
        second.user_vars.insert("zeta".into(), "last".into());

        let first = TerminalCheckpointV1::capture(&first).expect("capture first terminal");
        let second = TerminalCheckpointV1::capture(&second).expect("capture second terminal");
        assert_eq!(
            serde_json::to_vec(&first).expect("serialize first terminal"),
            serde_json::to_vec(&second).expect("serialize second terminal")
        );
    }
}
