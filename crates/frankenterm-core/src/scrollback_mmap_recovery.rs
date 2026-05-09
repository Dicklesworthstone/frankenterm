//! Crash-safe scrollback recovery — orphan-file scanner.
//!
//! **Beads:** [BR-TERM-EMULATOR-UPLIFT-2.5.2] — two parallel
//! bead IDs cover this substrate:
//! - `ft-5te6x` — session decomposition (this module's primary
//!   bead; closed earlier this session).
//! - `ft-2okh0.5.2` — canonical decomposition of parent epic
//!   `ft-2okh0.5`. Closed via cross-reference to ft-5te6x.
//!
//! Same cross-reference pattern as `ft-2okh0.5.1 ↔ ft-kscfg`
//! and `ft-2okh0.3.1 ↔ ft-d0ol8`. The bead-ID mapping table
//! lives at the parent design doc.
//!
//! **Design doc:** [`docs/design/crash-safe-scrollback.md`]
//! (../../../../docs/design/crash-safe-scrollback.md).
//! **Format types:** [`crate::scrollback_mmap_format`]
//! (shipped under ft-kscfg / ft-2okh0.5.1).
//!
//! # What this module ships
//!
//! The pure orphan-detection layer of the recovery flow. Walks a
//! scrollback directory (typically `~/.local/share/ft/scrollback/`),
//! reads each `.bin` file's 256-byte header via
//! [`crate::scrollback_mmap_format::ScrollbackHeader::decode`],
//! and classifies every candidate into one of:
//!
//! - [`OrphanState::Orphaned`] — header valid, no live owner; the
//!   restore prompt should offer this candidate.
//! - [`OrphanState::Locked`] — `.lock` file held by a live owner;
//!   skip (the owning ft instance is alive).
//! - [`OrphanState::Corrupt`] — header decode failed; the operator
//!   gets a structured error reason via the embedded
//!   [`HeaderDecodeError`].
//! - [`OrphanState::WrongShape`] — file exists but isn't a
//!   `<pane_uuid>.bin` (e.g. someone dropped a `.txt` next to the
//!   scrollback files); skip without raising.
//!
//! # What this module does NOT ship (filed as follow-on)
//!
//! - The interactive picker UI (a11y-clean keyboard-navigable
//!   prompt) — filed as `ft-5te6x.cont.picker`.
//! - The CLI commands `ft session list-orphans` /
//!   `ft session recover <id>` / `ft session discard <id>` —
//!   filed as `ft-5te6x.cont.cli`.
//! - The actual `mmap()` of the recovered file's content for read
//!   access — depends on `ft-z4u60` (the wiring follow-up to
//!   ft-kscfg) shipping the read-side `MmapScrollback::open`.
//!   This module concerns itself only with the *header-level*
//!   classification; ring-content access is the next layer.
//!
//! # Why ship the scanner first
//!
//! Orphan detection is the load-bearing decision for every other
//! recovery sub-task. The picker UI consumes
//! `Vec<OrphanCandidate>`; the CLI commands consume the same; the
//! kill-9 test fixture (ft-0ulxc) asserts that a known-uuid file
//! shows up in the scanner's output. Pinning the scan logic in
//! its own module, with synthetic-file unit tests, means each
//! follow-up implements against a stable contract.
//!
//! # Read-side flock semantic
//!
//! Per the design doc, `<pane_uuid>.bin.lock` is an `flock(LOCK_EX
//! | LOCK_NB)` advisory. A live ft instance holds the lock; an
//! orphan is a file whose lock is gone or whose lock is available
//! to a fresh `flock(LOCK_EX | LOCK_NB)` attempt. This module
//! exposes the lock-probe boundary as a [`LockProbe`] trait so
//! tests can substitute a deterministic probe — the production
//! implementation (a real `flock` call) lives under `ft-z4u60`.

use crate::scrollback_mmap_format::{HEADER_SIZE, HeaderDecodeError, ScrollbackHeader};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Classification of one candidate file in a scrollback directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanState {
    /// Header decoded cleanly and the file is not held by a live
    /// owner. Eligible for the restore prompt.
    Orphaned,
    /// `.lock` file is held by a live owner; another ft instance
    /// owns this scrollback. Skip without prompting.
    Locked,
    /// Header decode failed. The reason is in the
    /// `HeaderDecodeError` carried in [`OrphanCandidate::header`].
    /// Surface to the operator with the structured reason; the
    /// file is left in place so it can be hand-inspected.
    Corrupt,
    /// File doesn't match the `<pane_uuid>.bin` shape. Common
    /// cause: an unrelated file dropped into the scrollback dir.
    /// Skipped without raising.
    WrongShape,
}

/// One candidate file from the scan. Always carries the absolute
/// path; carries the parsed header iff [`Self::state`] is
/// [`OrphanState::Orphaned`] or — usefully — [`OrphanState::Locked`]
/// (held files still have decodable headers; the lock just means
/// another owner is alive).
#[derive(Debug, Clone)]
pub struct OrphanCandidate {
    pub path: PathBuf,
    pub state: OrphanState,
    /// `Ok(header)` when the header decoded cleanly,
    /// `Err(decode_error)` when [`Self::state`] is
    /// [`OrphanState::Corrupt`]. Always `None` for
    /// [`OrphanState::WrongShape`].
    pub header: Option<Result<ScrollbackHeader, HeaderDecodeError>>,
}

impl OrphanCandidate {
    /// Convenience: extract the parsed header iff the candidate
    /// is in a state where the header was readable
    /// (`Orphaned` or `Locked`).
    #[must_use]
    pub fn header_ok(&self) -> Option<&ScrollbackHeader> {
        match (&self.state, &self.header) {
            (OrphanState::Orphaned | OrphanState::Locked, Some(Ok(header))) => Some(header),
            _ => None,
        }
    }

    /// Convenience: extract the structured decode error iff
    /// [`Self::state`] is [`OrphanState::Corrupt`].
    #[must_use]
    pub fn corrupt_reason(&self) -> Option<&HeaderDecodeError> {
        match (&self.state, &self.header) {
            (OrphanState::Corrupt, Some(Err(err))) => Some(err),
            _ => None,
        }
    }
}

/// Action the recovery picker will apply to selected candidates when
/// the operator confirms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Re-attach eligible orphaned scrollback files.
    Recover,
    /// Discard selected files. Corrupt files are selectable only in
    /// this mode because they cannot be recovered safely.
    Discard,
}

/// Decision emitted by the picker for each displayed candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDecision {
    /// Recover the scrollback file at this path.
    Recover(PathBuf),
    /// Discard the scrollback file at this path.
    Discard(PathBuf),
    /// Leave this scrollback file untouched.
    Skip(PathBuf),
}

/// Keyboard input understood by the recovery picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanPickerKey {
    /// Move highlight one row up.
    Up,
    /// Move highlight one row down.
    Down,
    /// Toggle the highlighted row.
    Toggle,
    /// Confirm the current selection.
    Confirm,
    /// Cancel the picker.
    Cancel,
}

/// Result of applying one key to the picker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanPickerOutcome {
    /// Picker remains open.
    Pending,
    /// Operator confirmed; decisions are ready for the caller.
    Confirmed(Vec<RecoveryDecision>),
    /// Operator cancelled; caller should leave every file untouched.
    Cancelled,
}

/// Badge shown next to one displayed picker row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanPickerBadge {
    /// Eligible orphan file.
    Orphaned,
    /// Held by another live owner; greyed out and never selectable.
    Locked,
    /// Header decode failed; shown with the structured reason.
    Corrupt,
}

impl OrphanPickerBadge {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Orphaned => "orphaned",
            Self::Locked => "locked",
            Self::Corrupt => "corrupt",
        }
    }
}

/// One row in the interactive recovery picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanPickerRow {
    pub path: PathBuf,
    pub pane_uuid_short: String,
    pub created_at_epoch_ms: Option<u64>,
    pub bytes_written: Option<u64>,
    pub last_msync_age_ms: Option<u64>,
    pub badge: OrphanPickerBadge,
    pub corrupt_reason: Option<String>,
    pub selectable: bool,
    pub selected: bool,
    pub accessibility_label: String,
}

/// Pure picker state for scrollback-orphan recovery. This is the
/// UI-independent layer consumed by a terminal picker, a snapshot
/// test, or the later CLI command plumbing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanPickerState {
    action: RecoveryAction,
    rows: Vec<OrphanPickerRow>,
    highlighted: Option<usize>,
}

impl OrphanPickerState {
    /// Build display rows from scanner output. Wrong-shape files are
    /// intentionally hidden; locked files are displayed but disabled.
    #[must_use]
    pub fn new(candidates: &[OrphanCandidate], action: RecoveryAction, now_epoch_ms: u64) -> Self {
        let rows: Vec<OrphanPickerRow> = candidates
            .iter()
            .filter_map(|candidate| row_from_candidate(candidate, action, now_epoch_ms))
            .collect();
        let highlighted = rows.first().map(|_| 0);
        Self {
            action,
            rows,
            highlighted,
        }
    }

    #[must_use]
    pub fn action(&self) -> RecoveryAction {
        self.action
    }

    #[must_use]
    pub fn rows(&self) -> &[OrphanPickerRow] {
        &self.rows
    }

    #[must_use]
    pub fn highlighted(&self) -> Option<usize> {
        self.highlighted
    }

    #[must_use]
    pub fn highlighted_row(&self) -> Option<&OrphanPickerRow> {
        self.highlighted.and_then(|idx| self.rows.get(idx))
    }

    /// Move highlight one displayed row up, saturating at the top.
    pub fn move_up(&mut self) {
        if let Some(idx) = self.highlighted {
            self.highlighted = Some(idx.saturating_sub(1));
        }
    }

    /// Move highlight one displayed row down, saturating at the last
    /// visible row.
    pub fn move_down(&mut self) {
        if let Some(idx) = self.highlighted {
            let last = self.rows.len().saturating_sub(1);
            self.highlighted = Some((idx + 1).min(last));
        }
    }

    /// Toggle the highlighted row. Disabled rows are left unchanged.
    pub fn toggle_highlighted(&mut self) {
        let Some(idx) = self.highlighted else {
            return;
        };
        let Some(row) = self.rows.get_mut(idx) else {
            return;
        };
        if row.selectable {
            row.selected = !row.selected;
        }
    }

    /// Convert the current selection into one decision per displayed
    /// row. Hidden wrong-shape files are deliberately absent.
    #[must_use]
    pub fn confirm(&self) -> Vec<RecoveryDecision> {
        self.rows
            .iter()
            .map(|row| {
                if row.selected {
                    match self.action {
                        RecoveryAction::Recover => RecoveryDecision::Recover(row.path.clone()),
                        RecoveryAction::Discard => RecoveryDecision::Discard(row.path.clone()),
                    }
                } else {
                    RecoveryDecision::Skip(row.path.clone())
                }
            })
            .collect()
    }

    /// Apply one keyboard action and return the resulting picker
    /// outcome. This maps directly to up/down, space, enter, and q/Esc.
    pub fn handle_key(&mut self, key: OrphanPickerKey) -> OrphanPickerOutcome {
        match key {
            OrphanPickerKey::Up => {
                self.move_up();
                OrphanPickerOutcome::Pending
            }
            OrphanPickerKey::Down => {
                self.move_down();
                OrphanPickerOutcome::Pending
            }
            OrphanPickerKey::Toggle => {
                self.toggle_highlighted();
                OrphanPickerOutcome::Pending
            }
            OrphanPickerKey::Confirm => OrphanPickerOutcome::Confirmed(self.confirm()),
            OrphanPickerKey::Cancel => OrphanPickerOutcome::Cancelled,
        }
    }
}

fn row_from_candidate(
    candidate: &OrphanCandidate,
    action: RecoveryAction,
    now_epoch_ms: u64,
) -> Option<OrphanPickerRow> {
    match candidate.state {
        OrphanState::WrongShape => None,
        OrphanState::Orphaned | OrphanState::Locked => {
            let header = candidate.header_ok()?;
            let badge = if candidate.state == OrphanState::Locked {
                OrphanPickerBadge::Locked
            } else {
                OrphanPickerBadge::Orphaned
            };
            let selectable = candidate.state == OrphanState::Orphaned;
            let last_msync_age_ms = now_epoch_ms.checked_sub(header.last_msync_at_epoch_ms);
            let pane_uuid_short = pane_uuid_short_from_header(header);
            let accessibility_label = format_accessibility_label(
                &pane_uuid_short,
                badge,
                Some(header.total_bytes_written),
                last_msync_age_ms,
                None,
                selectable,
            );
            Some(OrphanPickerRow {
                path: candidate.path.clone(),
                pane_uuid_short,
                created_at_epoch_ms: Some(header.created_at_epoch_ms),
                bytes_written: Some(header.total_bytes_written),
                last_msync_age_ms,
                badge,
                corrupt_reason: None,
                selectable,
                selected: false,
                accessibility_label,
            })
        }
        OrphanState::Corrupt => {
            let reason = candidate.corrupt_reason().map_or_else(
                || "unknown header decode error".to_string(),
                ToString::to_string,
            );
            let selectable = action == RecoveryAction::Discard;
            let pane_uuid_short = pane_uuid_short_from_path(&candidate.path);
            let accessibility_label = format_accessibility_label(
                &pane_uuid_short,
                OrphanPickerBadge::Corrupt,
                None,
                None,
                Some(&reason),
                selectable,
            );
            Some(OrphanPickerRow {
                path: candidate.path.clone(),
                pane_uuid_short,
                created_at_epoch_ms: None,
                bytes_written: None,
                last_msync_age_ms: None,
                badge: OrphanPickerBadge::Corrupt,
                corrupt_reason: Some(reason),
                selectable,
                selected: false,
                accessibility_label,
            })
        }
    }
}

fn pane_uuid_short_from_header(header: &ScrollbackHeader) -> String {
    let mut out = String::with_capacity(16);
    for byte in header.pane_uuid.iter().take(8) {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn pane_uuid_short_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.chars().take(16).collect())
        .unwrap_or_else(|| "unknown".to_string())
}

fn format_accessibility_label(
    pane_uuid_short: &str,
    badge: OrphanPickerBadge,
    bytes_written: Option<u64>,
    last_msync_age_ms: Option<u64>,
    corrupt_reason: Option<&str>,
    selectable: bool,
) -> String {
    let availability = if selectable { "selectable" } else { "disabled" };
    match badge {
        OrphanPickerBadge::Corrupt => format!(
            "scrollback orphan {pane_uuid_short}, corrupt, {availability}, reason: {}",
            corrupt_reason.unwrap_or("unknown")
        ),
        OrphanPickerBadge::Locked => format!(
            "scrollback orphan {pane_uuid_short}, locked, disabled, {bytes} bytes written, last sync age {age} ms",
            bytes = bytes_written.unwrap_or(0),
            age = last_msync_age_ms.unwrap_or(0)
        ),
        OrphanPickerBadge::Orphaned => format!(
            "scrollback orphan {pane_uuid_short}, orphaned, {availability}, {bytes} bytes written, last sync age {age} ms",
            bytes = bytes_written.unwrap_or(0),
            age = last_msync_age_ms.unwrap_or(0)
        ),
    }
}

/// Probe whether a `<pane_uuid>.bin.lock` is held by a live owner.
/// Production implementation calls `flock(LOCK_EX | LOCK_NB)`;
/// tests substitute a deterministic probe via a closure-impl.
pub trait LockProbe {
    /// Returns `true` if the lock is currently held by another
    /// process (i.e. the corresponding `.bin` is owned by a live
    /// ft instance — not an orphan).
    fn is_locked(&self, lock_path: &Path) -> bool;
}

/// Synchronous closure-shaped LockProbe. Useful for tests and for
/// embedding ad-hoc probes (e.g. environment-variable overrides).
impl<F> LockProbe for F
where
    F: Fn(&Path) -> bool,
{
    fn is_locked(&self, lock_path: &Path) -> bool {
        (self)(lock_path)
    }
}

/// Default lock probe used when the caller has no override.
/// Returns `false` for every input — no lock = orphan. The
/// production probe (with the real `flock` syscall) is wired in
/// under `ft-z4u60`; until then, the default treats every file as
/// orphaned, which is the safe direction (the picker just shows
/// extra entries; the operator can discard them).
#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysOrphaned;

impl LockProbe for AlwaysOrphaned {
    fn is_locked(&self, _lock_path: &Path) -> bool {
        false
    }
}

/// Walk `scrollback_dir` and classify every entry. Files matching
/// the `<pane_uuid>.bin` shape are header-decoded; non-matching
/// names are tagged [`OrphanState::WrongShape`] and listed without
/// raising.
///
/// `lock_probe` is consulted for every successfully-decoded
/// header to decide between [`OrphanState::Orphaned`] and
/// [`OrphanState::Locked`].
///
/// IO errors on directory enumeration are surfaced as `Err`. IO
/// errors on individual file reads are recorded as
/// [`OrphanState::Corrupt`] with a synthesized
/// [`HeaderDecodeError::Truncated`] entry (the file is
/// uninspectable — same operator UX as a truly truncated header).
pub fn scan_orphans(
    scrollback_dir: &Path,
    lock_probe: &impl LockProbe,
) -> std::io::Result<Vec<OrphanCandidate>> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(scrollback_dir)?;
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    paths.sort();
    for path in paths {
        out.push(classify_path(&path, lock_probe));
    }
    Ok(out)
}

/// Classify one path. Public for callers that already have a
/// `Vec<PathBuf>` and want to reuse the per-file logic without
/// re-walking the directory.
pub fn classify_path(path: &Path, lock_probe: &impl LockProbe) -> OrphanCandidate {
    if !is_scrollback_filename(path) {
        return OrphanCandidate {
            path: path.to_path_buf(),
            state: OrphanState::WrongShape,
            header: None,
        };
    }

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => {
            return OrphanCandidate {
                path: path.to_path_buf(),
                state: OrphanState::Corrupt,
                header: Some(Err(HeaderDecodeError::Truncated {
                    expected: HEADER_SIZE,
                    actual: 0,
                })),
            };
        }
    };

    match ScrollbackHeader::decode(&bytes) {
        Ok(header) => {
            let lock_path = path.with_extension("bin.lock");
            let is_held = lock_probe.is_locked(&lock_path);
            OrphanCandidate {
                path: path.to_path_buf(),
                state: if is_held {
                    OrphanState::Locked
                } else {
                    OrphanState::Orphaned
                },
                header: Some(Ok(header)),
            }
        }
        Err(err) => OrphanCandidate {
            path: path.to_path_buf(),
            state: OrphanState::Corrupt,
            header: Some(Err(err)),
        },
    }
}

/// Heuristic: is this filename a scrollback `<pane_uuid>.bin`?
/// Accepts any 64-character hex stem (the `pane_uuid` is 32 bytes
/// → 64 hex chars) plus `.bin` extension. Anything else is
/// classified as [`OrphanState::WrongShape`].
fn is_scrollback_filename(path: &Path) -> bool {
    let stem = match path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s,
        None => return false,
    };
    let ext = path.extension().and_then(|s| s.to_str());
    if ext != Some("bin") {
        return false;
    }
    if stem.len() != 64 {
        return false;
    }
    stem.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrollback_mmap_format::{FormatVersion, HeaderFlags};
    use std::fs;
    use std::io::Write;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ft_5te6x_{label}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_valid_scrollback(dir: &Path, uuid_byte: u8) -> PathBuf {
        let stem = format!("{uuid_byte:02x}").repeat(32);
        let path = dir.join(format!("{stem}.bin"));
        let header = ScrollbackHeader {
            version: FormatVersion::V1,
            flags: HeaderFlags::empty(),
            capacity_bytes: 1024,
            write_cursor_bytes: 256,
            pane_uuid: [uuid_byte; 32],
            created_at_epoch_ms: 1_000,
            last_msync_at_epoch_ms: 2_000,
            redactions_applied: 3,
            total_bytes_written: 100,
        };
        let bytes = header.encode();
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(&bytes).unwrap();
        path
    }

    fn write_corrupt_scrollback(dir: &Path, uuid_byte: u8) -> PathBuf {
        let stem = format!("{uuid_byte:02x}").repeat(32);
        let path = dir.join(format!("{stem}.bin"));
        let mut bad = vec![0u8; HEADER_SIZE];
        bad[0..4].copy_from_slice(b"NOPE");
        fs::write(&path, bad).unwrap();
        path
    }

    #[test]
    fn scan_empty_dir_returns_empty_vec() {
        let dir = temp_dir("empty");
        let out = scan_orphans(&dir, &AlwaysOrphaned).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn scan_decodes_valid_scrollback_as_orphaned() {
        let dir = temp_dir("valid");
        write_valid_scrollback(&dir, 0xAB);
        let out = scan_orphans(&dir, &AlwaysOrphaned).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::Orphaned);
        let h = out[0].header_ok().expect("header decoded");
        assert_eq!(h.pane_uuid[0], 0xAB);
        assert_eq!(h.write_cursor_bytes, 256);
    }

    #[test]
    fn scan_marks_locked_files_correctly() {
        let dir = temp_dir("locked");
        let p = write_valid_scrollback(&dir, 0xCD);
        let lock_p = p.with_extension("bin.lock");
        let out = scan_orphans(&dir, &|probe_path: &Path| probe_path == lock_p).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::Locked);
        // header is still decoded for locked files.
        assert!(out[0].header_ok().is_some());
    }

    #[test]
    fn scan_marks_corrupt_header_with_decode_error() {
        let dir = temp_dir("corrupt");
        // Write a 64-char hex stem with bad magic bytes.
        let stem = "ee".repeat(32);
        let path = dir.join(format!("{stem}.bin"));
        let mut bad = vec![0u8; HEADER_SIZE];
        bad[0..4].copy_from_slice(b"XXXX"); // bad magic
        fs::write(&path, bad).unwrap();

        let out = scan_orphans(&dir, &AlwaysOrphaned).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::Corrupt);
        let reason = out[0].corrupt_reason().expect("decode error attached");
        assert!(matches!(reason, HeaderDecodeError::BadMagic { .. }));
    }

    #[test]
    fn scan_marks_truncated_file_as_corrupt() {
        let dir = temp_dir("trunc");
        let stem = "11".repeat(32);
        let path = dir.join(format!("{stem}.bin"));
        fs::write(&path, [0u8; 50]).unwrap(); // shorter than HEADER_SIZE

        let out = scan_orphans(&dir, &AlwaysOrphaned).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::Corrupt);
        let reason = out[0].corrupt_reason().expect("decode error attached");
        assert!(matches!(reason, HeaderDecodeError::Truncated { .. }));
    }

    #[test]
    fn scan_skips_wrong_shape_files() {
        let dir = temp_dir("wrong_shape");
        // Drop a non-matching file.
        fs::write(dir.join("README.md"), b"not a scrollback").unwrap();
        // Drop a `.txt` file too.
        fs::write(dir.join("notes.txt"), b"unrelated").unwrap();
        // Drop a scrollback-shaped file alongside.
        write_valid_scrollback(&dir, 0xFF);

        let out = scan_orphans(&dir, &AlwaysOrphaned).unwrap();
        assert_eq!(out.len(), 3);
        let states: Vec<&OrphanState> = out.iter().map(|c| &c.state).collect();
        // Sorted by path, so `README.md` (R) < `<hex>.bin` (f) < `notes.txt` (n).
        // Sort order: ASCII compares uppercase < lowercase, so 'R' < 'f' < 'n'.
        assert!(states.contains(&&OrphanState::Orphaned));
        assert_eq!(
            states
                .iter()
                .filter(|s| ***s == OrphanState::WrongShape)
                .count(),
            2
        );
    }

    #[test]
    fn scan_classifies_short_stem_as_wrong_shape() {
        let dir = temp_dir("short");
        // .bin extension but stem is only 8 chars, not 64.
        fs::write(dir.join("abcd1234.bin"), [0u8; HEADER_SIZE]).unwrap();
        let out = scan_orphans(&dir, &AlwaysOrphaned).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::WrongShape);
    }

    #[test]
    fn scan_classifies_non_hex_stem_as_wrong_shape() {
        let dir = temp_dir("nonhex");
        // 64 chars but contains non-hex.
        let stem = "z".repeat(64);
        fs::write(dir.join(format!("{stem}.bin")), [0u8; HEADER_SIZE]).unwrap();
        let out = scan_orphans(&dir, &AlwaysOrphaned).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::WrongShape);
    }

    #[test]
    fn scan_classifies_wrong_extension_as_wrong_shape() {
        let dir = temp_dir("wrongext");
        // 64-hex stem but `.dat` extension instead of `.bin`.
        let stem = "00".repeat(32);
        fs::write(dir.join(format!("{stem}.dat")), [0u8; HEADER_SIZE]).unwrap();
        let out = scan_orphans(&dir, &AlwaysOrphaned).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::WrongShape);
    }

    #[test]
    fn scan_handles_multiple_orphans_in_sorted_order() {
        let dir = temp_dir("multi");
        write_valid_scrollback(&dir, 0x00);
        write_valid_scrollback(&dir, 0x11);
        write_valid_scrollback(&dir, 0x22);
        let out = scan_orphans(&dir, &AlwaysOrphaned).unwrap();
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|c| c.state == OrphanState::Orphaned));
        // Sorted by path → uuid bytes 0x00 < 0x11 < 0x22.
        assert_eq!(out[0].header_ok().unwrap().pane_uuid[0], 0x00);
        assert_eq!(out[1].header_ok().unwrap().pane_uuid[0], 0x11);
        assert_eq!(out[2].header_ok().unwrap().pane_uuid[0], 0x22);
    }

    #[test]
    fn classify_path_works_standalone_without_directory_walk() {
        let dir = temp_dir("standalone");
        let p = write_valid_scrollback(&dir, 0x55);
        let candidate = classify_path(&p, &AlwaysOrphaned);
        assert_eq!(candidate.state, OrphanState::Orphaned);
        assert!(candidate.header_ok().is_some());
    }

    #[test]
    fn scan_returns_io_error_on_missing_directory() {
        let dir = std::env::temp_dir().join("ft_5te6x_does_not_exist_anywhere");
        let _ = fs::remove_dir_all(&dir);
        let result = scan_orphans(&dir, &AlwaysOrphaned);
        assert!(result.is_err());
    }

    #[test]
    fn lock_probe_closure_form_works() {
        let dir = temp_dir("closure_probe");
        let p = write_valid_scrollback(&dir, 0xEE);
        let expected_lock = p.with_extension("bin.lock");
        let probe = |path: &Path| path == expected_lock;
        let out = scan_orphans(&dir, &probe).unwrap();
        assert_eq!(out[0].state, OrphanState::Locked);
    }

    #[test]
    fn always_orphaned_probe_treats_every_file_as_orphan() {
        let dir = temp_dir("always_orphaned");
        write_valid_scrollback(&dir, 0x99);
        let out = scan_orphans(&dir, &AlwaysOrphaned).unwrap();
        assert_eq!(out[0].state, OrphanState::Orphaned);
    }

    #[test]
    fn picker_filters_wrong_shape_and_builds_accessible_rows() {
        let dir = temp_dir("picker_rows");
        fs::write(dir.join("notes.txt"), b"unrelated").unwrap();
        let orphan_path = write_valid_scrollback(&dir, 0x10);
        let corrupt_path = write_corrupt_scrollback(&dir, 0x20);
        let candidates = scan_orphans(&dir, &AlwaysOrphaned).unwrap();

        let picker = OrphanPickerState::new(&candidates, RecoveryAction::Recover, 3_000);

        assert_eq!(picker.rows().len(), 2);
        assert_eq!(picker.highlighted(), Some(0));
        assert_eq!(picker.rows()[0].path, orphan_path);
        assert_eq!(picker.rows()[0].badge, OrphanPickerBadge::Orphaned);
        assert!(picker.rows()[0].selectable);
        assert!(
            picker.rows()[0]
                .accessibility_label
                .contains("1010101010101010")
        );
        assert!(
            picker.rows()[0]
                .accessibility_label
                .contains("100 bytes written")
        );
        assert_eq!(picker.rows()[1].path, corrupt_path);
        assert_eq!(picker.rows()[1].badge, OrphanPickerBadge::Corrupt);
        assert!(!picker.rows()[1].selectable);
        assert!(
            picker.rows()[1]
                .accessibility_label
                .contains("corrupt, disabled")
        );
    }

    #[test]
    fn picker_keyboard_toggle_and_confirm_recovery_decisions() {
        let dir = temp_dir("picker_keyboard");
        let first = write_valid_scrollback(&dir, 0x01);
        let second = write_valid_scrollback(&dir, 0x02);
        let candidates = scan_orphans(&dir, &AlwaysOrphaned).unwrap();
        let mut picker = OrphanPickerState::new(&candidates, RecoveryAction::Recover, 3_000);

        assert_eq!(
            picker.handle_key(OrphanPickerKey::Toggle),
            OrphanPickerOutcome::Pending
        );
        assert!(picker.rows()[0].selected);
        assert_eq!(
            picker.handle_key(OrphanPickerKey::Down),
            OrphanPickerOutcome::Pending
        );
        assert_eq!(picker.highlighted(), Some(1));
        assert_eq!(
            picker.handle_key(OrphanPickerKey::Confirm),
            OrphanPickerOutcome::Confirmed(vec![
                RecoveryDecision::Recover(first),
                RecoveryDecision::Skip(second),
            ])
        );
    }

    #[test]
    fn picker_locked_rows_are_visible_but_not_selectable() {
        let dir = temp_dir("picker_locked");
        let locked = write_valid_scrollback(&dir, 0x33);
        let lock_p = locked.with_extension("bin.lock");
        let candidates = scan_orphans(&dir, &|probe_path: &Path| probe_path == lock_p).unwrap();
        let mut picker = OrphanPickerState::new(&candidates, RecoveryAction::Recover, 3_000);

        assert_eq!(picker.rows().len(), 1);
        assert_eq!(picker.rows()[0].badge, OrphanPickerBadge::Locked);
        assert!(!picker.rows()[0].selectable);
        picker.toggle_highlighted();
        assert!(!picker.rows()[0].selected);
        assert_eq!(picker.confirm(), vec![RecoveryDecision::Skip(locked)]);
    }

    #[test]
    fn picker_discard_mode_can_select_corrupt_candidates() {
        let dir = temp_dir("picker_discard_corrupt");
        let corrupt = write_corrupt_scrollback(&dir, 0x44);
        let candidates = scan_orphans(&dir, &AlwaysOrphaned).unwrap();
        let mut picker = OrphanPickerState::new(&candidates, RecoveryAction::Discard, 3_000);

        assert_eq!(picker.rows()[0].badge, OrphanPickerBadge::Corrupt);
        assert!(picker.rows()[0].selectable);
        assert_eq!(
            picker.handle_key(OrphanPickerKey::Toggle),
            OrphanPickerOutcome::Pending
        );
        assert_eq!(
            picker.handle_key(OrphanPickerKey::Confirm),
            OrphanPickerOutcome::Confirmed(vec![RecoveryDecision::Discard(corrupt)])
        );
    }

    #[test]
    fn picker_cancel_reports_cancelled_without_mutating_selection() {
        let dir = temp_dir("picker_cancel");
        write_valid_scrollback(&dir, 0x77);
        let candidates = scan_orphans(&dir, &AlwaysOrphaned).unwrap();
        let mut picker = OrphanPickerState::new(&candidates, RecoveryAction::Recover, 3_000);

        picker.toggle_highlighted();
        assert_eq!(
            picker.handle_key(OrphanPickerKey::Cancel),
            OrphanPickerOutcome::Cancelled
        );
        assert!(picker.rows()[0].selected);
    }
}
