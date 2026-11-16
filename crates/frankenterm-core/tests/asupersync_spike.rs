#![cfg(feature = "asupersync-runtime")]

mod common;

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use asupersync::channel::mpsc;
use asupersync::combinator::{Either, Select};
use asupersync::sync::{Mutex, Semaphore};
use asupersync::{Budget, CancelKind, Cx, LabConfig, LabRuntime, Time};
use common::fixtures::{RuntimeFixture, healthy_cx, mock_unix_stream_pair};
use frankenterm_core::cx::{CxRuntimeBuilder, spawn_with_cx};

async fn sleep_from_current(duration: Duration) {
    let now = Cx::current()
        .and_then(|cx| cx.timer_driver())
        .map_or(Time::ZERO, |driver| driver.now());
    asupersync::time::sleep(now, duration).await;
}

#[test]
fn spike_unixstream_pdu_framing_with_partial_reads() {
    let runtime = RuntimeFixture::current_thread();

    runtime.block_on(async {
        let cx = healthy_cx();
        let (client, server) = mock_unix_stream_pair();
        let payload = b"frankenterm-spike";

        let mut frame = Vec::with_capacity(5 + payload.len());
        frame.extend_from_slice(
            &u32::try_from(1 + payload.len())
                .expect("payload length should fit in u32")
                .to_be_bytes(),
        );
        frame.push(0x7f);
        frame.extend_from_slice(payload);

        let mut start = 0usize;
        let mut written = 0usize;
        for end in [2usize, 5, frame.len()] {
            written += client
                .write(&cx, &frame[start..end])
                .await
                .expect("write chunk");
            start = end;
        }

        let mut received = Vec::new();
        for max_bytes in [2usize, 3, 4, frame.len()] {
            let chunk = server.read(&cx, max_bytes).await.expect("read chunk");
            if !chunk.is_empty() {
                received.extend_from_slice(&chunk);
            }
            if received.len() == frame.len() {
                break;
            }
        }

        assert_eq!(written, frame.len(), "full frame should be written");
        assert_eq!(
            received.len(),
            frame.len(),
            "full frame should be reconstructed"
        );
        assert_eq!(
            u32::from_be_bytes(received[0..4].try_into().expect("length prefix")) as usize,
            1 + payload.len(),
            "length prefix should include type byte + payload"
        );
        assert_eq!(received[4], 0x7f, "type byte should survive partial reads");
        assert_eq!(&received[5..], payload, "payload should round-trip");
        assert_eq!(client.bytes_written(), frame.len() as u64);
        assert_eq!(server.bytes_read(), frame.len() as u64);
    });
}

#[test]
fn spike_two_phase_channel_send_in_scope_cancels_cleanly() {
    let runtime = CxRuntimeBuilder::current_thread()
        .build()
        .expect("build migration runtime");
    let handle = runtime.handle();
    let root_cx = frankenterm_core::cx::for_testing();
    let scope = root_cx.scope();

    runtime.block_on(async {
        let (tx, mut rx) = mpsc::channel::<usize>(8);
        let delivered = Arc::new(AtomicUsize::new(0));

        let producer = spawn_with_cx(&handle, &root_cx, move |producer_cx| async move {
            let mut sent = 0usize;
            loop {
                if producer_cx.checkpoint().is_err() {
                    break;
                }

                let permit = match tx.reserve(&producer_cx).await {
                    Ok(permit) => permit,
                    Err(_) => break,
                };
                permit.send(sent);
                sent += 1;
                asupersync::runtime::yield_now().await;
            }
            sent
        });

        let received_counter = Arc::clone(&delivered);
        let consumer = spawn_with_cx(&handle, &root_cx, move |consumer_cx| async move {
            let mut received = 0usize;
            loop {
                if consumer_cx.checkpoint().is_err() {
                    break;
                }

                match rx.recv(&consumer_cx).await {
                    Ok(_) => {
                        received += 1;
                        received_counter.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(_) => break,
                }

                asupersync::runtime::yield_now().await;
            }
            received
        });

        while delivered.load(Ordering::SeqCst) < 3 {
            asupersync::runtime::yield_now().await;
        }

        root_cx.cancel_with(CancelKind::User, Some("mid-stream spike cancellation"));

        let sent = producer.await;
        let received = consumer.await;

        assert_eq!(scope.region_id(), root_cx.region_id());
        assert_eq!(scope.budget(), root_cx.budget());
        assert!(
            root_cx.checkpoint().is_err(),
            "parent context should reflect explicit cancellation"
        );
        assert!(
            sent >= 3,
            "producer should commit some messages before cancellation"
        );
        assert!(
            received >= 3,
            "consumer should drain at least a few messages before cancellation"
        );
        assert_eq!(delivered.load(Ordering::SeqCst), received);
    });
}

#[test]
fn spike_pool_pattern_semaphore_mutex_budget_timeout() {
    let runtime = CxRuntimeBuilder::current_thread()
        .build()
        .expect("build migration runtime");
    let handle = runtime.handle();

    runtime.block_on(async {
        let gate = Arc::new(Semaphore::new(3));
        let pool = Arc::new(Mutex::new(VecDeque::<usize>::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let timed_out = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for task_id in 0usize..10 {
            let gate = Arc::clone(&gate);
            let pool = Arc::clone(&pool);
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            let completed = Arc::clone(&completed);
            let timed_out = Arc::clone(&timed_out);

            tasks.push(handle.spawn(async move {
                let task_cx = if task_id < 7 {
                    Cx::for_testing_with_budget(Budget::new().with_deadline(Time::from_secs(30)))
                } else {
                    let timeout_cx =
                        Cx::for_testing_with_budget(Budget::new().with_deadline(Time::ZERO));
                    timeout_cx.cancel_with(CancelKind::Timeout, Some("spike pool timeout"));
                    timeout_cx
                };

                match gate.acquire(&task_cx, 1).await {
                    Ok(permit) => {
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        max_active.fetch_max(current, Ordering::SeqCst);
                        {
                            let mut entries = pool.lock(&task_cx).await.expect("lock pool");
                            entries.push_back(task_id);
                        }
                        asupersync::runtime::yield_now().await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        completed.fetch_add(1, Ordering::SeqCst);
                        drop(permit);
                    }
                    Err(_) => {
                        timed_out.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }));
        }

        for task in tasks {
            task.await;
        }

        let inspect_cx = healthy_cx();
        let entries = pool.lock(&inspect_cx).await.expect("inspect pool");
        assert!(
            max_active.load(Ordering::SeqCst) <= 3,
            "semaphore must cap concurrency at 3"
        );
        assert!(
            timed_out.load(Ordering::SeqCst) > 0,
            "at least one task should observe timeout-style cancellation"
        );
        assert_eq!(
            entries.len(),
            completed.load(Ordering::SeqCst),
            "successful acquisitions should be recorded exactly once"
        );
        assert_eq!(
            active.load(Ordering::SeqCst),
            0,
            "all tasks should exit cleanly"
        );
        assert_eq!(gate.available_permits(), 3, "permits must not leak");
    });
}

#[test]
fn spike_labruntime_virtual_time_and_oracle_report() {
    let wall_start = Instant::now();
    let mut runtime = LabRuntime::new(
        LabConfig::new(7)
            .with_auto_advance()
            .worker_count(2)
            .max_steps(10_000),
    );
    let region = runtime.state.create_root_region(Budget::INFINITE);
    let (task_id, _handle) = runtime
        .state
        .create_task(region, Budget::INFINITE, async {
            sleep_from_current(Duration::from_secs(1)).await;
        })
        .expect("spawn lab sleep task");
    runtime.scheduler.lock().schedule(task_id, 0);

    runtime.step_for_test();
    let virtual_time = runtime.run_with_auto_advance();
    let report = runtime.run_until_quiescent_with_report();

    assert!(
        virtual_time.auto_advances >= 1,
        "auto-advance should jump to the timer deadline"
    );
    assert!(
        runtime.now() >= Time::from_secs(1),
        "virtual time should advance by at least one second"
    );
    assert!(
        wall_start.elapsed() < Duration::from_secs(1),
        "virtual-time sleep should not consume a real second"
    );
    assert!(report.oracle_report.all_passed());
    assert_eq!(report.invariant_violations, [] as [std::string::String; 0]);
}

#[test]
fn spike_select_and_race_semantics() {
    let runtime = RuntimeFixture::current_thread();

    runtime.block_on(async {
        let cx = healthy_cx();
        let (tx, mut rx) = mpsc::channel::<&'static str>(1);
        tx.send(&cx, "ready").await.expect("seed ready message");

        let selected = Select::new(
            rx.recv(&cx),
            Box::pin(sleep_from_current(Duration::from_millis(1))),
        )
        .await
        .expect("select should succeed");
        assert!(
            matches!(selected, Either::Left(Ok("ready"))),
            "ready channel receive should beat a pending timer"
        );

        let futures: Vec<Pin<Box<dyn std::future::Future<Output = u8> + Send>>> =
            vec![Box::pin(async { 11_u8 }), Box::pin(async { 22_u8 })];
        let raced = cx.race(futures).await.expect("race should complete");
        assert_eq!(raced, 11);
    });

    let sleep_won = Arc::new(AtomicUsize::new(usize::MAX));
    let sleep_won_task = Arc::clone(&sleep_won);
    let mut runtime = LabRuntime::new(
        LabConfig::new(23)
            .with_auto_advance()
            .worker_count(2)
            .max_steps(10_000),
    );
    let region = runtime.state.create_root_region(Budget::INFINITE);
    let (_tx, mut rx) = mpsc::channel::<usize>(1);
    let (task_id, _handle) = runtime
        .state
        .create_task(region, Budget::INFINITE, async move {
            let cx = Cx::for_testing();
            let selected = Select::new(
                rx.recv(&cx),
                Box::pin(sleep_from_current(Duration::from_secs(1))),
            )
            .await
            .expect("select should not error");
            let outcome = match selected {
                Either::Left(Ok(_)) => 0,
                Either::Left(Err(_)) => 1,
                Either::Right(()) => 2,
            };
            sleep_won_task.store(outcome, Ordering::SeqCst);
        })
        .expect("spawn select task");
    runtime.scheduler.lock().schedule(task_id, 0);

    runtime.step_for_test();
    let report = runtime.run_with_auto_advance();
    let oracle_report = runtime.run_until_quiescent_with_report();

    assert!(
        report.auto_advances >= 1,
        "virtual-time select should advance to the timer deadline"
    );
    assert_eq!(
        sleep_won.load(Ordering::SeqCst),
        2,
        "sleep branch should win when no message arrives"
    );
    assert!(oracle_report.oracle_report.all_passed());
    assert_eq!(
        oracle_report.invariant_violations,
        [] as [std::string::String; 0]
    );
}
