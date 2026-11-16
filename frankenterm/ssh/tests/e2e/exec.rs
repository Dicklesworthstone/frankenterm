use crate::sshd::*;
use frankenterm_ssh::runtime::block_on;
use frankenterm_ssh::ExecResult;
use portable_pty::{Child, ChildKiller};
use rstest::*;
use std::io::{Read, Write};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn read_all(mut reader: impl Read) -> String {
    let mut output = String::new();
    reader
        .read_to_string(&mut output)
        .expect("failed to read exec stream");
    output
}

fn collect_exec_result(result: ExecResult) -> (String, String, u32) {
    let ExecResult {
        stdout,
        stderr,
        mut child,
        ..
    } = result;

    let stdout = read_all(stdout);
    let stderr = read_all(stderr);
    let status = child.wait().expect("exec wait failed");

    (stdout, stderr, status.exit_code())
}

fn collect_exec_result_with_input(result: ExecResult, input: &[u8]) -> (String, String, u32) {
    let ExecResult {
        mut stdin,
        stdout,
        stderr,
        mut child,
    } = result;

    stdin
        .write_all(input)
        .expect("failed to write exec stdin payload");
    drop(stdin);

    let stdout = read_all(stdout);
    let stderr = read_all(stderr);
    let status = child.wait().expect("exec wait failed");

    (stdout, stderr, status.exit_code())
}

fn wait_for_exit(child: &mut impl Child, timeout: Duration) -> u32 {
    let deadline = Instant::now() + timeout;

    loop {
        if let Some(status) = child.try_wait().expect("exec try_wait failed") {
            return status.exit_code();
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for killed exec to exit"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn log_exec_outcome(scenario_id: &str, command: &str, stdout: &str, stderr: &str, exit_code: u32) {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_millis();
    let reason_code = if exit_code == 0 {
        "exec_ok"
    } else {
        "exec_non_zero_exit"
    };

    eprintln!(
        "{{\"timestamp_ms\":{timestamp_ms},\"component\":\"frankenterm_ssh_e2e_exec\",\"scenario_id\":\"{scenario_id}\",\"command\":{command:?},\"outcome\":\"exec_completed\",\"reason_code\":\"{reason_code}\",\"exit_code\":{exit_code},\"stdout\":{stdout:?},\"stderr\":{stderr:?}}}"
    );
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn exec_should_capture_stdout_stderr_and_zero_exit(#[future] session: SessionWithSshd) {
    if !sshd_available() {
        return;
    }
    block_on(async {
        let session: SessionWithSshd = session.await;
        let command = "sh -lc 'printf stdout; printf stderr >&2; exit 0'";
        let result = session
            .exec(command, None)
            .await
            .expect("exec should succeed");
        let (stdout, stderr, exit_code) = collect_exec_result(result);

        log_exec_outcome("exec-happy-path", command, &stdout, &stderr, exit_code);

        assert_eq!(stdout, "stdout");
        assert_eq!(stderr, "stderr");
        assert_eq!(exit_code, 0);
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn exec_should_recover_after_non_zero_exit_on_same_session(#[future] session: SessionWithSshd) {
    if !sshd_available() {
        return;
    }
    block_on(async {
        let session: SessionWithSshd = session.await;

        let failing_command = "sh -lc 'printf injected-failure >&2; exit 23'";
        let failing = session
            .exec(failing_command, None)
            .await
            .expect("failing exec should still establish the channel");
        let (stdout, stderr, exit_code) = collect_exec_result(failing);

        log_exec_outcome(
            "exec-failure-path",
            failing_command,
            &stdout,
            &stderr,
            exit_code,
        );

        assert!(stdout.is_empty(), "unexpected stdout from failing exec");
        assert_eq!(stderr, "injected-failure");
        assert_eq!(exit_code, 23);

        let recovery_command = "sh -lc 'printf recovered-output; exit 0'";
        let recovered = session
            .exec(recovery_command, None)
            .await
            .expect("session should recover after a non-zero exit");
        let (stdout, stderr, exit_code) = collect_exec_result(recovered);

        log_exec_outcome(
            "exec-recovery-path",
            recovery_command,
            &stdout,
            &stderr,
            exit_code,
        );

        assert_eq!(stdout, "recovered-output");
        assert!(stderr.is_empty(), "unexpected stderr from recovery exec");
        assert_eq!(exit_code, 0);
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn exec_should_echo_stdin_and_allow_followup_exec(#[future] session: SessionWithSshd) {
    if !sshd_available() {
        return;
    }
    block_on(async {
        let session: SessionWithSshd = session.await;

        let command = "sh -lc 'IFS= read -r line; printf \"%s\" \"$line\"; exit 0'";
        let input = b"stdin-roundtrip\n";
        let result = session
            .exec(command, None)
            .await
            .expect("stdin-driven exec should start");
        let (stdout, stderr, exit_code) = collect_exec_result_with_input(result, input);

        log_exec_outcome("exec-stdin-path", command, &stdout, &stderr, exit_code);

        assert_eq!(stdout, "stdin-roundtrip");
        assert!(stderr.is_empty(), "unexpected stderr from stdin exec");
        assert_eq!(exit_code, 0);

        let recovery_command = "sh -lc 'printf stdin-followup; exit 0'";
        let recovered = session
            .exec(recovery_command, None)
            .await
            .expect("session should remain usable after stdin-driven exec");
        let (stdout, stderr, exit_code) = collect_exec_result(recovered);

        log_exec_outcome(
            "exec-stdin-recovery-path",
            recovery_command,
            &stdout,
            &stderr,
            exit_code,
        );

        assert_eq!(stdout, "stdin-followup");
        assert!(
            stderr.is_empty(),
            "unexpected stderr from stdin recovery exec"
        );
        assert_eq!(exit_code, 0);
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
#[cfg_attr(not(feature = "libssh-rs"), ignore)]
fn exec_should_terminate_after_kill_and_allow_followup_exec(#[future] session: SessionWithSshd) {
    if !sshd_available() {
        return;
    }
    block_on(async {
        let session: SessionWithSshd = session.await;
        let command = "sleep 30";
        let ExecResult {
            stdout,
            stderr,
            mut child,
            ..
        } = session
            .exec(command, None)
            .await
            .expect("long-running exec should start");

        std::thread::sleep(Duration::from_millis(200));
        child.kill().expect("kill should signal the remote exec");

        let exit_code = wait_for_exit(&mut child, Duration::from_secs(5));
        let stdout = read_all(stdout);
        let stderr = read_all(stderr);

        log_exec_outcome("exec-kill-path", command, &stdout, &stderr, exit_code);

        assert!(stdout.is_empty(), "killed exec should not emit stdout");
        assert!(stderr.is_empty(), "killed exec should not emit stderr");
        assert_ne!(exit_code, 0, "killed exec should not report success");

        let recovery_command = "sh -lc 'printf post-kill-recovery; exit 0'";
        let recovered = session
            .exec(recovery_command, None)
            .await
            .expect("session should accept another exec after kill");
        let (stdout, stderr, exit_code) = collect_exec_result(recovered);

        log_exec_outcome(
            "exec-kill-recovery-path",
            recovery_command,
            &stdout,
            &stderr,
            exit_code,
        );

        assert_eq!(stdout, "post-kill-recovery");
        assert!(
            stderr.is_empty(),
            "unexpected stderr from post-kill recovery"
        );
        assert_eq!(exit_code, 0);
    })
}
