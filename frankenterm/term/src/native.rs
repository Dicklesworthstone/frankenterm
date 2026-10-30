//! A synchronous, caller-driven native direct-SSH terminal.
//!
//! This owns the canonical parser/model, not a transport or input queue. Input
//! encoding is read-only and parser replies are transferred whole to the one
//! downstream egress owner. All calls belong to one embedding serialization
//! domain. No task, writer thread, clipboard, notification, filesystem spill,
//! image decoder or alternate ANSI implementation is installed.

use crate::color::{ColorPalette, SrgbaTuple};
use crate::config::{GridEngine, NewlineCanon, Osc52WritePolicy, TerminalFeatureProfile};
use crate::input::{MouseButton, MouseEvent, MouseEventKind};
use crate::render::{
    RenderFrameError, RenderFrameLimits, RenderFrameWitness, capture_terminal_frame,
};
use crate::{
    BoundedOutputLimits, BoundedReplyCopy, BoundedReplyDisposition, KeyCode, KeyModifiers,
    StableRowIndex, Terminal, TerminalConfiguration, TerminalSize,
};
use std::convert::TryFrom;
use std::io;
use std::sync::Arc;

pub const MAX_FEED_BYTES: usize = 16 * 1024;
pub const MAX_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_REPLY_BYTES: usize = 16 * 1024;
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RESIDENT_CELLS: usize = 65536;
pub const MAX_CELL_GRAPHEME_BYTES: usize = 256;
pub const MAX_MOUSE_STEPS: i32 = 32;
/// RGBA8 entries of a base palette: 256 indexed colors, then foreground,
/// background, cursor foreground/background/border, selection fg/bg.
pub const BASE_PALETTE_COLORS: usize = 263;
pub const BASE_PALETTE_BYTES: usize = BASE_PALETTE_COLORS * 4;

fn decode_native_palette(rgba: &[u8]) -> Result<ColorPalette, NativeTerminalError> {
    if rgba.len() != BASE_PALETTE_BYTES {
        return Err(NativeTerminalError::Invalid);
    }
    let color = |index: usize| {
        let [red, green, blue, alpha] =
            [0, 1, 2, 3].map(|c| f32::from(rgba[index * 4 + c]) / 255.0);
        SrgbaTuple(red, green, blue, alpha)
    };
    let mut palette = ColorPalette::default();
    for (index, entry) in palette.colors.0.iter_mut().enumerate() {
        *entry = color(index);
    }
    palette.foreground = color(256);
    palette.background = color(257);
    palette.cursor_fg = color(258);
    palette.cursor_bg = color(259);
    palette.cursor_border = color(260);
    palette.selection_fg = color(261);
    palette.selection_bg = color(262);
    Ok(palette)
}

/// Finite, content-free native boundary failures. The numeric values are also
/// the version 1 C ABI status values; no parser or terminal bytes enter errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum NativeTerminalError {
    Invalid = 1,
    Stale = 2,
    Closed = 3,
    Capacity = 4,
    Busy = 5,
    Unsupported = 6,
    Panic = 7,
    SequenceExhausted = 8,
    ViewportUnavailable = 9,
}

impl std::fmt::Display for NativeTerminalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "native terminal {:?}", self)
    }
}

impl std::error::Error for NativeTerminalError {}

impl From<io::Error> for NativeTerminalError {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::OutOfMemory => Self::Capacity,
            io::ErrorKind::BrokenPipe => Self::Closed,
            io::ErrorKind::Unsupported => Self::Unsupported,
            _ => Self::Invalid,
        }
    }
}

impl From<RenderFrameError> for NativeTerminalError {
    fn from(error: RenderFrameError) -> Self {
        match error {
            RenderFrameError::Capacity | RenderFrameError::Allocation => Self::Capacity,
            RenderFrameError::SequenceExhausted => Self::SequenceExhausted,
            RenderFrameError::UnresidentViewport => Self::ViewportUnavailable,
            RenderFrameError::UnsupportedStorage
            | RenderFrameError::UnsupportedImage
            | RenderFrameError::UnsupportedLineMode
            | RenderFrameError::UnsupportedAttributes => Self::Unsupported,
            _ => Self::Invalid,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct NativeTerminalConfig {
    pub size: TerminalSize,
    pub scrollback_rows: usize,
    pub channel_id: [u8; 16],
    pub transport_id: [u8; 16],
    pub route_revision: u64,
    pub font_revision: u64,
}

impl NativeTerminalConfig {
    pub fn validate(&self) -> Result<(), NativeTerminalError> {
        self.retained_scrollback_rows()?;
        if self.channel_id == [0; 16]
            || self.transport_id == [0; 16]
            || self.route_revision == 0
            || self.font_revision == 0
        {
            return Err(NativeTerminalError::Invalid);
        }
        Ok(())
    }

    /// `scrollback_rows` is the most history kept. Both screens plus the
    /// history must fit [`MAX_RESIDENT_CELLS`], so a larger grid keeps what
    /// fits beside them; only a grid whose two screens alone exceed the
    /// budget is refused.
    pub fn retained_scrollback_rows(&self) -> Result<usize, NativeTerminalError> {
        let size = self.size;
        if !(1..=512).contains(&size.cols)
            || !(1..=512).contains(&size.rows)
            || size.pixel_width > 65535
            || size.pixel_height > 65535
            || self.scrollback_rows > 4096
        {
            return Err(NativeTerminalError::Invalid);
        }
        let screens = 2 * size.rows * size.cols;
        if screens > MAX_RESIDENT_CELLS {
            return Err(NativeTerminalError::Capacity);
        }
        Ok(self
            .scrollback_rows
            .min((MAX_RESIDENT_CELLS - screens) / size.cols))
    }

    /// Cells kept resident: both screens and the retained history.
    pub fn resident_cell_budget(&self) -> Result<usize, NativeTerminalError> {
        let retained = self.retained_scrollback_rows()?;
        Ok((2 * self.size.rows + retained) * self.size.cols)
    }
}

#[derive(Debug)]
struct NativeConfiguration {
    scrollback_rows: usize,
    /// The machine's configured theme. Terminal OSC color changes override
    /// it transiently and their resets return to it.
    palette: ColorPalette,
}

impl TerminalConfiguration for NativeConfiguration {
    fn feature_profile(&self) -> TerminalFeatureProfile {
        TerminalFeatureProfile::NativeText
    }

    fn maximum_cell_grapheme_bytes(&self) -> usize {
        MAX_CELL_GRAPHEME_BYTES
    }

    fn enable_kitty_keyboard(&self) -> bool {
        true
    }

    fn color_palette(&self) -> ColorPalette {
        self.palette.clone()
    }

    fn scrollback_size(&self) -> usize {
        self.scrollback_rows
    }

    fn grid_engine(&self) -> GridEngine {
        GridEngine::Legacy
    }

    fn canonicalize_pasted_newlines(&self) -> NewlineCanon {
        NewlineCanon::CarriageReturn
    }

    fn osc52_write_policy(&self) -> Osc52WritePolicy {
        Osc52WritePolicy::Deny
    }

    fn max_user_vars(&self) -> usize {
        0
    }

    fn max_unicode_version_stack_depth(&self) -> usize {
        16
    }

    fn max_accumulating_title_len(&self) -> usize {
        1024
    }
}

/// Pure keyboard phases. Repeat uses the canonical terminal's down-event
/// semantics. A complete press includes its canonical release where the
/// negotiated protocol reports releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeKeyPhase {
    Down,
    Repeated,
    Up,
    Press,
}

/// Complete, finite pointer effects. A click includes both transitions, so
/// local admission cannot accept a press while refusing its release. Drags
/// and held buttons require a separate ownership contract and are not exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeMouseAction {
    Click,
    Wheel(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativePreparedPaste {
    pub bytes: usize,
    pub bracketed: bool,
}

pub struct NativeTerminal {
    terminal: Terminal,
    config: NativeTerminalConfig,
    /// The installed history size and theme; both survive changes of the other.
    retained_scrollback: usize,
    base_palette: ColorPalette,
    witness: RenderFrameWitness,
    pending_reply: Option<u64>,
    closed: bool,
}

impl NativeTerminal {
    pub fn new(config: NativeTerminalConfig) -> Result<Self, NativeTerminalError> {
        config.validate()?;
        let retained_scrollback = config.retained_scrollback_rows()?;
        let terminal = Terminal::new_bounded_output_with_parser_limit(
            config.size,
            Arc::new(NativeConfiguration {
                scrollback_rows: retained_scrollback,
                palette: ColorPalette::default(),
            }),
            "FrankenTerm",
            env!("CARGO_PKG_VERSION"),
            BoundedOutputLimits {
                // Input never uses this writer. These minimum bounded slots
                // keep the shared writer's invariant without a second egress
                // path; only pure encoders are exposed by NativeTerminal.
                input_bytes: MAX_REPLY_BYTES,
                reply_bytes: MAX_REPLY_BYTES,
                input_units: 1,
                reply_units: 128,
                maximum_unit_bytes: MAX_REPLY_BYTES,
                maximum_paste_bytes: 1,
                maximum_drain_bytes: MAX_REPLY_BYTES,
            },
            MAX_FEED_BYTES,
        )?;
        Ok(Self {
            terminal,
            config,
            retained_scrollback,
            base_palette: ColorPalette::default(),
            witness: RenderFrameWitness {
                channel_id: config.channel_id,
                transport_id: config.transport_id,
                route_revision: config.route_revision,
                geometry_revision: 1,
                palette_revision: 1,
                font_revision: config.font_revision,
                frame_revision: 1,
            },
            pending_reply: None,
            closed: false,
        })
    }

    fn live(&self) -> Result<(), NativeTerminalError> {
        if self.closed {
            Err(NativeTerminalError::Closed)
        } else {
            Ok(())
        }
    }

    fn can_mutate(&self) -> Result<(), NativeTerminalError> {
        self.live()?;
        // The terminal uses saturating semantic generations. Refuse before
        // the sentinel can become a valid native frame or repeat a version.
        if self.terminal.current_seqno() >= usize::MAX - 16 {
            return Err(NativeTerminalError::SequenceExhausted);
        }
        Ok(())
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), NativeTerminalError> {
        self.can_mutate()?;
        if bytes.len() > MAX_FEED_BYTES {
            return Err(NativeTerminalError::Capacity);
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let palette = self
            .witness
            .palette_revision
            .checked_add(1)
            .ok_or(NativeTerminalError::SequenceExhausted)?;
        let geometry = self
            .witness
            .geometry_revision
            .checked_add(1)
            .ok_or(NativeTerminalError::SequenceExhausted)?;
        let alternate = self.terminal.is_alt_screen_active();
        let dropped_replies = self
            .terminal
            .bounded_output_status()
            .ok_or(NativeTerminalError::Invalid)?
            .dropped_replies;
        self.terminal.advance_bytes(bytes);
        if self.terminal.cell_grapheme_limit_exceeded()
            || self.terminal.native_execution_limit_exceeded()
            || self
                .terminal
                .bounded_output_status()
                .is_some_and(|status| status.closed || status.dropped_replies != dropped_replies)
        {
            self.poison();
            return Err(NativeTerminalError::Capacity);
        }
        // Conservatively invalidate native palette preparation after every
        // output batch. This may rebuild an unchanged palette; it never lets
        // changed colors reuse a stale palette identity.
        self.witness.palette_revision = palette;
        // The alternate screen is a different grid with its own rows, so a
        // renderer must not accept it under the previous geometry identity.
        if self.terminal.is_alt_screen_active() != alternate {
            self.witness.geometry_revision = geometry;
        }
        Ok(())
    }

    /// A grid too large to keep the configured history beside both screens
    /// keeps the history that fits, dropping the oldest rows now; a smaller
    /// grid later raises the limit again but cannot recover dropped rows.
    pub fn resize(&mut self, size: TerminalSize) -> Result<(), NativeTerminalError> {
        self.can_mutate()?;
        let mut proposed = self.config;
        proposed.size = size;
        let retained = proposed.retained_scrollback_rows()?;
        if size == self.config.size {
            return Ok(());
        }
        let revision = self
            .witness
            .geometry_revision
            .checked_add(1)
            .ok_or(NativeTerminalError::SequenceExhausted)?;
        self.terminal.resize(size);
        if retained != self.retained_scrollback {
            self.retained_scrollback = retained;
            self.install_configuration();
            self.terminal.trim_scrollback_to_config();
        }
        self.config.size = size;
        self.witness.geometry_revision = revision;
        Ok(())
    }

    fn install_configuration(&mut self) {
        self.terminal.set_config(Arc::new(NativeConfiguration {
            scrollback_rows: self.retained_scrollback,
            palette: self.base_palette.clone(),
        }));
    }

    /// The history rows this grid keeps at most (see
    /// [`NativeTerminalConfig::retained_scrollback_rows`]).
    pub fn retained_scrollback_rows(&self) -> usize {
        self.retained_scrollback
    }

    pub fn config(&self) -> NativeTerminalConfig {
        self.config
    }

    /// Replaces the configured base palette (the machine theme) from
    /// [`BASE_PALETTE_BYTES`] of sRGB RGBA8 in the [`BASE_PALETTE_COLORS`]
    /// order. An application's OSC overrides stay in effect until it resets
    /// them, which then yields this palette.
    pub fn set_base_palette(&mut self, rgba: &[u8]) -> Result<(), NativeTerminalError> {
        self.can_mutate()?;
        let palette = decode_native_palette(rgba)?;
        let revision = self
            .witness
            .palette_revision
            .checked_add(1)
            .ok_or(NativeTerminalError::SequenceExhausted)?;
        self.base_palette = palette;
        self.install_configuration();
        self.witness.palette_revision = revision;
        Ok(())
    }

    /// A verified mux publication's complete native RGBA8 palette. It is a
    /// transient terminal override, independent of the configured machine
    /// theme. Geometry/configuration updates preserve it; RIS on pane switch
    /// clears it and restores the configured palette, including its defaults.
    pub fn set_mux_palette(&mut self, rgba: &[u8]) -> Result<(), NativeTerminalError> {
        self.can_mutate()?;
        let palette = decode_native_palette(rgba)?;
        let revision = self
            .witness
            .palette_revision
            .checked_add(1)
            .ok_or(NativeTerminalError::SequenceExhausted)?;
        *self.terminal.palette_mut() = palette;
        self.witness.palette_revision = revision;
        Ok(())
    }

    pub fn set_font_revision(&mut self, revision: u64) -> Result<(), NativeTerminalError> {
        self.live()?;
        if revision < self.witness.font_revision || revision == 0 {
            return Err(NativeTerminalError::Stale);
        }
        self.witness.font_revision = revision;
        self.config.font_revision = revision;
        Ok(())
    }

    /// No byte is copied until the complete encoding is known to fit. The
    /// canonical encoder is run twice against the same immutable model; each
    /// temporary per-key String is bounded independently of the input length.
    pub fn encode_key(
        &self,
        usage: u32,
        modifiers: u32,
        phase: NativeKeyPhase,
        characters: &str,
        output: &mut [u8],
    ) -> Result<usize, NativeTerminalError> {
        self.live()?;
        if !(1..=0xe7).contains(&usage) || modifiers & !31 != 0 || characters.len() > 256 {
            return Err(NativeTerminalError::Invalid);
        }
        let key = hardware_key(usage)?;
        if key.is_none() && characters.is_empty() {
            return Err(NativeTerminalError::Unsupported);
        }
        let mods = native_modifiers(modifiers);
        self.copy_encoding(output, |emit| {
            if let Some(key) = key {
                self.emit_key(key, mods, phase, emit)
            } else {
                for character in characters.chars() {
                    self.emit_key(KeyCode::Char(character), mods, phase, emit)?;
                }
                Ok(())
            }
        })
    }

    pub fn encode_text(&self, text: &str, output: &mut [u8]) -> Result<usize, NativeTerminalError> {
        self.live()?;
        if text.len() > MAX_TEXT_BYTES {
            return Err(NativeTerminalError::Capacity);
        }
        self.copy_encoding(output, |emit| {
            let mut chars = text.chars().peekable();
            while let Some(character) = chars.next() {
                let key = match character {
                    '\r' => {
                        if chars.peek() == Some(&'\n') {
                            chars.next();
                        }
                        KeyCode::Enter
                    }
                    '\n' => KeyCode::Enter,
                    character => KeyCode::Char(character),
                };
                self.emit_key(key, KeyModifiers::NONE, NativeKeyPhase::Press, emit)?;
            }
            Ok(())
        })
    }

    /// Encode against the current canonical mouse/keyboard modes, but only for
    /// the exact displayed geometry. Pixel protocols use the authoritative
    /// model's cell center, never UIKit points or a guessed display scale.
    pub fn encode_mouse(
        &self,
        action: NativeMouseAction,
        column: u32,
        row: u32,
        modifiers: u32,
        geometry_revision: u64,
        output: &mut [u8],
    ) -> Result<usize, NativeTerminalError> {
        self.live()?;
        if geometry_revision == 0
            || geometry_revision != self.witness.geometry_revision
            || self.terminal.synchronized_output()
        {
            return Err(NativeTerminalError::Stale);
        }
        let size = self.config.size;
        if column as usize >= size.cols
            || row as usize >= size.rows
            || modifiers & !31 != 0
            || output.len() > MAX_INPUT_BYTES
        {
            return Err(NativeTerminalError::Invalid);
        }
        let (button, repetitions) = match action {
            NativeMouseAction::Click => (MouseButton::Left, 1),
            NativeMouseAction::Wheel(steps)
                if steps != 0 && (-MAX_MOUSE_STEPS..=MAX_MOUSE_STEPS).contains(&steps) =>
            {
                (
                    if steps < 0 {
                        MouseButton::WheelUp(1)
                    } else {
                        MouseButton::WheelDown(1)
                    },
                    steps.unsigned_abs() as usize,
                )
            }
            NativeMouseAction::Wheel(_) => return Err(NativeTerminalError::Invalid),
        };
        if self.terminal.is_mouse_grabbed()
            && self.terminal.mouse_uses_pixel_coordinates()
            && (size.pixel_width < size.cols || size.pixel_height < size.rows)
        {
            return Err(NativeTerminalError::Unsupported);
        }
        let event = MouseEvent {
            kind: MouseEventKind::Press,
            button,
            x: column as usize,
            y: i64::from(row),
            x_pixel_offset: (size.pixel_width / size.cols / 2) as isize,
            y_pixel_offset: (size.pixel_height / size.rows / 2) as isize,
            modifiers: native_modifiers(modifiers),
        };
        let bytes = match action {
            NativeMouseAction::Click => self.terminal.encode_mouse_click(event),
            NativeMouseAction::Wheel(_) => self.terminal.encode_mouse_wheel(event),
        }
        .map_err(|_| NativeTerminalError::Unsupported)?;
        let count = bytes
            .len()
            .checked_mul(repetitions)
            .ok_or(NativeTerminalError::Capacity)?;
        if count > output.len() {
            return Err(NativeTerminalError::Capacity);
        }
        if !bytes.is_empty() {
            for destination in output[..count].chunks_exact_mut(bytes.len()) {
                destination.copy_from_slice(&bytes);
            }
        }
        Ok(count)
    }

    fn emit_key(
        &self,
        key: KeyCode,
        modifiers: KeyModifiers,
        phase: NativeKeyPhase,
        emit: &mut dyn FnMut(&str) -> Result<(), NativeTerminalError>,
    ) -> Result<(), NativeTerminalError> {
        if phase != NativeKeyPhase::Up {
            let encoded = self
                .terminal
                .encode_key(key, modifiers, true)
                .map_err(|_| NativeTerminalError::Unsupported)?;
            emit(&encoded)?;
        }
        if matches!(phase, NativeKeyPhase::Up | NativeKeyPhase::Press) {
            let encoded = self
                .terminal
                .encode_key(key, modifiers, false)
                .map_err(|_| NativeTerminalError::Unsupported)?;
            emit(&encoded)?;
        }
        Ok(())
    }

    fn copy_encoding(
        &self,
        output: &mut [u8],
        encode: impl Fn(
            &mut dyn FnMut(&str) -> Result<(), NativeTerminalError>,
        ) -> Result<(), NativeTerminalError>,
    ) -> Result<usize, NativeTerminalError> {
        if output.len() > MAX_INPUT_BYTES {
            return Err(NativeTerminalError::Invalid);
        }
        let mut length = 0usize;
        encode(&mut |part| {
            length = length
                .checked_add(part.len())
                .ok_or(NativeTerminalError::Capacity)?;
            if length > output.len() {
                return Err(NativeTerminalError::Capacity);
            }
            Ok(())
        })?;
        let mut offset = 0;
        encode(&mut |part| {
            let end = offset + part.len();
            output[offset..end].copy_from_slice(part.as_bytes());
            offset = end;
            Ok(())
        })?;
        Ok(length)
    }

    pub fn prepare_paste(
        &self,
        text: &str,
        output: &mut [u8],
    ) -> Result<NativePreparedPaste, NativeTerminalError> {
        self.live()?;
        if text.len() > MAX_INPUT_BYTES || output.len() > MAX_INPUT_BYTES {
            return Err(NativeTerminalError::Capacity);
        }
        if text.chars().any(|character| {
            let code = u32::from(character);
            (code < 32 && !matches!(code, 9 | 10 | 13)) || (127..=159).contains(&code)
        }) {
            return Err(NativeTerminalError::Invalid);
        }
        let bracketed = self.terminal.bracketed_paste_enabled();
        let canon = if bracketed {
            NewlineCanon::None
        } else {
            NewlineCanon::CarriageReturn
        };
        let mut bytes = 0usize;
        canon.for_each_chunk(text, |chunk| bytes += chunk.len());
        if bytes > output.len() || bytes + (if bracketed { 12 } else { 0 }) > MAX_INPUT_BYTES {
            return Err(NativeTerminalError::Capacity);
        }
        let mut offset = 0;
        canon.for_each_chunk(text, |chunk| {
            let end = offset + chunk.len();
            output[offset..end].copy_from_slice(chunk.as_bytes());
            offset = end;
        });
        Ok(NativePreparedPaste { bytes, bracketed })
    }

    pub fn copy_reply(
        &mut self,
        output: &mut [u8],
    ) -> Result<Option<BoundedReplyCopy>, NativeTerminalError> {
        self.live()?;
        if self.pending_reply.is_some() {
            return Err(NativeTerminalError::Busy);
        }
        if output.len() > MAX_REPLY_BYTES {
            return Err(NativeTerminalError::Invalid);
        }
        let copied = self.terminal.copy_bounded_reply(output)?;
        self.pending_reply = copied.map(|reply| reply.sequence);
        Ok(copied)
    }

    pub fn settle_reply(
        &mut self,
        sequence: u64,
        disposition: BoundedReplyDisposition,
    ) -> Result<(), NativeTerminalError> {
        self.live()?;
        if self.pending_reply != Some(sequence) {
            return Err(NativeTerminalError::Stale);
        }
        self.terminal.settle_bounded_reply(sequence, disposition)?;
        self.pending_reply = None;
        Ok(())
    }

    pub fn capture_frame(
        &mut self,
        viewport_top: Option<i64>,
    ) -> Result<Vec<u8>, NativeTerminalError> {
        self.live()?;
        let revision = self
            .witness
            .frame_revision
            .checked_add(1)
            .ok_or(NativeTerminalError::SequenceExhausted)?;
        let viewport = viewport_top
            .map(StableRowIndex::try_from)
            .transpose()
            .map_err(|_| NativeTerminalError::Invalid)?;
        let mut witness = self.witness;
        witness.frame_revision = revision;
        let frame = capture_terminal_frame(
            &self.terminal,
            witness,
            viewport,
            RenderFrameLimits {
                max_grapheme_bytes: MAX_CELL_GRAPHEME_BYTES,
                max_uri_bytes: 512,
                ..RenderFrameLimits::default()
            },
        )?;
        self.witness = witness;
        Ok(frame)
    }

    pub fn close(&mut self) -> Result<(), NativeTerminalError> {
        if self.pending_reply.is_some() {
            return Err(NativeTerminalError::Busy);
        }
        self.poison();
        Ok(())
    }

    pub(crate) fn poison(&mut self) {
        self.closed = true;
        self.pending_reply = None;
        let _ = self.terminal.close_bounded_output();
    }
}

fn native_modifiers(bits: u32) -> KeyModifiers {
    let mut modifiers = KeyModifiers::NONE;
    if bits & 1 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if bits & 2 != 0 {
        modifiers |= KeyModifiers::CTRL;
    }
    if bits & 4 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if bits & 8 != 0 {
        modifiers |= KeyModifiers::SUPER;
    }
    // Caps Lock is already reflected in UIKit's layout-resolved characters.
    // The canonical termwiz Modifiers type has no caps-lock protocol flag.
    modifiers
}

/// USB HID classification only. Escape/keyboard protocol bytes always come
/// from TerminalState::encode_key; printable characters come from the native
/// keyboard layout, never a fabricated US-layout fallback.
fn hardware_key(usage: u32) -> Result<Option<KeyCode>, NativeTerminalError> {
    use KeyCode::*;
    let key = match usage {
        0x04..=0x27 | 0x2d..=0x38 | 0x64 => return Ok(None),
        0x28 | 0x58 => Enter,
        0x29 => Escape,
        0x2a => Backspace,
        0x2b => Tab,
        0x2c => Char(' '),
        0x39 => CapsLock,
        0x3a..=0x45 => Function((usage - 0x3a + 1) as u8),
        0x46 => PrintScreen,
        0x47 => ScrollLock,
        0x48 => Pause,
        0x49 => Insert,
        0x4a => Home,
        0x4b => PageUp,
        0x4c => Delete,
        0x4d => End,
        0x4e => PageDown,
        0x4f => RightArrow,
        0x50 => LeftArrow,
        0x51 => DownArrow,
        0x52 => UpArrow,
        0x53 => NumLock,
        0x54 => Divide,
        0x55 => Multiply,
        0x56 => Subtract,
        0x57 => Add,
        0x59 => Numpad1,
        0x5a => Numpad2,
        0x5b => Numpad3,
        0x5c => Numpad4,
        0x5d => Numpad5,
        0x5e => Numpad6,
        0x5f => Numpad7,
        0x60 => Numpad8,
        0x61 => Numpad9,
        0x62 => Numpad0,
        0x63 => Decimal,
        0x65 => Applications,
        0x68..=0x73 => Function((usage - 0x68 + 13) as u8),
        0x7f => VolumeMute,
        0x80 => VolumeUp,
        0x81 => VolumeDown,
        0xe0 => LeftControl,
        0xe1 => LeftShift,
        0xe2 => LeftAlt,
        0xe3 => LeftWindows,
        0xe4 => RightControl,
        0xe5 => RightShift,
        0xe6 => RightAlt,
        0xe7 => RightWindows,
        _ => return Err(NativeTerminalError::Unsupported),
    };
    Ok(Some(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::TryInto;

    fn config() -> NativeTerminalConfig {
        NativeTerminalConfig {
            size: TerminalSize {
                rows: 3,
                cols: 12,
                ..TerminalSize::default()
            },
            scrollback_rows: 8,
            channel_id: [1; 16],
            transport_id: [2; 16],
            route_revision: 3,
            font_revision: 4,
        }
    }

    fn terminal() -> NativeTerminal {
        NativeTerminal::new(config()).unwrap()
    }

    #[test]
    fn mouse_click_is_pure_complete_mode_aware_and_capacity_atomic() {
        let mut terminal = terminal();
        let mut bytes = [0xa5; 128];
        assert_eq!(
            terminal
                .encode_mouse(NativeMouseAction::Click, 2, 1, 0, 1, &mut bytes)
                .unwrap(),
            0
        );
        terminal.feed(b"\x1b[?1000h\x1b[?1006h\x1b[6n").unwrap();
        let before = terminal.terminal.bounded_output_status().unwrap();
        let sequence = terminal.terminal.current_seqno();
        let expected = b"\x1b[<0;3;2M\x1b[<0;3;2m";
        assert_eq!(
            terminal.encode_mouse(
                NativeMouseAction::Click,
                2,
                1,
                0,
                1,
                &mut bytes[..expected.len() - 1]
            ),
            Err(NativeTerminalError::Capacity)
        );
        assert_eq!(
            bytes, [0xa5; 128],
            "a refused release must not leave a copied press"
        );
        for _ in 0..2 {
            let count = terminal
                .encode_mouse(NativeMouseAction::Click, 2, 1, 0, 1, &mut bytes)
                .unwrap();
            assert_eq!(&bytes[..count], expected);
        }
        assert_eq!(terminal.terminal.current_seqno(), sequence);
        assert_eq!(terminal.terminal.bounded_output_status().unwrap(), before);
        let reply = terminal.copy_reply(&mut bytes).unwrap().unwrap();
        assert_eq!(
            &bytes[..reply.bytes],
            b"\x1b[1;1R",
            "mouse encoding cannot consume parser replies"
        );
        terminal
            .settle_reply(reply.sequence, BoundedReplyDisposition::Accepted)
            .unwrap();
        terminal.feed(b"\x1b[?1006l").unwrap();
        let count = terminal
            .encode_mouse(NativeMouseAction::Click, 2, 1, 3, 1, &mut bytes)
            .unwrap();
        assert_eq!(
            &bytes[..count],
            b"\x1b[M4#\"\x1b[M##\"",
            "legacy release is button 3, without modifiers"
        );
        terminal.feed(b"\x1b[?1000l").unwrap();
        assert_eq!(
            terminal
                .encode_mouse(NativeMouseAction::Click, 2, 1, 0, 1, &mut bytes)
                .unwrap(),
            0
        );
    }

    #[test]
    fn mouse_wheel_uses_tracking_or_canonical_alternate_screen_keys() {
        let mut terminal = terminal();
        let mut bytes = [0; 256];
        assert_eq!(
            terminal
                .encode_mouse(NativeMouseAction::Wheel(-1), 0, 0, 0, 1, &mut bytes)
                .unwrap(),
            0
        );
        terminal.feed(b"\x1b[?1002h\x1b[?1006h").unwrap();
        let count = terminal
            .encode_mouse(NativeMouseAction::Wheel(-2), 2, 1, 4, 1, &mut bytes)
            .unwrap();
        assert_eq!(&bytes[..count], b"\x1b[<72;3;2M\x1b[<72;3;2M");
        terminal.feed(b"\x1b[?1002l\x1b[?1049h\x1b[?1h").unwrap();
        let count = terminal
            .encode_mouse(NativeMouseAction::Wheel(2), 0, 0, 0, 2, &mut bytes)
            .unwrap();
        assert_eq!(&bytes[..count], b"\x1bOB\x1bOB\x1bOB\x1bOB\x1bOB\x1bOB");
        assert_eq!(
            terminal
                .terminal
                .bounded_output_status()
                .unwrap()
                .pending_input_bytes,
            0
        );
    }

    #[test]
    fn mouse_geometry_and_steps_fail_before_copy_and_pixel_modes_use_model_extent() {
        let mut terminal = terminal();
        terminal.feed(b"\x1b[?1000h\x1b[?1016h").unwrap();
        let mut bytes = [0xa5; 128];
        assert_eq!(
            terminal.encode_mouse(NativeMouseAction::Click, 2, 1, 0, 1, &mut bytes),
            Err(NativeTerminalError::Unsupported)
        );
        let mut size = terminal.config().size;
        size.pixel_width = 120;
        size.pixel_height = 60;
        terminal.resize(size).unwrap();
        for (action, column, row, geometry, expected) in [
            (
                NativeMouseAction::Click,
                2,
                1,
                1,
                NativeTerminalError::Stale,
            ),
            (
                NativeMouseAction::Click,
                12,
                1,
                2,
                NativeTerminalError::Invalid,
            ),
            (
                NativeMouseAction::Click,
                2,
                3,
                2,
                NativeTerminalError::Invalid,
            ),
            (
                NativeMouseAction::Wheel(0),
                2,
                1,
                2,
                NativeTerminalError::Invalid,
            ),
            (
                NativeMouseAction::Wheel(i32::MIN),
                2,
                1,
                2,
                NativeTerminalError::Invalid,
            ),
            (
                NativeMouseAction::Wheel(33),
                2,
                1,
                2,
                NativeTerminalError::Invalid,
            ),
        ] {
            assert_eq!(
                terminal.encode_mouse(action, column, row, 0, geometry, &mut bytes),
                Err(expected)
            );
            assert_eq!(bytes, [0xa5; 128]);
        }
        let count = terminal
            .encode_mouse(NativeMouseAction::Click, 2, 1, 0, 2, &mut bytes)
            .unwrap();
        assert_eq!(&bytes[..count], b"\x1b[<0;26;31M\x1b[<0;26;31m");
        terminal.feed(b"\x1b[?2026h").unwrap();
        assert_eq!(
            terminal.encode_mouse(NativeMouseAction::Click, 2, 1, 0, 2, &mut bytes),
            Err(NativeTerminalError::Stale)
        );
    }

    #[test]
    fn mouse_utf8_coordinates_and_capture_grab_flag_follow_canonical_modes() {
        let mut terminal = terminal();
        let flag = crate::render::RENDER_FRAME_FLAG_MOUSE_GRABBED;
        let captured_flag = |term: &mut NativeTerminal| {
            let bytes = term.capture_frame(None).unwrap();
            u32::from_le_bytes(bytes[12..16].try_into().unwrap()) & flag
        };
        assert_eq!(captured_flag(&mut terminal), 0);
        let mut size = terminal.config().size;
        size.cols = 300;
        terminal.resize(size).unwrap();
        terminal.feed(b"\x1b[?1003h\x1b[?1005h").unwrap();
        assert_eq!(captured_flag(&mut terminal), flag);
        let mut bytes = [0; 128];
        let count = terminal
            .encode_mouse(NativeMouseAction::Click, 250, 0, 0, 2, &mut bytes)
            .unwrap();
        assert_eq!(&bytes[..count], b"\x1b[M \xc4\x9b!\x1b[M#\xc4\x9b!");
        terminal.feed(b"\x1b[?1003l").unwrap();
        assert_eq!(captured_flag(&mut terminal), 0);
    }

    #[test]
    fn pure_canonical_key_encoding_respects_modes_and_preserves_pending_reply() {
        let mut terminal = terminal();
        terminal.feed(b"\x1b[6n").unwrap();
        let before = terminal.terminal.bounded_output_status().unwrap();
        let mut bytes = [0u8; 128];
        let count = terminal
            .encode_key(0x52, 0, NativeKeyPhase::Press, "", &mut bytes)
            .unwrap();
        assert_eq!(&bytes[..count], b"\x1b[A");
        assert_eq!(terminal.terminal.bounded_output_status().unwrap(), before);
        terminal.feed(b"\x1b[?1h").unwrap();
        let count = terminal
            .encode_key(0x52, 0, NativeKeyPhase::Down, "", &mut bytes)
            .unwrap();
        assert_eq!(&bytes[..count], b"\x1bOA");
        let reply = terminal.copy_reply(&mut bytes).unwrap().unwrap();
        assert_eq!(&bytes[..reply.bytes], b"\x1b[1;1R");
        assert_eq!(
            terminal.copy_reply(&mut bytes),
            Err(NativeTerminalError::Busy)
        );
        assert_eq!(terminal.close(), Err(NativeTerminalError::Busy));
        assert_eq!(
            terminal.settle_reply(reply.sequence + 1, BoundedReplyDisposition::Accepted),
            Err(NativeTerminalError::Stale)
        );
        terminal
            .settle_reply(reply.sequence, BoundedReplyDisposition::Accepted)
            .unwrap();
        assert_eq!(
            terminal.settle_reply(reply.sequence, BoundedReplyDisposition::Accepted),
            Err(NativeTerminalError::Stale)
        );
        assert_eq!(terminal.copy_reply(&mut bytes).unwrap(), None);
    }

    #[test]
    fn negotiated_kitty_press_release_and_pop_reuse_canonical_encoding_without_egress() {
        let mut terminal = terminal();
        terminal.feed(b"\x1b[>11u\x1b[?u").unwrap();
        let before = terminal.terminal.bounded_output_status().unwrap();
        let mut bytes = [0; 128];
        let count = terminal
            .encode_key(0x13, 8, NativeKeyPhase::Down, "p", &mut bytes)
            .unwrap();
        assert_eq!(&bytes[..count], b"\x1b[112;9u");
        let count = terminal
            .encode_key(0x13, 8, NativeKeyPhase::Up, "p", &mut bytes)
            .unwrap();
        assert_eq!(&bytes[..count], b"\x1b[112;9:3u");
        let count = terminal
            .encode_key(0x13, 8, NativeKeyPhase::Press, "p", &mut bytes)
            .unwrap();
        assert_eq!(&bytes[..count], b"\x1b[112;9u\x1b[112;9:3u");
        assert_eq!(terminal.terminal.bounded_output_status().unwrap(), before);
        let reply = terminal.copy_reply(&mut bytes).unwrap().unwrap();
        assert_eq!(&bytes[..reply.bytes], b"\x1b[?11u");
        terminal
            .settle_reply(reply.sequence, BoundedReplyDisposition::Accepted)
            .unwrap();
        terminal.feed(b"\x1b[<u").unwrap();
        assert_eq!(
            terminal
                .encode_key(0x13, 8, NativeKeyPhase::Up, "p", &mut bytes)
                .unwrap(),
            0
        );
    }

    #[test]
    fn text_enter_policy_is_canonical_and_capacity_failure_copies_nothing() {
        let mut terminal = terminal();
        let mut bytes = [0xa5; 64];
        let count = terminal.encode_text("a\nb\r\nc", &mut bytes).unwrap();
        assert_eq!(&bytes[..count], b"a\rb\rc");
        terminal.feed(b"\x1b[20h").unwrap();
        let count = terminal.encode_text("a\nb\r\nc", &mut bytes).unwrap();
        assert_eq!(&bytes[..count], b"a\r\nb\r\nc");
        let mut refused = [0xa5; 2];
        assert_eq!(
            terminal.encode_text("abc", &mut refused),
            Err(NativeTerminalError::Capacity)
        );
        assert_eq!(refused, [0xa5; 2]);
        assert_eq!(
            terminal
                .terminal
                .bounded_output_status()
                .unwrap()
                .pending_input_bytes,
            0
        );
    }

    #[test]
    fn paste_preparation_preserves_negotiated_policy_without_publishing_wrappers() {
        let mut terminal = terminal();
        let mut bytes = [0xa5; 64];
        let prepared = terminal.prepare_paste("a\r\nb\nc", &mut bytes).unwrap();
        assert!(!prepared.bracketed);
        assert_eq!(&bytes[..prepared.bytes], b"a\rb\rc");
        terminal.feed(b"\x1b[?2004h").unwrap();
        let prepared = terminal.prepare_paste("a\r\nb\nc", &mut bytes).unwrap();
        assert!(prepared.bracketed);
        assert_eq!(&bytes[..prepared.bytes], b"a\r\nb\nc");
        let before = bytes;
        assert_eq!(
            terminal.prepare_paste("unsafe\x1b[201~", &mut bytes),
            Err(NativeTerminalError::Invalid)
        );
        assert_eq!(bytes, before);
        assert_eq!(
            terminal
                .terminal
                .bounded_output_status()
                .unwrap()
                .pending_input_bytes,
            0
        );
    }

    #[test]
    fn parser_retained_string_bound_spans_chunks_and_resumes_at_terminator() {
        let mut terminal = terminal();
        terminal.feed(b"\x1b]0;").unwrap();
        terminal.feed(&vec![b'x'; MAX_FEED_BYTES]).unwrap();
        terminal.feed(&vec![b'x'; MAX_FEED_BYTES]).unwrap();
        terminal.feed(b"\x07ok").unwrap();
        let frame = terminal.capture_frame(None).unwrap();
        assert_eq!(&frame[..4], b"FTSF");
        assert!(frame.windows(2).any(|bytes| bytes == b"ok"));
        assert!(!frame.windows(8).any(|bytes| bytes == b"xxxxxxxx"));
    }

    #[test]
    fn combining_mark_growth_is_bounded_across_feed_calls_and_closes_model() {
        let mut terminal = terminal();
        terminal.feed(b"a").unwrap();
        for _ in 0..127 {
            terminal.feed("\u{301}".as_bytes()).unwrap();
        }
        assert_eq!(
            terminal.feed("\u{301}".as_bytes()),
            Err(NativeTerminalError::Capacity)
        );
        assert_eq!(
            terminal.feed(b"must not resume"),
            Err(NativeTerminalError::Closed)
        );
        assert_eq!(
            terminal.capture_frame(None),
            Err(NativeTerminalError::Closed)
        );
        terminal.close().unwrap();
        terminal.close().unwrap();
    }

    #[test]
    fn same_feed_ascii_junction_cannot_bypass_retained_grapheme_limit() {
        let mut terminal = terminal();
        // The ASCII fast path writes b directly; 128 combining acute accents
        // then join that cell without an intervening feed. b has no NFC acute
        // composition, so the resulting 257 UTF-8 bytes exceed the 256 cap.
        let bytes = format!("b{}", "\u{301}".repeat(128));
        assert_eq!(
            terminal.feed(bytes.as_bytes()),
            Err(NativeTerminalError::Capacity)
        );
        assert_eq!(
            terminal.capture_frame(None),
            Err(NativeTerminalError::Closed)
        );
    }

    #[test]
    fn expansive_u32_repeat_is_refused_before_work_and_later_handlers_are_inert() {
        let mut terminal = terminal();
        terminal.feed(b"a").unwrap();
        assert_eq!(
            terminal.feed(b"\x1b[4294967295b\nignored\x1b[31m\x1b[6n"),
            Err(NativeTerminalError::Capacity)
        );
        assert!(terminal.terminal.native_execution_limit_exceeded());
        assert_eq!(terminal.terminal.cursor_pos().x, 1);
        assert_eq!(terminal.terminal.cursor_pos().y, 0);
        assert_eq!(
            terminal
                .terminal
                .bounded_output_status()
                .unwrap()
                .admitted_through,
            0
        );
        assert_eq!(
            terminal.capture_frame(None),
            Err(NativeTerminalError::Closed)
        );
        assert_eq!(terminal.feed(b"retry"), Err(NativeTerminalError::Closed));
    }

    #[test]
    fn reverse_wrap_count_cannot_hold_the_native_registry_for_u32_iterations() {
        let mut terminal = terminal();
        terminal.feed(b"\x1b[?45h").unwrap();
        assert_eq!(
            terminal.feed(b"\x1b[4294967295D"),
            Err(NativeTerminalError::Capacity)
        );
        assert_eq!(terminal.terminal.cursor_pos().x, 0);
        assert_eq!(terminal.terminal.cursor_pos().y, 0);
        assert!(terminal.terminal.native_execution_limit_exceeded());
    }

    #[test]
    fn expanded_work_budget_accumulates_across_commands_and_renews_only_next_feed() {
        let mut separate = terminal();
        separate.feed(b"a\x1b[4096b").unwrap();
        separate.feed(b"\x1b[4096b").unwrap();
        assert!(separate.capture_frame(None).is_ok());

        let mut combined = terminal();
        assert_eq!(
            combined.feed(b"a\x1b[4096b\x1b[4096b"),
            Err(NativeTerminalError::Capacity)
        );
        assert!(combined.terminal.native_execution_limit_exceeded());
    }

    #[test]
    fn idempotent_u32_insert_tab_and_keyboard_pop_match_geometry_bounded_execution() {
        let mut huge = terminal();
        let mut bounded = terminal();
        huge.feed(
            b"abcdef\x1b[1G\x1b[4294967295@\x1b[4294967295I\x1b[4294967295Z\x1b[<4294967295u",
        )
        .unwrap();
        bounded
            .feed(b"abcdef\x1b[1G\x1b[12@\x1b[12I\x1b[12Z\x1b[<128u")
            .unwrap();
        assert_eq!(
            huge.capture_frame(None).unwrap(),
            bounded.capture_frame(None).unwrap()
        );
        assert_eq!(huge.terminal.cursor_pos().x, 0);
        assert!(!huge.terminal.native_execution_limit_exceeded());
    }

    #[test]
    fn repeated_small_screen_commands_share_the_same_native_work_budget() {
        let cases: &[(&[u8], usize)] = &[
            (b"\x1b[2J", 2000),
            (b"\x1b#8", 2000),
            (b"\x1bc", 512),
            (b"\x1b[?3h", 2000),
            (b"\x1b[?1049h\x1b[?1049l", 1000),
        ];
        for (sequence, count) in cases {
            let mut terminal = terminal();
            let input = sequence.repeat(*count);
            assert!(input.len() <= MAX_FEED_BYTES);
            assert_eq!(terminal.feed(&input), Err(NativeTerminalError::Capacity));
            assert!(terminal.terminal.native_execution_limit_exceeded());
            assert_eq!(
                terminal.capture_frame(None),
                Err(NativeTerminalError::Closed)
            );
        }
    }

    #[test]
    fn ordinary_byte_bounded_line_output_does_not_consume_command_expansion_budget() {
        let mut terminal = terminal();
        terminal.feed(&b"x\r\n".repeat(MAX_FEED_BYTES / 3)).unwrap();
        assert!(!terminal.terminal.native_execution_limit_exceeded());
        assert!(terminal.capture_frame(None).is_ok());
    }

    #[test]
    fn parser_reply_overflow_closes_instead_of_silently_losing_a_reply() {
        let mut terminal = terminal();
        terminal.feed(b"\x1b[6n").unwrap();
        let reply = terminal
            .copy_reply(&mut [0; MAX_REPLY_BYTES])
            .unwrap()
            .unwrap();
        terminal
            .settle_reply(reply.sequence, BoundedReplyDisposition::Dropped)
            .unwrap();
        // An explicit downstream refusal is already settled; it does not
        // become a new parser admission failure during an unrelated feed.
        terminal.feed(b"still live").unwrap();
        assert_eq!(
            terminal.feed(&b"\x1b[6n".repeat(129)),
            Err(NativeTerminalError::Capacity)
        );
        assert_eq!(
            terminal.copy_reply(&mut [0; MAX_REPLY_BYTES]),
            Err(NativeTerminalError::Closed)
        );
        assert_eq!(
            terminal.capture_frame(None),
            Err(NativeTerminalError::Closed)
        );
    }

    #[test]
    fn native_text_profile_refuses_image_execution_and_clipboard_capability() {
        let mut terminal = terminal();
        terminal
            .feed(b"\x1b[c\x1bPq~\x1b\\\x1b_Ga=T,f=32,s=1,v=1;AAAAAA==\x1b\\")
            .unwrap();
        let mut bytes = [0; MAX_REPLY_BYTES];
        let reply = terminal.copy_reply(&mut bytes).unwrap().unwrap();
        assert_eq!(&bytes[..reply.bytes], b"\x1b[?65;22c");
        terminal
            .settle_reply(reply.sequence, BoundedReplyDisposition::Accepted)
            .unwrap();
        terminal.feed(b"\x1bP+q4D73\x1b\\").unwrap();
        let reply = terminal.copy_reply(&mut bytes).unwrap().unwrap();
        assert_eq!(&bytes[..reply.bytes], b"\x1bP0+r4D73\x1b\\");
        terminal
            .settle_reply(reply.sequence, BoundedReplyDisposition::Dropped)
            .unwrap();
        assert_eq!(
            terminal
                .terminal
                .bounded_output_status()
                .unwrap()
                .dropped_replies,
            1
        );
        assert!(terminal.capture_frame(None).is_ok());
    }

    #[test]
    fn frames_are_owned_and_carry_source_resize_palette_and_font_revisions() {
        let mut terminal = terminal();
        terminal.feed(b"one").unwrap();
        let first = terminal.capture_frame(None).unwrap();
        assert_eq!(&first[16..32], &[1; 16]);
        assert_eq!(&first[32..48], &[2; 16]);
        let old = first.clone();
        let mut size = terminal.config().size;
        size.cols = 16;
        terminal.resize(size).unwrap();
        terminal.set_font_revision(7).unwrap();
        terminal.feed(b"\x1b]10;rgb:ff/00/00\x07two").unwrap();
        let second = terminal.capture_frame(None).unwrap();
        assert_eq!(first, old);
        assert_eq!(u64::from_le_bytes(second[64..72].try_into().unwrap()), 2);
        assert_eq!(u64::from_le_bytes(second[80..88].try_into().unwrap()), 7);
        assert!(
            u64::from_le_bytes(second[72..80].try_into().unwrap())
                > u64::from_le_bytes(first[72..80].try_into().unwrap())
        );
        assert_eq!(
            terminal.set_font_revision(6),
            Err(NativeTerminalError::Stale)
        );
    }

    #[test]
    fn base_palette_is_the_configured_theme_under_transient_osc_overrides() {
        let mut terminal = terminal();
        let entry = |frame: &[u8], index: usize| -> [u8; 4] {
            let at = u32::from_le_bytes(frame[152..156].try_into().unwrap()) as usize + index * 4;
            frame[at..at + 4].try_into().unwrap()
        };
        let revision = |frame: &[u8]| u64::from_le_bytes(frame[72..80].try_into().unwrap());
        let background = [0x14, 0x21, 0x3d, 0xff];
        let mut theme = [9u8, 9, 9, 0xff].repeat(BASE_PALETTE_COLORS);
        theme[4..8].copy_from_slice(&[0xcc, 0x33, 0x33, 0xff]);
        theme[200 * 4..201 * 4].copy_from_slice(&[1, 2, 3, 0xff]);
        theme[257 * 4..258 * 4].copy_from_slice(&background);
        let before = terminal.capture_frame(None).unwrap();
        assert_ne!(entry(&before, 257), background);

        terminal.set_base_palette(&theme).unwrap();
        let themed = terminal.capture_frame(None).unwrap();
        assert!(revision(&themed) > revision(&before));
        assert_eq!(entry(&themed, 1), [0xcc, 0x33, 0x33, 0xff]);
        assert_eq!(entry(&themed, 200), [1, 2, 3, 0xff]);
        assert_eq!(entry(&themed, 256), [9, 9, 9, 0xff]);
        assert_eq!(entry(&themed, 257), background);
        assert_eq!(entry(&themed, 262), [9, 9, 9, 0xff]);

        // Applications querying the background see the theme.
        terminal.feed(b"\x1b]11;?\x07").unwrap();
        let mut bytes = [0u8; 128];
        let reply = terminal.copy_reply(&mut bytes).unwrap().unwrap();
        let text = String::from_utf8_lossy(&bytes[..reply.bytes]).into_owned();
        assert!(text.contains("rgb:1414/2121/3d3d"), "{:?}", text);
        terminal
            .settle_reply(reply.sequence, BoundedReplyDisposition::Accepted)
            .unwrap();

        // An OSC override is transient; its reset returns to the theme.
        terminal.feed(b"\x1b]11;rgb:00/00/00\x07").unwrap();
        let overridden = terminal.capture_frame(None).unwrap();
        assert_eq!(entry(&overridden, 257), [0, 0, 0, 0xff]);
        terminal.feed(b"\x1b]111\x07").unwrap();
        let reset = terminal.capture_frame(None).unwrap();
        assert_eq!(entry(&reset, 257), background);

        assert_eq!(
            terminal.set_base_palette(&theme[..BASE_PALETTE_BYTES - 1]),
            Err(NativeTerminalError::Invalid)
        );
    }

    #[test]
    fn mux_palette_is_transient_and_survives_geometry_configuration_reinstallation() {
        let mut options = config();
        options.scrollback_rows = 256;
        let mut terminal = NativeTerminal::new(options).unwrap();
        let local = [10u8, 20, 30, 255].repeat(BASE_PALETTE_COLORS);
        let newer_local = [40u8, 50, 60, 255].repeat(BASE_PALETTE_COLORS);
        let mut remote = [70u8, 80, 90, 100].repeat(BASE_PALETTE_COLORS);
        remote[258 * 4..259 * 4].copy_from_slice(&[255, 0, 128, 64]);
        remote[260 * 4..261 * 4].copy_from_slice(&[0, 255, 0, 255]);
        terminal.set_base_palette(&local).unwrap();
        let revision = terminal.witness.palette_revision;
        terminal.set_mux_palette(&remote).unwrap();
        assert!(terminal.witness.palette_revision > revision);
        let projected = terminal.terminal.palette();
        assert_eq!(
            projected.cursor_fg,
            SrgbaTuple(1.0, 0.0, 128.0 / 255.0, 64.0 / 255.0)
        );
        assert_eq!(projected.cursor_border, SrgbaTuple(0.0, 1.0, 0.0, 1.0));
        terminal.set_base_palette(&newer_local).unwrap();
        assert_eq!(
            terminal.terminal.palette(),
            projected,
            "local preference does not erase remote state"
        );
        let before = terminal.retained_scrollback_rows();
        terminal
            .resize(TerminalSize {
                rows: 128,
                cols: 200,
                ..TerminalSize::default()
            })
            .unwrap();
        assert!(terminal.retained_scrollback_rows() < before);
        assert_eq!(terminal.terminal.palette(), projected);
        terminal.feed(b"\x1bc").unwrap();
        assert_eq!(
            terminal.terminal.palette(),
            decode_native_palette(&newer_local).unwrap()
        );
    }

    #[test]
    fn invalid_mux_palette_does_not_mutate_color_or_frame_authority() {
        let mut terminal = terminal();
        let original = terminal.terminal.palette();
        let revision = terminal.witness.palette_revision;
        let sequence = terminal.terminal.current_seqno();
        for length in [0, BASE_PALETTE_BYTES - 1, BASE_PALETTE_BYTES + 1] {
            assert_eq!(
                terminal.set_mux_palette(&vec![0; length]),
                Err(NativeTerminalError::Invalid)
            );
            assert_eq!(terminal.terminal.palette(), original);
            assert_eq!(terminal.witness.palette_revision, revision);
            assert_eq!(terminal.terminal.current_seqno(), sequence);
        }
    }

    #[test]
    fn switching_screens_advances_the_geometry_revision_and_other_output_does_not() {
        let mut terminal = terminal();
        let flags = |frame: &[u8]| u32::from_le_bytes(frame[12..16].try_into().unwrap());
        let geometry = |frame: &[u8]| u64::from_le_bytes(frame[64..72].try_into().unwrap());
        terminal.feed(b"primary").unwrap();
        let primary = terminal.capture_frame(None).unwrap();
        terminal.feed(b"\x1b[?1049h").unwrap();
        let alternate = terminal.capture_frame(None).unwrap();
        assert_eq!(flags(&primary) & 4, 0);
        assert_eq!(flags(&alternate) & 4, 4);
        assert!(geometry(&alternate) > geometry(&primary));
        terminal.feed(b"full screen").unwrap();
        let drawn = terminal.capture_frame(None).unwrap();
        assert_eq!(geometry(&drawn), geometry(&alternate));
        terminal.feed(b"\x1b[?1049l").unwrap();
        let restored = terminal.capture_frame(None).unwrap();
        assert_eq!(flags(&restored) & 4, 0);
        assert!(geometry(&restored) > geometry(&drawn));
        terminal.feed(b"\x1b[?1049h\x1b[?1049l").unwrap();
        let round_trip = terminal.capture_frame(None).unwrap();
        assert_eq!(geometry(&round_trip), geometry(&restored));
    }

    #[test]
    fn geometry_accounts_for_both_screens_and_keeps_the_history_that_fits() {
        let mut config = config();
        config.size.rows = 256;
        config.size.cols = 256;
        config.scrollback_rows = 0;
        assert!(matches!(
            NativeTerminal::new(config),
            Err(NativeTerminalError::Capacity)
        ));
        config.size.rows = 128;
        assert!(NativeTerminal::new(config).is_ok());
        // Both screens fill the budget exactly: no history fits beside them.
        config.scrollback_rows = 1;
        let terminal = NativeTerminal::new(config).unwrap();
        assert_eq!(terminal.retained_scrollback_rows(), 0);
        assert_eq!(config.resident_cell_budget(), Ok(MAX_RESIDENT_CELLS));
    }

    #[test]
    fn a_grid_too_large_for_its_history_keeps_the_history_that_fits() {
        let mut terminal = terminal();
        let row =
            |frame: &[u8], at: usize| i64::from_le_bytes(frame[at..at + 8].try_into().unwrap());
        let history = |frame: &[u8]| (row(frame, 176), row(frame, 184));
        terminal.feed(b"\r\nx".repeat(20).as_slice()).unwrap();
        assert_eq!(history(&terminal.capture_frame(None).unwrap()), (10, 18));

        // 100 rows of 320 columns: (200 + 8) * 320 cells would exceed the
        // budget, which leaves room for 4 history rows beside both screens.
        let large = TerminalSize {
            rows: 100,
            cols: 320,
            ..TerminalSize::default()
        };
        terminal.resize(large).unwrap();
        assert_eq!(terminal.retained_scrollback_rows(), 4);
        assert!(terminal.terminal.screen().scrollback_rows() <= 104);
        let (first, live) = history(&terminal.capture_frame(None).unwrap());
        assert!(live - first <= 4, "{:?}", (first, live));
        assert_eq!(
            terminal.config().resident_cell_budget(),
            Ok((200 + 4) * 320)
        );

        // A new theme keeps the smaller history, which refills only to it.
        terminal.set_base_palette(&[9; BASE_PALETTE_BYTES]).unwrap();
        terminal.feed(b"\r\ny".repeat(200).as_slice()).unwrap();
        let (first, live) = history(&terminal.capture_frame(None).unwrap());
        assert_eq!(live - first, 4);
        assert!(terminal.terminal.screen().scrollback_rows() <= 104);

        // A smaller grid raises the limit again.
        terminal
            .resize(TerminalSize {
                rows: 3,
                cols: 12,
                ..TerminalSize::default()
            })
            .unwrap();
        assert_eq!(terminal.retained_scrollback_rows(), 8);
        // Two screens alone over the budget are refused, changing nothing.
        assert_eq!(
            terminal.resize(TerminalSize {
                rows: 256,
                cols: 256,
                ..TerminalSize::default()
            }),
            Err(NativeTerminalError::Capacity)
        );
        assert_eq!(terminal.config().size.rows, 3);
        assert_eq!(terminal.retained_scrollback_rows(), 8);
    }

    #[test]
    fn frames_report_the_resident_history_range_a_viewport_may_select() {
        let mut terminal = terminal();
        let flags = |frame: &[u8]| u32::from_le_bytes(frame[12..16].try_into().unwrap());
        let row =
            |frame: &[u8], at: usize| i64::from_le_bytes(frame[at..at + 8].try_into().unwrap());
        let history = |frame: &[u8]| (row(frame, 176), row(frame, 184));
        let fresh = terminal.capture_frame(None).unwrap();
        assert_eq!(flags(&fresh) & 16, 16);
        assert_eq!(history(&fresh), (0, 0));

        terminal.feed(b"1\r\n2\r\n3\r\n4\r\n5\r\n6").unwrap();
        let live = terminal.capture_frame(None).unwrap();
        assert_eq!(history(&live), (0, 3), "rows 1..3 scrolled into history");
        assert_eq!(row(&live, 104), 3);
        let oldest = terminal.capture_frame(Some(0)).unwrap();
        assert_eq!((row(&oldest, 104), history(&oldest)), (0, (0, 3)));
        assert!(
            terminal.capture_frame(Some(4)).is_err(),
            "past the live viewport"
        );

        // Eight rows of scrollback keep 11 of the 26 rows; the range follows.
        terminal.feed(b"\r\nx".repeat(20).as_slice()).unwrap();
        let evicted = terminal.capture_frame(None).unwrap();
        assert_eq!(history(&evicted), (15, 23));
        assert_eq!(
            terminal.capture_frame(Some(14)),
            Err(NativeTerminalError::ViewportUnavailable)
        );
        assert_eq!(row(&terminal.capture_frame(Some(15)).unwrap(), 104), 15);

        // The alternate screen has no history to select.
        terminal.feed(b"\x1b[?1049h").unwrap();
        let alternate = terminal.capture_frame(None).unwrap();
        let (first, live) = history(&alternate);
        assert_eq!(first, live);
        assert_eq!(row(&alternate, 104), live);
    }

    #[test]
    fn evicted_history_viewport_is_unavailable_without_stopping_live_output_or_replies() {
        let mut terminal = terminal();
        terminal.capture_frame(Some(0)).unwrap();
        terminal.feed(b"row\r\n".repeat(32).as_slice()).unwrap();
        assert_eq!(
            terminal.capture_frame(Some(0)),
            Err(NativeTerminalError::ViewportUnavailable)
        );
        terminal.feed(b"live\x1b[6n").unwrap();
        let reply = terminal
            .copy_reply(&mut [0; MAX_REPLY_BYTES])
            .unwrap()
            .unwrap();
        terminal
            .settle_reply(reply.sequence, BoundedReplyDisposition::Accepted)
            .unwrap();
        assert!(terminal.capture_frame(None).is_ok());
    }
}
