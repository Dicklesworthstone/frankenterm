use crate::terminalstate::image::*;
use crate::terminalstate::{ImageAttachParams, PlacementInfo};
use crate::{Alert, StableRowIndex, TerminalState};
use ::image::{
    DynamicImage, GenericImage, GenericImageView, ImageBuffer, RgbImage, Rgba, RgbaImage,
};
use anyhow::Context;
use frankenterm_cell::image::{ImageDataType, MAX_IMAGE_WIRE_FRAMES};
use frankenterm_escape_parser::apc::{
    KittyFrameCompositionMode, KittyImage, KittyImageCompression, KittyImageData, KittyImageDelete,
    KittyImageFormat, KittyImageFrame, KittyImageFrameCompose, KittyImagePlacement,
    KittyImageTransmit, KittyImageVerbosity,
};
use frankenterm_surface::change::ImageData;
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug)]
pub struct KittyImageState {
    accumulator: Vec<KittyImage>,
    /// Total materialized (post-base64, pre-zlib) bytes retained by
    /// `accumulator`.  Keeping this alongside the chunks lets us reject an
    /// unfinished transfer before coalescing it into another full-size buffer.
    accumulator_encoded_bytes: usize,
    max_image_id: u32,
    number_to_id: HashMap<u32, u32>,
    id_to_data: HashMap<u32, Arc<ImageData>>,
    placements: HashMap<(u32, Option<u32>), PlacementInfo>,
    used_memory: usize,
    /// Memory budget (bytes) for stored image data. Loaded from config.
    /// Default: 320 MiB.
    pub(crate) image_budget_bytes: usize,
    /// Per-image transmission-size cap applied after decompression and
    /// before image decoding. Defaults to
    /// [`DEFAULT_KITTY_IMAGE_MAX_TRANSMISSION_BYTES`] (16 MiB). See
    /// the const's doc comment for rationale.
    pub(crate) max_transmission_bytes: usize,
}

/// Fully checked resource-ledger transaction for one image append.
///
/// The image performs all fallible validation and allocation separately. This
/// plan is committed only while that prepared append holds the target payload
/// stable, after which the image commit itself is infallible.
#[derive(Debug)]
struct KittyImageGrowthPlan {
    image_id: u32,
    expected_used_memory: usize,
    evictions: Vec<(u32, usize)>,
    committed_memory: usize,
}

impl Default for KittyImageState {
    fn default() -> Self {
        Self {
            accumulator: Vec::new(),
            accumulator_encoded_bytes: 0,
            max_image_id: 0,
            number_to_id: HashMap::new(),
            id_to_data: HashMap::new(),
            placements: HashMap::new(),
            used_memory: 0,
            image_budget_bytes: 320 * 1024 * 1024,
            max_transmission_bytes: DEFAULT_KITTY_IMAGE_MAX_TRANSMISSION_BYTES,
        }
    }
}

// Setter / getter for the transmission-size cap. Wired from
// `TerminalConfiguration::kitty_image_max_transmission_bytes` at
// `TerminalState::new` and `set_config` (ft-heic8). The
// `set_max_transmission_bytes` setter also lets tests lower the
// cap to assert the rejection path without constructing a
// 16 MiB+ payload.
impl KittyImageState {
    /// Return the image-ID allocation high-water mark only when no active
    /// out-of-band Kitty state would be lost by checkpoint v1. Screen-attached
    /// image cells are checked separately; this covers incomplete
    /// transmissions, reusable image IDs, and placement bookkeeping that line
    /// serialization alone cannot reconstruct.
    #[cfg(feature = "use_serde")]
    pub(crate) fn checkpoint_high_water_if_quiescent(&self) -> Option<u32> {
        (self.accumulator.is_empty()
            && self.accumulator_encoded_bytes == 0
            && self.number_to_id.is_empty()
            && self.id_to_data.is_empty()
            && self.placements.is_empty()
            && self.used_memory == 0)
            .then_some(self.max_image_id)
    }

    /// Restore the ID-allocation high-water mark after the checkpoint validator
    /// has proved that no live Kitty image/transmission state is present.
    #[cfg(feature = "use_serde")]
    pub(crate) fn restore_quiescent_checkpoint_high_water(&mut self, max_image_id: u32) {
        debug_assert!(self.checkpoint_high_water_if_quiescent().is_some());
        self.max_image_id = max_image_id;
    }

    #[cfg(all(test, feature = "use_serde"))]
    pub(crate) fn mark_nonempty_for_checkpoint_test(&mut self) {
        self.used_memory = 1;
    }

    /// Override the per-image transmission-size cap.
    pub(crate) fn set_max_transmission_bytes(&mut self, bytes: usize) {
        self.max_transmission_bytes = bytes;
        if self.accumulator_encoded_bytes > bytes {
            log::warn!(
                "discarding {}-byte incomplete Kitty transmission after cap was lowered to {}",
                self.accumulator_encoded_bytes,
                bytes
            );
            self.clear_accumulator();
        }
    }

    /// Current per-image transmission-size cap.
    #[cfg(test)]
    pub(crate) fn max_transmission_bytes(&self) -> usize {
        self.max_transmission_bytes
    }

    /// Per ft-mv27v (cont of ft-d1pv3): test-only accessor for the
    /// number of currently-cached images. The cap-rejection
    /// integration test asserts this stays at zero after a
    /// rejected payload — proving no id-space slot was consumed.
    #[cfg(test)]
    pub(crate) fn id_to_data_len(&self) -> usize {
        self.id_to_data.len()
    }

    /// Per ft-mv27v: test-only accessor for the highest image_id
    /// allocated so far. A rejected payload must not advance this
    /// counter (otherwise an attacker could exhaust the id space
    /// via repeated oversized payloads).
    #[cfg(test)]
    pub(crate) fn max_image_id(&self) -> u32 {
        self.max_image_id
    }
}

fn next_kitty_frame_number(frame_count: usize) -> anyhow::Result<u32> {
    let frame_count = u32::try_from(frame_count).context("kitty animation has too many frames")?;
    frame_count
        .checked_add(1)
        .context("kitty animation frame space exhausted")
}

/// Maximum number of accumulated multi-chunk Kitty image fragments.
/// Prevents unbounded growth from malformed escape sequences that start
/// a multi-chunk transfer but never send the final chunk.
const MAX_KITTY_ACCUMULATOR_CHUNKS: usize = 4096;

/// The image wire format and renderer both cap animations at this many frames.
/// Apply the same cap at the Kitty mutation boundary so a local session cannot
/// build an animation that can neither be serialized nor rendered remotely.
const MAX_KITTY_ANIMATION_FRAMES: usize = MAX_IMAGE_WIRE_FRAMES;

/// Maximum number of image-number-to-id mappings before deterministic
/// numeric-key eviction. Prevents unbounded HashMap growth in long-running
/// sessions with many unique image numbers.
const MAX_KITTY_NUMBER_TO_ID_ENTRIES: usize = 4096;

/// Default per-image transmission-size cap for Kitty graphics, applied
/// to the post-decompression payload. Prevents memory bombs from
/// adversarial APC payloads (e.g., a zlib-compressed PNG that
/// decompresses to GBs of RGBA, or a single direct payload that's
/// already huge).
///
/// 16 MiB sized per `ft-2okh0.1`'s security gate. Roughly fits a
/// 2048×2048 RGBA frame and accommodates typical PNGs from image.nvim,
/// yazi, and Kitty's `icat`. Operators who need larger transmissions
/// (4 K AI-generated previews etc.) can override the field at runtime
/// via `KittyImageState::set_max_transmission_bytes`; the
/// continuation bead wires this to `[kitty.image]` config.
pub(crate) const DEFAULT_KITTY_IMAGE_MAX_TRANSMISSION_BYTES: usize = 16 * 1024 * 1024;
const MAX_KITTY_ALT_TEXT_CHARS: usize = 256;
const KITTY_ZLIB_OUTPUT_CHUNK_BYTES: usize = 64 * 1024;

fn checked_image_data_len(data: &ImageDataType) -> anyhow::Result<usize> {
    match data {
        ImageDataType::EncodedFile(data) => Ok(data.len()),
        ImageDataType::EncodedLease(_) => Ok(0),
        ImageDataType::Rgba8 { data, .. } => Ok(data.len()),
        ImageDataType::AnimRgba8 { frames, .. } => {
            frames.iter().try_fold(0usize, |total, frame| {
                total
                    .checked_add(frame.len())
                    .context("kitty image resident byte count overflow")
            })
        }
    }
}

fn checked_image_len(data: &ImageData) -> anyhow::Result<usize> {
    checked_image_data_len(&data.data())
}

fn checked_kitty_frame_buffer_len(width: u32, height: u32) -> anyhow::Result<usize> {
    anyhow::ensure!(
        width != 0 && height != 0,
        "Kitty frame dimensions must be nonzero, got {}x{}",
        width,
        height
    );
    usize::try_from(u128::from(width) * u128::from(height) * 4)
        .context("Kitty frame dimensions exceed the addressable byte length")
}

fn ensure_kitty_frame_buffer_len(width: u32, height: u32, actual: usize) -> anyhow::Result<()> {
    let expected = checked_kitty_frame_buffer_len(width, height)?;
    anyhow::ensure!(
        expected == actual,
        "Kitty frame data size mismatch: {}x{} requires {} bytes, got {}",
        width,
        height,
        expected,
        actual
    );
    Ok(())
}

/// Inflate a zlib-wrapped Kitty payload in fixed-size output steps while
/// enforcing the configured post-decompression ceiling before extending the
/// retained output.  The one-byte probe at the exact limit distinguishes an
/// exact-fit stream from a stream that has at least one forbidden extra byte.
fn decompress_kitty_zlib_bounded(input: &[u8], max_output: usize) -> anyhow::Result<Vec<u8>> {
    use miniz_oxide::inflate::stream::{inflate, InflateState};
    use miniz_oxide::{DataFormat, MZFlush, MZStatus};

    let initial_capacity = input
        .len()
        .saturating_mul(2)
        .min(max_output)
        .min(KITTY_ZLIB_OUTPUT_CHUNK_BYTES);
    let mut output = Vec::with_capacity(initial_capacity);
    let mut state = InflateState::new_boxed(DataFormat::Zlib);
    let mut input_offset = 0usize;
    let mut chunk = vec![0u8; KITTY_ZLIB_OUTPUT_CHUNK_BYTES];

    loop {
        let remaining = max_output.saturating_sub(output.len());
        let at_limit = remaining == 0;
        let mut limit_probe = [0u8; 1];
        let output_slice = if at_limit {
            &mut limit_probe[..]
        } else {
            &mut chunk[..remaining.min(KITTY_ZLIB_OUTPUT_CHUNK_BYTES)]
        };

        let result = inflate(
            &mut state,
            &input[input_offset..],
            output_slice,
            MZFlush::None,
        );
        input_offset = input_offset
            .checked_add(result.bytes_consumed)
            .context("kitty zlib input offset overflow")?;
        anyhow::ensure!(
            input_offset <= input.len(),
            "kitty zlib decoder consumed beyond its input"
        );

        if at_limit && result.bytes_written != 0 {
            anyhow::bail!(
                "Kitty graphics transmission rejected: decompressed payload exceeds \
                 per-image cap {} bytes",
                max_output
            );
        }
        if !at_limit {
            output
                .try_reserve_exact(result.bytes_written)
                .context("reserving bounded Kitty zlib output")?;
            output.extend_from_slice(&output_slice[..result.bytes_written]);
        }

        match result.status {
            Ok(MZStatus::StreamEnd) => return Ok(output),
            Ok(MZStatus::Ok) => {
                if result.bytes_consumed == 0 && result.bytes_written == 0 {
                    anyhow::bail!("decompressing Kitty image data made no progress");
                }
            }
            Ok(MZStatus::NeedDict) => {
                anyhow::bail!("decompressing Kitty image data requires a preset dictionary")
            }
            Err(err) => anyhow::bail!("decompressing Kitty image data: {err:?}"),
        }
    }
}

fn sanitize_kitty_alt_text(input: &str) -> Option<String> {
    let mut out = String::with_capacity(input.len());
    let mut prev_was_space = false;

    for ch in input.chars() {
        let cp = ch as u32;
        let is_control = cp < 0x20 || cp == 0x7f || (0x80..=0x9f).contains(&cp);
        if is_control {
            if matches!(ch, '\t' | '\n' | '\r') && !prev_was_space {
                out.push(' ');
                prev_was_space = true;
            }
            continue;
        }

        if ch.is_whitespace() {
            if !prev_was_space {
                out.push(' ');
                prev_was_space = true;
            }
            continue;
        }

        out.push(ch);
        prev_was_space = false;
    }

    let trimmed = out.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut text = String::new();
    for ch in trimmed.chars().take(MAX_KITTY_ALT_TEXT_CHARS) {
        text.push(ch);
    }
    Some(text)
}

fn kitty_data_filename(data: &KittyImageData) -> Option<String> {
    match data {
        KittyImageData::File { path, .. } | KittyImageData::TemporaryFile { path, .. } => {
            Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .filter(|name| !name.is_empty())
        }
        KittyImageData::Direct(_)
        | KittyImageData::DirectBin(_)
        | KittyImageData::SharedMem { .. } => None,
    }
}

fn resolve_kitty_alt_text(transmit: &KittyImageTransmit) -> Option<String> {
    if let Some(bytes) = &transmit.alt_text {
        let decoded = String::from_utf8_lossy(bytes);
        if let Some(text) = sanitize_kitty_alt_text(&decoded) {
            return Some(text);
        }
    }

    kitty_data_filename(&transmit.data).and_then(|filename| sanitize_kitty_alt_text(&filename))
}

fn placement_stable_rows(info: PlacementInfo) -> Option<std::ops::Range<StableRowIndex>> {
    let rows = StableRowIndex::try_from(info.rows).ok()?;
    let end = info.first_row.checked_add(rows)?;
    Some(info.first_row..end)
}

fn materialize_kitty_direct_chunk(
    img: &mut KittyImage,
    remaining: usize,
    transmission_cap: usize,
) -> anyhow::Result<Vec<u8>> {
    let transmit = match img {
        KittyImage::TransmitData { transmit, .. }
        | KittyImage::TransmitDataAndDisplay { transmit, .. } => transmit,
        _ => anyhow::bail!("expected a Kitty image transmission chunk"),
    };
    let data = std::mem::replace(&mut transmit.data, KittyImageData::DirectBin(Vec::new()));
    let materialized = match data {
        KittyImageData::DirectBin(data) => data,
        KittyImageData::Direct(data) => {
            // Reject an obviously oversized base64 fragment before allocating
            // its decoded representation. A valid padded base64 encoding of
            // `remaining` bytes needs no more than ceil(remaining / 3) * 4
            // characters.
            let max_base64_chars = (remaining.saturating_add(2) / 3).saturating_mul(4);
            anyhow::ensure!(
                data.len() <= max_base64_chars,
                "Kitty graphics transmission rejected: encoded chunk {} bytes \
                 cannot fit remaining aggregate cap {} bytes",
                data.len(),
                remaining
            );
            KittyImageData::Direct(data)
                .load_data_bounded(remaining)
                .context("decoding Kitty image chunk")?
        }
        other => anyhow::bail!(
            "expected direct data for a multi-chunk Kitty transmission, found {other:#?}"
        ),
    };
    anyhow::ensure!(
        materialized.len() <= remaining,
        "Kitty graphics transmission rejected: accumulated payload would exceed \
         per-image cap {} bytes",
        transmission_cap
    );
    Ok(materialized)
}

impl KittyImageState {
    fn clear_accumulator(&mut self) {
        self.accumulator.clear();
        self.accumulator_encoded_bytes = 0;
    }

    /// Materialize and retain one direct-data chunk.  Each base64 fragment is
    /// decoded exactly once, and the aggregate pre-zlib byte count is admitted
    /// before the fragment enters long-lived session state.
    fn accumulate_chunk(&mut self, mut img: KittyImage) -> anyhow::Result<()> {
        if self.accumulator.len() >= MAX_KITTY_ACCUMULATOR_CHUNKS {
            log::warn!(
                "kitty image accumulator exceeded {} chunks, discarding incomplete transfer",
                MAX_KITTY_ACCUMULATOR_CHUNKS
            );
            self.clear_accumulator();
            anyhow::bail!(
                "Kitty graphics transmission rejected: more than {} chunks",
                MAX_KITTY_ACCUMULATOR_CHUNKS
            );
        }

        let remaining = match self
            .max_transmission_bytes
            .checked_sub(self.accumulator_encoded_bytes)
        {
            Some(remaining) => remaining,
            None => {
                self.clear_accumulator();
                anyhow::bail!("kitty accumulator byte accounting exceeded its cap");
            }
        };
        let materialized =
            materialize_kitty_direct_chunk(&mut img, remaining, self.max_transmission_bytes);

        let materialized = match materialized {
            Ok(data) => data,
            Err(err) => {
                self.clear_accumulator();
                return Err(err);
            }
        };
        let materialized_len = materialized.len();
        let transmit = match &mut img {
            KittyImage::TransmitData { transmit, .. }
            | KittyImage::TransmitDataAndDisplay { transmit, .. } => transmit,
            _ => unreachable!("transmission variant checked above"),
        };
        transmit.data = KittyImageData::DirectBin(materialized);
        self.accumulator_encoded_bytes = self
            .accumulator_encoded_bytes
            .checked_add(materialized_len)
            .context("kitty accumulator byte count overflow")?;
        self.accumulator.push(img);
        Ok(())
    }

    fn remove_data_for_id(&mut self, image_id: u32) -> anyhow::Result<()> {
        if let Some(data) = self.id_to_data.get(&image_id) {
            let resident_bytes = checked_image_len(data)?;
            let remaining = self
                .used_memory
                .checked_sub(resident_bytes)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "kitty image memory accounting underflow removing id {}: used={} image={}",
                        image_id,
                        self.used_memory,
                        resident_bytes
                    )
                })?;
            self.id_to_data.remove(&image_id);
            self.used_memory = remaining;
        }
        self.number_to_id.retain(|_num, id| *id != image_id);
        Ok(())
    }

    fn eviction_plan(
        &self,
        excluded_image_id: u32,
        bytes_needed: usize,
    ) -> anyhow::Result<Vec<(u32, usize)>> {
        if bytes_needed == 0 {
            return Ok(Vec::new());
        }

        let referenced: HashSet<u32> = self.placements.keys().map(|(id, _)| *id).collect();
        let mut candidates: Vec<u32> = self
            .id_to_data
            .keys()
            .copied()
            .filter(|id| *id != excluded_image_id && !referenced.contains(id))
            .collect();
        candidates.sort_unstable();

        let mut plan = Vec::new();
        let mut freed = 0usize;
        for id in candidates {
            let resident_bytes = checked_image_len(
                self.id_to_data
                    .get(&id)
                    .expect("eviction candidate must still exist"),
            )?;
            freed = freed
                .checked_add(resident_bytes)
                .context("kitty eviction byte count overflow")?;
            plan.push((id, resident_bytes));
            if freed >= bytes_needed {
                return Ok(plan);
            }
        }

        anyhow::bail!(
            "Kitty image memory budget exhausted: need to free {} bytes, but only {} \
             unreferenced bytes are available",
            bytes_needed,
            freed
        )
    }

    fn apply_eviction_plan(&mut self, plan: &[(u32, usize)]) -> anyhow::Result<()> {
        let total_freed = plan.iter().try_fold(0usize, |total, (_, resident_bytes)| {
            total
                .checked_add(*resident_bytes)
                .context("kitty eviction byte count overflow")
        })?;
        let remaining = self.used_memory.checked_sub(total_freed).ok_or_else(|| {
            anyhow::anyhow!(
                "kitty image memory accounting underflow during eviction: used={} freed={}",
                self.used_memory,
                total_freed
            )
        })?;

        for (id, _) in plan {
            anyhow::ensure!(
                self.id_to_data.contains_key(id),
                "kitty eviction candidate {} disappeared before commit",
                id
            );
        }
        for (id, _) in plan {
            self.id_to_data.remove(id);
            self.number_to_id
                .retain(|_number, mapped_id| mapped_id != id);
        }
        self.used_memory = remaining;
        if total_freed != 0 {
            log::info!("pruned {} bytes of unreferenced Kitty images", total_freed);
        }
        Ok(())
    }

    /// Replace one stored image as a single commit.  All arithmetic and the
    /// complete eviction plan are validated before any resident entry changes.
    fn record_id_to_data(&mut self, image_id: u32, data: Arc<ImageData>) -> anyhow::Result<()> {
        let new_len = checked_image_len(&data)?;
        anyhow::ensure!(
            new_len <= self.image_budget_bytes,
            "Kitty image {} needs {} resident bytes, exceeding per-image budget {}",
            image_id,
            new_len,
            self.image_budget_bytes
        );

        let old_len = self
            .id_to_data
            .get(&image_id)
            .map(|old| checked_image_len(old))
            .transpose()?
            .unwrap_or(0);
        let retained = self.used_memory.checked_sub(old_len).ok_or_else(|| {
            anyhow::anyhow!(
                "kitty image memory accounting underflow replacing id {}: used={} old={}",
                image_id,
                self.used_memory,
                old_len
            )
        })?;
        let projected = retained
            .checked_add(new_len)
            .context("kitty image memory accounting overflow")?;
        let plan =
            self.eviction_plan(image_id, projected.saturating_sub(self.image_budget_bytes))?;
        let freed = plan.iter().try_fold(0usize, |total, (_, bytes)| {
            total
                .checked_add(*bytes)
                .context("kitty eviction byte count overflow")
        })?;
        let committed = projected
            .checked_sub(freed)
            .context("kitty image projected byte count underflow")?;
        anyhow::ensure!(
            committed <= self.image_budget_bytes,
            "Kitty image memory budget admission failed: projected {} > budget {}",
            committed,
            self.image_budget_bytes
        );

        self.apply_eviction_plan(&plan)?;
        self.id_to_data.remove(&image_id);
        self.id_to_data.insert(image_id, data);
        self.used_memory = committed;
        Ok(())
    }

    /// Build the complete, nonmutating admission plan for growing an existing
    /// image. Callers can use this before acquiring `ImageData::data_mut()` so
    /// a predictable resource rejection never forces the guard to rehash an
    /// otherwise untouched large animation.
    fn plan_image_growth(
        &self,
        image_id: u32,
        current_image_len: usize,
        growth: usize,
    ) -> anyhow::Result<KittyImageGrowthPlan> {
        anyhow::ensure!(
            self.id_to_data.contains_key(&image_id),
            "cannot grow missing Kitty image id {}",
            image_id
        );
        let resulting_image_len = current_image_len
            .checked_add(growth)
            .context("Kitty image resident byte count overflow")?;
        anyhow::ensure!(
            resulting_image_len <= self.image_budget_bytes,
            "Kitty image {} would grow to {} resident bytes, exceeding per-image budget {}",
            image_id,
            resulting_image_len,
            self.image_budget_bytes
        );
        let projected = self
            .used_memory
            .checked_add(growth)
            .context("kitty total image memory accounting overflow")?;
        let plan =
            self.eviction_plan(image_id, projected.saturating_sub(self.image_budget_bytes))?;
        let freed = plan.iter().try_fold(0usize, |total, (_, bytes)| {
            total
                .checked_add(*bytes)
                .context("kitty eviction byte count overflow")
        })?;
        let committed = projected
            .checked_sub(freed)
            .context("kitty image projected byte count underflow")?;
        anyhow::ensure!(
            committed <= self.image_budget_bytes,
            "Kitty image memory budget admission failed: committed {} > budget {}",
            committed,
            self.image_budget_bytes
        );
        Ok(KittyImageGrowthPlan {
            image_id,
            expected_used_memory: self.used_memory,
            evictions: plan,
            committed_memory: committed,
        })
    }

    /// Commit a previously checked resource-ledger plan. The caller holds a
    /// prepared image append whose eventual commit cannot fail.
    fn commit_image_growth_plan(&mut self, plan: KittyImageGrowthPlan) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.used_memory == plan.expected_used_memory,
            "Kitty image memory state changed between growth plan and commit: expected {}, got {}",
            plan.expected_used_memory,
            self.used_memory
        );
        anyhow::ensure!(
            self.id_to_data.contains_key(&plan.image_id),
            "cannot commit growth for missing Kitty image id {}",
            plan.image_id
        );
        self.apply_eviction_plan(&plan.evictions)?;
        self.used_memory = plan.committed_memory;
        Ok(())
    }

    fn record_number_mapping(&mut self, image_number: u32, image_id: u32) {
        let id_to_data = &self.id_to_data;
        self.number_to_id
            .retain(|_, id| id_to_data.contains_key(id));
        self.number_to_id.insert(image_number, image_id);

        if self.number_to_id.len() > MAX_KITTY_NUMBER_TO_ID_ENTRIES {
            let excess = self.number_to_id.len() - MAX_KITTY_NUMBER_TO_ID_ENTRIES;
            let mut keys_to_remove: Vec<u32> = self
                .number_to_id
                .keys()
                .copied()
                .filter(|number| *number != image_number)
                .collect();
            keys_to_remove.sort_unstable();
            for number in keys_to_remove.into_iter().take(excess) {
                self.number_to_id.remove(&number);
            }
            log::warn!(
                "kitty number_to_id exceeded cap ({}), evicted {} numeric-key mappings",
                MAX_KITTY_NUMBER_TO_ID_ENTRIES,
                excess
            );
        }
    }
}

impl TerminalState {
    fn kitty_img_place(
        &mut self,
        image_id: Option<u32>,
        image_number: Option<u32>,
        placement: KittyImagePlacement,
        verbosity: KittyImageVerbosity,
    ) -> anyhow::Result<()> {
        let image_id = match image_id {
            Some(id) => id,
            None => *self
                .kitty_img
                .number_to_id
                .get(
                    &image_number
                        .ok_or_else(|| anyhow::anyhow!("no image_id or image_number specified!"))?,
                )
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "image_number has no matching image id {:?} in number_to_id",
                        image_number
                    )
                })?,
        };

        log::trace!(
            "kitty_img_place image_id {:?} image_no {:?} placement {:?} verb {:?}",
            image_id,
            image_number,
            placement,
            verbosity
        );
        if image_id != 0 {
            self.kitty_remove_placement(image_id, placement.placement_id);
        }
        let img = Arc::clone(self.kitty_img.id_to_data.get(&image_id).ok_or_else(|| {
            anyhow::anyhow!(
                "no matching image id {} in id_to_data for image_number {:?}",
                image_id,
                image_number
            )
        })?);

        let (image_width, image_height) = img.data().dimensions()?;

        let info = self.assign_image_to_cells(ImageAttachParams {
            image_width,
            image_height,
            source_width: placement.w,
            source_height: placement.h,
            source_origin_x: placement.x.unwrap_or(0),
            source_origin_y: placement.y.unwrap_or(0),
            cell_padding_left: placement.x_offset.unwrap_or(0).min(u16::MAX as u32) as u16,
            cell_padding_top: placement.y_offset.unwrap_or(0).min(u16::MAX as u32) as u16,
            data: img,
            style: ImageAttachStyle::Kitty,
            z_index: placement.z_index.unwrap_or(0),
            columns: placement.columns.map(|x| x as usize),
            rows: placement.rows.map(|x| x as usize),
            image_id: Some(image_id),
            placement_id: placement.placement_id,
            do_not_move_cursor: placement.do_not_move_cursor,
        })?;

        self.kitty_img
            .placements
            .insert((image_id, placement.placement_id), info);
        log::trace!(
            "record placement for {} (image_number {:?}) {:?}",
            image_id,
            image_number,
            placement.placement_id
        );

        Ok(())
    }

    fn kitty_img_inner(&mut self, img: KittyImage) -> anyhow::Result<()> {
        match self
            .coalesce_kitty_accumulation(img)
            .context("coalesce_kitty_accumulation")?
        {
            KittyImage::TransmitData {
                transmit,
                verbosity,
            } => {
                self.kitty_img_transmit(transmit, verbosity)?;
                Ok(())
            }
            KittyImage::TransmitDataAndDisplay {
                transmit,
                placement,
                verbosity,
            } => {
                log::trace!("TransmitDataAndDisplay {:#?} {:#?}", transmit, placement);
                let image_number = transmit.image_number;
                let image_id = self.kitty_img_transmit(transmit, verbosity)?;
                self.kitty_img_place(Some(image_id), image_number, placement, verbosity)
            }
            _ => anyhow::bail!("impossible KittImage variant"),
        }
    }

    pub(crate) fn kitty_img(&mut self, img: KittyImage) -> anyhow::Result<()> {
        log::trace!("{:?}", img);
        if !self.config.enable_kitty_graphics() {
            return Ok(());
        }
        let verbosity = img.verbosity();
        match img {
            KittyImage::Query { transmit } => match transmit
                .data
                .load_data_bounded(self.kitty_img.max_transmission_bytes)
            {
                Ok(_) => {
                    self.kitty_send_response(
                        verbosity,
                        true,
                        transmit.image_id,
                        transmit.image_number,
                        "OK".to_string(),
                    );
                }
                Err(err) => {
                    self.kitty_send_response(
                        verbosity,
                        false,
                        transmit.image_id,
                        transmit.image_number,
                        format!("ERROR:{:#}", err),
                    );
                }
            },
            KittyImage::TransmitData {
                transmit,
                verbosity,
            } => {
                let more_data_follows = transmit.more_data_follows;
                let img = KittyImage::TransmitData {
                    transmit,
                    verbosity,
                };
                if more_data_follows {
                    self.kitty_img.accumulate_chunk(img)?;
                } else {
                    self.kitty_img_inner(img)?;
                }
            }
            KittyImage::TransmitDataAndDisplay {
                transmit,
                placement,
                verbosity,
            } => {
                let more_data_follows = transmit.more_data_follows;
                let img = KittyImage::TransmitDataAndDisplay {
                    transmit,
                    placement,
                    verbosity,
                };
                if more_data_follows {
                    self.kitty_img.accumulate_chunk(img)?;
                } else {
                    self.kitty_img_inner(img)?;
                }
            }
            KittyImage::Display {
                image_id,
                image_number,
                placement,
                verbosity,
            } => {
                self.kitty_img_place(image_id, image_number, placement, verbosity)?;
            }
            KittyImage::Delete {
                what:
                    KittyImageDelete::ByImageId {
                        image_id,
                        placement_id,
                        delete,
                    },
                verbosity,
            } => {
                log::trace!(
                    "remove a placement: image_id {} placement_id {:?} delete {} verb {:?}",
                    image_id,
                    placement_id,
                    delete,
                    verbosity
                );

                self.kitty_remove_placement(image_id, placement_id);

                if delete {
                    self.kitty_img.remove_data_for_id(image_id)?;
                }
            }
            KittyImage::Delete {
                what: KittyImageDelete::All { delete },
                verbosity: _,
            } => {
                self.kitty_remove_all_placements(delete);
            }
            KittyImage::Delete { what, verbosity } => {
                log::warn!("unhandled KittyImage::Delete {:?} {:?}", what, verbosity);
            }
            KittyImage::TransmitFrame {
                transmit,
                frame,
                verbosity,
            } => {
                if let Err(err) = self.kitty_frame_transmit(transmit, frame, verbosity) {
                    log::error!("Error {:#} while handling KittyImage::TransmitFrame", err,);
                }
            }
            KittyImage::ComposeFrame { frame, verbosity } => {
                if let Err(err) = self.kitty_frame_compose(frame, verbosity) {
                    log::error!("Error {:#} while handling KittyImage::ComposeFrame", err);
                }
            }
        };

        Ok(())
    }

    fn kitty_remove_placement_from_model(
        &mut self,
        image_id: u32,
        placement_id: Option<u32>,
        info: PlacementInfo,
    ) {
        let seqno = self.seqno;
        let screen = self.screen_mut();
        let Some(stable_rows) = placement_stable_rows(info) else {
            return;
        };
        let range = screen.stable_range(&stable_rows);
        for idx in range {
            let line = screen.line_mut(idx);
            for c in line.cells_mut() {
                c.attrs_mut()
                    .detach_image_with_placement(image_id, placement_id);
            }
            line.update_last_change_seqno(seqno);
        }
    }

    /// Force every row containing a placement of `image_id` into the next
    /// line-delta publication.  Kitty frame operations mutate the shared image
    /// object without touching cell attributes, so the rows would otherwise
    /// retain their old sequence numbers and remote renderers could keep stale
    /// pixels indefinitely.
    fn kitty_mark_image_placements_dirty(&mut self, image_id: u32) {
        let placement_infos: Vec<PlacementInfo> = self
            .kitty_img
            .placements
            .iter()
            .filter_map(|((placed_image_id, _), info)| {
                (*placed_image_id == image_id).then_some(*info)
            })
            .collect();
        let mut physical_rows = Vec::new();
        {
            let screen = self.screen();
            for info in placement_infos {
                let Some(stable_rows) = placement_stable_rows(info) else {
                    continue;
                };
                physical_rows.extend(screen.stable_range(&stable_rows));
            }
        }
        physical_rows.sort_unstable();
        physical_rows.dedup();

        let seqno = self.seqno;
        let screen = self.screen_mut();
        for row in physical_rows {
            screen.line_mut(row).update_last_change_seqno(seqno);
        }
    }

    fn kitty_remove_placement(&mut self, image_id: u32, placement_id: Option<u32>) {
        if placement_id.is_some() {
            if let Some(info) = self.kitty_img.placements.remove(&(image_id, placement_id)) {
                log::trace!("removed placement {} {:?}", image_id, placement_id);
                self.kitty_remove_placement_from_model(image_id, placement_id, info);
            }
        } else {
            let mut to_clear = vec![];
            for (id, p) in self.kitty_img.placements.keys() {
                if *id == image_id {
                    to_clear.push(*p);
                }
            }
            for p in to_clear.into_iter() {
                if let Some(info) = self.kitty_img.placements.remove(&(image_id, p)) {
                    self.kitty_remove_placement_from_model(image_id, p, info);
                }
            }
        }

        log::trace!(
            "after remove: there are {} placements, {} images, {} memory",
            self.kitty_img.placements.len(),
            self.kitty_img.id_to_data.len(),
            self.kitty_img.used_memory,
        );
    }

    pub(crate) fn kitty_remove_all_placements(&mut self, delete: bool) {
        for ((image_id, p), info) in std::mem::take(&mut self.kitty_img.placements).into_iter() {
            self.kitty_remove_placement_from_model(image_id, p, info);
        }
        if delete {
            self.kitty_img.id_to_data.clear();
            self.kitty_img.used_memory = 0;
            self.kitty_img.number_to_id.clear();
        }
    }

    fn kitty_send_response(
        &mut self,
        verbosity: KittyImageVerbosity,
        success: bool,
        image_id: Option<u32>,
        image_no: Option<u32>,
        message: String,
    ) {
        match verbosity {
            KittyImageVerbosity::Verbose => {}
            KittyImageVerbosity::OnlyErrors => {
                if success {
                    return;
                }
            }
            KittyImageVerbosity::Quiet => {
                return;
            }
        }

        log::trace!("Query Response: {}", message);

        match (image_id, image_no) {
            (Some(id), Some(no)) => {
                write!(self.writer, "\x1b_GI={},i={};{}\x1b\\", no, id, message).ok();
            }
            (Some(id), None) => {
                write!(self.writer, "\x1b_Gi={};{}\x1b\\", id, message).ok();
            }
            (None, Some(no)) => {
                write!(self.writer, "\x1b_GI={};{}\x1b\\", no, message).ok();
            }
            (None, None) => {
                write!(self.writer, "\x1b_G{}\x1b\\", message).ok();
            }
        }
        self.writer.flush().ok();
    }

    fn kitty_frame_compose(
        &mut self,
        frame: KittyImageFrameCompose,
        verbosity: KittyImageVerbosity,
    ) -> anyhow::Result<()> {
        let image_id = match frame.image_number {
            Some(no) => match self.kitty_img.number_to_id.get(&no) {
                Some(id) => *id,
                None => {
                    self.kitty_send_response(
                        verbosity,
                        false,
                        frame.image_id,
                        frame.image_number,
                        "ENOENT".to_string(),
                    );
                    anyhow::bail!("no such image_number {}", no);
                }
            },
            None => frame.image_id.ok_or_else(|| {
                self.kitty_send_response(
                    verbosity,
                    false,
                    frame.image_id,
                    frame.image_number,
                    "ENOENT".to_string(),
                );
                anyhow::anyhow!("no image_id")
            })?,
        };

        let src_frame = frame.source_frame.ok_or_else(|| {
            self.kitty_send_response(
                verbosity,
                false,
                frame.image_id,
                frame.image_number,
                "ENOENT".to_string(),
            );
            anyhow::anyhow!("missing source frame")
        })? as usize;
        let target_frame = frame.target_frame.ok_or_else(|| {
            self.kitty_send_response(
                verbosity,
                false,
                frame.image_id,
                frame.image_number,
                "ENOENT".to_string(),
            );
            anyhow::anyhow!("missing target frame")
        })? as usize;

        let img = Arc::clone(
            self.kitty_img
                .id_to_data
                .get(&image_id)
                .ok_or_else(|| anyhow::anyhow!("invalid image id {}", image_id))?,
        );

        // Reject every shape/cardinality/index error while holding only the
        // read guard. `ImageDataMutGuard::drop` intentionally repairs embedded
        // hashes for arbitrary mutations; acquiring it for a command that can
        // be proven invalid here would turn a cheap rejection into a full
        // animation rehash.
        let (target_width, target_height) = {
            let image = img.data();
            match &*image {
                ImageDataType::EncodedLease(_) | ImageDataType::EncodedFile(_) => {
                    anyhow::bail!("invalid image type")
                }
                ImageDataType::Rgba8 {
                    width,
                    height,
                    data,
                    ..
                } => {
                    anyhow::ensure!(
                        src_frame == target_frame && src_frame == 1,
                        "src_frame={} target_frame={} but there is only a single frame",
                        src_frame,
                        target_frame
                    );
                    ensure_kitty_frame_buffer_len(*width, *height, data.len())?;
                    (*width, *height)
                }
                ImageDataType::AnimRgba8 {
                    width,
                    height,
                    frames,
                    durations,
                    hashes,
                } => {
                    anyhow::ensure!(
                        frames.len() == durations.len() && frames.len() == hashes.len(),
                        "ill formed Kitty animation metadata: frames={} durations={} hashes={}",
                        frames.len(),
                        durations.len(),
                        hashes.len()
                    );
                    anyhow::ensure!(
                        frames.len() <= MAX_KITTY_ANIMATION_FRAMES,
                        "Kitty animation has {} frames, exceeding frame budget {}",
                        frames.len(),
                        MAX_KITTY_ANIMATION_FRAMES
                    );
                    anyhow::ensure!(
                        src_frame > 0 && src_frame <= frames.len(),
                        "src_frame {} is out of range",
                        src_frame
                    );
                    anyhow::ensure!(
                        target_frame > 0 && target_frame <= frames.len(),
                        "target_frame {} is out of range",
                        target_frame
                    );
                    ensure_kitty_frame_buffer_len(*width, *height, frames[src_frame - 1].len())?;
                    ensure_kitty_frame_buffer_len(*width, *height, frames[target_frame - 1].len())?;
                    (*width, *height)
                }
            }
        };

        let dest_x = frame.x.unwrap_or(0);
        let dest_y = frame.y.unwrap_or(0);
        let (_, _, clipped_width, clipped_height) = clipped_view_region(
            target_width,
            target_height,
            frame.src_x,
            frame.src_y,
            frame.w,
            frame.h,
        )?;
        if clipped_width == 0
            || clipped_height == 0
            || dest_x >= target_width
            || dest_y >= target_height
        {
            // A fully clipped composition is a successful no-op. Validate its
            // source coordinates above, but do not materialize a potentially
            // large source-frame copy or invalidate target authority.
            return Ok(());
        }

        let src = {
            let image = img.data();
            match &*image {
                ImageDataType::Rgba8 {
                    width,
                    height,
                    data,
                    ..
                } => clip_view(
                    *width,
                    *height,
                    data,
                    frame.src_x,
                    frame.src_y,
                    frame.w,
                    frame.h,
                )?,
                ImageDataType::AnimRgba8 {
                    width,
                    height,
                    frames,
                    ..
                } => clip_view(
                    *width,
                    *height,
                    &frames[src_frame - 1],
                    frame.src_x,
                    frame.src_y,
                    frame.w,
                    frame.h,
                )?,
                ImageDataType::EncodedLease(_) | ImageDataType::EncodedFile(_) => {
                    anyhow::bail!("invalid image type")
                }
            }
        };
        let mut target = img
            .decoded_frame_mut(target_frame - 1)
            .context("acquiring Kitty target frame mutation authority")?;
        let (width, height) = target.image_dimensions();
        let mut dest: ImageBuffer<Rgba<u8>, &mut [u8]> =
            ImageBuffer::from_raw(width, height, &mut *target)
                .ok_or_else(|| anyhow::anyhow!("ill formed image"))?;
        blit(&mut dest, &src, dest_x, dest_y, frame.composition_mode)?;
        drop(dest);
        drop(target);
        self.kitty_mark_image_placements_dirty(image_id);
        Ok(())
    }

    fn kitty_frame_transmit(
        &mut self,
        mut transmit: KittyImageTransmit,
        frame: KittyImageFrame,
        verbosity: KittyImageVerbosity,
    ) -> anyhow::Result<()> {
        if let Some(no) = transmit.image_number.take() {
            match self.kitty_img.number_to_id.get(&no) {
                Some(id) => {
                    transmit.image_id.replace(*id);
                }
                None => {
                    transmit.image_number.replace(no);
                }
            }
        }

        let (image_id, image_number, img) = self.kitty_img_transmit_inner(transmit)?;

        let img = match img.decode() {
            ImageDataType::Rgba8 {
                data,
                width,
                height,
                ..
            } => RgbaImage::from_vec(width, height, data)
                .ok_or_else(|| anyhow::anyhow!("data isn't rgba8"))?,
            wat => anyhow::bail!("data isn't rgba8 {:?}", wat),
        };

        let background_pixel = frame.background_pixel.unwrap_or(0);
        let background_pixel = Rgba([
            ((background_pixel >> 24) & 0xff) as u8,
            ((background_pixel >> 16) & 0xff) as u8,
            ((background_pixel >> 8) & 0xff) as u8,
            (background_pixel & 0xff) as u8,
        ]);

        let anim = match self.kitty_img.id_to_data.get(&image_id).cloned() {
            Some(anim) => anim,
            None => {
                self.kitty_send_response(
                    verbosity,
                    false,
                    Some(image_id),
                    image_number,
                    "ENOENT".to_string(),
                );
                anyhow::bail!(
                    "no matching image id {} in id_to_data for image_number {:?}",
                    image_id,
                    image_number
                )
            }
        };

        let current_image_len = checked_image_len(&anim)?;
        let (planned_growth, target_width, target_height) = {
            let image = anim.data();
            match &*image {
                ImageDataType::EncodedLease(_) | ImageDataType::EncodedFile(_) => {
                    anyhow::bail!("Expected decoded image for image id {}", image_id)
                }
                ImageDataType::Rgba8 {
                    data,
                    width,
                    height,
                    ..
                } => {
                    match frame.base_frame {
                        Some(1) | None => {}
                        Some(n) => anyhow::bail!(
                            "attempted to copy frame {} but there is only a single frame",
                            n
                        ),
                    }
                    ensure_kitty_frame_buffer_len(*width, *height, data.len())?;
                    let growth = match frame.frame_number {
                        Some(1) => None,
                        Some(2) | None => Some(data.len()),
                        Some(n) => anyhow::bail!(
                            "attempted to edit frame {} but there is only a single frame",
                            n
                        ),
                    };
                    (growth, *width, *height)
                }
                ImageDataType::AnimRgba8 {
                    width,
                    height,
                    frames,
                    durations,
                    hashes,
                } => {
                    anyhow::ensure!(
                        frames.len() == durations.len() && frames.len() == hashes.len(),
                        "ill formed Kitty animation metadata: frames={} durations={} hashes={}",
                        frames.len(),
                        durations.len(),
                        hashes.len()
                    );
                    anyhow::ensure!(
                        frames.len() <= MAX_KITTY_ANIMATION_FRAMES,
                        "Kitty animation has {} frames, exceeding frame budget {}",
                        frames.len(),
                        MAX_KITTY_ANIMATION_FRAMES
                    );
                    let append_frame_no = next_kitty_frame_number(frames.len())?;
                    let frame_no = frame.frame_number.unwrap_or(append_frame_no);
                    let growth = if frame_no == append_frame_no {
                        anyhow::ensure!(
                            frames.len() < MAX_KITTY_ANIMATION_FRAMES,
                            "Kitty animation rejected: frame count would exceed {}",
                            MAX_KITTY_ANIMATION_FRAMES
                        );
                        if let Some(base_frame) = frame.base_frame {
                            let base_frame = base_frame as usize;
                            anyhow::ensure!(
                                base_frame > 0 && base_frame <= frames.len(),
                                "attempted to copy frame {} which is outside range 1-{}",
                                base_frame,
                                frames.len()
                            );
                            ensure_kitty_frame_buffer_len(
                                *width,
                                *height,
                                frames[base_frame - 1].len(),
                            )?;
                        }
                        Some(checked_kitty_frame_buffer_len(*width, *height)?)
                    } else {
                        anyhow::ensure!(
                            frame_no > 0 && frame_no < append_frame_no,
                            "attempted to edit frame {} which is outside range 1-{}",
                            frame_no,
                            frames.len()
                        );
                        ensure_kitty_frame_buffer_len(
                            *width,
                            *height,
                            frames[frame_no as usize - 1].len(),
                        )?;
                        None
                    };
                    (growth, *width, *height)
                }
            }
        };
        let mut growth_admission = if let Some(growth) = planned_growth {
            // This first pass is deliberately nonmutating. The authoritative
            // commit consumes this exact plan only after frame construction,
            // validation, hashing, and vector reservation have succeeded.
            Some(
                self.kitty_img
                    .plan_image_growth(image_id, current_image_len, growth)?,
            )
        } else {
            None
        };
        let x = frame.x.unwrap_or(0);
        let y = frame.y.unwrap_or(0);
        let frame_gap = Duration::from_millis(match frame.duration_ms {
            None | Some(0) => 40,
            Some(n) => n.into(),
        });
        enum PreparedFrameMutation {
            Edit(usize),
            Append(Vec<u8>),
        }
        let prepared = {
            let image = anim.data();
            match &*image {
                ImageDataType::Rgba8 {
                    data,
                    width,
                    height,
                    ..
                } => match frame.frame_number {
                    Some(1) => PreparedFrameMutation::Edit(0),
                    Some(2) | None => {
                        let mut new_frame = if frame.base_frame.is_some() {
                            RgbaImage::from_vec(*width, *height, data.clone()).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "kitty image data size mismatch: {}x{} vs {} bytes",
                                    width,
                                    height,
                                    data.len()
                                )
                            })?
                        } else {
                            RgbaImage::from_pixel(*width, *height, background_pixel)
                        };
                        blit(&mut new_frame, &img, x, y, frame.composition_mode)?;
                        PreparedFrameMutation::Append(new_frame.into_vec())
                    }
                    Some(n) => anyhow::bail!(
                        "attempted to edit frame {} but there is only a single frame",
                        n
                    ),
                },
                ImageDataType::AnimRgba8 {
                    width,
                    height,
                    frames,
                    ..
                } => {
                    let append_frame_no = next_kitty_frame_number(frames.len())?;
                    let frame_no = frame.frame_number.unwrap_or(append_frame_no);
                    if frame_no == append_frame_no {
                        let mut new_frame = match frame.base_frame {
                            None => RgbaImage::from_pixel(*width, *height, background_pixel),
                            Some(base_frame) => {
                                let base_frame = base_frame as usize;
                                RgbaImage::from_vec(*width, *height, frames[base_frame - 1].clone())
                                    .ok_or_else(|| {
                                        anyhow::anyhow!(
                                            "kitty frame {} data size mismatch: {}x{} vs {} bytes",
                                            base_frame,
                                            width,
                                            height,
                                            frames[base_frame - 1].len()
                                        )
                                    })?
                            }
                        };
                        blit(&mut new_frame, &img, x, y, frame.composition_mode)?;
                        PreparedFrameMutation::Append(new_frame.into_vec())
                    } else {
                        PreparedFrameMutation::Edit(frame_no as usize - 1)
                    }
                }
                ImageDataType::EncodedLease(_) | ImageDataType::EncodedFile(_) => {
                    anyhow::bail!("Expected decoded image for image id {}", image_id)
                }
            }
        };

        match prepared {
            PreparedFrameMutation::Edit(frame_index) => {
                if x >= target_width || y >= target_height {
                    // The transmitted patch is fully outside the edited
                    // frame. Preserve revision/authority and avoid hashing a
                    // large target for this successful no-op.
                    return Ok(());
                }
                let mut target = anim
                    .decoded_frame_mut(frame_index)
                    .context("acquiring Kitty transmitted-frame mutation authority")?;
                let (width, height) = target.image_dimensions();
                let mut target_image: ImageBuffer<Rgba<u8>, &mut [u8]> =
                    ImageBuffer::from_raw(width, height, &mut *target)
                        .ok_or_else(|| anyhow::anyhow!("ill formed image"))?;
                blit(&mut target_image, &img, x, y, frame.composition_mode)?;
                drop(target_image);
                drop(target);
            }
            PreparedFrameMutation::Append(new_frame) => {
                let growth = new_frame.len();
                let prepared = anim
                    .prepare_decoded_frame_append(new_frame, frame_gap, MAX_KITTY_ANIMATION_FRAMES)
                    .context("preparing Kitty animation frame append")?;
                anyhow::ensure!(
                    prepared.additional_decoded_bytes() == growth,
                    "prepared Kitty frame growth changed from {} to {} bytes",
                    growth,
                    prepared.additional_decoded_bytes()
                );
                anyhow::ensure!(
                    prepared.existing_decoded_bytes() == current_image_len,
                    "Kitty image {} changed size between growth plan and append preparation: expected {}, got {} bytes",
                    image_id,
                    current_image_len,
                    prepared.existing_decoded_bytes()
                );
                let admission = growth_admission.take().ok_or_else(|| {
                    anyhow::anyhow!("missing Kitty memory admission plan for frame append")
                })?;
                self.kitty_img.commit_image_growth_plan(admission)?;
                prepared.commit();
            }
        }
        self.kitty_mark_image_placements_dirty(image_id);
        Ok(())
    }

    fn kitty_img_transmit_inner(
        &mut self,
        transmit: KittyImageTransmit,
    ) -> anyhow::Result<(u32, Option<u32>, ImageDataType)> {
        log::trace!("transmit {:?}", transmit);
        let (id, no) = match (transmit.image_id, transmit.image_number) {
            (Some(_), Some(_)) => {
                anyhow::bail!("EINVAL: cannot use both i= and I= in the same request");
            }
            (None, None) => {
                // Assume image id 0
                (0, None)
            }
            (Some(id), None) => (id, None),
            (None, Some(no)) => {
                let id = self
                    .kitty_img
                    .max_image_id
                    .checked_add(1)
                    .context("kitty image id space exhausted")?;
                (id, Some(no))
            }
        };

        let data = transmit
            .data
            .load_data_bounded(self.kitty_img.max_transmission_bytes)
            .map_err(|error| {
                anyhow::anyhow!(
                    "Kitty graphics transmission rejected by the per-image cap of {} bytes: {error}",
                    self.kitty_img.max_transmission_bytes
                )
            })?;

        anyhow::ensure!(
            data.len() <= self.kitty_img.max_transmission_bytes,
            "Kitty graphics transmission rejected: encoded payload {} bytes exceeds \
             per-image cap {} bytes",
            data.len(),
            self.kitty_img.max_transmission_bytes
        );

        let data = match transmit.compression {
            KittyImageCompression::None => data,
            KittyImageCompression::Deflate => {
                decompress_kitty_zlib_bounded(&data, self.kitty_img.max_transmission_bytes)?
            }
        };

        // Defense in depth after optional decompression. Pre-decompression
        // bytes are checked above and accumulated multi-chunk bytes are
        // admitted before entering session state.
        if data.len() > self.kitty_img.max_transmission_bytes {
            anyhow::bail!(
                "Kitty graphics transmission rejected: payload {} bytes \
                 exceeds per-image cap {} bytes \
                 (raise via KittyImageState::set_max_transmission_bytes)",
                data.len(),
                self.kitty_img.max_transmission_bytes,
            );
        }

        let img = match transmit.format {
            None | Some(KittyImageFormat::Rgba) | Some(KittyImageFormat::Rgb) => {
                let (width, height) = match (transmit.width, transmit.height) {
                    (Some(w), Some(h)) => (w, h),
                    _ => {
                        anyhow::bail!("missing width/height info for kitty img");
                    }
                };

                check_image_dimensions(width, height)?;

                let data = match transmit.format {
                    Some(KittyImageFormat::Rgb) => {
                        let img = DynamicImage::ImageRgb8(
                            RgbImage::from_vec(width, height, data)
                                .ok_or_else(|| anyhow::anyhow!("failed to decode image"))?,
                        );
                        let img = img.into_rgba8();
                        img.into_vec()
                    }
                    _ => data,
                };

                let expected_rgba_len = u128::from(width) * u128::from(height) * 4;
                anyhow::ensure!(
                    expected_rgba_len == data.len() as u128,
                    "transmit data len is {} but it doesn't match width*height*4 {}x{}x4 = {}",
                    data.len(),
                    width,
                    height,
                    expected_rgba_len
                );

                ImageDataType::new_single_frame(width, height, data)
            }
            Some(KittyImageFormat::Png) => {
                let info = dimensions(&data)?;
                check_image_dimensions(info.width, info.height)?;
                let decoded = image::load_from_memory(&data).context("decode png")?;
                let (width, height) = decoded.dimensions();
                check_image_dimensions(width, height)?;
                let data = decoded.into_rgba8().into_vec();
                ImageDataType::new_single_frame(width, height, data)
            }
        };

        Ok((id, no, img))
    }

    fn kitty_img_transmit(
        &mut self,
        transmit: KittyImageTransmit,
        verbosity: KittyImageVerbosity,
    ) -> anyhow::Result<u32> {
        let alt_text = resolve_kitty_alt_text(&transmit);
        let (image_id, image_number, img) = self.kitty_img_transmit_inner(transmit)?;

        // Kitty images are mutable by frame compose/transmit operations.  Do
        // not route them through the content-deduplicating image cache: two
        // image IDs with identical initial pixels must remain independent.
        let img = Arc::new(ImageData::with_data(img));
        self.kitty_img.record_id_to_data(image_id, img)?;
        if let Some(image_number) = image_number {
            self.kitty_img.record_number_mapping(image_number, image_id);
        }
        self.kitty_img.max_image_id = self.kitty_img.max_image_id.max(image_id);
        if let Some(text) = alt_text {
            if let Some(handler) = self.alert_handler.as_mut() {
                handler.alert(Alert::ImageAltText { image_id, text });
            }
        }

        if image_number.is_some() {
            self.kitty_send_response(
                verbosity,
                true,
                Some(image_id),
                image_number,
                "OK".to_string(),
            );
        }

        Ok(image_id)
    }

    fn coalesce_kitty_accumulation(&mut self, img: KittyImage) -> anyhow::Result<KittyImage> {
        if self.kitty_img.accumulator.is_empty() {
            Ok(img)
        } else {
            let final_verbosity = img.verbosity();
            self.kitty_img.accumulate_chunk(img)?;

            let total_bytes = std::mem::take(&mut self.kitty_img.accumulator_encoded_bytes);
            let accumulated = std::mem::take(&mut self.kitty_img.accumulator);
            let mut accumulated = accumulated.into_iter();
            let first = accumulated
                .next()
                .ok_or_else(|| anyhow::anyhow!("Kitty image accumulator unexpectedly empty"))?;
            let (mut trans, place) = match first {
                KittyImage::TransmitData { transmit, .. } => (transmit, None),
                KittyImage::TransmitDataAndDisplay {
                    transmit,
                    placement,
                    ..
                } => (transmit, Some(placement)),
                _ => unreachable!(),
            };

            let mut coalesced =
                match std::mem::replace(&mut trans.data, KittyImageData::DirectBin(Vec::new())) {
                    KittyImageData::DirectBin(data) => data,
                    _ => unreachable!("accumulator stores only materialized direct chunks"),
                };
            let additional = total_bytes
                .checked_sub(coalesced.len())
                .context("kitty accumulator byte count underflow")?;
            coalesced
                .try_reserve_exact(additional)
                .context("reserving Kitty image coalescing buffer")?;
            for item in accumulated {
                let transmit = match item {
                    KittyImage::TransmitData { transmit, .. }
                    | KittyImage::TransmitDataAndDisplay { transmit, .. } => transmit,
                    _ => unreachable!("accumulator stores only transmission chunks"),
                };
                match transmit.data {
                    KittyImageData::DirectBin(mut data) => coalesced.append(&mut data),
                    _ => unreachable!("accumulator stores only materialized direct chunks"),
                }
            }
            anyhow::ensure!(
                coalesced.len() == total_bytes,
                "kitty accumulator byte accounting mismatch: retained={} expected={}",
                coalesced.len(),
                total_bytes
            );
            trans.data = KittyImageData::DirectBin(coalesced);
            trans.more_data_follows = false;

            if let Some(placement) = place {
                Ok(KittyImage::TransmitDataAndDisplay {
                    transmit: trans,
                    placement,
                    verbosity: final_verbosity,
                })
            } else {
                Ok(KittyImage::TransmitData {
                    transmit: trans,
                    verbosity: final_verbosity,
                })
            }
        }
    }
}

/// Make a copy of the source region.
/// Ideally we wouldn't need this, but Rust's mutability rules
/// make it very awkward to mutably reference a frame while
/// an immutable reference exists to a separate frame.
fn clip_view(
    width: u32,
    height: u32,
    data: &[u8],
    src_x: Option<u32>,
    src_y: Option<u32>,
    view_width: Option<u32>,
    view_height: Option<u32>,
) -> anyhow::Result<RgbaImage> {
    let src = ImageBuffer::from_raw(width, height, data)
        .ok_or_else(|| anyhow::anyhow!("ill formed image"))?;

    let (src_x, src_y, view_width, view_height) =
        clipped_view_region(width, height, src_x, src_y, view_width, view_height)?;

    if view_width == 0 || view_height == 0 {
        return Ok(RgbaImage::new(view_width, view_height));
    }

    let view = src
        .try_view(src_x, src_y, view_width, view_height)
        .context("Kitty source image region is outside frame bounds")?;

    let mut tmp = RgbaImage::new(view_width, view_height);
    tmp.copy_from(&*view, 0, 0).context("copy source image")?;
    Ok(tmp)
}

/// Validate and clip a Kitty source rectangle without reading or allocating
/// its pixel payload. This lets fully off-destination compositions terminate
/// before cloning a potentially large frame while retaining source-coordinate
/// error semantics.
fn clipped_view_region(
    width: u32,
    height: u32,
    src_x: Option<u32>,
    src_y: Option<u32>,
    view_width: Option<u32>,
    view_height: Option<u32>,
) -> anyhow::Result<(u32, u32, u32, u32)> {
    let src_x = src_x.unwrap_or(0);
    let src_y = src_y.unwrap_or(0);
    anyhow::ensure!(
        src_x <= width && src_y <= height,
        "Kitty source image origin ({src_x},{src_y}) is outside frame bounds {width}x{height}"
    );

    let (view_width, view_height) = image::imageops::overlay_bounds(
        (width, height),
        (view_width.unwrap_or(width), view_height.unwrap_or(height)),
        src_x,
        src_y,
    );
    Ok((src_x, src_y, view_width, view_height))
}

fn blit<D, S, P>(
    dest: &mut D,
    src: &S,
    x: u32,
    y: u32,
    mode: KittyFrameCompositionMode,
) -> anyhow::Result<()>
where
    D: GenericImage<Pixel = P>,
    S: GenericImageView<Pixel = P>,
{
    match mode {
        KittyFrameCompositionMode::Overwrite => {
            ::image::imageops::replace(dest, src, x.into(), y.into());
        }
        KittyFrameCompositionMode::AlphaBlending => {
            ::image::imageops::overlay(dest, src, x.into(), y.into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ColorPalette;
    use crate::{AlertHandler, TerminalConfiguration, TerminalSize};
    use frankenterm_cell::image::{ImageDataValidationLimits, ImageDataValidationSummary};
    use serde_json::Value;
    use std::sync::Mutex;

    #[derive(Debug)]
    struct KittyAlertTestConfig {
        max_transmission: usize,
    }

    impl TerminalConfiguration for KittyAlertTestConfig {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn enable_kitty_graphics(&self) -> bool {
            true
        }

        fn kitty_image_max_transmission_bytes(&self) -> usize {
            self.max_transmission
        }
    }

    struct RecordingAlertHandler {
        alerts: Arc<Mutex<Vec<Alert>>>,
    }

    impl AlertHandler for RecordingAlertHandler {
        fn alert(&mut self, alert: Alert) {
            self.alerts.lock().unwrap().push(alert);
        }
    }

    fn terminal_with_alerts(max_transmission: usize) -> (TerminalState, Arc<Mutex<Vec<Alert>>>) {
        let alerts = Arc::new(Mutex::new(Vec::new()));
        let config: Arc<dyn TerminalConfiguration> =
            Arc::new(KittyAlertTestConfig { max_transmission });
        let mut terminal = TerminalState::new(
            TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 640,
                pixel_height: 384,
                dpi: 96,
            },
            config,
            "test-program",
            "1.0",
            Box::new(std::io::sink()),
        );
        terminal.set_notification_handler(Box::new(RecordingAlertHandler {
            alerts: Arc::clone(&alerts),
        }));
        (terminal, alerts)
    }

    fn rgba_image(width: u32, height: u32, data: Vec<u8>) -> Arc<ImageData> {
        Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            width, height, data,
        )))
    }

    fn assert_embedded_hashes_match_pixels(image: &ImageData) {
        match &*image.data() {
            ImageDataType::Rgba8 { data, hash, .. } => {
                assert_eq!(*hash, ImageDataType::hash_bytes(data));
            }
            ImageDataType::AnimRgba8 { frames, hashes, .. } => {
                assert_eq!(frames.len(), hashes.len());
                for (frame, hash) in frames.iter().zip(hashes) {
                    assert_eq!(*hash, ImageDataType::hash_bytes(frame));
                }
            }
            ImageDataType::EncodedFile(_) | ImageDataType::EncodedLease(_) => {
                panic!("Kitty mutation test retained encoded image data")
            }
        }
    }

    fn seed_validation_authority(image: &ImageData) -> ([u8; 32], ImageDataValidationSummary) {
        let revision = image.current_content_hash();
        let validated = image
            .normalize_for_content_revision_with_limits(
                revision,
                usize::MAX,
                ImageDataValidationLimits::UNBOUNDED,
                &|| false,
            )
            .expect("test image is structurally valid");
        assert!(validated.replacement.is_none());
        assert_eq!(
            image.validated_summary_for_content_revision(
                revision,
                ImageDataValidationLimits::UNBOUNDED,
            ),
            Some(validated.summary)
        );
        (revision, validated.summary)
    }

    fn assert_validation_authority(
        image: &ImageData,
        revision: [u8; 32],
        summary: ImageDataValidationSummary,
    ) {
        assert_eq!(
            image.validated_summary_for_content_revision(
                revision,
                ImageDataValidationLimits::UNBOUNDED,
            ),
            Some(summary),
            "a rejected nonmutating Kitty command must not acquire data_mut and clear validation authority"
        );
    }

    fn rgba_transmit(
        image_id: Option<u32>,
        image_number: Option<u32>,
        width: u32,
        height: u32,
        data: Vec<u8>,
    ) -> KittyImageTransmit {
        KittyImageTransmit {
            format: Some(KittyImageFormat::Rgba),
            data: KittyImageData::DirectBin(data),
            width: Some(width),
            height: Some(height),
            image_id,
            image_number,
            compression: KittyImageCompression::None,
            more_data_follows: false,
            alt_text: None,
        }
    }

    fn transmission_chunk(data: KittyImageData, more_data_follows: bool) -> KittyImage {
        KittyImage::TransmitData {
            transmit: KittyImageTransmit {
                format: Some(KittyImageFormat::Rgba),
                data,
                width: Some(1),
                height: Some(1),
                image_id: Some(7),
                image_number: None,
                compression: KittyImageCompression::None,
                more_data_follows,
                alt_text: None,
            },
            verbosity: KittyImageVerbosity::Quiet,
        }
    }

    fn record_test_placement(
        terminal: &mut TerminalState,
        image_id: u32,
        placement_id: Option<u32>,
        first_visible_row: usize,
        rows: usize,
    ) {
        let first_row = terminal
            .screen()
            .visible_row_to_stable_row(i64::try_from(first_visible_row).unwrap());
        terminal.kitty_img.placements.insert(
            (image_id, placement_id),
            PlacementInfo {
                first_row,
                rows,
                cols: 1,
            },
        );
    }

    fn begin_dirty_tracking(terminal: &mut TerminalState) -> frankenterm_surface::SequenceNo {
        terminal.increment_seqno();
        let baseline = terminal.current_seqno();
        terminal
            .screen_mut()
            .for_each_phys_line_mut(|_, line| line.update_last_change_seqno(baseline));
        terminal.increment_seqno();
        baseline
    }

    fn dirty_rows_since(
        terminal: &TerminalState,
        seqno: frankenterm_surface::SequenceNo,
    ) -> Vec<usize> {
        let mut dirty = Vec::new();
        terminal.screen().for_each_phys_line(|row, line| {
            if line.changed_since(seqno) {
                dirty.push(row);
            }
        });
        dirty
    }

    #[test]
    fn placement_stable_rows_rejects_unrepresentable_end() {
        assert_eq!(
            placement_stable_rows(PlacementInfo {
                first_row: StableRowIndex::MAX - 1,
                rows: 1,
                cols: 1,
            }),
            Some((StableRowIndex::MAX - 1)..StableRowIndex::MAX)
        );
        assert_eq!(
            placement_stable_rows(PlacementInfo {
                first_row: StableRowIndex::MAX,
                rows: 1,
                cols: 1,
            }),
            None
        );
        assert_eq!(
            placement_stable_rows(PlacementInfo {
                first_row: StableRowIndex::MAX - 1,
                rows: 2,
                cols: 1,
            }),
            None
        );
    }

    #[test]
    fn default_max_transmission_bytes_is_16_mib() {
        let state = KittyImageState::default();
        assert_eq!(
            state.max_transmission_bytes(),
            DEFAULT_KITTY_IMAGE_MAX_TRANSMISSION_BYTES,
        );
        assert_eq!(state.max_transmission_bytes(), 16 * 1024 * 1024);
    }

    #[test]
    fn set_max_transmission_bytes_overrides_default() {
        let mut state = KittyImageState::default();
        let new_cap = 32 * 1024 * 1024;
        state.set_max_transmission_bytes(new_cap);
        assert_eq!(state.max_transmission_bytes(), new_cap);
    }

    #[test]
    fn lowering_transmission_cap_discards_now_oversized_accumulator() {
        let mut state = KittyImageState::default();
        state
            .accumulate_chunk(transmission_chunk(
                KittyImageData::DirectBin(vec![1, 2, 3, 4]),
                true,
            ))
            .unwrap();

        state.set_max_transmission_bytes(3);
        assert!(state.accumulator.is_empty());
        assert_eq!(state.accumulator_encoded_bytes, 0);
    }

    #[test]
    fn max_transmission_bytes_is_independent_of_image_budget() {
        // The two caps cover different attack surfaces:
        //   - max_transmission_bytes: per-image upload cap (DoS via
        //     adversarial single-image transmissions)
        //   - image_budget_bytes: total resident-image RAM cap
        //     (fairness across many legitimate images)
        // Mutating one must not change the other.
        let mut state = KittyImageState::default();
        let baseline_budget = state.image_budget_bytes;

        state.set_max_transmission_bytes(1024);
        assert_eq!(state.image_budget_bytes, baseline_budget);

        state.image_budget_bytes = 64 * 1024 * 1024;
        assert_eq!(state.max_transmission_bytes(), 1024);
    }

    #[test]
    fn cap_can_be_lowered_for_resource_constrained_hosts() {
        // The bead lists 16 MiB as the *default*. Operators on
        // resource-constrained hosts (CI runners, embedded targets)
        // can lower it; the API must accept any non-zero value.
        let mut state = KittyImageState::default();
        state.set_max_transmission_bytes(64 * 1024);
        assert_eq!(state.max_transmission_bytes(), 64 * 1024);
    }

    #[test]
    fn cap_can_be_raised_for_large_image_workflows() {
        // 4 K AI-generated previews, scientific-imaging tools, etc.
        // need higher caps. The continuation bead wires this to
        // [kitty.image] config; until then the runtime setter is
        // the override path.
        let mut state = KittyImageState::default();
        state.set_max_transmission_bytes(256 * 1024 * 1024);
        assert_eq!(state.max_transmission_bytes(), 256 * 1024 * 1024);
    }

    #[test]
    fn bounded_zlib_decompression_accepts_exact_limit() {
        let plain = vec![0x5au8; KITTY_ZLIB_OUTPUT_CHUNK_BYTES + 17];
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&plain, 6);

        assert_eq!(
            decompress_kitty_zlib_bounded(&compressed, plain.len()).unwrap(),
            plain
        );
    }

    #[test]
    fn bounded_zlib_decompression_rejects_first_byte_over_limit() {
        let plain = vec![0x3cu8; KITTY_ZLIB_OUTPUT_CHUNK_BYTES + 17];
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&plain, 6);

        let err = decompress_kitty_zlib_bounded(&compressed, plain.len() - 1).unwrap_err();
        assert!(err.to_string().contains("exceeds per-image cap"));
    }

    #[test]
    fn bounded_zlib_decompression_rejects_corrupt_stream() {
        let err = decompress_kitty_zlib_bounded(&[0x78, 0x9c, 0xff], 1024).unwrap_err();
        assert!(err.to_string().contains("decompressing Kitty image data"));
    }

    #[test]
    fn accumulator_enforces_aggregate_materialized_byte_cap() {
        let mut state = KittyImageState::default();
        state.set_max_transmission_bytes(4);
        state
            .accumulate_chunk(transmission_chunk(
                KittyImageData::DirectBin(vec![1, 2, 3]),
                true,
            ))
            .unwrap();

        let err = state
            .accumulate_chunk(transmission_chunk(
                KittyImageData::DirectBin(vec![4, 5]),
                false,
            ))
            .unwrap_err();
        assert!(err.to_string().contains("accumulated payload"));
        assert!(state.accumulator.is_empty());
        assert_eq!(state.accumulator_encoded_bytes, 0);
    }

    #[test]
    fn accumulator_chunk_count_overflow_discards_incomplete_transfer() {
        let mut state = KittyImageState {
            accumulator: vec![
                transmission_chunk(KittyImageData::DirectBin(Vec::new()), true);
                MAX_KITTY_ACCUMULATOR_CHUNKS
            ],
            ..Default::default()
        };

        let err = state
            .accumulate_chunk(transmission_chunk(
                KittyImageData::DirectBin(Vec::new()),
                false,
            ))
            .unwrap_err();
        assert!(err.to_string().contains("more than"));
        assert!(state.accumulator.is_empty());
        assert_eq!(state.accumulator_encoded_bytes, 0);
    }

    #[test]
    fn accumulator_decodes_each_base64_chunk_once_and_coalesces_exactly() {
        let (mut terminal, _alerts) = terminal_with_alerts(4);
        terminal
            .kitty_img
            .accumulate_chunk(transmission_chunk(
                KittyImageData::Direct("AQI=".to_string()),
                true,
            ))
            .unwrap();
        assert!(matches!(
            &terminal.kitty_img.accumulator[0],
            KittyImage::TransmitData {
                transmit: KittyImageTransmit {
                    data: KittyImageData::DirectBin(data),
                    ..
                },
                ..
            } if data.as_slice() == [1, 2]
        ));

        let coalesced = terminal
            .coalesce_kitty_accumulation(transmission_chunk(
                KittyImageData::Direct("AwQ=".to_string()),
                false,
            ))
            .unwrap();
        let KittyImage::TransmitData { transmit, .. } = coalesced else {
            panic!("coalescing changed the Kitty transmission variant");
        };
        assert_eq!(transmit.data, KittyImageData::DirectBin(vec![1, 2, 3, 4]));
        assert!(!transmit.more_data_follows);
        assert!(terminal.kitty_img.accumulator.is_empty());
        assert_eq!(terminal.kitty_img.accumulator_encoded_bytes, 0);
    }

    #[test]
    fn record_id_to_data_replacement_accounts_image_zero() {
        let mut state = KittyImageState {
            image_budget_bytes: 8,
            ..Default::default()
        };
        state
            .record_id_to_data(0, rgba_image(1, 1, vec![1; 4]))
            .unwrap();
        state
            .record_id_to_data(0, rgba_image(2, 1, vec![2; 8]))
            .unwrap();

        assert_eq!(state.used_memory, 8);
        assert_eq!(state.id_to_data.len(), 1);
        assert_eq!(
            checked_image_len(state.id_to_data.get(&0).unwrap()).unwrap(),
            8
        );
    }

    #[test]
    fn record_id_to_data_evicts_only_after_complete_admission_plan() {
        let mut state = KittyImageState {
            image_budget_bytes: 4,
            ..Default::default()
        };
        state
            .record_id_to_data(1, rgba_image(1, 1, vec![1; 4]))
            .unwrap();
        state.record_number_mapping(11, 1);
        state
            .record_id_to_data(2, rgba_image(1, 1, vec![2; 4]))
            .unwrap();

        assert_eq!(state.used_memory, 4);
        assert!(!state.id_to_data.contains_key(&1));
        assert!(state.id_to_data.contains_key(&2));
        assert!(!state.number_to_id.contains_key(&11));
    }

    #[test]
    fn record_id_to_data_budget_failure_preserves_referenced_state() {
        let mut state = KittyImageState {
            image_budget_bytes: 4,
            ..Default::default()
        };
        state
            .record_id_to_data(1, rgba_image(1, 1, vec![1; 4]))
            .unwrap();
        state.placements.insert(
            (1, None),
            PlacementInfo {
                first_row: 0,
                rows: 1,
                cols: 1,
            },
        );

        let err = state
            .record_id_to_data(2, rgba_image(1, 1, vec![2; 4]))
            .unwrap_err();
        assert!(err.to_string().contains("memory budget exhausted"));
        assert_eq!(state.used_memory, 4);
        assert!(state.id_to_data.contains_key(&1));
        assert!(!state.id_to_data.contains_key(&2));
    }

    #[test]
    fn failed_numbered_transmit_does_not_commit_id_or_mapping() {
        let (mut terminal, _alerts) = terminal_with_alerts(1024);
        terminal.kitty_img.image_budget_bytes = 4;
        let err = terminal
            .kitty_img_transmit(
                rgba_transmit(None, Some(9), 2, 1, vec![0; 8]),
                KittyImageVerbosity::Quiet,
            )
            .unwrap_err();

        assert!(err.to_string().contains("per-image budget"));
        assert_eq!(terminal.kitty_img.max_image_id, 0);
        assert!(!terminal.kitty_img.number_to_id.contains_key(&9));
        assert!(terminal.kitty_img.id_to_data.is_empty());
        assert_eq!(terminal.kitty_img.used_memory, 0);
    }

    #[test]
    fn successful_numbered_transmit_commits_data_mapping_and_id_together() {
        let (mut terminal, _alerts) = terminal_with_alerts(1024);
        terminal
            .kitty_img_transmit(
                rgba_transmit(None, Some(9), 1, 1, vec![0; 4]),
                KittyImageVerbosity::Quiet,
            )
            .unwrap();

        assert_eq!(terminal.kitty_img.max_image_id, 1);
        assert_eq!(terminal.kitty_img.number_to_id.get(&9), Some(&1));
        assert!(terminal.kitty_img.id_to_data.contains_key(&1));
        assert_eq!(terminal.kitty_img.used_memory, 4);
    }

    #[test]
    fn kitty_image_number_allocation_rejects_id_overflow() {
        let (mut terminal, _alerts) = terminal_with_alerts(1024);
        terminal.kitty_img.max_image_id = u32::MAX;

        let err = terminal
            .kitty_img_transmit_inner(KittyImageTransmit {
                format: Some(KittyImageFormat::Rgba),
                data: KittyImageData::DirectBin(vec![0u8; 4]),
                width: Some(1),
                height: Some(1),
                image_id: None,
                image_number: Some(9),
                compression: KittyImageCompression::None,
                more_data_follows: false,
                alt_text: None,
            })
            .unwrap_err();

        assert!(err.to_string().contains("image id space exhausted"));
        assert!(!terminal.kitty_img.number_to_id.contains_key(&9));
    }

    #[test]
    fn next_kitty_frame_number_checks_protocol_boundary() {
        assert_eq!(next_kitty_frame_number(0).unwrap(), 1);
        assert_eq!(
            next_kitty_frame_number((u32::MAX as usize).saturating_sub(1)).unwrap(),
            u32::MAX
        );
        assert!(next_kitty_frame_number(u32::MAX as usize).is_err());
    }

    #[test]
    fn frame_compose_revisions_and_dirties_every_placed_row() {
        let (mut terminal, _alerts) = terminal_with_alerts(1024);
        let image = rgba_image(2, 1, vec![255, 0, 0, 255, 0, 0, 255, 255]);
        terminal
            .kitty_img
            .record_id_to_data(7, Arc::clone(&image))
            .unwrap();
        record_test_placement(&mut terminal, 7, Some(1), 0, 1);
        record_test_placement(&mut terminal, 7, Some(2), 2, 2);
        let old_revision = image.current_content_hash();
        let baseline = begin_dirty_tracking(&mut terminal);

        terminal
            .kitty_frame_compose(
                KittyImageFrameCompose {
                    image_id: Some(7),
                    image_number: None,
                    target_frame: Some(1),
                    source_frame: Some(1),
                    x: Some(1),
                    y: Some(0),
                    w: Some(1),
                    h: Some(1),
                    src_x: Some(0),
                    src_y: Some(0),
                    composition_mode: KittyFrameCompositionMode::Overwrite,
                },
                KittyImageVerbosity::Quiet,
            )
            .unwrap();

        assert_ne!(image.current_content_hash(), old_revision);
        assert_eq!(dirty_rows_since(&terminal, baseline), vec![0, 2, 3]);
        let image_data = image.data();
        let ImageDataType::Rgba8 { data: pixels, .. } = &*image_data else {
            panic!("frame compose changed a static image to an unexpected variant");
        };
        assert_eq!(
            pixels.as_slice(),
            &[255, 0, 0, 255, 255, 0, 0, 255],
            "the first pixel should have been copied over the second"
        );
        drop(image_data);
        assert_embedded_hashes_match_pixels(&image);
    }

    #[test]
    fn invalid_frame_compose_preserves_validation_authority_without_mutable_access() {
        let (mut terminal, _alerts) = terminal_with_alerts(1024);
        let image = rgba_image(2, 1, vec![1, 2, 3, 255, 4, 5, 6, 255]);
        terminal
            .kitty_img
            .record_id_to_data(7, Arc::clone(&image))
            .unwrap();
        let (revision, summary) = seed_validation_authority(&image);

        let error = terminal
            .kitty_frame_compose(
                KittyImageFrameCompose {
                    image_id: Some(7),
                    image_number: None,
                    target_frame: Some(1),
                    source_frame: Some(2),
                    x: None,
                    y: None,
                    w: None,
                    h: None,
                    src_x: None,
                    src_y: None,
                    composition_mode: KittyFrameCompositionMode::Overwrite,
                },
                KittyImageVerbosity::Quiet,
            )
            .unwrap_err();

        assert!(error.to_string().contains("only a single frame"));
        assert_validation_authority(&image, revision, summary);
    }

    #[test]
    fn fully_clipped_frame_compose_rejects_zero_dimension_stored_image() {
        let (mut terminal, _alerts) = terminal_with_alerts(1024);
        let pixels = Vec::new();
        let image = Arc::new(ImageData::with_data(ImageDataType::Rgba8 {
            width: 0,
            height: 1,
            hash: ImageDataType::hash_bytes(&pixels),
            data: pixels,
        }));
        terminal
            .kitty_img
            .record_id_to_data(7, Arc::clone(&image))
            .unwrap();
        record_test_placement(&mut terminal, 7, None, 0, 1);
        let original_revision = image.current_content_hash();
        let baseline = begin_dirty_tracking(&mut terminal);

        let error = terminal
            .kitty_frame_compose(
                KittyImageFrameCompose {
                    image_id: Some(7),
                    image_number: None,
                    target_frame: Some(1),
                    source_frame: Some(1),
                    x: None,
                    y: None,
                    w: None,
                    h: None,
                    src_x: None,
                    src_y: None,
                    composition_mode: KittyFrameCompositionMode::Overwrite,
                },
                KittyImageVerbosity::Quiet,
            )
            .expect_err("zero-width stored images must fail before the clipped no-op path");

        assert!(error.to_string().contains("must be nonzero"));
        assert_eq!(image.current_content_hash(), original_revision);
        assert!(dirty_rows_since(&terminal, baseline).is_empty());
    }

    #[test]
    fn static_frame_compose_rejects_out_of_bounds_source_without_mutation_or_dirty_rows() {
        let (mut terminal, _alerts) = terminal_with_alerts(1024);
        let image = rgba_image(2, 1, vec![1, 2, 3, 255, 4, 5, 6, 255]);
        terminal
            .kitty_img
            .record_id_to_data(7, Arc::clone(&image))
            .unwrap();
        record_test_placement(&mut terminal, 7, None, 0, 1);
        let (revision, summary) = seed_validation_authority(&image);
        let baseline = begin_dirty_tracking(&mut terminal);

        let error = terminal
            .kitty_frame_compose(
                KittyImageFrameCompose {
                    image_id: Some(7),
                    image_number: None,
                    target_frame: Some(1),
                    source_frame: Some(1),
                    // Destination is also fully outside. Source validation
                    // must still fail before the allocation-free no-op path.
                    x: Some(2),
                    y: None,
                    w: Some(1),
                    h: Some(1),
                    src_x: Some(3),
                    src_y: Some(0),
                    composition_mode: KittyFrameCompositionMode::Overwrite,
                },
                KittyImageVerbosity::Quiet,
            )
            .unwrap_err();

        assert!(error.to_string().contains("outside frame bounds"));
        assert_validation_authority(&image, revision, summary);
        assert!(dirty_rows_since(&terminal, baseline).is_empty());
    }

    #[test]
    fn static_frame_compose_exact_source_edge_is_zero_area_noop() {
        let (mut terminal, _alerts) = terminal_with_alerts(1024);
        let image = rgba_image(2, 1, vec![1, 2, 3, 255, 4, 5, 6, 255]);
        terminal
            .kitty_img
            .record_id_to_data(7, Arc::clone(&image))
            .unwrap();
        record_test_placement(&mut terminal, 7, None, 0, 1);
        let (revision, summary) = seed_validation_authority(&image);
        let baseline = begin_dirty_tracking(&mut terminal);

        terminal
            .kitty_frame_compose(
                KittyImageFrameCompose {
                    image_id: Some(7),
                    image_number: None,
                    target_frame: Some(1),
                    source_frame: Some(1),
                    x: None,
                    y: None,
                    w: Some(1),
                    h: Some(1),
                    src_x: Some(2),
                    src_y: Some(0),
                    composition_mode: KittyFrameCompositionMode::Overwrite,
                },
                KittyImageVerbosity::Quiet,
            )
            .unwrap();

        assert_validation_authority(&image, revision, summary);
        assert!(dirty_rows_since(&terminal, baseline).is_empty());
    }

    #[test]
    fn animation_frame_compose_outside_destination_is_noop_without_frame_hashing() {
        let (mut terminal, _alerts) = terminal_with_alerts(1024);
        let first = vec![1, 2, 3, 255];
        let second = vec![4, 5, 6, 255];
        let animation = Arc::new(ImageData::with_data(ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::ZERO, Duration::from_millis(20)],
            hashes: vec![
                ImageDataType::hash_bytes(&first),
                ImageDataType::hash_bytes(&second),
            ],
            frames: vec![first, second],
        }));
        terminal
            .kitty_img
            .record_id_to_data(7, Arc::clone(&animation))
            .unwrap();
        record_test_placement(&mut terminal, 7, None, 0, 1);
        let (revision, summary) = seed_validation_authority(&animation);
        let baseline = begin_dirty_tracking(&mut terminal);

        terminal
            .kitty_frame_compose(
                KittyImageFrameCompose {
                    image_id: Some(7),
                    image_number: None,
                    target_frame: Some(2),
                    source_frame: Some(1),
                    x: Some(1),
                    y: Some(0),
                    w: None,
                    h: None,
                    src_x: None,
                    src_y: None,
                    composition_mode: KittyFrameCompositionMode::Overwrite,
                },
                KittyImageVerbosity::Quiet,
            )
            .unwrap();

        assert_validation_authority(&animation, revision, summary);
        assert!(dirty_rows_since(&terminal, baseline).is_empty());
    }

    #[test]
    fn static_frame_transmit_edit_outside_destination_is_noop_without_hashing() {
        let (mut terminal, _alerts) = terminal_with_alerts(1024);
        let image = rgba_image(1, 1, vec![1, 2, 3, 255]);
        terminal
            .kitty_img
            .record_id_to_data(7, Arc::clone(&image))
            .unwrap();
        record_test_placement(&mut terminal, 7, None, 0, 1);
        let (revision, summary) = seed_validation_authority(&image);
        let baseline = begin_dirty_tracking(&mut terminal);

        terminal
            .kitty_frame_transmit(
                rgba_transmit(Some(7), None, 1, 1, vec![9, 8, 7, 255]),
                KittyImageFrame {
                    x: Some(1),
                    y: Some(0),
                    base_frame: None,
                    frame_number: Some(1),
                    duration_ms: Some(20),
                    composition_mode: KittyFrameCompositionMode::Overwrite,
                    background_pixel: None,
                },
                KittyImageVerbosity::Quiet,
            )
            .unwrap();

        assert_validation_authority(&image, revision, summary);
        assert!(dirty_rows_since(&terminal, baseline).is_empty());
    }

    #[test]
    fn frame_transmit_append_updates_memory_revision_and_all_placed_rows() {
        let (mut terminal, _alerts) = terminal_with_alerts(1024);
        terminal.kitty_img.image_budget_bytes = 16;
        let image = rgba_image(2, 1, vec![1, 1, 1, 255, 2, 2, 2, 255]);
        terminal
            .kitty_img
            .record_id_to_data(7, Arc::clone(&image))
            .unwrap();
        record_test_placement(&mut terminal, 7, Some(1), 0, 2);
        let old_revision = image.current_content_hash();
        let baseline = begin_dirty_tracking(&mut terminal);

        terminal
            .kitty_frame_transmit(
                rgba_transmit(Some(7), None, 1, 1, vec![9, 8, 7, 255]),
                KittyImageFrame {
                    x: Some(1),
                    y: Some(0),
                    base_frame: Some(1),
                    frame_number: None,
                    duration_ms: Some(20),
                    composition_mode: KittyFrameCompositionMode::Overwrite,
                    background_pixel: None,
                },
                KittyImageVerbosity::Quiet,
            )
            .unwrap();

        assert_eq!(terminal.kitty_img.used_memory, 16);
        assert_ne!(image.current_content_hash(), old_revision);
        assert_eq!(dirty_rows_since(&terminal, baseline), vec![0, 1]);
        let data = image.data();
        let ImageDataType::AnimRgba8 {
            frames,
            durations,
            hashes,
            ..
        } = &*data
        else {
            panic!("frame append did not promote the image to an animation");
        };
        assert_eq!(frames.len(), 2);
        assert_eq!(durations.len(), 2);
        assert_eq!(hashes.len(), 2);
        assert_eq!(&frames[1][4..], &[9, 8, 7, 255]);
        assert_eq!(durations[1], Duration::from_millis(20));
        drop(data);
        assert_embedded_hashes_match_pixels(&image);
    }

    #[test]
    fn animation_compose_append_and_edit_publish_guard_repaired_hashes() {
        let (mut terminal, _alerts) = terminal_with_alerts(1024);
        terminal.kitty_img.image_budget_bytes = 32;
        let first = vec![1, 2, 3, 255, 4, 5, 6, 255];
        let second = vec![7, 8, 9, 255, 10, 11, 12, 255];
        let animation = Arc::new(ImageData::with_data(ImageDataType::AnimRgba8 {
            width: 2,
            height: 1,
            durations: vec![Duration::ZERO, Duration::from_millis(20)],
            hashes: vec![
                ImageDataType::hash_bytes(&first),
                ImageDataType::hash_bytes(&second),
            ],
            frames: vec![first, second],
        }));
        terminal
            .kitty_img
            .record_id_to_data(7, Arc::clone(&animation))
            .unwrap();

        terminal
            .kitty_frame_compose(
                KittyImageFrameCompose {
                    image_id: Some(7),
                    image_number: None,
                    target_frame: Some(2),
                    source_frame: Some(1),
                    x: Some(1),
                    y: Some(0),
                    w: Some(1),
                    h: Some(1),
                    src_x: Some(0),
                    src_y: Some(0),
                    composition_mode: KittyFrameCompositionMode::Overwrite,
                },
                KittyImageVerbosity::Quiet,
            )
            .unwrap();
        assert_embedded_hashes_match_pixels(&animation);

        terminal
            .kitty_frame_transmit(
                rgba_transmit(Some(7), None, 1, 1, vec![21, 22, 23, 255]),
                KittyImageFrame {
                    x: Some(0),
                    y: Some(0),
                    base_frame: Some(2),
                    frame_number: None,
                    duration_ms: Some(30),
                    composition_mode: KittyFrameCompositionMode::Overwrite,
                    background_pixel: None,
                },
                KittyImageVerbosity::Quiet,
            )
            .unwrap();
        assert_embedded_hashes_match_pixels(&animation);

        terminal
            .kitty_frame_transmit(
                rgba_transmit(Some(7), None, 1, 1, vec![31, 32, 33, 255]),
                KittyImageFrame {
                    x: Some(1),
                    y: Some(0),
                    base_frame: None,
                    frame_number: Some(2),
                    duration_ms: Some(40),
                    composition_mode: KittyFrameCompositionMode::Overwrite,
                    background_pixel: None,
                },
                KittyImageVerbosity::Quiet,
            )
            .unwrap();
        assert_embedded_hashes_match_pixels(&animation);
    }

    #[test]
    fn frame_append_budget_failure_is_transactional_and_does_not_dirty_rows() {
        let (mut terminal, _alerts) = terminal_with_alerts(1024);
        terminal.kitty_img.image_budget_bytes = 8;
        let image = rgba_image(2, 1, vec![1, 1, 1, 255, 2, 2, 2, 255]);
        terminal
            .kitty_img
            .record_id_to_data(7, Arc::clone(&image))
            .unwrap();
        record_test_placement(&mut terminal, 7, None, 0, 1);
        let old_revision = image.current_content_hash();
        let (validated_revision, validated_summary) = seed_validation_authority(&image);
        let baseline = begin_dirty_tracking(&mut terminal);

        let err = terminal
            .kitty_frame_transmit(
                rgba_transmit(Some(7), None, 1, 1, vec![9, 8, 7, 255]),
                KittyImageFrame {
                    x: Some(1),
                    y: Some(0),
                    base_frame: Some(1),
                    frame_number: None,
                    duration_ms: Some(20),
                    composition_mode: KittyFrameCompositionMode::Overwrite,
                    background_pixel: None,
                },
                KittyImageVerbosity::Quiet,
            )
            .unwrap_err();

        assert!(err.to_string().contains("exceeding per-image budget"));
        assert_eq!(terminal.kitty_img.used_memory, 8);
        assert_eq!(image.current_content_hash(), old_revision);
        assert_validation_authority(&image, validated_revision, validated_summary);
        assert!(matches!(&*image.data(), ImageDataType::Rgba8 { .. }));
        assert!(dirty_rows_since(&terminal, baseline).is_empty());
    }

    #[test]
    fn frame_append_rejects_wire_incompatible_frame_count_without_growth() {
        let (mut terminal, _alerts) = terminal_with_alerts(1024);
        let frame = vec![0u8; 4];
        let frame_hash = ImageDataType::hash_bytes(&frame);
        let animation = Arc::new(ImageData::with_data(ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            frames: vec![frame; MAX_KITTY_ANIMATION_FRAMES],
            durations: vec![Duration::from_millis(1); MAX_KITTY_ANIMATION_FRAMES],
            hashes: vec![frame_hash; MAX_KITTY_ANIMATION_FRAMES],
        }));
        let resident_bytes = MAX_KITTY_ANIMATION_FRAMES * 4;
        terminal.kitty_img.image_budget_bytes = resident_bytes + 4;
        terminal
            .kitty_img
            .record_id_to_data(7, Arc::clone(&animation))
            .unwrap();
        let (validated_revision, validated_summary) = seed_validation_authority(&animation);

        let err = terminal
            .kitty_frame_transmit(
                rgba_transmit(Some(7), None, 1, 1, vec![1, 2, 3, 4]),
                KittyImageFrame {
                    x: None,
                    y: None,
                    base_frame: None,
                    frame_number: None,
                    duration_ms: None,
                    composition_mode: KittyFrameCompositionMode::Overwrite,
                    background_pixel: None,
                },
                KittyImageVerbosity::Quiet,
            )
            .unwrap_err();

        assert!(err.to_string().contains("frame count would exceed"));
        assert_eq!(terminal.kitty_img.used_memory, resident_bytes);
        assert_validation_authority(&animation, validated_revision, validated_summary);
        let data = animation.data();
        let ImageDataType::AnimRgba8 { frames, .. } = &*data else {
            panic!("animation changed variant after rejected append");
        };
        assert_eq!(frames.len(), MAX_KITTY_ANIMATION_FRAMES);
    }

    #[test]
    fn identical_kitty_ids_do_not_share_mutable_image_storage() {
        let (mut terminal, _alerts) = terminal_with_alerts(1024);
        let pixels = vec![255, 0, 0, 255, 0, 0, 255, 255];
        terminal
            .kitty_img_transmit(
                rgba_transmit(Some(1), None, 2, 1, pixels.clone()),
                KittyImageVerbosity::Quiet,
            )
            .unwrap();
        terminal
            .kitty_img_transmit(
                rgba_transmit(Some(2), None, 2, 1, pixels.clone()),
                KittyImageVerbosity::Quiet,
            )
            .unwrap();
        let first = Arc::clone(terminal.kitty_img.id_to_data.get(&1).unwrap());
        let second = Arc::clone(terminal.kitty_img.id_to_data.get(&2).unwrap());
        assert!(!Arc::ptr_eq(&first, &second));

        terminal
            .kitty_frame_compose(
                KittyImageFrameCompose {
                    image_id: Some(1),
                    image_number: None,
                    target_frame: Some(1),
                    source_frame: Some(1),
                    x: Some(1),
                    y: Some(0),
                    w: Some(1),
                    h: Some(1),
                    src_x: Some(0),
                    src_y: Some(0),
                    composition_mode: KittyFrameCompositionMode::Overwrite,
                },
                KittyImageVerbosity::Quiet,
            )
            .unwrap();

        let second_data = second.data();
        let ImageDataType::Rgba8 { data, .. } = &*second_data else {
            panic!("second image changed variant");
        };
        assert_eq!(data, &pixels);
    }

    #[test]
    fn kitty_transmit_emits_sanitized_alt_text_alert_after_admission() {
        let (mut terminal, alerts) = terminal_with_alerts(1024);
        let img = KittyImage::TransmitData {
            transmit: KittyImageTransmit {
                format: Some(KittyImageFormat::Rgba),
                data: KittyImageData::DirectBin(vec![0u8; 8 * 8 * 4]),
                width: Some(8),
                height: Some(8),
                image_id: Some(7),
                image_number: None,
                compression: KittyImageCompression::None,
                more_data_follows: false,
                alt_text: Some(b"Sales\tQ3\x07 chart".to_vec()),
            },
            verbosity: KittyImageVerbosity::Quiet,
        };

        terminal.kitty_img(img).unwrap();

        assert_eq!(
            *alerts.lock().unwrap(),
            vec![Alert::ImageAltText {
                image_id: 7,
                text: "Sales Q3 chart".to_string(),
            }],
        );
    }

    #[test]
    fn kitty_rejected_transmit_does_not_emit_alt_text_alert() {
        let (mut terminal, alerts) = terminal_with_alerts(1);
        let img = KittyImage::TransmitData {
            transmit: KittyImageTransmit {
                format: Some(KittyImageFormat::Rgba),
                data: KittyImageData::DirectBin(vec![0u8; 8 * 8 * 4]),
                width: Some(8),
                height: Some(8),
                image_id: Some(7),
                image_number: None,
                compression: KittyImageCompression::None,
                more_data_follows: false,
                alt_text: Some(b"rejected".to_vec()),
            },
            verbosity: KittyImageVerbosity::Quiet,
        };

        assert!(terminal.kitty_img(img).is_err());
        assert!(alerts.lock().unwrap().is_empty());
    }

    #[test]
    fn kitty_file_source_alt_text_falls_back_to_filename() {
        let transmit = KittyImageTransmit {
            format: Some(KittyImageFormat::Png),
            data: KittyImageData::File {
                path: "/tmp/screenshots/sales-chart.png".to_string(),
                data_size: Some(4096),
                data_offset: Some(128),
            },
            width: None,
            height: None,
            image_id: Some(11),
            image_number: None,
            compression: KittyImageCompression::None,
            more_data_follows: false,
            alt_text: None,
        };

        assert_eq!(
            resolve_kitty_alt_text(&transmit),
            Some("sales-chart.png".to_string()),
        );
    }

    fn kitty_graphics_fixture_path(name: &str, file: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("tests/golden/kitty_graphics")
            .join(name)
            .join(file)
    }

    fn read_kitty_graphics_fixture_input(name: &str) -> Vec<u8> {
        let mut input = std::fs::read(kitty_graphics_fixture_path(name, "input.bin"))
            .unwrap_or_else(|err| {
                panic!(
                    "failed to read Kitty graphics fixture input for {}: {}",
                    name, err
                )
            });
        while matches!(input.last(), Some(b'\n' | b'\r')) {
            input.pop();
        }
        input
    }

    fn read_kitty_graphics_fixture_expected(name: &str) -> Value {
        let expected = std::fs::read_to_string(kitty_graphics_fixture_path(name, "expected.json"))
            .unwrap_or_else(|err| {
                panic!(
                    "failed to read Kitty graphics fixture expected JSON for {}: {}",
                    name, err
                )
            });
        serde_json::from_str(&expected).unwrap_or_else(|err| {
            panic!(
                "failed to parse Kitty graphics fixture expected JSON for {}: {}",
                name, err
            )
        })
    }

    #[test]
    fn kitty_graphics_alt_text_golden_fixtures_match_term_layer() {
        for name in ["image_nvim", "yazi", "icat"] {
            let expected = read_kitty_graphics_fixture_expected(name);
            let input = read_kitty_graphics_fixture_input(name);
            let img = KittyImage::parse_apc(&input)
                .unwrap_or_else(|| panic!("fixture {} did not parse as Kitty APC", name));

            let (transmit, placement) = match &img {
                KittyImage::TransmitData { transmit, .. } => (transmit, None),
                KittyImage::TransmitDataAndDisplay {
                    transmit,
                    placement,
                    ..
                } => (transmit, Some(placement)),
                other => panic!(
                    "fixture {} parsed unexpected Kitty image variant: {:?}",
                    name, other
                ),
            };

            assert_eq!(
                transmit.image_id,
                expected["expected_image_id"].as_u64().map(|id| id as u32),
                "fixture {name} image id mismatch",
            );

            let expected_alt_text = expected["expected_alt_text"].as_str().map(str::to_string);
            assert_eq!(
                resolve_kitty_alt_text(transmit),
                expected_alt_text.clone(),
                "fixture {name} alt-text mismatch",
            );

            match expected["expected_placement"]["kind"].as_str() {
                Some("classical") => {
                    let placement =
                        placement.unwrap_or_else(|| panic!("fixture {} missing placement", name));
                    assert_eq!(
                        placement.columns,
                        expected["expected_placement"]["columns"]
                            .as_u64()
                            .map(|v| v as u32),
                        "fixture {name} placement columns mismatch",
                    );
                    assert_eq!(
                        placement.rows,
                        expected["expected_placement"]["rows"]
                            .as_u64()
                            .map(|v| v as u32),
                        "fixture {name} placement rows mismatch",
                    );
                }
                Some("none") => assert!(
                    placement.is_none(),
                    "fixture {} expected no placement, got {:?}",
                    name,
                    placement,
                ),
                other => panic!(
                    "fixture {} has unsupported placement kind: {:?}",
                    name, other
                ),
            }

            if expected["expected_alert"].as_bool().unwrap_or(false) {
                let (mut terminal, alerts) = terminal_with_alerts(1024);
                terminal.kitty_img(img).unwrap();
                assert_eq!(
                    *alerts.lock().unwrap(),
                    vec![Alert::ImageAltText {
                        image_id: expected["expected_image_id"].as_u64().unwrap() as u32,
                        text: expected_alt_text.unwrap(),
                    }],
                    "fixture {name} alert mismatch",
                );
            }
        }
    }
}
