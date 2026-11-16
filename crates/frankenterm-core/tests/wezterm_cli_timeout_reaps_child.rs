//! ft-yykm1: a `wezterm cli` call that times out must reap its child.
//!
//! A long-lived `ft watch` polling a dead mux once leaked one defunct process
//! per poll: the CLI call's deadline was an outer timeout, and dropping the
//! future left the child unreaped. Nine watchers held 8,228 zombies and
//! exhausted the user's process limit. The deadline now belongs to the
//! process supervisor, which kills and reaps before the call settles.
//!
//! The probe runs in its own process (this test binary, re-executed with
//! `FT_WEZTERM_CLI` pointing at a fake CLI that never answers). That process
//! has no other children, so once the timed-out calls return, `waitpid` must
//! find nothing at all: no zombie and no straggler still running.

#![cfg(unix)]
#![forbid(unsafe_code)]

use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder};

const PROBE_ENV: &str = "FT_YYKM1_REAP_PROBE";
const TEST_NAME: &str = "timed_out_cli_calls_leave_no_child_behind";

#[test]
fn timed_out_cli_calls_leave_no_child_behind() {
    if std::env::var_os(PROBE_ENV).is_some() {
        probe();
        return;
    }

    let dir = tempfile::tempdir().expect("create stub dir");
    let stub = dir.path().join("wezterm");
    std::fs::write(&stub, "#!/bin/sh\nsleep 30\n").expect("write the never-answering CLI");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
        .expect("make the stub executable");

    let output = Command::new(std::env::current_exe().expect("this test binary"))
        .args([TEST_NAME, "--exact", "--nocapture", "--test-threads=1"])
        .env(PROBE_ENV, "1")
        .env("FT_WEZTERM_CLI", &stub)
        .env_remove("WEZTERM_UNIX_SOCKET")
        .output()
        .expect("run the probe process");
    assert!(
        output.status.success(),
        "probe failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn probe() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("build runtime");
    runtime.block_on(async {
        let cx = frankenterm_core::cx::for_testing();
        let client = frankenterm_core::wezterm::WeztermClient::new().with_timeout(1);
        for call in 0..2 {
            assert!(
                client.list_panes_with_cx(&cx).await.is_err(),
                "call {call}: the stub never answers, so the listing must time out"
            );
        }
    });

    match rustix::process::waitpid(None, rustix::process::WaitOptions::NOHANG) {
        Err(rustix::io::Errno::CHILD) => {}
        other => panic!("a timed-out CLI call left a child behind: {other:?}"),
    }
}
