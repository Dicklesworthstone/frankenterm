use anyhow::Context;
use std::ffi::OsString;
use std::process::{Command, Stdio};

pub fn spawn_daemonized_copy(
    args: Vec<OsString>,
    config: &config::ConfigHandle,
) -> anyhow::Result<()> {
    let mut cmd = Command::new(
        std::env::current_exe().context("resolving current executable for daemonize")?,
    );
    for arg in args {
        cmd.arg(arg);
    }

    cmd.stdin(Stdio::null());
    cmd.stdout(config.daemon_options.open_stdout()?);
    cmd.stderr(config.daemon_options.open_stderr()?);
    configure_spawned_daemon_command(&mut cmd);

    let _child = cmd
        .spawn()
        .context("spawning daemonized mux server child")?;
    Ok(())
}

/// Starts the re-exec'd server in a session of its own, as daemon(3) does.
///
/// Without it the "daemon" keeps its launcher's session, process group and
/// controlling terminal, so it dies with whatever ends them: a closed
/// terminal, a killed GUI app, a torn-down login session.
#[cfg(unix)]
#[allow(unsafe_code)]
fn configure_spawned_daemon_command(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: `pre_exec` runs the hook in the forked child before `exec`, where
    // only async-signal-safe work is allowed. The hook captures nothing, calls
    // only setsid(2), which is async-signal-safe, and builds its error from
    // errno without allocating. A freshly forked child never leads a process
    // group, so setsid only fails if the kernel refuses; `spawn` then returns
    // that error to the invoker instead of starting an undetached server.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(windows)]
fn configure_spawned_daemon_command(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;

    cmd.creation_flags(winapi::um::winbase::DETACHED_PROCESS);
}

#[cfg(all(test, unix))]
mod tests {
    use super::configure_spawned_daemon_command;
    use std::process::{Command, Stdio};

    /// `ps` fields: pid and process group everywhere, plus the session id on
    /// Linux. macOS `ps` prints a session of 0 for every process, so there the
    /// process group is the observable effect of setsid.
    fn ps_fields() -> Vec<&'static str> {
        let mut fields = vec!["-o", "pid=", "-o", "pgid="];
        if cfg!(target_os = "linux") {
            fields.extend(["-o", "sid="]);
        }
        fields
    }

    fn parse_ids(stdout: &[u8]) -> Vec<u32> {
        std::str::from_utf8(stdout)
            .expect("ps prints ASCII")
            .split_whitespace()
            .map(|id| id.parse().expect("ps prints decimal ids"))
            .collect()
    }

    fn launcher_ids() -> Vec<u32> {
        let output = Command::new("ps")
            .args(ps_fields())
            .arg("-p")
            .arg(std::process::id().to_string())
            .output()
            .expect("run ps on the test process");
        assert!(output.status.success(), "ps failed: {:?}", output.status);
        parse_ids(&output.stdout)
    }

    /// Ids of a child that execs `ps` on itself, so `ps` reports the spawned
    /// process exactly as the hook left it.
    fn child_ids(detach: bool) -> Vec<u32> {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("exec ps \"$@\" -p $$")
            .arg("sh")
            .args(ps_fields())
            .stdin(Stdio::null());
        if detach {
            configure_spawned_daemon_command(&mut cmd);
        }
        let output = cmd.output().expect("spawn a child that runs ps");
        assert!(
            output.status.success(),
            "child ps failed: {:?}",
            output.status
        );
        parse_ids(&output.stdout)
    }

    #[test]
    fn daemon_child_leaves_the_launcher_session_and_process_group() {
        let launcher = launcher_ids();
        let attached = child_ids(false);
        let detached = child_ids(true);
        let fields = ps_fields().len() / 2;
        assert_eq!(launcher.len(), fields, "launcher ids: {launcher:?}");
        assert_eq!(attached.len(), fields, "attached child ids: {attached:?}");
        assert_eq!(detached.len(), fields, "detached child ids: {detached:?}");

        // Control: a child spawned without the hook stays in the launcher's
        // process group (and session), so the checks below are not vacuous.
        assert_eq!(
            attached[1..],
            launcher[1..],
            "an undetached child shares its launcher's process group and session"
        );

        assert_eq!(
            detached[1], detached[0],
            "the daemon child must lead its own process group: {detached:?}"
        );
        assert_ne!(detached[1], launcher[1]);
        if cfg!(target_os = "linux") {
            assert_eq!(
                detached[2], detached[0],
                "the daemon child must lead its own session: {detached:?}"
            );
            assert_ne!(detached[2], launcher[2]);
        }
    }
}
