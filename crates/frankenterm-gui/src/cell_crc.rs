use frankenterm_core::cell_consistency_crc::{
    CellDiff, CellGridHash, CellRecord, GridSizeMismatch, MismatchAction, MismatchClass,
    MismatchPolicy, classify_mismatch, diff_grids, hash_cell_grid,
};
use serde::Serialize;
use std::fs::{OpenOptions, create_dir_all};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct GuiCellCrcHash {
    pub fnv: u64,
    pub crc: u32,
}

impl From<CellGridHash> for GuiCellCrcHash {
    fn from(hash: CellGridHash) -> Self {
        Self {
            fnv: hash.fnv,
            crc: hash.crc,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct GuiCellRecord {
    pub row: u16,
    pub col: u16,
    pub glyph_id: u32,
    pub fg_color: u32,
    pub bg_color: u32,
}

impl From<CellRecord> for GuiCellRecord {
    fn from(cell: CellRecord) -> Self {
        Self {
            row: cell.row,
            col: cell.col,
            glyph_id: cell.glyph_id,
            fg_color: cell.fg_color,
            bg_color: cell.bg_color,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuiCellDiff {
    pub row: u16,
    pub col: u16,
    pub expected: GuiCellRecord,
    pub actual: GuiCellRecord,
}

impl From<CellDiff> for GuiCellDiff {
    fn from(diff: CellDiff) -> Self {
        Self {
            row: diff.row,
            col: diff.col,
            expected: diff.expected.into(),
            actual: diff.actual.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuiCellCrcFrameReport {
    pub frame_idx: u64,
    pub expected_hash: CellGridHash,
    pub actual_hash: CellGridHash,
    pub matched: bool,
    pub grid_size: usize,
    pub mismatch_class: MismatchClass,
    pub action: Option<MismatchAction>,
    pub repro_seed: Option<u64>,
    pub diff_cells: Vec<GuiCellDiff>,
}

impl GuiCellCrcFrameReport {
    #[must_use]
    pub fn to_json_line(&self) -> GuiCellCrcJsonLine {
        GuiCellCrcJsonLine {
            frame_idx: self.frame_idx,
            expected_hash: self.expected_hash.into(),
            actual_hash: self.actual_hash.into(),
            matched: self.matched,
            grid_size: self.grid_size,
            mismatch_class: mismatch_class_label(self.mismatch_class),
            action: self.action.map(mismatch_action_label),
            repro_seed: self.repro_seed,
            diff_cells: self.diff_cells.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuiCellCrcJsonLine {
    pub frame_idx: u64,
    pub expected_hash: GuiCellCrcHash,
    pub actual_hash: GuiCellCrcHash,
    #[serde(rename = "match")]
    pub matched: bool,
    pub grid_size: usize,
    pub mismatch_class: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repro_seed: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub diff_cells: Vec<GuiCellDiff>,
}

#[must_use]
pub const fn gui_cell_crc_enabled() -> bool {
    cfg!(feature = "debug-cell-crc")
}

#[must_use]
pub const fn gui_cell_crc_policy_for_features() -> MismatchPolicy {
    if cfg!(feature = "fail-on-cell-mismatch") {
        MismatchPolicy::ci_strict()
    } else if cfg!(feature = "debug-cell-diff") {
        MismatchPolicy {
            on_both_differ: MismatchAction::DumpAndContinue,
            on_single_half_differs: MismatchAction::LogOnly,
        }
    } else {
        MismatchPolicy {
            on_both_differ: MismatchAction::LogOnly,
            on_single_half_differs: MismatchAction::LogOnly,
        }
    }
}

pub fn evaluate_cell_crc_frame(
    frame_idx: u64,
    expected: &[CellRecord],
    actual: &[CellRecord],
    policy: MismatchPolicy,
    repro_seed: Option<u64>,
) -> Result<GuiCellCrcFrameReport, GridSizeMismatch> {
    if expected.len() != actual.len() {
        return Err(GridSizeMismatch {
            expected_len: expected.len(),
            actual_len: actual.len(),
        });
    }

    let expected_hash = hash_cell_grid(expected);
    let actual_hash = hash_cell_grid(actual);
    let mismatch_class = classify_mismatch(expected_hash, actual_hash);
    let action = policy.action_for(mismatch_class);
    let diff_cells = if mismatch_class.is_mismatch() && cfg!(feature = "debug-cell-diff") {
        diff_grids(expected, actual)?
            .into_iter()
            .map(Into::into)
            .collect()
    } else {
        Vec::new()
    };

    Ok(GuiCellCrcFrameReport {
        frame_idx,
        expected_hash,
        actual_hash,
        matched: !mismatch_class.is_mismatch(),
        grid_size: expected.len(),
        mismatch_class,
        action,
        repro_seed,
        diff_cells,
    })
}

#[must_use]
pub fn default_cell_crc_log_dir() -> PathBuf {
    dirs_next::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ft")
        .join("debug")
}

#[must_use]
pub fn cell_crc_log_path(log_dir: &Path, unix_epoch_millis: u128) -> PathBuf {
    log_dir.join(format!("cell-crc-{unix_epoch_millis}.jsonl"))
}

pub fn append_cell_crc_json_line(
    report: &GuiCellCrcFrameReport,
    log_dir: &Path,
) -> io::Result<PathBuf> {
    create_dir_all(log_dir)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis();
    let path = cell_crc_log_path(log_dir, now);
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    serde_json::to_writer(&mut file, &report.to_json_line()).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    Ok(path)
}

#[must_use]
pub const fn mismatch_class_label(class: MismatchClass) -> &'static str {
    match class {
        MismatchClass::Match => "match",
        MismatchClass::BothDiffer => "both_differ",
        MismatchClass::OnlyFnvDiffers => "only_fnv_differs",
        MismatchClass::OnlyCrcDiffers => "only_crc_differs",
    }
}

#[must_use]
pub const fn mismatch_action_label(action: MismatchAction) -> &'static str {
    match action {
        MismatchAction::LogOnly => "log_only",
        MismatchAction::DumpAndContinue => "dump_and_continue",
        MismatchAction::Fail => "fail",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(row: u16, col: u16, glyph: u32) -> CellRecord {
        CellRecord::new(row, col, glyph, 0xff00_00ff, 0x0000_00ff)
    }

    #[test]
    fn gui_policy_defaults_to_log_only_without_debug_diff_feature() {
        let policy = gui_cell_crc_policy_for_features();

        if cfg!(feature = "fail-on-cell-mismatch") {
            assert_eq!(
                policy.action_for(MismatchClass::BothDiffer),
                Some(MismatchAction::Fail)
            );
        } else if cfg!(feature = "debug-cell-diff") {
            assert_eq!(
                policy.action_for(MismatchClass::BothDiffer),
                Some(MismatchAction::DumpAndContinue)
            );
        } else {
            assert_eq!(
                policy.action_for(MismatchClass::BothDiffer),
                Some(MismatchAction::LogOnly)
            );
        }
    }

    #[test]
    fn evaluate_cell_crc_frame_emits_json_schema_fields() {
        let expected = [cell(0, 0, 65), cell(0, 1, 66)];
        let actual = [cell(0, 0, 65), cell(0, 1, 67)];

        let report = evaluate_cell_crc_frame(
            9,
            &expected,
            &actual,
            MismatchPolicy {
                on_both_differ: MismatchAction::DumpAndContinue,
                on_single_half_differs: MismatchAction::LogOnly,
            },
            Some(123),
        )
        .unwrap();

        assert!(!report.matched);
        assert_eq!(report.grid_size, 2);
        assert_eq!(report.frame_idx, 9);
        assert_eq!(report.repro_seed, Some(123));
        assert_eq!(report.action, Some(MismatchAction::DumpAndContinue));

        let json = serde_json::to_value(report.to_json_line()).unwrap();
        assert_eq!(json["frame_idx"], 9);
        assert_eq!(json["match"], false);
        assert_eq!(json["grid_size"], 2);
        assert_eq!(json["action"], "dump_and_continue");
        assert_eq!(json["repro_seed"], 123);
        assert!(json.get("expected_hash").is_some());
        assert!(json.get("actual_hash").is_some());
    }

    #[test]
    fn evaluate_cell_crc_frame_rejects_grid_size_mismatch() {
        let expected = [cell(0, 0, 65), cell(0, 1, 66)];
        let actual = [cell(0, 0, 65)];

        let err = evaluate_cell_crc_frame(1, &expected, &actual, MismatchPolicy::default(), None)
            .unwrap_err();

        assert_eq!(err.expected_len, 2);
        assert_eq!(err.actual_len, 1);
    }

    #[test]
    fn append_cell_crc_json_line_writes_one_jsonl_record() {
        let dir = tempfile::tempdir().unwrap();
        let grid = [cell(0, 0, 65)];
        let report =
            evaluate_cell_crc_frame(1, &grid, &grid, MismatchPolicy::default(), None).unwrap();

        let path = append_cell_crc_json_line(&report, dir.path()).unwrap();
        let contents = std::fs::read_to_string(path).unwrap();

        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains("\"frame_idx\":1"));
        assert!(contents.contains("\"match\":true"));
    }
}
