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
pub const TERMINAL_CHECKPOINT_VERSION: u32 = 3;

/// The schema whose lines are runs of cells sharing attributes from a
/// per-screen table (`TerminalCheckpointLineEncoding::Runs`). Everything
/// else is version 3's; every decoder of this release admits both.
pub const COMPACT_TERMINAL_CHECKPOINT_VERSION: u32 = 4;

/// How a capture writes screen lines.
///
/// Version 3 stores one object per cell, about 300 bytes each with its
/// attributes spelled out; a pane with a few thousand rows of history made a
/// payload of tens of megabytes that the guardian stages, hashes, validates
/// and stores on every capture. Version 4 stores a line as runs: one string of
/// text per run of cells sharing attributes, the attributes once per screen.
/// A reader older than this release refuses version 4, so a writer chooses
/// it only for a reader known to admit it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalCheckpointLineEncoding {
    /// Version 3: every release reads it.
    #[default]
    Cells,
    /// Version 4: runs and a per-screen attribute table.
    Runs,
}

impl TerminalCheckpointLineEncoding {
    /// The schema version a capture in this encoding carries.
    #[must_use]
    pub const fn version(self) -> u32 {
        match self {
            Self::Cells => TERMINAL_CHECKPOINT_VERSION,
            Self::Runs => COMPACT_TERMINAL_CHECKPOINT_VERSION,
        }
    }

    /// The encoding of a schema version this release decodes.
    #[must_use]
    pub const fn of_version(version: u32) -> Option<Self> {
        match version {
            TERMINAL_CHECKPOINT_VERSION => Some(Self::Cells),
            COMPACT_TERMINAL_CHECKPOINT_VERSION => Some(Self::Runs),
            _ => None,
        }
    }
}

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
    /// The ceiling a failed pass exceeded, as (resource, observed, maximum),
    /// so a refusal reports a typed resource limit rather than a parse error.
    exceeded: Option<(&'static str, usize, usize)>,
}

impl JsonStructuralBudget {
    const MAX_DEPTH: usize = 128;
    const BYTES_PER_NODE: usize = 64;

    fn new(limits: TerminalCheckpointLimits) -> Self {
        Self {
            retained_bytes: 0,
            limits,
            exceeded: None,
        }
    }

    fn charge(&mut self, bytes: usize) -> Result<(), &'static str> {
        self.retained_bytes = self
            .retained_bytes
            .checked_add(bytes)
            .ok_or("checkpoint structural accounting overflow")?;
        if self.retained_bytes > self.limits.max_retained_capture_bytes {
            self.exceeded = Some((
                "json_structural_bytes",
                self.retained_bytes,
                self.limits.max_retained_capture_bytes,
            ));
            return Err("checkpoint structural memory limit");
        }
        Ok(())
    }

    fn charge_node(&mut self) -> Result<(), &'static str> {
        self.charge(Self::BYTES_PER_NODE)
    }

    fn charge_string(&mut self, value: &str) -> Result<(), &'static str> {
        if value.len() > self.limits.max_string_bytes {
            self.exceeded = Some((
                "json_string_bytes",
                value.len(),
                self.limits.max_string_bytes,
            ));
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

    fn child_depth<E: serde::de::Error>(&mut self) -> Result<usize, E> {
        let depth = self
            .depth
            .checked_add(1)
            .ok_or_else(|| E::custom("checkpoint nesting overflow"))?;
        if depth > JsonStructuralBudget::MAX_DEPTH {
            self.budget.exceeded = Some(("json_depth", depth, JsonStructuralBudget::MAX_DEPTH));
            return Err(E::custom("checkpoint nesting limit"));
        }
        Ok(depth)
    }
}

/// Run the decoder's allocation-light structural pass over canonical JSON and
/// return the memory it charges. `to_canonical_json` runs the same pass, so a
/// checkpoint its own decoder would refuse is never emitted.
fn json_structural_retained_bytes(
    bytes: &[u8],
    limits: TerminalCheckpointLimits,
) -> Result<usize, TerminalCheckpointError> {
    let mut budget = JsonStructuralBudget::new(limits);
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let pass = JsonStructuralSeed {
        budget: &mut budget,
        depth: 0,
    }
    .deserialize(&mut deserializer)
    .and_then(|()| deserializer.end());
    match (pass, budget.exceeded) {
        (Ok(()), _) => Ok(budget.retained_bytes),
        (Err(_), Some((resource, observed, maximum))) => {
            Err(resource_limit(resource, observed, maximum))
        }
        (Err(_), None) => Err(TerminalCheckpointError::Serialization),
    }
}

/// The structural charge and byte length of `value`'s compact JSON. The
/// plaintext encoding is zeroized as soon as it has been measured.
fn measure_canonical_json<T: serde::Serialize + ?Sized>(
    value: &T,
    limits: TerminalCheckpointLimits,
) -> Result<(usize, usize), TerminalCheckpointError> {
    let encoded = Zeroizing::new(
        serde_json::to_vec(value).map_err(|_| TerminalCheckpointError::Serialization)?,
    );
    let structural = json_structural_retained_bytes(&encoded, limits)?;
    Ok((structural, encoded.len()))
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
pub struct ValidatedTerminalCheckpointV3 {
    checkpoint: TerminalCheckpointV3,
    limits: TerminalCheckpointLimits,
}

impl std::fmt::Debug for ValidatedTerminalCheckpointV3 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("ValidatedTerminalCheckpointV3")
            .field(&self.checkpoint)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum CheckpointColorAttribute {
    Default,
    PaletteIndex { index: u8 },
    TrueColorWithDefaultFallback { color: CheckpointSrgba },
    TrueColorWithPaletteFallback { color: CheckpointSrgba, index: u8 },
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
        RgbColor::new_8bpc((self.0 >> 16) as u8, (self.0 >> 8) as u8, self.0 as u8)
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
            Self::Kitty(bits) => {
                KeyboardEncoding::Kitty(KittyKeyboardFlags::from_bits(bits).ok_or(
                    TerminalCheckpointError::InvalidField {
                        field: "keyboard_encoding",
                        reason: "unknown Kitty keyboard flag bits",
                    },
                )?)
            }
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

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
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

// Keys a capture's attribute table. The parser's enums have no Hash; their
// discriminants hash what their Eq compares.
impl std::hash::Hash for CheckpointCellAttributes {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::mem::discriminant(&self.intensity).hash(state);
        self.underline.hash(state);
        std::mem::discriminant(&self.blink).hash(state);
        (
            self.italic,
            self.reverse,
            self.strikethrough,
            self.invisible,
            self.wrapped,
            self.overline,
        )
            .hash(state);
        std::mem::discriminant(&self.semantic_type).hash(state);
        std::mem::discriminant(&self.vertical_align).hash(state);
        self.foreground.hash(state);
        self.background.hash(state);
        self.underline_color.hash(state);
        self.hyperlink.hash(state);
    }
}

impl CheckpointCellAttributes {
    fn capture(value: &CellAttributes) -> Result<Self, TerminalCheckpointError> {
        if value.has_image_attachments() {
            return Err(TerminalCheckpointError::UnsupportedGraphicsState);
        }
        // A protected cell or pen would come back unprotected.
        if value.protected() {
            return Err(TerminalCheckpointError::UnsupportedCharacterProtection);
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

/// Version 4's cells: consecutive cells of one line sharing one entry of
/// their screen's attribute table. Without `clusters`, each char of `text` is
/// a cell one column wide. Otherwise `clusters` gives every cell as (chars,
/// columns); it is present only when some cell is not one char one column
/// wide, so each run has one encoding.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointRun {
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    clusters: Option<Vec<(u32, u8)>>,
    attributes: u32,
}

/// A capture closes a run at this much text, well under the decoder's
/// per-string limit however long the line or its clusters.
const CHECKPOINT_RUN_TEXT_BYTES: usize = 32 * 1024;

impl CheckpointRun {
    fn push_cell(&mut self, text: &str, width: u8) -> Result<(), TerminalCheckpointError> {
        let chars = if text.len() == 1 {
            1
        } else {
            text.chars().count()
        };
        if self.clusters.is_none() && (chars != 1 || width != 1) {
            // Every earlier cell was one char one column wide.
            let mut clusters = Vec::new();
            let earlier = self.text.chars().count();
            clusters
                .try_reserve(earlier + 1)
                .map_err(|_| TerminalCheckpointError::ResourceAllocation("run.clusters"))?;
            clusters.resize(earlier, (1, 1));
            self.clusters = Some(clusters);
        }
        if let Some(clusters) = self.clusters.as_mut() {
            let chars = u32::try_from(chars)
                .map_err(|_| TerminalCheckpointError::ArithmeticOverflow("run.clusters"))?;
            clusters.push((chars, width));
        }
        self.text.push_str(text);
        Ok(())
    }

    /// Whether the run ends in a one-column blank (" ").
    fn ends_in_single_blank(&self) -> bool {
        self.text.ends_with(' ')
            && self
                .clusters
                .as_ref()
                .is_none_or(|clusters| clusters.last() == Some(&(1, 1)))
    }

    /// Drop the last cell, a one-column blank (see `ends_in_single_blank`).
    fn pop_single_blank(&mut self) {
        self.text.pop();
        if let Some(clusters) = self.clusters.as_mut() {
            clusters.pop();
            if clusters.iter().all(|cluster| *cluster == (1, 1)) {
                self.clusters = None;
            }
        }
    }

    /// The columns its cells cover.
    fn columns(&self) -> usize {
        match &self.clusters {
            None => self.text.chars().count(),
            Some(clusters) => clusters
                .iter()
                .map(|(_, width)| usize::from(*width))
                .sum(),
        }
    }

    /// The text of its `index`th cell, if it has that many.
    fn cell_text(&self, index: usize) -> Option<&str> {
        let (start, chars) = match &self.clusters {
            None => (index, 1),
            Some(clusters) => {
                let chars = clusters.get(index)?.0 as usize;
                let start = clusters[..index]
                    .iter()
                    .map(|(chars, _)| *chars as usize)
                    .sum();
                (start, chars)
            }
        };
        char_span(&self.text, start, chars)
    }

    /// How many cells it holds.
    fn cells(&self) -> usize {
        match &self.clusters {
            None => self.text.chars().count(),
            Some(clusters) => clusters.len(),
        }
    }
}

/// The `chars` chars of `text` that start after its first `start` chars,
/// if it has them all.
fn char_span(text: &str, start: usize, chars: usize) -> Option<&str> {
    let mut bounds = text
        .char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(text.len()));
    let begin = bounds.nth(start)?;
    let end = bounds.nth(chars.checked_sub(1)?)?;
    Some(&text[begin..end])
}

/// A capture's attribute table for one screen: each distinct attribute set
/// once, in the order the screen's lines first use it.
#[derive(Default)]
struct CheckpointAttributeTable {
    entries: Vec<CheckpointCellAttributes>,
    index: HashMap<CheckpointCellAttributes, u32>,
}

impl CheckpointAttributeTable {
    fn intern(
        &mut self,
        attributes: CheckpointCellAttributes,
    ) -> Result<u32, TerminalCheckpointError> {
        if let Some(index) = self.index.get(&attributes) {
            return Ok(*index);
        }
        let index = u32::try_from(self.entries.len())
            .map_err(|_| TerminalCheckpointError::ArithmeticOverflow("screen.attributes"))?;
        self.entries
            .try_reserve(1)
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("screen.attributes"))?;
        self.index
            .try_reserve(1)
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("screen.attributes"))?;
        self.entries.push(attributes.clone());
        self.index.insert(attributes, index);
        Ok(index)
    }
}

/// Exactly one of `cells` (version 3) and `runs` (version 4) is present;
/// version 3's bytes are what they were before version 4 existed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointLine {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cells: Option<Vec<CheckpointCell>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runs: Option<Vec<CheckpointRun>>,
    seqno: u64,
    scale: CheckpointLineScale,
    bidi_enabled: bool,
    bidi_hint: CheckpointBidiHint,
}

fn checkpoint_cell_width(width: usize) -> Result<u8, TerminalCheckpointError> {
    let width = u8::try_from(width).map_err(|_| TerminalCheckpointError::InvalidField {
        field: "cell.width",
        reason: "cell width does not fit the checkpoint wire type",
    })?;
    if !(1..=2).contains(&width) {
        return Err(TerminalCheckpointError::InvalidField {
            field: "cell.width",
            reason: "cell width must be one or two columns",
        });
    }
    Ok(width)
}

fn line_encoding_mismatch() -> TerminalCheckpointError {
    TerminalCheckpointError::InvalidField {
        field: "line.cells",
        reason: "a line's cells must be encoded as its checkpoint version encodes them",
    }
}

impl CheckpointLine {
    /// Capture `line` as cells (version 3) or, given its screen's attribute
    /// table, as runs (version 4).
    fn capture(
        line: &Line,
        table: Option<&mut CheckpointAttributeTable>,
    ) -> Result<Self, TerminalCheckpointError> {
        if line.has_image_attachments() {
            return Err(TerminalCheckpointError::UnsupportedGraphicsState);
        }
        let (cells, runs) = match table {
            None => (Some(Self::capture_cells(line)?), None),
            Some(table) => (None, Some(Self::capture_runs(line, table)?)),
        };
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
            runs,
            seqno: u64_from_usize(line.current_seqno(), "line.seqno")?,
            scale,
            bidi_enabled,
            bidi_hint: bidi_hint.into(),
        })
    }

    fn capture_cells(line: &Line) -> Result<Vec<CheckpointCell>, TerminalCheckpointError> {
        let mut cells = Vec::new();
        cells
            .try_reserve_exact(line.len())
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("line.cells"))?;
        for cell in line.visible_cells() {
            cells.push(CheckpointCell {
                text: cell.str().to_string(),
                width: checkpoint_cell_width(cell.width())?,
                attributes: CheckpointCellAttributes::capture(cell.attrs())?,
            });
        }
        // A row's trailing default blanks are what a line past its end shows
        // anyway (`Line::prune_trailing_blanks` drops them likewise); a
        // scroll creates a row with a full width of them. Stored, they made
        // every pane's checkpoint about 2 MB, whatever it showed, and the
        // guardian stages, scans, validates and acknowledges every byte. A
        // wrapped row's last cell carries the wrap and stays.
        if cells
            .last()
            .is_some_and(|cell| cell.width == 1 && cell.text == " ")
        {
            let blank = CheckpointCellAttributes::capture(&CellAttributes::blank())?;
            while cells
                .last()
                .is_some_and(|cell| cell.width == 1 && cell.text == " " && cell.attributes == blank)
            {
                cells.pop();
            }
        }
        Ok(cells)
    }

    /// The cells `capture_cells` keeps, as runs. A run ends where the
    /// attributes change; attributes that compare unequal live but capture
    /// equal continue it, so neighbouring runs name different entries unless
    /// the first closed at `CHECKPOINT_RUN_TEXT_BYTES`.
    fn capture_runs(
        line: &Line,
        table: &mut CheckpointAttributeTable,
    ) -> Result<Vec<CheckpointRun>, TerminalCheckpointError> {
        let mut runs: Vec<CheckpointRun> = Vec::new();
        let mut previous: Option<&CellAttributes> = None;
        let mut entry = 0u32;
        for cell in line.visible_cells() {
            let width = checkpoint_cell_width(cell.width())?;
            let (text, attributes) = match cell {
                CellRef::CellRef { cell, .. } => (cell.str(), cell.attrs()),
                CellRef::ClusterRef { text, attrs, .. } => (text, attrs),
            };
            if previous != Some(attributes) {
                entry = table.intern(CheckpointCellAttributes::capture(attributes)?)?;
                previous = Some(attributes);
            }
            let continues = runs.last().is_some_and(|run| {
                run.attributes == entry && run.text.len() + text.len() <= CHECKPOINT_RUN_TEXT_BYTES
            });
            if !continues {
                runs.try_reserve(1)
                    .map_err(|_| TerminalCheckpointError::ResourceAllocation("line.runs"))?;
                runs.push(CheckpointRun {
                    text: String::new(),
                    clusters: None,
                    attributes: entry,
                });
            }
            if let Some(run) = runs.last_mut() {
                run.push_cell(text, width)?;
            }
        }
        // Trailing default blanks, as in `capture_cells`.
        if runs.last().is_some_and(CheckpointRun::ends_in_single_blank) {
            let blank = CheckpointCellAttributes::capture(&CellAttributes::blank())?;
            while let Some(run) = runs.last_mut() {
                if !run.ends_in_single_blank()
                    || table.entries.get(run.attributes as usize) != Some(&blank)
                {
                    break;
                }
                run.pop_single_blank();
                if run.text.is_empty() {
                    runs.pop();
                }
            }
        }
        Ok(runs)
    }

    /// The text and attributes of its `index`th stored cell; a wide cell is
    /// one stored cell. `table` is its screen's version 4 attribute table.
    fn cell<'a>(
        &'a self,
        index: usize,
        table: Option<&'a [CheckpointCellAttributes]>,
    ) -> Option<(&'a str, &'a CheckpointCellAttributes)> {
        if let Some(cells) = &self.cells {
            return cells
                .get(index)
                .map(|cell| (cell.text.as_str(), &cell.attributes));
        }
        let (runs, table) = (self.runs.as_ref()?, table?);
        let mut index = index;
        for run in runs {
            let cells = run.cells();
            if index < cells {
                return Some((run.cell_text(index)?, table.get(run.attributes as usize)?));
            }
            index -= cells;
        }
        None
    }

    fn into_live(self, table: Option<&[CellAttributes]>) -> Result<Line, TerminalCheckpointError> {
        let seqno = usize_from_u64(self.seqno, "line.seqno")?;
        let cells = match (self.cells, self.runs, table) {
            (Some(cells), None, None) => Self::cells_into_live(cells)?,
            (None, Some(runs), Some(table)) => Self::runs_into_live(&runs, table)?,
            _ => return Err(line_encoding_mismatch()),
        };
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

    fn cells_into_live(cells: Vec<CheckpointCell>) -> Result<Vec<Cell>, TerminalCheckpointError> {
        let mut live = Vec::new();
        let stored_cells = cells.iter().try_fold(0usize, |total, cell| {
            total
                .checked_add(usize::from(cell.width))
                .ok_or(TerminalCheckpointError::ArithmeticOverflow("line.cells"))
        })?;
        live.try_reserve_exact(stored_cells)
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("line.cells"))?;
        for cell in cells {
            let width = usize::from(cell.width);
            let attributes = cell.attributes.into_live()?;
            live.push(Cell::new_grapheme_with_width(
                &cell.text,
                width,
                attributes.clone(),
            ));
            for _ in 1..width {
                live.push(Cell::blank_with_attrs(attributes.clone()));
            }
        }
        Ok(live)
    }

    fn runs_into_live(
        runs: &[CheckpointRun],
        table: &[CellAttributes],
    ) -> Result<Vec<Cell>, TerminalCheckpointError> {
        let mut live = Vec::new();
        let columns = runs.iter().try_fold(0usize, |total, run| {
            total
                .checked_add(run.columns())
                .ok_or(TerminalCheckpointError::ArithmeticOverflow("line.cells"))
        })?;
        live.try_reserve_exact(columns)
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("line.cells"))?;
        for run in runs {
            let attributes = table
                .get(run.attributes as usize)
                .ok_or_else(line_encoding_mismatch)?;
            match &run.clusters {
                None => {
                    for (start, ch) in run.text.char_indices() {
                        live.push(Cell::new_grapheme_with_width(
                            &run.text[start..start + ch.len_utf8()],
                            1,
                            attributes.clone(),
                        ));
                    }
                }
                Some(clusters) => {
                    let mut rest = run.text.as_str();
                    for (chars, width) in clusters {
                        let text = char_span(rest, 0, *chars as usize)
                            .ok_or_else(line_encoding_mismatch)?;
                        rest = &rest[text.len()..];
                        let width = usize::from(*width);
                        live.push(Cell::new_grapheme_with_width(
                            text,
                            width,
                            attributes.clone(),
                        ));
                        for _ in 1..width {
                            live.push(Cell::blank_with_attrs(attributes.clone()));
                        }
                    }
                }
            }
        }
        Ok(live)
    }

    /// `table` is its screen's version 4 attribute table; a version 3 line
    /// has none.
    fn validate(
        &self,
        limits: TerminalCheckpointLimits,
        usage: &mut crate::screen::ScreenCheckpointUsage,
        terminal_seqno: u64,
        table: Option<&[CheckpointCellAttributes]>,
    ) -> Result<(), TerminalCheckpointError> {
        if self.seqno > terminal_seqno {
            return Err(TerminalCheckpointError::InvalidField {
                field: "line.seqno",
                reason: "line sequence number is newer than the terminal",
            });
        }
        checked_accumulate(&mut usage.lines, 1, limits.max_total_lines, "screen_lines")?;
        checked_accumulate(
            &mut usage.retained_capture_bytes,
            std::mem::size_of::<Line>() + std::mem::size_of::<CheckpointLine>(),
            limits.max_retained_capture_bytes,
            "retained_capture_bytes",
        )?;
        let occupied_columns = match (&self.cells, &self.runs, table) {
            (Some(cells), None, None) => Self::validate_cells(cells, limits, usage)?,
            (None, Some(runs), Some(table)) => {
                Self::validate_runs(runs, table.len(), limits, usage)?
            }
            _ => return Err(line_encoding_mismatch()),
        };
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

    /// The columns a version 4 line's runs cover, each cell charged as
    /// `validate_cells` charges one; the attributes are charged once, with
    /// their table.
    fn validate_runs(
        runs: &[CheckpointRun],
        table_entries: usize,
        limits: TerminalCheckpointLimits,
        usage: &mut crate::screen::ScreenCheckpointUsage,
    ) -> Result<usize, TerminalCheckpointError> {
        let mut occupied_columns = 0usize;
        for run in runs {
            if run.attributes as usize >= table_entries {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "run.attributes",
                    reason: "a run's attributes must be an entry of its screen's table",
                });
            }
            if run.text.is_empty() || run.text.chars().any(char::is_control) {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "run.text",
                    reason: "run text must be nonempty and contain no control characters",
                });
            }
            ensure_limit("run_text_bytes", run.text.len(), limits.max_string_bytes)?;
            checked_accumulate(
                &mut usage.cell_text_bytes,
                run.text.len(),
                limits.max_total_cell_text_bytes,
                "cell_text_bytes",
            )?;
            let (cells, columns) = match &run.clusters {
                None => {
                    let chars = run.text.chars().count();
                    (chars, chars)
                }
                Some(clusters) => {
                    if clusters.iter().all(|cluster| *cluster == (1, 1)) {
                        return Err(TerminalCheckpointError::InvalidField {
                            field: "run.clusters",
                            reason: "clusters are stored only when some cell is not one char one column wide",
                        });
                    }
                    let mut rest = run.text.as_str();
                    let mut columns = 0usize;
                    for (chars, width) in clusters {
                        if !(1..=2).contains(width) {
                            return Err(TerminalCheckpointError::InvalidField {
                                field: "cell.width",
                                reason: "cell width must be one or two columns",
                            });
                        }
                        let text = char_span(rest, 0, *chars as usize).ok_or(
                            TerminalCheckpointError::InvalidField {
                                field: "run.clusters",
                                reason: "every cell holds at least one char of the run's text",
                            },
                        )?;
                        ensure_limit(
                            "cell_text_value_bytes",
                            text.len(),
                            limits.max_string_bytes,
                        )?;
                        rest = &rest[text.len()..];
                        columns += usize::from(*width);
                    }
                    if !rest.is_empty() {
                        return Err(TerminalCheckpointError::InvalidField {
                            field: "run.clusters",
                            reason: "a run's cells must hold all of its text",
                        });
                    }
                    (clusters.len(), columns)
                }
            };
            checked_accumulate(
                &mut usage.cell_records,
                cells,
                limits.max_total_cell_records,
                "screen_cell_records",
            )?;
            let retained = cells
                .checked_mul(std::mem::size_of::<Cell>())
                .and_then(|bytes| bytes.checked_add(std::mem::size_of::<CheckpointRun>()))
                .and_then(|bytes| bytes.checked_add(run.text.len()))
                .ok_or(TerminalCheckpointError::ArithmeticOverflow(
                    "retained_capture_bytes",
                ))?;
            checked_accumulate(
                &mut usage.retained_capture_bytes,
                retained,
                limits.max_retained_capture_bytes,
                "retained_capture_bytes",
            )?;
            occupied_columns = occupied_columns.checked_add(columns).ok_or(
                TerminalCheckpointError::ArithmeticOverflow("line_occupied_columns"),
            )?;
        }
        Ok(occupied_columns)
    }

    /// The columns a version 3 line's cells cover.
    fn validate_cells(
        cells: &[CheckpointCell],
        limits: TerminalCheckpointLimits,
        usage: &mut crate::screen::ScreenCheckpointUsage,
    ) -> Result<usize, TerminalCheckpointError> {
        let mut occupied_columns = 0usize;
        for cell in cells {
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
        Ok(occupied_columns)
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
        entries.extend(
            widths
                .iter()
                .map(|(codepoint, width)| CheckpointCustomCellWidth {
                    codepoint: *codepoint,
                    width: *width,
                }),
        );
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
        let retained_bytes =
            widths
                .len()
                .checked_mul(48)
                .ok_or(TerminalCheckpointError::ArithmeticOverflow(
                    "retained_capture_bytes",
                ))?;
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
        let custom_cell_width_map = CheckpointCustomCellWidthTableBuilder::live_widths(value)
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

    fn validate(&self, table_count: usize) -> Result<(), TerminalCheckpointError> {
        self.referenced_index(table_count)?;
        Ok(())
    }

    fn referenced_index(
        &self,
        table_count: usize,
    ) -> Result<Option<usize>, TerminalCheckpointError> {
        self.custom_cell_width_map
            .map(|index| {
                let index =
                    usize::try_from(index).map_err(|_| TerminalCheckpointError::InvalidField {
                        field: "unicode_version.custom_cell_width_map",
                        reason: "custom-width table index does not fit this architecture",
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
        ensure_limit("config.max_user_vars", max_user_vars, limits.max_user_vars)?;
        ensure_limit(
            "config.max_unicode_version_stack_depth",
            max_unicode_version_stack_depth,
            limits.max_unicode_stack_depth,
        )?;
        ensure_limit(
            "config.max_accumulating_title_len",
            max_accumulating_title_len,
            limits
                .max_string_bytes
                .min(limits.max_terminal_string_bytes),
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
            && checkpoint.max_unicode_version_stack_depth == self.max_unicode_version_stack_depth
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
            limits
                .max_string_bytes
                .min(limits.max_terminal_string_bytes),
        )
    }

    fn to_replay_configuration(
        &self,
        limits: TerminalCheckpointLimits,
        tables: &[Arc<HashMap<u32, u8>>],
    ) -> Result<ReplayTerminalConfiguration, TerminalCheckpointError> {
        Ok(ReplayTerminalConfiguration {
            scrollback_size: usize_from_u64(self.scrollback_size, "config.scrollback_size")?,
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
    wrap_next_column: Option<u64>,
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
            wrap_next_column: value.wrap_next_column.map(|column| column as u64),
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
            wrap_next_column: self
                .wrap_next_column
                .map(|column| usize_from_u64(column, "screen.saved_cursor.wrap_next_column"))
                .transpose()?,
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
        if self.wrap_next != self.wrap_next_column.is_some()
            || self.wrap_next_column.is_some_and(|column| {
                column == 0 || column > limits.max_cols as u64 || column <= self.position.x
            })
        {
            return Err(TerminalCheckpointError::InvalidField {
                field: "screen.saved_cursor.wrap_next_column",
                reason: "pending wrap requires a bounded insertion point after the cursor",
            });
        }
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
    /// Version 4 only: the attribute sets its lines' runs name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attributes: Option<Vec<CheckpointCellAttributes>>,
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
        encoding: TerminalCheckpointLineEncoding,
    ) -> Result<Self, TerminalCheckpointError> {
        let mut lines = Vec::new();
        lines
            .try_reserve_exact(value.lines.len())
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("screen.lines"))?;
        let mut table = match encoding {
            TerminalCheckpointLineEncoding::Cells => None,
            TerminalCheckpointLineEncoding::Runs => Some(CheckpointAttributeTable::default()),
        };
        for line in &value.lines {
            lines.push(CheckpointLine::capture(line, table.as_mut())?);
        }
        let mut keyboard_stack = Vec::new();
        keyboard_stack
            .try_reserve_exact(value.keyboard_stack.len())
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("screen.keyboard_stack"))?;
        for encoding in value.keyboard_stack {
            keyboard_stack.push(CheckpointKeyboardEncoding::from(encoding));
        }
        Ok(Self {
            attributes: table.map(|table| table.entries),
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

    /// The text and attributes of the `col`th stored cell of line `row`.
    fn cell(&self, row: usize, col: usize) -> Option<(&str, &CheckpointCellAttributes)> {
        self.lines
            .get(row)?
            .cell(col, self.attributes.as_deref())
    }

    fn into_live(self) -> Result<crate::screen::ScreenCheckpointParts, TerminalCheckpointError> {
        // Each entry becomes live attributes once; its cells share clones.
        let table = match self.attributes {
            None => None,
            Some(entries) => {
                let mut live = Vec::new();
                live.try_reserve_exact(entries.len())
                    .map_err(|_| TerminalCheckpointError::ResourceAllocation("screen.attributes"))?;
                for entry in entries {
                    live.push(entry.into_live()?);
                }
                Some(live)
            }
        };
        let mut lines = Vec::new();
        lines
            .try_reserve_exact(self.lines.len())
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("screen.lines"))?;
        for line in self.lines {
            lines.push(line.into_live(table.as_deref())?);
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
        encoding: TerminalCheckpointLineEncoding,
    ) -> Result<(), TerminalCheckpointError> {
        let table = match (encoding, self.attributes.as_deref()) {
            (TerminalCheckpointLineEncoding::Cells, None) => None,
            (TerminalCheckpointLineEncoding::Runs, Some(table)) => Some(table),
            _ => {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "screen.attributes",
                    reason: "a screen has an attribute table exactly when its lines are runs",
                });
            }
        };
        if let Some(table) = table {
            ensure_limit(
                "screen_attributes",
                table.len(),
                limits.max_total_cell_records,
            )?;
            for entry in table {
                checked_accumulate(
                    &mut usage.retained_capture_bytes,
                    std::mem::size_of::<CheckpointCellAttributes>()
                        + std::mem::size_of::<CellAttributes>(),
                    limits.max_retained_capture_bytes,
                    "retained_capture_bytes",
                )?;
                entry.validate(limits, usage)?;
            }
        }
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
        let visible_grid_cells =
            rows.checked_mul(cols)
                .ok_or(TerminalCheckpointError::ArithmeticOverflow(
                    "visible_grid_cells",
                ))?;
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
        let cold_prefix_line_count =
            usize_from_u64(self.cold_prefix_line_count, "screen.cold_prefix_line_count")?;
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
        let line_count =
            u64::try_from(self.lines.len()).map_err(|_| TerminalCheckpointError::InvalidField {
                field: "screen.lines",
                reason: "line count does not fit the checkpoint wire type",
            })?;
        let stable_end = self.stable_row_index_offset.checked_add(line_count).ok_or(
            TerminalCheckpointError::ArithmeticOverflow("screen.stable_row_range"),
        )?;
        let stable_max = u64::try_from(StableRowIndex::MAX).unwrap_or(u64::MAX);
        if stable_end > stable_max {
            return Err(TerminalCheckpointError::InvalidField {
                field: "screen.stable_row_index_offset",
                reason: "stable row range exceeds the runtime index type",
            });
        }
        for line in &self.lines {
            line.validate(limits, usage, terminal_seqno, table)?;
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
    /// How a capture under these limits writes its lines. Decoding admits
    /// either encoding; a terminal restored from a checkpoint captures
    /// again in the encoding it was restored from.
    pub line_encoding: TerminalCheckpointLineEncoding,
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
            max_total_custom_cell_widths: 2 * frankenterm_cell::MAX_CUSTOM_CELL_WIDTH_EXPANSION,
            max_terminal_string_bytes: 8 * 1024 * 1024,
            max_pixel_dimension: 1_048_576,
            max_dpi: 1_000_000,
            line_encoding: TerminalCheckpointLineEncoding::Cells,
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
            (
                "configured_max_rows",
                self.max_rows,
                Self::ABSOLUTE_MAX_ROWS,
            ),
            (
                "configured_max_cols",
                self.max_cols,
                Self::ABSOLUTE_MAX_COLS,
            ),
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
            (
                "limits.max_replay_record_bytes",
                self.max_replay_record_bytes,
            ),
            ("limits.max_replay_total_bytes", self.max_replay_total_bytes),
            ("limits.max_replay_records", self.max_replay_records),
            (
                "limits.max_replay_actions_per_record",
                self.max_replay_actions_per_record,
            ),
            ("limits.max_rows", self.max_rows),
            ("limits.max_cols", self.max_cols),
            ("limits.max_visible_grid_cells", self.max_visible_grid_cells),
            ("limits.max_total_lines", self.max_total_lines),
            ("limits.max_total_cell_records", self.max_total_cell_records),
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
pub struct TerminalCheckpointV3 {
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

impl std::fmt::Debug for TerminalCheckpointV3 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalCheckpointV3")
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

// Historical wire projections deliberately remain private. They describe the
// exact V2 field order without making V2 input acceptable to the live decoder.
#[derive(Serialize, Deserialize)]
#[serde(remote = "CheckpointSavedCursor", deny_unknown_fields)]
struct HistoricalSavedCursorV2 {
    position: CheckpointCursorPosition,
    wrap_next: bool,
    #[serde(skip)]
    wrap_next_column: Option<u64>,
    pen: CheckpointCellAttributes,
    dec_origin_mode: bool,
    g0_charset: CheckpointCharSet,
    g1_charset: CheckpointCharSet,
}

mod historical_saved_cursor_option {
    use super::*;

    #[derive(Serialize)]
    struct Borrowed<'a>(#[serde(with = "HistoricalSavedCursorV2")] &'a CheckpointSavedCursor);

    #[derive(Deserialize)]
    struct Owned(#[serde(with = "HistoricalSavedCursorV2")] CheckpointSavedCursor);

    pub(super) fn serialize<S: serde::Serializer>(
        value: &Option<CheckpointSavedCursor>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.as_ref().map(Borrowed).serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<CheckpointSavedCursor>, D::Error> {
        Option::<Owned>::deserialize(deserializer).map(|value| value.map(|cursor| cursor.0))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "CheckpointScreen", deny_unknown_fields)]
struct HistoricalScreenV2 {
    #[serde(skip)]
    attributes: Option<Vec<CheckpointCellAttributes>>,
    lines: Vec<CheckpointLine>,
    stable_row_index_offset: u64,
    cold_snapshot_generation: Option<CheckpointScrollbackGeneration>,
    cold_prefix_line_count: u64,
    allow_scrollback: bool,
    keyboard_stack: Vec<CheckpointKeyboardEncoding>,
    physical_rows: u32,
    physical_cols: u32,
    dpi: u32,
    #[serde(with = "historical_saved_cursor_option")]
    saved_cursor: Option<CheckpointSavedCursor>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "TerminalCheckpointV3", deny_unknown_fields)]
struct HistoricalTerminalV2 {
    version: u32,
    custom_cell_width_maps: Vec<CheckpointCustomCellWidthMap>,
    replay_config: CheckpointReplayConfigV2,
    #[serde(with = "HistoricalScreenV2")]
    primary_screen: CheckpointScreen,
    #[serde(with = "HistoricalScreenV2")]
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

#[derive(Serialize)]
struct HistoricalTerminalRef<'a>(#[serde(with = "HistoricalTerminalV2")] &'a TerminalCheckpointV3);

/// Explicit, capability-free preparation of the losslessly reconstructible V2
/// subset. The caller must separately authenticate these exact source bytes and
/// publish a new target identity; this value grants no guardian or replay lease.
pub struct PreparedTerminalCheckpointV2Migration {
    source: Zeroizing<Vec<u8>>,
    target: Zeroizing<Vec<u8>>,
    rows: u32,
    cols: u32,
    pixel_width: u64,
    pixel_height: u64,
}

impl std::fmt::Debug for PreparedTerminalCheckpointV2Migration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedTerminalCheckpointV2Migration")
            .field("source_bytes", &self.source.len())
            .field("target_bytes", &self.target.len())
            .field("rows", &self.rows)
            .field("cols", &self.cols)
            .field("pixel_width", &self.pixel_width)
            .field("pixel_height", &self.pixel_height)
            .finish()
    }
}

impl PreparedTerminalCheckpointV2Migration {
    pub fn decode_canonical_json(
        bytes: &[u8],
        limits: TerminalCheckpointLimits,
    ) -> Result<Self, TerminalCheckpointError> {
        limits.validate_policy()?;
        ensure_limit("encoded_bytes", bytes.len(), limits.max_encoded_bytes)?;
        if !bytes.starts_with(b"{\"version\":2,") {
            return Err(TerminalCheckpointError::NonCanonicalEncoding);
        }
        let mut budget = JsonStructuralBudget::new(limits);
        let mut decoder = serde_json::Deserializer::from_slice(bytes);
        JsonStructuralSeed {
            budget: &mut budget,
            depth: 0,
        }
        .deserialize(&mut decoder)
        .map_err(|_| TerminalCheckpointError::Serialization)?;
        decoder
            .end()
            .map_err(|_| TerminalCheckpointError::Serialization)?;
        let mut decoder = serde_json::Deserializer::from_slice(bytes);
        let mut checkpoint = HistoricalTerminalV2::deserialize(&mut decoder)
            .map_err(|_| TerminalCheckpointError::Serialization)?;
        decoder
            .end()
            .map_err(|_| TerminalCheckpointError::Serialization)?;
        for screen in [&checkpoint.primary_screen, &checkpoint.alternate_screen] {
            if screen
                .saved_cursor
                .as_ref()
                .is_some_and(|cursor| cursor.wrap_next)
            {
                return Err(TerminalCheckpointError::InvalidField {
                    field: "migration.v2.saved_cursor.wrap_next",
                    reason: "historical pending-wrap insertion endpoint is unavailable",
                });
            }
        }
        // Charge both retained canonical buffers together with the structural
        // materialization estimate. Writers reserve fallibly within the remaining
        // budget; retaining two individually bounded payloads is insufficient.
        let remaining = limits
            .max_retained_capture_bytes
            .checked_sub(budget.retained_bytes)
            .ok_or_else(|| {
                resource_limit(
                    "migration_retained_bytes",
                    budget.retained_bytes,
                    limits.max_retained_capture_bytes,
                )
            })?;
        let maximum = limits.max_encoded_bytes.min(remaining);
        let mut writer = BoundedCheckpointWriter::new(maximum);
        if serde_json::to_writer(&mut writer, &HistoricalTerminalRef(&checkpoint)).is_err() {
            return Err(match writer.failure {
                Some(BoundedWriterFailure::Limit) => resource_limit(
                    "migration_retained_bytes",
                    maximum.saturating_add(1),
                    maximum,
                ),
                Some(BoundedWriterFailure::Allocation) => {
                    TerminalCheckpointError::ResourceAllocation("migration.source")
                }
                None => TerminalCheckpointError::Serialization,
            });
        }
        let source = writer.into_inner();
        if source.as_slice() != bytes {
            return Err(TerminalCheckpointError::NonCanonicalEncoding);
        }
        let remaining = remaining.checked_sub(source.capacity()).ok_or_else(|| {
            resource_limit("migration_retained_bytes", source.capacity(), remaining)
        })?;
        // This is an explicit semantic projection, never a JSON version edit.
        // All common fields stay in the same owned model; V2's absent endpoints
        // were constructed as None by the historical cursor projection.
        checkpoint.version = TERMINAL_CHECKPOINT_VERSION;
        let target_limits = TerminalCheckpointLimits {
            max_encoded_bytes: limits.max_encoded_bytes.min(remaining),
            ..limits
        };
        let target = checkpoint.to_canonical_json(target_limits)?;
        ensure_limit("migration_retained_bytes", target.capacity(), remaining)?;
        let rows = checkpoint.primary_screen.physical_rows;
        let cols = checkpoint.primary_screen.physical_cols;
        let pixel_width = checkpoint.pixel_width;
        let pixel_height = checkpoint.pixel_height;
        drop(checkpoint);
        Ok(Self {
            source,
            target,
            rows,
            cols,
            pixel_width,
            pixel_height,
        })
    }

    #[must_use]
    pub fn source_canonical_payload(&self) -> &[u8] {
        &self.source
    }

    #[must_use]
    pub fn target_canonical_payload(&self) -> &[u8] {
        &self.target
    }

    #[must_use]
    pub const fn rows(&self) -> u32 {
        self.rows
    }

    #[must_use]
    pub const fn cols(&self) -> u32 {
        self.cols
    }

    #[must_use]
    pub const fn pixel_width(&self) -> u64 {
        self.pixel_width
    }

    #[must_use]
    pub const fn pixel_height(&self) -> u64 {
        self.pixel_height
    }
}

/// Bounded hot terminal state captured under the terminal lock.
/// The cold-history snapshot generation and interval are pinned under lock,
/// while materialization of cold rows and fragments from the spill sink is deferred
/// outside the lock.
#[derive(Debug)]
pub struct StagedHotCheckpoint {
    staged_primary: crate::screen::StagedScreenCheckpoint,
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
    custom_cell_width_maps: Vec<CheckpointCustomCellWidthMap>,
    replay_config: CheckpointReplayConfigV2,
    screen_limits: crate::screen::ScreenCheckpointLimits,
    screen_usage: crate::screen::ScreenCheckpointUsage,
    limits: TerminalCheckpointLimits,
}

/// Immutable hot state bound to the actual external parser boundary before
/// materialization leaves the reader thread.
pub struct StagedRecoveryCheckpoint {
    hot: StagedHotCheckpoint,
    stream_bytes: u64,
    limits: TerminalCheckpointLimits,
}

impl StagedRecoveryCheckpoint {
    pub fn semantic_generation(&self) -> u64 {
        self.hot.seqno
    }

    pub fn materialize(
        self,
    ) -> Result<crate::RecoveryTerminalCheckpointV3, TerminalCheckpointError> {
        let mut checkpoint = self.hot.materialize_cold_history(self.limits)?;
        let rows = checkpoint.primary_rows();
        let cols = checkpoint.primary_cols();
        let payload = checkpoint.encode_trimming_history(self.limits)?;
        Ok(crate::RecoveryTerminalCheckpointV3::from_canonical_parts(
            payload,
            rows,
            cols,
            self.stream_bytes,
        ))
    }
}

impl StagedHotCheckpoint {
    pub fn bind_external_parser_ground(
        self,
        ground: frankenterm_escape_parser::parser::RecoveryGroundBoundary<'_>,
    ) -> StagedRecoveryCheckpoint {
        let limits = self.limits;
        StagedRecoveryCheckpoint {
            hot: self,
            stream_bytes: ground.stream_bytes(),
            limits,
        }
    }

    /// Materialize cold-history rows and fragments from the spill sink outside the terminal lock,
    /// revalidate generation against the pinned generation (failing if stale),
    /// and assemble the canonical `TerminalCheckpointV3`.
    pub fn materialize_cold_history(
        mut self,
        limits: TerminalCheckpointLimits,
    ) -> Result<TerminalCheckpointV3, TerminalCheckpointError> {
        limits.validate_policy()?;

        let parts = self
            .staged_primary
            .materialize(&self.screen_limits, &mut self.screen_usage)
            .map_err(|err| match err {
                crate::screen::ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent => {
                    TerminalCheckpointError::StaleColdGeneration
                }
                crate::screen::ScreenCheckpointCaptureError::ColdScrollbackSnapshot(s) => {
                    TerminalCheckpointError::ColdScrollbackSnapshot(s)
                }
                crate::screen::ScreenCheckpointCaptureError::ColdScrollbackNotRecoveryGrade => {
                    TerminalCheckpointError::ColdScrollbackNotRecoveryGrade
                }
                _ => TerminalCheckpointError::InvalidField {
                    field: "primary_screen",
                    reason: "screen checkpoint capture failed",
                },
            })?;

        // The alternate screen was captured in this encoding when staged.
        let encoding = self.limits.line_encoding;
        let primary_screen = CheckpointScreen::capture(parts, encoding)?;
        let checkpoint = TerminalCheckpointV3 {
            version: encoding.version(),
            custom_cell_width_maps: self.custom_cell_width_maps,
            replay_config: self.replay_config,
            primary_screen,
            alternate_screen: self.alternate_screen,
            alternate_screen_active: self.alternate_screen_active,
            pen: self.pen,
            cursor: self.cursor,
            wrap_next: self.wrap_next,
            clear_semantic_attribute_on_newline: self.clear_semantic_attribute_on_newline,
            last_semantic_command_status: self.last_semantic_command_status,
            insert: self.insert,
            dec_auto_wrap: self.dec_auto_wrap,
            saved_dec_private_modes: self.saved_dec_private_modes,
            reverse_wraparound_mode: self.reverse_wraparound_mode,
            reverse_video_mode: self.reverse_video_mode,
            synchronized_output: self.synchronized_output,
            dec_origin_mode: self.dec_origin_mode,
            top_margin: self.top_margin,
            bottom_margin: self.bottom_margin,
            left_margin: self.left_margin,
            right_margin: self.right_margin,
            left_and_right_margin_mode: self.left_and_right_margin_mode,
            application_cursor_keys: self.application_cursor_keys,
            modify_other_keys: self.modify_other_keys,
            dec_ansi_mode: self.dec_ansi_mode,
            sixel_display_mode: self.sixel_display_mode,
            use_private_color_registers_for_each_graphic: self
                .use_private_color_registers_for_each_graphic,
            color_map: self.color_map,
            application_keypad: self.application_keypad,
            bracketed_paste: self.bracketed_paste,
            any_event_mouse: self.any_event_mouse,
            focus_tracking: self.focus_tracking,
            mouse_encoding: self.mouse_encoding,
            mouse_tracking: self.mouse_tracking,
            button_event_mouse: self.button_event_mouse,
            current_mouse_buttons: self.current_mouse_buttons,
            last_mouse_move: self.last_mouse_move,
            cursor_visible: self.cursor_visible,
            keyboard_encoding: self.keyboard_encoding,
            g0_charset: self.g0_charset,
            g1_charset: self.g1_charset,
            shift_out: self.shift_out,
            newline_mode: self.newline_mode,
            tab_stops: self.tab_stops,
            tab_width: self.tab_width,
            title: self.title,
            icon_title: self.icon_title,
            progress: self.progress,
            palette: self.palette,
            pixel_width: self.pixel_width,
            pixel_height: self.pixel_height,
            dpi: self.dpi,
            current_dir: self.current_dir,
            term_program: self.term_program,
            term_version: self.term_version,
            sixel_scrolls_right: self.sixel_scrolls_right,
            user_vars: self.user_vars,
            kitty_max_image_id: self.kitty_max_image_id,
            seqno: self.seqno,
            unicode_version: self.unicode_version,
            unicode_version_stack: self.unicode_version_stack,
            enable_conpty_quirks: self.enable_conpty_quirks,
            suppress_initial_title_change: self.suppress_initial_title_change,
            accumulating_title: self.accumulating_title,
            lost_focus_seqno: self.lost_focus_seqno,
            lost_focus_alerted_seqno: self.lost_focus_alerted_seqno,
            focused: self.focused,
            bidi_enabled: self.bidi_enabled,
            bidi_hint: self.bidi_hint,
        };

        checkpoint.validate(self.limits)?;
        checkpoint.validate(limits)?;
        Ok(checkpoint)
    }

    /// Number of physical rows in the primary screen.
    #[must_use]
    pub fn primary_rows(&self) -> usize {
        self.staged_primary.physical_rows
    }

    /// Number of physical columns in the primary screen.
    #[must_use]
    pub fn primary_cols(&self) -> usize {
        self.staged_primary.physical_cols
    }
}

impl TerminalCheckpointV3 {
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
            ensure_limit(
                "terminal_string_value_bytes",
                value.len(),
                limits.max_string_bytes,
            )?;
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

        for version in std::iter::once(&terminal.unicode_version).chain(
            terminal
                .unicode_version_stack
                .iter()
                .map(|entry| &entry.vers),
        ) {
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

        charge_records(usage, terminal.saved_dec_private_modes.len(), 64, limits)?;
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
        Self::preflight_terminal_fields(terminal, limits, &mut usage, false)?;
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
        Self::capture_staged(terminal, limits)?.materialize_cold_history(limits)
    }

    /// Capture bounded hot terminal state and pin cold-history generation under the terminal lock.
    pub fn capture_staged(
        terminal: &TerminalState,
        limits: TerminalCheckpointLimits,
    ) -> Result<StagedHotCheckpoint, TerminalCheckpointError> {
        limits.validate_policy()?;
        if terminal.current_seqno() == usize::MAX {
            return Err(TerminalCheckpointError::InvalidField {
                field: "seqno",
                reason: "terminal semantic generation exhausted",
            });
        }
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
        Self::preflight_terminal_fields(terminal, limits, &mut screen_usage, true)?;
        // Restore starts with protection off; any other mode decides which
        // erases keep protected cells, possibly in cold scrollback.
        if terminal.protected_mode != ProtectedMode::Off {
            return Err(TerminalCheckpointError::UnsupportedCharacterProtection);
        }
        let kitty_max_image_id = terminal
            .kitty_img
            .checkpoint_high_water_if_quiescent()
            .ok_or(TerminalCheckpointError::UnsupportedGraphicsState)?;
        let screen_limits = limits.screen_limits();
        Self::preflight_checkpoint_attributes(&terminal.pen, limits, &mut screen_usage)?;
        for saved in [
            terminal.screen.screen.saved_cursor.as_ref(),
            terminal.screen.alt_screen.saved_cursor.as_ref(),
        ]
        .iter()
        .copied()
        .flatten()
        {
            Self::preflight_checkpoint_attributes(&saved.pen, limits, &mut screen_usage)?;
        }

        // Alternate screen has no scrollback; capture resident parts directly
        let alternate_screen = terminal
            .screen
            .alt_screen
            .checkpoint_parts(&screen_limits, &mut screen_usage)?;

        let staged_primary = terminal
            .screen
            .screen
            .checkpoint_parts_staged(&screen_limits, &mut screen_usage)
            .map_err(|err| match err {
                crate::screen::ScreenCheckpointCaptureError::ColdScrollbackMetadataInconsistent => {
                    TerminalCheckpointError::StaleColdGeneration
                }
                crate::screen::ScreenCheckpointCaptureError::ColdScrollbackSnapshot(s) => {
                    TerminalCheckpointError::ColdScrollbackSnapshot(s)
                }
                crate::screen::ScreenCheckpointCaptureError::ColdScrollbackNotRecoveryGrade => {
                    TerminalCheckpointError::ColdScrollbackNotRecoveryGrade
                }
                _ => TerminalCheckpointError::InvalidField {
                    field: "primary_screen",
                    reason: "screen checkpoint capture failed",
                },
            })?;

        let checkpoint_unicode_version =
            CheckpointUnicodeVersion::capture(&terminal.unicode_version, &custom_cell_width_maps)?;
        let mut checkpoint_unicode_version_stack = Vec::new();
        checkpoint_unicode_version_stack
            .try_reserve_exact(terminal.unicode_version_stack.len())
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("unicode_version_stack"))?;
        for entry in &terminal.unicode_version_stack {
            checkpoint_unicode_version_stack.push(CheckpointUnicodeVersionStackEntry {
                version: CheckpointUnicodeVersion::capture(&entry.vers, &custom_cell_width_maps)?,
                label: entry.label.clone(),
            });
        }

        Ok(StagedHotCheckpoint {
            staged_primary,
            alternate_screen: CheckpointScreen::capture(alternate_screen, limits.line_encoding)?,
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
            left_margin: u64_from_usize(terminal.left_and_right_margins.start, "left_margin")?,
            right_margin: u64_from_usize(terminal.left_and_right_margins.end, "right_margin")?,
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
            lost_focus_seqno: u64_from_usize(terminal.lost_focus_seqno, "lost_focus_seqno")?,
            lost_focus_alerted_seqno: u64_from_usize(
                terminal.lost_focus_alerted_seqno,
                "lost_focus_alerted_seqno",
            )?,
            focused: terminal.focused,
            bidi_enabled: terminal.bidi_enabled,
            bidi_hint: terminal.bidi_hint.map(CheckpointBidiHint::from),
            custom_cell_width_maps,
            replay_config,
            screen_limits,
            screen_usage,
            limits,
        })
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
            ensure_limit(
                "terminal_string_value_bytes",
                value.len(),
                limits.max_string_bytes,
            )?;
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
        let encoding = TerminalCheckpointLineEncoding::of_version(self.version).ok_or(
            TerminalCheckpointError::UnsupportedVersion {
                observed: self.version,
                supported: COMPACT_TERMINAL_CHECKPOINT_VERSION,
            },
        )?;
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
            .validate(true, limits, &mut usage, self.seqno, encoding)?;
        self.alternate_screen
            .validate(false, limits, &mut usage, self.seqno, encoding)?;

        let rows = self.primary_screen.physical_rows;
        let cols = self.primary_screen.physical_cols;
        let rows_usize =
            usize::try_from(rows).map_err(|_| TerminalCheckpointError::InvalidField {
                field: "primary_screen.physical_rows",
                reason: "value does not fit this target architecture",
            })?;
        let configured_scrollback =
            usize_from_u64(self.replay_config.scrollback_size, "config.scrollback_size")?;
        let aggregate_visible_rows =
            rows_usize
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

        charge_records(&mut usage, self.saved_dec_private_modes.len(), 64, limits)?;
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
    /// emitted as a recovery authority, and the decoder's structural pass
    /// runs last, so neither is a payload the decoder would refuse.
    pub fn to_canonical_json(
        &self,
        limits: TerminalCheckpointLimits,
    ) -> Result<Zeroizing<Vec<u8>>, TerminalCheckpointError> {
        let encoded = self.encode_validated_canonical_json(limits)?;
        json_structural_retained_bytes(&encoded, limits)?;
        Ok(encoded)
    }

    /// Like `to_canonical_json` for a live recovery capture, except that a
    /// checkpoint over the decoder's structural or encoded-byte budget first
    /// gives up its oldest primary-screen scrollback (see
    /// `drop_oldest_primary_history_to_fit`). A recovery image of the newest
    /// history beats none: a payload its decoder refused used to poison the
    /// pane's live parser and end the pane.
    fn encode_trimming_history(
        &mut self,
        limits: TerminalCheckpointLimits,
    ) -> Result<Zeroizing<Vec<u8>>, TerminalCheckpointError> {
        match self.to_canonical_json(limits) {
            Err(TerminalCheckpointError::ResourceLimit {
                resource: "json_structural_bytes" | "encoded_bytes",
                ..
            }) => {
                self.drop_oldest_primary_history_to_fit(limits)?;
                self.to_canonical_json(limits)
            }
            result => result,
        }
    }

    /// Drop the fewest oldest primary-screen scrollback lines that bring the
    /// canonical encoding within 7/8 of both the decoder's structural budget
    /// and the encoded-byte ceiling. Each line's share is measured alone: the
    /// structural pass charges every node once wherever it sits, so the shares
    /// add up exactly. The cold prefix is the oldest history, so it shrinks
    /// first; the stable row offset moves past the dropped lines.
    fn drop_oldest_primary_history_to_fit(
        &mut self,
        limits: TerminalCheckpointLimits,
    ) -> Result<(), TerminalCheckpointError> {
        let structural_target =
            limits.max_retained_capture_bytes - limits.max_retained_capture_bytes / 8;
        let encoded_target = limits.max_encoded_bytes - limits.max_encoded_bytes / 8;
        let measuring = TerminalCheckpointLimits {
            max_retained_capture_bytes: usize::MAX,
            max_string_bytes: usize::MAX,
            ..limits
        };

        let rows = self.primary_screen.physical_rows as usize;
        let droppable = self.primary_screen.lines.len().saturating_sub(rows);
        let mut shares = Vec::with_capacity(droppable);
        for line in &self.primary_screen.lines[..droppable] {
            let (structural, bytes) = measure_canonical_json(line, measuring)?;
            // Inside the lines array a line also costs its separating comma.
            shares.push((structural, bytes + 1));
        }
        let scrollback: Vec<CheckpointLine> =
            self.primary_screen.lines.drain(..droppable).collect();
        let rest = measure_canonical_json(&*self, measuring);
        self.primary_screen.lines.splice(0..0, scrollback);
        let (mut structural, mut encoded) = rest?;
        for (line_structural, line_bytes) in &shares {
            structural = structural.saturating_add(*line_structural);
            encoded = encoded.saturating_add(*line_bytes);
        }

        let mut dropped = 0;
        while (structural > structural_target || encoded > encoded_target) && dropped < droppable {
            structural -= shares[dropped].0;
            encoded -= shares[dropped].1;
            dropped += 1;
        }
        if structural > structural_target {
            return Err(resource_limit(
                "json_structural_bytes",
                structural,
                structural_target,
            ));
        }
        if encoded > encoded_target {
            return Err(resource_limit("encoded_bytes", encoded, encoded_target));
        }
        if dropped == 0 {
            return Ok(());
        }
        self.primary_screen.lines.drain(..dropped);
        let dropped = u64::try_from(dropped)
            .map_err(|_| TerminalCheckpointError::ArithmeticOverflow("screen.dropped_history"))?;
        self.primary_screen.stable_row_index_offset = self
            .primary_screen
            .stable_row_index_offset
            .checked_add(dropped)
            .ok_or(TerminalCheckpointError::ArithmeticOverflow(
                "screen.stable_row_index_offset",
            ))?;
        self.primary_screen.cold_prefix_line_count = self
            .primary_screen
            .cold_prefix_line_count
            .saturating_sub(dropped);
        Ok(())
    }

    /// `to_canonical_json` without the structural pass: decoding has already
    /// run that pass over the bytes it compares this re-encoding against.
    fn encode_validated_canonical_json(
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
    ) -> Result<ValidatedTerminalCheckpointV3, TerminalCheckpointError> {
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
        let encoding = TerminalCheckpointLineEncoding::of_version(observed).ok_or(
            TerminalCheckpointError::UnsupportedVersion {
                observed,
                supported: COMPACT_TERMINAL_CHECKPOINT_VERSION,
            },
        )?;
        if version_text != observed.to_string() {
            return Err(TerminalCheckpointError::NonCanonicalEncoding);
        }

        json_structural_retained_bytes(bytes, limits)?;

        let checkpoint: Self =
            serde_json::from_slice(bytes).map_err(|_| TerminalCheckpointError::Serialization)?;
        // Validates, then re-encodes: the structural pass already ran above.
        let canonical = checkpoint.encode_validated_canonical_json(limits)?;
        if canonical.as_slice() != bytes {
            return Err(TerminalCheckpointError::NonCanonicalEncoding);
        }
        // What it restores captures again as it was captured.
        let limits = TerminalCheckpointLimits {
            line_encoding: encoding,
            ..limits
        };
        Ok(ValidatedTerminalCheckpointV3 { checkpoint, limits })
    }

    /// The schema version a canonical payload declares, read from its prefix
    /// without decoding it; `None` for bytes no decoder would admit.
    #[must_use]
    pub fn canonical_payload_version(bytes: &[u8]) -> Option<u32> {
        let tail = bytes.strip_prefix(b"{\"version\":")?;
        let comma = tail.iter().take(11).position(|byte| *byte == b',')?;
        let version = std::str::from_utf8(&tail[..comma]).ok()?.parse().ok()?;
        TerminalCheckpointLineEncoding::of_version(version).map(|_| version)
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    pub fn validate_into(
        self,
        limits: TerminalCheckpointLimits,
    ) -> Result<ValidatedTerminalCheckpointV3, TerminalCheckpointError> {
        self.validate(limits)?;
        Ok(ValidatedTerminalCheckpointV3 {
            checkpoint: self,
            limits,
        })
    }

    pub fn into_recovery_checkpoint_at_external_parser_ground(
        mut self,
        ground: frankenterm_escape_parser::parser::RecoveryGroundBoundary<'_>,
        limits: TerminalCheckpointLimits,
    ) -> Result<crate::RecoveryTerminalCheckpointV3, TerminalCheckpointError> {
        let active_screen = if self.alternate_screen_active {
            &self.alternate_screen
        } else {
            &self.primary_screen
        };
        let rows = active_screen.physical_rows as usize;
        let cols = active_screen.physical_cols as usize;
        let canonical_payload = self.encode_trimming_history(limits)?;
        Ok(crate::RecoveryTerminalCheckpointV3::from_canonical_parts(
            canonical_payload,
            rows,
            cols,
            ground.stream_bytes(),
        ))
    }

    /// Number of physical rows in the primary screen.
    #[must_use]
    pub fn primary_rows(&self) -> usize {
        self.primary_screen.physical_rows as usize
    }

    /// Number of physical columns in the primary screen.
    #[must_use]
    pub fn primary_cols(&self) -> usize {
        self.primary_screen.physical_cols as usize
    }

    /// Number of lines in the primary screen buffer (including cold scrollback prefix rows).
    #[must_use]
    pub fn primary_lines_count(&self) -> usize {
        self.primary_screen.lines.len()
    }

    /// Number of cold scrollback prefix rows prepended to the primary screen buffer.
    #[must_use]
    pub fn primary_cold_prefix_lines(&self) -> u64 {
        self.primary_screen.cold_prefix_line_count
    }

    /// Number of lines in the alternate screen buffer.
    #[must_use]
    pub fn alternate_lines_count(&self) -> usize {
        self.alternate_screen.lines.len()
    }

    /// Parser-observed title from this exact semantic checkpoint.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Parser-observed current-directory URL, without a live process fallback.
    #[must_use]
    pub fn current_directory(&self) -> Option<&str> {
        self.current_dir.as_deref()
    }

    #[must_use]
    pub fn semantic_generation(&self) -> u64 {
        self.seqno
    }

    #[must_use]
    pub fn pixel_dimensions_and_dpi(&self) -> (u64, u64, u32) {
        (self.pixel_width, self.pixel_height, self.dpi)
    }

    /// Whether the alternate screen buffer is currently active.
    #[must_use]
    pub fn is_alternate_screen_active(&self) -> bool {
        self.alternate_screen_active
    }

    /// Whether bracketed paste mode is enabled.
    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    /// Cursor position as `(row, col)`.
    #[must_use]
    pub fn cursor_position(&self) -> (VisibleRowIndex, u64) {
        (self.cursor.y, self.cursor.x)
    }

    /// Whether the cursor is visible.
    #[must_use]
    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// Text of the cell at `(row, col)` in the primary screen.
    #[must_use]
    pub fn primary_cell_text(&self, row: usize, col: usize) -> Option<&str> {
        self.primary_screen.cell(row, col).map(|(text, _)| text)
    }

    /// Text of the cell at `(row, col)` in the alternate screen.
    #[must_use]
    pub fn alternate_cell_text(&self, row: usize, col: usize) -> Option<&str> {
        self.alternate_screen.cell(row, col).map(|(text, _)| text)
    }

    /// Intensity attribute of the cell at `(row, col)` in the primary screen.
    #[must_use]
    pub fn primary_cell_intensity(&self, row: usize, col: usize) -> Option<Intensity> {
        self.primary_screen
            .cell(row, col)
            .map(|(_, attributes)| attributes.intensity)
    }

    /// Underline attribute of the cell at `(row, col)` in the alternate screen.
    #[must_use]
    pub fn alternate_cell_underline(&self, row: usize, col: usize) -> Option<Underline> {
        self.alternate_screen
            .cell(row, col)
            .map(|(_, attributes)| attributes.underline)
    }
}

impl ValidatedTerminalCheckpointV3 {
    /// Reconstruct the complete immutable replay configuration without live
    /// writer, callback, or spill capabilities.
    pub fn replay_configuration(
        &self,
    ) -> Result<Arc<dyn TerminalConfiguration>, TerminalCheckpointError> {
        let maps = decode_custom_cell_width_maps(&self.checkpoint.custom_cell_width_maps)?;
        Ok(Arc::new(
            self.checkpoint
                .replay_config
                .to_replay_configuration(self.limits, &maps)?,
        ))
    }

    /// Reference to the underlying validated `TerminalCheckpointV3`.
    #[must_use]
    pub fn checkpoint(&self) -> &TerminalCheckpointV3 {
        &self.checkpoint
    }

    /// Consume the wrapper and return the validated `TerminalCheckpointV3`.
    #[must_use]
    pub fn into_checkpoint(self) -> TerminalCheckpointV3 {
        self.checkpoint
    }

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

    /// Authenticated terminal pixel width retained by the canonical model.
    #[must_use]
    pub const fn pixel_width(&self) -> u64 {
        self.checkpoint.pixel_width
    }

    /// Authenticated terminal pixel height retained by the canonical model.
    #[must_use]
    pub const fn pixel_height(&self) -> u64 {
        self.checkpoint.pixel_height
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
            replay_projection.to_replay_configuration(self.limits, &custom_cell_width_maps)?,
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
        checkpoint: TerminalCheckpointV3,
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
        let aggregate_visible_rows =
            rows.checked_mul(2)
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

        let TerminalCheckpointV3 {
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
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("saved_dec_private_modes"))?;
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
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("current_mouse_buttons"))?;
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
            .map_err(|_| TerminalCheckpointError::ResourceAllocation("unicode_version_stack"))?;
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
        let restored_lost_focus_seqno = usize_from_u64(lost_focus_seqno, "lost_focus_seqno")?;
        let restored_lost_focus_alerted_seqno =
            usize_from_u64(lost_focus_alerted_seqno, "lost_focus_alerted_seqno")?;
        let restored_kitty = KittyImageState::quiescent_for_checkpoint_restore(
            config.kitty_image_budget_bytes(),
            config.kitty_image_max_transmission_bytes(),
            kitty_max_image_id,
        );
        let mut restored_image_cache = lru::LruCache::unbounded();
        restored_image_cache.resize(NonZeroUsize::new(16).expect("nonzero cache capacity"));
        // Hot per-batch configuration is derived from the restoring config,
        // exactly as a freshly constructed terminal captures it.
        let batch_config = BatchConfig::capture(config.as_ref());

        Ok(TerminalState {
            config,
            batch_config,
            screen: screens,
            pen: restored_pen,
            // Capture refuses any other mode.
            protected_mode: ProtectedMode::Off,
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
            osc52_prompt: crate::terminal::Osc52PromptSlot::default(),
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
            // Restored cells were not written by this performer, so any of
            // them may end in ZWJ; keep the continuation scan enabled.
            zwj_tail_cell_possible: true,
            print_char_table: Default::default(),
            enable_conpty_quirks,
            line_flood: super::performer::line_flood_default(),
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
    /// A protected cell or pen, or a protection mode other than off: state
    /// the checkpoint format does not hold (DECSCA, SPA).
    UnsupportedCharacterProtection,
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
    StaleColdGeneration,
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
            Self::UnsupportedCharacterProtection => formatter.write_str(
                "terminal contains character protection unsupported by checkpointing",
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
            Self::StaleColdGeneration => formatter.write_str(
                "cold scrollback snapshot generation is stale",
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
        ) -> Result<crate::config::ScrollbackSnapshot, crate::config::ScrollbackSpillError>
        {
            Err(crate::config::ScrollbackSpillError::StorageUnavailable)
        }

        fn replace_scrollback_prefix(
            &self,
            expected_generation: Option<crate::config::ScrollbackSnapshotGeneration>,
            prefix: crate::config::ScrollbackPrefix<'_>,
            _max_retained_rows: usize,
        ) -> Result<crate::config::ScrollbackReplaceCommit, crate::config::ScrollbackSpillError>
        {
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
        ) -> Result<crate::config::ScrollbackClearCommit, crate::config::ScrollbackSpillError>
        {
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

        fn scrollback_spill_sink(&self) -> Option<Arc<dyn crate::config::ScrollbackSpillSink>> {
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
        for entry in ordered_entries.iter().rev() {
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
        terminal
            .unicode_version_stack
            .push(UnicodeVersionStackEntry {
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
        let checkpoint = TerminalCheckpointV3::capture(&terminal).expect("capture terminal");
        let encoded = serde_json::to_vec(&checkpoint).expect("serialize checkpoint");
        let decoded: TerminalCheckpointV3 =
            serde_json::from_slice(&encoded).expect("deserialize checkpoint");

        assert_eq!(decoded, checkpoint);
        assert!(checkpoint.alternate_screen_active);
        assert_eq!(checkpoint.title, "checkpoint-title");
        assert_ne!(
            checkpoint.primary_screen.lines,
            checkpoint.alternate_screen.lines
        );
    }

    /// A scroll creates rows a full width of default blanks; a checkpoint
    /// stores none of a row's trailing ones (what a line past its end shows
    /// anyway), but keeps a colored blank and a wrapped row's last cell.
    #[test]
    fn trailing_default_blanks_are_not_stored() {
        let mut terminal = Terminal::new(
            TerminalSize {
                rows: 80,
                cols: 120,
                pixel_width: 960,
                pixel_height: 1280,
                dpi: 96,
            },
            Arc::new(CheckpointTestConfig),
            "FrankenTerm",
            "checkpoint-test",
            Box::new(Vec::<u8>::new()),
        );
        for tick in 0..200 {
            terminal.advance_bytes(format!("tick {tick}\r\n").as_bytes());
        }
        terminal.advance_bytes(b"red:\x1b[41m \x1b[0m\r\n");
        terminal.advance_bytes([b'w'; 121]);
        let checkpoint = TerminalCheckpointV3::capture(&terminal).expect("capture terminal");
        let blank =
            CheckpointCellAttributes::capture(&CellAttributes::blank()).expect("blank attributes");
        let lines = checkpoint
            .primary_screen
            .lines
            .iter()
            .map(|line| line.cells.as_deref().expect("version 3 cells"))
            .collect::<Vec<_>>();
        assert!(lines
            .iter()
            .all(|cells| cells.last().is_none_or(|cell| {
                !(cell.width == 1 && cell.text == " " && cell.attributes == blank)
            })));
        // The colored blank and the wrapped row's last cell stay.
        assert!(lines.iter().any(|cells| cells
            .last()
            .is_some_and(|cell| cell.text == " " && cell.attributes != blank)));
        assert!(lines.iter().any(|cells| cells.len() == 120
            && cells.last().is_some_and(|cell| cell.attributes.wrapped)));

        let limits = TerminalCheckpointLimits::default();
        let payload = checkpoint
            .to_canonical_json(limits)
            .expect("encode checkpoint");
        // Its ~280 rows, each stored a full 120 cells wide, were about 8 MB;
        // their short lines alone are well under 1 MB.
        assert!(payload.len() < 1024 * 1024, "{} bytes", payload.len());
        let decoded =
            TerminalCheckpointV3::decode_canonical_json(&payload, limits).expect("decode");
        assert_eq!(decoded.checkpoint(), &checkpoint);
        decoded
            .restore_inert(Arc::new(CheckpointTestConfig))
            .expect("restore");
    }

    #[test]
    fn unsupported_out_of_band_graphics_fail_closed() {
        let mut terminal = terminal();
        terminal.kitty_img.mark_nonempty_for_checkpoint_test();

        assert_eq!(
            TerminalCheckpointV3::capture(&terminal),
            Err(TerminalCheckpointError::UnsupportedGraphicsState)
        );
    }

    /// The format holds no character protection (DECSCA, SPA), so capture
    /// refuses while a pen, a cell or the protection mode could still
    /// change what an erase keeps.
    #[test]
    fn character_protection_fails_closed() {
        let refused = Err(TerminalCheckpointError::UnsupportedCharacterProtection);
        let mut terminal = terminal();
        terminal.advance_bytes(b"\x1b[1\"q");
        assert_eq!(TerminalCheckpointV3::capture(&terminal), refused);

        // DECSCA 0 stops protecting, but leaves the DEC mode.
        terminal.advance_bytes(b"P\x1b[0\"q");
        assert_eq!(TerminalCheckpointV3::capture(&terminal), refused);

        // DECSTR turns protection off; the cell printed before stays
        // protected for a later DECSCA or SPA.
        terminal.advance_bytes(b"\x1b[!p");
        assert_eq!(TerminalCheckpointV3::capture(&terminal), refused);
        terminal.advance_bytes(b"\x1b[2J");
        assert!(TerminalCheckpointV3::capture(&terminal).is_ok());

        // SPA's mode outlasts EPA, until an ED 2 keeps no protected cell.
        terminal.advance_bytes(b"\x1bV\x1bW");
        assert_eq!(TerminalCheckpointV3::capture(&terminal), refused);
        terminal.advance_bytes(b"\x1b[2J");
        assert!(TerminalCheckpointV3::capture(&terminal).is_ok());
    }

    #[test]
    fn canonical_projection_sorts_terminal_maps() {
        let mut first = terminal();
        first.user_vars.insert("zeta".into(), "last".into());
        first.user_vars.insert("alpha".into(), "first".into());
        let mut second = terminal();
        second.user_vars.insert("alpha".into(), "first".into());
        second.user_vars.insert("zeta".into(), "last".into());

        let first = TerminalCheckpointV3::capture(&first).expect("capture first terminal");
        let second = TerminalCheckpointV3::capture(&second).expect("capture second terminal");
        assert_eq!(
            serde_json::to_vec(&first).expect("serialize first terminal"),
            serde_json::to_vec(&second).expect("serialize second terminal")
        );
    }

    #[test]
    fn canonical_custom_width_table_deduplicates_shared_semantics_and_sorts_content() {
        let limits = TerminalCheckpointLimits::default();
        let first = TerminalCheckpointV3::capture_with_limits(
            &terminal_with_distinct_custom_width_maps(false),
            limits,
        )
        .expect("capture first custom-width terminal");
        let second = TerminalCheckpointV3::capture_with_limits(
            &terminal_with_distinct_custom_width_maps(true),
            limits,
        )
        .expect("capture reverse-insertion custom-width terminal");

        assert_eq!(first.custom_cell_width_maps.len(), 2);
        assert!(first
            .custom_cell_width_maps
            .windows(2)
            .all(|pair| pair[0] < pair[1]));
        assert_ne!(
            first.replay_config.unicode_version.custom_cell_width_map,
            first.unicode_version.custom_cell_width_map,
        );
        assert_eq!(
            first.unicode_version_stack[0].version.custom_cell_width_map,
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
        let mut checkpoint = TerminalCheckpointV3::capture_with_limits(
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
        let mut checkpoint = TerminalCheckpointV3::capture_with_limits(
            &terminal_with_distinct_custom_width_maps(false),
            limits,
        )
        .expect("capture custom-width terminal");
        checkpoint
            .replay_config
            .unicode_version
            .custom_cell_width_map = checkpoint.unicode_version.custom_cell_width_map;

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
        let checkpoint = TerminalCheckpointV3::capture_with_limits(
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
        let mut checkpoint = TerminalCheckpointV3::capture_with_limits(
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
        let checkpoint = TerminalCheckpointV3::capture_with_limits(&terminal(), limits)
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
    fn cold_seam_checkpoint_replays_and_activates_the_canonical_replacement() {
        use crate::config::ScrollbackSpillSink;

        let (mut live, sink) = crate::screen::tests::cold_seam_test_terminal();
        let limits = TerminalCheckpointLimits::default();
        let config = live.get_config();
        let before = TerminalCheckpointV3::capture_with_limits(&live, limits).unwrap();
        let oldest = sink.oldest_scrollback_row().unwrap();
        let frontier = live.screen().phys_to_stable_row_index(0);
        let stored_before: Vec<_> = (oldest..frontier)
            .map(|row| sink.load_scrollback_line(row).unwrap())
            .collect();
        let mut prepared = live
            .screen()
            .capture_cold_seam_reflow()
            .unwrap()
            .expect("fixture must exercise a real cold/resident seam")
            .hydrate(|| false)
            .unwrap();
        assert!(prepared.is_ready());
        live.increment_seqno();
        let seqno = live.current_seqno();
        assert!(live
            .screen_mut()
            .install_cold_seam_reflow(&mut prepared, seqno)
            .unwrap());
        let captured = TerminalCheckpointV3::capture_with_limits(&live, limits).unwrap();
        let text = |screen: &CheckpointScreen| {
            screen
                .lines
                .iter()
                .map(|line| line.clone().into_live(None).unwrap().as_str().into_owned())
                .collect::<String>()
        };
        assert_eq!(text(&before.primary_screen), text(&captured.primary_screen));
        assert_ne!(before.primary_screen.lines, captured.primary_screen.lines);
        assert_eq!(
            before.primary_screen.cold_snapshot_generation,
            captured.primary_screen.cold_snapshot_generation,
            "canonical overlay retains the original storage CAS authority"
        );
        assert_eq!(
            (oldest..frontier)
                .map(|row| sink.load_scrollback_line(row).unwrap())
                .collect::<Vec<_>>(),
            stored_before,
            "live seam installation must not rewrite immutable storage"
        );
        let canonical = captured.to_canonical_json(limits).unwrap();
        let mut inert = TerminalCheckpointV3::decode_canonical_json(&canonical, limits)
            .unwrap()
            .restore_inert(config)
            .unwrap();
        assert_eq!(
            inert
                .checkpoint()
                .unwrap()
                .to_canonical_json(limits)
                .unwrap(),
            canonical
        );
        inert.replay_bytes(b"tail").unwrap();
        let replayed = inert.checkpoint().unwrap();
        let activated = inert.into_live(Box::new(std::io::sink())).unwrap();
        let after = TerminalCheckpointV3::capture_with_limits(&activated, limits).unwrap();
        assert_eq!(after.primary_screen.lines, replayed.primary_screen.lines);
        assert_eq!(after.cursor, replayed.cursor);
        assert_eq!(after.wrap_next, replayed.wrap_next);
        let original_generation = captured.primary_screen.cold_snapshot_generation.unwrap();
        let committed_generation = after.primary_screen.cold_snapshot_generation.unwrap();
        assert_eq!(
            committed_generation.content_epoch,
            original_generation.content_epoch
        );
        assert_eq!(
            committed_generation.revision,
            original_generation.revision + 1
        );
    }

    /// A 4x10 terminal holding about 100 lines of scrollback, and its checkpoint.
    fn checkpoint_with_scrollback() -> TerminalCheckpointV3 {
        let mut live = Terminal::new(
            TerminalSize {
                rows: 4,
                cols: 10,
                pixel_width: 100,
                pixel_height: 80,
                dpi: 96,
            },
            Arc::new(RichCheckpointTestConfig),
            "FrankenTerm",
            "history-budget",
            Box::new(std::io::sink()),
        );
        for line in 0..104 {
            live.advance_bytes(format!("line {line:04}\r\n").as_bytes());
        }
        let checkpoint =
            TerminalCheckpointV3::capture_with_limits(&live, TerminalCheckpointLimits::default())
                .unwrap();
        assert!(checkpoint.primary_screen.lines.len() > 100);
        checkpoint
    }

    /// Default limits with a retained-memory budget of `retained` bytes.
    fn limits_with_retained_budget(retained: usize) -> TerminalCheckpointLimits {
        TerminalCheckpointLimits {
            max_retained_capture_bytes: retained,
            max_replay_actions_per_record: retained
                / std::mem::size_of::<frankenterm_escape_parser::Action>(),
            ..TerminalCheckpointLimits::default()
        }
    }

    #[test]
    fn encoder_refuses_a_payload_its_decoder_would_refuse() {
        let checkpoint = checkpoint_with_scrollback();
        let full = checkpoint
            .to_canonical_json(TerminalCheckpointLimits::default())
            .unwrap();
        let charged =
            json_structural_retained_bytes(&full, TerminalCheckpointLimits::default()).unwrap();
        let tight = limits_with_retained_budget(charged / 2);
        tight.validate_policy().unwrap();

        // The decoder's structural pass refuses the full payload, and the
        // encoder now says so too, with the same typed limit, instead of
        // emitting a recovery image nothing can decode.
        assert!(matches!(
            TerminalCheckpointV3::decode_canonical_json(&full, tight),
            Err(TerminalCheckpointError::ResourceLimit {
                resource: "json_structural_bytes",
                ..
            })
        ));
        assert!(matches!(
            checkpoint.to_canonical_json(tight),
            Err(TerminalCheckpointError::ResourceLimit {
                resource: "json_structural_bytes",
                ..
            })
        ));
    }

    #[test]
    fn live_capture_drops_only_the_oldest_history_until_its_decoder_admits_it() {
        let checkpoint = checkpoint_with_scrollback();
        let full = checkpoint
            .to_canonical_json(TerminalCheckpointLimits::default())
            .unwrap();
        let charged =
            json_structural_retained_bytes(&full, TerminalCheckpointLimits::default()).unwrap();
        let tight = limits_with_retained_budget(charged / 2);

        let mut trimmed = checkpoint.clone();
        let payload = trimmed.encode_trimming_history(tight).unwrap();
        let before = &checkpoint.primary_screen;
        let after = &trimmed.primary_screen;
        let dropped = before.lines.len() - after.lines.len();
        assert!(dropped > 0, "an over-budget capture must give up history");
        assert!(
            after.lines.len() > before.physical_rows as usize,
            "the newest history is kept"
        );
        // Exactly the oldest lines go; the newest stay, in order.
        assert_eq!(after.lines[..], before.lines[dropped..]);
        assert_eq!(
            after.stable_row_index_offset,
            before.stable_row_index_offset + dropped as u64
        );
        // It fits with the 1/8 headroom, and its decoder admits it.
        let fitted = json_structural_retained_bytes(&payload, tight).unwrap();
        assert!(fitted <= tight.max_retained_capture_bytes - tight.max_retained_capture_bytes / 8);
        let restored = TerminalCheckpointV3::decode_canonical_json(&payload, tight)
            .unwrap()
            .restore_inert(Arc::new(RichCheckpointTestConfig))
            .unwrap()
            .into_live(Box::new(std::io::sink()))
            .unwrap();
        let recaptured = TerminalCheckpointV3::capture_with_limits(
            &restored,
            TerminalCheckpointLimits::default(),
        )
        .unwrap();
        assert_eq!(recaptured.primary_screen.lines, after.lines);
        assert_eq!(
            recaptured.primary_screen.stable_row_index_offset,
            after.stable_row_index_offset
        );

        // A capture within budget is left whole.
        let mut whole = checkpoint.clone();
        whole
            .encode_trimming_history(TerminalCheckpointLimits::default())
            .unwrap();
        assert!(whole == checkpoint);
    }

    #[test]
    fn cold_seam_checkpoint_rejects_replaced_source_and_preserves_retryable_text() {
        use crate::config::ScrollbackSpillSink;

        let (mut live, sink) = crate::screen::tests::cold_seam_test_terminal();
        let limits = TerminalCheckpointLimits::default();
        let config = live.get_config();
        let mut prepared = live
            .screen()
            .capture_cold_seam_reflow()
            .unwrap()
            .expect("fixture must exercise a real cold/resident seam")
            .hydrate(|| false)
            .unwrap();
        assert!(prepared.is_ready());
        live.increment_seqno();
        let seqno = live.current_seqno();
        assert!(live
            .screen_mut()
            .install_cold_seam_reflow(&mut prepared, seqno)
            .unwrap());
        let canonical = TerminalCheckpointV3::capture_with_limits(&live, limits)
            .unwrap()
            .to_canonical_json(limits)
            .unwrap();
        let inert = TerminalCheckpointV3::decode_canonical_json(&canonical, limits)
            .unwrap()
            .restore_inert(config)
            .unwrap();
        let oldest = sink.oldest_scrollback_row().unwrap();
        let original = sink.load_scrollback_line(oldest).unwrap();
        assert!(sink.store_scrollback_line(oldest, &original, 32));
        assert!(TerminalCheckpointV3::capture_with_limits(&live, limits).is_err());
        let failure = match inert.into_live(Box::new(std::io::sink())) {
            Ok(_) => panic!("stale cold generation cannot activate replacement text"),
            Err(failure) => failure,
        };
        assert_eq!(
            failure.error(),
            &crate::InertTerminalError::ScrollbackActivation(
                crate::config::ScrollbackActivationError::Spill(
                    crate::config::ScrollbackSpillError::SnapshotGenerationMismatch,
                ),
            )
        );
        let (_, recovered) = failure.into_parts();
        assert_eq!(
            recovered
                .checkpoint()
                .unwrap()
                .to_canonical_json(limits)
                .unwrap(),
            canonical,
            "failed activation retains the complete canonical model"
        );
        assert_eq!(sink.load_scrollback_line(oldest).unwrap(), original);
    }

    /// A guardian restore replays every output record after its checkpoint,
    /// so a row a dead writer spilled after the checkpoint is already in the
    /// model: activation supersedes the advanced store with the model's
    /// exact cold prefix, keeping neither the stale row nor a duplicate.
    #[test]
    fn cold_seam_checkpoint_supersedes_a_store_its_replay_cleared() {
        use crate::config::ScrollbackSpillSink;

        let (mut live, sink) = crate::screen::tests::cold_seam_test_terminal();
        let limits = TerminalCheckpointLimits::default();
        let config = live.get_config();
        let canonical = TerminalCheckpointV3::capture_with_limits(&live, limits)
            .unwrap()
            .to_canonical_json(limits)
            .unwrap();
        let mut inert = TerminalCheckpointV3::decode_canonical_json(&canonical, limits)
            .unwrap()
            .restore_inert(config)
            .unwrap();
        assert!(sink.retained_scrollback_rows() > 0);
        // The writer printed ESC[3J after the checkpoint and committed that
        // clear, which moves a real store to a content epoch the restored
        // model cannot name; replaying the same bytes tells activation so.
        live.advance_bytes(b"\x1b[3J");
        assert_eq!(sink.retained_scrollback_rows(), 0);
        inert.replay_bytes(b"\x1b[3J").unwrap();
        let model = inert.checkpoint().unwrap();
        inert.supersede_cold_scrollback_successors();
        let activated = match inert.into_live(Box::new(std::io::sink())) {
            Ok(activated) => activated,
            Err(failure) => panic!("superseding activation failed: {:?}", failure.error()),
        };
        assert!(sink
            .last_supersede_cleared
            .load(std::sync::atomic::Ordering::Relaxed));
        let restored = TerminalCheckpointV3::capture_with_limits(&activated, limits).unwrap();
        assert_eq!(restored.primary_screen.lines, model.primary_screen.lines);
    }

    #[test]
    fn scrollback_ancestry_admits_only_its_lineage() {
        use crate::config::{ScrollbackAncestry, ScrollbackSnapshotGeneration};

        let epoch = [7; 16];
        let other = [9; 16];
        let ancestor = ScrollbackSnapshotGeneration::new(epoch, 5);
        let strict = ScrollbackAncestry {
            ancestor,
            cleared_since: false,
        };
        let cleared = ScrollbackAncestry {
            cleared_since: true,
            ..strict
        };
        let generation = ScrollbackSnapshotGeneration::new;
        // The ancestor's epoch: no older than the ancestor, either way.
        for ancestry in [strict, cleared] {
            assert!(ancestry.admits(generation(epoch, 5)));
            assert!(ancestry.admits(generation(epoch, 9)));
            assert!(!ancestry.admits(generation(epoch, 4)));
            assert!(ancestry.admits_receipt(generation(epoch, 6)));
            assert!(!ancestry.admits_receipt(generation(epoch, 5)));
        }
        // Another epoch only after a replayed clear, and a receipt there is a
        // published revision.
        assert!(!strict.admits(generation(other, 0)));
        assert!(!strict.admits_receipt(generation(other, 1)));
        assert!(cleared.admits(generation(other, 0)));
        assert!(cleared.admits_receipt(generation(other, 1)));
        assert!(!cleared.admits_receipt(generation(other, 0)));
    }

    #[test]
    fn cold_seam_checkpoint_supersedes_an_advanced_store_when_asked() {
        use crate::config::ScrollbackSpillSink;

        let (mut live, sink) = crate::screen::tests::cold_seam_test_terminal();
        let limits = TerminalCheckpointLimits::default();
        let config = live.get_config();
        let mut prepared = live
            .screen()
            .capture_cold_seam_reflow()
            .unwrap()
            .expect("fixture must exercise a real cold/resident seam")
            .hydrate(|| false)
            .unwrap();
        assert!(prepared.is_ready());
        live.increment_seqno();
        let seqno = live.current_seqno();
        assert!(live
            .screen_mut()
            .install_cold_seam_reflow(&mut prepared, seqno)
            .unwrap());
        let canonical = TerminalCheckpointV3::capture_with_limits(&live, limits)
            .unwrap()
            .to_canonical_json(limits)
            .unwrap();
        let inert = TerminalCheckpointV3::decode_canonical_json(&canonical, limits)
            .unwrap()
            .restore_inert(config)
            .unwrap();
        let oldest = sink.oldest_scrollback_row().unwrap();
        let original = sink.load_scrollback_line(oldest).unwrap();
        // The dead writer's last spill: one more row past the checkpoint's
        // cold prefix, after the checkpoint pinned its generation.
        let stale_tail =
            oldest + crate::StableRowIndex::try_from(sink.retained_scrollback_rows()).unwrap();
        assert!(sink.store_scrollback_line(stale_tail, &original, 32));

        let failure = match inert.into_live(Box::new(std::io::sink())) {
            Ok(_) => panic!("strict activation must refuse an advanced cold store"),
            Err(failure) => failure,
        };
        assert_eq!(
            failure.error(),
            &crate::InertTerminalError::ScrollbackActivation(
                crate::config::ScrollbackActivationError::Spill(
                    crate::config::ScrollbackSpillError::SnapshotGenerationMismatch,
                ),
            )
        );
        let (_, mut recovered) = failure.into_parts();
        let model = recovered.checkpoint().unwrap();
        let ancestor = model.primary_screen.cold_snapshot_generation.unwrap();
        recovered.supersede_cold_scrollback_successors();
        let activated = match recovered.into_live(Box::new(std::io::sink())) {
            Ok(activated) => activated,
            Err(failure) => panic!("superseding activation failed: {:?}", failure.error()),
        };

        let first_resident = activated.screen().phys_to_stable_row_index(0);
        assert_eq!(sink.oldest_scrollback_row(), Some(oldest));
        assert_eq!(
            sink.retained_scrollback_rows(),
            usize::try_from(first_resident - oldest).unwrap(),
            "the store holds exactly the published prefix, contiguous and without duplicates"
        );
        assert!(
            sink.load_scrollback_line(first_resident).is_none(),
            "no stored row survives past the published prefix"
        );
        // Reading the cold rows back from the store yields the model's rows.
        let after = TerminalCheckpointV3::capture_with_limits(&activated, limits).unwrap();
        assert_eq!(after.primary_screen.lines, model.primary_screen.lines);
        let committed = after.primary_screen.cold_snapshot_generation.unwrap();
        assert_eq!(committed.content_epoch, ancestor.content_epoch);
        assert!(
            committed.revision > ancestor.revision + 1,
            "the successor advances the store's generation, not the checkpoint's"
        );
    }

    /// A terminal using every part of the schema: a resize, both screens,
    /// a hyperlink, saved cursors, user vars, a palette, a custom width map.
    fn rich_terminal() -> Terminal {
        let config: Arc<dyn TerminalConfiguration + Send + Sync> =
            Arc::new(RichCheckpointTestConfig);
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
        terminal
            .unicode_version_stack
            .push(UnicodeVersionStackEntry {
                vers: prior_unicode_version,
                label: Some("prior-unicode".into()),
            });
        terminal.accumulating_title = Some("pending-title-fragment".into());
        terminal
    }

    #[test]
    fn canonical_restore_roundtrips_complete_semantic_projection() {
        let terminal = rich_terminal();
        let limits = TerminalCheckpointLimits::default();
        let before = TerminalCheckpointV3::capture_with_limits(&terminal, limits)
            .expect("capture rich terminal");
        let canonical = before
            .to_canonical_json(limits)
            .expect("encode canonical rich terminal");
        let validated = TerminalCheckpointV3::decode_canonical_json(&canonical, limits)
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

    fn runs_limits() -> TerminalCheckpointLimits {
        TerminalCheckpointLimits {
            line_encoding: TerminalCheckpointLineEncoding::Runs,
            ..TerminalCheckpointLimits::default()
        }
    }

    /// Captures `terminal` in both encodings, restores each payload, and
    /// captures every restored terminal in both encodings again: each must
    /// give back exactly what capturing `terminal` in that encoding gave, so
    /// either payload restores the same terminal. Returns both payloads.
    fn assert_both_encodings_restore_alike(
        terminal: &Terminal,
        config: fn() -> Arc<dyn TerminalConfiguration + Send + Sync>,
    ) -> (Zeroizing<Vec<u8>>, Zeroizing<Vec<u8>>) {
        let cells_limits = TerminalCheckpointLimits::default();
        let runs_limits = runs_limits();
        let encode = |limits: TerminalCheckpointLimits| {
            TerminalCheckpointV3::capture_with_limits(terminal, limits)
                .expect("capture")
                .to_canonical_json(limits)
                .expect("encode")
        };
        let cells = encode(cells_limits);
        let runs = encode(runs_limits);
        assert!(cells.starts_with(b"{\"version\":3,"));
        assert!(runs.starts_with(b"{\"version\":4,"));
        for payload in [&cells, &runs] {
            let decoded = TerminalCheckpointV3::decode_canonical_json(payload, cells_limits)
                .expect("decode")
                .into_checkpoint();
            for (limits, expected) in [(cells_limits, &cells), (runs_limits, &runs)] {
                let restored = ValidatedTerminalCheckpointV3 {
                    checkpoint: decoded.clone(),
                    limits,
                }
                .restore_inert(config())
                .expect("restore");
                let again = restored
                    .checkpoint()
                    .expect("capture the restored terminal")
                    .to_canonical_json(limits)
                    .expect("encode the restored terminal");
                assert!(
                    again == *expected,
                    "a version {} payload restored and captured as {:?} differs",
                    decoded.version,
                    limits.line_encoding,
                );
            }
        }
        (cells, runs)
    }

    fn plain_config() -> Arc<dyn TerminalConfiguration + Send + Sync> {
        Arc::new(CheckpointTestConfig)
    }

    fn rich_config() -> Arc<dyn TerminalConfiguration + Send + Sync> {
        Arc::new(RichCheckpointTestConfig)
    }

    /// Every cell shape and attribute a run holds: wide and custom-width
    /// cells, emoji sequences, combining marks, colors of every kind,
    /// hyperlinks, colored and trailing blanks, wrapped and double-width
    /// rows, tabs, history, and an active alternate screen.
    const MIXED_CELLS: &str = concat!(
        "plain ascii\r\n",
        "\x1b[1;31mbold red\x1b[0m normal \x1b[38;2;10;20;30mtruecolor\x1b[0m\r\n",
        "wide \u{65e5}\u{672c} crab \u{1f980} thumb \u{1f44d}\u{1f3fd} family ",
        "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} flag \u{1f1fa}\u{1f1f8} heart \u{2764}\u{fe0f}\r\n",
        "combining e\u{301}a\u{308} x-wide xx\r\n",
        "\x1b[4:3m\x1b[58;2;1;2;3mcurly\x1b[0m \x1b[7mreverse\x1b[0m \x1b[9mstrike\x1b[0m\r\n",
        "\x1b[44m   \x1b[0m colored blanks, trailing spaces    \r\n",
        "\x1b#6double width\r\n",
        "tab\tseparated\tcells\r\n",
        "\x1b]8;id=a;https://example.invalid/a\x07link\x1b]8;;\x07 after\r\n",
    );

    #[test]
    fn both_line_encodings_restore_the_same_terminal() {
        let mut mixed = terminal();
        mixed.advance_bytes(MIXED_CELLS.as_bytes());
        mixed.advance_bytes([b'w'; 200]);
        mixed.advance_bytes(b"\r\n");
        for tick in 0..40 {
            mixed.advance_bytes(format!("tick {tick}\r\n").as_bytes());
        }
        assert_both_encodings_restore_alike(&mixed, plain_config);

        mixed.advance_bytes(b"\x1b[?1049h\x1b[32malt \xe6\xbc\xa2 green\x1b[0m\r\nsecond row");
        assert_both_encodings_restore_alike(&mixed, plain_config);

        let mut rich = rich_terminal();
        rich.advance_bytes(MIXED_CELLS.as_bytes());
        assert_both_encodings_restore_alike(&rich, rich_config);
        assert_both_encodings_restore_alike(&terminal(), plain_config);
    }

    /// The payload a pane of short log lines makes: runs carry each line's
    /// text once and each attribute set once per screen.
    #[test]
    fn runs_make_a_small_fraction_of_the_cells_payload() {
        let mut ticks = Terminal::new(
            TerminalSize {
                rows: 80,
                cols: 120,
                pixel_width: 960,
                pixel_height: 1280,
                dpi: 96,
            },
            Arc::new(CheckpointTestConfig),
            "FrankenTerm",
            "checkpoint-test",
            Box::new(Vec::<u8>::new()),
        );
        for tick in 0..500 {
            ticks.advance_bytes(
                format!("\x1b[32m{tick:>6}\x1b[0m tick at \x1b[1msome time\x1b[0m done\r\n")
                    .as_bytes(),
            );
        }
        let (cells, runs) = assert_both_encodings_restore_alike(&ticks, plain_config);
        assert!(
            runs.len() * 20 < cells.len(),
            "runs {} bytes, cells {} bytes",
            runs.len(),
            cells.len()
        );
        let checkpoint = TerminalCheckpointV3::decode_canonical_json(&runs, runs_limits())
            .expect("decode runs")
            .into_checkpoint();
        assert_eq!(
            checkpoint
                .primary_screen
                .attributes
                .as_ref()
                .map(Vec::len),
            Some(3),
            "default, green and bold, each once"
        );
        assert_eq!(
            TerminalCheckpointV3::canonical_payload_version(&runs),
            Some(COMPACT_TERMINAL_CHECKPOINT_VERSION)
        );
        assert_eq!(
            TerminalCheckpointV3::canonical_payload_version(&cells),
            Some(TERMINAL_CHECKPOINT_VERSION)
        );
        assert_eq!(
            TerminalCheckpointV3::canonical_payload_version(b"{\"version\":5,"),
            None
        );
        assert_eq!(TerminalCheckpointV3::canonical_payload_version(b"{}"), None);
    }

    fn run_of(checkpoint: &mut TerminalCheckpointV3, line: usize) -> &mut CheckpointRun {
        &mut checkpoint.primary_screen.lines[line]
            .runs
            .as_mut()
            .expect("version 4 runs")[0]
    }

    #[test]
    fn decoder_refuses_runs_that_misstate_their_cells() {
        let mut terminal = terminal();
        terminal.advance_bytes("plain\r\nwide \u{65e5}\u{672c} e\u{301}".as_bytes());
        let checkpoint = TerminalCheckpointV3::capture_with_limits(&terminal, runs_limits())
            .expect("capture runs");
        assert!(run_of(&mut checkpoint.clone(), 0).clusters.is_none());
        assert!(run_of(&mut checkpoint.clone(), 1).clusters.is_some());
        let table_len = checkpoint
            .primary_screen
            .attributes
            .as_ref()
            .map_or(0, Vec::len);

        let cases: Vec<(&str, Box<dyn Fn(&mut TerminalCheckpointV3)>)> = vec![
            (
                "run.attributes",
                Box::new(move |c| run_of(c, 0).attributes = table_len as u32),
            ),
            ("run.text", Box::new(|c| run_of(c, 0).text.clear())),
            ("run.text", Box::new(|c| run_of(c, 0).text.push('\u{7}'))),
            (
                "run.clusters",
                Box::new(|c| run_of(c, 0).clusters = Some(vec![(1, 1); 5])),
            ),
            (
                "run.clusters",
                Box::new(|c| {
                    run_of(c, 1).clusters.as_mut().unwrap().pop();
                }),
            ),
            (
                "run.clusters",
                Box::new(|c| run_of(c, 1).clusters.as_mut().unwrap().push((1, 1))),
            ),
            (
                "run.clusters",
                Box::new(|c| run_of(c, 1).clusters.as_mut().unwrap()[0] = (0, 1)),
            ),
            (
                "cell.width",
                Box::new(|c| run_of(c, 1).clusters.as_mut().unwrap()[0] = (1, 3)),
            ),
            (
                "screen.attributes",
                Box::new(|c| c.primary_screen.attributes = None),
            ),
            ("screen.attributes", Box::new(|c| c.version = 3)),
            (
                "line.cells",
                Box::new(|c| {
                    let line = &mut c.primary_screen.lines[0];
                    line.runs = None;
                    line.cells = Some(Vec::new());
                }),
            ),
        ];
        let limits = TerminalCheckpointLimits::default();
        for (field, damage) in cases {
            let mut damaged = checkpoint.clone();
            damage(&mut damaged);
            let bytes = serde_json::to_vec(&damaged).expect("encode damaged checkpoint");
            match TerminalCheckpointV3::decode_canonical_json(&bytes, limits) {
                Err(TerminalCheckpointError::InvalidField { field: refused, .. })
                    if refused == field => {}
                other => panic!("expected {} refused, got {:?}", field, other),
            }
        }
        let undamaged = serde_json::to_vec(&checkpoint).expect("encode checkpoint");
        TerminalCheckpointV3::decode_canonical_json(&undamaged, limits).expect("decode runs");
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

        /// Any output, in a small pane whose history scrolls, restores alike
        /// from either encoding.
        #[test]
        fn any_output_restores_alike_from_either_encoding(
            pieces in proptest::collection::vec(proptest::sample::select(vec![
                "a", "word ", "  ", "\u{65e5}", "\u{1f980}", "e\u{301}", "\u{2764}\u{fe0f}",
                "\r\n", "\r", "\t", "\x1b[31m", "\x1b[1;4m", "\x1b[38;2;1;2;3m", "\x1b[44m",
                "\x1b[0m", "\x1b[7m", "\x1b[2K", "\x1b[H", "\x1b[3;5H", "\x1b#6",
                "\x1b]8;;https://example.invalid\x07", "\x1b]8;;\x07", "\x1b[?1049h",
                "\x1b[?1049l", "0123456789",
            ]), 0..160)
        ) {
            let mut small = Terminal::new(
                TerminalSize {
                    rows: 5,
                    cols: 12,
                    pixel_width: 120,
                    pixel_height: 100,
                    dpi: 96,
                },
                Arc::new(CheckpointTestConfig),
                "FrankenTerm",
                "checkpoint-proptest",
                Box::new(Vec::<u8>::new()),
            );
            small.advance_bytes(pieces.concat().as_bytes());
            assert_both_encodings_restore_alike(&small, plain_config);
        }
    }

    #[test]
    fn saved_pending_wrap_checkpoint_preserves_resize_then_typing() {
        let limits = TerminalCheckpointLimits::default();
        let config: Arc<dyn TerminalConfiguration + Send + Sync> =
            Arc::new(RichCheckpointTestConfig);
        let size = TerminalSize {
            rows: 4,
            cols: 10,
            pixel_width: 100,
            pixel_height: 80,
            dpi: 96,
        };
        let mut live = Terminal::new(
            size,
            Arc::clone(&config),
            "FrankenTerm",
            "saved-wrap-checkpoint",
            Box::new(std::io::sink()),
        );
        live.advance_bytes(b"\x1b[?69h\x1b[3;6sABCD\x1b[?1049h\x1b[2;9s");
        let checkpoint = TerminalCheckpointV3::capture_with_limits(&live, limits).unwrap();
        assert_eq!(checkpoint.version, 3);
        assert_eq!(
            checkpoint
                .primary_screen
                .saved_cursor
                .as_ref()
                .unwrap()
                .wrap_next_column,
            Some(6)
        );
        let canonical = checkpoint.to_canonical_json(limits).unwrap();
        let inert = TerminalCheckpointV3::decode_canonical_json(&canonical, limits)
            .unwrap()
            .restore_inert(config)
            .unwrap();
        let mut restored = inert.into_live(Box::new(std::io::sink())).unwrap();
        restored.resize(TerminalSize {
            cols: 12,
            pixel_width: 120,
            ..size
        });
        restored.advance_bytes(b"\x1b[?1049lZ");
        let text: Vec<String> = restored
            .screen()
            .all_lines()
            .iter()
            .map(|line| line.as_str().trim_end_matches(' ').to_owned())
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(text, vec!["  ABCDZ"]);

        for endpoint in [None, Some(0), Some(5), Some(limits.max_cols as u64 + 1)] {
            let mut invalid = checkpoint.clone();
            invalid
                .primary_screen
                .saved_cursor
                .as_mut()
                .unwrap()
                .wrap_next_column = endpoint;
            assert!(
                invalid.to_canonical_json(limits).is_err(),
                "endpoint={:?}",
                endpoint
            );
        }
        let mut old_version = checkpoint;
        old_version.version = 2;
        assert!(old_version.to_canonical_json(limits).is_err());
    }

    #[test]
    fn writer_preparation_failure_returns_the_complete_retryable_inert_model() {
        let limits = TerminalCheckpointLimits::default();
        let config: Arc<dyn TerminalConfiguration + Send + Sync> =
            Arc::new(RichCheckpointTestConfig);
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
        let checkpoint = TerminalCheckpointV3::capture_with_limits(&live, limits)
            .expect("capture writer-preparation fixture");
        let canonical = checkpoint
            .to_canonical_json(limits)
            .expect("encode writer-preparation fixture");
        let mut inert = TerminalCheckpointV3::decode_canonical_json(&canonical, limits)
            .expect("decode writer-preparation fixture")
            .restore_inert(config)
            .expect("restore writer-preparation fixture");
        inert.force_writer_preparation_failure_for_test();

        let failure = match inert.into_live(Box::new(Vec::<u8>::new())) {
            Ok(_) => panic!("forced writer preparation must fail"),
            Err(failure) => failure,
        };
        assert_eq!(
            failure.error(),
            &crate::InertTerminalError::WriterActivation
        );
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
        let checkpoint = TerminalCheckpointV3::capture_with_limits(&live, limits)
            .expect("capture resident-only recovery fixture");
        let canonical = checkpoint
            .to_canonical_json(limits)
            .expect("encode resident-only recovery fixture");
        let activation_config: Arc<dyn TerminalConfiguration> =
            Arc::new(ReplacementActivationConfig {
                tier_enabled: true,
                sink: Arc::clone(&sink),
            });
        let inert = TerminalCheckpointV3::decode_canonical_json(&canonical, limits)
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
        let checkpoint = TerminalCheckpointV3::capture_with_limits(&terminal(), limits)
            .expect("capture fixture");
        let canonical = checkpoint
            .to_canonical_json(limits)
            .expect("encode fixture");

        let mut unknown_version = canonical.clone();
        let version = b"{\"version\":3";
        assert!(unknown_version.starts_with(version));
        unknown_version[version.len() - 1] = b'5';
        assert!(matches!(
            TerminalCheckpointV3::decode_canonical_json(&unknown_version, limits),
            Err(TerminalCheckpointError::UnsupportedVersion { observed: 5, .. })
        ));
        // Version 4 names the encoding, which these cells are not.
        let mut relabelled = canonical.clone();
        relabelled[version.len() - 1] = b'4';
        assert!(matches!(
            TerminalCheckpointV3::decode_canonical_json(&relabelled, limits),
            Err(TerminalCheckpointError::InvalidField {
                field: "screen.attributes",
                ..
            })
        ));

        // Mutate the canonical bytes in place so this negative reaches typed
        // missing-field validation. Round-tripping through `Value` sorts map
        // keys when serde_json's preserve-order feature is disabled, which
        // would reject at the earlier canonical-version-prefix fence and fail
        // to exercise the intended contract.
        let encoded_title =
            serde_json::to_vec(&checkpoint.title).expect("encode fixture title value");
        let mut title_field = b",\"title\":".to_vec();
        title_field.extend_from_slice(&encoded_title);
        let title_offset = canonical
            .windows(title_field.len())
            .position(|window| window == title_field)
            .expect("find the exact canonical title field");
        let mut omitted = canonical.clone();
        omitted.drain(title_offset..title_offset + title_field.len());
        assert!(matches!(
            TerminalCheckpointV3::decode_canonical_json(&omitted, limits),
            Err(TerminalCheckpointError::Serialization)
        ));

        let final_brace = canonical
            .len()
            .checked_sub(1)
            .expect("canonical checkpoint is nonempty");
        assert_eq!(canonical[final_brace], b'}');
        let mut extra = canonical[..final_brace].to_vec();
        extra.extend_from_slice(b",\"unexpected\":true}");
        assert!(matches!(
            TerminalCheckpointV3::decode_canonical_json(&extra, limits),
            Err(TerminalCheckpointError::Serialization)
        ));
    }

    fn historical_v2_wire(mut checkpoint: TerminalCheckpointV3) -> Vec<u8> {
        // Construct the historical typed wire, rather than rewriting a V3 JSON
        // version or treating the resulting data as an authenticated artifact.
        checkpoint.version = 2;
        serde_json::to_vec(&HistoricalTerminalRef(&checkpoint)).unwrap()
    }

    #[test]
    fn historical_v2_preparation_preserves_source_and_all_common_state() {
        let limits = TerminalCheckpointLimits::default();
        let mut live = terminal_with_distinct_custom_width_maps(false);
        live.advance_bytes(b"primary\x1b]0;historical-title\x07\x1b]8;id=old;https://example.invalid/v2\x07link\x1b]8;;\x07\x1b7");
        live.advance_bytes(b"\x1b[?1049h\x1b[31;4malternate\x1b7");
        live.user_vars
            .insert("historical".into(), "retained".into());
        live.progress = Progress::Percentage(67);
        live.accumulating_title = Some("pending-fragment".into());
        let expected = TerminalCheckpointV3::capture_with_limits(&live, limits).unwrap();
        assert!(expected.primary_screen.saved_cursor.is_some());
        assert!(expected.alternate_screen.saved_cursor.is_some());
        let source = historical_v2_wire(expected.clone());
        assert!(source.starts_with(b"{\"version\":2,"));
        assert!(!source
            .windows(b"wrap_next_column".len())
            .any(|part| part == b"wrap_next_column"));
        assert!(TerminalCheckpointV3::decode_canonical_json(&source, limits).is_err());
        let prepared =
            PreparedTerminalCheckpointV2Migration::decode_canonical_json(&source, limits).unwrap();
        assert_eq!(prepared.source_canonical_payload(), source);
        assert_eq!(prepared.rows(), expected.primary_screen.physical_rows);
        assert_eq!(prepared.cols(), expected.primary_screen.physical_cols);
        assert_eq!(prepared.pixel_width(), expected.pixel_width);
        assert_eq!(prepared.pixel_height(), expected.pixel_height);
        let target = TerminalCheckpointV3::decode_canonical_json(
            prepared.target_canonical_payload(),
            limits,
        )
        .unwrap();
        assert_eq!(
            target.checkpoint(),
            &expected,
            "every common field must survive projection"
        );
        assert_eq!(
            prepared.target_canonical_payload(),
            expected.to_canonical_json(limits).unwrap().as_slice()
        );
        assert!(
            prepared.source.capacity() + prepared.target.capacity()
                <= limits.max_retained_capture_bytes
        );
    }

    #[test]
    fn historical_v2_preparation_rejects_ambiguous_saved_wrap_on_either_screen() {
        let limits = TerminalCheckpointLimits::default();
        let mut live = terminal();
        live.advance_bytes(b"primary\x1b7\x1b[?1049hsecondary\x1b7");
        let checkpoint = TerminalCheckpointV3::capture_with_limits(&live, limits).unwrap();
        for alternate in [false, true] {
            let mut ambiguous = checkpoint.clone();
            let screen = if alternate {
                &mut ambiguous.alternate_screen
            } else {
                &mut ambiguous.primary_screen
            };
            let saved = screen.saved_cursor.as_mut().unwrap();
            saved.wrap_next = true;
            saved.wrap_next_column = Some(saved.position.x + 1);
            let source = historical_v2_wire(ambiguous);
            assert!(matches!(
                PreparedTerminalCheckpointV2Migration::decode_canonical_json(&source, limits),
                Err(TerminalCheckpointError::InvalidField {
                    field: "migration.v2.saved_cursor.wrap_next",
                    ..
                })
            ));
        }
    }

    #[test]
    fn historical_v2_preparation_requires_exact_wire_and_joint_memory_budget() {
        let limits = TerminalCheckpointLimits::default();
        let mut live = terminal();
        live.advance_bytes(b"saved\x1b7");
        let checkpoint = TerminalCheckpointV3::capture_with_limits(&live, limits).unwrap();
        let source = historical_v2_wire(checkpoint.clone());
        let mut unknown = source[..source.len() - 1].to_vec();
        unknown.extend_from_slice(b",\"unexpected\":true}");
        let mut whitespace = source.clone();
        whitespace.push(b' ');
        let mut omitted = source.clone();
        let field = b"\"wrap_next\":false,";
        let offset = omitted
            .windows(field.len())
            .position(|part| part == field)
            .unwrap();
        omitted.drain(offset..offset + field.len());
        let mut new_cursor_field = source.clone();
        let offset = new_cursor_field
            .windows(field.len())
            .position(|part| part == field)
            .unwrap()
            + field.len();
        new_cursor_field.splice(
            offset..offset,
            b"\"wrap_next_column\":null,".iter().copied(),
        );
        for invalid in [
            unknown,
            whitespace,
            omitted,
            new_cursor_field,
            checkpoint.to_canonical_json(limits).unwrap().to_vec(),
        ] {
            assert!(
                PreparedTerminalCheckpointV2Migration::decode_canonical_json(&invalid, limits)
                    .is_err()
            );
        }
        let prepared =
            PreparedTerminalCheckpointV2Migration::decode_canonical_json(&source, limits).unwrap();
        let mut budget = JsonStructuralBudget::new(limits);
        let mut decoder = serde_json::Deserializer::from_slice(&source);
        JsonStructuralSeed {
            budget: &mut budget,
            depth: 0,
        }
        .deserialize(&mut decoder)
        .unwrap();
        let joint_limit =
            budget.retained_bytes + prepared.source.capacity() + prepared.target.len() - 1;
        assert!(joint_limit > source.len());
        assert!(
            PreparedTerminalCheckpointV2Migration::decode_canonical_json(
                &source,
                TerminalCheckpointLimits {
                    max_retained_capture_bytes: joint_limit,
                    ..limits
                }
            )
            .is_err(),
            "individually fitting buffers must not evade the joint retained limit"
        );
        assert!(
            PreparedTerminalCheckpointV2Migration::decode_canonical_json(
                &source,
                TerminalCheckpointLimits {
                    max_encoded_bytes: source.len() - 1,
                    ..limits
                }
            )
            .is_err()
        );
    }

    #[test]
    fn capture_rejects_resource_policy_below_live_geometry() {
        let limits = TerminalCheckpointLimits {
            max_rows: 1,
            ..TerminalCheckpointLimits::default()
        };

        assert!(matches!(
            TerminalCheckpointV3::capture_with_limits(&terminal(), limits),
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
        terminal
            .unicode_version_stack
            .push(UnicodeVersionStackEntry {
                vers: prior_unicode_version,
                label: Some("retained".into()),
            });
        terminal.accumulating_title = Some("retained-title".into());
        let lowered: Arc<dyn TerminalConfiguration> = Arc::new(LoweredCheckpointTestConfig);
        terminal.set_config(Arc::clone(&lowered));

        let limits = TerminalCheckpointLimits::default();
        let checkpoint = TerminalCheckpointV3::capture_with_limits(&terminal, limits)
            .expect("capture state retained across a config-limit decrease");
        let canonical = checkpoint
            .to_canonical_json(limits)
            .expect("encode lowered-config checkpoint");
        let inert = TerminalCheckpointV3::decode_canonical_json(&canonical, limits)
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

    #[derive(Debug)]
    struct StagedCaptureTestSink {
        identity: std::sync::Mutex<crate::config::ScrollbackIntervalIdentity>,
        rows: std::sync::Mutex<Vec<(StableRowIndex, Line)>>,
        generation: std::sync::Mutex<crate::config::ScrollbackSnapshotGeneration>,
    }

    impl StagedCaptureTestSink {
        fn new() -> Self {
            Self {
                identity: std::sync::Mutex::new(
                    crate::config::ScrollbackIntervalIdentity::default(),
                ),
                rows: std::sync::Mutex::new(Vec::new()),
                generation: std::sync::Mutex::new(
                    crate::config::ScrollbackSnapshotGeneration::new([1; 16], 1),
                ),
            }
        }

        fn advance_identity(&self) {
            *self.identity.lock().unwrap() = crate::config::ScrollbackIntervalIdentity::default();
        }
    }

    impl crate::config::ScrollbackSpillSink for StagedCaptureTestSink {
        fn store_scrollback_line(
            &self,
            stable_row: StableRowIndex,
            line: &Line,
            _max_retained_rows: usize,
        ) -> bool {
            self.rows.lock().unwrap().push((stable_row, line.clone()));
            true
        }

        fn load_scrollback_line(&self, stable_row: StableRowIndex) -> Option<Line> {
            self.rows
                .lock()
                .unwrap()
                .iter()
                .find(|(r, _)| *r == stable_row)
                .map(|(_, l)| l.clone())
        }

        fn oldest_scrollback_row(&self) -> Option<StableRowIndex> {
            self.rows.lock().unwrap().first().map(|(r, _)| *r)
        }

        fn retained_scrollback_rows(&self) -> usize {
            self.rows.lock().unwrap().len()
        }

        fn retained_scrollback_bytes(&self) -> usize {
            self.rows.lock().unwrap().len() * 80
        }

        fn try_capture_scrollback_interval(&self) -> crate::config::ScrollbackIntervalCapture {
            let rows = self.rows.lock().unwrap();
            let range = if rows.is_empty() {
                None
            } else {
                let first = rows.first().unwrap().0;
                let last = rows.last().unwrap().0;
                Some(first..last + 1)
            };
            self.identity.lock().unwrap().capture(range)
        }

        fn snapshot_scrollback(
            &self,
            expected_newest_exclusive: StableRowIndex,
            _limits: crate::config::ScrollbackSnapshotLimits,
        ) -> Result<crate::config::ScrollbackSnapshot, crate::config::ScrollbackSpillError>
        {
            let rows = self.rows.lock().unwrap();
            let filtered: Vec<Line> = rows
                .iter()
                .filter(|(idx, _)| *idx < expected_newest_exclusive)
                .map(|(_, line)| line.clone())
                .collect();
            let oldest = if filtered.is_empty() {
                None
            } else {
                filtered
                    .first()
                    .and_then(|_| rows.first().map(|(idx, _)| *idx))
            };
            let gen = *self.generation.lock().unwrap();
            let count = filtered.len();
            crate::config::ScrollbackSnapshot::from_contiguous_rows(
                gen,
                crate::config::ScrollbackSnapshotFidelity::ExactSemantic,
                oldest,
                expected_newest_exclusive,
                (count * 80) as u64,
                count * 80,
                filtered,
            )
        }

        fn replace_scrollback_prefix(
            &self,
            _expected_generation: Option<crate::config::ScrollbackSnapshotGeneration>,
            _prefix: crate::config::ScrollbackPrefix<'_>,
            _max_retained_rows: usize,
        ) -> Result<crate::config::ScrollbackReplaceCommit, crate::config::ScrollbackSpillError>
        {
            Err(crate::config::ScrollbackSpillError::StorageUnavailable)
        }

        fn clear_scrollback(
            &self,
        ) -> Result<crate::config::ScrollbackClearCommit, crate::config::ScrollbackSpillError>
        {
            Err(crate::config::ScrollbackSpillError::StorageUnavailable)
        }
    }

    #[derive(Debug)]
    struct StagedCaptureTestConfig {
        sink: Arc<StagedCaptureTestSink>,
    }

    impl TerminalConfiguration for StagedCaptureTestConfig {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn scrollback_size(&self) -> usize {
            1000
        }

        fn scrollback_tier_config(&self) -> crate::config::ScrollbackTierConfig {
            crate::config::ScrollbackTierConfig {
                enabled: true,
                hot_lines: 5,
                warm_max_bytes: 0,
            }
        }

        fn scrollback_spill_sink(&self) -> Option<Arc<dyn crate::config::ScrollbackSpillSink>> {
            Some(Arc::clone(&self.sink) as Arc<dyn crate::config::ScrollbackSpillSink>)
        }
    }

    #[test]
    fn test_two_phase_staged_hot_capture_and_cold_materialization() {
        let sink = Arc::new(StagedCaptureTestSink::new());
        let config: Arc<dyn TerminalConfiguration> = Arc::new(StagedCaptureTestConfig {
            sink: Arc::clone(&sink),
        });

        let mut terminal = Terminal::new(
            TerminalSize {
                rows: 10,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            },
            config,
            "FrankenTerm",
            "staged-test",
            Box::new(Vec::<u8>::new()),
        );

        terminal.advance_bytes(b"cold row 0\r\n");
        terminal.advance_bytes(b"\x1b[1mcold row 1 bold\x1b[0m\r\n");
        for i in 2..15 {
            terminal.advance_bytes(format!("resident row {i}\r\n").as_bytes());
        }
        terminal.advance_bytes("\x1b[1;31mHello \u{1F980}\x1b[0m\r\n".as_bytes());
        terminal.advance_bytes("\x1b[?2004h".as_bytes());
        terminal.advance_bytes(b"\x1b[4;6H");

        assert_eq!(terminal.screen().phys_to_stable_row_index(0), 2);
        assert_eq!(sink.rows.lock().unwrap().len(), 2);

        let limits = TerminalCheckpointLimits::default();

        // Phase 1: Under terminal lock
        let staged = TerminalCheckpointV3::capture_staged(&terminal, limits)
            .expect("capture_staged must succeed under lock");

        // Release terminal lock immediately
        drop(terminal);

        // Phase 2: Outside terminal lock
        let checkpoint = staged
            .materialize_cold_history(limits)
            .expect("materialize_cold_history must succeed outside lock");

        assert_eq!(checkpoint.primary_screen.cold_prefix_line_count, 2);
        assert!(checkpoint.primary_screen.cold_snapshot_generation.is_some());
        assert_eq!(checkpoint.primary_screen.stable_row_index_offset, 0);
        assert!(checkpoint.bracketed_paste);
        assert_eq!(checkpoint.cursor.x, 5);
        assert_eq!(checkpoint.cursor.y, 3);

        // Roundtrip fidelity verification
        let canonical = checkpoint
            .to_canonical_json(limits)
            .expect("encode canonical");
        let decoded = TerminalCheckpointV3::decode_canonical_json(&canonical, limits)
            .expect("decode canonical");
        assert_eq!(decoded.checkpoint.primary_screen.cold_prefix_line_count, 2);
        assert_eq!(
            decoded.checkpoint.primary_screen.lines.len(),
            checkpoint.primary_screen.lines.len()
        );
        assert!(decoded.checkpoint.bracketed_paste);
    }

    #[test]
    fn test_two_phase_staged_capture_rejects_stale_cold_generation() {
        let sink = Arc::new(StagedCaptureTestSink::new());
        let config: Arc<dyn TerminalConfiguration> = Arc::new(StagedCaptureTestConfig {
            sink: Arc::clone(&sink),
        });

        let mut terminal = Terminal::new(
            TerminalSize {
                rows: 10,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            },
            config,
            "FrankenTerm",
            "stale-gen-test",
            Box::new(Vec::<u8>::new()),
        );
        terminal.advance_bytes(b"cold row\r\n");
        for i in 1..15 {
            terminal.advance_bytes(format!("resident row {i}\r\n").as_bytes());
        }

        assert_eq!(terminal.screen().phys_to_stable_row_index(0), 1);
        assert_eq!(sink.rows.lock().unwrap().len(), 1);

        let limits = TerminalCheckpointLimits::default();

        // Phase 1: Capture staged (pins generation)
        let staged = TerminalCheckpointV3::capture_staged(&terminal, limits)
            .expect("staged capture succeeds");
        drop(terminal);

        // Simulate concurrent destructive change / rotation in the cold spill sink
        sink.advance_identity();

        // Phase 2: Materialize outside lock must detect the stale generation
        let result = staged.materialize_cold_history(limits);
        assert_eq!(
            result.err(),
            Some(TerminalCheckpointError::StaleColdGeneration),
            "materialize_cold_history must reject stale cold generation"
        );
    }
}
