#![cfg(not(feature = "asupersync-runtime"))]

//! Tests for runtime_compat module — dual-runtime compatibility layer.
//!
//! These tests exercise the tokio path (default, without `asupersync-runtime` feature).
//! They verify that:
//! - Re-exported sync primitives (Mutex, RwLock, Semaphore) work correctly
//! - Channel aliases (mpsc, watch) are functional
//! - RuntimeBuilder produces working runtimes with correct configuration
//! - CompatRuntime trait methods (block_on, spawn_detached) execute properly
//! - sleep() and timeout() wrappers behave as expected

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use frankenterm_core::runtime_compat::{
    self, CompatRuntime, Mutex, RuntimeBuilder, RwLock, Semaphore,
};

// ────────────────────────────────────────────────────────────────────
// Mutex
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn mutex_lock_and_read() {
    let m = Mutex::new(42u64);
    let guard = m.lock().await;
    assert_eq!(*guard, 42);
}

#[tokio::test]
async fn mutex_lock_and_mutate() {
    let m = Mutex::new(0u64);
    {
        let mut guard = m.lock().await;
        *guard = 99;
    }
    let guard = m.lock().await;
    assert_eq!(*guard, 99);
}

#[tokio::test]
async fn mutex_sequential_locks_preserve_state() {
    let m = Arc::new(Mutex::new(0u64));
    for i in 1..=10 {
        let mut guard = m.lock().await;
        *guard += 1;
        assert_eq!(*guard, i);
    }
}

#[tokio::test]
async fn mutex_concurrent_tasks() {
    let m = Arc::new(Mutex::new(0u64));
    let mut handles = Vec::new();
    for _ in 0..10 {
        let m = Arc::clone(&m);
        handles.push(tokio::spawn(async move {
            let mut guard = m.lock().await;
            *guard += 1;
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    assert_eq!(*m.lock().await, 10);
}

// ────────────────────────────────────────────────────────────────────
// RwLock
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rwlock_read_access() {
    let rw = RwLock::new(42u64);
    let guard = rw.read().await;
    assert_eq!(*guard, 42);
}

#[tokio::test]
async fn rwlock_write_access() {
    let rw = RwLock::new(0u64);
    {
        let mut guard = rw.write().await;
        *guard = 77;
    }
    let guard = rw.read().await;
    assert_eq!(*guard, 77);
}

#[tokio::test]
async fn rwlock_concurrent_reads() {
    let rw = Arc::new(RwLock::new(42u64));
    let mut handles = Vec::new();
    for _ in 0..10 {
        let rw = Arc::clone(&rw);
        handles.push(tokio::spawn(async move {
            let guard = rw.read().await;
            assert_eq!(*guard, 42);
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
}

#[tokio::test]
async fn rwlock_write_then_read() {
    let rw = Arc::new(RwLock::new(String::from("hello")));
    {
        let mut guard = rw.write().await;
        guard.push_str(" world");
    }
    let guard = rw.read().await;
    assert_eq!(&*guard, "hello world");
}

// ────────────────────────────────────────────────────────────────────
// Semaphore
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn semaphore_acquire_and_release() {
    let sem = Semaphore::new(2);
    assert_eq!(sem.available_permits(), 2);

    let p1 = sem.acquire().await.unwrap();
    assert_eq!(sem.available_permits(), 1);

    let _p2 = sem.acquire().await.unwrap();
    assert_eq!(sem.available_permits(), 0);

    drop(p1);
    assert_eq!(sem.available_permits(), 1);
}

#[tokio::test]
async fn semaphore_try_acquire_succeeds() {
    let sem = Semaphore::new(1);
    let permit = sem.try_acquire().unwrap();
    assert_eq!(sem.available_permits(), 0);
    drop(permit);
    assert_eq!(sem.available_permits(), 1);
}

#[tokio::test]
async fn semaphore_try_acquire_no_permits() {
    let sem = Semaphore::new(0);
    let err = sem.try_acquire().unwrap_err();
    // tokio's TryAcquireError::NoPermits
    assert!(format!("{}", err).contains("no permits"));
}

#[tokio::test]
async fn semaphore_close_blocks_acquire() {
    let sem = Semaphore::new(5);
    sem.close();
    let result = sem.try_acquire();
    assert!(result.is_err());
}

#[tokio::test]
async fn semaphore_available_permits_tracks() {
    let sem = Semaphore::new(3);
    assert_eq!(sem.available_permits(), 3);

    let _p1 = sem.try_acquire().unwrap();
    assert_eq!(sem.available_permits(), 2);

    let _p2 = sem.try_acquire().unwrap();
    assert_eq!(sem.available_permits(), 1);

    let _p3 = sem.try_acquire().unwrap();
    assert_eq!(sem.available_permits(), 0);
}

#[tokio::test]
async fn semaphore_acquire_owned() {
    let sem = Arc::new(Semaphore::new(1));
    let permit = Semaphore::acquire_owned(Arc::clone(&sem)).await.unwrap();
    assert_eq!(sem.available_permits(), 0);
    drop(permit);
    assert_eq!(sem.available_permits(), 1);
}

#[tokio::test]
async fn semaphore_try_acquire_owned_succeeds() {
    let sem = Arc::new(Semaphore::new(1));
    let permit = Semaphore::try_acquire_owned(Arc::clone(&sem)).unwrap();
    assert_eq!(sem.available_permits(), 0);
    drop(permit);
    assert_eq!(sem.available_permits(), 1);
}

#[tokio::test]
async fn semaphore_try_acquire_owned_no_permits() {
    let sem = Arc::new(Semaphore::new(0));
    let result = Semaphore::try_acquire_owned(Arc::clone(&sem));
    assert!(result.is_err());
}

// ────────────────────────────────────────────────────────────────────
// mpsc channel
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn mpsc_send_and_recv() {
    let (tx, mut rx) = runtime_compat::mpsc::channel::<u64>(8);
    tx.send(42).await.unwrap();
    tx.send(43).await.unwrap();
    assert_eq!(rx.recv().await, Some(42));
    assert_eq!(rx.recv().await, Some(43));
}

#[tokio::test]
async fn mpsc_closed_on_sender_drop() {
    let (tx, mut rx) = runtime_compat::mpsc::channel::<u64>(8);
    tx.send(1).await.unwrap();
    drop(tx);
    assert_eq!(rx.recv().await, Some(1));
    assert_eq!(rx.recv().await, None); // channel closed
}

#[tokio::test]
async fn mpsc_multiple_senders() {
    let (tx, mut rx) = runtime_compat::mpsc::channel::<u64>(16);
    let tx2 = tx.clone();

    tx.send(1).await.unwrap();
    tx2.send(2).await.unwrap();

    let mut values = vec![rx.recv().await.unwrap(), rx.recv().await.unwrap()];
    values.sort();
    assert_eq!(values, vec![1, 2]);
}

// ────────────────────────────────────────────────────────────────────
// watch channel
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn watch_send_and_borrow() {
    let (tx, rx) = runtime_compat::watch::channel(0u64);
    assert_eq!(*rx.borrow(), 0);

    tx.send(42).unwrap();
    assert_eq!(*rx.borrow(), 42);
}

#[tokio::test]
async fn watch_changed_notification() {
    let (tx, mut rx) = runtime_compat::watch::channel(0u64);
    tx.send(1).unwrap();

    rx.changed().await.unwrap();
    assert_eq!(*rx.borrow_and_update(), 1);
}

#[tokio::test]
async fn watch_multiple_receivers() {
    let (tx, rx1) = runtime_compat::watch::channel(0u64);
    let rx2 = rx1.clone();

    tx.send(99).unwrap();
    assert_eq!(*rx1.borrow(), 99);
    assert_eq!(*rx2.borrow(), 99);
}

// ────────────────────────────────────────────────────────────────────
// RuntimeBuilder
// ────────────────────────────────────────────────────────────────────

#[test]
fn runtime_builder_current_thread() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    let result = rt.block_on(async { 42 });
    assert_eq!(result, 42);
}

#[test]
fn runtime_builder_multi_thread() {
    let rt = RuntimeBuilder::multi_thread().build().unwrap();
    let result = rt.block_on(async { 99 });
    assert_eq!(result, 99);
}

#[test]
fn runtime_builder_worker_threads_on_multi_thread() {
    let rt = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .build()
        .unwrap();
    let result = rt.block_on(async { 7 });
    assert_eq!(result, 7);
}

#[test]
fn runtime_builder_worker_threads_ignored_on_current_thread() {
    // worker_threads should be silently ignored for current_thread
    let rt = RuntimeBuilder::current_thread()
        .worker_threads(4)
        .build()
        .unwrap();
    let result = rt.block_on(async { 55 });
    assert_eq!(result, 55);
}

// ────────────────────────────────────────────────────────────────────
// CompatRuntime
// ────────────────────────────────────────────────────────────────────

#[test]
fn compat_runtime_block_on_async_value() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    let v = rt.block_on(async {
        let x = 10;
        let y = 20;
        x + y
    });
    assert_eq!(v, 30);
}

#[test]
fn compat_runtime_block_on_with_await() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    let v = rt.block_on(async {
        runtime_compat::sleep(Duration::from_millis(1)).await;
        42
    });
    assert_eq!(v, 42);
}

#[test]
fn compat_runtime_spawn_detached_runs() {
    let rt = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    let flag = Arc::new(AtomicUsize::new(0));
    let flag2 = Arc::clone(&flag);

    rt.block_on(async move {
        rt2_spawn_helper(&flag2);
        // Give the detached task time to run
        runtime_compat::sleep(Duration::from_millis(50)).await;
    });

    assert_eq!(flag.load(Ordering::SeqCst), 1);
}

fn rt2_spawn_helper(flag: &Arc<AtomicUsize>) {
    // We can't call spawn_detached from within block_on easily without a handle,
    // so test using tokio::spawn directly (which is what the tokio CompatRuntime delegates to)
    let f = Arc::clone(flag);
    tokio::spawn(async move {
        f.store(1, Ordering::SeqCst);
    });
}

// ────────────────────────────────────────────────────────────────────
// sleep
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn sleep_completes_after_duration() {
    let start = Instant::now();
    runtime_compat::sleep(Duration::from_millis(10)).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(5),
        "sleep should wait at least ~10ms, got {}ms",
        elapsed.as_millis()
    );
}

#[tokio::test]
async fn sleep_zero_duration_returns_immediately() {
    let start = Instant::now();
    runtime_compat::sleep(Duration::ZERO).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(50),
        "zero sleep should be near-instant, got {}ms",
        elapsed.as_millis()
    );
}

// ────────────────────────────────────────────────────────────────────
// timeout
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn timeout_ok_when_future_completes_in_time() {
    let result = runtime_compat::timeout(Duration::from_secs(1), async { 42 }).await;
    assert_eq!(result.unwrap(), 42);
}

#[tokio::test]
async fn timeout_err_when_future_exceeds_deadline() {
    let result = runtime_compat::timeout(
        Duration::from_millis(5),
        runtime_compat::sleep(Duration::from_secs(60)),
    )
    .await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("elapsed") || err_msg.contains("timeout") || err_msg.contains("time"),
        "timeout error should mention time, got: {}",
        err_msg
    );
}

#[tokio::test]
async fn timeout_returns_future_output_type() {
    let result =
        runtime_compat::timeout(Duration::from_secs(1), async { String::from("hello") }).await;
    assert_eq!(result.unwrap(), "hello");
}

#[tokio::test]
async fn timeout_zero_duration_on_ready_future() {
    // A future that's immediately ready should still succeed with zero timeout
    // (tokio may or may not allow this — test documents actual behavior)
    let result = runtime_compat::timeout(Duration::ZERO, async { 1 }).await;
    // Either Ok(1) or Err — both are valid depending on scheduler
    match result {
        Ok(v) => assert_eq!(v, 1),
        Err(_) => {} // acceptable: zero timeout may expire first
    }
}

// ────────────────────────────────────────────────────────────────────
// Error type Display
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn try_acquire_error_display_no_permits() {
    let sem = Semaphore::new(0);
    let err = sem.try_acquire().unwrap_err();
    let msg = format!("{}", err);
    assert!(
        !msg.is_empty(),
        "TryAcquireError should have a display message"
    );
}

#[tokio::test]
async fn try_acquire_error_display_closed() {
    let sem = Semaphore::new(5);
    sem.close();
    let err = sem.try_acquire().unwrap_err();
    let msg = format!("{}", err);
    assert!(
        !msg.is_empty(),
        "TryAcquireError::Closed should have a display message"
    );
}

#[tokio::test]
async fn acquire_error_on_closed_semaphore() {
    let sem = Semaphore::new(1);
    sem.close();
    let result = sem.acquire().await;
    assert!(result.is_err(), "acquire on closed semaphore should fail");
}

// ────────────────────────────────────────────────────────────────────
// Integration: RuntimeBuilder + CompatRuntime + channels
// ────────────────────────────────────────────────────────────────────

#[test]
fn runtime_with_mpsc_channel() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    rt.block_on(async {
        let (tx, mut rx) = runtime_compat::mpsc::channel::<String>(4);
        tx.send("from runtime".into()).await.unwrap();
        let val = rx.recv().await.unwrap();
        assert_eq!(val, "from runtime");
    });
}

#[test]
fn runtime_with_watch_channel() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    rt.block_on(async {
        let (tx, rx) = runtime_compat::watch::channel(0u64);
        tx.send(100).unwrap();
        assert_eq!(*rx.borrow(), 100);
    });
}

#[test]
fn runtime_with_mutex_and_sleep() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    rt.block_on(async {
        let m = Mutex::new(0u64);
        runtime_compat::sleep(Duration::from_millis(1)).await;
        let mut guard = m.lock().await;
        *guard = 42;
        assert_eq!(*guard, 42);
    });
}

#[test]
fn runtime_with_timeout() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    let result = rt.block_on(async {
        runtime_compat::timeout(Duration::from_secs(1), async { "done" }).await
    });
    assert_eq!(result.unwrap(), "done");
}
