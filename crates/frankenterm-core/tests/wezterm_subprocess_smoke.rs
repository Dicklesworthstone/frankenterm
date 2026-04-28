//! Smoke test for the `WeztermSubprocessFixture` (tests/common/wezterm_subprocess.rs).
//!
//! Proves that the fixture can spawn a real `wezterm-mux-server`, that
//! `WeztermClient::with_socket(...)` connects to it, and that
//! `list_panes()` returns the auto-spawned default pane. This is the
//! foundation for migrating MockWezterm-backed tests in snapshot_e2e.rs
//! (ft-dvgzi) and watchdog_labruntime.rs (ft-2funa) onto a real mux
//! subprocess.
//!
//! Gated on `FT_REAL_WEZTERM_TESTS=1` so default `cargo test` runs do not
//! require the wezterm-mux-server binary on PATH.
//!
//! Bead: ft-dvgzi.

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use common::wezterm_subprocess::{WeztermSubprocessFixture, should_run};

#[test]
fn fixture_spawn_returns_socket_path_inside_tempdir() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_WEZTERM_TESTS=1 to run real-wezterm fixture tests");
        return;
    }
    let fixture = WeztermSubprocessFixture::spawn().expect("spawn mux subprocess");
    assert!(
        fixture.socket_path().exists(),
        "socket should exist after spawn: {}",
        fixture.socket_path().display()
    );
    assert!(
        fixture.socket_path().starts_with(fixture.home_dir()),
        "socket must live under hermetic home_dir to avoid collision with the user's interactive wezterm session"
    );
    let pid = fixture.pid().expect("child pid present");
    assert!(pid > 0);
    // Drop tears down the subprocess and tempdir.
}

#[test]
fn fixture_drop_kills_subprocess_and_removes_tempdir() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_WEZTERM_TESTS=1 to run real-wezterm fixture tests");
        return;
    }
    let (pid, home_path) = {
        let fixture = WeztermSubprocessFixture::spawn().expect("spawn mux subprocess");
        let pid = fixture.pid().expect("pid");
        let home = fixture.home_dir().to_path_buf();
        (pid, home)
        // fixture drops here
    };
    // Give Drop a moment to finish.
    std::thread::sleep(std::time::Duration::from_millis(150));
    // `kill -0 <pid>` exits 0 if the process exists, 1 otherwise. We expect
    // the mux-server to have been reaped by Drop.
    let kill_status = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .expect("invoke kill -0");
    assert!(
        !kill_status.success(),
        "mux-server pid {pid} should be reaped after fixture Drop"
    );
    assert!(
        !home_path.exists(),
        "TempDir should be removed after fixture Drop: {}",
        home_path.display()
    );
}

#[test]
fn real_wezterm_client_lists_default_pane_via_subprocess() {
    if !should_run() {
        eprintln!("skip: set FT_REAL_WEZTERM_TESTS=1 to run real-wezterm fixture tests");
        return;
    }
    let fixture = WeztermSubprocessFixture::spawn().expect("spawn mux subprocess");
    let client = fixture.client();

    let runtime = RuntimeFixture::current_thread();
    let panes = runtime
        .block_on(async move { client.list_panes().await })
        .expect("list_panes against real mux subprocess");

    assert!(
        !panes.is_empty(),
        "default_prog in fixture config should auto-spawn at least one pane"
    );
    // The auto-spawned default pane comes from the `default_prog` we set
    // in the fixture (`/bin/sh -c 'sleep 600'`). Pane id is mux-assigned,
    // not caller-chosen — this is exactly the design constraint that
    // makes migrating snapshot_e2e.rs structurally non-trivial.
    let pane = &panes[0];
    eprintln!(
        "{{\"phase\":\"smoke\",\"event\":\"pane_listed\",\"pane_id\":{},\"window_id\":{},\"tab_id\":{},\"workspace\":\"{}\"}}",
        pane.pane_id,
        pane.window_id,
        pane.tab_id,
        pane.workspace.as_deref().unwrap_or("")
    );
}
