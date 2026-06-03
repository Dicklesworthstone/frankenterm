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

            let handle = std::thread::spawn(|| {
                let _guard = ENTRIES.lock().unwrap();
                panic!("simulate GUI debug log poison");
            });

            assert!(handle.join().is_err());

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
