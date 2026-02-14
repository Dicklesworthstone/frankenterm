#![cfg_attr(not(feature = "std"), no_std)]
// Vendored from WezTerm — suppress cosmetic clippy lints
#![allow(clippy::collapsible_if)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::needless_return)]
#![allow(clippy::wrong_self_convention)]

use core::hash::{Hash, Hasher};
use core::str::FromStr;
#[cfg(feature = "std")]
use csscolorparser::Color;
use frankenterm_dynamic::{FromDynamic, FromDynamicOptions, ToDynamic, Value};
#[cfg(not(feature = "std"))]
#[allow(unused)]
use num_traits::float::Float;
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "std")]
use std::collections::HashMap;
#[cfg(feature = "std")]
use std::sync::LazyLock;

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

#[cfg(feature = "std")]
static SRGB_TO_F32_TABLE: LazyLock<[f32; 256]> = LazyLock::new(generate_srgb8_to_linear_f32_table);
#[cfg(feature = "std")]
static F32_TO_U8_TABLE: LazyLock<[u32; 104]> = LazyLock::new(generate_linear_f32_to_srgb8_table);
#[cfg(feature = "std")]
static RGB_TO_SRGB_TABLE: LazyLock<[u8; 256]> = LazyLock::new(generate_rgb_to_srgb8_table);
#[cfg(feature = "std")]
static RGB_TO_F32_TABLE: LazyLock<[f32; 256]> = LazyLock::new(generate_rgb_to_linear_f32_table);

#[cfg(feature = "std")]
fn generate_rgb_to_srgb8_table() -> [u8; 256] {
    let mut table = [0; 256];
    for (val, entry) in table.iter_mut().enumerate() {
        let linear = (val as f32) / 255.0;
        *entry = linear_f32_to_srgb8_using_table(linear);
    }
    table
}

#[cfg(feature = "std")]
fn generate_rgb_to_linear_f32_table() -> [f32; 256] {
    let mut table = [0.; 256];
    for (val, entry) in table.iter_mut().enumerate() {
        *entry = (val as f32) / 255.0;
    }
    table
}

#[cfg(feature = "std")]
fn generate_srgb8_to_linear_f32_table() -> [f32; 256] {
    let mut table = [0.; 256];
    for (val, entry) in table.iter_mut().enumerate() {
        let c = (val as f32) / 255.0;
        *entry = if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        };
    }
    table
}

#[allow(clippy::unreadable_literal)]
#[cfg(feature = "std")]
fn generate_linear_f32_to_srgb8_table() -> [u32; 104] {
    // My intent was to generate this array on the fly using the code that is commented
    // out below.  It is based on this gist:
    // https://gist.github.com/rygorous/2203834
    // but for whatever reason, the rust translation yields different numbers.
    // I haven't had an opportunity to dig in to why that is, and I just wanted
    // to get things rolling, so we're in a slightly gross state for now.
    [
        0x0073000d, 0x007a000d, 0x0080000d, 0x0087000d, 0x008d000d, 0x0094000d, 0x009a000d,
        0x00a1000d, 0x00a7001a, 0x00b4001a, 0x00c1001a, 0x00ce001a, 0x00da001a, 0x00e7001a,
        0x00f4001a, 0x0101001a, 0x010e0033, 0x01280033, 0x01410033, 0x015b0033, 0x01750033,
        0x018f0033, 0x01a80033, 0x01c20033, 0x01dc0067, 0x020f0067, 0x02430067, 0x02760067,
        0x02aa0067, 0x02dd0067, 0x03110067, 0x03440067, 0x037800ce, 0x03df00ce, 0x044600ce,
        0x04ad00ce, 0x051400ce, 0x057b00c5, 0x05dd00bc, 0x063b00b5, 0x06970158, 0x07420142,
        0x07e30130, 0x087b0120, 0x090b0112, 0x09940106, 0x0a1700fc, 0x0a9500f2, 0x0b0f01cb,
        0x0bf401ae, 0x0ccb0195, 0x0d950180, 0x0e56016e, 0x0f0d015e, 0x0fbc0150, 0x10630143,
        0x11070264, 0x1238023e, 0x1357021d, 0x14660201, 0x156601e9, 0x165a01d3, 0x174401c0,
        0x182401af, 0x18fe0331, 0x1a9602fe, 0x1c1502d2, 0x1d7e02ad, 0x1ed4028d, 0x201a0270,
        0x21520256, 0x227d0240, 0x239f0443, 0x25c003fe, 0x27bf03c4, 0x29a10392, 0x2b6a0367,
        0x2d1d0341, 0x2ebe031f, 0x304d0300, 0x31d105b0, 0x34a80555, 0x37520507, 0x39d504c5,
        0x3c37048b, 0x3e7c0458, 0x40a8042a, 0x42bd0401, 0x44c20798, 0x488e071e, 0x4c1c06b6,
        0x4f76065d, 0x52a50610, 0x55ac05cc, 0x5892058f, 0x5b590559, 0x5e0c0a23, 0x631c0980,
        0x67db08f6, 0x6c55087f, 0x70940818, 0x74a007bd, 0x787d076c, 0x7c330723,
    ]
    /*
    let numexp = 13;
    let mantissa_msb = 3;
    let nbuckets = numexp << mantissa_msb;
    let bucketsize = 1 << (23 - mantissa_msb);
    let mantshift = 12;

    let mut table = [0;104];

    let sum_aa = bucketsize as f64;
    let mut sum_ab = 0.0f64;
    let mut sum_bb = 0.0f64;

    for i in 0..bucketsize {
        let j = (i >> mantshift) as f64;

        sum_ab += j;
        sum_bb += j * j;
    }

    let inv_det = 1.0 / (sum_aa * sum_bb - sum_ab * sum_ab);
    eprintln!("sum_ab={:e} sum_bb={:e} inv_det={:e}", sum_ab, sum_bb, inv_det);

    for bucket in 0..nbuckets {
        let start = ((127 - numexp) << 23) + bucket*bucketsize;

        let mut sum_a = 0.0;
        let mut sum_b = 0.0;

        for i in 0..bucketsize {
            let j = i >> mantshift;

            let val = linear_f32_to_srgbf32(f32::from_bits(start + i)) as f64 + 0.5;
            sum_a += val;
            sum_b += j as f64 * val;
        }

        let solved_a = inv_det * (sum_bb*sum_a - sum_ab*sum_b);
        let solved_b = inv_det * (sum_aa*sum_b - sum_ab*sum_a);
        let scaled_a = solved_a * 65536.0 / 512.0;
        let scaled_b = solved_b * 65536.0;

        let int_a = (scaled_a + 0.5) as u32;
        let int_b = (scaled_b + 0.5) as u32;

        table[bucket as usize] = (int_a << 16) + int_b;
    }

    table
    */
}

/// Convert from linear rgb in floating point form (0-1.0) to srgb in floating point (0-255.0)
fn linear_f32_to_srgbf32(f: f32) -> f32 {
    if f <= 0.04045 {
        f * 12.92
    } else {
        f.powf(1.0 / 2.4) * 1.055 - 0.055
    }
}

#[cfg(feature = "std")]
pub fn linear_u8_to_srgb8(f: u8) -> u8 {
    unsafe { *RGB_TO_SRGB_TABLE.get_unchecked(f as usize) }
}

#[cfg(feature = "std")]
fn linear_f32_to_srgb8_using_table(f: f32) -> u8 {
    #[allow(clippy::unreadable_literal)]
    const ALMOST_ONE: u32 = 0x3f7fffff;
    #[allow(clippy::unreadable_literal)]
    const MINVAL: u32 = (127 - 13) << 23;
    let minval = f32::from_bits(MINVAL);
    let almost_one = f32::from_bits(ALMOST_ONE);

    let f = if f < minval {
        minval
    } else if f > almost_one {
        almost_one
    } else {
        f
    };

    let f_bits = f.to_bits();
    let tab = unsafe { *F32_TO_U8_TABLE.get_unchecked(((f_bits - MINVAL) >> 20) as usize) };
    let bias = (tab >> 16) << 9;
    let scale = tab & 0xffff;

    let t = (f_bits >> 12) & 0xff;

    ((bias + scale * t) >> 16) as u8
}

fn linear_f32_to_srgb8(f: f32) -> u8 {
    #[cfg(feature = "std")]
    {
        return linear_f32_to_srgb8_using_table(f);
    }
    #[cfg(not(feature = "std"))]
    {
        (linear_f32_to_srgbf32(f) * 255.) as u8
    }
}

/// Convert from srgb in u8 0-255 to linear floating point rgb 0-1.0
fn srgb8_to_linear_f32(val: u8) -> f32 {
    #[cfg(feature = "std")]
    {
        return unsafe { *SRGB_TO_F32_TABLE.get_unchecked(val as usize) };
    }
    #[cfg(not(feature = "std"))]
    {
        let c = (val as f32) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
}

fn rgb_to_linear_f32(val: u8) -> f32 {
    #[cfg(feature = "std")]
    {
        unsafe { *RGB_TO_F32_TABLE.get_unchecked(val as usize) }
    }
    #[cfg(not(feature = "std"))]
    {
        (val as f32) / 255.0
    }
}

/// A pixel holding SRGBA32 data in big endian format
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SrgbaPixel(u32);

impl SrgbaPixel {
    /// Create a pixel with the provided sRGBA values in u8 format
    pub fn rgba(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        #[allow(clippy::cast_lossless)]
        let word = (blue as u32) << 24 | (green as u32) << 16 | (red as u32) << 8 | alpha as u32;
        Self(word.to_be())
    }

    /// Returns the unpacked sRGBA components as u8
    #[inline]
    pub fn as_rgba(self) -> (u8, u8, u8, u8) {
        let host = u32::from_be(self.0);
        (
            (host >> 8) as u8,
            (host >> 16) as u8,
            (host >> 24) as u8,
            (host & 0xff) as u8,
        )
    }

    /// Returns RGBA channels in linear f32 format
    pub fn to_linear(self) -> LinearRgba {
        let (r, g, b, a) = self.as_rgba();
        LinearRgba::with_srgba(r, g, b, a)
    }

    /// Create a pixel with the provided big-endian u32 SRGBA data
    pub fn with_srgba_u32(word: u32) -> Self {
        Self(word)
    }

    /// Returns the underlying big-endian u32 SRGBA data
    pub fn as_srgba32(self) -> u32 {
        self.0
    }

    pub fn as_srgba_tuple(self) -> (f32, f32, f32, f32) {
        let u8tuple = self.as_rgba();
        let SrgbaTuple(r, g, b, a) = u8tuple.into();
        (r, g, b, a)
    }
}

/// A pixel value encoded as SRGBA RGBA values in f32 format (range: 0.0-1.0)
#[derive(Copy, Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub struct SrgbaTuple(pub f32, pub f32, pub f32, pub f32);

impl SrgbaTuple {
    pub fn premultiply(self) -> Self {
        let SrgbaTuple(r, g, b, a) = self;
        Self(r * a, g * a, b * a, a)
    }

    pub fn demultiply(self) -> Self {
        let SrgbaTuple(r, g, b, a) = self;
        if a != 0. {
            Self(r / a, g / a, b / a, a)
        } else {
            self
        }
    }

    pub fn to_tuple_rgba(self) -> (f32, f32, f32, f32) {
        (self.0, self.1, self.2, self.3)
    }

    pub fn as_rgba_u8(self) -> (u8, u8, u8, u8) {
        let (r, g, b, a) = (self.0, self.1, self.2, self.3);
        (
            (r * 255.0) as u8,
            (g * 255.0) as u8,
            (b * 255.0) as u8,
            (a * 255.0) as u8,
        )
    }

    pub fn interpolate(self, other: Self, k: f64) -> Self {
        let k = k as f32;

        let SrgbaTuple(r0, g0, b0, a0) = self.premultiply();
        let SrgbaTuple(r1, g1, b1, a1) = other.premultiply();

        let r = SrgbaTuple(
            r0 + k * (r1 - r0),
            g0 + k * (g1 - g0),
            b0 + k * (b1 - b0),
            a0 + k * (a1 - a0),
        );

        r.demultiply()
    }
}

impl ToDynamic for SrgbaTuple {
    fn to_dynamic(&self) -> Value {
        self.to_color_string().to_dynamic()
    }
}

impl FromDynamic for SrgbaTuple {
    fn from_dynamic(
        value: &Value,
        options: FromDynamicOptions,
    ) -> Result<Self, frankenterm_dynamic::Error> {
        let s = String::from_dynamic(value, options)?;
        Ok(SrgbaTuple::from_str(&s).map_err(|()| format!("unknown color name: {}", s))?)
    }
}

impl From<SrgbaPixel> for SrgbaTuple {
    fn from(pixel: SrgbaPixel) -> SrgbaTuple {
        let (r, g, b, a) = pixel.as_srgba_tuple();
        SrgbaTuple(r, g, b, a)
    }
}

impl From<(f32, f32, f32, f32)> for SrgbaTuple {
    fn from((r, g, b, a): (f32, f32, f32, f32)) -> SrgbaTuple {
        SrgbaTuple(r, g, b, a)
    }
}

impl From<(u8, u8, u8, u8)> for SrgbaTuple {
    fn from((r, g, b, a): (u8, u8, u8, u8)) -> SrgbaTuple {
        SrgbaTuple(
            r as f32 / 255.,
            g as f32 / 255.,
            b as f32 / 255.,
            a as f32 / 255.,
        )
    }
}

impl From<(u8, u8, u8)> for SrgbaTuple {
    fn from((r, g, b): (u8, u8, u8)) -> SrgbaTuple {
        SrgbaTuple(r as f32 / 255., g as f32 / 255., b as f32 / 255., 1.0)
    }
}

impl From<SrgbaTuple> for (f32, f32, f32, f32) {
    fn from(t: SrgbaTuple) -> (f32, f32, f32, f32) {
        (t.0, t.1, t.2, t.3)
    }
}

#[cfg(feature = "std")]
impl From<Color> for SrgbaTuple {
    fn from(color: Color) -> Self {
        Self(
            color.r as f32,
            color.g as f32,
            color.b as f32,
            color.a as f32,
        )
    }
}

#[cfg(feature = "std")]
static NAMED_COLORS: LazyLock<HashMap<String, SrgbaTuple>> = LazyLock::new(build_colors);

const RGB_TXT: &str = core::include_str!("rgb.txt");

fn iter_rgb_txt(mut func: impl FnMut(&str, SrgbaTuple) -> bool) {
    let transparent = SrgbaTuple(0., 0., 0., 0.);
    for name in &["transparent", "none", "clear"] {
        if (func)(name, transparent) {
            return;
        }
    }

    for line in RGB_TXT.lines() {
        let mut fields = line.split_ascii_whitespace();
        let red = fields.next().unwrap();
        let green = fields.next().unwrap();
        let blue = fields.next().unwrap();
        let name = fields.collect::<Vec<&str>>().join(" ");

        let name = name.to_ascii_lowercase();
        let color = SrgbaTuple(
            red.parse::<f32>().unwrap() / 255.,
            green.parse::<f32>().unwrap() / 255.,
            blue.parse::<f32>().unwrap() / 255.,
            1.0,
        );

        if (func)(&name, color) {
            return;
        }
    }
}

#[cfg(feature = "std")]
fn build_colors() -> HashMap<String, SrgbaTuple> {
    let mut map = HashMap::new();

    iter_rgb_txt(|name, color| {
        map.insert(name.to_string(), color);
        false
    });
    map
}

impl SrgbaTuple {
    /// Construct a color from an X11/SVG/CSS3 color name.
    /// Returns None if the supplied name is not recognized.
    /// The list of names can be found here:
    /// <https://en.wikipedia.org/wiki/X11_color_names>
    pub fn from_named(name: &str) -> Option<Self> {
        #[cfg(feature = "std")]
        {
            return NAMED_COLORS.get(&name.to_ascii_lowercase()).cloned();
        }
        #[cfg(not(feature = "std"))]
        {
            let mut result = None;
            iter_rgb_txt(|candidate, color| {
                if candidate.eq_ignore_ascii_case(name) {
                    result.replace(color);
                    true
                } else {
                    false
                }
            });
            result
        }
    }

    /// Returns self multiplied by the supplied alpha value.
    /// We don't need to linearize for this, as alpha is defined
    /// as being linear even in srgba!
    pub fn mul_alpha(self, alpha: f32) -> Self {
        Self(self.0, self.1, self.2, self.3 * alpha)
    }

    pub fn to_linear(self) -> LinearRgba {
        // See https://docs.rs/palette/0.5.0/src/palette/encoding/srgb.rs.html#43
        fn to_linear(v: f32) -> f32 {
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        }
        // Note that alpha is always linear
        LinearRgba(
            to_linear(self.0),
            to_linear(self.1),
            to_linear(self.2),
            self.3,
        )
    }

    pub fn to_srgb_u8(self) -> (u8, u8, u8, u8) {
        (
            (self.0 * 255.) as u8,
            (self.1 * 255.) as u8,
            (self.2 * 255.) as u8,
            (self.3 * 255.) as u8,
        )
    }

    /// Format as a color string: `#RRGGBB` if opaque, `rgba(...)` if transparent.
    pub fn to_color_string(self) -> String {
        if self.3 == 1.0 {
            self.to_rgb_string()
        } else {
            self.to_rgba_string()
        }
    }
}

impl core::fmt::Display for SrgbaTuple {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.to_color_string())
    }
}

impl SrgbaTuple {
    /// Returns a string of the form `#RRGGBB`
    pub fn to_rgb_string(self) -> String {
        format!(
            "#{:02x}{:02x}{:02x}",
            (self.0 * 255.) as u8,
            (self.1 * 255.) as u8,
            (self.2 * 255.) as u8
        )
    }

    pub fn to_rgba_string(self) -> String {
        format!(
            "rgba({}% {}% {}% {}%)",
            (self.0 * 100.),
            (self.1 * 100.),
            (self.2 * 100.),
            (self.3 * 100.)
        )
    }

    /// Returns a string of the form `rgb:RRRR/GGGG/BBBB`
    pub fn to_x11_16bit_rgb_string(self) -> String {
        format!(
            "rgb:{:04x}/{:04x}/{:04x}",
            (self.0 * 65535.) as u16,
            (self.1 * 65535.) as u16,
            (self.2 * 65535.) as u16
        )
    }

    #[cfg(feature = "std")]
    pub fn to_laba(self) -> (f64, f64, f64, f64) {
        Color::new(self.0.into(), self.1.into(), self.2.into(), self.3.into()).to_lab()
    }

    #[cfg(feature = "std")]
    pub fn to_hsla(self) -> (f64, f64, f64, f64) {
        Color::new(self.0.into(), self.1.into(), self.2.into(), self.3.into()).to_hsla()
    }

    #[cfg(feature = "std")]
    pub fn from_hsla(h: f64, s: f64, l: f64, a: f64) -> Self {
        let Color { r, g, b, a } = Color::from_hsla(h, s, l, a);
        Self(r as f32, g as f32, b as f32, a as f32)
    }

    /// Scale the color towards the maximum saturation by factor, a value ranging from 0.0 to 1.0.
    #[cfg(feature = "std")]
    pub fn saturate(&self, factor: f64) -> Self {
        let (h, s, l, a) = self.to_hsla();
        let s = apply_scale(s, factor);
        Self::from_hsla(h, s, l, a)
    }

    /// Increase the saturation by amount, a value ranging from 0.0 to 1.0.
    #[cfg(feature = "std")]
    pub fn saturate_fixed(&self, amount: f64) -> Self {
        let (h, s, l, a) = self.to_hsla();
        let s = apply_fixed(s, amount);
        Self::from_hsla(h, s, l, a)
    }

    /// Scale the color towards the maximum lightness by factor, a value ranging from 0.0 to 1.0
    #[cfg(feature = "std")]
    pub fn lighten(&self, factor: f64) -> Self {
        let (h, s, l, a) = self.to_hsla();
        let l = apply_scale(l, factor);
        Self::from_hsla(h, s, l, a)
    }

    /// Lighten the color by amount, a value ranging from 0.0 to 1.0
    #[cfg(feature = "std")]
    pub fn lighten_fixed(&self, amount: f64) -> Self {
        let (h, s, l, a) = self.to_hsla();
        let l = apply_fixed(l, amount);
        Self::from_hsla(h, s, l, a)
    }

    /// Rotate the hue angle by the specified number of degrees
    #[cfg(feature = "std")]
    pub fn adjust_hue_fixed(&self, amount: f64) -> Self {
        let (h, s, l, a) = self.to_hsla();
        let h = normalize_angle(h + amount);
        Self::from_hsla(h, s, l, a)
    }

    #[cfg(feature = "std")]
    pub fn complement(&self) -> Self {
        self.adjust_hue_fixed(180.)
    }

    #[cfg(feature = "std")]
    pub fn complement_ryb(&self) -> Self {
        self.adjust_hue_fixed_ryb(180.)
    }

    #[cfg(feature = "std")]
    pub fn triad(&self) -> (Self, Self) {
        (self.adjust_hue_fixed(120.), self.adjust_hue_fixed(-120.))
    }

    #[cfg(feature = "std")]
    pub fn square(&self) -> (Self, Self, Self) {
        (
            self.adjust_hue_fixed(90.),
            self.adjust_hue_fixed(270.),
            self.adjust_hue_fixed(180.),
        )
    }

    /// Rotate the hue angle by the specified number of degrees, using
    /// the RYB color wheel
    #[cfg(feature = "std")]
    pub fn adjust_hue_fixed_ryb(&self, amount: f64) -> Self {
        let (h, s, l, a) = self.to_hsla();
        let h = rgb_hue_to_ryb_hue(h);
        let h = normalize_angle(h + amount);
        let h = ryb_huge_to_rgb_hue(h);
        Self::from_hsla(h, s, l, a)
    }

    #[cfg(feature = "std")]
    fn lab_value(&self) -> deltae::LabValue {
        let (l, a, b, _alpha) = self.to_laba();
        deltae::LabValue {
            l: l as f32,
            a: a as f32,
            b: b as f32,
        }
    }

    #[cfg(feature = "std")]
    pub fn delta_e(&self, other: &Self) -> f32 {
        let a = self.lab_value();
        let b = other.lab_value();
        *deltae::DeltaE::new(a, b, deltae::DEMethod::DE2000).value()
    }

    #[cfg(feature = "std")]
    pub fn contrast_ratio(&self, other: &Self) -> f32 {
        self.to_linear().contrast_ratio(&other.to_linear())
    }

    /// Assuming that `self` represents the foreground color
    /// and `other` represents the background color, if the
    /// contrast ratio is below min_ratio, returns Some color
    /// that equals or exceeds the min_ratio to use as an alternative
    /// foreground color.
    /// If the ratio is already suitable, returns None; the caller should
    /// continue to use `self` as the foreground color.
    #[cfg(feature = "std")]
    pub fn ensure_contrast_ratio(&self, other: &Self, min_ratio: f32) -> Option<Self> {
        self.to_linear()
            .ensure_contrast_ratio(&other.to_linear(), min_ratio)
            .map(|linear| linear.to_srgb())
    }
}

/// Convert an RGB color space hue angle to an RYB colorspace hue angle
/// <https://github.com/TNMEM/Material-Design-Color-Picker/blob/1afe330c67d9db4deef7031d601324b538b43b09/rybcolor.js#L33>
#[cfg(feature = "std")]
fn rgb_hue_to_ryb_hue(hue: f64) -> f64 {
    if hue < 35. {
        map_range(hue, 0., 35., 0., 60.)
    } else if hue < 60. {
        map_range(hue, 35., 60., 60., 122.)
    } else if hue < 120. {
        map_range(hue, 60., 120., 122., 165.)
    } else if hue < 180. {
        map_range(hue, 120., 180., 165., 218.)
    } else if hue < 240. {
        map_range(hue, 180., 240., 218., 275.)
    } else if hue < 300. {
        map_range(hue, 240., 300., 275., 330.)
    } else {
        map_range(hue, 300., 360., 330., 360.)
    }
}

/// Convert an RYB color space hue angle to an RGB colorspace hue angle
#[cfg(feature = "std")]
fn ryb_huge_to_rgb_hue(hue: f64) -> f64 {
    if hue < 60. {
        map_range(hue, 0., 60., 0., 35.)
    } else if hue < 122. {
        map_range(hue, 60., 122., 35., 60.)
    } else if hue < 165. {
        map_range(hue, 122., 165., 60., 120.)
    } else if hue < 218. {
        map_range(hue, 165., 218., 120., 180.)
    } else if hue < 275. {
        map_range(hue, 218., 275., 180., 240.)
    } else if hue < 330. {
        map_range(hue, 275., 330., 240., 300.)
    } else {
        map_range(hue, 330., 360., 300., 360.)
    }
}

#[cfg(feature = "std")]
fn map_range(x: f64, x1: f64, x2: f64, y1: f64, y2: f64) -> f64 {
    let a_slope = (y2 - y1) / (x2 - x1);
    let a_slope_intercept = y1 - (a_slope * x1);
    x * a_slope + a_slope_intercept
}

#[cfg(feature = "std")]
fn normalize_angle(t: f64) -> f64 {
    let mut t = t % 360.0;
    if t < 0.0 {
        t += 360.0;
    }
    t
}

#[cfg(feature = "std")]
fn apply_scale(current: f64, factor: f64) -> f64 {
    let difference = if factor >= 0. { 1.0 - current } else { current };
    let delta = difference.max(0.) * factor;
    (current + delta).max(0.)
}

#[cfg(feature = "std")]
fn apply_fixed(current: f64, amount: f64) -> f64 {
    (current + amount).max(0.)
}

impl Hash for SrgbaTuple {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.to_ne_bytes().hash(state);
        self.1.to_ne_bytes().hash(state);
        self.2.to_ne_bytes().hash(state);
        self.3.to_ne_bytes().hash(state);
    }
}

impl Eq for SrgbaTuple {}

fn x_parse_color_component(value: &str) -> Result<f32, ()> {
    let mut component = 0u16;
    let mut num_digits = 0;

    for c in value.chars() {
        num_digits += 1;
        component <<= 4;

        let nybble = match c.to_digit(16) {
            Some(v) => v as u16,
            None => return Err(()),
        };
        component |= nybble;
    }

    // From XParseColor, the `rgb:` prefixed syntax scales the
    // value into 16 bits from the number of bits specified
    Ok((match num_digits {
        1 => (component | component << 4) as f32,
        2 => component as f32,
        3 => (component >> 4) as f32,
        4 => (component >> 8) as f32,
        _ => return Err(()),
    }) / 255.0)
}

impl FromStr for SrgbaTuple {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Workaround <https://github.com/mazznoer/csscolorparser-rs/pull/7/files>
        if !s.is_ascii() {
            return Err(());
        }
        if !s.is_empty() && s.as_bytes()[0] == b'#' {
            // Probably `#RGB`

            let digits = (s.len() - 1) / 3;
            if 1 + (digits * 3) != s.len() {
                return Err(());
            }

            if digits == 0 || digits > 4 {
                // Max of 16 bits supported
                return Err(());
            }

            let mut chars = s.chars().skip(1);

            macro_rules! digit {
                () => {{
                    let mut component = 0u16;

                    for _ in 0..digits {
                        component <<= 4;

                        let nybble = match chars.next().unwrap().to_digit(16) {
                            Some(v) => v as u16,
                            None => return Err(()),
                        };
                        component |= nybble;
                    }

                    // From XParseColor, the `#` syntax takes the most significant
                    // bits and uses those for the color value.  That function produces
                    // 16-bit color components but we want 8-bit components so we shift
                    // or truncate the bits here depending on the number of digits
                    (match digits {
                        1 => (component << 4) as f32,
                        2 => component as f32,
                        3 => (component >> 4) as f32,
                        4 => (component >> 8) as f32,
                        _ => return Err(()),
                    }) / 255.0
                }};
            }
            Ok(Self(digit!(), digit!(), digit!(), 1.0))
        } else if let Some(value) = s.strip_prefix("rgb:") {
            let fields: Vec<&str> = value.split('/').collect();
            if fields.len() != 3 {
                return Err(());
            }

            let red = x_parse_color_component(fields[0])?;
            let green = x_parse_color_component(fields[1])?;
            let blue = x_parse_color_component(fields[2])?;
            Ok(Self(red, green, blue, 1.0))
        } else if let Some(value) = s.strip_prefix("rgba:") {
            let fields: Vec<&str> = value.split('/').collect();
            if fields.len() == 4 {
                let red = x_parse_color_component(fields[0])?;
                let green = x_parse_color_component(fields[1])?;
                let blue = x_parse_color_component(fields[2])?;
                let alpha = x_parse_color_component(fields[3])?;
                return Ok(Self(red, green, blue, alpha));
            }

            let fields: Vec<_> = s[5..].split_ascii_whitespace().collect();
            if fields.len() == 4 {
                fn field(s: &str) -> Result<f32, ()> {
                    if s.ends_with('%') {
                        let v: f32 = s[0..s.len() - 1].parse().map_err(|_| ())?;
                        Ok(v / 100.)
                    } else {
                        let v: f32 = s.parse().map_err(|_| ())?;
                        if !(0. ..=255.0).contains(&v) {
                            Err(())
                        } else {
                            Ok(v / 255.)
                        }
                    }
                }
                let r: f32 = field(fields[0])?;
                let g: f32 = field(fields[1])?;
                let b: f32 = field(fields[2])?;
                let a: f32 = field(fields[3])?;

                Ok(Self(r, g, b, a))
            } else {
                Err(())
            }
        } else if let Some(rest) = s.strip_prefix("hsl:") {
            let fields: Vec<_> = rest.split_ascii_whitespace().collect();
            if fields.len() == 3 {
                // Expected to be degrees in range 0-360, but we allow for negative and wrapping
                let h: i32 = fields[0].parse().map_err(|_| ())?;
                // Expected to be percentage in range 0-100
                let s: i32 = fields[1].parse().map_err(|_| ())?;
                // Expected to be percentage in range 0-100
                let l: i32 = fields[2].parse().map_err(|_| ())?;

                fn hsl_to_rgb(hue: i32, sat: i32, light: i32) -> (f32, f32, f32) {
                    let hue = hue % 360;
                    let hue = if hue < 0 { hue + 360 } else { hue } as f32;
                    let sat = sat as f32 / 100.;
                    let light = light as f32 / 100.;
                    let a = sat * light.min(1. - light);
                    let f = |n: f32| -> f32 {
                        let k = (n + hue / 30.) % 12.;
                        light - a * (k - 3.).min(9. - k).clamp(-1., 1.)
                    };
                    (f(0.), f(8.), f(4.))
                }

                let (r, g, b) = hsl_to_rgb(h, s, l);
                Ok(Self(r, g, b, 1.0))
            } else {
                Err(())
            }
        } else {
            #[cfg(feature = "std")]
            {
                if let Ok(c) = csscolorparser::parse(s) {
                    return Ok(Self(c.r as f32, c.g as f32, c.b as f32, c.a as f32));
                }
            }
            Self::from_named(s).ok_or(())
        }
    }
}

/// A pixel value encoded as linear RGBA values in f32 format (range: 0.0-1.0)
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct LinearRgba(pub f32, pub f32, pub f32, pub f32);

impl Eq for LinearRgba {}

impl Hash for LinearRgba {
    fn hash<H>(&self, state: &mut H)
    where
        H: Hasher,
    {
        self.0.to_ne_bytes().hash(state);
        self.1.to_ne_bytes().hash(state);
        self.2.to_ne_bytes().hash(state);
        self.3.to_ne_bytes().hash(state);
    }
}

impl From<(f32, f32, f32, f32)> for LinearRgba {
    fn from((r, g, b, a): (f32, f32, f32, f32)) -> Self {
        Self(r, g, b, a)
    }
}

impl From<[f32; 4]> for LinearRgba {
    fn from([r, g, b, a]: [f32; 4]) -> Self {
        Self(r, g, b, a)
    }
}

impl From<LinearRgba> for [f32; 4] {
    fn from(val: LinearRgba) -> Self {
        [val.0, val.1, val.2, val.3]
    }
}

impl LinearRgba {
    /// Convert SRGBA u8 components to LinearRgba.
    /// Note that alpha in SRGBA colorspace is already linear,
    /// so this only applies gamma correction to RGB.
    pub fn with_srgba(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self(
            srgb8_to_linear_f32(red),
            srgb8_to_linear_f32(green),
            srgb8_to_linear_f32(blue),
            rgb_to_linear_f32(alpha),
        )
    }

    /// Convert linear RGBA u8 components to LinearRgba (f32)
    pub fn with_rgba(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self(
            rgb_to_linear_f32(red),
            rgb_to_linear_f32(green),
            rgb_to_linear_f32(blue),
            rgb_to_linear_f32(alpha),
        )
    }

    /// Create using the provided f32 components in the range 0.0-1.0
    pub const fn with_components(red: f32, green: f32, blue: f32, alpha: f32) -> Self {
        Self(red, green, blue, alpha)
    }

    pub const TRANSPARENT: Self = Self::with_components(0., 0., 0., 0.);

    /// Returns true if this color is fully transparent
    pub fn is_fully_transparent(self) -> bool {
        self.3 == 0.0
    }

    /// Returns self, except when self is transparent, in which case returns other
    pub fn when_fully_transparent(self, other: Self) -> Self {
        if self.is_fully_transparent() {
            other
        } else {
            self
        }
    }

    /// Returns self multiplied by the supplied alpha value
    pub fn mul_alpha(self, alpha: f32) -> Self {
        Self(self.0, self.1, self.2, self.3 * alpha)
    }

    /// Convert to an SRGB u32 pixel
    pub fn srgba_pixel(self) -> SrgbaPixel {
        SrgbaPixel::rgba(
            linear_f32_to_srgb8(self.0),
            linear_f32_to_srgb8(self.1),
            linear_f32_to_srgb8(self.2),
            (self.3 * 255.) as u8,
        )
    }

    /// Returns the individual RGBA channels as f32 components 0.0-1.0
    pub fn tuple(self) -> (f32, f32, f32, f32) {
        (self.0, self.1, self.2, self.3)
    }

    pub fn to_srgb(self) -> SrgbaTuple {
        // Note that alpha is always linear
        SrgbaTuple(
            linear_f32_to_srgbf32(self.0),
            linear_f32_to_srgbf32(self.1),
            linear_f32_to_srgbf32(self.2),
            self.3,
        )
    }

    #[cfg(feature = "std")]
    pub fn relative_luminance(&self) -> f32 {
        0.2126 * self.0 + 0.7152 * self.1 + 0.0722 * self.2
    }

    #[cfg(feature = "std")]
    pub fn contrast_ratio(&self, other: &Self) -> f32 {
        let lum_a = self.relative_luminance();
        let lum_b = other.relative_luminance();
        Self::lum_contrast_ratio(lum_a, lum_b)
    }

    #[cfg(feature = "std")]
    fn lum_contrast_ratio(lum_a: f32, lum_b: f32) -> f32 {
        let a = lum_a + 0.05;
        let b = lum_b + 0.05;
        if a > b {
            a / b
        } else {
            b / a
        }
    }

    #[cfg(feature = "std")]
    fn to_oklaba(&self) -> [f32; 4] {
        let (r, g, b, alpha) = (self.0, self.1, self.2, self.3);
        let l_ = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
        let m_ = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
        let s_ = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
        let l = 0.2104542553 * l_ + 0.7936177850 * m_ - 0.0040720468 * s_;
        let a = 1.9779984951 * l_ - 2.4285922050 * m_ + 0.4505937099 * s_;
        let b = 0.0259040371 * l_ + 0.7827717662 * m_ - 0.8086757660 * s_;
        [l, a, b, alpha]
    }

    #[cfg(feature = "std")]
    fn from_oklaba(l: f32, a: f32, b: f32, alpha: f32) -> Self {
        let l_ = (l + 0.3963377774 * a + 0.2158037573 * b).powi(3);
        let m_ = (l - 0.1055613458 * a - 0.0638541728 * b).powi(3);
        let s_ = (l - 0.0894841775 * a - 1.2914855480 * b).powi(3);

        let r = 4.0767416621 * l_ - 3.3077115913 * m_ + 0.2309699292 * s_;
        let g = -1.2684380046 * l_ + 2.6097574011 * m_ - 0.3413193965 * s_;
        let b = -0.0041960863 * l_ - 0.7034186147 * m_ + 1.7076147010 * s_;

        Self(r, g, b, alpha)
    }

    /// Assuming that `self` represents the foreground color
    /// and `other` represents the background color, if the
    /// contrast ratio is below min_ratio, returns Some color
    /// that equals or exceeds the min_ratio to use as an alternative
    /// foreground color.
    /// If the ratio is already suitable, returns None; the caller should
    /// continue to use `self` as the foreground color.
    #[cfg(feature = "std")]
    pub fn ensure_contrast_ratio(&self, other: &Self, min_ratio: f32) -> Option<Self> {
        if self == other {
            // Intentionally the same color, don't try to fixup
            return None;
        }

        let fg_lum = self.relative_luminance();
        let bg_lum = other.relative_luminance();
        let ratio = Self::lum_contrast_ratio(fg_lum, bg_lum);
        if ratio >= min_ratio {
            // Already has desired ratio or better
            return None;
        }

        let [_fg_l, fg_a, fg_b, fg_alpha] = self.to_oklaba();

        let reduced_lum = ((bg_lum + 0.05) / min_ratio - 0.05).clamp(0.05, 1.0);
        let reduced_col = Self::from_oklaba(reduced_lum, fg_a, fg_b, fg_alpha);
        let reduced_ratio = reduced_col.contrast_ratio(other);

        let increased_lum = ((bg_lum + 0.05) * min_ratio - 0.05).clamp(0.05, 1.0);
        let increased_col = Self::from_oklaba(increased_lum, fg_a, fg_b, fg_alpha);
        let increased_ratio = reduced_col.contrast_ratio(other);

        // Prefer the reduced luminance version if the fg is dimmer than bg
        if fg_lum < bg_lum {
            if reduced_ratio >= min_ratio {
                return Some(reduced_col);
            }
        }
        // Otherwise, let's find a satisfactory alternative
        if increased_ratio >= min_ratio {
            return Some(increased_col);
        }
        if reduced_ratio >= min_ratio {
            return Some(reduced_col);
        }

        // Didn't find one that satifies the min_ratio, but did we find
        // one that is better than the existing ratio?
        if reduced_ratio > ratio {
            return Some(reduced_col);
        }
        if increased_ratio > ratio {
            return Some(increased_col);
        }

        // What they had was as good as it gets
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    #[test]
    fn named_rgb() {
        let dark_green = SrgbaTuple::from_named("DarkGreen").unwrap();
        assert_eq!(dark_green.to_rgb_string(), "#006400");
    }

    #[test]
    fn from_hsl() {
        let foo = SrgbaTuple::from_str("hsl:235 100  50").unwrap();
        assert_eq!(foo.to_rgb_string(), "#0015ff");
    }

    #[test]
    fn from_rgba() {
        assert_eq!(
            SrgbaTuple::from_str("clear").unwrap().to_rgba_string(),
            "rgba(0% 0% 0% 0%)"
        );
        assert_eq!(
            SrgbaTuple::from_str("rgba:100% 0 0 50%")
                .unwrap()
                .to_rgba_string(),
            "rgba(100% 0% 0% 50%)"
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn from_css() {
        assert_eq!(
            SrgbaTuple::from_str("rgb(255,0,0)")
                .unwrap()
                .to_rgb_string(),
            "#ff0000"
        );

        let rgba = SrgbaTuple::from_str("rgba(255,0,0,1)").unwrap();
        let round_trip = SrgbaTuple::from_str(&rgba.to_rgba_string()).unwrap();
        assert_eq!(rgba, round_trip);
        assert_eq!(rgba.to_rgba_string(), "rgba(100% 0% 0% 100%)");
    }

    #[test]
    fn from_rgb() {
        assert!(SrgbaTuple::from_str("").is_err());
        assert!(SrgbaTuple::from_str("#xyxyxy").is_err());

        let foo = SrgbaTuple::from_str("#f00f00f00").unwrap();
        assert_eq!(foo.to_rgb_string(), "#f0f0f0");

        let black = SrgbaTuple::from_str("#000").unwrap();
        assert_eq!(black.to_rgb_string(), "#000000");

        let black = SrgbaTuple::from_str("#FFF").unwrap();
        assert_eq!(black.to_rgb_string(), "#f0f0f0");

        let black = SrgbaTuple::from_str("#000000").unwrap();
        assert_eq!(black.to_rgb_string(), "#000000");

        let grey = SrgbaTuple::from_str("rgb:D6/D6/D6").unwrap();
        assert_eq!(grey.to_rgb_string(), "#d6d6d6");

        let grey = SrgbaTuple::from_str("rgb:f0f0/f0f0/f0f0").unwrap();
        assert_eq!(grey.to_rgb_string(), "#f0f0f0");
    }

    #[cfg(feature = "std")]
    #[test]
    fn linear_rgb_contrast_ratio() {
        let a = LinearRgba::with_srgba(255, 0, 0, 1);
        let b = LinearRgba::with_srgba(0, 255, 0, 1);
        let contrast_ratio = a.contrast_ratio(&b);
        assert!(
            (2.91 - contrast_ratio).abs() < 0.01,
            "contrast({}) == 2.91",
            contrast_ratio
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn srgba_contrast_ratio() {
        let a = SrgbaTuple::from_str("hsl:0   100  50").unwrap();
        let b = SrgbaTuple::from_str("hsl:120 100  50").unwrap();
        let contrast_ratio = a.contrast_ratio(&b);
        assert!(
            (2.91 - contrast_ratio).abs() < 0.01,
            "contrast({}) == 2.91",
            contrast_ratio
        );
    }

    // ── SrgbaPixel ────────────────────────────────────────────

    #[test]
    fn srgba_pixel_rgba_roundtrip() {
        let p = SrgbaPixel::rgba(255, 128, 0, 200);
        let (r, g, b, a) = p.as_rgba();
        assert_eq!((r, g, b, a), (255, 128, 0, 200));
    }

    #[test]
    fn srgba_pixel_black() {
        let p = SrgbaPixel::rgba(0, 0, 0, 255);
        let (r, g, b, a) = p.as_rgba();
        assert_eq!((r, g, b, a), (0, 0, 0, 255));
    }

    #[test]
    fn srgba_pixel_white() {
        let p = SrgbaPixel::rgba(255, 255, 255, 255);
        let (r, g, b, a) = p.as_rgba();
        assert_eq!((r, g, b, a), (255, 255, 255, 255));
    }

    #[test]
    fn srgba_pixel_with_srgba_u32_and_as_srgba32() {
        let p = SrgbaPixel::rgba(100, 150, 200, 255);
        let raw = p.as_srgba32();
        let p2 = SrgbaPixel::with_srgba_u32(raw);
        assert_eq!(p, p2);
    }

    #[test]
    fn srgba_pixel_to_linear() {
        let p = SrgbaPixel::rgba(255, 0, 0, 255);
        let lin = p.to_linear();
        assert!(lin.0 > 0.9); // red channel should be high
        assert!(lin.1 < 0.01); // green near zero
        assert!(lin.2 < 0.01); // blue near zero
    }

    #[test]
    fn srgba_pixel_as_srgba_tuple() {
        let p = SrgbaPixel::rgba(255, 0, 0, 255);
        let (r, g, b, a) = p.as_srgba_tuple();
        assert!((r - 1.0).abs() < 0.01);
        assert!(g < 0.01);
        assert!(b < 0.01);
        assert!((a - 1.0).abs() < 0.01);
    }

    #[test]
    fn srgba_pixel_debug_eq() {
        let a = SrgbaPixel::rgba(1, 2, 3, 4);
        let b = SrgbaPixel::rgba(1, 2, 3, 4);
        assert_eq!(a, b);
        let debug = format!("{a:?}");
        assert!(debug.contains("SrgbaPixel"));
    }

    // ── SrgbaTuple construction ───────────────────────────────

    #[test]
    fn srgba_tuple_from_u8_tuple() {
        let t: SrgbaTuple = (255u8, 0u8, 0u8, 255u8).into();
        assert!((t.0 - 1.0).abs() < 0.01);
        assert!(t.1 < 0.01);
    }

    #[test]
    fn srgba_tuple_from_u8_triple() {
        let t: SrgbaTuple = (128u8, 128u8, 128u8).into();
        assert!((t.0 - 0.502).abs() < 0.01);
        assert!((t.3 - 1.0).abs() < 0.001); // alpha defaults to 1.0
    }

    #[test]
    fn srgba_tuple_from_f32_tuple() {
        let t: SrgbaTuple = (0.5f32, 0.25f32, 0.75f32, 1.0f32).into();
        assert_eq!(t.0, 0.5);
        assert_eq!(t.1, 0.25);
    }

    #[test]
    fn srgba_tuple_default() {
        let t = SrgbaTuple::default();
        assert_eq!(t, SrgbaTuple(0.0, 0.0, 0.0, 0.0));
    }

    // ── SrgbaTuple operations ─────────────────────────────────

    #[test]
    fn srgba_tuple_premultiply() {
        let t = SrgbaTuple(1.0, 0.5, 0.25, 0.5);
        let pm = t.premultiply();
        assert!((pm.0 - 0.5).abs() < 0.001);
        assert!((pm.1 - 0.25).abs() < 0.001);
        assert!((pm.2 - 0.125).abs() < 0.001);
        assert!((pm.3 - 0.5).abs() < 0.001);
    }

    #[test]
    fn srgba_tuple_demultiply() {
        let pm = SrgbaTuple(0.5, 0.25, 0.125, 0.5);
        let dm = pm.demultiply();
        assert!((dm.0 - 1.0).abs() < 0.001);
        assert!((dm.1 - 0.5).abs() < 0.001);
    }

    #[test]
    fn srgba_tuple_demultiply_zero_alpha() {
        let t = SrgbaTuple(0.5, 0.5, 0.5, 0.0);
        let dm = t.demultiply();
        assert_eq!(dm, t); // unchanged when alpha is 0
    }

    #[test]
    fn srgba_tuple_mul_alpha() {
        let t = SrgbaTuple(1.0, 1.0, 1.0, 1.0);
        let t2 = t.mul_alpha(0.5);
        assert!((t2.3 - 0.5).abs() < 0.001);
        assert_eq!(t2.0, 1.0); // RGB unchanged
    }

    #[test]
    fn srgba_tuple_interpolate_midpoint() {
        let a = SrgbaTuple(0.0, 0.0, 0.0, 1.0);
        let b = SrgbaTuple(1.0, 1.0, 1.0, 1.0);
        let mid = a.interpolate(b, 0.5);
        assert!((mid.0 - 0.5).abs() < 0.01);
        assert!((mid.1 - 0.5).abs() < 0.01);
    }

    #[test]
    fn srgba_tuple_interpolate_endpoints() {
        let a = SrgbaTuple(0.0, 0.0, 0.0, 1.0);
        let b = SrgbaTuple(1.0, 1.0, 1.0, 1.0);
        let start = a.interpolate(b, 0.0);
        let end = a.interpolate(b, 1.0);
        assert!((start.0 - 0.0).abs() < 0.01);
        assert!((end.0 - 1.0).abs() < 0.01);
    }

    #[test]
    fn srgba_tuple_to_tuple_rgba() {
        let t = SrgbaTuple(0.1, 0.2, 0.3, 0.4);
        let (r, g, b, a) = t.to_tuple_rgba();
        assert_eq!((r, g, b, a), (0.1, 0.2, 0.3, 0.4));
    }

    #[test]
    fn srgba_tuple_as_rgba_u8() {
        let t = SrgbaTuple(1.0, 0.5, 0.0, 1.0);
        let (r, g, b, a) = t.as_rgba_u8();
        assert_eq!(r, 255);
        assert_eq!(g, 127);
        assert_eq!(b, 0);
        assert_eq!(a, 255);
    }

    // ── SrgbaTuple string conversions ─────────────────────────

    #[test]
    fn to_rgb_string_red() {
        let t = SrgbaTuple(1.0, 0.0, 0.0, 1.0);
        assert_eq!(t.to_rgb_string(), "#ff0000");
    }

    #[test]
    fn to_string_opaque() {
        let t = SrgbaTuple(0.0, 1.0, 0.0, 1.0);
        assert_eq!(t.to_string(), "#00ff00");
    }

    #[test]
    fn to_string_transparent_uses_rgba() {
        let t = SrgbaTuple(1.0, 0.0, 0.0, 0.5);
        let s = t.to_string();
        assert!(s.starts_with("rgba("));
    }

    #[test]
    fn to_x11_16bit_rgb_string() {
        let t = SrgbaTuple(1.0, 1.0, 1.0, 1.0);
        assert_eq!(t.to_x11_16bit_rgb_string(), "rgb:ffff/ffff/ffff");
    }

    #[test]
    fn to_x11_16bit_rgb_string_black() {
        let t = SrgbaTuple(0.0, 0.0, 0.0, 1.0);
        assert_eq!(t.to_x11_16bit_rgb_string(), "rgb:0000/0000/0000");
    }

    // ── SrgbaTuple named colors ───────────────────────────────

    #[test]
    fn named_transparent() {
        let t = SrgbaTuple::from_named("transparent").unwrap();
        assert_eq!(t, SrgbaTuple(0.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn named_none() {
        let t = SrgbaTuple::from_named("none").unwrap();
        assert_eq!(t, SrgbaTuple(0.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn named_clear() {
        let t = SrgbaTuple::from_named("clear").unwrap();
        assert_eq!(t, SrgbaTuple(0.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn named_case_insensitive() {
        let a = SrgbaTuple::from_named("Red").unwrap();
        let b = SrgbaTuple::from_named("red").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn named_unknown_returns_none() {
        assert!(SrgbaTuple::from_named("notacolor").is_none());
    }

    // ── SrgbaTuple to/from linear ─────────────────────────────

    #[test]
    fn srgba_to_linear_roundtrip() {
        let srgb = SrgbaTuple(0.5, 0.5, 0.5, 1.0);
        let linear = srgb.to_linear();
        let back = linear.to_srgb();
        assert!((back.0 - srgb.0).abs() < 0.02);
        assert!((back.1 - srgb.1).abs() < 0.02);
    }

    #[test]
    fn srgba_to_linear_black() {
        let linear = SrgbaTuple(0.0, 0.0, 0.0, 1.0).to_linear();
        assert_eq!(linear.0, 0.0);
        assert_eq!(linear.1, 0.0);
        assert_eq!(linear.2, 0.0);
    }

    #[test]
    fn srgba_to_linear_white() {
        let linear = SrgbaTuple(1.0, 1.0, 1.0, 1.0).to_linear();
        assert!((linear.0 - 1.0).abs() < 0.01);
    }

    // ── SrgbaTuple HSL operations ─────────────────────────────

    #[cfg(feature = "std")]
    #[test]
    fn lighten_increases_lightness() {
        let dark = SrgbaTuple::from_str("hsl:0 100 25").unwrap();
        let lighter = dark.lighten(0.5);
        let (_, _, l_dark, _) = dark.to_hsla();
        let (_, _, l_light, _) = lighter.to_hsla();
        assert!(l_light > l_dark);
    }

    #[cfg(feature = "std")]
    #[test]
    fn complement_shifts_hue_180() {
        let red = SrgbaTuple::from_str("hsl:0 100 50").unwrap();
        let comp = red.complement();
        let (h, _, _, _) = comp.to_hsla();
        assert!((h - 180.0).abs() < 1.0);
    }

    #[cfg(feature = "std")]
    #[test]
    fn triad_returns_two_colors() {
        let c = SrgbaTuple::from_str("hsl:0 100 50").unwrap();
        let (t1, t2) = c.triad();
        let (h1, _, _, _) = t1.to_hsla();
        let (h2, _, _, _) = t2.to_hsla();
        assert!((h1 - 120.0).abs() < 1.0);
        assert!((h2 - 240.0).abs() < 1.0);
    }

    #[cfg(feature = "std")]
    #[test]
    fn square_returns_three_colors() {
        let c = SrgbaTuple::from_str("hsl:0 100 50").unwrap();
        let (s1, s2, s3) = c.square();
        let _ = (s1, s2, s3); // just verify it doesn't panic
    }

    // ── LinearRgba ────────────────────────────────────────────

    #[test]
    fn linear_rgba_transparent() {
        assert!(LinearRgba::TRANSPARENT.is_fully_transparent());
    }

    #[test]
    fn linear_rgba_when_fully_transparent_returns_other() {
        let other = LinearRgba(1.0, 0.0, 0.0, 1.0);
        let result = LinearRgba::TRANSPARENT.when_fully_transparent(other);
        assert_eq!(result, other);
    }

    #[test]
    fn linear_rgba_when_not_transparent_returns_self() {
        let self_ = LinearRgba(0.0, 1.0, 0.0, 1.0);
        let other = LinearRgba(1.0, 0.0, 0.0, 1.0);
        let result = self_.when_fully_transparent(other);
        assert_eq!(result, self_);
    }

    #[test]
    fn linear_rgba_with_components() {
        let c = LinearRgba::with_components(0.1, 0.2, 0.3, 0.4);
        assert_eq!(c.tuple(), (0.1, 0.2, 0.3, 0.4));
    }

    #[test]
    fn linear_rgba_mul_alpha() {
        let c = LinearRgba(1.0, 1.0, 1.0, 1.0);
        let c2 = c.mul_alpha(0.5);
        assert!((c2.3 - 0.5).abs() < 0.001);
    }

    #[test]
    fn linear_rgba_from_tuple() {
        let c: LinearRgba = (0.5f32, 0.6f32, 0.7f32, 0.8f32).into();
        assert_eq!(c.0, 0.5);
    }

    #[test]
    fn linear_rgba_from_array() {
        let c: LinearRgba = [0.1f32, 0.2, 0.3, 0.4].into();
        assert_eq!(c.0, 0.1);
    }

    #[test]
    fn linear_rgba_to_array() {
        let c = LinearRgba(0.1, 0.2, 0.3, 0.4);
        let arr: [f32; 4] = c.into();
        assert_eq!(arr, [0.1, 0.2, 0.3, 0.4]);
    }

    #[cfg(feature = "std")]
    #[test]
    fn linear_rgba_srgba_pixel_roundtrip() {
        let orig = LinearRgba::with_srgba(200, 100, 50, 255);
        let pixel = orig.srgba_pixel();
        let (r, g, b, _a) = pixel.as_rgba();
        assert!((r as i32 - 200).abs() <= 1);
        assert!((g as i32 - 100).abs() <= 1);
        assert!((b as i32 - 50).abs() <= 1);
    }

    #[cfg(feature = "std")]
    #[test]
    fn linear_rgba_relative_luminance_white() {
        let white = LinearRgba(1.0, 1.0, 1.0, 1.0);
        let lum = white.relative_luminance();
        assert!((lum - 1.0).abs() < 0.01);
    }

    #[cfg(feature = "std")]
    #[test]
    fn linear_rgba_relative_luminance_black() {
        let black = LinearRgba(0.0, 0.0, 0.0, 1.0);
        let lum = black.relative_luminance();
        assert!(lum < 0.01);
    }

    #[cfg(feature = "std")]
    #[test]
    fn linear_rgba_contrast_ratio_black_white() {
        let black = LinearRgba(0.0, 0.0, 0.0, 1.0);
        let white = LinearRgba(1.0, 1.0, 1.0, 1.0);
        let ratio = black.contrast_ratio(&white);
        assert!((ratio - 21.0).abs() < 0.1);
    }

    #[test]
    fn linear_rgba_default() {
        let d = LinearRgba::default();
        assert_eq!(d, LinearRgba(0.0, 0.0, 0.0, 0.0));
    }

    // ── SrgbaTuple Hash/Eq ────────────────────────────────────

    #[cfg(feature = "std")]
    #[test]
    fn srgba_tuple_hash_consistent() {
        use std::collections::hash_map::DefaultHasher;
        let a = SrgbaTuple(0.5, 0.5, 0.5, 1.0);
        let b = SrgbaTuple(0.5, 0.5, 0.5, 1.0);
        let mut ha = DefaultHasher::new();
        let mut hb = DefaultHasher::new();
        a.hash(&mut ha);
        b.hash(&mut hb);
        assert_eq!(ha.finish(), hb.finish());
    }

    // ── FromStr edge cases ────────────────────────────────────

    #[test]
    fn from_str_non_ascii_rejected() {
        assert!(SrgbaTuple::from_str("café").is_err());
    }

    #[test]
    fn from_str_hash_single_digit() {
        let t = SrgbaTuple::from_str("#F00").unwrap();
        // #F00 means R=F0, G=00, B=00 (scaled from 1-digit)
        assert!((t.0 - (0xF0 as f32 / 255.0)).abs() < 0.01);
    }

    #[test]
    fn from_str_rgba_colon_four_fields() {
        let t = SrgbaTuple::from_str("rgba:ff/00/ff/80").unwrap();
        assert!((t.0 - 1.0).abs() < 0.01); // red
        assert!(t.1 < 0.01); // green
        assert!((t.2 - 1.0).abs() < 0.01); // blue
        assert!((t.3 - 0.502).abs() < 0.01); // alpha ~0.5
    }

    #[test]
    fn from_str_invalid_hash_length() {
        assert!(SrgbaTuple::from_str("#12").is_err());
        assert!(SrgbaTuple::from_str("#12345").is_err());
    }

    // ── delta_e ───────────────────────────────────────────────

    #[cfg(feature = "std")]
    #[test]
    fn delta_e_identical_colors_is_zero() {
        let c = SrgbaTuple(0.5, 0.5, 0.5, 1.0);
        let de = c.delta_e(&c);
        assert!(de < 0.01);
    }

    #[cfg(feature = "std")]
    #[test]
    fn delta_e_different_colors_positive() {
        let a = SrgbaTuple(1.0, 0.0, 0.0, 1.0);
        let b = SrgbaTuple(0.0, 0.0, 1.0, 1.0);
        let de = a.delta_e(&b);
        assert!(de > 1.0);
    }

    // ── SrgbaTuple::to_srgb_u8 ──────────────────────────────

    #[test]
    fn to_srgb_u8_full_values() {
        let t = SrgbaTuple(1.0, 0.0, 0.5, 1.0);
        let (r, g, b, a) = t.to_srgb_u8();
        assert_eq!(r, 255);
        assert_eq!(g, 0);
        assert_eq!(b, 127);
        assert_eq!(a, 255);
    }

    #[test]
    fn to_srgb_u8_zeros() {
        let (r, g, b, a) = SrgbaTuple(0.0, 0.0, 0.0, 0.0).to_srgb_u8();
        assert_eq!((r, g, b, a), (0, 0, 0, 0));
    }

    // ── SrgbaTuple HSL color operations ──────────────────────

    #[cfg(feature = "std")]
    #[test]
    fn to_hsla_red() {
        let red = SrgbaTuple(1.0, 0.0, 0.0, 1.0);
        let (h, s, l, a) = red.to_hsla();
        assert!(h.abs() < 1.0 || (h - 360.0).abs() < 1.0); // hue ~0 or ~360
        assert!((s - 1.0).abs() < 0.01); // fully saturated
        assert!((l - 0.5).abs() < 0.01); // lightness 50%
        assert!((a - 1.0).abs() < 0.01);
    }

    #[cfg(feature = "std")]
    #[test]
    fn to_hsla_green() {
        let green = SrgbaTuple(0.0, 1.0, 0.0, 1.0);
        let (h, _, _, _) = green.to_hsla();
        assert!((h - 120.0).abs() < 1.0);
    }

    #[cfg(feature = "std")]
    #[test]
    fn to_hsla_blue() {
        let blue = SrgbaTuple(0.0, 0.0, 1.0, 1.0);
        let (h, _, _, _) = blue.to_hsla();
        assert!((h - 240.0).abs() < 1.0);
    }

    #[cfg(feature = "std")]
    #[test]
    fn from_hsla_roundtrip() {
        let original = SrgbaTuple(0.8, 0.3, 0.6, 1.0);
        let (h, s, l, a) = original.to_hsla();
        let reconstructed = SrgbaTuple::from_hsla(h, s, l, a);
        assert!((reconstructed.0 - original.0).abs() < 0.02);
        assert!((reconstructed.1 - original.1).abs() < 0.02);
        assert!((reconstructed.2 - original.2).abs() < 0.02);
    }

    #[cfg(feature = "std")]
    #[test]
    fn to_laba_roundtrip_consistency() {
        let c = SrgbaTuple(0.5, 0.3, 0.7, 1.0);
        let (l, a, b, alpha) = c.to_laba();
        assert!(l > 0.0); // lightness positive
        assert!((alpha - 1.0).abs() < 0.01);
        // a and b are chrominance channels - just verify they're finite
        assert!(a.is_finite());
        assert!(b.is_finite());
    }

    #[cfg(feature = "std")]
    #[test]
    fn saturate_increases_saturation() {
        let muted = SrgbaTuple::from_str("hsl:0 50 50").unwrap();
        let vivid = muted.saturate(0.5);
        let (_, s_muted, _, _) = muted.to_hsla();
        let (_, s_vivid, _, _) = vivid.to_hsla();
        assert!(s_vivid > s_muted);
    }

    #[cfg(feature = "std")]
    #[test]
    fn saturate_fixed_adds_saturation() {
        let c = SrgbaTuple::from_str("hsl:120 30 50").unwrap();
        let more = c.saturate_fixed(0.2);
        let (_, s_orig, _, _) = c.to_hsla();
        let (_, s_more, _, _) = more.to_hsla();
        assert!(s_more > s_orig);
    }

    #[cfg(feature = "std")]
    #[test]
    fn lighten_fixed_adds_lightness() {
        let c = SrgbaTuple::from_str("hsl:0 100 30").unwrap();
        let lighter = c.lighten_fixed(0.2);
        let (_, _, l_orig, _) = c.to_hsla();
        let (_, _, l_light, _) = lighter.to_hsla();
        assert!(l_light > l_orig);
    }

    #[cfg(feature = "std")]
    #[test]
    fn adjust_hue_fixed_rotates() {
        let c = SrgbaTuple::from_str("hsl:0 100 50").unwrap();
        let rotated = c.adjust_hue_fixed(90.0);
        let (h, _, _, _) = rotated.to_hsla();
        assert!((h - 90.0).abs() < 1.0);
    }

    #[cfg(feature = "std")]
    #[test]
    fn adjust_hue_fixed_negative() {
        let c = SrgbaTuple::from_str("hsl:90 100 50").unwrap();
        let rotated = c.adjust_hue_fixed(-90.0);
        let (h, _, _, _) = rotated.to_hsla();
        assert!(h.abs() < 1.0 || (h - 360.0).abs() < 1.0);
    }

    #[cfg(feature = "std")]
    #[test]
    fn complement_ryb_shifts_hue() {
        let c = SrgbaTuple::from_str("hsl:0 100 50").unwrap();
        let comp = c.complement_ryb();
        // RYB complement of red should differ from RGB complement
        let (h_ryb, _, _, _) = comp.to_hsla();
        let (h_rgb, _, _, _) = c.complement().to_hsla();
        // They should be different because RYB uses a different color wheel
        assert!((h_ryb - h_rgb).abs() > 1.0);
    }

    #[cfg(feature = "std")]
    #[test]
    fn adjust_hue_fixed_ryb_basic() {
        let c = SrgbaTuple::from_str("hsl:0 100 50").unwrap();
        let adjusted = c.adjust_hue_fixed_ryb(90.0);
        // Should produce a valid color
        assert!(adjusted.0 >= 0.0 && adjusted.0 <= 1.0);
        assert!(adjusted.1 >= 0.0 && adjusted.1 <= 1.0);
        assert!(adjusted.2 >= 0.0 && adjusted.2 <= 1.0);
    }

    // ── SrgbaTuple ToDynamic / FromDynamic ───────────────────

    #[test]
    fn to_dynamic_opaque_color() {
        let c = SrgbaTuple(1.0, 0.0, 0.0, 1.0);
        let val = c.to_dynamic();
        // Should produce a string value like "#ff0000"
        match &val {
            Value::String(s) => assert_eq!(s.as_str(), "#ff0000"),
            _ => panic!("expected string value"),
        }
    }

    #[test]
    fn from_dynamic_named_color() {
        let val = Value::String("red".into());
        let c = SrgbaTuple::from_dynamic(&val, FromDynamicOptions::default()).unwrap();
        assert_eq!(c.to_rgb_string(), "#ff0000");
    }

    #[test]
    fn from_dynamic_hex_color() {
        let val = Value::String("#00ff00".into());
        let c = SrgbaTuple::from_dynamic(&val, FromDynamicOptions::default()).unwrap();
        assert_eq!(c.to_rgb_string(), "#00ff00");
    }

    #[test]
    fn from_dynamic_invalid_color() {
        let val = Value::String("notacolor".into());
        let result = SrgbaTuple::from_dynamic(&val, FromDynamicOptions::default());
        assert!(result.is_err());
    }

    // ── LinearRgba::with_rgba ───────────────────────────────

    #[test]
    fn linear_rgba_with_rgba_black() {
        let c = LinearRgba::with_rgba(0, 0, 0, 255);
        assert_eq!(c.0, 0.0);
        assert_eq!(c.1, 0.0);
        assert_eq!(c.2, 0.0);
        assert!((c.3 - 1.0).abs() < 0.01);
    }

    #[test]
    fn linear_rgba_with_rgba_white() {
        let c = LinearRgba::with_rgba(255, 255, 255, 255);
        assert!((c.0 - 1.0).abs() < 0.01);
        assert!((c.1 - 1.0).abs() < 0.01);
        assert!((c.2 - 1.0).abs() < 0.01);
    }

    #[test]
    fn linear_rgba_with_srgba_vs_with_rgba_differ() {
        // sRGB applies gamma correction, linear doesn't
        let srgba = LinearRgba::with_srgba(128, 128, 128, 255);
        let rgba = LinearRgba::with_rgba(128, 128, 128, 255);
        // sRGB 128 → linear should be < 0.5 due to gamma
        assert!(srgba.0 < rgba.0);
    }

    // ── LinearRgba ensure_contrast_ratio ─────────────────────

    #[cfg(feature = "std")]
    #[test]
    fn ensure_contrast_ratio_already_sufficient() {
        let black = LinearRgba(0.0, 0.0, 0.0, 1.0);
        let white = LinearRgba(1.0, 1.0, 1.0, 1.0);
        // Black on white has ~21:1 ratio, asking for 4.5 should return None
        let result = black.ensure_contrast_ratio(&white, 4.5);
        assert!(result.is_none());
    }

    #[cfg(feature = "std")]
    #[test]
    fn ensure_contrast_ratio_same_color_returns_none() {
        let c = LinearRgba(0.5, 0.5, 0.5, 1.0);
        let result = c.ensure_contrast_ratio(&c, 4.5);
        assert!(result.is_none());
    }

    #[cfg(feature = "std")]
    #[test]
    fn ensure_contrast_ratio_low_contrast_returns_some() {
        let fg = LinearRgba(0.5, 0.5, 0.5, 1.0);
        let bg = LinearRgba(0.45, 0.45, 0.45, 1.0);
        let result = fg.ensure_contrast_ratio(&bg, 4.5);
        // Should suggest an alternative with better contrast
        assert!(result.is_some());
    }

    // ── LinearRgba oklaba roundtrip ──────────────────────────

    #[cfg(feature = "std")]
    #[test]
    fn oklaba_roundtrip() {
        let orig = LinearRgba(0.5, 0.3, 0.7, 1.0);
        let [l, a, b, alpha] = orig.to_oklaba();
        let back = LinearRgba::from_oklaba(l, a, b, alpha);
        assert!((back.0 - orig.0).abs() < 0.01);
        assert!((back.1 - orig.1).abs() < 0.01);
        assert!((back.2 - orig.2).abs() < 0.01);
    }

    #[cfg(feature = "std")]
    #[test]
    fn oklaba_black() {
        let black = LinearRgba(0.0, 0.0, 0.0, 1.0);
        let [l, _, _, _] = black.to_oklaba();
        assert!(l.abs() < 0.01); // L should be ~0 for black
    }

    #[cfg(feature = "std")]
    #[test]
    fn oklaba_white() {
        let white = LinearRgba(1.0, 1.0, 1.0, 1.0);
        let [l, _, _, _] = white.to_oklaba();
        assert!((l - 1.0).abs() < 0.01); // L should be ~1 for white
    }

    // ── LinearRgba Hash ─────────────────────────────────────

    #[cfg(feature = "std")]
    #[test]
    fn linear_rgba_hash_consistent() {
        use std::collections::hash_map::DefaultHasher;
        let a = LinearRgba(0.5, 0.5, 0.5, 1.0);
        let b = LinearRgba(0.5, 0.5, 0.5, 1.0);
        let mut ha = DefaultHasher::new();
        let mut hb = DefaultHasher::new();
        a.hash(&mut ha);
        b.hash(&mut hb);
        assert_eq!(ha.finish(), hb.finish());
    }

    #[cfg(feature = "std")]
    #[test]
    fn linear_rgba_hash_differs_for_different_values() {
        use std::collections::hash_map::DefaultHasher;
        let a = LinearRgba(0.5, 0.5, 0.5, 1.0);
        let b = LinearRgba(0.5, 0.5, 0.6, 1.0);
        let mut ha = DefaultHasher::new();
        let mut hb = DefaultHasher::new();
        a.hash(&mut ha);
        b.hash(&mut hb);
        assert_ne!(ha.finish(), hb.finish());
    }

    // ── FromStr additional edge cases ────────────────────────

    #[test]
    fn from_str_hsl_negative_hue() {
        let t = SrgbaTuple::from_str("hsl:-120 100 50").unwrap();
        // -120 wraps to 240 degrees (blue)
        assert!(t.2 > 0.9); // blue channel high
    }

    #[test]
    fn from_str_hsl_wrapping_hue() {
        let t = SrgbaTuple::from_str("hsl:360 100 50").unwrap();
        // 360 degrees wraps to 0 (red)
        assert!(t.0 > 0.9); // red channel high
    }

    #[test]
    fn from_str_hsl_zero_saturation_is_grey() {
        let t = SrgbaTuple::from_str("hsl:0 0 50").unwrap();
        // With 0 saturation, all channels should be equal (grey)
        assert!((t.0 - t.1).abs() < 0.01);
        assert!((t.1 - t.2).abs() < 0.01);
    }

    #[test]
    fn from_str_hsl_zero_lightness_is_black() {
        let t = SrgbaTuple::from_str("hsl:0 100 0").unwrap();
        assert!(t.0 < 0.01);
        assert!(t.1 < 0.01);
        assert!(t.2 < 0.01);
    }

    #[test]
    fn from_str_hsl_full_lightness_is_white() {
        let t = SrgbaTuple::from_str("hsl:0 100 100").unwrap();
        assert!((t.0 - 1.0).abs() < 0.01);
        assert!((t.1 - 1.0).abs() < 0.01);
        assert!((t.2 - 1.0).abs() < 0.01);
    }

    #[test]
    fn from_str_hsl_invalid_field_count() {
        assert!(SrgbaTuple::from_str("hsl:0 100").is_err());
        assert!(SrgbaTuple::from_str("hsl:0 100 50 1").is_err());
    }

    #[test]
    fn from_str_rgb_colon_single_digit() {
        let t = SrgbaTuple::from_str("rgb:F/0/F").unwrap();
        // Single digit F → FF → 255
        assert!((t.0 - 1.0).abs() < 0.01);
        assert!(t.1 < 0.01);
        assert!((t.2 - 1.0).abs() < 0.01);
    }

    #[test]
    fn from_str_rgb_colon_three_digit() {
        let t = SrgbaTuple::from_str("rgb:FFF/000/FFF").unwrap();
        // 3-digit FFF → FF (shift right 4)
        assert!((t.0 - 1.0).abs() < 0.01);
        assert!(t.1 < 0.01);
    }

    #[test]
    fn from_str_rgb_colon_invalid_field_count() {
        assert!(SrgbaTuple::from_str("rgb:FF/FF").is_err());
        assert!(SrgbaTuple::from_str("rgb:FF/FF/FF/FF").is_err());
    }

    #[test]
    fn from_str_rgb_colon_invalid_hex() {
        assert!(SrgbaTuple::from_str("rgb:GG/00/00").is_err());
    }

    #[test]
    fn from_str_rgba_percent_format() {
        let t = SrgbaTuple::from_str("rgba:50% 0% 100% 75%").unwrap();
        assert!((t.0 - 0.5).abs() < 0.01);
        assert!(t.1 < 0.01);
        assert!((t.2 - 1.0).abs() < 0.01);
        assert!((t.3 - 0.75).abs() < 0.01);
    }

    #[test]
    fn from_str_rgba_numeric_format() {
        let t = SrgbaTuple::from_str("rgba:255 0 128 255").unwrap();
        assert!((t.0 - 1.0).abs() < 0.01);
        assert!(t.1 < 0.01);
        assert!((t.2 - 0.502).abs() < 0.01);
    }

    #[test]
    fn from_str_rgba_invalid_field_count() {
        assert!(SrgbaTuple::from_str("rgba:100% 0% 0%").is_err());
    }

    #[test]
    fn from_str_hash_four_digit() {
        // #RRRRGGGGBBBB format (4 digits per channel)
        let t = SrgbaTuple::from_str("#FFFF00000000").unwrap();
        assert!((t.0 - 1.0).abs() < 0.01); // red
        assert!(t.1 < 0.01); // green
        assert!(t.2 < 0.01); // blue
    }

    #[test]
    fn from_str_hash_too_many_digits() {
        // 5 digits per component → 16 chars after #, not divisible by 3 cleanly → error
        assert!(SrgbaTuple::from_str("#FFFFF00000FFFFF").is_err());
    }

    #[test]
    fn from_str_empty_hash() {
        assert!(SrgbaTuple::from_str("#").is_err());
    }

    #[cfg(feature = "std")]
    #[test]
    fn from_str_css_rgb_function() {
        let t = SrgbaTuple::from_str("rgb(0, 128, 255)").unwrap();
        assert!(t.0 < 0.01);
        assert!((t.1 - 0.502).abs() < 0.01);
        assert!((t.2 - 1.0).abs() < 0.01);
    }

    #[cfg(feature = "std")]
    #[test]
    fn from_str_css_hsl_function() {
        let t = SrgbaTuple::from_str("hsl(120, 100%, 50%)").unwrap();
        assert!(t.0 < 0.01); // red low
        assert!(t.1 > 0.4); // green channel is high
    }

    // ── SrgbaTuple named color coverage ──────────────────────

    #[test]
    fn named_red() {
        let c = SrgbaTuple::from_named("red").unwrap();
        assert_eq!(c.to_rgb_string(), "#ff0000");
    }

    #[test]
    fn named_blue() {
        let c = SrgbaTuple::from_named("blue").unwrap();
        assert_eq!(c.to_rgb_string(), "#0000ff");
    }

    #[test]
    fn named_white() {
        let c = SrgbaTuple::from_named("white").unwrap();
        assert_eq!(c.to_rgb_string(), "#ffffff");
    }

    #[test]
    fn named_coral() {
        let c = SrgbaTuple::from_named("coral").unwrap();
        assert_eq!(c.to_rgb_string(), "#ff7f50");
    }

    // ── SrgbaPixel additional tests ─────────────────────────

    #[test]
    fn srgba_pixel_clone() {
        let p = SrgbaPixel::rgba(10, 20, 30, 40);
        let p2 = p;
        assert_eq!(p, p2);
    }

    #[test]
    fn srgba_pixel_zero_alpha() {
        let p = SrgbaPixel::rgba(255, 255, 255, 0);
        let (_, _, _, a) = p.as_rgba();
        assert_eq!(a, 0);
    }

    #[test]
    fn srgba_pixel_from_into_srgba_tuple() {
        let p = SrgbaPixel::rgba(255, 0, 0, 255);
        let t: SrgbaTuple = p.into();
        assert!((t.0 - 1.0).abs() < 0.01);
        assert!(t.1 < 0.01);
    }

    // ── Conversion table functions ──────────────────────────

    #[cfg(feature = "std")]
    #[test]
    fn linear_u8_to_srgb8_boundaries() {
        assert_eq!(linear_u8_to_srgb8(0), 0);
        let high = linear_u8_to_srgb8(255);
        assert!(high > 250); // should be close to 255
    }

    #[cfg(feature = "std")]
    #[test]
    fn linear_u8_to_srgb8_monotonic() {
        // sRGB conversion should be monotonically increasing
        let mut prev = linear_u8_to_srgb8(0);
        for i in 1..=255u8 {
            let curr = linear_u8_to_srgb8(i);
            assert!(
                curr >= prev,
                "srgb8({}) = {} < srgb8({}) = {}",
                i,
                curr,
                i - 1,
                prev
            );
            prev = curr;
        }
    }

    // ── SrgbaTuple into (f32,f32,f32,f32) ───────────────────

    #[test]
    fn srgba_tuple_into_f32_tuple() {
        let t = SrgbaTuple(0.1, 0.2, 0.3, 0.4);
        let (r, g, b, a): (f32, f32, f32, f32) = t.into();
        assert_eq!((r, g, b, a), (0.1, 0.2, 0.3, 0.4));
    }

    // ── SrgbaTuple Eq ───────────────────────────────────────

    #[test]
    fn srgba_tuple_eq_symmetric() {
        let a = SrgbaTuple(0.5, 0.5, 0.5, 1.0);
        let b = SrgbaTuple(0.5, 0.5, 0.5, 1.0);
        assert_eq!(a, b);
        assert_eq!(b, a);
    }

    #[test]
    fn srgba_tuple_ne_different_alpha() {
        let a = SrgbaTuple(0.5, 0.5, 0.5, 1.0);
        let b = SrgbaTuple(0.5, 0.5, 0.5, 0.5);
        assert_ne!(a, b);
    }

    // ── LinearRgba::is_fully_transparent edge cases ─────────

    #[test]
    fn linear_rgba_not_transparent_with_tiny_alpha() {
        let c = LinearRgba(0.0, 0.0, 0.0, 0.001);
        assert!(!c.is_fully_transparent());
    }

    // ── Contrast ratio symmetry ─────────────────────────────

    #[cfg(feature = "std")]
    #[test]
    fn contrast_ratio_symmetric() {
        let a = LinearRgba::with_srgba(200, 50, 50, 255);
        let b = LinearRgba::with_srgba(50, 200, 50, 255);
        let ratio_ab = a.contrast_ratio(&b);
        let ratio_ba = b.contrast_ratio(&a);
        assert!((ratio_ab - ratio_ba).abs() < 0.001);
    }

    #[cfg(feature = "std")]
    #[test]
    fn contrast_ratio_same_color_is_one() {
        let c = LinearRgba::with_srgba(128, 128, 128, 255);
        let ratio = c.contrast_ratio(&c);
        assert!((ratio - 1.0).abs() < 0.001);
    }

    // ── SrgbaTuple delta_e symmetry ─────────────────────────

    #[cfg(feature = "std")]
    #[test]
    fn delta_e_symmetric() {
        let a = SrgbaTuple(1.0, 0.0, 0.0, 1.0);
        let b = SrgbaTuple(0.0, 1.0, 0.0, 1.0);
        let de_ab = a.delta_e(&b);
        let de_ba = b.delta_e(&a);
        assert!((de_ab - de_ba).abs() < 0.01);
    }

    // ── SrgbaTuple::contrast_ratio via srgba ────────────────

    #[cfg(feature = "std")]
    #[test]
    fn srgba_contrast_ratio_same_is_one() {
        let c = SrgbaTuple(0.5, 0.5, 0.5, 1.0);
        let ratio = c.contrast_ratio(&c);
        assert!((ratio - 1.0).abs() < 0.01);
    }

    #[cfg(feature = "std")]
    #[test]
    fn srgba_contrast_ratio_black_white() {
        let black = SrgbaTuple(0.0, 0.0, 0.0, 1.0);
        let white = SrgbaTuple(1.0, 1.0, 1.0, 1.0);
        let ratio = black.contrast_ratio(&white);
        assert!((ratio - 21.0).abs() < 0.5);
    }

    // ── SrgbaTuple ensure_contrast_ratio ─────────────────────

    #[cfg(feature = "std")]
    #[test]
    fn srgba_ensure_contrast_ratio_sufficient() {
        let black = SrgbaTuple(0.0, 0.0, 0.0, 1.0);
        let white = SrgbaTuple(1.0, 1.0, 1.0, 1.0);
        assert!(black.ensure_contrast_ratio(&white, 4.5).is_none());
    }

    // ── linear_f32_to_srgbf32 ───────────────────────────────

    #[test]
    fn linear_f32_to_srgbf32_zero() {
        assert_eq!(linear_f32_to_srgbf32(0.0), 0.0);
    }

    #[test]
    fn linear_f32_to_srgbf32_one() {
        assert!((linear_f32_to_srgbf32(1.0) - 1.0).abs() < 0.001);
    }

    #[test]
    fn linear_f32_to_srgbf32_low_linear() {
        // Below 0.04045 threshold uses linear scaling
        let v = linear_f32_to_srgbf32(0.001);
        assert!(v > 0.0);
        assert!(v < 0.04045);
    }

    // ── Third-pass expansion ────────────────────────────────────

    #[test]
    fn srgba_tuple_premultiply_demultiply_roundtrip() {
        let orig = SrgbaTuple(0.8, 0.4, 0.2, 0.6);
        let roundtripped = orig.premultiply().demultiply();
        assert!((roundtripped.0 - orig.0).abs() < 0.001);
        assert!((roundtripped.1 - orig.1).abs() < 0.001);
        assert!((roundtripped.2 - orig.2).abs() < 0.001);
        assert!((roundtripped.3 - orig.3).abs() < 0.001);
    }

    #[test]
    fn srgba_tuple_premultiply_full_alpha_unchanged() {
        let t = SrgbaTuple(0.5, 0.3, 0.7, 1.0);
        let pm = t.premultiply();
        assert!((pm.0 - t.0).abs() < 0.001);
        assert!((pm.1 - t.1).abs() < 0.001);
        assert!((pm.2 - t.2).abs() < 0.001);
    }

    #[test]
    fn srgba_tuple_copy_clone() {
        let a = SrgbaTuple(0.1, 0.2, 0.3, 0.4);
        let b = a; // Copy
        #[allow(clippy::clone_on_copy)]
        let c = a.clone();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn srgba_tuple_interpolate_quarter() {
        let a = SrgbaTuple(0.0, 0.0, 0.0, 1.0);
        let b = SrgbaTuple(1.0, 1.0, 1.0, 1.0);
        let q = a.interpolate(b, 0.25);
        assert!((q.0 - 0.25).abs() < 0.02);
    }

    #[test]
    fn srgba_pixel_all_channels_max() {
        let p = SrgbaPixel::rgba(255, 255, 255, 255);
        assert_eq!(p.as_rgba(), (255, 255, 255, 255));
    }

    #[test]
    fn srgba_pixel_all_channels_zero() {
        let p = SrgbaPixel::rgba(0, 0, 0, 0);
        assert_eq!(p.as_rgba(), (0, 0, 0, 0));
    }

    #[test]
    fn srgba_tuple_to_linear_preserves_alpha() {
        let t = SrgbaTuple(0.5, 0.5, 0.5, 0.75);
        let lin = t.to_linear();
        assert!((lin.3 - 0.75).abs() < 0.001);
    }

    #[test]
    fn linear_rgba_to_srgb_preserves_alpha() {
        let lin = LinearRgba(0.5, 0.5, 0.5, 0.3);
        let srgb = lin.to_srgb();
        assert!((srgb.3 - 0.3).abs() < 0.001);
    }

    #[test]
    fn srgba_tuple_mul_alpha_zero_makes_transparent() {
        let t = SrgbaTuple(1.0, 0.5, 0.25, 1.0);
        let t2 = t.mul_alpha(0.0);
        assert_eq!(t2.3, 0.0);
        assert_eq!(t2.0, 1.0); // RGB unchanged
    }

    #[test]
    fn linear_rgba_with_components_debug() {
        let c = LinearRgba::with_components(0.1, 0.2, 0.3, 0.4);
        let debug = format!("{c:?}");
        assert!(debug.contains("LinearRgba"));
    }

    #[test]
    fn srgba_pixel_to_linear_black() {
        let p = SrgbaPixel::rgba(0, 0, 0, 255);
        let lin = p.to_linear();
        assert_eq!(lin.0, 0.0);
        assert_eq!(lin.1, 0.0);
        assert_eq!(lin.2, 0.0);
    }

    #[test]
    fn srgba_tuple_from_u8_triple_alpha_is_one() {
        let t: SrgbaTuple = (100u8, 100u8, 100u8).into();
        assert!((t.3 - 1.0).abs() < 0.001);
    }

    #[cfg(feature = "std")]
    #[test]
    fn srgba_tuple_hashset_dedup() {
        use std::collections::HashSet;
        let a = SrgbaTuple(0.5, 0.5, 0.5, 1.0);
        let b = SrgbaTuple(0.5, 0.5, 0.5, 1.0);
        let mut set = HashSet::new();
        set.insert(a);
        set.insert(b);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn linear_rgba_clone_eq() {
        let a = LinearRgba(0.1, 0.2, 0.3, 0.4);
        let b = a;
        assert_eq!(a, b);
    }
}
