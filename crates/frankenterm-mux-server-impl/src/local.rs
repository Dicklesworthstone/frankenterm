use anyhow::{Context as _, anyhow};
use config::{UnixDomain, create_user_owned_dirs};
use fs2::FileExt;
use promise::spawn::spawn_into_main_thread;
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
                    let dispatch_config = self.dispatch_config;
                    spawn_into_main_thread(async move {
                        crate::dispatch::process_unix_auto_with_config(stream, dispatch_config)
                            .await
                            .map_err(|e| {
                                log::error!("{:#}", e);
                                e
                            })
                    })
                    .detach();
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
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening socket lock {}", lock_path.display()))?;

    lock_file
        .try_lock_exclusive()
        .with_context(|| format!("locking socket lock {}", lock_path.display()))?;

    Ok(lock_file)
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
}
