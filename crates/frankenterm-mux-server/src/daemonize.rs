#![cfg(unix)]
use anyhow::Context;
use libc::pid_t;
use std::io::Write;
use std::os::unix::io::{AsRawFd, IntoRawFd, RawFd};

enum Fork {
    #[allow(dead_code)]
    Child(pid_t),
    Parent(pid_t),
}

#[allow(unsafe_code)]
fn fork() -> anyhow::Result<Fork> {
    // SAFETY: `fork` is invoked before the daemonized child starts worker
    // threads. The parent immediately waits or exits; the child performs only
    // the small post-fork setup in this module before returning to the normal
    // startup path.
    let pid = unsafe { libc::fork() };

    if pid == 0 {
        // We are the child
        Ok(Fork::Child(current_pid()))
    } else if pid < 0 {
        let err: anyhow::Error = std::io::Error::last_os_error().into();
        Err(err.context("fork"))
    } else {
        // We are the parent
        Ok(Fork::Parent(pid))
    }
}

#[allow(unsafe_code)]
fn setsid() -> anyhow::Result<()> {
    // SAFETY: Called only in the first daemon child after fork and before the
    // second fork. `setsid` has no Rust aliasing requirements; errors are
    // checked through the POSIX `-1` return value.
    let pid = unsafe { libc::setsid() };
    if pid == -1 {
        let err: anyhow::Error = std::io::Error::last_os_error().into();
        Err(err.context("setsid"))
    } else {
        Ok(())
    }
}

#[allow(unsafe_code)]
fn lock_pid_file(config: &config::ConfigHandle) -> anyhow::Result<std::fs::File> {
    let pid_file = config.daemon_options.pid_file();
    let pid_file_dir = pid_file
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent?", pid_file.display()))?;
    std::fs::create_dir_all(&pid_file_dir).with_context(|| {
        format!(
            "while creating directory structure: {}",
            pid_file_dir.display()
        )
    })?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&pid_file)
        .with_context(|| format!("opening pid file {}", pid_file.display()))?;
    config::set_sticky_bit(&pid_file);
    // SAFETY: `file.as_raw_fd()` is a live descriptor owned by `file`, and
    // `flock` does not take ownership of it. The return value is checked.
    let res = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if res != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("unable to lock pid file {}: {}", pid_file.display(), err);
    }

    // SAFETY: `file.as_raw_fd()` is still live and opened for writing. Truncate
    // failure is intentionally ignored to preserve the previous best-effort
    // pid-file behavior; the subsequent write still reports I/O errors.
    let _ = unsafe { libc::ftruncate(file.as_raw_fd(), 0) };

    Ok(file)
}

#[allow(unsafe_code)]
fn wait_for_intermediate_child(pid: pid_t) -> ! {
    let mut status: libc::c_int = 0;
    // SAFETY: `pid` is the child pid returned by the immediately preceding
    // `fork` call in this process. `status` points to valid writable storage
    // for the duration of the call, and the return value is checked.
    let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
    if waited == -1 {
        let err = std::io::Error::last_os_error();
        eprintln!(
            "frankenterm-mux-server: waitpid({pid}) on daemonize \
             intermediate failed: {err}; exit status unknown"
        );
        std::process::exit(1);
    }
    if libc::WIFEXITED(status) {
        std::process::exit(libc::WEXITSTATUS(status));
    }
    if libc::WIFSIGNALED(status) {
        let sig = libc::WTERMSIG(status);
        eprintln!(
            "frankenterm-mux-server: daemonize intermediate child \
             killed by signal {sig}"
        );
        // Conventional encoding: 128 + signal number.
        std::process::exit(128 + sig);
    }
    std::process::exit(1);
}

#[allow(unsafe_code)]
fn current_pid() -> pid_t {
    // SAFETY: `getpid` takes no pointers and cannot violate Rust aliasing or
    // memory-safety invariants.
    unsafe { libc::getpid() }
}

#[allow(unsafe_code)]
fn redirect_standard_streams(
    devnull: &std::fs::File,
    stdout: &std::fs::File,
    stderr: &std::fs::File,
) {
    // SAFETY: All source descriptors are live for the duration of these calls.
    // `dup2` duplicates them onto the standard descriptor numbers without
    // taking ownership of the Rust `File` values.
    unsafe { libc::dup2(devnull.as_raw_fd(), libc::STDIN_FILENO) };
    unsafe { libc::dup2(stdout.as_raw_fd(), libc::STDOUT_FILENO) };
    unsafe { libc::dup2(stderr.as_raw_fd(), libc::STDERR_FILENO) };
}

pub fn daemonize(config: &config::ConfigHandle) -> anyhow::Result<Option<RawFd>> {
    let pid_file = if !config::running_under_wsl() {
        // pid file locking is only partly functional when running under
        // WSL 1; it is possible for the pid file to exist after a reboot
        // and for attempts to open and lock it to fail when there are no
        // other processes that might possibly hold a lock on it.
        // So, we only use a pid file when not under WSL.

        Some(lock_pid_file(config)?)
    } else {
        None
    };
    let stdout = config.daemon_options.open_stdout()?;
    let stderr = config.daemon_options.open_stderr()?;
    let devnull = std::fs::File::open("/dev/null").context("opening /dev/null for read")?;

    match fork()? {
        Fork::Parent(pid) => {
            // Propagate the intermediate child's exit status to the
            // invoking shell so a failure anywhere in the child's
            // post-fork setup (setsid, the second fork, pidfile write,
            // stdio redirection) surfaces as a non-zero exit on the
            // grandparent — not silent success.
            //
            // Earlier revisions ignored both the waitpid return value
            // and the status word: a failing intermediate child would
            // still see the grandparent exit(0), leaving the user
            // thinking "daemon started" when nothing was actually
            // running.
            wait_for_intermediate_child(pid);
        }
        Fork::Child(_) => {}
    }

    setsid()?;
    match fork()? {
        Fork::Parent(_) => {
            std::process::exit(0);
        }
        Fork::Child(_) => {}
    }

    let pid_file_fd = pid_file.map(|mut pid_file| {
        writeln!(pid_file, "{}", current_pid()).ok();
        // Leak it so that the descriptor remains open for the duration
        // of the process runtime
        let fd = pid_file.into_raw_fd();

        // Since we will always re-exec, we need to clear FD_CLOEXEC
        // in order for the pidfile to be inherited in our newly
        // exec'd self
        set_cloexec(fd, false);

        fd
    });

    redirect_standard_streams(&devnull, &stdout, &stderr);

    Ok(pid_file_fd)
}

#[allow(unsafe_code)]
pub fn set_cloexec(fd: RawFd, enable: bool) {
    // SAFETY: `fd` is expected to be an open file descriptor supplied by the
    // daemonization path. `fcntl` does not take ownership; failures are
    // best-effort and preserve the previous no-panic behavior.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags == -1 {
            return;
        }

        let flags = if enable {
            flags | libc::FD_CLOEXEC
        } else {
            flags & !libc::FD_CLOEXEC
        };

        libc::fcntl(fd, libc::F_SETFD, flags);
    }
}
