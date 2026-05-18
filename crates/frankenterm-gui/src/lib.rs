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
