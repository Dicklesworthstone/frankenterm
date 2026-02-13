pub use frankenterm_color_types::{LinearRgba, SrgbaTuple};
use frankenterm_dynamic::{FromDynamic, FromDynamicOptions, ToDynamic, Value};
use num_derive::FromPrimitive;
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::allocate::*;

#[derive(Debug, Clone, Copy, FromPrimitive, PartialEq, Eq, FromDynamic, ToDynamic)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[repr(u8)]
/// These correspond to the classic ANSI color indices and are
/// used for convenience/readability in code
pub enum AnsiColor {
    /// "Dark" black
    Black = 0,
    /// Dark red
    Maroon,
    /// Dark green
    Green,
    /// "Dark" yellow
    Olive,
    /// Dark blue
    Navy,
    /// Dark purple
    Purple,
    /// "Dark" cyan
    Teal,
    /// "Dark" white
    Silver,
    /// "Bright" black
    Grey,
    /// Bright red
    Red,
    /// Bright green
    Lime,
    /// Bright yellow
    Yellow,
    /// Bright blue
    Blue,
    /// Bright purple
    Fuchsia,
    /// Bright Cyan/Aqua
    Aqua,
    /// Bright white
    White,
}

impl From<AnsiColor> for u8 {
    fn from(col: AnsiColor) -> u8 {
        col as u8
    }
}

/// Describes a color in the SRGB colorspace using red, green and blue
/// components in the range 0-255.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Hash)]
pub struct RgbColor {
    bits: u32,
}

impl Into<SrgbaTuple> for RgbColor {
    fn into(self) -> SrgbaTuple {
        self.to_tuple_rgba()
    }
}

impl RgbColor {
    /// Construct a color from discrete red, green, blue values
    /// in the range 0-255.
    pub const fn new_8bpc(red: u8, green: u8, blue: u8) -> Self {
        Self {
            bits: ((red as u32) << 16) | ((green as u32) << 8) | blue as u32,
        }
    }

    /// Construct a color from discrete red, green, blue values
    /// in the range 0.0-1.0 in the sRGB colorspace.
    pub fn new_f32(red: f32, green: f32, blue: f32) -> Self {
        let red = (red * 255.) as u8;
        let green = (green * 255.) as u8;
        let blue = (blue * 255.) as u8;
        Self::new_8bpc(red, green, blue)
    }

    /// Returns red, green, blue as 8bpc values.
    /// Will convert from 10bpc if that is the internal storage.
    pub fn to_tuple_rgb8(self) -> (u8, u8, u8) {
        (
            (self.bits >> 16) as u8,
            (self.bits >> 8) as u8,
            self.bits as u8,
        )
    }

    /// Returns red, green, blue as floating point values in the range 0.0-1.0.
    /// An alpha channel with the value of 1.0 is included.
    /// The values are in the sRGB colorspace.
    pub fn to_tuple_rgba(self) -> SrgbaTuple {
        SrgbaTuple(
            (self.bits >> 16) as u8 as f32 / 255.0,
            (self.bits >> 8) as u8 as f32 / 255.0,
            self.bits as u8 as f32 / 255.0,
            1.0,
        )
    }

    /// Returns red, green, blue as floating point values in the range 0.0-1.0.
    /// An alpha channel with the value of 1.0 is included.
    /// The values are converted from sRGB to linear colorspace.
    pub fn to_linear_tuple_rgba(self) -> LinearRgba {
        self.to_tuple_rgba().to_linear()
    }

    /// Construct a color from an X11/SVG/CSS3 color name.
    /// Returns None if the supplied name is not recognized.
    /// The list of names can be found here:
    /// <https://en.wikipedia.org/wiki/X11_color_names>
    pub fn from_named(name: &str) -> Option<RgbColor> {
        Some(SrgbaTuple::from_named(name)?.into())
    }

    /// Returns a string of the form `#RRGGBB`
    pub fn to_rgb_string(self) -> String {
        let (red, green, blue) = self.to_tuple_rgb8();
        format!("#{:02x}{:02x}{:02x}", red, green, blue)
    }

    /// Returns a string of the form `rgb:RRRR/GGGG/BBBB`
    pub fn to_x11_16bit_rgb_string(self) -> String {
        let (red, green, blue) = self.to_tuple_rgb8();
        format!(
            "rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}",
            red, red, green, green, blue, blue
        )
    }

    /// Construct a color from a string of the form `#RRGGBB` where
    /// R, G and B are all hex digits.
    /// `hsl:hue sat light` is also accepted, and allows specifying a color
    /// in the HSL color space, where `hue` is measure in degrees and has
    /// a range of 0-360, and both `sat` and `light` are specified in percentage
    /// in the range 0-100.
    pub fn from_rgb_str(s: &str) -> Option<RgbColor> {
        let srgb: SrgbaTuple = s.parse().ok()?;
        Some(srgb.into())
    }

    /// Construct a color from an SVG/CSS3 color name.
    /// or from a string of the form `#RRGGBB` where
    /// R, G and B are all hex digits.
    /// `hsl:hue sat light` is also accepted, and allows specifying a color
    /// in the HSL color space, where `hue` is measure in degrees and has
    /// a range of 0-360, and both `sat` and `light` are specified in percentage
    /// in the range 0-100.
    /// Returns None if the supplied name is not recognized.
    /// The list of names can be found here:
    /// <https://ogeon.github.io/docs/palette/master/palette/named/index.html>
    pub fn from_named_or_rgb_string(s: &str) -> Option<Self> {
        RgbColor::from_rgb_str(&s).or_else(|| RgbColor::from_named(&s))
    }
}

impl From<SrgbaTuple> for RgbColor {
    fn from(srgb: SrgbaTuple) -> RgbColor {
        let SrgbaTuple(r, g, b, _) = srgb;
        Self::new_f32(r, g, b)
    }
}

/// This is mildly unfortunate: in order to round trip RgbColor with serde
/// we need to provide a Serialize impl equivalent to the Deserialize impl
/// below.  We use the impl below to allow more flexible specification of
/// color strings in the config file.  A side effect of doing it this way
/// is that we have to serialize RgbColor as a 7-byte string when we could
/// otherwise serialize it as a 3-byte array.  There's probably a way
/// to make this work more efficiently, but for now this will do.
#[cfg(feature = "use_serde")]
impl Serialize for RgbColor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = self.to_rgb_string();
        s.serialize(serializer)
    }
}

#[cfg(feature = "use_serde")]
impl<'de> Deserialize<'de> for RgbColor {
    fn deserialize<D>(deserializer: D) -> Result<RgbColor, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        RgbColor::from_named_or_rgb_string(&s)
            .ok_or_else(|| format!("unknown color name: {}", s))
            .map_err(serde::de::Error::custom)
    }
}

impl ToDynamic for RgbColor {
    fn to_dynamic(&self) -> Value {
        self.to_rgb_string().to_dynamic()
    }
}

impl FromDynamic for RgbColor {
    fn from_dynamic(
        value: &Value,
        options: FromDynamicOptions,
    ) -> Result<Self, frankenterm_dynamic::Error> {
        let s = String::from_dynamic(value, options)?;
        Ok(RgbColor::from_named_or_rgb_string(&s)
            .ok_or_else(|| format!("unknown color name: {}", s))?)
    }
}

/// An index into the fixed color palette.
pub type PaletteIndex = u8;

/// Specifies the color to be used when rendering a cell.
/// This differs from `ColorAttribute` in that this type can only
/// specify one of the possible color types at once, whereas the
/// `ColorAttribute` type can specify a TrueColor value and a fallback.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ColorSpec {
    Default,
    /// Use either a raw number, or use values from the `AnsiColor` enum
    PaletteIndex(PaletteIndex),
    TrueColor(SrgbaTuple),
}

impl Default for ColorSpec {
    fn default() -> Self {
        ColorSpec::Default
    }
}

impl From<AnsiColor> for ColorSpec {
    fn from(col: AnsiColor) -> Self {
        ColorSpec::PaletteIndex(col as u8)
    }
}

impl From<RgbColor> for ColorSpec {
    fn from(col: RgbColor) -> Self {
        ColorSpec::TrueColor(col.into())
    }
}

impl From<SrgbaTuple> for ColorSpec {
    fn from(col: SrgbaTuple) -> Self {
        ColorSpec::TrueColor(col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_hsl() {
        let foo = RgbColor::from_rgb_str("hsl:235 100  50").unwrap();
        assert_eq!(foo.to_rgb_string(), "#0015ff");
    }

    #[test]
    fn from_rgb() {
        assert!(RgbColor::from_rgb_str("").is_none());
        assert!(RgbColor::from_rgb_str("#xyxyxy").is_none());

        let black = RgbColor::from_rgb_str("#FFF").unwrap();
        assert_eq!(black.to_tuple_rgb8(), (0xf0, 0xf0, 0xf0));

        let black = RgbColor::from_rgb_str("#000000").unwrap();
        assert_eq!(black.to_tuple_rgb8(), (0, 0, 0));

        let grey = RgbColor::from_rgb_str("rgb:D6/D6/D6").unwrap();
        assert_eq!(grey.to_tuple_rgb8(), (0xd6, 0xd6, 0xd6));

        let grey = RgbColor::from_rgb_str("rgb:f0f0/f0f0/f0f0").unwrap();
        assert_eq!(grey.to_tuple_rgb8(), (0xf0, 0xf0, 0xf0));
    }

    // --- AnsiColor tests ---

    #[test]
    fn ansi_color_u8_values() {
        assert_eq!(AnsiColor::Black as u8, 0);
        assert_eq!(AnsiColor::Maroon as u8, 1);
        assert_eq!(AnsiColor::Green as u8, 2);
        assert_eq!(AnsiColor::Olive as u8, 3);
        assert_eq!(AnsiColor::Navy as u8, 4);
        assert_eq!(AnsiColor::Purple as u8, 5);
        assert_eq!(AnsiColor::Teal as u8, 6);
        assert_eq!(AnsiColor::Silver as u8, 7);
        assert_eq!(AnsiColor::Grey as u8, 8);
        assert_eq!(AnsiColor::Red as u8, 9);
        assert_eq!(AnsiColor::Lime as u8, 10);
        assert_eq!(AnsiColor::Yellow as u8, 11);
        assert_eq!(AnsiColor::Blue as u8, 12);
        assert_eq!(AnsiColor::Fuchsia as u8, 13);
        assert_eq!(AnsiColor::Aqua as u8, 14);
        assert_eq!(AnsiColor::White as u8, 15);
    }

    #[test]
    fn ansi_color_from_into_u8() {
        let v: u8 = AnsiColor::Red.into();
        assert_eq!(v, 9);
        let v: u8 = AnsiColor::Black.into();
        assert_eq!(v, 0);
        let v: u8 = AnsiColor::White.into();
        assert_eq!(v, 15);
    }

    #[test]
    fn ansi_color_clone_copy() {
        let a = AnsiColor::Blue;
        let b = a;
        let c = a.clone();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn ansi_color_debug() {
        let dbg = format!("{:?}", AnsiColor::Fuchsia);
        assert!(dbg.contains("Fuchsia"));
    }

    #[test]
    fn ansi_color_equality() {
        assert_eq!(AnsiColor::Teal, AnsiColor::Teal);
        assert_ne!(AnsiColor::Teal, AnsiColor::Aqua);
    }

    // --- RgbColor tests ---

    #[test]
    fn rgb_new_8bpc() {
        let c = RgbColor::new_8bpc(0xff, 0x80, 0x00);
        assert_eq!(c.to_tuple_rgb8(), (0xff, 0x80, 0x00));
    }

    #[test]
    fn rgb_new_8bpc_black() {
        let c = RgbColor::new_8bpc(0, 0, 0);
        assert_eq!(c.to_tuple_rgb8(), (0, 0, 0));
    }

    #[test]
    fn rgb_new_8bpc_white() {
        let c = RgbColor::new_8bpc(255, 255, 255);
        assert_eq!(c.to_tuple_rgb8(), (255, 255, 255));
    }

    #[test]
    fn rgb_new_f32() {
        let c = RgbColor::new_f32(1.0, 0.0, 0.0);
        assert_eq!(c.to_tuple_rgb8(), (255, 0, 0));
    }

    #[test]
    fn rgb_new_f32_mid() {
        let c = RgbColor::new_f32(0.5, 0.5, 0.5);
        let (r, g, b) = c.to_tuple_rgb8();
        // 0.5 * 255 = 127.5 -> 127 as u8
        assert_eq!(r, 127);
        assert_eq!(g, 127);
        assert_eq!(b, 127);
    }

    #[test]
    fn rgb_default_is_black() {
        let c = RgbColor::default();
        assert_eq!(c.to_tuple_rgb8(), (0, 0, 0));
    }

    #[test]
    fn rgb_to_tuple_rgba() {
        let c = RgbColor::new_8bpc(255, 0, 0);
        let SrgbaTuple(r, g, b, a) = c.to_tuple_rgba();
        assert!((r - 1.0).abs() < 0.01);
        assert!(g.abs() < 0.01);
        assert!(b.abs() < 0.01);
        assert!((a - 1.0).abs() < 0.01);
    }

    #[test]
    fn rgb_to_linear_tuple_rgba() {
        let c = RgbColor::new_8bpc(255, 255, 255);
        let linear = c.to_linear_tuple_rgba();
        // White in linear should be (1,1,1,1)
        assert!((linear.0 - 1.0).abs() < 0.01);
        assert!((linear.3 - 1.0).abs() < 0.01);
    }

    #[test]
    fn rgb_to_rgb_string() {
        let c = RgbColor::new_8bpc(0xab, 0xcd, 0xef);
        assert_eq!(c.to_rgb_string(), "#abcdef");
    }

    #[test]
    fn rgb_to_rgb_string_black() {
        let c = RgbColor::new_8bpc(0, 0, 0);
        assert_eq!(c.to_rgb_string(), "#000000");
    }

    #[test]
    fn rgb_to_x11_16bit_rgb_string() {
        let c = RgbColor::new_8bpc(0xab, 0xcd, 0xef);
        assert_eq!(c.to_x11_16bit_rgb_string(), "rgb:abab/cdcd/efef");
    }

    #[test]
    fn rgb_from_named() {
        let red = RgbColor::from_named("red").unwrap();
        assert_eq!(red.to_tuple_rgb8(), (255, 0, 0));
    }

    #[test]
    fn rgb_from_named_unknown() {
        assert!(RgbColor::from_named("notacolor").is_none());
    }

    #[test]
    fn rgb_from_named_or_rgb_string_named() {
        let c = RgbColor::from_named_or_rgb_string("blue").unwrap();
        assert_eq!(c.to_tuple_rgb8(), (0, 0, 255));
    }

    #[test]
    fn rgb_from_named_or_rgb_string_hex() {
        let c = RgbColor::from_named_or_rgb_string("#ff0000").unwrap();
        assert_eq!(c.to_tuple_rgb8(), (255, 0, 0));
    }

    #[test]
    fn rgb_from_named_or_rgb_string_invalid() {
        assert!(RgbColor::from_named_or_rgb_string("!!!").is_none());
    }

    #[test]
    fn rgb_clone_copy() {
        let a = RgbColor::new_8bpc(1, 2, 3);
        let b = a;
        let c = a.clone();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn rgb_equality() {
        let a = RgbColor::new_8bpc(10, 20, 30);
        let b = RgbColor::new_8bpc(10, 20, 30);
        let c = RgbColor::new_8bpc(10, 20, 31);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn rgb_hash_consistency() {
        use core::hash::{Hash, Hasher};
        let a = RgbColor::new_8bpc(10, 20, 30);
        let b = RgbColor::new_8bpc(10, 20, 30);
        let hash_of = |c: &RgbColor| {
            let mut h = alloc::collections::BTreeSet::new();
            h.insert(c.to_rgb_string());
            h
        };
        assert_eq!(hash_of(&a), hash_of(&b));
    }

    #[test]
    fn rgb_into_srgba_tuple() {
        let c = RgbColor::new_8bpc(128, 64, 32);
        let tuple: SrgbaTuple = c.into();
        assert!((tuple.0 - 128.0 / 255.0).abs() < 0.01);
        assert!((tuple.1 - 64.0 / 255.0).abs() < 0.01);
        assert!((tuple.2 - 32.0 / 255.0).abs() < 0.01);
        assert!((tuple.3 - 1.0).abs() < 0.01);
    }

    #[test]
    fn rgb_from_srgba_tuple() {
        let tuple = SrgbaTuple(1.0, 0.0, 0.0, 1.0);
        let c: RgbColor = tuple.into();
        assert_eq!(c.to_tuple_rgb8(), (255, 0, 0));
    }

    #[test]
    fn rgb_debug() {
        let c = RgbColor::new_8bpc(1, 2, 3);
        let dbg = format!("{:?}", c);
        assert!(dbg.contains("RgbColor"));
    }

    // --- ColorSpec tests ---

    #[test]
    fn color_spec_default() {
        let cs = ColorSpec::default();
        assert_eq!(cs, ColorSpec::Default);
    }

    #[test]
    fn color_spec_from_ansi_color() {
        let cs: ColorSpec = AnsiColor::Red.into();
        assert_eq!(cs, ColorSpec::PaletteIndex(9));
    }

    #[test]
    fn color_spec_from_ansi_black() {
        let cs: ColorSpec = AnsiColor::Black.into();
        assert_eq!(cs, ColorSpec::PaletteIndex(0));
    }

    #[test]
    fn color_spec_from_rgb_color() {
        let rgb = RgbColor::new_8bpc(128, 64, 32);
        let cs: ColorSpec = rgb.into();
        match cs {
            ColorSpec::TrueColor(_) => {}
            _ => panic!("expected TrueColor"),
        }
    }

    #[test]
    fn color_spec_from_srgba_tuple() {
        let tuple = SrgbaTuple(0.5, 0.5, 0.5, 1.0);
        let cs: ColorSpec = tuple.into();
        assert_eq!(cs, ColorSpec::TrueColor(SrgbaTuple(0.5, 0.5, 0.5, 1.0)));
    }

    #[test]
    fn color_spec_clone_copy() {
        let a = ColorSpec::PaletteIndex(42);
        let b = a;
        let c = a.clone();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn color_spec_debug() {
        let cs = ColorSpec::PaletteIndex(5);
        let dbg = format!("{:?}", cs);
        assert!(dbg.contains("PaletteIndex"));
        assert!(dbg.contains("5"));
    }

    #[test]
    fn color_spec_equality() {
        assert_eq!(ColorSpec::Default, ColorSpec::Default);
        assert_ne!(ColorSpec::Default, ColorSpec::PaletteIndex(0));
        assert_ne!(ColorSpec::PaletteIndex(0), ColorSpec::PaletteIndex(1));
    }
}
