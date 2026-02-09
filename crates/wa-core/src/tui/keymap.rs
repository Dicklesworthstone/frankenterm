//! Canonical keybinding table and input dispatcher for wa TUI.
//!
//! All keybindings are defined declaratively in [`KEYMAP`] as
//! `(KeyPattern, Scope, Action)` tuples.  The [`resolve`] function matches
//! an incoming [`KeyInput`](super::ftui_compat::KeyInput) against the table
//! for a given view, with global bindings checked first (deterministic
//! conflict policy: **global wins over view-specific**).
//!
//! # Parity guarantee
//!
//! This module is the single source of truth for keybinding behavior.  Both
//! the ratatui (`app.rs`) and ftui (`ftui_stub.rs`) backends should dispatch
//! through [`resolve`] — or, during the migration period, maintain byte-level
//! parity with the canonical table (verified by the `parity_` tests below).
//!
//! # Deletion criterion
//! After the `tui` feature is dropped (FTUI-09.3), the legacy parity tests
//! can be removed, but the keymap itself and the resolver remain.

use super::ftui_compat::{Key, KeyInput};

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

/// Scope in which a keybinding is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Active in every view (checked first).
    Global,
    /// Active only in the named view.
    Panes,
    Events,
    Triage,
    History,
    Search,
    Timeline,
}

// ---------------------------------------------------------------------------
// Action
// ---------------------------------------------------------------------------

/// Canonical action produced by a keybinding.
///
/// Actions are framework-agnostic intents.  The event loop (legacy or ftui)
/// translates them into the appropriate state mutation or command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    // -- global --
    Quit,
    ShowHelp,
    Refresh,
    NextTab,
    PrevTab,
    GoToView(u8), // 1-7

    // -- list navigation (shared pattern across Panes/Events/Triage/History/Search) --
    ListNext,
    ListPrev,

    // -- filter / text input --
    FilterAppendChar(char),
    FilterDeleteChar,
    FilterClear,

    // -- panes --
    ToggleUnhandledOnly,
    ToggleBookmarkedOnly,
    CycleAgentFilter,
    CycleDomainFilter,
    CycleRulesetProfile,
    ApplyRulesetProfile,

    // -- events --
    EventsFilterDigit(char),

    // -- triage --
    TriagePrimaryAction,
    TriageMute,
    TriageToggleExpand,
    TriageNumberedAction(u8), // 1-9

    // -- history --
    ToggleUndoableOnly,

    // -- search --
    SearchNextSaved,
    SearchPrevSaved,
    SearchRunSaved,
    SearchToggleSaved,
    SearchExecute,

    // -- timeline --
    TimelineZoomIn,
    TimelineZoomOut,
    TimelineScrollLeft,
    TimelineScrollRight,
}

// ---------------------------------------------------------------------------
// KeyPattern — declarative key matcher
// ---------------------------------------------------------------------------

/// A pattern that matches a key input.
///
/// Created via helper functions below for readability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyPattern {
    pub key: Key,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl KeyPattern {
    const fn matches(&self, input: &KeyInput) -> bool {
        if self.ctrl != input.ctrl || self.alt != input.alt || self.shift != input.shift {
            return false;
        }
        match (&self.key, &input.key) {
            (Key::Char(a), Key::Char(b)) => *a == *b,
            (Key::Enter, Key::Enter)
            | (Key::Esc, Key::Esc)
            | (Key::Tab, Key::Tab)
            | (Key::BackTab, Key::BackTab)
            | (Key::Backspace, Key::Backspace)
            | (Key::Up, Key::Up)
            | (Key::Down, Key::Down)
            | (Key::Left, Key::Left)
            | (Key::Right, Key::Right)
            | (Key::Home, Key::Home)
            | (Key::End, Key::End)
            | (Key::PageUp, Key::PageUp)
            | (Key::PageDown, Key::PageDown)
            | (Key::Delete, Key::Delete) => true,
            (Key::F(a), Key::F(b)) => *a == *b,
            _ => false,
        }
    }
}

/// Plain key, no modifiers.
const fn key(k: Key) -> KeyPattern {
    KeyPattern {
        key: k,
        ctrl: false,
        alt: false,
        shift: false,
    }
}

/// Ctrl + key.
const fn ctrl(k: Key) -> KeyPattern {
    KeyPattern {
        key: k,
        ctrl: true,
        alt: false,
        shift: false,
    }
}

// ---------------------------------------------------------------------------
// Binding entry
// ---------------------------------------------------------------------------

/// A single entry in the canonical keymap.
struct Binding {
    pattern: KeyPattern,
    scope: Scope,
    action: Action,
}

// ---------------------------------------------------------------------------
// The canonical keymap
// ---------------------------------------------------------------------------

/// All wa TUI keybindings.  Order matters for resolution within a scope
/// (first match wins), but in practice there should be no duplicates.
static KEYMAP: &[Binding] = &[
    // ---- Global ----
    Binding {
        pattern: key(Key::Char('q')),
        scope: Scope::Global,
        action: Action::Quit,
    },
    Binding {
        pattern: key(Key::Char('?')),
        scope: Scope::Global,
        action: Action::ShowHelp,
    },
    Binding {
        pattern: key(Key::Char('r')),
        scope: Scope::Global,
        action: Action::Refresh,
    },
    Binding {
        pattern: key(Key::Tab),
        scope: Scope::Global,
        action: Action::NextTab,
    },
    Binding {
        pattern: key(Key::BackTab),
        scope: Scope::Global,
        action: Action::PrevTab,
    },
    Binding {
        pattern: key(Key::Char('1')),
        scope: Scope::Global,
        action: Action::GoToView(1),
    },
    Binding {
        pattern: key(Key::Char('2')),
        scope: Scope::Global,
        action: Action::GoToView(2),
    },
    Binding {
        pattern: key(Key::Char('3')),
        scope: Scope::Global,
        action: Action::GoToView(3),
    },
    Binding {
        pattern: key(Key::Char('4')),
        scope: Scope::Global,
        action: Action::GoToView(4),
    },
    Binding {
        pattern: key(Key::Char('5')),
        scope: Scope::Global,
        action: Action::GoToView(5),
    },
    Binding {
        pattern: key(Key::Char('6')),
        scope: Scope::Global,
        action: Action::GoToView(6),
    },
    Binding {
        pattern: key(Key::Char('7')),
        scope: Scope::Global,
        action: Action::GoToView(7),
    },
    Binding {
        pattern: key(Key::Char('8')),
        scope: Scope::Global,
        action: Action::GoToView(8),
    },
    // ---- Panes ----
    Binding {
        pattern: key(Key::Down),
        scope: Scope::Panes,
        action: Action::ListNext,
    },
    Binding {
        pattern: key(Key::Char('j')),
        scope: Scope::Panes,
        action: Action::ListNext,
    },
    Binding {
        pattern: key(Key::Up),
        scope: Scope::Panes,
        action: Action::ListPrev,
    },
    Binding {
        pattern: key(Key::Char('k')),
        scope: Scope::Panes,
        action: Action::ListPrev,
    },
    Binding {
        pattern: key(Key::Char('u')),
        scope: Scope::Panes,
        action: Action::ToggleUnhandledOnly,
    },
    Binding {
        pattern: key(Key::Char('b')),
        scope: Scope::Panes,
        action: Action::ToggleBookmarkedOnly,
    },
    Binding {
        pattern: key(Key::Char('a')),
        scope: Scope::Panes,
        action: Action::CycleAgentFilter,
    },
    Binding {
        pattern: key(Key::Char('d')),
        scope: Scope::Panes,
        action: Action::CycleDomainFilter,
    },
    Binding {
        pattern: key(Key::Char('p')),
        scope: Scope::Panes,
        action: Action::CycleRulesetProfile,
    },
    Binding {
        pattern: key(Key::Enter),
        scope: Scope::Panes,
        action: Action::ApplyRulesetProfile,
    },
    Binding {
        pattern: key(Key::Backspace),
        scope: Scope::Panes,
        action: Action::FilterDeleteChar,
    },
    Binding {
        pattern: key(Key::Esc),
        scope: Scope::Panes,
        action: Action::FilterClear,
    },
    // ---- Events ----
    Binding {
        pattern: key(Key::Down),
        scope: Scope::Events,
        action: Action::ListNext,
    },
    Binding {
        pattern: key(Key::Char('j')),
        scope: Scope::Events,
        action: Action::ListNext,
    },
    Binding {
        pattern: key(Key::Up),
        scope: Scope::Events,
        action: Action::ListPrev,
    },
    Binding {
        pattern: key(Key::Char('k')),
        scope: Scope::Events,
        action: Action::ListPrev,
    },
    Binding {
        pattern: key(Key::Char('u')),
        scope: Scope::Events,
        action: Action::ToggleUnhandledOnly,
    },
    Binding {
        pattern: key(Key::Backspace),
        scope: Scope::Events,
        action: Action::FilterDeleteChar,
    },
    Binding {
        pattern: key(Key::Esc),
        scope: Scope::Events,
        action: Action::FilterClear,
    },
    // ---- Triage ----
    Binding {
        pattern: key(Key::Down),
        scope: Scope::Triage,
        action: Action::ListNext,
    },
    Binding {
        pattern: key(Key::Char('j')),
        scope: Scope::Triage,
        action: Action::ListNext,
    },
    Binding {
        pattern: key(Key::Up),
        scope: Scope::Triage,
        action: Action::ListPrev,
    },
    Binding {
        pattern: key(Key::Char('k')),
        scope: Scope::Triage,
        action: Action::ListPrev,
    },
    Binding {
        pattern: key(Key::Enter),
        scope: Scope::Triage,
        action: Action::TriagePrimaryAction,
    },
    Binding {
        pattern: key(Key::Char('a')),
        scope: Scope::Triage,
        action: Action::TriagePrimaryAction,
    },
    Binding {
        pattern: key(Key::Char('m')),
        scope: Scope::Triage,
        action: Action::TriageMute,
    },
    Binding {
        pattern: key(Key::Char('e')),
        scope: Scope::Triage,
        action: Action::TriageToggleExpand,
    },
    // ---- History ----
    Binding {
        pattern: key(Key::Down),
        scope: Scope::History,
        action: Action::ListNext,
    },
    Binding {
        pattern: key(Key::Char('j')),
        scope: Scope::History,
        action: Action::ListNext,
    },
    Binding {
        pattern: key(Key::Up),
        scope: Scope::History,
        action: Action::ListPrev,
    },
    Binding {
        pattern: key(Key::Char('k')),
        scope: Scope::History,
        action: Action::ListPrev,
    },
    Binding {
        pattern: key(Key::Char('u')),
        scope: Scope::History,
        action: Action::ToggleUndoableOnly,
    },
    Binding {
        pattern: key(Key::Backspace),
        scope: Scope::History,
        action: Action::FilterDeleteChar,
    },
    Binding {
        pattern: key(Key::Esc),
        scope: Scope::History,
        action: Action::FilterClear,
    },
    // ---- Search ----
    Binding {
        pattern: ctrl(Key::Char('n')),
        scope: Scope::Search,
        action: Action::SearchNextSaved,
    },
    Binding {
        pattern: ctrl(Key::Char('p')),
        scope: Scope::Search,
        action: Action::SearchPrevSaved,
    },
    Binding {
        pattern: ctrl(Key::Char('r')),
        scope: Scope::Search,
        action: Action::SearchRunSaved,
    },
    Binding {
        pattern: ctrl(Key::Char('e')),
        scope: Scope::Search,
        action: Action::SearchToggleSaved,
    },
    Binding {
        pattern: key(Key::Down),
        scope: Scope::Search,
        action: Action::ListNext,
    },
    Binding {
        pattern: key(Key::Char('j')),
        scope: Scope::Search,
        action: Action::ListNext,
    },
    Binding {
        pattern: key(Key::Up),
        scope: Scope::Search,
        action: Action::ListPrev,
    },
    Binding {
        pattern: key(Key::Char('k')),
        scope: Scope::Search,
        action: Action::ListPrev,
    },
    Binding {
        pattern: key(Key::Backspace),
        scope: Scope::Search,
        action: Action::FilterDeleteChar,
    },
    Binding {
        pattern: key(Key::Enter),
        scope: Scope::Search,
        action: Action::SearchExecute,
    },
    Binding {
        pattern: key(Key::Esc),
        scope: Scope::Search,
        action: Action::FilterClear,
    },
    // ---- Timeline ----
    Binding {
        pattern: key(Key::Down),
        scope: Scope::Timeline,
        action: Action::ListNext,
    },
    Binding {
        pattern: key(Key::Char('j')),
        scope: Scope::Timeline,
        action: Action::ListNext,
    },
    Binding {
        pattern: key(Key::Up),
        scope: Scope::Timeline,
        action: Action::ListPrev,
    },
    Binding {
        pattern: key(Key::Char('k')),
        scope: Scope::Timeline,
        action: Action::ListPrev,
    },
    Binding {
        pattern: key(Key::Right),
        scope: Scope::Timeline,
        action: Action::TimelineScrollRight,
    },
    Binding {
        pattern: key(Key::Char('l')),
        scope: Scope::Timeline,
        action: Action::TimelineScrollRight,
    },
    Binding {
        pattern: key(Key::Left),
        scope: Scope::Timeline,
        action: Action::TimelineScrollLeft,
    },
    Binding {
        pattern: key(Key::Char('h')),
        scope: Scope::Timeline,
        action: Action::TimelineScrollLeft,
    },
    Binding {
        pattern: key(Key::Char('+')),
        scope: Scope::Timeline,
        action: Action::TimelineZoomIn,
    },
    Binding {
        pattern: key(Key::Char('-')),
        scope: Scope::Timeline,
        action: Action::TimelineZoomOut,
    },
];

// ---------------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------------

/// Convert a view name to its keymap scope.
///
/// Home and Help have no view-specific bindings, so they return `None`.
fn view_scope(view_name: &str) -> Option<Scope> {
    match view_name {
        "Panes" => Some(Scope::Panes),
        "Events" => Some(Scope::Events),
        "Triage" => Some(Scope::Triage),
        "History" => Some(Scope::History),
        "Search" => Some(Scope::Search),
        "Timeline" => Some(Scope::Timeline),
        _ => None,
    }
}

/// Resolve a key input to an action.
///
/// Resolution order:
/// 1. Global bindings (always checked first).
/// 2. View-specific bindings for the active view.
/// 3. Fallback heuristics for unbound printable characters:
///    - Panes/History: `FilterAppendChar` for non-control chars
///    - Events: `EventsFilterDigit` for ASCII digits
///    - Search: `FilterAppendChar` for any char
///    - Triage: `TriageNumberedAction` for digits 1-9
///
/// Returns `None` if the key is not bound in the current context.
pub fn resolve(input: &KeyInput, view_name: &str) -> Option<Action> {
    // 1. Global bindings
    //
    // Special case: `r` without Ctrl is Refresh globally, but `Ctrl+R` in
    // Search is SearchRunSaved.  The legacy code checks `!CONTROL` for `r`.
    // Since global is checked first, we need to skip global `r` when Ctrl is
    // pressed.
    for b in KEYMAP.iter().filter(|b| b.scope == Scope::Global) {
        if b.pattern.matches(input) {
            // Skip global `r` when Ctrl is held (let view-specific handle it)
            if matches!(b.action, Action::Refresh) && input.ctrl {
                continue;
            }
            return Some(b.action);
        }
    }

    // 2. View-specific bindings
    if let Some(scope) = view_scope(view_name) {
        for b in KEYMAP.iter().filter(|b| b.scope == scope) {
            if b.pattern.matches(input) {
                return Some(b.action);
            }
        }

        // 3. Fallback heuristics for unbound printable chars
        if !input.ctrl && !input.alt {
            if let Key::Char(ch) = input.key {
                match scope {
                    Scope::Panes | Scope::History => {
                        if !ch.is_control() {
                            return Some(Action::FilterAppendChar(ch));
                        }
                    }
                    Scope::Events => {
                        if ch.is_ascii_digit() {
                            return Some(Action::EventsFilterDigit(ch));
                        }
                    }
                    Scope::Search => {
                        return Some(Action::FilterAppendChar(ch));
                    }
                    Scope::Triage => {
                        if let Some(digit) = ch.to_digit(10) {
                            if digit >= 1 {
                                return Some(Action::TriageNumberedAction(digit as u8));
                            }
                        }
                    }
                    Scope::Timeline => {}
                    Scope::Global => unreachable!(),
                }
            }
        }
    }

    None
}

/// Return a human-readable description of the action (for help text).
pub fn action_label(action: &Action) -> &'static str {
    match action {
        Action::Quit => "Quit",
        Action::ShowHelp => "Show help",
        Action::Refresh => "Refresh data",
        Action::NextTab => "Next tab",
        Action::PrevTab => "Previous tab",
        Action::GoToView(_) => "Go to view",
        Action::ListNext => "Select next item",
        Action::ListPrev => "Select previous item",
        Action::FilterAppendChar(_) => "Type filter character",
        Action::FilterDeleteChar => "Delete filter character",
        Action::FilterClear => "Clear filter",
        Action::ToggleUnhandledOnly => "Toggle unhandled only",
        Action::ToggleBookmarkedOnly => "Toggle bookmarked only",
        Action::CycleAgentFilter => "Cycle agent filter",
        Action::CycleDomainFilter => "Cycle domain filter",
        Action::CycleRulesetProfile => "Cycle ruleset profile",
        Action::ApplyRulesetProfile => "Apply ruleset profile",
        Action::EventsFilterDigit(_) => "Filter by pane ID digit",
        Action::TriagePrimaryAction => "Execute primary action",
        Action::TriageMute => "Mute selected event",
        Action::TriageToggleExpand => "Toggle workflow expand",
        Action::TriageNumberedAction(_) => "Execute numbered action",
        Action::ToggleUndoableOnly => "Toggle undoable only",
        Action::SearchNextSaved => "Next saved search",
        Action::SearchPrevSaved => "Previous saved search",
        Action::SearchRunSaved => "Run saved search",
        Action::SearchToggleSaved => "Toggle saved search",
        Action::SearchExecute => "Execute search",
        Action::TimelineZoomIn => "Zoom in timeline",
        Action::TimelineZoomOut => "Zoom out timeline",
        Action::TimelineScrollLeft => "Scroll timeline left",
        Action::TimelineScrollRight => "Scroll timeline right",
    }
}

/// Return all bindings for a given scope (for building help text).
pub fn bindings_for_scope(scope: Scope) -> Vec<(&'static KeyPattern, Action)> {
    KEYMAP
        .iter()
        .filter(|b| b.scope == scope)
        .map(|b| (&b.pattern, b.action))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ki(k: Key) -> KeyInput {
        KeyInput::new(k)
    }

    fn ki_ctrl(k: Key) -> KeyInput {
        KeyInput {
            key: k,
            ctrl: true,
            alt: false,
            shift: false,
        }
    }

    // -- global parity --

    #[test]
    fn parity_global_quit() {
        assert_eq!(resolve(&ki(Key::Char('q')), "Home"), Some(Action::Quit));
        assert_eq!(resolve(&ki(Key::Char('q')), "Panes"), Some(Action::Quit));
        assert_eq!(resolve(&ki(Key::Char('q')), "Search"), Some(Action::Quit));
    }

    #[test]
    fn parity_global_help() {
        assert_eq!(resolve(&ki(Key::Char('?')), "Home"), Some(Action::ShowHelp));
    }

    #[test]
    fn parity_global_refresh() {
        assert_eq!(resolve(&ki(Key::Char('r')), "Home"), Some(Action::Refresh));
    }

    #[test]
    fn parity_global_refresh_not_ctrl() {
        // Ctrl+R should NOT be global refresh (it's search-specific)
        assert_ne!(
            resolve(&ki_ctrl(Key::Char('r')), "Search"),
            Some(Action::Refresh)
        );
        assert_eq!(
            resolve(&ki_ctrl(Key::Char('r')), "Search"),
            Some(Action::SearchRunSaved)
        );
    }

    #[test]
    fn parity_global_tab_navigation() {
        assert_eq!(resolve(&ki(Key::Tab), "Home"), Some(Action::NextTab));
        assert_eq!(resolve(&ki(Key::BackTab), "Home"), Some(Action::PrevTab));
    }

    #[test]
    fn parity_global_number_keys() {
        for n in 1..=7 {
            let ch = char::from_digit(n, 10).unwrap();
            assert_eq!(
                resolve(&ki(Key::Char(ch)), "Home"),
                Some(Action::GoToView(n as u8))
            );
        }
    }

    // -- panes parity --

    #[test]
    fn parity_panes_list_nav() {
        assert_eq!(resolve(&ki(Key::Down), "Panes"), Some(Action::ListNext));
        assert_eq!(
            resolve(&ki(Key::Char('j')), "Panes"),
            Some(Action::ListNext)
        );
        assert_eq!(resolve(&ki(Key::Up), "Panes"), Some(Action::ListPrev));
        assert_eq!(
            resolve(&ki(Key::Char('k')), "Panes"),
            Some(Action::ListPrev)
        );
    }

    #[test]
    fn parity_panes_toggles() {
        assert_eq!(
            resolve(&ki(Key::Char('u')), "Panes"),
            Some(Action::ToggleUnhandledOnly)
        );
        assert_eq!(
            resolve(&ki(Key::Char('b')), "Panes"),
            Some(Action::ToggleBookmarkedOnly)
        );
        assert_eq!(
            resolve(&ki(Key::Char('a')), "Panes"),
            Some(Action::CycleAgentFilter)
        );
        assert_eq!(
            resolve(&ki(Key::Char('d')), "Panes"),
            Some(Action::CycleDomainFilter)
        );
    }

    #[test]
    fn parity_panes_profile() {
        assert_eq!(
            resolve(&ki(Key::Char('p')), "Panes"),
            Some(Action::CycleRulesetProfile)
        );
        assert_eq!(
            resolve(&ki(Key::Enter), "Panes"),
            Some(Action::ApplyRulesetProfile)
        );
    }

    #[test]
    fn parity_panes_filter() {
        assert_eq!(
            resolve(&ki(Key::Backspace), "Panes"),
            Some(Action::FilterDeleteChar)
        );
        assert_eq!(resolve(&ki(Key::Esc), "Panes"), Some(Action::FilterClear));
        assert_eq!(
            resolve(&ki(Key::Char('x')), "Panes"),
            Some(Action::FilterAppendChar('x'))
        );
    }

    // -- events parity --

    #[test]
    fn parity_events_filter_digit() {
        assert_eq!(
            resolve(&ki(Key::Char('5')), "Events"),
            // Global GoToView(5) wins over events digit filter
            Some(Action::GoToView(5))
        );
        // But 0, 9 are not global (8 is now GoToView(8))
        assert_eq!(
            resolve(&ki(Key::Char('0')), "Events"),
            Some(Action::EventsFilterDigit('0'))
        );
        assert_eq!(
            resolve(&ki(Key::Char('8')), "Events"),
            Some(Action::GoToView(8))
        );
    }

    #[test]
    fn parity_events_non_digit_ignored() {
        // Non-digit printable chars are not bound in Events
        assert_eq!(resolve(&ki(Key::Char('x')), "Events"), None);
    }

    // -- triage parity --

    #[test]
    fn parity_triage_actions() {
        assert_eq!(
            resolve(&ki(Key::Enter), "Triage"),
            Some(Action::TriagePrimaryAction)
        );
        assert_eq!(
            resolve(&ki(Key::Char('a')), "Triage"),
            Some(Action::TriagePrimaryAction)
        );
        assert_eq!(
            resolve(&ki(Key::Char('m')), "Triage"),
            Some(Action::TriageMute)
        );
        assert_eq!(
            resolve(&ki(Key::Char('e')), "Triage"),
            Some(Action::TriageToggleExpand)
        );
    }

    #[test]
    fn parity_triage_numbered_actions() {
        // 1-8 are global (GoToView), 9 is triage-specific
        assert_eq!(
            resolve(&ki(Key::Char('8')), "Triage"),
            Some(Action::GoToView(8))
        );
        assert_eq!(
            resolve(&ki(Key::Char('9')), "Triage"),
            Some(Action::TriageNumberedAction(9))
        );
        // 0 has no triage action (digit >= 1 required)
        assert_eq!(resolve(&ki(Key::Char('0')), "Triage"), None);
    }

    // -- history parity --

    #[test]
    fn parity_history_undoable_toggle() {
        assert_eq!(
            resolve(&ki(Key::Char('u')), "History"),
            Some(Action::ToggleUndoableOnly)
        );
    }

    #[test]
    fn parity_history_esc_clears() {
        assert_eq!(resolve(&ki(Key::Esc), "History"), Some(Action::FilterClear));
    }

    #[test]
    fn parity_history_filter_input() {
        assert_eq!(
            resolve(&ki(Key::Char('z')), "History"),
            Some(Action::FilterAppendChar('z'))
        );
    }

    // -- search parity --

    #[test]
    fn parity_search_saved_controls() {
        assert_eq!(
            resolve(&ki_ctrl(Key::Char('n')), "Search"),
            Some(Action::SearchNextSaved)
        );
        assert_eq!(
            resolve(&ki_ctrl(Key::Char('p')), "Search"),
            Some(Action::SearchPrevSaved)
        );
        assert_eq!(
            resolve(&ki_ctrl(Key::Char('e')), "Search"),
            Some(Action::SearchToggleSaved)
        );
    }

    #[test]
    fn parity_search_execute() {
        assert_eq!(
            resolve(&ki(Key::Enter), "Search"),
            Some(Action::SearchExecute)
        );
    }

    #[test]
    fn parity_search_char_input() {
        assert_eq!(
            resolve(&ki(Key::Char('z')), "Search"),
            Some(Action::FilterAppendChar('z'))
        );
    }

    // -- conflict policy --

    #[test]
    fn global_wins_over_view_specific() {
        // 'q' is global quit — even in views with text input
        assert_eq!(resolve(&ki(Key::Char('q')), "Panes"), Some(Action::Quit));
        assert_eq!(resolve(&ki(Key::Char('q')), "Search"), Some(Action::Quit));
    }

    #[test]
    fn home_and_help_have_no_view_bindings() {
        // Only global bindings work in Home/Help
        assert_eq!(resolve(&ki(Key::Char('q')), "Home"), Some(Action::Quit));
        assert_eq!(resolve(&ki(Key::Down), "Home"), None);
        assert_eq!(resolve(&ki(Key::Down), "Help"), None);
    }

    // -- structural --

    #[test]
    fn no_duplicate_patterns_within_scope() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for b in KEYMAP {
            let key = format!("{:?}|{:?}", b.scope, b.pattern);
            assert!(seen.insert(key.clone()), "Duplicate keymap entry: {key}");
        }
    }

    #[test]
    fn all_global_bindings_have_labels() {
        for b in KEYMAP.iter().filter(|b| b.scope == Scope::Global) {
            let label = action_label(&b.action);
            assert!(!label.is_empty());
        }
    }

    #[test]
    fn bindings_for_scope_returns_correct_count() {
        let globals = bindings_for_scope(Scope::Global);
        assert_eq!(globals.len(), 13); // q ? r Tab BackTab 1-8
    }
}
