//! Frame slots and frame pacing, the platform-independent half
//! (ft-yccm0.4.2.1).
//!
//! The renderer keeps [`FRAME_SLOTS`] frame slots. Each slot owns one set of
//! per-frame GPU buffers ([`SlotBuffer`]) in shared, write-combined memory:
//! on Apple silicon's unified memory the CPU writes a frame's data exactly
//! where the GPU reads it, with no staging buffer and no copy.
//!
//! The price is that the CPU must never write a slot the GPU is still
//! reading. [`SlotRing`] enforces that:
//!
//! - slots are leased in strict rotation (0, 1, 2, 0, ...), and a lease waits
//!   until the GPU has finished the frame that last used its slot, so at most
//!   [`FRAME_SLOTS`] frames are ever in flight;
//! - a [`SlotLease`] is the only way to obtain a slot for writing, and it
//!   exists only while the slot is not in flight;
//! - submitting a lease turns it into a [`CompletionToken`] that the GPU's
//!   completion handler completes; completing (or dropping) the token frees
//!   the slot.
//!
//! Buffers are sized for the grid by [`SlotSizes::for_grid`] and grown with
//! [`grown_capacity`]: geometrically, only when the grid outgrows them, and
//! never shrunk, so a steady-state frame allocates no GPU object.

use crate::cell_bg::BackgroundUniforms;
use crate::cell_text::TextUniforms;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Frames the renderer keeps in flight at most: one being written by the
/// CPU, one queued, one being drawn, matching `CAMetalLayer`'s default of
/// three drawables.
pub const FRAME_SLOTS: usize = 3;

/// Smallest buffer the renderer allocates. Metal rejects zero-length buffers,
/// and a floor keeps tiny grids from regrowing through many small sizes.
pub const MIN_BUFFER_BYTES: usize = 4096;

/// Bytes of the per-frame uniform block. 256 bytes keeps every offset in the
/// buffer aligned for any Metal constant-buffer binding.
pub const UNIFORMS_BYTES: usize = 256;

/// Bytes per cell of the background grid: one RGBA8 color (ft-yccm0.4.2.2).
pub const CELL_BG_BYTES_PER_CELL: usize = 4;

/// Bytes per glyph instance in the CellText buffer: one
/// [`crate::cell_text::CellText`] (ft-yccm0.4.2.3).
pub const CELL_TEXT_INSTANCE_BYTES: usize = 24;

/// Glyph instances reserved per cell. A wide glyph is one instance spanning
/// two cells and decorations are drawn by the fragment shader, so one per
/// cell covers a full screen; a frame that needs more grows the buffer
/// ([`SlotSizes::for_frame`]).
pub const CELL_TEXT_INSTANCES_PER_CELL: usize = 1;

/// Bytes per row of the per-row table: the row's first instance and its
/// instance count (`u32` each), then 8 bytes reserved for per-row flags.
pub const ROW_TABLE_BYTES_PER_ROW: usize = 16;

/// One of the per-frame buffers every slot owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SlotBuffer {
    /// The frame's [`FrameUniforms`].
    Uniforms,
    /// One background color per cell.
    CellBg,
    /// Glyph instances.
    CellText,
    /// Per-row ranges into the other buffers.
    RowTable,
}

impl SlotBuffer {
    /// Every buffer, in slot (and argument-table binding) order.
    pub const ALL: [Self; 4] = [Self::Uniforms, Self::CellBg, Self::CellText, Self::RowTable];

    /// Position in [`Self::ALL`]; also the buffer's argument-table binding
    /// index in every frame's shaders.
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Stable name for labels and diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Uniforms => "uniforms",
            Self::CellBg => "cell_bg",
            Self::CellText => "cell_text",
            Self::RowTable => "row_table",
        }
    }
}

/// The terminal grid a frame draws, in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GridExtent {
    pub rows: u32,
    pub cols: u32,
}

impl GridExtent {
    /// A grid of `rows x cols` cells, each saturated to `u32::MAX`.
    #[must_use]
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            rows: u32::try_from(rows).unwrap_or(u32::MAX),
            cols: u32::try_from(cols).unwrap_or(u32::MAX),
        }
    }

    /// Cells in the grid.
    #[must_use]
    pub fn cells(self) -> u64 {
        u64::from(self.rows) * u64::from(self.cols)
    }
}

/// Bytes each per-frame buffer needs for one grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotSizes {
    bytes: [usize; SlotBuffer::ALL.len()],
}

impl SlotSizes {
    /// The bytes a frame of `grid` needs in each buffer, or `None` if a size
    /// does not fit in `usize`.
    #[must_use]
    pub fn for_grid(grid: GridExtent) -> Option<Self> {
        let cells = usize::try_from(grid.cells()).ok()?;
        let rows = usize::try_from(grid.rows).ok()?;
        let cell_text = cells
            .checked_mul(CELL_TEXT_INSTANCES_PER_CELL)?
            .checked_mul(CELL_TEXT_INSTANCE_BYTES)?;
        Some(Self {
            bytes: [
                UNIFORMS_BYTES,
                cells.checked_mul(CELL_BG_BYTES_PER_CELL)?,
                cell_text,
                rows.checked_mul(ROW_TABLE_BYTES_PER_ROW)?,
            ],
        })
    }

    /// The bytes a frame of `grid` that draws `text_instances` glyph
    /// instances needs: [`Self::for_grid`], with the CellText buffer large
    /// enough for every instance when there are more than it reserves.
    #[must_use]
    pub fn for_frame(grid: GridExtent, text_instances: usize) -> Option<Self> {
        let mut sizes = Self::for_grid(grid)?;
        let text = text_instances.checked_mul(CELL_TEXT_INSTANCE_BYTES)?;
        let at = SlotBuffer::CellText.index();
        sizes.bytes[at] = sizes.bytes[at].max(text);
        Some(sizes)
    }

    /// Bytes `buffer` needs.
    #[must_use]
    pub fn bytes(&self, buffer: SlotBuffer) -> usize {
        self.bytes[buffer.index()]
    }
}

/// The capacity a buffer of `current` bytes (0 = not allocated yet) should
/// have to hold `required` bytes.
///
/// Returns `current` unchanged whenever it already suffices, so a steady
/// grid never reallocates and a shrinking grid keeps its buffers. Otherwise
/// returns the next power of two at or above `max(required,
/// MIN_BUFFER_BYTES)`. Every capacity this function produces is therefore a
/// power of two (or exactly `required` past the largest one), so each growth
/// at least doubles the buffer and a run of resizes costs O(log size)
/// allocations.
#[must_use]
pub fn grown_capacity(current: usize, required: usize) -> usize {
    if current > 0 && current >= required {
        return current;
    }
    let floor = required.max(MIN_BUFFER_BYTES);
    floor.checked_next_power_of_two().unwrap_or(floor)
}

/// The per-frame uniform block, written into [`SlotBuffer::Uniforms`] with
/// [`Self::to_bytes`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FrameUniforms {
    /// Monotonic frame number (the slot lease's [`SlotLease::frame`]).
    pub frame: u64,
    /// Drawable size in pixels.
    pub viewport: [u32; 2],
    pub grid: GridExtent,
    /// Premultiplied clear (default background) color, RGBA.
    pub clear: [f32; 4],
    /// The background pass's cell geometry, ring offset, cursor and tints
    /// (ft-yccm0.4.2.2).
    pub background: BackgroundUniforms,
    /// The text pass's decoration geometry (ft-yccm0.4.2.3).
    pub text: TextUniforms,
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_f32s(bytes: &mut [u8], at: usize, values: &[f32]) {
    for (index, value) in values.iter().enumerate() {
        let offset = at + index * 4;
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
}

impl FrameUniforms {
    /// The little-endian layout of the shaders' `FrameUniforms` struct
    /// (`src/shaders/background.metal`, Metal Shading Language alignment):
    ///
    /// | offset | field |
    /// |---|---|
    /// | 0 | `frame: ulong` |
    /// | 8 | `viewport: uint2` |
    /// | 16 | `grid: uint2` (rows, cols) |
    /// | 32 | `clear: float4` |
    /// | 48 | `cell_size: float2` |
    /// | 56 | `grid_origin: float2` |
    /// | 64 | `row_offset: uint` |
    /// | 68 | `cursor_shape: uint` |
    /// | 72 | `cursor_cell: uint2` (col, row) |
    /// | 80 | `cursor_color: float4` |
    /// | 96 | `cursor_thickness: float` |
    /// | 100 | `cursor_width: uint` (cells) |
    /// | 112 | `selection_tint: float4` |
    /// | 128 | `search_tint: float4` |
    /// | 144 | `current_tint: float4` |
    /// | 160 | `underline_position: float` |
    /// | 164 | `line_thickness: float` |
    /// | 168 | `strikethrough_position: float` |
    /// | 176 | `hsb: float4`, see [`Self::to_bytes_with_hsb`] |
    ///
    /// Everything else is zero, up to [`UNIFORMS_BYTES`]. The colors (clear,
    /// cursor and tints) are given premultiplied and sRGB-encoded and are
    /// written in premultiplied linear light
    /// ([`crate::color::linear_premultiplied`]): the shaders blend in linear
    /// light (ft-yccm0.4.7.3).
    #[must_use]
    pub fn to_bytes(&self) -> [u8; UNIFORMS_BYTES] {
        self.to_bytes_with_hsb(None)
    }

    /// [`Self::to_bytes`] for a pane drawn with inactive-pane dimming
    /// (ft-yccm0.4.6): `hsb` multiplies every color's hue, saturation and
    /// brightness, as the WebGpu renderer's `inactive_pane_hsb` does
    /// ([`crate::apply_hsb`] is the CPU reference). It is stored at offset
    /// 176 with `w = 1`; without it the field stays zero, which the shaders
    /// read as no transform.
    #[must_use]
    pub fn to_bytes_with_hsb(&self, hsb: Option<[f32; 3]>) -> [u8; UNIFORMS_BYTES] {
        let mut bytes = [0u8; UNIFORMS_BYTES];
        if let Some([hue, saturation, brightness]) = hsb {
            put_f32s(&mut bytes, 176, &[hue, saturation, brightness, 1.0]);
        }
        bytes[0..8].copy_from_slice(&self.frame.to_le_bytes());
        put_u32(&mut bytes, 8, self.viewport[0]);
        put_u32(&mut bytes, 12, self.viewport[1]);
        put_u32(&mut bytes, 16, self.grid.rows);
        put_u32(&mut bytes, 20, self.grid.cols);
        let linear = crate::color::linear_premultiplied;
        put_f32s(&mut bytes, 32, &linear(self.clear));
        let background = &self.background;
        put_f32s(&mut bytes, 48, &background.cell_size);
        put_f32s(&mut bytes, 56, &background.grid_origin);
        put_u32(&mut bytes, 64, background.row_offset);
        put_u32(&mut bytes, 68, background.cursor.shape.code());
        put_u32(&mut bytes, 72, background.cursor.col);
        put_u32(&mut bytes, 76, background.cursor.row);
        put_f32s(&mut bytes, 80, &linear(background.cursor.color));
        put_f32s(&mut bytes, 96, &[background.cursor.thickness]);
        put_u32(&mut bytes, 100, background.cursor.width_cells);
        put_f32s(&mut bytes, 112, &linear(background.selection_tint));
        put_f32s(&mut bytes, 128, &linear(background.search_tint));
        put_f32s(&mut bytes, 144, &linear(background.current_match_tint));
        let text = &self.text;
        put_f32s(
            &mut bytes,
            160,
            &[
                text.underline_position,
                text.line_thickness,
                text.strikethrough_position,
            ],
        );
        bytes
    }

    /// The frame number stored in a uniform block, for tests and readback.
    #[must_use]
    pub fn frame_of(bytes: &[u8]) -> Option<u64> {
        Some(u64::from_le_bytes(bytes.get(0..8)?.try_into().ok()?))
    }
}

/// Where a slot is in its life cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// Not in use; the next lease of this slot may write it.
    Free,
    /// Leased to the CPU, which is writing frame `frame` into it.
    Encoding { frame: u64 },
    /// Frame `frame` was committed; the GPU may still be reading the slot.
    InFlight { frame: u64 },
}

#[derive(Debug)]
struct Ring {
    next: usize,
    slots: [SlotState; FRAME_SLOTS],
    next_frame: u64,
    frames_completed: u64,
    peak_in_flight: usize,
}

impl Ring {
    fn in_flight(&self) -> usize {
        self.slots
            .iter()
            .filter(|state| matches!(state, SlotState::InFlight { .. }))
            .count()
    }
}

/// [`SlotRing::acquire`] waited out its timeout: the GPU had not finished the
/// frame that last used the next slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotTimeout {
    /// The slot the lease waited for.
    pub slot: usize,
    /// What that slot was doing when the wait gave up.
    pub state: SlotState,
    /// Frames in flight at that moment.
    pub in_flight: usize,
}

/// Rotation and pacing of the [`FRAME_SLOTS`] frame slots; see the module
/// docs. Shared (`Arc`) between the render thread and completion handlers.
#[derive(Debug)]
pub struct SlotRing {
    ring: Mutex<Ring>,
    freed: Condvar,
}

impl SlotRing {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            ring: Mutex::new(Ring {
                next: 0,
                slots: [SlotState::Free; FRAME_SLOTS],
                next_frame: 0,
                frames_completed: 0,
                peak_in_flight: 0,
            }),
            freed: Condvar::new(),
        })
    }

    // Every operation leaves the ring consistent before it can panic, so a
    // poisoned lock still guards valid state.
    fn lock(&self) -> MutexGuard<'_, Ring> {
        self.ring.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Leases the next slot in rotation, waiting up to `timeout` for the GPU
    /// to finish the frame that last used it.
    pub fn acquire(self: &Arc<Self>, timeout: Duration) -> Result<SlotLease, SlotTimeout> {
        let deadline = Instant::now().checked_add(timeout);
        let mut ring = self.lock();
        loop {
            let slot = ring.next;
            if ring.slots[slot] == SlotState::Free {
                let frame = ring.next_frame;
                ring.next_frame += 1;
                ring.slots[slot] = SlotState::Encoding { frame };
                ring.next = (slot + 1) % FRAME_SLOTS;
                return Ok(SlotLease {
                    ring: Arc::clone(self),
                    slot,
                    frame,
                    submitted: false,
                });
            }
            let now = Instant::now();
            let remaining = match deadline {
                Some(deadline) if deadline > now => deadline - now,
                Some(_) => {
                    return Err(SlotTimeout {
                        slot,
                        state: ring.slots[slot],
                        in_flight: ring.in_flight(),
                    });
                }
                // A timeout too large to represent waits indefinitely.
                None => Duration::from_secs(3600),
            };
            ring = self
                .freed
                .wait_timeout(ring, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// The current state of `slot`.
    #[must_use]
    pub fn state(&self, slot: usize) -> SlotState {
        self.lock().slots[slot]
    }

    /// Frames committed and not yet completed.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.lock().in_flight()
    }

    /// The most frames ever in flight at once.
    #[must_use]
    pub fn peak_in_flight(&self) -> usize {
        self.lock().peak_in_flight
    }

    /// Frames the GPU has completed.
    #[must_use]
    pub fn frames_completed(&self) -> u64 {
        self.lock().frames_completed
    }

    /// The number the next lease's frame gets. Glyphs inserted or touched
    /// for the frame being prepared are stamped with it (ft-yccm0.4.2.3).
    #[must_use]
    pub fn next_frame(&self) -> u64 {
        self.lock().next_frame
    }

    /// Every frame numbered below this has finished: it completed on the GPU
    /// or was dropped unsubmitted, so nothing it drew is still being read.
    /// The glyph atlases reuse a region only once its last frame is below it
    /// (ft-yccm0.4.3.2).
    #[must_use]
    pub fn retired_before(&self) -> u64 {
        let ring = self.lock();
        ring.slots
            .iter()
            .filter_map(|state| match state {
                SlotState::Free => None,
                SlotState::Encoding { frame } | SlotState::InFlight { frame } => Some(*frame),
            })
            .min()
            .unwrap_or(ring.next_frame)
    }

    /// Waits up to `timeout` until no slot is leased or in flight. Returns
    /// whether the ring went idle.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut ring = self.lock();
        loop {
            if ring.slots.iter().all(|state| *state == SlotState::Free) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            ring = self
                .freed
                .wait_timeout(ring, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn submit(&self, slot: usize, frame: u64) {
        let mut ring = self.lock();
        debug_assert_eq!(ring.slots[slot], SlotState::Encoding { frame });
        ring.slots[slot] = SlotState::InFlight { frame };
        ring.peak_in_flight = ring.peak_in_flight.max(ring.in_flight());
    }

    fn release(&self, slot: usize, expected: SlotState) {
        let mut ring = self.lock();
        if ring.slots[slot] == expected {
            ring.slots[slot] = SlotState::Free;
            if matches!(expected, SlotState::InFlight { .. }) {
                ring.frames_completed += 1;
            }
            drop(ring);
            self.freed.notify_all();
        }
    }
}

/// Exclusive write access to one frame slot that is not in flight. Submit it
/// once the frame is committed; dropping it unsubmitted (the frame was
/// abandoned before anything reached the GPU) frees the slot.
#[derive(Debug)]
pub struct SlotLease {
    ring: Arc<SlotRing>,
    slot: usize,
    frame: u64,
    submitted: bool,
}

impl SlotLease {
    /// The leased slot, `0..FRAME_SLOTS`.
    #[must_use]
    pub fn slot(&self) -> usize {
        self.slot
    }

    /// The frame number this lease writes; frame numbers increase by one per
    /// lease.
    #[must_use]
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Whether this lease came from `ring`.
    #[must_use]
    pub fn is_from(&self, ring: &Arc<SlotRing>) -> bool {
        Arc::ptr_eq(&self.ring, ring)
    }

    /// The frame was committed: the slot stays in flight until the returned
    /// token completes.
    #[must_use = "the slot stays in flight until the token completes or drops"]
    pub fn submit(mut self) -> CompletionToken {
        self.submitted = true;
        self.ring.submit(self.slot, self.frame);
        CompletionToken {
            ring: Arc::clone(&self.ring),
            slot: self.slot,
            frame: self.frame,
            done: AtomicBool::new(false),
        }
    }
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        if !self.submitted {
            self.ring
                .release(self.slot, SlotState::Encoding { frame: self.frame });
        }
    }
}

/// Completes a submitted frame. Move it into the GPU completion handler.
/// Completing is idempotent, and dropping an uncompleted token completes it,
/// so a handler that Metal releases without running (a command buffer that
/// was never committed) cannot strand its slot.
#[derive(Debug)]
pub struct CompletionToken {
    ring: Arc<SlotRing>,
    slot: usize,
    frame: u64,
    done: AtomicBool,
}

impl CompletionToken {
    /// The slot this token frees.
    #[must_use]
    pub fn slot(&self) -> usize {
        self.slot
    }

    /// The frame this token completes.
    #[must_use]
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// The GPU finished the frame: free its slot.
    pub fn complete(&self) {
        if !self.done.swap(true, Ordering::AcqRel) {
            self.ring
                .release(self.slot, SlotState::InFlight { frame: self.frame });
        }
    }
}

impl Drop for CompletionToken {
    fn drop(&mut self) {
        self.complete();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc;

    const LONG: Duration = Duration::from_secs(10);

    #[test]
    fn slot_buffers_are_sized_for_the_grid() {
        let sizes = SlotSizes::for_grid(GridExtent::new(80, 120)).unwrap();
        assert_eq!(sizes.bytes(SlotBuffer::Uniforms), UNIFORMS_BYTES);
        assert_eq!(sizes.bytes(SlotBuffer::CellBg), 80 * 120 * 4);
        assert_eq!(sizes.bytes(SlotBuffer::CellText), 80 * 120 * 24);
        assert_eq!(sizes.bytes(SlotBuffer::RowTable), 80 * 16);
        let empty = SlotSizes::for_grid(GridExtent::default()).unwrap();
        assert_eq!(empty.bytes(SlotBuffer::CellBg), 0);
        assert_eq!(empty.bytes(SlotBuffer::Uniforms), UNIFORMS_BYTES);
        // The operator's 6K window.
        let large = SlotSizes::for_grid(GridExtent::new(117, 512)).unwrap();
        assert_eq!(large.bytes(SlotBuffer::CellText), 117 * 512 * 24);
    }

    /// ft-yccm0.4.2.3: a frame with more glyph instances than cells (combining
    /// marks, ligature pieces) sizes CellText for all of them; fewer keep the
    /// per-cell reservation.
    #[test]
    fn frames_with_extra_glyph_instances_grow_only_the_cell_text_buffer() {
        let grid = GridExtent::new(80, 120);
        let reserved = SlotSizes::for_grid(grid).unwrap();
        assert_eq!(SlotSizes::for_frame(grid, 10).unwrap(), reserved);
        let crowded = SlotSizes::for_frame(grid, 2 * 80 * 120).unwrap();
        assert_eq!(
            crowded.bytes(SlotBuffer::CellText),
            2 * reserved.bytes(SlotBuffer::CellText)
        );
        for kind in [
            SlotBuffer::Uniforms,
            SlotBuffer::CellBg,
            SlotBuffer::RowTable,
        ] {
            assert_eq!(crowded.bytes(kind), reserved.bytes(kind));
        }
        assert!(SlotSizes::for_frame(grid, usize::MAX).is_none());
    }

    /// ft-yccm0.4.3.2: the fence the glyph atlases reuse regions behind.
    #[test]
    fn retired_before_is_the_oldest_unfinished_frame() {
        let ring = SlotRing::new();
        assert_eq!(ring.retired_before(), 0, "nothing leased yet");
        assert_eq!(ring.next_frame(), 0);
        let first = ring.acquire(LONG).unwrap();
        assert_eq!(ring.next_frame(), 1, "the lease took frame 0");
        assert_eq!(ring.retired_before(), 0, "frame 0 is being encoded");
        let token0 = first.submit();
        let token1 = ring.acquire(LONG).unwrap().submit();
        let encoding = ring.acquire(LONG).unwrap();
        assert_eq!(ring.retired_before(), 0);
        // Frame 1 finishing first does not retire frame 0.
        token1.complete();
        assert_eq!(ring.retired_before(), 0);
        token0.complete();
        assert_eq!(ring.retired_before(), 2, "frame 2 is still being encoded");
        drop(encoding);
        assert_eq!(
            ring.retired_before(),
            3,
            "a dropped lease counts as finished"
        );
    }

    #[test]
    fn grid_extents_saturate_instead_of_wrapping() {
        let grid = GridExtent::new(usize::MAX, 3);
        assert_eq!(grid.rows, u32::MAX);
        assert_eq!(grid.cells(), u64::from(u32::MAX) * 3);
        let huge = GridExtent {
            rows: u32::MAX,
            cols: u32::MAX,
        };
        if usize::BITS == 64 {
            // u32::MAX^2 * 32 bytes overflows a 64-bit usize.
            assert!(SlotSizes::for_grid(huge).is_none());
        }
    }

    #[test]
    fn capacity_grows_geometrically_and_never_per_frame() {
        assert_eq!(grown_capacity(0, 0), MIN_BUFFER_BYTES);
        assert_eq!(grown_capacity(0, 1), MIN_BUFFER_BYTES);
        assert_eq!(grown_capacity(0, 38_400), 65_536);
        // A steady or shrinking grid keeps its buffer.
        assert_eq!(grown_capacity(65_536, 38_400), 65_536);
        assert_eq!(grown_capacity(65_536, 65_536), 65_536);
        assert_eq!(grown_capacity(65_536, 1), 65_536);
        // Growth at least doubles.
        assert_eq!(grown_capacity(65_536, 65_537), 131_072);
        let mut capacity = 0;
        let mut allocations = 0;
        for required in (1..=1_000_000).step_by(997) {
            let next = grown_capacity(capacity, required);
            if next != capacity {
                assert!(next >= required);
                assert!(
                    capacity == 0 || next >= 2 * capacity,
                    "{capacity} -> {next}"
                );
                allocations += 1;
                capacity = next;
            }
        }
        assert!(
            allocations <= 9,
            "{allocations} reallocations for 1 MB of growth"
        );
        // Past the largest power of two, grow to exactly what is needed.
        assert_eq!(grown_capacity(0, usize::MAX), usize::MAX);
    }

    #[test]
    fn uniforms_use_the_documented_layout() {
        let uniforms = FrameUniforms {
            frame: 0x0102_0304_0506_0708,
            viewport: [1920, 1080],
            grid: GridExtent::new(80, 120),
            clear: [0.5, 0.25, 0.0, 1.0],
            ..FrameUniforms::default()
        };
        let bytes = uniforms.to_bytes();
        assert_eq!(&bytes[0..8], &0x0102_0304_0506_0708_u64.to_le_bytes());
        assert_eq!(&bytes[8..16], &[0x80, 0x07, 0, 0, 0x38, 0x04, 0, 0]);
        assert_eq!(&bytes[16..24], &[80, 0, 0, 0, 120, 0, 0, 0]);
        assert_eq!(&bytes[24..32], &[0; 8]);
        // The clear color is written in linear light (ft-yccm0.4.7.3).
        let red = crate::color::srgb_to_linear(0.5);
        assert!(red < 0.25, "sRGB 0.5 is about a fifth of the light: {red}");
        assert_eq!(&bytes[32..36], &red.to_le_bytes());
        assert_eq!(&bytes[44..48], &1.0_f32.to_le_bytes());
        assert!(bytes[48..].iter().all(|&byte| byte == 0));
        assert_eq!(FrameUniforms::frame_of(&bytes), Some(uniforms.frame));
        assert_eq!(FrameUniforms::frame_of(&bytes[..7]), None);
    }

    #[test]
    fn background_uniforms_follow_the_shader_struct_layout() {
        use crate::cell_bg::{BACKGROUND_SHADER, CursorShape, CursorUniform};
        let uniforms = FrameUniforms {
            background: BackgroundUniforms {
                cell_size: [8.0, 16.0],
                grid_origin: [4.0, 2.0],
                row_offset: 5,
                cursor: CursorUniform {
                    shape: CursorShape::Bar,
                    col: 3,
                    row: 1,
                    width_cells: 2,
                    thickness: 2.0,
                    color: [0.75, 0.5, 0.25, 0.75],
                },
                selection_tint: [0.1; 4],
                search_tint: [0.2; 4],
                current_match_tint: [0.3; 4],
            },
            ..FrameUniforms::default()
        };
        let bytes = uniforms.to_bytes();
        let f32_at = |at: usize| f32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        assert_eq!(
            [f32_at(48), f32_at(52), f32_at(56), f32_at(60)],
            [8.0, 16.0, 4.0, 2.0]
        );
        assert_eq!(
            [u32_at(64), u32_at(68), u32_at(72), u32_at(76)],
            [5, 4, 3, 1]
        );
        // Colors are written in premultiplied linear light (ft-yccm0.4.7.3);
        // the tints are premultiplied white, which converts exactly.
        assert_eq!(
            [f32_at(80), f32_at(84), f32_at(88), f32_at(92)],
            crate::color::linear_premultiplied([0.75, 0.5, 0.25, 0.75])
        );
        assert!(
            (f32_at(80) - 0.75).abs() < f32::EPSILON,
            "full red stays full at 0.75 coverage"
        );
        assert_eq!((f32_at(96), u32_at(100)), (2.0, 2));
        assert_eq!((f32_at(112), f32_at(128), f32_at(144)), (0.1, 0.2, 0.3));
        assert!(bytes[104..112].iter().all(|&byte| byte == 0));
        assert!(bytes[160..].iter().all(|&byte| byte == 0), "no text fields");
        // The shader's struct comments carry the same offsets.
        for (field, offset) in [
            ("frame", 0),
            ("viewport", 8),
            ("grid", 16),
            ("clear", 32),
            ("cell_size", 48),
            ("grid_origin", 56),
            ("row_offset", 64),
            ("cursor_shape", 68),
            ("cursor_cell", 72),
            ("cursor_color", 80),
            ("cursor_thickness", 96),
            ("cursor_width", 100),
            ("selection_tint", 112),
            ("search_tint", 128),
            ("current_tint", 144),
            ("hsb", 176),
        ] {
            let declaration = format!(" {field};");
            let line = BACKGROUND_SHADER
                .lines()
                .find(|line| line.contains(&declaration) && line.contains("//"))
                .unwrap_or_else(|| panic!("the shader declares {field}"));
            assert!(
                line.contains(&format!("// {offset}")),
                "{field} is at {offset}: {line}"
            );
        }
    }

    /// ft-yccm0.4.2.3: the text pass's fields follow the background's, and the
    /// text shader declares the whole block at the same offsets.
    #[test]
    fn text_uniforms_follow_the_shader_struct_layout() {
        use crate::cell_text::TEXT_SHADER;
        let uniforms = FrameUniforms {
            text: TextUniforms {
                underline_position: 13.0,
                line_thickness: 1.5,
                strikethrough_position: 7.0,
            },
            ..FrameUniforms::default()
        };
        let bytes = uniforms.to_bytes();
        let f32_at = |at: usize| f32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        assert_eq!((f32_at(160), f32_at(164), f32_at(168)), (13.0, 1.5, 7.0));
        assert!(bytes[..160].iter().all(|&byte| byte == 0));
        assert!(bytes[172..].iter().all(|&byte| byte == 0));
        for (field, offset) in [
            ("frame", 0),
            ("viewport", 8),
            ("grid", 16),
            ("clear", 32),
            ("cell_size", 48),
            ("grid_origin", 56),
            ("row_offset", 64),
            ("cursor_shape", 68),
            ("cursor_cell", 72),
            ("cursor_color", 80),
            ("cursor_thickness", 96),
            ("cursor_width", 100),
            ("selection_tint", 112),
            ("search_tint", 128),
            ("current_tint", 144),
            ("underline_position", 160),
            ("line_thickness", 164),
            ("strikethrough_position", 168),
            ("hsb", 176),
        ] {
            let declaration = format!(" {field};");
            let line = TEXT_SHADER
                .lines()
                .find(|line| line.contains(&declaration) && line.contains("//"))
                .unwrap_or_else(|| panic!("the text shader declares {field}"));
            assert!(
                line.contains(&format!("// {offset}")),
                "{field} is at {offset}: {line}"
            );
        }
    }

    /// ft-yccm0.4.6: inactive-pane dimming is stored at 176 with `w = 1`;
    /// without it the block is byte-for-byte what `to_bytes` writes.
    #[test]
    fn hsb_dimming_is_flagged_at_its_offset_and_absent_by_default() {
        let uniforms = FrameUniforms {
            viewport: [640, 480],
            ..FrameUniforms::default()
        };
        assert_eq!(uniforms.to_bytes_with_hsb(None), uniforms.to_bytes());
        let bytes = uniforms.to_bytes_with_hsb(Some([1.0, 0.8, 0.7]));
        let f32_at = |at: usize| f32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        assert_eq!(
            [f32_at(176), f32_at(180), f32_at(184), f32_at(188)],
            [1.0, 0.8, 0.7, 1.0]
        );
        assert_eq!(bytes[..176], uniforms.to_bytes()[..176]);
        assert!(bytes[192..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn slots_rotate_in_strict_order_with_increasing_frame_numbers() {
        let ring = SlotRing::new();
        for expected_frame in 0..10_u64 {
            let lease = ring.acquire(LONG).unwrap();
            assert_eq!(lease.frame(), expected_frame);
            assert_eq!(lease.slot(), usize::try_from(expected_frame % 3).unwrap());
            assert_eq!(
                ring.state(lease.slot()),
                SlotState::Encoding {
                    frame: expected_frame
                }
            );
            let token = lease.submit();
            assert_eq!(
                ring.state(token.slot()),
                SlotState::InFlight {
                    frame: expected_frame
                }
            );
            token.complete();
            assert_eq!(ring.state(token.slot()), SlotState::Free);
        }
        assert_eq!(ring.frames_completed(), 10);
        assert_eq!(ring.peak_in_flight(), 1);
    }

    #[test]
    fn a_fourth_frame_waits_for_the_oldest_and_times_out_while_it_is_in_flight() {
        let ring = SlotRing::new();
        let tokens: Vec<CompletionToken> = (0..3)
            .map(|_| ring.acquire(LONG).unwrap().submit())
            .collect();
        assert_eq!(ring.in_flight(), 3);
        let err = ring.acquire(Duration::from_millis(20)).unwrap_err();
        assert_eq!(
            err,
            SlotTimeout {
                slot: 0,
                state: SlotState::InFlight { frame: 0 },
                in_flight: 3,
            }
        );
        // Completing a younger frame does not free the next slot in rotation.
        tokens[2].complete();
        assert!(ring.acquire(Duration::from_millis(20)).is_err());
        tokens[0].complete();
        let lease = ring.acquire(Duration::from_millis(20)).unwrap();
        assert_eq!((lease.slot(), lease.frame()), (0, 3));
        assert_eq!(ring.peak_in_flight(), 3);
    }

    #[test]
    fn an_abandoned_lease_frees_its_slot_without_counting_a_completion() {
        let ring = SlotRing::new();
        let lease = ring.acquire(LONG).unwrap();
        drop(lease);
        assert_eq!(ring.state(0), SlotState::Free);
        assert_eq!(ring.frames_completed(), 0);
        assert!(ring.wait_idle(Duration::ZERO));
        // Rotation still moves on: the next lease is slot 1, frame 1.
        let next = ring.acquire(LONG).unwrap();
        assert_eq!((next.slot(), next.frame()), (1, 1));
    }

    #[test]
    fn completion_is_idempotent_and_a_dropped_token_completes() {
        let ring = SlotRing::new();
        let token = ring.acquire(LONG).unwrap().submit();
        token.complete();
        token.complete();
        assert_eq!(ring.frames_completed(), 1);
        let dropped = ring.acquire(LONG).unwrap().submit();
        assert_eq!(ring.in_flight(), 1);
        drop(dropped);
        assert_eq!(ring.in_flight(), 0);
        assert_eq!(ring.frames_completed(), 2);
        // A stale completion never frees a slot that moved on to a newer frame.
        let lease = ring.acquire(LONG).unwrap();
        let ghost = CompletionToken {
            ring: Arc::clone(&ring),
            slot: lease.slot(),
            frame: lease.frame() + 100,
            done: AtomicBool::new(false),
        };
        ghost.complete();
        assert_eq!(
            ring.state(lease.slot()),
            SlotState::Encoding {
                frame: lease.frame()
            }
        );
    }

    #[test]
    fn wait_idle_reports_outstanding_work() {
        let ring = SlotRing::new();
        let token = ring.acquire(LONG).unwrap().submit();
        assert!(!ring.wait_idle(Duration::from_millis(10)));
        let completer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            token.complete();
        });
        assert!(ring.wait_idle(LONG));
        completer.join().unwrap();
    }

    /// The bead's native test, modeled: a slow "GPU" completes frames late
    /// and, at completion, checks that the slot still holds the data the CPU
    /// wrote for that frame. A CPU that overwrote an in-flight slot would
    /// make the GPU see a later frame's number.
    #[test]
    fn frames_never_overwrite_in_flight_data_under_slow_gpu_completion() {
        const FRAMES: u64 = 60;
        let ring = SlotRing::new();
        let slot_data: Arc<[AtomicU64; FRAME_SLOTS]> =
            Arc::new(std::array::from_fn(|_| AtomicU64::new(u64::MAX)));
        let (submit_tx, submit_rx) = mpsc::channel::<CompletionToken>();
        let gpu = {
            let slot_data = Arc::clone(&slot_data);
            let ring = Arc::clone(&ring);
            std::thread::spawn(move || {
                let mut completed = 0;
                for token in submit_rx {
                    // Varying, sometimes long, GPU latency.
                    std::thread::sleep(Duration::from_millis([3, 0, 9, 1, 6][completed % 5]));
                    let seen = slot_data[token.slot()].load(Ordering::Acquire);
                    assert_eq!(
                        seen,
                        token.frame(),
                        "slot {} was overwritten while frame {} was in flight",
                        token.slot(),
                        token.frame()
                    );
                    assert!(ring.in_flight() <= FRAME_SLOTS);
                    token.complete();
                    completed += 1;
                }
                completed
            })
        };
        for frame in 0..FRAMES {
            let lease = ring.acquire(LONG).unwrap();
            assert_eq!(lease.frame(), frame);
            // The CPU writes this frame's data into its slot...
            slot_data[lease.slot()].store(frame, Ordering::Release);
            // ...and commits it.
            submit_tx.send(lease.submit()).unwrap();
        }
        drop(submit_tx);
        assert_eq!(gpu.join().unwrap(), usize::try_from(FRAMES).unwrap());
        assert!(ring.wait_idle(LONG));
        assert_eq!(ring.frames_completed(), FRAMES);
        assert_eq!(
            ring.peak_in_flight(),
            FRAME_SLOTS,
            "the slow GPU kept every slot busy"
        );
    }

    #[test]
    fn the_ring_and_its_tokens_cross_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SlotRing>();
        assert_send_sync::<CompletionToken>();
        assert_send_sync::<SlotLease>();
    }
}
