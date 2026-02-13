//! Colors for attributes

pub use frankenterm_cell::color::{AnsiColor, ColorAttribute, RgbColor, SrgbaTuple};
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::result::Result;

#[derive(Clone, PartialEq)]
pub struct Palette256(pub [SrgbaTuple; 256]);

#[cfg(feature = "use_serde")]
impl Serialize for Palette256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.to_vec().serialize(serializer)
    }
}

#[cfg(feature = "use_serde")]
impl<'de> Deserialize<'de> for Palette256 {
    fn deserialize<D>(deserializer: D) -> Result<Palette256, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = Vec::<SrgbaTuple>::deserialize(deserializer)?;
        use std::convert::TryInto;
        Ok(Self(s.try_into().map_err(|_| {
            serde::de::Error::custom("Palette256 size mismatch")
        })?))
    }
}

impl std::iter::FromIterator<SrgbaTuple> for Palette256 {
    fn from_iter<I: IntoIterator<Item = SrgbaTuple>>(iter: I) -> Self {
        let mut colors = [SrgbaTuple::default(); 256];
        for (s, d) in iter.into_iter().zip(colors.iter_mut()) {
            *d = s;
        }
        Self(colors)
    }
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub struct ColorPalette {
    pub colors: Palette256,
    pub foreground: SrgbaTuple,
    pub background: SrgbaTuple,
    pub cursor_fg: SrgbaTuple,
    pub cursor_bg: SrgbaTuple,
    pub cursor_border: SrgbaTuple,
    pub selection_fg: SrgbaTuple,
    pub selection_bg: SrgbaTuple,
    pub scrollbar_thumb: SrgbaTuple,
    pub split: SrgbaTuple,
}

impl fmt::Debug for Palette256 {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        // If we wanted to dump all of the entries, we'd use this:
        // self.0[..].fmt(fmt)
        // However, we typically don't care about those and we're interested
        // in the Debug-ability of ColorPalette that embeds us.
        write!(fmt, "[suppressed]")
    }
}

impl ColorPalette {
    pub fn resolve_fg(&self, color: ColorAttribute) -> SrgbaTuple {
        match color {
            ColorAttribute::Default => self.foreground,
            ColorAttribute::PaletteIndex(idx) => self.colors.0[idx as usize],
            ColorAttribute::TrueColorWithPaletteFallback(color, _)
            | ColorAttribute::TrueColorWithDefaultFallback(color) => color.into(),
        }
    }
    pub fn resolve_bg(&self, color: ColorAttribute) -> SrgbaTuple {
        match color {
            ColorAttribute::Default => self.background,
            ColorAttribute::PaletteIndex(idx) => self.colors.0[idx as usize],
            ColorAttribute::TrueColorWithPaletteFallback(color, _)
            | ColorAttribute::TrueColorWithDefaultFallback(color) => color.into(),
        }
    }
}

lazy_static::lazy_static! {
    static ref DEFAULT_PALETTE: ColorPalette = ColorPalette::compute_default();
}

impl Default for ColorPalette {
    /// Construct a default color palette
    fn default() -> ColorPalette {
        DEFAULT_PALETTE.clone()
    }
}

impl ColorPalette {
    fn compute_default() -> Self {
        let mut colors = [SrgbaTuple::default(); 256];

        // The XTerm ansi color set
        let ansi: [SrgbaTuple; 16] = [
            // Black
            RgbColor::new_8bpc(0x00, 0x00, 0x00).into(),
            // Maroon
            RgbColor::new_8bpc(0xcc, 0x55, 0x55).into(),
            // Green
            RgbColor::new_8bpc(0x55, 0xcc, 0x55).into(),
            // Olive
            RgbColor::new_8bpc(0xcd, 0xcd, 0x55).into(),
            // Navy
            RgbColor::new_8bpc(0x54, 0x55, 0xcb).into(),
            // Purple
            RgbColor::new_8bpc(0xcc, 0x55, 0xcc).into(),
            // Teal
            RgbColor::new_8bpc(0x7a, 0xca, 0xca).into(),
            // Silver
            RgbColor::new_8bpc(0xcc, 0xcc, 0xcc).into(),
            // Grey
            RgbColor::new_8bpc(0x55, 0x55, 0x55).into(),
            // Red
            RgbColor::new_8bpc(0xff, 0x55, 0x55).into(),
            // Lime
            RgbColor::new_8bpc(0x55, 0xff, 0x55).into(),
            // Yellow
            RgbColor::new_8bpc(0xff, 0xff, 0x55).into(),
            // Blue
            RgbColor::new_8bpc(0x55, 0x55, 0xff).into(),
            // Fuchsia
            RgbColor::new_8bpc(0xff, 0x55, 0xff).into(),
            // Aqua
            RgbColor::new_8bpc(0x55, 0xff, 0xff).into(),
            // White
            RgbColor::new_8bpc(0xff, 0xff, 0xff).into(),
        ];

        colors[0..16].copy_from_slice(&ansi);

        // 216 color cube.
        // This isn't the perfect color cube, but it matches the values used
        // by xterm, which are slightly brighter.
        static RAMP6: [u8; 6] = [0, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
        for idx in 0..216 {
            let blue = RAMP6[idx % 6];
            let green = RAMP6[idx / 6 % 6];
            let red = RAMP6[idx / 6 / 6 % 6];

            colors[16 + idx] = RgbColor::new_8bpc(red, green, blue).into();
        }

        // 24 grey scales
        static GREYS: [u8; 24] = [
            0x08, 0x12, 0x1c, 0x26, 0x30, 0x3a, 0x44, 0x4e, 0x58, 0x62, 0x6c, 0x76, 0x80, 0x8a,
            0x94, 0x9e, 0xa8, 0xb2, /* Grey70 */
            0xbc, 0xc6, 0xd0, 0xda, 0xe4, 0xee,
        ];

        for idx in 0..24 {
            let grey = GREYS[idx];
            colors[232 + idx] = RgbColor::new_8bpc(grey, grey, grey).into();
        }

        let foreground = colors[249]; // Grey70
        let background = colors[AnsiColor::Black as usize];

        let cursor_bg = RgbColor::new_8bpc(0x52, 0xad, 0x70).into();
        let cursor_border = RgbColor::new_8bpc(0x52, 0xad, 0x70).into();
        let cursor_fg = colors[AnsiColor::Black as usize].into();

        let selection_fg = SrgbaTuple(0., 0., 0., 0.);
        let selection_bg = SrgbaTuple(0.5, 0.4, 0.6, 0.5);

        let scrollbar_thumb = RgbColor::new_8bpc(0x22, 0x22, 0x22).into();
        let split = RgbColor::new_8bpc(0x44, 0x44, 0x44).into();

        ColorPalette {
            colors: Palette256(colors),
            foreground,
            background,
            cursor_fg,
            cursor_bg,
            cursor_border,
            selection_fg,
            selection_bg,
            scrollbar_thumb,
            split,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Palette256 ──────────────────────────────────────────

    #[test]
    fn palette256_debug_is_suppressed() {
        let palette = Palette256([SrgbaTuple::default(); 256]);
        assert_eq!(format!("{palette:?}"), "[suppressed]");
    }

    #[test]
    fn palette256_clone_eq() {
        let a = Palette256([SrgbaTuple::default(); 256]);
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn palette256_from_iterator() {
        let colors = (0..256).map(|i| {
            let v = i as f32 / 255.0;
            SrgbaTuple(v, v, v, 1.0)
        });
        let palette: Palette256 = colors.collect();
        // First entry should be black
        assert_eq!(palette.0[0], SrgbaTuple(0.0, 0.0, 0.0, 1.0));
        // Last entry should be white
        let last = palette.0[255];
        assert!((last.0 - 1.0).abs() < 0.01);
    }

    // ── ColorPalette defaults ───────────────────────────────

    #[test]
    fn default_palette_has_256_colors() {
        let palette = ColorPalette::default();
        assert_eq!(palette.colors.0.len(), 256);
    }

    #[test]
    fn default_palette_black_is_first_ansi() {
        let palette = ColorPalette::default();
        let black = palette.colors.0[AnsiColor::Black as usize];
        // Black should have very low RGB values
        assert!(black.0 < 0.01);
        assert!(black.1 < 0.01);
        assert!(black.2 < 0.01);
    }

    #[test]
    fn default_palette_white_is_bright() {
        let palette = ColorPalette::default();
        let white = palette.colors.0[15]; // Bright white
        assert!(white.0 > 0.9);
        assert!(white.1 > 0.9);
        assert!(white.2 > 0.9);
    }

    #[test]
    fn default_palette_foreground_is_grey() {
        let palette = ColorPalette::default();
        // Foreground is set to colors[249] (a grey)
        assert!(palette.foreground.0 > 0.5);
    }

    #[test]
    fn default_palette_clone_eq() {
        let a = ColorPalette::default();
        let b = a.clone();
        assert_eq!(a, b);
    }

    // ── resolve_fg / resolve_bg ─────────────────────────────

    #[test]
    fn resolve_fg_default_returns_foreground() {
        let palette = ColorPalette::default();
        assert_eq!(
            palette.resolve_fg(ColorAttribute::Default),
            palette.foreground
        );
    }

    #[test]
    fn resolve_bg_default_returns_background() {
        let palette = ColorPalette::default();
        assert_eq!(
            palette.resolve_bg(ColorAttribute::Default),
            palette.background
        );
    }

    #[test]
    fn resolve_fg_palette_index() {
        let palette = ColorPalette::default();
        let color = palette.resolve_fg(ColorAttribute::PaletteIndex(1)); // Red/Maroon
        assert_eq!(color, palette.colors.0[1]);
    }

    #[test]
    fn resolve_bg_palette_index() {
        let palette = ColorPalette::default();
        let color = palette.resolve_bg(ColorAttribute::PaletteIndex(2)); // Green
        assert_eq!(color, palette.colors.0[2]);
    }

    #[test]
    fn resolve_fg_truecolor() {
        let palette = ColorPalette::default();
        let rgb = RgbColor::new_8bpc(0x12, 0x34, 0x56);
        let color = palette.resolve_fg(ColorAttribute::TrueColorWithDefaultFallback(rgb.into()));
        let expected: SrgbaTuple = rgb.into();
        assert_eq!(color, expected);
    }

    #[test]
    fn resolve_bg_truecolor_with_palette_fallback() {
        let palette = ColorPalette::default();
        let rgb = RgbColor::new_8bpc(0xab, 0xcd, 0xef);
        let color = palette.resolve_bg(ColorAttribute::TrueColorWithPaletteFallback(
            rgb.into(),
            100,
        ));
        let expected: SrgbaTuple = rgb.into();
        assert_eq!(color, expected);
    }

    // ── 216 color cube ──────────────────────────────────────

    #[test]
    fn color_cube_entry_16_is_black() {
        let palette = ColorPalette::default();
        let entry = palette.colors.0[16]; // First color cube entry (0,0,0)
        assert!(entry.0 < 0.01);
        assert!(entry.1 < 0.01);
        assert!(entry.2 < 0.01);
    }

    #[test]
    fn color_cube_entry_231_is_white() {
        let palette = ColorPalette::default();
        let entry = palette.colors.0[231]; // Last color cube entry (5,5,5) = (0xff, 0xff, 0xff)
        assert!(entry.0 > 0.9);
        assert!(entry.1 > 0.9);
        assert!(entry.2 > 0.9);
    }

    // ── 24 grey ramp ────────────────────────────────────────

    #[test]
    fn grey_ramp_is_monotonically_increasing() {
        let palette = ColorPalette::default();
        for i in 232..255 {
            let a = palette.colors.0[i];
            let b = palette.colors.0[i + 1];
            assert!(
                b.0 >= a.0,
                "Grey ramp not increasing at index {i}: {a:?} vs {b:?}"
            );
        }
    }

    #[test]
    fn grey_entries_are_grey() {
        let palette = ColorPalette::default();
        for i in 232..256 {
            let c = palette.colors.0[i];
            assert!(
                (c.0 - c.1).abs() < 0.01 && (c.1 - c.2).abs() < 0.01,
                "Entry {i} is not grey: {c:?}"
            );
        }
    }
}
