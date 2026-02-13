//! DPOR-style concurrency tests for web streaming fanout semantics.
//!
//! These tests use LabRuntime schedule exploration to validate core stream
//! invariants under many interleavings without relying on wall-clock timing.

#![cfg(feature = "web")]

use asupersync::lab::explorer::{ExplorerConfig, ScheduleExplorer};
use asupersync::runtime::yield_now;
use asupersync::{Budget, LabRuntime, TaskId};
use frankenterm_core::policy::Redactor;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn schedule_task(runtime: &mut LabRuntime, task_id: TaskId) {
    runtime
        .scheduler
        .lock()
        .expect("lock scheduler")
        .schedule(task_id, 0);
}

#[test]
fn dpor_stream_subscribers_preserve_message_ordering() {
    let config = ExplorerConfig {
        base_seed: 17,
        max_runs: 12,
        max_steps_per_run: 100_000,
        worker_count: 3,
        record_traces: true,
    };

    let mut explorer = ScheduleExplorer::new(config);
    let report = explorer.explore(|runtime| {
        let total_events = 24_u64;
        let queues: Arc<Mutex<HashMap<u8, VecDeque<u64>>>> = Arc::new(Mutex::new(HashMap::from([
            (1_u8, VecDeque::new()),
            (2_u8, VecDeque::new()),
        ])));
        let received: Arc<Mutex<HashMap<u8, Vec<u64>>>> = Arc::new(Mutex::new(HashMap::from([
            (1_u8, Vec::new()),
            (2_u8, Vec::new()),
        ])));
        let done = Arc::new(AtomicBool::new(false));

        let region = runtime.state.create_root_region(Budget::INFINITE);

        let sub1_queues = Arc::clone(&queues);
        let sub1_received = Arc::clone(&received);
        let sub1_done = Arc::clone(&done);
        let (sub1_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                loop {
                    if let Some(message) = {
                        let mut guard = sub1_queues.lock().expect("lock subscriber queue");
                        guard.get_mut(&1).and_then(VecDeque::pop_front)
                    } {
                        let mut out = sub1_received.lock().expect("lock received log");
                        out.get_mut(&1).expect("subscriber 1 log").push(message);
                        continue;
                    }

                    let should_exit = sub1_done.load(Ordering::SeqCst) && {
                        let guard = sub1_queues.lock().expect("lock subscriber queue");
                        guard.get(&1).is_none_or(VecDeque::is_empty)
                    };
                    if should_exit {
                        break;
                    }
                    yield_now().await;
                }
            })
            .expect("create subscriber 1");

        let sub2_queues = Arc::clone(&queues);
        let sub2_received = Arc::clone(&received);
        let sub2_done = Arc::clone(&done);
        let (sub2_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                loop {
                    if let Some(message) = {
                        let mut guard = sub2_queues.lock().expect("lock subscriber queue");
                        guard.get_mut(&2).and_then(VecDeque::pop_front)
                    } {
                        let mut out = sub2_received.lock().expect("lock received log");
                        out.get_mut(&2).expect("subscriber 2 log").push(message);
                        continue;
                    }

                    let should_exit = sub2_done.load(Ordering::SeqCst) && {
                        let guard = sub2_queues.lock().expect("lock subscriber queue");
                        guard.get(&2).is_none_or(VecDeque::is_empty)
                    };
                    if should_exit {
                        break;
                    }
                    yield_now().await;
                }
            })
            .expect("create subscriber 2");

        let pub_queues = Arc::clone(&queues);
        let pub_done = Arc::clone(&done);
        let (publisher_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                for seq in 1..=total_events {
                    {
                        let mut guard = pub_queues.lock().expect("lock fanout queues");
                        for queue in guard.values_mut() {
                            queue.push_back(seq);
                        }
                    }
                    yield_now().await;
                }
                pub_done.store(true, Ordering::SeqCst);
            })
            .expect("create publisher");

        schedule_task(runtime, sub1_id);
        schedule_task(runtime, sub2_id);
        schedule_task(runtime, publisher_id);
        runtime.run_until_quiescent();

        let logs = received.lock().expect("lock final logs");
        let sub1 = logs.get(&1).expect("sub1 log present");
        let sub2 = logs.get(&2).expect("sub2 log present");

        assert_eq!(sub1.len(), total_events as usize);
        assert_eq!(sub2.len(), total_events as usize);
        assert_eq!(
            sub1, sub2,
            "all subscribers must observe identical ordering"
        );
        assert!(
            sub1.windows(2).all(|pair| pair[0] < pair[1]),
            "messages must remain strictly ordered"
        );
    });

    assert!(!report.has_violations());
    assert!(report.total_runs >= 8);
}

#[test]
fn dpor_stream_disconnect_race_releases_subscriber_resources() {
    let config = ExplorerConfig {
        base_seed: 33,
        max_runs: 12,
        max_steps_per_run: 140_000,
        worker_count: 4,
        record_traces: true,
    };

    let mut explorer = ScheduleExplorer::new(config);
    let report = explorer.explore(|runtime| {
        let total_events = 30_u64;
        let disconnect_after = 8_u64;
        let queues: Arc<Mutex<HashMap<u8, VecDeque<u64>>>> = Arc::new(Mutex::new(HashMap::from([
            (1_u8, VecDeque::new()),
            (2_u8, VecDeque::new()),
        ])));
        let active_ids = Arc::new(Mutex::new(vec![1_u8, 2_u8]));
        let subscriber_count = Arc::new(AtomicUsize::new(2));
        let published = Arc::new(AtomicU64::new(0));
        let disconnected = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));

        let region = runtime.state.create_root_region(Budget::INFINITE);

        let sub1_queues = Arc::clone(&queues);
        let sub1_done = Arc::clone(&done);
        let sub1_count = Arc::clone(&subscriber_count);
        let (sub1_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                loop {
                    let _ = {
                        let mut guard = sub1_queues.lock().expect("lock subscriber queue");
                        guard.get_mut(&1).and_then(VecDeque::pop_front)
                    };
                    let should_exit = sub1_done.load(Ordering::SeqCst) && {
                        let guard = sub1_queues.lock().expect("lock subscriber queue");
                        guard.get(&1).is_none_or(VecDeque::is_empty)
                    };
                    if should_exit {
                        break;
                    }
                    yield_now().await;
                }
                sub1_count.fetch_sub(1, Ordering::SeqCst);
            })
            .expect("create subscriber 1");

        let sub2_queues = Arc::clone(&queues);
        let sub2_done = Arc::clone(&done);
        let sub2_disc = Arc::clone(&disconnected);
        let sub2_count = Arc::clone(&subscriber_count);
        let (sub2_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                loop {
                    let queue_present = {
                        let mut guard = sub2_queues.lock().expect("lock subscriber queue");
                        if let Some(queue) = guard.get_mut(&2) {
                            let _ = queue.pop_front();
                            true
                        } else {
                            false
                        }
                    };

                    if !queue_present && sub2_disc.load(Ordering::SeqCst) {
                        break;
                    }

                    let should_exit = sub2_done.load(Ordering::SeqCst) && {
                        let guard = sub2_queues.lock().expect("lock subscriber queue");
                        guard.get(&2).is_none_or(VecDeque::is_empty)
                    };
                    if should_exit {
                        break;
                    }
                    yield_now().await;
                }
                sub2_count.fetch_sub(1, Ordering::SeqCst);
            })
            .expect("create subscriber 2");

        let disc_published = Arc::clone(&published);
        let disc_active_ids = Arc::clone(&active_ids);
        let disc_queues = Arc::clone(&queues);
        let disc_flag = Arc::clone(&disconnected);
        let (disconnect_task_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                while disc_published.load(Ordering::SeqCst) < disconnect_after {
                    yield_now().await;
                }
                {
                    let mut ids = disc_active_ids.lock().expect("lock active ids");
                    ids.retain(|id| *id != 2);
                }
                {
                    let mut queues = disc_queues.lock().expect("lock queues");
                    queues.remove(&2);
                }
                disc_flag.store(true, Ordering::SeqCst);
            })
            .expect("create disconnect task");

        let pub_queues = Arc::clone(&queues);
        let pub_ids = Arc::clone(&active_ids);
        let pub_published = Arc::clone(&published);
        let pub_done = Arc::clone(&done);
        let (publisher_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                for seq in 1..=total_events {
                    let recipients = {
                        let ids = pub_ids.lock().expect("lock active ids");
                        ids.clone()
                    };
                    {
                        let mut guard = pub_queues.lock().expect("lock fanout queues");
                        for id in recipients {
                            if let Some(queue) = guard.get_mut(&id) {
                                queue.push_back(seq);
                            }
                        }
                    }
                    pub_published.store(seq, Ordering::SeqCst);
                    yield_now().await;
                }
                pub_done.store(true, Ordering::SeqCst);
            })
            .expect("create publisher");

        schedule_task(runtime, sub1_id);
        schedule_task(runtime, sub2_id);
        schedule_task(runtime, disconnect_task_id);
        schedule_task(runtime, publisher_id);
        runtime.run_until_quiescent();

        assert_eq!(
            subscriber_count.load(Ordering::SeqCst),
            0,
            "all subscriber resources should be released after disconnect/finish"
        );
        let queues = queues.lock().expect("lock queues");
        assert!(
            !queues.contains_key(&2),
            "disconnected subscriber queue should be cleaned up"
        );
    });

    assert!(!report.has_violations());
}

#[test]
fn dpor_stream_slow_subscriber_does_not_block_fast_subscriber() {
    let config = ExplorerConfig {
        base_seed: 91,
        max_runs: 12,
        max_steps_per_run: 160_000,
        worker_count: 3,
        record_traces: true,
    };

    let mut explorer = ScheduleExplorer::new(config);
    let report = explorer.explore(|runtime| {
        let total_events = 80_u64;
        let queue_cap_fast = (total_events as usize) + 8;
        let queue_cap_slow = 4_usize;

        let queues: Arc<Mutex<HashMap<u8, VecDeque<u64>>>> = Arc::new(Mutex::new(HashMap::from([
            (1_u8, VecDeque::new()),
            (2_u8, VecDeque::new()),
        ])));
        let dropped: Arc<Mutex<HashMap<u8, usize>>> = Arc::new(Mutex::new(HashMap::from([
            (1_u8, 0_usize),
            (2_u8, 0_usize),
        ])));
        let received: Arc<Mutex<HashMap<u8, Vec<u64>>>> = Arc::new(Mutex::new(HashMap::from([
            (1_u8, Vec::new()),
            (2_u8, Vec::new()),
        ])));
        let done = Arc::new(AtomicBool::new(false));

        let region = runtime.state.create_root_region(Budget::INFINITE);

        let fast_queues = Arc::clone(&queues);
        let fast_received = Arc::clone(&received);
        let fast_done = Arc::clone(&done);
        let (fast_sub_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                loop {
                    if let Some(message) = {
                        let mut guard = fast_queues.lock().expect("lock fast queue");
                        guard.get_mut(&1).and_then(VecDeque::pop_front)
                    } {
                        let mut out = fast_received.lock().expect("lock fast output");
                        out.get_mut(&1).expect("fast log").push(message);
                        continue;
                    }
                    let should_exit = fast_done.load(Ordering::SeqCst) && {
                        let guard = fast_queues.lock().expect("lock fast queue");
                        guard.get(&1).is_none_or(VecDeque::is_empty)
                    };
                    if should_exit {
                        break;
                    }
                    yield_now().await;
                }
            })
            .expect("create fast subscriber");

        let slow_queues = Arc::clone(&queues);
        let slow_received = Arc::clone(&received);
        let slow_done = Arc::clone(&done);
        let (slow_sub_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                loop {
                    // Intentionally slow consumer to force its own queue pressure.
                    yield_now().await;
                    yield_now().await;
                    yield_now().await;
                    if let Some(message) = {
                        let mut guard = slow_queues.lock().expect("lock slow queue");
                        guard.get_mut(&2).and_then(VecDeque::pop_front)
                    } {
                        let mut out = slow_received.lock().expect("lock slow output");
                        out.get_mut(&2).expect("slow log").push(message);
                        continue;
                    }
                    let should_exit = slow_done.load(Ordering::SeqCst) && {
                        let guard = slow_queues.lock().expect("lock slow queue");
                        guard.get(&2).is_none_or(VecDeque::is_empty)
                    };
                    if should_exit {
                        break;
                    }
                    yield_now().await;
                }
            })
            .expect("create slow subscriber");

        let pub_queues = Arc::clone(&queues);
        let pub_dropped = Arc::clone(&dropped);
        let pub_done = Arc::clone(&done);
        let (publisher_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                for seq in 1..=total_events {
                    {
                        let mut queues = pub_queues.lock().expect("lock fanout queues");
                        let mut dropped = pub_dropped.lock().expect("lock drop counters");
                        for (id, queue) in queues.iter_mut() {
                            let capacity = if *id == 1 {
                                queue_cap_fast
                            } else {
                                queue_cap_slow
                            };
                            if queue.len() >= capacity {
                                let _ = queue.pop_front();
                                *dropped.get_mut(id).expect("drop counter") += 1;
                            }
                            queue.push_back(seq);
                        }
                    }
                    yield_now().await;
                }
                pub_done.store(true, Ordering::SeqCst);
            })
            .expect("create publisher");

        schedule_task(runtime, fast_sub_id);
        schedule_task(runtime, slow_sub_id);
        schedule_task(runtime, publisher_id);
        runtime.run_until_quiescent();

        let logs = received.lock().expect("lock final logs");
        let fast = logs.get(&1).expect("fast log");
        let slow = logs.get(&2).expect("slow log");
        let drops = dropped.lock().expect("lock drop counters");

        assert_eq!(drops.get(&1).copied().unwrap_or_default(), 0);
        assert_eq!(
            fast.len(),
            total_events as usize,
            "fast subscriber should remain unaffected by slow-subscriber pressure"
        );
        assert_eq!(fast.last().copied(), Some(total_events));
        assert!(slow.len() <= total_events as usize);
        assert!(
            drops.get(&2).copied().unwrap_or_default() > 0 || slow.len() < total_events as usize,
            "slow subscriber should demonstrate backpressure effects"
        );
    });

    assert!(!report.has_violations());
}

#[test]
fn dpor_stream_redaction_is_consistent_across_subscribers() {
    let config = ExplorerConfig {
        base_seed: 123,
        max_runs: 10,
        max_steps_per_run: 120_000,
        worker_count: 3,
        record_traces: true,
    };

    let mut explorer = ScheduleExplorer::new(config);
    let report = explorer.explore(|runtime| {
        let payloads = vec![
            "usage reached with secret sk-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb #1"
                .to_string(),
            "auth token sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa #2".to_string(),
            "Bearer sk-cccccccccccccccccccccccccccccccccccccccccccccccc #3".to_string(),
            "normal text without secret #4".to_string(),
        ];
        let secret_markers = ["sk-aaaaaaaa", "sk-bbbbbbbb", "sk-cccccccc", "Bearer sk-"];

        let redactor = Arc::new(Redactor::new());
        let queues: Arc<Mutex<HashMap<u8, VecDeque<String>>>> =
            Arc::new(Mutex::new(HashMap::from([
                (1_u8, VecDeque::new()),
                (2_u8, VecDeque::new()),
            ])));
        let received: Arc<Mutex<HashMap<u8, Vec<String>>>> = Arc::new(Mutex::new(HashMap::from([
            (1_u8, Vec::new()),
            (2_u8, Vec::new()),
        ])));
        let done = Arc::new(AtomicBool::new(false));

        let region = runtime.state.create_root_region(Budget::INFINITE);

        let sub1_queues = Arc::clone(&queues);
        let sub1_received = Arc::clone(&received);
        let sub1_done = Arc::clone(&done);
        let (sub1_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                loop {
                    if let Some(message) = {
                        let mut guard = sub1_queues.lock().expect("lock subscriber queue");
                        guard.get_mut(&1).and_then(VecDeque::pop_front)
                    } {
                        let mut out = sub1_received.lock().expect("lock subscriber output");
                        out.get_mut(&1).expect("subscriber 1 log").push(message);
                        continue;
                    }
                    let should_exit = sub1_done.load(Ordering::SeqCst) && {
                        let guard = sub1_queues.lock().expect("lock subscriber queue");
                        guard.get(&1).is_none_or(VecDeque::is_empty)
                    };
                    if should_exit {
                        break;
                    }
                    yield_now().await;
                }
            })
            .expect("create subscriber 1");

        let sub2_queues = Arc::clone(&queues);
        let sub2_received = Arc::clone(&received);
        let sub2_done = Arc::clone(&done);
        let (sub2_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                loop {
                    if let Some(message) = {
                        let mut guard = sub2_queues.lock().expect("lock subscriber queue");
                        guard.get_mut(&2).and_then(VecDeque::pop_front)
                    } {
                        let mut out = sub2_received.lock().expect("lock subscriber output");
                        out.get_mut(&2).expect("subscriber 2 log").push(message);
                        continue;
                    }
                    let should_exit = sub2_done.load(Ordering::SeqCst) && {
                        let guard = sub2_queues.lock().expect("lock subscriber queue");
                        guard.get(&2).is_none_or(VecDeque::is_empty)
                    };
                    if should_exit {
                        break;
                    }
                    yield_now().await;
                }
            })
            .expect("create subscriber 2");

        let pub_queues = Arc::clone(&queues);
        let pub_done = Arc::clone(&done);
        let pub_redactor = Arc::clone(&redactor);
        let (publisher_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                for raw in payloads {
                    let redacted = pub_redactor.redact(&raw);
                    {
                        let mut guard = pub_queues.lock().expect("lock fanout queues");
                        for queue in guard.values_mut() {
                            queue.push_back(redacted.clone());
                        }
                    }
                    yield_now().await;
                }
                pub_done.store(true, Ordering::SeqCst);
            })
            .expect("create publisher");

        schedule_task(runtime, sub1_id);
        schedule_task(runtime, sub2_id);
        schedule_task(runtime, publisher_id);
        runtime.run_until_quiescent();

        let logs = received.lock().expect("lock final logs");
        let sub1 = logs.get(&1).expect("sub1 log");
        let sub2 = logs.get(&2).expect("sub2 log");
        assert_eq!(
            sub1, sub2,
            "redaction output must be identical across subscribers"
        );

        for value in sub1 {
            assert!(value.contains("[REDACTED]") || value.contains("normal text"));
            for marker in &secret_markers {
                assert!(
                    !value.contains(marker),
                    "redacted stream must not leak raw secret marker: {marker}"
                );
            }
        }
    });

    assert!(!report.has_violations());
}
