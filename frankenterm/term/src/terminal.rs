use super::*;
use crate::terminalstate::performer::Performer;
use frankenterm_escape_parser::parser::Parser;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum ClipboardSelection {
    Clipboard,
    PrimarySelection,
}

pub trait Clipboard: Send + Sync {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        data: Option<String>,
    ) -> anyhow::Result<()>;
}

impl Clipboard for Box<dyn Clipboard> {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        data: Option<String>,
    ) -> anyhow::Result<()> {
        self.as_ref().set_contents(selection, data)
    }
}

pub trait DeviceControlHandler: Send + Sync {
    fn handle_device_control(&mut self, _control: frankenterm_escape_parser::DeviceControlMode);
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum Progress {
    #[default]
    None,
    Percentage(u8),
    Error(u8),
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum Alert {
    Bell,
    ToastNotification {
        /// The title text for the notification.
        title: Option<String>,
        /// The message body
        body: String,
        /// Whether clicking on the notification should focus the
        /// window/tab/pane that generated it
        focus: bool,
    },
    CurrentWorkingDirectoryChanged,
    IconTitleChanged(Option<String>),
    WindowTitleChanged(String),
    TabTitleChanged(Option<String>),
    /// When the color palette has been updated
    PaletteChanged,
    /// A UserVar has changed value
    SetUserVar {
        name: String,
        value: String,
    },
    /// When something bumps the seqno in the terminal model and
    /// the terminal is not focused
    OutputSinceFocusLost,
    /// A change to the progress bar state
    Progress(Progress),
    /// An app sent `OSC 1337;SetProfile=<name>` to request a profile
    /// switch. Per ft-fy4ty's security gate, the term layer never
    /// applies the switch silently — it surfaces the request to the
    /// embedder via this alert and lets the embedder show a user
    /// confirmation prompt before any profile mutation happens.
    /// Embedders that want the legacy iTerm2 silent-switch behavior
    /// must opt in via their own confirmation logic.
    SetProfileRequested {
        /// The profile name the application asked for.
        name: String,
    },
    /// An app sent `OSC 22;<shape>` to request a mouse cursor shape
    /// change. The argument is the free-form W3C-style cursor name
    /// (`pointer`, `text`, `wait`, …). Per ft-7yiu2 the term layer
    /// just routes the request through; the GUI maps the string to
    /// a native cursor. Unrecognized values are dropped by the
    /// embedder, not by the term layer.
    MouseShapeRequested {
        /// The shape name, as supplied by the application.
        shape: String,
    },
    /// A Kitty graphics image carried alt-text suitable for
    /// accessibility announcement after sanitization.
    ImageAltText {
        /// The admitted Kitty image id.
        image_id: u32,
        /// Sanitized screen-reader text.
        text: String,
    },
}

pub trait AlertHandler: Send + Sync {
    fn alert(&mut self, alert: Alert);
}

pub trait DownloadHandler: Send + Sync {
    fn save_to_downloads(&self, name: Option<String>, data: Vec<u8>);
}

/// Represents an instance of a terminal emulator.
pub struct Terminal {
    /// The terminal model/state
    state: TerminalState,
    /// Baseline terminal escape sequence parser
    parser: Parser,
}

impl Deref for Terminal {
    type Target = TerminalState;

    fn deref(&self) -> &TerminalState {
        &self.state
    }
}

impl DerefMut for Terminal {
    fn deref_mut(&mut self) -> &mut TerminalState {
        &mut self.state
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, FromDynamic, ToDynamic)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub struct TerminalSize {
    pub rows: usize,
    pub cols: usize,
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub dpi: u32,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        }
    }
}

impl Terminal {
    /// Construct a new Terminal.
    /// `physical_rows` and `physical_cols` describe the dimensions
    /// of the visible portion of the terminal display in terms of
    /// the number of text cells.
    ///
    /// `pixel_width` and `pixel_height` describe the dimensions of
    /// that same visible area but in pixels.
    ///
    /// `term_program` and `term_version` are required to identify
    /// the host terminal program; they are used to respond to the
    /// terminal identification sequence `\033[>q`.
    ///
    /// `writer` is anything that implements `std::io::Write`; it
    /// is used to send input to the connected program; both keyboard
    /// and mouse input is encoded and written to that stream, as
    /// are answerback responses to a number of escape sequences.
    pub fn new(
        size: TerminalSize,
        config: Arc<dyn TerminalConfiguration + Send + Sync>,
        term_program: &str,
        term_version: &str,
        // writing to the writer sends data to input of the pty
        writer: Box<dyn std::io::Write + Send>,
    ) -> Terminal {
        Terminal {
            state: TerminalState::new(size, config, term_program, term_version, writer),
            parser: Parser::new(),
        }
    }

    /// Feed the terminal parser a slice of bytes from the output
    /// of the associated program.
    /// The slice is not required to be a complete sequence of escape
    /// characters; it is valid to feed in chunks of data as they arrive.
    /// The output is parsed and applied to the terminal model.
    pub fn advance_bytes<B: AsRef<[u8]>>(&mut self, bytes: B) {
        self.state.increment_seqno();
        {
            let bytes = bytes.as_ref();

            let mut performer = Performer::new(&mut self.state);

            self.parser.parse(bytes, |action| performer.perform(action));
        }
        self.trigger_unseen_output_notif();
    }

    pub fn perform_actions(&mut self, actions: Vec<frankenterm_escape_parser::Action>) {
        self.state.increment_seqno();
        {
            let mut performer = Performer::new(&mut self.state);
            for action in actions {
                performer.perform(action);
            }
        }
        self.trigger_unseen_output_notif();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ColorPalette;
    use crate::{CellAttributes, CursorPosition, Line};
    use proptest::prelude::*;
    use std::sync::Arc;

    #[derive(Debug)]
    struct PropTermConfig;

    impl TerminalConfiguration for PropTermConfig {
        fn scrollback_size(&self) -> usize {
            64
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

    fn make_prop_term(rows: usize, cols: usize) -> Terminal {
        Terminal::new(
            TerminalSize {
                rows,
                cols,
                pixel_width: cols * 8,
                pixel_height: rows * 16,
                dpi: 96,
            },
            Arc::new(PropTermConfig),
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

    fn snapshot_term(term: &Terminal) -> TerminalSnapshot {
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

    fn chunked_snapshot(payload: &[u8], chunk_sizes: &[usize]) -> TerminalSnapshot {
        let mut term = make_prop_term(8, 16);
        let mut offset = 0;
        for size in chunk_sizes {
            if offset >= payload.len() {
                break;
            }
            let end = (offset + (*size).max(1)).min(payload.len());
            term.advance_bytes(&payload[offset..end]);
            offset = end;
        }
        if offset < payload.len() {
            term.advance_bytes(&payload[offset..]);
        }
        snapshot_term(&term)
    }

    fn single_snapshot(payload: &[u8]) -> TerminalSnapshot {
        let mut term = make_prop_term(8, 16);
        term.advance_bytes(payload);
        snapshot_term(&term)
    }

    fn arb_chunk_sizes() -> impl Strategy<Value = Vec<usize>> {
        proptest::collection::vec(1usize..8, 0..24)
    }

    fn arb_ascii_text() -> impl Strategy<Value = String> {
        proptest::collection::vec(0x20u8..=0x7Eu8, 0..48)
            .prop_map(|bytes| bytes.into_iter().map(char::from).collect())
    }

    fn arb_safe_label() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                (b'A'..=b'Z').prop_map(char::from),
                (b'a'..=b'z').prop_map(char::from),
                (b'0'..=b'9').prop_map(char::from),
                Just(' '),
                Just('_'),
                Just('.'),
                Just('/'),
                Just(':'),
                Just('-'),
            ],
            0..24,
        )
        .prop_map(|chars| chars.into_iter().collect())
    }

    fn arb_multibyte_text() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just("a"),
                Just("b"),
                Just(" "),
                Just("\u{00e9}"),
                Just("\u{03bb}"),
                Just("\u{4e2d}"),
                Just("\u{8a9e}"),
                Just("\u{1f980}"),
            ],
            0..32,
        )
        .prop_map(|parts| parts.concat())
    }

    fn arb_control_stream() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just("a"),
                Just("b"),
                Just("c"),
                Just("\r"),
                Just("\n"),
                Just("\x08"),
                Just("\t"),
            ],
            0..40,
        )
        .prop_map(|parts| parts.concat())
    }

    #[derive(Debug, Clone)]
    enum PropScrollAction {
        PrintRun(u8, usize),
        CarriageReturn,
        LineFeed,
        ReverseIndex,
        ScrollUp(u8),
        ScrollDown(u8),
        CursorToTop,
        CursorToBottom,
    }

    fn arb_scroll_action() -> impl Strategy<Value = PropScrollAction> {
        prop_oneof![
            (b'A'..=b'Z', 1usize..=24)
                .prop_map(|(byte, len)| { PropScrollAction::PrintRun(byte, len) }),
            Just(PropScrollAction::CarriageReturn),
            Just(PropScrollAction::LineFeed),
            Just(PropScrollAction::ReverseIndex),
            (1u8..=4).prop_map(PropScrollAction::ScrollUp),
            (1u8..=4).prop_map(PropScrollAction::ScrollDown),
            Just(PropScrollAction::CursorToTop),
            Just(PropScrollAction::CursorToBottom),
        ]
    }

    fn arb_scroll_region() -> impl Strategy<Value = (u8, u8)> {
        (1u8..=7).prop_flat_map(|top| (Just(top), (top + 1)..=8))
    }

    fn scrolling_payload(
        auto_wrap: bool,
        top: u8,
        bottom: u8,
        actions: &[PropScrollAction],
    ) -> Vec<u8> {
        let wrap_mode = if auto_wrap { "h" } else { "l" };
        let mut payload = format!("\x1b[?7{wrap_mode}\x1b[{top};{bottom}r").into_bytes();

        for row in 1..=8 {
            let run = if row % 2 == 0 { 20 } else { 12 };
            payload.extend_from_slice(format!("\x1b[{row};1H").as_bytes());
            for _ in 0..run {
                payload.push(b'0' + row);
            }
        }

        payload.extend_from_slice(format!("\x1b[{bottom};1H").as_bytes());
        for action in actions {
            match *action {
                PropScrollAction::PrintRun(byte, len) => {
                    for _ in 0..len {
                        payload.push(byte);
                    }
                }
                PropScrollAction::CarriageReturn => payload.push(b'\r'),
                PropScrollAction::LineFeed => payload.push(b'\n'),
                PropScrollAction::ReverseIndex => payload.extend_from_slice(b"\x1bM"),
                PropScrollAction::ScrollUp(lines) => {
                    payload.extend_from_slice(format!("\x1b[{lines}S").as_bytes());
                }
                PropScrollAction::ScrollDown(lines) => {
                    payload.extend_from_slice(format!("\x1b[{lines}T").as_bytes());
                }
                PropScrollAction::CursorToTop => {
                    payload.extend_from_slice(format!("\x1b[{top};1H").as_bytes());
                }
                PropScrollAction::CursorToBottom => {
                    payload.extend_from_slice(format!("\x1b[{bottom};1H").as_bytes());
                }
            }
        }

        payload
    }

    fn assert_cell_grid_invariants(
        term: &Terminal,
        rows: usize,
        cols: usize,
        auto_wrap: bool,
        top: u8,
        bottom: u8,
        actions: &[PropScrollAction],
    ) -> Result<(), proptest::test_runner::TestCaseError> {
        let cursor = term.cursor_pos();
        prop_assert!(
            cursor.x <= cols,
            "cursor column out of bounds: cursor={cursor:?} cols={cols} auto_wrap={auto_wrap} region={top}..={bottom} actions={actions:?}"
        );
        prop_assert!(
            cursor.y >= 0 && (cursor.y as usize) < rows,
            "cursor row out of bounds: cursor={cursor:?} rows={rows} auto_wrap={auto_wrap} region={top}..={bottom} actions={actions:?}"
        );

        let screen = term.screen();
        let phys_row = screen.phys_row(cursor.y);
        prop_assert!(
            phys_row < screen.scrollback_rows(),
            "cursor phys row {phys_row} outside scrollback rows {} auto_wrap={auto_wrap} region={top}..={bottom} actions={actions:?}",
            screen.scrollback_rows()
        );

        let stable_row = screen.visible_row_to_stable_row(cursor.y);
        let mapped_phys = screen
            .stable_row_to_phys(stable_row)
            .expect("cursor stable row must map to a physical row");
        prop_assert_eq!(
            mapped_phys,
            phys_row,
            "cursor visible/stable/physical row mapping must roundtrip auto_wrap={} region={}..={} actions={:?}",
            auto_wrap,
            top,
            bottom,
            actions
        );

        let all_lines = screen.all_lines();
        prop_assert!(
            all_lines.len() >= rows,
            "screen lost visible rows: all_lines={} rows={rows} auto_wrap={auto_wrap} region={top}..={bottom} actions={actions:?}",
            all_lines.len()
        );
        prop_assert_eq!(
            all_lines.len(),
            screen.scrollback_rows(),
            "screen all_lines and scrollback row count diverged auto_wrap={} region={}..={} actions={:?}",
            auto_wrap,
            top,
            bottom,
            actions
        );

        for (line_idx, line) in all_lines.iter().enumerate() {
            prop_assert!(
                line.len() <= cols,
                "line {line_idx} exceeds terminal width: len={} cols={cols} auto_wrap={auto_wrap} region={top}..={bottom} actions={actions:?}",
                line.len()
            );
            for cell in line.visible_cells() {
                prop_assert!(
                    (1..=cols).contains(&cell.width()),
                    "line {line_idx} has invalid cell width {} auto_wrap={auto_wrap} region={top}..={bottom} actions={actions:?}",
                    cell.width()
                );
            }
        }

        Ok(())
    }

    #[test]
    fn clipboard_selection_equality() {
        assert_eq!(ClipboardSelection::Clipboard, ClipboardSelection::Clipboard);
        assert_eq!(
            ClipboardSelection::PrimarySelection,
            ClipboardSelection::PrimarySelection
        );
        assert_ne!(
            ClipboardSelection::Clipboard,
            ClipboardSelection::PrimarySelection
        );
    }

    #[test]
    fn clipboard_selection_debug() {
        let dbg = format!("{:?}", ClipboardSelection::Clipboard);
        assert_eq!(dbg, "Clipboard");
    }

    #[test]
    fn clipboard_selection_clone() {
        let sel = ClipboardSelection::PrimarySelection;
        let cloned = sel;
        assert_eq!(sel, cloned);
    }

    #[test]
    fn progress_default_is_none() {
        assert_eq!(Progress::default(), Progress::None);
    }

    #[test]
    fn progress_equality() {
        assert_eq!(Progress::None, Progress::None);
        assert_eq!(Progress::Percentage(50), Progress::Percentage(50));
        assert_ne!(Progress::Percentage(50), Progress::Percentage(75));
        assert_eq!(Progress::Error(1), Progress::Error(1));
        assert_ne!(Progress::Error(1), Progress::Error(2));
        assert_eq!(Progress::Indeterminate, Progress::Indeterminate);
        assert_ne!(Progress::None, Progress::Indeterminate);
    }

    #[test]
    fn progress_clone() {
        let p = Progress::Percentage(42);
        let cloned = p.clone();
        assert_eq!(p, cloned);
    }

    #[test]
    fn progress_debug() {
        assert!(format!("{:?}", Progress::None).contains("None"));
        assert!(format!("{:?}", Progress::Percentage(50)).contains("50"));
        assert!(format!("{:?}", Progress::Error(1)).contains("Error"));
        assert!(format!("{:?}", Progress::Indeterminate).contains("Indeterminate"));
    }

    #[test]
    fn alert_bell() {
        let a = Alert::Bell;
        let b = Alert::Bell;
        assert_eq!(a, b);
    }

    #[test]
    fn alert_toast_notification() {
        let alert = Alert::ToastNotification {
            title: Some("Title".to_string()),
            body: "Body text".to_string(),
            focus: true,
        };
        let alert2 = alert.clone();
        assert_eq!(alert, alert2);
    }

    #[test]
    fn alert_toast_notification_no_title() {
        let alert = Alert::ToastNotification {
            title: None,
            body: "message".to_string(),
            focus: false,
        };
        assert!(matches!(&alert, Alert::ToastNotification { .. }));
        if let Alert::ToastNotification { title, body, focus } = &alert {
            assert!(title.is_none());
            assert_eq!(body, "message");
            assert!(!focus);
        }
    }

    #[test]
    fn alert_variants_inequality() {
        assert_ne!(Alert::Bell, Alert::PaletteChanged);
        assert_ne!(
            Alert::CurrentWorkingDirectoryChanged,
            Alert::OutputSinceFocusLost
        );
    }

    #[test]
    fn alert_set_user_var() {
        let a = Alert::SetUserVar {
            name: "foo".to_string(),
            value: "bar".to_string(),
        };
        let b = Alert::SetUserVar {
            name: "foo".to_string(),
            value: "bar".to_string(),
        };
        assert_eq!(a, b);
    }

    #[test]
    fn alert_progress() {
        let a = Alert::Progress(Progress::Percentage(75));
        let b = Alert::Progress(Progress::Percentage(75));
        assert_eq!(a, b);
        assert_ne!(a, Alert::Progress(Progress::None));
    }

    #[test]
    fn alert_window_title_changed() {
        let a = Alert::WindowTitleChanged("hello".to_string());
        let b = Alert::WindowTitleChanged("hello".to_string());
        assert_eq!(a, b);
        assert_ne!(a, Alert::WindowTitleChanged("world".to_string()));
    }

    #[test]
    fn alert_icon_title_changed() {
        let a = Alert::IconTitleChanged(Some("icon".to_string()));
        let b = Alert::IconTitleChanged(None);
        assert_ne!(a, b);
    }

    #[test]
    fn alert_tab_title_changed() {
        let a = Alert::TabTitleChanged(Some("tab".to_string()));
        let b = Alert::TabTitleChanged(Some("tab".to_string()));
        assert_eq!(a, b);
    }

    #[test]
    fn terminal_size_default() {
        let size = TerminalSize::default();
        assert_eq!(size.rows, 24);
        assert_eq!(size.cols, 80);
        assert_eq!(size.pixel_width, 0);
        assert_eq!(size.pixel_height, 0);
        assert_eq!(size.dpi, 0);
    }

    #[test]
    fn terminal_size_equality() {
        let a = TerminalSize::default();
        let b = TerminalSize::default();
        assert_eq!(a, b);
    }

    #[test]
    fn terminal_size_inequality() {
        let a = TerminalSize::default();
        let b = TerminalSize {
            rows: 25,
            ..TerminalSize::default()
        };
        assert_ne!(a, b);
    }

    #[test]
    fn terminal_size_clone_and_copy() {
        let a = TerminalSize {
            rows: 40,
            cols: 120,
            pixel_width: 960,
            pixel_height: 640,
            dpi: 96,
        };
        let b = a; // Copy
        #[allow(clippy::clone_on_copy)]
        let c = a.clone(); // Clone
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn terminal_size_debug() {
        let size = TerminalSize::default();
        let dbg = format!("{:?}", size);
        assert!(dbg.contains("TerminalSize"));
        assert!(dbg.contains("24"));
        assert!(dbg.contains("80"));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        #[test]
        fn incremental_ascii_text_matches_single_shot(
            text in arb_ascii_text(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = text.into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_multibyte_text_matches_single_shot(
            text in arb_multibyte_text(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = text.into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_control_stream_matches_single_shot(
            text in arb_control_stream(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = text.into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_cursor_csi_sequences_match_single_shot(
            row in 1u8..=6,
            col in 1u8..=12,
            right in 1u8..=6,
            left in 1u8..=6,
            text in arb_ascii_text(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!(
                "home\x1b[{row};{col}H{text}\x1b[{right}C>\x1b[{left}D<"
            ).into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_sgr_palette_sequences_match_single_shot(
            fg in 30u8..=37,
            bg in 40u8..=47,
            text in arb_ascii_text(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!("\x1b[{fg};{bg};1m{text}Z\x1b[0m!").into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_sgr_truecolor_sequences_match_single_shot(
            fg_r in any::<u8>(),
            fg_g in any::<u8>(),
            fg_b in any::<u8>(),
            bg_r in any::<u8>(),
            bg_g in any::<u8>(),
            bg_b in any::<u8>(),
            text in arb_ascii_text(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!(
                "\x1b[38;2;{fg_r};{fg_g};{fg_b};48;2;{bg_r};{bg_g};{bg_b}m{text}Q\x1b[0m!"
            ).into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_osc_title_st_sequences_match_single_shot(
            title in arb_safe_label(),
            body in arb_ascii_text(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!("\x1b]0;{title}\x1b\\{body}").into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_osc_title_bel_sequences_match_single_shot(
            title in arb_safe_label(),
            body in arb_ascii_text(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!("\x1b]2;{title}\x07{body}").into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_osc_palette_change_sequences_match_single_shot(
            index in any::<u8>(),
            red in any::<u8>(),
            green in any::<u8>(),
            blue in any::<u8>(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!(
                "\x1b]4;{index};rgb:{red:02x}/{green:02x}/{blue:02x}\x1b\\X"
            ).into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_osc_palette_reset_sequences_match_single_shot(
            index in any::<u8>(),
            red in any::<u8>(),
            green in any::<u8>(),
            blue in any::<u8>(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!(
                "\x1b]4;{index};rgb:{red:02x}/{green:02x}/{blue:02x}\x1b\\\x1b]104;{index}\x1b\\X"
            ).into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_osc_dynamic_color_sequences_match_single_shot(
            fg_r in any::<u8>(),
            fg_g in any::<u8>(),
            fg_b in any::<u8>(),
            bg_r in any::<u8>(),
            bg_g in any::<u8>(),
            bg_b in any::<u8>(),
            cursor_r in any::<u8>(),
            cursor_g in any::<u8>(),
            cursor_b in any::<u8>(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!(
                "\x1b]10;rgb:{fg_r:02x}/{fg_g:02x}/{fg_b:02x}\x1b\\\
                 \x1b]11;rgb:{bg_r:02x}/{bg_g:02x}/{bg_b:02x}\x1b\\\
                 \x1b]12;rgb:{cursor_r:02x}/{cursor_g:02x}/{cursor_b:02x}\x1b\\X"
            ).into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_mixed_escape_stream_matches_single_shot(
            title in arb_safe_label(),
            body in arb_multibyte_text(),
            index in any::<u8>(),
            red in any::<u8>(),
            green in any::<u8>(),
            blue in any::<u8>(),
            row in 1u8..=6,
            col in 1u8..=12,
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!(
                "\x1b]0;{title}\x1b\\{body}\n\
                 \x1b[31;47mZ\x1b[0m\
                 \x1b]4;{index};rgb:{red:02x}/{green:02x}/{blue:02x}\x1b\\\
                 \x1b[{row};{col}H\u{4e2d}"
            ).into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn scrolling_cell_grid_invariants_hold_under_all_line_wrap_modes(
            (top, bottom) in arb_scroll_region(),
            actions in proptest::collection::vec(arb_scroll_action(), 0..32),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            for auto_wrap in [false, true] {
                let payload = scrolling_payload(auto_wrap, top, bottom, &actions);
                prop_assert_eq!(
                    single_snapshot(&payload),
                    chunked_snapshot(&payload, &chunk_sizes),
                    "chunked parse changed scrolling grid auto_wrap={} region={}..={} actions={:?}",
                    auto_wrap,
                    top,
                    bottom,
                    actions
                );

                let mut term = make_prop_term(8, 16);
                term.advance_bytes(&payload);
                assert_cell_grid_invariants(&term, 8, 16, auto_wrap, top, bottom, &actions)?;
            }
        }
    }
}
