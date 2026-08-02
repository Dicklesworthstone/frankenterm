//! Source-level regression guard for bug #4 (the smol->asupersync blocking
//! class). `PaneWriter::write` is a sync `std::io::Write` impl invoked on the
//! GUI main-thread spawn queue; it must enqueue its mux RPC through the runtime
//! without synchronously waiting for the reply.
//!
//! This is intentionally a cheap static check that fails fast at test time if
//! anyone reverts the write path. The behavioural guards live in
//! `client::tests::main_thread_pane_write_round_trips_ft_connect_fix`.

use std::path::Path;

#[test]
fn pane_writer_write_is_nonblocking_and_spawns_the_rpc() {
    let src = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pane/clientpane.rs"),
    )
    .expect("read clientpane.rs source");
    let impl_start = src
        .find("impl std::io::Write for PaneWriter")
        .expect("locate PaneWriter Write implementation");
    let write_start = src[impl_start..]
        .find("fn write(")
        .map(|offset| impl_start + offset)
        .expect("locate PaneWriter::write");
    let flush_start = src[write_start..]
        .find("fn flush(")
        .map(|offset| write_start + offset)
        .expect("locate PaneWriter::flush after PaneWriter::write");
    let write_body = &src[write_start..flush_start];

    // Assertions must inspect executable source, not comments describing the
    // historical blocking implementation. The old guard searched the whole
    // file for a blocking symbol and therefore passed solely because that
    // symbol appeared in a comment next to the nonblocking implementation.
    let executable = write_body
        .lines()
        .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        executable.contains("write_to_pane(WriteToPane"),
        "PaneWriter::write must construct the remote pane-write RPC"
    );
    assert!(
        executable.contains("promise::spawn::spawn(request).detach()"),
        "PaneWriter::write must detach the constructed RPC onto the runtime"
    );
    for forbidden in ["block_on(", "block_on_io("] {
        assert!(
            !executable.contains(forbidden),
            "PaneWriter::write must never wait synchronously via {}",
            forbidden
        );
    }
}
