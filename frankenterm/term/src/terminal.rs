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
        match &alert {
            Alert::ToastNotification { title, body, focus } => {
                assert!(title.is_none());
                assert_eq!(body, "message");
                assert!(!focus);
            }
            _ => panic!("expected ToastNotification"),
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
}
