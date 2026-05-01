//! Crash-safe scrollback recovery — orphan-file scanner.
//!
//! **Bead:** [BR-TERM-EMULATOR-UPLIFT-2.5.2] / `ft-5te6x`.
//! **Design doc:** [`docs/design/crash-safe-scrollback.md`]
//! (../../../../docs/design/crash-safe-scrollback.md).
//! **Format types:** [`crate::scrollback_mmap_format`]
//! (shipped under ft-kscfg).
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
        let dir = std::env::temp_dir().join(format!(
            "ft_5te6x_{label}_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_valid_scrollback(dir: &Path, uuid_byte: u8) -> PathBuf {
        let stem: String = (0..32).map(|_| format!("{uuid_byte:02x}")).collect();
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
        let stem: String = (0..32).map(|_| "ee".to_string()).collect();
        let path = dir.join(format!("{stem}.bin"));
        let mut bad = vec![0u8; HEADER_SIZE];
        bad[0..4].copy_from_slice(b"XXXX"); // bad magic
        fs::write(&path, &bad).unwrap();

        let out = scan_orphans(&dir, &AlwaysOrphaned).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::Corrupt);
        let reason = out[0].corrupt_reason().expect("decode error attached");
        assert!(matches!(reason, HeaderDecodeError::BadMagic { .. }));
    }

    #[test]
    fn scan_marks_truncated_file_as_corrupt() {
        let dir = temp_dir("trunc");
        let stem: String = (0..32).map(|_| "11".to_string()).collect();
        let path = dir.join(format!("{stem}.bin"));
        fs::write(&path, &[0u8; 50]).unwrap(); // shorter than HEADER_SIZE

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
            states.iter().filter(|s| ***s == OrphanState::WrongShape).count(),
            2
        );
    }

    #[test]
    fn scan_classifies_short_stem_as_wrong_shape() {
        let dir = temp_dir("short");
        // .bin extension but stem is only 8 chars, not 64.
        fs::write(dir.join("abcd1234.bin"), &[0u8; HEADER_SIZE]).unwrap();
        let out = scan_orphans(&dir, &AlwaysOrphaned).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::WrongShape);
    }

    #[test]
    fn scan_classifies_non_hex_stem_as_wrong_shape() {
        let dir = temp_dir("nonhex");
        // 64 chars but contains non-hex.
        let stem: String = std::iter::repeat('z').take(64).collect();
        fs::write(dir.join(format!("{stem}.bin")), &[0u8; HEADER_SIZE]).unwrap();
        let out = scan_orphans(&dir, &AlwaysOrphaned).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, OrphanState::WrongShape);
    }

    #[test]
    fn scan_classifies_wrong_extension_as_wrong_shape() {
        let dir = temp_dir("wrongext");
        // 64-hex stem but `.dat` extension instead of `.bin`.
        let stem: String = (0..32).map(|_| "00".to_string()).collect();
        fs::write(dir.join(format!("{stem}.dat")), &[0u8; HEADER_SIZE]).unwrap();
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
}
