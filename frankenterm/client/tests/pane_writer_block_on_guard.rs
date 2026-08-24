//! Source-level regression guard for bug #4 (the smol->asupersync blocking
//! class). `PaneWriter::write` is a sync `std::io::Write` impl invoked on the
//! GUI main-thread spawn queue; it must transfer bounded ownership to the
//! shared reliable-input FIFO without constructing or awaiting an RPC there.
//!
//! This is intentionally a cheap static check that fails fast at test time if
//! anyone reverts the write path. The behavioural guards live in the client-
//! pane and mux-server modules.

use std::path::Path;

#[test]
fn pane_writer_write_is_nonblocking_and_uses_the_shared_reliable_fifo() {
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
        executable.contains(".reliable_input_queue"),
        "PaneWriter::write must enter the ClientInner shared reliable-input lane"
    );
    assert!(
        executable.contains(".enqueue_pane_write("),
        "PaneWriter::write must transfer ownership through bounded pane-write admission"
    );
    for forbidden in [
        "write_to_pane(",
        "WriteToPane",
        "dispatch_interactive_rpc(",
        "block_on(",
        "block_on_io(",
    ] {
        assert!(
            !executable.contains(forbidden),
            "PaneWriter::write must not retain legacy or blocking path {}",
            forbidden
        );
    }

    let impl_end = src[flush_start..]
        .find("\n}\n\n#[cfg(test)]")
        .map(|offset| flush_start + offset)
        .expect("locate the end of PaneWriter's Write implementation");
    let flush_executable = src[flush_start..impl_end]
        .lines()
        .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        flush_executable.contains(".flush_pending()"),
        "PaneWriter::flush must observe pending ownership and sticky failure under one lock"
    );
    for torn_snapshot in [".sticky_failure()", ".pending_chunks()"] {
        assert!(
            !flush_executable.contains(torn_snapshot),
            "PaneWriter::flush must not reconstruct delivery state from separate {} reads",
            torn_snapshot
        );
    }

    let enqueue_start = src
        .find("fn enqueue_pane_write(")
        .expect("locate bounded pane-write admission");
    let worker_start = src[enqueue_start..]
        .find("fn start_worker_now(")
        .map(|offset| enqueue_start + offset)
        .expect("locate reliable-input worker after pane-write admission");
    let enqueue_executable = src[enqueue_start..worker_start]
        .lines()
        .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        enqueue_executable.contains("delivery.try_accept_chunk()"),
        "queue ownership transfer must linearize against sticky terminal delivery state"
    );

    let run_start = src[worker_start..]
        .find("async fn run(")
        .map(|offset| worker_start + offset)
        .expect("locate reliable-input worker loop");
    let attempt_start = src[run_start..]
        .find("async fn attempt(")
        .map(|offset| run_start + offset)
        .expect("locate reliable-input attempt dispatcher");
    let run_executable = src[run_start..attempt_start]
        .lines()
        .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        run_executable.contains("queue.fail_pane_write_stream(&entry, outcome, failure)"),
        "a terminal pane-write result must retire later chunks owned by that same writer"
    );
}
