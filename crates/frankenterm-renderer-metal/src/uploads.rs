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
//! shader collapses them to an empty quad and the text draw covers
//! `rows * capacity` instances. The capacity starts at one instance per cell
//! (or the fullest row) and grows by half when a row outgrows it, which
//! re-lays the slot out once.

use crate::cell_bg::CellBgGrid;
use crate::cell_text::CellTextGrid;
use crate::frame::{
    CELL_TEXT_INSTANCE_BYTES, CELL_TEXT_INSTANCES_PER_CELL, GridExtent, ROW_TABLE_BYTES_PER_ROW,
};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

/// Per-ring-row change generations of one grid instance.
#[derive(Debug)]
pub struct RowChanges {
    epoch: u64,
    latest: u64,
    rows: Vec<u64>,
}

impl RowChanges {
    /// A new grid instance of `rows` ring rows, every row at generation 1.
    #[must_use]
    pub fn new(rows: usize) -> Self {
        Self {
            epoch: NEXT_EPOCH.fetch_add(1, Ordering::Relaxed),
            latest: 1,
            rows: vec![1; rows],
        }
    }

    /// Ring row `ring_row` changed.
    pub fn touch(&mut self, ring_row: usize) {
        if let Some(row) = self.rows.get_mut(ring_row) {
            self.latest += 1;
            *row = self.latest;
        }
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
}

impl Clone for RowChanges {
    /// A clone diverges from its source, so it is a new instance: a slot
    /// that held the source re-uploads it in full.
    fn clone(&self) -> Self {
        Self {
            epoch: NEXT_EPOCH.fetch_add(1, Ordering::Relaxed),
            latest: self.latest,
            rows: self.rows.clone(),
        }
    }
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
    /// Ring rows of glyph instances (and their row-table entries) to upload.
    pub text_rows: Vec<u32>,
}

impl UploadPlan {
    /// Bytes the plan copies into the slot for a grid of `extent`.
    #[must_use]
    pub fn bytes(&self, extent: GridExtent) -> u64 {
        let cell_row = u64::from(extent.cols) * 4;
        let text_row = u64::from(self.capacity) * CELL_TEXT_INSTANCE_BYTES as u64
            + ROW_TABLE_BYTES_PER_ROW as u64;
        self.cell_rows.len() as u64 * cell_row + self.text_rows.len() as u64 * text_row
    }

    /// Instances the text draw covers for a grid of `extent`.
    #[must_use]
    pub fn instances(&self, extent: GridExtent) -> usize {
        usize::try_from(u64::from(extent.rows) * u64::from(self.capacity)).unwrap_or(usize::MAX)
    }
}

/// What one frame slot last uploaded.
#[derive(Debug, Clone, Default)]
pub struct SlotUploads {
    cells: Option<u64>,
    text: Option<u64>,
    extent: Option<GridExtent>,
    capacity: u32,
    buffers: Option<u64>,
    cells_written: Vec<u64>,
    text_written: Vec<u64>,
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
        let extent = cells
            .map(CellBgGrid::extent)
            .or_else(|| text.map(CellTextGrid::extent));
        let needed = text.map_or(1, needed_capacity);
        let same_grids = self.cells == cells.map(|grid| grid.changes().epoch())
            && self.text == text.map(|grid| grid.changes().epoch())
            && self.extent == extent;
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
            self.cells_written.clear();
            self.text_written.clear();
        }
        let mut plan = UploadPlan {
            full,
            capacity: self.capacity,
            ..UploadPlan::default()
        };
        if let Some(grid) = cells {
            let rows = usize::try_from(grid.extent().rows).unwrap_or(usize::MAX);
            self.cells_written.resize(rows, 0);
            for (ring, written) in self.cells_written.iter_mut().enumerate() {
                let generation = grid.changes().row(ring);
                if full || *written != generation {
                    *written = generation;
                    plan.cell_rows.push(u32::try_from(ring).unwrap_or(u32::MAX));
                }
            }
        }
        if let Some(grid) = text {
            self.text_written.resize(grid.ring_rows(), 0);
            for (ring, written) in self.text_written.iter_mut().enumerate() {
                let generation = grid.changes().row(ring);
                if full || *written != generation {
                    *written = generation;
                    plan.text_rows.push(u32::try_from(ring).unwrap_or(u32::MAX));
                }
            }
        }
        plan
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
        cells: Vec<u8>,
        text: Vec<u8>,
        table: Vec<u8>,
        bytes: u64,
    }

    impl SlotModel {
        fn upload(&mut self, cells: &CellBgGrid, text: &CellTextGrid) -> UploadPlan {
            let plan = self.uploads.plan(Some(cells), Some(text), 0);
            let extent = cells.extent();
            let cell_row = extent.cols as usize * 4;
            let region = plan.capacity as usize * CELL_TEXT_INSTANCE_BYTES;
            self.cells.resize(cells.as_bytes().len(), 0);
            self.text.resize(text.ring_rows() * region, 0);
            self.table
                .resize(text.ring_rows() * ROW_TABLE_BYTES_PER_ROW, 0);
            for &ring in &plan.cell_rows {
                let at = ring as usize * cell_row;
                self.cells[at..at + cell_row].copy_from_slice(&cells.as_bytes()[at..at + cell_row]);
            }
            for &ring in &plan.text_rows {
                let ring = ring as usize;
                let mut bytes = Vec::new();
                text_region(text, ring, plan.capacity, &mut bytes);
                self.text[ring * region..(ring + 1) * region].copy_from_slice(&bytes);
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
            (one.cell_rows.clone(), one.text_rows.clone()),
            (vec![ring], vec![ring])
        );
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
        let (mut rows, mut cols) = (8_usize, 6_usize);
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
                        cells.scroll_up(lines);
                        text.scroll_up(lines);
                    }
                    _ => {
                        if next(20) == 0 {
                            rows = 3 + next(10) as usize;
                            cols = 2 + next(10) as usize;
                            (cells, text) = grids(rows, cols);
                        }
                    }
                }
            }
            let slot = &mut slots[frame % 3];
            let plan = slot.upload(&cells, &text);
            let (expected_cells, expected_text, expected_table) =
                full_layout(&cells, &text, plan.capacity);
            assert_eq!(
                slot.cells, expected_cells,
                "frame {frame}: cell backgrounds"
            );
            assert_eq!(slot.text, expected_text, "frame {frame}: glyph instances");
            assert_eq!(slot.table, expected_table, "frame {frame}: row table");
        }
    }
}
