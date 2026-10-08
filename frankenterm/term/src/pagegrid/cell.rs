//! The packed 8-byte cell and its inline style (PageGrid ADR section 3.1).
//!
//! Every style legacy `CellAttributes` stores without its boxed
//! `FatAttributes` (the attribute bits plus a default or palette foreground
//! and background) fits in the cell's 32-bit style field. Only true colour and
//! underline colour need the page's rich-style table (ADR D2, D3).

use super::style::{RichStyle, RichStyleTable};
use crate::color::ColorAttribute;
use frankenterm_cell::{Blink, CellAttributes, Intensity, SemanticType, Underline, VerticalAlign};

/// The 14 inline attribute bits, in legacy order minus wrap and semantic
/// type: intensity 2, underline 3, blink 2, italic, reverse, strikethrough,
/// invisible, overline, vertical align 2.
pub fn encode_attrs(attrs: &CellAttributes) -> u16 {
    let intensity: u16 = match attrs.intensity() {
        Intensity::Normal => 0,
        Intensity::Bold => 1,
        Intensity::Half => 2,
    };
    let underline: u16 = match attrs.underline() {
        Underline::None => 0,
        Underline::Single => 1,
        Underline::Double => 2,
        Underline::Curly => 3,
        Underline::Dotted => 4,
        Underline::Dashed => 5,
    };
    let blink: u16 = match attrs.blink() {
        Blink::None => 0,
        Blink::Slow => 1,
        Blink::Rapid => 2,
    };
    let vertical_align: u16 = match attrs.vertical_align() {
        VerticalAlign::BaseLine => 0,
        VerticalAlign::SuperScript => 1,
        VerticalAlign::SubScript => 2,
    };
    intensity
        | (underline << 2)
        | (blink << 5)
        | (u16::from(attrs.italic()) << 7)
        | (u16::from(attrs.reverse()) << 8)
        | (u16::from(attrs.strikethrough()) << 9)
        | (u16::from(attrs.invisible()) << 10)
        | (u16::from(attrs.overline()) << 11)
        | (vertical_align << 12)
}

/// Applies [`encode_attrs`] bits to `attrs`.
pub fn apply_attrs(bits: u16, attrs: &mut CellAttributes) {
    attrs.set_intensity(match bits & 0b11 {
        1 => Intensity::Bold,
        2 => Intensity::Half,
        _ => Intensity::Normal,
    });
    attrs.set_underline(match (bits >> 2) & 0b111 {
        1 => Underline::Single,
        2 => Underline::Double,
        3 => Underline::Curly,
        4 => Underline::Dotted,
        5 => Underline::Dashed,
        _ => Underline::None,
    });
    attrs.set_blink(match (bits >> 5) & 0b11 {
        1 => Blink::Slow,
        2 => Blink::Rapid,
        _ => Blink::None,
    });
    attrs.set_italic(bits & (1 << 7) != 0);
    attrs.set_reverse(bits & (1 << 8) != 0);
    attrs.set_strikethrough(bits & (1 << 9) != 0);
    attrs.set_invisible(bits & (1 << 10) != 0);
    attrs.set_overline(bits & (1 << 11) != 0);
    attrs.set_vertical_align(match (bits >> 12) & 0b11 {
        1 => VerticalAlign::SuperScript,
        2 => VerticalAlign::SubScript,
        _ => VerticalAlign::BaseLine,
    });
}

/// 0 for the default colour, `n + 1` for palette index `n`; `None` for
/// colours the inline form cannot hold.
fn encode_color(color: ColorAttribute) -> Option<u16> {
    match color {
        ColorAttribute::Default => Some(0),
        ColorAttribute::PaletteIndex(index) => Some(u16::from(index) + 1),
        ColorAttribute::TrueColorWithDefaultFallback(_)
        | ColorAttribute::TrueColorWithPaletteFallback(..) => None,
    }
}

fn decode_color(code: u16) -> ColorAttribute {
    match code {
        0 => ColorAttribute::Default,
        // Codes above 256 never reach a cell (I4); clamp rather than wrap.
        code => ColorAttribute::PaletteIndex((code - 1).min(255) as u8),
    }
}

/// A cell's style inline: attrs 14 bits, then fg 9 bits, then bg 9 bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct InlineStyle(u32);

impl InlineStyle {
    pub const DEFAULT: InlineStyle = InlineStyle(0);

    /// `fg` and `bg` are 0 for default or `n + 1` for palette index `n`.
    pub fn new(attrs: u16, fg: u16, bg: u16) -> Self {
        debug_assert!(attrs < 1 << 14 && fg <= 256 && bg <= 256);
        Self(u32::from(attrs) | (u32::from(fg) << 14) | (u32::from(bg) << 23))
    }

    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub fn attrs(self) -> u16 {
        (self.0 & 0x3FFF) as u16
    }

    pub fn fg(self) -> u16 {
        ((self.0 >> 14) & 0x1FF) as u16
    }

    pub fn bg(self) -> u16 {
        ((self.0 >> 23) & 0x1FF) as u16
    }

    pub fn with_fg(self, fg: u16) -> Self {
        Self::new(self.attrs(), fg, self.bg())
    }

    pub fn with_bg(self, bg: u16) -> Self {
        Self::new(self.attrs(), self.fg(), bg)
    }
}

/// How a legacy style is held on a page.
#[derive(Clone, Debug, PartialEq)]
pub enum StyleClass {
    Inline(InlineStyle),
    Rich(RichStyle),
}

/// Classifies a legacy pen or cell style. Hyperlinks, images, wrap and
/// semantic type are not part of the style; they have their own cell bits
/// and side maps.
pub fn classify(attrs: &CellAttributes) -> StyleClass {
    let bits = encode_attrs(attrs);
    let underline_color = attrs.underline_color();
    match (
        encode_color(attrs.foreground()),
        encode_color(attrs.background()),
        underline_color,
    ) {
        (Some(fg), Some(bg), ColorAttribute::Default) => {
            StyleClass::Inline(InlineStyle::new(bits, fg, bg))
        }
        _ => StyleClass::Rich(RichStyle {
            attrs: bits,
            fg: attrs.foreground(),
            bg: attrs.background(),
            underline_color,
        }),
    }
}

/// The style part of a legacy `CellAttributes` for a stored cell style:
/// everything except hyperlink, images, wrap and semantic type.
pub fn style_attributes(style: CellStyle, table: &RichStyleTable) -> CellAttributes {
    let mut attrs = CellAttributes::default();
    match style {
        CellStyle::Inline(inline) => {
            apply_attrs(inline.attrs(), &mut attrs);
            attrs.set_foreground(decode_color(inline.fg()));
            attrs.set_background(decode_color(inline.bg()));
        }
        CellStyle::Rich(id) => {
            let style = table.get(id).expect("a stored rich-style id is live (I5)");
            apply_attrs(style.attrs, &mut attrs);
            attrs.set_foreground(style.fg);
            attrs.set_background(style.bg);
            attrs.set_underline_color(style.underline_color);
        }
    }
    attrs
}

/// The text a write stores in a cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph<'a> {
    /// Materializes as `" "`.
    Blank,
    Char(char),
    /// Two or more scalars, kept whole in the page grapheme arena.
    Cluster(&'a str),
}

impl<'a> Glyph<'a> {
    /// Classifies a legacy cell string. A lone space is blank: legacy cannot
    /// tell a printed space from an erased cell either. A cluster that starts
    /// with a space keeps U+0020 as its first scalar.
    pub fn from_text(text: &'a str) -> Self {
        let mut chars = text.chars();
        match (chars.next(), chars.next()) {
            (None, _) | (Some(' '), None) => Glyph::Blank,
            (Some(ch), None) => Glyph::Char(ch),
            (Some(_), Some(_)) => Glyph::Cluster(text),
        }
    }

    /// The codepoint field: 0 for blank or a lone space, else the first
    /// scalar.
    pub fn codepoint(self) -> u32 {
        match self {
            Glyph::Blank | Glyph::Char(' ') => 0,
            Glyph::Char(ch) => u32::from(ch),
            Glyph::Cluster(text) => text.chars().next().map_or(0, u32::from),
        }
    }
}

/// The style a cell stores: inline, or an id into the page's rich table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CellStyle {
    Inline(InlineStyle),
    Rich(u32),
}

/// A packed cell (`u64`, ADR section 3.1). The all-zero cell is a default
/// blank.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct PackedCell(u64);

impl PackedCell {
    pub const BLANK: PackedCell = PackedCell(0);

    const CODEPOINT_MASK: u64 = (1 << 21) - 1;
    const GRAPHEME: u64 = 1 << 21;
    const WIDE: u64 = 1 << 22;
    const HIDDEN: u64 = 1 << 23;
    const SEMANTIC_SHIFT: u32 = 24;
    const SEMANTIC_MASK: u64 = 0b11 << 24;
    const HYPERLINK: u64 = 1 << 26;
    const IMAGE: u64 = 1 << 27;
    const RICH: u64 = 1 << 28;
    const STYLE_SHIFT: u32 = 29;
    const STYLE_MASK: u64 = 0xFFFF_FFFF << 29;
    const WRAPPED: u64 = 1 << 61;
    /// Legacy `CellAttributes::protected` (DECSCA, SPA). It is a cell bit
    /// rather than a style bit: the inline style's 32 bits are full, and a
    /// pen's protection must not cost a print run anything beyond its
    /// template.
    const PROTECTED: u64 = 1 << 62;
    pub const RESERVED: u64 = 1 << 63;

    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub fn codepoint(self) -> u32 {
        (self.0 & Self::CODEPOINT_MASK) as u32
    }

    /// The first scalar of the grapheme; `None` for a blank cell.
    pub fn first_char(self) -> Option<char> {
        match self.codepoint() {
            0 => None,
            cp => std::char::from_u32(cp),
        }
    }

    pub fn is_blank(self) -> bool {
        self.codepoint() == 0
    }

    pub fn has_grapheme(self) -> bool {
        self.0 & Self::GRAPHEME != 0
    }

    pub fn is_wide(self) -> bool {
        self.0 & Self::WIDE != 0
    }

    pub fn is_hidden(self) -> bool {
        self.0 & Self::HIDDEN != 0
    }

    pub fn has_hyperlink(self) -> bool {
        self.0 & Self::HYPERLINK != 0
    }

    pub fn has_image(self) -> bool {
        self.0 & Self::IMAGE != 0
    }

    pub fn is_rich(self) -> bool {
        self.0 & Self::RICH != 0
    }

    pub fn is_wrapped(self) -> bool {
        self.0 & Self::WRAPPED != 0
    }

    pub fn is_protected(self) -> bool {
        self.0 & Self::PROTECTED != 0
    }

    pub fn semantic(self) -> SemanticType {
        match (self.0 & Self::SEMANTIC_MASK) >> Self::SEMANTIC_SHIFT {
            1 => SemanticType::Input,
            2 => SemanticType::Prompt,
            _ => SemanticType::Output,
        }
    }

    pub fn style(self) -> CellStyle {
        let field = ((self.0 & Self::STYLE_MASK) >> Self::STYLE_SHIFT) as u32;
        if self.is_rich() {
            CellStyle::Rich(field)
        } else {
            CellStyle::Inline(InlineStyle::from_bits(field))
        }
    }

    fn with_flag(self, flag: u64, on: bool) -> Self {
        if on {
            Self(self.0 | flag)
        } else {
            Self(self.0 & !flag)
        }
    }

    /// Sets the first scalar; 0 is blank. Use [`Glyph::codepoint`], which
    /// canonicalizes a lone space to 0.
    pub fn with_codepoint(self, cp: u32) -> Self {
        debug_assert!(cp <= 0x10_FFFF);
        Self((self.0 & !Self::CODEPOINT_MASK) | (u64::from(cp) & Self::CODEPOINT_MASK))
    }

    pub fn with_grapheme(self, on: bool) -> Self {
        self.with_flag(Self::GRAPHEME, on)
    }

    pub fn with_wide(self, on: bool) -> Self {
        self.with_flag(Self::WIDE, on)
    }

    pub fn with_hidden(self, on: bool) -> Self {
        self.with_flag(Self::HIDDEN, on)
    }

    pub fn with_hyperlink(self, on: bool) -> Self {
        self.with_flag(Self::HYPERLINK, on)
    }

    pub fn with_image(self, on: bool) -> Self {
        self.with_flag(Self::IMAGE, on)
    }

    pub fn with_wrapped(self, on: bool) -> Self {
        self.with_flag(Self::WRAPPED, on)
    }

    pub fn with_protected(self, on: bool) -> Self {
        self.with_flag(Self::PROTECTED, on)
    }

    pub fn with_semantic(self, semantic: SemanticType) -> Self {
        let code = semantic as u64;
        Self((self.0 & !Self::SEMANTIC_MASK) | (code << Self::SEMANTIC_SHIFT))
    }

    pub fn with_style(self, style: CellStyle) -> Self {
        let (rich, field) = match style {
            CellStyle::Inline(inline) => (false, inline.bits()),
            CellStyle::Rich(id) => (true, id),
        };
        Self((self.0 & !Self::STYLE_MASK) | (u64::from(field) << Self::STYLE_SHIFT))
            .with_flag(Self::RICH, rich)
    }
}
