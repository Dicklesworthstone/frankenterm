#![cfg(unix)]

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use proptest::prelude::*;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone)]
struct SpawnRaceCase {
    clone_reader_before_spawn: bool,
    take_writer_before_spawn: bool,
    drop_slave_before_reader_start: bool,
    write_before_reader_start: bool,
    start_wait_before_reader_join: bool,
    probe_try_wait_before_io: bool,
    controlling_tty: bool,
    rows: u16,
    cols: u16,
    payload: String,
}

fn arb_payload() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            (b'a'..=b'z').prop_map(char::from),
            (b'A'..=b'Z').prop_map(char::from),
            (b'0'..=b'9').prop_map(char::from),
            Just('_'),
            Just('-'),
            Just('.'),
        ],
        1..32,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

fn arb_spawn_race_case() -> impl Strategy<Value = SpawnRaceCase> {
    (
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        2u16..=40,
        8u16..=160,
        arb_payload(),
    )
        .prop_map(
            |(
                clone_reader_before_spawn,
                take_writer_before_spawn,
                drop_slave_before_reader_start,
                write_before_reader_start,
                start_wait_before_reader_join,
                probe_try_wait_before_io,
                controlling_tty,
                rows,
                cols,
                payload,
            )| SpawnRaceCase {
                clone_reader_before_spawn,
                take_writer_before_spawn,
                drop_slave_before_reader_start,
                write_before_reader_start,
                start_wait_before_reader_join,
                probe_try_wait_before_io,
                controlling_tty,
                rows,
                cols,
                payload,
            },
        )
}

fn start_waiter(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
) -> (
    Box<dyn portable_pty::ChildKiller + Send + Sync>,
    mpsc::Receiver<std::io::Result<portable_pty::ExitStatus>>,
) {
    let killer = child.clone_killer();
    let (status_tx, status_rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = status_tx.send(child.wait());
    });
    (killer, status_rx)
}

fn recv_child_status(
    killer: &mut dyn portable_pty::ChildKiller,
    status_rx: &mpsc::Receiver<std::io::Result<portable_pty::ExitStatus>>,
    case: &SpawnRaceCase,
) -> anyhow::Result<portable_pty::ExitStatus> {
    match status_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(status) => Ok(status?),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = killer.kill();
            anyhow::bail!("timed out waiting for child exit in {case:?}");
        }
        Err(err) => anyhow::bail!("child waiter disconnected in {case:?}: {err}"),
    }
}

fn recv_output(
    output_rx: &mpsc::Receiver<std::io::Result<String>>,
    case: &SpawnRaceCase,
) -> anyhow::Result<String> {
    match output_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(output) => Ok(output?),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            anyhow::bail!("timed out waiting for pty reader EOF in {case:?}");
        }
        Err(err) => anyhow::bail!("pty reader disconnected in {case:?}: {err}"),
    }
}

fn normalize_pty_eof_echo(output: &str) -> String {
    output.replace("\r\n^D\u{8}\u{8}", "")
}

fn run_spawn_race_case(case: &SpawnRaceCase) -> anyhow::Result<(String, portable_pty::ExitStatus)> {
    let pty_system = NativePtySystem::default();
    let pair = pty_system.openpty(PtySize {
        rows: case.rows,
        cols: case.cols,
        pixel_width: case.cols.saturating_mul(8),
        pixel_height: case.rows.saturating_mul(16),
    })?;
    let master = pair.master;
    let mut slave = Some(pair.slave);

    let mut reader = if case.clone_reader_before_spawn {
        Some(master.try_clone_reader()?)
    } else {
        None
    };
    let mut writer = if case.take_writer_before_spawn {
        Some(master.take_writer()?)
    } else {
        None
    };

    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.arg("-c");
    cmd.arg("printf '%s\\n' \"$1\"");
    cmd.arg("ft-spawn-race");
    cmd.arg(&case.payload);
    cmd.set_controlling_tty(case.controlling_tty);

    let mut child = slave
        .as_ref()
        .expect("slave is available before spawn")
        .spawn_command(cmd)?;
    if case.probe_try_wait_before_io {
        let _ = child.try_wait()?;
    }

    let wait_before_reader_join = case.start_wait_before_reader_join;
    let mut deferred_child = Some(child);
    let mut wait_state = wait_before_reader_join.then(|| {
        let child = deferred_child
            .take()
            .expect("child must be available for early wait");
        start_waiter(child)
    });

    if reader.is_none() {
        reader = Some(master.try_clone_reader()?);
    }
    if writer.is_none() {
        writer = Some(master.take_writer()?);
    }

    if case.write_before_reader_start {
        drop(writer.take().expect("writer is available before reader"));
    }

    if case.drop_slave_before_reader_start {
        drop(slave.take());
    }

    let mut reader = reader.expect("reader is available before reader thread");
    let (output_tx, output_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut output = String::new();
        let result = reader.read_to_string(&mut output).map(|_| output);
        let _ = output_tx.send(result);
    });

    if let Some(mut writer) = writer.take() {
        writer.flush()?;
        drop(writer);
    }

    if !case.drop_slave_before_reader_start {
        drop(slave.take());
    }

    if wait_state.is_none() {
        let child = deferred_child
            .take()
            .expect("child must be available for late wait");
        wait_state = Some(start_waiter(child));
    }
    let (mut killer, status_rx) = wait_state.expect("waiter must be started");

    let (output, status) = if wait_before_reader_join {
        let status = recv_child_status(&mut *killer, &status_rx, case)?;
        let output = recv_output(&output_rx, case)?;
        (output, status)
    } else {
        let output = recv_output(&output_rx, case)?;
        let status = recv_child_status(&mut *killer, &status_rx, case)?;
        (output, status)
    };

    drop(master);
    Ok((output, status))
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 48,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn short_lived_subprocess_spawn_is_race_invariant(case in arb_spawn_race_case()) {
        let expected_line = format!("{}\r\n", case.payload);
        let (output, status) = run_spawn_race_case(&case)
            .map_err(|err| TestCaseError::fail(format!("{err:#}")))?;

        prop_assert!(
            status.success(),
            "child exited unsuccessfully for {:?}: {}",
            case,
            status
        );
        let normalized_output = normalize_pty_eof_echo(&output);
        prop_assert_eq!(
            normalized_output,
            expected_line,
            "pty spawn/read/write/drop interleaving changed subprocess output for {:?}",
            case
        );
    }
}
