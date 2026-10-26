//! Dirty-row uploads (ft-yccm0.4.2.4), the platform-independent half.
//!
//! Each frame slot's buffers keep what the slot last uploaded. A frame
//! uploads only the ring rows that changed since the slot it lands in last
//! wrote them, so all three slots converge on the current grid, and an
//! unchanged row costs no CPU and no upload.
//!
//! - Every grid ([`crate::CellBgGrid`], [`crate::CellTextGrid`]) carries
//!   [`RowChanges`]: an epoch naming that grid instance, and a change
//!   generation per ring row that moves when the row's content does.
//! - [`SlotUploads`] remembers, per slot, the epochs, extent and generations
//!   it wrote, and plans the next frame's rows ([`UploadPlan`]).
//! - Scrolling rotates a grid's ring (`row_offset`, a uniform), so it
//!   changes only the rows it exposes: a full-screen scroll of N lines
//!   uploads N rows.
//!
//! Glyph instances live in fixed regions, `capacity` instances per ring row
//! ([`text_region`]), so one row's upload never moves another row. Unused
//! instances in a region are zero: no glyph and no decoration, so the vertex
//! shader collapses them to an empty quad. Buffers reserve `rows * capacity`
//! instances, but the text draw ends at the last populated ring row's last
//! instance; entirely empty grids submit no text draw. The capacity starts
//! at one instance per cell (or the fullest row) and grows by half when a
//! row outgrows it, which re-lays the slot out once.
//!
//! A new layout initializes every reserved instance. Later dirty rows
//! write only through the larger of their old and new lengths in that
//! slot, clearing removed glyphs without rewriting already-zero padding.

use crate::cell_bg::CellBgGrid;
use crate::cell_text::CellTextGrid;
use crate::frame::{
    CELL_TEXT_INSTANCE_BYTES, CELL_TEXT_INSTANCES_PER_CELL, GridExtent, ROW_TABLE_BYTES_PER_ROW,
};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

/// Dirty-row summaries cover one machine word of rows. A sparse frame
/// compares one generation per block and only reads individual generations
/// in blocks that changed since this particular frame slot last caught up.
const ROW_CHANGE_BLOCK: usize = 64;

fn next_epoch() -> u64 {
    NEXT_EPOCH
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |epoch| {
            epoch.checked_add(1)
        })
        .expect("row change epoch exhausted")
}

/// Per-ring-row change generations of one grid instance.
#[derive(Debug)]
pub struct RowChanges {
    epoch: u64,
    latest: u64,
    rows: Vec<u64>,
    blocks: Vec<u64>,
}

impl RowChanges {
    /// A new grid instance of `rows` ring rows, every row at generation 1.
    #[must_use]
    pub fn new(rows: usize) -> Self {
        Self {
            epoch: next_epoch(),
            latest: 1,
            rows: vec![1; rows],
            blocks: vec![1; rows.div_ceil(ROW_CHANGE_BLOCK)],
        }
    }

    /// Ring row `ring_row` changed.
    pub fn touch(&mut self, ring_row: usize) {
        if ring_row >= self.rows.len() {
            return;
        }
        let Some(latest) = self.latest.checked_add(1) else {
            // Generations may be compared with `>` only within an epoch.
            // A new identity forces every lagging slot to initialize in
            // full, so wrapping can never hide a dirty row.
            self.epoch = next_epoch();
            self.latest = 1;
            self.rows.fill(1);
            self.blocks.fill(1);
            return;
        };
        self.latest = latest;
        self.rows[ring_row] = latest;
        self.blocks[ring_row / ROW_CHANGE_BLOCK] = latest;
    }

    /// Names this grid instance; a clone is another instance.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The generation of ring row `ring_row`'s current content.
    #[must_use]
    pub fn row(&self, ring_row: usize) -> u64 {
        self.rows.get(ring_row).copied().unwrap_or(0)
    }

    /// Changed rows since `written`, which must belong to this epoch.
    /// Zero means a fresh layout and includes every stored row. Each slot
    /// keeps its own `written`, so skipped frames require no damage log and
    /// never discard changes that another slot has not consumed yet.
    fn since(&self, written: u64) -> impl Iterator<Item = usize> + '_ {
        let blocks = if self.latest > written {
            self.blocks.as_slice()
        } else {
            &[]
        };
        blocks
            .iter()
            .enumerate()
            .filter(move |(_, generation)| **generation > written)
            .flat_map(move |(block, _)| {
                let first = block * ROW_CHANGE_BLOCK;
                let end = first.saturating_add(ROW_CHANGE_BLOCK).min(self.rows.len());
                self.rows[first..end]
                    .iter()
                    .enumerate()
                    .filter_map(move |(row, generation)| {
                        (*generation > written).then_some(first + row)
                    })
            })
    }
}

impl Clone for RowChanges {
    /// A clone diverges from its source, so it is a new instance: a slot
    /// that held the source re-uploads it in full.
    fn clone(&self) -> Self {
        Self {
            epoch: next_epoch(),
            latest: self.latest,
            rows: self.rows.clone(),
            blocks: self.blocks.clone(),
        }
    }
}

/// One dirty text row's upload into its fixed region in a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRowUpload {
    /// The physical ring row, independent of the logical scroll offset.
    pub row: u32,
    /// Instances to write, including any removed instances to zero. This
    /// is the full row capacity when initializing a layout, and the larger
    /// of this slot's old row length and the new length otherwise.
    pub instances: u32,
}

/// The ring rows one frame uploads into its slot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UploadPlan {
    /// The slot is laid out afresh: every row is listed.
    pub full: bool,
    /// Glyph instances reserved per ring row ([`text_region`]).
    pub capacity: u32,
    /// Ring rows of cell backgrounds to upload.
    pub cell_rows: Vec<u32>,
    /// Glyph row regions (and their row-table entries) to upload.
    pub text_rows: Vec<TextRowUpload>,
}

impl UploadPlan {
    /// Bytes the plan copies into the slot for a grid of `extent`.
    #[must_use]
    pub fn bytes(&self, extent: GridExtent) -> u64 {
        let cell_row = u64::from(extent.cols) * 4;
        let text_bytes: u64 = self
            .text_rows
            .iter()
            .map(|row| {
                u64::from(row.instances) * CELL_TEXT_INSTANCE_BYTES as u64
                    + ROW_TABLE_BYTES_PER_ROW as u64
            })
            .sum();
        self.cell_rows.len() as u64 * cell_row + text_bytes
    }

    /// Instances reserved in the slot for a grid of `extent`. Buffer sizing
    /// must use this full layout even when [`Self::draw_instances`] is smaller.
    #[must_use]
    pub fn instances(&self, extent: GridExtent) -> usize {
        usize::try_from(u64::from(extent.rows) * u64::from(self.capacity)).unwrap_or(usize::MAX)
    }

    /// The fixed-layout prefix that contains every text instance. Empty
    /// regions before the last glyph retain their positions, while trailing
    /// padding and empty rows need no vertex shader invocations.
    #[must_use]
    pub fn draw_instances(&self, text: &CellTextGrid) -> usize {
        text.last_nonempty_ring_row().map_or(0, |row| {
            let count = text.ring_row_len(row).min(self.capacity as usize);
            row.saturating_mul(self.capacity as usize)
                .saturating_add(count)
        })
    }
}

/// What one frame slot last uploaded.
#[derive(Debug, Clone, Default)]
pub struct SlotUploads {
    cells: Option<u64>,
    text: Option<u64>,
    cells_latest: u64,
    text_latest: u64,
    extent: Option<GridExtent>,
    capacity: u32,
    buffers: Option<u64>,
    /// Actual row lengths last uploaded to this slot; padding stays zero
    /// until a later upload puts an instance there.
    text_lengths: Vec<u32>,
}

/// The instances each ring row of `text` needs at least: one per cell, or
/// the fullest row.
#[must_use]
pub fn needed_capacity(text: &CellTextGrid) -> u32 {
    let per_cell = u64::from(text.extent().cols) * CELL_TEXT_INSTANCES_PER_CELL as u64;
    let fullest = (0..text.ring_rows())
        .map(|ring| text.ring_row_len(ring))
        .max()
        .unwrap_or(0) as u64;
    u32::try_from(per_cell.max(fullest).max(1)).unwrap_or(u32::MAX)
}

impl SlotUploads {
    /// Forgets what the slot holds, so its next frame uploads everything (a
    /// write into it failed).
    pub fn invalidate(&mut self) {
        *self = Self::default();
    }

    /// Plans the frame about to be written into this slot, whose buffers are
    /// at `buffers_generation` (they change when the slot's buffers are
    /// replaced), and records the rows as written.
    pub fn plan(
        &mut self,
        cells: Option<&CellBgGrid>,
        text: Option<&CellTextGrid>,
        buffers_generation: u64,
    ) -> UploadPlan {
        let mut plan = UploadPlan::default();
        self.plan_into(cells, text, buffers_generation, &mut plan);
        plan
    }

    /// Plans into reusable storage. The native renderer shares one scratch
    /// plan across the panes it uploads sequentially, so recurring dirty
    /// frames do not allocate row lists after reaching their high-water mark.
    pub fn plan_into(
        &mut self,
        cells: Option<&CellBgGrid>,
        text: Option<&CellTextGrid>,
        buffers_generation: u64,
        plan: &mut UploadPlan,
    ) {
        plan.full = false;
        plan.capacity = self.capacity;
        plan.cell_rows.clear();
        plan.text_rows.clear();
        let extent = cells
            .map(CellBgGrid::extent)
            .or_else(|| text.map(CellTextGrid::extent));
        let cells_latest = cells.map_or(0, |grid| grid.changes().latest);
        let text_latest = text.map_or(0, |grid| grid.changes().latest);
        let same_grids = self.cells == cells.map(|grid| grid.changes().epoch())
            && self.text == text.map(|grid| grid.changes().epoch())
            && self.extent == extent;
        // An idle pane must not scan all its rows just to discover that
        // nothing changed. The latest generation changes on every row
        // mutation; epochs also distinguish replacement grids with the
        // same generation. Scroll offsets are uniforms, so rotating only
        // empty rows needs no upload even though the offset changed.
        if same_grids
            && self.buffers == Some(buffers_generation)
            && self.cells_latest == cells_latest
            && self.text_latest == text_latest
        {
            return;
        }
        let needed = text.map_or(1, |grid| {
            if same_grids {
                // The slot's current layout already fits unchanged rows.
                // Only changed rows can outgrow it; scanning every row for
                // the fullest one would undo sparse dirty-row planning.
                grid.changes()
                    .since(self.text_latest)
                    .map(|ring| u32::try_from(grid.ring_row_len(ring)).unwrap_or(u32::MAX))
                    .fold(self.capacity, u32::max)
            } else {
                needed_capacity(grid)
            }
        });
        let full =
            !same_grids || self.buffers != Some(buffers_generation) || needed > self.capacity;
        if full {
            self.capacity = if same_grids && needed > self.capacity {
                needed.max(self.capacity.saturating_add(self.capacity / 2))
            } else if same_grids {
                self.capacity.max(needed)
            } else {
                needed
            };
            self.cells = cells.map(|grid| grid.changes().epoch());
            self.text = text.map(|grid| grid.changes().epoch());
            self.extent = extent;
            self.buffers = Some(buffers_generation);
            self.text_lengths.clear();
        }
        plan.full = full;
        plan.capacity = self.capacity;
        if let Some(grid) = cells {
            for ring in grid
                .changes()
                .since(if full { 0 } else { self.cells_latest })
            {
                plan.cell_rows.push(u32::try_from(ring).unwrap_or(u32::MAX));
            }
        }
        if let Some(grid) = text {
            self.text_lengths.resize(grid.ring_rows(), 0);
            for ring in grid
                .changes()
                .since(if full { 0 } else { self.text_latest })
            {
                let length = u32::try_from(grid.ring_row_len(ring))
                    .unwrap_or(u32::MAX)
                    .min(self.capacity);
                let instances = if full {
                    self.capacity
                } else {
                    self.text_lengths[ring].max(length)
                };
                self.text_lengths[ring] = length;
                plan.text_rows.push(TextRowUpload {
                    row: u32::try_from(ring).unwrap_or(u32::MAX),
                    instances,
                });
            }
        }
        self.cells_latest = cells_latest;
        self.text_latest = text_latest;
    }
}

/// Ring row `ring_row` of `text` as its CellText region: the row's instance
/// bytes, then zero instances up to `capacity` (empty quads). Appends to
/// `out`; a row longer than `capacity` is cut (the plan's capacity is never
/// below [`needed_capacity`]).
pub fn text_region(text: &CellTextGrid, ring_row: usize, capacity: u32, out: &mut Vec<u8>) {
    let region = capacity as usize * CELL_TEXT_INSTANCE_BYTES;
    let bytes = text.ring_row_instances(ring_row);
    let kept = bytes.len().min(region);
    out.extend_from_slice(&bytes[..kept]);
    out.resize(out.len() + (region - kept), 0);
}

/// The row-table entry of ring row `ring_row` in the region layout: its
/// first instance (`ring_row * capacity`) and its instance count (`u32`
/// each), then 8 reserved bytes.
#[must_use]
pub fn row_table_entry(
    text: &CellTextGrid,
    ring_row: usize,
    capacity: u32,
) -> [u8; ROW_TABLE_BYTES_PER_ROW] {
    let first = u32::try_from(ring_row as u64 * u64::from(capacity)).unwrap_or(u32::MAX);
    let count =
        u32::try_from(text.ring_row_len(ring_row).min(capacity as usize)).unwrap_or(u32::MAX);
    let mut entry = [0; ROW_TABLE_BYTES_PER_ROW];
    entry[0..4].copy_from_slice(&first.to_le_bytes());
    entry[4..8].copy_from_slice(&count.to_le_bytes());
    entry
}

/// The full-rebuild reference: the CellBg, CellText and RowTable buffer
/// contents of `cells` and `text` laid out at `capacity`, every row.
#[must_use]
pub fn full_layout(
    cells: &CellBgGrid,
    text: &CellTextGrid,
    capacity: u32,
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut instances = Vec::new();
    let mut table = Vec::new();
    for ring in 0..text.ring_rows() {
        text_region(text, ring, capacity, &mut instances);
        table.extend_from_slice(&row_table_entry(text, ring, capacity));
    }
    (cells.as_bytes().to_vec(), instances, table)
}

#[cfg(test)]
// Fixtures are a few rows and columns: their casts cannot truncate.
#[allow(clippy::cast_possible_truncation, clippy::needless_range_loop)]
mod tests {
    use super::*;
    use crate::{CellBg, CellText};

    /// A slot's three buffers as plain memory, written the way the Metal
    /// half writes them.
    #[derive(Default)]
    struct SlotModel {
        uploads: SlotUploads,
        generation: u64,
        cells: Vec<u8>,
        text: Vec<u8>,
        table: Vec<u8>,
        bytes: u64,
    }

    impl SlotModel {
        fn upload(&mut self, cells: &CellBgGrid, text: &CellTextGrid) -> UploadPlan {
            let plan = self.uploads.plan(Some(cells), Some(text), self.generation);
            let extent = cells.extent();
            let cell_row = extent.cols as usize * 4;
            let region = plan.capacity as usize * CELL_TEXT_INSTANCE_BYTES;
            self.cells.resize(cells.as_bytes().len(), 0);
            // Fresh memory is deliberately nonzero: only a full upload can
            // establish the zero-padding invariant for a new layout.
            self.text.resize(text.ring_rows() * region, 0xa5);
            self.table
                .resize(text.ring_rows() * ROW_TABLE_BYTES_PER_ROW, 0);
            for &ring in &plan.cell_rows {
                let at = ring as usize * cell_row;
                self.cells[at..at + cell_row].copy_from_slice(&cells.as_bytes()[at..at + cell_row]);
            }
            for upload in &plan.text_rows {
                let ring = upload.row as usize;
                let mut bytes = Vec::new();
                text_region(text, ring, upload.instances, &mut bytes);
                let at = ring * region;
                self.text[at..at + bytes.len()].copy_from_slice(&bytes);
                let at = ring * ROW_TABLE_BYTES_PER_ROW;
                self.table[at..at + ROW_TABLE_BYTES_PER_ROW].copy_from_slice(&row_table_entry(
                    text,
                    ring,
                    plan.capacity,
                ));
            }
            self.bytes += plan.bytes(extent);
            plan
        }
    }

    fn grids(rows: usize, cols: usize) -> (CellBgGrid, CellTextGrid) {
        let extent = GridExtent::new(rows, cols);
        (CellBgGrid::new(extent), CellTextGrid::new(extent))
    }

    fn text_rows(plan: &UploadPlan) -> Vec<u32> {
        plan.text_rows.iter().map(|upload| upload.row).collect()
    }

    fn write_row(cells: &mut CellBgGrid, text: &mut CellTextGrid, row: u32, seed: u8) {
        let cols = cells.extent().cols;
        text.clear_row(row);
        for col in 0..cols {
            cells.set(row, col, CellBg::rgb(seed, col as u8, row as u8));
            text.push(row, CellText::new(col as u16, [seed, 1, 2, 255]));
        }
    }

    #[test]
    fn row_changes_track_rows_and_name_grid_instances() {
        let (mut cells, mut text) = grids(4, 3);
        let before: Vec<u64> = (0..4).map(|ring| cells.changes().row(ring)).collect();
        cells.set(2, 1, CellBg::rgb(9, 9, 9));
        let ring = cells.ring_row(2) as usize;
        for row in 0..4 {
            assert_eq!(
                cells.changes().row(row) != before[row],
                row == ring,
                "only ring row {ring} changed"
            );
        }
        // Rewriting a value it already holds changes nothing.
        let after = cells.changes().row(ring);
        cells.set(2, 1, CellBg::rgb(9, 9, 9));
        assert_eq!(cells.changes().row(ring), after);

        text.push(1, CellText::new(0, [1, 2, 3, 255]));
        let text_ring = text.ring_row(1) as usize;
        assert!(text.changes().row(text_ring) > 1);
        // Clearing an empty row changes nothing.
        let empty = text.ring_row(3) as usize;
        let generation = text.changes().row(empty);
        text.clear_row(3);
        assert_eq!(text.changes().row(empty), generation);

        let clone = cells.clone();
        assert_ne!(clone.changes().epoch(), cells.changes().epoch());
        assert_eq!(clone, cells, "equality compares content, not instances");
    }

    #[test]
    fn row_change_blocks_keep_sparse_catch_up_across_word_boundaries() {
        let mut changes = RowChanges::new(130);
        assert_eq!(
            changes.since(0).collect::<Vec<_>>(),
            (0..130).collect::<Vec<_>>()
        );
        let initial = changes.latest;
        for row in [0, 63, 64, 127, 129] {
            changes.touch(row);
        }
        let intermediate = changes.latest;
        assert_eq!(
            changes.since(initial).collect::<Vec<_>>(),
            [0, 63, 64, 127, 129]
        );
        changes.touch(63);
        changes.touch(65);
        assert_eq!(changes.since(intermediate).collect::<Vec<_>>(), [63, 65]);
        assert_eq!(
            changes.since(initial).collect::<Vec<_>>(),
            [0, 63, 64, 65, 127, 129]
        );
        assert_eq!(changes.since(changes.latest).count(), 0);
        let latest = changes.latest;
        changes.touch(130);
        assert_eq!(changes.latest, latest, "out-of-bounds writes are ignored");
    }

    #[test]
    fn row_generation_overflow_changes_identity_and_reinitializes_summaries() {
        let mut changes = RowChanges::new(130);
        let epoch = changes.epoch();
        changes.latest = u64::MAX - 1;
        changes.touch(129);
        assert_eq!(changes.latest, u64::MAX);
        assert_eq!(changes.since(u64::MAX - 1).collect::<Vec<_>>(), [129]);
        changes.touch(64);
        assert_ne!(
            changes.epoch(),
            epoch,
            "old slot generations belong to another epoch"
        );
        assert_eq!(changes.latest, 1);
        assert!(changes.rows.iter().all(|generation| *generation == 1));
        assert!(changes.blocks.iter().all(|generation| *generation == 1));
        assert_eq!(
            changes.since(0).collect::<Vec<_>>(),
            (0..130).collect::<Vec<_>>()
        );
        changes.touch(63);
        assert_eq!(changes.since(1).collect::<Vec<_>>(), [63]);
    }

    #[test]
    fn reused_plans_keep_row_storage_on_idle_damage_growth_and_invalidated_retry() {
        let (mut cells, mut text) = grids(130, 2);
        let mut uploads = SlotUploads::default();
        let mut plan = UploadPlan::default();
        uploads.plan_into(Some(&cells), Some(&text), 0, &mut plan);
        let cell_storage = (plan.cell_rows.as_ptr(), plan.cell_rows.capacity());
        let text_storage = (plan.text_rows.as_ptr(), plan.text_rows.capacity());
        for row in [0, 63, 64, 127, 129] {
            write_row(&mut cells, &mut text, row, 7);
            uploads.plan_into(Some(&cells), Some(&text), 0, &mut plan);
            assert_eq!(plan.cell_rows, [row]);
            assert_eq!(text_rows(&plan), [row]);
            uploads.plan_into(Some(&cells), Some(&text), 0, &mut plan);
            assert!(plan.cell_rows.is_empty() && plan.text_rows.is_empty());
            assert_eq!(
                (plan.cell_rows.as_ptr(), plan.cell_rows.capacity()),
                cell_storage
            );
            assert_eq!(
                (plan.text_rows.as_ptr(), plan.text_rows.capacity()),
                text_storage
            );
        }
        for _ in 0..6 {
            text.push(129, CellText::new(0, [1, 2, 3, 255]));
        }
        uploads.plan_into(Some(&cells), Some(&text), 0, &mut plan);
        assert!(plan.full);
        assert!(
            plan.capacity >= 8,
            "a changed tail row can grow the entire layout"
        );
        assert_eq!(plan.text_rows.len(), 130);
        uploads.invalidate();
        uploads.plan_into(Some(&cells), Some(&text), 1, &mut plan);
        assert!(plan.full);
        assert_eq!(plan.cell_rows.len(), 130);
        assert_eq!(
            (plan.cell_rows.as_ptr(), plan.cell_rows.capacity()),
            cell_storage
        );
        assert_eq!(
            (plan.text_rows.as_ptr(), plan.text_rows.capacity()),
            text_storage
        );
    }

    #[test]
    fn a_slot_uploads_everything_once_then_only_changed_rows() {
        let (mut cells, mut text) = grids(6, 4);
        let mut slot = SlotModel::default();
        let first = slot.upload(&cells, &text);
        assert!(first.full);
        assert_eq!((first.cell_rows.len(), first.text_rows.len()), (6, 6));
        let idle = slot.upload(&cells, &text);
        assert!(!idle.full && idle.cell_rows.is_empty() && idle.text_rows.is_empty());
        assert_eq!(
            idle.bytes(cells.extent()),
            0,
            "an unchanged frame uploads nothing"
        );

        write_row(&mut cells, &mut text, 3, 7);
        let one = slot.upload(&cells, &text);
        let ring = cells.ring_row(3);
        assert_eq!(
            (one.cell_rows.clone(), text_rows(&one)),
            (vec![ring], vec![ring])
        );
    }

    #[test]
    fn idle_plans_resume_after_independent_background_and_text_changes() {
        let (mut cells, mut text) = grids(6, 4);
        let mut slot = SlotModel::default();
        slot.upload(&cells, &text);
        for _ in 0..3 {
            assert_eq!(slot.upload(&cells, &text).bytes(cells.extent()), 0);
        }

        cells.set(2, 1, CellBg::rgb(9, 8, 7));
        let background = slot.upload(&cells, &text);
        assert_eq!(background.cell_rows, vec![2]);
        assert!(background.text_rows.is_empty());
        assert_eq!(slot.upload(&cells, &text).bytes(cells.extent()), 0);

        text.push(4, CellText::new(1, [1, 2, 3, 255]));
        let glyph = slot.upload(&cells, &text);
        assert!(glyph.cell_rows.is_empty());
        assert_eq!(text_rows(&glyph), vec![4]);
        assert_eq!(slot.upload(&cells, &text).bytes(cells.extent()), 0);

        text.clear_row(4);
        let cleared = slot.upload(&cells, &text);
        assert!(cleared.cell_rows.is_empty());
        assert_eq!(text_rows(&cleared), vec![4]);
        assert!(slot.text.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn idle_plans_still_reinitialize_replaced_grids_buffers_and_optional_text() {
        let (cells, text) = grids(6, 4);
        let mut uploads = SlotUploads::default();
        assert!(uploads.plan(Some(&cells), Some(&text), 0).full);
        assert!(!uploads.plan(Some(&cells), Some(&text), 0).full);

        // Clones have identical row generations but distinct grid epochs.
        let replacement_cells = cells.clone();
        let replacement_text = text.clone();
        assert!(uploads.plan(Some(&replacement_cells), Some(&text), 0).full);
        assert!(
            uploads
                .plan(Some(&replacement_cells), Some(&replacement_text), 0)
                .full
        );
        let replaced_buffers = uploads.plan(Some(&replacement_cells), Some(&replacement_text), 1);
        assert!(replaced_buffers.full);
        assert_eq!(replaced_buffers.cell_rows.len(), 6);
        assert_eq!(replaced_buffers.text_rows.len(), 6);
        assert!(
            replaced_buffers
                .text_rows
                .iter()
                .all(|row| row.instances == 4)
        );

        assert!(uploads.plan(Some(&replacement_cells), None, 1).full);
        assert!(!uploads.plan(Some(&replacement_cells), None, 1).full);
        let restored = uploads.plan(Some(&replacement_cells), Some(&replacement_text), 1);
        assert!(restored.full);
        assert!(restored.text_rows.iter().all(|row| row.instances == 4));
        uploads.invalidate();
        let retried = uploads.plan(Some(&replacement_cells), Some(&replacement_text), 1);
        assert!(retried.full);
        assert!(retried.text_rows.iter().all(|row| row.instances == 4));
    }

    #[test]
    fn empty_row_rotation_needs_no_upload_and_later_writes_use_the_new_offset() {
        let (mut cells, mut text) = grids(6, 4);
        let mut slot = SlotModel::default();
        slot.upload(&cells, &text);
        cells.scroll_up(2);
        text.scroll_up(2);
        assert_eq!(cells.row_offset(), 2);
        assert_eq!(slot.upload(&cells, &text).bytes(cells.extent()), 0);

        write_row(&mut cells, &mut text, 5, 7);
        let changed = slot.upload(&cells, &text);
        assert_eq!(changed.cell_rows, vec![1]);
        assert_eq!(text_rows(&changed), vec![1]);
        let (expected_cells, expected_text, expected_table) =
            full_layout(&cells, &text, changed.capacity);
        assert_eq!(slot.cells, expected_cells);
        assert_eq!(slot.text, expected_text);
        assert_eq!(slot.table, expected_table);
    }

    #[test]
    fn three_slots_converge_and_each_uploads_a_change_once() {
        let (mut cells, mut text) = grids(5, 4);
        let mut slots: Vec<SlotModel> = (0..3).map(|_| SlotModel::default()).collect();
        for slot in &mut slots {
            slot.upload(&cells, &text);
        }
        write_row(&mut cells, &mut text, 1, 3);
        for frame in 0..6 {
            let plan = slots[frame % 3].upload(&cells, &text);
            let expected = usize::from(frame < 3);
            assert_eq!(plan.cell_rows.len(), expected, "frame {frame}");
            assert_eq!(plan.text_rows.len(), expected, "frame {frame}");
        }
    }

    #[test]
    fn a_scroll_of_n_lines_uploads_n_rows() {
        let (mut cells, mut text) = grids(10, 8);
        for row in 0..10 {
            write_row(&mut cells, &mut text, row, row as u8 + 1);
        }
        let mut slot = SlotModel::default();
        slot.upload(&cells, &text);
        for lines in [1_u32, 3] {
            cells.scroll_up(lines);
            text.scroll_up(lines);
            for row in 10 - lines..10 {
                write_row(&mut cells, &mut text, row, 50 + row as u8);
            }
            let before = slot.bytes;
            let plan = slot.upload(&cells, &text);
            assert_eq!(plan.cell_rows.len(), lines as usize);
            assert_eq!(plan.text_rows.len(), lines as usize);
            let row_bytes = 8 * 4 + u64::from(plan.capacity) * 24 + 16;
            assert_eq!(slot.bytes - before, u64::from(lines) * row_bytes);
        }
    }

    #[test]
    fn scrolling_back_exposes_the_top_rows_and_uploads_only_them() {
        let (mut cells, mut text) = grids(6, 4);
        for row in 0..6 {
            write_row(&mut cells, &mut text, row, row as u8 + 1);
        }
        let shown: Vec<_> = (0..6).map(|row| cells.get(row, 0)).collect();
        let mut slot = SlotModel::default();
        slot.upload(&cells, &text);
        cells.scroll_down(2);
        text.scroll_down(2);
        for row in 0..2 {
            assert_eq!(
                cells.get(row, 0),
                Some(CellBg::DEFAULT),
                "row {row} is exposed"
            );
            assert_eq!(text.row(row).len(), 0, "row {row} is exposed");
        }
        for row in 2..6 {
            assert_eq!(
                cells.get(row, 0),
                shown[row as usize - 2],
                "row {row} moved down"
            );
        }
        for row in 0..2 {
            write_row(&mut cells, &mut text, row, 90 + row as u8);
        }
        let plan = slot.upload(&cells, &text);
        assert_eq!((plan.cell_rows.len(), plan.text_rows.len()), (2, 2));
        let (expected_cells, expected_text, _) = full_layout(&cells, &text, plan.capacity);
        assert_eq!(
            (slot.cells.clone(), slot.text.clone()),
            (expected_cells, expected_text)
        );
    }

    #[test]
    fn a_new_grid_new_buffers_or_an_overfull_row_relay_the_slot_out() {
        let (cells, mut text) = grids(4, 2);
        let mut uploads = SlotUploads::default();
        assert!(uploads.plan(Some(&cells), Some(&text), 0).full);
        assert!(!uploads.plan(Some(&cells), Some(&text), 0).full);
        assert!(
            uploads.plan(Some(&cells), Some(&text), 1).full,
            "buffers replaced"
        );
        let (other, _) = grids(4, 2);
        assert!(
            uploads.plan(Some(&other), Some(&text), 1).full,
            "another grid"
        );
        for layer in 0..5 {
            text.push(0, CellText::new(0, [layer, 0, 0, 255]));
        }
        let grown = uploads.plan(Some(&other), Some(&text), 1);
        assert!(grown.full, "a row past the capacity");
        assert!(grown.capacity >= 5);
        assert!(
            grown
                .text_rows
                .iter()
                .all(|row| row.instances == grown.capacity)
        );
    }

    #[test]
    fn sparse_text_uploads_retire_each_slots_own_previous_length() {
        let (cells, mut text) = grids(3, 512);
        let mut slots: Vec<SlotModel> = (0..3).map(|_| SlotModel::default()).collect();
        for slot in &mut slots {
            let initial = slot.upload(&cells, &text);
            assert!(initial.full);
            assert!(initial.text_rows.iter().all(|row| row.instances == 512));
            assert!(slot.text.iter().all(|byte| *byte == 0));
        }
        let set_row = |text: &mut CellTextGrid, count: u8| {
            text.clear_row(1);
            for col in 0..count {
                text.push(1, CellText::new(u16::from(col), [col, 9, 7, 255]));
            }
        };
        // Slots deliberately see different intermediate lengths.
        set_row(&mut text, 8);
        assert_eq!(
            slots[0].upload(&cells, &text).bytes(cells.extent()),
            8 * 24 + 16
        );
        set_row(&mut text, 3);
        assert_eq!(
            slots[1].upload(&cells, &text).bytes(cells.extent()),
            3 * 24 + 16
        );
        set_row(&mut text, 1);
        for (slot, instances) in slots.iter_mut().zip([8, 3, 1]) {
            let plan = slot.upload(&cells, &text);
            assert!(!plan.full);
            assert_eq!(plan.capacity, 512);
            assert_eq!(plan.text_rows, [TextRowUpload { row: 1, instances }]);
            assert_eq!(plan.bytes(cells.extent()), u64::from(instances) * 24 + 16);
            let (_, expected, _) = full_layout(&cells, &text, plan.capacity);
            assert_eq!(slot.text, expected);
            assert_eq!(slot.upload(&cells, &text).bytes(cells.extent()), 0);
        }
        set_row(&mut text, 0);
        for slot in &mut slots {
            let cleared = slot.upload(&cells, &text);
            assert_eq!(
                cleared.text_rows,
                [TextRowUpload {
                    row: 1,
                    instances: 1
                }]
            );
            assert_eq!(cleared.bytes(cells.extent()), 24 + 16);
            assert!(slot.text.iter().all(|byte| *byte == 0));
        }
    }

    #[test]
    fn text_draws_trim_only_the_unused_suffix_of_the_full_ring_layout() {
        let (cells, mut text) = grids(6, 4);
        let mut slot = SlotModel::default();
        let empty = slot.upload(&cells, &text);
        assert_eq!(empty.instances(cells.extent()), 24);
        assert_eq!(empty.draw_instances(&text), 0);
        assert_eq!(slot.text.len(), 24 * CELL_TEXT_INSTANCE_BYTES);
        assert_eq!(empty.bytes(cells.extent()), 6 * (4 * 4 + 4 * 24 + 16));

        text.push(0, CellText::new(1, [1, 2, 3, 255]));
        let first = slot.upload(&cells, &text);
        assert_eq!(first.draw_instances(&text), 1);
        text.push(3, CellText::new(0, [4, 5, 6, 255]));
        text.push(3, CellText::new(2, [7, 8, 9, 255]));
        let sparse = slot.upload(&cells, &text);
        assert_eq!(sparse.instances(cells.extent()), 24);
        assert_eq!(
            sparse.draw_instances(&text),
            14,
            "internal empty regions keep their offsets"
        );
        assert_eq!(slot.upload(&cells, &text).draw_instances(&text), 14);

        text.clear_row(3);
        let shorter = slot.upload(&cells, &text);
        assert_eq!(shorter.draw_instances(&text), 1);
        assert_eq!(
            shorter.bytes(cells.extent()),
            2 * 24 + 16,
            "all old instances are cleared; existing padding stays zero"
        );
        assert_eq!(slot.text.len(), 24 * CELL_TEXT_INSTANCE_BYTES);
        assert!(
            slot.text[CELL_TEXT_INSTANCE_BYTES..]
                .iter()
                .all(|byte| *byte == 0)
        );
        text.clear_row(0);
        assert_eq!(slot.upload(&cells, &text).draw_instances(&text), 0);
    }

    /// Randomized writes, clears, scrolls, new grids and resizes: every
    /// slot's buffers equal the full-rebuild layout of the current grids
    /// after each frame it receives.
    #[test]
    fn incremental_slots_equal_the_full_rebuild_layout() {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        // Cross both full summary blocks and the final partial block. Slots
        // observe different intermediate writes, scrolls and row lengths.
        let (mut rows, mut cols) = (130_usize, 6_usize);
        let (mut cells, mut text) = grids(rows, cols);
        let mut slots: Vec<SlotModel> = (0..3).map(|_| SlotModel::default()).collect();
        for frame in 0..2000_usize {
            for _ in 0..next(4) {
                let row = next(rows as u64) as u32;
                match next(10) {
                    0..=4 => write_row(&mut cells, &mut text, row, next(250) as u8),
                    5 => {
                        let col = next(cols as u64) as u32;
                        cells.set(row, col, CellBg::rgb(next(255) as u8, 0, 0));
                    }
                    6 => text.clear_row(row),
                    7 => {
                        for _ in 0..next(3) {
                            text.push(row, CellText::new(0, [9, 9, 9, 255]));
                        }
                    }
                    8 => {
                        let lines = 1 + next(rows as u64 - 1) as u32;
                        if next(2) == 0 {
                            cells.scroll_up(lines);
                            text.scroll_up(lines);
                        } else {
                            cells.scroll_down(lines);
                            text.scroll_down(lines);
                        }
                    }
                    _ => {
                        if next(20) == 0 {
                            rows = 3 + next(260) as usize;
                            cols = 2 + next(10) as usize;
                            (cells, text) = grids(rows, cols);
                        }
                    }
                }
            }
            let slot = &mut slots[frame % 3];
            if frame % 97 == 0 {
                // Simulate a partially failed write, whose contents can
                // no longer be trusted even without replacing the buffer.
                slot.uploads.invalidate();
                slot.text.fill(0x5a);
            } else if frame % 131 == 0 {
                // Replaced buffers have no known initialized prefix.
                slot.generation += 1;
                slot.text.fill(0xa5);
            }
            let plan = slot.upload(&cells, &text);
            let (expected_cells, expected_text, expected_table) =
                full_layout(&cells, &text, plan.capacity);
            assert_eq!(
                slot.cells, expected_cells,
                "frame {frame}: cell backgrounds"
            );
            assert_eq!(slot.text, expected_text, "frame {frame}: glyph instances");
            assert_eq!(slot.table, expected_table, "frame {frame}: row table");
            // A full row scan is the independent oracle for the cached tail.
            let last = (0..text.ring_rows())
                .rev()
                .find(|&row| text.ring_row_len(row) > 0);
            assert_eq!(text.last_nonempty_ring_row(), last, "frame {frame}: tail");
            let drawn = plan.draw_instances(&text);
            let expected_drawn = last.map_or(0, |row| {
                row * plan.capacity as usize + text.ring_row_len(row)
            });
            assert_eq!(drawn, expected_drawn, "frame {frame}: draw prefix");
            assert!(drawn <= plan.instances(cells.extent()));
            assert!(
                slot.text[drawn * CELL_TEXT_INSTANCE_BYTES..]
                    .iter()
                    .all(|byte| *byte == 0),
                "frame {frame}: omitted instances are empty"
            );
        }
    }
}
