# Durable scrollback crash and recovery suite, native macOS (ft-yccm0.2.1.7 AC4/AC5)

`transcript.txt` is the subset of the frankenterm-mux-server-impl lib suite that kills, tears,
reorders, flips bits in or recovers the durable scrollback store. It was run natively on
mac-studio and selected with `crash-transcript.sh`, whose filters match on test names.

**Host and source**
- mac-studio, macOS 27.0.1 arm64, APFS, default TMPDIR under `/var/folders`.
- Toolchain: rustc nightly 2026-08-30.
- Source: the code of 0460bffe4. That includes 2.1.3's digest-only WAL, 2.1.4's sealed segments,
  2.1.6's gap markers, and da2c97e4b's fix for the SIGKILL crash points on macOS.

**Result:** 66 passed, 0 failed. Two guardian_proxy recovery tests are ignored; they need a sealed
candidate identity and run under strict RCH/DSR only. The suite includes:
- the SIGKILL subprocess steps of the batch transaction, the window tail and the gap marker;
- `every_crash_state_inside_a_transaction_recovers_an_exact_committed_prefix`, the crash-state
  model: torn appends, and unit renames of the WAL and the manifest;
- torn and reordered segment tails;
- the digest-only WAL single-bit-flip fuzz.

**Fixed before this run.** The SIGKILL tests had failed 3 of 3 under load on macOS: the child
died of SIGABRT instead of SIGKILL. A SIGKILL sent to the process can take effect after `kill`
returns, and the immediate `abort()` won that race. Since da2c97e4b the crash point waits for
the kill.

**Load.** The full lib suite at the same source passed 806/0 three times in a row at load 230-253
(da2c97e4b proof). This transcript ran at load 154-175.
