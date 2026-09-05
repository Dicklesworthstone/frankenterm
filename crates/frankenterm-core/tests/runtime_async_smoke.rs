use std::sync::mpsc as std_mpsc;
use std::time::Duration;

#[cfg(unix)]
use frankenterm_core::runtime_async::process::{Command, CommandCancellation};
use frankenterm_core::runtime_async::{self, CompatRuntime, RuntimeBuilder, mpsc, watch};
#[cfg(unix)]
use std::time::Instant;

#[cfg(unix)]
#[test]
fn captured_commands_preserve_inherited_environment_removal() {
    // Seed only an owned child, never this multithreaded test process.
    // The ignored helper is selected exactly so a zero-test child
    // cannot turn environment inheritance into a false positive.
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "inherited_environment_removal_child",
            "--ignored",
            "--nocapture",
        ])
        .env("FT_CAPTURE_INHERITED_CANARY", "inherited")
        .stdout_limit(64 * 1024)
        .stderr_limit(64 * 1024)
        .output_blocking(Duration::from_secs(30))
        .unwrap();
    assert!(
        output.status.success(),
        "owned environment regression failed: status={}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("1 passed; 0 failed"),
        "owned helper must execute exactly one passing test"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "requires the owned parent's inherited environment canary"]
fn inherited_environment_removal_child() {
    const KEY: &str = "FT_CAPTURE_INHERITED_CANARY";
    assert_eq!(std::env::var(KEY).unwrap(), "inherited");
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let cx = frankenterm_core::cx::for_testing();
        for controlled in [false, true] {
            for (case, expected) in [
                (0, "x:inherited"),
                (1, ":missing"),
                (2, "x:override"),
                (3, ":missing"),
                (4, "x:"),
            ] {
                let mut command = Command::new("/bin/sh");
                command.args([
                    "-c",
                    "printf '%s:%s' \"${FT_CAPTURE_INHERITED_CANARY+x}\" \"${FT_CAPTURE_INHERITED_CANARY-missing}\"",
                ]);
                match case {
                    1 => {
                        command.env_remove(KEY);
                    }
                    2 => {
                        command.env_remove(KEY).env(KEY, "override");
                    }
                    3 => {
                        command.env(KEY, "override").env_remove(KEY);
                    }
                    4 => {
                        command.env(KEY, "");
                    }
                    _ => {}
                }
                command.stdout_limit(64).stderr_limit(64);
                let output = if controlled {
                    let report = command
                        .output_with_cx_controlled(
                            &cx,
                            Instant::now() + Duration::from_secs(5),
                            &CommandCancellation::new(),
                        )
                        .await;
                    assert!(report.supervisor_settled);
                    assert!(report.spawned_pid.is_some());
                    report.output.unwrap()
                } else {
                    command.output_blocking(Duration::from_secs(5)).unwrap()
                };
                assert!(output.status.success());
                assert_eq!(
                    output.stdout,
                    expected.as_bytes(),
                    "case {case}, controlled={controlled}"
                );
                assert!(output.stderr.is_empty());
            }
        }
    });
}

#[test]
fn runtime_builder_current_thread_runs_future() {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("runtime should build");
    let value = runtime.block_on(async { 2 + 2 });
    assert_eq!(value, 4);
}

#[test]
fn runtime_builder_multi_thread_runs_detached_tasks() {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .build()
        .expect("runtime should build");
    let (tx, rx) = std_mpsc::channel();
    runtime.spawn_detached(async move {
        tx.send("ran")
            .expect("detached task should signal completion");
    });

    let signal = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("detached task should run on active runtime");
    assert_eq!(signal, "ran");
}

#[test]
fn runtime_helpers_support_mpsc_round_trip() {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("runtime should build");
    let values = runtime.block_on(async {
        let (tx, mut rx) = mpsc::channel::<u8>(4);
        runtime_async::mpsc_send(&tx, 7)
            .await
            .expect("first send should succeed");
        runtime_async::mpsc_send(&tx, 9)
            .await
            .expect("second send should succeed");

        let first = runtime_async::mpsc_recv_option(&mut rx)
            .await
            .expect("first value should arrive");
        let second = runtime_async::mpsc_recv_option(&mut rx)
            .await
            .expect("second value should arrive");
        (first, second)
    });

    assert_eq!(values, (7, 9));
}

#[test]
fn runtime_helpers_support_watch_change_consumption() {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("runtime should build");
    let values = runtime.block_on(async {
        let (tx, mut rx) = watch::channel(0usize);
        assert!(!runtime_async::watch_has_changed(&rx));

        tx.send(1).expect("first watch send should succeed");
        runtime_async::watch_changed(&mut rx)
            .await
            .expect("receiver should observe first change");
        let first = runtime_async::watch_borrow_and_update_clone(&mut rx);

        tx.send(2).expect("second watch send should succeed");
        runtime_async::watch_changed(&mut rx)
            .await
            .expect("receiver should observe second change");
        let second = runtime_async::watch_borrow_and_update_clone(&mut rx);

        (first, second)
    });

    assert_eq!(values, (1, 2));
}

#[test]
fn timeout_reports_elapsed_for_slow_future() {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("runtime should build");
    let err = runtime
        .block_on(async {
            runtime_async::timeout(
                Duration::from_millis(5),
                runtime_async::sleep(Duration::from_secs(60)),
            )
            .await
        })
        .expect_err("slow future should time out");

    assert!(
        err.contains("elapsed") || err.contains("timeout") || err.contains("time"),
        "timeout error should mention time, got: {err}"
    );
}

#[test]
fn channel_constructors_are_available() {
    let (_tx, _rx) = mpsc::channel::<u8>(4);
    let (_tx, _rx) = watch::channel(0usize);
}

#[test]
fn sleep_and_timeout_helpers_work() {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("runtime should build");
    let result = runtime.block_on(async {
        runtime_async::sleep(Duration::from_millis(1)).await;
        runtime_async::timeout(Duration::from_secs(1), async { 7u8 }).await
    });
    let value = result.expect("timeout wrapper should resolve");
    assert_eq!(value, 7u8);
}
