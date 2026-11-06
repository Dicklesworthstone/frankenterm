# Durable scrollback crash suite and random crash schedules, native macOS (ft-yccm0.2.1.7 AC4)

`transcript.txt` holds two native runs on one source, both made by `crash-transcript.sh`:
- the full frankenterm-mux-server-impl lib suite;
- `random_crash_schedules_recover_an_exact_committed_prefix` with
  `FT_CRASH_SCHEDULE_HISTORIES=1000`.

**Host and source**
- yi-ap, macOS 27.0.1 arm64, APFS, default TMPDIR under `/var/folders`.
- Toolchain: rustc nightly 2026-08-30.
- Build: debug test build without debuginfo.
- Source: gate 033c75af8, which is 6742c1532 plus that commit's impl `lib.rs` and core
  `storage/mmap_store.rs`. The landed blobs differ from it only in one test binding, which was
  renamed for clippy.

**Result**
- Lib suite: 866 passed, 0 failed, 4 ignored, in 70.20 s, at load 15.
- Crash schedules: 1000 histories and 10749 crash states, all recovered, in 1334.64 s.

Each crash state must reopen to an exact in-order retained prefix that keeps every committed
row. It must also serve a snapshot, take the next row and reopen again. One test prints its ok
line twice because it re-runs itself in a child process.

**What the schedules found.** Earlier runs of the same schedules found five recovery bugs:
- a torn WAL stage cut durable window rows;
- a staged first-row manifest wedged the pane;
- a first row that never landed left the pane refusing rows;
- a zero-filled first row refused the store;
- a torn final record of the core sequence journal refused the pane.

5e4445d1b fixes all five, and this run is the native proof of those fixes.
