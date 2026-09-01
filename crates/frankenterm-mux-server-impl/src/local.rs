use anyhow::{Context as _, anyhow};
use config::{UnixDomain, create_user_owned_dirs};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;
use wezterm_uds::UnixListener;

const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

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
                            reservation
                                .spawn_local(async move {
                                    if let Err(error) =
                                        crate::dispatch::process_unix_auto_with_config(
                                            stream,
                                            dispatch_config,
                                        )
                                        .await
                                    {
                                        log::error!("{error:#}");
                                    }
                                })
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

    let socket_lock = acquire_socket_lock(sock_path)?;

    // We want to remove the socket if it exists.
    // However, on windows, we can't tell if the unix domain socket
    // exists using the methods on Path, so instead we just unconditionally
    // remove it and see what error occurs.
    match std::fs::remove_file(sock_path) {
        Ok(()) => {}
        Err(err) => match err.kind() {
            std::io::ErrorKind::NotFound => {}
            _ => return Err(err).context(format!("Unable to remove {}", sock_path.display())),
        },
    }

    let listener = UnixListener::bind(sock_path)
        .with_context(|| format!("Failed to bind to {}", sock_path.display()))?;

    config::set_sticky_bit(sock_path);

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
        || open_metadata.len() != 0
        || named_metadata.len() != 0
        || (require_private_mode
            && (open_metadata.permissions().mode() & 0o077 != 0
                || named_metadata.permissions().mode() & 0o077 != 0))
    {
        anyhow::bail!(
            "socket lock is not a private, direct, empty single-link file owned by uid {expected_uid}: {}",
            lock_path.display()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

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

        for unsafe_kind in ["symlink", "hardlink", "nonempty"] {
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
                "nonempty" => {
                    target_file.write_all(b"not a lease").expect("write target");
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
    }
}
