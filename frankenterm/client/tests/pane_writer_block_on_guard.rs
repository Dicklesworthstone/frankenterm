//! Source-level regression guard for bug #4 (the smol->asupersync block_on
//! class). `PaneWriter::write` is a sync `std::io::Write` impl invoked on the
//! GUI main-thread spawn queue; it MUST drive its mux RPC via `block_on_io`
//! (spawn-on-runtime + join, reactor-driven, main-thread-safe), never via the
//! bare `block_on` (whose main-thread guard panics and which does not drive the
//! reply).
//!
//! This is intentionally a cheap static check that fails fast at test time if
//! anyone reverts the write path. The behavioural guards live in
//! `client::tests::main_thread_pane_write_round_trips_ft_connect_fix` and
//! `promise::spawn::tests::block_on_io_is_safe_inside_dispatch_scope`.

use std::path::Path;

#[test]
fn pane_writer_write_uses_block_on_io_not_block_on() {
    let src = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pane/clientpane.rs"),
    )
    .expect("read clientpane.rs source");

    // The fixed write path must spawn the RPC onto the runtime.
    assert!(
        src.contains("block_on_io"),
        "PaneWriter::write must use promise::spawn::block_on_io so the mux RPC \
         is reactor-driven and main-thread-safe"
    );

    // The exact pre-fix pattern must never come back. `block_on` directly on a
    // mux RPC from this sync Write impl panics on the GUI main-thread dispatch.
    assert!(
        !src.contains("block_on(self.client.client.write_to_pane"),
        "PaneWriter::write regressed to bare block_on(write_to_pane); this panics \
         on the GUI main-thread spawn queue — use block_on_io"
    );
}
