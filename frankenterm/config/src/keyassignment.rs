use crate::default_true;
use crate::keys::KeyNoAction;
use crate::window::WindowLevel;
use frankenterm_dynamic::{FromDynamic, FromDynamicOptions, ToDynamic, Value};
use frankenterm_input_types::{KeyCode, Modifiers};
use frankenterm_term::input::MouseButton;
use frankenterm_term::SemanticType;
#[cfg(feature = "lua")]
use luahelper::impl_lua_conversion_dynamic;
use ordered_float::NotNan;
use portable_pty::CommandBuilder;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::path::PathBuf;

#[derive(Default, Debug, Clone, FromDynamic, ToDynamic, PartialEq, Eq)]
pub struct LauncherActionArgs {
    pub flags: LauncherFlags,
    pub title: Option<String>,
    pub help_text: Option<String>,
    pub fuzzy_help_text: Option<String>,
    pub alphabet: Option<String>,
}

bitflags::bitflags! {
    #[derive(Default,  FromDynamic, ToDynamic)]
    #[dynamic(try_from="String", into="String")]
    pub struct LauncherFlags :u32 {
        const ZERO = 0;
        const FUZZY = 1;
        const TABS = 2;
        const LAUNCH_MENU_ITEMS = 4;
        const DOMAINS = 8;
        const KEY_ASSIGNMENTS = 16;
        const WORKSPACES = 32;
        const COMMANDS = 64;
    }
}

impl From<LauncherFlags> for String {
    fn from(val: LauncherFlags) -> Self {
        val.to_string()
    }
}

impl From<&LauncherFlags> for String {
    fn from(val: &LauncherFlags) -> Self {
        val.to_string()
    }
}

impl ToString for LauncherFlags {
    fn to_string(&self) -> String {
        let mut s = vec![];
        if self.contains(Self::FUZZY) {
            s.push("FUZZY");
        }
        if self.contains(Self::TABS) {
            s.push("TABS");
        }
        if self.contains(Self::LAUNCH_MENU_ITEMS) {
            s.push("LAUNCH_MENU_ITEMS");
        }
        if self.contains(Self::DOMAINS) {
            s.push("DOMAINS");
        }
        if self.contains(Self::KEY_ASSIGNMENTS) {
            s.push("KEY_ASSIGNMENTS");
        }
        if self.contains(Self::WORKSPACES) {
            s.push("WORKSPACES");
        }
        if self.contains(Self::COMMANDS) {
            s.push("COMMANDS");
        }
        s.join("|")
    }
}

impl TryFrom<String> for LauncherFlags {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        let mut flags = LauncherFlags::default();

        for ele in s.split('|') {
            let ele = ele.trim();
            match ele {
                "FUZZY" => flags |= Self::FUZZY,
                "TABS" => flags |= Self::TABS,
                "LAUNCH_MENU_ITEMS" => flags |= Self::LAUNCH_MENU_ITEMS,
                "DOMAINS" => flags |= Self::DOMAINS,
                "KEY_ASSIGNMENTS" => flags |= Self::KEY_ASSIGNMENTS,
                "WORKSPACES" => flags |= Self::WORKSPACES,
                "COMMANDS" => flags |= Self::COMMANDS,
                _ => {
                    return Err(format!("invalid LauncherFlags `{}` in `{}`", ele, s));
                }
            }
        }

        Ok(flags)
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, FromDynamic, ToDynamic)]
pub enum SelectionMode {
    Cell,
    Word,
    Line,
    SemanticZone,
    Block,
}

#[derive(Debug, Clone, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum Pattern {
    CaseSensitiveString(String),
    CaseInSensitiveString(String),
    Regex(String),
    CurrentSelectionOrEmptyString,
}

impl Pattern {
    pub fn is_empty(&self) -> bool {
        match self {
            Self::CaseSensitiveString(s) | Self::CaseInSensitiveString(s) | Self::Regex(s) => {
                s.is_empty()
            }
            Self::CurrentSelectionOrEmptyString => true,
        }
    }
}

impl Default for Pattern {
    fn default() -> Self {
        Self::CurrentSelectionOrEmptyString
    }
}

/// A mouse event that can trigger an action
#[derive(Debug, Clone, PartialEq, Eq, Ord, PartialOrd, Hash, FromDynamic, ToDynamic)]
pub enum MouseEventTrigger {
    /// Mouse button is pressed. streak is how many times in a row
    /// it was pressed.
    Down { streak: usize, button: MouseButton },
    /// Mouse button is held down while the cursor is moving. streak is how many times in a row
    /// it was pressed, with the last of those being held to form the drag.
    Drag { streak: usize, button: MouseButton },
    /// Mouse button is being released. streak is how many times
    /// in a row it was pressed and released.
    Up { streak: usize, button: MouseButton },
}

/// When spawning a tab, specify which domain should be used to
/// host/spawn that tab.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum SpawnTabDomain {
    /// Use the default domain
    DefaultDomain,
    /// Use the domain from the current tab in the associated window
    CurrentPaneDomain,
    /// Use a specific domain by name
    DomainName(String),
    /// Use a specific domain by id
    DomainId(usize),
}

impl Default for SpawnTabDomain {
    fn default() -> Self {
        Self::CurrentPaneDomain
    }
}

#[derive(Default, Clone, PartialEq, FromDynamic, ToDynamic)]
pub struct SpawnCommand {
    /// Optional descriptive label
    pub label: Option<String>,

    /// The command line to use.
    /// If omitted, the default command associated with the
    /// domain will be used instead, which is typically the
    /// shell for the user.
    pub args: Option<Vec<String>>,

    /// Specifies the current working directory for the command.
    /// If omitted, a default will be used; typically that will
    /// be the home directory of the user, but may also be the
    /// current working directory of the wezterm process when
    /// it was launched, or for some domains it may be some
    /// other location appropriate to the domain.
    pub cwd: Option<PathBuf>,

    /// Specifies a map of environment variables that should be set.
    /// Whether this is used depends on the domain.
    #[dynamic(default)]
    pub set_environment_variables: HashMap<String, String>,

    #[dynamic(default)]
    pub domain: SpawnTabDomain,

    pub position: Option<crate::GuiPosition>,
}
#[cfg(feature = "lua")]
impl_lua_conversion_dynamic!(SpawnCommand);

impl std::fmt::Debug for SpawnCommand {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(fmt, "{}", self)
    }
}

impl std::fmt::Display for SpawnCommand {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(fmt, "SpawnCommand")?;
        if let Some(label) = &self.label {
            write!(fmt, " label='{}'", label)?;
        }
        write!(fmt, " domain={:?}", self.domain)?;
        if let Some(args) = &self.args {
            write!(fmt, " args={:?}", args)?;
        }
        if let Some(cwd) = &self.cwd {
            write!(fmt, " cwd={}", cwd.display())?;
        }
        for (k, v) in &self.set_environment_variables {
            write!(fmt, " {}={}", k, v)?;
        }
        Ok(())
    }
}

impl SpawnCommand {
    pub fn label_for_palette(&self) -> Option<String> {
        if let Some(label) = &self.label {
            Some(label.to_string())
        } else if let Some(args) = &self.args {
            Some(shlex::try_join(args.iter().map(|s| s.as_str())).ok()?)
        } else {
            None
        }
    }

    pub fn from_command_builder(cmd: &CommandBuilder) -> anyhow::Result<Self> {
        let mut args = vec![];
        let mut set_environment_variables = HashMap::new();
        for arg in cmd.get_argv() {
            args.push(
                arg.to_str()
                    .ok_or_else(|| anyhow::anyhow!("command argument is not utf8"))?
                    .to_string(),
            );
        }
        for (k, v) in cmd.iter_full_env_as_str() {
            set_environment_variables.insert(k.to_string(), v.to_string());
        }
        let cwd = match cmd.get_cwd() {
            Some(cwd) => Some(PathBuf::from(cwd)),
            None => None,
        };
        Ok(Self {
            label: None,
            domain: SpawnTabDomain::DefaultDomain,
            args: if args.is_empty() { None } else { Some(args) },
            set_environment_variables,
            cwd,
            position: None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, FromDynamic, ToDynamic)]
pub enum PaneDirection {
    Up,
    Down,
    Left,
    Right,
    Next,
    Prev,
}

impl PaneDirection {
    pub fn direction_from_str(arg: &str) -> Result<PaneDirection, String> {
        for candidate in PaneDirection::variants() {
            if candidate.to_lowercase() == arg.to_lowercase() {
                if let Ok(direction) = PaneDirection::from_dynamic(
                    &Value::String(candidate.to_string()),
                    FromDynamicOptions::default(),
                ) {
                    return Ok(direction);
                }
            }
        }
        Err(format!(
            "invalid direction {arg}, possible values are {:?}",
            PaneDirection::variants()
        ))
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, FromDynamic, ToDynamic, Serialize, Deserialize)]
pub enum ScrollbackEraseMode {
    ScrollbackOnly,
    ScrollbackAndViewport,
}

impl Default for ScrollbackEraseMode {
    fn default() -> Self {
        Self::ScrollbackOnly
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum ClipboardCopyDestination {
    Clipboard,
    PrimarySelection,
    ClipboardAndPrimarySelection,
}
#[cfg(feature = "lua")]
impl_lua_conversion_dynamic!(ClipboardCopyDestination);

impl Default for ClipboardCopyDestination {
    fn default() -> Self {
        Self::ClipboardAndPrimarySelection
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum ClipboardPasteSource {
    Clipboard,
    PrimarySelection,
}

impl Default for ClipboardPasteSource {
    fn default() -> Self {
        Self::Clipboard
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum PaneSelectMode {
    Activate,
    SwapWithActive,
    SwapWithActiveKeepFocus,
    MoveToNewTab,
    MoveToNewWindow,
}

impl Default for PaneSelectMode {
    fn default() -> Self {
        Self::Activate
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq, FromDynamic, ToDynamic)]
pub struct PaneSelectArguments {
    /// Overrides the main quick_select_alphabet config
    #[dynamic(default)]
    pub alphabet: String,

    #[dynamic(default)]
    pub mode: PaneSelectMode,

    #[dynamic(default)]
    pub show_pane_ids: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum CharSelectGroup {
    RecentlyUsed,
    SmileysAndEmotion,
    PeopleAndBody,
    AnimalsAndNature,
    FoodAndDrink,
    TravelAndPlaces,
    Activities,
    Objects,
    Symbols,
    Flags,
    NerdFonts,
    UnicodeNames,
    ShortCodes,
}

// next is default, previous is the reverse
macro_rules! char_select_group_impl_next_prev {
    ($($x:ident => $y:ident),+ $(,)?) => {
        impl CharSelectGroup {
            pub const fn next(self) -> Self {
                match self {
                    $(CharSelectGroup::$x => CharSelectGroup::$y),+
                }
            }

            pub const fn previous(self) -> Self {
                match self {
                    $(CharSelectGroup::$y => CharSelectGroup::$x),+
                }
            }
        }
    };
}

char_select_group_impl_next_prev! (
    RecentlyUsed => SmileysAndEmotion,
    SmileysAndEmotion => PeopleAndBody,
    PeopleAndBody => AnimalsAndNature,
    AnimalsAndNature => FoodAndDrink,
    FoodAndDrink => TravelAndPlaces,
    TravelAndPlaces => Activities,
    Activities => Objects,
    Objects => Symbols,
    Symbols => Flags,
    Flags => NerdFonts,
    NerdFonts => UnicodeNames,
    UnicodeNames => ShortCodes,
    ShortCodes => RecentlyUsed,
);

impl Default for CharSelectGroup {
    fn default() -> Self {
        Self::SmileysAndEmotion
    }
}

#[derive(Debug, Clone, PartialEq, Eq, FromDynamic, ToDynamic)]
pub struct CharSelectArguments {
    #[dynamic(default)]
    pub group: Option<CharSelectGroup>,
    #[dynamic(default = "default_true")]
    pub copy_on_select: bool,
    #[dynamic(default)]
    pub copy_to: ClipboardCopyDestination,
}

impl Default for CharSelectArguments {
    fn default() -> Self {
        Self {
            group: None,
            copy_on_select: true,
            copy_to: ClipboardCopyDestination::default(),
        }
    }
}

#[derive(Default, Debug, Clone, PartialEq, FromDynamic, ToDynamic)]
pub struct QuickSelectArguments {
    /// Overrides the main quick_select_alphabet config
    #[dynamic(default)]
    pub alphabet: String,
    /// Overrides the main quick_select_patterns config
    #[dynamic(default)]
    pub patterns: Vec<String>,
    #[dynamic(default)]
    pub action: Option<Box<KeyAssignment>>,
    /// Skip triggering `action` after paste is performed (capital selection)
    #[dynamic(default)]
    pub skip_action_on_paste: bool,
    /// Label to use in place of "copy" when `action` is set
    #[dynamic(default)]
    pub label: String,
    /// How many lines before and how many lines after the viewport to
    /// search to produce the quickselect results
    pub scope_lines: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, FromDynamic, ToDynamic)]
pub struct PromptInputLine {
    pub action: Box<KeyAssignment>,
    /// Optional label to pre-fill the input line with
    #[dynamic(default)]
    pub initial_value: Option<String>,
    /// Descriptive text to show ahead of prompt
    #[dynamic(default)]
    pub description: String,
    /// Text to show for prompt
    #[dynamic(default = "default_prompt")]
    pub prompt: String,
}

fn default_prompt() -> String {
    "> ".to_string()
}

#[derive(Debug, Clone, PartialEq, FromDynamic, ToDynamic)]
pub struct InputSelectorEntry {
    pub label: String,
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, FromDynamic, ToDynamic)]
pub struct InputSelector {
    pub action: Box<KeyAssignment>,
    #[dynamic(default)]
    pub title: String,

    pub choices: Vec<InputSelectorEntry>,

    #[dynamic(default)]
    pub fuzzy: bool,

    #[dynamic(default = "default_num_alphabet")]
    pub alphabet: String,

    #[dynamic(default = "default_description")]
    pub description: String,

    #[dynamic(default = "default_fuzzy_description")]
    pub fuzzy_description: String,
}

fn default_num_alphabet() -> String {
    "1234567890abcdefghilmnopqrstuvwxyz".to_string()
}

fn default_description() -> String {
    "Select an item and press Enter = accept,  Esc = cancel,  / = filter".to_string()
}

fn default_fuzzy_description() -> String {
    "Fuzzy matching: ".to_string()
}

#[derive(Debug, Clone, PartialEq, FromDynamic, ToDynamic)]
pub struct Confirmation {
    pub action: Box<KeyAssignment>,
    #[dynamic(default)]
    pub cancel: Option<Box<KeyAssignment>>,
    /// Text to show for confirmation
    #[dynamic(default = "default_message")]
    pub message: String,
}

fn default_message() -> String {
    "🛑 Really continue?".to_string()
}

#[derive(Debug, Clone, PartialEq, FromDynamic, ToDynamic)]
pub enum KeyAssignment {
    SpawnTab(SpawnTabDomain),
    SpawnWindow,
    ToggleFullScreen,
    ToggleAlwaysOnTop,
    ToggleAlwaysOnBottom,
    SetWindowLevel(WindowLevel),
    CopyTo(ClipboardCopyDestination),
    CopyTextTo {
        text: String,
        destination: ClipboardCopyDestination,
    },
    PasteFrom(ClipboardPasteSource),
    ActivateTabRelative(isize),
    ActivateTabRelativeNoWrap(isize),
    IncreaseFontSize,
    DecreaseFontSize,
    ResetFontSize,
    ResetFontAndWindowSize,
    ActivateTab(isize),
    ActivateLastTab,
    SendString(String),
    SendKey(KeyNoAction),
    Nop,
    DisableDefaultAssignment,
    Hide,
    Show,
    CloseCurrentTab {
        confirm: bool,
    },
    ReloadConfiguration,
    MoveTabRelative(isize),
    MoveTab(usize),
    ScrollByPage(NotNan<f64>),
    ScrollByLine(isize),
    ScrollByCurrentEventWheelDelta,
    ScrollToPrompt(isize),
    ScrollToTop,
    ScrollToBottom,
    ShowTabNavigator,
    ShowDebugOverlay,
    HideApplication,
    QuitApplication,
    SpawnCommandInNewTab(SpawnCommand),
    SpawnCommandInNewWindow(SpawnCommand),
    SplitHorizontal(SpawnCommand),
    SplitVertical(SpawnCommand),
    ShowLauncher,
    ShowLauncherArgs(LauncherActionArgs),
    ClearScrollback(ScrollbackEraseMode),
    Search(Pattern),
    ActivateCopyMode,

    SelectTextAtMouseCursor(SelectionMode),
    ExtendSelectionToMouseCursor(SelectionMode),
    OpenLinkAtMouseCursor,
    ClearSelection,
    CompleteSelection(ClipboardCopyDestination),
    CompleteSelectionOrOpenLinkAtMouseCursor(ClipboardCopyDestination),
    StartWindowDrag,

    AdjustPaneSize(PaneDirection, usize),
    ActivatePaneDirection(PaneDirection),
    ActivatePaneByIndex(usize),
    TogglePaneZoomState,
    SetPaneZoomState(bool),
    CloseCurrentPane {
        confirm: bool,
    },
    EmitEvent(String),
    QuickSelect,
    QuickSelectArgs(QuickSelectArguments),

    Multiple(Vec<KeyAssignment>),

    SwitchToWorkspace {
        name: Option<String>,
        spawn: Option<SpawnCommand>,
    },
    SwitchWorkspaceRelative(isize),

    ActivateKeyTable {
        name: String,
        #[dynamic(default)]
        timeout_milliseconds: Option<u64>,
        #[dynamic(default)]
        replace_current: bool,
        #[dynamic(default = "crate::default_true")]
        one_shot: bool,
        #[dynamic(default)]
        until_unknown: bool,
        #[dynamic(default)]
        prevent_fallback: bool,
    },
    PopKeyTable,
    ClearKeyTableStack,
    DetachDomain(SpawnTabDomain),
    AttachDomain(String),

    CopyMode(CopyModeAssignment),
    RotatePanes(RotationDirection),
    SplitPane(SplitPane),
    PaneSelect(PaneSelectArguments),
    CharSelect(CharSelectArguments),

    ResetTerminal,
    OpenUri(String),
    ActivateCommandPalette,
    ActivateWindow(usize),
    ActivateWindowRelative(isize),
    ActivateWindowRelativeNoWrap(isize),
    PromptInputLine(PromptInputLine),
    InputSelector(InputSelector),
    Confirmation(Confirmation),
}
#[cfg(feature = "lua")]
impl_lua_conversion_dynamic!(KeyAssignment);

#[derive(Debug, Clone, PartialEq, FromDynamic, ToDynamic)]
pub struct SplitPane {
    pub direction: PaneDirection,
    #[dynamic(default)]
    pub size: SplitSize,
    #[dynamic(default)]
    pub command: SpawnCommand,
    #[dynamic(default)]
    pub top_level: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum SplitSize {
    Cells(usize),
    Percent(u8),
}

impl Default for SplitSize {
    fn default() -> Self {
        Self::Percent(50)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum RotationDirection {
    Clockwise,
    CounterClockwise,
}

#[derive(Debug, Clone, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum CopyModeAssignment {
    MoveToViewportBottom,
    MoveToViewportTop,
    MoveToViewportMiddle,
    MoveToScrollbackTop,
    MoveToScrollbackBottom,
    SetSelectionMode(Option<SelectionMode>),
    ClearSelectionMode,
    MoveToStartOfLineContent,
    MoveToEndOfLineContent,
    MoveToStartOfLine,
    MoveToStartOfNextLine,
    MoveToSelectionOtherEnd,
    MoveToSelectionOtherEndHoriz,
    MoveBackwardWord,
    MoveForwardWord,
    MoveForwardWordEnd,
    MoveRight,
    MoveLeft,
    MoveUp,
    MoveDown,
    MoveByPage(NotNan<f64>),
    PageUp,
    PageDown,
    Close,
    PriorMatch,
    NextMatch,
    PriorMatchPage,
    NextMatchPage,
    CycleMatchType,
    ClearPattern,
    EditPattern,
    AcceptPattern,
    MoveBackwardSemanticZone,
    MoveForwardSemanticZone,
    MoveBackwardZoneOfType(SemanticType),
    MoveForwardZoneOfType(SemanticType),
    JumpForward { prev_char: bool },
    JumpBackward { prev_char: bool },
    JumpAgain,
    JumpReverse,
}

pub type KeyTable = HashMap<(KeyCode, Modifiers), KeyTableEntry>;

#[derive(Debug, Clone, Default)]
pub struct KeyTables {
    pub default: KeyTable,
    pub by_name: HashMap<String, KeyTable>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KeyTableEntry {
    pub action: KeyAssignment,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::TryFrom;

    // --- LauncherFlags ---

    #[test]
    fn launcher_flags_default_is_zero() {
        let flags = LauncherFlags::default();
        assert_eq!(flags, LauncherFlags::ZERO);
    }

    #[test]
    fn launcher_flags_to_string_empty() {
        let flags = LauncherFlags::ZERO;
        assert_eq!(flags.to_string(), "");
    }

    #[test]
    fn launcher_flags_to_string_single() {
        assert_eq!(LauncherFlags::FUZZY.to_string(), "FUZZY");
        assert_eq!(LauncherFlags::TABS.to_string(), "TABS");
        assert_eq!(LauncherFlags::DOMAINS.to_string(), "DOMAINS");
        assert_eq!(LauncherFlags::COMMANDS.to_string(), "COMMANDS");
    }

    #[test]
    fn launcher_flags_to_string_combined() {
        let flags = LauncherFlags::FUZZY | LauncherFlags::TABS;
        assert_eq!(flags.to_string(), "FUZZY|TABS");
    }

    #[test]
    fn launcher_flags_try_from_string_single() {
        let flags = LauncherFlags::try_from("FUZZY".to_string()).unwrap();
        assert_eq!(flags, LauncherFlags::FUZZY);
    }

    #[test]
    fn launcher_flags_try_from_string_combined() {
        let flags = LauncherFlags::try_from("FUZZY|TABS|DOMAINS".to_string()).unwrap();
        assert!(flags.contains(LauncherFlags::FUZZY));
        assert!(flags.contains(LauncherFlags::TABS));
        assert!(flags.contains(LauncherFlags::DOMAINS));
    }

    #[test]
    fn launcher_flags_try_from_invalid() {
        let result = LauncherFlags::try_from("INVALID".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn launcher_flags_roundtrip() {
        let flags = LauncherFlags::WORKSPACES | LauncherFlags::KEY_ASSIGNMENTS;
        let s = flags.to_string();
        let parsed = LauncherFlags::try_from(s).unwrap();
        assert_eq!(flags, parsed);
    }

    // --- LauncherActionArgs ---

    #[test]
    fn launcher_action_args_default() {
        let args = LauncherActionArgs::default();
        assert_eq!(args.flags, LauncherFlags::ZERO);
        assert!(args.title.is_none());
        assert!(args.help_text.is_none());
        assert!(args.fuzzy_help_text.is_none());
        assert!(args.alphabet.is_none());
    }

    // --- SelectionMode ---

    #[test]
    fn selection_mode_equality() {
        assert_eq!(SelectionMode::Cell, SelectionMode::Cell);
        assert_ne!(SelectionMode::Cell, SelectionMode::Word);
        assert_ne!(SelectionMode::Word, SelectionMode::Line);
        assert_ne!(SelectionMode::Line, SelectionMode::SemanticZone);
        assert_ne!(SelectionMode::SemanticZone, SelectionMode::Block);
    }

    #[test]
    fn selection_mode_clone_copy() {
        let a = SelectionMode::Word;
        let b = a;
        let c = a.clone();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn selection_mode_debug() {
        let dbg = format!("{:?}", SelectionMode::SemanticZone);
        assert!(dbg.contains("SemanticZone"));
    }

    // --- Pattern ---

    #[test]
    fn pattern_default() {
        assert_eq!(Pattern::default(), Pattern::CurrentSelectionOrEmptyString);
    }

    #[test]
    fn pattern_is_empty_current_selection() {
        assert!(Pattern::CurrentSelectionOrEmptyString.is_empty());
    }

    #[test]
    fn pattern_is_empty_empty_string() {
        assert!(Pattern::CaseSensitiveString(String::new()).is_empty());
        assert!(Pattern::CaseInSensitiveString(String::new()).is_empty());
        assert!(Pattern::Regex(String::new()).is_empty());
    }

    #[test]
    fn pattern_is_empty_non_empty() {
        assert!(!Pattern::CaseSensitiveString("foo".to_string()).is_empty());
        assert!(!Pattern::CaseInSensitiveString("bar".to_string()).is_empty());
        assert!(!Pattern::Regex(".*".to_string()).is_empty());
    }

    #[test]
    fn pattern_equality() {
        assert_eq!(
            Pattern::CaseSensitiveString("a".to_string()),
            Pattern::CaseSensitiveString("a".to_string())
        );
        assert_ne!(
            Pattern::CaseSensitiveString("a".to_string()),
            Pattern::CaseInSensitiveString("a".to_string())
        );
    }

    // --- SpawnTabDomain ---

    #[test]
    fn spawn_tab_domain_default() {
        assert_eq!(SpawnTabDomain::default(), SpawnTabDomain::CurrentPaneDomain);
    }

    #[test]
    fn spawn_tab_domain_equality() {
        assert_eq!(SpawnTabDomain::DefaultDomain, SpawnTabDomain::DefaultDomain);
        assert_ne!(
            SpawnTabDomain::DefaultDomain,
            SpawnTabDomain::CurrentPaneDomain
        );
        assert_eq!(
            SpawnTabDomain::DomainName("foo".to_string()),
            SpawnTabDomain::DomainName("foo".to_string())
        );
        assert_ne!(
            SpawnTabDomain::DomainName("foo".to_string()),
            SpawnTabDomain::DomainName("bar".to_string())
        );
        assert_eq!(SpawnTabDomain::DomainId(1), SpawnTabDomain::DomainId(1));
        assert_ne!(SpawnTabDomain::DomainId(1), SpawnTabDomain::DomainId(2));
    }

    // --- PaneDirection ---

    #[test]
    fn pane_direction_equality() {
        assert_eq!(PaneDirection::Up, PaneDirection::Up);
        assert_ne!(PaneDirection::Up, PaneDirection::Down);
        assert_ne!(PaneDirection::Left, PaneDirection::Right);
        assert_ne!(PaneDirection::Next, PaneDirection::Prev);
    }

    #[test]
    fn pane_direction_clone_copy() {
        let a = PaneDirection::Left;
        let b = a;
        let c = a.clone();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn pane_direction_from_str() {
        assert_eq!(
            PaneDirection::direction_from_str("Up").unwrap(),
            PaneDirection::Up
        );
        assert_eq!(
            PaneDirection::direction_from_str("down").unwrap(),
            PaneDirection::Down
        );
        assert_eq!(
            PaneDirection::direction_from_str("LEFT").unwrap(),
            PaneDirection::Left
        );
    }

    #[test]
    fn pane_direction_from_str_invalid() {
        assert!(PaneDirection::direction_from_str("diagonal").is_err());
    }

    // --- ScrollbackEraseMode ---

    #[test]
    fn scrollback_erase_mode_default() {
        assert_eq!(
            ScrollbackEraseMode::default(),
            ScrollbackEraseMode::ScrollbackOnly
        );
    }

    #[test]
    fn scrollback_erase_mode_equality() {
        assert_eq!(
            ScrollbackEraseMode::ScrollbackOnly,
            ScrollbackEraseMode::ScrollbackOnly
        );
        assert_ne!(
            ScrollbackEraseMode::ScrollbackOnly,
            ScrollbackEraseMode::ScrollbackAndViewport
        );
    }

    // --- ClipboardCopyDestination ---

    #[test]
    fn clipboard_copy_destination_default() {
        assert_eq!(
            ClipboardCopyDestination::default(),
            ClipboardCopyDestination::ClipboardAndPrimarySelection
        );
    }

    #[test]
    fn clipboard_copy_destination_equality() {
        assert_eq!(
            ClipboardCopyDestination::Clipboard,
            ClipboardCopyDestination::Clipboard
        );
        assert_ne!(
            ClipboardCopyDestination::Clipboard,
            ClipboardCopyDestination::PrimarySelection
        );
    }

    // --- ClipboardPasteSource ---

    #[test]
    fn clipboard_paste_source_default() {
        assert_eq!(
            ClipboardPasteSource::default(),
            ClipboardPasteSource::Clipboard
        );
    }

    #[test]
    fn clipboard_paste_source_equality() {
        assert_eq!(
            ClipboardPasteSource::Clipboard,
            ClipboardPasteSource::Clipboard
        );
        assert_ne!(
            ClipboardPasteSource::Clipboard,
            ClipboardPasteSource::PrimarySelection
        );
    }

    // --- PaneSelectMode ---

    #[test]
    fn pane_select_mode_default() {
        assert_eq!(PaneSelectMode::default(), PaneSelectMode::Activate);
    }

    #[test]
    fn pane_select_mode_equality() {
        assert_eq!(PaneSelectMode::Activate, PaneSelectMode::Activate);
        assert_ne!(PaneSelectMode::Activate, PaneSelectMode::SwapWithActive);
        assert_ne!(
            PaneSelectMode::SwapWithActive,
            PaneSelectMode::SwapWithActiveKeepFocus
        );
        assert_ne!(
            PaneSelectMode::MoveToNewTab,
            PaneSelectMode::MoveToNewWindow
        );
    }

    // --- PaneSelectArguments ---

    #[test]
    fn pane_select_arguments_default() {
        let args = PaneSelectArguments::default();
        assert_eq!(args.alphabet, "");
        assert_eq!(args.mode, PaneSelectMode::Activate);
        assert!(!args.show_pane_ids);
    }

    // --- CharSelectGroup ---

    #[test]
    fn char_select_group_default() {
        assert_eq!(
            CharSelectGroup::default(),
            CharSelectGroup::SmileysAndEmotion
        );
    }

    #[test]
    fn char_select_group_next_wraps() {
        let g = CharSelectGroup::ShortCodes;
        assert_eq!(g.next(), CharSelectGroup::RecentlyUsed);
    }

    #[test]
    fn char_select_group_previous_wraps() {
        let g = CharSelectGroup::RecentlyUsed;
        assert_eq!(g.previous(), CharSelectGroup::ShortCodes);
    }

    #[test]
    fn char_select_group_next_chain() {
        let start = CharSelectGroup::RecentlyUsed;
        assert_eq!(start.next(), CharSelectGroup::SmileysAndEmotion);
        assert_eq!(start.next().next(), CharSelectGroup::PeopleAndBody);
        assert_eq!(
            start.next().next().next(),
            CharSelectGroup::AnimalsAndNature
        );
    }

    #[test]
    fn char_select_group_next_previous_inverse() {
        let groups = [
            CharSelectGroup::RecentlyUsed,
            CharSelectGroup::SmileysAndEmotion,
            CharSelectGroup::PeopleAndBody,
            CharSelectGroup::AnimalsAndNature,
            CharSelectGroup::FoodAndDrink,
            CharSelectGroup::TravelAndPlaces,
            CharSelectGroup::Activities,
            CharSelectGroup::Objects,
            CharSelectGroup::Symbols,
            CharSelectGroup::Flags,
            CharSelectGroup::NerdFonts,
            CharSelectGroup::UnicodeNames,
            CharSelectGroup::ShortCodes,
        ];
        for g in &groups {
            assert_eq!(g.next().previous(), *g);
            assert_eq!(g.previous().next(), *g);
        }
    }

    #[test]
    fn char_select_group_full_cycle() {
        let start = CharSelectGroup::RecentlyUsed;
        let mut current = start;
        for _ in 0..13 {
            current = current.next();
        }
        assert_eq!(current, start);
    }

    // --- CharSelectArguments ---

    #[test]
    fn char_select_arguments_default() {
        let args = CharSelectArguments::default();
        assert!(args.group.is_none());
        assert!(args.copy_on_select);
        assert_eq!(
            args.copy_to,
            ClipboardCopyDestination::ClipboardAndPrimarySelection
        );
    }

    // --- SplitSize ---

    #[test]
    fn split_size_default() {
        assert_eq!(SplitSize::default(), SplitSize::Percent(50));
    }

    #[test]
    fn split_size_equality() {
        assert_eq!(SplitSize::Cells(10), SplitSize::Cells(10));
        assert_ne!(SplitSize::Cells(10), SplitSize::Cells(20));
        assert_eq!(SplitSize::Percent(50), SplitSize::Percent(50));
        assert_ne!(SplitSize::Cells(50), SplitSize::Percent(50));
    }

    // --- RotationDirection ---

    #[test]
    fn rotation_direction_equality() {
        assert_eq!(RotationDirection::Clockwise, RotationDirection::Clockwise);
        assert_ne!(
            RotationDirection::Clockwise,
            RotationDirection::CounterClockwise
        );
    }

    // --- SpawnCommand ---

    #[test]
    fn spawn_command_default() {
        let cmd = SpawnCommand::default();
        assert!(cmd.label.is_none());
        assert!(cmd.args.is_none());
        assert!(cmd.cwd.is_none());
        assert!(cmd.set_environment_variables.is_empty());
        assert_eq!(cmd.domain, SpawnTabDomain::CurrentPaneDomain);
        assert!(cmd.position.is_none());
    }

    #[test]
    fn spawn_command_display_minimal() {
        let cmd = SpawnCommand::default();
        let s = format!("{}", cmd);
        assert!(s.contains("SpawnCommand"));
        assert!(s.contains("CurrentPaneDomain"));
    }

    #[test]
    fn spawn_command_display_with_label() {
        let cmd = SpawnCommand {
            label: Some("my shell".to_string()),
            ..SpawnCommand::default()
        };
        let s = format!("{}", cmd);
        assert!(s.contains("my shell"));
    }

    #[test]
    fn spawn_command_label_for_palette_from_label() {
        let cmd = SpawnCommand {
            label: Some("my label".to_string()),
            ..SpawnCommand::default()
        };
        assert_eq!(cmd.label_for_palette(), Some("my label".to_string()));
    }

    #[test]
    fn spawn_command_label_for_palette_from_args() {
        let cmd = SpawnCommand {
            args: Some(vec![
                "bash".to_string(),
                "-c".to_string(),
                "echo hi".to_string(),
            ]),
            ..SpawnCommand::default()
        };
        let label = cmd.label_for_palette().unwrap();
        assert!(label.contains("bash"));
    }

    #[test]
    fn spawn_command_label_for_palette_none() {
        let cmd = SpawnCommand::default();
        assert!(cmd.label_for_palette().is_none());
    }

    // --- InputSelectorEntry ---

    #[test]
    fn input_selector_entry_basic() {
        let entry = InputSelectorEntry {
            label: "test".to_string(),
            id: Some("id1".to_string()),
        };
        assert_eq!(entry.label, "test");
        assert_eq!(entry.id, Some("id1".to_string()));
    }

    // --- KeyTableEntry ---

    #[test]
    fn key_table_entry_clone() {
        let entry = KeyTableEntry {
            action: KeyAssignment::Nop,
        };
        let cloned = entry.clone();
        assert_eq!(entry, cloned);
    }

    #[test]
    fn key_assignment_nop_equality() {
        assert_eq!(KeyAssignment::Nop, KeyAssignment::Nop);
        assert_ne!(KeyAssignment::Nop, KeyAssignment::SpawnWindow);
    }

    #[test]
    fn key_assignment_send_string() {
        let a = KeyAssignment::SendString("hello".to_string());
        let b = KeyAssignment::SendString("hello".to_string());
        let c = KeyAssignment::SendString("world".to_string());
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // --- CopyModeAssignment ---

    #[test]
    fn copy_mode_assignment_equality() {
        assert_eq!(CopyModeAssignment::Close, CopyModeAssignment::Close);
        assert_ne!(CopyModeAssignment::Close, CopyModeAssignment::PageUp);
        assert_ne!(CopyModeAssignment::MoveLeft, CopyModeAssignment::MoveRight);
    }

    // --- MouseEventTrigger ---

    #[test]
    fn mouse_event_trigger_debug() {
        let trigger = MouseEventTrigger::Down {
            streak: 1,
            button: MouseButton::Left,
        };
        let dbg = format!("{:?}", trigger);
        assert!(dbg.contains("Down"));
        assert!(dbg.contains("Left"));
    }
}
