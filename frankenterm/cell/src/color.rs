//! Colors for attributes
// for FromPrimitive
#![allow(clippy::useless_attribute)]

pub use frankenterm_color_types::{LinearRgba, SrgbaTuple};
use frankenterm_dynamic::{FromDynamic, ToDynamic};
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Serialize};

#[cfg(not(feature = "std"))]
extern crate alloc;
#[cfg(not(feature = "std"))]
use crate::alloc::string::ToString;
#[cfg(not(feature = "std"))]
use alloc::format;
#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

pub use frankenterm_escape_parser::color::{AnsiColor, ColorSpec, PaletteIndex, RgbColor};

/// Specifies the color to be used when rendering a cell.  This is the
/// type used in the `CellAttributes` struct and can specify an optional
/// TrueColor value, allowing a fallback to a more traditional palette
/// index if TrueColor is not available.
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, Eq, PartialEq, FromDynamic, ToDynamic, Hash)]
pub enum ColorAttribute {
    /// Use RgbColor when supported, falling back to the specified PaletteIndex.
    TrueColorWithPaletteFallback(SrgbaTuple, PaletteIndex),
    /// Use RgbColor when supported, falling back to the default color
    TrueColorWithDefaultFallback(SrgbaTuple),
    /// Use the specified PaletteIndex
    PaletteIndex(PaletteIndex),
    /// Use the default color
    Default,
}

impl Default for ColorAttribute {
    fn default() -> Self {
        ColorAttribute::Default
    }
}

impl From<AnsiColor> for ColorAttribute {
    fn from(col: AnsiColor) -> Self {
        ColorAttribute::PaletteIndex(col as u8)
    }
}

impl From<ColorSpec> for ColorAttribute {
    fn from(spec: ColorSpec) -> Self {
        match spec {
            ColorSpec::Default => ColorAttribute::Default,
            ColorSpec::PaletteIndex(idx) => ColorAttribute::PaletteIndex(idx),
            ColorSpec::TrueColor(color) => ColorAttribute::TrueColorWithDefaultFallback(color),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_attribute_default() {
        assert_eq!(ColorAttribute::default(), ColorAttribute::Default);
    }

    #[test]
    fn color_attribute_equality() {
        assert_eq!(ColorAttribute::Default, ColorAttribute::Default);
        assert_ne!(ColorAttribute::Default, ColorAttribute::PaletteIndex(0));
    }

    #[test]
    fn color_attribute_palette_equality() {
        assert_eq!(
            ColorAttribute::PaletteIndex(5),
            ColorAttribute::PaletteIndex(5)
        );
        assert_ne!(
            ColorAttribute::PaletteIndex(5),
            ColorAttribute::PaletteIndex(6)
        );
    }

    #[test]
    fn color_attribute_true_color_equality() {
        let c1 = SrgbaTuple(1.0, 0.0, 0.0, 1.0);
        let c2 = SrgbaTuple(1.0, 0.0, 0.0, 1.0);
        let c3 = SrgbaTuple(0.0, 1.0, 0.0, 1.0);
        assert_eq!(
            ColorAttribute::TrueColorWithDefaultFallback(c1),
            ColorAttribute::TrueColorWithDefaultFallback(c2)
        );
        assert_ne!(
            ColorAttribute::TrueColorWithDefaultFallback(c1),
            ColorAttribute::TrueColorWithDefaultFallback(c3)
        );
    }

    #[test]
    fn color_attribute_true_color_with_fallback() {
        let color = SrgbaTuple(0.5, 0.5, 0.5, 1.0);
        let a = ColorAttribute::TrueColorWithPaletteFallback(color, 7);
        let b = ColorAttribute::TrueColorWithPaletteFallback(color, 7);
        let c = ColorAttribute::TrueColorWithPaletteFallback(color, 8);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn color_attribute_from_ansi_color() {
        let ca: ColorAttribute = AnsiColor::Red.into();
        assert_eq!(ca, ColorAttribute::PaletteIndex(9));

        let ca: ColorAttribute = AnsiColor::Black.into();
        assert_eq!(ca, ColorAttribute::PaletteIndex(0));

        let ca: ColorAttribute = AnsiColor::White.into();
        assert_eq!(ca, ColorAttribute::PaletteIndex(15));
    }

    #[test]
    fn color_attribute_from_color_spec_default() {
        let ca: ColorAttribute = ColorSpec::Default.into();
        assert_eq!(ca, ColorAttribute::Default);
    }

    #[test]
    fn color_attribute_from_color_spec_palette() {
        let ca: ColorAttribute = ColorSpec::PaletteIndex(42).into();
        assert_eq!(ca, ColorAttribute::PaletteIndex(42));
    }

    #[test]
    fn color_attribute_from_color_spec_true_color() {
        let color = SrgbaTuple(1.0, 0.0, 0.0, 1.0);
        let ca: ColorAttribute = ColorSpec::TrueColor(color).into();
        assert_eq!(ca, ColorAttribute::TrueColorWithDefaultFallback(color));
    }

    #[test]
    fn color_attribute_clone_copy() {
        let a = ColorAttribute::PaletteIndex(3);
        let b = a;
        let c = a.clone();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn color_attribute_debug() {
        let dbg = format!("{:?}", ColorAttribute::Default);
        assert!(dbg.contains("Default"));

        let dbg = format!("{:?}", ColorAttribute::PaletteIndex(42));
        assert!(dbg.contains("PaletteIndex"));
        assert!(dbg.contains("42"));
    }

    #[test]
    fn color_attribute_hash_consistency() {
        use core::hash::Hash;
        // Just verify it doesn't panic
        let mut set = alloc::collections::BTreeSet::new();
        set.insert(format!("{:?}", ColorAttribute::Default));
        set.insert(format!("{:?}", ColorAttribute::PaletteIndex(0)));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn color_attribute_variants_differ() {
        let variants: alloc::vec::Vec<ColorAttribute> = alloc::vec![
            ColorAttribute::Default,
            ColorAttribute::PaletteIndex(0),
            ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(0.0, 0.0, 0.0, 1.0)),
            ColorAttribute::TrueColorWithPaletteFallback(SrgbaTuple(0.0, 0.0, 0.0, 1.0), 0),
        ];
        for (i, a) in variants.iter().enumerate() {
            for (j, b) in variants.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b);
                }
            }
        }
    }
}
