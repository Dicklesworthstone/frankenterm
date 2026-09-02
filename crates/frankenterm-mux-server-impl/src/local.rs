use anyhow::{Context as _, anyhow};
use config::{UnixDomain, create_user_owned_dirs};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;
use wezterm_uds::{UnixListener, UnixStream};

const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);
#[cfg(unix)]
const SOCKET_LEASE_RECORD_MAX_BYTES: u64 = 256;
#[cfg(unix)]
const STALE_SOCKET_QUARANTINE_DIRECTORY: &str = ".stale-frankenterm-sockets";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcceptErrorAction {
    ContinueAfter(Duration),
}

pub struct LocalListener {
    listener: UnixListener,
    dispatch_config: crate::dispatch::DispatchRuntimeConfig,
    _socket_lock: Option<File>,
}

impl LocalListener {
    pub fn new(
        listener: UnixListener,
        dispatch_config: crate::dispatch::DispatchRuntimeConfig,
    ) -> Self {
        Self {
            listener,
            dispatch_config,
            _socket_lock: None,
        }
    }

    pub fn with_domain(
        unix_dom: &UnixDomain,
        dispatch_config: crate::dispatch::DispatchRuntimeConfig,
    ) -> anyhow::Result<Self> {
        let (listener, socket_lock) = safely_create_sock_path(unix_dom)?;
        Ok(Self {
            listener,
            dispatch_config,
            _socket_lock: Some(socket_lock),
        })
    }

    fn accept_error_action(err: &std::io::Error) -> AcceptErrorAction {
        log::error!("accept failed: {}", err);
        AcceptErrorAction::ContinueAfter(ACCEPT_ERROR_BACKOFF)
    }

    pub fn run(&mut self) {
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    let dispatch_config = self.dispatch_config.clone();
                    match promise::spawn::try_reserve_main_thread(
                        promise::spawn::MainThreadServiceClass::Interactive,
                        4 * 1024,
                    ) {
                        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                            admit_connection(
                                reservation,
                                stream,
                                dispatch_config,
                                crate::dispatch::process_unix_auto_with_config,
                            )
                            .detach();
                        }
                        rejected => {
                            metrics::counter!(
                                "mux.server.local_accept_admission",
                                "outcome" => "terminal_rejection"
                            )
                            .increment(1);
                            log::error!(
                                "main-thread scheduler rejected local connection before dispatch construction: {rejected:?}"
                            );
                        }
                    }
                }
                Err(err) => match Self::accept_error_action(&err) {
                    AcceptErrorAction::ContinueAfter(backoff) => {
                        std::thread::sleep(backoff);
                    }
                },
            }
        }
    }
}

/// Take care when setting up the listener socket;
/// we need to be sure that the directory that we create it in
/// is owned by the user and has appropriate file permissions
/// that prevent other users from manipulating its contents.
/// Admit an accepted connection under a main-thread reservation and run
/// `session` for it as a main-thread-local future.
///
/// This is called from the listener thread, never the main thread.
/// `spawn_local` binds a runnable to the thread that creates it, so spawning
/// the session future directly from here makes the main-thread executor's
/// first poll panic ("local task polled by a thread that didn't spawn it"),
/// the unwind-time drop panics again, and the whole mux server aborts the
/// moment any client connects (observed 2026-09-02 with `ft list` against a
/// headless `frankenterm-mux-server`). The admission is therefore handed to
/// the main thread first and the local future is created there.
fn admit_connection<S, Fut>(
    reservation: promise::spawn::MainThreadSpawnReservation,
    stream: UnixStream,
    dispatch_config: crate::dispatch::DispatchRuntimeConfig,
    session: S,
) -> promise::spawn::MainThreadSpawnedTask<()>
where
    S: FnOnce(UnixStream, crate::dispatch::DispatchRuntimeConfig) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + 'static,
{
    reservation.handoff_to_main_thread_local(move |reservation| {
        reservation
            .spawn_local(async move {
                if let Err(error) = session(stream, dispatch_config).await {
                    log::error!("{error:#}");
                }
            })
            .detach();
    })
}

fn safely_create_sock_path(unix_dom: &UnixDomain) -> anyhow::Result<(UnixListener, File)> {
    let sock_path = &unix_dom.socket_path();
    log::trace!("setting up {}", sock_path.display());

    let sock_dir = sock_path
        .parent()
        .ok_or_else(|| anyhow!("sock_path {} has no parent dir", sock_path.display()))?;

    create_user_owned_dirs(sock_dir)?;

    #[cfg(unix)]
    {
        use config::running_under_wsl;
        use std::os::unix::fs::PermissionsExt;

        if !running_under_wsl() && !unix_dom.skip_permissions_check {
            // Let's be sure that the ownership looks sane
            let meta = sock_dir.symlink_metadata()?;

            let permissions = meta.permissions();
            if (permissions.mode() & 0o22) != 0 {
                anyhow::bail!(
                    "The permissions for {} are insecure and currently \
                     allow other users to write to it (permissions={:?})",
                    sock_dir.display(),
                    permissions
                );
            }
        }
    }

    let mut socket_lock = acquire_socket_lock(sock_path)?;

    #[cfg(unix)]
    quarantine_existing_socket_under_lease(sock_path, &socket_lock)?;
    #[cfg(not(unix))]
    if sock_path.symlink_metadata().is_ok() {
        anyhow::bail!(
            "refusing to overwrite an existing socket authority without a Unix lease: {}",
            sock_path.display()
        );
    }

    let listener = UnixListener::bind(sock_path)
        .with_context(|| format!("Failed to bind to {}", sock_path.display()))?;

    config::set_sticky_bit(sock_path);
    #[cfg(unix)]
    publish_socket_lease_record(sock_path, &mut socket_lock)?;

    Ok((listener, socket_lock))
}

fn socket_lock_path(sock_path: &Path) -> PathBuf {
    let mut path = sock_path.as_os_str().to_os_string();
    path.push(".lock");
    PathBuf::from(path)
}

fn acquire_socket_lock(sock_path: &Path) -> anyhow::Result<File> {
    let lock_path = socket_lock_path(sock_path);
    let mut options = OpenOptions::new();
    options.create(true).read(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let lock_file = options
        .open(&lock_path)
        .with_context(|| format!("opening socket lock {}", lock_path.display()))?;

    #[cfg(unix)]
    validate_socket_lock_identity(sock_path, &lock_path, &lock_file, false)?;

    lock_file
        .try_lock_exclusive()
        .with_context(|| format!("locking socket lock {}", lock_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        // Pre-lease FrankenTerm builds created otherwise valid lock files with
        // the process umask (commonly 0644). Normalize only after structural
        // validation and exclusive acquisition, and do it through the pinned
        // descriptor so no pathname alias can be chmodded accidentally.
        if lock_file.metadata()?.permissions().mode() & 0o777 != 0o600 {
            lock_file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            lock_file.sync_all()?;
        }
        validate_socket_lock_identity(sock_path, &lock_path, &lock_file, true)?;
    }

    Ok(lock_file)
}

#[cfg(unix)]
fn canonical_socket_lease_record(
    socket_metadata: &std::fs::Metadata,
    publisher_pid: u32,
) -> String {
    use std::os::unix::fs::MetadataExt as _;

    format!(
        "FT_SOCKET_LEASE_V1 pid={publisher_pid} dev={} ino={} ctime={} ctime_nsec={}\n",
        socket_metadata.dev(),
        socket_metadata.ino(),
        socket_metadata.ctime(),
        socket_metadata.ctime_nsec(),
    )
}

#[cfg(unix)]
fn process_is_proven_absent(pid: u32) -> bool {
    let Ok(raw_pid) = i32::try_from(pid) else {
        return false;
    };
    let Some(pid) = rustix::process::Pid::from_raw(raw_pid) else {
        return false;
    };
    matches!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    )
}

#[cfg(unix)]
fn socket_object_matches(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

    left.file_type().is_socket()
        && right.file_type().is_socket()
        && left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.uid() == right.uid()
        && left.nlink() == 1
        && right.nlink() == 1
}

#[cfg(unix)]
fn socket_identity_matches(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    socket_object_matches(left, right)
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(unix)]
fn socket_lease_record_pid(socket_metadata: &std::fs::Metadata, lock_file: &File) -> Option<u32> {
    use std::os::unix::fs::FileExt as _;

    let record_len = lock_file.metadata().ok()?.len();
    if record_len == 0 || record_len > SOCKET_LEASE_RECORD_MAX_BYTES {
        return None;
    }
    let mut observed = vec![0_u8; usize::try_from(record_len).ok()?];
    lock_file.read_exact_at(&mut observed, 0).ok()?;
    let observed = std::str::from_utf8(&observed).ok()?;
    let pid = observed
        .strip_prefix("FT_SOCKET_LEASE_V1 pid=")?
        .split_once(' ')?
        .0;
    if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let parsed = pid.parse::<u32>().ok()?;
    if parsed == 0 || parsed.to_string() != pid {
        return None;
    }
    (observed == canonical_socket_lease_record(socket_metadata, parsed)).then_some(parsed)
}

#[cfg(unix)]
fn create_stale_socket_quarantine_slot(
    sock_path: &Path,
    socket_metadata: &std::fs::Metadata,
) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let parent = sock_path
        .parent()
        .ok_or_else(|| anyhow!("socket path {} has no parent", sock_path.display()))?;
    let quarantine_root = parent.join(STALE_SOCKET_QUARANTINE_DIRECTORY);
    create_user_owned_dirs(&quarantine_root)?;
    for attempt in 0..32_u8 {
        let slot = quarantine_root.join(format!(
            "dev-{}.ino-{}.attempt-{attempt}",
            socket_metadata.dev(),
            socket_metadata.ino()
        ));
        match std::fs::create_dir(&slot) {
            Ok(()) => {
                std::fs::set_permissions(&slot, std::fs::Permissions::from_mode(0o700))?;
                let metadata = slot.symlink_metadata()?;
                if !metadata.is_dir()
                    || metadata.uid() != rustix::process::geteuid().as_raw()
                    || metadata.dev() != socket_metadata.dev()
                    || metadata.mode() & 0o7777 != 0o700
                {
                    anyhow::bail!("stale socket quarantine slot failed owner validation");
                }
                return Ok(slot);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("stale socket quarantine slot attempts are exhausted")
}

#[cfg(unix)]
fn quarantine_existing_socket_under_lease(
    sock_path: &Path,
    lock_file: &File,
) -> anyhow::Result<()> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};

    let initial = match sock_path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("inspecting existing socket authority"),
    };
    let parent = sock_path
        .parent()
        .ok_or_else(|| anyhow!("socket path {} has no parent", sock_path.display()))?;
    let parent_metadata = parent.symlink_metadata()?;
    let expected_uid = rustix::process::geteuid().as_raw();
    if !initial.file_type().is_socket()
        || initial.uid() != expected_uid
        || initial.dev() != parent_metadata.dev()
        || initial.nlink() != 1
        || !parent_metadata.is_dir()
        || parent_metadata.uid() != expected_uid
        || parent_metadata.permissions().mode() & 0o7777 != 0o700
    {
        anyhow::bail!(
            "refusing to overwrite a socket path that is not one direct owner-controlled socket: {}",
            sock_path.display()
        );
    }
    let publisher_pid = socket_lease_record_pid(&initial, lock_file).ok_or_else(|| {
        anyhow!(
            "refusing to replace existing socket without an exact versioned lease: {}",
            sock_path.display()
        )
    })?;
    if !process_is_proven_absent(publisher_pid) {
        anyhow::bail!(
            "refusing to replace socket while its recorded publisher is not proven absent: {}",
            sock_path.display()
        );
    }

    let slot = create_stale_socket_quarantine_slot(sock_path, &initial)?;
    let revalidated = sock_path.symlink_metadata()?;
    if !socket_identity_matches(&initial, &revalidated)
        || socket_lease_record_pid(&initial, lock_file) != Some(publisher_pid)
        || !process_is_proven_absent(publisher_pid)
    {
        anyhow::bail!("existing socket authority changed before quarantine");
    }
    let quarantined = slot.join("socket");
    std::fs::rename(sock_path, &quarantined)
        .with_context(|| format!("quarantining stale socket {}", sock_path.display()))?;
    let moved = quarantined.symlink_metadata()?;
    // A successful rename updates ctime on Unix. At this point the stable
    // device/inode/owner/link identity, not the pre-rename lease ctime, proves
    // that the quarantined object is the exact socket we admitted above.
    if !socket_object_matches(&initial, &moved) {
        anyhow::bail!("stale socket identity changed during quarantine");
    }
    File::open(&slot)?.sync_all()?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn publish_socket_lease_record(sock_path: &Path, lock_file: &mut File) -> anyhow::Result<()> {
    use std::io::{Read as _, Seek as _, Write as _};
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

    let lock_path = socket_lock_path(sock_path);
    validate_socket_lock_identity(sock_path, &lock_path, lock_file, true)?;
    let before = sock_path
        .symlink_metadata()
        .with_context(|| format!("inspecting published socket {}", sock_path.display()))?;
    let parent = sock_path
        .parent()
        .ok_or_else(|| anyhow!("socket path {} has no parent", sock_path.display()))?
        .symlink_metadata()?;
    let expected_uid = rustix::process::geteuid().as_raw();
    anyhow::ensure!(
        before.file_type().is_socket()
            && before.uid() == expected_uid
            && before.dev() == parent.dev()
            && before.nlink() == 1,
        "published socket is not one direct owner-controlled socket: {}",
        sock_path.display()
    );
    let record = canonical_socket_lease_record(&before, std::process::id());
    anyhow::ensure!(
        record.len() as u64 <= SOCKET_LEASE_RECORD_MAX_BYTES,
        "socket lease record exceeds its fixed bound"
    );

    lock_file.set_len(0)?;
    lock_file.rewind()?;
    lock_file.write_all(record.as_bytes())?;
    lock_file.sync_all()?;
    validate_socket_lock_identity(sock_path, &lock_path, lock_file, true)?;

    let mut observed = String::new();
    lock_file.rewind()?;
    lock_file
        .take(SOCKET_LEASE_RECORD_MAX_BYTES + 1)
        .read_to_string(&mut observed)?;
    anyhow::ensure!(
        observed == record,
        "socket lease record did not persist exactly"
    );
    let after = sock_path
        .symlink_metadata()
        .with_context(|| format!("revalidating published socket {}", sock_path.display()))?;
    anyhow::ensure!(
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec(),
        "published socket identity changed while sealing its lease record"
    );
    Ok(())
}

#[cfg(unix)]
fn validate_socket_lock_identity(
    sock_path: &Path,
    lock_path: &Path,
    lock_file: &File,
    require_private_mode: bool,
) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let socket_directory = sock_path
        .parent()
        .ok_or_else(|| anyhow!("socket path {} has no parent", sock_path.display()))?;
    let directory_metadata = socket_directory
        .symlink_metadata()
        .with_context(|| format!("inspecting socket directory {}", socket_directory.display()))?;
    let open_metadata = lock_file
        .metadata()
        .with_context(|| format!("inspecting open socket lock {}", lock_path.display()))?;
    let named_metadata = lock_path
        .symlink_metadata()
        .with_context(|| format!("revalidating socket lock {}", lock_path.display()))?;
    let expected_uid = rustix::process::geteuid().as_raw();

    if !directory_metadata.is_dir()
        || directory_metadata.uid() != expected_uid
        || directory_metadata.permissions().mode() & 0o077 != 0
    {
        anyhow::bail!(
            "socket directory is not a private directory owned by uid {expected_uid}: {}",
            socket_directory.display()
        );
    }
    if !open_metadata.is_file()
        || !named_metadata.is_file()
        || open_metadata.uid() != expected_uid
        || named_metadata.uid() != expected_uid
        || open_metadata.dev() != directory_metadata.dev()
        || open_metadata.dev() != named_metadata.dev()
        || open_metadata.ino() != named_metadata.ino()
        || open_metadata.nlink() != 1
        || named_metadata.nlink() != 1
        || open_metadata.len() != named_metadata.len()
        || open_metadata.len() > SOCKET_LEASE_RECORD_MAX_BYTES
        || (require_private_mode
            && (open_metadata.permissions().mode() & 0o077 != 0
                || named_metadata.permissions().mode() & 0o077 != 0))
    {
        anyhow::bail!(
            "socket lock is not a private, direct, bounded single-link file owned by uid {expected_uid}: {}",
            lock_path.display()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Regression guard for the accept-loop abort: admitting a connection from
    /// a non-main thread must create the session future on the main thread.
    /// Under the old direct `spawn_local` the first `try_tick` below panicked
    /// inside async_task's thread check and the process aborted.
    #[cfg(unix)]
    #[test]
    fn accepted_connection_is_admitted_on_the_main_thread_from_the_listener_thread() {
        let _lock = crate::GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let exec = promise::spawn::SimpleExecutor::new();
        let (client, server) = UnixStream::pair().expect("socket pair");
        let reservation = match promise::spawn::try_reserve_main_thread(
            promise::spawn::MainThreadServiceClass::Interactive,
            4 * 1024,
        ) {
            promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
            other => panic!("expected a reserved admission, got {other:?}"),
        };
        let main_thread = std::thread::current().id();
        let session_thread = std::sync::Arc::new(std::sync::Mutex::new(None));
        let session_thread_in_task = std::sync::Arc::clone(&session_thread);

        std::thread::spawn(move || {
            assert_ne!(std::thread::current().id(), main_thread);
            admit_connection(
                reservation,
                server,
                crate::dispatch::DispatchRuntimeConfig::default(),
                move |_stream, _config| async move {
                    *session_thread_in_task.lock().unwrap() = Some(std::thread::current().id());
                    Ok(())
                },
            )
            .detach();
        })
        .join()
        .expect("listener-side admission must not panic");

        assert!(
            exec.try_tick().expect("bootstrap must be queued"),
            "the handoff bootstrap must be queued for the main thread"
        );
        assert!(
            exec.try_tick().expect("session future must be queued"),
            "the local session future must be queued after the bootstrap"
        );
        assert_eq!(
            *session_thread.lock().unwrap(),
            Some(main_thread),
            "the session future must run on the main thread"
        );
        drop(client);
    }

    #[cfg(unix)]
    fn proven_absent_test_pid() -> u32 {
        (1_000_000..1_001_000)
            .find(|raw| process_is_proven_absent(*raw))
            .expect("the test host must expose one absent PID in the reserved high range")
    }

    #[cfg(unix)]
    fn write_socket_lease_record_for_test(lock_file: &mut File, socket: &Path, publisher_pid: u32) {
        use std::io::{Seek as _, Write as _};

        let metadata = socket.symlink_metadata().expect("inspect fixture socket");
        let record = canonical_socket_lease_record(&metadata, publisher_pid);
        lock_file.set_len(0).expect("truncate fixture lease");
        lock_file.rewind().expect("rewind fixture lease");
        lock_file
            .write_all(record.as_bytes())
            .expect("write fixture lease");
        lock_file.sync_all().expect("sync fixture lease");
    }

    #[test]
    fn transient_accept_failure_keeps_listener_alive_after_backoff() {
        let err = std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "client disconnected during accept",
        );

        assert_eq!(
            LocalListener::accept_error_action(&err),
            AcceptErrorAction::ContinueAfter(ACCEPT_ERROR_BACKOFF)
        );
    }

    #[test]
    fn socket_lock_path_appends_lock_suffix_without_replacing_extension() {
        assert_eq!(
            socket_lock_path(Path::new("/tmp/ft.sock")),
            PathBuf::from("/tmp/ft.sock.lock")
        );
        assert_eq!(
            socket_lock_path(Path::new("/tmp/tmux-501/default")),
            PathBuf::from("/tmp/tmux-501/default.lock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn socket_lock_is_private_and_rejects_aliases() {
        use std::io::Write as _;
        use std::os::unix::fs::{
            MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _, symlink,
        };

        for unsafe_kind in ["symlink", "hardlink", "oversized"] {
            let runtime = tempfile::tempdir().expect("socket lock runtime");
            std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
                .expect("make socket lock runtime private");
            let socket = runtime.path().join("gui.sock");
            let lock = socket_lock_path(&socket);
            let target = runtime.path().join("target");
            let mut target_file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&target)
                .expect("create alias target");
            match unsafe_kind {
                "symlink" => symlink(&target, &lock).expect("create symlink lock"),
                "hardlink" => std::fs::hard_link(&target, &lock).expect("create hardlink lock"),
                "oversized" => {
                    target_file
                        .write_all(&vec![b'x'; SOCKET_LEASE_RECORD_MAX_BYTES as usize + 1])
                        .expect("write oversized target");
                    std::fs::rename(&target, &lock).expect("install nonempty lock");
                }
                _ => unreachable!(),
            }

            assert!(
                acquire_socket_lock(&socket).is_err(),
                "{unsafe_kind} lock must fail closed"
            );
        }

        let runtime = tempfile::tempdir().expect("valid socket lock runtime");
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
            .expect("make valid runtime private");
        let socket = runtime.path().join("gui.sock");
        let lock_file = acquire_socket_lock(&socket).expect("acquire valid lock");
        let lock = socket_lock_path(&socket);
        let metadata = lock_file.metadata().expect("inspect valid lock");
        assert_eq!(metadata.uid(), rustix::process::geteuid().as_raw());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(metadata.nlink(), 1);
        assert_eq!(metadata.len(), 0);
        assert_eq!(
            metadata.ino(),
            lock.symlink_metadata().expect("inspect named lock").ino()
        );

        let legacy_runtime = tempfile::tempdir().expect("legacy socket lock runtime");
        std::fs::set_permissions(
            legacy_runtime.path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("make legacy runtime private");
        let legacy_socket = legacy_runtime.path().join("gui.sock");
        let legacy_lock = socket_lock_path(&legacy_socket);
        let legacy_file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o644)
            .open(&legacy_lock)
            .expect("create legacy mode lock");
        drop(legacy_file);
        let normalized = acquire_socket_lock(&legacy_socket).expect("normalize legacy lock mode");
        assert_eq!(
            normalized
                .metadata()
                .expect("inspect normalized lock")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let recorded_runtime = tempfile::tempdir().expect("recorded socket runtime");
        std::fs::set_permissions(
            recorded_runtime.path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("make recorded runtime private");
        let recorded_socket = recorded_runtime.path().join("gui.sock");
        let mut recorded_lock =
            acquire_socket_lock(&recorded_socket).expect("acquire recorded socket lock");
        let _listener = UnixListener::bind(&recorded_socket).expect("bind recorded socket");
        publish_socket_lease_record(&recorded_socket, &mut recorded_lock)
            .expect("publish socket lease record");
        let socket_metadata = recorded_socket
            .symlink_metadata()
            .expect("inspect recorded socket");
        let expected = canonical_socket_lease_record(&socket_metadata, std::process::id());
        assert_eq!(
            std::fs::read_to_string(socket_lock_path(&recorded_socket)).unwrap(),
            expected
        );
    }

    #[cfg(unix)]
    #[test]
    fn exact_dead_socket_lease_is_quarantined_without_deletion() {
        use std::os::unix::fs::PermissionsExt as _;

        let runtime = tempfile::tempdir().expect("stale socket runtime");
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
            .expect("make stale socket runtime private");
        let socket = runtime.path().join("gui.sock");
        let pid = proven_absent_test_pid();
        let mut initial_lock = acquire_socket_lock(&socket).expect("acquire initial socket lease");
        let listener = UnixListener::bind(&socket).expect("bind stale socket fixture");
        write_socket_lease_record_for_test(&mut initial_lock, &socket, pid);
        drop(listener);
        drop(initial_lock);

        let replacement_lock = acquire_socket_lock(&socket).expect("reacquire stale socket lease");
        quarantine_existing_socket_under_lease(&socket, &replacement_lock)
            .expect("quarantine exact stale socket");
        assert!(!socket.exists());
        assert!(socket_lock_path(&socket).exists());
        let quarantine = runtime.path().join(STALE_SOCKET_QUARANTINE_DIRECTORY);
        let slots = std::fs::read_dir(&quarantine)
            .expect("read stale socket quarantine")
            .collect::<Result<Vec<_>, _>>()
            .expect("read stale socket quarantine entries");
        assert_eq!(slots.len(), 1);
        assert!(slots[0].path().join("socket").exists());
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_or_live_socket_authority_is_never_replaced() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let foreign_runtime = tempfile::tempdir().expect("foreign socket runtime");
        std::fs::set_permissions(
            foreign_runtime.path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("make foreign runtime private");
        let foreign = foreign_runtime.path().join("gui.sock");
        let mut foreign_file = File::create(&foreign).expect("create foreign regular file");
        foreign_file
            .write_all(b"must remain")
            .expect("write foreign regular file");
        let foreign_lock = acquire_socket_lock(&foreign).expect("acquire foreign path lock");
        assert!(quarantine_existing_socket_under_lease(&foreign, &foreign_lock).is_err());
        assert_eq!(std::fs::read(&foreign).unwrap(), b"must remain");

        let live_runtime = tempfile::tempdir().expect("live publisher runtime");
        std::fs::set_permissions(live_runtime.path(), std::fs::Permissions::from_mode(0o700))
            .expect("make live publisher runtime private");
        let live_socket = live_runtime.path().join("gui.sock");
        let mut live_lock = acquire_socket_lock(&live_socket).expect("acquire live publisher lock");
        let live_listener = UnixListener::bind(&live_socket).expect("bind live publisher socket");
        write_socket_lease_record_for_test(&mut live_lock, &live_socket, std::process::id());
        drop(live_listener);
        drop(live_lock);
        let takeover_lock =
            acquire_socket_lock(&live_socket).expect("reacquire live publisher lock");
        assert!(quarantine_existing_socket_under_lease(&live_socket, &takeover_lock).is_err());
        assert!(live_socket.exists());
    }

    #[cfg(unix)]
    #[test]
    fn stale_lease_cannot_quarantine_a_rebound_socket() {
        use std::os::unix::fs::PermissionsExt as _;

        let runtime = tempfile::tempdir().expect("rebound socket runtime");
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
            .expect("make rebound socket runtime private");
        let socket = runtime.path().join("gui.sock");
        let pid = proven_absent_test_pid();
        let mut initial_lock = acquire_socket_lock(&socket).expect("acquire original socket lease");
        let original_listener = UnixListener::bind(&socket).expect("bind original socket");
        write_socket_lease_record_for_test(&mut initial_lock, &socket, pid);
        drop(original_listener);
        drop(initial_lock);
        let retained_original = runtime.path().join("retained-original-socket");
        std::fs::rename(&socket, &retained_original).expect("retain original socket");
        drop(UnixListener::bind(&socket).expect("bind replacement socket"));

        let replacement_lock = acquire_socket_lock(&socket).expect("acquire replacement lock");
        assert!(quarantine_existing_socket_under_lease(&socket, &replacement_lock).is_err());
        assert!(socket.exists());
        assert!(retained_original.exists());
    }
}
