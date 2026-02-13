// clippy hates bitflags
#![allow(clippy::suspicious_arithmetic_impl, clippy::redundant_field_names)]

use super::VisibleRowIndex;
use frankenterm_dynamic::{FromDynamic, ToDynamic};
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub use termwiz::input::{KeyCode, Modifiers as KeyModifiers};

#[cfg_attr(feature = "use_serde", derive(Deserialize, Serialize))]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd, Hash, FromDynamic, ToDynamic)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp(usize),
    WheelDown(usize),
    WheelLeft(usize),
    WheelRight(usize),
    None,
}

#[cfg_attr(feature = "use_serde", derive(Deserialize, Serialize))]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum MouseEventKind {
    Press,
    Release,
    Move,
}

#[cfg_attr(feature = "use_serde", derive(Deserialize, Serialize))]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct MouseEvent {
    pub kind: MouseEventKind,
    pub x: usize,
    pub y: VisibleRowIndex,
    pub x_pixel_offset: isize,
    pub y_pixel_offset: isize,
    pub button: MouseButton,
    pub modifiers: KeyModifiers,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ClickPosition {
    pub column: usize,
    pub row: i64,
    pub x_pixel_offset: isize,
    pub y_pixel_offset: isize,
}

/// This is a little helper that keeps track of the "click streak",
/// which is the number of successive clicks of the same mouse button
/// within the `CLICK_INTERVAL`.  The streak is reset to 1 each time
/// the mouse button differs from the last click, or when the elapsed
/// time exceeds `CLICK_INTERVAL`, or when the cursor position
/// changes to a different character cell.
#[derive(Debug, Clone)]
pub struct LastMouseClick {
    pub button: MouseButton,
    pub position: ClickPosition,
    time: Instant,
    pub streak: usize,
}

/// The multi-click interval, measured in milliseconds
const CLICK_INTERVAL: u64 = 500;

impl LastMouseClick {
    pub fn new(button: MouseButton, position: ClickPosition) -> Self {
        Self {
            button,
            position,
            time: Instant::now(),
            streak: 1,
        }
    }

    pub fn add(&self, button: MouseButton, position: ClickPosition) -> Self {
        let now = Instant::now();
        let streak = if button == self.button
            && position.column == self.position.column
            && position.row == self.position.row
            && now.duration_since(self.time) <= Duration::from_millis(CLICK_INTERVAL)
        {
            self.streak + 1
        } else {
            1
        };
        Self {
            button,
            position,
            time: now,
            streak,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(col: usize, row: i64) -> ClickPosition {
        ClickPosition {
            column: col,
            row,
            x_pixel_offset: 0,
            y_pixel_offset: 0,
        }
    }

    // ── MouseButton ─────────────────────────────────────────

    #[test]
    fn mouse_button_eq() {
        assert_eq!(MouseButton::Left, MouseButton::Left);
        assert_ne!(MouseButton::Left, MouseButton::Right);
        assert_ne!(MouseButton::Left, MouseButton::Middle);
    }

    #[test]
    fn mouse_button_wheel_variants() {
        assert_eq!(MouseButton::WheelUp(1), MouseButton::WheelUp(1));
        assert_ne!(MouseButton::WheelUp(1), MouseButton::WheelUp(2));
        assert_ne!(MouseButton::WheelUp(1), MouseButton::WheelDown(1));
    }

    #[test]
    fn mouse_button_clone_copy() {
        let b = MouseButton::Left;
        let c = b;
        assert_eq!(b, c);
    }

    #[test]
    fn mouse_button_debug() {
        let debug = format!("{:?}", MouseButton::Left);
        assert_eq!(debug, "Left");
    }

    #[test]
    fn mouse_button_ord() {
        // Just verify it doesn't panic
        let _ = MouseButton::Left.cmp(&MouseButton::Right);
    }

    #[test]
    fn mouse_button_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(MouseButton::Left);
        set.insert(MouseButton::Right);
        set.insert(MouseButton::Left); // duplicate
        assert_eq!(set.len(), 2);
    }

    // ── MouseEventKind ──────────────────────────────────────

    #[test]
    fn mouse_event_kind_eq() {
        assert_eq!(MouseEventKind::Press, MouseEventKind::Press);
        assert_ne!(MouseEventKind::Press, MouseEventKind::Release);
        assert_ne!(MouseEventKind::Release, MouseEventKind::Move);
    }

    #[test]
    fn mouse_event_kind_clone_copy() {
        let k = MouseEventKind::Move;
        let k2 = k;
        assert_eq!(k, k2);
    }

    // ── MouseEvent ──────────────────────────────────────────

    #[test]
    fn mouse_event_construction() {
        let evt = MouseEvent {
            kind: MouseEventKind::Press,
            x: 10,
            y: 5,
            x_pixel_offset: 2,
            y_pixel_offset: 3,
            button: MouseButton::Left,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(evt.kind, MouseEventKind::Press);
        assert_eq!(evt.x, 10);
        assert_eq!(evt.y, 5);
        assert_eq!(evt.button, MouseButton::Left);
    }

    #[test]
    fn mouse_event_eq() {
        let evt1 = MouseEvent {
            kind: MouseEventKind::Press,
            x: 0,
            y: 0,
            x_pixel_offset: 0,
            y_pixel_offset: 0,
            button: MouseButton::Left,
            modifiers: KeyModifiers::NONE,
        };
        let evt2 = evt1;
        assert_eq!(evt1, evt2);
    }

    // ── ClickPosition ───────────────────────────────────────

    #[test]
    fn click_position_eq() {
        let a = pos(5, 10);
        let b = pos(5, 10);
        assert_eq!(a, b);
    }

    #[test]
    fn click_position_ne() {
        assert_ne!(pos(5, 10), pos(6, 10));
        assert_ne!(pos(5, 10), pos(5, 11));
    }

    // ── LastMouseClick ──────────────────────────────────────

    #[test]
    fn new_click_has_streak_1() {
        let click = LastMouseClick::new(MouseButton::Left, pos(0, 0));
        assert_eq!(click.streak, 1);
        assert_eq!(click.button, MouseButton::Left);
    }

    #[test]
    fn same_button_same_position_increments_streak() {
        let click1 = LastMouseClick::new(MouseButton::Left, pos(5, 3));
        let click2 = click1.add(MouseButton::Left, pos(5, 3));
        assert_eq!(click2.streak, 2);
        let click3 = click2.add(MouseButton::Left, pos(5, 3));
        assert_eq!(click3.streak, 3);
    }

    #[test]
    fn different_button_resets_streak() {
        let click1 = LastMouseClick::new(MouseButton::Left, pos(5, 3));
        let click2 = click1.add(MouseButton::Right, pos(5, 3));
        assert_eq!(click2.streak, 1);
    }

    #[test]
    fn different_position_resets_streak() {
        let click1 = LastMouseClick::new(MouseButton::Left, pos(5, 3));
        let click2 = click1.add(MouseButton::Left, pos(6, 3));
        assert_eq!(click2.streak, 1);
    }

    #[test]
    fn different_row_resets_streak() {
        let click1 = LastMouseClick::new(MouseButton::Left, pos(5, 3));
        let click2 = click1.add(MouseButton::Left, pos(5, 4));
        assert_eq!(click2.streak, 1);
    }

    #[test]
    fn click_preserves_position_and_button() {
        let click = LastMouseClick::new(MouseButton::Middle, pos(10, 20));
        assert_eq!(click.position.column, 10);
        assert_eq!(click.position.row, 20);
        assert_eq!(click.button, MouseButton::Middle);
    }
}
