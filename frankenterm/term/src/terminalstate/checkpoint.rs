//! Canonical semantic terminal-state checkpoint model.
//!
//! This module projects live terminal state into deterministic, capability-free
//! data. It does not own filesystem publication or raw-output identity; the mux
//! guardian binds encoded bytes to `GuardianCheckpointBoundary` after capture.

use super::*;
use frankenterm_escape_parser::csi::KittyKeyboardFlags;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::convert::{TryFrom, TryInto};
use std::sync::Arc;

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

impl CheckpointCharSet {
    const fn into_live(self) -> CharSet {
        match self {
            Self::Ascii => CharSet::Ascii,
            Self::Uk => CharSet::Uk,
            Self::DecLineDrawing => CharSet::DecLineDrawing,
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

impl CheckpointMouseEncoding {
    const fn into_live(self) -> MouseEncoding {
        match self {
            Self::X10 => MouseEncoding::X10,
            Self::Utf8 => MouseEncoding::Utf8,
            Self::Sgr => MouseEncoding::SGR,
            Self::SgrPixels => MouseEncoding::SgrPixels,
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

impl CheckpointKeyboardEncoding {
    fn into_live(self) -> Result<KeyboardEncoding, TerminalCheckpointError> {
        Ok(match self {
            Self::Xterm => KeyboardEncoding::Xterm,
            Self::CsiU => KeyboardEncoding::CsiU,
            Self::Win32 => KeyboardEncoding::Win32,
            Self::Kitty(bits) => KeyboardEncoding::Kitty(
                KittyKeyboardFlags::from_bits(bits).ok_or(
                    TerminalCheckpointError::InvalidField {
                        field: "keyboard_encoding",
                        reason: "unknown Kitty keyboard flag bits",
                    },
                )?,
            ),
        })
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

impl CheckpointBidiHint {
    const fn into_live(self) -> ParagraphDirectionHint {
        match self {
            Self::LeftToRight => ParagraphDirectionHint::LeftToRight,
            Self::RightToLeft => ParagraphDirectionHint::RightToLeft,
            Self::AutoLeftToRight => ParagraphDirectionHint::AutoLeftToRight,
            Self::AutoRightToLeft => ParagraphDirectionHint::AutoRightToLeft,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointHyperlink {
    params: BTreeMap<String, String>,
    uri: String,
    implicit: bool,
}

impl CheckpointHyperlink {
    fn capture(link: &Hyperlink) -> Self {
        Self {
            params: link
                .params()
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            uri: link.uri().to_string(),
            implicit: link.is_implicit(),
        }
    }

    fn into_live(self) -> Hyperlink {
        Hyperlink::new_with_params_and_implicit(
            self.uri,
            self.params.into_iter().collect::<HashMap<_, _>>(),
            self.implicit,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointCellAttributes {
    intensity: Intensity,
    underline: Underline,
    blink: Blink,
    italic: bool,
    reverse: bool,
    strikethrough: bool,
    invisible: bool,
    wrapped: bool,
    overline: bool,
    semantic_type: SemanticType,
    vertical_align: VerticalAlign,
    foreground: ColorAttribute,
    background: ColorAttribute,
    underline_color: ColorAttribute,
    hyperlink: Option<CheckpointHyperlink>,
}

impl CheckpointCellAttributes {
    fn capture(value: &CellAttributes) -> Result<Self, TerminalCheckpointError> {
        if value.has_image_attachments() {
            return Err(TerminalCheckpointError::UnsupportedGraphicsState);
        }
        Ok(Self {
            intensity: value.intensity(),
            underline: value.underline(),
            blink: value.blink(),
            italic: value.italic(),
            reverse: value.reverse(),
            strikethrough: value.strikethrough(),
            invisible: value.invisible(),
            wrapped: value.wrapped(),
            overline: value.overline(),
            semantic_type: value.semantic_type(),
            vertical_align: value.vertical_align(),
            foreground: value.foreground(),
            background: value.background(),
            underline_color: value.underline_color(),
            hyperlink: value
                .hyperlink()
                .map(|link| CheckpointHyperlink::capture(link)),
        })
    }

    fn into_live(self) -> CellAttributes {
        let mut value = CellAttributes::blank();
        value
            .set_intensity(self.intensity)
            .set_underline(self.underline)
            .set_blink(self.blink)
            .set_italic(self.italic)
            .set_reverse(self.reverse)
            .set_strikethrough(self.strikethrough)
            .set_invisible(self.invisible)
            .set_wrapped(self.wrapped)
            .set_overline(self.overline)
            .set_semantic_type(self.semantic_type)
            .set_vertical_align(self.vertical_align)
            .set_foreground(self.foreground)
            .set_background(self.background)
            .set_underline_color(self.underline_color);
        value.set_hyperlink(
            self.hyperlink
                .map(|link| Arc::new(link.into_live())),
        );
        value
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointCell {
    text: String,
    width: u8,
    attributes: CheckpointCellAttributes,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointLineScale {
    Single,
    DoubleWidth,
    DoubleHeightTop,
    DoubleHeightBottom,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointLine {
    cells: Vec<CheckpointCell>,
    seqno: SequenceNo,
    scale: CheckpointLineScale,
    bidi_enabled: bool,
    bidi_hint: CheckpointBidiHint,
}

impl CheckpointLine {
    fn capture(line: &Line) -> Result<Self, TerminalCheckpointError> {
        if line.has_image_attachments() {
            return Err(TerminalCheckpointError::UnsupportedGraphicsState);
        }
        let mut cells = Vec::new();
        for cell in line.visible_cells() {
            let width = u8::try_from(cell.width()).map_err(|_| {
                TerminalCheckpointError::InvalidField {
                    field: "cell.width",
                    reason: "cell width does not fit the v1 wire type",
                }
            })?;
            if !(1..=2).contains(&width) {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "cell.width",
                    reason: "cell width must be one or two columns",
                });
            }
            cells.push(CheckpointCell {
                text: cell.str().to_string(),
                width,
                attributes: CheckpointCellAttributes::capture(cell.attrs())?,
            });
        }
        let scale = if line.is_double_height_top() {
            CheckpointLineScale::DoubleHeightTop
        } else if line.is_double_height_bottom() {
            CheckpointLineScale::DoubleHeightBottom
        } else if line.is_double_width() {
            CheckpointLineScale::DoubleWidth
        } else {
            CheckpointLineScale::Single
        };
        let (bidi_enabled, bidi_hint) = line.bidi_info();
        Ok(Self {
            cells,
            seqno: line.current_seqno(),
            scale,
            bidi_enabled,
            bidi_hint: bidi_hint.into(),
        })
    }

    fn into_live(self) -> Line {
        let mut cells = Vec::new();
        for cell in self.cells {
            let width = usize::from(cell.width);
            let attributes = cell.attributes.into_live();
            cells.push(Cell::new_grapheme_with_width(
                &cell.text,
                width,
                attributes.clone(),
            ));
            for _ in 1..width {
                cells.push(Cell::blank_with_attrs(attributes.clone()));
            }
        }
        let mut line = Line::from_cells(cells, self.seqno);
        match self.scale {
            CheckpointLineScale::Single => {}
            CheckpointLineScale::DoubleWidth => line.set_double_width(self.seqno),
            CheckpointLineScale::DoubleHeightTop => line.set_double_height_top(self.seqno),
            CheckpointLineScale::DoubleHeightBottom => line.set_double_height_bottom(self.seqno),
        }
        line.set_bidi_info(self.bidi_enabled, self.bidi_hint.into_live(), self.seqno);
        line.rebuild_checkpoint_hyperlink_bits();
        line
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

impl CheckpointUnicodeVersion {
    fn into_live(self) -> UnicodeVersion {
        UnicodeVersion {
            version: self.version,
            ambiguous_are_wide: self.ambiguous_are_wide,
            cell_widths: if self.custom_cell_widths.is_empty() {
                None
            } else {
                Some(Arc::new(self.custom_cell_widths.into_iter().collect()))
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointSavedCursor {
    position: CursorPosition,
    wrap_next: bool,
    pen: CheckpointCellAttributes,
    dec_origin_mode: bool,
    g0_charset: CheckpointCharSet,
    g1_charset: CheckpointCharSet,
}

impl CheckpointSavedCursor {
    fn capture(value: &SavedCursor) -> Result<Self, TerminalCheckpointError> {
        Ok(Self {
            position: value.position,
            wrap_next: value.wrap_next,
            pen: CheckpointCellAttributes::capture(&value.pen)?,
            dec_origin_mode: value.dec_origin_mode,
            g0_charset: value.g0_charset.into(),
            g1_charset: value.g1_charset.into(),
        })
    }

    fn into_live(self) -> SavedCursor {
        SavedCursor {
            position: self.position,
            wrap_next: self.wrap_next,
            pen: self.pen.into_live(),
            dec_origin_mode: self.dec_origin_mode,
            g0_charset: self.g0_charset.into_live(),
            g1_charset: self.g1_charset.into_live(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointScreen {
    lines: Vec<CheckpointLine>,
    stable_row_index_offset: usize,
    allow_scrollback: bool,
    keyboard_stack: Vec<CheckpointKeyboardEncoding>,
    physical_rows: usize,
    physical_cols: usize,
    dpi: u32,
    saved_cursor: Option<CheckpointSavedCursor>,
}

impl CheckpointScreen {
    fn capture(
        value: crate::screen::ScreenCheckpointParts,
    ) -> Result<Self, TerminalCheckpointError> {
        Ok(Self {
            lines: value
                .lines
                .iter()
                .map(CheckpointLine::capture)
                .collect::<Result<Vec<_>, _>>()?,
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
            saved_cursor: value
                .saved_cursor
                .as_ref()
                .map(CheckpointSavedCursor::capture)
                .transpose()?,
        })
    }

    fn into_live(self) -> Result<crate::screen::ScreenCheckpointParts, TerminalCheckpointError> {
        Ok(crate::screen::ScreenCheckpointParts {
            lines: self.lines.into_iter().map(CheckpointLine::into_live).collect(),
            stable_row_index_offset: self.stable_row_index_offset,
            allow_scrollback: self.allow_scrollback,
            keyboard_stack: self
                .keyboard_stack
                .into_iter()
                .map(CheckpointKeyboardEncoding::into_live)
                .collect::<Result<Vec<_>, _>>()?,
            physical_rows: self.physical_rows,
            physical_cols: self.physical_cols,
            dpi: self.dpi,
            saved_cursor: self.saved_cursor.map(CheckpointSavedCursor::into_live),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointUnicodeVersionStackEntry {
    version: CheckpointUnicodeVersion,
    label: Option<String>,
}

/// Hard resource envelope for semantic checkpoint capture, decoding, and
/// restoration.  Callers may choose a smaller policy, but every field remains
/// bounded and checked with overflow detection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalCheckpointLimits {
    pub max_encoded_bytes: usize,
    pub max_rows: usize,
    pub max_cols: usize,
    pub max_total_lines: usize,
    pub max_total_cells: usize,
    pub max_total_cell_text_bytes: usize,
    pub max_total_hyperlink_bytes: usize,
    pub max_total_hyperlink_params: usize,
    pub max_string_bytes: usize,
    pub max_hyperlink_params_per_link: usize,
    pub max_cold_scrollback_bytes: usize,
    pub max_keyboard_stack_depth: usize,
    pub max_saved_dec_private_modes: usize,
    pub max_color_registers: usize,
    pub max_mouse_buttons: usize,
    pub max_tab_stops: usize,
    pub max_user_vars: usize,
    pub max_unicode_stack_depth: usize,
    pub max_custom_cell_widths: usize,
    pub max_terminal_string_bytes: usize,
    pub max_pixel_dimension: usize,
    pub max_dpi: u32,
}

impl Default for TerminalCheckpointLimits {
    fn default() -> Self {
        Self {
            max_encoded_bytes: 256 * 1024 * 1024,
            max_rows: 4_096,
            max_cols: 16_384,
            max_total_lines: 262_144,
            max_total_cells: 16 * 1024 * 1024,
            max_total_cell_text_bytes: 128 * 1024 * 1024,
            max_total_hyperlink_bytes: 32 * 1024 * 1024,
            max_total_hyperlink_params: 1024 * 1024,
            max_string_bytes: 1024 * 1024,
            max_hyperlink_params_per_link: 256,
            max_cold_scrollback_bytes: 128 * 1024 * 1024,
            max_keyboard_stack_depth: 128,
            max_saved_dec_private_modes: 256,
            max_color_registers: 4_096,
            max_mouse_buttons: 3,
            max_tab_stops: 32_768,
            max_user_vars: 512,
            max_unicode_stack_depth: 64,
            max_custom_cell_widths: 1_048_576,
            max_terminal_string_bytes: 8 * 1024 * 1024,
            max_pixel_dimension: 1_048_576,
            max_dpi: 1_000_000,
        }
    }
}

impl TerminalCheckpointLimits {
    fn screen_limits(self) -> crate::screen::ScreenCheckpointLimits {
        crate::screen::ScreenCheckpointLimits {
            max_total_lines: self.max_total_lines,
            max_total_cells: self.max_total_cells,
            max_total_cell_text_bytes: self.max_total_cell_text_bytes,
            max_total_hyperlink_bytes: self.max_total_hyperlink_bytes,
            max_total_hyperlink_params: self.max_total_hyperlink_params,
            max_string_bytes: self.max_string_bytes,
            max_hyperlink_params_per_link: self.max_hyperlink_params_per_link,
            max_cold_scrollback_bytes: self.max_cold_scrollback_bytes,
            max_keyboard_stack_depth: self.max_keyboard_stack_depth,
            max_rows: self.max_rows,
            max_cols: self.max_cols,
        }
    }
}

/// Capability-free semantic projection of one terminal model.
///
/// Fields remain private so callers cannot construct an unvalidated authority;
/// serde decoding will be paired with a validating constructor before restore.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalCheckpointV1 {
    version: u32,
    primary_screen: CheckpointScreen,
    alternate_screen: CheckpointScreen,
    alternate_screen_active: bool,
    pen: CheckpointCellAttributes,
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
    kitty_max_image_id: u32,
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
        Self::capture_with_limits(terminal, TerminalCheckpointLimits::default())
    }

    /// Capture under an explicit hard resource envelope.  Both screens are
    /// preflighted before resident lines are cloned, and reachable cold rows are
    /// materialized only after their sink ledger passes the same envelope.
    pub fn capture_with_limits(
        terminal: &TerminalState,
        limits: TerminalCheckpointLimits,
    ) -> Result<Self, TerminalCheckpointError> {
        Self::preflight_terminal_fields(terminal, limits)?;
        let kitty_max_image_id = terminal
            .kitty_img
            .checkpoint_high_water_if_quiescent()
            .ok_or(TerminalCheckpointError::UnsupportedGraphicsState)?;
        let screen_limits = limits.screen_limits();
        let mut screen_usage = crate::screen::ScreenCheckpointUsage::default();
        Self::preflight_checkpoint_attributes(
            &terminal.pen,
            limits,
            &mut screen_usage,
        )?;
        for saved in [
            terminal.screen.screen.saved_cursor.as_ref(),
            terminal.screen.alt_screen.saved_cursor.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            Self::preflight_checkpoint_attributes(
                &saved.pen,
                limits,
                &mut screen_usage,
            )?;
        }
        let primary_screen = terminal
            .screen
            .screen
            .checkpoint_parts(&screen_limits, &mut screen_usage)?;
        let alternate_screen = terminal
            .screen
            .alt_screen
            .checkpoint_parts(&screen_limits, &mut screen_usage)?;

        let checkpoint = Self {
            version: TERMINAL_CHECKPOINT_VERSION,
            primary_screen: CheckpointScreen::capture(primary_screen)?,
            alternate_screen: CheckpointScreen::capture(alternate_screen)?,
            alternate_screen_active: terminal.screen.alt_screen_is_active,
            pen: CheckpointCellAttributes::capture(&terminal.pen)?,
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
            kitty_max_image_id,
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
        };
        checkpoint.validate(limits)?;
        Ok(checkpoint)
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum TerminalCheckpointError {
    UnsupportedGraphicsState,
    UnsupportedVersion {
        observed: u32,
        supported: u32,
    },
    ResourceLimit {
        resource: &'static str,
        observed: usize,
        maximum: usize,
    },
    ArithmeticOverflow(&'static str),
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },
    ColdScrollbackMetadataInconsistent,
    ColdScrollbackRowMissing(StableRowIndex),
    InvalidCurrentDirectory,
    Serialization(String),
    NonCanonicalEncoding,
}

impl From<crate::screen::ScreenCheckpointCaptureError> for TerminalCheckpointError {
    fn from(value: crate::screen::ScreenCheckpointCaptureError) -> Self {
        match value {
            crate::screen::ScreenCheckpointCaptureError::ResourceLimit {
                resource,
                observed,
                maximum,
            } => Self::ResourceLimit {
                resource,
                observed,
                maximum,
            },
            crate::screen::ScreenCheckpointCaptureError::ArithmeticOverflow(resource) => {
                Self::ArithmeticOverflow(resource)
            }
            crate::screen::ScreenCheckpointCaptureError::InvalidLineGeometry { .. } => {
                Self::InvalidField {
                    field: "screen.lines",
                    reason: "stored and semantic cell geometry differ",
                }
            }
            crate::screen::ScreenCheckpointCaptureError::UnsupportedGraphicsState => {
                Self::UnsupportedGraphicsState
            }
            crate::screen::ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent => {
                Self::ColdScrollbackMetadataInconsistent
            }
            crate::screen::ScreenCheckpointCaptureError::ColdScrollbackRowMissing(row) => {
                Self::ColdScrollbackRowMissing(row)
            }
        }
    }
}

impl std::fmt::Display for TerminalCheckpointError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedGraphicsState => formatter.write_str(
                "terminal contains graphics state unsupported by checkpoint v1",
            ),
            Self::UnsupportedVersion {
                observed,
                supported,
            } => write!(
                formatter,
                "terminal checkpoint version {observed} is unsupported; expected {supported}"
            ),
            Self::ResourceLimit {
                resource,
                observed,
                maximum,
            } => write!(
                formatter,
                "terminal checkpoint {resource} exceeds its limit: {observed} > {maximum}"
            ),
            Self::ArithmeticOverflow(resource) => {
                write!(formatter, "terminal checkpoint {resource} accounting overflowed")
            }
            Self::InvalidField { field, reason } => {
                write!(formatter, "terminal checkpoint field {field} is invalid: {reason}")
            }
            Self::ColdScrollbackMetadataInconsistent => formatter.write_str(
                "terminal checkpoint cold scrollback metadata changed or is inconsistent",
            ),
            Self::ColdScrollbackRowMissing(row) => write!(
                formatter,
                "terminal checkpoint cold scrollback row {row} could not be hydrated"
            ),
            Self::InvalidCurrentDirectory => {
                formatter.write_str("terminal checkpoint current directory is not a valid URL")
            }
            Self::Serialization(error) => {
                write!(formatter, "terminal checkpoint serialization failed: {error}")
            }
            Self::NonCanonicalEncoding => formatter.write_str(
                "terminal checkpoint bytes are not the canonical v1 representation",
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
