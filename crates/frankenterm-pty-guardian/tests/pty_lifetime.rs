#![cfg(unix)]
#![forbid(unsafe_code)]

use frankenterm_pty_guardian::{
    GuardianClient, GuardianClientError, GuardianService, GuardianServiceConfig,
    GuardianServiceError,
};
use mux::guardian_protocol::{
    GUARDIAN_MAX_CENSUS_BYTES, GuardianCensusPageRequest, GuardianCensusPaneStatus,
    GuardianCheckpointIdentityDigest, GuardianRejectionCode, GuardianReplayRequestV1,
    GuardianReplaySelectorV1, GuardianReply,
};
use nix::sys::signal::kill;
use nix::unistd::Pid;
use portable_pty::{CommandBuilder, PtySize};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

struct ServiceThread {
    stop: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<Result<(), GuardianServiceError>>>,
}

impl Drop for ServiceThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.abort.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct ReleaseMarker(std::path::PathBuf);

impl Drop for ReleaseMarker {
    fn drop(&mut self) {
        if !self.0.exists() {
            let _ = write_new_file(&self.0, b"release-after-test-failure");
        }
    }
}

fn secure_test_temp_root() -> anyhow::Result<std::path::PathBuf> {
    // RCH and other build orchestrators may point TMPDIR inside a project or
    // cache tree whose writable ancestors intentionally fail the guardian's
    // no-follow filesystem policy. This Unix-only integration test needs the
    // platform sticky temporary root so it exercises the accepted production
    // path rather than weakening that policy for an unsafe harness override.
    Ok(std::fs::canonicalize("/tmp")?)
}

#[test]
fn empty_guarded_stop_returns_authenticated_success_before_service_exit() -> anyhow::Result<()> {
    let directory = tempfile::Builder::new()
        .prefix("frankenterm-pty-guardian-stop-")
        .tempdir_in(secure_test_temp_root()?)?
        .keep();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    let socket_path = directory.join("guardian.sock");
    let token_path = directory.join("guardian.token");
    write_new_file(&token_path, &[0x6b; 32])?;

    let config = GuardianServiceConfig::new(
        socket_path.clone(),
        token_path.clone(),
        4,
        4,
        64 * 1024,
        256 * 1024,
        Duration::from_millis(10),
    )?;
    let mut service = GuardianService::bind(config)?;
    let stop = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let service_stop = Arc::clone(&stop);
    let service_abort = Arc::clone(&abort);
    let handle =
        thread::spawn(move || service.run_until_with_test_abort(&service_stop, &service_abort));
    let mut service_thread = ServiceThread {
        stop,
        abort,
        handle: Some(handle),
    };
    let mut client = GuardianClient::connect(&socket_path, &token_path, Uuid::new_v4())?;
    client.guarded_stop(Uuid::new_v4(), Uuid::new_v4())?;
    anyhow::ensure!(
        wait_until(Duration::from_secs(3), || service_thread
            .handle
            .as_ref()
            .is_some_and(|handle| handle.is_finished())),
        "guardian did not exit after flushing its guarded-stop success response"
    );
    service_thread
        .handle
        .take()
        .expect("service thread handle remains present")
        .join()
        .map_err(|_| anyhow::anyhow!("guardian service thread panicked"))??;
    anyhow::ensure!(
        socket_path.exists(),
        "guardian must not unlink its socket during guarded stop"
    );
    Ok(())
}

#[test]
fn replay_resume_wait_wakes_on_durable_output_and_expires_at_its_deadline() -> anyhow::Result<()> {
    let directory = tempfile::Builder::new()
        .prefix("frankenterm-pty-guardian-replay-wait-")
        .tempdir_in(secure_test_temp_root()?)?
        .keep();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    let socket_path = directory.join("guardian.sock");
    let token_path = directory.join("guardian.token");
    let emit_path = directory.join("emit.marker");
    let release_path = directory.join("release.marker");
    write_new_file(&token_path, &[0x69; 32])?;

    let config = GuardianServiceConfig::new(
        socket_path.clone(),
        token_path.clone(),
        4,
        4,
        64 * 1024,
        256 * 1024,
        Duration::from_millis(5),
    )?;
    let mut service = GuardianService::bind(config)?;
    let stop = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let service_stop = Arc::clone(&stop);
    let service_abort = Arc::clone(&abort);
    let handle =
        thread::spawn(move || service.run_until_with_test_abort(&service_stop, &service_abort));
    let mut service_thread = ServiceThread {
        stop,
        abort,
        handle: Some(handle),
    };
    let _release_on_failure = ReleaseMarker(release_path.clone());

    let pane_id = Uuid::new_v4();
    let mut client = GuardianClient::connect(&socket_path, &token_path, Uuid::new_v4())?;
    let mut command = CommandBuilder::new("/bin/sh");
    command.arg("-c");
    command.arg(
        "while [ ! -e \"$FT_GUARDIAN_EMIT_FILE\" ]; do sleep 0.01; done; \
         printf wake; \
         while [ ! -e \"$FT_GUARDIAN_RELEASE_FILE\" ]; do sleep 0.01; done",
    );
    command.env("FT_GUARDIAN_EMIT_FILE", &emit_path);
    command.env("FT_GUARDIAN_RELEASE_FILE", &release_path);
    anyhow::ensure!(
        matches!(
            client.spawn(
                pane_id,
                Uuid::new_v4(),
                Uuid::new_v4(),
                command,
                PtySize::default(),
            )?,
            GuardianReply::Spawned {
                pane_id: spawned,
                generation: 0
            } if spawned == pane_id
        ),
        "guardian returned an unexpected replay-wait spawn receipt"
    );
    anyhow::ensure!(
        matches!(
            client.claim(pane_id, 0, Uuid::new_v4(), Uuid::new_v4())?,
            GuardianReply::Claimed {
                pane_id: claimed,
                generation: 1,
                next_sequence: 1
            } if claimed == pane_id
        ),
        "guardian returned an unexpected replay-wait claim receipt"
    );
    // Census `next_sequence` is the control-plane mutation sequence, not the
    // encrypted output-journal sequence. Pin the exact pane segment length so
    // the wake assertion below observes the durable data-plane boundary.
    let output_directory = directory.join("guardian-output-v3");
    anyhow::ensure!(
        wait_until(Duration::from_secs(2), || {
            pane_output_segment_size(&output_directory, pane_id).is_some()
        }),
        "guardian did not publish the pane output segment before replay"
    );
    let initial_segment_bytes = pane_output_segment_size(&output_directory, pane_id)
        .ok_or_else(|| anyhow::anyhow!("guardian pane output segment disappeared"))?;

    let checkpoint_id = GuardianCheckpointIdentityDigest::from_bytes([0x6a; 32])?;
    let emit = thread::spawn({
        let emit_path = emit_path.clone();
        move || {
            thread::sleep(Duration::from_millis(75));
            write_new_file(&emit_path, b"emit")
        }
    });
    let wake_started = Instant::now();
    let wake_result = client.replay(
        pane_id,
        1,
        Uuid::new_v4(),
        GuardianReplayRequestV1::Open {
            selector: GuardianReplaySelectorV1::Resume {
                checkpoint_id,
                next_sequence: 1,
                previous_record_digest: [0; 32],
            },
            max_plaintext_bytes: 4_096,
            max_records: 16,
            wait_millis: 1_000,
        },
    );
    let wake_elapsed = wake_started.elapsed();
    emit.join()
        .map_err(|_| anyhow::anyhow!("replay emit helper panicked"))??;
    anyhow::ensure!(
        matches!(
            wake_result,
            Err(GuardianClientError::Rejected(
                GuardianRejectionCode::InvalidRequest
            ))
        ),
        "the synthetic checkpoint must fail only after the durable-output wake"
    );
    anyhow::ensure!(
        wake_elapsed >= Duration::from_millis(25) && wake_elapsed < Duration::from_millis(900),
        "durable output did not release the server-held replay before its deadline: {wake_elapsed:?}"
    );
    anyhow::ensure!(
        wait_until(Duration::from_secs(2), || {
            pane_output_segment_size(&output_directory, pane_id)
                .is_some_and(|bytes| bytes > initial_segment_bytes)
        }),
        "the replay wake was not backed by a durable output-journal append"
    );

    let deadline_started = Instant::now();
    let deadline_result = client.replay(
        pane_id,
        1,
        Uuid::new_v4(),
        GuardianReplayRequestV1::Open {
            selector: GuardianReplaySelectorV1::Resume {
                checkpoint_id,
                next_sequence: 2,
                previous_record_digest: [0x6b; 32],
            },
            max_plaintext_bytes: 4_096,
            max_records: 16,
            wait_millis: 200,
        },
    );
    let deadline_elapsed = deadline_started.elapsed();
    anyhow::ensure!(
        matches!(
            deadline_result,
            Err(GuardianClientError::Rejected(
                GuardianRejectionCode::InvalidRequest
            ))
        ),
        "the synthetic checkpoint must reach its canonical rejection at the wait deadline"
    );
    anyhow::ensure!(
        deadline_elapsed >= Duration::from_millis(150) && deadline_elapsed < Duration::from_secs(2),
        "replay deadline was not bounded by the authenticated wait: {deadline_elapsed:?}"
    );

    write_new_file(&release_path, b"release")?;
    anyhow::ensure!(
        wait_until(Duration::from_secs(3), || {
            census_status(&mut client, pane_id).is_some_and(|(status, generation)| {
                status == GuardianCensusPaneStatus::ExitedUnclaimed && generation == 1
            })
        }),
        "replay-wait child did not reach a terminal unclaimed state"
    );
    anyhow::ensure!(
        matches!(
            client.close(pane_id, 1, 0, Uuid::new_v4(), Uuid::new_v4())?,
            GuardianReply::MutationApplied {
                pane_id: closed,
                generation: 1,
                sequence: 0
            } if closed == pane_id
        ),
        "guardian returned an unexpected replay-wait close receipt"
    );
    anyhow::ensure!(
        wait_until(Duration::from_secs(3), || client
            .guarded_stop(Uuid::new_v4(), Uuid::new_v4())
            .is_ok()),
        "replay-wait pane resources were not reclaimed for guarded stop"
    );
    service_thread
        .handle
        .take()
        .expect("service thread handle remains present")
        .join()
        .map_err(|_| anyhow::anyhow!("guardian service thread panicked"))??;
    Ok(())
}

#[test]
fn guardian_owned_native_pty_survives_final_mux_connection_drop() -> anyhow::Result<()> {
    // Keep the directory rather than deleting it: repository policy forbids
    // agents and their test helpers from deleting files without permission.
    let directory = tempfile::Builder::new()
        .prefix("frankenterm-pty-guardian-lifetime-")
        .tempdir_in(secure_test_temp_root()?)?
        .keep();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    let socket_path = directory.join("guardian.sock");
    let token_path = directory.join("guardian.token");
    let pid_path = directory.join("child.pid");
    let start_path = directory.join("start-after-disconnect.marker");
    let survived_path = directory.join("survived.marker");
    let release_path = directory.join("release.marker");
    write_new_file(&token_path, &[0x5a; 32])?;

    let config = GuardianServiceConfig::new(
        socket_path.clone(),
        token_path.clone(),
        8,
        8,
        64 * 1024,
        512 * 1024,
        Duration::from_millis(10),
    )?;
    let mut service = GuardianService::bind(config)?;
    let stop = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let service_stop = Arc::clone(&stop);
    let service_abort = Arc::clone(&abort);
    let handle =
        thread::spawn(move || service.run_until_with_test_abort(&service_stop, &service_abort));
    let mut service_thread = ServiceThread {
        stop,
        abort,
        handle: Some(handle),
    };
    let _release_on_failure = ReleaseMarker(release_path.clone());

    let pane_id = Uuid::new_v4();
    let mut first_client = GuardianClient::connect(&socket_path, &token_path, Uuid::new_v4())?;
    let guardian_incarnation = first_client.guardian_incarnation();
    let mut command = CommandBuilder::new("/bin/sh");
    command.arg("-c");
    command.arg(
        "printf '%s\\n' \"$$\" > \"$FT_GUARDIAN_PID_FILE\"; \
         while [ ! -e \"$FT_GUARDIAN_START_FILE\" ]; do sleep 0.05; done; \
         sleep 0.2; \
         printf survived > \"$FT_GUARDIAN_SURVIVED_FILE\"; \
         while [ ! -e \"$FT_GUARDIAN_RELEASE_FILE\" ]; do sleep 0.05; done",
    );
    command.env("FT_GUARDIAN_PID_FILE", &pid_path);
    command.env("FT_GUARDIAN_START_FILE", &start_path);
    command.env("FT_GUARDIAN_SURVIVED_FILE", &survived_path);
    command.env("FT_GUARDIAN_RELEASE_FILE", &release_path);

    let spawn = first_client.spawn(
        pane_id,
        Uuid::new_v4(),
        Uuid::new_v4(),
        command,
        PtySize::default(),
    )?;
    anyhow::ensure!(
        matches!(
            spawn,
            GuardianReply::Spawned {
                pane_id: spawned,
                generation: 0
            } if spawned == pane_id
        ),
        "guardian returned an unexpected spawn receipt"
    );
    let claim = first_client.claim(pane_id, 0, Uuid::new_v4(), Uuid::new_v4())?;
    anyhow::ensure!(
        matches!(
            claim,
            GuardianReply::Claimed {
                pane_id: claimed,
                generation: 1,
                next_sequence: 1
            } if claimed == pane_id
        ),
        "guardian returned an unexpected claim receipt"
    );
    anyhow::ensure!(
        wait_until(Duration::from_secs(3), || pid_path.is_file()),
        "PTY child never published its process identity"
    );
    let pid: i32 = std::fs::read_to_string(&pid_path)?.trim().parse()?;

    service_thread.stop.store(true, Ordering::Release);
    anyhow::ensure!(
        !wait_until(Duration::from_millis(250), || service_thread
            .handle
            .as_ref()
            .is_some_and(|handle| handle.is_finished())),
        "external test stop flag bypassed guarded shutdown while a PTY was owned"
    );
    service_thread.stop.store(false, Ordering::Release);

    // This is the mutation-sensitive boundary: the last authenticated
    // connection for the owning mux disappears while the real child is still
    // waiting. Only after that disconnect do we authorize the child to create
    // its survival marker. A connection-owned PTY would close here and the
    // marker below would never be written.
    drop(first_client);

    let mut successor = GuardianClient::connect(&socket_path, &token_path, Uuid::new_v4())?;
    anyhow::ensure!(
        successor.guardian_incarnation() == guardian_incarnation,
        "test reconnected to a different guardian incarnation"
    );
    let retired = wait_until(Duration::from_secs(3), || {
        census_status(&mut successor, pane_id).is_some_and(|(status, generation)| {
            status == GuardianCensusPaneStatus::LiveUnclaimed && generation == 1
        })
    });
    anyhow::ensure!(
        retired,
        "final connection removal did not retire the exact mux lease while retaining the pane"
    );
    let probe = successor.probe()?;
    anyhow::ensure!(
        probe.guardian_incarnation == guardian_incarnation && probe.pane_count == 1,
        "bounded authenticated probe did not traverse the guardian census"
    );
    anyhow::ensure!(
        matches!(
            successor.guarded_stop(Uuid::new_v4(), Uuid::new_v4()),
            Err(GuardianClientError::Rejected(
                GuardianRejectionCode::OwnedPanesPresent
            ))
        ),
        "guarded stop did not atomically refuse while the guardian owned a pane"
    );

    write_new_file(&start_path, b"start")?;
    anyhow::ensure!(
        wait_until(Duration::from_secs(4), || survived_path.is_file()),
        "child did not survive final mux/client disconnect"
    );
    kill(Pid::from_raw(pid), None::<nix::sys::signal::Signal>)?;

    write_new_file(&release_path, b"release")?;
    let exited = wait_until(Duration::from_secs(4), || {
        census_status(&mut successor, pane_id).is_some_and(|(status, generation)| {
            status == GuardianCensusPaneStatus::ExitedUnclaimed && generation == 1
        })
    });
    anyhow::ensure!(
        exited,
        "guardian did not observe and reap the released child"
    );

    let close = successor.close(pane_id, 1, 0, Uuid::new_v4(), Uuid::new_v4())?;
    anyhow::ensure!(
        matches!(
            close,
            GuardianReply::MutationApplied {
                pane_id: closed,
                generation: 1,
                sequence: 0
            } if closed == pane_id
        ),
        "guardian returned an unexpected terminal close receipt"
    );
    anyhow::ensure!(
        wait_until(Duration::from_secs(3), || successor
            .guarded_stop(Uuid::new_v4(), Uuid::new_v4())
            .is_ok()),
        "silent terminal pane resources were not reclaimed for guarded stop"
    );
    service_thread
        .handle
        .take()
        .expect("service thread handle remains present")
        .join()
        .map_err(|_| anyhow::anyhow!("guardian service thread panicked"))??;
    anyhow::ensure!(
        socket_path.exists(),
        "guardian must retain its socket after guarded stop"
    );
    Ok(())
}

fn census_status(
    client: &mut GuardianClient,
    pane_id: Uuid,
) -> Option<(GuardianCensusPaneStatus, u64)> {
    census_entry(client, pane_id).map(|entry| (entry.status, entry.generation))
}

fn census_entry(
    client: &mut GuardianClient,
    pane_id: Uuid,
) -> Option<mux::guardian_protocol::GuardianCensusEntry> {
    let page =
        GuardianCensusPageRequest::new(Uuid::nil(), 0, 16, GUARDIAN_MAX_CENSUS_BYTES).ok()?;
    let GuardianReply::CensusPage { entries, .. } = client.census(page).ok()? else {
        return None;
    };
    entries.into_iter().find(|entry| entry.pane_id == pane_id)
}

fn pane_output_segment_size(output_directory: &Path, pane_id: Uuid) -> Option<u64> {
    let prefix = format!("pane-{pane_id}.guardian-");
    let mut size = None;
    for entry in std::fs::read_dir(output_directory).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let name = name.to_str()?;
        if !name.starts_with(&prefix) || !name.ends_with(".ftgout") {
            continue;
        }
        if size.is_some() {
            return None;
        }
        let metadata = entry.metadata().ok()?;
        if !metadata.is_file() {
            return None;
        }
        size = Some(metadata.len());
    }
    size
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn write_new_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
