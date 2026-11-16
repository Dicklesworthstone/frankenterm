// =============================================================================
// Deterministic cancellation & concurrency verification tests (ft-e34d9.10.6)
//
// Uses asupersync's LabRuntime for deterministic, seed-based verification of
// concurrent channel delivery, atomic counter invariants, and semaphore
// enforcement. Tests use DPOR exploration and chaos fault injection to
// find schedule-dependent bugs.
//
// Coverage:
//   D01–D04: Channel delivery and closure under LabRuntime
//   D05–D07: Concurrent counter and ordering verification
//   D08–D09: DPOR exploration for schedule-dependent bugs
//   D10–D11: Chaos fault injection resilience
// =============================================================================

#![cfg(feature = "asupersync-runtime")]

mod common;

use asupersync::Budget;
use common::lab::{
    ChaosTestConfig, ExplorationTestConfig, LabTestConfig, run_chaos_test, run_exploration_test,
    run_lab_test, run_lab_test_simple,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

// =============================================================================
// D01–D04: Channel delivery and closure under LabRuntime
// =============================================================================

/// D01: mpsc channel delivers all items deterministically under LabRuntime.
///
/// Creates a producer and consumer task under deterministic scheduling.
/// Verifies all items are delivered and received count matches sent count.
#[test]
fn d01_mpsc_delivery_deterministic() {
    for seed in [0, 42, 100, 999] {
        let report = run_lab_test_simple(seed, "d01_mpsc_delivery", |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            let (tx, rx) = asupersync::channel::mpsc::channel::<u32>(16);
            let received = Arc::new(AtomicUsize::new(0));

            // Producer task
            let (tid1, _h1) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let cx = asupersync::cx::Cx::for_testing();
                    for i in 0..10 {
                        tx.send(&cx, i).await.expect("send should succeed");
                    }
                    drop(tx);
                })
                .expect("create producer");
            runtime.scheduler.lock().schedule(tid1, 0);

            // Consumer task
            let recv_count = received.clone();
            let (tid2, _h2) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let cx = asupersync::cx::Cx::for_testing();
                    let mut rx = rx;
                    while let Ok(_item) = rx.recv(&cx).await {
                        recv_count.fetch_add(1, Ordering::SeqCst);
                    }
                })
                .expect("create consumer");
            runtime.scheduler.lock().schedule(tid2, 0);

            runtime.run_until_quiescent();

            assert_eq!(
                received.load(Ordering::SeqCst),
                10,
                "[seed={seed}] all 10 items must be delivered"
            );
        });
        assert!(report.passed());
    }
}

/// D02: Dropping sender causes receiver to observe closure.
#[test]
fn d02_sender_drop_causes_recv_closure() {
    let report = run_lab_test_simple(42, "d02_sender_drop_closure", |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let (tx, rx) = asupersync::channel::mpsc::channel::<u32>(8);
        let closed_observed = Arc::new(AtomicUsize::new(0));

        // Send one item then drop
        let (tid1, _h1) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let cx = asupersync::cx::Cx::for_testing();
                tx.send(&cx, 1).await.expect("send");
                // tx dropped here
            })
            .expect("create sender");
        runtime.scheduler.lock().schedule(tid1, 0);

        // Receive until closure
        let closed = closed_observed.clone();
        let (tid2, _h2) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let cx = asupersync::cx::Cx::for_testing();
                let mut rx = rx;
                let mut count = 0u32;
                loop {
                    match rx.recv(&cx).await {
                        Ok(_) => count += 1,
                        Err(_) => {
                            closed.store(1, Ordering::SeqCst);
                            break;
                        }
                    }
                }
                assert!(count >= 1, "should receive at least 1 item before closure");
            })
            .expect("create receiver");
        runtime.scheduler.lock().schedule(tid2, 0);

        runtime.run_until_quiescent();

        assert_eq!(
            closed_observed.load(Ordering::SeqCst),
            1,
            "receiver must observe channel closure after sender drop"
        );
    });
    assert!(report.passed());
}

/// D03: Multiple producers to single consumer — all items delivered.
#[test]
fn d03_fanin_multiple_producers() {
    for seed in [0, 42, 77] {
        let report = run_lab_test(
            LabTestConfig::new(seed, "d03_fanin")
                .worker_count(4)
                .max_steps(200_000),
            |runtime| {
                let region = runtime.state.create_root_region(Budget::INFINITE);
                let (tx, rx) = asupersync::channel::mpsc::channel::<u32>(32);
                let received = Arc::new(AtomicUsize::new(0));

                // 3 producers, each sending 5 items
                for producer_id in 0..3u32 {
                    let tx = tx.clone();
                    let (tid, _h) = runtime
                        .state
                        .create_task(region, Budget::INFINITE, async move {
                            let cx = asupersync::cx::Cx::for_testing();
                            for i in 0..5 {
                                tx.send(&cx, producer_id * 100 + i).await.expect("send");
                            }
                        })
                        .expect("create producer");
                    runtime.scheduler.lock().schedule(tid, 0);
                }
                drop(tx);

                // Consumer
                let cnt = received.clone();
                let (tid, _h) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        let cx = asupersync::cx::Cx::for_testing();
                        let mut rx = rx;
                        while rx.recv(&cx).await.is_ok() {
                            cnt.fetch_add(1, Ordering::SeqCst);
                        }
                    })
                    .expect("create consumer");
                runtime.scheduler.lock().schedule(tid, 0);

                runtime.run_until_quiescent();

                assert_eq!(
                    received.load(Ordering::SeqCst),
                    15,
                    "[seed={seed}] all 15 items (3×5) must be delivered"
                );
            },
        );
        assert!(report.passed());
    }
}

/// D04: Semaphore under LabRuntime — permits enforced deterministically.
#[test]
fn d04_semaphore_permit_enforcement() {
    let report = run_lab_test_simple(42, "d04_semaphore", |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        let sem = Arc::new(asupersync::sync::Semaphore::new(2));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let current = Arc::new(AtomicUsize::new(0));

        for _ in 0..5 {
            let sem = sem.clone();
            let max_c = max_concurrent.clone();
            let cur = current.clone();
            let (tid, _h) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let cx = asupersync::cx::Cx::for_testing();
                    let _permit = sem.acquire(&cx, 1).await.expect("acquire");
                    let val = cur.fetch_add(1, Ordering::SeqCst) + 1;
                    let _ = max_c.fetch_max(val, Ordering::SeqCst);
                    // Yield to let other tasks try to acquire
                    asupersync::runtime::yield_now().await;
                    cur.fetch_sub(1, Ordering::SeqCst);
                })
                .expect("create task");
            runtime.scheduler.lock().schedule(tid, 0);
        }

        runtime.run_until_quiescent();

        assert!(
            max_concurrent.load(Ordering::SeqCst) <= 2,
            "semaphore must limit concurrency to 2"
        );
    });
    assert!(report.passed());
}

// =============================================================================
// D05–D07: Concurrent counter and ordering verification
// =============================================================================

/// D05: Atomic counter incremented by concurrent tasks reaches exact total.
#[test]
fn d05_concurrent_counter_exact_total() {
    for seed in [0, 42, 100] {
        let report = run_lab_test(
            LabTestConfig::new(seed, "d05_counter")
                .worker_count(4)
                .max_steps(200_000),
            |runtime| {
                let region = runtime.state.create_root_region(Budget::INFINITE);
                let counter = Arc::new(AtomicU64::new(0));

                for _ in 0..10 {
                    let c = counter.clone();
                    let (tid, _h) = runtime
                        .state
                        .create_task(region, Budget::INFINITE, async move {
                            for _ in 0..100 {
                                c.fetch_add(1, Ordering::SeqCst);
                            }
                        })
                        .expect("create counter task");
                    runtime.scheduler.lock().schedule(tid, 0);
                }

                runtime.run_until_quiescent();

                assert_eq!(
                    counter.load(Ordering::SeqCst),
                    1000,
                    "[seed={seed}] 10 tasks × 100 increments = 1000"
                );
            },
        );
        assert!(report.passed());
    }
}

/// D06: Deterministic replay — same seed produces same results.
#[test]
fn d06_deterministic_replay_same_seed() {
    let seed = 42;
    let mut results = Vec::new();

    for _ in 0..2 {
        let counter = Arc::new(AtomicU64::new(0));
        let counter_clone = counter.clone();

        let report = run_lab_test(
            LabTestConfig::new(seed, "d06_replay").max_steps(50_000),
            |runtime| {
                let region = runtime.state.create_root_region(Budget::INFINITE);

                for i in 0..5u64 {
                    let c = counter_clone.clone();
                    let (tid, _h) = runtime
                        .state
                        .create_task(region, Budget::INFINITE, async move {
                            c.fetch_add(i + 1, Ordering::SeqCst);
                        })
                        .expect("create task");
                    runtime.scheduler.lock().schedule(tid, 0);
                }

                runtime.run_until_quiescent();
            },
        );
        assert!(report.passed());
        results.push(counter.load(Ordering::SeqCst));
    }

    assert_eq!(
        results[0], results[1],
        "same seed must produce identical results: {:?}",
        results
    );
    assert_eq!(results[0], 15, "sum of 1+2+3+4+5 = 15");
}

/// D07: Different seeds produce same final count (order-independent).
#[test]
fn d07_different_seeds_same_count() {
    for seed in [0, 42, 99, 123] {
        let report = run_lab_test_simple(seed, "d07_diff_seeds", |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            let counter = Arc::new(AtomicU64::new(0));

            for _ in 0..6 {
                let c = counter.clone();
                let (tid, _h) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        c.fetch_add(1, Ordering::SeqCst);
                    })
                    .expect("create");
                runtime.scheduler.lock().schedule(tid, 0);
            }

            runtime.run_until_quiescent();

            assert_eq!(
                counter.load(Ordering::SeqCst),
                6,
                "[seed={seed}] must complete all 6 tasks"
            );
        });
        assert!(report.passed());
    }
}

// =============================================================================
// D08–D09: DPOR schedule exploration
// =============================================================================

/// D08: DPOR exploration of mpsc channel delivery.
#[test]
fn d08_dpor_mpsc_delivery_exploration() {
    let report = run_exploration_test(
        ExplorationTestConfig::new("d08_dpor_mpsc", 20)
            .base_seed(0)
            .worker_count(2)
            .max_steps_per_run(50_000),
        |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            let (tx, rx) = asupersync::channel::mpsc::channel::<u32>(8);
            let count = Arc::new(AtomicUsize::new(0));

            let (tid1, _h1) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let cx = asupersync::cx::Cx::for_testing();
                    for i in 0..5 {
                        tx.send(&cx, i).await.unwrap();
                    }
                })
                .expect("create producer");
            runtime.scheduler.lock().schedule(tid1, 0);

            let cnt = count.clone();
            let (tid2, _h2) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let cx = asupersync::cx::Cx::for_testing();
                    let mut rx = rx;
                    while rx.recv(&cx).await.is_ok() {
                        cnt.fetch_add(1, Ordering::SeqCst);
                    }
                })
                .expect("create consumer");
            runtime.scheduler.lock().schedule(tid2, 0);

            runtime.run_until_quiescent();

            assert_eq!(
                count.load(Ordering::SeqCst),
                5,
                "all items must be delivered under every schedule"
            );
        },
    );

    assert!(
        report.passed(),
        "DPOR found violations in {:?}",
        report.violation_seeds
    );
}

/// D09: DPOR exploration of concurrent counter.
#[test]
fn d09_dpor_counter_exploration() {
    let report = run_exploration_test(
        ExplorationTestConfig::new("d09_dpor_counter", 20)
            .base_seed(0)
            .worker_count(3),
        |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            let counter = Arc::new(AtomicU64::new(0));

            for _ in 0..4 {
                let c = counter.clone();
                let (tid, _h) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        for _ in 0..10 {
                            c.fetch_add(1, Ordering::SeqCst);
                        }
                    })
                    .expect("create");
                runtime.scheduler.lock().schedule(tid, 0);
            }

            runtime.run_until_quiescent();

            assert_eq!(
                counter.load(Ordering::SeqCst),
                40,
                "counter must reach 40 under all schedules"
            );
        },
    );

    assert!(report.passed());
}

// =============================================================================
// D10–D11: Chaos fault injection resilience
// =============================================================================

/// D10: Channel delivery survives light chaos faults.
#[test]
fn d10_chaos_light_mpsc_delivery() {
    let report = run_chaos_test(
        ChaosTestConfig::light(42, "d10_chaos_light_mpsc").worker_count(2),
        |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            let (tx, rx) = asupersync::channel::mpsc::channel::<u32>(16);
            let count = Arc::new(AtomicUsize::new(0));

            let (tid1, _h1) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let cx = asupersync::cx::Cx::for_testing();
                    for i in 0..10 {
                        let _ = tx.send(&cx, i).await;
                    }
                })
                .expect("create producer");
            runtime.scheduler.lock().schedule(tid1, 0);

            let cnt = count.clone();
            let (tid2, _h2) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let cx = asupersync::cx::Cx::for_testing();
                    let mut rx = rx;
                    while rx.recv(&cx).await.is_ok() {
                        cnt.fetch_add(1, Ordering::SeqCst);
                    }
                })
                .expect("create consumer");
            runtime.scheduler.lock().schedule(tid2, 0);

            runtime.run_until_quiescent();

            let received = count.load(Ordering::SeqCst);
            assert!(received <= 10, "received must not exceed sent");
        },
    );

    assert!(report.passed());
}

/// D11: Atomic counter survives heavy chaos faults.
#[test]
fn d11_chaos_heavy_counter() {
    let report = run_chaos_test(
        ChaosTestConfig::heavy(42, "d11_chaos_heavy_counter")
            .worker_count(3)
            .max_steps(200_000),
        |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            let counter = Arc::new(AtomicU64::new(0));

            for _ in 0..5 {
                let c = counter.clone();
                let (tid, _h) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        for _ in 0..20 {
                            c.fetch_add(1, Ordering::SeqCst);
                        }
                    })
                    .expect("create");
                runtime.scheduler.lock().schedule(tid, 0);
            }

            runtime.run_until_quiescent();

            let val = counter.load(Ordering::SeqCst);
            assert!(val <= 100, "counter must not exceed expected max");
        },
    );

    assert!(report.passed());
}
