//! Canonical semantic terminal-state checkpoint model.
//!
//! This module projects live terminal state into deterministic, capability-free
//! data. It does not own filesystem publication or raw-output identity; the mux
//! guardian binds encoded bytes to `GuardianCheckpointBoundary` after capture.

use super::*;
use crate::color::{ColorAttribute, SrgbaTuple};
use frankenterm_escape_parser::csi::KittyKeyboardFlags;
use serde::de::DeserializeSeed;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::convert::TryFrom;
use std::sync::Arc;
use zeroize::Zeroizing;

/// Current semantic terminal checkpoint schema.
pub const TERMINAL_CHECKPOINT_VERSION: u32 = 2;

fn resource_limit(
    resource: &'static str,
    observed: usize,
    maximum: usize,
) -> TerminalCheckpointError {
    TerminalCheckpointError::ResourceLimit {
        resource,
        observed,
        maximum,
    }
}

fn ensure_limit(
    resource: &'static str,
    observed: usize,
    maximum: usize,
) -> Result<(), TerminalCheckpointError> {
    if observed > maximum {
        return Err(resource_limit(resource, observed, maximum));
    }
    Ok(())
}

fn checked_accumulate(
    current: &mut usize,
    additional: usize,
    maximum: usize,
    resource: &'static str,
) -> Result<(), TerminalCheckpointError> {
    let observed = current
        .checked_add(additional)
        .ok_or(TerminalCheckpointError::ArithmeticOverflow(resource))?;
    ensure_limit(resource, observed, maximum)?;
    *current = observed;
    Ok(())
}

fn usize_from_u64(value: u64, field: &'static str) -> Result<usize, TerminalCheckpointError> {
    usize::try_from(value).map_err(|_| TerminalCheckpointError::InvalidField {
        field,
        reason: "value does not fit this target architecture",
    })
}

fn u64_from_usize(value: usize, field: &'static str) -> Result<u64, TerminalCheckpointError> {
    u64::try_from(value).map_err(|_| TerminalCheckpointError::InvalidField {
        field,
        reason: "value does not fit the checkpoint wire type",
    })
}

struct BoundedCheckpointWriter {
    bytes: Zeroizing<Vec<u8>>,
    maximum: usize,
    failure: Option<BoundedWriterFailure>,
}

#[derive(Clone, Copy)]
enum BoundedWriterFailure {
    Limit,
    Allocation,
}

impl BoundedCheckpointWriter {
    fn new(maximum: usize) -> Self {
        Self {
            bytes: Zeroizing::new(Vec::new()),
            maximum,
            failure: None,
        }
    }

    fn into_inner(mut self) -> Zeroizing<Vec<u8>> {
        std::mem::take(&mut self.bytes)
    }
}

impl std::io::Write for BoundedCheckpointWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let observed = match self.bytes.len().checked_add(buffer.len()) {
            Some(observed) => observed,
            None => {
                self.failure = Some(BoundedWriterFailure::Limit);
                return Err(std::io::Error::other("checkpoint output limit"));
            }
        };
        if observed > self.maximum {
            self.failure = Some(BoundedWriterFailure::Limit);
            return Err(std::io::Error::other("checkpoint output limit"));
        }
        if self.bytes.try_reserve(buffer.len()).is_err() {
            self.failure = Some(BoundedWriterFailure::Allocation);
            return Err(std::io::Error::other("checkpoint output allocation"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct JsonStructuralBudget {
    retained_bytes: usize,
    limits: TerminalCheckpointLimits,
}

impl JsonStructuralBudget {
    const MAX_DEPTH: usize = 128;
    const BYTES_PER_NODE: usize = 64;

    fn charge(&mut self, bytes: usize) -> Result<(), &'static str> {
        self.retained_bytes = self
            .retained_bytes
            .checked_add(bytes)
            .ok_or("checkpoint structural accounting overflow")?;
        if self.retained_bytes > self.limits.max_retained_capture_bytes {
            return Err("checkpoint structural memory limit");
        }
        Ok(())
    }

    fn charge_node(&mut self) -> Result<(), &'static str> {
        self.charge(Self::BYTES_PER_NODE)
    }

    fn charge_string(&mut self, value: &str) -> Result<(), &'static str> {
        if value.len() > self.limits.max_string_bytes {
            return Err("checkpoint string limit");
        }
        self.charge_node()?;
        self.charge(value.len())
    }
}

struct JsonStructuralSeed<'a> {
    budget: &'a mut JsonStructuralBudget,
    depth: usize,
}

impl<'de> serde::de::DeserializeSeed<'de> for JsonStructuralSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(JsonStructuralVisitor {
            budget: self.budget,
            depth: self.depth,
        })
    }
}

struct JsonStructuralVisitor<'a> {
    budget: &'a mut JsonStructuralBudget,
    depth: usize,
}

impl JsonStructuralVisitor<'_> {
    fn charge_node<E: serde::de::Error>(&mut self) -> Result<(), E> {
        self.budget.charge_node().map_err(E::custom)
    }

    fn charge_string<E: serde::de::Error>(&mut self, value: &str) -> Result<(), E> {
        self.budget.charge_string(value).map_err(E::custom)
    }

    fn child_depth<E: serde::de::Error>(&self) -> Result<usize, E> {
        let depth = self
            .depth
            .checked_add(1)
            .ok_or_else(|| E::custom("checkpoint nesting overflow"))?;
        if depth > JsonStructuralBudget::MAX_DEPTH {
            return Err(E::custom("checkpoint nesting limit"));
        }
        Ok(depth)
    }
}

impl<'de> serde::de::Visitor<'de> for JsonStructuralVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("bounded checkpoint JSON")
    }

    fn visit_bool<E: serde::de::Error>(mut self, _value: bool) -> Result<(), E> {
        self.charge_node()
    }

    fn visit_i64<E: serde::de::Error>(mut self, _value: i64) -> Result<(), E> {
        self.charge_node()
    }

    fn visit_u64<E: serde::de::Error>(mut self, _value: u64) -> Result<(), E> {
        self.charge_node()
    }

    fn visit_f64<E: serde::de::Error>(mut self, _value: f64) -> Result<(), E> {
        self.charge_node()
    }

    fn visit_unit<E: serde::de::Error>(mut self) -> Result<(), E> {
        self.charge_node()
    }

    fn visit_none<E: serde::de::Error>(self) -> Result<(), E> {
        self.visit_unit()
    }

    fn visit_some<D>(self, deserializer: D) -> Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        JsonStructuralSeed {
            budget: self.budget,
            depth: self.depth,
        }
        .deserialize(deserializer)
    }

    fn visit_borrowed_str<E: serde::de::Error>(mut self, value: &'de str) -> Result<(), E> {
        self.charge_string(value)
    }

    fn visit_str<E: serde::de::Error>(mut self, value: &str) -> Result<(), E> {
        self.charge_string(value)
    }

    fn visit_string<E: serde::de::Error>(mut self, value: String) -> Result<(), E> {
        self.charge_string(&value)
    }

    fn visit_seq<A>(mut self, mut sequence: A) -> Result<(), A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        self.charge_node()?;
        let depth = self.child_depth()?;
        while sequence
            .next_element_seed(JsonStructuralSeed {
                budget: self.budget,
                depth,
            })?
            .is_some()
        {}
        Ok(())
    }

    fn visit_map<A>(mut self, mut map: A) -> Result<(), A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        self.charge_node()?;
        let depth = self.child_depth()?;
        while map
            .next_key_seed(JsonStructuralSeed {
                budget: self.budget,
                depth,
            })?
            .is_some()
        {
            map.next_value_seed(JsonStructuralSeed {
                budget: self.budget,
                depth,
            })?;
        }
        Ok(())
    }
}

/// A checkpoint that passed canonical decoding, semantic validation, and all
/// target-architecture conversions.  This type deliberately has no Deserialize
/// implementation and is the only checkpoint authority accepted by restore.
pub struct ValidatedTerminalCheckpointV2 {
    checkpoint: TerminalCheckpointV2,
    limits: TerminalCheckpointLimits,
}

impl std::fmt::Debug for ValidatedTerminalCheckpointV2 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("ValidatedTerminalCheckpointV2")
            .field(&self.checkpoint)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointSrgba {
    red_bits: u32,
    green_bits: u32,
    blue_bits: u32,
    alpha_bits: u32,
}

impl CheckpointSrgba {
    fn canonical_component_bits(
        component: f32,
        field: &'static str,
    ) -> Result<u32, TerminalCheckpointError> {
        if !component.is_finite() {
            return Err(TerminalCheckpointError::InvalidField {
                field,
                reason: "color components must be finite",
            });
        }
        if !(0.0..=1.0).contains(&component) {
            return Err(TerminalCheckpointError::InvalidField {
                field,
                reason: "color components must be in the inclusive range zero to one",
            });
        }
        Ok(if component == 0.0 {
            0.0f32.to_bits()
        } else {
            component.to_bits()
        })
    }

    fn capture(value: SrgbaTuple) -> Result<Self, TerminalCheckpointError> {
        Ok(Self {
            red_bits: Self::canonical_component_bits(value.0, "color.red")?,
            green_bits: Self::canonical_component_bits(value.1, "color.green")?,
            blue_bits: Self::canonical_component_bits(value.2, "color.blue")?,
            alpha_bits: Self::canonical_component_bits(value.3, "color.alpha")?,
        })
    }

    fn validate(self) -> Result<(), TerminalCheckpointError> {
        for (field, bits) in [
            ("color.red", self.red_bits),
            ("color.green", self.green_bits),
            ("color.blue", self.blue_bits),
            ("color.alpha", self.alpha_bits),
        ] {
            let value = f32::from_bits(bits);
            if !value.is_finite() {
                return Err(TerminalCheckpointError::InvalidField {
                    field,
                    reason: "color components must be finite",
                });
            }
            if !(0.0..=1.0).contains(&value) {
                return Err(TerminalCheckpointError::InvalidField {
                    field,
                    reason: "color components must be in the inclusive range zero to one",
                });
            }
            if value == 0.0 && bits != 0 {
                return Err(TerminalCheckpointError::InvalidField {
                    field,
                    reason: "negative zero is not canonical",
                });
            }
        }
        Ok(())
    }

    fn into_live(self) -> SrgbaTuple {
        SrgbaTuple(
            f32::from_bits(self.red_bits),
            f32::from_bits(self.green_bits),
            f32::from_bits(self.blue_bits),
            f32::from_bits(self.alpha_bits),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum CheckpointColorAttribute {
    Default,
    PaletteIndex {
        index: u8,
    },
    TrueColorWithDefaultFallback {
        color: CheckpointSrgba,
    },
    TrueColorWithPaletteFallback {
        color: CheckpointSrgba,
        index: u8,
    },
}

impl CheckpointColorAttribute {
    fn capture(value: ColorAttribute) -> Result<Self, TerminalCheckpointError> {
        Ok(match value {
            ColorAttribute::Default => Self::Default,
            ColorAttribute::PaletteIndex(index) => Self::PaletteIndex { index },
            ColorAttribute::TrueColorWithDefaultFallback(color) => {
                Self::TrueColorWithDefaultFallback {
                    color: CheckpointSrgba::capture(color)?,
                }
            }
            ColorAttribute::TrueColorWithPaletteFallback(color, index) => {
                Self::TrueColorWithPaletteFallback {
                    color: CheckpointSrgba::capture(color)?,
                    index,
                }
            }
        })
    }

    fn validate(self) -> Result<(), TerminalCheckpointError> {
        match self {
            Self::Default | Self::PaletteIndex { .. } => Ok(()),
            Self::TrueColorWithDefaultFallback { color }
            | Self::TrueColorWithPaletteFallback { color, .. } => color.validate(),
        }
    }

    fn into_live(self) -> ColorAttribute {
        match self {
            Self::Default => ColorAttribute::Default,
            Self::PaletteIndex { index } => ColorAttribute::PaletteIndex(index),
            Self::TrueColorWithDefaultFallback { color } => {
                ColorAttribute::TrueColorWithDefaultFallback(color.into_live())
            }
            Self::TrueColorWithPaletteFallback { color, index } => {
                ColorAttribute::TrueColorWithPaletteFallback(color.into_live(), index)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
struct CheckpointRgbColor(u32);

impl From<RgbColor> for CheckpointRgbColor {
    fn from(value: RgbColor) -> Self {
        let (red, green, blue) = value.to_tuple_rgb8();
        Self((u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue))
    }
}

impl CheckpointRgbColor {
    fn validate(self) -> Result<(), TerminalCheckpointError> {
        if self.0 & 0xff00_0000 != 0 {
            return Err(TerminalCheckpointError::InvalidField {
                field: "color_map",
                reason: "RGB colors must use exactly 24 bits",
            });
        }
        Ok(())
    }

    fn into_live(self) -> RgbColor {
        RgbColor::new_8bpc(
            (self.0 >> 16) as u8,
            (self.0 >> 8) as u8,
            self.0 as u8,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointColorPalette {
    colors: Vec<CheckpointSrgba>,
    foreground: CheckpointSrgba,
    background: CheckpointSrgba,
    cursor_fg: CheckpointSrgba,
    cursor_bg: CheckpointSrgba,
    cursor_border: CheckpointSrgba,
    selection_fg: CheckpointSrgba,
    selection_bg: CheckpointSrgba,
    scrollbar_thumb: CheckpointSrgba,
    split: CheckpointSrgba,
}

impl CheckpointColorPalette {
    fn capture(value: &ColorPalette) -> Result<Self, TerminalCheckpointError> {
        let mut colors = Vec::new();
        colors
            .try_reserve_exact(value.colors.0.len())
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("palette.colors"))?;
        for color in value.colors.0.iter().copied() {
            colors.push(CheckpointSrgba::capture(color)?);
        }
        Ok(Self {
            colors,
            foreground: CheckpointSrgba::capture(value.foreground)?,
            background: CheckpointSrgba::capture(value.background)?,
            cursor_fg: CheckpointSrgba::capture(value.cursor_fg)?,
            cursor_bg: CheckpointSrgba::capture(value.cursor_bg)?,
            cursor_border: CheckpointSrgba::capture(value.cursor_border)?,
            selection_fg: CheckpointSrgba::capture(value.selection_fg)?,
            selection_bg: CheckpointSrgba::capture(value.selection_bg)?,
            scrollbar_thumb: CheckpointSrgba::capture(value.scrollbar_thumb)?,
            split: CheckpointSrgba::capture(value.split)?,
        })
    }

    fn validate(&self) -> Result<(), TerminalCheckpointError> {
        if self.colors.len() != 256 {
            return Err(TerminalCheckpointError::InvalidField {
                field: "palette.colors",
                reason: "palette must contain exactly 256 colors",
            });
        }
        for color in self.colors.iter().copied().chain([
            self.foreground,
            self.background,
            self.cursor_fg,
            self.cursor_bg,
            self.cursor_border,
            self.selection_fg,
            self.selection_bg,
            self.scrollbar_thumb,
            self.split,
        ]) {
            color.validate()?;
        }
        Ok(())
    }

    fn into_live(self) -> Result<ColorPalette, TerminalCheckpointError> {
        if self.colors.len() != 256 {
            return Err(TerminalCheckpointError::InvalidField {
                field: "palette.colors",
                reason: "palette must contain exactly 256 colors",
            });
        }
        let mut colors = [SrgbaTuple::default(); 256];
        for (destination, source) in colors.iter_mut().zip(self.colors) {
            *destination = source.into_live();
        }
        Ok(ColorPalette {
            colors: crate::color::Palette256(colors),
            foreground: self.foreground.into_live(),
            background: self.background.into_live(),
            cursor_fg: self.cursor_fg.into_live(),
            cursor_bg: self.cursor_bg.into_live(),
            cursor_border: self.cursor_border.into_live(),
            selection_fg: self.selection_fg.into_live(),
            selection_bg: self.selection_bg.into_live(),
            scrollbar_thumb: self.scrollbar_thumb.into_live(),
            split: self.split.into_live(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointCursorPosition {
    x: u64,
    y: i64,
    shape: frankenterm_surface::CursorShape,
    seqno: u64,
}

impl CheckpointCursorPosition {
    fn capture(value: CursorPosition) -> Result<Self, TerminalCheckpointError> {
        Ok(Self {
            x: u64_from_usize(value.x, "cursor.x")?,
            y: value.y,
            shape: value.shape,
            seqno: u64_from_usize(value.seqno, "cursor.seqno")?,
        })
    }

    fn into_live(self) -> Result<CursorPosition, TerminalCheckpointError> {
        Ok(CursorPosition {
            x: usize_from_u64(self.x, "cursor.x")?,
            y: self.y,
            shape: self.shape,
            visibility: frankenterm_surface::CursorVisibility::Visible,
            seqno: usize_from_u64(self.seqno, "cursor.seqno")?,
        })
    }

    fn validate(
        self,
        cols: u32,
        rows: u32,
        terminal_seqno: u64,
        field: &'static str,
    ) -> Result<(), TerminalCheckpointError> {
        if self.x >= u64::from(cols) {
            return Err(TerminalCheckpointError::InvalidField {
                field,
                reason: "cursor column lies outside the screen",
            });
        }
        if self.y < 0 || self.y >= i64::from(rows) {
            return Err(TerminalCheckpointError::InvalidField {
                field,
                reason: "cursor row lies outside the screen",
            });
        }
        if self.seqno > terminal_seqno {
            return Err(TerminalCheckpointError::InvalidField {
                field,
                reason: "cursor sequence number is newer than the terminal",
            });
        }
        Ok(())
    }

    fn validate_retained(
        self,
        terminal_seqno: u64,
        field: &'static str,
    ) -> Result<(), TerminalCheckpointError> {
        usize_from_u64(self.x, field)?;
        if self.seqno > terminal_seqno {
            return Err(TerminalCheckpointError::InvalidField {
                field,
                reason: "retained cursor sequence number is newer than the terminal",
            });
        }
        usize_from_u64(self.seqno, field)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "count")]
enum CheckpointMouseButton {
    Left,
    Middle,
    Right,
    WheelUp(u64),
    WheelDown(u64),
    WheelLeft(u64),
    WheelRight(u64),
    None,
}

impl CheckpointMouseButton {
    fn capture(value: MouseButton) -> Result<Self, TerminalCheckpointError> {
        Ok(match value {
            MouseButton::Left => Self::Left,
            MouseButton::Middle => Self::Middle,
            MouseButton::Right => Self::Right,
            MouseButton::WheelUp(count) => {
                Self::WheelUp(u64_from_usize(count, "mouse_button.count")?)
            }
            MouseButton::WheelDown(count) => {
                Self::WheelDown(u64_from_usize(count, "mouse_button.count")?)
            }
            MouseButton::WheelLeft(count) => {
                Self::WheelLeft(u64_from_usize(count, "mouse_button.count")?)
            }
            MouseButton::WheelRight(count) => {
                Self::WheelRight(u64_from_usize(count, "mouse_button.count")?)
            }
            MouseButton::None => Self::None,
        })
    }

    fn into_live(self) -> Result<MouseButton, TerminalCheckpointError> {
        Ok(match self {
            Self::Left => MouseButton::Left,
            Self::Middle => MouseButton::Middle,
            Self::Right => MouseButton::Right,
            Self::WheelUp(count) => {
                MouseButton::WheelUp(usize_from_u64(count, "mouse_button.count")?)
            }
            Self::WheelDown(count) => {
                MouseButton::WheelDown(usize_from_u64(count, "mouse_button.count")?)
            }
            Self::WheelLeft(count) => {
                MouseButton::WheelLeft(usize_from_u64(count, "mouse_button.count")?)
            }
            Self::WheelRight(count) => {
                MouseButton::WheelRight(usize_from_u64(count, "mouse_button.count")?)
            }
            Self::None => MouseButton::None,
        })
    }

    const fn pressed_bit(self) -> Option<u8> {
        match self {
            Self::Left => Some(1),
            Self::Middle => Some(2),
            Self::Right => Some(4),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointMouseEvent {
    kind: MouseEventKind,
    x: u64,
    y: i64,
    x_pixel_offset: i64,
    y_pixel_offset: i64,
    button: CheckpointMouseButton,
    modifier_bits: u16,
}

impl CheckpointMouseEvent {
    fn capture(value: MouseEvent) -> Result<Self, TerminalCheckpointError> {
        Ok(Self {
            kind: value.kind,
            x: u64_from_usize(value.x, "last_mouse_move.x")?,
            y: value.y,
            x_pixel_offset: i64::try_from(value.x_pixel_offset).map_err(|_| {
                TerminalCheckpointError::InvalidField {
                    field: "last_mouse_move.x_pixel_offset",
                    reason: "value does not fit the checkpoint wire type",
                }
            })?,
            y_pixel_offset: i64::try_from(value.y_pixel_offset).map_err(|_| {
                TerminalCheckpointError::InvalidField {
                    field: "last_mouse_move.y_pixel_offset",
                    reason: "value does not fit the checkpoint wire type",
                }
            })?,
            button: CheckpointMouseButton::capture(value.button)?,
            modifier_bits: value.modifiers.bits(),
        })
    }

    fn into_live(self) -> Result<MouseEvent, TerminalCheckpointError> {
        Ok(MouseEvent {
            kind: self.kind,
            x: usize_from_u64(self.x, "last_mouse_move.x")?,
            y: self.y,
            x_pixel_offset: isize::try_from(self.x_pixel_offset).map_err(|_| {
                TerminalCheckpointError::InvalidField {
                    field: "last_mouse_move.x_pixel_offset",
                    reason: "value does not fit this target architecture",
                }
            })?,
            y_pixel_offset: isize::try_from(self.y_pixel_offset).map_err(|_| {
                TerminalCheckpointError::InvalidField {
                    field: "last_mouse_move.y_pixel_offset",
                    reason: "value does not fit this target architecture",
                }
            })?,
            button: self.button.into_live()?,
            modifiers: KeyModifiers::from_bits(self.modifier_bits).ok_or(
                TerminalCheckpointError::InvalidField {
                    field: "last_mouse_move.modifier_bits",
                    reason: "unknown modifier bits",
                },
            )?,
        })
    }

    fn validate(self) -> Result<(), TerminalCheckpointError> {
        if self.kind != MouseEventKind::Move {
            return Err(TerminalCheckpointError::InvalidField {
                field: "last_mouse_move.kind",
                reason: "the retained mouse event must be a move",
            });
        }
        usize_from_u64(self.x, "last_mouse_move.x")?;
        KeyModifiers::from_bits(self.modifier_bits).ok_or(
            TerminalCheckpointError::InvalidField {
                field: "last_mouse_move.modifier_bits",
                reason: "unknown modifier bits",
            },
        )?;
        isize::try_from(self.x_pixel_offset).map_err(|_| {
            TerminalCheckpointError::InvalidField {
                field: "last_mouse_move.x_pixel_offset",
                reason: "value does not fit this target architecture",
            }
        })?;
        isize::try_from(self.y_pixel_offset).map_err(|_| {
            TerminalCheckpointError::InvalidField {
                field: "last_mouse_move.y_pixel_offset",
                reason: "value does not fit this target architecture",
            }
        })?;
        self.button.into_live()?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointCharSet {
    Ascii,
    Uk,
    DecLineDrawing,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointSavedDecMode {
    ApplicationCursorKeys,
    DecAnsiMode,
    ReverseVideo,
    OriginMode,
    AutoWrap,
    ShowCursor,
    ReverseWraparound,
    EnableAlternateScreen,
    LeftRightMarginMode,
    SixelDisplayMode,
    MouseTracking,
    ButtonEventMouse,
    AnyEventMouse,
    FocusTracking,
    Utf8Mouse,
    SgrMouse,
    SgrPixelsMouse,
    OptEnableAlternateScreen,
    ClearAndEnableAlternateScreen,
    UsePrivateColorRegistersForEachGraphic,
    BracketedPaste,
    SynchronizedOutput,
    SixelScrollsRight,
    Win32InputMode,
}

impl CheckpointSavedDecMode {
    fn capture(code: u16) -> Result<Self, TerminalCheckpointError> {
        Ok(match code {
            1 => Self::ApplicationCursorKeys,
            2 => Self::DecAnsiMode,
            5 => Self::ReverseVideo,
            6 => Self::OriginMode,
            7 => Self::AutoWrap,
            25 => Self::ShowCursor,
            45 => Self::ReverseWraparound,
            47 => Self::EnableAlternateScreen,
            69 => Self::LeftRightMarginMode,
            80 => Self::SixelDisplayMode,
            1000 => Self::MouseTracking,
            1002 => Self::ButtonEventMouse,
            1003 => Self::AnyEventMouse,
            1004 => Self::FocusTracking,
            1005 => Self::Utf8Mouse,
            1006 => Self::SgrMouse,
            1016 => Self::SgrPixelsMouse,
            1047 => Self::OptEnableAlternateScreen,
            1049 => Self::ClearAndEnableAlternateScreen,
            1070 => Self::UsePrivateColorRegistersForEachGraphic,
            2004 => Self::BracketedPaste,
            2026 => Self::SynchronizedOutput,
            8452 => Self::SixelScrollsRight,
            9001 => Self::Win32InputMode,
            _ => {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "saved_dec_private_modes",
                    reason: "saved mode is not producer-reachable",
                });
            }
        })
    }

    const fn into_live(self) -> u16 {
        match self {
            Self::ApplicationCursorKeys => 1,
            Self::DecAnsiMode => 2,
            Self::ReverseVideo => 5,
            Self::OriginMode => 6,
            Self::AutoWrap => 7,
            Self::ShowCursor => 25,
            Self::ReverseWraparound => 45,
            Self::EnableAlternateScreen => 47,
            Self::LeftRightMarginMode => 69,
            Self::SixelDisplayMode => 80,
            Self::MouseTracking => 1000,
            Self::ButtonEventMouse => 1002,
            Self::AnyEventMouse => 1003,
            Self::FocusTracking => 1004,
            Self::Utf8Mouse => 1005,
            Self::SgrMouse => 1006,
            Self::SgrPixelsMouse => 1016,
            Self::OptEnableAlternateScreen => 1047,
            Self::ClearAndEnableAlternateScreen => 1049,
            Self::UsePrivateColorRegistersForEachGraphic => 1070,
            Self::BracketedPaste => 2004,
            Self::SynchronizedOutput => 2026,
            Self::SixelScrollsRight => 8452,
            Self::Win32InputMode => 9001,
        }
    }
}

impl From<CharSet> for CheckpointCharSet {
    fn from(value: CharSet) -> Self {
        match value {
            CharSet::Ascii => Self::Ascii,
            CharSet::Uk => Self::Uk,
            CharSet::DecLineDrawing => Self::DecLineDrawing,
        }
    }
}

impl CheckpointCharSet {
    const fn into_live(self) -> CharSet {
        match self {
            Self::Ascii => CharSet::Ascii,
            Self::Uk => CharSet::Uk,
            Self::DecLineDrawing => CharSet::DecLineDrawing,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointMouseEncoding {
    X10,
    Utf8,
    Sgr,
    SgrPixels,
}

impl From<MouseEncoding> for CheckpointMouseEncoding {
    fn from(value: MouseEncoding) -> Self {
        match value {
            MouseEncoding::X10 => Self::X10,
            MouseEncoding::Utf8 => Self::Utf8,
            MouseEncoding::SGR => Self::Sgr,
            MouseEncoding::SgrPixels => Self::SgrPixels,
        }
    }
}

impl CheckpointMouseEncoding {
    const fn into_live(self) -> MouseEncoding {
        match self {
            Self::X10 => MouseEncoding::X10,
            Self::Utf8 => MouseEncoding::Utf8,
            Self::Sgr => MouseEncoding::SGR,
            Self::SgrPixels => MouseEncoding::SgrPixels,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "flags")]
enum CheckpointKeyboardEncoding {
    Xterm,
    CsiU,
    Win32,
    Kitty(u16),
}

impl From<KeyboardEncoding> for CheckpointKeyboardEncoding {
    fn from(value: KeyboardEncoding) -> Self {
        match value {
            KeyboardEncoding::Xterm => Self::Xterm,
            KeyboardEncoding::CsiU => Self::CsiU,
            KeyboardEncoding::Win32 => Self::Win32,
            KeyboardEncoding::Kitty(flags) => Self::Kitty(flags.bits()),
        }
    }
}

impl CheckpointKeyboardEncoding {
    fn into_live(self) -> Result<KeyboardEncoding, TerminalCheckpointError> {
        Ok(match self {
            Self::Xterm => KeyboardEncoding::Xterm,
            Self::CsiU => KeyboardEncoding::CsiU,
            Self::Win32 => KeyboardEncoding::Win32,
            Self::Kitty(bits) => KeyboardEncoding::Kitty(
                KittyKeyboardFlags::from_bits(bits).ok_or(
                    TerminalCheckpointError::InvalidField {
                        field: "keyboard_encoding",
                        reason: "unknown Kitty keyboard flag bits",
                    },
                )?,
            ),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointBidiHint {
    LeftToRight,
    RightToLeft,
    AutoLeftToRight,
    AutoRightToLeft,
}

impl From<ParagraphDirectionHint> for CheckpointBidiHint {
    fn from(value: ParagraphDirectionHint) -> Self {
        match value {
            ParagraphDirectionHint::LeftToRight => Self::LeftToRight,
            ParagraphDirectionHint::RightToLeft => Self::RightToLeft,
            ParagraphDirectionHint::AutoLeftToRight => Self::AutoLeftToRight,
            ParagraphDirectionHint::AutoRightToLeft => Self::AutoRightToLeft,
        }
    }
}

impl CheckpointBidiHint {
    const fn into_live(self) -> ParagraphDirectionHint {
        match self {
            Self::LeftToRight => ParagraphDirectionHint::LeftToRight,
            Self::RightToLeft => ParagraphDirectionHint::RightToLeft,
            Self::AutoLeftToRight => ParagraphDirectionHint::AutoLeftToRight,
            Self::AutoRightToLeft => ParagraphDirectionHint::AutoRightToLeft,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointHyperlink {
    params: BTreeMap<String, String>,
    uri: String,
    implicit: bool,
}

impl CheckpointHyperlink {
    fn capture(link: &Hyperlink) -> Self {
        Self {
            params: link
                .params()
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            uri: link.uri().to_string(),
            implicit: link.is_implicit(),
        }
    }

    fn into_live(self) -> Result<Hyperlink, TerminalCheckpointError> {
        let mut params = HashMap::new();
        params
            .try_reserve(self.params.len())
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("hyperlink.params"))?;
        params.extend(self.params);
        Ok(Hyperlink::new_with_params_and_implicit(
            self.uri,
            params,
            self.implicit,
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointCellAttributes {
    intensity: Intensity,
    underline: Underline,
    blink: Blink,
    italic: bool,
    reverse: bool,
    strikethrough: bool,
    invisible: bool,
    wrapped: bool,
    overline: bool,
    semantic_type: SemanticType,
    vertical_align: VerticalAlign,
    foreground: CheckpointColorAttribute,
    background: CheckpointColorAttribute,
    underline_color: CheckpointColorAttribute,
    hyperlink: Option<CheckpointHyperlink>,
}

impl CheckpointCellAttributes {
    fn capture(value: &CellAttributes) -> Result<Self, TerminalCheckpointError> {
        if value.has_image_attachments() {
            return Err(TerminalCheckpointError::UnsupportedGraphicsState);
        }
        Ok(Self {
            intensity: value.intensity(),
            underline: value.underline(),
            blink: value.blink(),
            italic: value.italic(),
            reverse: value.reverse(),
            strikethrough: value.strikethrough(),
            invisible: value.invisible(),
            wrapped: value.wrapped(),
            overline: value.overline(),
            semantic_type: value.semantic_type(),
            vertical_align: value.vertical_align(),
            foreground: CheckpointColorAttribute::capture(value.foreground())?,
            background: CheckpointColorAttribute::capture(value.background())?,
            underline_color: CheckpointColorAttribute::capture(value.underline_color())?,
            hyperlink: value
                .hyperlink()
                .map(|link| CheckpointHyperlink::capture(link)),
        })
    }

    fn into_live(self) -> Result<CellAttributes, TerminalCheckpointError> {
        let mut value = CellAttributes::blank();
        value
            .set_intensity(self.intensity)
            .set_underline(self.underline)
            .set_blink(self.blink)
            .set_italic(self.italic)
            .set_reverse(self.reverse)
            .set_strikethrough(self.strikethrough)
            .set_invisible(self.invisible)
            .set_wrapped(self.wrapped)
            .set_overline(self.overline)
            .set_semantic_type(self.semantic_type)
            .set_vertical_align(self.vertical_align)
            .set_foreground(self.foreground.into_live())
            .set_background(self.background.into_live())
            .set_underline_color(self.underline_color.into_live());
        value.set_hyperlink(
            self.hyperlink
                .map(CheckpointHyperlink::into_live)
                .transpose()?
                .map(Arc::new),
        );
        Ok(value)
    }

    fn validate(
        &self,
        limits: TerminalCheckpointLimits,
        usage: &mut crate::screen::ScreenCheckpointUsage,
    ) -> Result<(), TerminalCheckpointError> {
        self.foreground.validate()?;
        self.background.validate()?;
        self.underline_color.validate()?;
        let Some(link) = self.hyperlink.as_ref() else {
            return Ok(());
        };
        ensure_limit(
            "hyperlink_params_per_link",
            link.params.len(),
            limits.max_hyperlink_params_per_link,
        )?;
        checked_accumulate(
            &mut usage.hyperlink_params,
            link.params.len(),
            limits.max_total_hyperlink_params,
            "hyperlink_params",
        )?;
        ensure_limit(
            "hyperlink_uri_bytes",
            link.uri.len(),
            limits.max_string_bytes,
        )?;
        let mut link_bytes = link.uri.len();
        for (key, value) in &link.params {
            ensure_limit(
                "hyperlink_param_key_bytes",
                key.len(),
                limits.max_string_bytes,
            )?;
            ensure_limit(
                "hyperlink_param_value_bytes",
                value.len(),
                limits.max_string_bytes,
            )?;
            link_bytes = link_bytes
                .checked_add(key.len())
                .and_then(|bytes| bytes.checked_add(value.len()))
                .ok_or(TerminalCheckpointError::ArithmeticOverflow(
                    "hyperlink_bytes",
                ))?;
        }
        checked_accumulate(
            &mut usage.hyperlink_bytes,
            link_bytes,
            limits.max_total_hyperlink_bytes,
            "hyperlink_bytes",
        )?;
        checked_accumulate(
            &mut usage.retained_capture_bytes,
            link_bytes,
            limits.max_retained_capture_bytes,
            "retained_capture_bytes",
        )?;
        let parameter_overhead = link
            .params
            .len()
            .checked_mul(2 * std::mem::size_of::<String>() + 48)
            .ok_or(TerminalCheckpointError::ArithmeticOverflow(
                "retained_capture_bytes",
            ))?;
        checked_accumulate(
            &mut usage.retained_capture_bytes,
            parameter_overhead,
            limits.max_retained_capture_bytes,
            "retained_capture_bytes",
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointCell {
    text: String,
    width: u8,
    attributes: CheckpointCellAttributes,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointLineScale {
    Single,
    DoubleWidth,
    DoubleHeightTop,
    DoubleHeightBottom,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointLine {
    cells: Vec<CheckpointCell>,
    seqno: u64,
    scale: CheckpointLineScale,
    bidi_enabled: bool,
    bidi_hint: CheckpointBidiHint,
}

impl CheckpointLine {
    fn capture(line: &Line) -> Result<Self, TerminalCheckpointError> {
        if line.has_image_attachments() {
            return Err(TerminalCheckpointError::UnsupportedGraphicsState);
        }
        let mut cells = Vec::new();
        cells
            .try_reserve_exact(line.len())
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("line.cells"))?;
        for cell in line.visible_cells() {
            let width = u8::try_from(cell.width()).map_err(|_| {
                TerminalCheckpointError::InvalidField {
                    field: "cell.width",
                    reason: "cell width does not fit the checkpoint wire type",
                }
            })?;
            if !(1..=2).contains(&width) {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "cell.width",
                    reason: "cell width must be one or two columns",
                });
            }
            cells.push(CheckpointCell {
                text: cell.str().to_string(),
                width,
                attributes: CheckpointCellAttributes::capture(cell.attrs())?,
            });
        }
        let scale = if line.is_double_height_top() {
            CheckpointLineScale::DoubleHeightTop
        } else if line.is_double_height_bottom() {
            CheckpointLineScale::DoubleHeightBottom
        } else if line.is_double_width() {
            CheckpointLineScale::DoubleWidth
        } else {
            CheckpointLineScale::Single
        };
        let (bidi_enabled, bidi_hint) = line.bidi_info();
        Ok(Self {
            cells,
            seqno: u64_from_usize(line.current_seqno(), "line.seqno")?,
            scale,
            bidi_enabled,
            bidi_hint: bidi_hint.into(),
        })
    }

    fn into_live(self) -> Result<Line, TerminalCheckpointError> {
        let seqno = usize_from_u64(self.seqno, "line.seqno")?;
        let mut cells = Vec::new();
        let stored_cells = self.cells.iter().try_fold(0usize, |total, cell| {
            total
                .checked_add(usize::from(cell.width))
                .ok_or(TerminalCheckpointError::ArithmeticOverflow("line.cells"))
        })?;
        cells
            .try_reserve_exact(stored_cells)
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("line.cells"))?;
        for cell in self.cells {
            let width = usize::from(cell.width);
            let attributes = cell.attributes.into_live()?;
            cells.push(Cell::new_grapheme_with_width(
                &cell.text,
                width,
                attributes.clone(),
            ));
            for _ in 1..width {
                cells.push(Cell::blank_with_attrs(attributes.clone()));
            }
        }
        let mut line = Line::from_cells(cells, seqno);
        match self.scale {
            CheckpointLineScale::Single => {}
            CheckpointLineScale::DoubleWidth => line.set_double_width(seqno),
            CheckpointLineScale::DoubleHeightTop => line.set_double_height_top(seqno),
            CheckpointLineScale::DoubleHeightBottom => line.set_double_height_bottom(seqno),
        }
        line.set_bidi_info(self.bidi_enabled, self.bidi_hint.into_live(), seqno);
        line.rebuild_checkpoint_hyperlink_bits();
        Ok(line)
    }

    fn validate(
        &self,
        limits: TerminalCheckpointLimits,
        usage: &mut crate::screen::ScreenCheckpointUsage,
        terminal_seqno: u64,
    ) -> Result<(), TerminalCheckpointError> {
        if self.seqno > terminal_seqno {
            return Err(TerminalCheckpointError::InvalidField {
                field: "line.seqno",
                reason: "line sequence number is newer than the terminal",
            });
        }
        checked_accumulate(
            &mut usage.lines,
            1,
            limits.max_total_lines,
            "screen_lines",
        )?;
        checked_accumulate(
            &mut usage.retained_capture_bytes,
            std::mem::size_of::<Line>() + std::mem::size_of::<CheckpointLine>(),
            limits.max_retained_capture_bytes,
            "retained_capture_bytes",
        )?;
        let mut occupied_columns = 0usize;
        for cell in &self.cells {
            checked_accumulate(
                &mut usage.cell_records,
                1,
                limits.max_total_cell_records,
                "screen_cell_records",
            )?;
            if !(1..=2).contains(&cell.width) {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "cell.width",
                    reason: "cell width must be one or two columns",
                });
            }
            occupied_columns = occupied_columns
                .checked_add(usize::from(cell.width))
                .ok_or(TerminalCheckpointError::ArithmeticOverflow(
                    "line_occupied_columns",
                ))?;
            if cell.text.is_empty() || cell.text.chars().any(char::is_control) {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "cell.text",
                    reason: "cell text must be nonempty and contain no control characters",
                });
            }
            ensure_limit(
                "cell_text_value_bytes",
                cell.text.len(),
                limits.max_string_bytes,
            )?;
            checked_accumulate(
                &mut usage.cell_text_bytes,
                cell.text.len(),
                limits.max_total_cell_text_bytes,
                "cell_text_bytes",
            )?;
            checked_accumulate(
                &mut usage.retained_capture_bytes,
                std::mem::size_of::<Cell>()
                    + std::mem::size_of::<CheckpointCell>()
                    + cell.text.len(),
                limits.max_retained_capture_bytes,
                "retained_capture_bytes",
            )?;
            cell.attributes.validate(limits, usage)?;
        }
        checked_accumulate(
            &mut usage.cells,
            occupied_columns,
            limits.max_total_cells,
            "screen_cells",
        )?;
        if occupied_columns > limits.max_cols {
            return Err(resource_limit(
                "line_occupied_columns",
                occupied_columns,
                limits.max_cols,
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointCustomCellWidth {
    codepoint: u32,
    width: u8,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointCustomCellWidthMap {
    entries: Vec<CheckpointCustomCellWidth>,
}

impl CheckpointCustomCellWidthMap {
    fn matches_live(&self, widths: &HashMap<u32, u8>) -> bool {
        self.entries.len() == widths.len()
            && self
                .entries
                .iter()
                .all(|entry| widths.get(&entry.codepoint) == Some(&entry.width))
    }

    fn validate(
        &self,
        limits: TerminalCheckpointLimits,
        custom_width_count: &mut usize,
        usage: &mut crate::screen::ScreenCheckpointUsage,
    ) -> Result<(), TerminalCheckpointError> {
        if self.entries.is_empty() {
            return Err(TerminalCheckpointError::InvalidField {
                field: "custom_cell_width_maps",
                reason: "empty custom-width maps must be represented by no reference",
            });
        }
        ensure_limit(
            "custom_cell_widths_per_map",
            self.entries.len(),
            frankenterm_cell::MAX_CUSTOM_CELL_WIDTH_EXPANSION,
        )?;
        checked_accumulate(
            custom_width_count,
            self.entries.len(),
            limits.max_total_custom_cell_widths,
            "custom_cell_widths",
        )?;
        let structural_bytes = self.entries.len().checked_mul(48).ok_or(
            TerminalCheckpointError::ArithmeticOverflow("retained_capture_bytes"),
        )?;
        checked_accumulate(
            &mut usage.retained_capture_bytes,
            structural_bytes,
            limits.max_retained_capture_bytes,
            "retained_capture_bytes",
        )?;
        let mut previous_codepoint = None;
        for entry in &self.entries {
            if char::from_u32(entry.codepoint).is_none() {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "custom_cell_width_maps.entries",
                    reason: "custom-width keys must be Unicode scalar values",
                });
            }
            if previous_codepoint.is_some_and(|previous| previous >= entry.codepoint) {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "custom_cell_width_maps.entries",
                    reason: "custom-width keys must be strictly increasing",
                });
            }
            if !(1..=2).contains(&entry.width) {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "custom_cell_width_maps.entries",
                    reason: "custom widths must be one or two columns",
                });
            }
            previous_codepoint = Some(entry.codepoint);
        }
        Ok(())
    }

    fn decode(&self) -> Result<Arc<HashMap<u32, u8>>, TerminalCheckpointError> {
        let mut widths = HashMap::new();
        widths
            .try_reserve(self.entries.len())
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("custom_cell_width_maps"))?;
        widths.extend(
            self.entries
                .iter()
                .map(|entry| (entry.codepoint, entry.width)),
        );
        Ok(Arc::new(widths))
    }
}

#[derive(Default)]
struct CheckpointCustomCellWidthTableBuilder {
    tables: Vec<CheckpointCustomCellWidthMap>,
    total_entries: usize,
}

impl CheckpointCustomCellWidthTableBuilder {
    fn live_widths(value: &UnicodeVersion) -> Option<&HashMap<u32, u8>> {
        value
            .cell_widths
            .as_deref()
            .filter(|widths| !widths.is_empty())
    }

    fn live_maps_equal(left: &UnicodeVersion, right: &UnicodeVersion) -> bool {
        match (Self::live_widths(left), Self::live_widths(right)) {
            (None, None) => true,
            (Some(left), Some(right)) => left == right,
            _ => false,
        }
    }

    fn register(
        &mut self,
        value: &UnicodeVersion,
        limits: TerminalCheckpointLimits,
    ) -> Result<(), TerminalCheckpointError> {
        let Some(widths) = Self::live_widths(value) else {
            return Ok(());
        };
        if self.tables.iter().any(|table| table.matches_live(widths)) {
            return Ok(());
        }
        ensure_limit(
            "custom_cell_widths_per_map",
            widths.len(),
            frankenterm_cell::MAX_CUSTOM_CELL_WIDTH_EXPANSION,
        )?;
        if self.tables.len() >= 2 {
            return Err(TerminalCheckpointError::InvalidField {
                field: "custom_cell_width_maps",
                reason: "live terminal state can reference at most config and current width maps",
            });
        }
        let observed = self.total_entries.checked_add(widths.len()).ok_or(
            TerminalCheckpointError::ArithmeticOverflow("custom_cell_widths"),
        )?;
        ensure_limit(
            "custom_cell_widths",
            observed,
            limits.max_total_custom_cell_widths,
        )?;
        for (codepoint, width) in widths {
            if char::from_u32(*codepoint).is_none() || !(1..=2).contains(width) {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "unicode_version.custom_cell_widths",
                    reason: "live custom widths must use scalar keys and one or two columns",
                });
            }
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(widths.len())
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("custom_cell_width_maps"))?;
        entries.extend(widths.iter().map(|(codepoint, width)| {
            CheckpointCustomCellWidth {
                codepoint: *codepoint,
                width: *width,
            }
        }));
        entries.sort_unstable_by_key(|entry| entry.codepoint);
        self.tables.push(CheckpointCustomCellWidthMap { entries });
        self.total_entries = observed;
        Ok(())
    }

    fn finish(mut self) -> Vec<CheckpointCustomCellWidthMap> {
        self.tables.sort_unstable();
        self.tables
    }
}

fn validate_custom_cell_width_maps(
    tables: &[CheckpointCustomCellWidthMap],
    limits: TerminalCheckpointLimits,
    usage: &mut crate::screen::ScreenCheckpointUsage,
) -> Result<(), TerminalCheckpointError> {
    if tables.len() > 2 {
        return Err(TerminalCheckpointError::InvalidField {
            field: "custom_cell_width_maps",
            reason: "reachable terminal state can reference at most two custom-width maps",
        });
    }
    if tables.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(TerminalCheckpointError::InvalidField {
            field: "custom_cell_width_maps",
            reason: "custom-width maps must be strictly lexicographically ordered",
        });
    }
    let table_bytes = tables
        .len()
        .checked_mul(std::mem::size_of::<CheckpointCustomCellWidthMap>())
        .ok_or(TerminalCheckpointError::ArithmeticOverflow(
            "retained_capture_bytes",
        ))?;
    checked_accumulate(
        &mut usage.retained_capture_bytes,
        table_bytes,
        limits.max_retained_capture_bytes,
        "retained_capture_bytes",
    )?;
    let mut custom_width_count = 0usize;
    for table in tables {
        table.validate(limits, &mut custom_width_count, usage)?;
    }
    Ok(())
}

fn decode_custom_cell_width_maps(
    tables: &[CheckpointCustomCellWidthMap],
) -> Result<Vec<Arc<HashMap<u32, u8>>>, TerminalCheckpointError> {
    let mut decoded = Vec::new();
    decoded
        .try_reserve_exact(tables.len())
        .map_err(|_| TerminalCheckpointError::ResourceAllocation("custom_cell_width_maps"))?;
    for table in tables {
        decoded.push(table.decode()?);
    }
    Ok(decoded)
}

fn validate_live_custom_cell_width_maps(
    tables: &[Arc<HashMap<u32, u8>>],
    limits: TerminalCheckpointLimits,
    usage: &mut crate::screen::ScreenCheckpointUsage,
) -> Result<(), TerminalCheckpointError> {
    if tables.len() > 2 {
        return Err(TerminalCheckpointError::InvalidField {
            field: "custom_cell_width_maps",
            reason: "reachable terminal state can reference at most two custom-width maps",
        });
    }
    let table_bytes = tables
        .len()
        .checked_mul(std::mem::size_of::<Arc<HashMap<u32, u8>>>())
        .ok_or(TerminalCheckpointError::ArithmeticOverflow(
            "retained_capture_bytes",
        ))?;
    checked_accumulate(
        &mut usage.retained_capture_bytes,
        table_bytes,
        limits.max_retained_capture_bytes,
        "retained_capture_bytes",
    )?;
    let mut custom_width_count = 0usize;
    for widths in tables {
        if widths.is_empty() {
            return Err(TerminalCheckpointError::InvalidField {
                field: "custom_cell_width_maps",
                reason: "empty custom-width maps must be represented by no reference",
            });
        }
        ensure_limit(
            "custom_cell_widths_per_map",
            widths.len(),
            frankenterm_cell::MAX_CUSTOM_CELL_WIDTH_EXPANSION,
        )?;
        checked_accumulate(
            &mut custom_width_count,
            widths.len(),
            limits.max_total_custom_cell_widths,
            "custom_cell_widths",
        )?;
        let retained_bytes = widths.len().checked_mul(48).ok_or(
            TerminalCheckpointError::ArithmeticOverflow("retained_capture_bytes"),
        )?;
        checked_accumulate(
            &mut usage.retained_capture_bytes,
            retained_bytes,
            limits.max_retained_capture_bytes,
            "retained_capture_bytes",
        )?;
        for (codepoint, width) in widths.iter() {
            if char::from_u32(*codepoint).is_none() {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "unicode_version.custom_cell_widths",
                    reason: "custom-width keys must be Unicode scalar values",
                });
            }
            if !(1..=2).contains(width) {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "unicode_version.custom_cell_widths",
                    reason: "custom widths must be one or two columns",
                });
            }
        }
    }
    Ok(())
}

fn live_custom_cell_width_map_index(
    value: &UnicodeVersion,
    tables: &[Arc<HashMap<u32, u8>>],
) -> Result<Option<usize>, TerminalCheckpointError> {
    let Some(widths) = CheckpointCustomCellWidthTableBuilder::live_widths(value) else {
        return Ok(None);
    };
    tables
        .iter()
        .position(|table| table.as_ref() == widths)
        .map(Some)
        .ok_or(TerminalCheckpointError::InvalidField {
            field: "unicode_version.custom_cell_widths",
            reason: "live custom-width map is absent from the admitted checkpoint table",
        })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointUnicodeVersion {
    version: u8,
    ambiguous_are_wide: bool,
    custom_cell_width_map: Option<u32>,
}

impl CheckpointUnicodeVersion {
    fn capture(
        value: &UnicodeVersion,
        tables: &[CheckpointCustomCellWidthMap],
    ) -> Result<Self, TerminalCheckpointError> {
        let custom_cell_width_map =
            CheckpointCustomCellWidthTableBuilder::live_widths(value)
                .map(|widths| {
                    let index = tables
                        .iter()
                        .position(|table| table.matches_live(widths))
                        .ok_or(TerminalCheckpointError::InvalidField {
                            field: "custom_cell_width_maps",
                            reason: "capture table is missing a registered live width map",
                        })?;
                    u32::try_from(index).map_err(|_| TerminalCheckpointError::InvalidField {
                        field: "custom_cell_width_maps",
                        reason: "custom-width table index does not fit the wire type",
                    })
                })
                .transpose()?;
        Ok(Self {
            version: value.version,
            ambiguous_are_wide: value.ambiguous_are_wide,
            custom_cell_width_map,
        })
    }

    fn into_live(
        self,
        tables: &[Arc<HashMap<u32, u8>>],
    ) -> Result<UnicodeVersion, TerminalCheckpointError> {
        let cell_widths = self
            .custom_cell_width_map
            .map(|index| {
                tables
                    .get(usize::try_from(index).map_err(|_| {
                        TerminalCheckpointError::InvalidField {
                            field: "unicode_version.custom_cell_width_map",
                            reason: "custom-width table index does not fit this architecture",
                        }
                    })?)
                    .cloned()
                    .ok_or(TerminalCheckpointError::InvalidField {
                        field: "unicode_version.custom_cell_width_map",
                        reason: "custom-width table reference is out of bounds",
                    })
            })
            .transpose()?;
        Ok(UnicodeVersion {
            version: self.version,
            ambiguous_are_wide: self.ambiguous_are_wide,
            cell_widths,
        })
    }

    fn validate(
        &self,
        table_count: usize,
    ) -> Result<(), TerminalCheckpointError> {
        self.referenced_index(table_count)?;
        Ok(())
    }

    fn referenced_index(
        &self,
        table_count: usize,
    ) -> Result<Option<usize>, TerminalCheckpointError> {
        self.custom_cell_width_map
            .map(|index| {
                let index = usize::try_from(index).map_err(|_| {
                    TerminalCheckpointError::InvalidField {
                        field: "unicode_version.custom_cell_width_map",
                        reason: "custom-width table index does not fit this architecture",
                    }
                })?;
                if index >= table_count {
                    return Err(TerminalCheckpointError::InvalidField {
                        field: "unicode_version.custom_cell_width_map",
                        reason: "custom-width table reference is out of bounds",
                    });
                }
                Ok(index)
            })
            .transpose()
    }
}

const CHECKPOINT_REPLAY_CONFIG_VERSION: u32 = 2;

/// Immutable, capability-free values that can change the terminal model while
/// authenticated raw output is replayed. Operational tier placement, spill
/// capabilities, replies, clipboard access, and input encoding are excluded.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointReplayConfigV2 {
    version: u32,
    scrollback_size: u64,
    color_palette: CheckpointColorPalette,
    enable_kitty_keyboard: bool,
    max_user_vars: u64,
    max_unicode_version_stack_depth: u64,
    max_accumulating_title_len: u64,
    unicode_version: CheckpointUnicodeVersion,
    normalize_output_to_unicode_nfc: bool,
    bidi_enabled: bool,
    bidi_hint: CheckpointBidiHint,
}

struct PendingReplayConfigV2 {
    version: u32,
    scrollback_size: u64,
    color_palette: CheckpointColorPalette,
    enable_kitty_keyboard: bool,
    max_user_vars: u64,
    max_unicode_version_stack_depth: u64,
    max_accumulating_title_len: u64,
    unicode_version: UnicodeVersion,
    normalize_output_to_unicode_nfc: bool,
    bidi_enabled: bool,
    bidi_hint: CheckpointBidiHint,
}

impl PendingReplayConfigV2 {
    fn capture(
        config: &dyn TerminalConfiguration,
        limits: TerminalCheckpointLimits,
    ) -> Result<Self, TerminalCheckpointError> {
        limits.validate_policy()?;
        let revision_before = config.revision();
        let scrollback_size = config.scrollback_size();
        let enable_kitty_keyboard = config.enable_kitty_keyboard();
        let max_user_vars = config.max_user_vars();
        let max_unicode_version_stack_depth = config.max_unicode_version_stack_depth();
        let max_accumulating_title_len = config.max_accumulating_title_len();
        let normalize_output_to_unicode_nfc = config.normalize_output_to_unicode_nfc();
        let bidi = config.bidi_mode();
        let palette = config.color_palette();
        let unicode_version = config.unicode_version();
        let custom_widths = unicode_version
            .cell_widths
            .as_ref()
            .map_or(0, |widths| widths.len());
        ensure_limit(
            "config.unicode_version.custom_cell_widths_per_map",
            custom_widths,
            frankenterm_cell::MAX_CUSTOM_CELL_WIDTH_EXPANSION,
        )?;
        ensure_limit(
            "config.unicode_version.custom_cell_widths",
            custom_widths,
            limits.max_total_custom_cell_widths,
        )?;
        ensure_limit(
            "config.scrollback_size",
            scrollback_size,
            limits.max_total_lines,
        )?;
        ensure_limit(
            "config.max_user_vars",
            max_user_vars,
            limits.max_user_vars,
        )?;
        ensure_limit(
            "config.max_unicode_version_stack_depth",
            max_unicode_version_stack_depth,
            limits.max_unicode_stack_depth,
        )?;
        ensure_limit(
            "config.max_accumulating_title_len",
            max_accumulating_title_len,
            limits.max_string_bytes.min(limits.max_terminal_string_bytes),
        )?;
        let projection = Self {
            version: CHECKPOINT_REPLAY_CONFIG_VERSION,
            scrollback_size: u64_from_usize(scrollback_size, "config.scrollback_size")?,
            color_palette: CheckpointColorPalette::capture(&palette)?,
            enable_kitty_keyboard,
            max_user_vars: u64_from_usize(max_user_vars, "config.max_user_vars")?,
            max_unicode_version_stack_depth: u64_from_usize(
                max_unicode_version_stack_depth,
                "config.max_unicode_version_stack_depth",
            )?,
            max_accumulating_title_len: u64_from_usize(
                max_accumulating_title_len,
                "config.max_accumulating_title_len",
            )?,
            unicode_version,
            normalize_output_to_unicode_nfc,
            bidi_enabled: bidi.enabled,
            bidi_hint: bidi.hint.into(),
        };
        if config.revision() != revision_before {
            return Err(TerminalCheckpointError::ConfigurationChangedDuringProjection);
        }
        Ok(projection)
    }

    fn into_checkpoint(
        self,
        tables: &[CheckpointCustomCellWidthMap],
    ) -> Result<CheckpointReplayConfigV2, TerminalCheckpointError> {
        Ok(CheckpointReplayConfigV2 {
            version: self.version,
            scrollback_size: self.scrollback_size,
            color_palette: self.color_palette,
            enable_kitty_keyboard: self.enable_kitty_keyboard,
            max_user_vars: self.max_user_vars,
            max_unicode_version_stack_depth: self.max_unicode_version_stack_depth,
            max_accumulating_title_len: self.max_accumulating_title_len,
            unicode_version: CheckpointUnicodeVersion::capture(&self.unicode_version, tables)?,
            normalize_output_to_unicode_nfc: self.normalize_output_to_unicode_nfc,
            bidi_enabled: self.bidi_enabled,
            bidi_hint: self.bidi_hint,
        })
    }

    fn matches_checkpoint(
        &self,
        checkpoint: &CheckpointReplayConfigV2,
        tables: &[Arc<HashMap<u32, u8>>],
    ) -> Result<bool, TerminalCheckpointError> {
        let expected_unicode = checkpoint.unicode_version.clone().into_live(tables)?;
        Ok(checkpoint.version == self.version
            && checkpoint.scrollback_size == self.scrollback_size
            && checkpoint.color_palette == self.color_palette
            && checkpoint.enable_kitty_keyboard == self.enable_kitty_keyboard
            && checkpoint.max_user_vars == self.max_user_vars
            && checkpoint.max_unicode_version_stack_depth
                == self.max_unicode_version_stack_depth
            && checkpoint.max_accumulating_title_len == self.max_accumulating_title_len
            && expected_unicode == self.unicode_version
            && checkpoint.normalize_output_to_unicode_nfc == self.normalize_output_to_unicode_nfc
            && checkpoint.bidi_enabled == self.bidi_enabled
            && checkpoint.bidi_hint == self.bidi_hint)
    }
}

impl CheckpointReplayConfigV2 {
    fn validate(
        &self,
        limits: TerminalCheckpointLimits,
        table_count: usize,
        usage: &mut crate::screen::ScreenCheckpointUsage,
    ) -> Result<(), TerminalCheckpointError> {
        if self.version != CHECKPOINT_REPLAY_CONFIG_VERSION {
            return Err(TerminalCheckpointError::InvalidField {
                field: "replay_config.version",
                reason: "unsupported replay configuration version",
            });
        }
        self.color_palette.validate()?;
        let palette_bytes = self
            .color_palette
            .colors
            .len()
            .checked_mul(std::mem::size_of::<CheckpointSrgba>())
            .ok_or(TerminalCheckpointError::ArithmeticOverflow(
                "retained_capture_bytes",
            ))?;
        checked_accumulate(
            &mut usage.retained_capture_bytes,
            palette_bytes,
            limits.max_retained_capture_bytes,
            "retained_capture_bytes",
        )?;
        self.unicode_version.validate(table_count)?;

        let scrollback_size = usize_from_u64(self.scrollback_size, "config.scrollback_size")?;
        ensure_limit(
            "config.scrollback_size",
            scrollback_size,
            limits.max_total_lines,
        )?;
        let max_user_vars = usize_from_u64(self.max_user_vars, "config.max_user_vars")?;
        ensure_limit("config.max_user_vars", max_user_vars, limits.max_user_vars)?;
        let max_unicode_stack = usize_from_u64(
            self.max_unicode_version_stack_depth,
            "config.max_unicode_version_stack_depth",
        )?;
        ensure_limit(
            "config.max_unicode_version_stack_depth",
            max_unicode_stack,
            limits.max_unicode_stack_depth,
        )?;
        let max_title = usize_from_u64(
            self.max_accumulating_title_len,
            "config.max_accumulating_title_len",
        )?;
        ensure_limit(
            "config.max_accumulating_title_len",
            max_title,
            limits.max_string_bytes.min(limits.max_terminal_string_bytes),
        )
    }

    fn to_replay_configuration(
        &self,
        limits: TerminalCheckpointLimits,
        tables: &[Arc<HashMap<u32, u8>>],
    ) -> Result<ReplayTerminalConfiguration, TerminalCheckpointError> {
        Ok(ReplayTerminalConfiguration {
            scrollback_size: usize_from_u64(
                self.scrollback_size,
                "config.scrollback_size",
            )?,
            color_palette: self.color_palette.clone().into_live()?,
            enable_kitty_keyboard: self.enable_kitty_keyboard,
            max_user_vars: usize_from_u64(self.max_user_vars, "config.max_user_vars")?,
            max_unicode_version_stack_depth: usize_from_u64(
                self.max_unicode_version_stack_depth,
                "config.max_unicode_version_stack_depth",
            )?,
            max_accumulating_title_len: usize_from_u64(
                self.max_accumulating_title_len,
                "config.max_accumulating_title_len",
            )?,
            max_color_map_entries: limits.max_color_registers,
            unicode_version: self.unicode_version.clone().into_live(tables)?,
            normalize_output_to_unicode_nfc: self.normalize_output_to_unicode_nfc,
            bidi_mode: BidiMode {
                enabled: self.bidi_enabled,
                hint: self.bidi_hint.into_live(),
            },
        })
    }

    pub(crate) fn matches_stable(
        &self,
        config: &dyn TerminalConfiguration,
        limits: TerminalCheckpointLimits,
        tables: &[Arc<HashMap<u32, u8>>],
    ) -> Result<bool, TerminalCheckpointError> {
        PendingReplayConfigV2::capture(config, limits)?.matches_checkpoint(self, tables)
    }
}

struct ReplayTerminalConfiguration {
    scrollback_size: usize,
    color_palette: ColorPalette,
    enable_kitty_keyboard: bool,
    max_user_vars: usize,
    max_unicode_version_stack_depth: usize,
    max_accumulating_title_len: usize,
    max_color_map_entries: usize,
    unicode_version: UnicodeVersion,
    normalize_output_to_unicode_nfc: bool,
    bidi_mode: BidiMode,
}

impl std::fmt::Debug for ReplayTerminalConfiguration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReplayTerminalConfiguration")
            .field("scrollback_size", &self.scrollback_size)
            .finish_non_exhaustive()
    }
}

impl TerminalConfiguration for ReplayTerminalConfiguration {
    fn scrollback_size(&self) -> usize {
        self.scrollback_size
    }

    fn color_palette(&self) -> ColorPalette {
        self.color_palette.clone()
    }

    fn enable_kitty_keyboard(&self) -> bool {
        self.enable_kitty_keyboard
    }

    fn max_user_vars(&self) -> usize {
        self.max_user_vars
    }

    fn max_unicode_version_stack_depth(&self) -> usize {
        self.max_unicode_version_stack_depth
    }

    fn max_accumulating_title_len(&self) -> usize {
        self.max_accumulating_title_len
    }

    fn max_color_map_entries(&self) -> usize {
        self.max_color_map_entries
    }

    fn unicode_version(&self) -> UnicodeVersion {
        self.unicode_version.clone()
    }

    fn normalize_output_to_unicode_nfc(&self) -> bool {
        self.normalize_output_to_unicode_nfc
    }

    fn bidi_mode(&self) -> BidiMode {
        self.bidi_mode
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointSavedCursor {
    position: CheckpointCursorPosition,
    wrap_next: bool,
    pen: CheckpointCellAttributes,
    dec_origin_mode: bool,
    g0_charset: CheckpointCharSet,
    g1_charset: CheckpointCharSet,
}

impl CheckpointSavedCursor {
    fn capture(value: &SavedCursor) -> Result<Self, TerminalCheckpointError> {
        Ok(Self {
            position: CheckpointCursorPosition::capture(value.position)?,
            wrap_next: value.wrap_next,
            pen: CheckpointCellAttributes::capture(&value.pen)?,
            dec_origin_mode: value.dec_origin_mode,
            g0_charset: value.g0_charset.into(),
            g1_charset: value.g1_charset.into(),
        })
    }

    fn into_live(self) -> Result<SavedCursor, TerminalCheckpointError> {
        Ok(SavedCursor {
            position: self.position.into_live()?,
            wrap_next: self.wrap_next,
            pen: self.pen.into_live()?,
            dec_origin_mode: self.dec_origin_mode,
            g0_charset: self.g0_charset.into_live(),
            g1_charset: self.g1_charset.into_live(),
        })
    }


    fn validate(
        &self,
        terminal_seqno: u64,
        limits: TerminalCheckpointLimits,
        usage: &mut crate::screen::ScreenCheckpointUsage,
    ) -> Result<(), TerminalCheckpointError> {
        self.position
            .validate_retained(terminal_seqno, "screen.saved_cursor")?;
        self.pen.validate(limits, usage)
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointScrollbackGeneration {
    content_epoch: [u8; 16],
    revision: u64,
}

impl std::fmt::Debug for CheckpointScrollbackGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CheckpointScrollbackGeneration")
            .field("content_epoch", &"[REDACTED]")
            .field("revision", &self.revision)
            .finish()
    }
}

impl From<crate::config::ScrollbackSnapshotGeneration> for CheckpointScrollbackGeneration {
    fn from(generation: crate::config::ScrollbackSnapshotGeneration) -> Self {
        Self {
            content_epoch: generation.content_epoch(),
            revision: generation.revision(),
        }
    }
}

impl CheckpointScrollbackGeneration {
    fn into_live(self) -> crate::config::ScrollbackSnapshotGeneration {
        crate::config::ScrollbackSnapshotGeneration::new(self.content_epoch, self.revision)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointScreen {
    lines: Vec<CheckpointLine>,
    stable_row_index_offset: u64,
    cold_snapshot_generation: Option<CheckpointScrollbackGeneration>,
    cold_prefix_line_count: u64,
    allow_scrollback: bool,
    keyboard_stack: Vec<CheckpointKeyboardEncoding>,
    physical_rows: u32,
    physical_cols: u32,
    dpi: u32,
    saved_cursor: Option<CheckpointSavedCursor>,
}

impl CheckpointScreen {
    fn capture(
        value: crate::screen::ScreenCheckpointParts,
    ) -> Result<Self, TerminalCheckpointError> {
        let mut lines = Vec::new();
        lines
            .try_reserve_exact(value.lines.len())
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("screen.lines"))?;
        for line in &value.lines {
            lines.push(CheckpointLine::capture(line)?);
        }
        let mut keyboard_stack = Vec::new();
        keyboard_stack
            .try_reserve_exact(value.keyboard_stack.len())
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("screen.keyboard_stack"))?;
        for encoding in value.keyboard_stack {
            keyboard_stack.push(CheckpointKeyboardEncoding::from(encoding));
        }
        Ok(Self {
            lines,
            stable_row_index_offset: u64_from_usize(
                value.stable_row_index_offset,
                "screen.stable_row_index_offset",
            )?,
            cold_snapshot_generation: value.cold_snapshot_generation.map(Into::into),
            cold_prefix_line_count: u64_from_usize(
                value.cold_prefix_line_count,
                "screen.cold_prefix_line_count",
            )?,
            allow_scrollback: value.allow_scrollback,
            keyboard_stack,
            physical_rows: u32::try_from(value.physical_rows).map_err(|_| {
                TerminalCheckpointError::InvalidField {
                    field: "screen.physical_rows",
                    reason: "value does not fit the checkpoint wire type",
                }
            })?,
            physical_cols: u32::try_from(value.physical_cols).map_err(|_| {
                TerminalCheckpointError::InvalidField {
                    field: "screen.physical_cols",
                    reason: "value does not fit the checkpoint wire type",
                }
            })?,
            dpi: value.dpi,
            saved_cursor: value
                .saved_cursor
                .as_ref()
                .map(CheckpointSavedCursor::capture)
                .transpose()?,
        })
    }

    fn into_live(self) -> Result<crate::screen::ScreenCheckpointParts, TerminalCheckpointError> {
        let mut lines = Vec::new();
        lines
            .try_reserve_exact(self.lines.len())
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("screen.lines"))?;
        for line in self.lines {
            lines.push(line.into_live()?);
        }
        let mut keyboard_stack = Vec::new();
        keyboard_stack
            .try_reserve_exact(self.keyboard_stack.len())
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("screen.keyboard_stack"))?;
        for encoding in self.keyboard_stack {
            keyboard_stack.push(encoding.into_live()?);
        }
        Ok(crate::screen::ScreenCheckpointParts {
            lines,
            stable_row_index_offset: usize_from_u64(
                self.stable_row_index_offset,
                "screen.stable_row_index_offset",
            )?,
            cold_snapshot_generation: self
                .cold_snapshot_generation
                .map(CheckpointScrollbackGeneration::into_live),
            cold_prefix_line_count: usize_from_u64(
                self.cold_prefix_line_count,
                "screen.cold_prefix_line_count",
            )?,
            allow_scrollback: self.allow_scrollback,
            keyboard_stack,
            physical_rows: usize::try_from(self.physical_rows).map_err(|_| {
                TerminalCheckpointError::InvalidField {
                    field: "screen.physical_rows",
                    reason: "value does not fit this target architecture",
                }
            })?,
            physical_cols: usize::try_from(self.physical_cols).map_err(|_| {
                TerminalCheckpointError::InvalidField {
                    field: "screen.physical_cols",
                    reason: "value does not fit this target architecture",
                }
            })?,
            dpi: self.dpi,
            saved_cursor: self
                .saved_cursor
                .map(CheckpointSavedCursor::into_live)
                .transpose()?,
        })
    }

    fn validate(
        &self,
        expected_scrollback: bool,
        limits: TerminalCheckpointLimits,
        usage: &mut crate::screen::ScreenCheckpointUsage,
        terminal_seqno: u64,
    ) -> Result<(), TerminalCheckpointError> {
        if self.allow_scrollback != expected_scrollback {
            return Err(TerminalCheckpointError::InvalidField {
                field: "screen.allow_scrollback",
                reason: "primary and alternate screen roles are not canonical",
            });
        }
        let rows = usize::try_from(self.physical_rows).map_err(|_| {
            TerminalCheckpointError::InvalidField {
                field: "screen.physical_rows",
                reason: "value does not fit this target architecture",
            }
        })?;
        let cols = usize::try_from(self.physical_cols).map_err(|_| {
            TerminalCheckpointError::InvalidField {
                field: "screen.physical_cols",
                reason: "value does not fit this target architecture",
            }
        })?;
        if rows == 0 || rows > limits.max_rows {
            return Err(resource_limit("physical_rows", rows, limits.max_rows));
        }
        if cols == 0 || cols > limits.max_cols {
            return Err(resource_limit("physical_cols", cols, limits.max_cols));
        }
        let visible_grid_cells = rows.checked_mul(cols).ok_or(
            TerminalCheckpointError::ArithmeticOverflow("visible_grid_cells"),
        )?;
        ensure_limit(
            "visible_grid_cells",
            visible_grid_cells,
            limits.max_visible_grid_cells,
        )?;
        ensure_limit(
            "keyboard_stack_depth",
            self.keyboard_stack.len(),
            limits.max_keyboard_stack_depth,
        )?;
        for encoding in &self.keyboard_stack {
            match encoding {
                CheckpointKeyboardEncoding::Kitty(bits) => {
                    KittyKeyboardFlags::from_bits(*bits).ok_or(
                        TerminalCheckpointError::InvalidField {
                            field: "screen.keyboard_stack",
                            reason: "unknown Kitty keyboard flag bits",
                        },
                    )?;
                }
                _ => {
                    return Err(TerminalCheckpointError::InvalidField {
                        field: "screen.keyboard_stack",
                        reason: "screen keyboard stacks may contain only Kitty states",
                    });
                }
            }
        }
        if self.lines.len() < rows {
            return Err(TerminalCheckpointError::InvalidField {
                field: "screen.lines",
                reason: "screen has fewer lines than its visible height",
            });
        }
        let cold_prefix_line_count = usize_from_u64(
            self.cold_prefix_line_count,
            "screen.cold_prefix_line_count",
        )?;
        if cold_prefix_line_count > self.lines.len().saturating_sub(rows) {
            return Err(TerminalCheckpointError::InvalidField {
                field: "screen.cold_prefix_line_count",
                reason: "cold prefix must be wholly contained in scrollback",
            });
        }
        if cold_prefix_line_count != 0 && self.cold_snapshot_generation.is_none() {
            return Err(TerminalCheckpointError::InvalidField {
                field: "screen.cold_snapshot_generation",
                reason: "a nonempty cold prefix requires its exact source generation",
            });
        }
        if !expected_scrollback {
            if self.stable_row_index_offset != 0
                || self.cold_prefix_line_count != 0
                || self.cold_snapshot_generation.is_some()
            {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "alternate_screen",
                    reason: "alternate screen cannot retain scrollback metadata",
                });
            }
            if self.lines.len() != rows {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "alternate_screen.lines",
                    reason: "alternate screen line count must equal its visible height",
                });
            }
        }
        let line_count = u64::try_from(self.lines.len()).map_err(|_| {
            TerminalCheckpointError::InvalidField {
                field: "screen.lines",
                reason: "line count does not fit the checkpoint wire type",
            }
        })?;
        let stable_end = self
            .stable_row_index_offset
            .checked_add(line_count)
            .ok_or(TerminalCheckpointError::ArithmeticOverflow(
                "screen.stable_row_range",
            ))?;
        let stable_max = u64::try_from(StableRowIndex::MAX).unwrap_or(u64::MAX);
        if stable_end > stable_max {
            return Err(TerminalCheckpointError::InvalidField {
                field: "screen.stable_row_index_offset",
                reason: "stable row range exceeds the runtime index type",
            });
        }
        for line in &self.lines {
            line.validate(limits, usage, terminal_seqno)?;
        }
        if let Some(saved) = self.saved_cursor.as_ref() {
            saved.validate(terminal_seqno, limits, usage)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointUnicodeVersionStackEntry {
    version: CheckpointUnicodeVersion,
    label: Option<String>,
}

/// Hard resource envelope for semantic checkpoint capture, decoding, and
/// restoration.  Callers may choose a smaller policy, but every field remains
/// bounded and checked with overflow detection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalCheckpointLimits {
    pub max_encoded_bytes: usize,
    pub max_retained_capture_bytes: usize,
    pub max_replay_record_bytes: usize,
    pub max_replay_total_bytes: usize,
    pub max_replay_records: usize,
    pub max_replay_actions_per_record: usize,
    pub max_rows: usize,
    pub max_cols: usize,
    pub max_visible_grid_cells: usize,
    pub max_total_lines: usize,
    pub max_total_cell_records: usize,
    pub max_total_cells: usize,
    pub max_total_cell_text_bytes: usize,
    pub max_total_hyperlink_bytes: usize,
    pub max_total_hyperlink_params: usize,
    pub max_string_bytes: usize,
    pub max_hyperlink_params_per_link: usize,
    pub max_cold_scrollback_bytes: usize,
    pub max_keyboard_stack_depth: usize,
    pub max_saved_dec_private_modes: usize,
    pub max_color_registers: usize,
    pub max_mouse_buttons: usize,
    pub max_tab_stops: usize,
    pub max_user_vars: usize,
    pub max_unicode_stack_depth: usize,
    pub max_total_custom_cell_widths: usize,
    pub max_terminal_string_bytes: usize,
    pub max_pixel_dimension: usize,
    pub max_dpi: u32,
}

impl Default for TerminalCheckpointLimits {
    fn default() -> Self {
        Self {
            max_encoded_bytes: 128 * 1024 * 1024,
            max_retained_capture_bytes: 512 * 1024 * 1024,
            max_replay_record_bytes: 1024 * 1024,
            max_replay_total_bytes: 1024 * 1024 * 1024,
            max_replay_records: 1_000_000,
            max_replay_actions_per_record: 1024 * 1024,
            max_rows: 4_096,
            max_cols: 16_384,
            max_visible_grid_cells: 8 * 1024 * 1024,
            max_total_lines: 131_072,
            max_total_cell_records: 2 * 1024 * 1024,
            max_total_cells: 2 * 1024 * 1024,
            max_total_cell_text_bytes: 64 * 1024 * 1024,
            max_total_hyperlink_bytes: 16 * 1024 * 1024,
            max_total_hyperlink_params: 256 * 1024,
            max_string_bytes: 1024 * 1024,
            max_hyperlink_params_per_link: 256,
            max_cold_scrollback_bytes: 64 * 1024 * 1024,
            max_keyboard_stack_depth: 128,
            max_saved_dec_private_modes: 256,
            max_color_registers: 4_096,
            max_mouse_buttons: 3,
            max_tab_stops: 32_768,
            max_user_vars: 512,
            max_unicode_stack_depth: 64,
            max_total_custom_cell_widths: 2
                * frankenterm_cell::MAX_CUSTOM_CELL_WIDTH_EXPANSION,
            max_terminal_string_bytes: 8 * 1024 * 1024,
            max_pixel_dimension: 1_048_576,
            max_dpi: 1_000_000,
        }
    }
}

impl TerminalCheckpointLimits {
    const ABSOLUTE_MAX_ENCODED_BYTES: usize = 256 * 1024 * 1024;
    const ABSOLUTE_MAX_RETAINED_CAPTURE_BYTES: usize = 512 * 1024 * 1024;
    const ABSOLUTE_MAX_REPLAY_RECORD_BYTES: usize = 16 * 1024 * 1024;
    const ABSOLUTE_MAX_REPLAY_TOTAL_BYTES: usize = 2 * 1024 * 1024 * 1024;
    const ABSOLUTE_MAX_REPLAY_RECORDS: usize = 4_000_000;
    const ABSOLUTE_MAX_REPLAY_ACTIONS_PER_RECORD: usize = 16 * 1024 * 1024;
    const ABSOLUTE_MAX_ROWS: usize = 4_096;
    const ABSOLUTE_MAX_COLS: usize = 16_384;
    const ABSOLUTE_MAX_VISIBLE_GRID_CELLS: usize = 16 * 1024 * 1024;
    const ABSOLUTE_MAX_TOTAL_LINES: usize = 262_144;
    const ABSOLUTE_MAX_TOTAL_CELL_RECORDS: usize = 4 * 1024 * 1024;
    const ABSOLUTE_MAX_TOTAL_CELLS: usize = 4 * 1024 * 1024;
    const ABSOLUTE_MAX_TOTAL_CELL_TEXT_BYTES: usize = 128 * 1024 * 1024;
    const ABSOLUTE_MAX_TOTAL_HYPERLINK_BYTES: usize = 32 * 1024 * 1024;
    const ABSOLUTE_MAX_TOTAL_HYPERLINK_PARAMS: usize = 1024 * 1024;
    const ABSOLUTE_MAX_STRING_BYTES: usize = 8 * 1024 * 1024;
    const ABSOLUTE_MAX_HYPERLINK_PARAMS_PER_LINK: usize = 1024;
    const ABSOLUTE_MAX_COLD_SCROLLBACK_BYTES: usize = 128 * 1024 * 1024;
    const ABSOLUTE_MAX_KEYBOARD_STACK_DEPTH: usize = 128;
    const ABSOLUTE_MAX_SAVED_DEC_PRIVATE_MODES: usize = 256;
    const ABSOLUTE_MAX_COLOR_REGISTERS: usize = 4_096;
    const ABSOLUTE_MAX_MOUSE_BUTTONS: usize = 3;
    const ABSOLUTE_MAX_TAB_STOPS: usize = 65_536;
    const ABSOLUTE_MAX_USER_VARS: usize = 512;
    const ABSOLUTE_MAX_UNICODE_STACK_DEPTH: usize = 64;
    const ABSOLUTE_MAX_TOTAL_CUSTOM_CELL_WIDTHS: usize =
        2 * frankenterm_cell::MAX_CUSTOM_CELL_WIDTH_EXPANSION;
    const ABSOLUTE_MAX_TERMINAL_STRING_BYTES: usize = 32 * 1024 * 1024;
    const ABSOLUTE_MAX_PIXEL_DIMENSION: usize = 1_048_576;
    const ABSOLUTE_MAX_DPI: u32 = 1_000_000;

    fn validate_policy(self) -> Result<(), TerminalCheckpointError> {
        for (resource, observed, maximum) in [
            (
                "configured_max_encoded_bytes",
                self.max_encoded_bytes,
                Self::ABSOLUTE_MAX_ENCODED_BYTES,
            ),
            (
                "configured_max_retained_capture_bytes",
                self.max_retained_capture_bytes,
                Self::ABSOLUTE_MAX_RETAINED_CAPTURE_BYTES,
            ),
            (
                "configured_max_replay_record_bytes",
                self.max_replay_record_bytes,
                Self::ABSOLUTE_MAX_REPLAY_RECORD_BYTES,
            ),
            (
                "configured_max_replay_total_bytes",
                self.max_replay_total_bytes,
                Self::ABSOLUTE_MAX_REPLAY_TOTAL_BYTES,
            ),
            (
                "configured_max_replay_records",
                self.max_replay_records,
                Self::ABSOLUTE_MAX_REPLAY_RECORDS,
            ),
            (
                "configured_max_replay_actions_per_record",
                self.max_replay_actions_per_record,
                Self::ABSOLUTE_MAX_REPLAY_ACTIONS_PER_RECORD,
            ),
            ("configured_max_rows", self.max_rows, Self::ABSOLUTE_MAX_ROWS),
            ("configured_max_cols", self.max_cols, Self::ABSOLUTE_MAX_COLS),
            (
                "configured_max_visible_grid_cells",
                self.max_visible_grid_cells,
                Self::ABSOLUTE_MAX_VISIBLE_GRID_CELLS,
            ),
            (
                "configured_max_total_lines",
                self.max_total_lines,
                Self::ABSOLUTE_MAX_TOTAL_LINES,
            ),
            (
                "configured_max_total_cells",
                self.max_total_cells,
                Self::ABSOLUTE_MAX_TOTAL_CELLS,
            ),
            (
                "configured_max_total_cell_records",
                self.max_total_cell_records,
                Self::ABSOLUTE_MAX_TOTAL_CELL_RECORDS,
            ),
            (
                "configured_max_total_cell_text_bytes",
                self.max_total_cell_text_bytes,
                Self::ABSOLUTE_MAX_TOTAL_CELL_TEXT_BYTES,
            ),
            (
                "configured_max_total_hyperlink_bytes",
                self.max_total_hyperlink_bytes,
                Self::ABSOLUTE_MAX_TOTAL_HYPERLINK_BYTES,
            ),
            (
                "configured_max_total_hyperlink_params",
                self.max_total_hyperlink_params,
                Self::ABSOLUTE_MAX_TOTAL_HYPERLINK_PARAMS,
            ),
            (
                "configured_max_string_bytes",
                self.max_string_bytes,
                Self::ABSOLUTE_MAX_STRING_BYTES,
            ),
            (
                "configured_max_hyperlink_params_per_link",
                self.max_hyperlink_params_per_link,
                Self::ABSOLUTE_MAX_HYPERLINK_PARAMS_PER_LINK,
            ),
            (
                "configured_max_cold_scrollback_bytes",
                self.max_cold_scrollback_bytes,
                Self::ABSOLUTE_MAX_COLD_SCROLLBACK_BYTES,
            ),
            (
                "configured_max_keyboard_stack_depth",
                self.max_keyboard_stack_depth,
                Self::ABSOLUTE_MAX_KEYBOARD_STACK_DEPTH,
            ),
            (
                "configured_max_saved_dec_private_modes",
                self.max_saved_dec_private_modes,
                Self::ABSOLUTE_MAX_SAVED_DEC_PRIVATE_MODES,
            ),
            (
                "configured_max_color_registers",
                self.max_color_registers,
                Self::ABSOLUTE_MAX_COLOR_REGISTERS,
            ),
            (
                "configured_max_mouse_buttons",
                self.max_mouse_buttons,
                Self::ABSOLUTE_MAX_MOUSE_BUTTONS,
            ),
            (
                "configured_max_tab_stops",
                self.max_tab_stops,
                Self::ABSOLUTE_MAX_TAB_STOPS,
            ),
            (
                "configured_max_user_vars",
                self.max_user_vars,
                Self::ABSOLUTE_MAX_USER_VARS,
            ),
            (
                "configured_max_unicode_stack_depth",
                self.max_unicode_stack_depth,
                Self::ABSOLUTE_MAX_UNICODE_STACK_DEPTH,
            ),
            (
                "configured_max_total_custom_cell_widths",
                self.max_total_custom_cell_widths,
                Self::ABSOLUTE_MAX_TOTAL_CUSTOM_CELL_WIDTHS,
            ),
            (
                "configured_max_terminal_string_bytes",
                self.max_terminal_string_bytes,
                Self::ABSOLUTE_MAX_TERMINAL_STRING_BYTES,
            ),
            (
                "configured_max_pixel_dimension",
                self.max_pixel_dimension,
                Self::ABSOLUTE_MAX_PIXEL_DIMENSION,
            ),
        ] {
            ensure_limit(resource, observed, maximum)?;
        }
        if self.max_dpi > Self::ABSOLUTE_MAX_DPI {
            return Err(resource_limit(
                "configured_max_dpi",
                usize::try_from(self.max_dpi).unwrap_or(usize::MAX),
                usize::try_from(Self::ABSOLUTE_MAX_DPI).unwrap_or(usize::MAX),
            ));
        }
        for (field, value) in [
            ("limits.max_encoded_bytes", self.max_encoded_bytes),
            (
                "limits.max_retained_capture_bytes",
                self.max_retained_capture_bytes,
            ),
            ("limits.max_replay_record_bytes", self.max_replay_record_bytes),
            ("limits.max_replay_total_bytes", self.max_replay_total_bytes),
            ("limits.max_replay_records", self.max_replay_records),
            (
                "limits.max_replay_actions_per_record",
                self.max_replay_actions_per_record,
            ),
            ("limits.max_rows", self.max_rows),
            ("limits.max_cols", self.max_cols),
            (
                "limits.max_visible_grid_cells",
                self.max_visible_grid_cells,
            ),
            ("limits.max_total_lines", self.max_total_lines),
            (
                "limits.max_total_cell_records",
                self.max_total_cell_records,
            ),
            ("limits.max_total_cells", self.max_total_cells),
            (
                "limits.max_total_cell_text_bytes",
                self.max_total_cell_text_bytes,
            ),
            ("limits.max_string_bytes", self.max_string_bytes),
        ] {
            if value == 0 {
                return Err(TerminalCheckpointError::InvalidField {
                    field,
                    reason: "limit must be nonzero",
                });
            }
        }
        if self.max_hyperlink_params_per_link > self.max_total_hyperlink_params {
            return Err(TerminalCheckpointError::InvalidField {
                field: "limits.max_hyperlink_params_per_link",
                reason: "per-link maximum exceeds the aggregate maximum",
            });
        }
        if self.max_replay_record_bytes > self.max_replay_total_bytes {
            return Err(TerminalCheckpointError::InvalidField {
                field: "limits.max_replay_record_bytes",
                reason: "per-record maximum exceeds the aggregate replay maximum",
            });
        }
        let action_size = std::mem::size_of::<frankenterm_escape_parser::Action>();
        let maximum_actions_by_retained_bytes = self
            .max_retained_capture_bytes
            .checked_div(action_size)
            .unwrap_or(0);
        if self.max_replay_actions_per_record > maximum_actions_by_retained_bytes {
            return Err(TerminalCheckpointError::InvalidField {
                field: "limits.max_replay_actions_per_record",
                reason: "action batch can exceed the retained-memory envelope",
            });
        }
        Ok(())
    }

    fn screen_limits(self) -> crate::screen::ScreenCheckpointLimits {
        crate::screen::ScreenCheckpointLimits {
            max_total_lines: self.max_total_lines,
            max_total_cell_records: self.max_total_cell_records,
            max_total_cells: self.max_total_cells,
            max_total_cell_text_bytes: self.max_total_cell_text_bytes,
            max_total_hyperlink_bytes: self.max_total_hyperlink_bytes,
            max_total_hyperlink_params: self.max_total_hyperlink_params,
            max_string_bytes: self.max_string_bytes,
            max_hyperlink_params_per_link: self.max_hyperlink_params_per_link,
            max_cold_scrollback_bytes: self.max_cold_scrollback_bytes,
            max_keyboard_stack_depth: self.max_keyboard_stack_depth,
            max_rows: self.max_rows,
            max_cols: self.max_cols,
            max_visible_grid_cells: self.max_visible_grid_cells,
            max_retained_capture_bytes: self.max_retained_capture_bytes,
            estimated_bytes_per_line: std::mem::size_of::<Line>()
                .saturating_add(std::mem::size_of::<CheckpointLine>()),
            estimated_bytes_per_cell: std::mem::size_of::<Cell>()
                .saturating_add(std::mem::size_of::<CheckpointCell>()),
        }
    }
}

/// Capability-free semantic projection of one terminal model.
///
/// Fields remain private so callers cannot construct an unvalidated authority;
/// serde decoding will be paired with a validating constructor before restore.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalCheckpointV2 {
    version: u32,
    custom_cell_width_maps: Vec<CheckpointCustomCellWidthMap>,
    replay_config: CheckpointReplayConfigV2,
    primary_screen: CheckpointScreen,
    alternate_screen: CheckpointScreen,
    alternate_screen_active: bool,
    pen: CheckpointCellAttributes,
    cursor: CheckpointCursorPosition,
    wrap_next: bool,
    clear_semantic_attribute_on_newline: bool,
    last_semantic_command_status: Option<i32>,
    insert: bool,
    dec_auto_wrap: bool,
    saved_dec_private_modes: BTreeMap<CheckpointSavedDecMode, bool>,
    reverse_wraparound_mode: bool,
    reverse_video_mode: bool,
    synchronized_output: bool,
    dec_origin_mode: bool,
    top_margin: VisibleRowIndex,
    bottom_margin: VisibleRowIndex,
    left_margin: u64,
    right_margin: u64,
    left_and_right_margin_mode: bool,
    application_cursor_keys: bool,
    modify_other_keys: Option<i64>,
    dec_ansi_mode: bool,
    sixel_display_mode: bool,
    use_private_color_registers_for_each_graphic: bool,
    color_map: BTreeMap<u16, CheckpointRgbColor>,
    application_keypad: bool,
    bracketed_paste: bool,
    any_event_mouse: bool,
    focus_tracking: bool,
    mouse_encoding: CheckpointMouseEncoding,
    mouse_tracking: bool,
    button_event_mouse: bool,
    current_mouse_buttons: Vec<CheckpointMouseButton>,
    last_mouse_move: Option<CheckpointMouseEvent>,
    cursor_visible: bool,
    keyboard_encoding: CheckpointKeyboardEncoding,
    g0_charset: CheckpointCharSet,
    g1_charset: CheckpointCharSet,
    shift_out: bool,
    newline_mode: bool,
    tab_stops: Vec<bool>,
    tab_width: u64,
    title: String,
    icon_title: Option<String>,
    progress: Progress,
    palette: Option<CheckpointColorPalette>,
    pixel_width: u64,
    pixel_height: u64,
    dpi: u32,
    current_dir: Option<String>,
    term_program: String,
    term_version: String,
    sixel_scrolls_right: bool,
    user_vars: BTreeMap<String, String>,
    kitty_max_image_id: u32,
    seqno: u64,
    unicode_version: CheckpointUnicodeVersion,
    unicode_version_stack: Vec<CheckpointUnicodeVersionStackEntry>,
    enable_conpty_quirks: bool,
    suppress_initial_title_change: bool,
    accumulating_title: Option<String>,
    lost_focus_seqno: u64,
    lost_focus_alerted_seqno: u64,
    focused: bool,
    bidi_enabled: Option<bool>,
    bidi_hint: Option<CheckpointBidiHint>,
}

impl std::fmt::Debug for TerminalCheckpointV2 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalCheckpointV2")
            .field("version", &self.version)
            .field("rows", &self.primary_screen.physical_rows)
            .field("cols", &self.primary_screen.physical_cols)
            .field("primary_lines", &self.primary_screen.lines.len())
            .field(
                "primary_cold_prefix_lines",
                &self.primary_screen.cold_prefix_line_count,
            )
            .field(
                "primary_cold_generation_present",
                &self.primary_screen.cold_snapshot_generation.is_some(),
            )
            .field("alternate_lines", &self.alternate_screen.lines.len())
            .field("alternate_screen_active", &self.alternate_screen_active)
            .field("title_bytes", &self.title.len())
            .field("user_var_count", &self.user_vars.len())
            .field(
                "custom_cell_width_map_count",
                &self.custom_cell_width_maps.len(),
            )
            .field("unicode_stack_depth", &self.unicode_version_stack.len())
            .field("seqno", &self.seqno)
            .finish_non_exhaustive()
    }
}

impl TerminalCheckpointV2 {
    fn preflight_checkpoint_attributes(
        attributes: &CellAttributes,
        limits: TerminalCheckpointLimits,
        usage: &mut crate::screen::ScreenCheckpointUsage,
    ) -> Result<(), TerminalCheckpointError> {
        if attributes.has_image_attachments() {
            return Err(TerminalCheckpointError::UnsupportedGraphicsState);
        }
        for color in [
            attributes.foreground(),
            attributes.background(),
            attributes.underline_color(),
        ] {
            CheckpointColorAttribute::capture(color)?;
        }
        let Some(link) = attributes.hyperlink() else {
            return Ok(());
        };
        ensure_limit(
            "hyperlink_params_per_link",
            link.params().len(),
            limits.max_hyperlink_params_per_link,
        )?;
        checked_accumulate(
            &mut usage.hyperlink_params,
            link.params().len(),
            limits.max_total_hyperlink_params,
            "hyperlink_params",
        )?;
        ensure_limit(
            "hyperlink_uri_bytes",
            link.uri().len(),
            limits.max_string_bytes,
        )?;
        let mut link_bytes = link.uri().len();
        for (key, value) in link.params() {
            ensure_limit(
                "hyperlink_param_key_bytes",
                key.len(),
                limits.max_string_bytes,
            )?;
            ensure_limit(
                "hyperlink_param_value_bytes",
                value.len(),
                limits.max_string_bytes,
            )?;
            link_bytes = link_bytes
                .checked_add(key.len())
                .and_then(|bytes| bytes.checked_add(value.len()))
                .ok_or(TerminalCheckpointError::ArithmeticOverflow(
                    "hyperlink_bytes",
                ))?;
        }
        checked_accumulate(
            &mut usage.hyperlink_bytes,
            link_bytes,
            limits.max_total_hyperlink_bytes,
            "hyperlink_bytes",
        )?;
        checked_accumulate(
            &mut usage.retained_capture_bytes,
            link_bytes,
            limits.max_retained_capture_bytes,
            "retained_capture_bytes",
        )?;
        let parameter_overhead = link
            .params()
            .len()
            .checked_mul(2 * std::mem::size_of::<String>() + 48)
            .ok_or(TerminalCheckpointError::ArithmeticOverflow(
                "retained_capture_bytes",
            ))?;
        checked_accumulate(
            &mut usage.retained_capture_bytes,
            parameter_overhead,
            limits.max_retained_capture_bytes,
            "retained_capture_bytes",
        )?;
        Ok(())
    }

    fn preflight_terminal_fields(
        terminal: &TerminalState,
        limits: TerminalCheckpointLimits,
        usage: &mut crate::screen::ScreenCheckpointUsage,
        require_checkpoint_boundary: bool,
    ) -> Result<(), TerminalCheckpointError> {
        fn charge_records(
            usage: &mut crate::screen::ScreenCheckpointUsage,
            count: usize,
            bytes_per_record: usize,
            limits: TerminalCheckpointLimits,
        ) -> Result<(), TerminalCheckpointError> {
            let bytes = count.checked_mul(bytes_per_record).ok_or(
                TerminalCheckpointError::ArithmeticOverflow("retained_capture_bytes"),
            )?;
            checked_accumulate(
                &mut usage.retained_capture_bytes,
                bytes,
                limits.max_retained_capture_bytes,
                "retained_capture_bytes",
            )
        }

        fn charge_string(
            value: &str,
            total: &mut usize,
            usage: &mut crate::screen::ScreenCheckpointUsage,
            limits: TerminalCheckpointLimits,
        ) -> Result<(), TerminalCheckpointError> {
            ensure_limit("terminal_string_value_bytes", value.len(), limits.max_string_bytes)?;
            checked_accumulate(
                total,
                value.len(),
                limits.max_terminal_string_bytes,
                "terminal_string_bytes",
            )?;
            checked_accumulate(
                &mut usage.retained_capture_bytes,
                value.len(),
                limits.max_retained_capture_bytes,
                "retained_capture_bytes",
            )
        }

        checked_accumulate(
            &mut usage.retained_capture_bytes,
            std::mem::size_of::<Self>(),
            limits.max_retained_capture_bytes,
            "retained_capture_bytes",
        )?;
        ensure_limit(
            "saved_dec_private_modes",
            terminal.saved_dec_private_modes.len(),
            limits.max_saved_dec_private_modes,
        )?;
        ensure_limit(
            "color_registers",
            terminal.color_map.len(),
            limits.max_color_registers,
        )?;
        ensure_limit(
            "current_mouse_buttons",
            terminal.current_mouse_buttons.len(),
            limits.max_mouse_buttons,
        )?;
        ensure_limit("tab_stops", terminal.tabs.tabs.len(), limits.max_tab_stops)?;
        ensure_limit("user_vars", terminal.user_vars.len(), limits.max_user_vars)?;
        ensure_limit(
            "unicode_version_stack",
            terminal.unicode_version_stack.len(),
            limits.max_unicode_stack_depth,
        )?;
        ensure_limit(
            "pixel_width",
            terminal.pixel_width,
            limits.max_pixel_dimension,
        )?;
        ensure_limit(
            "pixel_height",
            terminal.pixel_height,
            limits.max_pixel_dimension,
        )?;
        if terminal.dpi > limits.max_dpi {
            return Err(resource_limit(
                "dpi",
                usize::try_from(terminal.dpi).unwrap_or(usize::MAX),
                usize::try_from(limits.max_dpi).unwrap_or(usize::MAX),
            ));
        }
        if terminal.tabs.tab_width == 0 {
            return Err(TerminalCheckpointError::InvalidField {
                field: "tab_width",
                reason: "tab width must be nonzero",
            });
        }
        if terminal.modify_other_keys == Some(0) {
            return Err(TerminalCheckpointError::InvalidField {
                field: "modify_other_keys",
                reason: "zero must be represented as none",
            });
        }
        if require_checkpoint_boundary && terminal.synchronized_output {
            return Err(TerminalCheckpointError::InvalidField {
                field: "synchronized_output",
                reason: "guardian checkpoints require a completed synchronized-output frame",
            });
        }
        if terminal.seqno == 0 || terminal.seqno == SequenceNo::MAX {
            return Err(TerminalCheckpointError::InvalidField {
                field: "seqno",
                reason: "terminal sequence number must be nonzero and unsaturated",
            });
        }

        let mut pressed = 0u8;
        for button in terminal.current_mouse_buttons.iter().copied() {
            let button = CheckpointMouseButton::capture(button)?;
            let Some(bit) = button.pressed_bit() else {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "current_mouse_buttons",
                    reason: "only physical pressed buttons may be retained",
                });
            };
            if pressed & bit != 0 {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "current_mouse_buttons",
                    reason: "pressed buttons must be unique",
                });
            }
            pressed |= bit;
        }
        if let Some(event) = terminal.last_mouse_move {
            if event.kind != MouseEventKind::Move {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "last_mouse_move.kind",
                    reason: "the retained mouse event must be a move",
                });
            }
        }

        let mut terminal_string_bytes = 0usize;
        for value in [
            Some(terminal.title.as_str()),
            terminal.icon_title.as_deref(),
            terminal.current_dir.as_ref().map(Url::as_str),
            Some(terminal.term_program.as_str()),
            Some(terminal.term_version.as_str()),
            terminal.accumulating_title.as_deref(),
        ]
        .iter()
        .copied()
        .flatten()
        {
            charge_string(value, &mut terminal_string_bytes, usage, limits)?;
        }
        for (name, value) in &terminal.user_vars {
            charge_string(name, &mut terminal_string_bytes, usage, limits)?;
            charge_string(value, &mut terminal_string_bytes, usage, limits)?;
        }
        for entry in &terminal.unicode_version_stack {
            if let Some(label) = entry.label.as_deref() {
                charge_string(label, &mut terminal_string_bytes, usage, limits)?;
            }
        }

        for version in std::iter::once(&terminal.unicode_version)
            .chain(terminal.unicode_version_stack.iter().map(|entry| &entry.vers))
        {
            if let Some(widths) = version.cell_widths.as_ref() {
                ensure_limit(
                    "custom_cell_widths_per_map",
                    widths.len(),
                    frankenterm_cell::MAX_CUSTOM_CELL_WIDTH_EXPANSION,
                )?;
                for (codepoint, width) in widths.iter() {
                    if char::from_u32(*codepoint).is_none() {
                        return Err(TerminalCheckpointError::InvalidField {
                            field: "unicode_version.custom_cell_widths",
                            reason: "custom-width keys must be Unicode scalar values",
                        });
                    }
                    if !(1..=2).contains(width) {
                        return Err(TerminalCheckpointError::InvalidField {
                            field: "unicode_version.custom_cell_widths",
                            reason: "custom widths must be one or two columns",
                        });
                    }
                }
            }
        }

        if let Some(palette) = terminal.palette.as_ref() {
            for color in palette.colors.0.iter().copied().chain([
                palette.foreground,
                palette.background,
                palette.cursor_fg,
                palette.cursor_bg,
                palette.cursor_border,
                palette.selection_fg,
                palette.selection_bg,
                palette.scrollbar_thumb,
                palette.split,
            ]) {
                CheckpointSrgba::capture(color)?;
            }
        }

        charge_records(
            usage,
            terminal.saved_dec_private_modes.len(),
            64,
            limits,
        )?;
        charge_records(usage, terminal.color_map.len(), 64, limits)?;
        charge_records(
            usage,
            terminal.current_mouse_buttons.len(),
            std::mem::size_of::<CheckpointMouseButton>(),
            limits,
        )?;
        charge_records(
            usage,
            terminal.tabs.tabs.len(),
            std::mem::size_of::<bool>(),
            limits,
        )?;
        charge_records(usage, terminal.user_vars.len(), 128, limits)?;
        charge_records(
            usage,
            terminal.unicode_version_stack.len(),
            std::mem::size_of::<CheckpointUnicodeVersionStackEntry>() + 32,
            limits,
        )?;
        Ok(())
    }

    /// Validate the currently resident semantic model against the replay hard
    /// envelope without cloning lines or consulting a cold-storage capability.
    ///
    /// Guardian replay uses this after each bounded journal record.  The inert
    /// replay configuration has no spill sink, so all reachable state is
    /// resident and this is a complete allocation-accounting pass.
    pub(crate) fn validate_inert_replay_resources(
        terminal: &TerminalState,
        replay_projection: &CheckpointReplayConfigV2,
        custom_cell_width_maps: &[Arc<HashMap<u32, u8>>],
        limits: TerminalCheckpointLimits,
    ) -> Result<(), TerminalCheckpointError> {
        limits.validate_policy()?;
        let mut usage = crate::screen::ScreenCheckpointUsage::default();
        validate_live_custom_cell_width_maps(custom_cell_width_maps, limits, &mut usage)?;
        let table_count = custom_cell_width_maps.len();
        replay_projection.validate(limits, table_count, &mut usage)?;
        let current_index =
            live_custom_cell_width_map_index(&terminal.unicode_version, custom_cell_width_maps)?;
        for entry in &terminal.unicode_version_stack {
            let saved_index =
                live_custom_cell_width_map_index(&entry.vers, custom_cell_width_maps)?;
            if saved_index != current_index {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "unicode_version_stack",
                    reason: "saved Unicode versions must share the current custom-width map",
                });
            }
        }
        Self::preflight_terminal_fields(
            terminal,
            limits,
            &mut usage,
            false,
        )?;
        terminal
            .kitty_img
            .checkpoint_high_water_if_quiescent()
            .ok_or(TerminalCheckpointError::UnsupportedGraphicsState)?;
        Self::preflight_checkpoint_attributes(&terminal.pen, limits, &mut usage)?;
        for saved in [
            terminal.screen.screen.saved_cursor.as_ref(),
            terminal.screen.alt_screen.saved_cursor.as_ref(),
        ]
        .iter()
        .copied()
        .flatten()
        {
            Self::preflight_checkpoint_attributes(&saved.pen, limits, &mut usage)?;
        }
        let screen_limits = limits.screen_limits();
        terminal
            .screen
            .screen
            .preflight_resident_checkpoint_usage(&screen_limits, &mut usage)?;
        terminal
            .screen
            .screen
            .preflight_recovery_checkpoint_boundary()?;
        terminal
            .screen
            .alt_screen
            .preflight_resident_checkpoint_usage(&screen_limits, &mut usage)?;
        terminal
            .screen
            .alt_screen
            .preflight_recovery_checkpoint_boundary()?;
        Ok(())
    }

    /// Capture every supported semantic field or reject unsupported graphics state.
    #[cfg(test)]
    pub(crate) fn capture(terminal: &TerminalState) -> Result<Self, TerminalCheckpointError> {
        Self::capture_with_limits(terminal, TerminalCheckpointLimits::default())
    }

    /// Capture under an explicit hard resource envelope.  Both screens are
    /// preflighted before resident lines are cloned, and reachable cold rows are
    /// materialized only after their sink ledger passes the same envelope.
    pub(crate) fn capture_with_limits(
        terminal: &TerminalState,
        limits: TerminalCheckpointLimits,
    ) -> Result<Self, TerminalCheckpointError> {
        limits.validate_policy()?;
        let lease_config = Arc::clone(&terminal.config);
        let _config_lease = lease_config.acquire_recovery_activation_lease();
        let pending_replay_config =
            PendingReplayConfigV2::capture(terminal.config.as_ref(), limits)?;
        for entry in &terminal.unicode_version_stack {
            if !CheckpointCustomCellWidthTableBuilder::live_maps_equal(
                &entry.vers,
                &terminal.unicode_version,
            ) {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "unicode_version_stack",
                    reason: "saved Unicode versions must share the current custom-width map",
                });
            }
        }
        let mut width_table_builder = CheckpointCustomCellWidthTableBuilder::default();
        width_table_builder.register(&pending_replay_config.unicode_version, limits)?;
        width_table_builder.register(&terminal.unicode_version, limits)?;
        for entry in &terminal.unicode_version_stack {
            width_table_builder.register(&entry.vers, limits)?;
        }
        let custom_cell_width_maps = width_table_builder.finish();
        let replay_config = pending_replay_config.into_checkpoint(&custom_cell_width_maps)?;
        let mut screen_usage = crate::screen::ScreenCheckpointUsage::default();
        validate_custom_cell_width_maps(&custom_cell_width_maps, limits, &mut screen_usage)?;
        replay_config.validate(limits, custom_cell_width_maps.len(), &mut screen_usage)?;
        Self::preflight_terminal_fields(
            terminal,
            limits,
            &mut screen_usage,
            true,
        )?;
        let kitty_max_image_id = terminal
            .kitty_img
            .checkpoint_high_water_if_quiescent()
            .ok_or(TerminalCheckpointError::UnsupportedGraphicsState)?;
        let screen_limits = limits.screen_limits();
        Self::preflight_checkpoint_attributes(
            &terminal.pen,
            limits,
            &mut screen_usage,
        )?;
        for saved in [
            terminal.screen.screen.saved_cursor.as_ref(),
            terminal.screen.alt_screen.saved_cursor.as_ref(),
        ]
        .iter()
        .copied()
        .flatten()
        {
            Self::preflight_checkpoint_attributes(
                &saved.pen,
                limits,
                &mut screen_usage,
            )?;
        }
        let primary_screen = terminal
            .screen
            .screen
            .checkpoint_parts(&screen_limits, &mut screen_usage)?;
        let alternate_screen = terminal
            .screen
            .alt_screen
            .checkpoint_parts(&screen_limits, &mut screen_usage)?;
        let checkpoint_unicode_version =
            CheckpointUnicodeVersion::capture(&terminal.unicode_version, &custom_cell_width_maps)?;
        let mut checkpoint_unicode_version_stack = Vec::new();
        checkpoint_unicode_version_stack
            .try_reserve_exact(terminal.unicode_version_stack.len())
            .map_err(|_| {
                TerminalCheckpointError::ResourceAllocation("unicode_version_stack")
            })?;
        for entry in &terminal.unicode_version_stack {
            checkpoint_unicode_version_stack.push(CheckpointUnicodeVersionStackEntry {
                version: CheckpointUnicodeVersion::capture(
                    &entry.vers,
                    &custom_cell_width_maps,
                )?,
                label: entry.label.clone(),
            });
        }

        let checkpoint = Self {
            version: TERMINAL_CHECKPOINT_VERSION,
            custom_cell_width_maps,
            replay_config,
            primary_screen: CheckpointScreen::capture(primary_screen)?,
            alternate_screen: CheckpointScreen::capture(alternate_screen)?,
            alternate_screen_active: terminal.screen.alt_screen_is_active,
            pen: CheckpointCellAttributes::capture(&terminal.pen)?,
            cursor: CheckpointCursorPosition::capture(terminal.cursor)?,
            wrap_next: terminal.wrap_next,
            clear_semantic_attribute_on_newline: terminal.clear_semantic_attribute_on_newline,
            last_semantic_command_status: terminal.last_semantic_command_status,
            insert: terminal.insert,
            dec_auto_wrap: terminal.dec_auto_wrap,
            saved_dec_private_modes: terminal
                .saved_dec_private_modes
                .iter()
                .map(|(mode, enabled)| Ok((CheckpointSavedDecMode::capture(*mode)?, *enabled)))
                .collect::<Result<BTreeMap<_, _>, TerminalCheckpointError>>()?,
            reverse_wraparound_mode: terminal.reverse_wraparound_mode,
            reverse_video_mode: terminal.reverse_video_mode,
            synchronized_output: terminal.synchronized_output,
            dec_origin_mode: terminal.dec_origin_mode,
            top_margin: terminal.top_and_bottom_margins.start,
            bottom_margin: terminal.top_and_bottom_margins.end,
            left_margin: u64_from_usize(
                terminal.left_and_right_margins.start,
                "left_margin",
            )?,
            right_margin: u64_from_usize(
                terminal.left_and_right_margins.end,
                "right_margin",
            )?,
            left_and_right_margin_mode: terminal.left_and_right_margin_mode,
            application_cursor_keys: terminal.application_cursor_keys,
            modify_other_keys: terminal.modify_other_keys,
            dec_ansi_mode: terminal.dec_ansi_mode,
            sixel_display_mode: terminal.sixel_display_mode,
            use_private_color_registers_for_each_graphic: terminal
                .use_private_color_registers_for_each_graphic,
            color_map: terminal
                .color_map
                .iter()
                .map(|(index, color)| (*index, CheckpointRgbColor::from(*color)))
                .collect(),
            application_keypad: terminal.application_keypad,
            bracketed_paste: terminal.bracketed_paste,
            any_event_mouse: terminal.any_event_mouse,
            focus_tracking: terminal.focus_tracking,
            mouse_encoding: terminal.mouse_encoding.into(),
            mouse_tracking: terminal.mouse_tracking,
            button_event_mouse: terminal.button_event_mouse,
            current_mouse_buttons: terminal
                .current_mouse_buttons
                .iter()
                .copied()
                .map(CheckpointMouseButton::capture)
                .collect::<Result<Vec<_>, _>>()?,
            last_mouse_move: terminal
                .last_mouse_move
                .map(CheckpointMouseEvent::capture)
                .transpose()?,
            cursor_visible: terminal.cursor_visible,
            keyboard_encoding: terminal.keyboard_encoding.into(),
            g0_charset: terminal.g0_charset.into(),
            g1_charset: terminal.g1_charset.into(),
            shift_out: terminal.shift_out,
            newline_mode: terminal.newline_mode,
            tab_stops: terminal.tabs.tabs.clone(),
            tab_width: u64_from_usize(terminal.tabs.tab_width, "tab_width")?,
            title: terminal.title.clone(),
            icon_title: terminal.icon_title.clone(),
            progress: terminal.progress.clone(),
            palette: terminal
                .palette
                .as_ref()
                .map(CheckpointColorPalette::capture)
                .transpose()?,
            pixel_width: u64_from_usize(terminal.pixel_width, "pixel_width")?,
            pixel_height: u64_from_usize(terminal.pixel_height, "pixel_height")?,
            dpi: terminal.dpi,
            current_dir: terminal.current_dir.as_ref().map(ToString::to_string),
            term_program: terminal.term_program.clone(),
            term_version: terminal.term_version.clone(),
            sixel_scrolls_right: terminal.sixel_scrolls_right,
            user_vars: terminal
                .user_vars
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            kitty_max_image_id,
            seqno: u64_from_usize(terminal.seqno, "seqno")?,
            unicode_version: checkpoint_unicode_version,
            unicode_version_stack: checkpoint_unicode_version_stack,
            enable_conpty_quirks: terminal.enable_conpty_quirks,
            suppress_initial_title_change: terminal.suppress_initial_title_change,
            accumulating_title: terminal.accumulating_title.clone(),
            lost_focus_seqno: u64_from_usize(
                terminal.lost_focus_seqno,
                "lost_focus_seqno",
            )?,
            lost_focus_alerted_seqno: u64_from_usize(
                terminal.lost_focus_alerted_seqno,
                "lost_focus_alerted_seqno",
            )?,
            focused: terminal.focused,
            bidi_enabled: terminal.bidi_enabled,
            bidi_hint: terminal.bidi_hint.map(CheckpointBidiHint::from),
        };
        checkpoint.validate(limits)?;
        Ok(checkpoint)
    }

    /// Validate the complete semantic authority before any runtime object,
    /// writer, handler, spill sink, or parser is constructed.
    pub fn validate(
        &self,
        limits: TerminalCheckpointLimits,
    ) -> Result<(), TerminalCheckpointError> {
        fn charge_string(
            value: &str,
            total: &mut usize,
            usage: &mut crate::screen::ScreenCheckpointUsage,
            limits: TerminalCheckpointLimits,
        ) -> Result<(), TerminalCheckpointError> {
            ensure_limit("terminal_string_value_bytes", value.len(), limits.max_string_bytes)?;
            checked_accumulate(
                total,
                value.len(),
                limits.max_terminal_string_bytes,
                "terminal_string_bytes",
            )?;
            checked_accumulate(
                &mut usage.retained_capture_bytes,
                value.len(),
                limits.max_retained_capture_bytes,
                "retained_capture_bytes",
            )
        }

        fn charge_records(
            usage: &mut crate::screen::ScreenCheckpointUsage,
            count: usize,
            bytes_per_record: usize,
            limits: TerminalCheckpointLimits,
        ) -> Result<(), TerminalCheckpointError> {
            let bytes = count.checked_mul(bytes_per_record).ok_or(
                TerminalCheckpointError::ArithmeticOverflow("retained_capture_bytes"),
            )?;
            checked_accumulate(
                &mut usage.retained_capture_bytes,
                bytes,
                limits.max_retained_capture_bytes,
                "retained_capture_bytes",
            )
        }

        limits.validate_policy()?;
        if self.version != TERMINAL_CHECKPOINT_VERSION {
            return Err(TerminalCheckpointError::UnsupportedVersion {
                observed: self.version,
                supported: TERMINAL_CHECKPOINT_VERSION,
            });
        }
        let native_seqno = usize_from_u64(self.seqno, "seqno")?;
        if native_seqno == 0 || native_seqno == SequenceNo::MAX {
            return Err(TerminalCheckpointError::InvalidField {
                field: "seqno",
                reason: "terminal sequence number must be nonzero and unsaturated",
            });
        }
        if self.synchronized_output {
            return Err(TerminalCheckpointError::InvalidField {
                field: "synchronized_output",
                reason: "guardian checkpoints require a completed synchronized-output frame",
            });
        }
        if self.modify_other_keys == Some(0) {
            return Err(TerminalCheckpointError::InvalidField {
                field: "modify_other_keys",
                reason: "zero must be represented as none",
            });
        }
        if self.primary_screen.physical_rows != self.alternate_screen.physical_rows
            || self.primary_screen.physical_cols != self.alternate_screen.physical_cols
            || self.primary_screen.dpi != self.alternate_screen.dpi
            || self.primary_screen.dpi != self.dpi
        {
            return Err(TerminalCheckpointError::InvalidField {
                field: "screen.geometry",
                reason: "primary, alternate, and terminal geometry must agree",
            });
        }

        let mut usage = crate::screen::ScreenCheckpointUsage::default();
        checked_accumulate(
            &mut usage.retained_capture_bytes,
            std::mem::size_of::<Self>(),
            limits.max_retained_capture_bytes,
            "retained_capture_bytes",
        )?;
        self.pen.validate(limits, &mut usage)?;
        validate_custom_cell_width_maps(&self.custom_cell_width_maps, limits, &mut usage)?;
        let table_count = self.custom_cell_width_maps.len();
        self.replay_config
            .validate(limits, table_count, &mut usage)?;
        let mut referenced_tables = 0u8;
        if let Some(index) = self
            .replay_config
            .unicode_version
            .referenced_index(table_count)?
        {
            referenced_tables |= 1u8 << index;
        }
        self.primary_screen
            .validate(true, limits, &mut usage, self.seqno)?;
        self.alternate_screen
            .validate(false, limits, &mut usage, self.seqno)?;

        let rows = self.primary_screen.physical_rows;
        let cols = self.primary_screen.physical_cols;
        let rows_usize = usize::try_from(rows).map_err(|_| {
            TerminalCheckpointError::InvalidField {
                field: "primary_screen.physical_rows",
                reason: "value does not fit this target architecture",
            }
        })?;
        let configured_scrollback = usize_from_u64(
            self.replay_config.scrollback_size,
            "config.scrollback_size",
        )?;
        let aggregate_visible_rows = rows_usize
            .checked_mul(2)
            .ok_or(TerminalCheckpointError::ArithmeticOverflow(
                "configured_total_lines",
            ))?;
        if configured_scrollback
            .checked_add(aggregate_visible_rows)
            .is_none_or(|total_lines| total_lines > limits.max_total_lines)
        {
            return Err(TerminalCheckpointError::InvalidField {
                field: "replay_config.scrollback_size",
                reason: "configuration can exceed the aggregate screen allocation envelope",
            });
        }
        self.cursor.validate(cols, rows, self.seqno, "cursor")?;
        if self.top_margin < 0
            || self.top_margin >= self.bottom_margin
            || self.bottom_margin > i64::from(rows)
        {
            return Err(TerminalCheckpointError::InvalidField {
                field: "top_and_bottom_margins",
                reason: "vertical margins must be a nonempty in-bounds half-open range",
            });
        }
        if self.left_margin >= self.right_margin || self.right_margin > u64::from(cols) {
            return Err(TerminalCheckpointError::InvalidField {
                field: "left_and_right_margins",
                reason: "horizontal margins must be a nonempty in-bounds half-open range",
            });
        }
        usize_from_u64(self.left_margin, "left_margin")?;
        usize_from_u64(self.right_margin, "right_margin")?;
        let tab_width = usize_from_u64(self.tab_width, "tab_width")?;
        if tab_width != 8 {
            return Err(TerminalCheckpointError::InvalidField {
                field: "tab_width",
                reason: "the checkpoint schema requires the producer tab width of eight",
            });
        }
        ensure_limit("tab_stops", self.tab_stops.len(), limits.max_tab_stops)?;
        if self.tab_stops.len() < usize::try_from(cols).unwrap_or(usize::MAX) {
            return Err(TerminalCheckpointError::InvalidField {
                field: "tab_stops",
                reason: "tab-stop vector cannot be shorter than the screen width",
            });
        }
        ensure_limit(
            "saved_dec_private_modes",
            self.saved_dec_private_modes.len(),
            limits.max_saved_dec_private_modes,
        )?;
        ensure_limit(
            "color_registers",
            self.color_map.len(),
            limits.max_color_registers,
        )?;
        for color in self.color_map.values().copied() {
            color.validate()?;
        }
        if let Some(palette) = self.palette.as_ref() {
            palette.validate()?;
        }
        let pixel_width = usize_from_u64(self.pixel_width, "pixel_width")?;
        let pixel_height = usize_from_u64(self.pixel_height, "pixel_height")?;
        ensure_limit("pixel_width", pixel_width, limits.max_pixel_dimension)?;
        ensure_limit("pixel_height", pixel_height, limits.max_pixel_dimension)?;
        if self.dpi > limits.max_dpi {
            return Err(resource_limit(
                "dpi",
                usize::try_from(self.dpi).unwrap_or(usize::MAX),
                usize::try_from(limits.max_dpi).unwrap_or(usize::MAX),
            ));
        }

        match self.keyboard_encoding {
            CheckpointKeyboardEncoding::Xterm | CheckpointKeyboardEncoding::Win32 => {}
            _ => {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "keyboard_encoding",
                    reason: "global keyboard encoding must be xterm or win32",
                });
            }
        }
        ensure_limit(
            "current_mouse_buttons",
            self.current_mouse_buttons.len(),
            limits.max_mouse_buttons,
        )?;
        let mut pressed = 0u8;
        for button in self.current_mouse_buttons.iter().copied() {
            let Some(bit) = button.pressed_bit() else {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "current_mouse_buttons",
                    reason: "only physical pressed buttons may be retained",
                });
            };
            if pressed & bit != 0 {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "current_mouse_buttons",
                    reason: "pressed buttons must be unique",
                });
            }
            pressed |= bit;
        }
        if let Some(event) = self.last_mouse_move {
            event.validate()?;
        }
        match &self.progress {
            Progress::Percentage(value) | Progress::Error(value) if *value > 100 => {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "progress",
                    reason: "progress values must be at most one hundred",
                });
            }
            _ => {}
        }
        if self.lost_focus_seqno > self.seqno || self.lost_focus_alerted_seqno > self.seqno {
            return Err(TerminalCheckpointError::InvalidField {
                field: "focus_seqno",
                reason: "focus sequence numbers cannot be newer than the terminal",
            });
        }
        usize_from_u64(self.lost_focus_seqno, "lost_focus_seqno")?;
        usize_from_u64(self.lost_focus_alerted_seqno, "lost_focus_alerted_seqno")?;

        ensure_limit("user_vars", self.user_vars.len(), limits.max_user_vars)?;
        ensure_limit(
            "unicode_version_stack",
            self.unicode_version_stack.len(),
            limits.max_unicode_stack_depth,
        )?;
        let mut terminal_string_bytes = 0usize;
        for value in [
            Some(self.title.as_str()),
            self.icon_title.as_deref(),
            self.current_dir.as_deref(),
            Some(self.term_program.as_str()),
            Some(self.term_version.as_str()),
            self.accumulating_title.as_deref(),
        ]
        .iter()
        .copied()
        .flatten()
        {
            charge_string(value, &mut terminal_string_bytes, &mut usage, limits)?;
        }
        for (name, value) in &self.user_vars {
            charge_string(name, &mut terminal_string_bytes, &mut usage, limits)?;
            charge_string(value, &mut terminal_string_bytes, &mut usage, limits)?;
        }
        if let Some(current_dir) = self.current_dir.as_ref() {
            let parsed = Url::parse(current_dir)
                .map_err(|_| TerminalCheckpointError::InvalidCurrentDirectory)?;
            if parsed.to_string() != *current_dir {
                return Err(TerminalCheckpointError::InvalidCurrentDirectory);
            }
        }

        let current_width_map = self.unicode_version.referenced_index(table_count)?;
        if let Some(index) = current_width_map {
            referenced_tables |= 1u8 << index;
        }
        for entry in &self.unicode_version_stack {
            let saved_width_map = entry.version.referenced_index(table_count)?;
            if saved_width_map != current_width_map {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "unicode_version_stack",
                    reason: "saved Unicode versions must share the current custom-width map",
                });
            }
            if let Some(index) = saved_width_map {
                referenced_tables |= 1u8 << index;
            }
            if let Some(label) = entry.label.as_deref() {
                charge_string(label, &mut terminal_string_bytes, &mut usage, limits)?;
            }
        }
        let expected_references = (1u8 << table_count) - 1;
        if referenced_tables != expected_references {
            return Err(TerminalCheckpointError::InvalidField {
                field: "custom_cell_width_maps",
                reason: "every custom-width map must be semantically referenced",
            });
        }

        charge_records(
            &mut usage,
            self.saved_dec_private_modes.len(),
            64,
            limits,
        )?;
        charge_records(&mut usage, self.color_map.len(), 64, limits)?;
        charge_records(
            &mut usage,
            self.current_mouse_buttons.len(),
            std::mem::size_of::<CheckpointMouseButton>(),
            limits,
        )?;
        charge_records(
            &mut usage,
            self.tab_stops.len(),
            std::mem::size_of::<bool>(),
            limits,
        )?;
        charge_records(&mut usage, self.user_vars.len(), 128, limits)?;
        charge_records(
            &mut usage,
            self.unicode_version_stack.len(),
            std::mem::size_of::<CheckpointUnicodeVersionStackEntry>() + 32,
            limits,
        )?;
        Ok(())
    }

    /// Encode the unique compact JSON representation under a writer-enforced
    /// byte ceiling.  Validation runs first, so impossible state is never
    /// emitted as a recovery authority.
    pub fn to_canonical_json(
        &self,
        limits: TerminalCheckpointLimits,
    ) -> Result<Zeroizing<Vec<u8>>, TerminalCheckpointError> {
        self.validate(limits)?;
        let mut writer = BoundedCheckpointWriter::new(limits.max_encoded_bytes);
        if serde_json::to_writer(&mut writer, self).is_err() {
            return Err(match writer.failure {
                Some(BoundedWriterFailure::Limit) => resource_limit(
                    "encoded_bytes",
                    limits.max_encoded_bytes.saturating_add(1),
                    limits.max_encoded_bytes,
                ),
                Some(BoundedWriterFailure::Allocation) => {
                    TerminalCheckpointError::ResourceAllocation("encoded_bytes")
                }
                None => TerminalCheckpointError::Serialization,
            });
        }
        Ok(writer.into_inner())
    }

    /// Decode only byte-for-byte canonical JSON for the current schema. An allocation-light
    /// structural pass enforces nesting, per-string, and retained-memory
    /// ceilings before the typed payload is materialized; semantic validation
    /// and exact re-encoding follow before an admitted authority is returned.
    pub fn decode_canonical_json(
        bytes: &[u8],
        limits: TerminalCheckpointLimits,
    ) -> Result<ValidatedTerminalCheckpointV2, TerminalCheckpointError> {
        limits.validate_policy()?;
        if bytes.is_empty() {
            return Err(TerminalCheckpointError::InvalidField {
                field: "encoded_payload",
                reason: "checkpoint payload must not be empty",
            });
        }
        ensure_limit("encoded_bytes", bytes.len(), limits.max_encoded_bytes)?;

        const VERSION_PREFIX: &[u8] = b"{\"version\":";
        if !bytes.starts_with(VERSION_PREFIX) {
            return Err(TerminalCheckpointError::NonCanonicalEncoding);
        }
        let version_tail = &bytes[VERSION_PREFIX.len()..];
        let comma = version_tail
            .iter()
            .position(|byte| *byte == b',')
            .ok_or(TerminalCheckpointError::Serialization)?;
        let version_text = std::str::from_utf8(&version_tail[..comma])
            .map_err(|_| TerminalCheckpointError::Serialization)?;
        let observed = version_text
            .parse::<u32>()
            .map_err(|_| TerminalCheckpointError::Serialization)?;
        if observed != TERMINAL_CHECKPOINT_VERSION {
            return Err(TerminalCheckpointError::UnsupportedVersion {
                observed,
                supported: TERMINAL_CHECKPOINT_VERSION,
            });
        }
        if version_text != "2" {
            return Err(TerminalCheckpointError::NonCanonicalEncoding);
        }

        let mut budget = JsonStructuralBudget {
            retained_bytes: 0,
            limits,
        };
        let mut structural_deserializer = serde_json::Deserializer::from_slice(bytes);
        JsonStructuralSeed {
            budget: &mut budget,
            depth: 0,
        }
        .deserialize(&mut structural_deserializer)
        .map_err(|_| TerminalCheckpointError::Serialization)?;
        structural_deserializer
            .end()
            .map_err(|_| TerminalCheckpointError::Serialization)?;

        let checkpoint: Self = serde_json::from_slice(bytes)
            .map_err(|_| TerminalCheckpointError::Serialization)?;
        checkpoint.validate(limits)?;
        let canonical = checkpoint.to_canonical_json(limits)?;
        if canonical.as_slice() != bytes {
            return Err(TerminalCheckpointError::NonCanonicalEncoding);
        }
        Ok(ValidatedTerminalCheckpointV2 { checkpoint, limits })
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }
}

impl ValidatedTerminalCheckpointV2 {
    /// Canonical physical row count proven equal across both screens during
    /// checkpoint validation.
    #[must_use]
    pub const fn rows(&self) -> u32 {
        self.checkpoint.primary_screen.physical_rows
    }

    /// Canonical physical column count proven equal across both screens during
    /// checkpoint validation.
    #[must_use]
    pub const fn cols(&self) -> u32 {
        self.checkpoint.primary_screen.physical_cols
    }

    /// Rebuild an off-topology terminal with no writer thread, callbacks, or
    /// spill capability. The supplied live configuration is retained only as
    /// the revision-fenced activation authority; replay runs against an
    /// immutable capability-free projection derived from the checkpoint.
    pub fn restore_inert(
        self,
        intended_live_config: Arc<dyn TerminalConfiguration>,
    ) -> Result<crate::InertTerminal, TerminalCheckpointError> {
        let custom_cell_width_maps =
            decode_custom_cell_width_maps(&self.checkpoint.custom_cell_width_maps)?;
        let lease_config = Arc::clone(&intended_live_config);
        let intended_live_config_revision = {
            let _lease = lease_config.acquire_recovery_activation_lease();
            let revision = lease_config.revision();
            if !self.checkpoint.replay_config.matches_stable(
                lease_config.as_ref(),
                self.limits,
                &custom_cell_width_maps,
            )? {
                return Err(TerminalCheckpointError::ReplayConfigurationMismatch);
            }
            revision
        };
        let replay_projection = self.checkpoint.replay_config.clone();
        let replay_config: Arc<dyn TerminalConfiguration> = Arc::new(
            replay_projection
                .to_replay_configuration(self.limits, &custom_cell_width_maps)?,
        );
        let state = TerminalState::from_validated_checkpoint(
            self.checkpoint,
            self.limits,
            replay_config,
            &custom_cell_width_maps,
        )?;
        Ok(crate::InertTerminal::from_restored_state(
            state,
            replay_projection,
            custom_cell_width_maps,
            intended_live_config,
            intended_live_config_revision,
            self.limits,
        ))
    }
}

impl TerminalState {
    fn from_validated_checkpoint(
        checkpoint: TerminalCheckpointV2,
        limits: TerminalCheckpointLimits,
        config: Arc<dyn TerminalConfiguration>,
        custom_cell_width_maps: &[Arc<HashMap<u32, u8>>],
    ) -> Result<Self, TerminalCheckpointError> {
        checkpoint.validate(limits)?;
        let rows = usize::try_from(checkpoint.primary_screen.physical_rows).map_err(|_| {
            TerminalCheckpointError::InvalidField {
                field: "primary_screen.physical_rows",
                reason: "value does not fit this target architecture",
            }
        })?;
        let cols = usize::try_from(checkpoint.primary_screen.physical_cols).map_err(|_| {
            TerminalCheckpointError::InvalidField {
                field: "primary_screen.physical_cols",
                reason: "value does not fit this target architecture",
            }
        })?;
        let configured_scrollback = config.scrollback_size();
        let aggregate_visible_rows = rows
            .checked_mul(2)
            .ok_or(TerminalCheckpointError::ArithmeticOverflow(
                "configured_total_lines",
            ))?;
        if configured_scrollback
            .checked_add(aggregate_visible_rows)
            .is_none_or(|total_lines| total_lines > limits.max_total_lines)
        {
            return Err(TerminalCheckpointError::InvalidRestoreConfiguration {
                reason: "scrollback retention exceeds the admitted allocation envelope",
            });
        }
        if config.scrollback_tier_config().enabled {
            return Err(TerminalCheckpointError::InvalidRestoreConfiguration {
                reason: "checkpoint replay requires tiering to remain disabled",
            });
        }
        if config.scrollback_spill_sink().is_some() {
            return Err(TerminalCheckpointError::InvalidRestoreConfiguration {
                reason: "checkpoint replay configuration must not expose a spill sink",
            });
        }
        if config.max_user_vars() > limits.max_user_vars
            || config.max_unicode_version_stack_depth() > limits.max_unicode_stack_depth
            || config.max_color_map_entries() > limits.max_color_registers
            || config.max_accumulating_title_len() > limits.max_string_bytes
            || config.max_accumulating_title_len() > limits.max_terminal_string_bytes
        {
            return Err(TerminalCheckpointError::InvalidRestoreConfiguration {
                reason: "runtime collection limits exceed the admitted allocation envelope",
            });
        }

        let TerminalCheckpointV2 {
            version: _,
            custom_cell_width_maps: _,
            replay_config: _,
            primary_screen,
            alternate_screen,
            alternate_screen_active,
            pen,
            cursor,
            wrap_next,
            clear_semantic_attribute_on_newline,
            last_semantic_command_status,
            insert,
            dec_auto_wrap,
            saved_dec_private_modes,
            reverse_wraparound_mode,
            reverse_video_mode,
            synchronized_output,
            dec_origin_mode,
            top_margin,
            bottom_margin,
            left_margin,
            right_margin,
            left_and_right_margin_mode,
            application_cursor_keys,
            modify_other_keys,
            dec_ansi_mode,
            sixel_display_mode,
            use_private_color_registers_for_each_graphic,
            color_map,
            application_keypad,
            bracketed_paste,
            any_event_mouse,
            focus_tracking,
            mouse_encoding,
            mouse_tracking,
            button_event_mouse,
            current_mouse_buttons,
            last_mouse_move,
            cursor_visible,
            keyboard_encoding,
            g0_charset,
            g1_charset,
            shift_out,
            newline_mode,
            tab_stops,
            tab_width,
            title,
            icon_title,
            progress,
            palette,
            pixel_width,
            pixel_height,
            dpi,
            current_dir,
            term_program,
            term_version,
            sixel_scrolls_right,
            user_vars,
            kitty_max_image_id,
            seqno,
            unicode_version,
            unicode_version_stack,
            enable_conpty_quirks,
            suppress_initial_title_change,
            accumulating_title,
            lost_focus_seqno,
            lost_focus_alerted_seqno,
            focused,
            bidi_enabled,
            bidi_hint,
        } = checkpoint;

        let seqno = usize_from_u64(seqno, "seqno")?;
        let size = TerminalSize {
            rows,
            cols,
            pixel_width: usize_from_u64(pixel_width, "pixel_width")?,
            pixel_height: usize_from_u64(pixel_height, "pixel_height")?,
            dpi,
        };
        let bidi_mode = config.bidi_mode();
        let primary_screen = Screen::from_validated_checkpoint_parts(
            primary_screen.into_live()?,
            &config,
            seqno,
            bidi_mode,
        )?;
        let alternate_screen = Screen::from_validated_checkpoint_parts(
            alternate_screen.into_live()?,
            &config,
            seqno,
            bidi_mode,
        )?;
        let screens = ScreenOrAlt {
            screen: primary_screen,
            alt_screen: alternate_screen,
            alt_screen_is_active: alternate_screen_active,
        };

        let mut restored_saved_modes = HashMap::new();
        restored_saved_modes
            .try_reserve(saved_dec_private_modes.len())
            .map_err(|_| {
                TerminalCheckpointError::ResourceAllocation("saved_dec_private_modes")
            })?;
        restored_saved_modes.extend(
            saved_dec_private_modes
                .into_iter()
                .map(|(mode, enabled)| (mode.into_live(), enabled)),
        );
        let mut restored_color_map = HashMap::new();
        restored_color_map
            .try_reserve(color_map.len())
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("color_map"))?;
        restored_color_map.extend(
            color_map
                .into_iter()
                .map(|(index, color)| (index, color.into_live())),
        );
        let mut restored_buttons = Vec::new();
        restored_buttons
            .try_reserve_exact(current_mouse_buttons.len())
            .map_err(|_| {
                TerminalCheckpointError::ResourceAllocation("current_mouse_buttons")
            })?;
        for button in current_mouse_buttons {
            restored_buttons.push(button.into_live()?);
        }
        let mut restored_user_vars = HashMap::new();
        restored_user_vars
            .try_reserve(user_vars.len())
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("user_vars"))?;
        restored_user_vars.extend(user_vars);
        let mut restored_unicode_stack = Vec::new();
        restored_unicode_stack
            .try_reserve_exact(unicode_version_stack.len())
            .map_err(|_| {
                TerminalCheckpointError::ResourceAllocation("unicode_version_stack")
            })?;
        for entry in unicode_version_stack {
            restored_unicode_stack.push(UnicodeVersionStackEntry {
                vers: entry.version.into_live(custom_cell_width_maps)?,
                label: entry.label,
            });
        }

        // Complete every fallible semantic conversion before constructing the
        // terminal.  Restore must not allocate a default state and then replace
        // its collections: the validated checkpoint is the sole state source.
        let restored_pen = pen.into_live()?;
        let restored_cursor = cursor.into_live()?;
        let restored_left_margin = usize_from_u64(left_margin, "left_margin")?;
        let restored_right_margin = usize_from_u64(right_margin, "right_margin")?;
        let restored_last_mouse_move = last_mouse_move
            .map(CheckpointMouseEvent::into_live)
            .transpose()?;
        let restored_keyboard_encoding = keyboard_encoding.into_live()?;
        let restored_tabs = TabStop {
            tabs: tab_stops,
            tab_width: usize_from_u64(tab_width, "tab_width")?,
        };
        let restored_palette = palette.map(CheckpointColorPalette::into_live).transpose()?;
        let restored_current_dir = current_dir
            .map(|directory| {
                Url::parse(&directory).map_err(|_| TerminalCheckpointError::InvalidCurrentDirectory)
            })
            .transpose()?;
        let restored_unicode_version = unicode_version.into_live(custom_cell_width_maps)?;
        let restored_bidi_hint = bidi_hint.map(CheckpointBidiHint::into_live);
        let restored_lost_focus_seqno =
            usize_from_u64(lost_focus_seqno, "lost_focus_seqno")?;
        let restored_lost_focus_alerted_seqno =
            usize_from_u64(lost_focus_alerted_seqno, "lost_focus_alerted_seqno")?;
        let restored_kitty = KittyImageState::quiescent_for_checkpoint_restore(
            config.kitty_image_budget_bytes(),
            config.kitty_image_max_transmission_bytes(),
            kitty_max_image_id,
        );
        let mut restored_image_cache = lru::LruCache::unbounded();
        restored_image_cache.resize(NonZeroUsize::new(16).expect("nonzero cache capacity"));

        Ok(TerminalState {
            config,
            screen: screens,
            pen: restored_pen,
            cursor: restored_cursor,
            wrap_next,
            clear_semantic_attribute_on_newline,
            last_semantic_command_status,
            insert,
            dec_auto_wrap,
            saved_dec_private_modes: restored_saved_modes,
            reverse_wraparound_mode,
            reverse_video_mode,
            synchronized_output,
            dec_origin_mode,
            top_and_bottom_margins: top_margin..bottom_margin,
            left_and_right_margins: restored_left_margin..restored_right_margin,
            left_and_right_margin_mode,
            application_cursor_keys,
            modify_other_keys,
            dec_ansi_mode,
            sixel_display_mode,
            use_private_color_registers_for_each_graphic,
            color_map: restored_color_map,
            application_keypad,
            bracketed_paste,
            any_event_mouse,
            focus_tracking,
            mouse_encoding: mouse_encoding.into_live(),
            mouse_tracking,
            button_event_mouse,
            current_mouse_buttons: restored_buttons,
            last_mouse_move: restored_last_mouse_move,
            cursor_visible,
            keyboard_encoding: restored_keyboard_encoding,
            g0_charset: g0_charset.into_live(),
            g1_charset: g1_charset.into_live(),
            shift_out,
            newline_mode,
            tabs: restored_tabs,
            title,
            icon_title,
            progress,
            palette: restored_palette,
            pixel_width: size.pixel_width,
            pixel_height: size.pixel_height,
            dpi,
            clipboard: None,
            device_control_handler: None,
            alert_handler: None,
            download_handler: None,
            current_dir: restored_current_dir,
            term_program,
            term_version,
            writer: BufWriter::with_capacity(0, ThreadedWriter::inert()),
            writer_is_inert: true,
            image_cache: restored_image_cache,
            sixel_scrolls_right,
            user_vars: restored_user_vars,
            kitty_img: restored_kitty,
            seqno,
            unicode_version: restored_unicode_version,
            unicode_version_stack: restored_unicode_stack,
            enable_conpty_quirks,
            suppress_initial_title_change,
            accumulating_title,
            lost_focus_seqno: restored_lost_focus_seqno,
            lost_focus_alerted_seqno: restored_lost_focus_alerted_seqno,
            focused,
            bidi_enabled,
            bidi_hint: restored_bidi_hint,
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum TerminalCheckpointError {
    UnsupportedGraphicsState,
    UnsupportedVersion {
        observed: u32,
        supported: u32,
    },
    ResourceLimit {
        resource: &'static str,
        observed: usize,
        maximum: usize,
    },
    ArithmeticOverflow(&'static str),
    ResourceAllocation(&'static str),
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },
    ColdScrollbackMetadataInconsistent,
    ColdScrollbackSnapshot(crate::config::ScrollbackSpillError),
    ColdScrollbackNotRecoveryGrade,
    InvalidCurrentDirectory,
    InvalidRestoreConfiguration {
        reason: &'static str,
    },
    ConfigurationChangedDuringProjection,
    ReplayConfigurationMismatch,
    Serialization,
    NonCanonicalEncoding,
}

impl From<crate::screen::ScreenCheckpointCaptureError> for TerminalCheckpointError {
    fn from(value: crate::screen::ScreenCheckpointCaptureError) -> Self {
        match value {
            crate::screen::ScreenCheckpointCaptureError::ResourceLimit {
                resource,
                observed,
                maximum,
            } => Self::ResourceLimit {
                resource,
                observed,
                maximum,
            },
            crate::screen::ScreenCheckpointCaptureError::ArithmeticOverflow(resource) => {
                Self::ArithmeticOverflow(resource)
            }
            crate::screen::ScreenCheckpointCaptureError::ResourceAllocation(resource) => {
                Self::ResourceAllocation(resource)
            }
            crate::screen::ScreenCheckpointCaptureError::InvalidLineGeometry => {
                Self::InvalidField {
                    field: "screen.lines",
                    reason: "stored and semantic cell geometry differ",
                }
            }
            crate::screen::ScreenCheckpointCaptureError::UnsupportedGraphicsState => {
                Self::UnsupportedGraphicsState
            }
            crate::screen::ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent => {
                Self::ColdScrollbackMetadataInconsistent
            }
            crate::screen::ScreenCheckpointCaptureError::ColdScrollbackSnapshot(error) => {
                Self::ColdScrollbackSnapshot(error)
            }
            crate::screen::ScreenCheckpointCaptureError::ColdScrollbackNotRecoveryGrade => {
                Self::ColdScrollbackNotRecoveryGrade
            }
        }
    }
}

impl std::fmt::Display for TerminalCheckpointError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedGraphicsState => formatter.write_str(
                "terminal contains graphics state unsupported by checkpointing",
            ),
            Self::UnsupportedVersion {
                observed,
                supported,
            } => write!(
                formatter,
                "terminal checkpoint version {observed} is unsupported; expected {supported}"
            ),
            Self::ResourceLimit {
                resource,
                observed,
                maximum,
            } => write!(
                formatter,
                "terminal checkpoint {resource} exceeds its limit: {observed} > {maximum}"
            ),
            Self::ArithmeticOverflow(resource) => {
                write!(formatter, "terminal checkpoint {resource} accounting overflowed")
            }
            Self::ResourceAllocation(resource) => write!(
                formatter,
                "terminal checkpoint could not reserve memory for {resource}"
            ),
            Self::InvalidField { field, reason } => {
                write!(formatter, "terminal checkpoint field {field} is invalid: {reason}")
            }
            Self::ColdScrollbackMetadataInconsistent => formatter.write_str(
                "terminal checkpoint cold scrollback metadata changed or is inconsistent",
            ),
            Self::ColdScrollbackSnapshot(error) => {
                write!(formatter, "terminal checkpoint cold scrollback snapshot failed: {error}")
            }
            Self::ColdScrollbackNotRecoveryGrade => formatter.write_str(
                "terminal checkpoint cold scrollback is not exact semantic recovery data",
            ),
            Self::InvalidCurrentDirectory => {
                formatter.write_str("terminal checkpoint current directory is not a valid URL")
            }
            Self::InvalidRestoreConfiguration { reason } => {
                write!(formatter, "terminal checkpoint restore configuration is invalid: {reason}")
            }
            Self::ConfigurationChangedDuringProjection => formatter.write_str(
                "terminal configuration changed while its replay projection was captured",
            ),
            Self::ReplayConfigurationMismatch => formatter.write_str(
                "terminal checkpoint replay configuration does not match the intended live configuration",
            ),
            Self::Serialization => {
                formatter.write_str("terminal checkpoint serialization failed")
            }
            Self::NonCanonicalEncoding => formatter.write_str(
                "terminal checkpoint bytes are not the canonical current-schema representation",
            ),
        }
    }
}

impl std::error::Error for TerminalCheckpointError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Terminal;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Debug)]
    struct CheckpointTestConfig;

    impl TerminalConfiguration for CheckpointTestConfig {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
    }

    #[derive(Debug)]
    struct RichCheckpointTestConfig;

    impl TerminalConfiguration for RichCheckpointTestConfig {
        fn scrollback_size(&self) -> usize {
            128
        }

        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn enable_kitty_keyboard(&self) -> bool {
            true
        }

        fn max_user_vars(&self) -> usize {
            8
        }

        fn max_unicode_version_stack_depth(&self) -> usize {
            8
        }

        fn max_accumulating_title_len(&self) -> usize {
            256
        }

        fn unicode_version(&self) -> UnicodeVersion {
            UnicodeVersion {
                version: 14,
                ambiguous_are_wide: true,
                cell_widths: Some(Arc::new(HashMap::from([(u32::from('x'), 2)]))),
            }
        }

        fn normalize_output_to_unicode_nfc(&self) -> bool {
            true
        }

        fn bidi_mode(&self) -> BidiMode {
            BidiMode {
                enabled: true,
                hint: ParagraphDirectionHint::AutoRightToLeft,
            }
        }
    }

    #[derive(Debug, Default)]
    struct MalformedReplacementReceiptSink {
        replace_calls: AtomicUsize,
        published_rows: AtomicUsize,
    }

    impl crate::config::ScrollbackSpillSink for MalformedReplacementReceiptSink {
        fn store_scrollback_line(
            &self,
            _stable_row: StableRowIndex,
            _line: &Line,
            _max_retained_rows: usize,
        ) -> bool {
            false
        }

        fn load_scrollback_line(&self, _stable_row: StableRowIndex) -> Option<Line> {
            None
        }

        fn oldest_scrollback_row(&self) -> Option<StableRowIndex> {
            None
        }

        fn retained_scrollback_rows(&self) -> usize {
            self.published_rows.load(Ordering::Relaxed)
        }

        fn retained_scrollback_bytes(&self) -> usize {
            0
        }

        fn snapshot_scrollback(
            &self,
            _expected_newest_exclusive: StableRowIndex,
            _limits: crate::config::ScrollbackSnapshotLimits,
        ) -> Result<
            crate::config::ScrollbackSnapshot,
            crate::config::ScrollbackSpillError,
        > {
            Err(crate::config::ScrollbackSpillError::StorageUnavailable)
        }

        fn replace_scrollback_prefix(
            &self,
            expected_generation: Option<crate::config::ScrollbackSnapshotGeneration>,
            prefix: crate::config::ScrollbackPrefix<'_>,
            _max_retained_rows: usize,
        ) -> Result<
            crate::config::ScrollbackReplaceCommit,
            crate::config::ScrollbackSpillError,
        > {
            if expected_generation.is_some() {
                return Err(crate::config::ScrollbackSpillError::SnapshotGenerationMismatch);
            }
            self.replace_calls.fetch_add(1, Ordering::Relaxed);
            self.published_rows
                .store(prefix.row_count(), Ordering::Relaxed);

            // Model a sink that reached its durable publication point but
            // returned a malformed, non-advanced generation receipt.
            Ok(crate::config::ScrollbackReplaceCommit::new(
                crate::config::ScrollbackSnapshotGeneration::new([9; 16], 0),
                prefix.oldest_stable_row(),
                prefix.newest_stable_row_exclusive(),
            ))
        }

        fn clear_scrollback(
            &self,
        ) -> Result<
            crate::config::ScrollbackClearCommit,
            crate::config::ScrollbackSpillError,
        > {
            self.published_rows.store(0, Ordering::Relaxed);
            Ok(crate::config::ScrollbackClearCommit::new(
                crate::config::ScrollbackSnapshotGeneration::new([9; 16], 1),
            ))
        }
    }

    #[derive(Debug)]
    struct ReplacementActivationConfig {
        tier_enabled: bool,
        sink: Arc<MalformedReplacementReceiptSink>,
    }

    impl TerminalConfiguration for ReplacementActivationConfig {
        fn scrollback_size(&self) -> usize {
            128
        }

        fn scrollback_tier_config(&self) -> crate::config::ScrollbackTierConfig {
            crate::config::ScrollbackTierConfig {
                enabled: self.tier_enabled,
                hot_lines: if self.tier_enabled { 1 } else { 128 },
                warm_max_bytes: 0,
            }
        }

        fn scrollback_spill_sink(
            &self,
        ) -> Option<Arc<dyn crate::config::ScrollbackSpillSink>> {
            if self.tier_enabled {
                Some(self.sink.clone())
            } else {
                None
            }
        }

        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
    }

    #[derive(Debug)]
    struct LoweredCheckpointTestConfig;

    impl TerminalConfiguration for LoweredCheckpointTestConfig {
        fn scrollback_size(&self) -> usize {
            0
        }

        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn max_user_vars(&self) -> usize {
            0
        }

        fn max_unicode_version_stack_depth(&self) -> usize {
            0
        }

        fn max_accumulating_title_len(&self) -> usize {
            0
        }

        fn max_color_map_entries(&self) -> usize {
            0
        }
    }

    fn terminal() -> Terminal {
        Terminal::new(
            TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            },
            Arc::new(CheckpointTestConfig),
            "FrankenTerm",
            "checkpoint-test",
            Box::new(Vec::<u8>::new()),
        )
    }

    fn terminal_with_distinct_custom_width_maps(reverse_insertion: bool) -> Terminal {
        let mut terminal = Terminal::new(
            TerminalSize::default(),
            Arc::new(RichCheckpointTestConfig),
            "FrankenTerm",
            "checkpoint-width-table-test",
            Box::new(Vec::<u8>::new()),
        );
        let ordered_entries = if reverse_insertion {
            [(u32::from('z'), 2), (u32::from('a'), 1)]
        } else {
            [(u32::from('a'), 1), (u32::from('z'), 2)]
        };
        let mut current_widths = HashMap::new();
        for entry in ordered_entries {
            current_widths.insert(entry.0, entry.1);
        }
        terminal.unicode_version = UnicodeVersion {
            version: 15,
            ambiguous_are_wide: false,
            cell_widths: Some(Arc::new(current_widths)),
        };

        let mut saved_widths = HashMap::new();
        for entry in ordered_entries.into_iter().rev() {
            saved_widths.insert(entry.0, entry.1);
        }
        let saved_widths = Arc::new(saved_widths);
        assert!(!Arc::ptr_eq(
            terminal
                .unicode_version
                .cell_widths
                .as_ref()
                .expect("current width map"),
            &saved_widths,
        ));
        terminal.unicode_version_stack.push(UnicodeVersionStackEntry {
            vers: UnicodeVersion {
                version: 9,
                ambiguous_are_wide: false,
                cell_widths: Some(saved_widths),
            },
            label: Some("saved-width-map".into()),
        });
        terminal
    }

    #[test]
    fn semantic_projection_roundtrips_and_tracks_both_screens() {
        let mut terminal = terminal();
        terminal.advance_bytes(b"primary\x1b]2;checkpoint-title\x07");
        terminal.advance_bytes(b"\x1b[?1049h\x1b[31malternate");
        let checkpoint = TerminalCheckpointV2::capture(&terminal).expect("capture terminal");
        let encoded = serde_json::to_vec(&checkpoint).expect("serialize checkpoint");
        let decoded: TerminalCheckpointV2 =
            serde_json::from_slice(&encoded).expect("deserialize checkpoint");

        assert_eq!(decoded, checkpoint);
        assert!(checkpoint.alternate_screen_active);
        assert_eq!(checkpoint.title, "checkpoint-title");
        assert_ne!(checkpoint.primary_screen.lines, checkpoint.alternate_screen.lines);
    }

    #[test]
    fn unsupported_out_of_band_graphics_fail_closed() {
        let mut terminal = terminal();
        terminal.kitty_img.mark_nonempty_for_checkpoint_test();

        assert_eq!(
            TerminalCheckpointV2::capture(&terminal),
            Err(TerminalCheckpointError::UnsupportedGraphicsState)
        );
    }

    #[test]
    fn canonical_projection_sorts_terminal_maps() {
        let mut first = terminal();
        first.user_vars.insert("zeta".into(), "last".into());
        first.user_vars.insert("alpha".into(), "first".into());
        let mut second = terminal();
        second.user_vars.insert("alpha".into(), "first".into());
        second.user_vars.insert("zeta".into(), "last".into());

        let first = TerminalCheckpointV2::capture(&first).expect("capture first terminal");
        let second = TerminalCheckpointV2::capture(&second).expect("capture second terminal");
        assert_eq!(
            serde_json::to_vec(&first).expect("serialize first terminal"),
            serde_json::to_vec(&second).expect("serialize second terminal")
        );
    }

    #[test]
    fn canonical_custom_width_table_deduplicates_shared_semantics_and_sorts_content() {
        let limits = TerminalCheckpointLimits::default();
        let first = TerminalCheckpointV2::capture_with_limits(
            &terminal_with_distinct_custom_width_maps(false),
            limits,
        )
        .expect("capture first custom-width terminal");
        let second = TerminalCheckpointV2::capture_with_limits(
            &terminal_with_distinct_custom_width_maps(true),
            limits,
        )
        .expect("capture reverse-insertion custom-width terminal");

        assert_eq!(first.custom_cell_width_maps.len(), 2);
        assert!(
            first
                .custom_cell_width_maps
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        assert_ne!(
            first.replay_config.unicode_version.custom_cell_width_map,
            first.unicode_version.custom_cell_width_map,
        );
        assert_eq!(
            first.unicode_version_stack[0]
                .version
                .custom_cell_width_map,
            first.unicode_version.custom_cell_width_map,
        );
        assert_eq!(
            first.to_canonical_json(limits).expect("encode first table"),
            second
                .to_canonical_json(limits)
                .expect("encode reverse-insertion table"),
        );
    }

    #[test]
    fn custom_width_table_rejects_out_of_bounds_reference() {
        let limits = TerminalCheckpointLimits::default();
        let mut checkpoint = TerminalCheckpointV2::capture_with_limits(
            &terminal_with_distinct_custom_width_maps(false),
            limits,
        )
        .expect("capture custom-width terminal");
        checkpoint.unicode_version.custom_cell_width_map =
            Some(u32::try_from(checkpoint.custom_cell_width_maps.len()).unwrap());

        assert!(matches!(
            checkpoint.validate(limits),
            Err(TerminalCheckpointError::InvalidField {
                field: "unicode_version.custom_cell_width_map",
                ..
            })
        ));
    }

    #[test]
    fn custom_width_table_rejects_unreferenced_rows() {
        let limits = TerminalCheckpointLimits::default();
        let mut checkpoint = TerminalCheckpointV2::capture_with_limits(
            &terminal_with_distinct_custom_width_maps(false),
            limits,
        )
        .expect("capture custom-width terminal");
        checkpoint.replay_config.unicode_version.custom_cell_width_map =
            checkpoint.unicode_version.custom_cell_width_map;

        assert!(matches!(
            checkpoint.validate(limits),
            Err(TerminalCheckpointError::InvalidField {
                field: "custom_cell_width_maps",
                reason: "every custom-width map must be semantically referenced",
            })
        ));
    }

    #[test]
    fn custom_width_table_rejects_noncanonical_or_empty_maps() {
        let limits = TerminalCheckpointLimits::default();
        let checkpoint = TerminalCheckpointV2::capture_with_limits(
            &terminal_with_distinct_custom_width_maps(false),
            limits,
        )
        .expect("capture custom-width terminal");

        let mut unsorted = checkpoint.clone();
        let multi_entry = unsorted
            .custom_cell_width_maps
            .iter_mut()
            .find(|table| table.entries.len() > 1)
            .expect("multi-entry width map");
        multi_entry.entries.swap(0, 1);
        assert!(matches!(
            unsorted.validate(limits),
            Err(TerminalCheckpointError::InvalidField { .. })
        ));

        let mut duplicate = checkpoint.clone();
        let duplicate_row = duplicate.custom_cell_width_maps[0].clone();
        duplicate.custom_cell_width_maps[1] = duplicate_row;
        assert!(matches!(
            duplicate.validate(limits),
            Err(TerminalCheckpointError::InvalidField {
                field: "custom_cell_width_maps",
                reason: "custom-width maps must be strictly lexicographically ordered",
            })
        ));

        let mut empty = checkpoint;
        empty.custom_cell_width_maps[0].entries.clear();
        assert!(matches!(
            empty.validate(limits),
            Err(TerminalCheckpointError::InvalidField {
                field: "custom_cell_width_maps",
                ..
            })
        ));
    }

    #[test]
    fn custom_width_table_rejects_stack_map_mismatch() {
        let limits = TerminalCheckpointLimits::default();
        let mut checkpoint = TerminalCheckpointV2::capture_with_limits(
            &terminal_with_distinct_custom_width_maps(false),
            limits,
        )
        .expect("capture custom-width terminal");
        checkpoint.unicode_version_stack[0]
            .version
            .custom_cell_width_map = checkpoint
            .replay_config
            .unicode_version
            .custom_cell_width_map;

        assert!(matches!(
            checkpoint.validate(limits),
            Err(TerminalCheckpointError::InvalidField {
                field: "unicode_version_stack",
                ..
            })
        ));
    }

    #[test]
    fn checkpoint_rejects_unbound_or_alternate_cold_prefix_metadata() {
        let limits = TerminalCheckpointLimits::default();
        let checkpoint = TerminalCheckpointV2::capture_with_limits(&terminal(), limits)
            .expect("capture cold-prefix validation fixture");

        let mut unbound = checkpoint.clone();
        let inserted_line = unbound.primary_screen.lines[0].clone();
        unbound.primary_screen.lines.insert(0, inserted_line);
        unbound.primary_screen.cold_prefix_line_count = 1;
        assert!(matches!(
            unbound.validate(limits),
            Err(TerminalCheckpointError::InvalidField {
                field: "screen.cold_snapshot_generation",
                ..
            })
        ));

        let mut alternate = checkpoint;
        alternate.alternate_screen.cold_snapshot_generation =
            Some(CheckpointScrollbackGeneration {
                content_epoch: [7; 16],
                revision: 9,
            });
        assert!(matches!(
            alternate.validate(limits),
            Err(TerminalCheckpointError::InvalidField {
                field: "alternate_screen",
                ..
            })
        ));
    }

    #[test]
    fn canonical_restore_roundtrips_complete_semantic_projection() {
        let config: Arc<dyn TerminalConfiguration + Send + Sync> = Arc::new(RichCheckpointTestConfig);
        let mut terminal = Terminal::new(
            TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            },
            Arc::clone(&config),
            "FrankenTerm",
            "checkpoint-rich-test",
            Box::new(Vec::<u8>::new()),
        );
        terminal.advance_bytes(
            b"primary-x\x1b]0;checkpoint-rich-title\x07\x1b]8;id=roundtrip;https://example.invalid/checkpoint\x07link\x1b]8;;\x07\x1b7",
        );
        terminal.resize(TerminalSize {
            rows: 30,
            cols: 100,
            pixel_width: 1_000,
            pixel_height: 600,
            dpi: 110,
        });
        terminal.advance_bytes(b"\x1b[?1049h\x1b[31;4malternate-x\x1b7");
        terminal.user_vars.insert("zeta".into(), "last".into());
        terminal.user_vars.insert("alpha".into(), "first".into());
        terminal.current_dir = Some(Url::parse("file:///tmp/checkpoint-rich").unwrap());
        terminal.progress = Progress::Percentage(67);
        terminal.palette = Some(ColorPalette::default());
        let mut prior_unicode_version = terminal.unicode_version.clone();
        prior_unicode_version.version = 9;
        terminal.unicode_version_stack.push(UnicodeVersionStackEntry {
            vers: prior_unicode_version,
            label: Some("prior-unicode".into()),
        });
        terminal.accumulating_title = Some("pending-title-fragment".into());

        let limits = TerminalCheckpointLimits::default();
        let before = TerminalCheckpointV2::capture_with_limits(&terminal, limits)
            .expect("capture rich terminal");
        let canonical = before
            .to_canonical_json(limits)
            .expect("encode canonical rich terminal");
        let validated = TerminalCheckpointV2::decode_canonical_json(&canonical, limits)
            .expect("decode canonical rich terminal");
        let inert = validated
            .restore_inert(Arc::new(RichCheckpointTestConfig))
            .expect("restore rich terminal off topology");
        let after = inert.checkpoint().expect("recapture restored terminal");

        assert_eq!(
            after
                .to_canonical_json(limits)
                .expect("encode restored rich terminal"),
            canonical,
        );
    }

    #[test]
    fn writer_preparation_failure_returns_the_complete_retryable_inert_model() {
        let limits = TerminalCheckpointLimits::default();
        let config: Arc<dyn TerminalConfiguration + Send + Sync> = Arc::new(RichCheckpointTestConfig);
        let mut live = Terminal::new(
            TerminalSize::default(),
            Arc::clone(&config),
            "FrankenTerm",
            "checkpoint-writer-preparation-test",
            Box::new(Vec::<u8>::new()),
        );
        for index in 0..48 {
            live.advance_bytes(format!("retained-{index}\r\n").as_bytes());
        }
        let checkpoint = TerminalCheckpointV2::capture_with_limits(&live, limits)
            .expect("capture writer-preparation fixture");
        let canonical = checkpoint
            .to_canonical_json(limits)
            .expect("encode writer-preparation fixture");
        let mut inert = TerminalCheckpointV2::decode_canonical_json(&canonical, limits)
            .expect("decode writer-preparation fixture")
            .restore_inert(config)
            .expect("restore writer-preparation fixture");
        inert.force_writer_preparation_failure_for_test();

        let failure = match inert.into_live(Box::new(Vec::<u8>::new())) {
            Ok(_) => panic!("forced writer preparation must fail"),
            Err(failure) => failure,
        };
        assert_eq!(failure.error(), &crate::InertTerminalError::WriterActivation);
        let (_error, recovered) = failure.into_parts();
        assert_eq!(
            recovered
                .checkpoint()
                .expect("recapture retryable inert model")
                .to_canonical_json(limits)
                .expect("encode retryable inert model"),
            canonical,
        );
    }

    #[test]
    fn malformed_published_receipt_poisons_activation_without_writer_swap_or_retry() {
        let limits = TerminalCheckpointLimits::default();
        let sink = Arc::new(MalformedReplacementReceiptSink::default());
        let capture_config: Arc<dyn TerminalConfiguration + Send + Sync> =
            Arc::new(ReplacementActivationConfig {
                tier_enabled: false,
                sink: Arc::clone(&sink),
            });
        let mut live = Terminal::new(
            TerminalSize::default(),
            capture_config,
            "FrankenTerm",
            "checkpoint-malformed-receipt-test",
            Box::new(Vec::<u8>::new()),
        );
        for index in 0..48 {
            live.advance_bytes(format!("retained-{index}\r\n").as_bytes());
        }
        let checkpoint = TerminalCheckpointV2::capture_with_limits(&live, limits)
            .expect("capture resident-only recovery fixture");
        let canonical = checkpoint
            .to_canonical_json(limits)
            .expect("encode resident-only recovery fixture");
        let activation_config: Arc<dyn TerminalConfiguration> =
            Arc::new(ReplacementActivationConfig {
                tier_enabled: true,
                sink: Arc::clone(&sink),
            });
        let inert = TerminalCheckpointV2::decode_canonical_json(&canonical, limits)
            .expect("validate resident-only recovery fixture")
            .restore_inert(activation_config)
            .expect("restore retiering fixture off topology");

        let first_failure = match inert.into_live(Box::new(Vec::<u8>::new())) {
            Ok(_) => panic!("malformed replacement receipt must not activate a terminal"),
            Err(failure) => failure,
        };
        assert_eq!(
            first_failure.error(),
            &crate::InertTerminalError::ScrollbackActivation(
                crate::config::ScrollbackActivationError::CommitOutcomeIndeterminate,
            ),
        );
        assert_eq!(sink.replace_calls.load(Ordering::Relaxed), 1);
        assert!(sink.published_rows.load(Ordering::Relaxed) > 0);
        let (_error, mut recovered) = first_failure.into_parts();
        assert!(recovered.writer_is_inert_for_test());
        assert_eq!(
            recovered.checkpoint(),
            Err(crate::InertTerminalError::ActivationPoisoned),
        );
        assert_eq!(
            recovered.replay_bytes(b"must-not-cross-quarantine"),
            Err(crate::InertTerminalError::ActivationPoisoned),
        );

        let retry_failure = match recovered.into_live(Box::new(Vec::<u8>::new())) {
            Ok(_) => panic!("poisoned activation must not install a live writer"),
            Err(failure) => failure,
        };
        assert_eq!(
            retry_failure.error(),
            &crate::InertTerminalError::ActivationPoisoned,
        );
        assert_eq!(sink.replace_calls.load(Ordering::Relaxed), 1);
        let (_error, still_inert) = retry_failure.into_parts();
        assert!(still_inert.writer_is_inert_for_test());
    }

    #[test]
    fn canonical_decoder_rejects_unknown_omitted_and_extra_fields() {
        let limits = TerminalCheckpointLimits::default();
        let checkpoint = TerminalCheckpointV2::capture_with_limits(&terminal(), limits)
            .expect("capture fixture");
        let canonical = checkpoint
            .to_canonical_json(limits)
            .expect("encode fixture");

        let mut unknown_version = canonical.clone();
        let version = b"{\"version\":2";
        assert!(unknown_version.starts_with(version));
        unknown_version[version.len() - 1] = b'3';
        assert!(matches!(
            TerminalCheckpointV2::decode_canonical_json(&unknown_version, limits),
            Err(TerminalCheckpointError::UnsupportedVersion { observed: 3, .. })
        ));

        let mut value: serde_json::Value =
            serde_json::from_slice(&canonical).expect("parse fixture JSON");
        value
            .as_object_mut()
            .expect("checkpoint is an object")
            .remove("title");
        let omitted = serde_json::to_vec(&value).expect("encode omitted-field fixture");
        assert!(matches!(
            TerminalCheckpointV2::decode_canonical_json(&omitted, limits),
            Err(TerminalCheckpointError::Serialization)
        ));

        let mut extra_value: serde_json::Value =
            serde_json::from_slice(&canonical).expect("reparse fixture JSON");
        extra_value
            .as_object_mut()
            .expect("checkpoint is an object")
            .insert("unexpected".into(), serde_json::Value::Bool(true));
        let extra = serde_json::to_vec(&extra_value).expect("encode extra-field fixture");
        assert!(matches!(
            TerminalCheckpointV2::decode_canonical_json(&extra, limits),
            Err(TerminalCheckpointError::Serialization)
        ));
    }

    #[test]
    fn capture_rejects_resource_policy_below_live_geometry() {
        let limits = TerminalCheckpointLimits {
            max_rows: 1,
            ..TerminalCheckpointLimits::default()
        };

        assert!(matches!(
            TerminalCheckpointV2::capture_with_limits(&terminal(), limits),
            Err(TerminalCheckpointError::ResourceLimit {
                resource: "physical_rows",
                ..
            })
        ));
    }

    #[test]
    fn lowered_dynamic_limits_do_not_erase_reachable_checkpoint_state() {
        let rich_config: Arc<dyn TerminalConfiguration> = Arc::new(RichCheckpointTestConfig);
        let mut terminal = Terminal::new(
            TerminalSize::default(),
            rich_config,
            "FrankenTerm",
            "checkpoint-lowered-config-test",
            Box::new(Vec::<u8>::new()),
        );
        for index in 0..40 {
            terminal.advance_bytes(format!("retained-{index}\r\n").as_bytes());
        }
        terminal.user_vars.insert("retained".into(), "value".into());
        let mut prior_unicode_version = terminal.unicode_version.clone();
        prior_unicode_version.version = 9;
        terminal.unicode_version_stack.push(UnicodeVersionStackEntry {
            vers: prior_unicode_version,
            label: Some("retained".into()),
        });
        terminal.accumulating_title = Some("retained-title".into());
        let lowered: Arc<dyn TerminalConfiguration> = Arc::new(LoweredCheckpointTestConfig);
        terminal.set_config(Arc::clone(&lowered));

        let limits = TerminalCheckpointLimits::default();
        let checkpoint = TerminalCheckpointV2::capture_with_limits(&terminal, limits)
            .expect("capture state retained across a config-limit decrease");
        let canonical = checkpoint
            .to_canonical_json(limits)
            .expect("encode lowered-config checkpoint");
        let inert = TerminalCheckpointV2::decode_canonical_json(&canonical, limits)
            .expect("validate lowered-config checkpoint")
            .restore_inert(lowered)
            .expect("restore state retained across a config-limit decrease");

        assert_eq!(
            inert
                .checkpoint()
                .expect("recapture lowered-config terminal")
                .to_canonical_json(limits)
                .expect("encode restored lowered-config terminal"),
            canonical,
        );
    }
}
