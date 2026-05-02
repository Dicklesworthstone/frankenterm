use mux::renderable::StableCursorPosition;
use std::time::Instant;

#[derive(Clone)]
pub struct PrevCursorPos {
    pos: StableCursorPosition,
    when: Instant,
}

impl PrevCursorPos {
    pub fn new() -> Self {
        PrevCursorPos {
            pos: StableCursorPosition::default(),
            when: Instant::now(),
        }
    }

    /// Make the cursor look like it moved
    pub fn bump(&mut self) {
        self.when = Instant::now();
    }

    /// Update the cursor position if it's different.
    ///
    /// Returns the previous position when the cursor moved so callers
    /// can invalidate both old and new cursor rows.
    pub fn update(&mut self, newpos: &StableCursorPosition) -> Option<StableCursorPosition> {
        if &self.pos != newpos {
            let previous = self.pos;
            self.pos = *newpos;
            self.when = Instant::now();
            Some(previous)
        } else {
            None
        }
    }

    /// When did the cursor last move?
    pub fn last_cursor_movement(&self) -> Instant {
        self.when
    }
}
