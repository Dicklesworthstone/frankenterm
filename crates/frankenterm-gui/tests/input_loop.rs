use frankenterm_core::cx::Cx;
use frankenterm_core::input_priority::{InputPriorityClass, Platform};
use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder};
use frankenterm_gui::input_loop::{
    InputLoopConfig, spawn_latency_pinned_input_loop,
    spawn_latency_pinned_input_loop_with_priority_applier,
};
use std::future::Future;
use std::sync::{Arc, Mutex};

fn run_async_test<F>(future: F)
where
    F: Future<Output = ()>,
{
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    runtime.block_on(future);
}

#[test]
fn input_loop_processes_enqueued_bytes_in_order() {
    run_async_test(async {
        let cx = Cx::for_testing();
        let writes = Arc::new(Mutex::new(Vec::new()));
        let writes_for_task = Arc::clone(&writes);
        let handle = spawn_latency_pinned_input_loop_with_priority_applier(
            Cx::for_testing(),
            InputLoopConfig {
                platform: Platform::Linux,
                priority_class: InputPriorityClass::LowLatency,
                queue_capacity: 4,
            },
            |_| true,
            move |bytes| {
                let writes_for_task = Arc::clone(&writes_for_task);
                async move {
                    writes_for_task.lock().expect("writes lock").push(bytes);
                    Ok(())
                }
            },
        );

        handle.enqueue_pty_bytes(&cx, b"a".to_vec()).await.unwrap();
        handle.enqueue_pty_bytes(&cx, b"b".to_vec()).await.unwrap();
        let report = handle.shutdown(&cx).await.unwrap();

        assert_eq!(report.processed_events, 2);
        assert_eq!(report.priority_stats.low_latency_grants_total, 1);
        assert_eq!(
            *writes.lock().expect("writes lock"),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
    });
}

#[test]
fn unsupported_platform_records_priority_fallback_without_dropping_input() {
    run_async_test(async {
        let cx = Cx::for_testing();
        let handle = spawn_latency_pinned_input_loop(
            Cx::for_testing(),
            InputLoopConfig {
                platform: Platform::Other,
                priority_class: InputPriorityClass::LowLatency,
                queue_capacity: 1,
            },
            |_| async { Ok(()) },
        );

        handle.enqueue_pty_bytes(&cx, b"x".to_vec()).await.unwrap();
        let report = handle.shutdown(&cx).await.unwrap();

        assert_eq!(report.processed_events, 1);
        assert_eq!(report.priority_stats.fallback_unsupported_platform_total, 1);
        assert!(!report.priority_apply.applied);
    });
}

#[test]
fn normal_priority_records_normal_request_fallback() {
    run_async_test(async {
        let cx = Cx::for_testing();
        let handle = spawn_latency_pinned_input_loop(
            Cx::for_testing(),
            InputLoopConfig {
                platform: Platform::Linux,
                priority_class: InputPriorityClass::Normal,
                queue_capacity: 1,
            },
            |_| async { Ok(()) },
        );

        let report = handle.shutdown(&cx).await.unwrap();

        assert_eq!(report.processed_events, 0);
        assert_eq!(report.priority_stats.normal_requests_total, 1);
    });
}
