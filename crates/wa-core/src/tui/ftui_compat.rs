//! Temporary compatibility adapter for TUI backend migration.
//!
//! This module defines framework-agnostic types and traits that serve as the
//! boundary between view/state logic and the rendering backend. During the
//! FTUI migration period:
//!
//! - Under the `tui` feature: these types adapt to ratatui/crossterm primitives.
//! - Under the `ftui` feature: these types adapt to ftui primitives.
//!
//! Views should incrementally migrate from calling ratatui APIs directly to
//! programming against these adapter types instead. Once all views are ported
//! to native ftui rendering (FTUI-09.3), this entire module is deleted.
//!
//! # Deletion criteria
//!
//! Each type/trait documents its own deletion criterion inline. The module as a
//! whole can be removed when:
//!
//! 1. All views render via `ftui::Frame` directly.
//! 2. The `tui` feature flag and ratatui dependency are removed.
//! 3. No code outside this module references these adapter types.

// ---------------------------------------------------------------------------
// Area — framework-agnostic layout rectangle
// ---------------------------------------------------------------------------

/// Framework-agnostic screen rectangle for layout calculations.
///
/// Maps to `ratatui::layout::Rect` under `tui` and to the equivalent ftui
/// buffer bounds under `ftui`.
///
/// # Deletion criterion
/// Remove when views use `ftui::layout` primitives directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Area {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Area {
    #[must_use]
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Split into top (height `n`) and remainder.
    #[must_use]
    pub const fn split_top(&self, n: u16) -> (Self, Self) {
        let top_h = if n > self.height { self.height } else { n };
        let top = Self::new(self.x, self.y, self.width, top_h);
        let bot = Self::new(self.x, self.y + top_h, self.width, self.height - top_h);
        (top, bot)
    }
}

#[cfg(feature = "tui")]
impl From<ratatui::layout::Rect> for Area {
    fn from(r: ratatui::layout::Rect) -> Self {
        Self {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        }
    }
}

#[cfg(feature = "tui")]
impl From<Area> for ratatui::layout::Rect {
    fn from(a: Area) -> Self {
        Self {
            x: a.x,
            y: a.y,
            width: a.width,
            height: a.height,
        }
    }
}

// ---------------------------------------------------------------------------
// ColorSpec / StyleSpec — framework-agnostic styling
// ---------------------------------------------------------------------------

/// Framework-agnostic color specification.
///
/// # Deletion criterion
/// Remove when views use `ftui::Style` / `ftui::Color` directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpec {
    Reset,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
    Rgb(u8, u8, u8),
}

#[cfg(feature = "tui")]
impl From<ColorSpec> for ratatui::style::Color {
    fn from(c: ColorSpec) -> Self {
        match c {
            ColorSpec::Reset => Self::Reset,
            ColorSpec::Black => Self::Black,
            ColorSpec::Red => Self::Red,
            ColorSpec::Green => Self::Green,
            ColorSpec::Yellow => Self::Yellow,
            ColorSpec::Blue => Self::Blue,
            ColorSpec::Magenta => Self::Magenta,
            ColorSpec::Cyan => Self::Cyan,
            ColorSpec::Gray => Self::Gray,
            ColorSpec::DarkGray => Self::DarkGray,
            ColorSpec::LightRed => Self::LightRed,
            ColorSpec::LightGreen => Self::LightGreen,
            ColorSpec::LightYellow => Self::LightYellow,
            ColorSpec::LightBlue => Self::LightBlue,
            ColorSpec::LightMagenta => Self::LightMagenta,
            ColorSpec::LightCyan => Self::LightCyan,
            ColorSpec::White => Self::White,
            ColorSpec::Rgb(r, g, b) => Self::Rgb(r, g, b),
        }
    }
}

/// Framework-agnostic text style.
///
/// # Deletion criterion
/// Remove when views use `ftui::Style` directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StyleSpec {
    pub fg: Option<ColorSpec>,
    pub bg: Option<ColorSpec>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reversed: bool,
}

impl StyleSpec {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            fg: None,
            bg: None,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            reversed: false,
        }
    }

    #[must_use]
    pub const fn fg(mut self, color: ColorSpec) -> Self {
        self.fg = Some(color);
        self
    }

    #[must_use]
    pub const fn bg(mut self, color: ColorSpec) -> Self {
        self.bg = Some(color);
        self
    }

    #[must_use]
    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    #[must_use]
    pub const fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    #[must_use]
    pub const fn reversed(mut self) -> Self {
        self.reversed = true;
        self
    }
}

#[cfg(feature = "tui")]
impl From<StyleSpec> for ratatui::style::Style {
    fn from(s: StyleSpec) -> Self {
        use ratatui::style::Modifier;
        let mut style = Self::default();
        if let Some(fg) = s.fg {
            style = style.fg(fg.into());
        }
        if let Some(bg) = s.bg {
            style = style.bg(bg.into());
        }
        let mut mods = Modifier::empty();
        if s.bold {
            mods |= Modifier::BOLD;
        }
        if s.dim {
            mods |= Modifier::DIM;
        }
        if s.italic {
            mods |= Modifier::ITALIC;
        }
        if s.underline {
            mods |= Modifier::UNDERLINED;
        }
        if s.reversed {
            mods |= Modifier::REVERSED;
        }
        style.add_modifier(mods)
    }
}

// ---------------------------------------------------------------------------
// InputEvent / KeyInput — framework-agnostic terminal input
// ---------------------------------------------------------------------------

/// Normalized terminal input event.
///
/// Maps crossterm events (under `tui`) and ftui events (under `ftui`) into a
/// common representation that the application layer handles.
///
/// # Deletion criterion
/// Remove when the event loop uses `ftui::Event` / `ftui::Model::update` directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    Key(KeyInput),
    Resize { width: u16, height: u16 },
    Tick,
}

/// Normalized key press.
///
/// # Deletion criterion
/// Remove when keybinding code uses `ftui::KeyEvent` directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInput {
    pub key: Key,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl KeyInput {
    #[must_use]
    pub const fn new(key: Key) -> Self {
        Self {
            key,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    #[must_use]
    pub fn is_char(&self, c: char) -> bool {
        matches!(self.key, Key::Char(ch) if ch == c)
    }
}

/// Framework-agnostic key codes.
///
/// Covers the common subset used by wa's TUI keybindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    F(u8),
}

#[cfg(feature = "tui")]
impl From<crossterm::event::KeyEvent> for KeyInput {
    fn from(ke: crossterm::event::KeyEvent) -> Self {
        let key = match ke.code {
            crossterm::event::KeyCode::Char(c) => Key::Char(c),
            crossterm::event::KeyCode::Enter => Key::Enter,
            crossterm::event::KeyCode::Esc => Key::Esc,
            crossterm::event::KeyCode::Tab => Key::Tab,
            crossterm::event::KeyCode::BackTab => Key::BackTab,
            crossterm::event::KeyCode::Backspace => Key::Backspace,
            crossterm::event::KeyCode::Up => Key::Up,
            crossterm::event::KeyCode::Down => Key::Down,
            crossterm::event::KeyCode::Left => Key::Left,
            crossterm::event::KeyCode::Right => Key::Right,
            crossterm::event::KeyCode::Home => Key::Home,
            crossterm::event::KeyCode::End => Key::End,
            crossterm::event::KeyCode::PageUp => Key::PageUp,
            crossterm::event::KeyCode::PageDown => Key::PageDown,
            crossterm::event::KeyCode::Delete => Key::Delete,
            crossterm::event::KeyCode::F(n) => Key::F(n),
            // Unmapped keys become Esc (ignored by keybinding handlers)
            _ => Key::Esc,
        };
        Self {
            key,
            ctrl: ke
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL),
            alt: ke.modifiers.contains(crossterm::event::KeyModifiers::ALT),
            shift: ke.modifiers.contains(crossterm::event::KeyModifiers::SHIFT),
        }
    }
}

#[cfg(feature = "ftui")]
impl From<ftui::KeyEvent> for KeyInput {
    fn from(ke: ftui::KeyEvent) -> Self {
        let key = match ke.code {
            ftui::KeyCode::Char(c) => Key::Char(c),
            ftui::KeyCode::Enter => Key::Enter,
            ftui::KeyCode::Escape => Key::Esc,
            ftui::KeyCode::Tab => Key::Tab,
            ftui::KeyCode::BackTab => Key::BackTab,
            ftui::KeyCode::Backspace => Key::Backspace,
            ftui::KeyCode::Up => Key::Up,
            ftui::KeyCode::Down => Key::Down,
            ftui::KeyCode::Left => Key::Left,
            ftui::KeyCode::Right => Key::Right,
            ftui::KeyCode::Home => Key::Home,
            ftui::KeyCode::End => Key::End,
            ftui::KeyCode::PageUp => Key::PageUp,
            ftui::KeyCode::PageDown => Key::PageDown,
            ftui::KeyCode::Delete => Key::Delete,
            ftui::KeyCode::F(n) => Key::F(n),
            _ => Key::Esc,
        };
        Self {
            key,
            ctrl: ke.modifiers.contains(ftui::Modifiers::CTRL),
            alt: ke.modifiers.contains(ftui::Modifiers::ALT),
            shift: ke.modifiers.contains(ftui::Modifiers::SHIFT),
        }
    }
}

#[cfg(feature = "ftui")]
impl From<ftui::Event> for InputEvent {
    fn from(ev: ftui::Event) -> Self {
        match ev {
            ftui::Event::Key(ke) => Self::Key(ke.into()),
            ftui::Event::Resize { width, height } => Self::Resize { width, height },
            ftui::Event::Tick => Self::Tick,
            // Mouse, paste, focus, clipboard → Tick (ignored for now)
            _ => Self::Tick,
        }
    }
}

// ---------------------------------------------------------------------------
// RenderSurface — the rendering boundary trait
// ---------------------------------------------------------------------------

/// The rendering boundary between view logic and the backend.
///
/// Views that are being migrated should implement their rendering in terms of
/// `RenderSurface` rather than calling `ratatui::Buffer` or `ftui::Frame`
/// methods directly. This allows the same view logic to work under either
/// backend.
///
/// The trait exposes a minimal API sufficient for wa's current views:
/// styled text output, line drawing, and area management.
///
/// # Deletion criterion
/// Remove when all views render via `ftui::Frame` natively and the `tui`
/// feature is dropped.
pub trait RenderSurface {
    /// Total area available for rendering.
    fn area(&self) -> Area;

    /// Write a styled string at the given position.
    ///
    /// Characters that would extend beyond the surface bounds are clipped.
    fn put_str(&mut self, x: u16, y: u16, text: &str, style: StyleSpec);

    /// Fill a rectangular region with a character and style.
    fn fill(&mut self, area: Area, ch: char, style: StyleSpec);

    /// Draw a horizontal line of a repeated character.
    fn hline(&mut self, x: u16, y: u16, width: u16, ch: char, style: StyleSpec) {
        for dx in 0..width {
            self.put_str(x + dx, y, &ch.to_string(), style);
        }
    }
}

#[cfg(feature = "tui")]
impl RenderSurface for RatatuiSurface<'_> {
    fn area(&self) -> Area {
        self.area.into()
    }

    fn put_str(&mut self, x: u16, y: u16, text: &str, style: StyleSpec) {
        let ratatui_style: ratatui::style::Style = style.into();
        let abs_x = self.area.x + x;
        let abs_y = self.area.y + y;
        if abs_y >= self.area.y + self.area.height {
            return;
        }
        // Write characters one at a time, clipping at width
        let max_x = self.area.x + self.area.width;
        let mut col = abs_x;
        for ch in text.chars() {
            if col >= max_x {
                break;
            }
            if let Some(cell) = self
                .buf
                .cell_mut(ratatui::layout::Position::new(col, abs_y))
            {
                cell.set_char(ch).set_style(ratatui_style);
            }
            col += 1;
        }
    }

    fn fill(&mut self, area: Area, ch: char, style: StyleSpec) {
        let ratatui_style: ratatui::style::Style = style.into();
        let rect: ratatui::layout::Rect = area.into();
        let clipped = rect.intersection(self.area);
        for y in clipped.y..clipped.y + clipped.height {
            for x in clipped.x..clipped.x + clipped.width {
                if let Some(cell) = self.buf.cell_mut(ratatui::layout::Position::new(x, y)) {
                    cell.set_char(ch).set_style(ratatui_style);
                }
            }
        }
    }
}

/// Wraps a ratatui `Buffer` + `Rect` as a `RenderSurface`.
///
/// # Deletion criterion
/// Remove when the `tui` feature is dropped.
#[cfg(feature = "tui")]
pub struct RatatuiSurface<'a> {
    pub buf: &'a mut ratatui::buffer::Buffer,
    pub area: ratatui::layout::Rect,
}

#[cfg(feature = "tui")]
impl<'a> RatatuiSurface<'a> {
    pub fn new(buf: &'a mut ratatui::buffer::Buffer, area: ratatui::layout::Rect) -> Self {
        Self { buf, area }
    }
}

// ---------------------------------------------------------------------------
// ScreenMode — framework-agnostic screen mode policy
// ---------------------------------------------------------------------------

/// Terminal screen mode for the TUI session.
///
/// Determines whether the UI takes over the full screen (alternate screen)
/// or renders inline within scrollback. The mode also governs scrollback
/// preservation, subprocess output visibility, and cleanup behavior.
///
/// # Mode policy (FTUI-03.3)
///
/// | Context               | Mode         | Rationale                              |
/// |-----------------------|--------------|----------------------------------------|
/// | Interactive TUI       | `AltScreen`  | Full-screen UI, clean layout, Esc exit |
/// | Agent harness / daemon| `Inline`     | Scrollback preserved for log tailing   |
/// | Command handoff       | N/A (suspend)| Session suspends, mode doesn't change  |
///
/// # Scrollback safety
///
/// - `AltScreen`: scrollback is invisible while active; restored on leave.
///   Content written to stdout during alt-screen is lost.
/// - `Inline`: scrollback is preserved. UI occupies a fixed region at the
///   bottom (or top) and logs scroll above it. On exit, the UI region is
///   erased but log content remains visible.
/// - `InlineAuto`: like `Inline`, but the UI height adapts to content within
///   the configured min/max bounds.
///
/// # Deletion criterion
/// Remove when views use `ftui::ScreenMode` directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenMode {
    /// Full-screen alternate screen. Standard TUI mode.
    ///
    /// - Scrollback hidden while active, restored on exit.
    /// - Best for: interactive dashboards, multi-view navigation.
    AltScreen,

    /// Inline mode with fixed UI height.
    ///
    /// - Scrollback preserved; UI pinned at terminal bottom.
    /// - Best for: agent harness, daemon monitoring, status panels.
    Inline {
        /// Height of the UI region in terminal rows.
        ui_height: u16,
    },

    /// Inline mode with auto-sizing UI region.
    ///
    /// - UI height adapts to rendered content within bounds.
    /// - Best for: variable-height status displays.
    InlineAuto {
        /// Minimum UI height in rows.
        min_height: u16,
        /// Maximum UI height in rows.
        max_height: u16,
    },
}

impl Default for ScreenMode {
    fn default() -> Self {
        Self::AltScreen
    }
}

impl ScreenMode {
    /// Returns `true` if this mode preserves terminal scrollback.
    #[must_use]
    pub const fn preserves_scrollback(&self) -> bool {
        !matches!(self, Self::AltScreen)
    }

    /// Returns `true` if this mode uses the alternate screen buffer.
    #[must_use]
    pub const fn is_alt_screen(&self) -> bool {
        matches!(self, Self::AltScreen)
    }
}

#[cfg(feature = "ftui")]
impl From<ScreenMode> for ftui::ScreenMode {
    fn from(m: ScreenMode) -> Self {
        match m {
            ScreenMode::AltScreen => Self::AltScreen,
            ScreenMode::Inline { ui_height } => Self::Inline { ui_height },
            ScreenMode::InlineAuto {
                min_height,
                max_height,
            } => Self::InlineAuto {
                min_height,
                max_height,
            },
        }
    }
}

#[cfg(feature = "ftui")]
impl From<ftui::ScreenMode> for ScreenMode {
    fn from(m: ftui::ScreenMode) -> Self {
        match m {
            ftui::ScreenMode::AltScreen => Self::AltScreen,
            ftui::ScreenMode::Inline { ui_height } => Self::Inline { ui_height },
            ftui::ScreenMode::InlineAuto {
                min_height,
                max_height,
            } => Self::InlineAuto {
                min_height,
                max_height,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Migration markers
// ---------------------------------------------------------------------------

/// Marker macro for legacy ratatui callsites that need migration.
///
/// Usage: `legacy_ratatui!("render_home_view: status bar layout");`
///
/// During compilation with the `ftui` feature, this emits a compile-time note
/// so remaining migration sites are visible in build output.
///
/// # Deletion criterion
/// Remove when no callsites remain (FTUI-09.3).
// The macro and re-export are intentionally unused until views begin
// migrating.  Suppress the warnings so they don't pollute CI output.
#[allow(unused_macros)]
#[cfg(feature = "ftui")]
macro_rules! legacy_ratatui {
    ($site:expr) => {
        // Intentionally empty under ftui — the ftui build should not contain
        // any legacy_ratatui! invocations once migration is complete.
        // During the transition, add compile_warning! when stabilised, or
        // rely on grep for "legacy_ratatui!" to find remaining sites.
    };
}

#[allow(unused_macros)]
#[cfg(not(feature = "ftui"))]
macro_rules! legacy_ratatui {
    ($site:expr) => {};
}

#[allow(unused_imports)]
pub(crate) use legacy_ratatui;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn area_split_top() {
        let area = Area::new(0, 0, 80, 24);
        let (top, bot) = area.split_top(3);
        assert_eq!(top, Area::new(0, 0, 80, 3));
        assert_eq!(bot, Area::new(0, 3, 80, 21));
    }

    #[test]
    fn area_split_top_overflow() {
        let area = Area::new(0, 0, 80, 5);
        let (top, bot) = area.split_top(10);
        assert_eq!(top, Area::new(0, 0, 80, 5));
        assert_eq!(bot, Area::new(0, 5, 80, 0));
        assert!(bot.is_empty());
    }

    #[test]
    fn area_default_is_empty() {
        assert!(Area::default().is_empty());
    }

    #[test]
    fn style_spec_builder() {
        let s = StyleSpec::new()
            .fg(ColorSpec::Green)
            .bg(ColorSpec::Black)
            .bold()
            .dim();
        assert_eq!(s.fg, Some(ColorSpec::Green));
        assert_eq!(s.bg, Some(ColorSpec::Black));
        assert!(s.bold);
        assert!(s.dim);
        assert!(!s.italic);
    }

    #[test]
    fn key_input_is_char() {
        let ki = KeyInput::new(Key::Char('q'));
        assert!(ki.is_char('q'));
        assert!(!ki.is_char('x'));
    }

    #[test]
    fn key_input_modifiers() {
        let ki = KeyInput {
            key: Key::Char('c'),
            ctrl: true,
            alt: false,
            shift: false,
        };
        assert!(ki.ctrl);
        assert!(!ki.alt);
    }

    #[test]
    fn input_event_variants() {
        let k = InputEvent::Key(KeyInput::new(Key::Enter));
        assert!(matches!(k, InputEvent::Key(_)));

        let r = InputEvent::Resize {
            width: 80,
            height: 24,
        };
        assert!(matches!(r, InputEvent::Resize { .. }));

        let t = InputEvent::Tick;
        assert!(matches!(t, InputEvent::Tick));
    }

    #[test]
    fn screen_mode_default_is_alt_screen() {
        assert_eq!(ScreenMode::default(), ScreenMode::AltScreen);
    }

    #[test]
    fn screen_mode_scrollback_preservation() {
        assert!(!ScreenMode::AltScreen.preserves_scrollback());
        assert!(ScreenMode::Inline { ui_height: 10 }.preserves_scrollback());
        assert!(
            ScreenMode::InlineAuto {
                min_height: 5,
                max_height: 20
            }
            .preserves_scrollback()
        );
    }

    #[test]
    fn screen_mode_is_alt_screen() {
        assert!(ScreenMode::AltScreen.is_alt_screen());
        assert!(!ScreenMode::Inline { ui_height: 10 }.is_alt_screen());
    }

    // -- FTUI-07.1 gap-fill tests --

    #[test]
    fn area_non_empty() {
        let a = Area::new(5, 10, 80, 24);
        assert!(!a.is_empty());
        assert_eq!(a.x, 5);
        assert_eq!(a.y, 10);
    }

    #[test]
    fn area_zero_width_is_empty() {
        let a = Area::new(0, 0, 0, 24);
        assert!(a.is_empty());
    }

    #[test]
    fn area_split_top_zero() {
        let area = Area::new(0, 0, 80, 24);
        let (top, bot) = area.split_top(0);
        assert!(top.is_empty());
        assert_eq!(bot, area);
    }

    #[test]
    fn style_spec_reversed_builder() {
        let s = StyleSpec::new().fg(ColorSpec::Red).reversed();
        assert!(s.reversed);
        assert!(!s.bold);
        assert_eq!(s.fg, Some(ColorSpec::Red));
    }

    #[test]
    fn style_spec_default_all_false() {
        let s = StyleSpec::default();
        assert!(s.fg.is_none());
        assert!(s.bg.is_none());
        assert!(!s.bold);
        assert!(!s.dim);
        assert!(!s.italic);
        assert!(!s.underline);
        assert!(!s.reversed);
    }

    #[test]
    fn style_spec_bg_only() {
        let s = StyleSpec::new().bg(ColorSpec::Cyan);
        assert!(s.fg.is_none());
        assert_eq!(s.bg, Some(ColorSpec::Cyan));
    }

    #[test]
    fn color_spec_rgb_equality() {
        assert_eq!(ColorSpec::Rgb(255, 0, 128), ColorSpec::Rgb(255, 0, 128));
        assert_ne!(ColorSpec::Rgb(255, 0, 128), ColorSpec::Rgb(0, 0, 128));
    }

    #[test]
    fn key_input_new_no_modifiers() {
        let ki = KeyInput::new(Key::Enter);
        assert!(!ki.ctrl);
        assert!(!ki.alt);
        assert!(!ki.shift);
        assert_eq!(ki.key, Key::Enter);
    }

    #[test]
    fn key_input_is_char_non_char_key() {
        let ki = KeyInput::new(Key::Esc);
        assert!(!ki.is_char('q'));
    }

    #[test]
    fn key_input_all_modifiers() {
        let ki = KeyInput {
            key: Key::Char('a'),
            ctrl: true,
            alt: true,
            shift: true,
        };
        assert!(ki.ctrl);
        assert!(ki.alt);
        assert!(ki.shift);
        assert!(ki.is_char('a'));
    }

    #[test]
    fn input_event_resize_fields() {
        let ev = InputEvent::Resize {
            width: 120,
            height: 40,
        };
        if let InputEvent::Resize { width, height } = ev {
            assert_eq!(width, 120);
            assert_eq!(height, 40);
        } else {
            panic!("Expected Resize variant");
        }
    }

    #[test]
    fn key_enum_f_key() {
        let ki = KeyInput::new(Key::F(5));
        assert_eq!(ki.key, Key::F(5));
        assert!(!ki.is_char('5'));
    }

    #[test]
    fn screen_mode_inline_auto_is_not_alt() {
        let mode = ScreenMode::InlineAuto {
            min_height: 5,
            max_height: 20,
        };
        assert!(!mode.is_alt_screen());
        assert!(mode.preserves_scrollback());
    }

    #[cfg(feature = "tui")]
    mod ratatui_compat {
        use super::*;

        #[test]
        fn area_roundtrip() {
            let orig = ratatui::layout::Rect::new(5, 10, 80, 24);
            let compat: Area = orig.into();
            let back: ratatui::layout::Rect = compat.into();
            assert_eq!(orig, back);
        }

        #[test]
        fn color_spec_to_ratatui() {
            let c: ratatui::style::Color = ColorSpec::Rgb(255, 0, 128).into();
            assert_eq!(c, ratatui::style::Color::Rgb(255, 0, 128));
        }

        #[test]
        fn style_spec_to_ratatui() {
            let s = StyleSpec::new().fg(ColorSpec::Red).bold().reversed();
            let rs: ratatui::style::Style = s.into();
            assert_eq!(rs.fg, Some(ratatui::style::Color::Red));
        }

        #[test]
        fn crossterm_key_to_key_input() {
            let ce = crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('j'),
                crossterm::event::KeyModifiers::CONTROL,
            );
            let ki: KeyInput = ce.into();
            assert!(ki.is_char('j'));
            assert!(ki.ctrl);
            assert!(!ki.alt);
        }
    }
}
