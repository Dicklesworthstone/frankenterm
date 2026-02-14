//! Screen state tracking via escape sequence detection
//!
//! This module tracks terminal screen state (primarily alt-screen mode) by
//! parsing escape sequences in captured output. This eliminates the need
//! for Lua hooks in WezTerm, dramatically improving performance.
//!
//! # Background
//!
//! Terminal applications switch to the alternate screen buffer using
//! standardized escape sequences:
//!
//! - Enter alt-screen: `ESC [ ? 1049 h` (smcup)
//! - Leave alt-screen: `ESC [ ? 1049 l` (rmcup)
//!
//! Some older applications use `ESC [ ? 47 h/l` instead.
//!
//! # Usage
//!
//! ```rust,ignore
//! use frankenterm_core::screen_state::ScreenStateTracker;
//!
//! let mut tracker = ScreenStateTracker::new();
//!
//! // Process captured terminal output
//! tracker.process_output(pane_id, output_bytes);
//!
//! // Query state
//! if tracker.is_alt_screen(pane_id) {
//!     // Pane is in alternate screen mode (vim, less, etc.)
//! }
//! ```

use std::collections::HashMap;

/// Escape sequence bytes for entering alt-screen (smcup)
/// ESC [ ? 1049 h
const ALT_SCREEN_ENTER_1049: &[u8] = b"\x1b[?1049h";

/// Escape sequence bytes for leaving alt-screen (rmcup)
/// ESC [ ? 1049 l
const ALT_SCREEN_LEAVE_1049: &[u8] = b"\x1b[?1049l";

/// Alternative escape sequence for entering alt-screen (older xterm)
/// ESC [ ? 47 h
const ALT_SCREEN_ENTER_47: &[u8] = b"\x1b[?47h";

/// Alternative escape sequence for leaving alt-screen (older xterm)
/// ESC [ ? 47 l
const ALT_SCREEN_LEAVE_47: &[u8] = b"\x1b[?47l";

/// Maximum bytes to retain in tail buffer for handling sequences split
/// across capture boundaries. ESC sequences are short, 16 bytes is plenty.
const TAIL_BUFFER_SIZE: usize = 16;

/// State tracked per pane
#[derive(Debug, Default)]
struct PaneScreenState {
    /// Whether alternate screen buffer is active
    alt_screen_active: bool,
    /// Tail buffer for handling sequences split across captures
    tail_buffer: Vec<u8>,
}

/// Tracks terminal screen state by parsing escape sequences.
///
/// This provides alt-screen detection without requiring Lua hooks,
/// eliminating the performance bottleneck of WezTerm's `update-status` event.
#[derive(Debug, Default)]
pub struct ScreenStateTracker {
    /// Per-pane state
    pane_states: HashMap<u64, PaneScreenState>,
}

impl ScreenStateTracker {
    /// Create a new screen state tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Process captured terminal output and update screen state.
    ///
    /// This scans the output for alt-screen enter/leave escape sequences
    /// and updates the tracked state accordingly.
    ///
    /// # Arguments
    ///
    /// * `pane_id` - The WezTerm pane ID
    /// * `output` - Raw bytes of captured terminal output
    pub fn process_output(&mut self, pane_id: u64, output: &[u8]) {
        if output.is_empty() {
            return;
        }

        let state = self.pane_states.entry(pane_id).or_default();

        // Combine tail buffer with new output to handle split sequences
        let search_buf: Vec<u8> = if state.tail_buffer.is_empty() {
            output.to_vec()
        } else {
            let mut combined = std::mem::take(&mut state.tail_buffer);
            combined.extend_from_slice(output);
            combined
        };

        // Process all escape sequences in order
        state.alt_screen_active =
            Self::detect_final_alt_screen_state(&search_buf, state.alt_screen_active);

        // Save tail for next capture (in case sequence is split)
        let tail_start = search_buf.len().saturating_sub(TAIL_BUFFER_SIZE);
        state.tail_buffer = search_buf[tail_start..].to_vec();
    }

    /// Detect the final alt-screen state after processing all sequences in the buffer.
    ///
    /// This finds all enter/leave sequences and returns the state after the last one.
    fn detect_final_alt_screen_state(buf: &[u8], current_state: bool) -> bool {
        let mut result = current_state;
        let mut pos = 0;

        while pos < buf.len() {
            // Find next ESC character
            let Some(esc_pos) = memchr::memchr(0x1b, &buf[pos..]) else {
                break;
            };
            let abs_pos = pos + esc_pos;

            // Check for alt-screen sequences at this position
            let remaining = &buf[abs_pos..];

            if remaining.starts_with(ALT_SCREEN_ENTER_1049) {
                result = true;
                pos = abs_pos + ALT_SCREEN_ENTER_1049.len();
            } else if remaining.starts_with(ALT_SCREEN_LEAVE_1049) {
                result = false;
                pos = abs_pos + ALT_SCREEN_LEAVE_1049.len();
            } else if remaining.starts_with(ALT_SCREEN_ENTER_47) {
                result = true;
                pos = abs_pos + ALT_SCREEN_ENTER_47.len();
            } else if remaining.starts_with(ALT_SCREEN_LEAVE_47) {
                result = false;
                pos = abs_pos + ALT_SCREEN_LEAVE_47.len();
            } else {
                // Not a recognized sequence, skip past this ESC
                pos = abs_pos + 1;
            }
        }

        result
    }

    /// Query whether a pane is currently in alt-screen mode.
    ///
    /// Returns `false` if the pane has not been seen or has no recorded state.
    #[must_use]
    pub fn is_alt_screen(&self, pane_id: u64) -> bool {
        self.pane_states
            .get(&pane_id)
            .is_some_and(|s| s.alt_screen_active)
    }

    /// Set the alt-screen state for a pane directly.
    ///
    /// This is useful for initializing state from external sources or tests.
    pub fn set_alt_screen(&mut self, pane_id: u64, active: bool) {
        self.pane_states
            .entry(pane_id)
            .or_default()
            .alt_screen_active = active;
    }

    /// Clear all tracked state for a pane.
    ///
    /// Call this when a pane is destroyed.
    pub fn clear_pane(&mut self, pane_id: u64) {
        self.pane_states.remove(&pane_id);
    }

    /// Reset the detection context for a pane.
    ///
    /// This clears the tail buffer, useful when the capture stream is reset.
    pub fn reset_context(&mut self, pane_id: u64) {
        if let Some(state) = self.pane_states.get_mut(&pane_id) {
            state.tail_buffer.clear();
        }
    }

    /// Get all pane IDs being tracked.
    #[must_use]
    pub fn tracked_panes(&self) -> Vec<u64> {
        self.pane_states.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_tracker_default_state() {
        let tracker = ScreenStateTracker::new();
        assert!(!tracker.is_alt_screen(0));
        assert!(!tracker.is_alt_screen(999));
    }

    #[test]
    fn test_enter_alt_screen_1049() {
        let mut tracker = ScreenStateTracker::new();
        assert!(!tracker.is_alt_screen(1));

        // Enter alt-screen with ESC[?1049h
        tracker.process_output(1, b"\x1b[?1049h");
        assert!(tracker.is_alt_screen(1));
    }

    #[test]
    fn test_leave_alt_screen_1049() {
        let mut tracker = ScreenStateTracker::new();

        // Enter then leave
        tracker.process_output(1, b"\x1b[?1049h");
        assert!(tracker.is_alt_screen(1));

        tracker.process_output(1, b"\x1b[?1049l");
        assert!(!tracker.is_alt_screen(1));
    }

    #[test]
    fn test_enter_alt_screen_47() {
        let mut tracker = ScreenStateTracker::new();

        // Enter with older ESC[?47h sequence
        tracker.process_output(1, b"\x1b[?47h");
        assert!(tracker.is_alt_screen(1));

        // Leave with ESC[?47l
        tracker.process_output(1, b"\x1b[?47l");
        assert!(!tracker.is_alt_screen(1));
    }

    #[test]
    fn test_mixed_sequences() {
        let mut tracker = ScreenStateTracker::new();

        // Enter with 1049, leave with 47 (mixed)
        tracker.process_output(1, b"\x1b[?1049h");
        assert!(tracker.is_alt_screen(1));

        tracker.process_output(1, b"\x1b[?47l");
        assert!(!tracker.is_alt_screen(1));
    }

    #[test]
    fn test_sequence_in_middle_of_output() {
        let mut tracker = ScreenStateTracker::new();

        // Sequence embedded in normal output
        let output = b"Hello world\x1b[?1049hMore text after";
        tracker.process_output(1, output);
        assert!(tracker.is_alt_screen(1));
    }

    #[test]
    fn test_multiple_sequences_in_single_output() {
        let mut tracker = ScreenStateTracker::new();

        // Enter and leave in same output - final state should be "left"
        let output = b"\x1b[?1049hsome vim content\x1b[?1049l";
        tracker.process_output(1, output);
        assert!(!tracker.is_alt_screen(1));

        // Enter, leave, enter - final state should be "entered"
        let output2 = b"\x1b[?1049h\x1b[?1049l\x1b[?1049h";
        tracker.process_output(2, output2);
        assert!(tracker.is_alt_screen(2));
    }

    #[test]
    fn test_sequence_split_across_captures() {
        let mut tracker = ScreenStateTracker::new();

        // Split the sequence ESC[?1049h across two captures
        // First capture ends with ESC[?10
        tracker.process_output(1, b"normal text\x1b[?10");
        assert!(!tracker.is_alt_screen(1)); // Not yet detected

        // Second capture starts with 49h
        tracker.process_output(1, b"49hmore text");
        assert!(tracker.is_alt_screen(1)); // Now detected from combined buffer
    }

    #[test]
    fn test_multiple_panes_independent() {
        let mut tracker = ScreenStateTracker::new();

        tracker.process_output(1, b"\x1b[?1049h");
        tracker.process_output(2, b"normal output");

        assert!(tracker.is_alt_screen(1));
        assert!(!tracker.is_alt_screen(2));

        tracker.process_output(2, b"\x1b[?1049h");
        assert!(tracker.is_alt_screen(1));
        assert!(tracker.is_alt_screen(2));
    }

    #[test]
    fn test_clear_pane() {
        let mut tracker = ScreenStateTracker::new();

        tracker.process_output(1, b"\x1b[?1049h");
        assert!(tracker.is_alt_screen(1));

        tracker.clear_pane(1);
        assert!(!tracker.is_alt_screen(1)); // Back to default (false)
    }

    #[test]
    fn test_set_alt_screen_directly() {
        let mut tracker = ScreenStateTracker::new();

        tracker.set_alt_screen(1, true);
        assert!(tracker.is_alt_screen(1));

        tracker.set_alt_screen(1, false);
        assert!(!tracker.is_alt_screen(1));
    }

    #[test]
    fn test_empty_output() {
        let mut tracker = ScreenStateTracker::new();

        tracker.process_output(1, b"\x1b[?1049h");
        assert!(tracker.is_alt_screen(1));

        // Empty output shouldn't change state
        tracker.process_output(1, b"");
        assert!(tracker.is_alt_screen(1));
    }

    #[test]
    fn test_reset_context() {
        let mut tracker = ScreenStateTracker::new();

        // Start a split sequence
        tracker.process_output(1, b"text\x1b[?10");
        tracker.reset_context(1);

        // The split sequence should NOT be completed now
        tracker.process_output(1, b"49h");
        assert!(!tracker.is_alt_screen(1)); // "49h" alone doesn't match
    }

    #[test]
    fn test_tracked_panes() {
        let mut tracker = ScreenStateTracker::new();

        assert!(tracker.tracked_panes().is_empty());

        tracker.process_output(5, b"some output");
        tracker.process_output(10, b"other output");

        let panes = tracker.tracked_panes();
        assert_eq!(panes.len(), 2);
        assert!(panes.contains(&5));
        assert!(panes.contains(&10));
    }

    #[test]
    fn test_other_escape_sequences_ignored() {
        let mut tracker = ScreenStateTracker::new();

        // Various escape sequences that are NOT alt-screen
        let output = b"\x1b[2J\x1b[H\x1b[0m\x1b[32mgreen\x1b[0m";
        tracker.process_output(1, output);
        assert!(!tracker.is_alt_screen(1));
    }

    #[test]
    fn test_partial_sequence_not_matched() {
        let mut tracker = ScreenStateTracker::new();

        // ESC[?104 without the final 9h
        tracker.process_output(1, b"\x1b[?104");
        assert!(!tracker.is_alt_screen(1));

        // ESC[?1049 without h or l
        tracker.process_output(2, b"\x1b[?1049");
        assert!(!tracker.is_alt_screen(2));
    }

    // -----------------------------------------------------------------------
    // Rapid enter/leave cycling (resize storm simulation)
    // -----------------------------------------------------------------------

    #[test]
    fn rapid_alt_screen_cycling_settles_correctly() {
        let mut tracker = ScreenStateTracker::new();

        // 100 enter/leave cycles - simulates rapid vim open/close during resize storm.
        for _ in 0..100 {
            tracker.process_output(1, b"\x1b[?1049h");
            assert!(tracker.is_alt_screen(1));
            tracker.process_output(1, b"\x1b[?1049l");
            assert!(!tracker.is_alt_screen(1));
        }
    }

    #[test]
    fn many_enters_without_leave_stays_active() {
        let mut tracker = ScreenStateTracker::new();

        // Multiple redundant enters (some apps send smcup repeatedly).
        for _ in 0..50 {
            tracker.process_output(1, b"\x1b[?1049h");
        }
        assert!(tracker.is_alt_screen(1));

        // Single leave should deactivate.
        tracker.process_output(1, b"\x1b[?1049l");
        assert!(!tracker.is_alt_screen(1));
    }

    // -----------------------------------------------------------------------
    // Multi-pane stress
    // -----------------------------------------------------------------------

    #[test]
    fn hundred_panes_independent_state() {
        let mut tracker = ScreenStateTracker::new();

        // Even panes enter alt-screen, odd panes stay normal.
        for pane_id in 0..100u64 {
            if pane_id % 2 == 0 {
                tracker.process_output(pane_id, b"\x1b[?1049h");
            } else {
                tracker.process_output(pane_id, b"normal output");
            }
        }

        for pane_id in 0..100u64 {
            if pane_id % 2 == 0 {
                assert!(
                    tracker.is_alt_screen(pane_id),
                    "pane {pane_id} should be alt"
                );
            } else {
                assert!(
                    !tracker.is_alt_screen(pane_id),
                    "pane {pane_id} should not be alt"
                );
            }
        }

        assert_eq!(tracker.tracked_panes().len(), 100);
    }

    // -----------------------------------------------------------------------
    // Sequence boundary splitting edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn split_at_every_byte_boundary_of_1049h() {
        // ESC [ ? 1 0 4 9 h  = 8 bytes
        let full = b"\x1b[?1049h";
        for split_at in 1..full.len() {
            let mut tracker = ScreenStateTracker::new();
            tracker.process_output(1, &full[..split_at]);
            tracker.process_output(1, &full[split_at..]);
            assert!(
                tracker.is_alt_screen(1),
                "split at byte {split_at} should still detect alt-screen"
            );
        }
    }

    #[test]
    fn split_at_every_byte_boundary_of_1049l() {
        let full = b"\x1b[?1049l";
        for split_at in 1..full.len() {
            let mut tracker = ScreenStateTracker::new();
            // First enter alt-screen.
            tracker.process_output(1, b"\x1b[?1049h");
            assert!(tracker.is_alt_screen(1));
            // Then leave via split sequence.
            tracker.process_output(1, &full[..split_at]);
            tracker.process_output(1, &full[split_at..]);
            assert!(
                !tracker.is_alt_screen(1),
                "split at byte {split_at} should detect leave"
            );
        }
    }

    #[test]
    fn split_at_every_byte_boundary_of_47h() {
        let full = b"\x1b[?47h";
        for split_at in 1..full.len() {
            let mut tracker = ScreenStateTracker::new();
            tracker.process_output(1, &full[..split_at]);
            tracker.process_output(1, &full[split_at..]);
            assert!(
                tracker.is_alt_screen(1),
                "47h split at byte {split_at} should detect alt-screen"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Large output with embedded sequences
    // -----------------------------------------------------------------------

    #[test]
    fn large_output_with_embedded_enter() {
        let mut tracker = ScreenStateTracker::new();
        let mut data = vec![b'A'; 65536];
        // Place enter sequence at offset 32768.
        let seq = b"\x1b[?1049h";
        data[32768..32768 + seq.len()].copy_from_slice(seq);
        tracker.process_output(1, &data);
        assert!(tracker.is_alt_screen(1));
    }

    #[test]
    fn large_output_with_enter_and_leave() {
        let mut tracker = ScreenStateTracker::new();
        let mut data = vec![b'X'; 65536];
        // Enter near the start.
        let enter = b"\x1b[?1049h";
        data[100..100 + enter.len()].copy_from_slice(enter);
        // Leave near the end.
        let leave = b"\x1b[?1049l";
        data[65000..65000 + leave.len()].copy_from_slice(leave);
        tracker.process_output(1, &data);
        // Last sequence is leave, so should not be alt-screen.
        assert!(!tracker.is_alt_screen(1));
    }

    // -----------------------------------------------------------------------
    // Clear and re-track
    // -----------------------------------------------------------------------

    #[test]
    fn clear_pane_removes_from_tracked_list() {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(1, b"data");
        tracker.process_output(2, b"data");
        assert_eq!(tracker.tracked_panes().len(), 2);

        tracker.clear_pane(1);
        assert_eq!(tracker.tracked_panes().len(), 1);
        assert!(!tracker.tracked_panes().contains(&1));
        assert!(tracker.tracked_panes().contains(&2));
    }

    #[test]
    fn clear_nonexistent_pane_is_safe() {
        let mut tracker = ScreenStateTracker::new();
        // Clearing a pane that was never tracked should not panic.
        tracker.clear_pane(999);
        assert!(tracker.tracked_panes().is_empty());
    }

    #[test]
    fn reset_context_preserves_alt_screen_state() {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(1, b"\x1b[?1049h");
        assert!(tracker.is_alt_screen(1));

        // Reset context should clear tail buffer but NOT change alt-screen state.
        tracker.reset_context(1);
        assert!(tracker.is_alt_screen(1));
    }

    #[test]
    fn reset_context_on_nonexistent_pane_is_safe() {
        let mut tracker = ScreenStateTracker::new();
        // Should not panic.
        tracker.reset_context(42);
    }

    // -----------------------------------------------------------------------
    // set_alt_screen creates pane entry if needed
    // -----------------------------------------------------------------------

    #[test]
    fn set_alt_screen_creates_pane_entry() {
        let mut tracker = ScreenStateTracker::new();
        assert!(tracker.tracked_panes().is_empty());

        tracker.set_alt_screen(5, true);
        assert!(tracker.tracked_panes().contains(&5));
        assert!(tracker.is_alt_screen(5));
    }
}
