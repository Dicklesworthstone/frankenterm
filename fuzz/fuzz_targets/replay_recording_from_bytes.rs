//! Fuzz harness for `Recording::from_bytes` (.war binary recording loader).
//!
//! ft-p2bjm. Oversize cap at 256 MiB was added in 3a0cac70; this harness
//! exercises the frame-header state machine itself (`parse_frame` at
//! `replay.rs:36`) under adversarial byte sequences below that cap.
//!
//! Attack surface: any command that opens a user-supplied recording path
//! (`ft replay load`, session restore, CI-fed fixtures). Oracle is
//! no-panic / no-allocation-explosion — libfuzzer handles timeouts for
//! infinite-loop detection.

#![no_main]

use libfuzzer_sys::fuzz_target;

use frankenterm_core::replay::Recording;

fuzz_target!(|data: &[u8]| {
    // Keep per-input cost bounded so we explore shape rather than burn
    // budget on 256 MiB allocs. Still well above the envelope-header
    // + multi-frame shapes the format defines.
    if data.len() > 8_000_000 {
        return;
    }
    let _ = Recording::from_bytes(data);
});
