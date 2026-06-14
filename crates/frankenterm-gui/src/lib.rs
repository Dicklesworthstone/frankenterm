#![cfg_attr(not(feature = "headless-render"), allow(dead_code))]
// Keep this in sync with Cargo.toml: the vendored GUI crate is not yet a
// pedantic-clean primary lint target.
#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

pub mod accessibility_preferences;
pub mod adaptive_fps_loop;
pub mod floating_panes;
pub mod gpu_regression;
pub mod gpu_regression_fuzz;
pub mod input_loop;
pub mod osc8_gui;
pub mod plugins;
pub mod renderer_slo;
pub mod rollout_env;

/// Keep drag-selection geometry alive while live output dirties rows under it.
///
/// This lives in the library crate so the binary-owned `termwindow` selection
/// lifecycle can keep a small pure predicate under normal `cargo test --lib`
/// coverage.
pub fn should_preserve_dirty_selection_during_mouse_drag(
    active_selection_drag_pane_id: Option<usize>,
    captured_pane_id: Option<usize>,
    left_mouse_button_down: bool,
    pane_id: usize,
) -> bool {
    left_mouse_button_down
        && active_selection_drag_pane_id == Some(pane_id)
        && captured_pane_id == Some(pane_id)
}

/// Build an exclusive stable-row range from a top row and visible row count.
///
/// Binary-owned render code uses this to avoid wrapping stable-row arithmetic
/// when a viewport is near the representable row boundary.
pub fn checked_stable_row_range_from_top(
    top: wezterm_term::StableRowIndex,
    row_count: usize,
) -> Option<std::ops::Range<wezterm_term::StableRowIndex>> {
    let row_count =
        <wezterm_term::StableRowIndex as std::convert::TryFrom<usize>>::try_from(row_count).ok()?;
    let end = top.checked_add(row_count)?;
    Some(top..end)
}

#[doc(hidden)]
pub mod owner_last_guard {
    use std::mem::ManuallyDrop;

    pub struct OwnerLastGuardedMapping<M, S, O> {
        mapping: ManuallyDrop<M>,
        slice: ManuallyDrop<S>,
        owner: ManuallyDrop<O>,
    }

    impl<M, S, O> OwnerLastGuardedMapping<M, S, O> {
        pub fn new(mapping: M, slice: S, owner: O) -> Self {
            Self {
                mapping: ManuallyDrop::new(mapping),
                slice: ManuallyDrop::new(slice),
                owner: ManuallyDrop::new(owner),
            }
        }

        pub fn mapping_mut(&mut self) -> &mut M {
            &mut self.mapping
        }
    }

    impl<M, S, O> Drop for OwnerLastGuardedMapping<M, S, O> {
        fn drop(&mut self) {
            unsafe {
                // SAFETY: each field is wrapped in ManuallyDrop and is dropped exactly
                // once here, in dependency order, so derived views go away before owner.
                ManuallyDrop::drop(&mut self.mapping);
                ManuallyDrop::drop(&mut self.slice);
                ManuallyDrop::drop(&mut self.owner);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::OwnerLastGuardedMapping;
        use std::cell::RefCell;
        use std::rc::Rc;

        struct DropProbe {
            name: &'static str,
            log: Rc<RefCell<Vec<&'static str>>>,
        }

        impl DropProbe {
            fn new(name: &'static str, log: &Rc<RefCell<Vec<&'static str>>>) -> Self {
                Self {
                    name,
                    log: Rc::clone(log),
                }
            }
        }

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.log.borrow_mut().push(self.name);
            }
        }

        #[test]
        fn drops_derived_mapping_and_slice_before_owner() {
            let log = Rc::new(RefCell::new(Vec::new()));

            {
                let mut guard = OwnerLastGuardedMapping::new(
                    DropProbe::new("mapping", &log),
                    DropProbe::new("slice", &log),
                    DropProbe::new("owner", &log),
                );

                assert_eq!(guard.mapping_mut().name, "mapping");
            }

            assert_eq!(&*log.borrow(), &["mapping", "slice", "owner"]);
        }
    }
}

#[cfg(test)]
mod selection_lifecycle_tests {
    use super::{
        checked_stable_row_range_from_top, should_preserve_dirty_selection_during_mouse_drag,
    };
    use wezterm_term::StableRowIndex;

    #[test]
    fn dirty_selection_is_preserved_only_for_active_left_drag_on_same_pane() {
        assert!(should_preserve_dirty_selection_during_mouse_drag(
            Some(7),
            Some(7),
            true,
            7,
        ));
        assert!(!should_preserve_dirty_selection_during_mouse_drag(
            None,
            Some(7),
            true,
            7,
        ));
        assert!(!should_preserve_dirty_selection_during_mouse_drag(
            Some(7),
            None,
            true,
            7,
        ));
        assert!(!should_preserve_dirty_selection_during_mouse_drag(
            Some(7),
            Some(8),
            true,
            7,
        ));
        assert!(!should_preserve_dirty_selection_during_mouse_drag(
            Some(8),
            Some(7),
            true,
            7,
        ));
        assert!(!should_preserve_dirty_selection_during_mouse_drag(
            Some(7),
            Some(7),
            false,
            7,
        ));
    }

    #[test]
    fn checked_stable_row_range_from_top_rejects_unrepresentable_ranges() {
        assert_eq!(checked_stable_row_range_from_top(10, 3), Some(10..13));
        assert_eq!(
            checked_stable_row_range_from_top(StableRowIndex::MAX, 1),
            None
        );
        assert_eq!(checked_stable_row_range_from_top(0, usize::MAX), None);
    }
}

pub mod command_rules {
    use config::keyassignment::KeyAssignment::*;
    use config::keyassignment::*;
    use config::window::WindowLevel;
    use mux::domain::DomainState;
    use ordered_float::NotNan;
    use window::Modifiers;

    pub const PANE_SELECT_DEFAULT_MODES: [PaneSelectMode; 5] = [
        PaneSelectMode::Activate,
        PaneSelectMode::SwapWithActive,
        PaneSelectMode::SwapWithActiveKeepFocus,
        PaneSelectMode::MoveToNewTab,
        PaneSelectMode::MoveToNewWindow,
    ];

    pub fn domain_detach_command_is_available(
        name: &str,
        state: DomainState,
        detachable: bool,
    ) -> bool {
        state == DomainState::Attached && detachable && name != "local"
    }

    pub fn pane_select_default_keys(mode: PaneSelectMode) -> Vec<(Modifiers, String)> {
        match mode {
            PaneSelectMode::Activate => {
                vec![(Modifiers::CTRL.union(Modifiers::SHIFT), "9".into())]
            }
            PaneSelectMode::SwapWithActive => {
                vec![(Modifiers::CTRL.union(Modifiers::SHIFT), "0".into())]
            }
            PaneSelectMode::SwapWithActiveKeepFocus => {
                vec![(Modifiers::SUPER.union(Modifiers::SHIFT), "0".into())]
            }
            PaneSelectMode::MoveToNewTab => {
                vec![(Modifiers::CTRL.union(Modifiers::SHIFT), "t".into())]
            }
            PaneSelectMode::MoveToNewWindow => {
                vec![(Modifiers::CTRL.union(Modifiers::SHIFT), "y".into())]
            }
        }
    }

    pub fn pane_select_default_action(mode: PaneSelectMode) -> KeyAssignment {
        PaneSelect(PaneSelectArguments {
            alphabet: String::new(),
            mode,
            show_pane_ids: false,
        })
    }

    /// Returns a list of key assignment actions that should be included in
    /// the default key assignments and command palette.
    pub fn compute_default_actions() -> Vec<KeyAssignment> {
        // These are ordered by their position within the various menus.
        vec![
            // ----------------- WezTerm
            ReloadConfiguration,
            #[cfg(target_os = "macos")]
            HideApplication,
            #[cfg(target_os = "macos")]
            QuitApplication,
            // ----------------- Shell
            SpawnTab(SpawnTabDomain::CurrentPaneDomain),
            SpawnWindow,
            SplitVertical(SpawnCommand {
                domain: SpawnTabDomain::CurrentPaneDomain,
                ..Default::default()
            }),
            SplitHorizontal(SpawnCommand {
                domain: SpawnTabDomain::CurrentPaneDomain,
                ..Default::default()
            }),
            CloseCurrentTab { confirm: true },
            CloseCurrentPane { confirm: true },
            ResetTerminal,
            // ----------------- Edit
            #[cfg(not(target_os = "macos"))]
            PasteFrom(ClipboardPasteSource::PrimarySelection),
            #[cfg(not(target_os = "macos"))]
            CopyTo(ClipboardCopyDestination::PrimarySelection),
            CopyTo(ClipboardCopyDestination::Clipboard),
            PasteFrom(ClipboardPasteSource::Clipboard),
            ClearScrollback(ScrollbackEraseMode::ScrollbackOnly),
            ClearScrollback(ScrollbackEraseMode::ScrollbackAndViewport),
            QuickSelect,
            CharSelect(CharSelectArguments::default()),
            ActivateCopyMode,
            ClearKeyTableStack,
            ActivateCommandPalette,
            // ----------------- View
            DecreaseFontSize,
            IncreaseFontSize,
            ResetFontSize,
            ResetFontAndWindowSize,
            ScrollByPage(NotNan::new(-1.0).unwrap()),
            ScrollByPage(NotNan::new(1.0).unwrap()),
            ScrollToTop,
            ScrollToBottom,
            // ----------------- Window
            ToggleFullScreen,
            ToggleAlwaysOnTop,
            ToggleAlwaysOnBottom,
            SetWindowLevel(WindowLevel::AlwaysOnBottom),
            SetWindowLevel(WindowLevel::Normal),
            SetWindowLevel(WindowLevel::AlwaysOnTop),
            Hide,
            Search(Pattern::CurrentSelectionOrEmptyString),
            pane_select_default_action(PaneSelectMode::Activate),
            pane_select_default_action(PaneSelectMode::SwapWithActive),
            pane_select_default_action(PaneSelectMode::SwapWithActiveKeepFocus),
            pane_select_default_action(PaneSelectMode::MoveToNewTab),
            pane_select_default_action(PaneSelectMode::MoveToNewWindow),
            RotatePanes(RotationDirection::Clockwise),
            RotatePanes(RotationDirection::CounterClockwise),
            // --- Swap Layouts & Floating Panes ---
            SwapLayoutNext,
            SwapLayoutPrev,
            ToggleFloatingPane,
            FloatingPaneCommand(FloatingPaneKeyCommand::SnapLeft),
            FloatingPaneCommand(FloatingPaneKeyCommand::SnapRight),
            FloatingPaneCommand(FloatingPaneKeyCommand::RaiseToTop),
            FloatingPaneCommand(FloatingPaneKeyCommand::CycleOverlapping),
            CycleStackForward,
            CycleStackBackward,
            // --- Agent swarm mass operations ---
            KillStuckAgents,
            PauseAllAgents,
            FocusErrorPanes,
            CycleAgentAutoLayout,
            ToggleDashboard,
            ActivateTab(0),
            ActivateTab(1),
            ActivateTab(2),
            ActivateTab(3),
            ActivateTab(4),
            ActivateTab(5),
            ActivateTab(6),
            ActivateTab(7),
            ActivateTab(-1),
            ActivateTabRelative(-1),
            ActivateTabRelative(1),
            ActivateWindow(0),
            ActivateWindow(1),
            ActivateWindow(2),
            ActivateWindow(3),
            ActivateWindow(4),
            ActivateWindow(5),
            ActivateWindow(6),
            ActivateWindow(7),
            ActivateWindow(8),
            ActivateWindow(9),
            ActivateWindowRelative(-1),
            ActivateWindowRelative(1),
            MoveTabRelative(-1),
            MoveTabRelative(1),
            AdjustPaneSize(PaneDirection::Left, 1),
            AdjustPaneSize(PaneDirection::Right, 1),
            AdjustPaneSize(PaneDirection::Up, 1),
            AdjustPaneSize(PaneDirection::Down, 1),
            ActivatePaneDirection(PaneDirection::Left),
            ActivatePaneDirection(PaneDirection::Right),
            ActivatePaneDirection(PaneDirection::Up),
            ActivatePaneDirection(PaneDirection::Down),
            TogglePaneZoomState,
            ActivateLastTab,
            ShowLauncher,
            ShowTabNavigator,
            // ----------------- Help
            OpenUri("https://github.com/Dicklesworthstone/frankenterm".to_string()),
            OpenUri("https://github.com/Dicklesworthstone/frankenterm/discussions/".to_string()),
            OpenUri("https://github.com/Dicklesworthstone/frankenterm/issues/".to_string()),
            ShowDebugOverlay,
            // ----------------- Misc
            OpenLinkAtMouseCursor,
        ]
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn detach_domain_commands_require_detachable_attached_non_local_domain() {
            assert!(domain_detach_command_is_available(
                "remote",
                DomainState::Attached,
                true,
            ));

            assert!(!domain_detach_command_is_available(
                "remote",
                DomainState::Attached,
                false,
            ));
            assert!(!domain_detach_command_is_available(
                "remote",
                DomainState::Detached,
                true,
            ));
            assert!(!domain_detach_command_is_available(
                "local",
                DomainState::Attached,
                true,
            ));
        }

        /// Every `PaneSelect` mode must ship with at least one default chord.
        /// Pre-fix the five rows had `keys: vec![]` and a "FIXME" comment, so a
        /// freshly-installed user could only reach pane-management through the
        /// menu / lua. This test fences that regression.
        #[test]
        fn pane_select_modes_all_carry_default_keybindings() {
            for mode in PANE_SELECT_DEFAULT_MODES {
                let keys = pane_select_default_keys(mode);
                assert!(
                    !keys.is_empty(),
                    "PaneSelectMode::{mode:?} ships without a default chord"
                );
            }
        }

        /// The five `PaneSelect` defaults must all be distinct chords. A
        /// silent collision would mean two modes fire on the same key press,
        /// which is its own accessibility bug.
        #[test]
        fn pane_select_default_chords_are_pairwise_distinct() {
            let chords: Vec<_> = PANE_SELECT_DEFAULT_MODES
                .into_iter()
                .map(|mode| pane_select_default_keys(mode)[0].clone())
                .collect();
            let mut seen = std::collections::HashSet::new();
            for (mods, key) in &chords {
                let label = format!("{mods:?}+{key}");
                assert!(
                    seen.insert(label.clone()),
                    "duplicate PaneSelect default chord: {label}"
                );
            }
        }

        #[test]
        fn unqualified_current_domain_detach_is_not_a_default_palette_action() {
            assert!(
                !compute_default_actions().iter().any(|action| matches!(
                    action,
                    DetachDomain(SpawnTabDomain::CurrentPaneDomain)
                )),
                "CurrentPaneDomain detach depends on the active pane domain being detachable; \
                 generated domain-specific detach entries carry that runtime capability check"
            );
        }
    }
}
pub mod status_text {
    use finl_unicode::grapheme_clusters::Graphemes;
    use termwiz::cell::{Cell, CellAttributes};
    use termwiz::color::ColorSpec;
    use termwiz::escape::csi::Sgr;
    use termwiz::escape::parser::Parser;
    use termwiz::escape::{Action, CSI, ControlCode};
    use termwiz::surface::SEQ_ZERO;
    use wezterm_term::Line;

    const MAX_STATUS_PARSE_CELLS: usize = 4096;
    const MAX_STATUS_PARSE_BYTES: usize = 64 * 1024;
    const STATUS_BYTES_PER_CELL_BUDGET: usize = 64;

    pub fn parse_status_text(text: &str, default_cell: CellAttributes) -> Line {
        parse_status_text_with_cell_limit(text, default_cell, MAX_STATUS_PARSE_CELLS)
    }

    pub fn parse_status_text_with_cell_limit(
        text: &str,
        default_cell: CellAttributes,
        max_cells: usize,
    ) -> Line {
        let max_cells = max_cells.min(MAX_STATUS_PARSE_CELLS);
        if max_cells == 0 {
            return Line::with_width(0, SEQ_ZERO);
        }

        let max_bytes = max_cells
            .saturating_mul(STATUS_BYTES_PER_CELL_BUDGET)
            .clamp(1, MAX_STATUS_PARSE_BYTES);
        let text = status_text_prefix(text, max_bytes);
        let mut pen = default_cell.clone();
        let mut cells = vec![];
        let mut ignoring = false;
        let mut print_buffer = String::new();

        fn flush_print(
            buf: &mut String,
            cells: &mut Vec<Cell>,
            pen: &CellAttributes,
            max_cells: usize,
        ) {
            for g in Graphemes::new(buf.as_str()) {
                if cells.len() >= max_cells {
                    break;
                }
                let cell = Cell::new_grapheme(g, pen.clone(), None);
                let width = cell.width();
                if cells.len().saturating_add(width) > max_cells {
                    break;
                }
                cells.push(cell);
                for _ in 1..width {
                    // Line/Screen expect double wide graphemes to be followed by a blank in
                    // the next column position, otherwise we'll render incorrectly.
                    cells.push(Cell::blank_with_attrs(pen.clone()));
                }
            }
            buf.clear();
        }

        let mut parser = Parser::new();
        parser.parse(text.as_bytes(), |action| {
            if ignoring || cells.len() >= max_cells {
                return;
            }
            match action {
                Action::Print(c) => print_buffer.push(c),
                Action::PrintString(s) => print_buffer.push_str(&s),
                Action::Control(c) => {
                    flush_print(&mut print_buffer, &mut cells, &pen, max_cells);
                    match c {
                        ControlCode::CarriageReturn | ControlCode::LineFeed => {
                            ignoring = true;
                        }
                        _ => {}
                    }
                }
                Action::CSI(csi) => {
                    flush_print(&mut print_buffer, &mut cells, &pen, max_cells);
                    match csi {
                        CSI::Sgr(sgr) => match sgr {
                            Sgr::Reset => pen = default_cell.clone(),
                            Sgr::Intensity(i) => {
                                pen.set_intensity(i);
                            }
                            Sgr::Underline(u) => {
                                pen.set_underline(u);
                            }
                            Sgr::Overline(o) => {
                                pen.set_overline(o);
                            }
                            Sgr::VerticalAlign(o) => {
                                pen.set_vertical_align(o);
                            }
                            Sgr::Blink(b) => {
                                pen.set_blink(b);
                            }
                            Sgr::Italic(i) => {
                                pen.set_italic(i);
                            }
                            Sgr::Inverse(inverse) => {
                                pen.set_reverse(inverse);
                            }
                            Sgr::Invisible(invis) => {
                                pen.set_invisible(invis);
                            }
                            Sgr::StrikeThrough(strike) => {
                                pen.set_strikethrough(strike);
                            }
                            Sgr::Foreground(col) => {
                                if let ColorSpec::Default = col {
                                    pen.set_foreground(default_cell.foreground());
                                } else {
                                    pen.set_foreground(col);
                                }
                            }
                            Sgr::Background(col) => {
                                if let ColorSpec::Default = col {
                                    pen.set_background(default_cell.background());
                                } else {
                                    pen.set_background(col);
                                }
                            }
                            Sgr::UnderlineColor(col) => {
                                pen.set_underline_color(col);
                            }
                            Sgr::Font(_) => {}
                        },
                        _ => {}
                    }
                }
                Action::OperatingSystemCommand(_)
                | Action::DeviceControl(_)
                | Action::Esc(_)
                | Action::KittyImage(_)
                | Action::XtGetTcap(_)
                | Action::Sixel(_) => {
                    flush_print(&mut print_buffer, &mut cells, &pen, max_cells);
                }
            }
        });
        flush_print(&mut print_buffer, &mut cells, &pen, max_cells);
        Line::from_cells(cells, SEQ_ZERO)
    }

    fn status_text_prefix(text: &str, max_bytes: usize) -> &str {
        if text.len() <= max_bytes {
            return text;
        }

        let mut end = max_bytes;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn status_text_parser_respects_cell_limit() {
            let line = parse_status_text_with_cell_limit("abcdef", CellAttributes::default(), 3);

            assert_eq!(line.len(), 3);
            assert_eq!(line.as_str().as_ref(), "abc");
        }

        #[test]
        fn status_text_parser_does_not_split_double_width_graphemes() {
            let line =
                parse_status_text_with_cell_limit("\u{1f600}abc", CellAttributes::default(), 1);

            assert_eq!(line.len(), 0);
        }
    }
}
pub mod selector_math {
    pub fn selector_label_count(filtered_entries_len: usize, max_items: usize) -> usize {
        filtered_entries_len.min(max_items.saturating_add(1))
    }

    pub fn visible_mouse_entry_index(
        top_row: usize,
        y: u16,
        filtered_entries_len: usize,
    ) -> Option<usize> {
        let row_offset = usize::from(y).checked_sub(1)?;
        let active_idx = top_row.saturating_add(row_offset);
        (active_idx < filtered_entries_len).then_some(active_idx)
    }

    #[cfg(test)]
    mod tests {
        use super::{selector_label_count, visible_mouse_entry_index};

        #[test]
        fn selector_label_count_tracks_filtered_entries_with_visible_row() {
            assert_eq!(selector_label_count(2, 10), 2);
            assert_eq!(selector_label_count(9, 10), 9);
        }

        #[test]
        fn selector_label_count_caps_to_visible_rows_plus_one() {
            assert_eq!(selector_label_count(25, 3), 4);
            assert_eq!(selector_label_count(25, 0), 1);
        }

        #[test]
        fn selector_label_count_saturates_extreme_row_capacity() {
            assert_eq!(selector_label_count(usize::MAX, usize::MAX), usize::MAX);
        }

        #[test]
        fn selector_visible_mouse_entry_index_maps_screen_row_to_filtered_entry() {
            assert_eq!(visible_mouse_entry_index(0, 1, 3), Some(0));
            assert_eq!(visible_mouse_entry_index(5, 2, 10), Some(6));
        }

        #[test]
        fn selector_visible_mouse_entry_index_rejects_header_and_filtered_tail() {
            assert_eq!(visible_mouse_entry_index(0, 0, 3), None);
            assert_eq!(visible_mouse_entry_index(8, 4, 10), None);
        }

        #[test]
        fn selector_visible_mouse_entry_index_saturates_scrolled_extremes() {
            assert_eq!(visible_mouse_entry_index(usize::MAX, 2, usize::MAX), None);
        }
    }
}
pub mod smart_selection_a11y;
pub mod status_bar;
pub mod triple_buffer_gui;

pub mod gui_debug_log {
    use chrono::{DateTime, Local};
    use log::Level;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, MutexGuard};

    const CAPACITY: usize = 256;

    #[derive(Debug, Clone)]
    pub struct GuiDebugLogEntry {
        pub sequence: u64,
        pub then: DateTime<Local>,
        pub level: Level,
        pub target: String,
        pub message: String,
    }

    static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    lazy_static::lazy_static! {
        static ref ENTRIES: Mutex<VecDeque<GuiDebugLogEntry>> =
            Mutex::new(VecDeque::with_capacity(CAPACITY));
    }

    fn lock_entries(_context: &str) -> MutexGuard<'static, VecDeque<GuiDebugLogEntry>> {
        ENTRIES.lock().unwrap_or_else(|poisoned| {
            // Avoid log::warn! here: this is the log sink and may re-enter ENTRIES.
            ENTRIES.clear_poison();
            poisoned.into_inner()
        })
    }

    pub fn record(level: Level, target: impl Into<String>, message: impl Into<String>) -> u64 {
        let sequence = NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let entry = GuiDebugLogEntry {
            sequence,
            then: Local::now(),
            level,
            target: target.into(),
            message: message.into(),
        };

        let mut entries = lock_entries("recording log entry");
        if entries.len() == CAPACITY {
            entries.pop_front();
        }
        entries.push_back(entry);
        sequence
    }

    pub fn entries_after(sequence: Option<u64>) -> Vec<GuiDebugLogEntry> {
        let min_sequence = sequence.unwrap_or(0);
        lock_entries("reading log entries")
            .iter()
            .filter(|entry| entry.sequence > min_sequence)
            .cloned()
            .collect()
    }

    #[cfg(test)]
    fn reset_for_tests() {
        NEXT_SEQUENCE.store(1, Ordering::Relaxed);
        lock_entries("resetting test state").clear();
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        lazy_static::lazy_static! {
            static ref TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        }

        fn lock_test() -> std::sync::MutexGuard<'static, ()> {
            TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        #[test]
        fn entries_after_filters_by_sequence() {
            let _guard = lock_test();
            reset_for_tests();

            let first = record(Level::Info, "test", "first");
            let second = record(Level::Warn, "test", "second");

            let entries = entries_after(Some(first));
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].sequence, second);
            assert_eq!(entries[0].message, "second");
        }

        #[test]
        fn entries_are_bounded_to_recent_capacity() {
            let _guard = lock_test();
            reset_for_tests();

            for index in 0..(CAPACITY + 4) {
                record(Level::Info, "test", format!("entry-{index}"));
            }

            let entries = entries_after(None);
            assert_eq!(entries.len(), CAPACITY);
            assert_eq!(entries[0].message, "entry-4");
            assert_eq!(
                entries[CAPACITY - 1].message,
                format!("entry-{}", CAPACITY + 3)
            );
        }

        #[test]
        fn entries_recover_after_poisoned_lock() {
            let _guard = lock_test();
            reset_for_tests();

            let poison_result = std::panic::catch_unwind(|| {
                let _guard = ENTRIES
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                std::panic::resume_unwind(Box::new("simulate GUI debug log poison"));
            });

            assert!(poison_result.is_err());

            let sequence = record(Level::Error, "test", "after poison");
            let entries = entries_after(None);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].sequence, sequence);
            assert_eq!(entries[0].message, "after poison");
        }
    }
}

#[cfg(any(feature = "debug-cell-crc", test))]
pub mod cell_crc;

#[cfg(feature = "headless-render")]
pub mod headless_render;

#[cfg(test)]
extern crate self as frankenterm_gui;

#[cfg(test)]
#[path = "../tests/ssim_parity.rs"]
mod ssim_parity;
