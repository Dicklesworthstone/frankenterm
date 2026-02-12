use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    let ran = Arc::new(AtomicBool::new(false));
    let ran_task = Arc::clone(&ran);
    runtime.spawn_detached(async move {
        ran_task.store(true, Ordering::SeqCst);
    });

    std::thread::sleep(Duration::from_millis(25));
    assert!(
        ran.load(Ordering::SeqCst),
        "detached task should run on active runtime"
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
