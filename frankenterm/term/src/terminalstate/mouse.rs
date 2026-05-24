use crate::TerminalState;
use crate::input::*;
use crate::terminalstate::MouseEncoding;
use anyhow::bail;
use std::io::Write;

fn sgr_pixel_coord(cell: usize, total_pixels: usize, cells: usize, pixel_offset: isize) -> usize {
    let cell_pixels = total_pixels / cells.max(1);
    cell.saturating_mul(cell_pixels)
        .saturating_add(pixel_offset.max(0) as usize)
        .saturating_add(1)
}

fn sgr_cell_coords(event: &MouseEvent) -> (usize, i64) {
    (event.x.saturating_add(1), event.y.saturating_add(1))
}

fn x10_encoded_coord_value(value: i64) -> Option<u32> {
    value
        .checked_add(1)
        .and_then(|value| value.checked_add(32))
        .and_then(|value| u32::try_from(value).ok())
}

fn clamp_mouse_coords(x: usize, y: i64, cols: usize, rows: usize) -> (usize, i64) {
    let x = match cols.checked_sub(1) {
        Some(max_x) => x.min(max_x),
        None => 0,
    };
    let y = match rows
        .checked_sub(1)
        .and_then(|max_y| i64::try_from(max_y).ok())
    {
        Some(max_y) => y.clamp(0, max_y),
        None => 0,
    };
    (x, y)
}

impl TerminalState {
    fn sgr_pixel_coords(&self, event: MouseEvent) -> (usize, usize) {
        (
            sgr_pixel_coord(
                event.x,
                self.pixel_width,
                self.screen.physical_cols,
                event.x_pixel_offset,
            ),
            sgr_pixel_coord(
                event.y.max(0) as usize,
                self.pixel_height,
                self.screen.physical_rows,
                event.y_pixel_offset,
            ),
        )
    }

    /// Encode a coordinate value using X10 encoding or Utf8 encoding.
    /// Out of bounds coords are reported as the 0 byte value.
    fn encode_coord(&self, value: i64, dest: &mut Vec<u8>) {
        // Convert to 1-based and offset into the printable character range
        let Some(value) = x10_encoded_coord_value(value) else {
            dest.push(0);
            return;
        };
        if self.mouse_encoding == MouseEncoding::Utf8 {
            if value < 0x800 {
                let mut utf8 = [0; 2];
                if let Some(ch) = char::from_u32(value) {
                    dest.extend_from_slice(ch.encode_utf8(&mut utf8).as_bytes());
                } else {
                    dest.push(0);
                }
            } else {
                // out of range
                dest.push(0);
            }
        } else if let Ok(value) = u8::try_from(value) {
            dest.push(value);
        } else {
            // out of range
            dest.push(0);
        }
    }

    fn encode_x10_or_utf8(&mut self, event: MouseEvent, button: i8) -> anyhow::Result<()> {
        let mut buf = vec![b'\x1b', b'[', b'M', (32 + button) as u8];
        self.encode_coord(event.x as i64, &mut buf);
        self.encode_coord(event.y, &mut buf);
        log::trace!("{event:?} {buf:?}");
        self.writer.write(&buf)?;
        self.writer.flush()?;
        Ok(())
    }

    fn mouse_report_button_number(&self, event: &MouseEvent) -> (i8, MouseButton) {
        let button = match event.button {
            MouseButton::None => self
                .current_mouse_buttons
                .last()
                .copied()
                .unwrap_or(MouseButton::None),
            b => b,
        };
        let mut code = match button {
            MouseButton::None => 3,
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::WheelUp(_) => 64,
            MouseButton::WheelDown(_) => 65,
            MouseButton::WheelLeft(_) => 66,
            MouseButton::WheelRight(_) => 67,
        };

        if event.modifiers.contains(KeyModifiers::SHIFT) {
            code += 4;
        }
        if event.modifiers.contains(KeyModifiers::ALT) {
            code += 8;
        }
        if event.modifiers.contains(KeyModifiers::CTRL) {
            code += 16;
        }

        (code, button)
    }

    fn mouse_wheel(&mut self, event: MouseEvent) -> anyhow::Result<()> {
        let (button, _button) = self.mouse_report_button_number(&event);

        if self.mouse_encoding == MouseEncoding::SGR
            && (self.mouse_tracking || self.button_event_mouse || self.any_event_mouse)
        {
            let (x, y) = sgr_cell_coords(&event);
            log::trace!("wheel {event:?} ESC [<{};{};{}M", button, x, y);
            write!(self.writer, "\x1b[<{};{};{}M", button, x, y)?;
            self.writer.flush()?;
        } else if self.mouse_encoding == MouseEncoding::SgrPixels
            && (self.mouse_tracking || self.button_event_mouse || self.any_event_mouse)
        {
            let (x, y) = self.sgr_pixel_coords(event);
            log::trace!("wheel {event:?} ESC [<{};{};{}M", button, x, y);
            write!(self.writer, "\x1b[<{};{};{}M", button, x, y)?;
            self.writer.flush()?;
        } else if self.mouse_tracking || self.button_event_mouse || self.any_event_mouse {
            self.encode_x10_or_utf8(event, button)?;
        } else if self.screen.is_alt_screen_active() {
            // Send cursor keys instead (equivalent to xterm's alternateScroll mode)
            for _ in 0..self.config.alternate_buffer_wheel_scroll_speed() {
                self.key_down(
                    match event.button {
                        MouseButton::WheelDown(_) => KeyCode::DownArrow,
                        MouseButton::WheelUp(_) => KeyCode::UpArrow,
                        MouseButton::WheelLeft(_) => KeyCode::LeftArrow,
                        MouseButton::WheelRight(_) => KeyCode::RightArrow,
                        _ => bail!("unexpected mouse event"),
                    },
                    KeyModifiers::default(),
                )?;
            }
        }
        Ok(())
    }

    fn mouse_button_press(&mut self, event: MouseEvent) -> anyhow::Result<()> {
        let (button, event_button) = self.mouse_report_button_number(&event);
        self.current_mouse_buttons.retain(|&b| b != event_button);
        self.current_mouse_buttons.push(event_button);

        if !(self.mouse_tracking || self.button_event_mouse || self.any_event_mouse) {
            return Ok(());
        }

        if self.mouse_encoding == MouseEncoding::SGR {
            let (x, y) = sgr_cell_coords(&event);
            log::trace!("press {event:?} ESC [<{};{};{}M", button, x, y);
            write!(self.writer, "\x1b[<{};{};{}M", button, x, y)?;
            self.writer.flush()?;
        } else if self.mouse_encoding == MouseEncoding::SgrPixels {
            let (x, y) = self.sgr_pixel_coords(event);
            log::trace!("press {event:?} ESC [<{};{};{}M", button, x, y);
            write!(self.writer, "\x1b[<{};{};{}M", button, x, y)?;
            self.writer.flush()?;
        } else {
            self.encode_x10_or_utf8(event, button)?;
        }

        Ok(())
    }

    fn mouse_button_release(&mut self, event: MouseEvent) -> anyhow::Result<()> {
        let (release_button, button) = self.mouse_report_button_number(&event);
        if !self.current_mouse_buttons.is_empty() {
            self.current_mouse_buttons.retain(|&b| b != button);
            if self.mouse_tracking || self.button_event_mouse || self.any_event_mouse {
                if self.mouse_encoding == MouseEncoding::SGR {
                    let (x, y) = sgr_cell_coords(&event);
                    log::trace!("release {event:?} ESC [<{};{};{}m", release_button, x, y);
                    write!(self.writer, "\x1b[<{};{};{}m", release_button, x, y)?;
                    self.writer.flush()?;
                } else if self.mouse_encoding == MouseEncoding::SgrPixels {
                    let (x, y) = self.sgr_pixel_coords(event);
                    log::trace!("release {event:?} ESC [<{};{};{}m", release_button, x, y);
                    write!(self.writer, "\x1b[<{};{};{}m", release_button, x, y)?;
                    self.writer.flush()?;
                } else {
                    let release_button = 3;
                    self.encode_x10_or_utf8(event, release_button)?;
                }
            }
        }

        Ok(())
    }

    fn mouse_move(&mut self, event: MouseEvent) -> anyhow::Result<()> {
        let moved = match (&self.last_mouse_move, self.mouse_encoding) {
            (None, _) => true,
            (Some(last), MouseEncoding::SgrPixels) => {
                last.x != event.x
                    || last.y != event.y
                    || last.x_pixel_offset != event.x_pixel_offset
                    || last.y_pixel_offset != event.y_pixel_offset
            }
            (Some(last), _) => last.x != event.x || last.y != event.y,
        };

        let reportable = (self.any_event_mouse || !self.current_mouse_buttons.is_empty()) && moved;
        // Note: self.mouse_tracking on its own is for clicks, not drags!
        if reportable && (self.button_event_mouse || self.any_event_mouse) {
            match self.last_mouse_move.as_ref() {
                Some(last) if *last == event => {
                    return Ok(());
                }
                _ => {}
            }
            self.last_mouse_move.replace(event);

            let (button, _button) = self.mouse_report_button_number(&event);
            let button = 32 + button;

            if self.mouse_encoding == MouseEncoding::SGR {
                let (x, y) = sgr_cell_coords(&event);
                log::trace!("move {event:?} ESC [<{};{};{}M", button, x, y);
                write!(self.writer, "\x1b[<{};{};{}M", button, x, y)?;
                self.writer.flush()?;
            } else if self.mouse_encoding == MouseEncoding::SgrPixels {
                let (x, y) = self.sgr_pixel_coords(event);
                log::trace!("move {event:?} ESC [<{};{};{}M", button, x, y);
                write!(self.writer, "\x1b[<{};{};{}M", button, x, y)?;
                self.writer.flush()?;
            } else {
                self.encode_x10_or_utf8(event, button)?;
            }
        }
        Ok(())
    }

    /// Informs the terminal of a mouse event.
    /// If mouse reporting has been activated, the mouse event will be encoded
    /// appropriately and written to the associated writer.
    pub fn mouse_event(&mut self, mut event: MouseEvent) -> anyhow::Result<()> {
        // Clamp the mouse coordinates to the size of the model.
        // This situation can trigger for example when the
        // window is resized and leaves a partial row at the bottom of the
        // terminal.  The mouse can move over that portion and the gui layer
        // can thus send us out-of-bounds row or column numbers.  We want to
        // make sure that we clamp this and handle it nicely at the model layer.
        let (x, y) = clamp_mouse_coords(
            event.x,
            event.y,
            self.screen().physical_cols,
            self.screen().physical_rows,
        );
        event.x = x;
        event.y = y;

        match event {
            MouseEvent {
                kind: MouseEventKind::Press,
                button:
                    MouseButton::WheelUp(_)
                    | MouseButton::WheelDown(_)
                    | MouseButton::WheelLeft(_)
                    | MouseButton::WheelRight(_),
                ..
            } => self.mouse_wheel(event),
            MouseEvent {
                kind: MouseEventKind::Press | MouseEventKind::Release,
                button: MouseButton::None,
                ..
            } => {
                // Horizontal wheel not plumbed to anything useful
                Ok(())
            }
            MouseEvent {
                kind: MouseEventKind::Press,
                ..
            } => self.mouse_button_press(event),
            MouseEvent {
                kind: MouseEventKind::Release,
                ..
            } => self.mouse_button_release(event),
            MouseEvent {
                kind: MouseEventKind::Move,
                ..
            } => self.mouse_move(event),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{clamp_mouse_coords, sgr_cell_coords, sgr_pixel_coord, x10_encoded_coord_value};
    use crate::input::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    #[test]
    fn sgr_pixel_coord_matches_existing_one_based_formula() {
        assert_eq!(sgr_pixel_coord(4, 80, 10, 3), 36);
        assert_eq!(sgr_pixel_coord(4, 80, 10, -3), 33);
    }

    #[test]
    fn sgr_pixel_coord_saturates_extreme_values() {
        assert_eq!(
            sgr_pixel_coord(usize::MAX, usize::MAX, 1, isize::MAX),
            usize::MAX
        );
        assert_eq!(sgr_pixel_coord(2, 9, 0, 0), 19);
    }

    #[test]
    fn sgr_cell_coords_saturate_one_based_conversion() {
        let event = MouseEvent {
            x: usize::MAX,
            y: i64::MAX,
            x_pixel_offset: 0,
            y_pixel_offset: 0,
            button: MouseButton::None,
            modifiers: KeyModifiers::NONE,
            kind: MouseEventKind::Move,
        };

        assert_eq!(sgr_cell_coords(&event), (usize::MAX, i64::MAX));
    }

    #[test]
    fn x10_coord_offset_rejects_overflow_and_negative_values() {
        assert_eq!(x10_encoded_coord_value(0), Some(33));
        assert_eq!(x10_encoded_coord_value(-40), None);
        assert_eq!(x10_encoded_coord_value(i64::MAX), None);
    }

    #[test]
    fn clamp_mouse_coords_handles_zero_sized_screens() {
        assert_eq!(clamp_mouse_coords(12, 34, 0, 0), (0, 0));
        assert_eq!(clamp_mouse_coords(12, -5, 10, 4), (9, 0));
        assert_eq!(clamp_mouse_coords(12, 9, 10, 4), (9, 3));
    }
}
