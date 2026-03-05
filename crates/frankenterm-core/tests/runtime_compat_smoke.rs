use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use frankenterm_core::runtime_compat::{self, CompatRuntime, RuntimeBuilder, mpsc, watch};

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
        runtime_compat::mpsc_send(&tx, 7)
            .await
            .expect("first send should succeed");
        runtime_compat::mpsc_send(&tx, 9)
            .await
            .expect("second send should succeed");

        let first = runtime_compat::mpsc_recv_option(&mut rx)
            .await
            .expect("first value should arrive");
        let second = runtime_compat::mpsc_recv_option(&mut rx)
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
        assert!(!runtime_compat::watch_has_changed(&rx));

        tx.send(1).expect("first watch send should succeed");
        runtime_compat::watch_changed(&mut rx)
            .await
            .expect("receiver should observe first change");
        let first = runtime_compat::watch_borrow_and_update_clone(&mut rx);

        tx.send(2).expect("second watch send should succeed");
        runtime_compat::watch_changed(&mut rx)
            .await
            .expect("receiver should observe second change");
        let second = runtime_compat::watch_borrow_and_update_clone(&mut rx);

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
            runtime_compat::timeout(
                Duration::from_millis(5),
                runtime_compat::sleep(Duration::from_secs(60)),
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
        runtime_compat::sleep(Duration::from_millis(1)).await;
        runtime_compat::timeout(Duration::from_secs(1), async { 7u8 }).await
    });
    let value = result.expect("timeout wrapper should resolve");
    assert_eq!(value, 7u8);
}
