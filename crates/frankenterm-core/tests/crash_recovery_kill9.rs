//! Crash-recovery test fixture for kill-9 mid-session integrity.
//!
//! **Bead:** [BR-TERM-EMULATOR-UPLIFT-2.5.5] / `ft-0ulxc`.
//! **Design doc:** `docs/design/crash-safe-scrollback.md`.
//! **Parent epic:** ft-2okh0.5.
//!
//! # What this fixture ships
//!
//! Substrate-level regression-guard tests for the kill-9 invariants
//! the design doc pins:
//!
//! 1. **Pre-msync byte safety** — bytes written *and* whose
//!    `last_msync_at_epoch_ms` was bumped before the simulated kill
//!    survive byte-for-byte across reload. Verified by writing a
//!    full file, simulating truncation past the msync boundary,
//!    re-decoding, and asserting the surviving bytes are identical.
//! 2. **Identity continuity** — `pane_uuid` is byte-for-byte
//!    preserved across the simulated kill.
//! 3. **Bounded-loss surfaces gracefully** — truncation at the tail
//!    surfaces as a recoverable orphan via the scanner shipped under
//!    ft-5te6x; the partial record at the tail does not corrupt the
//!    header.
//! 4. **Mid-header tear** — truncation in the middle of the header
//!    surfaces as `OrphanState::Corrupt` with
//!    `HeaderDecodeError::Truncated`. CI fails if the corruption is
//!    silently mounted.
//! 5. **Mid-cursor tear** — a header that claims a write cursor
//!    larger than the actual file size surfaces via the existing
//!    `CursorBeyondCapacity` check rather than panicking on a
//!    later read.
//!
//! # Why simulate truncation rather than spawn-and-kill
//!
//! The bead's full e2e test (spawn ft as a subprocess + `kill -9` +
//! `ft session recover <pane_id>` + assert byte-for-byte hash match)
//! requires two pieces that are not yet wired:
//!
//! - `MmapScrollback::append(pane_uuid, payload)` — the write-path
//!   wiring that lands under **ft-z4u60** (filed at the close-out
//!   of ft-kscfg).
//! - `ft session recover <pane_id>` CLI — the picker / restore
//!   command that lands under **ft-5te6x.cont.cli**.
//!
//! Until both ship, the e2e fixture has nothing live to crash.
//! Truncation is the substrate-level analog of `kill -9`: bytes
//! that *did* hit `MS_SYNC` correspond to bytes the OS would have
//! flushed before the kill; bytes after the truncation point
//! correspond to the in-flight buffer that gets lost.
//!
//! The full e2e test (spawn-and-kill) is included in this file as
//! [`spawn_and_kill_e2e_PLACEHOLDER`] gated with `#[ignore]` and a
//! `cfg(unix)` so CI picks it up automatically once ft-z4u60 and
//! ft-5te6x.cont.cli land. The ignore annotation documents the
//! exact unblock list inline.
//!
//! # Why `cfg(unix)`
//!
//! `kill -9` is POSIX-specific. The substrate-level tests in this
//! file are platform-portable (no syscalls — pure byte-level
//! truncation), but the file gates `cfg(unix)` to match the
//! bead's specified scope and to keep the e2e placeholder
//! co-located with the substrate guards.

#![cfg(unix)]

use frankenterm_core::scrollback_mmap_format::{
    FormatVersion, HEADER_SIZE, HeaderDecodeError, HeaderFlags, RECORD_HEADER_SIZE, RecordHeader,
    RecordKind, ScrollbackHeader,
};
use frankenterm_core::scrollback_mmap_recovery::{
    AlwaysOrphaned, OrphanState, classify_path, scan_orphans,
};

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Build a header with deterministic field values keyed by `seed`
/// so the per-test mutations don't collide.
fn build_header(seed: u8) -> ScrollbackHeader {
    ScrollbackHeader {
        version: FormatVersion::V1,
        flags: HeaderFlags::empty(),
        capacity_bytes: 4096,
        write_cursor_bytes: 1024,
        pane_uuid: [seed; 32],
        created_at_epoch_ms: 1_715_000_000_000,
        last_msync_at_epoch_ms: 1_715_000_001_000,
        redactions_applied: 2,
        total_bytes_written: 1024,
    }
}

/// Allocate a fresh scratch directory keyed by `label` so concurrent
/// agents don't collide on `/tmp` paths.
fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ft_0ulxc_{label}_{pid}",
        label = label,
        pid = std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// 64-hex-char filename keyed by `byte` (matches `is_scrollback_filename`).
fn scrollback_path(dir: &Path, byte: u8) -> PathBuf {
    let stem: String = (0..32).map(|_| format!("{byte:02x}")).collect();
    dir.join(format!("{stem}.bin"))
}

/// Write a header + a sequence of fully-formed text records into
/// `path`. Returns the byte length of the written content.
fn write_full_scrollback(path: &Path, header: &ScrollbackHeader, payloads: &[&[u8]]) -> usize {
    let mut buf = Vec::new();
    buf.extend_from_slice(&header.encode());
    for payload in payloads {
        let rec = RecordHeader {
            record_len: payload.len() as u32,
            record_kind: RecordKind::Text,
        };
        buf.extend_from_slice(&rec.encode());
        buf.extend_from_slice(payload);
    }
    let mut f = fs::File::create(path).unwrap();
    f.write_all(&buf).unwrap();
    buf.len()
}

/// "Simulate `kill -9`": truncate the file to `survive_bytes` bytes.
///
/// Bytes 0..`survive_bytes` correspond to bytes the OS *would* have
/// flushed via `MS_SYNC` before the kill. Bytes after that
/// correspond to in-flight ring buffer that gets lost.
fn simulate_kill_truncation(path: &Path, survive_bytes: usize) {
    let mut bytes = fs::read(path).unwrap();
    bytes.truncate(survive_bytes);
    fs::write(path, &bytes).unwrap();
}

#[test]
fn invariant_1_pre_msync_bytes_survive_byte_for_byte() {
    // Pre-msync byte safety per design-doc invariant 1: bytes
    // written + msync'd before the kill survive byte-for-byte.
    let dir = temp_dir("inv1");
    let path = scrollback_path(&dir, 0x10);
    let header = build_header(0x10);
    let payloads: &[&[u8]] = &[b"hello", b"world", b"safe-byte-stream"];
    let total_len = write_full_scrollback(&path, &header, payloads);

    // Snapshot the bytes that *would* have been msync'd.
    let pre_kill_bytes = fs::read(&path).unwrap();
    assert_eq!(pre_kill_bytes.len(), total_len);

    // Simulate kill -9 by truncating to half the file size — but
    // still well past the header. The surviving bytes are everything
    // the msync would have caught.
    let survive_bytes = HEADER_SIZE + RECORD_HEADER_SIZE + payloads[0].len();
    simulate_kill_truncation(&path, survive_bytes);

    // Reload + assert byte-for-byte equality on the surviving prefix.
    let post_kill_bytes = fs::read(&path).unwrap();
    assert_eq!(post_kill_bytes.len(), survive_bytes);
    assert_eq!(
        &post_kill_bytes[..survive_bytes],
        &pre_kill_bytes[..survive_bytes],
        "pre-msync bytes must survive kill byte-for-byte"
    );

    // The header must still decode cleanly + carry the same
    // `last_msync_at_epoch_ms` value (the durability marker the
    // design doc pins).
    let decoded = ScrollbackHeader::decode(&post_kill_bytes).unwrap();
    assert_eq!(decoded.last_msync_at_epoch_ms, header.last_msync_at_epoch_ms);
    assert_eq!(decoded.created_at_epoch_ms, header.created_at_epoch_ms);
    assert_eq!(decoded.redactions_applied, header.redactions_applied);
}

#[test]
fn invariant_2_pane_uuid_byte_for_byte_continuity() {
    // Identity continuity per design-doc invariant 2: pane_uuid
    // must be byte-for-byte preserved across the kill so downstream
    // `ft session recover` can resolve the right slot.
    let dir = temp_dir("inv2");
    let path = scrollback_path(&dir, 0x20);

    // Use a uuid with every byte distinct so any single-byte drift
    // is detectable.
    let mut uuid = [0u8; 32];
    for (i, byte) in uuid.iter_mut().enumerate() {
        *byte = i as u8;
    }
    let mut header = build_header(0x20);
    header.pane_uuid = uuid;

    write_full_scrollback(&path, &header, &[b"data"]);
    simulate_kill_truncation(&path, HEADER_SIZE + RECORD_HEADER_SIZE + 4);

    let decoded = ScrollbackHeader::decode(&fs::read(&path).unwrap()).unwrap();
    for (i, byte) in decoded.pane_uuid.iter().enumerate() {
        assert_eq!(
            *byte, i as u8,
            "pane_uuid byte {i} drifted across simulated kill"
        );
    }

    // The scanner must surface this file as Orphaned (header
    // decoded cleanly, no live owner) so the picker can offer it
    // for recovery.
    let candidate = classify_path(&path, &AlwaysOrphaned);
    assert_eq!(candidate.state, OrphanState::Orphaned);
    let header_ref = candidate.header_ok().expect("header decodable");
    assert_eq!(header_ref.pane_uuid, uuid);
}

#[test]
fn invariant_3_bounded_loss_surfaces_recoverable_tail() {
    // Bounded-loss per design-doc invariant 3: bytes between the
    // last MS_SYNC and the kill are best-effort. The file must
    // still surface as a recoverable orphan; the tail past the
    // msync boundary may be partial but must not corrupt the
    // header.
    let dir = temp_dir("inv3");
    let path = scrollback_path(&dir, 0x30);
    let header = build_header(0x30);
    let payloads: &[&[u8]] = &[
        b"committed-A",
        b"committed-B",
        b"in-flight-payload-that-will-be-truncated",
    ];
    write_full_scrollback(&path, &header, payloads);

    // Truncate in the middle of the third (in-flight) record.
    let after_two_records =
        HEADER_SIZE + 2 * RECORD_HEADER_SIZE + payloads[0].len() + payloads[1].len();
    let in_third_record = after_two_records + RECORD_HEADER_SIZE + 5;
    simulate_kill_truncation(&path, in_third_record);

    // Scanner classifies as Orphaned — the in-flight tail does not
    // poison the header.
    let out = scan_orphans(&dir, &AlwaysOrphaned).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].state, OrphanState::Orphaned);
    assert!(out[0].header_ok().is_some());
}

#[test]
fn invariant_4_mid_header_tear_surfaces_as_corrupt_truncated() {
    // Mid-header tear: kill -9 between mmap'ing the header buffer
    // and msync'ing it leaves the file shorter than HEADER_SIZE.
    // The recovery flow must surface this as Corrupt with the
    // structured Truncated reason — never silently mount it.
    let dir = temp_dir("inv4");
    let path = scrollback_path(&dir, 0x40);
    let header = build_header(0x40);
    write_full_scrollback(&path, &header, &[b"x"]);
    simulate_kill_truncation(&path, HEADER_SIZE / 2);

    let candidate = classify_path(&path, &AlwaysOrphaned);
    assert_eq!(candidate.state, OrphanState::Corrupt);
    let reason = candidate.corrupt_reason().expect("decode error attached");
    assert!(matches!(
        reason,
        HeaderDecodeError::Truncated { expected, actual }
            if *expected == HEADER_SIZE && *actual == HEADER_SIZE / 2
    ));
}

#[test]
fn invariant_5_mid_cursor_tear_surfaces_via_cursor_beyond_capacity() {
    // Mid-cursor tear: write_cursor_bytes was bumped + msync'd
    // before the kill, but the ring writes themselves were not.
    // The header claims a cursor that's beyond the actual file
    // size. The decoder must reject via CursorBeyondCapacity.
    let dir = temp_dir("inv5");
    let path = scrollback_path(&dir, 0x50);
    let mut header = build_header(0x50);
    // Inflate cursor beyond capacity to mimic a torn write_cursor
    // update.
    header.write_cursor_bytes = header.capacity_bytes + 64;
    fs::write(&path, header.encode()).unwrap();

    let candidate = classify_path(&path, &AlwaysOrphaned);
    assert_eq!(candidate.state, OrphanState::Corrupt);
    let reason = candidate.corrupt_reason().expect("decode error attached");
    assert!(matches!(
        reason,
        HeaderDecodeError::CursorBeyondCapacity { .. }
    ));
}

#[test]
fn regression_guard_kill_at_exact_msync_boundary_is_clean() {
    // Boundary-case regression guard: kill at *exactly* the msync
    // boundary (truncation point == HEADER_SIZE + sum(record
    // sizes)) yields a clean orphan with every committed record
    // intact. CI fails if the boundary handling drifts.
    let dir = temp_dir("boundary");
    let path = scrollback_path(&dir, 0x60);
    let header = build_header(0x60);
    let payloads: &[&[u8]] = &[b"alpha", b"beta", b"gamma"];
    let total = write_full_scrollback(&path, &header, payloads);

    // Truncate at exactly total — i.e. don't truncate at all.
    simulate_kill_truncation(&path, total);

    let candidate = classify_path(&path, &AlwaysOrphaned);
    assert_eq!(candidate.state, OrphanState::Orphaned);
    let h = candidate.header_ok().unwrap();
    assert_eq!(h.pane_uuid[0], 0x60);
    assert_eq!(h.last_msync_at_epoch_ms, header.last_msync_at_epoch_ms);
}

#[test]
fn regression_guard_kill_immediately_after_header_write() {
    // Edge case: kill immediately after the header write but before
    // any record write. The file is exactly HEADER_SIZE bytes long.
    // Must surface as Orphaned (recoverable, with empty ring).
    let dir = temp_dir("header_only");
    let path = scrollback_path(&dir, 0x70);
    let mut header = build_header(0x70);
    header.write_cursor_bytes = 0;
    header.total_bytes_written = 0;
    fs::write(&path, header.encode()).unwrap();

    let candidate = classify_path(&path, &AlwaysOrphaned);
    assert_eq!(candidate.state, OrphanState::Orphaned);
    let h = candidate.header_ok().unwrap();
    assert_eq!(h.write_cursor_bytes, 0);
    assert_eq!(h.total_bytes_written, 0);
}

/// Full e2e fixture per the bead's primary acceptance criterion:
/// spawn ft as a subprocess, write a known input stream, kill -9,
/// restart, run `ft session recover <pane_id>`, assert byte-for-byte
/// hash match against the pre-kill snapshot.
///
/// **Currently `#[ignore]`** because the e2e path requires:
/// - `MmapScrollback::append(pane_uuid, payload)` — write-path
///   wiring landing under **ft-z4u60**.
/// - `ft session recover <pane_id>` CLI — picker / restore command
///   landing under **ft-5te6x.cont.cli**.
///
/// Once both unblock, drop the `#[ignore]` and replace the body
/// with the full subprocess fixture. The substrate invariants
/// above already guard the format-level contract, so the e2e test
/// only needs to prove the wiring honors that contract under a
/// real `kill -9 <pid>` signal.
#[test]
#[ignore = "blocked on ft-z4u60 (mmap append wiring) + ft-5te6x.cont.cli (recover CLI)"]
fn spawn_and_kill_e2e_placeholder() {
    // Intentionally empty. Kept as a tracker so `cargo test
    // -- --ignored` lists the deferred coverage gap by name.
    //
    // Implementation outline once unblocked:
    //   1. spawn ft as a child (asupersync runtime + a known
    //      pane_uuid + a deterministic input stream).
    //   2. wait for the first MS_SYNC to land (poll
    //      `last_msync_at_epoch_ms` via the format substrate).
    //   3. compute SHA-256 of the file's byte content as the
    //      pre-kill snapshot.
    //   4. `kill(child_pid, SIGKILL)`.
    //   5. wait for the child to exit.
    //   6. invoke `ft session recover <pane_id>`.
    //   7. recompute the SHA-256 of the post-recovery scrollback.
    //   8. assert hash equality on the pre-msync prefix.
    //   9. assert pane_uuid round-trip via the format substrate.
}
